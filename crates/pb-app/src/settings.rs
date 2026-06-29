//! Persisted user preferences — a typed model serialized to `settings.toml`.
//!
//! **Privacy boundary (task #2):** this writes ONLY app preferences (navigation
//! feel, default scale / recursive, the letterbox color, info-panel opacity, the
//! windowed/fullscreen choice) to the OS config dir. It never records anything
//! photo-*derived* (no viewed paths, no recent list, no thumbnails) — app config is
//! explicitly in-bounds (ADR-018; the no-trace test is scoped to photo data only).
//! Writes happen only on an explicit user action (Settings ▸ Save, or the
//! fullscreen toggle), never on the view/decode path.
//!
//! All I/O is best-effort: a missing / unreadable / malformed file means "use
//! defaults," and a failed write is silently ignored (a preference not sticking must
//! never break viewing). The file is TOML; an older `key = value` `fullscreen` file
//! is a valid TOML subset, so it still loads (its other fields default).

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::slideshow;

/// The default scale mode applied to a freshly shown photo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScaleModePref {
    #[default]
    Fit,
    Fill,
    Original,
}

/// How the viewer chooses windowed vs. fullscreen at launch.
///
/// `Remember` (the default) restores whatever the window was last in — which is
/// exactly the old behavior, since the runtime fullscreen toggle persists
/// [`Settings::fullscreen`] on every change. `Fullscreen` / `Windowed` pin it.
/// A CLI `--fullscreen` / `--windowed` flag still overrides whatever this says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StartupMode {
    Fullscreen,
    Windowed,
    #[default]
    Remember,
}

/// All persisted preferences. `#[serde(default)]` makes any missing key fall back to
/// [`Settings::default`], so partial / older files (e.g. one that only set
/// `fullscreen`) load cleanly, and unknown keys are ignored — forward/backward
/// compatible as the schema grows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The last window mode the user was in (borderless fullscreen vs. a window),
    /// updated on every runtime toggle. Backs [`StartupMode::Remember`].
    pub fullscreen: bool,
    /// Whether to start fullscreen, windowed, or restore the last mode (#22).
    pub startup_mode: StartupMode,
    /// Open folders recursively by default (picker / drag-drop / association).
    pub recursive: bool,
    /// Hold-to-fly: starting advance rate in photos/sec — the ramp's floor (#19).
    pub start_speed: f32,
    /// Hold-to-fly: seconds to ramp from `start_speed` up to the ceiling (#19).
    pub ramp_secs: f32,
    /// Hold-to-fly ceiling in photos/sec; `0` = uncapped (the display refresh, #20).
    pub max_advance_rate: u32,
    /// Initial delay (ms) before a held nav key begins auto-repeating.
    pub hold_delay_ms: u32,
    /// Default scale mode for a freshly shown photo.
    pub scale_mode: ScaleModePref,
    /// Letterbox / background fill (sRGB) shown behind a non-filling image.
    pub letterbox: [u8; 3],
    /// Info-panel background opacity, `0` (transparent) – `100` (opaque).
    pub info_opacity: u8,
    /// Default slideshow interval in seconds — the per-slide dwell a fresh session
    /// starts at (#31). Clamped to the slideshow's own `[MIN, MAX]_INTERVAL`; the
    /// `[` / `]` keys still adjust it live for the session without rewriting this.
    pub slideshow_interval_secs: f64,
}

impl Default for Settings {
    /// The defaults mirror today's in-code constants, so a fresh install behaves
    /// exactly as before any settings file exists.
    fn default() -> Self {
        Self {
            fullscreen: false,
            startup_mode: StartupMode::Remember,
            recursive: true,
            start_speed: 3.0,    // main.rs ADVANCE_MIN_RATE
            ramp_secs: 4.0,      // main.rs ADVANCE_RAMP_SECS
            max_advance_rate: 0, // uncapped → display refresh (#20)
            hold_delay_ms: 400,  // main.rs initial_delay
            scale_mode: ScaleModePref::Fit,
            letterbox: [10, 10, 12], // pb_render::LETTERBOX (rgb)
            info_opacity: 60,        // hud::BG alpha 153/255 ≈ 60%
            slideshow_interval_secs: slideshow::DEFAULT_INTERVAL.as_secs_f64(), // 4.0
        }
    }
}

