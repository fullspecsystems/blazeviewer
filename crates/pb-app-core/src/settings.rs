//! Persisted user preferences — a typed model serialized to `settings.toml`.
//!
//! **Privacy boundary (task #2):** this writes ONLY app preferences (navigation
//! feel, default scale / recursive, the letterbox color, info-panel opacity, the
//! windowed/fullscreen choice, and the last windowed position + size) to the OS
//! config dir. It never records anything photo-*derived* (no viewed paths, no recent
//! list, no thumbnails) — app config is explicitly in-bounds (ADR-018; the no-trace
//! test is scoped to photo data only). Writes happen only on an explicit user action
//! (Settings ▸ Save, the fullscreen toggle, or moving/resizing the window), never on
//! the view/decode path.
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

/// What a plain (no-modifier) scroll does — a mouse wheel or a precision-trackpad
/// two-finger swipe. The *other* action is always reachable by holding Ctrl, so
/// this only swaps which one is the unmodified default. (macOS trackpad swipes
/// arrive as pixel-precise pan events and always pan; this governs wheel/line
/// scrolling, which is all Windows surfaces.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrollAction {
    #[default]
    Pan,
    Zoom,
}

/// The viewer's light/dark preference (task #46): `System` (the default) follows the
/// OS theme live; `Light` / `Dark` pin it. Drives the HUD color scheme
/// (`pb_hud::hud::Theme`), which letterbox color fills the image view
/// ([`Settings::letterbox_for`]), and the chrome dialogs' theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppearanceMode {
    #[default]
    System,
    Light,
    Dark,
}

/// Where the chrome accent color comes from (task: brand-vs-system accent). `System` (the
/// default) follows the OS accent — the user's Windows *Settings ▸ Colors* pick — falling back
/// to the brand when the OS has none (Linux today) or the pick is illegible; `Custom` uses
/// [`Settings::accent_custom`]; `Brand` pins the PhotoBlaze orange. The shell resolves this to a
/// concrete color (with a contrast guard) and pushes it to `pb_ui::set_accent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccentSource {
    #[default]
    System,
    Custom,
    Brand,
}

/// Horizontal placement of the basic info line (`i`) along the bottom edge
/// (task #54). `Center` (the default) shares the bottom-center with the toast
/// (which stacks above); `Right` shares the corner with the Inspector (which
/// lifts above it); `Left` shares with the folder tree (which caps its height).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InfoLineAlign {
    Left,
    #[default]
    Center,
    Right,
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

/// Which backend generates AI image descriptions (task #44). `Auto` (the default)
/// prefers Apple's on-device Foundation Models when the host reports it available
/// (macOS 27+, Apple Intelligence on), else falls back to a configured local endpoint;
/// `AppleOnDevice` / `LocalEndpoint` pin one explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DescribeBackend {
    #[default]
    Auto,
    AppleOnDevice,
    LocalEndpoint,
}

