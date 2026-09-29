//! The storage seam.
//!
//! `sync` is generic over this trait so the reconciliation rules — the most
//! bug-prone part of the app — can be tested against a fake without HTTP.

use std::collections::BTreeMap;
use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{CoreError, Result};
use crate::task::{fields, priority, status, Task};

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
    /// Reject values the Base does not already define.
    ///
    /// A single-select silently gains a new option when written an unknown
    /// value, so an unvalidated write is a schema change to a table 14 people
    /// share. Checked here, before anything reaches the network.
    pub fn validate(&self) -> Result<()> {
        if let Some(v) = &self.status {
            if !status::is_valid(v) {
                return Err(CoreError::Config(format!(
                    "\"{v}\" is not a valid status. Expected one of: {}",
                    status::ALL.join(", ")
                )));
            }
        }
        if let Some(v) = &self.priority {
            if !priority::is_valid(v) {
                return Err(CoreError::Config(format!(
                    "\"{v}\" is not a valid priority. Expected one of: {}",
                    priority::ALL.join(", ")
                )));
            }
        }
        if let Some(t) = &self.title {
            if t.trim().is_empty() {
                return Err(CoreError::Config("A task needs a title.".into()));
            }
        }
        Ok(())
    }

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
            // A Person cell takes objects, not bare ids. Sending ["ou_x"]
            // fails with UserFieldConvFail (1254066), verified against the
            // live Base.
            let people = ids
                .iter()
                .map(|id| serde_json::json!({ "id": id }))
                .collect();
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
    fn owner_writes_as_objects_not_bare_ids() {
        // Regression: bare id strings are rejected with UserFieldConvFail.
        let patch = TaskPatch {
            owner_ids: Some(vec!["ou_a".into(), "ou_b".into()]),
            ..Default::default()
        };
        let f = patch.to_fields();
        let owners = f.get(fields::OWNER).and_then(Value::as_array).unwrap();
        assert_eq!(owners.len(), 2);
        assert!(owners[0].is_object(), "a Person cell takes objects, not strings");
        assert_eq!(owners[0].get("id").and_then(Value::as_str), Some("ou_a"));
        assert_eq!(owners[1].get("id").and_then(Value::as_str), Some("ou_b"));
    }

    #[test]
    fn a_made_up_status_is_refused_before_it_reaches_lark() {
        // Lark would ADD it as a new option, changing the schema for everyone.
        let patch = TaskPatch { status: Some("Nonsense".into()), ..Default::default() };
        let err = patch.validate().unwrap_err().to_string();
        assert!(err.contains("not a valid status"), "got: {err}");
    }

    #[test]
    fn every_real_status_passes_validation() {
        for s in crate::task::status::ALL {
            let patch = TaskPatch { status: Some(s.into()), ..Default::default() };
            assert!(patch.validate().is_ok(), "{s} should be accepted");
        }
    }

    #[test]
    fn a_made_up_priority_is_refused() {
        let patch = TaskPatch { priority: Some("P9".into()), ..Default::default() };
        assert!(patch.validate().is_err());
    }

    #[test]
    fn every_real_priority_passes_validation() {
        for p in crate::task::priority::ALL {
            let patch = TaskPatch { priority: Some(p.into()), ..Default::default() };
            assert!(patch.validate().is_ok(), "{p} should be accepted");
        }
    }

    #[test]
    fn a_blank_title_is_refused() {
        let patch = TaskPatch { title: Some("   ".into()), ..Default::default() };
        assert!(patch.validate().is_err());
    }

    #[test]
    fn a_patch_that_touches_nothing_validates() {
        assert!(TaskPatch::default().validate().is_ok());
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
