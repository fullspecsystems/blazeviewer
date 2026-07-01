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
//! keyboard already calls**.
//!
//! **Shortcuts, per platform.** *Windows* shows shortcuts as right-aligned label hints
//! (`"Open File…\tO"`) with no real accelerator registered — the winit key handler owns
//! those keys and a registered accelerator would double-fire. *macOS* registers **real
//! ⌘-chord key-equivalents** (Settings ⌘, / Copy ⌘C / Move to Trash ⌘⌫ / …) because the
//! keymap never binds ⌘-chords, so NSMenu owns them cleanly with no double-fire; the
//! **bare-key** items (nav, rotate, frame-step) carry **no accelerator and no hint text** —
//! an NSMenu key-equivalent for a bare key would *steal* it from the keymap (breaking
//! hold-to-fly), and literal hint text in an NSMenuItem title is non-idiomatic whitespace.

use muda::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};

use crate::action::Action;
use crate::keymap::Keymap;

/// Stable string ids for the menu items. Kept in one place so the builder and the
/// dispatcher ([`action_for`]) can never drift apart.
pub mod ids {
    pub const OPEN_FILE: &str = "open_file";
    pub const OPEN_FOLDER: &str = "open_folder";
    pub const CANCEL_SCAN: &str = "cancel_scan";
    pub const SAVE_ROTATION: &str = "save_rotation";
    pub const DELETE: &str = "delete";
    pub const DELETE_PERMANENTLY: &str = "delete_permanently";
    pub const SETTINGS: &str = "settings";
    pub const EXIT: &str = "exit";

    pub const UNDO: &str = "undo";
    pub const COPY: &str = "copy";
    pub const COPY_PATH: &str = "copy_path";

    pub const FIT: &str = "fit";
    pub const FILL: &str = "fill";
    pub const ORIGINAL: &str = "original";
    pub const ZOOM_IN: &str = "zoom_in";
    pub const ZOOM_OUT: &str = "zoom_out";
    pub const FULLSCREEN: &str = "fullscreen";
    /// macOS-only: the native (Spaces) fullscreen toggle (`toggleFullScreen:` on
    /// ⌃⌘F / Globe+F), distinct from the borderless `FULLSCREEN` speed mode.
    /// Intercepted directly in the menu-event loop, not routed through `Action`.
    /// macOS-only (both its menu item and its handler are `cfg`'d to macOS), so the
    /// constant is too — otherwise it reads as dead code on Windows/Linux.
    #[cfg(target_os = "macos")]
    pub const NATIVE_FULLSCREEN: &str = "native_fullscreen";
    pub const RECURSIVE: &str = "recursive";
    pub const SLIDESHOW: &str = "slideshow";
    pub const SLIDESHOW_FASTER: &str = "slideshow_faster";
    pub const SLIDESHOW_SLOWER: &str = "slideshow_slower";
    pub const INFO: &str = "info";
    pub const FULL_EXIF: &str = "full_exif";

    pub const NEXT: &str = "next";
    pub const PREVIOUS: &str = "previous";
    pub const RANDOM: &str = "random";
    pub const RANDOM_PREV: &str = "random_prev";
    pub const ROTATE_RIGHT: &str = "rotate_right";
    pub const ROTATE_LEFT: &str = "rotate_left";
    pub const PLAY_PAUSE: &str = "play_pause";
    pub const FRAME_NEXT: &str = "frame_next";
    pub const FRAME_PREV: &str = "frame_prev";

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
    CancelScan,
    SaveRotation,
    Delete,
    DeletePermanently,
    Settings,
    Exit,
    Undo,
    Copy,
    CopyPath,
    Fit,
    Fill,
    Original,
    ZoomIn,
    ZoomOut,
    Fullscreen,
    Recursive,
    Slideshow,
    SlideshowFaster,
    SlideshowSlower,
    Info,
    FullExif,
    Next,
    Previous,
    Random,
    RandomPrev,
    RotateRight,
    RotateLeft,
    PlayPause,
    FrameNext,
    FramePrev,
    Help,
    About,
}

