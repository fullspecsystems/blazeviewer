//! The central, named set of user actions — the single vocabulary shared by the
//! keymap (`keymap.rs`), the native menu (`menu.rs`), and the keybindings help
//! overlay (task #8). One enum is what keeps the hotkeys, the menu, and the help
//! table from drifting apart: every command is dispatched through `Action`.
//!
//! Pure data (no winit, no I/O), so the id/kind tables are unit-tested here.

/// How an action is driven from the keyboard — the press handler needs this to
/// know whether to fire once, start a hold-to-blaze, or track a continuous hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActionKind {
    /// Fires once per key-down (OS auto-repeat ignored): rotate, copy, toggle a
    /// panel, open Settings, etc.
    OneShot,
    /// Photo navigation — fires on press and then repeats via hold-to-blaze
    /// (`about_to_wait`): next / prev / random.
    Nav,
    /// Continuous while the key is held, applied each frame: pan and zoom.
    Held,
    /// Animation frame-step — steps one frame on press, then repeats while held to
    /// scrub through an animation's frames (`about_to_wait`): `,` / `.`.
    FrameStep,
}

/// Every user-invokable action. The `id` strings (stable snake_case) are the
/// on-disk keymap-config names; `kind` tells the input layer how to drive it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Action {
    // Navigation (hold-to-blaze).
    Next,
    Prev,
    Random,
    RandomPrev,
    /// Next item even while a video is playing (task #94): plain `Next` (space)
    /// becomes the pause/resume toggle over a live clip, so this is the escape
    /// forward — same `Nav::Forward`, never intercepted by the video override.
    SkipNext,
    // Pan (continuous while held).
    PanLeft,
    PanRight,
    PanUp,
    PanDown,
    // Zoom (continuous while held).
    ZoomIn,
    ZoomOut,
    // Scale modes.
    ScaleFit,
    ScaleFill,
    ScaleOriginal,
    ToggleOriginal,
    // Rotate.
    RotateCw,
    RotateCcw,
    /// Pin the current photo for flicker comparison — or unpin it if it's already the
    /// pin (task #43). The whole pin-management surface.
    ComparePin,
    /// Flip between the pinned photo and the current one (full-screen A/B flicker —
    /// the culling tool). With nothing pinned, pins the current photo instead, so a
    /// single key drives the whole feature.
    CompareToggle,
    // File operations.
    Copy,
    CopyPath,
    /// Copy the current photo's details (dimensions, codec, size, EXIF tags) to the
    /// clipboard as text — the same facts the info panel shows. Named "image details"
    /// rather than "EXIF" since not every format carries EXIF (a PNG still has
    /// dimensions/codec/size).
    CopyImageDetails,
    /// Copy the text *visible in* the current photo (on-device OCR + any QR-code
    /// payloads, task #45) to the clipboard. Menu/context-menu by default (no key);
    /// runs the scan on demand if the result isn't already cached.
    CopyImageText,
    /// Show the text recognized in the current photo (plus QR payloads) in a HUD
    /// panel (`T`, task #45) — read before copying. Same on-device scan and RAM-only
    /// cache as [`Action::CopyImageText`].
    ShowImageText,
    /// Describe the current photo with a vision model (`D`, task #44) — on-device
    /// (Apple Foundation Models) or a local endpoint — shown in the HUD panel. RAM-only
    /// per-image cache, dropped on exit (privacy #2).
    DescribeImage,
    /// Ask the vision model a specific question about the current photo (task #44,
    /// subtask 9): opens a text field, then runs the answer through the same describe
    /// backend + panel. E.g. "What products are visible?" / "What year is this?".
    AskImage,
    /// Copy the current photo's AI description to the clipboard (task #44). Menu /
    /// context-menu (no default key); generates one on demand if not already cached.
    CopyDescription,
    /// Reveal the current photo in the OS file manager (macOS Finder / Windows File
    /// Explorer), with its containing folder open and the file selected. Only real
    /// on-disk files can be revealed (an archive entry / the empty deck cannot).
    RevealInFileManager,
    SaveRotation,
    Delete,
    DeletePermanent,
    Undo,
    OpenFile,
    OpenFolder,
    // View toggles / panels.
    Info,
    FullExif,
    Help,
    /// Toggle the folder-tree overlay (`Shift+F`): the current photo's folder in its
    /// hierarchy (up affordance, root, ancestors, siblings, children), drawn in the
    /// top-left corner. Rows click-to-open; the "… n more" markers page the window
    /// (see `.taskmaster/docs/folder-tree-plan.md`).
    FolderTree,
    /// Toggle the Thumbnails strip (`Shift+T`, task #83): the left pane's second tab
    /// beside Folders — a scrollable vertical strip of neighbor thumbnails with
    /// click-to-jump. Inspector-style per-tab semantics: opens the pane on the
    /// Thumbnails tab, switches if Folders is showing, closes if already showing.
    Thumbnails,
    /// Toggle rich-panel visibility (`Tab`, task #54 — the Photoshop idiom): hides the
    /// Inspector/Help/folder-tree panels without closing them; pressing again (or any
    /// panel toggle) reveals. No-op with no panel open. The ephemeral HUD (toasts,
    /// basic info line, hints) is unaffected.
    TogglePanels,
    /// Go up: open the deck root's parent folder (⌘↑ on macOS — Finder's Enclosing
    /// Folder chord, via the menu accelerator; Alt+↑ on Windows — Explorer's up).
    /// On an archive deck, opens the folder containing the archive.
    OpenParent,
    /// Go to the previous sibling folder — open the folder before the deck root in
    /// its parent's sorted listing (⌘← / Alt+←; PhotoBlaze has no back/forward
    /// history, so the chords are free).
    PrevFolder,
    /// Go to the next sibling folder (⌘→ / Alt+→).
    NextFolder,
    Fullscreen,
    Recursive,
    /// Toggle whether archives (`.zip`/`.7z`/tar/rar…) show as browsable "doors" while
    /// scanning a folder (task #104), mirroring `settings.show_archives`. Menu-only by
    /// default (View ▸ Show Archives), unbound — like a scan preference, it re-scans the
    /// current folder to add/remove the doors. Never affects opening an archive directly.
    ShowArchives,
    /// Stop an in-flight folder scan, keeping whatever has streamed in so far. Only
    /// meaningful while a scan is running (the menu item disables otherwise); unbound by
    /// default — Esc stays Quit.
    CancelScan,
    // Slideshow (timer-driven advance).
    SlideshowToggle,
    SlideshowFaster,
    SlideshowSlower,
    // Animation playback (on-demand; never autoplay).
    PlayPause,
    FrameNext,
    FramePrev,
    // Live Photo audio (mute toggle; #38).
    MuteLiveAudio,
    // Subtitles/captions on a video (toggle; task #90).
    ToggleSubtitles,
    // Cycle to the next subtitle track, Off included (task #99). Separate from the
    // toggle because subtitles have two verbs — an on/off AND a pick — which is also
    // why `Shift` reads as "cycle" here and will read as "reverse" on audio's `A`.
    SubtitleCycle,
    /// `A` / `Shift+A` — step to the next/previous **audio** track (task #99).
    ///
    /// There is no audio on/off twin to `C`: you cannot turn audio off from a track list —
    /// that is Mute, a different question with its own key.
    AudioNext,
    AudioPrev,
    // Docked windowed toolbar (Windows/Linux winit shell only; #61). macOS uses its native
    // toolbar's own Hide Toolbar, so this action is inert there.
    ToggleToolbar,
    /// **Quick Sort** (task #136) — file the displayed photo into slot *n*'s configured
    /// folder with one keypress, then advance. The payload is a **0-based index** into
    /// `settings.quick_sort`; the user-facing number is one more than it
    /// ([`slot_number`](Action::slot_number)), which is why the index is carried rather than
    /// the label — a binding names a *slot*, not whatever folder is in it today.
    ///
    /// Defaults: `1`–`7` for slots 1–7, `Shift+1`–`Shift+7` for slots 8–14; the rest ship
    /// unbound (see [`crate::quick_sort::DEFAULT_BOUND_SLOTS`]).
    QuickSort(u8),
    // Application.
    Settings,
    About,
    Quit,
}

