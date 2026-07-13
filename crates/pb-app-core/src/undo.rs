//! The undo model — reversible user edits recorded on [`AppCore`](crate::AppCore)'s undo stack.

use std::path::PathBuf;

/// A reversible user edit, recorded on the undo stack (Edit ▸ Undo / `Ctrl+Z`). RAM-only,
/// dropped on exit — no on-disk undo journal (privacy #2).
///
/// Two kinds with different lifetimes across a playlist rebuild (see [`Self::survives_rebuild`]):
/// a [`SaveRotation`](Self::SaveRotation) is keyed by item **index**, so it's dropped the moment
/// the indices are reassigned; a [`Deletion`](Self::Deletion) carries a self-contained
/// Recycle-Bin handle + the path, so it survives the delete's own same-deck rebuild (that's the
/// whole point — the delete triggers the rebuild) and is only cleared when a genuinely new deck
/// opens.
pub enum UndoAction {
    /// Undo a saved EXIF rotation: restore `path`'s Orientation tag to `prev` — the value it
    /// held *before* the save. `item` is the playlist index it was saved at, used to refresh
    /// that photo's cached decode after the restore.
    SaveRotation {
        item: usize,
        path: PathBuf,
        prev: u8,
    },
    /// Undo a delete-to-Recycle-Bin: restore the file from the bin via `handle` and re-insert it
    /// into the playlist at `index` (its position when deleted, clamped to the current length).
    /// `name` is the file name, for the "Restored {name}" confirmation. Only recorded when the
    /// delete genuinely recycled to a restorable bin (Windows/Linux — see
    /// [`crate::delete::verify_recycled`]).
    Deletion {
        index: usize,
        path: PathBuf,
        name: String,
        handle: crate::delete::RestoreHandle,
    },
}

impl UndoAction {
    /// The dynamic Edit-menu title for this action (e.g. "Undo Save Rotation"), so the menu
    /// shows *what* the next undo will reverse.
    pub fn menu_label(&self) -> &'static str {
        match self {
            UndoAction::SaveRotation { .. } => "Undo Save Rotation",
            UndoAction::Deletion { .. } => "Undo Delete",
        }
    }

    /// Whether this entry stays valid after a playlist rebuild. A [`Deletion`](Self::Deletion) is
    /// path/handle-based and outlives the delete's own same-deck rebuild; a
    /// [`SaveRotation`](Self::SaveRotation) is index-based and is invalidated when the indices
    /// are reassigned. (A genuinely new deck clears *everything* regardless — see
    /// `AppCore::rebuild_playlist`.)
    pub fn survives_rebuild(&self) -> bool {
        matches!(self, UndoAction::Deletion { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_rotation_is_index_based_and_does_not_survive_a_rebuild() {
        let a = UndoAction::SaveRotation {
            item: 3,
            path: PathBuf::from("photos/a.jpg"),
            prev: 1,
        };
        assert!(!a.survives_rebuild());
        assert_eq!(a.menu_label(), "Undo Save Rotation");
    }
    // A `Deletion` can't be constructed in a unit test (its `RestoreHandle` wraps a private
    // `trash::TrashItem` with no public constructor), so its `survives_rebuild == true` /
    // "Undo Delete" label are exercised by the reinsertion + round-trip paths instead.
}
