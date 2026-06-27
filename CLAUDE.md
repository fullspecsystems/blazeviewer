# PhotoBlaze — Project Guide

PhotoBlaze is a photo viewer with exactly one obsession: **how fast you can flick
through thousands of images.** No chrome, fit-to-screen, keyboard-driven, with
photos decoded ahead of time and held resident in GPU memory so the next frame is
already there when you press a key.

## Prime directive

> **Every architectural decision is answered by one question: "Will this make it
> faster, or have basically zero performance impact?"** If it's neither, it
> doesn't ship. Speed is the feature.

Corollary: **we do not guess about speed — we measure it.** Performance claims
require numbers from the benchmark corpus. Architecture choices that affect the
hot path are built behind swappable seams and A/B tested (see *Instrumentation*).

## The performance model (read this before optimizing anything)

The naive intuition is "tune the GPU rendering." That's wrong. Drawing one
textured quad is microseconds; the GPU is never the bottleneck for display. The
real wall is **decode throughput**, and the architecture exists to hide it:

1. **Decode-to-fit.** Never decode more pixels than the display shows. On the
   7680×3840 target, a 24 MP JPEG is decoded at a reduced scale (libjpeg-turbo
   DCT scaling 1/2, 1/4, 1/8), often cutting decode several-fold. A major lever
   in general — but **measured inert on this 7680-wide display for ≤24 MP photos**
   (decode-spike: 0/200 triggered scaling), where full-decode throughput dominates
   instead. Encoded in `pb-decode::FitBox`.
2. **Preview-first, then refine.** Show the embedded thumbnail/preview (EXIF,
   HEIC, RAW) instantly, swap in the scaled full decode when ready. Makes fast
   scrubbing feel instant even when full decode lags.
3. **Prefetch ring → resident VRAM.** A direction-biased window of neighbors is
   decoded and uploaded *ahead* of the user into a ring of resident GPU
   textures. **A keypress is a rebind, never a decode or an upload.**
4. **Self-paced advance.** Holding a key advances to the newest *ready* frame
   each vsync — so you fly when decode keeps up and degrade gracefully (to
   previews) when it can't. Capped at the monitor refresh rate.

The metric that matters is **keypress → photon** (input to the pixel actually
scanning out), target ≤ one refresh interval (~8.3 ms @ 120 Hz). Throughput is
capped by refresh: the job is "one fresh frame per vsync, with an instant preview
fallback," not "infinite fps."

## Architecture

```
crates/
  pb-core    pure nav / precomputed-random / prefetch / cache-residency
             — no I/O, no GPU, deterministic, 100% unit-testable
  pb-decode  decode abstraction (decode-to-fit + preview-first) + swappable backends
  pb-render  fit-to-screen geometry now; wgpu presenter (swapchain, ring, draw) later
  pb-app     the binary: winit event loop, decode thread pool, wiring
```

The crate boundaries *are* the A/B seams. Anything whose "is this faster?" answer
is non-obvious goes behind a trait so alternatives can be benchmarked: decode
backend, cache/eviction policy, present mode, upload strategy.

### Threading
- **Event-loop thread (winit):** input, swapchain, draw. Never blocks on I/O or
  decode. On keypress: advance index → rebind resident texture → present.
- **Decode pool:** a dedicated worker pool with **priorities + cancellation**
  (not bare rayon — work-stealing reorders prefetch and there are no priorities).
  Pulls jobs from the prefetch scheduler, decodes-to-fit, hands off for upload.
- **Upload (`UploadStrategy` seam):** v1 uses a **persistent staging-buffer ring**
  (`copy_buffer_to_texture`) — measured ~48 GB/s, 3.4× the 120 Hz budget. Never
  `write_texture` (the trap: 60–75 fps on large frames). Uploads land in the
  resident ring during prefetch — **never on the keypress frame**. A zero-copy CUDA
  alias is the gated escalation behind the same seam.

## Test-Driven Development (required)

- **Write the test first.** Especially for `pb-core` logic (nav, random walk,
  prefetch window, eviction) — it's pure, so there is no excuse.
