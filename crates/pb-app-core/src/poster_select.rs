//! The poster-selection ledger (task #114): purpose-neutral, per-item state for
//! "which frame is this movie's poster" — ONE scored walk per movie per deck,
//! whose typed result fans out to every consumer (thumb tile, display Fit, the
//! parked Original in phase 3).
//!
//! Lifecycle (Codex #114 r1: `invalidate_content` does NOT clear `meta_cache`,
//! so "clear beside it" was never a real anchor): the OWNER calls
//! [`PosterSelector::reset`] from every content boundary — `invalidate_content`,
//! deck replacement / empty-state, teardown — and [`PosterSelector::forget`]
//! for a single-item content change (a saved rotation). Geometry changes leave
//! the ledger alone by design: a selection is viewport-independent.
//!
//! RAM-only, never serialized — a poster choice is a viewing-derived datum
//! (ADR-018); dropping the struct is the privacy guarantee.

use std::collections::HashMap;

use pb_decode::PosterChoice;

/// Which consumer wants this movie's poster right now. The selection job's
/// scheduling class is `Display` iff any display demand exists; a thumb-only
/// selection parks under the pool's thumb occupancy cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demand {
    Thumb,
    Display,
}

/// One item's selection state. Phase 4 adds the retry arm
/// (`WaitingForReentry`/`Terminal`); until then a failed selection falls back to
/// the legacy failed-set behavior in the drain.
#[derive(Debug, Clone, PartialEq)]
pub enum Selection {
    /// The ONE walk is wanted/in flight. `thumb`/`display` record the demand
    /// union — the want must be re-emitted every prefetch pass while this state
    /// exists (the pool is level-triggered), and it is cancelled only when the
    /// union empties (a display-cancel must not kill a selection the thumb
    /// still needs — Codex #114 r2).
    Selecting { thumb: bool, display: bool },
    /// The walk completed; the choice is remembered for the rest of the deck.
    /// Phase 1: a later re-need (evicted artifact) re-enters `Selecting` (one
    /// fresh walk — still never two at once); phases 2–3 replace that re-walk
    /// with the decode-forward replay / GPU derive from the installed Original.
    Chosen(PosterChoice),
}

/// The ledger: `content_gen` fences every mutation and lookup — a result from
/// the previous deck can never install state under a recycled index (the #109
/// deck-identity lesson, applied from day one here).
#[derive(Debug, Default)]
pub struct PosterSelector {
    content_gen: u64,
    items: HashMap<usize, Selection>,
}

impl PosterSelector {
    /// The generation selections are currently fenced to.
    pub fn content_gen(&self) -> u64 {
        self.content_gen
    }

    /// Wipe everything and adopt `content_gen` — the content-boundary reset
    /// (deck rebuild, source replacement, teardown).
    pub fn reset(&mut self, content_gen: u64) {
        self.items.clear();
        self.content_gen = content_gen;
    }

    /// Forget one item's selection (a single-item content change: the saved
    /// rotation path re-encodes pixels under the same index).
    pub fn forget(&mut self, item: usize) {
        self.items.remove(&item);
    }

    /// Record `demand` for `item` and say whether the selection WANT must be
    /// emitted this pass. `true` while the walk is wanted (Absent → installs
    /// `Selecting`; `Selecting` → level-triggered re-emission, demand unioned
    /// in). `false` once `Chosen` — the caller serves the need from the choice
    /// (or calls [`reopen`](Self::reopen) when the pixels are genuinely gone).
    pub fn want(&mut self, item: usize, demand: Demand) -> bool {
        match self.items.get_mut(&item) {
            None => {
                self.items.insert(
                    item,
                    Selection::Selecting {
                        thumb: demand == Demand::Thumb,
                        display: demand == Demand::Display,
                    },
                );
                true
            }
            Some(Selection::Selecting { thumb, display }) => {
                match demand {
                    Demand::Thumb => *thumb = true,
                    Demand::Display => *display = true,
                }
                true
            }
            Some(Selection::Chosen(_)) => false,
        }
    }

