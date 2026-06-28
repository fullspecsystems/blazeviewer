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
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Icon, Window, WindowId};

use pb_core::open::{self, LaunchInput, Source};
use pb_core::{full_ring, prefetch_targets, Playlist, ResidentRing};
use pb_decode::{
    decode_bytes, decode_named_bytes, is_supported_extension, read_exif_fields, DecodeError,
    DecodedImage, FitBox, PixelFormat,
};
use pb_render::{
    test_pattern, Renderer, Rotation, ScaleMode, ViewTransform, WgpuRenderer, MAX_ZOOM, MIN_ZOOM,
};
use pb_source::{seven_z_projected_bytes, FsSource, PhotoSource, SevenZSource, ZipSource};

mod action;
mod archive;
mod clipboard;
#[cfg(windows)]
mod darkmode;
mod decode_pool;
mod delete;
mod dialog;
mod hud;
mod icon;
mod keymap;
mod menu;
mod metrics;
mod save_rotation;
mod settings;
use action::{Action, ActionKind};
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

/// Hold-to-fly advance curve: while a nav key is held past the initial tap delay,
/// the auto-advance rate ramps from a gentle `ADVANCE_MIN_RATE` (a few photos/sec,
/// for control) up to the display refresh rate over `ADVANCE_RAMP_SECS`. Same
/// linear-rate shape as the zoom/pan ramps. The ramp only sets the *attempted*
/// cadence — decode readiness still caps the real rate (a miss holds, never skips)
/// and the refresh rate is the hard ceiling (a configurable lower cap is a future
/// settings option; until then refresh is the max).
const ADVANCE_MIN_RATE: f32 = 3.0;
const ADVANCE_RAMP_SECS: f32 = 4.0;
/// How long the delete icon shows on the just-deleted photo before the playlist
/// advances to the next one — so the trash/recycle feedback registers first (#28).
const DELETE_ADVANCE_DELAY: Duration = Duration::from_millis(160);

