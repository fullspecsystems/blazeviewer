# Task 110 — GPU-derived Lanczos Fit from the retained Original (execution plan, v2)

> **STATUS: plan v2 — 2026-07-18, Codex-reviewed (four P0 correctness blockers folded in).** Executes
> the escalation flagged in `gpu-mipmap-hq-scaling.md` §5/§0 and the #110 line in `current-status.md`.
> Anchors are current-tree; trust symbol names over line numbers. **Owner priority: BEFORE item-6** —
> higher value and independent (operates on the current photo's already-retained Original).
>
> ⚠️ **Honesty correction (Codex):** this is a **box-prefiltered + Lanczos composite**, not
> "Lanczos-from-Original," and it is **not a proven quality upgrade over the box-mip trilinear frame** —
> it must be **A/B measured**, not asserted. The instant frame today is already mip-trilinear (Phase 1);
> #110's job is a *better* interim and the removal of the 1s CPU re-decode.

---

> **PROGRESS (branch `feat/110-gpu-lanczos-from-original`, 2026-07-18):** 110a partly done — the
> scale-aware Lanczos coefficients (`resample.rs`, 6 CPU tests, `90f8c4a5`) and the RingSlot owned-texture
> retention (`create_image_texture` → `RingSlot.texture/was_clamped/mode`, `fa5b30ab`) are committed and
> green. **Remaining 110a:** the two WGSL passes (§3a/§3b colour chain) + the odd-dim MIPGEN regression
> test. Then **110b** wires the derive in (the felt phase). Also landed this session: **ADR-024** (the
> two-mode invariant this plan serves) and the preview-into-native-tier fix (`100b3d3c`).

## 1. Problem & goal

On a `[f]` toggle / resize the current photo is instantly re-shown by mip-trilinear downscaling of its
retained full-res Original (Phase 1) — sharp-ish but soft/aliased on high-frequency detail — then ~1s
later a CPU-Lanczos Fit re-decodes over SMB and replaces it. **Goal:** derive an exact-size, higher-
quality Fit from the retained mipped Original **on the GPU** (mip-assisted separable resampling), so the
interim is crisper and the ~1s CPU re-decode disappears — for any photo whose Original is resident (the
current photo always; neighbours once item-6 lands).

**Non-goals:** ring retain (item-6); upscaling/magnification; video/planar; the CPU decode path for
photos with no resident Original (they keep CPU-Lanczos until item-6).

---

## 2. The four P0 blockers Codex found (must be designed around, not discovered mid-build)

1. **Mip storage is NOT what the draft assumed.** `generate_mips` filters in linear-premultiplied space
   but **stores STRAIGHT alpha**, and **mode 0 stores sRGB-ENCODED `Rgba8Unorm`** (not linear). So the
   Lanczos source is straight + encoded — the derive must re-linearize + re-premultiply on load (see §3b).
2. **Tap bound was wrong.** Antialiased minification widens Lanczos support by the residual scale `s`:
   support = `a·max(s,1)`. At residual 2×, Lanczos-3 ≈ **12–13 taps/axis** (not 7); Lanczos-2 ≈ 8–9.
3. **The v1 source is `renderer.held`, not a ring slot.** After the settle nukes the ring, the current
   photo's Original survives *only* in `held`. So `derive_fit_from_original(orig_slot)` can't reach it —
   the API needs a `Held | Ring(slot)` source selector ("derive from the currently-presented Original"),
   and must verify the presented resource is an eligible Original (§3d).
4. **A fp16 mode-0 Fit can't reuse mode-0 scene plumbing.** It must be sampled as **mode 2** (else the
   scene applies sRGB EOTF twice) **and** use `scene_scale(false)` on an HDR surface (today's `hdr=true`
   route selects mode 2 but wrongly skips SDR-white scaling). The renderer's `hdr` boolean **conflates
   storage/transfer with content dynamic range** — separate them (§3c). *Chosen mitigation:* mode-0 final
   is **RGBA8-sRGB** (not fp16), sidestepping this for the common case; only mode-2 finals are fp16.

