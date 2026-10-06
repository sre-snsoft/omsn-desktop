# Spec — OMSN Desktop: verify login, readable titles, live status, version & updates

Date: 2026-10-01
Source: five owner requirements (R1–R5), triaged.
Status: **R1 ready to execute. R3, R4 ready to build. R5 blocked on D5. R2 blocked on D1–D3.**

Standing constraint: keep it minimal. 14 internal SRE engineers on macOS. The
Base schema does not change; the OMSN Claude plugin writes to the same table.

---

## R1 — Verify the new in-app Lark sign-in (VERIFY, not a feature)

**Requirement.** A human must exercise the OAuth flow end to end against the
live tenant before anything else is built on top of it.

**Why it cannot be deferred.** 109 Rust + 28 frontend tests cover request
*shape* and error *handling*. Nothing in the suite has exchanged a real
authorization code with `open.larksuite.com`. Items 2, 3, 7 and 10 below can
only be checked by a person.

### Acceptance criteria (run in order)

| # | Step | Expected |
|---|---|---|
| 1 | `rm -f ~/.config/omsn/token.json`, launch | Welcome screen with `SIGN IN WITH LARK`. **Not** an error notice. |
| 2 | Click sign in, approve in the browser | Consent screen lists exactly `bitable:app`, `contact:user.base:readonly`, `offline_access`. Browser tab reads "Signed in. You can close this tab and return to OMSN." App shows a `Signed in` toast and your tasks. |
| 3 | Compare `OMSN_OAUTH_REDIRECT` in `~/.config/omsn/desktop.env` with the Lark console entry | Byte-identical, and loopback (`http://127.0.0.1:<port>/...`). A mismatch is the single most likely failure; `oauth::validate_redirect` refuses non-loopback **before** opening a browser. |
| 4 | Launch a second copy of OMSN, sign in there | Message "Cannot listen on 127.0.0.1:`<port>` for the sign-in redirect (is another copy of OMSN running?)", raised **before** a browser opens. No code is delivered to the other process. |
| 5 | Start sign-in, click Deny on the consent page | Clear failure in the app, retry works, no hang. |
| 6 | Start sign-in, walk away >3 min (`CONSENT_TIMEOUT` = 180 s) | App stops being busy and offers sign-in again. |
| 7 | Sign in, quit, relaunch | Tasks load with **no** browser prompt (so `offline_access` really granted and the refresh token persisted). `ls -l ~/.config/omsn/token.json` is `-rw-------`. |
| 8 | Edit `token.json`: set `expires_at` to the past, keep `refresh_token`. Relaunch | Silent refresh, no browser. This path previously dead-ended. |
| 9 | Edit `token.json`: blank the `refresh_token`. Relaunch | Session-expired notice **with** a SIGN IN button that actually opens a browser (routes to `authorize`, not `sign_in`). |
| 10 | Cross-check the list against the Base | Only your own rows. A task owned by someone else must not appear (`only_mine`, matched on `open_id` only). |
| 11 | `npm run tauri dev` and sign in | **One** browser tab opens, not two, and no random logout (React StrictMode fires `sign_in` twice; the `connecting` mutex must absorb it). |

**Out of scope.** Fixing anything found — report it, triage separately.

**Open questions.** None. Execute.

---

## R2 — Settings panel: current version, update check, auto-check toggle (HIDDEN COST)

**Requirement (symptom, restated).** The owner wants to know when a newer
version exists and to be able to get it, without asking anyone.

Their hypothesis — "a Settings panel like Penguin's, with an auto-updater" —
is the expensive solution to that symptom. Verified costs are in the findings
section of the triage. The spec below covers only the subset recommended for
now; the full silent updater is deferred pending **D1–D3**.

### Phase A — ship now (recommended scope)

**Requirement.** The app tells the user what version they are running and, on
demand, whether a newer release exists.

Acceptance criteria:
- A `SETTINGS` affordance in the titlebar opens an in-app panel (same pixel
  treatment as the existing `.confirm` overlay — reuse it, do not introduce a
  second overlay style).
- The panel shows `Current version: <x.y.z>`, read from `getVersion()`
  (`@tauri-apps/api/app`). **Verified: already permitted** — `core:default` →
  `core:app:default` → `allow-version`. No capability change.
