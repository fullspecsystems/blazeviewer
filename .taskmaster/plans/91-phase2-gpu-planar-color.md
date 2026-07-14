# Task #91 Phase 2 — Planar GPU color path (P010/NV12 in-shader)

> Status: **IMPLEMENTED + measured (2026-07-14)** — landed on `main` across pb-decode /
> pb-render / pb-app-core; awaiting owner end-to-end verification on a real HDR display. · Owner: JD
>
> **Measured win (0D A/B, `net_decode_throughput`, Dune 4K DoVi/HDR10+ TrueHD, release):**
> the RGBA/fp16 CPU-convert path (pre-Phase-2, `pack_scrgb_f16` + R8 threads) ran **1.48× real-time
> (35.5 fps)**; the planar **P010** GPU-color path runs **8.45× real-time (203 fps)** — **5.72×
> faster**, clearing the ≥1.5× 4K30 gate by ~5.6×. The per-frame CPU color convert is gone from the
> hot path. Colors verified against an independent from-spec golden. **Still to verify (owner):**
> live playback timing / audio sync with the new first-frame negotiation, and the HDR look on a real
> EDR panel. `PB_VIDEO_NO_PLANAR=1` reverts to the old path instantly.
>
> Prior status: **revised per Codex review (2026-07-14) — implementation-ready** · Owner: JD
> Parent: the video playback overhaul (`.taskmaster/docs/video-playback-overhaul.md` §8), task #91.
> Prereq: Phase 1 done; **0D proved the bottleneck** (below). v1 of this plan drew a full Codex
> review; every P0/P1 finding is folded in and each source claim was re-verified against the tree
> before writing this. Naming note: the phase is **"planar GPU color path"** — *color* moves to the
> GPU; **scaling stays on the CPU (swscale-to-fit)** for v1 (Codex answer 1).

## Goal

Move the per-frame **video color conversion off the CPU and into the wgpu shader** for the
FFmpeg-backed video path (macOS fallback + Linux; Windows MF already does this for NV12). The FFmpeg
producer stops emitting `Rgba8`/`Rgba16Float` and instead ships **planar NV12 (8-bit) / P010LE
(10/12-bit)** frames — YUV→RGB, PQ/HLG EOTF, and source-primaries→scRGB all run in the existing fp16
scene pass. This retires the CPU `pack_scrgb_f16` PQ/HLG→scRGB pass (**R6**) and its per-frame
scoped-thread fan-out (**R8**) *on the fast path*, while a parallel CPU fallback stays for
ineligible clips.

## Why now — the 0D evidence (measured 2026-07-14, owner's BeeNAS over gigabit WiFi 6E/7)

Headless decode-throughput traces (`net_decode_throughput`/`net_audio_throughput`, pb-decode,
`PB_NET_TEST_MKV`) on the Dune 4K DoVi/HDR/TrueHD corpus:

| Stage | Margin over real-time |
|---|---|
| SMB raw read | ~15× |
| Audio decode (TrueHD → stereo) | ~29.5× |
| **Video decode + convert** (VideoToolbox HW decode + CPU P010→scRGB fp16) | **~1.19× (28.6 fps)** |

Warm cache was identical to cold → **decode+convert-bound, not network-bound**. HW decode and the
swscale scale are cheap; the cost is `pack_scrgb_f16` (a 65 536-entry LUT ×3 + a 3×3 matrix + f16
pack **per pixel**, `convert.rs:228-310`), only clearing real-time because of the R8 per-frame thread
fan-out (a stopgap). The GPU already does exactly this math for HDR stills essentially for free.

**Non-motivation:** 1F (network read-ahead) is deshelved — 0D showed 15× network headroom.

## What already exists (do not rebuild) — verified in-tree

