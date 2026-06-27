# PhotoBlaze — Architecture

> Prime directive: *will this make it faster, or have basically zero performance
> impact?* This document describes a system designed around that question.

## 1. The reframe that drives everything

Displaying one image is a textured quad — microseconds on any modern GPU. The GPU
is **not** the bottleneck for showing a photo. The wall is **decode throughput**,
and almost the entire architecture exists to hide decode latency behind the user.

Back-of-envelope: a 24 MP JPEG is ~80 ms to full-decode on one libjpeg-turbo
thread → ~12/s/core → ~190/s across 16 cores. You cannot full-decode large images
at 120 Hz. Two techniques collapse the wall:

- **Decode-to-fit:** decode at the smallest scale that still covers the on-screen
  size. A 24 MP JPEG fit to 7680×3840 at 1/2 scale is ~1/4 the work. (JPEG/WebP
  support this natively; see the decode tiers below.)
  > **Measured caveat (decode-spike, 2026-06-26):** on the *7680-wide* display,
  > power-of-2 DCT scaling almost never triggers for ≤24 MP photos (0/200 in the
  > spike) — the on-screen size is too large to halve without undershooting. So on
  > this display+library the lever is largely inert and **full-decode throughput is
  > what matters**. turbojpeg's 1/8-granular scaling would recover some of it.
  > Decode-to-fit remains a strong lever on smaller displays / higher-MP sources.
- **Preview-first:** display the embedded thumbnail (sub-millisecond) instantly,
  then refine to the scaled decode.

With both, plus a prefetch ring, sustaining "one fresh frame per refresh" is
realistic — which is all a 120 Hz display can show anyway.

## 2. Latency budget

The metric is **keypress → photon**: from input event to the pixel scanning out.
Target: ≤ one refresh interval (~8.3 ms @ 120 Hz) for a cache hit.

```
keypress ─► sample input ─► advance index ─► rebind resident texture ─► encode draw ─► present ─► scanout
            (top of loop)    (pb-core, ns)    (cache hit, no upload)     (µs)          (flip)     (photon)
```

The entire steady-state hot path is a **cache lookup + rebind + draw**. There is
no decode and no upload on this path — those happened earlier, during prefetch.
If a keypress ever triggers a decode/upload, the prefetch window was wrong.

## 3. Components & threads

```
                    ┌─────────────────────────────────────────────┐
  keyboard ───────► │  Event loop (winit) — never blocks           │
                    │  • track held physical keys, self-paced      │
                    │  • advance to newest READY frame each vsync   │
                    │  • rebind resident texture, encode draw       │ ─► present (Mailbox / flip + waitable)
                    └───┬──────────────────────────────▲───────────┘
              index,    │ current ± direction            │ "frame N resident" (texture id)
              direction ▼                                │
                    ┌──────────────────────┐             │
                    │ Prefetch scheduler    │             │
                    │ pb-core: targets +    │             │
                    │ residency plan        │             │
                    └───┬──────────────────┘             │
              decode    │ prioritized jobs (+cancel)      │ upload done
              requests  ▼                                 │
   ┌──────────────┐  ┌──────────────────────┐   ┌─────────┴──────────┐
   │ RAM byte LRU │◄─►│ Decode pool          │──►│ Upload stage        │
   │ (compressed) │  │ priority queue,       │   │ persistent-mapped   │
   │ mmap/readahead│  │ decode-to-fit,        │   │ buffer pool ->      │
   └──────────────┘  │ preview-first         │   │ copy_buffer_to_tex  │
                     └──────────────────────┘   └─────────┬──────────┘
                                                          ▼
                                                ┌────────────────────┐
                                                │ Resident texture    │
                                                │ ring (VRAM)         │
                                                │ + thumbnail atlas    │
                                                └────────────────────┘
```

- **Event loop (1 thread):** owns the window + swapchain. Input, advance, draw,
  present. Tracks held physical keys (ignores OS key-repeat) and advances to the
  newest ready frame each vsync, capped at refresh rate.
- **Prefetch scheduler:** pure `pb-core`. From `(current, direction)` computes a
  prioritized target window (`prefetch_targets`) and a load/evict plan
  (`plan_residency`) against the VRAM budget. Issues decode jobs; cancels jobs
  that fall outside the window (direction reversal).
