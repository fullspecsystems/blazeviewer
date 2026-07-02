//! PhotoBlaze macOS FFI bridge (NS1, ADR-021) — a [`swift-bridge`] staticlib that exposes
//! the platform-neutral [`pb_app_core::AppCore`] to a SwiftUI/AppKit host.
//!
//! **Shape** (mirrors the winit shell in `crates/pb-app/src/main.rs`, the worked reference):
//! - The Swift host owns the `NSWindow` + `MTKView`/`CAMetalLayer` and the run loop.
//! - **Events in:** the host translates `NSEvent` / gesture recognizers / menu clicks into
//!   calls on [`AppCoreHandle`] (`key_down` / `key_up` / `focus_lost` / `tick` / …), which
//!   build the shell-neutral [`pb_app_core::contract::CoreEvent`]s and call `AppCore::handle`.
//! - **Effects out:** the host pulls [`AppCoreHandle::next_effect`] **on the main actor**
//!   until `None` and executes each [`ffi::CoreEffectFfi`] (render, set title, wake, quit, …).
//!   A worker thread may only *schedule* a main-thread drain — never touch AppKit/render
//!   directly.
//!
//! **macOS-only.** The `swift-bridge` dependency and this whole module are target-gated, so
//! on Windows/Linux the crate compiles to an empty staticlib and the winit `pb-app` build is
//! untouched.
//!
//! **Slice 1 (this file)** proves the event→effect round-trip against the real
//! `AppCore::handle` on a *headless* core (no surface, no photos yet). A live `CAMetalLayer`
//! surface, a real photo source, and the remaining effects/events are layered on in the
//! following NS1 slices — see `.taskmaster/docs/macos-native-ui-plan.md` (§NS1).
#![cfg(target_os = "macos")]
// The `#[swift_bridge::bridge]` macro emits `extern "C"` shims with same-type pointer casts
// (`*mut AppCoreHandle` → `*mut AppCoreHandle`); that's generated glue we can't edit, so allow
// the lint crate-wide rather than fail `clippy -D warnings`.
#![allow(clippy::unnecessary_cast)]

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use pb_app_core::archive::ArchiveOpenError;
use pb_app_core::contract::{self, CoreEvent, Modifiers};
use pb_app_core::engine;
use pb_app_core::overlay::OpenPanel;
use pb_app_core::scan::{self, Resolved, ScanProgress, ScanUpdate};
use pb_app_core::{Action, AppCore, PbKey, Viewport};
use pb_core::open::{self, Cursor, LaunchInput, Source};
use pb_core::ResidentRing;
use pb_render::Renderer as _;

/// An in-flight streaming folder walk — the mirror of the winit shell's `DirScan`
/// (worker thread + generation guard; the progress dialog is NS2).
struct DirScan {
    generation: u64,
    rx: Receiver<(u64, ScanUpdate)>,
    progress: ScanProgress,
}

/// An in-flight background archive open — the mirror of the winit shell's `ArchiveLoad`.
struct ArchiveLoad {
    generation: u64,
    rx: Receiver<(u64, Result<Resolved, ArchiveOpenError>)>,
    was_password_attempt: bool,
    progress: pb_source::OpenProgress,
}

/// The opaque handle the Swift host holds — it owns the entire `AppCore` engine **plus the
/// shell's Rust half**: the scan/archive worker threads the `Begin*` effects spawn. Those
/// effects never cross to Swift — this crate handles them exactly like the winit shell
/// (`std::thread` + mpsc + generation guards), and `tick` polls the results back into
/// `AppCore::handle`. Swift only sees the genuinely native effects (title, dialogs,
/// clipboard, …).
pub struct AppCoreHandle {
    core: AppCore,
    dir_scan: Option<DirScan>,
    scan_gen: u64,
    archive_load: Option<ArchiveLoad>,
    archive_gen: u64,
    /// The payload of an in-flight `WriteClipboard` effect. Stashed here (the drain emits a
    /// bare marker) because a `Vec<u8>` inside a transparent-enum payload is exactly the
    /// shape swift-bridge mis-generates (gotcha #3); the host pulls it via the
    /// `clipboard_*`/`take_clipboard_*` accessors instead.
    pending_clipboard: Option<contract::ClipboardPayload>,
}

impl AppCoreHandle {
    /// Construct the real engine (decode pool, user settings + keymap, HUD compositor) at
    /// the given drawable size (`width`×`height` in physical pixels, `scale` = backing
    /// scale factor) — see [`AppCore::new_host`]. The deck starts empty; `open_path` /
    /// `attach_layer` bring photos + the surface.
    fn new(width: u32, height: u32, scale: f32) -> AppCoreHandle {
        AppCoreHandle {
            core: AppCore::new_host(Viewport {
                width,
                height,
                scale_factor: scale,
            }),
            dir_scan: None,
            scan_gen: 0,
            archive_load: None,
            archive_gen: 0,
            pending_clipboard: None,
        }
    }

