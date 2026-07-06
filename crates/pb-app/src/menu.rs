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

// `macos_menu_chord` (the ⌘-accelerator table the keyboard-help overlay formats through) moved
// to `pb_app_core::keymap` (NS0 5.5 / Phase B) so `AppCore`'s help-overlay build can reach it.
// `build_menu` here registers the native NSMenu key-equivalents via muda's `Accelerator`
// directly, so it never needed this table.

/// Stable string ids for the menu items. Kept in one place so the builder and the
/// dispatcher ([`action_for`]) can never drift apart.
pub mod ids {
    pub const OPEN_FILE: &str = "open_file";
    pub const OPEN_FOLDER: &str = "open_folder";
    pub const CANCEL_SCAN: &str = "cancel_scan";
    pub const SAVE_ROTATION: &str = "save_rotation";
    pub const REVEAL: &str = "reveal";
    pub const DELETE: &str = "delete";
    pub const DELETE_PERMANENTLY: &str = "delete_permanently";
    pub const SETTINGS: &str = "settings";
    pub const EXIT: &str = "exit";

    pub const UNDO: &str = "undo";
    pub const COPY: &str = "copy";
    pub const COPY_PATH: &str = "copy_path";
    pub const COPY_IMAGE_DETAILS: &str = "copy_image_details";
    /// Matches `Action::CopyImageText.id()` so the Mac host's raw-id path agrees.
    pub const COPY_IMAGE_TEXT: &str = "copy_text";
    // AI image description (task #44) — ids match the corresponding `Action::*.id()`.
    pub const DESCRIBE: &str = "describe";
    pub const ASK_IMAGE: &str = "ask_image";
    pub const COPY_DESCRIPTION: &str = "copy_description";

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
    pub const FOLDER_TREE: &str = "folder_tree";
    pub const TOGGLE_PANELS: &str = "toggle_panels";
    pub const OPEN_PARENT: &str = "open_parent";
    pub const PREV_FOLDER: &str = "prev_folder";
    pub const NEXT_FOLDER: &str = "next_folder";

