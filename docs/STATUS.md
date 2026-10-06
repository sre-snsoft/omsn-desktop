# OMSN Desktop — current state

Living handoff note. Update it when something here stops being true.

Last updated: 2026-10-06 · repo **`sre-snsoft/omsn-desktop`** (public) · branch `main`
Tests: **172 Rust** (`cd desktop/src-tauri && cargo test --lib`) · **67 frontend** (`cd desktop && npm test`) · 0 warnings

## Where things live

| | |
|---|---|
| Source, releases, installer | **`sre-snsoft/omsn-desktop`** (public), branch `main` |
| The OMSN Claude plugin | `sre-snsoft/task-management` — a *separate* writer to the same Base |
| Signing key | `~/.config/omsn/updater.key` — in no repository, unrecoverable |

The app was extracted from `task-management` with `git subtree`, so its
history came with it. It is public because the Tauri updater fetches plain
URLs and cannot authenticate — a private repo would need a second public one
just to host artifacts, or a token shipped in the binary. Audited before the
move: no credentials in the tree. The app id and Base ids are identifiers, not
secrets; reading the data still needs the viewer's own Lark session.

`task-management@omsn-desktop` remains only as an archive of where this came
from. Nothing merges.

## What this is

A floating pixel-art macOS widget showing the signed-in person their own tasks
from the team's existing Lark Base (`SW1zbLwdNaYsyAsIpBElmk8rglh` /
`tblvOg8Ge09bG85d`). Tauri 2, Rust core + React/TS.

The Base schema is the contract and does not change casually. The OMSN Claude
plugin writes to the same table, so **this app is never the only writer** —
that single fact drives most of the design below.

## Run it

```bash
cd desktop && npm run tauri dev     # Rust changes need a restart; React hot-reloads
```

One instance only: it holds port 8765 for the sign-in redirect.

## Decisions already made — do not silently revisit

| Decision | Why |
|---|---|
| **Personal-first, enforced in Rust** at the command boundary, before serialising. No "all tasks" command exists. | Filtering in the UI would mean the whole team's rows had already crossed into the webview. |
| Base **advanced permissions not enabled** | Would hide unassigned work from everyone and break the shared Kanban. Owner assigns at standup instead. |
| **Done is never listed**; On Hold is | On Hold is blocked, not finished — hiding it lets it rot unseen. |
| **Manual update check**, not a silent updater | A Minisign key is permanent and unrecoverable; not worth creating that door before distribution works. Owner later asked for a true updater — see Open. |
| **No Apple Developer cert** | One-time `xattr -dr com.apple.quarantine` per person. In-app updates replace the bundle themselves, so quarantine only bites the first install. |
| **Title only** is editable in-app | The stated need (typos) and the only field with no other repair path. |
| Delete confirms as **permanent** | Nobody has verified whether this Base has a recoverable trash. |

## Hard-won facts (verified against the live tenant — don't re-derive)

- **Lark requires `client_secret`** on both the authorization-code and refresh
  exchanges. A secret-less call fails `invalid_client` / "The auth method is
  not supported" (code 20140). PKCE is sent but does **not** replace client
  auth; it stops a local process racing the loopback redirect.
  *Trap that fooled this project twice: Lark validates the **grant before the
  client**, so probing with an invalid code or refresh token always returns
  `invalid_grant` and never reveals the client-auth requirement. Only a valid
  grant tells the truth.*
- **`defaults::APP_SECRET` is deliberately empty.** The secret is read from
  `~/.config/omsn/desktop.env`; a test pins that no secret is compiled in.
  `AppConfig::ready_to_sign_in()` fails before the browser opens, naming the
  file and key.
- **The redirect listener must bind both loopback families.** `localhost`
  resolves to `127.0.0.1` *and* `::1`; browsers here prefer IPv6, so an
  IPv4-only listener meant the code was never delivered and sign-in timed out
  silently.
- **Owner filter uses `contains`, never `is`.** `Owner` holds multiple people;
  `is` matches only sole-owner rows and silently dropped a co-owned task
  (verified: `is` → 16 records, co-owned missing; `contains` → 17, present).
  Filtered read ≈ 541 ms vs ≈ 2586 ms for the 315-record full walk.
- **`CurrentUser`** is rejected as a filter value (1254018); an explicit
  `open_id` is required.
- **`open_id` is app-scoped but reconciles**: Lark resolves person fields into
  the *requesting* app's scope, so rows written by the OMSN plugin carry this
  app's id for the same human. Ownership matches on `open_id` only — never
  display name, which is mutable and can collide.
- **An unknown single-select value is *added* as a new option**, it is not
  rejected. `TaskPatch::validate` exists because a stray `Nonsense` option
  once reached the shared Base this way. Validate before every write.
- Lark **datetime fields are epoch milliseconds**.