/// The last windowed-mode geometry: the window's outer (decorated) top-left and its
/// inner (client) size, in physical pixels, in the virtual-desktop coordinate space.
/// Persisted so toggling back to windowed — and the next launch — restore where the
/// user left the window rather than snapping to the OS default corner (#1). Restored
/// only when [`geometry_on_screen`] confirms enough of it still lands on a connected
/// monitor (guards against a saved spot going off-screen after a monitor change).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
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
    /// Hold-to-blaze: starting advance rate in photos/sec — the ramp's floor (#19).
    pub start_speed: f32,
    /// Hold-to-blaze: seconds to ramp from `start_speed` up to the ceiling (#19).
    pub ramp_secs: f32,
    /// Hold-to-blaze ceiling in photos/sec; `0` = uncapped (the display refresh, #20).
    pub max_advance_rate: u32,
    /// Initial delay (ms) before a held nav key begins auto-repeating.
    pub hold_delay_ms: u32,
    /// What a plain scroll (mouse wheel or precision-trackpad two-finger swipe)
    /// does; the other action is always reachable with Ctrl held. Default `Pan`.
    pub scroll_action: ScrollAction,
    /// Default scale mode for a freshly shown photo.
    pub scale_mode: ScaleModePref,
    /// Light/dark preference (task #46): System follows the OS; Light/Dark pin it.
    pub appearance_mode: AppearanceMode,
    /// Where the chrome accent comes from: `System` (follow the OS accent), `Custom`
    /// ([`accent_custom`](Self::accent_custom)), or `Brand` (the PhotoBlaze orange).
    pub accent_source: AccentSource,
    /// The custom accent color (sRGB) used when [`accent_source`](Self::accent_source) is
    /// `Custom`. Defaults to the Windows accent blue — a good, neutral starting point to tweak
    /// from (rather than the brand orange, which the `Brand` option already covers).
    pub accent_custom: [u8; 3],
    /// Where the basic info line (`i`) sits along the bottom edge (task #54).
    /// Default `Center`.
    pub info_line_align: InfoLineAlign,
    /// Whether the info line (`i`) starts shown on a fresh launch — the default state; the `i`
    /// key still toggles it live, and flipping this in Settings applies at once (task #54).
    pub show_image_info: bool,
    /// Transparent toolbar (task #59, macOS): extend the photo under a translucent glass
    /// toolbar so a zoomed/cropped image shows under it (fit mode is unchanged). Default `true`
    /// (the most Mac-like look); a legibility scrim keeps the title readable. Windowed-mode only.
    pub glass_toolbar: bool,
    /// Show the windowed **toolbar** — the docked, mouse-driven strip of nav / view
    /// affordances under the menu bar (task #61, Windows/Linux winit shell). Default `true`
    /// for discoverability; a power user turns it off to reclaim the strip (the keyboard does
    /// everything without it). Windowed-mode only — the fullscreen speed mode is chrome-free.
    /// Applied live. macOS uses its native toolbar (Hide Toolbar), so it ignores this key.
    pub show_toolbar: bool,
    /// Which fields the info line shows: folder, file name, resolution (W×H), codec. Applied
    /// live. Folder is prepended to the file name with a `/` (the relative dir when the scan is
    /// recursive, else the containing folder's name). The line hides if the enabled fields
    /// produce no text (an empty pill reads as a bug).
    pub info_show_folder: bool,
    pub info_show_filename: bool,
    pub info_show_resolution: bool,
    pub info_show_codec: bool,
    /// Letterbox / background fill (sRGB) shown behind a non-filling image, in **dark**
    /// mode (the pre-#46 `letterbox` key, so existing files keep their chosen color).
    pub letterbox: [u8; 3],
    /// The **light**-mode letterbox / background fill (task #46).
    pub letterbox_light: [u8; 3],
    /// Info-panel background opacity, `0` (transparent) – `100` (opaque).
    pub info_opacity: u8,
    /// Native rich-panel (folder tree / inspector / scan pill / toast) background opacity,
    /// `50`–`100` — lets the user see more of the photo through the chrome. Defaults high to
    /// preserve contrast (roughly the pre-slider look). Only the macOS native panels read it.
    pub panel_opacity: u8,
    /// Default slideshow interval in seconds — the per-slide dwell a fresh session
    /// starts at (#31). Clamped to the slideshow's own `[MIN, MAX]_INTERVAL`; the
    /// `[` / `]` keys still adjust it live for the session without rewriting this.
    pub slideshow_interval_secs: f64,
    /// Last windowed position + size (#1); `None` until the user has been windowed at
    /// least once. Only restored when still on a connected monitor (`geometry_on_screen`).
    pub window: Option<WindowGeometry>,
    /// Where the Open dialog starts. `None` (default) = the current photo's folder;
    /// `Some(dir)` = always start in this folder. This is a **user-chosen preference**
    /// (set deliberately in Settings), not a record of viewed paths — so it stays within
    /// the no-trace boundary. Pinning a folder also stops the OS dialog from surfacing its
    /// own last-folder memory on the next launch.
    pub picker_dir: Option<PathBuf>,
    /// The folder of the most recent folder/file open — the Open dialog's default
    /// start on a fresh launch (nothing open yet), so the picker lands back in your
    /// library. It never auto-opens anything (owner call, 2026-07-03: a bare launch
    /// shows the empty open screen; the brief reopen-last-folder behavior is gone).
    /// A pinned `picker_dir` takes precedence. **A deliberate, owner-approved
    /// exception (2026-07-02) to the no-viewing-trace rule:** one folder path — never
    /// file names — updated only when the opened folder changes, written by the same
    /// explicit open action that loads it (never passively while viewing).
    pub last_folder: Option<PathBuf>,
    /// Play a Live Photo's audio when its motion plays (#38). Muted via `M` / the Image
    /// menu; persisted so the choice sticks. Default on (audio plays).
    pub mute_live_audio: bool,
    /// Show subtitles/captions on a video when a track is available (task #90). Toggled
    /// via `C` / the View menu; persisted so the choice sticks. Default off.
    ///
    /// This is a *preference*, not a viewing trace (privacy #2): it records that the user
    /// likes captions, never which video or which track.
    pub subtitles: bool,
    /// Which backend generates AI image descriptions (task #44). Default `Auto`.
    pub describe_backend: DescribeBackend,
    /// The OpenAI-compatible endpoint base URL for the `LocalEndpoint` backend
    /// (LM Studio default). Only ever contacted on an explicit describe/ask command
    /// (or opt-in dwell) — never on the view path.
    pub describe_endpoint: String,
    /// Model name to request from the endpoint; empty = the endpoint's loaded model.
    pub describe_model: String,
    /// Describe automatically after the user parks on an image (opt-in — a passive
    /// send must be a deliberate election, since the endpoint could be remote). Default off.
    pub describe_auto: bool,
    /// Speak the description aloud via the platform TTS when it arrives (task #44,
    /// subtask 7). Default off. Toggled from the Image menu.
    pub speak_descriptions: bool,
    /// A custom prompt template. `None` (empty in the UI) uses the built-in accessibility
    /// instruction. Placeholders: `{context}` / `{filename}` / `{folder}` / `{datetime}` /
    /// `{camera}` / `{location}` (see `prompt::build_prompt`).
    pub describe_prompt: Option<String>,
    /// Response length cap for the endpoint backend (max tokens). The UI offers presets
    /// (Brief 256 / Standard 512 / Detailed 1024); a hand-set value is honored and snaps
    /// to the nearest preset in the picker. Clamped to a sane range.
    pub describe_max_tokens: u32,
    /// Floating-panel positions (task #54): where the user last dragged each rich
    /// panel, in **logical points**, top-left origin, `None` = the panel's default
    /// home. Written on drag-end (an explicit user gesture) once the presenters land;
    /// clamped back on-screen at restore ([`clamp_panel_pos`]). Layout is app
    /// footprint, not a viewing trace (ADR-018) — open/closed state is deliberately
    /// NOT persisted, so a fresh launch always starts clean.
    pub panel_pos_inspector: Option<(f32, f32)>,
    /// The folder-tree panel's dragged position (see `panel_pos_inspector`).
    pub panel_pos_tree: Option<(f32, f32)>,
    /// The Help panel's dragged position (see `panel_pos_inspector`).
    pub panel_pos_help: Option<(f32, f32)>,
}

