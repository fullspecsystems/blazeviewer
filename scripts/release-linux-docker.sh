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
#   ./scripts/release-linux-docker.sh                 # x86_64 (default)
#   ./scripts/release-linux-docker.sh arm64           # aarch64
#   ./scripts/release-linux-docker.sh both            # x86_64 then aarch64
#   ./scripts/release-linux-docker.sh both --upload   # build both, then publish to the feed
#   DOCKER=podman ./scripts/release-linux-docker.sh
#
# --upload runs scripts/release-linux-upload.sh AFTER the build(s) succeed — from the host (it
# needs your ssh keys), scp'ing whatever this run produced to downloads.blazeviewer.app.
#
# Output: dist/BlazeViewer-<version>-<arch>.AppImage  (owned by you via the bind mount)
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

ARCH_ARG="amd64"
UPLOAD=0
for a in "$@"; do
  case "$a" in
    --upload) UPLOAD=1 ;;
    amd64|x86_64|arm64|aarch64|both) ARCH_ARG="$a" ;;
    *) echo "usage: $0 [amd64|arm64|both] [--upload]" >&2; exit 1 ;;
  esac
done

case "$ARCH_ARG" in
  amd64|x86_64) ARCHES=(amd64) ;;
  arm64|aarch64) ARCHES=(arm64) ;;
  both) ARCHES=(amd64 arm64) ;;
esac

ENGINE="${DOCKER:-docker}"   # OrbStack provides the `docker` CLI; override for podman/etc.
# Same gate as the Linux CI lane: `command -v` only proves OrbStack's CLI shim is installed,
# not that its VM is up, so a stopped/half-booted OrbStack used to surface as a silent hang
# in the first `docker build` instead of a message here. See docker-preflight.sh.
# shellcheck source=scripts/docker-preflight.sh
source "$ROOT/scripts/docker-preflight.sh"
docker_preflight "$ENGINE" || exit 1

VERSION="$(grep -m1 '^version' crates/pb-app/Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"

build_arch() {
  local arch="$1" platform="linux/$1" image="blazeviewer-appimage-builder:$1"
  echo ">> Blaze Viewer AppImage  ($platform, via $ENGINE)"

  # 1. Builder image — cached; only rebuilds when the Dockerfile / toolchain pin change.
  "$ENGINE" build --platform "$platform" -t "$image" -f scripts/appimage.Dockerfile .

  # 2. Run the standard release script inside the container. A container-only target dir and a
  # cached registry volume keep it off the host's macOS `target/` and make repeat builds fast.
  # APPIMAGE_EXTRACT_AND_RUN avoids FUSE (no --privileged / /dev/fuse needed).
  "$ENGINE" run --rm --platform "$platform" \
    -v "$PWD":/src -w /src \
    -v "blazeviewer-target-$arch:/cargo-target" \
    -v "blazeviewer-cargo-registry:/opt/cargo/registry" \
    -e CARGO_TARGET_DIR=/cargo-target \
    -e APPIMAGE_EXTRACT_AND_RUN=1 \
    "$image" bash scripts/release-linux.sh
}

for arch in "${ARCHES[@]}"; do
  build_arch "$arch"
  triple=x86_64; [ "$arch" = arm64 ] && triple=aarch64
  echo ">> done -> dist/BlazeViewer-$VERSION-$triple.AppImage"
  echo ""
done

# 3. Publish (host-side; needs your ssh keys / YubiKey — runs after the container work).
if [ "$UPLOAD" -eq 1 ]; then
  echo ">> uploading to downloads.blazeviewer.app"
  bash scripts/release-linux-upload.sh
fi
