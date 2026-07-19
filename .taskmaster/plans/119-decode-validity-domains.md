# 119 — Decode validity domains: geometry-immortal Originals, one staleness law

_Status: **rev 3 — IMPLEMENTATION-READY** (2026-07-19). Codex rounds 1–2 folded (review log at
the bottom; round 2 verdict: "implement with edits" — this rev IS those edits). Task #119 (high);
lands #109 item 3 (decode content identity) as a side effect. Owner directive: "holistically
address the root cause and bolster the architecture around this whole system so that we never
hit this class of problem again." Executes on `main` (wt1 owns #120 the diagnostics panel; the
mac agent's #121 poster-walk driver + #109.1 shell supersession touch disjoint files)._

## The ask (owner, 2026-07-19)

> Blaze through a folder, outrun the buffer, stop — picture focuses in ~0.5 s. Hit F:
> fullscreen, blurry, then clears. Hit F again: windowed, blurry, then clears. Repeat 4–5
> times before toggles finally become instant — "it should take 0 once I'm stopped for a few
> seconds." Then advance to the next photo: blurry again, "it should already be preloaded."

## Current reality (verified, with anchors; corrected per Codex r1)

The pipeline has **one geometry epoch doing two jobs** (staleness of *viewport-sized* work AND
staleness of *deck-scoped* work), and each work kind carves out its own exemption ad hoc — or
gets none:

