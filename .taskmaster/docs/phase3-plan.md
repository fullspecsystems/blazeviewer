# Phase 3 — The Prefetch Engine (implementation plan)

_Drafted 2026-06-27; revised after two plan reviews. The headline milestone:
**hold a key and fly on full-res photos.** Turns the synchronous, decode-bound
viewer (~4–5 fps on 24–45 MP wedding JPEGs) into one where a keypress is a
**rebind, never a decode or an upload.**_

> Prime directive check: every item below is either a direct speed win or
> perf-neutral, behind a benchmarkable seam, and TDD'd. Nothing here ships on
> vibes — the exit criterion is a measured number.

---

## 1. Why today is slow (grounded in the code)

| Fact | Location | Consequence |
|---|---|---|
| Decode + file read run **on the event-loop thread** | `pb-app/src/main.rs:228` (`load_current` → `decode_image_file`), also `:145` | A big photo blocks input, advance, and present. Violates the project's "never block the event loop" rule. |
| Upload uses `queue.write_texture` | `pb-render/src/gpu.rs:250` | The documented trap (60–75 fps on large frames). The upload spike proved `copy_buffer_to_texture` ≈ 48 GB/s — 3.4× left on the table. |
| A **new** texture+view+sampler+bind group is built per nav | `gpu.rs:535` `set_image` → `upload_image` (`:221`) | Per-keypress GPU allocation + upload churn; no residency. |
| Policy exists, **slot bookkeeping doesn't** | `pb-core::prefetch::prefetch_targets`, `pb-core::cache::plan_residency` | We can decide *what* to keep resident, but nothing maps item→GPU-slot or picks victims. |

The pure nav/prefetch/residency logic in `pb-core` is already tested and ready to
drive a real engine. Phase 3 is mostly *wiring threads, a ring, and a staging
uploader around primitives that already exist*, plus one new pure module (ring
slot bookkeeping) and the instrumentation to prove it.

---

## 2. Target architecture

```
 ┌─ Event-loop thread (winit) ────────────────────────────────────────┐
 │ keypress → advance is GATED on readiness (see §3.5):               │
 │   target ready (Resident) → present_slot → present     (≤1 frame)  │
 │   target not ready         → keep showing prev frame, wait; the    │
 │                              advance does NOT run ahead (no skip)   │
 │ about_to_wait: drain a BUDGETED number of decode results →         │
 │   reserve slot → upload (staging ring) → mark_resident             │
 │ NEVER blocks on I/O or decode. NEVER uploads on the keypress frame. │
 └────────────────────────────────────────────────────────────────────┘
        ▲ outcomes {DecodeKey, result}        │ prioritized want-list
        │ bounded mpsc (byte-budgeted)         ▼ set_targets(epoch)
 ┌─ Decode pool (2–8 workers, capped) ────────────────────────────────┐
 │ priority queue (want-list order) + per-job cancel + DecodeKey       │
 │ worker: read bytes off-disk → decode-to-fit → send {DecodeKey, img} │
 └────────────────────────────────────────────────────────────────────┘
```

Three new/changed seams (all benchmarkable, per the A/B methodology):

- **`UploadStrategy`** (new trait, `pb-render`): v1 = recycled mapped-staging-buffer
  ring via `copy_buffer_to_texture`. CUDA zero-copy is the Phase-7 alias behind
  this same seam.
- **Resident texture ring** (`pb-render`): N pre-allocated fit-size slots reused
  across photos. `upload_slot` (prefetch-time) / `present_slot` (keypress-time).
- **`ResidentRing`** (new pure module, `pb-core`): item↔slot bookkeeping with
  **slot states + reservations + pinning**. No GPU, fully unit/property-testable.

---

## 3. Module-by-module design

### 3.0 Invariants — epochs, decode keys, reservations, pinning (the anti-staleness spine)

