# Codex review of `110-gpu-lanczos-from-original.md`

> Reviewer: Codex, 2026-07-18, against the current tree. Verdict cross-checked by Claude Code and
> **folded into plan v2**. This captures the raw findings; the plan is the authority.

## Verdict
Keep the architecture (mip-assisted separable resampling), but **do not implement v1 unchanged** — four
P0 correctness blockers, and the plan overstated both quality and latency. It is a **box-prefiltered +
Lanczos composite**, not "Lanczos from Original," and **not a proven upgrade over box-mip trilinear** —
must be A/B measured.

## P0 blockers (all folded into plan v2 §2–§3)
1. **Mip storage mischaracterized.** `generate_mips` **stores straight alpha**, and **mode 0 stores
   sRGB-encoded `Rgba8Unorm`** (not linear). The derive must re-linearize (mode-0 EOTF) + re-premultiply
   on load; keep premultiplied-linear between the separable passes; un-premultiply + OETF only on final
   store. (Scene expects straight alpha + `ALPHA_BLENDING` — a premult Fit darkens edges.)
2. **Tap bound wrong.** Antialiased minification widens support to `a·max(s,1)`. Residual 2× → Lanczos-3
   ≈ 12–13 taps/axis (not 7), L2 ≈ 8–9. Center map `src=(dst+0.5)·src/dst−0.5`; weight
   `sinc(d/s)·sinc(d/(a·s))`, normalized per dst coord; **precompute per-axis coefficients once**.
3. **v1 source is `renderer.held`, not a ring slot** (the ring is nuked at the settle; only the current
   photo's Original survives in `held`). API needs a `Held | Ring(slot)` selector ("derive from the
   currently-presented Original"), verifying it's an eligible Original.
4. **fp16 mode-0 Fit can't reuse mode-0 plumbing** — must sample as **mode 2** + `scene_scale(false)` on
   HDR surfaces; the `hdr` boolean conflates storage/transfer with content DR and must be split. Chosen
   mitigation: **mode-0 final = RGBA8-sRGB** (not fp16); only mode-2 finals are fp16.

## P1 + key corrections
- "Crisp within a frame or two" conflicts with the **180 ms settle** + shared non-preemptible queue —
  give discrete fullscreen a **shorter coalescing delay**; a later keypress can still queue behind a
  submitted derive (→ one derive/tick, parked-only, submit-after-present).
- **The real A/B is "last eligible box mip" vs `mip_bias=-1` (one level finer),** both scale-aware — NOT
  separable-vs-2D (separable is clearly right).
- **Output = FITTED IMAGE dims, not viewport** (aspect / content-top inset / 90°·270° rotation / rounding
  / no-upscale) or the photo distorts.
- **fp16 intermediate always; RGBA8-sRGB final mode 0; fp16 final mode 2.** Not every Fit fp16.
- **Ownership:** refactor `upload_image` to return an owned bundle; `RingSlot` needs owned `Texture`
  (not cloneable in wgpu 22), uploaded dims + mip count, mode/color class, hdr/scene-scale, `was_clamped`.
  View aliasing: full-chain view OK as sampled source only because the intermediate is a different texture.
- **Scheduling:** renderer op from the tick, **not** a decode-pool job (pool must not own GPU resources).
  Coalesce by (epoch, content_gen); parked-only; reserve consistently + release/fallback-to-CPU on failure.
- **Perf:** benchmark **output pixel counts**, not ratios — 7360→1440 from mip 2 ≈ 25M loads (few ms), but
  the 7680×3840 target → ~5760×3840 from L0 ≈ 400M loads + ~226 MB scratch (can miss refreshes on iGPUs).
  Timestamp queries aren't enabled today; CPU-around-submit timing is meaningless — use timestamps +
  queue-to-present/PresentMon.
- **`clamp_to_max`:** derive inherits irreversible aliasing → record `was_clamped`, fall back to CPU Fit;
  select the mip with the **actual uploaded** dims (`RingSlot.w/h` are pre-clamp today).
- **Mode 1 → CPU Fit (a).** (b) can't store TRC/primaries-applied pixels back into an encoded RGBA8
  pyramid; the correct follow-up is a one-time GPU L0→scene-linear-BT.709-fp16 pyramid (own plan). Mode-1's
  fallback today is L0 **bilinear**, not box-mip trilinear.
- **Kernel:** ship L2 + L3 in the first A/B; Kaiser only if both fail. Incumbent CPU resize is
  premult-alpha but encoded-space RGBA8 (compat reference, not linear-light ground truth).
- **Tests:** pure-CPU coefficient/kernel test (exact) + GPU tolerances (cross-adapter float/sin/fp16
  nondeterminism); **odd-dim MIPGEN regression** (the shader drops the trailing odd row/col — its comment
  is wrong). VRAM: account final + scratch + Original mips before allocating; fp16 Fit is not VRAM-neutral.
- **`view_formats`:** empty today — a mode-0 sRGB-reinterpretation view needs `Rgba8UnormSrgb` declared, or
  do EOTF in-shader.

**Bottom line:** architecture sound with the above; the decisive A/B is last-eligible-mip vs one-finer,
both scale-aware, and the whole thing must be measured (perceptual + alias energy), not asserted.
