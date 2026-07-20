#!/usr/bin/env bash
# Build the NS1 SwiftUI/AppKit host (ADR-021, macos-native-ui-plan §NS1): the Rust engine
# as a swift-bridge staticlib inside a SwiftPM executable, assembled into a .app.
#
#   1. cargo build -p pb-mac-ffi        (staticlib; build.rs regenerates generated/ glue)
#   2. create-package                   → crates/pb-mac-ffi/PbMacFfi (xcframework Swift pkg)
#   3. swift build --package-path mac   (the SwiftUI executable)
#   4. assemble "target/swift-host/<profile>/Blaze Viewer.app"
#      (the official Mac app since the 2026-07-02 cutover; renamed to Blaze Viewer /
#      ca.fullspec.BlazeViewer in task #101 — the only macOS build since task #70)
#
# Usage:
#   scripts/build-swift-host.sh [--debug|--release] [--run] [--ffvideo|--no-ffvideo|--bundle-ffmpeg]
#   (default: --release, and --ffvideo ON — override with --no-ffvideo or PB_FFVIDEO=0)
#
# --ffvideo (task #84, DEV-ONLY): builds the Rust engine with the FFmpeg video backend
# (MKV/WebM/VP8/VP9/AV1 playback + posters via the dual-backend §8 routing) and links the
# HOMEBREW FFmpeg dylibs — the resulting .app runs only on a machine with `brew install
# ffmpeg`. NOW THE DEFAULT for this dev script (a local Homebrew-linked build is fine; the
# license/bundling concern is only about shipping). If Homebrew FFmpeg is missing the default
# falls back to a no-video build; an explicit --ffvideo hard-errors. NEVER used for a
# release/DMG — phase 7 bundles FFmpeg properly (release-macos.sh passes --bundle-ffmpeg,
# not this).
#
# --no-ffvideo: force the video backend OFF (e.g. a quick UI-only build, or no Homebrew FFmpeg).
#
# --bundle-ffmpeg (task #84 phase 7): implies --ffvideo, then bundles the pinned LGPL FFmpeg
# (scripts/build-ffmpeg-macos.sh) into Contents/Frameworks so the .app runs with NO Homebrew
# dependency, and audits the closure. Ad-hoc signed here; release-macos.sh NOW passes this for
# the shipping build and re-signs the FFmpeg dylibs inside-out with the Developer ID before
# notarizing (pass release-macos.sh --no-video to opt out).
#
# This is the only macOS build now — the old egui/winit "strangler-fig" bundle was retired
# in task #70 (pb-app no longer compiles on macOS; the guard lives in crates/pb-app/build.rs).
# `--run` quits any running instance first: `open` never relaunches a live app
# (LaunchServices activates the existing instance by bundle id — even an installed
# /Applications copy), which otherwise silently tests a stale build.
set -euo pipefail