Decoded frames are produced asynchronously, so **a result can arrive after the
world it was decoded for has changed** (a resize, a fit↔original toggle, a newer
decode of the same item, or after it was cancelled). Three mechanisms keep that
from corrupting what's on screen — they thread through the pool, the ring, and the
app:

- **`epoch: u64`** on `App` — a monotonic generation bumped whenever decode
  geometry changes (resize, fit↔original, ring re-reservation). Every job is
  stamped with the current epoch; every outcome carries it back.
- **`DecodeKey { item, epoch }`** — identifies a unit of decode work. The pool
  **dedups** on it (never two in-flight decodes of the same key) and the app
  **discards** any outcome whose epoch ≠ the current epoch (decoded for a stale
  geometry).
- **Slot states** `Empty | Pending { item, epoch } | Resident { item }` — a slot
  is **reserved** (→`Pending`) the instant it's chosen as a victim, so a second
  reservation in the same drain tick can't grab it. `mark_resident` only flips
  `Pending → Resident` when the outcome's `(item, epoch)` matches the reservation;
  otherwise the slot is freed and the result dropped.
- **Displayed-slot pin** — the slot currently on screen is never an eviction
  victim (critical during a miss-hold, when the UI still depends on the previous
  frame's slot).

These are pure, and they are the main thing the ring's property tests exercise.

### 3.1 `pb-render` — `UploadStrategy` seam + staging ring (replaces `write_texture`)

New trait:

```rust
pub trait UploadStrategy {
    /// Stage `rgba` (w*h*4) and record a copy into `tex` at (0,0). Called only
    /// during prefetch (between frames), never on the keypress frame.
    fn upload(&mut self, device:&Device, encoder:&mut CommandEncoder,
              tex:&Texture, rgba:&[u8], w:u32, h:u32) -> bool; // false = no buffer free this tick
    fn after_submit(&mut self); // recycle: kick map_async on consumed buffers
    fn name(&self) -> &'static str;
}
```

v1 impl `StagingRingUpload` — **a recycled mapped-buffer ring, not "persistently
mapped"** (wgpu forbids using a mapped buffer as `COPY_SRC`). Each buffer cycles
through an explicit state machine:

```
Writable(mapped) --write padded rows--> unmap --record copy_buffer_to_texture-->
  InFlight(COPY_SRC) --[after submit completes]--> map_async(Write) --> Writable
```

- Ring depth ≥ `desired_maximum_frame_latency + 1` so a `Writable` buffer is
  normally available. If none is free this tick, `upload` returns `false` and the
  caller defers that upload to a later tick (ties into the per-tick upload budget,
  §3.5). `after_submit` (called once per frame after `queue.submit`) kicks
  `map_async` on consumed buffers; completion is observed in the existing
  `poll()`/`about_to_wait`.
- Each buffer is ≥ one fit-size frame at the **256-byte-aligned** row stride
  (reuse the padding math already in `render_offscreen`, `gpu.rs:699`).
- **Evaluate `wgpu::util::StagingBelt` first** — it implements exactly this
  mapped-ring + recall state machine; hand-roll only if profiling shows its
  chunking is suboptimal (an A/B). Either way the seam is identical.
- Replaces `queue.write_texture` in `upload_image`. **Golden test proves
  byte-identical output** to the old path — a safe, isolated swap.

### 3.2 `pb-render` — resident texture ring

Extend `Renderer` + `WgpuRenderer`:

```rust
fn reserve_ring(&mut self, capacity:usize, slot_w:u32, slot_h:u32); // alloc slots at fit size
fn upload_slot(&mut self, slot:usize, rgba:&[u8], w:u32, h:u32) -> bool; // staging-ring upload (prefetch)
fn present_slot(&mut self, slot:usize);                            // bind + recompute quad (keypress)
```

Design:
- **Fixed-size slots, sub-rect UVs.** Each slot is one texture allocated at the
  fit-box size (display size, clamped to GPU max). Decode-to-fit guarantees the
  decoded image is ≤ that box, so we upload it into the slot's top-left and the
  draw maps UVs to `(used_w/slot_w, used_h/slot_h)`. **Zero per-nav allocation —
  the whole point.**
- **UV bleed guard.** Linear filtering at the used/unused boundary would
  interpolate stale pixels from the rest of the slot. Fix: on upload, **replicate
  the last used row and column into a 1-px gutter** (so the bilinear footprint at
  the edge samples a duplicated edge, not garbage), keep the sampler clamp-to-edge,
  and inset UVs by a half-texel. A golden test asserts the right/bottom **edge
  pixels** match the source (no bleed).
- **Original (1:1) mode is outside the ring** (full-res can exceed the fit box).
  It keeps the single-texture re-decode path — flying in 1:1 isn't the target case.
- **Capacity adapts to a VRAM budget.** A fit slot on the 7680×2160 display is
  ~66 MB; cap the ring at a budget (default ≈ 1–2 GB → ~16–32 slots; far smaller
  per-slot on normal displays). `capacity = clamp(budget/slot_bytes, 4, 64)`.

### 3.3 `pb-core` — `ring.rs` (new, pure) — slot bookkeeping with reservations

```rust
enum SlotState { Empty, Pending { item: usize, epoch: u64 }, Resident { item: usize } }

pub struct ResidentRing {
    slots: Vec<SlotState>,
    by_item: HashMap<usize, usize>,   // item -> slot (Pending or Resident)
    displayed: Option<usize>,         // pinned slot, never evicted
}
pub struct Reservation { pub item: usize, pub slot: usize, pub epoch: u64 }

impl ResidentRing {
    pub fn slot_for(&self, item: usize) -> Option<usize>;        // Resident only (keypress hit test)
    pub fn set_displayed(&mut self, slot: usize);               // pin the on-screen slot
    /// Choose+reserve a victim slot for `item` at `epoch`, marking it Pending.
    /// Victims, in order: Empty → Resident not in `keep` → (never the displayed
    /// slot, never a slot already Pending). Returns None if nothing is evictable.
    pub fn reserve(&mut self, item: usize, epoch: u64, keep: &[usize]) -> Option<Reservation>;
    /// Flip Pending→Resident iff (item, epoch) matches the reservation; else free
    /// the slot and return false (stale/cancelled result).
    pub fn mark_resident(&mut self, item: usize, slot: usize, epoch: u64) -> bool;
    pub fn on_epoch_change(&mut self);  // drop all Pending + Resident (geometry changed)
}
```

- `reserve` uses `plan_residency`'s spirit (keep the highest-priority `capacity`
  targets) but is **stateful**: it mutates the ring to `Pending` so consecutive
  reservations in one drain tick never collide. `keep` is the current `targets`
  list (don't evict something we still want); the displayed slot is always kept.
- **Pure → unit + `proptest`:** capacity never exceeded; no two items share a
  slot; the displayed slot is never evicted; a stale-epoch `mark_resident` is
  rejected and frees the slot; reserve+mark round-trips; replanning is idempotent
  when nothing moved.

### 3.4 `pb-app` — `decode_pool.rs` (new) — the priority worker pool

```rust
pub struct DecodePool { /* workers, shared prio queue + condvar, bounded results rx */ }
pub struct Job { key: DecodeKey, path: Arc<Path>, fit: Option<FitBox>, cancel: Arc<AtomicBool>, prio: u32 }
pub struct Outcome { key: DecodeKey, result: Result<DecodedImage, DecodeError> }
impl DecodePool {
    pub fn new(workers: usize, byte_budget: usize, decoder: Arc<dyn ImageDecoder>) -> (Self, Receiver<Outcome>);
    pub fn set_targets(&self, epoch: u64, prioritized: &[(usize, Arc<Path>, Option<FitBox>)]);
    pub fn try_drain(&self) -> impl Iterator<Item = Outcome>;
}
```

- **Worker count capped: `clamp(cores - 1, 2, 8)` by default** (not `cores - 1`,
  which is ~31 here — each worker holds a full RGBA + resize scratch buffer, so an
  uncapped pool spikes RAM and disk). Tunable + A/B'd.
- **Decoded-byte budget / backpressure.** The results channel is bounded by total
  decoded bytes; when over budget, workers park before starting a new decode
  rather than racing ahead of what the uploader can drain. Keeps memory bounded
  no matter how deep the prefetch window is.
- **Keys + dedup + cancellation.** Jobs carry `DecodeKey { item, epoch }`;
  `set_targets` flags `cancel` on queued/in-flight jobs no longer wanted, drops
  them, and enqueues missing keys in priority order (0 = current). The pool never
  double-queues a key. Outcomes carry the key back for the app's staleness check.
- **Cooperative preemption** (owner-confirmed): no mid-decode interruption; the
  priority queue keeps the on-screen image first in line. Add a reserved
  "priority-lane" worker only if measurement shows it waiting.
- Decoder is `Arc<dyn ImageDecoder>` → `zune-jpeg` now, `turbojpeg` later, no pool
  changes.

### 3.5 `pb-app` — wiring (`App`) — the gated-advance state machine

`App` gains: `pool`, `ring: ResidentRing`, bounded `results` receiver, `epoch`,
`displayed_item`, `target_item`, `ahead/behind`, `last_present`.

**Advance is gated on readiness — this is how "every photo shown, none skipped"
and "hold previous on a miss" stop contradicting each other.** The cursor never
runs ahead of what's been shown:

```
about_to_wait, while a nav key is held and past the initial-delay:
  if target_item != displayed_item:            # we owe a frame
      match ring.slot_for(target_item):
        Some(slot) -> present_slot; displayed_item = target_item; last_present = now
        None       -> keep showing displayed_item; ensure target_item is priority 0; WAIT
                      # ^ the transient "hold previous frame" — NOT a skip
  else if now >= last_present + frame_interval: # caught up; time for the next one
      playlist.step(dir); target_item = playlist.current()
      # try to present it on this same tick (falls into the branch above)
```

So fly speed = `min(refresh, decode throughput)`: instant when prefetch keeps up
(the common case), gracefully slowing to decode-rate through a hard patch, and
**every photo is shown in order.** (A *fast/skip-to-newest-ready* mode stays a
measured A/B knob — see §6.1 — but is not the default.)

**On any advance / target change:** recompute `targets = prefetch_targets(&pl,
ahead, behind)` and `pool.set_targets(epoch, …)`. A keypress itself only does the
gated present above — **no decode, no upload on the keypress frame.**

**Draining + upload, budgeted (`about_to_wait`):**
- Pop ready outcomes; **discard any whose epoch ≠ current** (stale geometry).
- Order them **target/current first**, then by prefetch priority.
- Upload **at most `MAX_UPLOADS_PER_TICK` (start 1–2)** per tick: for each,
  `slot = ring.reserve(item, epoch, &targets)`; if `Some`, `upload_slot`; if it
  returns `true` (a staging buffer was free), `ring.mark_resident`. Anything not
  uploaded this tick stays queued (the channel is byte-bounded) for the next tick.
  This caps event-loop time so a burst of finished decodes can't stall a frame.
- If the freshly-resident item **is** `target_item`, the next gated tick presents
  it (or present immediately and `set_displayed(slot)`).

**Epoch bumps:** `Resized`, fit↔original, and `reserve_ring` all bump `epoch`,
call `ring.on_epoch_change()`, and re-issue `set_targets` — so in-flight decodes
for the old geometry are discarded on arrival.

### 3.6 Instrumentation — keypress → photon (the proof)

Two layers, gated so they compile out of release (split across the sequencing so
measurement exists *before* the refactors — see §5):

- **3.6a (portable, lands first):** `tracing` zones + per-frame **NDJSON** for
  scan / read / decode / reserve / upload / render, **wgpu timestamp queries**
  (`TIMESTAMP_QUERY` + `wgpu-profiler`) for the upload-vs-draw GPU split, Tracy via
  `tracing-tracy` behind a `profile` feature, and a unit-tested
  `percentiles(p50/p95/p99)` helper. Also tracks **ready-miss rate** (advances that
  hit the wait branch).
- **3.6b (Windows photon, behind a `metrics` feature):** true scanout via DXGI
  `GetFrameStatistics().SyncQPCTime`, reached through a `wgpu_hal` DX12 downcast to
  the underlying `IDXGISwapChain` (small, **quarantined** unsafe surface in one
  `platform::win` helper). Validate against Intel **PresentMon**. Input stamped via
  QPC at keypress.

---

## 4. Testing strategy (TDD — write the test first)

| Unit | Test kind | Asserts |
|---|---|---|
| `pb-core::ring` | unit + `proptest` | capacity never exceeded; slots unique; **displayed slot never evicted**; **stale-epoch `mark_resident` rejected + frees slot**; reserve→mark round-trips; idempotent when stable |
| gated advance | pure unit | extract a `decide(displayed, target, ring, now) -> Present(slot)|Wait|Step` and test: hit→present, miss→wait (no step), caught-up+due→step. **No skip on a miss.** |
| `decode_pool` | integration w/ a fake `ImageDecoder` (controllable latency) | current decoded first; cancelled/stale-key outcomes dropped; dedup (no double-queue); **byte budget caps in-flight memory**; survives churn |
| `StagingRingUpload` | golden + unit | byte-identical to the `write_texture` baseline; `upload` returns `false` when no buffer is free; recycles after `after_submit` |
| Ring renderer | reference-PNG golden | each slot binds & draws correctly; **edge pixels of a sub-rect upload match source (no UV bleed)** |
| upload budget | pure unit | ≤ `MAX_UPLOADS_PER_TICK` per tick; target/current uploaded first; remainder deferred |
| `percentiles` | pure unit | p50/p95/p99 on known inputs |

Coverage stays >80% (`cargo-llvm-cov`); the GPU/present shell is `#[coverage(off)]`
so the number stays honest. New visual tests use reference-PNG + tolerance (toward
the nv-flip bar in CLAUDE.md); retrofitting the Phase-1/2 smoke tests is Phase-6
rigor. Decoder fuzzing is already Phase 6.

---

## 5. Sequencing (each step is runnable, and green = test + clippy + **fmt**)

> Measurement moves to the front (per "we do not guess about speed"): without
> per-stage numbers we can't prove 3.1/3.2 helped. The photon-accurate DXGI work
> still lands last, when there's something fast to measure precisely.

0. **3.0 Measure + harden (prereqs)** — land *before* touching the hot path:
   - **Stage timers (3.6a):** `tracing` zones + per-frame NDJSON for
     scan/read/decode/upload/render + ready-miss rate, and a unit-tested
     `percentiles` helper. Every later step now shows a before/after.
   - **Harden** (cheap, and some get *worse* under prefetch): `Focused(false)` →
     clear `held` (the promised focus-loss net); re-decode the first frame once the
     window settles to its true size; `cargo fmt --all` to clear the drift.
   - **TDD red:** a failing test pinning the random-prefetch cycle-boundary bug
     (§7), left red until random nav is wired.
1. **3.1 Staging-ring `UploadStrategy`** — swap `write_texture` → recycled staging
   ring behind the new seam. Isolated, golden-test-proven byte-identical.
   Removes the documented trap; unblocks the ring. **Lowest risk.**
2. **3.2 Decode pool (no ring yet)** — move read+decode off the event loop with
   keys/cancel/byte-budget; `about_to_wait` drains (budgeted) and uploads into the
   single texture. The loop stops freezing on big photos even before prefetch.
3. **3.3 Prefetch + resident ring** — `pb-core::ring` (states/reserve/pin),
   renderer slots + UV gutter, wire `prefetch_targets → pool → reserve →
   upload_slot → mark_resident`, keypress→`present_slot`. **This is hold-to-fly.**
4. **3.4 Gated advance + miss handling** — the §3.5 state machine, displayed-slot
   pin, per-tick upload budget; tune `ahead/behind`.
5. **3.5 Photon-accurate keypress→photon (3.6b)** — DXGI `GetFrameStatistics`
   behind `metrics`, validated vs PresentMon; produce p50/p95/p99 and check the
   exit criterion.

**Exit criterion (from `roadmap.md`):** holding →/space sustains ~refresh-rate
paging on the corpus with cache-hit **keypress→photon ≤ ~1 frame (p95)**; misses
fall back to preview (Phase 4), never stall.

---

## 6. Decisions (resolved with owner 2026-06-27)

1. **Advance model:** ✅ **Every-frame-shown via gated advance** (§3.5) — the
   cursor never runs ahead of what's been shown; a miss holds the previous frame
   *for that one step* until its decode lands, then shows it. No skip, no stall.
   *Skip-to-newest-ready* stays a measured A/B knob, not the default.
2. **Ring VRAM budget:** ✅ **Adaptive ~1–2 GB cap** — `clamp(budget/slot_bytes,
   4, 64)`.
3. **Original (1:1) mode:** ✅ **Outside the ring** — re-decode on demand.
4. **Mid-decode preemption:** ✅ **Cooperative only** to start; reserved
   priority-lane worker only if measurement demands it.
5. **DXGI photon timestamp:** small quarantined `unsafe` wgpu-hal downcast behind a
   `metrics` feature + PresentMon validation (revisit at 3.5).
6. **Decoder:** stays pure-Rust `zune-jpeg`; the pool is decoder-agnostic so
   `turbojpeg` swaps in later. No action.

---

## 7. Pre-existing issues found in review (2026-06-27) — scheduled above

Real, and now placed — not new scope creep. The first four are folded into **3.0**;
the architectural ones (keys/reservations/pinning/budgets) are in **§3.0/§3.3/§3.5**.

- **Random prefetch cycle-boundary miss (pb-core bug).** `extend_random`
  (`prefetch.rs:71`) wraps `pl.shuffle().at(p)` within the *current* deck, but
  `random_next()` swaps in a fresh `reshuffled()` deck at exhaustion
  (`playlist.rs:116`) — so prefetch at the wrap targets the old cycle's items.
  Doesn't bite yet (random/`enter` nav isn't wired). **Fix:** make the next cycle
  peekable (compute eagerly / `peek_next_cycle`) so prefetch spans the boundary;
  pin with a failing test in 3.0, fix when random nav is wired.
- **First frame can decode at a stale window size** (`main.rs:320`→`:339`): re-decode
  once the real size is known. Fixed in 3.0.
- **Held keys not cleared on focus loss** (`main.rs:411`): only a key-up clears
  `held`; a lost release strands a held key → runaway advance (and, post-3.3,
  runaway prefetch). Add the `Focused(false)` net in 3.0.
- **`cargo fmt --check` fails** (`hud.rs`, `main.rs` drift). Cleared in 3.0; `fmt`
  is now part of the green bar for every step.
- **Visual tests are pixel spot-checks, not perceptual golden.** New ring/upload
  work uses reference-PNG + tolerance; retrofitting the Phase-1/2 smoke tests is
  Phase-6 rigor.
- **Startup blocks on the full folder scan before first paint** (`main.rs:533`): a
  slipped Phase-2 "first image ASAP" item. **Independent of the engine** and needs
  a `pb-core::Playlist` that can *grow* (it takes a fixed `len` today). ✅ **Owner
  decision (2026-06-27): deferred to a separable track *after* the engine lands.**
</content>
