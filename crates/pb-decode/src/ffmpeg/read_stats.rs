//! A ~2 s latency window for I/O-shaped operations (task #133, plan slice 1) —
//! the pure half of the `demux stall diag` / `src read diag` lines.
//!
//! Callers fold each operation's wall time; when a window closes the fold
//! returns a [`WindowSummary`] for the caller to print (the struct never does
//! I/O itself, and "now" is injected, so tests need no sleeps). The shape
//! mirrors the sample-buffer route's Swift `sb-read diag` — same window length,
//! same slow-read buckets — so the two routes' traces read alike. The >40 ms
//! bucket ≈ a missed 24 fps frame interval: the starvation smoking gun.

use std::time::{Duration, Instant};

/// Window length: long enough to smooth scheduler noise, short enough to see a
/// stall arrive (matches the Swift `sb-read diag` and the session diag cadence).
const WINDOW: Duration = Duration::from_secs(2);

/// One closed window's facts, ready to format.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowSummary {
    /// Actual window length (≥ [`WINDOW`]; folds are the only clock ticks).
    pub secs: f64,
    pub reads: u32,
    pub avg_ms: f64,
    pub max_ms: f64,
    /// Operations over 20 ms — noticeable latency spikes.
    pub over_20: u32,
    /// Operations over 40 ms — a missed 24 fps frame interval.
    pub over_40: u32,
    /// Bytes moved this window (0 unless folded via [`ReadStats::fold_bytes`]).
    pub bytes: u64,
}

/// The accumulator. One per instrumented site; not thread-safe by design (each
/// site folds from its own thread).
pub struct ReadStats {
    win_start: Instant,
    reads: u32,
    total: Duration,
    max: Duration,
    over_20: u32,
    over_40: u32,
    bytes: u64,
}

impl ReadStats {
    pub fn new(now: Instant) -> Self {
        ReadStats {
            win_start: now,
            reads: 0,
            total: Duration::ZERO,
            max: Duration::ZERO,
            over_20: 0,
            over_40: 0,
            bytes: 0,
        }
    }

    /// Fold one operation's duration. Returns `Some(summary)` when this fold
    /// closed a ≥2 s window (the window then restarts at `now`).
    pub fn fold(&mut self, took: Duration, now: Instant) -> Option<WindowSummary> {
        self.fold_bytes(took, 0, now)
    }

    /// [`fold`](Self::fold), also accumulating the operation's byte count (the
    /// filler's throughput readout).
    pub fn fold_bytes(
        &mut self,
        took: Duration,
        bytes: u64,
        now: Instant,
    ) -> Option<WindowSummary> {
        self.bytes += bytes;
        self.reads += 1;
        self.total += took;
        if took > self.max {
            self.max = took;
        }
        if took > Duration::from_millis(20) {
            self.over_20 += 1;
        }
        if took > Duration::from_millis(40) {
            self.over_40 += 1;
        }
        let elapsed = now.saturating_duration_since(self.win_start);
        if elapsed < WINDOW {
            return None;
        }
        let summary = WindowSummary {
            secs: elapsed.as_secs_f64(),
            reads: self.reads,
            avg_ms: self.total.as_secs_f64() * 1000.0 / f64::from(self.reads),
            max_ms: self.max.as_secs_f64() * 1000.0,
            over_20: self.over_20,
            over_40: self.over_40,
            bytes: self.bytes,
        };
        *self = ReadStats::new(now);
        Some(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn no_emission_mid_window() {
        let t0 = Instant::now();
        let mut s = ReadStats::new(t0);
        for i in 0..100 {
            assert_eq!(s.fold(ms(1), t0 + ms(10 * i)), None, "fold {i} mid-window");
        }
    }

    #[test]
    fn window_emits_after_two_seconds_with_correct_facts() {
        let t0 = Instant::now();
        let mut s = ReadStats::new(t0);
        assert_eq!(s.fold(ms(10), t0 + ms(100)), None);
        assert_eq!(s.fold(ms(30), t0 + ms(200)), None);
        let out = s.fold(ms(50), t0 + ms(2100)).expect("window closed");
        assert_eq!(out.reads, 3);
        assert_eq!(out.over_20, 2, "30ms and 50ms clear 20ms");
        assert_eq!(out.over_40, 1, "only 50ms clears 40ms");
        assert!((out.avg_ms - 30.0).abs() < 1e-9, "avg of 10/30/50");
        assert!((out.max_ms - 50.0).abs() < 1e-9);
        assert!((out.secs - 2.1).abs() < 1e-9);
    }

    #[test]
    fn window_resets_after_emission() {
        let t0 = Instant::now();
        let mut s = ReadStats::new(t0);
        let _ = s.fold(ms(45), t0 + ms(2000)).expect("first window");
        // The next window starts fresh — no carryover of counts or max.
        let out = s
            .fold(ms(1), t0 + ms(4100))
            .expect("second window (2.1s after the reset at 2.0s)");
        assert_eq!(out.reads, 1);
        assert_eq!(out.over_20, 0);
        assert_eq!(out.over_40, 0);
        assert!((out.max_ms - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fold_bytes_accumulates_into_the_summary() {
        let t0 = Instant::now();
        let mut s = ReadStats::new(t0);
        assert_eq!(s.fold_bytes(ms(1), 1000, t0), None);
        assert_eq!(s.fold(ms(1), t0), None, "plain fold adds no bytes");
        let out = s.fold_bytes(ms(1), 500, t0 + ms(2000)).expect("window");
        assert_eq!(out.bytes, 1500);
    }

    #[test]
    fn boundary_buckets_are_exclusive() {
        let t0 = Instant::now();
        let mut s = ReadStats::new(t0);
        let _ = s.fold(ms(20), t0);
        let _ = s.fold(ms(40), t0);
        let out = s.fold(ms(0), t0 + ms(2000)).expect("window");
        assert_eq!(out.over_20, 1, "exactly 20ms does not count; 40ms does");
        assert_eq!(out.over_40, 0, "exactly 40ms does not count");
    }
}
