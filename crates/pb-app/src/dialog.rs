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

/// Which section of the Settings dialog is showing. The bottom Save/Cancel bar is
/// global (it commits every tab's edits at once); tabs only switch what's visible.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    #[default]
    General,
    Display,
    Shortcuts,
}

/// The egui-facing edit state for the Settings form — the same fields as
/// [`settings::Settings`] but in shapes the egui widgets want (a combo index, an
/// `f32` color). Built from the live settings on open ([`SettingsDraft::from_settings`])
/// and folded back on Save ([`SettingsDraft::to_settings`]).
struct SettingsDraft {
    /// Display refresh in Hz — caps the max-speed slider (not persisted).
    refresh_hz: u32,
    start_speed: f32,
    ramp_secs: f32,
    max_fps: u32,
    hold_delay_ms: u32,
    scroll_action: usize, // 0 = Pan, 1 = Zoom (what a plain scroll does)
    recursive: bool,
    scale_mode: usize,       // 0 = Fit, 1 = Fill, 2 = Original
    letterbox: [f32; 3],     // 0..1 per channel (egui color picker)
    info_opacity: u8,        // 0..100
    startup_mode: usize,     // 0 = Fullscreen, 1 = Windowed, 2 = Remember
    slideshow_interval: f64, // seconds (default slideshow dwell)
    /// File-picker start: `false` = the current photo's folder, `true` = a pinned folder.
    picker_fixed: bool,
    /// The pinned folder (when `picker_fixed`); `None` until the user chooses one.
    picker_dir: Option<PathBuf>,
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
            letterbox: [
                s.letterbox[0] as f32 / 255.0,
                s.letterbox[1] as f32 / 255.0,
                s.letterbox[2] as f32 / 255.0,
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
        s.letterbox = [
            (self.letterbox[0] * 255.0).round().clamp(0.0, 255.0) as u8,
            (self.letterbox[1] * 255.0).round().clamp(0.0, 255.0) as u8,
            (self.letterbox[2] * 255.0).round().clamp(0.0, 255.0) as u8,
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
        s.clamp();
        s
    }
}

/// The keybinding editor's mutable state, lent to [`settings_ui`] so the inline
/// editor can read/rebind the draft keymap. The actual key *capture* happens in
/// [`DialogWindow::handle_capture_event`] (raw winit events), not in egui — egui only
/// arms a slot and renders the result.
struct KbEdit<'a> {
    /// The draft keymap being edited (a clone of the live one; committed on Save).
    keymap: &'a mut Keymap,
    /// The slot awaiting a keypress (`Some((action, slot))`), or `None` when idle.
    capturing: &'a mut Option<(Action, usize)>,
    /// Whether any binding changed (so Save knows to persist + apply the keymap).
    dirty: &'a mut bool,
    /// A transient note shown atop the section, e.g. "Moved Ctrl+C from Copy".
    note: &'a mut Option<String>,
}

