//! Thumbnail-cache **policy** (task #83): which thumbs are resident, what the
//! byte budget allows, what gets evicted first, and what to fill next. Pure —
//! no I/O, no pixels here beyond an opaque payload `T` (the orchestration layer
//! stores CPU RGBA8 buffers; tests store `()`), so every invariant is unit- and
//! property-testable. RAM-only by construction (ADR-018: no thumbnail DB, ever).
//!
//! Eviction protects what the user is looking at, not just what's near the
//! cursor (plan §7): visible cells are pinned, then overscan, then the warm
//! window around the current item, then everything else farthest-first.

use std::collections::HashMap;

/// Quality tier of a stored thumb. `Preview` = an embedded preview (EXIF/HEIC/
/// RAW) stored at its native size, displayed upscaled-soft; `Full` = derived
/// from a real decode (the T0 byproduct) at up to the thumb edge. Upgrades are
/// monotonic: a `Full` is never replaced by a `Preview`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum ThumbTier {
    Preview,
    Full,
}

/// A resident thumbnail: actual pixel dimensions (entries record what they hold
/// — an embedded preview's size need not match the generated-thumb ceiling),
/// exact payload bytes, the monotonic generation stamped at insert (the shells'
/// per-entry change detector), and the payload itself.
#[derive(Debug)]
pub struct ThumbEntry<T> {
    pub tier: ThumbTier,
    pub w: u32,
    pub h: u32,
    pub bytes: u64,
    pub gen: u64,
    pub payload: T,
}

/// The demand window the eviction classes and the fill plan are computed
/// against: the shell-reported visible cell range, the overscan margin, and
/// the current item (whose ± `warm` neighborhood stays warm). All in playlist
/// indices; ranges are inclusive and clamped by the caller to the deck.
#[derive(Clone, Copy, Debug)]
pub struct ThumbDemand {
    pub visible: (usize, usize),
    pub overscan: (usize, usize),
    pub current: usize,
    pub warm: usize,
}

impl ThumbDemand {
    /// A demand centered on `current` before the shell has reported a viewport
    /// (panel just opened): visible ≈ nothing yet, overscan empty, warm active.
    pub fn centered(current: usize, warm: usize) -> Self {
        ThumbDemand {
            visible: (current, current),
            overscan: (current, current),
            current,
            warm,
        }
    }

    fn class(&self, item: usize) -> u8 {
        let inside = |(a, b): (usize, usize)| item >= a && item <= b;
        if inside(self.visible) {
            0 // pinned
        } else if inside(self.overscan) {
            1
        } else if item.abs_diff(self.current) <= self.warm {
            2
        } else {
            3
        }
    }
}

/// Byte-budgeted thumbnail residency, keyed by playlist index. See module docs.
pub struct ThumbCache<T> {
    entries: HashMap<usize, ThumbEntry<T>>,
    budget: u64,
    used: u64,
    /// Bumped on every mutation — the shells' cheap "anything changed?" probe.
    dirty: u64,
    /// Bumped on `clear` (deck rebuild / delete): results carrying an older deck
    /// generation are discarded by the caller before they ever reach `insert`.
    deck_gen: u64,
    /// Monotonic per-entry generation source (stamped on insert).
    next_gen: u64,
}

/// What `insert` did with the offered thumb.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InsertOutcome {
    /// Stored (evicting lower-class entries if needed).
    Stored,
    /// Dropped: a same-or-better tier is already resident for this item.
    AlreadyBetter,
    /// Dropped: storing it would evict only entries of an equal-or-more
    /// protected class (the cache is full of things we want more).
    NoRoom,
}

impl<T> ThumbCache<T> {
    pub fn new(budget: u64) -> Self {
        ThumbCache {
            entries: HashMap::new(),
            budget: budget.max(1),
            used: 0,
            dirty: 0,
            deck_gen: 0,
            next_gen: 0,
        }
    }

    pub fn budget(&self) -> u64 {
        self.budget
    }