- **Decode pool (N≈physical cores):** priority queue with cancellation — *not*
  bare rayon (work-stealing reorders prefetch and offers no priorities). Reads
  bytes (from the RAM LRU or mmap), decodes-to-fit, emits previews first. The
  on-screen image **preempts the queue** and cancels now-stale speculative
  decodes (qimgv's proven highest-felt-speed trick).
- **Upload stage (`UploadStrategy` seam):** v1 uploads through a **persistent
  staging-buffer ring** (`copy_buffer_to_texture`) — measured ~48 GB/s, 3.4× the
  120 Hz budget (ADR-017). Never `write_texture` (the trap: ~60–75 fps on large
  frames). Uploads happen only during prefetch. A zero-copy alias from CUDA memory
  is the escalation path (§7), behind the same seam.
- **Resident ring (VRAM):** fixed pool of pre-allocated screen-fit textures keyed
  by item index; the keypress path just rebinds one. Plus a small thumbnail atlas
  for instant previews.

## 4. Memory tiers & VRAM budget

| Tier | Holds | Sizing |
|---|---|---|
| Disk / page cache | encoded bytes | mmap + readahead |
| RAM byte LRU | encoded bytes (a few GB) | makes reverse/revisit instant — no disk re-read |
| RAM decoded | scaled decodes for the active window only | full decodes are large; don't hoard |
| VRAM ring | screen-fit textures + thumbnail atlas | see math below |

VRAM math on the 32 GB target (from `research/gpu-decode-pipeline.md`):
- 7680×3840 RGBA8 = **118 MB** → ~**230** full-screen frames in ~27 GB usable.
- RGBA16 (HDR) ≈ 236 MB → ~115 frames.
- BC7-compressed ≈ 30 MB → ~900 frames, but BC7 *encode* is slow — reserve it for
  a cold/disk cache, never the flick path.

**Conclusion: 32 GB is not the constraint.** Keep decoded frames resident; do not
re-decode on revisit. The ring can be large enough that realistic scrubbing never
overruns it for fit-sized textures.

## 5. Decode strategy & format tiers

Three tiers per image:
- **Tier 0 — instant:** embedded thumbnail/preview (EXIF IFD1, JPEG MPF, HEIC
  `thmb`, RAW preview). Sub-millisecond; shown immediately while scrubbing.
- **Tier 1 — fast:** scaled-to-fit decode of the full image for the active
  window. The steady-state quality.
- **Tier 2 — full:** native-resolution decode, only when settled / zooming.

Decode-to-fit value ranking: **JPEG ≫ WebP > JXL(C) ≫ rest.** JPEG (the bulk of
real libraries) is where the scaled-decode win is largest, so it's first to land.

Format backends live behind `pb-decode::ImageDecoder` (decode-to-fit +
preview-first are part of the trait) so any codec is swappable and A/B-benchmarkable.

## 6. GPU API & presentation

- **wgpu is the v1 renderer** (DX12 backend on Windows, Metal on macOS), behind a
  thin `Renderer` trait. `winit` owns window + input. **Native D3D12 is a retained
  acceleration backend** behind the same trait, used only if a measured need
  appears (§7). Raw Vulkan is rejected on Windows (no DXGI waitable object).
- **Present mode `Mailbox`** for low latency without tearing (the RTX 5090
  qualifies). `Immediate` (ALLOW_TEARING, sync interval 0) only for absolute-
  minimum latency accepting a faint tear. Never plain `Fifo` (queues frames).
- **Low-latency loop:** flip-model swapchain + frame-latency **waitable object** +
  `SetMaximumFrameLatency(1)`; **wait at the top of the loop**, then sample input,
  render newest, present. wgpu's DX12 backend (`Dx12BackendOptions`, v27+) exposes
  the waitable handle — the hook for self-paced advance. (~1 frame latency; lower
  with ALLOW_TEARING + Independent Flip via borderless-fullscreen at native res.)
- **Refresh rate** via winit `monitor.refresh_rate_millihertz()` → cap the
  advance rate (integer approximation; for the cap, not exact pacing).
- **HDR** follows wgpu's HDR surface support when it ships (the in-shader color
  path is already ready), or the native-D3D12 acceleration backend if pursued (§8).

## 7. Zero-copy GPU decode (gated acceleration backend)

"Decode straight into VRAM": `nvImageCodec`/nvJPEG decode into CUDA device memory,
and a CUDA↔D3D12 external-memory step aliases it as a renderable texture — no PCIe
upload. wgpu exposes no stable external-texture import (confirmed by the codex
review: only the unsafe-HAL `create_texture_from_hal`; wgpu's `ExternalTexture` is
*not* CUDA/DXGI import), so this path needs the native-D3D12 acceleration backend
(§6).

**Status: deferred, gated.** The spikes showed CPU decode + the staging-ring upload
already clear 120 Hz for the real corpus, so v1 does **not** build this. It is a
later spike with a **kill criterion (ADR-012): keep it only if it beats the tuned
CPU + staging path by a meaningful margin on the high-MP stress test (45/60/100 MP).**

**Scope if pursued.** Covers what nvImageCodec decodes (JPEG/PNG/WebP/JPEG2000/
TIFF); HEIC/AVIF/JXL/SVG/RAW stay on the CPU path regardless. NVDEC tile-decode for
AVIF/HEVC-still is a separate, larger effort.

