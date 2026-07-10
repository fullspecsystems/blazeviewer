#!/usr/bin/env bash
# Build the NS1 SwiftUI/AppKit host (ADR-021, macos-native-ui-plan §NS1): the Rust engine
# as a swift-bridge staticlib inside a SwiftPM executable, assembled into a .app.
#
#   1. cargo build -p pb-mac-ffi        (staticlib; build.rs regenerates generated/ glue)
#   2. create-package                   → crates/pb-mac-ffi/PbMacFfi (xcframework Swift pkg)
#   3. swift build --package-path mac   (the SwiftUI executable)
#   4. assemble target/swift-host/<profile>/PhotoBlaze.app
#      (the official Mac app since the 2026-07-02 cutover: name AND bundle id are
#      PhotoBlaze / com.jdlien.PhotoBlaze — the only macOS build since task #70)
#
# Usage:
#   scripts/build-swift-host.sh [--debug|--release] [--run]   (default: --release)
#
# This is the only macOS build now — the old egui/winit "strangler-fig" bundle was retired
# in task #70 (pb-app no longer compiles on macOS; the guard lives in crates/pb-app/build.rs).
# `--run` quits any running PhotoBlaze first: `open` never relaunches a live app
# (LaunchServices activates the existing instance by bundle id — even an installed
# /Applications copy), which otherwise silently tests a stale build.
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

# Version in lockstep with the app crate (crates/pb-app/Cargo.toml).
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
# + the flat icns fallback (CFBundleIconFile=PhotoBlaze). Regenerate via
# scripts/build-macos-icons.sh when the icon changes.
cp packaging/macos/PhotoBlaze.icns packaging/macos/Assets.car "$APP_DIR/Contents/Resources/"

# Embed Sparkle.framework (task #65, macOS auto-update). `swift build` LINKS against Sparkle
# but — a SwiftPM executable has no Xcode "Embed Frameworks" phase — does not copy it into the
# bundle, so we do it here. The executable's load command is `@rpath/Sparkle.framework/...`
# and Package.swift adds `-rpath @executable_path/../Frameworks`, so it resolves once embedded.
# This is MANDATORY: the binary is already linked, so a missing framework is a launch-time dyld
# crash, not a soft absence. release-macos.sh re-signs the framework's nested helpers (Autoupdate,
# Updater.app, XPC services) with the Developer ID before notarization.
SPARKLE_FW="$(find mac/.build/artifacts -type d -name 'Sparkle.framework' -path '*macos-arm64*' 2>/dev/null | head -1)"
[[ -n "$SPARKLE_FW" ]] || SPARKLE_FW="$(find mac/.build/artifacts -type d -name 'Sparkle.framework' 2>/dev/null | head -1)"
[[ -n "$SPARKLE_FW" ]] || { echo "error: Sparkle.framework not found under mac/.build/artifacts (run 'swift package --package-path mac resolve')" >&2; exit 1; }
echo "==> Embedding Sparkle.framework"
mkdir -p "$APP_DIR/Contents/Frameworks"
cp -R "$SPARKLE_FW" "$APP_DIR/Contents/Frameworks/"

echo "==> Done: $APP_DIR"
if [[ "$RUN" == 1 ]]; then
	# `open` will NOT relaunch an already-running app — LaunchServices just activates
	# the live instance (matched by bundle id, so even an installed /Applications copy
	# shadows this fresh build). That's the classic "why am I testing a stale build"
	# trap; quit any running PhotoBlaze first and wait for it to exit.
	if pgrep -qf 'PhotoBlaze.app/Contents/MacOS/PhotoBlaze'; then
		echo "==> Quitting running PhotoBlaze (stale-instance guard)"
		osascript -e 'quit app id "com.jdlien.PhotoBlaze"' >/dev/null 2>&1 || true
		for _ in 1 2 3 4 5 6 7 8 9 10; do
			pgrep -qf 'PhotoBlaze.app/Contents/MacOS/PhotoBlaze' || break
			sleep 0.5
		done
		pkill -f 'PhotoBlaze.app/Contents/MacOS/PhotoBlaze' 2>/dev/null || true
	fi
	open "$APP_DIR"
fi
