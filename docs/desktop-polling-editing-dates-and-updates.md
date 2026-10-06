# Spec — OMSN Desktop: poll cost, row editing, completion dates, settings & updates

Date: 2026-10-01
Source: four owner requirements (R6–R9), triaged.
Predecessor: [desktop-updates-readability-and-realtime.md](./desktop-updates-readability-and-realtime.md) (R1–R5)
Status: **R6 ready to build (D7, D8 shape it, not block it). R7 blocked on D9/D11.
R8 blocked on D12 — and is mostly a Lark Base edit, not app work. R9 blocked on
D13 and on a release pipeline that does not exist.**

Standing constraint: keep it minimal. 14 internal SRE engineers on macOS. The
Base schema does not change without the owner saying so; the OMSN Claude plugin
writes to the same table.

---

## Restatement and class

| # | Restated in one line | Class |
|---|---|---|
| R6 | A status change feels slow, and the app re-reads the whole 315-row table on every refresh. | **BUG** (two distinct defects + one cost problem) |
| R7 | A row needs a delete (confirmed) and a way to fix a typo in its title. | **NEEDS A DECISION** (scope of "editable") |
| R8 | A completed task should record the day, not just the month. | **BUG in the Base, not the app** — and the field is not a completion date at all |
| R9 | A Penguin-style Settings overlay that checks GitHub and, if behind, downloads and restarts. | **CONFLICTS** with the earlier "manual check only" decision, and **HIDDEN COST** (no release exists to check against) |

---

## Findings

### R6 — one of the three supplied facts does not match the code

Accepted as measured, unchanged:

- `POLL_INTERVAL_MS = 20_000` (focused only), `SETTLE_POLL_MS = 400`,
  `SETTLE_WINDOW_MS = 12_000` — all confirmed in `src/App.tsx`.
- A full-table walk is **315 records / 2 paginated GETs / 2586 ms**.
- `records/search` with an explicit `open_id` Owner filter returns
  **16 records in 558 ms**. `"CurrentUser"` is rejected (1254018 InvalidFilter).

**Corrected:** the claim that "the 400 ms settle chase fires roughly 6
overlapping 2.6-second full-table walks per write" is not what the code does.
The settle loop calls `load(false)`:

```ts
void load(false);                       // App.tsx, the settling effect
// → invoke('list_my_tasks', { refresh: false })
// → commands.rs: `if refresh { poll }` is skipped entirely
// → store_cell::snapshot — a pure in-memory read, no network
```

So the settle chase performs **zero** network requests. There is no queueing
of full-table walks behind a write. The felt delay has three other causes, all
verified in the source.

#### Cause 1 — overlapping polls, and the store accepting an older result as newer

`store_cell::poll` has **no in-flight guard**, and `App.tsx` fires a network
refresh from three unthrottled triggers: the 20 s interval, a
`window.addEventListener('focus', tick)`, and the `↻` button. For an
always-on-top widget, alt-tabbing in and out starts a fresh 2.6 s walk each
time, and mashing `↻` starts one per click.

`Store::apply_poll` then does `self.polled = tasks` wholesale, with no regard
for when that fetch *began*:

```rust
pub fn apply_poll(&mut self, tasks: Vec<Task>, now_millis: i64) {
    self.polled = tasks;                 // sync.rs — unconditional
    self.fetched_at_millis = now_millis;
```

Two consequences, both reachable by ordinary use:

- **Poll A starts, poll B starts, B returns first, A returns second.** A's
  older data overwrites B's newer data *and* is stamped with the later
  `fetched_at_millis`. The store now holds stale rows labelled fresh. In the UI
  this also releases the R5 pin (`fetched_at_millis !== pin.fetchedAt`), so the
  row jumps **and** shows its pre-write status.
- **A poll started before a write settles, returning after it.**
  `settle_update` folds the server's updated record into `polled` at t≈0.8 s and
  removes the overlay; the in-flight walk returns at t≈2.6 s and replaces the
  whole vector with pre-write data. The overlay is already gone, so nothing
  protects it. The UI shows the revert as soon as anything calls `load(false)` —
  which happens the moment the user touches a *second* row.

This is the same class of defect R5 fixed at the lock level, surviving at the
data level. The 2.6 s walk is what makes the window wide enough to hit; a 558 ms
filtered poll shrinks it 4.6×, which is a second reason to do the filter.

#### Cause 2 — the pending dim is the delay the user is reporting

`.task--pending { opacity: 0.55 }`. A status write is instant in text but the
row sits at 55 % opacity for `PUT duration + up to 400 ms` of settle
granularity. Against an API whose GETs measure 1.3 s each, that is plausibly
0.6–1.5 s of visibly "working" UI per click. The app is *telling* the user
there is a delay. That is honest, and it is also exactly what they are
complaining about.