impl Default for Settings {
    /// The defaults mirror today's in-code constants, so a fresh install behaves
    /// exactly as before any settings file exists.
    fn default() -> Self {
        Self {
            fullscreen: false,
            startup_mode: StartupMode::Remember,
            recursive: true,
            // A gentle floor that ramps up to the refresh ceiling, so hold-to-blaze
            // shows off its acceleration by default rather than starting at full tilt.
            start_speed: 2.0,    // photos/sec the ramp starts from (#19)
            ramp_secs: 5.0,      // seconds to reach the ceiling (#19)
            max_advance_rate: 0, // uncapped → display refresh is the ceiling (#20)
            hold_delay_ms: 250,  // tap→repeat handoff; 200 made accidental blazing too easy
            scroll_action: ScrollAction::Pan, // scroll pans; Ctrl+scroll zooms
            scale_mode: ScaleModePref::Fit,
            appearance_mode: AppearanceMode::System, // follow the OS light/dark theme
            accent_source: AccentSource::System,     // follow the OS accent (brand fallback)
            accent_custom: [0x00, 0x78, 0xd4],       // Windows accent blue — a good neutral
            // starting point when you switch to Custom
            info_line_align: InfoLineAlign::Center, // bottom-center, stacks with the toast
            show_image_info: false,                 // off until opted in (unchanged launch)
            glass_toolbar: true,                    // transparent toolbar on by default (#59)
            show_toolbar: true,                     // docked toolbar on by default (#61)
            info_show_folder: false,                // opt-in — filename alone by default
            info_show_filename: true,
            info_show_resolution: true,
            info_show_codec: true,
            letterbox: [10, 10, 12],          // pb_render::LETTERBOX (rgb)
            letterbox_light: [240, 241, 245], // the light-mode analog (#46)
            info_opacity: 60,                 // hud::BG alpha 153/255 ≈ 60%
            panel_opacity: 92,                // conservative-high (≈ today's material look)
            slideshow_interval_secs: slideshow::DEFAULT_INTERVAL.as_secs_f64(), // 4.0
            window: None,
            picker_dir: None,       // start in the current photo's folder
            last_folder: None,      // no folder to reopen until the first open
            mute_live_audio: false, // Live Photo audio plays by default (#38)
            subtitles: false,       // captions off until asked for (task #90)
            describe_backend: DescribeBackend::Auto,
            // LM Studio's default; a bare install of Ollama uses :11434 instead.
            describe_endpoint: "http://localhost:1234/v1".to_string(),
            describe_model: String::new(), // the endpoint's loaded model
            describe_auto: false,          // opt-in (privacy: passive send is deliberate)
            speak_descriptions: false,
            describe_prompt: None,     // built-in accessibility instruction
            describe_max_tokens: 512,  // "Standard" length preset
            panel_pos_inspector: None, // default homes until the user drags (task #54)
            panel_pos_tree: None,
            panel_pos_help: None,
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
        self.panel_opacity = self.panel_opacity.clamp(50, 100);
        // Keep the response cap sane (a stray 0 would ask for an empty reply; a huge value
        // could stall the panel). Covers the presets 256/512/1024 with headroom.
        self.describe_max_tokens = self.describe_max_tokens.clamp(16, 4096);
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

    /// The letterbox / background fill for a **resolved** dark/light flag (task #46).
    /// The caller resolves [`AppearanceMode`] against the live OS theme first
    /// (`AppCore::effective_dark`); this just picks the matching color.
    pub fn letterbox_for(&self, dark: bool) -> [u8; 3] {
        if dark {
            self.letterbox
        } else {
            self.letterbox_light
        }
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
        let body = format!(
            "# {} settings (preferences only, never viewing data)\n{toml}",
            crate::APP_NAME
        );
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, body).is_err() {
            return false;
        }
        std::fs::rename(&tmp, &path).is_ok()
    }
}

fn settings_path() -> Option<PathBuf> {
    // The per-user config dir now lives in `pb-app-core` (shared with the keymap
    // loader); settings.toml sits beside keymap.toml in it.
    crate::config_dir().map(|d| d.join("settings.toml"))
}

/// The raw `settings.toml` text, if the file exists and is readable.
fn read_settings_text() -> Option<String> {
    std::fs::read_to_string(settings_path()?).ok()
}

/// Minimum overlap (physical px) a restored window must share with a single monitor
/// to count as visible — enough of the window, including its title bar, to see and
/// grab. Below this the saved spot is treated as off-screen and the default is used.
pub const MIN_VISIBLE_W: u32 = 200;
pub const MIN_VISIBLE_H: u32 = 80;

/// Whether `geom` overlaps some monitor by at least `min_w`×`min_h` physical px, so a
/// restored window lands where the user can actually see and drag it. Each monitor is
/// `(x, y, w, h)` in the same physical-pixel virtual-desktop space as `geom`. Pure (no
/// winit types) so the off-screen guard is unit-testable. Requiring the overlap with a
/// *single* monitor (not the union) keeps a grabbable chunk on one screen rather than
/// scattered slivers across several.
pub fn geometry_on_screen(
    geom: WindowGeometry,
    monitors: &[(i32, i32, u32, u32)],
    min_w: u32,
    min_h: u32,
) -> bool {
    let gx1 = geom.x.saturating_add(geom.w as i32);
    let gy1 = geom.y.saturating_add(geom.h as i32);
    monitors.iter().any(|&(mx, my, mw, mh)| {
        let mx1 = mx.saturating_add(mw as i32);
        let my1 = my.saturating_add(mh as i32);
        let ox = (gx1.min(mx1) - geom.x.max(mx)).max(0);
        let oy = (gy1.min(my1) - geom.y.max(my)).max(0);
        ox as u32 >= min_w && oy as u32 >= min_h
    })
}

/// Clamp a persisted floating-panel position so the whole panel stays inside the
/// window (task #54): a spot saved on a bigger window / different backing scale must
/// come back grabbable, never off-screen. All values are logical points, top-left
/// origin; `panel` is the panel's current size, `win` the window's client size. A
/// panel larger than the window pins to the top-left (the title bar stays reachable).
/// Pure and total, so restore-time safety is unit-testable.
pub fn clamp_panel_pos(pos: (f32, f32), panel: (f32, f32), win: (f32, f32)) -> (f32, f32) {
    // A hand-edited/garbage coordinate (NaN/∞) resets to the origin, like
    // `Settings::clamp` does for other non-finite floats.
    let sane = |v: f32| if v.is_finite() { v } else { 0.0 };
    let max_x = (win.0 - panel.0).max(0.0);
    let max_y = (win.1 - panel.1).max(0.0);
    (sane(pos.0).clamp(0.0, max_x), sane(pos.1).clamp(0.0, max_y))
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
    fn panel_positions_round_trip_and_default_to_none() {
        let mut s = Settings::default();
        assert_eq!(s.panel_pos_inspector, None);
        s.panel_pos_inspector = Some((120.5, 40.0));
        s.panel_pos_tree = Some((0.0, 0.0));
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.panel_pos_inspector, Some((120.5, 40.0)));
        assert_eq!(back.panel_pos_tree, Some((0.0, 0.0)));
        assert_eq!(back.panel_pos_help, None);
    }

