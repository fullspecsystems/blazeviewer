//! The **Windows** native windowed-mode menu bar, plus the shared cross-platform menu model.
//!
//! Built with [`muda`] — a real Windows `HMENU`. The menu exists **only in windowed mode**;
//! fullscreen is the chrome-free "speed mode" and stays menu-free. A native OS menu is
//! OS-drawn and present only when windowed, so it's **perf-neutral** (see `CLAUDE.md`).
//! macOS ships the native SwiftUI host (which builds its own AppKit menu); Linux draws an
//! egui menu (`panels_ui`). Both consume the dependency-free model in this module — the
//! id → [`action_for`] mapping, the group/row spec, and the keyboard-nav state machine — so
//! only the native muda *construction* ([`build_menu`]) is Windows-specific. (The `#[cfg(windows)]`
//! split that stops Linux compiling `build_menu` + muda at all is a follow-up — task #73.)
//!
//! Clicks emit a [`muda::MenuEvent`] carrying the item's id; `main.rs` polls the event
//! channel in `about_to_wait`, maps the id to an action via the pure [`action_for`]
//! (unit-tested here), and calls the **same `App` methods the keyboard already calls**.
//!
//! **Shortcuts.** Windows shows shortcuts as right-aligned label hints (`"Open File…\tO"`)
//! with no real accelerator registered — the winit key handler owns those keys, and a
//! registered accelerator would double-fire.

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
    pub const RECURSIVE: &str = "recursive";
    pub const SLIDESHOW: &str = "slideshow";
    pub const SLIDESHOW_FASTER: &str = "slideshow_faster";
    pub const SLIDESHOW_SLOWER: &str = "slideshow_slower";
    /// Matches `Action::ToggleToolbar.id()` (task #61).
    pub const TOOLBAR: &str = "toggle_toolbar";
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
    pub const TOGGLE_SUBTITLES: &str = "toggle_subtitles";
    pub const SUBTITLE_CYCLE: &str = "subtitle_cycle";

    /// Prefix for the Playback ▸ Subtitle Track flyout's **dynamic** ids
    /// (`"subtitle_track:3"` → picker row 3). Every other id in this module is a constant,
    /// because every other item exists for the life of the menu; these are rebuilt per file,
    /// so the row index has to ride in the id itself. Parsed back by
    /// [`subtitle_track_row`] — the only place that knows the encoding.
    pub const SUBTITLE_TRACK_PREFIX: &str = "subtitle_track:";
    /// Prefix for the Playback ▸ Audio Track flyout's dynamic ids (task #99) — same
    /// scheme, parsed back by [`audio_track_row`].
    pub const AUDIO_TRACK_PREFIX: &str = "audio_track:";
    /// Matches `Action::AudioNext.id()` (the Linux egui bar's cycle item; the native
    /// bars answer "which track" with the flyout instead).
    pub const AUDIO_CYCLE: &str = "audio_next";

    pub const HELP: &str = "help";
    pub const ABOUT: &str = "about";
}

/// The picker row a dynamic subtitle-track id names (`"subtitle_track:3"` → `Some(3)`), or
/// `None` for any other id — including a malformed one, which is simply ignored the way
/// [`action_for`] ignores an unknown id.
///
/// **The row is an index into `AppCore::subtitle_picker_rows`**, which is the same list
/// `Shift+C` and the Mac's popover walk. That is the whole contract: the flyout may *hide*
/// rows (it omits `Off`, which its own toggle owns) but must never renumber them, or a click
/// would select a different track than the one it read.
pub fn subtitle_track_row(id: &str) -> Option<usize> {
    id.strip_prefix(ids::SUBTITLE_TRACK_PREFIX)?.parse().ok()
}

/// The picker row a dynamic audio-track id names (`"audio_track:2"` → `Some(2)`), or
/// `None` for any other id. **The row indexes `AppCore::audio_picker_rows`** — the same
/// list `A`/`Shift+A` cycle and the Mac's flyout walk, and unlike the subtitle list it
/// has no synthetic rows (no `Off`, no `Automatic`), so the flyout shows every row and
/// the indices are naturally canonical.
pub fn audio_track_row(id: &str) -> Option<usize> {
    id.strip_prefix(ids::AUDIO_TRACK_PREFIX)?.parse().ok()
}

/// One row of the Playback ▸ Subtitle Track flyout, as a shell-neutral model.
///
/// The flyout's *rules* live here as a pure function ([`subtitle_track_rows`]) rather than
/// inside the muda construction, so they can be unit-tested without a menu, a window, or an
/// OS. The Windows builder just walks this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackRow {
    /// A pickable track: its **canonical picker index**, its label, and whether it is what
    /// is on screen right now.
    Track {
        row: usize,
        label: String,
        active: bool,
    },
    /// A divider (under `Automatic` — see below).
    Separator,
    /// A disabled explanatory line. Never a track, never clickable.
    Note(&'static str),
}

