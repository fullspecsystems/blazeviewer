//! The undo model — reversible user edits recorded on [`AppCore`](crate::AppCore)'s undo stack.
//!
//! **Invariant for every variant (and every future one — metadata edits, etc.):** an entry is
//! keyed by the file's **stable identity (its path)**, never by a playlist *index*. Playlist
//! indices are reassigned on every rebuild (a delete-advance, a recursive toggle, an undo-restore
//! reinsert), so an index-keyed entry silently rots the moment any *other* action reshapes the
//! deck. Path-keyed entries survive a same-deck rebuild untouched, and any transient index an
//! action needs (e.g. which cached decode to drop) is re-resolved from the path at *apply* time
//! (`AppCore::undo`). Only a genuinely new deck (a different root) clears the stack.
//!
//! This is what lets undo entries of *different kinds* coexist: you can Save Rotation, then delete
//! two photos, then `Ctrl+Z` three times (undelete, undelete, undo-rotate) and every entry is
//! still valid — none was collateral damage of the deletes' rebuilds.

use std::path::PathBuf;

/// A reversible user edit, recorded on the undo stack (Edit ▸ Undo / `Ctrl+Z`). RAM-only,
/// dropped on exit — no on-disk undo journal (privacy #2). Every variant is path-keyed so it
/// survives a same-deck rebuild (see the module invariant).
pub enum UndoAction {
    /// Undo a saved EXIF rotation: restore `path`'s Orientation tag to `prev` — the value it
    /// held *before* the save. The photo's current playlist index (to refresh its cached decode
    /// after the restore) is re-resolved from `path` at apply time, so this stays valid across an
    /// intervening delete that reshaped the deck.
    SaveRotation { path: PathBuf, prev: u8 },
    /// Undo a delete-to-Recycle-Bin: restore the file from the bin via `handle` and re-insert it
    /// into the playlist at `index` (its position when deleted, clamped to the current length).
    /// `name` is the file name, for the "Restored {name}" confirmation. Only recorded when the
    /// delete genuinely recycled to a restorable bin (every platform now — see
    /// [`crate::delete::recycle`]).
    Deletion {
        index: usize,
        path: PathBuf,
        name: String,
        handle: crate::delete::RestoreHandle,
    },
}

impl UndoAction {
    /// The dynamic Edit-menu title for this action, naming the specific file so the menu shows
    /// *exactly* what the next undo will reverse (e.g. "Undo Delete a.jpg"). A long file name is
    /// middle-elided (`elide_name`) so the menu item stays a sane width on both shells; both the
    /// winit menu (muda) and the macOS `NSMenuItem` take an arbitrary title string, so this is
    /// portable. Returns an owned `String` because the file name is per-entry, not static.
    pub fn menu_label(&self) -> String {
        match self {
            UndoAction::SaveRotation { path, .. } => {
                let name = crate::engine::file_name_of(&path.to_string_lossy());
                format!("Undo Rotate {}", elide_name(&name))
            }
            UndoAction::Deletion { name, .. } => format!("Undo Delete {}", elide_name(name)),
        }
    }
}

/// Cap a file name for the Edit ▸ Undo menu label. Names within [`MENU_NAME_MAX`] pass through;
/// longer ones are **middle-elided** — head + `…` + tail — so both the start and the extension
/// stay visible ("my_very_long_photo_na…3821.jpg"). Counts by `char` (Unicode scalar) so it never
/// splits mid-codepoint on a non-ASCII name.
fn elide_name(name: &str) -> String {
    /// Longest file name shown in full; longer names elide to exactly this many chars.
    const MENU_NAME_MAX: usize = 32;
    /// Tail kept past the ellipsis — enough for a short suffix + a 3–4 char extension.
    const TAIL: usize = 8;
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= MENU_NAME_MAX {
        return name.to_string();
    }
    let head: String = chars[..MENU_NAME_MAX - TAIL - 1].iter().collect();
    let tail: String = chars[chars.len() - TAIL..].iter().collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_rotation_labels_itself_with_the_file_name() {
        let a = UndoAction::SaveRotation {
            path: PathBuf::from("photos/a.jpg"),
            prev: 1,
        };
        assert_eq!(a.menu_label(), "Undo Rotate a.jpg");
    }

    #[test]
    fn a_short_name_passes_through_a_long_one_middle_elides() {
        assert_eq!(elide_name("a.jpg"), "a.jpg");
        assert_eq!(elide_name("IMG_1234.jpg"), "IMG_1234.jpg");
        let long = "my_very_long_vacation_photo_from_2026.jpeg";
        let out = elide_name(long);
        assert!(out.contains('…'), "long name elides: {out}");
        assert_eq!(out.chars().count(), 32, "elides to exactly the cap: {out}");
        assert!(out.starts_with("my_very_long"), "keeps the head: {out}");
        assert!(out.ends_with(".jpeg"), "keeps the extension: {out}");
    }
    // A `Deletion` can't be constructed in a unit test (its `RestoreHandle` wraps a private
    // trash handle with no public constructor), so its "Undo Delete <name>" label and
    // cross-rebuild survival are exercised by the reinsertion + round-trip paths instead.
}
