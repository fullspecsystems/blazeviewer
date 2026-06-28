//! The native windowed-mode menu bar.
//!
//! Built with [`muda`] — a real Windows `HMENU` now, a native macOS `NSMenu`
//! later from the same API (the "macOS is a cheap port" goal). The menu exists
//! **only in windowed mode**; fullscreen is the chrome-free "speed mode" and stays
//! menu-free. A native OS menu is OS-drawn and present only when windowed, so it's
//! **perf-neutral** — consistent with the prime directive (see `CLAUDE.md`).
//!
//! Clicks emit a [`muda::MenuEvent`] carrying the item's id; `main.rs` polls the
//! event channel in `about_to_wait`, maps the id to a [`MenuAction`] via the pure
//! [`action_for`] (unit-tested here), and calls the **same `App` methods the
//! keyboard already calls**. Shortcuts are shown as label hints only (`"Open
//! File…\tO"`); no real muda accelerators are registered, because the winit key
//! handler already owns those keys — a registered accelerator would double-fire.

use muda::{Menu, MenuItem, PredefinedMenuItem, Submenu};

/// Stable string ids for the menu items. Kept in one place so the builder and the
/// dispatcher ([`action_for`]) can never drift apart.
pub mod ids {
    pub const OPEN_FILE: &str = "open_file";
    pub const OPEN_FOLDER: &str = "open_folder";
    pub const SAVE_ROTATION: &str = "save_rotation";
    pub const DELETE: &str = "delete";
    pub const DELETE_PERMANENTLY: &str = "delete_permanently";
    pub const EXIT: &str = "exit";

    pub const COPY: &str = "copy";

    pub const FIT: &str = "fit";
    pub const FILL: &str = "fill";
    pub const ORIGINAL: &str = "original";
    pub const ZOOM_IN: &str = "zoom_in";
    pub const ZOOM_OUT: &str = "zoom_out";
    pub const FULLSCREEN: &str = "fullscreen";
    pub const RECURSIVE: &str = "recursive";
    pub const INFO: &str = "info";
    pub const FULL_EXIF: &str = "full_exif";

    pub const NEXT: &str = "next";
    pub const PREVIOUS: &str = "previous";
    pub const RANDOM: &str = "random";
    pub const RANDOM_PREV: &str = "random_prev";
    pub const ROTATE_RIGHT: &str = "rotate_right";
    pub const ROTATE_LEFT: &str = "rotate_left";

    pub const HELP: &str = "help";
    pub const ABOUT: &str = "about";
}

/// A menu action — one per clickable item. Each maps to an operation the keyboard
/// already triggers; `main.rs` dispatches it to the matching `App` method. Keeping
/// the id→action step a pure function (no `App`, no muda) makes it unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    OpenFile,
    OpenFolder,
    SaveRotation,
    Delete,
    DeletePermanently,
    Exit,
    Copy,
    Fit,
    Fill,
    Original,
    ZoomIn,
    ZoomOut,
    Fullscreen,
    Recursive,
    Info,
    FullExif,
    Next,
    Previous,
    Random,
    RandomPrev,
    RotateRight,
    RotateLeft,
    Help,
    About,
}

/// Map a clicked item's id to its [`MenuAction`]. `None` for an unknown id (so a
/// stray/foreign menu event is simply ignored). Pure — the seam the tests pin.
pub fn action_for(id: &str) -> Option<MenuAction> {
    use ids::*;
    let action = match id {
        OPEN_FILE => MenuAction::OpenFile,
        OPEN_FOLDER => MenuAction::OpenFolder,
        SAVE_ROTATION => MenuAction::SaveRotation,
        DELETE => MenuAction::Delete,
        DELETE_PERMANENTLY => MenuAction::DeletePermanently,
        EXIT => MenuAction::Exit,
        COPY => MenuAction::Copy,
        FIT => MenuAction::Fit,
        FILL => MenuAction::Fill,
        ORIGINAL => MenuAction::Original,
        ZOOM_IN => MenuAction::ZoomIn,
        ZOOM_OUT => MenuAction::ZoomOut,
        FULLSCREEN => MenuAction::Fullscreen,
        RECURSIVE => MenuAction::Recursive,
        INFO => MenuAction::Info,
        FULL_EXIF => MenuAction::FullExif,
        NEXT => MenuAction::Next,
        PREVIOUS => MenuAction::Previous,
        RANDOM => MenuAction::Random,
        RANDOM_PREV => MenuAction::RandomPrev,
        ROTATE_RIGHT => MenuAction::RotateRight,
        ROTATE_LEFT => MenuAction::RotateLeft,
        HELP => MenuAction::Help,
        ABOUT => MenuAction::About,
        _ => return None,
    };
    Some(action)
}

/// One labelled item. The label may carry a shortcut hint after a tab (`"\t"`),
/// which Windows right-aligns in the accelerator column — a *hint only*, not a
/// registered accelerator (the winit key handler owns the keys).
fn item(id: &str, label: &str) -> MenuItem {
    // `None` accelerator: do NOT register a real muda accelerator, or it would
    // double-fire alongside our winit key handler.
    MenuItem::with_id(id, label, true, None)
}

