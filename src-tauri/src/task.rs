//! The task domain model.
//!
//! Lark Base cells are loosely typed — a single-select arrives as a bare
//! string or a one-element array, a person field as an array of objects. All
//! of that normalising happens here so the rest of the app sees clean data.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Field names as they exist in the team's Base. The app never invents
/// columns; changing the schema is explicitly out of scope.
pub mod fields {
    pub const TITLE: &str = "Title";
    pub const STATUS: &str = "Status";
    pub const OWNER: &str = "Owner";
    pub const PRIORITY: &str = "Priority";
    pub const CATEGORY: &str = "Category";
    pub const WORKSTREAM: &str = "Workstream";
    pub const REMARKS: &str = "Remarks";
    pub const DUE_DATE: &str = "Due Date";
    /// Datetime (`yyyy/MM/dd`), stamped by whoever marks a task Done.
    /// `Completed Month` is a formula over this field, so leaving it blank
    /// leaves the month blank too — honest, and better than a wrong date.
    pub const COMPLETED_DATE: &str = "Completed Date";
    pub const CREATED: &str = "Created";
    pub const MODIFIED: &str = "Modified";
}

/// Status values, verified against the live Base.
///
/// `THIS_WEEK` does not appear in the data today, but the OMSN plugin's
/// validator accepts it, so it is treated as active rather than silently
/// dropping such a task out of every view.
pub mod status {
    pub const BACKLOG: &str = "Backlog";
    pub const THIS_WEEK: &str = "This Week";
    pub const IN_PROGRESS: &str = "In Progress";
    pub const ON_HOLD: &str = "On Hold";
    pub const DONE: &str = "Done";

    /// Every value the Base's Status select accepts, verified against the
    /// live field definition.
    pub const ALL: [&str; 5] = [BACKLOG, THIS_WEEK, IN_PROGRESS, ON_HOLD, DONE];

    /// Lark *adds* an unknown value as a new option rather than rejecting it,
    /// so a typo from this app would silently pollute the shared Base's
    /// schema for all 14 people. Validate before writing, never after.
    pub fn is_valid(value: &str) -> bool {
        ALL.contains(&value)
    }
}

/// Priority values as stored in the Base — the full label, not a bare "P1".
pub mod priority {
    pub const P0: &str = "P0 - Critical";
    pub const P1: &str = "P1 - Important";
    pub const P2: &str = "P2 - Normal";

    pub const ALL: [&str; 3] = [P0, P1, P2];

    /// Same auto-create hazard as Status.
    pub fn is_valid(value: &str) -> bool {
        ALL.contains(&value)
    }

    /// Sort rank, lowest first. Unknown or empty sorts last.
    pub fn rank(value: Option<&str>) -> u8 {
        match value {
            Some(v) if v.starts_with("P0") => 0,
            Some(v) if v.starts_with("P1") => 1,
            Some(v) if v.starts_with("P2") => 2,
            _ => 3,
        }
    }
}

/// An item older than this with no movement needs a decision at standup.
pub const STALE_DAYS: i64 = 14;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Person {
    pub id: String,
    pub name: String,
}

/// The signed-in user.
///
/// A struct rather than loose `&str` arguments: `is_owned_by` is the app's
/// only access gate, and two interchangeable strings could be passed in the
/// wrong order at a call site without the compiler noticing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Viewer {
    pub open_id: String,
    pub display_name: String,
}

impl Viewer {
    pub fn new(open_id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Viewer { open_id: open_id.into(), display_name: display_name.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub record_id: String,
    pub title: String,
    pub status: String,
    pub owners: Vec<Person>,
    pub priority: Option<String>,
    pub category: Option<String>,
    pub workstream: Option<String>,
    pub remarks: Option<String>,
    pub due_date: Option<i64>,
    /// The day the work was finished, set when a client writes `Done`.
    /// Blank for anything completed in the Lark UI by hand, and for every
    /// record that predates this field — neither is guessed at.
    pub completed_date: Option<i64>,
    pub created: Option<i64>,
    pub modified: Option<i64>,
}

/// Pull a scalar out of a cell that may be a string or a one-element array.
fn scalar(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(items) => items.first().and_then(|v| match v {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Object(o) => o.get("text").and_then(Value::as_str).map(str::to_string),
            _ => None,
        }),
        Value::Object(o) => o.get("text").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// Lark returns dates as epoch milliseconds, sometimes as a numeric string.
fn epoch_millis(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s
            .parse::<i64>()
            .ok()
            .or_else(|| DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())),
        _ => None,
    }
}

fn people(value: Option<&Value>) -> Vec<Person> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let id = obj
                .get("id")
                .or_else(|| obj.get("open_id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = obj.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            if id.is_empty() && name.is_empty() {
                None
            } else {
                Some(Person { id, name })
            }
        })
        .collect()
}

