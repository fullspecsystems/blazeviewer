#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! PhotoBlaze — the application shell (Phase 3: the prefetch engine).
//!
//! A chrome-less, fit-to-screen viewer built to **hold a key and blaze**. Decode +
//! file I/O run on a priority worker pool (`decode_pool`), neighbors are decoded
//! *ahead* of you and uploaded into a resident GPU texture ring, so a keypress is
//! a **rebind, never a decode or upload**. Advance is **gated on readiness**:
//! every photo is shown in order (none skipped); a cache miss holds the previous
//! frame until its decode lands, then shows it — blaze speed is min(refresh, decode).
//!
//!   space       next photo  ·  ⌫  previous photo
//!   enter       random photo (precomputed shuffle; hold to blaze)
//!   shift+enter  previous random photo (step back through the random walk)
//!   ← ↑ ↓ →     pan around the photo (hold; accelerates)
//!   = / -       zoom in / out (hold; accelerates; numpad +/- too)
//!   8 / 9       scaling mode: fit / fill (all prefetched)
//!   0           toggle original 1:1 ↔ fit
//!   r / Shift+R rotate 90° clockwise / counter-clockwise (per-image, RAM-only)
//!   Ctrl+R      toggle recursive subfolder scan (keeps the current photo)
//!   o / Shift+O open file(s) / open a folder (native picker)
//!   F11 / Alt+Enter  toggle fullscreen <-> windowed
//!   i / Shift+I info panel (path · WxH · codec) / full-EXIF "nerd" panel
//!   / or ?      keybindings help overlay
//!   esc         quit
//!
//! Usage:
//!   cargo run -p pb-app --release -- "D:\Media\Pictures\2003\Halloween"
//!   cargo run -p pb-app --release -- "D:\Media\Pictures" -r          # recurse subfolders
//!   cargo run -p pb-app --release -- "D:\Media\Pictures" -r --windowed --metrics

// pb-app is the **Windows/Linux** winit shell — macOS ships via the native SwiftUI host
// (mac/ + the pb-mac-ffi staticlib), which never links this crate (task #70 — the NS0–NS2
// rearchitecture made the host the mac app). The unsupported-target guard lives in build.rs:
// a build-script check fails with one clean message on macOS/other targets, instead of a rustc
// cascade from the (intentionally) macOS-incomplete source here.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorIcon, Icon, Window, WindowId};

use pb_core::open::{self, LaunchInput, Source};
use pb_core::{Playlist, ResidentRing};
use pb_decode::{decode_bytes, FitBox};
use pb_render::{Renderer, ViewTransform, WgpuRenderer};
use pb_source::ItemSource;

mod accent;
mod clipboard;
#[cfg(windows)]
mod darkmode;
mod default_app;
mod dialog;
mod egui_overlay;
mod egui_shot;
mod hud_gallery;
mod live_audio;
mod md;
mod menu;
mod panels_ui;
mod pb_key_winit;
mod reveal;
mod sdf_rect;
// Windows single-instance: elect one primary process, forward later launches
// (Explorer double-click / multi-select) to it via WM_COPYDATA (task #14).
#[cfg(windows)]
mod single_instance;
mod toolbar;
mod video_audio;
// WASAPI shared-mode render engine for the Windows video-audio backend (MF Source
// Reader → PCM → WASAPI); the video_audio.rs Windows imp is a thin front over it.
#[cfg(windows)]
mod wasapi_audio;
// Velopack per-user installer lifecycle hooks + background auto-update (Windows ship path).
mod update;
mod win_console;
// The action vocabulary, physical-key model, keymap, and slideshow timing now live
// in the platform-neutral `pb-app-core` (NS0). Re-export them at the crate root so
// the existing `crate::action` / `crate::keymap` / `crate::pb_key` / `crate::slideshow`
// paths in the winit shell modules (and the `use action::…` lines below) keep
// resolving unchanged.
use pb_app_core::{
    action, contract, keymap, pb_key, slideshow, AppCore, ArchiveScope, Nav, UndoAction, Viewport,
};
// The HUD CPU compositor (info panel / toasts / pie / chip) and its Font Awesome icon
// rasterizer now live in the shell-neutral `pb-hud` crate (NS0). Re-export them at the
// crate root so the existing `crate::hud` / `crate::icon` / bare `hud::…` / `icon::…`
// paths across the winit shell modules keep resolving unchanged.
pub use pb_hud::{hud, icon};
// Persisted preferences model now lives in pb-app-core (NS0 5.5). Re-export at the crate
// root so `crate::settings` / bare `settings::…` across main.rs + dialog.rs stay unchanged.
pub use pb_app_core::settings;
// The animation model (Playback + the decode/prep state types) now lives in pb-app-core (NS0
// 5.5); re-export so `crate::animation` / bare `animation::…` stay unchanged.
pub use pb_app_core::animation;

use action::Action;
use hud::Hud;
use keymap::Keymap;
use live_audio::LiveAudio;
use menu::MenuAction;
use pb_app_core::decode_pool::{recommended_workers, DecodeFn, DecodePool};
// Engine tuning constants + pure helpers migrated to pb-app-core (NS0 5.5 / Phase B) so the
// orchestration methods that use them can live on `AppCore`. The shell still shares several.
use pb_app_core::engine::{
    file_name_of, fresh_shuffle_seed, ring_capacity, scale_mode_of, window_for_capacity,
    RING_BUDGET_BYTES,
};
// Only the order-guarantee tests decode inline now (the runtime paths go through the pool);
// gate the import so a release build doesn't see it as unused (task #18 finding #5).
#[cfg(test)]
use pb_app_core::engine::decode_item;
use pb_app_core::metrics::StageTimes;
// Playlist-resolution currency migrated to pb-app-core (NS0 5.6 Step 2): the `Resolved` snapshot
// + the `ScanUpdate` stream message. The resolver *functions* still run on the shell's scan/archive
// worker threads (which stay here); they produce these core types.
use pb_app_core::scan::{self, Resolved, ScanUpdate};
// Re-export so the shell's `ScanProgress` refs + dialog.rs's `crate::ScanProgress` stay unchanged.
pub use pb_app_core::archive;
pub use pb_app_core::scan::ScanProgress;

use pb_app_core::engine::POOL_BUDGET_BYTES;
/// Per-decode wall time *as the pool sees it* (i.e. under real concurrent load),
/// printed with the `--metrics` report. Isolated decode is fast; this shows how much
/// 8-way contention inflates it (it's how the RAW-demosaic-on-preview stall was
/// found). Only recorded under `--metrics` (the flag below), so it's zero-overhead
/// and unbounded-growth-free in normal runs.
static POOL_DECODE_MS: std::sync::Mutex<Vec<(f64, String)>> = std::sync::Mutex::new(Vec::new());
/// Whether `--metrics` is on (gates the `POOL_DECODE_MS` recording in the off-thread
/// decode closure, which has no access to the `StageTimes`).
static METRICS_ON_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How long an off-thread directory scan must run before the "Scanning Folder" progress
/// dialog appears. A normal folder resolves in well under this, so the common case never
/// flashes a dialog (and never pays for the extra window); only a genuinely large/nested
/// tree (the `~/Library` case) reveals it — with a live count, current folder, and Cancel.
const SCAN_DIALOG_DELAY: Duration = Duration::from_millis(250);

/// How often the scan card is re-rasterized at most. The live current-folder line changes per
/// directory (fast); throttling the rebuild keeps the software composite off the hot path
/// while the displayed path/count lag by at most this. Show/hide is immediate.
const SCAN_CARD_REFRESH: Duration = Duration::from_millis(120);

/// Play-hint flash timing (SwiftUI parity): fade in, hold, then fade out — unless the pointer
/// holds it open (see [`App::tick_play_hint`]).
const PLAY_HINT_FADE_IN: Duration = Duration::from_millis(200);
const PLAY_HINT_HOLD: Duration = Duration::from_secs(3);
const PLAY_HINT_FADE_OUT: Duration = Duration::from_millis(250);
/// The overlay fade (info line / tree / Inspector): quick in (appearing should
/// feel immediate), slower out (auto-hide reads as a deliberate dissolve).
const INFO_FADE_IN: Duration = Duration::from_millis(100);
const INFO_FADE_OUT: Duration = Duration::from_millis(250);
/// Delay before the toolbar's "Press F to exit" fullscreen hint appears, so it lands after the
/// borderless-fullscreen transition has settled rather than flickering over the windowed frame.
const FULLSCREEN_HINT_DELAY: Duration = Duration::from_millis(500);

/// Shell-side fade bookkeeping for one overlay layer: stamps are set on the
/// visibility **edge** (`edge`, called every turn — renders aren't guaranteed),
/// and the last content is retained so the out-ramp keeps drawing after the
/// core has hidden it (`resolve`, folded into the snapshot per render).
struct PanelFade<T> {
    last: Option<T>,
    was_visible: bool,
    shown_at: Option<Instant>,
    vanished_at: Option<Instant>,
}

impl<T: Clone> PanelFade<T> {
    fn new() -> Self {
        Self {
            last: None,
            was_visible: false,
            shown_at: None,
            vanished_at: None,
        }
    }

    /// Track a visibility edge; `true` when the overlay needs a re-render.
    fn edge(&mut self, visible: bool, now: Instant) -> bool {
        if visible == self.was_visible {
            return false;
        }
        self.was_visible = visible;
        if visible {
            self.shown_at = Some(now);
            self.vanished_at = None;
        } else {
            self.shown_at = None;
            if self.last.is_some() {
                self.vanished_at = Some(now);
            }
        }
        true
    }

    /// Still mid fade-out — keeps the overlay compositing + rendering.
    fn fading_out(&self) -> bool {
        self.vanished_at
            .is_some_and(|at| at.elapsed() < INFO_FADE_OUT)
    }

    /// Fold the fade into a snapshot slot: ramp a present value in; replay the
    /// retained one on the way out. Returns the fade for the drawn content.
    fn resolve(&mut self, slot: &mut Option<T>, now: Instant) -> f32 {
        match slot.take() {
            Some(v) => {
                let fade = self
                    .shown_at
                    .map(|s| now.duration_since(s).as_secs_f32() / INFO_FADE_IN.as_secs_f32())
                    .unwrap_or(1.0)
                    .min(1.0);
                self.last = Some(v.clone());
                *slot = Some(v);
                fade
            }
            None => {
                if let (Some(last), Some(gone)) = (self.last.as_ref(), self.vanished_at) {
                    let t = now.duration_since(gone).as_secs_f32() / INFO_FADE_OUT.as_secs_f32();
                    if t < 1.0 {
                        *slot = Some(last.clone());
                        return 1.0 - t;
                    }
                    self.last = None;
                    self.vanished_at = None;
                }
                1.0
            }
        }
    }
}

/// Whether an Escape press should quit, given an optional "ignore Esc until"
/// guard set briefly after the file picker closes (to swallow the stray Esc that
/// dismissed it). Quits when there is no guard, or it has already expired.
fn esc_quits(guard: Option<Instant>, now: Instant) -> bool {
    match guard {
        Some(until) => now >= until,
        None => true,
    }
}

/// `PB_DOOR_DIAG=1` → shell-side overlay/door diagnostics to stderr (dev-only; zero cost when
/// off). Same env as the core's `door_diag`, so the shell's overlay lines interleave with the
/// core's deck/draw lines in one capture when chasing the "door card stuck over a photo" bug.
fn door_diag() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PB_DOOR_DIAG").is_some())
}

/// Build the winit event loop. On Linux, prefer the **X11 (XWayland) backend inside
/// WSL**: WSLg's RDP→Wayland input bridge sends bogus keycodes that overflow-panic
/// winit's Wayland backend (`key + 8` on e.g. Alt+Right — unfixed upstream as of
/// winit 0.30.13), and its Wayland connection also drops on display hiccups, while
/// XWayland is stable. A real Linux desktop keeps winit's normal preference (Wayland
/// when available). `PB_BACKEND=wayland|x11` overrides in either direction.
fn build_event_loop() -> Result<EventLoop<()>, winit::error::EventLoopError> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use winit::platform::wayland::EventLoopBuilderExtWayland as _;
        use winit::platform::x11::EventLoopBuilderExtX11 as _;
        let mut builder = EventLoop::builder();
        let in_wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::fs::metadata("/proc/sys/fs/binfmt_misc/WSLInterop").is_ok();
        let x11_available = std::env::var_os("DISPLAY").is_some();
        match std::env::var("PB_BACKEND").as_deref() {
            Ok("x11") => {
                builder.with_x11();
            }
            Ok("wayland") => {
                builder.with_wayland();
            }
            _ if in_wsl && x11_available => {
                eprintln!(
                    "{}: WSL detected — using the X11 (XWayland) backend \
                     (set PB_BACKEND=wayland to override)",
                    pb_app_core::APP_NAME
                );
                builder.with_x11();
            }
            _ => {}
        }
        builder.build()
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    EventLoop::new()
}

/// The letter a `KeyCode` names (`KeyA` → `'a'`), for the menu-bar mnemonics.
#[cfg(all(unix, not(target_os = "macos")))]
fn letter_of(code: KeyCode) -> Option<char> {
    use KeyCode::*;
    const MAP: [(KeyCode, char); 26] = [
        (KeyA, 'a'),
        (KeyB, 'b'),
        (KeyC, 'c'),
        (KeyD, 'd'),
        (KeyE, 'e'),
        (KeyF, 'f'),
        (KeyG, 'g'),
        (KeyH, 'h'),
        (KeyI, 'i'),
        (KeyJ, 'j'),
        (KeyK, 'k'),
        (KeyL, 'l'),
        (KeyM, 'm'),
        (KeyN, 'n'),
        (KeyO, 'o'),
        (KeyP, 'p'),
        (KeyQ, 'q'),
        (KeyR, 'r'),
        (KeyS, 's'),
        (KeyT, 't'),
        (KeyU, 'u'),
        (KeyV, 'v'),
        (KeyW, 'w'),
        (KeyX, 'x'),
        (KeyY, 'y'),
        (KeyZ, 'z'),
    ];
    MAP.iter().find(|(k, _)| *k == code).map(|&(_, c)| c)
}

/// Collect monitor bounds as `(x, y, w, h)` physical-pixel rects in virtual-desktop
/// space — the shape [`settings::geometry_on_screen`] checks a saved window against
/// to decide if restoring it would land off-screen (#1).
fn collect_monitor_rects(
    monitors: impl Iterator<Item = winit::monitor::MonitorHandle>,
) -> Vec<(i32, i32, u32, u32)> {
    monitors
        .map(|m| {
            let p = m.position();
            let s = m.size();
            (p.x, p.y, s.width, s.height)
        })
        .collect()
}

struct App {
    /// The winit window (NS0: shell-owned — `WinitShell` in the eventual crate split).
    /// `None` until `resumed` creates it; always in lockstep with `core.renderer`, which
    /// is created on this window's surface.
    window: Option<Arc<Window>>,
    /// The platform-neutral orchestration state (NS0 step 5, ADR-021), reached as
    /// `self.core.*`. Grown incrementally off this shell; it now owns the held-key /
    /// input / self-paced-advance timing, the view + geometry transform, the metadata
    /// caches, the whole prefetch/decode/residency engine, the metrics, the nav/playlist
    /// state, the HUD overlay state + compositor (`pb-hud`), and the renderer. Only the
    /// OS window handle stays shell-owned; the `handle(CoreEvent)` dispatch follows.
    core: AppCore,

    /// Files dropped on the window this burst; winit delivers one event per file,
    /// so they're coalesced here and applied once in `about_to_wait`.
    pending_drops: Vec<PathBuf>,
    /// The native menu bar (windowed mode only). Built once, kept alive here so its
    /// native handle outlives the window. `None` until the first window is created.
    // Read only by the Windows/macOS native-menu paths; on Linux it's held but never read.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    menu: Option<muda::Menu>,
    /// The "Save Rotation" menu item, kept so its enabled state can be toggled at
    /// runtime (only enabled when the current photo has an unsaved rotation on an
    /// EXIF-writable file).
    save_rotation_item: Option<muda::MenuItem>,
    /// The File ▸ Show in Finder/Explorer menu item, enabled only when a real on-disk file
    /// is displayed (greyed out for archive entries + the empty deck; see `AppCore::can_reveal`).
    reveal_item: Option<muda::MenuItem>,
    /// The File ▸ Stop Scanning menu item, enabled only while a folder scan is streaming in.
    cancel_scan_item: Option<muda::MenuItem>,
    /// The Edit ▸ Undo menu item, kept so its title + enabled state can mirror the top of
    /// the undo stack at runtime.
    undo_item: Option<muda::MenuItem>,
    /// Image ▸ Pin for Compare (task #43): enabled with a photo shown; checked while the
    /// displayed photo IS the pin.
    compare_pin_item: Option<muda::CheckMenuItem>,
    /// Image ▸ Compare with Pinned (the `Y` flip): enabled once a pin exists.
    compare_toggle_item: Option<muda::MenuItem>,
    /// The View-menu checkable items (scale mode / recursive / fullscreen / info), kept
    /// so their checked state can mirror the live app state at runtime.
    view_checks: Option<menu::ViewChecks>,
    /// The last [`contract::MenuState`] pushed to the native menu — the single cache
    /// behind [`App::apply_menu_state`]. Every runtime menu mirror (checkmarks, Save
    /// Rotation / Stop Scanning / Undo enabled+label, the macOS native-fullscreen label,
    /// the Live Photo mute check) is diffed field-by-field against this, so the per-tick
    /// refresh only touches the OS for items that actually changed. `None` = nothing
    /// pushed yet (re-assert everything).
    menu_state: Option<contract::MenuState>,
    /// Whether the menu has been attached to the current window (`init_for_hwnd`),
    /// so fullscreen↔windowed toggles can show/hide it instead of re-initializing.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    menu_attached: bool,
    /// The Appearance mode last pushed to the OS-drawn chrome (title bar + native
    /// menu), so `apply_chrome_theme` (checked each `about_to_wait` turn) only
    /// touches the window when the preference actually changes. `None` = not yet
    /// applied.
    applied_appearance: Option<settings::AppearanceMode>,
    /// The `(accent source, custom color, resolved dark)` last pushed to `pb_ui::set_accent`, so
    /// `apply_accent` (checked each `about_to_wait` turn) only re-resolves the chrome accent when
    /// the source, the custom color, or the theme (the legibility guard is theme-dependent)
    /// actually changes. `None` = not yet applied.
    applied_accent: Option<(settings::AccentSource, [u8; 3], bool)>,
    /// Live OS-accent subscription (Windows `ColorValuesChanged`) — held so the callback stays
    /// registered; on a change it flags `accent::take_os_accent_changed`, which invalidates
    /// `applied_accent` so the next turn re-resolves. Kept alive for its `Drop`, not read.
    #[allow(dead_code)]
    accent_watcher: Option<accent::AccentWatcher>,
    /// The open egui dialog window (Settings / About), or `None`. At most one at a
    /// time; its events are routed by window id in `window_event`.
    dialog: Option<dialog::DialogWindow>,
    /// An in-flight background archive open (a `.7z` eager-decompresses off-thread so
    /// it can't freeze the event loop). `None` when no archive is loading.
    archive_load: Option<ArchiveLoad>,
    /// Monotonic id for archive-open requests; a newer open bumps it so a superseded
    /// load's result is discarded when it finally arrives.
    archive_gen: u64,
    /// An in-flight background **directory scan** (a large/recursive folder is walked
    /// off the event loop so it can't beachball — then crash — the way opening a huge
    /// tree like `~/Library` did). `None` when no scan is running.
    dir_scan: Option<DirScan>,
    /// Monotonic id for directory-scan requests; a newer open bumps it so a superseded
    /// scan's result is discarded when it finally arrives.
    scan_gen: u64,
    /// A launch input (an archive) deferred until the window exists, so the viewer
    /// appears immediately and a slow / encrypted / failed open uses the spinner +
    /// dialogs instead of blocking startup or only logging. Fired once in `resumed`.
    pending_launch: Option<open::OpenPlan>,
    /// The archive currently awaiting a password: set when the password prompt opens,
    /// re-opened with the entered password on submit, cleared on success / cancel.

    // --- Live Photo audio (the animation playback *state* moved to AppCore in NS0 5.5; this
    // ObjC `AVAudioPlayer` handle stays shell-owned, driven from `self.core.playback`) ---
    /// The Live Photo's audio (its `.mov` track), playing while the motion plays — the
    /// "cheap path" (task #38). `None` when nothing is playing / it's a silent clip / not
    /// a Live Photo. Dropped (which stops it) on pause-to-step, finish, or navigate.
    live_audio: Option<LiveAudio>,

    /// The video item's audio player + clock source (task #79 phase 5). Owned here
    /// (a platform object); driven by the `*VideoAudio` effects; sampled ~4×/s into
    /// `AppCore::video_audio_clock`. `None` = no video playing / silent clip.
    video_audio: Option<video_audio::VideoAudio>,
    /// Last time the video audio clock was sampled (throttles the bridge).
    video_audio_sampled_at: Instant,
    /// Whether the audio player has reported a non-Opening state yet — drives the
    /// adaptive sampling cadence (fast while opening, ~4 Hz after).
    video_audio_ready: bool,

    /// A dialog the orchestration layer asked the shell to open, deferred so the opener
    /// methods don't need the `ActiveEventLoop` (window creation lives in the shell's
    /// `drain_effects`, not scattered through orchestration). Only one dialog shows at a
    /// time, so it's an `Option`. Opened at the same boundary as `effects`. (NS0 dialog
    /// inversion; the shell owns `DialogWindow` outright in NS2.)
    pending_dialog: Option<DialogRequest>,
    /// The core's requested next wake-up (`CoreEffect::SetWake`), stored by `drain_effects`
    /// and min'd with the shell's own dialog-repaint deadline in `about_to_wait` for the
    /// event loop's control-flow. `None` = the core is idle.
    requested_wake: Option<Instant>,

    /// The **egui rich-panel overlay** (task #54 Phase 4): Help, the Inspector, and the
    /// folder tree drawn as a real retained-mode UI over the photo, sharing the main
    /// renderer's device/queue. `None` until `resumed` creates the renderer. The winit
    /// shell sets the `native_*` panel flags so the core suppresses the CPU-HUD versions
    /// of these panels and drives this instead.
    egui_overlay: Option<egui_overlay::EguiOverlay>,
    /// The toolbar's last-drawn state (task #61), diffed each turn so the overlay re-renders
    /// only when a button's state actually changes (the counter/play state change on nav).
    last_toolbar_state: Option<toolbar::ToolbarState>,
    /// When the egui overlay was last (re)rendered, used to floor animation-driven repaints to
    /// the refresh interval. egui asks for ASAP repaints while a control is hovered (tooltip)
    /// or a nav button is held; an unthrottled render→request_redraw loop would spin the event
    /// loop far above vsync (audible GPU coil-whine). See `update_overlay`.
    last_overlay_render: Option<Instant>,
    /// The single toolbar nav/random button currently pressed-and-held (hold-to-blaze). The core
    /// tracks one `pointer_nav`; this mirrors which button owns it so press/release edges are
    /// detected across frames. Cleared on release; the core also has a focus-loss safety net.
    toolbar_nav_held: Option<pb_app_core::Action>,
    /// A toast queued to appear at a later instant (message + fire time). Used for the toolbar's
    /// "Press F to exit" fullscreen hint: shown a beat *after* the click so it lands once the
    /// borderless-fullscreen transition has settled, not flickering over the windowed frame first.
    pending_toast: Option<(Instant, String)>,
    /// The last snapshotted info line + its appear/vanish stamps, driving the
    /// fade (the line keeps drawing briefly after the core hides it). Stamps are
    /// set by `update_overlay` on the visibility EDGE (it runs every turn;
    /// renders don't), never inside the render itself.
    last_info_line: Option<panels_ui::InfoLine>,
    info_line_was_visible: bool,
    info_shown_at: Option<Instant>,
    info_vanished_at: Option<Instant>,
    /// Fade ramps for the folder tree + Inspector (same 100 ms in / 250 ms out).
    tree_fade: PanelFade<panels_ui::TreeFrame>,
    inspector_fade: PanelFade<panels_ui::InspectorFrame>,
    /// The Thumbnails strip's shell-side state (task #83): the egui texture cache + scroll
    /// bookkeeping. RAM-only, dropped on exit; the thumb *pixels* live in the core store.
    thumb_strip: panels_ui::ThumbStripState,
    /// The user-resizable left-pane width, shared by the Folders and Thumbnails tabs (task #83).
    /// Session-only (not persisted — a pane width isn't a viewing trace, but there's no settings
    /// field for it yet); starts at the default and is updated by the pane's drag handle.
    tree_width: f32,
    /// The user-resizable Inspector width (task #83), same session-only story as `tree_width`.
    inspector_width: f32,
    /// Whether the pointer is over an egui panel/area, from the **live** pointer position
    /// hit-tested against the last frame's layout (not egui's stored, one-frame-late pointer).
    /// The shell is the single cursor writer ([`resolve_cursor`](Self::resolve_cursor)): over a
    /// panel it shows egui's hover cursor, over the photo the core's, and — lag-free — the
    /// resize arrow inside a reported handle zone. Updated on every `CursorMoved`.
    pointer_over_panel: bool,
    /// The last pointer position in **physical** window pixels (for the cursor resolve's
    /// geometric resize-zone hit-test). `None` until the first move / after the pointer leaves.
    last_pointer: Option<(f64, f64)>,
    /// The core's most-recent desired photo cursor (from `SetCursor`). Stored, not applied
    /// directly — `resolve_cursor` composes it with egui's want each frame so the two never
    /// fight over the window cursor (the resize-handle flicker).
    core_cursor: contract::CursorKind,
    /// The window cursor the shell last applied — so `resolve_cursor` only calls `set_cursor`
    /// on a real change (idempotent, no per-move thrash).
    applied_cursor: Option<CursorIcon>,
    /// The left pane's resize-handle strip rect in egui **points** `[x0, y0, x1, y1]`, reported
    /// by the panel each render. `resolve_cursor` hit-tests the live pointer against it to show
    /// the resize arrow the instant the pointer crosses in — no dependency on egui's laggy
    /// per-frame hover cursor. Gated on the left pane being open.
    left_pane_edge: Option<[f32; 4]>,
    /// The Inspector's resize-handle strip rect (egui points), same as `left_pane_edge`.
    inspector_edge: Option<[f32; 4]>,
    /// Whether the egui panel texture needs re-rendering next turn — set on a panel
    /// state/content change (`CoreEffect::PanelsChanged`), an egui-consumed event, or a
    /// timed egui repaint. When clear and a panel is open, the retained texture is reused
    /// (a nav frame costs no egui work — the hot-path contract).
    overlay_dirty: bool,
    /// Whether an egui panel texture is currently handed to the renderer (so the compositor
    /// draws it). Tracks the last `set_egui_overlay(Some/None)` so we only re-hand on a
    /// visibility edge, not every frame.
    overlay_active: bool,
    /// Keyboard/pointer state of the Linux egui menu bar (which dropdown is open, which
    /// row is selected, Alt-mnemonic hint). Shell-owned so the key handler can drive it
    /// GTK-style (Alt+F / F10 / arrows / Enter / Esc) — see `menu::menu_nav_key`.
    #[cfg(all(unix, not(target_os = "macos")))]
    menu_nav: menu::MenuNav,
    /// The presented archive door's item, or `None` — the signature that dirties the retained
    /// overlay when the door changes (task #105). `Some(None)` can't occur: `door_presented`
    /// implies a `displayed_item`.
    door_sig: Option<Option<usize>>,
    /// The last `play_hint_seq` the shell flashed, so a bump (a fresh motion item) re-arms the
    /// hint. Play-hint fade timing is shell-owned (the `native_play` seam): the core signals
    /// *when* + *what*, the shell renders + fades the egui pill.
    play_hint_seq: u64,
    /// When the play hint's flash began (its fade-in / hold clock), or `None` when not shown.
    play_hint_shown: Option<Instant>,
    /// When the play hint began fading out, or `None` while it's fading in / holding.
    play_hint_fade_out: Option<Instant>,
    /// The motion kind (1 = Live Photo, 2 = animation) the hint is showing — kept so the icon
    /// stays put while it fades out after the item stops being a motion item (`kind` → 0).
    play_hint_kind: u8,
    /// Whether the pointer is over the play hint (pins its auto-fade open).
    play_hint_hovered: bool,
    /// This tick's computed play-hint pill (kind + fade alpha), or `None` when hidden — set by
    /// [`App::tick_play_hint`] each turn and read by `render_overlay_frame`.
    play_hint_frame: Option<panels_ui::PlayHintFrame>,
    /// The next wake the play-hint animation needs (fade-in/out frame, or the hold-expiry that
    /// starts the fade-out), folded into the event loop's wake so the animation self-drives —
    /// never relying on redraw self-pump, which stalls on Linux (the pill would otherwise linger
    /// until the next input event). `None` when static (hover-pinned) or hidden.
    play_hint_wake: Option<Instant>,
    /// The subtitle generation this shell has already handed to the renderer (task #90.5).
    /// A cue lives for seconds, so this skips the re-upload on all but a few frames — the
    /// `thumb_gen` contract, same as the macOS host's `NSImage` cache.
    subtitle_gen: u64,
    /// The Playback ▸ Subtitle Track flyout (task #99), kept so its **rows** can be rebuilt
    /// when the file on screen changes — every other menu handle only needs its checkmark
    /// mirrored.
    subtitle_tracks_menu: Option<muda::Submenu>,
    /// What the flyout was last built for. muda has no `menuNeedsUpdate` (macOS's route), so
    /// the rows are rebuilt on change instead of pulled at open — and this is what makes
    /// "on change" cheap: the *inputs* the list depends on, `Copy` and allocation-free, so
    /// the per-tick check is a tuple compare. Reading the real rows every tick would allocate
    /// a `Vec<String>` behind a playing video, which is exactly the hot path.
    subtitle_menu_sig: Option<SubtitleMenuSig>,
    /// The Playback ▸ Audio Track flyout (task #99) — same rebuilt-rows shape as
    /// `subtitle_tracks_menu`, same reasons.
    audio_tracks_menu: Option<muda::Submenu>,
    audio_menu_sig: Option<AudioMenuSig>,
    /// An in-flight audio track switch: (engine sequence, requested picker row). The
    /// engine runs on its own thread and confirms asynchronously; the sample tick polls
    /// the outcome and reports it to the core, which toasts **only on a confirmed
    /// switch** (#99 — audio may fail while the old track plays on, and a toast naming
    /// a track over unchanged audio teaches the user to distrust every toast).
    pending_audio_switch: Option<(u64, usize)>,
    /// The active-audio picker row last seen (`-1` = none) — **trace-only** change
    /// detection (`PB_AUDIO_TRACE`). The report itself is NOT deduped on this: the
    /// core's stored id is generation-scoped and a re-probe invalidates it without
    /// the row number moving, so a dedupe froze the tick out (owner-hit 2026-07-17).
    audio_row_reported: i64,
}

/// The inputs the Playback ▸ Subtitle Track flyout's rows depend on. When this is unchanged,
/// the rows cannot have changed, so the rebuild is skipped (see `App::subtitle_menu_sig`).
///
/// `tracks_known` is the one that flips `false → true` when a probe lands, turning "Reading
/// Tracks…" into the real list; `active` covers a `Shift+C` cycle moving the tick.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubtitleMenuSig {
    item: Option<usize>,
    video_showing: bool,
    tracks_known: bool,
    active: Option<u64>,
    on: bool,
}

/// The inputs the Playback ▸ Audio Track flyout's rows depend on — the audio twin of
/// [`SubtitleMenuSig`], minus `on` (audio has no off toggle: that's Mute). Keyed on the
/// displayed item *being a video*, not on a live session: the track list is a fact about
/// the file, and it must show over the poster too (owner, 2026-07-17).
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioMenuSig {
    item: Option<usize>,
    video_item: bool,
    tracks_known: bool,
    /// The active track's **(catalog generation, local_id)** — the shell-reported
    /// tick. The generation matters: a re-probe mints new ids with the same
    /// local_id, and a sig keyed on local_id alone missed the stale→fresh flip,
    /// leaving the flyout drawn from the stale (tickless) state.
    active: Option<(u64, u64)>,
}

/// A deferred dialog open (see [`App::pending_dialog`]). Carries only what the opener
/// computed; `settings`/`keymap`/the parent window are read from `self` when the shell
/// actually opens it, so no borrows are captured here.
enum DialogRequest {
    /// A plain themed dialog — About / Settings / Confirm / Message / Password — with a
    /// body message (empty for About/Settings).
    Simple {
        kind: dialog::DialogKind,
        message: String,
        /// Show the "Don't show archives" opt-out checkbox (task #104) — only ever set on
        /// the empty-archive Message dialog, so pressing `P` on an archive with no images
        /// offers a one-click way to stop listing archives. `false` for every other dialog.
        archive_optout: bool,
    },
    /// The archive "Opening…" determinate-progress dialog (a progress handle + redraw are
    /// wired after it opens).
    Loading {
        message: String,
        progress: pb_source::OpenProgress,
    },
    /// The folder "Scanning…" determinate-progress dialog.
    Scanning {
        message: String,
        progress: ScanProgress,
    },
}