/// Build the Playback ▸ Subtitle Track flyout's rows from the core's picker list.
///
/// Three things this encodes, each of which was a defect somewhere first:
///
/// 1. **The tri-state is not collapsed.** "No Video", "Reading Tracks…" and "No Subtitle
///    Tracks" are three different answers. Saying "no subtitles" over a file whose probe
///    hasn't landed is a confident lie, and saying "reading tracks" over a photograph is
///    nonsense.
/// 2. **Row 0 (`Off`) is dropped, and every other row keeps its index.** The `Subtitles`
///    toggle above the flyout owns off/on; an `Off` row here would be a second, competing
///    answer to that question. Hiding a row is fine — renumbering would silently select the
///    wrong track (see [`subtitle_track_row`]).
/// 3. **`Automatic` gets a divider under it.** It is a different *kind* of answer from the
///    tracks below it ("you choose" vs "this one"), so it doesn't read as one of them.
pub fn subtitle_track_rows(
    picker: &[pb_app_core::tracks::PickerRow],
    video_showing: bool,
    tracks_known: bool,
) -> Vec<TrackRow> {
    if !video_showing {
        return vec![TrackRow::Note("No Video")];
    }
    let rows: Vec<TrackRow> = picker
        .iter()
        .enumerate()
        .skip(1) // row 0 is Off — the toggle's job, not the flyout's
        .flat_map(|(row, r)| {
            let item = TrackRow::Track {
                row,
                label: r.label.clone(),
                active: r.active,
            };
            // The Automatic row is a different kind of answer from the tracks under it.
            if r.label == pb_app_core::tracks::AUTOMATIC_ROW {
                vec![item, TrackRow::Separator]
            } else {
                vec![item]
            }
        })
        .collect();
    // Nothing but `Off` came back, which happens two ways: the probe hasn't landed (the list
    // is empty), or it landed and the file has no renderable track (`picker_choices` returns
    // a bare `[Off]`, since `Automatic` with nothing to resolve to would be a row that does
    // nothing). Only `tracks_known` tells those apart, and they are not the same answer.
    if rows.is_empty() {
        return vec![TrackRow::Note(if tracks_known {
            "No Subtitle Tracks"
        } else {
            "Reading Tracks…"
        })];
    }
    rows
}

/// Build the Playback ▸ Audio Track flyout's rows (task #99) — the audio twin of
/// [`subtitle_track_rows`], with two deliberate differences (both the macOS bar's shape):
/// no `Off`/`Automatic` rows exist to drop (you cannot turn audio *off* from a track
/// list — that's Mute — and "no choice" already *is* one of these rows: the default the
/// tick sits on), and therefore no separator either. The tri-state notes are the same
/// three honest answers, and every row keeps its canonical picker index.
///
/// `video_item` is "the displayed item is a video" — playing or **at its poster** — not
/// "a session is showing": the track list is a fact about the file, and hiding it until
/// playback claimed "No Video" over an obvious film (owner, 2026-07-17).
pub fn audio_track_rows(
    picker: &[pb_app_core::tracks::PickerRow],
    video_item: bool,
    tracks_known: bool,
) -> Vec<TrackRow> {
    if !video_item {
        return vec![TrackRow::Note("No Video")];
    }
    let rows: Vec<TrackRow> = picker
        .iter()
        .enumerate()
        .map(|(row, r)| TrackRow::Track {
            row,
            label: r.label.clone(),
            active: r.active,
        })
        .collect();
    if rows.is_empty() {
        return vec![TrackRow::Note(if tracks_known {
            "No Audio Tracks"
        } else {
            "Reading Tracks…"
        })];
    }
    rows
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
    ToggleToolbar,
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
    ToggleSubtitles,
    SubtitleCycle,
    AudioCycle,
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
            MenuAction::ToggleToolbar => Action::ToggleToolbar,
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
            MenuAction::ToggleSubtitles => Action::ToggleSubtitles,
            MenuAction::SubtitleCycle => Action::SubtitleCycle,
            MenuAction::AudioCycle => Action::AudioNext,
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
        TOOLBAR => MenuAction::ToggleToolbar,
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
        TOGGLE_SUBTITLES => MenuAction::ToggleSubtitles,
        SUBTITLE_CYCLE => MenuAction::SubtitleCycle,
        AUDIO_CYCLE => MenuAction::AudioCycle,
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
// The native menu is Windows/macOS-only (muda needs the off-by-default `gtk` feature on Linux),
// so this — and `BuiltMenu`/`build_menu` below — are dead on Linux; allow it only there.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn check_item(id: &str, label: &str) -> CheckMenuItem {
    CheckMenuItem::with_id(id, label, true, false, None)
}

/// The user's **current** shortcut for `action` as a display string (e.g. `"Space"`,
/// `"Shift+R"`, `"P"`), or empty if the action is unbound. Sourced live from the keymap so
/// the menu reflects **customized** bindings (`KeyChord`'s `Display` — the same text the
/// Settings ▸ Shortcuts editor shows), never a hardcoded guess. The first binding wins when
/// an action has two (menus show one accelerator). Used by the Windows native menu (the Linux
/// egui menu renders labels without the `\t` accelerator hint); kept + tested cross-platform.
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
/// so an un-shortcutted item just reads as its plain label. Used by the Windows native menu;
/// the Linux egui menu builds its rows from the plain labels + the model, not this `\t` form.
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
    /// View ▸ Show Toolbar (#61): checked while the docked windowed toolbar is on.
    pub toolbar: CheckMenuItem,
    /// The two info overlays are mutually exclusive toggles (basic vs. full EXIF);
    /// exactly one — or neither — is checked, mirroring `App::info`.
    pub info: CheckMenuItem,
    pub full_exif: CheckMenuItem,
    /// View ▸ Hide Panels (task #54): checked while rich panels are Tab-hidden;
    /// enabled only with a panel open (mirrors the core's `hide_panels_enabled`).
    pub toggle_panels: CheckMenuItem,
    /// Checked when Live Photo audio is muted (#38), mirroring `settings.mute_live_audio`.
    pub mute_live_audio: CheckMenuItem,
    /// View ▸ Subtitles (task #90): checked when captions are on, mirroring
    /// `settings.subtitles`. Always *enabled*, like Mute Live Photo Audio — a preference
    /// you set whenever you like, not a property of what's on screen.
    pub subtitles: CheckMenuItem,
}

/// Everything [`build_menu`] hands back: the menu itself plus the item handles whose
/// state the app toggles at runtime (Save Rotation's enabled flag; the View checks).
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
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
    /// The Playback ▸ Subtitle Track flyout (task #99). Held because its rows are **per
    /// file**, so unlike everything else here they are rebuilt at runtime rather than
    /// checked/enabled — see `App::refresh_subtitle_track_menu`. Starts empty.
    ///
    /// ⚠ **Never disable the holder** to say "no video". macOS learned this the hard way: a
    /// disabled submenu item can't be hovered, so its delegate never fires again and nothing
    /// can re-enable it — greying it out once greyed it out *permanently*. muda rebuilds
    /// rather than pulls, so it isn't literally the same trap, but the rule that saves us is
    /// the same one: the state lives in the submenu's **contents** ("No Video"), which are
    /// always re-derivable, never in the holder's enabled flag.
    pub subtitle_tracks: Submenu,
    /// The Playback ▸ Audio Track flyout (task #99) — same rebuilt-per-file contract
    /// (and the same ⚠ never-disable-the-holder rule) as `subtitle_tracks`.
    pub audio_tracks: Submenu,
    pub checks: ViewChecks,
}