    pub const NEXT: &str = "next";
    pub const PREVIOUS: &str = "previous";
    pub const RANDOM: &str = "random";
    pub const RANDOM_PREV: &str = "random_prev";
    pub const ROTATE_RIGHT: &str = "rotate_right";
    pub const ROTATE_LEFT: &str = "rotate_left";
    pub const COMPARE_PIN: &str = "compare_pin";
    pub const COMPARE_TOGGLE: &str = "compare_toggle";
    pub const PLAY_PAUSE: &str = "play_pause";
    pub const FRAME_NEXT: &str = "frame_next";
    pub const FRAME_PREV: &str = "frame_prev";
    pub const MUTE_LIVE_AUDIO: &str = "mute_live_audio";

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
    Reveal,
    Delete,
    DeletePermanently,
    Settings,
    Exit,
    Undo,
    Copy,
    CopyPath,
    CopyImageDetails,
    CopyImageText,
    DescribeImage,
    AskImage,
    CopyDescription,
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
    FolderTree,
    TogglePanels,
    OpenParent,
    PrevFolder,
    NextFolder,
    Next,
    Previous,
    Random,
    RandomPrev,
    RotateRight,
    RotateLeft,
    ComparePin,
    CompareToggle,
    PlayPause,
    FrameNext,
    FramePrev,
    MuteLiveAudio,
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
            MenuAction::Reveal => Action::RevealInFileManager,
            MenuAction::Delete => Action::Delete,
            MenuAction::DeletePermanently => Action::DeletePermanent,
            MenuAction::Settings => Action::Settings,
            MenuAction::Exit => Action::Quit,
            MenuAction::Undo => Action::Undo,
            MenuAction::Copy => Action::Copy,
            MenuAction::CopyPath => Action::CopyPath,
            MenuAction::CopyImageDetails => Action::CopyImageDetails,
            MenuAction::CopyImageText => Action::CopyImageText,
            MenuAction::DescribeImage => Action::DescribeImage,
            MenuAction::AskImage => Action::AskImage,
            MenuAction::CopyDescription => Action::CopyDescription,
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
            MenuAction::FolderTree => Action::FolderTree,
            MenuAction::TogglePanels => Action::TogglePanels,
            MenuAction::OpenParent => Action::OpenParent,
            MenuAction::PrevFolder => Action::PrevFolder,
            MenuAction::NextFolder => Action::NextFolder,
            MenuAction::Next => Action::Next,
            MenuAction::Previous => Action::Prev,
            MenuAction::Random => Action::Random,
            MenuAction::RandomPrev => Action::RandomPrev,
            MenuAction::RotateRight => Action::RotateCw,
            MenuAction::RotateLeft => Action::RotateCcw,
            MenuAction::ComparePin => Action::ComparePin,
            MenuAction::CompareToggle => Action::CompareToggle,
            MenuAction::PlayPause => Action::PlayPause,
            MenuAction::FrameNext => Action::FrameNext,
            MenuAction::FramePrev => Action::FramePrev,
            MenuAction::MuteLiveAudio => Action::MuteLiveAudio,
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
        REVEAL => MenuAction::Reveal,
        DELETE => MenuAction::Delete,
        DELETE_PERMANENTLY => MenuAction::DeletePermanently,
        SETTINGS => MenuAction::Settings,
        EXIT => MenuAction::Exit,
        UNDO => MenuAction::Undo,
        COPY => MenuAction::Copy,
        COPY_PATH => MenuAction::CopyPath,
        COPY_IMAGE_DETAILS => MenuAction::CopyImageDetails,
        COPY_IMAGE_TEXT => MenuAction::CopyImageText,
        DESCRIBE => MenuAction::DescribeImage,
        ASK_IMAGE => MenuAction::AskImage,
        COPY_DESCRIPTION => MenuAction::CopyDescription,
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
        FOLDER_TREE => MenuAction::FolderTree,
        TOGGLE_PANELS => MenuAction::TogglePanels,
        OPEN_PARENT => MenuAction::OpenParent,
        PREV_FOLDER => MenuAction::PrevFolder,
        NEXT_FOLDER => MenuAction::NextFolder,
        NEXT => MenuAction::Next,
        PREVIOUS => MenuAction::Previous,
        RANDOM => MenuAction::Random,
        RANDOM_PREV => MenuAction::RandomPrev,
        ROTATE_RIGHT => MenuAction::RotateRight,
        ROTATE_LEFT => MenuAction::RotateLeft,
        COMPARE_PIN => MenuAction::ComparePin,
        COMPARE_TOGGLE => MenuAction::CompareToggle,
        PLAY_PAUSE => MenuAction::PlayPause,
        FRAME_NEXT => MenuAction::FrameNext,
        FRAME_PREV => MenuAction::FramePrev,
        MUTE_LIVE_AUDIO => MenuAction::MuteLiveAudio,
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
    /// View ▸ Hide Panels (task #54): checked while rich panels are Tab-hidden;
    /// enabled only with a panel open (mirrors the core's `hide_panels_enabled`).
    pub toggle_panels: CheckMenuItem,
    /// Checked when Live Photo audio is muted (#38), mirroring `settings.mute_live_audio`.
    pub mute_live_audio: CheckMenuItem,
}

/// Everything [`build_menu`] hands back: the menu itself plus the item handles whose
/// state the app toggles at runtime (Save Rotation's enabled flag; the View checks).
pub struct BuiltMenu {
    pub menu: Menu,
    pub save_rotation: MenuItem,
    /// The File ▸ Show in Finder/Explorer item. Returned so the app can enable it only when
    /// the displayed photo is a real on-disk file (see `AppCore::can_reveal`); it greys out
    /// for archive entries + the empty deck. Starts disabled (empty deck at launch).
    pub reveal: MenuItem,
    /// File ▸ Stop Scanning. Returned so the app can enable it only while a folder scan is
    /// streaming in (see `App::refresh_cancel_scan_menu_item`). Starts disabled.
    pub cancel_scan: MenuItem,
    /// The Edit ▸ Undo item. Returned so the app can flip its enabled state + title
    /// ("Undo" → "Undo Save Rotation") to mirror the top of the undo stack (see
    /// `App::refresh_undo_menu_item`). Starts disabled (nothing to undo at launch).
    pub undo: MenuItem,
    /// Image ▸ Pin for Compare (task #43). Checkable — checked while the displayed
    /// photo IS the pin; enabled only with a photo on screen. Starts disabled.
    pub compare_pin: CheckMenuItem,
    /// Image ▸ Compare with Pinned — the `Y` flip. Enabled once a pin exists.
    /// Starts disabled.
    pub compare_toggle: MenuItem,
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
    // Disabled until a real on-disk file is displayed (toggled at runtime).
    let reveal = MenuItem::with_id(ids::REVEAL, "Show in File Explorer", false, None);
    // Disabled until a folder scan is actually streaming in (toggled at runtime).
    let cancel_scan = MenuItem::with_id(ids::CANCEL_SCAN, "Stop Scanning", false, None);