/// What the user did in the dialog window — the shell's raw extraction from egui. The shell
/// ([`App::dialog_event`]) drives egui + pulls any payload (a password, the edited
/// settings/keymap), then [`App::route_dialog_outcome`] dispatches it (NS0 5.6): every case maps
/// to a [`contract::DialogResult`] and is handed to the core as a
/// [`CoreEvent::DialogResolved`] — the core runs the reaction and emits the `CloseDialog` /
/// `CancelScan` / `CancelArchiveLoad` / `BeginArchiveOpen` (password retry) effects. (The
/// Loading/Scanning progress *dialogs* themselves — `become_loading`, `set_scan` — remain
/// shell-owned.)
enum DialogOutcome {
    /// Esc / close button dismissed a dialog of this kind (cancels the matching in-flight op).
    Dismissed(Option<dialog::DialogKind>),
    /// Password entry submitted (archive unlock); `None` if extraction failed. A
    /// [`SecretString`](pb_app_core::SecretString) — zeroized, redacted (session-archive-password-cache).
    PasswordSubmitted(Option<pb_app_core::SecretString>),
    /// The password prompt's Cancel — abandon the pending archive.
    PasswordCancelled,
    /// The "Ask about image" question submitted (task #44). `None` shouldn't happen on Ask
    /// but folds to an empty question the core ignores.
    AskSubmitted(Option<String>),
    /// A live Settings edit from the auto-saving dialog, carrying the (optionally) edited
    /// settings + keymap for the frame. Applied + persisted immediately; the window stays
    /// open. `settings` is boxed to keep this variant small (the struct grew with the
    /// AI-describe fields — else `clippy::large_enum_variant`).
    SettingsEdited {
        settings: Option<Box<settings::Settings>>,
        keymap: Option<Keymap>,
    },
    /// Settings dialog closed (Done). Edits were already applied live, so this only clears
    /// the dialog-open state. (Esc / close go through [`DialogOutcome::Dismissed`].)
    SettingsCancelled,
    /// The archive "Opening…" dialog's Cancel.
    LoadingCancelled,
    /// The folder "Scanning…" dialog's Cancel.
    ScanningCancelled,
    /// A Confirm dialog answered (`true` = the destructive action was confirmed).
    ConfirmAnswered(bool),
    /// A Message (or any other) dialog's OK / close.
    Closed,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn new(
        windowed: bool,
        root: PathBuf,
        source: Arc<dyn ItemSource>,
        start: usize,
        recursive: bool,
        scan_root: Option<PathBuf>,
        metrics: StageTimes,
        settings: settings::Settings,
        overrides: &pb_app_core::LaunchOverrides,
    ) -> Self {
        let playlist = Playlist::new(source.len(), fresh_shuffle_seed()).with_cursor(start);
        let decode: Arc<DecodeFn> = Arc::new(
            |src: &dyn ItemSource,
             item,
             fit,
             allow_preview,
             purpose,
             cancel: &std::sync::atomic::AtomicBool| {
                if !METRICS_ON_FLAG.load(std::sync::atomic::Ordering::Relaxed) {
                    return pb_app_core::engine::decode_item_for(
                        src,
                        item,
                        fit,
                        allow_preview,
                        purpose,
                        cancel,
                    );
                }
                let t0 = Instant::now();
                let r = pb_app_core::engine::decode_item_for(
                    src,
                    item,
                    fit,
                    allow_preview,
                    purpose,
                    cancel,
                );
                let ms = t0.elapsed().as_secs_f64() * 1e3;
                let tag = format!(
                    "{}{}",
                    if allow_preview { "prev " } else { "full " },
                    src.name(item)
                );
                POOL_DECODE_MS.lock().unwrap().push((ms, tag));
                r
            },
        );
        let (pool, results) = DecodePool::new(recommended_workers(), POOL_BUDGET_BYTES, decode);
        // The macOS archive-video handoff channels (the shell side is the SwiftUI host;
        // this winit shell never sends on them, but the fields are unconditional so every
        // constructor wires a live pair — mirrors `AppCore::headless`).
        let (poster_read_tx, poster_read_rx) = std::sync::mpsc::channel();
        let (video_read_tx, video_read_rx) = std::sync::mpsc::channel();
        // `settings` (pristine, loaded by `main`) drives the launch defaults; the hold loop reads
        // it live. CLI overrides apply to live state via `apply_launch_overrides` below — never to
        // `settings`, so a later save can't leak them to disk (privacy #2).
        let mut core = AppCore {
            now: Instant::now(),
            viewport: Viewport {
                width: 1,
                height: 1,
                scale_factor: 1.0,
            },
            held: HashMap::new(),
            // Pointer-driven hold-to-blaze (toolbar nav/random press-and-hold); the winit
            // shell has no such toolbar, so it starts idle like the other constructors.
            pointer_nav: None,
            last_present: None,
            frame_interval: Duration::from_micros(8_333),
            hold_start: None,
            initial_delay: Duration::from_millis(settings.hold_delay_ms as u64),
            slideshow: slideshow::Slideshow {
                interval: Duration::from_secs_f64(settings.slideshow_interval_secs),
                ..slideshow::Slideshow::default()
            },
            mods: contract::Modifiers::NONE,
            esc_guard_until: None,
            persist_prefs: true, // a live shell persists the remembered last_folder
            os_dark: true,       // dark until `resumed` reads the real window theme (#46)
            hud_dark: true,      // matches the Hud's default Theme::DARK
            fit: None,
            // Start in the user's default scale mode (8/9/0 still switch it live).
            view: ViewTransform {
                mode: scale_mode_of(settings.scale_mode),
                ..ViewTransform::default()
            },
            last_cursor: None,
            dragging: false,
            rotations: HashMap::new(),
            video_resume: HashMap::new(),
            zoom_started: None,
            zoom_last: None,
            pan_started: None,
            pan_last: None,
            resize_settle_at: None,
            geometry_save_at: None,
            windowed,
            // Top strip reserved by an in-client menu bar (the Linux egui bar); 0 until the
            // shell reports one via `set_content_top_inset`. No menu inset on Windows/macOS.
            content_top_inset: 0,
            meta_cache: HashMap::new(),
            current: None,
            exif_cache: HashMap::new(),
            details_probe: None,
            details_gen: 0,
            catalog_seq: 0,
            // Audio track selection (task #99) is macOS-only so far — this shell has no
            // player to ask what it is playing, and the tick is reported, never derived.
            audio_active: None,
            // Subtitles (task #90): start from the user's persisted preference, never from
            // `Default`. This is post-mortem bug #2 — `subtitles = true` on disk launching
            // with captions off, so the preference looks like it never saved. It was latent
            // here only because this shell had no presenter to reveal it; `from_settings`
            // takes the whole `Settings` precisely so the style can't be forgotten either.
            subtitles: pb_app_core::subtitle_engine::SubtitleEngine::from_settings(&settings),
            recognized_text: HashMap::new(),
            text_scan: None,
            text_gen: 0,
            descriptions: HashMap::new(),
            describe_scan: None,
            describe_gen: 0,
            pool,
            results,
            ring: ResidentRing::new(0),
            ahead: 8,
            behind: 2,
            failed: HashSet::new(),
            deleted: HashSet::new(),
            preview_resident: HashSet::new(),
            pending_uploads: Vec::new(),
            upgrade_done: HashSet::new(),
            last_upgrade_set: Vec::new(),
            full_requested_at: HashMap::new(),
            preview_watchdog: None,
            live_motion_cache: HashMap::new(),
            metrics,
            // Latency timers for "feeling fast" (open→first, open→cached, resize); gated by
            // PB_PERF, live to stderr. Mirrors `AppCore::headless`.
            perf: pb_app_core::perf::Perf::new(pb_app_core::perf::env_enabled()),
            // A launch straight onto an archive (the resolve_playlist
            // safety-net path) starts unscoped, like apply_archive stamps.
            archive_scope: source.container().is_some().then(|| ArchiveScope {
                full: Arc::clone(&source),
                prefix: String::new(),
            }),
            source,
            playlist,
            targets: Vec::new(),
            last_nav: Nav::Forward,
            displayed_item: None,
            presented_epoch: None,
            target_item: None,
            compare_pin: None,
            compare_return: None,
            compare_pin_id: None,
            compare_carry: None,
            epoch: 1,
            content_gen: 1,
            root,
            scan_root,
            recursive,
            scanning: false,
            launching: false,
            dialog_open: false,
            archive_loading: false,
            redraw_pending: false,
            resize_hold: None,
            scan_bootstrapped: false,
            password_archive: None,
            archive_passwords: Vec::new(),
            pending_delete: None,
            pending_confirm_delete: None,
            info_line: false,
            info_line_shown: false,
            info_line_item: None,
            info_line_w: 0,
            info_line_h: 0,
            panels: pb_app_core::Panels::default(),
            // The rich panels (Help / Inspector / folder tree) are presented by the
            // egui overlay (task #54 Phase 4), so the core suppresses their CPU-HUD
            // rasterization and emits `PanelsChanged` instead. The ephemeral layer
            // (toasts, `i` line, play hint, empty-state) stays a CPU quad for now.
            native_help: true,
            last_help_visible: false,
            // The egui overlay draws the welcome / empty-state buttons (task #54 Phase 4) —
            // suppress the CPU-HUD open panel and let the shell render + hit-test them.
            native_open: true,
            last_open_visible: false,
            native_inspector: true,
            last_inspector_snap: None,
            // The egui overlay draws the Thumbnails strip (task #83 phase 7) — Shift+T
            // opens the left pane's second tab; the shell renders + hit-tests the cells.
            native_thumbs: true,
            native_tree: true,
            last_tree_visible: false,
            overlay_shown: false,
            overlay_item: None,
            toast: None,
            native_toast: false,
            // The egui overlay draws the info readout (task #54 Phase 4) — suppress the CPU
            // HUD line and let the shell render + duck it around the panels.
            native_info: true,
            last_info_snap: None,
            // The egui overlay draws the play hint (task #54 Phase 4) — the core only
            // flash-signals it (bumps `play_hint_seq`); the shell renders + fades the pill.
            native_play: true,
            play_hint_seq: 0,
            toast_native: None,
            toast_seq: 0,
            wait_started: None,
            pie_finish: None,
            pie_glow_started: None,
            decode_ewma: 0.25,
            pie_drawn: false,
            pie_pushed: None,
            chip_sig: None,
            chip_built: Instant::now(),
            folder_tree_open: false,
            left_tab: Default::default(),
            thumbs: Default::default(),
            folder_tree_sig: None,
            folder_tree_panel: None,
            folder_tree_counts: None,
            tree_io: None,
            fs_tree: None,
            fs_tree_io: None,
            climb_anchor: None,
            hud: Hud::load(),
            renderer: None,
            undo_stack: Vec::new(),
            playback: None,
            anim_frame_shown_at: None,
            anim_decode: None,
            anim_stream: None,
            video: None,
            video_seq: 0,
            // The macOS FFmpeg dual-backend fallback session (inert on the winit shell).
            video_ffmpeg_fallback: None,
            // The macOS sample-buffer opt-in (inert on the winit shell).
            sample_buffer_opt_in: false,
            dovi_warned: std::collections::HashSet::new(),
            video_diag_last: None,
            // macOS-only archive-video handoff state (inert on the winit shell — see the
            // channel note above): pending bytes for the shell to pull, poster-request
            // bookkeeping, and the off-thread read channels. (`content_top_inset` is set with
            // the windowed group above.)
            pending_video_bytes: None,
            pending_poster_bytes: std::collections::HashMap::new(),
            poster_req_seq: 0,
            poster_inflight: std::collections::HashSet::new(),
            poster_read_tx,
            poster_read_rx,
            video_read_tx,
            video_read_rx,
            video_seek_last: None,
            pending_delete_retry: None,
            video_pill_text: None,
            video_osd_until: None,
            video_geometry_stale: false,
            video_paused_by_resize: false,
            prepared: None,
            anim_gen: 0,
            anim_hint_shown_for: None,
            framestep_started: None,
            framestep_last: None,
            live_revert_at: None,
            keymap: Keymap::load(),
            settings,
            launch: pb_app_core::LaunchOverrides::default(),
            effects: Vec::new(),
        };
        // Session-only CLI launch overrides → live state (never persisted); see the method.
        core.apply_launch_overrides(overrides);
        Self {
            window: None,
            core,
            pending_drops: Vec::new(),
            applied_appearance: None,
            applied_accent: None,
            accent_watcher: None,
            menu: None,
            save_rotation_item: None,
            reveal_item: None,
            compare_pin_item: None,
            compare_toggle_item: None,
            cancel_scan_item: None,
            undo_item: None,
            view_checks: None,
            menu_state: None,
            menu_attached: false,
            dialog: None,
            archive_load: None,
            archive_gen: 0,
            dir_scan: None,
            scan_gen: 0,
            pending_launch: None,
            live_audio: None,
            video_audio: None,
            video_audio_sampled_at: Instant::now(),
            video_audio_ready: false,
            pending_dialog: None,
            requested_wake: None,
            egui_overlay: None,
            last_toolbar_state: None,
            last_overlay_render: None,
            toolbar_nav_held: None,
            pending_toast: None,
            last_info_line: None,
            info_line_was_visible: false,
            info_shown_at: None,
            info_vanished_at: None,
            tree_fade: PanelFade::new(),
            inspector_fade: PanelFade::new(),
            thumb_strip: panels_ui::ThumbStripState::default(),
            tree_width: 280.0,
            inspector_width: 360.0,
            pointer_over_panel: false,
            last_pointer: None,
            core_cursor: contract::CursorKind::Default,
            applied_cursor: None,
            left_pane_edge: None,
            inspector_edge: None,
            overlay_dirty: false,
            door_sig: None,
            overlay_active: false,
            #[cfg(all(unix, not(target_os = "macos")))]
            menu_nav: menu::MenuNav::default(),
            play_hint_seq: 0,
            play_hint_shown: None,
            play_hint_fade_out: None,
            play_hint_kind: 0,
            play_hint_hovered: false,
            play_hint_frame: None,
            play_hint_wake: None,
            subtitle_gen: 0,
            subtitle_tracks_menu: None,
            subtitle_menu_sig: None,
            audio_tracks_menu: None,
            audio_menu_sig: None,
            pending_audio_switch: None,
            audio_row_reported: -1,
        }
    }

    /// Confirm, then permanently delete the displayed photo (`Shift+Del`). Irreversible, so it
    /// opens the themed confirm dialog first (dark-aware, cross-platform); the delete runs on Yes
    /// via `DialogResolved`(ConfirmAnswered) → the core `do_delete(.., true)`. Only real files (not archive
    /// entries) can be deleted. The recoverable `Del` path is a pure core arm
    /// ([`AppCore::delete_to_trash`]).
    fn confirm_delete_permanent(&mut self) {
        // Settle any still-pending delete-advance first (e.g. a rapid second Del).
        self.core.flush_pending_delete();
        let Some(item) = self.core.displayed_item else {
            return;
        };
        if self.core.source.path(item).is_none() {
            self.core.show_toast("Can't delete this"); // archive entry — no file
            return;
        }
        let name = file_name_of(self.core.source.name(item));
        self.core.pending_confirm_delete = Some(item);
        self.open_confirm_delete(&name);
    }

    /// Defer a launch **plan** until the window + engine exist (`resumed` fires it).
    /// Used for an archive *and* a folder scan on the command line / double-click so startup
    /// shows the window first and the open runs behind the spinner / dialog / streaming scan
    /// (a synchronous launch resolve, before the event loop, blocked the window on a big
    /// tree). The plan — not the raw input — is deferred so the startup recursive override is
    /// preserved (re-planning in `resumed` would drop it).
    fn queue_launch(&mut self, plan: open::OpenPlan) {
        self.pending_launch = Some(plan);
        self.core.launching = true; // core mirror: suppress the open hint until it resolves
    }

    /// Open a launch input at runtime (the file picker or a drag-drop): plan it,
    /// build the playlist, and jump to the plan's cursor (the dropped/clicked
    /// photo, or the first of a folder). Empty selections are ignored so the
    /// current photo isn't blanked.
    fn open_input(&mut self, input: LaunchInput) {
        let plan = open::plan(input);
        self.core.open_plan(plan.source, plan.cursor);
    }

    /// Start opening an archive at runtime (picker / drag-drop / a deferred launch).
    /// A `.zip` opens synchronously (just a directory read). Every other kind —
    /// 7z and the whole tar family — opens on a background thread
    /// ([`ArchiveKind::background_open`]; even a lazy plain tar's index walk is
    /// O(entries) of file I/O): the current photo stays visible and the loop stays
    /// responsive until the open lands (picked up in
    /// [`poll_archive_load`](App::poll_archive_load)). The per-kind dispatch —
    /// including the 7z RAM pre-flight — is `scan::load_archive`, shared with the
    /// mac shell. A second open supersedes the first via `archive_gen`.
    ///
    /// `password` decrypts an encrypted archive: `None` on the first open (an
    /// encrypted archive then reports `PasswordRequired`, which prompts), `Some` when
    /// re-opening with a password the user entered.
    fn begin_archive_open(&mut self, path: PathBuf, password: Option<pb_app_core::SecretString>) {
        let kind = pb_source::archive_kind(&path).unwrap_or(pb_source::ArchiveKind::Zip);
        // Auto-try cached session passwords (MRU-first) only on an INITIAL open — a user-entered
        // retry tries exactly what they typed (session-archive-password-cache).
        let cached = if password.is_none() {
            self.core.archive_passwords_snapshot()
        } else {
            Vec::new()
        };
        // The user-entered password (if any), kept for the harvest-on-success and the
        // wrong-password re-prompt (a repeat `PasswordRequired` with `Some` here was wrong).
        let attempted_password = password.clone();
        // Anti-stacking: cancel any open already in flight before starting another, so
        // two eager decompresses never run (and pile up RAM) at once — the original
        // hang's worst case was the user re-triggering a "never-finishing" open and
        // stacking full-archive workers. The superseded worker stops at its next entry
        // boundary and frees its partial buffers. `take()` (not just cancel) drops the
        // handle + rx now: a result the old worker already sent — its generation still
        // matches — must never be received after this newer open. The zip-sync path
        // below has no gen bump of its own to protect it.
        if let Some(prev) = self.archive_load.take() {
            prev.progress.request_cancel();
        }
        // Cross-type supersession (cross-deck open race, Codex-diagnosed 2026-07-17): a folder
        // scan is a DIFFERENT worker than an archive open, so the `archive_gen` bump above never
        // cancels it. Left alive, its next cumulative batch reaches the core and extends *this*
        // archive deck (`apply_scan_batch` → `extend_playlist`) while both GPU rings still hold
        // the archive's textures — the "title advances, view frozen, door card over a photo"
        // corruption. Drop the scan handle now so no stale folder batch survives this open.
        // (The core also guards the extend, so this is belt-and-braces + stops the worker sooner.)
        self.cancel_dir_scan();
        self.dir_scan = None;
        // The synchronous ZIP shortcut is only safe when NO auto-try will run: a wrong-password
        // ZIP attempt decrypts the entire first entry (up to ~1 GiB via `ZipSource::password_ok`),
        // so any auto-try must go off the event loop. With an empty cache and no user password
        // (a fresh session, every non-encrypted-archive session) this stays today's fast path.
        let will_autotry = password.is_none() && !cached.is_empty();
        if !kind.background_open() && !will_autotry {
            let pw = password.as_ref().map(|p| p.expose().to_owned());
            let result = scan::open_archive(&path, pw);
            self.finish_archive_open((result, None), attempted_password, path);
            return;
        }
        self.archive_gen += 1;
        let generation = self.archive_gen;
        let progress = pb_source::OpenProgress::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        let worker_progress = progress.clone();
        std::thread::spawn(move || {
            // A user-entered password tries exactly that; an initial open auto-tries the cached
            // session passwords (and reports the winner for MRU promotion).
            let out = match password {
                Some(pw) => (
                    scan::load_archive(
                        &worker_path,
                        kind,
                        Some(pw.expose().to_owned()),
                        &worker_progress,
                    ),
                    None,
                ),
                None => {
                    scan::load_archive_with_cache(&worker_path, kind, &cached, &worker_progress)
                }
            };
            let _ = tx.send((generation, out));
        });
        // Show the determinate progress + Cancel dialog. If the password prompt is still
        // open (a just-verified password), promote it in place — same window, no flicker;
        // otherwise (drag-drop / picker / launch) open a fresh loading dialog.
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("archive");
        let msg = format!("Opening \u{201c}{name}\u{201d}\u{2026}");
        if self.dialog.as_ref().map(|d| d.kind()) == Some(dialog::DialogKind::Password) {
            if let Some(d) = self.dialog.as_mut() {
                d.become_loading(&msg, progress.clone());
            }
        } else {
            self.pending_dialog = Some(DialogRequest::Loading {
                message: msg,
                progress: progress.clone(),
            });
        }
        self.archive_load = Some(ArchiveLoad {
            generation,
            rx,
            path,
            attempted_password,
            progress,
        });
    }

    /// Pick up a finished background archive open (called each tick while one is in
    /// flight). Routes the result through [`finish_archive_open`](App::finish_archive_open),
    /// and drops a superseded result (a newer open bumped `archive_gen`).
    fn poll_archive_load(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let (load_gen, recv) = match self.archive_load.as_ref() {
            Some(load) => (load.generation, load.rx.try_recv()),
            None => return,
        };
        match recv {
            Ok((generation, result)) => {
                let load = self.archive_load.take();
                if generation != load_gen {
                    return; // superseded by a newer open
                }
                let (path, attempted) = match load {
                    Some(l) => (l.path, l.attempted_password),
                    None => return,
                };
                self.finish_archive_open(result, attempted, path);
            }
            Err(TryRecvError::Empty) => {} // still loading
            Err(TryRecvError::Disconnected) => {
                // Worker died without a result (e.g. a panic inside the decompressor).
                // Drop the handle and don't strand its progress dialog — the Loading
                // spinner would otherwise stay up forever (mirrors poll_dir_scan).
                self.archive_load = None;
                if self.dialog.as_ref().map(|d| d.kind()) == Some(dialog::DialogKind::Loading) {
                    self.dialog = None;
                }
            }
        }
    }

    /// Act on a finished archive open (zip-sync or 7z-async), shared by both paths:
    /// a non-empty success rebuilds the playlist (closing any password prompt); a
    /// `PasswordRequired` opens (or re-prompts, after a wrong attempt) the password
    /// dialog; any other failure shows the error dialog.
    ///
    /// `result` is `(open outcome, auto-try winner)` — the cached password that unlocked it,
    /// if any. `attempted` is the user-entered password this open carried. On any success
    /// (even an *empty* archive — the password was still correct), the unlocking password is
    /// remembered: harvest for a new user entry, MRU promotion for a cached winner
    /// (session-archive-password-cache). A repeat `PasswordRequired` with `attempted.is_some()`
    /// was a wrong entry, so the prompt shows the retry error.
    fn finish_archive_open(
        &mut self,
        result: (
            Result<Resolved, archive::ArchiveOpenError>,
            Option<pb_app_core::SecretString>,
        ),
        attempted: Option<pb_app_core::SecretString>,
        path: PathBuf,
    ) {
        let (result, winner) = result;
        // Remember the unlocking password (harvest a user entry / MRU-promote a cached winner)
        // whenever the archive actually OPENED — `Ok` (with images) or `Empty` (opened, no
        // images): a correct password is worth reusing even if this archive had nothing to show.
        // A wrong password is `PasswordRequired` (no winner; `attempted` drives the re-prompt),
        // so this never remembers a wrong one.
        let opened = matches!(result, Ok(_) | Err(archive::ArchiveOpenError::Empty));
        if opened {
            if let Some(pw) = attempted.as_ref().or(winner.as_ref()) {
                self.core.remember_archive_password(pw);
            }
        }
        match result {
            Ok(r) if !r.source.is_empty() => {
                // Close the loading/password dialog (host-side, like the scan's Done), then
                // hand the resolved playlist to the core to install + forget the pending pw.
                self.close_dialog();
                self.core.handle(contract::CoreEvent::ArchiveResolved(r));
            }
            Ok(_) | Err(archive::ArchiveOpenError::Empty) => {
                self.fail_archive_open(&archive::ArchiveOpenError::Empty)
            }
            Err(archive::ArchiveOpenError::PasswordRequired) => {
                self.prompt_archive_password(path, attempted.is_some())
            }
            // User cancelled: drop quietly, keeping whatever was on screen — no error
            // dialog. The loading dialog is already closed (or closes here as a backstop).
            Err(archive::ArchiveOpenError::Cancelled) => {
                self.core.password_archive = None;
                self.close_dialog();
            }
            Err(e) => self.fail_archive_open(&e),
        }
    }

    /// Ask the in-flight archive open (if any) to stop. The worker returns
    /// [`Cancelled`](archive::ArchiveOpenError::Cancelled) at its next entry boundary,
    /// freeing its partial buffers; [`poll_archive_load`](App::poll_archive_load) then
    /// drops it quietly. Used by the loading dialog's Cancel button and the Esc/close path.
    fn cancel_archive_load(&mut self) {
        if let Some(load) = self.archive_load.as_ref() {
            load.progress.request_cancel();
        }
    }