#### Cause 3 — `create_task` and `delete_task` still hold the store lock across the network

`store_cell` exists to enforce "no I/O while the store lock is held". Two
commands bypass it:

```rust
// commands.rs, create_task AND delete_task
let mut guard = state.store.write().await;
let store = guard.as_mut()...;
store.create(patch).await?;        // network, under the exclusive lock
```

`tokio::sync::RwLock` is FIFO-fair, so adding a task and then immediately
clicking a status marker reproduces the original R5 queueing. **R7 adds a
delete button, which makes this a user-reachable path rather than a latent
one.** It must be fixed before R7 ships.

#### Also found: neither write command checks ownership

`update_task` and `delete_task` accept any `record_id` from the webview and
never ask whether the viewer owns that row. `list_my_tasks` means the UI cannot
*offer* a foreign row, but the project's own rule is "enforced in Rust at the
command boundary, never in the UI". With a delete button shipping, a stale or
mistaken `record_id` could destroy someone else's record. Cheap to close.

#### On the "fails silently to empty" caution

Narrower than it sounds, and now measurable:

- A **malformed** filter is loud: Lark returns 1254018, `from_lark_code` maps
  an unknown code to `CoreError::Api`, and the user sees an error. Verified in
  `error.rs` — nothing swallows it.
- The genuinely silent case is a **well-formed filter carrying an identifier in
  the wrong namespace** (e.g. `union_id` where `open_id` is expected). That
  returns 0 rows, and `App.tsx` renders `NOTHING ASSIGNED / that's a real
  state, not a bug` — the worst possible copy for a broken filter.
- A third, unmeasured risk: **multi-owner rows.** `Owner` is
  `"type": "user", "multiple": true` (confirmed in the live schema). If the
  server's `is` operator does not match a record where the viewer is one of
  several owners, the user silently loses rows they own. A zero-row check does
  not catch a *partial* loss.

Both are closed by one cheap mechanism — see R6-1 below.

Scope is already sufficient: the app requests `bitable:app` (`oauth.rs:28`),
which covers `records/search`. **No re-consent for 14 people.**

### R7 — the backend is done; only the UI is missing

`delete_task` and `update_task` are both registered in `lib.rs` and
`TaskPatch` already carries `title`, `status`, `priority`, `remarks` and
`owner_ids`, all validated in `TaskPatch::validate` before anything reaches the
network. Title is checked non-blank; priority and status are checked against
the Base's real option lists. So "edit the title" and "delete" are UI-only
work plus the two Rust fixes above.

Placement constraint: the row already has three distinct hit areas — status
marker (left), body (click to expand), and an absolutely-positioned hover strip
(top-right, 2 buttons). The window minimum is 360 px wide. Adding edit and
delete to the hover strip makes four small buttons there and puts an
irreversible delete one mis-click from `✓` (Done). That is the wrong place.

### R8 — verified against the live Base: there is no completion date to add a day to

`Completed Month` (`fldmTlLxx0`) is a **formula**, not a date field:

```
IF([Status]="Done", TEXT([Modified],"YYYY-MM"), "")
```

Verified facts:

- It has **no** `style` / `property` / `date_formatter` at all. The `"YYYY-MM"`
  is a literal argument to `TEXT()`. Its output is a plain **string**
  (`"2026-09"`), and `""` — not `null` — for non-Done rows.
- **There is no completion-date field of any kind** in the table. No
  "Completed Date", "Done Date", "Closed At". All 19 fields were enumerated.
- The three real date fields (`Due Date`, `Created`, `Modified`) are all
  already `yyyy/MM/dd`. **Nothing in the Base is formatted month-and-year
  except this formula.**
- Neither writer touches it. `grep -r "Completed Month"` across
  `desktop/` and `plugins/omsn/` returns nothing; `TaskPatch::to_fields` writes
  only Title/Status/Priority/Remarks/Owner, and `update-item.sh` writes only
  Status/Priority/Category/Remarks/Due Date/Owner.
- The desktop app renders **no absolute date anywhere**: `formatClock` is
  time-only, `relativeTime` is relative, the age chip is `daysSince`. So the
  month-and-year the owner is looking at is the **Lark Base grid**, not the app.

So R8 is a Lark Base formula question, and the one-character answer
(`"YYYY-MM-DD"`) is a trap:

- **The field is not a completion date.** It is "the month this row was last
  touched, if it currently says Done". Record NO.011 was created 2026-04-23 and
  reads `2026-08` only because somebody edited it on 2026-08-20
  (`Cycle Days: 119`). Any later edit re-stamps it.
- **Bulk edits corrupt it in batches.** `Modified` timestamps cluster — six
  records at `2026-09-14T16:13`, four at `2026-07-28T10:51`. Those four all
  read `2026-07` regardless of when the work actually finished.
