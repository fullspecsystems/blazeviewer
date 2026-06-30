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

    /// The item [`Playlist::random_next`] would land on next, *without* advancing.
    /// Lets the prefetcher keep the photo behind `enter` resident even while the
    /// user is paging sequentially, so switching into random nav is an instant hit
    /// (mirrors how random mode hedges the sequential neighbours). Spans the
    /// reshuffle boundary like `random_next` does.
    pub fn peek_random_next(&self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let item = if !self.random_started {
            self.shuffle.at(0)
        } else if self.shuffle_pos + 1 >= self.len {
            self.next_shuffle().at(0)
        } else {
            self.shuffle.at(self.shuffle_pos + 1)
        };
        item.map(|i| i as usize)
    }

    /// The item [`Playlist::random_prev`] would land on next, *without* advancing —
    /// the prior entry in the random walk (for `Shift+Enter`: revisit one you flew
    /// past). Mirrors `random_prev`'s wrap (at the deck start it wraps to the tail).
    /// Before any random nav it is the deck's first item (same as `peek_random_next`
    /// — there is no prior yet, and we don't invent one).
    pub fn peek_random_prev(&self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let item = if !self.random_started {
            self.shuffle.at(0)
        } else if self.shuffle_pos == 0 {
            if self.wrap {
                self.shuffle.at(self.len - 1)
            } else {
                self.shuffle.at(0)
            }
        } else {
            self.shuffle.at(self.shuffle_pos - 1)
        };
        item.map(|i| i as usize)
    }

    /// Current position within the random order.
    pub fn shuffle_pos(&self) -> usize {
        self.shuffle_pos
    }

    /// Grow the playlist to `new_len`, preserving the current cursor — the streaming
    /// scan's "more images arrived" path. Because indices are **append-only** (a grow
    /// never reorders existing items), the displayed photo never moves: index *i*
    /// still means the same photo. A no-op when `new_len` isn't larger (snapshots only
    /// grow; we never shrink under the user).
    ///
    /// The random deck is regenerated to cover the larger universe (a cheap integer
    /// shuffle — the expensive random-*ahead* prefetch is suppressed separately while a
    /// scan is live, so this doesn't thrash the decode ring). If a random walk is
    /// already in progress, `shuffle_pos` is reseated onto the current item in the new
    /// deck, so the next [`random_next`](Playlist::random_next) advances to a fresh item
    /// and the on-screen photo stays put. (Already-visited history is not preserved
    /// across a grow — the accepted relaxation during a live scan.)
    pub fn extend(&mut self, new_len: usize) {
        if new_len <= self.len {
            return;
        }
        self.len = new_len;
        self.shuffle = self.shuffle.resized(new_len);
        if self.random_started {
            self.shuffle_pos = self.shuffle.position_of(self.cursor as u32).unwrap_or(0);
        }
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
    fn peek_random_next_matches_random_next() {
        // From the not-started state, then mid-deck: peek == what we land on.
        let mut pl = Playlist::new(10, 1);
        let peek = pl.peek_random_next();
        pl.random_next();
        assert_eq!(peek, pl.current());
        let peek = pl.peek_random_next();
        pl.random_next();
        assert_eq!(peek, pl.current());
        // Across the reshuffle boundary (exhaust the deck, then peek the next).
        let mut pl = Playlist::new(5, 7);
        for _ in 0..5 {
            pl.random_next();
        }
        let peek = pl.peek_random_next();
        pl.random_next(); // reshuffles into a fresh deck
        assert_eq!(peek, pl.current());
        // Empty playlist has nothing to peek.
        assert_eq!(Playlist::new(0, 1).peek_random_next(), None);
    }

    #[test]
    fn peek_random_prev_matches_random_prev() {
        // Mid-deck: peek-prev == stepping back.
        let mut pl = Playlist::new(10, 1);
        pl.random_next();
        pl.random_next();
        pl.random_next();
        let peek = pl.peek_random_prev();
        pl.random_prev();
        assert_eq!(peek, pl.current());
        // At the deck start, prev wraps to the tail (same deck).
        let mut pl = Playlist::new(6, 3);
        pl.random_next(); // started, at pos 0
        let peek = pl.peek_random_prev();
        pl.random_prev(); // pos 0 -> wraps to len-1
        assert_eq!(peek, pl.current());
        // Empty playlist has nothing to peek.
        assert_eq!(Playlist::new(0, 1).peek_random_prev(), None);
    }

    #[test]
    fn with_cursor_seats_and_clamps_the_start() {
        assert_eq!(Playlist::new(5, 1).with_cursor(3).current(), Some(3));
        // Out-of-range clamps to the last item.
        assert_eq!(Playlist::new(5, 1).with_cursor(99).current(), Some(4));
        // Empty playlist stays empty.
        assert_eq!(Playlist::new(0, 1).with_cursor(2).current(), None);
    }

    // --- streaming grow (`extend`) ---------------------------------------------

    #[test]
    fn extend_grows_len_and_keeps_the_displayed_photo() {
        // A streaming scan appends images; the photo on screen must not move (indices
        // are append-only, so index i is still the same photo).
        let mut pl = Playlist::new(3, 1).with_cursor(2);
        pl.extend(10);
        assert_eq!(pl.len(), 10);
        assert_eq!(pl.current(), Some(2));
        // The newly appended range is now navigable.
        pl.next();
        assert_eq!(pl.current(), Some(3));
    }

    #[test]
    fn extend_is_a_noop_when_not_larger() {
        // Snapshots only ever grow; a same/smaller len is ignored (never shrinks under
        // the user).
        let mut pl = Playlist::new(5, 1).with_cursor(2);
        pl.extend(5);
        assert_eq!(pl.len(), 5);
        pl.extend(3);
        assert_eq!(pl.len(), 5);
        assert_eq!(pl.current(), Some(2));
    }

    #[test]
    fn extend_updates_sequential_wrap_to_the_new_len() {
        let mut pl = Playlist::new(3, 1); // wrap = true, cursor 0
        pl.extend(5);
        pl.prev(); // wraps to the NEW last index (4), not the old (2)
        assert_eq!(pl.current(), Some(4));
    }

    #[test]
    fn extend_before_random_started_covers_the_full_grown_universe() {
        let mut pl = Playlist::new(3, 9);
        pl.extend(20);
        // A full random cycle now visits every one of the 20 items exactly once.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..20 {
            pl.random_next();
            seen.insert(pl.current().unwrap());
        }
        assert_eq!(seen.len(), 20);
    }

    #[test]
    fn extend_after_random_started_keeps_current_and_continues_the_walk() {
        let mut pl = Playlist::new(5, 4);
        pl.random_next();
        pl.random_next();
        let here = pl.current().unwrap();
        pl.extend(40);
        // The displayed photo is unchanged by the grow (cursor preserved)...
        assert_eq!(pl.current(), Some(here));
        // ...and the walk continues from here: the next random lands on a different,
        // in-range item (shuffle_pos was reseated onto `here` in the regenerated deck).
        let next = pl.peek_random_next();
        assert_ne!(next, Some(here));
        pl.random_next();
        assert_eq!(pl.current(), next);
        assert!(pl.current().unwrap() < 40);
    }
}
