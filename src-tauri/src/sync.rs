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

use std::collections::HashMap;

use serde::Serialize;

use crate::error::Result;
use crate::repo::{TaskPatch, TaskRepository};
use crate::task::{only_mine, Task, Viewer};

/// A write we have sent but not yet seen reflected in a poll.
#[derive(Debug, Clone)]
pub struct PendingWrite {
    pub patch: TaskPatch,
    /// `Modified` of the record when we sent the write, if it was known.
    pub base_modified: Option<i64>,
    pub sent_at_millis: i64,
}

/// How long to keep believing a pending write before giving up on it.
pub const PENDING_TIMEOUT_MILLIS: i64 = 60_000;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub tasks: Vec<Task>,
    pub fetched_at_millis: i64,
    /// Writes still awaiting confirmation — the UI can show a subtle marker.
    pub pending_ids: Vec<String>,
    /// Set when the last refresh failed; the UI shows stale data with a warning
    /// rather than an empty screen.
    pub stale: bool,
}

pub struct Store<R: TaskRepository> {
    repo: R,
    polled: Vec<Task>,
    pending: HashMap<String, PendingWrite>,
    fetched_at_millis: i64,
    stale: bool,
}

impl<R: TaskRepository> Store<R> {
    pub fn new(repo: R) -> Self {
        Store {
            repo,
            polled: Vec::new(),
            pending: HashMap::new(),
            fetched_at_millis: 0,
            stale: false,
        }
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
            stale: self.stale,
        }
    }

    /// Fetch from the server and retire any pending write it confirms.
    ///
    /// A failed refresh keeps the previous snapshot and flags it stale: showing
    /// yesterday's tasks with a warning beats showing none.
    pub async fn refresh(&mut self, now_millis: i64) -> Result<()> {
        match self.repo.list_all().await {
            Ok(tasks) => {
                self.polled = tasks;
                self.fetched_at_millis = now_millis;
                self.stale = false;
                self.retire_confirmed(now_millis);
                Ok(())
            }
            Err(err) => {
                self.stale = true;
                Err(err)
            }
        }
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

    /// Send a field-level update and overlay it until a poll confirms it.
    pub async fn update(
        &mut self,
        record_id: &str,
        patch: TaskPatch,
        now_millis: i64,
    ) -> Result<Task> {
        let base_modified = self
            .polled
            .iter()
            .find(|t| t.record_id == record_id)
            .and_then(|t| t.modified);

        self.pending.insert(
            record_id.to_string(),
            PendingWrite { patch: patch.clone(), base_modified, sent_at_millis: now_millis },
        );

        match self.repo.update(record_id, &patch).await {
            Ok(updated) => {
                // Fold the server's own answer straight in rather than waiting
                // for the next tick.
                if let Some(slot) = self.polled.iter_mut().find(|t| t.record_id == record_id) {
                    *slot = updated.clone();
                }
                self.pending.remove(record_id);
                Ok(updated)
            }
            Err(err) => {
                // The write failed, so the overlay is a lie — remove it.
                self.pending.remove(record_id);
                Err(err)
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CoreError;
    use std::sync::Mutex;

    /// A repository that serves canned data and records what it was sent.
    struct FakeRepo {
        tasks: Mutex<Vec<Task>>,
        fail_next: Mutex<bool>,
        last_patch: Mutex<Option<TaskPatch>>,
    }

    impl FakeRepo {
        fn with(tasks: Vec<Task>) -> Self {
            FakeRepo {
                tasks: Mutex::new(tasks),
                fail_next: Mutex::new(false),
                last_patch: Mutex::new(None),
            }
        }
    }

    impl TaskRepository for FakeRepo {
        async fn list_all(&self) -> Result<Vec<Task>> {
            if *self.fail_next.lock().unwrap() {
                return Err(CoreError::Unauthorized);
            }
            Ok(self.tasks.lock().unwrap().clone())
        }

        async fn create(&self, patch: &TaskPatch) -> Result<Task> {
            let mut task = task_named("new", "ou_me");
            task.record_id = "rec_new".into();
            if let Some(t) = &patch.title {
                task.title = t.clone();
            }
            Ok(task)
        }

        async fn update(&self, record_id: &str, patch: &TaskPatch) -> Result<Task> {
            *self.last_patch.lock().unwrap() = Some(patch.clone());
            if *self.fail_next.lock().unwrap() {
                return Err(CoreError::Forbidden);
            }
            let tasks = self.tasks.lock().unwrap();
            let base = tasks.iter().find(|t| t.record_id == record_id).unwrap();
            let mut updated = patch.apply_to(base);
            updated.modified = Some(base.modified.unwrap_or(0) + 1_000);
            Ok(updated)
        }

        async fn delete(&self, _record_id: &str) -> Result<()> {
            Ok(())
        }
    }

    fn task_named(title: &str, owner: &str) -> Task {
        Task {
            record_id: format!("rec_{title}"),
            title: title.into(),
            status: "Backlog".into(),
            owners: vec![crate::task::Person { id: owner.into(), name: "Me".into() }],
            priority: None,
            category: None,
            workstream: None,
            remarks: None,
            due_date: None,
            created: Some(1_000),
            modified: Some(1_000),
        }
    }

    fn me() -> Viewer {
        Viewer::new("ou_me", "Me")
    }

    #[tokio::test]
    async fn snapshot_shows_only_the_viewers_tasks() {
        let repo = FakeRepo::with(vec![task_named("mine", "ou_me"), task_named("theirs", "ou_x")]);
        let mut store = Store::new(repo);
        store.refresh(5_000).await.unwrap();

        let snap = store.snapshot(&me());
        assert_eq!(snap.tasks.len(), 1);
        assert_eq!(snap.tasks[0].title, "mine");
    }

    #[tokio::test]
    async fn a_write_sends_only_the_changed_field() {
        let repo = FakeRepo::with(vec![task_named("a", "ou_me")]);
        let mut store = Store::new(repo);
        store.refresh(0).await.unwrap();

        let patch = TaskPatch { status: Some("Done".into()), ..Default::default() };
        store.update("rec_a", patch, 1_000).await.unwrap();

        let sent = store.repo.last_patch.lock().unwrap().clone().unwrap();
        assert_eq!(sent.status.as_deref(), Some("Done"));
        assert!(sent.title.is_none(), "must not resend an untouched field");
        assert!(sent.remarks.is_none());
    }

    #[tokio::test]
    async fn an_edit_does_not_revert_when_a_stale_poll_lands() {
        // The regression this whole module exists to prevent.
        let repo = FakeRepo::with(vec![task_named("a", "ou_me")]);
        let mut store = Store::new(repo);
        store.refresh(0).await.unwrap();

        // Pretend the write is in flight: overlay set, server not yet updated.
        store.pending.insert(
            "rec_a".to_string(),
            PendingWrite {
                patch: TaskPatch { status: Some("In Progress".into()), ..Default::default() },
                base_modified: Some(1_000),
                sent_at_millis: 1_000,
            },
        );

        // A poll returns the pre-write snapshot (Modified unchanged).
        store.refresh(2_000).await.unwrap();

        let snap = store.snapshot(&me());
        assert_eq!(
            snap.tasks[0].status, "In Progress",
            "the user's edit must survive a poll that predates it"
        );
        assert_eq!(snap.pending_ids.len(), 1);
    }

    #[tokio::test]
    async fn pending_overlay_retires_once_the_server_catches_up() {
        let repo = FakeRepo::with(vec![task_named("a", "ou_me")]);
        let mut store = Store::new(repo);
        store.refresh(0).await.unwrap();
        store.pending.insert(
            "rec_a".to_string(),
            PendingWrite {
                patch: TaskPatch { status: Some("In Progress".into()), ..Default::default() },
                base_modified: Some(1_000),
                sent_at_millis: 1_000,
            },
        );

        // Server now reports a newer Modified — the write has landed.
        store.repo.tasks.lock().unwrap()[0].modified = Some(2_000);
        store.repo.tasks.lock().unwrap()[0].status = "In Progress".into();
        store.refresh(3_000).await.unwrap();

        assert!(store.snapshot(&me()).pending_ids.is_empty(), "overlay should be retired");
    }

    #[tokio::test]
    async fn a_stuck_pending_write_expires_rather_than_sticking_forever() {
        let repo = FakeRepo::with(vec![task_named("a", "ou_me")]);
        let mut store = Store::new(repo);
        store.refresh(0).await.unwrap();
        store.pending.insert(
            "rec_a".to_string(),
            PendingWrite {
                patch: TaskPatch { status: Some("In Progress".into()), ..Default::default() },
                base_modified: Some(1_000),
                sent_at_millis: 0,
            },
        );

        store.refresh(PENDING_TIMEOUT_MILLIS + 1).await.unwrap();
        assert!(
            store.snapshot(&me()).pending_ids.is_empty(),
            "a write that never confirms must not overlay forever"
        );
    }

    #[tokio::test]
    async fn a_failed_write_leaves_no_overlay() {
        let repo = FakeRepo::with(vec![task_named("a", "ou_me")]);
        let mut store = Store::new(repo);
        store.refresh(0).await.unwrap();
        *store.repo.fail_next.lock().unwrap() = true;

        let patch = TaskPatch { status: Some("Done".into()), ..Default::default() };
        let result = store.update("rec_a", patch, 1_000).await;

        assert!(result.is_err());
        let snap = store.snapshot(&me());
        assert!(snap.pending_ids.is_empty(), "a rejected write must not look applied");
        assert_eq!(snap.tasks[0].status, "Backlog", "UI must show the real server value");
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_last_snapshot_and_flags_it() {
        let repo = FakeRepo::with(vec![task_named("a", "ou_me")]);
        let mut store = Store::new(repo);
        store.refresh(1_000).await.unwrap();
        *store.repo.fail_next.lock().unwrap() = true;

        assert!(store.refresh(2_000).await.is_err());
        let snap = store.snapshot(&me());
        assert_eq!(snap.tasks.len(), 1, "keep showing what we had");
        assert!(snap.stale, "but tell the user it is stale");
        assert_eq!(snap.fetched_at_millis, 1_000, "timestamp is of the last good fetch");
    }

    #[tokio::test]
    async fn delete_removes_the_task_and_any_overlay() {
        let repo = FakeRepo::with(vec![task_named("a", "ou_me")]);
        let mut store = Store::new(repo);
        store.refresh(0).await.unwrap();
        store.delete("rec_a").await.unwrap();

        let snap = store.snapshot(&me());
        assert!(snap.tasks.is_empty());
        assert!(snap.pending_ids.is_empty());
    }
}
