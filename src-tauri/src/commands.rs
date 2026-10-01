//! The Tauri bridge.
//!
//! This is the only place `CoreError` becomes `UiError`, and the only place
//! tasks cross into the webview. The personal-first rule is applied *before*
//! serialising: other people's rows never reach the frontend, so no UI bug or
//! injected script can reveal them.

use std::sync::Arc;

use chrono::Utc;
use tauri::State;
use tokio::sync::{Mutex, RwLock};

use crate::auth::{self, TokenSet};
use crate::config::AppConfig;
use crate::error::{Result, UiError};
use crate::lark::BitableRepo;
use crate::oauth;
use crate::repo::TaskPatch;
use crate::sync::{Snapshot, Store};
use crate::task::Viewer;

pub struct AppState {
    pub store: RwLock<Option<Store<BitableRepo>>>,
    pub viewer: RwLock<Option<Viewer>>,
    /// Process-wide, so every repo instance shares one refresh lock. Lark
    /// rotates the refresh token, so two concurrent exchanges invalidate each
    /// other — and React StrictMode fires sign_in twice on every dev launch.
    pub tokens: Arc<Mutex<TokenSet>>,
    /// Serialises sign-in itself, so those two calls cannot both connect.
    pub connecting: Mutex<()>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            store: RwLock::new(None),
            viewer: RwLock::new(None),
            tokens: Arc::new(Mutex::new(TokenSet::default())),
            connecting: Mutex::new(()),
        }
    }
}

fn now_millis() -> i64 {
    Utc::now().timestamp_millis()
}

/// Build the repository and identify the user. Called once at startup and
/// again after a sign-in.
async fn connect(tokens: Arc<Mutex<TokenSet>>) -> Result<(Store<BitableRepo>, Viewer)> {
    let cfg = AppConfig::load()?;

    // Seed the shared cell from disk only if nothing is loaded yet; an
    // already-refreshed token in memory is newer than the file.
    {
        let mut guard = tokens.lock().await;
        if guard.access_token.is_empty() {
            *guard = auth::load_tokens()?;
        }
    }

    // whoami goes through the same refreshing transport, so an expired
    // session heals here instead of dead-ending at the sign-in prompt.
    let repo = BitableRepo::new(cfg, tokens)?;
    let viewer = repo.whoami().await?;
    Ok((Store::new(repo), viewer))
}

#[tauri::command]
pub async fn sign_in(state: State<'_, AppState>) -> std::result::Result<Viewer, UiError> {
    // Held for the whole connect: a second concurrent call waits rather than
    // racing a token rotation.
    let _lock = state.connecting.lock().await;
    let (store, viewer) = connect(state.tokens.clone()).await?;
    *state.store.write().await = Some(store);
    *state.viewer.write().await = Some(viewer.clone());
    Ok(viewer)
}

/// Full browser sign-in: consent, loopback redirect, code exchange.
///
/// Unlike `sign_in` (which only reuses or refreshes a stored session) this can
/// recover from a dead or missing refresh token, which is the only way a new
/// teammate can authenticate at all.
#[tauri::command]
pub async fn authorize(state: State<'_, AppState>) -> std::result::Result<Viewer, UiError> {
    let _lock = state.connecting.lock().await;
    let cfg = AppConfig::load()?;

    // A non-loopback redirect would send the code to someone else while we
    // waited locally and timed out, so refuse before opening a browser.
    oauth::validate_redirect(&cfg.oauth_redirect)?;
    let port = oauth::redirect_port(&cfg.oauth_redirect)?;
    let pending = oauth::begin(&cfg, &cfg.oauth_redirect);

    open_in_browser(&pending.url)?;

    // The listener is synchronous and blocks; keep it off the async runtime.
    let expected = pending.state.clone();
    let code = tokio::task::spawn_blocking(move || oauth::wait_for_code(port, &expected))
        .await
        .map_err(|e| crate::error::CoreError::Auth(format!("sign-in task failed: {e}")))??;

    let http = reqwest::Client::new();
    let tokens = oauth::exchange(&http, &cfg, &pending, &code, Utc::now().timestamp()).await?;
    auth::save_tokens(&tokens)?;
    *state.tokens.lock().await = tokens;

    let (store, viewer) = connect(state.tokens.clone()).await?;
    *state.store.write().await = Some(store);
    *state.viewer.write().await = Some(viewer.clone());
    Ok(viewer)
}

