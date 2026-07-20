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
