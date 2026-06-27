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
//!   space / →   next photo
//!   ⌫ / ←       previous photo
//!   0 / o       toggle fit-to-screen <-> original 1:1 (synchronous; outside the ring)
//!   i           toggle info panel (path · resolution · codec)
//!   esc         quit
//!
//! Usage:
//!   cargo run -p pb-app --release -- "D:\Pictures\2003\Halloween"
//!   cargo run -p pb-app --release -- "D:\Pictures" -r          # recurse subfolders
//!   cargo run -p pb-app --release -- "D:\Pictures" -r --windowed --metrics

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
use pb_decode::{decode_image_file, DecodedImage, FitBox};
use pb_render::{test_pattern, Renderer, ScaleMode, WgpuRenderer};

mod decode_pool;
mod hud;
mod metrics;
use decode_pool::{recommended_workers, DecodeFn, DecodePool, Outcome};
use hud::Hud;
use metrics::StageTimes;

/// VRAM budget for the resident texture ring (~1.5 GB → ~16–32 fit-size slots on
/// a 7680-wide display, far more on smaller ones). Capacity is clamped to [4, 64].
const RING_BUDGET_BYTES: u64 = 1_500_000_000;
/// Cap on decoded-but-not-yet-uploaded bytes held by the pool (backpressure).
const POOL_BUDGET_BYTES: usize = 512 * 1024 * 1024;
/// Max slot uploads performed per `about_to_wait` tick, so a burst of finished
/// decodes can't blow the frame budget.
const UPLOADS_PER_TICK: usize = 2;

