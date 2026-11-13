//! Slideshow timer state (task #23) — the pure, testable core.
//!
//! A slideshow is just a timer-driven advance: when it's on and the current slide
//! has been shown for `interval`, the engine advances one photo in the last
//! navigation direction (see `AppCore::tick`). The event-loop wiring (the deadline
//! check, the readiness gate, suppression while a nav key is held) lives in the core's
//! tick; this module owns only the state and the arithmetic, so it can be unit-tested
//! without a clock or an event loop.
//!
//! **Adjusting the interval (task #140).** Two tables are the tuning knobs.
//! [`ZONES`] sizes steps by how long the interval already is: a `[` / `]` tap moves
//! 0.1s under 10s, 0.5s up to 30s, 1s above that, and caps how big a held step may
//! get (fine below 1s, so a hold slows into the floor). [`HOLD_CURVE`] is how a held
//! key accelerates over time. Every step snaps to its own grid in the direction of
//! travel, so the values you see stay clean (5.1 → 5.5 → 6.0, never 5.6 → 6.1). The
//! math runs in whole deciseconds, so repeated steps can't drift.
//!
//! Privacy (task #2): RAM-only. The on/off flag and the interval are never written
//! to disk (a persisted *default* interval goes through the allowed config —
//! `settings.rs` / task #22 — not here).

use std::time::{Duration, Instant};

/// Floor for the interval — a *very* fast slideshow, but never 0 (that would be an
/// uncapped flood).
pub const MIN_INTERVAL: Duration = Duration::from_millis(100);
/// Ceiling for the interval — a sane upper bound for the `[` / `]` adjustment.
pub const MAX_INTERVAL: Duration = Duration::from_secs(60);
/// The default interval a fresh slideshow starts at.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(4);

/// One rung of the `[` / `]` hold-acceleration ladder: once the key has been held for
/// `after` (measured from the key going down), repeat every `every`, moving `step` each
/// time (held between the current [`Zone`]'s `tap` and `max`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stage {
    pub after: Duration,
    pub step: Duration,
    pub every: Duration,
}

const fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// **The hold-acceleration curve — the tuning knob for how a held `[` / `]` feels.**
/// Sorted by `after` with the first rung at zero, and by `step` (a test enforces all
/// three). The first repeat also waits out the shared hold delay
/// (`AppCore::initial_delay`, the same tap-vs-hold threshold nav keys use), so a quick
/// tap is always exactly one step.
///
/// A hold never starts on a rung finer than the current zone's tap: at 12s, where a tap
/// is 0.5s, it starts on the 0.5s rung (and its cadence) rather than on the 0.1s rung
/// with its steps forced up to 0.5s, which would run fast and then slow down a second
/// later. See [`hold_stage`].
///
/// Run `cargo test -p pb-app-core curve_report -- --ignored --nocapture` to print the
/// value-over-time trajectory this table produces before trying it on the keyboard.
#[rustfmt::skip] // one row per rung, so the curve reads as a table
pub const HOLD_CURVE: &[Stage] = &[
    Stage { after: ms(0),    step: ms(100),  every: ms(80)  }, // 0.1s steps
    Stage { after: ms(1000), step: ms(500),  every: ms(150) }, // then 0.5s
    Stage { after: ms(2500), step: ms(1000), every: ms(150) }, // then 1s
    Stage { after: ms(5000), step: ms(5000), every: ms(200) }, // then 5s, so 60s is reachable
];

/// A band of interval sizes, from `from` up to the next zone's `from`. Within it a tap
/// (and each menu click) moves `tap`, and a held step is never larger than `max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Zone {
    pub from: Duration,
    pub tap: Duration,
    pub max: Duration,
}

