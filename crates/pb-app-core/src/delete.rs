//! Delete the current photo (tasks.json #28): `Del` → Recycle Bin (recoverable, no
//! prompt), `Shift+Del` → permanent **with a confirmation**. This is the first op
//! that *removes* a user's file — like Save Rotation it only ever runs on an explicit
//! command, never as a byproduct of viewing (the "modify only on explicit command"
//! boundary in CLAUDE.md).
//!
//! The pure playlist-cursor math (`cursor_after_removal`) moved to `pb_app_core::engine`
//! (NS0 5.5 / Phase B) with `flush_pending_delete`; the file deletion ([`send_to_trash`] /
//! [`delete_permanently`]) is the I/O shell that stays here. The permanent-delete confirmation
//! is a themed egui dialog (`dialog::DialogKind::Confirm`), and the playlist rebuild + advance
//! live in `AppCore` now.

use std::path::Path;

/// Send `path` to the OS Recycle Bin / Trash (recoverable). No confirmation — the
/// deletion is reversible, mirroring Explorer's `Del`.
pub fn send_to_trash(path: &Path) -> Result<(), String> {
    trash::delete(path).map_err(|e| e.to_string())
}

/// Permanently delete `path` (irreversible). Reached only after the egui confirm
/// dialog answers Yes, mirroring Explorer's `Shift+Del`.
pub fn delete_permanently(path: &Path) -> Result<(), String> {
    std::fs::remove_file(path).map_err(|e| e.to_string())
}