Plus **P1:** "crisp within a frame or two" conflicts with the **180 ms settle** and the **shared,
non-preemptible GPU queue** — give the discrete-fullscreen derive a *shorter* coalescing delay, and note
a later keypress can still queue behind a submitted derive (§4).

---

## 3. Design

### 3a. Algorithm — mip-assisted, scale-aware, separable
- **Separable two passes** (H then V), fp16 intermediate (Lanczos negative lobes need signed storage).
  Separable is clearly right (tens of millions of loads vs tap-count²) — that is NOT the A/B.
- **Source mip:** two candidates to **A/B** — "last eligible box mip ≥ target" (fastest, most
  box-prefiltered) vs **`mip_bias = -1`** (start one level finer, residual 2–4×, wider kernel; likely the
  better quality/perf point). Codex: *this* is the real design fork, not separable-vs-2D.
- **Sampling math:** center mapping `src = (dst+0.5)·src_size/dst_size − 0.5`; support `a·max(s,1)` for
  residual minification `s`; weight `sinc(d/s)·sinc(d/(a·s))`, **normalized by the tap-weight sum per
  destination coordinate** (avoids edge/phase brightness drift). Clamp/mirror taps at the source extent.
- **Precompute coefficients once per derive, per axis** (every row shares the horizontal coefficients,
  every column the vertical) into a small buffer — never recompute `sin`/normalization per fragment.
- **Output dimensions = the FITTED IMAGE size, not the viewport** — account for aspect ratio,
  `content_top_inset`, 90°/270° rotation, integer rounding, and **no upscale**. A viewport-sized texture
  would distort the photo. (This mirrors what `pb_render::fit_rect` / the CPU decode-to-fit compute.)

### 3b. Color + alpha chain (Codex's corrected sequence — load-bearing)
Mips are straight-alpha, mode-0 sRGB-encoded. So:
1. **H pass:** load straight source; **mode 0**: sRGB EOTF to linear; **premultiply** RGB by α;
   accumulate *signed* RGB + α with the coefficients; normalize by weight sum.
2. Store the H intermediate as **premultiplied-LINEAR fp16**. **Do NOT un-premultiply between passes.**
3. **V pass:** filter the premultiplied-linear values; normalize.
4. **Final store:** clamp α to [0,1]; if α > ε **un-premultiply once**, else write RGB = 0.
5. **Mode 0 final:** OETF (sRGB encode) after un-premult → store **straight `Rgba8Unorm` sRGB**.
   **Mode 2 final:** store **straight scene-linear `Rgba16Float`**, no OETF.

The scene shader expects **straight alpha** + `ALPHA_BLENDING`; a premultiplied Fit would double-apply α
and darken edges (`SCENE_WGSL`). Verify the final store matches what the scene sampler consumes.

**Odd-dim caveat:** `MIPGEN_WGSL` currently **drops the trailing odd row/column** (not "underweights"
as its comment claims) — its mip phase is off on odd dims. Add an odd-dim mip regression **before**
relying on mip phase in the derive; treat the mip phase as slightly biased until fixed.

### 3c. Output format & scene integration
- **fp16 intermediate always** (signed lobes). **Final: RGBA8-sRGB for mode 0, fp16 scene-linear for
  mode 2.** Don't make every Fit fp16 — mode-0 packing halves persistent Fit VRAM with no signed-info
  loss (the intermediate carried the signal).
- A **fp16 (mode-2) Fit's bind group must use mode 2**, and on an HDR surface must apply
  `scene_scale(false)` (SDR-white scaling) — so **split the `hdr` boolean into (storage/transfer mode)
  vs (content dynamic range)** in the bind-group/`ColorUniform` construction. Get this wrong → double
  EOTF or wrong SDR-white.
- If a mode-0 derive samples the Original through an **sRGB-reinterpretation view** (to get hardware EOTF
  instead of per-tap `pow`), the Original texture must declare `Rgba8UnormSrgb` in `view_formats` —
  **empty today**; add it at Original creation or do the EOTF in-shader.

