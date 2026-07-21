//! **Delete** — the `AppCore` half of [`crate::delete`] and [`crate::retry`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! `delete.rs` performs the trash/permanent removal; this file holds the `impl AppCore`
//! methods that confirm it, apply the result to the deck, and drive the retry path when the
//! file is locked.
//!
//! ⚠ Deleting is an **explicit user edit** — one of the few places the app writes to the
//! user's disk at all, and allowed precisely because it is never a byproduct of viewing.
//! See the privacy guarantee in the root `CLAUDE.md`.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Request the permanent-delete confirmation for the displayed photo (`Shift+Del`, or the
    /// trash-fallback paths in [`delete_to_trash`](Self::delete_to_trash) / [`do_delete`](Self::do_delete)).
    /// Irreversible, so it never deletes here: it settles any pending delete-advance, refuses an
    /// archive entry (no file on disk → toast), **arms `pending_confirm_delete`**, and asks the
    /// shell to render the confirm dialog. The Yes answer routes back through
    /// `ConfirmAnswered(true)` → `do_delete(.., true)` (unchanged). The guard + arm were
    /// duplicated in both shells' `confirm_delete_permanent` (#131 A.3 — inverted off
    /// `ShellFlowAction`).
    ///
    /// ⚠ **Cross-machine lever (#131 A.3 / §6a):** on macOS this still emits the legacy
    /// `ShellFlowAction(DeletePermanent)` so the native shell's existing, working delete-confirm
    /// path (its own guard/arm/sheet) runs unchanged until the Mac session wires the new
    /// `ShowDeleteConfirm` effect into `map_effect`/Swift and flips this `cfg` arm off. Everywhere
    /// else the core arms and emits `ShowDeleteConfirm { name }` directly. Delete the macOS arm
    /// once the Mac has the `ShowDeleteConfirm` handler + a green `delete_permanent_*` test.
    pub fn request_delete_confirm(&mut self) {
        #[cfg(target_os = "macos")]
        {
            // Lever: macOS keeps its legacy path (its shell does flush/guard/arm/ShowDialog).
            self.effects.push(contract::CoreEffect::ShellFlowAction(
                Action::DeletePermanent,
            ));
        }
        #[cfg(not(target_os = "macos"))]
        {
            // Settle any still-pending delete-advance first (e.g. a rapid second Del).
            self.flush_pending_delete();
            let Some(item) = self.displayed_item else {
                return;
            };
            if self.source.path(item).is_none() {
                self.show_toast("Can't delete this"); // archive entry — no file
                return;
            }
            let name = crate::engine::file_name_of(self.source.name(item));
            self.pending_confirm_delete = Some(item);
            self.effects
                .push(contract::CoreEffect::ShowDeleteConfirm { name });
        }
    }

    /// **Delete to Trash** (`Del`): send the displayed photo to the OS Recycle Bin / Trash
    /// (recoverable, no prompt). Archive entries have no file on disk → a toast, no-op. The
    /// playlist advance is deferred a beat by [`do_delete`](Self::do_delete).
    pub fn delete_to_trash(&mut self) {
        // Settle any still-pending delete-advance first (e.g. a rapid second Del).
        self.flush_pending_delete();
        let Some(item) = self.displayed_item else {
            return;
        };
        let Some(path) = self.source.path(item).map(Path::to_path_buf) else {
            self.show_toast("Can't delete this"); // archive entry — no file
            return;
        };
        // On a drive configured to bypass the Recycle Bin, a "recycle" would silently permanently
        // delete (the shell honors the per-volume NukeOnDelete policy) — no undo, and (via the
        // trash crate's FOF_NO_UI) no warning. Route Del through the permanent-delete confirmation
        // instead: the shell opens the themed confirm dialog, whose Yes calls do_delete(.., true).
        if !crate::delete::will_recycle(&path) {
            self.request_delete_confirm(); // #131 A.3 (re-derives the displayed item, same as before)
            return;
        }
        self.do_delete(item, &path, false);
    }

    /// Perform the actual deletion of `item` (`path`) — recoverable (Recycle Bin) or permanent —
    /// then flash an icon-only pill on the still-shown photo and defer the playlist advance a beat
    /// (`DELETE_ADVANCE_DELAY`) so the feedback registers first. The permanent path reaches here
    /// only after the shell's confirm dialog answers Yes (`do_delete(.., true)`). An explicit,
    /// user-initiated file removal — never the passive view path (privacy #2). The trash / remove
    /// I/O is cross-platform (`crate::delete`), so this is a pure core method.
    pub fn do_delete(&mut self, item: usize, path: &Path, permanent: bool) {
        // Release media handles FIRST (task #79 action matrix): a playing video's
        // reader holds the file open; stopping starts its (async) retirement so
        // the delete — or its brief retry below — can succeed.
        if self.video.as_ref().is_some_and(|v| v.item() == item) {
            self.stop_video();
        }
        let res = if permanent {
            crate::delete::delete_permanently(path).map(|()| None)
        } else {
            crate::delete::recycle(path).map(Some)
        };
        match res {
            Ok(outcome) => self.finish_delete(item, path, permanent, outcome),
            Err(e) => {
                // A video's decoder can still be retiring (~1 s on HEVC) — the handle
                // clears momentarily, so retry off the event loop instead of failing.
                if self.item_is_video(item) {
                    eprintln!(
                        "delete blocked (retrying while the reader retires): {}: {e}",
                        path.display()
                    );
                    self.pending_delete_retry = Some(crate::app_core::DeleteRetry {
                        at: self.now + DELETE_RETRY_INTERVAL,
                        item,
                        path: path.to_path_buf(),
                        permanent,
                        tries_left: DELETE_RETRY_MAX,
                    });
                    return;
                }
                // A recoverable delete the OS refused (a read-only / no-Trash volume — common on
                // macOS network shares) would otherwise dead-end on "Delete failed". Offer the
                // permanent-delete confirmation instead (the same themed dialog Shift+Del uses),
                // so the user can still remove the file deliberately. A *permanent* delete that
                // fails is a genuine error with nowhere left to escalate.
                if !permanent {
                    eprintln!(
                        "trash refused, offering permanent delete: {}: {e}",
                        path.display()
                    );
                    self.request_delete_confirm(); // #131 A.3
                    return;
                }
                eprintln!("delete failed: {}: {e}", path.display());
                self.show_toast("Delete failed");
            }
        }
    }

    /// The post-I/O half of a successful delete: freeze playback, flash the icon
    /// pill, defer the playlist advance a beat. `outcome` is `None` for a permanent delete;
    /// for the recoverable path it carries whether the file actually reached a restorable Recycle
    /// Bin / Trash (from [`crate::delete::recycle`]) — captured at delete time because macOS can't
    /// re-derive the Trash location from the original path afterward. When restorable, records the
    /// Edit ▸ Undo entry.
    fn finish_delete(
        &mut self,
        item: usize,
        path: &Path,
        permanent: bool,
        outcome: Option<crate::delete::RecycleOutcome>,
    ) {
        // Deleting a playing animation stops playback so the doomed photo freezes on its current
        // frame under the trash icon (rather than animating until removal).
        self.stop_playback();
        debug_assert_eq!(
            permanent,
            outcome.is_none(),
            "a permanent delete carries no recycle outcome; a recoverable one always does"
        );
        let _ = permanent;
        let icon = match outcome {
            // Explicit Shift+Del / confirmed permanent delete: trash icon, no undo.
            None => ToastIcon::Delete,
            // Recoverable delete that reached a restorable bin: record an undo entry
            // (Ctrl+Z / Edit ▸ Undo) and show the recycle icon.
            Some(crate::delete::RecycleOutcome::Recycled(handle)) => {
                let name = crate::engine::file_name_of(&path.to_string_lossy());
                self.undo_stack.push(UndoAction::Deletion {
                    index: item,
                    path: path.to_path_buf(),
                    name,
                    handle,
                });
                ToastIcon::Recycle
            }
            // A bypass-the-bin volume slipped past `will_recycle` and nuked it (Windows/Linux):
            // show the permanent icon rather than a misleading recycle one, and record no undo.
            Some(crate::delete::RecycleOutcome::Permanent) => ToastIcon::Delete,
        };
        self.show_toast_icon("", icon);
        self.pending_delete = Some((self.now + DELETE_ADVANCE_DELAY, item));
    }

    /// Drive a scheduled delete retry (a video whose reader was still retiring).
    /// Called from `tick`; bounded — after the tries run out it reports honestly.
    pub fn poll_delete_retry(&mut self) {
        let due = self
            .pending_delete_retry
            .as_ref()
            .is_some_and(|r| self.now >= r.at);
        if !due {
            return;
        }
        let mut retry = self.pending_delete_retry.take().expect("checked above");
        let res = if retry.permanent {
            crate::delete::delete_permanently(&retry.path).map(|()| None)
        } else {
            crate::delete::recycle(&retry.path).map(Some)
        };
        match res {
            Ok(outcome) => self.finish_delete(retry.item, &retry.path, retry.permanent, outcome),
            Err(e) => {
                retry.tries_left = retry.tries_left.saturating_sub(1);
                if retry.tries_left == 0 {
                    eprintln!("delete failed: {}: {e}", retry.path.display());
                    self.show_toast("Delete failed");
                } else {
                    retry.at = self.now + DELETE_RETRY_INTERVAL;
                    self.pending_delete_retry = Some(retry);
                }
            }
        }
    }

    /// Perform a deferred delete's playlist advance: drop the removed item, rebuild the
    /// source from the remaining paths (indices shift, so index-keyed state resets —
    /// fine for an explicit, infrequent command), and advance to the next photo (the
    /// previous if it was the last; the empty state if none remain). Idempotent — a
    /// no-op when nothing is pending.
    pub fn flush_pending_delete(&mut self) {
        let Some((_, removed)) = self.pending_delete.take() else {
            return;
        };
        let len = self.source.len();
        // If a scan is still streaming in, tombstone the deleted path so a later batch (whose
        // cumulative list still has it) can't bring it back. (No-op once the scan finishes.)
        if self.scanning {
            if let Some(p) = self.source.path(removed).map(Path::to_path_buf) {
                self.deleted.insert(p);
            }
        }
        match cursor_after_removal(len, removed) {
            None => self.enter_empty_state(),
            Some(start) => {
                let remaining: Vec<PathBuf> = (0..len)
                    .filter(|&i| i != removed)
                    .filter_map(|i| self.source.path(i).map(Path::to_path_buf))
                    .collect();
                let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(remaining));
                let root = self.root.clone();
                let scan_root = self.scan_root.clone();
                let recursive = self.recursive;
                self.rebuild_playlist(src, root, scan_root, recursive, start);
            }
        }
    }
}

