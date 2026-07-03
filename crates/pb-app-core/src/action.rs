//! The central, named set of user actions — the single vocabulary shared by the
//! keymap (`keymap.rs`), the native menu (`menu.rs`), and the keybindings help
//! overlay (task #8). One enum is what keeps the hotkeys, the menu, and the help
//! table from drifting apart: every command is dispatched through `Action`.
//!
//! Pure data (no winit, no I/O), so the id/kind tables are unit-tested here.

/// How an action is driven from the keyboard — the press handler needs this to
/// know whether to fire once, start a hold-to-fly, or track a continuous hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActionKind {
    /// Fires once per key-down (OS auto-repeat ignored): rotate, copy, toggle a
    /// panel, open Settings, etc.
    OneShot,
    /// Photo navigation — fires on press and then repeats via hold-to-fly
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
    // Navigation (hold-to-fly).
    Next,
    Prev,
    Random,
    RandomPrev,
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
    // Application.
    Settings,
    About,
    Quit,
}

impl Action {
    /// Every action in a stable order — drives the default-binding table, the
    /// id↔action lookup, and config validation.
    pub const ALL: &'static [Action] = &[
        Action::Next,
        Action::Prev,
        Action::Random,
        Action::RandomPrev,
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
        Action::OpenParent,
        Action::PrevFolder,
        Action::NextFolder,
        Action::Fullscreen,
        Action::Recursive,
        Action::CancelScan,
        Action::SlideshowToggle,
        Action::SlideshowFaster,
        Action::SlideshowSlower,
        Action::PlayPause,
        Action::FrameNext,
        Action::FramePrev,
        Action::MuteLiveAudio,
        Action::Settings,
        Action::About,
        Action::Quit,
    ];

    /// Stable snake_case identifier — the key under `[keys]` in the config file.
    pub fn id(self) -> &'static str {
        match self {
            Action::Next => "next",
            Action::Prev => "prev",
            Action::Random => "random",
            Action::RandomPrev => "random_prev",
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
            Action::OpenParent => "open_parent",
            Action::PrevFolder => "prev_folder",
            Action::NextFolder => "next_folder",
            Action::Fullscreen => "fullscreen",
            Action::Recursive => "recursive",
            Action::CancelScan => "cancel_scan",
            Action::SlideshowToggle => "slideshow",
            Action::SlideshowFaster => "slideshow_faster",
            Action::SlideshowSlower => "slideshow_slower",
            Action::PlayPause => "play_pause",
            Action::FrameNext => "frame_next",
            Action::FramePrev => "frame_prev",
            Action::MuteLiveAudio => "mute_live_audio",
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
            Action::Next => "Next image",
            Action::Prev => "Previous image",
            Action::Random => "Random image",
            Action::RandomPrev => "Previous random image",
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
            Action::OpenParent => "Open parent folder",
            Action::PrevFolder => "Previous folder",
            Action::NextFolder => "Next folder",
            Action::Fullscreen => "Toggle fullscreen",
            Action::Recursive => "Recursive (current folder)",
            Action::CancelScan => "Stop scanning",
            Action::SlideshowToggle => "Slideshow",
            Action::SlideshowFaster => "Slideshow faster",
            Action::SlideshowSlower => "Slideshow slower",
            Action::PlayPause => "Play/pause animation",
            Action::FrameNext => "Next frame",
            Action::FramePrev => "Previous frame",
            Action::MuteLiveAudio => "Mute Live Photo audio",
            Action::Settings => "Settings",
            Action::About => "About",
            Action::Quit => "Quit",
        }
    }

    /// How the input layer drives this action.
    pub fn kind(self) -> ActionKind {
        match self {
            Action::Next | Action::Prev | Action::Random | Action::RandomPrev => ActionKind::Nav,
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
