# Manual test script — #110 + item-6 + watchdog (session of 2026-07-18/19)

Everything below shipped on `feat/110-gpu-lanczos-from-original` with tests green, but the
*felt* behaviour needs your eyes on the physical 7680×2160 display (not RDP — pin the display
per the mixed-DPI memory). Build with `pwsh scripts/build-windows.ps1 -Run` (debug console for
the diag lines) and confirm the About build id is fresh — a running exe silently blocks rebuilds.

Useful levers: `PB_SHARP_DIAG=1` (derive + watchdog + sharpen lifecycle lines),
`PB_PERF=1` (resize→on-screen ms), `PB_SCALE_POLICY=cpu` (kill the GPU derive — the
incumbent path, for A/B feel), `PB_DERIVE_MIP_BIAS=0` (the softer mip policy the data
rejected), `PB_DERIVE_KERNEL=2`. Corpus: the wedding folder over SMB.

## A. The owner-felt #110 wins (the point of the session)

1. **Fullscreen toggle is crisp instantly.** Park on a big JPEG → `F` → the photo should be
   sharp within a frame or two, with **no ~1 s soft-then-sharpen step**. Diag: a
   `[sharp-diag] GPU-derived Fit item=…` line per toggle; **no** CPU re-decode for the current
   photo after the settle. Toggle rapidly F/F/F — every landing crisp (straggler size events
   re-derive in ms; watch for any flash).
2. **The toggle reacts faster** (50 ms settle vs 180 ms). Should feel snappier than yesterday.
3. **Compare the feel**: relaunch with `PB_SCALE_POLICY=cpu` → the old soft-then-sharpen
   returns. (This is the A/B that proves what you're feeling.)
4. **Window-edge drag resize**: same crispness after you stop dragging (180 ms settle kept).
   `PB_PERF=1` resize→on-screen should be ~ms, not ~700–1000 ms.
5. **Advance right after a toggle** (item-6): park → `F` → immediately Space/Backspace through
   the neighbours. They must land **sharp, never the ~256 px preview flash** (previously they
   showed blurry for a couple of seconds). Diag: `GPU-derived Fit` lines on nav.
6. **Fit ↔ 1:1 ↔ Fit** on a parked photo: still instant both ways (the pre-existing rebind);
   then from 1:1 back to Fit on a photo whose Fit was never decoded (e.g. land in 1:1 mode,
   then switch) — should derive, crisp, no wait.

## B. Correctness edges (each exercises a specific new code path)

7. **Rotated photo**: `R` (90°) on a landscape photo → `F` toggle → must stay sharp and
   correctly aspected (the derive now swaps its target axes for 90/270 — pre-fix it would have
   been ~1.4× upscaled/soft).
8. **HDR** (an HDR AVIF/HEIC, HDR desktop on): park → toggle → correct brightness, no washed
   or double-darkened look (the fp16 mode-2 derive final + scene-scale split).
9. **Wide-gamut ICC (P3) photos**: the derive REFUSES mode-1 sources by design → CPU fallback
   → colours identical to yesterday, just the old speed. No colour shift allowed.
10. **RAW**: unchanged behaviour end-to-end (excluded from the parked tier + watchdog).
11. **Videos + archive doors**: browse past them, toggle fullscreen on them — no derive
    interference, posters/cards as before.
12. **Zoom/pan after a toggle**: zoom in on a derived Fit — behaves exactly like a decoded one.
13. **Undo a saved rotation of a photo you've navigated AWAY from**, then go back to it — it
    must show the reverted orientation (the retention-era content purge; pre-fix its stale
    Original could survive).
14. **VRAM stability**: Task Manager GPU memory over ~50 toggles/navs — flat-ish, no climb
    (derived textures replace, scratch is transient; mip-inclusive accounting now bounds the
    ring honestly).

## C. The watchdog (ADR-024 safety net — stress)

15. Blaze hard (hold Space through hundreds over SMB), flip `F` mid-blaze, release keys while
    the window loses focus, repeat. **Any photo that lands blurry must self-heal to sharp
    within ~2 s** with no input. Diag: `[sharp-diag] preview watchdog FIRED …` = the net
    caught a stuck `held_nav`. If you ever see a photo stuck blurry >3 s, that's a NEW bug —
    grab the sharp-diag output.

## D. The data (already measured; re-run if curious)

`cargo test -p pb-render --release -- --ignored ab_report --nocapture`
— the 110c matrix. Defaults shipped from this data: **Lanczos-3, mip_bias −1** (FLIP ≤ 0.012
vs ground truth everywhere; bias 0 went soft above 2× and collapsed at exactly 2×; L2 aliased
on 1-px diagonals). On-screen sanity check: zone-plate-like content (fine brick, fabric,
distant foliage) at fullscreen should show **no shimmer** (aliasing) and **no mush** (blur)
after a toggle.

## Known/accepted (not bugs tonight)

- Mode-1 (ICC) photos keep the CPU re-decode path (their pyramid conversion is #110's 110d).
- The Phase-1b display-capped pyramid budget is a reviewed design draft, not implemented
  (zero effect on the 7680 display; see `.taskmaster/plans/110c-phase1b-display-capped-pyramid.md`).
- ~~macOS uses the old drop-all ring rebuild (remap_ring trait default)~~ — **STALE, corrected
  2026-07-19 (#113).** macOS calls the REAL `remap_ring`: shared `rebuild_ring`
  (`app_core_impl.rs:8046`) invokes it on `WgpuRenderer`, which overrides the trait default at
  `gpu.rs:3709`, and the mac shell constructs exactly that renderer. The whole arc was verified
  on-device on Metal — `GPU-derived Fit` fires per toggle, `PB_SCALE_POLICY=cpu` is a clean A/B,
  and all 69 pb-render tests (incl. the real GPU derive suite) pass headless on Metal.
- ⚠ **Test #110 on a JPEG, not whatever sorts first.** RAW is excluded from the parked tier, so a
  folder whose first item is a `.NEF` shows ZERO derive lines and looks broken. (Cost the #113
  verifier four confusing toggles.)
