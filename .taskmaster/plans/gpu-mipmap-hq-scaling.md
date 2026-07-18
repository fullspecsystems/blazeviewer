# Plan — High-quality GPU image scaling (mipmaps + trilinear), and retire the resize re-decode

**Status:** draft (pre-Codex-review) · **Owner track:** rendering quality / #106.7 §6 follow-on
· **Author:** 2026-07-18

## 1. Problem

Fitting a photo to the window is almost always a **downscale** (a 24 MP / 6000×4000
photo into, say, a 3440×1440 window is a ~2.8× reduction). The renderer samples the image
texture with **plain bilinear and no mipmaps**:

- `upload_image` (`crates/pb-render/src/gpu.rs:1272`) creates the texture with
  `mip_level_count: 1`.
- The sampler (`gpu.rs:1288`) is `mag=Linear, min=Linear, mipmap_filter=Linear` — but with a
  single mip level, `mipmap_filter` does nothing.
- The scene shader samples with `textureSample(tex, samp, in.uv)` (`SCENE_WGSL`,
  `gpu.rs:107`).

Bilinear reads only a 2×2 texel neighbourhood per output pixel, so at a 2–3× downscale it
**undersamples** the source — the soft, faintly-aliased look the owner sees the instant a
fullscreen toggle GPU-scales the frame. The code itself flags this as deferred
(`gpu.rs:1286`: *"Crisp high-ratio downscaling via mipmaps/Lanczos is a later quality pass"*).

