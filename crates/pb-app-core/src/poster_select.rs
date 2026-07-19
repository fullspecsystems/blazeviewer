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
    /// Items whose native winner can NOT install as an Original (an enabled
    /// color transform — mode 1 is unmipped and derive-rejected). Stops the
    /// parked pre-install from replaying such a video forever. Content-inherent:
    /// survives `reopen` (artifact re-needs), cleared by `reset`/`forget`.
    original_blocked: std::collections::HashSet<usize>,
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
        self.original_blocked.clear();
        self.content_gen = content_gen;
    }

    /// Forget one item's selection (a single-item content change: the saved
    /// rotation path re-encodes pixels under the same index).
    pub fn forget(&mut self, item: usize) {
        self.items.remove(&item);
        self.original_blocked.remove(&item);
    }

    /// Record that `item`'s native winner cannot install as an Original (mode-1
    /// color) — the parked pre-install stops asking.
    pub fn block_original(&mut self, item: usize) {
        self.original_blocked.insert(item);
    }

    /// Whether the Original install is known-impossible for `item`.
    pub fn original_blocked(&self, item: usize) -> bool {
        self.original_blocked.contains(&item)
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

    /// Start an emission pass: clear every `Selecting` entry's demand bits so
    /// this pass's [`want`](Self::want) calls rebuild the union from the LIVE
    /// consumers (phase-1 review finding 3: demand recorded once was historical
    /// — a consumer that left kept its bit forever, mis-classing the job and
    /// poisoning the wrong failed set on error).
    pub fn begin_pass(&mut self) {
        for sel in self.items.values_mut() {
            if let Selection::Selecting { thumb, display } = sel {
                *thumb = false;
                *display = false;
            }
        }
    }

    /// End an emission pass: a `Selecting` entry whose demand union stayed
    /// empty has NO live consumer — drop it to Absent so its (uncancellable
    /// from here) pool job dies via level-triggered non-re-emission, and a
    /// future consumer starts fresh.
    pub fn end_pass(&mut self) {
        self.items.retain(|_, sel| {
            !matches!(
                sel,
                Selection::Selecting {
                    thumb: false,
                    display: false,
                }
            )
        });
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
    fn pass_brackets_rebuild_demand_from_live_consumers() {
        // Review f3: demand recorded once was historical — a consumer that left
        // kept its bit forever. The pass brackets rebuild the union each pass.
        let mut s = PosterSelector::default();
        s.want(5, Demand::Thumb);
        s.want(5, Demand::Display);
        assert_eq!(s.demands(5), (true, true));
        // Next pass: only the thumb re-asks — display's bit must not linger.
        s.begin_pass();
        s.want(5, Demand::Thumb);
        s.end_pass();
        assert_eq!(s.demands(5), (true, false), "display demand left");
        assert!(!s.display_class(5), "the job de-classes with it");
        // A pass where NO consumer re-asks drops the entry entirely: the pool
        // job dies by level-triggered non-re-emission and a future consumer
        // starts fresh.
        s.begin_pass();
        s.end_pass();
        assert_eq!(s.demands(5), (false, false));
        assert!(s.want(5, Demand::Display), "absent again — fresh walk");
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
