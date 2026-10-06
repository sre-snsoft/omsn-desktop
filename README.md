<p align="center"><b>OMSN Desktop</b></p>

A floating macOS widget showing the tasks assigned to you in the Platform
team's Lark tracker. Add, start, finish and tidy your own work without opening
Lark.

Only your own tasks are ever fetched — that is enforced in the Rust core,
before anything reaches the UI.

## Install

```bash
OMSN_LARK_APP_SECRET=<secret> bash -c "$(curl -fsSL https://raw.githubusercontent.com/sre-snsoft/omsn-desktop/main/install.sh)"
```

Ask Adrian for the secret. One command: it picks the right build for your Mac,
installs it, and opens it.

**Why a script rather than dragging the .dmg across.** The app is not signed
with an Apple Developer certificate, and macOS quarantines anything a *browser*
downloads — so a hand-dragged bundle is refused as "unidentified developer".
The installer places the app and clears that flag before first launch, so the
dialog never appears.

## Updating

Settings (⚙) → **Check again**. Updates download, verify their signature and
restart in place. An update that fails verification is refused.

## Developing

```bash
npm install
npm run tauri dev      # Rust changes need a restart; React hot-reloads
npm test               # frontend
cd src-tauri && cargo test --lib
```

One instance at a time: it holds port 8765 for the sign-in redirect.

`docs/STATUS.md` is the handoff note — decisions already made, facts verified
against the live Lark tenant, and what is still open. Read it before changing
anything non-obvious.

## Releasing

```bash
./dist-tools/release.sh --dry-run   # rehearse
./dist-tools/release.sh             # build both arches, sign, publish
```

Requires the Minisign signing key at `~/.config/omsn/updater.key`. **It is not
in this repository and cannot be regenerated** — losing it means every
installed copy must be replaced by hand.

## Requirements

- macOS, Apple Silicon or Intel
- A Lark account with access to the Platform Work Tracker
