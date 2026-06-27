//! Lightweight, opt-in timing instrumentation for the hot path (plan §3.6a).
//!
//! The point of Phase 3 is speed, and "we do not guess about speed — we measure
//! it." This module is the cheap, dependency-free first layer: it records
//! per-stage durations (scan / read / decode / upload / render) and summarizes
//! them as p50/p95/p99 so every later optimization shows a before/after.
//!
//! **Privacy (tasks.json #2):** nothing is written to disk by default. Timings
//! live only in RAM; a summary is printed to the console (debug builds) on exit.
//! Persisting per-frame NDJSON is opt-in (a future `--metrics <file>` flag).

use std::collections::BTreeMap;
use std::time::Duration;

/// Nearest-rank percentiles of `samples` at each percentile in `ps` (`0.0..=100.0`).
///
/// Returns `NaN` for each requested percentile when `samples` is empty. Does not
/// mutate the input. Nearest-rank is used (not interpolation) so the result is
/// always an observed sample — unambiguous to reason about and to test.
pub fn percentiles(samples: &[f64], ps: &[f64]) -> Vec<f64> {
    if samples.is_empty() {
        return vec![f64::NAN; ps.len()];
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    ps.iter()
        .map(|&p| {
            let p = p.clamp(0.0, 100.0);
            // 1-based rank = ceil(p/100 * n); index is rank-1, clamped to [0, n).
            let rank = ((p / 100.0) * n as f64).ceil() as usize;
            let idx = rank.saturating_sub(1).min(n - 1);
            sorted[idx]
        })
        .collect()
}

/// Collects per-stage timing samples (in milliseconds) and reports percentiles.
///
/// Cheap and allocation-light on the hot path: a stage push is one `Vec` append.
/// Disabled instances record nothing, so leaving timing calls in place costs a
/// branch when metrics are off.
#[derive(Default)]
pub struct StageTimes {
    enabled: bool,
    stages: BTreeMap<&'static str, Vec<f64>>,
}

impl StageTimes {
    /// A recorder that ignores all samples (the default, privacy-preserving mode).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            stages: BTreeMap::new(),
        }
    }

    /// A recorder that accumulates samples for an end-of-run summary.
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            stages: BTreeMap::new(),
        }
    }

    /// Record one timing for `stage` (no-op when disabled).
    pub fn record(&mut self, stage: &'static str, dur: Duration) {
        if !self.enabled {
            return;
        }
        self.stages
            .entry(stage)
            .or_default()
            .push(dur.as_secs_f64() * 1000.0);
    }

    /// One summary row per stage: `(stage, count, p50_ms, p95_ms, p99_ms)`.
    pub fn summary(&self) -> Vec<(&'static str, usize, f64, f64, f64)> {
        self.stages
            .iter()
            .map(|(&stage, samples)| {
                let p = percentiles(samples, &[50.0, 95.0, 99.0]);
                (stage, samples.len(), p[0], p[1], p[2])
            })
            .collect()
    }

    /// A human-readable multi-line report (empty string when nothing was recorded).
    pub fn report(&self) -> String {
        let rows = self.summary();
        if rows.is_empty() {
            return String::new();
        }
        let mut out = String::from("stage timings (ms)        n      p50      p95      p99\n");
        for (stage, n, p50, p95, p99) in rows {
            out.push_str(&format!(
                "  {stage:<22} {n:>5}  {p50:>7.3}  {p95:>7.3}  {p99:>7.3}\n"
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn percentiles_of_empty_are_nan() {
        let out = percentiles(&[], &[50.0, 95.0, 99.0]);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|v| v.is_nan()));
    }

    #[test]
    fn percentiles_of_single_sample_are_that_sample() {
        let out = percentiles(&[7.5], &[0.0, 50.0, 100.0]);
        assert!(out.iter().all(|&v| approx(v, 7.5)));
    }

    #[test]
    fn nearest_rank_on_one_through_ten() {
        // Unsorted on purpose — the function must sort a copy.
        let s = [10.0, 1.0, 9.0, 2.0, 8.0, 3.0, 7.0, 4.0, 6.0, 5.0];
        let out = percentiles(&s, &[0.0, 50.0, 95.0, 99.0, 100.0]);
        // ranks: ceil(0)=0->idx0=1, ceil(5)=5->idx4=5, ceil(9.5)=10->idx9=10,
        //        ceil(9.9)=10->10, ceil(10)=10->10.
        assert!(approx(out[0], 1.0), "p0 = {}", out[0]);
        assert!(approx(out[1], 5.0), "p50 = {}", out[1]);
        assert!(approx(out[2], 10.0), "p95 = {}", out[2]);
        assert!(approx(out[3], 10.0), "p99 = {}", out[3]);
        assert!(approx(out[4], 10.0), "p100 = {}", out[4]);
    }

    #[test]
    fn percentiles_clamp_out_of_range() {
        let s = [1.0, 2.0, 3.0];
        let out = percentiles(&s, &[-10.0, 200.0]);
        assert!(approx(out[0], 1.0));
        assert!(approx(out[1], 3.0));
    }

    #[test]
    fn disabled_records_nothing() {
        let mut t = StageTimes::disabled();
        t.record("decode", Duration::from_millis(5));
        assert!(t.summary().is_empty());
        assert_eq!(t.report(), "");
    }

    #[test]
    fn enabled_collects_and_summarizes() {
        let mut t = StageTimes::enabled();
        for ms in [2u64, 4, 6, 8, 10] {
            t.record("decode", Duration::from_millis(ms));
        }
        let rows = t.summary();
        assert_eq!(rows.len(), 1);
        let (stage, n, p50, _p95, _p99) = rows[0];
        assert_eq!(stage, "decode");
        assert_eq!(n, 5);
        // nearest-rank p50 of 2,4,6,8,10 -> rank ceil(2.5)=3 -> idx2 -> 6ms.
        assert!(approx(p50, 6.0), "p50 = {p50}");
        assert!(!t.report().is_empty());
    }
}
