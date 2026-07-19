# 114 — One-run poster pipeline: choose once, keep an Original, derive everything else

_Status: **rev 4 — IMPLEMENTATION-READY** (2026-07-19). Codex rounds 1–3 folded (review log at
the bottom; round 3: "after those [six contract edits], rev 3 is ready to implement" — this rev
IS those edits). Owner green-lit 2026-07-19 ("implement the complete plan after your final
review lands"). Branch: `feat/114-poster-pipeline`._

## The ask (owner, 2026-07-19)

> "We should have _one_ run to generate posters for movies. It seems that there are currently
> several. The idea to downscale and judge based on the smaller image is both more effective AND
> more efficient, so that'd be a good place to start. We should create an original for films —
> the biggest they could be is 4K, more commonly 1920×1080, which is nothing anyways to keep an
> original. Then we never have to re-derive it if a user changes window size or scaling."

## Current reality (verified, with anchors)

1. **Multiple walks per movie.** The thumb strip and the display view each run the FULL scored
   walk (Thumb-purpose videos route through the normal poster path, engine.rs:492), and the pool
   _deliberately_ dedups by `(item, purpose, rep_kind)` (decode_pool.rs:218) — so the two walks
   run **concurrently** (both wants assembled in one scheduler batch, app_core_impl.rs:5779/5859):
   two MF readers, two burst decodes, two deep-seek sequences per film, each multi-second on SMB.
2. **The scoring is resolution-dependent** — frames are scored AFTER scaling to the requested fit
   (MF negotiates each reader at the consumer's fit and scores the fitted sample,
   mf_poster.rs:286/:375) against fixed thresholds (video.rs:518). Symptom: the Ali Wong
   Netflix specials — thumb (small judge) skips the white vignette card, poster (large judge)
   accepts it. The FFmpeg backend already has the right architecture: raw winner retained,
   scoring through a reduced converter, winner converted once at the end
   (ffmpeg/poster.rs:167/:193/:303 — `cfd55ffe` changed FFmpeg, **not** MF).
3. **No Original for videos** — `full_res_eligible` excludes them (app_core_impl.rs:6207), a
   rule written before #110. A resize drops the poster Fit with nothing to derive from → **every
   viewport resize re-runs the walk** (the owner's spinner), while photos re-derive in a frame.
4. **Eviction → full re-walk** (the observed Gremlins ×3 churn) — nothing remembers the choice.
5. **Transient failures are permanent for the session** — `thumbs.failed` is never re-planned
   (app_core_impl.rs:7046); display failures insert `failed` (:7097/:7430) and a failed current
   target is immediately marked caught-up (:6996). One SMB hiccup = a blank tile all session.
6. **Seek-refusing containers lose even their head frames.** Raw Blu-ray `.m2ts`: a deep
   reader's scan error propagates via `r?` and **discards the already-found head best**
   (mf_poster.rs:428) — `SetCurrentPosition` failures are already survived (:424); it's the
   deep-scan error path that throws the walk's work away. `0xC00D36E5` = MF_E_INVALID_POSITION,
   typed at the COM layer only (lost after `mf_open_msg` conversion).
7. ~~Cancelled walks logged as failures~~ — fixed (`cb65b6d6`, `DecodeError::is_cancelled`).

## Design

### 1. One selection per movie: the `PosterSelector` state machine (round 1 reshaped this)

A bare `item → PosterChoice` map cannot dedupe the two walks (they're scheduled in the same
batch before any choice exists). The selection becomes **purpose-neutral, explicit state** in
AppCore:

```
PosterSelection = Absent
               | Selecting { content_gen, consumers: {Thumb?, Display?}, retries }
               | Chosen(PosterChoice)
               | WaitingForReentry { retries }   // §4 (retry re-enters Selecting)
               | Terminal                        // failed twice — honest blank
```

- `Selecting` is installed **before** `set_targets`; while it exists, neither purpose schedules
  a second walk — the selection job is the ONE walk, and `consumers` is the **union** of thumb +
  display demand (a display-cancel must not kill a selection the thumb still needs; cancel only
  when the union empties).
- **The selection is its own pool work kind — geometry-neutral, level-triggered, class-mutable**
  (round 2; a selection dressed as a Fit/Original display want inherits three wrong behaviors):
  - Keyed `(item, content_gen)`, **not** the geometry epoch — the pool cancels every tracked job
    on an epoch change (decode_pool.rs:295) and `pending_uploads` is cleared on ring rebuilds
    (app_core_impl.rs:7492), but a selection is geometry-independent by definition: it must
    survive a mid-walk resize, and its outcome must not ride the geometry-scoped pending list.
  - **Re-emitted in every `set_targets`** while the consumer union is nonempty — the pool is
    level-triggered and cancels wants that stop being asked for (decode_pool.rs:314); `Selecting`
    without a live job is the bug, not a state.
  - **Stable identity, mutable scheduling class** (round-2 occupancy finding): a thumb-only
    selection counts against the thumb admission cap (decode_pool.rs:391 — far-away movies must
    not occupy every worker), and when display demand joins, the job is **promoted** to display
    class without changing identity — never cancel/restart (the per-purpose dedup identity would
    otherwise force exactly that). **The promotion contract** (round 3): scheduling class is a
    field on the QUEUED job, separate from purpose/consumer identity; promotion mutates that
    field in `set_targets` and **notifies the condvar** — a bare priority change leaves the job
    parked behind the thumb cap, because `pop_best` filters by class while the cap is full
    (decode_pool.rs:485). An already-running selection is never touched: it keeps a
    `took_thumb_slot` bit from admission so it releases exactly the slot it took
    (decode_pool.rs:402/:461).
- **A typed selection result** rides the outcome seam (the pool's `DecodeFn` returns one
  `DecodedImage` today, decode_pool.rs:57 — the selection outcome instead carries
  `PosterChoice + artifacts`), **matched at the top of `drain_results`** — before the thumb
  branch (:7033) and the display rep routing (:7171), fenced by `content_gen` (round 2: the
  existing branches move the single image and cannot fan out). **Artifacts are produced on the
  worker, never the event loop**: the selection job already holds the native buffer off-thread,
  so it cuts the thumb tile AND the display-size Fit there and returns all three; the drain only
  routes. The display Fit is included even when Original admission is denied (§3), so the screen
  always gets its poster. The payload implements summed byte accounting
  (`OutcomePayload::bytes()`) — multiple pixel buffers must all count against the pool budget.
  **The Fit artifact is geometry-tagged** (round 3): it carries the `(epoch, FitBox)` it was cut
  for, because the selection itself survives a resize but its Fit is viewport-specific — normal
  Fits are epoch-rejected in the drain (:7055) and staged results are cleared on rebuild
  (:7493), and an untagged selection Fit would bypass both. A stale tag drops **only that
  artifact** (choice/thumb/native survive); the poster then comes from deriving the admitted
  Original, or — when no Original was admitted — a geometry-scoped recut (replay at the current
  fit) is enqueued.
- `PosterChoice` stores **the absolute backend seek locator** — `{ origin, relative_ts }` —
  because replay must seek `origin + relative_ts` exactly as playback does
  (mf_video_producer.rs:220/:326); MPEG-TS files have nonzero origins, so a bare
  session-relative PTS is the wrong replay coordinate (round 2 caught rev 2 claiming
  otherwise). The timestamp is captured from `ReadSample`'s output, which the walk currently
  discards entirely (mf_poster.rs:368). **And the origin applies to the walk's OWN deep seeks**
  (round 3): the head scan establishes the origin from its first sample, and every deep
  `SetCurrentPosition` seeks `origin + offset` — today's bare-offset seeks (mf_poster.rs:457)
  carry the same MPEG-TS coordinate bug, and without this the §5 invalid-position fallback would
  merely mask it by returning the head frame.

### 2. Fixed-size judging + native-winner retention (one pass, no replay needed)

Round 1's key structural point: on MF, negotiating the reader at 256 px gives cheap judging but
**no native winner to keep**, and a PTS is NOT a "seek once, grab the frame" key (MF seeks land
on a preceding keyframe and decode forward — the playback seek path already implements the
decode-forward-until-`ts ≥ target` algorithm, mf_video_producer.rs:213/:252). So the two ideas
fuse into one contract, per backend:

- **MF (Windows) — two variants, A/B-gated on the corpus** (round 2: "small added cost" was
  unsupported — native output makes EVERY candidate pay a full-res RGB conversion+copy
  (mf_video.rs:352) plus a CPU reduction, and the existing Lanczos helper consumes its source
  (common.rs:319), so a scratch-reusing borrowed reducer is needed either way):
  - **Variant A (native walk)**: negotiate the reader at the capped edge (§3 ceiling);
    per candidate, reduce to the judge size (`POSTER_JUDGE_WIDTH` = **256 px**, owner-locked)
    and score that; retain the best-so-far native buffer. Ends holding the winner — no replay.
  - **Variant B (fitted walk + winner replay)**: keep today's fitted negotiation (cheap
    candidates), score on a judge-size reduction (consistency fixed either way), then acquire
    the winner once at the end via the decode-forward replay (one GOP cost, amortized).
  - The A/B records walk latency, candidates reached before the 15 s deadline, and CPU time
    over the movie corpus; the winner ships, the loser stays behind the seam.
  - **Active native RAM is bounded by a permit, at the SCHEDULER level** (rounds 2+3): during a
    native walk the retained best + current candidate ≈ two native buffers coexist per
    selection, and the pool's `inflight_bytes` only counts bytes AFTER a decode returns
    (decode_pool.rs:414) — eight UHD walks ≈ 0.5 GiB invisible to the budget. The permit covers
    **native-sized work in both variants** (Variant B's fitted output can equal native, its
    winner replay always materializes a native frame, and the worker-side fp16 conversion peaks
    on top), and it is part of the pool's **runnable/admission predicate** (the `pop_best`
    layer, like the thumb cap): a native job without a permit stays queued while lighter work
    runs. Never acquired inside the decode closure — that would park workers behind native jobs.
