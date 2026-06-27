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
//!   ← ↑ ↓ →     pan around the photo (hold; accelerates)
//!   = / -       zoom in / out (hold; accelerates; numpad +/- too)
//!   8 / 9       scaling mode: fit / fill (all prefetched)
//!   0           toggle original 1:1 ↔ fit
//!   r / Shift+R rotate 90° clockwise / counter-clockwise (per-image, RAM-only)
//!   i / Shift+I info panel (path · WxH · codec) / full-EXIF "nerd" panel
//!   / or ?      keybindings help overlay
//!   esc         quit
//!
//! Usage:
//!   cargo run -p pb-app --release -- "D:\Media\Pictures\2003\Halloween"
//!   cargo run -p pb-app --release -- "D:\Media\Pictures" -r          # recurse subfolders
//!   cargo run -p pb-app --release -- "D:\Media\Pictures" -r --windowed --metrics

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use pb_core::{prefetch_targets, Playlist, ResidentRing};
use pb_decode::{
    decode_image_file, is_supported_extension, read_exif_fields, DecodedImage, FitBox,
};
use pb_render::{
    test_pattern, Renderer, Rotation, ScaleMode, ViewTransform, WgpuRenderer, MAX_ZOOM, MIN_ZOOM,
};

mod decode_pool;
mod hud;
mod metrics;
use decode_pool::{recommended_workers, DecodeFn, DecodePool, Outcome};
use hud::{Hud, Row};
use metrics::StageTimes;

/// VRAM budget for the resident texture ring (~1.5 GB → ~16–32 fit-size slots on
/// a 7680-wide display, far more on smaller ones). Capacity is clamped to [4, 64].
const RING_BUDGET_BYTES: u64 = 1_500_000_000;
/// Cap on decoded-but-not-yet-uploaded bytes held by the pool (backpressure).
const POOL_BUDGET_BYTES: usize = 512 * 1024 * 1024;
/// Max slot uploads performed per `about_to_wait` tick, so a burst of finished
/// decodes can't blow the frame budget.
const UPLOADS_PER_TICK: usize = 2;

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
/// share the single overlay quad, so they're mutually exclusive.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InfoMode {
    Off,
    Basic,
    Full,
    Help,
}

/// Build a photo's info panel data from its path + decoded image.
fn meta_for_path(path: &Path, root: &Path, img: &DecodedImage) -> PhotoMeta {
    let rel = match path.strip_prefix(root) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string(),
    };
    PhotoMeta {
        rel,
        w: img.orig_width,
        h: img.orig_height,
        codec: img.codec,
    }
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

struct App {
    windowed: bool,
    paths: Vec<Arc<Path>>,
    playlist: Playlist,
    active: Option<Active>,
    /// Physical keys currently held (OS auto-repeat ignored).
    held: HashSet<KeyCode>,
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
    /// Hold timers for the zoom/pan acceleration ramps (start = when the hold
    /// began; last = previous step, for time-based deltas).
    zoom_started: Option<Instant>,
    zoom_last: Option<Instant>,
    pan_started: Option<Instant>,
    pan_last: Option<Instant>,
}

