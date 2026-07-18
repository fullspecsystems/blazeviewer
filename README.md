# Blaze Viewer

<!-- TODO: Come up with a wordmark and show it here (and in the about screen, and the website -->

Blaze Viewer lets you blaze through images as fast as your monitor refreshes. It is a flexible viewer for images and videos with a central, underlying obsession: everything should feel as fast as possible and the app should never get in your way.

- You almost never wait for images to load
- Everything is controlled with customizable keyboard shortcuts
- Virtually all modern image and video formats just work — even Apple Live Photos and HEIC files
- You can browse nested folders or archives (zip/7z/tar/rar) without extracting them. Encrypted archives are supported.

For a feature or change to ship, it has to pass a simple, evidence-based test:
_will this make it faster, or have near-zero performance impact?_

If not, it doesn't ship.

## About This Project

Blaze Viewer started in July 2025 as a proof of concept: lots of photo viewers feel slow and make you wait for images to load. But what if you just want to _blaze_ through a large photo library at maximum speed? How could one build the fastest photo viewer possible?

Blaze Viewer was born to answer that question, and make viewing photos fast and effortless, yet keep all the flexibility and features you expect from a modern photo viewer at your fingertips.

## How It's Made

Blaze Viewer is written in Rust and renders through [wgpu](https://github.com/gfx-rs/wgpu) (Direct3D 12 on Windows, Metal on macOS, Vulkan on Linux), so the core is cross-platform from the ground up. The image hot path is hand-written wgpu on every platform; the surrounding chrome — settings, dialogs, etc. — uses [egui](https://github.com/emilk/egui) on Windows and Linux, while macOS wraps the same Rust engine in a native SwiftUI shell that follows modern macOS conventions.

The speed comes from the architecture, not from micro-tuning the GPU. Images are decoded no larger than the screen shows, an embedded preview is shown instantly and swapped for the full decode when ready, and a direction-biased ring of neighbours is decoded and uploaded into resident GPU textures _ahead_ of you. By the time you press a key, the next frame is already there. Everything feels instant.

The codebase is a Cargo workspace split into crates:

| Crate         | Responsibility                                                                          |
| ------------- | --------------------------------------------------------------------------------------- |
| `pb-core`     | Navigation, prefetch, and cache-residency logic. Fully unit-tested                      |
| `pb-decode`   | Multi-codec decode behind a trait (decode-to-fit, preview-first, swappable backends)    |
| `pb-source`   | The item-source seam: encoded bytes for items over a folder or an archive.              |
| `pb-render`   | wgpu presentation and fit-to-screen geometry                                            |
| `pb-ui`       | The egui design system (tokens + components) powering the chrome                        |
| `pb-app-core` | Platform-neutral orchestration: actions, keymap, timing — no windowing or GPU           |
| `pb-app`      | The winit shell binary (Windows/Linux); macOS drives the same engine via a SwiftUI host |

Video support covers modern codecs — using the OS decoders where they suffice and [FFmpeg](https://github.com/FFmpeg/FFmpeg) where they don't — with customizable subtitle support built in.

## Download & Install

Grab a build for your platform from [blazeviewer.app/download](https://blazeviewer.app/download). Windows, macOS, and Linux each auto-update in place, so you only download once.

- **macOS** (Apple silicon, macOS 14+) — [download](https://downloads.blazeviewer.app/latest/mac)
- **Windows** (64-bit, Windows 10/11) — [download](https://downloads.blazeviewer.app/latest/windows) · [ARM64](https://downloads.blazeviewer.app/latest/windows-arm64) · [portable zip](https://downloads.blazeviewer.app/win/BlazeViewer-win-Portable.zip)
- **Linux** (AppImage) — [x86_64](https://downloads.blazeviewer.app/latest/linux) · [aarch64](https://downloads.blazeviewer.app/latest/linux-arm64)

## Building from Source

You'll need the Rust toolchain via [rustup](https://rustup.rs); the exact version is pinned in `rust-toolchain.toml` and installed automatically on first build.

```sh
git clone https://github.com/fullspecsystems/blazeviewer.git
cd blazeviewer
```

**Linux** — a plain cargo run works; add features for HEIC and full video:

```sh
cargo run -p pb-app --release --features livephoto,pb-decode/libheif
```

**Windows** — use the build script, not a bare `cargo run`. It enters the VS Developer shell FFmpeg needs and builds with the ship feature set (`libheif,dav1d,ffprobe`):

```powershell
pwsh scripts/build-windows.ps1 -Run          # add -Release for an optimized build
pwsh scripts/build-windows.ps1 -NoNative -Run # skip the native libs (no vcpkg needed)
```

**macOS** — `pb-app` doesn't build here; the Mac app is the SwiftUI host over the Rust engine:

```sh
scripts/build-swift-host.sh --debug --run   # dev video needs `brew install ffmpeg`
```

Common workspace commands:

```sh
cargo test                              # unit, property, and golden-image tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all
cargo bench                             # Criterion microbenchmarks over the corpus
cargo run -p pb-ui --example gallery    # preview the design system (light/dark/both)
```
