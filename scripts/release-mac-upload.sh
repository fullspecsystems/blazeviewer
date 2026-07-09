#!/usr/bin/env bash
# Publish the macOS PhotoBlaze DMG + Sparkle appcast to the downloads.fullspec.ca feed —
# straight from the Mac, no Windows detour.
#
# This is the Mac-native equivalent of scripts/release-mac-upload.ps1. Because the DMG is built
# on the Mac (scripts/release-macos.sh), scp'ing it to jdlien.com directly from here is one hop
# instead of "copy to the Windows box, then PowerShell scp". The remote mac/ dir is jdlien-owned,
# so no sudo is involved.
#
# It uploads the DMG, its .sha256 sidecar, and appcast.xml (the Sparkle feed, task #65), then
# repoints PhotoBlaze-latest.dmg → the uploaded DMG so /photoblaze/latest/mac serves the newest.
# Idempotent: re-running with the same DMG just re-uploads and re-points to the same target.
#
# Usage:
#   scripts/release-mac-upload.sh                       # newest PhotoBlaze-*.dmg in dist/
#   scripts/release-mac-upload.sh --source <dir>        # from another folder
#   scripts/release-mac-upload.sh --dmg <path-to.dmg>   # a specific DMG
set -euo pipefail

SOURCE="dist"
DMG=""
UPLOAD_HOST="jdlien.com"
REMOTE_DIR="/var/www/downloads.fullspec.ca/photoblaze/mac"
while [[ $# -gt 0 ]]; do
	case "$1" in
		--source) SOURCE="$2"; shift 2 ;;
		--dmg) DMG="$2"; shift 2 ;;
		--host) UPLOAD_HOST="$2"; shift 2 ;;
		--remote-dir) REMOTE_DIR="$2"; shift 2 ;;
		-*) echo "unknown flag: $1" >&2; exit 2 ;;
		*) echo "unexpected arg: $1" >&2; exit 2 ;;
	esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# 1) Locate the DMG: explicit --dmg, else the newest PhotoBlaze-*.dmg in --source (never the alias).
if [[ -n "$DMG" ]]; then
	[[ -f "$DMG" ]] || { echo "error: --dmg not found: $DMG" >&2; exit 1; }
else
	DMG="$(ls -t "$SOURCE"/PhotoBlaze-*.dmg 2>/dev/null | grep -v 'PhotoBlaze-latest.dmg' | head -1 || true)"
	[[ -n "$DMG" ]] || { echo "error: no PhotoBlaze-*.dmg in $SOURCE (pass --dmg <path> or --source <dir>)" >&2; exit 1; }
fi
DMG_DIR="$(cd "$(dirname "$DMG")" && pwd)"
NAME="$(basename "$DMG")"
[[ "$NAME" != "PhotoBlaze-latest.dmg" ]] || { echo "error: refusing to publish the 'latest' alias; need a versioned DMG" >&2; exit 1; }
echo "==> DMG: $DMG_DIR/$NAME"

# 2) Assemble the upload set: the DMG, its .sha256 sidecar, and the Sparkle appcast (each if present).
FILES=("$NAME")
if [[ -f "$DMG_DIR/$NAME.sha256" ]]; then FILES+=("$NAME.sha256"); else echo "    (no $NAME.sha256 sidecar; uploading the DMG only)"; fi
if [[ -f "$DMG_DIR/appcast.xml" ]]; then FILES+=("appcast.xml"); else echo "    (no appcast.xml alongside the DMG; macOS auto-update won't see this release)"; fi

# 3) scp the file(s). cd into the source so scp uses bare names.
echo "==> scp ${FILES[*]} -> ${UPLOAD_HOST}:${REMOTE_DIR}/"
( cd "$DMG_DIR" && scp "${FILES[@]}" "${UPLOAD_HOST}:${REMOTE_DIR}/" )

# 4) Repoint the permanent "latest" symlink (relative target so it stays valid within the dir).
echo "==> repoint PhotoBlaze-latest.dmg -> $NAME"
ssh -o BatchMode=yes "$UPLOAD_HOST" "cd '$REMOTE_DIR' && ln -sfn '$NAME' PhotoBlaze-latest.dmg && ls -l PhotoBlaze-latest.dmg"

# 5) Verify the permanent URLs now serve this build. /photoblaze/latest/mac is a 302 → the DMG,
#    so follow redirects (-L) and check the final status.
LATEST_CODE="$(curl -sL -o /dev/null -w '%{http_code}' -I https://downloads.fullspec.ca/photoblaze/latest/mac || true)"
[[ "$LATEST_CODE" == "200" ]] || { echo "error: latest/mac returned HTTP $LATEST_CODE (after redirects)" >&2; exit 1; }
echo "==> Live:"
echo "    https://downloads.fullspec.ca/photoblaze/latest/mac   (HTTP 200)"
echo "    https://downloads.fullspec.ca/photoblaze/mac/$NAME"
if [[ -f "$DMG_DIR/appcast.xml" ]]; then
	echo "    https://downloads.fullspec.ca/photoblaze/mac/appcast.xml  (Sparkle feed)"
fi