impl Task {
    /// Build a task from one Bitable record. Unknown or missing cells become
    /// `None` rather than failing the whole fetch — one malformed row must not
    /// blank the user's board.
    pub fn from_record(record_id: String, f: &serde_json::Map<String, Value>) -> Self {
        Task {
            record_id,
            title: scalar(f.get(fields::TITLE)).unwrap_or_else(|| "(untitled)".to_string()),
            status: scalar(f.get(fields::STATUS)).unwrap_or_else(|| "Backlog".to_string()),
            owners: people(f.get(fields::OWNER)),
            priority: scalar(f.get(fields::PRIORITY)),
            category: scalar(f.get(fields::CATEGORY)),
            workstream: scalar(f.get(fields::WORKSTREAM)),
            remarks: scalar(f.get(fields::REMARKS)),
            due_date: epoch_millis(f.get(fields::DUE_DATE)),
            completed_date: epoch_millis(f.get(fields::COMPLETED_DATE)),
            created: epoch_millis(f.get(fields::CREATED)),
            modified: epoch_millis(f.get(fields::MODIFIED)),
        }
    }

    /// True when this task belongs to the given person.
    ///
    /// Matched on `open_id` only. `open_id` is app-scoped, but Lark resolves
    /// person fields into the *requesting* app's scope — verified against the
    /// live Base: records written by the OMSN plugin come back carrying this
    /// app's id for the same human. Display names are deliberately not used;
    /// they are mutable, can collide, and this is the app's only access gate.
    pub fn is_owned_by(&self, viewer: &Viewer) -> bool {
        !viewer.open_id.is_empty()
            && self.owners.iter().any(|p| !p.id.is_empty() && p.id == viewer.open_id)
    }

    pub fn is_unassigned(&self) -> bool {
        self.owners.is_empty()
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.status.as_str(),
            status::IN_PROGRESS | status::ON_HOLD | status::BACKLOG | status::THIS_WEEK
        )
    }

    /// Whole days since creation, using the supplied clock so tests are stable.
    pub fn age_days(&self, now: DateTime<Utc>) -> Option<i64> {
        let created = self.created?;
        Some((now.timestamp_millis() - created) / 86_400_000)
    }

    /// Whole days since the record last changed.
    pub fn days_since_movement(&self, now: DateTime<Utc>) -> Option<i64> {
        let last = self.modified.or(self.created)?;
        Some((now.timestamp_millis() - last) / 86_400_000)
    }

    /// Needs a decision: in flight and nothing has moved for STALE_DAYS.
    ///
    /// Measured from `Modified`, not `Created` — with the OMSN plugin writing
    /// to the same table, a touch by either client counts as movement.
    pub fn needs_attention(&self, now: DateTime<Utc>) -> bool {
        self.status == status::IN_PROGRESS
            && self.days_since_movement(now).is_some_and(|d| d >= STALE_DAYS)
    }
}

