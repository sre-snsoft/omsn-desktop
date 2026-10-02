//! The Tauri bridge.
//!
//! This is the only place `CoreError` becomes `UiError`, and the only place
//! tasks cross into the webview. The personal-first rule is applied *before*
//! serialising: other people's rows never reach the frontend, so no UI bug or
//! injected script can reveal them.

use std::sync::Arc;

use chrono::{Local, Utc};
use tauri::{AppHandle, Emitter, State};
use tokio::sync::{Mutex, RwLock};

use crate::auth::{self, TokenSet};
use crate::config::AppConfig;
use crate::error::{Result, UiError};
use crate::lark::BitableRepo;
use crate::oauth;
use crate::repo::{stamp_completion, TaskPatch};
use crate::store_cell::{self, SettleNotifier, StoreCell, WriteGate};
use crate::sync::{Snapshot, Store};
use crate::task::Viewer;

pub struct AppState {
    /// `Arc` because a background write keeps using it after the command that
    /// started it has already answered the UI.
    pub store: Arc<StoreCell<BitableRepo>>,
    pub viewer: RwLock<Option<Viewer>>,
    /// Process-wide, so every repo instance shares one refresh lock. Lark
    /// rotates the refresh token, so two concurrent exchanges invalidate each
    /// other — and React StrictMode fires sign_in twice on every dev launch.
    pub tokens: Arc<Mutex<TokenSet>>,
    /// Serialises sign-in itself, so those two calls cannot both connect.
    pub connecting: Mutex<()>,
    /// Serialises outbound record writes, so two rapid clicks reach the Base
    /// in the order they were made.
    pub write_gate: Arc<WriteGate>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            store: Arc::new(RwLock::new(None)),
            viewer: RwLock::new(None),
            tokens: Arc::new(Mutex::new(TokenSet::default())),
            connecting: Mutex::new(()),
            write_gate: Arc::new(Mutex::new(())),
        }
    }
}

fn now_millis() -> i64 {
    Utc::now().timestamp_millis()
}

/// The Tauri event the UI listens on to drop a row's in-flight marker.
pub const WRITE_SETTLED_EVENT: &str = "omsn://write-settled";

/// Carries a settled write back to the webview.
///
/// `core:app:default` already grants `allow-register-listener`, so this needs
/// no capability change. Only the record id travels — never a patch, never a
/// response body.
struct EmitToWindow(AppHandle);

impl SettleNotifier for EmitToWindow {
    fn write_settled(&self, record_id: &str) {
        // A failed emit means there is no window left to tell; the backstop
        // poll covers the rest. Nothing here is worth an error notice.
        let _ = self.0.emit(WRITE_SETTLED_EVENT, record_id);
    }
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
    store_cell::calibrate_fetch_mode(&state.store, &viewer, now_millis()).await;
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

    // Reserve the port BEFORE sending the user to consent. Opening the browser
    // first would let them approve and have the code delivered to whatever
    // already holds the port.
    let listener = oauth::bind_listener(port)?;
    open_in_browser(&pending.url)?;

    // The listener is synchronous and blocks; keep it off the async runtime.
    let expected = pending.state.clone();
    let code = tokio::task::spawn_blocking(move || oauth::wait_for_code(listener, &expected))
        .await
        .map_err(|e| crate::error::CoreError::Auth(format!("sign-in task failed: {e}")))??;

    let http = reqwest::Client::new();
    let tokens = oauth::exchange(&http, &cfg, &pending, &code, Utc::now().timestamp()).await?;
    auth::save_tokens(&tokens)?;
    *state.tokens.lock().await = tokens;

    let (store, viewer) = connect(state.tokens.clone()).await?;
    *state.store.write().await = Some(store);
    *state.viewer.write().await = Some(viewer.clone());
    store_cell::calibrate_fetch_mode(&state.store, &viewer, now_millis()).await;
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

    if refresh {
        if let Err(err) = store_cell::poll(&state.store, &viewer, now_millis()).await {
            // With no good snapshot behind it, an empty list would read as
            // "nothing assigned to you" — the opposite of what happened.
            let never_loaded = store_cell::fetched_at_millis(&state.store).await == 0;
            if never_loaded || matches!(err, crate::error::CoreError::Unauthorized) {
                return Err(err.into());
            }
            // Otherwise keep serving the last good data, marked stale.
        }
    }
    Ok(store_cell::snapshot(&state.store, &viewer).await?)
}

