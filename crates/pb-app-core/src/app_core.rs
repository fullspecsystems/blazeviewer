//! [`AppCore`] — the orchestration state the shell drives (NS0 step 5, ADR-021).
//!
//! The winit `App` god-object is being split into this platform-neutral core (the
//! orchestration state + logic) and a thin shell (`WinitShell` now, an AppKit host later)
//! that owns the window / menu / dialog surface and translates its events into
//! [`CoreEvent`](crate::CoreEvent)s / drains [`CoreEffect`](crate::CoreEffect)s.
//!
//! Filled **incrementally**: each step-5 increment relocates one low-coupling field group
//! off the shell into `AppCore` (reached as `self.core.*`) and stays green. First in: the
//! held-key + input-modifier + self-paced-advance **timing** state — already shell-neutral
//! (`PbKey`/`Action`/`Modifiers`/`Slideshow` + `std`), so it needs no engine-crate deps.
//! Nav/prefetch/decode/residency, the renderer (`Box<dyn Renderer>`), and the
//! `handle(CoreEvent)` dispatch follow (see the step-5 increment order in the brief).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pb_core::{Playlist, ResidentRing};
use pb_decode::FitBox;
use pb_hud::hud::Hud;
use pb_render::{Renderer, Rotation, ViewTransform};
use pb_source::PhotoSource;

use crate::animation::{AnimDecode, Playback, Prepared};
use crate::contract::CoreEffect;
use crate::decode_pool::{DecodePool, Outcome};
use crate::keymap::Keymap;
use crate::metrics::StageTimes;
use crate::overlay::{InfoMode, OpenButton, OpenPanel, PlayHint, Toast};
use crate::settings::Settings;
use crate::undo::UndoAction;
use crate::{Action, Modifiers, PbKey, PhotoMeta, Slideshow};

/// A navigation move: forward (`space`/`→`), backward (`backspace`/`←`), a
/// precomputed-random jump (`enter`), or a step back through the random walk
/// (`shift+enter`). All are gated + self-paced + prefetchable the same way (random
/// walks a known shuffle order, so its next/prior targets are knowable — see
/// `pb_core::ShuffleOrder`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Forward,
    Backward,
    Random,
    RandomPrev,
}

/// The surface geometry the core needs for overlay sizing + hit-testing (NS0 5.5 / Phase
/// 0.4): the window's inner size in physical pixels and its DPI scale factor. **Core-owned
/// display state** — the shell updates it on `Resized` / `ScaleFactorChanged` / `resumed`, so
/// orchestration methods never read `window.inner_size()` / `window.scale_factor()` directly
/// (which is what let the HUD-build + hit-rect methods move off the shell).
#[derive(Clone, Copy)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
}

/// The platform-neutral orchestration state the shell drives. Grows as step 5 relocates
/// field groups off the winit shell; the shell holds one `AppCore` and reaches its state
/// through `self.core.*` (fields are `pub` during the incremental move — they collapse
/// behind `handle(CoreEvent)` / accessors once the split is complete).
pub struct AppCore {
    /// The injected wall-clock "now" for this event/tick — **the core never calls
    /// `Instant::now()`** (NS0 5.5 / Phase 0.3). The shell stamps it at each event-loop entry
    /// (and `CoreEvent::Tick` carries the same instant), so all timing within one event uses a
    /// single consistent instant and unit tests can drive time deterministically.
    pub now: Instant,
    /// Surface geometry (physical size + DPI scale) for overlay sizing / hit-testing, kept
    /// current by the shell (NS0 5.5 / Phase 0.4) so the core never reads the window directly.
    pub viewport: Viewport,
    /// Physical keys currently held → the [`Action`] each resolved to at press time (the
    /// hold-to-fly / continuous-action set). OS key-repeat is ignored; focus loss clears it.
    pub held: HashMap<PbKey, Action>,
    /// When the current on-screen frame was presented — the anchor for the self-paced
    /// advance interval and the slideshow dwell deadline.
    pub last_present: Option<Instant>,
    /// The advance cadence cap (one frame per this interval), seeded to the monitor refresh.
    pub frame_interval: Duration,
    /// When the current nav key-hold began (drives the accelerating hold-to-fly ramp).
    pub hold_start: Option<Instant>,
    /// The tap-vs-hold delay before a held nav key starts flying.
    pub initial_delay: Duration,
    /// Slideshow state (on/off + dwell interval).
    pub slideshow: Slideshow,
    /// The current keyboard modifier state (the shell-neutral mirror of the OS modifiers).
    pub mods: Modifiers,
    /// Briefly guards Esc-to-quit after a modal (picker / dialog) closes, so its stray Esc
    /// leak can't also quit the app.
    pub esc_guard_until: Option<Instant>,

