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

use crate::animation::{AnimDecode, AnimStream, Playback, Prepared};
use crate::contract::CoreEffect;
use crate::decode_pool::{DecodePool, Outcome};
use crate::keymap::Keymap;
use crate::launch::LaunchOverrides;
use crate::metrics::StageTimes;
use crate::overlay::{Panels, Toast, TreePanel};
use crate::settings::Settings;
use crate::undo::UndoAction;
use crate::{Action, Modifiers, PbKey, PhotoMeta, Slideshow};

/// The per-deck folder-counts cache entry: the deck identity it was built for
/// (root + length, so streaming batches refresh it) and the shared map —
/// `disk_counts` keyed by absolute folder path.
pub type FolderCounts = (PathBuf, usize, Arc<HashMap<PathBuf, u64>>);

/// The [`FsTree`](crate::fs_tree::FsTree)'s off-thread `read_dir` plumbing: the channel a
/// short-lived worker sends `(folder, subfolders)` back on (polled by `tick`) and the set
/// of folders whose read is in flight (so we don't re-kick). The `read_dir` runs off the
/// event loop so a dead share can't freeze a chevron. (The tree re-roots when the current
/// folder falls outside its root — no stored basis needed.)
pub struct FsTreeIo {
    pub tx: std::sync::mpsc::Sender<(PathBuf, Vec<PathBuf>)>,
    pub rx: Receiver<(PathBuf, Vec<PathBuf>)>,
    pub pending: HashSet<PathBuf>,
}

/// An archive deck's scoping state: the full source as opened, and the
/// forward-slashed internal-folder prefix the deck is currently scoped to
/// (`""` = the whole archive). The ⇧F tree's archive rows and the Go commands
/// re-scope by wrapping `full` in a `pb_source::ScopedSource` — never by
/// re-opening the archive file.
#[derive(Clone)]
pub struct ArchiveScope {
    pub full: Arc<dyn PhotoSource>,
    pub prefix: String,
}

/// A navigation move: forward (`space`/`→`), backward (`backspace`/`←`), a
/// precomputed-random jump (`enter`), or a step back through the random walk
/// (`shift+enter`). All are gated + self-paced + prefetchable the same way (random
/// walks a known shuffle order, so its next/prior targets are knowable — see
/// `pb_core::ShuffleOrder`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
/// A delete attempt deferred because the file's video reader was still retiring
/// (HEVC teardown blocks ~1 s off-thread) — retried a bounded number of times
/// from `tick`, never blocking the event loop (task #79 phase 7).
#[derive(Debug, Clone)]
pub struct DeleteRetry {
    pub at: Instant,
    pub item: usize,
    pub path: std::path::PathBuf,
    pub permanent: bool,
    pub tries_left: u32,
}

/// One item's memoized Details data (task #98).
///
/// Replaces the old `(u64, Vec<(String, String)>)` tuple. Two reasons, beyond the
/// obvious readability of a named field:
///
/// 1. The tuple's first element was **file size**, while the field's doc comment called
///    it `mtime` — a mismatch that survived precisely because a bare `u64` in a tuple
///    can't be wrong out loud.
/// 2. The track catalog needed somewhere to live, and a *parallel* `media_tracks` map
///    keyed by the same item would be a second source of truth to keep in sync (and to
///    forget to evict). One entry, one lifetime, one eviction.
#[derive(Debug, Clone, Default)]
pub struct ItemDetails {
    /// Exact byte size (from a stat, or the archive directory's hint for an entry).
    pub size: u64,
    /// The EXIF / basic-fact rows, as label→value pairs.
    pub fields: Vec<(String, String)>,
    /// The video's audio/subtitle tracks. `None` for stills, and for a video whose
    /// probe hasn't run or failed — which is *not* the same as "no tracks", hence the
    /// catalog's own [`pb_decode::TrackCompleteness`] once it is `Some`.
    pub media: Option<pb_decode::MediaTrackCatalog>,
    /// The basic probe's audio-presence fact, **independent of the catalog**. This is
    /// what lets a backend that can't enumerate tracks still say something true
    /// ("Present — details unavailable" vs "No"). Deriving it from the catalog instead
    /// would be circular: an un-enumerable set is empty either way, so the answer would
    /// always be whatever the emptiness happened to imply. `None` = not probed.
    pub has_audio: Option<bool>,
    /// How far the (async, video-only) container probe got. Stills are born `Ready`.
    pub probe_state: crate::media_details::ProbeState,
}

