# 121 — One poster walk: a shared driver, three thin backends

_Status: **rev 2 — IMPLEMENTATION-READY**. Renumbered #115 → **#121** at the 2026-07-19 merge: the parallel Windows agent was concurrently using #115 for the compcol RAR task. Phase 1 landed as `7089c548`, whose message says "#115 phase 1" — same work. Codex round 1 folded (review log at the bottom:
no P0, 5×P1, 3×P2 — rev 2 IS those corrections). Owner ask 2026-07-19: "we desperately need
to DRY up this part of the codebase… can we make a reasonably bounded change that removes the
duplication and ensures that windows and mac versions are on the same poster code?"
Addresses technical-debt-audit findings **#5** (three poster extractors, no unifying trait)
and partially **#4** (platform routing sprawl). Follows task #114 (Windows one-run poster
pipeline) and subsumes **#92.2** (the macOS AVFoundation deep walk)._

## The ask, and what it is not

**Is:** one implementation of the poster *walk policy*, with each platform backend
implementing a small trait. macOS gets a real deep scored walk (#92.2) as a consequence of
implementing that trait rather than as a fourth hand-written copy.

**Is not:** the #114 selection pipeline on macOS. `PosterChoice`/select/replay, the
`PosterSelector` ledger, and `poster_select_supported()` are explicitly **out of scope**
(§Out of scope) — this plan is the foundation that makes that work small later, not the work
itself.

## Current reality (verified, with anchors)

### The duplication is near-verbatim

`deep_scan` exists twice — `mf_poster.rs:695` and `ffmpeg/poster.rs:352` — as the same
algorithm:

```
if duration is None            -> false          | identical
if dur < POSTER_DEEP_MIN       -> false          | identical
cap = poster_deep_cap(dur)                       | identical
last = ZERO                                      | identical
for off in POSTER_SEEK_OFFSETS:                  | identical
    target = off.min(cap)                        | identical
    if target <= 1s || target <= last: continue  | identical (same comment, verbatim, both files)
    last = target                                | identical
    if cancelled: Err                            | identical
    [MF only] if past deadline: return false
    seek(target)                                 | MF: reopen_at_rgb32 | FF: in-place avformat_seek_file
    scan(POSTER_BURST_FRAMES, best)              | same contract, different mechanics
    if good: return true                         | identical
```

`Best` exists twice — `mf_poster.rs:477` and `ffmpeg/poster.rs:122` — same fields, same
`consider`/`win` semantics, and the same doc sentence in both ("first max wins —
deterministic, so path and in-RAM posters stay bit-identical"). The only real difference is
what a frame *is*: `(Vec<u8>, u32, u32)` vs a refcounted `ff::frame::Video`. That is a type
parameter, not a design divergence.

`av_poster.rs` (150 lines) is a **third, much older** shape: first-bright-frame only
(`poster_frame_bright_enough`, `av_poster.rs:74`), capped at `POSTER_MAX_FRAMES` /
`POSTER_MAX_MEDIA` (`av_poster.rs:71`). No scoring, no seeking, no timestamps — #92.2 was
never implemented.

### How the drift happened (so we do not repeat it)

MF came first (#79). FFmpeg was written later for mac/Linux (#84) natively against ffmpeg
idioms. Improvements then landed on **different platforms at different times**: `cfd55ffe`
gave two-scale scoring to FFmpeg **not** MF; #114 gave origin capture, judge-size scoring and
the typed BDMV degrade to MF **not** FFmpeg.

**Sharing has already begun, because the drift caused a user-visible bug**:
`POSTER_JUDGE_WIDTH` + `poster_judge()` were hoisted into `video.rs:562/605` precisely because
resolution-dependent scoring made the thumb and the poster disagree (the Ali Wong divergence,
#92/#114). This plan finishes the move that bug started.

### What is genuinely backend-specific (must stay separate)

| Concern | MF (Windows) | FFmpeg (mac/Linux) | AVFoundation (macOS) |
|---|---|---|---|
| seek | **recreate** the reader (`reopen_at_rgb32:763`) — measured ~86 ms fresh open vs ~1 s warm HEVC reposition | seek **in place** (`avformat_seek_file`) | **must recreate** — `AVAssetReader` cannot seek; a new reader with a `timeRange` start |
| deadline | inline checks in `scan` (`:648`) **and** between seeks (`:720`) | `set_op_deadline` interrupt callback (`ffmpeg/io.rs:70`), fires only inside blocking libav work | inline |
| seek refusal | typed `MF_E_INVALID_POSITION` from **two** sites: `SetCurrentPosition` (`:763`) and post-seek `ReadSample` (`:658`) | untyped: any failure → next offset | untyped (for now) |
| frame handoff | RGBA8 at negotiated size | raw `AVFrame`, winner converted once | RGBA8 at reader scale |
| judging | `video::poster_judge` (RGBA8) | RGBA8 **or** scRGB fp16 for HDR (`ffmpeg/poster.rs:41/59`) | RGBA8 |
| timestamps | absolute hns, exact | optional `pts` + time base | `CMTime` (value/timescale, flags) |
| reader lifetime | **must be retired off-thread** (`retire_reader`; a plain drop can block ~1 s — `:892`) | RAII | RAII |

The seek mechanics, the HDR judge branch, the timestamp representation, and MF's reader
retirement are essential differences. The *policy* — which offsets, in what order, when to
stop, what a deadline means, what survives a refusal — is not.

## Design

### The seam

New module `crates/pb-decode/src/poster_walk.rs`. Platform-neutral: no `unsafe`, no
`cfg`, no I/O. It owns the policy; backends own the mechanics.

```rust
/// Why a seek did not land.
pub enum SeekError {
    /// The container refuses positioned reads (raw BDMV .m2ts;
    /// MF_E_INVALID_POSITION). No deeper offset can ever work: stop the walk and
    /// keep the head best (#114 phase 4).
    Refused,
    /// This offset failed; try the next one.
    Failed,
    /// The job was cancelled while seeking (the pool retired it).
    Cancelled,
}

/// What one burst concluded. THREE outcomes, not a bool: a burst can end the whole
/// walk (Codex P1-1 — MF's second invalid-position site lives inside `scan`, and
/// collapsing it into "found nothing" would silently turn a stop into
/// "try the next offset", regressing #114 phase 4).
pub enum ScanOutcome {
    /// A genuinely-good frame — the walk stops here; this is the poster.
    Good,
    /// Limit or EOF reached without a good frame — try the next offset.
    Exhausted,
    /// Stop the ENTIRE walk and keep `best`: the deadline expired, or the
    /// container refused positioned reads from inside the burst.
    Stop,
}

/// The best frame so far. Generic over the backend's frame representation so MF
/// (owned RGBA) and FFmpeg (refcounted AVFrame) share ONE ranking implementation.
pub struct Best<F> { /* score, frame, ts_hns */ }

impl<F> Best<F> {
    pub fn new() -> Self;
    /// Rank for the all-bad fallback; first max wins (deterministic).
    /// The frame is materialized ONLY if it actually replaces the incumbent —
    /// FFmpeg clones its refcounted AVFrame per replacement today
    /// (`ffmpeg/poster.rs:142`), and a by-value API would force a clone per
    /// candidate (Codex P2-1).
    pub fn consider_with(&mut self, score: f32, ts_hns: i64, make: impl FnOnce() -> F);
    /// A genuinely-good frame: the winner outright, regardless of ranking.
    pub fn win(&mut self, frame: F, ts_hns: i64);
    pub fn take(self) -> Option<(F, i64)>;
}

pub trait PosterBackend {
    type Frame;
    fn duration(&self) -> Option<Duration>;
    fn cancelled(&self) -> bool;
    /// Position for the next burst. Recreate or seek in place — the backend's
    /// choice; the driver never learns which.
    fn seek(&mut self, target: Duration) -> Result<(), SeekError>;
    /// Decode up to `limit` frames from the current position, judging each into
    /// `best`. Judging lives here because the pixel format differs (RGBA8 vs
    /// scRGB fp16) — but every backend calls the SHARED judge in `video.rs`,
    /// never a private threshold. The backend KEEPS its own per-sample deadline
    /// check and reports expiry as `Stop` (Codex P1-2: removing MF's per-sample
    /// check at `:648` would let a full burst overrun the watchdog).
    fn scan(&mut self, limit: usize, best: &mut Best<Self::Frame>, deadline: Instant)
        -> Result<ScanOutcome, DecodeError>;
}

/// The ONE walk policy. `Ok(true)` = a genuinely-good frame was found;
/// `Ok(false)` = fall back to `best`'s ranked frame (which may still be
/// excellent). Deadline exhaustion is `Ok(false)`, never an error — a poster is a
/// background nicety.
pub fn walk_poster<B: PosterBackend>(
    backend: &mut B,
    best: &mut Best<B::Frame>,
    deadline: Instant,
) -> Result<bool, DecodeError>;
```

**`origin_hns` is deliberately NOT a trait method** (Codex P1-4 caught rev 1 contradicting
itself here). The origin is a property of the stream, needed only by the caller assembling a
`PosterChoice`; the driver never reads it. It stays a concrete accessor on each backend.
`Best::ts_hns` is **absolute**; the caller subtracts the origin.

### The driver body (the policy, once)

1. **Head phase** — `scan(POSTER_HEAD_FRAMES, best, deadline)`:
   `Good` → `Ok(true)`; `Stop` → `Ok(false)`; `Exhausted` → fall through.
2. **Deep phase**, only if `duration >= POSTER_DEEP_MIN`:
   - `cap = poster_deep_cap(dur)`; iterate `POSTER_SEEK_OFFSETS` shallow→deep
   - skip `target <= 1s` (head already covered) and `target <= last` (collapsed by the cap)
   - check `cancelled()` → `Err(cancelled)`; check `deadline` → `Ok(false)`
   - `seek`: `Refused` → `Ok(false)` (head best survives); `Cancelled` → `Err`;
     `Failed` → **recheck cancellation and deadline, then** `continue` (Codex P1-2: a cancel
     landing during the final seek would otherwise fall off the loop and return a poster)
   - `scan`: `Good` → `Ok(true)`; `Stop` → `Ok(false)`; `Exhausted` → next offset
3. `Ok(false)` — the caller assembles from `best`.

### Judging stays shared, at one call site per format

The judge cannot move into the driver (the driver never sees pixels), so the contract is:
**backends call the shared judge; they never define a threshold.** Hoist
`scrgb_frame_score`/`scrgb_frame_bright_enough` (`ffmpeg/poster.rs:41/59`) into `video.rs`
beside `poster_judge` as `poster_judge_scrgb`, math **moved unchanged**. Keep a no-resize
judge entry for FFmpeg, whose walk converter already outputs judge-sized pixels —
`poster_judge_frame` clones buffers even at ≤256 px (`video.rs:570`), so routing FFmpeg
through the resizing entry would add a per-candidate allocation (Codex P2-1). After this,
every judging threshold in the repo lives in `video.rs`.

### Timestamps: one conversion, explicitly specified (Codex P1-4)

Add to `video.rs` a single tested helper:

```rust
/// Rational timestamp -> absolute 100 ns units, in signed i128 with explicit
/// rounding and saturation. The ONE conversion every backend uses.
pub fn rational_to_hns(value: i64, num: i64, den: i64) -> Option<i64>;
```

Per backend:
- **MF**: already exact absolute hns (`mf_poster.rs:672`); passes through.
- **FFmpeg**: `pts` is **optional**. Convert via `rational_to_hns` with the stream time base —
  **never** `VideoFacts::pts_to_duration`, which clamps negatives and round-trips through `f64`
  (`probe.rs:61`). A missing PTS is synthesized deterministically, mirroring the producer's
  existing rule (`ffmpeg/video_producer.rs:559`); **silently substituting 0 is forbidden.**
- **AVFoundation**: `CMTime` → `rational_to_hns(value, 10_000_000, timescale)`. Validate the
  sample is numeric with a positive timescale; reject invalid/indefinite, and constrain
  nonzero epochs rather than ignoring them.
- **AV origin**: retain the track's native `CMTime` origin as backend state and set each deep
  range start to `origin + target`, not bare `target` — a track's time range may begin after
  zero.

**Deliberate non-change:** FFmpeg seeks relative to the declared `start_time`
(`ffmpeg/poster.rs:245`), not the first decoded PTS. Aligning it with MF's first-sample origin
may improve parity but is a *separate* behavior change and is **not** in this plan.

## Per-backend mapping

### MF (`mf_poster.rs`) — behavior must not change

- New `struct MfPosterBackend { input, reader, dims: (u32,u32,i32), origin, cancel, … }`.
- `seek` = today's `reopen_at_rgb32` (`:763`), mapping `is_invalid_position` (`:368`) →
  `SeekError::Refused`, everything else → `Failed`.
- `scan` = today's `scan` (`:637`), **keeping its per-sample cancel and deadline checks**
  (`:648`), mapping the post-seek `ReadSample` invalid-position error (`:658`) →
  `ScanOutcome::Stop` and deadline expiry → `Stop`. Both #114 phase-4 degrade sites survive.
- **Reader retirement is part of the backend's contract** (Codex P1-5). `retire_reader` moves
  the ~1 s COM teardown off the worker (`:892`). The backend must retire: the previous
  positioned reader before replacing it (`:730`), a failed reopen's reader (`:783`), and the
  active reader on **every** exit path — implemented as a `Drop` impl so no early return can
  leak one, and without creating a second COM reference to the head reader that later drops
  inline.
- **Neutrality requires** the deadline still be created at its current point, *before* stream
  facts and negotiation (`:523`).
- `poster_inner` (`:516`) keeps negotiation, the `native_walk` variant, and `PosterChoice`
  assembly; it calls `walk_poster` instead of `scan` + `deep_scan`.
- **Untouched**: `decode_video_poster_select` (`:168`), `cut_selection` (`:186`),
  `decode_video_poster_replay` (`:248`).

### FFmpeg (`ffmpeg/poster.rs`) — mechanical port, then one deliberate change

- `PosterWalk` (`:158`) already has `scan`/`seek` in the right shape; it becomes the trait impl.
- Errors change from `String` to `DecodeError` at this boundary (`poster_inner` already maps
  them at its edge). All seek failures map to `Failed`, so the refusal path is inert here —
  **behavior unchanged**.
- `Best` becomes `Best<ff::frame::Video>` and gains `ts_hns` (new, free).
- **Phase 3b is a real behavior change** (Codex P1-2 — rev 1 wrongly called phase 3
  behavior-neutral). Today the deadline is armed before facts/decoder/converter setup
  (`:274`) and enforced *only* by the interrupt callback during libav work (`io.rs:70`). Adding
  a driver check between bursts means expiry returns the accumulated best instead of
  attempting and failing further libav calls. Additionally, an interrupt-triggered packet read
  surfaces as a plain read error (`:224`) and must be **classified**: cancellation →
  cancellation error; elapsed deadline → `Stop`; otherwise → a real decode error.

### AVFoundation (`av_poster.rs`) — the new work (#92.2)

Rev 1 said "add an offset parameter", which does not implement the trait: today
`decode_live_motion_streaming` (`livephoto.rs:403`) creates, starts, exhausts and destroys the
reader inside one call (`:965`, `:1018`), leaving no handle to retain between `seek` and
`scan` (Codex P1-3). The adopted design:

1. **`seek` records a pending offset; `scan` invokes an offset-capable helper.** Smaller than
   a resumable session type and still an honest trait impl.
2. **Additive API only.** Rust has no default parameters: keep the public
   `decode_live_motion_streaming` signature **unchanged** and add an internal
   `decode_live_motion_streaming_at(path, max_long_edge, start: Option<Duration>, cancel,
   timed_emit)`; the public Live-Photo entry delegates with `None`.
3. **Do not add PTS to `AnimFrame`** (`animation.rs:69`) — it is constructed by every animation
   backend. Use an AV-private timed-frame callback (or a parallel internal event type).
4. **Positioning** = build a new `AVAssetReader` with
   `timeRange = CMTimeRange(start: origin + target, duration: .positiveInfinity)`; failure →
   `SeekError::Failed`.
5. **`scan`** = pull frames, call `video::poster_judge`, `consider_with`/`win`.
6. Delete the bespoke first-bright-frame logic (`av_poster.rs:74-82`).

**Intended behavior changes on macOS** (Codex P2-2 — all deliberate, all to be tested):
- head phase becomes frame-count-only; the `POSTER_MAX_MEDIA` ~1 s media cap (`:71`) is gone
- fallback becomes best-scored, not last-dark-frame
- stop condition becomes bright **and textured**, not first-bright
- a 15 s `POSTER_DEADLINE` now applies
- deep seeks past the intro now happen (the point of #92.2)

Watch low-frame-rate/VFR media, where the frame-count head phase now covers more wall-clock
media than the old 1 s cap allowed.

## Test plan

The driver is pure policy, so it gets the tests **no backend has today** — a scripted
`FakeBackend` (programmable per-burst outcomes and seek results, recording every call):

- head walk finds a good frame → **zero seeks issued**
- head fails → offsets tried shallow→deep, in order
- `target <= 1s` skipped; duplicate targets collapsed under the cap (a 30 s clip)
- `duration = None` and `duration < POSTER_DEEP_MIN` → no deep phase at all
- `SeekError::Refused` on the 1st deep offset → walk stops, **head best survives**
- **`ScanOutcome::Stop` from inside a burst → same degrade** (the second MF site — the
  regression rev 1 would have shipped; pinned platform-neutrally for the first time)
- `SeekError::Failed` → continues to the next offset
- **cancellation or deadline expiring during the LAST failed seek** → does not return a poster
- **deadline expiring inside a scan** → `Ok(false)` with the best so far, not an error
- `cancelled()` → `Err`, and `is_cancelled()` is true
- `Best`: first-max-wins determinism; `win` beats a higher-ranked earlier candidate;
  `ts_hns` rides the retained frame (not the last-seen one); `consider_with`'s closure runs
  **only** on replacement
- `rational_to_hns`: negative values, nonzero origins, saturation, rounding

Backend-level:
- MF: the existing classification test (`:1174`) stays; **note it does not cover either walk
  site** — that coverage is the driver's `FakeBackend` tests above.
- FFmpeg: existing poster corpus tests unchanged; new tests for missing-PTS synthesis and
  interrupt classification (cancel vs deadline vs real error).
- AVFoundation: **zero-offset Live-Photo regression tests** (the public entry must behave
  identically) land with phase 4a, before any poster change; nonzero offset yields an absolute
  PTS ≥ `origin + target`; a synthetic clip (black lead → white vignette card → textured
  scene) asserts the walk picks the scene; corpus tests gated on the existing
  `PB_LIVE_TEST_MOV`-style env var.

## Risks

1. **MF cannot be compiled or run on the macOS dev box.** The port is blind.
   Mitigation: the driver's tests are platform-neutral and run here; the
   `cargo check -p pb-app --target x86_64-pc-windows-msvc` cross-check (windows-cross-check
   memory) catches signature/struct breaks; Codex review; **owner smoke on Windows before this
   is trusted**. Counter-argument in favor: this refactor *moves logic that cannot be tested on
   this machine into code that can be tested on any machine*.
2. **Regressing #114 on Windows.** Mitigated by the seam's placement (`poster_inner` and
   everything above it is untouched) and by `ScanOutcome::Stop` preserving both degrade sites.
   The reader-retirement contract is the other watch item — a leaked reader costs ~1 s of
   worker time, not correctness, but it is exactly the kind of thing a blind port drops.
3. **`Best<F>` allocation churn.** Addressed by `consider_with`'s lazy materialization.
4. **AVFoundation `timeRange` behavior on odd containers.** Unknown until measured; a failed
   reader construction maps to `Failed` and the walk degrades to the head best — i.e. today's
   behavior, never worse.

## Phases (one commit each; Codex round at the end)

1. **`poster_walk.rs` + `rational_to_hns` + tests** — driver, `Best<F>`, `ScanOutcome`,
   `SeekError`, `FakeBackend`. Nothing wired. Pure addition, zero behavior change.
2. **MF onto the driver** — delete `deep_scan` + `Best`; implement the trait incl. the
   retirement contract. Windows cross-check + full `pb-decode` suite. **Behavior-neutral.**
3. **FFmpeg, split** (Codex P2-3, for clean bisection):
   - **3a** mechanical port + scRGB judge hoist + `ts_hns`. **Behavior-neutral.**
   - **3b** driver deadline check + interrupt classification. **Intended change.**
4. **AVFoundation, split** (#92.2):
   - **4a** the internal `_at` helper + timed-frame callback + PTS plumbing, with zero-offset
     Live-Photo regression tests. **Behavior-neutral.**
   - **4b** `av_poster` onto the driver. **The intended behavior change.**

Six commits; any regression bisects to one backend and, within FFmpeg/AV, to
mechanical-vs-semantic.

## Out of scope (deliberately)

- ❌ The #114 selection pipeline on macOS — `PosterChoice` plumbing,
  `decode_video_poster_select`, `decode_video_poster_replay`, the `PosterSelector` ledger
  (`pb-app-core/src/poster_select.rs:51`). **Follow-up plan.**
- ❌ Flipping `engine::poster_select_supported()` (`engine.rs:463`). It is a platform bool and
  macOS has *three* poster routes (AVFoundation path / FFmpeg path / Swift-shell archive
  entry, `engine.rs:730`); flipping it wholesale would route archive-entry videos to a
  `select_item` that is `Unsupported` off Windows (`engine.rs:533`). Needs a per-item
  capability check — its own change.
- ❌ The `engine.rs:615-760` routing fork (audit #4).
- ❌ Aligning FFmpeg's seek origin with MF's first-sample origin (see §Timestamps).
- ❌ `DecodeError::Cancelled` variant (#114 phase 5 optional). Independent.

## Success criteria

1. `deep_scan` and `Best` exist **exactly once** in the repo.
2. Every poster judging threshold, and the one rational→hns conversion, live in `video.rs`.
3. All three backends reach posters through `walk_poster`.
4. The walk policy has real unit tests (it has none today), including both invalid-position
   degrade sites.
5. macOS MP4/MOV feature films get a scene poster, not a black frame (#92.2 closed).
6. Windows poster behavior is unchanged (owner-smoked).

## Codex review log

- **Round 1 (2026-07-19, rev 1): no P0, 5×P1 + 3×P2 — all folded into rev 2.**
  **P1-1 (load-bearing):** `scan -> Result<bool, _>` could not express MF's *second*
  invalid-position site (`ReadSample` inside `scan`, `:658`, caught at `deep_scan:752`);
  rev 1's proposed `Ok(false)` mapping would have turned "stop the walk, keep the head best"
  into "try the next offset" — a silent #114 phase-4 regression. Fixed with `ScanOutcome`
  {Good, Exhausted, Stop}. Rev 1's claim that existing MF mock tests covered this was also
  **false** — verified: `:1174` only tests `is_invalid_position` classification, neither walk
  site. **P1-2:** deadline/cancellation semantics underspecified — MF stays neutral only if
  the deadline is still created at `:523`, `scan` keeps its per-sample check at `:648`, and
  only the between-seek check (`:720`) moves; FFmpeg's phase is **not** behavior-neutral
  (interrupt-callback-only enforcement today, `io.rs:70`) and must classify interrupt errors
  (`:224`); the driver must recheck cancel/deadline after a `Failed` seek, and `SeekError`
  gains `Cancelled`. **P1-3:** "add an offset parameter" was insufficient —
  `decode_live_motion_streaming` owns the whole reader lifecycle in one call (`:965`/`:1018`),
  so the AV backend records a pending offset and calls an internal `_at` helper; the public
  signature stays; PTS must not be bolted onto `AnimFrame` (`animation.rs:69`). **P1-4:**
  timestamp conversion rules specified (`rational_to_hns`, i128, no `pts_to_duration`
  `probe.rs:61`, no silent zero for missing FFmpeg PTS, AV `origin + target` and CMTime flag
  validation); the rev-1 trait/prose contradiction over `origin_hns` resolved (concrete
  accessor, not a trait method). **P1-5:** MF reader-retirement lifecycle made an explicit
  contract (`:730`/`:783`/`:892`, `Drop`-based). **P2-1:** `Best::consider` by value would
  force a clone per FFmpeg candidate (`:142` clones only on replacement) → `consider_with`
  lazy closure, plus a no-resize judge path (`video.rs:570`). **P2-2:** phase 4's AV behavior
  changes enumerated (media cap, fallback, stop condition, deadline, deep seeks) with a VFR
  watch item. **P2-3:** phases 3 and 4 each split into mechanical vs semantic halves, and the
  extra test cases folded into the test plan.
  Confirmed correct by the review: the duplication claim, `Best`'s shared policy, the trait
  being the right seam (MF-recreate vs FFmpeg-seek-in-place both expressible; `scan` rightly
  owning judging while the driver owns ordering), the scRGB hoist being behavior-neutral if
  moved unchanged, the #114 surface genuinely untouched, `poster_select_supported` staying
  Windows-only being coherent, AVFoundation `timeRange` + `CMSampleBufferGetPresentationTimeStamp`
  being technically feasible, and the out-of-scope boundary being sound.
