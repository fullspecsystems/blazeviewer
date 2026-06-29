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
    // File operations.
    Copy,
    SaveRotation,
    Delete,
    DeletePermanent,
    OpenFile,
    OpenFolder,
    // View toggles / panels.
    Info,
    FullExif,
    Help,
    Fullscreen,
    Recursive,
    // Slideshow (timer-driven advance).
    SlideshowToggle,
    SlideshowFaster,
    SlideshowSlower,
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
        Action::Copy,
        Action::SaveRotation,
        Action::Delete,
        Action::DeletePermanent,
        Action::OpenFile,
        Action::OpenFolder,
        Action::Info,
        Action::FullExif,
        Action::Help,
        Action::Fullscreen,
        Action::Recursive,
        Action::SlideshowToggle,
        Action::SlideshowFaster,
        Action::SlideshowSlower,
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
            Action::Copy => "copy",
            Action::SaveRotation => "save_rotation",
            Action::Delete => "delete",
            Action::DeletePermanent => "delete_permanent",
            Action::OpenFile => "open_file",
            Action::OpenFolder => "open_folder",
            Action::Info => "info",
            Action::FullExif => "full_exif",
            Action::Help => "help",
            Action::Fullscreen => "fullscreen",
            Action::Recursive => "recursive",
            Action::SlideshowToggle => "slideshow",
            Action::SlideshowFaster => "slideshow_faster",
            Action::SlideshowSlower => "slideshow_slower",
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
            Action::Next => "Next photo",
            Action::Prev => "Previous photo",
            Action::Random => "Random photo",
            Action::RandomPrev => "Previous random photo",
            Action::PanLeft => "Pan left",
            Action::PanRight => "Pan right",
            Action::PanUp => "Pan up",
            Action::PanDown => "Pan down",
            Action::ZoomIn => "Zoom in",
            Action::ZoomOut => "Zoom out",
            Action::ScaleFit => "Fit to screen",
            Action::ScaleFill => "Crop to fill",
            Action::ScaleOriginal => "Original size",
            Action::ToggleOriginal => "Toggle 1:1 and fit",
            Action::RotateCw => "Rotate clockwise",
            Action::RotateCcw => "Rotate counter-clockwise",
            Action::Copy => "Copy image",
            Action::SaveRotation => "Save rotation",
            Action::Delete => "Delete to Recycle Bin",
            Action::DeletePermanent => "Delete permanently",
            Action::OpenFile => "Open file",
            Action::OpenFolder => "Open folder",
            Action::Info => "Info panel",
            Action::FullExif => "Full EXIF panel",
            Action::Help => "Keyboard help",
            Action::Fullscreen => "Toggle fullscreen",
            Action::Recursive => "Recursive (current folder)",
            Action::SlideshowToggle => "Slideshow",
            Action::SlideshowFaster => "Slideshow faster",
            Action::SlideshowSlower => "Slideshow slower",
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
    }
}
