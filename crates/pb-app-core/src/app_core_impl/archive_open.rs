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
