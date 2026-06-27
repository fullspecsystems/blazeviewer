# PhotoBlaze — Decisions Log (ADRs) & Open Questions

Status legend: **Accepted** (decided), **Proposed** (default pending owner
confirmation), **Open** (needs owner input — see bottom).

---

## Accepted

### ADR-001 — Language: Rust
Bare-metal performance, fearless concurrency for the decode pool + shared caches,
and the heavy codecs are C either way (we bind to them). Rust makes the
*orchestration* memory-safe, which is where a C++ version would bleed.

### ADR-002 — GPU API: wgpu (D3D12 on Windows, Metal on macOS), behind a `Renderer` trait
*Revised again 2026-06-26 after the decode/upload spikes + codex review (see the
post-spike update below).* **wgpu is the v1 renderer** — DX12 backend on Windows,
Metal on macOS — behind a thin `Renderer` trait. The spikes showed CPU decode
(2.5×) and a persistent staging-buffer upload ring (3.4×) already clear 120 Hz for
the real corpus, so wgpu's portability costs nothing measurable here. **Native
D3D12 is retained as a measured *acceleration backend* behind the same trait** (see
ADR-012), not the default. Raw Vulkan stays rejected on Windows (no DXGI waitable);
within wgpu we prefer the DX12 backend for its frame-latency waitable object.

### ADR-002a — macOS is a cheap wgpu/Metal port, not a separate effort
With wgpu as the v1 renderer, the macOS (Apple Silicon) port is largely a recompile
to the Metal backend plus a hardware-decode/upload backend swap; the trait seams
isolate the platform bits. (Reverts the earlier native-D3D12-only stance — still
deferred to v2, but no longer a rewrite.)

### ADR-003 — Presentation: flip-model + waitable object, present mode `Mailbox`
Lowest latency without tearing. `SetMaximumFrameLatency(1)`, wait at the **top**
of the loop, then sample input → render newest → present. `Immediate` is a toggle
for absolute-minimum latency (accepting a faint tear). Never plain `Fifo`.

### ADR-004 — Decode-to-fit + preview-first are part of the decode contract
`pb-decode::ImageDecoder` takes a `FitBox` and may return an embedded preview
first. These are the two biggest decode-speed levers, so they're not optional
add-ons — they're the interface.

### ADR-005 — "Random" = a precomputed shuffle order
Rolling a die per keypress is unprefetchable. A precomputed permutation makes the
next random targets *known*, hence preloadable; walking it visits each photo once
before reshuffling. (`pb-core::ShuffleOrder`.)

### ADR-006 — The keypress path is a rebind, never a decode or upload
Steady state = cache lookup + rebind resident texture + draw. Decode and upload
happen during prefetch. A keypress that triggers either means the prefetch window
is wrong.

### ADR-007 — Workspace with trait-based A/B seams
`pb-core` / `pb-decode` / `pb-render` / `pb-app`. Every non-obvious hot-path
choice (decode backend, cache policy, prefetch policy, present mode, upload
strategy) is a trait with a benchmarked default.

### ADR-008 — Decode pool: priority queue + cancellation (not bare rayon)
Rayon's work-stealing reorders prefetch and has no priorities; a direction
reversal must be able to cancel now-stale jobs.

### ADR-009 — Color management in-shader (moxcms)
Extract 3×3 matrix + TRC for common profiles and apply on the GPU — effectively
free. `lcms2` behind a flag for exotic CLUT/CMYK. Identical on Windows and macOS.

### ADR-010 — TDD, >80% coverage (cargo-llvm-cov), golden-image rendering tests
Pure logic isolated in `pb-core`; headless **wgpu** render (WARP/lavapipe in CI)
→ readback → nv-flip diff vs reference PNGs for the GPU path; proptest for
invariants; cargo-fuzz for decoders. GPU shell marked `#[coverage(off)]` to keep
the number honest.

### ADR-011 — Instrumentation is first-class
`tracing` + Tracy with **wgpu timestamp queries** (+ `wgpu-profiler`), compiled out
of release; keypress→photon via DXGI `GetFrameStatistics`, validated against
PresentMon; Criterion locally + deterministic instruction-count gate (CodSpeed/iai)
in CI on platform-independent code. Report p50/p95/p99, never means.

### ADR-012 — GPU decode + zero-copy is a *gated acceleration backend*, not v1
*Revised 2026-06-26 (spikes + codex).* v1 ships the **CPU decode pool + persistent
staging-ring upload on wgpu** (ADR-017). `nvImageCodec` GPU decode + CUDA→D3D12
zero-copy is kept behind the `DecodeBackend` / `UploadStrategy` seams as an
acceleration path, pursued **only if** a measured bottleneck appears (chiefly
high-MP). **Kill criterion:** keep the zero-copy path only if it beats the tuned
CPU + staging path by a meaningful margin on the high-MP stress test (45/60/100 MP);
otherwise drop it. NVIDIA's current nvJPEG docs list hardware-JPEG acceleration on
Blackwell/Ada/Hopper/Ampere — the earlier "datacenter-only" caveat is likely stale
— but the RTX 5090's actual path must still be queried and benchmarked before any
design depends on it. (FFI cost stands: no Rust bindings; hand-rolled bindgen +
`cudarc`.)

