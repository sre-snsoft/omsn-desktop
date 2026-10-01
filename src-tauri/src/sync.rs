//! The single owner of server state.
//!
//! Rust holds one snapshot; the UI renders what it is given and keeps no cache
//! of its own. That matters here because the OMSN Claude plugin writes to the
//! same Base table, so the app is never the only writer:
//!
//!   * writes send only changed fields, so a concurrent edit to a *different*
//!     field by the plugin is not clobbered;
//!   * a write is held in a pending overlay until a poll confirms it, so the
//!     user's change does not visibly revert when a poll returns a snapshot
//!     taken before the write landed.
//!
//! Everything here is short and lock-free by construction: no method performs
//! I/O. The network sits in `store_cell`, outside the lock, so a click is
//! never queued behind a full-table poll.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::error::{CoreError, Result};
use crate::repo::{TaskPatch, TaskRepository};
use crate::task::{only_mine, Task, Viewer};

/// A write we have sent but not yet seen reflected in a poll.
#[derive(Debug, Clone)]
pub struct PendingWrite {
    pub patch: TaskPatch,
    /// `Modified` of the record when we sent the write, if it was known.
    pub base_modified: Option<i64>,
    pub sent_at_millis: i64,
    /// Identifies the write this overlay belongs to. A completion arriving
    /// with an older seq has been superseded by a later click, so it must not
    /// clear the overlay the newer click put there.
    pub seq: u64,
}

/// A write the server refused, kept until the UI has had a chance to show it.
///
/// Instant UI means the command returns before the server has answered, so a
/// rejection has no promise left to reject. It is reported through the next
/// snapshot instead — never swallowed, because a silent snap-back would leave
/// the user believing a change they can no longer see applied.
#[derive(Debug, Clone, Serialize)]
pub struct WriteFailure {
    /// Monotonic, so the UI can tell a new failure from one it already showed.
    pub seq: u64,
    pub record_id: String,
    /// Already user-safe: every `CoreError` message is written for a human.
    pub message: String,
}

/// How long to keep believing a pending write before giving up on it.
pub const PENDING_TIMEOUT_MILLIS: i64 = 60_000;

/// Rejections worth remembering. They are never cleared by a poll — a poll
/// completing a few milliseconds after a rejection would otherwise erase the
/// only record of it and leave the row silently snapped back. The UI shows
/// each one once, keyed on `seq`, so carrying a short history costs nothing.
const MAX_REMEMBERED_FAILURES: usize = 8;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub tasks: Vec<Task>,
    pub fetched_at_millis: i64,
    /// Writes still awaiting confirmation — the UI shows a subtle marker.
    pub pending_ids: Vec<String>,
    /// Writes the server refused, newest last. The UI reports each `seq` once.
    pub write_failures: Vec<WriteFailure>,
    /// Set when the last refresh failed; the UI shows stale data with a warning
    /// rather than an empty screen.
    pub stale: bool,
}

pub struct Store<R: TaskRepository> {
    /// Shared so a background write can keep using it with the lock released.
    repo: Arc<R>,
    polled: Vec<Task>,
    pending: HashMap<String, PendingWrite>,
    failures: Vec<WriteFailure>,
    next_seq: u64,
    fetched_at_millis: i64,
    stale: bool,
}

impl<R: TaskRepository> Store<R> {
    pub fn new(repo: R) -> Self {
        Store {
            repo: Arc::new(repo),
            polled: Vec::new(),
            pending: HashMap::new(),
            failures: Vec::new(),
            next_seq: 1,
            fetched_at_millis: 0,
            stale: false,
        }
    }

    /// A handle that outlives the lock, for I/O that must not hold it.
    pub fn repo(&self) -> Arc<R> {
        self.repo.clone()
    }

    /// The view handed to the UI: polled data with pending writes laid over it.
    pub fn snapshot(&self, viewer: &Viewer) -> Snapshot {
        let merged = self
            .polled
            .iter()
            .map(|task| match self.pending.get(&task.record_id) {
                Some(p) => p.patch.apply_to(task),
                None => task.clone(),
            })
            .collect();

        Snapshot {
            tasks: only_mine(merged, viewer),
            fetched_at_millis: self.fetched_at_millis,
            pending_ids: self.pending.keys().cloned().collect(),
            write_failures: self.failures.clone(),
            stale: self.stale,
        }
    }

    pub fn fetched_at_millis(&self) -> i64 {
        self.fetched_at_millis
    }

    /// Fold a completed poll in and retire any pending write it confirms.
    pub fn apply_poll(&mut self, tasks: Vec<Task>, now_millis: i64) {
        self.polled = tasks;
        self.fetched_at_millis = now_millis;
        self.stale = false;
        self.retire_confirmed(now_millis);
    }