/// Build the full menu bar. Best-effort: a failed `append` (rare) is logged and the
/// rest of the menu still builds, so the app never fails to start over a menu glitch.
///
/// Returns the menu plus the item handles `main.rs` toggles at runtime: the **Save
/// Rotation** item (enabled only when the current photo has an unsaved rotation on an
/// EXIF-writable file — see `App::refresh_save_menu_item`; starts disabled), and the
/// **View checks** (scale mode / recursive / fullscreen — see
/// `App::refresh_view_menu_checks`).
#[cfg_attr(not(windows), allow(dead_code))] // used on Windows; Linux has no native menu
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
    // Derive the hint from the keymap (F is the primary now, cross-platform), not a hardcoded
    // F11 — so the menu, the help overlay, and the toolbar's exit toast all advertise one key.
    let fullscreen = check_item(
        ids::FULLSCREEN,
        &labeled(keymap, "Quick Full Screen", Action::Fullscreen),
    );
    let slideshow = check_item(ids::SLIDESHOW, "Slideshow\tS");
    // The docked windowed toolbar (#61) — unbound by default, so no shortcut hint.
    let toolbar = check_item(ids::TOOLBAR, "Show Toolbar");
    let info = check_item(ids::INFO, "Show Image Info\tI");
    let full_exif = check_item(ids::FULL_EXIF, "Show Detailed Info\tShift+I");
    let toggle_panels = check_item(ids::TOGGLE_PANELS, "Hide Panels\tTab");
    let mute_live_audio = check_item(ids::MUTE_LIVE_AUDIO, "Mute Live Photo Audio\tM");
    let subtitles = check_item(ids::TOGGLE_SUBTITLES, "Subtitles\tC");

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
        &toolbar,
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
    ]);

    // Playback (task #99) — everything time-based in one place: the transport, and the track
    // choices beside it. Mirrors the macOS bar, which is the owner's call (2026-07-15) over a
    // View ▸ Subtitles flyout, and for the reason IINA and VLC both land here: the audio and
    // subtitle pickers belong side by side, and "View" is where neither belongs. The transport
    // items moved here out of Image, which keeps the still-image verbs.
    //
    // The Subtitle Track flyout starts empty; `App::refresh_subtitle_track_menu` fills it from
    // the file on screen. macOS pulls its rows in `NSMenuDelegate.menuNeedsUpdate` (fires as
    // the menu opens, so it can never be stale); muda has no such hook — the tree is built
    // once and only its checkmarks are mirrored — so the shell rebuilds this on change instead.
    let subtitle_tracks = Submenu::new("Subtitle Track", true);
    // The Audio Track flyout (task #99): audio and subtitles side by side is the reason
    // this menu exists. No toggle pairs with it — you can't turn audio *off* from a
    // track list (that's Mute) — and it sits above the subtitle pair, macOS-style.
    let audio_tracks = Submenu::new("Audio Track", true);
    let playback = Submenu::new("&Playback", true);
    let _ = playback.append_items(&[
        &item(
            ids::PLAY_PAUSE,
            &labeled(keymap, "Play/Pause", Action::PlayPause),
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
        &audio_tracks,
        // Two items, because they answer two different questions and neither may clobber the
        // other (owner, 2026-07-15): "Subtitles" is on/off — the `C` key's twin — and
        // "Subtitle Track" is *which*. Fusing them (an Off row inside the flyout, and no
        // toggle) was the defect: turning subtitles off had to forget the track you had
        // picked, so picking Chinese and toggling twice gave you English.
        &subtitles,
        &subtitle_tracks,
        &sep(),
        &mute_live_audio,
    ]);

    let help = Submenu::new("&Help", true);
    let _ = help.append_items(&[
        &item(ids::HELP, "Keyboard Shortcuts\t?"),
        &item(ids::ABOUT, &format!("About {}", pb_app_core::APP_NAME)),
    ]);

    for sub in [&file, &edit, &view, &go, &image, &playback, &help] {
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
        subtitle_tracks,
        audio_tracks,
        checks: ViewChecks {
            fit,
            fill,
            original,
            recursive,
            fullscreen,
            slideshow,
            toolbar,
            info,
            full_exif,
            toggle_panels,
            mute_live_audio,
            subtitles,
        },
    }
}

/// A shell-neutral description of the windowed menu bar for the **egui overlay** used on
/// Linux — where winit makes a raw X11/Wayland window, not a `gtk::ApplicationWindow`, so
/// muda has nothing to attach a native bar to (this is why Tauri uses `tao`, not winit).
/// Built from the same ids/labels/keymap hints as [`build_menu`] plus the live [`MenuState`]
/// for the check/enabled marks, so the egui bar and the Win/mac native bar stay in lockstep.
/// Clicks carry a [`MenuAction`] dispatched through the very same `App::dispatch_menu`.
#[cfg(all(unix, not(target_os = "macos")))]
pub struct MenuGroup {
    pub title: &'static str,
    pub items: Vec<MenuRow>,
}

/// One row of an egui menu dropdown: a dispatchable item or a separator.
#[cfg(all(unix, not(target_os = "macos")))]
pub enum MenuRow {
    Separator,
    Item {
        action: MenuAction,
        label: String,
        /// Right-aligned shortcut hint (live from the keymap), or empty.
        shortcut: String,
        enabled: bool,
        /// `Some(checked)` for a checkable item (draws a check column); `None` = plain.
        checked: Option<bool>,
    },
}