/// The non-quick-sort actions, in stable order. Split out from [`Action::ALL`] only so the
/// per-slot variants can be appended without hand-writing [`crate::quick_sort::SLOT_COUNT`]
/// more lines that would silently drift from it.
const BASE_ACTIONS: &[Action] = &[
    Action::Next,
    Action::Prev,
    Action::Random,
    Action::RandomPrev,
    Action::SkipNext,
    Action::PanLeft,
    Action::PanRight,
    Action::PanUp,
    Action::PanDown,
    Action::ZoomIn,
    Action::ZoomOut,
    Action::ScaleFit,
    Action::ScaleFill,
    Action::ScaleOriginal,
    Action::ToggleOriginal,
    Action::RotateCw,
    Action::RotateCcw,
    Action::ComparePin,
    Action::CompareToggle,
    Action::Copy,
    Action::CopyPath,
    Action::CopyImageDetails,
    Action::CopyImageText,
    Action::ShowImageText,
    Action::DescribeImage,
    Action::AskImage,
    Action::CopyDescription,
    Action::RevealInFileManager,
    Action::SaveRotation,
    Action::Delete,
    Action::DeletePermanent,
    Action::Undo,
    Action::OpenFile,
    Action::OpenFolder,
    Action::Info,
    Action::FullExif,
    Action::Help,
    Action::FolderTree,
    Action::Thumbnails,
    Action::TogglePanels,
    Action::OpenParent,
    Action::PrevFolder,
    Action::NextFolder,
    Action::Fullscreen,
    Action::Recursive,
    Action::ShowArchives,
    Action::CancelScan,
    Action::SlideshowToggle,
    Action::SlideshowFaster,
    Action::SlideshowSlower,
    Action::PlayPause,
    Action::FrameNext,
    Action::FramePrev,
    Action::MuteLiveAudio,
    Action::ToggleSubtitles,
    Action::SubtitleCycle,
    Action::AudioNext,
    Action::AudioPrev,
    Action::ToggleToolbar,
    Action::Settings,
    Action::About,
    Action::Quit,
];