**Risks — benchmark, don't assume:** hand-rolled nvImageCodec FFI + `cudarc`; the
RTX 5090's hardware-JPEG path must be queried/benchmarked (NVIDIA now lists HW-JPEG
on Blackwell/Ada/Hopper/Ampere — the "datacenter-only" caveat is likely stale, but
verify); CUDA↔D3D12 shared-fence sync must be race/tear-free.

**macOS:** the *easier* zero-copy target — unified memory removes the upload
entirely; the `UploadStrategy` seam lets a Metal/UMA backend short-circuit it.

## 8. Color & HDR

Do color management **in-shader**: for the common matrix/TRC profiles (sRGB, P3,
Adobe RGB) extract a 3×3 matrix + transfer curves (via `moxcms`) and apply on the
GPU — effectively free, so per-image ICC transforms are *not* a perf concern.
Complex CLUT/CMYK profiles fall back to `lcms2` behind a flag. v1 targets
wide-gamut **SDR (Display-P3)**; true-HDR surface output follows wgpu's HDR surface
support when it ships (the in-shader path is already ready), or the native-D3D12
acceleration backend if pursued.

## 9. A/B seams (where "is it faster?" gets answered with data)

Each is a trait with interchangeable implementations and a benchmarked default:

- **Decode backend** — e.g. `turbojpeg` vs `zune-jpeg`.
- **Cache/eviction policy** — `pb-core::plan_residency` variants.
- **Prefetch policy** — `pb-core::prefetch_targets` window shapes/sizes.
- **Present mode** — Mailbox vs Immediate vs raw-D3D12.
- **Upload strategy** — StagingBelt vs persistent-mapped pool vs (later) zero-copy.

A single A/B runner replays a scripted keypress workload per variant and logs
per-frame NDJSON; we compare p50/p95/p99 keypress→photon, not means.

## 10. Cross-platform boundary

Windows-first, but the Mac (Apple Silicon) port is kept cheap by quarantining
platform specifics behind single helpers:

| Seam | Windows (v1) | macOS (port) |
|---|---|---|
| Renderer | **wgpu** (DX12 backend) | wgpu (Metal backend) |
| GPU decode (optional) | nvImageCodec/CUDA — gated acceleration | VideoToolbox/ImageIO |
| Upload | **staging-buffer ring** (`copy_buffer_to_texture`) | UMA short-circuit |
| Photon timestamp | DXGI `GetFrameStatistics` | `CAMetalDrawable.presentedTime` |
| Refresh rate | winit monitor query | winit monitor query |
| Color | in-shader (identical) | in-shader (identical) |

The constant across platforms is **wgpu + the CPU decode pool + staging-ring upload
+ in-shader color**; GPU-decode/zero-copy is an optional, gated acceleration behind
the traits.

## 11. Prior-art validation (what the fast viewers prove)

The design above matches what the fastest tools actually do (see
`research/prior-art-viewers.md`):

- **Embedded-preview-first is table stakes** — it's the entire basis of Photo
  Mechanic's reputation and Lightroom's "Embedded & Sidecar" (~90% faster
  import); macOS ImageIO thumbnails decode ~30× faster than full images. We make
  it part of the decode contract (ADR-004).
- **GPU-resident textures are the open lane.** Every famous *Windows* viewer
  (JPEGView, IrfanView, FastStone, XnView) is CPU/GDI with no GPU and mostly
  single-threaded (XnView 55 s vs multithreaded nomacs 12 s for 2000 thumbnails).
  A GPU-resident, multithreaded pipeline is precisely how PhotoBlaze wins.
- **Scheduling beats horsepower.** qimgv's smoothness is pure queue preemption +
  discard-on-reverse, not GPU. Adopted in §3.
- **Two-tier prefetch** (read bytes far ahead, decode only near the cursor) — our
  RAM byte LRU + near-cursor decode window.
- **Keep the flick cache lean.** Photo Mechanic deliberately avoids a big
  persistent cache because it slows launch — our resident ring is bounded and
  disposable, with the (optional) persistent layer only for thumbnails.
- **Decode-to-fit is a free win competitors leave on the table** (JPEGView /
  IrfanView / FastStone don't DCT-scale JPEG). We do (ADR-004).

**Alternative to evaluate:** mpv's **libplacebo** (multi-backend Vulkan/GL/D3D11,
EWA-Lanczos scalers, built-in frame queue) is the gold standard for GPU scaling
quality. We default to wgpu (for the DXGI latency hooks and the portable Metal
path), but the high-quality downscaler is an A/B seam: port EWA-Lanczos to WGSL,
or bind libplacebo, and compare. Reference architectures to read: **JPEGView**
(pipeline shape) and **libplacebo** (GPU stack).
