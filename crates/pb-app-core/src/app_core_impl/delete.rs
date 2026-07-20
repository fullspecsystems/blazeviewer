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
            self.effects.push(contract::CoreEffect::ShellFlowAction(
                Action::DeletePermanent,
            ));
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
                    self.effects.push(contract::CoreEffect::ShellFlowAction(
                        Action::DeletePermanent,
                    ));
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
