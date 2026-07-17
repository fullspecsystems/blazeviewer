//! One-shot ("episodic") latency timers for the operations that decide how fast
//! Blaze Viewer *feels* — distinct from [`metrics::StageTimes`](crate::metrics::StageTimes),
//! which times the repeating per-frame stages (decode / upload / render). These time whole
//! user-visible operations, the ones the prime directive is really about:
//!
//! 1. **open → first photo on screen** — the single most important number when you open a
//!    folder or an archive; it's what "feeling fast" means on launch.
//! 2. **open → every photo cached at full resolution** — the prefetch finishing. Speeding up
//!    (1) must not blow this up (a first-image bias that starves the rest is a regression).
//! 3. **a Fit↔1:1 / resize switch → the re-fit photo back on screen** — the "why is this
//!    slow, it should be instant" case that started this investigation.
//!
//! **Gated by `PB_PERF`** (live to stderr), so the numbers show up while you use the app —
//! including on macOS, where you read them by launching the executable directly with stderr
//! captured. Also folded into the `--metrics` summary (winit) via the caller. **Zero cost
//! when off:** every method is one bool check and returns.
//!
//! **Privacy (#2):** durations only, in RAM, never a path or a pixel; nothing is persisted.
//!
//! Deliberately pure — every method takes `now` explicitly rather than reading the clock —
//! so the episode logic is unit-testable without sleeping.

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Which episode a `presented` call completed, for the caller to label the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Episode {
    /// Open → the first photo appeared (metric 1).
    FirstPhoto,
    /// A Fit↔1:1 or window resize → the re-decoded photo is back on screen (metric 3).
    Resize,
}

impl Episode {
    /// A short, stable label for the stderr line and the `--metrics` stage name.
    pub fn label(self) -> &'static str {
        match self {
            Episode::FirstPhoto => "open->first-photo",
            Episode::Resize => "resize->on-screen",
        }
    }
}

/// Episodic timers for the three operations above. One instance lives on `AppCore`.
pub struct Perf {
    enabled: bool,
    /// When the current open began (`open_begin`) — the start for metrics 1 and 2.
    open_started: Option<Instant>,
    /// True from an open until its first photo is presented — so metric 1 fires once.
    first_pending: bool,
    /// The deck size the open resolved to (0 until `deck_ready`) — the target for metric 2.
    total: usize,
    /// Distinct items that have reached full residency this open (metric 2). A `Set`, not a
    /// counter, because an item can be re-decoded (preview→full, a revisit) more than once.
    full_seen: HashSet<usize>,
    /// Latches metric 2 so "all cached" is reported exactly once.
    all_done: bool,
    /// When a resize / scale-mode change kicked off a re-decode of the current photo.
    resize_started: Option<Instant>,
}

