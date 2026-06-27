# PhotoBlaze — Roadmap

Phased build plan, ordered so each phase produces something runnable and the
speed-critical machinery appears as early as possible. Seeds
`.taskmaster/tasks/tasks.json`. Each phase lists **exit criteria**.

Reflects the decode/upload spikes + codex review (2026-06-26): **wgpu + CPU decode
+ persistent staging-ring upload is the v1 engine; native D3D12 + zero-copy is a
late, gated acceleration spike** (`decisions.md` ADR-002/012/017 + the post-spike
update). Felt-speed multipliers (previews, preemption, cancellation, resident
textures, never blocking the UI) come *before* any GPU-decode work.

> Cadence: one photo on screen fast (Phase 1–2) → *hold a key and fly* on the CPU
> engine (Phase 3) → instant previews (Phase 4) → breadth & rigor (5–6) → only then
> consider the zero-copy escalation, gated by data (Phase 7).

---

## Progress & learnings (as of 2026-06-27)

**Done:** Phase 0 (foundations, spikes), Phase 1 (wgpu window + render), and
**Phase 2 (sequential viewer)** — plus polish beyond the plan: decode-to-fit
downscaling (Lanczos3), linear sampling, fit/original scale-mode toggle (`0`/`o`),
self-paced auto-repeat with an initial delay, GPU-adaptive texture limits, and an
info-panel overlay (`i`) with a from-scratch text layer. See
[`current-status.md`](../current-status.md) for the full handoff.

**Learned this session (informs later phases):**
- The GPU-stack reversal (wgpu + CPU decode beats native-D3D12/zero-copy for this
  workload) — already in `decisions.md`; v1 is wgpu.
- **Decode-to-fit isn't just speed, it's quality:** uploading full-res + GPU
  minification aliases/grains high-res photos. Downscaling to display size on the
  CPU (Lanczos3) fixed it *and* shrank textures (helps the Phase-3 ring).
- **Color space matters:** the surface must be non-sRGB (the JPEG bytes are already
  sRGB) or the image washes out.
- **Self-paced advance is the right nav model** — ignore OS key-repeat, drive from
  the frame loop on held keys; this also killed a resource-churn slowdown. Phase 3
  generalizes it with the prefetch ring.
- **Startup window flicker** on Windows needs hidden-until-first-frame.

**Next:** Phase 3 (the prefetch engine) is where the wedding-photo case (4–5 fps,
decode-bound) becomes instant. See below.

## Phase 0 — Foundations  *(largely done)*
- [x] Cargo workspace + 4 crates; `pb-core` (rng, shuffle, playlist, prefetch,
      cache) with **38 passing tests**; `pb-decode` trait; `pb-render::fit_rect`.
- [x] Research docs, architecture, decisions, roadmap.
- [x] **Decode spike** (CPU 2.5×) and **upload spike** (staging ring 3.4×) — they
      set the architecture; reports in `.taskmaster/reports/`.
- [x] Toolchain confirmed (cargo 1.96; `cargo test` green).
- [ ] CI: build, test, clippy, fmt, coverage (`cargo-llvm-cov`).
- **Exit:** CI green; coverage reported.

## Phase 1 — First pixels (wgpu)
- winit borderless-fullscreen at native res; `esc` quits.
- **wgpu** device/surface, **DX12 backend** on Windows; flip-model + frame-latency
  waitable; present mode `Mailbox`. (Behind the `Renderer` trait.)
- Draw one hardcoded RGBA texture as a textured quad via `fit_rect`.
- Read refresh rate; keymap skeleton (held-physical-key tracking, repeat ignored).
- Golden-image harness: headless wgpu render → readback → nv-flip diff (WARP/lavapipe in CI).
- **Exit:** test image shown correctly letterboxed; golden test passes on a
  software adapter; `esc` quits.

## Phase 2 — Real images, single-threaded (CPU)
- Directory scan (incremental; first image ASAP).
- Decode backend behind `ImageDecoder`: **`zune-jpeg`** to start (pure Rust, no
  NASM/cmake — validated by the spike at 2.5×); `turbojpeg` (DCT scaled decode) is
  the A/B alternative once NASM+cmake are available.
- Upload via the **staging-buffer ring** (`copy_buffer_to_texture`), one frame at a time.
- Wire `Playlist` nav (space/→, ⌫/←, enter) to real files; EXIF orientation.
- **Exit:** page through a folder of JPEGs fit-to-screen; per-image decode + upload
  time logged.