/// Build the egui menu-bar model for the current [`MenuState`]. Mirrors [`build_menu`]'s
/// structure exactly (same groups, order, ids, labels), reading the check/enabled marks
/// from `s`. Kept beside `build_menu` on purpose — this file is the one place the two menu
/// presentations could drift, so they sit together.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn menu_bar_spec(keymap: &Keymap, s: &crate::contract::MenuState) -> Vec<MenuGroup> {
    use crate::contract::ScaleMode;
    // A plain (uncheckable) item; `label` may carry a hardcoded "\tHint".
    let item = |action, label: &str, enabled| {
        let (label, shortcut) = match label.split_once('\t') {
            Some((l, h)) => (l.to_string(), h.to_string()),
            None => (label.to_string(), String::new()),
        };
        MenuRow::Item {
            action,
            label,
            shortcut,
            enabled,
            checked: None,
        }
    };
    // A checkable item (shows a check column tracking `checked`).
    let check = |action, label: &str, enabled, checked: bool| {
        let (label, shortcut) = match label.split_once('\t') {
            Some((l, h)) => (l.to_string(), h.to_string()),
            None => (label.to_string(), String::new()),
        };
        MenuRow::Item {
            action,
            label,
            shortcut,
            enabled,
            checked: Some(checked),
        }
    };
    let sep = || MenuRow::Separator;
    // Shortcut hint for a keymap-bound item, live from the user's bindings (as `labeled`).
    let l = |base, action| labeled(keymap, base, action);

    vec![
        MenuGroup {
            title: "File",
            items: vec![
                item(MenuAction::OpenFile, "Open File…\tO", true),
                item(MenuAction::OpenFolder, "Open Folder…\tShift+O", true),
                item(
                    MenuAction::CancelScan,
                    "Stop Scanning",
                    s.cancel_scan_enabled,
                ),
                sep(),
                item(
                    MenuAction::SaveRotation,
                    "Save Rotation\tCtrl+S",
                    s.save_rotation_enabled,
                ),
                item(
                    MenuAction::Reveal,
                    "Show in File Explorer",
                    s.reveal_enabled,
                ),
                sep(),
                item(MenuAction::Delete, "Delete\tDel", true),
                item(
                    MenuAction::DeletePermanently,
                    "Delete Permanently\tShift+Del",
                    true,
                ),
                sep(),
                item(MenuAction::Settings, "Settings…\tCtrl+,", true),
                sep(),
                item(MenuAction::Exit, "Exit\tEsc", true),
            ],
        },
        MenuGroup {
            title: "Edit",
            items: vec![
                item(
                    MenuAction::Undo,
                    &format!("{}\tCtrl+Z", s.undo.as_deref().unwrap_or("Undo")),
                    s.undo.is_some(),
                ),
                sep(),
                item(MenuAction::Copy, "Copy\tCtrl+C", true),
                item(MenuAction::CopyPath, "Copy File Path\tShift+Ctrl+C", true),
                item(
                    MenuAction::CopyImageText,
                    &l("Copy Text from Image", Action::CopyImageText),
                    true,
                ),
                sep(),
                item(
                    MenuAction::DescribeImage,
                    &l("Describe Image", Action::DescribeImage),
                    true,
                ),
                item(
                    MenuAction::AskImage,
                    &l("Ask About Image…", Action::AskImage),
                    true,
                ),
                item(
                    MenuAction::CopyDescription,
                    &l("Copy AI Description", Action::CopyDescription),
                    true,
                ),
            ],
        },
        MenuGroup {
            title: "View",
            items: vec![
                check(MenuAction::Fit, "Fit\t8", true, s.scale == ScaleMode::Fit),
                check(
                    MenuAction::Fill,
                    "Crop to Fill\t9",
                    true,
                    s.scale == ScaleMode::Fill,
                ),
                check(
                    MenuAction::Original,
                    "Original 1:1\t0",
                    true,
                    s.scale == ScaleMode::Original,
                ),
                sep(),
                item(MenuAction::ZoomIn, "Zoom In\t=", true),
                item(MenuAction::ZoomOut, "Zoom Out\t-", true),
                sep(),
                check(
                    MenuAction::Fullscreen,
                    &l("Quick Full Screen", Action::Fullscreen),
                    true,
                    s.fullscreen,
                ),
                check(
                    MenuAction::Recursive,
                    "Recursive (This Folder)\tCtrl+R",
                    true,
                    s.recursive,
                ),
                check(MenuAction::Slideshow, "Slideshow\tS", true, s.slideshow),
                item(MenuAction::SlideshowFaster, "Slideshow Faster\t[", true),
                item(MenuAction::SlideshowSlower, "Slideshow Slower\t]", true),
                sep(),
                check(
                    MenuAction::ToggleToolbar,
                    "Show Toolbar",
                    true,
                    s.show_toolbar,
                ),
                check(MenuAction::Info, "Show Image Info\tI", true, s.info_basic),
                check(
                    MenuAction::FullExif,
                    "Show Detailed Info\tShift+I",
                    true,
                    s.info_full,
                ),
                item(
                    MenuAction::FolderTree,
                    &l("Show Folder Tree", Action::FolderTree),
                    true,
                ),
                check(
                    MenuAction::TogglePanels,
                    "Hide Panels\tTab",
                    s.hide_panels_enabled,
                    s.panels_hidden,
                ),
            ],
        },
        MenuGroup {
            title: "Go",
            items: vec![
                item(
                    MenuAction::OpenParent,
                    &l("Parent Folder", Action::OpenParent),
                    true,
                ),
                sep(),
                item(
                    MenuAction::PrevFolder,
                    &l("Previous Folder", Action::PrevFolder),
                    true,
                ),
                item(
                    MenuAction::NextFolder,
                    &l("Next Folder", Action::NextFolder),
                    true,
                ),
            ],
        },
        MenuGroup {
            title: "Image",
            items: vec![
                item(MenuAction::Next, &l("Next", Action::Next), true),
                item(MenuAction::Previous, &l("Previous", Action::Prev), true),
                item(MenuAction::Random, &l("Random", Action::Random), true),
                item(
                    MenuAction::RandomPrev,
                    &l("Previous Random", Action::RandomPrev),
                    true,
                ),
                sep(),
                item(
                    MenuAction::RotateRight,
                    &l("Rotate Right", Action::RotateCw),
                    true,
                ),
                item(
                    MenuAction::RotateLeft,
                    &l("Rotate Left", Action::RotateCcw),
                    true,
                ),
                sep(),
                check(
                    MenuAction::ComparePin,
                    &l("Pin for Compare", Action::ComparePin),
                    s.compare_pin_enabled,
                    s.compare_pinned_here,
                ),
                item(
                    MenuAction::CompareToggle,
                    &l("Compare with Pinned", Action::CompareToggle),
                    s.compare_toggle_enabled,
                ),
            ],
        },
        // Playback — the same grouping as the Windows/macOS bars (task #99), so the three
        // don't teach three different mental models. The one deliberate difference: **no
        // track flyout**. This bar is a hand-rolled egui dropdown with no submenu support, so
        // "which subtitle track" stays `Shift+C` here, and the item below advertises it. That
        // is why `SubtitleCycle` survives in the model at all — the native bars dropped it
        // once their flyout could answer the same question better.
        MenuGroup {
            title: "Playback",
            items: vec![
                item(
                    MenuAction::PlayPause,
                    &l("Play/Pause", Action::PlayPause),
                    true,
                ),
                item(
                    MenuAction::FrameNext,
                    &l("Next Frame", Action::FrameNext),
                    true,
                ),
                item(
                    MenuAction::FramePrev,
                    &l("Previous Frame", Action::FramePrev),
                    true,
                ),
                sep(),
                // No flyouts in this hand-rolled bar (no submenu support), so "which
                // audio track" stays `A` here — this item is what advertises it, the
                // same reason `Next Subtitle Track` survives below.
                item(MenuAction::AudioCycle, "Next Audio Track\tA", true),
                check(
                    MenuAction::ToggleSubtitles,
                    "Subtitles\tC",
                    true,
                    s.subtitles,
                ),
                // Always enabled, like the toggle above it: the core answers "No subtitle
                // tracks" / "Reading tracks…" with a toast, which is a better answer than a
                // greyed-out item that explains nothing.
                item(
                    MenuAction::SubtitleCycle,
                    "Next Subtitle Track\tShift+C",
                    true,
                ),
                sep(),
                check(
                    MenuAction::MuteLiveAudio,
                    "Mute Live Photo Audio\tM",
                    true,
                    s.mute_live_audio,
                ),
            ],
        },
        MenuGroup {
            title: "Help",
            items: vec![
                item(MenuAction::Help, "Keyboard Shortcuts\t?", true),
                item(
                    MenuAction::About,
                    &format!("About {}", pb_app_core::APP_NAME),
                    true,
                ),
            ],
        },
    ]
}