/// Stable snake_case config ids for the quick-sort slots, **1-based** to match what the user
/// sees. Written out rather than generated because `id()` must hand back `&'static str` and
/// `format!` is not const; `quick_sort_ids_match_the_slot_count` keeps it honest.
const QUICK_SORT_IDS: [&str; crate::quick_sort::SLOT_COUNT] = [
    "quick_sort_1",
    "quick_sort_2",
    "quick_sort_3",
    "quick_sort_4",
    "quick_sort_5",
    "quick_sort_6",
    "quick_sort_7",
    "quick_sort_8",
    "quick_sort_9",
    "quick_sort_10",
    "quick_sort_11",
    "quick_sort_12",
    "quick_sort_13",
    "quick_sort_14",
    "quick_sort_15",
    "quick_sort_16",
];

/// The Shortcuts-editor / help-overlay names for the slots. Deliberately the **slot number**,
/// not the configured folder: a binding survives the user repointing the slot, and a label
/// that changed under them would make the editor unreadable. The folder's name appears where
/// it belongs — on the pill the sort flashes, and in Settings ▸ Quick Sort.
const QUICK_SORT_LABELS: [&str; crate::quick_sort::SLOT_COUNT] = [
    "Quick Sort 1",
    "Quick Sort 2",
    "Quick Sort 3",
    "Quick Sort 4",
    "Quick Sort 5",
    "Quick Sort 6",
    "Quick Sort 7",
    "Quick Sort 8",
    "Quick Sort 9",
    "Quick Sort 10",
    "Quick Sort 11",
    "Quick Sort 12",
    "Quick Sort 13",
    "Quick Sort 14",
    "Quick Sort 15",
    "Quick Sort 16",
];

