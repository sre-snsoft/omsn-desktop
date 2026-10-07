<p align="center"><b>OMSN Desktop</b></p>

<p align="center">
A small floating window showing the tasks assigned to you in the Platform
team's Lark tracker.<br>
Add, start, finish and tidy your own work without opening Lark.
</p>

---

# Install

**One command.** Paste it into Terminal and press Enter.

```bash
OMSN_LARK_APP_SECRET=PASTE_SECRET_HERE bash -c "$(curl -fsSL https://raw.githubusercontent.com/sre-snsoft/omsn-desktop/main/install.sh)"
```

Replace `PASTE_SECRET_HERE` with the secret Adrian sent you, then:

1. The app installs itself and opens.
2. Click **SIGN IN WITH LARK** — your browser opens.
3. Click **Authorize**, then close the tab.
4. Your tasks appear. Done.

That is the whole setup. No dragging, no security warnings, no settings to fill
in. Works on both Apple Silicon and Intel Macs.

> **Don't have the secret?** Ask Adrian. The app still installs without it, but
> sign-in will tell you it is missing.

---

# Using it

| You want to | Do this |
|---|---|
| **Add a task** | Type in the box at the top, press Enter |
| **Start something** | Hover the row → **▶** |
| **Read a long title** | Click the task text — it expands |
| **Fix a typo** | Click the task → **EDIT** |
| **Finish a task** | Hover → **✓** → confirm |
| **Put it back** | Hover → **←** (back to Backlog) |
| **Delete it** | Click the task → **DELETE** (asks first; cannot be undone) |
| **Move the window** | Drag the top bar — it floats above other apps |
| **See more at once** | Resize it; the list fits itself to the window |

Your list shows **only your own tasks**, and never completed ones. Changes save
to Lark immediately — the same tracker `/omsn:create` and the weekly report
use, so it stays in step with everything else.

A task marked `STALE` has been In Progress for over two weeks without moving.
That is a prompt to discuss it at standup, not an error.

---

# Updating

**Settings (⚙) → Check again.**

If a new version exists it downloads, verifies it, and restarts itself. You do
not need to re-run the install command or sign in again.

---

# If something goes wrong

| What you see | What to do |
|---|---|
| *"desktop.env is missing OMSN_LARK_APP_SECRET"* | Re-run the install command with the secret |
| *"Your session expired"* | Click **SIGN IN** — sessions last about 30 days |
| *"No releases published yet"* on update | Nothing new to install; you are current |
| Nothing at all after signing in | You may genuinely have no open tasks — check the tracker |
| Anything else | Ping Adrian with a screenshot |

To start over completely: delete `/Applications/OMSN Desktop.app` and
`~/.config/omsn/`, then run the install command again.

---

<details>
<summary><b>For maintainers</b></summary>

### Why install via a script

The app is not signed with an Apple Developer certificate ($99/yr), and macOS
quarantines anything a *browser* downloads — so a hand-dragged `.dmg` is
refused as "unidentified developer". The installer places the app and clears
that flag before first launch, so the dialog never appears. Updates are
delivered by the app itself and are unaffected.

### Develop

```bash
npm install
npm run tauri dev      # Rust changes need a restart; React hot-reloads
npm test               # frontend
cd src-tauri && cargo test --lib
```

One instance at a time: it holds port 8765 for the sign-in redirect.

Read **`docs/STATUS.md`** before changing anything non-obvious. It records the
decisions already made, the facts verified against the live Lark tenant, and
what is still open — several were expensive to learn.

### Release

```bash
./dist-tools/release.sh --dry-run   # rehearse
./dist-tools/release.sh             # build both arches, sign, publish
```

Bump the version in `package.json`, `src-tauri/tauri.conf.json` and
`src-tauri/Cargo.toml` first — the script refuses to build if they disagree.

Signing uses the Minisign key at `~/.config/omsn/updater.key`. The matching
public key is compiled into every installed copy, so an update that does not
verify is refused. **The key is in no repository and cannot be regenerated:**
a new keypair would make every existing install reject every future update.

</details>
