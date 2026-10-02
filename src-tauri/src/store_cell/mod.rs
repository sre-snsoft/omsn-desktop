//! Where the store meets the network, and the only place the store lock is
//! taken.
//!
//! The rule this module exists to enforce: **no I/O happens while the store
//! lock is held.** `tokio::sync::RwLock` is FIFO-fair, so a writer that
//! queues behind a reader holding the lock across a full-table walk — up to 20
//! sequential HTTPS GETs — waits for the whole walk. That was the R5 bug: a
//! click landing during a poll could not even start its PUT for seconds.
//!
//! So every operation here is: take the lock, decide, release; do the I/O;
//! take the lock, fold the answer in, release.

use std::collections::BTreeSet;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use crate::error::{CoreError, Result};
use crate::repo::{TaskPatch, TaskRepository};
use crate::sync::{FetchMode, Snapshot, Store};
use crate::task::{Task, Viewer};

/// The store as the app holds it: absent until someone signs in.
pub type StoreCell<R> = RwLock<Option<Store<R>>>;

/// Serialises outbound record writes so two rapid clicks reach the Base in
/// the order they were made. Held only across one PUT, never across a poll,
/// so it cannot reintroduce the queueing this module removes.
pub type WriteGate = Mutex<()>;

fn signed_out<T>() -> Result<T> {
    Err(CoreError::Unauthorized)
}

/// Told when a write has settled, so the UI can drop the in-flight marker the
/// moment Rust knows the outcome rather than on the next tick of a timer.
///
/// A trait rather than an `AppHandle` so the store keeps no Tauri dependency
/// and the tests can watch what was announced.
pub trait SettleNotifier: Send + Sync {
    fn write_settled(&self, record_id: &str);
}

/// A notifier for contexts with no UI attached, such as tests of the
/// store rules themselves.
pub struct Unannounced;

impl SettleNotifier for Unannounced {
    fn write_settled(&self, _record_id: &str) {}
}

/// Read the server and fold the result in, with the lock released for the walk.
///
/// `now_millis` is both the moment the walk began and the time its rows are
/// stamped with. One clock read, taken by the caller, so the start can be
/// compared against what the store already holds.
///
/// A poll arriving while another is still running is a no-op, not an error:
/// the data it would have fetched is already on its way.
pub async fn poll<R: TaskRepository>(
    cell: &StoreCell<R>,
    viewer: &Viewer,
    now_millis: i64,
) -> Result<()> {
    let (repo, mode, walk) = {
        let mut guard = cell.write().await;
        let Some(store) = guard.as_mut() else { return signed_out() };
        match store.begin_poll(now_millis) {
            Some(walk) => (store.repo(), store.fetch_mode(), walk),
            None => return Ok(()),
        }
    };

    let fetched = fetch(&repo, mode, viewer).await;

    let mut guard = cell.write().await;
    let Some(store) = guard.as_mut() else { return signed_out() };
    store.end_poll();
    match fetched {
        Ok(tasks) => {
            // A discarded result is not a failure; see `Store::apply_poll`.
            store.apply_poll(tasks, walk, now_millis);
            Ok(())
        }
        Err(err) => {
            store.mark_stale();
            Err(err)
        }
    }
}

/// Read the Base the way this session has been calibrated to read it.
///
/// A filtered poll that comes back empty is never believed on its own: the UI
/// renders an empty list as "NOTHING ASSIGNED", which is the worst possible
/// copy for a filter that has quietly stopped matching. One walk settles it.
async fn fetch<R: TaskRepository>(
    repo: &Arc<R>,
    mode: FetchMode,
    viewer: &Viewer,
) -> Result<Vec<Task>> {
    if mode == FetchMode::FullWalk {
        return repo.list_all().await;
    }
    let mine = repo.list_owned_by(&viewer.open_id).await?;
    if mine.is_empty() {
        return repo.list_all().await;
    }
    Ok(mine)
}

/// Decide once per session whether the server-side Owner filter can be
/// trusted, and seed the store from the walk it does anyway.
///
/// `Owner` is `multiple: true`, and whether Lark's `is` operator matches a row
/// the viewer merely co-owns is **not verified**. A partial loss would be
/// silent — the user would simply stop seeing work they share with someone.
/// So the filter is never trusted on inspection: it is checked against a real
/// walk, record id by record id, and any disagreement at all puts the session
/// on full walks for its whole lifetime. The user sees a slower app and no
/// message; there is nothing for them to act on.
pub async fn calibrate_fetch_mode<R: TaskRepository>(
    cell: &StoreCell<R>,
    viewer: &Viewer,
    now_millis: i64,
) -> FetchMode {
    let claimed = {
        let mut guard = cell.write().await;
        guard.as_mut().and_then(|store| store.begin_poll(now_millis).map(|w| (store.repo(), w)))
    };
    let Some((repo, walk)) = claimed else { return FetchMode::FullWalk };

    let full = repo.list_all().await;
    let filtered = repo.list_owned_by(&viewer.open_id).await;
    let mode = match (&full, &filtered) {
        (Ok(all), Ok(mine)) => agreed(all, mine, viewer),
        _ => FetchMode::FullWalk,
    };

    let mut guard = cell.write().await;
    if let Some(store) = guard.as_mut() {
        store.end_poll();
        store.set_fetch_mode(mode);
        if let Ok(tasks) = full {
            store.apply_poll(tasks, walk, now_millis);
        }
    }
    mode
}

