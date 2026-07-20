//! **Settings, keymap and theme** — the `AppCore` methods that apply user preferences
//! (task #125, the first concern split out of `app_core_impl.rs`).
//!
//! A separate `impl AppCore` block, not a separate type: Rust lets an inherent impl span
//! several modules in one crate, so every method here moved **byte-identically** from
//! `app_core_impl.rs` with no change to call sites, signatures or visibility. That is what
//! makes the split provable rather than merely tested — see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Apply the keymap edited in the Settings dialog: swap it in live (every keypress
    /// resolves through `self.keymap`, so future input uses it immediately) and persist
    /// `keymap.toml`. If the help overlay is open, rebuild it so its key labels — read
    /// from the live keymap — reflect the new bindings.
    pub fn apply_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
        self.keymap.save();
        if self.overlay_shown && self.slot_content() == Some(SlotContent::Help) {
            self.show_overlay();
        }
        // The Help panel's shortcut labels just changed — nudge a native Help view to
        // re-pull (visibility didn't change, so the tick diff wouldn't catch it).
        if self.help_panel_visible() {
            self.emit_panels_changed();
        }
    }

    /// Re-apply the resolved appearance (task #46): retint the HUD's color scheme, set
    /// the effective letterbox fill, and — only when the resolved dark/light actually
    /// flipped — rebuild every visible overlay bitmap (they were composited with the
    /// old scheme) through the same invalidation a DPI change uses. Runs on
    /// `OsThemeChanged`, inside [`apply_settings`](Self::apply_settings), and from the
    /// shells right after the renderer stands up.
    pub fn refresh_theme(&mut self) {
        let dark = self.effective_dark();
        if let Some(r) = self.renderer.as_mut() {
            r.set_letterbox(self.settings.letterbox_for(dark));
        }
        if let Some(h) = self.hud.as_mut() {
            h.set_theme(hud::Theme::of(dark));
        }
        if dark != self.hud_dark {
            self.hud_dark = dark;
            // Pie / scan card / info panel / tree / open hint all re-rasterize with the
            // new scheme. A plain transient toast keeps its old-scheme bitmap (it fades
            // out within ~1.3 s and its source content isn't retained).
            self.rescale_overlays();
        }
    }

    /// Apply the settings the user saved in the dialog: swap in the new model, apply
    /// the parts that aren't read live (hold delay, appearance + letterbox color,
    /// default scale mode), then persist to disk (an explicit user action — privacy
    /// #2). The nav-feel rates (start speed / ramp / max) and the info-panel opacity
    /// are read live, so swapping `self.settings` is enough for those.
    pub fn apply_settings(&mut self, new: settings::Settings) {
        let old = std::mem::replace(&mut self.settings, new);

        // Held-key repeat delay is cached on the struct (the curve below reads the
        // rates live, but this one is a Duration captured at construction).
        self.initial_delay = Duration::from_millis(self.settings.hold_delay_ms as u64);

        // Default slideshow interval → the live timer. A running slideshow's deadline is
        // `last_present + interval`, recomputed each tick, so this takes effect at once
        // (the `[`/`]` live override is just a different write to the same field).
        self.slideshow.interval = Duration::from_secs_f64(self.settings.slideshow_interval_secs);

        // Appearance + letterbox / background fill → HUD scheme + renderer (task #46);
        // rebuilds the overlay bitmaps when the resolved theme flipped.
        self.refresh_theme();

        // Default scale mode: apply live if it changed (re-frames + reloads at the new
        // fit). `set_scale_mode` redraws for us.
        let scale_changed = old.scale_mode != self.settings.scale_mode;
        if scale_changed {
            self.set_scale_mode(scale_mode_of(self.settings.scale_mode));
        }

        // An explicit Settings change to theme / mute supersedes a CLI session override
        // (--theme / --mute) for the rest of this launch, so the dialog choice takes effect and
        // the override no longer masks it (and the saved value below is the user's real choice).
        if old.appearance_mode != self.settings.appearance_mode {
            self.launch.theme = None;
        }
        if old.mute_live_audio != self.settings.mute_live_audio {
            self.launch.mute = None;
        }

        // Subtitle appearance → the live engine (task #90.4). The rasterizer caches on
        // (text, params), so a changed style rebuilds the bitmap on the very next tick —
        // which is what makes the Settings preview and a playing film agree.
        self.subtitles.style = self.settings.subtitle_style.clone();
        // …and the forced-subtitles preference (task #99). Same lesson as post-mortem bug
        // #2: a preference that only reaches the engine at construction saves to disk and
        // does nothing until relaunch, which reads as the setting being broken. The next
        // tick re-resolves through `resolve_display`, so turning it off drops the signs
        // immediately (`tick_subtitles`'s single clearing exit) rather than at next launch.
        self.subtitles.selection.always_forced = self.settings.forced_subtitles;

        // Persist the whole model (atomic write; best-effort).
        self.settings.save();

        // A new info-line alignment (or opacity/theme) re-places the line at once, which
        // re-lifts/re-caps its colliders (panel + tree) for the new span.
        if old.info_line_align != self.settings.info_line_align && self.info_line_shown {
            self.show_info_line();
        }

        // The "show image info" default also applies live — flipping it in Settings shows or
        // hides the current line, not just the next launch.
        if old.show_image_info != self.settings.show_image_info {
            self.info_line = self.settings.show_image_info;
        }
        // The field toggles (filename / resolution / codec) are read live by info_line_*(), so
        // if the line is up, re-place it to reflect the new content — or hide it if the change
        // left no fields on (info_line_visible now returns false).
        let fields_changed = old.info_show_folder != self.settings.info_show_folder
            || old.info_show_filename != self.settings.info_show_filename
            || old.info_show_resolution != self.settings.info_show_resolution
            || old.info_show_codec != self.settings.info_show_codec;
        if self.info_line
            && (fields_changed || old.show_image_info != self.settings.show_image_info)
        {
            if self.info_line_visible() {
                self.show_info_line();
            } else {
                self.hide_info_line();
            }
        } else if !self.info_line && old.show_image_info != self.settings.show_image_info {
            self.hide_info_line();
        }

        // Redraw so the new letterbox shows even when the scale mode didn't change,
        // and rebuild the info panel so a new opacity takes effect immediately.
        if self.overlay_shown {
            self.show_overlay();
        } else if !scale_changed {
            self.draw();
        }

        // Re-pull the native panels: their opacity (and theme) come from settings but aren't
        // in the panel snapshot, so without this a *natively* presented tree/inspector (the
        // egui overlay on winit; the SwiftUI panels on mac) wouldn't pick up a Panel-opacity
        // change until the next unrelated repaint.
        self.emit_panels_changed();
    }

    /// The primary *keymap* binding for an action (numpad alternates skipped), bypassing
    /// [`help_shortcut`](Self::help_shortcut)'s menu-accelerator preference. For the help
    /// rows that teach the viewer's bare-key habits — Open O / ⇧O, Quit Esc — where the
    /// ⌘-chord is the one every Mac user already knows (owner call, 2026-07-03).
    pub fn keymap_shortcut(&self, action: Action) -> String {
        self.keymap
            .bindings_for(action)
            .iter()
            .find(|c| !c.code.is_numpad())
            .map(|c| c.shortcut_label())
            .unwrap_or_default()
    }
}