    #[test]
    fn clamp_panel_pos_keeps_the_panel_inside_the_window() {
        // In-bounds passes through untouched.
        assert_eq!(
            clamp_panel_pos((100.0, 50.0), (300.0, 200.0), (1000.0, 800.0)),
            (100.0, 50.0)
        );
        // Saved on a bigger window: pulled back so the whole panel fits.
        assert_eq!(
            clamp_panel_pos((900.0, 700.0), (300.0, 200.0), (1000.0, 800.0)),
            (700.0, 600.0)
        );
        // Negative (dragged off the top-left on some setups): pinned to origin.
        assert_eq!(
            clamp_panel_pos((-50.0, -10.0), (300.0, 200.0), (1000.0, 800.0)),
            (0.0, 0.0)
        );
        // Panel larger than the window: top-left, title bar reachable.
        assert_eq!(
            clamp_panel_pos((100.0, 100.0), (2000.0, 900.0), (1000.0, 800.0)),
            (0.0, 0.0)
        );
        // Garbage coordinates reset to the origin instead of poisoning the layout.
        assert_eq!(
            clamp_panel_pos((f32::NAN, f32::INFINITY), (300.0, 200.0), (1000.0, 800.0)),
            (0.0, 0.0)
        );
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
    fn appearance_mode_round_trips_and_defaults_to_system() {
        // Absent in old files → System (follow the OS, the pre-#46 behavior).
        let s: Settings = toml::from_str("fullscreen = true\n").unwrap();
        assert_eq!(s.appearance_mode, AppearanceMode::System);

        for (text, mode) in [
            ("appearance_mode = \"system\"", AppearanceMode::System),
            ("appearance_mode = \"light\"", AppearanceMode::Light),
            ("appearance_mode = \"dark\"", AppearanceMode::Dark),
        ] {
            assert_eq!(
                toml::from_str::<Settings>(text).unwrap().appearance_mode,
                mode
            );
        }
    }

    #[test]
    fn letterbox_for_picks_the_theme_matching_fill() {
        let s = Settings {
            letterbox: [1, 2, 3],
            letterbox_light: [201, 202, 203],
            ..Settings::default()
        };
        assert_eq!(s.letterbox_for(true), [1, 2, 3]);
        assert_eq!(s.letterbox_for(false), [201, 202, 203]);

        // An old file (no letterbox_light) still gets the light default.
        let s: Settings = toml::from_str("letterbox = [9, 9, 9]\n").unwrap();
        assert_eq!(s.letterbox_for(true), [9, 9, 9]);
        assert_eq!(s.letterbox_for(false), Settings::default().letterbox_light);
    }

    #[test]
    fn malformed_toml_is_not_accepted() {
        // `load()` would fall back to defaults; the parse itself must error.
        assert!(toml::from_str::<Settings>("this is = = not valid").is_err());
    }

    #[test]
    fn picker_dir_round_trips_and_defaults_to_none() {
        // Absent in old files → None (start in the current photo's folder).
        let s: Settings = toml::from_str("fullscreen = true\n").unwrap();
        assert_eq!(s.picker_dir, None);

        // A pinned folder round-trips through TOML.
        let s = Settings {
            picker_dir: Some(PathBuf::from("/home/jd/Pictures")),
            ..Settings::default()
        };
        let back: Settings = toml::from_str(&toml::to_string_pretty(&s).unwrap()).unwrap();
        assert_eq!(s.picker_dir, back.picker_dir);
    }

    #[test]
    fn window_geometry_round_trips_and_defaults_to_none() {
        // Absent in old files → None (no window restore, falls back to the default spot).
        let s: Settings = toml::from_str("fullscreen = true\n").unwrap();
        assert_eq!(s.window, None);

        // Present → restored exactly.
        let s = Settings {
            window: Some(WindowGeometry {
                x: -100,
                y: 40,
                w: 1280,
                h: 800,
            }),
            ..Settings::default()
        };
        let back: Settings = toml::from_str(&toml::to_string_pretty(&s).unwrap()).unwrap();
        assert_eq!(s.window, back.window);
    }

    /// A primary monitor at the origin plus a second one to its left (negative x), the
    /// classic dual-monitor layout the off-screen guard has to get right.
    const MONS: &[(i32, i32, u32, u32)] = &[(0, 0, 1920, 1080), (-1920, 0, 1920, 1080)];

    fn geom(x: i32, y: i32) -> WindowGeometry {
        WindowGeometry {
            x,
            y,
            w: 1280,
            h: 800,
        }
    }

    #[test]
    fn geometry_on_screen_accepts_a_fully_visible_window() {
        assert!(geometry_on_screen(
            geom(100, 100),
            MONS,
            MIN_VISIBLE_W,
            MIN_VISIBLE_H
        ));
        // Fully on the left (negative-x) monitor counts too.
        assert!(geometry_on_screen(
            geom(-1800, 100),
            MONS,
            MIN_VISIBLE_W,
            MIN_VISIBLE_H
        ));
    }

    #[test]
    fn geometry_on_screen_rejects_a_fully_off_screen_window() {
        // Far to the right of every monitor (e.g. the second display was unplugged).
        assert!(!geometry_on_screen(
            geom(10_000, 100),
            MONS,
            MIN_VISIBLE_W,
            MIN_VISIBLE_H
        ));
        // Above every monitor.
        assert!(!geometry_on_screen(
            geom(100, -5_000),
            MONS,
            MIN_VISIBLE_W,
            MIN_VISIBLE_H
        ));
        // No monitors at all → never visible.
        assert!(!geometry_on_screen(
            geom(0, 0),
            &[],
            MIN_VISIBLE_W,
            MIN_VISIBLE_H
        ));
    }

    #[test]
    fn geometry_on_screen_thresholds_a_sliver() {
        // Window pushed right so only 150px overlap the primary monitor — below the
        // 200px minimum, so it's treated as off-screen (not enough to grab).
        assert!(!geometry_on_screen(
            geom(1920 - 150, 100),
            MONS,
            MIN_VISIBLE_W,
            MIN_VISIBLE_H
        ));
        // Pull it back so 300px overlap — comfortably grabbable.
        assert!(geometry_on_screen(
            geom(1920 - 300, 100),
            MONS,
            MIN_VISIBLE_W,
            MIN_VISIBLE_H
        ));
    }

    #[test]
    fn geometry_on_screen_requires_a_visible_title_bar_strip() {
        // Dropped almost entirely below the monitor: only 40px of height remain on
        // screen, under the 80px minimum, so the title bar isn't reachable → rejected.
        assert!(!geometry_on_screen(
            geom(100, 1080 - 40),
            MONS,
            MIN_VISIBLE_W,
            MIN_VISIBLE_H
        ));
    }
}
