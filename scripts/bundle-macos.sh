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
ICON_BUNDLE="icons/AppIcon.icon"                      # Icon Composer Liquid Glass source (macOS 26+)
LEGACY_PNG="icons/photoblaze-icon-macos-legacy.png"   # flat 1024² artwork (transparent interior)

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

# Icon — two icons, two eras (see Info.plist CFBundleIconName / CFBundleIconFile):
#   • Modern (macOS 26 Tahoe / 27 Golden Gate): the Liquid Glass icon. actool compiles
#     icons/AppIcon.icon into Assets.car, which the system renders with the glass look.
#   • Legacy (pre-26): a flat PhotoBlaze.icns from the legacy PNG. Its "photo" interior is
#     exported transparent (goes dark on a dark Dock), so we flood-fill it white — the
#     intended look — before building the icns. The opaque frame blocks the fill, so the
#     outer corners stay transparent (free-form icon, flame breaks the bounds).

# Modern: Liquid Glass via actool → Assets.car. Best-effort: needs full Xcode 26+ (the
# .icon format is new); on an older toolchain it's skipped and the legacy .icns carries.
if [[ -d "$ICON_BUNDLE" ]] && xcrun --find actool >/dev/null 2>&1; then
	ACTOOL_TMP="$(mktemp -d)"
	if xcrun actool "$ICON_BUNDLE" \
		--compile "$ACTOOL_TMP" \
		--app-icon AppIcon \
		--output-partial-info-plist "$ACTOOL_TMP/partial.plist" \
		--platform macosx \
		--minimum-deployment-target 26.0 \
		--errors --warnings \
		--output-format human-readable-text >/dev/null 2>&1 && [[ -f "$ACTOOL_TMP/Assets.car" ]]; then
		cp "$ACTOOL_TMP/Assets.car" "$APP_DIR/Contents/Resources/Assets.car"
		echo "==> Icon (modern): Assets.car (Liquid Glass) compiled from $ICON_BUNDLE"
	else
		echo "==> Icon (modern): SKIPPED — actool couldn't compile $ICON_BUNDLE (needs Xcode 26+)"
	fi
	rm -rf "$ACTOOL_TMP"
else
	echo "==> Icon (modern): SKIPPED — no 'xcrun actool' or missing $ICON_BUNDLE"
fi

# Legacy: flat PhotoBlaze.icns. A prebuilt packaging/macos/PhotoBlaze.icns wins; else
# build it from the legacy PNG (interior flood-filled white via ImageMagick when present).
ICNS_OUT="$APP_DIR/Contents/Resources/$APP_NAME.icns"
if [[ -f "packaging/macos/$APP_NAME.icns" ]]; then
	cp "packaging/macos/$APP_NAME.icns" "$ICNS_OUT"
	echo "==> Icon (legacy): packaging/macos/$APP_NAME.icns"
elif [[ -f "$LEGACY_PNG" ]] && command -v iconutil >/dev/null 2>&1; then
	WORK="$(mktemp -d)"
	SRC_PNG="$LEGACY_PNG"
	if command -v magick >/dev/null 2>&1; then
		W="$(sips -g pixelWidth  "$LEGACY_PNG" | awk '/pixelWidth/{print $2}')"
		H="$(sips -g pixelHeight "$LEGACY_PNG" | awk '/pixelHeight/{print $2}')"
		# Seed the flood-fill at the interior center (just above the mountains).
		magick "$LEGACY_PNG" -fuzz 15% -fill white \
			-draw "color $(( W / 2 )),$(( H * 53 / 100 )) floodfill" "$WORK/filled.png"
		SRC_PNG="$WORK/filled.png"
		echo "==> Icon (legacy): building $APP_NAME.icns from $LEGACY_PNG (interior filled white)"
	else
		echo "==> Icon (legacy): building $APP_NAME.icns from $LEGACY_PNG"
		echo "    note: ImageMagick not found — interior stays transparent (looks off on a dark Dock); 'brew install imagemagick' for the filled look"
	fi
	ICONSET="$WORK/$APP_NAME.iconset"; mkdir -p "$ICONSET"
	for s in 16 32 64 128 256 512; do
		sips -z "$s" "$s"             "$SRC_PNG" --out "$ICONSET/icon_${s}x${s}.png"     >/dev/null
		sips -z "$((s*2))" "$((s*2))" "$SRC_PNG" --out "$ICONSET/icon_${s}x${s}@2x.png"  >/dev/null
	done
	iconutil -c icns "$ICONSET" -o "$ICNS_OUT"
	rm -rf "$WORK"
else
	echo "==> Icon (legacy): none (no $LEGACY_PNG or no iconutil)"
fi

echo "==> Done: $APP_DIR"
echo "    open \"$APP_DIR\"   # or pass a folder: open -a \"$APP_DIR\" --args <folder>"
