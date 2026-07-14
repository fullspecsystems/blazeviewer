//! Our own dialog windows (Settings, About), rendered with **egui** in a second
//! winit window with its own small wgpu surface.
//!
//! Why ours instead of a native dialog: a native Win32 TaskDialog can't show a
//! large custom icon or follow the OS dark theme. egui gives both for free (and
//! ports to macOS later). The dialog only runs while open — off the photo hot path.
//! egui is locked to the OS-resolved light/dark theme at open, and the `pbui`
//! design-system style (tokens + components) is reasserted each frame on top.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_wgpu::wgpu;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Theme, Window, WindowId};

use pb_decode::{decode_bytes, FitBox};
use pb_source::OpenProgress;

use crate::action::Action;
use crate::keymap::{KeyChord, Keymap};
use crate::settings;
use crate::ScanProgress;
use pb_ui as pbui;

/// Which dialog a [`DialogWindow`] is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    About,
    Settings,
    /// A themed Yes/No confirmation (e.g. permanent delete). The prompt text is
    /// carried on the [`DialogWindow`]; the answer surfaces via
    /// [`DialogWindow::take_confirm_result`].
    Confirm,
    /// A one-button informational / error notice (e.g. an archive-open failure):
    /// a warning icon + the body text + a single OK button. The body is carried on
    /// the [`DialogWindow`]; OK / Esc close it (OK surfaces as
    /// [`take_confirm_result`] `Some(true)`). Opened via [`crate::App::open_message`].
    ///
    /// [`take_confirm_result`]: DialogWindow::take_confirm_result
    Message,
    /// An archive password prompt: a lock icon + the prompt, a masked text field,
    /// an optional inline error (after a wrong attempt), and an Unlock/Cancel bar.
    /// Unlock surfaces as [`take_confirm_result`] `Some(true)` with the entered text
    /// available via [`take_submitted_password`]; Cancel / Esc surface `Some(false)`
    /// / close it. Opened via [`crate::App::prompt_archive_password`].
    ///
    /// [`take_confirm_result`]: DialogWindow::take_confirm_result
    /// [`take_submitted_password`]: DialogWindow::take_submitted_password
    Password,
    /// The "Ask about image" prompt (task #44): a sparkles icon + a **multi-line** text
    /// field for a question about the current photo, and an Ask/Cancel bar. Ask surfaces as
    /// [`take_confirm_result`] `Some(true)` with the text via [`take_ask_result`]; the core
    /// runs it through the describe backend. Cancel / Esc close it.
    ///
    /// [`take_confirm_result`]: DialogWindow::take_confirm_result
    /// [`take_ask_result`]: DialogWindow::take_ask_result
    AskImage,
    /// An archive-loading progress view: a message, a determinate progress bar driven
    /// by a [`pb_source::OpenProgress`] handle, the bytes/percent done, and a single
    /// **Cancel** button. Cancel (or Esc) requests cancellation of the in-flight eager
    /// 7z decode. A `Password` dialog turns into this in place once the password is
    /// verified (see [`become_loading`](DialogWindow::become_loading)); the non-password
    /// paths open it fresh.
    Loading,
    /// A folder-scanning progress view: a message (the folder being opened), an
    /// *indeterminate* spinner with a live "N images found" count and the subfolder
    /// currently being walked, and a single **Cancel** button. A directory walk has no
    /// knowable total (a pre-count would walk the tree twice), so — unlike
    /// [`Loading`](DialogKind::Loading) — there's no determinate bar; the count + folder
    /// are the progress. Driven by a [`crate::ScanProgress`] handle the off-thread walk
    /// updates; Cancel / Esc request cancellation of the in-flight scan. Opened (deferred
    /// to slow scans) by [`crate::App::poll_dir_scan`].
    Scanning,
}

/// Which section of the Settings dialog is showing. Edits auto-save live across every
/// tab (the bottom bar is just **Done**); tabs only switch what's visible.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    #[default]
    General,
    Display,
    Ai,
    Shortcuts,
}

/// Response-length presets for the AI describe backend (`describe_max_tokens`), shown as
/// Brief / Standard / Detailed. A hand-set value snaps to the nearest on open.
const LEN_PRESETS: [u32; 3] = [256, 512, 1024];

/// Nearest response-length preset index for a saved `max_tokens`.
fn len_preset_index(max_tokens: u32) -> usize {
    LEN_PRESETS
        .iter()
        .enumerate()
        .min_by_key(|(_, v)| v.abs_diff(max_tokens))
        .map(|(i, _)| i)
        .unwrap_or(1)
}

/// The Test-connection state for the AI tab: the probe runs off-thread so the dialog stays
/// responsive, and the result is shown inline.
#[derive(Default)]
enum ConnTest {
    #[default]
    Idle,
    Testing(std::sync::mpsc::Receiver<Result<Vec<String>, String>>),
    /// A finished probe: `ok` colors the line (green/amber/red), `msg` is the summary.
    Done {
        ok: bool,
        msg: String,
    },
}

/// The egui-facing edit state for the Settings form — the same fields as
/// [`settings::Settings`] but in shapes the egui widgets want (a combo index, an
/// `f32` color). Built from the live settings on open ([`SettingsDraft::from_settings`])
/// and folded back live on every edit ([`SettingsDraft::to_settings`]) — auto-save.
struct SettingsDraft {
    /// Display refresh in Hz — caps the max-speed slider (not persisted).
    refresh_hz: u32,
    start_speed: f32,
    ramp_secs: f32,
    max_fps: u32,
    hold_delay_ms: u32,
    scroll_action: usize, // 0 = Pan, 1 = Zoom (what a plain scroll does)
    recursive: bool,
    scale_mode: usize,      // 0 = Fit, 1 = Fill, 2 = Original
    appearance: usize,      // 0 = System, 1 = Light, 2 = Dark (#46)
    accent_source: usize,   // 0 = System, 1 = Custom, 2 = Blaze Orange (accent color)
    accent_custom: [u8; 3], // sRGB bytes — the custom accent (egui `color_edit_button_srgb`)
    info_line_align: usize, // 0 = Left, 1 = Center, 2 = Right (task #54)
    // The docked windowed toolbar (task #61).
    show_toolbar: bool,
    // Image-info readout (`i`): the launch default + which fields it lists (task #54).
    show_image_info: bool,
    info_show_folder: bool,
    info_show_filename: bool,
    info_show_resolution: bool,
    info_show_codec: bool,
    letterbox: [f32; 3], // 0..1 per channel (egui color picker) — the dark fill
    letterbox_light: [f32; 3], // the light-mode fill (#46)
    info_opacity: u8,    // 0..100
    startup_mode: usize, // 0 = Fullscreen, 1 = Windowed, 2 = Remember
    slideshow_interval: f64, // seconds (default slideshow dwell)
    /// File-picker start: `false` = the current photo's folder, `true` = a pinned folder.
    picker_fixed: bool,
    /// The pinned folder (when `picker_fixed`); `None` until the user chooses one.
    picker_dir: Option<PathBuf>,
    // --- AI descriptions (task #44) ---
    describe_backend: usize, // 0 = Auto, 1 = Apple on-device, 2 = Local endpoint
    describe_endpoint: String,
    describe_model: String,
    /// Custom prompt; empty = the built-in accessibility instruction.
    describe_prompt: String,
    describe_length: usize, // index into LEN_PRESETS (Brief / Standard / Detailed)
    describe_auto: bool,
    speak_descriptions: bool,
}

impl SettingsDraft {
    /// Build the draft from the persisted model. `refresh_hz` caps the max-speed
    /// slider; an uncapped (`0`) or ≥refresh saved rate shows pinned at the ceiling.
    fn from_settings(s: &settings::Settings, refresh_hz: u32) -> Self {
        let hz = refresh_hz.max(1);
        let max_fps = if s.max_advance_rate == 0 || s.max_advance_rate >= hz {
            hz
        } else {
            s.max_advance_rate
        };
        Self {
            refresh_hz: hz,
            start_speed: s.start_speed,
            ramp_secs: s.ramp_secs,
            scroll_action: match s.scroll_action {
                settings::ScrollAction::Pan => 0,
                settings::ScrollAction::Zoom => 1,
            },
            max_fps,
            hold_delay_ms: s.hold_delay_ms,
            recursive: s.recursive,
            scale_mode: match s.scale_mode {
                settings::ScaleModePref::Fit => 0,
                settings::ScaleModePref::Fill => 1,
                settings::ScaleModePref::Original => 2,
            },
            appearance: match s.appearance_mode {
                settings::AppearanceMode::System => 0,
                settings::AppearanceMode::Light => 1,
                settings::AppearanceMode::Dark => 2,
            },
            accent_source: match s.accent_source {
                settings::AccentSource::System => 0,
                settings::AccentSource::Custom => 1,
                settings::AccentSource::Brand => 2,
            },
            accent_custom: s.accent_custom,
            info_line_align: match s.info_line_align {
                settings::InfoLineAlign::Left => 0,
                settings::InfoLineAlign::Center => 1,
                settings::InfoLineAlign::Right => 2,
            },
            show_toolbar: s.show_toolbar,
            show_image_info: s.show_image_info,
            info_show_folder: s.info_show_folder,
            info_show_filename: s.info_show_filename,
            info_show_resolution: s.info_show_resolution,
            info_show_codec: s.info_show_codec,
            letterbox: [
                s.letterbox[0] as f32 / 255.0,
                s.letterbox[1] as f32 / 255.0,
                s.letterbox[2] as f32 / 255.0,
            ],
            letterbox_light: [
                s.letterbox_light[0] as f32 / 255.0,
                s.letterbox_light[1] as f32 / 255.0,
                s.letterbox_light[2] as f32 / 255.0,
            ],
            info_opacity: s.info_opacity,
            startup_mode: match s.startup_mode {
                settings::StartupMode::Fullscreen => 0,
                settings::StartupMode::Windowed => 1,
                settings::StartupMode::Remember => 2,
            },
            slideshow_interval: s.slideshow_interval_secs,
            picker_fixed: s.picker_dir.is_some(),
            picker_dir: s.picker_dir.clone(),
            describe_backend: match s.describe_backend {
                settings::DescribeBackend::Auto => 0,
                settings::DescribeBackend::AppleOnDevice => 1,
                settings::DescribeBackend::LocalEndpoint => 2,
            },
            describe_endpoint: s.describe_endpoint.clone(),
            describe_model: s.describe_model.clone(),
            describe_prompt: s.describe_prompt.clone().unwrap_or_default(),
            describe_length: len_preset_index(s.describe_max_tokens),
            describe_auto: s.describe_auto,
            speak_descriptions: s.speak_descriptions,
        }
    }

