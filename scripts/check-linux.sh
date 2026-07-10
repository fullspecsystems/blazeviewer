#!/usr/bin/env bash
# The **Linux gate** (task #71): clippy `-D warnings` + tests + a release build over the whole
# workspace, run inside the Ubuntu 26.04 appimage container — the only environment with the
# FFmpeg 8 / libheif 1.21 the `livephoto`+`libheif` build links (a stock `ubuntu-latest` runner
# does NOT have them, which is why this can't be a plain hosted job).
#
# Reusable: run it locally on a Mac/Windows via OrbStack/Docker, or from the self-hosted CI
# runner (same image, same command → the same result). `--locked` everywhere, so a drifted
# Cargo.lock fails the gate. The container installs the EXACT toolchain from rust-toolchain.toml
# (the Dockerfile reads it), and the first step asserts `rustc --version` matches the pin.
#
# This is the gate that would have caught the Linux-only dead code + new-compiler lints fixed in
# f71c0720 — CI never lints pb-app (or the full graph) on Linux otherwise.
#
# Usage:  scripts/check-linux.sh [amd64|arm64]   (default amd64 — runs under Rosetta on Apple Silicon)
#         DOCKER=podman scripts/check-linux.sh
set -euo pipefail
cd "$(dirname "$0")/.."

ARCH="${1:-amd64}"
case "$ARCH" in
  amd64|x86_64) ARCH=amd64 ;;
  arm64|aarch64) ARCH=arm64 ;;
  *) echo "usage: $0 [amd64|arm64]" >&2; exit 1 ;;
esac
PLATFORM="linux/$ARCH"
IMAGE="photoblaze-appimage-builder:$ARCH"
ENGINE="${DOCKER:-docker}" # OrbStack provides the `docker` CLI; override for podman/etc.

command -v "$ENGINE" >/dev/null 2>&1 || {
  echo "!! '$ENGINE' not found. Start OrbStack (or set DOCKER=<engine>)." >&2
  exit 1
}

echo ">> Linux gate  ($PLATFORM, via $ENGINE)"

# Builder image — cached; rebuilds only when the Dockerfile or the rust-toolchain.toml pin change
# (the Dockerfile COPYs the pin file, so a bump invalidates the rust layer).
"$ENGINE" build --platform "$PLATFORM" -t "$IMAGE" -f scripts/appimage.Dockerfile .

# The full lint surface: every crate's OWN clippy runs (compiling a crate as a mere dependency does
# not run its lints — pb-decode + pb-ui both had fixes in f71c0720). Features are package-qualified
# to pb-app so the non-livephoto crates (pb-core, pb-ui, …) don't get asked for a feature they lack;
# pb-app/livephoto + pb-app/libheif still pull the livephoto/libheif paths through pb-app-core/pb-decode.
CLIPPY_PKGS="-p pb-app -p pb-app-core -p pb-core -p pb-decode -p pb-source -p pb-render -p pb-hud -p pb-ui"
TEST_PKGS="-p pb-app -p pb-app-core -p pb-core -p pb-decode"
FEATURES="pb-app/livephoto,pb-app/libheif"

# A container-only target dir (cached volume) keeps it off the host's macOS `target/`; the shared
# cargo registry volume is read-mostly. APPIMAGE_EXTRACT_AND_RUN isn't needed (no packaging here).
"$ENGINE" run --rm --platform "$PLATFORM" \
  -v "$PWD":/src -w /src \
  -v "photoblaze-target-$ARCH:/cargo-target" \
  -v "photoblaze-cargo-registry:/opt/cargo/registry" \
  -e CARGO_TARGET_DIR=/cargo-target \
  "$IMAGE" bash -euo pipefail -c "
    echo '== toolchain (must match rust-toolchain.toml) =='
    rustc --version
    echo '== clippy -D warnings (full workspace) =='
    cargo clippy --locked --all-targets $CLIPPY_PKGS --features $FEATURES -- -D warnings
    echo '== tests =='
    cargo test --locked $TEST_PKGS --features $FEATURES
    echo '== release build =='
    cargo build --release --locked -p pb-app --features livephoto,libheif
  "

echo ""
echo ">> Linux gate: PASS"
