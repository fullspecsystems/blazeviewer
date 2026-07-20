//! **Menu state** — the `AppCore` methods that project core state onto the shells' menus
//! and the context menu (task #125).
//!
//! A *topic*. `menu_state_from` is a pure projection: every checkbox and enabled/disabled
//! flag a shell draws is derived here, so muda (Windows/Linux) and the native macOS menus
//! cannot drift apart on what the app thinks is true.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Build the [`contract::MenuState`] for the given live state — the pure mapping from
    /// the app's view/edit state to the shell-neutral menu model. Takes no `self` and
    /// touches no muda, so it's unit-tested directly (`menu_state_*` tests). The
    /// mappings it owns are the only non-trivial logic: `pb_render::ScaleMode` → the
    /// View scale group, and the panel state → the info checkmarks (the basic line and
    /// the Inspector's Details tab check independently — they decoupled in task #54)
    /// plus the Hide Panels toggle (checked while hidden, enabled only with a panel
    /// open — matching the `Tab` no-op).
    #[allow(clippy::too_many_arguments)]
    pub fn menu_state_from(
        scale: ScaleMode,
        info_line: bool,
        panels: Panels,
        tree_open: bool,
        recursive: bool,
        fullscreen: bool,
        slideshow: bool,
        mute_live_audio: bool,
        subtitles: bool,
        save_rotation_enabled: bool,
        reveal_enabled: bool,
        cancel_scan_enabled: bool,
        undo: Option<String>,
        native_fullscreen_engaged: bool,
        displayed_item: Option<usize>,
        compare_pin: Option<usize>,
    ) -> contract::MenuState {
        contract::MenuState {
            scale: match scale {
                ScaleMode::Fit => contract::ScaleMode::Fit,
                ScaleMode::Fill => contract::ScaleMode::Fill,
                ScaleMode::Original => contract::ScaleMode::Original,
            },
            info_basic: info_line,
            // The Details tab checks whether visible or Tab-hidden — hidden ≠ closed,
            // and the Hide Panels checkmark explains the invisibility.
            info_full: panels.inspector == Some(InspectorTab::Details),
            panels_hidden: panels.hidden,
            hide_panels_enabled: panels.any_open(tree_open, info_line),
            recursive,
            fullscreen,
            slideshow,
            // The docked toolbar (#61) is a shell-honored setting, not derived from view state,
            // so the choke point defaults it off and the shell overrides it from `settings`.
            show_toolbar: false,
            // Show Archives (task #104) is likewise a setting, not derived view state: default
            // it off here and let each shell override it from `settings.show_archives`.
            show_archives: false,
            mute_live_audio,
            subtitles,
            // Compare (task #43): both raw states cross so the derivation lives HERE,
            // the one choke point, instead of drifting per shell.
            compare_pin_enabled: displayed_item.is_some(),
            compare_pinned_here: displayed_item.is_some() && displayed_item == compare_pin,
            compare_toggle_enabled: compare_pin.is_some() && displayed_item.is_some(),
            save_rotation_enabled,
            reveal_enabled,
            cancel_scan_enabled,
            undo,
            native_fullscreen_engaged,
        }
    }

    /// Right-click over the photo (task #41): ask the shell to pop up the **photo context
    /// menu** at the cursor. Fills a shell-neutral [`contract::ContextMenuState`] from live
    /// state (Play only when the photo has motion, Show in Finder/Explorer only for a real
    /// on-disk file) and pushes [`contract::CoreEffect::ShowContextMenu`]. Over the empty
    /// deck there's nothing per-photo to offer, so no menu is shown.
    pub fn show_context_menu(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        let state = contract::ContextMenuState {
            has_image: true,
            has_motion: self.has_motion(item) || self.item_is_video(item),
            can_reveal: self.source.path(item).is_some(),
            fullscreen: !self.windowed,
            compare_pinned: self.compare_pin.is_some(),
            compare_pinned_here: self.compare_pin == Some(item),
        };
        self.effects
            .push(contract::CoreEffect::ShowContextMenu(state));
    }
}
