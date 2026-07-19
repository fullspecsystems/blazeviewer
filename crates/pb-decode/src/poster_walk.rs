//! The **one** poster walk policy (task #121) — shared by every backend.
//!
//! A feature film opens on black, a studio logo, or a fade for its first 30–90 s, so a
//! head-only walk hands back a black poster. The policy that fixes that — walk the head
//! cheaply, then seek past the intro shallow→deep, stopping at the first genuinely-good
//! frame — used to exist **twice**, near-verbatim, in `mf_poster.rs` and `ffmpeg/poster.rs`
//! (and not at all in `av_poster.rs`). The two copies drifted: two-scale scoring landed in
//! FFmpeg only (`cfd55ffe`), while origin capture, judge-size scoring, and the typed BDMV
//! degrade landed in MF only (#114). This module is the policy, once.
//!
//! **The split:** this driver owns *ordering and stop/continue policy* — which offsets, in
//! what order, when a deadline ends the walk, what survives a refusal. A [`PosterBackend`]
//! owns *mechanics* — opening/positioning a reader, decoding, converting, and judging. The
//! driver never sees a pixel or a reader, which is exactly why it is testable on every
//! platform (including ones with no video backend at all) — see the `FakeBackend` tests.
//!
//! Pure policy: no `unsafe`, no `cfg`, no I/O.

use std::time::{Duration, Instant};

use crate::video::{
    poster_deep_cap, POSTER_BURST_FRAMES, POSTER_DEEP_MIN, POSTER_HEAD_FRAMES, POSTER_SEEK_OFFSETS,
};
use crate::DecodeError;

/// Why a seek did not land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekError {
    /// The container **refuses positioned reads** — a raw BDMV `.m2ts` answering
    /// `MF_E_INVALID_POSITION` (0xC00D36E5). No deeper offset can ever succeed, so the walk
    /// stops and the accumulated head best becomes the poster (task #114 phase 4). Blanket-
    /// converting *every* seek failure this way would mask real corruption — only this typed
    /// class degrades.
    Refused,
    /// This offset failed; try the next one.
    Failed,
    /// The job was cancelled mid-seek (the pool retired it).
    Cancelled,
}

/// What one burst concluded.
///
/// Three outcomes, not a `bool`: a burst can end the **whole walk**, and collapsing that into
/// "found nothing" would silently turn "stop, keep the head best" into "try the next offset".
/// That distinction is load-bearing on MF, where the invalid-position refusal arrives from
/// *two* sites — the `SetCurrentPosition` seek **and** the post-seek `ReadSample` inside the
/// burst itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanOutcome {
    /// A genuinely-good frame — the walk stops here; this is the poster.
    Good,
    /// The frame limit or EOF was reached without a good frame — try the next offset.
    Exhausted,
    /// Stop the **entire** walk and keep `best`: the deadline expired, or the container
    /// refused positioned reads from inside the burst.
    Stop,
}

/// The best frame so far, generic over the backend's frame representation so MF (owned
/// RGBA) and FFmpeg (a refcounted `AVFrame`) share ONE ranking implementation.
///
/// Two-tier by design: `consider_with` *ranks* candidates for the all-bad fallback, while
/// `win` takes a genuinely-good frame outright — a good frame is the poster regardless of
/// what out-*ranked* it earlier (ranking only decides the fallback).
pub struct Best<F> {
    score: f32,
    frame: Option<F>,
    /// The retained frame's **absolute** timestamp in 100 ns units — the stream origin has
    /// NOT been subtracted. This is what makes a choice replayable (task #114); the caller
    /// assembling a `PosterChoice` does the subtraction.
    ts_hns: i64,
}

impl<F> Default for Best<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> Best<F> {
    pub fn new() -> Self {
        Self {
            score: f32::NEG_INFINITY,
            frame: None,
            ts_hns: 0,
        }
    }

    /// Offer a scored frame for the **fallback** ranking; keep it if it beats the incumbent
    /// (first max wins — deterministic, so path and in-RAM posters stay bit-identical).
    ///
    /// The frame is materialized **only if it actually replaces** the incumbent: FFmpeg's
    /// frames are refcounted and it clones per replacement today, so a by-value parameter
    /// would force a clone (and a drop) for every candidate the walk merely looks at.
    pub fn consider_with(&mut self, score: f32, ts_hns: i64, make: impl FnOnce() -> F) {
        if self.frame.is_none() || score > self.score {
            self.score = score;
            self.frame = Some(make());
            self.ts_hns = ts_hns;
        }
    }

