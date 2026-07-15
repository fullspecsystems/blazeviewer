//! Precomputed random visiting order.
//!
//! "Random" navigation is implemented as walking a *precomputed* permutation of
//! all items rather than rolling a die per keypress. This is the key trick that
//! makes random seeks prefetchable: because the next random targets are already
//! known, the prefetcher can decode them into VRAM ahead of time, so holding
//! [enter] blazes through a shuffled deck just as fast as sequential paging.
//!
//! Walking the order visits every item exactly once before a reshuffle, so the
//! user sees no repeats within a cycle.

use crate::rng::SplitMix64;

/// A fixed permutation of `0..len`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShuffleOrder {
    order: Vec<u32>,
    seed: u64,
}

impl ShuffleOrder {
    /// Build a permutation of `0..len` from `seed` via Fisher–Yates.
    pub fn new(len: usize, seed: u64) -> Self {
        let mut order: Vec<u32> = (0..len as u32).collect();
        let mut rng = SplitMix64::new(seed);
        for i in (1..len).rev() {
            let j = rng.next_bounded((i + 1) as u64) as usize;
            order.swap(i, j);
        }
        Self { order, seed }
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The item index at position `pos` in the shuffled order.
    pub fn at(&self, pos: usize) -> Option<u32> {
        self.order.get(pos).copied()
    }

    /// The position of `item` within the order (the inverse of [`at`]), or `None`
    /// if it isn't in this universe. Used when a streaming grow reseats the random
    /// cursor onto the currently displayed item in the regenerated deck. O(len) —
    /// called once per grow, off the hot path.
    ///
    /// [`at`]: ShuffleOrder::at
    pub fn position_of(&self, item: u32) -> Option<usize> {
        self.order.iter().position(|&v| v == item)
    }

    /// A fresh permutation of `0..new_len` from the **same seed** — used when a
    /// streaming scan grows the universe and the deck must cover the newly found
    /// items. Deterministic for a given (seed, len), so it stays reproducible/testable.
    /// (Distinct from [`reshuffled`], which advances the seed for the *next cycle* at
    /// the same length.)
    ///
    /// [`reshuffled`]: ShuffleOrder::reshuffled
    pub fn resized(&self, new_len: usize) -> Self {
        ShuffleOrder::new(new_len, self.seed)
    }

    /// Produce the next permutation for when the current order is exhausted.
    ///
    /// The seed is advanced deterministically, and for `len > 1` we guarantee
    /// the first item of the new order differs from the last item of the old
    /// one, so the user never sees the same photo twice across the seam.
    pub fn reshuffled(&self) -> Self {
        let next_seed = self
            .seed
            .wrapping_mul(0x2545_F491_4F6C_DD1D)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut next = ShuffleOrder::new(self.order.len(), next_seed);
        if self.order.len() > 1 {
            if let (Some(&last), Some(&first)) = (self.order.last(), next.order.first()) {
                if last == first {
                    next.order.swap(0, 1);
                }
            }
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_permutation(o: &ShuffleOrder, len: usize) -> bool {
        let mut seen = vec![false; len];
        for pos in 0..len {
            let v = o.at(pos).unwrap() as usize;
            if v >= len || seen[v] {
                return false;
            }
            seen[v] = true;
        }
        seen.iter().all(|&s| s)
    }

    #[test]
    fn is_a_valid_permutation() {
        for len in [0usize, 1, 2, 5, 50, 999] {
            let o = ShuffleOrder::new(len, 0xABCD);
            assert_eq!(o.len(), len);
            assert!(is_permutation(&o, len));
        }
    }

    #[test]
    fn deterministic_for_same_seed() {
        let a = ShuffleOrder::new(100, 7);
        let b = ShuffleOrder::new(100, 7);
        assert_eq!(a, b);
    }

    #[test]
    fn empty_and_singleton() {
        assert!(ShuffleOrder::new(0, 1).is_empty());
        assert_eq!(ShuffleOrder::new(1, 1).at(0), Some(0));
    }

    #[test]
    fn at_out_of_bounds_is_none() {
        let o = ShuffleOrder::new(3, 1);
        assert_eq!(o.at(3), None);
    }

    #[test]
    fn reshuffle_is_permutation_without_seam_repeat() {
        for len in [2usize, 3, 10, 100] {
            let a = ShuffleOrder::new(len, 123);
            let b = a.reshuffled();
            assert!(is_permutation(&b, len));
            // No visible repeat across the cycle boundary.
            assert_ne!(a.at(len - 1), b.at(0));
        }
    }

    #[test]
    fn resized_is_a_permutation_of_the_new_len() {
        // Growing the universe (a streaming scan finds more images) yields a fresh,
        // valid permutation that covers every new index.
        let o = ShuffleOrder::new(5, 42);
        for new_len in [5usize, 6, 30, 1000] {
            let bigger = o.resized(new_len);
            assert_eq!(bigger.len(), new_len);
            assert!(is_permutation(&bigger, new_len));
        }
    }

    #[test]
    fn resized_is_deterministic_from_the_same_seed() {
        let o = ShuffleOrder::new(5, 42);
        assert_eq!(o.resized(30), o.resized(30));
    }

    #[test]
    fn position_of_inverts_at() {
        let o = ShuffleOrder::new(20, 7);
        for pos in 0..20 {
            let item = o.at(pos).unwrap();
            assert_eq!(o.position_of(item), Some(pos));
        }
        // An item outside the universe isn't present.
        assert_eq!(o.position_of(999), None);
    }
}
