//! Resident-ring slot bookkeeping (plan §3.3).
//!
//! The renderer owns N reusable GPU texture slots; this is the **pure mirror**
//! that decides which item lives in which slot, picks eviction victims, and
//! guards against stale asynchronous decodes corrupting residency. No GPU, no
//! I/O — so the policy that keeps the cache warm is exhaustively testable.
//!
//! Three states + an epoch are the anti-staleness spine (plan §3.0): a slot is
//! `Pending` from the moment it is chosen as a victim, so a second reservation
//! in the same drain tick can't grab it; `mark_resident` only commits when the
//! completing upload's `(item, epoch)` still matches the reservation, so a result
//! decoded for an old geometry (after a resize / fit toggle) is rejected.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Empty,
    Pending { item: usize, epoch: u64 },
    Resident { item: usize },
}

/// A decision to upload `item` into ring slot `slot`, stamped with the `epoch`
/// it was planned for (so a stale completion can be rejected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    pub item: usize,
    pub slot: usize,
    pub epoch: u64,
}

/// Tracks item↔slot residency for a texture ring, bounded by both a slot count
/// and a VRAM byte budget (so full-res Original-mode slots can't OOM a mixed
/// folder even though they're far larger than fit slots).
#[derive(Debug, Clone)]
pub struct ResidentRing {
    slots: Vec<SlotState>,
    /// Committed VRAM bytes per slot (0 when Empty); a Pending/Resident slot holds
    /// its decoded image's byte size so total residency can be bounded.
    slot_bytes: Vec<u64>,
    /// item -> slot, for every Pending or Resident item (keeps lookups O(1)).
    by_item: HashMap<usize, usize>,
    /// The on-screen slot, pinned so it is never an eviction victim.
    displayed: Option<usize>,
    /// Hard VRAM cap: reserving evicts until `committed_bytes + new ≤ this`.
    byte_budget: u64,
    /// Sum of `slot_bytes` over Pending + Resident slots.
    committed_bytes: u64,
}

impl ResidentRing {
    /// A count-only ring (no byte limit). Convenient for pure nav/cache tests.
    pub fn new(capacity: usize) -> Self {
        Self::new_with_budget(capacity, u64::MAX)
    }

