#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! PhotoBlaze — the application shell (Phase 2).
//!
//! A chrome-less, fit-to-screen viewer that pages through a folder of JPEGs.
//! Every photo is shown — slow ones just take their moment to decode; nothing is
//! skipped.
//!
//!   space / →   next photo
//!   ⌫ / ←       previous photo
//!   0 / o       toggle fit-to-screen <-> original 1:1 (centered)
//!   esc         quit
//!
//! Holding a key is **self-paced**: OS auto-repeats are ignored, and advancing is
//! driven from the frame loop based on which key is currently held, capped at the
//! display refresh. This means releasing a key stops within one decode — no
//! backlog of queued repeats. Phase 3 adds the decode pool + prefetch ring so big
//! photos are decoded *ahead* of you (the per-photo wait disappears); random
//! navigation (enter) comes later.
//!
//! Usage:
//!   cargo run -p pb-app --release -- "D:\Pictures\2003\Halloween"
//!   cargo run -p pb-app --release -- "D:\Pictures" -r          # recurse subfolders
//!   cargo run -p pb-app --release -- "D:\Pictures" -r --windowed

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

use pb_core::Playlist;
use pb_decode::{decode_image_file, FitBox};
use pb_render::{test_pattern, Renderer, ScaleMode, WgpuRenderer};

struct Active {
    window: Arc<Window>,
    renderer: WgpuRenderer,
}

struct App {
    windowed: bool,
    paths: Vec<PathBuf>,
    playlist: Playlist,
    active: Option<Active>,
    /// Physical keys currently held (OS auto-repeat ignored).
    held: HashSet<KeyCode>,
    /// When the last self-paced advance happened, to cap the rate.
    last_advance: Option<Instant>,
    /// Minimum time between advances while holding (≈ one display refresh).
    frame_interval: Duration,
    /// Decode-to-fit target = the display size; photos are downscaled to it.
    fit: Option<FitBox>,
    /// Fit-to-screen (default) vs. original 1:1 centered.
    scale_mode: ScaleMode,
}

impl App {
    fn new(windowed: bool, paths: Vec<PathBuf>) -> Self {
        let playlist = Playlist::new(paths.len(), 0);
        Self {
            windowed,
            paths,
            playlist,
            active: None,
            held: HashSet::new(),
            last_advance: None,
            frame_interval: Duration::from_micros(8_333), // ~120 Hz until we read the real rate
            fit: None,
            scale_mode: ScaleMode::Fit,
        }
    }

    /// The decode-to-fit target for the current mode: the display size in Fit
    /// mode (downscale large photos), or None in Original mode (decode full-res).
    fn decode_fit(&self) -> Option<FitBox> {
        match self.scale_mode {
            ScaleMode::Fit => self.fit,
            ScaleMode::Original => None,
        }
    }

    /// Toggle between fit-to-screen and original 1:1 (re-decodes at the right
    /// resolution: full-res for Original, downscaled for Fit).
    fn toggle_scale(&mut self, event_loop: &ActiveEventLoop) {
        self.scale_mode = match self.scale_mode {
            ScaleMode::Fit => ScaleMode::Original,
            ScaleMode::Original => ScaleMode::Fit,
        };
        if let Some(a) = self.active.as_mut() {
            a.renderer.set_scale_mode(self.scale_mode);
        }
        self.load_current();
        self.draw(event_loop);
    }

    /// Decode the image at the current cursor (or a fallback) for first display.
    fn initial_image(&self) -> (Vec<u8>, u32, u32, String) {
        match self.playlist.current() {
            Some(idx) => match decode_image_file(&self.paths[idx], self.decode_fit()) {
                Ok(img) => (
                    img.pixels,
                    img.width,
                    img.height,
                    title_for(&self.paths[idx], idx, self.paths.len()),
                ),
                Err(e) => {
                    eprintln!("decode failed: {}: {e}", self.paths[idx].display());
                    let p = test_pattern(1600, 1000);
                    (p, 1600, 1000, "PhotoBlaze (decode error)".to_string())
                }
            },
            None => {
                let p = test_pattern(1600, 1000);
                (p, 1600, 1000, "PhotoBlaze (no images)".to_string())
            }
        }
    }