    /// Open a single file / folder / archive path — the `--pb-open` dev argument, or a
    /// one-item drop. See [`open_paths`](Self::open_paths).
    fn open_path(&mut self, path: &str) {
        self.open_paths(vec![path.to_string()]);
    }

    /// Open launch/drop/panel paths — the winit shell's `classify_inputs` mirrored (ADR-019):
    /// a lone directory scans recursively, a lone `.zip`/`.7z` opens to its contents, files
    /// scan/list per the launch policy (a single file → its folder flat, cursor on it). Routed
    /// through the core's `open_plan`, whose `Begin*` effects this crate executes on its
    /// worker threads. Empty / all-empty input is ignored (never blanks the current photo).
    fn open_paths(&mut self, paths: Vec<String>) {
        self.core.now = Instant::now();
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect();
        if paths.is_empty() {
            return;
        }
        let input = if paths.len() == 1 && paths[0].is_dir() {
            LaunchInput::Directory(paths.into_iter().next().expect("len == 1"))
        } else if paths.len() == 1 && is_archive(&paths[0]) {
            LaunchInput::Archive(paths.into_iter().next().expect("len == 1"))
        } else {
            // One or more files (a directory inside a multi-selection is uncommon and is
            // ignored). If somehow every path is a directory, open the first.
            let files: Vec<PathBuf> = paths.iter().filter(|p| !p.is_dir()).cloned().collect();
            if files.is_empty() {
                LaunchInput::Directory(paths.into_iter().next().expect("non-empty"))
            } else {
                LaunchInput::Files(files)
            }
        };
        let plan = open::plan(input);
        self.core.open_plan(plan.source, plan.cursor);
    }