    /// Fold the edited draft back onto `base`, preserving fields the form doesn't
    /// expose (notably the remembered last fullscreen state). Clamped to valid ranges.
    fn to_settings(&self, base: &settings::Settings) -> settings::Settings {
        let mut s = base.clone();
        s.start_speed = self.start_speed;
        s.ramp_secs = self.ramp_secs;
        // The slider tops out at the refresh rate; that ceiling means "uncapped" (0).
        s.max_advance_rate = if self.max_fps >= self.refresh_hz {
            0
        } else {
            self.max_fps
        };
        s.hold_delay_ms = self.hold_delay_ms;
        s.scroll_action = match self.scroll_action {
            1 => settings::ScrollAction::Zoom,
            _ => settings::ScrollAction::Pan,
        };
        s.recursive = self.recursive;
        s.scale_mode = match self.scale_mode {
            1 => settings::ScaleModePref::Fill,
            2 => settings::ScaleModePref::Original,
            _ => settings::ScaleModePref::Fit,
        };
        s.appearance_mode = match self.appearance {
            1 => settings::AppearanceMode::Light,
            2 => settings::AppearanceMode::Dark,
            _ => settings::AppearanceMode::System,
        };
        s.accent_source = match self.accent_source {
            1 => settings::AccentSource::Custom,
            2 => settings::AccentSource::Brand,
            _ => settings::AccentSource::System,
        };
        s.accent_custom = self.accent_custom;
        s.info_line_align = match self.info_line_align {
            0 => settings::InfoLineAlign::Left,
            1 => settings::InfoLineAlign::Center,
            _ => settings::InfoLineAlign::Right,
        };
        s.show_toolbar = self.show_toolbar;
        s.show_image_info = self.show_image_info;
        s.info_show_folder = self.info_show_folder;
        s.info_show_filename = self.info_show_filename;
        s.info_show_resolution = self.info_show_resolution;
        s.info_show_codec = self.info_show_codec;
        s.letterbox = [
            (self.letterbox[0] * 255.0).round().clamp(0.0, 255.0) as u8,
            (self.letterbox[1] * 255.0).round().clamp(0.0, 255.0) as u8,
            (self.letterbox[2] * 255.0).round().clamp(0.0, 255.0) as u8,
        ];
        s.letterbox_light = [
            (self.letterbox_light[0] * 255.0).round().clamp(0.0, 255.0) as u8,
            (self.letterbox_light[1] * 255.0).round().clamp(0.0, 255.0) as u8,
            (self.letterbox_light[2] * 255.0).round().clamp(0.0, 255.0) as u8,
        ];
        s.info_opacity = self.info_opacity;
        s.startup_mode = match self.startup_mode {
            0 => settings::StartupMode::Fullscreen,
            1 => settings::StartupMode::Windowed,
            _ => settings::StartupMode::Remember,
        };
        s.slideshow_interval_secs = self.slideshow_interval;
        // Pin a folder only when "a specific folder" is selected *and* one was chosen;
        // otherwise fall back to the current-photo's-folder default.
        s.picker_dir = if self.picker_fixed {
            self.picker_dir.clone()
        } else {
            None
        };
        s.describe_backend = match self.describe_backend {
            1 => settings::DescribeBackend::AppleOnDevice,
            2 => settings::DescribeBackend::LocalEndpoint,
            _ => settings::DescribeBackend::Auto,
        };
        s.describe_endpoint = self.describe_endpoint.trim().to_string();
        s.describe_model = self.describe_model.trim().to_string();
        // Empty custom prompt → None (use the built-in instruction).
        let prompt = self.describe_prompt.trim();
        s.describe_prompt = (!prompt.is_empty()).then(|| prompt.to_string());
        s.describe_max_tokens = LEN_PRESETS[self.describe_length.min(LEN_PRESETS.len() - 1)];
        s.describe_auto = self.describe_auto;
        s.speak_descriptions = self.speak_descriptions;
        s.clamp();
        s
    }
}

/// The keybinding editor's mutable state, lent to [`settings_ui`] so the inline
/// editor can read/rebind the draft keymap. The actual key *capture* happens in
/// [`DialogWindow::handle_capture_event`] (raw winit events), not in egui — egui only
/// arms a slot and renders the result.
struct KbEdit<'a> {
    /// The draft keymap being edited (a clone of the live one; auto-saved on change).
    keymap: &'a mut Keymap,
    /// The slot awaiting a keypress (`Some((action, slot))`), or `None` when idle.
    capturing: &'a mut Option<(Action, usize)>,
    /// Whether a binding changed this frame (so `render` emits a live keymap edit).
    dirty: &'a mut bool,
    /// A transient note shown atop the section, e.g. "Moved Ctrl+C from Copy".
    note: &'a mut Option<String>,
}

/// The keyboard-shortcut editor's command list — shared with the SwiftUI host's
/// Shortcuts pane so the two editors can't drift (moved to `pb_app_core::keymap`
/// for NS2.6). Every listed command is rebindable, including ones with no default
/// key (their slots read a dimmed "Set"/"Add" placeholder).
const KB_GROUPS: &[(&str, &[Action])] = pb_app_core::keymap::EDITOR_GROUPS;

/// Is this physical key a bare modifier? (Capture waits for a "real" key to combine
/// with the held modifiers, rather than committing on the modifier press itself.)
fn is_modifier_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::ControlLeft
            | KeyCode::ControlRight
            | KeyCode::ShiftLeft
            | KeyCode::ShiftRight
            | KeyCode::AltLeft
            | KeyCode::AltRight
            | KeyCode::SuperLeft
            | KeyCode::SuperRight
    )
}

/// A second OS window rendering an egui dialog (its own wgpu device/surface).
pub struct DialogWindow {
    window: Arc<Window>,
    kind: DialogKind,
    // wgpu (kept alive together; `_instance` must outlive the surface).
    _instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    // egui
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    /// Whether this dialog renders in dark mode (resolved once from the OS theme at
    /// open). Reapplied to the design-system style every frame via `pbui::apply_style`.
    dark_ui: bool,
    /// The About card's app icon (a PNG). Status icons (lock/warning/trash) are drawn
    /// on demand via `pb_ui::icon`, not stored here.
    icon: Option<egui::TextureHandle>,
    draft: SettingsDraft,
    /// The live settings the draft folds onto — seeded at open, then advanced every
    /// time a live edit is applied, so the next fold diffs against the applied state
    /// (an idle frame or a load-time echo is a no-op) and unexposed fields survive.
    settings_base: settings::Settings,
    /// A live settings edit produced during a render frame (the folded draft differed
    /// from [`settings_base`]): the auto-saving idiom. Taken by the shell right after
    /// [`render`] and routed as [`contract::DialogResult::SettingsEdited`], which applies
    /// + persists without closing the window — parity with the macOS shell.
    ///
    /// [`settings_base`]: DialogWindow::settings_base
    /// [`render`]: DialogWindow::render
    pending_settings_edit: Option<settings::Settings>,
    /// The draft keymap edited by the inline keybinding editor (a clone of the live
    /// one, seeded at open). Auto-saved: a change is applied + persisted live.
    keymap_draft: Keymap,
    /// Set when a binding changed this frame; `render` drains it into a live keymap
    /// edit (then clears it) so only a real change persists.
    keymap_dirty: bool,
    /// The slot awaiting a keypress for rebinding (`Some((action, slot))`), else idle.
    capturing: Option<(Action, usize)>,
    /// Live modifier state for key capture, tracked from `ModifiersChanged` so a
    /// captured chord matches what the viewer would build.
    cap_ctrl: bool,
    cap_shift: bool,
    cap_alt: bool,
    cap_logo: bool,
    /// A transient note for the keybinding editor (e.g. a "moved from …" message).
    keymap_note: Option<String>,
    /// Which Settings tab (General / Appearance / AI / Shortcuts) is showing.
    settings_tab: SettingsTab,
    /// The AI tab's Test-connection state (probe runs off the UI thread).
    conn_test: ConnTest,
    /// Models the last probe listed (vision-capable first) — fills the Model picker.
    describe_models: Vec<String>,
    /// A live keymap edit produced during a render frame (a binding actually changed):
    /// rides the same auto-save channel as [`pending_settings_edit`], applied + persisted
    /// without closing the window.
    ///
    /// [`pending_settings_edit`]: DialogWindow::pending_settings_edit
    pending_keymap_edit: Option<Keymap>,
    /// The prompt for a [`DialogKind::Confirm`]/[`Message`]/[`Password`] dialog.
    ///
    /// [`Message`]: DialogKind::Message
    /// [`Password`]: DialogKind::Password
    confirm_msg: String,
    /// Set when the user answers a Confirm dialog: `Some(true)` = confirmed,
    /// `Some(false)` = cancelled. Polled + cleared by [`take_confirm_result`].
    ///
    /// [`take_confirm_result`]: DialogWindow::take_confirm_result
    confirm_result: Option<bool>,
    /// The masked password field's text (a [`DialogKind::Password`] dialog).
    password_input: String,
    /// An inline error shown under the password field after a wrong attempt
    /// (e.g. "Incorrect password"); `None` on first prompt.
    password_error: Option<String>,
    /// While a submitted password is being validated (the async 7z re-open): the
    /// field + Unlock are disabled and a "Checking…" spinner shows, so a slow
    /// archive doesn't look frozen and the user can't double-submit.
    checking: bool,
    /// The text the user just submitted (Unlock / Enter on a Password dialog), taken
    /// by [`take_submitted_password`] right after the answering frame.
    ///
    /// [`take_submitted_password`]: DialogWindow::take_submitted_password
    submitted_password: Option<String>,
    /// The "Ask about image" question field's live contents (a [`DialogKind::AskImage`]
    /// dialog); cleared on close so each Ask starts blank.
    ask_input: String,
    /// The question the user just submitted (Ask / ⌘Enter), taken by [`take_ask_result`]
    /// during the answering frame. `None` until then.
    ///
    /// [`take_ask_result`]: DialogWindow::take_ask_result
    submitted_ask: Option<String>,
    /// One-shot: request keyboard focus for the password field on the next render
    /// (set on open and after a wrong attempt). Done once per request rather than
    /// every frame — re-grabbing focus each frame suppresses the field's Enter-driven
    /// `lost_focus`, which is how submit is detected.
    focus_password: bool,
    /// When egui next wants to be repainted (animations, a combo popup opening, the
    /// "Checking…" spinner). egui is immediate-mode, so a frame only happens when
    /// something asks for it; without honoring this the dialog would freeze between
    /// OS events (a clicked dropdown wouldn't open until you moved the mouse). A
    /// zero-delay request is re-armed immediately in `render`; a timed one is woken
    /// by the main loop via [`repaint_at`](DialogWindow::repaint_at).
    next_repaint: Option<Instant>,
    /// The shared progress + cancel handle for a [`DialogKind::Loading`] view (the eager
    /// 7z decode runs on a worker thread; this dialog reads its `fraction`/bytes to draw
    /// the bar and calls `request_cancel` from the Cancel button / Esc). `None` otherwise.
    progress: Option<OpenProgress>,
    /// The shared progress + cancel handle for a [`DialogKind::Scanning`] view (the folder
    /// walk runs on a worker thread; this dialog reads its image count + current folder to
    /// draw the indeterminate progress and calls `request_cancel` from Cancel / Esc).
    /// `None` otherwise.
    scan_progress: Option<ScanProgress>,
}

