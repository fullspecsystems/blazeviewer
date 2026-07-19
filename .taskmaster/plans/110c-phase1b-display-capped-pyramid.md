# Phase 1b — the display-capped pyramid budget (ADR-024) — DESIGN DRAFT

> **STATUS: design draft — 2026-07-19, written for Codex review; NOT implemented.** The rest of
> #110 (110a/110b/110c harness + data-driven kernel pick) and item-6 (retain/remap + derive-on-nav)
> shipped on `feat/110-gpu-lanczos-from-original`. This is the remaining ADR-024 residency piece:
> **cap the resident pyramid's L0 to ~display resolution and budget the parked tier by detected
> RAM/VRAM.** Deliberately deferred to its own reviewed plan because it changes *residency
> semantics* (what an "Original" means) — a wrong cut here regresses 1:1 correctness, the one
> thing the parked tier exists for.
>
> **Importance/urgency (recorded per the deferral rule):** LOW urgency on the owner's own
> hardware — a 7680-wide display caps at ~11–15K L0, which no photo in the corpus exceeds, so
> this changes nothing there. HIGH importance for the 4 GB-laptop story ADR-024 promises, and a
> prerequisite for widening `full_res_radius` beyond 1 safely. Ship risk if skipped: none today
> (the gigapixel ceiling + ring byte budget still bound worst cases).

## 1. What ADR-024 fixes and the semantic knot

Cap the **parked tier's** decoded Original at `display_long_edge × ZOOM_HEADROOM` so a 24 MP and
a 100 MP photo cost the same resident footprint for *viewing* (display-bound, not image-bound),
self-scaling from the RTX 5090 desktop to a 4 GB laptop.

The knot: `Representation::Original` is **geometry-independent by contract** (survives geometry
changes — that independence is what item-6 retention rides on), but a display-capped L0 is a
function of the display. And in **ScaleMode::Original (1:1)**, presenting a capped texture is
*wrong pixels* — pixel-peeping needs natives. The cap therefore cannot silently redefine
`Original`; it needs its own identity.

## 2. Proposed design

1. **A capped Original is a distinct thing: `Representation::Pyramid { l0_cap }`** (or a
   `cap: Option<u32>` field on `Original` — reviewer input wanted; the typed variant makes the
   1:1 ineligibility unrepresentable-by-accident, matching the doors precedent). `RepKind` gains
   `Pyramid`. Identity: an item may hold a `Pyramid` **or** an `Original`, never both (the
   pyramid IS the capped original; holding both doubles VRAM for nothing).
2. **Decode:** the parked tier requests `Pyramid` when the photo's long edge exceeds the cap
   (`display_long_edge × ZOOM_HEADROOM`, headroom default **1.5**, settings-exposed later), by
   passing a decode-to-fit box of the cap (the existing `FitBox` path — no new decode code).
   At or under the cap, it requests plain `Original` exactly as today.
3. **Display selection:**
   - Fit display / the #110 derive: a `Pyramid` satisfies both (its L0 ≥ display, so the derive
     quality is unchanged for fit-to-screen; `derive_fit` sources it like an Original).
   - **ScaleMode::Original (1:1) / zoom past headroom: a `Pyramid` is NOT eligible** — the
     display path decodes the native Original on demand (today's async re-decode; the pyramid
     keeps showing meanwhile, quality-monotonic). Within the headroom, zoom serves from the
     pyramid (that is what the headroom buys).
4. **Budget:** parked-tier budget = `min(RING_BUDGET_BYTES, VRAM_FRACTION × detected VRAM)`
   with `VRAM_FRACTION` ≈ 1/8 and detected VRAM from the adapter
   (`wgpu::Adapter::get_info`/limits — Windows: DXGI adapter DedicatedVideoMemory via the
   existing `display` helper seam). `full_res_radius` auto-drops to 0 when a single pyramid
   exceeds ~1/3 of that budget. RAM side: the decode buffers are transient; no separate cap.
5. **Cap changes** (monitor swap / DPI change): the cap is sampled at *decode request* time; a
   resident pyramid built for a smaller display is simply **stale-but-usable** (it still serves
   fit-derives at the old quality) and is re-requested at the new cap lazily by the parked tier
   diff (`is_tracked_rep` miss on the new size? — needs a staleness stamp: store `l0_cap` in the
   rep and treat a larger current cap as "wanted but not resident", mirroring the Fit epoch
   pattern). Shrinking displays never force re-decode (bigger is fine).
6. **`full_res_eligible` becomes allocation-aware** (mip plan §4d): eligibility by projected
   bytes (`mip_chain_bytes(capped dims, bpp)`) against the tier budget — replacing the blunt
   `FULL_RES_MAX_PIXELS` for pyramids (natives keep the 200 MP gigapixel ceiling). HDR (8 B/px)
   is thereby automatically half the pixel budget of SDR.

## 3. What does NOT change

- The blazing Fit ring, previews, thumbs: untouched.
- The owner's 7680-wide display: cap ≈ 11520 px long edge → every corpus photo ≤ that → plain
  `Original`s everywhere, byte-for-byte today's behaviour.
- Gigapixel: `FULL_RES_MAX_PIXELS` still gates natives; a >200 MP photo gets a `Pyramid` now
  (bounded by the cap) instead of nothing — strictly better, and true-1:1 region decode stays
  the named deferral.

## 4. Test plan (pure-core first)

- Rep identity: pyramid-vs-original exclusivity; 1:1 ineligibility of `Pyramid`; derive
  eligibility of `Pyramid`.
- Cap math: headroom, long-edge selection, monitor-grow staleness, shrink no-op.
- Budget: radius auto-drop; allocation-aware eligibility (HDR halves pixels).
- The no-trace + residency invariants stay green (`viewing_a_folder_writes_nothing_to_disk`).

## 5. Open questions for review

1. Typed `Pyramid` variant vs `cap` field on `Original` — which keeps the compiler doing the
   1:1-ineligibility policing with less churn? (`RepKind` is matched in ~30 places.)
2. Zoom headroom 1.5× vs 2× — 2× doubles pyramid bytes ×4; is 1.5× enough zoom before the
   native re-decode kicks in?
3. Should the 110b derive's `DERIVE_SCRATCH_MAX` fold into the same VRAM-fraction budget?
4. Monitor-grow staleness: is the lazy re-request (item 5) worth the rep-stamp complexity in v1,
   or is "rebuilt on next content change" acceptable (displays change rarely)?
