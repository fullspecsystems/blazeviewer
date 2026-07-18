# Plan — High-quality GPU image scaling (mipmaps + trilinear), and retire the resize re-decode

**Status:** rev2 (Codex-reviewed 2026-07-18) · **Track:** rendering quality / #106.7 §6 follow-on

## 0. Test findings — Phase 1 shipped + a surface-bug breakthrough (owner, 2026-07-18)

**Phase 1 (mipmaps + trilinear) shipped (`d82df25f`) and owner-tested.** The *instant* fullscreen
frame is visibly better, BUT the re-decode swap is **still clearly visible on high-frequency
content** (grass, detailed fabric/patterns) — confirming Codex's warning that **box-mip trilinear
≠ Lanczos-3**. So Phase D **cannot** just serve the mipped Original; it needs the escalation:
**GPU-derive an exact-size Lanczos/Kaiser fit from the mipped Original texture** (task #110). The
plain-mip path alone doesn't retire the re-decode.

**BREAKTHROUGH: the "photo stuck on a blurry preview" and the "door card frozen over a photo" are
the SAME root — the GPU surface DROPPING PRESENTS — not the sharpen/decode logic and not the
door/deck logic.** Proven by a `PB_SHARP_DIAG` + `PB_DOOR_DIAG` capture on a plain **folder**
(source-independent) at `sharp-diag.log`:
- Sharpen works: **12/12** `full landed → UPGRADE (sharpen applied)`; the lone `NO sharpen` was a
  correct `held_nav=true` (mid-blaze). The core DOES decode + apply the full.
- The surface dropped **17** presents (`render: surface lost/outdated — … present_mode=Mailbox —
  frame dropped`) + 40 frames drawn from `Held`, while the surface **config size fluctuated**:
  `1117×882`, `1454×864`, `1454×884`. So a size change → dropped present; when a **sharpen** (or a
  door frame) present is the one dropped, that frame never reaches the screen → stuck on the old
  (blurry / wrong) frame until a resize/switch forces a fresh present.
- My size-drift heal (`heal_surface_if_dropped`, `8b5dc30b`) **never fired** (no `surface heal` /
  `size OK` lines) — the drops recover on the per-tick retry once the size settles, so
  `redraw_pending` clears before the heal checks; a present dropped *mid-fluctuation* is stranded.
- 🔎 Suspicious: `1454×884` ↔ `1454×864` is a **20px oscillation** — something may be toggling a
  ~20px inset (docked toolbar / menu / info line?) and churning the surface avoidably. Worth
  checking whether the client area is oscillating on its own (an app bug) vs the owner resizing.

**⇒ The critical path is now surface robustness, NOT this scaling plan.** The surface-recreation /
Lost-vs-Outdated split + resize-churn handling (Codex's DX12 guidance; task #110 §, and the
[[archive-card-over-photo-bug]] memory's reserve fix) will collapse a whole class of "stuck" bugs
at once (stuck-blurry, frozen-door, "titles advance but view frozen"). The mipmap Phase D and VRAM
work stay queued behind it. Corpus for repro: `D:\Media\Pictures\…\Gill & JD's Wedding` (a folder;
the archive was ruled out).

## 1. Problem

Fitting a photo to the window is almost always a **downscale** (a 24 MP / 6000×4000 photo into
a 3440×1440 window is ~2.8×). The renderer samples the image texture with **plain bilinear, no
mipmaps** (`crates/pb-render/src/gpu.rs:1272` `mip_level_count: 1`; sampler `gpu.rs:1288`; scene
shader `SCENE_WGSL` samples `textureSample` at `gpu.rs:107`). Bilinear reads only a 2×2 texel
neighbourhood, so at 2–3× it **undersamples** — the soft/aliased look the owner sees the instant
a fullscreen toggle GPU-scales the frame (`gpu.rs:1286` already flags this as deferred).

To route around it, the app CPU-Lanczos-re-decodes the fit on every resize/fullscreen (over SMB
a full re-read — the ~1 s wait), keeps both reps resident for the `0` toggle (#106.7), and we now
also rebind the resident full-res Original for an instant frame (`bcd37ea6`/`792dfa9e`). But that
instant frame is still the **bilinear** GPU downscale of the Original, so the ~1 s Lanczos
re-decode + a bilinear→Lanczos swap-flicker remain.

## 2. Goal (and the corrected, phased shape)

1. **Phase 1 — near-Lanczos GPU downscaling:** generate mipmaps on upload (correctly: linear
   light, premultiplied alpha, odd-dim-safe) for the textures that are GPU-downscaled, and let
   the already-configured trilinear sampler use them. This alone makes the *instant* fullscreen
   frame sharp, so the subsequent Lanczos swap becomes near-invisible.
2. **Phase D — retire the resize re-decode (CONDITIONAL, deferred):** box-mip trilinear is much
   better than mipless bilinear but **not** equal to Lanczos-3 (Codex). So keep the background
   Lanczos Fit **behind a switch**, A/B it against mipped-Original-only at 2–3×, and remove the
   re-decode only if the owner confirms the swap is genuinely redundant. Implementing it also
   needs a real renderer **retain/remap** API (below) — not a `mark_resolved` tweak.

Non-goals: the first-frame CPU decode-to-fit; upscaling quality (separate — see §7); video/planar
sampling; anisotropic filtering (wrong lever — uniform axis-aligned scale, no perspective).

## 3. Corrected facts (Codex, with anchors)

- **The upload GPU work is NOT on spare workers.** Decodes are off-thread, but finished images
  are *uploaded from the event-loop tick* (`app_core_impl.rs:6531`, ≤2/tick) and mip passes share
  the **same GPU queue as presentation** — two large neighbour chains can sit ahead of a draw.
  Benchmark **GPU duration + queue-to-present latency**, not how fast `upload_slot` returns.
- **`nv-flip` is NOT a dev-dependency yet** (`pb-render/Cargo.toml:32`) — the rev1 claim was
  wrong. Add it, or use another perceptual metric.
- **The offscreen harness** (`gpu.rs:3389`) exercises the real scene+present passes (good) but
  requests `force_fallback_adapter: false` (`gpu.rs:3378`) — it does **not** guarantee
  WARP/lavapipe determinism. Use perceptual tolerances, not byte-exact goldens.
- **`clamp_to_max` uses nearest-neighbour** reduction (`gpu.rs:1206`); mips can't recover detail
  it already destroyed — the "huge clamped panorama looks right" case needs that step redesigned
  (Lanczos/area clamp), tracked separately.
- **Ring VRAM accounting is already wrong**: the ring records `img.pixels.len()` = L0 only
  (`app_core_impl.rs:6669`); with mips, true residency ≈ 4/3×. And a **pre-existing upgrade-budget
  bug**: preview→full uploads then `set_slot_bytes` (`:6672`) without the required
  `make_room_for_upgrade` (`ring.rs:453`).
- **`full_res_eligible` is pixel-count only** (`app_core_impl.rs:5742`), does **not** exclude HDR,
  and HDR bypasses `clamp_to_max` (`gpu.rs:1252`); the 200 MP ceiling (`engine.rs:56`) is not a
  byte ceiling (200 MP HDR + mips ≈ 2.13 GB > the 1.5 GB budget).
- **`upload_image` is shared** by toast/pie/overlay/tree/subtitle uploads (`gpu.rs:2258`, `:2690`)
  and by `upload_image_reusable` (`gpu.rs:1328`, animation/video, re-uploads L0 every frame).
- The renderer's `held` frame is deliberately **outside** the ring budget (`gpu.rs:1872`); a
  `RingSlot` stores only the bind group, not an accessible texture/view (`gpu.rs:1748`).
- Scene texture-sample (`SCENE_WGSL` frag `gpu.rs:105`) does either the sRGB EOTF (mode 0) or the
  source ICC TRC + source-linear→BT.709 matrix (mode 1); HDR fp16 is mode 2, already scene-linear
  (`color.rs`, `ColorUniform`). CPU Lanczos runs on **source-encoded U8x4** *before* the GPU
  colour transform (`common.rs:316`).

## 4. Phase 1 design — correct mip generation

### 4a. A `MipPolicy` seam (do NOT mip every texture)
`upload_image` is generic. Add an explicit policy param (extend the `Renderer::upload_slot` seam,
`lib.rs:224`, which currently has no representation kind):

| Texture path | Policy |
|---|---|
| Parked still `Original`; Fill/Original **display** rep | **Full chain** (the key HQ-downscale source) |
| CPU-Lanczos `Fit` at its natural viewport size | **L0 only** (already prefiltered, shown ~1:1) |
| Preview/thumbnail Fit | L0 only |
| `upload_image_reusable` (animation/video) | **L0 only** — never mip (re-uploads L0/frame → stale lower levels = a correctness bug) |
| Planar video, toast/pie/overlay/tree/subtitle, present/tonemap, egui | L0 only |
| Offscreen tests | explicit mipped/non-mipped variant |

Never *skip mips above a size threshold* — the largest Originals need them most. If VRAM can't
afford a chain, don't promise that Original as an HQ Fit fallback: use the Lanczos Fit instead
(the eligibility decision, §4d).

### 4b. Generation (per-upload GPU blit chain, wgpu 22)
- `mip_level_count = 1 + floor(log2(max(w,h)))` computed **on integers, after `clamp_to_max`**.
- Texture usage += `RENDER_ATTACHMENT`.
- Build the mip pipelines **once** alongside the others (`gpu.rs:558`); need **separate
  `Rgba8Unorm` and `Rgba16Float` pipelines** (target format is part of the pipeline).
- For `level` in `1..N`: one render pass per level. Source view
  `base_mip_level = level-1, mip_level_count = Some(1)`; target view
  `base_mip_level = level, mip_level_count = Some(1)`. **Views must not overlap** (can't bind the
  all-mips view as source while rendering a level). Rely on same-queue ordering (L0 copy →
  mip-gen → scene draws). Record in the upload encoder; the final scene view spans the whole chain.
- **Correct downsample fragment** (not a bilinear tap):
  - Four explicit `textureLoad`s of the L(level-1) texels (handles the average honestly).
  - **Odd dimensions**: `3→1` etc. must not drop the odd texel — clamp/weight the extra
    column/row (a 2- or 3-tap edge case). Test 1×N, 3×3, 5×2.
  - **Linear-light averaging** (Codex §2): naïve encoded averaging is *not* always small — a
    50/50 black/white edge is wrong by a lot. Per mode:
    - **Mode 0**: sRGB-decode → average → sRGB-encode.
    - **Mode 1**: apply the source parametric TRC → average in source-linear → inverse TRC before
      store. (The primaries matrix stays in the scene shader — it commutes with linear averaging.)
      → the mip-gen fragment needs the same TRC params the scene uses (thread the `ColorUniform`/
      TRC through, or generate mips per colour mode).
    - **Mode 2 (HDR fp16)**: already linear → average directly.
  - **Premultiplied-alpha filtering** for straight-alpha sources (the decoded contract is straight
    alpha, `pb-decode/lib.rs:223`): premultiply → average → un-premultiply, or PNG/SVG edges get
    halos.
- A "13-tap Lanczos" mip kernel is **not** a small drop-in (two passes + intermediate; a UNORM
  intermediate clamps negative lobes). Ship the correct linear-light box first; only escalate the
  kernel if the golden test demands it.

### 4c. Sampler / shader — unchanged
The sampler already has `mipmap_filter: Linear` (`gpu.rs:1291`); with mips present,
`textureSample` auto-selects the LOD from screen-space UV derivatives → trilinear. `SCENE_WGSL` is
unchanged. Present/tonemap texture stays single-level (sampled ~1:1, `gpu.rs:169`).

### 4d. VRAM — exact accounting + allocation-aware eligibility
- Record **exact allocation** per slot: `Σ_levels max(1, w>>l)·max(1, h>>l)·bpp` (4 B SDR, 8 B
  fp16), on post-clamp dims — replace the L0-only `img.pixels.len()` byte count.
- Make `full_res_eligible` **allocation-aware** (a byte ceiling by pixel-format + mip policy +
  device-max-dim), not a pixel count; **exclude/limit HDR** (200 MP HDR + mips ≈ 2.13 GB).
- Fix the pre-existing **upgrade-budget** path: call `make_room_for_upgrade` (`ring.rs:453`)
  before `set_slot_bytes`, and wire **renderer eviction notifications** so freeing a core slot
  actually drops the renderer texture (else VRAM isn't reclaimed).
- Don't let a large mipped Original sit in the out-of-budget `held` slot indefinitely after a
  resize (§ Phase D).

## 5. Phase D design — retire the re-decode (deferred, conditional)
Not shippable as a `resize_hold`/`mark_resolved` tweak (Codex): resize always arms the settle
(`app_core_impl.rs:1418`), which invalidates geometry and **replaces the whole core ring**
(`:6858`), and `reserve_ring` keeps only the displayed texture as an out-of-budget `held`
(`gpu.rs:2882`). Requirements:
- A renderer **retain + old-slot→new-slot remap** API (the unused `ResidentRing::drop_fit_slots`,
  `ring.rs:211`, is the core half; there is no renderer half yet).
- A display-selection rule: **"Fit may be satisfied by a mipmapped Original."**
- **Split the settle timer**: "geometry settled" (also resumes paused video/audio + repositions
  overlays, `:1833`) must stay; only "re-decode the current Fit" becomes conditional.
- **Keep the Lanczos Fit behind a switch**; A/B at 1.25–8×; remove the re-decode only if the owner
  confirms it's redundant.
- **Escalation if box mips miss the bar (Codex's preferred fallback):** derive an exact-size
  Lanczos/Kaiser Fit **from the mipped Original GPU texture in one background GPU pass** — no SMB
  re-read, no retained CPU pixels. Needs `RingSlot` to expose a texture/view (`gpu.rs:1748`).

## 6. Golden / correctness tests
- Add `nv-flip` (or a chosen perceptual metric) as a `pb-render` dev-dep.
- **Ratios**: 1.25, 1.5, exact 2.0, 2.2, 2.8, 3.7, and a 6–8× stress case; include **subpixel
  phase offsets** (exact 2× flatters a box chain).
- **Patterns**: chirps/checkerboards, slanted edges, Siemens star / zone plate, fine text and
  1-px diagonals, a natural foliage/fabric crop, black/white **and coloured** edges (gamma),
  **transparent coloured** edges (alpha), and both **profiled SDR** + **fp16 HDR** fixtures.
- Compare **perceptual similarity AND alias energy** (a zone plate alone rewards blur). Two
  references: current **encoded-space Lanczos** (compat) and a **linear-light** reference
  (correctness — the linear-light mip won't match the encoded-space CPU Lanczos exactly).
- A separate **deterministic** test reads back individual generated mip levels and checks odd
  dims, gamma, and alpha directly (no adapter variance).
- Perceptual tolerances (WARP/lavapipe + fp16 + `pow` differ slightly cross-adapter).

## 7. Upscaling (separate, not required here)
Fit **does** magnify small images (view scale isn't capped at 1, `view.rs:105`) while CPU
decode-to-fit never upscales (`common.rs:325`). Mips don't help magnification. Retiring the
re-read doesn't regress small images (the re-decode returns the same L0 anyway). Add a
bicubic/Catmull-Rom magnify path **only if** owner testing shows a need — out of scope for the
downscale fix.

## 8. Phasing / acceptance
- **Phase 1a** — mip-gen pipelines + correct linear-light/premult/odd-dim downsample fragment +
  the `MipPolicy` seam (Original/display-rep full chain; everything else L0). Deterministic
  mip-level readback test + the golden-image suite. **Benchmark GPU pass duration +
  queue-to-present.**
- **Phase 1b** — exact VRAM accounting + allocation-aware (HDR-limited) `full_res_eligible` + the
  `make_room_for_upgrade`/eviction fix.
- **Phase D** — deferred, behind a switch, A/B, needs the retain/remap API. Do **not** rip out the
  Lanczos re-decode until confirmed redundant.
- **Manual (owner):** fullscreen toggle on a large archive photo is instant + sharp (no bilinear
  softness); the residual Lanczos swap is now imperceptible; nav crisp; zoom/1:1 unaffected; HDR
  correct; transparent-edge PNG/SVG have no halos; a clamped panorama unaffected (its softness is
  `clamp_to_max`, tracked separately); animation/video unaffected. **Metal smoke test** for fp16
  mips.
- **Changelog** entry on ship.