### ADR-013 — Provisional library picks
Per the table in `CLAUDE.md` / `architecture.md`. All provisional and
benchmark-justified; the seams exist so any can be replaced with data.

### ADR-014 — SVG via resvg; RAW via embedded-preview extraction
resvg/usvg rasterizes to a tiny-skia pixmap at on-screen resolution (uploads
directly). RAW browses via its embedded full-size JPEG preview (cheap); full
demosaic is deferred to a later zoom feature.

### ADR-015 — HEIC and AVIF are first-class in v1 (CPU-decoded)
*Per Q-2.* Included from v1, but **CPU-decoded** (`libheif-rs` / `dav1d` +
`avif-parse`) into the resident ring — there is no turnkey GPU decoder. NVDEC
tile-decoding is a much larger, later optimization (gated by measurement). We
accept the `libheif` Windows build cost: pin vcpkg ports or ship prebuilt DLLs,
and isolate it behind a cargo feature so a broken build never blocks the core.

### ADR-016 — Color: wide-gamut SDR (Display-P3) for v1; HDR when wgpu surfaces it
*Per Q-3; updated 2026-06-26.* In-shader color management (`moxcms`, matrix + TRC)
targeting Display-P3 now. True-HDR *surface* output follows wgpu's HDR surface
support when it ships (the in-shader pipeline is already ready for it), or via the
native-D3D12 acceleration backend if that path is ever pursued. (No longer "free via
D3D12" now that wgpu is the default renderer.)

### ADR-017 — Upload via a persistent staging-buffer ring, never `write_texture`
*Measured 2026-06-26 (upload spike).* `queue.write_texture` collapses to ~60–75 fps
on large frames (fresh staging allocated per call); a persistent mapped staging
buffer + `copy_buffer_to_texture` hits ~48 GB/s ≈ 414 fps for a 118 MB frame (3.4×
budget) — pure wgpu. v1 uploads through a staging-buffer ring behind an
`UploadStrategy` seam. Still to measure: a faithful end-to-end run (per-frame CPU
write into mapped staging → copy → draw → present while holding nav at 120 Hz).

---

## Owner decisions (resolved 2026-06-26)

| Q | Decision | Effect |
|---|----------|--------|
| Q-1 Zero-copy vs portability | **Pursue zero-copy now** | Native D3D12 renderer (ADR-002); GPU decode is a primary v1 goal (ADR-012); Mac port is a separate later backend (ADR-002a). |
| Q-2 HEIC/AVIF in v1 | **Both first-class** | Included v1, CPU-decoded (ADR-015); libheif behind a feature flag. |
| Q-3 Color/HDR | **Wide-gamut SDR (P3)** | In-shader moxcms now; HDR via the D3D12 swapchain later (ADR-016). |
| Q-4 Toolchain | **Owner installs it** | Owner sets up `rustup`; plan assumes the toolchain will be present. |
| Q-5 RAW depth | **Embedded-preview only** (default) | Browse via embedded JPEG preview; full demosaic deferred to v2. `rawler` is LGPL-2.1 — revisit if broad RAW coverage is needed. |

> The owner is optimizing for maximum speed over portability. These decisions
> intentionally trade the cheap wgpu Mac port and a simpler build for the fastest
> possible Windows pipeline. The trait seams keep the deferred options open.

### Update — 2026-06-26 (post-spike + codex review): Q-1 reversed
The decode spike (CPU **2.5×**) and upload spike (staging ring **3.4×**) showed the
portable path already clears 120 Hz for the real ≤16 MP corpus, and the codex review
concurred: **A for the v1 engine, C for the architecture.** wgpu + CPU decode +
staging-ring upload is the v1 foundation; native D3D12 + zero-copy becomes a *gated
acceleration backend* (ADR-012 kill criterion), not the default. ADR-002, ADR-002a,
ADR-012, ADR-016 revised; ADR-017 added. This **supersedes** the Q-1 "pursue
zero-copy now" decision above. Codex also asked for: previews sequenced before
zero-copy, a high-MP stress test (45/60/100 MP + progressive/odd-chroma/large-ICC/
orientation) measuring p50/p95/p99 decode·upload·ready-miss·keypress→photon, and a
faithful end-to-end upload run — all reflected in the roadmap.

---

## Naming & domain (checked 2026-06-26)

The **PhotoBlaze** name is clear for our purposes: no established photo-viewer
software owns it, and `photoblaze` is free on crates.io. The only same-name
collisions are a small e-commerce photo-editing service and a dormant hobby
GitHub repo (`kerryhatcher/PhotoBlaze`, a photo-management app) — neither in our
space.

**Prospective domain: `photoblaze.app`** — confirmed available (RDAP) on
2026-06-26 and the natural fit for an app. Also open: `.io`, `.dev`, `.net`,
`.photo`. Taken: `.com` (registered 2004, a parked for-sale premium) and `.xyz`
(parked). Grab `photoblaze.app` if/when the project warrants a public presence.