- **Coverage target: >80%**, measured with **`cargo-llvm-cov`** (Windows-native;
  tarpaulin is Linux-only). The hard-to-test GPU/present shell is marked
  `#[coverage(off)]` so the number stays honest rather than gamed.
- **Property tests** (`proptest`) for nav/cache invariants (e.g. a random cycle
  visits each item exactly once; a residency plan never exceeds capacity).
- **Golden-image tests** for rendering: a headless **wgpu** render reads back to
  a CPU buffer, compared to reference PNGs with a perceptual diff (**nv-flip**) and
  a tolerance. Run in CI on a software adapter (**WARP**/lavapipe, no GPU required).
- **Fuzz** the decoders (`cargo-fuzz`) — they parse hostile bytes.
- Keep logic testable by isolating it from I/O and GPU (the whole point of
  `pb-core`). If something is hard to test, that's usually a design smell.

## Instrumentation & A/B methodology (first-class, not an afterthought)

- **Live profiling:** `tracing` + `tracing-tracy` (Tracy) with **wgpu timestamp
  queries** (+ `wgpu-profiler`) for unified CPU+GPU zones and frame markers. Behind a
  feature; compiles out of release.
- **keypress → photon:** timestamp input via QPC, read true scanout time from
  DXGI `GetFrameStatistics().SyncQPCTime` (flip-model swapchain + waitable object
  + max-frame-latency = 1). Validate against Intel **PresentMon** as an oracle.
  GPU pass split (upload vs draw) via wgpu timestamp queries. macOS later uses
  the same seam with `CAMetalDrawable.presentedTime`.
- **Microbenchmarks:** **Criterion** locally over the pinned corpus (decode from
  memory). **CI regression gate** runs deterministic instruction-count benches
  (**CodSpeed** / iai-callgrind) on *platform-independent* code on Linux — never
  gate on Windows wall-clock noise.
- **A/B pattern:** `Box<dyn Trait>` at the swap seam (decode backend, cache
  policy, present mode), generics in the hot inner loop, cargo features only for
  whole-program/profiler toggles. One runner replays a scripted keypress workload
  per variant and logs **per-frame NDJSON**; report **p50/p95/p99**, never means.

## Minimal UI (the entire user-facing surface)

- Borderless, chrome-less window; borderless-fullscreen at native res (enables
  DXGI Independent Flip for lowest latency).
- Image fit to screen, centered, **never cropped** (`pb-render::fit_rect`).
- Keymap:
  - `space` / `→` — next photo
  - `backspace` / `←` — previous photo
  - `enter` — random photo (precomputed shuffle order; reversible)
  - `esc` — quit
- Hold any nav key to iterate as fast as frames become ready.
- Key handling tracks **held physical keys** (`Pressed`/`Released`), ignoring OS
  key-repeat events, plus a focus-loss release net (avoids known winit repeat/lost
  key-up bugs).

## Cross-platform discipline (Windows now, Apple Silicon later)

Windows 11 is the target, but the spikes + codex review (see `decisions.md`,
post-spike update) put us on the **portable path**: **wgpu is the v1 renderer**
(DX12 backend on Windows, Metal on macOS), because CPU decode (2.5×) and the
staging-ring upload (3.4×) already clear 120 Hz — wgpu's portability costs nothing
measurable here.
- **`winit` owns windowing + input**; the wgpu surface is created on its window
  handle. Rendering/upload/decode sit behind the `Renderer`, `DecodeBackend`, and
  `UploadStrategy` traits.
- **macOS is a cheap port** (wgpu Metal backend + a hardware-decode/upload backend
  swap), not a rewrite — deferred to v2.
- **GPU decode + zero-copy is a gated acceleration backend** (native D3D12 +
  nvImageCodec), pursued only if the high-MP stress test proves a need (ADR-012
  kill criterion). The CPU pool is the permanent baseline and handles formats GPU
  decode can't.
- Do color management **in-shader** (matrix + TRC) so it ports unchanged.
- Isolate every platform-specific call (refresh-rate query, swapchain latency
  hook, photon timestamp) behind a single helper so the eventual port is a small
  surface.