#[tauri::command]
pub async fn update_task(
    app: AppHandle,
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

    // Completing a task records the day it was completed. Decided here, not
    // in the webview, and only ever on a write that sets Done.
    let patch = stamp_completion(patch, Local::now());

    // Overlay the change and answer immediately: the PUT, a token refresh and
    // a possible retry are tens of seconds in the worst case, and the user
    // clicked a button. The write continues in the background and settles
    // itself; `write_failures` in a later snapshot is how a rejection gets
    // back to them.
    let started =
        store_cell::begin_write(&state.store, &viewer, &record_id, patch.clone(), now_millis())
            .await?;

    tauri::async_runtime::spawn(store_cell::finish_write(
        state.store.clone(),
        state.write_gate.clone(),
        started.repo,
        Arc::new(EmitToWindow(app)),
        record_id,
        patch,
        started.seq,
    ));

    Ok(started.snapshot)
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

    // A new task belongs to whoever created it unless stated otherwise.
    let patch = stamp_completion(patch, Local::now());
    let patch = TaskPatch {
        owner_ids: patch.owner_ids.or_else(|| Some(vec![viewer.open_id.clone()])),
        ..patch
    };
    Ok(store_cell::create(&state.store, &viewer, patch).await?)
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

    // Ownership is checked inside, before anything is sent.
    Ok(store_cell::delete(&state.store, &viewer, &record_id).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// `authorize` holds `connecting` for its entire body, writes
    /// `state.tokens`, and then calls `connect()` — which locks `state.tokens`
    /// again, and whose `whoami()` locks it once more per request.
    /// `tokio::sync::Mutex` is NOT reentrant, so if any of those guards
    /// outlived its statement the command would hang forever with no error.
    /// This replays the exact sequence under a timeout.
    #[tokio::test]
    async fn the_authorize_lock_sequence_cannot_self_deadlock() {
        let state = AppState::default();

        let replay = async {
            // authorize(): serialise against sign_in for the whole body.
            let _lock = state.connecting.lock().await;

            // ...after oauth::exchange succeeded.
            *state.tokens.lock().await = TokenSet {
                access_token: "u-just-exchanged".into(),
                refresh_token: "ur-just-exchanged".into(),
                expires_at: i64::MAX,
            };

            // connect(): seeds the shared cell from disk only if empty.
            {
                let mut guard = state.tokens.lock().await;
                assert!(
                    !guard.access_token.is_empty(),
                    "a token exchanged one line ago must not be overwritten from disk"
                );
                guard.expires_at = i64::MAX;
            }

            // BitableRepo::access_token(), once per outbound request.
            let token = state.tokens.lock().await.access_token.clone();
            assert_eq!(token, "u-just-exchanged");

            // The two writes authorize finishes with.
            *state.store.write().await = None;
            *state.viewer.write().await = None;
        };

        tokio::time::timeout(Duration::from_secs(5), replay)
            .await
            .expect("authorize's lock sequence self-deadlocked");
    }

    /// Two sign-ins must not overlap: Lark rotates the refresh token, so a
    /// second concurrent exchange would invalidate the first. React StrictMode
    /// fires the mount effect twice on every dev launch, so this is routine.
    #[tokio::test]
    async fn connecting_serialises_two_concurrent_sign_ins() {
        let state = Arc::new(AppState::default());
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

        let a = {
            let (state, order) = (state.clone(), order.clone());
            tokio::spawn(async move {
                let _lock = state.connecting.lock().await;
                order.lock().await.push("a-enter");
                tokio::time::sleep(Duration::from_millis(150)).await;
                order.lock().await.push("a-exit");
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        let b = {
            let (state, order) = (state.clone(), order.clone());
            tokio::spawn(async move {
                let _lock = state.connecting.lock().await;
                order.lock().await.push("b-enter");
                order.lock().await.push("b-exit");
            })
        };

        tokio::time::timeout(Duration::from_secs(5), async {
            a.await.unwrap();
            b.await.unwrap();
        })
        .await
        .expect("the connecting mutex deadlocked");

        assert_eq!(
            *order.lock().await,
            vec!["a-enter", "a-exit", "b-enter", "b-exit"],
            "the second sign-in must wait for the first to finish, not interleave"
        );
    }

    /// `authorize` holds `connecting` across the whole 180s browser wait, so
    /// any `sign_in` issued meanwhile is blocked for that entire time with no
    /// feedback. Scaled down, but the shape is what ships.
    #[tokio::test]
    async fn a_browser_wait_blocks_sign_in_for_its_whole_duration() {
        let state = Arc::new(AppState::default());
        let consent_wait = Duration::from_millis(400);

        let authorizing = {
            let state = state.clone();
            tokio::spawn(async move {
                let _lock = state.connecting.lock().await;
                tokio::time::sleep(consent_wait).await; // oauth::wait_for_code
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = std::time::Instant::now();
        {
            let _lock = state.connecting.lock().await;
        }
        let blocked_for = started.elapsed();
        authorizing.await.unwrap();

        assert!(
            blocked_for >= Duration::from_millis(250),
            "expected sign_in to be blocked by the consent wait, waited only {blocked_for:?}"
        );
    }

    /// Before any sign-in there is no viewer and no store, so every data
    /// command must say "sign in", never return an empty task list that reads
    /// as "nothing assigned to you".
    #[tokio::test]
    async fn a_cold_state_reports_unauthorized_rather_than_an_empty_list() {
        let state = AppState::default();
        assert!(state.viewer.read().await.is_none());
        assert!(state.store.read().await.is_none());
        let ui: UiError = crate::error::CoreError::Unauthorized.into();
        assert!(ui.needs_login, "the UI needs a sign-in affordance for this");
        assert_eq!(ui.kind, "unauthorized");
    }

    /// The default token set must never look usable, or startup would skip the
    /// disk load and send empty bearer headers.
    #[tokio::test]
    async fn the_default_token_set_is_not_usable() {
        let state = AppState::default();
        let guard = state.tokens.lock().await;
        assert!(guard.access_token.is_empty());
        assert!(!guard.is_usable(0));
        assert!(!guard.can_refresh());
    }
}
