//! **Archive open orchestration** — the `AppCore` half of [`crate::archive_open`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! `archive_open.rs` holds the open *logic* (display name, status shape, the delay
//! constant); this file holds the `impl AppCore` methods that begin, poll, finish, fail and
//! cancel an open, including the password-retry path.
//!
//! Read [`crate::archive_open`]'s privacy note before touching the password path.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Start opening an archive — **the production entry point** both shells call.
    ///
    /// A plain `.zip` with no cached passwords to auto-try opens *synchronously* (that reads a
    /// central directory, not entry data) and returns its terminal outcome without ever
    /// spawning a worker or showing chrome. Everything else — 7z, the tar family, or any open
    /// that will auto-try cached passwords — goes to a worker thread, returns
    /// [`ArchiveOutcome::Pending`], and lands through [`poll_archive_load`](Self::poll_archive_load).
    ///
    /// The auto-try only runs on an *initial* open (`password.is_none()`), so a user-entered
    /// password is never silently replaced by a cached one.
    pub fn begin_archive_open(
        &mut self,
        path: std::path::PathBuf,
        password: Option<crate::SecretString>,
    ) -> crate::archive_open::ArchiveOutcome {
        use crate::archive_open::ArchiveOutcome;

        let kind = pb_source::archive_kind(&path).unwrap_or(pb_source::ArchiveKind::Zip);
        // Auto-try cached session passwords (MRU-first) only on an INITIAL open, so a
        // same-password folder asks once (session-archive-password-cache).
        let cached = if password.is_none() {
            self.archive_passwords_snapshot()
        } else {
            Vec::new()
        };
        let attempted_password = password.clone();

        // Claim the shared generation space. Both flows are registered here now, which is what
        // lets the core cancel a displaced walk ITSELF rather than each shell remembering to
        // (task #126 §12.6 — the interim unconditional cancel in the shells retires with this).
        let (id, superseded) = self
            .bg
            .begin(crate::background::OpKind::ArchiveOpen, self.now);
        self.supersede(superseded);

        // A wrong-password ZIP attempt decrypts the whole first entry, so it must go
        // off-thread; an empty cache with no user password is the synchronous fast path.
        let will_autotry = password.is_none() && !cached.is_empty();
        if !kind.background_open() && !will_autotry {
            let pw = password.as_ref().map(|p| p.expose().to_owned());
            let result =
                crate::scan::load_archive(&path, kind, pw, &pb_source::OpenProgress::new());
            self.bg.finish(id);
            return self.finish_archive_open((result, None), attempted_password, path);
        }

        let wire_gen = self.archive_wire_gen.wrapping_add(1);
        self.archive_wire_gen = wire_gen;
        let progress = pb_source::OpenProgress::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        let worker_progress = progress.clone();
        std::thread::spawn(move || {
            let out = match password {
                Some(pw) => (
                    crate::scan::load_archive(
                        &worker_path,
                        kind,
                        Some(pw.expose().to_owned()),
                        &worker_progress,
                    ),
                    None,
                ),
                None => crate::scan::load_archive_with_cache(
                    &worker_path,
                    kind,
                    &cached,
                    &worker_progress,
                ),
            };
            let _ = tx.send((wire_gen, out));
        });
        self.archive_load = Some(crate::archive_open::ArchiveOpenState {
            id,
            rx,
            wire_gen,
            path,
            attempted_password,
            progress,
        });
        ArchiveOutcome::Pending
    }

    /// Install an archive open that is already running (or, in tests, one that never will),
    /// so a test can drive the worker's channel itself — no thread, no filesystem, no sleeps.
    /// The deterministic completion point Codex asked an injectable runtime for; the mpsc
    /// channel already was one (§11.3).
    #[doc(hidden)]
    pub fn arm_archive_open(
        &mut self,
        wire_gen: u64,
        rx: std::sync::mpsc::Receiver<(u64, crate::archive_open::ArchiveResult)>,
        progress: pb_source::OpenProgress,
        path: std::path::PathBuf,
        attempted_password: Option<crate::SecretString>,
    ) -> Option<(crate::background::OpId, crate::background::OpKind)> {
        let (id, superseded) = self
            .bg
            .begin(crate::background::OpKind::ArchiveOpen, self.now);
        self.supersede(superseded);
        self.archive_load = Some(crate::archive_open::ArchiveOpenState {
            id,
            rx,
            wire_gen,
            path,
            attempted_password,
            progress,
        });
        superseded
    }

    /// Pick up a finished background archive open (called each `tick`).
    pub fn poll_archive_load(&mut self) -> crate::archive_open::ArchiveOutcome {
        use crate::archive_open::ArchiveOutcome;
        use std::sync::mpsc::TryRecvError;

        let (id, wire_gen, recv) = match self.archive_load.as_ref() {
            Some(l) => (l.id, l.wire_gen, l.rx.try_recv()),
            None => return ArchiveOutcome::Pending,
        };
        // One staleness gate for both flows: an open superseded by a newer open *or* by a
        // folder scan fails this, so its result can never rebuild the deck underneath.
        if !self.bg.is_current(id) {
            self.archive_load = None;
            return ArchiveOutcome::Cancelled;
        }
        match recv {
            Ok((g, result)) => {
                if g != wire_gen {
                    return ArchiveOutcome::Pending; // defensive; the channel is per-open
                }
                let load = self.archive_load.take().expect("checked above");
                self.bg.finish(id);
                self.finish_archive_open(result, load.attempted_password, load.path)
            }
            Err(TryRecvError::Empty) => ArchiveOutcome::Pending,
            Err(TryRecvError::Disconnected) => {
                // The worker died without sending a terminal result. Never strand its chrome.
                self.archive_load = None;
                self.bg.finish(id);
                ArchiveOutcome::Cancelled
            }
        }
    }

    /// Apply a completed open. Private: the password handling below must not be reachable from
    /// a shell (`crate::archive_open`'s privacy note).
    fn finish_archive_open(
        &mut self,
        result: crate::archive_open::ArchiveResult,
        attempted: Option<crate::SecretString>,
        path: std::path::PathBuf,
    ) -> crate::archive_open::ArchiveOutcome {
        use crate::archive::ArchiveOpenError;
        use crate::archive_open::ArchiveOutcome;

        let (outcome, winner) = result;
        // An archive that opened at all — even to find nothing viewable — proves its password.
        // Promote it here, inside the core: the winning secret is never returned to a shell.
        //
        // A CANCELLED open promotes too, on this same rule but from `cancel_archive_load`,
        // gated on the open having actually produced decrypted bytes. See the reasoning there:
        // cancelling is about the wait, not the password.
        if matches!(outcome, Ok(_) | Err(ArchiveOpenError::Empty)) {
            if let Some(pw) = attempted.as_ref().or(winner.as_ref()) {
                self.remember_archive_password(pw);
            }
        }
        match outcome {
            Ok(resolved) if !resolved.source.is_empty() => {
                self.password_archive = None;
                self.handle(contract::CoreEvent::ArchiveResolved(resolved));
                ArchiveOutcome::Opened
            }
            Ok(_) => {
                self.password_archive = None;
                ArchiveOutcome::Failed(ArchiveOpenError::Empty)
            }
            Err(ArchiveOpenError::PasswordRequired) => {
                // Remember the path so a submitted password re-opens it. `wrong` is true only
                // when THIS attempt carried a password and it was rejected — a first prompt
                // opens fresh chrome, a retry corrects the chrome already up.
                self.password_archive = Some(path.clone());
                ArchiveOutcome::NeedPassword {
                    path,
                    wrong: attempted.is_some(),
                }
            }
            Err(ArchiveOpenError::Cancelled) => {
                self.password_archive = None;
                ArchiveOutcome::Cancelled
            }
            Err(e) => {
                self.password_archive = None;
                ArchiveOutcome::Failed(e)
            }
        }
    }

    /// Ask an in-flight archive open to stop. Idempotent, and it clears the handle itself so no
    /// call site has to remember (the dir-scan lesson, §11.2).
    pub fn cancel_archive_load(&mut self) {
        if let Some(load) = self.archive_load.take() {
            load.request_cancel();
            // Keep a password this open already PROVED (owner call 2026-07-20). Nobody cancels
            // because they regret typing the correct password — they cancel because a big
            // archive is slow, and the next thing they open is often a smaller one with the
            // same password. Re-prompting there is the annoyance worth removing.
            //
            // The gate is `done() > 0`: proof, not assumption. Decompressed bytes only appear
            // if the key was right — a wrong one fails the decrypt/CRC before producing any.
            //
            // It is deliberately CONSERVATIVE, and the asymmetry is the point:
            //   * 7z / RAR are eager decodes counting decompressed bytes, so `done > 0` really
            //     does prove the password. This is the case that matters — they are the slow
            //     opens people actually cancel.
            //   * The tar family counts *compressed* bytes consumed instead, which would prove
            //     nothing — but tar has no encryption, so `attempted_password` is `None` and
            //     this never fires for it.
            //   * A ZIP opens lazily and may never stream bytes, so it simply will not promote.
            // So the gate can UNDER-promote (a missed convenience) but can never OVER-promote.
            // That direction is chosen: a wrong password in the MRU is auto-tried against every
            // later archive, and a wrong-password ZIP attempt decrypts that archive's entire
            // first entry — up to ~1 GiB — to discover it was wrong.
            if load.progress.done() > 0 {
                if let Some(pw) = load.attempted_password.as_ref() {
                    self.remember_archive_password(pw);
                }
            }
        }
        if self.bg.active_is_archive() {
            self.bg.cancel();
        }
    }

    /// The in-flight open's shared progress handle, for chrome that polls it directly (the
    /// winit Loading dialog's determinate bar owns one and reads it per frame).
    ///
    /// Handing out a clone is safe and carries nothing sensitive: `OpenProgress` is a shared
    /// counter plus a cancel flag, not part of the `SecretString` path. Prefer
    /// [`archive_status`](Self::archive_status) for a one-shot read; this exists only for
    /// chrome that must keep polling.
    pub fn archive_progress(&self) -> Option<pb_source::OpenProgress> {
        self.archive_load.as_ref().map(|l| l.progress.clone())
    }

    /// A live view of the open in flight, for whatever chrome a shell draws. `None` when none
    /// is running. Cheap and non-mutating, so it is safe to call every frame.
    pub fn archive_status(&self) -> Option<crate::archive_open::ArchiveStatus> {
        let load = self.archive_load.as_ref()?;
        Some(crate::archive_open::ArchiveStatus {
            name: crate::archive_open::archive_display_name(&load.path),
            fraction: load.progress.fraction(),
            slow: self
                .bg
                .is_slow(self.now, crate::archive_open::LOADING_DIALOG_DELAY),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{test_core, FakeArchive, ARCHIVE};

    use crate::archive_open::ArchiveOutcome;

    /// A headless core over a fake archive deck, installed the way a real
    /// archive open lands ([`AppCore::apply_archive`]).
    fn archive_core(names: &[&str]) -> AppCore {
        let mut core = test_core();
        let container = std::env::temp_dir().join("deck.zip");
        let source: Arc<dyn ItemSource> = Arc::new(FakeArchive {
            names: names.iter().map(|s| s.to_string()).collect(),
            container: container.clone(),
        });
        core.apply_archive(crate::scan::Resolved {
            root: container,
            source,
            scan_root: None,
            recursive: false,
            start: 0,
        });
        core
    }

    fn deck_names(core: &AppCore) -> Vec<&str> {
        (0..core.source.len())
            .map(|i| core.source.name(i))
            .collect()
    }

    #[test]
    fn apply_archive_stamps_the_unscoped_scope() {
        let core = archive_core(ARCHIVE);
        let scope = core.archive_scope.as_ref().expect("archive decks scope");
        assert_eq!(scope.prefix, "");
        assert!(
            Arc::ptr_eq(&scope.full, &core.source),
            "unscoped: the deck IS the full source, no wrapper"
        );
        assert_eq!(core.source.len(), ARCHIVE.len());
    }

    #[test]
    fn rescope_filters_the_deck_and_parent_steps_back_up() {
        let mut core = archive_core(ARCHIVE);
        core.rescope_archive("a/b".to_string());
        assert_eq!(deck_names(&core), vec!["a/b/one.jpg", "a/b/c/two.jpg"]);
        assert_eq!(core.archive_scope.as_ref().unwrap().prefix, "a/b");
        assert_eq!(core.displayed_item, Some(0), "cursor resets to the first");
        assert_eq!(
            core.source.container().map(crate::folder_tree::name_of),
            Some("deck.zip".to_string()),
            "the scoped deck still knows its archive (title, up row, Go anchor)"
        );

        // ⌘↑ steps the scope up one level at a time: a/b → a → the whole archive.
        core.open_parent_cmd();
        assert_eq!(core.archive_scope.as_ref().unwrap().prefix, "a");
        assert_eq!(core.source.len(), 4);
        core.open_parent_cmd();
        let scope = core.archive_scope.as_ref().unwrap();
        assert_eq!(scope.prefix, "");
        assert!(
            Arc::ptr_eq(&scope.full, &core.source),
            "back to the whole archive = the original source, unwrapped"
        );

        // From the archive root, ⌘↑ exits to the folder on disk containing the
        // archive — the pre-scoping behavior, now one level further up the ladder.
        core.effects.clear();
        core.open_parent_cmd();
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::BeginDirScan { .. })),
            "the containing folder opens as a normal dir scan"
        );
    }

    #[test]
    fn sibling_cmd_steps_scopes_in_ram_without_a_worker() {
        let mut core = archive_core(ARCHIVE);
        core.settings.show_archives = true;
        // At the archive ROOT, ⌘←/→ steps the archive's own internal folders first — a cursor
        // jump, no disk worker (task #108). Displayed on the first internal folder ("a/b") →
        // jump to the next one ("a/b/c" at index 1).
        core.displayed_item = Some(0);
        core.open_sibling_cmd(1);
        assert_eq!(
            core.playlist.current(),
            Some(1),
            "stepped to the next internal folder"
        );
        assert!(
            core.tree_io.is_none(),
            "an internal-folder step is a cursor jump, not a disk worker"
        );

        // Only past the LAST internal folder ("" at index 4) does the root step to the adjacent
        // archive on disk (a worker).
        core.displayed_item = Some(4);
        core.open_sibling_cmd(1);
        assert!(
            core.tree_io.is_some(),
            "no more internal folders → adjacent archive via a disk worker"
        );
        core.tree_io = None; // cancels the fire-and-forget probe
                             // With Show Archives off, past the last internal folder there's nothing to step to.
        core.settings.show_archives = false;
        core.displayed_item = Some(4);
        core.open_sibling_cmd(1);
        assert!(
            core.tree_io.is_none(),
            "archives hidden → no disk worker at the archive root"
        );
        core.settings.show_archives = true;

        // Scoped into an internal folder: stepping stays in-RAM, no disk worker (the subject).
        core.rescope_archive("a/b".to_string());
        core.open_sibling_cmd(1);
        assert_eq!(core.archive_scope.as_ref().unwrap().prefix, "a/bc");
        assert_eq!(deck_names(&core), vec!["a/bc/three.jpg"]);
        assert!(core.tree_io.is_none(), "internal-folder stepping is in-RAM");
        core.open_sibling_cmd(1);
        assert_eq!(
            core.archive_scope.as_ref().unwrap().prefix,
            "a/bc",
            "at the end of the sorted row — nothing to step to"
        );
        core.open_sibling_cmd(-1);
        assert_eq!(core.archive_scope.as_ref().unwrap().prefix, "a/b");
        assert!(core.tree_io.is_none());
    }

    /// Open Parent out of an archive lands the deck cursor on that archive's **door** when Show
    /// Archives is on (so `space` continues past it), and on the folder's first item when it's
    /// off (task #108 — the off case avoids the streaming-scan stall on a filtered-out target).
    #[test]
    fn open_parent_out_of_an_archive_lands_on_its_door_when_archives_shown() {
        let door = std::env::temp_dir().join("deck.zip"); // archive_core's root/container
        let begin_cursor = |core: &AppCore| {
            core.effects.iter().find_map(|e| match e {
                contract::CoreEffect::BeginDirScan { cursor, .. } => Some(cursor.clone()),
                _ => None,
            })
        };

        let mut shown = archive_core(ARCHIVE);
        shown.settings.show_archives = true;
        shown.effects.clear();
        shown.open_parent_cmd();
        assert_eq!(
            begin_cursor(&shown),
            Some(pb_core::open::Cursor::At(door.clone())),
            "archives shown → land on the archive door"
        );

        let mut hidden = archive_core(ARCHIVE);
        hidden.settings.show_archives = false;
        hidden.effects.clear();
        hidden.open_parent_cmd();
        assert_eq!(
            begin_cursor(&hidden),
            Some(pb_core::open::Cursor::First),
            "archives hidden → first item (no stall on a filtered-out door)"
        );
    }

    #[test]
    fn a_disk_rebuild_clears_the_archive_scope() {
        let mut core = archive_core(ARCHIVE);
        core.rescope_archive("a".to_string());
        assert!(core.archive_scope.is_some());
        let dir = std::env::temp_dir();
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![dir.join("a.png")]));
        core.rebuild_playlist(source, dir.clone(), Some(dir), true, 0);
        assert!(
            core.archive_scope.is_none(),
            "a disk deck must not keep the old archive resident"
        );
    }

    /// Arm an archive open on a core with a channel the TEST drives.
    fn armed_archive_core(
        attempted: Option<crate::SecretString>,
    ) -> (
        AppCore,
        std::sync::mpsc::Sender<(u64, crate::archive_open::ArchiveResult)>,
    ) {
        let mut core = test_core();
        let (tx, rx) = std::sync::mpsc::channel();
        core.arm_archive_open(
            1,
            rx,
            pb_source::OpenProgress::new(),
            std::path::PathBuf::from("/vault/holiday.7z"),
            attempted,
        );
        (core, tx)
    }

    /// #131 B — `archive_loading()` is a pure getter derived from `archive_load`, replacing the
    /// old hand-synced `archive_loading: bool` mirror field (the winit shell wrote it each tick;
    /// macOS never wrote it, so it was stale on macOS by construction). No shell involvement: it
    /// flips true the instant an open is armed and false the instant it clears.
    #[test]
    fn archive_loading_tracks_archive_load_with_no_shell_sync() {
        let idle = test_core();
        assert!(
            !idle.archive_loading(),
            "idle core: no archive open in flight"
        );

        let (mut core, _tx) = armed_archive_core(None);
        assert!(
            core.archive_loading(),
            "armed open: the getter reports true with no tick/sync step"
        );

        core.archive_load = None;
        assert!(!core.archive_loading(), "cleared: the getter reports false");
    }

    /// The symmetric half: a folder scan displaces an in-flight open, through the same gate.
    #[test]
    fn a_walk_cancels_the_displaced_archive_open_inside_the_core() {
        let (mut core, _tx) = armed_archive_core(None);
        let progress = core
            .archive_load
            .as_ref()
            .map(|l| l.progress.clone())
            .unwrap();

        let (_tx2, rx2) = std::sync::mpsc::channel();
        core.arm_dir_scan(2, rx2, crate::scan::ScanProgress::new(), "Photos".into());

        assert!(core.archive_load.is_none(), "the open is dropped");
        assert!(progress.is_cancelled(), "and its worker told to stop");
    }

    /// A wrong password re-prompts for the SAME archive, and says so, so a shell corrects the
    /// dialog already up instead of re-opening one.
    #[test]
    fn a_wrong_password_reprompts_for_the_same_operation() {
        let (mut core, tx) = armed_archive_core(Some(crate::SecretString::from("wrong")));
        tx.send((
            1,
            (
                Err(crate::archive::ArchiveOpenError::PasswordRequired),
                None,
            ),
        ))
        .unwrap();

        match core.poll_archive_load() {
            ArchiveOutcome::NeedPassword { path, wrong } => {
                assert!(wrong, "this attempt carried a password and it was rejected");
                assert_eq!(path, std::path::PathBuf::from("/vault/holiday.7z"));
            }
            other => panic!("expected a re-prompt, got {other:?}"),
        }
        assert_eq!(
            core.password_archive,
            Some(std::path::PathBuf::from("/vault/holiday.7z")),
            "the path is remembered so a submitted password re-opens it"
        );
    }

    /// A FIRST prompt is not a retry — the distinction the inline-error chrome depends on.
    #[test]
    fn a_first_prompt_is_not_marked_wrong() {
        let (mut core, tx) = armed_archive_core(None);
        tx.send((
            1,
            (
                Err(crate::archive::ArchiveOpenError::PasswordRequired),
                None,
            ),
        ))
        .unwrap();
        assert!(matches!(
            core.poll_archive_load(),
            ArchiveOutcome::NeedPassword { wrong: false, .. }
        ));
    }

    /// PRIVACY (plan §6): a winning password is promoted into the session cache exactly once,
    /// and is NEVER handed back to the shell — the outcome carries no secret at all.
    #[test]
    fn a_winning_password_is_promoted_once_and_never_returned() {
        let (mut core, tx) = armed_archive_core(Some(crate::SecretString::from("hunter2")));
        // An archive that opened but held nothing viewable still proves its password.
        tx.send((1, (Err(crate::archive::ArchiveOpenError::Empty), None)))
            .unwrap();
        let outcome = core.poll_archive_load();

        assert_eq!(
            core.archive_passwords.len(),
            1,
            "promoted exactly once, not per poll"
        );
        assert!(!format!("{outcome:?}").contains("hunter2"));
        assert!(
            !format!("{:?}", core.archive_passwords).contains("hunter2"),
            "the session cache must not render its secrets even in Debug"
        );
    }

    /// A superseded open's result must not rebuild the deck underneath whatever replaced it.
    #[test]
    fn a_superseded_open_applies_nothing() {
        let (mut core, tx) = armed_archive_core(None);
        // A walk supersedes it; the worker then finishes anyway.
        let (_tx2, rx2) = std::sync::mpsc::channel();
        core.arm_dir_scan(9, rx2, crate::scan::ScanProgress::new(), "Photos".into());
        let _ = tx.send((1, (Err(crate::archive::ArchiveOpenError::Empty), None)));

        assert!(matches!(core.poll_archive_load(), ArchiveOutcome::Pending));
        assert!(
            core.archive_passwords.is_empty(),
            "a stale result must not promote anything either"
        );
    }

    /// Cancel clears the handle itself and makes an in-flight result stale.
    #[test]
    fn cancelling_an_open_clears_it_and_is_idempotent() {
        let (mut core, tx) = armed_archive_core(None);
        let progress = core
            .archive_load
            .as_ref()
            .map(|l| l.progress.clone())
            .unwrap();

        core.cancel_archive_load();
        assert!(core.archive_load.is_none());
        assert!(progress.is_cancelled());
        assert_eq!(core.bg.active(), None, "the operation slot is released");

        let _ = tx.send((1, (Err(crate::archive::ArchiveOpenError::Empty), None)));
        assert!(matches!(core.poll_archive_load(), ArchiveOutcome::Pending));
        core.cancel_archive_load(); // idempotent
    }

    /// A dead worker (sender dropped with no terminal message) must not strand chrome.
    #[test]
    fn a_dead_worker_ends_the_operation_rather_than_hanging() {
        let (mut core, tx) = armed_archive_core(None);
        drop(tx);
        assert!(matches!(
            core.poll_archive_load(),
            ArchiveOutcome::Cancelled
        ));
        assert!(core.archive_load.is_none());
        assert_eq!(
            core.bg.active(),
            None,
            "every terminal path clears the slot"
        );
    }

    /// The reconciliation query, matching `scan_status`: present while in flight, gone after.
    #[test]
    fn archive_status_tracks_the_open() {
        let (mut core, _tx) = armed_archive_core(None);
        let s = core.archive_status().expect("an open is in flight");
        assert_eq!(s.name, "holiday.7z");
        assert!(!s.slow, "a fresh open warrants no chrome yet");

        // Pin the ARCHIVE's own delay, not the scan pill's. This test previously advanced by
        // SCAN_DIALOG_DELAY and passed only because the two constants happened to be equal;
        // they diverged on 2026-07-20 (the modal dialog earns a higher bar than the ambient
        // pill) and this is what caught it.
        core.now += crate::archive_open::LOADING_DIALOG_DELAY - Duration::from_millis(1);
        assert!(
            !core.archive_status().unwrap().slow,
            "just under the gate: still no chrome"
        );
        core.now += Duration::from_millis(1);
        assert!(core.archive_status().unwrap().slow, "at the gate: reveal");

        core.cancel_archive_load();
        assert!(core.archive_status().is_none());
    }

    /// The REVERSE of `an_archive_open_supersedes_an_in_flight_scan`, and the direction that
    /// actually caused the historical corruption: a folder scan started over an in-flight
    /// archive open must abandon that open, so a late `ArchiveResolved` cannot rebuild the deck
    /// back onto the archive on top of the folder now being scanned.
    ///
    /// Both directions run through one `supersede` helper in the core; this pins the half the
    /// shells used to own by hand (and which the winit shell did with an unconditional
    /// `cancel_dir_scan()` at the right call site, macOS with its own copy).
    #[test]
    fn a_folder_scan_supersedes_an_in_flight_archive_open() {
        let (mut core, tx) = armed_archive_core(Some(crate::SecretString::new("pw")));
        let open_id = core.archive_load.as_ref().map(|l| l.id).unwrap();
        assert!(core.bg.is_current(open_id));

        // Opening a folder now: the scan claims the shared generation space.
        let (_tx2, rx2) = std::sync::mpsc::channel();
        core.arm_dir_scan(1, rx2, crate::scan::ScanProgress::new(), "Photos".into());

        assert!(
            !core.bg.is_current(open_id),
            "the archive open is stale the instant the scan begins"
        );
        assert!(
            core.archive_load.is_none(),
            "and its worker handle is dropped, not left to land later"
        );

        // The abandoned worker can no longer deliver ANYTHING: dropping the state dropped the
        // receiver, so its send fails outright. That is a stronger guarantee than "the result
        // is ignored" — there is no channel left for a stale result to arrive on.
        assert!(
            tx.send((1, (Err(crate::archive::ArchiveOpenError::Empty), None)))
                .is_err(),
            "the superseded worker's channel must be gone, not merely ignored"
        );
        assert!(
            matches!(
                core.poll_archive_load(),
                crate::archive_open::ArchiveOutcome::Pending
            ),
            "and polling finds nothing to apply"
        );
    }

    /// Cancelling an open that HAS decrypted something keeps the password (owner call
    /// 2026-07-20). Nobody cancels because they regret typing the correct one — they cancel a
    /// slow archive, and often open a smaller one with the same password next.
    #[test]
    fn cancelling_a_progressing_open_remembers_its_proven_password() {
        let pw = crate::SecretString::new("hunter2");
        let (mut core, _tx) = armed_archive_core(Some(pw.clone()));
        // Decrypted output appeared: proof the key was right.
        core.archive_load.as_ref().unwrap().progress.add_done(4096);

        core.cancel_archive_load();

        assert_eq!(
            core.archive_passwords.len(),
            1,
            "a password that demonstrably decrypted something is worth keeping"
        );
        assert!(core.archive_load.is_none());
    }

    /// ...but an INSTANT cancel has proved nothing, so it remembers nothing. Not tidiness: a
    /// wrong password in the MRU is auto-tried against every later archive, and a
    /// wrong-password ZIP attempt decrypts that archive's whole first entry to find out.
    #[test]
    fn cancelling_before_anything_decrypted_remembers_nothing() {
        let pw = crate::SecretString::new("probably-wrong");
        let (mut core, _tx) = armed_archive_core(Some(pw.clone()));
        assert_eq!(core.archive_load.as_ref().unwrap().progress.done(), 0);

        core.cancel_archive_load();

        assert!(
            core.archive_passwords.is_empty(),
            "nothing decrypted yet - unproven, so it must not poison the auto-try cache"
        );
    }

    /// The other half of the same decision: an open that COMPLETES does remember it, so the
    /// same-password folder asks once. Without this, the test above could be satisfied by
    /// never remembering anything.
    #[test]
    fn a_completed_open_does_remember_its_password() {
        let pw = crate::SecretString::new("hunter2");
        let (mut core, tx) = armed_archive_core(Some(pw.clone()));
        tx.send((1, (Err(crate::archive::ArchiveOpenError::Empty), None)))
            .unwrap();
        core.poll_archive_load();
        assert_eq!(
            core.archive_passwords.len(),
            1,
            "an archive that opened at all proves its password"
        );
    }

    /// Inside an open archive, an entry named `inner.zip` is **not** a door — so `P`
    /// cannot enter it. Nesting stays unrepresentable rather than merely refused.
    #[test]
    fn p_inside_an_archive_cannot_enter_a_nested_zip() {
        let mut core = archive_core(&["a.jpg", "inner.zip"]);
        core.displayed_item = Some(1);
        core.effects.clear();

        assert_eq!(core.item_archive_kind(1), None, "an entry is never a door");
        core.toggle_play_pause();
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::BeginArchiveOpen { .. })),
            "a nested .zip entry must not open"
        );
    }
}