    /// Take this frame as the winner outright — the walk found a genuinely-good poster and
    /// stops here, so it is the result regardless of earlier ranking.
    pub fn win(&mut self, frame: F, ts_hns: i64) {
        self.score = f32::INFINITY;
        self.frame = Some(frame);
        self.ts_hns = ts_hns;
    }

    /// Whether anything at all has been retained (a walk that decoded no frames yields `None`).
    pub fn is_empty(&self) -> bool {
        self.frame.is_none()
    }

    /// The retained frame and its absolute timestamp.
    pub fn take(self) -> Option<(F, i64)> {
        self.frame.map(|f| (f, self.ts_hns))
    }
}

/// One platform's poster mechanics. Implemented by Media Foundation (Windows), FFmpeg
/// (mac/Linux), and AVFoundation (macOS).
pub trait PosterBackend {
    /// The backend's retained-frame representation (owned RGBA, a refcounted `AVFrame`, …).
    type Frame;

    /// The clip's duration, if the container declares one. `None` disables the deep phase —
    /// there is nothing to compute a cap from.
    fn duration(&self) -> Option<Duration>;

    /// Whether the caller has cancelled this job.
    fn cancelled(&self) -> bool;

    /// Position for the next burst. Recreating the reader (MF, AVFoundation) and seeking in
    /// place (FFmpeg) are equally valid — the driver never learns which.
    fn seek(&mut self, target: Duration) -> Result<(), SeekError>;

    /// Decode up to `limit` frames from the current position, judging each into `best`.
    ///
    /// Judging lives here because the pixel format differs (RGBA8 vs scRGB fp16) — but every
    /// backend calls the **shared** judge in [`crate::video`], never a private threshold.
    /// The backend also keeps its own per-sample deadline check and reports expiry as
    /// [`ScanOutcome::Stop`]: the driver only checks between bursts, so without that a single
    /// burst on hostile input could overrun the watchdog.
    fn scan(
        &mut self,
        limit: usize,
        best: &mut Best<Self::Frame>,
        deadline: Instant,
    ) -> Result<ScanOutcome, DecodeError>;
}

/// The cancellation error every backend and the driver agree on. There is no
/// `DecodeError::Cancelled` variant yet; `DecodeError::is_cancelled` knows this convention
/// by the message suffix, so it must keep saying "cancelled".
fn cancelled_err() -> DecodeError {
    DecodeError::Corrupt("poster walk cancelled".into())
}

