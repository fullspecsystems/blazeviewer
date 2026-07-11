//! `LaunchOverrides` — one-shot, session-only launch intents parsed from the CLI
//! (task #78).
//!
//! Kept here in the platform-neutral core (not the winit shell) so it is **plain data
//! the core consumes** and the settings-shaped folding is pure + unit-tested. It is
//! deliberately **clap-free**: `pb-app-core` stays toml-only per NS0, so the shell's
//! `cli` module owns the clap `Cli` derive and maps it into this struct.
//!
//! **Never persisted.** These override the loaded [`Settings`] for the current launch
//! only — a scripted `--theme dark --slideshow=5` run must not rewrite the user's
//! config. The shell folds the settings-shaped fields onto a `Settings` *copy*
//! ([`LaunchOverrides::apply_to_settings`]) and applies the launch-only fields while
//! building the core; neither path calls `Settings::save`.

use crate::app_core::Nav;
use crate::settings::{AppearanceMode, ScaleModePref, Settings, StartupMode};

/// Where a `--start-at` launch should begin. Resolved against the playlist once a
/// (possibly deferred / streamed) scan has listed its entries — a folder scan does not
/// know its full deck at parse time, so the shell applies this after the first resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartAt {
    /// 1-based position from the command line (`--start-at 100`). Clamped to the deck
    /// at resolve time.
    Index(usize),
    /// First entry whose file name matches (`--start-at sunset.jpg`), compared
    /// case-insensitively on the basename.
    Name(String),
}

/// The initial slideshow intent from `--slideshow[=SECS]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlideshowStart {
    /// Explicit per-slide seconds from `--slideshow=SECS`, already clamped to
    /// `[slideshow::MIN_INTERVAL, MAX_INTERVAL]` by the CLI mapping (Mixed strictness:
    /// an out-of-range value clamps, it does not error). `None` = `--slideshow` with no
    /// value ⇒ start a slideshow at the saved / default interval.
    pub interval_secs: Option<f64>,
}

/// Session-only launch overrides parsed from the CLI. Every field follows the same
/// rule: **unset ⇒ leave the loaded preference alone**; a set field wins for this
/// launch only. Split into two groups by *where* the shell applies them — the
/// settings-shaped group folds onto a [`Settings`] copy via [`Self::apply_to_settings`];
/// the launch-only group is applied while constructing the core.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LaunchOverrides {
    // --- settings-shaped: folded onto a `Settings` copy (apply_to_settings) ---
    /// `--windowed` ⇒ `Some(true)`, `--fullscreen` ⇒ `Some(false)`. Last-one-wins is
    /// resolved by the CLI layer (clap `overrides_with`), so at most one is set here.
    pub windowed: Option<bool>,
    /// `--recursive` ⇒ `Some(true)`, `--no-recursive` ⇒ `Some(false)`.
    pub recursive: Option<bool>,
    /// `--info` ⇒ `Some(true)`, `--no-info` ⇒ `Some(false)` (the `i` info line).
    pub show_info: Option<bool>,
    /// `--scale <fit|fill|original>`.
    pub scale: Option<ScaleModePref>,
    /// `--theme <light|dark|system>`.
    pub theme: Option<AppearanceMode>,
    /// `--mute` ⇒ `Some(true)` (force-mute Live Photo audio for this launch). There is
    /// no un-mute flag, so this is never `Some(false)` from the CLI today.
    pub mute: Option<bool>,

    // --- launch-only state: applied by the shell while building the core ---
    /// `--details` — open the Inspector (Details tab) on launch. Panels start closed and
    /// are not a persisted preference, so this is a one-way "open it" flag.
    pub open_details: bool,
    /// `--folders` — open the folder tree on launch. One-way, like `open_details`.
    pub open_folders: bool,
    /// `--slideshow[=SECS]` — start a slideshow, optionally at `SECS`.
    pub slideshow: Option<SlideshowStart>,
    /// `--shuffle` — advance in the precomputed random order (see [`Self::launch_nav`]).
    pub shuffle: bool,
    /// `--reverse` — advance backward (see [`Self::launch_nav`]).
    pub reverse: bool,
    /// `--start-at <N|name>`.
    pub start_at: Option<StartAt>,
    /// `--new-window` — reserved parse-accepted **no-op** today: the bypass seam for the
    /// future single-instance task (#1), so clap accepts it and scripts can adopt it now.
    pub new_window: bool,
    /// `--metrics` — the hidden dev stage-timing report on exit.
    pub metrics: bool,
}

impl LaunchOverrides {
    /// The initial `last_nav` direction implied by `--shuffle` / `--reverse`. The
    /// slideshow and hold-to-fly both auto-advance in `last_nav`
    /// (`app_core_impl.rs`: the per-tick `advance(self.last_nav)`), and starting a
    /// slideshow does not reset it, so the entire `--shuffle` / `--reverse` feature is
    /// this one mapping over the four existing [`Nav`] variants:
    ///
    /// | `shuffle` | `reverse` | [`Nav`] | slideshow plays |
    /// |-----------|-----------|---------|-----------------|
    /// | `false`   | `false`   | `Forward`     | 1 → 2 → 3 … |
    /// | `false`   | `true`    | `Backward`    | … 3 → 2 → 1 |
    /// | `true`    | `false`   | `Random`      | random walk forward |
    /// | `true`    | `true`    | `RandomPrev`  | random walk backward (the `Shift+Enter` action) |
    pub fn launch_nav(&self) -> Nav {
        match (self.shuffle, self.reverse) {
            (false, false) => Nav::Forward,
            (false, true) => Nav::Backward,
            (true, false) => Nav::Random,
            (true, true) => Nav::RandomPrev,
        }
    }

