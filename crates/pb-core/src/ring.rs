//! Resident-ring slot bookkeeping (plan §3.3; typed representation #106.7).
//!
//! The renderer owns N reusable GPU texture slots; this is the **pure mirror**
//! that decides which item+representation lives in which slot, picks eviction
//! victims, and guards against stale asynchronous decodes corrupting residency.
//! No GPU, no I/O — so the policy that keeps the cache warm is exhaustively
//! testable.
//!
//! **Typed representation (#106.7).** A slot holds an item in a specific
//! [`Representation`]: a decode-to-`Fit` texture (sized for the viewport at a
//! geometry epoch) or the full-resolution `Original` (geometry-independent). An
//! item can be resident in **both** at once — a parked photo keeps its Lanczos
//! `Fit` for the current view *and* its `Original` so a Fit↔1:1 toggle is an
//! instant rebind, never a re-decode. Residency is therefore keyed by
//! `(item, RepKind)`, and a slot carries `{ item, content_gen, representation }`.
//!
//! **Two anti-staleness spines.** A slot is `Pending` from the moment it is
//! chosen as a victim, so a second reservation in the same drain tick can't grab
//! it; `mark_resident` only commits when the completing upload's
//! `(item, content_gen, representation)` still matches the reservation. That
//! rejects both a result decoded for an **old geometry** (the `Fit`'s
//! `geometry_epoch` no longer matches after a resize / fit toggle) and one
//! decoded for **old content** (the `content_gen` advanced because index N now
//! names different pixels — a deck rebuild, a saved rotation, a delete).
//!
//! **Retention.** A geometry change purges only `Fit` slots
//! ([`drop_fit_slots`](ResidentRing::drop_fit_slots)) and keeps valid
//! `Original`s; a content change purges everything ([`clear`](ResidentRing::clear)).

use std::collections::{HashMap, HashSet};

/// Which representation of an item's pixels a ring slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Representation {
    /// A decode-to-fit texture, sized for the viewport at `geometry_epoch`. Valid
    /// only at that epoch — a resize / Fit↔1:1 toggle bumps the epoch and makes it
    /// stale (the pixels were downscaled for the old viewport).
    Fit { geometry_epoch: u64 },
    /// The full-resolution texture. Geometry-independent: it survives a resize /
    /// scale-mode toggle (the whole point of #106.7 — a parked photo's 1:1 stays an
    /// instant rebind) and is invalidated only by a content change.
    Original,
}

/// The identity discriminant of a [`Representation`]. An item holds at most one
/// `Fit` slot and at most one `Original` slot, so residency is keyed by
/// `(item, RepKind)` — the `Fit`'s `geometry_epoch` is a *staleness* stamp, not
/// part of the identity (there is never more than one `Fit` for an item at a time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepKind {
    Fit,
    Original,
}

impl Representation {
    pub fn kind(self) -> RepKind {
        match self {
            Representation::Fit { .. } => RepKind::Fit,
            Representation::Original => RepKind::Original,
        }
    }

    pub fn is_original(self) -> bool {
        matches!(self, Representation::Original)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Empty,
    Pending {
        item: usize,
        content_gen: u64,
        rep: Representation,
    },
    Resident {
        item: usize,
        content_gen: u64,
        rep: Representation,
    },
}

impl SlotState {
    fn occupant(&self) -> Option<(usize, RepKind)> {
        match *self {
            SlotState::Empty => None,
            SlotState::Pending { item, rep, .. } | SlotState::Resident { item, rep, .. } => {
                Some((item, rep.kind()))
            }
        }
    }
}

/// A decision to upload `item` (in `representation`) into ring slot `slot`, stamped
/// with the `content_gen` it was planned for (so a stale completion can be rejected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    pub item: usize,
    pub slot: usize,
    pub content_gen: u64,
    pub representation: Representation,
}

