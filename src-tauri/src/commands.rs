//! The Tauri bridge.
//!
//! This is the only place `CoreError` becomes `UiError`, and the only place
//! tasks cross into the webview. The personal-first rule is applied *before*
//! serialising: other people's rows never reach the frontend, so no UI bug or
//! injected script can reveal them.

use chrono::Utc;
use tauri::State;
use tokio::sync::RwLock;

use crate::config::{cached_access_token, AppConfig};
use crate::error::{Result, UiError};
use crate::lark::BitableRepo;
use crate::repo::TaskPatch;
use crate::sync::{Snapshot, Store};
use crate::task::Viewer;

pub struct AppState {
    pub store: RwLock<Option<Store<BitableRepo>>>,
    pub viewer: RwLock<Option<Viewer>>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState { store: RwLock::new(None), viewer: RwLock::new(None) }
    }
}

fn now_millis() -> i64 {
    Utc::now().timestamp_millis()
}

/// Build the repository and identify the user. Called once at startup and
/// again after a sign-in.
async fn connect() -> Result<(Store<BitableRepo>, Viewer)> {
    let cfg = AppConfig::load()?;
    let token = cached_access_token()?;
    let repo = BitableRepo::new(cfg.base_token, cfg.table_id, token)?;
    let viewer = repo.whoami().await?;
    Ok((Store::new(repo), viewer))
}

#[tauri::command]
pub async fn sign_in(state: State<'_, AppState>) -> std::result::Result<Viewer, UiError> {
    let (store, viewer) = connect().await?;
    *state.store.write().await = Some(store);
    *state.viewer.write().await = Some(viewer.clone());
    Ok(viewer)
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
        // A failed refresh still yields the previous snapshot, flagged stale.
        if let Err(err) = store.refresh(now_millis()).await {
            if matches!(err, crate::error::CoreError::Unauthorized) {
                return Err(err.into());
            }
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