/// Keyboard/pointer state for the egui menu bar (Linux). The bar and its dropdown are
/// fully state-driven — pointer and keyboard both mutate this one struct, so hover and
/// arrow-key selection can never fight. Owned by the shell (`App`), rendered by
/// `panels_ui::menu_bar`, driven from the key handler via [`menu_nav_key`].
#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct MenuNav {
    /// Which group's dropdown is open (index into the `menu_bar_spec` vec), or none.
    pub open: Option<usize>,
    /// Selected row in the open group (index into its `items`, item rows only). `None`
    /// after a mouse-open (no preselection — GTK/Windows behavior).
    pub sel: Option<usize>,
    /// Underline the title mnemonics (Alt is held) — the GTK "reveal on Alt" hint.
    pub alt_hint: bool,
}

/// A key press routed to the menu bar, in menu terms (the shell maps winit keycodes).
#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuKey {
    Esc,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    /// Enter / Space — activate the selected item.
    Activate,
    /// A plain letter while a menu is open: an item mnemonic (first letter of the label).
    Char(char),
    /// Alt+letter: a bar mnemonic (Alt+F → File), GTK/Windows style.
    AltChar(char),
    /// F10 toggles the first menu (the freedesktop menubar key).
    F10,
}

/// What [`menu_nav_key`] did with the key.
#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuKeyOutcome {
    /// Not a menu key in this state — let the app's keymap handle it.
    Ignored,
    /// The menu took it (state changed, or swallowed by an open dropdown's key grab).
    Consumed,
    /// Activate this item — the menu closed itself; the shell dispatches the action.
    Activate(MenuAction),
}

/// The bar mnemonic of a menu title: its first letter, lowercased (File → `f`).
#[cfg(all(unix, not(target_os = "macos")))]
fn mnemonic(title: &str) -> Option<char> {
    title.chars().next().map(|c| c.to_ascii_lowercase())
}

/// Whether row `i` is a selectable (enabled, non-separator) item.
#[cfg(all(unix, not(target_os = "macos")))]
fn selectable(items: &[MenuRow], i: usize) -> bool {
    matches!(items.get(i), Some(MenuRow::Item { enabled: true, .. }))
}

/// First selectable row, for opening a menu from the keyboard.
#[cfg(all(unix, not(target_os = "macos")))]
fn first_selectable(items: &[MenuRow]) -> Option<usize> {
    (0..items.len()).find(|&i| selectable(items, i))
}

/// Step the selection over the selectable rows, wrapping. `from = None` enters from the
/// top (Down) or bottom (Up), matching native menus.
#[cfg(all(unix, not(target_os = "macos")))]
fn step_selectable(items: &[MenuRow], from: Option<usize>, down: bool) -> Option<usize> {
    let n = items.len();
    if n == 0 {
        return None;
    }
    let start = match (from, down) {
        (Some(i), true) => i + 1,
        (Some(i), false) => i + n - 1, // mod-n step back
        (None, true) => 0,
        (None, false) => n - 1,
    };
    (0..n)
        .map(|k| {
            if down {
                (start + k) % n
            } else {
                (start + n - k) % n
            }
        })
        .find(|&i| selectable(items, i))
}