/// The renderer's move instruction when a geometry change compacts the ring: the
/// surviving `Original` texture in `from` must be relocated to `to` in the rebuilt
/// slot vector (the ring may shrink when Original mode's larger slots change the
/// capacity). `from == to` is a valid no-move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotRemap {
    pub from: usize,
    pub to: usize,
    pub item: usize,
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
    /// (item, rep-kind) -> slot, for every Pending or Resident occupant (keeps
    /// lookups O(1)). An item may have both a `Fit` and an `Original` entry.
    by_key: HashMap<(usize, RepKind), usize>,
    /// The on-screen slot, pinned so it is never an eviction victim.
    displayed: Option<usize>,
    /// Hard VRAM cap: reserving evicts until `committed_bytes + new ≤ this`.
    byte_budget: u64,
    /// Sum of `slot_bytes` over Pending + Resident slots.
    committed_bytes: u64,
    /// Reservations this ring REFUSED (rank beyond the window, budget with nothing
    /// lower-priority evictable) since the last time its answer could have changed
    /// (task #122): a caller that re-requests a refused decode every scheduling pass
    /// burns a full native decode per pass — the fullscreen-park livelock. Cleared by
    /// [`clear`](Self::clear), [`compact_to`](Self::compact_to) (a rebuild resizes
    /// budget/capacity), and a [`set_displayed`](Self::set_displayed) *change* (nav
    /// re-ranks the keep list). Consulted via [`denied`](Self::denied); "already
    /// tracked" refusals are NOT recorded (they're success-adjacent, not denials).
    denied: HashSet<(usize, RepKind)>,
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
            by_key: HashMap::new(),
            displayed: None,
            byte_budget: byte_budget.max(1),
            committed_bytes: 0,
            denied: HashSet::new(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// The slot holding `item` in representation `kind` as a fully-uploaded
    /// (Resident) texture, if any. This is the keypress hit-test: `Some` means
    /// "rebind, don't decode".
    pub fn slot_for_rep(&self, item: usize, kind: RepKind) -> Option<usize> {
        match self.by_key.get(&(item, kind)) {
            Some(&s) if matches!(self.slots[s], SlotState::Resident { .. }) => Some(s),
            _ => None,
        }
    }

    /// The slot holding `item`'s **Fit** texture, resident. The common display
    /// query in Fit/Fill modes.
    pub fn slot_for(&self, item: usize) -> Option<usize> {
        self.slot_for_rep(item, RepKind::Fit)
    }

    /// The slot holding `item`'s resident **Original** (full-res) texture, if any.
    pub fn original_slot(&self, item: usize) -> Option<usize> {
        self.slot_for_rep(item, RepKind::Original)
    }

    /// Whether `item` is resident or has an in-flight reservation in **any**
    /// representation.
    pub fn is_tracked(&self, item: usize) -> bool {
        self.by_key.contains_key(&(item, RepKind::Fit))
            || self.by_key.contains_key(&(item, RepKind::Original))
    }

    /// Whether `item` is tracked in the given representation kind (resident or Pending).
    pub fn is_tracked_rep(&self, item: usize, kind: RepKind) -> bool {
        self.by_key.contains_key(&(item, kind))
    }

    /// Pin the on-screen slot so it is never chosen as an eviction victim. A *change*
    /// of displayed slot also voids past reservation refusals (#122): nav re-ranks the
    /// keep list, so a previously refused item may be admittable now.
    pub fn set_displayed(&mut self, slot: usize) {
        debug_assert!(slot < self.slots.len());
        if self.displayed != Some(slot) {
            self.denied.clear();
        }
        self.displayed = Some(slot);
    }

    /// Reset all residency. Called on a **content** change (deck rebuild, saved
    /// rotation, delete/undo, teardown): index N now names different pixels, so every
    /// in-flight or resident texture — Fit *and* Original — is invalid.
    pub fn clear(&mut self) {
        for s in &mut self.slots {
            *s = SlotState::Empty;
        }
        for b in &mut self.slot_bytes {
            *b = 0;
        }
        self.by_key.clear();
        self.displayed = None;
        self.committed_bytes = 0;
        self.denied.clear();
    }

    /// A **geometry** change (resize / Fit↔1:1 toggle): drop every `Fit` slot (their
    /// pixels were sized for the old viewport) but **keep** valid `Original` slots —
    /// they are geometry-independent, so a parked photo's 1:1 stays an instant rebind
    /// across the toggle (#106.7 §3). Returns the surviving Originals as
    /// `(item, slot)` so the caller can tell the renderer which textures to retain.
    /// The `displayed` pin is cleared (a new frame is about to be presented at the
    /// new epoch).
    pub fn drop_fit_slots(&mut self) -> Vec<(usize, usize)> {
        let mut kept = Vec::new();
        for slot in 0..self.slots.len() {
            match self.slots[slot] {
                SlotState::Empty => {}
                // Any Fit slot (Pending or Resident) is stale at the new epoch → free it.
                // A Pending Fit's completion would be rejected by content_gen/rep on
                // mark_resident anyway; freeing the slot now reclaims it immediately.
                SlotState::Pending { rep, .. } | SlotState::Resident { rep, .. }
                    if rep.kind() == RepKind::Fit =>
                {
                    self.committed_bytes =
                        self.committed_bytes.saturating_sub(self.slot_bytes[slot]);
                    self.slot_bytes[slot] = 0;
                    self.slots[slot] = SlotState::Empty;
                }
                // A resident Original survives and is a retainable texture → report it.
                SlotState::Resident { item, .. } => kept.push((item, slot)),
                // A Pending Original survives (its decode is still wanted) but is not yet
                // a texture, so it is not reported to the renderer.
                SlotState::Pending { .. } => {}
            }
        }
        // Rebuild by_key from the surviving occupants (Fit entries are gone).
        self.by_key.clear();
        for (slot, state) in self.slots.iter().enumerate() {
            if let Some((item, kind)) = state.occupant() {
                self.by_key.insert((item, kind), slot);
            }
        }
        self.displayed = None;
        kept
    }

    /// Compact the surviving occupants into a ring of `new_capacity` slots, preserving
    /// priority order (`keep`, index 0 = highest). Used when a geometry change shrinks
    /// the ring (Original mode's larger slots reduce the count): the retained
    /// `Original` textures must move to valid slot indices in the smaller vector.
    /// Returns the [`SlotRemap`]s (old slot → new slot) for the renderer to relocate
    /// its textures; occupants that no longer fit (rank ≥ new_capacity, or over budget)
    /// are dropped. Must be called *after* [`drop_fit_slots`], on a ring whose only
    /// occupants are `Resident` Originals.
    pub fn compact_to(&mut self, new_capacity: usize, keep: &[usize]) -> Vec<SlotRemap> {
        // A rebuild resizes capacity/budget occupancy — every past refusal is void (#122).
        self.denied.clear();
        // Gather survivors with their priority rank.
        let rank_of = |it: usize| keep.iter().position(|&k| k == it).unwrap_or(usize::MAX);
        let mut survivors: Vec<(usize, usize, u64)> = Vec::new(); // (old_slot, item, bytes)
        for (slot, state) in self.slots.iter().enumerate() {
            if let SlotState::Resident { item, .. } = *state {
                survivors.push((slot, item, self.slot_bytes[slot]));
            }
        }
        // Highest priority first; admit until capacity or budget is hit.
        survivors.sort_by_key(|&(_, item, _)| rank_of(item));

        let old_slots = std::mem::take(&mut self.slots);
        let old_bytes = std::mem::take(&mut self.slot_bytes);
        self.slots = vec![SlotState::Empty; new_capacity];
        self.slot_bytes = vec![0; new_capacity];
        self.by_key.clear();
        self.committed_bytes = 0;
        self.displayed = None;

        let mut remaps = Vec::new();
        let mut next = 0usize;
        for (old_slot, item, bytes) in survivors {
            if next >= new_capacity {
                break;
            }
            if rank_of(item) >= new_capacity {
                continue; // no longer belongs resident
            }
            if self.committed_bytes + bytes > self.byte_budget && self.committed_bytes != 0 {
                continue; // over budget; drop the lower-priority survivor
            }
            let to = next;
            next += 1;
            self.slots[to] = old_slots[old_slot];
            self.slot_bytes[to] = bytes;
            self.committed_bytes += bytes;
            self.by_key.insert((item, RepKind::Original), to);
            remaps.push(SlotRemap {
                from: old_slot,
                to,
                item,
            });
        }
        let _ = old_bytes;
        remaps
    }

    /// Choose and reserve a slot for `item` (in `representation`) with no byte
    /// accounting (count-only). `content_gen` is the current content generation.
    pub fn reserve(
        &mut self,
        item: usize,
        content_gen: u64,
        representation: Representation,
        keep: &[usize],
    ) -> Option<Reservation> {
        self.reserve_bytes(item, content_gen, representation, 0, keep)
    }

    /// Choose and reserve a slot to upload `item` (`item_bytes` of VRAM, in
    /// `representation`) into, marking it `Pending`.
    ///
    /// `keep` is the prioritized target list (index 0 = highest priority). Evicts
    /// the lowest-priority victims — an item not in `keep`, else a `keep` item
    /// strictly lower priority than `item` — until there is a free slot **and**
    /// `committed_bytes + item_bytes ≤ byte_budget`. The displayed slot is never a
    /// victim. A single item larger than the whole budget is still allowed once
    /// nothing else is resident (so the current photo always shows). Returns `None`
    /// when `item` is already tracked *in this representation*, doesn't belong
    /// resident (rank ≥ capacity), or nothing lower-priority can be freed to make room.
    pub fn reserve_bytes(
        &mut self,
        item: usize,
        content_gen: u64,
        representation: Representation,
        item_bytes: u64,
        keep: &[usize],
    ) -> Option<Reservation> {
        let cap = self.slots.len();
        let key = (item, representation.kind());
        if cap == 0 || self.by_key.contains_key(&key) {
            return None;
        }
        let rank_of = |it: usize| keep.iter().position(|&k| k == it);
        // Only reserve items still wanted (present in `keep`): a decode that
        // finished after navigation moved on must not consume a slot ahead of the
        // current targets. Every genuine refusal from here down is RECORDED (#122,
        // `denied`) so a scheduler can stop re-requesting a decode this ring cannot
        // admit until the ranks can change.
        let Some(item_rank) = rank_of(item) else {
            self.denied.insert(key);
            return None;
        };
        // A target ranked beyond the ring's capacity doesn't belong resident.
        if item_rank >= cap {
            self.denied.insert(key);
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
                    self.slots[slot] = SlotState::Pending {
                        item,
                        content_gen,
                        rep: representation,
                    };
                    self.slot_bytes[slot] = item_bytes;
                    self.committed_bytes += item_bytes;
                    self.by_key.insert(key, slot);
                    return Some(Reservation {
                        item,
                        slot,
                        content_gen,
                        representation,
                    });
                }
            }
            let Some(slot) = self.pick_victim(&rank_of, item_rank, representation.kind()) else {
                // Budget full with nothing lower-priority evictable — a genuine denial.
                self.denied.insert(key);
                return None;
            };
            self.free_slot(slot);
        }
    }

    /// Whether a reservation for `(item, kind)` was REFUSED since the last event that
    /// could change the answer (a rebuild via [`compact_to`](Self::compact_to)/
    /// [`clear`](Self::clear), or a displayed-slot change) — task #122's livelock guard:
    /// a scheduler consulting this stops re-requesting a decode whose landing this ring
    /// must refuse, instead of burning one full native decode per scheduling pass.
    pub fn denied(&self, item: usize, kind: RepKind) -> bool {
        self.denied.contains(&(item, kind))
    }

    /// Whether [`reserve_bytes`](Self::reserve_bytes) would succeed for
    /// `(item, kind, item_bytes)` **right now**, without mutating anything — the same
    /// rank / dedup / budget / eviction rules, evaluated as a dry run (#122: the parked
    /// full-res emitter pre-flights wants so a decode the ring must refuse is never
    /// requested in the first place). Feasibility mirrors the reserve loop: a slot is
    /// obtainable (free, or at least one evictable victim), and after evicting every
    /// evictable victim the bytes fit the budget (or nothing non-evictable remains —
    /// the single-over-budget rule). A victim is evictable per `pick_victim`'s terminal
    /// rules: never the displayed slot; unranked beats ranked; ranked only when
    /// strictly lower priority, or same-rank in a *different* representation.
    pub fn admittable(&self, item: usize, kind: RepKind, item_bytes: u64, keep: &[usize]) -> bool {
        let cap = self.slots.len();
        if cap == 0 || self.by_key.contains_key(&(item, kind)) {
            return false;
        }
        let rank_of = |it: usize| keep.iter().position(|&k| k == it);
        let Some(item_rank) = rank_of(item) else {
            return false;
        };
        if item_rank >= cap {
            return false;
        }
        let mut free = 0usize;
        let mut evictable = 0usize;
        let mut evictable_bytes = 0u64;
        for (slot, state) in self.slots.iter().enumerate() {
            let Some((occ_item, occ_kind)) = state.occupant() else {
                free += 1;
                continue;
            };
            if Some(slot) == self.displayed {
                continue;
            }
            let can_evict = match rank_of(occ_item) {
                None => true,
                Some(r) if r > item_rank => true,
                Some(r) if r == item_rank => occ_kind != kind,
                Some(_) => false,
            };
            if can_evict {
                evictable += 1;
                evictable_bytes += self.slot_bytes[slot];
            }
        }
        if free == 0 && evictable == 0 {
            return false;
        }
        let min_committed = self.committed_bytes.saturating_sub(evictable_bytes);
        min_committed + item_bytes <= self.byte_budget || min_committed == 0
    }

    /// The lowest-priority evictable occupied slot, or `None` when every candidate
    /// is the displayed slot or not strictly lower priority than `item_rank`.
    ///
    /// Score tiers (higher = better victim): a slot whose item is not in `keep`
    /// beats a wanted one; within the wanted tier, a higher `keep` rank (further from
    /// the cursor) is the better victim. A slot in the **same representation** as the
    /// incoming reservation and at the *same* item rank is a worse victim than a
    /// different-representation slot at that rank — but item rank dominates, so a
    /// low-priority Original is still freed for a high-priority Fit and vice-versa.
    fn pick_victim<F: Fn(usize) -> Option<usize>>(
        &self,
        rank_of: &F,
        item_rank: usize,
        incoming: RepKind,
    ) -> Option<usize> {
        let mut best: Option<(u8, usize, u8, usize)> = None; // (tier, rank, rep-tiebreak, slot)
        for (slot, state) in self.slots.iter().enumerate() {
            if Some(slot) == self.displayed {
                continue;
            }
            let (item, kind) = match state.occupant() {
                None => continue,
                Some(o) => o,
            };
            let (tier, rank) = match rank_of(item) {
                None => (1u8, usize::MAX),
                Some(r) => (0, r),
            };
            // Prefer evicting a slot of the *other* representation when ranks tie, so an
            // Original upgrade doesn't cannibalise the on-screen Fit of an equally-ranked
            // neighbour before a spare same-rank slot of its own kind.
            let rep_tiebreak = u8::from(kind != incoming);
            let cand = (tier, rank, rep_tiebreak, slot);
            let better = match best {
                None => true,
                Some(b) => (cand.0, cand.1, cand.2) > (b.0, b.1, b.2),
            };
            if better {
                best = Some(cand);
            }
        }
        let (tier, rank, _, slot) = best?;
        if tier == 0 && rank < item_rank {
            return None; // would evict a strictly-higher-priority item
        }
        if tier == 0 && rank == item_rank {
            // Same item priority: only evict if it's a *different* representation slot
            // (making room for this rep of the same/adjacent item is legitimate); never
            // evict the same rep at the same rank (that is this very item or a peer).
            let victim_kind = self.slots[slot].occupant().map(|(_, k)| k);
            if victim_kind == Some(incoming) {
                return None;
            }
        }
        Some(slot)
    }

    /// Empty a slot, releasing its occupant mapping and committed bytes.
    fn free_slot(&mut self, slot: usize) {
        if let Some((item, kind)) = self.slots[slot].occupant() {
            self.by_key.remove(&(item, kind));
        }
        self.committed_bytes -= self.slot_bytes[slot];
        self.slot_bytes[slot] = 0;
        self.slots[slot] = SlotState::Empty;
    }

    /// Evict lower-priority slots until the (item, kind) slot can grow to `need` bytes
    /// without the ring exceeding its budget, then report success. Used to make room for
    /// an **in-place upgrade** (preview→full) *before* the upload, so the ring is never
    /// left over-budget (#106.7 §7). Because the slot already holds `current` bytes, only
    /// the *growth* (`need - current`) must be freed. `item`'s own slots and the displayed
    /// slot are never victims. Returns whether it now fits (always true once nothing
    /// evictable remains and the item is effectively alone — the current photo must show,
    /// mirroring the single-over-budget rule).
    pub fn make_room_for_upgrade(
        &mut self,
        item: usize,
        kind: RepKind,
        need: u64,
        keep: &[usize],
    ) -> bool {
        let rank_of = |it: usize| keep.iter().position(|&k| k == it);
        let current = self
            .by_key
            .get(&(item, kind))
            .map(|&s| self.slot_bytes[s])
            .unwrap_or(0);
        loop {
            // Post-upgrade committed = committed - current + need. Evicting reduces
            // committed (never touching the item's own slot), so this converges.
            if self.committed_bytes.saturating_sub(current) + need <= self.byte_budget {
                return true;
            }
            // Find the lowest-priority evictable slot that is neither displayed nor a
            // slot of `item` itself.
            let mut best: Option<(u8, usize, usize)> = None;
            for (slot, state) in self.slots.iter().enumerate() {
                if Some(slot) == self.displayed {
                    continue;
                }
                let (it, _) = match state.occupant() {
                    None => continue,
                    Some(o) => o,
                };
                if it == item {
                    continue;
                }
                let (tier, rank) = match rank_of(it) {
                    None => (1u8, usize::MAX),
                    Some(r) => (0, r),
                };
                let cand = (tier, rank, slot);
                if best.is_none_or(|b| (cand.0, cand.1) > (b.0, b.1)) {
                    best = Some(cand);
                }
            }
            match best {
                Some((_, _, slot)) => self.free_slot(slot),
                // Nothing left to evict; admit anyway if the item is effectively alone
                // (committed is just its own other-rep slot or empty) — the current photo
                // must always show, mirroring `reserve_bytes`' single-over-budget rule.
                None => return true,
            }
        }
    }

    /// Adjust the recorded VRAM bytes of `item`'s slot in `kind` — used to upgrade a
    /// preview texture to the full-resolution one *in place* (re-uploading the same
    /// slot) while keeping `committed_bytes` honest. No eviction here: call
    /// [`make_room_for_upgrade`](Self::make_room_for_upgrade) first when the new size is
    /// larger. No-op if `item` isn't tracked in `kind`.
    pub fn set_slot_bytes(&mut self, item: usize, kind: RepKind, bytes: u64) {
        if let Some(&slot) = self.by_key.get(&(item, kind)) {
            self.committed_bytes = self
                .committed_bytes
                .saturating_sub(self.slot_bytes[slot])
                .saturating_add(bytes);
            self.slot_bytes[slot] = bytes;
        }
    }

    /// Commit a completed upload: `Pending(item, content_gen, rep)` →
    /// `Resident(...)`. Returns `false` if the reservation is stale — the slot was
    /// reused, the content generation advanced (index N now names different pixels), or
    /// the representation's geometry epoch no longer matches — in which case the caller
    /// drops the decoded result.
    pub fn mark_resident(
        &mut self,
        item: usize,
        slot: usize,
        content_gen: u64,
        representation: Representation,
    ) -> bool {
        if slot >= self.slots.len() {
            return false;
        }
        match self.slots[slot] {
            SlotState::Pending {
                item: it,
                content_gen: cg,
                rep,
            } if it == item && cg == content_gen && rep == representation => {
                self.slots[slot] = SlotState::Resident {
                    item,
                    content_gen,
                    rep,
                };
                self.by_key.insert((item, rep.kind()), slot);
                true
            }
            _ => false,
        }
    }

    /// Roll back a reservation that will never be fulfilled: `Pending(item, content_gen, rep)` →
    /// `Empty`, releasing its key mapping and committed bytes. The #110 GPU-derive path reserves
    /// its destination Fit slot BEFORE dispatching the derive (refuse-before-reserve, plan §4);
    /// when the derive reports ineligible/failed, the caller releases here and schedules the CPU
    /// Fit instead — a stale `Pending` would otherwise pin the slot and block that very CPU
    /// decode (`reserve_bytes` refuses while the key is tracked). Same staleness contract as
    /// [`mark_resident`](Self::mark_resident): only the caller's own pending reservation is
    /// released; a reused slot, advanced generation, or resident occupant returns `false`
    /// untouched (evicting a resident is `reserve_bytes`' job, not a rollback).
    pub fn release_pending(
        &mut self,
        item: usize,
        slot: usize,
        content_gen: u64,
        representation: Representation,
    ) -> bool {
        if slot >= self.slots.len() {
            return false;
        }
        match self.slots[slot] {
            SlotState::Pending {
                item: it,
                content_gen: cg,
                rep,
            } if it == item && cg == content_gen && rep == representation => {
                self.free_slot(slot);
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

    const CG: u64 = 1; // a fixed content generation for tests that don't vary it

    // ── #122: the reservation-refusal latch + the dry-run pre-flight ──

    /// `admittable` must agree with `reserve_bytes` on identically-built rings — it IS
    /// the dry run of the same rules.
    #[test]
    fn admittable_mirrors_reserve_bytes() {
        // Each case: (capacity, budget, residents[(item, bytes)], displayed_slot?,
        //             candidate (item, bytes), keep)
        type Case = (
            usize,
            u64,
            &'static [(usize, u64)],
            Option<usize>,
            (usize, u64),
            &'static [usize],
        );
        let cases: &[Case] = &[
            // Free slot, fits: admit.
            (4, 1000, &[(0, 100)], None, (1, 100), &[0, 1]),
            // Not in keep: refuse.
            (4, 1000, &[], None, (9, 100), &[0, 1]),
            // Rank beyond capacity: refuse.
            (1, 1000, &[(0, 10)], None, (1, 10), &[0, 1]),
            // Budget full, the only resident is higher-priority: refuse.
            (2, 100, &[(0, 90)], None, (1, 50), &[0, 1]),
            // Budget full but a LOWER-priority resident is evictable: admit.
            (2, 100, &[(1, 90)], None, (0, 50), &[0, 1]),
            // The evictable slot is displayed → pinned: refuse.
            (2, 100, &[(1, 90)], Some(0), (0, 50), &[0, 1]),
            // Alone and over budget (single-over-budget rule): admit.
            (2, 100, &[], None, (0, 500), &[0]),
        ];
        for (i, &(cap, budget, residents, displayed, (item, bytes), keep)) in
            cases.iter().enumerate()
        {
            let build = || {
                let mut r = ResidentRing::new_with_budget(cap, budget);
                for &(it, b) in residents {
                    let res = r.reserve_bytes(it, CG, fit(1), b, keep).expect("seed");
                    r.mark_resident(it, res.slot, CG, fit(1));
                }
                if let Some(s) = displayed {
                    r.set_displayed(s);
                }
                r
            };
            let predicted = build().admittable(item, RepKind::Fit, bytes, keep);
            let actual = build()
                .reserve_bytes(item, CG, fit(1), bytes, keep)
                .is_some();
            assert_eq!(
                predicted, actual,
                "case {i}: admittable={predicted} but reserve_bytes={actual}"
            );
        }
    }

    #[test]
    fn admittable_refuses_an_already_tracked_rep_and_a_same_rank_same_rep_victim() {
        let mut r = ResidentRing::new_with_budget(2, 100);
        let res = r.reserve_bytes(0, CG, fit(1), 90, &[0, 1]).expect("seed");
        r.mark_resident(0, res.slot, CG, fit(1));
        assert!(
            !r.admittable(0, RepKind::Fit, 10, &[0, 1]),
            "already tracked"
        );
        // Same rank, same rep is never a victim; a different rep of rank-0 IS
        // (making room for this rep of the same item is legitimate).
        assert!(r.admittable(0, RepKind::Original, 90, &[0, 1]));
    }

    /// The #122 livelock guard: a refusal is remembered until the answer could change —
    /// a rebuild (`compact_to`/`clear`) or a displayed-slot CHANGE — so a scheduler can
    /// stop re-requesting a decode the ring must refuse at landing.
    #[test]
    fn a_refusal_is_latched_until_a_rebuild_or_a_displayed_change() {
        let mut r = ResidentRing::new_with_budget(2, 100);
        let res = r.reserve_bytes(0, CG, fit(1), 90, &[0, 1]).expect("seed");
        r.mark_resident(0, res.slot, CG, fit(1));
        r.set_displayed(res.slot);
        assert!(!r.denied(1, RepKind::Original), "no refusal yet");
        assert!(
            r.reserve_bytes(1, CG, ORIG, 50, &[0, 1]).is_none(),
            "budget full, the resident is displayed+higher-priority"
        );
        assert!(r.denied(1, RepKind::Original), "the refusal is latched");
        // Re-pinning the SAME slot changes nothing…
        r.set_displayed(res.slot);
        assert!(r.denied(1, RepKind::Original));
        // …but a rebuild voids every refusal.
        r.compact_to(2, &[0, 1]);
        assert!(!r.denied(1, RepKind::Original), "rebuild clears the latch");

        // Displayed-slot CHANGE also clears (nav re-ranks the keep list).
        assert!(
            r.reserve_bytes(1, CG, ORIG, 500, &[0]).is_none(),
            "not in keep"
        );
        assert!(r.denied(1, RepKind::Original));
        let other = r.reserve_bytes(1, CG, fit(1), 1, &[1]).expect("a slot");
        r.mark_resident(1, other.slot, CG, fit(1));
        r.set_displayed(other.slot);
        assert!(
            !r.denied(1, RepKind::Original),
            "landing on a different photo re-opens refused items"
        );
    }

    #[test]
    fn an_already_tracked_reservation_is_not_recorded_as_a_denial() {
        let mut r = ResidentRing::new_with_budget(4, 1000);
        let res = r.reserve_bytes(0, CG, fit(1), 10, &[0]).expect("seed");
        r.mark_resident(0, res.slot, CG, fit(1));
        assert!(
            r.reserve_bytes(0, CG, fit(1), 10, &[0]).is_none(),
            "tracked"
        );
        assert!(
            !r.denied(0, RepKind::Fit),
            "tracked is success-adjacent, not a refusal"
        );
    }

    #[test]
    fn release_pending_frees_the_slot_and_its_bytes() {
        let mut r = ResidentRing::new_with_budget(4, 1000);
        let res = r.reserve_bytes(0, CG, fit(1), 400, &[0]).expect("reserve");
        assert!(r.is_tracked_rep(0, RepKind::Fit));
        assert!(r.release_pending(0, res.slot, CG, fit(1)));
        assert!(!r.is_tracked_rep(0, RepKind::Fit), "key mapping released");
        // Slot + budget genuinely free again: a full-budget re-reservation succeeds.
        assert!(
            r.reserve_bytes(0, CG, fit(1), 1000, &[0]).is_some(),
            "released bytes must return to the budget"
        );
    }

    #[test]
    fn release_pending_refuses_stale_wrong_or_resident() {
        let mut r = ResidentRing::new_with_budget(4, 1000);
        let res = r
            .reserve_bytes(0, CG, fit(1), 100, &[0, 1])
            .expect("reserve");
        // Wrong item / generation / representation: refused, reservation untouched.
        assert!(!r.release_pending(1, res.slot, CG, fit(1)));
        assert!(!r.release_pending(0, res.slot, CG + 1, fit(1)));
        assert!(!r.release_pending(0, res.slot, CG, fit(2)));
        assert!(r.is_tracked_rep(0, RepKind::Fit));
        // A RESIDENT occupant is not a pending reservation — releasing it would be an
        // eviction in disguise; refused.
        assert!(r.mark_resident(0, res.slot, CG, fit(1)));
        assert!(!r.release_pending(0, res.slot, CG, fit(1)));
        assert!(r.is_tracked_rep(0, RepKind::Fit));
    }
    fn fit(epoch: u64) -> Representation {
        Representation::Fit {
            geometry_epoch: epoch,
        }
    }
    const ORIG: Representation = Representation::Original;

    fn resident_items(r: &ResidentRing) -> Vec<usize> {
        r.slots
            .iter()
            .filter_map(|s| match s {
                SlotState::Resident { item, .. } => Some(*item),
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
        let res = r.reserve(7, CG, fit(1), &[7]).expect("a slot");
        assert_eq!(r.slot_for(7), None); // Pending, not yet resident
        assert!(r.is_tracked(7));
        assert!(r.mark_resident(res.item, res.slot, res.content_gen, res.representation));
        assert_eq!(r.slot_for(7), Some(res.slot));
    }

    #[test]
    fn reserving_a_tracked_rep_is_a_noop_but_the_other_rep_reserves() {
        let mut r = ResidentRing::new(4);
        let res = r.reserve(7, CG, fit(1), &[7]).unwrap();
        r.mark_resident(res.item, res.slot, res.content_gen, res.representation);
        // Same item+rep: no-op.
        assert_eq!(r.reserve(7, CG, fit(1), &[7]), None);
        // The OTHER representation of the same item gets its own slot.
        let o = r.reserve(7, CG, ORIG, &[7]).unwrap();
        assert_ne!(o.slot, res.slot);
        r.mark_resident(7, o.slot, CG, ORIG);
        assert_eq!(r.slot_for(7), Some(res.slot), "Fit slot");
        assert_eq!(r.original_slot(7), Some(o.slot), "Original slot");
        assert_eq!(occupied_count(&r), 2, "item 7 holds two slots");
    }

    #[test]
    fn capacity_is_never_exceeded() {
        let mut r = ResidentRing::new(3);
        let keep: Vec<usize> = (0..10).collect();
        for item in 0..10 {
            if let Some(res) = r.reserve(item, CG, fit(1), &keep) {
                r.mark_resident(res.item, res.slot, res.content_gen, res.representation);
            }
        }
        assert!(occupied_count(&r) <= 3);
        assert!(resident_items(&r).len() <= 3);
    }

    #[test]
    fn eviction_prefers_items_not_in_keep() {
        let mut r = ResidentRing::new(2);
        let a = r.reserve(10, CG, fit(1), &[10]).unwrap();
        r.mark_resident(a.item, a.slot, a.content_gen, a.representation);
        let b = r.reserve(20, CG, fit(1), &[10, 20]).unwrap();
        r.mark_resident(b.item, b.slot, b.content_gen, b.representation);
        // Want C (top) and A; B is no longer wanted -> B is the victim.
        let keep = [30, 10];
        let c = r.reserve(30, CG, fit(1), &keep).unwrap();
        r.mark_resident(c.item, c.slot, c.content_gen, c.representation);
        assert_eq!(r.slot_for(20), None, "B (not in keep) evicted");
        assert_eq!(r.slot_for(10), Some(a.slot), "A retained");
        assert!(r.slot_for(30).is_some());
    }

    #[test]
    fn displayed_slot_is_never_evicted() {
        let mut r = ResidentRing::new(2);
        let a = r.reserve(10, CG, fit(1), &[10]).unwrap();
        r.mark_resident(a.item, a.slot, a.content_gen, a.representation);
        r.set_displayed(a.slot);
        for item in [20usize, 30, 40, 50] {
            let keep = [item, 999];
            if let Some(res) = r.reserve(item, CG, fit(1), &keep) {
                assert_ne!(res.slot, a.slot, "must not reserve the displayed slot");
                r.mark_resident(res.item, res.slot, res.content_gen, res.representation);
            }
        }
        assert_eq!(r.slot_for(10), Some(a.slot), "displayed A survived");
    }

    #[test]
    fn stale_content_gen_completion_is_rejected() {
        let mut r = ResidentRing::new(2);
        let res = r.reserve(10, 1, fit(1), &[10]).unwrap();
        // Content advanced (deck rebuilt) → the completion for gen 1 is dropped.
        assert!(!r.mark_resident(10, res.slot, 2, fit(1)));
        assert_eq!(r.slot_for(10), None);
        // The correct-gen completion still commits.
        assert!(r.mark_resident(10, res.slot, 1, fit(1)));
        assert_eq!(r.slot_for(10), Some(res.slot));
    }

    #[test]
    fn stale_geometry_epoch_completion_is_rejected() {
        let mut r = ResidentRing::new(2);
        let res = r.reserve(10, CG, fit(1), &[10]).unwrap();
        // A Fit decoded for a newer geometry epoch than the reservation is dropped.
        assert!(!r.mark_resident(10, res.slot, CG, fit(2)));
        assert_eq!(r.slot_for(10), None);
        assert!(r.mark_resident(10, res.slot, CG, fit(1)));
        assert_eq!(r.slot_for(10), Some(res.slot));
    }

    #[test]
    fn low_priority_item_does_not_evict_higher_priority() {
        let mut r = ResidentRing::new(1);
        let a = r.reserve(10, CG, fit(1), &[10]).unwrap();
        r.mark_resident(a.item, a.slot, a.content_gen, a.representation);
        assert_eq!(r.reserve(20, CG, fit(1), &[10, 20]), None);
        assert_eq!(r.slot_for(10), Some(a.slot));
    }

    #[test]
    fn reserve_rejects_an_item_not_in_keep() {
        let mut r = ResidentRing::new(4);
        assert_eq!(r.reserve(99, CG, fit(1), &[1, 2, 3]), None);
        assert!(!r.is_tracked(99));
    }

    #[test]
    fn byte_budget_bounds_residency() {
        let mut r = ResidentRing::new_with_budget(8, 250);
        for item in [10usize, 11] {
            let res = r
                .reserve_bytes(item, CG, fit(1), 100, &[10, 11, 12])
                .unwrap();
            r.mark_resident(item, res.slot, CG, fit(1));
        }
        let keep = [12usize, 11, 10];
        let c = r.reserve_bytes(12, CG, fit(1), 100, &keep).unwrap();
        r.mark_resident(12, c.slot, CG, fit(1));
        let resident = [10, 11, 12]
            .iter()
            .filter(|&&i| r.slot_for(i).is_some())
            .count();
        assert_eq!(resident, 2, "byte budget caps residency at 2 of 3");
        assert!(r.slot_for(12).is_some(), "top-priority item stays resident");
    }

    #[test]
    fn make_room_for_upgrade_evicts_before_the_ring_goes_over_budget() {
        // Preview→full upgrade in place: the new size must be admitted by evicting a
        // lower-priority slot FIRST, never by transiently exceeding the budget (§7).
        let mut r = ResidentRing::new_with_budget(4, 1000);
        let a = r.reserve_bytes(1, CG, fit(1), 100, &[1, 2]).unwrap();
        r.mark_resident(1, a.slot, CG, fit(1));
        let b = r.reserve_bytes(2, CG, fit(1), 100, &[1, 2]).unwrap();
        r.mark_resident(2, b.slot, CG, fit(1));
        // Upgrade item 1 from 100 to 900. Post-upgrade: 900 + item2's 100 = 1000 ≤ budget → item 2 stays.
        assert!(r.make_room_for_upgrade(1, RepKind::Fit, 900, &[1, 2]));
        r.set_slot_bytes(1, RepKind::Fit, 900);
        assert!(r.slot_for(2).is_some(), "fits exactly; no eviction needed");
        assert!(r.committed_bytes <= 1000);
        // Now upgrade item 1 to 950: 950 + 100 > 1000 → item 2 (lower priority) is evicted first.
        assert!(r.make_room_for_upgrade(1, RepKind::Fit, 950, &[1, 2]));
        assert_eq!(
            r.slot_for(2),
            None,
            "lower-priority slot evicted before the upgrade"
        );
        r.set_slot_bytes(1, RepKind::Fit, 950);
        assert!(r.committed_bytes <= 1000, "never left over budget");
    }

    #[test]
    fn single_item_larger_than_budget_still_reserves() {
        let mut r = ResidentRing::new_with_budget(4, 100);
        assert!(r.reserve_bytes(7, CG, fit(1), 500, &[7]).is_some());
    }

    // ── retention (#106.7 §3) ──

    #[test]
    fn drop_fit_slots_keeps_originals_across_a_geometry_change() {
        let mut r = ResidentRing::new(4);
        // Item 5 resident as BOTH Fit and Original; item 6 resident as Fit only.
        let f5 = r.reserve(5, CG, fit(1), &[5, 6]).unwrap();
        r.mark_resident(5, f5.slot, CG, fit(1));
        let o5 = r.reserve(5, CG, ORIG, &[5, 6]).unwrap();
        r.mark_resident(5, o5.slot, CG, ORIG);
        let f6 = r.reserve(6, CG, fit(1), &[5, 6]).unwrap();
        r.mark_resident(6, f6.slot, CG, fit(1));
        r.set_displayed(f5.slot);

        let kept = r.drop_fit_slots();
        // Both Fit slots gone; the Original survives.
        assert_eq!(r.slot_for(5), None, "Fit(5) dropped by the geometry change");
        assert_eq!(r.slot_for(6), None, "Fit(6) dropped");
        assert_eq!(r.original_slot(5), Some(o5.slot), "Original(5) retained");
        assert_eq!(
            kept,
            vec![(5usize, o5.slot)],
            "the surviving Original is reported"
        );
    }

    #[test]
    fn clear_drops_originals_too_a_content_change_is_total() {
        let mut r = ResidentRing::new(4);
        let o = r.reserve(5, CG, ORIG, &[5]).unwrap();
        r.mark_resident(5, o.slot, CG, ORIG);
        r.clear();
        assert_eq!(
            r.original_slot(5),
            None,
            "a content change purges Originals"
        );
        assert!(!r.is_tracked(5));
    }

    #[test]
    fn compact_relocates_retained_originals_into_a_smaller_ring() {
        // Geometry change to Original mode shrinks the ring; the retained Original
        // textures must remap to valid slots in the smaller vector.
        let mut r = ResidentRing::new(6);
        for (i, &item) in [10usize, 11, 12].iter().enumerate() {
            let res = r.reserve(item, CG, ORIG, &[10, 11, 12]).unwrap();
            // spread them across non-contiguous slots to make the remap meaningful
            let _ = i;
            r.mark_resident(item, res.slot, CG, ORIG);
        }
        let _ = r.drop_fit_slots(); // no Fit slots, but mirrors the real call order
        let remaps = r.compact_to(2, &[10, 11, 12]);
        // Only the top 2 priority items survive into a 2-slot ring.
        assert_eq!(r.capacity(), 2);
        assert!(r.original_slot(10).is_some(), "top priority kept");
        assert!(r.original_slot(11).is_some(), "second priority kept");
        assert_eq!(
            r.original_slot(12),
            None,
            "third dropped (rank ≥ new capacity)"
        );
        // Every remap points at a valid new slot and the by_key agrees.
        for m in &remaps {
            assert!(m.to < 2);
            assert_eq!(r.original_slot(m.item), Some(m.to));
        }
        assert_eq!(remaps.len(), 2);
    }

    #[test]
    fn clear_resets_everything() {
        let mut r = ResidentRing::new(3);
        let a = r.reserve(1, CG, fit(1), &[1]).unwrap();
        r.mark_resident(a.item, a.slot, a.content_gen, a.representation);
        r.set_displayed(a.slot);
        r.clear();
        assert_eq!(occupied_count(&r), 0);
        assert_eq!(r.slot_for(1), None);
        assert!(!r.is_tracked(1));
        assert!(r.reserve(1, CG, fit(2), &[1]).is_some());
    }

    #[test]
    fn byte_accounting_stays_consistent_under_random_ops() {
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
            let rep = if rng.next_bounded(2) == 0 {
                fit(1)
            } else {
                ORIG
            };
            let bytes = 1 + rng.next_bounded(budget / 4);
            if let Some(res) = ring.reserve_bytes(item, CG, rep, bytes, &keep) {
                if rng.next_bounded(4) != 0 {
                    ring.mark_resident(res.item, res.slot, CG, rep);
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
    fn randomized_invariants_hold() {
        let mut rng = SplitMix64::new(0x9E37_79B9_7F4A_7C15);
        let cap = 8;
        let n_items = 40u64;
        let mut ring = ResidentRing::new(cap);
        let mut epoch = 1u64;
        let mut content_gen = 1u64;

        for _ in 0..20_000 {
            let mut keep = Vec::new();
            while keep.len() < cap {
                let it = rng.next_bounded(n_items) as usize;
                if !keep.contains(&it) {
                    keep.push(it);
                }
            }
            let item = keep[rng.next_bounded(keep.len() as u64) as usize];
            let rep = if rng.next_bounded(3) == 0 {
                ORIG
            } else {
                fit(epoch)
            };

            match rng.next_bounded(12) {
                0 => {
                    ring.clear();
                    epoch += 1;
                    content_gen += 1;
                }
                1 => {
                    // Geometry change: drop Fit slots, keep Originals, bump epoch.
                    ring.drop_fit_slots();
                    epoch += 1;
                }
                2 => {
                    if let Some(slot) =
                        (0..cap).find(|&s| !matches!(ring.slots[s], SlotState::Empty))
                    {
                        ring.set_displayed(slot);
                    }
                }
                _ => {
                    if let Some(res) = ring.reserve(item, content_gen, rep, &keep) {
                        match rng.next_bounded(4) {
                            0 => {}
                            1 => {
                                // Stale completion (wrong content_gen) must be rejected.
                                let _ = ring.mark_resident(
                                    res.item,
                                    res.slot,
                                    content_gen.wrapping_sub(1),
                                    rep,
                                );
                            }
                            _ => {
                                assert!(ring.mark_resident(res.item, res.slot, content_gen, rep));
                            }
                        }
                    }
                }
            }

            assert!(occupied_count(&ring) <= cap);
            // No (item, rep-kind) occupies two slots; by_key is consistent.
            let mut seen = std::collections::HashSet::new();
            for (slot, state) in ring.slots.iter().enumerate() {
                let Some((it, kind)) = state.occupant() else {
                    continue;
                };
                assert!(seen.insert((it, kind)), "({it},{kind:?}) in two slots");
                assert_eq!(
                    ring.by_key.get(&(it, kind)),
                    Some(&slot),
                    "by_key out of sync"
                );
            }
            assert_eq!(ring.by_key.len(), occupied_count(&ring));
        }
    }
}