/// Run the walk: the cheap head pass, then — only if the head found nothing good — seek past
/// the intro shallow→deep.
///
/// `Ok(true)` means a genuinely-good frame was found and `best` holds it. `Ok(false)` means
/// fall back to `best`'s ranked frame, which may still be a perfectly good poster (the
/// least-black frame of a dark clip) or nothing at all if no frame decoded. **Deadline
/// exhaustion is `Ok(false)`, never an error** — a poster is a background nicety, never worth
/// failing an item over.
pub fn walk_poster<B: PosterBackend>(
    backend: &mut B,
    best: &mut Best<B::Frame>,
    deadline: Instant,
) -> Result<bool, DecodeError> {
    // Phase 1 — the cheap head walk from the start. A clip that opens on content (Live
    // Photos, home video) settles here with no seeking at all; a logo-on-black / fade-in
    // leaves `best` weak and falls through.
    match backend.scan(POSTER_HEAD_FRAMES, best, deadline)? {
        ScanOutcome::Good => return Ok(true),
        ScanOutcome::Stop => return Ok(false),
        ScanOutcome::Exhausted => {}
    }

    // Phase 2 — seek past the intro (the feature-film case), shallow → deep, stopping at the
    // first good frame so the poster is as early as the intro allows.
    let Some(dur) = backend.duration() else {
        return Ok(false); // no duration → nothing to clamp the offsets against
    };
    if dur < POSTER_DEEP_MIN {
        return Ok(false); // short clips are already covered by the head walk
    }

    let cap = poster_deep_cap(dur);
    let mut last = Duration::ZERO;
    for off in POSTER_SEEK_OFFSETS {
        let target = off.min(cap);
        // Skip offsets the head walk already covered (~1 s) and duplicates (a short clip
        // collapses the deeper offsets onto the cap).
        if target <= Duration::from_secs(1) || target <= last {
            continue;
        }
        last = target;
        if backend.cancelled() {
            return Err(cancelled_err());
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        match backend.seek(target) {
            Ok(()) => {}
            // The container refuses positioned reads: no deeper offset can succeed, so stop
            // and let the head best serve as the poster.
            Err(SeekError::Refused) => return Ok(false),
            Err(SeekError::Cancelled) => return Err(cancelled_err()),
            // This offset didn't open — try the next. Re-check cancellation and the deadline
            // FIRST: a cancel landing during the last seek would otherwise fall off the end
            // of the loop and return a poster for a job nobody wants any more.
            Err(SeekError::Failed) => {
                if backend.cancelled() {
                    return Err(cancelled_err());
                }
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                continue;
            }
        }
        match backend.scan(POSTER_BURST_FRAMES, best, deadline)? {
            ScanOutcome::Good => return Ok(true),
            ScanOutcome::Stop => return Ok(false),
            ScanOutcome::Exhausted => {}
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// One scripted burst: what `scan` should conclude, and the candidate it ranks.
    struct Burst {
        outcome: ScanOutcome,
        /// `(score, ts_hns)` offered to `Best` before returning, if any.
        candidate: Option<(f32, i64)>,
    }

    impl Burst {
        fn exhausted(score: f32, ts: i64) -> Self {
            Self {
                outcome: ScanOutcome::Exhausted,
                candidate: Some((score, ts)),
            }
        }
        fn good(ts: i64) -> Self {
            Self {
                outcome: ScanOutcome::Good,
                candidate: Some((f32::INFINITY, ts)),
            }
        }
        fn plain(outcome: ScanOutcome) -> Self {
            Self {
                outcome,
                candidate: None,
            }
        }
    }

    /// A backend with no video in it at all — the point of the seam. Every call is recorded
    /// so a test can assert not just the result but *what the driver did to get there*.
    struct FakeBackend {
        duration: Option<Duration>,
        bursts: RefCell<Vec<Burst>>,
        /// Seek results, consumed in order; missing entries default to `Ok`.
        seeks: RefCell<Vec<Result<(), SeekError>>>,
        seeked: RefCell<Vec<Duration>>,
        scans: RefCell<Vec<usize>>,
        cancel_after_seeks: Option<usize>,
        /// Frames materialized via `consider_with` — proves the closure is lazy.
        made: RefCell<usize>,
    }

    impl FakeBackend {
        fn new(duration: Option<Duration>, bursts: Vec<Burst>) -> Self {
            Self {
                duration,
                bursts: RefCell::new(bursts),
                seeks: RefCell::new(Vec::new()),
                seeked: RefCell::new(Vec::new()),
                scans: RefCell::new(Vec::new()),
                cancel_after_seeks: None,
                made: RefCell::new(0),
            }
        }
        fn with_seeks(mut self, seeks: Vec<Result<(), SeekError>>) -> Self {
            self.seeks = RefCell::new(seeks);
            self
        }
        fn cancel_after_seeks(mut self, n: usize) -> Self {
            self.cancel_after_seeks = Some(n);
            self
        }
        fn seek_targets(&self) -> Vec<Duration> {
            self.seeked.borrow().clone()
        }
    }

    impl PosterBackend for FakeBackend {
        type Frame = i64; // a frame is just its timestamp here

        fn duration(&self) -> Option<Duration> {
            self.duration
        }

        fn cancelled(&self) -> bool {
            self.cancel_after_seeks
                .is_some_and(|n| self.seeked.borrow().len() >= n)
        }

        fn seek(&mut self, target: Duration) -> Result<(), SeekError> {
            self.seeked.borrow_mut().push(target);
            let mut seeks = self.seeks.borrow_mut();
            if seeks.is_empty() {
                Ok(())
            } else {
                seeks.remove(0)
            }
        }

        fn scan(
            &mut self,
            limit: usize,
            best: &mut Best<i64>,
            _deadline: Instant,
        ) -> Result<ScanOutcome, DecodeError> {
            self.scans.borrow_mut().push(limit);
            let mut bursts = self.bursts.borrow_mut();
            if bursts.is_empty() {
                return Ok(ScanOutcome::Exhausted);
            }
            let b = bursts.remove(0);
            if let Some((score, ts)) = b.candidate {
                if b.outcome == ScanOutcome::Good {
                    best.win(ts, ts);
                } else {
                    let made = &self.made;
                    best.consider_with(score, ts, || {
                        *made.borrow_mut() += 1;
                        ts
                    });
                }
            }
            Ok(b.outcome)
        }
    }

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// A clip that opens on content settles in the head walk — and must issue **no seeks**.
    /// Seeking is the expensive part (MF recreates a reader per offset), so "the head was
    /// enough" has to mean zero I/O beyond it.
    #[test]
    fn a_good_head_frame_ends_the_walk_with_no_seeks() {
        let mut b = FakeBackend::new(
            Some(Duration::from_secs(7200)),
            vec![Burst::good(12_000_000)],
        );
        let mut best = Best::new();
        assert!(walk_poster(&mut b, &mut best, far_deadline()).unwrap());
        assert!(
            b.seek_targets().is_empty(),
            "a good head frame must not seek"
        );
        assert_eq!(best.take(), Some((12_000_000, 12_000_000)));
    }

    /// The head walk asks for `POSTER_HEAD_FRAMES`; each deep burst asks for
    /// `POSTER_BURST_FRAMES` (the deep pass samples, it does not re-walk).
    #[test]
    fn the_head_and_deep_phases_use_their_own_frame_limits() {
        let mut b = FakeBackend::new(
            Some(Duration::from_secs(7200)),
            vec![Burst::exhausted(0.1, 0), Burst::good(80_000_000)],
        );
        let mut best = Best::new();
        assert!(walk_poster(&mut b, &mut best, far_deadline()).unwrap());
        assert_eq!(
            *b.scans.borrow(),
            vec![POSTER_HEAD_FRAMES, POSTER_BURST_FRAMES]
        );
    }

    /// A feature film: the head falls through, then offsets are tried shallow→deep in order.
    /// The 8 s offset comes first — anchored to how long real intros run, NOT to a fraction
    /// of the clip, so a 2-hour film is sampled at ~8 s rather than ~90 s.
    #[test]
    fn deep_offsets_are_tried_shallow_to_deep_in_order() {
        let bursts = (0..7).map(|i| Burst::exhausted(0.1, i)).collect();
        let mut b = FakeBackend::new(Some(Duration::from_secs(7200)), bursts);
        let mut best = Best::new();
        assert!(!walk_poster(&mut b, &mut best, far_deadline()).unwrap());
        assert_eq!(
            b.seek_targets(),
            vec![
                Duration::from_secs(8),
                Duration::from_secs(20),
                Duration::from_secs(45),
                Duration::from_secs(90),
                Duration::from_secs(150),
                Duration::from_secs(240),
            ]
        );
    }

    /// The cap (min(half the clip, 5 min)) collapses the deeper offsets onto one another; the
    /// walk must sample each distinct point once, not seek to 30 s four times.
    #[test]
    fn offsets_are_clamped_to_the_cap_and_duplicates_collapse() {
        // 60 s clip → cap 30 s. Offsets 8/20/45/90/150/240 → 8, 20, 30, then all duplicates.
        let bursts = (0..5).map(|i| Burst::exhausted(0.1, i)).collect();
        let mut b = FakeBackend::new(Some(Duration::from_secs(60)), bursts);
        let mut best = Best::new();
        assert!(!walk_poster(&mut b, &mut best, far_deadline()).unwrap());
        assert_eq!(
            b.seek_targets(),
            vec![
                Duration::from_secs(8),
                Duration::from_secs(20),
                Duration::from_secs(30),
            ],
            "the cap collapses 45/90/150/240 onto 30 s — sample it once"
        );
    }

    /// An offset at or under ~1 s is already covered by the head walk.
    #[test]
    fn offsets_the_head_walk_already_covered_are_skipped() {
        // 12 s clip (>= POSTER_DEEP_MIN) → cap 6 s. The 8 s offset clamps to 6 s; deeper
        // offsets collapse onto it.
        let mut b = FakeBackend::new(
            Some(Duration::from_secs(12)),
            vec![Burst::exhausted(0.1, 0), Burst::exhausted(0.2, 1)],
        );
        let mut best = Best::new();
        assert!(!walk_poster(&mut b, &mut best, far_deadline()).unwrap());
        assert_eq!(b.seek_targets(), vec![Duration::from_secs(6)]);
    }

    /// No declared duration → nothing to clamp against, so no deep phase at all.
    #[test]
    fn a_clip_with_no_duration_never_seeks() {
        let mut b = FakeBackend::new(None, vec![Burst::exhausted(0.1, 0)]);
        let mut best = Best::new();
        assert!(!walk_poster(&mut b, &mut best, far_deadline()).unwrap());
        assert!(b.seek_targets().is_empty());
    }

    /// Short clips are covered by the head walk; seeking them is wasted I/O.
    #[test]
    fn a_clip_shorter_than_the_deep_minimum_never_seeks() {
        let mut b = FakeBackend::new(
            Some(POSTER_DEEP_MIN - Duration::from_millis(1)),
            vec![Burst::exhausted(0.1, 0)],
        );
        let mut best = Best::new();
        assert!(!walk_poster(&mut b, &mut best, far_deadline()).unwrap());
        assert!(b.seek_targets().is_empty());
    }

    /// **The BDMV contract, from the SEEK site** (task #114 phase 4): a raw `.m2ts` refusing
    /// positioned reads stops the walk and keeps the head best — it must never discard the
    /// work the head walk already did, and never try deeper offsets that cannot succeed.
    #[test]
    fn a_refused_seek_stops_the_walk_and_keeps_the_head_best() {
        let mut b = FakeBackend::new(
            Some(Duration::from_secs(7200)),
            vec![Burst::exhausted(0.5, 4_200_000)],
        )
        .with_seeks(vec![Err(SeekError::Refused)]);
        let mut best = Best::new();
        assert!(!walk_poster(&mut b, &mut best, far_deadline()).unwrap());
        assert_eq!(
            b.seek_targets().len(),
            1,
            "a refusal must not try deeper offsets"
        );
        assert_eq!(
            best.take(),
            Some((4_200_000, 4_200_000)),
            "the head best survives the refusal"
        );
    }

    /// **The BDMV contract, from the SCAN site.** On MF the same typed refusal also arrives
    /// from the post-seek `ReadSample` *inside* the burst. If that collapsed into "this burst
    /// found nothing", the walk would march on through every deeper offset — which is exactly
    /// the regression a `bool` return would have shipped.
    #[test]
    fn a_stop_from_inside_a_burst_ends_the_walk_and_keeps_the_best() {
        let mut b = FakeBackend::new(
            Some(Duration::from_secs(7200)),
            vec![
                Burst::exhausted(0.5, 4_200_000),
                Burst::plain(ScanOutcome::Stop),
            ],
        );
        let mut best = Best::new();
        assert!(!walk_poster(&mut b, &mut best, far_deadline()).unwrap());
        assert_eq!(
            b.seek_targets().len(),
            1,
            "a Stop inside the first deep burst must not try deeper offsets"
        );
        assert_eq!(best.take(), Some((4_200_000, 4_200_000)));
    }

    /// A plain failed seek is not a refusal — that offset just didn't open, so try the next.
    #[test]
    fn a_failed_seek_moves_on_to_the_next_offset() {
        let bursts = (0..7).map(|i| Burst::exhausted(0.1, i)).collect();
        let mut b = FakeBackend::new(Some(Duration::from_secs(7200)), bursts)
            .with_seeks(vec![Err(SeekError::Failed), Ok(())]);
        let mut best = Best::new();
        assert!(!walk_poster(&mut b, &mut best, far_deadline()).unwrap());
        assert!(
            b.seek_targets().len() > 1,
            "a plain failure must keep descending"
        );
    }

    /// Cancellation surfaced *by the seek itself*.
    #[test]
    fn a_cancelled_seek_aborts_the_walk() {
        let mut b = FakeBackend::new(
            Some(Duration::from_secs(7200)),
            vec![Burst::exhausted(0.1, 0)],
        )
        .with_seeks(vec![Err(SeekError::Cancelled)]);
        let mut best = Best::new();
        let e = walk_poster(&mut b, &mut best, far_deadline()).unwrap_err();
        assert!(
            e.is_cancelled(),
            "must read as a cancellation, not a failure"
        );
    }

    /// A cancel landing during the **LAST** offset's failed seek. Without the re-check after
    /// a `Failed` seek the loop simply ends and hands back a poster for a job the pool has
    /// already retired.
    ///
    /// The fixture cancels only after the *final* seek (a 2-hour clip yields exactly 6
    /// offsets) — cancelling earlier proves nothing, because the next iteration's
    /// top-of-loop check would catch it regardless of the re-check. Verified by mutation:
    /// deleting the re-check turns this red.
    #[test]
    fn cancellation_during_the_final_failed_seek_is_not_swallowed() {
        let bursts = (0..7).map(|i| Burst::exhausted(0.1, i)).collect();
        let mut b = FakeBackend::new(Some(Duration::from_secs(7200)), bursts)
            .with_seeks(vec![Err(SeekError::Failed); 6])
            .cancel_after_seeks(6);
        let mut best = Best::new();
        let e = walk_poster(&mut b, &mut best, far_deadline()).unwrap_err();
        assert!(
            e.is_cancelled(),
            "a cancel on the last seek must not be swallowed"
        );
    }

    /// An expired deadline ends the walk with whatever it has — a poster is a background
    /// nicety, so exhaustion is `Ok(false)`, never an error that would blank the tile.
    #[test]
    fn an_expired_deadline_returns_the_best_so_far_and_is_not_an_error() {
        let mut b = FakeBackend::new(
            Some(Duration::from_secs(7200)),
            vec![Burst::exhausted(0.5, 999)],
        );
        let mut best = Best::new();
        let past = Instant::now() - Duration::from_secs(1);
        let good = walk_poster(&mut b, &mut best, past).expect("deadline is never an error");
        assert!(!good);
        assert!(b.seek_targets().is_empty(), "no seek past the deadline");
        assert_eq!(best.take(), Some((999, 999)));
    }

    /// The backend's own per-sample deadline check surfaces as `Stop` — the walk ends there
    /// rather than descending, and still returns what it has.
    #[test]
    fn a_deadline_reported_from_inside_a_burst_ends_the_walk() {
        let mut b = FakeBackend::new(
            Some(Duration::from_secs(7200)),
            vec![Burst::plain(ScanOutcome::Stop)],
        );
        let mut best = Best::new();
        assert!(!walk_poster(&mut b, &mut best, far_deadline()).unwrap());
        assert!(b.seek_targets().is_empty());
        assert!(best.is_empty());
    }

    // ---- Best -------------------------------------------------------------------------

    /// First max wins: an equal score does NOT displace the incumbent, so the same clip
    /// always yields the same poster (path and in-RAM decodes stay bit-identical).
    #[test]
    fn best_keeps_the_first_of_equally_ranked_candidates() {
        let mut best: Best<i64> = Best::new();
        best.consider_with(0.5, 100, || 100);
        best.consider_with(0.5, 200, || 200);
        assert_eq!(best.take(), Some((100, 100)));
    }

    #[test]
    fn best_replaces_on_a_strictly_higher_score() {
        let mut best: Best<i64> = Best::new();
        best.consider_with(0.2, 100, || 100);
        best.consider_with(0.9, 200, || 200);
        assert_eq!(best.take(), Some((200, 200)));
    }

    /// A genuinely-good frame is the winner outright, even though an earlier candidate
    /// out-*ranked* it — ranking only ever decides the all-bad fallback.
    #[test]
    fn a_won_frame_beats_a_higher_ranked_earlier_candidate() {
        let mut best: Best<i64> = Best::new();
        best.consider_with(f32::MAX, 100, || 100);
        best.win(200, 200);
        assert_eq!(best.take(), Some((200, 200)));
        // …and nothing later can displace a winner by ranking.
        let mut best2: Best<i64> = Best::new();
        best2.win(200, 200);
        best2.consider_with(f32::MAX, 300, || 300);
        assert_eq!(best2.take(), Some((200, 200)));
    }

    /// The timestamp rides the RETAINED frame, not the last one looked at — a locator that
    /// pointed at a frame we didn't keep would replay the wrong pixels.
    #[test]
    fn the_timestamp_rides_the_retained_frame() {
        let mut best: Best<i64> = Best::new();
        best.consider_with(0.9, 111, || 111);
        best.consider_with(0.1, 222, || 222); // rejected
        assert_eq!(best.take(), Some((111, 111)));
    }

    /// `consider_with` materializes a frame only on replacement — the whole reason it takes a
    /// closure. FFmpeg clones a refcounted `AVFrame` in there; paying that per *candidate*
    /// rather than per *replacement* would be a per-frame allocation on the walk.
    #[test]
    fn consider_with_only_materializes_the_frame_it_keeps() {
        let mut b = FakeBackend::new(Some(Duration::from_secs(5)), vec![Burst::exhausted(0.9, 1)]);
        let mut best = Best::new();
        let _ = walk_poster(&mut b, &mut best, far_deadline()).unwrap();
        assert_eq!(*b.made.borrow(), 1, "one candidate, one materialization");

        // A rejected candidate must not materialize at all.
        let mut best2: Best<i64> = Best::new();
        let mut made = 0;
        best2.consider_with(0.9, 1, || 1);
        best2.consider_with(0.1, 2, || {
            made += 1;
            2
        });
        assert_eq!(made, 0, "a rejected candidate is never materialized");
    }

    #[test]
    fn an_empty_best_yields_nothing() {
        let best: Best<i64> = Best::new();
        assert!(best.is_empty());
        assert_eq!(best.take(), None);
    }
}
