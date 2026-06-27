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

/// Tracks item↔slot residency for a fixed-capacity texture ring.
#[derive(Debug, Clone)]
pub struct ResidentRing {
    slots: Vec<SlotState>,
    /// item -> slot, for every Pending or Resident item (keeps lookups O(1)).
    by_item: HashMap<usize, usize>,
    /// The on-screen slot, pinned so it is never an eviction victim.
    displayed: Option<usize>,
}

impl ResidentRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: vec![SlotState::Empty; capacity],
            by_item: HashMap::new(),
            displayed: None,
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
        self.by_item.clear();
        self.displayed = None;
    }

    /// Choose and reserve a slot to upload `item` into, marking it `Pending`.
    ///
    /// `keep` is the prioritized target list (index 0 = highest priority). Victim
    /// preference: an Empty slot, else an occupied slot whose item is *not* in
    /// `keep`, else the lowest-priority `keep` item — but only if it is strictly
    /// lower priority than `item` (so we never evict something more important).
    /// The displayed slot is never a victim. Returns `None` when `item` is already
    /// tracked, doesn't belong resident (rank ≥ capacity), or nothing is freeable.
    pub fn reserve(&mut self, item: usize, epoch: u64, keep: &[usize]) -> Option<Reservation> {
        let cap = self.slots.len();
        if cap == 0 || self.by_item.contains_key(&item) {
            return None;
        }
        let rank_of = |it: usize| keep.iter().position(|&k| k == it);
        let item_rank = rank_of(item);
        // A target ranked beyond the ring's capacity doesn't belong resident.
        if let Some(r) = item_rank {
            if r >= cap {
                return None;
            }
        }

        // Pick the best victim. Score tiers (higher = better victim):
        //   2 = Empty, 1 = occupied but not in `keep`, 0 = occupied and wanted.
        // Within tier 0, a higher `keep` rank (lower priority) is the better victim.
        let mut best: Option<(u8, usize, usize)> = None; // (tier, rank, slot)
        for (slot, state) in self.slots.iter().enumerate() {
            if Some(slot) == self.displayed {
                continue;
            }
            let (tier, rank) = match state {
                SlotState::Empty => (2u8, usize::MAX),
                SlotState::Pending { item: it, .. } | SlotState::Resident { item: it } => {
                    match rank_of(*it) {
                        None => (1, usize::MAX),
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
        // When every freeable slot holds a wanted item, only evict one strictly
        // lower priority than the incoming item.
        if tier == 0 {
            match item_rank {
                Some(ir) if rank > ir => {}
                _ => return None,
            }
        }

        if let SlotState::Pending { item: old, .. } | SlotState::Resident { item: old } =
            self.slots[slot]
        {
            self.by_item.remove(&old);
        }
        self.slots[slot] = SlotState::Pending { item, epoch };
        self.by_item.insert(item, slot);
        Some(Reservation { item, slot, epoch })
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