To route around the weak GPU downscale, the app pays for a **CPU Lanczos decode-to-fit** on
every geometry change: on a resize/fullscreen the ring is invalidated and the current photo is
re-decoded at the new fit (over SMB that's a full re-read of the file — the owner's ~1 s wait),
and the Fit↔1:1 toggle keeps *both* representations resident so a toggle is a pure rebind
(#106.7). We just added an instant-Original rebind for resize (`bcd37ea6` + `792dfa9e`), which
removes the EXIF-preview flash and the stuck pie, but the instant frame is still the
**bilinear GPU downscale** of the full-res Original, and the ~1 s Lanczos re-decode + a subtle
bilinear→Lanczos swap remain.

## 2. Goal

1. **Make GPU downscaling near-Lanczos quality** by generating a mipmap chain on upload and
   engaging the already-configured trilinear sampler.
2. **Retire the resize/fullscreen re-decode**: with a high-quality GPU downscale, serving the
   fit from the resident full-res **Original** is good enough, so a resize becomes a pure
   rebind (like the `0` toggle) — instant, sharp, no ~1 s wait, no swap-flicker.

Non-goals: changing the CPU decode-to-fit path for the *first* frame of a photo (still Lanczos
via `common::finalize`); zoom/1:1 quality (already true-1:1); video/planar (NV12) sampling.

## 3. Current state (anchors)

- `upload_image` (`gpu.rs:1230`) — the one texture-upload path; builds the texture + sampler +
  color uniform + bind group. Used by the ring (`upload_slot`), `set_image`, offscreen, egui.
  Its reusable twin `upload_image_reusable` (`gpu.rs:1328`) re-uploads into an existing texture
  for animation/video frames.
- `clamp_to_max` (`gpu.rs:~1258`) — huge SDR images are CPU-downscaled to the GPU max dimension
  before upload; HDR fp16 is already fit-sized in the decoder.
- Texture formats: SDR = `Rgba8Unorm` (source-encoded; a `mode` flag drives the in-shader color
  transform), HDR = `Rgba16Float` (scene-linear).
- Scene shader `SCENE_WGSL` (`gpu.rs:29`), fragment at `gpu.rs:105`, samples `textureSample`.
- `resize` (`app_core_impl.rs:1347`) — rebinds the Original (part A, `bcd37ea6`), sets
  `resize_hold`, defers the crisp re-decode 180 ms (`resize_settle_at`).
- `refresh_after_geometry_change` (`app_core_impl.rs:~5342`) — the settle re-decode trigger.
- `drain_results` (`app_core_impl.rs:~6640`) — the `resize_hold` quality-monotonic preview
  guard + `mark_resolved` (`792dfa9e`).
- Ring VRAM budget: `RING_BUDGET_BYTES`, `slot_bytes_estimate` (`app_core_impl.rs`),
  `ring_capacity`.
- Golden/offscreen harness: `offscreen_letterboxes_and_draws_image` (`gpu.rs:4595`) and the
  NV12 offscreen tests render headless to a buffer and read back; `nv-flip` is available
  (`crates/pb-render/Cargo.toml`) for perceptual diffs.

## 4. Design

### (A) Mipmap generation on upload

Create the texture with a full chain and generate it once per upload:

- `mip_level_count = 1 + floor(log2(max(img_w, img_h)))`.
- Add `RENDER_ATTACHMENT` to the texture usage (needed to render into each mip level).
- A dedicated **mip-gen pipeline** (its own tiny WGSL, or reuse a fullscreen-triangle vertex +
  a downsample fragment): for `level` in `1..N`, begin a render pass targeting `level`'s view,
  bind a texture view of `level-1` + a Linear-clamp sampler, draw the fullscreen triangle. One
  encoder, N-1 passes, submitted with the upload.
- This is off the keypress frame — uploads happen during **prefetch** — so the cost is paid on
  spare workers, never on a present (the architecture's contract). Measure it anyway
  (`PB_PERF`/a Criterion micro-bench) to confirm it doesn't starve the on-screen sharpen.

**Quality tiers for the downsample kernel** (start simple, escalate only if a golden test
demands it):

1. **Box / bilinear mip-gen** (each level = a 2×2 average of the previous). Simple, standard
   trilinear. Already a large step up from single-level bilinear. Ship this first.
2. **Wide kernel mip-gen** (e.g. a separable Kaiser/Lanczos-3, 13-tap) if tier 1 isn't sharp
   enough vs CPU Lanczos in the golden test. More shader work; behind the same seam.

### (B) Colour-space correctness of mip generation

Downsampling must happen in **linear** light or the mips are subtly wrong (dark fringing on
high-contrast edges):

- **HDR `Rgba16Float`** is already scene-linear → downsample directly. ✔
- **SDR `Rgba8Unorm`** holds *source-encoded* (≈sRGB/gamma) data that the scene shader converts
  later via the `mode` flag. Averaging encoded values is technically wrong. Options, cheapest
  first: (i) accept it — at photographic content + these ratios the error is small and it's
  still far better than undersampled bilinear; (ii) mip-gen fragment linearizes → averages →
  re-encodes (sRGB approx) so the stored mip matches how L0 is interpreted; (iii) upload SDR as
  `Rgba8UnormSrgb` so the hardware linearizes on sample (but that collides with the existing
  `mode`-flag colour path — likely off the table). **Recommendation:** ship (i), and make the
  mip-gen fragment do (ii) if the golden test shows visible fringing. Decide with Codex.

### (C) Sampler — no shader change

The sampler already declares `mipmap_filter: Linear`; with mips present, `textureSample`
auto-selects the level from screen-space UV derivatives → trilinear. `SCENE_WGSL` is
unchanged. (Confirm the scene sampler is the one on the image bind group at `gpu.rs:1288`, not
the present/overlay/egui samplers, which don't need mips.)

### (D) Retire the resize/fullscreen re-decode

With a high-quality GPU downscale, serving the current photo from its resident full-res
**Original** across a resize is near-Lanczos. So:

- On resize, keep the current behaviour of **rebinding the Original** (part A), but **do not
  re-decode the current item's Fit** — no settle re-decode for it, no swap, no ~1 s wait, no
  flicker. It becomes a pure rebind like the `0` toggle.
- **Neighbours**: their resident Fit slots are now stale-sized. Either (a) still re-decode
  neighbours at the new fit so nav is crisp (keep the settle for neighbours, drop it for the
  current item), or (b) also serve neighbours from their Originals when resident (radius) and
  only re-decode the ones without an Original. Prefer (a) for a smaller change; revisit (b) if
  nav-after-resize softness is noticeable.
- **Simplify `resize_hold`**: if the current item isn't re-decoded, there's no settle preview
  to skip — the quality-monotonic guard + the `792dfa9e` `mark_resolved` may collapse into
  "rebind the Original + `mark_resolved` at the new epoch, don't re-decode it." Re-derive the
  exact state machine once (A) lands; keep the fallback (no Original resident → old re-decode
  path) intact.
- **Fallback unchanged**: radius 0 / just-blazed / excluded items (RAW/SVG/video/gigapixel) with
  no resident Original still take the old upscale-then-re-decode path.

### (E) VRAM budget

Mips add ~33% per texture (`1 + 1/4 + 1/16 + … = 4/3`). Update `slot_bytes_estimate` /
`RING_BUDGET_BYTES` accounting so the ring capacity math still holds — a resident ring of full
chains must stay within the VRAM budget (esp. with the parked full-res Original tier, which is
the largest). Confirm the gigapixel ceiling still bounds the worst case.

## 5. Phases

1. **Mip-gen + trilinear (the quality win).** Tier-1 (box) mip-gen in `upload_image` (+
   `upload_image_reusable`), usage flag, VRAM accounting. Golden test. Measure upload cost. This
   alone makes the *instant* fullscreen frame sharp.
2. **Retire the resize re-decode.** Serve the current item from the Original, drop its settle
   re-decode, simplify `resize_hold`. Result: fullscreen = instant + sharp + smooth, matching
   the `0` toggle.
3. **(Optional) High-quality mip kernel (tier 2)** only if the golden test shows tier-1 mips are
   visibly softer than CPU Lanczos at target ratios.

## 6. Acceptance / tests

- **Golden image (headless, no GPU-vendor dependence via WARP/lavapipe):** render a
  high-frequency pattern (fine lines / a zone plate) downscaled ~3×; compare `bilinear-no-mips`
  vs `trilinear+mips` vs a **CPU Lanczos reference** with `nv-flip`; assert `trilinear+mips` is
  perceptually closer to Lanczos than bilinear, and below a tolerance. Extend
  `offscreen_letterboxes_and_draws_image` (`gpu.rs:4595`).
- **VRAM:** a unit assertion that `slot_bytes_estimate` includes the mip overhead and the ring
  capacity stays within `RING_BUDGET_BYTES` at the pro-range sizes (#106.7 §9 ceiling).
- **Hot path:** `present` stays a rebind (no per-keypress mip-gen); mip-gen only on upload.
  `PB_PERF` `resize→on-screen` for a resident Original becomes ~0 ms (rebind) with no re-decode.
- **Manual (owner):** fullscreen toggle on a large archive photo is instant + sharp, no ~1 s
  reload, no flicker; nav after a resize is crisp; zoom/1:1 unaffected; HDR photo still correct;
  a huge (clamped) panorama still uploads and looks right; video/animation frames unaffected.

## 7. Risks / open questions (for Codex)

1. **sRGB mip-gen correctness** (§B) — ship the naive average or do linearize→average→encode?
2. **VRAM +33%** — does the ring budget / gigapixel ceiling still hold with full chains for the
   parked Original tier? Any capacity regressions?
3. **Upload cost** — N-1 render passes per upload; acceptable on prefetch workers, or should
   mip-gen be capped/skipped above some size, or only generated for the *display* rep?
4. **`upload_image_reusable`** (animation/video frames): should per-frame animation textures get
   mips at all (they're re-uploaded every frame; mip-gen per frame may be wasteful)? Probably
   **skip mips for the reusable/animation path**, mips only for still photos. Confirm.
5. **Interaction with `clamp_to_max`** and with the fp16 HDR path.
6. **Retiring the re-decode (§D)** — is serving the current item permanently from the Original
   (bilinear-of-mips) acceptable vs a Lanczos Fit, or should we still re-decode a Lanczos Fit in
   the background and swap it *seamlessly*? The owner explicitly prefers smoothness; confirm the
   quality is genuinely indistinguishable at target ratios before dropping the re-decode.
7. Anything the plan misses in the present/tonemap or the `mode`-flag colour path.