    /// Start scanning a folder source off the event loop. Walking a large or deeply
    /// nested tree (the worst case: someone opens `~/Library`) can take many seconds;
    /// doing it synchronously froze the run loop (beachball) and could then get the
    /// unresponsive app killed. So the walk runs on a worker thread and the resolved
    /// playlist is picked up in [`poll_dir_scan`](App::poll_dir_scan) — the current view
    /// stays until it lands. A second open supersedes the first via `scan_gen` + the
    /// shared cancel flag, so a giant in-flight scan is abandoned rather than left to
    /// finish. Mirrors the async archive path ([`begin_archive_open`](App::begin_archive_open)).
    fn begin_dir_scan(&mut self, source: Source, cursor: open::Cursor) {
        // Abandon any scan already running — its result would be stale, and it may be a
        // huge walk we don't want competing for I/O with the new one.
        self.cancel_dir_scan();
        self.core.deleted.clear(); // fresh scan → fresh universe, no stale tombstones
        self.scan_gen += 1;
        let generation = self.scan_gen;
        let progress = ScanProgress::new();
        let name = scan_display_name(&source);
        // `begin_dir_scan` is only reached for a folder scan (`open_input` routes explicit
        // lists and archives elsewhere); pull the roots + recursive flag for the walk.
        let (roots, recursive) = match source {
            Source::Scan { roots, recursive } => (roots, recursive),
            _ => return,
        };
        // Cross-type supersession (cross-deck open race, 2026-07-17): starting a folder scan must
        // also drop any in-flight archive open — otherwise a stale `ArchiveResolved` landing after
        // this rebuilds the deck back onto the archive on top of the folder we're now scanning.
        // Symmetric with the scan-drop in `begin_archive_open`.
        if let Some(prev) = self.archive_load.take() {
            prev.progress.request_cancel();
        }
        let root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
        let scan_root = roots.first().cloned();
        let worker_progress = progress.clone();
        // Read the live Show Archives preference (task #104) at spawn time: with it off, the
        // walk drops archive "doors" so the deck never lists them.
        let show_archives = self.core.settings.show_archives;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            scan::stream_scan(
                roots,
                recursive,
                show_archives,
                cursor,
                root,
                scan_root,
                generation,
                worker_progress,
                tx,
            );
        });
        // If a Scanning dialog is already up (a previous slow scan the user then re-opened
        // over), re-point it at this new walk in place — same window, no flicker — so it
        // tracks the new folder instead of showing a now-frozen old count.
        if self.dialog.as_ref().map(|d| d.kind()) == Some(dialog::DialogKind::Scanning) {
            let msg = scan_message(&name);
            if let Some(d) = self.dialog.as_mut() {
                d.set_scan(&msg, progress.clone());
            }
        }
        self.core.scanning = true; // core mirror: use sequential-only prefetch while streaming
        self.core.scan_bootstrapped = false; // fresh scan → first non-empty batch bootstraps
        self.dir_scan = Some(DirScan {
            generation,
            rx,
            progress,
            name,
            started: Instant::now(),
        });
    }

    /// Pick up a finished background directory scan (called each `about_to_wait` tick).
    /// On a result for the current generation: rebuild the playlist, or — if the folder
    /// held no supported images — log and keep whatever is on screen (an open failure
    /// never blanks the current photo); either way the Scanning dialog (if up) closes.
    /// While a scan is still running and has outlasted [`SCAN_DIALOG_DELAY`], reveal the
    /// "Scanning Folder" progress dialog (live image count + current folder + Cancel) so a
    /// genuinely slow walk shows it's working and is cancellable. Mirrors
    /// [`poll_archive_load`](App::poll_archive_load).
    fn poll_dir_scan(&mut self) {
        use std::sync::mpsc::TryRecvError;
        // Drain every snapshot queued this tick (several batches may have piled up), applying
        // each as it comes: the first non-empty one bootstraps the view, the rest extend it.
        loop {
            let (cur_gen, recv) = match self.dir_scan.as_ref() {
                Some(scan) => (scan.generation, scan.rx.try_recv()),
                None => return,
            };
            match recv {
                Ok((generation, ScanUpdate::Batch(resolved))) => {
                    if generation != cur_gen {
                        continue; // superseded by a newer open (defensive; rx is per-scan)
                    }
                    // The core filters mid-scan deletes, bootstraps the first non-empty batch
                    // (`scan_bootstrapped`), and extends the rest — see `AppCore::apply_scan_batch`.
                    self.core.handle(contract::CoreEvent::ScanBatch(resolved));
                    // A photo is on screen now — a revealed Scanning dialog has served its
                    // purpose; drop it so browsing starts at the first image, not the end
                    // of the walk (the scan-count chip takes over as progress).
                    if self.core.scan_bootstrapped {
                        self.close_scanning_dialog();
                    }
                }
                Ok((generation, ScanUpdate::Done)) => {
                    if generation != cur_gen {
                        continue; // superseded
                    }
                    // Capture the scanned folder's name before dropping the handle — an empty
                    // folder toasts with it (③) instead of stranding / interrupting.
                    let scanned = self
                        .dir_scan
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.dir_scan = None; // walk finished — drop the worker handle
                    let never_bootstrapped = !self.core.scan_bootstrapped;
                    // Core: resume normal prefetch + restore the open hint if the deck stayed empty.
                    self.core.handle(contract::CoreEvent::ScanDone);
                    self.close_scanning_dialog(); // walk finished — drop the progress dialog
                    if never_bootstrapped {
                        // No images: keep whatever's on screen; a non-modal toast (never a
                        // blocking alert) if a deck is already up (③ keep-deck-until-photos).
                        self.core.scan_found_no_photos(&scanned);
                    }
                    return;
                }
                Err(TryRecvError::Empty) => {
                    // Still scanning and nothing on screen yet: once the walk is slow enough
                    // to notice, reveal the Scanning dialog (count + current folder + Cancel).
                    // Gated on `!scan_bootstrapped` so it never pops over an already-shown photo,
                    // and only when no other dialog is up *or queued* (don't steal a
                    // Settings/Message window the user opened over a background scan, and don't
                    // overwrite a Password/Message request `poll_archive_load` queued into
                    // `pending_dialog` earlier this same tick — it opens at the end of the drain).
                    let reveal = !self.core.scan_bootstrapped
                        && self
                            .dir_scan
                            .as_ref()
                            .is_some_and(|s| s.started.elapsed() >= SCAN_DIALOG_DELAY);
                    if reveal && self.dialog.is_none() && self.pending_dialog.is_none() {
                        let (name, progress) = match self.dir_scan.as_ref() {
                            Some(s) => (s.name.clone(), s.progress.clone()),
                            None => return,
                        };
                        self.open_scanning_dialog(&name, progress);
                    }
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    self.dir_scan = None;
                    self.core.scanning = false;
                    self.close_scanning_dialog(); // worker died — don't strand its dialog
                    return;
                }
            }
        }
    }

    /// Open the deferred "Scanning Folder" progress dialog for an in-flight folder walk
    /// (a live image count, the current subfolder, and a Cancel button). Mirrors the 7z
    /// loading dialog in [`begin_archive_open`](App::begin_archive_open); only called once
    /// the scan has outlasted [`SCAN_DIALOG_DELAY`] and no other dialog is showing.
    fn open_scanning_dialog(&mut self, name: &str, progress: ScanProgress) {
        let msg = scan_message(name);
        self.pending_dialog = Some(DialogRequest::Scanning {
            message: msg,
            progress,
        });
    }

    /// Close the dialog window only if it's the Scanning progress view (a folder scan
    /// finished, was cancelled, or its worker died). Leaves any other dialog
    /// (Settings, Message, …) untouched.
    fn close_scanning_dialog(&mut self) {
        if self.dialog.as_ref().map(|d| d.kind()) == Some(dialog::DialogKind::Scanning) {
            self.dialog = None;
        }
    }

    /// Ask the in-flight directory scan (if any) to stop. The worker bails at its next
    /// entry; [`poll_dir_scan`](App::poll_dir_scan) (or the superseding open) then drops
    /// it. Used when a newer open arrives and on teardown.
    fn cancel_dir_scan(&mut self) {
        if let Some(scan) = self.dir_scan.as_ref() {
            scan.progress.request_cancel();
        }
        // Every cancel path clears `dir_scan` immediately after; keep the core mirror in sync
        // so a `request_prefetch` after the cancel uses the normal (random-ahead) prefetch.
        self.core.scanning = false;
    }

    /// User command (File ▸ Stop Scanning, or a bound key): stop an in-flight folder scan,
    /// **keeping whatever has streamed in so far** (cancel-keeps-partial — the partial
    /// playlist is already live). Resumes normal prefetch (the deck is final now) and flashes
    /// a confirmation. A no-op when no scan is running (the menu item is disabled then).
    fn cancel_scan_command(&mut self) {
        if self.dir_scan.is_none() {
            return;
        }
        self.cancel_dir_scan();
        self.dir_scan = None;
        self.close_scanning_dialog();
        self.core.request_prefetch();
        self.core.show_toast("Scan stopped");
    }

    /// A terminal archive-open failure (not a password retry): forget the pending
    /// archive and replace any open dialog with the error notice.
    fn fail_archive_open(&mut self, e: &archive::ArchiveOpenError) {
        self.core.password_archive = None;
        self.report_archive_error(e);
    }

    /// Close the egui dialog window (if any). Dropping it scrubs an entered password.
    fn close_dialog(&mut self) {
        self.dialog = None;
    }

    /// Prompt for an archive's password (or re-prompt after a wrong one). Remembers
    /// `path` so a submitted password re-opens it. On the first prompt a fresh
    /// Password dialog opens; on a retry (`wrong`) the existing dialog gets an inline
    /// "Incorrect password" error and a cleared field rather than a jarring re-open.
    fn prompt_archive_password(&mut self, path: PathBuf, wrong: bool) {
        self.core.password_archive = Some(path.clone());
        let is_password_dialog =
            self.dialog.as_ref().map(|d| d.kind()) == Some(dialog::DialogKind::Password);
        if wrong && is_password_dialog {
            if let Some(d) = self.dialog.as_mut() {
                d.set_password_error("Incorrect password. Please try again.");
                d.focus();
                d.request_redraw();
            }
            return;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("this archive");
        // Two lines: the lead on one, the (possibly long) file name on its own.
        let prompt = format!("Enter the password for\n\u{201c}{name}\u{201d}");
        self.pending_dialog = Some(DialogRequest::Simple {
            kind: dialog::DialogKind::Password,
            message: prompt,
            archive_optout: false,
        });
    }

    /// Surface an archive-open failure to the user via the egui message dialog
    /// (too-large / corrupt / password / OOM / empty), and log it.
    fn report_archive_error(&mut self, e: &archive::ArchiveOpenError) {
        let msg = e.user_message();
        eprintln!("{}: {msg}", pb_app_core::APP_NAME);
        // An archive with nothing viewable (task #104): offer "Don't show archives" right on
        // the notice, so a folder full of empty/irrelevant archives is one click from staying
        // out of the deck. Every other archive error is a plain notice.
        let offer_optout = matches!(e, archive::ArchiveOpenError::Empty);
        self.open_message_ex(&msg, offer_optout);
    }

    /// Toggle recursive scanning of the current folder (`Ctrl+R`), keeping the current photo
    /// in view. A no-op for an explicit file list (multi-select / dropped photos): there is
    /// no single root to walk.
    ///
    /// The re-scan **streams** like any folder open, so a large tree doesn't freeze the loop.
    /// Two nice properties fall out: turning recursion **on** streams the subfolders in behind
    /// the current photo; turning it **off** *mid-scan* is an escape hatch — `begin_dir_scan`
    /// supersedes the in-flight recursive walk and re-scans just the flat root (fast), i.e.
    /// "stop, I only wanted this folder". The current photo is preserved by path
    /// (`Cursor::At`), falling back to the first image if it isn't in the new listing (e.g.
    /// turning recursion off while viewing a subfolder photo).
    fn toggle_recursive(&mut self) {
        let Some(root) = self.core.scan_root.clone() else {
            return;
        };
        let recursive = !self.core.recursive;
        let cursor = self
            .core
            .displayed_item
            .and_then(|i| self.core.source.path(i))
            .map(Path::to_path_buf)
            .map(open::Cursor::At)
            .unwrap_or(open::Cursor::First);
        self.begin_dir_scan(
            Source::Scan {
                roots: vec![root],
                recursive,
            },
            cursor,
        );
        // Acknowledge the toggle now; the new listing streams in via `poll_dir_scan` (and
        // `self.core.recursive` updates when the first batch bootstraps).
        let msg = if recursive {
            "Recursive folders: on"
        } else {
            "Recursive folders: off"
        };
        self.core.show_toast(msg);
    }

    /// Re-scan the current folder with the live settings (recursive + Show Archives),
    /// keeping the current photo in view — the streaming re-open `toggle_recursive` does,
    /// minus the flag flip. Used when a preference that changes *what the walk admits*
    /// (Show Archives, task #104) changes. A no-op for an archive/explicit deck (no scan
    /// root to re-walk).
    fn rescan_current_folder(&mut self) {
        let Some(root) = self.core.scan_root.clone() else {
            return;
        };
        let cursor = self
            .core
            .displayed_item
            .and_then(|i| self.core.source.path(i))
            .map(Path::to_path_buf)
            .map(open::Cursor::At)
            .unwrap_or(open::Cursor::First);
        self.begin_dir_scan(
            Source::Scan {
                roots: vec![root],
                recursive: self.core.recursive,
            },
            cursor,
        );
    }

    /// Toggle View ▸ Show Archives (task #104): flip whether archives show as browsable
    /// "doors" while scanning a folder, persist the preference, and re-scan the current
    /// folder so the doors appear/disappear at once (`begin_dir_scan` reads the new value).
    /// The checkmark tracks it via the per-tick `MenuState` diff. Also reachable from the
    /// "no images" dialog's opt-out checkbox (`apply_hide_archives`).
    fn toggle_show_archives(&mut self) {
        let on = !self.core.settings.show_archives;
        self.core.settings.show_archives = on;
        self.core.settings.save();
        self.rescan_current_folder();
        self.core.show_toast(if on {
            "Show archives: on"
        } else {
            "Show archives: off"
        });
    }

    /// Apply the "Don't show archives" opt-out from the empty-archive dialog's checkbox:
    /// set the preference to `hide` (checked = hide), persist it, and re-scan the current
    /// folder so the change lands immediately behind the dialog. Idempotent — re-applying
    /// the same value just re-scans harmlessly. No toast: the dialog checkbox *is* the
    /// feedback.
    fn apply_hide_archives(&mut self, hide: bool) {
        let show = !hide;
        if self.core.settings.show_archives == show {
            return;
        }
        self.core.settings.show_archives = show;
        self.core.settings.save();
        self.rescan_current_folder();
    }

    // ── egui rich-panel overlay (task #54 Phase 4) ──────────────────────────────

    /// Whether any egui-presented rich panel (Help / Inspector / folder tree) is on
    /// screen right now — the gate for feeding pointer events to egui and for running
    /// the overlay frame.
    fn overlay_panel_visible(&self) -> bool {
        self.core.help_panel_visible()
            || self.core.inspector_panel_visible()
            || self.core.tree_panel_visible()
            // The Thumbnails strip (task #83) is the left pane's other tab — interactive (tab
            // bar, cell clicks, scroll), so it joins the pointer-routing + render gate. Without
            // this, switching to it makes the gate false (tree_panel_visible is false on this
            // tab) and the overlay deactivates: the strip never composites and looks closed.
            || self.core.thumbs_visible()
            // The scan pill is interactive (its Cancel button), so it joins the pointer-routing
            // gate — egui only *consumes* a click actually over the pill, so panning the photo
            // elsewhere during a scan still works.
            || self.scan_pill_visible()
            // The welcome screen's Open buttons are interactive too.
            || self.core.open_panel_visible()
            // The play hint is interactive (hover pins it, click plays) while it's shown.
            || self.play_hint_shown.is_some()
            // An archive door's card (task #105) is the *only* thing on screen for that
            // item — its frame is a transparent sentinel — so the overlay must stay alive
            // and composited while one is presented, or the viewer sees an empty
            // letterbox. It is also interactive (its Open button).
            || self.core.door_presented()
            // The info line's playback bar (a live video) is a scrubber — route pointer
            // events so clicks/drags on it reach egui (it only consumes events over the pill).
            || self.video_bar_interactive()
            // The Linux windowed menu bar is interactive — route its clicks to egui (egui only
            // consumes a click actually over the bar/dropdown, so the photo stays pannable).
            || self.menu_bar_visible()
            // The docked toolbar (task #61) is interactive — route pointer events to it (egui
            // only consumes a click actually over the strip, so the photo below stays pannable).
            || self.toolbar_visible()
    }

    /// Whether the docked toolbar (task #61) should show: the setting is on and we're in
    /// windowed mode. Hidden in the chrome-free fullscreen speed mode (like the menu bar).
    /// macOS uses its own native toolbar, so this winit strip is off there.
    fn toolbar_visible(&self) -> bool {
        cfg!(not(target_os = "macos")) && self.core.windowed && self.core.settings.show_toolbar
    }

    /// Physical-pixel strip the toolbar reserves at the top (below any menu bar), 0 when hidden.
    /// Added to the renderer's `content_top_inset` so a fit photo sits below the bar, not under it.
    fn toolbar_inset_px(&self) -> u32 {
        if self.toolbar_visible() {
            (toolbar::TOOLBAR_H * self.core.viewport.scale_factor).round() as u32
        } else {
            0
        }
    }

    /// Total top chrome inset (physical px): the Linux menu bar plus the toolbar. The renderer
    /// reserves this so fit content clears all in-client top chrome.
    fn chrome_inset_px(&self) -> u32 {
        self.menu_inset_px() + self.toolbar_inset_px()
    }

    /// Re-reserve/free the top chrome band after something that can change it (a live
    /// `show_toolbar` settings toggle, task #61). Re-fits the photo below the new inset and
    /// re-renders the overlay so the strip appears/disappears on the same frame — the atomic
    /// swap the plan calls for. No-op when the inset is unchanged (the common settings edit).
    fn refresh_chrome_inset(&mut self) {
        let inset = self.chrome_inset_px();
        if self.core.content_top_inset != inset {
            self.core.set_content_top_inset(inset);
            // Re-push the current view so the renderer rebuilds the photo quad against the new
            // content region (the setter only stores the inset; `render` reuses the cached
            // geometry). Without this the image keeps its old fit and doesn't grow/shrink into
            // the space the toolbar freed or took — Fit/Fill re-fit, zoom/pan overflow rides under.
            self.core.push_view();
            self.core.draw();
        }
        self.overlay_dirty = true;
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Build the toolbar's live state from the core (task #61). `&mut` because `current_has_motion`
    /// resolves + caches Live-Photo pairing on first touch. Reads present-truth (`display_counter`
    /// from `displayed_item`), the video-aware motion accessors, and the menu-state toggles.
    fn build_toolbar_state(&mut self) -> toolbar::ToolbarState {
        let has_motion = self.core.current_has_motion();
        let playing = self.core.motion_playing();
        let counter = self.core.display_counter();
        let tree_visible = self.core.tree_panel_visible();
        let slideshow_interval = self.core.slideshow.interval;
        // The menu-state toggles (info line / Details tab / reveal-enabled) — the same source
        // the native menu checkmarks read, so the toolbar's on-states can't drift from them.
        let ms = self.current_menu_state();
        // Translucency experiment (#61): the toolbar rides the same *Info panel opacity* slider
        // the info panels use, so one control tunes all the chrome. `info_opacity` is a 0–100
        // percentage; map it linearly to a 0–255 alpha (egui alpha-blends, no material remap).
        let alpha =
            ((self.core.settings.info_opacity.min(100) as f32 / 100.0) * 255.0).round() as u8;
        toolbar::ToolbarState {
            dark: self.core.hud_dark,
            alpha,
            counter,
            has_motion,
            playing,
            slideshow: ms.slideshow,
            slideshow_interval,
            info_basic: ms.info_basic,
            info_full: ms.info_full,
            tree_visible,
            can_delete: ms.reveal_enabled,
        }
    }

    /// Whether the egui overlay has **any** content to composite — the interactive panels
    /// *or* the non-interactive info readout (`i`). Drives overlay activation/render. (The
    /// plain info line is deliberately absent from [`overlay_panel_visible`](Self::overlay_panel_visible),
    /// which gates *pointer* routing: the readout must never intercept clicks meant for the
    /// photo. With a live video it joins that gate via `video_bar_interactive` — the playback
    /// bar is a scrubber.)
    fn overlay_visible(&self) -> bool {
        self.overlay_panel_visible()
            || self.toolbar_visible()
            || self.core.info_line_visible()
            // Keep compositing (and re-rendering) through any layer's fade-out.
            || self
                .info_vanished_at
                .is_some_and(|at| at.elapsed() < INFO_FADE_OUT)
            || self.tree_fade.fading_out()
            || self.inspector_fade.fading_out()
    }

    /// Whether the info line currently carries the interactive video playback bar
    /// (task #79 follow-up): visible line + a live session's progress row. Runs on
    /// every pointer event, so the no-video common case short-circuits first.
    fn video_bar_interactive(&self) -> bool {
        self.core.video.is_some()
            && self.core.info_line_visible()
            && self.core.video_progress_row().is_some()
    }

    /// Whether the windowed menu bar should show — **Linux only** (the egui stand-in for the
    /// native muda bar, which can't attach to winit's non-GTK window). Windowed mode only; the
    /// fullscreen speed mode stays chrome-free, matching the native bar's windowed-only rule.
    fn menu_bar_visible(&self) -> bool {
        cfg!(all(unix, not(target_os = "macos"))) && self.core.windowed
    }

    /// Mark the egui overlay for a re-render this turn and wake the redraw — used when
    /// menu state changes from the key/mouse handlers (outside an egui frame).
    #[cfg(all(unix, not(target_os = "macos")))]
    fn touch_overlay(&mut self) {
        self.overlay_dirty = true;
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Feed one key press to the menu bar's state machine ([`menu::menu_nav_key`]),
    /// building the live menu spec so enabled/disabled rows are honored. `Ignored` means
    /// the menus don't own this key in the current state — let the keymap have it.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn menu_key(&mut self, code: KeyCode) -> menu::MenuKeyOutcome {
        use menu::{MenuKey, MenuKeyOutcome};
        let open = self.menu_nav.open.is_some();
        let alt = self.core.mods.alt;
        let key = if open {
            match code {
                KeyCode::Escape => Some(MenuKey::Esc),
                KeyCode::ArrowLeft => Some(MenuKey::Left),
                KeyCode::ArrowRight => Some(MenuKey::Right),
                KeyCode::ArrowUp => Some(MenuKey::Up),
                KeyCode::ArrowDown => Some(MenuKey::Down),
                KeyCode::Home => Some(MenuKey::Home),
                KeyCode::End => Some(MenuKey::End),
                KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space => Some(MenuKey::Activate),
                KeyCode::F10 => Some(MenuKey::F10),
                c => letter_of(c).map(|ch| {
                    if alt {
                        MenuKey::AltChar(ch)
                    } else {
                        MenuKey::Char(ch)
                    }
                }),
            }
        } else if code == KeyCode::F10 {
            Some(MenuKey::F10)
        } else if alt {
            letter_of(code).map(MenuKey::AltChar)
        } else {
            None
        };
        match key {
            Some(k) => {
                let groups = menu::menu_bar_spec(&self.core.keymap, &self.current_menu_state());
                menu::menu_nav_key(&mut self.menu_nav, &groups, k)
            }
            // An open dropdown grabs unmapped presses too (digits, F-keys…) so they
            // never leak into photo nav underneath the menu.
            None if open => MenuKeyOutcome::Consumed,
            None => MenuKeyOutcome::Ignored,
        }
    }

    /// Physical-pixel top strip to reserve for the windowed menu bar (0 elsewhere / in
    /// fullscreen). Fed to the renderer's `content_top_inset` so the photo fits + centers
    /// *below* the bar instead of under it — never clipping the image's top edge. Scales
    /// [`panels_ui::MENU_BAR_H`] by the live DPI factor.
    fn menu_inset_px(&self) -> u32 {
        #[cfg(all(unix, not(target_os = "macos")))]
        if self.menu_bar_visible() {
            return (panels_ui::MENU_BAR_H * self.core.viewport.scale_factor).round() as u32;
        }
        0
    }

    /// Recreate the overlay's offscreen target for a new surface size and force a
    /// re-render (the panels re-lay-out; the renderer is re-handed the new texture).
    fn resize_overlay(&mut self, w: u32, h: u32) {
        if let Some(dev) = self.core.renderer.as_ref().map(|r| r.device()) {
            if let Some(ov) = self.egui_overlay.as_mut() {
                ov.resize(dev, w, h);
            }
        }
        self.overlay_dirty = true;
        // Re-lay-out the panels *now* rather than deferring to the next `about_to_wait`.
        // A live resize reconfigures the swapchain and repaints synchronously (see the
        // `Resized` handler → `handle_resized`), so a deferred re-render would let that
        // frame composite the stale, old-size overlay texture — stretched to the new
        // viewport by the 1:1 fullscreen-triangle compositor. Only when a panel or the
        // info line is actually shown; nothing composites otherwise.
        if self.overlay_visible() {
            self.render_overlay_frame();
            self.overlay_dirty = false;
        }
    }

    /// Hand the core's subtitle overlay to the wgpu presenter (task #90.5) — this shell's
    /// half of the contract `tick_subtitles` fills in.
    ///
    /// ⚠ **Shell-local on purpose; this does NOT belong in `tick_subtitles`.** The macOS
    /// host draws the very same bitmap as a SwiftUI overlay above its `AVPlayerLayer`
    /// (`pb-mac-ffi`'s `subtitle_rgba`/`subtitle_rect`). Moving this into the shared tick
    /// would draw every cue twice there — once into a canvas the player layer is covering,
    /// once for real. One rasterizer, two presenters, and each shell owns its own.
    ///
    /// Re-uploads only when the core's generation moves, which is on a cue change — never
    /// on the ~120 frames per second in between.
    fn present_subtitles(&mut self) {
        let gen = self.core.subtitles.gen();
        if gen == self.subtitle_gen {
            return;
        }
        self.subtitle_gen = gen;
        // Split the borrow: the bitmap is read out of `subtitles` while `renderer` is
        // borrowed mutably. Disjoint fields, so destructuring is what makes it legal.
        let AppCore {
            subtitles,
            renderer,
            viewport,
            ..
        } = &mut self.core;
        let Some(r) = renderer.as_mut() else {
            return;
        };
        // `rect()` is logical points (what a UI toolkit positions in — the macOS host's
        // unit); wgpu draws in physical px. `update()` divided by this exact factor, so
        // multiplying back is the inverse, not an approximation.
        let s = viewport.scale_factor.max(0.01);
        match (subtitles.bitmap(), subtitles.rect()) {
            (Some(b), Some(rect)) => {
                r.set_subtitle_overlay(Some((&b.rgba, b.w, b.h)), rect.x * s, rect.y * s)
            }
            // `gen` moved but there is nothing to show: the cue ended, subtitles were
            // switched off, or the video stopped. Clearing is the whole point of routing
            // every such case through one exit (post-mortem bug #1 — a frozen cue).
            _ => r.set_subtitle_overlay(None, 0.0, 0.0),
        }
    }

    /// Drive the egui rich-panel overlay each `about_to_wait` turn: (re)render the panels
    /// into the offscreen texture when they changed (or an egui animation frame is due)
    /// and hand it to the renderer, or clear the composited layer when nothing is open.
    /// Retained — a nav frame with a static panel open re-renders nothing. Returns egui's
    /// next timed-repaint deadline for the wake calc.
    fn update_overlay(&mut self, now: Instant) -> Option<Instant> {
        // The info-line fade stamps live HERE, on the visibility edge — this runs
        // every turn, while renders don't. (Stamping inside the render was the
        // owner-reported inconsistency: hide-without-a-render popped the line and
        // left a stale appear stamp, so the NEXT show popped too.)
        let line_now = self.core.info_line_visible();
        if line_now != self.info_line_was_visible {
            self.info_line_was_visible = line_now;
            if line_now {
                self.info_shown_at = Some(now);
                self.info_vanished_at = None;
            } else {
                self.info_shown_at = None;
                if self.last_info_line.is_some() {
                    self.info_vanished_at = Some(now);
                }
            }
            self.overlay_dirty = true;
        }
        // Same edge tracking for the faded panels (tree / Inspector).
        if self.tree_fade.edge(self.core.tree_panel_visible(), now)
            | self
                .inspector_fade
                .edge(self.core.inspector_panel_visible(), now)
        {
            self.overlay_dirty = true;
        }
        // Toolbar (task #61): re-render when a button's state changes (counter, play, toggles)
        // or while a nav button is held (so the mouse-up release edge is observed within a tick,
        // even if the blaze loop is momentarily idle). Retained otherwise — a static toolbar over a
        // still photo re-renders nothing.
        if self.toolbar_visible() {
            let st = self.build_toolbar_state();
            if self.last_toolbar_state != Some(st) {
                self.last_toolbar_state = Some(st);
                self.overlay_dirty = true;
            }
            if self.toolbar_nav_held.is_some() {
                self.overlay_dirty = true;
            }
        } else if self.last_toolbar_state.take().is_some() {
            self.overlay_dirty = true;
        }
        if !self.overlay_visible() {
            // Nothing open: drop the composited layer once, on the visibility edge.
            if self.overlay_active {
                if let Some(r) = self.core.renderer.as_mut() {
                    r.set_egui_overlay(None);
                }
                self.overlay_active = false;
                self.overlay_dirty = false;
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            return None;
        }
        // A panel is open. Re-render egui only when it changed, an egui animation frame is due,
        // or it just became visible — never per nav frame (retained texture). Renders are
        // FLOORED to the refresh interval: egui asks for ASAP repaints while a control is hovered
        // (tooltip fade) or a nav button is held, and an unthrottled render→request_redraw→render
        // loop would spin the event loop far above vsync (audible GPU coil-whine). First
        // activation renders immediately (no blank frame); everything else waits out the frame,
        // so any deferred work is picked up on the next slot (≤ one refresh later, imperceptible).
        let frame = self.core.frame_interval;
        let throttled = self
            .last_overlay_render
            .is_some_and(|t| now.saturating_duration_since(t) < frame);
        let repaint_due = self
            .egui_overlay
            .as_ref()
            .and_then(|o| o.repaint_at())
            .is_some_and(|at| now >= at);
        let want = self.overlay_dirty || repaint_due;
        if !self.overlay_active || (want && !throttled) {
            self.render_overlay_frame();
            self.last_overlay_render = Some(now);
            self.overlay_dirty = false;
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
        // Next wake. If we still owe a render (a dirty or a due repaint we throttled), ask to wake
        // one frame after the last render — never "now", which would busy-spin the loop above. Any
        // genuinely future egui repaint stands on its own; an ASAP request floors to that slot.
        let next_slot = self.last_overlay_render.map(|t| t + frame);
        let owe = self.overlay_dirty
            || self
                .egui_overlay
                .as_ref()
                .and_then(|o| o.repaint_at())
                .is_some_and(|at| now >= at);
        let future_repaint = self
            .egui_overlay
            .as_ref()
            .and_then(|o| o.repaint_at())
            .filter(|&at| at > now);
        [owe.then_some(next_slot).flatten(), future_repaint]
            .into_iter()
            .flatten()
            .min()
            .filter(|&at| at > now)
    }

    /// Render one egui frame of the open panels into the overlay's offscreen texture and
    /// hand it to the renderer. The panel data is snapshotted from the core so the egui
    /// closure doesn't borrow it; panel actions collected during the frame are applied
    /// afterward (once the egui borrows have ended).
    fn render_overlay_frame(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        // The toolbar's state (task #61), computed before the immutable-borrow closure below
        // (`current_has_motion` needs `&mut`). `None` when the toolbar is hidden.
        let toolbar_state = self.toolbar_visible().then(|| self.build_toolbar_state());
        let mut frame = panels_ui::PanelFrame::snapshot(&self.core);
        // PB_DOOR_DIAG: every time the overlay texture is actually rebuilt, whether the door card
        // is in it. If the card is stuck on screen but these lines STOP (no rebuild) while the
        // core's `[door-diag] draw door_card=None` keeps printing, the overlay texture is stale
        // (shell bug). If `frame.door=Some` here, the core still believes it's a door (core bug).
        if door_diag() {
            eprintln!(
                "[door-diag] shell render_overlay_frame: frame.door={:?} core.door_presented={} displayed={:?}",
                frame.door.as_ref().map(|d| d.name.clone()),
                self.core.door_presented(),
                self.core.displayed_item,
            );
        }
        // Fade the info line in/out over ~100 ms (macOS-shell parity — its SwiftUI
        // overlays transition; the egui line used to pop, most visibly on the
        // video hover reveal). Appearances ramp `fade` up; a disappearance keeps
        // drawing the *last* line briefly with `fade` ramping down. The line
        // itself requests egui repaints while fading.
        let now = Instant::now();
        // The tree + Inspector share the same ramp mechanics (retained content
        // draws through the out leg after the core hides the panel).
        frame.tree_fade = self.tree_fade.resolve(&mut frame.tree, now);
        frame.inspector_fade = self.inspector_fade.resolve(&mut frame.inspector, now);
        match frame.info.take() {
            Some(mut line) => {
                // Ramp from the appear stamp `update_overlay` set on the edge.
                line.fade = self
                    .info_shown_at
                    .map(|s| now.duration_since(s).as_secs_f32() / INFO_FADE_IN.as_secs_f32())
                    .unwrap_or(1.0)
                    .min(1.0);
                self.last_info_line = Some(line.clone());
                frame.info = Some(line);
            }
            None => {
                // Fade the LAST line out from the vanish stamp (the core has
                // already hidden it; the shell keeps drawing through the ramp).
                if let (Some(last), Some(gone)) =
                    (self.last_info_line.as_ref(), self.info_vanished_at)
                {
                    let t = now.duration_since(gone).as_secs_f32() / INFO_FADE_OUT.as_secs_f32();
                    if t < 1.0 {
                        let mut line = last.clone();
                        line.fade = 1.0 - t;
                        frame.info = Some(line);
                    } else {
                        self.last_info_line = None;
                        self.info_vanished_at = None;
                    }
                }
            }
        }
        // Scan state is shell-owned (`dir_scan`), so the pill is filled in here rather than
        // by `snapshot` (which only reaches the core).
        frame.scan = self.scan_pill_frame();
        // Play-hint fade is shell-owned too (computed by `tick_play_hint` each turn).
        frame.play_hint = self.play_hint_frame.clone();
        // Linux: reserve the menu-bar strip so the top-anchored panels (tree / inspector /
        // scan pill) sit below the bar, not under it. Logical px — the egui overlay lays out
        // in points, same units as the `TopBottomPanel` height (not the physical `menu_inset_px`).
        #[cfg(all(unix, not(target_os = "macos")))]
        if self.menu_bar_visible() {
            frame.top_inset = panels_ui::MENU_BAR_H;
        }
        // The toolbar (task #61) stacks below any menu bar, so the top-anchored panels clear both.
        if toolbar_state.is_some() {
            frame.top_inset += toolbar::TOOLBAR_H;
        }
        let mut actions: Vec<panels_ui::PanelAction> = Vec::new();
        // The Thumbnails strip (task #83) is rendered after `build` in the same egui frame — it
        // reads the core's RAM thumb store live and owns an egui texture cache, so it can't be a
        // pure `PanelFrame` snapshot. Take its pending follow-scroll here (the mutable core op,
        // the macOS `take_thumb_scroll`) before the immutable core borrows below; apply the
        // FollowState handshake via the returned actions.
        let thumbs_visible = self.core.thumbs_visible();
        let thumb_pending = if thumbs_visible {
            self.core
                .thumbs
                .pending_scroll
                .take()
                .map(|c| (c.item, c.gen))
        } else {
            None
        };
        // The left pane's + Inspector's live widths are shell-owned (resizable).
        frame.pane_width = self.tree_width;
        frame.inspector_width = self.inspector_width;
        let thumbs_dark = frame.dark;
        let thumbs_alpha = frame.panel_alpha;
        let thumbs_top = frame.top_inset;
        let thumbs_width = self.tree_width;
        // Linux: the windowed menu bar (the egui stand-in for the native muda bar). Build its
        // spec here from the live menu state + keymap — an owned `Vec`, so it can be borrowed
        // into the render closure without tangling with the `&mut self.egui_overlay` borrow
        // below — then draw it in the same egui frame as the panels.
        #[cfg(all(unix, not(target_os = "macos")))]
        let menu_groups: Option<Vec<menu::MenuGroup>> = self
            .menu_bar_visible()
            .then(|| menu::menu_bar_spec(&self.core.keymap, &self.current_menu_state()));
        {
            // Borrowed as a field-local so the egui closure can take it alongside the
            // disjoint `&mut self.egui_overlay` borrow.
            #[cfg(all(unix, not(target_os = "macos")))]
            let menu_nav = &mut self.menu_nav;
            // Disjoint field-locals so the thumbs strip can read the core + own its texture
            // cache inside the closure alongside the `&mut self.egui_overlay` borrow.
            let thumb_core = &self.core;
            let thumb_strip = &mut self.thumb_strip;
            // The toolbar's held-nav slot (task #61), a disjoint field-local for the closure.
            let toolbar_nav_held = &mut self.toolbar_nav_held;
            // The live keymap, so the toolbar's tooltips show each action's real binding.
            let toolbar_keymap = &self.core.keymap;
            let (device, queue) = match self.core.renderer.as_ref() {
                Some(r) => (r.device(), r.queue()),
                None => return,
            };
            if let Some(ov) = self.egui_overlay.as_mut() {
                ov.run(&window, device, queue, |ctx| {
                    panels_ui::build(ctx, &frame, &mut actions);
                    if thumbs_visible {
                        panels_ui::thumbs_panel(
                            ctx,
                            thumb_core,
                            thumb_strip,
                            thumbs_dark,
                            thumbs_alpha,
                            thumbs_top,
                            thumbs_width,
                            1.0,
                            thumb_pending,
                            &mut actions,
                        );
                    }
                    #[cfg(all(unix, not(target_os = "macos")))]
                    if let Some(groups) = &menu_groups {
                        panels_ui::menu_bar(
                            ctx,
                            frame.dark,
                            frame.panel_alpha,
                            groups,
                            menu_nav,
                            &mut actions,
                        );
                    }
                    // The docked toolbar (task #61) — after the menu bar so it stacks below it.
                    if let Some(st) = &toolbar_state {
                        toolbar::toolbar(ctx, st, toolbar_keymap, toolbar_nav_held, &mut actions);
                    }
                });
            }
        }
        // Hand the (retained) texture to the renderer for compositing. `pointer_over_panel` is
        // NOT refreshed here — it tracks the *live* pointer in the event handler; deriving it from
        // egui's stored (one-frame-late) pointer here was the source of the right-to-left flicker.
        if let Some(ov) = self.egui_overlay.as_ref() {
            let target = ov.target();
            if let Some(r) = self.core.renderer.as_mut() {
                r.set_egui_overlay(Some(target));
            }
        }
        self.overlay_active = true;
        for action in actions {
            self.apply_panel_action(action);
        }
    }

    /// The shell's **single** cursor writer, run once per tick (after the overlay renders). It
    /// composes three sources so egui and the core never fight over the window cursor — the
    /// resize-handle flicker crossing in from the photo. Priority, highest first:
    ///
    /// - a **resize zone** — a panel's reported handle rect, hit-tested against the *live*
    ///   pointer in egui points — wins with the horizontal-resize arrow, geometrically & lag-free;
    /// - else, over a panel, egui's own hover cursor (pointer-hand over a tab/row, text, …);
    /// - else the core's photo cursor (grab while pannable, otherwise the arrow).
    ///
    /// Applies only on a real change, so it's idempotent and never thrashes.
    fn resolve_cursor(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        // The live pointer in egui **points** — same space as the panels' reported handle rects
        // (`ui.min_rect()` is points; the winit pointer is physical, so divide by ppp).
        let ppp = self
            .egui_overlay
            .as_ref()
            .map(|o| o.pixels_per_point())
            .unwrap_or(1.0);
        let pt = self
            .last_pointer
            .map(|(x, y)| (x as f32 / ppp, y as f32 / ppp));
        let in_zone = |zone: Option<[f32; 4]>, open: bool| -> bool {
            match (open, zone, pt) {
                (true, Some(z), Some((px, py))) => {
                    px >= z[0] && px <= z[2] && py >= z[1] && py <= z[3]
                }
                _ => false,
            }
        };
        // Gate each zone on its panel being open (a stale rect from a prior open is ignored).
        let left_open = self.core.tree_panel_visible() || self.core.thumbs_visible();
        let over_resize = in_zone(self.left_pane_edge, left_open)
            || in_zone(self.inspector_edge, self.core.inspector_panel_visible());

        let cursor = if over_resize {
            CursorIcon::EwResize
        } else if self.pointer_over_panel && self.overlay_panel_visible() {
            self.egui_overlay
                .as_ref()
                .map(|o| egui_cursor_to_winit(o.desired_cursor()))
                .unwrap_or(CursorIcon::Default)
        } else {
            cursor_icon(self.core_cursor)
        };
        if self.applied_cursor != Some(cursor) {
            window.set_cursor(cursor);
            self.applied_cursor = Some(cursor);
        }
    }

    /// Apply a panel interaction (a close/tab/copy/tree action from the egui frame) to
    /// the core, then mark the overlay dirty so it reflects the new state next turn.
    fn apply_panel_action(&mut self, action: panels_ui::PanelAction) {
        use panels_ui::PanelAction as A;
        match action {
            A::CloseHelp => self.core.toggle_help(),
            A::CloseInspector => self.core.panels.inspector = None,
            A::CloseTree => self.core.toggle_folder_tree(),
            A::SelectTab(tab) => self.core.panels.open_inspector(tab),
            // Left-pane tab-bar click (task #83) — ride the same ⇧F/⇧T actions the keyboard
            // does (the strip only pushes this for the non-selected tab, so it never closes).
            A::SelectLeftTab(pb_app_core::LeftTab::Folders) => self.core.toggle_folder_tree(),
            A::SelectLeftTab(pb_app_core::LeftTab::Thumbnails) => self.core.toggle_thumbnails(),
            A::CloseThumbs => self.core.toggle_thumbnails(),
            // The left pane's resize drag — store the shared width; next render lays out at it.
            A::SetPaneWidth(w) => self.tree_width = w,
            A::SetInspectorWidth(w) => self.inspector_width = w,
            // The panels report their resize-handle strip rects (egui points) each render so
            // `resolve_cursor` can own the resize arrow geometrically (lag-free crossing in).
            A::PaneResizeZone(r) => {
                self.left_pane_edge = Some([r.min.x, r.min.y, r.max.x, r.max.y]);
            }
            A::InspectorResizeZone(r) => {
                self.inspector_edge = Some([r.min.x, r.min.y, r.max.x, r.max.y]);
            }
            A::ThumbClick(i) => self.core.thumb_jump(i),
            // The strip reported its demand window — mirror the macOS `thumbs_set_viewport`:
            // record it, rebalance the cache to it, and kick prefetch to fill it.
            A::ThumbViewport { visible, overscan } => {
                self.core.thumbs.viewport = Some((visible, overscan));
                if let Some(cur) = self.core.playlist.current() {
                    let demand = self.core.thumbs.demand(cur);
                    self.core.thumbs.cache.rebalance(&demand);
                }
                self.core.request_prefetch();
            }
            A::ThumbUserScrolled => self.core.thumbs.follow.user_scrolled(),
            A::ThumbScrollDone(gen) => self.core.thumbs.follow.programmatic_done(gen),
            A::CopyDetails => self.core.dispatch_action(Action::CopyImageDetails),
            A::CopyText => self.core.dispatch_action(Action::CopyImageText),
            A::CopyDescribe => self.core.dispatch_action(Action::CopyDescription),
            // The Describe tab's "Ask" button opens the ask-a-question dialog
            // (`DialogKind::AskImage`) — fully wired on winit (multi-line question field).
            A::Ask => self.core.dispatch_action(Action::AskImage),
            A::TreeToggle(path) => self.core.fs_tree_toggle(&path),
            A::TreeOpen(path) => self.core.fs_tree_open(path),
            A::TreeExtendUp => self.core.fs_tree_extend_up(),
            // An archive folder row: re-scope the deck / open the container folder (task #66).
            A::TreeActivate(i) => self.core.tree_activate(i),
            A::CancelScan => self.cancel_scan_command(),
            A::OpenFile => self.core.dispatch_action(Action::OpenFile),
            A::OpenFolder => self.core.dispatch_action(Action::OpenFolder),
            A::PlayPause => {
                // Click the hint → play (like the P key) and dismiss it (done its job).
                self.play_hint_fade_out = Some(Instant::now());
                self.core.dispatch_action(Action::PlayPause);
            }
            A::PlayHintHover(hovered) => self.play_hint_hovered = hovered,
            // The playback bar was clicked/dragged — seek the video to that fraction.
            A::SeekVideo(frac) => self.core.video_seek_fraction(frac),
            // The playback row's play/pause button — the `P` key's path exactly
            // (pause/resume a running clip, replay an ended one).
            A::VideoPlayPause => self.core.dispatch_action(Action::PlayPause),
            // The Linux windowed menu bar dispatches through the very same path the native
            // muda bar uses, so an egui menu click behaves identically to a keyboard action.
            #[cfg(all(unix, not(target_os = "macos")))]
            A::Menu(action) => self.dispatch_menu(action),
            // A toolbar one-shot button: the exact `Action` path a keypress/menu item takes.
            A::ToolbarAction(action) => {
                self.core.dispatch_action(action);
                // Entering the (windowed-only) toolbar's Full Screen button hides the toolbar
                // itself, so teach the exit key — the strip can't offer a "come back" button.
                // Only on this click path, never the hotkey/menu (the user already knows those).
                // Mirrors the macOS toolbar's one-shot hint; the key is read live from the keymap
                // so a remap tracks, and it's skipped if Fullscreen has been unbound entirely.
                if action == Action::Fullscreen && !self.core.windowed {
                    let key = self
                        .core
                        .keymap
                        .bindings_for(Action::Fullscreen)
                        .first()
                        .map(|c| c.shortcut_label());
                    if let Some(key) = key {
                        // Queue it, don't show it now: the borderless-fullscreen switch happens on
                        // a later effect drain, so an immediate toast flickers over the windowed
                        // frame first. Fire it after a short delay, once fullscreen has settled.
                        self.pending_toast = Some((
                            Instant::now() + FULLSCREEN_HINT_DELAY,
                            format!("Press {key} to exit Quick Full Screen"),
                        ));
                    }
                }
            }
            // A toolbar nav/random press: begin pointer hold-to-blaze (an initial advance now, the
            // self-paced blaze while held). A quick click is begin→release = one advance.
            A::ToolbarNavPress(action) => self.core.begin_pointer_nav(action),
            A::ToolbarNavRelease => self.core.end_pointer_nav(),
        }
        self.overlay_dirty = true;
    }

    /// Shell side of [`CoreEffect::SetWindowMode`]: apply the borderless-fullscreen ⇄ windowed
    /// window ops, run from the drain. Reads the already-flipped `self.core.windowed` (the effect's
    /// `WindowMode` payload is the same signal). The core `Fullscreen` arm flipped the mode; this
    /// does the platform side — snapshot + persist the windowed geometry (an explicit user action,
    /// never the view path — privacy #2), then set_fullscreen/decorations, macOS chrome + menu
    /// attach *before* the windowed sizing, then the windowed geometry restore. NOTE: F11 /
    /// Alt+Enter has a brief flip-model resize artifact (the photo stretches for a frame as the
    /// compositor scales the old buffer); the DWM-cloak fix regressed the taskbar so it's accepted
    /// (tasks.json #21).
    /// Core + shell response to a new client size: update the core (viewport / fit /
    /// swapchain reconfigure / debounced crisp re-decode), re-assert the macOS EDR surface
    /// when the fit changed, redraw, and remember the windowed geometry. Shared by the
    /// `Resized` event and the *synchronous*-resize path in `apply_window_mode`: winit
    /// returns `Some(new_size)` and emits **no** `Resized` event when the OS satisfies a
    /// `request_inner_size` immediately, so a fullscreen→windowed restore would otherwise
    /// never tell the core its new size — leaving the empty-state Open panel centered for
    /// the old surface until a manual resize or hover re-centered it.
    fn handle_resized(&mut self, width: u32, height: u32) {
        // Compute the fit-change *before* `handle` updates the core's fit, so the shell can
        // gate its GPU/window bits (the macOS EDR re-assert + the redraw) on the same
        // signal without recomputing it inside the core.
        let new_fit = FitBox {
            max_width: width.max(1),
            max_height: height.max(1),
        };
        let fit_changed = Some(new_fit) != self.core.fit;
        // Core: viewport + fit + swapchain reconfigure + the debounced crisp re-decode.
        self.core.handle(contract::CoreEvent::Resized {
            width,
            height,
            scale: self.core.viewport.scale_factor,
        });
        // Re-reserve the top chrome strip (Linux menu bar + toolbar, task #61; 0 in fullscreen)
        // for the new size/DPI/mode before the redraw below picks up the new geometry — keeps the
        // photo below the in-client chrome.
        let inset = self.chrome_inset_px();
        self.core.set_content_top_inset(inset);
        if fit_changed {
            self.core.draw();
        }
        // Remember the new windowed size so it can be restored later (#1).
        self.track_windowed_geometry();
    }

    /// Break a frozen-display loop. When a frame is persistently dropped because the surface is
    /// `Lost`/`Outdated` AND the swapchain's configured size has drifted from the live window (a
    /// fullscreen / DPI / monitor transition whose `Resized` never fully reached the renderer),
    /// the reconfigure inside `render` keeps re-using that stale size, so every frame is dropped
    /// and the display freezes on the last presented frame until a manual resize. The renderer
    /// holds no window handle, so it can't self-correct — re-assert the window's TRUE current size
    /// here. Gated on an actual size mismatch, so a drop for any other reason never churns a
    /// resize (that case is only logged, for follow-up). Owner-reported as "door card / photo
    /// stuck, won't refresh when advancing" — the frozen contents were a red herring; the freeze
    /// is this surface loop (confirmed via PB_DOOR_DIAG: core state correct, every frame dropped).
    fn heal_surface_if_dropped(&mut self) {
        if !self.core.redraw_pending {
            return;
        }
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let sz = window.inner_size();
        if sz.width == 0 || sz.height == 0 {
            return; // minimized — not a real size to react to
        }
        let cfg = self.core.renderer.as_ref().map(|r| r.surface_size());
        if cfg == Some((sz.width, sz.height)) {
            // Size is correct — a dropped frame here is a transient surface loss that recovers on
            // its own, not the stale-swapchain freeze. Don't churn a same-size reconfigure.
            if door_diag() {
                eprintln!(
                    "[door-diag] frame dropped, surface size OK ({}x{}) — not a size drift",
                    sz.width, sz.height
                );
            }
            return;
        }
        if door_diag() {
            eprintln!(
                "[door-diag] surface heal: window {}x{} != config {:?} — re-asserting size",
                sz.width, sz.height, cfg
            );
        }
        // The full resize path: viewport + fit + swapchain reconfigure + refit.
        self.handle_resized(sz.width, sz.height);
        // Belt: `handle_resized` skips the renderer when the core's fit already matched (a
        // renderer-config-only drift), so force the swapchain to the true size directly if it's
        // still stale, then request a fresh frame — the corrected surface should now present.
        if let Some(r) = self.core.renderer.as_mut() {
            if r.surface_size() != (sz.width, sz.height) {
                r.resize(sz.width, sz.height);
            }
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    fn apply_window_mode(&mut self) {
        // Clone the window handle (an Arc) so it can be driven while `self` is still borrowed
        // mutably below (the menu attach needs `&mut self`).
        let Some(window) = self.window.clone() else {
            return;
        };
        // Leaving windowed mode: snapshot where the window is now (a live window read — shell)
        // *before* we resize away from it, so toggling back (and the next launch) restore this
        // spot rather than the OS default corner (#1). Then persist the new mode + remembered
        // geometry together (one atomic write) — an explicit user action, never the view path.
        if !self.core.windowed {
            self.capture_windowed_geometry();
        }
        self.core.geometry_save_at = None;
        self.core.settings.save();
        if self.core.windowed {
            window.set_fullscreen(None);
            window.set_decorations(true);
        } else {
            // Borderless "windowed fullscreen": size a decoration-less window
            // to the monitor ourselves instead of the OS fullscreen API, which
            // makes Windows apply fullscreen-optimizations that drop DWM
            // composition on focus changes / transitions and flash the legacy
            // basic-theme caption. A plain borderless window stays composited.
            // (The monitor sizing itself happens below, *after* the menu is hidden.)
            //
            // Linux (X11/Wayland) is the exception: a manually-sized borderless
            // window can't paint over the compositor's reserved panel zones (the
            // GNOME top bar / taskbar), so it never truly covers the screen. Use
            // the real fullscreen API there — the DWM concern above is Windows-only.
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                window.set_decorations(false);
                window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            }
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            {
                window.set_fullscreen(None);
                window.set_decorations(false);
            }
        }
        // Show the menu in windowed mode, hide it in fullscreen (the chrome-free speed
        // mode). This MUST run *before* the sizing below in BOTH modes: adding/removing
        // the native menu bar changes the client area without moving the outer window, so
        // a borderless-fullscreen window sized while the menu is still attached ends up one
        // menu-bar taller than the monitor — its bottom overhangs off-screen and crops the
        // photo (task #57). Hiding the menu first makes the fullscreen outer == the monitor;
        // it also lets the windowed restore account for the menu height (avoids a per-toggle
        // height drift). Adding/removing the bar resizes the client area → a `Resized` event
        // → the debounced re-decode path.
        self.apply_menu_for_mode();

        if self.core.windowed {
            // Restore the saved windowed geometry when enough of it still lands on a
            // connected monitor; otherwise fall back to the default size at the
            // OS-chosen spot (so a stale off-screen position can't strand the window).
            let rects = collect_monitor_rects(window.available_monitors());
            // A synchronously-applied resize returns `Some(new)` and fires no `Resized`
            // event, so drive the core directly (see `handle_resized`) — otherwise the
            // fullscreen→windowed restore leaves the empty-state Open panel centered for
            // the old (fullscreen) surface until a manual resize or hover.
            match self.core.windowed_restore(&rects) {
                Some(g) => {
                    let applied = window.request_inner_size(PhysicalSize::new(g.w, g.h));
                    window.set_outer_position(PhysicalPosition::new(g.x, g.y));
                    if let Some(new) = applied {
                        self.handle_resized(new.width, new.height);
                    }
                }
                None => {
                    if let Some(new) = window.request_inner_size(PhysicalSize::new(1280, 800)) {
                        self.handle_resized(new.width, new.height);
                    }
                }
            }
        } else if let Some(mon) = window.current_monitor() {
            // Linux drove real fullscreen above; the compositor owns the surface size and
            // emits an async `Resized` the normal path handles — never hand-size it here (a
            // manual `request_inner_size` while fullscreen fights the compositor).
            #[cfg(all(unix, not(target_os = "macos")))]
            let _ = mon;
            // Size the borderless window to exactly the monitor — done here, *after* the
            // menu is hidden, so the outer bounds match the monitor instead of hanging one
            // menu-bar-height below it (task #57). If winit applied the resize synchronously
            // it returns the new size and emits no `Resized` event — feed it to the core
            // ourselves (see `handle_resized`), else overlays stay placed for the old surface.
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            {
                window.set_outer_position(mon.position());
                if let Some(new) = window.request_inner_size(mon.size()) {
                    self.handle_resized(new.width, new.height);
                }
            }
        }
    }

    /// Snapshot the live window's outer (decorated) top-left + inner (client) size
    /// into `settings.window`, so the windowed spot can be restored later (#1). A
    /// failed `outer_position` query (rare) or a zero size leaves the old value.
    fn capture_windowed_geometry(&mut self) {
        let Some(a) = self.window.as_ref() else {
            return;
        };
        let Ok(pos) = a.outer_position() else {
            return;
        };
        let size = a.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.core.settings.window = Some(settings::WindowGeometry {
            x: pos.x,
            y: pos.y,
            w: size.width,
            h: size.height,
        });
    }

    /// While windowed, refresh the remembered geometry from the live window and arm
    /// the debounced save (#1). Called on every Moved/Resized; the disk write itself
    /// waits until the user stops (`about_to_wait`), so a drag isn't a write storm.
    /// No-op in fullscreen — that geometry is the monitor, not a user-chosen spot.
    fn track_windowed_geometry(&mut self) {
        if !self.core.windowed {
            return;
        }
        let before = self.core.settings.window;
        self.capture_windowed_geometry();
        if self.core.settings.window != before {
            self.core.geometry_save_at = Some(Instant::now() + Duration::from_millis(500));
        }
    }

    /// Build the native menu bar once (cross-platform; muda owns the OS handle).
    // Called only from the Windows/macOS window-setup paths; unreached on Linux (no native menu).
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    fn ensure_menu(&mut self) {
        if self.menu.is_none() {
            let built = menu::build_menu(&self.core.keymap);
            self.menu = Some(built.menu);
            self.save_rotation_item = Some(built.save_rotation);
            self.reveal_item = Some(built.reveal);
            self.cancel_scan_item = Some(built.cancel_scan);
            self.undo_item = Some(built.undo);
            self.compare_pin_item = Some(built.compare_pin);
            self.compare_toggle_item = Some(built.compare_toggle);
            self.subtitle_tracks_menu = Some(built.subtitle_tracks);
            self.audio_tracks_menu = Some(built.audio_tracks);
            self.view_checks = Some(built.checks);
        }
    }

    /// Rebuild the Playback ▸ Subtitle Track flyout's rows for the file on screen (task #99).
    ///
    /// **The one menu item whose *contents* are runtime state**, because the track list is a
    /// property of the file rather than of the app. macOS gets this for free — its
    /// `NSMenuDelegate.menuNeedsUpdate` fires as the menu opens, so the list is built from
    /// whatever is on screen at that instant and there is nothing to invalidate. muda has no
    /// such hook (the tree is built once; `apply_menu_to_native` only mirrors checkmarks onto
    /// it), so this runs on the tick instead and guards itself with a cheap signature — a
    /// tuple compare, no allocation — because the alternative is formatting a `Vec<String>`
    /// of track labels every frame behind a playing video.
    // Windows/macOS only: there is no native menu on Linux (its egui bar has no submenus,
    // so `Shift+C` stays the route there).
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    fn refresh_subtitle_track_menu(&mut self) {
        if self.subtitle_tracks_menu.is_none() {
            return;
        }
        let sig = SubtitleMenuSig {
            item: self.core.displayed_item,
            video_showing: self.core.video_showing(),
            tracks_known: self.core.subtitle_tracks_known(),
            active: self.core.subtitles.active_track(),
            on: self.core.subtitles_on(),
        };
        if self.subtitle_menu_sig == Some(sig) {
            return;
        }
        self.subtitle_menu_sig = Some(sig);

        // Only *now* — past the guard — is it worth asking the core for real rows.
        let rows = menu::subtitle_track_rows(
            &self.core.subtitle_picker_rows(),
            sig.video_showing,
            sig.tracks_known,
        );
        // `PB_SUBTITLE_TRACE=1` — what the flyout would show, without opening it. A native
        // menu's contents are otherwise invisible to everything except a human with a mouse,
        // which makes "is the list right?" the one question about this feature that no test
        // can answer.
        self.core
            .subtitles
            .trace(|| format!("menu: subtitle track flyout = {rows:?}"));
        let Some(flyout) = self.subtitle_tracks_menu.as_ref() else {
            return;
        };
        // muda has no `remove_all`, and `remove_at` shifts the rest down — so drain from 0.
        while flyout.remove_at(0).is_some() {}
        for row in rows {
            let appended = match row {
                menu::TrackRow::Separator => flyout.append(&muda::PredefinedMenuItem::separator()),
                // A note is an item that says why there is nothing to pick. Disabled: it is an
                // explanation, not an offer.
                menu::TrackRow::Note(text) => {
                    flyout.append(&muda::MenuItem::with_id(text, text, false, None))
                }
                menu::TrackRow::Track { row, label, active } => {
                    let id = format!("{}{row}", menu::ids::SUBTITLE_TRACK_PREFIX);
                    flyout.append(&muda::CheckMenuItem::with_id(id, label, true, active, None))
                }
            };
            if let Err(e) = appended {
                eprintln!("menu: failed to append a subtitle track row: {e}");
            }
        }
    }

    /// Rebuild the Playback ▸ Audio Track flyout's rows (task #99) — the audio twin of
    /// [`Self::refresh_subtitle_track_menu`], same signature guard, same "state lives in
    /// the contents, never the holder's enabled flag" rule.
    #[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
    fn refresh_audio_track_menu(&mut self) {
        if self.audio_tracks_menu.is_none() {
            return;
        }
        let sig = AudioMenuSig {
            item: self.core.displayed_item,
            video_item: self.core.displayed_is_video(),
            tracks_known: self.core.audio_tracks_known(),
            active: self
                .core
                .audio_active
                .map(|id| (id.catalog_generation, id.local_id)),
        };
        if self.audio_menu_sig == Some(sig) {
            return;
        }
        self.audio_menu_sig = Some(sig);

        let rows = menu::audio_track_rows(
            &self.core.audio_picker_rows(),
            sig.video_item,
            sig.tracks_known,
        );
        // Same `PB_SUBTITLE_TRACE=1` window the subtitle flyout gets — a native menu's
        // contents are otherwise invisible to everything but a human with a mouse.
        self.core
            .subtitles
            .trace(|| format!("menu: audio track flyout = {rows:?}"));
        let Some(flyout) = self.audio_tracks_menu.as_ref() else {
            return;
        };
        while flyout.remove_at(0).is_some() {}
        for row in rows {
            let appended = match row {
                menu::TrackRow::Separator => flyout.append(&muda::PredefinedMenuItem::separator()),
                menu::TrackRow::Note(text) => {
                    flyout.append(&muda::MenuItem::with_id(text, text, false, None))
                }
                menu::TrackRow::Track { row, label, active } => {
                    let id = format!("{}{row}", menu::ids::AUDIO_TRACK_PREFIX);
                    flyout.append(&muda::CheckMenuItem::with_id(id, label, true, active, None))
                }
            };
            if let Err(e) = appended {
                eprintln!("menu: failed to append an audio track row: {e}");
            }
        }
    }

    /// Switch the playing video's audio to picker row `row` (task #99) — the winit half
    /// of `CoreEffect::SelectAudioTrack`, also called directly by the flyout. The row is
    /// translated to the engine's own currency (Windows: an MF reader stream index, via
    /// MF's catalog or the FFmpeg→MF bridge; Linux: an FFmpeg stream index). A row the
    /// engine can't reach is refused immediately — the core toasts the failure and the
    /// tick stays on what is actually playing, never the request.
    fn select_audio_track(&mut self, row: usize) {
        // The row's two possible currencies (task #99): the engine serves whichever
        // its decoders can, FFmpeg first (Windows falls back to MF; Linux is FFmpeg
        // end-to-end and ignores `mf`).
        let ff = self.core.audio_row_ff_stream(row);
        let mf = self.core.audio_row_mf_stream(row);
        let seq = match &self.video_audio {
            Some(audio) if ff >= 0 || mf >= 0 => audio.set_track(ff, mf),
            // No engine: the flyout lists tracks over the poster too (they are facts
            // about the file), but a switch needs a playing session — say that,
            // rather than the "couldn't switch" that implies something broke.
            None => {
                self.core.show_toast_icon(
                    "Play the video to switch audio tracks",
                    pb_app_core::ToastIcon::AudioTrackFailed,
                );
                return;
            }
            // No locator in any currency: a switch that cannot happen must not
            // pretend to be in flight.
            _ => {
                self.core.audio_track_switched(row, false);
                return;
            }
        };
        self.pending_audio_switch = Some((seq, row));
    }

    /// Derive the current [`contract::MenuState`] from live app state and, **only when it
    /// changed** since the last one applied (`self.menu_state`), emit a single
    /// [`CoreEffect::SetMenuState`] — the shell mirrors it onto the native menu in the drain
    /// (`apply_menu_to_native`). This is the core side of the menu seam (NS0, ADR-021): the
    /// core decides *what* the menu should read; it never touches a muda handle. The change
    /// gate keeps this off the per-tick path (nothing is pushed when nothing moved), so it's
    /// safe to call every tick from `about_to_wait`; a no-op until the menu exists.
    fn apply_menu_state(&mut self) {
        // Linux: the windowed menu is the egui overlay bar (no muda handles / `view_checks`).
        // The bar reads the live state directly in `render_overlay_frame`; here we only cache it
        // + re-render the bar when it changes (there's nothing native to mirror onto).
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let next = self.current_menu_state();
            if self.menu_state.as_ref() != Some(&next) {
                self.menu_state = Some(next);
                self.overlay_dirty = true;
            }
        }
        // Win/mac: mirror the change onto the native (muda) menu via a single `SetMenuState`
        // effect the drain applies (`apply_menu_to_native`) — only when it actually moved, so
        // this stays off the per-tick path.
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            // No menu yet (not built): nothing to mirror, and don't cache — so the first apply
            // once the items exist re-asserts every one of them from scratch. All menu handles
            // are built together in `ensure_menu`, so `view_checks` gates them all.
            if self.view_checks.is_none() {
                return;
            }
            let next = self.current_menu_state();
            if self.menu_state.as_ref() == Some(&next) {
                return;
            }
            self.menu_state = Some(next.clone());
            self.core
                .effects
                .push(contract::CoreEffect::SetMenuState(next));
        }
    }

    /// Compute the [`contract::MenuState`] for the current live view/edit state — the pure
    /// mapping shared by [`apply_menu_state`](Self::apply_menu_state) (native menu sync on
    /// Win/mac) and the Linux egui menu bar (`render_overlay_frame`), so both read one truth.
    fn current_menu_state(&self) -> contract::MenuState {
        // Native (Spaces) fullscreen is a macOS-only concept; Windows/Linux have no such
        // item, so it's always `false`.
        let native_fullscreen = false;

        let mut state = AppCore::menu_state_from(
            self.core.view.mode,
            self.core.info_line,
            self.core.panels,
            self.core.folder_tree_open,
            self.core.recursive,
            !self.core.windowed, // `windowed` is the inverse of the fullscreen checkbox
            self.core.slideshow.on,
            self.core.effective_mute(),
            self.core.settings.subtitles,
            self.core.can_save_rotation(),
            self.core.can_reveal(),
            self.dir_scan.is_some(),
            // `None` = nothing to undo (disabled "Undo"); `Some(label)` = enabled w/ label.
            self.core.undo_stack.last().map(UndoAction::menu_label),
            native_fullscreen,
            self.core.displayed_item,
            self.core.compare_pin,
        );
        // The docked toolbar (#61) is a shell-honored setting, not derived view state, so it's
        // set here (the choke point defaults it off). It rides `MenuState` so the View ▸ Show
        // Toolbar checkmark tracks it through the same per-tick diff as every other check.
        state.show_toolbar = self.core.settings.show_toolbar;
        // Show Archives (task #104) is a setting too, so the View ▸ Show Archives checkmark
        // tracks it through the same per-tick MenuState diff.
        state.show_archives = self.core.settings.show_archives;
        state
    }

    /// The shell side of [`CoreEffect::SetMenuState`]: mirror a [`contract::MenuState`] onto
    /// the live native (muda) menu handles. Applies every mirrored item unconditionally —
    /// the muda setters are idempotent and the core only emits the effect when the state
    /// actually changed (via `apply_menu_state`), so there's no per-tick cost and no need to
    /// re-diff here. A no-op for any item group whose handles aren't built yet (they're all
    /// created together in `ensure_menu`). This is the only place a menu handle is touched —
    /// the seam an AppKit shell re-implements.
    fn apply_menu_to_native(&self, state: &contract::MenuState) {
        // View-menu checkmarks (scale group / recursive / fullscreen / slideshow / info).
        if let Some(c) = self.view_checks.as_ref() {
            c.fit.set_checked(state.scale == contract::ScaleMode::Fit);
            c.fill.set_checked(state.scale == contract::ScaleMode::Fill);
            c.original
                .set_checked(state.scale == contract::ScaleMode::Original);
            c.recursive.set_checked(state.recursive);
            c.fullscreen.set_checked(state.fullscreen);
            c.slideshow.set_checked(state.slideshow);
            c.toolbar.set_checked(state.show_toolbar);
            c.show_archives.set_checked(state.show_archives);
            c.mute_live_audio.set_checked(state.mute_live_audio);
            c.subtitles.set_checked(state.subtitles);
            c.info.set_checked(state.info_basic);
            c.full_exif.set_checked(state.info_full);
            c.toggle_panels.set_checked(state.panels_hidden);
            c.toggle_panels.set_enabled(state.hide_panels_enabled);
        }
        // File ▸ Save Rotation enabled state.
        if let Some(it) = self.save_rotation_item.as_ref() {
            it.set_enabled(state.save_rotation_enabled);
        }
        // File ▸ Show in Finder/Explorer enabled state.
        if let Some(it) = self.reveal_item.as_ref() {
            it.set_enabled(state.reveal_enabled);
        }
        // File ▸ Stop Scanning enabled state.
        if let Some(it) = self.cancel_scan_item.as_ref() {
            it.set_enabled(state.cancel_scan_enabled);
        }
        // Image ▸ compare pair (task #43): the pin item enables with a photo shown and
        // checks while the displayed photo IS the pin; the flip enables once a pin exists.
        if let Some(it) = self.compare_pin_item.as_ref() {
            it.set_enabled(state.compare_pin_enabled);
            it.set_checked(state.compare_pinned_here);
        }
        if let Some(it) = self.compare_toggle_item.as_ref() {
            it.set_enabled(state.compare_toggle_enabled);
        }
        // Edit ▸ Undo title + enabled state (Windows appends the `\tCtrl+Z` hint).
        if let Some(it) = self.undo_item.as_ref() {
            let base = state.undo.as_deref().unwrap_or("Undo");
            it.set_text(format!("{base}\tCtrl+Z"));
            it.set_enabled(state.undo.is_some());
        }
    }

    /// The shell side of [`contract::CoreEffect::ShowContextMenu`] (task #41): build a fresh
    /// muda popup from the shell-neutral [`contract::ContextMenuState`] and show it at the
    /// cursor. The per-OS `show_context_menu_for_*` call is **synchronous** (TrackPopupMenu on
    /// Windows, `popUpMenuPositioningItem` on macOS) and posts any selection to the shared
    /// [`muda::MenuEvent`] channel *before* it returns — the same channel the menu bar uses —
    /// so the click is picked up by the next `about_to_wait` poll (→ `action_for` →
    /// `dispatch_action`), and the local menu only needs to outlive this call. `None` position
    /// = the current cursor location. The seam an AppKit host re-implements with an `NSMenu`.
    fn show_context_menu_native(&mut self, state: &contract::ContextMenuState) {
        let menu = menu::build_context_menu(state);
        let Some(window) = self.window.as_ref() else {
            return;
        };
        #[cfg(windows)]
        if let Some(hwnd) = hwnd_of(window) {
            use muda::ContextMenu;
            // SAFETY: `hwnd` is this live window's valid handle; `None` = show at the cursor.
            unsafe {
                let _ = menu.show_context_menu_for_hwnd(hwnd, None);
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = (menu, window);
    }

    /// Attach the menu bar in windowed mode, hide it in fullscreen. The menu is a
    /// windowed-only discoverability layer — fullscreen stays chrome-free. OS-drawn,
    /// so it costs nothing on the render hot path. (Windows now; macOS later mirrors
    /// this behind the same muda API.)
    #[cfg(windows)]
    fn apply_menu_for_mode(&mut self) {
        self.ensure_menu();
        let Some(hwnd) = self.window.as_ref().and_then(|w| hwnd_of(w)) else {
            return;
        };
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        // SAFETY: `hwnd` is the live window's handle for as long as `active` is set.
        unsafe {
            if self.core.windowed {
                if self.menu_attached {
                    let _ = menu.show_for_hwnd(hwnd);
                } else {
                    let _ = menu.init_for_hwnd(hwnd);
                    self.menu_attached = true;
                }
            } else if self.menu_attached {
                let _ = menu.hide_for_hwnd(hwnd);
            }
        }
    }

    /// Other platforms (Linux/X11/Wayland): no native menu wired yet.
    #[cfg(not(any(windows, target_os = "macos")))]
    fn apply_menu_for_mode(&mut self) {}

    /// React to a runtime OS light↔dark theme change or an Appearance-preference
    /// change: re-flush the popup menu themes and re-assert the bar's theme —
    /// `Auto` when following the OS, pinned `Light`/`Dark` otherwise (#46).
    #[cfg(windows)]
    fn refresh_menu_theme(&self) {
        darkmode::flush_menu_themes();
        if !self.menu_attached || !self.core.windowed {
            return;
        }
        let theme = match self.core.effective_appearance() {
            settings::AppearanceMode::System => muda::MenuTheme::Auto,
            settings::AppearanceMode::Light => muda::MenuTheme::Light,
            settings::AppearanceMode::Dark => muda::MenuTheme::Dark,
        };
        if let Some(a) = self.window.as_ref() {
            if let (Some(menu), Some(hwnd)) = (self.menu.as_ref(), hwnd_of(a)) {
                // SAFETY: the menu is attached to this live window's valid handle.
                unsafe {
                    let _ = menu.set_theme_for_hwnd(hwnd, theme);
                }
            }
        }
    }

    /// Push the Appearance preference onto the **OS-drawn chrome** — the DWM title
    /// bar (winit `set_theme`) and the native menu bar + its popup dropdowns — so a
    /// pinned Light/Dark never renders a themed dialog/HUD under a mismatched bar
    /// (`System` = follow the OS, the pre-#46 behavior). Checked every
    /// `about_to_wait` turn behind a change guard (one enum compare), so it catches
    /// a Settings save from any path.
    fn apply_chrome_theme(&mut self) {
        if self.window.is_none() {
            return;
        }
        let mode = self.core.effective_appearance();
        if self.applied_appearance == Some(mode) {
            return;
        }
        self.applied_appearance = Some(mode);
        let theme = match mode {
            settings::AppearanceMode::System => None,
            settings::AppearanceMode::Light => Some(winit::window::Theme::Light),
            settings::AppearanceMode::Dark => Some(winit::window::Theme::Dark),
        };
        if let Some(w) = self.window.as_ref() {
            w.set_theme(theme);
        }
        #[cfg(windows)]
        {
            // Popup menus are OS-drawn app-wide: force/unforce their scheme too.
            darkmode::set_app_mode(theme.map(|t| t == winit::window::Theme::Dark));
            self.refresh_menu_theme();
        }
    }

    /// Resolve the chrome accent from the chosen source (System / Custom / Brand) and push it to
    /// `pb_ui` — but only when the inputs changed (source, custom color, or the resolved theme,
    /// since the legibility guard is theme-dependent). Checked every `about_to_wait` turn behind
    /// that guard, so it catches a Settings save from any path and an OS light↔dark flip; on a
    /// real change it re-renders the overlay so open panels repaint in the new accent.
    fn apply_accent(&mut self) {
        let dark = self.core.hud_dark;
        let key = (
            self.core.settings.accent_source,
            self.core.settings.accent_custom,
            dark,
        );
        if self.applied_accent == Some(key) {
            return;
        }
        self.applied_accent = Some(key);
        accent::apply(&self.core.settings, dark);
        // Repaint the retained chrome (open panels) in the new accent. Dialogs rebuild their
        // palette on the next frame; the overlay needs an explicit re-render nudge.
        self.overlay_dirty = true;
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Run a menu action by mapping it to the central [`Action`] and dispatching it —
    /// the menu and the keyboard share one dispatcher, so they can never drift. The
    /// id→`MenuAction` mapping is the pure, unit-tested `menu::action_for`.
    fn dispatch_menu(&mut self, action: MenuAction) {
        self.core.dispatch_action(action.to_action());
    }

    /// Execute a [`contract::CoreEffect::ShellFlowAction`] — the **flow** arms of the action
    /// vocabulary the core routes to the shell (the dialogs / window mode / scan / file-edit /
    /// quit commands `AppCore::dispatch_action` doesn't own end-to-end yet). The result-mirror
    /// of that core method's flow branch; `_ =>` is unreachable (the core only routes the flow
    /// subset here) but keeps the match total. Runs from `drain_effects`. (NS0: replaced by
    /// specific effects/`CoreEvent`s + native macOS handling as 5.6 inverts each flow.)
    fn perform_flow_action(&mut self, action: Action) {
        match action {
            Action::DeletePermanent => self.confirm_delete_permanent(),
            Action::Recursive => self.toggle_recursive(),
            Action::ShowArchives => self.toggle_show_archives(),
            Action::CancelScan => self.cancel_scan_command(),
            Action::Quit => self.begin_exit(),
            // Toggle the docked windowed toolbar (#61): flip + persist the setting, then
            // re-reserve/free the photo's top inset and re-render so it appears/disappears
            // immediately (the same atomic swap the live Settings toggle uses).
            Action::ToggleToolbar => {
                self.core.settings.show_toolbar = !self.core.settings.show_toolbar;
                self.core.settings.save();
                self.refresh_chrome_inset();
            }
            _ => {}
        }
    }

    /// Core tail of the picker flow, run by the shell after the modal panel closes with the
    /// resolved [`LaunchInput`] (`None` = cancelled). Sets the Esc/held guard **first** (the
    /// modal ran its own message loop; the Esc/Enter that dismissed it can leak to our window
    /// as a stray key event — drop any keys it left "held", and guard Esc-to-quit briefly so
    /// cancelling the picker never closes PhotoBlaze), then opens the input if one was picked.
    fn finish_picker(&mut self, input: Option<LaunchInput>) {
        self.core.held.clear();
        self.core.esc_guard_until = Some(Instant::now() + Duration::from_millis(300));
        if let Some(input) = input {
            self.open_input(input);
        }
    }

    /// Open (or focus, if already open) one of our egui dialog windows. Only one
    /// dialog is shown at a time; requesting a different kind replaces it.
    fn open_dialog(&mut self, kind: dialog::DialogKind) {
        if let Some(d) = self.dialog.as_ref() {
            if d.kind() == kind {
                d.focus();
                return;
            }
        }
        self.pending_dialog = Some(DialogRequest::Simple {
            kind,
            message: String::new(),
            archive_optout: false,
        });
    }

    /// Open the themed (dark-aware egui) "Delete Permanently" confirmation for `name`.
    /// The actual deletion happens when the dialog answers Yes (see `dialog_event`),
    /// acting on `pending_confirm_delete`.
    fn open_confirm_delete(&mut self, name: &str) {
        let msg = format!("Permanently delete \u{2018}{name}\u{2019}?");
        self.pending_dialog = Some(DialogRequest::Simple {
            kind: dialog::DialogKind::Confirm,
            message: msg,
            archive_optout: false,
        });
    }

    /// Open a one-button informational / error notice (egui `DialogKind::Message`):
    /// a warning icon + `message` + an OK button, centered over the viewer, closing
    /// on OK / Esc. The archive-open path (`archive::ArchiveOpenError::user_message`)
    /// calls this to surface a too-large / corrupt / password / OOM / empty failure.
    pub fn open_message(&mut self, message: &str) {
        self.open_message_ex(message, false);
    }

    /// Like [`open_message`](Self::open_message) but with the `archive_optout` checkbox
    /// (task #104): the empty-archive path passes `true` so the "no images" notice also
    /// offers "Don't show archives", a one-click way to stop listing archives you keep
    /// running into. Every other caller uses the plain `open_message`.
    fn open_message_ex(&mut self, message: &str, archive_optout: bool) {
        self.pending_dialog = Some(DialogRequest::Simple {
            kind: dialog::DialogKind::Message,
            message: message.to_string(),
            archive_optout,
        });
    }

    /// Route an event for the dialog window (egui owns it). Esc / close button
    /// dismiss it; everything else feeds egui and triggers repaints.
    fn dialog_event(&mut self, event: WindowEvent) {
        // While the keybinding editor is capturing, route key events to it (so they
        // rebind a slot instead of closing the dialog or driving egui). Modifier state
        // is tracked always so the captured chord matches the viewer's. Non-key events
        // fall through to the normal handling below.
        if let Some(d) = self.dialog.as_mut() {
            d.note_modifiers(&event);
            if d.capturing_active() && d.handle_capture_event(&event) {
                d.request_redraw();
                return;
            }
        }
        let close = matches!(event, WindowEvent::CloseRequested)
            || matches!(
                &event,
                WindowEvent::KeyboardInput {
                    event: KeyEvent {
                        physical_key: PhysicalKey::Code(KeyCode::Escape),
                        state: ElementState::Pressed,
                        ..
                    },
                    ..
                }
            );
        if close {
            let kind = self.dialog.as_ref().map(|d| d.kind());
            self.route_dialog_outcome(DialogOutcome::Dismissed(kind));
            return;
        }
        // Render the dialog and pick up a button answer (if one was clicked).
        let kind = self.dialog.as_ref().map(|d| d.kind());
        let mut answer: Option<bool> = None;
        // A live Settings edit produced by this render frame (auto-save); routed below
        // once the `d` borrow is released.
        let mut live_edit: Option<(Option<Box<settings::Settings>>, Option<Keymap>)> = None;
        // The empty-archive dialog's "Don't show archives" checkbox toggled this frame (task
        // #104): applied *live* (behind the dialog) once the `d` borrow is released, so it's
        // independent of which window closes the modal or whether OK is ever clicked.
        let mut optout_change: Option<bool> = None;
        // Disjoint fields, borrowed apart: the Settings preview needs the core's rasterizer
        // while the dialog window is borrowed mutably. Destructuring is what makes that
        // legal — `self.dialog.as_mut()` alongside `self.core.subtitles` would not be.
        let App { dialog, core, .. } = self;
        if let Some(d) = dialog.as_mut() {
            let repaint = d.on_event(&event);
            match &event {
                WindowEvent::Resized(size) => {
                    d.resize(*size);
                    d.request_redraw();
                }
                WindowEvent::RedrawRequested => {
                    // Lend the app's one rasterizer to the Subtitles tab's live swatch.
                    // Asking for it here is also what *starts* the 261 ms font worker, so
                    // opening Settings pays that cost rather than a film's first cue.
                    d.render(core.subtitles.rasterizer_mut());
                    answer = d.take_confirm_result();
                    live_edit = d.take_settings_edit();
                    optout_change = d.take_hide_archives_change();
                }
                _ => {
                    if repaint {
                        d.request_redraw();
                    }
                }
            }
        }
        // Apply the archive opt-out the moment its checkbox changed — before any close path,
        // so checking the box hides archives even if the user then dismisses with Esc.
        if let Some(hide) = optout_change {
            self.apply_hide_archives(hide);
        }
        // Apply + persist a live Settings edit (the auto-save path — window stays open).
        if let Some((settings, keymap)) = live_edit {
            let prev_show_archives = self.core.settings.show_archives;
            self.route_dialog_outcome(DialogOutcome::SettingsEdited { settings, keymap });
            // The edit may have toggled the toolbar (task #61): re-reserve/free the photo's top
            // inset and re-render so the strip appears/disappears immediately (no restart).
            self.refresh_chrome_inset();
            // A Show Archives change (task #104) re-scans the current folder so the doors
            // appear/disappear at once — Settings behaves like the View ▸ Show Archives toggle,
            // not like Recursive (whose Settings entry is only a default for future opens).
            if self.core.settings.show_archives != prev_show_archives {
                self.rescan_current_folder();
            }
        }
        if let Some(confirmed) = answer {
            // Turn the button answer into an outcome, extracting any egui-side payload here
            // (password / edited settings + keymap); `route_dialog_outcome` then hands it to the
            // core (or, for a password submit, drives the archive worker shell-side).
            let outcome = match kind {
                Some(dialog::DialogKind::Password) if confirmed => {
                    DialogOutcome::PasswordSubmitted(
                        self.dialog
                            .as_mut()
                            .and_then(|d| d.take_submitted_password()),
                    )
                }
                Some(dialog::DialogKind::Password) => DialogOutcome::PasswordCancelled,
                Some(dialog::DialogKind::AskImage) if confirmed => DialogOutcome::AskSubmitted(
                    self.dialog.as_mut().and_then(|d| d.take_ask_result()),
                ),
                Some(dialog::DialogKind::AskImage) => DialogOutcome::Closed, // Ask cancel = close
                // Auto-save dialog: Done (or any close) just clears the open state —
                // every edit was already applied + persisted live via `live_edit` above.
                Some(dialog::DialogKind::Settings) => DialogOutcome::SettingsCancelled,
                Some(dialog::DialogKind::Loading) => DialogOutcome::LoadingCancelled,
                Some(dialog::DialogKind::Scanning) => DialogOutcome::ScanningCancelled,
                Some(dialog::DialogKind::Confirm) => DialogOutcome::ConfirmAnswered(confirmed),
                _ => DialogOutcome::Closed,
            };
            self.route_dialog_outcome(outcome);
        }
    }

    /// Map a [`DialogOutcome`] the shell extracted from the dialog window to the shell-neutral
    /// [`contract::DialogResult`] and hand it to the core via [`CoreEvent::DialogResolved`] (NS0
    /// 5.6). The core runs every reaction — including the password submit (it shows "Checking…" +
    /// re-opens the archive via `BeginArchiveOpen`) — and emits the close / cancel / begin effects,
    /// which the drain right after `dialog_event` (window_event) carries out.
    fn route_dialog_outcome(&mut self, outcome: DialogOutcome) {
        let result = match outcome {
            DialogOutcome::Dismissed(kind) => {
                contract::DialogResult::Dismissed(kind.map(core_dialog_kind))
            }
            DialogOutcome::PasswordSubmitted(pw) => contract::DialogResult::PasswordSubmitted(pw),
            DialogOutcome::PasswordCancelled => contract::DialogResult::PasswordCancelled,
            DialogOutcome::AskSubmitted(q) => {
                contract::DialogResult::AskSubmitted(q.unwrap_or_default())
            }
            DialogOutcome::SettingsEdited { settings, keymap } => {
                contract::DialogResult::SettingsEdited { settings, keymap }
            }
            DialogOutcome::SettingsCancelled => contract::DialogResult::SettingsCancelled,
            DialogOutcome::LoadingCancelled => contract::DialogResult::LoadingCancelled,
            DialogOutcome::ScanningCancelled => contract::DialogResult::ScanningCancelled,
            DialogOutcome::ConfirmAnswered(c) => contract::DialogResult::ConfirmAnswered(c),
            DialogOutcome::Closed => contract::DialogResult::Closed,
        };
        self.core
            .handle(contract::CoreEvent::DialogResolved(result));
    }

    /// The ambient **scan status card**: while a folder scan is streaming in (and the first
    /// photo is already up), show a fixed-width card in the top-right (equal inset from the top
    /// and right edges) — `Scanning "Folder"`, the folder currently being walked, the browsable
    /// count (`8,230 images found`), and a centered **Cancel Scan** button. The count *is* the
    /// progress (Codex P3: the **browsable** `source.len()`, not the worker's look-ahead
    /// `found`). Deferred past [`SCAN_DIALOG_DELAY`] so a quick folder never flashes it;
    /// rebuilt only when its content changes and no faster than [`SCAN_CARD_REFRESH`] (the
    /// current-folder line changes per directory); cleared when the scan ends.
    /// Whether the ambient scan pill should be on screen: a folder scan is streaming in, a
    /// photo is up, and the walk has outlasted [`SCAN_DIALOG_DELAY`] (so a fast folder never
    /// flashes it). The egui overlay draws it (the SwiftUI `ScanPillView` parity); the
    /// pre-bootstrap slow-scan case is still the separate `DialogKind::Scanning` window.
    fn scan_pill_visible(&self) -> bool {
        self.core.displayed_item.is_some()
            && self.core.scan_bootstrapped
            && self
                .dir_scan
                .as_ref()
                .is_some_and(|s| s.started.elapsed() >= SCAN_DIALOG_DELAY)
    }

    /// The scan pill's data for this overlay frame (heading name, browsable count, current
    /// sub-folder), or `None` when no pill is shown. Scan state is shell-owned (`dir_scan`),
    /// so the shell builds this rather than a core accessor.
    fn scan_pill_frame(&self) -> Option<panels_ui::ScanPill> {
        if !self.scan_pill_visible() {
            return None;
        }
        let scan = self.dir_scan.as_ref()?;
        // The current folder; blanked while it's just the root (it duplicates the heading).
        let cur = scan.progress.current();
        let current = if cur == scan.name { String::new() } else { cur };
        Some(panels_ui::ScanPill {
            name: scan.name.clone(),
            found: self.core.source.len(),
            current,
        })
    }

    /// Drive the egui scan pill each tick: when its content (visibility / folder / count)
    /// changes, mark the overlay dirty so it re-renders. Show/hide is immediate; a content
    /// tick (folder/count) is throttled by [`SCAN_CARD_REFRESH`] so a fast-streaming deck
    /// doesn't re-lay-out the overlay every batch. (The pill's own spinner already requests
    /// per-frame repaints while visible, so the live count stays current between ticks.)
    fn tick_chip(&mut self) {
        let want = self.scan_pill_frame().map(|s| (s.name, s.current, s.found));
        if want == self.core.chip_sig {
            return;
        }
        let toggling = want.is_some() != self.core.chip_sig.is_some();
        if !toggling && self.core.chip_built.elapsed() < SCAN_CARD_REFRESH {
            return;
        }
        self.core.chip_sig = want;
        self.core.chip_built = Instant::now();
        self.overlay_dirty = true;
    }

    /// Drive the egui play hint's flash / hold / fade (the `native_play` seam). The core bumps
    /// `play_hint_seq` on a fresh motion item; the shell flashes the pill, holds it
    /// [`PLAY_HINT_HOLD`] (pinned indefinitely while the pointer hovers it), then fades it out.
    /// Returns the pill's data for this overlay frame, and marks the overlay dirty while it's
    /// animating so the fade actually renders. `None` when nothing is shown.
    fn tick_play_hint(&mut self, now: Instant) -> Option<panels_ui::PlayHintFrame> {
        self.play_hint_wake = None;
        let kind = self.core.play_hint_kind();
        // Fresh motion item → flash.
        if kind != 0 && self.core.play_hint_seq != self.play_hint_seq {
            self.play_hint_seq = self.core.play_hint_seq;
            self.play_hint_shown = Some(now);
            self.play_hint_fade_out = None;
            self.play_hint_kind = kind;
        }
        let shown = self.play_hint_shown?;
        if kind != 0 {
            self.play_hint_kind = kind;
        }
        if self.play_hint_hovered && kind != 0 {
            // Hover pins it fully open and restarts the hold clock (so un-hover resumes the
            // countdown from now).
            self.play_hint_shown = Some(now - PLAY_HINT_FADE_IN);
            self.play_hint_fade_out = None;
        } else if kind == 0 && self.play_hint_fade_out.is_none() {
            // The item stopped being a motion item (played / advanced) → fade out.
            self.play_hint_fade_out = Some(now);
        } else if self.play_hint_fade_out.is_none()
            && now.duration_since(shown) >= PLAY_HINT_FADE_IN + PLAY_HINT_HOLD
        {
            // The hold elapsed → auto fade out.
            self.play_hint_fade_out = Some(now);
        }
        let alpha = match self.play_hint_fade_out {
            Some(fo) => {
                let a =
                    1.0 - now.duration_since(fo).as_secs_f32() / PLAY_HINT_FADE_OUT.as_secs_f32();
                if a <= 0.0 {
                    self.play_hint_shown = None;
                    self.play_hint_fade_out = None;
                    return None;
                }
                a
            }
            None => {
                (now.duration_since(shown).as_secs_f32() / PLAY_HINT_FADE_IN.as_secs_f32()).min(1.0)
            }
        };
        self.overlay_dirty = true; // keep re-rendering while it animates
                                   // Schedule the next wake so the animation self-drives — the Linux redraw self-pump
                                   // stalls, so without an explicit wake the pill freezes mid-fade until the next input
                                   // event. Fading in/out → next frame; holding fully open → the hold-expiry that starts
                                   // the fade-out; hover-pinned → none (a pointer event re-ticks it).
        const ANIM_FRAME: Duration = Duration::from_millis(16);
        self.play_hint_wake = if self.play_hint_fade_out.is_some() {
            Some(now + ANIM_FRAME)
        } else if self.play_hint_hovered && kind != 0 {
            None
        } else if now.duration_since(shown) < PLAY_HINT_FADE_IN {
            Some(now + ANIM_FRAME)
        } else {
            Some(shown + PLAY_HINT_FADE_IN + PLAY_HINT_HOLD)
        };
        Some(panels_ui::PlayHintFrame {
            kind: self.play_hint_kind,
            shortcut: self.core.shortcut_for(Action::PlayPause),
            alpha,
        })
    }

    /// Esc / window-close: shut down with a perceived-*instant* exit, writing
    /// nothing to disk (tasks #6 + #2). Order matters:
    /// 1. Hide the window FIRST, so it vanishes before the heavy frees — the close
    ///    always feels instant regardless of how long teardown takes.
    /// 2. Drop the RAM-only, photo-derived session state (no disk flush — the only
    ///    persistent thing PhotoBlaze touches is the photos it *reads*).
    /// 3. Exit the loop; `run_app` returns and `Drop` then frees the renderer
    ///    (VRAM) and joins the decode pool — all while the window is already gone.
    fn begin_exit(&mut self) {
        if let Some(a) = self.window.as_ref() {
            a.set_visible(false);
        }
        self.clear_session_state();
        // If a background update was downloaded this session, install it now — this exits the
        // process (the next launch is the new version). A no-op if nothing was staged.
        update::apply_on_quit();
        self.core.effects.push(contract::CoreEffect::Quit);
    }

    /// The shell side of [`CoreEffect::WriteClipboard`]: perform the platform clipboard
    /// write and surface the same success/failure toast the inline copy did (run from the
    /// drain, so still the same event-loop turn — the toast renders synchronously via
    /// `show_toast`). This is the only place the clipboard APIs are touched — the seam an
    /// AppKit shell re-implements with `NSPasteboard`. (NS-later: once `AppCore` is split
    /// out, the write result comes back as a `CoreEvent` and the core owns the toast.)
    fn write_clipboard(&mut self, payload: contract::ClipboardPayload) {
        match payload {
            contract::ClipboardPayload::Image { rgba, w, h, file } => {
                let wrote = match file {
                    Some(path) => clipboard::set_image_and_file(w, h, &rgba, &path),
                    None => clipboard::set_image(w, h, &rgba),
                };
                match wrote {
                    // Icon-only pill (the clipboard glyph says it all).
                    Ok(()) => self.core.show_toast_icon("", pb_app_core::ToastIcon::Copy),
                    Err(e) => {
                        eprintln!("copy: clipboard write failed: {e}");
                        self.core.show_toast("Copy failed");
                    }
                }
            }
            contract::ClipboardPayload::Text { text, toast } => {
                // The core supplies the toast when it knows better (recognized text:
                // "Copied 214 characters" / "+ 1 QR code"). Otherwise: a single-line
                // payload is a file path → name it ("Copied IMG…"); a multi-line one
                // is the EXIF blob → generic, since its "filename" is meaningless. A
                // path never contains a newline, so this is safe.
                let toast = toast.unwrap_or_else(|| {
                    if text.contains('\n') {
                        "Copied to clipboard".to_string()
                    } else {
                        format!("Copied {}", file_name_of(&text))
                    }
                });
                match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                    Ok(()) => self.core.show_toast(&toast),
                    Err(e) => {
                        eprintln!("copy text: clipboard write failed: {e}");
                        self.core.show_toast("Copy failed");
                    }
                }
            }
        }
    }

    /// Execute and clear the [`contract::CoreEffect`]s orchestration queued this
    /// iteration — the one place the core's intents become real winit/native calls (the
    /// seam an AppKit shell re-implements; NS0, ADR-021). Called at the end of each
    /// `ApplicationHandler` entry that can produce effects. More variants join as their
    /// call sites convert off direct winit access. The `drain` metric times this so a
    /// hold-to-blaze frame's total event-loop cost is `present + drain` (window ops the
    /// advance path used to do inline now land here — the total, not `present`, is flat).
    fn drain_effects(&mut self, event_loop: &ActiveEventLoop) {
        let t0 = Instant::now();
        // Drain until quiescent: a `ShellFlowAction` runs a shell flow method that can enqueue a
        // follow-up effect which must land the SAME event turn — e.g. Fullscreen→`SetWindowMode`
        // (else the mode flip lags a tick) and Quit→`Quit`. Bounded so a pathological re-push
        // can't spin the loop.
        let mut guard = 0;
        while !self.core.effects.is_empty() && guard < 16 {
            guard += 1;
            for effect in std::mem::take(&mut self.core.effects) {
                match effect {
                    contract::CoreEffect::Quit => event_loop.exit(),
                    contract::CoreEffect::RequestRender => {
                        if let Some(w) = self.window.as_ref() {
                            w.request_redraw();
                        }
                    }
                    contract::CoreEffect::SetTitle(title) => {
                        if let Some(w) = self.window.as_ref() {
                            w.set_title(&title);
                        }
                    }
                    contract::CoreEffect::SetCursor(kind) => {
                        // Don't apply the photo cursor directly — the shell is the single cursor
                        // writer (`resolve_cursor`, run once per tick after the overlay renders).
                        // It composes this core want with egui's hover cursor and the live
                        // pointer's resize-zone geometry, so the two never fight (the flicker).
                        self.core_cursor = kind;
                    }
                    contract::CoreEffect::SetMenuState(state) => {
                        self.apply_menu_to_native(&state);
                    }
                    // A rich panel's visibility / active tab / content changed (task #54):
                    // the winit shell presents Help / Inspector / tree via the egui overlay
                    // (`native_help`/`native_inspector`/`native_tree` are on), so re-render
                    // the panel texture next turn. Matched explicitly (no wildcard) to keep
                    // new variants a compile error.
                    contract::CoreEffect::PanelsChanged => {
                        self.overlay_dirty = true;
                    }
                    contract::CoreEffect::SetWindowMode(_mode) => {
                        self.apply_window_mode();
                    }
                    // Present a chrome dialog (About / Settings today; the payload-carrying kinds
                    // still open via their own flow paths). Maps the shell-neutral kind onto the
                    // shell's `dialog::DialogKind` and defers the actual window to `open_dialog`.
                    contract::CoreEffect::ShowDialog(kind) => {
                        self.open_dialog(shell_dialog_kind(kind));
                    }
                    // Close the open dialog window (NS0 5.6 — the ubiquitous dialog-outcome close).
                    contract::CoreEffect::CloseDialog => self.dialog = None,
                    // Put the password dialog into its "Checking…" state while validating.
                    contract::CoreEffect::SetDialogChecking => {
                        if let Some(d) = self.dialog.as_mut() {
                            d.set_checking(true);
                            d.request_redraw();
                        }
                    }
                    // Cancel the in-flight directory scan + drop its worker handle.
                    contract::CoreEffect::CancelScan => {
                        self.cancel_dir_scan();
                        self.dir_scan = None;
                    }
                    // Request the in-flight archive open stop (the poll frees it; no-op if none).
                    contract::CoreEffect::CancelArchiveLoad => self.cancel_archive_load(),
                    // Start the off-thread archive / folder-scan workers (NS0 5.6 Step 3c). The
                    // core routed the open; the shell owns the thread + progress dialog + generation.
                    contract::CoreEffect::BeginArchiveOpen { path, password } => {
                        self.begin_archive_open(path, password);
                    }
                    contract::CoreEffect::BeginDirScan { source, cursor } => {
                        self.begin_dir_scan(source, cursor);
                    }
                    contract::CoreEffect::WriteClipboard(payload) => {
                        self.write_clipboard(payload);
                    }
                    // Reveal the current photo in the OS file manager (macOS Finder / Windows
                    // File Explorer). The core already validated a real on-disk file; the shell
                    // runs the per-OS launch behind `reveal::in_file_manager`.
                    contract::CoreEffect::RevealPath(path) => {
                        reveal::in_file_manager(&path);
                    }
                    // Pop up the right-click photo context menu at the cursor (task #41).
                    contract::CoreEffect::ShowContextMenu(state) => {
                        self.show_context_menu_native(&state);
                    }
                    contract::CoreEffect::OpenFolderPanel { start_dir } => {
                        let input = rfd::FileDialog::new()
                            .set_directory(&start_dir)
                            .pick_folder()
                            .map(LaunchInput::Directory);
                        self.finish_picker(input);
                    }
                    contract::CoreEffect::OpenFilePanel { start_dir } => {
                        // Offer archives alongside images in the default filter (opening a zip
                        // to view its photos is the same use case), plus an All-files escape
                        // hatch. The picked paths go through `classify_inputs` — like drag-drop
                        // — so a single picked `.zip` opens as an archive instead of being
                        // mistaken for one file inside its folder.
                        let mut exts: Vec<&str> = IMAGE_FILTER_EXTS.to_vec();
                        exts.extend_from_slice(VIDEO_FILTER_EXTS);
                        exts.push("zip");
                        exts.push("7z");
                        // The tar family (#102). rfd matches on the FINAL extension,
                        // so `.tar.gz` needs the bare `gz` entry — a picked bare
                        // `photo.jpg.gz` classifies to None and is refused cleanly.
                        exts.extend_from_slice(&[
                            "tar", "tgz", "tbz2", "tbz", "tzst", "txz", "gz", "bz2", "zst", "xz",
                        ]);
                        // RAR5 + comic books (#103): .cbr = RAR, .cbz = ZIP.
                        exts.extend_from_slice(&["rar", "cbr", "cbz"]);
                        let input = rfd::FileDialog::new()
                            .add_filter("Images, videos & archives", &exts)
                            .add_filter("All files", &["*"])
                            .set_directory(&start_dir)
                            .pick_files()
                            .filter(|ps| !ps.is_empty())
                            .map(classify_inputs);
                        self.finish_picker(input);
                    }
                    // Live Photo audio (task #38): the core decides when/where; the shell owns the
                    // ObjC `AVAudioPlayer`. A no-op on non-macOS (the stub player returns None).
                    contract::CoreEffect::StartLiveAudio { path, at_secs } => {
                        self.live_audio = LiveAudio::play(&path, at_secs);
                    }
                    contract::CoreEffect::StopLiveAudio => self.live_audio = None,
                    contract::CoreEffect::PauseLiveAudio => {
                        if let Some(a) = &self.live_audio {
                            a.pause();
                        }
                    }
                    contract::CoreEffect::ResumeLiveAudio => {
                        if let Some(a) = &self.live_audio {
                            a.resume();
                        }
                    }
                    // Video audio (task #79 phase 5): the shell owns the WinRT player;
                    // the core decides when. Opens PAUSED — the core resumes it
                    // together with the video preroll.
                    contract::CoreEffect::StartVideoAudio {
                        input,
                        session_id,
                        muted,
                    } => {
                        self.video_audio_ready = false; // fast sampling until it opens
                        self.pending_audio_switch = None; // a switch can't outlive its session
                        self.audio_row_reported = -1;
                        self.video_audio = video_audio::VideoAudio::open(&input, session_id, muted);
                        if self.video_audio.is_none() {
                            // No player (creation failed / platform stub): tell the
                            // session once so it degrades to silent immediately
                            // instead of waiting out the readiness timeout.
                            self.core
                                .video_audio_clock(pb_app_core::video::AudioClockSample {
                                    session_id,
                                    state: pb_app_core::video::AudioClockState::Failed,
                                    position: Duration::ZERO,
                                    sampled_at_monotonic: Duration::ZERO,
                                });
                        }
                    }
                    // Audio track selection (task #99): translate the row to the
                    // engine's currency and ask it to switch; the outcome comes back
                    // through the sample tick (`switch_result`) and only a confirmed
                    // switch toasts. `A` / `Shift+A` and the Playback ▸ Audio Track
                    // flyout both land here.
                    contract::CoreEffect::SelectAudioTrack { row } => self.select_audio_track(row),
                    contract::CoreEffect::StopVideoAudio => {
                        self.video_audio = None;
                        self.pending_audio_switch = None;
                        self.audio_row_reported = -1;
                    }
                    contract::CoreEffect::PauseVideoAudio => {
                        if let Some(a) = &self.video_audio {
                            a.pause();
                        }
                    }
                    contract::CoreEffect::ResumeVideoAudio => {
                        if let Some(a) = &self.video_audio {
                            a.resume();
                        }
                    }
                    contract::CoreEffect::SetVideoAudioMuted(muted) => {
                        if let Some(a) = &self.video_audio {
                            a.set_muted(muted);
                        }
                    }
                    contract::CoreEffect::SeekVideoAudio { position } => {
                        if let Some(a) = &self.video_audio {
                            a.seek(position);
                        }
                    }
                    // macOS-native video (task 79.9): on macOS the whole media pipeline is the
                    // SwiftUI host's `AVPlayer`, so these commands are emitted ONLY when the core
                    // holds a `Native` video backend — which this winit shell never constructs (it
                    // drives the Windows/Linux `VideoSession` + its separate audio player above).
                    // Matched explicitly (the no-wildcard rule) and inert here.
                    contract::CoreEffect::PlayVideo { .. }
                    | contract::CoreEffect::PlayVideoBytes { .. }
                    | contract::CoreEffect::PlaySampleBuffer { .. }
                    | contract::CoreEffect::RequestVideoPoster { .. }
                    | contract::CoreEffect::PauseVideo { .. }
                    | contract::CoreEffect::ResumeVideo { .. }
                    | contract::CoreEffect::SeekVideoBy { .. }
                    | contract::CoreEffect::SeekVideoFraction { .. }
                    | contract::CoreEffect::StepVideo { .. }
                    | contract::CoreEffect::SetVideoMuted { .. }
                    | contract::CoreEffect::StopVideo { .. }
                    | contract::CoreEffect::CaptureNativeVideoFrame { .. } => {}
                    // The core routed a flow action (dialog / window / scan / file edit / quit) it
                    // doesn't own end-to-end yet — run the shell half.
                    contract::CoreEffect::ShellFlowAction(action) => {
                        self.perform_flow_action(action);
                    }
                    // The core's requested next wake (from the Tick handler). Stored, not applied
                    // here — `about_to_wait` mins it with the shell's dialog-repaint deadline for
                    // the event loop's control-flow.
                    contract::CoreEffect::SetWake(at) => self.requested_wake = at,
                    // Not emitted by the core today, but matched explicitly (no `_`
                    // wildcard) so a new effect variant is a compile error here rather
                    // than a silently swallowed no-op.
                    contract::CoreEffect::HideWindow => {
                        if let Some(a) = self.window.as_ref() {
                            a.set_visible(false);
                        }
                    }
                    contract::CoreEffect::ReportError(msg) => self.open_message(&msg),
                }
            }
        }
        self.open_pending_dialog(event_loop);
        self.core.metrics.record("drain", t0.elapsed());
    }

    /// Open the dialog an opener method deferred (see [`App::pending_dialog`]). The one
    /// place a `DialogWindow` is created — where the shell owns the `ActiveEventLoop` — so
    /// orchestration only records *what* to open. Mirrors the old inline opens exactly.
    fn open_pending_dialog(&mut self, event_loop: &ActiveEventLoop) {
        let Some(req) = self.pending_dialog.take() else {
            return;
        };
        let refresh = self.core.refresh_hz();
        let parent = self.window.clone();
        match req {
            DialogRequest::Simple {
                kind,
                message,
                archive_optout,
            } => {
                let mut dlg = dialog::DialogWindow::open(
                    kind,
                    event_loop,
                    refresh,
                    &message,
                    &self.core.settings,
                    &self.core.keymap,
                    parent.as_deref(),
                );
                // The empty-archive notice (task #104) grows a "Don't show archives" checkbox.
                if archive_optout {
                    if let Some(d) = dlg.as_mut() {
                        d.enable_archive_optout();
                    }
                }
                self.dialog = dlg;
            }
            DialogRequest::Loading { message, progress } => {
                let mut dlg = dialog::DialogWindow::open(
                    dialog::DialogKind::Loading,
                    event_loop,
                    refresh,
                    &message,
                    &self.core.settings,
                    &self.core.keymap,
                    parent.as_deref(),
                );
                if let Some(d) = dlg.as_mut() {
                    d.set_progress(Some(progress));
                    d.request_redraw();
                }
                self.dialog = dlg;
            }
            DialogRequest::Scanning { message, progress } => {
                let mut dlg = dialog::DialogWindow::open(
                    dialog::DialogKind::Scanning,
                    event_loop,
                    refresh,
                    &message,
                    &self.core.settings,
                    &self.core.keymap,
                    parent.as_deref(),
                );
                if let Some(d) = dlg.as_mut() {
                    d.set_scan(&message, progress);
                }
                self.dialog = dlg;
            }
        }
    }

    /// Drop every RAM-backed, photo-derived cache: decoded-pixel residency, staged
    /// uploads, per-item metadata, per-image rotation overrides, and the
    /// failed/transient overlay state. Pure in-memory clears — **never a disk
    /// write** (privacy task #2). `Drop` would reclaim all of this on its own; doing
    /// it explicitly at teardown keeps the privacy guarantee auditable in one place.
    fn clear_session_state(&mut self) {
        // Abandon a still-running background scan so it stops walking on teardown.
        self.cancel_dir_scan();
        self.core.ring = ResidentRing::new(0);
        self.core.pending_uploads.clear();
        self.core.meta_cache.clear();
        self.core.exif_cache.clear();
        // Recognized text (OCR + QR, task #45) is pixel-derived — drop it with the
        // other RAM caches, along with any scan still in flight.
        self.core.recognized_text.clear();
        self.core.text_scan = None;
        // AI descriptions (task #44) are likewise pixel-derived — drop them + any
        // in-flight describe (privacy #2: nothing viewing-derived survives teardown).
        self.core.descriptions.clear();
        self.core.describe_scan = None;
        self.core.live_motion_cache.clear();
        self.core.rotations.clear();
        self.core.video_resume.clear();
        // Session archive passwords (session-archive-password-cache): wipe (zeroizing) at
        // teardown. Explicit, not just `Drop`, so it's auditable here with the other RAM
        // caches — and so it holds even if the process later exits without unwinding.
        self.core.clear_archive_passwords();
        self.core.failed.clear();
        self.core.preview_resident.clear();
        self.core.preview_watchdog = None;
        self.core.upgrade_done.clear();
        self.core.last_upgrade_set.clear();
        self.core.undo_stack.clear();
        self.core.current = None;
        self.core.toast = None;
        self.core.wait_started = None;
        self.core.pie_finish = None;
        self.core.pie_glow_started = None;
        self.core.folder_tree_open = false;
        self.core.folder_tree_sig = None;
        self.core.folder_tree_panel = None;
        // The full archive source (a solid 7z holds its decompressed bytes) is a
        // RAM cache like the rest — drop it with them.
        self.core.archive_scope = None;
        // Drop any on-demand animation playback + in-flight decode (RAM-only — #2).
        self.core.stop_playback();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Stamp the injected clock (NS0 5.5) before any core method runs on this entry.
        self.core.now = Instant::now();
        if self.core.renderer.is_some() {
            return;
        }

        if let Some(hz) = event_loop
            .primary_monitor()
            .and_then(|m| m.refresh_rate_millihertz())
        {
            let hz = hz as f64 / 1000.0;
            println!("display refresh: {hz:.2} Hz");
            if hz > 0.0 {
                self.core.frame_interval = Duration::from_secs_f64(1.0 / hz);
            }
        }

        // Create the window HIDDEN first, so decode-to-fit can target its real
        // client size and the unpainted area never shows during GPU setup; let
        // the OS size fullscreen (correct under any scale factor).
        let mut attrs = Window::default_attributes()
            .with_title(pb_app_core::APP_NAME)
            .with_visible(false);
        if let Some(icon) = load_window_icon() {
            attrs = attrs.with_window_icon(Some(icon));
        }
        // The saved windowed geometry to restore on launch, if it still lands on a
        // connected monitor (#1) — used as the creation hint and re-applied after the
        // menu attaches below (so the client size accounts for the menu bar).
        let restore = if self.core.windowed {
            let rects = collect_monitor_rects(event_loop.available_monitors());
            self.core.windowed_restore(&rects)
        } else {
            None
        };
        attrs = if self.core.windowed {
            match restore {
                Some(g) => attrs
                    .with_inner_size(PhysicalSize::new(g.w, g.h))
                    .with_position(PhysicalPosition::new(g.x, g.y)),
                None => attrs.with_inner_size(PhysicalSize::new(1280, 800)),
            }
        } else {
            // Borderless "windowed fullscreen": a decoration-less window sized to
            // the monitor — NOT the OS fullscreen API (which triggers Windows
            // fullscreen-optimizations and the legacy basic-theme caption flash).
            let mut a = attrs.with_decorations(false);
            if let Some(mon) = event_loop.primary_monitor() {
                a = a.with_inner_size(mon.size()).with_position(mon.position());
            }
            a
        };

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );
        // Point the window icon at the exe's multi-size .ico so the small title-bar
        // size is its purpose-rendered 16px bitmap, not a crude downscale of one
        // big image (winit's `Icon` is single-size). See apply_native_window_icon.
        #[cfg(windows)]
        apply_native_window_icon(&window);

        // Attach the windowed-mode menu bar *before* measuring the client size, so
        // the first decode-to-fit already accounts for the menu's height (no soft
        // first frame). Fullscreen stays menu-free.
        #[cfg(windows)]
        {
            // Opt the app + window into the OS dark/light theme first, so muda's
            // `Auto` renders the menu (bar + dropdowns) to match the desktop rather
            // than always-light. Must precede the menu's first paint.
            darkmode::init_app();
            if let Some(hwnd) = hwnd_of(&window) {
                darkmode::allow_for_window(hwnd);
            }
            self.ensure_menu();
            if self.core.windowed {
                if let (Some(menu), Some(hwnd)) = (self.menu.as_ref(), hwnd_of(&window)) {
                    // SAFETY: `hwnd` is this freshly-created window's valid handle.
                    unsafe {
                        let _ = menu.init_for_hwnd(hwnd);
                    }
                    self.menu_attached = true;
                }
            }
        }

        // Now that the menu bar is attached, re-apply the saved client size + position
        // so the restored window matches what was saved exactly — attaching the menu
        // shrinks the client area, so sizing only pre-attach would lose its height
        // each launch (#1). No-op when there's nothing to restore.
        if let Some(g) = restore {
            let _ = window.request_inner_size(PhysicalSize::new(g.w, g.h));
            window.set_outer_position(PhysicalPosition::new(g.x, g.y));
        }

        self.core.viewport.scale_factor = window.scale_factor() as f32;
        let isz = window.inner_size();
        self.core.viewport.width = isz.width.max(1);
        self.core.viewport.height = isz.height.max(1);
        self.core.fit = Some(FitBox {
            max_width: isz.width.max(1),
            max_height: isz.height.max(1),
        });

        // Decode the first image at the display size while the window is hidden.
        let (rgba, iw, ih, color, hdr, peak, title) = self.core.initial_image();
        window.set_title(&title);

        let mut renderer = WgpuRenderer::new(
            window.clone(),
            isz.width.max(1),
            isz.height.max(1),
            &rgba,
            iw,
            ih,
            color,
            hdr,
            peak,
        );
        // Seed the OS theme before the first frame paints, so the Appearance preference
        // resolves against the real desktop theme (#46; `None` = no OS signal → dark),
        // then apply the user's saved letterbox color for that resolved theme AND
        // retint the HUD compositor now — the empty-state open panel is rasterized
        // below, before the first present, and it must not flash the dark scheme on a
        // light desktop. (`refresh_theme` skips the letterbox: `core.renderer` isn't
        // installed yet, hence the direct set here.)
        self.core.os_dark = window.theme() != Some(winit::window::Theme::Light);
        renderer.set_letterbox(self.core.effective_letterbox());
        self.core.refresh_theme();
        let now = window.inner_size();
        if now != isz {
            self.core.fit = Some(FitBox {
                max_width: now.width.max(1),
                max_height: now.height.max(1),
            });
            renderer.resize(now.width, now.height);
            // The real window size differs from what we decoded for, but we do NOT re-decode
            // on the event loop (task #18 finding #5): the GPU refits the first frame to the
            // corrected size, and `request_prefetch` (below) re-decodes the current item at
            // the right fit off-thread and presents it in place when ready.
        }

        // Empty launch (no folder/file given): a blank background instead of an image.
        // The egui overlay draws the Open File / Open Folder welcome (on the first tick).
        if self.core.playlist.current().is_none() {
            renderer.clear_image();
        }

        // Reserve the top strip for the in-client chrome (Linux menu bar + toolbar, task #61)
        // so even the first (hidden) frame places the photo below it — no top-edge clip on
        // reveal. No-op in fullscreen / when the toolbar is off.
        renderer.set_content_top_inset(self.chrome_inset_px());

        // Present the first frame WHILE HIDDEN, then reveal — no white startup gap.
        let _ = renderer.render();
        window.set_visible(true);
        window.request_redraw();

        // Phase 3 engine: size the resident ring to the display and start filling
        // it. The first frame is already up via the single-image path; navigation
        // switches to the ring.
        let fit = self.core.fit.unwrap_or(FitBox {
            max_width: 1,
            max_height: 1,
        });
        let cap = ring_capacity(self.core.slot_bytes_estimate());
        self.core.ring = ResidentRing::new_with_budget(cap, RING_BUDGET_BYTES);
        renderer.reserve_ring(cap, fit.max_width, fit.max_height);
        let (ahead, behind) = window_for_capacity(cap);
        self.core.ahead = ahead;
        self.core.behind = behind;
        self.core.displayed_item = self.core.playlist.current();
        self.core.target_item = self.core.playlist.current();
        self.core.last_present = Some(Instant::now());

        self.window = Some(window);
        self.core.renderer = Some(Box::new(renderer));
        // Build the egui rich-panel overlay now the renderer (device/queue) exists: it
        // shares the renderer's device and draws the panels into an offscreen texture the
        // renderer composites over the photo (task #54 Phase 4).
        if let Some(win) = self.window.clone() {
            let size = win.inner_size();
            let dark = self.core.hud_dark;
            if let Some(dev) = self.core.renderer.as_ref().map(|r| r.device()) {
                self.egui_overlay = Some(egui_overlay::EguiOverlay::new(
                    &win,
                    dev,
                    dark,
                    size.width,
                    size.height,
                ));
            }
        }
        // Re-run the theme application now that the renderer lives in the core (the
        // early call above retinted the HUD; this one lands the letterbox through the
        // core-owned path), and push the Appearance preference onto the OS chrome
        // (title bar + menu) — a pinned Light/Dark must match from the first frame.
        self.core.refresh_theme();
        self.apply_chrome_theme();
        self.apply_accent();
        // Live OS-accent updates: re-resolve the chrome accent when the user changes their
        // system accent color, no restart needed (Windows; a no-op elsewhere). Subscribe once.
        if self.accent_watcher.is_none() {
            if let Some(w) = self.window.clone() {
                self.accent_watcher = accent::watch_system_accent(w);
            }
        }
        self.core.request_prefetch();

        // Now that the window + engine are live, kick off any launch we deferred (an archive
        // or a folder scan): a big .7z loads behind the spinner, a folder streams in (window
        // shows first), and an encrypted / failed open can use the egui dialogs (a synchronous
        // launch resolve, before the event loop, could do none of these).
        if let Some(plan) = self.pending_launch.take() {
            self.core.launching = false; // the deferred launch is firing now
            self.core.open_plan(plan.source, plan.cursor);
        }
        // Run the launch effects (BeginArchiveOpen / BeginDirScan) now rather than leaving
        // them queued for the first `about_to_wait` — the worker exists before the first
        // Tick/polls run instead of one turn later.
        self.drain_effects(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // Stamp the injected clock once per event (NS0 5.5); the core reads `self.core.now`.
        self.core.now = Instant::now();
        // Events for our egui dialog window go to egui, not the photo viewer.
        if self.dialog.as_ref().map(|d| d.id()) == Some(id) {
            self.dialog_event(event);
            self.drain_effects(event_loop);
            return;
        }
        // Feed the egui rich-panel overlay the events it needs. Two classes:
        //   • pointer/scroll — only while a panel is open, and if egui takes it (the
        //     pointer is over a panel) we swallow it so it interacts with the panel
        //     instead of panning/zooming the photo;
        //   • DPI/size changes — always, so egui's `pixels_per_point` + screen rect track
        //     a monitor move (its own offscreen target is resized separately below).
        // Keyboard is deliberately NOT routed to egui: nav keys, panel hotkeys, and
        // Esc-quits stay with the app, and the held-key KeyUp net must never be swallowed
        // (a stuck blaze is the worst bug here). Panels have no inline text inputs (ADR-023),
        // so egui needs no keyboard focus.
        let panel_open = self.overlay_panel_visible();
        let mut overlay_consumed = false;
        if let (Some(ov), Some(win)) = (self.egui_overlay.as_mut(), self.window.clone()) {
            let pointer = matches!(
                &event,
                WindowEvent::CursorMoved { .. }
                    | WindowEvent::CursorLeft { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
            );
            let track = matches!(
                &event,
                WindowEvent::ScaleFactorChanged { .. } | WindowEvent::Resized(..)
            );
            if (panel_open && pointer) || track {
                let resp = ov.on_window_event(&win, &event);
                if resp.repaint {
                    self.overlay_dirty = true;
                }
                overlay_consumed = panel_open && pointer && resp.consumed;
            }
            // Track the live pointer + whether it's over a panel (exact, lag-free), so the
            // shell's `resolve_cursor` flips ownership the instant the pointer crosses the pane
            // edge from the photo side — not a render later. Deriving this from egui's stored
            // (one-frame-late) pointer was the right-to-left edge flicker.
            match &event {
                WindowEvent::CursorMoved { position, .. } => {
                    self.last_pointer = Some((position.x, position.y));
                    self.pointer_over_panel =
                        panel_open && ov.physical_point_over_area(position.x, position.y);
                }
                WindowEvent::CursorLeft { .. } => {
                    self.pointer_over_panel = false;
                    self.last_pointer = None;
                }
                _ => {}
            }
        }
        if overlay_consumed {
            self.drain_effects(event_loop);
            return;
        }
        // While a menu dropdown is open, a click egui didn't take (i.e. not on the
        // bar/dropdown/panels) closes the menu and must NOT also reach the photo
        // (drag-to-pan / tree-click) — native menus eat their closing click.
        #[cfg(all(unix, not(target_os = "macos")))]
        if self.menu_nav.open.is_some()
            && matches!(
                event,
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    ..
                }
            )
        {
            self.menu_nav.open = None;
            self.menu_nav.sel = None;
            self.touch_overlay();
            self.drain_effects(event_loop);
            return;
        }
        match event {
            WindowEvent::CloseRequested => self.begin_exit(),

            WindowEvent::Resized(size) => {
                // A minimize reports a 0×0 client area. It is not a resize to react to — doing so
                // would reconfigure + re-lay-out the overlay + draw for a window nobody can see,
                // and (see `AppCore::resize`) clamp the fit to 1×1, which flashes a solid color on
                // restore. Ignore it; the resident texture and fit survive, so restore is instant.
                if size.width == 0 || size.height == 0 {
                    return;
                }
                // Resize + re-lay-out the egui overlay *before* `handle_resized`'s
                // synchronous redraw, so the frame it presents composites the overlay at the
                // new size instead of stretching the previous (old-size) texture across the
                // new viewport. The overlay is drawn via a fullscreen triangle that maps the
                // texture 1:1 to the viewport, so a size mismatch visibly stretches the panels
                // during a live resize drag. egui already recorded the new size from the same
                // event above (the `track` branch), so the re-layout reflows correctly.
                self.resize_overlay(size.width, size.height);
                self.handle_resized(size.width, size.height);
            }

            // The window's backing scale factor changed — a move to a monitor with a
            // different DPI (a 1× display ↔ a 2× Retina one), or a live OS DPI change. Route it
            // through the same `Resized` handler at the current size + new scale: the core updates
            // the scale factor + rescales every CPU-rasterized overlay so its text stays crisp.
            // The `Resized` winit sends right after reconfigures the swapchain + re-decodes.
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.core.handle(contract::CoreEvent::Resized {
                    width: self.core.viewport.width,
                    height: self.core.viewport.height,
                    scale: scale_factor as f32,
                });
            }

            // Track the windowed position so toggling back / relaunching restores it
            // (#1). A fullscreen window's position is the monitor, not a user choice,
            // so `track_windowed_geometry` ignores it there.
            WindowEvent::Moved(_) => {
                self.track_windowed_geometry();
            }

            WindowEvent::RedrawRequested => self.core.draw(),

            // Drag-and-drop: winit sends one event per file. Coalesce and apply on
            // the next `about_to_wait` tick (a folder browses recursively; dropped
            // photos become the playlist).
            WindowEvent::DroppedFile(path) => {
                self.pending_drops.push(path);
                // Take keyboard focus: a drop leaves the drag source (Explorer)
                // foreground, so nav keys would silently go nowhere until a click
                // (owner-reported; the macOS shell needs its own AppKit activate —
                // focus is per-shell, only the core is shared).
                if let Some(w) = self.window.as_ref() {
                    w.focus_window();
                }
                self.core.effects.push(contract::CoreEffect::RequestRender);
            }

            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state,
                        repeat,
                        ..
                    },
                ..
            } => match state {
                ElementState::Pressed => {
                    // Linux windowed menu bar: the keyboard drives the menus GTK-style —
                    // Alt+mnemonic (Alt+F → File) or F10 opens, arrows navigate, Enter
                    // activates, Esc closes the menu (instead of quitting the app). While
                    // a dropdown is open it grabs every key *press* (native menu
                    // behavior); key *releases* still flow to the core's held-key tracker
                    // below, so a blaze can never strand.
                    #[cfg(all(unix, not(target_os = "macos")))]
                    if self.menu_bar_visible() {
                        match self.menu_key(code) {
                            menu::MenuKeyOutcome::Ignored => {}
                            menu::MenuKeyOutcome::Consumed => {
                                self.touch_overlay();
                                self.drain_effects(event_loop);
                                return;
                            }
                            menu::MenuKeyOutcome::Activate(action) => {
                                self.touch_overlay();
                                self.dispatch_menu(action);
                                self.drain_effects(event_loop);
                                return;
                            }
                        }
                    }
                    if code == KeyCode::Escape {
                        // A dialog is open but the main window kept keyboard focus:
                        // Esc dismisses the dialog (cancelling any pending confirm),
                        // never the app. (Normally the focused dialog window swallows
                        // Esc itself in `dialog_event`.)
                        if self.dialog.is_some() {
                            self.dialog = None;
                            self.core.pending_confirm_delete = None;
                            // Same leak guard as the dialog path: a held/repeated Esc
                            // after this close must not fall through to quit.
                            self.core.esc_guard_until =
                                Some(Instant::now() + Duration::from_millis(300));
                            return;
                        }
                        // Swallow a stray Esc that leaked from dismissing the file
                        // picker (open_picker); a real Esc a moment later still quits.
                        let quit = esc_quits(self.core.esc_guard_until, Instant::now());
                        self.core.esc_guard_until = None;
                        if quit {
                            self.begin_exit();
                        }
                    } else if self.dialog.is_some() {
                        // A dialog is open but the main window kept keyboard focus — Windows
                        // often refuses to move focus to a dialog spawned in response to the
                        // very keypress that opened it (the empty-archive "no images" case:
                        // `P` opens the door, the door is empty, the Message dialog appears
                        // while `P`'s window stays focused). The dialog is *modal*, so no key
                        // may fall through to the viewer — that was the reported bug, where
                        // Enter/Space to click OK also advanced the photo. Enter/Space answer a
                        // Message dialog (its only button is OK), the same as the focused dialog
                        // window would; every other key is swallowed here rather than driving
                        // navigation. (Esc is handled above; Password/Ask/Settings need typing,
                        // which reaches them only when the dialog window actually has focus.)
                        let is_message = self.dialog.as_ref().map(|d| d.kind())
                            == Some(dialog::DialogKind::Message);
                        if is_message
                            && matches!(
                                code,
                                KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space
                            )
                        {
                            self.dialog = None;
                        }
                        // Anything else: swallow. A modal dialog never drives the viewer.
                    } else if let Some(key) = pb_key_winit::from_winit(code) {
                        // Translate to a shell-neutral `CoreEvent` and let the core resolve +
                        // route it (`handle`: repeat-gate + ⌘-no-fall-through, then one-shot →
                        // `dispatch_action`, nav → hold-to-blaze, held → track, frame-step). This is
                        // the SAME entry point the macOS Swift host drives (NS0 Phase C2). Keys the
                        // keymap can't name map to `None` and are ignored. `mods` is the tracked
                        // modifier state (updated by `ModifiersChanged`).
                        self.core.handle(contract::CoreEvent::KeyDown {
                            key,
                            mods: self.core.mods,
                            repeat,
                        });
                    }
                }
                ElementState::Released => {
                    if let Some(key) = pb_key_winit::from_winit(code) {
                        self.core.handle(contract::CoreEvent::KeyUp { key });
                    }
                }
            },

            // OS light↔dark theme switched at runtime: re-theme the native menu so
            // it keeps matching the desktop (the window title bar is winit's), and
            // tell the core so `Appearance: System` re-resolves the HUD + letterbox
            // live (#46).
            WindowEvent::ThemeChanged(t) => {
                #[cfg(windows)]
                self.refresh_menu_theme();
                self.core.handle(contract::CoreEvent::OsThemeChanged {
                    dark: t == winit::window::Theme::Dark,
                });
                // Re-theme the egui overlay to match (its panels track the OS scheme like
                // the HUD + dialogs); the core resolves `hud_dark` from the same signal.
                let dark = self.core.hud_dark;
                if let Some(ov) = self.egui_overlay.as_mut() {
                    ov.set_theme(dark);
                }
                self.overlay_dirty = true;
            }

            // Track the modifier state for chord building (Shift+R, Ctrl+R, Alt+Enter, ⌘…).
            WindowEvent::ModifiersChanged(mods) => {
                let s = mods.state();
                self.core.mods = contract::Modifiers {
                    ctrl: s.control_key(),
                    shift: s.shift_key(),
                    alt: s.alt_key(),
                    // `super_key()` is Cmd (⌘) on macOS, the Windows key elsewhere.
                    logo: s.super_key(),
                };
                // Holding Alt reveals the menu-bar mnemonic underlines (GTK style).
                #[cfg(all(unix, not(target_os = "macos")))]
                if self.menu_nav.alt_hint != s.alt_key() {
                    self.menu_nav.alt_hint = s.alt_key();
                    if self.menu_bar_visible() {
                        self.touch_overlay();
                    }
                }
            }

            // Focus loss can swallow the key-up event; clear held keys so
            // navigation never gets stuck auto-advancing (a known winit repeat /
            // lost-key-up hazard, called out in CLAUDE.md).
            WindowEvent::Focused(false) => {
                // The core clears the held set + gesture accumulators + any stuck drag (the
                // focus-loss release net) — same entry point the Swift host uses (NS0 Phase C2).
                self.core.handle(contract::CoreEvent::FocusLost);
            }

            // Track the pointer (anchor for pinch/wheel zoom) and, while the left
            // button is held, drag-to-pan: move the image by the cursor delta.
            WindowEvent::CursorMoved { position, .. } => {
                self.core.handle(contract::CoreEvent::PointerMoved {
                    x: position.x as f32,
                    y: position.y as f32,
                });
            }

            // Pointer left the window: drop any folder-tree hover so it doesn't stay lit. (The
            // welcome/play-hint/scan-pill hovers are egui's now.)
            WindowEvent::CursorLeft { .. } => {
                self.core.last_cursor = None;
                self.core.update_tree_hover();
            }

            // Left button toggles drag-to-pan (the cross-platform pan gesture).
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let pressed = state == ElementState::Pressed;
                // The interactive on-image controls (welcome Open buttons, play hint, scan-pill
                // Cancel) are all egui buttons handled in the overlay now — a press over any egui
                // panel is swallowed before this event by `overlay_consumed`. So here only the
                // folder-tree click and drag-to-pan remain.
                if pressed && self.core.folder_tree_click() {
                    // A folder-tree row opened a folder / a "… n more" marker paged.
                } else {
                    self.core.dragging = pressed;
                    self.core.refresh_cursor();
                }
            }

            // Right button opens the photo context menu (task #41) — the common per-photo
            // commands, at the cursor. Works in the borderless fullscreen speed mode too,
            // where the menu bar is hidden. Shown on press; the core decides the item set.
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                self.core.show_context_menu();
            }

            // Trackpad pinch (macOS): magnify about the cursor. `delta` is the
            // incremental magnification (+ spread to zoom in, − pinch to zoom out).
            WindowEvent::PinchGesture { delta, .. } => {
                self.core.handle(contract::CoreEvent::Pinch {
                    delta: delta as f32,
                });
            }

            // Trackpad two-finger double-tap (macOS "smart magnify"): toggle 100%,
            // sharing the keyboard's `0` / menu toggle so they can't drift.
            WindowEvent::DoubleTapGesture { .. } => {
                self.core.handle(contract::CoreEvent::DoubleTap);
            }

            // Scroll. macOS sends pixel-precise `PixelDelta` (a trackpad two-finger swipe);
            // Windows reports both a real mouse wheel and a precision-trackpad swipe as
            // `LineDelta` (and a macOS *mouse* wheel is `LineDelta` too). Both honor the same
            // `Scroll wheel` setting — pan by default, or zoom — with Ctrl always flipping to
            // the other action. So the setting is live on a Mac trackpad too (it used to be
            // ignored there, hard-wired to pan); the default stays Pan, and Ctrl+swipe zooms.
            WindowEvent::MouseWheel { delta, .. } => {
                let scroll = match delta {
                    MouseScrollDelta::PixelDelta(p) => contract::ScrollDelta::Pixels {
                        x: p.x as f32,
                        y: p.y as f32,
                    },
                    MouseScrollDelta::LineDelta(x, y) => contract::ScrollDelta::Lines { x, y },
                };
                self.core.handle(contract::CoreEvent::Scroll(scroll));
            }

            _ => {}
        }
        self.drain_effects(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Stamp the injected clock once per tick (NS0 5.5): the core reads `self.core.now`
        // instead of calling `Instant::now()`, so all timing this tick is consistent.
        self.core.now = Instant::now();
        let now = self.core.now;
        // Fire a queued toast (the toolbar's delayed fullscreen "Press F to exit" hint) once its
        // delay elapses — but only if we're still in fullscreen (a quick exit within the delay
        // would make it stale). The core tick below then owns its fade via `toast_active`.
        if self
            .pending_toast
            .as_ref()
            .is_some_and(|(at, _)| now >= *at)
        {
            let (_, msg) = self.pending_toast.take().unwrap();
            if !self.core.windowed {
                self.core.show_toast(&msg);
            }
        }
        // A background download finished: tell the user once (it installs when they quit).
        if update::newly_ready() {
            self.core
                .show_toast("Update ready. It installs when you quit.");
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
        // Keep the OS-drawn chrome (title bar, menu) on the Appearance preference;
        // no-ops unless the preference changed (e.g. a Settings save this turn).
        self.apply_chrome_theme();
        // Same for the chrome accent (System/Custom/Brand) — no-op unless the source, custom
        // color, or resolved theme changed (a Settings save, or an OS light↔dark flip). An OS
        // accent-color change (Windows `ColorValuesChanged`) isn't in the guard key, so clear it
        // to force a re-resolve of the live System accent this turn.
        if accent::take_os_accent_changed() {
            self.applied_accent = None;
        }
        self.apply_accent();
        // 0. Native menu-bar clicks (windowed mode). Map each id to the same action
        // the keyboard triggers and dispatch it; an unknown/foreign id is ignored.
        while let Ok(ev) = muda::MenuEvent::receiver().try_recv() {
            // A Playback ▸ Subtitle Track row (task #99) — the one menu id that isn't a
            // constant, because the rows are per-file. It carries a picker row rather than a
            // verb, so it can't go through `dispatch_menu`: `Action` is the *keyboard's*
            // fixed vocabulary, and a track pick has an argument. Same core call the Mac
            // host's `pickSubtitleTrack` and the playback bar's popover make.
            if let Some(row) = menu::subtitle_track_row(ev.id.as_ref()) {
                self.core.select_subtitle_row(row);
                self.menu_state = None;
                self.subtitle_menu_sig = None;
            } else if let Some(row) = menu::audio_track_row(ev.id.as_ref()) {
                // A Playback ▸ Audio Track row — same shape, but the tick does NOT move
                // here: the shell attempts the switch and the engine confirms it (#99);
                // the flyout re-ticks when the report lands. The sig reset is what undoes
                // muda's auto-flipped checkmark on the clicked row — without it a refused
                // switch left every clicked row checked at once (owner-hit 2026-07-17).
                self.select_audio_track(row);
                self.menu_state = None;
                self.audio_menu_sig = None;
            } else if let Some(action) = menu::action_for(ev.id.as_ref()) {
                self.dispatch_menu(action);
                // muda auto-flips a clicked CheckMenuItem's native checkmark, which can
                // desync from the real state (e.g. clicking the already-active scale mode
                // unchecks it though nothing changed). Invalidate the cache so the refresh
                // below re-asserts the true state unconditionally.
                self.menu_state = None;
            }
        }
        // Mirror the live app state onto the whole native menu in one diffed pass — the
        // View checkmarks, Save Rotation / Stop Scanning / Undo enabled+label, and (macOS)
        // the native-fullscreen title. Cached field-by-field, so this per-tick call is a
        // no-op unless something actually changed.
        self.apply_menu_state();
        // The Playback ▸ Subtitle Track flyout's *rows* (task #99) — not state to mirror but
        // a list to rebuild, since the tracks belong to the file on screen. Its own signature
        // guard, so this is a tuple compare on all but the few ticks where the list moved.
        #[cfg(any(windows, target_os = "macos"))]
        self.refresh_subtitle_track_menu();
        #[cfg(any(windows, target_os = "macos"))]
        self.refresh_audio_track_menu();
        // 0b. Apply any files dropped on the window this burst (coalesced — winit
        // delivers one `DroppedFile` per file).
        if !self.pending_drops.is_empty() {
            let drops = std::mem::take(&mut self.pending_drops);
            self.open_input(classify_inputs(drops));
        }
        // 0b′. Apply any paths forwarded by a secondary launch (task #14): an Explorer
        // double-click / multi-select on an already-running PhotoBlaze. Same path as a
        // drop (classify → open_input), plus raise the window to the front — the OS gave
        // us foreground rights (AllowSetForegroundWindow on the sender) so this sticks.
        #[cfg(windows)]
        {
            let forwarded = single_instance::take_forwarded();
            if !forwarded.is_empty() {
                println!(
                    "{}: opened {} forwarded path(s)",
                    pb_app_core::APP_NAME,
                    forwarded.len()
                );
                if let Some(w) = self.window.as_ref() {
                    w.set_minimized(false);
                    w.focus_window();
                }
                self.open_input(classify_inputs(forwarded));
                self.core.effects.push(contract::CoreEffect::RequestRender);
            }
        }
        // 0c. Video audio clock bridge (task #79 phase 5): sample the player's
        // position/state into the core — the master clock while audio plays.
        // Cadence is adaptive (phase 7): FAST while the player is still opening,
        // because preroll waits on "audio ready" and a 250 ms grid would quantize
        // P → first-frame; ~4 Hz once it's up (plenty for the clock).
        if let Some(va) = &self.video_audio {
            // Fast while opening (preroll waits on "audio ready") and while a track
            // switch is in flight (its confirmation — tick + toast — rides this poll).
            let cadence = if self.video_audio_ready && self.pending_audio_switch.is_none() {
                Duration::from_millis(250)
            } else {
                Duration::from_millis(30)
            };
            if self.video_audio_sampled_at.elapsed() >= cadence {
                self.video_audio_sampled_at = Instant::now();
                if let Some(sample) = va.sample() {
                    self.video_audio_ready =
                        !matches!(sample.state, pb_app_core::video::AudioClockState::Opening);
                    self.core.video_audio_clock(sample);
                }
                // Audio track selection (task #99): pick up a finished switch, and keep
                // the picker's tick pinned to what the engine is ACTUALLY decoding —
                // the mac host's `reportActiveAudioStream` rule, ported. Active first,
                // then the outcome toast, so the toast names the new track.
                let switch = self
                    .pending_audio_switch
                    .and_then(|(seq, row)| va.switch_result(seq).map(|ok| (row, ok)));
                // The engine reports its playing stream in whichever currency its
                // decoder speaks; resolve through the matching accessor. Reported
                // EVERY sample tick, never deduped: the stored id is generation-
                // scoped, and a re-probe mints a new generation — a dedupe on the
                // row number froze the tick out permanently after one (owner-hit).
                let (ff, mf) = (va.active_ff_stream(), va.active_mf_stream());
                let active_row = if ff >= 0 {
                    self.core.audio_row_for_ff_stream(ff)
                } else {
                    self.core.audio_row_for_mf_stream(mf)
                };
                if active_row != self.audio_row_reported {
                    self.audio_row_reported = active_row;
                    if std::env::var("PB_AUDIO_TRACE").is_ok() {
                        eprintln!("[pb-audio] shell: active row -> {active_row} (ff={ff} mf={mf})");
                    }
                }
                self.core.set_active_audio_row(active_row);
                if let Some((row, ok)) = switch {
                    self.pending_audio_switch = None;
                    self.core.audio_track_switched(row, ok);
                }
            }
        }
        // 0c. Pick up a finished background archive open (.7z eager decompress) or
        // directory scan (large/nested folder walked off the event loop).
        self.poll_archive_load();
        self.poll_dir_scan();

        // Sync the core-owned mirrors of the shell flow state this tick reads: whether a chrome
        // dialog is up (the slideshow pauses under one) and whether an archive is still loading
        // (keeps `work_pending` polling). Synced after the polls so a just-finished archive/scan
        // is reflected; a dialog opened later this tick (in the drain) applies next tick.
        self.core.dialog_open = self.dialog.is_some();
        self.core.archive_loading = self.archive_load.is_some();

        // The ambient scan-count chip (below the loading pie) while a folder scan streams in.
        // A host-side overlay update, independent of the core photo tick.
        self.tick_chip();

        // Pump an open dialog's egui animation clock (immediate-mode: a combo popup / spinner /
        // hover fade only advances when a frame is requested) and surface its next repaint
        // deadline. Host-side — a second winit window + egui, which the macOS host renders
        // natively instead.
        let dialog_repaint = self.dialog.as_ref().and_then(|d| d.repaint_at());
        if let Some(at) = dialog_repaint {
            if now >= at {
                if let Some(d) = self.dialog.as_ref() {
                    d.request_redraw();
                }
            }
        }
        let dialog_wake = dialog_repaint.filter(|&at| at > now);

        // The per-tick CORE loop (hold-to-blaze / slideshow / prefetch / animation) — the SAME
        // entry the macOS Swift host drives. It pushes `SetWake(core_wake)` (stored in
        // `self.requested_wake` by the drain below).
        self.core.handle(contract::CoreEvent::Tick(now));

        // Execute the tick's effects (SetWake → `requested_wake`, StopLiveAudio, any
        // ShellFlowAction, redraws, …). Must run before we read `requested_wake`.
        self.drain_effects(event_loop);

        // If the tick's dropped-frame retry (core `redraw_pending`) is stuck because the
        // swapchain size drifted from the window, re-assert the true size so presents resume —
        // otherwise the display freezes on the last frame until a manual resize.
        self.heal_surface_if_dropped();

        // Drive the egui play hint's flash/fade (after the core tick bumped `play_hint_seq`);
        // stash this frame's pill for `render_overlay_frame` and dirty the overlay while it
        // animates.
        self.play_hint_frame = self.tick_play_hint(now);

        // The archive door card (task #105). The overlay is retained and only rebuilt when
        // it is dirty, so adding the card to `PanelFrame` would never make it appear, change
        // as you cross to the next archive, or clear on the way back to a photo. Signature =
        // the **presented** door's item (never the playlist cursor, which runs ahead of the
        // screen); a change on either edge — photo→door, door→door, door→photo — rebuilds.
        // Allocation-free: `door_presented` exists so this per-frame poll costs nothing.
        let door_sig = self
            .core
            .door_presented()
            .then_some(self.core.displayed_item);
        if door_sig != self.door_sig {
            if door_diag() {
                eprintln!(
                    "[door-diag] shell door_sig {:?} -> {:?} (overlay_visible={} overlay_active={})",
                    self.door_sig,
                    door_sig,
                    self.overlay_visible(),
                    self.overlay_active,
                );
            }
            self.door_sig = door_sig;
            self.overlay_dirty = true;
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }

        // Hand the tick's subtitle overlay to the wgpu presenter (task #90.5). A cue change
        // needs no redraw request of its own: the only state this does anything in is a
        // playing video, which already draws every frame.
        self.present_subtitles();

        // Drive the egui rich-panel overlay: (re)render the panels into the offscreen
        // texture when they change (or an egui animation is due) and hand it to the
        // renderer, or clear it when nothing is open. Retained — a nav frame with a static
        // panel open re-renders nothing (the hot-path contract). Returns egui's next timed
        // repaint deadline for the wake calc.
        let overlay_wake = self.update_overlay(now);

        // Own the window cursor for this tick: compose egui's hover cursor, the core's photo
        // cursor, and the live pointer's resize-zone geometry into one authoritative value
        // (single writer — the two used to fight and flicker on the resize handle).
        self.resolve_cursor();

        // The event loop's next wake: the earliest of the core's requested wake, the shell's
        // own dialog-repaint deadline, and the overlay's egui repaint; `None` = idle.
        let wake = [
            self.requested_wake,
            dialog_wake,
            overlay_wake,
            self.play_hint_wake,
            // The delayed fullscreen hint's fire time (already-due ones fired above).
            self.pending_toast.as_ref().map(|(at, _)| *at),
        ]
        .into_iter()
        .flatten()
        .min();
        match wake {
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// Map the shell-neutral [`contract::CursorKind`] the core emits to the winit
/// [`CursorIcon`] the shell shows (NS0). `Hidden` is unused on the `SetCursor` path —
/// the chrome-free hide uses a separate mechanism — so it falls back to the arrow.
/// Map the shell-neutral [`contract::DialogKind`] the core emits (via `ShowDialog`) to the
/// shell's own [`dialog::DialogKind`] — a 1:1 mirror (NS0 5.6). The payload-carrying kinds
/// (Confirm/Message/Password/Loading/Scanning) currently reach the shell through their flow
/// paths, not `ShowDialog`, but the map is total so it stays correct as those invert.
fn shell_dialog_kind(kind: contract::DialogKind) -> dialog::DialogKind {
    match kind {
        contract::DialogKind::About => dialog::DialogKind::About,
        contract::DialogKind::Settings => dialog::DialogKind::Settings,
        contract::DialogKind::Confirm => dialog::DialogKind::Confirm,
        contract::DialogKind::Message => dialog::DialogKind::Message,
        contract::DialogKind::Password => dialog::DialogKind::Password,
        contract::DialogKind::AskImage => dialog::DialogKind::AskImage,
        contract::DialogKind::Loading => dialog::DialogKind::Loading,
        contract::DialogKind::Scanning => dialog::DialogKind::Scanning,
    }
}

/// The reverse of [`shell_dialog_kind`] — the shell's [`dialog::DialogKind`] → the shell-neutral
/// [`contract::DialogKind`] the core reasons about (NS0 5.6: carried on `DialogResult::Dismissed`
/// so the core can tell a Scanning dismiss from the others).
fn core_dialog_kind(kind: dialog::DialogKind) -> contract::DialogKind {
    match kind {
        dialog::DialogKind::About => contract::DialogKind::About,
        dialog::DialogKind::Settings => contract::DialogKind::Settings,
        dialog::DialogKind::Confirm => contract::DialogKind::Confirm,
        dialog::DialogKind::Message => contract::DialogKind::Message,
        dialog::DialogKind::Password => contract::DialogKind::Password,
        dialog::DialogKind::AskImage => contract::DialogKind::AskImage,
        dialog::DialogKind::Loading => contract::DialogKind::Loading,
        dialog::DialogKind::Scanning => contract::DialogKind::Scanning,
    }
}

fn cursor_icon(kind: contract::CursorKind) -> CursorIcon {
    match kind {
        contract::CursorKind::Default | contract::CursorKind::Hidden => CursorIcon::Default,
        contract::CursorKind::Grab => CursorIcon::Grab,
        contract::CursorKind::Grabbing => CursorIcon::Grabbing,
        contract::CursorKind::Pointer => CursorIcon::Pointer,
    }
}

/// Map egui's desired hover cursor to the winit one, for the over-a-panel case of the shell's
/// [`App::resolve_cursor`] (the panels use a small subset — pointer-hand, resize, text). The
/// resize handles are already owned geometrically there, so `ResizeHorizontal` here is just a
/// belt-and-braces mapping; everything unrecognized falls back to the arrow.
fn egui_cursor_to_winit(c: egui::CursorIcon) -> CursorIcon {
    use egui::CursorIcon as E;
    match c {
        E::PointingHand => CursorIcon::Pointer,
        E::ResizeHorizontal | E::ResizeColumn | E::ResizeEast | E::ResizeWest => {
            CursorIcon::EwResize
        }
        E::ResizeVertical | E::ResizeRow | E::ResizeNorth | E::ResizeSouth => CursorIcon::NsResize,
        E::Text | E::VerticalText => CursorIcon::Text,
        E::Grab => CursorIcon::Grab,
        E::Grabbing => CursorIcon::Grabbing,
        E::Crosshair => CursorIcon::Crosshair,
        E::NotAllowed | E::NoDrop => CursorIcon::NotAllowed,
        E::Move => CursorIcon::Move,
        E::Progress => CursorIcon::Progress,
        E::Wait => CursorIcon::Wait,
        E::Help => CursorIcon::Help,
        _ => CursorIcon::Default,
    }
}

/// The folder name shown in the Scanning dialog ("Scanning "name"…") — the first scan
/// root's own name, falling back to its full path for a root with no file name (e.g. `/`).
fn scan_display_name(source: &Source) -> String {
    if let Source::Scan { roots, .. } = source {
        if let Some(first) = roots.first() {
            return first
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| first.display().to_string());
        }
    }
    "folder".to_string()
}

/// The Scanning dialog's headline ("Scanning "name"…"), with typographic quotes/ellipsis
/// to match the loading dialog's "Opening "name"…".
fn scan_message(name: &str) -> String {
    format!("Scanning \u{201c}{name}\u{201d}\u{2026}")
}

/// Scan `dir` for supported images, sorted by full path — a thin synchronous wrapper
/// over [`collect_images`] used by the tests. Production callers go through
/// [`resolve_source`] (which calls `collect_images` directly with a [`ScanProgress`]
/// handle, on the off-thread scan worker).
#[cfg(test)]
fn scan_images(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    scan::collect_images(dir, recursive, true, None, &mut paths);
    paths.sort();
    paths
}

/// Extensions advertised in the file picker's "Images" filter (a hint only — the
/// user can still switch to "All Files"). A representative subset of what we decode.
const IMAGE_FILTER_EXTS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "gif", "bmp", "tif", "tiff", "webp", "tga", "qoi", "jxl",
    "svg", "svgz", "heic", "heif", "avif", "hdr", "exr", "arw", "nef", "cr2", "cr3", "dng", "raf",
    "rw2", "orf", "srw", "pef", "raw",
];

/// Video containers the library lists (task #79) — the picker offers them alongside
/// images. Mirrors `pb_app_core::video::VideoContainer`'s recognition list.
const VIDEO_FILTER_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "qt", "mkv", "webm", "avi", "wmv", "asf", "mpg", "mpeg", "mts", "m2ts",
    "3gp", "3g2",
];

/// On Windows, point the window's title-bar/taskbar icon at the multi-size icon
/// embedded in the .exe (`build.rs`), so each size is the purpose-rendered bitmap
/// from our `.ico` instead of Windows crudely downscaling one big image — which is
/// what winit's single-size `Icon` forces, and what mangles the 16px title-bar size.
#[cfg(windows)]
pub(crate) fn apply_native_window_icon(window: &Window) {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Shell::ExtractIconExW;
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, HICON, WM_SETICON};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // ICON_SMALL = 0 (title bar), ICON_BIG = 1 (alt-tab / taskbar).
    const ICON_SMALL: usize = 0;
    const ICON_BIG: usize = 1;

    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(h.hwnd.get() as *mut core::ffi::c_void);
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let wide: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut big = HICON::default();
    let mut small = HICON::default();
    unsafe {
        let n = ExtractIconExW(
            PCWSTR(wide.as_ptr()),
            0,
            Some(&mut big as *mut _),
            Some(&mut small as *mut _),
            1,
        );
        if n == 0 {
            return;
        }
        if !big.0.is_null() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_BIG)),
                Some(LPARAM(big.0 as isize)),
            );
        }
        if !small.0.is_null() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL)),
                Some(LPARAM(small.0 as isize)),
            );
        }
    }
}

