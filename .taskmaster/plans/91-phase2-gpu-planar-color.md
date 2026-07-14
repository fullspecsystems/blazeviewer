# Task #91 Phase 2 — Planar GPU color + scale path (P010/NV12 in-shader)

> Status: **draft for Codex review** · Owner: JD · 2026-07-14
> Parent: the video playback overhaul (`.taskmaster/docs/video-playback-overhaul.md` §8), task #91.
> Prereq context: Phase 1 is done; **0D proved the bottleneck** (below). This plan supersedes the
> §8 sketch with implementation-ready seams.

## Goal

Move the per-frame **video color conversion off the CPU and into the wgpu shader** for the
FFmpeg-backed video path (macOS fallback + Linux; the Windows MF path already does this). The
FFmpeg producer stops emitting `Rgba8`/`Rgba16Float` and instead ships **planar NV12 (SDR) / P010
(10-bit HDR)** frames that the GPU converts (YUV→RGB, PQ/HLG EOTF, source-primaries→scRGB) in the
existing fp16 scene pass. This retires the CPU `pack_scrgb_f16` PQ/HLG→scRGB pass (**R6**) and its
per-frame scoped-thread fan-out (**R8**).

## Why now — the 0D evidence (measured 2026-07-14, owner's BeeNAS over gigabit WiFi 6E/7)

Headless decode-throughput traces (`net_decode_throughput` / `net_audio_throughput`, pb-decode,
`PB_NET_TEST_MKV`) on the Dune 4K DoVi/HDR/TrueHD corpus:

| Stage | Margin over real-time |
|---|---|
| SMB raw read | ~15× |
| Audio decode (TrueHD → stereo) | ~29.5× |
| **Video decode + convert** (VideoToolbox HW decode + CPU P010→scRGB fp16) | **~1.19× (28.6 fps)** |

Warm-cache video was identical to cold (1.19× vs 1.17×) → **decode+convert-bound, not network-bound**.
Of that pipeline, HW decode is cheap and the swscale scale is cheap; the cost is `pack_scrgb_f16`
(a 65 536-entry LUT lookup ×3 + a 3×3 matrix + f16 pack **per pixel**, `convert.rs:228-310`), which
only clears real-time today because of the R8 per-frame thread fan-out (a stopgap). Phase 2 is the
one change that lifts this margin: the GPU already does exactly this math for HDR stills, essentially
for free.

**Non-motivation:** 1F (network read-ahead) is deshelved — 0D showed 15× network headroom. This plan
does not touch network buffering.

## What already exists (do not rebuild)

