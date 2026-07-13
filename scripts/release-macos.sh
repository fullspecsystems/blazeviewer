#!/usr/bin/env bash
# Sign, package (DMG), notarize, and staple PhotoBlaze.app for distribution.
#
# Pipeline: build the SwiftUI host .app (scripts/build-swift-host.sh) → codesign with the Developer ID
# Application cert under the hardened runtime → hdiutil DMG (drag-to-/Applications) →
# codesign the DMG → notarytool submit --wait → stapler staple → SHA256 sidecar.
# Output: dist/PhotoBlaze-<version>.dmg (+ .sha256).
#
# Gated like the Windows job: each stage skips cleanly when its inputs are absent, so a
# fork or a local dry-run still produces an (unsigned) DMG.
#
# Credentials (env / GitHub secrets — never hardcode; notarytool reads them at run time):
#   CSC_LINK                     base64 of the Developer ID Application .p12  (codesign)
#   CSC_KEY_PASSWORD             that .p12's password
#   APPLE_ID                     Apple ID e-mail                              (notarize)
#   APPLE_APP_SPECIFIC_PASSWORD  app-specific password
#   APPLE_TEAM_ID                Developer Team ID
# Locally you can skip CSC_LINK: if a "Developer ID Application" identity is already in
# your login keychain it's used directly (no base64 dance).
#
# FFmpeg video (task #84): the release build ships the cross-platform FFmpeg backend by
# default — build-swift-host.sh --bundle-ffmpeg links the ffvideo feature and bundles the
# pinned LGPL FFmpeg dylibs (scripts/build-ffmpeg-macos.sh) into Contents/Frameworks. The
# signing block below re-signs those dylibs with the Developer ID inside-out (before the
# app binary), so hardened-runtime library validation + notarization accept them. Pass
# --no-video for a video-less DMG (skips the FFmpeg build/bundle entirely).
#
# Usage: scripts/release-macos.sh [--release|--debug] [--no-video]
#   Builds the SwiftUI host (PhotoBlaze.app via build-swift-host.sh) — it IS PhotoBlaze on
#   macOS since the 2026-07-02 cutover (the old egui/winit bundle was retired in task #70).
#   Runs fine LOCALLY with a Developer ID identity in the login keychain + the three APPLE_*
#   env vars — no CI required (Actions credits are finite).
set -euo pipefail

PROFILE="release"
BUNDLE_VIDEO=1   # ship the FFmpeg video backend by default (task #84); --no-video opts out
for a in "$@"; do
	case "$a" in
		--debug) PROFILE="debug" ;;
		--release) PROFILE="release" ;;
		--no-video) BUNDLE_VIDEO=0 ;;
		--swift-host) ;; # accepted for compat; the SwiftUI host is the only target now
		*) echo "unknown arg: $a (usage: release-macos.sh [--release|--debug] [--no-video])" >&2; exit 2 ;;
	esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

DIST="dist"
SHORT_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/pb-app/Cargo.toml | head -1)"
# The SwiftUI host is PhotoBlaze on macOS (the egui/winit bundle was retired in task #70).
APP_NAME="PhotoBlaze"
BIN_NAME="PhotoBlaze"
APP="target/swift-host/$PROFILE/$APP_NAME.app"
BUILD_CMD="./scripts/build-swift-host.sh --$PROFILE"
if [[ $BUNDLE_VIDEO == 1 ]]; then
	# --bundle-ffmpeg implies --ffvideo, then bundles the pinned LGPL FFmpeg into
	# Contents/Frameworks (building it on first run, ~10-20 min, if third_party/ffmpeg is
	# absent). nasm is that build's one non-Xcode prereq — fail loud rather than silently
	# ship a video-less DMG.
	command -v nasm >/dev/null || {
		echo "error: shipping video needs nasm for the FFmpeg build ('brew install nasm')." >&2
		echo "       Pass --no-video to build a DMG without the video backend." >&2
		exit 1
	}
	BUILD_CMD="$BUILD_CMD --bundle-ffmpeg"
else
	# A video-less DMG must NOT link Homebrew FFmpeg (build-swift-host.sh now defaults
	# --ffvideo ON for dev convenience; a shipped DMG can't depend on Homebrew dylibs).
	BUILD_CMD="$BUILD_CMD --no-ffvideo"
fi
DMG="$DIST/$APP_NAME-$SHORT_VERSION.dmg"