PROFILE=release
RUN=0
# Dev builds default to the FFmpeg video backend (--ffvideo) — a local build linking
# Homebrew FFmpeg is fine; the license/bundling concern is only about SHIPPING (the
# release path uses --bundle-ffmpeg with the pinned LGPL build). Override with
# --no-ffvideo or PB_FFVIDEO=0. When ffvideo is the DEFAULT (not asked for explicitly)
# and Homebrew FFmpeg is missing, we warn and build without video instead of failing;
# an explicit --ffvideo/--bundle-ffmpeg still hard-errors.
FFVIDEO="${PB_FFVIDEO:-1}"
FFVIDEO_EXPLICIT=0
BUNDLE_FFMPEG=0
for a in "$@"; do
	case "$a" in
		--debug) PROFILE=debug ;;
		--release) PROFILE=release ;;
		--run) RUN=1 ;;
		--ffvideo) FFVIDEO=1; FFVIDEO_EXPLICIT=1 ;;
		--no-ffvideo) FFVIDEO=0 ;;
		# --bundle-ffmpeg (task #84 phase 7): implies --ffvideo, then bundles the pinned
		# LGPL FFmpeg into Contents/Frameworks so the .app runs with NO Homebrew dependency.
		# Ad-hoc signed here; release-macos.sh passes this for the shipping build and re-signs
		# the FFmpeg dylibs inside-out with the Developer ID before notarizing.
		--bundle-ffmpeg) FFVIDEO=1; FFVIDEO_EXPLICIT=1; BUNDLE_FFMPEG=1 ;;
		*) echo "unknown arg: $a (usage: build-swift-host.sh [--debug|--release] [--run] [--ffvideo|--no-ffvideo|--bundle-ffmpeg])" >&2; exit 2 ;;
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
	FF_LIBDIR="$(pkg-config --variable=libdir libavcodec 2>/dev/null || true)"
	if [[ -z "$FF_LIBDIR" ]]; then
		if [[ "$FFVIDEO_EXPLICIT" == "1" ]]; then
			echo "--ffvideo needs Homebrew FFmpeg (pkg-config can't find libavcodec)" >&2
			exit 2
		fi
		# ffvideo is the default, not an explicit ask — don't break a machine without
		# Homebrew FFmpeg; build without the video backend and say so.
		echo "==> ffvideo (default) skipped: Homebrew FFmpeg not found — building WITHOUT video." >&2
		echo "    (brew install ffmpeg to enable it, or pass --no-ffvideo to silence this.)" >&2
	else
		FEATURE_ARGS="--features ffvideo"
		FF_LINK_ARGS="-Xlinker -L$FF_LIBDIR -Xlinker -lavcodec -Xlinker -lavformat -Xlinker -lavutil -Xlinker -lswscale -Xlinker -lswresample"
		echo "==> ffvideo: linking FFmpeg from $FF_LIBDIR (dev-only build)"
	fi
fi

if [[ "$PROFILE" == "release" ]]; then
	MACOSX_DEPLOYMENT_TARGET=14.0 cargo build -p pb-mac-ffi --release --target aarch64-apple-darwin $FEATURE_ARGS
else
	MACOSX_DEPLOYMENT_TARGET=14.0 cargo build -p pb-mac-ffi --target aarch64-apple-darwin $FEATURE_ARGS
fi

echo "==> create-package (swift-bridge glue + staticlib → Swift package)"
cargo run -q -p pb-mac-ffi --features package --bin create-package -- "--$PROFILE"

echo "==> swift build ($PROFILE)"
# Homebrew's FFmpeg is built for a much newer macOS than our 14.0 floor (ADR-021), so the
# linker warns once per dylib — five lines, every dev build, telling us only that we are on
# the dev path, which the line above already said. The release build doesn't link Homebrew
# at all (see CLAUDE.md's "Homebrew trap"), so this can never fire on a shipped artifact.
#
# Filtered EXACTLY: the pattern is scoped to `/opt/homebrew`, so a linker warning about
# anything else — including our own code or the bundled LGPL dylibs — still comes through.
# `|| true` keeps grep's "matched nothing" exit code from failing the pipeline under
# `pipefail`; swift build's own status is what PIPESTATUS checks.
swift build --package-path mac -c "$PROFILE" $FF_LINK_ARGS 2>&1 |
	{ grep -v "^ld: warning: building for macOS-.*but linking with dylib '/opt/homebrew/" || true; }
[[ "${PIPESTATUS[0]}" -eq 0 ]] || { echo "error: swift build failed" >&2; exit 1; }
BIN="$(swift build --package-path mac -c "$PROFILE" --show-bin-path)/BlazeViewerMac"
[[ -x "$BIN" ]] || { echo "error: $BIN not found" >&2; exit 1; }

# Version in lockstep with the app crate (crates/pb-app/Cargo.toml).
SHORT_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/pb-app/Cargo.toml | head -1)"
# Build stamp for the About panel (PBBuildID) — same format as pb-app's PB_BUILD_ID
# (build.rs): short commit hash, "-dirty" when the tree has changes. Empty outside git.
BUILD_ID="$(git rev-parse --short HEAD 2>/dev/null || true)"
if [[ -n "$BUILD_ID" ]] && [[ -n "$(git status --porcelain 2>/dev/null)" ]]; then
	BUILD_ID="$BUILD_ID-dirty"