    /// A ring bounded by `capacity` slots *and* `byte_budget` resident bytes.
    pub fn new_with_budget(capacity: usize, byte_budget: u64) -> Self {
        Self {
            slots: vec![SlotState::Empty; capacity],
            slot_bytes: vec![0; capacity],
            by_item: HashMap::new(),
            displayed: None,
            byte_budget: byte_budget.max(1),
            committed_bytes: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// The slot holding `item` as a fully-uploaded (Resident) texture, if any.
    /// This is the keypress hit-test: `Some` means "rebind, don't decode".
    pub fn slot_for(&self, item: usize) -> Option<usize> {
        match self.by_item.get(&item) {
            Some(&s) if matches!(self.slots[s], SlotState::Resident { .. }) => Some(s),
            _ => None,
        }
    }

    /// Whether `item` is already resident or has an in-flight reservation.
    pub fn is_tracked(&self, item: usize) -> bool {
        self.by_item.contains_key(&item)
    }

    /// Pin the on-screen slot so it is never chosen as an eviction victim.
    pub fn set_displayed(&mut self, slot: usize) {
        debug_assert!(slot < self.slots.len());
        self.displayed = Some(slot);
    }

    /// Reset all residency. Called on an epoch change (resize / fit toggle): every
    /// in-flight or resident texture was decoded for the old geometry.
    pub fn clear(&mut self) {
        for s in &mut self.slots {
            *s = SlotState::Empty;
        }
        for b in &mut self.slot_bytes {
            *b = 0;
        }
        self.by_item.clear();
        self.displayed = None;
        self.committed_bytes = 0;
    }

    /// Choose and reserve a slot for `item` with no byte accounting (count-only).
    pub fn reserve(&mut self, item: usize, epoch: u64, keep: &[usize]) -> Option<Reservation> {
        self.reserve_bytes(item, epoch, 0, keep)
    }

    /// Choose and reserve a slot to upload `item` (`item_bytes` of VRAM) into,
    /// marking it `Pending`.
    ///
    /// `keep` is the prioritized target list (index 0 = highest priority). Evicts
    /// the lowest-priority victims — an item not in `keep`, else a `keep` item
    /// strictly lower priority than `item` — until there is a free slot **and**
    /// `committed_bytes + item_bytes ≤ byte_budget`. The displayed slot is never a
    /// victim. A single item larger than the whole budget is still allowed once
    /// nothing else is resident (so the current photo always shows). Returns `None`
    /// when `item` is already tracked, doesn't belong resident (rank ≥ capacity),
    /// or nothing lower-priority can be freed to make room.
    pub fn reserve_bytes(
        &mut self,
        item: usize,
        epoch: u64,
        item_bytes: u64,
        keep: &[usize],
    ) -> Option<Reservation> {
        let cap = self.slots.len();
        if cap == 0 || self.by_item.contains_key(&item) {
            return None;
        }
        let rank_of = |it: usize| keep.iter().position(|&k| k == it);
        // Only reserve items still wanted (present in `keep`): a decode that
        // finished after navigation moved on must not consume a slot ahead of the
        // current targets.
        let item_rank = rank_of(item)?;
        // A target ranked beyond the ring's capacity doesn't belong resident.
        if item_rank >= cap {
            return None;
        }

        loop {
            let free = self
                .slots
                .iter()
                .position(|s| matches!(s, SlotState::Empty));
            if let Some(slot) = free {
                // Reserve once it fits the budget, or once nothing else is resident
                // (a single over-budget image must still be showable).
                if self.committed_bytes + item_bytes <= self.byte_budget
                    || self.committed_bytes == 0
                {
                    self.slots[slot] = SlotState::Pending { item, epoch };
                    self.slot_bytes[slot] = item_bytes;
                    self.committed_bytes += item_bytes;
                    self.by_item.insert(item, slot);
                    return Some(Reservation { item, slot, epoch });
                }
            }
            match self.pick_victim(&rank_of, item_rank) {
                Some(slot) => self.free_slot(slot),
                None => return None,
            }
        }
    }

    /// The lowest-priority evictable occupied slot, or `None` when every candidate
    /// is the displayed slot or not strictly lower priority than `item_rank`.
    /// Score tiers (higher = better victim): 1 = occupied but not in `keep`,
    /// 0 = occupied and wanted (higher `keep` rank within tier 0 is a better victim).
    fn pick_victim<F: Fn(usize) -> Option<usize>>(
        &self,
        rank_of: &F,
        item_rank: usize,
    ) -> Option<usize> {
        let mut best: Option<(u8, usize, usize)> = None; // (tier, rank, slot)
        for (slot, state) in self.slots.iter().enumerate() {
            if Some(slot) == self.displayed {
                continue;
            }
            let (tier, rank) = match state {
                SlotState::Empty => continue,
                SlotState::Pending { item: it, .. } | SlotState::Resident { item: it } => {
                    match rank_of(*it) {
                        None => (1u8, usize::MAX),
                        Some(r) => (0, r),
                    }
                }
            };
            let cand = (tier, rank, slot);
            let better = match best {
                None => true,
                Some(b) => (cand.0, cand.1) > (b.0, b.1),
            };
            if better {
                best = Some(cand);
            }
        }
        let (tier, rank, slot) = best?;
        if tier == 0 && rank <= item_rank {
            return None; // would evict an equal-or-higher-priority item
        }
        Some(slot)
    }

    /// Empty a slot, releasing its item mapping and committed bytes.
    fn free_slot(&mut self, slot: usize) {
        if let SlotState::Pending { item: old, .. } | SlotState::Resident { item: old } =
            self.slots[slot]
        {
            self.by_item.remove(&old);
        }
        self.committed_bytes -= self.slot_bytes[slot];
        self.slot_bytes[slot] = 0;
        self.slots[slot] = SlotState::Empty;
    }

    /// Commit a completed upload: `Pending(item, epoch)` → `Resident(item)`.
    /// Returns `false` if the reservation is stale (the slot was reused or the
    /// epoch advanced), in which case the caller drops the decoded result.
    pub fn mark_resident(&mut self, item: usize, slot: usize, epoch: u64) -> bool {
        if slot >= self.slots.len() {
            return false;
        }
        match self.slots[slot] {
            SlotState::Pending { item: it, epoch: e } if it == item && e == epoch => {
                self.slots[slot] = SlotState::Resident { item };
                self.by_item.insert(item, slot);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::SplitMix64;

    fn resident_items(r: &ResidentRing) -> Vec<usize> {
        r.slots
            .iter()
            .filter_map(|s| match s {
                SlotState::Resident { item } => Some(*item),
                _ => None,
            })
            .collect()
    }

    fn occupied_count(r: &ResidentRing) -> usize {
        r.slots
            .iter()
            .filter(|s| !matches!(s, SlotState::Empty))
            .count()
    }

    #[test]
    fn new_ring_is_empty() {
        let r = ResidentRing::new(4);
        assert_eq!(r.capacity(), 4);
        assert_eq!(r.slot_for(0), None);
        assert!(!r.is_tracked(0));
    }

    #[test]
    fn reserve_then_mark_makes_resident() {
        let mut r = ResidentRing::new(4);
        let res = r.reserve(7, 1, &[7]).expect("a slot");
        // Pending, not yet resident.
        assert_eq!(r.slot_for(7), None);
        assert!(r.is_tracked(7));
        assert!(r.mark_resident(res.item, res.slot, res.epoch));
        assert_eq!(r.slot_for(7), Some(res.slot));
    }

    #[test]
    fn reserving_a_tracked_item_is_a_noop() {
        let mut r = ResidentRing::new(4);
        let res = r.reserve(7, 1, &[7]).unwrap();
        r.mark_resident(res.item, res.slot, res.epoch);
        assert_eq!(r.reserve(7, 1, &[7]), None);
        // ...and while only Pending too.
        r.reserve(8, 1, &[7, 8]).unwrap();
        assert_eq!(r.reserve(8, 1, &[7, 8]), None);
    }

    #[test]
    fn capacity_is_never_exceeded() {
        let mut r = ResidentRing::new(3);
        // Try to make 10 distinct items resident; keep wants them all (in order).
        let keep: Vec<usize> = (0..10).collect();
        for item in 0..10 {
            if let Some(res) = r.reserve(item, 1, &keep) {
                r.mark_resident(res.item, res.slot, res.epoch);
            }
        }
        assert!(occupied_count(&r) <= 3);
        assert!(resident_items(&r).len() <= 3);
    }

    #[test]
    fn eviction_prefers_items_not_in_keep() {
        let mut r = ResidentRing::new(2);
        // A and B resident.
        let a = r.reserve(10, 1, &[10]).unwrap();
        r.mark_resident(a.item, a.slot, a.epoch);
        let b = r.reserve(20, 1, &[10, 20]).unwrap();
        r.mark_resident(b.item, b.slot, b.epoch);
        // Now want C (top priority) and A; B is no longer wanted -> B is the victim.
        let keep = [30, 10];
        let c = r.reserve(30, 1, &keep).unwrap();
        r.mark_resident(c.item, c.slot, c.epoch);
        assert_eq!(
            r.slot_for(20),
            None,
            "B (not in keep) should have been evicted"
        );
        assert_eq!(
            r.slot_for(10),
            Some(a.slot),
            "A (in keep) should be retained"
        );
        assert!(r.slot_for(30).is_some());
    }

    #[test]
    fn displayed_slot_is_never_evicted() {
        let mut r = ResidentRing::new(2);
        let a = r.reserve(10, 1, &[10]).unwrap();
        r.mark_resident(a.item, a.slot, a.epoch);
        r.set_displayed(a.slot);
        // Fill the other slot, then force pressure with items that don't keep A.
        for item in [20usize, 30, 40, 50] {
            let keep = [item, 999]; // A (10) is not in keep -> would be a victim
            if let Some(res) = r.reserve(item, 1, &keep) {
                assert_ne!(res.slot, a.slot, "must not reserve the displayed slot");
                r.mark_resident(res.item, res.slot, res.epoch);
            }
        }
        assert_eq!(
            r.slot_for(10),
            Some(a.slot),
            "displayed A survived eviction"
        );
    }

    #[test]
    fn stale_epoch_completion_is_rejected() {
        let mut r = ResidentRing::new(2);
        let res = r.reserve(10, 1, &[10]).unwrap();
        // A completion stamped with a newer epoch (geometry changed) is dropped.
        assert!(!r.mark_resident(10, res.slot, 2));
        assert_eq!(r.slot_for(10), None);
        // The correct-epoch completion still commits.
        assert!(r.mark_resident(10, res.slot, 1));
        assert_eq!(r.slot_for(10), Some(res.slot));
    }

    #[test]
    fn low_priority_item_does_not_evict_higher_priority() {
        let mut r = ResidentRing::new(1);
        let a = r.reserve(10, 1, &[10]).unwrap();
        r.mark_resident(a.item, a.slot, a.epoch);
        // B is rank 1 with capacity 1 -> doesn't belong resident; A must stay.
        assert_eq!(r.reserve(20, 1, &[10, 20]), None);
        assert_eq!(r.slot_for(10), Some(a.slot));
    }

    #[test]
    fn reserve_rejects_an_item_not_in_keep() {
        let mut r = ResidentRing::new(4);
        // A decode that finished after navigation moved on: item 99 isn't wanted.
        assert_eq!(r.reserve(99, 1, &[1, 2, 3]), None);
        assert!(!r.is_tracked(99));
    }

    #[test]
    fn byte_budget_bounds_residency() {
        // 8 slots but a 250-byte budget: only ~2 of the 100-byte items fit, so a
        // mixed folder of big full-res images can't blow past the VRAM budget.
        let mut r = ResidentRing::new_with_budget(8, 250);
        for item in [10usize, 11] {
            let res = r.reserve_bytes(item, 1, 100, &[10, 11, 12]).unwrap();
            r.mark_resident(item, res.slot, 1);
        }
        // committed = 200; a third 100-byte item (200+100 > 250) evicts the lowest.
        let keep = [12usize, 11, 10]; // 12 highest priority now
        let c = r.reserve_bytes(12, 1, 100, &keep).unwrap();
        r.mark_resident(12, c.slot, 1);
        let resident = [10, 11, 12]
            .iter()
            .filter(|&&i| r.slot_for(i).is_some())
            .count();
        assert_eq!(resident, 2, "byte budget caps residency at 2 of 3");
        assert!(
            r.slot_for(12).is_some(),
            "the top-priority item stays resident"
        );
    }

    #[test]
    fn single_item_larger_than_budget_still_reserves() {
        let mut r = ResidentRing::new_with_budget(4, 100);
        // 500 > budget, but with nothing else resident the current photo must show.
        assert!(r.reserve_bytes(7, 1, 500, &[7]).is_some());
    }

    #[test]
    fn randomized_byte_budget_is_respected() {
        let mut rng = SplitMix64::new(0xB16B_00B5);
        let cap = 16;
        let budget = 1000u64;
        let n_items = 30u64;
        let mut ring = ResidentRing::new_with_budget(cap, budget);
        for _ in 0..20_000 {
            let mut keep = Vec::new();
            while keep.len() < cap {
                let it = rng.next_bounded(n_items) as usize;
                if !keep.contains(&it) {
                    keep.push(it);
                }
            }
            let item = keep[rng.next_bounded(keep.len() as u64) as usize];
            // Each item is well under budget, so committed must never exceed it.
            let bytes = 1 + rng.next_bounded(budget / 4);
            if let Some(res) = ring.reserve_bytes(item, 1, bytes, &keep) {
                if rng.next_bounded(4) != 0 {
                    ring.mark_resident(res.item, res.slot, 1);
                }
            }
            let sum: u64 = ring.slot_bytes.iter().sum();
            assert_eq!(
                ring.committed_bytes, sum,
                "committed out of sync with slots"
            );
            assert!(
                ring.committed_bytes <= budget,
                "budget exceeded: {} > {budget}",
                ring.committed_bytes
            );
        }
    }

    #[test]
    fn clear_resets_everything() {
        let mut r = ResidentRing::new(3);
        let a = r.reserve(1, 1, &[1]).unwrap();
        r.mark_resident(a.item, a.slot, a.epoch);
        r.set_displayed(a.slot);
        r.clear();
        assert_eq!(occupied_count(&r), 0);
        assert_eq!(r.slot_for(1), None);
        assert!(!r.is_tracked(1));
        // After clear, the (previously displayed) slot is freely reservable again.
        assert!(r.reserve(1, 2, &[1]).is_some());
    }

    // Randomized stress: hammer the ring with reservations, completions, displayed
    // changes and clears; the core invariants must hold at every step. Uses the
    // crate's own deterministic PRNG so pb-core stays dependency-free.
    #[test]
    fn randomized_invariants_hold() {
        let mut rng = SplitMix64::new(0x9E37_79B9_7F4A_7C15);
        let cap = 8;
        let n_items = 40u64;
        let mut ring = ResidentRing::new(cap);
        let mut epoch = 1u64;

        for _ in 0..20_000 {
            // A random prioritized target window (a prefix of distinct items).
            let mut keep = Vec::new();
            while keep.len() < cap {
                let it = rng.next_bounded(n_items) as usize;
                if !keep.contains(&it) {
                    keep.push(it);
                }
            }
            let item = keep[rng.next_bounded(keep.len() as u64) as usize];

            match rng.next_bounded(10) {
                0 => {
                    ring.clear();
                    epoch += 1;
                }
                1 => {
                    // Pin a random occupied slot as displayed, if any.
                    if let Some(slot) =
                        (0..cap).find(|&s| !matches!(ring.slots[s], SlotState::Empty))
                    {
                        ring.set_displayed(slot);
                    }
                }
                _ => {
                    if let Some(res) = ring.reserve(item, epoch, &keep) {
                        // Usually complete promptly; sometimes leave it Pending,
                        // and sometimes complete with a stale epoch (must reject).
                        match rng.next_bounded(4) {
                            0 => {}
                            1 => {
                                let _ =
                                    ring.mark_resident(res.item, res.slot, epoch.wrapping_sub(1));
                            }
                            _ => {
                                assert!(ring.mark_resident(res.item, res.slot, res.epoch));
                            }
                        }
                    }
                }
            }

            // Invariant 1: never more occupied slots than capacity.
            assert!(occupied_count(&ring) <= cap);
            // Invariant 2: no item occupies two slots; by_item is consistent.
            let mut seen = std::collections::HashSet::new();
            for (slot, state) in ring.slots.iter().enumerate() {
                let it = match state {
                    SlotState::Empty => continue,
                    SlotState::Pending { item, .. } | SlotState::Resident { item } => *item,
                };
                assert!(seen.insert(it), "item {it} in two slots");
                assert_eq!(ring.by_item.get(&it), Some(&slot), "by_item out of sync");
            }
            // Invariant 3: by_item has exactly one entry per occupied slot.
            assert_eq!(ring.by_item.len(), occupied_count(&ring));
        }
    }
}
