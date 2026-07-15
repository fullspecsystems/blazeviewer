#!/usr/bin/env bash
# Local end-to-end test of the macOS Sparkle updater (task #65) — prove the SEED build's
# updater actually works BEFORE it ships.
#
# WHY: v0.1.1 is the first Sparkle build, so nothing in the field can auto-update *to* it — but
# if ITS updater is broken, everyone who installs it is stranded (they can't get 0.1.2
# automatically either). This is the cheap proof that detect → download → verify → install works.
#
# HOW: from your real notarized DMG it fabricates an "old" build — same Developer ID identity
# (so Sparkle's same-team check passes), version bumped DOWN (default 0.1.0), the feed repointed
# at a local web server, plus a localhost ATS exception and the auto-install pref off (so the
# update dialog is visible). It serves an EdDSA-signed appcast advertising the real DMG, launches
# the old build, and you hit "Check for Updates…" and watch it install the real version.
#
# Everything runs from a temp dir and is torn down on exit — nothing touches /Applications or the
# production feed.
#
# Usage: scripts/test-sparkle-update.sh <notarized-dmg> [--old-version X.Y.Z] [--port N]
#   e.g. scripts/test-sparkle-update.sh dist/BlazeViewer-0.2.0.dmg
set -euo pipefail

# Must match CFBundleName/CFBundleExecutable in packaging/macos/Info-swift-host.plist and
# APP_NAME in build-swift-host.sh. The space is deliberate (macOS convention) — every use
# below stays quoted.
APP_NAME="Blaze Viewer"
DMG=""
OLD_VERSION="0.1.0"
PORT="8765"
while [[ $# -gt 0 ]]; do
	case "$1" in
		--old-version) OLD_VERSION="$2"; shift 2 ;;
		--port) PORT="$2"; shift 2 ;;
		-*) echo "unknown flag: $1" >&2; exit 2 ;;
		*) DMG="$1"; shift ;;
	esac
done
[[ -n "$DMG" && -f "$DMG" ]] || { echo "usage: test-sparkle-update.sh <notarized-dmg> [--old-version X.Y.Z] [--port N]" >&2; exit 2; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
DMG="$(cd "$(dirname "$DMG")" && pwd)/$(basename "$DMG")" # absolutize before we chdir around

# A Developer ID Application identity is required: the fabricated old build must be signed with
# the SAME identity as the notarized DMG, or Sparkle refuses to install (team mismatch).
IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null | grep 'Developer ID Application' | head -1 | awk '{print $2}')"
[[ -n "$IDENTITY" ]] || { echo "error: no 'Developer ID Application' identity in the keychain (needed to re-sign the old build)" >&2; exit 1; }