## Phase 3 — Make holding a key fly  *(headline engine)*
- Priority decode pool with cancellation + **queue preemption** (on-screen image jumps the line).
- Prefetch scheduler driving `prefetch_targets` + `plan_residency`.
- Resident texture ring; staging-ring uploads, prefetch-only (never the keypress frame).
- Self-paced advance: hold key → newest ready frame each vsync, capped at refresh.
- keypress→photon instrumentation (DXGI `GetFrameStatistics`) + Tracy + GPU timestamps.
- **Exit:** holding →/space sustains ~refresh-rate paging on the corpus with
  cache-hit keypress→photon ≤ ~1 frame (p95); misses fall back to preview, not stall.

> The post-core **feature backlog** (`tasks.json` #1–#10: rotate, zoom, scaling
> modes, privacy, metadata panel, help overlay, configurable keybindings, recursive
> mode, feedback toast) begins once this engine is stable.

## Phase 4 — Instant previews  *(moved ahead of any GPU-decode work, per codex)*
- Embedded thumbnail/preview extraction (EXIF IFD1, JPEG MPF; HEIC `thmb` in P5).
- Thumbnail atlas; show preview instantly, refine to the scaled decode.
- RAM byte LRU (compressed) so reverse/revisit skips disk (two-tier prefetch).
- **Exit:** fast scrubbing never shows a blank frame; reversing direction is instant.

## Phase 5 — Format breadth (HEIC/AVIF first-class, CPU-decoded)
- PNG/APNG, WebP (`use_scaling`), JXL (`jxl-oxide`), TIFF/BMP/QOI, SVG (`resvg`,
  rasterize-to-fit), RAW embedded-preview (`kamadak-exif` → JPEG).
- HEIC (`libheif-rs`) + AVIF (`dav1d`+`avif-parse`) into the ring, each behind a
  cargo feature so a broken C build never blocks the core.
- **Exit:** every in-scope format opens and pages at fit resolution; per-format
  decode benchmarks recorded.

## Phase 6 — Rigor + high-MP stress test
- Pinned/checksummed benchmark corpus (`assets/test-corpus`).
- A/B runner: scripted keypress workload per variant → per-frame NDJSON →
  p50/p95/p99; CI instruction-count regression gate.
- In-shader color (`moxcms` matrix/TRC), wide-gamut **P3**; property tests; decoder fuzzing; coverage >80%.
- **High-MP stress test (gates Phase 7):** 45 / 60 / 100 MP JPEGs + progressive,
  awkward chroma subsampling, large ICC profiles, EXIF orientation, mixed aspect.
  Workload: cold folder, hold-next at 120 Hz, direction reversal, **no
  keypress-triggered decode/upload on cache hits**. Measure p50/p95/p99 for decode,
  upload, ready-miss rate, and keypress→photon. Plus the **faithful end-to-end
  upload run** (per-frame CPU write into mapped staging → copy → draw → present).
- **Exit:** A/B reports comparable; coverage >80%; regression gate active; high-MP
  data in hand to decide Phase 7.

## Phase 7 — Zero-copy escalation spike  *(GATED — only if Phase 6 says so)*
- Pursued only if the high-MP data shows the tuned CPU + staging path misses the bar.
- nvImageCodec FFI (bindgen) + `cudarc`; CUDA→D3D12 external-memory interop behind a
  **native-D3D12 `Renderer` acceleration backend** (the wgpu external-import gap is
  real — only unsafe-HAL otherwise).
- Benchmark the RTX 5090 hardware-JPEG path (NVIDIA now lists HW-JPEG on
  Blackwell/Ada/Hopper/Ampere — verify, don't assume).
- **Kill criterion (ADR-012):** keep it only if it beats the tuned CPU + staging
  path by a meaningful margin on the high-MP stress test; otherwise drop it.
- **Exit:** a data-backed keep/drop decision.

## Phase 8 — v2 / later
- **HDR10 / scRGB** output when wgpu ships surface support (color pipeline already correct).
- NVDEC tile-decode for AVIF/HEVC-still (GPU HEIC/AVIF) — gated by measurement.
- Animation: GIF / APNG / animated WebP / AVIF sequences.
- Zoom/pan full-resolution (Tier-2) decode; full RAW demosaic (GPU/WGSL).
- **macOS (Apple Silicon) port:** wgpu Metal backend + VideoToolbox/ImageIO decode +
  UMA upload short-circuit — a recompile + backend swap, not a rewrite.

---

### Cross-phase performance targets
- **keypress→photon (cache hit):** ≤ 1 refresh interval (p95).
- **Hold-to-fly throughput:** ~monitor refresh on the corpus (fit-sized).
- **No stalls:** a cache miss degrades to a preview within 1 frame, never a freeze.
- **No regressions:** CI instruction-count gate blocks hot-path regressions.
- **Zero-copy is kept only if it wins** the high-MP stress test by a meaningful
  margin over the tuned CPU + staging path (else it's dropped).