- **The entire render-side NV12 GPU path is shipped** (task #79.10, Windows NVDEC): the `fs_scene_nv12`
  wgsl entry (`gpu.rs:98-126`), `R8Unorm` Y + `Rg8Unorm` UV textures + a filtering sampler, the
  `nv12_bgl` bind-group layout (`gpu.rs:453-487`), the `scene_nv12` pipeline (`gpu.rs:680-704`),
  `upload_nv12_reusable` (`gpu.rs:1283-1381`) over the `StagingUpload` ring, `ColorUniform::new_nv12`
  (`gpu.rs:334-343`), the `ReuseSlot` (`gpu.rs:1695-1707`), and golden tests
  (`offscreen_nv12_matches_the_cpu_reference`, `gpu.rs:3367-3395`). It is fed **only** by the Windows
  MF producer (`mf_video_producer.rs` `OutKind::Nv12`) — the only planar `VideoFrame` emitted today.
- **The fp16 scRGB scene + present pipeline** (`INTERMEDIATE_FORMAT = Rgba16Float`, `gpu.rs:22`): the
  scene pass linearizes/converts into scene-linear, the present pass tone-maps (SDR extended-Reinhard
  with a per-image `peak`) or passes through (HDR/EDR). Phase 2 feeds into this unchanged.
- **`hw.rs` already produces CPU NV12/P010** from the VideoToolbox/VAAPI surface
  (`transfer_if_hw`, `hw.rs:197-216`) — it is then immediately swscaled to RGBA and the planar data is
  discarded.
- **The frame contract half-anticipates this:** `PixelFormat::Nv12` exists (`lib.rs:193-198`);
  `VideoColorInfo` carries `yuv_matrix`, `full_range`, CICP `(primaries,transfer,matrix)`, and `peak`
  (`video.rs:163-194`); `video.rs:156` explicitly says "a future fp16/P010 backend can re-derive."
- **CPU references the shader must match:** `pb-decode/src/yuv.rs` (the fuzzed YUV→RGB bit-reference —
  Bt601/709/2020, limited/full, 8/10/12-bit) and `pb-decode/src/color.rs` (`pq_to_scrgb`,
  `hlg_to_scrgb`, `linear_primaries_matrix`, `color.rs:206-260`) — the exact math `pack_scrgb_f16`
  runs today.

**The gap is one component:** the FFmpeg `FrameConverter` (`convert.rs`) is hard-wired to swscale
everything to interleaved RGBA8/RGBA64LE→fp16. Plus P010 needs a new `PixelFormat` variant + a
10-bit GPU shader (the NV12 shader is 8-bit SDR only).

## Decisions (recommended — the ⚠ ones are Codex questions in §Open questions)

1. **Keep the tight, display-fitted `VideoFrame` buffer for v1** (no plane/stride/crop/coded-size
   fields). `convert.rs` swscales YUV→**NV12/P010 at the fitted output size** (scale + chroma only,
   **no** YUV→RGB, **no** `pack_scrgb_f16`), producing a tight `[Y plane][interleaved UV plane]`
   buffer exactly like the MF NV12 frame. The GPU does all color. This removes the R6/R8 cost (the
   measured bottleneck) with the **smallest** contract change. The §2A "coded-res + GPU scale, crop
   in UV" purist path is deferred (⚠ Q1) — decode-to-fit is measured inert on the big display anyway.
2. **The planar fast path is eligibility-gated; the CPU RGBA/fp16 path stays as the fallback.**
   Eligible when: **4:2:0 subsampling**, **`rotation == 0`**, **SAR == 1** (square pixels), and — for
   P010 — the **adapter supports `TEXTURE_FORMAT_16BIT_NORM`**. Otherwise fall back to today's
   `swscale`→RGBA/`pack_scrgb_f16` path (rotated/anamorphic/exotic clips keep working). This confines
   the new hot path to the overwhelmingly-common case and never regresses correctness (⚠ Q2 on
   rotation/SAR).
3. **HDR peak becomes metadata-driven** (MaxCLL / mastering-display), replacing the R11 running-max
   `conv.peak()` (which lived inside `pack_scrgb_f16`, now gone for P010). Decays/resets on seek. The
   corpus carries MaxCLL 1000, so metadata wins here (⚠ Q4).
4. **Golden tests are per-byte-tolerance GPU-vs-CPU** (matching the shipped NV12 golden harness).
   **There is no `nv-flip` in the repo** — the CLAUDE.md claim is wrong; do not add a perceptual-diff
   dependency for this. The references are `yuv.rs` (YUV→RGB) and `color.rs` (PQ/HLG EOTF + primaries).
5. **SDR video also goes planar (NV12)** via the same producer seam — the render path already exists,
   so it's nearly free and removes the CPU YUV→RGB for SDR video too. P010/HDR is the headline (the
   0D bottleneck); NV12/SDR rides along.

## Design

### 2A. Frame contract — add `PixelFormat::P010`

- `crates/pb-decode/src/lib.rs` `PixelFormat` (`:185-222`): add `P010` — 4:2:0, **16-bit-per-sample,
  10 valid high bits**, tight layout `[Y: w·h·2 bytes][interleaved UV: (w/2)·(h/2)·2·2 bytes]`.
  `frame_bytes` = `w·h·2 + w·h` = `w·h·3` (`:214-221`); `bytes_per_pixel` panics like `Nv12`
  (subsampled). Even-dim requirement like `Nv12`.
- `VideoFrame::is_well_formed` (`video.rs:224-234`): extend the NV12 even-dim + `frame_bytes` check to
  P010.
- **No new `VideoFrame` fields.** `VideoColorInfo` already carries `yuv_matrix`, `full_range`, `cicp`
  `(primaries,transfer,matrix)`, `transform` (primaries+TRC), and `peak` — all Phase 2 needs. (The
  §2A per-plane/stride/crop/coded-size contract is deferred with Q1.)

### 2B. Producer emits planar (the "wire FFmpeg to planar" work)