WORK="$(mktemp -d)"
FEED="$WORK/feed"          # served over http: appcast.xml + the DMG (the enclosure)
OLD_APP="$WORK/$APP_NAME.app"
MOUNT=""
SERVER_PID=""
cleanup() {
	[[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null || true
	[[ -n "$MOUNT" && -d "$MOUNT" ]] && hdiutil detach "$MOUNT" -quiet 2>/dev/null || true
	rm -rf "$WORK"
}
trap cleanup EXIT

# 1) Pull the .app out of the DMG — it becomes the "old" build we downgrade + run.
echo "==> Mounting $DMG"
MOUNT="$(hdiutil attach -nobrowse -readonly "$DMG" | awk -F'\t' '/\/Volumes\//{print $NF; exit}')"
[[ -n "$MOUNT" && -d "$MOUNT/$APP_NAME.app" ]] || { echo "error: $APP_NAME.app not found in the DMG" >&2; exit 1; }
REAL_VERSION="$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$MOUNT/$APP_NAME.app/Contents/Info.plist")"
echo "    DMG advertises version $REAL_VERSION"
# old must be strictly older, or Sparkle sees no update.
if [[ "$OLD_VERSION" == "$REAL_VERSION" ]] || [[ "$(printf '%s\n%s\n' "$OLD_VERSION" "$REAL_VERSION" | sort -V | tail -1)" == "$OLD_VERSION" ]]; then
	echo "error: --old-version ($OLD_VERSION) must be strictly LOWER than the DMG's version ($REAL_VERSION)" >&2
	exit 1
fi
cp -R "$MOUNT/$APP_NAME.app" "$OLD_APP"
hdiutil detach "$MOUNT" -quiet; MOUNT=""

# 2) Downgrade + repoint the old build's Info.plist, then re-sign (editing the plist breaks the
#    seal). SUAutomaticallyUpdate=false so the update prompt is visible; localhost ATS exception
#    so Sparkle may fetch the http feed.
PL="$OLD_APP/Contents/Info.plist"
echo "==> Fabricating the old build ($OLD_VERSION, feed → http://localhost:$PORT)"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $OLD_VERSION" "$PL"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $OLD_VERSION" "$PL"
/usr/libexec/PlistBuddy -c "Set :SUFeedURL http://localhost:$PORT/appcast.xml" "$PL"
/usr/libexec/PlistBuddy -c "Set :SUAutomaticallyUpdate false" "$PL" 2>/dev/null || \
	/usr/libexec/PlistBuddy -c "Add :SUAutomaticallyUpdate bool false" "$PL"
/usr/libexec/PlistBuddy -c "Add :NSAppTransportSecurity dict" "$PL" 2>/dev/null || true
/usr/libexec/PlistBuddy -c "Add :NSAppTransportSecurity:NSExceptionDomains dict" "$PL" 2>/dev/null || true
/usr/libexec/PlistBuddy -c "Add :NSAppTransportSecurity:NSExceptionDomains:localhost dict" "$PL" 2>/dev/null || true
/usr/libexec/PlistBuddy -c "Add :NSAppTransportSecurity:NSExceptionDomains:localhost:NSExceptionAllowsInsecureHTTPLoads bool true" "$PL" 2>/dev/null || true

# Re-sign inside-out, same order as release-macos.sh (framework helpers first, then the app).
FW="$OLD_APP/Contents/Frameworks/Sparkle.framework"
if [[ -d "$FW" ]]; then
	for nested in \
		"Versions/B/XPCServices/Installer.xpc" \
		"Versions/B/XPCServices/Downloader.xpc" \
		"Versions/B/Autoupdate" \
		"Versions/B/Updater.app"; do
		[[ -e "$FW/$nested" ]] && codesign --force --options runtime --timestamp=none --sign "$IDENTITY" "$FW/$nested"
	done
	codesign --force --options runtime --timestamp=none --sign "$IDENTITY" "$FW"
fi
codesign --force --options runtime --timestamp=none --sign "$IDENTITY" "$OLD_APP/Contents/MacOS/$APP_NAME"
codesign --force --options runtime --timestamp=none --sign "$IDENTITY" "$OLD_APP"
codesign --verify --strict "$OLD_APP" && echo "    old build re-signed OK"
xattr -dr com.apple.quarantine "$OLD_APP" 2>/dev/null || true

# 3) Staging feed: the DMG (enclosure) + an EdDSA-signed appcast pointing at localhost.
mkdir -p "$FEED"
cp "$DMG" "$FEED/"
PB_APPCAST_BASE_URL="http://localhost:$PORT" ./scripts/generate-mac-appcast.sh "$FEED/$(basename "$DMG")" "$REAL_VERSION" "$FEED/appcast.xml"

# 4) Serve the feed over http (Sparkle needs a URL, not a file path).
echo "==> Serving $FEED at http://localhost:$PORT"
( cd "$FEED" && exec python3 -m http.server "$PORT" >/dev/null 2>&1 ) &
SERVER_PID=$!
sleep 1
curl -fsS "http://localhost:$PORT/appcast.xml" >/dev/null || { echo "error: local feed not reachable on port $PORT" >&2; exit 1; }

# 5) Launch the old build (quit any running instance first so LaunchServices doesn't just
#    activate a different instance by bundle id).
osascript -e 'quit app id "ca.fullspec.BlazeViewer"' >/dev/null 2>&1 || true
sleep 1
echo "==> Launching the old build ($OLD_VERSION)"
open -n "$OLD_APP"

cat <<INSTRUCTIONS

────────────────────────────────────────────────────────────────────────────
  Sparkle update test is live.

  In the launched $APP_NAME ($OLD_VERSION):
    1. Menu ▸ $APP_NAME ▸ Check for Updates…
    2. Expect: "A new version of $APP_NAME is available" → version $REAL_VERSION
       (if you see it, the EdDSA signature verified and the feed parsed correctly)
    3. Click Install Update → it downloads, verifies, and installs on quit,
       then relaunches as $REAL_VERSION (check $APP_NAME ▸ About).

  A FAILURE to find/verify the update is exactly what this test exists to catch
  BEFORE the seed ships. Common culprits: SUPublicEDKey mismatch, a broken
  enclosure signature, or the framework helpers not signed.

  Press Ctrl-C here when done — the local server, mounted DMG, and temp files
  are all cleaned up automatically.
────────────────────────────────────────────────────────────────────────────

INSTRUCTIONS

# Hold until the user is done, then the EXIT trap cleans up.
echo "==> Waiting… (Ctrl-C to finish and clean up)"
while kill -0 "$SERVER_PID" 2>/dev/null; do sleep 2; done