/// **Step sizes by how long the interval already is — the other tuning knob.** A tenth
/// is a big change at 0.4s and a pointless one at 40s, so a tap grows with the interval.
/// `max` keeps the low end fine (0.3s vs 0.4s is a third faster) and stops a coarse rung
/// leaping the floor (5s steps would go 5.0 → 0.1). A step never crosses a boundary: it
/// stops on it, so the next step is sized for the zone it enters, and a tap up and a tap
/// down always cancel out (9.9 → 10.0 → 10.5 → 10.0 → 9.9). Sorted ascending from zero.
#[rustfmt::skip] // one row per zone, so it reads as a table
pub const ZONES: &[Zone] = &[
    Zone { from: ms(0),      tap: ms(100),  max: ms(100)  }, // under 1s: always 0.1s
    Zone { from: ms(1000),   tap: ms(100),  max: ms(1000) }, // 1s to 10s
    Zone { from: ms(10_000), tap: ms(500),  max: ms(5000) }, // 10s to 30s
    Zone { from: ms(30_000), tap: ms(1000), max: ms(5000) }, // 30s and up
];

/// A `[` / `]` key hold in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdjustHold {
    /// When the key went down (the rung clock: [`Stage::after`] counts from here).
    pub started: Instant,
    /// When the last step landed (the repeat clock: [`Stage::every`] counts from here).
    pub last_step: Instant,
}

/// What a [`Slideshow::tick_hold`] did, for the caller to act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldTick {
    /// No `[` / `]` held and none was: nothing to do.
    Idle,
    /// A key is held but no step was due (or both keys are held and cancel out).
    Held,
    /// A key is held and the interval just stepped to this value (flash it).
    Stepped(Duration),
    /// The hold just ended: the current slide's dwell restarts now.
    Released,
}

/// The slideshow's runtime state: whether it's running, how long each slide shows,
/// and any `[` / `]` adjustment in progress. RAM-only; dropped on exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slideshow {
    pub on: bool,
    pub interval: Duration,
    /// The `[` / `]` hold in progress, if any.
    pub hold: Option<AdjustHold>,
    /// When the interval was last changed. The current slide's dwell counts from the
    /// later of this and when the slide was presented ([`Slideshow::dwell_anchor`]), so a
    /// speed change gives the slide a full interval at the new speed instead of
    /// advancing the moment the new interval is shorter than the time already shown.
    pub adjusted_at: Option<Instant>,
}

impl Default for Slideshow {
    fn default() -> Self {
        Self {
            on: false,
            interval: DEFAULT_INTERVAL,
            hold: None,
            adjusted_at: None,
        }
    }
}

impl Slideshow {
    /// Flip on/off; returns the new on-state (so the caller can pick the toast).
    pub fn toggle(&mut self) -> bool {
        self.on = !self.on;
        self.on
    }

    /// Move the interval by `steps` taps (negative shortens = faster, positive lengthens
    /// = slower), each sized by [`ZONES`] for where the interval is, clamped to
    /// `[MIN_INTERVAL, MAX_INTERVAL]`. Returns the new interval (for the `2.0s`-style
    /// toast).
    pub fn adjust(&mut self, steps: i32) -> Duration {
        let dir = steps.signum();
        for _ in 0..steps.unsigned_abs() {
            self.interval = tap_interval(self.interval, dir);
        }
        self.interval
    }

    /// A `[` / `]` key went down: start a hold and take one tap step now. `dir` is `-1`
    /// (faster) or `+1` (slower). Returns the new interval.
    pub fn press(&mut self, dir: i32, now: Instant) -> Duration {
        self.hold = Some(AdjustHold {
            started: now,
            last_step: now,
        });
        self.adjusted_at = Some(now);
        self.adjust(dir)
    }

