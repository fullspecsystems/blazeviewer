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
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorIcon, Icon, Window, WindowId};

use pb_core::open::{self, LaunchInput, Source};
use pb_core::{full_ring, prefetch_targets, prefetch_targets_scanning, Playlist, ResidentRing};
use pb_decode::{
    decode_bytes, decode_named_bytes, is_supported_extension, read_exif_fields, DecodeError,
    DecodedImage, FitBox, PixelFormat,
};
use pb_render::{
    test_pattern, Renderer, Rotation, ScaleMode, ViewTransform, WgpuRenderer, MAX_ZOOM, MIN_ZOOM,
};
use pb_source::{seven_z_projected_bytes, FsSource, PhotoSource, SevenZSource, ZipSource};

mod action;
mod animation;
mod archive;
mod clipboard;
#[cfg(windows)]
mod darkmode;
mod decode_pool;
mod delete;
mod dialog;
#[cfg(target_os = "macos")]
mod hdr_surface;
mod hud;
mod hud_gallery;
mod icon;
mod keymap;
#[cfg(target_os = "macos")]
mod macos_chrome;
#[cfg(target_os = "macos")]
mod macos_open;
mod menu;
mod metrics;
#[cfg(target_os = "macos")]
mod proxy_icon;
mod save_rotation;
mod settings;
mod slideshow;
use action::{Action, ActionKind};
use animation::Playback;
use decode_pool::{recommended_workers, DecodeFn, DecodePool, Outcome};
use hud::{Hud, Row};
use keymap::{KeyChord, Keymap};
use menu::MenuAction;
use metrics::StageTimes;

/// VRAM budget for the resident texture ring (~1.5 GB → ~16–32 fit-size slots on
/// a 7680-wide display, far more on smaller ones). Capacity is clamped to [4, 64].
const RING_BUDGET_BYTES: u64 = 1_500_000_000;
/// Cap on decoded-but-not-yet-uploaded bytes held by the pool (backpressure).
const POOL_BUDGET_BYTES: usize = 512 * 1024 * 1024;
/// Max slot uploads performed per `about_to_wait` tick, so a burst of finished
/// decodes can't blow the frame budget.
const UPLOADS_PER_TICK: usize = 2;
/// Per-decode wall time *as the pool sees it* (i.e. under real concurrent load),
/// printed with the `--metrics` report. Isolated decode is fast; this shows how much
/// 8-way contention inflates it (it's how the RAW-demosaic-on-preview stall was
/// found). Only recorded under `--metrics` (the flag below), so it's zero-overhead
/// and unbounded-growth-free in normal runs.
static POOL_DECODE_MS: std::sync::Mutex<Vec<(f64, String)>> = std::sync::Mutex::new(Vec::new());
/// Whether `--metrics` is on (gates the `POOL_DECODE_MS` recording in the off-thread
/// decode closure, which has no access to the `StageTimes`).
static METRICS_ON_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Cap on the **full-resolution** "sharp ring" upgraded around the cursor when
/// parked (`upgrade_set`). The preview window can be much larger (up to the ring's
/// 64-slot capacity on small windows), but holding more than this many *fulls*
/// resident is wasted decode — nobody pause-and-steps two dozen photos before
/// either flying (previews carry that) or stopping. Keeps the on-park decode burst
/// bounded. On a 7680 fullscreen the byte-budgeted capacity (~12–32) binds first.
const MAX_FULL_RING: usize = 24;

/// Hold-to-zoom curve: the e-folding zoom rate (per second) ramps from a gentle
/// start (fine tuning) to a fast max over `ZOOM_RAMP_SECS`. Time-based so it's
/// frame-rate independent.
const ZOOM_MIN_RATE: f32 = 0.5;
const ZOOM_MAX_RATE: f32 = 2.5;
const ZOOM_RAMP_SECS: f32 = 0.7;

/// Hold-to-pan curve: pan speed (px/sec) ramps from a gentle start to a fast max
/// over `PAN_RAMP_SECS`. Time-based, same shape as zoom (per the owner's note).
const PAN_MIN_SPEED: f32 = 450.0;
const PAN_MAX_SPEED: f32 = 3200.0;
const PAN_RAMP_SECS: f32 = 0.7;

/// Trackpad gesture tuning. `PINCH_GAIN` scales macOS's incremental magnification
/// (`WindowEvent::PinchGesture` delta) into a zoom factor (`1 + delta·gain`).
/// `WHEEL_ZOOM_STEP` is the per-line zoom factor for **Ctrl+scroll** (the explicit
/// zoom gesture; plain scroll pans instead — see the `MouseWheel` handler).
const PINCH_GAIN: f32 = 1.0;
const WHEEL_ZOOM_STEP: f32 = 0.1;
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

/// Repeat interval for the held frame-step scrub (`,`/`.`), after the initial tap
/// delay (`initial_delay`). ~14 fps — quick enough to scrub, slow enough to read (#37).
const FRAME_STEP_REPEAT: Duration = Duration::from_millis(70);

/// The frame-step direction encoded by an action: `+1` next / `-1` previous / `0`
/// for anything else.
fn frame_step_dir(action: Action) -> i32 {
    match action {
        Action::FrameNext => 1,
        Action::FramePrev => -1,
        _ => 0,
    }
}

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

/// Fixed width of the scan status card, in logical px (scaled by the display factor, then
/// clamped to the window). Wide enough for a reasonable current-folder path; longer paths are
/// truncated. Fixed so the card doesn't jitter as the count / live path change width.
const SCAN_CARD_WIDTH: f32 = 320.0;

/// How often the scan card is re-rasterized at most. The live current-folder line changes per
/// directory (fast); throttling the rebuild keeps the software composite off the hot path
/// while the displayed path/count lag by at most this. Show/hide is immediate.
const SCAN_CARD_REFRESH: Duration = Duration::from_millis(120);

/// The minimum gap since the last shown photo before the next held-key auto-advance,
/// given how long auto-repeat has been running (`elapsed`, measured from when the
/// initial tap delay expired). The advance rate ramps linearly from `min_rate`
/// (photos/sec) up to the **ceiling** over `ramp_secs`, then holds there. The ceiling
/// is the configured `max_rate` cap (#20) clamped to the display refresh (`max_rate`
/// ≤ 0, or ≥ refresh, means "uncapped" → refresh is the hard limit). The returned
/// interval is the rate's reciprocal, floored at the ceiling's interval so it's never
/// faster. Pure + time-based (frame-rate independent), so it's unit-testable without
/// the event loop.
fn advance_interval(
    elapsed: Duration,
    min_rate: f32,
    ramp_secs: f32,
    max_rate: f32,
    frame_interval: Duration,
) -> Duration {
    let frame_secs = frame_interval.as_secs_f32();
    let refresh_rate = 1.0 / frame_secs.max(f32::MIN_POSITIVE);
    // Effective ceiling: the configured cap, never above refresh; 0/negative or a cap
    // at/above refresh means uncapped (refresh is the hard limit).
    let ceiling = if max_rate > 0.0 {
        max_rate.min(refresh_rate)
    } else {
        refresh_rate
    };
    // Interval at the ceiling: exactly the refresh frame when uncapped (no float
    // drift), else the cap's reciprocal (a deliberately slower scan limit).
    let ceil_interval = if ceiling >= refresh_rate {
        frame_interval
    } else {
        Duration::from_secs_f32(1.0 / ceiling)
    };
    let min_rate = min_rate.min(ceiling);
    // No headroom to ramp (floor already at/above the ceiling): run at the ceiling.
    if min_rate >= ceiling {
        return ceil_interval;
    }
    let t = if ramp_secs > 0.0 {
        (elapsed.as_secs_f32() / ramp_secs).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let rate = min_rate + (ceiling - min_rate) * t;
    if rate >= ceiling {
        return ceil_interval;
    }
    let min_secs = ceil_interval.as_secs_f32(); // fastest (smallest) interval allowed
    let secs = (1.0 / rate.max(f32::MIN_POSITIVE)).clamp(min_secs, 60.0);
    Duration::from_secs_f32(secs)
}

/// "Not-ready" loading-pie tuning (the top-right affordance shown while the next
/// photo is still decoding). The fill is a deliberate "honest-ish" fake: there is
/// no true decode progress, so it eases asymptotically toward — but never reaches
/// — full, on a time constant self-calibrated to how long misses usually take.
/// Only appears once a wait outlasts `PIE_SHOW_DELAY`, so fast hits never flash it.
const PIE_SHOW_DELAY: f32 = 0.12; // s a wait must persist before the pie appears
const PIE_TAU_MIN: f32 = 0.06; // s floor on the fill time constant
const PIE_FILL_CAP: f32 = 0.93; // the wedge never quite completes (the "lie")
const PIE_FINISH_FADE: f32 = 0.18; // s to snap-to-full then fade once ready
const PIE_GLOW_DUR: f32 = 0.30; // s the keypress brighten-pulse decays over
const PIE_EWMA_ALPHA: f32 = 0.30; // weight of the latest wait in the time estimate
const PIE_DIAMETER: f32 = 46.0; // logical px (scaled by the display factor)
const PIE_MARGIN: f32 = 24.0; // logical px in from the top-right corner

/// Ring capacity from the per-slot byte size and the VRAM budget. Full-res
/// (Original) slots are several times bigger than fit slots, so the prefetch
/// window is correspondingly smaller — but still resident and async.
fn ring_capacity(slot_bytes: u64) -> usize {
    ((RING_BUDGET_BYTES / slot_bytes.max(1)) as usize).clamp(4, 64)
}

/// Split the ring into an ahead/behind prefetch window (the current item, always
/// resident, takes the remaining slot). Biased forward; a few behind so reversing
/// stays cheap.
fn window_for_capacity(cap: usize) -> (usize, usize) {
    let usable = cap.saturating_sub(1);
    let ahead = (usable * 4 / 5).max(1);
    let behind = usable.saturating_sub(ahead);
    (ahead, behind)
}

/// Translate the decoder's color transform into the renderer's (identical fields,
/// distinct crate types so neither crate depends on the other).
fn render_color(c: &pb_decode::ColorTransform) -> pb_render::ColorTransform {
    pb_render::ColorTransform {
        matrix: c.matrix,
        trc: c.trc,
        enabled: c.enabled,
    }
}

/// Whether a decoded image is HDR (scene-linear fp16 → the renderer's HDR path).
fn is_hdr(img: &DecodedImage) -> bool {
    img.format == PixelFormat::Rgba16F
}

/// One photo's info, for the corner overlay panel.
#[derive(Clone)]
struct PhotoMeta {
    rel: String,
    w: u32,
    h: u32,
    codec: &'static str,
    /// If this photo is an animated container (GIF/APNG/WebP, or an AVIF/HEIC
    /// sequence on macOS), which kind — so the viewer can offer on-demand playback
    /// (the ▶ P hint, task #37). `None` for a still. Sniffed during decode.
    animated: Option<pb_decode::AnimationKind>,
}

/// Which overlay is showing: nothing, the one-line basic panel (`i`), the
/// full-EXIF "nerd" table (`Shift+I`), or the keybindings help (`/` or `?`). All
/// share the single overlay quad, so they're mutually exclusive. (About is a
/// native dialog now, not an overlay — see `about_dialog`.)
#[derive(Clone, Copy, PartialEq, Eq)]
enum InfoMode {
    Off,
    Basic,
    Full,
    Help,
}

/// A reversible user edit, recorded on the undo stack (Edit ▸ Undo / `Ctrl+Z`). RAM-only,
/// dropped on exit — no on-disk undo journal (privacy #2). The stack is cleared whenever
/// the playlist/source changes (open, delete-rebuild, empty state — see
/// [`App::rebuild_playlist`]/[`App::enter_empty_state`]), so a recorded `item` index
/// always indexes the current source. Only a saved rotation is reversible today; deletes
/// (recoverable / permanent) are a future / never extension of the same stack.
enum UndoAction {
    /// Undo a saved EXIF rotation: restore `path`'s Orientation tag to `prev` — the value
    /// it held *before* the save. `item` is the playlist index it was saved at, used to
    /// refresh that photo's cached decode after the restore.
    SaveRotation {
        item: usize,
        path: PathBuf,
        prev: u8,
    },
}

impl UndoAction {
    /// The dynamic Edit-menu title for this action (e.g. "Undo Save Rotation"), so the
    /// menu shows *what* the next undo will reverse (see `App::refresh_undo_menu_item`).
    fn menu_label(&self) -> &'static str {
        match self {
            UndoAction::SaveRotation { .. } => "Undo Save Rotation",
        }
    }
}

/// A navigation step from a held key: sequential forward (`space`), sequential
/// backward (`backspace`), a precomputed-random jump (`enter`), or a step back
/// through the random walk (`shift+enter`, to revisit one you flew past). All are
/// gated + self-paced + prefetchable the same way (random walks a known shuffle
/// order, so its next/prior targets are knowable — see `pb_core::ShuffleOrder`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Nav {
    Forward,
    Backward,
    Random,
    RandomPrev,
}

/// The navigation direction for a nav [`Action`], or `None` for any non-nav action.
/// Bridges the central keymap vocabulary to the engine's `Nav` (used by the press
/// handler and `held_nav`).
fn nav_of(action: Action) -> Option<Nav> {
    match action {
        Action::Next => Some(Nav::Forward),
        Action::Prev => Some(Nav::Backward),
        Action::Random => Some(Nav::Random),
        Action::RandomPrev => Some(Nav::RandomPrev),
        _ => None,
    }
}

/// Build a photo's info panel data from its path + decoded image.
/// A path shown relative to the scan root (forward-slashed), or its file name if
/// it isn't under the root.
fn rel_to_root(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string(),
    }
}

/// The info-panel metadata for `item`: its display path (root-relative for a real
/// file, the archive-relative entry name otherwise) plus the decoded dimensions
/// and codec.
fn meta_for(source: &dyn PhotoSource, item: usize, root: &Path, img: &DecodedImage) -> PhotoMeta {
    let rel = match source.path(item) {
        Some(p) => rel_to_root(p, root),
        None => source.name(item).to_string(),
    };
    PhotoMeta {
        rel,
        w: img.orig_width,
        h: img.orig_height,
        codec: img.codec,
        animated: img.animated,
    }
}

/// Resolve item `item`'s encoded bytes from `source` and decode them to fit. The
/// single decode entry point shared by the off-thread pool and the synchronous
/// (first-frame / resize / copy) paths, so a filesystem photo and a ZIP entry
/// decode through exactly the same routing. All reads are RAM-only.
fn decode_item(
    source: &dyn PhotoSource,
    item: usize,
    fit: Option<FitBox>,
    allow_preview: bool,
) -> Result<DecodedImage, DecodeError> {
    let bytes = source
        .bytes(item)
        .map_err(|e| DecodeError::Corrupt(format!("read error: {e}")))?;
    let mut img = decode_named_bytes(source.name(item), &bytes, fit, allow_preview)?;
    // Cheap header sniff so the viewer knows an on-demand animation is available
    // (the ▶ P hint / `P` to play). Off the keypress path — this runs in the decode
    // worker (or the sync first-paint), never on the event loop. The pixels stay the
    // still first frame; only `decode_animation` (on `P`) decodes the whole sequence.
    img.animated = pb_decode::detect_animation(&bytes);
    Ok(img)
}

/// Max displayed characters for an EXIF value; longer ones are truncated so a
/// single field can't blow out the panel width.
const EXIF_VALUE_MAX: usize = 72;

/// Whether an EXIF `(tag, value)` is a binary blob better left out of the panel —
/// Apple's MakerNote/Padding render as kilobytes of hex, and any value that long
/// is binary noise, not human-readable metadata.
fn is_exif_blob(tag: &str, value: &str) -> bool {
    matches!(tag, "MakerNote" | "Padding") || value.len() > 256
}

/// Truncate an over-long EXIF value to `EXIF_VALUE_MAX` characters with an
/// ellipsis (counted in chars, so multibyte values aren't split mid-codepoint).
fn truncate_exif_value(value: &str) -> String {
    if value.chars().count() <= EXIF_VALUE_MAX {
        value.to_string()
    } else {
        let mut s: String = value.chars().take(EXIF_VALUE_MAX).collect();
        s.push('…');
        s
    }
}

struct Active {
    window: Arc<Window>,
    renderer: WgpuRenderer,
}

/// A transient bottom-center status toast (e.g. "Recursive folders: on"): a pill
/// rasterized once, held briefly at full opacity, then faded out by re-uploading
/// the bitmap with scaled alpha. Used for command feedback that has no other
/// on-screen cue (tasks.json #10) — deliberately NOT shown for next/prev/zoom.
struct Toast {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    started: Instant,
    /// Alpha last pushed to the renderer, so the fade re-uploads only on change.
    uploaded_alpha: f32,
}

impl Toast {
    /// Full-opacity hold, then a short linear fade (~1.3 s total).
    const HOLD: Duration = Duration::from_millis(950);
    const FADE: Duration = Duration::from_millis(380);

    /// The toast's alpha at `now`, or `None` once it has fully expired.
    fn alpha(&self, now: Instant) -> Option<f32> {
        let e = now.saturating_duration_since(self.started);
        if e <= Self::HOLD {
            Some(1.0)
        } else {
            let f = (e - Self::HOLD).as_secs_f32() / Self::FADE.as_secs_f32();
            (f < 1.0).then_some(1.0 - f)
        }
    }
}