    /// The pointer moved (physical px, top-left origin — the winit `CursorMoved` convention;
    /// the Swift view flips AppKit's bottom-left origin and scales by the backing factor).
    /// Anchors pinch/wheel zoom, drives drag-to-pan while the left button is down, and
    /// un-hides the cursor.
    fn pointer_moved(&mut self, x: f32, y: f32) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::PointerMoved { x, y });
    }

    /// Left mouse button pressed/released — the winit shell's `MouseInput(Left)` arm
    /// mirrored: a press on an interactive on-image control (an open-panel button, the play
    /// hint, the scan-chip's Cancel) fires that control; anywhere else it toggles
    /// drag-to-pan.
    fn mouse_left(&mut self, pressed: bool) {
        self.core.now = Instant::now();
        let open_hit = if pressed {
            self.core.open_hovered_button()
        } else {
            None
        };
        if let Some(button) = open_hit {
            match button {
                pb_app_core::OpenButton::File => self.core.dispatch_action(Action::OpenFile),
                pb_app_core::OpenButton::Folder => self.core.dispatch_action(Action::OpenFolder),
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
            // The scan-count chip's Cancel: stop the walk, keep what streamed in.
            self.cancel_dir_scan();
            self.core.request_prefetch();
            self.core.show_toast("Scan stopped");
        } else {
            self.core.dragging = pressed;
            self.core.refresh_cursor();
        }
    }

    /// Line-precise scroll (a mouse wheel notch) — pan or zoom per the Scroll-wheel setting.
    fn scroll_lines(&mut self, x: f32, y: f32) {
        self.core.now = Instant::now();
        self.core
            .handle(CoreEvent::Scroll(contract::ScrollDelta::Lines { x, y }));
    }

    /// Pixel-precise scroll (a trackpad two-finger swipe), in **physical px** (the winit
    /// convention — the Swift view scales AppKit's points by the backing factor).
    fn scroll_pixels(&mut self, x: f32, y: f32) {
        self.core.now = Instant::now();
        self.core
            .handle(CoreEvent::Scroll(contract::ScrollDelta::Pixels { x, y }));
    }

    /// Trackpad pinch: `delta` is the incremental magnification (`NSEvent.magnification`,
    /// + spread to zoom in, − pinch to zoom out) — zooms about the cursor.
    fn pinch(&mut self, delta: f32) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::Pinch { delta });
    }

    /// Trackpad two-finger double-tap ("smart magnify"): toggle 100% ↔ fit, sharing the
    /// keyboard's `0` toggle so they can't drift.
    fn double_tap(&mut self) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::DoubleTap);
    }

    /// Whether engine work is still outstanding (prefetch/decode/scan/archive in flight) —
    /// the host keeps the display link running while true, pauses when idle (the frame
    /// pump's continuous-vs-on-demand decision, NS1 item 7).
    fn work_pending(&self) -> bool {
        self.core.work_pending() || self.dir_scan.is_some() || self.archive_load.is_some()
    }

    /// Show a HUD toast — the host's feedback line after it executes a platform op
    /// (clipboard written, etc.), mirroring the winit shell's post-write toasts.
    fn toast(&mut self, msg: &str) {
        self.core.show_toast(msg);
    }

    // ---- The WriteClipboard payload accessors (see `pending_clipboard`). The host, on
    // the `WriteClipboard` marker: try `take_clipboard_text()` first; if empty, read the
    // image dimensions/file and `take_clipboard_image()`.

    /// The pending text payload ("" if none / it's an image). Consumes it.
    fn take_clipboard_text(&mut self) -> String {
        match self.pending_clipboard.take() {
            Some(contract::ClipboardPayload::Text(t)) => t,
            other => {
                self.pending_clipboard = other; // put a non-text payload back
                String::new()
            }
        }
    }

    /// Pending image width in px (0 = no image payload).
    fn clipboard_image_width(&self) -> u32 {
        match &self.pending_clipboard {
            Some(contract::ClipboardPayload::Image { w, .. }) => *w,
            _ => 0,
        }
    }

    /// Pending image height in px (0 = no image payload).
    fn clipboard_image_height(&self) -> u32 {
        match &self.pending_clipboard {
            Some(contract::ClipboardPayload::Image { h, .. }) => *h,
            _ => 0,
        }
    }

    /// The on-disk file to offer alongside the pixels ("" for an archive entry).
    fn clipboard_image_file(&self) -> String {
        match &self.pending_clipboard {
            Some(contract::ClipboardPayload::Image { file: Some(p), .. }) => {
                p.to_string_lossy().into_owned()
            }
            _ => String::new(),
        }
    }

    /// The pending image's RGBA8 pixels (`w*h*4`; empty if none). Consumes the payload.
    fn take_clipboard_image(&mut self) -> Vec<u8> {
        match self.pending_clipboard.take() {
            Some(contract::ClipboardPayload::Image { rgba, .. }) => rgba,
            other => {
                self.pending_clipboard = other;
                Vec::new()
            }
        }
    }

    /// A physical key went down. `key` is a [`PbKey`] name accepted by `PbKey::from_name`
    /// (e.g. `"Space"`, `"Escape"`, `"Right"`, `"C"` — NOT winit's `"ArrowRight"`/`"KeyC"`
    /// spellings) — the Swift host maps `NSEvent` → this name (the input-adapter job, NS1).
    /// Unknown names are ignored. OS auto-repeat is passed via `is_repeat` (named to dodge
    /// the Swift keyword `repeat` — swift-bridge gotcha #4: a Rust param named after a Swift
    /// keyword generates Swift glue that doesn't compile); the core drops repeats for held
    /// actions, exactly as the winit shell does.
    fn key_down(
        &mut self,
        key: &str,
        ctrl: bool,
        shift: bool,
        alt: bool,
        logo: bool,
        is_repeat: bool,
    ) {
        // The FFI is host/shell code, so it stamps the clock (the core never reads it — NS0).
        self.core.now = Instant::now();
        if let Some(key) = PbKey::from_name(key) {
            self.core.handle(CoreEvent::KeyDown {
                key,
                mods: Modifiers {
                    ctrl,
                    shift,
                    alt,
                    logo,
                },
                repeat: is_repeat,
            });
        }
    }

    /// A physical key was released.
    fn key_up(&mut self, key: &str) {
        self.core.now = Instant::now();
        if let Some(key) = PbKey::from_name(key) {
            self.core.handle(CoreEvent::KeyUp { key });
        }
    }

    /// The window lost key focus — the core clears held keys (the focus-loss release net).
    fn focus_lost(&mut self) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::FocusLost);
    }

    /// A frame / idle tick: drives held-key pacing, slideshow dwell, prefetch, and animation.
    /// The host calls this each frame it draws and on the scheduled wake deadlines returned
    /// via `SetWake` (a `MTKViewDelegate.draw(in:)` + timer, per the plan). Like the winit
    /// `about_to_wait`, the shell's flow-polling runs first: finished scan/archive workers
    /// feed their results into the core before the tick evaluates pacing/prefetch.
    fn tick(&mut self) {
        self.core.now = Instant::now();
        self.poll_dir_scan();
        self.poll_archive_load();
        self.core.handle(CoreEvent::Tick(self.core.now));
    }

    /// Attach the host's **retained `CAMetalLayer`** (passed as its raw pointer bits — the
    /// slice of NS1 item 2) and stand the wgpu renderer up on it: create the surface, route
    /// the real size through the standard `Resized` path, and — on an empty deck — show the
    /// blank letterbox + the "Press O to open" hint exactly like the winit shell's
    /// `resumed()`. The host then pokes the layer's EDR colorspace (see `wants_edr`) and
    /// calls `render`.
    ///
    /// Safety contract (upheld by the Swift host; see `WgpuRenderer::new_from_ca_layer`):
    /// the layer is valid + retained and outlives the renderer (`detach_layer` runs before
    /// the view/layer dies), and every call happens on the main actor.
    fn attach_layer(&mut self, layer_ptr: usize, width: u32, height: u32, scale: f32) {
        self.core.now = Instant::now();
        // Viewport + fit first (through the standard path; renderer is still None so the
        // swapchain half no-ops) so the initial decode-to-fit below uses the real size.
        self.core.handle(CoreEvent::Resized {
            width,
            height,
            scale,
        });
        // Decode the first image (or the test pattern / blank) at the display size —
        // the same synchronous preview-first path the winit `resumed` uses.
        let (rgba, iw, ih, color, hdr, peak, title) = self.core.initial_image();
        // SAFETY: the host passes a valid retained CAMetalLayer and guarantees the
        // lifetime + main-thread rules (documented on `new_from_ca_layer`).
        let mut renderer = unsafe {
            pb_render::WgpuRenderer::new_from_ca_layer(
                layer_ptr as *mut std::ffi::c_void,
                width,
                height,
                &rgba,
                iw,
                ih,
                color,
                hdr,
                peak,
            )
        };
        renderer.set_letterbox(self.core.settings.letterbox);
        // Size the resident ring to the display and start filling it — navigation is a
        // rebind into this ring, never a decode (mirrors the winit `resumed`).
        let cap = engine::ring_capacity(self.core.slot_bytes_estimate());
        self.core.ring = ResidentRing::new_with_budget(cap, engine::RING_BUDGET_BYTES);
        renderer.reserve_ring(cap, width.max(1), height.max(1));
        let (ahead, behind) = engine::window_for_capacity(cap);
        self.core.ahead = ahead;
        self.core.behind = behind;
        self.core.displayed_item = self.core.playlist.current();
        self.core.target_item = self.core.playlist.current();
        self.core.last_present = Some(Instant::now());
        self.core.renderer = Some(Box::new(renderer));
        self.core
            .effects
            .push(contract::CoreEffect::SetTitle(title));
        self.core.request_prefetch();
        // Empty deck (bare launch): blank letterbox + the centered Open File / Open
        // Folder call to action.
        if self.core.playlist.current().is_none() {
            let panel = self.core.open_panel_bitmap();
            if let Some(r) = self.core.renderer.as_mut() {
                r.clear_image();
                if let Some((bitmap, w, h, file, folder)) = panel {
                    r.set_message(Some((&bitmap, w, h)));
                    self.core.open_panel = Some(OpenPanel { w, h, file, folder });
                }
            }
        }
    }

    /// Drop the renderer (and its wgpu surface). The host MUST call this before the
    /// hosting view/layer is destroyed — the other half of the layer-lifetime contract.
    fn detach_layer(&mut self) {
        self.core.renderer = None;
    }

    /// The surface resized (or moved to a display with a different backing scale):
    /// `width`×`height` in physical pixels. The host calls `render` afterwards.
    fn resized(&mut self, width: u32, height: u32, scale: f32) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::Resized {
            width,
            height,
            scale,
        });
    }

    /// Draw a frame into the attached layer. No-op when no layer is attached.
    fn render(&mut self) {
        if let Some(r) = self.core.renderer.as_mut() {
            let _ = r.render();
        }
    }

    /// Whether the surface came up fp16 scRGB (HDR/wide-gamut capable) — when true the
    /// host must set the layer's colorspace to extended-linear-sRGB + enable EDR (the
    /// macOS layer poke `pb-app/src/hdr_surface.rs` does on the winit target; the host
    /// owns the layer here) and report the panel headroom via `set_edr_headroom`.
    fn wants_edr(&self) -> bool {
        self.core
            .renderer
            .as_ref()
            .and_then(|r| r.hdr_surface_wants_edr())
            .is_some()
    }

    /// The display's EDR headroom (max EDR color component value; ≥ 1.0) for the
    /// highlight roll-off — macOS hard-clips above it (unlike Windows' DWM tone-map).
    fn set_edr_headroom(&mut self, headroom: f32) {
        if let Some(r) = self.core.renderer.as_mut() {
            r.set_edr_headroom(headroom);
        }
    }

    /// Pull the next effect the core produced, or `None` when the queue is drained — the host
    /// loops this on the main actor after each event/tick (`while let e = next_effect() { … }`).
    /// The **shell-internal effects are intercepted here** and never surface to Swift: the
    /// `Begin*`/`Cancel*` scan+archive flow runs on this crate's worker threads (the winit
    /// shell's drain does the same). Everything not yet bridged arrives as
    /// [`ffi::CoreEffectFfi::Other`].
    ///
    /// Pull-style rather than `-> Vec<CoreEffectFfi>` (swift-bridge gotcha #3): 0.1.59
    /// generates the *Rust* half of a `Vec<transparent enum>` return but not the Swift-side
    /// `Vectorizable` conformance or the `Vec_…` C shims, so the generated Swift doesn't
    /// compile. `Option<transparent enum>` is fully supported — and a handful of nanosecond
    /// FFI calls per event is free anyway.
    fn next_effect(&mut self) -> Option<ffi::CoreEffectFfi> {
        use contract::CoreEffect as C;
        // Intercepted effects can enqueue follow-ups (a sync zip open resolves inline →
        // `ArchiveResolved` → more effects); re-reading the queue each pass keeps this the
        // same loop-until-quiescent shape as the winit drain.
        while !self.core.effects.is_empty() {
            match self.core.effects.remove(0) {
                C::BeginDirScan { source, cursor } => self.begin_dir_scan(source, cursor),
                C::BeginArchiveOpen { path, password } => self.begin_archive_open(path, password),
                C::CancelScan => self.cancel_dir_scan(),
                C::CancelArchiveLoad => self.cancel_archive_load(),
                // Flow commands whose execution is shell-*Rust* (the scan worker lives in
                // this crate) — run here; only the genuinely Swift-native flows surface.
                C::ShellFlowAction(Action::Recursive) => self.toggle_recursive(),
                C::ShellFlowAction(Action::CancelScan) => self.cancel_scan_command(),
                // The image payload can be tens of MB — stash it and surface a bare
                // marker; the host pulls it via the clipboard accessors (gotcha #3).
                C::WriteClipboard(payload) => {
                    self.pending_clipboard = Some(payload);
                    return Some(ffi::CoreEffectFfi::WriteClipboard);
                }
                other => return Some(map_effect(other)),
            }
        }
        None
    }

    /// Toggle recursive scanning of the current folder — the winit shell's
    /// `toggle_recursive` mirrored: re-stream the walk with the flag flipped, preserving
    /// the current photo by path. A no-op for an explicit list / archive (no scan root).
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
            .map(Cursor::At)
            .unwrap_or(Cursor::First);
        self.begin_dir_scan(
            Source::Scan {
                roots: vec![root],
                recursive,
            },
            cursor,
        );
        self.core.show_toast(if recursive {
            "Recursive folders: on"
        } else {
            "Recursive folders: off"
        });
    }

    /// File ▸ Stop Scanning — stop the in-flight walk, **keeping what streamed in**
    /// (the winit `cancel_scan_command` mirrored). No-op when no scan is running.
    fn cancel_scan_command(&mut self) {
        if self.dir_scan.is_none() {
            return;
        }
        self.cancel_dir_scan();
        self.core.request_prefetch();
        self.core.show_toast("Scan stopped");
    }

    // ---- The shell's Rust half: the scan/archive worker flow (mirrors the winit shell's
    // `begin_*`/`poll_*`; the progress/password dialogs those drive are NS2 — failures
    // surface as `ReportError` for now).

    /// Start a streaming folder walk on a worker thread (`CoreEffect::BeginDirScan`).
    fn begin_dir_scan(&mut self, source: Source, cursor: Cursor) {
        self.cancel_dir_scan();
        self.core.deleted.clear(); // fresh scan → fresh universe, no stale tombstones
        self.scan_gen += 1;
        let generation = self.scan_gen;
        let progress = ScanProgress::new();
        let (roots, recursive) = match source {
            Source::Scan { roots, recursive } => (roots, recursive),
            _ => return, // open_plan routes explicit lists + archives elsewhere
        };
        let root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
        let scan_root = roots.first().cloned();
        let worker_progress = progress.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            scan::stream_scan(
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
        self.core.scanning = true; // sequential-only prefetch while streaming
        self.core.scan_bootstrapped = false; // first non-empty batch bootstraps
        self.dir_scan = Some(DirScan {
            generation,
            rx,
            progress,
        });
    }

    /// Drain finished scan snapshots into the core (each `tick`). The first non-empty batch
    /// bootstraps the view, the rest extend it; `Done` resumes normal prefetch.
    fn poll_dir_scan(&mut self) {
        use std::sync::mpsc::TryRecvError;
        loop {
            let (cur_gen, recv) = match self.dir_scan.as_ref() {
                Some(s) => (s.generation, s.rx.try_recv()),
                None => return,
            };
            match recv {
                Ok((generation, ScanUpdate::Batch(resolved))) => {
                    if generation != cur_gen {
                        continue; // superseded by a newer open
                    }
                    self.core.handle(CoreEvent::ScanBatch(resolved));
                }
                Ok((generation, ScanUpdate::Done)) => {
                    if generation != cur_gen {
                        continue;
                    }
                    self.dir_scan = None;
                    let never_bootstrapped = !self.core.scan_bootstrapped;
                    self.core.handle(CoreEvent::ScanDone);
                    if never_bootstrapped {
                        self.core.effects.push(contract::CoreEffect::ReportError(
                            "No supported images in that selection.".into(),
                        ));
                    }
                    return;
                }
                // Still scanning (the deferred Scanning progress dialog is NS2).
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.dir_scan = None;
                    self.core.scanning = false;
                    return;
                }
            }
        }
    }

    /// Ask the in-flight walk (if any) to stop, and resume normal prefetch.
    fn cancel_dir_scan(&mut self) {
        if let Some(s) = self.dir_scan.as_ref() {
            s.progress.request_cancel();
        }
        self.dir_scan = None;
        self.core.scanning = false;
    }

    /// Start opening an archive (`CoreEffect::BeginArchiveOpen`): a `.zip` synchronously, a
    /// `.7z` on a worker thread after the RAM pre-flight (a real OOM aborts uncatchably, so
    /// over-budget is refused up front).
    fn begin_archive_open(&mut self, path: PathBuf, password: Option<String>) {
        let is_7z = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("7z"));
        let was_password_attempt = password.is_some();
        // Anti-stacking: a newer open supersedes (and cancels) an in-flight one.
        if let Some(prev) = self.archive_load.as_ref() {
            prev.progress.request_cancel();
        }
        if !is_7z {
            let result = scan::open_archive(&path, password);
            self.finish_archive_open(result, was_password_attempt);
            return;
        }
        if let Err(e) = scan::seven_z_preflight(&path, password.as_deref()) {
            self.finish_archive_open(Err(e), was_password_attempt);
            return;
        }
        self.archive_gen += 1;
        let generation = self.archive_gen;
        let progress = pb_source::OpenProgress::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_path = path;
        let worker_progress = progress.clone();
        std::thread::spawn(move || {
            let result = scan::load_seven_z(&worker_path, password, &worker_progress);
            let _ = tx.send((generation, result));
        });
        self.archive_load = Some(ArchiveLoad {
            generation,
            rx,
            was_password_attempt,
            progress,
        });
    }

    /// Pick up a finished background archive open (each `tick`).
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
                let was_attempt = load.map(|l| l.was_password_attempt).unwrap_or(false);
                self.finish_archive_open(result, was_attempt);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.archive_load = None,
        }
    }

    /// Act on a finished archive open. Success installs the playlist via
    /// `ArchiveResolved`; failures report. Password entry needs the NS2 native dialogs, so
    /// `PasswordRequired` surfaces as an error until then.
    fn finish_archive_open(
        &mut self,
        result: Result<Resolved, ArchiveOpenError>,
        was_password_attempt: bool,
    ) {
        match result {
            Ok(r) if !r.source.is_empty() => {
                self.core.handle(CoreEvent::ArchiveResolved(r));
            }
            Ok(_) => self.report_error(ArchiveOpenError::Empty.user_message()),
            Err(ArchiveOpenError::PasswordRequired) => {
                self.core.password_archive = None;
                let msg = if was_password_attempt {
                    "Incorrect archive password.".to_string()
                } else {
                    "That archive is password-protected (password entry arrives with the \
                     native dialogs — NS2)."
                        .to_string()
                };
                self.report_error(msg);
            }
            // User cancelled: drop quietly, keeping whatever is on screen.
            Err(ArchiveOpenError::Cancelled) => {
                self.core.password_archive = None;
            }
            Err(e) => self.report_error(e.user_message()),
        }
    }

    /// Ask the in-flight archive open (if any) to stop; the poll drops it quietly.
    fn cancel_archive_load(&mut self) {
        if let Some(load) = self.archive_load.as_ref() {
            load.progress.request_cancel();
        }
    }

    fn report_error(&mut self, msg: String) {
        self.core
            .effects
            .push(contract::CoreEffect::ReportError(msg));
    }
}

