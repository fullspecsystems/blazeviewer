# 114 — One-run poster pipeline: choose once, keep an Original, derive everything else

_Status: **DESIGN DRAFT rev 2** (2026-07-19) — Codex round 1 folded (review log at the bottom);
round 2 + owner sign-off pending. **Do not implement** until green-lit._

## The ask (owner, 2026-07-19)

> "We should have _one_ run to generate posters for movies. It seems that there are currently
> several. The idea to downscale and judge based on the smaller image is both more effective AND
> more efficient, so that'd be a good place to start. We should create an original for films —
> the biggest they could be is 4K, more commonly 1920×1080, which is nothing anyways to keep an
> original. Then we never have to re-derive it if a user changes window size or scaling."

## Current reality (verified, with anchors)

1. **Multiple walks per movie.** The thumb strip and the display view each run the FULL scored
   walk (Thumb-purpose videos route through the normal poster path, engine.rs:492), and the pool
   *deliberately* dedups by `(item, purpose, rep_kind)` (decode_pool.rs:218) — so the two walks
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
               | Selecting { content_gen, consumers: {Thumb?, Display?} }
               | Chosen(PosterChoice)
               | WaitingForReentry { retries }   // §4
               | Terminal                        // failed twice — honest blank
```

- `Selecting` is installed **before** `set_targets`; while it exists, neither purpose schedules
  a second walk — the selection job is the ONE walk, and `consumers` is the **union** of thumb +
  display demand (a display-cancel must not kill a selection the thumb still needs; cancel only
  when the union empties).
- **A typed selection result** rides the outcome seam (the pool's `DecodeFn` returns one
  `DecodedImage` today, decode_pool.rs:57 — the selection outcome instead carries
  `PosterChoice { ts, native_w, native_h, hdr } + native winner pixels + judge metadata`), and
  the drain **fans it out** to every registered consumer: thumb tile cut + poster Fit + the
  Original install (§3). This typed seam is where "the other purpose consumes the choice"
  actually happens — it had nowhere safe to live in rev 1.
- `PosterChoice.ts` is captured from `ReadSample`'s timestamp output (the walk currently
  discards timestamps entirely, mf_poster.rs:368) and its coordinate is **defined**:
  session-relative (origin-adjusted — MPEG-TS files have nonzero origins).

### 2. Fixed-size judging + native-winner retention (one pass, no replay needed)

Round 1's key structural point: on MF, negotiating the reader at 256 px gives cheap judging but
**no native winner to keep**, and a PTS is NOT a "seek once, grab the frame" key (MF seeks land
on a preceding keyframe and decode forward — the playback seek path already implements the
decode-forward-until-`ts ≥ target` algorithm, mf_video_producer.rs:213/:252). So the two ideas
fuse into one contract, per backend:

- **MF (Windows)**: negotiate the selection reader at **native** (bounded by a hard edge
  ceiling); for each candidate, CPU-downscale to the shared judge size (`POSTER_JUDGE_WIDTH`
  ≈ 256 px) and score THAT; retain the **best-so-far native buffer** (exactly one frame resident
  at a time, same keep-if-better shape as today's `best.consider`). The walk ends holding the
  chosen frame at native res — no second pass, no replay.
- **FFmpeg (mac/Linux)**: already two-scale — replace its private 480-edge reduction
  (ffmpeg/poster.rs:337) with the shared judge constant; it retains the raw winning `AVFrame`
  and converts once at the end, which becomes "convert at native" — HW decode and the two-scale
  architecture untouched.
- **Honest efficiency accounting** (round 1): fixed-size judging buys *consistency* (the gate
  stops being resolution-dependent — the Ali Wong fix); the per-candidate downscale on MF is a
  small added cost, not a saving. The *efficiency* comes from §1 (half the walks) and §3 (no
  re-walks ever again). Known limitation: FFmpeg's **HDR** poster scorer is brightness-only
  (ffmpeg/poster.rs:202) — the title-card detail gate does not cover HDR films yet; noted, out
  of scope.
- **The replay contract exists but is the *recovery* path, not the hot path**: re-obtaining an
  evicted Original = fresh reader + `SetCurrentPosition(choice.ts)` + decode-forward until
  `ts ≥ choice.ts` (the playback algorithm — fresh-reader positioning avoids the warm-HEVC ~1 s
  trap). It can decode a whole GOP; **benchmark over SMB with HEVC/long-GOP material before
  relying on it** (prime directive), and it must reproduce the *same* frame by timestamp match,
  never "first frame after seek".

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
  not an invariant.
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
- The retry count is preserved *before* enqueue (a second failure transitions to `Terminal`;
  cancellation of the retry job does **not** consume the attempt).
- Poster-selection retry is **item-level across consumers** (one retry even if both thumb and
  display want it — no double walks through the back door).
- On `Recovered` for the current target: clear the caught-up/presentation state stamped by the
  failure and re-present, so the healed image actually appears.

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

| Observed this session | Fixed by |
|---|---|
| Resize spinner on videos (~seconds) | §3 Original + GPU derive |
| Thumb shows a scene, poster shows the white card | §2 judging (+§3 same-frame thumb) |
| White-ish posters on the Ali Wong specials | §2 (the small-judge pick wins) |
| Gremlins ×3 re-walk churn after eviction | §1 selector + §2 replay recovery |
| Two concurrent walks (SMB readers) per movie | §1 purpose-neutral selection |
| Rare permanently-blank thumbs on SMB hiccups | §4 retry state machine |
| BDMV `.m2ts` losing even their head frames | §5 typed invalid-position fallback |
| "corrupt image: cancelled" console flood | done (`cb65b6d6`) |

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
  `mf_video_producer.rs` (79.10 rebuilds the producer; §2's replay borrows its *algorithm*, not
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
- **No-trace**: existing disk-diff tests extended to a movie folder; lifecycle tests carry the
  teardown guarantee.
- **Measured end-to-end**: SMB movie-folder browse — walk count (expect ≈1/movie), total poster
  wall-time, reader opens; resize→sharp latency for a displayed video (expect walk-time →
  derive-time).

## Phases (after owner sign-off) — re-cut by round 1 (MF phase 1 is structural, not small)

1. **The selector + typed result seam** (state machine, union demand, content_gen, lifecycle
   method + tests) — pure orchestration, no walk changes yet; kills the double walk on its own.
2. **Walk rework**: MF native-negotiation + judge-downscale scoring + timestamp capture +
   best-native retention; FFmpeg judge-constant swap; corpus threshold re-validation.
3. **Original install + demand-gated admission + video `full_res_eligible` arm + same-frame
   thumb** (kills the resize spinner); the replay recovery path + its SMB benchmark.
4. **Retry state machine + typed invalid-position head-best fallback.**
5. *(Optional)* `DecodeError::Cancelled`; macOS backend parity (with #92.2).

## Open questions (owner)

1. Judge width: 256 px (proposed) or 320 px? (Corpus A/B decides; 256 is the proposal.)
2. Retry scope: videos/thumbs only (proposed), or photos too?
3. Ship after phase 2 (judging + one-run, quality + churn wins) or hold for phase 3 (the resize
   fix) as one arc?
4. The MF native-negotiation ceiling: cap at 4096-edge (proposed) — above it, negotiate capped
   and accept a non-native Original?

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
- **Round 2: pending** — re-review of rev 2 before owner sign-off.
