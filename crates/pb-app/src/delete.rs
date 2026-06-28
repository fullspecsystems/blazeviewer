//! Delete the current photo (tasks.json #28): `Del` → Recycle Bin (recoverable, no
//! prompt), `Shift+Del` → permanent **with a confirmation**. This is the first op
//! that *removes* a user's file — like Save Rotation it only ever runs on an explicit
//! command, never as a byproduct of viewing (the "modify only on explicit command"
//! boundary in CLAUDE.md).
//!
//! The pure playlist-cursor math ([`cursor_after_removal`]) is unit-tested here; the
//! file deletion ([`send_to_trash`] / [`delete_permanently`]) is the I/O shell. The
//! permanent-delete confirmation is a themed egui dialog (`dialog::DialogKind::Confirm`,
//! dark-aware + cross-platform), and the playlist rebuild + advance live in `main.rs`
//! (they need the `PhotoSource`).

use std::path::Path;

/// Where the cursor lands after removing item `removed` from a playlist of `len`
/// items. `None` means the playlist is now empty. Otherwise the cursor stays on the
/// `removed` index — which, after the items shift down, is the **next** photo — except
/// when the **last** item was removed, where it falls back to the new last index (the
/// **previous** photo). Pure + unit-tested.
pub fn cursor_after_removal(len: usize, removed: usize) -> Option<usize> {
    if len <= 1 {
        return None;
    }
    let new_len = len - 1;
    Some(removed.min(new_len - 1))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_middle_keeps_cursor_on_the_next_photo() {
        // [0 1 2 3 4], remove 2 → [0 1 3 4]; cursor stays at 2 (was photo 3, the next).
        assert_eq!(cursor_after_removal(5, 2), Some(2));
        assert_eq!(cursor_after_removal(5, 0), Some(0));
    }

    #[test]
    fn remove_last_falls_back_to_the_previous() {
        // [0 1 2], remove 2 (the last) → [0 1]; cursor → 1 (the new last = previous).
        assert_eq!(cursor_after_removal(3, 2), Some(1));
        assert_eq!(cursor_after_removal(2, 1), Some(0));
    }

    #[test]
    fn remove_only_item_empties_the_playlist() {
        assert_eq!(cursor_after_removal(1, 0), None);
        assert_eq!(cursor_after_removal(0, 0), None);
    }

    #[test]
    fn cursor_stays_in_bounds_of_the_shrunk_list() {
        for len in 1..=10 {
            for removed in 0..len {
                if let Some(c) = cursor_after_removal(len, removed) {
                    assert!(
                        c < len - 1,
                        "cursor {c} must index the shrunk list (len {})",
                        len - 1
                    );
                }
            }
        }
    }
}