    /// Advance a `[` / `]` hold. `held` is the net direction of the adjust keys currently
    /// down (`None` = neither is held; `Some(0)` = both, which cancel out). Repeats begin
    /// once the key has been held `initial_delay`, then follow [`HOLD_CURVE`].
    pub fn tick_hold(
        &mut self,
        held: Option<i32>,
        now: Instant,
        initial_delay: Duration,
    ) -> HoldTick {
        let Some(dir) = held else {
            if self.hold.take().is_some() {
                self.adjusted_at = Some(now);
                return HoldTick::Released;
            }
            return HoldTick::Idle;
        };
        let Some(hold) = self.hold else {
            // Held with no hold on record (a press always starts one, so this is only a
            // defensive restart): begin timing from here.
            self.hold = Some(AdjustHold {
                started: now,
                last_step: now,
            });
            return HoldTick::Held;
        };
        if dir == 0 {
            return HoldTick::Held;
        }
        let held_for = now.saturating_duration_since(hold.started);
        let stage = hold_stage(held_for, self.interval, dir);
        if held_for < initial_delay || now < hold.last_step + stage.every {
            return HoldTick::Held;
        }
        self.interval = step_interval(self.interval, dir, stage.step);
        self.hold = Some(AdjustHold {
            last_step: now,
            ..hold
        });
        HoldTick::Stepped(self.interval)
    }

    /// Whether the current slide is due to advance: on, and shown at least
    /// `interval` ago. `since_shown` is `now − dwell_anchor`, computed by the caller
    /// (kept Duration-based so this is trivially testable, no `Instant` needed).
    pub fn is_due(&self, since_shown: Duration) -> bool {
        self.on && since_shown >= self.interval
    }

    /// The instant the current slide's dwell counts from: the later of when it was
    /// presented (`last_present`) and the last interval change.
    pub fn dwell_anchor(&self, last_present: Option<Instant>) -> Option<Instant> {
        match (last_present, self.adjusted_at) {
            (Some(p), Some(a)) => Some(p.max(a)),
            (p, a) => p.or(a),
        }
    }
}

/// The [`HOLD_CURVE`] rung reached by time alone, after a key has been held `held_for`.
pub fn stage_for(held_for: Duration) -> Stage {
    HOLD_CURVE[time_rung(held_for)]
}

fn time_rung(held_for: Duration) -> usize {
    HOLD_CURVE
        .iter()
        .rposition(|s| held_for >= s.after)
        .unwrap_or(0)
}

/// The [`HOLD_CURVE`] rung a held key steps on: the later of the rung its hold time has
/// reached and the first rung at least as coarse as a tap here, so a hold at 12s starts
/// on 0.5s steps at the 0.5s rung's cadence.
pub fn hold_stage(held_for: Duration, cur: Duration, dir: i32) -> Stage {
    let tap = zone_for(cur, dir).tap;
    let by_size = HOLD_CURVE
        .iter()
        .position(|s| s.step >= tap)
        .unwrap_or(HOLD_CURVE.len() - 1);
    HOLD_CURVE[time_rung(held_for).max(by_size)]
}

/// The [`ZONES`] row a step from `cur` toward `dir` moves within. Directional so a
/// boundary pivots cleanly: going *down*, the boundary itself belongs to the zone below
/// (10.0 → 9.9); going *up*, to the zone above (10.0 → 10.5).
pub fn zone_for(cur: Duration, dir: i32) -> Zone {
    let cur = to_ds(cur);
    ZONES
        .iter()
        .rev()
        .find(|z| {
            let from = to_ds(z.from);
            if dir < 0 {
                from < cur
            } else {
                from <= cur
            }
        })
        .copied()
        .unwrap_or(ZONES[0])
}

/// One tap from `cur` in direction `dir`: a step of the current zone's `tap` size.
pub fn tap_interval(cur: Duration, dir: i32) -> Duration {
    step_interval(cur, dir, zone_for(cur, dir).tap)
}