/// The window / taskbar icon, decoded from the PNG embedded in the binary
/// (downscaled to 256²). `None` if decoding fails — the app just runs with the
/// default icon. (The .exe file icon for Explorer is embedded separately by
/// `build.rs`.)
pub(crate) fn load_window_icon() -> Option<Icon> {
    const PNG: &[u8] = include_bytes!("../icons/blazeviewer.png");
    let fit = FitBox {
        max_width: 256,
        max_height: 256,
    };
    let img = decode_bytes(PNG, Some(fit), false).ok()?;
    Icon::from_rgba(img.pixels, img.width, img.height).ok()
}

/// The window's Win32 `HWND` as an `isize` (what muda's `init_for_hwnd` expects),
/// via the same `RawWindowHandle::Win32` path as `apply_native_window_icon`.
#[cfg(windows)]
fn hwnd_of(window: &Window) -> Option<isize> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// Whether a path names an archive we open as a playlist — the one classifier
/// (`pb_source::archive_kind`): zip, 7z, and the tar family.
fn is_archive(p: &Path) -> bool {
    pb_source::archive_kind(p).is_some()
}

/// Classify launch / drop / picker paths into a [`LaunchInput`] — the one step
/// that touches the disk (an `fs::metadata` "file or folder?"). A lone directory
/// becomes `Directory`, a lone `.zip` becomes `Archive`; anything else collects
/// the files into `Files`.
fn classify_inputs(paths: Vec<PathBuf>) -> LaunchInput {
    let paths: Vec<PathBuf> = paths
        .into_iter()
        .filter(|p| !p.as_os_str().is_empty())
        .collect();
    if paths.is_empty() {
        return LaunchInput::Empty;
    }
    if paths.len() == 1 && fs::metadata(&paths[0]).map(|m| m.is_dir()).unwrap_or(false) {
        return LaunchInput::Directory(paths.into_iter().next().expect("len == 1"));
    }
    // A single archive opens to its contents rather than scanning its folder.
    if paths.len() == 1 && is_archive(&paths[0]) {
        return LaunchInput::Archive(paths.into_iter().next().expect("len == 1"));
    }
    // One or more files (a directory inside a multi-selection is uncommon and is
    // ignored here). If somehow every path is a directory, open the first.
    let files: Vec<PathBuf> = paths
        .iter()
        .filter(|p| !fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false))
        .cloned()
        .collect();
    if files.is_empty() {
        return LaunchInput::Directory(paths.into_iter().next().expect("non-empty"));
    }
    LaunchInput::Files(files)
}