/// A copy of `rgba` with its alpha channel scaled by `factor` (clamped 0..=1).
fn scale_alpha(rgba: &[u8], factor: f32) -> Vec<u8> {
    let f = factor.clamp(0.0, 1.0);
    let mut out = rgba.to_vec();
    for px in out.chunks_exact_mut(4) {
        px[3] = (px[3] as f32 * f).round().clamp(0.0, 255.0) as u8;
    }
    out
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
    /// Where the playlist's images come from — a filesystem listing or an archive.
    /// `pb-core` navigates by index alone; this resolves an index to bytes + name.
    source: Arc<dyn PhotoSource>,
    playlist: Playlist,
    active: Option<Active>,
    /// Physical keys currently held → the [`Action`] each resolved to at press time
    /// (OS auto-repeat ignored). Drives hold-to-fly nav and continuous pan/zoom; the
    /// action is captured on key-down so it stays stable while held, and is keyed by
    /// `KeyCode` so the key-up (which carries no modifiers) can remove it.
    held: HashMap<KeyCode, Action>,
    /// When a frame was last actually presented. Caps the advance rate from the
    /// presentation (not the advance attempt), so a late-arriving miss isn't
    /// replaced in the same tick it finally shows; also delays the idle panel.
    last_present: Option<Instant>,
    /// Minimum time between advances while holding (≈ one display refresh).
    frame_interval: Duration,
    /// When the current hold's first press happened (for the initial-delay gate).
    hold_start: Option<Instant>,
    /// Delay after the first press before auto-repeat begins (tap = one photo).
    initial_delay: Duration,
    /// Decode-to-fit target = the display size; photos are downscaled to it.
    fit: Option<FitBox>,
    /// Per-photo view transform (scaling mode + rotation + zoom + pan).
    view: ViewTransform,
    /// Last cursor position in physical pixels — the anchor for pinch/wheel zoom
    /// and the reference point for drag-to-pan. `None` until the pointer first
    /// moves over the window (then we zoom about the screen center).
    last_cursor: Option<[f32; 2]>,
    /// Whether the left mouse button is held — drives drag-to-pan (cross-platform,
    /// the primary pan gesture on Windows where trackpad pinch/swipe aren't emitted).
    dragging: bool,
    /// Scan root, for showing paths relative to it.
    root: PathBuf,
    /// Text renderer for the info panel (None if no system font was found).
    hud: Option<Hud>,
    /// Which info overlay is active (`i` basic / `Shift+I` full EXIF / off).
    info: InfoMode,
    /// Whether the panel is currently drawn.
    overlay_shown: bool,
    /// Which item the drawn panel was built for; when it differs from
    /// `displayed_item` the panel is stale and gets rebuilt (so it tracks the
    /// photo with no blank flash on single-step navigation).
    overlay_item: Option<usize>,
    /// The current photo's info, for the panel.
    current: Option<PhotoMeta>,
    /// Display scale factor, for sizing the panel.
    scale_factor: f32,
    /// Per-stage timing (decode/upload/render); disabled unless `--metrics` is passed.
    metrics: StageTimes,

    // --- Phase 3 prefetch engine ---
    /// Off-thread priority decode pool (decode + I/O never block the event loop).
    pool: DecodePool,
    /// Completed decodes, drained + uploaded during `about_to_wait`.
    results: Receiver<Outcome>,
    /// Pure item↔slot residency mirror for the renderer's texture ring.
    ring: ResidentRing,
    /// Geometry generation; bumped on resize / fit toggle. Stale-epoch decodes are
    /// discarded so an old-size result can't land on screen.
    epoch: u64,
    /// What's currently on screen.
    displayed_item: Option<usize>,
    /// The item we're trying to show (== displayed once caught up).
    target_item: Option<usize>,
    /// Slideshow timer state (task #23): on/off + the per-slide interval. RAM-only,
    /// dropped on exit (privacy #2).
    slideshow: slideshow::Slideshow,
    /// The last navigation direction, so the slideshow auto-advances the way the user
    /// last moved (space → forward, backspace → back, enter → random). Updated on
    /// every `advance`, so manual nav during a slideshow steers it.
    last_nav: Nav,
    /// The current prefetch want-list (priority order), used as eviction `keep`.
    targets: Vec<usize>,
    /// Per-item info panel data, cached when decoded (RAM-only; privacy task #2).
    meta_cache: HashMap<usize, PhotoMeta>,
    /// Prefetch window: items ahead / behind the cursor.
    ahead: usize,
    behind: usize,
    /// Items whose decode failed (corrupt/unreadable): skipped, never retried, so
    /// a bad JPEG can't stall hold-to-fly or spin the event loop forever.
    failed: HashSet<usize>,
    /// Paths the user deleted **while a folder scan was still streaming in** — tombstones,
    /// so a later batch (built from the worker's cumulative list, which still contains the
    /// deleted path) can't reintroduce it. Filtered out of each incoming snapshot; RAM-only,
    /// reset at the start of every scan. Empty in the common case (no delete mid-scan), so
    /// it costs nothing then.
    deleted: HashSet<PathBuf>,
    /// Decoded images that arrived faster than the per-tick upload budget; carried
    /// (in priority order) to the next tick so no decode work is wasted. They hold
    /// their pool byte-budget reservation, which is the intended backpressure.
    pending_uploads: Vec<Outcome>,
    /// Per-image rotation overrides (`r` / `Shift+R`); RAM-only, dropped on exit
    /// (privacy task #2). Absent = upright (identity).
    rotations: HashMap<usize, Rotation>,
    /// Whether a Shift key is currently held (for `Shift+R`, `Shift+I`).
    shift: bool,
    /// Whether a Ctrl key is held (for `Ctrl+R` = toggle recursive scan).
    ctrl: bool,
    /// Whether an Alt key is held (for `Alt+Enter` = toggle fullscreen).
    alt: bool,
    /// Whether the "super" key is held — **Cmd (⌘) on macOS**, the Windows key
    /// elsewhere. Lets Mac's OS-standard ⌘-shortcuts (⌘C/⌘S/…) be distinct chords
    /// from the bare keys, so holding Cmd doesn't fire a bare-key action.
    logo: bool,
    /// Whether the current scan-based playlist is recursive (`Ctrl+R` toggles).
    recursive: bool,
    /// The directory the current playlist was scanned from — enables the `Ctrl+R`
    /// recursive toggle and re-scans. `None` for an explicit file list (a
    /// multi-select or dropped photos), where recursion has no folder to walk.
    scan_root: Option<PathBuf>,
    /// Files dropped on the window this burst; winit delivers one event per file,
    /// so they're coalesced here and applied once in `about_to_wait`.
    pending_drops: Vec<PathBuf>,
    /// The transient bottom-center status toast (e.g. recursion on/off), or `None`.
    toast: Option<Toast>,
    /// Briefly set after the file picker closes: ignore Esc-to-quit until this
    /// instant, so the Esc that dismissed the modal picker doesn't also exit the
    /// app (the dialog's own message loop can leak that key to our window).
    esc_guard_until: Option<Instant>,
    /// When a window resize/toggle has "settled" enough to re-decode at the new
    /// fit. A drag fires many Resized events; we GPU-scale the resident texture
    /// instantly per event and defer the expensive decode-to-fit + ring refill
    /// until this instant (debounced), so resizing stays smooth.
    resize_settle_at: Option<Instant>,
    /// Debounced "persist the windowed geometry" deadline (#1). Moving/resizing a
    /// window fires a flurry of events; we update the in-memory geometry per event but
    /// only write `settings.toml` once the user stops, so a drag isn't a write storm.
    geometry_save_at: Option<Instant>,
    /// macOS: the EDR headroom last applied to the renderer (the window's display).
    /// On a window move we re-query the new screen and only reconfigure when it
    /// changes — so dragging across a display with different HDR capability adapts.
    #[cfg(target_os = "macos")]
    last_edr_headroom: f32,
    /// Hold timers for the zoom/pan acceleration ramps (start = when the hold
    /// began; last = previous step, for time-based deltas).
    zoom_started: Option<Instant>,
    zoom_last: Option<Instant>,
    pan_started: Option<Instant>,
    pan_last: Option<Instant>,
    /// "Not-ready" loading-pie state (the top-right affordance). `wait_started` is
    /// when the current miss began (None when caught up); `pie_finish` plays the
    /// snap-to-full fade once the photo lands; `pie_glow_started` is the last
    /// keypress brighten-pulse. `decode_ewma` is the self-calibrating fill time
    /// constant (a rolling mean of how long misses actually take). `pie_drawn`
    /// tracks whether a pie bitmap is up, and `pie_pushed` the last
    /// (progress, glow, alpha) rasterized, so we re-upload only on a visible change.
    wait_started: Option<Instant>,
    pie_finish: Option<Instant>,
    pie_glow_started: Option<Instant>,
    decode_ewma: f32,
    pie_drawn: bool,
    pie_pushed: Option<(f32, f32, f32)>,
    /// The scan status card's content signature `(folder name, current folder, browsable
    /// count)` while it's shown, or `None` when hidden. Cached so the card is only
    /// re-rasterized when its content actually changes (and then no faster than
    /// [`SCAN_CARD_REFRESH`]), off the photo hot path.
    chip_sig: Option<(String, String, usize)>,
    /// When the scan card was last (re)built — throttles the rebuild (the current-folder line
    /// changes fast; see [`SCAN_CARD_REFRESH`]).
    chip_built: Instant,
    /// The **Cancel Scan button's** on-screen rect in physical px `[x0, y0, x1, y1]` while the
    /// card is shown — only the button is clickable (not the whole card). The reusable overlay
    /// hit-region: the first interactive on-image control; future EXIF copy buttons register
    /// rects the same way. `None` when the card is hidden.
    chip_rect: Option<[f32; 4]>,
    /// Whether the pointer is currently over the Cancel Scan button — drives its hover "lit"
    /// state. Flipped only on a hover **enter/leave transition** (see [`App::update_chip_hover`]),
    /// which re-rasterizes the card once; never per cursor-move or per frame.
    chip_hovered: bool,
    /// Items whose resident ring slot holds a fast *preview* (e.g. a HEIC
    /// thumbnail) rather than the full decode. While idle these are upgraded
    /// ("sharpened") to full in priority order. Pruned to resident in `request_prefetch`.
    preview_resident: HashSet<usize>,
    /// Items whose full-resolution decode turned out to be no better than the
    /// preview (e.g. a RAW whose only embedded image *is* its preview) — so we
    /// don't keep re-requesting their upgrade every idle tick.
    upgrade_done: HashSet<usize>,
    /// The last full-upgrade set (the "sharp ring") we issued, so the idle pump
    /// re-issues only when it changes (not every tick → no per-frame churn).
    last_upgrade_set: Vec<usize>,
    /// When each item's full ("sharpen") decode was first requested, to measure the
    /// real end-to-end sharpen latency (full requested → full on screen) via the
    /// `sharpen` metric stage. RAM-only, pruned to resident in `request_prefetch`.
    full_requested_at: HashMap<usize, Instant>,
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
    /// Mac convention — no checkmark). `native_fullscreen_on` caches the last-pushed
    /// state so the per-tick refresh is a no-op when nothing changed.
    #[cfg(target_os = "macos")]
    native_fullscreen_item: Option<muda::MenuItem>,
    #[cfg(target_os = "macos")]
    native_fullscreen_on: bool,
    /// macOS-only: the file currently shown as the title-bar proxy icon (the window's
    /// represented file). Caches the last-pushed value so the per-tick refresh is a
    /// no-op `setRepresentedURL:` call when the displayed photo hasn't changed. `None`
    /// = no proxy (fullscreen, an archive entry, or the empty state). See
    /// [`proxy_icon::set_represented_url`] / [`App::refresh_proxy_icon`].
    #[cfg(target_os = "macos")]
    proxy_icon_path: Option<PathBuf>,
    /// The "Save Rotation" menu item, kept so its enabled state can be toggled at
    /// runtime (only enabled when the current photo has an unsaved rotation on an
    /// EXIF-writable file). `save_enabled` caches the last-pushed state so the
    /// refresh is a no-op Win32 call when nothing changed.
    save_rotation_item: Option<muda::MenuItem>,
    save_enabled: bool,
    /// The File ▸ Stop Scanning menu item, enabled only while a folder scan is streaming in.
    /// `cancel_scan_enabled` caches the last-pushed state so the per-tick refresh is a no-op
    /// when unchanged.
    cancel_scan_item: Option<muda::MenuItem>,
    cancel_scan_enabled: bool,
    /// The undo stack (Edit ▸ Undo / `Ctrl+Z`) — RAM-only, cleared on any source/playlist
    /// change (see [`UndoAction`]). The most recent reversible edit is on top.
    undo_stack: Vec<UndoAction>,
    /// The Edit ▸ Undo menu item, kept so its title + enabled state can mirror the top of
    /// the undo stack at runtime. `undo_menu_state` caches the last-pushed label so the
    /// per-tick refresh is a no-op when unchanged: `None` = never pushed; `Some(None)` =
    /// disabled ("Undo"); `Some(Some(label))` = enabled showing `label`.
    undo_item: Option<muda::MenuItem>,
    undo_menu_state: Option<Option<&'static str>>,
    /// The View-menu checkable items (scale mode / recursive / fullscreen / info), kept
    /// so their checked state can mirror the live app state at runtime. `view_checks_state`
    /// caches the last-pushed `(scale mode, recursive, fullscreen, slideshow, info)` so the
    /// per-tick refresh is a no-op Win32 call when nothing changed.
    view_checks: Option<menu::ViewChecks>,
    view_checks_state: Option<(ScaleMode, bool, bool, bool, InfoMode)>,
    /// A delete whose playlist-advance is deferred: `(fire_at, removed_index)`. The
    /// deleted photo stays on screen with its icon until `fire_at`, then the playlist
    /// drops the item and advances (see `delete_current` / `flush_pending_delete`).
    pending_delete: Option<(Instant, usize)>,
    /// The item awaiting a permanent-delete confirmation: set when the (themed egui)
    /// confirm dialog opens, consumed when it answers Yes (see `dialog_event`).
    pending_confirm_delete: Option<usize>,
    /// Whether the menu has been attached to the current window (`init_for_hwnd`),
    /// so fullscreen↔windowed toggles can show/hide it instead of re-initializing.
    menu_attached: bool,
    /// The open egui dialog window (Settings / About), or `None`. At most one at a
    /// time; its events are routed by window id in `window_event`.
    dialog: Option<dialog::DialogWindow>,
    /// Configurable key bindings (task #8): the chord→action map the keyboard
    /// dispatch and the help overlay both read. Loaded once at startup (defaults +
    /// optional `keymap.toml`); read-only.
    keymap: Keymap,
    /// Persisted user preferences (nav feel, defaults, etc.). Loaded at startup;
    /// the hold-to-fly curve reads `start_speed`/`ramp_secs`/`max_advance_rate` live,
    /// so a future Settings dialog can apply changes by mutating this in place.
    settings: settings::Settings,
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

    // --- Animated images (task #37) — all RAM-only, dropped on navigate (privacy #2) ---
    /// On-demand playback for the currently displayed animated image, once the user
    /// pressed `P` (or stepped a frame). `None` when the current image is a still or
    /// nothing is playing/paused. Presented via `set_image`, never the prefetch ring.
    playback: Option<Playback>,
    /// When the current animation frame was shown — the deadline anchor for advancing
    /// to the next frame.
    anim_frame_shown_at: Option<Instant>,
    /// An off-thread animation decode in flight (a `P`/frame-step kicked it), so a
    /// large sequence never blocks the event loop. `None` when idle.
    anim_decode: Option<AnimDecode>,
    /// Monotonic id for animation-decode requests; a newer one supersedes a stale
    /// result that finally arrives after the user moved on.
    anim_gen: u64,
    /// The displayed item the "▶ P to play" hint was last shown for, so it flashes
    /// once per settle on an animated still (not every tick, and not while flying).
    anim_hint_shown_for: Option<usize>,
    /// Held-key frame-step (`,`/`.` scrub) timing: when the hold began (for the
    /// initial tap delay) and when the last repeat fired.
    framestep_started: Option<Instant>,
    framestep_last: Option<Instant>,
}