fn open_in_browser(url: &str) -> Result<()> {
    tauri_plugin_opener::open_url(url, None::<&str>)
        .map_err(|e| crate::error::CoreError::Auth(format!("Could not open the browser: {e}")))
}

#[tauri::command]
pub async fn current_viewer(state: State<'_, AppState>) -> std::result::Result<Option<Viewer>, UiError> {
    Ok(state.viewer.read().await.clone())
}

/// The signed-in user's tasks. There is deliberately no "all tasks" command.
#[tauri::command]
pub async fn list_my_tasks(
    state: State<'_, AppState>,
    refresh: bool,
) -> std::result::Result<Snapshot, UiError> {
    let viewer = state
        .viewer
        .read()
        .await
        .clone()
        .ok_or_else(|| UiError::from(crate::error::CoreError::Unauthorized))?;

    let mut guard = state.store.write().await;
    let store = guard
        .as_mut()
        .ok_or_else(|| UiError::from(crate::error::CoreError::Unauthorized))?;

    if refresh {
        if let Err(err) = store.refresh(now_millis()).await {
            // With no good snapshot behind it, an empty list would read as
            // "nothing assigned to you" — the opposite of what happened.
            let never_loaded = store.snapshot(&viewer).fetched_at_millis == 0;
            if never_loaded || matches!(err, crate::error::CoreError::Unauthorized) {
                return Err(err.into());
            }
            // Otherwise keep serving the last good data, marked stale.
        }
    }
    Ok(store.snapshot(&viewer))
}

#[tauri::command]
pub async fn update_task(
    state: State<'_, AppState>,
    record_id: String,
    patch: TaskPatch,
) -> std::result::Result<Snapshot, UiError> {
    let viewer = state
        .viewer
        .read()
        .await
        .clone()
        .ok_or_else(|| UiError::from(crate::error::CoreError::Unauthorized))?;
    let mut guard = state.store.write().await;
    let store = guard
        .as_mut()
        .ok_or_else(|| UiError::from(crate::error::CoreError::Unauthorized))?;

    store.update(&record_id, patch, now_millis()).await?;
    Ok(store.snapshot(&viewer))
}

#[tauri::command]
pub async fn create_task(
    state: State<'_, AppState>,
    patch: TaskPatch,
) -> std::result::Result<Snapshot, UiError> {
    let viewer = state
        .viewer
        .read()
        .await
        .clone()
        .ok_or_else(|| UiError::from(crate::error::CoreError::Unauthorized))?;
    let mut guard = state.store.write().await;
    let store = guard
        .as_mut()
        .ok_or_else(|| UiError::from(crate::error::CoreError::Unauthorized))?;

    // A new task belongs to whoever created it unless stated otherwise.
    let patch = TaskPatch {
        owner_ids: patch.owner_ids.or_else(|| Some(vec![viewer.open_id.clone()])),
        ..patch
    };
    store.create(patch).await?;
    Ok(store.snapshot(&viewer))
}

/// Deleting is irreversible, so the UI must confirm before calling this.
#[tauri::command]
pub async fn delete_task(
    state: State<'_, AppState>,
    record_id: String,
) -> std::result::Result<Snapshot, UiError> {
    let viewer = state
        .viewer
        .read()
        .await
        .clone()
        .ok_or_else(|| UiError::from(crate::error::CoreError::Unauthorized))?;
    let mut guard = state.store.write().await;
    let store = guard
        .as_mut()
        .ok_or_else(|| UiError::from(crate::error::CoreError::Unauthorized))?;

    store.delete(&record_id).await?;
    Ok(store.snapshot(&viewer))
}