    /// A failed refresh keeps the previous snapshot and flags it stale: showing
    /// yesterday's tasks with a warning beats showing none.
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// Drop pending writes the server has caught up with, or that timed out.
    fn retire_confirmed(&mut self, now_millis: i64) {
        let polled = &self.polled;
        self.pending.retain(|record_id, pending| {
            let Some(task) = polled.iter().find(|t| &t.record_id == record_id) else {
                // The record is gone from the server — nothing left to overlay.
                return false;
            };
            let moved_on = match (task.modified, pending.base_modified) {
                (Some(current), Some(base)) => current > base,
                // Without timestamps to compare, fall back to the timeout alone.
                _ => false,
            };
            let expired = now_millis - pending.sent_at_millis > PENDING_TIMEOUT_MILLIS;
            !moved_on && !expired
        });
    }

    /// Overlay a write and hand back the ticket its completion must quote.
    ///
    /// Returns before anything is sent. The caller performs the request with
    /// the lock released and reports back through `settle_update`.
    ///
    /// Validation happens here rather than in the transport: a single-select
    /// silently gains a new option when written an unknown value, so a bad
    /// value must be refused while there is still a caller to refuse it to.
    /// Once the overlay is laid down the UI has already been told "done".
    pub fn begin_update(
        &mut self,
        record_id: &str,
        patch: TaskPatch,
        now_millis: i64,
    ) -> Result<u64> {
        if record_id.trim().is_empty() {
            return Err(CoreError::Config("update requires a record id".into()));
        }
        if patch.is_empty() {
            return Err(CoreError::Config("Nothing to update".into()));
        }
        patch.validate()?;

        let seq = self.next_seq;
        self.next_seq += 1;

        // A second click before the first confirms must not drop the first
        // click's field: merge intent, keep the oldest baseline so the overlay
        // only retires once the server has moved past where we started.
        let previous = self.pending.get(record_id);
        let merged = match previous {
            Some(p) => merge_patches(&p.patch, &patch),
            None => patch,
        };
        let base_modified = previous.and_then(|p| p.base_modified).or_else(|| {
            self.polled.iter().find(|t| t.record_id == record_id).and_then(|t| t.modified)
        });

        self.pending.insert(
            record_id.to_string(),
            PendingWrite { patch: merged, base_modified, sent_at_millis: now_millis, seq },
        );
        Ok(seq)
    }

    /// Report the outcome of the write `seq` started.
    ///
    /// On success the server's own record is folded straight in rather than
    /// waiting for the next tick. On failure the overlay goes and the row
    /// snaps back to the server's value, with the reason kept for the UI.
    pub fn settle_update(&mut self, record_id: &str, seq: u64, outcome: Result<Task>) {
        // A later click already replaced this overlay. Its own completion owns
        // the overlay now, so only the server record is worth keeping.
        let still_ours = self.pending.get(record_id).map(|p| p.seq) == Some(seq);

        match outcome {
            Ok(updated) => {
                if let Some(slot) = self.polled.iter_mut().find(|t| t.record_id == record_id) {
                    *slot = updated;
                }
                if still_ours {
                    self.pending.remove(record_id);
                }
            }
            Err(err) => {
                if still_ours {
                    self.pending.remove(record_id);
                }
                self.remember_failure(record_id, seq, err);
            }
        }
    }

    fn remember_failure(&mut self, record_id: &str, seq: u64, err: CoreError) {
        self.failures.push(WriteFailure {
            seq,
            record_id: record_id.to_string(),
            message: err.to_string(),
        });
        if self.failures.len() > MAX_REMEMBERED_FAILURES {
            self.failures.remove(0);
        }
    }

    pub async fn create(&mut self, patch: TaskPatch) -> Result<Task> {
        let created = self.repo.create(&patch).await?;
        self.polled.push(created.clone());
        Ok(created)
    }

    pub async fn delete(&mut self, record_id: &str) -> Result<()> {
        self.repo.delete(record_id).await?;
        self.polled.retain(|t| t.record_id != record_id);
        self.pending.remove(record_id);
        Ok(())
    }
}