impl DialogWindow {
    /// Create and show the dialog window, centered over `parent` (the main viewer
    /// window) when given. `refresh_hz` caps the Settings "max photos/sec" slider;
    /// `settings` + `keymap` seed the Settings form + keybinding editor (ignored by the
    /// other kinds). Returns `None` if window/GPU setup fails (best-effort — a failed
    /// dialog must never take down the viewer).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        kind: DialogKind,
        event_loop: &ActiveEventLoop,
        refresh_hz: u32,
        message: &str,
        settings: &settings::Settings,
        keymap: &Keymap,
        parent: Option<&Window>,
    ) -> Option<DialogWindow> {
        let (w, h, resizable, title) = match kind {
            // Windows (Segoe UI): the original tuned height.
            #[cfg(target_os = "windows")]
            DialogKind::About => (254.0, 321.0, false, "About PhotoBlaze"),
            // Linux uses the bundled fonts, whose line metrics run taller still — the 321px
            // Windows height clips the bottom rows, so give it more room.
            #[cfg(all(unix, not(target_os = "macos")))]
            DialogKind::About => (254.0, 383.0, false, "About PhotoBlaze"),
            DialogKind::Settings => (560.0, 660.0, true, "PhotoBlaze Settings"),
            DialogKind::Confirm => (450.0, 172.0, false, "Confirm Delete"),
            DialogKind::Message => (470.0, 185.0, false, "PhotoBlaze"),
            DialogKind::Password => (500.0, 250.0, false, "Password Required"),
            DialogKind::AskImage => (500.0, 320.0, false, "Ask About Image"),
            DialogKind::Loading => (500.0, 210.0, false, "Opening Archive"),
            // A touch taller than Loading for the extra current-folder line under the count.
            DialogKind::Scanning => (500.0, 220.0, false, "Scanning Folder"),
        };
        // Created HIDDEN: we render the first (themed) frame before revealing, so the
        // OS never flashes the default white window before our dark frame lands.
        let mut attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(w, h))
            .with_resizable(resizable)
            // Use the PhotoBlaze icon, not the generic default (the Win32 call below
            // then upgrades it to the .exe's crisp multi-size .ico on Windows).
            .with_window_icon(crate::load_window_icon())
            .with_visible(false);
        // Center over the parent's outer rect (so it lands on the viewer, not the OS
        // cascade position). Falls back to the default position if it can't be read.
        if let Some(p) = parent {
            if let Ok(ppos) = p.outer_position() {
                let psize = p.outer_size();
                let scale = p.scale_factor();
                let (dw, dh) = (w * scale, h * scale);
                let mut x = ppos.x as f64 + (psize.width as f64 - dw) / 2.0;
                let mut y = ppos.y as f64 + (psize.height as f64 - dh) / 2.0;
                // Keep the dialog fully on-screen: centering over a parent that sits at
                // a screen corner would otherwise push it half off the monitor (#4).
                if let Some(mon) = p.current_monitor() {
                    let mp = mon.position();
                    let ms = mon.size();
                    let (cx, cy) = clamp_to_monitor(
                        (x, y),
                        (dw, dh),
                        (mp.x as f64, mp.y as f64, ms.width as f64, ms.height as f64),
                    );
                    x = cx;
                    y = cy;
                }
                attrs = attrs.with_position(PhysicalPosition::new(x, y));
            }
        }
        let window = Arc::new(event_loop.create_window(attrs).ok()?);
        // Match the viewer: point the title-bar / taskbar icon at the exe's multi-size
        // .ico so the small size is its purpose-rendered bitmap, not a crude downscale.
        #[cfg(windows)]
        crate::apply_native_window_icon(&window);
        let size = window.inner_size();
        // The dialog theme honors the Appearance preference (#46): System follows the
        // OS-resolved window theme (the pre-#46 behavior); Light/Dark pin it — and the
        // pin is pushed onto the window itself so the OS-drawn title bar matches the
        // egui body instead of keeping the desktop's scheme.
        window.set_theme(match settings.appearance_mode {
            settings::AppearanceMode::Light => Some(Theme::Light),
            settings::AppearanceMode::Dark => Some(Theme::Dark),
            settings::AppearanceMode::System => None,
        });
        let dark_ui = match settings.appearance_mode {
            settings::AppearanceMode::Light => false,
            settings::AppearanceMode::Dark => true,
            settings::AppearanceMode::System => window.theme() != Some(Theme::Light),
        };

        // Match the viewer's GPU setup (pb-render): restrict to `Backends::PRIMARY`
        // (DX12 on Windows / Metal / Vulkan) so we never select the GL *secondary*
        // backend. On a VM the low-power adapter is often a GL compatibility device
        // (e.g. Parallels' "Apple M2 Max (Compat)") whose `request_device` then fails
        // `LimitsExceeded` (it reports `max_compute_workgroups_per_dimension = 0`),
        // which killed the dialog entirely — no window, and the transient GL context
        // corrupted the main DX12 swapchain into a stretched frame. PRIMARY keeps the
        // dialog on the same backend as the viewer.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let surface = instance.create_surface(window.clone()).ok()?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))?;
        // Request exactly the adapter's own limits — egui needs nothing beyond the
        // defaults, so this can never exceed what the device supports (unlike
        // `DeviceDescriptor::default()`, which demands the full default limit set).
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                required_limits: adapter.limits(),
                ..Default::default()
            },
            None,
        ))
        .ok()?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let egui_ctx = egui::Context::default();
        // Lock egui to the OS-resolved theme (explicit, not `System`) so our custom
        // palette in `pbui::apply_style` isn't re-clobbered by egui's own per-frame
        // light/dark resolution; then install the native UI font + design-system style.
        egui_ctx.set_theme(if dark_ui {
            egui::ThemePreference::Dark
        } else {
            egui::ThemePreference::Light
        });
        pbui::install_fonts(&egui_ctx);
        pbui::apply_style(&egui_ctx, dark_ui);
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(&device, format, None, 1, false);
        let icon = load_icon_texture(&egui_ctx);

        // Seed the fold baseline from the *normalized* round-trip, not the raw settings:
        // `to_settings` trims/clamps, so a non-normalized stored value would otherwise make
        // the first frame's fold differ and spuriously auto-save on mere open. Normalizing
        // the baseline makes "open Settings, change nothing, close" a guaranteed zero-write.
        let draft = SettingsDraft::from_settings(settings, refresh_hz);
        let settings_base = draft.to_settings(settings);

        let mut dlg = DialogWindow {
            window,
            kind,
            _instance: instance,
            surface,
            device,
            queue,
            config,
            egui_ctx,
            egui_state,
            egui_renderer,
            dark_ui,
            icon,
            draft,
            settings_base,
            pending_settings_edit: None,
            keymap_draft: keymap.clone(),
            keymap_dirty: false,
            capturing: None,
            cap_ctrl: false,
            cap_shift: false,
            cap_alt: false,
            cap_logo: false,
            keymap_note: None,
            settings_tab: SettingsTab::default(),
            conn_test: ConnTest::default(),
            describe_models: Vec::new(),
            pending_keymap_edit: None,
            confirm_msg: message.to_string(),
            confirm_result: None,
            password_input: String::new(),
            ask_input: String::new(),
            submitted_ask: None,
            password_error: None,
            checking: false,
            submitted_password: None,
            // Focus the text field on open for the two text-entry dialogs.
            focus_password: matches!(kind, DialogKind::Password | DialogKind::AskImage),
            next_repaint: None,
            progress: None,
            scan_progress: None,
        };
        // Prime two hidden frames: the first lets egui apply its base theme, the second
        // paints with our design-system style layered on top — so the window is already
        // correct when revealed (no default-theme flash).
        dlg.render();
        dlg.render();
        dlg.window.set_visible(true);
        // Grab keyboard focus so Esc / Enter act on the dialog (not the viewer, which
        // would otherwise treat Esc as quit).
        dlg.window.focus_window();
        dlg.window.request_redraw();
        Some(dlg)
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn kind(&self) -> DialogKind {
        self.kind
    }

    /// Take the answer to a [`DialogKind::Confirm`] dialog, if the user has answered
    /// this frame: `Some(true)` = confirmed, `Some(false)` = cancelled. `None` until
    /// a button is clicked. The caller closes the dialog and acts on the result.
    pub fn take_confirm_result(&mut self) -> Option<bool> {
        self.confirm_result.take()
    }

    /// Take the password the user just submitted on a [`DialogKind::Password`]
    /// dialog (Unlock / Enter), set during the answering frame. `None` until then.
    /// The caller pairs this with a `take_confirm_result()` of `Some(true)`.
    pub fn take_submitted_password(&mut self) -> Option<String> {
        self.submitted_password.take()
    }

    /// Take the question the user just submitted on a [`DialogKind::AskImage`] dialog
    /// (Ask / ⌘Enter), set during the answering frame. `None` until then. The caller pairs
    /// this with a `take_confirm_result()` of `Some(true)`.
    pub fn take_ask_result(&mut self) -> Option<String> {
        self.submitted_ask.take()
    }

    /// Take a live Settings edit produced this render frame (auto-save): the settings
    /// (when the form changed) and/or the keymap (when a binding changed). `None` when
    /// the frame changed nothing. Polled by the shell after every [`render`] and routed
    /// as [`contract::DialogResult::SettingsEdited`] — apply + persist, window stays open.
    ///
    /// [`render`]: DialogWindow::render
    #[allow(clippy::type_complexity)]
    pub fn take_settings_edit(
        &mut self,
    ) -> Option<(Option<Box<settings::Settings>>, Option<Keymap>)> {
        let s = self.pending_settings_edit.take();
        let k = self.pending_keymap_edit.take();
        (s.is_some() || k.is_some()).then(|| (s.map(Box::new), k))
    }

    /// Whether the keybinding editor is waiting for a keypress to bind. While true the
    /// event router feeds key events to [`handle_capture_event`] instead of egui.
    ///
    /// [`handle_capture_event`]: DialogWindow::handle_capture_event
    pub fn capturing_active(&self) -> bool {
        self.capturing.is_some()
    }

    /// Keep the capture modifier state fresh from `ModifiersChanged` (always — even
    /// when not capturing — so an armed slot sees the true modifier state). A no-op
    /// for every other event.
    pub fn note_modifiers(&mut self, event: &WindowEvent) {
        if let WindowEvent::ModifiersChanged(mods) = event {
            self.cap_ctrl = mods.state().control_key();
            self.cap_shift = mods.state().shift_key();
            self.cap_alt = mods.state().alt_key();
            self.cap_logo = mods.state().super_key();
        }
    }

    /// Consume a key event while the keybinding editor is capturing: a non-modifier
    /// key binds the armed slot (stealing the chord from any prior owner), Esc cancels,
    /// a bare modifier or a key release is swallowed (wait for the real key). Returns
    /// whether the event was consumed (so the router skips egui / the Esc-closes path);
    /// non-key events return `false` and fall through to normal handling.
    pub fn handle_capture_event(&mut self, event: &WindowEvent) -> bool {
        let Some((action, slot)) = self.capturing else {
            return false;
        };
        match event {
            WindowEvent::ModifiersChanged(_) => true, // tracked in `note_modifiers`
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                let code = *code;
                if code == KeyCode::Escape {
                    self.capturing = None; // cancel, leave the binding unchanged
                    return true;
                }
                if is_modifier_key(code) {
                    return true; // wait for a real key to combine with held modifiers
                }
                // Map the winit key into the shell-neutral `PbKey` the keymap stores.
                // A physically-unnameable key (e.g. F13) can't form a persistable
                // binding, so stay armed and wait for one the keymap can express.
                let Some(key) = crate::pb_key_winit::from_winit(code) else {
                    return true;
                };
                let chord = KeyChord::new(
                    key,
                    self.cap_ctrl,
                    self.cap_shift,
                    self.cap_alt,
                    self.cap_logo,
                );
                let stolen = self.keymap_draft.set_slot(action, slot, chord);
                self.keymap_dirty = true;
                self.capturing = None;
                self.keymap_note =
                    stolen.map(|a| format!("Moved {chord} from \u{201c}{}\u{201d}", a.label()));
                true
            }
            // Swallow key releases while armed so they don't leak to egui.
            WindowEvent::KeyboardInput { .. } => true,
            _ => false,
        }
    }

    /// Show an inline error under the password field (a wrong attempt), clear the
    /// field, and leave the dialog open + interactive for another try.
    pub fn set_password_error(&mut self, msg: impl Into<String>) {
        // Scrub the rejected attempt rather than just dropping the bytes.
        scrub(&mut self.password_input);
        self.password_error = Some(msg.into());
        self.checking = false;
        self.focus_password = true; // re-focus the cleared field for the next try
    }

    /// Toggle the "Checking…" state while a submitted password is validated (the
    /// field + Unlock are disabled so the slow 7z re-open can't be double-submitted).
    pub fn set_checking(&mut self, on: bool) {
        self.checking = on;
    }

    /// Attach (or clear) the [`OpenProgress`] handle a [`DialogKind::Loading`] view reads
    /// to draw its bar and cancel from. A cheap `Arc` clone shared with the load worker.
    pub fn set_progress(&mut self, progress: Option<OpenProgress>) {
        self.progress = progress;
    }

    /// Turn an open dialog into the loading view **in place** — same OS window and wgpu
    /// surface, no swap/flicker. Used when a verified password promotes the `Password`
    /// dialog to the decode-progress view: it switches the kind, retitles the window,
    /// scrubs the now-finished password field, and attaches the progress handle.
    pub fn become_loading(&mut self, message: &str, progress: OpenProgress) {
        self.kind = DialogKind::Loading;
        self.confirm_msg = message.to_string();
        self.progress = Some(progress);
        self.checking = false;
        scrub(&mut self.password_input);
        self.window.set_title("Opening Archive");
        self.request_redraw();
    }

    /// Point a [`DialogKind::Scanning`] view at a folder scan: its message + the shared
    /// [`ScanProgress`] the walk worker updates. Used on the deferred first reveal and to
    /// re-point an already-open scanning dialog at a newer scan in place (a second folder
    /// opened while the first was still walking), so it tracks the new folder instead of a
    /// frozen old count rather than flickering a fresh window.
    pub(crate) fn set_scan(&mut self, message: &str, progress: ScanProgress) {
        self.kind = DialogKind::Scanning;
        self.confirm_msg = message.to_string();
        self.scan_progress = Some(progress);
        self.window.set_title("Scanning Folder");
        self.request_redraw();
    }

    pub fn focus(&self) {
        self.window.focus_window();
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    /// Feed a winit event to egui; returns whether egui wants a repaint.
    pub fn on_event(&mut self, event: &WindowEvent) -> bool {
        self.egui_state.on_window_event(&self.window, event).repaint
    }

    pub fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Run egui for one frame and present it.
    pub fn render(&mut self) {
        // Reassert the design-system style each frame (cheap; off the photo hot path)
        // so it survives egui's own theme bookkeeping.
        pbui::apply_style(&self.egui_ctx, self.dark_ui);
        let raw_input = self.egui_state.take_egui_input(&self.window);
        let ctx = self.egui_ctx.clone();
        let kind = self.kind;
        let icon = self.icon.clone();
        let msg = self.confirm_msg.clone();
        let pw_error = self.password_error.clone();
        let checking = self.checking;
        let take_focus = self.focus_password;
        let progress = self.progress.clone();
        let scan_progress = self.scan_progress.clone();
        let draft = &mut self.draft;
        let password_input = &mut self.password_input;
        let ask_input = &mut self.ask_input;
        let mut kb = KbEdit {
            keymap: &mut self.keymap_draft,
            capturing: &mut self.capturing,
            dirty: &mut self.keymap_dirty,
            note: &mut self.keymap_note,
        };
        let settings_tab = &mut self.settings_tab;
        let conn_test = &mut self.conn_test;
        let describe_models = &mut self.describe_models;
        let mut confirm_click: Option<bool> = None;
        let full_output = ctx.run(raw_input, |ctx| match kind {
            DialogKind::About => {
                egui::CentralPanel::default().show(ctx, |ui| about_ui(ui, icon.as_ref()));
            }
            DialogKind::Settings => {
                // Pinned action bar at the bottom, then the scrolling settings page.
                // The lone Done button answers the dialog → the main loop closes it
                // (edits already auto-saved live; see the post-frame fold below).
                confirm_click = settings_button_bar(ctx);
                egui::CentralPanel::default()
                    .frame(egui::Frame::default().fill(ctx.style().visuals.panel_fill))
                    .show(ctx, |ui| {
                        settings_ui(ui, draft, &mut kb, settings_tab, conn_test, describe_models)
                    });
            }
            DialogKind::Confirm => {
                confirm_click = confirm_dialog(ctx, &msg);
            }
            DialogKind::Message => {
                confirm_click = message_dialog(ctx, &msg);
            }
            DialogKind::Password => {
                confirm_click = password_dialog(
                    ctx,
                    &msg,
                    password_input,
                    pw_error.as_deref(),
                    checking,
                    take_focus,
                );
            }
            DialogKind::AskImage => {
                confirm_click = ask_dialog(ctx, ask_input, take_focus);
            }
            DialogKind::Loading => {
                confirm_click = loading_dialog(ctx, &msg, progress.as_ref());
            }
            DialogKind::Scanning => {
                confirm_click = scanning_dialog(ctx, &msg, scan_progress.as_ref());
            }
        });
        // How soon egui wants the next frame: 0 = "again now" (a popup opening, the
        // Checking… spinner, an in-progress animation), a finite delay = a timed
        // refresh (a blinking text cursor), MAX = idle. Honored below so the dialog
        // keeps animating without an OS event nudging it.
        let repaint_delay = full_output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .unwrap_or(Duration::MAX);
        // The focus request (if any) was issued this frame; don't repeat it.
        self.focus_password = false;
        // Settings auto-save: the form edits `draft`/`keymap_draft` live (no Save button).
        // After the frame ran, fold the draft onto the live baseline; a real diff is a live
        // edit the shell routes as `SettingsEdited` (apply + persist, window stays open).
        // An idle frame or the open-time load echo folds equal → no-op, so disk is untouched.
        if kind == DialogKind::Settings {
            let folded = self.draft.to_settings(&self.settings_base);
            if folded != self.settings_base {
                self.settings_base = folded.clone();
                self.pending_settings_edit = Some(folded);
            }
            if self.keymap_dirty {
                self.keymap_dirty = false;
                self.pending_keymap_edit = Some(self.keymap_draft.clone());
            }
        }
        if confirm_click.is_some() {
            self.confirm_result = confirm_click;
            // On Unlock/Enter, snapshot the entered text for the app to validate.
            if confirm_click == Some(true) && kind == DialogKind::Password {
                self.submitted_password = Some(self.password_input.clone());
            }
            // On Ask, snapshot the typed question for the core to run through describe.
            if confirm_click == Some(true) && kind == DialogKind::AskImage {
                self.submitted_ask = Some(self.ask_input.clone());
            }
            // Settings' "Done" (and Esc/close) just closes — every edit was already applied
            // + persisted live above, so there is nothing to commit on the way out.
        }

        self.egui_state
            .handle_platform_output(&self.window, full_output.platform_output);
        let paint_jobs = self
            .egui_ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);
        let desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point: full_output.pixels_per_point,
        };
        for (id, delta) in &full_output.textures_delta.set {
            self.egui_renderer
                .update_texture(&self.device, &self.queue, *id, delta);
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("egui"),
            });
        let cmd_bufs = self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &paint_jobs,
            &desc,
        );

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut rpass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui-dialog"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // egui's CentralPanel fills the window, so this is just a
                            // backstop clear.
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .forget_lifetime();
            self.egui_renderer.render(&mut rpass, &paint_jobs, &desc);
        }
        self.queue.submit(
            cmd_bufs
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        frame.present();
        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }

        // Honor egui's repaint request so the dialog animates without an OS event:
        // a zero delay re-arms a redraw immediately (popups, the spinner); a finite
        // delay is parked on `next_repaint` for the main loop to wake on; MAX = idle.
        if repaint_delay.is_zero() {
            self.next_repaint = None;
            self.window.request_redraw();
        } else if repaint_delay < Duration::MAX {
            self.next_repaint = Some(Instant::now() + repaint_delay);
        } else {
            self.next_repaint = None;
        }
    }

    /// When the open dialog next wants to be repainted for a *timed* egui refresh
    /// (e.g. a blinking cursor), so the main loop can schedule a wake. `None` when
    /// idle or when an immediate redraw was already re-armed in [`render`](Self::render).
    pub fn repaint_at(&self) -> Option<Instant> {
        self.next_repaint
    }
}