    pub fn used_bytes(&self) -> u64 {
        self.used
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The shells' change probe: bumped on every insert/evict/clear.
    pub fn dirty(&self) -> u64 {
        self.dirty
    }

    /// The deck generation this cache's indices belong to. The orchestration
    /// layer stamps it on every derive/fill request and discards results whose
    /// generation no longer matches (plan §6).
    pub fn deck_gen(&self) -> u64 {
        self.deck_gen
    }

    pub fn get(&self, item: usize) -> Option<&ThumbEntry<T>> {
        self.entries.get(&item)
    }

    pub fn tier(&self, item: usize) -> Option<ThumbTier> {
        self.entries.get(&item).map(|e| e.tier)
    }

    /// Drop everything and bump the deck generation — the delete / rebuild rule
    /// (plan §6: clearing is the v1 simplification; indices were reassigned).
    /// Append-only growth (`extend_playlist`) must NOT call this — indices are
    /// stable there and the cache carries over.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.used = 0;
        self.deck_gen += 1;
        self.dirty += 1;
    }

    /// Offer a thumb for `item`. Tier upgrades are monotonic; a same-tier offer
    /// replaces (a re-derive after rotation-save carries fresher pixels). Evicts
    /// strictly lower-protection entries to make room; refuses rather than evict
    /// an equal-or-better class (so churn can't thrash the pinned set).
    #[allow(clippy::too_many_arguments)] // primitives, each self-describing
    pub fn insert(
        &mut self,
        item: usize,
        tier: ThumbTier,
        w: u32,
        h: u32,
        bytes: u64,
        payload: T,
        demand: &ThumbDemand,
    ) -> InsertOutcome {
        if let Some(existing) = self.entries.get(&item) {
            if existing.tier > tier {
                return InsertOutcome::AlreadyBetter;
            }
        }
        // Never admit an entry that alone exceeds the whole budget.
        if bytes > self.budget {
            return InsertOutcome::NoRoom;
        }
        let replaced: u64 = self.entries.get(&item).map(|e| e.bytes).unwrap_or(0);
        let need = (self.used - replaced + bytes).saturating_sub(self.budget);
        if need > 0 && !self.evict(need, demand.class(item), item, demand) {
            return InsertOutcome::NoRoom;
        }
        if let Some(old) = self.entries.remove(&item) {
            self.used -= old.bytes;
        }
        self.next_gen += 1;
        self.entries.insert(
            item,
            ThumbEntry {
                tier,
                w,
                h,
                bytes,
                gen: self.next_gen,
                payload,
            },
        );
        self.used += bytes;
        self.dirty += 1;
        InsertOutcome::Stored
    }

    /// Evict at least `need` bytes using only entries whose protection class is
    /// strictly greater (less protected) than `incoming_class` — or, within the
    /// same class, strictly farther from current than the incoming item (the
    /// deterministic nearest-visible rule when the visible set itself overflows).
    /// Returns false (having evicted nothing) if that can't free enough.
    fn evict(
        &mut self,
        need: u64,
        incoming_class: u8,
        incoming_item: usize,
        demand: &ThumbDemand,
    ) -> bool {
        let incoming_dist = incoming_item.abs_diff(demand.current);
        let mut victims: Vec<(u8, usize, usize)> = self
            .entries
            .keys()
            .filter_map(|&it| {
                let class = demand.class(it);
                let dist = it.abs_diff(demand.current);
                ((class, dist) > (incoming_class, incoming_dist)).then_some((class, dist, it))
            })
            .collect();
        // Least protected + farthest first.
        victims.sort_unstable_by(|a, b| b.cmp(a));
        let freeable: u64 = victims
            .iter()
            .map(|&(_, _, it)| self.entries[&it].bytes)
            .sum();
        if freeable < need {
            return false;
        }
        let mut freed = 0u64;
        for (_, _, it) in victims {
            if freed >= need {
                break;
            }
            if let Some(e) = self.entries.remove(&it) {
                self.used -= e.bytes;
                freed += e.bytes;
                self.dirty += 1;
            }
        }
        true
    }

