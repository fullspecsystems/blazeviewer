//! Prefetch policy: given where the user is and which way they're heading,
//! decide which items to have decoded and resident *next*.
//!
//! This is the single most important piece of logic for perceived speed: if the
//! window is right, every keypress is a cache hit and the photo is already in
//! VRAM. It is expressed as a pure function so we can unit-test it exhaustively
//! and A/B alternative policies in microbenchmarks with no GPU/I/O noise.

use crate::playlist::{Direction, Playlist};

/// Prioritized, de-duplicated list of item indices to keep resident, highest
/// priority first. The current item is always first.
///
/// `ahead` is the window size in the current direction of travel; `behind` is a
/// smaller trailing window so reversing direction is still cheap.
pub fn prefetch_targets(pl: &Playlist, ahead: usize, behind: usize) -> Vec<usize> {
    let len = pl.len();
    let mut out = Vec::with_capacity(ahead + behind + 1);
    if len == 0 {
        return out;
    }
    let mut seen = vec![false; len];
    let cur = pl.current().unwrap();
    push(&mut out, &mut seen, cur);

    match pl.last_direction() {
        Direction::Forward => {
            extend_linear(&mut out, &mut seen, pl, cur, 1, ahead);
            extend_linear(&mut out, &mut seen, pl, cur, -1, behind);
        }
        Direction::Backward => {
            extend_linear(&mut out, &mut seen, pl, cur, -1, ahead);
            extend_linear(&mut out, &mut seen, pl, cur, 1, behind);
        }
        Direction::Random => {
            let pos = pl.shuffle_pos();
            extend_random(&mut out, &mut seen, pl, pos, 1, ahead);
            extend_random(&mut out, &mut seen, pl, pos, -1, behind);
        }
    }
    out
}

fn extend_linear(
    out: &mut Vec<usize>,
    seen: &mut [bool],
    pl: &Playlist,
    cur: usize,
    sign: isize,
    count: usize,
) {
    let len = pl.len();
    for k in 1..=count {
        if let Some(i) = offset_index(cur, sign * k as isize, len, pl.wraps()) {
            push(out, seen, i);
        }
    }
}

fn extend_random(
    out: &mut Vec<usize>,
    seen: &mut [bool],
    pl: &Playlist,
    pos: usize,
    sign: isize,
    count: usize,
) {
    let len = pl.len();
    for k in 1..=count {
        if let Some(p) = offset_index(pos, sign * k as isize, len, pl.wraps()) {
            if let Some(i) = pl.shuffle().at(p) {
                push(out, seen, i as usize);
            }
        }
    }
}

fn push(out: &mut Vec<usize>, seen: &mut [bool], idx: usize) {
    if !seen[idx] {
        seen[idx] = true;
        out.push(idx);
    }
}

/// `cur + delta` within `[0, len)`, wrapping or returning `None` past the ends.
fn offset_index(cur: usize, delta: isize, len: usize, wrap: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let m = len as isize;
    let pos = cur as isize + delta;
    if wrap {
        Some((((pos % m) + m) % m) as usize)
    } else if pos < 0 || pos >= m {
        None
    } else {
        Some(pos as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_window_is_direction_biased() {
        let mut pl = Playlist::new(100, 1);
        pl.next(); // now at 1, Forward
        let t = prefetch_targets(&pl, 3, 1);
        // current first, then 3 ahead, then 1 behind
        assert_eq!(t, vec![1, 2, 3, 4, 0]);
    }

    #[test]
    fn backward_window_is_direction_biased() {
        let mut pl = Playlist::new(100, 1);
        for _ in 0..5 {
            pl.next();
        } // at 5, Forward
        pl.prev(); // at 4, Backward (kept away from index 0 so no wrap)
        let t = prefetch_targets(&pl, 3, 1);
        // current, then 3 behind (the travel direction), then 1 ahead
        assert_eq!(t, vec![4, 3, 2, 1, 5]);
    }

    #[test]
    fn wraps_at_boundaries() {
        let pl = Playlist::new(4, 1); // wrap on by default
        // at 0, Forward
        let t = prefetch_targets(&pl, 2, 2);
        // 0, then +1,+2 => 1,2, then -1,-2 => 3,2(dup) => 3
        assert_eq!(t, vec![0, 1, 2, 3]);
        assert_no_duplicates(&t);
    }

    #[test]
    fn no_wrap_omits_out_of_range() {
        let pl = Playlist::new(5, 1).with_wrap(false); // at 0, Forward
        let t = prefetch_targets(&pl, 3, 3);
        // nothing behind 0, three ahead
        assert_eq!(t, vec![0, 1, 2, 3]);
    }

    #[test]
    fn random_uses_the_shuffle_order() {
        let mut pl = Playlist::new(20, 0xC0FFEE);
        pl.random_next();
        let pos = pl.shuffle_pos();
        let expected_ahead: Vec<usize> =
            (1..=3).map(|k| pl.shuffle().at(pos + k).unwrap() as usize).collect();
        let t = prefetch_targets(&pl, 3, 0);
        assert_eq!(t[0], pl.current().unwrap());
        assert_eq!(&t[1..], &expected_ahead[..]);
    }

    #[test]
    fn empty_playlist_has_no_targets() {
        let pl = Playlist::new(0, 1);
        assert!(prefetch_targets(&pl, 5, 5).is_empty());
    }

    fn assert_no_duplicates(v: &[usize]) {
        let mut s = std::collections::HashSet::new();
        for &x in v {
            assert!(s.insert(x), "duplicate {x}");
        }
    }
}