    let file = Submenu::new("&File", true);
    let _ = file.append_items(&[
        &item(ids::OPEN_FILE, "Open File…\tO"),
        &item(ids::OPEN_FOLDER, "Open Folder…\tShift+O"),
        &cancel_scan,
        &sep(),
        &save_rotation,
        &reveal,
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
        // On-device OCR + QR payloads (task #45). Unbound by default; the hint
        // column picks up a user binding via the live keymap.
        &item(
            ids::COPY_IMAGE_TEXT,
            &labeled(keymap, "Copy Text from Image", Action::CopyImageText),
        ),
        // AI image description (task #44).
        &PredefinedMenuItem::separator(),
        &item(
            ids::DESCRIBE,
            &labeled(keymap, "Describe Image", Action::DescribeImage),
        ),
        &item(
            ids::ASK_IMAGE,
            &labeled(keymap, "Ask About Image…", Action::AskImage),
        ),
        &item(
            ids::COPY_DESCRIPTION,
            &labeled(keymap, "Copy AI Description", Action::CopyDescription),
        ),
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
    let toggle_panels = check_item(ids::TOGGLE_PANELS, "Hide Panels\tTab");
    let mute_live_audio = check_item(ids::MUTE_LIVE_AUDIO, "Mute Live Photo Audio\tM");

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
        &item(
            ids::FOLDER_TREE,
            &labeled(keymap, "Show Folder Tree", Action::FolderTree),
        ),
        &toggle_panels,
    ]);

    // Go — folder navigation (Explorer's Alt+↑/←/→ idioms; hints from the live keymap).
    let go = Submenu::new("&Go", true);
    let _ = go.append_items(&[
        &item(
            ids::OPEN_PARENT,
            &labeled(keymap, "Parent Folder", Action::OpenParent),
        ),
        &sep(),
        &item(
            ids::PREV_FOLDER,
            &labeled(keymap, "Previous Folder", Action::PrevFolder),
        ),
        &item(
            ids::NEXT_FOLDER,
            &labeled(keymap, "Next Folder", Action::NextFolder),
        ),
    ]);