impl Settings {
    /// Clamp every field into a sane range (defends against a hand-edited or garbage
    /// file). Non-finite floats reset to their default. Idempotent.
    pub fn clamp(&mut self) {
        let d = Settings::default();
        if !self.start_speed.is_finite() {
            self.start_speed = d.start_speed;
        }
        if !self.ramp_secs.is_finite() {
            self.ramp_secs = d.ramp_secs;
        }
        self.start_speed = self.start_speed.clamp(1.0, 60.0);
        self.ramp_secs = self.ramp_secs.clamp(0.0, 30.0);
        self.max_advance_rate = self.max_advance_rate.min(1000);
        self.hold_delay_ms = self.hold_delay_ms.min(2000);
        self.info_opacity = self.info_opacity.min(100);
        if !self.slideshow_interval_secs.is_finite() {
            self.slideshow_interval_secs = d.slideshow_interval_secs;
        }
        // Reuse the slideshow's own bounds so this default and the `[`/`]` live adjust
        // share one clamp (`max(0.0)` guards `from_secs_f64` against a negative panic).
        self.slideshow_interval_secs = slideshow::clamp_interval(Duration::from_secs_f64(
            self.slideshow_interval_secs.max(0.0),
        ))
        .as_secs_f64();
    }

    /// The effective "start fullscreen?" decision for this launch, resolving
    /// [`StartupMode::Remember`] against the saved last mode.
    pub fn start_fullscreen(&self) -> bool {
        match self.startup_mode {
            StartupMode::Fullscreen => true,
            StartupMode::Windowed => false,
            StartupMode::Remember => self.fullscreen,
        }
    }

    /// Load the settings, clamped. A missing / unreadable / malformed file yields the
    /// defaults (never an error — viewing must work with no config).
    pub fn load() -> Settings {
        let mut s = read_settings_text()
            .and_then(|t| toml::from_str::<Settings>(&t).ok())
            .unwrap_or_default();
        s.clamp();
        s
    }

    /// Persist the settings to `settings.toml`, atomically (write a temp file then
    /// rename, so a crash mid-write can't truncate the real file). Best-effort:
    /// returns whether it was written. An explicit user action only (privacy #2).
    pub fn save(&self) -> bool {
        let Some(path) = settings_path() else {
            return false;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(toml) = toml::to_string_pretty(self) else {
            return false;
        };
        let body = format!("# PhotoBlaze settings (preferences only, never photo data)\n{toml}");
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, body).is_err() {
            return false;
        }
        std::fs::rename(&tmp, &path).is_ok()
    }
}

/// Per-user config directory for PhotoBlaze (created on demand), or `None` if the
/// platform's config location can't be determined. Shared with the keymap loader
/// (`keymap::read_config`), which reads `keymap.toml` from the same directory.
pub(crate) fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("PhotoBlaze"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/PhotoBlaze"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join("photoblaze"))
    }
}

fn settings_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("settings.toml"))
}

/// The raw `settings.toml` text, if the file exists and is readable.
fn read_settings_text() -> Option<String> {
    std::fs::read_to_string(settings_path()?).ok()
}