- A month is plausibly read as "roughly when it finished". A specific day reads
  as a fact. **Adding precision to a lossy signal makes it more wrong, not less.**
- `Completed Month` is a grouping key by name. Switching it to day granularity
  fragments a monthly roll-up into ~30 buckets for anyone who groups by it.

Two facts that support the owner's instinct, and one that constrains the fix:

- **Day precision is already the house convention.** Every other date surface
  uses it: all three real Base fields are `yyyy/MM/dd`, the plugin validates
  `Due Date` as `^[0-9]{4}-[0-9]{2}-[0-9]{2}$`, and the reporting skills format
  days (`platform-weekly-report` uses `%b %-d` and `%Y-%m-%d`; `ping-omsn` uses
  `%a %-d %b`). `Completed Month` is the only month-granularity thing anywhere.
  The owner is right that it is the odd one out.
- **A formula string cannot be consumed by the app even if it wanted to.**
  `epoch_millis` in `task.rs` accepts an integer or RFC3339 only, so a
  `"2026-09"` or `"2026-09-14"` formula output parses to `None` and is silently
  dropped. If a completion date is ever to reach the app, it must be a real
  `datetime` field, not a formula.
- **Neither writer can write a date today.** `TaskPatch::to_fields` emits five
  fields; `update-item.sh` emits six. Whichever option is chosen, both need the
  new field plumbed through — `desktop/src-tauri/src/task.rs` (`fields`
  module), `desktop/src-tauri/src/repo.rs` (`TaskPatch` + `to_fields`), and
  `plugins/omsn/skills/update/scripts/update-item.sh`.

Two unrelated defects found in the same schema pass, **not in scope**, raised
because nobody is watching them:

- The `Status` select contains an option named **`Nonsense`**. That is the
  literal value from the desktop app's own test
  (`a_made_up_status_is_refused_before_it_reaches_lark`). It reached the live
  Base at some point and Lark auto-created the option — the exact pollution
  `TaskPatch::validate` exists to prevent. Someone should delete the option in
  Lark; one minute of work.
- `Quarter` is `IF(AND([Status]="Done",[Created]<DATE(2026,7,1)),"Q2","Q3")`.
  It is hardcoded to the 2026 Q2/Q3 boundary and keys off `Created`, not
  completion. As of today (2026-10-01) everything is mislabelled `Q3` forever.
- **Both READMEs' schema tables are stale.** They document nine fields against
  nineteen live ones: missing `ID`, `Scope`, `Workstream`, `Completed Month`,
  `Cycle Days`, `Quarter`, `Done %` and the three Parent/link/lookup fields;
  they misname `Created`/`Modified` as `Created Time`/`Modified Time`; and
  `README.zh.md` still lists a removed `派发人` and `团队`. `Category` has 14
  live options, not the 8 documented. Separately,
  `plugins/omsn/skills/update/SKILL.md:83` documents a `To Do` status that the
  script it documents does not accept (it takes `Backlog`). A doc-sync pass is
  worth one line item but is not part of R6–R9.

### R9 — two conflicts and a missing prerequisite

**Conflict 1 — this reverses an explicit decision.** In the predecessor spec,
"download and install anything" was put out of scope and the full updater was
deferred pending D1–D3, on the grounds that the Minisign private key is a
permanent, unrecoverable secret. "Download and restart" *is* the full updater.
Surfacing it, not silently honouring it.

**Conflict 2 — nobody mentioned this one, and it is sharper.** The repo is
private (confirmed: `"isPrivate": true`). Tauri's updater fetches a plain URL.
It does support custom request headers, so a GitHub PAT would work — but that
means **shipping a real credential in the binary**, which directly reverses
commit `36455d7` "zero-config distribution — PKCE public client, no shipped
secret". The app_id and base_token are addresses; a PAT is not. Either the
artifacts go somewhere publicly readable, or the no-shipped-secret property is
given up.

**The prerequisite that blocks all of R9.** Verified, not assumed:

- `gh release list --repo sre-snsoft/task-management` → **empty. No releases.**
- `.github/workflows` → **does not exist. No CI.**
- The only tag in the repo is `v0.3.1`, and it is the *plugin's*
  (`plugins/omsn/.claude-plugin/plugin.json`), not the desktop app's.
- `tauri.conf.json` has no `updater` block and no `createUpdaterArtifacts`.
- There is no `.dmg`.

So "up to date if it matches the latest on GitHub" currently compares against
nothing. **A Settings panel whose only button always fails is worse than no
Settings panel**, and the version it would display is already visible in the
pager footer (shipped with R4). R9's user-visible value is zero until a release
pipeline exists.