/// An in-flight off-thread animation decode (the whole sequence for `item`), kicked
/// by `P` / frame-step so a big GIF/WebP never stalls the event loop. The still first
/// frame stays on screen until it lands.
struct AnimDecode {
    gen: u64,
    item: usize,
    /// The geometry epoch at kick time; a resize in between invalidates the fit, so a
    /// late result for the old size is discarded.
    epoch: u64,
    /// Start playing on arrival (`P`) vs. land paused for frame-stepping (`,`/`.`).
    autoplay: bool,
    rx: std::sync::mpsc::Receiver<Result<pb_decode::Animation, DecodeError>>,
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
            source,
            playlist,
            active: None,
            held: HashMap::new(),
            last_present: None,
            frame_interval: Duration::from_micros(8_333), // ~120 Hz until we read the real rate
            hold_start: None,
            initial_delay: Duration::from_millis(settings.hold_delay_ms as u64),
            fit: None,
            // Start in the user's default scale mode (8/9/0 still switch it live).
            view: ViewTransform {
                mode: scale_mode_of(settings.scale_mode),
                ..ViewTransform::default()
            },
            last_cursor: None,
            dragging: false,
            root,
            hud: Hud::load(),
            info: InfoMode::Off,
            overlay_shown: false,
            overlay_item: None,
            current: None,
            scale_factor: 1.0,
            metrics,
            pool,
            results,
            ring: ResidentRing::new(0),
            epoch: 1,
            displayed_item: None,
            target_item: None,
            // Seed the per-slide dwell from the saved default (#31); `[`/`]` still
            // adjust it live for the session without rewriting the setting.
            slideshow: slideshow::Slideshow {
                interval: Duration::from_secs_f64(settings.slideshow_interval_secs),
                ..slideshow::Slideshow::default()
            },
            last_nav: Nav::Forward,
            targets: Vec::new(),
            meta_cache: HashMap::new(),
            ahead: 8,
            behind: 2,
            failed: HashSet::new(),
            deleted: HashSet::new(),
            pending_uploads: Vec::new(),
            rotations: HashMap::new(),
            shift: false,
            ctrl: false,
            alt: false,
            logo: false,
            recursive,
            scan_root,
            pending_drops: Vec::new(),
            toast: None,
            esc_guard_until: None,
            resize_settle_at: None,
            geometry_save_at: None,
            #[cfg(target_os = "macos")]
            last_edr_headroom: 1.0,
            zoom_started: None,
            zoom_last: None,
            pan_started: None,
            pan_last: None,
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
            preview_resident: HashSet::new(),
            upgrade_done: HashSet::new(),
            last_upgrade_set: Vec::new(),
            full_requested_at: HashMap::new(),
            menu: None,
            #[cfg(target_os = "macos")]
            window_menu: None,
            #[cfg(target_os = "macos")]
            native_fullscreen_item: None,
            #[cfg(target_os = "macos")]
            native_fullscreen_on: false,
            #[cfg(target_os = "macos")]
            proxy_icon_path: None,
            save_rotation_item: None,
            save_enabled: false,
            cancel_scan_item: None,
            cancel_scan_enabled: false,
            undo_stack: Vec::new(),
            undo_item: None,
            undo_menu_state: None,
            view_checks: None,
            view_checks_state: None,
            pending_delete: None,
            pending_confirm_delete: None,
            menu_attached: false,
            dialog: None,
            keymap: Keymap::load(),
            settings,
            archive_load: None,
            archive_gen: 0,
            dir_scan: None,
            scan_gen: 0,
            pending_launch: None,
            password_archive: None,
            playback: None,
            anim_frame_shown_at: None,
            anim_decode: None,
            anim_gen: 0,
            anim_hint_shown_for: None,
            framestep_started: None,
            framestep_last: None,
        }
    }

    /// The decode-to-fit target for the current mode: the display size in Fit mode
    /// (downscale large photos), or full resolution for Fill / Original (so Fill
    /// isn't upscale-blurry and Original is pixel-exact).
    fn decode_fit(&self) -> Option<FitBox> {
        match self.view.mode {
            ScaleMode::Fit => self.fit,
            ScaleMode::Fill | ScaleMode::Original => None,
        }
    }

    /// Estimated bytes for one resident ring slot at the current scale mode: the
    /// decode-target box for bounded modes (Fit, and Fill later), or the current
    /// photo's true full-res size for Original. Sizes the ring so VRAM stays in
    /// budget even though full-res textures are much larger than fit ones.
    fn slot_bytes_estimate(&self) -> u64 {
        let fit = self.fit.unwrap_or(FitBox {
            max_width: 1,
            max_height: 1,
        });
        let fit_bytes = fit.max_width as u64 * fit.max_height as u64 * 4;
        match self.decode_fit() {
            // Bounded decode (Fit/Fill): a slot is at most the target box.
            Some(b) => (b.max_width as u64 * b.max_height as u64 * 4).max(1),
            // Full-res (Original): estimate from the current photo's true size,
            // never below a fit slot (and clamp_to_max bounds the real extreme).
            None => self
                .current
                .as_ref()
                .map(|m| (m.w as u64 * m.h as u64 * 4).max(fit_bytes))
                .unwrap_or(fit_bytes),
        }
    }

    /// Recompute the prefetch want-list and hand it to the decode pool. Two tiers:
    /// the whole window is fetched as fast **previews** (HEIC thumbnails etc.) so
    /// scrolling never outruns decode; then, once **settled**, a current-first ring
    /// of the resident window is re-fetched at full resolution and upgraded in place
    /// (see `upgrade_set`). While a nav key is held the upgrade set is empty, so fast
    /// scrolling stays entirely on the cheap preview tier — the parallel decoders
    /// aren't tied up on fulls you fly past. (Pre-libheif this was a single on-screen
    /// full because WIC's HEVC decoder serialized; libheif decodes in parallel, so we
    /// now fill a VRAM-bounded ring of fulls around the cursor.)
    fn request_prefetch(&mut self) {
        // While a folder scan is streaming in, the random deck regenerates on every batch,
        // so prefetching the random look-ahead would decode-then-evict photos the user never
        // sees (thrash). Use the sequential-only, no-wrap variant until the scan completes,
        // then normal prefetch (with its random hedges) resumes (`poll_dir_scan` Done arm).
        self.targets = if self.dir_scan.is_some() {
            prefetch_targets_scanning(&self.playlist, self.ahead, self.behind)
        } else {
            prefetch_targets(&self.playlist, self.ahead, self.behind)
        };
        let fit = self.decode_fit();
        // Drop tier bookkeeping for items no longer resident (evicted).
        self.preview_resident
            .retain(|i| self.ring.slot_for(*i).is_some());
        self.upgrade_done
            .retain(|i| self.ring.slot_for(*i).is_some());
        self.full_requested_at
            .retain(|i, _| self.ring.slot_for(*i).is_some());
        // Items decoded but not yet uploaded must not be re-requested (the pool no
        // longer tracks them, so it would decode them again).
        let pending: HashSet<usize> = self.pending_uploads.iter().map(|o| o.key.item).collect();
        let sharpen = self.sharpen_now();
        let ring: HashSet<usize> = self.prefetch_fulls().into_iter().collect();
        // Stamp when each full was first requested, for the `sharpen` latency metric.
        if let Some(d) = sharpen {
            self.full_requested_at.entry(d).or_insert_with(Instant::now);
        }
        for &t in &ring {
            self.full_requested_at.entry(t).or_insert_with(Instant::now);
        }

        // Build the job list in three priority tiers (the pool decodes by position):
        //   1. `sharpen` — the on-screen photo's full, so what you're looking at goes
        //      sharp ASAP the moment you park.
        //   2. previews — the whole window, so flying / re-flying is always instant.
        //   3. `ring` fulls — the sharp ring prefetched around the cursor, queued
        //      behind every preview, so a fast fly stays smooth (these decode only in
        //      the pool's spare capacity) and the fulls land ahead of where you're
        //      heading — a stop finds the photo already sharp.
        type Job = (usize, Option<FitBox>, bool);
        let (mut head, mut previews, mut fulls): (Vec<Job>, Vec<Job>, Vec<Job>) =
            (Vec::new(), Vec::new(), Vec::new());
        for &t in &self.targets {
            if self.failed.contains(&t) || pending.contains(&t) {
                continue;
            }
            let resident = self.ring.slot_for(t).is_some();
            let is_prev = resident && self.preview_resident.contains(&t);
            if resident && !is_prev {
                continue; // already full
            }
            if !resident {
                previews.push((t, fit, true));
            } else if Some(t) == sharpen {
                head.push((t, fit, false));
            } else if ring.contains(&t) {
                fulls.push((t, fit, false));
            }
            // else: resident preview not in the ring → leave it as a preview
        }
        let mut jobs = head;
        jobs.append(&mut previews);
        jobs.append(&mut fulls);
        self.pool.set_targets(self.epoch, &self.source, &jobs);
    }

    /// The on-screen photo to sharpen FIRST (top decode priority): the displayed one,
    /// but only when parked (no nav key held) and currently a resident preview with a
    /// better decode to pull. `None` while flying (sharpening a frame that's about to
    /// change is pointless) and `None` once it's already full.
    fn sharpen_now(&self) -> Option<usize> {
        if self.held_nav().is_some() {
            return None;
        }
        let d = self.displayed_item?;
        (self.ring.slot_for(d).is_some()
            && self.preview_resident.contains(&d)
            && !self.upgrade_done.contains(&d))
        .then_some(d)
    }

    /// The full-res "sharp ring" to prefetch around the cursor at LOW priority (below
    /// every preview) — a VRAM-bounded, current-first prefix of the window, filtered
    /// to resident previews, minus `sharpen_now` (requested at high priority instead).
    ///
    /// Unlike `sharpen_now`, this runs EVEN WHILE FLYING: the fulls are queued behind
    /// all previews (see `request_prefetch`), so a fast fly stays preview-smooth — the
    /// pool decodes them only in spare capacity. But as you slow down or browse, the
    /// fulls for where you're heading land *ahead* of you, so a stop finds the photo
    /// already sharp instead of paying a cold ~115 ms–1 s decode after the fact. The
    /// workers that decode them would otherwise be idle, so it's near-free.
    fn prefetch_fulls(&self) -> Vec<usize> {
        let full_bytes = self.slot_bytes_estimate();
        let sharpen = self.sharpen_now();
        full_ring(
            &self.targets,
            full_bytes,
            RING_BUDGET_BYTES,
            self.ring.capacity().min(MAX_FULL_RING),
        )
        .into_iter()
        .filter(|&i| {
            Some(i) != sharpen
                && self.ring.slot_for(i).is_some()
                && self.preview_resident.contains(&i)
                && !self.upgrade_done.contains(&i)
                && !self.is_raw_item(i)
        })
        .collect()
    }

    /// Whether `item`'s full decode is a slow RAW demosaic (seconds, and once started
    /// it can't be cancelled). Excluded from the speculative ahead-ring so a few RAWs
    /// in the window can't tie up the decode workers — starving the previews a fly
    /// needs — for neighbours you may never visit. The displayed RAW still sharpens
    /// via `sharpen_now`, and a RAW's embedded preview is often near-full-res anyway.
    fn is_raw_item(&self, item: usize) -> bool {
        Path::new(self.source.name(item))
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| pb_decode::is_raw_extension(&e.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    /// All items we currently want decoded to full (sharpen + ahead-ring), for the
    /// idle pump's change-detection and "keep ticking while sharpening" gate.
    fn fulls_wanted(&self) -> Vec<usize> {
        let mut v = Vec::new();
        if let Some(d) = self.sharpen_now() {
            v.push(d);
        }
        v.extend(self.prefetch_fulls());
        v
    }

    /// Load the per-photo view state for `item`: rotation from the RAM override
    /// map (upright if absent), zoom/pan reset to a fresh framing. Returns the
    /// view to push to the renderer. (Scaling mode is global and left unchanged.)
    fn view_for(&mut self, item: usize) -> ViewTransform {
        self.view.rotation = self.rotations.get(&item).copied().unwrap_or_default();
        self.view.zoom = 1.0;
        self.view.pan = [0.0, 0.0];
        self.view
    }

    /// Rotate the on-screen photo 90° clockwise (counter-clockwise on `Shift+R`).
    /// Per-image and RAM-only; returning to upright drops the override entry.
    fn rotate(&mut self, ccw: bool, event_loop: &ActiveEventLoop) {
        let Some(item) = self.displayed_item else {
            return;
        };
        let cur = self.rotations.get(&item).copied().unwrap_or_default();
        let new = if ccw { cur.ccw() } else { cur.cw() };
        if new == Rotation::default() {
            self.rotations.remove(&item);
        } else {
            self.rotations.insert(item, new);
        }
        self.view.rotation = new;
        self.push_view();
        // Flash a directional rotate icon (icon-only pill) as feedback.
        let ico = if ccw {
            icon::assets::ROTATE_LEFT
        } else {
            icon::assets::ROTATE_RIGHT
        };
        self.show_toast_icon("", Some(ico), event_loop);
    }

    /// Copy the current photo to the OS clipboard (`Ctrl+C` / Edit ▸ Copy, task #27).
    ///
    /// Decodes the original at **full resolution** here — not the fit-downscaled ring
    /// texture — so a paste lands at native size. This is a synchronous decode on the
    /// event-loop thread, which is fine: Copy is an explicit, infrequent user command
    /// (like the modal file picker), not the nav hot path. Any in-RAM rotation
    /// override is baked into the copied pixels so the clipboard is WYSIWYG.
    fn copy_image(&mut self, event_loop: &ActiveEventLoop) {
        let Some(item) = self.displayed_item else {
            return; // empty state — nothing to copy
        };
        let img = match decode_item(self.source.as_ref(), item, None, false) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("copy: decode failed: {}: {e}", self.source.name(item));
                self.show_toast("Copy failed", event_loop);
                return;
            }
        };
        let rgba = clipboard::to_clipboard_rgba8(&img);
        let rot = self.rotations.get(&item).copied().unwrap_or_default();
        let (rgba, w, h) = clipboard::rotate_rgba8(&rgba, img.width, img.height, rot);
        // Offer the source file as CF_HDROP too when there is one; an archive entry
        // has no file on disk, so it gets an image-only copy (pixels still paste).
        let wrote = match self.source.path(item) {
            Some(path) => clipboard::set_image_and_file(w, h, &rgba, path),
            None => clipboard::set_image(w, h, &rgba),
        };
        match wrote {
            // Icon-only pill (the clipboard glyph says it all).
            Ok(()) => self.show_toast_icon("", Some(icon::assets::CLIPBOARD), event_loop),
            Err(e) => {
                eprintln!("copy: clipboard write failed: {e}");
                self.show_toast("Copy failed", event_loop);
            }
        }
    }

    /// Copy the current photo's **file path** to the clipboard as text (Shift+Ctrl+C /
    /// Edit ▸ Copy File Path; ⇧⌘C on macOS). The full path for a filesystem source, or
    /// the entry name for an archive (which has no path on disk). An explicit user
    /// command — never the view path. Uses the cross-platform text clipboard (arboard),
    /// separate from the image clipboard (`clipboard.rs`).
    fn copy_path(&mut self, event_loop: &ActiveEventLoop) {
        let Some(item) = self.displayed_item else {
            return; // empty state — nothing to copy
        };
        let text = match self.source.path(item) {
            Some(p) => p.to_string_lossy().into_owned(),
            None => self.source.name(item).to_string(),
        };
        let fname = file_name_of(&text).to_string();
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(()) => {
                let msg = format!("Copied {fname}");
                self.show_toast(&msg, event_loop);
            }
            Err(e) => {
                eprintln!("copy path: clipboard write failed: {e}");
                self.show_toast("Copy path failed", event_loop);
            }
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
    fn save_rotation(&mut self, event_loop: &ActiveEventLoop) {
        let Some(item) = self.displayed_item else {
            return;
        };
        let rot = self.rotations.get(&item).copied().unwrap_or_default();
        if rot == Rotation::default() {
            self.show_toast("No rotation to save", event_loop);
            return;
        }
        let Some(path) = self.source.path(item).map(Path::to_path_buf) else {
            // Archive entry — no file on disk to write back to.
            self.show_toast("Can't save rotation here", event_loop);
            return;
        };
        if !save_rotation::is_orientation_writable(&path) {
            self.show_toast("Save rotation: JPEG only", event_loop);
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
                self.rotations.remove(&item);
                self.meta_cache.remove(&item);
                self.failed.remove(&item);
                self.preview_resident.remove(&item);
                self.upgrade_done.remove(&item);
                self.invalidate_geometry();
                self.load_current_sync(event_loop);
                self.target_item = self.playlist.current();
                self.request_prefetch();
                self.undo_stack.push(UndoAction::SaveRotation {
                    item,
                    path: path.clone(),
                    prev,
                });
                self.show_toast_icon("", Some(icon::assets::FLOPPY), event_loop);
            }
            Err(e) => {
                eprintln!("save rotation failed: {}: {e}", path.display());
                self.show_toast("Save failed", event_loop);
            }
        }
    }

    /// Reverse the most recent reversible edit (Edit ▸ Undo / `Ctrl+Z`). Today the only
    /// entry kind is a saved rotation: rewrite the file's EXIF Orientation back to the
    /// value it held before the save, then refresh so the reverted file is re-read
    /// (`invalidate_geometry` rebuilds the ring, so neighbors re-decode from disk too —
    /// the undone photo shows correctly whether or not it's the one on screen). On a
    /// write failure the file is untouched, so the entry is pushed back to retry.
    fn undo(&mut self, event_loop: &ActiveEventLoop) {
        let Some(action) = self.undo_stack.pop() else {
            self.show_toast("Nothing to undo", event_loop);
            return;
        };
        match action {
            UndoAction::SaveRotation { item, path, prev } => {
                match save_rotation::set_orientation(&path, prev) {
                    Ok(()) => {
                        self.rotations.remove(&item);
                        self.meta_cache.remove(&item);
                        self.failed.remove(&item);
                        self.preview_resident.remove(&item);
                        self.upgrade_done.remove(&item);
                        self.invalidate_geometry();
                        self.load_current_sync(event_loop);
                        self.target_item = self.playlist.current();
                        self.request_prefetch();
                        self.show_toast_icon(
                            "Rotation undone",
                            Some(icon::assets::UNDO),
                            event_loop,
                        );
                    }
                    Err(e) => {
                        eprintln!("undo rotation failed: {}: {e}", path.display());
                        self.show_toast("Undo failed", event_loop);
                        // The file wasn't changed, so the edit is still reversible —
                        // keep it on the stack for a retry.
                        self.undo_stack
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
    fn delete_current(&mut self, permanent: bool, event_loop: &ActiveEventLoop) {
        // Settle any still-pending delete-advance first (e.g. a rapid second Del).
        self.flush_pending_delete(event_loop);
        let Some(item) = self.displayed_item else {
            return;
        };
        let Some(path) = self.source.path(item).map(Path::to_path_buf) else {
            self.show_toast("Can't delete this", event_loop); // archive entry — no file
            return;
        };
        if permanent {
            // Permanent delete is irreversible — confirm first, via the themed egui
            // dialog (dark-aware, and cross-platform for the macOS port). The delete
            // runs when the dialog answers Yes (`dialog_event`), on this item.
            let name = file_name_of(self.source.name(item));
            self.pending_confirm_delete = Some(item);
            self.open_confirm_delete(&name, event_loop);
            return;
        }
        self.do_delete(item, &path, false, event_loop);
    }

    /// Perform the actual deletion of `item` (`path`) — recoverable (Recycle Bin) or
    /// permanent — then flash an icon-only pill on the still-shown photo and defer the
    /// playlist advance a beat (`DELETE_ADVANCE_DELAY`) so the feedback registers
    /// first. The permanent path reaches here only after the confirm dialog's Yes.
    fn do_delete(
        &mut self,
        item: usize,
        path: &Path,
        permanent: bool,
        event_loop: &ActiveEventLoop,
    ) {
        let res = if permanent {
            delete::delete_permanently(path)
        } else {
            delete::send_to_trash(path)
        };
        if let Err(e) = res {
            eprintln!("delete failed: {}: {e}", path.display());
            self.show_toast("Delete failed", event_loop);
            return;
        }
        // Deleting a playing animation stops playback so the doomed photo freezes on
        // its current frame under the trash icon (rather than animating until removal).
        self.stop_playback();
        // Recycle-bin icon for the recoverable delete, trash for a permanent one.
        let icon = if permanent {
            icon::assets::TRASH
        } else {
            icon::assets::RECYCLE
        };
        self.show_toast_icon("", Some(icon), event_loop);
        self.pending_delete = Some((Instant::now() + DELETE_ADVANCE_DELAY, item));
    }

    /// Perform a deferred delete's playlist advance: drop the removed item, rebuild the
    /// source from the remaining paths (indices shift, so index-keyed state resets —
    /// fine for an explicit, infrequent command), and advance to the next photo (the
    /// previous if it was the last; the empty state if none remain). Idempotent — a
    /// no-op when nothing is pending.
    fn flush_pending_delete(&mut self, event_loop: &ActiveEventLoop) {
        let Some((_, removed)) = self.pending_delete.take() else {
            return;
        };
        let len = self.source.len();
        // If a scan is still streaming in, tombstone the deleted path so a later batch (whose
        // cumulative list still has it) can't bring it back. (No-op once the scan finishes.)
        if self.dir_scan.is_some() {
            if let Some(p) = self.source.path(removed).map(Path::to_path_buf) {
                self.deleted.insert(p);
            }
        }
        match delete::cursor_after_removal(len, removed) {
            None => self.enter_empty_state(event_loop),
            Some(start) => {
                let remaining: Vec<PathBuf> = (0..len)
                    .filter(|&i| i != removed)
                    .filter_map(|i| self.source.path(i).map(Path::to_path_buf))
                    .collect();
                let src: Arc<dyn PhotoSource> = Arc::new(FsSource::new(remaining));
                let root = self.root.clone();
                let scan_root = self.scan_root.clone();
                let recursive = self.recursive;
                self.rebuild_playlist(src, root, scan_root, recursive, start, event_loop);
            }
        }
    }

    /// Clear to the "no images" placeholder after the last photo is deleted. Mirrors
    /// the bare-launch empty state (a test pattern + title; `O`/drag-drop reopen).
    fn enter_empty_state(&mut self, event_loop: &ActiveEventLoop) {
        self.pending_delete = None;
        self.source = Arc::new(FsSource::new(Vec::new()));
        self.playlist = Playlist::new(0, 0);
        self.rotations.clear();
        self.meta_cache.clear();
        self.failed.clear();
        self.preview_resident.clear();
        self.upgrade_done.clear();
        self.last_upgrade_set.clear();
        self.undo_stack.clear();
        self.invalidate_geometry();
        self.displayed_item = None;
        self.target_item = None;
        self.current = None;
        if let Some(a) = self.active.as_mut() {
            a.renderer.clear_image();
            a.renderer.set_overlay(None, 0);
            a.window.set_title("PhotoBlaze");
        }
        // Blank background + the centered "Press O to open…" hint (mirrors a bare launch).
        self.show_open_hint();
        self.overlay_shown = false;
        self.overlay_item = None;
        self.draw(event_loop);
    }

    /// Show ring `slot` (holding `item`): the keypress fast path — a rebind, no
    /// decode or upload. Updates the pin, title, and info panel.
    fn present_item(&mut self, item: usize, slot: usize, event_loop: &ActiveEventLoop) {
        let view = self.view_for(item);
        let title = title_for(self.source.name(item), item, self.source.len());
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_view(view);
            a.renderer.present_slot(slot);
            a.window.set_title(&title);
        }
        self.ring.set_displayed(slot);
        self.displayed_item = Some(item);
        self.current = self.meta_cache.get(&item).cloned();
        // The panel (if shown) is now stale for the old photo; `about_to_wait`
        // rebuilds it for `item` next tick (or hides it while flying), so it
        // tracks the photo with no blank flash. The bitmap stays up meanwhile.
        self.last_present = Some(Instant::now());
        self.draw(event_loop);
    }

    /// A target that failed to decode (corrupt/unreadable): count it as "shown"
    /// so the gated advance isn't stuck on it, but clear the previous frame's
    /// stale metadata — set a decode-error window title and drop the info panel so
    /// neither misreports the held-over pixels as the failed photo. The previous
    /// frame stays up rather than flashing black.
    fn present_failed(&mut self, item: usize, event_loop: &ActiveEventLoop) {
        self.displayed_item = Some(item);
        self.current = None;
        let name = file_name_of(self.source.name(item));
        let total = self.source.len();
        if let Some(a) = self.active.as_mut() {
            a.window
                .set_title(&format!("{name} ({}/{total}) - decode error", item + 1));
        }
        // The info panel belonged to the previous photo — drop it (and redraw to
        // remove it). Only touch the renderer if a panel was actually showing.
        if self.overlay_shown {
            if let Some(a) = self.active.as_mut() {
                a.renderer.set_overlay(None, 0);
            }
            self.overlay_shown = false;
            self.overlay_item = None;
            self.draw(event_loop);
        }
    }

    /// Try to show `target_item`: present it on a ring hit, otherwise keep the
    /// previous frame (a miss is a hold, never a skip). Returns whether shown.
    fn try_present_target(&mut self, event_loop: &ActiveEventLoop) -> bool {
        let Some(item) = self.target_item else {
            return false;
        };
        if self.displayed_item == Some(item) {
            return true;
        }
        if self.failed.contains(&item) {
            // Known-bad file: count it as shown (the previous frame stays up) so
            // navigation never stalls on a corrupt prefetched JPEG.
            self.present_failed(item, event_loop);
            return true;
        }
        if let Some(slot) = self.ring.slot_for(item) {
            self.present_item(item, slot, event_loop);
            true
        } else {
            false
        }
    }

    /// Drain finished decodes: discard stale/duplicate results, handle decode
    /// errors, then upload the highest-priority ready images (**current target
    /// first**) into ring slots — at most `UPLOADS_PER_TICK` per tick so a burst
    /// can't blow the frame budget. Lower-priority leftovers are stashed for the
    /// next tick (so the target never waits behind neighbors), keeping their pool
    /// byte-budget reservation as backpressure.
    fn drain_results(&mut self, event_loop: &ActiveEventLoop) {
        // Gather everything ready plus last tick's leftovers, dropping stale /
        // duplicate / errored results so only live decoded images remain.
        let mut ready: Vec<Outcome> = std::mem::take(&mut self.pending_uploads);
        while let Ok(o) = self.results.try_recv() {
            ready.push(o);
        }
        let mut target_failed: Option<usize> = None;
        ready.retain(|o| {
            if o.key.epoch != self.epoch {
                return false; // stale geometry
            }
            let item = o.key.item;
            let resident = self.ring.slot_for(item).is_some();
            if let Err(ref e) = o.result {
                if resident {
                    // A full-upgrade decode failed, but the resident preview is fine
                    // — keep it and stop retrying the upgrade.
                    self.upgrade_done.insert(item);
                    return false;
                }
                eprintln!("decode failed for item {item}: {e}");
                self.failed.insert(item);
                // Unstick the gated loop: a corrupt target counts as "shown".
                // (Deferred out of the closure — `present_failed` needs &mut self.)
                if self.target_item == Some(item) {
                    target_failed = Some(item);
                }
                return false;
            }
            if resident {
                // Already resident. The only outcome we still want is a *full*
                // decode upgrading a resident preview (uploaded in place below). A
                // preview-only upgrade result (e.g. RAW whose only image is its
                // preview) is marked done here so the idle pass stops retrying —
                // otherwise the upgrade loops forever, re-decoding every tick. Any
                // other already-resident duplicate is dropped.
                let is_prev = self.preview_resident.contains(&item);
                let img = o.result.as_ref().expect("Err handled above");
                if is_prev && img.is_preview {
                    self.upgrade_done.insert(item);
                }
                return is_prev && !img.is_preview;
            }
            true
        });
        if let Some(item) = target_failed {
            self.present_failed(item, event_loop);
        }

        // Current target first, then by prefetch priority, unknowns last.
        let target = self.target_item;
        ready.sort_by_key(|o| {
            let item = o.key.item;
            if target == Some(item) {
                0usize
            } else {
                self.targets
                    .iter()
                    .position(|&t| t == item)
                    .map(|p| p + 1)
                    .unwrap_or(usize::MAX)
            }
        });

        let mut uploads = 0;
        let mut leftover = Vec::new();
        for outcome in ready {
            let item = outcome.key.item;
            let Ok(ref img) = outcome.result else {
                continue; // errors were already filtered out above
            };
            // A full decode for an item already resident as a preview is its
            // in-place upgrade (the retain above kept only real fulls; preview-only
            // upgrade results were already marked `upgrade_done` and dropped).
            let upgrade =
                self.preview_resident.contains(&item) && self.ring.slot_for(item).is_some();
            if uploads >= UPLOADS_PER_TICK {
                // Carry still-wanted leftovers to the next tick (in priority order);
                // drop now-obsolete ones so they don't pin pool byte-budget while
                // the loop idles (work_pending wouldn't keep polling for them).
                if self.targets.contains(&item) && (upgrade || self.ring.slot_for(item).is_none()) {
                    leftover.push(outcome);
                }
                continue;
            }
            if !self.meta_cache.contains_key(&item) {
                let m = meta_for(self.source.as_ref(), item, &self.root, img);
                self.meta_cache.insert(item, m);
            }
            let item_bytes = img.pixels.len() as u64;
            if upgrade {
                let slot = self.ring.slot_for(item).expect("resident as preview");
                if let Some(a) = self.active.as_mut() {
                    let t0 = Instant::now();
                    a.renderer.upload_slot(
                        slot,
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(img),
                        img.peak,
                    );
                    self.metrics.record("upload", t0.elapsed());
                }
                self.ring.set_slot_bytes(item, item_bytes);
                self.preview_resident.remove(&item);
                // Real end-to-end sharpen latency for the ON-SCREEN photo (what the
                // user actually waits on): full requested → full on screen. Ahead-ring
                // fulls land late by design (low priority), so they'd skew this — only
                // record the displayed one.
                let t0 = self.full_requested_at.remove(&item);
                if self.displayed_item == Some(item) {
                    if let Some(t0) = t0 {
                        self.metrics.record("sharpen", t0.elapsed());
                    }
                }
                uploads += 1;
                // If it's the photo on screen, re-present the slot so the renderer
                // picks up the full texture's dimensions/peak and re-places the quad
                // (it kept the preview's dims otherwise — visible in Original mode),
                // then redraw it now-sharp. `present_slot` keeps the current view, so
                // any zoom/pan is preserved.
                if self.displayed_item == Some(item) {
                    if let Some(a) = self.active.as_mut() {
                        a.renderer.present_slot(slot);
                    }
                    self.draw(event_loop);
                }
                continue;
            }
            if let Some(res) = self
                .ring
                .reserve_bytes(item, self.epoch, item_bytes, &self.targets)
            {
                if let Some(a) = self.active.as_mut() {
                    let t0 = Instant::now();
                    a.renderer.upload_slot(
                        res.slot,
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(img),
                        img.peak,
                    );
                    self.metrics.record("upload", t0.elapsed());
                }
                self.ring.mark_resident(item, res.slot, self.epoch);
                if img.is_preview {
                    self.preview_resident.insert(item);
                } else {
                    self.preview_resident.remove(&item);
                }
                uploads += 1;
                if self.target_item == Some(item) && self.displayed_item != Some(item) {
                    self.present_item(item, res.slot, event_loop);
                }
            }
            // reserve == None (no longer wanted): drop the outcome, freeing budget.
        }
        self.pending_uploads = leftover;
    }

    /// Synchronous decode + display of the current item — an instant frame on the
    /// first paint and on geometry changes (resize / scale-mode toggle), before the
    /// async ring re-fills neighbors at the new resolution.
    fn load_current_sync(&mut self, event_loop: &ActiveEventLoop) {
        let Some(idx) = self.playlist.current() else {
            return;
        };
        let t0 = Instant::now();
        // Preview-first (`allow_preview = true`): this decode runs **synchronously on
        // the event-loop thread**, so it must be fast. For RAW/HEIC that means the
        // embedded preview (tens of ms) instead of a full sensor demosaic — which on a
        // 40 MB NEF is ~20 s and froze the loop into a beachball on a Finder open. The
        // full-resolution decode lands off-thread: `request_prefetch` (called by every
        // caller right after this) re-decodes this item into the ring and `sharpen_now`
        // upgrades the on-screen preview to full in place (`drain_results`). This is the
        // documented "preview-first, then refine" model, now applied to the first frame
        // too. (JPEG/PNG/etc. have no cheaper preview, so this is a full decode anyway —
        // fast enough not to beachball, and faster still once dev builds optimize the
        // decoders; see the `[profile.dev]` note in the workspace Cargo.toml.)
        let decoded = decode_item(self.source.as_ref(), idx, self.decode_fit(), true);
        self.metrics.record("decode", t0.elapsed());
        match decoded {
            Ok(img) => {
                let meta = meta_for(self.source.as_ref(), idx, &self.root, &img);
                self.current = Some(meta.clone());
                self.meta_cache.insert(idx, meta);
                let view = self.view_for(idx);
                let title = title_for(self.source.name(idx), idx, self.source.len());
                if let Some(a) = self.active.as_mut() {
                    a.renderer.set_view(view);
                    a.renderer.set_image(
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(&img),
                        img.peak,
                    );
                    a.renderer.set_overlay(None, 0);
                    a.window.set_title(&title);
                }
                self.overlay_shown = false;
                self.overlay_item = None;
                self.displayed_item = Some(idx);
            }
            Err(e) => {
                eprintln!("decode failed: {}: {e}", self.source.name(idx));
                self.failed.insert(idx);
                // Keep the gate unstuck (count the bad file as "shown") and clear
                // the stale frame's title/panel so they don't misreport it.
                self.present_failed(idx, event_loop);
            }
        }
        self.last_present = Some(Instant::now());
        self.draw(event_loop);
    }

    /// Bump the geometry epoch and rebuild the (now-invalid) ring. Called on resize
    /// and fit/original toggle so in-flight decodes for the old size are discarded.
    fn invalidate_geometry(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        let fit = self.fit.unwrap_or(FitBox {
            max_width: 1,
            max_height: 1,
        });
        let cap = ring_capacity(self.slot_bytes_estimate());
        self.ring = ResidentRing::new_with_budget(cap, RING_BUDGET_BYTES);
        if let Some(a) = self.active.as_mut() {
            a.renderer.reserve_ring(cap, fit.max_width, fit.max_height);
        }
        let (ahead, behind) = window_for_capacity(cap);
        self.ahead = ahead;
        self.behind = behind;
        // Drop decodes staged for the old geometry; they free their pool budget.
        self.pending_uploads.clear();
    }

    /// Apply a scaling mode (8 = fit, 9 = fill, 0 toggles original ↔ fill). Always
    /// resets zoom/pan back to the mode's natural framing — so tapping a mode key
    /// is also "reset my zoom." Only an actual mode *change* bumps the geometry
    /// epoch and re-buffers neighbors (the decode resolution can change); pressing
    /// the current mode's key just re-frames, no re-decode.
    fn set_scale_mode(&mut self, mode: ScaleMode, event_loop: &ActiveEventLoop) {
        let changed = self.view.mode != mode;
        self.view.mode = mode;
        self.view.zoom = 1.0;
        self.view.pan = [0.0, 0.0];
        self.push_view();
        if changed {
            self.invalidate_geometry();
            self.load_current_sync(event_loop);
            self.target_item = self.playlist.current();
            self.request_prefetch();
        } else {
            self.draw(event_loop);
        }
    }

    /// Defer a launch **plan** until the window + engine exist (`resumed` fires it).
    /// Used for an archive *and* a folder scan on the command line / double-click so startup
    /// shows the window first and the open runs behind the spinner / dialog / streaming scan
    /// (a synchronous launch resolve, before the event loop, blocked the window on a big
    /// tree). The plan — not the raw input — is deferred so the startup recursive override is
    /// preserved (re-planning in `resumed` would drop it).
    fn queue_launch(&mut self, plan: open::OpenPlan) {
        self.pending_launch = Some(plan);
    }

    /// Open a launch input at runtime (the file picker or a drag-drop): plan it,
    /// build the playlist, and jump to the plan's cursor (the dropped/clicked
    /// photo, or the first of a folder). Empty selections are ignored so the
    /// current photo isn't blanked.
    fn open_input(&mut self, input: LaunchInput, event_loop: &ActiveEventLoop) {
        let plan = open::plan(input);
        self.open_plan(plan.source, plan.cursor, event_loop);
    }

    /// Route a planned open to the right path — archives async, folder scans stream, an
    /// explicit list resolves inline. Shared by runtime opens ([`open_input`](App::open_input))
    /// and the deferred startup launch ([`resumed`](App::resumed)), so the startup recursive
    /// override (carried on the plan) is honored on both paths.
    fn open_plan(&mut self, source: Source, cursor: open::Cursor, event_loop: &ActiveEventLoop) {
        // Archives open via the async-aware path (a .7z decompresses off-thread so it
        // can't freeze the loop).
        if let Source::Archive(path) = &source {
            self.begin_archive_open(path.clone(), None, event_loop);
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
        self.rebuild_playlist(
            r.source,
            r.root,
            r.scan_root,
            r.recursive,
            r.start,
            event_loop,
        );
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
    fn begin_archive_open(
        &mut self,
        path: PathBuf,
        password: Option<String>,
        event_loop: &ActiveEventLoop,
    ) {
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
            self.finish_archive_open(result, was_password_attempt, path, event_loop);
            return;
        }
        // 7z: refuse instantly if it won't fit RAM (before any background work). A
        // pre-flight password error (a header-encrypted archive) routes to the prompt
        // like any other, not the generic error dialog.
        if let Err(e) = seven_z_preflight(&path, password.as_deref()) {
            self.finish_archive_open(Err(e), was_password_attempt, path, event_loop);
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
            let refresh = self.refresh_hz();
            let parent = self.active.as_ref().map(|a| a.window.clone());
            let mut dlg = dialog::DialogWindow::open(
                dialog::DialogKind::Loading,
                event_loop,
                refresh,
                &msg,
                &self.settings,
                &self.keymap,
                parent.as_deref(),
            );
            if let Some(d) = dlg.as_mut() {
                d.set_progress(Some(progress.clone()));
                d.request_redraw();
            }
            self.dialog = dlg;
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
    fn poll_archive_load(&mut self, event_loop: &ActiveEventLoop) {
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
                self.finish_archive_open(result, was_attempt, path, event_loop);
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
        event_loop: &ActiveEventLoop,
    ) {
        match result {
            Ok(r) if !r.source.is_empty() => {
                self.password_archive = None;
                self.close_dialog();
                self.rebuild_playlist(
                    r.source,
                    r.root,
                    r.scan_root,
                    r.recursive,
                    r.start,
                    event_loop,
                );
            }
            Ok(_) => self.fail_archive_open(&archive::ArchiveOpenError::Empty, event_loop),
            Err(archive::ArchiveOpenError::PasswordRequired) => {
                self.prompt_archive_password(path, was_password_attempt, event_loop)
            }
            // User cancelled: drop quietly, keeping whatever was on screen — no error
            // dialog. The loading dialog is already closed (or closes here as a backstop).
            Err(archive::ArchiveOpenError::Cancelled) => {
                self.password_archive = None;
                self.close_dialog();
            }
            Err(e) => self.fail_archive_open(&e, event_loop),
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
        self.deleted.clear(); // fresh scan → fresh universe, no stale tombstones
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
    fn poll_dir_scan(&mut self, event_loop: &ActiveEventLoop) {
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
                        self.rebuild_playlist(
                            resolved.source,
                            resolved.root,
                            resolved.scan_root,
                            resolved.recursive,
                            resolved.start,
                            event_loop,
                        );
                    } else {
                        // Later batch: grow the playlist in place, keeping the displayed
                        // photo and every per-image cache (indices are append-only).
                        self.extend_playlist(resolved.source);
                    }
                }
                Ok((generation, ScanUpdate::Done)) => {
                    if generation != cur_gen {
                        continue; // superseded
                    }
                    let scan = self.dir_scan.take();
                    self.close_scanning_dialog(); // walk finished — drop the progress dialog
                    if scan.is_some_and(|s| !s.bootstrapped) {
                        eprintln!("PhotoBlaze: no supported images in that selection");
                        // Nothing was ever shown and the scan found nothing: restore the
                        // "Press O to open" hint the scan had suppressed (a bare-folder launch
                        // onto an empty folder), but never blank an existing photo.
                        if self.source.is_empty() {
                            self.show_open_hint();
                        }
                    }
                    // Deck is final now: resume normal prefetch (random-ahead warm again).
                    self.request_prefetch();
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
                        self.open_scanning_dialog(&name, progress, event_loop);
                    }
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    self.dir_scan = None;
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
    fn open_scanning_dialog(
        &mut self,
        name: &str,
        progress: ScanProgress,
        event_loop: &ActiveEventLoop,
    ) {
        let msg = scan_message(name);
        let refresh = self.refresh_hz();
        let parent = self.active.as_ref().map(|a| a.window.clone());
        let mut dlg = dialog::DialogWindow::open(
            dialog::DialogKind::Scanning,
            event_loop,
            refresh,
            &msg,
            &self.settings,
            &self.keymap,
            parent.as_deref(),
        );
        if let Some(d) = dlg.as_mut() {
            d.set_scan(&msg, progress);
        }
        self.dialog = dlg;
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
    }

    /// User command (File ▸ Stop Scanning, or a bound key): stop an in-flight folder scan,
    /// **keeping whatever has streamed in so far** (cancel-keeps-partial — the partial
    /// playlist is already live). Resumes normal prefetch (the deck is final now) and flashes
    /// a confirmation. A no-op when no scan is running (the menu item is disabled then).
    fn cancel_scan_command(&mut self, event_loop: &ActiveEventLoop) {
        if self.dir_scan.is_none() {
            return;
        }
        self.cancel_dir_scan();
        self.dir_scan = None;
        self.close_scanning_dialog();
        self.request_prefetch();
        self.show_toast("Scan stopped", event_loop);
    }

    /// A terminal archive-open failure (not a password retry): forget the pending
    /// archive and replace any open dialog with the error notice.
    fn fail_archive_open(&mut self, e: &archive::ArchiveOpenError, event_loop: &ActiveEventLoop) {
        self.password_archive = None;
        self.report_archive_error(e, event_loop);
    }

    /// Close the egui dialog window (if any). Dropping it scrubs an entered password.
    fn close_dialog(&mut self) {
        self.dialog = None;
    }

    /// Prompt for an archive's password (or re-prompt after a wrong one). Remembers
    /// `path` so a submitted password re-opens it. On the first prompt a fresh
    /// Password dialog opens; on a retry (`wrong`) the existing dialog gets an inline
    /// "Incorrect password" error and a cleared field rather than a jarring re-open.
    fn prompt_archive_password(
        &mut self,
        path: PathBuf,
        wrong: bool,
        event_loop: &ActiveEventLoop,
    ) {
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
        let refresh = self.refresh_hz();
        let parent = self.active.as_ref().map(|a| a.window.clone());
        self.dialog = dialog::DialogWindow::open(
            dialog::DialogKind::Password,
            event_loop,
            refresh,
            &prompt,
            &self.settings,
            &self.keymap,
            parent.as_deref(),
        );
    }

    /// Surface an archive-open failure to the user via the egui message dialog
    /// (too-large / corrupt / password / OOM / empty), and log it.
    fn report_archive_error(
        &mut self,
        e: &archive::ArchiveOpenError,
        event_loop: &ActiveEventLoop,
    ) {
        let msg = e.user_message();
        eprintln!("PhotoBlaze: {msg}");
        self.open_message(&msg, event_loop);
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
    fn toggle_recursive(&mut self, event_loop: &ActiveEventLoop) {
        let Some(root) = self.scan_root.clone() else {
            return;
        };
        let recursive = !self.recursive;
        let cursor = self
            .displayed_item
            .and_then(|i| self.source.path(i))
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
        // `self.recursive` updates when the first batch bootstraps).
        let msg = if recursive {
            "Recursive folders: on"
        } else {
            "Recursive folders: off"
        };
        self.show_toast(msg, event_loop);
    }

    /// Start / stop the slideshow (task #23, the `S` key + View ▸ Slideshow). Starting
    /// resets the timer (`last_present = now`) so the first slide shows for a full
    /// interval before advancing; `about_to_wait` drives the auto-advance from there.
    fn toggle_slideshow(&mut self, event_loop: &ActiveEventLoop) {
        let on = self.slideshow.toggle();
        if on {
            self.last_present = Some(Instant::now());
        }
        self.show_toast(
            if on { "Slideshow" } else { "Slideshow Stopped" },
            event_loop,
        );
    }

    /// Change the slideshow interval by `steps` × 0.5s (the `[` / `]` keys: `-1`
    /// shortens, `+1` lengthens), clamped, and flash the new value (e.g. `2.0s`). The
    /// change applies live: the deadline is `last_present + interval`, so a running
    /// slideshow's current slide gets more / less remaining time immediately.
    fn adjust_slideshow(&mut self, steps: i32, event_loop: &ActiveEventLoop) {
        let interval = self.slideshow.adjust(steps);
        self.show_toast(&slideshow::format_interval(interval), event_loop);
    }

    /// Toggle macOS **native (Spaces) fullscreen** — the green-button / ⌃⌘F behavior —
    /// as a deliberate alternative to our borderless speed mode (F / ⌥⏎ / F11). winit's
    /// `Fullscreen::Borderless(None)` maps to AppKit's `toggleFullScreen:` on macOS.
    /// Driven from our "Enter Full Screen" menu item (⌃⌘F). The Enter/Exit label is kept
    /// in sync separately (`refresh_native_fullscreen_label`), reading the real window
    /// state — so it stays correct even for the green-button / gesture toggles.
    #[cfg(target_os = "macos")]
    fn toggle_native_fullscreen(&mut self) {
        let Some(window) = self.active.as_ref().map(|a| a.window.clone()) else {
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
    fn reconfigure_edr_for_display(&mut self, event_loop: &ActiveEventLoop) {
        let changed = match self.active.as_ref() {
            Some(a) if a.renderer.hdr_surface_wants_edr().is_some() => {
                let hr = hdr_surface::window_max_edr(&a.window);
                if (hr - self.last_edr_headroom).abs() > 0.01 {
                    // Different display HDR capability — re-poke the layer (colorspace
                    // + wantsEDR) for the new screen, then update the roll-off below.
                    hdr_surface::configure(&a.window);
                    Some(hr)
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(hr) = changed {
            if let Some(a) = self.active.as_mut() {
                a.renderer.set_edr_headroom(hr);
            }
            self.last_edr_headroom = hr;
            self.draw(event_loop);
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
        self.settings.fullscreen = !self.windowed;
        // Leaving windowed mode: snapshot where the window is now, so toggling back
        // (and the next launch) restore this spot rather than the OS default corner (#1).
        if !self.windowed {
            self.capture_windowed_geometry();
        }
        // Persist the new mode + remembered geometry together (one atomic write). An
        // explicit user action (the toggle), never the view path — privacy #2.
        self.geometry_save_at = None;
        self.settings.save();

        // Clone the window handle (an Arc) so the window can be driven while `self` is
        // still borrowed mutably below (the menu attach needs `&mut self`).
        let Some(window) = self.active.as_ref().map(|a| a.window.clone()) else {
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
            match self.windowed_restore(&rects) {
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
        let Some(a) = self.active.as_ref() else {
            return;
        };
        let Ok(pos) = a.window.outer_position() else {
            return;
        };
        let size = a.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.settings.window = Some(settings::WindowGeometry {
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
        let before = self.settings.window;
        self.capture_windowed_geometry();
        if self.settings.window != before {
            self.geometry_save_at = Some(Instant::now() + Duration::from_millis(500));
        }
    }

    /// The saved windowed geometry to restore, but only when enough of it still lands
    /// on one of `monitors` — else `None`, so a window saved on a now-disconnected or
    /// rearranged monitor opens at the default spot instead of off-screen (#1).
    fn windowed_restore(
        &self,
        monitors: &[(i32, i32, u32, u32)],
    ) -> Option<settings::WindowGeometry> {
        let g = self.settings.window?;
        settings::geometry_on_screen(
            g,
            monitors,
            settings::MIN_VISIBLE_W,
            settings::MIN_VISIBLE_H,
        )
        .then_some(g)
    }

    /// Build the native menu bar once (cross-platform; muda owns the OS handle).
    fn ensure_menu(&mut self) {
        if self.menu.is_none() {
            let built = menu::build_menu();
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
        let Some(item) = self.displayed_item else {
            return false;
        };
        let rotated = self
            .rotations
            .get(&item)
            .is_some_and(|r| *r != Rotation::default());
        rotated
            && self
                .source
                .path(item)
                .is_some_and(save_rotation::is_orientation_writable)
    }

    /// Sync the "Save Rotation" menu item's enabled state to [`can_save_rotation`].
    /// Cheap + idempotent (skips the Win32 call when unchanged), so it's safe to call
    /// from the per-tick `about_to_wait`.
    fn refresh_save_menu_item(&mut self) {
        let want = self.can_save_rotation();
        if want != self.save_enabled {
            if let Some(it) = self.save_rotation_item.as_ref() {
                it.set_enabled(want);
            }
            self.save_enabled = want;
        }
    }

    /// Enable File ▸ Stop Scanning only while a folder scan is streaming in. Cheap + cached
    /// (skips the OS call when unchanged), so it's safe to call from the per-tick
    /// `about_to_wait` alongside [`refresh_save_menu_item`](App::refresh_save_menu_item).
    fn refresh_cancel_scan_menu_item(&mut self) {
        let want = self.dir_scan.is_some();
        if want != self.cancel_scan_enabled {
            if let Some(it) = self.cancel_scan_item.as_ref() {
                it.set_enabled(want);
            }
            self.cancel_scan_enabled = want;
        }
    }

    /// Mirror the top of the undo stack onto the Edit ▸ Undo item: disabled + plain
    /// "Undo" when empty, otherwise enabled with a title naming the next undo (e.g.
    /// "Undo Save Rotation"). On Windows the `\tCtrl+Z` accelerator hint is appended to
    /// the label (macOS shows the real ⌘Z key-equivalent the item already carries).
    /// Cheap + cached, so it's safe to call from the per-tick `about_to_wait`.
    fn refresh_undo_menu_item(&mut self) {
        // `None` = nothing to undo (disabled); `Some(label)` = enabled with that label.
        let top = self.undo_stack.last().map(UndoAction::menu_label);
        if self.undo_menu_state == Some(top) {
            return;
        }
        if let Some(it) = self.undo_item.as_ref() {
            let base = top.unwrap_or("Undo");
            #[cfg(target_os = "macos")]
            it.set_text(base);
            #[cfg(not(target_os = "macos"))]
            it.set_text(format!("{base}\tCtrl+Z"));
            it.set_enabled(top.is_some());
        }
        self.undo_menu_state = Some(top);
    }

    /// Mirror the live view state onto the View-menu checkmarks: scale mode (one of
    /// Fit / Crop to Fill / Original checked), Recursive Folders, and Fullscreen.
    /// Cheap + cached (skips the Win32 calls when nothing changed), so it's safe to
    /// call from the per-tick `about_to_wait` alongside [`refresh_save_menu_item`].
    fn refresh_view_menu_checks(&mut self) {
        let Some(c) = self.view_checks.as_ref() else {
            return;
        };
        // `windowed` is the inverse of fullscreen.
        let state = (
            self.view.mode,
            self.recursive,
            !self.windowed,
            self.slideshow.on,
            self.info,
        );
        if self.view_checks_state == Some(state) {
            return;
        }
        let (mode, recursive, fullscreen, slideshow, info) = state;
        c.fit.set_checked(mode == ScaleMode::Fit);
        c.fill.set_checked(mode == ScaleMode::Fill);
        c.original.set_checked(mode == ScaleMode::Original);
        c.recursive.set_checked(recursive);
        c.fullscreen.set_checked(fullscreen);
        c.slideshow.set_checked(slideshow);
        c.info.set_checked(info == InfoMode::Basic);
        c.full_exif.set_checked(info == InfoMode::Full);
        self.view_checks_state = Some(state);
    }

    /// macOS: flip the native (Spaces) fullscreen menu item's title between "Enter Full
    /// Screen" and "Exit Full Screen" to mirror the live state — the Mac-standard
    /// behavior (a title toggle, never a checkmark). Driven off the real
    /// `NSWindow.styleMask` ([`hdr_surface::window_is_fullscreen`]), not winit's
    /// `Window::fullscreen()` — the latter tracks the *requested* borderless mode and
    /// reads `None` even while `toggleFullScreen:` has us fullscreen, so it never flips.
    /// The styleMask is the OS truth however fullscreen was entered (our menu, ⌃⌘F, the
    /// green button, a Mission Control gesture). Cached, so the per-tick call is a no-op
    /// until it actually changes. (macOS's own auto-injected Globe/Fn+F fullscreen item
    /// won't update its label for us, which is why we manage our own.)
    #[cfg(target_os = "macos")]
    fn refresh_native_fullscreen_label(&mut self) {
        let Some(item) = self.native_fullscreen_item.as_ref() else {
            return;
        };
        let on = self
            .active
            .as_ref()
            .is_some_and(|a| hdr_surface::window_is_fullscreen(&a.window));
        if self.native_fullscreen_on == on {
            return;
        }
        item.set_text(if on {
            "Exit Full Screen"
        } else {
            "Enter Full Screen"
        });
        self.native_fullscreen_on = on;
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
            self.displayed_item
                .and_then(|i| self.source.path(i))
                .map(Path::to_path_buf)
        } else {
            None
        };
        if self.proxy_icon_path == want {
            return;
        }
        if let Some(a) = self.active.as_ref() {
            proxy_icon::set_represented_url(&a.window, want.as_deref());
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
        let Some(hwnd) = self.active.as_ref().and_then(|a| hwnd_of(&a.window)) else {
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
        if let Some(a) = self.active.as_ref() {
            if let (Some(menu), Some(hwnd)) = (self.menu.as_ref(), hwnd_of(&a.window)) {
                // SAFETY: the menu is attached to this live window's valid handle.
                unsafe {
                    let _ = menu.set_theme_for_hwnd(hwnd, muda::MenuTheme::Auto);
                }
            }
        }
    }

    /// Step the zoom by `factor` (menu Zoom In/Out — the keyboard zoom is the
    /// continuous hold-to-zoom). Multiplies the current zoom, clamps to the allowed
    /// range, and re-frames. `factor` > 1 zooms in, < 1 zooms out.
    fn zoom_step(&mut self, factor: f32, event_loop: &ActiveEventLoop) {
        self.view.zoom = (self.view.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.push_view();
        self.draw(event_loop);
    }

    /// Run a menu action by mapping it to the central [`Action`] and dispatching it —
    /// the menu and the keyboard share one dispatcher, so they can never drift. The
    /// id→`MenuAction` mapping is the pure, unit-tested `menu::action_for`.
    fn dispatch_menu(&mut self, action: MenuAction, event_loop: &ActiveEventLoop) {
        self.dispatch_action(action.to_action(), event_loop);
    }

    /// The single effect-half dispatcher for every [`Action`], shared by the keyboard
    /// (one-shot keys, via the keymap) and the menu. Navigation here is a single
    /// step (what the menu wants); the keyboard's held-to-fly nav and continuous
    /// pan/zoom are driven by the hold loop (`about_to_wait`), not this path.
    fn dispatch_action(&mut self, action: Action, event_loop: &ActiveEventLoop) {
        match action {
            Action::Next => self.advance(Nav::Forward, event_loop),
            Action::Prev => self.advance(Nav::Backward, event_loop),
            Action::Random => self.advance(Nav::Random, event_loop),
            Action::RandomPrev => self.advance(Nav::RandomPrev, event_loop),
            // Pan is continuous-while-held only (the hold loop); never single-dispatched.
            Action::PanLeft | Action::PanRight | Action::PanUp | Action::PanDown => {}
            Action::ZoomIn => self.zoom_step(1.25, event_loop),
            Action::ZoomOut => self.zoom_step(0.8, event_loop),
            Action::ScaleFit => self.set_scale_mode(ScaleMode::Fit, event_loop),
            Action::ScaleFill => self.set_scale_mode(ScaleMode::Fill, event_loop),
            Action::ScaleOriginal => self.set_scale_mode(ScaleMode::Original, event_loop),
            Action::ToggleOriginal => {
                let next = if self.view.mode == ScaleMode::Original {
                    ScaleMode::Fit
                } else {
                    ScaleMode::Original
                };
                self.set_scale_mode(next, event_loop);
            }
            Action::RotateCw => self.rotate(false, event_loop),
            Action::RotateCcw => self.rotate(true, event_loop),
            Action::Copy => self.copy_image(event_loop),
            Action::CopyPath => self.copy_path(event_loop),
            Action::SaveRotation => self.save_rotation(event_loop),
            Action::Delete => self.delete_current(false, event_loop),
            Action::DeletePermanent => self.delete_current(true, event_loop),
            Action::Undo => self.undo(event_loop),
            Action::OpenFile => self.open_picker(false, event_loop),
            Action::OpenFolder => self.open_picker(true, event_loop),
            Action::Info => self.toggle_info(false, event_loop),
            Action::FullExif => self.toggle_info(true, event_loop),
            Action::Help => self.toggle_help(event_loop),
            Action::Fullscreen => self.toggle_fullscreen(),
            Action::Recursive => self.toggle_recursive(event_loop),
            Action::CancelScan => self.cancel_scan_command(event_loop),
            Action::SlideshowToggle => self.toggle_slideshow(event_loop),
            Action::SlideshowFaster => self.adjust_slideshow(-1, event_loop),
            Action::SlideshowSlower => self.adjust_slideshow(1, event_loop),
            Action::PlayPause => self.toggle_play_pause(event_loop),
            // A menu click is a single step; the keyboard's hold-to-scrub goes through
            // `frame_step_press` (the FrameStep press arm) instead.
            Action::FrameNext => self.frame_step(1, event_loop),
            Action::FramePrev => self.frame_step(-1, event_loop),
            Action::Settings => self.open_settings(event_loop),
            Action::About => self.open_about(event_loop),
            Action::Quit => self.begin_exit(event_loop),
        }
    }

    /// Show the native picker (`O` = file(s), `Shift+O` = folder) and open the
    /// result. Modal — it blocks the event loop while open, which is fine: the app
    /// isn't flying through photos with a dialog up.
    fn open_picker(&mut self, folder: bool, event_loop: &ActiveEventLoop) {
        let fallback = default_picker_dir();
        let mut start_dir = picker_start_dir(
            self.settings.picker_dir.as_deref(),
            self.source.container(),
            self.scan_root.as_deref(),
            &self.root,
            &fallback,
        );
        // If the chosen folder no longer exists (e.g. a pinned folder was deleted or
        // unmounted), use the safe default rather than letting the OS dialog surface its
        // own remembered last folder.
        if !start_dir.is_dir() {
            start_dir = fallback;
        }
        let input = if folder {
            rfd::FileDialog::new()
                .set_directory(&start_dir)
                .pick_folder()
                .map(LaunchInput::Directory)
        } else {
            // Offer archives alongside images in the default filter (opening a zip
            // to view its photos is the same use case), plus an All-files escape
            // hatch. The picked paths go through `classify_inputs` — like drag-drop
            // — so a single picked `.zip` opens as an archive instead of being
            // mistaken for one file inside its folder.
            let mut exts: Vec<&str> = IMAGE_FILTER_EXTS.to_vec();
            exts.push("zip");
            exts.push("7z");
            rfd::FileDialog::new()
                .add_filter("Images & archives", &exts)
                .add_filter("All files", &["*"])
                .set_directory(&start_dir)
                .pick_files()
                .filter(|ps| !ps.is_empty())
                .map(classify_inputs)
        };
        // The modal picker ran its own message loop; the Esc (or Enter) used to
        // dismiss it can leak to our window as a stray key event. Drop any keys it
        // left "held", and guard Esc-to-quit briefly so cancelling the picker never
        // closes PhotoBlaze.
        self.held.clear();
        self.esc_guard_until = Some(Instant::now() + Duration::from_millis(300));
        if let Some(input) = input {
            self.open_input(input, event_loop);
        }
    }

    /// Replace the playlist with a new source and re-show at `start`. Every bit
    /// of index-keyed state (per-item rotation overrides, the metadata cache, the
    /// failed set, the resident ring) is dropped because the indices are
    /// reassigned; the geometry-epoch bump discards any in-flight decode for the
    /// old set.
    fn rebuild_playlist(
        &mut self,
        source: Arc<dyn PhotoSource>,
        root: PathBuf,
        scan_root: Option<PathBuf>,
        recursive: bool,
        start: usize,
        event_loop: &ActiveEventLoop,
    ) {
        if source.is_empty() {
            return;
        }
        let start = start.min(source.len() - 1);
        self.pending_delete = None; // any rebuild supersedes a deferred delete-advance
        self.stop_playback(); // a new source drops any playback of the old one (#2)
        self.source = source;
        self.root = root;
        self.scan_root = scan_root;
        self.recursive = recursive;
        self.playlist = Playlist::new(self.source.len(), 0).with_cursor(start);
        // Indices are reassigned — drop everything keyed by item index.
        self.rotations.clear();
        self.meta_cache.clear();
        self.failed.clear();
        self.preview_resident.clear();
        self.upgrade_done.clear();
        self.last_upgrade_set.clear();
        // Undo entries reference the old source's indices/paths — drop them too.
        self.undo_stack.clear();
        // Invalidate the ring + bump the epoch (discards in-flight old decodes),
        // then synchronously show the new current photo and refill around it.
        self.invalidate_geometry();
        self.displayed_item = self.playlist.current();
        self.target_item = self.playlist.current();
        self.load_current_sync(event_loop);
        self.request_prefetch();
        if let Some(a) = self.active.as_ref() {
            a.window.request_redraw();
        }
    }

    /// Grow the playlist in place as a streaming scan delivers more images: swap in the
    /// larger snapshot and extend the cursor's universe **without** resetting the displayed
    /// photo, the cursor, the resident ring, or any per-image cache. The contrast with
    /// [`rebuild_playlist`](App::rebuild_playlist) is the whole point — a fresh open nukes
    /// everything; a *grow* keeps it, because indices are append-only (index `i` is still
    /// the same photo). New neighbours become decodable, so we re-issue prefetch (still the
    /// scanning, anti-thrash variant — the scan isn't done yet), and the title's "X / N"
    /// total ticks up. A no-op if the snapshot isn't actually larger.
    fn extend_playlist(&mut self, source: Arc<dyn PhotoSource>) {
        let new_len = source.len();
        if new_len <= self.source.len() {
            return;
        }
        self.source = source;
        self.playlist.extend(new_len);
        self.request_prefetch();
        self.refresh_title();
    }

    /// Filter a streamed snapshot through the delete-tombstone set, rebuilding its `FsSource`
    /// without the deleted paths. Returns the snapshot **unchanged** (no allocation) in the
    /// common case where nothing was deleted mid-scan. Because the walk is append-only and we
    /// remove the *same* paths from every snapshot, the filtered result stays a prefix-superset
    /// of the current playlist — so the in-place [`extend_playlist`](App::extend_playlist) is
    /// still valid (the displayed photo's index doesn't shift). O(N) only when tombstones
    /// exist (rare).
    fn filter_deleted(&self, r: Resolved) -> Resolved {
        if self.deleted.is_empty() {
            return r;
        }
        let paths: Vec<PathBuf> = (0..r.source.len())
            .filter_map(|i| r.source.path(i).map(Path::to_path_buf))
            .filter(|p| !self.deleted.contains(p))
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

    /// Re-set the window title for the currently displayed photo (e.g. after a streaming
    /// grow bumps the "X / N" total). No-op if nothing is displayed.
    fn refresh_title(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        if item >= self.source.len() {
            return;
        }
        let title = title_for(self.source.name(item), item, self.source.len());
        if let Some(a) = self.active.as_ref() {
            a.window.set_title(&title);
        }
    }

    /// Push the current view transform to the renderer (re-places the quad).
    fn push_view(&mut self) {
        let view = self.view;
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_view(view);
        }
    }

    /// Decode the first image at the display size for an instant first frame.
    /// Returns `(pixels, w, h, color, hdr, peak, title)`.
    fn initial_image(
        &mut self,
    ) -> (
        Vec<u8>,
        u32,
        u32,
        pb_render::ColorTransform,
        bool,
        f32,
        String,
    ) {
        let srgb = pb_render::ColorTransform::srgb();
        match self.playlist.current() {
            // Preview-first (see `load_current_sync`): this runs synchronously while the
            // window is hidden during setup, so it grabs the fast embedded preview for
            // RAW/HEIC; the pool upgrades it to full once `resumed` kicks off prefetch.
            Some(idx) => match decode_item(self.source.as_ref(), idx, self.decode_fit(), true) {
                Ok(img) => {
                    let meta = meta_for(self.source.as_ref(), idx, &self.root, &img);
                    self.current = Some(meta.clone());
                    self.meta_cache.insert(idx, meta);
                    let title = title_for(self.source.name(idx), idx, self.source.len());
                    let (w, h, hdr, peak) = (img.width, img.height, is_hdr(&img), img.peak);
                    let color = render_color(&img.color);
                    (img.pixels, w, h, color, hdr, peak, title)
                }
                Err(e) => {
                    eprintln!("decode failed: {}: {e}", self.source.name(idx));
                    self.current = None;
                    let p = test_pattern(1600, 1000);
                    (
                        p,
                        1600,
                        1000,
                        srgb,
                        false,
                        1.0,
                        "PhotoBlaze (decode error)".to_string(),
                    )
                }
            },
            None => {
                // No images: hand the renderer a 1×1 dummy just to construct, then the
                // caller blanks it (`clear_image`) and shows the "Press O to open" hint.
                self.current = None;
                (
                    vec![0, 0, 0, 255],
                    1,
                    1,
                    srgb,
                    false,
                    1.0,
                    "PhotoBlaze".to_string(),
                )
            }
        }
    }

    /// Toggle an info panel: the one-line basic panel with `i`, or the full-EXIF
    /// "nerd" table with `Shift+I`. Selecting the mode that's already showing hides
    /// it. When shown it appears immediately (idle); after navigation it reappears
    /// once you stop (see `about_to_wait`).
    fn toggle_info(&mut self, full: bool, event_loop: &ActiveEventLoop) {
        let target = if full {
            InfoMode::Full
        } else {
            InfoMode::Basic
        };
        self.info = if self.info == target {
            InfoMode::Off
        } else {
            target
        };
        if self.info == InfoMode::Off {
            self.hide_overlay(event_loop);
        } else {
            self.show_overlay(event_loop);
        }
    }

    /// Toggle the keybindings help overlay (`/` or `?`). Shares the single overlay
    /// with the info panels, so it replaces whichever was showing.
    fn toggle_help(&mut self, event_loop: &ActiveEventLoop) {
        self.info = if self.info == InfoMode::Help {
            InfoMode::Off
        } else {
            InfoMode::Help
        };
        if self.info == InfoMode::Off {
            self.hide_overlay(event_loop);
        } else {
            self.show_overlay(event_loop);
        }
    }

    /// Open the "About PhotoBlaze" dialog (Help menu) — an egui window with the app
    /// icon + version, dark-mode-aware (see `dialog`).
    fn open_about(&mut self, event_loop: &ActiveEventLoop) {
        self.open_dialog(dialog::DialogKind::About, event_loop);
    }

    /// Open the Settings dialog (Ctrl+,) — an egui window seeded from the live
    /// settings; **Save** routes back to [`apply_settings`](Self::apply_settings).
    fn open_settings(&mut self, event_loop: &ActiveEventLoop) {
        self.open_dialog(dialog::DialogKind::Settings, event_loop);
    }

    /// Apply the settings the user saved in the dialog: swap in the new model, apply
    /// the parts that aren't read live (hold delay, letterbox color, default scale
    /// mode), then persist to disk (an explicit user action — privacy #2). The nav-feel
    /// rates (start speed / ramp / max) and the info-panel opacity are read live, so
    /// swapping `self.settings` is enough for those.
    fn apply_settings(&mut self, new: settings::Settings, event_loop: &ActiveEventLoop) {
        let old = std::mem::replace(&mut self.settings, new);
        let s = &self.settings;

        // Held-key repeat delay is cached on the struct (the curve below reads the
        // rates live, but this one is a Duration captured at construction).
        self.initial_delay = Duration::from_millis(s.hold_delay_ms as u64);

        // Default slideshow interval → the live timer. A running slideshow's deadline is
        // `last_present + interval`, recomputed each tick, so this takes effect at once
        // (the `[`/`]` live override is just a different write to the same field).
        self.slideshow.interval = Duration::from_secs_f64(s.slideshow_interval_secs);

        // Letterbox / background fill → renderer (takes effect on the next draw).
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_letterbox(s.letterbox);
        }

        // Default scale mode: apply live if it changed (re-frames + reloads at the new
        // fit). `set_scale_mode` redraws for us.
        let scale_changed = old.scale_mode != s.scale_mode;
        if scale_changed {
            self.set_scale_mode(scale_mode_of(s.scale_mode), event_loop);
        }

        // Persist the whole model (atomic write; best-effort).
        self.settings.save();

        // Redraw so the new letterbox shows even when the scale mode didn't change,
        // and rebuild the info panel so a new opacity takes effect immediately.
        if self.overlay_shown {
            self.show_overlay(event_loop);
        } else if !scale_changed {
            self.draw(event_loop);
        }
    }

    /// Apply the keymap edited in the Settings dialog: swap it in live (every keypress
    /// resolves through `self.keymap`, so future input uses it immediately) and persist
    /// `keymap.toml`. If the help overlay is open, rebuild it so its key labels — read
    /// from the live keymap — reflect the new bindings.
    fn apply_keymap(&mut self, keymap: Keymap, event_loop: &ActiveEventLoop) {
        self.keymap = keymap;
        self.keymap.save();
        if self.overlay_shown && self.info == InfoMode::Help {
            self.show_overlay(event_loop);
        }
    }

    /// Open (or focus, if already open) one of our egui dialog windows. Only one
    /// dialog is shown at a time; requesting a different kind replaces it.
    fn open_dialog(&mut self, kind: dialog::DialogKind, event_loop: &ActiveEventLoop) {
        if let Some(d) = self.dialog.as_ref() {
            if d.kind() == kind {
                d.focus();
                return;
            }
        }
        let refresh = self.refresh_hz();
        let parent = self.active.as_ref().map(|a| a.window.clone());
        self.dialog = dialog::DialogWindow::open(
            kind,
            event_loop,
            refresh,
            "",
            &self.settings,
            &self.keymap,
            parent.as_deref(),
        );
    }

    /// Refresh rate in Hz (rounded, ≥1) — caps the Settings fly-speed slider and is
    /// passed to every dialog window.
    fn refresh_hz(&self) -> u32 {
        (1.0 / self.frame_interval.as_secs_f32()).round().max(1.0) as u32
    }

    /// Open the themed (dark-aware egui) "Delete Permanently" confirmation for `name`.
    /// The actual deletion happens when the dialog answers Yes (see `dialog_event`),
    /// acting on `pending_confirm_delete`.
    fn open_confirm_delete(&mut self, name: &str, event_loop: &ActiveEventLoop) {
        let refresh = self.refresh_hz();
        let msg = format!("Permanently delete \u{2018}{name}\u{2019}?");
        let parent = self.active.as_ref().map(|a| a.window.clone());
        self.dialog = dialog::DialogWindow::open(
            dialog::DialogKind::Confirm,
            event_loop,
            refresh,
            &msg,
            &self.settings,
            &self.keymap,
            parent.as_deref(),
        );
    }

    /// Open a one-button informational / error notice (egui `DialogKind::Message`):
    /// a warning icon + `message` + an OK button, centered over the viewer, closing
    /// on OK / Esc. The archive-open path (`archive::ArchiveOpenError::user_message`)
    /// calls this to surface a too-large / corrupt / password / OOM / empty failure.
    pub fn open_message(&mut self, message: &str, event_loop: &ActiveEventLoop) {
        let refresh = self.refresh_hz();
        let parent = self.active.as_ref().map(|a| a.window.clone());
        self.dialog = dialog::DialogWindow::open(
            dialog::DialogKind::Message,
            event_loop,
            refresh,
            message,
            &self.settings,
            &self.keymap,
            parent.as_deref(),
        );
    }

    /// Route an event for the dialog window (egui owns it). Esc / close button
    /// dismiss it; everything else feeds egui and triggers repaints.
    fn dialog_event(&mut self, event: WindowEvent, event_loop: &ActiveEventLoop) {
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
            // The Esc that dismisses a (focused) dialog also leaks to the main window
            // as a trailing/synthetic press once focus snaps back — by then `dialog`
            // is None, so the main-window guard can't catch it. Briefly guard
            // quit-on-Esc so closing a dialog never also exits the app (the same leak
            // `open_picker` handles).
            self.esc_guard_until = Some(Instant::now() + Duration::from_millis(300));
            // Esc / close on the loading view cancels the in-flight open (the worker
            // stops and frees its partial RAM); harmless for the other kinds.
            self.cancel_archive_load();
            // Esc / close on the scanning view cancels the in-flight folder walk and
            // discards its partial result. Guarded to the Scanning kind so closing a
            // *different* dialog doesn't kill a fast scan still running quietly in the
            // background (one dispatched <SCAN_DIALOG_DELAY ago, before any dialog).
            if self.dialog.as_ref().map(|d| d.kind()) == Some(dialog::DialogKind::Scanning) {
                self.cancel_dir_scan();
                self.dir_scan = None;
            }
            self.dialog = None;
            self.pending_confirm_delete = None; // Esc / close = cancel the confirm
            self.password_archive = None; // Esc / close = abandon the password prompt
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
            match kind {
                // The password dialog stays open until the open succeeds or is
                // cancelled (a wrong password re-prompts in place), so it isn't
                // closed here like the others.
                Some(dialog::DialogKind::Password) => {
                    if confirmed {
                        let pw = self
                            .dialog
                            .as_mut()
                            .and_then(|d| d.take_submitted_password());
                        match (pw, self.password_archive.clone()) {
                            (Some(pw), Some(path)) => {
                                // Show the "Checking…" state, then validate (zip is
                                // synchronous; a 7z re-opens off-thread).
                                if let Some(d) = self.dialog.as_mut() {
                                    d.set_checking(true);
                                    d.request_redraw();
                                }
                                self.begin_archive_open(path, Some(pw), event_loop);
                            }
                            // No archive pending (shouldn't happen): just close.
                            _ => self.dialog = None,
                        }
                    } else {
                        // Cancel: close and forget the pending archive.
                        self.dialog = None;
                        self.password_archive = None;
                    }
                }
                // Settings: Save applies + persists the edited model; Cancel/Esc
                // discard (Esc is handled by the `close` path above).
                Some(dialog::DialogKind::Settings) => {
                    let (new, new_keymap) = if confirmed {
                        let d = self.dialog.as_mut();
                        match d {
                            Some(d) => (d.take_settings_result(), d.take_keymap_result()),
                            None => (None, None),
                        }
                    } else {
                        (None, None)
                    };
                    self.dialog = None;
                    if let Some(new) = new {
                        self.apply_settings(new, event_loop);
                    }
                    if let Some(km) = new_keymap {
                        self.apply_keymap(km, event_loop);
                    }
                }
                // Loading: the only button is Cancel (which already requested
                // cancellation); make sure the in-flight open stops, then close. The
                // worker returns Cancelled and `poll_archive_load` tidies up.
                Some(dialog::DialogKind::Loading) => {
                    self.cancel_archive_load();
                    self.dialog = None;
                    self.password_archive = None;
                }
                // Scanning: the only button is Cancel (which already requested
                // cancellation); stop the walk, discard its partial result, and close —
                // a cancelled scan must keep the current view, not load a half-walked tree.
                Some(dialog::DialogKind::Scanning) => {
                    self.cancel_dir_scan();
                    self.dir_scan = None;
                    self.dialog = None;
                }
                // Confirm drives a delete; Message / others just close.
                other => {
                    self.dialog = None;
                    if other == Some(dialog::DialogKind::Confirm) {
                        let item = self.pending_confirm_delete.take();
                        if confirmed {
                            if let Some(item) = item {
                                if let Some(path) = self.source.path(item).map(Path::to_path_buf) {
                                    self.do_delete(item, &path, true, event_loop);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// The keybindings help table: a title row, then every hotkey → action as a
    /// shaded-key / description pair. The key labels are read from the live keymap
    /// (task #8 — single source of truth), so rebinding a key updates the help. A
    /// few rows stay curated: pan (shown as arrow glyphs), help (`/ or ?`), and the
    /// "hold to fly" hint (no single binding).
    fn help_rows(&self) -> Vec<Row> {
        // Primary keys for an action, numpad aliases dropped, joined by " / ".
        let keys = |a: Action| {
            self.keymap
                .bindings_for(a)
                .iter()
                .filter(|c| !keymap::is_numpad(c.code))
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(" / ")
        };
        // Two actions shown on one row (e.g. rotate cw / ccw).
        let two = |a: Action, b: Action| format!("{} / {}", keys(a), keys(b));

        let entries: Vec<(String, &str)> = vec![
            (keys(Action::Next), "Next photo"),
            (keys(Action::Prev), "Previous photo"),
            (keys(Action::Random), "Random photo (shuffle)"),
            (keys(Action::RandomPrev), "Previous random photo"),
            ("Hold nav key".to_string(), "Fly through photos"),
            (
                "\u{2190} \u{2191} \u{2193} \u{2192}".to_string(),
                "Pan (hold to accelerate)",
            ),
            (two(Action::ZoomIn, Action::ZoomOut), "Zoom in / out (hold)"),
            (keys(Action::ScaleFit), "Fit to screen"),
            (keys(Action::ScaleFill), "Fill screen (crop)"),
            (
                keys(Action::ToggleOriginal),
                "Toggle original 1:1 \u{2194} fit",
            ),
            (
                two(Action::RotateCw, Action::RotateCcw),
                "Rotate 90\u{b0} cw / ccw",
            ),
            (keys(Action::Recursive), "Recursive scan (current folder)"),
            (
                two(Action::OpenFile, Action::OpenFolder),
                "Open file(s) / folder",
            ),
            (keys(Action::Fullscreen), "Toggle fullscreen"),
            (
                two(Action::Info, Action::FullExif),
                "Info / full-EXIF panel",
            ),
            ("/ or ?".to_string(), "This help"),
            (keys(Action::Quit), "Quit"),
        ];
        let mut rows = vec![Row::Span {
            text: "PhotoBlaze Help".to_string(),
            bold: true,
        }];
        rows.extend(entries.into_iter().map(|(k, d)| Row::Pair {
            label: k,
            value: d.to_string(),
        }));
        rows
    }

    /// The full-EXIF "nerd" panel rows for the displayed photo: a filename/path
    /// header (spanning both columns), then a two-column table of dimensions,
    /// codec, exact byte size, and every EXIF tag. Read on-demand from RAM
    /// (privacy task #2: nothing cached to disk). Capped to fit the screen height.
    fn exif_rows(&self) -> Vec<Row> {
        let Some(item) = self.displayed_item else {
            return Vec::new();
        };
        let name = self.source.name(item);
        let mut rows = Vec::new();
        // Identity header: filename (bold) over its folder (the filename is already
        // shown above, so the path row is the parent directory only).
        rows.push(Row::Span {
            text: file_name_of(name),
            bold: true,
        });
        // Location row. A real file shows its on-disk folder. An archive entry
        // shows the archive's path, with the in-archive folder appended (after a
        // `›`) when the entry lives in a subfolder — so a zip's photos report
        // *where the zip is* plus *where inside it they are*.
        let location = match (self.source.path(item), self.source.container()) {
            (Some(p), _) => p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.display().to_string()),
            (None, Some(zip)) => {
                let inner = Path::new(name)
                    .parent()
                    .map(|d| d.to_string_lossy().replace('\\', "/"))
                    .filter(|s| !s.is_empty());
                Some(match inner {
                    Some(dir) => format!("{} › {}", zip.display(), dir),
                    None => zip.display().to_string(),
                })
            }
            (None, None) => Path::new(name)
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.to_string_lossy().replace('\\', "/")),
        };
        if let Some(location) = location {
            rows.push(Row::Span {
                text: location,
                bold: false,
            });
        }
        if let Some(meta) = &self.current {
            rows.push(Row::Pair {
                label: "Dimensions".to_string(),
                value: format!("{} × {}", meta.w, meta.h),
            });
            rows.push(Row::Pair {
                label: "Codec".to_string(),
                value: meta.codec.to_uppercase(),
            });
        }
        // Read the encoded bytes once (RAM-only, via the source) for both the exact
        // size and the EXIF fields — works the same for a file or an archive entry.
        if let Ok(bytes) = self.source.bytes(item) {
            rows.push(Row::Pair {
                label: "File Size".to_string(),
                value: format!("{} bytes", hud::format_thousands(bytes.len() as u64)),
            });
            for (tag, val) in read_exif_fields(&bytes) {
                // Skip binary blobs that render as meaningless hex (Apple
                // MakerNote/Padding are kilobytes long); truncate anything else
                // that's overlong so one field can't blow out the panel width.
                if is_exif_blob(&tag, &val) {
                    continue;
                }
                rows.push(Row::Pair {
                    label: tag,
                    value: truncate_exif_value(&val),
                });
            }
        }
        // Cap to what fits the screen height (~1.5x the font size per line).
        if let Some(fit) = self.fit {
            let line_h = ((15.0 * self.scale_factor).max(8.0) * 1.5).max(1.0);
            let max_rows = (((fit.max_height as f32) - 40.0) / line_h).max(1.0) as usize;
            if rows.len() > max_rows {
                rows.truncate(max_rows.saturating_sub(1));
                rows.push(Row::Span {
                    text: "…".to_string(),
                    bold: false,
                });
            }
        }
        rows
    }

    /// Corner inset (physical px) for the info/EXIF/help panel. Scales with the
    /// surface's short edge so a fixed gap doesn't look jammed against the corner on a
    /// huge fullscreen display (#3), with a DPI-scaled floor for small windows. Read
    /// fresh on every (re)show, so toggling between window sizes always re-spaces it.
    fn overlay_margin(&self) -> u32 {
        let short_edge = self
            .fit
            .map(|f| f.max_width.min(f.max_height))
            .unwrap_or(800) as f32;
        let floor = 10.0 * self.scale_factor;
        (short_edge * 0.015).max(floor).round().max(1.0) as u32
    }

    /// Rasterize the active overlay (info panel or help) and draw it. The help
    /// overlay uses a larger font than the info panels.
    fn show_overlay(&mut self, event_loop: &ActiveEventLoop) {
        let px = (15.0 * self.scale_factor).max(8.0);
        let pad = (7.0 * self.scale_factor).round().max(2.0) as u32;
        // The info / EXIF panels honor the user's opacity setting; the help overlay
        // keeps the standard translucency.
        let info_bg = hud::bg_for_opacity(self.settings.info_opacity);
        let panel = match self.info {
            InfoMode::Off => return,
            InfoMode::Basic => {
                let (Some(hud), Some(meta)) = (self.hud.as_ref(), self.current.as_ref()) else {
                    return;
                };
                let text = format!("{} · {}×{} · {}", meta.rel, meta.w, meta.h, meta.codec);
                hud.render_panel(&text, px, pad, info_bg)
            }
            InfoMode::Full => {
                let rows = self.exif_rows();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                if rows.is_empty() {
                    return;
                }
                hud.render_table(&rows, px, pad, info_bg)
            }
            InfoMode::Help => {
                let help_px = (20.0 * self.scale_factor).max(12.0);
                let rows = self.help_rows();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                hud.render_table(&rows, help_px, pad, hud::BG)
            }
        };
        let Some((bitmap, w, h)) = panel else {
            return;
        };
        let margin = self.overlay_margin();
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_overlay(Some((&bitmap, w, h)), margin);
        }
        self.overlay_shown = true;
        self.overlay_item = self.displayed_item;
        self.draw(event_loop);
    }

    /// Hide the info panel (clears the overlay quad).
    fn hide_overlay(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_overlay(None, 0);
        }
        self.overlay_shown = false;
        self.overlay_item = None;
        self.draw(event_loop);
    }

    /// Build the centered empty-state hint panel ("Press O to open a file / or
    /// Shift+O to open a folder"). `None` if no system font loaded. Returns an owned
    /// bitmap so callers can apply it to a renderer they still own (e.g. mid-setup).
    fn open_hint_panel(&self) -> Option<(Vec<u8>, u32, u32)> {
        let px = (20.0 * self.scale_factor).max(12.0);
        let pad = (10.0 * self.scale_factor).round().max(3.0) as u32;
        self.hud.as_ref()?.render_centered(
            &["Press O to open a file", "or Shift+O to open a folder"],
            px,
            pad,
            hud::BG,
        )
    }

    /// Show the empty-state hint over the (blank) viewer — used when there are no
    /// images to display. Rebuilt against the current scale; the renderer re-centers
    /// it on resize and drops it the moment a photo is shown.
    fn show_open_hint(&mut self) {
        // Suppress the hint while a folder scan is pending (deferred startup launch) or
        // streaming in — the first photo is about to bootstrap, so "Press O to open" would
        // flash briefly and misleads (it implies nothing is loading). If the scan turns out
        // empty, `poll_dir_scan`'s Done arm restores the hint.
        if self.dir_scan.is_some() || self.pending_launch.is_some() {
            return;
        }
        let Some((bitmap, w, h)) = self.open_hint_panel() else {
            return;
        };
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_message(Some((&bitmap, w, h)));
        }
    }

    /// Flash a transient status message at the bottom-center (tasks.json #10) — for
    /// commands that otherwise give no visual feedback, e.g. the recursion toggle.
    /// A new toast replaces any current one.
    fn show_toast(&mut self, msg: &str, event_loop: &ActiveEventLoop) {
        self.show_toast_icon(msg, None, event_loop);
    }

    /// Like [`show_toast`] but with an optional leading duotone icon (an SVG source
    /// from [`icon::assets`]) — e.g. the clipboard glyph on the Copy toast, or an
    /// icon-only pill (empty `msg`) for the rotate toasts. Always redraws, so a
    /// caller that also changed the view (e.g. `rotate`) renders even when there's
    /// no system font to build a toast from.
    fn show_toast_icon(&mut self, msg: &str, icon: Option<&str>, event_loop: &ActiveEventLoop) {
        let px = (26.0 * self.scale_factor).max(16.0);
        let pad = (12.0 * self.scale_factor).round().max(4.0) as u32;
        if let Some(hud) = self.hud.as_ref() {
            if let Some((rgba, w, h)) = hud.render_panel_icon(msg, px, pad, icon, hud::BG) {
                self.toast = Some(Toast {
                    rgba,
                    w,
                    h,
                    started: Instant::now(),
                    uploaded_alpha: -1.0,
                });
                self.push_toast(1.0);
            }
        }
        self.draw(event_loop);
    }

    /// Upload the current toast bitmap to the renderer at `alpha` (its alpha
    /// channel scaled), centered near the bottom.
    fn push_toast(&mut self, alpha: f32) {
        let (faded, w, h) = {
            let Some(t) = self.toast.as_mut() else {
                return;
            };
            t.uploaded_alpha = alpha;
            (scale_alpha(&t.rgba, alpha), t.w, t.h)
        };
        let margin = (64.0 * self.scale_factor).round().max(8.0) as u32;
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_toast(Some((&faded, w, h)), margin);
        }
    }

    /// Advance the toast's hold/fade and return whether one is still active (so the
    /// event loop keeps ticking). Re-uploads only on a meaningful alpha change;
    /// clears the layer once expired.
    fn tick_toast(&mut self, now: Instant, event_loop: &ActiveEventLoop) -> bool {
        let Some(alpha) = self.toast.as_ref().and_then(|t| t.alpha(now)) else {
            if self.toast.take().is_some() {
                if let Some(a) = self.active.as_mut() {
                    a.renderer.set_toast(None, 0);
                }
                self.draw(event_loop);
            }
            return false;
        };
        let changed = self
            .toast
            .as_ref()
            .is_some_and(|t| (alpha - t.uploaded_alpha).abs() > 0.02);
        if changed {
            self.push_toast(alpha);
            self.draw(event_loop);
        }
        true
    }

    /// The current keypress brighten-pulse intensity (0..=1), decaying to 0 over
    /// `PIE_GLOW_DUR` after the last dropped nav press.
    fn pie_glow(&self, now: Instant) -> f32 {
        match self.pie_glow_started {
            Some(t) => (1.0 - (now - t).as_secs_f32() / PIE_GLOW_DUR).clamp(0.0, 1.0),
            None => 0.0,
        }
    }

    /// Drive the top-right "not-ready" loading pie (#2). While the next photo is
    /// still decoding (a miss outlasting `PIE_SHOW_DELAY`), show a pie that eases
    /// asymptotically toward — but never reaches — full, on a time constant
    /// self-calibrated to how long misses usually take (`decode_ewma`). Once the
    /// photo lands, learn from the wait, then snap to full and fade. Returns
    /// whether the pie still needs the loop to keep ticking.
    fn tick_pie(&mut self, now: Instant, event_loop: &ActiveEventLoop) -> bool {
        let not_ready = self.target_item.is_some() && self.displayed_item != self.target_item;
        if not_ready {
            self.pie_finish = None;
            let start = *self.wait_started.get_or_insert(now);
            let elapsed = (now - start).as_secs_f32();
            if elapsed >= PIE_SHOW_DELAY {
                let tau = self.decode_ewma.max(PIE_TAU_MIN);
                // Asymptotic ease: ~half-full at one tau, approaching the cap but
                // never quite arriving (the deliberate, honest-ish "lie").
                let progress = (1.0 - 2f32.powf(-elapsed / tau)).min(PIE_FILL_CAP);
                let glow = self.pie_glow(now);
                self.push_pie(progress, glow, 1.0, event_loop);
            } else {
                self.clear_pie(event_loop);
            }
            return true; // keep ticking while we wait
        }
        // Caught up. If we were mid-wait, learn how long it took (so the estimate
        // tracks this machine + folder), and if the pie was up, play the finish.
        if let Some(start) = self.wait_started.take() {
            let waited = (now - start).as_secs_f32();
            self.decode_ewma = (self.decode_ewma * (1.0 - PIE_EWMA_ALPHA)
                + waited * PIE_EWMA_ALPHA)
                .clamp(PIE_TAU_MIN, 2.0);
            if self.pie_drawn {
                self.pie_finish = Some(now);
            }
        }
        if let Some(fstart) = self.pie_finish {
            let t = (now - fstart).as_secs_f32();
            if t < PIE_FINISH_FADE {
                let glow = self.pie_glow(now);
                self.push_pie(1.0, glow, 1.0 - t / PIE_FINISH_FADE, event_loop);
                return true;
            }
            self.pie_finish = None;
        }
        self.clear_pie(event_loop);
        false
    }

    /// Rasterize + upload the pie at `progress`/`glow`, scaled by a global `alpha`
    /// (the finish fade). Re-uploads + redraws only when the visible result
    /// changes (quantized), so the slow tail of the asymptote doesn't churn.
    fn push_pie(&mut self, progress: f32, glow: f32, alpha: f32, event_loop: &ActiveEventLoop) {
        let want = (progress, glow, alpha);
        let unchanged = self.pie_pushed.is_some_and(|(p, g, a)| {
            (p - progress).abs() < 0.01 && (g - glow).abs() < 0.04 && (a - alpha).abs() < 0.02
        });
        if unchanged && self.pie_drawn {
            return;
        }
        let diameter = (PIE_DIAMETER * self.scale_factor).round().max(12.0) as u32;
        let (mut rgba, w, h) = hud::render_pie(diameter, progress, glow);
        if alpha < 1.0 {
            rgba = scale_alpha(&rgba, alpha);
        }
        let margin = (PIE_MARGIN * self.scale_factor).round().max(4.0) as u32;
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_pie(Some((&rgba, w, h)), margin);
        }
        self.pie_drawn = true;
        self.pie_pushed = Some(want);
        self.draw(event_loop);
    }

    /// Clear the pie layer if it's up (and redraw to remove it).
    fn clear_pie(&mut self, event_loop: &ActiveEventLoop) {
        if self.pie_drawn {
            if let Some(a) = self.active.as_mut() {
                a.renderer.set_pie(None, 0);
            }
            self.pie_drawn = false;
            self.pie_pushed = None;
            self.draw(event_loop);
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
    fn tick_chip(&mut self, event_loop: &ActiveEventLoop) {
        let want = match (self.dir_scan.as_ref(), self.displayed_item) {
            (Some(scan), Some(_))
                if scan.bootstrapped && scan.started.elapsed() >= SCAN_DIALOG_DELAY =>
            {
                // Current folder being walked; hide it while it's just the root (it would
                // duplicate the heading).
                let cur = scan.progress.current();
                let path = if cur == scan.name { String::new() } else { cur };
                Some((scan.name.clone(), path, self.source.len()))
            }
            _ => None,
        };
        if want == self.chip_sig {
            return;
        }
        // Show/hide is immediate; a content tick (folder/count) is throttled so the software
        // composite stays off the hot path.
        let toggling = want.is_some() != self.chip_sig.is_some();
        if !toggling && self.chip_built.elapsed() < SCAN_CARD_REFRESH {
            return;
        }
        match &want {
            Some((name, path, count)) => self.push_chip(name, path, *count, event_loop),
            None => self.clear_chip(event_loop),
        }
        self.chip_sig = want;
        self.chip_built = Instant::now();
    }

    /// Rasterize the scan status card and place it at the top-right with equal top/right insets.
    /// Records the centered **Cancel Scan button's** physical-px rect (the only click target).
    fn push_chip(&mut self, name: &str, path: &str, count: usize, event_loop: &ActiveEventLoop) {
        let heading = format!("Scanning \u{201c}{name}\u{201d}");
        let noun = if count == 1 { "image" } else { "images" };
        let count_line = format!("{} {noun} found", hud::format_thousands(count as u64));
        // Equal inset from the top and right edges; fixed card width, clamped to the window.
        let margin = (PIE_MARGIN * self.scale_factor).round().max(4.0) as u32;
        let win_w = self
            .active
            .as_ref()
            .map(|a| a.window.inner_size().width)
            .unwrap_or(0);
        let width = ((SCAN_CARD_WIDTH * self.scale_factor).round())
            .min((win_w as f32 - 2.0 * margin as f32).max(1.0))
            .max(1.0) as u32;
        let card = self.hud.as_ref().and_then(|hud| {
            let px = (15.0 * self.scale_factor).max(10.0);
            hud.render_scan_card(
                &heading,
                path,
                &count_line,
                "Cancel Scan",
                icon::assets::STOP,
                px,
                width,
                hud::BG,
                self.chip_hovered,
            )
        });
        let Some((rgba, w, h, btn)) = card else {
            self.chip_rect = None;
            return;
        };
        if let Some(a) = self.active.as_ref() {
            // Card top-left in physical px (right edge inset by `margin`, top inset by `margin`),
            // then the button rect offset within it → the click hit-target.
            let card_x0 = a.window.inner_size().width as f32 - margin as f32 - w as f32;
            let card_y0 = margin as f32;
            let [bx, by, bw, bh] = btn.map(|v| v as f32);
            self.chip_rect = Some([
                card_x0 + bx,
                card_y0 + by,
                card_x0 + bx + bw,
                card_y0 + by + bh,
            ]);
        }
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_chip(Some((&rgba, w, h)), margin, margin);
        }
        self.draw(event_loop);
    }

    /// A monitor/DPI change re-scaled the window (e.g. dragging from a 1× display to a 2×
    /// Retina one): every CPU-rasterized overlay was baked at the old [`scale_factor`] and is
    /// cached by *content* (which didn't change), so force each to rebuild at the new DPI —
    /// otherwise the overlay text looks soft / wrong-sized on the new monitor. The photo and
    /// the info/EXIF panel are re-decoded / re-shown by the `Resized` → settle path that
    /// follows this event (using the now-updated scale); here we invalidate the per-tick
    /// overlays (the loading pie and the scan card) and the empty-state hint.
    ///
    /// [`scale_factor`]: App::scale_factor
    fn rescale_overlays(&mut self) {
        self.pie_pushed = None; // re-rasterize the loading pie at the new scale next tick
        self.chip_sig = None; // re-rasterize the scan card next tick
        if self.overlay_shown {
            self.overlay_item = None; // force the info/EXIF/help panel to re-show next tick
        }
        if self.source.is_empty() {
            self.show_open_hint(); // re-rasterize the "Press O to open" hint
        }
        if let Some(a) = self.active.as_ref() {
            a.window.request_redraw();
        }
    }

    /// Clear the scan card layer if it's up (and redraw to remove it).
    fn clear_chip(&mut self, event_loop: &ActiveEventLoop) {
        if self.chip_sig.is_some() {
            if let Some(a) = self.active.as_mut() {
                a.renderer.set_chip(None, 0, 0);
            }
            self.chip_rect = None;
            self.chip_hovered = false;
            self.draw(event_loop);
        }
    }

    /// Hit-test a physical-px cursor position against the scan card's **Cancel Scan button**
    /// rect. The reusable overlay-click primitive: store a rect when you draw an interactive
    /// overlay, test it here before the click falls through to drag-to-pan. (Future EXIF copy
    /// buttons will register their own rects the same way.)
    fn chip_hit(&self, x: f32, y: f32) -> bool {
        self.chip_rect.is_some_and(|rect| point_in_rect(rect, x, y))
    }

    /// Update the Cancel Scan button's hover "lit" state from the latest cursor position, and —
    /// only when hover **changes** — re-rasterize the card so the button lights up / dims. This
    /// runs on every cursor-move, but the rebuild fires just on the enter/leave transition (one
    /// ~320px CPU composite), never per move or per frame, so it stays off the photo hot path.
    fn update_chip_hover(&mut self, event_loop: &ActiveEventLoop) {
        let hovered = self.last_cursor.is_some_and(|[x, y]| self.chip_hit(x, y));
        if hovered == self.chip_hovered {
            return;
        }
        self.chip_hovered = hovered;
        // Re-render the card in the new hover state; its content (name/path/count) is unchanged,
        // so this bypasses the content throttle and feels instant.
        if let Some((name, path, count)) = self.chip_sig.clone() {
            self.push_chip(&name, &path, count, event_loop);
        }
    }

    /// Render one frame.
    fn draw(&mut self, event_loop: &ActiveEventLoop) {
        let t0 = Instant::now();
        let drew = if let Some(a) = self.active.as_mut() {
            if let Err(e) = a.renderer.render() {
                eprintln!("fatal render error: {e:?}");
                event_loop.exit();
            }
            a.renderer.poll();
            true
        } else {
            false
        };
        if drew {
            self.metrics.record("render", t0.elapsed());
        }
    }

    /// Esc / window-close: shut down with a perceived-*instant* exit, writing
    /// nothing to disk (tasks #6 + #2). Order matters:
    /// 1. Hide the window FIRST, so it vanishes before the heavy frees — the close
    ///    always feels instant regardless of how long teardown takes.
    /// 2. Drop the RAM-only, photo-derived session state (no disk flush — the only
    ///    persistent thing PhotoBlaze touches is the photos it *reads*).
    /// 3. Exit the loop; `run_app` returns and `Drop` then frees the renderer
    ///    (VRAM) and joins the decode pool — all while the window is already gone.
    fn begin_exit(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(a) = self.active.as_ref() {
            a.window.set_visible(false);
        }
        self.clear_session_state();
        event_loop.exit();
    }

    /// Drop every RAM-backed, photo-derived cache: decoded-pixel residency, staged
    /// uploads, per-item metadata, per-image rotation overrides, and the
    /// failed/transient overlay state. Pure in-memory clears — **never a disk
    /// write** (privacy task #2). `Drop` would reclaim all of this on its own; doing
    /// it explicitly at teardown keeps the privacy guarantee auditable in one place.
    fn clear_session_state(&mut self) {
        // Abandon a still-running background scan so it stops walking on teardown.
        self.cancel_dir_scan();
        self.ring = ResidentRing::new(0);
        self.pending_uploads.clear();
        self.meta_cache.clear();
        self.rotations.clear();
        self.failed.clear();
        self.preview_resident.clear();
        self.upgrade_done.clear();
        self.last_upgrade_set.clear();
        self.undo_stack.clear();
        self.current = None;
        self.toast = None;
        self.wait_started = None;
        self.pie_finish = None;
        self.pie_glow_started = None;
        // Drop any on-demand animation playback + in-flight decode (RAM-only — #2).
        self.stop_playback();
    }

    /// Handle a nav keypress (space / backspace / enter). Tracks the held key for
    /// hold-to-fly, then either advances, or — when we're still catching up to the
    /// previous target, so the press can't be serviced yet — flashes the loading
    /// pie (brighten-on-keypress) so the input never feels dead.
    fn nav_press(&mut self, code: KeyCode, action: Action, event_loop: &ActiveEventLoop) {
        self.held.insert(code, action);
        self.hold_start = Some(Instant::now());
        let Some(nav) = nav_of(action) else {
            return;
        };
        if self.target_item.is_some() && self.displayed_item != self.target_item {
            self.pie_glow_started = Some(Instant::now());
        } else {
            self.advance(nav, event_loop);
        }
    }

    /// Advance one photo (sequential or random). The gated engine path: present on
    /// a ring hit, else hold the previous frame + prefetch while the decode lands.
    fn advance(&mut self, nav: Nav, event_loop: &ActiveEventLoop) {
        // Settle a deferred delete-advance before navigating, so a keypress during the
        // brief post-delete delay lands cleanly on the rebuilt playlist (no yank-back).
        self.flush_pending_delete(event_loop);
        // Never advance while the previous target is still pending (a miss in
        // flight): a fast second press would overwrite it and skip that photo.
        // Holding still flies — `about_to_wait` re-advances once it's caught up.
        if self.displayed_item != self.target_item {
            return;
        }
        // Navigating away from an animated image stops playback and reverts to the
        // still (the frames are RAM-only — privacy #2). A no-op on a still.
        self.stop_playback();
        // Remember the direction so the slideshow auto-advances the way the user last
        // moved (manual nav during a slideshow steers it). The slideshow's own
        // `advance(self.last_nav)` calls are then idempotent here.
        self.last_nav = nav;
        match nav {
            Nav::Forward => self.playlist.next(),
            Nav::Backward => self.playlist.prev(),
            Nav::Random => self.playlist.random_next(),
            Nav::RandomPrev => self.playlist.random_prev(),
        }
        self.target_item = self.playlist.current();
        // Both modes use the async engine: present on a ring hit, else hold the
        // previous frame while the decode (fit-sized or full-res) lands.
        self.try_present_target(event_loop);
        self.request_prefetch();
    }

    /// Which way we're currently paging, from the held nav actions (ambiguous/none =
    /// idle). Next (forward), Prev (backward), and Random / RandomPrev advance; two
    /// keys bound to the *same* direction (e.g. Enter + NumpadEnter) still count as
    /// one, but two *different* nav directions held at once is treated as idle.
    fn held_nav(&self) -> Option<Nav> {
        let mut dir: Option<Nav> = None;
        for &action in self.held.values() {
            if let Some(n) = nav_of(action) {
                match dir {
                    None => dir = Some(n),
                    Some(d) if d == n => {}
                    Some(_) => return None, // two different directions → idle
                }
            }
        }
        dir
    }

    /// Zoom direction from the held actions: `+1` in ([`Action::ZoomIn`]), `-1` out
    /// ([`Action::ZoomOut`]), `None` if neither or both.
    fn zoom_held(&self) -> Option<f32> {
        let mut zin = false;
        let mut zout = false;
        for &action in self.held.values() {
            match action {
                Action::ZoomIn => zin = true,
                Action::ZoomOut => zout = true,
                _ => {}
            }
        }
        match (zin, zout) {
            (true, false) => Some(1.0),
            (false, true) => Some(-1.0),
            _ => None,
        }
    }

    /// Pan velocity direction from the held pan actions (image-space; positive pan
    /// reveals the right/bottom). Diagonals combine. `(0, 0)` if none held.
    fn pan_held(&self) -> (f32, f32) {
        let mut x = 0.0;
        let mut y = 0.0;
        for &action in self.held.values() {
            match action {
                Action::PanLeft => x += 1.0,
                Action::PanRight => x -= 1.0,
                Action::PanUp => y += 1.0,
                Action::PanDown => y -= 1.0,
                _ => {}
            }
        }
        (x, y)
    }

    /// The current image texture + screen dimensions for pan-clamp math.
    fn screen_and_image(&self) -> Option<(u32, u32, u32, u32)> {
        let fit = self.fit?;
        let (iw, ih) = self.active.as_ref()?.renderer.image_size();
        Some((iw, ih, fit.max_width, fit.max_height))
    }

    /// Whether the image currently overflows the viewport (so panning does
    /// something). Drives the grab-hand cursor affordance.
    fn pannable(&self) -> bool {
        self.screen_and_image()
            .map(|(iw, ih, sw, sh)| {
                let mp = self.view.max_pan(iw, ih, sw, sh);
                mp[0] > 0.0 || mp[1] > 0.0
            })
            .unwrap_or(false)
    }

    /// Reflect the pan affordance in the pointer: a pointing hand over the Cancel Scan button,
    /// a closed hand while dragging, an open hand when the image is pannable, the default arrow
    /// otherwise.
    fn refresh_cursor(&self) {
        if let Some(a) = self.active.as_ref() {
            let over_button = self.last_cursor.is_some_and(|[x, y]| self.chip_hit(x, y));
            let icon = if self.dragging {
                CursorIcon::Grabbing
            } else if over_button {
                CursorIcon::Pointer
            } else if self.pannable() {
                CursorIcon::Grab
            } else {
                CursorIcon::Default
            };
            a.window.set_cursor(icon);
        }
    }

    /// Zoom by `factor` (>1 in, <1 out) about the cursor — the shared effect for
    /// trackpad pinch and mouse-wheel zoom. Anchors on the last cursor position,
    /// falling back to the screen center before the pointer has moved.
    fn zoom_about_cursor(&mut self, factor: f32, event_loop: &ActiveEventLoop) {
        let Some((iw, ih, sw, sh)) = self.screen_and_image() else {
            return;
        };
        let anchor = self
            .last_cursor
            .unwrap_or([sw as f32 / 2.0, sh as f32 / 2.0]);
        self.view.zoom_about(factor, anchor, iw, ih, sw, sh);
        self.push_view();
        self.draw(event_loop);
        // Zooming changes whether the image overflows — update the grab affordance
        // immediately (the pointer may not move after a wheel notch / pinch).
        self.refresh_cursor();
    }

    /// Pan by a raw pixel delta (trackpad two-finger swipe), clamped to the image
    /// bounds. No effect when the image fits within the screen (nothing to pan).
    fn pan_by_pixels(&mut self, dx: f32, dy: f32, event_loop: &ActiveEventLoop) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.view.pan[0] += dx;
        self.view.pan[1] += dy;
        if let Some((iw, ih, sw, sh)) = self.screen_and_image() {
            let mp = self.view.max_pan(iw, ih, sw, sh);
            self.view.pan[0] = self.view.pan[0].clamp(-mp[0], mp[0]);
            self.view.pan[1] = self.view.pan[1].clamp(-mp[1], mp[1]);
        }
        self.push_view();
        self.draw(event_loop);
    }

    /// Apply continuous zoom/pan while their keys are held, with a time-based
    /// acceleration ramp (gentle start for fine tuning, faster the longer held).
    /// Returns whether anything changed (so the loop keeps polling + redrawing).
    fn apply_view_holds(&mut self, now: Instant, event_loop: &ActiveEventLoop) -> bool {
        let mut changed = false;

        match self.zoom_held() {
            Some(dir) => {
                let start = *self.zoom_started.get_or_insert(now);
                let last = self.zoom_last.replace(now).unwrap_or(start);
                let dt = (now - last).as_secs_f32().min(0.1);
                let t = (now - start).as_secs_f32();
                let rate =
                    ZOOM_MIN_RATE + (ZOOM_MAX_RATE - ZOOM_MIN_RATE) * (t / ZOOM_RAMP_SECS).min(1.0);
                // Exponential (multiplicative) zoom about the screen center.
                self.view.zoom =
                    (self.view.zoom * (rate * dir * dt).exp()).clamp(MIN_ZOOM, MAX_ZOOM);
                changed = true;
            }
            None => {
                self.zoom_started = None;
                self.zoom_last = None;
            }
        }

        let (px, py) = self.pan_held();
        if px != 0.0 || py != 0.0 {
            let start = *self.pan_started.get_or_insert(now);
            let last = self.pan_last.replace(now).unwrap_or(start);
            let dt = (now - last).as_secs_f32().min(0.1);
            let t = (now - start).as_secs_f32();
            let speed =
                PAN_MIN_SPEED + (PAN_MAX_SPEED - PAN_MIN_SPEED) * (t / PAN_RAMP_SECS).min(1.0);
            self.view.pan[0] += px * speed * dt;
            self.view.pan[1] += py * speed * dt;
            if let Some((iw, ih, sw, sh)) = self.screen_and_image() {
                let mp = self.view.max_pan(iw, ih, sw, sh);
                self.view.pan[0] = self.view.pan[0].clamp(-mp[0], mp[0]);
                self.view.pan[1] = self.view.pan[1].clamp(-mp[1], mp[1]);
            }
            changed = true;
        } else {
            self.pan_started = None;
            self.pan_last = None;
        }

        if changed {
            self.push_view();
            self.draw(event_loop);
        }
        changed
    }

    /// Whether prefetch/upload work is still outstanding (keep polling if so).
    fn work_pending(&self) -> bool {
        self.archive_load.is_some()
            // An off-thread animation decode in flight keeps the loop polling so
            // `poll_anim_decode` picks it up promptly (active playback drives its own
            // precise next-frame wake via `tick_playback`, not this frame poll).
            || self.anim_decode.is_some()
            || self.displayed_item != self.target_item
            || self
                .targets
                .iter()
                .any(|&t| self.ring.slot_for(t).is_none() && !self.failed.contains(&t))
    }

    // --- Animation playback (task #37) -------------------------------------------------

    /// `P`: play/pause the current animation. If it isn't decoded yet (and the photo
    /// is animated), kick the off-thread decode and start playing when it lands. On a
    /// still, `P` does nothing.
    fn toggle_play_pause(&mut self, event_loop: &ActiveEventLoop) {
        if self.playback.is_some() {
            let playing = self.playback.as_mut().unwrap().toggle_play();
            if playing {
                // (Re)started — present the current frame (frame 0 when replaying a
                // finished loop, so the stale last frame doesn't linger) + anchor timing.
                self.present_anim_frame(event_loop);
            } else {
                self.draw(event_loop); // paused — just redraw the held frame
            }
            return;
        }
        if self.anim_decode.is_some() {
            return; // a decode is already on its way (it'll autoplay)
        }
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.current_is_animated(item) {
            self.start_animation_decode(item, true);
        }
    }

    /// Step the current animation one frame (`delta`: `+1` next, `-1` previous),
    /// pausing playback. If not decoded yet (and animated), kick a paused decode so
    /// the held-key scrub can step once frames are ready. A no-op on a still.
    fn frame_step(&mut self, delta: i32, event_loop: &ActiveEventLoop) {
        if self.playback.is_some() {
            self.playback.as_mut().unwrap().step(delta);
            self.present_anim_frame(event_loop);
            return;
        }
        if self.anim_decode.is_some() {
            return;
        }
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.current_is_animated(item) {
            self.start_animation_decode(item, false);
        }
    }

    /// Keyboard frame-step press: track the key for hold-to-scrub, then step once now.
    fn frame_step_press(&mut self, code: KeyCode, action: Action, event_loop: &ActiveEventLoop) {
        self.held.insert(code, action);
        let now = Instant::now();
        self.framestep_started = Some(now);
        self.framestep_last = Some(now);
        self.frame_step(frame_step_dir(action), event_loop);
    }

    /// Whether item `item` is an animated container (from the cached header sniff).
    fn current_is_animated(&self, item: usize) -> bool {
        self.meta_cache
            .get(&item)
            .and_then(|m| m.animated)
            .is_some()
    }

    /// Kick the whole-sequence decode for `item` on a worker thread so a big GIF/WebP
    /// never stalls the event loop; the still first frame stays on screen until it
    /// lands (picked up by `poll_anim_decode`). `autoplay` starts it playing (`P`) vs.
    /// paused (frame-step).
    fn start_animation_decode(&mut self, item: usize, autoplay: bool) {
        self.anim_gen += 1;
        let gen = self.anim_gen;
        let epoch = self.epoch;
        let source = Arc::clone(&self.source);
        let fit = self.decode_fit();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = match source.bytes(item) {
                Ok(bytes) => pb_decode::decode_animation(&bytes, fit),
                Err(e) => Err(DecodeError::Corrupt(format!("read error: {e}"))),
            };
            let _ = tx.send(result);
        });
        self.anim_decode = Some(AnimDecode {
            gen,
            item,
            epoch,
            autoplay,
            rx,
        });
        // The user has engaged — don't also nag with the "▶ P" hint.
        self.anim_hint_shown_for = self.displayed_item;
    }

    /// Pick up a finished off-thread animation decode (called each `about_to_wait`).
    /// Discards a stale result (superseded request, geometry change, or the user
    /// navigated away) and otherwise installs the [`Playback`] and shows frame 0.
    fn poll_anim_decode(&mut self, event_loop: &ActiveEventLoop) {
        use std::sync::mpsc::TryRecvError;
        // Receive (and copy out what we need) in a scope so the `anim_decode` borrow
        // ends before we mutate it / install the playback.
        let outcome = {
            let Some(d) = self.anim_decode.as_ref() else {
                return;
            };
            match d.rx.try_recv() {
                Ok(result) => Some((d.gen, d.epoch, d.item, d.autoplay, result)),
                Err(TryRecvError::Empty) => return, // still decoding
                Err(TryRecvError::Disconnected) => None, // worker died
            }
        };
        self.anim_decode = None;
        let Some((gen, epoch, item, autoplay, result)) = outcome else {
            return;
        };
        // Stale: a newer request superseded it, the fit changed, or we moved on.
        if gen != self.anim_gen || epoch != self.epoch || self.displayed_item != Some(item) {
            return;
        }
        match result {
            Ok(anim) => {
                let truncated = anim.truncated;
                self.playback = Some(Playback::new(anim, autoplay));
                self.present_anim_frame(event_loop);
                if truncated {
                    self.show_toast("Animation truncated", event_loop);
                }
            }
            Err(e) => {
                eprintln!("animation decode failed for item {item}: {e}");
                self.show_toast("Can't play this animation", event_loop);
            }
        }
    }

    /// Upload the current animation frame and redraw (the playback present path —
    /// `set_image`, never the prefetch ring). Resets the per-frame deadline anchor.
    fn present_anim_frame(&mut self, event_loop: &ActiveEventLoop) {
        {
            let Some(pb) = self.playback.as_ref() else {
                return;
            };
            let color = render_color(&pb.color());
            let frame = pb.current_frame();
            if let Some(a) = self.active.as_mut() {
                a.renderer
                    .set_image(&frame.rgba, frame.width, frame.height, color, false, 1.0);
            }
        }
        self.anim_frame_shown_at = Some(Instant::now());
        self.draw(event_loop);
    }

    /// Advance playback to the due frame and return the next frame's wake deadline
    /// (None when not actively playing), so the loop sleeps exactly until then.
    fn tick_playback(&mut self, now: Instant, event_loop: &ActiveEventLoop) -> Option<Instant> {
        let shown = self.anim_frame_shown_at;
        let due = self.playback.as_ref().is_some_and(|pb| {
            let since = shown
                .map(|t| now.saturating_duration_since(t))
                .unwrap_or(Duration::ZERO);
            pb.is_due(since)
        });
        if due {
            self.playback.as_mut().unwrap().advance();
            self.present_anim_frame(event_loop); // updates anim_frame_shown_at + draws
        }
        let shown = self.anim_frame_shown_at;
        self.playback
            .as_ref()
            .filter(|pb| pb.is_playing())
            .map(|pb| shown.unwrap_or(now) + pb.current_delay())
    }

    /// Drive the held-key frame-step scrub (`,`/`.`). Returns whether a frame-step key
    /// is held (so the loop keeps polling). One step on press, then repeats at
    /// [`FRAME_STEP_REPEAT`] after the initial tap delay.
    fn tick_frame_step(&mut self, now: Instant, event_loop: &ActiveEventLoop) -> bool {
        let dir = self.held_frame_step();
        if dir == 0 {
            self.framestep_started = None;
            self.framestep_last = None;
            return false;
        }
        // Need a decoded sequence to scrub; while it's still decoding, keep ticking.
        if self.playback.is_none() {
            return true;
        }
        let past_delay = match self.framestep_started {
            Some(t) => now >= t + self.initial_delay,
            None => true,
        };
        let due = match self.framestep_last {
            Some(t) => now >= t + FRAME_STEP_REPEAT,
            None => true,
        };
        if past_delay && due {
            self.playback.as_mut().unwrap().step(dir);
            self.present_anim_frame(event_loop);
            self.framestep_last = Some(now);
        }
        true
    }

    /// The held frame-step direction: `+1` ([`Action::FrameNext`]) / `-1`
    /// ([`Action::FramePrev`]) / `0` if neither or both.
    fn held_frame_step(&self) -> i32 {
        let mut dir = 0i32;
        for &action in self.held.values() {
            match action {
                Action::FrameNext => dir += 1,
                Action::FramePrev => dir -= 1,
                _ => {}
            }
        }
        dir.signum()
    }

    /// Flash the "▶ Press P to play" hint once when settling on an animated still —
    /// suppressed while flying (the nag the owner flagged) and once playback/stepping
    /// has engaged.
    fn maybe_show_anim_hint(&mut self, flying: bool, event_loop: &ActiveEventLoop) {
        if flying || self.playback.is_some() || self.anim_decode.is_some() {
            return;
        }
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.anim_hint_shown_for == Some(item) {
            return;
        }
        if self.current_is_animated(item) {
            self.anim_hint_shown_for = Some(item);
            self.show_toast_icon("Press P to play", Some(icon::assets::PLAY), event_loop);
        }
    }

    /// Stop and drop any playback / in-flight decode, reverting to the still. Called
    /// when navigating away or changing source (the frames are RAM-only — privacy #2).
    fn stop_playback(&mut self) {
        self.playback = None;
        self.anim_frame_shown_at = None;
        self.anim_decode = None;
        self.framestep_started = None;
        self.framestep_last = None;
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_some() {
            return;
        }

        if let Some(hz) = event_loop
            .primary_monitor()
            .and_then(|m| m.refresh_rate_millihertz())
        {
            let hz = hz as f64 / 1000.0;
            println!("display refresh: {hz:.2} Hz");
            if hz > 0.0 {
                self.frame_interval = Duration::from_secs_f64(1.0 / hz);
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
            self.windowed_restore(&rects)
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

        self.scale_factor = window.scale_factor() as f32;
        let isz = window.inner_size();
        self.fit = Some(FitBox {
            max_width: isz.width.max(1),
            max_height: isz.height.max(1),
        });

        // Decode the first image at the display size while the window is hidden.
        let (rgba, iw, ih, color, hdr, peak, title) = self.initial_image();
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
        renderer.set_letterbox(self.settings.letterbox);
        let now = window.inner_size();
        if now != isz {
            self.fit = Some(FitBox {
                max_width: now.width.max(1),
                max_height: now.height.max(1),
            });
            renderer.resize(now.width, now.height);
            // The real window size differs from what we decoded for — re-decode
            // the first image at the corrected fit so the first frame isn't soft.
            if let Some(idx) = self.playlist.current() {
                let t0 = Instant::now();
                // Preview-first (see `load_current_sync`): the full decode lands off-thread.
                let decoded = decode_item(self.source.as_ref(), idx, self.decode_fit(), true);
                self.metrics.record("decode", t0.elapsed());
                if let Ok(img) = decoded {
                    let meta = meta_for(self.source.as_ref(), idx, &self.root, &img);
                    self.current = Some(meta.clone());
                    self.meta_cache.insert(idx, meta);
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

        // Empty launch (no folder/file given): show a blank background with the
        // centered "Press O to open…" hint instead of an image.
        if self.playlist.current().is_none() {
            renderer.clear_image();
            if let Some((bitmap, w, h)) = self.open_hint_panel() {
                renderer.set_message(Some((&bitmap, w, h)));
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
        let fit = self.fit.unwrap_or(FitBox {
            max_width: 1,
            max_height: 1,
        });
        let cap = ring_capacity(self.slot_bytes_estimate());
        self.ring = ResidentRing::new_with_budget(cap, RING_BUDGET_BYTES);
        renderer.reserve_ring(cap, fit.max_width, fit.max_height);
        let (ahead, behind) = window_for_capacity(cap);
        self.ahead = ahead;
        self.behind = behind;
        self.displayed_item = self.playlist.current();
        self.target_item = self.playlist.current();
        self.last_present = Some(Instant::now());

        self.active = Some(Active { window, renderer });
        self.request_prefetch();

        // Now that the window + engine are live, kick off any launch we deferred (an archive
        // or a folder scan): a big .7z loads behind the spinner, a folder streams in (window
        // shows first), and an encrypted / failed open can use the egui dialogs (a synchronous
        // launch resolve, before the event loop, could do none of these).
        if let Some(plan) = self.pending_launch.take() {
            self.open_plan(plan.source, plan.cursor, event_loop);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // Events for our egui dialog window go to egui, not the photo viewer.
        if self.dialog.as_ref().map(|d| d.id()) == Some(id) {
            self.dialog_event(event, event_loop);
            return;
        }
        match event {
            WindowEvent::CloseRequested => self.begin_exit(event_loop),

            WindowEvent::Resized(size) => {
                let new_fit = FitBox {
                    max_width: size.width.max(1),
                    max_height: size.height.max(1),
                };
                if Some(new_fit) != self.fit {
                    self.fit = Some(new_fit);
                    if let Some(a) = self.active.as_mut() {
                        // Cheap, per-event: reconfigure the swapchain and let the
                        // renderer GPU-scale the resident texture to the new size.
                        a.renderer.resize(size.width, size.height);
                        // macOS: a surface reconfigure can reset the CAMetalLayer's
                        // colorspace/EDR, so re-assert them — keeps P3/HDR alive across
                        // a resize, fullscreen toggle, or a move to another display
                        // (which may have different EDR headroom).
                        #[cfg(target_os = "macos")]
                        if a.renderer.hdr_surface_wants_edr().is_some() {
                            let headroom = hdr_surface::configure(&a.window);
                            a.renderer.set_edr_headroom(headroom);
                            self.last_edr_headroom = headroom;
                        }
                    }
                    self.draw(event_loop);
                    // A drag fires Resized many times a second; re-decoding the
                    // current photo to the new fit on every one (a CPU decode on
                    // the event-loop thread) is what made resize crawl. Defer the
                    // crisp decode-to-fit + ring refill until the size settles.
                    self.resize_settle_at = Some(Instant::now() + Duration::from_millis(180));
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
                if (sf - self.scale_factor).abs() > f32::EPSILON {
                    self.scale_factor = sf;
                    self.rescale_overlays();
                }
            }

            // Track the windowed position so toggling back / relaunching restores it
            // (#1). A fullscreen window's position is the monitor, not a user choice,
            // so `track_windowed_geometry` ignores it there.
            WindowEvent::Moved(_) => {
                self.track_windowed_geometry();
                // macOS: adapt HDR/EDR if the window crossed onto a different display.
                #[cfg(target_os = "macos")]
                self.reconfigure_edr_for_display(event_loop);
            }

            WindowEvent::RedrawRequested => self.draw(event_loop),

            // Drag-and-drop: winit sends one event per file. Coalesce and apply on
            // the next `about_to_wait` tick (a folder browses recursively; dropped
            // photos become the playlist).
            WindowEvent::DroppedFile(path) => {
                self.pending_drops.push(path);
                if let Some(a) = self.active.as_ref() {
                    a.window.request_redraw();
                }
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
                            self.pending_confirm_delete = None;
                            // Same leak guard as the dialog path: a held/repeated Esc
                            // after this close must not fall through to quit.
                            self.esc_guard_until =
                                Some(Instant::now() + Duration::from_millis(300));
                            return;
                        }
                        // Swallow a stray Esc that leaked from dismissing the file
                        // picker (open_picker); a real Esc a moment later still quits.
                        let quit = esc_quits(self.esc_guard_until, Instant::now());
                        self.esc_guard_until = None;
                        if quit {
                            self.begin_exit(event_loop);
                        }
                    } else if !repeat {
                        // Real press only — OS auto-repeats are ignored so they can't
                        // queue up and delay the release. Every key is resolved through
                        // the configurable keymap (task #8) and routed by kind:
                        //   - one-shot → run the command now (`dispatch_action`);
                        //   - nav → start hold-to-fly (advance now, repeat in the loop);
                        //   - held → track by physical key; pan/zoom apply each frame in
                        //     `about_to_wait`.
                        // Holding for all of these is driven by `about_to_wait`.
                        let chord = KeyChord::new(code, self.ctrl, self.shift, self.alt, self.logo);
                        if let Some(act) = self.keymap.action_for(&chord) {
                            match act.kind() {
                                ActionKind::OneShot => self.dispatch_action(act, event_loop),
                                ActionKind::Nav => self.nav_press(code, act, event_loop),
                                ActionKind::Held => {
                                    self.held.insert(code, act);
                                }
                                ActionKind::FrameStep => {
                                    self.frame_step_press(code, act, event_loop)
                                }
                            }
                        }
                    }
                }
                ElementState::Released => {
                    self.held.remove(&code);
                }
            },

            // OS light↔dark theme switched at runtime: re-theme the native menu so
            // it keeps matching the desktop (the window title bar is winit's).
            WindowEvent::ThemeChanged(_) => {
                #[cfg(windows)]
                self.refresh_menu_theme();
            }

            // Track Shift for Shift+R / Shift+I.
            WindowEvent::ModifiersChanged(mods) => {
                self.shift = mods.state().shift_key();
                self.ctrl = mods.state().control_key();
                self.alt = mods.state().alt_key();
                // `super_key()` is Cmd (⌘) on macOS, the Windows key elsewhere.
                self.logo = mods.state().super_key();
            }

            // Focus loss can swallow the key-up event; clear held keys so
            // navigation never gets stuck auto-advancing (a known winit repeat /
            // lost-key-up hazard, called out in CLAUDE.md).
            WindowEvent::Focused(false) => {
                self.held.clear();
                self.hold_start = None;
                self.shift = false;
                self.ctrl = false;
                self.alt = false;
                self.logo = false;
                self.zoom_started = None;
                self.zoom_last = None;
                self.pan_started = None;
                self.pan_last = None;
                self.pie_glow_started = None;
                // Focus loss can swallow the button-up — never leave a drag stuck.
                self.dragging = false;
            }

            // Track the pointer (anchor for pinch/wheel zoom) and, while the left
            // button is held, drag-to-pan: move the image by the cursor delta.
            WindowEvent::CursorMoved { position, .. } => {
                let p = [position.x as f32, position.y as f32];
                if self.dragging {
                    if let Some(prev) = self.last_cursor {
                        self.pan_by_pixels(p[0] - prev[0], p[1] - prev[1], event_loop);
                    }
                }
                self.last_cursor = Some(p);
                self.update_chip_hover(event_loop);
                self.refresh_cursor();
            }

            // Pointer left the window: drop any Cancel Scan hover so the button doesn't stay lit.
            WindowEvent::CursorLeft { .. } => {
                self.last_cursor = None;
                self.update_chip_hover(event_loop);
            }

            // Left button toggles drag-to-pan (the cross-platform pan gesture).
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let pressed = state == ElementState::Pressed;
                // A press on the scan-count chip cancels the scan (the one interactive
                // on-image control) — and must NOT also start a drag-to-pan.
                if pressed
                    && self
                        .last_cursor
                        .is_some_and(|[cx, cy]| self.chip_hit(cx, cy))
                {
                    self.cancel_scan_command(event_loop);
                } else {
                    self.dragging = pressed;
                    self.refresh_cursor();
                }
            }

            // Trackpad pinch (macOS): magnify about the cursor. `delta` is the
            // incremental magnification (+ spread to zoom in, − pinch to zoom out).
            WindowEvent::PinchGesture { delta, .. } => {
                let factor = 1.0 + delta as f32 * PINCH_GAIN;
                self.zoom_about_cursor(factor, event_loop);
            }

            // Trackpad two-finger double-tap (macOS "smart magnify"): toggle 100%,
            // sharing the keyboard's `0` / menu toggle so they can't drift.
            WindowEvent::DoubleTapGesture { .. } => {
                self.dispatch_action(Action::ToggleOriginal, event_loop);
            }

            // Scroll. macOS sends pixel-precise `PixelDelta` (always pan — pinch is the
            // Mac zoom gesture); Windows reports both a real mouse wheel and a precision-
            // trackpad two-finger swipe as `LineDelta`. The `Scroll wheel` setting picks
            // what a plain `LineDelta` does (pan by default, or zoom); Ctrl always flips
            // to the other action — so a two-finger swipe pans like on the Mac, and zoom
            // stays reachable with Ctrl held (or as the default if the user prefers it).
            WindowEvent::MouseWheel { delta, .. } => match delta {
                MouseScrollDelta::PixelDelta(p) => self.pan_by_pixels(
                    p.x as f32 * GESTURE_PAN_DIR,
                    p.y as f32 * GESTURE_PAN_DIR,
                    event_loop,
                ),
                MouseScrollDelta::LineDelta(x, y) => {
                    let zooms = self.settings.scroll_action == settings::ScrollAction::Zoom;
                    if zooms != self.ctrl {
                        let factor = (1.0 + y * WHEEL_ZOOM_STEP).max(0.05);
                        self.zoom_about_cursor(factor, event_loop);
                    } else {
                        self.pan_by_pixels(
                            x * WHEEL_PAN_STEP * GESTURE_PAN_DIR,
                            y * WHEEL_PAN_STEP * GESTURE_PAN_DIR,
                            event_loop,
                        );
                    }
                }
            },

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
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
                self.dispatch_menu(action, event_loop);
                // muda auto-flips a clicked CheckMenuItem's native checkmark, which can
                // desync from the real state (e.g. clicking the already-active scale mode
                // unchecks it though nothing changed). Invalidate the cache so the refresh
                // below re-asserts the true state unconditionally.
                self.view_checks_state = None;
            }
        }
        // Keep "Save Rotation" enabled only when it applies (cheap + cached, so this
        // per-tick call is a no-op unless the photo/rotation actually changed).
        self.refresh_save_menu_item();
        // Keep Edit ▸ Undo's label + enabled state mirroring the top of the undo stack
        // (cached, so it's a no-op until the stack changes).
        self.refresh_undo_menu_item();
        // Enable File ▸ Stop Scanning only while a scan streams in (cached no-op otherwise).
        self.refresh_cancel_scan_menu_item();
        // Keep the View-menu checkmarks (scale mode / recursive / fullscreen / info) in
        // sync with the live state — likewise cached, so it's a no-op until state changes.
        self.refresh_view_menu_checks();
        // macOS: keep the native fullscreen item's title ("Enter"/"Exit Full Screen") in
        // sync — cached, and catches green-button / gesture toggles, not just our menu.
        #[cfg(target_os = "macos")]
        self.refresh_native_fullscreen_label();
        // macOS: keep the title-bar proxy icon pointed at the displayed photo (cached).
        #[cfg(target_os = "macos")]
        self.refresh_proxy_icon();
        // Deferred delete-advance: once the icon has shown for a beat, drop the item.
        if self.pending_delete.is_some_and(|(at, _)| now >= at) {
            self.flush_pending_delete(event_loop);
        }
        // 0b. Apply any files dropped on the window this burst (coalesced — winit
        // delivers one `DroppedFile` per file).
        if !self.pending_drops.is_empty() {
            let drops = std::mem::take(&mut self.pending_drops);
            self.open_input(classify_inputs(drops), event_loop);
        }
        // 0b'. macOS: files opened from Finder / the Dock / `open -a` arrive via
        // `application:openURLs:` (winit drops them — see `macos_open`); route them
        // through the same open path as drag-and-drop.
        #[cfg(target_os = "macos")]
        {
            let opened = macos_open::take_opened();
            if !opened.is_empty() {
                self.open_input(classify_inputs(opened), event_loop);
            }
        }
        // 0c. Pick up a finished background archive open (.7z eager decompress) or
        // directory scan (large/nested folder walked off the event loop).
        self.poll_archive_load(event_loop);
        self.poll_dir_scan(event_loop);

        // 1. Absorb finished decodes (uploads; presents the target if it arrived).
        self.drain_results(event_loop);

        // 1b. Pick up a finished off-thread animation decode (kicked by `P` /
        // frame-step) and install playback — never on the still/keypress hot path (#37).
        self.poll_anim_decode(event_loop);

        // 2. Continuous zoom/pan while their keys are held (accelerating ramp).
        let transforming = self.apply_view_holds(now, event_loop);

        // 3. Gated self-paced advance while a nav key (space/backspace) is held.
        // The initial tap delay gates *repeat*, not draining/presenting, so a
        // first-press miss shows the moment it decodes. (plain `match`, not
        // `is_none_or`: that's 1.82+ vs the 1.80 MSRV.)
        let nav = self.held_nav();
        let past_delay = match self.hold_start {
            Some(t) => now >= t + self.initial_delay,
            None => true,
        };
        if let Some(dir) = nav {
            // Advance only when caught up (target shown) AND the (accelerating)
            // interval elapsed, so every photo is shown and a miss simply holds.
            // The gap ramps from ~1/start_speed down to the ceiling's interval over
            // ramp_secs of held auto-repeat (measured from when the tap delay
            // expired), so holding a nav key flies slow -> fast for control. The
            // ceiling is the configured max-photos/sec cap (#20), or the refresh rate
            // when uncapped. All three are read live from the settings.
            let caught_up = self.displayed_item == self.target_item;
            let repeat_elapsed = match self.hold_start {
                Some(t) => now.saturating_duration_since(t + self.initial_delay),
                None => Duration::ZERO,
            };
            let interval = advance_interval(
                repeat_elapsed,
                self.settings.start_speed,
                self.settings.ramp_secs,
                self.settings.max_advance_rate as f32,
                self.frame_interval,
            );
            let due = match self.last_present {
                Some(t) => now >= t + interval,
                None => true,
            };
            if past_delay && caught_up && due {
                self.advance(dir, event_loop);
            } else if !caught_up {
                self.try_present_target(event_loop);
            }
        } else {
            self.hold_start = None;
        }

        // 3c. Slideshow auto-advance (task #23). When on and not overridden by a held
        // nav key (hold-to-fly takes precedence) or an open dialog (Settings/About pause
        // it; the picker is modal so it pauses on its own), advance in the last direction
        // once the interval has elapsed since the current slide was shown. Readiness-
        // gated like hold-to-fly: a not-ready next slide isn't caught up yet, so it holds
        // (never skips). Manual nav resets the timer (it moves `last_present`); a live
        // `[`/`]` interval change applies immediately (deadline is `last_present + interval`).
        let slideshow_running =
            self.slideshow.on && self.held_nav().is_none() && self.dialog.is_none();
        if slideshow_running {
            let caught_up = self.displayed_item == self.target_item;
            let since_shown = self
                .last_present
                .map(|t| now.saturating_duration_since(t))
                .unwrap_or(Duration::ZERO);
            if caught_up && self.slideshow.is_due(since_shown) {
                self.advance(self.last_nav, event_loop);
            }
        }

        // 3b. Sharpen / prefetch-ahead. When parked, re-issue the prefetch (which
        // requests the on-screen photo's full at top priority and the ahead-ring
        // behind the previews) whenever the wanted-fulls set changes — as previews
        // land and fulls complete, not every tick → no per-frame churn. While flying,
        // `advance` already re-issues per step, so the idle pump stays out of the way.
        // Keep the loop ticking while any sharpen is outstanding so `drain_results`
        // catches it.
        let mut sharpen_pending = false;
        if self.held_nav().is_none() {
            let upgrade = self.fulls_wanted();
            if upgrade != self.last_upgrade_set {
                self.last_upgrade_set = upgrade.clone();
                self.request_prefetch();
            }
            sharpen_pending = !upgrade.is_empty();
        }

        // 4. Info panel visibility. "Blaze mode" = actually flying (a nav key held
        // *past* the tap delay): hide the panel so it isn't a strobing distraction
        // while photos fly by. Otherwise — idle, or a single tap inside the delay —
        // keep it shown and tracking the current photo (rebuilt when the photo
        // changes, so single-stepping never blanks/flashes it). No timed delay:
        // the blaze gate alone keeps it off the fly. Left untouched mid zoom/pan.
        let flying = nav.is_some() && past_delay;
        // 4a′. Flash the "Press P to play" hint once on settling on an animated still —
        // suppressed while flying (no nag mid-fly) and once playback has engaged (#37).
        self.maybe_show_anim_hint(flying, event_loop);
        if self.info != InfoMode::Off {
            if flying {
                if self.overlay_shown {
                    self.hide_overlay(event_loop);
                }
            } else if !transforming
                // Help is static (no photo needed); the info panels need a photo.
                && (self.info == InfoMode::Help || self.current.is_some())
                && (!self.overlay_shown || self.overlay_item != self.displayed_item)
            {
                self.show_overlay(event_loop);
            }
        }

        // 4b. Transient status toast: hold then fade (re-uploading only when the
        // alpha changes); clears itself when expired. Shown only for specific
        // commands (e.g. the recursion toggle) — never per photo.
        let toast_active = self.tick_toast(now, event_loop);

        // 4c. The "not-ready" loading pie: shown while the next photo is still
        // decoding (a miss that outlasts the show-delay), fading out once it lands.
        let pie_active = self.tick_pie(now, event_loop);

        // 4c′. The ambient scan-count chip (below the pie) while a folder scan streams in.
        self.tick_chip(event_loop);

        // 4d. Once a resize/toggle has settled, run the deferred decode-to-fit:
        // rebuild the ring at the new slot size, re-show the current photo crisp,
        // and refill neighbours. Debounced from the Resized handler so a drag
        // doesn't re-decode on every intermediate size.
        let resizing = match self.resize_settle_at {
            Some(at) if now >= at => {
                self.resize_settle_at = None;
                self.invalidate_geometry();
                self.load_current_sync(event_loop);
                self.target_item = self.playlist.current();
                self.request_prefetch();
                // Re-place a visible info/EXIF/help panel against the settled surface
                // size, with a freshly sized corner margin. A fullscreen toggle resizes
                // the surface but leaves the panel's quad placed for the old one, so
                // after toggling it can end up jammed in the corner — re-show fixes it (#3).
                if self.overlay_shown {
                    self.show_overlay(event_loop);
                }
                false
            }
            Some(_) => true, // still settling — keep ticking so it fires
            None => false,
        };

        // 4e. Persist the windowed geometry once the user stops moving/resizing (#1).
        // Debounced from the Moved/Resized handlers so a drag isn't a write storm; an
        // explicit user action (positioning the window), never the view path.
        if let Some(at) = self.geometry_save_at {
            if now >= at {
                self.geometry_save_at = None;
                self.settings.save();
            }
        }

        // 4f. Pump an open dialog's egui animation clock. egui is immediate-mode, so a
        // combo popup opening, the Checking… spinner, or a hover fade only advances when
        // a frame is requested — without this the dialog freezes between OS events (the
        // "a clicked dropdown doesn't open until you move the mouse" jank). A zero-delay
        // request already re-armed a redraw inside the dialog's `render`; here we fire a
        // *timed* refresh that's now due and surface its deadline so the loop wakes for it.
        let dialog_repaint = self.dialog.as_ref().and_then(|d| d.repaint_at());
        if let Some(at) = dialog_repaint {
            if now >= at {
                if let Some(d) = self.dialog.as_ref() {
                    d.request_redraw();
                }
            }
        }
        let dialog_wake = dialog_repaint.filter(|&at| at > now);

        // 4g. On-demand animation (task #37), off the photo hot path. `tick_playback`
        // advances to the due frame and returns the next frame's precise deadline (None
        // when not actively playing) so we sleep exactly until then; `tick_frame_step`
        // drives the held `,`/`.` scrub (polls at frame rate, like a held nav key).
        let anim_wake = self.tick_playback(now, event_loop);
        let framestep_active = self.tick_frame_step(now, event_loop);

        // 5. Poll at the frame rate while interacting or work is outstanding; else sleep
        //    to the slideshow's next-slide deadline when it's the only thing pending;
        //    honor an open dialog's timed repaint; otherwise go fully idle until an event.
        let base_wake = if nav.is_some()
            || transforming
            || self.work_pending()
            || toast_active
            || pie_active
            || resizing
            || sharpen_pending
            || framestep_active
            || self.pending_delete.is_some()
            // Keep ticking until the debounced windowed-geometry save fires (#1), so a
            // window move/resize is persisted even when nothing else wakes the loop.
            || self.geometry_save_at.is_some()
        {
            Some(now + self.frame_interval)
        } else if slideshow_running {
            // Sleep until the next slide is due rather than spinning — when caught up and
            // waiting, only the deadline wakes us. Fall back to a frame poll if no slide
            // has shown yet; clamp into the future so a just-passed deadline still
            // schedules a wake (we advance on the following tick).
            Some(
                self.last_present
                    .map(|t| t + self.slideshow.interval)
                    .unwrap_or(now + self.frame_interval)
                    .max(now + Duration::from_millis(1)),
            )
        } else {
            None
        };
        // The earliest pending wake across the viewer, an open dialog, and the
        // animation's next-frame deadline; `None` = idle.
        let wake = [base_wake, dialog_wake, anim_wake]
            .into_iter()
            .flatten()
            .min();
        match wake {
            Some(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// The last path component of a (possibly archive-relative) name, e.g.
/// `trip/day1/IMG.jpg` → `IMG.jpg`. Falls back to the whole string.
fn file_name_of(name: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string()
}
/// Whether physical-px point `(x, y)` lies within `[x0, y0, x1, y1]` (inclusive) — the
/// overlay click hit-test (the scan-count chip today; EXIF copy buttons later).
fn point_in_rect([x0, y0, x1, y1]: [f32; 4], x: f32, y: f32) -> bool {
    x >= x0 && x <= x1 && y >= y0 && y <= y1
}

fn title_for(name: &str, idx: usize, n: usize) -> String {
    format!("{} ({}/{n})", file_name_of(name), idx + 1)
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

/// The folder the Open dialog should start in.
///
/// Priority: a user-pinned `fixed` folder (the Settings preference) wins outright. Else,
/// for an **archive** source, the folder that *contains* the archive — never the archive
/// itself: the OS file dialog can't browse inside a `.zip`/`.7z`, and an *encrypted* one
/// errors outright ("Windows cannot open the folder…"). Else the current photo's folder
/// (the scanned folder, then the display root). When all of those are empty — a bare
/// launch with nothing open — fall back to `fallback` (the user's Pictures/home), so the
/// dialog never falls back to *Windows'* own last-folder memory (a privacy trace).
fn picker_start_dir(
    fixed: Option<&Path>,
    container: Option<&Path>,
    scan_root: Option<&Path>,
    root: &Path,
    fallback: &Path,
) -> PathBuf {
    let non_empty = |p: Option<&Path>| {
        p.filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };
    if let Some(dir) = non_empty(fixed) {
        return dir;
    }
    if let Some(archive) = container {
        return non_empty(archive.parent()).unwrap_or_else(|| fallback.to_path_buf());
    }
    non_empty(scan_root)
        .or_else(|| non_empty(Some(root)))
        .unwrap_or_else(|| fallback.to_path_buf())
}

/// A safe default folder for the Open dialog when nothing else applies (a bare launch):
/// the user's Pictures folder if it exists, else their home, else the current directory.
/// Used so the dialog always opens somewhere real instead of letting Windows fall back to
/// its remembered last folder.
fn default_picker_dir() -> PathBuf {
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = std::env::var_os(home_var) {
        let home = PathBuf::from(home);
        let pics = home.join("Pictures");
        if pics.is_dir() {
            return pics;
        }
        if home.is_dir() {
            return home;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
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

/// Map the persisted default scale mode to the renderer's [`ScaleMode`].
fn scale_mode_of(p: settings::ScaleModePref) -> ScaleMode {
    match p {
        settings::ScaleModePref::Fit => ScaleMode::Fit,
        settings::ScaleModePref::Fill => ScaleMode::Fill,
        settings::ScaleModePref::Original => ScaleMode::Original,
    }
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

    let report = app.metrics.report();
    if !report.is_empty() {
        let mut d = POOL_DECODE_MS.lock().unwrap().clone();
        let times: Vec<f64> = d.iter().map(|(ms, _)| *ms).collect();
        let p = metrics::percentiles(&times, &[50.0, 95.0, 99.0]);
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

    #[test]
    fn truncate_exif_value_caps_long_values() {
        assert_eq!(truncate_exif_value("1/250 s"), "1/250 s");
        let out = truncate_exif_value(&"a".repeat(100));
        assert_eq!(out.chars().count(), EXIF_VALUE_MAX + 1); // value + ellipsis
        assert!(out.ends_with('…'));
        // Multibyte values are truncated on char boundaries, not bytes.
        let out = truncate_exif_value(&"é".repeat(100));
        assert_eq!(out.chars().count(), EXIF_VALUE_MAX + 1);
    }

    #[test]
    fn advance_interval_ramps_from_floor_to_refresh() {
        let frame = Duration::from_micros(8_333); // ~120 Hz
        let (min_rate, ramp) = (3.0, 4.0);
        // At the start of auto-repeat the gap is ~1/min_rate (a few photos/sec).
        let start = advance_interval(Duration::ZERO, min_rate, ramp, 0.0, frame);
        let expected = 1.0 / min_rate;
        assert!(
            (start.as_secs_f32() - expected).abs() < 1e-3,
            "start interval {start:?} should be ~1/min_rate ({expected}s)"
        );
        // Once the ramp completes it clamps exactly to the refresh interval, and
        // stays there past the ramp (refresh is the hard ceiling).
        assert_eq!(
            advance_interval(Duration::from_secs_f32(ramp), min_rate, ramp, 0.0, frame),
            frame
        );
        assert_eq!(
            advance_interval(Duration::from_secs(10), min_rate, ramp, 0.0, frame),
            frame
        );
    }

    #[test]
    fn advance_interval_caps_at_max_rate() {
        let frame = Duration::from_micros(8_333); // ~120 Hz
        let cap = 10.0; // photos/sec — well below refresh
        let cap_interval = Duration::from_secs_f32(1.0 / cap);
        // Once ramped, the gap holds at the cap's interval, never the refresh frame.
        let ramped = advance_interval(Duration::from_secs(10), 3.0, 4.0, cap, frame);
        assert!(
            (ramped.as_secs_f32() - cap_interval.as_secs_f32()).abs() < 1e-4,
            "should top out at the {cap}/s cap, got {ramped:?}"
        );
        assert!(ramped > frame, "the cap is slower than the refresh ceiling");
        // A cap at/above the refresh rate behaves as uncapped (refresh is the limit).
        assert_eq!(
            advance_interval(Duration::from_secs(10), 3.0, 4.0, 1000.0, frame),
            frame
        );
    }

    #[test]
    fn advance_interval_is_monotonic_and_never_below_refresh() {
        let frame = Duration::from_micros(8_333);
        let mut prev = advance_interval(Duration::ZERO, 3.0, 4.0, 0.0, frame);
        for ms in [200u64, 500, 1000, 2000, 3000, 4000] {
            let cur = advance_interval(Duration::from_millis(ms), 3.0, 4.0, 0.0, frame);
            assert!(cur <= prev, "interval should shrink as the hold continues");
            assert!(cur >= frame, "never faster than the refresh ceiling");
            prev = cur;
        }
    }

    #[test]
    fn advance_interval_no_ramp_when_floor_meets_refresh() {
        // A low-Hz display where min_rate >= refresh: no ramp, just the refresh cap.
        let frame = Duration::from_millis(500); // 2 Hz, below the 3/s floor
        assert_eq!(
            advance_interval(Duration::ZERO, 3.0, 4.0, 0.0, frame),
            frame
        );
        assert_eq!(
            advance_interval(Duration::from_secs(1), 3.0, 4.0, 0.0, frame),
            frame
        );
    }

    #[test]
    fn picker_starts_in_the_folder_containing_an_archive() {
        // Archive source: container is the .7z file; the Open dialog must start in its
        // parent folder, never the archive itself (the OS dialog can't browse inside it,
        // and an encrypted one errors). Holds for zip + 7z, encrypted or not.
        let fb = Path::new("fallback");
        let archive = Path::new("photos/trips/spain.7z");
        let got = picker_start_dir(None, Some(archive), None, archive, fb);
        assert_eq!(got, Path::new("photos/trips"));

        let zip = Path::new("albums/2015.zip");
        assert_eq!(
            picker_start_dir(None, Some(zip), None, zip, fb),
            Path::new("albums")
        );
    }

    #[test]
    fn picker_uses_the_scanned_folder_for_a_normal_source() {
        // No archive, no pin: prefer the scanned folder, else the display root.
        let fb = Path::new("fallback");
        let folder = Path::new("photos/trips");
        assert_eq!(
            picker_start_dir(None, None, Some(folder), folder, fb),
            folder
        );

        let root = Path::new("photos");
        assert_eq!(picker_start_dir(None, None, None, root, fb), root);
    }

    #[test]
    fn picker_pinned_folder_wins_over_everything() {
        // A user-pinned folder is used regardless of the current source (incl. archives).
        let fb = Path::new("fallback");
        let pinned = Path::new("D:/AlwaysHere");
        let archive = Path::new("photos/trips/spain.7z");
        assert_eq!(
            picker_start_dir(Some(pinned), Some(archive), None, archive, fb),
            pinned
        );
        assert_eq!(
            picker_start_dir(
                Some(pinned),
                None,
                Some(Path::new("photos")),
                Path::new("photos"),
                fb
            ),
            pinned
        );
    }

    #[test]
    fn picker_falls_back_when_there_is_no_current_folder() {
        // Bare launch: empty scan_root + empty root → the safe fallback, NOT an empty
        // path (which would let Windows surface its own remembered last folder).
        let fb = Path::new("fallback");
        let empty = Path::new("");
        assert_eq!(picker_start_dir(None, None, None, empty, fb), fb);
        assert_eq!(picker_start_dir(None, None, Some(empty), empty, fb), fb);
    }
}
