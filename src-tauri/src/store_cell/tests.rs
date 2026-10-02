//! Locking and background-write rules for `store_cell`.
//!
//! Split out of `mod.rs` to keep both files readable; the reconciliation
//! rules they exercise live in `sync`.

use super::*;
use crate::fake_repo::{me, task_named, FakeRepo};
use crate::sync::FetchMode;
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
        Arc::new(Unannounced),
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
    poll(&cell, &me(), 1_000).await.unwrap();
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
    poll(&cell, &me(), 1_000).await.unwrap();
    *repo_of(&cell).await.list_delay.lock().unwrap() = Duration::from_millis(500);

    let polling = {
        let cell = cell.clone();
        tokio::spawn(async move { poll(&cell, &me(), 2_000).await })
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
    poll(&cell, &me(), 1_000).await.unwrap();
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
                Arc::new(Unannounced),
                "rec_a".to_string(),
                status_patch("In Progress"),
                seq,
            )
            .await
        })
    };

    // A poll completes while the PUT is still in flight, so it reports the
    // pre-write row.
    poll(&cell, &me(), 2_100).await.unwrap();
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
    poll(&cell, &me(), 1_000).await.unwrap();
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
    poll(&cell, &me(), 1_000).await.unwrap();

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
    poll(&cell, &me(), 1_000).await.unwrap();

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
    poll(&cell, &me(), 1_000).await.unwrap();
    *repo_of(&cell).await.update_delay.lock().unwrap() = Duration::from_millis(120);
    let gate = Arc::new(Mutex::new(()));

    let first = begin_write(&cell, &me(), "rec_a", status_patch("In Progress"), 2_000)
        .await
        .unwrap();
    let a = tokio::spawn(finish_write(
        cell.clone(),
        gate.clone(),
        first.repo,
        Arc::new(Unannounced),
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
        Arc::new(Unannounced),
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
    poll(&cell, &me(), 1_000).await.unwrap();
    *repo_of(&cell).await.fail_next.lock().unwrap() = true;

    assert!(poll(&cell, &me(), 2_000).await.is_err());

    let snap = snapshot(&cell, &me()).await.unwrap();
    assert_eq!(snap.tasks.len(), 1, "keep showing what we had");
    assert!(snap.stale);
    assert_eq!(fetched_at_millis(&cell).await, 1_000);
}

#[tokio::test]
async fn every_operation_refuses_to_invent_data_before_sign_in() {
    let cell: Arc<StoreCell<FakeRepo>> = Arc::new(RwLock::new(None));

    assert!(matches!(poll(&cell, &me(), 1_000).await, Err(CoreError::Unauthorized)));
    assert!(matches!(
        delete(&cell, &me(), "rec_a").await,
        Err(CoreError::Unauthorized)
    ));
    assert!(matches!(
        create(&cell, &me(), TaskPatch { title: Some("x".into()), ..Default::default() })
            .await,
        Err(CoreError::Unauthorized)
    ));
    assert!(matches!(snapshot(&cell, &me()).await, Err(CoreError::Unauthorized)));
    assert!(matches!(
        begin_write(&cell, &me(), "rec_a", status_patch("Done"), 1_000).await,
        Err(CoreError::Unauthorized)
    ));
    assert_eq!(fetched_at_millis(&cell).await, 0);
}

// ---- R6-1: fetch only the viewer's rows, and prove the filter first ----

#[tokio::test]
async fn a_filter_that_agrees_with_a_full_walk_is_adopted() {
    let cell = cell_with(vec![task_named("a", "ou_me"), task_named("b", "ou_other")]);

    let mode = calibrate_fetch_mode(&cell, &me(), 1_000).await;

    assert_eq!(mode, FetchMode::Filtered);
    let repo = repo_of(&cell).await;
    assert_eq!((repo.walks(), repo.searches()), (1, 1), "one of each, at sign-in only");

    poll(&cell, &me(), 4_000).await.unwrap();
    assert_eq!(repo.walks(), 1, "a refresh must no longer walk the whole table");
    assert_eq!(repo.searches(), 2, "it asks the server for the viewer's rows");
    assert_eq!(snapshot(&cell, &me()).await.unwrap().tasks.len(), 1);
}

#[tokio::test]
async fn a_filter_that_misses_a_co_owned_row_puts_the_session_back_on_full_walks() {
    // UNVERIFIED against the live Base: `Owner` is multiple:true and it is
    // not known whether Lark's `is` operator matches a row the viewer only
    // co-owns. This is the mechanism that makes it survivable either way.
    let mut shared = task_named("shared", "ou_me");
    shared.owners.push(crate::task::Person { id: "ou_other".into(), name: "Them".into() });
    let cell = cell_with(vec![task_named("a", "ou_me"), shared]);
    repo_of(&cell).await.hide_from_filter("rec_shared");

    let mode = calibrate_fetch_mode(&cell, &me(), 1_000).await;

    assert_eq!(mode, FetchMode::FullWalk, "a partial loss must not be accepted");
    let repo = repo_of(&cell).await;
    poll(&cell, &me(), 4_000).await.unwrap();
    assert_eq!(repo.walks(), 2, "the session stays on the path that is correct");

    let titles: Vec<String> =
        snapshot(&cell, &me()).await.unwrap().tasks.iter().map(|t| t.title.clone()).collect();
    assert!(titles.contains(&"shared".to_string()), "the co-owned row must still show");
    assert_eq!(titles.len(), 2);
}

#[tokio::test]
async fn a_filter_that_cannot_be_reached_at_sign_in_is_not_trusted() {
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    *repo_of(&cell).await.fail_next.lock().unwrap() = true;

    assert_eq!(calibrate_fetch_mode(&cell, &me(), 1_000).await, FetchMode::FullWalk);
}

#[tokio::test]
async fn an_empty_filtered_poll_is_checked_against_a_full_walk_before_it_is_believed() {
    // "NOTHING ASSIGNED / that's a real state, not a bug" is the worst
    // possible copy for a filter that has quietly stopped matching.
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    calibrate_fetch_mode(&cell, &me(), 1_000).await;
    let repo = repo_of(&cell).await;
    repo.hide_from_filter("rec_a");

    poll(&cell, &me(), 4_000).await.unwrap();

    assert_eq!(repo.walks(), 2, "an empty answer must be confirmed by a real walk");
    assert_eq!(
        snapshot(&cell, &me()).await.unwrap().tasks.len(),
        1,
        "the user must not be told they have nothing assigned"
    );
}

#[tokio::test]
async fn a_genuinely_empty_account_still_reads_as_empty() {
    let cell = cell_with(vec![task_named("theirs", "ou_other")]);
    calibrate_fetch_mode(&cell, &me(), 1_000).await;

    poll(&cell, &me(), 4_000).await.unwrap();

    assert!(snapshot(&cell, &me()).await.unwrap().tasks.is_empty());
}

#[tokio::test]
async fn only_mine_still_guards_the_snapshot_whichever_path_fetched_it() {
    // Defence in depth: the server filter is an optimisation, not the gate.
    let cell = cell_with(vec![task_named("a", "ou_me"), task_named("b", "ou_other")]);
    calibrate_fetch_mode(&cell, &me(), 1_000).await;
    poll(&cell, &me(), 4_000).await.unwrap();

    let snap = snapshot(&cell, &me()).await.unwrap();
    assert!(snap.tasks.iter().all(|t| t.is_owned_by(&me())));
}

// ---- R6-2: one walk at a time, and never an older one over a newer ----

#[tokio::test]
async fn a_refresh_arriving_while_one_is_running_does_not_start_a_second_walk() {
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    poll(&cell, &me(), 1_000).await.unwrap();
    *repo_of(&cell).await.list_delay.lock().unwrap() = Duration::from_millis(300);

    let first = {
        let cell = cell.clone();
        tokio::spawn(async move { poll(&cell, &me(), 2_000).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;

    // Alt-tabbing back in, and mashing the refresh button.
    for at in [2_100, 2_200, 2_300] {
        poll(&cell, &me(), at).await.unwrap();
    }
    first.await.unwrap().unwrap();

    assert_eq!(repo_of(&cell).await.walks(), 2, "four triggers, two walks");
}

#[tokio::test]
async fn a_walk_that_left_before_a_write_settled_cannot_revert_it() {
    // The visible revert: the overlay has already retired by the time the
    // pre-write rows land, so nothing is left to protect the change.
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    poll(&cell, &me(), 1_000).await.unwrap();
    *repo_of(&cell).await.list_delay.lock().unwrap() = Duration::from_millis(300);

    let walking = {
        let cell = cell.clone();
        tokio::spawn(async move { poll(&cell, &me(), 2_000).await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;

    write_and_settle(&cell, "rec_a", status_patch("In Progress"), 2_100).await.unwrap();
    walking.await.unwrap().unwrap();

    let snap = snapshot(&cell, &me()).await.unwrap();
    assert_eq!(
        snap.tasks[0].status, "In Progress",
        "a walk that predates the write must not put the old status back"
    );
    assert_eq!(snap.fetched_at_millis, 1_000, "and must not claim to be fresher");
}

#[tokio::test]
async fn a_completion_date_reaches_the_base_alongside_the_status() {
    // The day itself is decided by `repo::stamp_completion`; this is the
    // plumbing between the store and the request.
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    poll(&cell, &me(), 1_000).await.unwrap();
    let done = TaskPatch {
        status: Some("Done".into()),
        completed_date: Some(1_760_000_000_000),
        ..Default::default()
    };

    write_and_settle(&cell, "rec_a", done, 2_000).await.unwrap();

    let sent = repo_of(&cell).await.sent_patches();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].completed_date, Some(1_760_000_000_000));
    assert_eq!(sent[0].status.as_deref(), Some("Done"));
    assert!(sent[0].title.is_none(), "only the changed fields travel");
    assert!(sent[0].remarks.is_none());
}

// ---- R6-3: the UI is told the moment a write settles ----

#[derive(Default)]
struct Announcements(std::sync::Mutex<Vec<String>>);

impl SettleNotifier for Announcements {
    fn write_settled(&self, record_id: &str) {
        self.0.lock().unwrap().push(record_id.to_string());
    }
}

#[tokio::test]
async fn a_settled_write_is_announced_so_the_row_can_stop_looking_busy() {
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    poll(&cell, &me(), 1_000).await.unwrap();
    let heard = Arc::new(Announcements::default());

    let started =
        begin_write(&cell, &me(), "rec_a", status_patch("In Progress"), 2_000).await.unwrap();
    finish_write(
        cell.clone(),
        Arc::new(Mutex::new(())),
        started.repo,
        heard.clone(),
        "rec_a".into(),
        status_patch("In Progress"),
        started.seq,
    )
    .await;

    assert_eq!(*heard.0.lock().unwrap(), vec!["rec_a".to_string()]);
}

#[tokio::test]
async fn a_rejected_write_is_announced_too() {
    // Otherwise the row that failed is the one left dimmed forever.
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    poll(&cell, &me(), 1_000).await.unwrap();
    let heard = Arc::new(Announcements::default());
    let started =
        begin_write(&cell, &me(), "rec_a", status_patch("Done"), 2_000).await.unwrap();
    *repo_of(&cell).await.fail_next.lock().unwrap() = true;

    finish_write(
        cell.clone(),
        Arc::new(Mutex::new(())),
        started.repo,
        heard.clone(),
        "rec_a".into(),
        status_patch("Done"),
        started.seq,
    )
    .await;

    assert_eq!(*heard.0.lock().unwrap(), vec!["rec_a".to_string()]);
}

// ---- R6-4: create and delete out of the lock, and owned by the viewer ----

#[tokio::test]
async fn a_click_does_not_queue_behind_a_create() {
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    poll(&cell, &me(), 1_000).await.unwrap();
    *repo_of(&cell).await.update_delay.lock().unwrap() = Duration::from_millis(400);

    let creating = {
        let cell = cell.clone();
        tokio::spawn(async move {
            create(&cell, &me(), TaskPatch { title: Some("new".into()), ..Default::default() })
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = Instant::now();
    begin_write(&cell, &me(), "rec_a", status_patch("In Progress"), 2_000).await.unwrap();
    let blocked_for = started.elapsed();

    creating.await.unwrap().unwrap();
    assert!(
        blocked_for < Duration::from_millis(200),
        "the click waited {blocked_for:?} for the POST to finish"
    );
}

#[tokio::test]
async fn a_click_does_not_queue_behind_a_delete() {
    let cell = cell_with(vec![task_named("a", "ou_me"), task_named("b", "ou_me")]);
    poll(&cell, &me(), 1_000).await.unwrap();
    *repo_of(&cell).await.update_delay.lock().unwrap() = Duration::from_millis(400);

    let deleting = {
        let cell = cell.clone();
        tokio::spawn(async move { delete(&cell, &me(), "rec_b").await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = Instant::now();
    begin_write(&cell, &me(), "rec_a", status_patch("In Progress"), 2_000).await.unwrap();
    let blocked_for = started.elapsed();

    deleting.await.unwrap().unwrap();
    assert!(blocked_for < Duration::from_millis(200), "the click waited {blocked_for:?}");
}

#[tokio::test]
async fn a_created_task_appears_in_the_snapshot_it_returns() {
    let cell = cell_with(vec![]);
    poll(&cell, &me(), 1_000).await.unwrap();

    let snap =
        create(&cell, &me(), TaskPatch { title: Some("new".into()), ..Default::default() })
            .await
            .unwrap();

    assert_eq!(snap.tasks.len(), 1);
    assert_eq!(snap.tasks[0].title, "new");
}

#[tokio::test]
async fn a_deleted_task_is_gone_from_the_snapshot_it_returns() {
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    poll(&cell, &me(), 1_000).await.unwrap();

    let snap = delete(&cell, &me(), "rec_a").await.unwrap();

    assert!(snap.tasks.is_empty());
}

#[tokio::test]
async fn a_refused_delete_leaves_the_row_where_it_was() {
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    poll(&cell, &me(), 1_000).await.unwrap();
    *repo_of(&cell).await.fail_next.lock().unwrap() = true;

    assert!(delete(&cell, &me(), "rec_a").await.is_err());

    assert_eq!(
        snapshot(&cell, &me()).await.unwrap().tasks.len(),
        1,
        "a row must not vanish optimistically and stay vanished"
    );
}

#[tokio::test]
async fn another_persons_row_cannot_be_edited_or_deleted() {
    // The UI cannot offer one, but the rule belongs in Rust: a stale or
    // mistaken record id must not be able to destroy someone else's work.
    let cell = cell_with(vec![task_named("mine", "ou_me"), task_named("theirs", "ou_them")]);
    poll(&cell, &me(), 1_000).await.unwrap();

    let refused = begin_write(&cell, &me(), "rec_theirs", status_patch("Done"), 2_000).await;
    assert!(matches!(refused, Err(CoreError::Forbidden)), "update must be refused");
    assert!(
        matches!(delete(&cell, &me(), "rec_theirs").await, Err(CoreError::Forbidden)),
        "delete must be refused"
    );

    let repo = repo_of(&cell).await;
    assert!(repo.sent_patches().is_empty(), "nothing may have been sent");
    assert_eq!(
        repo.tasks.lock().unwrap().len(),
        2,
        "the other person's record must still exist"
    );
}

#[tokio::test]
async fn a_record_id_the_store_has_never_seen_is_refused_before_the_network() {
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    poll(&cell, &me(), 1_000).await.unwrap();

    let refused = begin_write(&cell, &me(), "rec_made_up", status_patch("Done"), 2_000).await;
    assert!(matches!(refused, Err(CoreError::Invalid(_))), "an unknown id must be refused");
    assert!(delete(&cell, &me(), "rec_made_up").await.is_err());
    assert!(repo_of(&cell).await.sent_patches().is_empty());
}

#[tokio::test]
async fn signing_out_mid_write_is_not_a_panic() {
    let cell = cell_with(vec![task_named("a", "ou_me")]);
    poll(&cell, &me(), 1_000).await.unwrap();
    let started =
        begin_write(&cell, &me(), "rec_a", status_patch("Done"), 2_000).await.unwrap();

    *cell.write().await = None;

    finish_write(
        cell.clone(),
        Arc::new(Mutex::new(())),
        started.repo,
        Arc::new(Unannounced),
        "rec_a".into(),
        status_patch("Done"),
        started.seq,
    )
    .await;
}