One capability fact, verified from `src-tauri/gen/schemas/acl-manifests.json`
rather than assumed: `core:app:default` grants `allow-version`,
`allow-register-listener` and `allow-remove-listener` — so reading the version
and listening to Tauri events need **no capability change** — but there is **no
`core:app:allow-restart`** anywhere in the core ACL (only `allow-exit`).
Restarting the app requires `tauri-plugin-process` or a Rust-side relaunch:
another dependency, not a config line.

---

## R6 — A status change must feel immediate, and a refresh must not cost 2.6 s

### R6-1 — Fetch only the viewer's rows (server-side Owner filter)

**Requirement.** A refresh reads the viewer's rows from the Base, not the whole
table.

Design:

- Add `list_owned_by(&self, open_id: &str)` to `TaskRepository` **alongside**
  `list_all`. Implemented with `POST .../records/search`, body:
  ```json
  {"filter":{"conjunction":"and","conditions":[
     {"field_name":"Owner","operator":"is","value":["<open_id>"]}]}}
  ```
  with `user_id_type=open_id` on the query string so person cells come back in
  the same namespace the filter was built from.
- `list_all` **stays on the trait.** It is what the zero-row fallback and the
  sign-in cross-check call, and it is what a future manager team-view calls.
  The filter is a *parameter of the fetch*, never a hardcoded constant, so the
  team view costs nothing today and loses nothing later.
- `only_mine` in `task.rs` **stays exactly as it is.** It is the stated single
  access gate; the server filter is an optimisation, not a replacement.
- **The safety net (this is what makes the caution survivable).** On each
  sign-in, run one `list_all` and compare `only_mine(full)` against
  `list_owned_by(open_id)` by record id. They must match exactly. If they do
  not — wrong namespace, or a multi-owner row the server filter dropped — the
  session falls back to `list_all` + `only_mine` for its whole lifetime and the
  mismatch is logged to stderr. The user sees nothing except a slower app.
  Cost: one 2.6 s walk at startup, which already happens today.
- A filtered poll returning **zero** rows is never accepted on its own: retry
  once via `list_all` + `only_mine` before believing it.

Acceptance criteria:

- A focused refresh issues **one** POST, not two GETs, and completes in under
  1 s against the live Base. A tester records the wall time.
- A signed-in user sees exactly the same set of record ids as before the change.
  Compare lists before and after on a real account.
- A multi-owner task the viewer co-owns still appears. **Construct one
  deliberately** — this is the untested risk; do not sign off on it by
  inspection.
- Break it on purpose: hardcode a `union_id` as the filter value. Expected: the
  cross-check fails, the app serves the full-walk path, the list is still
  correct, and the user is **never** shown `NOTHING ASSIGNED`.
- Break it the other way: send a malformed filter. Expected: a readable error
  notice, not an empty list and not a crash.
- A genuinely empty account (no rows owned) still reads `NOTHING ASSIGNED`
  after the one fallback walk confirms it.
- No OAuth re-consent is triggered for any user.

Out of scope: `field_names` to trim the response payload (a further win, but
unmeasured and not needed); filtering by status server-side; any team view.

### R6-2 — The store must never accept an older fetch as newer

**Requirement.** A poll result may only be applied if it is newer than what the
store already holds.

Design (both halves; neither alone is sufficient):

- **One poll at a time.** An in-flight flag in `store_cell`: a `poll` that
  arrives while another is running returns immediately as a no-op. Also
  debounce the network refresh to a minimum interval (2 s) so focus-flapping
  and `↻`-mashing cannot stack walks.
- **Tag the fetch with its start time.** `poll` passes `started_at` as well as
  `now_millis`; `apply_poll` discards the result if `started_at` is earlier
  than the store's last mutation (the previous poll's start, or the most recent
  `settle_update`). A discarded poll leaves `polled` and `fetched_at_millis`
  untouched and is not an error.

Acceptance criteria:

- Two overlapping polls where the *first-started* returns last: the store ends
  up holding the newer data, and `fetched_at_millis` does not move backwards in
  content while moving forwards in value. Testable against `FakeRepo` with
  asymmetric `list_delay`.
- The settle-then-stale-poll sequence: begin a write, let it settle, then let a
  poll whose fetch started *before* the write complete. The row must still show
  the written status — including after a subsequent `load(false)`. This is the
  regression that currently fails.
- Alt-tab the window in and out ten times in five seconds: at most one network
  walk is in flight at any moment, and the pinned row does not jump.
- `↻` clicked five times rapidly issues at most one request per 2 s.
- The existing `a_poll_landing_mid_write_does_not_revert_the_users_change` test
  still passes, and a new test covers the *post-settle* case it does not.

### R6-3 — Close the settle feedback gap

**Requirement.** The in-flight marker clears as soon as Rust knows the outcome,
not up to 400 ms later.