impl Drop for DialogWindow {
    fn drop(&mut self) {
        // Scrub any entered password from RAM on close (privacy guarantee), covering
        // every teardown path — Cancel, Esc, the window close button, or replacement
        // by another dialog.
        scrub(&mut self.password_input);
        if let Some(p) = self.submitted_password.as_mut() {
            scrub(p);
        }
    }
}

/// Decode the embedded app icon into an egui texture (for the About card).
fn load_icon_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    const PNG: &[u8] = include_bytes!("../icons/photoblaze.png");
    let fit = FitBox {
        max_width: 256,
        max_height: 256,
    };
    let img = decode_bytes(PNG, Some(fit), false).ok()?;
    let color = egui::ColorImage::from_rgba_unmultiplied(
        [img.width as usize, img.height as usize],
        &img.pixels,
    );
    Some(ctx.load_texture("about-icon", color, egui::TextureOptions::LINEAR))
}

/// A friendly name for the CPU architecture this binary was built for, using each
/// platform's conventional label (`ARM64`/`x64`) rather than Rust's target-triple spelling
/// (`aarch64`/`x86_64`). `std::env::consts::ARCH` is a compile-time constant for the *build*
/// target, so this reflects the actual binary — not the host it happens to run on.
fn arch_label() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "ARM64",
        "x86_64" => "x64",
        "x86" => "x86",
        other => other,
    }
}

/// The About card: big centered icon, name, version, tagline, copyright, link.
fn about_ui(ui: &mut egui::Ui, icon: Option<&egui::TextureHandle>) {
    ui.vertical_centered(|ui| {
        ui.add_space(20.0);
        if let Some(tex) = icon {
            ui.image(egui::load::SizedTexture::new(
                tex.id(),
                egui::vec2(100.0, 100.0),
            ));
        }
        ui.add_space(12.0);
        ui.heading("PhotoBlaze");
        ui.add_space(2.0);
        ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
        // The build's git commit (set by build.rs) plus the CPU architecture this binary was
        // built for, so a local build can be traced to its exact commit *and* you can confirm at
        // a glance which arch it is — notably the native Windows ARM64 build vs x64. The commit is
        // absent for a build with no git available (source tarball); the arch always shows.
        ui.add_space(1.0);
        let arch = arch_label();
        let build_line = match option_env!("PB_BUILD_ID") {
            Some(build) => format!("Build {build} \u{00b7} {arch}"),
            None => arch.to_string(),
        };
        ui.label(egui::RichText::new(build_line).size(11.0).weak());
        ui.add_space(10.0);
        ui.label(pb_app_core::TAGLINE);
        ui.add_space(8.0);
        ui.label("\u{00a9} FullSpec Systems Inc. 2026");
        ui.add_space(12.0);
        ui.hyperlink_to(
            "github.com/jdlien/photoblaze",
            "https://github.com/jdlien/photoblaze",
        );
    });
}

// ── Dialog layout tokens ────────────────────────────────────────────────────
// Only the dialog *scaffold* (the second-window mechanism) lives here; the buttons,
// fields, and icons are **pb_ui components** (`pbui::{primary,secondary,danger}_button`,
// `pbui::text_field`, `pbui::icon::{lead_row,inline}`) so they match the rest of the app
// and there's nothing to re-maintain. Anatomy of a dialog: a `dialog_frame`-inset
// content panel on top, then a `button_bar` pinned to the bottom holding the pb_ui
// buttons right-aligned. Roles:
//   * DIALOG_PAD  — uniform content inset; also the gap to every edge + the divider.
//   * MSG_SIZE    — body/message text size.
// Button gaps use `pbui::SPACE_3`; text-field padding `pbui::FIELD_MARGIN`; status
// icons (lock/warning/trash) come tinted + placed from `pbui::icon`.
const DIALOG_PAD: f32 = 22.0;
const MSG_SIZE: f32 = 15.0;
/// Uniform inset of a dialog's bottom action bar — applied equally to the top (the
/// divider), right, and bottom, **and** used as the gap between buttons. This is the one
/// place dialog-button spacing is defined, so buttons always land balanced and no caller
/// hand-spaces them. Sourced from `pbui::GAP` so the button gap, button-bar inset, and
/// the gaps between cards are all the one standard value.
const BTN_BAR_PAD: f32 = pbui::GAP;