/// All supported images under `root` in playlist order — the sorted image sequence the
/// streaming scan emits, collected eagerly. Used by the order-guarantee tests.
#[cfg(test)]
fn sorted_image_walk(root: &Path, recursive: bool) -> Vec<PathBuf> {
    scan::image_walker(root, recursive)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && scan::is_supported_image(e.path()))
        .map(walkdir::DirEntry::into_path)
        .collect()
}

/// An in-flight background **directory scan**. A large/recursive folder is walked off
/// the event loop — the synchronous walk used to block winit for seconds (macOS
/// beachball) and then crash when the unresponsive window/GPU surface was torn down
/// (opening `~/Library` was the report). It now **streams**: snapshots ride back over `rx`
/// as the walk descends (see [`ScanUpdate`]), tagged with `generation` so a superseded scan
/// (a newer open bumped `App::scan_gen`) is discarded; `progress.cancel` lets that newer
/// open — or quit, or the Cancel button — stop a giant walk early. Mirrors [`ArchiveLoad`].
struct DirScan {
    generation: u64,
    rx: std::sync::mpsc::Receiver<(u64, ScanUpdate)>,
    /// Shared count + current-folder progress and the cancel flag for the walk. The
    /// Scanning dialog reads it; Cancel / Esc / a superseding open / teardown flip its
    /// cancel flag so the walk bails at its next entry.
    progress: ScanProgress,
    /// The folder name shown in the Scanning dialog ("Scanning "name"…").
    name: String,
    /// When the scan was dispatched, so the Scanning dialog is deferred to slow scans
    /// only (a normal folder resolves in milliseconds and never flashes it).
    started: Instant,
}

