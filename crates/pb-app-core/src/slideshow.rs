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

/// Floor for the interval — a *very* fast slideshow, but never 0 (that would be an
/// uncapped flood). Reached via the fine `[` steps below [`FINE_BELOW`].
pub const MIN_INTERVAL: Duration = Duration::from_millis(100);
/// Ceiling for the interval — a sane upper bound for the `[` / `]` adjustment.
pub const MAX_INTERVAL: Duration = Duration::from_secs(60);
/// The default interval a fresh slideshow starts at.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(4);
/// The coarse `[` / `]` step, used at and above [`FINE_BELOW`].
pub const STEP_COARSE: Duration = Duration::from_millis(500);
/// The fine `[` / `]` step, used below [`FINE_BELOW`] so a fast slideshow can be dialed
/// all the way down to [`MIN_INTERVAL`] in 0.1s increments.
pub const STEP_FINE: Duration = Duration::from_millis(100);
/// The interval at/below which `[` / `]` switch from the coarse to the fine step.
pub const FINE_BELOW: Duration = Duration::from_millis(500);

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

    /// Step the interval one notch per `steps` (negative shortens = faster, positive
    /// lengthens = slower), clamped to `[MIN_INTERVAL, MAX_INTERVAL]`. The step is
    /// **coarse** ([`STEP_COARSE`], 0.5s) at and above [`FINE_BELOW`] and **fine**
    /// ([`STEP_FINE`], 0.1s) below it — so the run down to [`MIN_INTERVAL`] is in 0.1s
    /// increments. Each notch snaps to the 0.1s grid so repeated steps don't drift.
    /// Returns the new interval (for the `2.0s`-style toast).
    pub fn adjust(&mut self, steps: i32) -> Duration {
        let mut secs = self.interval.as_secs_f64();
        let dir = steps.signum() as f64;
        for _ in 0..steps.unsigned_abs() {
            secs = snap_tenth(secs + dir * step_secs(secs, steps));
        }
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

/// The `[` / `]` step size (seconds) for the current `secs`, in the direction implied
/// by `steps`' sign: fine below [`FINE_BELOW`], coarse at/above it. Decreasing treats the
/// boundary as the top of the fine zone (0.5 → 0.4); increasing treats it as the bottom
/// of the coarse zone (0.5 → 1.0), so the pivot at 0.5s is symmetric.
fn step_secs(secs: f64, steps: i32) -> f64 {
    let boundary = FINE_BELOW.as_secs_f64();
    let in_fine = if steps < 0 {
        secs <= boundary + 1e-9
    } else {
        secs < boundary - 1e-9
    };
    if in_fine {
        STEP_FINE.as_secs_f64()
    } else {
        STEP_COARSE.as_secs_f64()
    }
}

/// Round to the nearest 0.1s — kills the float-accumulation drift from repeated
/// `0.1`/`0.5` steps so the value stays on a clean tenth-of-a-second grid.
fn snap_tenth(secs: f64) -> f64 {
    (secs * 10.0).round() / 10.0
}

/// Clamp an interval into the allowed `[MIN_INTERVAL, MAX_INTERVAL]` range. Shared by
/// the `[` / `]` live adjust ([`Slideshow::adjust`]) and the persisted-default clamp in
/// `settings.rs`, so the two can't drift — single source of truth for the bounds.
pub fn clamp_interval(d: Duration) -> Duration {
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
        // Drive far below the floor: clamps to MIN_INTERVAL (0.1s), never 0 or negative.
        for _ in 0..30 {
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
    fn adjust_uses_fine_steps_below_half_a_second() {
        let mut s = Slideshow {
            on: false,
            interval: Duration::from_millis(500),
        };
        // At/below 0.5s, `[` steps by 0.1s down to the 0.1s floor.
        assert_eq!(s.adjust(-1), Duration::from_millis(400));
        assert_eq!(s.adjust(-1), Duration::from_millis(300));
        assert_eq!(s.adjust(-3), Duration::from_millis(100)); // 0.3→0.2→0.1, then floor
        assert_eq!(s.interval, MIN_INTERVAL);
    }

    #[test]
    fn adjust_pivots_between_coarse_and_fine_at_half_a_second() {
        let mut s = Slideshow {
            on: false,
            interval: Duration::from_millis(400),
        };
        assert_eq!(s.adjust(1), Duration::from_millis(500)); // fine up to the pivot
        assert_eq!(s.adjust(1), Duration::from_secs(1)); // coarse 0.5s jump above it

        // Coarse coming back down lands exactly on 0.5 before the fine steps resume.
        assert_eq!(s.adjust(-1), Duration::from_millis(500));
        assert_eq!(s.adjust(-1), Duration::from_millis(400));
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