/// `Filtered` only if the server returned exactly the rows a full walk proves
/// the viewer owns. Nothing is logged but counts — no ids, no identifiers.
fn agreed(all: &[Task], filtered: &[Task], viewer: &Viewer) -> FetchMode {
    let owned: BTreeSet<&str> =
        all.iter().filter(|t| t.is_owned_by(viewer)).map(|t| t.record_id.as_str()).collect();
    let returned: BTreeSet<&str> = filtered.iter().map(|t| t.record_id.as_str()).collect();

    if owned == returned {
        return FetchMode::Filtered;
    }
    eprintln!(
        "omsn: owner filter returned {} of {} owned records; using full reads this session",
        returned.len(),
        owned.len()
    );
    FetchMode::FullWalk
}

/// Refuse to touch a record the viewer does not own.
///
/// Reads are personal-first; writes must be too. `list_my_tasks` means the UI
/// cannot *offer* someone else's row, but the rule belongs in Rust at the
/// command boundary, never in the UI — a stale or mistaken record id from the
/// webview must not be able to destroy another person's work. An id the store
/// has never seen is refused as well: it cannot be vouched for.
fn ensure_owned<R: TaskRepository>(
    store: &Store<R>,
    viewer: &Viewer,
    record_id: &str,
) -> Result<()> {
    match store.ownership_of(record_id, viewer) {
        Some(true) => Ok(()),
        Some(false) => Err(CoreError::Forbidden),
        None => Err(CoreError::Invalid(
            "That task is not in your list. Refresh and try again.".into(),
        )),
    }
}

/// The last good fetch time, or 0 if the store has never held one.
///
/// Distinguishes "the first load failed" — where an empty list would read as
/// "nothing assigned to you" — from "a later poll failed".
pub async fn fetched_at_millis<R: TaskRepository>(cell: &StoreCell<R>) -> i64 {
    cell.read().await.as_ref().map(Store::fetched_at_millis).unwrap_or(0)
}

pub async fn snapshot<R: TaskRepository>(
    cell: &StoreCell<R>,
    viewer: &Viewer,
) -> Result<Snapshot> {
    match cell.read().await.as_ref() {
        Some(store) => Ok(store.snapshot(viewer)),
        None => signed_out(),
    }
}

/// Lay the overlay down and return at once, with the PUT still to come.
///
/// Returns the snapshot the UI should paint *now* plus everything the
/// background task needs. Validation failures surface here, synchronously,
/// so an invalid select value is refused rather than shown as applied.
pub struct StartedWrite<R> {
    pub snapshot: Snapshot,
    pub repo: Arc<R>,
    pub seq: u64,
}

pub async fn begin_write<R: TaskRepository>(
    cell: &StoreCell<R>,
    viewer: &Viewer,
    record_id: &str,
    patch: TaskPatch,
    now_millis: i64,
) -> Result<StartedWrite<R>> {
    let mut guard = cell.write().await;
    let Some(store) = guard.as_mut() else { return signed_out() };

    ensure_owned(store, viewer, record_id)?;
    let seq = store.begin_update(record_id, patch, now_millis)?;
    Ok(StartedWrite { snapshot: store.snapshot(viewer), repo: store.repo(), seq })
}

/// Create a record, with the store lock released across the POST.
///
/// `tokio::sync::RwLock` is FIFO-fair, so holding it across the request made
/// every later click wait for the whole round trip. Same take / release / do
/// the I/O / take again shape as a status write.
pub async fn create<R: TaskRepository>(
    cell: &StoreCell<R>,
    viewer: &Viewer,
    patch: TaskPatch,
) -> Result<Snapshot> {
    let repo = {
        let guard = cell.read().await;
        let Some(store) = guard.as_ref() else { return signed_out() };
        store.repo()
    };

    let created = repo.create(&patch).await?;

    let mut guard = cell.write().await;
    let Some(store) = guard.as_mut() else { return signed_out() };
    store.fold_created(created);
    Ok(store.snapshot(viewer))
}

/// Delete a record, with the store lock released across the request.
///
/// Ownership is checked first, before anything is sent.
pub async fn delete<R: TaskRepository>(
    cell: &StoreCell<R>,
    viewer: &Viewer,
    record_id: &str,
) -> Result<Snapshot> {
    let repo = {
        let guard = cell.read().await;
        let Some(store) = guard.as_ref() else { return signed_out() };
        ensure_owned(store, viewer, record_id)?;
        store.repo()
    };

    repo.delete(record_id).await?;

    let mut guard = cell.write().await;
    let Some(store) = guard.as_mut() else { return signed_out() };
    store.fold_deleted(record_id);
    Ok(store.snapshot(viewer))
}

/// Send the write and settle the overlay against the answer.
///
/// Spawned, so nothing here can make the caller wait. A rejection is recorded
/// in the store rather than returned: by the time it arrives the command has
/// long since answered, and the user must still be told.
pub async fn finish_write<R: TaskRepository>(
    cell: Arc<StoreCell<R>>,
    gate: Arc<WriteGate>,
    repo: Arc<R>,
    notify: Arc<dyn SettleNotifier>,
    record_id: String,
    patch: TaskPatch,
    seq: u64,
) {
    let outcome = {
        let _ordered = gate.lock().await;
        repo.update(&record_id, &patch).await
    };

    {
        let mut guard = cell.write().await;
        if let Some(store) = guard.as_mut() {
            store.settle_update(&record_id, seq, outcome);
        }
        // Signed out while in flight: the store is gone and so is the overlay.
    }

    // Announced with the lock released, and announced even when the store has
    // gone: a listener that never hears back leaves the row dimmed.
    notify.write_settled(&record_id);
}

#[cfg(test)]
mod tests;
