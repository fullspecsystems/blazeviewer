//! The undo model — reversible user edits recorded on [`AppCore`](crate::AppCore)'s undo stack.

use std::path::PathBuf;

/// A reversible user edit, recorded on the undo stack (Edit ▸ Undo / `Ctrl+Z`). RAM-only,
/// dropped on exit — no on-disk undo journal (privacy #2). The stack is cleared whenever the
/// playlist/source changes (open, delete-rebuild, empty state), so a recorded `item` index
/// always indexes the current source. Only a saved rotation is reversible today; deletes
/// (recoverable / permanent) are a future / never extension of the same stack.
pub enum UndoAction {
    /// Undo a saved EXIF rotation: restore `path`'s Orientation tag to `prev` — the value it
    /// held *before* the save. `item` is the playlist index it was saved at, used to refresh
    /// that photo's cached decode after the restore.
    SaveRotation {
        item: usize,
        path: PathBuf,
        prev: u8,
    },
}

impl UndoAction {
    /// The dynamic Edit-menu title for this action (e.g. "Undo Save Rotation"), so the menu
    /// shows *what* the next undo will reverse.
    pub fn menu_label(&self) -> &'static str {
        match self {
            UndoAction::SaveRotation { .. } => "Undo Save Rotation",
        }
    }
}