    /// Trim to budget against a new demand window (called when the window moves:
    /// scroll, navigation). Unlike insert-time eviction this may evict from any
    /// class, least-protected + farthest first — the demand move itself is what
    /// re-classed entries out of protection.
    pub fn rebalance(&mut self, demand: &ThumbDemand) {
        if self.used <= self.budget {
            return;
        }
        let mut all: Vec<(u8, usize, usize)> = self
            .entries
            .keys()
            .map(|&it| (demand.class(it), it.abs_diff(demand.current), it))
            .collect();
        all.sort_unstable_by(|a, b| b.cmp(a));
        for (_, _, it) in all {
            if self.used <= self.budget {
                break;
            }
            if let Some(e) = self.entries.remove(&it) {
                self.used -= e.bytes;
                self.dirty += 1;
            }
        }
    }

    /// The cold cells to fill next (plan: visible → overscan → warm, near-to-far
    /// within each class), capped at `max_jobs`. Only items with **no** entry —
    /// tier refinement is T2's job, not the fill plan's. `len` clamps to the deck.
    pub fn fill_plan(&self, demand: &ThumbDemand, len: usize, max_jobs: usize) -> Vec<usize> {
        if len == 0 || max_jobs == 0 {
            return Vec::new();
        }
        let clamp = |i: usize| i.min(len - 1);
        let mut plan: Vec<usize> = Vec::new();
        let push = |it: usize, plan: &mut Vec<usize>| {
            if !self.entries.contains_key(&it) && !plan.contains(&it) {
                plan.push(it);
            }
        };
        // Visible then overscan: walk outward from the range midpoint.
        for (a, b) in [demand.visible, demand.overscan] {
            let (a, b) = (clamp(a), clamp(b));
            let mid = a + (b - a) / 2;
            for d in 0..=(b - a) {
                for it in [mid.checked_sub(d), Some(mid + d)].into_iter().flatten() {
                    if it >= a && it <= b {
                        push(it, &mut plan);
                    }
                }
            }
        }
        // Warm window: outward from current.
        let cur = clamp(demand.current);
        for d in 0..=demand.warm {
            for it in [cur.checked_sub(d), Some(cur + d)].into_iter().flatten() {
                if it < len {
                    push(it, &mut plan);
                }
            }
        }
        plan.truncate(max_jobs);
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demand(visible: (usize, usize), overscan: (usize, usize), current: usize) -> ThumbDemand {
        ThumbDemand {
            visible,
            overscan,
            current,
            warm: 64,
        }
    }

    fn put(c: &mut ThumbCache<()>, item: usize, bytes: u64, d: &ThumbDemand) -> InsertOutcome {
        c.insert(item, ThumbTier::Full, 512, 341, bytes, (), d)
    }

    #[test]
    fn stores_and_accounts_exact_bytes() {
        let d = demand((0, 5), (0, 10), 0);
        let mut c = ThumbCache::new(1000);
        assert_eq!(put(&mut c, 1, 400, &d), InsertOutcome::Stored);
        assert_eq!(put(&mut c, 2, 400, &d), InsertOutcome::Stored);
        assert_eq!(c.used_bytes(), 800);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn budget_is_never_exceeded() {
        let d = demand((0, 2), (0, 4), 0);
        let mut c = ThumbCache::new(1000);
        for i in 0..50 {
            put(&mut c, i, 300, &d);
            assert!(c.used_bytes() <= c.budget(), "over budget after insert {i}");
        }
    }

    #[test]
    fn evicts_least_protected_farthest_first() {
        let d = demand((10, 12), (8, 14), 10);
        let mut c = ThumbCache::new(1000);
        put(&mut c, 10, 300, &d); // visible
        put(&mut c, 13, 300, &d); // overscan
        put(&mut c, 40, 300, &d); // warm (|40-10| <= 64)
                                  // Inserting another visible item must evict the *warm* one, not overscan.
        assert_eq!(put(&mut c, 11, 300, &d), InsertOutcome::Stored);
        assert!(c.get(40).is_none(), "warm evicted");
        assert!(c.get(10).is_some());
        assert!(c.get(13).is_some());
        assert!(c.get(11).is_some());
    }

    #[test]
    fn pinned_visible_entries_are_never_evicted_by_inserts() {
        let d = demand((0, 3), (0, 3), 0);
        let mut c = ThumbCache::new(1000);
        for i in 0..=3 {
            assert_eq!(put(&mut c, i, 250, &d), InsertOutcome::Stored);
        }
        // Cache is exactly full of pinned entries; a warm insert must be refused.
        assert_eq!(put(&mut c, 50, 250, &d), InsertOutcome::NoRoom);
        for i in 0..=3 {
            assert!(c.get(i).is_some(), "pinned {i} survived");
        }
    }

    #[test]
    fn within_class_nearer_to_current_wins() {
        // Visible range wider than budget: nearest-visible subset is retained.
        let d = demand((0, 9), (0, 9), 0);
        let mut c = ThumbCache::new(1000);
        for i in (0..10).rev() {
            put(&mut c, i, 250, &d); // farthest inserted first
        }
        // Only 4 fit; the nearest-to-current (0..=3) must be the survivors.
        assert_eq!(c.len(), 4);
        for i in 0..4 {
            assert!(c.get(i).is_some(), "near-visible {i} retained");
        }
    }

    #[test]
    fn tier_upgrade_is_monotonic() {
        let d = demand((0, 5), (0, 5), 0);
        let mut c: ThumbCache<u8> = ThumbCache::new(1000);
        c.insert(1, ThumbTier::Full, 512, 341, 100, 7, &d);
        let out = c.insert(1, ThumbTier::Preview, 160, 120, 50, 9, &d);
        assert_eq!(out, InsertOutcome::AlreadyBetter);
        assert_eq!(c.get(1).unwrap().payload, 7, "full pixels kept");
        // Preview → Full upgrades.
        c.insert(2, ThumbTier::Preview, 160, 120, 50, 1, &d);
        c.insert(2, ThumbTier::Full, 512, 341, 100, 2, &d);
        assert_eq!(c.get(2).unwrap().payload, 2);
        assert_eq!(c.get(2).unwrap().tier, ThumbTier::Full);
    }

    #[test]
    fn clear_bumps_deck_gen_and_empties() {
        let d = demand((0, 5), (0, 5), 0);
        let mut c = ThumbCache::new(1000);
        put(&mut c, 1, 100, &d);
        let g0 = c.deck_gen();
        c.clear();
        assert_eq!(c.len(), 0);
        assert_eq!(c.used_bytes(), 0);
        assert_eq!(c.deck_gen(), g0 + 1);
    }

    #[test]
    fn dirty_bumps_on_every_mutation() {
        let d = demand((0, 5), (0, 5), 0);
        let mut c = ThumbCache::new(1000);
        let d0 = c.dirty();
        put(&mut c, 1, 100, &d);
        assert!(c.dirty() > d0);
        let d1 = c.dirty();
        c.clear();
        assert!(c.dirty() > d1);
    }

    #[test]
    fn oversized_entry_is_refused_outright() {
        let d = demand((0, 5), (0, 5), 0);
        let mut c = ThumbCache::new(1000);
        assert_eq!(put(&mut c, 1, 2000, &d), InsertOutcome::NoRoom);
        assert_eq!(c.used_bytes(), 0);
    }

    #[test]
    fn rebalance_trims_after_demand_moves() {
        let d0 = demand((0, 3), (0, 3), 0);
        let mut c = ThumbCache::new(1000);
        for i in 0..4 {
            put(&mut c, i, 250, &d0);
        }
        // Demand jumps far away with a tiny warm window: everything is class-3 now.
        let d1 = ThumbDemand {
            visible: (500, 503),
            overscan: (500, 503),
            current: 500,
            warm: 4,
        };
        // Insert a visible entry at the new location — evicts old far entries.
        assert_eq!(put(&mut c, 500, 250, &d1), InsertOutcome::Stored);
        assert!(c.used_bytes() <= c.budget());
        assert!(c.get(500).is_some());
        // Shrink the budget scenario: rebalance drops the least protected first.
        c.rebalance(&d1);
        assert!(c.used_bytes() <= c.budget());
    }

    #[test]
    fn fill_plan_orders_visible_overscan_warm_near_to_far() {
        let d = ThumbDemand {
            visible: (10, 12),
            overscan: (8, 14),
            current: 11,
            warm: 3,
        };
        let c: ThumbCache<()> = ThumbCache::new(1000);
        let plan = c.fill_plan(&d, 100, 64);
        // Visible first (11 is the midpoint), then overscan remainder, then warm.
        assert_eq!(&plan[..3], &[11, 10, 12], "visible near-to-far");
        assert!(
            plan[3..].starts_with(&[9, 13, 8, 14]),
            "overscan next: {plan:?}"
        );
        // Warm ±3 around 11 → nothing new (8..=14 already covers it).
        assert_eq!(plan.len(), 7);
    }

    #[test]
    fn fill_plan_skips_resident_and_respects_cap() {
        let d = ThumbDemand {
            visible: (0, 4),
            overscan: (0, 6),
            current: 2,
            warm: 0,
        };
        let mut c: ThumbCache<()> = ThumbCache::new(10_000);
        put(&mut c, 2, 10, &d);
        put(&mut c, 3, 10, &d);
        let plan = c.fill_plan(&d, 100, 3);
        assert_eq!(plan.len(), 3);
        assert!(!plan.contains(&2) && !plan.contains(&3), "resident skipped");
    }

    #[test]
    fn fill_plan_clamps_to_deck_and_handles_empty() {
        let d = ThumbDemand {
            visible: (95, 120),
            overscan: (90, 130),
            current: 98,
            warm: 5,
        };
        let c: ThumbCache<()> = ThumbCache::new(1000);
        let plan = c.fill_plan(&d, 100, 64);
        assert!(plan.iter().all(|&i| i < 100), "clamped: {plan:?}");
        assert!(c.fill_plan(&d, 0, 64).is_empty());
        assert!(c.fill_plan(&d, 100, 0).is_empty());
    }

    // Property: no interleaving of inserts/rebalances ever exceeds the budget,
    // and pinned (visible) entries survive any insert.
    proptest::proptest! {
        #[test]
        fn budget_and_pin_invariants(
            ops in proptest::collection::vec((0usize..200, 50u64..400), 1..120),
            vis_lo in 0usize..50,
        ) {
            let d = ThumbDemand {
                visible: (vis_lo, vis_lo + 5),
                overscan: (vis_lo.saturating_sub(5), vis_lo + 10),
                current: vis_lo + 2,
                warm: 20,
            };
            let mut c: ThumbCache<()> = ThumbCache::new(2000);
            for (item, bytes) in ops {
                let visible_before: Vec<usize> = (vis_lo..=vis_lo + 5)
                    .filter(|i| c.get(*i).is_some())
                    .collect();
                let out = c.insert(item, ThumbTier::Full, 512, 341, bytes, (), &d);
                proptest::prop_assert!(c.used_bytes() <= c.budget());
                // An insert never evicts a pinned entry (unless the incoming item
                // is itself pinned and nearer — the deterministic subset rule).
                if d.class(item) != 0 {
                    for v in visible_before {
                        proptest::prop_assert!(
                            c.get(v).is_some(),
                            "pinned {v} evicted by non-visible insert {item} ({out:?})"
                        );
                    }
                }
            }
            c.rebalance(&d);
            proptest::prop_assert!(c.used_bytes() <= c.budget());
        }
    }
}
