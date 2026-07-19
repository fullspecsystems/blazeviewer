//! Bounded session retry for transient decode failures (task #114 phase 4,
//! owner-extended to ALL item kinds — photos included).
//!
//! Today a failed decode is terminal for the session: one SMB hiccup inserts
//! `AppCore::failed` / `Thumbs::failed` and that tile/photo stays blank until
//! the folder reopens ("never re-planned"). This ledger wraps those sets with
//! ONE bounded second chance:
//!
//! - A failure records against the item's **attempt budget** (max
//!   [`MAX_FAILS`] failures per item per session — the initial attempt plus one
//!   retry; a genuinely corrupt file fails twice and stays failed).
//! - The retry fires on a **demand re-entry edge** per DOMAIN (Display = the
//!   prefetch window, Thumb = the strip's demand range): the item must LEAVE
//!   demand and come back — that's what bounds it (round-3 contract; "is
//!   visible this tick" would loop). On the edge the domain's failed gate is
//!   lifted and the normal machinery re-requests the item.
//! - Cancellation never consumes the budget: only a real FAILURE counts, so a
//!   retry decode cancelled by navigation retries again on the next edge.
//! - Success ([`RetryLedger::recover`]) clears everything for the item.
//! - Resident-preview full-decode errors are exempt by construction — they
//!   never enter the failed sets (they ride `upgrade_done` + the watchdog).
//!
//! RAM-only; reset with the deck (indices are reassigned).

use std::collections::HashMap;

/// A failure/demand domain (round-3 finding: a photo can leave display demand
/// while sitting in the much wider thumb window forever, so an item-level
/// demand union would never go absent — each domain gets its own edge).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Domain {
    Display,
    Thumb,
}

/// Failures allowed per item per session: the initial attempt + one retry.
pub const MAX_FAILS: u8 = 2;

#[derive(Debug, Default)]
pub struct RetryLedger {
    /// Item -> failures recorded this session (shared across domains: the
    /// owner's one-budget rule — no double-spend via the back door).
    fails: HashMap<usize, u8>,
    /// (item, domain) -> was the item in this domain's demand last pass.
    seen: HashMap<(usize, Domain), bool>,
}

impl RetryLedger {
    /// Record a real failure. Returns `true` while the item still has retry
    /// budget (the caller keeps it in the failed set either way; the budget
    /// only decides whether a future edge lifts the gate).
    pub fn fail(&mut self, item: usize) -> bool {
        let n = self.fails.entry(item).or_insert(0);
        *n = n.saturating_add(1);
        // The failure happened in-demand by construction (only demanded items
        // decode), so prime BOTH domains "present": the retry requires a real
        // leave-and-return, never the very next pass.
        let n = *n;
        self.seen.insert((item, Domain::Display), true);
        self.seen.insert((item, Domain::Thumb), true);
        n < MAX_FAILS
    }

    /// A decode/selection landed for the item: forget everything (Recovered).
    pub fn recover(&mut self, item: usize) {
        self.fails.remove(&item);
        self.seen.retain(|(i, _), _| *i != item);
    }

    /// Per-pass edge detection for one failed item in one domain: records the
    /// item's current demand membership and returns `true` exactly on an
    /// absent→present edge while retry budget remains — the caller lifts the
    /// domain's failed gate then.
    pub fn edge(&mut self, item: usize, domain: Domain, present: bool) -> bool {
        let was = self.seen.insert((item, domain), present).unwrap_or(false);
        present && !was && self.fails.get(&item).is_some_and(|&n| n < MAX_FAILS)
    }

    /// Whether the item has exhausted its budget (diagnostics/tests).
    pub fn terminal(&self, item: usize) -> bool {
        self.fails.get(&item).is_some_and(|&n| n >= MAX_FAILS)
    }

    /// Deck boundary: indices are reassigned — drop everything.
    pub fn reset(&mut self) {
        self.fails.clear();
        self.seen.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transient_failure_retries_on_the_reentry_edge_only() {
        let mut r = RetryLedger::default();
        assert!(r.fail(5), "first failure leaves retry budget");
        // Still present (the failure happened in-demand): NO edge — that's the
        // bound against tight loops.
        assert!(!r.edge(5, Domain::Display, true));
        assert!(!r.edge(5, Domain::Display, true), "still present, still no");
        // Leaves demand, comes back: the edge fires exactly once.
        assert!(!r.edge(5, Domain::Display, false));
        assert!(r.edge(5, Domain::Display, true), "absent -> present fires");
        assert!(!r.edge(5, Domain::Display, true), "and only once");
    }

    #[test]
    fn a_second_failure_is_terminal() {
        let mut r = RetryLedger::default();
        r.fail(5);
        assert!(!r.fail(5), "budget exhausted");
        assert!(r.terminal(5));
        r.edge(5, Domain::Display, false);
        assert!(
            !r.edge(5, Domain::Display, true),
            "no edge ever fires for a terminal item"
        );
    }

    #[test]
    fn domains_have_independent_edges_but_one_budget() {
        let mut r = RetryLedger::default();
        r.fail(5);
        // The item never leaves the wide thumb window but DOES leave display.
        assert!(!r.edge(5, Domain::Thumb, true));
        assert!(!r.edge(5, Domain::Display, false));
        assert!(
            !r.edge(5, Domain::Thumb, true),
            "thumb stays present: no edge"
        );
        assert!(
            r.edge(5, Domain::Display, true),
            "the display edge fires independently"
        );
    }

    #[test]
    fn recovery_clears_and_reset_wipes() {
        let mut r = RetryLedger::default();
        r.fail(5);
        r.recover(5);
        assert!(!r.terminal(5));
        // A recovered item's next failure starts a fresh budget.
        assert!(r.fail(5));
        r.reset();
        assert!(!r.terminal(5));
        assert!(r.fail(5), "post-reset budgets are fresh");
    }
}