impl MenuAction {
    /// Map a menu item to the central [`Action`] (task #8), so the menu dispatches
    /// through the same path as the keyboard. `Original` sets original mode (vs. the
    /// keyboard's `0`, which toggles); `Exit` is `Quit`.
    pub fn to_action(self) -> Action {
        match self {
            MenuAction::OpenFile => Action::OpenFile,
            MenuAction::OpenFolder => Action::OpenFolder,
            MenuAction::CancelScan => Action::CancelScan,
            MenuAction::SaveRotation => Action::SaveRotation,
            MenuAction::Delete => Action::Delete,
            MenuAction::DeletePermanently => Action::DeletePermanent,
            MenuAction::Settings => Action::Settings,
            MenuAction::Exit => Action::Quit,
            MenuAction::Undo => Action::Undo,
            MenuAction::Copy => Action::Copy,
            MenuAction::CopyPath => Action::CopyPath,
            MenuAction::Fit => Action::ScaleFit,
            MenuAction::Fill => Action::ScaleFill,
            MenuAction::Original => Action::ScaleOriginal,
            MenuAction::ZoomIn => Action::ZoomIn,
            MenuAction::ZoomOut => Action::ZoomOut,
            MenuAction::Fullscreen => Action::Fullscreen,
            MenuAction::Recursive => Action::Recursive,
            MenuAction::Slideshow => Action::SlideshowToggle,
            MenuAction::SlideshowFaster => Action::SlideshowFaster,
            MenuAction::SlideshowSlower => Action::SlideshowSlower,
            MenuAction::Info => Action::Info,
            MenuAction::FullExif => Action::FullExif,
            MenuAction::Next => Action::Next,
            MenuAction::Previous => Action::Prev,
            MenuAction::Random => Action::Random,
            MenuAction::RandomPrev => Action::RandomPrev,
            MenuAction::RotateRight => Action::RotateCw,
            MenuAction::RotateLeft => Action::RotateCcw,
            MenuAction::PlayPause => Action::PlayPause,
            MenuAction::FrameNext => Action::FrameNext,
            MenuAction::FramePrev => Action::FramePrev,
            MenuAction::Help => Action::Help,
            MenuAction::About => Action::About,
        }
    }
}

