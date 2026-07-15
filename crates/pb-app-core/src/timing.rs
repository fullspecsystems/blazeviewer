//! Shell-neutral pacing math for the self-paced advance (NS0, ADR-021).
//!
//! The hold-to-blaze nav pacing and the animation frame-step scrubbing both live on the
//! shell's per-frame loop, but the *decisions* they make are pure time arithmetic — no
//! winit, no GPU. Lifting them here keeps the pacing testable without the event loop and
//! shared by every shell (the plan's "move timing into AppCore" bullet).
//!
//! Two pieces:
//! - [`elapsed_since`] — the "has this deadline passed?" gate, used for both the initial
//!   tap delay and the repeat interval on both the nav and frame-step paths.
//! - [`advance_interval`] — the accelerating hold-to-blaze gap (ramps slow → fast).

use std::time::{Duration, Instant};

/// Has `now` reached `anchor + delay`? A `None` anchor means "not started yet" and fires
/// immediately (`true`) — so the first press/tick is never gated, only subsequent repeats
/// wait out the interval. This is the single gate behind both the initial tap-delay and
/// the repeat-interval checks on the nav hold-to-blaze and frame-step scrubbing paths.
pub fn elapsed_since(anchor: Option<Instant>, now: Instant, delay: Duration) -> bool {
    match anchor {
        Some(t) => now >= t + delay,
        None => true,
    }
}

/// The minimum gap since the last shown photo before the next held-key auto-advance,
/// given how long auto-repeat has been running (`elapsed`, measured from when the
/// initial tap delay expired). The advance rate ramps linearly from `min_rate`
/// (photos/sec) up to the **ceiling** over `ramp_secs`, then holds there. The ceiling
/// is the configured `max_rate` cap (#20) clamped to the display refresh (`max_rate`
/// ≤ 0, or ≥ refresh, means "uncapped" → refresh is the hard limit). The returned
/// interval is the rate's reciprocal, floored at the ceiling's interval so it's never
/// faster. Pure + time-based (frame-rate independent), so it's unit-testable without
/// the event loop.
pub fn advance_interval(
    elapsed: Duration,
    min_rate: f32,
    ramp_secs: f32,
    max_rate: f32,
    frame_interval: Duration,
) -> Duration {
    let frame_secs = frame_interval.as_secs_f32();
    let refresh_rate = 1.0 / frame_secs.max(f32::MIN_POSITIVE);
    // Effective ceiling: the configured cap, never above refresh; 0/negative or a cap
    // at/above refresh means uncapped (refresh is the hard limit).
    let ceiling = if max_rate > 0.0 {
        max_rate.min(refresh_rate)
    } else {
        refresh_rate
    };
    // Interval at the ceiling: exactly the refresh frame when uncapped (no float
    // drift), else the cap's reciprocal (a deliberately slower scan limit).
    let ceil_interval = if ceiling >= refresh_rate {
        frame_interval
    } else {
        Duration::from_secs_f32(1.0 / ceiling)
    };
    let min_rate = min_rate.min(ceiling);
    // No headroom to ramp (floor already at/above the ceiling): run at the ceiling.
    if min_rate >= ceiling {
        return ceil_interval;
    }
    let t = if ramp_secs > 0.0 {
        (elapsed.as_secs_f32() / ramp_secs).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let rate = min_rate + (ceiling - min_rate) * t;
    if rate >= ceiling {
        return ceil_interval;
    }
    let min_secs = ceil_interval.as_secs_f32(); // fastest (smallest) interval allowed
    let secs = (1.0 / rate.max(f32::MIN_POSITIVE)).clamp(min_secs, 60.0);
    Duration::from_secs_f32(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fixed clock origin for the gate tests (Instant can't be constructed directly;
    // derive everything from one `now`).
    fn clock() -> Instant {
        Instant::now()
    }

    #[test]
    fn elapsed_since_fires_immediately_when_not_started() {
        // `None` anchor → the first press/tick is never gated.
        assert!(elapsed_since(None, clock(), Duration::from_secs(1)));
    }

    #[test]
    fn elapsed_since_gates_until_the_deadline() {
        let now = clock();
        let anchor = Some(now);
        // Zero delay: the deadline is `now`, so `now >= now` fires.
        assert!(elapsed_since(anchor, now, Duration::ZERO));
        // A future deadline (anchor now, delay 10ms, evaluated at `now`) has not passed.
        assert!(!elapsed_since(anchor, now, Duration::from_millis(10)));
        // Evaluated 10ms later, it has.
        assert!(elapsed_since(
            anchor,
            now + Duration::from_millis(10),
            Duration::from_millis(10)
        ));
    }

    #[test]
    fn advance_interval_ramps_from_floor_to_refresh() {
        let frame = Duration::from_micros(8_333); // ~120 Hz
        let (min_rate, ramp) = (3.0, 4.0);
        // At the start of auto-repeat the gap is ~1/min_rate (a few photos/sec).
        let start = advance_interval(Duration::ZERO, min_rate, ramp, 0.0, frame);
        let expected = 1.0 / min_rate;
        assert!(
            (start.as_secs_f32() - expected).abs() < 1e-3,
            "start interval {start:?} should be ~1/min_rate ({expected}s)"
        );
        // Once the ramp completes it clamps exactly to the refresh interval, and
        // stays there past the ramp (refresh is the hard ceiling).
        assert_eq!(
            advance_interval(Duration::from_secs_f32(ramp), min_rate, ramp, 0.0, frame),
            frame
        );
        assert_eq!(
            advance_interval(Duration::from_secs(10), min_rate, ramp, 0.0, frame),
            frame
        );
    }

    #[test]
    fn advance_interval_caps_at_max_rate() {
        let frame = Duration::from_micros(8_333); // ~120 Hz
        let cap = 10.0; // photos/sec — well below refresh
        let cap_interval = Duration::from_secs_f32(1.0 / cap);
        // Once ramped, the gap holds at the cap's interval, never the refresh frame.
        let ramped = advance_interval(Duration::from_secs(10), 3.0, 4.0, cap, frame);
        assert!(
            (ramped.as_secs_f32() - cap_interval.as_secs_f32()).abs() < 1e-4,
            "should top out at the {cap}/s cap, got {ramped:?}"
        );
        assert!(ramped > frame, "the cap is slower than the refresh ceiling");
        // A cap at/above the refresh rate behaves as uncapped (refresh is the limit).
        assert_eq!(
            advance_interval(Duration::from_secs(10), 3.0, 4.0, 1000.0, frame),
            frame
        );
    }

    #[test]
    fn advance_interval_is_monotonic_and_never_below_refresh() {
        let frame = Duration::from_micros(8_333);
        let mut prev = advance_interval(Duration::ZERO, 3.0, 4.0, 0.0, frame);
        for ms in [200u64, 500, 1000, 2000, 3000, 4000] {
            let cur = advance_interval(Duration::from_millis(ms), 3.0, 4.0, 0.0, frame);
            assert!(cur <= prev, "interval should shrink as the hold continues");
            assert!(cur >= frame, "never faster than the refresh ceiling");
            prev = cur;
        }
    }

    #[test]
    fn advance_interval_no_ramp_when_floor_meets_refresh() {
        // A low-Hz display where min_rate >= refresh: no ramp, just the refresh cap.
        let frame = Duration::from_millis(500); // 2 Hz, below the 3/s floor
        assert_eq!(
            advance_interval(Duration::ZERO, 3.0, 4.0, 0.0, frame),
            frame
        );
        assert_eq!(
            advance_interval(Duration::from_secs(1), 3.0, 4.0, 0.0, frame),
            frame
        );
    }
}