/// Persist the fullscreen preference, preserving every other setting (load, set,
/// save the whole file). Best-effort; an explicit user action (the toggle).
pub fn save_fullscreen(fullscreen: bool) {
    let mut s = Settings::load();
    s.fullscreen = fullscreen;
    s.save();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_through_toml() {
        let s = Settings::default();
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        // Only `fullscreen` set; everything else defaults.
        let s: Settings = toml::from_str("fullscreen = true\n").unwrap();
        assert!(s.fullscreen);
        assert_eq!(s.start_speed, Settings::default().start_speed);
        assert_eq!(s.scale_mode, ScaleModePref::Fit);
        assert_eq!(s.info_opacity, 60);
    }

    #[test]
    fn old_key_value_fullscreen_file_still_loads() {
        // The previous format was a `key = value` subset of TOML.
        let s = toml::from_str::<Settings>("fullscreen = false\n").unwrap();
        assert!(!s.fullscreen);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let s: Settings =
            toml::from_str("fullscreen = true\nlegacy_option = 42\n").expect("ignore unknown");
        assert!(s.fullscreen);
    }

    #[test]
    fn scale_mode_round_trips_each_variant() {
        for (text, mode) in [
            ("scale_mode = \"fit\"", ScaleModePref::Fit),
            ("scale_mode = \"fill\"", ScaleModePref::Fill),
            ("scale_mode = \"original\"", ScaleModePref::Original),
        ] {
            assert_eq!(toml::from_str::<Settings>(text).unwrap().scale_mode, mode);
        }
    }

    #[test]
    fn clamp_bounds_out_of_range_values() {
        let mut s = Settings {
            start_speed: 1000.0,
            ramp_secs: -5.0,
            max_advance_rate: 99_999,
            hold_delay_ms: 60_000,
            info_opacity: 200,
            ..Settings::default()
        };
        s.clamp();
        assert_eq!(s.start_speed, 60.0);
        assert_eq!(s.ramp_secs, 0.0);
        assert_eq!(s.max_advance_rate, 1000);
        assert_eq!(s.hold_delay_ms, 2000);
        assert_eq!(s.info_opacity, 100);
    }

    #[test]
    fn clamp_resets_non_finite_floats() {
        let mut s = Settings {
            start_speed: f32::NAN,
            ramp_secs: f32::INFINITY,
            ..Settings::default()
        };
        s.clamp();
        assert_eq!(s.start_speed, Settings::default().start_speed);
        assert_eq!(s.ramp_secs, Settings::default().ramp_secs);
    }

    #[test]
    fn slideshow_interval_defaults_when_missing() {
        // A file that doesn't mention slideshow gets the shared default (4.0s).
        let s: Settings = toml::from_str("fullscreen = true\n").unwrap();
        assert_eq!(
            s.slideshow_interval_secs,
            slideshow::DEFAULT_INTERVAL.as_secs_f64()
        );
    }

    #[test]
    fn slideshow_interval_clamps_to_the_shared_bounds() {
        let lo = slideshow::MIN_INTERVAL.as_secs_f64();
        let hi = slideshow::MAX_INTERVAL.as_secs_f64();

        let mut s = Settings {
            slideshow_interval_secs: 9999.0,
            ..Settings::default()
        };
        s.clamp();
        assert_eq!(s.slideshow_interval_secs, hi);

        let mut s = Settings {
            slideshow_interval_secs: -5.0,
            ..Settings::default()
        };
        s.clamp();
        assert_eq!(s.slideshow_interval_secs, lo);

        // Non-finite resets to the default rather than panicking in `from_secs_f64`.
        let mut s = Settings {
            slideshow_interval_secs: f64::NAN,
            ..Settings::default()
        };
        s.clamp();
        assert_eq!(
            s.slideshow_interval_secs,
            slideshow::DEFAULT_INTERVAL.as_secs_f64()
        );
    }

    #[test]
    fn startup_mode_defaults_to_remember_and_resolves_to_last() {
        // An old file with only `fullscreen` (no startup_mode) keeps the old
        // "remember the last mode" behavior.
        let s = toml::from_str::<Settings>("fullscreen = true\n").unwrap();
        assert_eq!(s.startup_mode, StartupMode::Remember);
        assert!(
            s.start_fullscreen(),
            "Remember resolves to the saved fullscreen"
        );

        let s = toml::from_str::<Settings>("fullscreen = false\n").unwrap();
        assert!(!s.start_fullscreen());
    }

    #[test]
    fn startup_mode_pins_override_the_saved_last_mode() {
        // Fullscreen / Windowed ignore the remembered `fullscreen` bool entirely.
        let s = Settings {
            startup_mode: StartupMode::Fullscreen,
            fullscreen: false,
            ..Settings::default()
        };
        assert!(s.start_fullscreen());

        let s = Settings {
            startup_mode: StartupMode::Windowed,
            fullscreen: true,
            ..Settings::default()
        };
        assert!(!s.start_fullscreen());
    }

    #[test]
    fn startup_mode_round_trips_each_variant() {
        for (text, mode) in [
            ("startup_mode = \"fullscreen\"", StartupMode::Fullscreen),
            ("startup_mode = \"windowed\"", StartupMode::Windowed),
            ("startup_mode = \"remember\"", StartupMode::Remember),
        ] {
            assert_eq!(toml::from_str::<Settings>(text).unwrap().startup_mode, mode);
        }
    }

    #[test]
    fn malformed_toml_is_not_accepted() {
        // `load()` would fall back to defaults; the parse itself must error.
        assert!(toml::from_str::<Settings>("this is = = not valid").is_err());
    }
}