**On "settle backoff instead of fixed 400 ms": that is the wrong direction.**
Backoff makes later feedback *slower*, and the call is free — no network, pure
memory read. If the polling shape is kept, it should get **shorter**, not
longer. Recommended: 150 ms, and shrink `SETTLE_WINDOW_MS` from 12 s to 5 s
(the 60 s overlay timeout owns the tail; a 12 s UI chase against a sub-2 s
write is dead time).

Better: emit a Tauri event from `finish_write` when a write settles and have
`App.tsx` `load(false)` on that event, deleting the polling loop. Verified: no
capability change needed (`core:app:default` already grants
`allow-register-listener`). See **D8**.

Acceptance criteria:

- Time from write completion to the row un-dimming is under 200 ms. Measured,
  not asserted.
- Dropping the whole settle mechanism is not acceptable: a rejected write must
  still surface `write_failures` within the same window.
- If the event route is taken, a missed or duplicated event must not strand a
  row dimmed forever — keep a slow (1 s) backstop poll while `pending_ids` is
  non-empty.

### R6-4 — Move `create_task` / `delete_task` I/O out of the store lock, and check ownership

**Requirement.** No command performs network I/O while holding the store lock,
and no command mutates a record the viewer does not own.

**This is a prerequisite for R7.** Both halves are small.

Acceptance criteria:

- `create_task` and `delete_task` route through `store_cell` with the same
  take-lock / release / do-I/O / take-lock shape as `begin_write` and
  `finish_write`. The existing `a_click_does_not_queue_behind_a_full_table_poll`
  test is duplicated for create and for delete, and passes.
- Adding a task and immediately clicking a status marker repaints within one
  frame. Today it waits for the POST.
- `update_task` and `delete_task` return `Forbidden` for a `record_id` the
  viewer does not own, **before** any network call. Test with a record id
  belonging to another owner from the full `list_all` result.
- An unknown `record_id` is also refused, not sent.

Out of scope for all of R6: changing `POLL_INTERVAL_MS`. With the filter in
place, 10 s costs less than 20 s does today and is a reasonable follow-up — but
**it is not the fix for the reported symptom**, because the symptom is the
user's own action, which is already instant. Do not ship it as if it were.

---

## R7 — Delete with confirm, and fix a typo in a title

**Requirement.** A row can be renamed to fix a mistake, and can be deleted
after an explicit confirmation.

### Placement (recommended; see D10)

Both controls live in the **expanded row**, not the hover strip:

- Expansion is already a deliberate click, so an irreversible delete is two
  intentional actions away rather than adjacent to `✓`.
- The hover strip stays two buttons wide and keeps serving the frequent action
  (status) at the 360 px minimum.
- The expanded body already wraps the full title, which is where someone fixing
  a typo is looking.

Acceptance criteria — **edit**:

- Expanding a row reveals an `EDIT` affordance. Activating it turns the wrapped
  title into a text input seeded with the current title, `maxLength={200}` to
  match the add bar.
- Enter or a `SAVE` action commits; Escape or a `CANCEL` action reverts with no
  write sent. Clicking elsewhere does not silently commit.
- The write goes through `update_task` with **only** `title` in the patch, so a
  concurrent plugin edit to any other field is not clobbered. Assert on the
  sent patch, as `repeated_clicks_leave_the_ui_on_the_last_one` already does
  for status.
- A blank or whitespace-only title is refused by `TaskPatch::validate` and
  reported in the panel; the original title is not lost from the UI.
- The rename is instant (overlay) and settles like a status change. A rejected
  rename snaps back **and** surfaces `write_failures`.
- Editing does not change `Status`, so `sortTasks` keeps the row in its group;
  the R5 pin still applies because `Modified` moves.
- Keyboard reachable throughout. The row-expand `Enter`/`Space` handler must
  not fire while the input has focus.

Acceptance criteria — **delete**:

- Expanding a row reveals a `DELETE` affordance. Activating it opens a confirm
  overlay reusing the existing `.confirm` treatment (one overlay style, per the
  predecessor spec).
- The confirm names the task and says the deletion cannot be undone from the
  app. The destructive button reads **`DELETE`**, not `YES`, and is styled with
  `--danger`, not `pixel-btn--ok`. Muscle memory from `MARK AS DONE?` must not
  be able to destroy a record.
- Dismissing by clicking the backdrop or pressing Escape cancels. The default
  focus is on the cancel action.
- On confirm, the row disappears from the list and the pager does not strand
  the user on a now-empty page (`usePagination` already clamps; verify with a
  single-row last page).
- A rejected delete restores the row and surfaces the reason. It does not
  vanish optimistically and stay vanished.
- Deleting is refused in Rust for a row the viewer does not own (R6-4).

### What is editable

**Recommended: title only.** The stated need is "fix a typo", and the title is
the only field with no other repair path inside the app. See **D9**.