/// Map a clicked item's id to its [`MenuAction`]. `None` for an unknown id (so a
/// stray/foreign menu event is simply ignored). Pure — the seam the tests pin.
pub fn action_for(id: &str) -> Option<MenuAction> {
    use ids::*;
    let action = match id {
        OPEN_FILE => MenuAction::OpenFile,
        OPEN_FOLDER => MenuAction::OpenFolder,
        CANCEL_SCAN => MenuAction::CancelScan,
        SAVE_ROTATION => MenuAction::SaveRotation,
        DELETE => MenuAction::Delete,
        DELETE_PERMANENTLY => MenuAction::DeletePermanently,
        SETTINGS => MenuAction::Settings,
        EXIT => MenuAction::Exit,
        UNDO => MenuAction::Undo,
        COPY => MenuAction::Copy,
        COPY_PATH => MenuAction::CopyPath,
        FIT => MenuAction::Fit,
        FILL => MenuAction::Fill,
        ORIGINAL => MenuAction::Original,
        ZOOM_IN => MenuAction::ZoomIn,
        ZOOM_OUT => MenuAction::ZoomOut,
        FULLSCREEN => MenuAction::Fullscreen,
        RECURSIVE => MenuAction::Recursive,
        SLIDESHOW => MenuAction::Slideshow,
        SLIDESHOW_FASTER => MenuAction::SlideshowFaster,
        SLIDESHOW_SLOWER => MenuAction::SlideshowSlower,
        INFO => MenuAction::Info,
        FULL_EXIF => MenuAction::FullExif,
        NEXT => MenuAction::Next,
        PREVIOUS => MenuAction::Previous,
        RANDOM => MenuAction::Random,
        RANDOM_PREV => MenuAction::RandomPrev,
        ROTATE_RIGHT => MenuAction::RotateRight,
        ROTATE_LEFT => MenuAction::RotateLeft,
        PLAY_PAUSE => MenuAction::PlayPause,
        FRAME_NEXT => MenuAction::FrameNext,
        FRAME_PREV => MenuAction::FramePrev,
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

/// A checkable item (View menu: scale mode, recursive, fullscreen). Same "label may
/// carry a `\t` hint, no real accelerator" rule as [`item`]. Starts unchecked; the
/// app pushes the live state via [`ViewChecks`] right after the menu attaches, so the
/// initial value here doesn't matter.
fn check_item(id: &str, label: &str) -> CheckMenuItem {
    CheckMenuItem::with_id(id, label, true, false, None)
}

/// The user's **current** shortcut for `action` as a display string (e.g. `"Space"`,
/// `"Shift+R"`, `"P"`), or empty if the action is unbound. Sourced live from the keymap so
/// the menu reflects **customized** bindings (`KeyChord`'s `Display` — the same text the
/// Settings ▸ Shortcuts editor shows), never a hardcoded guess. The first binding wins when
/// an action has two (menus show one accelerator). Windows-only in production (macOS shows
/// bare-key items with no hint — see [`build_menu`]); kept + tested cross-platform.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn shortcut_hint(keymap: &Keymap, action: Action) -> String {
    keymap
        .bindings_for(action)
        .first()
        .map(|c| c.to_string())
        .unwrap_or_default()
}

/// A menu label with the action's current shortcut appended as a `\t` hint
/// (`"Next\tSpace"`), which **Windows** right-aligns in the accelerator column. Sourced
/// live from the keymap so it tracks customized bindings. No hint if the action is unbound,
/// so an un-shortcutted item just reads as its plain label. **Windows-only:** on macOS these
/// bare-key items show no hint at all (a `\t` in an NSMenuItem title is just stray
/// whitespace, and a real key-equivalent would hijack the key — see the module docs), so
/// this is unused there.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn labeled(keymap: &Keymap, base: &str, action: Action) -> String {
    let hint = shortcut_hint(keymap, action);
    if hint.is_empty() {
        base.to_string()
    } else {
        format!("{base}\t{hint}")
    }
}

/// Handles to the View-menu checkable items, returned from [`build_menu`] so the app
/// can mirror its live state onto them (see `App::refresh_view_menu_checks`). Scale
/// mode is a one-of-three group (exactly one checked); `recursive`/`fullscreen` are
/// independent toggles.
pub struct ViewChecks {
    pub fit: CheckMenuItem,
    pub fill: CheckMenuItem,
    pub original: CheckMenuItem,
    pub recursive: CheckMenuItem,
    pub fullscreen: CheckMenuItem,
    pub slideshow: CheckMenuItem,
    /// The two info overlays are mutually exclusive toggles (basic vs. full EXIF);
    /// exactly one — or neither — is checked, mirroring `App::info`.
    pub info: CheckMenuItem,
    pub full_exif: CheckMenuItem,
}

/// Everything [`build_menu`] hands back: the menu itself plus the item handles whose
/// state the app toggles at runtime (Save Rotation's enabled flag; the View checks).
pub struct BuiltMenu {
    pub menu: Menu,
    pub save_rotation: MenuItem,
    /// File ▸ Stop Scanning. Returned so the app can enable it only while a folder scan is
    /// streaming in (see `App::refresh_cancel_scan_menu_item`). Starts disabled.
    pub cancel_scan: MenuItem,
    /// The Edit ▸ Undo item. Returned so the app can flip its enabled state + title
    /// ("Undo" → "Undo Save Rotation") to mirror the top of the undo stack (see
    /// `App::refresh_undo_menu_item`). Starts disabled (nothing to undo at launch).
    pub undo: MenuItem,
    pub checks: ViewChecks,
    /// macOS-only: the **Window** submenu (Minimize ⌘M / Zoom / Bring All to Front).
    /// Returned so the app can call [`Submenu::set_as_windows_menu_for_nsapp`] on it —
    /// which muda requires be done *after* `Menu::init_for_nsapp` — to make macOS
    /// auto-populate the standard window list. There's no Window menu on Windows.
    #[cfg(target_os = "macos")]
    pub window: Submenu,
    /// macOS-only: the native (Spaces) fullscreen item. Returned so the app can flip
    /// its title between "Enter Full Screen" and "Exit Full Screen" to mirror the live
    /// native-fullscreen state (the Mac-standard behavior — no checkmark; see
    /// `App::refresh_native_fullscreen_label`). macOS also auto-injects its own
    /// Globe/Fn+F fullscreen item, which we can't suppress (muda gives no access to the
    /// raw `NSMenuItem` to wire the native `toggleFullScreen:` action), so this carries
    /// the ⌃⌘F shortcut + the label management the auto item won't do for us.
    #[cfg(target_os = "macos")]
    pub native_fullscreen: MenuItem,
}

