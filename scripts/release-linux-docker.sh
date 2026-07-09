#!/usr/bin/env bash
#
# Build the Linux **AppImage** inside a container — so you can produce it from a Mac (or
# Windows) with no Linux VM, and pick the target CPU arch.
#
# On Apple Silicon + OrbStack (or Docker Desktop), `linux/amd64` runs under Rosetta, so the
# **x86_64** AppImage most Linux users want builds at near-native speed. Pass `arm64` for the
# aarch64 build instead. The container is Ubuntu 26.04 (scripts/appimage.Dockerfile) to match
# the FFmpeg 8 / libheif 1.21 the code targets; inside it, this runs the normal
# scripts/release-linux.sh.
#
# Usage:
#   ./scripts/release-linux-docker.sh            # x86_64 (default)
#   ./scripts/release-linux-docker.sh arm64      # aarch64
#   DOCKER=podman ./scripts/release-linux-docker.sh
#
# Output: dist/PhotoBlaze-<version>-<arch>.AppImage  (owned by you via the bind mount)
set -euo pipefail

cd "$(dirname "$0")/.."

ARCH="${1:-amd64}"
case "$ARCH" in
  amd64|x86_64) ARCH=amd64; TRIPLE_ARCH=x86_64 ;;
  arm64|aarch64) ARCH=arm64; TRIPLE_ARCH=aarch64 ;;
  *) echo "usage: $0 [amd64|arm64]" >&2; exit 1 ;;
esac
PLATFORM="linux/$ARCH"
IMAGE="photoblaze-appimage-builder:$ARCH"
ENGINE="${DOCKER:-docker}"   # OrbStack provides the `docker` CLI; override for podman/etc.

if ! command -v "$ENGINE" >/dev/null 2>&1; then
  echo "!! '$ENGINE' not found. Start OrbStack (or set DOCKER=<engine>)." >&2
  exit 1
fi

echo ">> PhotoBlaze AppImage  ($PLATFORM, via $ENGINE)"

# 1. Builder image — cached; only rebuilds when the Dockerfile / toolchain pin change.
"$ENGINE" build --platform "$PLATFORM" -t "$IMAGE" -f scripts/appimage.Dockerfile .

# 2. Run the standard release script inside the container. A container-only target dir and a
# cached registry volume keep it off the host's macOS `target/` and make repeat builds fast.
# APPIMAGE_EXTRACT_AND_RUN avoids FUSE (no --privileged / /dev/fuse needed).
"$ENGINE" run --rm --platform "$PLATFORM" \
  -v "$PWD":/src -w /src \
  -v "photoblaze-target-$ARCH:/cargo-target" \
  -v "photoblaze-cargo-registry:/opt/cargo/registry" \
  -e CARGO_TARGET_DIR=/cargo-target \
  -e APPIMAGE_EXTRACT_AND_RUN=1 \
  "$IMAGE" bash scripts/release-linux.sh

echo ""
echo ">> done -> dist/PhotoBlaze-$(grep -m1 '^version' crates/pb-app/Cargo.toml | sed -E 's/.*"(.*)".*/\1/')-$TRIPLE_ARCH.AppImage"