/// Ring capacity from the per-slot (fit-box) size and the VRAM budget.
fn ring_capacity(fit: FitBox) -> usize {
    let slot_bytes = (fit.max_width as u64) * (fit.max_height as u64) * 4;
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
    /// When the last self-paced advance happened, to cap the rate.
    last_advance: Option<Instant>,
    /// Minimum time between advances while holding (≈ one display refresh).
    frame_interval: Duration,
    /// When the current hold's first press happened (for the initial-delay gate).
    hold_start: Option<Instant>,
    /// Delay after the first press before auto-repeat begins (tap = one photo).
    initial_delay: Duration,
    /// Decode-to-fit target = the display size; photos are downscaled to it.
    fit: Option<FitBox>,
    /// Fit-to-screen (default) vs. original 1:1 centered.
    scale_mode: ScaleMode,
    /// Scan root, for showing paths relative to it.
    root: PathBuf,
    /// Text renderer for the info panel (None if no system font was found).
    hud: Option<Hud>,
    /// Whether the info panel is enabled (toggled by `I`).
    info_visible: bool,
    /// Whether the panel is currently drawn (rebuilt only when the photo changes).
    overlay_shown: bool,
    /// The current photo's info, for the panel.
    current: Option<PhotoMeta>,
    /// Display scale factor, for sizing the panel.
    scale_factor: f32,
    /// Idle time before the panel appears once you stop navigating.
    idle_delay: Duration,
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
            last_advance: None,
            frame_interval: Duration::from_micros(8_333), // ~120 Hz until we read the real rate
            hold_start: None,
            initial_delay: Duration::from_millis(400),
            fit: None,
            scale_mode: ScaleMode::Fit,
            root,
            hud: Hud::load(),
            info_visible: false,
            overlay_shown: false,
            current: None,
            scale_factor: 1.0,
            idle_delay: Duration::from_millis(50),
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
        }
    }

    /// The decode-to-fit target for the current mode: the display size in Fit mode
    /// (downscale large photos), or None in Original mode (decode full-res).
    fn decode_fit(&self) -> Option<FitBox> {
        match self.scale_mode {
            ScaleMode::Fit => self.fit,
            ScaleMode::Original => None,
        }
    }

    /// Recompute the prefetch want-list and hand it to the decode pool. Items
    /// already resident are not re-requested.
    fn request_prefetch(&mut self) {
        self.targets = prefetch_targets(&self.playlist, self.ahead, self.behind);
        let fit = self.decode_fit();
        let jobs: Vec<(usize, Arc<Path>, Option<FitBox>)> = self
            .targets
            .iter()
            .filter(|&&t| self.ring.slot_for(t).is_none() && !self.failed.contains(&t))
            .map(|&t| (t, self.paths[t].clone(), fit))
            .collect();
        self.pool.set_targets(self.epoch, &jobs);
    }

    /// Show ring `slot` (holding `item`): the keypress fast path — a rebind, no
    /// decode or upload. Updates the pin, title, and info panel.
    fn present_item(&mut self, item: usize, slot: usize, event_loop: &ActiveEventLoop) {
        let title = title_for(&self.paths[item], item, self.paths.len());
        if let Some(a) = self.active.as_mut() {
            a.renderer.present_slot(slot);
            // The photo changed — drop the stale panel; it returns once idle.
            a.renderer.set_overlay(None, 0);
            a.window.set_title(&title);
        }
        self.ring.set_displayed(slot);
        self.displayed_item = Some(item);
        self.current = self.meta_cache.get(&item).cloned();
        self.overlay_shown = false;
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

    /// Drain finished decodes (budgeted): discard stale-epoch results, upload the
    /// rest into ring slots, and present the target if its decode just arrived.
    fn drain_results(&mut self, event_loop: &ActiveEventLoop) {
        let mut uploads = 0;
        while uploads < UPLOADS_PER_TICK {
            let outcome = match self.results.try_recv() {
                Ok(o) => o,
                Err(_) => break,
            };
            if outcome.key.epoch != self.epoch {
                continue; // decoded for an old geometry; its bytes free on drop
            }
            let item = outcome.key.item;
            if self.ring.slot_for(item).is_some() {
                continue; // already resident (a rare duplicate decode)
            }
            let img = match outcome.result {
                Ok(ref img) => img,
                Err(ref e) => {
                    eprintln!("decode failed for item {item}: {e}");
                    self.failed.insert(item);
                    // Unstick the gated loop: a corrupt target counts as "shown"
                    // (the previous frame stays up) so hold-to-fly skips past it.
                    if self.target_item == Some(item) {
                        self.displayed_item = Some(item);
                    }
                    continue;
                }
            };
            if !self.meta_cache.contains_key(&item) {
                let m = meta_for_path(&self.paths[item], &self.root, img);
                self.meta_cache.insert(item, m);
            }
            if let Some(res) = self.ring.reserve(item, self.epoch, &self.targets) {
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
        }
    }

    /// Synchronous decode + display of the current item (the first frame, geometry
    /// changes, and all Original-mode navigation — which sits outside the ring).
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
                let title = title_for(&self.paths[idx], idx, self.paths.len());
                if let Some(a) = self.active.as_mut() {
                    a.renderer.set_image(&img.pixels, img.width, img.height);
                    a.renderer.set_overlay(None, 0);
                    a.window.set_title(&title);
                }
                self.overlay_shown = false;
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
        let cap = ring_capacity(fit);
        self.ring = ResidentRing::new(cap);
        if let Some(a) = self.active.as_mut() {
            a.renderer.reserve_ring(cap, fit.max_width, fit.max_height);
        }
        let (ahead, behind) = window_for_capacity(cap);
        self.ahead = ahead;
        self.behind = behind;
    }

    /// After a geometry change: resume prefetch in Fit mode, or — in Original mode
    /// (which is outside the ring) — cancel prefetch and clear the want-list so the
    /// event loop can idle instead of polling on stale, never-resident targets.
    fn resume_prefetch_or_idle(&mut self) {
        if self.scale_mode == ScaleMode::Fit {
            self.request_prefetch();
        } else {
            self.targets.clear();
            self.pool.set_targets(self.epoch, &[]);
        }
    }

    /// Toggle fit-to-screen <-> original 1:1. Re-decodes the current image at the
    /// new geometry immediately; Fit resumes prefetch, Original cancels it.
    fn toggle_scale(&mut self, event_loop: &ActiveEventLoop) {
        self.scale_mode = match self.scale_mode {
            ScaleMode::Fit => ScaleMode::Original,
            ScaleMode::Original => ScaleMode::Fit,
        };
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_scale_mode(self.scale_mode);
        }
        self.invalidate_geometry();
        self.load_current_sync(event_loop);
        self.target_item = self.playlist.current();
        self.resume_prefetch_or_idle();
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

    /// Toggle the info panel. When turned on it shows immediately (we're idle);
    /// after navigation it reappears once you stop (see `about_to_wait`).
    fn toggle_info(&mut self, event_loop: &ActiveEventLoop) {
        self.info_visible = !self.info_visible;
        if self.info_visible {
            self.show_overlay(event_loop);
        } else {
            if let Some(a) = self.active.as_mut() {
                a.renderer.set_overlay(None, 0);
            }
            self.overlay_shown = false;
            self.draw(event_loop);
        }
    }

    /// Rasterize the current photo's info into a corner panel and draw it.
    fn show_overlay(&mut self, event_loop: &ActiveEventLoop) {
        let panel = {
            let (Some(hud), Some(meta)) = (self.hud.as_ref(), self.current.as_ref()) else {
                return;
            };
            let text = format!("{} · {}×{} · {}", meta.rel, meta.w, meta.h, meta.codec);
            let px = (15.0 * self.scale_factor).max(8.0);
            let pad = (7.0 * self.scale_factor).round().max(2.0) as u32;
            hud.render_panel(&text, px, pad)
        };
        let Some((bitmap, w, h)) = panel else {
            return;
        };
        let margin = (10.0 * self.scale_factor).round().max(1.0) as u32;
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_overlay(Some((&bitmap, w, h)), margin);
        }
        self.overlay_shown = true;
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
        self.last_advance = Some(Instant::now());
        self.target_item = self.playlist.current();
        match self.scale_mode {
            ScaleMode::Original => {
                self.load_current_sync(event_loop);
            }
            ScaleMode::Fit => {
                self.try_present_target(event_loop);
                self.request_prefetch();
            }
        }
    }

    /// Which way we're currently paging, from held keys (both/neither = idle).
    fn held_direction(&self) -> Option<bool> {
        let fwd = self.held.contains(&KeyCode::ArrowRight) || self.held.contains(&KeyCode::Space);
        let bwd =
            self.held.contains(&KeyCode::ArrowLeft) || self.held.contains(&KeyCode::Backspace);
        match (fwd, bwd) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
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
        let cap = ring_capacity(fit);
        self.ring = ResidentRing::new(cap);
        renderer.reserve_ring(cap, fit.max_width, fit.max_height);
        let (ahead, behind) = window_for_capacity(cap);
        self.ahead = ahead;
        self.behind = behind;
        self.displayed_item = self.playlist.current();
        self.target_item = self.playlist.current();

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
                    // image at the new size, and refill (Fit) or idle (Original).
                    self.invalidate_geometry();
                    self.load_current_sync(event_loop);
                    self.target_item = self.playlist.current();
                    self.resume_prefetch_or_idle();
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
                            KeyCode::Space | KeyCode::ArrowRight => {
                                self.held.insert(code);
                                self.hold_start = Some(Instant::now());
                                self.advance(true, event_loop);
                            }
                            KeyCode::Backspace | KeyCode::ArrowLeft => {
                                self.held.insert(code);
                                self.hold_start = Some(Instant::now());
                                self.advance(false, event_loop);
                            }
                            // Toggle fit-to-screen <-> original 1:1 (centered).
                            KeyCode::Digit0 | KeyCode::KeyO => self.toggle_scale(event_loop),
                            // Toggle the corner info panel.
                            KeyCode::KeyI => self.toggle_info(event_loop),
                            _ => {}
                        }
                    }
                }
                ElementState::Released => {
                    self.held.remove(&code);
                }
            },

            // Focus loss can swallow the key-up event; clear held keys so
            // navigation never gets stuck auto-advancing (a known winit repeat /
            // lost-key-up hazard, called out in CLAUDE.md).
            WindowEvent::Focused(false) => {
                self.held.clear();
                self.hold_start = None;
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // 1. Absorb finished decodes (uploads; presents the target if it arrived).
        self.drain_results(event_loop);

        // 2. Gated self-paced advance.
        match self.held_direction() {
            Some(forward) => {
                let now = Instant::now();
                // The initial tap delay gates *repeat*, not draining/presenting:
                // keep polling so a first-press miss shows the moment it decodes
                // (the earlier `return` here added up to the full delay of latency).
                let past_delay = self
                    .hold_start
                    .is_none_or(|t| now >= t + self.initial_delay);
                // Advance only when caught up (target shown) AND a frame elapsed —
                // so every photo is shown and a miss simply holds.
                let caught_up = self.displayed_item == self.target_item;
                let due = self
                    .last_advance
                    .is_none_or(|t| now >= t + self.frame_interval);
                if past_delay && caught_up && due {
                    self.advance(forward, event_loop);
                } else if !caught_up {
                    self.try_present_target(event_loop);
                }
                event_loop.set_control_flow(ControlFlow::WaitUntil(now + self.frame_interval));
            }
            None => {
                self.hold_start = None;
                // Show the info panel once we've been idle past the short delay.
                if self.info_visible && !self.overlay_shown && self.current.is_some() {
                    let now = Instant::now();
                    let due = self
                        .last_advance
                        .map(|t| t + self.idle_delay)
                        .unwrap_or(now);
                    if now < due {
                        event_loop.set_control_flow(ControlFlow::WaitUntil(due));
                        return;
                    }
                    self.show_overlay(event_loop);
                }
                // Keep polling while prefetch is still filling or a target isn't
                // shown yet; otherwise go fully idle until the next event.
                if self.work_pending() {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(
                        Instant::now() + self.frame_interval,
                    ));
                } else {
                    event_loop.set_control_flow(ControlFlow::Wait);
                }
            }
        }
    }
}

