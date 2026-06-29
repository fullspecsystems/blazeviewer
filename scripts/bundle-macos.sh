#!/usr/bin/env bash
# Assemble PhotoBlaze.app from the built binary + packaging/macos/Info.plist.
#
# Tool-agnostic by design: it just lays out the standard bundle tree, so any
# notarization workflow can consume the result (codesign → notarize → staple → DMG
# is task #11). No cargo-bundle dependency — full control over Info.plist for the
# file-association UTIs that land in task #6.
#
# Usage:
#   scripts/bundle-macos.sh [--debug|--release]   (default: --release)
#
# Output: target/<profile>/bundle/PhotoBlaze.app
set -euo pipefail

PROFILE="release"
[[ "${1:-}" == "--debug" ]] && PROFILE="debug"
[[ "${1:-}" == "--release" ]] && PROFILE="release"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

APP_NAME="PhotoBlaze"
BIN_NAME="photoblaze"
PLIST_SRC="packaging/macos/Info.plist"
ICON_SRC="photoblaze-icon.png"          # 1024×1024 master; task #7 ships the squircle .icns

# Version from the pb-app crate (keep the bundle in lockstep with the build).
SHORT_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/pb-app/Cargo.toml | head -1)"
VERSION="$SHORT_VERSION"

echo "==> Building $BIN_NAME ($PROFILE)"
if [[ "$PROFILE" == "release" ]]; then
	cargo build -p pb-app --release
else
	cargo build -p pb-app
fi
BIN_PATH="target/$PROFILE/$BIN_NAME"
[[ -x "$BIN_PATH" ]] || { echo "error: $BIN_PATH not found" >&2; exit 1; }

APP_DIR="target/$PROFILE/bundle/$APP_NAME.app"
echo "==> Assembling $APP_DIR (v$SHORT_VERSION)"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"

# Info.plist with the version placeholders filled in.
sed -e "s/__SHORT_VERSION__/$SHORT_VERSION/g" \
	-e "s/__VERSION__/$VERSION/g" \
	"$PLIST_SRC" > "$APP_DIR/Contents/Info.plist"

# PkgInfo (classic, harmless, expected by some tooling).
printf 'APPL????' > "$APP_DIR/Contents/PkgInfo"

# Executable.
cp "$BIN_PATH" "$APP_DIR/Contents/MacOS/$BIN_NAME"
chmod +x "$APP_DIR/Contents/MacOS/$BIN_NAME"

# Icon: prefer a real .icns if present (task #7); otherwise generate a placeholder
# from the 1024 master so the bundle isn't generic during development.
ICNS_OUT="$APP_DIR/Contents/Resources/$APP_NAME.icns"
if [[ -f "packaging/macos/$APP_NAME.icns" ]]; then
	cp "packaging/macos/$APP_NAME.icns" "$ICNS_OUT"
	echo "==> Icon: packaging/macos/$APP_NAME.icns"
elif [[ -f "$ICON_SRC" ]] && command -v iconutil >/dev/null 2>&1; then
	echo "==> Icon: generating placeholder .icns from $ICON_SRC (task #7 = real squircle)"
	ICONSET="$(mktemp -d)/$APP_NAME.iconset"
	mkdir -p "$ICONSET"
	for s in 16 32 64 128 256 512; do
		sips -z "$s" "$s"       "$ICON_SRC" --out "$ICONSET/icon_${s}x${s}.png"       >/dev/null
		sips -z "$((s*2))" "$((s*2))" "$ICON_SRC" --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
	done
	iconutil -c icns "$ICONSET" -o "$ICNS_OUT"
	rm -rf "$(dirname "$ICONSET")"
else
	echo "==> Icon: none (no .icns and no iconutil) — bundle will use the generic icon"
fi

echo "==> Done: $APP_DIR"
echo "    open \"$APP_DIR\"   # or pass a folder: open -a \"$APP_DIR\" --args <folder>"