/// Is this path a viewable archive (`.zip` / `.7z`)? Mirrors the winit shell's helper.
fn is_archive(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("zip") || e.eq_ignore_ascii_case("7z"))
        .unwrap_or(false)
}

/// Map a core [`contract::CoreEffect`] to the FFI enum the Swift host switches on.
fn map_effect(e: contract::CoreEffect) -> ffi::CoreEffectFfi {
    use contract::CoreEffect as C;
    use ffi::CoreEffectFfi as E;
    match e {
        C::RequestRender => E::RequestRender,
        C::SetTitle(title) => E::SetTitle(title),
        C::Quit => E::Quit,
        C::SetWake(None) => E::ClearWake,
        // The wake deadline crosses as seconds-from-now (an `Instant` has no meaning across
        // the FFI): 0.0 = wake immediately. Converted at drain time — the host schedules its
        // timer in the same event turn, so the relative delay stays accurate.
        C::SetWake(Some(at)) => {
            E::SetWake(at.saturating_duration_since(Instant::now()).as_secs_f64())
        }
        // A genuinely host-side command (DeletePermanent confirm / Recursive / CancelScan /
        // Quit teardown — see `CoreEffect::ShellFlowAction`), carried by its stable snake_case
        // action id. Esc quits through THIS (the keymap resolves Escape → Action::Quit → a
        // host-side flow action), not through `CoreEffect::Quit` — the host matches "quit".
        C::ShellFlowAction(action) => E::ShellFlowAction(action.id().to_string()),
        // A user-facing error (bad open, refused archive, …) — an NSAlert once the NS2
        // dialogs land; the host logs it until then.
        C::ReportError(msg) => E::ReportError(msg),
        // The native open panels (the empty-deck buttons + the O / ⇧O keys + the future
        // File menu): the host runs an NSOpenPanel at `start_dir` and feeds the picked
        // paths back through `open_paths` — the same classify-and-open as a drop.
        C::OpenFilePanel { start_dir } => {
            E::OpenFilePanel(start_dir.to_string_lossy().into_owned())
        }
        C::OpenFolderPanel { start_dir } => {
            E::OpenFolderPanel(start_dir.to_string_lossy().into_owned())
        }
        // Pointer cursor, by kind name — the host maps to NSCursor (hover feedback for the
        // on-canvas controls, the pan hand, the hidden viewer cursor).
        C::SetCursor(kind) => {
            use contract::CursorKind as K;
            E::SetCursor(
                match kind {
                    K::Default => "default",
                    K::Hidden => "hidden",
                    K::Grab => "grab",
                    K::Grabbing => "grabbing",
                    K::Pointer => "pointer",
                }
                .to_string(),
            )
        }
        // Reveal in Finder (File ▸ Show in Finder / the context menu) — NSWorkspace.
        C::RevealPath(path) => E::RevealPath(path.to_string_lossy().into_owned()),
        // The Live Photo's audio track (its companion .mov) — the host owns AVAudioPlayer.
        C::StartLiveAudio { path, at_secs } => {
            E::StartLiveAudio(path.to_string_lossy().into_owned(), at_secs)
        }
        C::StopLiveAudio => E::StopLiveAudio,
        C::PauseLiveAudio => E::PauseLiveAudio,
        C::ResumeLiveAudio => E::ResumeLiveAudio,
        // The borderless fullscreen speed mode (F) ↔ windowed. `true` = fullscreen.
        C::SetWindowMode(mode) => {
            E::SetWindowMode(matches!(mode, contract::WindowMode::Fullscreen))
        }
        // Esc-teardown step 1: hide before quitting so nothing flashes.
        C::HideWindow => E::HideWindow,
        // Menu state, dialogs, context menu, … — each bridged in a later NS1 slice.
        _ => E::Other,
    }
}

