//! The **subtitle engine** (task #90.3): the runtime that turns a playback position into
//! a bitmap on screen.
//!
//! This is the piece that joins the parts: discovery (#90.1) finds the file, [`CueTrack`]
//! (#90.2) parses it, [`resolve_track`]/[`place`] (#90.3) decide what and where, and the
//! rasterizer (`pb_hud::subtitle`) draws it. Both shells then read [`Self::bitmap`] +
//! [`Self::rect`] and composite — the wgpu overlay on winit, a SwiftUI overlay on macOS.
//!
//! **Nothing expensive happens on the event loop.** Two jobs go to workers:
//! - Building the rasterizer — `FontSystem::new()` is **261 ms** (spike). Built **once**,
//!   lazily, and kept: paying that per video would be worse than paying it once.
//! - Loading + parsing a sidecar — an `fs::read` of an arbitrary file, per item.
//!
//! Until they land, no subtitle draws. A cue arriving 200 ms into the first play of a
//! session is invisible; a frozen window is not.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use pb_hud::subtitle::{SubtitleBitmap, SubtitleRasterizer};
use pb_source::PhotoSource;

use crate::cues::CueTrack;
use crate::subtitle::{place, Rect, SubtitleMode, SubtitleStyle};

/// What a worker loaded for one item.
struct LoadedCues {
    item: usize,
    gen: u64,
    cues: Option<CueTrack>,
}

#[derive(Default)]
pub struct SubtitleEngine {
    pub mode: SubtitleMode,
    pub style: SubtitleStyle,
    /// Built once on a worker; `None` until it lands.
    raster: Option<SubtitleRasterizer>,
    raster_rx: Option<Receiver<SubtitleRasterizer>>,
    /// The cues for `loaded_for`, once a worker has parsed them.
    cues: Option<CueTrack>,
    loaded_for: Option<usize>,
    cues_rx: Option<Receiver<LoadedCues>>,
    /// The deck generation the load was requested in — a load that lands after a rebuild
    /// describes a different file at that index and is dropped, exactly like the Details
    /// probe's rule.
    load_gen: u64,
    /// Bumped whenever the bitmap changes, so a shell can skip an unchanged transfer
    /// (the `thumb_gen` pattern). `0` = nothing to show.
    gen: u64,
    bitmap: Option<SubtitleBitmap>,
    rect: Option<Rect>,
    /// `PB_SUBTITLE_TRACE=1` — see [`Self::trace`].
    tracing: bool,
    last_trace: Option<String>,
}

impl SubtitleEngine {
    pub fn new(mode: SubtitleMode) -> Self {
        Self {
            mode,
            ..Default::default()
        }
    }

    /// Start from the user's persisted preference (`C` / View ▸ Subtitles).
    ///
    /// `PB_SUBTITLE_TRACE=1` additionally prints why nothing is on screen — a diagnostic
    /// only; it never turns subtitles on.
    ///
    /// `Automatic` does **not** implement the forced-only rule yet: it shows the first
    /// renderable sidecar. That needs the catalog, which [`crate::subtitle::resolve_track`]
    /// is already written against (#99).
    pub fn from_settings(on: bool) -> Self {
        Self {
            mode: if on {
                SubtitleMode::Automatic
            } else {
                SubtitleMode::Off
            },
            tracing: std::env::var_os("PB_SUBTITLE_TRACE").is_some_and(|v| v == "1"),
            ..Default::default()
        }
    }

    /// `PB_SUBTITLE_TRACE=1` prints why nothing is on screen. Deduped on the message, so a
    /// steady state prints once rather than 120×/second — the log stays readable and the
    /// tick stays free (the closure isn't even called when tracing is off).
    pub fn trace(&mut self, msg: impl FnOnce() -> String) {
        if !self.tracing {
            return;
        }
        let m = msg();
        if self.last_trace.as_deref() != Some(m.as_str()) {
            eprintln!("[subtitles] {m}");
            self.last_trace = Some(m);
        }
    }