    /// Fold the settings-shaped overrides onto a loaded [`Settings`] copy. **Pure** —
    /// never touches disk. The shell calls this on `Settings::load()` before building
    /// the core; every unset field leaves the saved preference intact.
    ///
    /// `--windowed` / `--fullscreen` fold through [`StartupMode`] (+ the `fullscreen`
    /// mirror) so the existing `windowed = !settings.start_fullscreen()` launch logic
    /// resolves the flag with no special-casing.
    pub fn apply_to_settings(&self, s: &mut Settings) {
        if let Some(windowed) = self.windowed {
            s.startup_mode = if windowed {
                StartupMode::Windowed
            } else {
                StartupMode::Fullscreen
            };
            s.fullscreen = !windowed;
        }
        if let Some(recursive) = self.recursive {
            s.recursive = recursive;
        }
        if let Some(show) = self.show_info {
            s.show_image_info = show;
        }
        if let Some(scale) = self.scale {
            s.scale_mode = scale;
        }
        if let Some(theme) = self.theme {
            s.appearance_mode = theme;
        }
        if let Some(mute) = self.mute {
            s.mute_live_audio = mute;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_overrides_are_all_unset() {
        let o = LaunchOverrides::default();
        assert_eq!(o.launch_nav(), Nav::Forward);
        // Folding a no-op override onto default settings changes nothing.
        let mut s = Settings::default();
        let before = s.clone();
        o.apply_to_settings(&mut s);
        assert_eq!(s, before);
    }

    #[test]
    fn launch_nav_maps_shuffle_and_reverse_over_the_four_variants() {
        let nav = |shuffle, reverse| {
            LaunchOverrides {
                shuffle,
                reverse,
                ..Default::default()
            }
            .launch_nav()
        };
        assert_eq!(nav(false, false), Nav::Forward);
        assert_eq!(nav(false, true), Nav::Backward);
        assert_eq!(nav(true, false), Nav::Random);
        // --shuffle --reverse == reverse shuffle == the Shift+Enter (RandomPrev) action.
        assert_eq!(nav(true, true), Nav::RandomPrev);
    }

    #[test]
    fn windowed_flag_pins_startup_mode_so_start_fullscreen_resolves() {
        let mut s = Settings::default();
        LaunchOverrides {
            windowed: Some(true),
            ..Default::default()
        }
        .apply_to_settings(&mut s);
        assert_eq!(s.startup_mode, StartupMode::Windowed);
        assert!(!s.start_fullscreen(), "--windowed ⇒ launch windowed");

        let mut s = Settings::default();
        LaunchOverrides {
            windowed: Some(false),
            ..Default::default()
        }
        .apply_to_settings(&mut s);
        assert_eq!(s.startup_mode, StartupMode::Fullscreen);
        assert!(s.start_fullscreen(), "--fullscreen ⇒ launch fullscreen");
    }

    #[test]
    fn unset_window_flag_preserves_a_saved_startup_mode() {
        let mut s = Settings {
            startup_mode: StartupMode::Fullscreen,
            ..Settings::default()
        };
        // No --windowed / --fullscreen on the command line: the saved pref stands.
        LaunchOverrides::default().apply_to_settings(&mut s);
        assert_eq!(s.startup_mode, StartupMode::Fullscreen);
    }

    #[test]
    fn each_settings_shaped_field_folds_independently() {
        let mut s = Settings::default();
        LaunchOverrides {
            recursive: Some(true),
            show_info: Some(true),
            scale: Some(ScaleModePref::Fill),
            theme: Some(AppearanceMode::Dark),
            mute: Some(true),
            ..Default::default()
        }
        .apply_to_settings(&mut s);
        assert!(s.recursive);
        assert!(s.show_image_info);
        assert_eq!(s.scale_mode, ScaleModePref::Fill);
        assert_eq!(s.appearance_mode, AppearanceMode::Dark);
        assert!(s.mute_live_audio);
    }

    #[test]
    fn a_single_override_leaves_the_other_fields_untouched() {
        let base = Settings::default();
        let mut s = base.clone();
        LaunchOverrides {
            theme: Some(AppearanceMode::Light),
            ..Default::default()
        }
        .apply_to_settings(&mut s);
        // Only appearance_mode moved; everything else equals the untouched baseline.
        assert_eq!(s.appearance_mode, AppearanceMode::Light);
        assert_eq!(s.recursive, base.recursive);
        assert_eq!(s.scale_mode, base.scale_mode);
        assert_eq!(s.show_image_info, base.show_image_info);
        assert_eq!(s.mute_live_audio, base.mute_live_audio);
        assert_eq!(s.startup_mode, base.startup_mode);
    }
}