- A `Check for updates` action fetches a published `latest.json`, compares
  semver against the running version, and renders exactly one of:
  - `You're up to date!`
  - `v<new> is available` plus an action that opens the release page in the
    browser (`tauri_plugin_opener`, already a dependency).
  - A failure line that names the reason (offline / 404 / malformed JSON) and
    leaves `Check for updates` usable.
- The check runs once on startup and on demand. Nothing else.
- A checkbox `Check on startup`, **on** by default, persisted to
  `~/.config/omsn/settings.json` (mode 0600, same directory as the existing
  config). A missing or corrupt file falls back to defaults and does not block
  launch.

Failure cases a tester must check:
- No network → the panel says so; the app still works; no error notice in the
  main window.
- `latest.json` returns HTML (e.g. a login redirect, which is what a *private*
  repo does) → treated as a failed check, not a crash, and not reported as
  "up to date".
- `latest.json` advertises a version **older** than the running one (a rolled
  back release) → shows up to date, never offers a downgrade.
- Version string is not valid semver → failed check, not a silent pass.

Explicitly out of scope for Phase A:
- Downloading or installing anything. No `tauri-plugin-updater`, no Minisign
  keypair, no in-place bundle replacement.
- Hourly polling and re-checking on window focus. **Declined** — the app
  already polls Lark every 20 s while focused; a second background poller
  against a release manifest, for 14 people, buys nothing a startup check does
  not.
- Any setting other than the startup-check checkbox.

Open questions blocking Phase A:
- **D1** — where does `latest.json` live? Phase A needs one publicly readable
  URL and nothing more, so it is cheap either way, but it must be decided.

### Phase B — full silent updater (deferred, not specified)

Not specified until **D1, D2, D3** are answered and the `desktop.env`
distribution problem (below) is solved. Specifying it now would be designing
around an undecided hosting model.

### Prerequisite nobody asked about — and the real blocker

`AppConfig::load` requires `~/.config/omsn/desktop.env` with
`OMSN_BASE_TOKEN`, `OMSN_TABLE_ID`, `OMSN_LARK_APP_ID`,
**`OMSN_LARK_APP_SECRET`** and `OMSN_OAUTH_REDIRECT`.

A `.dmg` therefore does not make the app usable. Distribution means putting
the Lark **app secret** on 14 laptops as a hand-created file. An auto-updater
would be keeping up to date an app that a new teammate still cannot start.
**This outranks R2.** It is not one of the five requirements, so it is raised,
not silently added — see D6.

---

## R3 — Click a task to expand its full title (NEEDS A DECISION → recommended design below)

**Requirement.** Long task titles must be readable without resizing the window.

Verified mechanism of the symptom: `.task__title` is
`white-space: nowrap; overflow: hidden; text-overflow: ellipsis`. At the
default 440 px window roughly 31 characters fit, which matches the reported
`"Use AI to map Pulsar producers, t…"`. The only escape hatch today is the
native `title=` tooltip.

### Accepted design (pending **D4**)

**Clicking the task body toggles that row between clamped and fully wrapped.**

Acceptance criteria:
- Clicking anywhere in `.task__body` (title + meta) expands that row so the
  whole title wraps onto as many lines as it needs. Clicking it again
  collapses it.
- **The status marker keeps its current meaning.** The expand handler is bound
  to `.task__body` only, never to the `<li>`. The marker still advances
  status on click; the action buttons still act. No `stopPropagation` games,
  because the hit areas do not overlap.
- At most one row is expanded at a time. Expanding a second row collapses the
  first, so page height stays predictable.
- The expanded row is keyboard reachable and operable (`Enter`/`Space`), with
  `aria-expanded` reflecting state.
- Dragging across the title to select text does not toggle the row.
- Expansion state is per-row and transient: it resets on page change and is
  not persisted. A poll that replaces the snapshot must not collapse the
  expanded row (key off `record_id`).
- `usePagination`'s `ROW_HEIGHT_PX` assumption is documented as collapsed-row
  height; with one row expanded the list scrolls (`.list` is already
  `overflow-y: auto`) rather than clipping. A tester must confirm no content
  is unreachable at the 360 × 380 minimum window.

Bundled free win (do this regardless of D4):
- `.task__actions` is `opacity: 0` at rest but still occupies flex width,
  which is part of why so few characters fit. Make it not consume layout width
  at rest (`position: absolute` over the row, or `visibility`), recovering
  roughly 7 characters for every row with no interaction change.

Out of scope: editing the title, a detail view, showing remarks/category/
workstream in the expanded state. Expansion reveals the **title** only.