# Local credentials, two ways (CI keeps using repo-secret env vars):
#   a) PREFERRED — a notarytool keychain profile (secrets never touch disk). One-time:
#        xcrun notarytool store-credentials photoblaze-notary \
#          --apple-id you@example.com --team-id TEAMID   (prompts for the app password)
#      The script auto-uses it when the APPLE_* env vars are absent.
#   b) .env.release at the repo root (gitignored; see .env.release.example) — sourced
#      here so `./scripts/release-macos.sh --swift-host` just works.
[[ -f .env.release ]] && source .env.release

: "${CSC_LINK:=}"; : "${CSC_KEY_PASSWORD:=}"
: "${APPLE_ID:=}"; : "${APPLE_APP_SPECIFIC_PASSWORD:=}"; : "${APPLE_TEAM_ID:=}"
: "${NOTARY_PROFILE:=photoblaze-notary}"

TMP="$(mktemp -d)"
KEYCHAIN=""
cleanup() {
	[[ -n "$KEYCHAIN" && -f "$KEYCHAIN" ]] && security delete-keychain "$KEYCHAIN" 2>/dev/null || true
	rm -rf "$TMP"
}
trap cleanup EXIT

# The hash of the Developer ID Application identity in keychain $1 (default: search list).
find_identity() {
	security find-identity -v -p codesigning ${1:+"$1"} 2>/dev/null \
		| grep "Developer ID Application" | head -1 | awk '{print $2}'
}

# 1) Always build fresh — a stale $APP dir from a prior run must never be silently
#    re-signed/re-packaged (build-swift-host.sh already rm -rf's its own output).
$BUILD_CMD
[[ -d "$APP" ]] || { echo "error: $APP not found" >&2; exit 1; }
# An interrupted codesign leaves a .cstemp beside the binary; a later BUNDLE sign then
# fails on it ("invalid or unsupported format ... In subcomponent: *.cstemp"). Sweep.
find "$APP" -name "*.cstemp" -delete

mkdir -p "$DIST"
rm -f "$DMG" "$DMG.sha256"

# 2) Code-sign (hardened runtime). Import the .p12 into a throwaway keychain in CI; fall
#    back to an identity already in the login keychain when running locally.
IDENTITY=""
if [[ -n "$CSC_LINK" ]]; then
	echo "==> Importing Developer ID certificate into a temporary keychain"
	KEYCHAIN="${RUNNER_TEMP:-$TMP}/pb-signing.keychain-db"
	KPW="pb-$(date +%s)-$$"
	security create-keychain -p "$KPW" "$KEYCHAIN"
	security set-keychain-settings -lut 21600 "$KEYCHAIN"
	security unlock-keychain -p "$KPW" "$KEYCHAIN"
	printf '%s' "$CSC_LINK" | base64 --decode > "$TMP/cert.p12"
	security import "$TMP/cert.p12" -k "$KEYCHAIN" -P "$CSC_KEY_PASSWORD" \
		-T /usr/bin/codesign -T /usr/bin/security
	security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KPW" "$KEYCHAIN" >/dev/null
	# Put our keychain at the front of the search list so codesign resolves the identity.
	security list-keychains -d user -s "$KEYCHAIN" $(security list-keychains -d user | sed 's/["[:space:]]//g')
	IDENTITY="$(find_identity "$KEYCHAIN")"
else
	IDENTITY="$(find_identity)"
	[[ -n "$IDENTITY" ]] && echo "==> Using Developer ID identity from the login keychain"
fi