    /// Test-only: put the engine in a "showing" state directly, so the *wiring's*
    /// clear-on-exit rules can be tested without decoding a video. The real path only
    /// gets here through `update()`.
    #[cfg(test)]
    pub fn force_showing_for_test(&mut self) {
        self.bitmap = Some(SubtitleBitmap {
            rgba: vec![255; 4],
            w: 1,
            h: 1,
        });
        self.rect = Some(Rect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        });
        self.gen = 1;
    }

    /// The overlay to draw, or `None`. Physical pixels.
    pub fn bitmap(&self) -> Option<&SubtitleBitmap> {
        self.bitmap.as_ref()
    }

    /// Where to draw it, in **logical points** — what a UI toolkit positions in. The
    /// bitmap stays physical, so drawing it into this rect is what makes it Retina-sharp
    /// rather than a scaled-up blur.
    pub fn rect(&self) -> Option<Rect> {
        self.rect
    }

    /// Changes whenever the bitmap does; `0` = nothing showing.
    pub fn gen(&self) -> u64 {
        self.gen
    }

    /// Is a worker still doing something? Keeps the tick alive so results land promptly.
    pub fn working(&self) -> bool {
        self.raster_rx.is_some() || self.cues_rx.is_some()
    }

    /// Drop everything for the current item — navigation, stop, a new session. The next
    /// `ensure_loaded` re-discovers.
    pub fn clear_item(&mut self) {
        self.cues = None;
        self.loaded_for = None;
        self.cues_rx = None;
        self.hide();
    }

    /// Stop showing, without forgetting the track. Idempotent — calling it every tick
    /// while nothing is showing costs one `Option` check.
    pub fn hide(&mut self) {
        if self.bitmap.is_some() {
            self.bitmap = None;
            self.rect = None;
            self.gen = self.gen.wrapping_add(1).max(1);
        }
    }

    /// Kick the workers for `item` if needed. Idempotent and cheap when warm.
    pub fn ensure_loaded(&mut self, source: &Arc<dyn PhotoSource>, item: usize, deck_gen: u64) {
        // The rasterizer: once, ever. 261 ms is worth paying a single time.
        if self.raster.is_none() && self.raster_rx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(SubtitleRasterizer::new());
            });
            self.raster_rx = Some(rx);
        }
        // The cues: per item.
        if self.loaded_for == Some(item) || self.cues_rx.is_some() {
            return;
        }
        let source = Arc::clone(source);
        let (tx, rx) = std::sync::mpsc::channel();
        self.load_gen = deck_gen;
        std::thread::spawn(move || {
            let cues = load_cues(source.as_ref(), item);
            let _ = tx.send(LoadedCues {
                item,
                gen: deck_gen,
                cues,
            });
        });
        self.cues_rx = Some(rx);
    }

    /// Pick up whatever the workers finished. Call each tick.
    pub fn poll(&mut self, deck_gen: u64) {
        if let Some(rx) = &self.raster_rx {
            match rx.try_recv() {
                Ok(r) => {
                    self.raster = Some(r);
                    self.raster_rx = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.raster_rx = None, // worker died
            }
        }
        if let Some(rx) = &self.cues_rx {
            match rx.try_recv() {
                Ok(l) => {
                    self.cues_rx = None;
                    // Same staleness rule as the Details probe: a load that raced a deck
                    // rebuild names a different file at that index.
                    if l.gen == deck_gen && l.gen == self.load_gen {
                        let n = l.cues.as_ref().map_or(0, |c| c.len());
                        self.trace(|| format!("loaded item {}: {n} cues", l.item));
                        self.cues = l.cues;
                        self.loaded_for = Some(l.item);
                    } else {
                        let (g, lg) = (l.gen, self.load_gen);
                        self.trace(|| {
                            format!(
                                "dropped a stale load (gen {g}, load_gen {lg}, deck {deck_gen})"
                            )
                        });
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.cues_rx = None,
            }
        }
    }

    /// Rebuild the overlay for playback position `t`. Returns true if the bitmap changed.
    ///
    /// `viewport` and `video` are **physical px**; `scale` converts the placement to the
    /// logical points a UI toolkit wants.
    pub fn update(
        &mut self,
        t: Duration,
        viewport: (f32, f32),
        video: Rect,
        controls_h: f32,
        scale: f32,
    ) -> bool {
        let before = self.gen;
        // Off means off — the earliest possible exit, and zero work when subtitles aren't
        // in use (the plan's "zero per-frame work when off").
        if self.mode == SubtitleMode::Off {
            self.hide();
            return self.gen != before;
        }
        if self.cues.is_none() || self.raster.is_none() {
            let (c, r) = (self.cues.is_some(), self.raster.is_some());
            self.trace(|| format!("waiting: cues={c} rasterizer={r}"));
            self.hide();
            return self.gen != before;
        }
        let cues = self.cues.as_ref().expect("checked above");

        // Every active cue's lines, stacked in source order (overlaps are kept, #90.2).
        let lines: Vec<String> = cues
            .active_at(t)
            .flat_map(|c| c.lines.iter().cloned())
            .collect();
        if lines.is_empty() {
            self.hide();
            return self.gen != before;
        }

        let params = self.style.to_params(viewport);
        // The rasterizer caches on (text, params), so an unchanged cue costs a compare —
        // this is safe to call every tick even though it only *works* on a change.
        let raster = self.raster.as_mut().expect("checked above");
        let Some(bmp) = raster.render(&lines, &params) else {
            self.trace(|| format!("the rasterizer drew nothing for {lines:?}"));
            self.hide();
            return self.gen != before;
        };
        let (bw, bh) = (bmp.w, bmp.h);
        let changed = self.bitmap.as_ref() != Some(bmp);
        if changed {
            self.bitmap = Some(bmp.clone());
            self.gen = self.gen.wrapping_add(1).max(1);
            self.trace(|| format!("drew {bw}x{bh} at {t:?}: {lines:?}"));
        }
        // Place in physical px, then hand the shell logical points.
        let px = place(
            viewport,
            video,
            (bw as f32, bh as f32),
            &self.style,
            controls_h,
        );
        let s = scale.max(0.01);
        let rect = Rect {
            x: px.x / s,
            y: px.y / s,
            w: px.w / s,
            h: px.h / s,
        };
        if self.rect != Some(rect) {
            self.rect = Some(rect);
            self.trace(|| {
                format!(
                    "placed at {rect:?} pts (video {video:?} px, viewport {viewport:?} px, scale {s})"
                )
            });
            // A move with no repaint still needs the shell to reposition.
            if !changed {
                self.gen = self.gen.wrapping_add(1).max(1);
            }
        }
        self.gen != before
    }
}

/// The worker's job: find this item's sidecars, pick one, read it, parse it.
///
/// Read-only and RAM-only (privacy #2): the bytes are parsed and dropped, and nothing
/// about which subtitle was shown is remembered.
fn load_cues(source: &dyn PhotoSource, item: usize) -> Option<CueTrack> {
    let found = crate::sidecar::discover(source, item);
    // Until the #99 picker exists there is nothing to choose *with*, so take the first
    // renderable one. This is the temporary bit: #99 replaces it with a real selection.
    let m = found
        .iter()
        .find(|m| pb_decode::tracks::subtitle_capability(m.codec_raw).is_renderable_text())?;
    let bytes = read_sidecar(source, item, &m.origin)?;
    let text = crate::sidecar::decode_sidecar_text(&bytes);
    let mut track = CueTrack::parse(&text, m.codec_raw);
    track.set_forced(m.flags.forced);
    (!track.is_empty()).then_some(track)
}

/// Read a sidecar back through the source that found it — never by building a path
/// ourselves, so the source's own containment rules still apply.
fn read_sidecar(
    source: &dyn PhotoSource,
    item: usize,
    origin: &pb_decode::tracks::SidecarOrigin,
) -> Option<Vec<u8>> {
    use pb_decode::tracks::SidecarOrigin;
    let name = match origin {
        SidecarOrigin::Path(p) => p.file_name()?.to_str()?.to_string(),
        SidecarOrigin::ArchiveEntry { entry, .. } => entry.clone(),
    };
    source.sibling_bytes(item, &name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cues::SubtitleCue;

    fn cue_track() -> CueTrack {
        CueTrack::from_cues(vec![SubtitleCue {
            start: Duration::from_secs(1),
            end: Duration::from_secs(2),
            lines: vec!["Hello there".into()],
            forced: false,
            source_order: 0,
        }])
    }

    fn viewport() -> ((f32, f32), Rect) {
        (
            (1000.0, 500.0),
            Rect {
                x: 0.0,
                y: 50.0,
                w: 1000.0,
                h: 400.0,
            },
        )
    }

    /// Off means off — and it exits before touching the rasterizer, so subtitles cost
    /// nothing at all when unused.
    #[test]
    fn off_shows_nothing_and_needs_no_rasterizer() {
        let mut e = SubtitleEngine {
            mode: SubtitleMode::Off,
            cues: Some(cue_track()),
            ..Default::default()
        };
        let (vp, video) = viewport();
        e.update(Duration::from_millis(1500), vp, video, 0.0, 1.0);
        assert!(e.bitmap().is_none());
        assert_eq!(e.gen(), 0);
        assert!(e.raster.is_none(), "never even built one");
    }

    /// Nothing draws until the rasterizer worker lands — a missing font system must be a
    /// late subtitle, not a panic or a stall.
    #[test]
    fn nothing_draws_before_the_rasterizer_arrives() {
        let mut e = SubtitleEngine {
            mode: SubtitleMode::Automatic,
            cues: Some(cue_track()),
            ..Default::default()
        };
        let (vp, video) = viewport();
        assert!(!e.update(Duration::from_millis(1500), vp, video, 0.0, 1.0));
        assert!(e.bitmap().is_none());
    }

    /// The real path: a cue in window renders, and out of window clears — with the
    /// generation bumping exactly on the transitions.
    #[test]
    fn a_cue_renders_in_its_window_and_clears_outside_it() {
        let mut e = SubtitleEngine {
            mode: SubtitleMode::Automatic,
            cues: Some(cue_track()),
            raster: Some(SubtitleRasterizer::new()),
            ..Default::default()
        };
        let (vp, video) = viewport();

        // Before the cue: nothing.
        e.update(Duration::from_millis(500), vp, video, 0.0, 1.0);
        assert!(e.bitmap().is_none());
        let g0 = e.gen();

        // In the cue: a bitmap and a rect appear.
        assert!(e.update(Duration::from_millis(1500), vp, video, 0.0, 1.0));
        let b = e.bitmap().expect("drew").clone();
        assert!(b.w > 0 && b.h > 0);
        assert!(e.gen() > g0);
        let r = e.rect().expect("placed");
        assert!(r.y > 0.0 && r.bottom() <= 500.0, "on screen: {r:?}");

        // Still in the cue: unchanged, no churn.
        let g1 = e.gen();
        assert!(!e.update(Duration::from_millis(1600), vp, video, 0.0, 1.0));
        assert_eq!(e.gen(), g1, "an unchanged cue must not bump the generation");
        assert_eq!(e.bitmap(), Some(&b));

        // Past the cue: gone.
        assert!(e.update(Duration::from_millis(2500), vp, video, 0.0, 1.0));
        assert!(e.bitmap().is_none());
        assert!(e.gen() > g1);
    }

    /// The rect is in logical points while the bitmap stays physical — that ratio IS the
    /// sharpness. A 2× display gets a 2× bitmap drawn into the same point-sized rect.
    #[test]
    fn the_rect_is_logical_points_while_the_bitmap_is_physical() {
        let mut e = SubtitleEngine {
            mode: SubtitleMode::Automatic,
            cues: Some(cue_track()),
            raster: Some(SubtitleRasterizer::new()),
            ..Default::default()
        };
        let (vp, video) = viewport();
        e.update(Duration::from_millis(1500), vp, video, 0.0, 2.0);
        let b = e.bitmap().unwrap();
        let r = e.rect().unwrap();
        assert!(
            (r.w - b.w as f32 / 2.0).abs() < 0.01,
            "the rect is half the pixels at 2x: rect {} px {}",
            r.w,
            b.w
        );
    }

    /// The whole load path over a **real** filesystem, not a fake: an `.srt` beside a
    /// video is discovered through `PhotoSource`, read back through it, decoded, and
    /// parsed into cues. Every piece between "a file exists on disk" and "the engine has
    /// a track" is exercised here — the parts a fake source would paper over.
    #[test]
    fn a_real_srt_beside_a_real_video_loads_end_to_end() {
        let dir = std::env::temp_dir().join(format!("pb-sub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("Show.S01E01.1080p.mkv");
        std::fs::write(&video, b"not a real mkv, never decoded").unwrap();
        std::fs::write(
            dir.join("Show.S01E01.1080p.eng.srt"),
            "1\n00:00:01,000 --> 00:00:02,000\nHello there\n",
        )
        .unwrap();

        let source = pb_source::FsSource::new(vec![video]);
        let track = load_cues(&source, 0).expect("found, read, and parsed the sidecar");
        assert_eq!(track.len(), 1);
        let cue = track.active_at(Duration::from_millis(1500)).next().unwrap();
        assert_eq!(cue.lines, vec!["Hello there".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_item_forgets_the_track_and_hides() {
        let mut e = SubtitleEngine {
            mode: SubtitleMode::Automatic,
            cues: Some(cue_track()),
            loaded_for: Some(3),
            raster: Some(SubtitleRasterizer::new()),
            ..Default::default()
        };
        let (vp, video) = viewport();
        e.update(Duration::from_millis(1500), vp, video, 0.0, 1.0);
        assert!(e.bitmap().is_some());
        e.clear_item();
        assert!(e.bitmap().is_none());
        assert!(e.cues.is_none());
        assert_eq!(e.loaded_for, None);
    }

    /// A cue load that lands after a deck rebuild describes a different file — dropped,
    /// the same rule the Details probe applies.
    #[test]
    fn a_cue_load_landing_after_a_deck_rebuild_is_dropped() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(LoadedCues {
            item: 1,
            gen: 3,
            cues: Some(cue_track()),
        })
        .unwrap();
        let mut e = SubtitleEngine {
            cues_rx: Some(rx),
            load_gen: 3,
            ..Default::default()
        };
        e.poll(4); // the deck was rebuilt while the worker ran
        assert!(e.cues.is_none(), "a stale load must not be installed");
        assert_eq!(e.loaded_for, None);
    }

    #[test]
    fn a_matching_cue_load_installs() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(LoadedCues {
            item: 1,
            gen: 3,
            cues: Some(cue_track()),
        })
        .unwrap();
        let mut e = SubtitleEngine {
            cues_rx: Some(rx),
            load_gen: 3,
            ..Default::default()
        };
        e.poll(3);
        assert!(e.cues.is_some());
        assert_eq!(e.loaded_for, Some(1));
        assert!(!e.working(), "the receiver is consumed");
    }
}
