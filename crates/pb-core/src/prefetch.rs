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
    prefetch_targets_impl(pl, ahead, behind, false)
}

/// Like [`prefetch_targets`], but for use **while a directory scan is still streaming in**.
/// The random deck regenerates on every batch, so prefetching the random look-ahead would
/// decode-then-evict photos the user never sees (thrash). This variant prefetches only the
/// **stable sequential** neighbours of the current photo and does **not wrap** past the last
/// loaded item (the tail is still growing; wrapping to index 0 would skip the un-loaded
/// remainder). A random keypress mid-scan is served on-demand (preview-first) rather than
/// prefetched; once the scan completes, callers switch back to [`prefetch_targets`] and
/// random-ahead prefetch resumes.
pub fn prefetch_targets_scanning(pl: &Playlist, ahead: usize, behind: usize) -> Vec<usize> {
    prefetch_targets_impl(pl, ahead, behind, true)
}

fn prefetch_targets_impl(pl: &Playlist, ahead: usize, behind: usize, scanning: bool) -> Vec<usize> {
    let len = pl.len();
    let mut out = Vec::with_capacity(ahead + behind + 1);
    if len == 0 {
        return out;
    }
    let mut seen = vec![false; len];
    let cur = pl.current().unwrap();
    push(&mut out, &mut seen, cur);

    if scanning {
        // Anti-thrash: stable sequential neighbours only, no wrap (the tail is still
        // growing), biased by travel direction (Random defaults to forward). No random
        // look-ahead and no random hedges — the unstable deck is left alone until the
        // scan completes. See [`prefetch_targets_scanning`].
        let (ahead_sign, behind_sign) = match pl.last_direction() {
            Direction::Backward => (-1, 1),
            _ => (1, -1),
        };
        extend_linear(&mut out, &mut seen, pl, cur, ahead_sign, ahead, false);
        extend_linear(&mut out, &mut seen, pl, cur, behind_sign, behind, false);
        return out;
    }

    match pl.last_direction() {
        Direction::Forward => {
            // Cede up to two look-ahead slots to the random hedges — the
            // next-random (`enter`) and prior-random (`shift+enter`) photos — so
            // jumping into random nav while paging sequentially is an instant hit.
            // Appended last (prior-random lowest of all), so sequential fly is
            // unaffected; keep ≥1 look-ahead even on a tiny ring (a hedge that then
            // overflows capacity is simply dropped by the ring's rank check). In
            // random mode these are already covered by the random ahead/behind
            // windows, so the hedge lives only here.
            extend_linear(
                &mut out,
                &mut seen,
                pl,
                cur,
                1,
                ahead.saturating_sub(2).max(1),
                pl.wraps(),
            );
            extend_linear(&mut out, &mut seen, pl, cur, -1, behind, pl.wraps());
            hedge_random(&mut out, &mut seen, pl);
        }
        Direction::Backward => {
            extend_linear(
                &mut out,
                &mut seen,
                pl,
                cur,
                -1,
                ahead.saturating_sub(2).max(1),
                pl.wraps(),
            );
            extend_linear(&mut out, &mut seen, pl, cur, 1, behind, pl.wraps());
            hedge_random(&mut out, &mut seen, pl);
        }
        Direction::Random => {
            let pos = pl.shuffle_pos();
            // Steal a couple of slots from the random look-ahead for the
            // *sequential* neighbours of the current photo (cur±1), appended at
            // LOW priority. This makes the first space/backspace after a random
            // jump an instant ring hit instead of a cold decode; the low priority
            // means the random fly's look-ahead is filled first (the hedge only
            // loads once the pool catches up at rest, so holding [enter] still
            // flies). Total stays within ahead+behind+1 so the hedge is within
            // the ring's capacity.
            const HEDGE: usize = 2; // cur+1, cur-1
            let ahead_random = ahead.saturating_sub(HEDGE);
            extend_random(&mut out, &mut seen, pl, pos, 1, ahead_random);
            extend_random(&mut out, &mut seen, pl, pos, -1, behind);
            extend_linear(&mut out, &mut seen, pl, cur, 1, 1, pl.wraps());
            extend_linear(&mut out, &mut seen, pl, cur, -1, 1, pl.wraps());
        }
    }
    out
}