SIGNED=0
if [[ -n "$IDENTITY" ]]; then
	echo "==> Signing $APP ($IDENTITY)"
	# Sparkle.framework (task #65): re-sign its nested helpers with OUR Developer ID +
	# hardened runtime FIRST — inside-out, before the app binary/bundle. The framework ships
	# signed by the Sparkle project (no Team ID), which notarization rejects; each nested
	# code item (the two XPC services, the Autoupdate tool, the Updater.app helper) must carry
	# our signature. Order + item list per Sparkle's distribution docs. Guarded on existence so
	# a build without Sparkle bundled (e.g. an unsigned dry-run) skips cleanly.
	FW="$APP/Contents/Frameworks/Sparkle.framework"
	if [[ -d "$FW" ]]; then
		echo "==> Signing Sparkle.framework helpers"
		for nested in \
			"Versions/B/XPCServices/Installer.xpc" \
			"Versions/B/XPCServices/Downloader.xpc" \
			"Versions/B/Autoupdate" \
			"Versions/B/Updater.app"; do
			[[ -e "$FW/$nested" ]] && \
				codesign --force --options runtime --timestamp --sign "$IDENTITY" "$FW/$nested"
		done
		codesign --force --options runtime --timestamp --sign "$IDENTITY" "$FW"
	fi

	# Bundled FFmpeg dylibs (task #84): re-sign the pinned LGPL FFmpeg with OUR Developer ID +
	# hardened runtime, inside-out (BEFORE the app binary), exactly like Sparkle's helpers.
	# build-swift-host.sh --bundle-ffmpeg copied them into Contents/Frameworks and ad-hoc-signed
	# them; hardened-runtime library validation then requires OUR Team ID on every loaded dylib,
	# and notarization requires a secure timestamp — hence --options runtime --timestamp here.
	# The glob stays literal when nothing matches (a --no-video build), so `-f` skips it cleanly.
	ff_signed=0
	for dylib in "$APP/Contents/Frameworks"/libav*.dylib "$APP/Contents/Frameworks"/libsw*.dylib; do
		[[ -f "$dylib" ]] || continue
		[[ $ff_signed == 0 ]] && echo "==> Signing bundled FFmpeg dylibs (inside-out)" && ff_signed=1
		codesign --force --options runtime --timestamp --sign "$IDENTITY" "$dylib"
	done

	# Sign the executable, then the bundle (inside-out; the app holds only the one binary
	# plus non-code resources — Assets.car / .icns — Sparkle.framework, and the FFmpeg dylibs,
	# all signed above).
	# No `--entitlements`: a non-sandboxed Rust app needs no hardened-runtime exceptions, so
	# `--options runtime` alone is correct (an empty entitlements file is a no-op and AMFI's
	# XML parser is fussy about it).
	codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP/Contents/MacOS/$BIN_NAME"
	codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP"
	codesign --verify --deep --strict --verbose=2 "$APP"
	SIGNED=1
else
	echo "==> Code signing SKIPPED — no Developer ID identity (set CSC_LINK / CSC_KEY_PASSWORD)"
fi

# 3) Build the DMG (app + /Applications drop target), then sign the DMG too.
echo "==> Building $DMG"
STAGING="$TMP/dmg"
mkdir -p "$STAGING"
cp -R "$APP" "$STAGING/"
ln -s /Applications "$STAGING/Applications"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGING" -ov -format UDZO "$DMG" >/dev/null
[[ $SIGNED == 1 ]] && codesign --force --timestamp --sign "$IDENTITY" "$DMG"

# 4) Notarize + staple (a signed DMG + credentials: the APPLE_* env vars, or the
#    stored keychain profile for local runs).
NOTARIZED=0
if [[ $SIGNED == 1 ]]; then
	if [[ -n "$APPLE_ID" && -n "$APPLE_APP_SPECIFIC_PASSWORD" && -n "$APPLE_TEAM_ID" ]]; then
		echo "==> Notarizing $DMG (env credentials; --wait can take a few minutes)"
		xcrun notarytool submit "$DMG" \
			--apple-id "$APPLE_ID" \
			--password "$APPLE_APP_SPECIFIC_PASSWORD" \
			--team-id "$APPLE_TEAM_ID" \
			--wait
		NOTARIZED=1
	elif xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
		echo "==> Notarizing $DMG (keychain profile '$NOTARY_PROFILE'; --wait can take a few minutes)"
		xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
		NOTARIZED=1
	fi
fi
if [[ $NOTARIZED == 1 ]]; then
	xcrun stapler staple "$DMG"
	xcrun stapler validate "$DMG"
	# Gatekeeper assessment of the stapled DMG (informational).
	spctl -a -t open --context context:primary-signature -vv "$DMG" 2>&1 || true
	echo "==> Notarized + stapled."
else
	echo "==> Notarization SKIPPED (need a signed app + APPLE_* env vars, a .env.release,"
	echo "    or a stored profile: xcrun notarytool store-credentials $NOTARY_PROFILE ...)"
fi

# 5) Checksum sidecar (integrity independent of the signature).
( cd "$DIST" && shasum -a 256 "$(basename "$DMG")" | tee "$(basename "$DMG").sha256" )

# 6) Sparkle appcast (task #65): EdDSA-sign the notarized DMG and write dist/appcast.xml, which
#    release-mac-upload.ps1 publishes next to the DMG. Only on a notarized build (a dev/unsigned
#    DMG is never shipped). Hard-fails on error: a release DMG without a valid appcast is broken
#    and should stop the run.
if [[ $NOTARIZED == 1 ]]; then
	echo "==> Generating Sparkle appcast"
	./scripts/generate-mac-appcast.sh "$DMG" "$SHORT_VERSION"
else
	echo "==> Sparkle appcast SKIPPED (needs the notarized swift-host DMG)"
fi

echo "==> Done: $DMG"
[[ $SIGNED == 1 ]] || echo "    (UNSIGNED — for distribution, set the signing/notarization secrets)"
