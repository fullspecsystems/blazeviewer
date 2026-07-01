#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! PhotoBlaze — the application shell (Phase 3: the prefetch engine).
//!
//! A chrome-less, fit-to-screen viewer built to **hold a key and fly**. Decode +
//! file I/O run on a priority worker pool (`decode_pool`), neighbors are decoded
//! *ahead* of you and uploaded into a resident GPU texture ring, so a keypress is
//! a **rebind, never a decode or upload**. Advance is **gated on readiness**:
//! every photo is shown in order (none skipped); a cache miss holds the previous
//! frame until its decode lands, then shows it — fly speed is min(refresh, decode).
//!
//!   space       next photo  ·  ⌫  previous photo
//!   enter       random photo (precomputed shuffle; hold to fly)
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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
use pb_decode::{decode_bytes, is_supported_extension, FitBox};
use pb_render::{Renderer, Rotation, ViewTransform, WgpuRenderer};
use pb_source::{seven_z_projected_bytes, FsSource, PhotoSource, SevenZSource, ZipSource};

mod archive;
mod clipboard;
#[cfg(windows)]
mod darkmode;
mod delete;
mod dialog;
#[cfg(target_os = "macos")]
mod hdr_surface;
mod hud_gallery;
mod live_audio;
#[cfg(target_os = "macos")]
mod macos_chrome;
#[cfg(target_os = "macos")]
mod macos_open;
mod menu;
mod pb_key_winit;
#[cfg(target_os = "macos")]
mod proxy_icon;
mod save_rotation;
// The action vocabulary, physical-key model, keymap, and slideshow timing now live
// in the platform-neutral `pb-app-core` (NS0). Re-export them at the crate root so
// the existing `crate::action` / `crate::keymap` / `crate::pb_key` / `crate::slideshow`
// paths in the winit shell modules (and the `use action::…` lines below) keep
// resolving unchanged.
use pb_app_core::{
    action, contract, keymap, pb_key, slideshow, AppCore, InfoMode, Nav, OpenButton, OpenPanel,
    UndoAction, Viewport,
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
    decode_item, file_name_of, is_hdr, meta_for, render_color, ring_capacity, scale_mode_of,
    window_for_capacity, RING_BUDGET_BYTES,
};
use pb_app_core::metrics::StageTimes;

/// Cap on decoded-but-not-yet-uploaded bytes held by the pool (backpressure).
const POOL_BUDGET_BYTES: usize = 512 * 1024 * 1024;
/// Per-decode wall time *as the pool sees it* (i.e. under real concurrent load),
/// printed with the `--metrics` report. Isolated decode is fast; this shows how much
/// 8-way contention inflates it (it's how the RAW-demosaic-on-preview stall was
/// found). Only recorded under `--metrics` (the flag below), so it's zero-overhead
/// and unbounded-growth-free in normal runs.
static POOL_DECODE_MS: std::sync::Mutex<Vec<(f64, String)>> = std::sync::Mutex::new(Vec::new());
/// Whether `--metrics` is on (gates the `POOL_DECODE_MS` recording in the off-thread
/// decode closure, which has no access to the `StageTimes`).
static METRICS_ON_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Trackpad gesture tuning. `WHEEL_ZOOM_STEP` is the per-line zoom factor for
/// **Ctrl+scroll** (the explicit zoom gesture; plain scroll pans instead — see the
/// `MouseWheel` handler). (`PINCH_GAIN` moved to `pb_app_core::engine` with the pinch
/// arm of `handle`.)
const WHEEL_ZOOM_STEP: f32 = 0.1;
/// Per-**pixel** zoom factor for a pixel-precise scroll (`PixelDelta` — a macOS trackpad
/// two-finger swipe) when the `Scroll wheel` setting is Zoom (or Ctrl is held). Much smaller
/// than [`WHEEL_ZOOM_STEP`] because a trackpad delivers many events of tens of pixels each;
/// `0.0025` gives ~1.6× over a full swipe. Tunable if the zoom feels too fast/slow.
const PIXEL_ZOOM_STEP: f32 = 0.0025;
/// Pixels panned per scroll *line* (`LineDelta`). On Windows winit reports both a
/// real mouse wheel and a precision-trackpad two-finger swipe as `LineDelta` (it
/// never emits `PixelDelta` there), so plain scroll pans by this much per line:
/// the fractional high-res lines from a trackpad pan smoothly, while a 1.0 mouse
/// notch makes one comfortable step.
const WHEEL_PAN_STEP: f32 = 80.0;
/// Sign of two-finger trackpad panning (`PixelDelta` on macOS, `LineDelta` scroll
/// on Windows). `+1.0` makes the image follow the fingers (grab-and-drag); flip to
/// `-1.0` to invert.
const GESTURE_PAN_DIR: f32 = 1.0;

/// How long the delete icon shows on the just-deleted photo before the playlist
/// advances to the next one — so the trash/recycle feedback registers first (#28).
const DELETE_ADVANCE_DELAY: Duration = Duration::from_millis(160);

/// How long an off-thread directory scan must run before the "Scanning Folder" progress
/// dialog appears. A normal folder resolves in well under this, so the common case never
/// flashes a dialog (and never pays for the extra window); only a genuinely large/nested
/// tree (the `~/Library` case) reveals it — with a live count, current folder, and Cancel.
const SCAN_DIALOG_DELAY: Duration = Duration::from_millis(250);

/// How often the streaming scan worker publishes a growing playlist snapshot. Time-bounded
/// (not per-count) so the number of snapshots — and thus the per-snapshot O(N) `FsSource`
/// rebuild — stays small (≈ scan_duration / this) regardless of folder size. The first
/// batch lands at the first interval boundary (or at scan end for a fast folder, which is
/// then the only batch), bootstrapping the view well under [`SCAN_DIALOG_DELAY`]; this just
/// governs how often the rest refills.
const SCAN_BATCH_INTERVAL: Duration = Duration::from_millis(150);

/// How often the scan card is re-rasterized at most. The live current-folder line changes per
/// directory (fast); throttling the rebuild keeps the software composite off the hot path
/// while the displayed path/count lag by at most this. Show/hide is immediate.
const SCAN_CARD_REFRESH: Duration = Duration::from_millis(120);