/// A panel frame filled to match the window background, inset by `DIALOG_PAD` on
/// all sides. Used for both the content panel and the bottom button bar so their
/// padding lines up.
fn dialog_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::default()
        .fill(ctx.style().visuals.panel_fill)
        .inner_margin(egui::Margin::same(DIALOG_PAD))
}

/// The shared bottom action bar — the single place dialog buttons get their spacing, so
/// they always land balanced. Buttons are laid out right-to-left (added rightmost-first,
/// so they read `[Primary] [Cancel]` left-to-right) with a uniform [`BTN_BAR_PAD`] inset
/// on the top (the divider), right, and bottom, **and** the same value as the gap between
/// buttons (set via `item_spacing.x`). Callers just add buttons — no manual spacing.
fn button_bar(ctx: &egui::Context, id: &'static str, add: impl FnOnce(&mut egui::Ui)) {
    egui::TopBottomPanel::bottom(id)
        .frame(
            egui::Frame::default()
                .fill(ctx.style().visuals.panel_fill)
                .inner_margin(egui::Margin::same(BTN_BAR_PAD)),
        )
        .show(ctx, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = BTN_BAR_PAD;
                add(ui);
            });
        });
}

/// A themed confirm dialog modelled on Directory Opus's "Confirm File Delete": the
/// prompt + a ⚠ "cannot be undone" line, and a bottom-right button bar with a
/// prominent red Delete (default-focused) and Cancel. Returns `Some(true)` on
/// Delete, `Some(false)` on Cancel, else `None`. (Esc / the window close button also
/// cancel it, from the event router.) The shared `DIALOG_PAD` margin balances the
/// spacing around the text and around the buttons (equal to the divider + edges).
fn confirm_dialog(ctx: &egui::Context, message: &str) -> Option<bool> {
    let mut result = None;
    let p = pbui::Palette::new(ctx.style().visuals.dark_mode);
    // Right-to-left: Cancel added first (rightmost), Delete to its left — so the
    // visual order reads [Delete] [Cancel], matching Directory Opus.
    button_bar(ctx, "confirm_bar", |ui| {
        if pbui::secondary_button(ui, "Cancel").clicked() {
            result = Some(false);
        }
        let resp = pbui::danger_button(ui, &p, "Delete");
        if resp.clicked() {
            result = Some(true);
        }
        // Default focus on Delete (matches Directory Opus): Enter confirms.
        if ui.memory(|m| m.focused().is_none()) {
            resp.request_focus();
        }
    });
    // A Trash lead icon (danger tone) + the prompt and an "undo" warning. The prompt
    // wraps so a long file name can't run off the right edge.
    egui::CentralPanel::default()
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            pbui::icon::lead_row(
                ui,
                &p,
                pbui::icon::Icon::Trash,
                pbui::icon::Tone::Danger,
                |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(message).size(16.0)).wrap());
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("This operation cannot be undone.")
                            .color(p.text_secondary),
                    );
                },
            );
        });
    result
}

/// A one-button informational / error notice (e.g. an archive-open failure): a lead
/// warning icon + the body text, and a bottom-right OK button (default-focused).
/// Returns `Some(true)` when OK is clicked; Esc / the close button dismiss it via the
/// event router. Shares `DIALOG_PAD` with the confirm dialog for matching margins.
fn message_dialog(ctx: &egui::Context, message: &str) -> Option<bool> {
    let mut ok = None;
    let p = pbui::Palette::new(ctx.style().visuals.dark_mode);
    button_bar(ctx, "message_bar", |ui| {
        let resp = pbui::primary_button(ui, &p, "OK");
        if resp.clicked() {
            ok = Some(true);
        }
        if ui.memory(|m| m.focused().is_none()) {
            resp.request_focus();
        }
    });
    egui::CentralPanel::default()
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            // A warning lead icon + the message (wraps under itself, the icon staying
            // aligned to the first line — handled by `lead_row`).
            pbui::icon::lead_row(
                ui,
                &p,
                pbui::icon::Icon::Warning,
                pbui::icon::Tone::Warning,
                |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(message).size(MSG_SIZE)).wrap());
                },
            );
        });
    ok
}

/// The archive password-entry dialog: a lead lock icon + prompt, a masked
/// single-line field (auto-focused), an optional red error line after a wrong
/// attempt, and an [Unlock] [Cancel] bottom bar. Enter submits, Esc cancels (via
/// the event router). Returns `Some(true)` on Unlock/Enter, `Some(false)` on Cancel.
/// The typed text stays in `input`; the caller reads it via `take_submitted_password`.
/// While `checking`, the field + Unlock are disabled and a spinner shows.
fn password_dialog(
    ctx: &egui::Context,
    prompt: &str,
    input: &mut String,
    error: Option<&str>,
    checking: bool,
    take_focus: bool,
) -> Option<bool> {
    let mut result = None;
    let p = pbui::Palette::new(ctx.style().visuals.dark_mode);
    button_bar(ctx, "password_bar", |ui| {
        if pbui::secondary_button(ui, "Cancel").clicked() {
            result = Some(false);
        }
        // Disabled while the entered password is being validated (the slow 7z re-open).
        let unlock = ui
            .add_enabled_ui(!checking, |ui| pbui::primary_button(ui, &p, "Unlock"))
            .inner;
        if unlock.clicked() {
            result = Some(true);
        }
    });
    egui::CentralPanel::default()
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            // Lock lead icon in a gutter; the prompt + field + status form the content
            // column to its right (the gutter + vertical centering handled by `lead_row`).
            pbui::icon::lead_row(
                ui,
                &p,
                pbui::icon::Icon::Lock,
                pbui::icon::Tone::Neutral,
                |ui| {
                    // Two-line prompt: "Enter the password for" / the quoted file name.
                    ui.label(egui::RichText::new(prompt).size(MSG_SIZE));
                    ui.add_space(16.0); // breathing room between the prompt and field
                    let field = pbui::text_field(input, "Password")
                        .password(true)
                        .desired_width(f32::INFINITY);
                    let resp = ui.add_enabled(!checking, field);
                    // Focus the field once when requested (dialog opened / after a
                    // wrong attempt) — not every frame, which would re-grab focus on
                    // the same frame Enter releases it and swallow the `lost_focus`
                    // submit signal below.
                    if take_focus && !checking {
                        resp.request_focus();
                    }
                    // egui's singleline surrenders focus on Enter = the submit signal.
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        result = Some(true);
                    }
                    if let Some(err) = error {
                        ui.add_space(12.0);
                        ui.colored_label(egui::Color32::from_rgb(220, 90, 90), err);
                    }
                    if checking {
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.add_space(6.0);
                            ui.label("Checking…");
                        });
                    }
                },
            );
        });
    result
}

/// The "Ask about image" dialog (task #44): a **multi-line** question field + an Ask/Cancel
/// bar. Plain Enter inserts a newline (it's a real textarea); ⌘/Ctrl+Enter or the Ask button
/// submits. The core runs the question through the describe backend for the current photo.
fn ask_dialog(ctx: &egui::Context, input: &mut String, take_focus: bool) -> Option<bool> {
    let mut result = None;
    let p = pbui::Palette::new(ctx.style().visuals.dark_mode);
    button_bar(ctx, "ask_bar", |ui| {
        if pbui::secondary_button(ui, "Cancel").clicked() {
            result = Some(false);
        }
        let ask = ui
            .add_enabled_ui(!input.trim().is_empty(), |ui| {
                pbui::primary_button(ui, &p, "Ask")
            })
            .inner;
        if ask.clicked() {
            result = Some(true);
        }
    });
    egui::CentralPanel::default()
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            ui.label(egui::RichText::new("Ask a question about this image:").size(MSG_SIZE));
            ui.add_space(12.0);
            let resp = ui.add(
                egui::TextEdit::multiline(input)
                    .hint_text("e.g. What products are visible? What year does this look like?")
                    .desired_rows(4)
                    .desired_width(f32::INFINITY),
            );
            if take_focus {
                resp.request_focus();
            }
            // ⌘/Ctrl+Enter submits; plain Enter adds a newline (multi-line field).
            let submit_chord = ui.input(|i| {
                i.key_pressed(egui::Key::Enter) && (i.modifiers.command || i.modifiers.ctrl)
            });
            if resp.has_focus() && submit_chord && !input.trim().is_empty() {
                result = Some(true);
            }
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("⌘Enter to ask")
                    .size(12.0)
                    .color(p.text_secondary),
            );
        });
    result
}

/// The archive-loading view: a message (the file being opened), a determinate progress
/// bar with a `NN%  X / Y` caption, and a single bottom-right **Cancel** button. Cancel
/// requests cancellation of the in-flight eager 7z decode and answers `Some(false)` so
/// the main loop tidies up; Esc does the same via the event router. The dialog repaints
/// itself each frame (`progress_row`) so the bar tracks the worker without an OS nudge.
fn loading_dialog(
    ctx: &egui::Context,
    message: &str,
    progress: Option<&OpenProgress>,
) -> Option<bool> {
    let mut result = None;
    let p = pbui::Palette::new(ctx.style().visuals.dark_mode);
    button_bar(ctx, "loading_bar", |ui| {
        if pbui::secondary_button(ui, "Cancel").clicked() {
            if let Some(pr) = progress {
                pr.request_cancel();
            }
            result = Some(false);
        }
    });
    egui::CentralPanel::default()
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            ui.label(egui::RichText::new(message).size(MSG_SIZE));
            ui.add_space(18.0);
            match progress {
                Some(pr) => progress_row(ui, &p, pr),
                // No handle yet (the first priming frames before the worker is attached):
                // a plain spinner stand-in.
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.add_space(6.0);
                        ui.label("Preparing\u{2026}");
                    });
                }
            }
        });
    result
}

/// The progress bar plus its `NN%  done / total` caption, requesting a repaint each
/// frame so the bar advances as the worker thread bumps the shared counter (egui is
/// immediate-mode — without this the bar would only move on an OS event).
fn progress_row(ui: &mut egui::Ui, p: &pbui::Palette, progress: &OpenProgress) {
    ui.ctx().request_repaint();
    let frac = progress.fraction();
    pbui::progress_bar(ui, p, frac);
    ui.add_space(8.0);
    let (done, total) = (progress.done(), progress.total());
    let caption = if total > 0 {
        format!(
            "{}%   {} / {}",
            (frac * 100.0).round() as u32,
            crate::archive::human_gb(done),
            crate::archive::human_gb(total)
        )
    } else {
        "Preparing\u{2026}".to_string()
    };
    ui.label(
        egui::RichText::new(caption)
            .size(12.5)
            .color(p.text_secondary),
    );
}

/// The folder-scanning view: the folder being opened, an **indeterminate** spinner with a
/// live "N images found" count and the subfolder currently being walked, and a single
/// bottom-right **Cancel** button. A directory walk has no knowable total, so — unlike
/// [`loading_dialog`] — there is no determinate bar; the live count + current folder are
/// the progress. Cancel requests cancellation of the in-flight walk and answers
/// `Some(false)` so the main loop tidies up; Esc does the same via the event router. The
/// dialog repaints each frame so the count + folder track the worker without an OS nudge.
fn scanning_dialog(
    ctx: &egui::Context,
    message: &str,
    progress: Option<&ScanProgress>,
) -> Option<bool> {
    let mut result = None;
    let p = pbui::Palette::new(ctx.style().visuals.dark_mode);
    button_bar(ctx, "scanning_bar", |ui| {
        if pbui::secondary_button(ui, "Cancel").clicked() {
            if let Some(pr) = progress {
                pr.request_cancel();
            }
            result = Some(false);
        }
    });
    egui::CentralPanel::default()
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            // Immediate-mode: without a repaint request the spinner + count would only move
            // on an OS event (matches `progress_row`).
            ui.ctx().request_repaint();
            ui.label(egui::RichText::new(message).size(MSG_SIZE));
            ui.add_space(16.0);
            let found = progress.map(ScanProgress::found).unwrap_or(0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.add_space(8.0);
                // Before the first match, the count would read "0 images" — say "Searching…"
                // instead so a deep-but-sparse tree doesn't look like it found nothing.
                let label = if found == 0 {
                    "Searching\u{2026}".to_string()
                } else {
                    let noun = if found == 1 { "image" } else { "images" };
                    format!("{} {noun} found", fmt_count(found))
                };
                ui.label(egui::RichText::new(label).size(14.0));
            });
            // The current subfolder (elided to its last few components), quiet under the count.
            let current = progress.map(ScanProgress::current).unwrap_or_default();
            if !current.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(elide_path(&current))
                        .size(12.5)
                        .color(p.text_secondary),
                );
            }
        });
    result
}