## Architecture worth knowing before editing

- `sync.rs` — the only owner of server state. Nothing here does I/O.
  `begin_update` lays a pending overlay and returns a `seq`; `settle_update`
  folds the server's answer in, or removes the overlay and records a
  `WriteFailure`. Writes carry **only changed fields**, because the plugin may
  have edited a different column.
- `store_cell/` — one rule: **no I/O while the store lock is held.** A poll
  clones the repo handle, releases, walks, then re-takes the lock for
  microseconds. Breaking this is what made clicks queue behind a 2.6 s walk.
- Stale polls are **discarded**: a walk that began before the data currently
  held cannot overwrite it. Without this, an older walk returning late reverted
  settled writes and released the row pin.
- `oauth.rs` — loopback consent flow. Binds **before** opening the browser;
  serves each connection on its own thread; echoes nothing from the query
  string into the callback page.
- Rejections reach the UI through `Snapshot.write_failures` and are **not**
  cleared by a poll — the command has already returned, so there is no promise
  left to reject, and a poll landing moments later would erase the only notice.
- Errors: `CoreError::Invalid` for a bad user value, `Config` for a broken
  install. They read very differently to a non-engineer.

## Base changes already applied

- `Completed Date` (`fldcJSD3Ru`, datetime, `yyyy/MM/dd`) — written only on the
  write that sets `Done`, at **local noon** so the Base's timezone cannot show
  the previous day. Both the app and the plugin stamp it. No back-fill: there
  is no truthful historical source.
- `Completed Month` re-pointed to
  `IF(ISBLANK([Completed Date]),"",TEXT([Completed Date],"YYYY-MM-DD"))`. It
  previously read `Modified`, so it re-stamped on every edit.
- The stray `Nonsense` status option has been removed.

## Agents (`.claude/agents/`)

`product-manager` → `architecture-reviewer` → `implementer` →
`qa-tester` + `standards-auditor` + `code-reviewer`

The PM surfaces options and does **not** decide what the owner should decide.
The implementer is the only agent that writes production code and must stop and
report rather than improvise when a spec is wrong. **Agents must not make live
Lark calls** — one stalled doing exactly that; tests use `FakeRepo`.

## Distribution — working, verified end to end

Public artifacts repo: **`sre-snsoft/omsn-desktop`** (downloads and
`latest.json` only; source stays private). Current release **v0.1.1**, built
for `aarch64` *and* `x86_64`.

Install is one command:

```bash
OMSN_LARK_APP_SECRET=<secret> bash -c "$(curl -fsSL https://raw.githubusercontent.com/sre-snsoft/omsn-desktop/main/install.sh)"
```

Verified on a real machine: downloads, installs to `/Applications`, leaves no
`com.apple.quarantine`, writes `desktop.env` at 0600 preserving an existing
secret, and **launches with no Gatekeeper dialog** — because the quarantine
flag is only applied to things a *browser* downloads. That is why this is a
script rather than "drag the .dmg across", and why no Apple certificate is
needed.

Updating is Settings (⚙) → Check again. Verified live: v0.1.0 → v0.1.1
downloaded, signature-verified, self-replaced and relaunched.

Cut a release with `desktop/dist-tools/release.sh` (`--dry-run` to rehearse).
It refuses to build on a version mismatch across the three files that declare
it, detaches a stale `/Volumes/OMSN*` first (which otherwise fails deep inside
`bundle_dmg.sh`), logs build output rather than discarding it, and refuses to
republish an existing version.

**The signing key is the one unrecoverable thing here.**
`~/.config/omsn/updater.key` — lose it and every install must be replaced by
hand. It is not in either repository. Back it up.

The client secret travels with the install command. It is a *distribution*
secret for the internal team: not compiled into the binary, but anyone who can
run the installer has it. A test pins that `defaults::APP_SECRET` stays empty.

## Open — needs the owner

1. **Hand it to the team.** Send the install command plus the secret. Nothing
   technical is blocking this.
2. **Token in the Keychain.** Still a mode-600 file. The `keyring` crate is
   already a dependency and `CoreError::Keyring` exists. Note it would also
   stop `desktop/probe/` sharing the session, which is currently useful for
   verifying Lark behaviour.

## Known gaps

- Token is a mode-600 file at `~/.config/omsn/token.json`. The `keyring` crate
  is already a dependency and `CoreError::Keyring` exists; moving it there
  would also stop the probe sharing the session, which is currently useful for
  verification.
- `due_date` is parsed and sent to the webview but never rendered.
- `Quarter` in the Base is hardcoded to the 2026 Q2/Q3 boundary, so everything
  now reads `Q3` permanently.
- `oauth.rs` is over the 800-line house limit.
- No E2E test drives the real app; `desktop/probe/` is a working Python oracle
  for the Lark contract and is worth keeping.