    // --- View / geometry (NS0 5.2) ---
    /// Decode-to-fit target = the display size; photos are downscaled to it.
    pub fit: Option<FitBox>,
    /// Per-photo view transform (scaling mode + rotation + zoom + pan).
    pub view: ViewTransform,
    /// Last cursor position in physical px — the pinch/wheel-zoom anchor and the
    /// drag-to-pan reference. `None` until the pointer first moves over the window.
    pub last_cursor: Option<[f32; 2]>,
    /// Whether the left mouse button is held — drives drag-to-pan.
    pub dragging: bool,
    /// Per-image in-RAM rotation overrides (`r` / `Shift+R`), applied as a GPU transform
    /// at draw; RAM-only until the user Saves (privacy #2). Absent = upright.
    pub rotations: HashMap<usize, Rotation>,
    /// Pinch-zoom gesture timing accumulators (start + last event).
    pub zoom_started: Option<Instant>,
    pub zoom_last: Option<Instant>,
    /// Two-finger pan gesture timing accumulators (start + last event).
    pub pan_started: Option<Instant>,
    pub pan_last: Option<Instant>,
    /// When a window resize/toggle has "settled" enough to re-decode at the new fit.
    pub resize_settle_at: Option<Instant>,
    /// When to persist the debounced window-geometry change (an explicit user action).
    pub geometry_save_at: Option<Instant>,

    // --- Metadata caches (NS0 5.3) ---
    /// Per-item info-panel metadata, memoized so the panel doesn't re-derive per frame.
    pub meta_cache: HashMap<usize, PhotoMeta>,
    /// The displayed photo's metadata (mirror of the `meta_cache` entry for the current item).
    pub current: Option<PhotoMeta>,
    /// Per-item on-demand full-EXIF read (`Shift+I`): `(mtime, key/value pairs)`, so a
    /// re-open of the same unchanged file is instant. RAM-only (privacy #2).
    pub exif_cache: HashMap<usize, (u64, Vec<(String, String)>)>,

    // --- Prefetch / decode / residency (NS0 5.3) ---
    /// Off-thread priority decode pool — decode + I/O never block the event loop.
    pub pool: DecodePool,
    /// Completed decodes, drained + uploaded during the tick.
    pub results: Receiver<Outcome>,
    /// Pure item↔slot residency mirror for the renderer's texture ring.
    pub ring: ResidentRing,
    /// Prefetch window: how many items to decode ahead of / behind the cursor.
    pub ahead: usize,
    pub behind: usize,
    /// Items whose decode failed (skipped / marked), so we don't retry them in a loop.
    pub failed: HashSet<usize>,
    /// Paths deleted this session — hidden from the playlist without a rescan.
    pub deleted: HashSet<PathBuf>,
    /// Items currently showing only their preview (a full-decode upgrade can replace it).
    pub preview_resident: HashSet<usize>,
    /// Completed decodes awaiting GPU upload — drained on the tick, never on the keypress
    /// frame (a keypress stays a rebind).
    pub pending_uploads: Vec<Outcome>,
    /// Items whose full-res decode turned out no better than the preview (e.g. a RAW whose
    /// only embedded image *is* its preview) — so we don't re-request their upgrade each tick.
    pub upgrade_done: HashSet<usize>,
    /// The last full-upgrade set (the "sharp ring") issued, so the idle pump diffs against it.
    pub last_upgrade_set: Vec<usize>,
    /// When a full-res upgrade was requested per item, to rate-limit re-requests.
    pub full_requested_at: HashMap<usize, Instant>,
    /// Live Photo pairing, memoized per item: `Some(path)` = companion motion `.mov`, `None`
    /// = not a Live Photo. Filled lazily only when settled on a photo; RAM-only (privacy #2).
    pub live_motion_cache: HashMap<usize, Option<PathBuf>>,

    /// Per-stage timing (decode/upload/render); disabled unless `--metrics` is passed.
    pub metrics: StageTimes,

    // --- Nav / playlist (NS0 5.3) ---
    /// The photo source (filesystem / ZIP / 7z) behind the current playlist.
    pub source: Arc<dyn PhotoSource>,
    /// Pure navigation state: cursor + precomputed shuffle order.
    pub playlist: Playlist,
    /// The current prefetch want-list (priority order), used as eviction `keep`.
    pub targets: Vec<usize>,
    /// The last navigation direction, so the slideshow auto-advances the way the user last
    /// moved (space → forward, backspace → back, enter → random). Updated on every advance.
    pub last_nav: Nav,
    /// What's currently on screen.
    pub displayed_item: Option<usize>,
    /// The item we're trying to show (== `displayed_item` once caught up).
    pub target_item: Option<usize>,
    /// Geometry generation; bumped on resize / fit toggle. Stale-epoch decodes are discarded
    /// so an old-size result can't land on screen.
    pub epoch: u64,
    /// The root the playlist was opened from — for showing paths relative to it.
    pub root: PathBuf,
    /// The directory the current playlist was scanned from (enables the `Ctrl+R` recursive
    /// toggle + re-scan). `None` for an explicit file list.
    pub scan_root: Option<PathBuf>,
    /// Whether the current scan-based playlist is recursive (`Ctrl+R` toggles).
    pub recursive: bool,