/// Choose which items to hold at **full resolution** — the "sharp ring" the user
/// browses within — given the prioritized prefetch `window` (current first, from
/// [`prefetch_targets`]), the per-slot full-res byte estimate `full_bytes`, the
/// VRAM `budget`, and the ring's slot `capacity`.
///
/// Returns the longest current-first **prefix** of `window` that fits both the byte
/// budget (`count * full_bytes <= budget`) and the slot `capacity`. A prefix is the
/// right shape because `window` is already priority-ordered (current, then
/// direction-biased neighbours), so the cursor and the photos you're most likely to
/// step to are sharpened first.
///
/// The budget bound is load-bearing: the in-place preview→full upgrade
/// ([`crate::ring::ResidentRing::set_slot_bytes`]) does **not** evict, so a settled
/// folder of fulls would balloon past the VRAM cap unless the upgrade set is bounded
/// *before* it is issued. The current item is always included (a `>= 1` floor),
/// mirroring the ring's "a single over-budget image still shows" rule.
pub fn full_ring(window: &[usize], full_bytes: u64, budget: u64, capacity: usize) -> Vec<usize> {
    if window.is_empty() || capacity == 0 {
        return Vec::new();
    }
    let by_budget = (budget / full_bytes.max(1)).max(1) as usize;
    let take = window.len().min(by_budget).min(capacity);
    window[..take].to_vec()
}

