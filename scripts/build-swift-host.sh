#!/usr/bin/env bash
# Build the NS1 SwiftUI/AppKit host (ADR-021, macos-native-ui-plan §NS1): the Rust engine
# as a swift-bridge staticlib inside a SwiftPM executable, assembled into a .app.
#
#   1. cargo build -p pb-mac-ffi        (staticlib; build.rs regenerates generated/ glue)
#   2. create-package                   → crates/pb-mac-ffi/PbMacFfi (xcframework Swift pkg)
#   3. swift build --package-path mac   (the SwiftUI executable)
#   4. assemble target/swift-host/<profile>/PhotoBlaze.app
#      (the official Mac app since the 2026-07-02 cutover: name AND bundle id are
#      PhotoBlaze / com.jdlien.PhotoBlaze — it replaces the egui beta on macOS)
#
# Usage:
#   scripts/build-swift-host.sh [--debug|--release] [--run]   (default: --release)
#
# This is the strangler-fig target: the shippable egui-on-Mac beta
# (scripts/bundle-macos.sh) is untouched and never gated on this build.
# Reminder: `open` won't relaunch an app that's already running — quit it first (⌘Q).
set -euo pipefail

PROFILE=release
RUN=0
for a in "$@"; do
	case "$a" in
		--debug) PROFILE=debug ;;
		--release) PROFILE=release ;;
		--run) RUN=1 ;;
		*) echo "unknown arg: $a (usage: build-swift-host.sh [--debug|--release] [--run])" >&2; exit 2 ;;
	esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> cargo build -p pb-mac-ffi ($PROFILE, aarch64-apple-darwin)"
# Deployment target 14.0 (ADR-021) so the Rust objects match the Swift target's floor
# (otherwise ld warns: object built for newer macOS than being linked). No flag array:
# expanding an EMPTY array under `set -u` is "unbound variable" on the macOS runners'
# bash 3.2 (fixed in bash 4.4) — the CI failure that taught us.
if [[ "$PROFILE" == "release" ]]; then
	MACOSX_DEPLOYMENT_TARGET=14.0 cargo build -p pb-mac-ffi --release --target aarch64-apple-darwin
else
	MACOSX_DEPLOYMENT_TARGET=14.0 cargo build -p pb-mac-ffi --target aarch64-apple-darwin
fi

echo "==> create-package (swift-bridge glue + staticlib → Swift package)"
cargo run -q -p pb-mac-ffi --features package --bin create-package -- "--$PROFILE"

echo "==> swift build ($PROFILE)"
swift build --package-path mac -c "$PROFILE"
BIN="$(swift build --package-path mac -c "$PROFILE" --show-bin-path)/PhotoBlazeMac"
[[ -x "$BIN" ]] || { echo "error: $BIN not found" >&2; exit 1; }

# Version in lockstep with the app crate, like bundle-macos.sh.
SHORT_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/pb-app/Cargo.toml | head -1)"
# Build stamp for the About panel (PBBuildID) — same format as pb-app's PB_BUILD_ID
# (build.rs): short commit hash, "-dirty" when the tree has changes. Empty outside git.
BUILD_ID="$(git rev-parse --short HEAD 2>/dev/null || true)"
if [[ -n "$BUILD_ID" ]] && [[ -n "$(git status --porcelain 2>/dev/null)" ]]; then
	BUILD_ID="$BUILD_ID-dirty"
fi
APP_DIR="target/swift-host/$PROFILE/PhotoBlaze.app"
echo "==> Assembling $APP_DIR (v$SHORT_VERSION${BUILD_ID:+, build $BUILD_ID})"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
sed -e "s/__SHORT_VERSION__/$SHORT_VERSION/g" \
	-e "s/__VERSION__/$SHORT_VERSION/g" \
	-e "s/__BUILD_ID__/$BUILD_ID/g" \
	packaging/macos/Info-swift-host.plist > "$APP_DIR/Contents/Info.plist"
printf 'APPL????' > "$APP_DIR/Contents/PkgInfo"
cp "$BIN" "$APP_DIR/Contents/MacOS/PhotoBlaze"
chmod +x "$APP_DIR/Contents/MacOS/PhotoBlaze"
# The app icon: the prebuilt Liquid Glass Assets.car (Tahoe+, CFBundleIconName=AppIcon)
# + the flat icns fallback (CFBundleIconFile=PhotoBlaze) — the same assets the egui
# bundle ships. Regenerate via scripts/build-macos-icons.sh when the icon changes.
cp packaging/macos/PhotoBlaze.icns packaging/macos/Assets.car "$APP_DIR/Contents/Resources/"

echo "==> Done: $APP_DIR"
if [[ "$RUN" == 1 ]]; then
	open "$APP_DIR"
fi