/// Build the full menu bar. Best-effort: a failed `append` (rare) is logged and the
/// rest of the menu still builds, so the app never fails to start over a menu glitch.
///
/// Returns the menu plus the item handles `main.rs` toggles at runtime: the **Save
/// Rotation** item (enabled only when the current photo has an unsaved rotation on an
/// EXIF-writable file — see `App::refresh_save_menu_item`; starts disabled), and the
/// **View checks** (scale mode / recursive / fullscreen — see
/// `App::refresh_view_menu_checks`).
#[cfg(not(target_os = "macos"))]
pub fn build_menu(keymap: &Keymap) -> BuiltMenu {
    let menu = Menu::new();
    let sep = || PredefinedMenuItem::separator();

    // Disabled until a rotation is pending on an eligible file (toggled at runtime).
    let save_rotation = MenuItem::with_id(ids::SAVE_ROTATION, "Save Rotation\tCtrl+S", false, None);
    // Disabled until a folder scan is actually streaming in (toggled at runtime).
    let cancel_scan = MenuItem::with_id(ids::CANCEL_SCAN, "Stop Scanning", false, None);

    let file = Submenu::new("&File", true);
    let _ = file.append_items(&[
        &item(ids::OPEN_FILE, "Open File…\tO"),
        &item(ids::OPEN_FOLDER, "Open Folder…\tShift+O"),
        &cancel_scan,
        &sep(),
        &save_rotation,
        &sep(),
        &item(ids::DELETE, "Delete\tDel"),
        &item(ids::DELETE_PERMANENTLY, "Delete Permanently\tShift+Del"),
        &sep(),
        &item(ids::SETTINGS, "Settings…\tCtrl+,"),
        &sep(),
        &item(ids::EXIT, "Exit\tEsc"),
    ]);

    // Edit: undo (top, the convention) + clipboard ops (Windows convention — Copy lives
    // under Edit, not File). Undo starts disabled; the app toggles its label + enabled
    // state to mirror the undo stack (see `App::refresh_undo_menu_item`).
    let undo = MenuItem::with_id(ids::UNDO, "Undo\tCtrl+Z", false, None);
    let edit = Submenu::new("&Edit", true);
    let _ = edit.append_items(&[
        &undo,
        &PredefinedMenuItem::separator(),
        &item(ids::COPY, "Copy\tCtrl+C"),
        &item(ids::COPY_PATH, "Copy File Path\tShift+Ctrl+C"),
    ]);

    // Scale mode is a one-of-three group; recursive/fullscreen are toggles. All five
    // are checkable so the View menu shows the current state (handles returned below).
    let fit = check_item(ids::FIT, "Fit\t8");
    let fill = check_item(ids::FILL, "Crop to Fill\t9");
    let original = check_item(ids::ORIGINAL, "Original 1:1\t0");
    let recursive = check_item(ids::RECURSIVE, "Recursive (This Folder)\tCtrl+R");
    let fullscreen = check_item(ids::FULLSCREEN, "Fullscreen\tF11");
    let slideshow = check_item(ids::SLIDESHOW, "Slideshow\tS");
    let info = check_item(ids::INFO, "Show Image Info\tI");
    let full_exif = check_item(ids::FULL_EXIF, "Show All EXIF Info\tShift+I");

    let view = Submenu::new("&View", true);
    let _ = view.append_items(&[
        &fit,
        &fill,
        &original,
        &sep(),
        &item(ids::ZOOM_IN, "Zoom In\t="),
        &item(ids::ZOOM_OUT, "Zoom Out\t-"),
        &sep(),
        &fullscreen,
        &recursive,
        &slideshow,
        &item(ids::SLIDESHOW_FASTER, "Slideshow Faster\t["),
        &item(ids::SLIDESHOW_SLOWER, "Slideshow Slower\t]"),
        &sep(),
        &info,
        &full_exif,
    ]);

    let image = Submenu::new("&Image", true);
    let _ = image.append_items(&[
        &item(ids::NEXT, &labeled(keymap, "Next", Action::Next)),
        &item(ids::PREVIOUS, &labeled(keymap, "Previous", Action::Prev)),
        &item(ids::RANDOM, &labeled(keymap, "Random", Action::Random)),
        &item(
            ids::RANDOM_PREV,
            &labeled(keymap, "Previous Random", Action::RandomPrev),
        ),
        &sep(),
        &item(
            ids::ROTATE_RIGHT,
            &labeled(keymap, "Rotate Right", Action::RotateCw),
        ),
        &item(
            ids::ROTATE_LEFT,
            &labeled(keymap, "Rotate Left", Action::RotateCcw),
        ),
        &sep(),
        &item(
            ids::PLAY_PAUSE,
            &labeled(keymap, "Play/Pause Animation", Action::PlayPause),
        ),
        &item(
            ids::FRAME_NEXT,
            &labeled(keymap, "Next Frame", Action::FrameNext),
        ),
        &item(
            ids::FRAME_PREV,
            &labeled(keymap, "Previous Frame", Action::FramePrev),
        ),
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
    BuiltMenu {
        menu,
        save_rotation,
        cancel_scan,
        undo,
        checks: ViewChecks {
            fit,
            fill,
            original,
            recursive,
            fullscreen,
            slideshow,
            info,
            full_exif,
        },
    }
}

/// The macOS menu bar — same item ids (so [`action_for`] / dispatch are shared), but
/// built to Apple conventions: a leading **application menu** (the first submenu
/// becomes the bold app menu under `init_for_nsapp`), with About / Settings / Quit
/// there rather than in File, and **real ⌘ accelerators** instead of the Windows
/// hint-text. The accelerators are safe to register here (unlike Windows) because the
/// winit keymap never binds ⌘-chords (`KeyChord.logo`), so NSMenu owns them with no
/// double-fire. Bare-key fast-nav (Space / R / 8-9-0 / …) and the fullscreen toggles
/// (F / ⌥⏎ / F11) stay keymap-owned, so those items carry no accelerator.
#[cfg(target_os = "macos")]
pub fn build_menu(_keymap: &Keymap) -> BuiltMenu {
    use muda::accelerator::{Accelerator, Code, Modifiers};
    /// The ⌘ (Command) key — `Modifiers::SUPER` maps to NSEventModifierFlags::Command.
    const CMD: Modifiers = Modifiers::SUPER;
    /// A normal item carrying a real key-equivalent (NSMenu dispatches it → MenuEvent).
    fn cmd_item(id: &str, label: &str, mods: Modifiers, code: Code) -> MenuItem {
        MenuItem::with_id(id, label, true, Some(Accelerator::new(Some(mods), code)))
    }

    let menu = Menu::new();
    let sep = || PredefinedMenuItem::separator();

    // 1) Application menu. The FIRST submenu is rendered as the macOS app menu (bold,
    //    app-named). About / Settings / Quit live here per convention — not in File.
    //    Quit routes through our own id (→ Action::Quit → clean teardown, privacy #6)
    //    rather than PredefinedMenuItem::quit, which would bypass it.
    let app = Submenu::new("PhotoBlaze", true);
    let _ = app.append_items(&[
        &item(ids::ABOUT, "About PhotoBlaze"),
        &sep(),
        &cmd_item(ids::SETTINGS, "Settings…", CMD, Code::Comma),
        &sep(),
        &PredefinedMenuItem::hide(None),
        &PredefinedMenuItem::hide_others(None),
        &PredefinedMenuItem::show_all(None),
        &sep(),
        &cmd_item(ids::EXIT, "Quit PhotoBlaze", CMD, Code::KeyQ),
    ]);

    // Disabled until a rotation is pending on an eligible file (toggled at runtime).
    let save_rotation = MenuItem::with_id(
        ids::SAVE_ROTATION,
        "Save Rotation",
        false,
        Some(Accelerator::new(Some(CMD), Code::KeyS)),
    );

    // Disabled until a folder scan is actually streaming in (toggled at runtime). No
    // accelerator — it's a contextual command, menu-only.
    let cancel_scan = MenuItem::with_id(ids::CANCEL_SCAN, "Stop Scanning", false, None);

    let file = Submenu::new("File", true);
    let _ = file.append_items(&[
        &cmd_item(ids::OPEN_FILE, "Open File…", CMD, Code::KeyO),
        &cmd_item(
            ids::OPEN_FOLDER,
            "Open Folder…",
            CMD.union(Modifiers::SHIFT),
            Code::KeyO,
        ),
        &cancel_scan,
        &sep(),
        &save_rotation,
        &sep(),
        // macOS Finder idioms: Move to Trash = ⌘⌫, Delete Immediately = ⌥⌘⌫ (NOT ⇧⌘⌫,
        // which Finder maps to *Empty Trash*). These are ⌘-chords, so NSMenu owns them
        // with no double-fire against the keymap's bare Del / Shift+Del (`KeyChord.logo`).
        // `Code::Backspace` renders as the ⌫ glyph (muda → key-equivalent `\u{0008}`).
        &cmd_item(ids::DELETE, "Move to Trash", CMD, Code::Backspace),
        &cmd_item(
            ids::DELETE_PERMANENTLY,
            "Delete Immediately…",
            CMD.union(Modifiers::ALT),
            Code::Backspace,
        ),
    ]);

    // Undo (⌘Z) at the top of Edit, per macOS convention. Starts disabled; the app
    // flips its label + enabled state to mirror the undo stack.
    let undo = MenuItem::with_id(
        ids::UNDO,
        "Undo",
        false,
        Some(Accelerator::new(Some(CMD), Code::KeyZ)),
    );
    let edit = Submenu::new("Edit", true);
    let _ = edit.append_items(&[
        &undo,
        &sep(),
        &cmd_item(ids::COPY, "Copy", CMD, Code::KeyC),
        &cmd_item(
            ids::COPY_PATH,
            "Copy File Path",
            CMD.union(Modifiers::SHIFT),
            Code::KeyC,
        ),
    ]);

    let fit = check_item(ids::FIT, "Fit");
    let fill = check_item(ids::FILL, "Crop to Fill");
    let original = check_item(ids::ORIGINAL, "Original 1:1");
    let recursive = check_item(ids::RECURSIVE, "Recursive (This Folder)");
    let fullscreen = check_item(ids::FULLSCREEN, "Fullscreen");
    let slideshow = check_item(ids::SLIDESHOW, "Slideshow");
    let info = check_item(ids::INFO, "Show Image Info");
    let full_exif = check_item(ids::FULL_EXIF, "Show All EXIF Info");
    // Native (Spaces) fullscreen. Its title flips to "Exit Full Screen" while engaged
    // (Mac convention — no checkmark), driven by `App::refresh_native_fullscreen_label`.
    let native_fullscreen = cmd_item(
        ids::NATIVE_FULLSCREEN,
        "Enter Full Screen",
        CMD.union(Modifiers::CONTROL),
        Code::KeyF,
    );

    let view = Submenu::new("View", true);
    let _ = view.append_items(&[
        &fit,
        &fill,
        &original,
        &sep(),
        &item(ids::ZOOM_IN, "Zoom In"),
        &item(ids::ZOOM_OUT, "Zoom Out"),
        &sep(),
        // Two fullscreen modes (owner decision): our borderless speed mode (checkable,
        // bound to F / ⌥⏎ / F11 in the keymap), and the macOS-native Spaces fullscreen
        // (⌃⌘F) for those who want it. `SUPER` maps to ⌘ (muda's `META` does not — see
        // modifier_mask), so this is a real ⌃⌘F. macOS *also* auto-injects its own
        // Globe/Fn+F fullscreen item at the menu's end (a duplicate we can't suppress
        // via muda); ours is the one carrying ⌃⌘F + the Enter/Exit label management.
        &fullscreen,
        &native_fullscreen,
        &recursive,
        &slideshow,
        &item(ids::SLIDESHOW_FASTER, "Slideshow Faster"),
        &item(ids::SLIDESHOW_SLOWER, "Slideshow Slower"),
        &sep(),
        &info,
        &full_exif,
    ]);

    // No accelerators / no hint text: these are bare-key bindings (Space / R / , / . / …)
    // owned by the winit keymap. A macOS key-equivalent for a bare key would make NSMenu
    // *steal* it from the keymap (breaking hold-to-fly), and — unlike Windows, which
    // right-aligns a `\t` hint in the accelerator column — a literal hint in an NSMenuItem
    // title just renders as stray whitespace. So the Mac items show a plain label (the
    // idiomatic choice for a shortcut macOS can't express as a key-equivalent).
    let image = Submenu::new("Image", true);
    let _ = image.append_items(&[
        &item(ids::NEXT, "Next"),
        &item(ids::PREVIOUS, "Previous"),
        &item(ids::RANDOM, "Random"),
        &item(ids::RANDOM_PREV, "Previous Random"),
        &sep(),
        &item(ids::ROTATE_RIGHT, "Rotate Right"),
        &item(ids::ROTATE_LEFT, "Rotate Left"),
        &sep(),
        &item(ids::PLAY_PAUSE, "Play/Pause Animation"),
        &item(ids::FRAME_NEXT, "Next Frame"),
        &item(ids::FRAME_PREV, "Previous Frame"),
    ]);

    // Standard macOS Window menu. The predefined items carry their native labels,
    // selectors and ⌘-equivalents for free: Minimize = ⌘M (`performMiniaturize:`),
    // Zoom (`performZoom:`), Bring All to Front (`arrangeInFront:`). Marking it the
    // app's Window menu (`set_as_windows_menu_for_nsapp`, done in `apply_menu_for_mode`
    // after `init_for_nsapp`) lets macOS append the live window list below these.
    let window = Submenu::new("Window", true);
    let _ = window.append_items(&[
        &PredefinedMenuItem::minimize(None),
        &PredefinedMenuItem::maximize(None),
        &sep(),
        &PredefinedMenuItem::bring_all_to_front(None),
    ]);

    let help = Submenu::new("Help", true);
    let _ = help.append_items(&[&item(ids::HELP, "Keyboard Shortcuts")]);

    // App, File, Edit, View, Image, Window, Help — the conventional macOS order
    // (Window directly before Help).
    for sub in [&app, &file, &edit, &view, &image, &window, &help] {
        if let Err(e) = menu.append(sub) {
            eprintln!("menu: failed to append submenu: {e}");
        }
    }
    BuiltMenu {
        menu,
        save_rotation,
        cancel_scan,
        undo,
        checks: ViewChecks {
            fit,
            fill,
            original,
            recursive,
            fullscreen,
            slideshow,
            info,
            full_exif,
        },
        window,
        native_fullscreen,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::KeyChord;

    #[test]
    fn image_labels_carry_the_users_keymap_shortcut() {
        let km = Keymap::defaults();
        // The animation actions the Image menu added surface their default chords…
        assert_eq!(shortcut_hint(&km, Action::PlayPause), "P");
        assert_eq!(shortcut_hint(&km, Action::FrameNext), ".");
        assert_eq!(shortcut_hint(&km, Action::FramePrev), ",");
        // …and a label reads "base\thint" (a bound nav action too).
        assert_eq!(
            labeled(&km, "Play/Pause Animation", Action::PlayPause),
            "Play/Pause Animation\tP",
        );
        assert!(labeled(&km, "Next", Action::Next).starts_with("Next\t"));

        // It tracks a *customized* binding, not a hardcoded guess.
        let mut custom = Keymap::defaults();
        let chord = KeyChord::parse("Shift+P").expect("valid chord");
        custom.set_slot(Action::PlayPause, 0, chord);
        assert_eq!(shortcut_hint(&custom, Action::PlayPause), chord.to_string());
        assert_eq!(
            labeled(&custom, "Play/Pause Animation", Action::PlayPause),
            format!("Play/Pause Animation\t{chord}"),
        );

        // An unbound action falls back to the plain label (no trailing tab).
        let mut cleared = Keymap::defaults();
        cleared.clear_slot(Action::FrameNext, 0);
        assert_eq!(
            labeled(&cleared, "Next Frame", Action::FrameNext),
            "Next Frame"
        );
    }

    #[test]
    fn action_for_maps_every_known_id() {
        // Each id resolves to exactly the action the keyboard path triggers.
        assert_eq!(action_for(ids::OPEN_FILE), Some(MenuAction::OpenFile));
        assert_eq!(action_for(ids::OPEN_FOLDER), Some(MenuAction::OpenFolder));
        assert_eq!(action_for(ids::CANCEL_SCAN), Some(MenuAction::CancelScan));
        assert_eq!(
            action_for(ids::SAVE_ROTATION),
            Some(MenuAction::SaveRotation)
        );
        assert_eq!(action_for(ids::DELETE), Some(MenuAction::Delete));
        assert_eq!(
            action_for(ids::DELETE_PERMANENTLY),
            Some(MenuAction::DeletePermanently)
        );
        assert_eq!(action_for(ids::SETTINGS), Some(MenuAction::Settings));
        assert_eq!(action_for(ids::EXIT), Some(MenuAction::Exit));
        assert_eq!(action_for(ids::UNDO), Some(MenuAction::Undo));
        assert_eq!(action_for(ids::COPY), Some(MenuAction::Copy));
        assert_eq!(action_for(ids::FIT), Some(MenuAction::Fit));
        assert_eq!(action_for(ids::FILL), Some(MenuAction::Fill));
        assert_eq!(action_for(ids::ORIGINAL), Some(MenuAction::Original));
        assert_eq!(action_for(ids::ZOOM_IN), Some(MenuAction::ZoomIn));
        assert_eq!(action_for(ids::ZOOM_OUT), Some(MenuAction::ZoomOut));
        assert_eq!(action_for(ids::FULLSCREEN), Some(MenuAction::Fullscreen));
        assert_eq!(action_for(ids::RECURSIVE), Some(MenuAction::Recursive));
        assert_eq!(action_for(ids::SLIDESHOW), Some(MenuAction::Slideshow));
        assert_eq!(
            action_for(ids::SLIDESHOW_FASTER),
            Some(MenuAction::SlideshowFaster)
        );
        assert_eq!(
            action_for(ids::SLIDESHOW_SLOWER),
            Some(MenuAction::SlideshowSlower)
        );
        assert_eq!(action_for(ids::INFO), Some(MenuAction::Info));
        assert_eq!(action_for(ids::FULL_EXIF), Some(MenuAction::FullExif));
        assert_eq!(action_for(ids::NEXT), Some(MenuAction::Next));
        assert_eq!(action_for(ids::PREVIOUS), Some(MenuAction::Previous));
        assert_eq!(action_for(ids::RANDOM), Some(MenuAction::Random));
        assert_eq!(action_for(ids::RANDOM_PREV), Some(MenuAction::RandomPrev));
        assert_eq!(action_for(ids::ROTATE_RIGHT), Some(MenuAction::RotateRight));
        assert_eq!(action_for(ids::ROTATE_LEFT), Some(MenuAction::RotateLeft));
        assert_eq!(action_for(ids::PLAY_PAUSE), Some(MenuAction::PlayPause));
        assert_eq!(action_for(ids::FRAME_NEXT), Some(MenuAction::FrameNext));
        assert_eq!(action_for(ids::FRAME_PREV), Some(MenuAction::FramePrev));
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
