//! **Save rotation** — the `AppCore` half of [`crate::save_rotation`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! `save_rotation.rs` writes the EXIF Orientation (or a sidecar); this file holds the
//! `impl AppCore` methods that decide whether the displayed item can be saved and invoke it.
//!
//! ⚠ Per-image `rotations` stay RAM-only until the user explicitly chooses Save — that
//! boundary is the privacy guarantee, not an implementation detail.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Whether **Save Rotation** is available for the displayed photo: it has a pending in-RAM
    /// rotation *and* its source is a writable-orientation file (JPEG on disk, not an archive
    /// entry). Drives the Edit-menu item's enabled state (`apply_menu_state`).
    pub fn can_save_rotation(&self) -> bool {
        let Some(item) = self.displayed_item else {
            return false;
        };
        let rotated = self
            .rotations
            .get(&item)
            .is_some_and(|r| *r != Rotation::default());
        rotated
            && self
                .source
                .path(item)
                .is_some_and(crate::save_rotation::is_orientation_writable)
    }

    /// **Save Rotation** (`Ctrl+S` / Edit menu): bake the displayed photo's pending in-RAM
    /// rotation into its file's EXIF Orientation, then drop the RAM override + caches and re-read
    /// from disk so the pixels are re-oriented from the file (else a later re-decode would
    /// double-rotate). Records an undo entry. A deliberate, user-initiated write to the user's own
    /// file — never the passive view path (privacy #2). The EXIF write is platform-neutral
    /// `std::fs` (`crate::save_rotation`), so this is a pure core arm.
    pub fn save_rotation(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        let rot = self.rotations.get(&item).copied().unwrap_or_default();
        if rot == Rotation::default() {
            self.show_toast("No rotation to save");
            return;
        }
        let Some(path) = self.source.path(item).map(Path::to_path_buf) else {
            // Archive entry — no file on disk to write back to.
            self.show_toast("Can't save rotation here");
            return;
        };
        // Videos never persist a rotation (task #79 action matrix): footage can rotate
        // mid-clip, so there is no single correct value to write. The in-memory display
        // rotation stays available (and stays live during playback).
        match crate::video::item_kind(self.source.as_ref(), item) {
            crate::video::LibraryItemKind::Video(_) => {
                self.show_toast("Can't save rotation for video");
                return;
            }
            // A door is a place, not a picture — there is no orientation to write.
            crate::video::LibraryItemKind::Archive(_) => {
                self.show_toast("Can't save rotation for an archive");
                return;
            }
            // Exhaustive so a new kind states its own answer: what follows assumes a
            // rotatable image file on disk.
            crate::video::LibraryItemKind::Image => {}
        }
        if !crate::save_rotation::is_orientation_writable(&path) {
            self.show_toast("Save rotation: JPEG only");
            return;
        }
        // Capture the file's orientation *before* the write so the save can be reversed
        // (Edit ▸ Undo) by restoring this exact value.
        let prev = crate::save_rotation::read_orientation(&path);
        match crate::save_rotation::write_orientation(&path, rot) {
            Ok(_) => {
                // The rotation is now baked into the file's EXIF: drop the RAM override and
                // re-read from disk so the pixels are re-oriented from the file.
                self.rotations.remove(&item);
                self.meta_cache.remove(&item);
                self.exif_cache.remove(&item); // the file's EXIF (Orientation) just changed
                self.failed.remove(&item);
                self.preview_resident.remove(&item);
                self.upgrade_done.remove(&item);
                // A saved rotation rewrites the file's pixels-as-displayed → content change.
                self.invalidate_content();
                self.load_current_sync();
                self.target_item = self.playlist.current();
                self.request_prefetch();
                self.undo_stack.push(UndoAction::SaveRotation {
                    path: path.clone(),
                    prev,
                });
                self.show_toast_icon("Saved rotation", ToastIcon::Save);
            }
            Err(e) => {
                eprintln!("save rotation failed: {}: {e}", path.display());
                self.show_toast("Save failed");
            }
        }
    }
}
