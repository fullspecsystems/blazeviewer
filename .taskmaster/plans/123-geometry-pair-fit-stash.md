# 123 fix 2 — The geometry-pair Fit stash: toggle-back is a rebind, never a re-decode

_Status: **rev 2 — Codex round 1 folded, IMPLEMENTATION-READY** (2026-07-19). Task #123
fix 2, owner-approved in direction ("keep the fullscreen texture for whatever mode they are
in handy"). Fix 1 (current-Original priority) shipped `c1b4d707`._

## The ask (owner, 2026-07-19)

> "If I hit F, and it decodes, and the pie goes away and it looks good… then I toggle back —
> I just fucking HAD that exact image, why do I have to re-decode from scratch again?"

A fullscreen toggle is a **two-state oscillation**, and the one-deep anonymous `held`
fallback always misses it: A→B stashes A (display-continuity only, shown stretched), B→A
stashes B — the A texture was freed one toggle ago. Every toggle-back re-decodes pixels we
held one second earlier.

## Why real machinery is warranted

- Closes the pre-Original window (even with fix 1, the first toggle races one big decode);
  toggle-BACK becomes instant from the very first F.
- **The only instant path for derive-ineligible photos** — `derive_fit` permanently refuses
  ICC mode-1; RAW is excluded from the Original tier. For those, this is it.
- Cost: ≤2 Fit textures, budget-accounted (see §Accounting) vs a multi-second 36 MP decode.

## Design (rev 2 — reshaped by the Codex round; anchors verified by the review)

### 1. Texture sharing: `Arc<RingSlot>` (Codex blocking finding 2)

`RingSlot` owns plain `wgpu::BindGroup`/`wgpu::Texture` (`gpu.rs:2471`) — **not Clone** in
wgpu 22; every existing ring operation moves slots (`Option::take`, `gpu.rs:3650/:3728`).
The renderer's slot storage becomes `Vec<Option<Arc<RingSlot>>>`, `held:
Option<Arc<RingSlot>>`, `stash: [Option<Arc<RingSlot>>; 2]` — ring, held, and stash can
then **alias one allocation** with zero VRAM duplication. Mechanical deref changes at the
construction/read sites (`upload_slot`, `derive_fit`, `render`, `present_slot`,
`reserve_ring`/`remap_ring`).

### 2. Identity: the effective pixel geometry, not merely `FitBox` (finding 5)

```rust
/// pb-app-core: the core-side mirror of one stashed renderer texture (#119 exactness).
struct FitStash {
    item: usize,
    content_gen: u64,
    /// The EXACT effective decode geometry the texture was produced for: the fit box
    /// plus everything `derive_fit_box` folds in (`app_core_impl.rs:5654`) — the
    /// content top inset and the quarter-turn rotation parity.
    fit: FitBox,
    top_inset: u32,
    quarter_turned: bool,
    bytes: u64, // unique-allocation VRAM accounting (aliases count once)
}
```

Presentable iff **all** fields match the current state exactly. Viewport dims are already
physical px (`app_core.rs:78`), so DPI needs no separate key component. Novel size / deck
change / rotation change / saved edit: miss → the incumbent ladder.

### 3. Capture: at the START of the resize burst, BEFORE any mutation (finding 1)

NOT in `invalidate_geometry` — by then `resize()` has already replaced `self.fit`
(`app_core_impl.rs:1434`) and may have re-presented the resident **Original** for
`resize_hold` (`:1452`); a late capture would stash the Original mislabeled as the old Fit.

- Hook the top of `resize()` before `self.fit` mutates: if this is the **first event of a
  burst** (`resize_settle_at.is_none()`), the displayed item's Fit is resident + definitive
  (`!preview_resident`), Fit mode is active, and the renderer confirms the presented slot
  is that very Fit slot — capture. **Once per burst**: later debounced events never retag.
- Capture rotates into the stash slot that does NOT match the *incoming* geometry, after
  first preserving any slot that already matches it and deduplicating an already-stashed
  outgoing geometry (Codex Q1) — A→B→A→B ends with both A and B live.
- Scale-mode toggles (`set_scale_mode`, `:5468`) are out of scope: Fit↔1:1/Fill are already
  instant via the Original rebind.

### 4. Renderer seam (default no-op; fail-loud both ways — finding 6)

```rust
/// Alias the texture of ring slot `ring_slot` into stash slot `stash_idx` — the caller
/// names the slot it believes is presented; the renderer VERIFIES `present_idx ==
/// ring_slot` and refuses otherwise (the #109.4 discipline: the core must never record
/// a stash the renderer lacks, and never stash the wrong occupant).
fn stash_fit(&mut self, stash_idx: usize, ring_slot: usize) -> bool { false }
/// Present stash slot `stash_idx` (a rebind). False = empty slot: the caller drops its
/// mirror entry (loud) and falls through to the decode ladder.
fn present_stash(&mut self, stash_idx: usize) -> bool { false }
/// Drop a stash slot's texture (invalidation/eviction). Idempotent.
fn clear_stash(&mut self, stash_idx: usize) { }
```

Every core mirror mutation gates on the renderer's return; failures eprintln
(`[ring-desync]`) and clear the mirror. The stash path is **transactional around the
`present_item` false-return hole** (`:7372/:7385`, the un-landed #109.5): `present_stash`'s
own bool gates `mark_resolved` directly, so the stash never inherits that hole.

### 5. Settle: the stash check beats Original-rebind, derive, AND the CPU want (findings 3–4)

- In `refresh_after_geometry_change`: after `target_item` assignment + view push
  (`:5527`), BEFORE retained-Original presentation and `try_gpu_derive_fit` (`:5534`):
  exact-match lookup → `present_stash` → on renderer-confirmed success: clear
  `resize_hold`, clear preview/upgrade/full-request bookkeeping for the item, then
  `mark_resolved`. Skip Original-rebind and derive.
- **Want suppression** (finding 4 — without this the CPU decode still runs):
  `request_prefetch`'s display-want build (`:5997/:6134`) treats an exact-match active
  stash for the current item as "definitive Fit present" and suppresses ONLY that one
  display-Fit want. Neighbour refill and the parked Original tier are unaffected. The
  suppression predicate re-checks the exact identity every pass (never a cached bool).

### 6. Invalidation + accounting

- `invalidate_content`: clear both mirrors + `clear_stash` both slots (before any reuse,
  `:8254` discipline). A successfully-presented *different* item clears them (current-photo
  scope) — cleared on present-success, not on mere target change (finding 6).
- Accounting (Codex Q2 — "account it"): a worst-case 7680-class Fit is ~118 MB SDR /
  ~236 MB fp16 — NOT negligible. Stash bytes are charged like ring slots: the mirror
  carries `bytes`; the ring's byte budget is debited for **unique** stash allocations
  (an Arc alias of a live ring slot counts once). v1 mechanism: subtract live
  unique-stash bytes from the budget passed into `reserve_bytes`' arithmetic via a small
  `ResidentRing::set_external_bytes(u64)` the core updates on stash create/clear.
- Q3 (resolved): definitive fulls only — a stashed preview would falsely resolve blurry.
- Q4 (resolved): no view state — `view_for` resets zoom/pan on geometry re-present
  today (`:6769`); rebinding with the current view is parity.

## What deliberately does NOT change

Refinement ladder, derive path, `resize_hold` semantics on a stash MISS, `held`
display-continuity, ring `(item, RepKind)` identity, blaze/nav paths, privacy (VRAM-only,
dies with the process, cleared on content change).

## Tests (planned)

- pb-render: Arc refactor is behavior-neutral (existing golden/ring tests stay green);
  stash_fit verifies the presented slot (wrong slot → false); present_stash on empty →
  false; clear idempotent.
- pb-core: `set_external_bytes` debits the reserve arithmetic (a reservation that fit
  without the stash is refused with it, and vice versa on clear).
- Core (mock renderer with stash support): A→B→A = present_stash hit, ZERO display-Fit
  want emitted (pool enqueue log) while neighbour + parked wants still flow; novel size →
  miss → normal want; capture-once: mid-burst resize events don't retag; the resize_hold
  Original is never stashed (capture precedes the hold rebind); content change → mirrors +
  renderer cleared; different-item present-success → cleared; mirror-without-texture →
  loud fallthrough to decode; preview-resident Fit → not stashed.
- The oscillation pin: A→B→A→B→A — first A and first B decode once each, every later
  leg is a stash rebind.

## Review log

- **r1 (Codex, 2026-07-19, background session):** verdict "needs another rev" — direction
  affirmed; six blocking findings, ALL folded into rev 2: (1) capture at burst start
  before `self.fit`/`resize_hold` mutate (was: invalidate_geometry — would mislabel the
  held Original); (2) `RingSlot` is not Clone in wgpu 22 → `Arc<RingSlot>` aliasing for
  ring/held/stash; (3) exact settle insertion point in `refresh_after_geometry_change`
  with the resize_hold/bookkeeping/mark_resolved ordering; (4) a stash hit must also
  suppress the display-Fit want or the CPU decode races it; (5) identity = fit box +
  top inset + rotation parity (effective derive geometry), DPI excluded (viewport is
  already physical px); (6) fail-loud completeness — verify the presented slot at
  stash time, gate every mirror mutation on renderer success, clear-on-present-success
  not on target change, transactional around the un-landed #109.5 hole. Q1 conditional-yes
  (preserve-incoming/dedup-outgoing rotation), Q2 account it (~118–236 MB per slot, alias
  counts once), Q3 fulls only, Q4 no view state.
