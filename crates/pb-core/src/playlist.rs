//! The playlist + navigation cursor.
//!
//! A `Playlist` is the ordered set of images plus a cursor and the last
//! navigation direction. It owns both the sequential cursor (space/right,
//! backspace/left) and the precomputed-random walk ([enter]). It deliberately
//! stores only counts/indices — the actual paths live in the app layer — so the
//! whole thing is pure and unit-testable.

use crate::shuffle::ShuffleOrder;

/// The direction of the most recent navigation, used to bias prefetching toward
/// where the user is heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
    /// A random jump. There is no spatial locality, but the *upcoming* random
    /// targets are known from the precomputed order, so they remain prefetchable.
    Random,
}

/// Ordered images + a cursor + a precomputed random walk.
#[derive(Debug, Clone)]
pub struct Playlist {
    len: usize,
    cursor: usize,
    last_dir: Direction,
    wrap: bool,
    shuffle: ShuffleOrder,
    shuffle_pos: usize,
    random_started: bool,
}

impl Playlist {
    /// Create a playlist over `len` items. `seed` fixes the random walk so it is
    /// reproducible (and testable). The cursor starts at item 0.
    pub fn new(len: usize, seed: u64) -> Self {
        Self {
            len,
            cursor: 0,
            last_dir: Direction::Forward,
            wrap: true,
            shuffle: ShuffleOrder::new(len, seed),
            shuffle_pos: 0,
            random_started: false,
        }
    }