### 3d. Renderer surface area
- **Refactor `upload_image` to return an owned resource bundle**, not just a bind group. `RingSlot`
  gains: owned **`wgpu::Texture`** (not `Arc` unless a pending op needs independent retention — `Texture`
  isn't cloneable in wgpu 22; keep handles inside the renderer), the scene bind group, **actual uploaded
  dims + mip count**, source **mode/color class**, the **content-HDR flag / scene scale**, and
  **`was_clamped`** (did `clamp_to_max` alter it).
- **Source selector:** `derive_fit(source: DeriveSource, dst_slot, fit_w, fit_h)` where
  `DeriveSource = Held | Ring(slot)`. v1 uses `Held` (the current photo post-settle). Verify the source
  is a real, eligible Original (mip'd, not clamped, mode 0/2) before dispatch.
- **View aliasing:** a full-chain view is a valid *sampled source* only because the fp16 intermediate is
  a *different* texture — never sample a chain view while any of its mips is a render attachment (why
  `generate_mips` uses disjoint one-level views). The derive's passes target the intermediate/final, not
  the Original, so this holds — assert it.
- **`ScalePolicy` seam:** `BoxMipTrilinear` (today's instant present-path frame) · `GpuDeriveLanczos{
  kernel, mip_bias }` (this task) · `CpuLanczosDecode` (incumbent) — swappable for A/B.

---

## 4. Scheduling (a renderer op from the tick — NOT a decode-pool job)
The decode pool must never own GPU resources or hide submissions from queue ordering. So the derive is a
**renderer operation the core triggers on the event-loop tick**:
- **Coalesce by (geometry epoch, content_gen).** Dispatch **only while parked** (`held_nav().is_none()`).
- **Submit after a frame has presented**; run **at most one derive per tick**; do **not** fold it into
  the normal ≤2 upload burst unless a measurement proves the budget.
- Reserve the destination Fit slot **consistently with the core ring**; on any fallback (ineligible /
  derive fails), **release the reservation and schedule the CPU Fit** instead.
- **Shorter coalescing delay for a discrete fullscreen toggle** than the 180 ms interactive-resize settle
  (`app_core_impl.rs` resize path) — a toggle is one discrete event, not a drag stream. Keep the long
  settle for interactive resize.
- ⚠️ Shared queue: "not on the keypress handler" does **not** guarantee a clean keypress frame — a
  later keypress can land behind an already-submitted derive. This is why one-derive-per-tick + parked-only.

---

## 5. Performance & measurement (benchmark OUTPUT PIXELS, not ratios)
- 7360×4912 → 1440×961 from mip 2 (~1840×1228) ≈ **25M texture loads** for scale-aware Lanczos-3 —
  plausibly a few ms after coefficient precompute + hardware sRGB decode.
- **But the project target is 7680×3840**: that same photo → ~5760×3840 output from L0 ≈ **400M loads +
  a ~226 MB fp16 intermediate** → can **miss several refresh intervals on integrated GPUs.** So: bound by
  output pixel count, prefer `mip_bias` that keeps the source small, and benchmark on the real target.
- **Timestamp queries aren't enabled** on the device today (must request the feature); **CPU timing
  around `queue.submit` is meaningless.** Measure GPU timestamps + queue-to-present / PresentMon.

---

## 6. Eligibility & fallbacks
- **`was_clamped` → CPU Fit.** A derive from a nearest-neighbour `clamp_to_max`'d Original inherits
  irreversible aliasing — record `was_clamped`, fall back. **Use the texture's ACTUAL uploaded dims** to
  select the mip; `RingSlot.w/h` retain **pre-clamp** dims today.
- The 200 MP gigapixel gate does **not** cover a long panorama that exceeds `max_texture_dimension_2d`
  while under 200 MP — such an Original was clamped, so `was_clamped` catches it; verify.
- **Mode 1 (source-ICC) → CPU Fit (option a).** Its mips don't exist and can't be produced by threading
  the TRC into `MIPGEN_WGSL` (you can't store TRC/primaries-applied pixels back into an *encoded* RGBA8
  pyramid without inverse transforms). The correct follow-up (own plan) is a one-time GPU conversion of
  mode-1 L0 → scene-linear BT.709 fp16 pyramid, then treat as mode 2. Note mode-1's fallback *today* is
  **L0 bilinear**, not box-mip trilinear.

---

## 7. VRAM
Account **before allocation**: the derived Fit (RGBA8 for mode 0 / fp16 for mode 2) + the fp16 scratch
intermediate + the Original's mip chain (~4/3×L0, still under-counted — fold in the mip plan §4d fix).
An fp16 Fit is **not** VRAM-neutral vs the RGBA8 CPU Fit (at 8K it approaches the Original's size).
**Scratch policy:** either pool it and account it as persistent VRAM, or allocate/drop per derive and
benchmark cold allocation — "transient but reused" is contradictory; pick one.

---

## 8. A/B harness + correctness tests
- **Variants** (`ScalePolicy`): `BoxMipTrilinear`, `GpuDeriveLanczos{L2|L3, mip_bias ∈ {0,-1}}`,
  `CpuLanczosDecode`. **The core A/B is last-eligible-mip vs `mip_bias=-1`, both scale-aware.** Kernel:
  ship **L2 + L3** in the first A/B (L2 = safer/less halo, L3 = more detail/more ring); add Kaiser only if
  both fail. The incumbent CPU resize is premult-alpha but **encoded-space RGBA8** → a *compat* reference,
  not the linear-light ground truth.
- **Metrics:** perceptual (`nv-flip`, add as a `pb-render` dev-dep — not present yet) **and alias energy**
  (a zone plate rewards blur alone). Two references: encoded-space CPU-Lanczos (compat) + a linear-light
  reference (correctness).
- **Ratios** incl. sub-pixel phase: 1.25, 1.5, exact 2.0, 2.2, 2.8, 3.7, 5.1, 6–8× stress. **Patterns:**
  chirp/zone-plate/Siemens star, slanted edges, 1px diagonals, fine text, foliage/fabric, black/white +
  coloured edges (gamma), transparent coloured edges (alpha), profiled-SDR + fp16-HDR.
- **Determinism:** GPU float/`sin`/fp16 readback varies across adapters — keep a **pure-CPU coefficient/
  kernel unit test** (exact) + **tolerances** for GPU output. Plus the **odd-dim MIPGEN regression** (§3b).

---

## 9. Phasing
- **110a** — `RingSlot`/`held` owned-texture bundle (dims, mip count, mode, hdr/scene-scale, was_clamped);
  the two scale-aware Lanczos pipelines (fp16 intermediate; RGBA8-sRGB + fp16 finals); coefficient
  precompute; the pure-CPU coefficient test + odd-dim MIPGEN regression. **No behaviour change yet.**
- **110b** — `derive_fit(Held, …)` + the `ScalePolicy` seam; core triggers a derive (current photo only)
  when the presented Original is eligible, with CPU-Fit fallback; the shorter fullscreen coalescing delay;
  the `hdr`-boolean split for the fp16 mode-2 Fit. **Benchmark output-pixel budgets + timestamps.** *This
  is the phase the owner feels.*
- **110c** — the A/B/X harness + nv-flip + golden suite → pick kernel + mip_bias.
- **110d (defer)** — mode-1 fp16-pyramid conversion (own plan); compose with item-6 for neighbours.
- **Changelog** on ship (user-facing: fullscreen/1:1 crisp instantly, no re-sharpen pause).

## 10. Relationship to the other tracks
Independent of item-6 (the current photo always has a resident Original). Once item-6 retains neighbour
Originals, `derive_fit(Ring(slot), …)` covers advance-after-toggle too. #110 retires the mip plan's
Phase-D CPU re-decode *for photos with a resident Original*: the derive **is** the Fit, so box mips no
longer have to *be* Lanczos — they seed the derive.