impl Perf {
    /// A recorder gated by `enabled` (wire it to `PB_PERF`). When false every method is a
    /// no-op that returns `None`.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            open_started: None,
            first_pending: false,
            total: 0,
            full_seen: HashSet::new(),
            all_done: false,
            resize_started: None,
        }
    }

    /// Whether timing is on (lets the caller skip building a log line when it isn't).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// A fresh open began: reset every tracker and start the clock. Called the instant the
    /// user asks to open something, *before* the (possibly slow, possibly networked) archive
    /// or scan worker runs — so metric 1 includes that wait, which is exactly what the user
    /// feels.
    pub fn open_begin(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        self.open_started = Some(now);
        self.first_pending = true;
        self.total = 0;
        self.full_seen.clear();
        self.all_done = false;
        self.resize_started = None;
    }

    /// The deck is installed and its item count is known — the target for "all cached".
    /// Larger than the resident-ring capacity means metric 2 will (correctly) never fire:
    /// a deck that can't all be held was never all cached.
    pub fn deck_ready(&mut self, total: usize) {
        if !self.enabled {
            return;
        }
        self.total = total;
    }

    /// The current photo was presented at the current geometry. Returns the episode that
    /// just completed (first-photo after an open, or a resize) with its elapsed time, or
    /// `None`. First-photo wins when both are pending — an open supersedes a resize.
    pub fn presented(&mut self, now: Instant) -> Option<(Episode, Duration)> {
        if !self.enabled {
            return None;
        }
        if self.first_pending {
            self.first_pending = false;
            if let Some(t) = self.open_started {
                return Some((Episode::FirstPhoto, now.saturating_duration_since(t)));
            }
        }
        if let Some(t) = self.resize_started.take() {
            return Some((Episode::Resize, now.saturating_duration_since(t)));
        }
        None
    }

    /// An item reached full residency. Returns `(count, elapsed)` exactly once — when the
    /// last of `total` lands — for metric 2. Cheap on the hot path: one `Set` insert.
    pub fn full_resident(&mut self, item: usize, now: Instant) -> Option<(usize, Duration)> {
        if !self.enabled || self.all_done || self.total == 0 {
            return None;
        }
        self.full_seen.insert(item);
        if self.full_seen.len() >= self.total {
            self.all_done = true;
            return self
                .open_started
                .map(|t| (self.total, now.saturating_duration_since(t)));
        }
        None
    }

    /// A geometry change (Fit↔1:1, or a settled window resize) began re-decoding the current
    /// photo — the start for metric 3. The matching `presented` reports the elapsed time.
    pub fn resize_begin(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        self.resize_started = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn disabled_never_reports() {
        let base = Instant::now();
        let mut p = Perf::new(false);
        p.open_begin(base);
        p.deck_ready(3);
        assert_eq!(p.presented(at(base, 100)), None);
        assert_eq!(p.full_resident(0, at(base, 100)), None);
        p.resize_begin(base);
        assert_eq!(p.presented(at(base, 100)), None);
    }

    #[test]
    fn first_photo_fires_once_and_measures_from_open() {
        let base = Instant::now();
        let mut p = Perf::new(true);
        p.open_begin(base);
        let (ep, d) = p.presented(at(base, 1200)).expect("first photo");
        assert_eq!(ep, Episode::FirstPhoto);
        assert_eq!(d, Duration::from_millis(1200));
        // Only once — a later present of the same deck isn't "first photo".
        assert_eq!(p.presented(at(base, 1300)), None);
    }

    #[test]
    fn all_cached_fires_once_when_the_last_full_lands() {
        let base = Instant::now();
        let mut p = Perf::new(true);
        p.open_begin(base);
        p.deck_ready(3);
        assert_eq!(p.full_resident(0, at(base, 500)), None);
        assert_eq!(p.full_resident(1, at(base, 900)), None);
        // A duplicate doesn't advance the count.
        assert_eq!(p.full_resident(0, at(base, 1000)), None);
        let (n, d) = p.full_resident(2, at(base, 1500)).expect("all cached");
        assert_eq!(n, 3);
        assert_eq!(d, Duration::from_millis(1500));
        // Latched — no second report.
        assert_eq!(p.full_resident(2, at(base, 1600)), None);
    }

    #[test]
    fn a_deck_larger_than_what_gets_cached_never_reports_all() {
        let base = Instant::now();
        let mut p = Perf::new(true);
        p.open_begin(base);
        p.deck_ready(100); // only a windowful ever reach full
        for i in 0..10 {
            assert_eq!(p.full_resident(i, at(base, 100 * i as u64)), None);
        }
    }

    #[test]
    fn resize_measures_from_its_own_start_not_the_open() {
        let base = Instant::now();
        let mut p = Perf::new(true);
        p.open_begin(base);
        p.presented(at(base, 300)).expect("first photo consumes the open");
        // A resize later begins its own clock.
        p.resize_begin(at(base, 5000));
        let (ep, d) = p.presented(at(base, 7100)).expect("resize");
        assert_eq!(ep, Episode::Resize);
        assert_eq!(d, Duration::from_millis(2100));
    }

    #[test]
    fn open_supersedes_a_pending_resize() {
        let base = Instant::now();
        let mut p = Perf::new(true);
        p.resize_begin(base); // a resize was mid-flight…
        p.open_begin(at(base, 10)); // …when a new open arrived — it clears the resize.
        let (ep, _) = p.presented(at(base, 500)).expect("first photo");
        assert_eq!(ep, Episode::FirstPhoto);
        // The stale resize was dropped by open_begin, so no second episode fires.
        assert_eq!(p.presented(at(base, 600)), None);
    }
}