    /// Decode the current cursor image into the renderer. On a decode error the
    /// previous image stays on screen.
    fn load_current(&mut self) {
        let Some(idx) = self.playlist.current() else {
            return;
        };
        match decode_image_file(&self.paths[idx], self.decode_fit()) {
            Ok(img) => {
                if let Some(a) = self.active.as_mut() {
                    a.renderer.set_image(&img.pixels, img.width, img.height);
                    a.window
                        .set_title(&title_for(&self.paths[idx], idx, self.paths.len()));
                }
            }
            Err(e) => eprintln!("decode failed: {}: {e}", self.paths[idx].display()),
        }
    }

    /// Render one frame and reclaim the previous image's GPU texture.
    fn draw(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(a) = self.active.as_mut() {
            if let Err(e) = a.renderer.render() {
                eprintln!("fatal render error: {e:?}");
                event_loop.exit();
            }
            a.renderer.poll();
        }
    }

    /// Advance one photo (every photo is shown; slow ones just take their moment)
    /// and draw it.
    fn navigate(&mut self, forward: bool, event_loop: &ActiveEventLoop) {
        if forward {
            self.playlist.next();
        } else {
            self.playlist.prev();
        }
        self.load_current();
        self.draw(event_loop);
        self.last_advance = Some(Instant::now());
    }

    /// Which way we're currently paging, from held keys (both/neither = idle).
    fn held_direction(&self) -> Option<bool> {
        let fwd =
            self.held.contains(&KeyCode::ArrowRight) || self.held.contains(&KeyCode::Space);
        let bwd =
            self.held.contains(&KeyCode::ArrowLeft) || self.held.contains(&KeyCode::Backspace);
        match (fwd, bwd) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
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
        }

        // Present the first frame WHILE HIDDEN, then reveal — no white startup gap.
        let _ = renderer.render();
        window.set_visible(true);
        window.request_redraw();

        self.active = Some(Active { window, renderer });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                // Future decodes target the new size; the current image refits on
                // the GPU until the next navigation re-decodes at the new size.
                self.fit = Some(FitBox {
                    max_width: size.width.max(1),
                    max_height: size.height.max(1),
                });
                if let Some(a) = self.active.as_mut() {
                    a.renderer.resize(size.width, size.height);
                }
                self.draw(event_loop);
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
                                self.navigate(true, event_loop);
                            }
                            KeyCode::Backspace | KeyCode::ArrowLeft => {
                                self.held.insert(code);
                                self.navigate(false, event_loop);
                            }
                            // Toggle fit-to-screen <-> original 1:1 (centered).
                            KeyCode::Digit0 | KeyCode::KeyO => self.toggle_scale(event_loop),
                            _ => {}
                        }
                    }
                }
                ElementState::Released => {
                    self.held.remove(&code);
                }
            },

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Self-paced advance: while a nav key is held, page at most once per frame
        // interval. Releasing clears `held` and we go idle immediately.
        match self.held_direction() {
            Some(forward) => {
                let now = Instant::now();
                match self.last_advance {
                    Some(t) if now < t + self.frame_interval => {
                        event_loop.set_control_flow(ControlFlow::WaitUntil(t + self.frame_interval));
                    }
                    _ => {
                        self.navigate(forward, event_loop);
                        event_loop.set_control_flow(ControlFlow::WaitUntil(
                            Instant::now() + self.frame_interval,
                        ));
                    }
                }
            }
            None => {
                self.last_advance = None;
                event_loop.set_control_flow(ControlFlow::Wait);
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

    let mut app = App::new(windowed, paths);
    event_loop.run_app(&mut app).expect("event loop");
}