/// One step from `cur` in direction `dir` (`-1` shorter/faster, `+1` longer/slower) of
/// `step`, held between the zone's `tap` and `max` ([`ZONES`]), snapped to the step's
/// grid in the direction of travel, stopped on any zone boundary it would cross, and
/// clamped to `[MIN_INTERVAL, MAX_INTERVAL]`.
pub fn step_interval(cur: Duration, dir: i32, step: Duration) -> Duration {
    if dir == 0 {
        return clamp_interval(from_ds(to_ds(cur)));
    }
    let zone = zone_for(cur, dir);
    let step = to_ds(step.clamp(zone.tap, zone.max)).max(1);
    let cur = to_ds(cur);
    let mut next = if dir > 0 {
        (cur / step + 1) * step
    } else {
        cur.div_ceil(step).saturating_sub(1) * step
    };
    // Stop on the first boundary in the way rather than leaping it, so the next step is
    // sized for the zone it enters. Ascending order: going up the first hit is the
    // nearest; going down the last one is.
    for z in ZONES {
        let b = to_ds(z.from);
        let crossed = if dir > 0 {
            cur < b && next > b
        } else {
            cur > b && next < b
        };
        if crossed {
            next = b;
        }
    }
    clamp_interval(from_ds(next))
}

/// Whole deciseconds, rounded to the nearest (an off-grid value like a hand-edited
/// `4.25` joins the grid on its first step).
fn to_ds(d: Duration) -> u32 {
    let ds = (d.as_millis() + 50) / 100;
    u32::try_from(ds).unwrap_or(u32::MAX)
}

fn from_ds(ds: u32) -> Duration {
    Duration::from_millis(u64::from(ds) * 100)
}

