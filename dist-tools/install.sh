#!/bin/bash
#
# OMSN Desktop installer.
#
# Downloads the latest release, installs it, and writes the config the app
# needs. One paste, no manual steps.
#
#   curl -fsSL https://raw.githubusercontent.com/sre-snsoft/omsn-desktop-releases/main/install.sh | bash
#
# Why this exists rather than "drag the .dmg to Applications":
#   * The app is not signed with an Apple Developer certificate, so a bundle
#     downloaded by a *browser* carries com.apple.quarantine and Gatekeeper
#     refuses it. Clearing the flag here, before first launch, means the user
#     never sees that dialog. Updates after this are in-app and unaffected.
#   * Lark refuses a token exchange without a client secret, so the app needs
#     one on disk. Doing it here avoids asking 14 people to hand-edit a file.
set -euo pipefail

REPO="sre-snsoft/omsn-desktop-releases"
APP_NAME="OMSN Desktop.app"
CONFIG_DIR="$HOME/.config/omsn"
ENV_FILE="$CONFIG_DIR/desktop.env"

# Not secrets: the app id travels in every authorize URL, and the Base ids are
# addresses — reading the data still needs the person's own Lark session.
LARK_APP_ID="cli_aa31efc9e838df0e"
BASE_TOKEN="SW1zbLwdNaYsyAsIpBElmk8rglh"
TABLE_ID="tblvOg8Ge09bG85d"
OAUTH_REDIRECT="http://localhost:8765/callback"

# Shipped with the installer because Lark requires it. Treat it as a
# distribution secret for the internal Platform team, not a user secret.
LARK_APP_SECRET="${OMSN_LARK_APP_SECRET:-}"

say()  { printf '\033[1;35m▸\033[0m %s\n' "$1"; }
fail() { printf '\033[1;31m✗\033[0m %s\n' "$1" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || fail "OMSN Desktop is macOS only."

case "$(uname -m)" in
  arm64)  ASSET_ARCH="aarch64" ;;
  x86_64) ASSET_ARCH="x64" ;;
  *)      fail "Unsupported architecture: $(uname -m)" ;;
esac

WORK="$(mktemp -d)"
cleanup() {
  # Detach before removing, or the mount point lingers.
  [ -n "${MOUNTED:-}" ] && hdiutil detach "$MOUNTED" -quiet 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

say "Finding the latest release…"
DMG_URL=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
          | grep -o "https://[^\"]*${ASSET_ARCH}[^\"]*\.dmg" | head -1)
[ -n "$DMG_URL" ] || fail "No .dmg found for $ASSET_ARCH in the latest release of $REPO."

say "Downloading $(basename "$DMG_URL")…"
curl -fsSL --progress-bar "$DMG_URL" -o "$WORK/omsn.dmg" || fail "Download failed."

say "Installing to /Applications…"
MOUNTED=$(hdiutil attach "$WORK/omsn.dmg" -nobrowse -quiet \
          | grep -o '/Volumes/.*' | head -1)
[ -n "$MOUNTED" ] || fail "Could not mount the disk image."
[ -d "$MOUNTED/$APP_NAME" ] || fail "'$APP_NAME' not found inside the image."

# Quit a running copy, or the replace fails with a busy bundle.
osascript -e 'quit app "OMSN Desktop"' 2>/dev/null || true
rm -rf "/Applications/$APP_NAME"
cp -R "$MOUNTED/$APP_NAME" /Applications/

# The reason this installer exists: clear quarantine before first launch.
xattr -dr com.apple.quarantine "/Applications/$APP_NAME" 2>/dev/null || true

say "Writing configuration…"
mkdir -p "$CONFIG_DIR"
chmod 700 "$CONFIG_DIR"

if [ -z "$LARK_APP_SECRET" ] && [ -f "$ENV_FILE" ]; then
  # Never destroy a secret the user already has.
  LARK_APP_SECRET=$(grep '^OMSN_LARK_APP_SECRET=' "$ENV_FILE" 2>/dev/null | cut -d= -f2- || true)
fi

umask 077
cat > "$ENV_FILE" <<EOF
# Written by the OMSN Desktop installer. Safe to re-run.
OMSN_LARK_APP_ID=$LARK_APP_ID
OMSN_LARK_APP_SECRET=$LARK_APP_SECRET
OMSN_OAUTH_REDIRECT=$OAUTH_REDIRECT
OMSN_BASE_TOKEN=$BASE_TOKEN
OMSN_TABLE_ID=$TABLE_ID
EOF
chmod 600 "$ENV_FILE"

if [ -z "$LARK_APP_SECRET" ]; then
  printf '\033[1;33m!\033[0m %s\n' "No app secret was supplied."
  echo "  Ask Adrian for it, then run:"
  echo "    OMSN_LARK_APP_SECRET=<secret> bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh)\""
  echo "  Sign-in will fail until then, and the app will say so."
fi

say "Done. Launching…"
open "/Applications/$APP_NAME"
echo
echo "  Click SIGN IN WITH LARK. Updates from here are in-app:"
echo "  Settings (⚙) → Check again."