/// Drive the menu bar with one key press. Pure — no egui, no winit — so the
/// GTK/Windows-style rules live in one unit-tested place:
/// * closed: `Alt+mnemonic` or `F10` opens (selecting the first item); all else ignored.
/// * open: `Esc` closes; `←`/`→` switch menus; `↑`/`↓`/`Home`/`End` move the selection
///   (skipping separators + disabled rows, wrapping); `Enter`/`Space` activates; a letter
///   jumps to items starting with it (activating on a unique match); `Alt+mnemonic`
///   switches menus; `F10` closes. An open dropdown consumes every key it's fed.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn menu_nav_key(nav: &mut MenuNav, groups: &[MenuGroup], key: MenuKey) -> MenuKeyOutcome {
    use MenuKeyOutcome as Out;
    let open_group = |nav: &mut MenuNav, g: usize| {
        nav.open = Some(g);
        nav.sel = first_selectable(&groups[g].items);
    };
    let g = match nav.open {
        Some(g) => g,
        // Closed: only the open chords do anything.
        None => {
            return match key {
                MenuKey::AltChar(c) => {
                    match groups.iter().position(|gr| mnemonic(gr.title) == Some(c)) {
                        Some(g) => {
                            open_group(nav, g);
                            Out::Consumed
                        }
                        None => Out::Ignored,
                    }
                }
                MenuKey::F10 if !groups.is_empty() => {
                    open_group(nav, 0);
                    Out::Consumed
                }
                _ => Out::Ignored,
            };
        }
    };
    let items = &groups[g].items;
    match key {
        MenuKey::Esc | MenuKey::F10 => {
            nav.open = None;
            nav.sel = None;
        }
        MenuKey::Left => open_group(nav, (g + groups.len() - 1) % groups.len()),
        MenuKey::Right => open_group(nav, (g + 1) % groups.len()),
        MenuKey::Up => nav.sel = step_selectable(items, nav.sel, false),
        MenuKey::Down => nav.sel = step_selectable(items, nav.sel, true),
        MenuKey::Home => nav.sel = first_selectable(items),
        MenuKey::End => nav.sel = step_selectable(items, None, false),
        MenuKey::Activate => {
            if let Some(MenuRow::Item {
                action,
                enabled: true,
                ..
            }) = nav.sel.and_then(|i| items.get(i))
            {
                let action = *action;
                nav.open = None;
                nav.sel = None;
                return MenuKeyOutcome::Activate(action);
            }
        }
        MenuKey::AltChar(c) => {
            if let Some(g2) = groups.iter().position(|gr| mnemonic(gr.title) == Some(c)) {
                open_group(nav, g2);
            }
        }
        MenuKey::Char(c) => {
            // Item mnemonic: rows whose label starts with the letter. A unique match
            // activates; several matches cycle the selection through them.
            let matches: Vec<usize> = (0..items.len())
                .filter(|&i| {
                    selectable(items, i)
                        && matches!(&items[i], MenuRow::Item { label, .. }
                            if label.chars().next().map(|l| l.to_ascii_lowercase()) == Some(c))
                })
                .collect();
            match matches.as_slice() {
                [] => {}
                [only] => {
                    if let Some(MenuRow::Item { action, .. }) = items.get(*only) {
                        let action = *action;
                        nav.open = None;
                        nav.sel = None;
                        return MenuKeyOutcome::Activate(action);
                    }
                }
                many => {
                    let next = many
                        .iter()
                        .find(|&&i| nav.sel.is_none_or(|s| i > s))
                        .or(many.first());
                    nav.sel = next.copied();
                }
            }
        }
    }
    Out::Consumed
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
        let reveal_label = "Show in File Explorer";
        let _ = menu.append(&item(ids::REVEAL, reveal_label));
    }
    // Fullscreen toggle, last. The label tracks the live mode — vital in the fullscreen
    // speed mode, where the menu bar is hidden and this is the only pointer route out.
    let fullscreen_label = if state.fullscreen {
        "Exit Quick Full Screen"
    } else {
        "Enter Quick Full Screen"
    };
    let _ = menu.append_items(&[&sep(), &item(ids::FULLSCREEN, fullscreen_label)]);
    menu
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::KeyChord;

    /// A small three-menu bar for the nav tests: File(Open, Quit), Edit(sep-heavy,
    /// with a disabled row), View(checkable) — enough shape for every rule.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn bar() -> Vec<MenuGroup> {
        let item = |action, label: &str, enabled| MenuRow::Item {
            action,
            label: label.to_string(),
            shortcut: String::new(),
            enabled,
            checked: None,
        };
        vec![
            MenuGroup {
                title: "File",
                items: vec![
                    item(MenuAction::OpenFile, "Open File…", true),
                    item(MenuAction::OpenFolder, "Open Folder…", true),
                    MenuRow::Separator,
                    item(MenuAction::Exit, "Quit", true),
                ],
            },
            MenuGroup {
                title: "Edit",
                items: vec![
                    MenuRow::Separator,
                    item(MenuAction::Undo, "Undo", false),
                    item(MenuAction::Copy, "Copy", true),
                ],
            },
            MenuGroup {
                title: "View",
                items: vec![item(MenuAction::Slideshow, "Slideshow", true)],
            },
        ]
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn alt_mnemonic_opens_the_menu_and_selects_the_first_item() {
        let (groups, mut nav) = (bar(), MenuNav::default());
        assert_eq!(
            menu_nav_key(&mut nav, &groups, MenuKey::AltChar('f')),
            MenuKeyOutcome::Consumed
        );
        assert_eq!((nav.open, nav.sel), (Some(0), Some(0)));
        // Alt+E while open switches menus — and skips Edit's leading separator +
        // disabled Undo to land on Copy.
        menu_nav_key(&mut nav, &groups, MenuKey::AltChar('e'));
        assert_eq!((nav.open, nav.sel), (Some(1), Some(2)));
        // An unknown mnemonic while closed is ignored (falls through to the keymap).
        let mut closed = MenuNav::default();
        assert_eq!(
            menu_nav_key(&mut closed, &groups, MenuKey::AltChar('z')),
            MenuKeyOutcome::Ignored
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn f10_toggles_and_esc_closes() {
        let (groups, mut nav) = (bar(), MenuNav::default());
        menu_nav_key(&mut nav, &groups, MenuKey::F10);
        assert_eq!(nav.open, Some(0));
        menu_nav_key(&mut nav, &groups, MenuKey::F10);
        assert_eq!(nav.open, None);
        menu_nav_key(&mut nav, &groups, MenuKey::F10);
        assert_eq!(
            menu_nav_key(&mut nav, &groups, MenuKey::Esc),
            MenuKeyOutcome::Consumed // Esc is EATEN — it must never fall through to quit
        );
        assert_eq!((nav.open, nav.sel), (None, None));
        // Esc with everything closed is not a menu key.
        assert_eq!(
            menu_nav_key(&mut nav, &groups, MenuKey::Esc),
            MenuKeyOutcome::Ignored
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn arrows_wrap_and_skip_separators_and_disabled() {
        let (groups, mut nav) = (bar(), MenuNav::default());
        menu_nav_key(&mut nav, &groups, MenuKey::AltChar('f'));
        // Down: Open File(0) → Open Folder(1) → skips the separator(2) → Quit(3) → wraps.
        menu_nav_key(&mut nav, &groups, MenuKey::Down);
        assert_eq!(nav.sel, Some(1));
        menu_nav_key(&mut nav, &groups, MenuKey::Down);
        assert_eq!(nav.sel, Some(3));
        menu_nav_key(&mut nav, &groups, MenuKey::Down);
        assert_eq!(nav.sel, Some(0));
        // Up from the top wraps to the bottom.
        menu_nav_key(&mut nav, &groups, MenuKey::Up);
        assert_eq!(nav.sel, Some(3));
        menu_nav_key(&mut nav, &groups, MenuKey::End);
        assert_eq!(nav.sel, Some(3));
        menu_nav_key(&mut nav, &groups, MenuKey::Home);
        assert_eq!(nav.sel, Some(0));
        // Left/Right cycle the menus (wrapping) and reselect the first item.
        menu_nav_key(&mut nav, &groups, MenuKey::Left);
        assert_eq!((nav.open, nav.sel), (Some(2), Some(0)));
        menu_nav_key(&mut nav, &groups, MenuKey::Right);
        assert_eq!((nav.open, nav.sel), (Some(0), Some(0)));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn enter_activates_and_letters_jump() {
        let (groups, mut nav) = (bar(), MenuNav::default());
        menu_nav_key(&mut nav, &groups, MenuKey::AltChar('f'));
        menu_nav_key(&mut nav, &groups, MenuKey::End);
        assert_eq!(
            menu_nav_key(&mut nav, &groups, MenuKey::Activate),
            MenuKeyOutcome::Activate(MenuAction::Exit)
        );
        assert_eq!(nav.open, None, "activation closes the menu");
        // A unique letter match activates immediately (Q → Quit)…
        menu_nav_key(&mut nav, &groups, MenuKey::AltChar('f'));
        assert_eq!(
            menu_nav_key(&mut nav, &groups, MenuKey::Char('q')),
            MenuKeyOutcome::Activate(MenuAction::Exit)
        );
        // …an ambiguous one cycles the selection through the matches (O → the two Opens).
        menu_nav_key(&mut nav, &groups, MenuKey::AltChar('f'));
        menu_nav_key(&mut nav, &groups, MenuKey::Char('o'));
        assert_eq!(nav.sel, Some(1), "sel was 0, next 'O' match is row 1");
        menu_nav_key(&mut nav, &groups, MenuKey::Char('o'));
        assert_eq!(nav.sel, Some(0), "wraps back to the first match");
        // A letter matching nothing is swallowed, not dispatched.
        assert_eq!(
            menu_nav_key(&mut nav, &groups, MenuKey::Char('x')),
            MenuKeyOutcome::Consumed
        );
        // Enter with no selection is swallowed and keeps the menu open.
        let mut fresh = MenuNav {
            open: Some(0),
            sel: None,
            alt_hint: false,
        };
        assert_eq!(
            menu_nav_key(&mut fresh, &groups, MenuKey::Activate),
            MenuKeyOutcome::Consumed
        );
        assert_eq!(fresh.open, Some(0));
    }

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
        assert_eq!(action_for(ids::TOOLBAR), Some(MenuAction::ToggleToolbar));
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

    // ── The Playback ▸ Subtitle Track flyout (task #99) ──────────────────

    use pb_app_core::tracks::{PickerRow, AUTOMATIC_ROW, OFF_ROW};

    /// The core's canonical picker list for a file with three renderable tracks, with the
    /// tick sitting where `Automatic` resolved it (row 3) — the shape
    /// `subtitle::picker_choices` guarantees: `Off`, `Automatic`, then the tracks.
    fn picker() -> Vec<PickerRow> {
        let row = |label: &str, active| PickerRow {
            label: label.to_string(),
            active,
        };
        vec![
            row(OFF_ROW, false),
            row(AUTOMATIC_ROW, false),
            row("English · SubRip", false),
            row("English SDH · SubRip", true),
            row("Chinese · ASS", false),
        ]
    }

    #[test]
    fn a_track_id_round_trips_through_its_menu_id() {
        // The encoding the flyout writes is the one the dispatcher reads — the pair only
        // works if they agree, so pin them together.
        for row in [0usize, 3, 42] {
            let id = format!("{}{row}", ids::SUBTITLE_TRACK_PREFIX);
            assert_eq!(subtitle_track_row(&id), Some(row));
        }
        // Not a track id: every constant id, and junk. A stray/foreign event is ignored.
        assert_eq!(subtitle_track_row(ids::TOGGLE_SUBTITLES), None);
        assert_eq!(subtitle_track_row(ids::SUBTITLE_CYCLE), None);
        assert_eq!(subtitle_track_row(""), None);
        assert_eq!(subtitle_track_row("subtitle_track:"), None);
        assert_eq!(subtitle_track_row("subtitle_track:-1"), None);
        assert_eq!(subtitle_track_row("subtitle_track:abc"), None);
        // The two dispatchers must not both claim an id, or a click would fire twice.
        assert_eq!(action_for("subtitle_track:2"), None);
    }

    #[test]
    fn the_flyout_drops_the_off_row_without_renumbering_the_rest() {
        // THE load-bearing property. `Off` is the toggle's job, so the flyout hides it — but
        // the row index it hands back is the core's, and renumbering would select a
        // different track than the one the user read.
        let rows = subtitle_track_rows(&picker(), true, true);
        let tracks: Vec<(usize, &str, bool)> = rows
            .iter()
            .filter_map(|r| match r {
                TrackRow::Track { row, label, active } => Some((*row, label.as_str(), *active)),
                _ => None,
            })
            .collect();
        assert_eq!(
            tracks,
            vec![
                (1, AUTOMATIC_ROW, false),
                (2, "English · SubRip", false),
                (3, "English SDH · SubRip", true), // the tick rides along untouched
                (4, "Chinese · ASS", false),
            ],
            "row 0 is hidden; every surviving row keeps its index into picker_choices"
        );
        assert!(
            !rows
                .iter()
                .any(|r| matches!(r, TrackRow::Track { label, .. } if label == OFF_ROW)),
            "the Off row must not appear beside the toggle that owns it"
        );
        // Automatic is a different kind of answer from the tracks, so it gets a divider.
        assert_eq!(rows[1], TrackRow::Separator);
    }

    #[test]
    fn the_flyout_keeps_the_three_empty_answers_apart() {
        // Three different facts, and collapsing any pair of them is a lie. The list is empty
        // in all three cases, so only the flags tell them apart.
        assert_eq!(
            subtitle_track_rows(&[], false, false),
            vec![TrackRow::Note("No Video")]
        );
        // …and "no video" outranks the rest: "Reading Tracks…" over a photograph is nonsense.
        assert_eq!(
            subtitle_track_rows(&picker(), false, true),
            vec![TrackRow::Note("No Video")]
        );
        // A video whose probe hasn't landed. NOT "no subtitles" — that would be a confident
        // lie about a file we haven't finished reading.
        assert_eq!(
            subtitle_track_rows(&[], true, false),
            vec![TrackRow::Note("Reading Tracks…")]
        );
        // Probed, and genuinely none. `picker_choices` returns a bare `[Off]` here — which,
        // once Off is dropped, is indistinguishable from empty. Hence the flag.
        let off_only = vec![PickerRow {
            label: OFF_ROW.to_string(),
            active: true,
        }];
        assert_eq!(
            subtitle_track_rows(&off_only, true, true),
            vec![TrackRow::Note("No Subtitle Tracks")]
        );
    }

    // -- the Playback ▸ Audio Track flyout (task #99) -------------------------

    fn arow(label: &str, active: bool) -> PickerRow {
        PickerRow {
            label: label.to_string(),
            active,
        }
    }

    #[test]
    fn an_audio_track_id_round_trips_through_its_menu_id() {
        for row in [0usize, 3, 42] {
            let id = format!("{}{row}", ids::AUDIO_TRACK_PREFIX);
            assert_eq!(audio_track_row(&id), Some(row));
        }
        assert_eq!(audio_track_row(ids::MUTE_LIVE_AUDIO), None);
        assert_eq!(audio_track_row(""), None);
        assert_eq!(audio_track_row("audio_track:"), None);
        assert_eq!(audio_track_row("audio_track:abc"), None);
        // The dispatchers must not both claim an id, or a click would fire twice —
        // and the two flyouts' namespaces must not bleed into each other.
        assert_eq!(action_for("audio_track:2"), None);
        assert_eq!(subtitle_track_row("audio_track:2"), None);
        assert_eq!(audio_track_row("subtitle_track:2"), None);
    }

    /// The audio flyout shows **every** picker row at its canonical index — there is no
    /// `Off`/`Automatic` to hide (you can't turn audio off from a track list; that's
    /// Mute) and therefore no separator either.
    #[test]
    fn the_audio_flyout_lists_every_row_at_its_canonical_index() {
        let picker = vec![
            arow("English · E-AC-3 · 5.1", true),
            arow("French · AAC · Stereo", false),
        ];
        assert_eq!(
            audio_track_rows(&picker, true, true),
            vec![
                TrackRow::Track {
                    row: 0,
                    label: "English · E-AC-3 · 5.1".into(),
                    active: true,
                },
                TrackRow::Track {
                    row: 1,
                    label: "French · AAC · Stereo".into(),
                    active: false,
                },
            ]
        );
    }

    /// The same three honest empty answers as the subtitle flyout, never collapsed.
    #[test]
    fn the_audio_flyout_keeps_the_three_empty_answers_apart() {
        assert_eq!(
            audio_track_rows(&[], false, false),
            vec![TrackRow::Note("No Video")]
        );
        assert_eq!(
            audio_track_rows(&[arow("English · AAC", true)], false, true),
            vec![TrackRow::Note("No Video")]
        );
        assert_eq!(
            audio_track_rows(&[], true, false),
            vec![TrackRow::Note("Reading Tracks…")]
        );
        assert_eq!(
            audio_track_rows(&[], true, true),
            vec![TrackRow::Note("No Audio Tracks")]
        );
    }

    /// The Linux bar's cycle item dispatches through the same action the `A` key uses.
    #[test]
    fn the_audio_cycle_item_maps_to_audio_next() {
        assert_eq!(action_for(ids::AUDIO_CYCLE), Some(MenuAction::AudioCycle));
        assert_eq!(MenuAction::AudioCycle.to_action(), Action::AudioNext);
    }
}