- **FFmpeg (mac/Linux)**: already two-scale — replace its private 480-edge reduction
  (ffmpeg/poster.rs:337) with the shared judge constant; it retains the raw winning `AVFrame`
  and converts once at the end, which becomes "convert at native" — HW decode and the two-scale
  architecture untouched.
- **Honest efficiency accounting** (round 1): fixed-size judging buys _consistency_ (the gate
  stops being resolution-dependent — the Ali Wong fix); the per-candidate downscale on MF is a
  small added cost, not a saving. The _efficiency_ comes from §1 (half the walks) and §3 (no
  re-walks ever again). Known limitation: FFmpeg's **HDR** poster scorer is brightness-only
  (ffmpeg/poster.rs:202) — the title-card detail gate does not cover HDR films yet; noted, out
  of scope.
- **The replay contract** (Variant B's winner acquisition, and the recovery path for an evicted
  Original either way): fresh reader + `SetCurrentPosition(origin + relative_ts)` +
  decode-forward until the absolute timestamp matches (the playback algorithm — fresh-reader
  positioning avoids the warm-HEVC ~1 s trap). It can decode a whole GOP; **benchmark over SMB
  with HEVC/long-GOP material before relying on it** (prime directive), and it must reproduce
  the _same_ frame by timestamp match, never "first frame after seek".