    /// Whether any recorded demand for `item` is display-class (the selection
    /// job's scheduling class; thumb-only parks under the thumb cap).
    pub fn display_class(&self, item: usize) -> bool {
        matches!(
            self.items.get(&item),
            Some(Selection::Selecting { display: true, .. })
        )
    }

    /// The demand union recorded while selecting (for routing a finished
    /// payload to exactly the consumers that asked). `(thumb, display)`;
    /// `(false, false)` when not selecting.
    pub fn demands(&self, item: usize) -> (bool, bool) {
        match self.items.get(&item) {
            Some(Selection::Selecting { thumb, display }) => (*thumb, *display),
            _ => (false, false),
        }
    }

    /// Install a finished walk's choice. Fenced: a payload from another content
    /// generation is refused (`false`) and must be dropped by the caller.
    pub fn choose(&mut self, item: usize, gen: u64, choice: PosterChoice) -> bool {
        if gen != self.content_gen {
            return false;
        }
        self.items.insert(item, Selection::Chosen(choice));
        true
    }

    /// The remembered choice, if the walk completed.
    pub fn choice(&self, item: usize) -> Option<&PosterChoice> {
        match self.items.get(&item) {
            Some(Selection::Chosen(c)) => Some(c),
            _ => None,
        }
    }

    /// A selection failed or its artifacts are gone and a consumer needs pixels
    /// again: drop back to Absent so the next [`want`](Self::want) starts ONE
    /// fresh walk (phase 1's re-walk; phases 2–3 make this rare).
    pub fn reopen(&mut self, item: usize) {
        self.items.remove(&item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice() -> PosterChoice {
        PosterChoice {
            origin_hns: 100,
            relative_hns: 450_000_000,
            native_w: 3840,
            native_h: 2160,
            content_hdr: false,
        }
    }

    #[test]
    fn one_walk_serves_both_consumers_and_unions_demand() {
        let mut s = PosterSelector::default();
        assert!(s.want(5, Demand::Thumb), "absent -> selecting, emit");
        assert!(!s.display_class(5), "thumb-only parks under the thumb cap");
        assert!(s.want(5, Demand::Display), "still selecting -> re-emit");
        assert!(s.display_class(5), "display demand promotes the class");
        assert_eq!(s.demands(5), (true, true), "the union is recorded");
        assert!(s.want(5, Demand::Thumb), "level-triggered: emit every pass");
    }

    #[test]
    fn chosen_stops_emission_and_remembers() {
        let mut s = PosterSelector::default();
        s.reset(3);
        s.want(5, Demand::Display);
        assert!(s.choose(5, 3, choice()), "same generation installs");
        assert!(!s.want(5, Demand::Display), "chosen -> no more walks");
        assert_eq!(s.choice(5).unwrap().relative_hns, 450_000_000);
    }

    #[test]
    fn a_stale_generation_payload_is_refused() {
        let mut s = PosterSelector::default();
        s.reset(3);
        s.want(5, Demand::Display);
        assert!(!s.choose(5, 2, choice()), "old-deck payload refused");
        assert!(s.choice(5).is_none());
        assert!(s.want(5, Demand::Display), "still selecting");
    }

    #[test]
    fn reset_wipes_and_refences_forget_and_reopen_drop_one() {
        let mut s = PosterSelector::default();
        s.reset(1);
        s.want(4, Demand::Thumb);
        s.want(5, Demand::Display);
        s.choose(5, 1, choice());
        s.forget(5);
        assert!(s.choice(5).is_none(), "single-item content change forgets");
        s.want(5, Demand::Display);
        s.choose(5, 1, choice());
        s.reopen(5);
        assert!(s.want(5, Demand::Display), "reopen -> one fresh walk");
        s.reset(2);
        assert_eq!(s.content_gen(), 2);
        assert_eq!(s.demands(4), (false, false), "reset wiped everything");
        assert!(
            s.want(4, Demand::Thumb),
            "post-reset selections start fresh"
        );
    }
}
