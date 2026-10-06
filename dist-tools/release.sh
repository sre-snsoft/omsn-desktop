#!/bin/bash
#
# Build, sign and publish a release of OMSN Desktop.
#
#   ./dist-tools/release.sh            # build + publish the current version
#   ./dist-tools/release.sh --dry-run  # build and assemble, publish nothing
#
# Builds both Mac architectures. The team is not all on Apple Silicon, and an
# Intel user hitting "no build for your Mac" is a worse first impression than
# the extra few minutes this costs.
set -euo pipefail

RELEASE_REPO="sre-snsoft/omsn-desktop-releases"
KEY_PATH="$HOME/.config/omsn/updater.key"
DRY_RUN=false
[ "${1:-}" = "--dry-run" ] && DRY_RUN=true

cd "$(dirname "$0")/.."
ROOT="$PWD"

say()  { printf '\033[1;35m▸\033[0m %s\n' "$1"; }
fail() { printf '\033[1;31m✗\033[0m %s\n' "$1" >&2; exit 1; }

[ -f "$KEY_PATH" ] || fail "No signing key at $KEY_PATH. Without it the updater cannot verify anything."

# The version lives in three files. They must agree, or the app reports one
# number while the manifest advertises another.
VERSION=$(python3 - <<'PY'
import json, re, sys
pk = json.load(open('package.json'))['version']
tc = json.load(open('src-tauri/tauri.conf.json'))['version']
cg = re.search(r'^version = "([^"]+)"', open('src-tauri/Cargo.toml').read(), re.M).group(1)
if not pk == tc == cg:
    sys.exit(f"version mismatch: package.json={pk} tauri.conf={tc} Cargo.toml={cg}")
print(pk)
PY
) || fail "$VERSION"
say "Releasing v$VERSION"

if gh release view "v$VERSION" --repo "$RELEASE_REPO" >/dev/null 2>&1; then
  fail "v$VERSION is already published. Bump the version first."
fi

export TAURI_SIGNING_PRIVATE_KEY="$(cat "$KEY_PATH")"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"

# bundle_dmg.sh cannot create an image while a volume of the same name is
# attached, and fails with an opaque "failed to run bundle_dmg.sh". A test
# install leaves exactly such a mount behind.
for vol in /Volumes/OMSN*; do
  [ -d "$vol" ] || continue
  say "Detaching stale volume $vol"
  hdiutil detach "$vol" -force -quiet 2>/dev/null || fail "Could not detach $vol; eject it and retry."
done

STAGE="$(mktemp -d)"
LOGS="$(mktemp -d)"
trap 'rm -rf "$STAGE" "$LOGS"' EXIT

# target triple : updater platform key : asset arch in the bundle filename
TARGETS=(
  "aarch64-apple-darwin:darwin-aarch64:aarch64"
  "x86_64-apple-darwin:darwin-x86_64:x64"
)

PLATFORMS_JSON="{}"
for entry in "${TARGETS[@]}"; do
  IFS=: read -r triple platform arch <<< "$entry"
  say "Building ${triple}…"
  # Logged rather than discarded: a build failure with no output is a dead
  # end, and the first version of this script hid a real one.
  LOG="$LOGS/build-$arch.log"
  if ! npm run tauri build -- --target "$triple" >"$LOG" 2>&1; then
    echo "--- last 25 lines of $LOG ---" >&2
    tail -25 "$LOG" >&2
    fail "Build failed for $triple."
  fi

  BUNDLE="$ROOT/src-tauri/target/$triple/release/bundle"
  DMG=$(ls "$BUNDLE"/dmg/*_"$arch".dmg 2>/dev/null | head -1)
  TGZ="$BUNDLE/macos/OMSN Desktop.app.tar.gz"
  [ -n "$DMG" ] || fail "No .dmg produced for $arch."
  [ -f "$TGZ.sig" ] || fail "No signature produced for $arch — is the signing key set?"

  cp "$DMG" "$STAGE/OMSN.Desktop_${VERSION}_${arch}.dmg"
  cp "$TGZ" "$STAGE/OMSN.Desktop_${arch}.app.tar.gz"

  SIG=$(cat "$TGZ.sig")
  PLATFORMS_JSON=$(SIG="$SIG" PLATFORM="$platform" ARCH="$arch" VERSION="$VERSION" \
    REPO="$RELEASE_REPO" PREV="$PLATFORMS_JSON" python3 - <<'PY'
import json, os
prev = json.loads(os.environ["PREV"])
prev[os.environ["PLATFORM"]] = {
    "signature": os.environ["SIG"].strip(),
    # GitHub substitutes dots for spaces, so the asset is staged pre-dotted.
    "url": f"https://github.com/{os.environ['REPO']}/releases/download/"
           f"v{os.environ['VERSION']}/OMSN.Desktop_{os.environ['ARCH']}.app.tar.gz",
}
print(json.dumps(prev))
PY
)
  say "  built $(basename "$DMG")"
done

VERSION="$VERSION" PLATFORMS="$PLATFORMS_JSON" python3 - > "$STAGE/latest.json" <<'PY'
import json, os, datetime
print(json.dumps({
    "version": os.environ["VERSION"],
    "notes": f"OMSN Desktop v{os.environ['VERSION']}",
    "pub_date": datetime.datetime.now(datetime.timezone.utc)
                 .isoformat().replace("+00:00", "Z"),
    "platforms": json.loads(os.environ["PLATFORMS"]),
}, indent=2))
PY

say "Assembled:"
ls -la "$STAGE" | awk 'NR>1 {printf "    %s  %s\n", $5, $9}'

if $DRY_RUN; then
  say "--dry-run: nothing published. Staged in $STAGE (removed on exit)."
  exit 0
fi

say "Publishing v${VERSION}…"
gh release create "v$VERSION" "$STAGE"/* \
  --repo "$RELEASE_REPO" \
  --title "v$VERSION" \
  --notes "Install:

\`\`\`bash
OMSN_LARK_APP_SECRET=<secret> bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/$RELEASE_REPO/main/install.sh)\"
\`\`\`

Already installed? Settings (⚙) → Check again."

say "Done: https://github.com/$RELEASE_REPO/releases/tag/v$VERSION"