/// Whether an Escape press should quit, given an optional "ignore Esc until"
/// guard set briefly after the file picker closes (to swallow the stray Esc that
/// dismissed it). Quits when there is no guard, or it has already expired.
fn esc_quits(guard: Option<Instant>, now: Instant) -> bool {
    match guard {
        Some(until) => now >= until,
        None => true,
    }
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
    windowed: bool,
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
    /// macOS: the EDR headroom last applied to the renderer (the window's display).
    /// On a window move we re-query the new screen and only reconfigure when it
    /// changes — so dragging across a display with different HDR capability adapts.
    #[cfg(target_os = "macos")]
    last_edr_headroom: f32,
    /// The native menu bar (windowed mode only). Built once, kept alive here so its
    /// native handle outlives the window. `None` until the first window is created.
    menu: Option<muda::Menu>,
    /// macOS-only: the **Window** submenu, kept so [`apply_menu_for_mode`] can mark it
    /// as the NSApp Window menu (`set_as_windows_menu_for_nsapp`) right after
    /// `init_for_nsapp` — the order muda requires — which makes macOS append the live
    /// window list under Minimize / Zoom / Bring All to Front.
    #[cfg(target_os = "macos")]
    window_menu: Option<muda::Submenu>,
    /// macOS-only: the native (Spaces) fullscreen menu item, kept so its title can flip
    /// between "Enter Full Screen" and "Exit Full Screen" to mirror the live state (the
    /// Mac convention — no checkmark). The last-pushed engaged state is cached as part of
    /// [`App::menu_state`], so the per-tick refresh is a no-op when nothing changed.
    #[cfg(target_os = "macos")]
    native_fullscreen_item: Option<muda::MenuItem>,
    /// macOS-only: the file currently shown as the title-bar proxy icon (the window's
    /// represented file). Caches the last-pushed value so the per-tick refresh is a
    /// no-op `setRepresentedURL:` call when the displayed photo hasn't changed. `None`
    /// = no proxy (fullscreen, an archive entry, or the empty state). See
    /// [`proxy_icon::set_represented_url`] / [`App::refresh_proxy_icon`].
    #[cfg(target_os = "macos")]
    proxy_icon_path: Option<PathBuf>,
    /// The "Save Rotation" menu item, kept so its enabled state can be toggled at
    /// runtime (only enabled when the current photo has an unsaved rotation on an
    /// EXIF-writable file).
    save_rotation_item: Option<muda::MenuItem>,
    /// The File ▸ Stop Scanning menu item, enabled only while a folder scan is streaming in.
    cancel_scan_item: Option<muda::MenuItem>,
    /// The Edit ▸ Undo menu item, kept so its title + enabled state can mirror the top of
    /// the undo stack at runtime.
    undo_item: Option<muda::MenuItem>,
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
    menu_attached: bool,
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
    password_archive: Option<PathBuf>,

    // --- Live Photo audio (the animation playback *state* moved to AppCore in NS0 5.5; this
    // ObjC `AVAudioPlayer` handle stays shell-owned, driven from `self.core.playback`) ---
    /// The Live Photo's audio (its `.mov` track), playing while the motion plays — the
    /// "cheap path" (task #38). `None` when nothing is playing / it's a silent clip / not
    /// a Live Photo. Dropped (which stops it) on pause-to-step, finish, or navigate.
    live_audio: Option<LiveAudio>,

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

/// What the user did in the dialog window — the result mirror of [`DialogRequest`]. The
/// shell ([`App::dialog_event`]) drives egui, extracts any egui-side payload (a password,
/// the edited settings/keymap), and hands the core one of these; [`App::handle_dialog_outcome`]
/// runs the reaction. (NS0, ADR-021 step 4d: the *results* half of the dialog seam. It's not
/// yet a clean shell/core split — `handle_dialog_outcome` still closes the window and the
/// archive path reaches into the `DialogWindow` — because the archive/scan flow inverts with
/// step 5; at that point these become `CoreEvent`s and the window ops become effects.)
enum DialogOutcome {
    /// Esc / close button dismissed a dialog of this kind (cancels the matching in-flight op).
    Dismissed(Option<dialog::DialogKind>),
    /// Password entry submitted (archive unlock); `None` if extraction failed.
    PasswordSubmitted(Option<String>),
    /// The password prompt's Cancel — abandon the pending archive.
    PasswordCancelled,
    /// Settings saved, carrying the (optionally) edited settings + keymap.
    SettingsSaved {
        settings: Option<settings::Settings>,
        keymap: Option<Keymap>,
    },
    /// Settings dialog's Cancel (its Esc goes through [`DialogOutcome::Dismissed`]).
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
    fn new(
        windowed: bool,
        root: PathBuf,
        source: Arc<dyn PhotoSource>,
        start: usize,
        recursive: bool,
        scan_root: Option<PathBuf>,
        metrics: StageTimes,
    ) -> Self {
        let playlist = Playlist::new(source.len(), 0).with_cursor(start);
        let decode: Arc<DecodeFn> = Arc::new(|src: &dyn PhotoSource, item, fit, allow_preview| {
            if !METRICS_ON_FLAG.load(std::sync::atomic::Ordering::Relaxed) {
                return decode_item(src, item, fit, allow_preview);
            }
            let t0 = Instant::now();
            let r = decode_item(src, item, fit, allow_preview);
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            let tag = format!(
                "{}{}",
                if allow_preview { "prev " } else { "full " },
                src.name(item)
            );
            POOL_DECODE_MS.lock().unwrap().push((ms, tag));
            r
        });
        let (pool, results) = DecodePool::new(recommended_workers(), POOL_BUDGET_BYTES, decode);
        // Preferences (nav feel, defaults, …); the hold loop reads them live.
        let settings = settings::Settings::load();
        Self {
            windowed,
            window: None,
            core: AppCore {
                now: Instant::now(),
                viewport: Viewport {
                    width: 1,
                    height: 1,
                    scale_factor: 1.0,
                },
                held: HashMap::new(),
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
                fit: None,
                // Start in the user's default scale mode (8/9/0 still switch it live).
                view: ViewTransform {
                    mode: scale_mode_of(settings.scale_mode),
                    ..ViewTransform::default()
                },
                last_cursor: None,
                dragging: false,
                rotations: HashMap::new(),
                zoom_started: None,
                zoom_last: None,
                pan_started: None,
                pan_last: None,
                resize_settle_at: None,
                geometry_save_at: None,
                meta_cache: HashMap::new(),
                current: None,
                exif_cache: HashMap::new(),
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
                live_motion_cache: HashMap::new(),
                metrics,
                source,
                playlist,
                targets: Vec::new(),
                last_nav: Nav::Forward,
                displayed_item: None,
                target_item: None,
                epoch: 1,
                root,
                scan_root,
                recursive,
                scanning: false,
                launching: false,
                dialog_open: false,
                archive_loading: false,
                pending_delete: None,
                pending_confirm_delete: None,
                info: InfoMode::Off,
                overlay_shown: false,
                overlay_item: None,
                toast: None,
                wait_started: None,
                pie_finish: None,
                pie_glow_started: None,
                decode_ewma: 0.25,
                pie_drawn: false,
                pie_pushed: None,
                chip_sig: None,
                chip_built: Instant::now(),
                chip_rect: None,
                chip_hovered: false,
                open_panel: None,
                open_hover: None,
                play_hint: None,
                hud: Hud::load(),
                renderer: None,
                undo_stack: Vec::new(),
                playback: None,
                anim_frame_shown_at: None,
                anim_decode: None,
                prepared: None,
                anim_gen: 0,
                anim_hint_shown_for: None,
                framestep_started: None,
                framestep_last: None,
                live_revert_at: None,
                keymap: Keymap::load(),
                settings,
                effects: Vec::new(),
            },
            pending_drops: Vec::new(),
            #[cfg(target_os = "macos")]
            last_edr_headroom: 1.0,
            menu: None,
            #[cfg(target_os = "macos")]
            window_menu: None,
            #[cfg(target_os = "macos")]
            native_fullscreen_item: None,
            #[cfg(target_os = "macos")]
            proxy_icon_path: None,
            save_rotation_item: None,
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
            password_archive: None,
            live_audio: None,
            pending_dialog: None,
            requested_wake: None,
        }
    }

    /// Persist the current photo's in-RAM rotation to its file's EXIF Orientation
    /// tag (`Ctrl+S` / File ▸ Save Rotation, task #29) — lossless (the compressed
    /// pixels are untouched). The first write to a user's photo bytes, and like
    /// Copy/Delete it only ever runs on an explicit command. JPEG only for now;
    /// other formats (and archive entries, which have no file) are greyed out + toast.
    /// On success the RAM override is dropped and the photo refreshed from disk, so
    /// the displayed orientation now comes from the file (a clean round-trip with no
    /// double-rotation).
    fn save_rotation(&mut self) {
        let Some(item) = self.core.displayed_item else {
            return;
        };
        let rot = self.core.rotations.get(&item).copied().unwrap_or_default();
        if rot == Rotation::default() {
            self.core.show_toast("No rotation to save");
            return;
        }
        let Some(path) = self.core.source.path(item).map(Path::to_path_buf) else {
            // Archive entry — no file on disk to write back to.
            self.core.show_toast("Can't save rotation here");
            return;
        };
        if !save_rotation::is_orientation_writable(&path) {
            self.core.show_toast("Save rotation: JPEG only");
            return;
        }
        // Capture the file's orientation *before* the write so the save can be reversed
        // (Edit ▸ Undo) by restoring this exact value.
        let prev = save_rotation::read_orientation(&path);
        match save_rotation::write_orientation(&path, rot) {
            Ok(_) => {
                // The rotation is now baked into the file's EXIF: drop the RAM
                // override and re-read from disk so the pixels are re-oriented from
                // the file (else the ring's old-orientation pixels + a reset view
                // would show it un-rotated, or a later re-decode would double-rotate).
                self.core.rotations.remove(&item);
                self.core.meta_cache.remove(&item);
                self.core.exif_cache.remove(&item); // the file's EXIF (Orientation) just changed
                self.core.failed.remove(&item);
                self.core.preview_resident.remove(&item);
                self.core.upgrade_done.remove(&item);
                self.core.invalidate_geometry();
                self.core.load_current_sync();
                self.core.target_item = self.core.playlist.current();
                self.core.request_prefetch();
                self.core.undo_stack.push(UndoAction::SaveRotation {
                    item,
                    path: path.clone(),
                    prev,
                });
                self.core.show_toast_icon("", Some(icon::assets::FLOPPY));
            }
            Err(e) => {
                eprintln!("save rotation failed: {}: {e}", path.display());
                self.core.show_toast("Save failed");
            }
        }
    }

    /// Reverse the most recent reversible edit (Edit ▸ Undo / `Ctrl+Z`). Today the only
    /// entry kind is a saved rotation: rewrite the file's EXIF Orientation back to the
    /// value it held before the save, then refresh so the reverted file is re-read
    /// (`invalidate_geometry` rebuilds the ring, so neighbors re-decode from disk too —
    /// the undone photo shows correctly whether or not it's the one on screen). On a
    /// write failure the file is untouched, so the entry is pushed back to retry.
    fn undo(&mut self) {
        let Some(action) = self.core.undo_stack.pop() else {
            self.core.show_toast("Nothing to undo");
            return;
        };
        match action {
            UndoAction::SaveRotation { item, path, prev } => {
                match save_rotation::set_orientation(&path, prev) {
                    Ok(()) => {
                        self.core.rotations.remove(&item);
                        self.core.meta_cache.remove(&item);
                        self.core.exif_cache.remove(&item); // EXIF Orientation reverted on disk
                        self.core.failed.remove(&item);
                        self.core.preview_resident.remove(&item);
                        self.core.upgrade_done.remove(&item);
                        self.core.invalidate_geometry();
                        self.core.load_current_sync();
                        self.core.target_item = self.core.playlist.current();
                        self.core.request_prefetch();
                        self.core
                            .show_toast_icon("Rotation undone", Some(icon::assets::UNDO));
                    }
                    Err(e) => {
                        eprintln!("undo rotation failed: {}: {e}", path.display());
                        self.core.show_toast("Undo failed");
                        // The file wasn't changed, so the edit is still reversible —
                        // keep it on the stack for a retry.
                        self.core
                            .undo_stack
                            .push(UndoAction::SaveRotation { item, path, prev });
                    }
                }
            }
        }
    }

    /// Delete the current photo (task #28). `permanent` (`Shift+Del`) removes the
    /// file after a confirmation; otherwise (`Del`) it goes to the Recycle Bin —
    /// recoverable, no prompt. The first op that *removes* a user's file: explicit
    /// command only (CLAUDE.md boundary). Only real files (not archive entries) can be
    /// deleted. After deletion the playlist drops the item and advances to the next
    /// photo (the previous if it was the last; the empty state if none remain).
    fn delete_current(&mut self, permanent: bool) {
        // Settle any still-pending delete-advance first (e.g. a rapid second Del).
        self.core.flush_pending_delete();
        let Some(item) = self.core.displayed_item else {
            return;
        };
        let Some(path) = self.core.source.path(item).map(Path::to_path_buf) else {
            self.core.show_toast("Can't delete this"); // archive entry — no file
            return;
        };
        if permanent {
            // Permanent delete is irreversible — confirm first, via the themed egui
            // dialog (dark-aware, and cross-platform for the macOS port). The delete
            // runs when the dialog answers Yes (`dialog_event`), on this item.
            let name = file_name_of(self.core.source.name(item));
            self.core.pending_confirm_delete = Some(item);
            self.open_confirm_delete(&name);
            return;
        }
        self.do_delete(item, &path, false);
    }

    /// Perform the actual deletion of `item` (`path`) — recoverable (Recycle Bin) or
    /// permanent — then flash an icon-only pill on the still-shown photo and defer the
    /// playlist advance a beat (`DELETE_ADVANCE_DELAY`) so the feedback registers
    /// first. The permanent path reaches here only after the confirm dialog's Yes.
    fn do_delete(&mut self, item: usize, path: &Path, permanent: bool) {
        let res = if permanent {
            delete::delete_permanently(path)
        } else {
            delete::send_to_trash(path)
        };
        if let Err(e) = res {
            eprintln!("delete failed: {}: {e}", path.display());
            self.core.show_toast("Delete failed");
            return;
        }
        // Deleting a playing animation stops playback so the doomed photo freezes on
        // its current frame under the trash icon (rather than animating until removal).
        self.core.stop_playback();
        // Recycle-bin icon for the recoverable delete, trash for a permanent one.
        let icon = if permanent {
            icon::assets::TRASH
        } else {
            icon::assets::RECYCLE
        };
        self.core.show_toast_icon("", Some(icon));
        self.core.pending_delete = Some((Instant::now() + DELETE_ADVANCE_DELAY, item));
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
        self.open_plan(plan.source, plan.cursor);
    }

    /// Route a planned open to the right path — archives async, folder scans stream, an
    /// explicit list resolves inline. Shared by runtime opens ([`open_input`](App::open_input))
    /// and the deferred startup launch ([`resumed`](App::resumed)), so the startup recursive
    /// override (carried on the plan) is honored on both paths.
    fn open_plan(&mut self, source: Source, cursor: open::Cursor) {
        // Archives open via the async-aware path (a .7z decompresses off-thread so it
        // can't freeze the loop).
        if let Source::Archive(path) = &source {
            self.begin_archive_open(path.clone(), None);
            return;
        }
        // Folder scans also run OFF the event loop and **stream** in: walking a large/nested
        // tree (the worst case is opening `~/Library`) can take seconds, and doing it
        // synchronously beachballed the run loop and then crashed the app. The first batch
        // shows a photo almost immediately; the current view stays put until it lands
        // (`poll_dir_scan`).
        if matches!(source, Source::Scan { .. }) {
            self.begin_dir_scan(source, cursor);
            return;
        }
        // An explicit file list is finite (no directory walk), so resolve it inline.
        let r = resolve_playlist(&source, &cursor);
        if r.source.is_empty() {
            eprintln!("PhotoBlaze: no supported images in that selection");
            return;
        }
        self.core
            .rebuild_playlist(r.source, r.root, r.scan_root, r.recursive, r.start);
    }

    /// Start opening an archive at runtime (picker / drag-drop / a deferred launch).
    /// A `.zip` opens synchronously (just a directory read). A `.7z` is opened on a
    /// background thread after a synchronous RAM pre-flight: the current photo stays
    /// visible and the loop stays responsive until the eager decompress lands
    /// (picked up in [`poll_archive_load`](App::poll_archive_load)). A second open
    /// supersedes the first via `archive_gen`.
    ///
    /// `password` decrypts an encrypted archive: `None` on the first open (an
    /// encrypted archive then reports `PasswordRequired`, which prompts), `Some` when
    /// re-opening with a password the user entered.
    fn begin_archive_open(&mut self, path: PathBuf, password: Option<String>) {
        let is_7z = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("7z"));
        let was_password_attempt = password.is_some();
        // Anti-stacking: cancel any open already in flight before starting another, so
        // two eager 7z decompresses never run (and pile up RAM) at once — the original
        // hang's worst case was the user re-triggering a "never-finishing" open and
        // stacking full-archive workers. The superseded worker stops at its next entry
        // boundary and frees its partial buffers; its result is dropped (rx replaced).
        if let Some(prev) = self.archive_load.as_ref() {
            prev.progress.request_cancel();
        }
        if !is_7z {
            let result = open_archive(&path, password);
            self.finish_archive_open(result, was_password_attempt, path);
            return;
        }
        // 7z: refuse instantly if it won't fit RAM (before any background work). A
        // pre-flight password error (a header-encrypted archive) routes to the prompt
        // like any other, not the generic error dialog.
        if let Err(e) = seven_z_preflight(&path, password.as_deref()) {
            self.finish_archive_open(Err(e), was_password_attempt, path);
            return;
        }
        self.archive_gen += 1;
        let generation = self.archive_gen;
        let progress = pb_source::OpenProgress::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        let worker_progress = progress.clone();
        std::thread::spawn(move || {
            let result = load_seven_z(&worker_path, password, &worker_progress);
            let _ = tx.send((generation, result));
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
            was_password_attempt,
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
                let (path, was_attempt) = match load {
                    Some(l) => (l.path, l.was_password_attempt),
                    None => return,
                };
                self.finish_archive_open(result, was_attempt, path);
            }
            Err(TryRecvError::Empty) => {} // still loading
            Err(TryRecvError::Disconnected) => self.archive_load = None, // worker died
        }
    }

    /// Act on a finished archive open (zip-sync or 7z-async), shared by both paths:
    /// a non-empty success rebuilds the playlist (closing any password prompt); a
    /// `PasswordRequired` opens (or re-prompts, after a wrong attempt) the password
    /// dialog; any other failure shows the error dialog. `was_password_attempt` is
    /// whether this open carried a user-entered password (so a repeat means it was
    /// wrong).
    fn finish_archive_open(
        &mut self,
        result: Result<Resolved, archive::ArchiveOpenError>,
        was_password_attempt: bool,
        path: PathBuf,
    ) {
        match result {
            Ok(r) if !r.source.is_empty() => {
                self.password_archive = None;
                self.close_dialog();
                self.core
                    .rebuild_playlist(r.source, r.root, r.scan_root, r.recursive, r.start);
            }
            Ok(_) => self.fail_archive_open(&archive::ArchiveOpenError::Empty),
            Err(archive::ArchiveOpenError::PasswordRequired) => {
                self.prompt_archive_password(path, was_password_attempt)
            }
            // User cancelled: drop quietly, keeping whatever was on screen — no error
            // dialog. The loading dialog is already closed (or closes here as a backstop).
            Err(archive::ArchiveOpenError::Cancelled) => {
                self.password_archive = None;
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
        let root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
        let scan_root = roots.first().cloned();
        let worker_progress = progress.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            stream_scan(
                roots,
                recursive,
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
        self.dir_scan = Some(DirScan {
            generation,
            rx,
            progress,
            name,
            started: Instant::now(),
            bootstrapped: false,
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
                    // Drop any photos the user deleted mid-scan (the worker's cumulative list
                    // still has them). A no-op — returns the snapshot untouched — when nothing
                    // was deleted, which is the common case.
                    let resolved = self.filter_deleted(resolved);
                    let bootstrapped = self
                        .dir_scan
                        .as_ref()
                        .map(|s| s.bootstrapped)
                        .unwrap_or(true);
                    if resolved.source.is_empty() {
                        continue; // nothing to show yet (shouldn't happen — worker skips empties)
                    }
                    if !bootstrapped {
                        // First non-empty batch: show a photo now (display + decode). The
                        // Scanning dialog (if it had been revealed) stays until Done so a
                        // genuinely slow walk keeps its progress + Cancel; once the chip
                        // lands it'll demote to ambient at this point instead.
                        if let Some(s) = self.dir_scan.as_mut() {
                            s.bootstrapped = true;
                        }
                        self.core.rebuild_playlist(
                            resolved.source,
                            resolved.root,
                            resolved.scan_root,
                            resolved.recursive,
                            resolved.start,
                        );
                    } else {
                        // Later batch: grow the playlist in place, keeping the displayed
                        // photo and every per-image cache (indices are append-only).
                        self.core.extend_playlist(resolved.source);
                    }
                }
                Ok((generation, ScanUpdate::Done)) => {
                    if generation != cur_gen {
                        continue; // superseded
                    }
                    let scan = self.dir_scan.take();
                    self.core.scanning = false; // deck is final — resume normal prefetch below
                    self.close_scanning_dialog(); // walk finished — drop the progress dialog
                    if scan.is_some_and(|s| !s.bootstrapped) {
                        eprintln!("PhotoBlaze: no supported images in that selection");
                        // Nothing was ever shown and the scan found nothing: restore the
                        // "Press O to open" hint the scan had suppressed (a bare-folder launch
                        // onto an empty folder), but never blank an existing photo.
                        if self.core.source.is_empty() {
                            self.core.show_open_hint();
                        }
                    }
                    // Deck is final now: resume normal prefetch (random-ahead warm again).
                    self.core.request_prefetch();
                    return;
                }
                Err(TryRecvError::Empty) => {
                    // Still scanning and nothing on screen yet: once the walk is slow enough
                    // to notice, reveal the Scanning dialog (count + current folder + Cancel).
                    // Gated on `!bootstrapped` so it never pops over an already-shown photo,
                    // and only when no other dialog is up (don't steal a Settings/Message
                    // window the user opened over a background scan).
                    let reveal = self.dir_scan.as_ref().is_some_and(|s| {
                        !s.bootstrapped && s.started.elapsed() >= SCAN_DIALOG_DELAY
                    });
                    if reveal && self.dialog.is_none() {
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
        self.password_archive = None;
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
        self.password_archive = Some(path.clone());
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
        });
    }

    /// Surface an archive-open failure to the user via the egui message dialog
    /// (too-large / corrupt / password / OOM / empty), and log it.
    fn report_archive_error(&mut self, e: &archive::ArchiveOpenError) {
        let msg = e.user_message();
        eprintln!("PhotoBlaze: {msg}");
        self.open_message(&msg);
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

    /// Toggle macOS **native (Spaces) fullscreen** — the green-button / ⌃⌘F behavior —
    /// as a deliberate alternative to our borderless speed mode (F / ⌥⏎ / F11). winit's
    /// `Fullscreen::Borderless(None)` maps to AppKit's `toggleFullScreen:` on macOS.
    /// Driven from our "Enter Full Screen" menu item (⌃⌘F). The Enter/Exit label is kept
    /// in sync separately (via `App::apply_menu_state`), reading the real window
    /// state — so it stays correct even for the green-button / gesture toggles.
    #[cfg(target_os = "macos")]
    fn toggle_native_fullscreen(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        if window.fullscreen().is_some() {
            window.set_fullscreen(None);
        } else {
            window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        }
    }

    /// macOS: when the window has moved to a display with different EDR headroom (a
    /// multi-monitor setup where one panel is HDR and another isn't), reconfigure the
    /// CAMetalLayer + the renderer's highlight roll-off for the new screen and repaint.
    /// Cheap when nothing changed (one `NSScreen` query, no re-poke). Driven from
    /// `WindowEvent::Moved`, which fires throughout a drag.
    #[cfg(target_os = "macos")]
    fn reconfigure_edr_for_display(&mut self) {
        let changed = match (self.core.renderer.as_ref(), self.window.as_ref()) {
            (Some(r), Some(w)) if r.hdr_surface_wants_edr().is_some() => {
                let hr = hdr_surface::window_max_edr(w);
                if (hr - self.last_edr_headroom).abs() > 0.01 {
                    // Different display HDR capability — re-poke the layer (colorspace
                    // + wantsEDR) for the new screen, then update the roll-off below.
                    hdr_surface::configure(w);
                    Some(hr)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(hr) = changed {
            if let Some(r) = self.core.renderer.as_mut() {
                r.set_edr_headroom(hr);
            }
            self.last_edr_headroom = hr;
            self.core.draw();
        }
    }

    /// Toggle between borderless "windowed fullscreen" and a 1280x800 window (F11
    /// or Alt+Enter). The resulting resize event re-fits and re-decodes the photo.
    /// NOTE: there is a brief flip-model resize artifact (the photo stretches for a
    /// frame as the compositor scales the old buffer to the new size). A DWM-cloak
    /// fix removed it but regressed the taskbar (shell stopped auto-hiding it on
    /// fullscreen) and added a blank beat — net worse — so it was reverted; the
    /// minor flash is accepted (see tasks.json #21 for the proper-fix direction).
    fn toggle_fullscreen(&mut self) {
        self.windowed = !self.windowed;
        // Record the new mode as the remembered last state, in memory and on disk, so
        // `StartupMode::Remember` restores it and the Settings dialog stays in sync.
        self.core.settings.fullscreen = !self.windowed;
        // Leaving windowed mode: snapshot where the window is now, so toggling back
        // (and the next launch) restore this spot rather than the OS default corner (#1).
        if !self.windowed {
            self.capture_windowed_geometry();
        }
        // Persist the new mode + remembered geometry together (one atomic write). An
        // explicit user action (the toggle), never the view path — privacy #2.
        self.core.geometry_save_at = None;
        self.core.settings.save();

        // The window ops (fullscreen/decorations/sizing + macOS chrome + menu attach) are
        // shell work: emit the mode change and let the drain apply it (`apply_window_mode`),
        // which reads the `self.windowed` we just flipped. Same event-loop turn, so behavior
        // is unchanged.
        self.core
            .effects
            .push(contract::CoreEffect::SetWindowMode(if self.windowed {
                contract::WindowMode::Windowed
            } else {
                contract::WindowMode::Fullscreen
            }));
    }

    /// Shell side of [`CoreEffect::SetWindowMode`]: apply the borderless-fullscreen ⇄ windowed
    /// window ops, run from the drain. Reads the already-flipped `self.windowed` (the effect's
    /// `WindowMode` payload is the same signal). Preserves `toggle_fullscreen`'s exact former
    /// sequence — set_fullscreen/decorations, then macOS chrome + menu attach *before* the
    /// windowed sizing, then the windowed geometry restore.
    fn apply_window_mode(&mut self) {
        // Clone the window handle (an Arc) so it can be driven while `self` is still borrowed
        // mutably below (the menu attach needs `&mut self`).
        let Some(window) = self.window.clone() else {
            return;
        };
        if self.windowed {
            window.set_fullscreen(None);
            window.set_decorations(true);
        } else {
            // Borderless "windowed fullscreen": size a decoration-less window
            // to the monitor ourselves instead of the OS fullscreen API, which
            // makes Windows apply fullscreen-optimizations that drop DWM
            // composition on focus changes / transitions and flash the legacy
            // basic-theme caption. A plain borderless window stays composited.
            window.set_fullscreen(None);
            window.set_decorations(false);
            if let Some(mon) = window.current_monitor() {
                window.set_outer_position(mon.position());
                let _ = window.request_inner_size(mon.size());
            }
        }
        // macOS: auto-hide the menu bar + Dock in borderless fullscreen so it reclaims
        // that strip (chromeless) while staying in the current Space; restore them when
        // returning to windowed mode.
        #[cfg(target_os = "macos")]
        macos_chrome::set_chromeless(!self.windowed);
        // Show the menu in windowed mode, hide it in fullscreen (the chrome-free
        // speed mode). Adding/removing the bar resizes the client area → a `Resized`
        // event → the debounced re-decode path. Done *before* the windowed sizing
        // below so a restored client size already accounts for the menu bar's height
        // (sizing pre-menu would lose that height on every toggle — a slow drift).
        self.apply_menu_for_mode();

        if self.windowed {
            // Restore the saved windowed geometry when enough of it still lands on a
            // connected monitor; otherwise fall back to the default size at the
            // OS-chosen spot (so a stale off-screen position can't strand the window).
            let rects = collect_monitor_rects(window.available_monitors());
            match self.core.windowed_restore(&rects) {
                Some(g) => {
                    let _ = window.request_inner_size(PhysicalSize::new(g.w, g.h));
                    window.set_outer_position(PhysicalPosition::new(g.x, g.y));
                }
                None => {
                    let _ = window.request_inner_size(PhysicalSize::new(1280, 800));
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
        if !self.windowed {
            return;
        }
        let before = self.core.settings.window;
        self.capture_windowed_geometry();
        if self.core.settings.window != before {
            self.core.geometry_save_at = Some(Instant::now() + Duration::from_millis(500));
        }
    }

    /// Build the native menu bar once (cross-platform; muda owns the OS handle).
    fn ensure_menu(&mut self) {
        if self.menu.is_none() {
            let built = menu::build_menu(&self.core.keymap);
            self.menu = Some(built.menu);
            self.save_rotation_item = Some(built.save_rotation);
            self.cancel_scan_item = Some(built.cancel_scan);
            self.undo_item = Some(built.undo);
            self.view_checks = Some(built.checks);
            #[cfg(target_os = "macos")]
            {
                self.window_menu = Some(built.window);
                self.native_fullscreen_item = Some(built.native_fullscreen);
            }
        }
    }

    /// Whether "Save Rotation" applies right now: the displayed photo has an unsaved
    /// (non-upright) rotation override, sits on a real file on disk (not an archive
    /// entry), and that file's format supports a lossless EXIF Orientation rewrite.
    fn can_save_rotation(&self) -> bool {
        let Some(item) = self.core.displayed_item else {
            return false;
        };
        let rotated = self
            .core
            .rotations
            .get(&item)
            .is_some_and(|r| *r != Rotation::default());
        rotated
            && self
                .core
                .source
                .path(item)
                .is_some_and(save_rotation::is_orientation_writable)
    }

    /// Derive the current [`contract::MenuState`] from live app state and, **only when it
    /// changed** since the last one applied (`self.menu_state`), emit a single
    /// [`CoreEffect::SetMenuState`] — the shell mirrors it onto the native menu in the drain
    /// (`apply_menu_to_native`). This is the core side of the menu seam (NS0, ADR-021): the
    /// core decides *what* the menu should read; it never touches a muda handle. The change
    /// gate keeps this off the per-tick path (nothing is pushed when nothing moved), so it's
    /// safe to call every tick from `about_to_wait`; a no-op until the menu exists.
    fn apply_menu_state(&mut self) {
        // No menu yet (not built): nothing to mirror, and don't cache — so the first apply
        // once the items exist re-asserts every one of them from scratch. All menu handles
        // are built together in `ensure_menu`, so `view_checks` gates them all.
        if self.view_checks.is_none() {
            return;
        }
        // Native (Spaces) fullscreen is OS truth (the real `NSWindow.styleMask` via
        // `hdr_surface::window_is_fullscreen`), not winit's requested-mode flag — read
        // every tick so a green-button / gesture toggle flips the label too. Windows has
        // no such menu item, so it's always `false` there.
        #[cfg(target_os = "macos")]
        let native_fullscreen = self
            .window
            .as_ref()
            .is_some_and(|a| hdr_surface::window_is_fullscreen(a));
        #[cfg(not(target_os = "macos"))]
        let native_fullscreen = false;

        let next = AppCore::menu_state_from(
            self.core.view.mode,
            self.core.info,
            self.core.recursive,
            !self.windowed, // `windowed` is the inverse of the fullscreen checkbox
            self.core.slideshow.on,
            self.core.settings.mute_live_audio,
            self.can_save_rotation(),
            self.dir_scan.is_some(),
            // `None` = nothing to undo (disabled "Undo"); `Some(label)` = enabled w/ label.
            self.core.undo_stack.last().map(UndoAction::menu_label),
            native_fullscreen,
        );
        // Only sync when the state actually changed — kept off the per-tick path (nothing is
        // pushed when nothing moved). The shell applies it in the drain via
        // `apply_menu_to_native`; `MenuState` is `Copy`, so there's no alloc here.
        if self.menu_state == Some(next) {
            return;
        }
        self.menu_state = Some(next);
        self.core
            .effects
            .push(contract::CoreEffect::SetMenuState(next));
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
            c.mute_live_audio.set_checked(state.mute_live_audio);
            c.info
                .set_checked(state.info == contract::InfoOverlay::Basic);
            c.full_exif
                .set_checked(state.info == contract::InfoOverlay::FullExif);
        }
        // File ▸ Save Rotation enabled state.
        if let Some(it) = self.save_rotation_item.as_ref() {
            it.set_enabled(state.save_rotation_enabled);
        }
        // File ▸ Stop Scanning enabled state.
        if let Some(it) = self.cancel_scan_item.as_ref() {
            it.set_enabled(state.cancel_scan_enabled);
        }
        // Edit ▸ Undo title + enabled state (Windows appends the `\tCtrl+Z` hint; macOS
        // shows the real ⌘Z key-equivalent the item already carries).
        if let Some(it) = self.undo_item.as_ref() {
            let base = state.undo.unwrap_or("Undo");
            #[cfg(target_os = "macos")]
            it.set_text(base);
            #[cfg(not(target_os = "macos"))]
            it.set_text(format!("{base}\tCtrl+Z"));
            it.set_enabled(state.undo.is_some());
        }
        // macOS: native (Spaces) fullscreen item title ("Enter"/"Exit Full Screen") — a
        // title toggle, never a checkmark (the Mac convention).
        #[cfg(target_os = "macos")]
        if let Some(it) = self.native_fullscreen_item.as_ref() {
            it.set_text(if state.native_fullscreen_engaged {
                "Exit Full Screen"
            } else {
                "Enter Full Screen"
            });
        }
    }

    /// macOS: keep the title-bar **proxy icon** (the window's represented file) pointed
    /// at the displayed photo, so it shows the file's Finder icon and can be dragged out.
    /// Only in windowed mode (the borderless speed mode has no title bar), and only for a
    /// real on-disk file — an archive entry or the empty state clears it. Cached so the
    /// per-tick call is a no-op `setRepresentedURL:` until the displayed photo changes,
    /// keeping it off the hold-to-fly hot path. RAM-only, never persisted (privacy #2).
    #[cfg(target_os = "macos")]
    fn refresh_proxy_icon(&mut self) {
        let want = if self.windowed {
            self.core
                .displayed_item
                .and_then(|i| self.core.source.path(i))
                .map(Path::to_path_buf)
        } else {
            None
        };
        if self.proxy_icon_path == want {
            return;
        }
        if let Some(a) = self.window.as_ref() {
            proxy_icon::set_represented_url(a, want.as_deref());
        }
        self.proxy_icon_path = want;
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
            if self.windowed {
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

    /// On non-Windows platforms the menu isn't wired up yet (macOS uses
    /// `init_for_nsapp` — a future cheap port), so this is a no-op.
    /// macOS: the menu bar is the app-global `NSMenu` (one `init_for_nsapp`), not a
    /// per-window `HMENU`. Attach it once and leave it — macOS auto-hides the bar in
    /// fullscreen while keeping the ⌘ key-equivalents live, so there's no per-mode
    /// show/hide and nothing to do once attached.
    #[cfg(target_os = "macos")]
    fn apply_menu_for_mode(&mut self) {
        self.ensure_menu();
        if !self.menu_attached {
            if let Some(menu) = self.menu.as_ref() {
                menu.init_for_nsapp();
                // Must run *after* `init_for_nsapp` (muda requirement): hand the Window
                // submenu to AppKit as the app's Window menu, so macOS appends the live
                // window list under our Minimize / Zoom / Bring All to Front items.
                if let Some(window) = self.window_menu.as_ref() {
                    window.set_as_windows_menu_for_nsapp();
                }
                self.menu_attached = true;
            }
        }
    }

    /// Other platforms (Linux/X11/Wayland): no native menu wired yet.
    #[cfg(not(any(windows, target_os = "macos")))]
    fn apply_menu_for_mode(&mut self) {}

    /// React to a runtime OS light↔dark theme change: re-flush the popup menu
    /// themes and nudge muda to re-evaluate `Auto` and repaint the bar in the new
    /// theme. (The bar usually repaints on its own; this also covers the popups.)
    #[cfg(windows)]
    fn refresh_menu_theme(&self) {
        darkmode::flush_menu_themes();
        if !self.menu_attached || !self.windowed {
            return;
        }
        if let Some(a) = self.window.as_ref() {
            if let (Some(menu), Some(hwnd)) = (self.menu.as_ref(), hwnd_of(a)) {
                // SAFETY: the menu is attached to this live window's valid handle.
                unsafe {
                    let _ = menu.set_theme_for_hwnd(hwnd, muda::MenuTheme::Auto);
                }
            }
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
            Action::SaveRotation => self.save_rotation(),
            Action::Delete => self.delete_current(false),
            Action::DeletePermanent => self.delete_current(true),
            Action::Undo => self.undo(),
            Action::Fullscreen => self.toggle_fullscreen(),
            Action::Recursive => self.toggle_recursive(),
            Action::CancelScan => self.cancel_scan_command(),
            Action::MuteLiveAudio => self.toggle_mute_audio(),
            Action::Settings => self.open_settings(),
            Action::About => self.open_about(),
            Action::Quit => self.begin_exit(),
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

    /// Filter a streamed snapshot through the delete-tombstone set, rebuilding its `FsSource`
    /// without the deleted paths. Returns the snapshot **unchanged** (no allocation) in the
    /// common case where nothing was deleted mid-scan. Because the walk is append-only and we
    /// remove the *same* paths from every snapshot, the filtered result stays a prefix-superset
    /// of the current playlist — so the in-place [`extend_playlist`](App::extend_playlist) is
    /// still valid (the displayed photo's index doesn't shift). O(N) only when tombstones
    /// exist (rare).
    fn filter_deleted(&self, r: Resolved) -> Resolved {
        if self.core.deleted.is_empty() {
            return r;
        }
        let paths: Vec<PathBuf> = (0..r.source.len())
            .filter_map(|i| r.source.path(i).map(Path::to_path_buf))
            .filter(|p| !self.core.deleted.contains(p))
            .collect();
        let start = r.start.min(paths.len().saturating_sub(1));
        Resolved {
            source: Arc::new(FsSource::new(paths)),
            root: r.root,
            scan_root: r.scan_root,
            recursive: r.recursive,
            start,
        }
    }

    /// Open the "About PhotoBlaze" dialog (Help menu) — an egui window with the app
    /// icon + version, dark-mode-aware (see `dialog`).
    fn open_about(&mut self) {
        self.open_dialog(dialog::DialogKind::About);
    }

    /// Open the Settings dialog (Ctrl+,) — an egui window seeded from the live
    /// settings; **Save** routes back to [`apply_settings`](Self::apply_settings).
    fn open_settings(&mut self) {
        self.open_dialog(dialog::DialogKind::Settings);
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
        });
    }

    /// Open a one-button informational / error notice (egui `DialogKind::Message`):
    /// a warning icon + `message` + an OK button, centered over the viewer, closing
    /// on OK / Esc. The archive-open path (`archive::ArchiveOpenError::user_message`)
    /// calls this to surface a too-large / corrupt / password / OOM / empty failure.
    pub fn open_message(&mut self, message: &str) {
        self.pending_dialog = Some(DialogRequest::Simple {
            kind: dialog::DialogKind::Message,
            message: message.to_string(),
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
            self.handle_dialog_outcome(DialogOutcome::Dismissed(kind));
            return;
        }
        // Render the dialog and pick up a button answer (if one was clicked).
        let kind = self.dialog.as_ref().map(|d| d.kind());
        let mut answer: Option<bool> = None;
        if let Some(d) = self.dialog.as_mut() {
            let repaint = d.on_event(&event);
            match &event {
                WindowEvent::Resized(size) => {
                    d.resize(*size);
                    d.request_redraw();
                }
                WindowEvent::RedrawRequested => {
                    d.render();
                    answer = d.take_confirm_result();
                }
                _ => {
                    if repaint {
                        d.request_redraw();
                    }
                }
            }
        }
        if let Some(confirmed) = answer {
            // Turn the button answer into a shell-neutral outcome, extracting any egui-side
            // payload here (password / edited settings + keymap); the core reacts in
            // `handle_dialog_outcome`.
            let outcome = match kind {
                Some(dialog::DialogKind::Password) if confirmed => {
                    DialogOutcome::PasswordSubmitted(
                        self.dialog
                            .as_mut()
                            .and_then(|d| d.take_submitted_password()),
                    )
                }
                Some(dialog::DialogKind::Password) => DialogOutcome::PasswordCancelled,
                Some(dialog::DialogKind::Settings) if confirmed => {
                    let (settings, keymap) = self.dialog.as_mut().map_or((None, None), |d| {
                        (d.take_settings_result(), d.take_keymap_result())
                    });
                    DialogOutcome::SettingsSaved { settings, keymap }
                }
                Some(dialog::DialogKind::Settings) => DialogOutcome::SettingsCancelled,
                Some(dialog::DialogKind::Loading) => DialogOutcome::LoadingCancelled,
                Some(dialog::DialogKind::Scanning) => DialogOutcome::ScanningCancelled,
                Some(dialog::DialogKind::Confirm) => DialogOutcome::ConfirmAnswered(confirmed),
                _ => DialogOutcome::Closed,
            };
            self.handle_dialog_outcome(outcome);
        }
    }

    /// React to a [`DialogOutcome`] the shell extracted from the dialog window — the core
    /// half of the dialog-result seam (NS0 step 4d). Mirrors [`App::open_pending_dialog`]
    /// (the open side). Behavior is exactly the former inline `dialog_event` dispatch; only
    /// the egui/winit event handling + payload extraction stays shell-side. (NS-later: the
    /// `self.dialog = None` closes and the archive path's `become_loading` reach-in become
    /// effects once the archive/scan flow inverts with step 5.)
    fn handle_dialog_outcome(&mut self, outcome: DialogOutcome) {
        match outcome {
            DialogOutcome::Dismissed(kind) => {
                // The Esc that dismisses a (focused) dialog also leaks to the main window as a
                // trailing/synthetic press once focus snaps back — by then `dialog` is None, so
                // the main-window guard can't catch it. Briefly guard quit-on-Esc so closing a
                // dialog never also exits the app (the same leak `open_picker` handles).
                self.core.esc_guard_until = Some(Instant::now() + Duration::from_millis(300));
                // Esc / close on the loading view cancels the in-flight open (the worker stops
                // and frees its partial RAM); harmless for the other kinds.
                self.cancel_archive_load();
                // Esc / close on the scanning view cancels the in-flight folder walk and discards
                // its partial result. Guarded to the Scanning kind so closing a *different* dialog
                // doesn't kill a fast scan still running quietly in the background (one dispatched
                // <SCAN_DIALOG_DELAY ago, before any dialog).
                if kind == Some(dialog::DialogKind::Scanning) {
                    self.cancel_dir_scan();
                    self.dir_scan = None;
                }
                self.dialog = None;
                self.core.pending_confirm_delete = None; // Esc / close = cancel the confirm
                self.password_archive = None; // Esc / close = abandon the password prompt
            }
            // The password dialog stays open until the open succeeds or is cancelled (a wrong
            // password re-prompts in place), so it isn't closed here like the others.
            DialogOutcome::PasswordSubmitted(pw) => match (pw, self.password_archive.clone()) {
                (Some(pw), Some(path)) => {
                    // Show the "Checking…" state, then validate (zip is synchronous; a 7z
                    // re-opens off-thread).
                    if let Some(d) = self.dialog.as_mut() {
                        d.set_checking(true);
                        d.request_redraw();
                    }
                    self.begin_archive_open(path, Some(pw));
                }
                // No archive pending (shouldn't happen): just close.
                _ => self.dialog = None,
            },
            DialogOutcome::PasswordCancelled => {
                // Cancel: close and forget the pending archive.
                self.dialog = None;
                self.password_archive = None;
            }
            // Settings: Save applies + persists the edited model; Cancel/Esc discard.
            DialogOutcome::SettingsSaved { settings, keymap } => {
                self.dialog = None;
                if let Some(new) = settings {
                    self.core.apply_settings(new);
                }
                if let Some(km) = keymap {
                    self.core.apply_keymap(km);
                }
            }
            DialogOutcome::SettingsCancelled => self.dialog = None,
            // Loading: the only button is Cancel (which already requested cancellation); make
            // sure the in-flight open stops, then close. The worker returns Cancelled and
            // `poll_archive_load` tidies up.
            DialogOutcome::LoadingCancelled => {
                self.cancel_archive_load();
                self.dialog = None;
                self.password_archive = None;
            }
            // Scanning: the only button is Cancel (which already requested cancellation); stop
            // the walk, discard its partial result, and close — a cancelled scan must keep the
            // current view, not load a half-walked tree.
            DialogOutcome::ScanningCancelled => {
                self.cancel_dir_scan();
                self.dir_scan = None;
                self.dialog = None;
            }
            // Confirm drives a delete.
            DialogOutcome::ConfirmAnswered(confirmed) => {
                self.dialog = None;
                let item = self.core.pending_confirm_delete.take();
                if confirmed {
                    if let Some(item) = item {
                        if let Some(path) = self.core.source.path(item).map(Path::to_path_buf) {
                            self.do_delete(item, &path, true);
                        }
                    }
                }
            }
            // Message / others just close.
            DialogOutcome::Closed => self.dialog = None,
        }
    }

    /// The ambient **scan status card**: while a folder scan is streaming in (and the first
    /// photo is already up), show a fixed-width card in the top-right (equal inset from the top
    /// and right edges) — `Scanning "Folder"`, the folder currently being walked, the browsable
    /// count (`8,230 images found`), and a centered **Cancel Scan** button. The count *is* the
    /// progress (Codex P3: the **browsable** `source.len()`, not the worker's look-ahead
    /// `found`). Deferred past [`SCAN_DIALOG_DELAY`] so a quick folder never flashes it;
    /// rebuilt only when its content changes and no faster than [`SCAN_CARD_REFRESH`] (the
    /// current-folder line changes per directory); cleared when the scan ends.
    fn tick_chip(&mut self) {
        let want = match (self.dir_scan.as_ref(), self.core.displayed_item) {
            (Some(scan), Some(_))
                if scan.bootstrapped && scan.started.elapsed() >= SCAN_DIALOG_DELAY =>
            {
                // Current folder being walked; hide it while it's just the root (it would
                // duplicate the heading).
                let cur = scan.progress.current();
                let path = if cur == scan.name { String::new() } else { cur };
                Some((scan.name.clone(), path, self.core.source.len()))
            }
            _ => None,
        };
        if want == self.core.chip_sig {
            return;
        }
        // Show/hide is immediate; a content tick (folder/count) is throttled so the software
        // composite stays off the hot path.
        let toggling = want.is_some() != self.core.chip_sig.is_some();
        if !toggling && self.core.chip_built.elapsed() < SCAN_CARD_REFRESH {
            return;
        }
        match &want {
            Some((name, path, count)) => self.core.push_chip(name, path, *count),
            None => self.core.clear_chip(),
        }
        self.core.chip_sig = want;
        self.core.chip_built = Instant::now();
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
                    Ok(()) => self.core.show_toast_icon("", Some(icon::assets::CLIPBOARD)),
                    Err(e) => {
                        eprintln!("copy: clipboard write failed: {e}");
                        self.core.show_toast("Copy failed");
                    }
                }
            }
            contract::ClipboardPayload::Text(text) => {
                let fname = file_name_of(&text).to_string();
                match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
                    Ok(()) => self.core.show_toast(&format!("Copied {fname}")),
                    Err(e) => {
                        eprintln!("copy path: clipboard write failed: {e}");
                        self.core.show_toast("Copy path failed");
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
    /// hold-to-fly frame's total event-loop cost is `present + drain` (window ops the
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
                        if let Some(w) = self.window.as_ref() {
                            w.set_cursor(cursor_icon(kind));
                        }
                    }
                    contract::CoreEffect::SetMenuState(state) => {
                        self.apply_menu_to_native(&state);
                    }
                    contract::CoreEffect::SetWindowMode(_mode) => {
                        self.apply_window_mode();
                    }
                    contract::CoreEffect::WriteClipboard(payload) => {
                        self.write_clipboard(payload);
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
                        exts.push("zip");
                        exts.push("7z");
                        let input = rfd::FileDialog::new()
                            .add_filter("Images & archives", &exts)
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
                    // The core routed a flow action (dialog / window / scan / file edit / quit) it
                    // doesn't own end-to-end yet — run the shell half.
                    contract::CoreEffect::ShellFlowAction(action) => {
                        self.perform_flow_action(action);
                    }
                    // The core's requested next wake (from the Tick handler). Stored, not applied
                    // here — `about_to_wait` mins it with the shell's dialog-repaint deadline for
                    // the event loop's control-flow.
                    contract::CoreEffect::SetWake(at) => self.requested_wake = at,
                    _ => {}
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
            DialogRequest::Simple { kind, message } => {
                self.dialog = dialog::DialogWindow::open(
                    kind,
                    event_loop,
                    refresh,
                    &message,
                    &self.core.settings,
                    &self.core.keymap,
                    parent.as_deref(),
                );
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
        self.core.live_motion_cache.clear();
        self.core.rotations.clear();
        self.core.failed.clear();
        self.core.preview_resident.clear();
        self.core.upgrade_done.clear();
        self.core.last_upgrade_set.clear();
        self.core.undo_stack.clear();
        self.core.current = None;
        self.core.toast = None;
        self.core.wait_started = None;
        self.core.pie_finish = None;
        self.core.pie_glow_started = None;
        // Drop any on-demand animation playback + in-flight decode (RAM-only — #2).
        self.core.stop_playback();
    }

    // --- Animation playback (task #37) -------------------------------------------------

    /// Toggle Live Photo audio mute (`M` / Image menu). Persists the choice, updates the
    /// menu check + a toast, and takes effect immediately: muting silences a playing clip;
    /// unmuting a currently-playing Live Photo starts its audio at the current position so
    /// it stays in sync.
    fn toggle_mute_audio(&mut self) {
        let muted = !self.core.settings.mute_live_audio;
        self.core.settings.mute_live_audio = muted;
        self.core.settings.save();
        self.menu_state = None; // invalidate the cache so the check re-asserts
        self.apply_menu_state();
        if muted {
            self.live_audio = None; // silence any playing clip now
                                    // An icon-only pill (like the rotate toasts): a slashed speaker = now muted.
            self.core
                .show_toast_icon("", Some(icon::assets::VOLUME_SLASH));
        } else {
            // Unmuting mid-playback: resume audio at the motion's current position.
            if let (Some(pb), Some(item)) = (self.core.playback.as_ref(), self.core.displayed_item)
            {
                if pb.is_playing() {
                    let secs = pb.index() as f64 * pb.total_duration().as_secs_f64()
                        / pb.frame_count().max(1) as f64;
                    self.live_audio = self
                        .core
                        .live_motion_path(item)
                        .and_then(|p| LiveAudio::play(&p, secs));
                }
            }
            // A speaker with waves = now audible.
            self.core.show_toast_icon("", Some(icon::assets::VOLUME));
        }
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
            .with_title("PhotoBlaze")
            .with_visible(false);
        if let Some(icon) = load_window_icon() {
            attrs = attrs.with_window_icon(Some(icon));
        }
        // The saved windowed geometry to restore on launch, if it still lands on a
        // connected monitor (#1) — used as the creation hint and re-applied after the
        // menu attaches below (so the client size accounts for the menu bar).
        let restore = if self.windowed {
            let rects = collect_monitor_rects(event_loop.available_monitors());
            self.core.windowed_restore(&rects)
        } else {
            None
        };
        attrs = if self.windowed {
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
            if self.windowed {
                if let (Some(menu), Some(hwnd)) = (self.menu.as_ref(), hwnd_of(&window)) {
                    // SAFETY: `hwnd` is this freshly-created window's valid handle.
                    unsafe {
                        let _ = menu.init_for_hwnd(hwnd);
                    }
                    self.menu_attached = true;
                }
            }
        }

        // macOS: attach the app-global menu bar once, regardless of windowed/fullscreen
        // (the bar auto-hides in fullscreen but its ⌘-shortcuts stay live). `NSMenu`
        // has no per-window handle, so there's no `init_for_hwnd` equivalent.
        #[cfg(target_os = "macos")]
        self.apply_menu_for_mode();

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
        // Apply the user's saved letterbox color before the first frame paints.
        renderer.set_letterbox(self.core.settings.letterbox);
        let now = window.inner_size();
        if now != isz {
            self.core.fit = Some(FitBox {
                max_width: now.width.max(1),
                max_height: now.height.max(1),
            });
            renderer.resize(now.width, now.height);
            // The real window size differs from what we decoded for — re-decode
            // the first image at the corrected fit so the first frame isn't soft.
            if let Some(idx) = self.core.playlist.current() {
                let t0 = Instant::now();
                // Preview-first (see `load_current_sync`): the full decode lands off-thread.
                let decoded =
                    decode_item(self.core.source.as_ref(), idx, self.core.decode_fit(), true);
                self.core.metrics.record("decode", t0.elapsed());
                if let Ok(img) = decoded {
                    let meta = meta_for(self.core.source.as_ref(), idx, &self.core.root, &img);
                    self.core.current = Some(meta.clone());
                    self.core.meta_cache.insert(idx, meta);
                    renderer.set_image(
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(&img),
                        img.peak,
                    );
                }
            }
        }

        // macOS: configure the CAMetalLayer (scRGB colorspace + EDR) from the screen
        // the *window* is on, and give the renderer that screen's roll-off headroom.
        // After the initial resize (a surface reconfigure can reset the layer) and
        // before the first present, so the first HDR frame is already correct.
        #[cfg(target_os = "macos")]
        if renderer.hdr_surface_wants_edr().is_some() {
            let headroom = hdr_surface::configure(&window);
            renderer.set_edr_headroom(headroom);
            self.last_edr_headroom = headroom;
        }

        // Empty launch (no folder/file given): show a blank background with the centered
        // Open File / Open Folder call to action instead of an image.
        if self.core.playlist.current().is_none() {
            renderer.clear_image();
            if let Some((bitmap, w, h, file, folder)) = self.core.open_panel_bitmap() {
                renderer.set_message(Some((&bitmap, w, h)));
                self.core.open_panel = Some(OpenPanel { w, h, file, folder });
            }
        }

        // Present the first frame WHILE HIDDEN, then reveal — no white startup gap.
        let _ = renderer.render();
        window.set_visible(true);
        window.request_redraw();

        // macOS: a fullscreen launch *is* the borderless mode — auto-hide the menu bar +
        // Dock from the first frame so it's chromeless (toggle_fullscreen handles changes).
        #[cfg(target_os = "macos")]
        if !self.windowed {
            macos_chrome::set_chromeless(true);
        }

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
        self.core.request_prefetch();

        // Now that the window + engine are live, kick off any launch we deferred (an archive
        // or a folder scan): a big .7z loads behind the spinner, a folder streams in (window
        // shows first), and an encrypted / failed open can use the egui dialogs (a synchronous
        // launch resolve, before the event loop, could do none of these).
        if let Some(plan) = self.pending_launch.take() {
            self.core.launching = false; // the deferred launch is firing now
            self.open_plan(plan.source, plan.cursor);
        }
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
        match event {
            WindowEvent::CloseRequested => self.begin_exit(),

            WindowEvent::Resized(size) => {
                self.core.viewport.width = size.width.max(1);
                self.core.viewport.height = size.height.max(1);
                let new_fit = FitBox {
                    max_width: size.width.max(1),
                    max_height: size.height.max(1),
                };
                if Some(new_fit) != self.core.fit {
                    self.core.fit = Some(new_fit);
                    if let Some(r) = self.core.renderer.as_mut() {
                        // Cheap, per-event: reconfigure the swapchain and let the
                        // renderer GPU-scale the resident texture to the new size.
                        r.resize(size.width, size.height);
                        // macOS: a surface reconfigure can reset the CAMetalLayer's
                        // colorspace/EDR, so re-assert them — keeps P3/HDR alive across
                        // a resize, fullscreen toggle, or a move to another display
                        // (which may have different EDR headroom).
                        #[cfg(target_os = "macos")]
                        if r.hdr_surface_wants_edr().is_some() {
                            if let Some(w) = self.window.as_ref() {
                                let headroom = hdr_surface::configure(w);
                                r.set_edr_headroom(headroom);
                                self.last_edr_headroom = headroom;
                            }
                        }
                    }
                    self.core.draw();
                    // A drag fires Resized many times a second; re-decoding the
                    // current photo to the new fit on every one (a CPU decode on
                    // the event-loop thread) is what made resize crawl. Defer the
                    // crisp decode-to-fit + ring refill until the size settles.
                    self.core.resize_settle_at = Some(Instant::now() + Duration::from_millis(180));
                }
                // Remember the new windowed size so it can be restored later (#1).
                self.track_windowed_geometry();
            }

            // The window's backing scale factor changed — a move to a monitor with a
            // different DPI (a 1× display ↔ a 2× Retina one), or a live OS DPI change. Update
            // the factor every CPU-rasterized overlay is sized by, then rebuild them so the
            // overlay text stays crisp at the new DPI. The `Resized` that winit sends right
            // after this reconfigures the swapchain + re-decodes the photo to the new size.
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let sf = scale_factor as f32;
                if (sf - self.core.viewport.scale_factor).abs() > f32::EPSILON {
                    self.core.viewport.scale_factor = sf;
                    self.core.rescale_overlays();
                }
            }

            // Track the windowed position so toggling back / relaunching restores it
            // (#1). A fullscreen window's position is the monitor, not a user choice,
            // so `track_windowed_geometry` ignores it there.
            WindowEvent::Moved(_) => {
                self.track_windowed_geometry();
                // macOS: adapt HDR/EDR if the window crossed onto a different display.
                #[cfg(target_os = "macos")]
                self.reconfigure_edr_for_display();
            }

            WindowEvent::RedrawRequested => self.core.draw(),

            // Drag-and-drop: winit sends one event per file. Coalesce and apply on
            // the next `about_to_wait` tick (a folder browses recursively; dropped
            // photos become the playlist).
            WindowEvent::DroppedFile(path) => {
                self.pending_drops.push(path);
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
                    } else if let Some(key) = pb_key_winit::from_winit(code) {
                        // Translate to a shell-neutral `CoreEvent` and let the core resolve +
                        // route it (`handle`: repeat-gate + ⌘-no-fall-through, then one-shot →
                        // `dispatch_action`, nav → hold-to-fly, held → track, frame-step). This is
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
            // it keeps matching the desktop (the window title bar is winit's).
            WindowEvent::ThemeChanged(_) => {
                #[cfg(windows)]
                self.refresh_menu_theme();
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

            // Pointer left the window: drop any Cancel Scan / open-button / play-hint hover so
            // they don't stay lit.
            WindowEvent::CursorLeft { .. } => {
                self.core.last_cursor = None;
                self.core.update_chip_hover();
                self.core.update_open_hover();
                self.core.update_play_hint_hover();
            }

            // Left button toggles drag-to-pan (the cross-platform pan gesture).
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let pressed = state == ElementState::Pressed;
                // A press on an interactive on-image control (an open-panel button, the play
                // hint, or the scan-count chip's Cancel button) fires that control and must NOT
                // also start a drag-to-pan.
                let open_hit = pressed.then(|| self.core.open_hovered_button()).flatten();
                if let Some(button) = open_hit {
                    match button {
                        OpenButton::File => self.core.dispatch_action(Action::OpenFile),
                        OpenButton::Folder => self.core.dispatch_action(Action::OpenFolder),
                    }
                } else if pressed && self.core.play_hint_hit() {
                    // Click the play hint → play, and dismiss it (it's been used).
                    self.core.play_hint = None;
                    self.core.dispatch_action(Action::PlayPause);
                } else if pressed
                    && self
                        .core
                        .last_cursor
                        .is_some_and(|[cx, cy]| self.core.chip_hit(cx, cy))
                {
                    self.cancel_scan_command();
                } else {
                    self.core.dragging = pressed;
                    self.core.refresh_cursor();
                }
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
                let zooms = self.core.settings.scroll_action == settings::ScrollAction::Zoom;
                let zoom = zooms != self.core.mods.ctrl;
                match delta {
                    MouseScrollDelta::PixelDelta(p) => {
                        if zoom {
                            let factor = (1.0 + p.y as f32 * PIXEL_ZOOM_STEP).max(0.05);
                            self.core.zoom_about_cursor(factor);
                        } else {
                            self.core.pan_by_pixels(
                                p.x as f32 * GESTURE_PAN_DIR,
                                p.y as f32 * GESTURE_PAN_DIR,
                            );
                        }
                    }
                    MouseScrollDelta::LineDelta(x, y) => {
                        if zoom {
                            let factor = (1.0 + y * WHEEL_ZOOM_STEP).max(0.05);
                            self.core.zoom_about_cursor(factor);
                        } else {
                            self.core.pan_by_pixels(
                                x * WHEEL_PAN_STEP * GESTURE_PAN_DIR,
                                y * WHEEL_PAN_STEP * GESTURE_PAN_DIR,
                            );
                        }
                    }
                }
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
        // 0. Native menu-bar clicks (windowed mode). Map each id to the same action
        // the keyboard triggers and dispatch it; an unknown/foreign id is ignored.
        while let Ok(ev) = muda::MenuEvent::receiver().try_recv() {
            // macOS native (Spaces) fullscreen is handled directly, not via `Action`
            // (it's a platform-specific window command, not a portable app action).
            #[cfg(target_os = "macos")]
            if ev.id.as_ref() == menu::ids::NATIVE_FULLSCREEN {
                self.toggle_native_fullscreen();
                continue;
            }
            if let Some(action) = menu::action_for(ev.id.as_ref()) {
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
        // macOS: keep the title-bar proxy icon pointed at the displayed photo (cached).
        #[cfg(target_os = "macos")]
        self.refresh_proxy_icon();
        // Deferred delete-advance: once the icon has shown for a beat, drop the item.
        if self.core.pending_delete.is_some_and(|(at, _)| now >= at) {
            self.core.flush_pending_delete();
        }
        // 0b. Apply any files dropped on the window this burst (coalesced — winit
        // delivers one `DroppedFile` per file).
        if !self.pending_drops.is_empty() {
            let drops = std::mem::take(&mut self.pending_drops);
            self.open_input(classify_inputs(drops));
        }
        // 0b'. macOS: files opened from Finder / the Dock / `open -a` arrive via
        // `application:openURLs:` (winit drops them — see `macos_open`); route them
        // through the same open path as drag-and-drop.
        #[cfg(target_os = "macos")]
        {
            let opened = macos_open::take_opened();
            if !opened.is_empty() {
                self.open_input(classify_inputs(opened));
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

        // The per-tick CORE loop (hold-to-fly / slideshow / prefetch / animation) — the SAME
        // entry the macOS Swift host drives. It pushes `SetWake(core_wake)` (stored in
        // `self.requested_wake` by the drain below).
        self.core.handle(contract::CoreEvent::Tick(now));

        // Execute the tick's effects (SetWake → `requested_wake`, StopLiveAudio, any
        // ShellFlowAction, redraws, …). Must run before we read `requested_wake`.
        self.drain_effects(event_loop);

        // The event loop's next wake: the earliest of the core's requested wake and the shell's
        // own dialog-repaint deadline; `None` = idle until a real event.
        let wake = [self.requested_wake, dialog_wake]
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
fn cursor_icon(kind: contract::CursorKind) -> CursorIcon {
    match kind {
        contract::CursorKind::Default | contract::CursorKind::Hidden => CursorIcon::Default,
        contract::CursorKind::Grab => CursorIcon::Grab,
        contract::CursorKind::Grabbing => CursorIcon::Grabbing,
        contract::CursorKind::Pointer => CursorIcon::Pointer,
    }
}

/// Whether a path's extension is a supported image format (the decoder's single
/// source of truth — see `pb_decode::is_supported_extension`).
fn is_supported_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(is_supported_extension)
        .unwrap_or(false)
}

/// Walk `dir` for supported images, appending them to `out` (unsorted — the caller
/// sorts once across all roots). `recursive` descends every subfolder; otherwise only
/// the immediate children are listed.
///
/// This is deliberately **crash-proof on hostile trees**, the bug that made opening a
/// large nested folder (e.g. macOS's `~/Library`) beachball then die:
/// * **iterative** (walkdir, not recursion) — a tree thousands of levels deep can't
///   overflow the stack the old recursive walk did, and open directory handles stay
///   bounded instead of one-per-level;
/// * **never follows symlinks** (walkdir's default) — a directory symlink/alias that
///   points back at an ancestor can't send it into an infinite loop;
/// * **error-tolerant** — a permission-denied folder (macOS TCC guards much of
///   `~/Library`) or a file that vanished mid-walk is skipped, not fatal.
///
/// `cancel`, if set, stops the walk at the next entry so a superseding open can abandon
/// a huge in-flight scan; whatever was gathered so far is left in `out`.
fn collect_images(
    dir: &Path,
    recursive: bool,
    progress: Option<&ScanProgress>,
    out: &mut Vec<PathBuf>,
) {
    let max_depth = if recursive { usize::MAX } else { 1 };
    // follow_links(false) is the default, but state it: symlinked dirs are yielded yet
    // never descended, so the walk stays inside the intended tree and can't cycle.
    let walker = walkdir::WalkDir::new(dir)
        .max_depth(max_depth)
        .follow_links(false);
    for entry in walker {
        if progress.is_some_and(|p| p.is_cancelled()) {
            return;
        }
        // Skip unreadable entries (permissions, races) rather than aborting the scan.
        let Ok(entry) = entry else {
            continue;
        };
        // file_type() here does not traverse symlinks (matches follow_links(false)), so
        // a symlinked file/dir is not mistaken for a real one.
        let ft = entry.file_type();
        if ft.is_dir() {
            // Publish the directory now being walked so the Scanning dialog shows real
            // motion. Cheap: once per directory (a mutex write), not per file.
            if let Some(p) = progress {
                p.set_current(rel_display(entry.path(), dir));
            }
        } else if ft.is_file() && is_supported_image(entry.path()) {
            if let Some(p) = progress {
                p.incr_found();
            }
            out.push(entry.into_path());
        }
    }
}

/// A scanned directory's path relative to the scan root, as a display string for the
/// Scanning dialog's "current folder" caption. The root itself (empty relative path)
/// shows as its own folder name so the caption is never blank.
fn rel_display(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string()),
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
    collect_images(dir, recursive, None, &mut paths);
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
    const PNG: &[u8] = include_bytes!("../icons/photoblaze.png");
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

/// Whether a path names an archive we open as a playlist (`.zip` or `.7z`).
fn is_archive(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("zip") || e.eq_ignore_ascii_case("7z"))
        .unwrap_or(false)
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

/// Execute an [`open::OpenPlan`]'s [`Source`]: scan the roots (or filter the
/// explicit list) into the ordered image paths to play. Returns the paths, the
/// root for relative-path display, the scan root (for `Ctrl+R`; `None` for an
/// explicit list), and whether the scan was recursive.
fn resolve_source(
    source: &Source,
    progress: Option<&ScanProgress>,
) -> (Vec<PathBuf>, PathBuf, Option<PathBuf>, bool) {
    match source {
        Source::Scan { roots, recursive } => {
            let mut paths = Vec::new();
            for r in roots {
                collect_images(r, *recursive, progress, &mut paths);
            }
            paths.sort();
            let root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
            (paths, root, roots.first().cloned(), *recursive)
        }
        Source::Explicit(files) => {
            let paths: Vec<PathBuf> = files
                .iter()
                .filter(|p| is_supported_image(p.as_path()))
                .cloned()
                .collect();
            let root = files
                .first()
                .and_then(|p| p.parent())
                .filter(|d| !d.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            (paths, root, None, false)
        }
        // Archives don't resolve to a path list; `resolve_playlist` routes them to
        // `open_archive` instead. This arm only keeps the match exhaustive.
        Source::Archive(_) => (Vec::new(), PathBuf::from("."), None, false),
    }
}

/// A resolved playlist: the concrete [`PhotoSource`] plus the framing the app
/// needs (display root, the scan root for `Ctrl+R`, recursive flag, start index).
struct Resolved {
    source: Arc<dyn PhotoSource>,
    root: PathBuf,
    scan_root: Option<PathBuf>,
    recursive: bool,
    start: usize,
}

impl Resolved {
    /// The "nothing to show" fallback — an empty filesystem source. Callers treat
    /// `source.is_empty()` uniformly (empty folder, or an archive that failed/needs
    /// a password), so an open failure never blanks a currently-shown photo.
    fn empty() -> Self {
        Resolved {
            source: Arc::new(FsSource::new(Vec::new())),
            root: PathBuf::from("."),
            scan_root: None,
            recursive: false,
            start: 0,
        }
    }
}

/// Resolve a filesystem [`Source`] (folder scan or explicit list) into a playlist,
/// driving `progress` (image count + current folder) and honoring its cancel flag so a
/// superseding open / the Scanning dialog can abandon a huge in-flight scan. The cursor
/// math + `FsSource` build is shared with [`resolve_playlist`]; this carries the
/// (cancellable) directory I/O, so it's what the off-thread scan worker runs.
fn resolve_scan(
    source: &Source,
    cursor: &open::Cursor,
    progress: Option<&ScanProgress>,
) -> Resolved {
    let (paths, root, scan_root, recursive) = resolve_source(source, progress);
    let start = open::resolve_cursor(&paths, cursor);
    Resolved {
        source: Arc::new(FsSource::new(paths)),
        root,
        scan_root,
        recursive,
        start,
    }
}

/// The configured directory walker shared by the streaming scan and its tests: depth-first,
/// each directory's entries **sorted by file name** — which reproduces `Vec<PathBuf>::sort()`
/// order exactly (`Path`'s `Ord` is component-wise, not byte-string — verified), so streaming
/// changes nothing about the order today's walk-then-`paths.sort()` produces. Symlinks are
/// yielded but never followed, so the walk can't cycle. `recursive` sets the depth.
fn image_walker(root: &Path, recursive: bool) -> walkdir::WalkDir {
    walkdir::WalkDir::new(root)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .sort_by_file_name()
        .follow_links(false)
}

/// All supported images under `root` in playlist order — the sorted image sequence the
/// streaming scan emits, collected eagerly. Used by the order-guarantee tests.
#[cfg(test)]
fn sorted_image_walk(root: &Path, recursive: bool) -> Vec<PathBuf> {
    image_walker(root, recursive)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && is_supported_image(e.path()))
        .map(walkdir::DirEntry::into_path)
        .collect()
}

/// Build a playlist snapshot from the paths gathered so far. Runs on the **scan worker
/// thread** so constructing the `FsSource` (which rebuilds the display-name list, O(N)) never
/// touches the event loop — the UI just swaps the resulting `Arc`. `start` is resolved
/// against this snapshot; it's only used by the bootstrap (first) batch (later batches keep
/// the app's own cursor via [`extend_playlist`](App::extend_playlist)).
fn build_resolved(
    paths: Vec<PathBuf>,
    cursor: &open::Cursor,
    root: PathBuf,
    scan_root: Option<PathBuf>,
    recursive: bool,
) -> Resolved {
    let start = open::resolve_cursor(&paths, cursor);
    Resolved {
        source: Arc::new(FsSource::new(paths)),
        root,
        scan_root,
        recursive,
        start,
    }
}

/// Walk `roots` off the event loop, **streaming** the playlist in: emit a growing snapshot
/// every [`SCAN_BATCH_INTERVAL`] (and a final one), then [`ScanUpdate::Done`]. The first
/// non-empty batch lets the app show a photo almost immediately; later batches extend the
/// playlist in place, so the user browses while the rest of a big tree is still being walked.
/// Drives `progress` (image count + current folder) and bails at the next entry once its
/// cancel flag is set. Each snapshot is built here (off-thread) so the UI swap is O(1).
/// Sending stops early if the receiver is gone (a superseding open dropped it).
#[allow(clippy::too_many_arguments)]
fn stream_scan(
    roots: Vec<PathBuf>,
    recursive: bool,
    cursor: open::Cursor,
    root: PathBuf,
    scan_root: Option<PathBuf>,
    generation: u64,
    progress: ScanProgress,
    tx: std::sync::mpsc::Sender<(u64, ScanUpdate)>,
) {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut last_emit = Instant::now();
    let mut sent_len = 0usize;
    // For a single-file open (`Cursor::At`) we must not bootstrap until the opened file is
    // in the snapshot — otherwise `resolve_cursor` falls back to index 0 and we'd show the
    // wrong photo. So hold interval emits until the target is found (it always will be: it's
    // in the flat parent dir being scanned). `Cursor::First` never gates. The *final* emit
    // below is unconditional, so a target that's since been deleted still shows the folder.
    let target = match &cursor {
        open::Cursor::At(p) => Some(p.clone()),
        open::Cursor::First => None,
    };
    let mut gated = target.is_some();
    'outer: for r in &roots {
        for entry in image_walker(r, recursive) {
            if progress.is_cancelled() {
                break 'outer;
            }
            let Ok(entry) = entry else {
                continue; // skip unreadable entries (permissions, races) — don't abort
            };
            let ft = entry.file_type();
            if ft.is_dir() {
                // Publish the directory now being walked (relative to its root) for the chip.
                progress.set_current(rel_display(entry.path(), r));
            } else if ft.is_file() && is_supported_image(entry.path()) {
                let p = entry.into_path();
                progress.incr_found();
                if gated && target.as_ref() == Some(&p) {
                    gated = false; // the opened file is now in the snapshot — emits may start
                }
                paths.push(p);
                if !gated && last_emit.elapsed() >= SCAN_BATCH_INTERVAL {
                    let snap = build_resolved(
                        paths.clone(),
                        &cursor,
                        root.clone(),
                        scan_root.clone(),
                        recursive,
                    );
                    if tx.send((generation, ScanUpdate::Batch(snap))).is_err() {
                        return; // receiver dropped — superseded; stop and free our buffers
                    }
                    sent_len = paths.len();
                    last_emit = Instant::now();
                }
            }
        }
    }
    // Final batch: the un-emitted remainder, or the only batch for a fast folder.
    if !paths.is_empty() && (paths.len() > sent_len || sent_len == 0) {
        let snap = build_resolved(paths, &cursor, root, scan_root, recursive);
        let _ = tx.send((generation, ScanUpdate::Batch(snap)));
    }
    let _ = tx.send((generation, ScanUpdate::Done));
}

/// Turn a planned [`Source`] into a concrete [`PhotoSource`] plus playlist framing.
/// Scans and explicit lists become an [`FsSource`]; an archive opens a
/// [`ZipSource`] (entries read into RAM on demand, never extracted to disk). On a
/// hard archive failure it logs and falls back to an empty source.
fn resolve_playlist(source: &Source, cursor: &open::Cursor) -> Resolved {
    match source {
        Source::Scan { .. } | Source::Explicit(_) => resolve_scan(source, cursor, None),
        // The launch / picker / drop paths open archives via the async-aware
        // `App::begin_archive_open` (which surfaces failures through the egui
        // dialog), so this arm is only a safety net: log and show empty on failure.
        Source::Archive(path) => match open_archive(path, None) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("PhotoBlaze: {}", e.user_message());
                Resolved::empty()
            }
        },
    }
}

/// Open `path` as an archive playlist, dispatching by extension: `.7z` ->
/// [`SevenZSource`] (eager, RAM-budget pre-flight), anything else -> [`ZipSource`]
/// (lazy per-entry). Returns a structured [`ArchiveOpenError`](archive::ArchiveOpenError)
/// so the caller can show the right message; entries are read into RAM, never
/// extracted to disk.
///
/// `password` decrypts an encrypted archive (`None` on the first open; an encrypted
/// archive then returns [`PasswordRequired`](archive::ArchiveOpenError::PasswordRequired)
/// so the app can prompt, and a re-open carries the entered password). A ZIP's
/// directory reads without one, and a *wrong* password still opens — so an actual
/// entry decrypt ([`ZipSource::password_ok`]) is what catches it.
fn open_archive(
    path: &Path,
    password: Option<String>,
) -> Result<Resolved, archive::ArchiveOpenError> {
    let is_7z = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("7z"));
    if is_7z {
        seven_z_preflight(path, password.as_deref())?;
        // Synchronous safety-net path (the interactive paths use the async, cancellable
        // `begin_archive_open`): a throwaway progress handle no UI reads.
        load_seven_z(path, password, &pb_source::OpenProgress::new())
    } else {
        let has_password = password.is_some();
        let zs = ZipSource::open(path, password, is_supported_extension)?;
        // Encrypted but no password supplied -> prompt for one.
        if zs.needs_password() {
            return Err(archive::ArchiveOpenError::PasswordRequired);
        }
        // A password was supplied but it doesn't decrypt -> prompt again (open
        // succeeds regardless of the password; the entry read is the real check).
        if has_password && zs.is_encrypted() && !zs.password_ok() {
            return Err(archive::ArchiveOpenError::PasswordRequired);
        }
        if zs.is_empty() {
            return Err(archive::ArchiveOpenError::Empty);
        }
        Ok(archive_resolved(path, Arc::new(zs)))
    }
}

/// The RAM pre-flight for a 7z: predict-and-refuse before the (uncatchable) eager
/// decompress, so an archive whose resident image bytes won't fit the budget is
/// rejected instantly rather than aborting partway in. `password` is only needed
/// for a header-encrypted archive (else the header reads without one); a wrong /
/// missing one surfaces as `PasswordRequired` here, routing to the prompt.
fn seven_z_preflight(path: &Path, password: Option<&str>) -> Result<(), archive::ArchiveOpenError> {
    seven_z_preflight_within(path, password, archive::ram_budget())
}

/// The pre-flight comparison against an explicit `budget`, split out from
/// [`seven_z_preflight`] so tests can drive the over-budget refusal path
/// *deterministically* with an injected ceiling — rather than racing on the
/// process-global `PB_ARCHIVE_RAM_BUDGET` env var (Rust runs tests in parallel
/// threads, so mutating the environment from one test corrupts the others).
fn seven_z_preflight_within(
    path: &Path,
    password: Option<&str>,
    budget: u64,
) -> Result<(), archive::ArchiveOpenError> {
    let needed = seven_z_projected_bytes(path, password, is_supported_extension)?;
    if needed > budget {
        return Err(archive::ArchiveOpenError::TooLarge { needed, budget });
    }
    Ok(())
}

/// Eager-decompress a 7z into a [`Resolved`] (no pre-flight here — the caller runs
/// [`seven_z_preflight`] first). This is the slow step the runtime path runs on a
/// background thread (see `App::begin_archive_open`). `password` decrypts an
/// encrypted archive; a wrong one fails decode and surfaces as `PasswordRequired`.
fn load_seven_z(
    path: &Path,
    password: Option<String>,
    progress: &pb_source::OpenProgress,
) -> Result<Resolved, archive::ArchiveOpenError> {
    let src =
        SevenZSource::open_with_progress(path, password, is_supported_extension, Some(progress))?;
    if src.is_empty() {
        return Err(archive::ArchiveOpenError::Empty);
    }
    Ok(archive_resolved(path, Arc::new(src)))
}

/// A [`Resolved`] for an archive `source`: the archive path is the display root,
/// and entry names are already archive-relative (so the info panel uses them).
fn archive_resolved(path: &Path, source: Arc<dyn PhotoSource>) -> Resolved {
    Resolved {
        root: path.to_path_buf(),
        source,
        scan_root: None,
        recursive: false,
        start: 0,
    }
}

/// Shared, thread-safe progress + cancellation for an off-thread directory scan — the
/// folder-walk analogue of [`pb_source::OpenProgress`]. A folder walk has no knowable
/// total (you'd have to walk the tree twice), so this carries *indeterminate* progress:
/// a running count of images found and the directory currently being walked, plus the
/// cancel flag the Scanning dialog's Cancel / Esc (and a superseding open / teardown)
/// set. Cheap to [`clone`](Clone) — it's an `Arc` — so the walk worker and the UI thread
/// each hold one.
#[derive(Clone, Default)]
pub(crate) struct ScanProgress {
    inner: Arc<ScanProgressInner>,
}

#[derive(Default)]
struct ScanProgressInner {
    /// Supported images found so far (bumped per match by the walk worker).
    found: AtomicUsize,
    /// Set by the UI to stop the walk at its next entry (Cancel / Esc / a superseding open).
    cancel: AtomicBool,
    /// The directory currently being walked, relative to the scan root (display string).
    current: std::sync::Mutex<String>,
}

impl ScanProgress {
    fn new() -> Self {
        Self::default()
    }

    /// Supported images found so far (read by the Scanning dialog each frame).
    pub(crate) fn found(&self) -> usize {
        self.inner.found.load(Ordering::Relaxed)
    }

    /// Worker-side: record one more supported image.
    fn incr_found(&self) {
        self.inner.found.fetch_add(1, Ordering::Relaxed);
    }

    /// Ask the walk to stop at its next entry (the Cancel button / Esc / a superseding open).
    pub(crate) fn request_cancel(&self) {
        self.inner.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested (polled by the walk loop).
    fn is_cancelled(&self) -> bool {
        self.inner.cancel.load(Ordering::Relaxed)
    }

    /// Worker-side: publish the directory now being walked (relative to the scan root).
    /// A poisoned lock just means a prior writer panicked mid-update — drop the value
    /// rather than propagate; a stale caption is harmless.
    fn set_current(&self, dir: String) {
        if let Ok(mut g) = self.inner.current.lock() {
            *g = dir;
        }
    }

    /// The directory currently being walked (empty until the worker sets one).
    pub(crate) fn current(&self) -> String {
        self.inner
            .current
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

/// A message from the streaming scan worker ([`stream_scan`]). The walk runs off the event
/// loop and **streams** the playlist in: each `Batch` carries a growing [`Resolved`] snapshot
/// (the cumulative `FsSource` so far, built off-thread so the UI swap is O(1)); `Done` ends
/// the walk. The app bootstraps the playlist on the first non-empty batch (showing a photo
/// almost immediately) and extends it in place on the rest — so browsing starts before the
/// whole tree is scanned.
enum ScanUpdate {
    Batch(Resolved),
    Done,
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
    /// Whether the first non-empty batch has been applied (the first image shown). Until
    /// then the Scanning dialog may reveal and there's nothing on screen yet; the first
    /// batch *bootstraps* the playlist (display + decode), later batches *extend* it.
    bootstrapped: bool,
}

/// An in-flight background archive open. A `.7z` is eager-decompressed off the event
/// loop; the [`Resolved`] (or error) rides back over `rx` tagged with `generation`,
/// so a superseded open (a newer one bumped `App::archive_gen`) is discarded.
struct ArchiveLoad {
    generation: u64,
    rx: std::sync::mpsc::Receiver<(u64, Result<Resolved, archive::ArchiveOpenError>)>,
    /// The archive being opened, so a `PasswordRequired` result can re-prompt and
    /// re-open the same path with the entered password.
    path: PathBuf,
    /// Whether this open carried a user-entered password (a repeat `PasswordRequired`
    /// then means it was wrong, so the prompt shows the retry error).
    was_password_attempt: bool,
    /// Shared progress + cancel handle for this open. The loading dialog reads it to
    /// draw its bar; the Cancel button / Esc / a superseding open flips its cancel flag
    /// so the worker stops at the next entry boundary (freeing its partial RAM).
    progress: pb_source::OpenProgress,
}

fn main() {
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
            Ok(()) => println!("PhotoBlaze: wrote HUD gallery \u{2192} {out}"),
            Err(e) => eprintln!("PhotoBlaze: HUD gallery failed: {e}"),
        }
        return;
    }

    let cli_windowed = args.iter().any(|a| a == "--windowed" || a == "-w");
    let cli_fullscreen = args.iter().any(|a| a == "--fullscreen" || a == "-f");
    // Saved preferences drive the launch defaults (window mode + recursive scan); an
    // explicit CLI flag always wins. A fresh install (defaults) starts windowed.
    let startup_settings = settings::Settings::load();
    let windowed = if cli_windowed {
        true
    } else if cli_fullscreen {
        false
    } else {
        !startup_settings.start_fullscreen()
    };
    let force_recursive = args.iter().any(|a| a == "--recursive" || a == "-r");
    let force_flat = args.iter().any(|a| a == "--no-recursive");
    let metrics_on = args.iter().any(|a| a == "--metrics");

    // Every entry point (CLI, double-click via association, drag-drop, picker)
    // funnels through the same pure plan: classify the paths, decide the source +
    // cursor, then scan. A folder opens recursively by default; `--no-recursive`
    // forces flat and `-r` forces recursive on the command line.
    let positional: Vec<PathBuf> = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .collect();
    let mut plan = open::plan(classify_inputs(positional));
    if let Source::Scan { recursive, .. } = &mut plan.source {
        // Default from the saved preference; an explicit CLI flag overrides it.
        *recursive = startup_settings.recursive;
        if force_recursive {
            *recursive = true;
        }
        if force_flat {
            *recursive = false;
        }
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
        resolve_playlist(&plan.source, &plan.cursor)
    };

    match &plan.source {
        Source::Archive(_) => println!("PhotoBlaze: opening archive…"),
        Source::Scan { .. } => println!("PhotoBlaze: scanning folder…"),
        _ => {
            println!("PhotoBlaze: {} image(s)", resolved.source.len());
            if resolved.source.is_empty() {
                eprintln!("(no images - drop a photo or folder on the window, or press O to open)");
            }
        }
    }

    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    // macOS: graft `application:openURLs:` onto winit's app delegate NOW. winit sets the
    // delegate during `EventLoop::new()` above, and the run loop — which dispatches a cold
    // double-click's `openURLs` right after `applicationDidFinishLaunching` — hasn't started
    // yet. This is the only point early enough to catch the *launch* file; installing in
    // `resumed()` is too late for a cold open (the event has already been dispatched).
    #[cfg(target_os = "macos")]
    macos_open::install();

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
    );
    // Hand the deferred open (archive or folder scan) to the app; `resumed` fires it once
    // the window and engine are up. The plan carries the startup recursive override.
    if deferred {
        app.queue_launch(plan);
    }
    event_loop.run_app(&mut app).expect("event loop");

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
        let (paths, root, scan_root, recursive) = resolve_source(&src, None);
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

    /// The streaming walk (`sort_by_file_name`) must yield images in the **exact** order
    /// today's full-walk-then-`paths.sort()` produces — so showing photos before the scan
    /// finishes never reorders the playlist. Pins the boundary cases that motivated the
    /// design discussion: `a/b.jpg` vs `a.jpg` (a subdir vs a same-stem file), a subdir vs a
    /// later-named sibling file, and `img2` vs `img10` (stays byte-lexicographic, not natural).
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

        // Expected = exactly the images, in `Vec<PathBuf>::sort()` order (component-wise).
        let mut expected: Vec<PathBuf> = images.iter().map(|r| dir.join(r)).collect();
        expected.sort();
        assert_eq!(
            got, expected,
            "streaming walk order must equal paths.sort() (and skip the .txt)"
        );
        // Spell out the load-bearing boundary so a regression reads clearly: the subdir's
        // `a/b.jpg` sorts before the file `a.jpg` (component-wise: \"a\" < \"a.jpg\").
        let pos = |rel: &str| got.iter().position(|p| p == &dir.join(rel)).unwrap();
        assert!(pos("a/b.jpg") < pos("a.jpg"), "a/b.jpg before a.jpg");
        assert!(
            pos("a_subdir/x.jpg") < pos("z.jpg"),
            "a_subdir before z.jpg"
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
        collect_images(&dir, true, Some(&progress), &mut out);
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
        collect_images(&dir, true, Some(&progress), &mut out);

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
            rel_display(Path::new("/photos/Library/2024/Iceland"), root),
            Path::new("2024/Iceland").display().to_string()
        );
        // The root itself (empty relative) falls back to the root's own folder name.
        assert_eq!(rel_display(root, root), "Library");
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
        const IMG: &[u8] = include_bytes!("../icons/photoblaze.png");
        for rel in ["a.png", "b.png", "sub/c.png"] {
            fs::write(dir.join(rel), IMG).expect("seed image");
        }

        let before = snapshot_tree(&dir);

        // The actual disk-touching code the app runs while viewing, through the
        // real source seam: recursive scan → FsSource → decode_item (the pool's
        // step) + the Shift+I panel's byte read.
        let paths = scan_images(&dir, true);
        assert_eq!(
            paths.len(),
            3,
            "recursive scan should find all three images"
        );
        let source = FsSource::new(paths);
        let fit = FitBox {
            max_width: 64,
            max_height: 64,
        };
        for i in 0..source.len() {
            decode_item(&source, i, Some(fit), false).expect("decode");
            let bytes = source.bytes(i).expect("read for exif");
            let _ = read_exif_fields(&bytes);
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
        const IMG: &[u8] = include_bytes!("../icons/photoblaze.png");
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
            zw.finish().expect("finish zip");
        }

        let before = snapshot_tree(&dir);

        // The disk-touching code the app runs while viewing a zip.
        let resolved = resolve_playlist(&Source::Archive(zip_path.clone()), &open::Cursor::First);
        assert_eq!(resolved.source.len(), 3, "zip should yield three images");
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
        const IMG: &[u8] = include_bytes!("../icons/photoblaze.png");
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

        // Eager-open the 7z and view every entry: must not extract to disk.
        let resolved = resolve_playlist(&Source::Archive(z_path.clone()), &open::Cursor::First);
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
        const IMG: &[u8] = include_bytes!("../icons/photoblaze.png");
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
            seven_z_projected_bytes(&z_path, None, is_supported_extension).expect("project");
        assert!(
            needed >= IMG.len() as u64,
            "projection ({needed}) covers the image ({})",
            IMG.len()
        );

        // A 1-byte budget is below the projection -> instant, structured refusal
        // (not a load attempt, not an abort).
        match seven_z_preflight_within(&z_path, None, 1) {
            Err(archive::ArchiveOpenError::TooLarge { needed: n, budget }) => {
                assert_eq!(budget, 1);
                assert_eq!(n, needed);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }

        // The same archive fits under a generous budget: the pre-flight is the only gate.
        seven_z_preflight_within(&z_path, None, u64::MAX).expect("fits under a huge budget");

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
            InfoMode::Off,
            false,
            false,
            false,
            false, // mute_live_audio
            false,
            false,
            None,
            false,
        )
    }

    #[test]
    fn menu_state_maps_every_scale_mode() {
        let scale = |m| {
            AppCore::menu_state_from(
                m,
                InfoMode::Off,
                false,
                false,
                false,
                false, // mute_live_audio
                false,
                false,
                None,
                false,
            )
            .scale
        };
        assert_eq!(scale(ScaleMode::Fit), contract::ScaleMode::Fit);
        assert_eq!(scale(ScaleMode::Fill), contract::ScaleMode::Fill);
        assert_eq!(scale(ScaleMode::Original), contract::ScaleMode::Original);
    }

    #[test]
    fn menu_state_collapses_info_mode_to_the_two_checkmarks() {
        let info = |i| {
            AppCore::menu_state_from(
                ScaleMode::Fit,
                i,
                false,
                false,
                false,
                false, // mute_live_audio
                false,
                false,
                None,
                false,
            )
            .info
        };
        assert_eq!(info(InfoMode::Basic), contract::InfoOverlay::Basic);
        assert_eq!(info(InfoMode::Full), contract::InfoOverlay::FullExif);
        // The menu has no glyph for Help or Off — both leave *neither* box checked, the
        // exact behavior of the old `info == Basic` / `info == Full` checkmark tests.
        assert_eq!(info(InfoMode::Help), contract::InfoOverlay::Hidden);
        assert_eq!(info(InfoMode::Off), contract::InfoOverlay::Hidden);
    }

    #[test]
    fn menu_state_carries_undo_label_and_enabled_together() {
        // `None` on the undo stack → disabled "Undo"; a label → enabled with that title.
        assert_eq!(base_menu_state().undo, None);
        let with_undo = AppCore::menu_state_from(
            ScaleMode::Fit,
            InfoMode::Off,
            false,
            false,
            false,
            false, // mute_live_audio
            false,
            false,
            Some("Undo Save Rotation"),
            false,
        );
        assert_eq!(with_undo.undo, Some("Undo Save Rotation"));
    }

    #[test]
    fn menu_state_passes_through_every_bool_flag() {
        // Each toggle/enabled input lands on its own field (no crossed wires).
        let all_on = AppCore::menu_state_from(
            ScaleMode::Fit,
            InfoMode::Off,
            true, // recursive
            true, // fullscreen
            true, // slideshow
            true, // mute_live_audio
            true, // save_rotation_enabled
            true, // cancel_scan_enabled
            None,
            true, // native_fullscreen_engaged
        );
        assert!(all_on.recursive);
        assert!(all_on.fullscreen);
        assert!(all_on.slideshow);
        assert!(all_on.mute_live_audio);
        assert!(all_on.save_rotation_enabled);
        assert!(all_on.cancel_scan_enabled);
        assert!(all_on.native_fullscreen_engaged);

        // The baseline leaves them all off.
        let b = base_menu_state();
        assert!(
            !b.recursive
                && !b.fullscreen
                && !b.slideshow
                && !b.mute_live_audio
                && !b.save_rotation_enabled
                && !b.cancel_scan_enabled
                && !b.native_fullscreen_engaged
        );
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
            InfoMode::Off,
            false,
            false,
            true, // slideshow flipped
            false,
            false,
            false,
            None,
            false,
        );
        assert_ne!(a, changed);
    }
}