/// `next` laid over `base`, field by field. Returns a new patch; neither input
/// is touched.
fn merge_patches(base: &TaskPatch, next: &TaskPatch) -> TaskPatch {
    TaskPatch {
        title: next.title.clone().or_else(|| base.title.clone()),
        status: next.status.clone().or_else(|| base.status.clone()),
        priority: next.priority.clone().or_else(|| base.priority.clone()),
        remarks: next.remarks.clone().or_else(|| base.remarks.clone()),
        owner_ids: next.owner_ids.clone().or_else(|| base.owner_ids.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_repo::{me, task_named, FakeRepo};

    /// The reconciliation rules are pure, so they are exercised directly here.
    /// The locking and background-write rules live in `store_cell`.
    fn seeded(tasks: Vec<Task>) -> Store<FakeRepo> {
        let mut store = Store::new(FakeRepo::with(tasks.clone()));
        store.apply_poll(tasks, 1_000);
        store
    }

    #[test]
    fn snapshot_shows_only_the_viewers_tasks() {
        let store = seeded(vec![task_named("mine", "ou_me"), task_named("theirs", "ou_x")]);

        let snap = store.snapshot(&me());
        assert_eq!(snap.tasks.len(), 1);
        assert_eq!(snap.tasks[0].title, "mine");
    }

    #[test]
    fn an_overlay_is_visible_the_instant_the_write_begins() {
        // The whole point of R5: no network has happened yet.
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        let patch = TaskPatch { status: Some("In Progress".into()), ..Default::default() };

        store.begin_update("rec_a", patch, 2_000).unwrap();

        let snap = store.snapshot(&me());
        assert_eq!(snap.tasks[0].status, "In Progress");
        assert_eq!(snap.pending_ids, vec!["rec_a".to_string()]);
    }

    #[test]
    fn a_made_up_status_is_refused_before_any_overlay_is_laid_down() {
        // Lark would ADD it as a new single-select option for all 14 people,
        // and the UI would have already shown it as applied.
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        let patch = TaskPatch { status: Some("Nonsense".into()), ..Default::default() };

        let err = store.begin_update("rec_a", patch, 2_000).unwrap_err().to_string();

        assert!(err.contains("not a valid status"), "got: {err}");
        assert!(store.snapshot(&me()).pending_ids.is_empty(), "nothing may be overlaid");
        assert_eq!(store.snapshot(&me()).tasks[0].status, "Backlog");
    }

    #[test]
    fn an_empty_patch_is_refused_rather_than_sent() {
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        assert!(store.begin_update("rec_a", TaskPatch::default(), 2_000).is_err());
        assert!(store.begin_update("", status_patch("Done"), 2_000).is_err());
    }

    #[test]
    fn an_edit_does_not_revert_when_a_stale_poll_lands() {
        // The regression this whole module exists to prevent.
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        store.begin_update("rec_a", status_patch("In Progress"), 1_000).unwrap();

        // A poll returns the pre-write snapshot (Modified unchanged).
        store.apply_poll(vec![task_named("a", "ou_me")], 2_000);

        let snap = store.snapshot(&me());
        assert_eq!(
            snap.tasks[0].status, "In Progress",
            "the user's edit must survive a poll that predates it"
        );
        assert_eq!(snap.pending_ids.len(), 1);
    }

    #[test]
    fn pending_overlay_retires_once_the_server_catches_up() {
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        store.begin_update("rec_a", status_patch("In Progress"), 1_000).unwrap();

        let mut landed = task_named("a", "ou_me");
        landed.status = "In Progress".into();
        landed.modified = Some(2_000);
        store.apply_poll(vec![landed], 3_000);

        assert!(store.snapshot(&me()).pending_ids.is_empty(), "overlay should be retired");
    }

    #[test]
    fn a_stuck_pending_write_expires_rather_than_sticking_forever() {
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        store.begin_update("rec_a", status_patch("In Progress"), 0).unwrap();

        store.apply_poll(vec![task_named("a", "ou_me")], PENDING_TIMEOUT_MILLIS + 1);

        assert!(
            store.snapshot(&me()).pending_ids.is_empty(),
            "a write that never confirms must not overlay forever"
        );
    }

    #[test]
    fn a_rejected_write_snaps_back_and_says_why() {
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        let seq = store.begin_update("rec_a", status_patch("Done"), 1_000).unwrap();

        store.settle_update("rec_a", seq, Err(CoreError::Forbidden));

        let snap = store.snapshot(&me());
        assert!(snap.pending_ids.is_empty(), "a rejected write must not look applied");
        assert_eq!(snap.tasks[0].status, "Backlog", "UI must show the real server value");
        assert_eq!(snap.write_failures.len(), 1, "silent reversion is not acceptable");
        assert_eq!(snap.write_failures[0].record_id, "rec_a");
        assert_eq!(snap.write_failures[0].message, CoreError::Forbidden.to_string());
    }

    #[test]
    fn a_rejection_message_never_leaks_internals() {
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        let seq = store.begin_update("rec_a", status_patch("Done"), 1_000).unwrap();

        store.settle_update("rec_a", seq, Err(CoreError::Unauthorized));

        let message = &store.snapshot(&me()).write_failures[0].message;
        assert!(!message.contains("token"), "user message must not mention tokens");
        assert!(!message.contains("Bearer"));
        assert!(message.contains("sign in"), "it must say what to do: {message}");
    }

    #[test]
    fn a_successful_write_folds_the_server_record_in_and_clears_the_overlay() {
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        let seq = store.begin_update("rec_a", status_patch("In Progress"), 1_000).unwrap();

        let mut confirmed = task_named("a", "ou_me");
        confirmed.status = "In Progress".into();
        confirmed.modified = Some(2_000);
        store.settle_update("rec_a", seq, Ok(confirmed));

        let snap = store.snapshot(&me());
        assert_eq!(snap.tasks[0].status, "In Progress");
        assert_eq!(snap.tasks[0].modified, Some(2_000));
        assert!(snap.pending_ids.is_empty());
        assert!(snap.write_failures.is_empty());
    }

    #[test]
    fn a_second_click_supersedes_the_first_and_the_first_cannot_undo_it() {
        // ← then ✓ in quick succession. The stale completion must not strip
        // the overlay the second click is still relying on.
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        let first = store.begin_update("rec_a", status_patch("Backlog"), 1_000).unwrap();
        let second = store.begin_update("rec_a", status_patch("In Progress"), 1_100).unwrap();
        assert_ne!(first, second);

        let mut as_backlog = task_named("a", "ou_me");
        as_backlog.modified = Some(1_500);
        store.settle_update("rec_a", first, Ok(as_backlog));

        let snap = store.snapshot(&me());
        assert_eq!(snap.tasks[0].status, "In Progress", "the latest intent must still show");
        assert_eq!(snap.pending_ids.len(), 1, "the second write is still in flight");
    }

    #[test]
    fn rapid_clicks_on_different_fields_do_not_lose_either_one() {
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        store.begin_update("rec_a", status_patch("In Progress"), 1_000).unwrap();
        let priority =
            TaskPatch { priority: Some("P0 - Critical".into()), ..Default::default() };
        store.begin_update("rec_a", priority, 1_100).unwrap();

        let snap = store.snapshot(&me());
        assert_eq!(snap.tasks[0].status, "In Progress", "the earlier field must survive");
        assert_eq!(snap.tasks[0].priority.as_deref(), Some("P0 - Critical"));
    }

    #[test]
    fn a_rejection_survives_a_poll_that_lands_right_after_it() {
        // The 20s poll can complete milliseconds after a rejection. If the
        // poll wiped the failure the row would snap back with no explanation,
        // which is precisely the outcome R5 forbids.
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        let seq = store.begin_update("rec_a", status_patch("Done"), 1_000).unwrap();
        store.settle_update("rec_a", seq, Err(CoreError::Forbidden));

        store.apply_poll(vec![task_named("a", "ou_me")], 1_001);

        let snap = store.snapshot(&me());
        assert_eq!(snap.write_failures.len(), 1, "the user must still be told");
        assert_eq!(snap.write_failures[0].seq, seq);
    }

    #[test]
    fn remembered_rejections_are_capped() {
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        for i in 0..(MAX_REMEMBERED_FAILURES + 4) {
            let seq = store.begin_update("rec_a", status_patch("Done"), 1_000 + i as i64).unwrap();
            store.settle_update("rec_a", seq, Err(CoreError::Forbidden));
        }
        assert_eq!(store.snapshot(&me()).write_failures.len(), MAX_REMEMBERED_FAILURES);
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_snapshot_and_flags_it() {
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        store.mark_stale();

        let snap = store.snapshot(&me());
        assert_eq!(snap.tasks.len(), 1, "keep showing what we had");
        assert!(snap.stale, "but tell the user it is stale");
        assert_eq!(snap.fetched_at_millis, 1_000, "timestamp is of the last good fetch");
    }

    #[tokio::test]
    async fn delete_removes_the_task_and_any_overlay() {
        let mut store = seeded(vec![task_named("a", "ou_me")]);
        store.begin_update("rec_a", status_patch("Done"), 1_000).unwrap();
        store.delete("rec_a").await.unwrap();

        let snap = store.snapshot(&me());
        assert!(snap.tasks.is_empty());
        assert!(snap.pending_ids.is_empty());
    }

    #[test]
    fn merge_keeps_the_newer_value_and_never_mutates_its_inputs() {
        let base = TaskPatch { status: Some("Backlog".into()), ..Default::default() };
        let next = TaskPatch {
            status: Some("Done".into()),
            remarks: Some("note".into()),
            ..Default::default()
        };
        let merged = merge_patches(&base, &next);

        assert_eq!(base.status.as_deref(), Some("Backlog"), "inputs must be untouched");
        assert_eq!(merged.status.as_deref(), Some("Done"));
        assert_eq!(merged.remarks.as_deref(), Some("note"));
    }

    fn status_patch(status: &str) -> TaskPatch {
        TaskPatch { status: Some(status.into()), ..Default::default() }
    }
}