/// An in-flight background archive open. A `.7z` is eager-decompressed off the event
/// loop; the [`Resolved`] (or error) rides back over `rx` tagged with `generation`,
/// so a superseded open (a newer one bumped `App::archive_gen`) is discarded.
struct ArchiveLoad {
    generation: u64,
    /// The worker's result: the open outcome plus the **auto-try winner** — the cached
    /// password (if any) that unlocked the archive, so the shell can promote it to MRU
    /// (session-archive-password-cache). `None` winner = unencrypted, or a user-entered
    /// password (which the shell already has in `attempted_password`), or a failure.
    #[allow(clippy::type_complexity)]
    rx: std::sync::mpsc::Receiver<(
        u64,
        (
            Result<Resolved, archive::ArchiveOpenError>,
            Option<pb_app_core::SecretString>,
        ),
    )>,
    /// The archive being opened, so a `PasswordRequired` result can re-prompt and
    /// re-open the same path with the entered password.
    path: PathBuf,
    /// The user-entered password this open carried, if any. `Some` means a repeat
    /// `PasswordRequired` was a wrong entry (so the prompt shows the retry error), and a
    /// success harvests it into the session cache. `None` on an initial (auto-try) open.
    attempted_password: Option<pb_app_core::SecretString>,
    /// Shared progress + cancel handle for this open. The loading dialog reads it to
    /// draw its bar; the Cancel button / Esc / a superseding open flips its cancel flag
    /// so the worker stops at the next entry boundary (freeing its partial RAM).
    progress: pb_source::OpenProgress,
}