## Current library picks

These are the starting points from the research in `.taskmaster/docs/`. Each is
**provisional and benchmark-justified** — the A/B seams exist precisely so we can
replace any of them with data.

| Concern | Primary | A/B alternative / notes |
|---|---|---|
| JPEG | `turbojpeg` (libjpeg-turbo, **native scaled decode**) | `zune-jpeg` (pure Rust, SIMD; pair with `fast_image_resize`) |
| PNG/APNG | `png` (image-rs) + `zlib-rs` backend (pure Rust, fastest now) | — (no scaled decode exists for PNG) |
| WebP | `libwebp-sys` (`use_scaling` = true downscale-on-decode) | `image-webp` (pure Rust, no SIMD/scaling) |
| AVIF | `dav1d` crate + `avif-parse` | parallelize across images (single-tile ≈ no thread gain) |
| HEIC | `libheif-rs` | ⚠ **highest** Windows build risk — pin vcpkg ports or ship DLLs |
| JXL | `jxl-oxide` (pure Rust) | `jpegxl-rs` only if native DC downscale needed |
| TIFF / BMP / QOI | `tiff` / `image` / `qoi` (all pure Rust) | — |
| SVG | `resvg`/`usvg` → `tiny-skia` pixmap → texture | rasterize at on-screen res; watch `vello_hybrid` for live-zoom |
| RAW | `kamadak-exif` → extract embedded JPEG preview → JPEG path | full demosaic deferred (100×+ cost); `rawler` optional (LGPL) |
| Color | `moxcms` (pure Rust; 3×3 matrix + TRC in-shader) | `lcms2` behind a flag for exotic CLUT/CMYK profiles |
| Windowing | `winit` (window + input; `refresh_rate_millihertz` for the advance cap) | portable; the wgpu surface is created on its window handle |
| GPU API | **wgpu** (DX12 backend on Windows, Metal on macOS), present **Mailbox** | native D3D12 retained as a gated acceleration backend behind `Renderer` |
| GPU decode | **CPU decode pool** (`zune-jpeg`; `turbojpeg` as A/B) — measured 2.5× @ 120 Hz | nvImageCodec/CUDA zero-copy is a gated escalation (ADR-012 kill criterion); benchmark 5090 HW-JPEG first |

### Decode-to-fit value ranking
JPEG ≫ WebP > JXL(C) ≫ everything else. Prioritize the scaled-decode path where
it pays.

### What's actually wired (2026-06-27) — deviations from the provisional table
Multi-codec dispatch is implemented in `pb-decode` behind the `ImageDecoder` seam
(`decode_bytes` sniff-registry + extension routing for the ambiguous ones). The
pragmatic crate choices differ from the table above and are the current baseline:
- **`image` crate** (one dep, `default-features` curated) covers PNG/GIF/BMP/TIFF/
  **WebP**/TGA/QOI/ICO/PNM/HDR/EXR — chosen over the per-format crates (libwebp-sys,
  png+zlib-rs, …) for zero build risk; swap individual formats back behind the seam if
  benchmarks justify it.