fn extend_linear(
    out: &mut Vec<usize>,
    seen: &mut [bool],
    pl: &Playlist,
    cur: usize,
    sign: isize,
    count: usize,
    wrap: bool,
) {
    let len = pl.len();
    for k in 1..=count {
        if let Some(i) = offset_index(cur, sign * k as isize, len, wrap) {
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
    // Forward random nav reshuffles into a fresh deck at the end of a cycle, so to
    // prefetch the photos the user will *actually* see next we peek that next deck
    // for positions past the current deck's end (wrapping within the current deck
    // would target items already shown this cycle). Computed once, and only when
    // the window genuinely spills over the boundary.
    let next = (sign > 0 && pos + count >= len).then(|| pl.next_shuffle());
    for k in 1..=count {
        let raw = pos as isize + sign * k as isize;
        let item = if raw >= 0 && (raw as usize) < len {
            // Inside the current deck (both directions).
            pl.shuffle().at(raw as usize)
        } else if sign > 0 {
            // Past the end of the current deck → the reshuffled next deck.
            next.as_ref().and_then(|d| d.at(raw as usize - len))
        } else if pl.wraps() {
            // Behind, before the start → wrap to the deck's tail, matching
            // `random_prev` (which wraps within the same deck, not un-shuffling).
            offset_index(pos, sign * k as isize, len, true).and_then(|p| pl.shuffle().at(p))
        } else {
            None
        };
        if let Some(i) = item {
            push(out, seen, i as usize);
        }
    }
}

/// Append the next- and prior-random targets (what `enter` / `shift+enter` would
/// jump to) as low-priority hedges, so switching from sequential into random nav
/// is an instant hit. De-duplicated against what's already queued; the two
/// coincide before any random nav has started, so only one slot is used then.
fn hedge_random(out: &mut Vec<usize>, seen: &mut [bool], pl: &Playlist) {
    if let Some(r) = pl.peek_random_next() {
        push(out, seen, r);
    }
    if let Some(r) = pl.peek_random_prev() {
        push(out, seen, r);
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
        let pn = pl.peek_random_next().unwrap();
        let t = prefetch_targets(&pl, 5, 2);
        // current, then ahead-2 forward (two slots ceded to the random hedges),
        // then behind, then the next/prior-random hedges (so enter / shift+enter
        // are warm while paging sequentially).
        assert_eq!(t[0], 1);
        assert_eq!(&t[1..=3], &[2, 3, 4]); // 3 forward look-ahead (ahead 5 − 2)
        assert!(t.contains(&0), "the immediate previous photo (behind)");
        assert!(t.contains(&pn), "the next-random photo is hedged in");
    }

    #[test]
    fn backward_window_is_direction_biased() {
        let mut pl = Playlist::new(100, 1);
        for _ in 0..5 {
            pl.next();
        } // at 5, Forward
        pl.prev(); // at 4, Backward (kept away from index 0 so no wrap)
        let pn = pl.peek_random_next().unwrap();
        let t = prefetch_targets(&pl, 5, 2);
        // current, then 3 behind (travel dir), then ahead, then the random hedges.
        assert_eq!(t[0], 4);
        assert_eq!(&t[1..=3], &[3, 2, 1]);
        assert!(t.contains(&5), "one ahead (the reverse direction)");
        assert!(t.contains(&pn));
    }

    #[test]
    fn wraps_at_boundaries() {
        let pl = Playlist::new(10, 1); // wrap on by default; at 0, Forward
        let t = prefetch_targets(&pl, 3, 3);
        // Behind wraps past index 0 to the tail: -1,-2,-3 => 9,8,7 must be present.
        for w in [9, 8, 7] {
            assert!(t.contains(&w), "behind should wrap to {w}");
        }
        assert_eq!(t[0], 0);
        assert_no_duplicates(&t);
    }

    #[test]
    fn no_wrap_omits_out_of_range() {
        let pl = Playlist::new(10, 1).with_wrap(false); // at 0, Forward
        let pn = pl.peek_random_next().unwrap();
        let pp = pl.peek_random_prev().unwrap();
        let t = prefetch_targets(&pl, 3, 3);
        assert_eq!(t[0], 0);
        assert!(t.contains(&1), "one forward look-ahead");
        // Index 9 would only appear by wrapping -1 past the start, which no-wrap
        // forbids — though it may legitimately appear as a random hedge.
        assert!(
            !t.contains(&9) || pn == 9 || pp == 9,
            "no wrap-around behind index 0"
        );
    }

    #[test]
    fn random_uses_the_shuffle_order_then_a_sequential_hedge() {
        let mut pl = Playlist::new(20, 0xC0FFEE);
        pl.random_next();
        let pos = pl.shuffle_pos();
        let cur = pl.current().unwrap();
        // ahead=5, behind=2 → the random look-ahead is ahead-HEDGE = 3 deep
        // (high priority, for hold-to-fly), then a cur±1 sequential hedge.
        let t = prefetch_targets(&pl, 5, 2);
        assert_eq!(t[0], cur);
        let expected_random_ahead: Vec<usize> = (1..=3)
            .map(|k| pl.shuffle().at(pos + k).unwrap() as usize)
            .collect();
        assert_eq!(
            &t[1..=3],
            &expected_random_ahead[..],
            "the random look-ahead comes first"
        );
        // The current photo's sequential neighbours are also prefetched, so the
        // first space/backspace after a random jump is an instant hit.
        let len = pl.len();
        let next_seq = (cur + 1) % len;
        let prev_seq = (cur + len - 1) % len;
        assert!(t.contains(&next_seq), "cur+1 sequential hedge missing");
        assert!(t.contains(&prev_seq), "cur-1 sequential hedge missing");
    }

    #[test]
    fn empty_playlist_has_no_targets() {
        let pl = Playlist::new(0, 1);
        assert!(prefetch_targets(&pl, 5, 5).is_empty());
    }

    // Random `enter` nav is now wired, so the cycle-boundary prefetch is live:
    // `extend_random` peeks `Playlist::next_shuffle()` (the deck `random_next`
    // reshuffles into) for positions past the current deck's end, so prefetching
    // ahead at the boundary targets the *next* cycle's items — exactly what the
    // user sees next. (Was a pinned `#[ignore]`d bug; fixed with the wiring.)
    #[test]
    fn random_prefetch_spans_the_cycle_boundary() {
        let len = 8;
        let mut pl = Playlist::new(len, 0xABCD);
        // Walk to the final position of the current shuffle cycle.
        for _ in 0..len {
            pl.random_next();
        }
        // What the engine would prefetch ahead from here (ahead=5 → 3 random
        // look-ahead after the HEDGE steal, then the sequential hedge)...
        let predicted = prefetch_targets(&pl, 5, 0);
        // ...vs. what the user actually sees next, across the reshuffle boundary.
        let mut upcoming = Vec::new();
        for _ in 0..3 {
            pl.random_next();
            upcoming.push(pl.current().unwrap());
        }
        // The prefetched random look-ahead spans the reshuffle seam: it equals the
        // next three photos the user will see, drawn from the *reshuffled* deck.
        assert_eq!(&predicted[1..=3], &upcoming[..]);
    }

    fn assert_no_duplicates(v: &[usize]) {
        let mut s = std::collections::HashSet::new();
        for &x in v {
            assert!(s.insert(x), "duplicate {x}");
        }
    }

    // --- scanning variant (anti-thrash while a scan streams in) ----------------

    #[test]
    fn scanning_suppresses_the_random_ahead_window() {
        use std::collections::HashSet;
        // In random mode a normal prefetch peeks the shuffle deck ahead. During a scan
        // that deck regenerates every batch, so prefetching it would decode-then-evict
        // photos the user never sees. The scanning variant prefetches ONLY the stable
        // sequential neighbours of the current photo — nothing from the random deck.
        let mut pl = Playlist::new(50, 0xC0FFEE);
        pl.random_next();
        let cur = pl.current().unwrap() as isize;
        let len = pl.len() as isize;
        let t = prefetch_targets_scanning(&pl, 5, 2);
        assert_eq!(t[0], cur as usize);
        // Exactly: current + forward `ahead` + backward `behind`, in range, no wrap
        // (Random defaults to forward bias).
        let mut expected: HashSet<usize> = HashSet::from([cur as usize]);
        for k in 1..=5 {
            if cur + k < len {
                expected.insert((cur + k) as usize);
            }
        }
        for k in 1..=2 {
            if cur - k >= 0 {
                expected.insert((cur - k) as usize);
            }
        }
        let got: HashSet<usize> = t.iter().copied().collect();
        assert_eq!(
            got, expected,
            "scanning prefetch is sequential-only, no random deck"
        );
    }

    #[test]
    fn scanning_does_not_wrap_past_the_growing_tail() {
        // At index 0, a normal wrap-on prefetch pulls the tail (9, 8, 7) behind. During a
        // scan the tail is still growing, so wrapping back to it would skip the un-loaded
        // remainder — the scanning variant must not wrap.
        let pl = Playlist::new(10, 1); // wrap on, at 0, Forward
        let t = prefetch_targets_scanning(&pl, 3, 3);
        assert_eq!(t[0], 0);
        for w in [9, 8, 7] {
            assert!(
                !t.contains(&w),
                "scanning must not wrap behind 0 to the tail ({w})"
            );
        }
        // Forward neighbours (loaded, in range) are still prefetched.
        assert!(t.contains(&1) && t.contains(&2) && t.contains(&3));
        assert_no_duplicates(&t);
    }

    #[test]
    fn scanning_is_direction_biased() {
        let mut pl = Playlist::new(100, 1);
        for _ in 0..10 {
            pl.next();
        } // at 10, Forward
        let t = prefetch_targets_scanning(&pl, 4, 1);
        assert_eq!(t[0], 10);
        assert!(
            t.contains(&11) && t.contains(&12),
            "forward bias looks ahead"
        );
        pl.prev(); // at 9, Backward
        let t = prefetch_targets_scanning(&pl, 4, 1);
        assert_eq!(t[0], 9);
        assert!(t.contains(&8) && t.contains(&7), "backward bias looks back");
    }

    #[test]
    fn scanning_empty_playlist_has_no_targets() {
        assert!(prefetch_targets_scanning(&Playlist::new(0, 1), 5, 5).is_empty());
    }

    #[test]
    fn full_ring_is_empty_for_no_window_or_no_slots() {
        assert!(full_ring(&[], 100, 1000, 8).is_empty());
        assert!(full_ring(&[1, 2, 3], 100, 1000, 0).is_empty());
    }

    #[test]
    fn full_ring_takes_the_whole_window_when_it_fits() {
        // 5 items * 100 bytes = 500 <= 1000 budget, and 5 <= 8 slots.
        let w = [5, 6, 4, 7, 3];
        assert_eq!(full_ring(&w, 100, 1000, 8), w);
    }

    #[test]
    fn full_ring_is_bounded_by_the_byte_budget_current_first() {
        // budget 350 / 100 bytes = 3 fulls fit; the prefix keeps the current item
        // (index 0) and the highest-priority neighbours.
        let w = [5, 6, 4, 7, 3, 8];
        assert_eq!(full_ring(&w, 100, 350, 16), vec![5, 6, 4]);
    }

    #[test]
    fn full_ring_is_bounded_by_capacity() {
        // The budget would allow all six, but the ring only has 4 slots.
        let w = [5, 6, 4, 7, 3, 8];
        assert_eq!(full_ring(&w, 1, 1_000_000, 4), vec![5, 6, 4, 7]);
    }

    #[test]
    fn full_ring_always_keeps_the_current_item() {
        // One image larger than the entire budget still gets a full slot (>=1 floor),
        // matching the ring's single-over-budget-still-shows rule.
        assert_eq!(full_ring(&[9, 1, 2], 10_000, 100, 8), vec![9]);
    }
}