/// The minimum gap since the last shown photo before the next held-key auto-advance,
/// given how long auto-repeat has been running (`elapsed`, measured from when the
/// initial tap delay expired). The advance rate ramps linearly from `min_rate`
/// (photos/sec) up to the refresh rate (`1 / frame_interval`) over `ramp_secs`, then
/// holds there; the returned interval is the reciprocal, floored at `frame_interval`
/// so it's never faster than the refresh ceiling. Pure + time-based (frame-rate
/// independent), so the curve is unit-testable without the event loop.
fn advance_interval(
    elapsed: Duration,
    min_rate: f32,
    ramp_secs: f32,
    frame_interval: Duration,
) -> Duration {
    let frame_secs = frame_interval.as_secs_f32();
    let refresh_rate = 1.0 / frame_secs.max(f32::MIN_POSITIVE);
    // No headroom to ramp (floor already at/above refresh, e.g. a very low-Hz
    // display): just run at the refresh cap.
    if min_rate >= refresh_rate {
        return frame_interval;
    }
    let t = if ramp_secs > 0.0 {
        (elapsed.as_secs_f32() / ramp_secs).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let rate = min_rate + (refresh_rate - min_rate) * t;
    // At/above the ceiling, return the refresh interval exactly (no float drift).
    if rate >= refresh_rate {
        return frame_interval;
    }
    let secs = (1.0 / rate.max(f32::MIN_POSITIVE)).clamp(frame_secs, 60.0);
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
    decode_named_bytes(source.name(item), &bytes, fit, allow_preview)
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
    /// The "Save Rotation" menu item, kept so its enabled state can be toggled at
    /// runtime (only enabled when the current photo has an unsaved rotation on an
    /// EXIF-writable file). `save_enabled` caches the last-pushed state so the
    /// refresh is a no-op Win32 call when nothing changed.
    save_rotation_item: Option<muda::MenuItem>,
    save_enabled: bool,
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
    /// An in-flight background archive open (a `.7z` eager-decompresses off-thread so
    /// it can't freeze the event loop). `None` when no archive is loading.
    archive_load: Option<ArchiveLoad>,
    /// Monotonic id for archive-open requests; a newer open bumps it so a superseded
    /// load's result is discarded when it finally arrives.
    archive_gen: u64,
    /// A launch input (an archive) deferred until the window exists, so the viewer
    /// appears immediately and a slow / encrypted / failed open uses the spinner +
    /// dialogs instead of blocking startup or only logging. Fired once in `resumed`.
    pending_launch: Option<LaunchInput>,
    /// The archive currently awaiting a password: set when the password prompt opens,
    /// re-opened with the entered password on submit, cleared on success / cancel.
    password_archive: Option<PathBuf>,
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
        Self {
            windowed,
            source,
            playlist,
            active: None,
            held: HashMap::new(),
            last_present: None,
            frame_interval: Duration::from_micros(8_333), // ~120 Hz until we read the real rate
            hold_start: None,
            initial_delay: Duration::from_millis(400),
            fit: None,
            view: ViewTransform::default(),
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
            targets: Vec::new(),
            meta_cache: HashMap::new(),
            ahead: 8,
            behind: 2,
            failed: HashSet::new(),
            pending_uploads: Vec::new(),
            rotations: HashMap::new(),
            shift: false,
            ctrl: false,
            alt: false,
            recursive,
            scan_root,
            pending_drops: Vec::new(),
            toast: None,
            esc_guard_until: None,
            resize_settle_at: None,
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
            preview_resident: HashSet::new(),
            upgrade_done: HashSet::new(),
            last_upgrade_set: Vec::new(),
            full_requested_at: HashMap::new(),
            menu: None,
            save_rotation_item: None,
            save_enabled: false,
            pending_delete: None,
            pending_confirm_delete: None,
            menu_attached: false,
            dialog: None,
            keymap: Keymap::load(),
            archive_load: None,
            archive_gen: 0,
            pending_launch: None,
            password_archive: None,
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
        self.targets = prefetch_targets(&self.playlist, self.ahead, self.behind);
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
                self.show_toast_icon("", Some(icon::assets::FLOPPY), event_loop);
            }
            Err(e) => {
                eprintln!("save rotation failed: {}: {e}", path.display());
                self.show_toast("Save failed", event_loop);
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
        self.invalidate_geometry();
        self.displayed_item = None;
        self.target_item = None;
        self.current = None;
        let p = test_pattern(1600, 1000);
        let srgb = pb_render::ColorTransform::srgb();
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_image(&p, 1600, 1000, srgb, false, 1.0);
            a.renderer.set_overlay(None, 0);
            a.window.set_title("PhotoBlaze (no images)");
        }
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
        let decoded = decode_item(self.source.as_ref(), idx, self.decode_fit(), false);
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

    /// Defer a launch input until the window + engine exist (`resumed` fires it).
    /// Used for an archive on the command line / double-click so startup shows the
    /// window first and the open runs behind the spinner + dialogs.
    fn queue_launch(&mut self, input: LaunchInput) {
        self.pending_launch = Some(input);
    }

    /// Open a launch input at runtime (the file picker or a drag-drop): plan it,
    /// build the playlist, and jump to the plan's cursor (the dropped/clicked
    /// photo, or the first of a folder). Empty selections are ignored so the
    /// current photo isn't blanked.
    fn open_input(&mut self, input: LaunchInput, event_loop: &ActiveEventLoop) {
        let plan = open::plan(input);
        // Archives open via the async-aware path (a .7z decompresses off-thread so it
        // can't freeze the loop). Folders / file lists resolve synchronously (cheap).
        if let Source::Archive(path) = &plan.source {
            self.begin_archive_open(path.clone(), None, event_loop);
            return;
        }
        let r = resolve_playlist(&plan.source, &plan.cursor);
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
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send((generation, load_seven_z(&worker_path, password)));
        });
        self.archive_load = Some(ArchiveLoad {
            generation,
            rx,
            path,
            was_password_attempt,
        });
        self.show_toast("Loading archive…", event_loop);
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
            Err(e) => self.fail_archive_open(&e, event_loop),
        }
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

    /// Toggle recursive scanning of the current folder (`Ctrl+R`), keeping the
    /// current photo in view. A no-op for an explicit file list (multi-select /
    /// dropped photos): there is no single root to walk.
    fn toggle_recursive(&mut self, event_loop: &ActiveEventLoop) {
        let Some(root) = self.scan_root.clone() else {
            return;
        };
        let keep = self
            .displayed_item
            .and_then(|i| self.source.path(i))
            .map(Path::to_path_buf);
        let source = Source::Scan {
            roots: vec![root],
            recursive: !self.recursive,
        };
        let r = resolve_playlist(&source, &open::Cursor::First);
        if r.source.is_empty() {
            return;
        }
        // Re-find the photo we were viewing in the rebuilt (recursive) listing.
        let start = keep
            .as_ref()
            .and_then(|k| (0..r.source.len()).find(|&i| r.source.path(i) == Some(k.as_path())))
            .unwrap_or(0);
        let recursive = r.recursive;
        self.rebuild_playlist(r.source, r.root, r.scan_root, recursive, start, event_loop);
        let msg = if recursive {
            "Recursive folders: on"
        } else {
            "Recursive folders: off"
        };
        self.show_toast(msg, event_loop);
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
        settings::save_fullscreen(!self.windowed);
        if let Some(a) = self.active.as_ref() {
            if self.windowed {
                a.window.set_fullscreen(None);
                a.window.set_decorations(true);
                let _ = a.window.request_inner_size(PhysicalSize::new(1280, 800));
            } else {
                // Borderless "windowed fullscreen": size a decoration-less window
                // to the monitor ourselves instead of the OS fullscreen API, which
                // makes Windows apply fullscreen-optimizations that drop DWM
                // composition on focus changes / transitions and flash the legacy
                // basic-theme caption. A plain borderless window stays composited.
                a.window.set_fullscreen(None);
                a.window.set_decorations(false);
                if let Some(mon) = a.window.current_monitor() {
                    a.window.set_outer_position(mon.position());
                    let _ = a.window.request_inner_size(mon.size());
                }
            }
        }
        // Show the menu in windowed mode, hide it in fullscreen (the chrome-free
        // speed mode). Adding/removing the bar resizes the client area → a `Resized`
        // event → the debounced re-decode path.
        self.apply_menu_for_mode();
    }

    /// Build the native menu bar once (cross-platform; muda owns the OS handle).
    fn ensure_menu(&mut self) {
        if self.menu.is_none() {
            let (menu, save_rotation) = menu::build_menu();
            self.menu = Some(menu);
            self.save_rotation_item = Some(save_rotation);
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
    #[cfg(not(windows))]
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
            Action::SaveRotation => self.save_rotation(event_loop),
            Action::Delete => self.delete_current(false, event_loop),
            Action::DeletePermanent => self.delete_current(true, event_loop),
            Action::OpenFile => self.open_picker(false, event_loop),
            Action::OpenFolder => self.open_picker(true, event_loop),
            Action::Info => self.toggle_info(false, event_loop),
            Action::FullExif => self.toggle_info(true, event_loop),
            Action::Help => self.toggle_help(event_loop),
            Action::Fullscreen => self.toggle_fullscreen(),
            Action::Recursive => self.toggle_recursive(event_loop),
            Action::Settings => self.open_settings(event_loop),
            Action::About => self.open_about(event_loop),
            Action::Quit => self.begin_exit(event_loop),
        }
    }

    /// Show the native picker (`O` = file(s), `Shift+O` = folder) and open the
    /// result. Modal — it blocks the event loop while open, which is fine: the app
    /// isn't flying through photos with a dialog up.
    fn open_picker(&mut self, folder: bool, event_loop: &ActiveEventLoop) {
        let start_dir = self.scan_root.clone().unwrap_or_else(|| self.root.clone());
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
            Some(idx) => match decode_item(self.source.as_ref(), idx, self.decode_fit(), false) {
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
                self.current = None;
                let p = test_pattern(1600, 1000);
                (
                    p,
                    1600,
                    1000,
                    srgb,
                    false,
                    1.0,
                    "PhotoBlaze (no images)".to_string(),
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

    /// Open the Settings dialog (Ctrl+,) — an egui window (skeleton for now).
    fn open_settings(&mut self, event_loop: &ActiveEventLoop) {
        self.open_dialog(dialog::DialogKind::Settings, event_loop);
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
        self.dialog = dialog::DialogWindow::open(kind, event_loop, refresh, "", parent.as_deref());
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
            parent.as_deref(),
        );
    }

    /// Route an event for the dialog window (egui owns it). Esc / close button
    /// dismiss it; everything else feeds egui and triggers repaints.
    fn dialog_event(&mut self, event: WindowEvent, event_loop: &ActiveEventLoop) {
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
            (keys(Action::Recursive), "Toggle recursive folders"),
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

    /// Rasterize the active overlay (info panel or help) and draw it. The help
    /// overlay uses a larger font than the info panels.
    fn show_overlay(&mut self, event_loop: &ActiveEventLoop) {
        let px = (15.0 * self.scale_factor).max(8.0);
        let pad = (7.0 * self.scale_factor).round().max(2.0) as u32;
        let panel = match self.info {
            InfoMode::Off => return,
            InfoMode::Basic => {
                let (Some(hud), Some(meta)) = (self.hud.as_ref(), self.current.as_ref()) else {
                    return;
                };
                let text = format!("{} · {}×{} · {}", meta.rel, meta.w, meta.h, meta.codec);
                hud.render_panel(&text, px, pad)
            }
            InfoMode::Full => {
                let rows = self.exif_rows();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                if rows.is_empty() {
                    return;
                }
                hud.render_table(&rows, px, pad)
            }
            InfoMode::Help => {
                let help_px = (20.0 * self.scale_factor).max(12.0);
                let rows = self.help_rows();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                hud.render_table(&rows, help_px, pad)
            }
        };
        let Some((bitmap, w, h)) = panel else {
            return;
        };
        let margin = (10.0 * self.scale_factor).round().max(1.0) as u32;
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
            if let Some((rgba, w, h)) = hud.render_panel_icon(msg, px, pad, icon) {
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
        self.ring = ResidentRing::new(0);
        self.pending_uploads.clear();
        self.meta_cache.clear();
        self.rotations.clear();
        self.failed.clear();
        self.preview_resident.clear();
        self.upgrade_done.clear();
        self.last_upgrade_set.clear();
        self.current = None;
        self.toast = None;
        self.wait_started = None;
        self.pie_finish = None;
        self.pie_glow_started = None;
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
            || self.displayed_item != self.target_item
            || self
                .targets
                .iter()
                .any(|&t| self.ring.slot_for(t).is_none() && !self.failed.contains(&t))
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
        attrs = if self.windowed {
            attrs.with_inner_size(PhysicalSize::new(1280, 800))
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
                let decoded = decode_item(self.source.as_ref(), idx, self.decode_fit(), false);
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

        // Present the first frame WHILE HIDDEN, then reveal — no white startup gap.
        let _ = renderer.render();
        window.set_visible(true);
        window.request_redraw();

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

        // Now that the window + engine are live, kick off any launch input we
        // deferred (an archive): a big .7z loads behind the spinner, and an
        // encrypted / failed open can use the egui dialogs (a synchronous launch
        // resolve, before the event loop, could do neither).
        if let Some(input) = self.pending_launch.take() {
            self.open_input(input, event_loop);
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
                    }
                    self.draw(event_loop);
                    // A drag fires Resized many times a second; re-decoding the
                    // current photo to the new fit on every one (a CPU decode on
                    // the event-loop thread) is what made resize crawl. Defer the
                    // crisp decode-to-fit + ring refill until the size settles.
                    self.resize_settle_at = Some(Instant::now() + Duration::from_millis(180));
                }
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
                        let chord = KeyChord::new(code, self.ctrl, self.shift, self.alt);
                        if let Some(act) = self.keymap.action_for(&chord) {
                            match act.kind() {
                                ActionKind::OneShot => self.dispatch_action(act, event_loop),
                                ActionKind::Nav => self.nav_press(code, act, event_loop),
                                ActionKind::Held => {
                                    self.held.insert(code, act);
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
                self.zoom_started = None;
                self.zoom_last = None;
                self.pan_started = None;
                self.pan_last = None;
                self.pie_glow_started = None;
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        // 0. Native menu-bar clicks (windowed mode). Map each id to the same action
        // the keyboard triggers and dispatch it; an unknown/foreign id is ignored.
        while let Ok(ev) = muda::MenuEvent::receiver().try_recv() {
            if let Some(action) = menu::action_for(ev.id.as_ref()) {
                self.dispatch_menu(action, event_loop);
            }
        }
        // Keep "Save Rotation" enabled only when it applies (cheap + cached, so this
        // per-tick call is a no-op unless the photo/rotation actually changed).
        self.refresh_save_menu_item();
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
        // 0c. Pick up a finished background archive open (.7z eager decompress).
        self.poll_archive_load(event_loop);

        // 1. Absorb finished decodes (uploads; presents the target if it arrived).
        self.drain_results(event_loop);

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
            // The gap ramps from ~1/ADVANCE_MIN_RATE down to one refresh interval
            // over ADVANCE_RAMP_SECS of held auto-repeat (measured from when the tap
            // delay expired), so holding a nav key flies slow -> fast for control.
            let caught_up = self.displayed_item == self.target_item;
            let repeat_elapsed = match self.hold_start {
                Some(t) => now.saturating_duration_since(t + self.initial_delay),
                None => Duration::ZERO,
            };
            let interval = advance_interval(
                repeat_elapsed,
                ADVANCE_MIN_RATE,
                ADVANCE_RAMP_SECS,
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
                false
            }
            Some(_) => true, // still settling — keep ticking so it fires
            None => false,
        };

        // 5. Poll at the frame rate while interacting or work is outstanding;
        //    otherwise go fully idle until the next event.
        if nav.is_some()
            || transforming
            || self.work_pending()
            || toast_active
            || pie_active
            || resizing
            || sharpen_pending
            || self.pending_delete.is_some()
        {
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + self.frame_interval));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
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

fn collect_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if ft.is_dir() {
            collect_recursive(&path, out);
        } else if ft.is_file() && is_supported_image(&path) {
            out.push(path);
        }
    }
}

/// Scan `dir` for supported images, sorted by full path. `recursive` also walks
/// subfolders (a `-r` convenience now; the R-key toggle with folder-grouped
/// ordering is tasks.json #9). Non-recursive is the default, matching that design.
fn scan_images(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if recursive {
        collect_recursive(dir, &mut paths);
    } else {
        match std::fs::read_dir(dir) {
            Ok(rd) => {
                for entry in rd.flatten() {
                    let is_file = entry.file_type().map(|ft| ft.is_file()).unwrap_or(false);
                    let path = entry.path();
                    if is_file && is_supported_image(&path) {
                        paths.push(path);
                    }
                }
            }
            Err(e) => eprintln!("cannot read directory {}: {e}", dir.display()),
        }
    }
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
fn apply_native_window_icon(window: &Window) {
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
fn load_window_icon() -> Option<Icon> {
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
fn resolve_source(source: &Source) -> (Vec<PathBuf>, PathBuf, Option<PathBuf>, bool) {
    match source {
        Source::Scan { roots, recursive } => {
            let mut paths = Vec::new();
            for r in roots {
                paths.extend(scan_images(r, *recursive));
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

/// Turn a planned [`Source`] into a concrete [`PhotoSource`] plus playlist framing.
/// Scans and explicit lists become an [`FsSource`]; an archive opens a
/// [`ZipSource`] (entries read into RAM on demand, never extracted to disk). On a
/// hard archive failure it logs and falls back to an empty source.
fn resolve_playlist(source: &Source, cursor: &open::Cursor) -> Resolved {
    match source {
        Source::Scan { .. } | Source::Explicit(_) => {
            let (paths, root, scan_root, recursive) = resolve_source(source);
            let start = open::resolve_cursor(&paths, cursor);
            Resolved {
                source: Arc::new(FsSource::new(paths)),
                root,
                scan_root,
                recursive,
                start,
            }
        }
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
        load_seven_z(path, password)
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
    let needed = seven_z_projected_bytes(path, password, is_supported_extension)?;
    let budget = archive::ram_budget();
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
) -> Result<Resolved, archive::ArchiveOpenError> {
    let src = SevenZSource::open(path, password, is_supported_extension)?;
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
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli_windowed = args.iter().any(|a| a == "--windowed" || a == "-w");
    let cli_fullscreen = args.iter().any(|a| a == "--fullscreen" || a == "-f");
    // Default to windowed (more discoverable), but restore the saved preference if
    // there is one; an explicit CLI flag always wins.
    let windowed = if cli_windowed {
        true
    } else if cli_fullscreen {
        false
    } else {
        settings::load_fullscreen().map(|fs| !fs).unwrap_or(true)
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
    let input = classify_inputs(positional);
    let mut plan = open::plan(input.clone());
    if let Source::Scan { recursive, .. } = &mut plan.source {
        if force_recursive {
            *recursive = true;
        }
        if force_flat {
            *recursive = false;
        }
    }
    // An archive launch (CLI / double-click) is deferred until the window exists, so
    // the viewer shows immediately and a big / encrypted / failed open uses the
    // spinner + dialogs instead of blocking startup. Folders / files resolve now.
    let archive_launch = matches!(plan.source, Source::Archive(_));
    let resolved = if archive_launch {
        Resolved::empty()
    } else {
        resolve_playlist(&plan.source, &plan.cursor)
    };

    if archive_launch {
        println!("PhotoBlaze: opening archive…");
    } else {
        println!(
            "PhotoBlaze: {} image(s){}",
            resolved.source.len(),
            if resolved.recursive {
                " (recursive)"
            } else {
                ""
            }
        );
        if resolved.source.is_empty() {
            eprintln!("(no images - drop a photo or folder on the window, or press O to open)");
        }
    }

    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

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
    // Hand the deferred archive open to the app; `resumed` fires it once the window
    // and engine are up.
    if archive_launch {
        app.queue_launch(input);
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
        let (paths, root, scan_root, recursive) = resolve_source(&src);
        assert_eq!(
            paths,
            vec![PathBuf::from("/p/a.jpg"), PathBuf::from("/p/b.png")]
        );
        assert_eq!(root, PathBuf::from("/p"));
        assert_eq!(scan_root, None);
        assert!(!recursive);
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
        let start = advance_interval(Duration::ZERO, min_rate, ramp, frame);
        let expected = 1.0 / min_rate;
        assert!(
            (start.as_secs_f32() - expected).abs() < 1e-3,
            "start interval {start:?} should be ~1/min_rate ({expected}s)"
        );
        // Once the ramp completes it clamps exactly to the refresh interval, and
        // stays there past the ramp (refresh is the hard ceiling).
        assert_eq!(
            advance_interval(Duration::from_secs_f32(ramp), min_rate, ramp, frame),
            frame
        );
        assert_eq!(
            advance_interval(Duration::from_secs(10), min_rate, ramp, frame),
            frame
        );
    }

    #[test]
    fn advance_interval_is_monotonic_and_never_below_refresh() {
        let frame = Duration::from_micros(8_333);
        let mut prev = advance_interval(Duration::ZERO, 3.0, 4.0, frame);
        for ms in [200u64, 500, 1000, 2000, 3000, 4000] {
            let cur = advance_interval(Duration::from_millis(ms), 3.0, 4.0, frame);
            assert!(cur <= prev, "interval should shrink as the hold continues");
            assert!(cur >= frame, "never faster than the refresh ceiling");
            prev = cur;
        }
    }

    #[test]
    fn advance_interval_no_ramp_when_floor_meets_refresh() {
        // A low-Hz display where min_rate >= refresh: no ramp, just the refresh cap.
        let frame = Duration::from_millis(500); // 2 Hz, below the 3/s floor
        assert_eq!(advance_interval(Duration::ZERO, 3.0, 4.0, frame), frame);
        assert_eq!(
            advance_interval(Duration::from_secs(1), 3.0, 4.0, frame),
            frame
        );
    }
}