/// The keyboard-shortcut editor, grouped into cards by area (matching the menu).
/// Every [`Action`] appears so any command is rebindable, including ones with no
/// default key (their slots read "Set…"/"Add…").
const KB_GROUPS: &[(&str, &[Action])] = &[
    (
        "Navigation",
        &[
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
        ],
    ),
    (
        "View",
        &[
            Action::ScaleFit,
            Action::ScaleFill,
            Action::ScaleOriginal,
            Action::ToggleOriginal,
            Action::Info,
            Action::FullExif,
            Action::Help,
            Action::Fullscreen,
            Action::Recursive,
        ],
    ),
    (
        "Image & File",
        &[
            Action::RotateCw,
            Action::RotateCcw,
            Action::Copy,
            Action::SaveRotation,
            Action::Delete,
            Action::DeletePermanent,
            Action::OpenFile,
            Action::OpenFolder,
        ],
    ),
    (
        "Animation",
        &[
            Action::PlayPause,
            Action::FrameNext,
            Action::FramePrev,
            Action::MuteLiveAudio,
        ],
    ),
    (
        "Application",
        &[Action::Settings, Action::About, Action::Quit],
    ),
];

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
    /// The settings as they were when the dialog opened — the base the edited draft
    /// is folded onto on Save (so unexposed fields survive) and the Cancel baseline.
    settings_base: settings::Settings,
    /// The edited settings the user committed with **Save** (a [`DialogKind::Settings`]
    /// dialog), taken by [`take_settings_result`] right after the answering frame.
    ///
    /// [`take_settings_result`]: DialogWindow::take_settings_result
    submitted_settings: Option<settings::Settings>,
    /// The draft keymap edited by the inline keybinding editor (a clone of the live
    /// one, seeded at open). Committed on Save, discarded on Cancel.
    keymap_draft: Keymap,
    /// Whether any binding was changed (so Save only persists/applies if it was).
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
    /// Which Settings tab (General / Display / Shortcuts) is showing.
    settings_tab: SettingsTab,
    /// The edited keymap committed with **Save** (only set when it actually changed),
    /// taken by [`take_keymap_result`].
    ///
    /// [`take_keymap_result`]: DialogWindow::take_keymap_result
    submitted_keymap: Option<Keymap>,
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
            // macOS renders the body in SF Pro, whose taller line metrics need ~30px more
            // height than Segoe so the GitHub link at the bottom isn't clipped.
            #[cfg(target_os = "macos")]
            DialogKind::About => (254.0, 337.0, false, "About PhotoBlaze"),
            #[cfg(not(target_os = "macos"))]
            DialogKind::About => (254.0, 307.0, false, "About PhotoBlaze"),
            DialogKind::Settings => (560.0, 660.0, true, "PhotoBlaze Settings"),
            DialogKind::Confirm => (450.0, 172.0, false, "Confirm Delete"),
            DialogKind::Message => (470.0, 185.0, false, "PhotoBlaze"),
            DialogKind::Password => (500.0, 250.0, false, "Password Required"),
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
        let dark_ui = window.theme() != Some(Theme::Light);

        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).ok()?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
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
            draft: SettingsDraft::from_settings(settings, refresh_hz),
            settings_base: settings.clone(),
            submitted_settings: None,
            keymap_draft: keymap.clone(),
            keymap_dirty: false,
            capturing: None,
            cap_ctrl: false,
            cap_shift: false,
            cap_alt: false,
            cap_logo: false,
            keymap_note: None,
            settings_tab: SettingsTab::default(),
            submitted_keymap: None,
            confirm_msg: message.to_string(),
            confirm_result: None,
            password_input: String::new(),
            password_error: None,
            checking: false,
            submitted_password: None,
            focus_password: matches!(kind, DialogKind::Password),
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

    /// Take the edited settings the user committed with **Save** on a
    /// [`DialogKind::Settings`] dialog, set during the answering frame. `None` until
    /// then. The caller pairs this with a `take_confirm_result()` of `Some(true)`.
    pub fn take_settings_result(&mut self) -> Option<settings::Settings> {
        self.submitted_settings.take()
    }

    /// Take the edited keymap committed with **Save**, set during the answering frame
    /// *only if a binding actually changed*. `None` otherwise. Paired with a
    /// `take_confirm_result()` of `Some(true)`.
    pub fn take_keymap_result(&mut self) -> Option<Keymap> {
        self.submitted_keymap.take()
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
        let mut kb = KbEdit {
            keymap: &mut self.keymap_draft,
            capturing: &mut self.capturing,
            dirty: &mut self.keymap_dirty,
            note: &mut self.keymap_note,
        };
        let settings_tab = &mut self.settings_tab;
        let mut confirm_click: Option<bool> = None;
        let full_output = ctx.run(raw_input, |ctx| match kind {
            DialogKind::About => {
                egui::CentralPanel::default().show(ctx, |ui| about_ui(ui, icon.as_ref()));
            }
            DialogKind::Settings => {
                // Pinned action bar at the bottom, then the scrolling settings page.
                // Save / Cancel both answer the dialog → the main loop closes it.
                confirm_click = settings_button_bar(ctx);
                egui::CentralPanel::default()
                    .frame(egui::Frame::default().fill(ctx.style().visuals.panel_fill))
                    .show(ctx, |ui| settings_ui(ui, draft, &mut kb, settings_tab));
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
        if confirm_click.is_some() {
            self.confirm_result = confirm_click;
            // On Unlock/Enter, snapshot the entered text for the app to validate.
            if confirm_click == Some(true) && kind == DialogKind::Password {
                self.submitted_password = Some(self.password_input.clone());
            }
            // On Save, fold the edited draft onto the open-time base for the app to apply.
            if confirm_click == Some(true) && kind == DialogKind::Settings {
                self.submitted_settings = Some(self.draft.to_settings(&self.settings_base));
                // Hand back the edited keymap only if a binding actually changed.
                if self.keymap_dirty {
                    self.submitted_keymap = Some(self.keymap_draft.clone());
                }
            }
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
        ui.add_space(10.0);
        ui.label("An ultra-fast photo viewer");
        ui.add_space(8.0);
        ui.label("\u{00a9} JD Lien 2026");
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
/// (the user must confirm in Settings), so we deep-link to the right page. Best-effort,
/// and a no-op on other platforms for now.
fn open_default_apps() {
    #[cfg(windows)]
    {
        // `explorer.exe` resolves the `ms-settings:` protocol → the Default apps page.
        let _ = std::process::Command::new("explorer.exe")
            .arg("ms-settings:defaultapps")
            .spawn();
    }
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

/// The pinned bottom action bar for the Settings dialog: a right-aligned
/// `[Save] [Cancel]` pair (Save accent-filled). Returns `Some(true)` on Save,
/// `Some(false)` on Cancel, else `None`. On Save the main loop takes the edited
/// settings ([`DialogWindow::take_settings_result`]) and applies + persists them;
/// Cancel / Esc just close, discarding the draft.
fn settings_button_bar(ctx: &egui::Context) -> Option<bool> {
    let p = pbui::Palette::new(ctx.style().visuals.dark_mode);
    let mut result = None;
    // Same shared bar as every other dialog — uniform inset + button gap, no hand-spacing.
    button_bar(ctx, "settings_bar", |ui| {
        if pbui::secondary_button(ui, "Cancel").clicked() {
            result = Some(false);
        }
        if pbui::primary_button(ui, &p, "Save").clicked() {
            result = Some(true);
        }
    });
    result
}

/// The Settings form, laid out as Windows-11-style **grouped setting cards** — related
/// settings share one card under a semibold heading (far less scrolling than a card per
/// setting). Built on the `pbui` design system. The controls edit `d`, a draft built
/// from the live settings on open; Save folds it back via [`SettingsDraft::to_settings`].
fn settings_ui(ui: &mut egui::Ui, d: &mut SettingsDraft, kb: &mut KbEdit, tab: &mut SettingsTab) {
    let p = pbui::Palette::new(ui.visuals().dark_mode);

    // Key capture belongs to the Shortcuts tab only; leaving it cancels any armed slot.
    if *tab != SettingsTab::Shortcuts {
        *kb.capturing = None;
    }

    // Pinned tab strip, then the scrolling content for the active tab. Save/Cancel
    // (the bottom bar) is global — it commits every tab's edits at once.
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
                        SettingsTab::Shortcuts => keybindings_ui(ui, &p, kb),
                    }
                });
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
            (SettingsTab::General, "General"),
            (SettingsTab::Display, "Display"),
            (SettingsTab::Shortcuts, "Shortcuts"),
        ],
    );
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
            Some("Photos per second when you first hold a key"),
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
    ui.add_space(pbui::GAP);

    pbui::group_card(ui, p, Some("Slideshow"), |ui| {
        pbui::card_row(
            ui,
            p,
            None,
            "Slideshow interval",
            Some("Seconds each photo shows. The [ and ] keys adjust it live."),
            |ui| {
                pbui::slider_stepped(ui, &mut d.slideshow_interval, 0.1..=60.0, 0.1, 1, "s");
            },
        );
    });
    ui.add_space(pbui::GAP);

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
    ui.add_space(pbui::GAP);

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
                        "Current photo\u{2019}s folder"
                    })
                    .show_ui(ui, |ui| {
                        pbui::apply_to_ui(ui, p.dark);
                        ui.selectable_value(
                            &mut d.picker_fixed,
                            false,
                            "Current photo\u{2019}s folder",
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
    ui.add_space(pbui::GAP);

    pbui::group_card(ui, p, Some("System"), |ui| {
        pbui::card_row(
            ui,
            p,
            None,
            "Default photo viewer",
            Some("Opens Windows Default apps to set PhotoBlaze"),
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
            Some("Restore every setting to its default (applies on Save)"),
            |ui| {
                if pbui::secondary_button(ui, "Reset").clicked() {
                    // Repopulate the form with the defaults; Save commits, Cancel still
                    // reverts (nothing is written yet).
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
            "Default scale mode",
            Some("How a photo fits the window"),
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
            "Letterbox color",
            Some("Fills the screen around a photo that doesn\u{2019}t cover it"),
            |ui| {
                ui.color_edit_button_rgb(&mut d.letterbox);
            },
        );
        pbui::card_row(
            ui,
            p,
            None,
            "Info panel opacity",
            Some("How solid the info and EXIF panels look over a photo"),
            |ui| {
                pbui::slider(ui, &mut d.info_opacity, 0..=100, "%");
            },
        );
    });
}

/// The inline keyboard-shortcut editor: grouped cards (matching the menu), each
/// command with a Primary and Secondary chord slot. Clicking a slot arms key capture
/// (handled outside egui in [`DialogWindow::handle_capture_event`]); the actual edits
/// land on the draft keymap in `kb`, committed on the dialog's Save.
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
        ui.add_space(pbui::GAP);
    }

    if pbui::secondary_button(ui, "Reset shortcuts to defaults").clicked() {
        kb.keymap.reset_to_defaults();
        *kb.dirty = true;
        *kb.capturing = None;
        *kb.note = None;
    }
}

/// One chord slot (primary or secondary) for a command. Idle: a button showing the
/// bound chord, or "Set…"/"Add…" when empty — clicking it arms capture. Armed: a
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
    let label = match kb.keymap.slot(action, slot) {
        Some(c) => c.to_string(),
        None if slot == 0 => "Set\u{2026}".to_string(),
        None => "Add\u{2026}".to_string(),
    };
    if pbui::secondary_button(ui, &label).clicked() {
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
    use super::{clamp_to_monitor, elide_path, fmt_count};

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