/// Build the full menu bar. Best-effort: a failed `append` (rare) is logged and the
/// rest of the menu still builds, so the app never fails to start over a menu glitch.
///
/// Returns the menu plus the **Save Rotation** item, whose enabled state `main.rs`
/// toggles at runtime (only enabled when the current photo has an unsaved rotation on
/// an EXIF-writable file — see `App::refresh_save_menu_item`). It starts disabled.
pub fn build_menu() -> (Menu, MenuItem) {
    let menu = Menu::new();
    let sep = || PredefinedMenuItem::separator();

    // Disabled until a rotation is pending on an eligible file (toggled at runtime).
    let save_rotation = MenuItem::with_id(ids::SAVE_ROTATION, "Save Rotation\tCtrl+S", false, None);

    let file = Submenu::new("&File", true);
    let _ = file.append_items(&[
        &item(ids::OPEN_FILE, "Open File…\tO"),
        &item(ids::OPEN_FOLDER, "Open Folder…\tShift+O"),
        &sep(),
        &save_rotation,
        &sep(),
        &item(ids::DELETE, "Delete\tDel"),
        &item(ids::DELETE_PERMANENTLY, "Delete Permanently\tShift+Del"),
        &sep(),
        &item(ids::EXIT, "Exit\tEsc"),
    ]);

    // Edit: clipboard ops (Windows convention — Copy lives under Edit, not File).
    let edit = Submenu::new("&Edit", true);
    let _ = edit.append_items(&[&item(ids::COPY, "Copy\tCtrl+C")]);

    let view = Submenu::new("&View", true);
    let _ = view.append_items(&[
        &item(ids::FIT, "Fit\t8"),
        &item(ids::FILL, "Fill\t9"),
        &item(ids::ORIGINAL, "Original 1:1\t0"),
        &sep(),
        &item(ids::ZOOM_IN, "Zoom In\t="),
        &item(ids::ZOOM_OUT, "Zoom Out\t-"),
        &sep(),
        &item(ids::FULLSCREEN, "Fullscreen\tF11"),
        &item(ids::RECURSIVE, "Recursive Folders\tCtrl+R"),
        &sep(),
        &item(ids::INFO, "Info Panel\tI"),
        &item(ids::FULL_EXIF, "Full EXIF\tShift+I"),
    ]);

    let image = Submenu::new("&Image", true);
    let _ = image.append_items(&[
        &item(ids::NEXT, "Next\tSpace"),
        &item(ids::PREVIOUS, "Previous\tBackspace"),
        &item(ids::RANDOM, "Random\tEnter"),
        &item(ids::RANDOM_PREV, "Previous Random\tShift+Enter"),
        &sep(),
        &item(ids::ROTATE_RIGHT, "Rotate Right\tR"),
        &item(ids::ROTATE_LEFT, "Rotate Left\tShift+R"),
    ]);

    let help = Submenu::new("&Help", true);
    let _ = help.append_items(&[
        &item(ids::HELP, "Keyboard Shortcuts\t?"),
        &item(ids::ABOUT, "About PhotoBlaze"),
    ]);

    for sub in [&file, &edit, &view, &image, &help] {
        if let Err(e) = menu.append(sub) {
            eprintln!("menu: failed to append submenu: {e}");
        }
    }
    (menu, save_rotation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_for_maps_every_known_id() {
        // Each id resolves to exactly the action the keyboard path triggers.
        assert_eq!(action_for(ids::OPEN_FILE), Some(MenuAction::OpenFile));
        assert_eq!(action_for(ids::OPEN_FOLDER), Some(MenuAction::OpenFolder));
        assert_eq!(
            action_for(ids::SAVE_ROTATION),
            Some(MenuAction::SaveRotation)
        );
        assert_eq!(action_for(ids::DELETE), Some(MenuAction::Delete));
        assert_eq!(
            action_for(ids::DELETE_PERMANENTLY),
            Some(MenuAction::DeletePermanently)
        );
        assert_eq!(action_for(ids::EXIT), Some(MenuAction::Exit));
        assert_eq!(action_for(ids::COPY), Some(MenuAction::Copy));
        assert_eq!(action_for(ids::FIT), Some(MenuAction::Fit));
        assert_eq!(action_for(ids::FILL), Some(MenuAction::Fill));
        assert_eq!(action_for(ids::ORIGINAL), Some(MenuAction::Original));
        assert_eq!(action_for(ids::ZOOM_IN), Some(MenuAction::ZoomIn));
        assert_eq!(action_for(ids::ZOOM_OUT), Some(MenuAction::ZoomOut));
        assert_eq!(action_for(ids::FULLSCREEN), Some(MenuAction::Fullscreen));
        assert_eq!(action_for(ids::RECURSIVE), Some(MenuAction::Recursive));
        assert_eq!(action_for(ids::INFO), Some(MenuAction::Info));
        assert_eq!(action_for(ids::FULL_EXIF), Some(MenuAction::FullExif));
        assert_eq!(action_for(ids::NEXT), Some(MenuAction::Next));
        assert_eq!(action_for(ids::PREVIOUS), Some(MenuAction::Previous));
        assert_eq!(action_for(ids::RANDOM), Some(MenuAction::Random));
        assert_eq!(action_for(ids::RANDOM_PREV), Some(MenuAction::RandomPrev));
        assert_eq!(action_for(ids::ROTATE_RIGHT), Some(MenuAction::RotateRight));
        assert_eq!(action_for(ids::ROTATE_LEFT), Some(MenuAction::RotateLeft));
        assert_eq!(action_for(ids::HELP), Some(MenuAction::Help));
        assert_eq!(action_for(ids::ABOUT), Some(MenuAction::About));
    }

    #[test]
    fn action_for_rejects_unknown_ids() {
        assert_eq!(action_for(""), None);
        assert_eq!(action_for("not_a_real_id"), None);
        assert_eq!(action_for("OPEN_FILE"), None); // case-sensitive
    }
}