### 3. The chosen frame becomes the video's Original (the resize fix)

Round 1 verified the core claim: **#110 derives videos with zero render changes** —
`try_gpu_derive_fit`/`try_gpu_sharpen` key on target identity + a resident Original slot
(app_core_impl.rs:5491/:5591) and renderer eligibility is texture-based, not kind-based
(gpu.rs:3759). The orchestration contract, made explicit:

- The typed selection result **explicitly installs** the native winner as `RepKind::Original`
  (mipped) — never a Fit-keyed job silently returning native pixels.
- **Admission is demand-gated**: a thumb-only selection (movie far from the cursor) stores the
  choice, cuts the thumb, and does NOT upload a 44–88 MB Original — the native frame uploads
  only when display/parked demand admits it (the parked tier's normal rules). Until then the
  retained native buffer is dropped; re-admission uses the §2 replay path.
- `full_res_eligible` gains a video arm gated on `Chosen` **and** on
  `native_w × native_h` against a hard pixel/byte ceiling — "videos are at most 4K" is folklore,
  not an invariant. **The ceiling is a 4096 edge** (owner-accepted 2026-07-19, non-native above
  it): a 16:9 film on the 7680×2160 display fits height-bound at 3840×2160, so a native 4K
  poster displays 1:1 and the cap only ever bites >4K sources. VRAM stays boring because
  admission is demand-gated — a folder of five hundred 4K movies holds parked-window-many poster
  Originals (~3–7 × 44 MB), never five hundred.
- **Wide-gamut SDR posters need a derivable representation AND a storage/content split**
  (rounds 2+3): MF can return an enabled P3 transform, which the renderer stores as **mode 1 —
  deliberately unmipped and rejected as a derive source** (gpu.rs:1862/:3759). The fix is
  fp16 scene-linear — but naively that's wrong too, because `is_hdr` equates every `Rgba16F`
  image with HDR *content* (engine.rs:352) and the renderer's single `hdr` boolean selects both
  the fp16 storage mode and the HDR luminance scaling (gpu.rs:2932/:3679/:3705) — an SDR-P3
  poster would present at the wrong brightness on an HDR desktop. So: **storage format and
  content range split** (fp16 storage + `content_hdr = false`; the `RingSlot.content_hdr` field
  from #110 is the seam), brightness keyed on content, format on storage. The conversion is
  **baked on the worker** — the existing TRC + source→BT.709 matrix applied to scene-linear
  scRGB fp16 (mode 2 accepts only canonical scRGB and ignores `ColorTransform`, gpu.rs:109) —
  and it applies to **every enabled SDR matrix transform**, not a P3-labelled special case.
  (thumb.rs:119 has the TRC/matrix math; its clamped sRGB8 bake is NOT reusable verbatim.) An
  SDR-P3 resize regression test pins colors AND brightness.
- **Format accounting** (round 1 correction): Windows MF posters are RGBA8 (PQ/HLG
  SDR-clamped by design, mf_video.rs:298) → 4K ≈ 44 MB mipped; FFmpeg HDR posters are fp16
  (ffmpeg/poster.rs:396) → 4K ≈ **88 MB**. Both count against the parked quota (and #112's
  `parked_original_quota` when it lands).
- Wording fixed (round 1): this is a **geometry-independent poster Original** with the current
  poster color policy — NOT "the same reader configuration as playback" (playback may take the
  NV12 hardware path while posters are RGB32, mf_video_producer.rs:97). Rotation is fine (the
  advanced processor emits display-oriented pixels; the derive is texture-agnostic). Verified by
  HDR/rotation/DoVi corpus tests, not bit-identity claims.
- A resize then GPU-derives the video's Fit exactly like a photo — no walk, no decode, no
  spinner. The thumb tile is cut from the same native frame at choice time: **poster == thumb by
  construction**.

### 4. Bounded session retry — a state machine, not a counter (round 1 reshaped this)

The failed sets are terminal gates all over planning (skips at app_core_impl.rs:5790/:5871,
inserts at :7046/:7097/:7430, caught-up stamping at :6996), so "remove from the set and retry"
either loops or never schedules. Instead, per item:

`WaitingForReentry { retries } → RetryInFlight → Recovered | Terminal`

- **Re-entry is an absent→present demand edge** (the item left the window/targets and came
  back), not "is visible this tick" — that's what bounds it.
- The retry count is preserved _before_ enqueue (a second failure transitions to `Terminal`;
  cancellation of the retry job does **not** consume the attempt).
- Poster-selection retry is **item-level across consumers** (one retry even if both thumb and
  display want it — no double walks through the back door).
- On `Recovered` for the current target: clear the caught-up/presentation state stamped by the
  failure and re-present, so the healed image actually appears.
- **Scope: all item kinds — photos included** (owner decision 2026-07-19): `thumbs.failed` and
  `AppCore::failed` gate photos exactly the same way, and a transient SMB read error on a photo
  thumb is at least as common as on a movie poster. This is the leading suspect for the residual
  "stuck with no thumbnails" sightings (the other suspect, the #109 deck-identity hole, stays
  separately tracked; a repro after this ships points squarely at #109). **But "kind-agnostic
  for free" was too glib** (round 3) — the machinery is per-DOMAIN, not per-item:
  - **Failure/demand domains are tracked separately for Display and Thumb** (separate gates at
    :5791/:5872, separate inserts at :7046/:7097): a photo can leave display demand while
    sitting in the much wider thumb window forever, so an item-level demand union would never go
    absent and a display revisit would never retry. Each domain has its own absent→present edge;
    one bounded item-level attempt budget spans both (no double-spend).
  - **Resident-preview full-decode errors are exempt**: they stay on the existing
    `upgrade_done` + watchdog path (:6013/:7086) and never enter `WaitingForReentry` — two
    recovery mechanisms on one item would fight.
  - **`load_current_sync` failures** (:7369/:7430) insert `failed` and stamp the target
    resolved; recovery must clear that gate AND invalidate `presented_epoch` before normal
    drain routing, or the healed image uploads without presenting.
  - Startup `initial_image` never enters `failed` (:7575) — its pool fill is the ordinary first
    attempt, no interaction.

### 5. Head-only fallback, at the layer that actually fails (round 1 narrowed this)

Classify `MF_E_INVALID_POSITION` (0xC00D36E5) **at the COM layer** (before `mf_open_msg` erases
the code): on that specific error from `SetCurrentPosition`/`ReadSample` in the deep phase, stop
trying deeper offsets and **return the accumulated head best** (the head phase is genuinely
sequential — `scan` never positions, mf_poster.rs:303). Do NOT blanket-convert deep-scan errors
into fallback success — that would mask real corruption; only the typed invalid-position class
degrades. Test both failure points (`SetCurrentPosition` and the post-seek `ReadSample`).

### 6. Optional cleanup: a real `Cancelled` error variant

As rev 1 — strictly cleaner, touches every decoder + matcher, low value while
`is_cancelled` (`cb65b6d6`) is the single knower. Last phase, skippable.

## Lifecycle + privacy (round 1 corrected the anchor)

Rev 1 said "cleared beside `meta_cache`" — wrong anchor: `invalidate_content` does **not** clear
`meta_cache` (it bumps generations + rebuilds the ring, app_core_impl.rs:7460); metadata is
cleared manually at deck rebuild/empty-state (:3232/:3351), one entry on saved rotation (:654),
and at teardown (main.rs:3784). So the selector gets an **explicit lifecycle method** —
`clear_poster_selections()` — invoked from `invalidate_content`, deck replacement/empty-state,
the saved-rotation single-entry path, and `clear_session_state` (teardown). Selection outcomes
carry `content_gen` **now** (not deferred to #109 — a late old-deck result may otherwise
reinsert a choice under a recycled index; when #109's deck identity lands in `DecodeKey`, the
selector adopts it as a strict upgrade). Geometry changes leave selections alone (verified:
:7449 + the regression at :15317).

Privacy: RAM-only, never serialized (a poster choice is a viewing-derived datum — ADR-018); the
disk-diff no-trace test cannot prove teardown clearing, so **direct lifecycle unit tests** cover
teardown/content-change wipes, and PTS/path data never enters metrics or persistent diagnostics.

## What this fixes (symptom → mechanism)

| Observed this session                            | Fixed by                           |
| ------------------------------------------------ | ---------------------------------- |
| Resize spinner on videos (~seconds)              | §3 Original + GPU derive           |
| Thumb shows a scene, poster shows the white card | §2 judging (+§3 same-frame thumb)  |
| White-ish posters on the Ali Wong specials       | §2 (the small-judge pick wins)     |
| Gremlins ×3 re-walk churn after eviction         | §1 selector + §2 replay recovery   |
| Two concurrent walks (SMB readers) per movie     | §1 purpose-neutral selection       |
| Rare permanently-blank thumbs on SMB hiccups     | §4 retry state machine             |
| BDMV `.m2ts` losing even their head frames       | §5 typed invalid-position fallback |
| "corrupt image: cancelled" console flood         | done (`cb65b6d6`)                  |

## Interactions

- **#110/ADR-024**: extends "display = pure function of a resident Original" to videos; render
  layer untouched (verified round 1).
- **#112**: the video Original counts inside `parked_original_quota`; the fp16 88 MB case
  informs that quota's sizing.
- **#109**: selection outcomes carry `content_gen` from day one; adopts the shared deck
  identity when it lands.
- **#92**: subsumes the divergence + white-card quirk; the macOS AVFoundation backend (#92.2)
  adopts the judge constant + selector via the shared policy (video.rs) when it lands.
- **79.10 (parallel branch)**: shared-file edges are `video.rs` constants and
  `mf_video_producer.rs` (79.10 rebuilds the producer; §2's replay borrows its _algorithm_, not
  its code). Coordinate at merge; land order free.

## Test plan

- **Judging**: synthetic clip (black lead → white vignette card → textured scene): the same
  timestamp wins at every requested output size (the Ali Wong regression, pinned); corpus
  threshold re-validation numbers recorded.
- **Selector**: thumb + display racing → exactly ONE walk (the pool sees one selection job);
  union-of-consumers cancellation (display cancels, thumb keeps it alive); `Selecting` installed
  before `set_targets`; typed result fans out to both consumers; content_gen mismatch drops a
  stale result.
- **Lifecycle**: direct unit tests — content change / deck replacement / teardown wipe
  selections; geometry change preserves them; saved-rotation clears that item's selection.
- **Original**: after `Chosen` + display demand, resize issues NO poster job and derives (fake
  renderer, #110-suite style); thumb-only selection does NOT upload; eligibility ceiling
  enforced; fp16-vs-RGBA8 byte accounting.
- **Replay**: decode-forward reproduces the chosen frame by timestamp (not first-after-seek);
  **measured** over SMB (HEVC long-GOP, DoVi, HDR) before the recovery path ships.
- **Retry**: absent→present edge triggers exactly one retry; second failure → Terminal;
  cancellation doesn't consume the attempt; recovery re-presents a current target.
- **Fallback**: mock reader failing at `SetCurrentPosition` vs at post-seek `ReadSample` — both
  return the head best; a non-position deep error still fails the walk (corruption not masked).
- **SDR-P3 resize regression**: a P3-tagged poster derives correctly after a resize (the mode-2
  install path), colors intact.
- **RAM permit**: N concurrent native selections never exceed the permit; the ninth walk parks
  until a permit frees.
- **No-trace**: existing disk-diff tests extended to a movie folder; lifecycle tests carry the
  teardown guarantee.
- **Measured end-to-end**: SMB movie-folder browse — walk count (expect ≈1/movie), total poster
  wall-time, reader opens; resize→sharp latency for a displayed video (expect walk-time →
  derive-time).

## Phases — ONE ARC (owner decision 2026-07-19: single arc, one testing round)

Round 2 also killed the rev-2 phase-1/phase-2 split: a typed result with no timestamp/native
capture is a fiction, so the seam and the minimum walk changes land together.

1. **Selector + typed seam + minimum walk changes**: the state machine (pool work kind,
   level-triggered re-emission, class promotion, lifecycle method), the typed fan-out result,
   and the walk changes that make the result real — timestamp/origin capture + judge-size
   scoring (256 px) + the FFmpeg judge-constant swap; corpus threshold re-validation.
   **1e (owner feedback during phase-1 testing, 2026-07-19): placeholder-first video
   presents.** Landing on a film whose walk hasn't finished used to HOLD navigation (the
   "miss = hold, never skip" gate is invisible for photos' millisecond previews but a
   multi-second wall for a poster walk). Fix: a video's preview want returns the existing
   flat tile instantly (zero I/O) marked `is_preview`, so nav presents at once — blazing
   streams placeholders — and the selection's fitted poster upgrades it in place through
   the normal preview→full machinery. The sync first paint of a movie folder gets the same.
   Skipping-not-ready-videos was considered and rejected: it breaks "space advances exactly
   one" and spatial memory; placeholder-first removes the stall without touching semantics.
   All three want tiers (previews / sharpen head / ring fulls) route resident-placeholder
   videos to their selection, never a bare display full (the legacy walk through a back
   door).
2. **The MF variant A/B** (native walk vs fitted+winner-replay) + the scratch-reusing reducer +
   the native-RAM permit; the winner ships.
   **DONE + MEASURED (2026-07-19, 48 films over SMB, `ab_poster_walk`)**: fitted 32.1 s total
   vs native 31.2 s — a wash at 1080p (identical negotiated dims), but native is **15–20%
   slower on the 4K files** (Dune 1356→1597 ms, Love Actually 1444→1692, No Hard Feelings
   2247→2622 — the per-candidate native conversion cost the review predicted). **0 pick
   mismatches across all 48** — the shared judge makes the variants choose identical frames
   (Ali Wong: ts=18.0 s, a real scene). **Verdict: `PB_POSTER_WALK` defaults to FITTED**
   (fastest time-to-poster while browsing); the native frame arrives via the parked replay
   pre-install below. The `NATIVE_WALK_CAP = 2` scheduler permit gates every native-class job
   (native walks + all replays) at admission, so light work flows past them (pinned by test).
3. **Original install + demand-gated admission + the 4096-edge ceiling + same-frame thumb +
   the replay path** (kills the resize spinner). **DONE (2026-07-19)** with one honest scope
   narrowing: **mode-0 only** — an enabled color transform (wide-gamut SDR) would store mode-1
   (unmipped, derive-rejected), so those posters skip the Original install (recorded in the
   selector's `original_blocked` memo so the parked tier never replay-loops them) and keep the
   color-correct replay path; their fp16 mode-2 bake lands with 110d, NOT here (avoids
   mid-arc pb-render upload surgery; the plan's storage/content split note stands for that
   work). The missing piece the A/B exposed: with fitted walks the first resize would pay a
   replay — so the **parked tier pre-installs a parked video's Original via replay in spare
   capacity** (the photo parked-fulls pattern), making the first fullscreen toggle an instant
   GPU derive. Replay = fresh reader + absolute seek (`origin + relative`) + decode-forward to
   the target timestamp (the playback algorithm), deadline-capped.
4. **Retry state machine (all kinds, photos included) + typed invalid-position head-best
   fallback.**
5. _(Optional)_ `DecodeError::Cancelled`; macOS backend parity (with #92.2).

All phases merge as one arc with one owner-testing round at the end.

## Owner answers (2026-07-19) — the former open questions

1. **Judge width: 256 px.**
2. **Retry covers photos too** — same mechanism, kind-agnostic; leading suspect for the residual
   blank-thumbnails sightings (deck-identity #109 remains the other, separately tracked).
3. **One arc**, one round of testing.
4. **4096-edge ceiling, non-native accepted above it.** Rationale recorded in §3: a 4K film
   displays 1:1 at 3840×2160 on the owner's display, so 4096 is not overkill — it is exactly
   native for the common worst case; RAM/VRAM stays bounded because Original admission is
   demand-gated (parked window, ~3–7 posters resident), never per-folder.

## Codex review log

- **Round 1 (2026-07-19, rev 1): NOT sign-off-ready.** 1×P0, 5×P1, 1×P2 — all folded into rev 2:
  the P0 (an MF PTS is not a seek-once-decode-once key; seeks land on keyframes and decode
  forward) split the design into **native-winner retention as the hot path** and a
  decode-forward **replay contract as the measured recovery path**; the choice map became the
  purpose-neutral `PosterSelector` state machine with a typed fan-out result (the pool dedups
  per-purpose, so a map alone can't stop the double walk); the lifecycle anchor was corrected
  (`invalidate_content` doesn't clear `meta_cache` — explicit lifecycle method + content_gen on
  outcomes from day one); Original admission became demand-gated with a real pixel ceiling and
  fp16 accounting (HDR posters are 88 MB, and MF posters are SDR-clamped RGBA8 — the
  "same-reader-as-playback" claim retracted); the retry became a state machine keyed on demand
  edges; the m2ts fallback narrowed to typed MF_E_INVALID_POSITION at the COM layer returning
  the head best. Round 1 confirmed: the double-walk reality, the resolution-dependent scoring
  (and that `cfd55ffe`'s two-scale lives in FFmpeg, not MF), that #110 derives videos with zero
  render changes, geometry-preserves-metadata, and privacy soundness.
- **Round 2 (2026-07-19, rev 2): NOT sign-off-ready — no P0, 6×P1 + 1×P2, all folded into
  rev 3**: the selection became a distinct geometry-neutral pool work kind keyed
  `(item, content_gen)`, level-triggered (re-emitted every `set_targets`), exempt from
  epoch-cancellation, with a stable identity + mutable scheduling class (thumb-cap vs display
  promotion without restart); the typed result is matched at the top of the drain, artifacts
  (thumb + display Fit) are produced on the worker (never event-loop CPU resizes), the display
  Fit ships even when Original admission is denied, and payload bytes are summed; MF native
  judging became an **A/B-gated variant** vs fitted+winner-replay (per-candidate native
  conversion cost is unproven) with a scratch-reusing reducer and an active-native-RAM permit
  (inflight_bytes counts nothing until a decode returns); the replay locator became absolute
  (`origin + relative_ts` — MPEG-TS origins made the rev-2 session-relative claim wrong); SDR-P3
  posters install as mode-2 fp16 (mode 1 is deliberately unmipped and derive-rejected —
  gpu.rs:1862/:3759 — so "zero render changes" was false for P3 until this); the retry enum
  gained its in-flight state; the impossible phase-1/2 split merged. Round 2 confirmed closed:
  the lifecycle correction, demand-gated admission + fp16 math, the retry concept, the
  invalid-position fallback, and the native-retention architecture itself.
- **Round 3 (2026-07-19, rev 3): no P0s, 6×P1 contract gaps — "after those, ready to
  implement."** All six are rev 4: the selection's Fit artifact is `(epoch, FitBox)`-tagged
  (stale drops only that artifact; derive-or-recut covers it); fp16 **storage** split from HDR
  **content** (`content_hdr=false` + worker-side scRGB baking for every enabled SDR transform —
  the single `hdr` boolean would have presented SDR-P3 posters too bright on an HDR desktop);
  retry split into Display/Thumb failure+demand domains with one shared attempt budget, the
  resident-preview/watchdog exemption, and the `load_current_sync` recovery contract; the
  `{origin}` applied to the walk's own deep seeks (not just replay — today's bare-offset seeks
  carry the MPEG-TS bug); the native-RAM permit widened to both variants and moved into the
  scheduler's admission predicate; the promotion contract spelled (class field mutated on queued
  jobs + condvar notify; running jobs keep `took_thumb_slot`). Round 3 confirmed: judge-256, the
  one-arc structure, and the 4096 ceiling are sound.
- **Owner sign-off 2026-07-19**: "implement the complete plan after your final review lands and
  you've implemented any important corrections" — rev 4 is those corrections; implementation
  begins on `feat/114-poster-pipeline`.
- **Phase-1 implementation review (2026-07-19, commits 2ab7d274..a3630382): no P0s, 7×P1 +
  3×P2 — all but one folded** in the follow-up commit: the artifact tag gained its `FitBox`
  half (a promoted thumb-only walk's ~thumb output could pass the epoch check into the display
  path); routing became representation-aware (Fill/1:1 displays the Original rep — the payload
  was hardcoded to Fit and landed in the wrong slot); selector demand became per-pass live
  union (`begin_pass`/`end_pass` — historical bits mis-classed jobs and poisoned the wrong
  failed set); the payload's byte-budget now TRANSFERS to the synthetic display outcome
  (`synthetic_from` — it was released while the Fit sat in `pending_uploads`); selections stay
  tracked until their outcome is SENT (the untracked-channel window could double-walk); fresh
  cores fence the selector to their starting `content_gen` (gen-0 vs gen-1 refused every
  install); FFmpeg's judge reduction is width-anchored like the shared reducer (long-edge
  judged portrait clips at a different scale than MF); `Thumbs.pending` marks tiles in the
  derive queue (in-derive read as evicted → extra walk); thumb-cut resize errors propagate
  (swallowed errors returned success-with-no-tile → eviction-loop walks). Confirmed correct by
  the review: the pool's epoch exemption / generation replacement / promotion / thumb-slot
  accounting / placeholder invariant, MF's ts+origin capture and origin-relative deep seeks,
  the box reducer, Windows gating (non-Windows byte-identical), and privacy. **Deferred to the
  measure phase (P2-10)**: the walk-level same-timestamp-at-every-size fixture test and the
  corpus re-validation of `POSTER_DETAIL_MIN` at judge scale.