1. **The toggle storm (the #119 bug).** A fullscreen toggle bumps the geometry epoch
   (`rebuild_ring`, `app_core_impl.rs:8086`). Then:
   - `DecodePool::set_targets` cancels **every** queued/in-flight job whose purpose isn't
     `PosterSelect` on an epoch change (`decode_pool.rs:539-557`) — including
     `RepKind::Original` decodes, which are viewport-independent (native size, `fit: None`).
   - `drain_results` drops **any** outcome whose `key.epoch` mismatches
     (`app_core_impl.rs:7621-7622` "stale geometry") — an Original finishing mid-toggle is
     discarded on landing.

   So the parked full-res Original (#106.7 tier, built at `app_core_impl.rs:6113-6185`,
   appended below the thumb fills at `:6257-6259`) restarts from zero on every F press. Until
   it survives to residency, every toggle is a full CPU Fit re-decode behind a stretched held
   frame (= the blur). Once it lands, item-6 retain/remap + `resize_hold` + GPU derive make
   every later toggle instant — exactly the observed "suddenly consistently clear after 4–5
   tries." The blurry advance is the same kill: each toggle also restarted the neighbours'
   Fit refills and their parked Originals, so derive-on-nav had no source.

2. **Three work kinds are geometry-independent today, and each is handled differently:**

   | Work | Pool cancel on epoch bump | Drain gate | Content identity |
   |---|---|---|---|
   | `PosterSelect` | exempt (special-case, `decode_pool.rs:546-555`) | routed before the gate (`app_core_impl.rs:7585`), own content fence (`:7448-7451`) | **smuggled in `key.epoch`** (`decode_pool.rs:109-112`) |
   | `Thumb` | **killed** (same bug, lower stakes: a toggle with the strip open restarts every thumb fill) | routed before the gate (`:7594-7617`) with **no content check at all** — and `Thumbs::offer` stamps arrivals with the cache's generation *at arrival time* (`thumbs.rs:167/:183`), relabeling an old-deck outcome as current | none on the outcome |
   | Display `Original` | **killed** (the #119 bug) | **dropped** (`:7621`) | none |
   | Display `Fit` | killed — *correctly* (wrong size) | dropped — *correctly* | none |

3. **The epoch is also the accidental deck fence.** `invalidate_content`
   (`app_core_impl.rs:8072`) bumps `content_gen` *and then* the epoch (`:8086`; pinned by
   `invalidate_content_bumps_both_generations`; a resize bumps epoch only). The pool never
   sees `content_gen` — deck-change cancellation of non-selections works **only because the
   epoch happens to move too**. Exempting Originals from the epoch cancel without teaching
   the pool about content would break deck-change cancellation for them.

4. **The cross-deck dedup hole (#109 item 3 / audit finding #3).** `TrackedEntry.gen` is
   compared only for selections (`gen_ok`, `decode_pool.rs:569-573`: `Some(_) => true` for
   everything else) — a new deck's want for `(item, Display, Fit)` dedups against the *old*
   deck's still-in-flight job; only the coincidental epoch bump keeps stale results out.

5. **Staleness is checked in more places than the drain (Codex r1 f2/f3).**
   - `rebuild_ring` **unconditionally clears `pending_uploads`** (`app_core_impl.rs:8109`) —
     staged, already-paid-for Originals die on every geometry change here too.
   - `request_prefetch` absorbs channel results into `pending_uploads` *before* scheduling
     (`:5773`) and then suppresses replacement wants for every staged `(item, rep)`
     (`:5855`) — a stale staged outcome can suppress the fresh want that would replace it.
   So the law has **four** enforcement sites, not two: pool cancel, ingestion/staging,
   rebuild retention, drain admit.

6. **Content boundaries don't reliably quiesce the pool (Codex r1 f4).** The only production
   `set_targets` call is `:6263` (inside `request_prefetch`). `enter_empty_state` (`:3275`)
   invalidates content **without** retargeting the pool — old jobs keep decoding until some
   later prefetch. Today the epoch-cancel coincidence usually mops this up; under the new
   model it must be explicit.

7. **`work_pending` can sleep on surviving work (Codex r1 f5).** It observes neither pool
   jobs nor `pending_uploads` (`:335`; polled from the tick pump, `:1973`). A parked
   Original can still be in flight after every display Fit is resident and caught-up — the
   pump can sleep before the outcome is drained. Keeping more cross-epoch Originals alive
   makes this gap load-bearing.

8. **The thumb derive fence misses single-item content changes (Codex r1 f6).** Deck
   rebuilds call `thumbs.clear_deck`, but e.g. save-rotation invalidates content without
   advancing the thumb generation (`:675` area) — an in-flight derive from pre-edit pixels
   can land after the edit. (Codex also corrected rev 1: the thumb decode box is a fixed
   512×512, `thumbs.rs:21/:36` — there is **no** DPI-sized-thumb concern.)

9. **The ring already has the right model.** `ResidentRing::reserve_bytes`/`mark_resident`
   take `content_gen` explicitly; `Representation::Original` carries no geometry epoch while
   `Representation::Fit { geometry_epoch }` does (`pb-core/src/ring.rs:32/:331/:528`). The
   pool and the core's staging/drain are the only parts that don't speak this language.
   #109.4 (landed 2026-07-19) made the ring *bridge* fail loud; this plan makes the ring
   *feed* tell the truth.

## Design — one staleness law, enforced at every gate

> **Staleness is a declared property of the work, not an if-chain at each check site.**

```rust
/// What invalidates this job's result.
pub enum Validity {
    /// Depends on the viewport: stale when the geometry epoch moves OR the deck changes.
    Geometry,
    /// Depends only on the content: stale only when the deck (content generation) changes.
    Content,
}

pub fn validity(purpose: Purpose, rep: pb_core::RepKind) -> Validity {
    match (purpose, rep) {
        (Purpose::Display, pb_core::RepKind::Fit) => Validity::Geometry,
        (Purpose::Display, pb_core::RepKind::Original) => Validity::Content,
        (Purpose::Thumb, _) => Validity::Content,
        (Purpose::PosterSelect, _) => Validity::Content,
    }
}
```

The exhaustive match is the architectural guarantee: **a future purpose or rep does not
compile until someone declares its domain.** One law, four enforcement sites, one shared
predicate on the core side:

```rust
/// The ONE staleness predicate for a decode outcome (Codex r1 f1/f3: used by ingestion,
/// rebuild retention, and the drain — never re-derived inline).
fn outcome_is_stale(&self, o: &Outcome) -> bool {
    match validity(o.key.purpose, o.key.rep_kind) {
        Validity::Geometry => o.key.epoch != self.epoch || o.key.content_gen != self.content_gen,
        Validity::Content  => o.key.content_gen != self.content_gen,
    }
}
```

### Pool changes (`decode_pool.rs`)

- `DecodeKey` gains `content_gen: u64` — a real field, on every job. The `PosterSelect`
  smuggling (`key.epoch` = content gen) is retired: `key.epoch` is the enqueue-time geometry
  epoch for every job; `key.content_gen` is the content generation for every job.
  `Want::sel_gen` is deleted — every production selection want already passes
  `self.content_gen` (five construction sites: `app_core_impl.rs:6002/:6050/:6097/:6171/
  :6242`; only the last is the thumb-demand emitter), the same value `set_targets` will now
  carry once.
- `set_targets(epoch, content_gen, source, prioritized)`; `Inner` tracks both:
  - `content_gen` moved ⇒ cancel + drop **everything** (both domains die with the deck).
  - `epoch` moved ⇒ cancel + drop **`Validity::Geometry` jobs only**.
- `TrackedEntry.gen` becomes the job's `content_gen` for **all** purposes; the dedup check
  (`gen_ok` + the enqueue guard `:628-634`) requires a matching content generation uniformly
  — closing #109.3. No epoch comparison at dedup (Geometry jobs were purged by the epoch
  arm). The pointer-identity `untrack` guard (`:831-840`) is preserved untouched.
- `fn has_work(&self) -> bool` — cheap probe for `work_pending` (finding 7):
  `!queue.is_empty() || !tracked.is_empty() || outstanding > 0`. **`outstanding` is an
  `AtomicUsize` on `Shared` counting sent-but-not-yet-dropped outcomes** (Codex r2 hole 1):
  ordinary jobs untrack *before* the send (`decode_pool.rs:759-763` vs `:795`), so an
  outcome can sit in the channel with queue and tracked both empty — the pump could sleep on
  it. Incremented when the outcome's guard is built (before send), decremented in
  `BudgetGuard::drop` — which also covers zero-byte error outcomes (`:746` charges 0 bytes
  but the guard still exists). Between drain-receive and upload the
  `!pending_uploads.is_empty()` pump arm covers the window, so guard-drop timing is safe.

### Core changes (`app_core_impl.rs` + `thumbs.rs`)

- **Ingestion** (finding 5): where channel results are absorbed into `pending_uploads`
  (`:5773` and the drain's own `try_recv`), stale outcomes (per `outcome_is_stale`) are
  dropped **before** staging — so the want-suppression pass (`:5855`) only ever sees valid
  work and a stale Fit can never suppress its own replacement.
- **Rebuild retention** (finding 5): `rebuild_ring`'s unconditional
  `pending_uploads.clear()` (`:8108`) becomes: geometry rebuild → `retain(!outcome_is_stale)`
  (Content-valid outcomes survive; Geometry ones drop); content rebuild → the retain
  naturally clears everything (content mismatch), same call, no special case.
- **Drain** (finding 1): the shared gate runs **before any purpose-specific routing** —
  PosterSelect and Thumb outcomes are staleness-checked first (a current-content, old-epoch
  selection passes as `Content`; its fitted artifact stays guarded by
  `fit_tag_epoch`/`fit_tag` exactly as today). `route_poster_selection`'s own fence
  (`:7448-7451`) stays as a redundant back-stop. The `:7621` epoch check is replaced by the
  same predicate. With stale thumb outcomes now rejected at the gate, `Thumbs::offer`'s
  arrival-time stamping is no longer reachable by cross-deck decode outcomes.
- **Pool quiesce at content boundaries** (finding 6): `invalidate_content` itself calls
  `pool.set_targets(self.epoch, self.content_gen, &self.source, &[])` right after bumping —
  cancel-everything is want-set-empty behavior, so every content boundary (including
  `enter_empty_state`) explicitly quiesces the pool instead of leaning on the epoch
  coincidence.
- **Pump** (finding 7): `work_pending` gains `|| !self.pending_uploads.is_empty()
  || self.pool.has_work()` so a surviving parked Original keeps the tick pump alive until
  drained, then the app sleeps as before.
- **Thumb derive fence** (finding 8 + Codex r2 hole 2): `invalidate_content` advances the
  thumbs generation **unconditionally** (one owner; `clear_deck`'s own advance becomes
  redundant-but-harmless, noted in its doc), **preserving the strip's viewport/follow
  state** — in-flight derives from pre-edit pixels are rejected on landing
  (`thumbs.rs:207`) and the strip re-derives. Deliberate trade (correctness over cache
  retention): rotations are rare, a strip refill is cheap, and a wrong-pixels tile is a lie.
  **`pending` bookkeeping becomes generation-aware** (keyed/valued by `(generation, item)`):
  today a late stale result removes the item-only `pending` marker (`thumbs.rs:201`)
  *before* the generation check (`:207`), erasing the replacement derive's marker — the
  marker that prevents duplicate video poster replays (`app_core_impl.rs:6215`). A late
  old-generation result must retire only its own generation's pending work. No
  `Thumbs::offer` acceptance-signature change beyond the generation plumb — the drain gate
  covers decode outcomes; the generation fence covers the derive thread.
- **Synthetic constructors** (Codex r1): `Outcome::synthetic(item, epoch, content_gen, rep,
  result)` takes both generations explicitly (stale-outcome tests must be able to
  manufacture stale data deliberately, and can't accidentally manufacture "current" data);
  `synthetic_from`/`synthetic_carved` **inherit `donor.key.content_gen`** — never the
  current generation.

### What deliberately does NOT change

- **Fit staleness.** A resize still cancels and drops wrong-size Fit work — that cancel is
  correct and load-bearing (decode-to-purpose).
- **Priority order.** The parked tier stays below thumbs (`:6120-6123`, owner-calibrated).
- **Presentation.** Nothing touches `present_*`, `resize_hold`, or the reverted
  invalidate-on-miss repair (`cff70ca0`/`c383107a` stays reverted; #109.5 is separate work).
- **Selection machinery.** `fit_tag_epoch`/`fit_tag` artifact staleness, promotion-in-place,
  replay, native-class admission, send-before-untrack ordering: untouched.
- **Scheduling classes.** Thumb cap, native permit: untouched.

### Fixes that fall out

- The toggle storm (the #119 repro) and the blurry advance after it.
- A toggle with the thumb strip open no longer restarts every thumb fill.
- Staged (`pending_uploads`) content-valid outcomes survive geometry rebuilds instead of
  being discarded after the decode was paid for.
- The cross-deck dedup hole (#109.3), for every purpose.
- Cross-deck thumb relabeling (`Thumbs::offer` arrival-stamping) and the save-rotation
  stale-derive window.
- Content boundaries quiesce the pool even on the empty-deck path.

## Phases

**Phase 1 — pool (`decode_pool.rs`), test-first.**
`Validity` + `validity()`; `DecodeKey.content_gen`; `set_targets` signature + two-arm cancel;
unified dedup generation; `has_work`. Pool tests:
- epoch bump: Original/Thumb/selection jobs stay tracked + queued, Fit jobs cancelled;
- content bump: everything cancelled (and `set_targets(.., &[])` cancels all);
- cross-generation dedup: an old-gen tracked job does not dedup a new-gen want (old flagged,
  new enqueues; `untrack` pointer-guard preserved);
- an Original outcome delivered after an epoch bump still arrives;
- queued selection promotion across an epoch preserves replay/native class and refreshes
  `fit_tag_epoch`/`fit` without restarting.

**Phase 2 — core (`app_core_impl.rs`, `thumbs.rs`), test-first.**
`outcome_is_stale`; ingestion filtering; rebuild retention; the gate-before-routing drain;
`invalidate_content` pool quiesce + thumbs fence; `work_pending` arms; `route_poster_selection`
reads `content_gen`; synthetic constructor migration. Core tests (the Codex r1 matrix):
- **the repro pin:** an Original outcome with a stale `key.epoch` but current `content_gen`
  is admitted, reserved, marked resident;
- a stale-epoch Fit outcome is still dropped; a stale-content Original is dropped;
- a stale-content Thumb outcome is rejected before `Thumbs::offer`;
- a thumb derive accepted before a single-item content change cannot land after it;
- a stale staged Fit cannot suppress its replacement want; a stale-content staged outcome
  cannot suppress a new deck's same-index want;
- geometry rebuild retains staged Original/Thumb/selection outcomes, drops staged Fit;
  content rebuild drops all four kinds;
- Fill/Original display mode presents a current-content, stale-epoch Original;
- a parked Original finishing after all display Fits are resident keeps the pump awake
  (`work_pending`) and lands;
- **the handoff race pin (Codex r2):** `has_work()` is still true after a worker has sent +
  untracked but before the receiver drains (synchronize that exact state; include a
  zero-byte error outcome);
- a direct truth table for `outcome_is_stale`: stale-epoch/current-content → Fit stale,
  Original/Thumb/PosterSelect valid; stale-content → all four stale;
- the stale-ingestion suppression tests feed the stale outcome through the **results
  channel** (receiver-ingestion ordering), not by hand-inserting into `pending_uploads`;
- Fill and Original modes table-driven for the stale-epoch Original present;
- rebuild retention releases dropped pool-backed outcomes' byte-budget guards and holds
  retained ones until drained (pool-backed, not synthetic-only);
- thumb generation overlap: stale pixels never enter the cache; viewport/follow/pending
  scroll preserved; a replacement derive's pending marker survives the stale result's
  arrival; the replacement lands;
- empty-deck content invalidation cancels old queued/in-flight pool work;
- `synthetic_from` **and** `synthetic_carved` both preserve the donor's content generation.

**Phase 3 — proof, docs, follow-through.**
- `PB_SHARP_DIAG` line when a cross-epoch Original is admitted (visibility the fix fires).
- Owner manual verify: the engagement-shoot repro; expected — at most ONE blurry toggle ever
  (racing the very first Original decode), instant thereafter; advance-after-settle sharp.
  `PB_PERF` resize episode for numbers.
- Docs: pool module header, tech-debt audit finding #3, #109 description (item 3 done),
  #119 → done, CHANGELOG ("Fixed: fullscreen toggles and advancing right after them no
  longer re-blur once a photo's full-resolution decode is cached").

## Risks / edge cases (dispositions)

1. **Fill/Original display mode.** The display decode is `fit=None` ⇒ `Original` ⇒ Content:
   a toggle no longer cancels it; presenting an admitted survivor is correct (native pixels
   are viewport-free; `present_item` recomputes the view, `:7215`). The parked "other rep"
   Fit stays Geometry.
2. **Two Originals racing.** Two *current-generation* Originals are impossible: one tracked
   `(item, Display, Original)` entry per content generation. A cancelled decoder from the
   previous generation may physically overlap its replacement until it observes
   cancellation; the content fence makes that overlap safe (its result is dropped).
3. **Thumb box.** Fixed 512×512 (`thumbs.rs:21/:36`) — genuinely geometry-free; `Content` is
   the true domain, not an approximation.
4. **wt1 / mac-agent collision.** #120 (diagnostics) reads AppCore; #121 (poster driver)
   lives in pb-decode; neither touches `decode_pool.rs` or the drain. Land promptly.
5. **Pump wake cost.** `has_work` is one mutex lock per idle tick; the pump already ticks
   while any work exists. No hot-path cost (blazing keeps the pump awake anyway).

## Review log

- **r2 (Codex, 2026-07-19, same session):** verdict **"implement with edits"** — r1 folds
  confirmed correct; two remaining holes: (h1) `has_work` misses sent-but-undrained outcomes
  (untrack precedes send, `decode_pool.rs:759/:795`) → the `outstanding` atomic (counting
  zero-byte errors too); (h2) thumb `pending` is item-only (`thumbs.rs:87`) and a late stale
  result removes the marker (`:201`) before the generation check (`:207`), erasing the
  replacement's marker (guards the poster-replay dedup, `app_core_impl.rs:6215`) →
  generation-aware pending. Plus: centralize the thumbs fence in `invalidate_content`
  (single owner), test sharpening (truth table, handoff-race pin, channel-fed ingestion
  tests, table-driven Fill/Original, budget-guard release, `synthetic_from` too), and anchor
  fixes (`:8109`; the five selection sites `:6002/:6050/:6097/:6171/:6242`; "two
  current-generation Originals"). **All folded into rev 3.**
- **r1 (Codex, 2026-07-19, session 019f7cb1):** verdict "needs another rev" — Validity model
  affirmed; six findings: (f1) drain gate not total — Thumb routed before it with no content
  check, `Thumbs::offer` relabels old-deck arrivals; (f2) `rebuild_ring` clears
  `pending_uploads` unconditionally, contradicting the survival claim; (f3) stale outcomes
  poison ingestion/want-suppression before the drain; (f4) content boundaries
  (`enter_empty_state`) don't retarget the pool; (f5) `work_pending` blind to pool +
  staged work; (f6) thumb derive fence misses single-item content changes; plus anchor
  corrections (drain gate `:7621`, thumb routing `:7594-7617`, parked append `:6257-6259`,
  five `sel_gen` sites), the fixed 512×512 thumb box correction, and synthetic-constructor
  generation rules. **All folded into rev 2** (the four-site enforcement, `outcome_is_stale`,
  rebuild retention, pool quiesce, `has_work`/pump arms, thumbs fence, test matrix).