fi
# The shipped bundle + its inner executable. The space is deliberate and matches
# CFBundleName/CFBundleExecutable in Info-swift-host.plist: spaces are the macOS
# convention ("Visual Studio Code.app") and Finder shows the exact brand. Every use of
# these MUST stay quoted. (The SwiftPM product is BlazeViewerMac — an internal
# target name, renamed separately.) The DMG deliberately does NOT take the space — see
# release-macos.sh.
APP_NAME="Blaze Viewer"
APP_DIR="target/swift-host/$PROFILE/$APP_NAME.app"
echo "==> Assembling $APP_DIR (v$SHORT_VERSION${BUILD_ID:+, build $BUILD_ID})"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
sed -e "s/__SHORT_VERSION__/$SHORT_VERSION/g" \
	-e "s/__VERSION__/$SHORT_VERSION/g" \
	-e "s/__BUILD_ID__/$BUILD_ID/g" \
	packaging/macos/Info-swift-host.plist > "$APP_DIR/Contents/Info.plist"
printf 'APPL????' > "$APP_DIR/Contents/PkgInfo"
cp "$BIN" "$APP_DIR/Contents/MacOS/$APP_NAME"
chmod +x "$APP_DIR/Contents/MacOS/$APP_NAME"
# The app icon: the prebuilt Liquid Glass Assets.car (Tahoe+, CFBundleIconName=AppIcon)
# + the flat icns fallback (CFBundleIconFile=BlazeViewer). Regenerate via
# scripts/build-macos-icons.sh when the icon changes.
cp packaging/macos/BlazeViewer.icns packaging/macos/Assets.car "$APP_DIR/Contents/Resources/"

