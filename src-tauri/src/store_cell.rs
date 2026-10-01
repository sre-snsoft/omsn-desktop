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

use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use crate::error::{CoreError, Result};
use crate::repo::{TaskPatch, TaskRepository};
use crate::sync::{Snapshot, Store};
use crate::task::Viewer;

/// The store as the app holds it: absent until someone signs in.
pub type StoreCell<R> = RwLock<Option<Store<R>>>;

/// Serialises outbound record writes so two rapid clicks reach the Base in
/// the order they were made. Held only across one PUT, never across a poll,
/// so it cannot reintroduce the queueing this module removes.
pub type WriteGate = Mutex<()>;

fn signed_out<T>() -> Result<T> {
    Err(CoreError::Unauthorized)
}

/// Read the server and fold the result in, with the lock released for the walk.
pub async fn poll<R: TaskRepository>(cell: &StoreCell<R>, now_millis: i64) -> Result<()> {
    let repo = {
        let guard = cell.read().await;
        match guard.as_ref() {
            Some(store) => store.repo(),
            None => return signed_out(),
        }
    };

    let fetched = repo.list_all().await;

    let mut guard = cell.write().await;
    let Some(store) = guard.as_mut() else { return signed_out() };
    match fetched {
        Ok(tasks) => {
            store.apply_poll(tasks, now_millis);
            Ok(())
        }
        Err(err) => {
            store.mark_stale();
            Err(err)
        }
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

    let seq = store.begin_update(record_id, patch, now_millis)?;
    Ok(StartedWrite { snapshot: store.snapshot(viewer), repo: store.repo(), seq })
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
    record_id: String,
    patch: TaskPatch,
    seq: u64,
) {
    let outcome = {
        let _ordered = gate.lock().await;
        repo.update(&record_id, &patch).await
    };

    let mut guard = cell.write().await;
    if let Some(store) = guard.as_mut() {
        store.settle_update(&record_id, seq, outcome);
    }
    // Signed out while in flight: the store is gone and so is the overlay.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_repo::{me, task_named, FakeRepo};
    use std::time::{Duration, Instant};

    fn cell_with(tasks: Vec<crate::task::Task>) -> Arc<StoreCell<FakeRepo>> {
        Arc::new(RwLock::new(Some(Store::new(FakeRepo::with(tasks)))))
    }

    fn status_patch(status: &str) -> TaskPatch {
        TaskPatch { status: Some(status.into()), ..Default::default() }
    }

    async fn repo_of(cell: &Arc<StoreCell<FakeRepo>>) -> Arc<FakeRepo> {
        cell.read().await.as_ref().unwrap().repo()
    }

    /// Run a write end to end the way `update_task` does, and wait for it.
    async fn write_and_settle(
        cell: &Arc<StoreCell<FakeRepo>>,
        record_id: &str,
        patch: TaskPatch,
        now: i64,
    ) -> Result<Snapshot> {
        let started = begin_write(cell, &me(), record_id, patch.clone(), now).await?;
        let instant = started.snapshot.clone();
        finish_write(
            cell.clone(),
            Arc::new(Mutex::new(())),
            started.repo,
            record_id.to_string(),
            patch,
            started.seq,
        )
        .await;
        Ok(instant)
    }

    #[tokio::test]
    async fn a_write_returns_before_the_network_has_been_touched() {
        let cell = cell_with(vec![task_named("a", "ou_me")]);
        poll(&cell, 1_000).await.unwrap();
        // A PUT that takes a while, like a slow Lark round trip.
        *repo_of(&cell).await.update_delay.lock().unwrap() = Duration::from_millis(600);

        let started = Instant::now();
        let out =
            begin_write(&cell, &me(), "rec_a", status_patch("In Progress"), 2_000).await.unwrap();
        let elapsed = started.elapsed();

        assert!(elapsed < Duration::from_millis(100), "begin_write blocked for {elapsed:?}");
        assert_eq!(
            out.snapshot.tasks[0].status, "In Progress",
            "the snapshot the UI paints must already show the change"
        );
        assert_eq!(out.snapshot.pending_ids, vec!["rec_a".to_string()]);
    }

    #[tokio::test]
    async fn a_click_does_not_queue_behind_a_full_table_poll() {
        // The R5 bug, reproduced: list_all is a 20-request walk, and the old
        // code held the write lock across all of it.
        let cell = cell_with(vec![task_named("a", "ou_me")]);
        poll(&cell, 1_000).await.unwrap();
        *repo_of(&cell).await.list_delay.lock().unwrap() = Duration::from_millis(500);

        let polling = {
            let cell = cell.clone();
            tokio::spawn(async move { poll(&cell, 2_000).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;

        let started = Instant::now();
        begin_write(&cell, &me(), "rec_a", status_patch("In Progress"), 2_100).await.unwrap();
        let blocked_for = started.elapsed();

        assert!(
            blocked_for < Duration::from_millis(200),
            "the click waited {blocked_for:?} for the poll to finish"
        );
        polling.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_poll_landing_mid_write_does_not_revert_the_users_change() {
        let cell = cell_with(vec![task_named("a", "ou_me")]);
        poll(&cell, 1_000).await.unwrap();
        *repo_of(&cell).await.update_delay.lock().unwrap() = Duration::from_millis(300);

        let started =
            begin_write(&cell, &me(), "rec_a", status_patch("In Progress"), 2_000).await.unwrap();
        let writing = {
            let cell = cell.clone();
            let repo = started.repo.clone();
            let seq = started.seq;
            tokio::spawn(async move {
                finish_write(
                    cell,
                    Arc::new(Mutex::new(())),
                    repo,
                    "rec_a".to_string(),
                    status_patch("In Progress"),
                    seq,
                )
                .await
            })
        };

        // A poll completes while the PUT is still in flight, so it reports the
        // pre-write row.
        poll(&cell, 2_100).await.unwrap();
        let mid = snapshot(&cell, &me()).await.unwrap();
        assert_eq!(mid.tasks[0].status, "In Progress", "the overlay must hold");

        writing.await.unwrap();
        let after = snapshot(&cell, &me()).await.unwrap();
        assert_eq!(after.tasks[0].status, "In Progress");
        assert!(after.pending_ids.is_empty(), "confirmed writes stop being pending");
    }

    #[tokio::test]
    async fn a_rejected_write_snaps_back_and_reports_why_in_the_next_snapshot() {
        let cell = cell_with(vec![task_named("a", "ou_me")]);
        poll(&cell, 1_000).await.unwrap();
        *repo_of(&cell).await.fail_next.lock().unwrap() = true;

        let instant =
            write_and_settle(&cell, "rec_a", status_patch("In Progress"), 2_000).await.unwrap();
        assert_eq!(instant.tasks[0].status, "In Progress", "it looked applied at first");

        let settled = snapshot(&cell, &me()).await.unwrap();
        assert_eq!(settled.tasks[0].status, "Backlog", "and then snapped back");
        assert_eq!(settled.write_failures.len(), 1, "with a reason the user can read");
        assert!(settled.pending_ids.is_empty());
        assert!(
            !settled.write_failures[0].message.is_empty()
                && !settled.write_failures[0].message.contains("Bearer")
        );
    }

    #[tokio::test]
    async fn an_invalid_select_value_never_reaches_the_network() {
        // Lark turns an unknown single-select value into a permanent new option
        // for all 14 people. The background write must never get the chance.
        let cell = cell_with(vec![task_named("a", "ou_me")]);
        poll(&cell, 1_000).await.unwrap();

        let err = begin_write(&cell, &me(), "rec_a", status_patch("Nonsense"), 2_000)
            .await
            .err()
            .expect("an unknown select value must be refused")
            .to_string();

        assert!(err.contains("not a valid status"), "got: {err}");
        assert!(repo_of(&cell).await.sent_patches().is_empty(), "nothing may have been sent");
        assert!(snapshot(&cell, &me()).await.unwrap().pending_ids.is_empty());
    }

    #[tokio::test]
    async fn repeated_clicks_leave_the_ui_on_the_last_one() {
        let cell = cell_with(vec![task_named("a", "ou_me")]);
        poll(&cell, 1_000).await.unwrap();

        write_and_settle(&cell, "rec_a", status_patch("In Progress"), 2_000).await.unwrap();
        write_and_settle(&cell, "rec_a", status_patch("Backlog"), 2_100).await.unwrap();
        write_and_settle(&cell, "rec_a", status_patch("In Progress"), 2_200).await.unwrap();

        let snap = snapshot(&cell, &me()).await.unwrap();
        assert_eq!(snap.tasks[0].status, "In Progress");
        assert!(snap.pending_ids.is_empty(), "nothing may be left stranded in flight");
        assert!(snap.write_failures.is_empty());

        let sent = repo_of(&cell).await.sent_patches();
        assert_eq!(sent.len(), 3, "one write per click, no more");
        assert!(sent.iter().all(|p| p.title.is_none()), "only the changed field travels");
    }

    #[tokio::test]
    async fn writes_reach_the_base_one_at_a_time() {
        // Two PUTs overlapping on one record would leave the Base holding
        // whichever finished last rather than whichever was clicked last.
        let cell = cell_with(vec![task_named("a", "ou_me")]);
        poll(&cell, 1_000).await.unwrap();
        *repo_of(&cell).await.update_delay.lock().unwrap() = Duration::from_millis(120);
        let gate = Arc::new(Mutex::new(()));

        let first = begin_write(&cell, &me(), "rec_a", status_patch("In Progress"), 2_000)
            .await
            .unwrap();
        let a = tokio::spawn(finish_write(
            cell.clone(),
            gate.clone(),
            first.repo,
            "rec_a".into(),
            status_patch("In Progress"),
            first.seq,
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second =
            begin_write(&cell, &me(), "rec_a", status_patch("Done"), 2_020).await.unwrap();
        let b = tokio::spawn(finish_write(
            cell.clone(),
            gate.clone(),
            second.repo,
            "rec_a".into(),
            status_patch("Done"),
            second.seq,
        ));

        a.await.unwrap();
        b.await.unwrap();

        let sent = repo_of(&cell).await.sent_patches();
        assert_eq!(
            sent.iter().filter_map(|p| p.status.clone()).collect::<Vec<_>>(),
            vec!["In Progress".to_string(), "Done".to_string()],
            "the gate must keep clicks in order"
        );
        let snap = snapshot(&cell, &me()).await.unwrap();
        assert_eq!(snap.tasks[0].status, "Done", "the last click wins");
        assert!(snap.pending_ids.is_empty());
    }

    #[tokio::test]
    async fn a_failed_poll_keeps_the_previous_tasks_and_flags_them_stale() {
        let cell = cell_with(vec![task_named("a", "ou_me")]);
        poll(&cell, 1_000).await.unwrap();
        *repo_of(&cell).await.fail_next.lock().unwrap() = true;

        assert!(poll(&cell, 2_000).await.is_err());

        let snap = snapshot(&cell, &me()).await.unwrap();
        assert_eq!(snap.tasks.len(), 1, "keep showing what we had");
        assert!(snap.stale);
        assert_eq!(fetched_at_millis(&cell).await, 1_000);
    }

    #[tokio::test]
    async fn every_operation_refuses_to_invent_data_before_sign_in() {
        let cell: Arc<StoreCell<FakeRepo>> = Arc::new(RwLock::new(None));

        assert!(matches!(poll(&cell, 1_000).await, Err(CoreError::Unauthorized)));
        assert!(matches!(snapshot(&cell, &me()).await, Err(CoreError::Unauthorized)));
        assert!(matches!(
            begin_write(&cell, &me(), "rec_a", status_patch("Done"), 1_000).await,
            Err(CoreError::Unauthorized)
        ));
        assert_eq!(fetched_at_millis(&cell).await, 0);
    }

    #[tokio::test]
    async fn signing_out_mid_write_is_not_a_panic() {
        let cell = cell_with(vec![task_named("a", "ou_me")]);
        poll(&cell, 1_000).await.unwrap();
        let started =
            begin_write(&cell, &me(), "rec_a", status_patch("Done"), 2_000).await.unwrap();

        *cell.write().await = None;

        finish_write(
            cell.clone(),
            Arc::new(Mutex::new(())),
            started.repo,
            "rec_a".into(),
            status_patch("Done"),
            started.seq,
        )
        .await;
    }
}