Explicitly out of scope: editing `Remarks` (needs a textarea, a length policy
and its own scroll behaviour — nobody asked), `Owner` (reassigning work is a
conversation, not a button), `Category`, `Workstream`, `Due Date`, and
multi-select or bulk delete.

Open questions: **D9**, **D10**, **D11**.

---

## R8 — Record *when* a task was completed

**Requirement, restated from the symptom.** A completed task should carry the
date it was completed, to the day.

The owner's hypothesis — "the field shows only month and year, add the day" —
is addressing a field that is not a completion date. The requirement is the
date; the formula is not the answer. See **D12** for the choice; the
recommendation is below.

### Recommended: add one writable `Completed Date` field, and re-point the formula at it

1. **In Lark** (owner or Base admin, minutes): add a `datetime` field
   `Completed Date`, formatter `yyyy/MM/dd` to match the other three. Change
   `Completed Month` to `IF([Status]="Done",TEXT([Completed Date],"YYYY-MM"),"")`.
   This also **fixes** the re-stamping bug: a later edit to a Done row no longer
   moves its completion month.
2. **In the desktop app** (S): `TaskPatch` gains `completed_date: Option<i64>`
   (epoch millis — a Bitable datetime write rejects a string with 1254064).
   `setStatus(task, 'Done')` sends it alongside the status. Nothing else writes
   it, and nothing clears it.
3. **In the OMSN plugin** (S, separate change): `update-item.sh` sets
   `Completed Date` when `--status "Done"` is passed. Until this ships,
   plugin-completed tasks have a blank date — which is honest, and better than
   a wrong one.
4. **Backfill: do not.** There is nothing to back-fill from except `Modified`,
   which is already demonstrably wrong for at least four records. Leave
   historical rows blank rather than inventing dates.

Acceptance criteria:

- Marking a task Done from the desktop app sets `Completed Date` to today, and
  `Completed Month` to this month, both visible in the Lark grid.
- Marking a task Done from `/omsn:update` does the same.
- Re-opening a Done task (Done → In Progress) and completing it again sets the
  new date. Decide once whether re-opening clears the field; the simple rule is
  "last completion wins, never cleared".
- Editing any other field on a Done row does **not** change `Completed Date`
  or `Completed Month`. This is the bug being fixed — test it explicitly.
- A task completed directly in the Lark UI by a human has a blank
  `Completed Date` and a blank `Completed Month`. That is expected and must not
  be papered over by a client-side guess.
- The desktop app still hides Done tasks and shows no absolute dates. **No UI
  change in the app** beyond the extra field on the Done write.

Out of scope: showing completion dates in the desktop app (it has no Done
view), a Done/history page, and fixing the `Quarter` formula or the `Nonsense`
status option (both raised in Findings; both are Base cleanups the owner can
do in Lark in under five minutes).

Open question: **D12**.

---

## R9 — Settings overlay, version, update check, and getting the update

### R9a — The Settings overlay (do NOT build this first)

**Requirement.** A blurred-background overlay showing the running version and
the result of an update check, with a `Check again` action.

Design: generalise the existing `.confirm` into a shared `.overlay` /
`.overlay__box` base and make the confirm dialog a variant of it, then add
`backdrop-filter: blur(6px)` — the backdrop is already translucent
(`color-mix(in srgb, var(--shadow) 82%, transparent)`), so blur works with one
line. The panel must fit the 360 × 380 minimum window and scroll if it does
not. Version comes from the already-permitted `getVersion()`.

States the panel must render, exactly one at a time:

- `You're up to date` when the running version equals the latest.
- `v<new> is available` plus the action chosen in D13.
- A named failure: offline, 404, non-JSON (a private repo serves an HTML login
  page — treat as failed, **never** as up to date), malformed semver, or a
  latest *older* than the running version (show up to date; never offer a
  downgrade).
- `Check again` stays usable after any failure.

**R9a has no user value on its own.** The version is already in the pager
footer. Its only new capability is the update check, which needs R9b. Building
it first ships a panel whose button always fails.

### R9b — The release pipeline (the actual blocker; nobody asked for this)

**Requirement.** There is a versioned, downloadable macOS build and a
machine-readable record of the latest version.

Needs, in order: a `.dmg` (or `.app.tar.gz`) built from a tagged commit; a
GitHub Release to attach it to; a publicly readable `latest.json` (or the
GitHub Releases API, which needs auth for a private repo — see Conflict 2); and
tag-triggered CI on a macOS runner to produce all of it. Plus the existing
unanswered **D1** (where artifacts live) and the three-places version bump
(`tauri.conf.json` / `package.json` / `Cargo.toml`, all still `0.1.0`) made
automatic rather than manual.

Also unresolved from the predecessor spec and still true: **a `.dmg` on its own
does not make the app usable to a new teammate** — though the zero-config work
in `36455d7` has removed most of that problem. Worth re-confirming that a fresh
machine with no `~/.config/omsn/` at all now works end to end.