    // --- HUD / overlay state (NS0 5.3e; the Hud rasterizer stays shell-side for 5.4) ---
    /// Which info overlay is active (`i` basic / `Shift+I` full EXIF / `?` help / off).
    pub info: InfoMode,
    /// Whether the info panel is currently drawn.
    pub overlay_shown: bool,
    /// Which item the drawn panel was built for; when it differs from `displayed_item` the
    /// panel is stale and gets rebuilt (tracks the photo with no blank flash on single-step).
    pub overlay_item: Option<usize>,
    /// The current transient status toast (command feedback), or `None`.
    pub toast: Option<Toast>,
    /// When the current decode-wait started, for the delayed loading pie.
    pub wait_started: Option<Instant>,
    /// When the loading pie should finish its sweep.
    pub pie_finish: Option<Instant>,
    /// When the pie's completion glow started.
    pub pie_glow_started: Option<Instant>,
    /// EWMA of recent decode durations, to size the pie's expected sweep.
    pub decode_ewma: f32,
    /// Whether the pie was drawn this cycle (so it clears exactly once when done).
    pub pie_drawn: bool,
    /// The pie geometry last pushed to the renderer `(cx, cy, r)`, to skip redundant uploads.
    pub pie_pushed: Option<(f32, f32, f32)>,
    /// Signature of the last-built help/hint chip `(a, b, gen)`, to rebuild only on change.
    pub chip_sig: Option<(String, String, usize)>,
    /// When the current chip bitmap was built (for its fade/animation).
    pub chip_built: Instant,
    /// The chip's on-screen rect `[x, y, w, h]`, for hover hit-testing.
    pub chip_rect: Option<[f32; 4]>,
    /// Whether the pointer is over the chip.
    pub chip_hovered: bool,
    /// The empty-state open panel's geometry while shown, or `None`.
    pub open_panel: Option<OpenPanel>,
    /// Which empty-state open button the pointer is over, or `None`.
    pub open_hover: Option<OpenButton>,
    /// The interactive play hint riding the toast layer, or `None`.
    pub play_hint: Option<PlayHint>,

    // --- Rendering (NS0 5.4) ---
    /// The HUD text/overlay compositor (`pb-hud`), or `None` if no system font was found.
    /// CPU-rasterizes the info panel / toasts / pie / chip into RGBA bitmaps the renderer
    /// uploads as quads — behind a crate seam so a native-overlay backend can swap in later.
    pub hud: Option<Hud>,
    /// The GPU renderer, behind the [`Renderer`] trait object so backends are swappable and
    /// the core never names a concrete GPU type. `None` until the shell's window is created;
    /// then set to a concrete `WgpuRenderer` (boxed) built on that window's surface. The
    /// window itself stays shell-owned — the core drives rendering, the shell owns the OS
    /// handle it draws to.
    pub renderer: Option<Box<dyn Renderer>>,

    /// The undo stack of reversible user edits (Edit ▸ Undo / `Ctrl+Z`). RAM-only; cleared on
    /// a playlist/source change. The native "Undo" menu item stays shell-owned (a muda handle,
    /// enabled/disabled from this state).
    pub undo_stack: Vec<UndoAction>,

    // --- Animation / Live Photo playback (NS0 5.5 / Phase A4) ---
    /// Active animation playback (the frame cursor + timing), or `None` when showing a still.
    pub playback: Option<Playback>,
    /// When the current animation frame was shown (the per-frame dwell anchor).
    pub anim_frame_shown_at: Option<Instant>,
    /// An in-flight off-thread animation decode, or `None`.
    pub anim_decode: Option<AnimDecode>,
    /// An animation decoded ahead (eager prep) and held ready for instant playback.
    pub prepared: Option<Prepared>,
    /// Animation generation; bumped on navigate so a late decode for a past item is discarded.
    pub anim_gen: u64,
    /// The item the `▶ P` play hint was last shown for, so it isn't re-shown while parked.
    pub anim_hint_shown_for: Option<usize>,
    /// Frame-step hold timing (the `,`/`.` keys): when the hold started / last stepped.
    pub framestep_started: Option<Instant>,
    pub framestep_last: Option<Instant>,
    /// When to auto-revert a Live Photo's motion back to its still (after the clip plays once).
    pub live_revert_at: Option<Instant>,

    // --- Config (NS0 5.5 / Phase 0.5) ---
    /// The active keybindings (loaded from `keymap.toml`; editable via the shortcut editor).
    pub keymap: Keymap,
    /// Persisted user preferences (nav feel, defaults, saved window geometry). The hold loop
    /// reads them live; the Settings dialog edits + saves them.
    pub settings: Settings,

    // --- Effect sink (NS0 5.5 / Phase 0.1) ---
    /// Orchestration pushes [`CoreEffect`]s here instead of touching the OS directly; the
    /// shell's `drain_effects` executes them (the one place winit/muda/rfd/objc2 lives). Owned
    /// by the core so methods moved onto `impl AppCore` can emit effects without a threaded sink.
    pub effects: Vec<CoreEffect>,
}