# Bundled-library license texts + the notices summary (task #77). Required, not courtesy:
# LGPL-2.1 §6 ("You must supply a copy of this License") covers the FFmpeg dylibs in
# Contents/Frameworks. Dynamic linking satisfies the *relink* condition, but not this one —
# they are separate clauses, so a compliant linkage does not excuse a missing text.
# Contents/Resources/licenses is the conventional home, and the About panel names it.
mkdir -p "$APP_DIR/Contents/Resources/licenses"
cp licenses/* "$APP_DIR/Contents/Resources/licenses/"
cp THIRD-PARTY-NOTICES.md "$APP_DIR/Contents/Resources/"
# FFmpeg is the one macOS actually links (libheif and dav1d are Windows/Linux — macOS uses
# Image I/O), so that text is the hard requirement here. The rest of the folder rides along:
# over-including a license is harmless, and one copied folder cannot drift out of step.
[ -f "$APP_DIR/Contents/Resources/licenses/ffmpeg-COPYING.LGPLv2.1.txt" ] ||
	{ echo "error: licenses/ffmpeg-COPYING.LGPLv2.1.txt missing from the bundle — LGPL-2.1 §6 requires the license text to ship with the binary." >&2; exit 1; }

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

# Sparkle's license text (task #77). MIT: "The above copyright notice and this permission
# notice shall be included in all copies or substantial portions of the Software" — we ship
# the framework binary, so that notice has to travel with it. Its LICENSE also carries the
# vendored bsdiff (BSD-2-Clause) and sais-lite (MIT) notices, so the verbatim file covers
# all three at once.
#
# Sourced from the resolved artifact rather than a checked-in copy: it sits beside the very
# xcframework being embedded, so it cannot drift from the version that ships when
# Package.resolved moves. That is exactly the staleness licenses/README.md warns about
# ("when a pinned version moves, re-copy these"), avoided by construction. It is also why
# this is NOT in repo licenses/ — that folder is copied to Windows and Linux too, and
# Sparkle is macOS-only.
SPARKLE_LICENSE="$(dirname "$(dirname "$(dirname "$SPARKLE_FW")")")/LICENSE"
[ -f "$SPARKLE_LICENSE" ] ||
	{ echo "error: Sparkle LICENSE not found at $SPARKLE_LICENSE — MIT requires the copyright notice ship with the binary, and Sparkle.framework is embedded. Check the artifact layout after a Sparkle version bump." >&2; exit 1; }
cp "$SPARKLE_LICENSE" "$APP_DIR/Contents/Resources/licenses/sparkle-LICENSE.txt"

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

# Attribution drift guard (task #77). The obligation is PER-ARTIFACT, but it used to be
# enforced only by a unit test in crates/pb-app — the winit shell, which macOS never
# builds. A green test suite therefore "proved" a notice the Mac never showed, and every
# DMG through 0.3.0 shipped without it. So check the thing that actually ships: walk
# Contents/Frameworks and hold every bundled third-party binary to its license's terms.
#
# Two DIFFERENT obligations, deliberately not conflated:
#   LICENSE  — the text must ship (all of them: LGPL-2.1 §6, MIT, BSD-2-Clause).
#   CREDITS  — the name must appear in the About panel. Only copyleft needs this;
#              LGPL binds a work that "displays copyright notices during execution" to
#              carry the Library's among them. MIT/BSD ask for the notice in the
#              distribution, not on screen, so demanding a credits line for Sparkle
#              would be inventing a requirement.
#
# Matching is family-level (libavcodec.62.dylib -> "libav*" -> FFmpeg), so a soname or
# version bump does not trip it. Anything unrecognised hard-fails rather than being
# waved through — that is the case this exists to catch.
CREDITS_SRC="$REPO_ROOT/mac/Sources/BlazeViewerMac/CoreModel.swift"
LIC_DIR="$APP_DIR/Contents/Resources/licenses"
for item in "$APP_DIR/Contents/Frameworks"/*; do
	[ -e "$item" ] || continue
	base="$(basename "$item")"
	case "$base" in
		# name-in-credits | license file that must be present
		libav*|libsw*|libpostproc*) need_credit="FFmpeg";  need_lic="ffmpeg-COPYING.LGPLv2.1.txt" ;;
		libheif*|libde265*)         need_credit="libheif"; need_lic="libheif-COPYING.txt" ;;
		libdav1d*)                  need_credit="";        need_lic="dav1d-COPYING.txt" ;;
		Sparkle.framework)          need_credit="";        need_lic="sparkle-LICENSE.txt" ;;
		# Unknown: fail loudly rather than guess. If it is genuinely notice-free, add it
		# here explicitly with the reason.
		*) echo "error: $base is bundled but this guard does not know its attribution. Add it to the case in $0 with its license file, and name it in aboutPanelOptions() if its license requires an on-screen notice." >&2; exit 1 ;;
	esac
	[ -f "$LIC_DIR/$need_lic" ] ||
		{ echo "error: $base ships in Contents/Frameworks but $need_lic is missing from Contents/Resources/licenses. Its license requires the text to travel with the binary." >&2; exit 1; }
	[ -z "$need_credit" ] || grep -q "$need_credit" "$CREDITS_SRC" ||
		{ echo "error: $base ships in Contents/Frameworks but '$need_credit' is not named in the About panel credits ($CREDITS_SRC). LGPL requires a prominent notice in the work that displays copyright." >&2; exit 1; }
done

echo "==> Done: $APP_DIR"
if [[ "$RUN" == 1 ]]; then
	# `open` will NOT relaunch an already-running app — LaunchServices just activates
	# the live instance (matched by bundle id, so even an installed /Applications copy
	# shadows this fresh build). That's the classic "why am I testing a stale build"
	# trap; quit any running instance first and wait for it to exit.
	#
	# The pattern carries a space ("Blaze Viewer.app/Contents/MacOS/Blaze Viewer") — keep
	# it quoted. pgrep/pkill -f match against the full command line, so the space is data,
	# not a separator.
	RUNNING_PATTERN="$APP_NAME.app/Contents/MacOS/$APP_NAME"
	if pgrep -qf "$RUNNING_PATTERN"; then
		echo "==> Quitting running $APP_NAME (stale-instance guard)"
		osascript -e 'quit app id "ca.fullspec.BlazeViewer"' >/dev/null 2>&1 || true
		for _ in 1 2 3 4 5 6 7 8 9 10; do
			pgrep -qf "$RUNNING_PATTERN" || break
			sleep 0.5
		done
		pkill -f "$RUNNING_PATTERN" 2>/dev/null || true
	fi
	open "$APP_DIR"
fi
