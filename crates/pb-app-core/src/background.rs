//! **Background-operation identity and supersession** (task #126 phase 0).
//!
//! The two long-running shell-launched operations — a directory scan and an archive open —
//! supersede *each other*, not just themselves. Today each shell hand-maintains **two**
//! generation counters (`scan_gen`, `archive_gen`) plus a hand-written cross-cancel, and the
//! winit shell's own comment records why:
//!
//! > Cross-type supersession (cross-deck open race, Codex-diagnosed 2026-07-17): a folder
//! > scan is a DIFFERENT worker than an archive open, so the `archive_gen` bump above never
//! > cancels it. Left alive, its next cumulative batch reaches the core and extends *this*
//! > archive deck while both GPU rings still hold the archive's textures — the "title
//! > advances, view frozen, door card over a photo" corruption.
//!
//! That is **one invariant expressed as two counters plus a convention**, duplicated across
//! two shells. This module makes it one counter in one place: a single generation space
//! covering both kinds, so beginning either operation makes every earlier operation of
//! *either* kind stale by construction — the cross-cancel stops being something a call site
//! has to remember.
//!
//! Deliberately **not** a generic worker abstraction (Codex round 1 rejected that, and I
//! agree): a scan is *streaming* while an archive open is *one-shot, retrying and
//! secret-bearing*, so their state machines stay bespoke. This owns only what they genuinely
//! share — identity, supersession, cancellation, and the reveal-after-delay decision.
//!
//! Pure and fully unit-testable: no I/O, no threads, and time is *passed in* (the caller
//! supplies `now` from `AppCore::now`, which the shells already stamp once per event) rather
//! than read from the clock, so the reveal-delay tests are deterministic rather than
//! real-time.

use std::time::{Duration, Instant};

/// A monotonic identity for one background operation.
///
/// Opaque on purpose: the only meaningful questions are "is this still the current
/// operation?" ([`BackgroundOps::is_current`]) and equality. Exposing the integer would
/// invite the ordering comparisons that made two separate counters go wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct OpId(u64);

/// Which long-running operation an [`OpId`] names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpKind {
    /// A streaming directory walk (`scan::stream_scan`).
    DirScan,
    /// A one-shot archive open, possibly retrying after a password prompt.
    ArchiveOpen,
}

/// One in-flight operation.
#[derive(Clone, Copy, Debug)]
struct Op {
    id: OpId,
    kind: OpKind,
    /// When it began, for the reveal-after-delay decision.
    started: Instant,
    /// Whether its progress dialog has already been revealed — latched so a slow operation
    /// reveals **once**, not on every tick past the deadline.
    revealed: bool,
}

/// The coordinator: at most one background operation is current, and its identity is drawn
/// from a single generation space shared by every [`OpKind`].
#[derive(Debug, Default)]
pub struct BackgroundOps {
    next: u64,
    active: Option<Op>,
}

impl BackgroundOps {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin an operation, **superseding whatever was in flight** — of either kind. Returns
    /// the new identity; the caller stamps it on the worker so late results can be rejected.
    ///
    /// Returns the superseded operation too (if any) so the caller can cancel its worker.
    /// That pairing is the whole point: it is impossible to begin an operation and *forget*
    /// to consider the one it displaced, which is precisely what the two-counter shape let
    /// the shells do.
    pub fn begin(&mut self, kind: OpKind, now: Instant) -> (OpId, Option<(OpId, OpKind)>) {
        let superseded = self.active.map(|o| (o.id, o.kind));
        self.next = self.next.wrapping_add(1);
        let id = OpId(self.next);
        self.active = Some(Op {
            id,
            kind,
            started: now,
            revealed: false,
        });
        (id, superseded)
    }

    /// The operation currently in flight, if any.
    pub fn active(&self) -> Option<(OpId, OpKind)> {
        self.active.map(|o| (o.id, o.kind))
    }

    /// Whether `id` is still the current operation — **the staleness gate every worker
    /// result must pass** before it is allowed to touch app state.
    pub fn is_current(&self, id: OpId) -> bool {
        self.active.is_some_and(|o| o.id == id)
    }

    /// Cancel whatever is in flight, returning it so the caller can stop the worker.
    /// Idempotent: cancelling twice is a no-op, not a panic.
    pub fn cancel(&mut self) -> Option<(OpId, OpKind)> {
        self.active.take().map(|o| (o.id, o.kind))
    }