Open questions: **D4**.

---

## R4 — Show version and time in the app (CLEAR)

**Requirement.** The window shows the current time and the running version,
in the style of Penguin's header clock and `v1.16.0 · NgSE` status bar.

Acceptance criteria:
- A clock in the titlebar, local time, `h:mm AM/PM`, updating at least once a
  minute. Ticking it every second is acceptable and has a useful side effect:
  the pager's `Xm ago` label currently only refreshes when something else
  re-renders, and a 1 s tick makes it live.
- `v<x.y.z> · <viewer initials>` in the pager footer, from `getVersion()` and
  `viewer.display_name`. Before sign-in, show the version with no initials.
- The version string matches `tauri.conf.json`. **`tauri.conf.json` is the
  single source of truth**; `package.json` and `Cargo.toml` are currently all
  `0.1.0` and must be kept in step by the release step, not by hand in three
  places. A tester bumps `tauri.conf.json` to `0.1.1`, rebuilds, and sees
  `v0.1.1`.
- At the 360 px minimum width nothing overlaps and nothing is clipped. The
  existing `@media (max-width: 340px)` rule hides `.pager__synced`; the
  version line follows the same rule.
- The titlebar keeps its 78 px left padding for the overlaid traffic lights,
  and the clock stays inside `data-tauri-drag-region` so dragging the window
  still works from it.

Out of scope: timezone selection, 24-hour toggle, a date, build hash.

Open questions: none, apart from the cosmetic placement — clock in the
titlebar next to `N ACTIVE`, version in the footer. Say so if the owner wants
it the other way round.

---

## R5 — Moving a task back to Backlog is not reflected immediately (BUG — cause found)

**Requirement.** A status change must be visible in the list immediately, not
after a multi-second pause.

### Verified cause

1. `App.tsx` `setStatus` does
   `setSnapshot(await invoke<Snapshot>('update_task', …))`. There is **no
   optimistic update and no in-flight state** — the row is not disabled, not
   dimmed, nothing. The DOM is unchanged from the click until the IPC promise
   resolves.
2. `commands.rs` `update_task` takes `state.store.write().await` **before**
   calling `store.update`, and holds that exclusive lock across the network
   write.
3. `commands.rs` `list_my_tasks` (the 20 s poll, `refresh: true`) takes the
   **same** write lock and holds it across `store.refresh()` →
   `repo.list_all()`, which is a **full-table walk**: up to 20 sequential
   HTTPS GETs of 200 records each against `open.larksuite.com`. There is no
   server-side filter; `only_mine` filters in Rust afterwards.
   → If a poll is in flight when the user clicks, the click's `PUT` cannot
   even begin until the entire poll completes. `tokio::sync::RwLock` is
   FIFO-fair, so the writer simply queues. Worst case = poll duration + PUT
   duration.
4. `lark.rs` `send` may first call `access_token()`, which takes the shared
   tokens mutex and can perform a token refresh — another round trip — before
   the `PUT`. Timeouts are 10 s connect / 30 s request, so the worst case is
   tens of seconds of a completely inert UI.

**Conclusion: the delay is the network round-trip, serialised behind the
poll's full-table fetch, with zero UI acknowledgement.**

### Ruled out, with reasons

- **Not the 20 s poll interval.** `setStatus` renders the snapshot returned by
  `update_task`; it does not wait for the next tick. If it did, the delay
  would be up to 20 s and quantised, not "a few seconds".
- **Not the pending overlay.** `Store::update` inserts the overlay, then on
  success folds the server's record into `polled` and **removes the overlay**
  before `commands.rs` takes the snapshot. `pending_ids` is therefore always
  empty for a successful write, and `.task--pending` can never render for one.
  The overlay only guards against a poll landing *between* insert and
  removal — which cannot happen, because the write lock is held across both.
  **The pending-write machinery is currently dead code from the UI's point of
  view.** It is not a bug to fix separately: it comes back to life as part of
  this item.
- **Not the sort order — but there is a second, separate defect here.** After
  the move, `sortTasks` ranks `Backlog` 4th of 5 and orders within a group by
  `modified` **ascending**. The just-touched row has the newest `modified`, so
  it lands at the **very bottom of the entire list**. With more tasks than fit
  one page (`perPage` ≈ 7 at the default 660 px window) the row leaves the
  current page and the pager does not follow it: the user sees it vanish with
  no indication of where it went. On a single-page list it stays visible,
  which is consistent with the owner's "it comes back after a few seconds".