- `convert.rs`:
  - `output_format()` (`:110-116`): return `P010` for eligible HDR, `Nv12` for eligible SDR, else the
    current `Rgba16F`/`Rgba8`. Eligibility (Decision 2) computed at construction from the fit box +
    rotation + SAR + subsampling + a renderer-capability flag threaded in from `pb-app-core`.
  - `convert()` (`:143-223`): for the planar arm, run **one swscale pass YUV→`Pixel::NV12`/`P010`** at
    the fitted `(out_w,out_h)` (scale + chroma reposition only), then pack the Y + interleaved UV
    planes tightly (a new `tight_planar()` mirroring `tight_rgba` `:324-335`, honoring both plane
    strides). **`pack_scrgb_f16` (R6) and its R8 thread scope are not run on this path.**
  - Rotation: the planar arm requires `rotation == 0` (gate above), so `rotate_rgba`/`rotate_bytes`
    (`:214,219,339`) are not needed here. (Planar rotation of Y + subsampled UV is a non-goal, Q2.)
- `video_producer.rs` `make_frame` (`:577-618`): add a planar arm building `VideoColorInfo` with
  **live** `yuv_matrix` + `full_range` + `cicp` (so the renderer applies YUV once) and, for P010, the
  metadata `peak` (2D) — mirroring the MF NV12 color plumbing (`mf_video_producer.rs:129-142`). The
  SDR RGB branch's "matrix/range inert" is only for the RGBA fallback.
- `hw.rs`: unchanged — `transfer_if_hw` already yields the CPU NV12/P010 frame swscale consumes.

### 2C. GPU shader + textures (`fs_scene_p010`)

- **Textures:** `R16Unorm` Y + `Rg16Unorm` UV (10-bit stored high-aligned in 16). Requires the
  **`wgpu::Features::TEXTURE_FORMAT_16BIT_NORM`** device feature — currently `required_features:
  Features::empty()` (`gpu.rs:1512`). Request it **when the adapter advertises it**; expose a
  capability bool to `pb-app-core` so `convert.rs` eligibility (2B) falls back to CPU fp16 on adapters
  without it (⚠ Q3 — verify Metal/DX12/WARP/lavapipe support). These formats are filterable where
  supported, so the existing `Float{filterable:true}` bgl shape + `Filtering` sampler carry over.
- **Shader** (`fs_scene_p010`, or a mode flag on `fs_scene_nv12`): the pipeline order, each applied
  exactly once —
  1. **10-bit range expand + YUV matrix.** `R16Unorm` samples as `[0,1]` = `value/65535`; multiply to
     the 10-bit domain and apply limited/full constants (limited: `Y' = (Y·1023 − 64)/876`, chroma
     `(C·1023 − 512)/896`; **different from** the NV12 8-bit `/219`,`/224` at `gpu.rs:109-111`). Use
     the `yuv_matrix` coefficients already packed by `ColorUniform` — **must match `yuv.rs` within
     tolerance**, extended to the 10-bit path (`yuv.rs` already covers 10-bit — the `ten_bit_*` tests).
  2. **PQ (SMPTE-2084) or HLG EOTF → scene-linear.** New wgsl — **none exists today** (the current
     `eotf()` `gpu.rs:57-62` is only the moxcms 7-param parametric curve, which cannot express PQ/HLG).
     Must match `color::pq_to_scrgb` / `hlg_to_scrgb` (`color.rs:209-242`), including the 203-nit
     `SDR_WHITE_NITS` convention (1.0 = SDR white).
  3. **Source-primaries (BT.2020) → scRGB/BT.709 3×3 matrix** (match `linear_primaries_matrix`,
     `color.rs:249`).
  4. **Output scale** (`cx.scale.x`, the SDR-white scale) — same as the other scene modes.
  - **Relax the `[0,1]` clamp** at `gpu.rs:113-117` for the HDR path — it would clip PQ highlights.