impl App {
    fn new(windowed: bool, root: PathBuf, paths: Vec<PathBuf>, metrics: StageTimes) -> Self {
        let paths: Vec<Arc<Path>> = paths.into_iter().map(Arc::from).collect();
        let playlist = Playlist::new(paths.len(), 0);
        let decode: Arc<DecodeFn> = Arc::new(|p: &Path, fit| decode_image_file(p, fit));
        let (pool, results) = DecodePool::new(recommended_workers(), POOL_BUDGET_BYTES, decode);
        Self {
            windowed,
            paths,
            playlist,
            active: None,
            held: HashSet::new(),
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
            zoom_started: None,
            zoom_last: None,
            pan_started: None,
            pan_last: None,
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

    /// Recompute the prefetch want-list and hand it to the decode pool. Items
    /// already resident are not re-requested.
    fn request_prefetch(&mut self) {
        self.targets = prefetch_targets(&self.playlist, self.ahead, self.behind);
        let fit = self.decode_fit();
        // Items already decoded and waiting to upload must not be re-requested:
        // the pool no longer tracks them, so it would decode them a second time.
        let pending: HashSet<usize> = self.pending_uploads.iter().map(|o| o.key.item).collect();
        let jobs: Vec<(usize, Arc<Path>, Option<FitBox>)> = self
            .targets
            .iter()
            .filter(|&&t| {
                self.ring.slot_for(t).is_none()
                    && !self.failed.contains(&t)
                    && !pending.contains(&t)
            })
            .map(|&t| (t, self.paths[t].clone(), fit))
            .collect();
        self.pool.set_targets(self.epoch, &jobs);
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
        self.draw(event_loop);
    }

    /// Show ring `slot` (holding `item`): the keypress fast path — a rebind, no
    /// decode or upload. Updates the pin, title, and info panel.
    fn present_item(&mut self, item: usize, slot: usize, event_loop: &ActiveEventLoop) {
        let view = self.view_for(item);
        let title = title_for(&self.paths[item], item, self.paths.len());
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
            self.displayed_item = Some(item);
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
        ready.retain(|o| {
            if o.key.epoch != self.epoch || self.ring.slot_for(o.key.item).is_some() {
                return false; // stale geometry or already resident
            }
            if let Err(ref e) = o.result {
                let item = o.key.item;
                eprintln!("decode failed for item {item}: {e}");
                self.failed.insert(item);
                // Unstick the gated loop: a corrupt target counts as "shown".
                if self.target_item == Some(item) {
                    self.displayed_item = Some(item);
                }
                return false;
            }
            true
        });

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
            if uploads >= UPLOADS_PER_TICK {
                // Carry still-wanted leftovers to the next tick (in priority order);
                // drop now-obsolete ones so they don't pin pool byte-budget while
                // the loop idles (work_pending wouldn't keep polling for them).
                if self.targets.contains(&item) && self.ring.slot_for(item).is_none() {
                    leftover.push(outcome);
                }
                continue;
            }
            let Ok(ref img) = outcome.result else {
                continue; // errors were already filtered out above
            };
            if !self.meta_cache.contains_key(&item) {
                let m = meta_for_path(&self.paths[item], &self.root, img);
                self.meta_cache.insert(item, m);
            }
            let item_bytes = img.pixels.len() as u64;
            if let Some(res) = self
                .ring
                .reserve_bytes(item, self.epoch, item_bytes, &self.targets)
            {
                if let Some(a) = self.active.as_mut() {
                    let t0 = Instant::now();
                    a.renderer
                        .upload_slot(res.slot, &img.pixels, img.width, img.height);
                    self.metrics.record("upload", t0.elapsed());
                }
                self.ring.mark_resident(item, res.slot, self.epoch);
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
        let decoded = decode_image_file(&self.paths[idx], self.decode_fit());
        self.metrics.record("decode", t0.elapsed());
        match decoded {
            Ok(img) => {
                let meta = meta_for_path(&self.paths[idx], &self.root, &img);
                self.current = Some(meta.clone());
                self.meta_cache.insert(idx, meta);
                let view = self.view_for(idx);
                let title = title_for(&self.paths[idx], idx, self.paths.len());
                if let Some(a) = self.active.as_mut() {
                    a.renderer.set_view(view);
                    a.renderer.set_image(&img.pixels, img.width, img.height);
                    a.renderer.set_overlay(None, 0);
                    a.window.set_title(&title);
                }
                self.overlay_shown = false;
                self.overlay_item = None;
                self.displayed_item = Some(idx);
            }
            Err(e) => {
                eprintln!("decode failed: {}: {e}", self.paths[idx].display());
                self.failed.insert(idx);
                // Keep the gate unstuck: count the bad file as "shown" so the next
                // navigation isn't dropped by the caught-up guard in `advance`.
                self.displayed_item = Some(idx);
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

    /// Push the current view transform to the renderer (re-places the quad).
    fn push_view(&mut self) {
        let view = self.view;
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_view(view);
        }
    }

    /// Decode the first image at the display size for an instant first frame.
    fn initial_image(&mut self) -> (Vec<u8>, u32, u32, String) {
        match self.playlist.current() {
            Some(idx) => match decode_image_file(&self.paths[idx], self.decode_fit()) {
                Ok(img) => {
                    let meta = meta_for_path(&self.paths[idx], &self.root, &img);
                    self.current = Some(meta.clone());
                    self.meta_cache.insert(idx, meta);
                    let title = title_for(&self.paths[idx], idx, self.paths.len());
                    (img.pixels, img.width, img.height, title)
                }
                Err(e) => {
                    eprintln!("decode failed: {}: {e}", self.paths[idx].display());
                    self.current = None;
                    let p = test_pattern(1600, 1000);
                    (p, 1600, 1000, "PhotoBlaze (decode error)".to_string())
                }
            },
            None => {
                self.current = None;
                let p = test_pattern(1600, 1000);
                (p, 1600, 1000, "PhotoBlaze (no images)".to_string())
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

    /// The keybindings help table: a title row, then every hotkey → action as a
    /// shaded-key / description pair. Static (independent of the current photo).
    fn help_rows(&self) -> Vec<Row> {
        let mut rows = vec![Row::Span {
            text: "PhotoBlaze — Keys".to_string(),
            bold: true,
        }];
        let keys: &[(&str, &str)] = &[
            ("Space", "Next photo"),
            ("Backspace", "Previous photo"),
            ("Hold nav key", "Fly through photos"),
            ("← ↑ ↓ →", "Pan (hold to accelerate)"),
            ("= / -", "Zoom in / out (hold)"),
            ("8", "Fit to screen"),
            ("9", "Fill screen (crop)"),
            ("0", "Toggle original 1:1 ↔ fit"),
            ("r / Shift+R", "Rotate 90° cw / ccw"),
            ("i / Shift+I", "Info / full-EXIF panel"),
            ("/ or ?", "This help"),
            ("Esc", "Quit"),
        ];
        rows.extend(keys.iter().map(|(k, d)| Row::Pair {
            label: k.to_string(),
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
        let path = &self.paths[item];
        let mut rows = Vec::new();
        // Identity header: filename (bold) over its folder (the filename is already
        // shown above, so the path row is the parent directory only).
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        rows.push(Row::Span {
            text: filename,
            bold: true,
        });
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            rows.push(Row::Span {
                text: dir.display().to_string(),
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
        if let Ok(md) = std::fs::metadata(path) {
            rows.push(Row::Pair {
                label: "File Size".to_string(),
                value: format!("{} bytes", hud::format_thousands(md.len())),
            });
        }
        if let Ok(bytes) = std::fs::read(path) {
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

    /// Advance one photo. In Fit mode this is the gated engine path (present on a
    /// ring hit, else hold + prefetch); Original mode decodes synchronously.
    fn advance(&mut self, forward: bool, event_loop: &ActiveEventLoop) {
        // Never advance while the previous target is still pending (a miss in
        // flight): a fast second press would overwrite it and skip that photo.
        // Holding still flies — `about_to_wait` re-advances once it's caught up.
        if self.displayed_item != self.target_item {
            return;
        }
        if forward {
            self.playlist.next();
        } else {
            self.playlist.prev();
        }
        self.target_item = self.playlist.current();
        // Both modes use the async engine: present on a ring hit, else hold the
        // previous frame while the decode (fit-sized or full-res) lands.
        self.try_present_target(event_loop);
        self.request_prefetch();
    }

    /// Which way we're currently paging, from held keys (both/neither = idle).
    /// Arrows are pan now, so only space/backspace advance.
    fn held_direction(&self) -> Option<bool> {
        let fwd = self.held.contains(&KeyCode::Space);
        let bwd = self.held.contains(&KeyCode::Backspace);
        match (fwd, bwd) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
    }

    /// Zoom direction from held keys: `+1` in (`=`/`+`/numpad+), `-1` out
    /// (`-`/numpad-), `None` if neither or both.
    fn zoom_held(&self) -> Option<f32> {
        let zin = self.held.contains(&KeyCode::Equal) || self.held.contains(&KeyCode::NumpadAdd);
        let zout =
            self.held.contains(&KeyCode::Minus) || self.held.contains(&KeyCode::NumpadSubtract);
        match (zin, zout) {
            (true, false) => Some(1.0),
            (false, true) => Some(-1.0),
            _ => None,
        }
    }

    /// Pan velocity direction from held arrows (image-space; positive pan reveals
    /// the right/bottom). Diagonals combine. `(0, 0)` if no arrow is held.
    fn pan_held(&self) -> (f32, f32) {
        let mut x = 0.0;
        let mut y = 0.0;
        if self.held.contains(&KeyCode::ArrowLeft) {
            x += 1.0;
        }
        if self.held.contains(&KeyCode::ArrowRight) {
            x -= 1.0;
        }
        if self.held.contains(&KeyCode::ArrowUp) {
            y += 1.0;
        }
        if self.held.contains(&KeyCode::ArrowDown) {
            y -= 1.0;
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
        self.displayed_item != self.target_item
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
        attrs = if self.windowed {
            attrs.with_inner_size(PhysicalSize::new(1280, 800))
        } else {
            attrs
                .with_decorations(false)
                .with_fullscreen(Some(Fullscreen::Borderless(None)))
        };

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );

        self.scale_factor = window.scale_factor() as f32;
        let isz = window.inner_size();
        self.fit = Some(FitBox {
            max_width: isz.width.max(1),
            max_height: isz.height.max(1),
        });

        // Decode the first image at the display size while the window is hidden.
        let (rgba, iw, ih, title) = self.initial_image();
        window.set_title(&title);

        let mut renderer = WgpuRenderer::new(
            window.clone(),
            isz.width.max(1),
            isz.height.max(1),
            &rgba,
            iw,
            ih,
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
                let decoded = decode_image_file(&self.paths[idx], self.decode_fit());
                self.metrics.record("decode", t0.elapsed());
                if let Ok(img) = decoded {
                    let meta = meta_for_path(&self.paths[idx], &self.root, &img);
                    self.current = Some(meta.clone());
                    self.meta_cache.insert(idx, meta);
                    renderer.set_image(&img.pixels, img.width, img.height);
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
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                let new_fit = FitBox {
                    max_width: size.width.max(1),
                    max_height: size.height.max(1),
                };
                if Some(new_fit) != self.fit {
                    self.fit = Some(new_fit);
                    if let Some(a) = self.active.as_mut() {
                        a.renderer.resize(size.width, size.height);
                    }
                    // Geometry changed: invalidate the ring, re-show the current
                    // image at the new size, and refill the ring at that size.
                    self.invalidate_geometry();
                    self.load_current_sync(event_loop);
                    self.target_item = self.playlist.current();
                    self.request_prefetch();
                }
            }

            WindowEvent::RedrawRequested => self.draw(event_loop),

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
                        event_loop.exit();
                    } else if !repeat {
                        // Real press only — OS auto-repeats are ignored so they
                        // can't queue up and delay the release. Holding is driven
                        // by `about_to_wait`.
                        match code {
                            KeyCode::Space => {
                                self.held.insert(code);
                                self.hold_start = Some(Instant::now());
                                self.advance(true, event_loop);
                            }
                            KeyCode::Backspace => {
                                self.held.insert(code);
                                self.hold_start = Some(Instant::now());
                                self.advance(false, event_loop);
                            }
                            // Pan (arrows) and zoom (=/- and numpad) are continuous
                            // while held — tracked here, applied in `about_to_wait`.
                            KeyCode::ArrowLeft
                            | KeyCode::ArrowRight
                            | KeyCode::ArrowUp
                            | KeyCode::ArrowDown
                            | KeyCode::Equal
                            | KeyCode::Minus
                            | KeyCode::NumpadAdd
                            | KeyCode::NumpadSubtract => {
                                self.held.insert(code);
                            }
                            // Scaling mode: 8 fit, 9 fill, 0 toggles original ↔ fit.
                            KeyCode::Digit0 => {
                                let next = if self.view.mode == ScaleMode::Original {
                                    ScaleMode::Fit
                                } else {
                                    ScaleMode::Original
                                };
                                self.set_scale_mode(next, event_loop);
                            }
                            KeyCode::Digit8 => self.set_scale_mode(ScaleMode::Fit, event_loop),
                            KeyCode::Digit9 => self.set_scale_mode(ScaleMode::Fill, event_loop),
                            // Rotate 90° clockwise, or counter-clockwise with Shift.
                            KeyCode::KeyR => self.rotate(self.shift, event_loop),
                            // Info panel: i basic, Shift+I full EXIF.
                            KeyCode::KeyI => self.toggle_info(self.shift, event_loop),
                            // Keybindings help (`/` or `?` — same physical key).
                            KeyCode::Slash => self.toggle_help(event_loop),
                            _ => {}
                        }
                    }
                }
                ElementState::Released => {
                    self.held.remove(&code);
                }
            },

            // Track Shift for Shift+R / Shift+I.
            WindowEvent::ModifiersChanged(mods) => {
                self.shift = mods.state().shift_key();
            }

            // Focus loss can swallow the key-up event; clear held keys so
            // navigation never gets stuck auto-advancing (a known winit repeat /
            // lost-key-up hazard, called out in CLAUDE.md).
            WindowEvent::Focused(false) => {
                self.held.clear();
                self.hold_start = None;
                self.shift = false;
                self.zoom_started = None;
                self.zoom_last = None;
                self.pan_started = None;
                self.pan_last = None;
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        // 1. Absorb finished decodes (uploads; presents the target if it arrived).
        self.drain_results(event_loop);

        // 2. Continuous zoom/pan while their keys are held (accelerating ramp).
        let transforming = self.apply_view_holds(now, event_loop);

        // 3. Gated self-paced advance while a nav key (space/backspace) is held.
        // The initial tap delay gates *repeat*, not draining/presenting, so a
        // first-press miss shows the moment it decodes. (plain `match`, not
        // `is_none_or`: that's 1.82+ vs the 1.80 MSRV.)
        let nav = self.held_direction();
        let past_delay = match self.hold_start {
            Some(t) => now >= t + self.initial_delay,
            None => true,
        };
        if let Some(forward) = nav {
            // Advance only when caught up (target shown) AND a frame elapsed, so
            // every photo is shown and a miss simply holds.
            let caught_up = self.displayed_item == self.target_item;
            let due = match self.last_present {
                Some(t) => now >= t + self.frame_interval,
                None => true,
            };
            if past_delay && caught_up && due {
                self.advance(forward, event_loop);
            } else if !caught_up {
                self.try_present_target(event_loop);
            }
        } else {
            self.hold_start = None;
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

        // 5. Poll at the frame rate while interacting or work is outstanding;
        //    otherwise go fully idle until the next event.
        if nav.is_some() || transforming || self.work_pending() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + self.frame_interval));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

fn title_for(path: &Path, idx: usize, n: usize) -> String {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    format!("PhotoBlaze — {name} ({}/{n})", idx + 1)
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let windowed = args.iter().any(|a| a == "--windowed" || a == "-w");
    let recursive = args.iter().any(|a| a == "--recursive" || a == "-r");
    let metrics_on = args.iter().any(|a| a == "--metrics");
    let dir = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let paths = scan_images(&dir, recursive);
    println!(
        "PhotoBlaze: {} image(s) in {}{}",
        paths.len(),
        dir.display(),
        if recursive { " (recursive)" } else { "" }
    );
    if paths.is_empty() {
        eprintln!("(no supported images found — showing a placeholder; pass a folder path)");
    }

    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let metrics = if metrics_on {
        StageTimes::enabled()
    } else {
        StageTimes::disabled()
    };
    let mut app = App::new(windowed, dir, paths, metrics);
    event_loop.run_app(&mut app).expect("event loop");

    let report = app.metrics.report();
    if !report.is_empty() {
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
    fn truncate_exif_value_caps_long_values() {
        assert_eq!(truncate_exif_value("1/250 s"), "1/250 s");
        let out = truncate_exif_value(&"a".repeat(100));
        assert_eq!(out.chars().count(), EXIF_VALUE_MAX + 1); // value + ellipsis
        assert!(out.ends_with('…'));
        // Multibyte values are truncated on char boundaries, not bytes.
        let out = truncate_exif_value(&"é".repeat(100));
        assert_eq!(out.chars().count(), EXIF_VALUE_MAX + 1);
    }
}