/// The `--version` string: the crate version plus the git build id, matching the About box
/// (`env!("CARGO_PKG_VERSION")` + `option_env!("PB_BUILD_ID")`, stamped by `build.rs`).
fn cli_version() -> String {
    match option_env!("PB_BUILD_ID") {
        Some(id) => format!("{} ({id})", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Render a clap parse result (`--help` / `--version` / a usage error) and exit. With output
/// available we use clap's own formatting (stdout for help/version, stderr for errors). Without it
/// (a genuine GUI launch with no console — double-click / association) we pop a native dialog *only
/// for a real error* (`use_stderr`), so a bad flag isn't a silent failure; `--help` / `--version`
/// are pointless in a GUI, so those just exit quietly. Exit code is clap's own: 0 for help/version,
/// 2 for a usage error.
fn report_cli_error_and_exit(err: pb_cli::clap::Error, have_output: bool) -> ! {
    let code = err.exit_code();
    if have_output {
        let _ = err.print();
    } else if err.use_stderr() {
        show_startup_message_dialog(&err.render().to_string());
    }
    std::process::exit(code);
}

/// A native modal message for the no-console startup path (a parse error or a bad path from a
/// double-click / association launch). Uses the OS dialog directly — no event loop needed yet.
fn show_startup_message_dialog(text: &str) {
    rfd::MessageDialog::new()
        .set_title(pb_app_core::APP_NAME)
        .set_description(text)
        .set_level(rfd::MessageLevel::Warning)
        .show();
}

fn main() {
    // Velopack: installer lifecycle hooks + background auto-update. `velopack_startup` MUST run
    // before anything else — on an install/update/uninstall invocation it does its work and
    // exits. `start_background_check` kicks off a self-gating background update check (a no-op
    // for a dev run / not-yet-installed build); a downloaded update installs on quit.
    update::velopack_startup();
    update::start_background_check();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Hidden dev command: render the HUD component gallery to a PNG and exit before the event
    // loop ever starts (no window, no GPU). `--hud-gallery [out.png]` — the path is optional
    // (defaults next to the cwd). The companion of `cargo run -p pb-ui --example gallery`, but
    // for the on-image overlay layer; see `hud_gallery.rs`.
    if let Some(i) = args
        .iter()
        .position(|a| a == "--hud-gallery" || a.starts_with("--hud-gallery="))
    {
        let out = args[i]
            .split_once('=')
            .map(|(_, v)| v.to_string())
            .or_else(|| args.get(i + 1).filter(|a| !a.starts_with('-')).cloned())
            .unwrap_or_else(|| "hud-gallery.png".to_string());
        match hud_gallery::write_sheet(Path::new(&out)) {
            Ok(()) => println!(
                "{}: wrote HUD gallery \u{2192} {out}",
                pb_app_core::APP_NAME
            ),
            Err(e) => eprintln!("{}: HUD gallery failed: {e}", pb_app_core::APP_NAME),
        }
        return;
    }

    // Hidden dev command: render the egui rich panels (Help / Inspector / folder tree)
    // headlessly to a PNG and exit — the egui-overlay equivalent of `--hud-gallery`, for
    // previewing the panels without a live window. `--egui-shot [out.png] [--light]
    // [--tab=details|text|describe]`. See `egui_shot.rs`.
    if let Some(i) = args
        .iter()
        .position(|a| a == "--egui-shot" || a.starts_with("--egui-shot="))
    {
        let out = args[i]
            .split_once('=')
            .map(|(_, v)| v.to_string())
            .or_else(|| args.get(i + 1).filter(|a| !a.starts_with('-')).cloned())
            .unwrap_or_else(|| "egui-shot.png".to_string());
        let dark = !args.iter().any(|a| a == "--light");
        let tab = match args.iter().find_map(|a| a.strip_prefix("--tab=")) {
            Some("text") => pb_app_core::InspectorTab::Text,
            Some("describe") => pb_app_core::InspectorTab::Describe,
            _ => pb_app_core::InspectorTab::Details,
        };
        let welcome = args.iter().any(|a| a == "--welcome");
        match egui_shot::write_shot(Path::new(&out), dark, tab, welcome) {
            Ok(()) => println!(
                "{}: wrote egui panels \u{2192} {out}",
                pb_app_core::APP_NAME
            ),
            Err(e) => eprintln!("{}: egui shot failed: {e}", pb_app_core::APP_NAME),
        }
        return;
    }

    // Hidden dev command: render a Settings tab headlessly to a PNG and exit — the Settings
    // equivalent of `--egui-shot`. `--settings-shot [out.png] [--light]
    // [--tab=general|appearance|subtitles|shortcuts]`. See `egui_shot::write_settings_shot`.
    if let Some(i) = args
        .iter()
        .position(|a| a == "--settings-shot" || a.starts_with("--settings-shot="))
    {
        let out = args[i]
            .split_once('=')
            .map(|(_, v)| v.to_string())
            .or_else(|| args.get(i + 1).filter(|a| !a.starts_with('-')).cloned())
            .unwrap_or_else(|| "settings-shot.png".to_string());
        let dark = !args.iter().any(|a| a == "--light");
        let tab = args
            .iter()
            .find_map(|a| a.strip_prefix("--tab="))
            .unwrap_or("general");
        match egui_shot::write_settings_shot(Path::new(&out), dark, tab) {
            Ok(()) => println!("{}: wrote settings \u{2192} {out}", pb_app_core::APP_NAME),
            Err(e) => eprintln!("{}: settings shot failed: {e}", pb_app_core::APP_NAME),
        }
        return;
    }

    // Attach to the parent console (Windows GUI-subsystem builds) so clap's help / version /
    // error output is visible; elsewhere stdout already works. The return says whether output is
    // available (a console, pipe, or redirect) — which decides how a *real* parse error is
    // reported (stderr vs. a native dialog). Help / version never dialog.
    let have_output = win_console::attach_parent_console();

    // Parse the real CLI via the shared pb-cli parser. Help / version / bad input all come back
    // as a clap::Error (the lib never calls process::exit); we render + exit here.
    let cli = match pb_cli::parse_from(std::env::args_os(), &cli_version()) {
        Ok(cli) => cli,
        Err(e) => report_cli_error_and_exit(e, have_output),
    };
    let overrides = cli.to_overrides();

    // Saved preferences drive the launch defaults; the session overrides layer onto live state in
    // `apply_launch_overrides`. Window mode + recursive resolve here (pre-window): a flag wins,
    // else the saved preference. A fresh install (defaults) starts windowed.
    let startup_settings = settings::Settings::load();
    let windowed = overrides
        .windowed
        .unwrap_or_else(|| !startup_settings.start_fullscreen());
    let metrics_on = overrides.metrics;

    // Mixed strictness: a nonexistent positional path is a usage error (exit 2), reported to the
    // console when there is one, else a dialog — never a silent exit. `launch_paths()` folds in
    // the hidden `--pb-open` alias (macOS back-compat; accepted uniformly across shells).
    let launch_paths = cli.launch_paths();
    for p in &launch_paths {
        if !p.exists() {
            let msg = format!(
                "{}: no such file or folder: {}",
                pb_app_core::APP_NAME,
                p.display()
            );
            if have_output {
                eprintln!("{msg}");
            } else {
                show_startup_message_dialog(&msg);
            }
            std::process::exit(2);
        }
    }

    // Windows single-instance (task #14): unless `--new-window` opts out, the first launch becomes
    // the primary and every later launch (an Explorer double-click / multi-select) forwards its
    // paths to it and exits — reusing the one decode pool + VRAM ring instead of spawning a whole
    // new process per file. A secondary exits here, *before* the event loop / GPU / decode setup,
    // so the reuse costs it almost nothing. The `Primary` guard is kept alive for the process; the
    // IPC receiver is started after the event loop exists (it needs the loop's wake proxy).
    #[cfg(windows)]
    let _single_instance = if cli.new_window {
        None
    } else {
        match single_instance::acquire() {
            single_instance::Instance::Secondary => {
                if single_instance::forward(&single_instance::absolutize(&launch_paths)) {
                    // Delivered to the running instance — nothing more to do.
                    std::process::exit(0);
                }
                // The primary vanished (or never finished starting) before we could reach it: fall
                // through and run standalone rather than lose the open. We don't hold the mutex, so
                // we won't act as primary either.
                None
            }
            single_instance::Instance::Primary(guard) => Some(guard),
        }
    };

    // Every entry point (CLI, double-click via association, drag-drop, picker) funnels through the
    // same pure plan: classify the paths, decide the source + cursor, then scan. A bare launch
    // opens the empty state (nothing is auto-opened). A folder opens recursively by default;
    // `--recursive` / `--no-recursive` override the saved preference.
    let mut plan = open::plan(classify_inputs(launch_paths));
    if let Source::Scan { recursive, .. } = &mut plan.source {
        *recursive = overrides.recursive.unwrap_or(startup_settings.recursive);
    }
    // An archive **or folder** launch (CLI / double-click) is deferred until the window
    // exists, so the viewer shows immediately: an archive loads behind the spinner / dialogs,
    // and a folder *streams* in (the first photo appears almost at once) instead of blocking
    // startup on a big tree's full walk + sort. Only an explicit file list resolves now (it's
    // a finite, no-walk operation).
    let deferred = matches!(plan.source, Source::Archive(_) | Source::Scan { .. });
    let resolved = if deferred {
        Resolved::empty()
    } else {
        scan::resolve_playlist(&plan.source, &plan.cursor, startup_settings.show_archives)
    };

    match &plan.source {
        Source::Archive(_) => println!("{}: opening archive…", pb_app_core::APP_NAME),
        Source::Scan { .. } => println!("{}: scanning folder…", pb_app_core::APP_NAME),
        _ => {
            println!(
                "{}: {} image(s)",
                pb_app_core::APP_NAME,
                resolved.source.len()
            );
            if resolved.source.is_empty() {
                eprintln!(
                    "(no images - drop an image or folder on the window, or press O to open)"
                );
            }
        }
    }

    let event_loop = build_event_loop().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    // As the primary, start the single-instance IPC receiver (task #14): a message-only window
    // that hands forwarded paths to the app. It wakes the (Wait-blocked) loop via the proxy;
    // `about_to_wait` then drains the inbox. `send_event(())` is only a wake — the payload is `()`.
    #[cfg(windows)]
    if _single_instance.is_some() {
        let proxy = event_loop.create_proxy();
        single_instance::serve(move || {
            let _ = proxy.send_event(());
        });
    }

    let metrics = if metrics_on {
        METRICS_ON_FLAG.store(true, std::sync::atomic::Ordering::Relaxed);
        StageTimes::enabled()
    } else {
        StageTimes::disabled()
    };
    let mut app = App::new(
        windowed,
        resolved.root,
        resolved.source,
        resolved.start,
        resolved.recursive,
        resolved.scan_root,
        metrics,
        startup_settings,
        &overrides,
    );
    // Hand the deferred open (archive or folder scan) to the app; `resumed` fires it once
    // the window and engine are up. The plan carries the startup recursive override.
    if deferred {
        app.queue_launch(plan);
    }
    if let Err(e) = event_loop.run_app(&mut app) {
        // On WSLg / remote / software displays the Wayland (or X11) connection can be reset
        // out from under winit ("Connection reset by peer"); `run_app` then returns
        // `ExitFailure` with nothing left to drive. Don't `.expect()` — panicking here
        // unwinds and drops the GPU + windowing resources on the now-dead connection, which
        // segfaults (core dump). Exit hard instead: skip the destructors (there's no live
        // connection to release them against) and report cleanly.
        eprintln!(
            "{}: display connection lost — exiting ({e:?})",
            pb_app_core::APP_NAME
        );
        std::process::exit(1);
    }

    let report = app.core.metrics.report();
    if !report.is_empty() {
        let mut d = POOL_DECODE_MS.lock().unwrap().clone();
        let times: Vec<f64> = d.iter().map(|(ms, _)| *ms).collect();
        let p = pb_app_core::metrics::percentiles(&times, &[50.0, 95.0, 99.0]);
        println!(
            "\npool decode (under load): n={} p50={:.1} p95={:.1} p99={:.1} ms",
            d.len(),
            p[0],
            p[1],
            p[2]
        );
        d.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        println!("slowest decodes:");
        for (ms, tag) in d.iter().take(8) {
            println!("  {ms:>7.1} ms  {tag}");
        }
        print!("\n{report}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Pure helpers now live in pb-app-core::engine / pb-decode; imported here (not at the
    // crate root) so the bin build doesn't see them as unused after the NS0 Phase B move.
    use pb_app_core::engine::{is_exif_blob, point_in_rect, scale_alpha};
    use pb_app_core::Toast;
    use pb_decode::read_exif_fields;
    use pb_render::ScaleMode;
    use pb_source::FsSource;

    #[test]
    fn exif_blob_filters_makernote_and_oversized() {
        assert!(is_exif_blob("MakerNote", "0x4170706c..."));
        assert!(is_exif_blob("Padding", ""));
        assert!(is_exif_blob("Whatever", &"x".repeat(257))); // oversized → binary
        assert!(!is_exif_blob(
            "LensModel",
            "iPhone 11 Pro Max back triple camera"
        ));
        assert!(!is_exif_blob("Make", "Apple"));
    }

    #[test]
    fn classify_empty_and_files() {
        assert_eq!(classify_inputs(vec![]), LaunchInput::Empty);
        // Empty path strings are filtered out.
        assert_eq!(classify_inputs(vec![PathBuf::from("")]), LaunchInput::Empty);
        // Several paths (non-existent here, so treated as files) collect to Files.
        let files = vec![PathBuf::from("a.jpg"), PathBuf::from("b.png")];
        assert_eq!(classify_inputs(files.clone()), LaunchInput::Files(files));
    }

    #[test]
    fn classify_single_zip_as_archive() {
        // A lone .zip (from the picker, a drop, or a double-click) opens as an
        // archive — case-insensitively — not as one file in its folder.
        let zip = PathBuf::from("/photos/trip.zip");
        assert_eq!(
            classify_inputs(vec![zip.clone()]),
            LaunchInput::Archive(zip)
        );
        let up = PathBuf::from("ALBUM.ZIP");
        assert_eq!(classify_inputs(vec![up.clone()]), LaunchInput::Archive(up));
        // A zip among several files is just one of the files (multi-select stays a
        // file list; the non-image zip is dropped later when the list is resolved).
        let many = vec![PathBuf::from("a.jpg"), PathBuf::from("b.zip")];
        assert_eq!(classify_inputs(many.clone()), LaunchInput::Files(many));
    }

    #[test]
    fn resolve_explicit_filters_unsupported_and_keeps_order() {
        let src = Source::Explicit(vec![
            PathBuf::from("/p/a.jpg"),
            PathBuf::from("/p/notes.txt"),
            PathBuf::from("/p/b.png"),
        ]);
        let (paths, root, scan_root, recursive) = scan::resolve_source(&src, true, None);
        assert_eq!(
            paths,
            vec![PathBuf::from("/p/a.jpg"), PathBuf::from("/p/b.png")]
        );
        assert_eq!(root, PathBuf::from("/p"));
        assert_eq!(scan_root, None);
        assert!(!recursive);
    }

    /// A recursive scan descends nested subfolders and collects every supported image
    /// (sorted), skipping non-images; a flat scan stops at the immediate children. The
    /// basic contract the off-thread walker must keep.
    #[test]
    fn recursive_scan_walks_nested_subfolders() {
        let dir = std::env::temp_dir().join(format!("pb_scan_nested_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b/c")).expect("mkdir tree");
        for rel in [
            "top.jpg",
            "a/one.png",
            "a/b/two.webp",
            "a/b/c/three.gif",
            "a/notes.txt",
        ] {
            fs::write(dir.join(rel), b"x").expect("seed");
        }

        let flat = scan_images(&dir, false);
        assert_eq!(
            flat,
            vec![dir.join("top.jpg")],
            "a flat scan lists only the immediate children"
        );

        let deep = scan_images(&dir, true);
        assert_eq!(
            deep,
            vec![
                dir.join("a/b/c/three.gif"),
                dir.join("a/b/two.webp"),
                dir.join("a/one.png"),
                dir.join("top.jpg"),
            ],
            "a recursive scan finds every image (sorted), ignoring the .txt"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The streaming walk must yield images in the **exact** order the full-walk-then-
    /// `sort_by(ci_path_cmp)` produces — so showing photos before the scan finishes never
    /// reorders the playlist. Pins the boundary cases: **files before subfolders** (a root
    /// `a.jpg` / `z.jpg` before subfolder `a/b.jpg` / `a_subdir/x.jpg`), and `img2` vs `img10`
    /// (stays byte-lexicographic, not natural — a deferred opt-in).
    #[test]
    fn streaming_walk_order_matches_paths_sort() {
        let dir = std::env::temp_dir().join(format!("pb_stream_order_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a")).expect("mkdir a");
        fs::create_dir_all(dir.join("a_subdir")).expect("mkdir a_subdir");
        let images = [
            "z.jpg",
            "a.jpg",
            "img2.jpg",
            "img10.jpg",
            "a/b.jpg",
            "a_subdir/x.jpg",
        ];
        for rel in images {
            fs::write(dir.join(rel), b"x").expect("seed");
        }
        fs::write(dir.join("notes.txt"), b"x").expect("seed"); // non-image, must be skipped

        let got = sorted_image_walk(&dir, true);

        // Expected = exactly the images, in the deck's canonical order (`ci_path_cmp`:
        // files-before-folders, case-insensitive).
        let mut expected: Vec<PathBuf> = images.iter().map(|r| dir.join(r)).collect();
        expected.sort_by(|a, b| scan::ci_path_cmp(a, b));
        assert_eq!(
            got, expected,
            "streaming walk order must equal the deck sort (ci_path_cmp), skipping the .txt"
        );
        // Spell out the load-bearing boundary: a folder's own photos come before anything in
        // its subfolders — so the root files `a.jpg` / `z.jpg` precede the subfolder photos.
        let pos = |rel: &str| got.iter().position(|p| p == &dir.join(rel)).unwrap();
        assert!(
            pos("a.jpg") < pos("a/b.jpg"),
            "root a.jpg before subfolder a/b.jpg"
        );
        assert!(
            pos("z.jpg") < pos("a_subdir/x.jpg"),
            "root z.jpg before subfolder a_subdir/x.jpg"
        );
        // Lexicographic, NOT natural: "img10" < "img2" because '1' < '2'. (Natural sort
        // would flip these; it's a deferred opt-in, so we pin today's behavior.)
        assert!(
            pos("img10.jpg") < pos("img2.jpg"),
            "img10 before img2 (lexicographic)"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// THE regression for the reported crash: a directory symlink pointing back at an
    /// ancestor (the kind that riddles macOS's `~/Library`) must NOT send the recursive
    /// walk into an infinite loop. The old hand-rolled recursion would have looped →
    /// stack overflow → uncatchable abort; walkdir doesn't follow symlinks, so the walk
    /// terminates and returns only the real images.
    #[cfg(unix)]
    #[test]
    fn recursive_scan_does_not_follow_symlink_cycle() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("pb_scan_cycle_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).expect("mkdir");
        fs::write(dir.join("real.jpg"), b"x").expect("seed");
        fs::write(dir.join("sub/inner.png"), b"x").expect("seed");
        // A self-referential cycle: dir/sub/loop -> dir. Following it would recurse forever.
        symlink(&dir, dir.join("sub/loop")).expect("symlink");

        let images = scan_images(&dir, true);
        assert_eq!(
            images,
            vec![dir.join("real.jpg"), dir.join("sub/inner.png")],
            "the walk terminates and returns the real images, never the symlinked re-entry"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// A pre-cancelled scan stops immediately and gathers nothing — proving the cancel
    /// flag a superseding open (and the Scanning dialog's Cancel) relies on actually
    /// short-circuits the walk.
    #[test]
    fn scan_honors_cancel_flag() {
        let dir = std::env::temp_dir().join(format!("pb_scan_cancel_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a")).expect("mkdir");
        for rel in ["a.jpg", "a/b.png"] {
            fs::write(dir.join(rel), b"x").expect("seed");
        }

        let progress = ScanProgress::new();
        progress.request_cancel();
        let mut out = Vec::new();
        scan::collect_images(&dir, true, true, Some(&progress), &mut out);
        assert!(out.is_empty(), "a pre-cancelled scan collects nothing");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A live scan publishes its progress: the count matches the images gathered and the
    /// current folder gets set — what the Scanning dialog reads to show real motion.
    #[test]
    fn scan_reports_progress_count_and_current_folder() {
        let dir = std::env::temp_dir().join(format!("pb_scan_progress_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).expect("mkdir");
        for rel in ["a.jpg", "b.png", "sub/c.webp", "notes.txt"] {
            fs::write(dir.join(rel), b"x").expect("seed");
        }

        let progress = ScanProgress::new();
        let mut out = Vec::new();
        scan::collect_images(&dir, true, true, Some(&progress), &mut out);

        assert_eq!(out.len(), 3, "three supported images (the .txt is skipped)");
        assert_eq!(
            progress.found(),
            out.len(),
            "the published count matches the images gathered"
        );
        assert!(
            !progress.current().is_empty(),
            "the current folder is published during the walk"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rel_display_strips_root_then_falls_back_to_name() {
        let root = Path::new("/photos/Library");
        // A descendant shows its path relative to the root.
        assert_eq!(
            scan::rel_display(Path::new("/photos/Library/2024/Iceland"), root),
            Path::new("2024/Iceland").display().to_string()
        );
        // The root itself (empty relative) falls back to the root's own folder name.
        assert_eq!(scan::rel_display(root, root), "Library");
    }

    #[test]
    fn point_in_rect_hit_tests_the_chip() {
        let rect = [100.0, 50.0, 200.0, 80.0]; // x0,y0,x1,y1
        assert!(point_in_rect(rect, 150.0, 65.0), "inside");
        assert!(
            point_in_rect(rect, 100.0, 50.0),
            "top-left corner is inclusive"
        );
        assert!(
            point_in_rect(rect, 200.0, 80.0),
            "bottom-right corner is inclusive"
        );
        assert!(!point_in_rect(rect, 99.0, 65.0), "left of the rect");
        assert!(!point_in_rect(rect, 150.0, 81.0), "below the rect");
        assert!(!point_in_rect(rect, 250.0, 65.0), "right of the rect");
    }

    #[test]
    fn scan_display_name_uses_the_first_root_folder_name() {
        let source = Source::Scan {
            roots: vec![PathBuf::from("/photos/Vacation Pics")],
            recursive: true,
        };
        assert_eq!(scan_display_name(&source), "Vacation Pics");
    }

    #[test]
    fn toast_alpha_holds_then_fades_then_expires() {
        let t0 = Instant::now();
        let toast = Toast {
            rgba: Vec::new(),
            w: 1,
            h: 1,
            started: t0,
            uploaded_alpha: -1.0,
        };
        assert_eq!(toast.alpha(t0), Some(1.0));
        assert_eq!(toast.alpha(t0 + Toast::HOLD / 2), Some(1.0));
        let mid = toast.alpha(t0 + Toast::HOLD + Toast::FADE / 2).unwrap();
        assert!(mid > 0.0 && mid < 1.0, "mid-fade alpha was {mid}");
        assert_eq!(toast.alpha(t0 + Toast::HOLD + Toast::FADE), None);
    }

    #[test]
    fn scale_alpha_scales_only_the_alpha_channel() {
        let src = [10u8, 20, 30, 200];
        assert_eq!(scale_alpha(&src, 0.5), vec![10, 20, 30, 100]);
        assert_eq!(scale_alpha(&src, 0.0)[3], 0);
        assert_eq!(scale_alpha(&src, 1.0), src.to_vec());
        assert_eq!(scale_alpha(&src, 5.0)[3], 200); // clamped to 1.0
    }

    #[test]
    fn esc_quits_unless_the_picker_guard_is_active() {
        let now = Instant::now();
        assert!(esc_quits(None, now)); // no guard -> quits
        assert!(!esc_quits(Some(now + Duration::from_millis(100)), now)); // guarded
        assert!(esc_quits(Some(now), now)); // guard already expired -> quits
    }

    /// Recursively snapshot a directory tree for the no-trace before/after diff:
    /// every entry's path, plus each file's `(len, mtime)`. Catches a new file or
    /// directory anywhere under the root, and any change to an existing file.
    fn snapshot_tree(root: &Path) -> Vec<(PathBuf, Option<(u64, std::time::SystemTime)>)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, Option<(u64, std::time::SystemTime)>)>) {
            let Ok(rd) = fs::read_dir(dir) else {
                return;
            };
            for entry in rd.flatten() {
                let path = entry.path();
                let Ok(md) = entry.metadata() else {
                    continue;
                };
                if md.is_dir() {
                    out.push((path.clone(), None));
                    walk(&path, out);
                } else {
                    let mtime = md.modified().unwrap_or(std::time::UNIX_EPOCH);
                    out.push((path, Some((md.len(), mtime))));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out);
        out.sort();
        out
    }

    /// Privacy guarantee (task #2): viewing photos leaves **no trace on disk**. A
    /// full view session over a sandbox — recursive scan, decode-to-fit (what the
    /// prefetch pool does), and the on-demand EXIF read + file-size stat (the
    /// `Shift+I` panel) — must not create or modify a single file: no thumbnail DB,
    /// no decoded-pixel cache, no recent/MRU list of viewed paths. (The app's own
    /// install footprint — registry associations, config — is explicitly out of
    /// scope per ADR-018; this guards photo-*derived* data only.)
    #[test]
    fn viewing_a_folder_writes_nothing_to_disk() {
        // An isolated sandbox with real images and a subfolder.
        let dir = std::env::temp_dir().join(format!("pb_notrace_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).expect("mkdir sandbox");
        const IMG: &[u8] = include_bytes!("../icons/blazeviewer.png");
        for rel in ["a.png", "b.png", "sub/c.png", "d.png"] {
            fs::write(dir.join(rel), IMG).expect("seed image");
        }
        // Task #79: video items ride the same guarantee. A loose clip (garbage
        // bytes — the placeholder path must not even read them) and a Live-Photo
        // companion pair (the .mov is hidden by dedup, exactly like the app).
        fs::write(dir.join("clip.mp4"), b"not a real movie").expect("seed video");
        fs::write(dir.join("d.mov"), b"companion motion").expect("seed companion");
        // Task #104: an archive door rides the same guarantee. Garbage bytes for
        // the same reason as the clip — the door path must not even read them, and
        // never extracts to disk (which is what this test can actually prove).
        fs::write(dir.join("album.zip"), b"not a real zip").expect("seed archive");

        let before = snapshot_tree(&dir);

        // The actual disk-touching code the app runs while viewing, through the
        // real source seam: recursive scan → companion dedup → FsSource →
        // decode_item (the pool's step) + the Shift+I panel read (bytes for an
        // image, a stat only for a video or an archive door).
        let paths = scan::dedup_companions(scan_images(&dir, true), None);
        assert_eq!(
            paths.len(),
            6,
            "four images + the loose clip + the archive door; the companion .mov is hidden"
        );
        let source = FsSource::new(paths);
        let fit = FitBox {
            max_width: 64,
            max_height: 64,
        };
        for i in 0..source.len() {
            decode_item(&source, i, Some(fit), false).expect("decode");
            match pb_app_core::video::item_kind(&source, i) {
                pb_app_core::video::LibraryItemKind::Image => {
                    let bytes = source.bytes(i).expect("read for exif");
                    let _ = read_exif_fields(&bytes);
                }
                // The panel never RAM-reads a video: file size comes from a stat.
                pb_app_core::video::LibraryItemKind::Video(_) => {}
                // Nor an archive door (task #104) — same rule, and the seeded
                // `.zip` below is garbage, so a read would not merely be wasteful,
                // it would fail. Note what this arm can and cannot prove: this test
                // asserts nothing is *written*, and reading an archive writes
                // nothing, so a door that read its bytes would still pass here. The
                // read guarantee is pinned by `engine`'s panicking-source tests.
                pb_app_core::video::LibraryItemKind::Archive(_) => {}
            }
            if let Some(p) = source.path(i) {
                let _ = fs::metadata(p).expect("stat");
            }
        }

        let after = snapshot_tree(&dir);
        assert_eq!(
            before, after,
            "a view session must create or modify no files"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Privacy (task #79 phase 7): the no-trace guarantee holds through actual video
    /// PLAYBACK, not just viewing — poster + metadata probes, a real producer/session
    /// run, and a seek (which recreates the reader) against a sandboxed copy of the
    /// fixture must create or modify nothing on disk.
    #[cfg(windows)]
    #[test]
    fn playing_a_video_writes_nothing_to_disk() {
        use pb_app_core::video::{SeekGeneration, VideoSessionId, VideoSessionState};
        use pb_app_core::video_session::VideoSession;

        let dir = std::env::temp_dir().join(format!("pb_video_notrace_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir sandbox");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video/black_then_color.mp4");
        let clip = dir.join("clip.mp4");
        fs::copy(&fixture, &clip).expect("seed clip");

        let before = snapshot_tree(&dir);

        // Poster + metadata probes (the browse path).
        let _ = pb_decode::probe_video_stream(&clip).expect("probe");
        let _ =
            pb_decode::decode_video_poster(&clip, None, &std::sync::atomic::AtomicBool::new(false))
                .expect("poster");

        // A real playback session: play, seek forward (recreates the reader), play out.
        let sid = VideoSessionId(1);
        let (mut session, io) = VideoSession::new(sid, 64 * 64 * 4);
        let input = pb_decode::VideoInput::Path(clip.clone());
        std::thread::spawn(move || {
            pb_decode::run_video_producer(
                &input,
                None,
                sid,
                SeekGeneration::FIRST,
                io.events,
                io.msgs,
            );
        });
        let t0 = Instant::now();
        let mut sought = false;
        loop {
            let now = Instant::now();
            let _ = session.poll(now);
            if !sought && session.position(now) > Duration::from_millis(200) {
                sought = true;
                session.seek_to(Duration::from_millis(600), now, None);
            }
            match session.state() {
                VideoSessionState::Ended => break,
                VideoSessionState::Failed => panic!("playback failed: {:?}", session.error),
                _ => {}
            }
            assert!(
                t0.elapsed() < Duration::from_secs(15),
                "fixture must finish"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        session.stop();
        // Let the retiring reader threads run their course before the diff.
        std::thread::sleep(Duration::from_millis(300));

        let after = snapshot_tree(&dir);
        assert_eq!(
            before, after,
            "video playback must create or modify no files"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The privacy guarantee extends to archives: opening a `.zip` and viewing its
    /// images must NOT extract it to a temp directory or write anything — entries
    /// are read into RAM only. The sandbox holds just the `.zip`; a full view
    /// session (open → decode every entry → the Shift+I byte read) must leave the
    /// tree byte-for-byte identical.
    #[test]
    fn viewing_a_zip_writes_nothing_to_disk() {
        use std::io::Write as _;

        let dir = std::env::temp_dir().join(format!("pb_zip_notrace_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir sandbox");
        const IMG: &[u8] = include_bytes!("../icons/blazeviewer.png");
        const CLIP: &[u8] =
            include_bytes!("../../pb-decode/tests/fixtures/video/black_then_color.mp4");
        let zip_path = dir.join("album.zip");
        {
            let f = fs::File::create(&zip_path).expect("create zip");
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for name in ["a.png", "b.png", "sub/c.png"] {
                zw.start_file(name, opts).expect("start entry");
                zw.write_all(IMG).expect("write entry");
            }
            // A video entry too: its poster decodes from in-RAM bytes through the
            // MF byte stream — that path must be as write-free as the image one.
            zw.start_file("sub/clip.mp4", opts).expect("start entry");
            zw.write_all(CLIP).expect("write entry");
            zw.finish().expect("finish zip");
        }

        let before = snapshot_tree(&dir);

        // The disk-touching code the app runs while viewing a zip.
        let resolved = scan::resolve_playlist(
            &Source::Archive(zip_path.clone()),
            &open::Cursor::First,
            true,
        );
        assert_eq!(
            resolved.source.len(),
            4,
            "zip should yield three images + one video"
        );
        let fit = FitBox {
            max_width: 64,
            max_height: 64,
        };
        for i in 0..resolved.source.len() {
            // Images decode; the video entry posters via the in-RAM byte stream.
            decode_item(resolved.source.as_ref(), i, Some(fit), false).expect("decode");
            let bytes = resolved.source.bytes(i).expect("read for exif");
            let _ = read_exif_fields(&bytes);
        }

        let after = snapshot_tree(&dir);
        assert_eq!(
            before, after,
            "viewing a zip must create or modify no files (no extraction to disk)"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The privacy guarantee holds for 7z too — and this is the one most at risk,
    /// since a 7z is *eagerly decompressed*. It must go to RAM only, never extracted
    /// to a temp directory. Sandbox holds just the `.7z`; a full view session leaves
    /// the tree byte-for-byte identical.
    #[test]
    fn viewing_a_7z_writes_nothing_to_disk() {
        use sevenz_rust2::{ArchiveEntry, ArchiveWriter};

        let dir = std::env::temp_dir().join(format!("pb_7z_notrace_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir sandbox");
        const IMG: &[u8] = include_bytes!("../icons/blazeviewer.png");
        let z_path = dir.join("album.7z");
        {
            let mut sz = ArchiveWriter::create(&z_path).expect("create 7z");
            for name in ["a.png", "b.png", "sub/c.png"] {
                sz.push_archive_entry(ArchiveEntry::new_file(name), Some(IMG))
                    .expect("push entry");
            }
            sz.finish().expect("finish 7z");
        }

        let before = snapshot_tree(&dir);

        // Eager-open the 7z and view every entry: must not extract to disk. Go straight
        // through `load_seven_z` (the eager decompress-to-RAM step this test guards),
        // bypassing the live RAM-budget pre-flight: this asserts the *no-disk-write*
        // guarantee, not the budget gate (that's `over_budget_7z_is_refused_with_structured_error`).
        // The real `ram_budget()` floors to 0 on a low-memory machine (e.g. an 8 GB VM),
        // which would refuse even this 1 MB archive and flake the test; injecting past it
        // keeps the check deterministic while still exercising the decompress path.
        let resolved =
            scan::load_seven_z(&z_path, None, &pb_source::OpenProgress::new(), 0).expect("open 7z");
        assert_eq!(resolved.source.len(), 3, "7z should yield three images");
        let fit = FitBox {
            max_width: 64,
            max_height: 64,
        };
        for i in 0..resolved.source.len() {
            decode_item(resolved.source.as_ref(), i, Some(fit), false).expect("decode");
            let bytes = resolved.source.bytes(i).expect("read for exif");
            let _ = read_exif_fields(&bytes);
        }

        let after = snapshot_tree(&dir);
        assert_eq!(
            before, after,
            "viewing a 7z must create or modify no files (no extraction to disk)"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The privacy guarantee extends to the tar family (#102), lazy half: a plain
    /// `.tar` is indexed by seeking over headers and every `bytes(i)` is an
    /// open + seek + read — none of which may create or modify anything.
    #[test]
    fn viewing_a_tar_writes_nothing_to_disk() {
        let dir = std::env::temp_dir().join(format!("pb_tar_notrace_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir sandbox");
        const IMG: &[u8] = include_bytes!("../icons/blazeviewer.png");
        let tar_path = dir.join("album.tar");
        {
            let f = fs::File::create(&tar_path).expect("create tar");
            let mut tw = tar::Builder::new(f);
            for name in ["a.png", "b.png", "sub/c.png"] {
                let mut h = tar::Header::new_gnu();
                h.set_size(IMG.len() as u64);
                h.set_mode(0o644);
                tw.append_data(&mut h, name, IMG).expect("append entry");
            }
            tw.finish().expect("finish tar");
        }

        let before = snapshot_tree(&dir);

        let resolved = scan::resolve_playlist(
            &Source::Archive(tar_path.clone()),
            &open::Cursor::First,
            true,
        );
        assert_eq!(resolved.source.len(), 3, "tar should yield three images");
        let fit = FitBox {
            max_width: 64,
            max_height: 64,
        };
        for i in 0..resolved.source.len() {
            decode_item(resolved.source.as_ref(), i, Some(fit), false).expect("decode");
            let bytes = resolved.source.bytes(i).expect("read for exif");
            let _ = read_exif_fields(&bytes);
        }

        let after = snapshot_tree(&dir);
        assert_eq!(
            before, after,
            "viewing a tar must create or modify no files (no extraction to disk)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The eager half (#102): a `.tar.gz` is streamed whole into RAM — like the
    /// 7z test, the most at-risk shape. Nothing may land on disk.
    #[test]
    fn viewing_a_tar_gz_writes_nothing_to_disk() {
        use std::io::Write as _;

        let dir = std::env::temp_dir().join(format!("pb_tgz_notrace_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir sandbox");
        const IMG: &[u8] = include_bytes!("../icons/blazeviewer.png");
        let tgz_path = dir.join("album.tar.gz");
        {
            let mut tar_bytes = Vec::new();
            {
                let mut tw = tar::Builder::new(&mut tar_bytes);
                for name in ["a.png", "b.png"] {
                    let mut h = tar::Header::new_gnu();
                    h.set_size(IMG.len() as u64);
                    h.set_mode(0o644);
                    tw.append_data(&mut h, name, IMG).expect("append entry");
                }
                tw.finish().expect("finish tar");
            }
            let f = fs::File::create(&tgz_path).expect("create tgz");
            let mut gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            gz.write_all(&tar_bytes).expect("compress");
            gz.finish().expect("finish gz");
        }

        let before = snapshot_tree(&dir);

        // Straight into the eager decode-to-RAM step this test guards, with an
        // injected budget so a low-RAM machine's real ram_budget() (which can
        // floor to 0) can't flake the test — same reasoning as the 7z no-trace
        // test going through load_seven_z rather than the pre-flight.
        let src = pb_source::TarSource::open_compressed(
            &tgz_path,
            pb_source::ArchiveKind::TarGz,
            scan::is_supported_archive_entry,
            None,
            u64::MAX,
        )
        .expect("open tar.gz");
        let resolved = scan::archive_resolved(&tgz_path, std::sync::Arc::new(src));
        assert_eq!(resolved.source.len(), 2, "tar.gz should yield two images");
        let fit = FitBox {
            max_width: 64,
            max_height: 64,
        };
        for i in 0..resolved.source.len() {
            decode_item(resolved.source.as_ref(), i, Some(fit), false).expect("decode");
            let bytes = resolved.source.bytes(i).expect("read for exif");
            let _ = read_exif_fields(&bytes);
        }

        let after = snapshot_tree(&dir);
        assert_eq!(
            before, after,
            "viewing a tar.gz must create or modify no files (no extraction to disk)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The privacy guarantee extends to RAR (#103): the scan, the solid-group
    /// eager decode, and every lazy per-entry decode are RAM-only. (The fixture
    /// holds text-shaped bytes, so entries are read + CRC-checked rather than
    /// image-decoded — the disk-trace surface is identical.)
    #[test]
    fn viewing_a_rar_writes_nothing_to_disk() {
        const RAR: &[u8] = include_bytes!("../../pb-source/tests/fixtures/rar/lz_solid.rar");
        let dir = std::env::temp_dir().join(format!("pb_rar_notrace_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir sandbox");
        let rar_path = dir.join("album.rar");
        fs::write(&rar_path, RAR).expect("seed rar");
        // A password-protected solid RAR seeded alongside: decryption is RAM-only
        // too (PBKDF2 + AES-CBC), never staging plaintext to disk. Seed both
        // before the snapshot so the only disk activity under test is the decode.
        const ENC_RAR: &[u8] =
            include_bytes!("../../pb-source/tests/fixtures/rar/encrypted_solid.rar");
        let enc_path = dir.join("locked.rar");
        fs::write(&enc_path, ENC_RAR).expect("seed encrypted rar");

        let before = snapshot_tree(&dir);

        // Straight into the open (solid groups eager-decode here), with an
        // injected budget so a low-RAM machine's real ram_budget() can't flake
        // the test — same reasoning as the 7z and tar.gz no-trace tests.
        let src = pb_source::RarSource::open(
            &rar_path,
            scan::is_supported_archive_entry,
            None,
            u64::MAX,
            None,
        )
        .expect("open rar");
        let resolved = scan::archive_resolved(&rar_path, std::sync::Arc::new(src));
        assert_eq!(resolved.source.len(), 3, "rar should yield three entries");
        for i in 0..resolved.source.len() {
            let _ = resolved.source.bytes(i).expect("read entry");
        }

        // Decrypt the password-protected archive in the same window.
        let enc = pb_source::RarSource::open(
            &enc_path,
            scan::is_supported_archive_entry,
            None,
            u64::MAX,
            Some("hunter2"),
        )
        .expect("open encrypted rar");
        for i in 0..enc.len() {
            let _ = enc.bytes(i).expect("decrypt entry");
        }

        let after = snapshot_tree(&dir);
        assert_eq!(
            before, after,
            "viewing a rar must create or modify no files (no extraction to disk)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// An over-budget 7z must be refused with a *structured* [`ArchiveOpenError::TooLarge`],
    /// never a (uncatchable) allocation abort. A real allocation failure can't be
    /// safely injected, and the `PB_ARCHIVE_RAM_BUDGET` env var races parallel tests —
    /// so we drive the refusal deterministically by pre-flighting a real archive
    /// against an injected 1-byte budget ([`seven_z_preflight_within`]). The same
    /// archive must pass under a generous budget, proving the budget is the only gate.
    #[test]
    fn over_budget_7z_is_refused_with_structured_error() {
        use sevenz_rust2::{ArchiveEntry, ArchiveWriter};

        let dir = std::env::temp_dir().join(format!("pb_7z_budget_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir sandbox");
        const IMG: &[u8] = include_bytes!("../icons/blazeviewer.png");
        let z_path = dir.join("album.7z");
        {
            let mut sz = ArchiveWriter::create(&z_path).expect("create 7z");
            sz.push_archive_entry(ArchiveEntry::new_file("a.png"), Some(IMG))
                .expect("push entry");
            sz.finish().expect("finish 7z");
        }

        // The projection is the resident decompressed image bytes the eager open
        // would hold — at least the one image we put in.
        let needed =
            pb_source::seven_z_projected_bytes(&z_path, None, pb_decode::is_supported_extension)
                .expect("project");
        assert!(
            needed >= IMG.len() as u64,
            "projection ({needed}) covers the image ({})",
            IMG.len()
        );

        // A 1-byte budget is below the projection -> instant, structured refusal
        // (not a load attempt, not an abort).
        match scan::seven_z_preflight_within(&z_path, None, 1) {
            Err(archive::ArchiveOpenError::TooLarge { needed: n, budget }) => {
                assert_eq!(budget, 1);
                assert_eq!(n, needed);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }

        // The same archive fits under a generous budget: the pre-flight is the only gate.
        scan::seven_z_preflight_within(&z_path, None, u64::MAX).expect("fits under a huge budget");

        let _ = fs::remove_dir_all(&dir);
    }

    // --- Typed menu state (`App::menu_state_from`) --------------------------------
    // The pure derivation from live app state to the shell-neutral `contract::MenuState`.
    // The muda-touching `apply_menu_state` diff isn't unit-testable (needs OS handles),
    // but the mapping it depends on is fully covered here.

    /// A neutral baseline for the arguments, so each test varies just one thing.
    fn base_menu_state() -> contract::MenuState {
        AppCore::menu_state_from(
            ScaleMode::Fit,
            false, // info_line
            pb_app_core::Panels::default(),
            false, // tree_open
            false,
            false,
            false,
            false, // mute_live_audio
            false, // subtitles
            false, // save_rotation_enabled
            false, // reveal_enabled
            false, // cancel_scan_enabled
            None,
            false,
            None, // displayed_item
            None, // compare_pin
        )
    }

    #[test]
    fn menu_state_maps_every_scale_mode() {
        let scale = |m| {
            AppCore::menu_state_from(
                m,
                false, // info_line
                pb_app_core::Panels::default(),
                false, // tree_open
                false,
                false,
                false,
                false, // mute_live_audio
                false, // subtitles
                false, // save_rotation_enabled
                false, // reveal_enabled
                false, // cancel_scan_enabled
                None,
                false,
                None,
                None,
            )
            .scale
        };
        assert_eq!(scale(ScaleMode::Fit), contract::ScaleMode::Fit);
        assert_eq!(scale(ScaleMode::Fill), contract::ScaleMode::Fill);
        assert_eq!(scale(ScaleMode::Original), contract::ScaleMode::Original);
    }

    #[test]
    fn menu_state_maps_the_info_and_panel_checkmarks() {
        use pb_app_core::{InspectorTab, Panels};
        let state = |line: bool, panels: Panels, tree: bool| {
            AppCore::menu_state_from(
                ScaleMode::Fit,
                line,
                panels,
                tree,
                false,
                false,
                false,
                false, // mute_live_audio
                false, // subtitles
                false, // save_rotation_enabled
                false, // reveal_enabled
                false, // cancel_scan_enabled
                None,
                false,
                None,
                None,
            )
        };
        // The basic line and the Details tab check independently (task #54 decouple).
        let s = state(true, Panels::default(), false);
        assert!(s.info_basic && !s.info_full);
        let details = Panels {
            inspector: Some(InspectorTab::Details),
            ..Default::default()
        };
        let s = state(true, details, false);
        assert!(
            s.info_basic && s.info_full,
            "both can be checked at once now"
        );
        // Other tabs / Help leave the EXIF checkmark off.
        for panels in [
            Panels {
                inspector: Some(InspectorTab::Text),
                ..Default::default()
            },
            Panels {
                help: true,
                ..Default::default()
            },
        ] {
            assert!(!state(false, panels, false).info_full);
        }
        // Hide Panels: checked while hidden, enabled only with something open
        // (the tree counts).
        let s = state(false, Panels::default(), false);
        assert!(!s.panels_hidden && !s.hide_panels_enabled);
        let s = state(false, Panels::default(), true);
        assert!(s.hide_panels_enabled, "an open tree makes Tab meaningful");
        let hidden = Panels {
            inspector: Some(InspectorTab::Details),
            hidden: true,
            ..Default::default()
        };
        let s = state(false, hidden, false);
        assert!(s.panels_hidden && s.hide_panels_enabled);
        assert!(
            s.info_full,
            "Details stays checked while hidden: hidden != closed"
        );
    }

    #[test]
    fn menu_state_carries_undo_label_and_enabled_together() {
        // `None` on the undo stack → disabled "Undo"; a label → enabled with that title.
        assert_eq!(base_menu_state().undo, None);
        let with_undo = AppCore::menu_state_from(
            ScaleMode::Fit,
            false, // info_line
            pb_app_core::Panels::default(),
            false, // tree_open
            false,
            false,
            false,
            false, // mute_live_audio
            false, // subtitles
            false, // save_rotation_enabled
            false, // reveal_enabled
            false, // cancel_scan_enabled
            Some("Undo Save Rotation".to_string()),
            false,
            None,
            None,
        );
        assert_eq!(with_undo.undo.as_deref(), Some("Undo Save Rotation"));
    }

    #[test]
    fn menu_state_passes_through_every_bool_flag() {
        // Each toggle/enabled input lands on its own field (no crossed wires).
        let all_on = AppCore::menu_state_from(
            ScaleMode::Fit,
            false, // info_line
            pb_app_core::Panels::default(),
            false, // tree_open
            true,  // recursive
            true,  // fullscreen
            true,  // slideshow
            true,  // mute_live_audio
            true,  // subtitles
            true,  // save_rotation_enabled
            true,  // reveal_enabled
            true,  // cancel_scan_enabled
            None,
            true,    // native_fullscreen_engaged
            Some(0), // displayed_item
            Some(0), // compare_pin (the displayed photo IS the pin)
        );
        assert!(all_on.recursive);
        assert!(all_on.fullscreen);
        assert!(all_on.slideshow);
        assert!(all_on.mute_live_audio);
        assert!(all_on.subtitles);
        assert!(all_on.save_rotation_enabled);
        assert!(all_on.reveal_enabled);
        assert!(all_on.cancel_scan_enabled);
        assert!(all_on.native_fullscreen_engaged);
        assert!(all_on.compare_pin_enabled);
        assert!(all_on.compare_pinned_here);
        assert!(all_on.compare_toggle_enabled);

        // The baseline leaves them all off.
        let b = base_menu_state();
        assert!(
            !b.recursive
                && !b.fullscreen
                && !b.slideshow
                && !b.mute_live_audio
                && !b.subtitles
                && !b.save_rotation_enabled
                && !b.reveal_enabled
                && !b.cancel_scan_enabled
                && !b.native_fullscreen_engaged
                && !b.compare_pin_enabled
                && !b.compare_pinned_here
                && !b.compare_toggle_enabled
        );
    }

    #[test]
    fn menu_state_derives_the_compare_flags() {
        let state = |displayed: Option<usize>, pin: Option<usize>| {
            AppCore::menu_state_from(
                ScaleMode::Fit,
                false, // info_line
                pb_app_core::Panels::default(),
                false, // tree_open
                false,
                false,
                false,
                false, // mute_live_audio
                false, // subtitles
                false, // save_rotation_enabled
                false, // reveal_enabled
                false, // cancel_scan_enabled
                None,
                false,
                displayed,
                pin,
            )
        };
        // Photo shown, no pin: Pin enabled/unchecked, Compare disabled.
        let s = state(Some(3), None);
        assert!(s.compare_pin_enabled && !s.compare_pinned_here && !s.compare_toggle_enabled);
        // Pin elsewhere: both enabled, Pin unchecked.
        let s = state(Some(3), Some(0));
        assert!(s.compare_pin_enabled && !s.compare_pinned_here && s.compare_toggle_enabled);
        // Viewing the pin: Pin checked.
        let s = state(Some(0), Some(0));
        assert!(s.compare_pinned_here);
        // Empty deck: everything off, even with a stale pin index.
        let s = state(None, Some(0));
        assert!(!s.compare_pin_enabled && !s.compare_pinned_here && !s.compare_toggle_enabled);
    }

    #[test]
    fn menu_state_equality_drives_the_no_op_cache() {
        // The diff in `apply_menu_state` relies on `PartialEq`: identical inputs must
        // compare equal (→ no OS call), and any single change must compare unequal.
        let a = base_menu_state();
        let b = base_menu_state();
        assert_eq!(a, b);
        let changed = AppCore::menu_state_from(
            ScaleMode::Fit,
            false, // info_line
            pb_app_core::Panels::default(),
            false, // tree_open
            false,
            false,
            true,  // slideshow flipped
            false, // mute_live_audio
            false, // subtitles
            false, // save_rotation_enabled
            false, // reveal_enabled
            false, // cancel_scan_enabled
            None,
            false,
            None,
            None,
        );
        assert_ne!(a, changed);
    }
}