/// Keep only the signed-in user's tasks.
///
/// This is the single place the personal-first rule is applied, so there is
/// one obvious thing to change if the app ever grows a team view. It must stay
/// in the Rust core: filtering in the UI would mean the whole team's tasks
/// have already crossed the bridge into the webview.
pub fn only_mine(tasks: Vec<Task>, viewer: &Viewer) -> Vec<Task> {
    tasks.into_iter().filter(|t| t.is_owned_by(viewer)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields_of(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn parses_a_typical_record() {
        let f = fields_of(json!({
            "Title": "Migrate PG bucket to Aliyun OSS",
            "Status": ["In Progress"],
            "Owner": [{"id": "ou_abc", "name": "Kai Xuan"}],
            "Priority": "P1",
        }));
        let t = Task::from_record("rec1".into(), &f);
        assert_eq!(t.title, "Migrate PG bucket to Aliyun OSS");
        assert_eq!(t.status, "In Progress");
        assert_eq!(t.owners.len(), 1);
        assert_eq!(t.owners[0].name, "Kai Xuan");
        assert_eq!(t.priority.as_deref(), Some("P1"));
    }

    #[test]
    fn survives_a_record_with_nothing_in_it() {
        let t = Task::from_record("rec2".into(), &fields_of(json!({})));
        assert_eq!(t.title, "(untitled)");
        assert_eq!(t.status, "Backlog");
        assert!(t.owners.is_empty());
    }

    #[test]
    fn reads_a_completed_date_when_the_base_has_one() {
        let f = fields_of(json!({"Status": "Done", "Completed Date": 1_760_000_000_000i64}));
        assert_eq!(
            Task::from_record("r".into(), &f).completed_date,
            Some(1_760_000_000_000)
        );
    }

    #[test]
    fn a_task_completed_by_hand_in_lark_has_no_completion_date() {
        // Expected, not a bug: nothing may invent a date the Base does not hold.
        let f = fields_of(json!({"Status": "Done"}));
        assert_eq!(Task::from_record("r".into(), &f).completed_date, None);
    }

    #[test]
    fn handles_cjk_titles() {
        let f = fields_of(json!({"Title": "优化搭建新站点自动化", "Status": "In Progress"}));
        assert_eq!(Task::from_record("r".into(), &f).title, "优化搭建新站点自动化");
    }

    #[test]
    fn matches_owner_by_id_only() {
        let f = fields_of(json!({"Owner": [{"id": "ou_me", "name": "Adrian Chong"}]}));
        let t = Task::from_record("r".into(), &f);
        assert!(t.is_owned_by(&Viewer::new("ou_me", "anything")));
        assert!(!t.is_owned_by(&Viewer::new("ou_other", "Wei Siong")));
    }

    #[test]
    fn a_shared_display_name_does_not_leak_tasks() {
        // Two people can legitimately share a display name; only the id decides.
        let f = fields_of(json!({"Owner": [{"id": "ou_them", "name": "Adrian Chong"}]}));
        let t = Task::from_record("r".into(), &f);
        assert!(
            !t.is_owned_by(&Viewer::new("ou_me", "Adrian Chong")),
            "same name, different person — must not match"
        );
    }

    #[test]
    fn empty_identity_never_matches_everything() {
        let f = fields_of(json!({"Owner": [{"id": "", "name": ""}]}));
        let t = Task::from_record("r".into(), &f);
        assert!(!t.is_owned_by(&Viewer::new("", "")), "blank identity must match nothing");
    }

    #[test]
    fn multi_owner_task_belongs_to_each_owner() {
        let f = fields_of(json!({
            "Owner": [{"id": "ou_a", "name": "Bo Wei"}, {"id": "ou_b", "name": "Kai Xuan"}]
        }));
        let t = Task::from_record("r".into(), &f);
        assert!(t.is_owned_by(&Viewer::new("ou_a", "")));
        assert!(t.is_owned_by(&Viewer::new("ou_b", "")));
    }

    #[test]
    fn only_mine_excludes_unassigned_and_others() {
        let mine = Task::from_record("1".into(), &fields_of(json!({
            "Title": "mine", "Owner": [{"id": "ou_me", "name": "Me"}]
        })));
        let theirs = Task::from_record("2".into(), &fields_of(json!({
            "Title": "theirs", "Owner": [{"id": "ou_you", "name": "You"}]
        })));
        let orphan = Task::from_record("3".into(), &fields_of(json!({"Title": "orphan"})));

        let got = only_mine(vec![mine, theirs, orphan], &Viewer::new("ou_me", "Me"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "mine");
    }

    #[test]
    fn stale_in_progress_needs_attention() {
        let old = now().timestamp_millis() - 20 * 86_400_000;
        let f = fields_of(json!({"Status": "In Progress", "Created": old}));
        assert!(Task::from_record("r".into(), &f).needs_attention(now()));
    }

    #[test]
    fn fresh_work_is_left_alone() {
        let recent = now().timestamp_millis() - 3 * 86_400_000;
        let f = fields_of(json!({"Status": "In Progress", "Created": recent}));
        assert!(!Task::from_record("r".into(), &f).needs_attention(now()));
    }

    #[test]
    fn old_backlog_is_not_flagged() {
        let old = now().timestamp_millis() - 200 * 86_400_000;
        let f = fields_of(json!({"Status": "Backlog", "Created": old}));
        assert!(
            !Task::from_record("r".into(), &f).needs_attention(now()),
            "backlog is unscheduled, not rotting"
        );
    }

    #[test]
    fn movement_resets_the_staleness_clock() {
        // Created long ago, but touched yesterday — someone is on it.
        let f = fields_of(json!({
            "Status": "In Progress",
            "Created": now().timestamp_millis() - 200 * 86_400_000i64,
            "Modified": now().timestamp_millis() - 86_400_000i64,
        }));
        assert!(
            !Task::from_record("r".into(), &f).needs_attention(now()),
            "a recently touched task is moving, regardless of age"
        );
    }

    #[test]
    fn untouched_since_creation_still_counts_as_stale() {
        let old = now().timestamp_millis() - 90 * 86_400_000i64;
        let f = fields_of(json!({"Status": "In Progress", "Created": old}));
        assert!(Task::from_record("r".into(), &f).needs_attention(now()));
    }

    #[test]
    fn this_week_counts_as_active() {
        let f = fields_of(json!({"Status": "This Week"}));
        assert!(Task::from_record("r".into(), &f).is_active());
    }

    #[test]
    fn done_is_not_active() {
        let f = fields_of(json!({"Status": "Done"}));
        assert!(!Task::from_record("r".into(), &f).is_active());
    }

    #[test]
    fn priority_ranks_use_the_real_base_labels() {
        assert_eq!(priority::rank(Some("P0 - Critical")), 0);
        assert_eq!(priority::rank(Some("P1 - Important")), 1);
        assert_eq!(priority::rank(Some("P2 - Normal")), 2);
        assert_eq!(priority::rank(None), 3, "unset priority sorts last");
        assert_eq!(priority::rank(Some("")), 3);
    }

    #[test]
    fn accepts_dates_as_string_or_number() {
        let a = fields_of(json!({"Created": 1_700_000_000_000i64}));
        let b = fields_of(json!({"Created": "1700000000000"}));
        assert_eq!(
            Task::from_record("a".into(), &a).created,
            Task::from_record("b".into(), &b).created
        );
    }
}