impl ItemDetails {
    /// A still's entry: read synchronously, so it is complete the moment it exists.
    pub fn ready(size: u64, fields: Vec<(String, String)>) -> Self {
        ItemDetails {
            size,
            fields,
            media: None,
            has_audio: None,
            probe_state: crate::media_details::ProbeState::Ready,
        }
    }

    /// The placeholder a video's cold miss records while its worker probes. Its presence
    /// is also what stops a second worker being spawned for the same item.
    pub fn loading() -> Self {
        ItemDetails {
            probe_state: crate::media_details::ProbeState::Loading,
            ..Default::default()
        }
    }
}

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
    /// A nav action held by the **pointer** (a toolbar nav/random button pressed and held) —
    /// a second, single-slot source of hold-to-fly alongside the keyboard's `held` map, so a
    /// mouse user can "blaze" by holding the ‹ › / shuffle button exactly like holding Space.
    /// `held_nav()` considers it; focus loss clears it (mouse-up normally does).
    pub pointer_nav: Option<Action>,
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
    /// Whether preference changes made as a side effect of normal use (the remembered
    /// `last_folder`) may be written to the real `settings.toml`. `true` on a live host
    /// (`new_host` / the winit shell), `false` in `headless` so unit tests that open
    /// decks never touch the user's config. Explicit Settings-dialog saves and the
    /// mute/geometry saves are unaffected (tests deliberately never drive those).
    pub persist_prefs: bool,
    /// The **OS** light/dark theme, kept current by the shell (initial read + the
    /// `OsThemeChanged` event). What `AppearanceMode::System` resolves to (task #46);
    /// defaults to dark, matching the pre-#46 always-dark HUD, until the shell reports.
    pub os_dark: bool,
    /// The dark/light flag the HUD + letterbox were last themed for
    /// ([`refresh_theme`](Self::refresh_theme)'s change detector, so a redundant OS
    /// report doesn't rebuild every overlay bitmap).
    pub hud_dark: bool,

    // --- View / geometry (NS0 5.2) ---
    /// Decode-to-fit target = the display size; photos are downscaled to it.
    pub fit: Option<FitBox>,
    /// Per-photo view transform (scaling mode + rotation + zoom + pan).
    pub view: ViewTransform,
    /// Last cursor position in physical px — the pinch/wheel-zoom anchor and the
    /// drag-to-pan reference. `None` until the pointer first moves over the window.
    pub last_cursor: Option<[f32; 2]>,
    /// The translucent-top-bar inset (physical px) last pushed to the renderer via
    /// [`set_content_top_inset`](AppCore::set_content_top_inset). Cached here so the
    /// macOS native-video placement (`video_placement`) can reproduce the still
    /// renderer's inset handling (`quad_vertices`) exactly. `0` = classic opaque bar.
    pub content_top_inset: u32,
    /// Whether the left mouse button is held — drives drag-to-pan.
    pub dragging: bool,
    /// Per-image in-RAM rotation overrides (`r` / `Shift+R`), applied as a GPU transform
    /// at draw; RAM-only until the user Saves (privacy #2). Absent = upright.
    pub rotations: HashMap<usize, Rotation>,
    /// Per-video last-watched positions, keyed by item index (task #94.2). Written
    /// when leaving a video far enough into a long-enough clip, read when returning
    /// to resume near where you left off (so a stray Space→next then Backspace lands
    /// you back). **RAM-only, session-scoped** — dropped on deck rebuild / Esc /
    /// quit like every other runtime cache, never persisted (a viewing trace; the
    /// ADR-018 privacy guarantee holds). Absent = start from 0.
    pub video_resume: HashMap<usize, Duration>,
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
    /// The live window presentation mode: `true` = a decorated window, `false` = borderless
    /// "windowed fullscreen" (the chrome-free speed mode). Flipped by the `Fullscreen` action;
    /// the shell applies the actual window ops via `SetWindowMode` (`apply_window_mode`) and reads
    /// this to decide direction. The persistent preference is `settings.fullscreen` (the inverse);
    /// this is the current runtime state. NS0 5.6.
    pub windowed: bool,

    // --- Metadata caches (NS0 5.3) ---
    /// Per-item info-panel metadata, memoized so the panel doesn't re-derive per frame.
    pub meta_cache: HashMap<usize, PhotoMeta>,
    /// The displayed photo's metadata (mirror of the `meta_cache` entry for the current item).
    pub current: Option<PhotoMeta>,
    /// Per-item on-demand Details read (`Shift+I`), so a re-open of the same file is
    /// instant. RAM-only (privacy #2). See [`ItemDetails`].
    pub exif_cache: HashMap<usize, ItemDetails>,
    /// The in-flight video Details probe, if any (task #98). One at a time: the Inspector
    /// shows one item, and replacing this drops the old receiver — which *is* the
    /// cancellation.
    pub details_probe: Option<crate::media_details::DetailsProbe>,
    /// Details generation: bumped with `text_gen` on index reassignment, so a probe that
    /// lands after a deck rebuild — when `item` names a different file — is dropped.
    /// **Not** [`AppCore::epoch`], which is the *geometry* generation (a window resize
    /// bumps it) and would both invalidate good catalogs and miss real rebuilds.
    pub details_gen: u64,
    /// Subtitle overlay state (task #90): the cue clock, the loaded track, and the
    /// bitmap + rect the shells composite. Shares `details_gen` as its staleness
    /// generation — both describe "what file is at index i", which is the same question.
    pub subtitles: crate::subtitle_engine::SubtitleEngine,
    /// Per-item "text in image" results (`T` / Copy Text from Image, task #45):
    /// on-device OCR lines + QR payloads, cached so a revisit is instant. RAM-only
    /// (privacy #2) — dropped on rebuild and exit, never written anywhere.
    pub recognized_text: HashMap<usize, crate::image_text::ImageText>,
    /// The in-flight off-thread text scan, or `None`. Polled by the tick
    /// (`poll_text_scan`); superseded by replacing it (the old receiver drops).
    pub text_scan: Option<crate::image_text::TextScan>,
    /// Text-scan generation: bumped whenever playlist indices are reassigned so a
    /// late scan result can't cache under a recycled index.
    pub text_gen: u64,
    /// Per-item AI description (task #44, `D` / Ask): the vision model's text, or a
    /// user-facing error message. Cached so a revisit is instant; RAM-only (privacy #2),
    /// dropped on rebuild and exit, never written anywhere.
    pub descriptions: HashMap<usize, Result<String, String>>,
    /// The in-flight off-thread describe job, or `None`. Polled by the tick
    /// (`poll_describe_scan`); superseded by replacing it (the old receiver drops).
    pub describe_scan: Option<crate::describe::DescribeScan>,
    /// Describe generation: bumped with `text_gen` on index reassignment so a late
    /// describe result can't cache under a recycled index.
    pub describe_gen: u64,

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
    /// Archive decks only: the **unscoped** just-opened source plus the active
    /// internal-folder scope (see [`ArchiveScope`]). `None` for disk decks.
    /// Re-scoping (⇧F tree click, Go commands) filters `full` through a
    /// `ScopedSource` instead of re-opening the file — a solid 7z's eager
    /// decode is paid once, and an unlocked encrypted archive stays unlocked.
    /// RAM-only (privacy #2).
    pub archive_scope: Option<ArchiveScope>,
    /// Pure navigation state: cursor + precomputed shuffle order.
    pub playlist: Playlist,
    /// The current prefetch want-list (priority order), used as eviction `keep`.
    pub targets: Vec<usize>,
    /// The last navigation direction, so the slideshow auto-advances the way the user last
    /// moved (space → forward, backspace → back, enter → random). Updated on every advance.
    pub last_nav: Nav,
    /// What's currently on screen.
    pub displayed_item: Option<usize>,
    /// The geometry [`epoch`](Self::epoch) at which [`displayed_item`](Self::displayed_item)
    /// was last **resolved** — presented, or terminally failed — at the *current* fit.
    /// Readiness ("caught up") is `displayed_item == target_item && presented_epoch ==
    /// Some(epoch)`, so a geometry change (which bumps `epoch`) marks the on-screen frame
    /// stale even when the item index is unchanged, forcing an async re-present at the new
    /// fit instead of silently keeping the old-scale frame (task #18 finding #5).
    pub presented_epoch: Option<u64>,
    /// The item we're trying to show (== `displayed_item` once caught up).
    pub target_item: Option<usize>,
    /// Flicker-compare pin (task #43): the pinned photo's playlist index. `Y` flips
    /// between it and the current photo; the pin rides the prefetch want-list at
    /// top-2 priority so the flip is always a ring rebind, never a decode. RAM-only
    /// (privacy #2) — a pin is a viewing trace; it dies with the session.
    pub compare_pin: Option<usize>,
    /// Where the `Y` flip returns to from the pin — the last non-pin position the
    /// user flipped away from. Index-keyed and transient (dropped on rebuild).
    pub compare_return: Option<usize>,
    /// The pinned item's identity (full path, or archive-entry name) so a playlist
    /// rebuild (delete-advance, recursive toggle) re-resolves the pin — and a
    /// genuinely new deck, which can't match, clears it.
    pub compare_pin_id: Option<String>,
    /// A zoom/pan staged by `compare_jump`, consumed by the very next `view_for` so
    /// the flip's FIRST presented frame already carries the view — presenting at the
    /// reset (fit) view and re-imposing afterwards drew two frames and flashed the
    /// incoming photo centered for one of them (owner-reported flicker).
    pub compare_carry: Option<(f32, [f32; 2])>,
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
    /// Whether a directory scan is streaming the playlist in (the core-owned mirror of the
    /// shell's `dir_scan.is_some()`; the shell keeps it in sync at every `dir_scan` mutation).
    /// While set, `request_prefetch` uses the sequential-only, no-wrap prefetch so the random
    /// look-ahead doesn't thrash against the deck regenerating on each batch (NS0 5.5 Phase B).
    pub scanning: bool,
    /// Whether a launch input (an archive) is still deferred, waiting for the window to exist
    /// (the core-owned mirror of the shell's `pending_launch.is_some()`, kept in sync at its
    /// mutations). Suppresses the "Press O to open" empty-state hint until the launch resolves.
    pub launching: bool,
    /// Whether a chrome dialog (Settings / About / a progress card) is open — the core-owned
    /// mirror of the shell's `dialog.is_some()`, synced at each tick. The slideshow pauses while
    /// a dialog is up (host's own modal picker pauses itself). NS0 5.5 Phase C2 (the Tick loop).
    pub dialog_open: bool,
    /// Whether a background archive open is still decompressing (the mirror of the shell's
    /// `archive_load.is_some()`, synced at each tick) — keeps `work_pending` true so the loop
    /// keeps polling for the finished open. NS0 5.5 Phase C2.
    pub archive_loading: bool,
    /// The last `draw()` was **dropped** by the surface (Lost/Outdated/Timeout — routine
    /// during window-resize/fullscreen churn): nothing reached the screen, so the stale
    /// frame is still composited. Keeps `work_pending` true so the host pump stays awake,
    /// and `tick` retries the draw next frame (2026-07-04, the Mac "unfilled background
    /// after a fullscreen toggle" bug — a one-shot render with no retry).
    pub redraw_pending: bool,
    /// Whether the in-flight directory scan has applied its first non-empty batch (the first photo
    /// is shown). The `ScanBatch` handler bootstraps the playlist while this is false, then extends
    /// it; the host reads it to gate the Scanning-dialog reveal / the scan-count chip. Reset to
    /// false when a new scan begins. NS0 5.6 Step 3 (was the shell `DirScan::bootstrapped`).
    pub scan_bootstrapped: bool,
    /// A delete whose playlist-advance is deferred: `(fire_at, removed_index)`. The deleted
    /// photo stays on screen with its icon until `fire_at`, then the playlist drops the item and
    /// advances (`do_delete` sets it; `flush_pending_delete` fires it). RAM-only (privacy #2).
    pub pending_delete: Option<(Instant, usize)>,
    /// The item awaiting a permanent-delete confirmation: set when the (themed egui) confirm
    /// dialog opens, consumed when it answers Yes. The dialog itself stays shell-owned.
    pub pending_confirm_delete: Option<usize>,
    /// The archive path awaiting a password (pure data): set when the password prompt opens,
    /// carried so a submit re-opens that archive and a cancel/dismiss forgets it. The prompt +
    /// the worker spawn stay shell-owned; this is just the pending target. NS0 5.6.
    pub password_archive: Option<std::path::PathBuf>,

    // --- HUD / overlay state (NS0 5.3e; the Hud rasterizer stays shell-side for 5.4) ---
    /// The basic `i` info line (filename · resolution · format): the **ephemeral**
    /// glanceable readout, fully decoupled from the rich panels (task #54) — its own
    /// permanent bottom-right layer, so it coexists with a rich panel (which lifts
    /// above its strip) and is unaffected by `Tab`/`panels.hidden`. This is the
    /// user's on/off preference; `info_line_shown` is the actual drawn state.
    pub info_line: bool,
    /// Whether the info line is currently *drawn* (hidden while flying / with no
    /// photo, even when `info_line` is on) — mirrors `overlay_shown`.
    pub info_line_shown: bool,
    /// Which item the drawn info line was built for; a mismatch with `displayed_item`
    /// means it's stale and the tick rebuilds it (mirrors `overlay_item`).
    pub info_line_item: Option<usize>,
    /// The physical-px size of the last-rasterized info-line bitmap. Height drives
    /// how far a colliding panel reserves; width + the alignment give the line's
    /// horizontal span, so a panel reserves only when it actually overlaps the line
    /// (a wide centered line on a narrow window can hit both corner panels).
    pub info_line_w: u32,
    pub info_line_h: u32,
    /// Rich-panel open/visibility state (task #54): the tabbed Inspector
    /// (Details/Text/Describe), Help, and the `Tab` master-visibility flag.
    pub panels: Panels,
    /// The host presents the **Help** panel natively (task #54, mac-first): the core
    /// then suppresses Help's HUD rasterization and emits [`CoreEffect::PanelsChanged`]
    /// on visibility/content change so the shell pulls the model. Default `false` (the
    /// winit shell keeps the CPU-quad Help). The macOS host sets it at construction.
    /// The tree and Inspector follow with their own flags as they go native.
    pub native_help: bool,
    /// The last Help visibility the tick emitted a [`CoreEffect::PanelsChanged`] for,
    /// so the marker fires only on a real show/hide (not per tick).
    pub last_help_visible: bool,
    /// The host presents the **empty-state Open panel** natively (task #54, mac-first):
    /// the "no photos — open a file/folder" welcome surface. When set, the core
    /// suppresses its HUD rasterization (so its buttons aren't hit-tested beneath a
    /// native panel — fixes the cursor-through-Help glitch) and signals visibility via
    /// [`CoreEffect::PanelsChanged`]. Default `false` (winit keeps the HUD panel).
    pub native_open: bool,
    /// The last empty-state visibility the tick signalled a marker for.
    pub last_open_visible: bool,
    /// The host presents the **Inspector** (Details/Text/Describe tabs) natively (task
    /// #54, mac-first): the core suppresses those tabs' HUD rasterization and signals via
    /// [`CoreEffect::PanelsChanged`] on visibility / tab / content change — so async OCR
    /// and describe results re-signal the host to re-pull. Default `false` (winit keeps
    /// the CPU panel). The macOS host sets it at construction.
    pub native_inspector: bool,
    /// The last Inspector snapshot the tick emitted a marker for — visibility + tab +
    /// content — so the marker fires on a real change (open/close, tab switch, or an
    /// async result landing), never per tick. `None` = the Inspector was hidden.
    pub last_inspector_snap: Option<crate::panels::InspectorSnapshot>,
    /// The host presents the **Thumbnails strip** (task #83) natively: gates the
    /// `Shift+T` toggle so a shell without a strip presenter (the winit shell,
    /// until its egui parity phase) never enables capture / fills for a strip it
    /// can't draw. The macOS host sets it at construction.
    pub native_thumbs: bool,
    /// The host presents the **folder tree** (⇧F) natively (task #54, mac-first): the
    /// core skips rasterizing the tree quad and instead stores the derived rows/targets
    /// for the shell to read, signalling [`CoreEffect::PanelsChanged`] on change. A
    /// native list scrolls, so the HUD's windowing / "… n more" paging is dropped.
    /// Default `false` (winit keeps the CPU tree). The macOS host sets it at construction.
    pub native_tree: bool,
    /// The last folder-tree visibility the tick emitted a marker for.
    pub last_tree_visible: bool,
    /// Whether the info panel is currently drawn.
    pub overlay_shown: bool,
    /// Which item the drawn panel was built for; when it differs from `displayed_item` the
    /// panel is stale and gets rebuilt (tracks the photo with no blank flash on single-step).
    pub overlay_item: Option<usize>,
    /// The current transient status toast (command feedback), or `None`.
    pub toast: Option<Toast>,
    /// When set, toasts are **not** rasterized to the HUD layer — the core holds their data
    /// (`toast_native`) for the shell to render natively (macOS SwiftUI pill). The winit shell
    /// leaves this false and keeps the CPU-composited toast.
    pub native_toast: bool,
    /// When set, the one-line **info readout** (`i`) is not rasterized to the HUD — the shell
    /// draws it natively (macOS SwiftUI, or the winit egui overlay). The core still owns the
    /// toggle state + content string.
    pub native_info: bool,
    /// The last info-line snapshot the tick emitted a marker for (main text, codec, live,
    /// animated) — so a natively-drawn info line re-pulls when its content changes (a photo
    /// swap), never per tick. `None` = the info line was hidden.
    pub last_info_snap: Option<(String, String, bool, bool)>,
    /// When set, the **play hint** (▶/Live Photo, on a motion item) is not rasterized to the
    /// HUD — the shell draws it natively and owns its fade/hover. The core only signals *when*
    /// to flash it (`play_hint_seq`) and *what* it is (`play_hint_kind`).
    pub native_play: bool,
    /// Monotonic counter bumped each time a fresh motion item settles — the native shell keys
    /// its play-hint flash off a change here (the "show once per item" trigger).
    pub play_hint_seq: u64,
    /// The current native toast's data (message + icon + timing), when `native_toast`.
    pub toast_native: Option<crate::overlay::NativeToast>,
    /// Monotonic toast counter — the shell keys its entrance animation off it, so an identical
    /// message firing twice still re-animates.
    pub toast_seq: u64,
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
    /// Whether the folder-tree overlay (`Shift+F`) is open. RAM-only (privacy #2) —
    /// the tree is a view of the deck, never persisted.
    pub folder_tree_open: bool,
    /// Signature of the drawn tree (`root|current-folder`), so it rebuilds only when
    /// the current folder or the deck changes — never per frame, never per photo
    /// within the same folder. `None` = nothing drawn.
    pub folder_tree_sig: Option<String>,
    /// The drawn tree's interactive state (hit rects, click targets, cached rows,
    /// hover, page), or `None` while nothing is drawn. RAM-only.
    pub folder_tree_panel: Option<TreePanel>,
    /// Per-deck folder-counts cache (`disk_counts` keyed by root + deck length):
    /// the count badges AND the no-I/O flight derivation read it, so neither ever
    /// re-walks the playlist while the deck is unchanged. RAM-only.
    pub folder_tree_counts: Option<FolderCounts>,
    /// The in-flight tree-io worker (the settled `read_dir` derivation, or a Go
    /// sibling lookup): the tree's disk I/O runs off-thread so a dead share can't
    /// stall the event loop. `tick` polls it; `work_pending` keeps the loop ticking
    /// while it runs; a new job supersedes the old by dropping its receiver.
    /// (`pub` only because the shells build `AppCore` as a struct literal.)
    pub tree_io: Option<crate::folder_tree::TreeIo>,
    /// The last folder **Open Parent** (⌘↑) climbed to, so repeated presses walk up one
    /// level at a time from *there* — not from the current photo's folder, which always sits
    /// at the deepest level (a parent with no direct photos re-lands it there), so a naive
    /// current-folder anchor would get stuck oscillating. Cleared by any other open (via
    /// `open_plan`); `None` restarts the climb from the current folder. RAM-only.
    pub climb_anchor: Option<PathBuf>,
    /// The **Finder-style** resident folder browser (task #54, native path only): a
    /// persistent, lazily-populated [`FsTree`](crate::fs_tree::FsTree) that decouples
    /// browsing (expand/collapse) from loading photos, built for disk decks when
    /// `native_tree` is set. RAM-only. `None` for archive/empty decks (they keep the v1
    /// [`folder_tree_panel`](Self::folder_tree_panel)) and for the winit shell.
    pub fs_tree: Option<crate::fs_tree::FsTree>,
    /// The `FsTree`'s off-thread `read_dir` channel + in-flight set + the deck root it was
    /// built for (a change re-roots it). Created with the tree; dropped when it clears.
    pub fs_tree_io: Option<FsTreeIo>,
    /// The left pane's active tab (task #83): Folders (the tree) or Thumbnails.
    /// `folder_tree_open` stays the pane's open flag; this picks its content.
    pub left_tab: crate::overlay::LeftTab,
    /// Thumbnails-strip state (task #83): the RAM-only thumb cache + derive
    /// thread + auto-follow. Zero cost until the panel is first opened.
    pub thumbs: crate::thumbs::Thumbs,

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
    /// An in-flight **streaming** Live Photo motion decode (task #69) — frames play as they
    /// arrive. Set on every platform with a motion decoder: the Linux FFmpeg, macOS
    /// AVAssetReader, and Windows Media Foundation paths.
    pub anim_stream: Option<AnimStream>,
    /// Active **video** playback: the [`ActiveVideoBackend`](crate::video_native::ActiveVideoBackend)
    /// for the item on screen. `Session` (Windows/Linux) drives the streaming
    /// `VideoSession` + a decode producer; `Native` (macOS) is a passive mirror of
    /// the shell's `AVPlayer` (task 79.9). Entirely separate from `playback`
    /// (animations retain frames; video is forward-only with constant memory).
    pub video: Option<crate::video_native::ActiveVideoBackend>,
    /// Monotonic source of `VideoSessionId`s — a fresh id per session so straggler
    /// frames from a torn-down producer can never touch a newer session.
    pub video_seq: u64,
    /// macOS dual-backend (task #84 §8a level 2): the item whose `AVPlayer` just
    /// failed with a *classified* (recoverable) error — the very next
    /// `start_video_session` for it routes to the FFmpeg session instead, then
    /// the flag is consumed (a fresh open later retries native first). Never a
    /// loop: a session failure surfaces the error, it never re-arms this.
    pub video_ffmpeg_fallback: Option<usize>,
    /// macOS archive-video handoff: the in-RAM container bytes for the session the core
    /// just emitted `PlayVideoBytes` for, stashed for the shell to pull once
    /// (`take_pending_video_bytes`) and serve to `AVPlayer` via a resource loader. RAM-only,
    /// never written to disk (privacy #2). `None` except in the brief emit→pull window.
    pub pending_video_bytes: Option<Vec<u8>>,
    /// macOS archive-video **poster** handoff: container bytes stashed per request id for the
    /// shell to pull (`take_pending_poster_bytes`), generate a poster frame with
    /// `AVAssetImageGenerator`, and feed back via `video_poster_ready`. RAM-only. Keyed by id
    /// so multiple prefetch posters can be in flight. See the plan doc.
    pub pending_poster_bytes: std::collections::HashMap<u64, Vec<u8>>,
    /// Monotonic poster-request id (pairs a `RequestVideoPoster` with its `video_poster_ready`).
    pub poster_req_seq: u64,
    /// Archive-video items with a poster request **in flight** (macOS), so the tick doesn't
    /// re-request while one is pending. Cleared when `video_poster_ready` lands (or the deck
    /// changes) — so a revisit after ring eviction re-requests and regenerates the poster.
    pub poster_inflight: std::collections::HashSet<usize>,
    /// Off-thread archive-video poster byte reads (macOS): a worker reads the entry's bytes
    /// (a ZIP inflate mustn't block the event loop, esp. when prefetching several ahead) and
    /// sends `(request_id, item, bytes)`; the tick drains this, stashes the bytes, and emits
    /// `RequestVideoPoster`. Empty bytes = a read error (the tick clears the in-flight guard).
    pub poster_read_tx: std::sync::mpsc::Sender<(u64, usize, Vec<u8>)>,
    pub poster_read_rx: Receiver<(u64, usize, Vec<u8>)>,
    /// Off-thread archive-video **playback** byte read (macOS): pressing `P` on an archive
    /// entry reads its container bytes on a worker (a large ZIP inflate mustn't block the
    /// event loop), then the tick — if the session is still current — stashes them and emits
    /// `PlayVideoBytes`. Payload: `(session_id, entry_name, muted, bytes)`; empty = read error.
    pub video_read_tx:
        std::sync::mpsc::Sender<(crate::video::VideoSessionId, String, bool, Vec<u8>)>,
    pub video_read_rx: Receiver<(crate::video::VideoSessionId, String, bool, Vec<u8>)>,
    /// Last held-key video seek step (task #79 phase 6): the app's own repeat
    /// timer (OS key-repeat stays ignored, like every other held action).
    pub video_seek_last: Option<Instant>,
    /// A delete blocked by a still-retiring video reader, awaiting its bounded
    /// retry (task #79 phase 7 — the delete-while-playing case).
    pub pending_delete_retry: Option<DeleteRetry>,
    /// The text currently shown in the video position pill (`m:ss / m:ss`), so it
    /// re-rasterizes only when the second ticks over — not per frame.
    pub video_pill_text: Option<String>,
    /// While set, the info line is flashed as the video seek/step OSD (the user's
    /// `i` toggle is off, but the playback row is the position readout — it
    /// replaces the old `m:ss / m:ss` toast). Cleared by `tick` at the deadline.
    pub video_osd_until: Option<Instant>,
    /// A geometry change (resize / scale mode) landed while a video owned the
    /// display: the re-decode + ring refill were deferred (they'd saturate the
    /// pool mid-playback); `stop_video` re-issues the prefetch.
    pub video_geometry_stale: bool,
    /// A resize in flight paused the playback (freeze together, resume
    /// together — the modal drag loop stalls the presenter while audio would
    /// play on); the resize-settle arm resumes it.
    pub video_paused_by_resize: bool,
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
    /// reads them live; the Settings dialog edits + saves them. **Kept pristine** — CLI launch
    /// overrides never fold in here, so a save never leaks them to disk (task #78 / privacy #2).
    pub settings: Settings,
    /// Session-only CLI launch overrides (task #78), applied by [`AppCore::apply_launch_overrides`].
    /// Stored so the two settings read *live* (`--theme`, `--mute`) resolve through
    /// [`AppCore::effective_appearance`] / [`AppCore::effective_mute`] without mutating (and thus
    /// persisting) [`Self::settings`]. Default = no overrides.
    pub launch: LaunchOverrides,

    // --- Effect sink (NS0 5.5 / Phase 0.1) ---
    /// Orchestration pushes [`CoreEffect`]s here instead of touching the OS directly; the
    /// shell's `drain_effects` executes them (the one place winit/muda/rfd/objc2 lives). Owned
    /// by the core so methods moved onto `impl AppCore` can emit effects without a threaded sink.
    pub effects: Vec<CoreEffect>,
}