// NOTE: inside `#[swift_bridge::bridge]`, use `//` comments only — a `///` doc comment
// becomes a `#[doc]` attribute that swift-bridge-ir's parser rejects (panics in codegen).
#[swift_bridge::bridge]
mod ffi {
    // The subset of `CoreEffect` bridged so far (NS1 slice 1). It grows as each effect is
    // wired to a native handler; anything not yet mapped arrives as `Other`.
    enum CoreEffectFfi {
        RequestRender,
        SetTitle(String),
        // Tick again in this many seconds (0.0 = now): held-key pacing, slideshow dwell,
        // the animation's next frame. ClearWake = go idle until the next real event.
        SetWake(f64),
        ClearWake,
        Quit,
        // A host-side flow command, by stable Action id ("quit", "delete_permanent",
        // "recursive", "cancel_scan") — the host runs the native operation.
        ShellFlowAction(String),
        // A user-facing error message (an NSAlert once the NS2 dialogs land).
        ReportError(String),
        // Run the native NSOpenPanel at this start directory; picked paths return
        // via open_paths (files+archives / folders respectively).
        OpenFilePanel(String),
        OpenFolderPanel(String),
        // Pointer cursor by kind: "default" | "hidden" | "grab" | "grabbing" | "pointer".
        SetCursor(String),
        // A clipboard write is pending — pull it via take_clipboard_text() /
        // clipboard_image_*() + take_clipboard_image(), write to NSPasteboard, toast().
        WriteClipboard,
        // Reveal this path in Finder (select it in a new window).
        RevealPath(String),
        // Live Photo audio (path to the companion .mov, start offset in seconds).
        StartLiveAudio(String, f64),
        StopLiveAudio,
        PauseLiveAudio,
        ResumeLiveAudio,
        // true = enter the borderless fullscreen speed mode; false = restore windowed.
        SetWindowMode(bool),
        // Hide the window (the Esc-teardown step before Quit).
        HideWindow,
        Other,
    }