Size: M–L. **Recommend building this before R9a or R9c.**

### R9c — How the update is obtained

Three genuinely different answers; see **D13**. Honest costs:

| | What the user gets | What it costs | What it rules out |
|---|---|---|---|
| **1. Check only** | A notice and a button that opens the release page. They download and drag. | Needs R9b and one public URL. No keypair, no new plugin, no fee. ~Half a day on top of R9a. | Nothing. Fully forward-compatible with 2 and 3. |
| **2. Download and open the .dmg** *(recommended)* | The app fetches the `.dmg` to `~/Downloads`, shows progress, and opens it in Finder. They drag across and relaunch. | `reqwest` is already a dependency and `tauri_plugin_opener` is already used, so **no new dependency**. Needs R9b. ~1–1.5 days. | Does not literally restart for them — the drag and the relaunch stay manual. |
| **3. True in-app updater** | Exactly what was asked: download, replace, relaunch. | `tauri-plugin-updater` + `updater` bundle config + a **Minisign keypair whose private half is permanent and unrecoverable** (lose it and all 14 installs are orphaned and must be reinstalled by hand). Either public artifact hosting or a **PAT shipped in the binary**, reversing `36455d7`. `tauri-plugin-process` for the restart (`core:app` has no `allow-restart` — verified). Apple Developer ID **USD 99/yr** for signing + notarization, without which Gatekeeper may block the relaunched bundle. CI that does not exist yet. ~2–4 days plus the fee plus a one-way door. | Reverses the earlier "manual check only" decision and the no-shipped-secret property. |

Proportion check, stated plainly: for 14 engineers who all already have the
repo, option 3 saves each person roughly two minutes per release. At one
release a month that is ~28 minutes of team time a month, bought with a
permanent unrecoverable secret, an annual fee, and a credential in the binary.
Option 2 captures most of the felt benefit with none of those.

Acceptance criteria (whichever option, shared):

- Behind a release, out of date, and up to date are all demonstrated against a
  real published artifact — not a mocked `latest.json`.
- The check never blocks launch and never puts an error notice over the task
  list.
- For option 2 or 3: a download interrupted mid-flight leaves no partial file
  presented as valid, and the panel says what happened.
- For option 3 only: a tester verifies the relaunched bundle opens without a
  Gatekeeper prompt on a machine that has never seen the app. If it does
  prompt, option 3 has not actually shipped.

Open questions: **D1** (still), **D13**.

---

## Decisions required from the owner

| # | Decision | Recommendation | Blocks |
|---|---|---|---|
| **D7** | R6: adopt the server-side Owner filter, with a sign-in cross-check against one full walk as the safety net, and keep `only_mine`? | **Yes.** 4.6× faster for one extra startup walk that already happens; the cross-check closes both the namespace risk and the unmeasured multi-owner risk; `bitable:app` already covers it, so no re-consent. | R6-1 |
| **D8** | R6-3: shorten the settle poll to 150 ms, or replace it with a Tauri settle event? | **Event, with a 1 s backstop poll.** No capability change needed, and it removes a timer instead of adding one. If that feels like scope, 150 ms is a one-line interim. Backoff is the wrong direction either way. | R6-3 only |
| **D9** | R7: what is editable — title only, title + priority, or also remarks? | **Title only.** It is the stated need and the only field with no other repair path in the app. Priority is ~30 min as a three-chip picker if wanted later; remarks is a textarea and a length policy nobody asked for. | R7 |
| **D10** | R7: controls in the expanded row, or in the hover strip? | **Expanded row.** Four buttons do not fit 360 px, and delete must not sit next to `✓`. | R7 |
| **D11** | R7: does this Base have recoverable deleted records, and for how long? | Unverified — do not guess. **The confirm copy must assume there is no undo** until this is answered. One look in Lark settles it. | R7 confirm copy |
| **D12** | R8: add a writable `Completed Date` field (recommended), or just change the formula to `"YYYY-MM-DD"`? **And: does any Lark view, dashboard or report group by `Completed Month`?** | **Add the field.** The formula route gives false precision over `Modified`, which is demonstrably wrong for at least four records, and fragments any monthly grouping. Adding the field also fixes the re-stamping bug. This is a shared-Base schema change and is therefore yours to approve. | R8 |
| **D13** | R9: check-only, download-and-open-the-.dmg, or the true in-app updater? And do you accept that R9b (CI + release) comes first? | **Download-and-open-the-.dmg, after R9b.** It needs no new dependency, no keypair, no fee, and no shipped credential, and it keeps option 3 open. If you want option 3, say so explicitly — it reverses two earlier decisions and creates a permanent secret. | R9 |
| **D1** | *(carried over, still unanswered)* Where do release artifacts and `latest.json` live? | A small **public** repo or bucket for artifacts only, so no PAT ships in the binary. | R9b, R9c |