    // Compare (task #43): both start disabled (empty deck at launch);
    // `apply_menu_to_native` drives enabled/checked from `MenuState`.
    let compare_pin = CheckMenuItem::with_id(
        ids::COMPARE_PIN,
        labeled(keymap, "Pin for Compare", Action::ComparePin),
        false,
        false,
        None,
    );
    let compare_toggle = MenuItem::with_id(
        ids::COMPARE_TOGGLE,
        labeled(keymap, "Compare with Pinned", Action::CompareToggle),
        false,
        None,
    );

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
        &compare_pin,
        &compare_toggle,
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
        &sep(),
        &mute_live_audio,
    ]);

    let help = Submenu::new("&Help", true);
    let _ = help.append_items(&[
        &item(ids::HELP, "Keyboard Shortcuts\t?"),
        &item(ids::ABOUT, "About PhotoBlaze"),
    ]);

    for sub in [&file, &edit, &view, &go, &image, &help] {
        if let Err(e) = menu.append(sub) {
            eprintln!("menu: failed to append submenu: {e}");
        }
    }
    BuiltMenu {
        menu,
        save_rotation,
        reveal,
        cancel_scan,
        undo,
        compare_pin,
        compare_toggle,
        checks: ViewChecks {
            fit,
            fill,
            original,
            recursive,
            fullscreen,
            slideshow,
            info,
            full_exif,
            toggle_panels,
            mute_live_audio,
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

    // Disabled until a real on-disk file is displayed (toggled at runtime). Shortcut-less —
    // ⇧⌘R is claimed by other Mac apps; a user can bind one in Settings.
    let reveal = MenuItem::with_id(ids::REVEAL, "Show in Finder", false, None);

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
        &reveal,
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
        // On-device OCR + QR payloads (task #45); bare-key/unbound, so NO
        // accelerator (an NSMenu key-equivalent would steal the key — see the
        // menu-accelerator note at the top of this file).
        &item(ids::COPY_IMAGE_TEXT, "Copy Text from Image"),
    ]);

    let fit = check_item(ids::FIT, "Fit");
    let fill = check_item(ids::FILL, "Crop to Fill");
    let original = check_item(ids::ORIGINAL, "Original 1:1");
    let recursive = check_item(ids::RECURSIVE, "Recursive (This Folder)");
    let fullscreen = check_item(ids::FULLSCREEN, "Fullscreen");
    let slideshow = check_item(ids::SLIDESHOW, "Slideshow");
    let info = check_item(ids::INFO, "Show Image Info");
    let full_exif = check_item(ids::FULL_EXIF, "Show All EXIF Info");
    let toggle_panels = check_item(ids::TOGGLE_PANELS, "Hide Panels");
    let mute_live_audio = check_item(ids::MUTE_LIVE_AUDIO, "Mute Live Photo Audio");
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
        &toggle_panels,
    ]);

    // No accelerators / no hint text: these are bare-key bindings (Space / R / , / . / …)
    // owned by the winit keymap. A macOS key-equivalent for a bare key would make NSMenu
    // *steal* it from the keymap (breaking hold-to-fly), and — unlike Windows, which
    // right-aligns a `\t` hint in the accelerator column — a literal hint in an NSMenuItem
    // title just renders as stray whitespace. So the Mac items show a plain label (the
    // idiomatic choice for a shortcut macOS can't express as a key-equivalent).
    // Compare (task #43): bare keys (Y / ⇧Y) live in the keymap, so like the other
    // Image items these carry no key-equivalent. Start disabled (empty deck).
    let compare_pin =
        CheckMenuItem::with_id(ids::COMPARE_PIN, "Pin for Compare", false, false, None);
    let compare_toggle = MenuItem::with_id(ids::COMPARE_TOGGLE, "Compare with Pinned", false, None);

    // Go — folder navigation, Finder's chords (⌘↑ Enclosing Folder, ⌘←/⌘→ step between
    // sibling folders — PhotoBlaze has no back/forward history to shadow). Real ⌘ key-
    // equivalents (NSMenu-owned); the keymap binds `Alt+arrow` for Windows and never ⌘-chords,
    // so there's no double-fire. Matches the native macOS host's Go menu.
    let go = Submenu::new("Go", true);
    let _ = go.append_items(&[
        &cmd_item(ids::OPEN_PARENT, "Enclosing Folder", CMD, Code::ArrowUp),
        &sep(),
        &cmd_item(ids::PREV_FOLDER, "Previous Folder", CMD, Code::ArrowLeft),
        &cmd_item(ids::NEXT_FOLDER, "Next Folder", CMD, Code::ArrowRight),
    ]);

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
        &compare_pin,
        &compare_toggle,
        &sep(),
        &item(ids::PLAY_PAUSE, "Play/Pause Animation"),
        &item(ids::FRAME_NEXT, "Next Frame"),
        &item(ids::FRAME_PREV, "Previous Frame"),
        &sep(),
        &mute_live_audio,
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

    // App, File, Edit, View, Go, Image, Window, Help — the conventional macOS order
    // (Go between View and Image, Window directly before Help), matching the native host.
    for sub in [&app, &file, &edit, &view, &go, &image, &window, &help] {
        if let Err(e) = menu.append(sub) {
            eprintln!("menu: failed to append submenu: {e}");
        }
    }
    BuiltMenu {
        menu,
        save_rotation,
        reveal,
        cancel_scan,
        undo,
        compare_pin,
        compare_toggle,
        checks: ViewChecks {
            fit,
            fill,
            original,
            recursive,
            fullscreen,
            slideshow,
            info,
            full_exif,
            toggle_panels,
            mute_live_audio,
        },
        window,
        native_fullscreen,
    }
}

