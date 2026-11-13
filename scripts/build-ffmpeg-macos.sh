#!/usr/bin/env bash
# Build a pinned, **LGPL-clean, decode-only** FFmpeg for bundling into the macOS
# the shipped .app (task #84 plan §9-dist / §7 distribution; task #77 compliance).
#
# WHY a custom build (not Homebrew): Homebrew's FFmpeg is `--enable-gpl` (x264/x265
# and GPL filters) and **not redistributable**. `build-swift-host.sh --ffvideo`
# links it for DEV only; a shippable .app needs a self-contained FFmpeg whose
# license we can honor. This build is:
#   * LGPL v2.1+ — no `--enable-gpl`, no `--enable-nonfree` (FFmpeg's default).
#     We only ever DECODE, so the GPL-only encoders/filters are irrelevant.
#   * decode-focused — encoders, muxers, programs, avfilter, avdevice, postproc,
#     and network are all disabled (smaller, and narrows the license surface).
#     Every DECODER / DEMUXER / PARSER stays enabled, so format coverage matches
#     the Homebrew build the code is validated against — no accidentally-dropped
#     container. (A tighter `--disable-everything` allowlist is a future size win;
#     it trades coverage risk we can't fully retest here, so it's deferred.)
#   * VideoToolbox-enabled — the phase-6 hardware decode path (`ffmpeg/hw.rs`).
#   * `@rpath`-installed — `--install-name-dir=@rpath` stamps each dylib's id and
#     inter-lib deps as `@rpath/lib….dylib`, so `bundle-ffmpeg-macos.sh` only has
#     to point the app binary at them (the .app already carries an
#     `@executable_path/../Frameworks` rpath for Sparkle).
#
# The pin is FFmpeg 8.1.x (soname majors libavcodec.62, libavformat.62,
# libavutil.60, libswscale.9, libswresample.6). `ffmpeg-sys-next` 9.1 binds to
# whichever FFmpeg pkg-config finds (it supports 3.4 through 9.x, keyed by version
# cfgs), and `build-swift-host.sh --bundle-ffmpeg` points pkg-config at THIS pin —
# so headers and bundled dylibs always come from the same build. A patch bump
# (8.1.x → 8.1.y) is just FF_VERSION + FF_SHA256 below; moving to 9.x is a soname
# major bump and needs a full cross-platform retest.
#
# Rebuild-on-bump: the install writes $PREFIX/.ffmpeg-version, and
# build-swift-host.sh rebuilds (--clean) whenever that stamp differs from
# FF_VERSION here — so a bumped pin can never ship stale dylibs from an old build.
#
# Output: third_party/ffmpeg/<arch>/{lib,include} (git-ignored, reproducible).
# Prereqs: `brew install nasm`, Xcode CLT. Run time ~10–20 min (first build).
#
# Usage: scripts/build-ffmpeg-macos.sh [--arch arm64|x86_64] [--clean]
set -euo pipefail

FF_VERSION="8.1.3"
# sha256 of ffmpeg-${FF_VERSION}.tar.xz from https://ffmpeg.org/releases/ — computed from
# the official HTTPS download and pinned here (a mismatch aborts, catching a tampered/wrong
# file). 8.1.3's tarball was also checked against its detached PGP signature
# (ffmpeg-8.1.3.tar.xz.asc): good signature from the FFmpeg release signing key
# FCF9 86EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8. Repeat that check on every re-pin:
#   export GNUPGHOME=$(mktemp -d); curl -fsSL https://ffmpeg.org/ffmpeg-devel.asc | gpg --import
#   gpg --verify ffmpeg-X.tar.xz.asc ffmpeg-X.tar.xz   # "Good signature" + that fingerprint
FF_SHA256="7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3"

ARCH="$(uname -m)"   # arm64 on Apple Silicon; override with --arch
CLEAN=0
while [[ $# -gt 0 ]]; do
	case "$1" in
		--arch) ARCH="$2"; shift 2 ;;
		--clean) CLEAN=1; shift ;;
		*) echo "unknown arg: $1 (usage: build-ffmpeg-macos.sh [--arch arm64|x86_64] [--clean])" >&2; exit 2 ;;
	esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ROOT="$REPO_ROOT/third_party/ffmpeg"
SRC_DIR="$BUILD_ROOT/src/ffmpeg-$FF_VERSION"
PREFIX="$BUILD_ROOT/$ARCH"
TARBALL="$BUILD_ROOT/src/ffmpeg-$FF_VERSION.tar.xz"
MIN_MACOS=14.0

command -v nasm >/dev/null || { echo "error: nasm not found — run 'brew install nasm'" >&2; exit 1; }