### Accepted fix (pending **D5**)

Three parts. Parts 2 and 3 are needed regardless of how D5 is answered.

1. **Instant UI, eventual consistency.** A status change renders immediately
   and settles against the server afterwards. Preferred implementation is in
   **Rust, not React** — split `Store::update` into "insert the overlay and
   return the overlaid snapshot now" plus "perform the write in the
   background", and let the existing `retire_confirmed` logic resolve it. This
   keeps the architecture's stated rule ("Rust holds one snapshot; the UI keeps
   no cache of its own") and makes four already-written, already-passing
   overlay tests live instead of dead. A React-side optimistic cache would be
   faster to write and would violate that rule.
2. **A write must not queue behind a poll.** Either skip/abort a scheduled
   poll while a write is in flight, or restructure `refresh` to fetch outside
   the store lock and swap the result under it.
3. **Do not lose the row.** When a status change moves a task to a different
   page, the view must follow it (jump to its new page) or the row must stay
   pinned in place until the next poll. Needs the owner's preference; pinning
   is less jarring, following is more honest.

Acceptance criteria:
- Clicking `←` on an In Progress task repaints the row as `Backlog` within one
  animation frame, with no network wait.
- The row is visibly marked in-flight until confirmed (`.task--pending`
  already exists and is already styled — make it actually render).
- A **rejected** write snaps the row back to its true status **and** surfaces
  an error the user can read. Silent reversion is not acceptable.
- A poll that lands mid-write does not revert the user's change (the existing
  `an_edit_does_not_revert_when_a_stale_poll_lands` test must still pass, and
  must now exercise the real code path rather than a hand-inserted overlay).
- Clicking `←` repeatedly, or `←` then `✓`, does not strand the UI in a wrong
  state or send conflicting writes.
- With more tasks than one page, the moved task is still findable immediately —
  per the D5 answer.
- Measurement the tester records: time from click to repaint (target: under
  one frame) and time from click to confirmed (informational).

Out of scope:
- Server-side filtering of `list_all` to only the viewer's rows. It would cut
  poll cost substantially and is worth doing later, but it changes the request
  shape against a shared Base and is not needed to fix this bug.
- Changing the poll interval.
- Changing `sortTasks`.

Open questions: **D5**, and part 3's pin-vs-follow choice.

---

## Decisions required from the owner

| # | Decision | Blocks |
|---|---|---|
| D1 | Where do release artifacts / `latest.json` live? | R2 Phase A and B |
| D2 | Full silent updater now, or manual check + open the release page? | R2 Phase B |
| D3 | Buy an Apple Developer ID (USD 99/yr) so 14 people do not fight Gatekeeper, or document the "Open Anyway" step once? | R2 Phase B |
| D4 | R3: 2-line clamp only, click-to-expand, or both? | R3 |
| D5 | R5: is instant UI with eventual consistency acceptable — accepting that a rejected write snaps back a moment later? And pin vs follow for a row that changes page? | R5 |
| D6 | Do you want `desktop.env` / app-secret distribution specced? It is the actual blocker on shipping the app to anyone. | R2, and distribution generally |

## Build order

| Order | Item | Size | Notes |
|---|---|---|---|
| 0 | R1 verification | S (30–45 min, manual) | Blocks everything. No point polishing an app nobody can sign into. |
| 1 | R5 bug | M | Highest felt value. Needs D5. |
| 2 | R3 expand | S | Needs D4. Bundled CSS width win is unconditional. |
| 3 | R4 version + clock | S | Cheapest win; prerequisite for R2's UI. |
| 4 | R2 Phase A | M | Needs D1. |
| — | R2 Phase B | L + a one-way door | Deferred. Needs D1–D3 and D6. |

## Recommended against building

- **The full Tauri silent updater, now.** The Minisign private key is a
  permanent, unrecoverable secret: lose it and every installed copy must be
  reinstalled by hand. Do not create that one-way door before distribution is
  solved.
- **Hourly and on-focus update polling.** Startup plus a manual button covers
  the need for 14 people.
- **A general Settings panel**, if the update section is all it would contain.
  A footer line plus a "Check for updates" item is enough; a panel earns its
  keep when there is a second setting to put in it.
- **Fixing the dead pending-overlay code as its own task.** It is subsumed by
  R5.
