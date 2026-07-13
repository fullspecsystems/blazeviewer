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
#   scripts/build-swift-host.sh [--debug|--release] [--run] [--ffvideo|--bundle-ffmpeg]
#   (default: --release)
#
# --ffvideo (task #84, DEV-ONLY): builds the Rust engine with the FFmpeg video backend
# (MKV/WebM/VP8/VP9/AV1 playback + posters via the dual-backend §8 routing) and links the
# HOMEBREW FFmpeg dylibs — the resulting .app runs only on a machine with `brew install
# ffmpeg`. NEVER for a release/DMG (phase 7 bundles FFmpeg properly; release-macos.sh
# must not pass this).
#
# --bundle-ffmpeg (task #84 phase 7): implies --ffvideo, then bundles the pinned LGPL FFmpeg
# (scripts/build-ffmpeg-macos.sh) into Contents/Frameworks so the .app runs with NO Homebrew
# dependency, and audits the closure. Ad-hoc signed here; release-macos.sh NOW passes this for
# the shipping build and re-signs the FFmpeg dylibs inside-out with the Developer ID before
# notarizing (pass release-macos.sh --no-video to opt out).
#
# This is the only macOS build now — the old egui/winit "strangler-fig" bundle was retired
# in task #70 (pb-app no longer compiles on macOS; the guard lives in crates/pb-app/build.rs).
# `--run` quits any running PhotoBlaze first: `open` never relaunches a live app
# (LaunchServices activates the existing instance by bundle id — even an installed
# /Applications copy), which otherwise silently tests a stale build.
set -euo pipefail

PROFILE=release
RUN=0
FFVIDEO="${PB_FFVIDEO:-0}"
BUNDLE_FFMPEG=0
for a in "$@"; do
	case "$a" in
		--debug) PROFILE=debug ;;
		--release) PROFILE=release ;;
		--run) RUN=1 ;;
		--ffvideo) FFVIDEO=1 ;;
		# --bundle-ffmpeg (task #84 phase 7): implies --ffvideo, then bundles the pinned
		# LGPL FFmpeg into Contents/Frameworks so the .app runs with NO Homebrew dependency.
		# Ad-hoc signed here; release-macos.sh passes this for the shipping build and re-signs
		# the FFmpeg dylibs inside-out with the Developer ID before notarizing.
		--bundle-ffmpeg) FFVIDEO=1; BUNDLE_FFMPEG=1 ;;
		*) echo "unknown arg: $a (usage: build-swift-host.sh [--debug|--release] [--run] [--ffvideo] [--bundle-ffmpeg])" >&2; exit 2 ;;
	esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "==> cargo build -p pb-mac-ffi ($PROFILE, aarch64-apple-darwin)"
# Deployment target 14.0 (ADR-021) so the Rust objects match the Swift target's floor
# (otherwise ld warns: object built for newer macOS than being linked). No flag array:
# expanding an EMPTY array under `set -u` is "unbound variable" on the macOS runners'
# bash 3.2 (fixed in bash 4.4) — the CI failure that taught us.
# --ffvideo: the feature flag for cargo, and the Homebrew FFmpeg link line for the Swift
# link (a staticlib carries no link flags; passed at `swift build` time — NOT in
# Package.swift — so no SwiftPM manifest-cache staleness when toggling). No arrays: bash
# 3.2 under `set -u` (see the CI note above); Homebrew paths have no spaces.
FEATURE_ARGS=""
FF_LINK_ARGS=""
if [[ "$FFVIDEO" == "1" ]]; then
	FEATURE_ARGS="--features ffvideo"
	FF_LIBDIR="$(pkg-config --variable=libdir libavcodec 2>/dev/null || true)"
	if [[ -z "$FF_LIBDIR" ]]; then
		echo "--ffvideo needs Homebrew FFmpeg (pkg-config can't find libavcodec)" >&2
		exit 2
	fi
	FF_LINK_ARGS="-Xlinker -L$FF_LIBDIR -Xlinker -lavcodec -Xlinker -lavformat -Xlinker -lavutil -Xlinker -lswscale -Xlinker -lswresample"
	echo "==> ffvideo: linking FFmpeg from $FF_LIBDIR (dev-only build)"
fi

if [[ "$PROFILE" == "release" ]]; then
	MACOSX_DEPLOYMENT_TARGET=14.0 cargo build -p pb-mac-ffi --release --target aarch64-apple-darwin $FEATURE_ARGS
else
	MACOSX_DEPLOYMENT_TARGET=14.0 cargo build -p pb-mac-ffi --target aarch64-apple-darwin $FEATURE_ARGS
fi

echo "==> create-package (swift-bridge glue + staticlib → Swift package)"
cargo run -q -p pb-mac-ffi --features package --bin create-package -- "--$PROFILE"

echo "==> swift build ($PROFILE)"
swift build --package-path mac -c "$PROFILE" $FF_LINK_ARGS
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

# --bundle-ffmpeg: make the .app self-contained (no Homebrew FFmpeg dependency). Builds the
# pinned LGPL FFmpeg on first run, copies its dylibs into Frameworks, rewrites the binary's
# load commands to @rpath, and audits the closure. DEV-ONLY (ad-hoc signed).
if [[ "$BUNDLE_FFMPEG" == "1" ]]; then
	FF_LIBDIR="$REPO_ROOT/third_party/ffmpeg/$(uname -m)/lib"
	if [[ ! -e "$FF_LIBDIR/libavcodec.dylib" ]]; then
		echo "==> Building pinned LGPL FFmpeg (first run; ~10-20 min)"
		"$REPO_ROOT/scripts/build-ffmpeg-macos.sh"
	fi
	echo "==> Bundling FFmpeg into $APP_DIR (self-contained)"
	"$REPO_ROOT/scripts/bundle-ffmpeg-macos.sh" "$APP_DIR" --libdir "$FF_LIBDIR"
fi

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
