# 114 — One-run poster pipeline: choose once, keep an Original, derive everything else

_Status: **DESIGN DRAFT rev 1** (2026-07-19) — Codex review pending; owner sign-off pending.
**Do not implement** until green-lit._

## The ask (owner, 2026-07-19)

> "We should have _one_ run to generate posters for movies. It seems that there are currently
> several. The idea to downscale and judge based on the smaller image is both more effective AND
> more efficient, so that'd be a good place to start. We should create an original for films —
> the biggest they could be is 4K, more commonly 1920×1080, which is nothing anyways to keep an
> original. Then we never have to re-derive it if a user changes window size or scaling."

## Current reality (verified, with anchors)

The poster pipeline re-does expensive work constantly, and every observed symptom this session
traces to one of these:

1. **Multiple walks per movie.** The thumb strip and the display view each run the FULL scored
   walk independently (`engine.rs` routes Thumb-purpose videos through the normal poster path,
   ~engine.rs:492) — two MF readers, two burst decodes, two deep-seek sequences per film, often
   concurrently, each multi-second over SMB.
2. **The scoring is resolution-dependent**, because frames are scored AFTER scaling to the
   requested fit (`mf_poster.rs:288/:325` → `poster_frame_score` at `:384`) against fixed
   thresholds (`POSTER_LUMA_MIN`, `POSTER_DETAIL_MIN`, video.rs). Symptom: the Ali Wong
   Netflix specials — thumb (small judge) correctly skips the white vignette title card and
   finds a scene; poster (large judge) accepts the card. Same movie, same algorithm, two picks.