/// Group a non-negative integer with thousands separators ("1,234") for the scanning
/// count caption — `usize::to_string` has no grouping and a big recursive folder can
/// reach five or six digits.
fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let len = s.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Shorten a relative directory path to its last few components for the scanning view's
/// one-line "current folder" caption (`…/2024/Iceland`), so a deep path can't overflow.
/// Component-based (not pixel-width), so it never half-clips a name. Accepts either path
/// separator since the walk worker formats with the platform's.
fn elide_path(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    const KEEP: usize = 3;
    if parts.len() <= KEEP {
        parts.join("/")
    } else {
        format!("\u{2026}/{}", parts[parts.len() - KEEP..].join("/"))
    }
}

/// Open the OS "default apps" settings so the user can make PhotoBlaze the default
/// photo viewer. Windows doesn't let an app set itself as default programmatically
/// (the user must confirm in Settings), so we deep-link — to PhotoBlaze's own page
/// where the registration allows (see `default_app`). Best-effort, and a no-op on
/// other platforms for now.
fn open_default_apps() {
    crate::default_app::open_default_apps();
}

/// Best-effort scrub of a secret String's bytes in place (overwrite with NUL, which
/// stays valid UTF-8), then clear it — so an entered password isn't left lying in
/// the field's buffer. Matches the RAM-only / no-trace stance for secrets.
fn scrub(s: &mut String) {
    // SAFETY: writing NUL bytes keeps the buffer valid UTF-8.
    unsafe {
        for b in s.as_bytes_mut() {
            *b = 0;
        }
    }
    s.clear();
}

/// The pinned bottom action bar for the Settings dialog: a single right-aligned
/// accent **Done** button. Returns `Some(true)` when clicked, else `None`. The form
/// auto-saves — every edit is applied + persisted live via [`DialogWindow::take_settings_edit`]
/// as it happens — so Done (and Esc / close) only close the window; there is nothing to
/// commit or revert.
fn settings_button_bar(ctx: &egui::Context) -> Option<bool> {
    let p = pbui::Palette::new(ctx.style().visuals.dark_mode);
    let mut result = None;
    // Auto-saving form (like macOS): every edit already applied + persisted live, so the
    // bar is a single **Done** that just closes. No Save (nothing to commit) and no Cancel
    // (nothing to revert). Same shared bar as every other dialog — uniform inset + gap.
    button_bar(ctx, "settings_bar", |ui| {
        if pbui::primary_button(ui, &p, "Done").clicked() {
            result = Some(true);
        }
    });
    result
}

/// The Settings form, laid out as Windows-11-style **grouped setting cards** — related
/// settings share one card under a semibold heading (far less scrolling than a card per
/// setting). Built on the `pbui` design system. The controls edit `d`, a draft built
/// from the live settings on open; each change is folded back + applied live (auto-save)
/// via [`SettingsDraft::to_settings`] — no Save button.
fn settings_ui(
    ui: &mut egui::Ui,
    d: &mut SettingsDraft,
    kb: &mut KbEdit,
    tab: &mut SettingsTab,
    conn_test: &mut ConnTest,
    models: &mut Vec<String>,
) {
    let p = pbui::Palette::new(ui.visuals().dark_mode);

    // Key capture belongs to the Shortcuts tab only; leaving it cancels any armed slot.
    if *tab != SettingsTab::Shortcuts {
        *kb.capturing = None;
    }

    // Pinned tab strip, then the scrolling content for the active tab. Edits on any
    // tab auto-save live; the bottom bar is just Done.
    settings_tab_bar(ui, tab);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Frame::none()
                .inner_margin(egui::Margin {
                    left: pbui::PAGE_MARGIN,
                    right: pbui::PAGE_MARGIN,
                    top: pbui::SPACE_4,
                    bottom: pbui::SPACE_6,
                })
                .show(ui, |ui| {
                    // Explicit vertical spacing only (the gap between cards is uniform).
                    ui.spacing_mut().item_spacing.y = 0.0;
                    match *tab {
                        SettingsTab::General => general_tab(ui, &p, d),
                        SettingsTab::Display => display_tab(ui, &p, d),
                        SettingsTab::Ai => ai_tab(ui, &p, d, conn_test, models),
                        SettingsTab::Shortcuts => keybindings_ui(ui, &p, kb),
                    }
                });
        });
}

/// Dev-only (`--settings-shot`): render one Settings tab (the pinned tab strip + its card
/// stack) into `ui` for a headless PNG preview — screen capture is unreliable on this host
/// (TCC + borderless Metal), so this is the Settings equivalent of `--egui-shot`. Mirrors
/// [`settings_body`] minus the window chrome. `tab` picks the tab; only the card-only tabs
/// (General / Appearance) render content (the AI / Shortcuts tabs need live state — but the
/// tab strip shows all four, so their icons preview here). Uses a default draft.
pub(crate) fn settings_shot_body(ui: &mut egui::Ui, dark: bool, tab_name: &str) {
    let p = pbui::Palette::new(dark);
    let mut draft = SettingsDraft::from_settings(&settings::Settings::default(), 120);
    let mut tab = match tab_name {
        "appearance" => SettingsTab::Display,
        "shortcuts" => SettingsTab::Shortcuts,
        _ => SettingsTab::General,
    };
    settings_tab_bar(ui, &mut tab);
    egui::Frame::none()
        .inner_margin(egui::Margin {
            left: pbui::PAGE_MARGIN,
            right: pbui::PAGE_MARGIN,
            top: pbui::SPACE_4,
            bottom: pbui::SPACE_6,
        })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            match tab {
                SettingsTab::Display => display_tab(ui, &p, &mut draft),
                SettingsTab::Shortcuts => {
                    // A default keymap shows both bound chords and the empty "Set"/"Add"
                    // placeholder slots (some actions have no default binding).
                    let mut keymap = crate::keymap::Keymap::defaults();
                    let mut capturing = None;
                    let mut dirty = false;
                    let mut note = None;
                    let mut kb = KbEdit {
                        keymap: &mut keymap,
                        capturing: &mut capturing,
                        dirty: &mut dirty,
                        note: &mut note,
                    };
                    keybindings_ui(ui, &p, &mut kb);
                }
                _ => general_tab(ui, &p, &mut draft),
            }
        });
}

/// The pinned tab strip atop the Settings dialog. A [`pbui::tab_bar`] pivot: the labels
/// are inset to the page margin so they line up with the cards below, while its hairline
/// (and the active tab's accent underline) span the full width.
fn settings_tab_bar(ui: &mut egui::Ui, current: &mut SettingsTab) {
    ui.add_space(pbui::SPACE_3);
    pbui::tab_bar(
        ui,
        &pbui::Palette::new(ui.visuals().dark_mode),
        current,
        pbui::PAGE_MARGIN,
        &[
            (
                SettingsTab::General,
                "General",
                Some(pbui::icon::Icon::Sliders),
            ),
            (
                SettingsTab::Display,
                "Appearance",
                Some(pbui::icon::Icon::Brush),
            ),
            (SettingsTab::Ai, "AI", Some(pbui::icon::Icon::Sparkles)),
            (
                SettingsTab::Shortcuts,
                "Shortcuts",
                Some(pbui::icon::Icon::Keyboard),
            ),
        ],
    );
}

/// The **AI** tab (task #44): the image-description backend, model, prompt, and the
/// auto-describe / speak toggles, plus a Test-connection probe. Off by default and
/// entirely local unless the user points it at a server (privacy #2 / ADR-018).
fn ai_tab(
    ui: &mut egui::Ui,
    p: &pbui::Palette,
    d: &mut SettingsDraft,
    conn_test: &mut ConnTest,
    models: &mut Vec<String>,
) {
    pbui::group_card(ui, p, Some("Image Descriptions"), |ui| {
        pbui::card_row(
            ui,
            p,
            None,
            "Backend",
            Some("Auto uses Apple's on-device model when available, else your endpoint."),
            |ui| {
                egui::ComboBox::from_id_salt("describe_backend")
                    .width(160.0)
                    .selected_text(
                        ["Auto", "Apple on-device", "Local endpoint"][d.describe_backend.min(2)],
                    )
                    .show_ui(ui, |ui| {
                        pbui::apply_to_ui(ui, p.dark);
                        ui.selectable_value(&mut d.describe_backend, 0, "Auto");
                        // Apple FM is not wired yet (needs macOS 27 + subtask 5) — offered
                        // but disabled so the choice is honest and forward-compatible.
                        ui.add_enabled_ui(false, |ui| {
                            ui.selectable_value(&mut d.describe_backend, 1, "Apple on-device");
                        })
                        .response
                        .on_hover_text("Requires macOS 27 + Apple Intelligence");
                        ui.selectable_value(&mut d.describe_backend, 2, "Local endpoint");
                    });
            },
        );
        // Endpoint / model / Test are meaningful only for the endpoint backends.
        let endpoint_used = d.describe_backend != 1;
        ui.add_enabled_ui(endpoint_used, |ui| {
            pbui::card_row(
                ui,
                p,
                None,
                "Endpoint URL",
                Some("OpenAI-compatible server: LM Studio, Ollama, or llama.cpp."),
                |ui| {
                    ui.add(
                        pbui::text_field(&mut d.describe_endpoint, "http://localhost:1234/v1")
                            .desired_width(230.0),
                    );
                },
            );
            pbui::card_row(
                ui,
                p,
                None,
                "Model",
                Some("Blank = the server's loaded model. Pick from the list after Test."),
                |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            pbui::text_field(&mut d.describe_model, "(loaded model)")
                                .desired_width(150.0),
                        );
                        // The picker fills from the last probe; picking sets the field.
                        let current = if d.describe_model.is_empty() {
                            "(loaded model)".to_string()
                        } else {
                            d.describe_model.clone()
                        };
                        egui::ComboBox::from_id_salt("describe_model_pick")
                            .selected_text(current)
                            .width(72.0)
                            .show_ui(ui, |ui| {
                                pbui::apply_to_ui(ui, p.dark);
                                ui.selectable_value(
                                    &mut d.describe_model,
                                    String::new(),
                                    "(loaded model)",
                                );
                                for m in models.iter() {
                                    // Mark the vision-capable ones — the usable choices.
                                    let label = if pb_app_core::describe::looks_like_vision_model(m)
                                    {
                                        format!("{m}  ◆ vision")
                                    } else {
                                        m.clone()
                                    };
                                    ui.selectable_value(&mut d.describe_model, m.clone(), label);
                                }
                                if models.is_empty() {
                                    ui.label(
                                        egui::RichText::new("Run Test to list models")
                                            .color(p.text_secondary)
                                            .size(12.0),
                                    );
                                }
                            });
                    });
                },
            );
            pbui::card_row(ui, p, None, "Connection", None, |ui| {
                if pbui::secondary_button(ui, "Test & list models").clicked() {
                    start_conn_test(conn_test, &d.describe_endpoint);
                }
            });
            render_conn_test(ui, p, conn_test, models);
        });
        pbui::card_row(
            ui,
            p,
            None,
            "Response length",
            Some("How much the model writes about each image."),
            |ui| {
                egui::ComboBox::from_id_salt("describe_length")
                    .width(140.0)
                    .selected_text(["Brief", "Standard", "Detailed"][d.describe_length.min(2)])
                    .show_ui(ui, |ui| {
                        pbui::apply_to_ui(ui, p.dark);
                        ui.selectable_value(&mut d.describe_length, 0, "Brief");
                        ui.selectable_value(&mut d.describe_length, 1, "Standard");
                        ui.selectable_value(&mut d.describe_length, 2, "Detailed");
                    });
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Auto-describe while open",
            Some("With the description panel up, describe each image you move to — no extra D."),
            |ui| {
                pbui::toggle(ui, p, &mut d.describe_auto);
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Speak descriptions",
            Some("Read the description aloud with the system voice."),
            |ui| {
                pbui::toggle(ui, p, &mut d.speak_descriptions);
            },
        );
    });
    ui.add_space(pbui::SECTION_GAP);

    // Custom prompt — full-width multiline (the built-in instruction is the placeholder).
    pbui::group_card(ui, p, Some("Prompt"), |ui| {
        ui.add(
            egui::TextEdit::multiline(&mut d.describe_prompt)
                .hint_text(pb_app_core::prompt::DEFAULT_INSTRUCTION)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );
        ui.add_space(pbui::SPACE_2);
        ui.label(
            egui::RichText::new(
                "Leave blank to use the built-in instruction. Placeholders: {filename} {folder} \
                 {datetime} {camera} {location} {context}",
            )
            .color(p.text_secondary)
            .size(12.5),
        );
    });
    ui.add_space(pbui::GAP);

    // Privacy note (ADR-018): the honest caveat — images go to the configured endpoint.
    ui.label(
        egui::RichText::new(
            "Images are sent to the model server you set above, so keep it local (this Mac or \
             your own network) and one you trust — with auto-describe on, each photo is sent \
             automatically. Online services may keep images and use them to train. Apple's \
             on-device model (coming later) will run here with no server.",
        )
        .color(p.text_secondary)
        .size(12.5),
    );
}