/// Build the right-click **photo context menu** (task #41): a fresh popup of the most
/// common per-photo commands, shown over the image at the cursor. Reuses the menu-bar item
/// ids, so a click dispatches through the same [`action_for`] → [`Action`] path as the bar
/// (no parallel wiring) — muda ids need not be unique across menus. The set is curated from
/// [`ContextMenuState`](crate::contract::ContextMenuState): **Play** only when the photo has
/// motion, **Show in Finder/Explorer** only for a real on-disk file. Works in the borderless
/// fullscreen speed mode too, where the menu bar is hidden — its whole point.
pub fn build_context_menu(state: &crate::contract::ContextMenuState) -> Menu {
    let menu = Menu::new();
    let sep = || PredefinedMenuItem::separator();

    // Navigation.
    let _ = menu.append_items(&[
        &item(ids::NEXT, "Next"),
        &item(ids::PREVIOUS, "Previous"),
        &item(ids::RANDOM, "Random"),
        &item(ids::RANDOM_PREV, "Previous Random"),
        &sep(),
        // Transforms.
        &item(ids::ROTATE_LEFT, "Rotate Left"),
        &item(ids::ROTATE_RIGHT, "Rotate Right"),
    ]);
    // Compare (task #43): the pin item flips to its unpin reading on the pinned photo;
    // the flip appears only once a pin exists (matching the menu bar's enable gate).
    let pin_label = if state.compare_pinned_here {
        "Unpin from Compare"
    } else {
        "Pin for Compare"
    };
    let _ = menu.append_items(&[&sep(), &item(ids::COMPARE_PIN, pin_label)]);
    if state.compare_pinned {
        let _ = menu.append(&item(ids::COMPARE_TOGGLE, "Compare with Pinned"));
    }
    // Play/Pause only for a photo with a motion component (animated / Live Photo).
    if state.has_motion {
        let _ = menu.append(&item(ids::PLAY_PAUSE, "Play/Pause"));
    }
    // Auto-advance slideshow (a toggle — one label covers start + stop).
    let _ = menu.append_items(&[&sep(), &item(ids::SLIDESHOW, "Start/Stop Slideshow")]);
    // Clipboard group.
    let _ = menu.append_items(&[
        &sep(),
        &item(ids::COPY, "Copy Image"),
        &item(ids::COPY_PATH, "Copy File Path"),
        &item(ids::COPY_IMAGE_DETAILS, "Copy Image Details"),
        &item(ids::COPY_IMAGE_TEXT, "Copy Text from Image"),
    ]);
    // AI image description (task #44).
    let _ = menu.append_items(&[
        &sep(),
        &item(ids::DESCRIBE, "Describe Image"),
        &item(ids::ASK_IMAGE, "Ask About Image…"),
        &item(ids::COPY_DESCRIPTION, "Copy AI Description"),
    ]);
    // Reveal only for a real on-disk file (archive entries have no path). The label follows
    // the platform idiom, matching the File-menu item.
    if state.can_reveal {
        #[cfg(target_os = "macos")]
        let reveal_label = "Show in Finder";
        #[cfg(not(target_os = "macos"))]
        let reveal_label = "Show in File Explorer";
        let _ = menu.append(&item(ids::REVEAL, reveal_label));
    }
    // Fullscreen toggle, last. The label tracks the live mode — vital in the fullscreen
    // speed mode, where the menu bar is hidden and this is the only pointer route out.
    let fullscreen_label = if state.fullscreen {
        "Exit Fullscreen"
    } else {
        "Enter Fullscreen"
    };
    let _ = menu.append_items(&[&sep(), &item(ids::FULLSCREEN, fullscreen_label)]);
    menu
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
        assert_eq!(action_for(ids::REVEAL), Some(MenuAction::Reveal));
        assert_eq!(action_for(ids::DELETE), Some(MenuAction::Delete));
        assert_eq!(
            action_for(ids::DELETE_PERMANENTLY),
            Some(MenuAction::DeletePermanently)
        );
        assert_eq!(action_for(ids::SETTINGS), Some(MenuAction::Settings));
        assert_eq!(action_for(ids::EXIT), Some(MenuAction::Exit));
        assert_eq!(action_for(ids::UNDO), Some(MenuAction::Undo));
        assert_eq!(action_for(ids::COPY), Some(MenuAction::Copy));
        assert_eq!(action_for(ids::COPY_PATH), Some(MenuAction::CopyPath));
        assert_eq!(
            action_for(ids::COPY_IMAGE_DETAILS),
            Some(MenuAction::CopyImageDetails)
        );
        assert_eq!(
            action_for(ids::COPY_IMAGE_TEXT),
            Some(MenuAction::CopyImageText)
        );
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
        assert_eq!(
            action_for(ids::TOGGLE_PANELS),
            Some(MenuAction::TogglePanels)
        );
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