/// Clamp an interval into the allowed `[MIN_INTERVAL, MAX_INTERVAL]` range. Shared by
/// the `[` / `]` live adjust and the persisted-default clamp in `settings.rs`, so the
/// two can't drift — single source of truth for the bounds.
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

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    fn at(s: f64) -> Slideshow {
        Slideshow {
            interval: secs(s),
            ..Slideshow::default()
        }
    }

    /// The hold delay the core uses by default (Settings `hold_delay_ms`).
    const DELAY: Duration = Duration::from_millis(250);

    /// Simulate holding one adjust key from `start` for `hold` (ticking every 1 ms, then
    /// releasing), returning `(ms since press, interval)` for the press and every step.
    fn simulate(start: f64, dir: i32, hold: Duration) -> Vec<(u64, Duration)> {
        let t0 = Instant::now();
        let mut s = at(start);
        let mut out = vec![(0, s.press(dir, t0))];
        for t in 1..=hold.as_millis() as u64 {
            if let HoldTick::Stepped(d) = s.tick_hold(Some(dir), t0 + ms(t), DELAY) {
                out.push((t, d));
            }
        }
        out
    }

    #[test]
    fn default_is_off_at_four_seconds() {
        let s = Slideshow::default();
        assert!(!s.on);
        assert_eq!(s.interval, Duration::from_secs(4));
        assert_eq!(s.hold, None);
        assert_eq!(s.adjusted_at, None);
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
    fn a_tap_grows_with_the_interval() {
        let mut s = Slideshow::default(); // 4.0s
        assert_eq!(s.adjust(1), secs(4.1)); // ]
        assert_eq!(s.adjust(-1), secs(4.0)); // [
        assert_eq!(s.adjust(-3), secs(3.7));
        let mut s = at(0.5);
        assert_eq!(s.adjust(-1), secs(0.4));
        let mut s = at(25.0);
        assert_eq!(s.adjust(1), secs(25.5), "half seconds from 10s");
        let mut s = at(45.0);
        assert_eq!(s.adjust(-1), secs(44.0), "whole seconds from 30s");
    }

    #[test]
    fn a_tap_pivots_on_a_zone_boundary_by_direction() {
        let mut s = at(9.9);
        assert_eq!(s.adjust(1), secs(10.0));
        assert_eq!(s.adjust(1), secs(10.5));
        assert_eq!(s.adjust(-1), secs(10.0));
        assert_eq!(s.adjust(-1), secs(9.9));
        let mut s = at(29.5);
        assert_eq!(s.adjust(1), secs(30.0));
        assert_eq!(s.adjust(1), secs(31.0));
        assert_eq!(s.adjust(-1), secs(30.0));
        assert_eq!(s.adjust(-1), secs(29.5));
        // An off-grid value (a hand-edited default) joins its zone's grid on the first tap.
        assert_eq!(at(12.3).adjust(1), secs(12.5));
        assert_eq!(at(12.3).adjust(-1), secs(12.0));
    }

    #[test]
    fn taps_walk_the_same_ladder_both_ways() {
        // Every value a tap can reach, floor to ceiling, and the same values back down:
        // a tap up and a tap down always cancel out, wherever you are.
        let mut up = vec![MIN_INTERVAL];
        let mut s = at(MIN_INTERVAL.as_secs_f64());
        while s.interval < MAX_INTERVAL {
            up.push(s.adjust(1));
        }
        let mut down = vec![MAX_INTERVAL];
        while s.interval > MIN_INTERVAL {
            down.push(s.adjust(-1));
        }
        down.reverse();
        assert_eq!(up, down);
        for pair in up.windows(2) {
            assert_eq!(pair[1] - pair[0], zone_for(pair[0], 1).tap, "{pair:?}");
        }
    }

    #[test]
    fn adjust_clamps_to_floor_and_ceiling() {
        let mut s = Slideshow::default();
        for _ in 0..100 {
            s.adjust(-1);
        }
        assert_eq!(s.interval, MIN_INTERVAL); // never 0 or negative
        for _ in 0..1000 {
            s.adjust(1);
        }
        assert_eq!(s.interval, MAX_INTERVAL);
    }

    #[test]
    fn steps_snap_to_their_own_grid_in_the_direction_of_travel() {
        // Coming off 0.1s steps onto a 0.5s rung joins the 0.5 grid, it doesn't offset it.
        assert_eq!(step_interval(secs(5.1), 1, secs(0.5)), secs(5.5));
        assert_eq!(step_interval(secs(5.5), 1, secs(0.5)), secs(6.0));
        assert_eq!(step_interval(secs(5.1), -1, secs(0.5)), secs(5.0));
        assert_eq!(step_interval(secs(5.0), -1, secs(0.5)), secs(4.5));
        assert_eq!(step_interval(secs(4.3), 1, secs(1.0)), secs(5.0));
        assert_eq!(step_interval(secs(4.3), -1, secs(1.0)), secs(4.0));
        assert_eq!(step_interval(secs(26.0), 1, secs(5.0)), secs(30.0));
        // An off-grid value (a hand-edited default) joins the tenth grid on its first step.
        assert_eq!(
            step_interval(Duration::from_millis(4250), 1, ms(100)),
            secs(4.4)
        );
        // A step finer than the zone's tap is raised to it.
        assert_eq!(step_interval(secs(20.0), 1, ms(100)), secs(20.5));
    }

    #[test]
    fn the_low_end_stays_fine_whatever_the_rung() {
        // Below 1s every step is a tenth, even on the 1s and 5s rungs.
        assert_eq!(step_interval(secs(0.5), -1, secs(1.0)), secs(0.4));
        assert_eq!(step_interval(secs(0.9), 1, secs(5.0)), secs(1.0));
        // The boundary pivots by direction: down from 1.0 is fine, up from 1.0 is coarse.
        assert_eq!(step_interval(secs(1.0), -1, secs(0.5)), secs(0.9));
        assert_eq!(step_interval(secs(1.0), 1, secs(0.5)), secs(1.5));
        // Below 10s the 5s rung is held to 1s steps.
        assert_eq!(step_interval(secs(10.0), -1, secs(5.0)), secs(9.0));
        assert_eq!(step_interval(secs(2.0), -1, secs(5.0)), secs(1.0));
        assert_eq!(step_interval(secs(10.0), 1, secs(5.0)), secs(15.0));
    }

    #[test]
    fn a_coarse_step_lands_on_a_zone_boundary_instead_of_leaping_it() {
        assert_eq!(step_interval(secs(12.0), -1, secs(5.0)), secs(10.0));
        assert_eq!(step_interval(secs(1.5), -1, secs(1.0)), secs(1.0));
        assert_eq!(step_interval(secs(1.3), -1, secs(0.5)), secs(1.0));
        assert_eq!(step_interval(secs(33.0), -1, secs(5.0)), secs(30.0));
    }

    #[test]
    fn the_tables_are_well_formed() {
        assert_eq!(
            HOLD_CURVE[0].after,
            Duration::ZERO,
            "first rung starts at once"
        );
        for pair in HOLD_CURVE.windows(2) {
            assert!(pair[0].after < pair[1].after, "rungs sorted by `after`");
            assert!(pair[0].step <= pair[1].step, "rungs never get finer");
        }
        for s in HOLD_CURVE {
            assert!(s.step >= MIN_INTERVAL && s.every > Duration::ZERO);
        }
        assert_eq!(ZONES[0].from, Duration::ZERO, "zones start at zero");
        for pair in ZONES.windows(2) {
            assert!(pair[0].from < pair[1].from, "zones sorted ascending");
        }
        for z in ZONES {
            assert!(MIN_INTERVAL <= z.tap && z.tap <= z.max, "{z:?}");
        }
    }

    #[test]
    fn a_hold_starts_no_finer_than_a_tap() {
        let zero = Duration::ZERO;
        assert_eq!(hold_stage(zero, secs(4.0), 1), HOLD_CURVE[0]);
        assert_eq!(hold_stage(zero, secs(12.0), 1), HOLD_CURVE[1]);
        assert_eq!(hold_stage(zero, secs(45.0), -1), HOLD_CURVE[2]);
        // Going down from a boundary is the finer zone below it.
        assert_eq!(hold_stage(zero, secs(10.0), -1), HOLD_CURVE[0]);
        assert_eq!(hold_stage(zero, secs(10.0), 1), HOLD_CURVE[1]);
        // Time held still wins once it has reached a coarser rung.
        let late = HOLD_CURVE[HOLD_CURVE.len() - 1].after;
        assert_eq!(hold_stage(late, secs(12.0), 1), HOLD_CURVE[3]);
        // So the first repeat at 12s is a half second, not a tenth.
        let steps = simulate(12.0, 1, ms(500));
        assert_eq!(steps[0], (0, secs(12.5)));
        assert_eq!(steps[1], (DELAY.as_millis() as u64, secs(13.0)));
    }

    #[test]
    fn stage_for_picks_the_latest_rung_reached() {
        assert_eq!(stage_for(Duration::ZERO), HOLD_CURVE[0]);
        let last = HOLD_CURVE[HOLD_CURVE.len() - 1];
        assert_eq!(stage_for(last.after), last);
        assert_eq!(stage_for(last.after + Duration::from_secs(60)), last);
        let second = HOLD_CURVE[1];
        assert_eq!(stage_for(second.after - ms(1)), HOLD_CURVE[0]);
        assert_eq!(stage_for(second.after), second);
    }

    #[test]
    fn a_tap_is_exactly_one_step() {
        // Released inside the hold delay: the press step only, then Released.
        let t0 = Instant::now();
        let mut s = Slideshow::default();
        assert_eq!(s.press(1, t0), secs(4.1));
        for t in 1..DELAY.as_millis() as u64 {
            assert_eq!(s.tick_hold(Some(1), t0 + ms(t), DELAY), HoldTick::Held);
        }
        assert_eq!(s.tick_hold(None, t0 + DELAY, DELAY), HoldTick::Released);
        assert_eq!(s.interval, secs(4.1));
        assert_eq!(s.tick_hold(None, t0 + DELAY, DELAY), HoldTick::Idle);
    }

    #[test]
    fn holding_waits_out_the_hold_delay_before_repeating() {
        let steps = simulate(4.0, 1, ms(400));
        assert_eq!(steps[0], (0, secs(4.1)));
        assert_eq!(
            steps[1].0,
            DELAY.as_millis() as u64,
            "first repeat at the hold delay"
        );
    }

    #[test]
    fn holding_accelerates_and_every_value_sits_on_its_grid() {
        for (start, dir) in [(4.0, 1), (10.0, -1), (60.0, -1), (0.1, 1), (12.0, 1)] {
            let steps = simulate(start, dir, Duration::from_secs(15));
            for pair in steps.windows(2) {
                let (a, b) = (pair[0].1, pair[1].1);
                if a == b {
                    continue; // pinned at a limit
                }
                assert_eq!(
                    b > a,
                    dir > 0,
                    "monotonic in the travel direction from {start}"
                );
                // A coarse step (every rung above 0.1s is a multiple of 0.5s) lands on the
                // half-second grid, so a held key never shows values like 5.6, 6.1, 6.6.
                let (a_ds, b_ds) = (to_ds(a), to_ds(b));
                if a_ds.abs_diff(b_ds) >= 5 || b >= secs(10.0) {
                    assert_eq!(b_ds % 5, 0, "{a:?} -> {b:?} left the half-second grid");
                }
            }
        }
    }

    #[test]
    fn a_long_hold_reaches_either_limit_and_slows_into_the_floor() {
        let up = simulate(4.0, 1, Duration::from_secs(10));
        assert_eq!(up.last().unwrap().1, MAX_INTERVAL);
        let down = simulate(60.0, -1, Duration::from_secs(15));
        assert_eq!(down.last().unwrap().1, MIN_INTERVAL);
        // The last ten values on the way down are the fine run 1.0, 0.9, ... 0.1 (a hold
        // pinned at the floor keeps re-reporting 0.1s, which keeps its toast up).
        let mut tail: Vec<u32> = down
            .iter()
            .map(|&(_, d)| to_ds(d))
            .filter(|&ds| ds <= 10)
            .collect();
        tail.dedup();
        assert_eq!(tail, (1..=10).rev().collect::<Vec<u32>>());
    }

    #[test]
    fn both_keys_held_cancel_out_and_release_reanchors() {
        let t0 = Instant::now();
        let mut s = Slideshow::default();
        s.press(1, t0);
        let before = s.interval;
        for t in 1..2000 {
            assert_eq!(s.tick_hold(Some(0), t0 + ms(t), DELAY), HoldTick::Held);
        }
        assert_eq!(s.interval, before);
        let release = t0 + ms(2000);
        assert_eq!(s.tick_hold(None, release, DELAY), HoldTick::Released);
        assert_eq!(s.adjusted_at, Some(release));
        assert_eq!(s.hold, None);
    }

    #[test]
    fn the_dwell_counts_from_the_later_of_present_and_adjust() {
        let t0 = Instant::now();
        let mut s = Slideshow::default();
        assert_eq!(s.dwell_anchor(None), None);
        assert_eq!(s.dwell_anchor(Some(t0)), Some(t0));
        s.adjusted_at = Some(t0 + ms(500));
        assert_eq!(s.dwell_anchor(Some(t0)), Some(t0 + ms(500)));
        // A slide presented after the adjustment counts from its own present.
        assert_eq!(s.dwell_anchor(Some(t0 + ms(900))), Some(t0 + ms(900)));
        assert_eq!(s.dwell_anchor(None), Some(t0 + ms(500)));
    }

    #[test]
    fn is_due_respects_on_and_interval() {
        let mut s = at(2.0);
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

    /// Prints the trajectory [`HOLD_CURVE`] produces, for tuning by eye:
    /// `cargo test -p pb-app-core curve_report -- --ignored --nocapture`
    #[test]
    #[ignore = "tuning report, not a check"]
    fn curve_report() {
        for (start, dir, label) in [
            (4.0, 1, "hold ] from 4.0s"),
            (12.0, 1, "hold ] from 12.0s"),
            (10.0, -1, "hold [ from 10.0s"),
            (60.0, -1, "hold [ from 60.0s"),
        ] {
            println!("\n{label}");
            let steps = simulate(start, dir, Duration::from_secs(12));
            let mut last = None;
            for (t, d) in steps {
                if last == Some(d) {
                    continue;
                }
                last = Some(d);
                println!("  {:>6.2}s  {}", t as f64 / 1000.0, format_interval(d));
            }
        }
    }
}
