//! The storage seam.
//!
//! `sync` is generic over this trait so the reconciliation rules — the most
//! bug-prone part of the app — can be tested against a fake without HTTP.

use std::collections::BTreeMap;
use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::task::{fields, Task};

/// A partial update: only the fields the user actually changed.
///
/// Writes are field-level on purpose. The OMSN plugin edits the same records,
/// so sending a whole `Task` back would overwrite a column someone else just
/// changed. `Task` is the read shape; this is the write shape, and they are
/// deliberately different types.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub remarks: Option<String>,
    /// Owner writes as a list of open_ids, not the `{id, name}` objects it reads as.
    pub owner_ids: Option<Vec<String>>,
}

impl TaskPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.status.is_none()
            && self.priority.is_none()
            && self.remarks.is_none()
            && self.owner_ids.is_none()
    }

    /// Render to the Lark `fields` map, omitting anything untouched.
    pub fn to_fields(&self) -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        if let Some(v) = &self.title {
            out.insert(fields::TITLE.to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.status {
            out.insert(fields::STATUS.to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.priority {
            out.insert(fields::PRIORITY.to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.remarks {
            out.insert(fields::REMARKS.to_string(), Value::String(v.clone()));
        }
        if let Some(ids) = &self.owner_ids {
            let people = ids.iter().map(|id| Value::String(id.clone())).collect();
            out.insert(fields::OWNER.to_string(), Value::Array(people));
        }
        out
    }

    /// Apply this patch to a task, returning a new value. Never mutates.
    pub fn apply_to(&self, task: &Task) -> Task {
        let mut next = task.clone();
        if let Some(v) = &self.title {
            next.title = v.clone();
        }
        if let Some(v) = &self.status {
            next.status = v.clone();
        }
        if let Some(v) = &self.priority {
            next.priority = Some(v.clone());
        }
        if let Some(v) = &self.remarks {
            next.remarks = Some(v.clone());
        }
        next
    }
}

pub trait TaskRepository: Send + Sync {
    fn list_all(&self) -> impl Future<Output = Result<Vec<Task>>> + Send;
    fn create(&self, patch: &TaskPatch) -> impl Future<Output = Result<Task>> + Send;
    fn update(&self, record_id: &str, patch: &TaskPatch)
        -> impl Future<Output = Result<Task>> + Send;
    fn delete(&self, record_id: &str) -> impl Future<Output = Result<()>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_patch_renders_no_fields() {
        assert!(TaskPatch::default().is_empty());
        assert!(TaskPatch::default().to_fields().is_empty());
    }

    #[test]
    fn only_touched_fields_are_sent() {
        let patch = TaskPatch { status: Some("Done".into()), ..Default::default() };
        let f = patch.to_fields();
        assert_eq!(f.len(), 1, "an untouched field must not be transmitted");
        assert_eq!(f.get(fields::STATUS).and_then(Value::as_str), Some("Done"));
        assert!(!f.contains_key(fields::REMARKS));
    }

    #[test]
    fn owner_writes_as_a_list_of_ids() {
        let patch = TaskPatch {
            owner_ids: Some(vec!["ou_a".into(), "ou_b".into()]),
            ..Default::default()
        };
        let f = patch.to_fields();
        let owners = f.get(fields::OWNER).and_then(Value::as_array).unwrap();
        assert_eq!(owners.len(), 2);
        assert_eq!(owners[0].as_str(), Some("ou_a"));
    }

    #[test]
    fn apply_does_not_mutate_the_original() {
        let original = Task {
            record_id: "r".into(),
            title: "before".into(),
            status: "Backlog".into(),
            owners: vec![],
            priority: None,
            category: None,
            workstream: None,
            remarks: None,
            due_date: None,
            created: None,
            modified: None,
        };
        let patch = TaskPatch { status: Some("In Progress".into()), ..Default::default() };
        let updated = patch.apply_to(&original);

        assert_eq!(original.status, "Backlog", "source must be untouched");
        assert_eq!(updated.status, "In Progress");
        assert_eq!(updated.title, "before", "unpatched fields carry over");
    }
}