/// Kick the Test-connection probe on a worker thread (keeps the dialog responsive) and
/// park the receiver in `conn_test`; [`render_conn_test`] polls it.
fn start_conn_test(conn_test: &mut ConnTest, endpoint: &str) {
    let (tx, rx) = std::sync::mpsc::channel();
    let url = endpoint.trim().to_string();
    std::thread::spawn(move || {
        let _ = tx.send(pb_app_core::describe::probe_endpoint(&url).map_err(|e| e.user_message()));
    });
    *conn_test = ConnTest::Testing(rx);
}

/// Poll + render the Test-connection status line: reachability, model count, and a warning
/// when no served model looks vision-capable (the describe path needs a VLM). On success it
/// also fills `models` (vision-first) for the Model picker.
fn render_conn_test(
    ui: &mut egui::Ui,
    p: &pbui::Palette,
    conn_test: &mut ConnTest,
    models: &mut Vec<String>,
) {
    // Poll a running probe without holding the borrow across the reassignment.
    let landed = if let ConnTest::Testing(rx) = conn_test {
        match rx.try_recv() {
            Ok(result) => Some(Some(result)),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ui.ctx().request_repaint(); // keep the frame loop polling
                None
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(None),
        }
    } else {
        None
    };
    if let Some(outcome) = landed {
        // Capture the fetched models (vision-first) for the picker before summarizing.
        if let Some(Ok(list)) = &outcome {
            *models = pb_app_core::describe::sort_models_vision_first(list.clone());
        }
        *conn_test = match outcome {
            Some(Ok(models)) if models.is_empty() => ConnTest::Done {
                ok: false,
                msg: "Reachable, but no models are loaded.".to_string(),
            },
            Some(Ok(models)) => {
                let n = models.len();
                let plural = if n == 1 { "" } else { "s" };
                if models
                    .iter()
                    .any(|m| pb_app_core::describe::looks_like_vision_model(m))
                {
                    ConnTest::Done {
                        ok: true,
                        msg: format!("Reachable · {n} model{plural} · vision model present"),
                    }
                } else {
                    ConnTest::Done {
                        ok: false,
                        msg: format!(
                            "Reachable · {n} model{plural}, but none look vision-capable — \
                             describe needs a VLM (e.g. qwen2.5-vl)."
                        ),
                    }
                }
            }
            Some(Err(msg)) => ConnTest::Done { ok: false, msg },
            None => ConnTest::Done {
                ok: false,
                msg: "Test failed.".to_string(),
            },
        };
    }
    match conn_test {
        ConnTest::Idle => {}
        ConnTest::Testing(_) => {
            ui.label(
                egui::RichText::new("Testing…")
                    .color(p.text_secondary)
                    .size(12.5),
            );
        }
        ConnTest::Done { ok, msg } => {
            let color = if *ok { p.accent } else { p.danger };
            ui.label(egui::RichText::new(msg.as_str()).color(color).size(12.5));
        }
    }
}

