# How OMSN Desktop works

## The one thing that shapes everything

The app is **not the only writer** to the tracker. The OMSN Claude plugin
(`/omsn:create`, `/omsn:update`) writes to the same Lark Base table, and people
edit rows in Lark directly. Almost every design decision below follows from
that: writes carry only changed fields, a sent write is held until the server
confirms it, and a poll that started earlier can never overwrite newer data.

```mermaid
flowchart LR
    subgraph mac["Your Mac"]
        ui["React UI<br/><i>renders what it is given</i>"]
        core["Rust core<br/><i>owns all state</i>"]
        ui <-->|"Tauri IPC"| core
    end

    base[("Lark Base<br/>Work Tracker")]
    gh["GitHub Releases<br/><i>signed updates</i>"]

    core <-->|"HTTPS · your Lark session"| base
    core -->|"check · verify · install"| gh

    plugin["OMSN Claude plugin<br/><i>/omsn:create</i>"] --> base
    lark["Lark web / app"] --> base

    style core fill:#7b6cf6,color:#fff
    style base fill:#5ed39a,color:#0d2b1e
    style gh fill:#3a2d57,color:#f2ecff
```

## Why the Rust core holds the state

The UI keeps no cache of its own. Everything it shows comes from one snapshot
the core owns.

**Personal-first is enforced here, not in the UI.** The core filters to the
signed-in person *before* serialising, so other people's rows never cross into
the webview. There is deliberately no "list all tasks" command — filtering in
React would mean the whole team's data had already arrived in the browser
context, where any UI bug or injected script could reach it.

```mermaid
sequenceDiagram
    participant U as You
    participant R as React
    participant C as Rust core
    participant L as Lark Base

    U->>R: click ← (back to Backlog)
    R->>C: update_task
    C->>C: overlay the change
    C-->>R: snapshot (already applied)
    R-->>U: repaints immediately
    Note over C,L: the write continues in the background
    C->>L: PUT (only the changed field)
    L-->>C: updated record
    C->>C: retire the overlay
    C-->>R: event: settled
```

The overlay is what makes a click feel instant without lying. If the write is
**rejected**, the row snaps back *and* says why — the command has already
returned by then, so the failure travels back in the next snapshot rather than
as a rejected promise.

## Reading

A refresh asks Lark for only the rows you own (`records/search` filtered on
`Owner`), which is roughly five times faster than walking the table. At sign-in
the core cross-checks that filtered result against one full walk; **any**
disagreement and it falls back to full walks for the session. Losing the
speed-up is acceptable, losing a task is not.

Two rules keep the data honest:

- **No network I/O while the store lock is held.** A poll clones what it needs,
  releases the lock, fetches, then re-takes it briefly. Holding it across a
  fetch made clicks queue behind a multi-second read.
- **A poll that began before the data currently held is discarded.** Without
  that, a slow poll returning late would overwrite a newer write.

## Signing in

```mermaid
flowchart LR
    a["Click<br/>SIGN IN"] --> b["Browser opens<br/>Lark consent"]
    b --> c["Redirect to<br/>127.0.0.1:8765"]
    c --> d["Exchange code<br/>+ PKCE"]
    d --> e["Token stored<br/>mode 0600"]
    e --> f["Refreshed<br/>automatically"]
```

The redirect listener binds **both** loopback families. `localhost` resolves to
`127.0.0.1` and `::1`, and a browser preferring IPv6 against an IPv4-only
listener meant the code was never delivered — sign-in simply timed out with no
explanation.

PKCE is sent, but Lark still requires the client secret, so the app is a
confidential client. The secret is read from `~/.config/omsn/desktop.env` and
is **not** compiled into the binary; a test keeps it that way.

## Updating

```mermaid
flowchart LR
    s["Settings<br/>Check again"] --> m["Read latest.json"]
    m --> v{"Newer?"}
    v -->|no| ok["Up to date"]
    v -->|yes| dl["Download"]
    dl --> sig{"Signature<br/>valid?"}
    sig -->|no| stop["Refuse"]
    sig -->|yes| r["Replace + restart"]
```

Every release is signed with a Minisign key; the matching public key is
compiled into the app, so a bundle that does not verify is refused. That
matters because GitHub releases are writable by anyone with repo access — the
signature, not the hosting, is what makes an update trustworthy.

## Where the code lives

| Path | Holds |
|---|---|
| `src-tauri/src/sync.rs` | The snapshot and the pending-write overlay. No I/O. |
| `src-tauri/src/store_cell/` | Locking. Enforces "no I/O while the lock is held". |
| `src-tauri/src/lark.rs` | Bitable transport, token refresh, the owner filter. |
| `src-tauri/src/oauth.rs` | Loopback consent flow. |
| `src-tauri/src/commands.rs` | The only place tasks cross into the webview. |
| `src-tauri/src/task.rs` | Field names and select values, verified against the Base. |
| `src/` | React UI. Renders; decides nothing about access. |
| `probe/` | Python oracle for the Lark contract — useful when the Rust client misbehaves. |

`docs/STATUS.md` records what was learned the hard way. Read it before changing
anything non-obvious.