if [[ $CLEAN == 1 ]]; then
	echo "==> Cleaning $PREFIX and the source tree"
	rm -rf "$PREFIX" "$SRC_DIR"
fi

mkdir -p "$BUILD_ROOT/src"

# 1) Fetch + verify the pinned source.
if [[ ! -f "$TARBALL" ]]; then
	echo "==> Downloading ffmpeg-$FF_VERSION.tar.xz"
	curl -fSL --retry 3 -o "$TARBALL" "https://ffmpeg.org/releases/ffmpeg-$FF_VERSION.tar.xz"
fi
GOT="$(shasum -a 256 "$TARBALL" | awk '{print $1}')"
if [[ "$GOT" != "$FF_SHA256" ]]; then
	echo "error: sha256 mismatch for the FFmpeg tarball" >&2
	echo "  expected $FF_SHA256" >&2
	echo "  got      $GOT" >&2
	echo "  (delete $TARBALL to re-download, or update FF_SHA256 if intentionally re-pinning)" >&2
	exit 1
fi

if [[ ! -d "$SRC_DIR" ]]; then
	echo "==> Extracting to $SRC_DIR"
	mkdir -p "$SRC_DIR"
	tar -xf "$TARBALL" -C "$BUILD_ROOT/src"
fi

# 2) Configure — LGPL, decode-only, VideoToolbox, @rpath install names.
cd "$SRC_DIR"
echo "==> Configuring FFmpeg $FF_VERSION ($ARCH, LGPL decode-only, VideoToolbox)"
./configure \
	--prefix="$PREFIX" \
	--arch="$ARCH" \
	--cc=clang \
	--enable-shared --disable-static \
	--enable-pic \
	--disable-programs --disable-doc \
	--disable-encoders --disable-muxers \
	--disable-avdevice --disable-avfilter \
	--disable-network \
	--disable-debug \
	--disable-xlib --disable-libxcb --disable-sdl2 \
	--enable-videotoolbox \
	--install-name-dir='@rpath' \
	--extra-cflags="-mmacosx-version-min=$MIN_MACOS" \
	--extra-ldflags="-mmacosx-version-min=$MIN_MACOS" \
	>/dev/null

# 3) Assert LGPL (belt-and-suspenders: no --enable-gpl / --enable-nonfree leaked in).
if grep -qE '^(CONFIG_GPL|CONFIG_NONFREE)=yes' config.mak ffbuild/config.mak 2>/dev/null; then
	echo "error: build is GPL/nonfree — not redistributable. Aborting." >&2
	exit 1
fi
echo "==> Confirmed LGPL (no GPL/nonfree components)"

# 4) Build + install.
echo "==> Building (make -j)"
make -j"$(sysctl -n hw.ncpu)" >/dev/null
make install >/dev/null

# 5) Self-verify: no dylib may pull a non-system, non-sibling dependency (a stray
# Homebrew lib like libX11 from an auto-detected external — the exact bug the
# --disable-xlib/libxcb/sdl2 flags above prevent). System (/usr/lib, /System) and
# @rpath siblings are fine; anything else means the bundle wouldn't be self-contained.
echo "==> Verifying no external (non-system) dependencies leaked in"
STRAY=0
for f in "$PREFIX"/lib/libav*.dylib "$PREFIX"/lib/libsw*.dylib; do
	[[ -f "$f" && ! -L "$f" ]] || continue
	while IFS= read -r dep; do
		case "$dep" in
			@rpath/*|/usr/lib/*|/System/Library/*) ;;
			*) echo "  ✗ $(basename "$f") -> $dep" >&2; STRAY=1 ;;
		esac
	done < <(otool -L "$f" | tail -n +2 | awk '{print $1}')
done
if [[ $STRAY == 1 ]]; then
	echo "error: FFmpeg linked an external library — disable it in configure above." >&2
	exit 1
fi
echo "==> Clean: every dependency is a system lib or an @rpath sibling"

# 6) Stamp the install with the pinned version, written LAST so an interrupted build
# leaves no stamp and build-swift-host.sh retries it.
echo "$FF_VERSION" >"$PREFIX/.ffmpeg-version"

echo
echo "==> Installed to $PREFIX"
ls -1 "$PREFIX/lib"/*.dylib 2>/dev/null | sed "s#$PREFIX/lib/#  #"
echo
echo "==> dylib ids (should be @rpath/…):"
for f in "$PREFIX"/lib/libav*.dylib "$PREFIX"/lib/libsw*.dylib; do
	[[ -f "$f" ]] || continue
	otool -D "$f" | tail -1 | sed 's/^/  /'
done
echo
echo "Next: scripts/bundle-ffmpeg-macos.sh <"Blaze Viewer.app"> --libdir $PREFIX/lib"