    /// Retire `id` as terminally finished. Returns `false` — and changes nothing — when `id`
    /// is stale, which is the normal path for a superseded worker that completed anyway.
    pub fn finish(&mut self, id: OpId) -> bool {
        if self.is_current(id) {
            self.active = None;
            return true;
        }
        false
    }

    /// Whether the in-flight operation's progress dialog should be revealed **now**: it has
    /// outlasted `delay` and has not been revealed yet. Latches, so a slow operation reveals
    /// once rather than on every tick.
    ///
    /// This is the `SCAN_DIALOG_DELAY` decision that both shells currently duplicate (and
    /// each declares its own 250 ms constant for). Time is passed in, so the test for it is
    /// deterministic rather than a sleep.
    pub fn should_reveal(&mut self, now: Instant, delay: Duration) -> Option<(OpId, OpKind)> {
        let op = self.active.as_mut()?;
        if op.revealed || now.saturating_duration_since(op.started) < delay {
            return None;
        }
        op.revealed = true;
        Some((op.id, op.kind))
    }

    /// Whether the in-flight operation's dialog has been revealed — so a shell can decide
    /// whether it has anything to close.
    pub fn revealed(&self) -> bool {
        self.active.is_some_and(|o| o.revealed)
    }

    /// Whether the in-flight operation has outlasted `delay` — the same "slow enough to tell
    /// the user" fact as [`should_reveal`](Self::should_reveal), but **as a continuous
    /// predicate rather than a one-shot latch**, and without `&mut`.
    ///
    /// The two exist because the two chrome shapes ask different questions. A *modal* dialog
    /// is an event ("open one, once") and wants the latch, or it re-opens every tick. An
    /// *ambient pill* is a state ("is one warranted right now?") and must stay answerable for
    /// the whole walk — a latch would report `true` on exactly one tick and `false` forever
    /// after, so a pill driven by `should_reveal` would flicker for a single frame.
    ///
    /// Consequently this does **not** consume the reveal: a shell may call it every frame.
    pub fn is_slow(&self, now: Instant, delay: Duration) -> bool {
        self.active
            .is_some_and(|o| now.saturating_duration_since(o.started) >= delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// The invariant this module exists for: an archive open supersedes an in-flight folder
    /// scan (and vice versa), because both draw identity from ONE generation space. In the
    /// shells this required a hand-written `cancel_dir_scan()` at the right call site, and a
    /// missed call site was the "door card over a photo" corruption.
    #[test]
    fn beginning_either_kind_supersedes_the_other() {
        let now = t0();
        let mut ops = BackgroundOps::new();

        let (scan, superseded) = ops.begin(OpKind::DirScan, now);
        assert_eq!(superseded, None, "nothing was in flight");
        assert!(ops.is_current(scan));

        let (open, superseded) = ops.begin(OpKind::ArchiveOpen, now);
        assert_eq!(
            superseded,
            Some((scan, OpKind::DirScan)),
            "the displaced scan must be handed back so its worker gets cancelled"
        );
        assert!(ops.is_current(open));
        assert!(
            !ops.is_current(scan),
            "the scan is stale the instant the open begins - no call-site convention needed"
        );
    }

    /// A superseded worker that finishes anyway must not be able to retire the operation
    /// that replaced it.
    #[test]
    fn a_stale_result_cannot_finish_the_current_operation() {
        let now = t0();
        let mut ops = BackgroundOps::new();
        let (stale, _) = ops.begin(OpKind::DirScan, now);
        let (current, _) = ops.begin(OpKind::DirScan, now);

        assert!(!ops.finish(stale), "a stale id must be rejected");
        assert!(
            ops.is_current(current),
            "and must leave the current operation untouched"
        );
        assert!(ops.finish(current));
        assert_eq!(ops.active(), None, "a terminal path clears the slot");
    }

    /// Cancel makes the id stale immediately, so a result already in the channel is ignored.
    #[test]
    fn cancel_makes_in_flight_results_stale_and_is_idempotent() {
        let now = t0();
        let mut ops = BackgroundOps::new();
        let (id, _) = ops.begin(OpKind::ArchiveOpen, now);

        assert_eq!(ops.cancel(), Some((id, OpKind::ArchiveOpen)));
        assert!(!ops.is_current(id), "a result arriving now is stale");
        assert!(!ops.finish(id), "and cannot retire anything");
        assert_eq!(ops.cancel(), None, "cancelling twice is a no-op");
    }

    /// The reveal-after-delay decision: nothing before the deadline, exactly once after.
    #[test]
    fn the_progress_dialog_reveals_once_and_only_after_the_delay() {
        let now = t0();
        let delay = Duration::from_millis(250);
        let mut ops = BackgroundOps::new();
        let (id, _) = ops.begin(OpKind::DirScan, now);

        assert_eq!(ops.should_reveal(now, delay), None, "not yet");
        assert_eq!(
            ops.should_reveal(now + Duration::from_millis(249), delay),
            None,
            "still under the delay"
        );
        assert!(!ops.revealed());

        assert_eq!(
            ops.should_reveal(now + Duration::from_millis(250), delay),
            Some((id, OpKind::DirScan)),
            "reveals exactly at the deadline"
        );
        assert!(ops.revealed());
        assert_eq!(
            ops.should_reveal(now + Duration::from_secs(9), delay),
            None,
            "and never a second time - the latch is what stops per-tick re-reveals"
        );
    }

    /// A fast operation that finishes inside the delay never reveals anything, which is why
    /// the delay exists (a normal folder resolves in milliseconds and must not flash a
    /// dialog).
    #[test]
    fn a_fast_operation_never_reveals_a_dialog() {
        let now = t0();
        let delay = Duration::from_millis(250);
        let mut ops = BackgroundOps::new();
        let (id, _) = ops.begin(OpKind::DirScan, now);

        assert!(ops.finish(id));
        assert_eq!(
            ops.should_reveal(now + Duration::from_secs(1), delay),
            None,
            "nothing in flight - no dialog, however long we wait"
        );
        assert!(!ops.revealed());
    }

    /// Superseding resets the reveal clock: the new operation gets its own grace period
    /// rather than inheriting the old one's elapsed time (which would flash a dialog
    /// instantly on every subsequent open).
    #[test]
    fn superseding_restarts_the_reveal_delay() {
        let now = t0();
        let delay = Duration::from_millis(250);
        let mut ops = BackgroundOps::new();
        ops.begin(OpKind::DirScan, now);

        let late = now + Duration::from_secs(5);
        let (open, _) = ops.begin(OpKind::ArchiveOpen, late);
        assert_eq!(
            ops.should_reveal(late, delay),
            None,
            "the new operation starts its own grace period"
        );
        assert_eq!(
            ops.should_reveal(late + delay, delay),
            Some((open, OpKind::ArchiveOpen))
        );
    }

    /// `is_slow` answers the same question as `should_reveal` but keeps answering it, which
    /// is what an ambient pill needs: a latch would report `true` for one frame only.
    #[test]
    fn is_slow_is_continuous_where_should_reveal_latches() {
        let now = t0();
        let delay = Duration::from_millis(250);
        let mut ops = BackgroundOps::new();
        ops.begin(OpKind::DirScan, now);

        assert!(!ops.is_slow(now, delay), "not yet");
        assert!(!ops.is_slow(now + Duration::from_millis(249), delay));

        let late = now + Duration::from_millis(250);
        assert!(ops.is_slow(late, delay), "true at the deadline");
        assert!(
            ops.is_slow(late + Duration::from_secs(9), delay),
            "and stays true for the rest of the walk"
        );

        // Consuming the one-shot reveal must not change the continuous answer.
        assert!(ops.should_reveal(late, delay).is_some());
        assert!(ops.should_reveal(late, delay).is_none(), "latched");
        assert!(
            ops.is_slow(late, delay),
            "the pill's answer survives the dialog's latch being consumed"
        );
    }

    /// Nothing in flight is never slow, however long the clock runs.
    #[test]
    fn a_finished_operation_is_not_slow() {
        let now = t0();
        let delay = Duration::from_millis(250);
        let mut ops = BackgroundOps::new();
        let (id, _) = ops.begin(OpKind::DirScan, now);
        assert!(ops.finish(id));
        assert!(!ops.is_slow(now + Duration::from_secs(9), delay));
    }

    /// Identity is drawn from one space across kinds, so ids never collide between a scan
    /// and an open — the property that lets a single `is_current` gate both flows.
    #[test]
    fn ids_are_unique_across_kinds() {
        let now = t0();
        let mut ops = BackgroundOps::new();
        let mut seen = std::collections::HashSet::new();
        for i in 0..64 {
            let kind = if i % 2 == 0 {
                OpKind::DirScan
            } else {
                OpKind::ArchiveOpen
            };
            let (id, _) = ops.begin(kind, now);
            assert!(seen.insert(id), "id {id:?} reused");
        }
    }
}
