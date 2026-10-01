//! A `TaskRepository` that serves canned data, for tests only.
//!
//! Shared by `sync` (reconciliation rules) and `store_cell` (locking rules),
//! because both need the same double and the locking tests additionally need
//! to make a request *take time* — a poll that returns instantly cannot show
//! whether a click queued behind it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::error::{CoreError, Result};
use crate::repo::{TaskPatch, TaskRepository};
use crate::task::{Person, Task, Viewer};

pub struct FakeRepo {
    pub tasks: Mutex<Vec<Task>>,
    /// Makes every call fail until cleared, standing in for a dead session.
    pub fail_next: Mutex<bool>,
    pub last_patch: Mutex<Option<TaskPatch>>,
    /// Every patch sent, in order — the only way to see whether two rapid
    /// clicks produced two writes and in which order they reached the server.
    pub patches: Mutex<Vec<TaskPatch>>,
    pub list_delay: Mutex<Duration>,
    pub update_delay: Mutex<Duration>,
    pub list_calls: AtomicUsize,
}

impl FakeRepo {
    pub fn with(tasks: Vec<Task>) -> Self {
        FakeRepo {
            tasks: Mutex::new(tasks),
            fail_next: Mutex::new(false),
            last_patch: Mutex::new(None),
            patches: Mutex::new(Vec::new()),
            list_delay: Mutex::new(Duration::ZERO),
            update_delay: Mutex::new(Duration::ZERO),
            list_calls: AtomicUsize::new(0),
        }
    }

    pub fn sent_patches(&self) -> Vec<TaskPatch> {
        self.patches.lock().unwrap().clone()
    }

    /// Read a `Mutex` and drop the guard before any `await`: a std guard held
    /// across a yield point would make this future non-`Send`.
    fn delay(slot: &Mutex<Duration>) -> Duration {
        *slot.lock().unwrap()
    }
}

impl TaskRepository for FakeRepo {
    async fn list_all(&self) -> Result<Vec<Task>> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        let delay = Self::delay(&self.list_delay);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
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
        self.patches.lock().unwrap().push(patch.clone());

        let delay = Self::delay(&self.update_delay);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if *self.fail_next.lock().unwrap() {
            return Err(CoreError::Forbidden);
        }

        // Apply to the stored row so a later poll reports what the server now
        // holds, exactly as the live Base would.
        let mut tasks = self.tasks.lock().unwrap();
        let slot = tasks.iter_mut().find(|t| t.record_id == record_id).unwrap();
        let mut updated = patch.apply_to(slot);
        updated.modified = Some(slot.modified.unwrap_or(0) + 1_000);
        *slot = updated.clone();
        Ok(updated)
    }

    async fn delete(&self, _record_id: &str) -> Result<()> {
        Ok(())
    }
}

pub fn task_named(title: &str, owner: &str) -> Task {
    Task {
        record_id: format!("rec_{title}"),
        title: title.into(),
        status: "Backlog".into(),
        owners: vec![Person { id: owner.into(), name: "Me".into() }],
        priority: None,
        category: None,
        workstream: None,
        remarks: None,
        due_date: None,
        created: Some(1_000),
        modified: Some(1_000),
    }
}

pub fn me() -> Viewer {
    Viewer::new("ou_me", "Me")
}