---

## Build order

| Order | Item | Size | Notes |
|---|---|---|---|
| 1 | **R6-1** server-side Owner filter + cross-check + poll debounce | M | Biggest measured win. Also shrinks R6-2's race window 4.6×. |
| 2 | **R6-2** reject a poll older than the store's last mutation | S | Real visible-revert bug. Pure `sync.rs` + `store_cell.rs`; fully testable against `FakeRepo`. |
| 3 | **R6-4** create/delete out of the store lock + ownership check | S | **Prerequisite for R7.** |
| 4 | **R6-3** settle event (or 150 ms) | S | This is the one that answers "I still feel a bit delay". |
| 5 | **R8 step 1** Lark: add `Completed Date`, re-point the formula | XS (owner, in Lark) | Needs D12. No code. |
| 6 | **R7** edit title + delete with confirm | M | Needs D9/D10/D11 and item 3. |
| 7 | **R8 steps 2–3** stamp `Completed Date` on Done, both clients | S | Needs item 5. |
| 8 | **R9b** CI + `.dmg` + Release + `latest.json` | M–L | Nobody asked for it. Blocks all of R9. Needs D1. |
| 9 | **R9a** Settings overlay | M | Worthless before item 8. |
| 10 | **R9c** the chosen download behaviour | S / M / L | Needs D13. |

---

## Recommended against building

- **The true in-app updater (R9c option 3), now.** A permanent unrecoverable
  Minisign key, plus either public hosting or a PAT in the binary that reverses
  `36455d7`, plus USD 99/yr, plus `tauri-plugin-process`, plus CI that does not
  exist — to save 14 people two minutes a release. Option 2 gets most of the
  benefit with no one-way door. If the owner still wants it, that is their call,
  but it should be made with these numbers in front of them.
- **R9a before R9b.** The version is already on screen. Shipping a Settings
  panel whose only button always fails makes the app look broken.
- **Settle backoff.** The settle read is free; making later feedback slower is
  the opposite of the fix.
- **Lowering `POLL_INTERVAL_MS` as the answer to R6.** The reported symptom is
  the user's own click, which is already instant. 20 s → 10 s is a reasonable
  follow-up *after* R6-1 (it would cost less than today does), but shipping it
  as the fix would misattribute the bug.
- **Changing `Completed Month` to `"YYYY-MM-DD"` on its own.** It would satisfy
  the request in thirty seconds and make the data more misleading, because the
  source is `Modified`, not a completion event.
- **Back-filling historical completion dates.** There is no truthful source.
  Blank is better than wrong.
- **Editing remarks, owner, category or due date in the desktop app**, and bulk
  delete. Not asked for; each adds a control, a validation path and tests.
- **Removing `only_mine` once the server filter lands.** It is the access gate
  and it costs nothing.

---

## Verified / not verified

**Verified against the code:** the three `App.tsx` constants; that the settle
loop performs no network I/O; the overlapping-poll and stale-poll-after-settle
races; `create_task`/`delete_task` holding the lock across I/O; the absent
ownership check on both write commands; `bitable:app` scope coverage;
`core:app:default` granting `allow-version` and event listeners but **not**
restart; `.task--pending` being an opacity change; that the app renders no
absolute date anywhere.

Also confirmed: `due_date` is parsed in Rust, carried through `Task` and
`types.ts`, and **never rendered** — a dead data path, harmless, noted so
nobody assumes it is on screen somewhere.

**Verified against the live Base:** all 19 field names and types; the
`Completed Month` formula text and its lack of any formatter; the absence of
any completion-date field; `yyyy/MM/dd` on all three real date fields;
`Owner` being `multiple: true`; real Done records showing `"2026-04"`-shaped
strings and `""` for non-Done; the `Nonsense` status option; the hardcoded
`Quarter` boundary.

**Verified against GitHub:** repo is private; **no releases**; no
`.github/workflows`; the only tag is the plugin's `v0.3.1`.

**Not verified — must be settled before the dependent item ships:**

- Whether Lark's `is` operator on a `multiple: true` person field matches
  co-owned rows. **The single largest risk in R6-1.** The cross-check contains
  it; a deliberate multi-owner test confirms it.
- Whether this Base has recoverable deleted records (**D11**).
- Whether any Lark view, dashboard or report groups by `Completed Month`
  (**D12**).
- Whether a fresh machine with no `~/.config/omsn/` now runs the app end to end
  after the zero-config work. Assumed, not re-tested.
- `records/search` response shape beyond `items` / `page_token` / `has_more`,
  and whether it needs its own pagination at 16 rows (it does not today, but a
  manager team-view would).
