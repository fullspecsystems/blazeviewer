//! **Directory scan orchestration** — the `AppCore` half of [`crate::dir_scan`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! `dir_scan.rs` holds the scan *logic*; this file holds the `impl AppCore` methods that
//! arm, poll and cancel a scan against core state. Before the split those methods lived in
//! `app_core_impl.rs` — right next to their own module, because an `AppCore` method had
//! nowhere else to go. That is the growth this pairing exists to stop.
//!
//! Both flows were moved off the two shells by #126, which each carried a byte-similar copy;
//! the shells keep only dialog realisation. Everything here is shell-neutral and unit-tested.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Start a folder walk on a worker thread — **the production entry point** both shells
    /// call in place of their own `begin_dir_scan` copies.
    ///
    /// Walking a large or deeply nested tree (the worst case: someone opens `~/Library`) takes
    /// many seconds, and doing it synchronously froze the run loop and could get the app killed
    /// as unresponsive. So the walk streams over a channel and the current view stays up until
    /// its first batch lands.
    ///
    /// Returns the operation this **superseded**, if any. Today the caller must still stop a
    /// displaced *archive open* itself, because that worker is the shell's until step 2 moves
    /// it; a displaced walk is already stopped here.
    ///
    /// Non-`Source::Scan` input is rejected **before** any state is touched. The shell copies
    /// bumped the generation and cleared tombstones first and returned late, which was harmless
    /// with one caller but is a latent bug in a generally callable core transition (plan §5a).
    pub fn begin_dir_scan(
        &mut self,
        source: pb_core::open::Source,
        cursor: pb_core::open::Cursor,
    ) -> Option<(crate::background::OpId, crate::background::OpKind)> {
        let pb_core::open::Source::Scan { roots, recursive } = source else {
            return None; // explicit lists and archives are routed elsewhere by `open_plan`
        };
        let name = crate::dir_scan::scan_display_name(&roots);
        let root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
        let scan_root = roots.first().cloned();
        // The live Show Archives preference (task #104), read at spawn time: with it off the
        // walk drops archive "doors" so the deck never lists them.
        let show_archives = self.settings.show_archives;
        let progress = crate::scan::ScanProgress::new();
        let worker_progress = progress.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        // The wire generation is the worker's own tag on each update. It stays distinct from
        // the `OpId` because `stream_scan` stamps it, and only `arm_dir_scan` knows the id.
        let wire_gen = self.scan_wire_gen.wrapping_add(1);
        self.scan_wire_gen = wire_gen;
        std::thread::spawn(move || {
            crate::scan::stream_scan(
                roots,
                recursive,
                show_archives,
                cursor,
                root,
                scan_root,
                wire_gen,
                worker_progress,
                tx,
            );
        });
        self.arm_dir_scan(wire_gen, rx, progress, name)
    }

    /// A live view of the walk in flight, for whatever scan chrome a shell draws. `None` when
    /// no walk is running. Cheap and non-mutating, so it is safe to call every frame.
    pub fn scan_status(&self) -> Option<crate::dir_scan::ScanStatus> {
        let scan = self.dir_scan.as_ref()?;
        let current = scan.progress.current();
        Some(crate::dir_scan::ScanStatus {
            found: scan.progress.found(),
            current_dir: if current == scan.name {
                String::new()
            } else {
                current
            },
            slow: self
                .bg
                .is_slow(self.now, crate::dir_scan::SCAN_DIALOG_DELAY),
            bootstrapped: self.scan_bootstrapped,
            name: scan.name.clone(),
        })
    }

    /// Install an already-spawned walk. Returns the operation it **superseded**, if any, so
    /// the caller can stop that worker.
    ///
    /// The supersession *policy* lives in [`BackgroundOps`](crate::background::BackgroundOps)
    /// — one generation space across both operation kinds — while the *mechanism* for an
    /// archive open stays with the shell until step 2 moves that worker too. That split is
    /// deliberate: the invariant has a single owner even though the two workers do not yet.
    ///
    /// Separated from [`begin_dir_scan`](Self::begin_dir_scan) so tests can arm a scan with
    /// their own channel and drive it deterministically, with no thread and no sleeps.
    pub fn arm_dir_scan(
        &mut self,
        wire_gen: u64,
        rx: std::sync::mpsc::Receiver<(u64, crate::scan::ScanUpdate)>,
        progress: crate::scan::ScanProgress,
        name: String,
    ) -> Option<(crate::background::OpId, crate::background::OpKind)> {
        // A fresh scan is a fresh universe: no stale tombstones from the previous deck.
        self.deleted.clear();
        let (id, superseded) = self.bg.begin(crate::background::OpKind::DirScan, self.now);
        // Stop whatever was displaced *here*, inside the transition, rather than trusting each
        // call site to remember. The winit shell's `cancel_dir_scan` relied on callers clearing
        // the handle afterwards and its own comment overstated that they all do (two of five do
        // not); the macOS copy cleared it internally. This adopts the macOS shape, which is
        // correct by construction (task #126 §11.2).
        //
        // Since step 2 this also stops a displaced ARCHIVE OPEN, because the core owns that
        // worker too. Handling only the walk here was the exact §12.6 asymmetry — it type-checks
        // and reads fine, and silently drops the cross-type cancel in one direction.
        self.supersede(superseded);
        self.scanning = true; // sequential-only prefetch while streaming
        self.scan_bootstrapped = false; // the first non-empty batch bootstraps the view
        self.dir_scan = Some(crate::dir_scan::DirScanState::armed(
            id, wire_gen, rx, progress, name,
        ));
        superseded
    }

    /// **User-initiated** stop of a folder scan (the pill's Cancel, File ▸ Stop Scanning, or a
    /// bound key), keeping whatever streamed in so far — the partial playlist is already live.
    ///
    /// Distinct from the bare [`cancel_dir_scan`](Self::cancel_dir_scan), which is the
    /// *mechanism* and is also used for teardown and for cross-type supersession where a new
    /// deck is about to arrive. Only the user-initiated path restores the welcome hint,
    /// because only it leaves the user looking at nothing on purpose.
    ///
    /// ⚠ The hint restore is the fix for a real gap (task #126 ledger item 3, found 2026-07-20):
    /// [`finish_scan`](Self::finish_scan) restores the "Press O to open" hint when a walk ends
    /// naturally with an empty deck, but **no cancel path did**. `show_open_hint` early-returns
    /// while `scanning` is true, and cancelling never called it afterwards — so a cold launch
    /// into a slow folder, cancelled before the first photo, left an empty canvas with the hint
    /// still suppressed. Both shells had the same hole; fixing it here fixes both.
    ///
    /// Returns whether a scan was actually running, so the shell can skip its toast.
    pub fn cancel_scan_command(&mut self) -> bool {
        if self.dir_scan.is_none() {
            return false;
        }
        let nothing_shown = !self.scan_bootstrapped && self.source.is_empty();
        self.cancel_dir_scan();
        // `cancel_dir_scan` cleared `scanning`, so `show_open_hint` will no longer suppress
        // itself. Symmetric with `finish_scan`'s restore, and gated the same way: never blank
        // an existing photo.
        if nothing_shown {
            self.show_open_hint();
        }
        self.request_prefetch();
        true
    }

    /// Cancel any in-flight walk. Idempotent, and — unlike the winit shell's version — it
    /// clears the handle itself, so no call site has to remember (task #126 §11.2).
    pub fn cancel_dir_scan(&mut self) {
        if let Some(scan) = self.dir_scan.take() {
            scan.request_cancel();
        }
        self.bg.cancel();
        self.scanning = false;
    }

    /// Pump the walk's channel, applying every snapshot queued this tick. Returns what the
    /// shell should do with its Scanning dialog.
    ///
    /// Mirrors the shipped shell logic: the first non-empty batch bootstraps the view and the
    /// rest extend it; a `Done` for the current generation ends the walk (toasting when it
    /// found nothing); a slow walk with nothing on screen asks for the progress dialog; a
    /// dead worker never strands its dialog.
    pub fn poll_dir_scan(&mut self) -> crate::dir_scan::ScanPoll {
        use crate::dir_scan::{ScanDialogRequest, ScanPoll};
        use crate::scan::ScanUpdate;
        use std::sync::mpsc::TryRecvError;
        loop {
            let (wire_gen, id, recv) = match self.dir_scan.as_ref() {
                Some(s) => (s.wire_gen, s.id, s.rx.try_recv()),
                None => return ScanPoll::idle(),
            };
            // One staleness gate for both flows: a walk superseded by a newer scan *or* by an
            // archive open fails this, so its late batches can never touch the deck.
            if !self.bg.is_current(id) {
                self.dir_scan = None;
                self.scanning = false;
                return ScanPoll::dialog(ScanDialogRequest::Close);
            }
            match recv {
                Ok((g, ScanUpdate::Batch(resolved))) => {
                    if g != wire_gen {
                        continue; // superseded (defensive; the channel is per-scan)
                    }
                    self.handle(contract::CoreEvent::ScanBatch(resolved));
                    // A photo is on screen, so a revealed dialog has served its purpose —
                    // browsing should start at the first image, not the end of the walk.
                    if self.scan_bootstrapped {
                        return ScanPoll::dialog(ScanDialogRequest::Close);
                    }
                }
                Ok((g, ScanUpdate::Done)) => {
                    if g != wire_gen {
                        continue;
                    }
                    let scanned = self
                        .dir_scan
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.dir_scan = None;
                    self.bg.finish(id);
                    let never_bootstrapped = !self.scan_bootstrapped;
                    self.handle(contract::CoreEvent::ScanDone);
                    return ScanPoll {
                        dialog: ScanDialogRequest::Close,
                        found_no_photos: never_bootstrapped.then_some(scanned),
                    };
                }
                Err(TryRecvError::Empty) => {
                    // Reveal only once the walk is slow enough to notice, and never over a
                    // photo that is already up. `should_reveal` latches, so this fires once.
                    if self.scan_bootstrapped {
                        return ScanPoll::idle();
                    }
                    let due = self
                        .bg
                        .should_reveal(self.now, crate::dir_scan::SCAN_DIALOG_DELAY)
                        .is_some();
                    if !due {
                        return ScanPoll::idle();
                    }
                    return match self.dir_scan.as_ref() {
                        Some(s) => ScanPoll::dialog(ScanDialogRequest::Reveal {
                            name: s.name.clone(),
                            progress: s.progress.clone(),
                        }),
                        None => ScanPoll::idle(),
                    };
                }
                Err(TryRecvError::Disconnected) => {
                    // The worker died (panic, or dropped its sender without a terminal Done).
                    // Never strand its dialog.
                    self.dir_scan = None;
                    self.bg.finish(id);
                    self.scanning = false;
                    return ScanPoll::dialog(ScanDialogRequest::Close);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{test_core};
    use crate::dir_scan::{ScanDialogRequest};

    /// Arm a scan on a core with a channel the TEST drives - no thread, no sleeps. This is
    /// the deterministic completion point Codex asked an injectable runtime for; the mpsc
    /// channel already was one.
    fn armed_scan_core() -> (
        AppCore,
        std::sync::mpsc::Sender<(u64, crate::scan::ScanUpdate)>,
    ) {
        let mut core = test_core();
        let (tx, rx) = std::sync::mpsc::channel();
        core.arm_dir_scan(1, rx, crate::scan::ScanProgress::new(), "Photos".into());
        (core, tx)
    }

    /// `begin_dir_scan` must reject a non-scan source **before touching any state**. The shell
    /// copies bumped the generation and cleared tombstones first and returned late — harmless
    /// with exactly one caller, a latent bug in a generally callable core transition (§5a).
    #[test]
    fn begin_dir_scan_validates_before_it_mutates() {
        let mut core = test_core();
        core.deleted.insert(std::path::PathBuf::from("/gone.jpg"));

        let superseded = core.begin_dir_scan(
            pb_core::open::Source::Archive(std::path::PathBuf::from("/a.zip")),
            pb_core::open::Cursor::First,
        );

        assert_eq!(superseded, None);
        assert!(core.dir_scan.is_none(), "no walk was armed");
        assert_eq!(core.bg.active(), None, "no generation was claimed");
        assert!(!core.scanning);
        assert!(
            core.deleted.contains(std::path::Path::new("/gone.jpg")),
            "tombstones survive a rejected open - the shells cleared them first"
        );
    }

    /// The status query is what lets one core drive two different scan chromes: it reports
    /// `slow` and `bootstrapped` as SEPARATE facts, so a shell can show ambient chrome for the
    /// whole walk (macOS, and winit's pill) while blocking chrome hides once a photo is up.
    #[test]
    fn scan_status_reports_slow_and_bootstrapped_independently() {
        let (mut core, _tx) = armed_scan_core();
        let start = core.now;

        let s = core.scan_status().expect("a walk is in flight");
        assert_eq!(s.name, "Photos");
        assert!(!s.slow, "a fresh walk is not yet worth any chrome");
        assert!(!s.bootstrapped);

        core.now = start + crate::dir_scan::SCAN_DIALOG_DELAY;
        assert!(core.scan_status().unwrap().slow, "past the delay");

        // A photo lands. `slow` must NOT be cleared by it - they answer different questions.
        core.scan_bootstrapped = true;
        let s = core.scan_status().unwrap();
        assert!(s.slow && s.bootstrapped);

        // Unlike `should_reveal`'s latch, the query keeps answering for the whole walk.
        core.now = start + Duration::from_secs(30);
        assert!(core.scan_status().unwrap().slow);
    }

    /// No walk, no status - so chrome driven by it disappears the moment the walk ends.
    #[test]
    fn scan_status_is_none_once_the_walk_ends() {
        let (mut core, tx) = armed_scan_core();
        assert!(core.scan_status().is_some());
        tx.send((1, crate::scan::ScanUpdate::Done)).unwrap();
        core.poll_dir_scan();
        assert!(core.scan_status().is_none());
    }

    /// The sub-folder line is blanked while the walk is still in the root, so chrome does not
    /// print the headline twice. Both shells did this independently; the core does it once.
    #[test]
    fn scan_status_blanks_the_subfolder_while_it_repeats_the_headline() {
        let mut core = test_core();
        let (_tx, rx) = std::sync::mpsc::channel();
        let progress = crate::scan::ScanProgress::new();
        core.arm_dir_scan(1, rx, progress.clone(), "Photos".into());

        progress.set_current("Photos".into());
        assert_eq!(core.scan_status().unwrap().current_dir, "");

        progress.set_current("Photos/2019".into());
        assert_eq!(core.scan_status().unwrap().current_dir, "Photos/2019");
    }

    /// The invariant phase 0 exists for, now end-to-end: an archive open supersedes an
    /// in-flight walk, so the walk's late batches can never reach the deck. In the shells
    /// this needed a hand-written `cancel_dir_scan()` at the right call site, and missing it
    /// was the "door card over a photo" corruption.
    #[test]
    fn an_archive_open_supersedes_an_in_flight_scan() {
        let (mut core, tx) = armed_scan_core();
        // The archive open claims the shared generation space.
        let (_open, superseded) = core
            .bg
            .begin(crate::background::OpKind::ArchiveOpen, core.now);
        assert!(
            matches!(superseded, Some((_, crate::background::OpKind::DirScan))),
            "the displaced scan must be handed back so its worker is stopped"
        );

        // A batch the walk already sent now arrives. It must be dropped, not applied.
        tx.send((1, crate::scan::ScanUpdate::Done)).unwrap();
        let poll = core.poll_dir_scan();
        assert_eq!(poll.dialog, crate::dir_scan::ScanDialogRequest::Close);
        assert!(core.dir_scan.is_none(), "the stale walk is dropped");
        assert!(!core.scanning);
    }

    /// Arming a second walk supersedes the first through the same one gate.
    #[test]
    fn a_second_scan_supersedes_the_first() {
        let (mut core, _tx) = armed_scan_core();
        let first = core.dir_scan.as_ref().map(|s| s.id).unwrap();
        let (_tx2, rx2) = std::sync::mpsc::channel();
        core.arm_dir_scan(2, rx2, crate::scan::ScanProgress::new(), "Other".into());
        assert!(
            !core.bg.is_current(first),
            "the first walk is stale at once"
        );
        assert!(core.bg.is_current(core.dir_scan.as_ref().unwrap().id));
    }

    /// Cancel clears the handle ITSELF - the macOS shape (task #126 section 11.2). The winit
    /// copy relied on every call site clearing afterwards, and its comment overstated that
    /// they all do.
    #[test]
    fn cancel_clears_the_handle_without_help_from_the_call_site() {
        let (mut core, _tx) = armed_scan_core();
        core.cancel_dir_scan();
        assert!(core.dir_scan.is_none(), "no call-site convention required");
        assert!(!core.scanning);
        assert_eq!(core.bg.active(), None);
        core.cancel_dir_scan(); // idempotent
        assert!(core.dir_scan.is_none());
    }

    /// A slow walk with nothing on screen asks for the dialog - but only after the delay, and
    /// only once. Deterministic: `now` is moved by hand, never slept on.
    #[test]
    fn a_slow_walk_asks_for_the_dialog_once_and_only_after_the_delay() {
        let (mut core, _tx) = armed_scan_core();
        let start = core.now;

        assert_eq!(
            core.poll_dir_scan().dialog,
            ScanDialogRequest::None,
            "too soon"
        );

        core.now = start + crate::dir_scan::SCAN_DIALOG_DELAY - Duration::from_millis(1);
        assert_eq!(
            core.poll_dir_scan().dialog,
            ScanDialogRequest::None,
            "still under"
        );

        core.now = start + crate::dir_scan::SCAN_DIALOG_DELAY;
        assert_eq!(
            core.poll_dir_scan().dialog,
            ScanDialogRequest::Reveal {
                name: "Photos".into(),
                progress: crate::scan::ScanProgress::new(),
            },
            "reveals at the deadline"
        );

        core.now = start + Duration::from_secs(30);
        assert_eq!(
            core.poll_dir_scan().dialog,
            ScanDialogRequest::None,
            "and never again - the latch is what stops a per-tick re-reveal"
        );
    }

    /// The dialog must never pop over a photo that is already up: once a batch has
    /// bootstrapped the view, the walk goes quiet however slow it is.
    #[test]
    fn the_dialog_never_pops_over_an_already_bootstrapped_photo() {
        let (mut core, _tx) = armed_scan_core();
        core.scan_bootstrapped = true;
        core.now += Duration::from_secs(30);
        assert_eq!(
            core.poll_dir_scan().dialog,
            ScanDialogRequest::None,
            "a photo is on screen; a progress dialog would be an interruption"
        );
    }

    /// A finished walk that found nothing hands the folder name back so the shell can toast
    /// it, and closes the dialog. A walk that DID find photos toasts nothing.
    #[test]
    fn an_empty_walk_reports_its_folder_name_and_a_productive_one_does_not() {
        let (mut core, tx) = armed_scan_core();
        tx.send((1, crate::scan::ScanUpdate::Done)).unwrap();
        let poll = core.poll_dir_scan();
        assert_eq!(poll.dialog, ScanDialogRequest::Close);
        assert_eq!(
            poll.found_no_photos.as_deref(),
            Some("Photos"),
            "an empty folder is reported by name"
        );
        assert!(core.dir_scan.is_none(), "a terminal path clears the walk");
        assert_eq!(core.bg.active(), None, "and retires the operation");

        let (mut core, tx) = armed_scan_core();
        core.scan_bootstrapped = true; // photos were found
        tx.send((1, crate::scan::ScanUpdate::Done)).unwrap();
        let poll = core.poll_dir_scan();
        assert_eq!(poll.found_no_photos, None, "nothing to apologise for");
    }

    /// A worker that dies without sending `Done` (panic, or a dropped sender) must not strand
    /// its dialog on screen forever.
    #[test]
    fn a_dead_worker_never_strands_its_dialog() {
        let (mut core, tx) = armed_scan_core();
        drop(tx); // the worker vanished
        let poll = core.poll_dir_scan();
        assert_eq!(poll.dialog, ScanDialogRequest::Close);
        assert!(core.dir_scan.is_none());
        assert!(!core.scanning);
        assert_eq!(core.bg.active(), None);
    }

    /// Polling with no walk in flight is a no-op, not a panic - the tick calls it every frame.
    #[test]
    fn polling_with_no_walk_is_inert() {
        let mut core = test_core();
        assert_eq!(core.poll_dir_scan(), crate::dir_scan::ScanPoll::idle());
    }

    /// A batch tagged with a stale WIRE generation is skipped even while the operation id is
    /// current (belt-and-braces: the channel is per-scan, so this is defensive).
    #[test]
    fn a_batch_from_a_stale_wire_generation_is_skipped() {
        let (mut core, tx) = armed_scan_core();
        tx.send((99, crate::scan::ScanUpdate::Done)).unwrap(); // wrong generation
        drop(tx);
        let poll = core.poll_dir_scan();
        // The stale Done was skipped; the loop then saw the disconnect.
        assert_eq!(
            poll.found_no_photos, None,
            "a stale Done must not report a result"
        );
        assert!(core.dir_scan.is_none());
    }

    /// A fresh scan is a fresh universe: stale delete tombstones from the previous deck must
    /// not survive into it.
    #[test]
    fn arming_a_scan_clears_stale_delete_tombstones() {
        let mut core = test_core();
        core.deleted.insert(std::path::PathBuf::from("gone.jpg"));
        let (_tx, rx) = std::sync::mpsc::channel();
        core.arm_dir_scan(1, rx, crate::scan::ScanProgress::new(), "Photos".into());
        assert!(core.deleted.is_empty(), "fresh scan, fresh universe");
    }

    /// #126 ledger item 3, and the bug it turned out to be hiding. A scan that ends NATURALLY
    /// with an empty deck restores the "Press O to open" hint (`finish_scan`), but no cancel
    /// path did — `show_open_hint` suppresses itself while `scanning` is true, and cancelling
    /// never called it again. A cold launch into a slow folder, cancelled before the first
    /// photo, left an empty canvas with the hint still suppressed.
    #[test]
    fn cancelling_a_scan_with_an_empty_deck_restores_the_welcome_hint() {
        let (mut core, _tx) = armed_scan_core();
        assert!(core.source.is_empty() && !core.scan_bootstrapped);
        core.effects.clear();

        assert!(core.cancel_scan_command(), "a scan was running");

        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "the welcome hint must be re-shown, or the user is left on a blank canvas"
        );
        assert!(core.dir_scan.is_none());
        assert!(!core.scanning);
    }

    /// ...but a cancel that leaves a photo up must NOT blank it with a welcome hint. Same gate
    /// `finish_scan` uses.
    #[test]
    fn cancelling_a_scan_that_found_photos_leaves_the_deck_alone() {
        let (mut core, _tx) = armed_scan_core();
        core.scan_bootstrapped = true; // photos streamed in
        core.effects.clear();

        assert!(core.cancel_scan_command());

        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "a partial deck stays on screen - never replaced by the open hint"
        );
    }

    /// The command is a no-op when nothing is running, so the menu item / key is safe to spam.
    #[test]
    fn cancelling_with_no_scan_running_is_inert() {
        let mut core = test_core();
        core.effects.clear();
        assert!(!core.cancel_scan_command(), "nothing to cancel");
        assert!(core.effects.is_empty());
    }
}
