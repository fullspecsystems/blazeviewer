# Reproducible Linux AppImage builder for PhotoBlaze.
#
# Base = Ubuntu 26.04 to MATCH the dev distro: PhotoBlaze's `livephoto` backend targets
# FFmpeg 8 (libavcodec 62) via `ffmpeg-next 8.1`, and libheif 1.21 — both bleeding-edge, so
# an older LTS image (24.04 ships FFmpeg 6 / libavcodec 60) would fail to link. glibc here is
# 2.43, which becomes the AppImage's runtime floor: it runs on similarly-recent distros.
# Lowering that floor would mean building FFmpeg/libheif from source on an older base — a
# future enhancement, not needed to ship.
#
# Built/run by scripts/release-linux-docker.sh. Build context is the repo root (so the pinned
# rust-toolchain.toml can pre-warm the toolchain into the image).
FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive

# Build toolchain + the same -dev packages the dev VM uses (winit/rfd/muda GTK chain, FFmpeg,
# libheif + its de265/aom decode plugins so the bundle step can vendor them).
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential curl ca-certificates pkg-config git file \
      clang libclang-dev \
      libgtk-3-dev libxdo-dev \
      libxkbcommon-dev libwayland-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
      libavcodec-dev libavformat-dev libswscale-dev libswresample-dev \
      libheif-dev libheif-plugin-libde265 libde265-0 libheif-plugin-aomdec \
    && rm -rf /var/lib/apt/lists/*

# Rust via rustup, into a system prefix. The exact toolchain + components come from
# rust-toolchain.toml — the ONE source of truth shared with dev + CI (task #71): install rustup
# with NO default toolchain, COPY just the pin file, then `rustup show` installs the exact
# channel + components it names, pre-warmed into the image. Changing the pin invalidates this
# layer and reinstalls; the runtime bind-mount of the repo carries the same file, so the
# container never re-downloads the toolchain on `docker run`.
ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:$PATH
WORKDIR /src
COPY rust-toolchain.toml ./rust-toolchain.toml
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none \
    && rustup show \
    && rustc --version \
    && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME"