#[cfg(test)]
// A.3 asserts the `ShowDeleteConfirm` path; macOS keeps the legacy `ShellFlowAction` lever
// (#131 §6a), so these non-macOS assertions don't apply there. The Mac verifies its own path.
#[cfg(not(target_os = "macos"))]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{photos_named, test_core, FakeArchive};

    /// #131 A.3 — `request_delete_confirm` on a real file settles, arms `pending_confirm_delete`,
    /// and emits `ShowDeleteConfirm { name }` with the name **snapshotted** into the effect — never
    /// a `ShellFlowAction`. The Yes answer routes back through `ConfirmAnswered(true)` (unchanged).
    #[test]
    fn request_delete_confirm_arms_and_emits_show_delete_confirm() {
        let mut core = test_core();
        core.source = photos_named(&["holiday.jpg"]);
        core.displayed_item = Some(0);

        core.request_delete_confirm();

        assert_eq!(
            core.pending_confirm_delete,
            Some(0),
            "the item is armed for the Yes answer"
        );
        let name = core.effects.iter().find_map(|e| match e {
            contract::CoreEffect::ShowDeleteConfirm { name } => Some(name.clone()),
            _ => None,
        });
        assert_eq!(
            name.as_deref(),
            Some("holiday.jpg"),
            "the file name is snapshotted into the effect"
        );
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ShellFlowAction(_))),
            "no flow round-trip"
        );
    }

    /// An archive entry has no file on disk → a toast, nothing armed, no confirm.
    #[test]
    fn request_delete_confirm_refuses_an_archive_entry() {
        let mut core = test_core();
        core.native_toast = true;
        core.source = Arc::new(FakeArchive {
            names: vec!["a/photo.jpg".to_string()],
            container: std::env::temp_dir().join("deck.zip"),
        });
        core.displayed_item = Some(0);

        core.request_delete_confirm();

        assert_eq!(core.pending_confirm_delete, None, "nothing armed");
        assert_eq!(
            core.toast_native.as_ref().map(|t| t.message.as_str()),
            Some("Can't delete this")
        );
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ShowDeleteConfirm { .. })),
            "no confirm for a file-less entry"
        );
    }

    /// The dispatch arm routes `Action::DeletePermanent` through `request_delete_confirm`.
    #[test]
    fn delete_permanent_action_emits_show_delete_confirm() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.displayed_item = Some(0);

        core.handle(contract::CoreEvent::MenuAction(
            crate::action::Action::DeletePermanent,
        ));

        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::ShowDeleteConfirm { .. })));
        assert!(!core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::ShellFlowAction(_))));
    }

    /// The `do_delete` trash-refused fallback (delete.rs:90) re-offers permanent delete via
    /// `request_delete_confirm` → `ShowDeleteConfirm`, not the old `ShellFlowAction(DeletePermanent)`.
    /// Triggered deterministically by recycling a path that does not exist (the OS refuses, and
    /// nothing actually reaches the Recycle Bin).
    #[test]
    fn do_delete_trash_refused_reoffers_via_show_delete_confirm() {
        let mut core = test_core();
        let missing = std::env::temp_dir().join("pb-131-does-not-exist-holiday.jpg");
        core.source = Arc::new(FsSource::new(vec![missing.clone()]));
        core.displayed_item = Some(0);

        core.do_delete(0, &missing, false); // recoverable delete of a missing file → OS refuses

        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ShowDeleteConfirm { .. })),
            "the trash-refused fallback re-offers permanent delete"
        );
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ShellFlowAction(_))),
            "via the new effect, not the legacy flow action"
        );
    }
}