/// The **General** tab: hold-to-fly tuning, startup defaults, and system actions.
fn general_tab(ui: &mut egui::Ui, p: &pbui::Palette, d: &mut SettingsDraft) {
    let cap = d.refresh_hz;
    pbui::group_card(ui, p, Some("Navigation Feel"), |ui| {
        pbui::card_row(
            ui,
            p,
            None,
            "Start speed",
            Some("Images per second when you first hold a key"),
            |ui| {
                pbui::slider(ui, &mut d.start_speed, 1.0..=30.0, "/s");
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Ramp-up time",
            Some("Seconds to accelerate from start speed to max"),
            |ui| {
                pbui::slider(ui, &mut d.ramp_secs, 0.5..=10.0, " s");
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Max speed",
            Some("Upper limit while holding (capped at the refresh rate)"),
            |ui| {
                pbui::slider(ui, &mut d.max_fps, 1..=cap, "/s");
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Hold delay",
            Some("Pause before a held key starts repeating"),
            |ui| {
                pbui::slider(ui, &mut d.hold_delay_ms, 0..=1000, " ms");
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Scroll wheel",
            Some("Scroll & two-finger swipe mode. Hold Ctrl to switch modes."),
            |ui| {
                egui::ComboBox::from_id_salt("scroll_action")
                    .width(150.0)
                    .selected_text(["Pan", "Zoom"][d.scroll_action.min(1)])
                    .show_ui(ui, |ui| {
                        pbui::apply_to_ui(ui, p.dark);
                        ui.selectable_value(&mut d.scroll_action, 0, "Pan");
                        ui.selectable_value(&mut d.scroll_action, 1, "Zoom");
                    });
            },
        );
    });
    ui.add_space(pbui::SECTION_GAP);

    pbui::group_card(ui, p, Some("Slideshow"), |ui| {
        pbui::card_row(
            ui,
            p,
            None,
            "Slideshow interval",
            Some("Seconds each image shows. The [ and ] keys adjust it live."),
            |ui| {
                pbui::slider_stepped(ui, &mut d.slideshow_interval, 0.1..=60.0, 0.1, 1, "s");
            },
        );
    });
    ui.add_space(pbui::SECTION_GAP);

    pbui::group_card(ui, p, Some("Startup"), |ui| {
        pbui::card_row(
            ui,
            p,
            None,
            "Window mode",
            Some("How the window opens at launch"),
            |ui| {
                egui::ComboBox::from_id_salt("startup_mode")
                    .width(150.0)
                    .selected_text(["Fullscreen", "Windowed", "Remember last"][d.startup_mode])
                    .show_ui(ui, |ui| {
                        pbui::apply_to_ui(ui, p.dark);
                        ui.selectable_value(&mut d.startup_mode, 0, "Fullscreen");
                        ui.selectable_value(&mut d.startup_mode, 1, "Windowed");
                        ui.selectable_value(&mut d.startup_mode, 2, "Remember last");
                    });
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Open new folders recursively",
            Some("Default for newly opened folders. The View menu toggles the current one."),
            |ui| {
                pbui::toggle_with_label(ui, p, &mut d.recursive);
            },
        );
    });
    ui.add_space(pbui::SECTION_GAP);

    pbui::group_card(ui, p, Some("File Picker"), |ui| {
        pbui::card_row(
            ui,
            p,
            None,
            "Open in",
            Some("Where the Open dialog starts. A specific folder also keeps it from remembering where you last browsed."),
            |ui| {
                egui::ComboBox::from_id_salt("picker_mode")
                    .width(190.0)
                    .selected_text(if d.picker_fixed {
                        "A specific folder"
                    } else {
                        "Current image\u{2019}s folder"
                    })
                    .show_ui(ui, |ui| {
                        pbui::apply_to_ui(ui, p.dark);
                        ui.selectable_value(
                            &mut d.picker_fixed,
                            false,
                            "Current image\u{2019}s folder",
                        );
                        ui.selectable_value(&mut d.picker_fixed, true, "A specific folder");
                    });
            },
        );
        if d.picker_fixed {
            let path_label = d
                .picker_dir
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "No folder chosen yet".to_string());
            pbui::card_row(ui, p, None, "Folder", Some(path_label.as_str()), |ui| {
                if pbui::secondary_button(ui, "Choose\u{2026}").clicked() {
                    let start = d.picker_dir.clone();
                    let mut dlg = rfd::FileDialog::new();
                    if let Some(cur) = &start {
                        dlg = dlg.set_directory(cur);
                    }
                    if let Some(dir) = dlg.pick_folder() {
                        d.picker_dir = Some(dir);
                    }
                }
            });
        }
    });
    ui.add_space(pbui::SECTION_GAP);

    pbui::group_card(ui, p, Some("System"), |ui| {
        pbui::card_row(
            ui,
            p,
            None,
            "Default image viewer",
            Some("Opens PhotoBlaze's page in Windows Default apps"),
            |ui| {
                if pbui::secondary_button(ui, "Set default\u{2026}").clicked() {
                    open_default_apps();
                }
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Reset settings",
            Some("Restore every setting to its default"),
            |ui| {
                if pbui::secondary_button(ui, "Reset").clicked() {
                    // Repopulate the form with the defaults; the post-frame fold sees the
                    // diff and applies + persists it live, same as any other edit.
                    let hz = d.refresh_hz;
                    *d = SettingsDraft::from_settings(&settings::Settings::default(), hz);
                }
            },
        );
    });
}

/// The **Display** tab: how a photo is framed and how the overlays look.
fn display_tab(ui: &mut egui::Ui, p: &pbui::Palette, d: &mut SettingsDraft) {
    pbui::group_card(ui, p, Some("Appearance"), |ui| {
        pbui::card_row(
            ui,
            p,
            None,
            "Theme",
            Some("HUD and background colors; System follows the OS"),
            |ui| {
                egui::ComboBox::from_id_salt("appearance_mode")
                    .width(150.0)
                    .selected_text(["System", "Light", "Dark"][d.appearance])
                    .show_ui(ui, |ui| {
                        pbui::apply_to_ui(ui, p.dark);
                        ui.selectable_value(&mut d.appearance, 0, "System");
                        ui.selectable_value(&mut d.appearance, 1, "Light");
                        ui.selectable_value(&mut d.appearance, 2, "Dark");
                    });
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Accent color",
            Some("Buttons, tabs, and highlights; System follows your OS accent"),
            |ui| {
                egui::ComboBox::from_id_salt("accent_source")
                    .width(150.0)
                    .selected_text(["System", "Custom", "Blaze Orange"][d.accent_source])
                    .show_ui(ui, |ui| {
                        pbui::apply_to_ui(ui, p.dark);
                        ui.selectable_value(&mut d.accent_source, 0, "System");
                        ui.selectable_value(&mut d.accent_source, 1, "Custom");
                        ui.selectable_value(&mut d.accent_source, 2, "Blaze Orange");
                    });
            },
        );
        // The custom-color picker only when Custom is chosen (otherwise it's inert).
        if d.accent_source == 1 {
            pbui::card_row(
                ui,
                p,
                None,
                "Custom accent color",
                Some("Pick any color; it is kept readable against the panels"),
                |ui| {
                    ui.color_edit_button_srgb(&mut d.accent_custom);
                },
            );
        }
        pbui::card_row(
            ui,
            p,
            None,
            "Default scale mode",
            Some("How an image fits the window"),
            |ui| {
                egui::ComboBox::from_id_salt("scale_mode")
                    .width(150.0)
                    .selected_text(["Fit", "Crop to Fill", "Original"][d.scale_mode])
                    .show_ui(ui, |ui| {
                        // The popup is a top-level Area; re-assert our theme so options
                        // match the dialog (defensive — the ctx style already matches here).
                        pbui::apply_to_ui(ui, p.dark);
                        ui.selectable_value(&mut d.scale_mode, 0, "Fit");
                        ui.selectable_value(&mut d.scale_mode, 1, "Crop to Fill");
                        ui.selectable_value(&mut d.scale_mode, 2, "Original");
                    });
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Letterbox color (dark)",
            Some("Fills the screen around an image in the dark theme"),
            |ui| {
                ui.color_edit_button_rgb(&mut d.letterbox);
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Letterbox color (light)",
            Some("Fills the screen around an image in the light theme"),
            |ui| {
                ui.color_edit_button_rgb(&mut d.letterbox_light);
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Info panel opacity",
            Some("How solid the info and EXIF panels look over an image"),
            |ui| {
                pbui::slider(ui, &mut d.info_opacity, 0..=100, "%");
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "File info position",
            Some("Where the one-line info readout (I) sits along the bottom"),
            |ui| {
                egui::ComboBox::from_id_salt("info_line_align")
                    .width(150.0)
                    .selected_text(["Left", "Center", "Right"][d.info_line_align])
                    .show_ui(ui, |ui| {
                        pbui::apply_to_ui(ui, p.dark);
                        ui.selectable_value(&mut d.info_line_align, 0, "Left");
                        ui.selectable_value(&mut d.info_line_align, 1, "Center");
                        ui.selectable_value(&mut d.info_line_align, 2, "Right");
                    });
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Show toolbar",
            Some("A row of buttons under the menu for mouse control (the keyboard does it all without it)"),
            |ui| {
                pbui::toggle(ui, p, &mut d.show_toolbar);
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Show image info by default",
            Some("Whether the one-line readout starts shown on launch (I still toggles it)"),
            |ui| {
                pbui::toggle(ui, p, &mut d.show_image_info);
            },
        );
        pbui::card_row(ui, p, None, "Show folder", None, |ui| {
            pbui::toggle(ui, p, &mut d.info_show_folder);
        });
        pbui::card_row(ui, p, None, "Show filename", None, |ui| {
            pbui::toggle(ui, p, &mut d.info_show_filename);
        });
        pbui::card_row(ui, p, None, "Show resolution", None, |ui| {
            pbui::toggle(ui, p, &mut d.info_show_resolution);
        });
        pbui::card_row(ui, p, None, "Show codec", None, |ui| {
            pbui::toggle(ui, p, &mut d.info_show_codec);
        });
    });
}

/// The inline keyboard-shortcut editor: grouped cards (matching the menu), each
/// command with a Primary and Secondary chord slot. Clicking a slot arms key capture
/// (handled outside egui in [`DialogWindow::handle_capture_event`]); the actual edits
/// land on the draft keymap in `kb` and auto-save live (a changed binding is applied +
/// persisted the same frame).
fn keybindings_ui(ui: &mut egui::Ui, p: &pbui::Palette, kb: &mut KbEdit) {
    // A capture prompt while a slot is armed, else a transient "moved from …" note.
    if kb.capturing.is_some() {
        ui.label(egui::RichText::new("Press a key to bind it. Esc cancels.").color(p.accent));
        ui.add_space(pbui::SPACE_2);
    } else if let Some(note) = kb.note.clone() {
        ui.label(egui::RichText::new(note).color(p.text_secondary));
        ui.add_space(pbui::SPACE_2);
    }

    for &(title, actions) in KB_GROUPS {
        pbui::group_card(ui, p, Some(title), |ui| {
            for &action in actions {
                pbui::card_row(ui, p, None, action.label(), None, |ui| {
                    ui.horizontal(|ui| {
                        chord_slot(ui, p, kb, action, 0);
                        ui.add_space(pbui::SPACE_2);
                        chord_slot(ui, p, kb, action, 1);
                    });
                });
            }
        });
        ui.add_space(pbui::SECTION_GAP);
    }

    if pbui::secondary_button(ui, "Reset shortcuts to defaults").clicked() {
        kb.keymap.reset_to_defaults();
        *kb.dirty = true;
        *kb.capturing = None;
        *kb.note = None;
    }
}

/// One chord slot (primary or secondary) for a command. Idle: a button showing the
/// bound chord, or a dimmed "Set"/"Add" placeholder when empty — clicking it arms capture. Armed: a
/// "Press a key…" prompt plus a Clear button that removes the binding.
fn chord_slot(ui: &mut egui::Ui, p: &pbui::Palette, kb: &mut KbEdit, action: Action, slot: usize) {
    if *kb.capturing == Some((action, slot)) {
        ui.label(egui::RichText::new("Press a key\u{2026}").color(p.accent));
        if pbui::secondary_button(ui, "Clear").clicked() {
            kb.keymap.clear_slot(action, slot);
            *kb.dirty = true;
            *kb.capturing = None;
            *kb.note = None;
        }
        return;
    }
    // A bound slot shows its chord in the normal button style; an empty slot shows a dimmed
    // "Set"/"Add" placeholder (no ellipsis) so it reads as "nothing here yet, click to bind".
    let clicked = match kb.keymap.slot(action, slot) {
        Some(c) => pbui::secondary_button(ui, &c.to_string()).clicked(),
        None => pbui::placeholder_button(ui, p, if slot == 0 { "Set" } else { "Add" }).clicked(),
    };
    if clicked {
        *kb.capturing = Some((action, slot));
        *kb.note = None;
    }
}

/// Clamp a dialog's top-left `pos` so its `size` (`w`,`h`) rect stays fully inside the
/// monitor rect `mon` (`x`,`y`,`w`,`h`), all physical px. A dialog centered over a
/// parent window at a screen corner would otherwise spill off the monitor edge (#4).
/// If the dialog is larger than the monitor it's pinned to the monitor's top-left.
fn clamp_to_monitor(pos: (f64, f64), size: (f64, f64), mon: (f64, f64, f64, f64)) -> (f64, f64) {
    let (x, y) = pos;
    let (w, h) = size;
    let (mx, my, mw, mh) = mon;
    let cx = if w >= mw {
        mx
    } else {
        x.clamp(mx, mx + mw - w)
    };
    let cy = if h >= mh {
        my
    } else {
        y.clamp(my, my + mh - h)
    };
    (cx, cy)
}

#[cfg(test)]
mod tests {
    use super::{clamp_to_monitor, elide_path, fmt_count, settings, SettingsDraft};

    // Auto-save invariant #1 (privacy-critical): opening Settings and changing nothing must
    // write nothing. The dialog seeds its fold baseline from the *normalized* round-trip, so
    // even a non-normalized stored value (an untrimmed endpoint, an over-range advance rate)
    // folds equal on the first frame → no `SettingsEdited`, no `settings.toml` write.
    #[test]
    fn opening_settings_with_no_change_is_a_noop() {
        let s = settings::Settings {
            describe_endpoint: "  http://localhost:11434  ".to_string(), // to_settings trims
            max_advance_rate: 100_000, // clamps to "uncapped" (0) against any real refresh
            ..settings::Settings::default()
        };
        let hz = 120;
        let draft = SettingsDraft::from_settings(&s, hz);
        let base = draft.to_settings(&s); // what DialogWindow stores as `settings_base`
                                          // The first render frame re-folds the same (unedited) draft against `base`:
        assert_eq!(
            draft.to_settings(&base),
            base,
            "opening Settings with no change must not differ from the baseline"
        );
    }

    // Auto-save invariant #2: a genuine edit is detected against the normalized baseline, so
    // it *does* emit a live `SettingsEdited`.
    #[test]
    fn an_edit_is_detected_against_the_baseline() {
        let s = settings::Settings::default();
        let hz = 120;
        let mut draft = SettingsDraft::from_settings(&s, hz);
        let base = draft.to_settings(&s);
        draft.recursive = !draft.recursive; // as a toggle click would
        assert_ne!(
            draft.to_settings(&base),
            base,
            "a real edit must differ from the baseline (emits SettingsEdited)"
        );
    }

    #[test]
    fn fmt_count_groups_thousands() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(7), "7");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1,000");
        assert_eq!(fmt_count(1_234), "1,234");
        assert_eq!(fmt_count(12_345), "12,345");
        assert_eq!(fmt_count(1_000_000), "1,000,000");
    }

    #[test]
    fn elide_path_keeps_last_components() {
        // Short paths pass through (normalized to forward slashes).
        assert_eq!(elide_path("Iceland"), "Iceland");
        assert_eq!(elide_path("2024/Iceland"), "2024/Iceland");
        assert_eq!(elide_path("Photos/2024/Iceland"), "Photos/2024/Iceland");
        // Deep paths keep only the last few, prefixed with an ellipsis.
        assert_eq!(
            elide_path("Pictures/Photos/2024/Iceland"),
            "\u{2026}/Photos/2024/Iceland"
        );
        // Either separator is accepted (the worker formats with the platform's).
        assert_eq!(elide_path("a\\b\\c\\d\\e"), "\u{2026}/c/d/e");
        // Empty stays empty (the dialog hides the caption then).
        assert_eq!(elide_path(""), "");
    }

    // A 1920×1080 monitor at the origin.
    const MON: (f64, f64, f64, f64) = (0.0, 0.0, 1920.0, 1080.0);

    #[test]
    fn already_on_screen_is_unchanged() {
        let (x, y) = clamp_to_monitor((100.0, 100.0), (560.0, 660.0), MON);
        assert_eq!((x, y), (100.0, 100.0));
    }

    #[test]
    fn off_the_right_and_bottom_is_pulled_back_fully_on() {
        // Centered over a parent at the bottom-right corner pushes it past both edges.
        let (x, y) = clamp_to_monitor((1800.0, 900.0), (560.0, 660.0), MON);
        assert_eq!(x, 1920.0 - 560.0); // flush to the right edge, fully visible
        assert_eq!(y, 1080.0 - 660.0); // flush to the bottom edge, fully visible
    }

    #[test]
    fn off_the_top_left_is_pushed_in() {
        let (x, y) = clamp_to_monitor((-300.0, -200.0), (560.0, 660.0), MON);
        assert_eq!((x, y), (0.0, 0.0));
    }

    #[test]
    fn respects_a_monitor_at_a_negative_origin() {
        // A left-hand second monitor at negative x: clamping must use its own bounds.
        let mon = (-1920.0, 0.0, 1920.0, 1080.0);
        let (x, y) = clamp_to_monitor((-100.0, 50.0), (560.0, 660.0), mon);
        assert_eq!(x, -560.0); // flush to that monitor's right edge (x = 0 - 560)
        assert_eq!(y, 50.0);
    }

    #[test]
    fn dialog_larger_than_monitor_pins_to_top_left() {
        let (x, y) = clamp_to_monitor((50.0, 50.0), (4000.0, 4000.0), MON);
        assert_eq!((x, y), (0.0, 0.0));
    }
}
