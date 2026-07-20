//! **Undo** — the `AppCore` half of [`crate::undo`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! `undo.rs` owns the undo stack; this file holds the `impl AppCore` methods that pop it and
//! put a restored item back where it belonged in the deck.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// **Undo** (`Ctrl+Z` / Edit menu) the last reversible edit. Today that's a saved rotation:
    /// restore the file's previous EXIF Orientation and refresh the caches like the save did. On
    /// an I/O failure the file is untouched, so the entry is pushed back for a retry.
    pub fn undo(&mut self) {
        let Some(action) = self.undo_stack.pop() else {
            self.show_toast("Nothing to undo");
            return;
        };
        match action {
            UndoAction::SaveRotation { path, prev } => {
                match crate::save_rotation::set_orientation(&path, prev) {
                    Ok(()) => {
                        // Re-resolve the photo's *current* index by path — an intervening delete
                        // may have reshaped the deck since the save — to drop its stale cached
                        // decode so it re-reads the reverted orientation.
                        let idx = self.index_of_path(&path);
                        if let Some(idx) = idx {
                            self.rotations.remove(&idx);
                            self.meta_cache.remove(&idx);
                            self.exif_cache.remove(&idx); // EXIF Orientation reverted on disk
                            self.failed.remove(&idx);
                            self.preview_resident.remove(&idx);
                            self.upgrade_done.remove(&idx);
                        }
                        // The reverted orientation rewrote the file's pixels → content change,
                        // REGARDLESS of whether the photo is on screen (item-6 spec Part B): the
                        // ring retains Originals across geometry changes now, so a navigated-away
                        // neighbour's resident Original would otherwise keep the stale
                        // orientation. (The pre-item-6 code only invalidated when displayed — a
                        // latent hole that retention would have promoted to a visible bug.) The
                        // purge + re-prefetch run for any in-deck photo; the synchronous reload
                        // stays displayed-only.
                        if idx.is_some() {
                            self.invalidate_content();
                            if idx == self.displayed_item {
                                self.load_current_sync();
                            }
                            self.target_item = self.playlist.current();
                            self.request_prefetch();
                        }
                        self.show_toast_icon("Rotation undone", ToastIcon::Undo);
                    }
                    Err(e) => {
                        eprintln!("undo rotation failed: {}: {e}", path.display());
                        self.show_toast("Undo failed");
                        // A transient I/O error leaves the file unchanged, so keep the entry for a
                        // retry. But a *vanished* file (e.g. permanently deleted after the
                        // rotation) is unrecoverable — drop it rather than jam the stack.
                        if path.exists() {
                            self.undo_stack
                                .push(UndoAction::SaveRotation { path, prev });
                        }
                    }
                }
            }
            // Undo a delete: restore the file from the Recycle Bin and re-insert it into the
            // playlist at its old position, navigating to it so the "Restored …" toast lands on
            // the recovered photo.
            UndoAction::Deletion {
                index,
                path,
                name,
                handle,
            } => match crate::delete::restore(handle) {
                Ok(()) => {
                    self.reinsert_after_restore(index, &path);
                    self.show_toast_icon(&format!("Restored {name}"), ToastIcon::Undo);
                }
                Err(e) => {
                    eprintln!("undo delete failed: {}: {e}", path.display());
                    // A collision (a file already occupies the original path) is the usual
                    // failure; either way the file stays recoverable in the Recycle Bin, so the
                    // entry is spent (the handle was consumed) and we just report it.
                    self.show_toast("Couldn't restore");
                }
            },
        }
    }

    /// Restore a just-undeleted file to the playlist: rebuild the `FsSource` from the current
    /// paths with `path` re-inserted at `index` (its position when deleted, clamped to the current
    /// length), and navigate to it. A same-deck rebuild — the root is unchanged — so any *other*
    /// pending Deletion undo entries survive.
    pub(super) fn reinsert_after_restore(&mut self, index: usize, path: &Path) {
        let mut paths: Vec<PathBuf> = (0..self.source.len())
            .filter_map(|i| self.source.path(i).map(Path::to_path_buf))
            .collect();
        let at = index.min(paths.len());
        paths.insert(at, path.to_path_buf());
        let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(paths));
        let root = self.root.clone();
        let scan_root = self.scan_root.clone();
        let recursive = self.recursive;
        self.rebuild_playlist(src, root, scan_root, recursive, at);
    }

    /// The current playlist index of the photo at `path`, if it's still in the deck. Undo entries
    /// are keyed by stable path (see [`crate::undo`]); this re-resolves the transient index they
    /// need at apply time, since a rebuild between record and undo reassigns indices.
    fn index_of_path(&self, path: &Path) -> Option<usize> {
        (0..self.source.len()).find(|&i| self.source.path(i) == Some(path))
    }
}
