//! The storage seam.
//!
//! `sync` is generic over this trait so the reconciliation rules — the most
//! bug-prone part of the app — can be tested against a fake without HTTP.

use std::collections::BTreeMap;
use std::future::Future;

use chrono::{DateTime, Local};
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
    /// Epoch milliseconds. A Bitable datetime rejects a formatted string
    /// (1254064), so the day is carried as an instant and rendered by the Base.
    pub completed_date: Option<i64>,
}

/// The instant to stamp on a task being marked Done.
///
/// Noon rather than midnight on purpose: a Bitable datetime is an absolute
/// instant that the Base renders in *its own* timezone, so a midnight stamp
/// lands on the previous day for anyone a few hours west of it. Noon survives
/// a twelve-hour disagreement in either direction.
pub fn completion_stamp_millis(now: DateTime<Local>) -> i64 {
    let noon = now.date_naive().and_hms_opt(12, 0, 0).expect("12:00 exists on every day");
    noon.and_local_timezone(*now.offset())
        .single()
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|| now.timestamp_millis())
}

/// Stamp the completion day onto a patch that marks a task Done.
///
/// Returns a new patch; the input is untouched. Only a Done write carries a
/// date, and the value is decided here rather than in the webview — the UI
/// does not get to choose when a task was finished. A patch that does not
/// set Done leaves the field absent, so an unrelated edit to a Done row never
/// re-stamps it. That re-stamping was the original defect.
pub fn stamp_completion(patch: TaskPatch, now: DateTime<Local>) -> TaskPatch {
    let completing = patch.status.as_deref() == Some(status::DONE);
    TaskPatch {
        completed_date: completing.then(|| completion_stamp_millis(now)),
        ..patch
    }
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
                return Err(CoreError::Invalid(format!(
                    "\"{v}\" is not a valid status. Expected one of: {}",
                    status::ALL.join(", ")
                )));
            }
        }
        if let Some(v) = &self.priority {
            if !priority::is_valid(v) {
                return Err(CoreError::Invalid(format!(
                    "\"{v}\" is not a valid priority. Expected one of: {}",
                    priority::ALL.join(", ")
                )));
            }
        }
        if let Some(t) = &self.title {
            if t.trim().is_empty() {
                return Err(CoreError::Invalid("A task needs a title.".into()));
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
            && self.completed_date.is_none()
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
        if let Some(millis) = self.completed_date {
            out.insert(fields::COMPLETED_DATE.to_string(), Value::Number(millis.into()));
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
        if let Some(millis) = self.completed_date {
            next.completed_date = Some(millis);
        }
        next
    }
}

pub trait TaskRepository: Send + Sync {
    /// Every record in the table. Still here after the server-side filter
    /// landed: it is what the sign-in cross-check compares against, what the
    /// fallback path uses, and what a future team view would need.
    fn list_all(&self) -> impl Future<Output = Result<Vec<Task>>> + Send;

    /// Only the records the given person owns, filtered by the server.
    ///
    /// A parameter rather than a constant so this is an optimisation of the
    /// fetch, not a second access rule. `only_mine` remains the access gate.
    fn list_owned_by(&self, open_id: &str) -> impl Future<Output = Result<Vec<Task>>> + Send;
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
    fn a_done_write_carries_the_day_it_was_completed() {
        let patch = TaskPatch { status: Some("Done".into()), ..Default::default() };
        let stamped = stamp_completion(patch.clone(), fixed_now());

        assert!(patch.completed_date.is_none(), "the input must be untouched");
        let f = stamped.to_fields();
        assert_eq!(f.len(), 2, "status and the completion date, nothing else");
        assert_eq!(
            f.get(fields::COMPLETED_DATE).and_then(Value::as_i64),
            Some(completion_stamp_millis(fixed_now()))
        );
    }

    #[test]
    fn editing_another_field_on_a_done_row_does_not_restamp_it() {
        // The defect this field exists to fix: `Completed Month` used to be
        // derived from `Modified`, so any later touch moved it.
        let patch = TaskPatch { remarks: Some("picked back up".into()), ..Default::default() };
        let stamped = stamp_completion(patch, fixed_now());

        assert!(stamped.completed_date.is_none());
        assert!(!stamped.to_fields().contains_key(fields::COMPLETED_DATE));
    }

    #[test]
    fn moving_a_task_off_done_leaves_the_recorded_day_alone() {
        // Absent from the patch means absent from the request, so the Base
        // keeps what it has: last completion wins, never cleared.
        let patch = TaskPatch { status: Some("In Progress".into()), ..Default::default() };
        assert!(stamp_completion(patch, fixed_now()).completed_date.is_none());
    }

    #[test]
    fn the_webview_does_not_get_to_choose_the_completion_day() {
        let forged = TaskPatch {
            status: Some("Done".into()),
            completed_date: Some(1),
            ..Default::default()
        };
        assert_eq!(
            stamp_completion(forged, fixed_now()).completed_date,
            Some(completion_stamp_millis(fixed_now())),
            "the day is decided in Rust, not supplied by the UI"
        );
    }

    #[test]
    fn the_completion_stamp_lands_at_midday_on_the_right_day() {
        let now = fixed_now();
        let stamped = DateTime::from_timestamp_millis(completion_stamp_millis(now))
            .unwrap()
            .with_timezone(&now.timezone());
        assert_eq!(stamped.date_naive(), now.date_naive());
        assert_eq!(stamped.time().to_string(), "12:00:00");
    }

    #[test]
    fn a_completion_date_writes_as_epoch_millis_not_a_string() {
        // A Bitable datetime rejects a formatted string with 1254064.
        let patch = TaskPatch { completed_date: Some(1_760_000_000_000), ..Default::default() };
        assert!(patch.to_fields()[fields::COMPLETED_DATE].is_number());
    }

    #[test]
    fn a_blank_title_reads_as_a_user_mistake_not_a_broken_install() {
        let patch = TaskPatch { title: Some(" ".into()), ..Default::default() };
        let err = patch.validate().unwrap_err();
        assert!(matches!(err, CoreError::Invalid(_)), "got {err:?}");
        assert_eq!(err.to_string(), "A task needs a title.");
    }

    /// A stable instant with a real timezone offset, so the stamp can be
    /// asserted without depending on where the test runs.
    fn fixed_now() -> DateTime<Local> {
        DateTime::from_timestamp(1_760_000_000, 0).unwrap().with_timezone(&Local)
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
            completed_date: None,
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