- **Plumbing:** a `p010_bgl` + `scene_p010` pipeline mirroring `nv12_bgl`/`scene_nv12`
  (`gpu.rs:453-487`, `680-704`); `upload_p010_reusable` mirroring `upload_nv12_reusable`
  (`StagingUpload` already handles arbitrary bpp — no upload changes); a `ReuseSlot` format
  discriminator beyond the current `R8Unorm` check (`gpu.rs:1300-1303`); `render`'s pipeline pick
  (`gpu.rs:2698-2704`); `set_video_p010` (mirror `set_video_nv12` `gpu.rs:2351`); and
  `present_video_frame` dispatch (`app_core_impl.rs:7475-7503`).

### 2D. HDR policy (retire R11)

- The SDR-white **`peak`** the present pass uses (`PresentUniform.params.x`) came from the R11
  running-max in `pack_scrgb_f16` (`convert.rs:271,304-308`), read by `make_frame`. With the CPU
  convert gone for P010, source it from **stream/frame metadata** (mastering-display max-luminance /
  MaxCLL, already parsed for stills). No per-frame CPU pixel scan. If any adaptive peak is kept, it
  must **decay or reset on seek** (R11 was monotonic-forever — one bright frame permanently dimmed the
  rest of the session). Corpus MaxCLL = 1000 → metadata is authoritative here.
