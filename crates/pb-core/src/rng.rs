//! A tiny, fast, deterministic PRNG (SplitMix64).
//!
//! We deliberately avoid the `rand` crate here: a self-contained generator is
//! dependency-free, trivially reproducible in tests, and produces identical
//! sequences across platforms (Windows today, macOS later). That determinism is
//! what makes the precomputed-random walk testable.

/// SplitMix64 — see Steele, Lea & Flood (2014). Excellent statistical quality
/// for a single-`u64` state, and a couple of multiplies per draw.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Seed the generator. Any seed (including 0) is valid.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Draw the next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform integer in `[0, bound)` via Lemire's multiply-shift with
    /// rejection for the (rare) biased low tail. `bound` must be non-zero.
    pub fn next_bounded(&mut self, bound: u64) -> u64 {
        debug_assert!(bound > 0, "bound must be non-zero");
        let mut x = self.next_u64();
        let mut m = (x as u128).wrapping_mul(bound as u128);
        let mut low = m as u64;
        if low < bound {
            // 2^64 mod bound, computed without 128-bit division.
            let threshold = bound.wrapping_neg() % bound;
            while low < threshold {
                x = self.next_u64();
                m = (x as u128).wrapping_mul(bound as u128);
                low = m as u64;
            }
        }
        let _ = x;
        (m >> 64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_same_seed() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = SplitMix64::new(1);
        let mut b = SplitMix64::new(2);
        let seq_a: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let seq_b: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        assert_ne!(seq_a, seq_b);
    }

    #[test]
    fn bounded_stays_in_range() {
        let mut r = SplitMix64::new(12345);
        for bound in [1u64, 2, 3, 7, 100, 1000, u32::MAX as u64] {
            for _ in 0..2000 {
                let v = r.next_bounded(bound);
                assert!(v < bound, "{v} >= {bound}");
            }
        }
    }

    #[test]
    fn bound_of_one_is_always_zero() {
        let mut r = SplitMix64::new(7);
        for _ in 0..100 {
            assert_eq!(r.next_bounded(1), 0);
        }
    }

    #[test]
    fn bounded_covers_the_range() {
        // Every value in a small range should appear at least once.
        let mut r = SplitMix64::new(99);
        let mut seen = [false; 6];
        for _ in 0..10_000 {
            seen[r.next_bounded(6) as usize] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }
}