- **The render-side NV12 GPU path is shipped** (task #79.10, Windows NVDEC): `fs_scene_nv12`
  (`gpu.rs:98-126`), `R8Unorm` Y + `Rg8Unorm` UV + a filtering sampler, `nv12_bgl` (`gpu.rs:453-487`),
  the `scene_nv12` pipeline (`gpu.rs:680-704`), `upload_nv12_reusable` (`gpu.rs:1283-1381`) over the
  `StagingUpload` ring, `ColorUniform::new_nv12` (`gpu.rs:334-343`), the `ReuseSlot` (`gpu.rs:1695`),
  and a golden test (`offscreen_nv12_matches_the_cpu_reference`, `gpu.rs:3367`). Fed **only** by the
  Windows MF producer today. It is **SDR-8-bit only** (BT.601/709/2020 matrices, no transfer decode).
- **The fp16 scRGB scene + present pipeline** (`INTERMEDIATE_FORMAT = Rgba16Float`, `gpu.rs:22`): the
  present pass tone-maps SDR (extended-Reinhard, per-image `peak`) or passes through HDR/EDR. Video
  feeds this unchanged. `scene_scale(true/false)` selects HDR-passthrough vs SDR-encode.
- **`hw.rs` already yields CPU NV12/P010** from the VideoToolbox/VAAPI surface (`transfer_if_hw`,
  `hw.rs:197-216`) — currently swscaled to RGBA and the planar data discarded.
- **CPU references the shader must match:** `pb-decode/src/yuv.rs` (fuzzed YUV→RGB bit-reference —
  Bt601/709/2020, limited/full, 8/10/12-bit) and **`pb-decode/src/ffmpeg/color.rs`** (`pq_to_scrgb`,
  `hlg_to_scrgb`, `linear_primaries_matrix`, `SDR_WHITE_NITS = 203`). ⚠ The EOTFs live in
  `ffmpeg/color.rs`, **not** `pb-decode/src/color.rs` (v1 cited the wrong file). Both EOTFs
  `e.clamp(0.0,1.0)` their **encoded input**, then expand to scene-linear (PQ → up to `10000/203 ≈
  49.3`; HLG bakes a **1000-nit** OOTF peak). The shader must reproduce this exactly.
- **Lifecycle facts that constrain the design (verified):**
  - `convert.rs` (`:30-40`) explicitly defers pixel format/color to the first frame: construction
    records the *plan* (geometry, rotation, HDR-or-not from **decoder-reported** transfer); `resolved:
    Option<SourceColor>` and the scaler are set on the first `convert()`.
  - `video_producer.rs:123` sends `Opened { frame_bytes: output_format().frame_bytes(...) }`
    **before the credit/decode loop runs** — i.e. before any frame is decoded.
  - `video_session.rs` (`~:787`) treats `Opened.frame_bytes` as the negotiated constant size and
    charges the byte budget from it; it queues frames **without** calling `is_well_formed`, and
    presentation uses `split_at` (panics on a short/malformed planar buffer).
  - `set_scaler_colorspace` (`ffmpeg/color.rs:172`) is hard-configured **YUV→full-range-RGB**: dst
    coefficients `SWS_CS_DEFAULT`, dst range forced full. Not reusable for planar output.
  - `StagingUpload` pool is bounded at `MAX_STAGING_POOL = 3` (`upload.rs:33`); `UploadStrategy::upload`
    is one call (one encoder+submit) per band → the NV12 path submits **twice per frame**.

## Decisions (locked; Codex-reviewed)

1. **Fitted swscale output for v1** — swscale scales YUV→NV12/P010 at the fitted size (scale + chroma
   only), tight-packed like the MF NV12 frame; the GPU does all color. Keeps the tight display-fitted
   `VideoFrame`; removes the measured R6/R8 bottleneck with the smallest scope. Coded-res + GPU-scale
   is deferred unless swscale shows up in the new trace.
2. **Precision and transfer are independent axes** (Codex P0). A typed `Planar420Format::{Nv12, P010Le}`
   (storage precision) *and* a typed `VideoTransfer::{SrgbLike, Parametric, Pq, Hlg}` (shader transfer
   mode). **10-bit SDR → P010 + SrgbLike/Parametric** (not NV12 — don't quantize 10-bit SDR to 8).
   12-bit input → P010 with a **tested rounding** policy (or fallback). Renderer never reads raw CICP
   integers; it reads the typed contract.
3. **The planar fast path is eligibility-gated on the *actual software frame*** (post-HW-transfer),
   not decoder metadata: 4:2:0 pixel descriptor, **no alpha**, an implemented matrix, even output
   geometry, plus (P010) adapter `TEXTURE_FORMAT_16BIT_NORM`. **Rotation is handled in geometry/UV,
   not gated out** (portrait phone video is core media, Codex P1). **SAR≠1 is gated for v1** → CPU
   fallback. Ineligible clips take the **existing parallel CPU RGBA/fp16 fallback — which stays
   parallel, never demoted to serial.**
4. **HDR peak is metadata-first + a stable default** — parse frame/stream side-data (MaxCLL /
   mastering max-luminance), `peak = nits / 203`, default **1000 nits** for PQ/HLG when absent. This
   is **new parsing** (the FFmpeg video path has none today; v1's "already parsed for stills" was
   wrong). Static metadata does **not** decay/reset on seek (only an adaptive estimator would; none is
   built). No GPU histogram in this phase.
5. **One generalized planar path** — `Planar420Format` + a `PlanarParams` uniform + one bind-group
   layout + one uniform-driven shader entry + one two-plane upload; `SceneKind::{Rgba, Planar420}`
   replaces the `scene_is_nv12` bool; `ReuseSlot` gets an explicit slot-kind (no texture-format
   inference). NV12 is folded into this, not cloned.
6. **Golden tests read back the fp16 scene intermediate and compare against an *independent*
   reference** (zscale/libplacebo vectors), not only against our own CPU functions (Codex P0/testing).
   No `nv-flip` in the repo — per-value float tolerance on the fp16 readback.

## Design

### 2A. Frame + protocol contract (negotiation)

The core fix: **you cannot pick the output format before the first decoded frame** (verified above).

- **Typed options in, not a bool:** thread `RendererCapabilities` (does it support the planar GPU
  path? `TEXTURE_FORMAT_16BIT_NORM`? is the `PB_VIDEO_CPU_CONVERT` A/B escape hatch active?) into the
  producer as a typed `VideoProducerOptions`.
- **Negotiate on the first frame.** Decode + **retain** the first raw frame, run HW transfer, then
  finalize the output format/geometry/transfer/matrix/range from the *actual software frame*. Send
  `Opened` with the real, negotiated `frame_bytes`. Publish the retained first frame only after the
  first credit arrives. (Either extend the existing lazy-`convert` to drive this, or add a distinct
  `Negotiated` event — but **never budget credits against an unresolved format**.)
- **Lock for the session:** output `Planar420Format`, dims, `VideoTransfer`, matrix, range, peak.
  A material midstream change fails cleanly or explicitly renegotiates after a flush — never silent
  stale geometry (the current "mid-stream change is a clean failure" contract, extended to color).
- **Validate every frame** before queueing: `is_well_formed` + checked plane-offset math. Add
  `PixelFormat::{P010}` with **checked** `frame_bytes`/plane-offset helpers (no unchecked
  `w*h*mult`); `VideoSession` must reject a malformed frame instead of `split_at`-panicking at present.
- **New frame fields as needed:** storage dims + display rotation for the geometry/UV rotation
  (2C); a typed `VideoTransfer` + `Planar420Format`; `VideoColorInfo` describing the **destination**
  pixels (see 2B). Keep the buffer tight; no per-plane strides needed (swscale-to-fit is contiguous).

### 2B. Producer emits planar

- **New, separate planar swscale config** (do **not** reuse `set_scaler_colorspace`): a named
  `set_planar_scaler_colorspace` where **src and dst coefficients are both the selected output
  matrix** and **dst range is explicit** (preserve the resolved *source* range). The resulting
  `VideoColorInfo` must describe the **destination** pixels (matrix/range/transfer), because that's
  what the shader will invert. Keep the two configs in separate named functions so their contracts
  can't be confused.
- **`convert.rs` planar arm:** swscale YUV→`Pixel::NV12`/`P010LE` at the fitted `(out_w,out_h)` (scale
  + chroma reposition only — **no** YUV→RGB, **no** `pack_scrgb_f16`). Prove "scale+chroma only" is
  actually cheaper by benchmark and by asserting code values (below), not by assuming it from the
  output pixel format. Tight-pack via a new checked `tight_planar()` honoring both plane strides.
- **Crop:** apply `AVFrame` crop (`crop_top/bottom/left/right`) — feed swscale a cropped source view.
  The current converter validates dims but doesn't visibly account for crop; the planar path must.
- **`make_frame` color plumbing** (`video_producer.rs:~577`): build `VideoColorInfo` from the
  destination matrix/range + typed transfer + metadata peak (2D) — mirroring the MF NV12 plumbing.
- **Output-code assertions** (correctness, not perf): assert the packed scaler output for legal/full
  **black, white, neutral chroma, and a saturated primary** matches expected code values directly — so
  a mislabeled range/matrix is caught at the packer, before the shader.

### 2C. GPU planar path (generalized, uniform-driven)

- **Textures:** NV12 keeps `R8Unorm`/`Rg8Unorm`; P010 uses `R16Unorm`/`Rg16Unorm`. The latter needs
  `wgpu::Features::TEXTURE_FORMAT_16BIT_NORM` (currently `required_features: Features::empty()`,
  `gpu.rs:1512`). **Request it when the adapter advertises it; on device-creation failure, retry
  without it** and report the capability as false so the producer falls back to CPU. Both 8- and
  16-bit formats satisfy the existing filterable-float BGL shape → **one `planar_bgl`**.
- **One shader entry** (`fs_scene_planar`), driven by a `PlanarParams` uniform — all normalization
  constants generated by **pure-Rust helpers with unit tests** (no approximate WGSL literals):
  1. **Range expand → YUV.** Constants depend on storage precision. For high-aligned **P010** an
     `R16Unorm` sample is `(code10<<6)/65535`, so recover with **`65535/64`, not `1023`**:
     - limited Y: `(s − 4096/65535) · 65535/(876·64)`
     - limited C: `(s − 32768/65535) · 65535/(896·64)`
     - full Y: `s · 65535/(1023·64)`
     - full C: `(s − 32768/65535) · 65535/(1023·64)`
     8-bit NV12 keeps the existing `/219`,`/224` (limited) constants. Apply the matrix coefficients
     already packed by `ColorUniform`; **must match `yuv.rs`** (incl. its 10-bit path).
  2. **Transfer → scene-linear**, selected by `VideoTransfer`:
     - `Pq` — new WGSL matching `pq_to_scrgb` (incl. `·10000/203`).
     - `Hlg` — new WGSL matching `hlg_to_scrgb` (incl. the baked **1000-nit** OOTF).
     - `SrgbLike`/`Parametric` — the existing sRGB/parametric path (for 10-bit SDR P010).
  3. **Primaries → scRGB/709** 3×3 (match `linear_primaries_matrix`).
  4. **Output scale** (`cx.scale.x`) — as the other scene modes.
  - **Clamp discipline (corrected):** keep the encoded `[0,1]` clamp **before** the EOTF (the CPU
    functions clamp their input); **no clamp after** the EOTF or the primaries matrix — the fp16
    scene holds the >1.0 wide-gamut/HDR values. v1's "relax the clamp" was wrong.
  - **Matrix gating:** allow only explicitly-implemented matrix codes; do **not** collapse BT.2020
    non-constant (9) and constant-luminance (10) into one set — code 10 ≠ the code-9 Kr/Kb path.
    Unknown/unimplemented → CPU fallback, not a silent BT.709 guess.
- **`SceneKind::{Rgba, Planar420}`** replaces `scene_is_nv12`; `ReuseSlot` gets an explicit slot kind.
- **Two-plane single-submission upload:** record both `copy_buffer_to_texture` copies in **one**
  encoder/submit (today NV12 submits twice/frame); size the staging pool for two planes in rotation
  (raise `MAX_STAGING_POOL` if the counters show misses).
- **Rotation in geometry/UV:** apply display rotation in the quad/UV mapping (storage dims + rotation
  carried on the frame) so portrait phone video takes the planar path — **not** gated to the CPU
  fallback.
- **Typed present arg:** a `PlanarPresentation` (format, transfer, params, peak, hdr flag) instead of
  a long parameter list. `set_video_planar` must: call `scene_scale(true)` for PQ/HLG and
  `scene_scale(false)` for SDR; set the present **peak from `VideoColorInfo.peak`** (NV12 currently
  hard-codes peak 1); and define behavior for non-wgpu renderers + the `PB_VIDEO_CPU_CONVERT` hatch.

### 2D. HDR peak metadata (new work)

- Parse **frame side-data first, then stream/codec side-data**: MaxCLL and mastering-display
  max-luminance, with unit + validity checks (reject 0/NaN/absurd/contradictory).
- `peak = nits / 203`; documented default **1000 nits** for PQ/HLG when metadata is absent. HLG's
  1000-nit OOTF is intrinsic (2C) — the metadata peak still drives SDR tone-mapping of HLG.
- Static metadata → **no decay/reset on seek** (remove v1's wording). No GPU histogram this phase.
- Fixture tests: present / missing / malformed / conflicting metadata.

### 2E. Retire R8 on the fast path (keep the parallel fallback)

Once the planar path passes gates, `pack_scrgb_f16` + its scoped-thread fan-out (`convert.rs:257-307`)
is **not run on the planar path**. It **stays for (a) the ineligible-clip fallback (still parallel —
do not demote to serial; a persistent worker pool is acceptable) and (b) the stills HDR path**
(`common::finalize_hdr_scrgb`, unchanged). Do not remove the performant fallback until portrait
SDR/HDR clips meet the playback gates.

## Correctness gates

- **Two automated layers:** (1) read back the **fp16 scene intermediate** and compare float values
  (essential for >1.0 and negative wide-gamut components — an RGBA8 final can't prove HDR); (2)
  separately test **final SDR tone-mapping** with the metadata-derived peak.
- **Independent reference vectors are mandatory** (committed, generated with zscale/libplacebo — not
  self-comparison): PQ at 100/203/1000/10000 nits; HLG gray + primary ramps; legal/full black+white;
  BT.601/709/2020 non-constant matrices; spatial chroma edges + a chroma zone plate; 10-bit SDR P010;
  metadata missing/malformed; and unsupported matrix/alpha/4:2:2/4:4:4/crop/SAR fallback cases.
- **Chroma-siting decision (required):** the shipped NV12 golden uses uniform chroma, dodging the
  CPU-nearest vs GPU-bilinear difference. Either normalize swscale output to a documented destination
  siting, or carry a typed chroma location and offset the UV coordinates — pick one and test it.
- **Producer↔session tests:** negotiated `Opened.frame_bytes` matches every emitted frame; a
  malformed frame fails without a present panic; seek preserves format+metadata; capability=false
  selects fallback; optional-feature device-request failure retries without P010 and stays usable;
  first-frame and post-seek present use the correct peak + HDR scale.
- **CI:** the P010 GPU test must not silently skip on every adapter — require at least one known-capable
  runner; on unsupported adapters, run and assert the **fallback** path. Prove WARP/lavapipe behavior.
- Privacy/no-trace tests stay green (read-only, RAM-only).

## Performance gates (executable thresholds)

- Record the exact corpus frame rate; require a named minimum **decode/convert margin ≥ 1.5×
  sustained for 4K30**, plus **zero starvation/audio pauses** over the long run.
- Per-stage **p50/p95/p99** for: HW transfer, swscale, tight packing, staging upload, scene draw,
  total decode-to-present.
- `net_decode_throughput` gets an explicit **force-planar-capable** config (a headless bench has no
  renderer capability and would otherwise keep measuring the fallback).
- Matrix: portrait rotation, 10-bit SDR, missing metadata, software decode, VideoToolbox, VAAPI.
- **Allocation counters** (the zero-alloc claim is corrected): FFmpeg dst-frame allocations, planar
  `Vec` allocs/reuses, staging hits/misses, GPU texture/BGL creations, **submissions per presented
  frame**. v1 gate wording is corrected to **"no full RGBA/fp16 allocation and no per-frame OS-thread
  creation"**; a bounded planar `Vec` pool + reusable FFmpeg output frames + a two-plane single-submit
  upload are the path to true zero-alloc (land as pooling once counters justify).
- Retain existing gates: 10-min playback, seek-spam, EOS parking, stop/navigation, archive-bytes,
  privacy, audio-continuity.
- **CHANGELOG** `[Unreleased]` entry when it lands (user-facing: smoother 4K HDR).

## Fallback matrix (v1)

| Condition | Path |
|---|---|
| 4:2:0, no alpha, implemented matrix, even dims, (P010) 16-bit-norm adapter, SAR==1 | **Planar GPU** (rotation handled in UV) |
| SAR≠1, 4:2:2/4:4:4, alpha, unimplemented matrix, no-16bit-norm adapter, non-wgpu renderer, `PB_VIDEO_CPU_CONVERT` | **CPU RGBA/fp16 fallback (parallel)** |

## Test-first phases

1. **Contract:** `PixelFormat::P010` + checked `frame_bytes`/plane-offset/`is_well_formed`; typed
   `Planar420Format`, `VideoTransfer`, `PlanarPresentation`; `VideoSession` rejects malformed frames.
   Pure unit + fuzz tests. (pb-decode / pb-app-core.)
2. **Negotiation:** retain-first-frame → `Opened` with real geometry; typed `VideoProducerOptions`
   from `RendererCapabilities`; producer↔session tests (frame_bytes match, capability=false→fallback).
3. **GPU planar shader + textures + goldens** (synthetic planes, no producer): 16-bit-norm feature
   (query + retry-without), generalized `planar_bgl`/`fs_scene_planar`/`PlanarParams` (Rust-generated
   constants, unit-tested), `SceneKind`, explicit `ReuseSlot` kind, two-plane single-submit upload,
   `set_video_planar` (scene_scale + peak). fp16-readback goldens vs **independent** vectors across the
   matrix/range/transfer grid. This is the bulk of new code and is testable in isolation.
4. **HDR peak metadata:** frame+stream side-data parse, precedence, `nits/203`, default 1000, fixtures.
5. **Producer emits planar:** separate planar swscale config, crop, `tight_planar`, code-value
   asserts, eligibility on the actual sw frame; NV12/SDR first (render path exists), then P010/HDR;
   rotation via geometry. Owner-verify on the corpus.
6. **Retire R8 on the fast path** (parallel fallback + stills retained); re-measure 0D; perf gates.

## File map

- **pb-decode:** `PixelFormat::P010` + checked helpers (`lib.rs`); `is_well_formed` (`video.rs`);
  `convert.rs` planar arm + `tight_planar` + crop + code-value tests; `ffmpeg/color.rs`
  `set_planar_scaler_colorspace` + matrix gating + PQ/HLG constant helpers; `video_producer.rs`
  negotiation + `make_frame` planar + metadata peak; a CPU `p010_to_rgba` reference near `yuv.rs`.
  Update still-only branches in **`thumb.rs`, `thumbs.rs`** — or centralize an `is_planar_video()`.
- **pb-render:** `fs_scene_planar` + PQ/HLG WGSL; `R16Unorm`/`Rg16Unorm`; `planar_bgl`/`scene_planar`
  pipeline; generalized two-plane upload (`upload.rs`, one submit); `SceneKind`; `ReuseSlot` kind;
  `set_video_planar`(`PlanarPresentation`); feature request + retry + capability bool;
  `OffscreenSource::Planar` + `render_offscreen_planar`; fp16-readback goldens; allocation counters.
- **pb-app-core:** `contract.rs` typed `VideoProducerOptions`/`Planar420Format`/`VideoTransfer`;
  `present_video_frame` planar dispatch; `video_session.rs` frame validation + peak/HDR-scale
  threading; capability plumbing to producer; update the still-only branch in **`engine.rs`** or the
  centralized `is_planar_video()`.

## Non-goals (v1)

- **Zero-copy** IOSurface/`CVPixelBuffer`→wgpu import (Phase 4 escalation, only if readback+upload is
  proven the limiter after this).
- **SAR≠1 (anamorphic)** — CPU fallback (rotation is *not* a non-goal; it's handled in geometry).
- **DoVi dynamic / HDR10+ dynamic metadata** — HDR10-compatible **static PQ base layer** only; never
  claim "DoVi correct."
- **Coded-res + GPU-scale contract** — deferred (swscale-to-fit for v1).
- **Windows MF changes** — MF already ships NV12; MF P010 is a separate follow-on.
- **GPU histogram/adaptive peak** — metadata + stable default only.

## Resolutions to v1's open questions (Codex answers, recorded)

1. **Scale strategy** — fitted swscale for v1 (smaller scope, removes the measured bottleneck);
   preserve matrix/range, standardize chroma siting; escalate to coded-res GPU scale only if swscale
   appears in the new trace.
2. **Rotation/SAR** — SAR gating OK initially; rotation gating is only an intermediate slice, **not**
   the end state (portrait phone video is core); keep the parallel fallback until rotated clips pass.
   → this plan handles rotation in geometry/UV.
3. **16-bit-norm availability** — capability-gate is the right shape; wgpu 22 exposes it on Metal +
   DX12, Vulkan is adapter-dependent; query at runtime, **retry device creation without it on
   failure**, prove WARP/lavapipe in CI.
4. **HDR peak** — metadata-first + stable default; **no** adaptive GPU estimation this phase; normalize
   `1.0 = 203 nits`.
5. **Golden reference** — an **independent** reference (zscale/libplacebo) is mandatory; matching
   `yuv.rs`/CPU HDR functions proves regression-equivalence, not correctness.
6. **Range constants** — one generalized planar shader/BGL, uniform-driven normalization; keep the
   encoded `[0,1]` clamp before PQ/HLG, no post-EOTF clamp; use **`65535/64`**, not `1023`.