/// [`BASE_ACTIONS`] followed by one [`Action::QuickSort`] per slot. Built in const rather
/// than written out so the slot count is the single source of truth.
const ALL_ACTIONS: [Action; BASE_ACTIONS.len() + crate::quick_sort::SLOT_COUNT] = {
    let mut out = [Action::Next; BASE_ACTIONS.len() + crate::quick_sort::SLOT_COUNT];
    let mut i = 0;
    while i < BASE_ACTIONS.len() {
        out[i] = BASE_ACTIONS[i];
        i += 1;
    }
    let mut slot = 0;
    while slot < crate::quick_sort::SLOT_COUNT {
        out[BASE_ACTIONS.len() + slot] = Action::QuickSort(slot as u8);
        slot += 1;
    }
    out
};

impl Action {
    /// Every action in a stable order — drives the default-binding table, the
    /// id↔action lookup, and config validation.
    pub const ALL: &'static [Action] = &ALL_ACTIONS;

    /// The **1-based** slot number this quick-sort action files into — what the user sees on
    /// the key, in Settings, and in the "no folder set" message. `None` for anything else.
    pub fn slot_number(self) -> Option<usize> {
        match self {
            Action::QuickSort(i) => Some(i as usize + 1),
            _ => None,
        }
    }

    /// Stable snake_case identifier — the key under `[keys]` in the config file.
    pub fn id(self) -> &'static str {
        match self {
            // An out-of-range index cannot arrive from `ALL` or `from_id`; a hand-built one
            // falls back to slot 1's id rather than panicking on user-reachable input.
            Action::QuickSort(i) => QUICK_SORT_IDS
                .get(i as usize)
                .copied()
                .unwrap_or(QUICK_SORT_IDS[0]),
            Action::Next => "next",
            Action::Prev => "prev",
            Action::Random => "random",
            Action::RandomPrev => "random_prev",
            Action::SkipNext => "skip_next",
            Action::PanLeft => "pan_left",
            Action::PanRight => "pan_right",
            Action::PanUp => "pan_up",
            Action::PanDown => "pan_down",
            Action::ZoomIn => "zoom_in",
            Action::ZoomOut => "zoom_out",
            Action::ScaleFit => "scale_fit",
            Action::ScaleFill => "scale_fill",
            Action::ScaleOriginal => "scale_original",
            Action::ToggleOriginal => "toggle_original",
            Action::RotateCw => "rotate_cw",
            Action::RotateCcw => "rotate_ccw",
            Action::ComparePin => "compare_pin",
            Action::CompareToggle => "compare_toggle",
            Action::Copy => "copy",
            Action::CopyPath => "copy_path",
            Action::CopyImageDetails => "copy_image_details",
            Action::CopyImageText => "copy_text",
            Action::ShowImageText => "show_text",
            Action::DescribeImage => "describe",
            Action::AskImage => "ask_image",
            Action::CopyDescription => "copy_description",
            Action::RevealInFileManager => "reveal",
            Action::SaveRotation => "save_rotation",
            Action::Delete => "delete",
            Action::DeletePermanent => "delete_permanent",
            Action::Undo => "undo",
            Action::OpenFile => "open_file",
            Action::OpenFolder => "open_folder",
            Action::Info => "info",
            Action::FullExif => "full_exif",
            Action::Help => "help",
            Action::FolderTree => "folder_tree",
            Action::Thumbnails => "thumbnails",
            Action::TogglePanels => "toggle_panels",
            Action::OpenParent => "open_parent",
            Action::PrevFolder => "prev_folder",
            Action::NextFolder => "next_folder",
            Action::Fullscreen => "fullscreen",
            Action::Recursive => "recursive",
            Action::ShowArchives => "show_archives",
            Action::CancelScan => "cancel_scan",
            Action::SlideshowToggle => "slideshow",
            Action::SlideshowFaster => "slideshow_faster",
            Action::SlideshowSlower => "slideshow_slower",
            Action::PlayPause => "play_pause",
            Action::FrameNext => "frame_next",
            Action::FramePrev => "frame_prev",
            Action::MuteLiveAudio => "mute_live_audio",
            Action::ToggleSubtitles => "toggle_subtitles",
            Action::SubtitleCycle => "subtitle_cycle",
            Action::AudioNext => "audio_next",
            Action::AudioPrev => "audio_prev",
            Action::ToggleToolbar => "toggle_toolbar",
            Action::Settings => "settings",
            Action::About => "about",
            Action::Quit => "quit",
        }
    }

    /// Resolve a config id back to its action (`None` for an unknown id — the
    /// loader warns and skips it). Inverse of [`Action::id`].
    pub fn from_id(s: &str) -> Option<Action> {
        Action::ALL.iter().copied().find(|a| a.id() == s)
    }

    /// Human-readable name for the keybindings editor and (eventually) the help
    /// overlay — the one place these strings live, so the editor and help can't
    /// drift. Sentence-case, plain wording (no em-dashes).
    pub fn label(self) -> &'static str {
        match self {
            Action::QuickSort(i) => QUICK_SORT_LABELS
                .get(i as usize)
                .copied()
                .unwrap_or(QUICK_SORT_LABELS[0]),
            Action::Next => "Next image",
            Action::Prev => "Previous image",
            Action::Random => "Random image",
            Action::RandomPrev => "Previous random image",
            Action::SkipNext => "Next even while playing",
            Action::PanLeft => "Pan left",
            Action::PanRight => "Pan right",
            Action::PanUp => "Pan up",
            Action::PanDown => "Pan down",
            Action::ZoomIn => "Zoom in",
            Action::ZoomOut => "Zoom out",
            Action::ScaleFit => "Fit to screen",
            Action::ScaleFill => "Crop to fill",
            // "(1:1)" ties it to ToggleOriginal's "Toggle 1:1 and fit" and the macOS
            // View menu's "Original 1:1" — three surfaces, one vocabulary.
            Action::ScaleOriginal => "Original size (1:1)",
            Action::ToggleOriginal => "Toggle 1:1 and fit",
            Action::RotateCw => "Rotate clockwise",
            Action::RotateCcw => "Rotate counter-clockwise",
            Action::ComparePin => "Pin for compare",
            Action::CompareToggle => "Compare with pinned",
            Action::Copy => "Copy image",
            Action::CopyPath => "Copy file path",
            Action::CopyImageDetails => "Copy image details",
            Action::CopyImageText => "Copy text from image",
            Action::ShowImageText => "Show text in image",
            Action::DescribeImage => "Describe image",
            Action::AskImage => "Ask about image",
            Action::CopyDescription => "Copy AI description",
            // The platform-idiomatic name (Finder on macOS, File Explorer elsewhere) —
            // the menu bar sets its own label too; this feeds the keybindings editor / help.
            Action::RevealInFileManager => {
                #[cfg(target_os = "macos")]
                {
                    "Show in Finder"
                }
                #[cfg(not(target_os = "macos"))]
                {
                    "Show in File Explorer"
                }
            }
            Action::SaveRotation => "Save rotation",
            Action::Delete => "Delete to Recycle Bin",
            Action::DeletePermanent => "Delete permanently",
            Action::Undo => "Undo",
            Action::OpenFile => "Open file",
            Action::OpenFolder => "Open folder",
            Action::Info => "Info panel",
            Action::FullExif => "Detailed info panel",
            Action::Help => "Keyboard help",
            Action::FolderTree => "Folder tree",
            Action::Thumbnails => "Thumbnails",
            Action::TogglePanels => "Hide/show panels",
            Action::OpenParent => "Open parent folder",
            Action::PrevFolder => "Previous folder",
            Action::NextFolder => "Next folder",
            // A named feature (Title Case, like Apple's "Enter Full Screen") — our
            // borderless speed mode, deliberately distinct from the OS's native full screen.
            Action::Fullscreen => "Quick Full Screen",
            Action::Recursive => "Recursive (current folder)",
            Action::ShowArchives => "Show archives",
            Action::CancelScan => "Stop scanning",
            Action::SlideshowToggle => "Slideshow",
            Action::SlideshowFaster => "Slideshow faster",
            Action::SlideshowSlower => "Slideshow slower",
            Action::PlayPause => "Play/pause animation",
            Action::FrameNext => "Next frame",
            Action::FramePrev => "Previous frame",
            Action::MuteLiveAudio => "Mute Live Photo audio",
            Action::ToggleSubtitles => "Subtitles",
            Action::SubtitleCycle => "Next Subtitle Track",
            Action::AudioNext => "Next Audio Track",
            Action::AudioPrev => "Previous Audio Track",
            Action::ToggleToolbar => "Show toolbar",
            Action::Settings => "Settings",
            Action::About => "About",
            Action::Quit => "Quit",
        }
    }

    /// How the input layer drives this action.
    pub fn kind(self) -> ActionKind {
        match self {
            Action::Next
            | Action::Prev
            | Action::Random
            | Action::RandomPrev
            | Action::SkipNext => ActionKind::Nav,
            Action::PanLeft
            | Action::PanRight
            | Action::PanUp
            | Action::PanDown
            | Action::ZoomIn
            | Action::ZoomOut => ActionKind::Held,
            Action::FrameNext | Action::FramePrev => ActionKind::FrameStep,
            _ => ActionKind::OneShot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_nonempty() {
        let mut seen = std::collections::HashSet::new();
        for &a in Action::ALL {
            let id = a.id();
            assert!(!id.is_empty(), "{a:?} has an empty id");
            assert!(seen.insert(id), "duplicate id {id:?}");
        }
    }

    #[test]
    fn from_id_round_trips_every_action() {
        for &a in Action::ALL {
            assert_eq!(
                Action::from_id(a.id()),
                Some(a),
                "round-trip failed for {a:?}"
            );
        }
    }

    #[test]
    fn from_id_rejects_unknown() {
        assert_eq!(Action::from_id("not_an_action"), None);
        assert_eq!(Action::from_id(""), None);
        assert_eq!(Action::from_id("NEXT"), None); // case-sensitive
    }

    #[test]
    fn every_action_has_a_nonempty_label() {
        for &a in Action::ALL {
            assert!(!a.label().is_empty(), "{a:?} has an empty label");
        }
    }

    #[test]
    fn kinds_match_their_groups() {
        assert_eq!(Action::Next.kind(), ActionKind::Nav);
        assert_eq!(Action::RandomPrev.kind(), ActionKind::Nav);
        assert_eq!(Action::PanLeft.kind(), ActionKind::Held);
        assert_eq!(Action::ZoomOut.kind(), ActionKind::Held);
        assert_eq!(Action::Copy.kind(), ActionKind::OneShot);
        assert_eq!(Action::ToggleOriginal.kind(), ActionKind::OneShot);
        assert_eq!(Action::Quit.kind(), ActionKind::OneShot);
        assert_eq!(Action::PlayPause.kind(), ActionKind::OneShot);
        assert_eq!(Action::FrameNext.kind(), ActionKind::FrameStep);
        assert_eq!(Action::FramePrev.kind(), ActionKind::FrameStep);
    }
}