- **Dolby Vision:** the P010 path renders the **HDR10-compatible base layer** only (static PQ). DoVi
  dynamic metadata (RPU) is **not** implemented — surface as a capability, never claim "DoVi correct"
  (locked rule #8). HDR10+ same honest base-layer treatment.

### 2E. Remove the R8 stopgap

Once the P010 path passes gates, the per-frame scoped-thread fan-out in `pack_scrgb_f16`
(`convert.rs:257-307`) is **removed from the video path**. Keep a **serial** `pack_scrgb_f16` for (a)
the eligibility fallback (rotated/anamorphic/no-16bit-norm clips) and (b) the **stills HDR path**
(`common::finalize_hdr_scrgb`), which is unchanged and unaffected.

## Correctness gates

- **Golden GPU-vs-CPU, per-byte tolerance** (extend the shipped NV12 harness, `gpu.rs:2930-3167`):
  add `OffscreenSource::P010{y,uv,params}` + `render_offscreen_p010`, a CPU `p010_to_rgba` reference
  (mirror `yuv.rs:53`), and assert GPU output matches the CPU `yuv.rs`+`color.rs` pipeline within a
  small tolerance — across **BT.601/709/2020 × limited/full × SDR/PQ/HLG**, black/white/chroma ramps,
  and (for the fallback) odd crops/SAR/rotation. Enable `TEXTURE_FORMAT_16BIT_NORM` on the test device.
- **Independent reference** where feasible (FFmpeg `zscale`/libplacebo or a captured AVFoundation
  frame) so a bug in the CPU `pack_scrgb_f16` isn't just re-encoded into the GPU shader (⚠ Q5).
- **Physical-display EDR validation** (owner, M2 + the Studio Displays): HDR highlight scale + SDR-white
  behavior look right, not just "it decoded." Verify against the AVPlayer remux control (`dvhe.08`).
- **Privacy/no-trace tests stay green** — this path is read-only + RAM-only; no new disk writes.

## Performance gates

- **The 0D metric moves:** re-run `net_decode_throughput` on the corpus — the P010 path must clear the
  content interval with real margin (target ≫ the current 1.19×; the color convert leaves the CPU
  entirely, so decode throughput should approach the raw VideoToolbox decode rate).
- Steady-state upload+draw p99 clears the frame interval; **zero** post-preroll starvation/audio pauses
  on the hardware-supported corpus.
- **No per-frame OS-thread creation and no per-frame full-frame CPU allocation** on the planar path
  (the whole point — R8 gone; the `ReuseSlot` + `StagingUpload` ring already give zero-alloc steady
  state).

## Test-first phases

1. **Contract:** `PixelFormat::P010` + `frame_bytes`/`bytes_per_pixel`/`is_well_formed`; unit tests.
   (pb-decode, pure.)
2. **GPU P010 shader + textures + golden** (no producer needed — drive the offscreen harness with
   **synthetic** P010 planes): enable the 16-bit-norm feature (capability-gated), `fs_scene_p010`,
   `p010_bgl`/`scene_p010`, `upload_p010_reusable`, `set_video_p010`. Golden GPU-vs-CPU across the
   matrix/range/transfer grid. This is the bulk of the new code and is fully testable in isolation.
3. **Producer emits planar:** `convert.rs` eligibility + swscale-to-NV12/P010-at-fit + `tight_planar`;
   `make_frame` color plumbing; `present_video_frame` dispatch. NV12/SDR first (render path exists),
   then P010/HDR. Fallback to RGBA/fp16 when ineligible. (Owner-verify on the corpus.)
4. **HDR policy:** metadata-driven peak; retire R11 running-max; decay/reset on seek.
5. **Remove the R8 video stopgap** (serial fallback + stills path retained); re-measure 0D.
6. **Gates:** goldens, perf re-measure, physical EDR, privacy tests.

## Test matrix + file map

- **pb-decode:** `PixelFormat::P010` (`lib.rs`), `is_well_formed` (`video.rs`); `convert.rs` planar
  output + `tight_planar` + eligibility; `video_producer.rs` `make_frame` planar arm; a CPU
  `p010_to_rgba` reference (near `yuv.rs`). Fuzz the tight-planar packing.
- **pb-render:** `fs_scene_p010` + PQ/HLG EOTF wgsl; `R16Unorm`/`Rg16Unorm` slot; `p010_bgl`/
  `scene_p010`/`upload_p010_reusable`/`set_video_p010`; feature request + capability bool;
  `OffscreenSource::P010` + `render_offscreen_p010`; the GPU-vs-CPU goldens (`gpu.rs` test mod).
- **pb-app-core:** `present_video_frame` P010 dispatch; thread the renderer's 16-bit-norm capability
  to `convert.rs` eligibility; the metadata `peak` source (2D).

## Non-goals (v1)

- **Zero-copy** IOSurface/`CVPixelBuffer`→wgpu import (no external-texture ingest in the renderer;
  textures are `COPY_DST`) — that's the reserved Phase 4 escalation, only if readback+upload is proven
  the limiter *after* this.
- **Rotated / anamorphic (SAR≠1) planar** — those keep the CPU RGBA/fp16 fallback (Q2).
- **DoVi dynamic metadata / HDL10+ dynamic** — base-layer only (2D).
- **The §2A coded-res + GPU-scale + crop-in-UV contract** — deferred (Q1); v1 swscales to fit.
- **Windows MF changes** — MF already ships NV12; P010 on MF is a separate follow-on (79.10's
  reserved rung), out of scope here.

## Open questions (Codex)

1. **Scale strategy.** (a) swscale YUV→P010 **at fitted size** (this plan — small contract change,
   keeps the tight display-fitted buffer, removes the color convert which is the actual bottleneck) vs
   (b) upload **coded-res** P010 + GPU scales, crop/SAR in UV/geometry (the §2A end-state — removes
   swscale entirely but needs coded-size + crop fields on `VideoFrame` and more upload bandwidth).
   Recommendation: (a) for Phase 2, (b) as a follow-on only if swscale-scale shows up in a trace.
   Is (a) acceptable, given HDR chroma is scaled in the PQ-encoded domain (as swscale/BILINEAR does
   today)?
2. **Rotation/SAR gating.** Is gating the planar fast path on `rotation == 0` + `SAR == 1` (CPU
   fallback otherwise) acceptable for v1, or is rotated-HDR-over-network common enough to need
   in-shader rotation now?
3. **`TEXTURE_FORMAT_16BIT_NORM` availability.** Is it reliably present on Metal (Apple Silicon),
   DX12, and the CI software adapters (WARP / lavapipe)? If not, P010 needs the CPU fp16 fallback on
   those — confirm the capability-gate + fallback is the right shape.
4. **HDR peak source.** Metadata (MaxCLL/mastering) only, or a bounded GPU-computed adaptive peak with
   decay? Metadata is simpler and correct for the corpus; adaptive helps untagged HDR.
5. **Golden reference independence.** Is matching `yuv.rs` + `color.rs` (the CPU truth) sufficient, or
   must the golden also check against an independent reference (zscale/libplacebo/AVFoundation capture)
   to avoid preserving a latent CPU bug?
6. **NV12 8-bit range constants.** The shipped `fs_scene_nv12` uses `/219`,`/224` (`gpu.rs:109-111`).
   Confirm the 10-bit `/876`,`/896` derivation and that reusing one shader with a bit-depth flag is
   cleaner than a separate `fs_scene_p010`.