fn title_for(path: &Path, idx: usize, n: usize) -> String {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    format!("PhotoBlaze — {name} ({}/{n})", idx + 1)
}

fn has_jpeg_ext(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("jpg") | Some("jpeg")
    )
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
        } else if ft.is_file() && has_jpeg_ext(&path) {
            out.push(path);
        }
    }
}

/// Scan `dir` for JPEGs, sorted by full path. `recursive` also walks subfolders
/// (a `-r` convenience now; the R-key toggle with folder-grouped ordering is
/// tasks.json #9). Non-recursive is the default, matching that design.
fn scan_jpegs(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if recursive {
        collect_recursive(dir, &mut paths);
    } else {
        match std::fs::read_dir(dir) {
            Ok(rd) => {
                for entry in rd.flatten() {
                    let is_file = entry.file_type().map(|ft| ft.is_file()).unwrap_or(false);
                    let path = entry.path();
                    if is_file && has_jpeg_ext(&path) {
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

    let paths = scan_jpegs(&dir, recursive);
    println!(
        "PhotoBlaze: {} JPEG(s) in {}{}",
        paths.len(),
        dir.display(),
        if recursive { " (recursive)" } else { "" }
    );
    if paths.is_empty() {
        eprintln!("(no JPEGs found — showing a placeholder; pass a folder path)");
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