    extern "Rust" {
        type AppCoreHandle;

        #[swift_bridge(init)]
        fn new(width: u32, height: u32, scale: f32) -> AppCoreHandle;

        fn key_down(
            &mut self,
            key: &str,
            ctrl: bool,
            shift: bool,
            alt: bool,
            logo: bool,
            is_repeat: bool,
        );
        fn key_up(&mut self, key: &str);
        fn focus_lost(&mut self);
        fn tick(&mut self);
        fn next_effect(&mut self) -> Option<CoreEffectFfi>;
        fn open_path(&mut self, path: &str);
        fn open_paths(&mut self, paths: Vec<String>);

        // Pointer + gestures (NS1 item 4) — same conventions as the winit shell:
        // physical px, top-left origin; pixel scrolls scaled by the backing factor.
        fn pointer_moved(&mut self, x: f32, y: f32);
        fn mouse_left(&mut self, pressed: bool);
        fn scroll_lines(&mut self, x: f32, y: f32);
        fn scroll_pixels(&mut self, x: f32, y: f32);
        fn pinch(&mut self, delta: f32);
        fn double_tap(&mut self);
        fn work_pending(&self) -> bool;
        fn toast(&mut self, msg: &str);

        // The WriteClipboard payload accessors (marker effect + pull — see the field doc).
        fn take_clipboard_text(&mut self) -> String;
        fn clipboard_image_width(&self) -> u32;
        fn clipboard_image_height(&self) -> u32;
        fn clipboard_image_file(&self) -> String;
        fn take_clipboard_image(&mut self) -> Vec<u8>;

        // The canvas surface (NS1 item 2). `layer_ptr` = the retained CAMetalLayer's
        // pointer bits (swift-bridge has no raw-pointer type; usize crosses as UInt).
        fn attach_layer(&mut self, layer_ptr: usize, width: u32, height: u32, scale: f32);
        fn detach_layer(&mut self);
        fn resized(&mut self, width: u32, height: u32, scale: f32);
        fn render(&mut self);
        fn wants_edr(&self) -> bool;
        fn set_edr_headroom(&mut self, headroom: f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull the queue dry, as the Swift host's drain loop does.
    fn drain(h: &mut AppCoreHandle) -> Vec<ffi::CoreEffectFfi> {
        std::iter::from_fn(|| h.next_effect()).collect()
    }

    /// Slice-1 proof: an event driven in through the FFI produces effects the host can pull
    /// out — the full round-trip through the real `AppCore::handle`, and the queue is emptied
    /// by a drain. Escape resolves (default keymap) to `Action::Quit`, a HOST-side flow
    /// command — so the drain must contain `ShellFlowAction("quit")` (NOT `CoreEffect::Quit`;
    /// the host runs the quit teardown), even on a headless core with no photos.
    #[test]
    fn event_in_produces_effects_out() {
        let mut h = AppCoreHandle::new(1920, 1080, 2.0);
        h.key_down("Escape", false, false, false, false, false);
        let effects = drain(&mut h);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, ffi::CoreEffectFfi::ShellFlowAction(id) if id == "quit")),
            "Escape should resolve to the host-side quit flow action"
        );
        assert!(
            drain(&mut h).is_empty(),
            "draining empties the effect queue"
        );
    }

    /// An unknown key name is ignored (no panic, no effects) — the host can be liberal in
    /// what it forwards.
    #[test]
    fn unknown_key_name_is_ignored() {
        let mut h = AppCoreHandle::new(800, 600, 1.0);
        h.key_down("NotAKey", false, false, false, false, false);
        assert!(drain(&mut h).is_empty());
    }

    /// NS1 item 3 end-to-end (no Swift, no GPU): `open_path` on a real folder → the core's
    /// `BeginDirScan` effect is intercepted by the drain → the worker thread streams the
    /// walk → `tick` polls the batches into `ScanBatch`/`ScanDone` → the playlist
    /// bootstraps on the fixture image. The whole open flow, driven exactly as the Swift
    /// host drives it (open_path + a tick/drain loop).
    #[test]
    fn open_path_scans_a_folder_and_bootstraps_the_playlist() {
        const FIXTURE: &[u8] = include_bytes!("../../pb-app-core/tests/fixtures/orient6.jpg");
        let dir = std::env::temp_dir().join(format!("pb-mac-ffi-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("one.jpg"), FIXTURE).unwrap();

        let mut h = AppCoreHandle::new(800, 600, 1.0);
        h.open_path(dir.to_str().unwrap());
        let _ = drain(&mut h); // executes the intercepted BeginDirScan (spawns the worker)
        assert!(h.dir_scan.is_some(), "the scan worker should be in flight");

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while h.core.playlist.current().is_none() && Instant::now() < deadline {
            h.tick(); // polls the worker + applies ScanBatch/ScanDone, like the host's pump
            let _ = drain(&mut h);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            h.core.playlist.current(),
            Some(0),
            "the scan should bootstrap the playlist on the fixture image"
        );
        assert_eq!(h.core.source.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