3. **No Original for videos** — `full_res_eligible` excludes them ("a video has no still
   full-res", app_core_impl.rs:~6208), a rule written before #110 existed. So a geometry change
   drops the poster Fit and there is nothing to derive from → **every viewport resize re-runs
   the entire walk** (the owner's couple-of-seconds spinner), while photos re-derive in a frame.
4. **Eviction → full re-walk.** A poster Fit evicted from the ring is re-created by another
   complete walk (the observed Gremlins ×3 / Hell-or-High-Water ×3 churn), because nothing
   remembers which frame was chosen.
5. **Transient failures are permanent for the session.** A thumb decode error inserts
   `thumbs.failed` — "never re-planned (no retry loop)" (app_core_impl.rs:7046) — and a display
   error inserts `AppCore::failed` (:7097/:7430). One SMB hiccup = a blank tile until the folder
   reopens. A plausible contributor to the owner's rare residual missing thumbnails.
6. **Seek-refusing containers fail entirely.** Raw Blu-ray streams (`BDMV\STREAM\*.m2ts`) make
   MF refuse positioned reads (0xC00D36E5); the deep walk is seek-based, so those files get no
   poster at all even when their head has usable frames.
7. ~~Cancelled walks logged as failures~~ — fixed ahead of this plan (`cb65b6d6`,
   `DecodeError::is_cancelled`).

## Design

### 1. Fixed-size judging (the owner's "downscale and judge" — start here)

The walk decodes/scales every candidate to one **fixed judge size** (`POSTER_JUDGE_WIDTH`,
proposed ~256 px wide, aspect-preserved) regardless of what the consumer wants, and scores at
that size. Effects:

- **Effectiveness**: the detail gate becomes resolution-independent — the thumb-quality pick
  (which the owner observed is *better*) becomes THE pick. Grain and vignette texture average
  away at judge size, so bright-but-flat title cards fail `POSTER_DETAIL_MIN` at every display
  size, which is the behavior the gate always intended.
- **Efficiency**: candidate frames convert/copy/score at ~256 px instead of display size (the
  codec decode itself is unavoidable; the per-candidate scale+copy+score shrinks).
- The judge size joins the shared walk-policy constants in `video.rs` (the "single source of
  truth both the MF and FFmpeg poster backends read"), so Windows/macOS/Linux keep parity.
- Thresholds re-validated once at judge size over the movie corpus (`\\beenas\Media\Movies`) —
  measured, not asserted: the diag line already prints mean/std/score/detail per pick.

### 2. `PosterChoice`: choose once per item (the "one run")

A RAM-only map `item → PosterChoice { pts, native_w, native_h }` (beside `meta_cache`), filled
by the first walk that completes:

- **Dedup above the pool**: if a walk for item X is in flight for either purpose, the other
  purpose does not start a second one — it consumes the choice when it lands. (The pool's
  DecodeKey dedup is per-purpose by design; the choice store is where cross-purpose sharing
  belongs.)
- **Every later need is a cheap seek-decode**: evicted Fit, new window size, thumb refresh —
  seek straight to the remembered PTS, decode ONE frame, done. The multi-second scored walk
  runs at most once per item per session.
- **Lifecycle**: cleared on content change (`invalidate_content`), NOT on geometry change.
  ⚠ Deck identity: this map must not recreate the #109 `DecodeKey` hole — it lives in AppCore
  and is cleared/rebuilt exactly where `meta_cache` is; when #109's deck-generation lands in
  `DecodeKey`, the choice store adopts the same key.
- **Privacy**: RAM-only, dropped on exit — a poster choice is a viewing-derived datum and must
  never be serialized (ADR-018).

### 3. The chosen frame becomes the video's Original (the resize fix)

After choosing, decode the winner ONCE at native resolution and upload it mipped as the item's
`RepKind::Original`:

- `full_res_eligible` gains a video arm: eligible **iff a `PosterChoice` exists** (the
  exclusion's original rationale — "no still full-res" — is obsolete once a frame is chosen; the
  poster IS a still). Doors/SVG/RAW exclusions unchanged.
- A resize/scale toggle then GPU-derives the new Fit from the resident Original **exactly like a
  photo** (#110 machinery, zero new render code) — no walk, no decode, no spinner. Videos and
  photos become behaviorally identical under resize.
- **The thumb tile is cut from the same native frame** at choice time (CPU downscale, existing
  resize path) — poster == thumb **by construction**, closing the divergence permanently.
- **Cost**: a 1080p RGBA8 mipped Original ≈ 11 MB, 4K ≈ 44 MB — counted against the parked
  quota like photo Originals (and against #112's `parked_original_quota` when that lands).
  HDR films: the MF poster path outputs SDR RGBA8 today; the Original stays whatever the poster
  produces (mode-0) — no new HDR surface here.
- Playback parity invariant unchanged: the choice is a PTS; the frame is decoded by the same
  reader configuration as playback (rotation/color parity by construction, as today).

### 4. Bounded session retry for transient failures

`thumbs.failed` / `AppCore::failed` entries gain **one bounded retry**: when a failed item
re-enters the visible window/targets, it may be re-planned once (per session, counter on the
entry). A real corrupt file fails twice and stays failed; an SMB hiccup heals on the next
approach. No retry loops, no timers — re-entry-driven, cheap, honest.

### 5. Head-only fallback for seek-refusing containers

When the walk's first deep seek fails with a positioning error (0xC00D36E5 class), degrade to a
**sequential head-only walk** (the existing head-burst phase already decodes frames 0..N without
seeking): pick the best head frame by the same judge-size scoring. Raw BDMV streams get a real
poster whenever their head has content, instead of nothing. (The FFmpeg-decoder fallback stays
rejected on Windows: our FFmpeg is deliberately demux-only, +3 MB vs +16 MB — #100.)

### 6. Optional cleanup phase: a real `Cancelled` error variant

Cancellation currently travels as `Corrupt("…cancelled")` with `is_cancelled()` string-matching
(`cb65b6d6`). Promoting it to a `DecodeError::Cancelled` variant is strictly cleaner but touches
every decoder + matcher; low value while `is_cancelled` is the single knower. Listed as the last
phase, skippable.

## What this fixes (symptom → mechanism)

| Observed this session | Fixed by |
|---|---|
| Resize spinner on videos (~seconds) | §3 Original + GPU derive |
| Thumb shows a scene, poster shows the white card | §1 fixed-size judging (+§3 same-frame thumb) |
| White-ish posters on the Ali Wong specials | §1 (the small-judge pick wins) |
| Gremlins ×3 re-walk churn after eviction | §2 PTS re-decode |
| Two concurrent walks (SMB readers) per movie | §2 one-run dedup |
| Rare permanently-blank thumbs on SMB hiccups | §4 bounded retry |
| BDMV `.m2ts` no-poster (0xC00D36E5) | §5 head-only fallback |
| "corrupt image: cancelled" console flood | done (`cb65b6d6`) |

## Interactions

- **#110/ADR-024**: extends "the display is a pure function of a resident Original" to videos.
- **#112**: the video Original counts inside `parked_original_quota`; no new budget knobs.
- **#109**: the choice store must ride the deck-identity fix, not fork a new stale-key surface.
- **#92**: subsumes the recorded thumb/poster divergence + white-card quirk; the macOS
  AVFoundation backend (#92.2) adopts §1/§2 via the shared constants when it lands.
- **#106.3** (thumbs warm window): unchanged; thumbs just get cheaper to (re)fill.

## Test plan

- **Judging**: a synthetic clip fixture (black lead → white vignette card → textured scene):
  the same PTS wins at every requested output size (the Ali Wong regression, pinned); threshold
  re-validation numbers from the corpus recorded in the plan/commit.
- **One-run**: with thumb + display purposes racing, exactly one walk runs (fake pool
  bookkeeping); the second consumer lands from the choice; choice cleared on content change,
  survives geometry change.
- **Original**: after a choice, resize issues NO poster job and derives (fake renderer, same
  style as the #110 suite); `full_res_eligible` video arm gated on the choice existing.
- **Retry**: transient-fail → re-enter window → one re-plan; second failure sticks; counter
  never exceeds 1.
- **Fallback**: a mock reader refusing seeks still yields the best head frame; a reader with no
  usable head degrades to the least-black frame as today.
- **No-trace**: the existing `viewing_a_folder_writes_nothing_to_disk` covers the choice store
  by construction (RAM-only); extend the movie-folder variant to assert it too.
- **Measured** (prime directive): SMB movie-folder browse — total poster decode wall-time and
  reader-open count before/after (expect: walks ≈ one per movie, re-requests ~free); resize→
  sharp latency for a displayed video before/after (expect: walk-time → derive-time).

## Phases

1. **Fixed-size judging** + shared constant + corpus threshold re-validation (small, immediate
   quality win, no structural change).
2. **`PosterChoice` store + one-run dedup + PTS re-decode path** (kills the churn).
3. **Poster-Original + video `full_res_eligible` arm + same-frame thumb** (kills the resize
   spinner; behavioral parity with photos).
4. **Bounded retry + head-only seek fallback.**
5. *(Optional)* `DecodeError::Cancelled` variant; macOS backend parity pass (with #92.2).

## Open questions (owner)

1. Judge width: 256 px (proposed) or 320 px? (Corpus A/B decides; 256 is the efficiency pick.)
2. Should the retry (§4) also cover *photo* decode failures, or videos/thumbs only for now?
3. Phase 3 ordering: land 1+2 first and ship, or take 1–3 as one arc?