    /// Builder: whether sequential navigation wraps at the ends (default `true`).
    pub fn with_wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// Builder: start the cursor at `index` (clamped to `[0, len)`), e.g. on the
    /// photo the user double-clicked. A no-op on an empty playlist. The cursor
    /// resolution itself lives in [`crate::open::resolve_cursor`]; this just
    /// seats the result.
    pub fn with_cursor(mut self, index: usize) -> Self {
        if self.len > 0 {
            self.cursor = index.min(self.len - 1);
        }
        self
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The currently displayed item index, or `None` if the playlist is empty.
    pub fn current(&self) -> Option<usize> {
        if self.len == 0 {
            None
        } else {
            Some(self.cursor)
        }
    }

    pub fn last_direction(&self) -> Direction {
        self.last_dir
    }

    pub fn wraps(&self) -> bool {
        self.wrap
    }

    /// The precomputed random order (used by the prefetcher to peek ahead).
    pub fn shuffle(&self) -> &ShuffleOrder {
        &self.shuffle
    }

    /// The shuffle deck that [`Playlist::random_next`] will switch to once the
    /// current cycle is exhausted (deterministic — a pure function of the current
    /// seed). The prefetcher peeks this to preload *across* the reshuffle boundary,
    /// so holding [enter] past the end of a cycle still has the next cycle's first
    /// photos resident rather than the current deck's wrapped-around head.
    pub fn next_shuffle(&self) -> ShuffleOrder {
        self.shuffle.reshuffled()
    }

    /// Current position within the random order.
    pub fn shuffle_pos(&self) -> usize {
        self.shuffle_pos
    }

    /// Advance one item (space / right-arrow).
    pub fn next(&mut self) {
        if self.len == 0 {
            return;
        }
        self.cursor = step(self.cursor, 1, self.len, self.wrap);
        self.last_dir = Direction::Forward;
    }

    /// Go back one item (backspace / left-arrow).
    pub fn prev(&mut self) {
        if self.len == 0 {
            return;
        }
        self.cursor = step(self.cursor, -1, self.len, self.wrap);
        self.last_dir = Direction::Backward;
    }

    /// Jump to the next item in the precomputed random order ([enter]).
    pub fn random_next(&mut self) {
        if self.len == 0 {
            return;
        }
        if !self.random_started {
            self.random_started = true;
            self.shuffle_pos = 0;
        } else if self.shuffle_pos + 1 >= self.len {
            // Exhausted the deck — reshuffle and start the next cycle.
            self.shuffle = self.shuffle.reshuffled();
            self.shuffle_pos = 0;
        } else {
            self.shuffle_pos += 1;
        }
        self.cursor = self.shuffle.at(self.shuffle_pos).expect("pos in range") as usize;
        self.last_dir = Direction::Random;
    }

    /// Step back through the random history.
    pub fn random_prev(&mut self) {
        if self.len == 0 {
            return;
        }
        if !self.random_started {
            self.random_started = true;
            self.shuffle_pos = 0;
        } else if self.shuffle_pos == 0 {
            if self.wrap {
                self.shuffle_pos = self.len - 1;
            }
        } else {
            self.shuffle_pos -= 1;
        }
        self.cursor = self.shuffle.at(self.shuffle_pos).expect("pos in range") as usize;
        self.last_dir = Direction::Random;
    }
}

/// Move `cur` by `delta` within `[0, len)`, wrapping or clamping at the ends.
fn step(cur: usize, delta: isize, len: usize, wrap: bool) -> usize {
    debug_assert!(len > 0);
    let m = len as isize;
    let pos = cur as isize + delta;
    if wrap {
        (((pos % m) + m) % m) as usize
    } else if pos < 0 {
        0
    } else if pos >= m {
        len - 1
    } else {
        pos as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_playlist_is_inert() {
        let mut pl = Playlist::new(0, 1);
        assert!(pl.is_empty());
        assert_eq!(pl.current(), None);
        pl.next();
        pl.prev();
        pl.random_next();
        assert_eq!(pl.current(), None);
    }

    #[test]
    fn sequential_forward_and_back_with_wrap() {
        let mut pl = Playlist::new(3, 1);
        assert_eq!(pl.current(), Some(0));
        pl.next();
        assert_eq!(pl.current(), Some(1));
        assert_eq!(pl.last_direction(), Direction::Forward);
        pl.next();
        pl.next(); // wraps 2 -> 0
        assert_eq!(pl.current(), Some(0));
        pl.prev(); // wraps 0 -> 2
        assert_eq!(pl.current(), Some(2));
        assert_eq!(pl.last_direction(), Direction::Backward);
    }

    #[test]
    fn sequential_clamps_without_wrap() {
        let mut pl = Playlist::new(3, 1).with_wrap(false);
        pl.prev(); // clamp at 0
        assert_eq!(pl.current(), Some(0));
        pl.next();
        pl.next();
        pl.next(); // clamp at 2
        assert_eq!(pl.current(), Some(2));
    }

    #[test]
    fn random_walk_visits_every_item_once_per_cycle() {
        let len = 50;
        let mut pl = Playlist::new(len, 0xFEED);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..len {
            pl.random_next();
            seen.insert(pl.current().unwrap());
        }
        assert_eq!(
            seen.len(),
            len,
            "a full cycle should visit each item exactly once"
        );
        assert_eq!(pl.last_direction(), Direction::Random);
    }

    #[test]
    fn random_reshuffles_after_a_full_cycle() {
        let len = 8;
        let mut pl = Playlist::new(len, 3);
        for _ in 0..len {
            pl.random_next();
        }
        // Crossing into the next cycle must still yield a valid item.
        pl.random_next();
        assert!(pl.current().unwrap() < len);
    }

    #[test]
    fn random_prev_steps_back() {
        let mut pl = Playlist::new(10, 1);
        pl.random_next();
        let first = pl.current().unwrap();
        pl.random_next();
        pl.random_prev();
        assert_eq!(pl.current().unwrap(), first);
    }

    #[test]
    fn with_cursor_seats_and_clamps_the_start() {
        assert_eq!(Playlist::new(5, 1).with_cursor(3).current(), Some(3));
        // Out-of-range clamps to the last item.
        assert_eq!(Playlist::new(5, 1).with_cursor(99).current(), Some(4));
        // Empty playlist stays empty.
        assert_eq!(Playlist::new(0, 1).with_cursor(2).current(), None);
    }
}
