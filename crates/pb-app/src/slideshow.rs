//! Slideshow timer state (task #23) — the pure, testable core.
//!
//! A slideshow is just a timer-driven advance: when it's on and the current slide
//! has been shown for `interval`, the engine advances one photo in the last
//! navigation direction (see `App::about_to_wait` / `App::advance`). All the
//! event-loop wiring (the deadline check against `last_present`, the readiness gate,
//! suppression while a nav key is held) lives in `main.rs`; this module owns only
//! the state and the arithmetic, so it can be unit-tested without winit.
//!
//! Privacy (task #2): RAM-only. The on/off flag and the interval are never written
//! to disk (a persisted *default* interval would go through the allowed config —
//! `settings.rs` / task #22 — not here).

use std::time::Duration;

/// Floor for the interval — never 0 (that would be an uncapped flood, not a
/// slideshow).
pub const MIN_INTERVAL: Duration = Duration::from_millis(500);
/// Ceiling for the interval — a sane upper bound for the `[` / `]` adjustment.
pub const MAX_INTERVAL: Duration = Duration::from_secs(60);
/// The default interval a fresh slideshow starts at.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(4);
/// How much one `[` / `]` press changes the interval.
pub const STEP: Duration = Duration::from_millis(500);

/// The slideshow's runtime state: whether it's running and how long each slide
/// shows. RAM-only; dropped on exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slideshow {
    pub on: bool,
    pub interval: Duration,
}

impl Default for Slideshow {
    fn default() -> Self {
        Self {
            on: false,
            interval: DEFAULT_INTERVAL,
        }
    }
}

impl Slideshow {
    /// Flip on/off; returns the new on-state (so the caller can pick the toast).
    pub fn toggle(&mut self) -> bool {
        self.on = !self.on;
        self.on
    }

    /// Step the interval by `steps` × [`STEP`] (negative shortens, positive
    /// lengthens), clamped to `[MIN_INTERVAL, MAX_INTERVAL]`. Returns the new
    /// interval (for the `2.0s`-style toast).
    pub fn adjust(&mut self, steps: i32) -> Duration {
        let secs = self.interval.as_secs_f64() + steps as f64 * STEP.as_secs_f64();
        // `max(0.0)` guards `from_secs_f64` against a negative (it panics on < 0);
        // the real floor is then applied by `clamp_interval`.
        self.interval = clamp_interval(Duration::from_secs_f64(secs.max(0.0)));
        self.interval
    }

    /// Whether the current slide is due to advance: on, and shown at least
    /// `interval` ago. `since_shown` is `now − last_present`, computed by the caller
    /// (kept Duration-based so this is trivially testable, no `Instant` needed).
    pub fn is_due(&self, since_shown: Duration) -> bool {
        self.on && since_shown >= self.interval
    }
}

/// Clamp an interval into the allowed `[MIN_INTERVAL, MAX_INTERVAL]` range.
fn clamp_interval(d: Duration) -> Duration {
    d.clamp(MIN_INTERVAL, MAX_INTERVAL)
}

/// A human, one-decimal-second label for the interval toast, e.g. `"2.0s"`.
pub fn format_interval(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_off_at_four_seconds() {
        let s = Slideshow::default();
        assert!(!s.on);
        assert_eq!(s.interval, Duration::from_secs(4));
    }

    #[test]
    fn toggle_flips_and_reports() {
        let mut s = Slideshow::default();
        assert!(s.toggle()); // off -> on
        assert!(s.on);
        assert!(!s.toggle()); // on -> off
        assert!(!s.on);
    }

    #[test]
    fn adjust_steps_by_half_a_second() {
        let mut s = Slideshow::default(); // 4.0s
        assert_eq!(s.adjust(1), Duration::from_millis(4500)); // ]
        assert_eq!(s.adjust(-1), Duration::from_secs(4)); // [
        assert_eq!(s.adjust(-2), Duration::from_secs(3));
    }

    #[test]
    fn adjust_clamps_to_floor_and_ceiling() {
        let mut s = Slideshow::default();
        // Drive far below the floor: clamps to 0.5s, never 0 or negative.
        for _ in 0..20 {
            s.adjust(-1);
        }
        assert_eq!(s.interval, MIN_INTERVAL);
        // Drive far above the ceiling: clamps to 60s.
        for _ in 0..200 {
            s.adjust(1);
        }
        assert_eq!(s.interval, MAX_INTERVAL);
    }

    #[test]
    fn is_due_respects_on_and_interval() {
        let mut s = Slideshow {
            on: false,
            interval: Duration::from_secs(2),
        };
        // Off is never due, however long ago the slide was shown.
        assert!(!s.is_due(Duration::from_secs(10)));
        s.on = true;
        assert!(!s.is_due(Duration::from_millis(1999))); // not yet
        assert!(s.is_due(Duration::from_secs(2))); // exactly due
        assert!(s.is_due(Duration::from_secs(5))); // overdue
    }

    #[test]
    fn format_interval_is_one_decimal() {
        assert_eq!(format_interval(Duration::from_secs(2)), "2.0s");
        assert_eq!(format_interval(Duration::from_millis(500)), "0.5s");
        assert_eq!(format_interval(Duration::from_millis(4500)), "4.5s");
    }
}