- **JXL** `jxl-oxide`; **SVG** `resvg/usvg/tiny-skia` (rasterized at display res).
- **RAW** (ARW/NEF/CR2/DNG/…, `raw.rs`): hybrid. If the embedded preview is full-size
  (long edge ≥ `PREVIEW_FULL_MIN`=4000, e.g. Nikon's ≈7360) use it (fast); else **demosaic
  to true sensor res** via `rawloader` + `imagepipe` (e.g. Sony's 1616 thumbnail → 6048).
  Demosaic runs on a **256 MB-stack thread** — some RAW decoders recurse deep enough to
  overflow the default stack (a Nikon D800 NEF does; it's why the preview path exists for
  full-preview cameras). Slower than preview-only; "preview-first then refine" is the
  future optimization. JPEG-segment-parse finds embedded previews (`jpeg_spans`).
- **AVIF + HEIC**: **Windows WIC** (`wic.rs`, `cfg(windows)`, `windows` crate) using the
  OS codec extensions — NOT dav1d/libheif/vcpkg. First platform-specific decode backend
  (macOS would mirror with ImageIO). Needs the AV1/HEVC/HEIF Store extensions; absent →
  graceful decode error. **JPEG-2000 not added** (no pure-Rust decoder; OpenJPEG/C, rare).
- **Orientation** (subtle, was buggy): `common::read_orientation` scans **all** EXIF IFDs
  (HEIC/RAW put Orientation outside the primary IFD; a PRIMARY-only lookup wrongly read 1).
  WIC **already applies** the container rotation, so the WIC path passes orientation=1 (re-
  applying double-rotated). imagepipe also self-orients (demosaic path → 1). The RAW
  *preview* path applies the container's orientation to the sensor-order preview.
- Every backend returns full-res RGBA8 → shared `common::finalize`/`finalize_oriented`
  (orientation + Lanczos decode-to-fit). Native scaled-decode (JPEG DCT, WebP) still a TODO.
- **Panic-safety**: `decode_bytes`/`decode_image_file` wrap the decoders in `catch_panics`
  (`catch_unwind`) — a third-party decoder panicking on a hostile file becomes a
  `DecodeError`, not an app crash. **Release profile is `panic = "unwind"`** (changed from
  abort) so this works; the GPU present hot path never panics, so unwind tables don't cost
  it. A hard *stack overflow* (some NEFs) is still uncatchable — mitigated by the demosaic
  big-stack thread, not `catch_unwind`.
- **TGA is routed by extension** (`.tga`), not content — Targa has no magic number, so
  `image::guess_format` can't sniff it; `decode_image_file` hands it an explicit format hint.
- **Known v1 limitations** (deliberate): no ICC color management (Display-P3 iPhone HEICs
  render oversaturated — treated as sRGB); HDR/EXR clamped to SDR; CMYK JPEG mis-colored;
  first frame only (GIF/animated-WebP/Live-Photo/multipage-TIFF).

## Build, test, bench

> Rust toolchain required (`rustup`); `rust-toolchain.toml` pins stable + the
> components for coverage.

```sh
cargo test                 # unit + property + golden tests
cargo llvm-cov --workspace # coverage (target >80%)
cargo clippy --all-targets -- -D warnings
cargo fmt --all
cargo run -p pb-app        # the viewer (scaffold today)
cargo bench                # criterion microbenchmarks over the corpus
```

## Working norms

- **Test first.** New `pb-core` logic without a failing-then-passing test is not
  done.
- **No per-frame heap allocations on the hot path.** Pre-allocate pools; reuse.
- **Never block the event loop.** Decode and I/O are always off-thread.
- **Never decode or upload on the keypress frame.** If you're tempted to, the
  prefetch window is wrong — fix that instead.
- **Quarantine platform-specific code** behind the established helper seams.
- When a decision touches the hot path and the answer isn't obvious, **put it
  behind a seam and benchmark both** rather than arguing.


## Project Task Tracking

This project uses [taskmaster](https://github.com/eyaltoledano/claude-task-master) conventions for tracking tasks, with a `.taskmaster/tasks/tasks.json` using the following structure:

### Directory Structure

```
.taskmaster/
├── tasks/
│   └── tasks.json    # Active tasks
├── docs/             # Documentation or notes
└── archive.json      # Completed tasks (optional)
```

### Schema

**Important:** Task and subtask IDs must be numbers, not strings. Task Studio cannot look up individual tasks if IDs are quoted strings like `"25"` instead of `25`.

```json
{
  "master": {
    "tasks": [
      {
        "id": 1,
        "title": "Brief task title",
        "description": "What needs to be done",
        "status": "pending|in-progress|done|review|deferred|cancelled",
        "priority": "high|medium|low",
        "dependencies": [],
        "subtasks": [
          {
            "id": 1,
            "title": "Subtask title",
            "description": "Subtask details",
            "status": "pending"
          }
        ]
      }
    ]
  }
}
```

---
