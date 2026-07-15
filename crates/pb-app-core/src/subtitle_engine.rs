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
//! - Loading cues — an `fs::read` for a sidecar, or a **full walk of the container** for
//!   an embedded stream (39 s on the corpus MKV over SMB).
//!
//! Until they land, no subtitle draws. A cue arriving 200 ms into the first play of a
//! session is invisible; a frozen window is not.
//!
//! ## Cues stream; they do not arrive in a lump
//!
//! An embedded stream's reader hands cues over **as it finds them**, in presentation
//! order, and it walks the file ~70× faster than playback consumes it — so the first cue
//! is on screen in well under a second and the reader stays far ahead of the playhead
//! forever. Waiting for the whole 39 s read would be indistinguishable from broken. A
//! sidecar sends one batch and closes, through the same channel: no special case.
//! See `pb_decode::ffmpeg::cues` for the ratio this rests on.
//!
//! ## Selection goes through the catalog
//!
//! The engine does **not** discover anything itself. It reads the Details probe's
//! [`MediaTrackCatalog`] — which already holds the container's own subtitle streams *and*
//! the sidecars beside it, in one id namespace — and asks [`resolve_track`]. That is what
//! lets an embedded track be selected at all (#90.2), and what makes `Automatic`'s rules
//! real rather than "the first sidecar we tripped over".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use pb_decode::tracks::{MediaTrackCatalog, TrackLocator};
use pb_hud::subtitle::{SubtitleBitmap, SubtitleRasterizer};
use pb_source::ItemSource;

use crate::cues::{CueTrack, SubtitleCue};
use crate::subtitle::{place, resolve_track, Rect, SubtitleMode, SubtitleStyle};

/// Which track's cues are loaded / loading: an item plus a track's `local_id`.
///
/// The pair, not just the item — switching track (#99's `Shift+C`) has to restart the
/// load even though the item never changed.
type TrackKey = (usize, u64);

/// A cue load in flight.
///
/// Cues arrive in **batches**, not one lump: an embedded stream's reader walks the whole
/// container (39 s on the corpus MKV) and hands cues over as it finds them, staying far
/// ahead of the playhead. See `pb_decode::ffmpeg::cues`. A sidecar sends exactly one
/// batch and closes — same channel, no special case.
struct CueLoad {
    key: TrackKey,
    /// The deck generation this was started in. A load that outlives a rebuild describes
    /// a different file at that index — the Details probe's rule.
    gen: u64,
    rx: Receiver<Vec<SubtitleCue>>,
    cancel: Arc<AtomicBool>,
}

impl Drop for CueLoad {
    /// **Dropping the load cancels the read.** This is the whole cancellation design: a
    /// nav, a track switch, or a deck rebuild just replaces/clears the `Option`, and the
    /// worker aborts inside libav's blocking I/O rather than chewing through 20 GB of a
    /// film nobody is watching any more. Nothing has to remember to call anything.
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub struct SubtitleEngine {
    pub mode: SubtitleMode,
    pub style: SubtitleStyle,
    /// Built once on a worker; `None` until it lands.
    raster: Option<SubtitleRasterizer>,
    raster_rx: Option<Receiver<SubtitleRasterizer>>,
    /// The cues accumulated so far. `Some` but empty while a load is still filling in.
    cues: Option<CueTrack>,
    /// Set once a load has *finished*. While one is in flight, `load` is the guard that
    /// stops `ensure_loaded` from starting another.
    loaded_for: Option<TrackKey>,
    load: Option<CueLoad>,
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
        self.raster_rx.is_some() || self.load.is_some()
    }

    /// The track whose cues are loaded or loading, if any — what the #99 picker checks to
    /// put its checkmark on the right row.
    pub fn active_track(&self) -> Option<u64> {
        self.load
            .as_ref()
            .map(|l| l.key.1)
            .or(self.loaded_for.map(|k| k.1))
    }

    /// Drop everything for the current item — navigation, stop, a new session. The next
    /// `ensure_loaded` re-discovers. Cancels an in-flight read (see [`CueLoad::drop`]).
    pub fn clear_item(&mut self) {
        self.cues = None;
        self.loaded_for = None;
        self.load = None;
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
    ///
    /// `catalog` is the item's [`MediaTrackCatalog`] — the Details probe's, which already
    /// merges the container's own subtitle streams *and* the sidecars beside it into one
    /// id namespace. Consulting it is what lets an embedded track be selected at all
    /// (#90.2); before this, the engine did its own sidecar discovery and could not see
    /// inside the container. `None` = the probe hasn't landed yet; nothing to do but wait.
    ///
    /// `audio_language` is what `Automatic`'s forced rule matches against.
    pub fn ensure_loaded(
        &mut self,
        source: &Arc<dyn ItemSource>,
        item: usize,
        deck_gen: u64,
        catalog: Option<&MediaTrackCatalog>,
        audio_language: Option<&str>,
    ) {
        // The rasterizer: once, ever. 261 ms is worth paying a single time.
        if self.raster.is_none() && self.raster_rx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(SubtitleRasterizer::new());
            });
            self.raster_rx = Some(rx);
        }

        let Some(catalog) = catalog else {
            self.trace(|| "waiting: the track catalog hasn't been probed yet".into());
            return;
        };
        // The selection. `resolve_track` was written against the catalog long before
        // anything called it; this is that bridge finally carrying traffic.
        let Some(track) = resolve_track(self.mode, catalog, audio_language) else {
            let n = catalog.subtitles.tracks.len();
            self.trace(|| format!("no track resolved from {n} subtitle track(s)"));
            // Not an error — "show nothing" is a normal answer. But it must actually
            // show nothing, including forgetting a track selected a moment ago.
            self.cues = None;
            self.loaded_for = None;
            self.load = None;
            return;
        };
        let key: TrackKey = (item, track.id.local_id);

        // Warm, or already in flight for this exact track in this exact generation.
        if self.loaded_for == Some(key) {
            return;
        }
        if let Some(l) = &self.load {
            if l.key == key && l.gen == deck_gen {
                return;
            }
        }

        let Some(locator) = catalog.locator(track.id).cloned() else {
            self.trace(|| "the resolved track has no locator — cannot read it".into());
            return;
        };
        let codec_raw = track.codec_raw.clone();
        let forced = track.flags.forced;
        let path = source.path(item).map(|p| p.to_path_buf());
        let name = track.language.clone().unwrap_or_else(|| "?".into());

        // Replacing the Option cancels any previous read (see `CueLoad::drop`) — a track
        // switch must not leave the old reader racing the new one.
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let source = Arc::clone(source);
        let c2 = Arc::clone(&cancel);
        self.trace(|| {
            format!(
                "loading track {} ({name}, {codec_raw}) — {locator:?}",
                key.1
            )
        });
        std::thread::spawn(move || {
            stream_cues(
                source.as_ref(),
                item,
                &locator,
                &codec_raw,
                forced,
                path.as_deref(),
                c2,
                &tx,
            );
        });
        // Some, not None: a load that has started but delivered nothing yet is a real,
        // distinct state from "no track". `update` reads it as "no cue right now".
        self.cues = Some(CueTrack::default());
        self.loaded_for = None;
        self.load = Some(CueLoad {
            key,
            gen: deck_gen,
            rx,
            cancel,
        });
    }

    /// Pick up whatever the workers produced. Call each tick.
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

        let Some(load) = &mut self.load else { return };

        // The staleness rule, applied to the load rather than to each message: a deck
        // rebuild reassigned the indices, so this load names a different file now.
        if load.gen != deck_gen {
            let g = load.gen;
            self.trace(|| format!("dropped a stale load (gen {g}, deck {deck_gen})"));
            self.clear_item();
            return;
        }

        // Drain everything waiting — a fast reader can deliver several batches between
        // two ticks, and leaving them queued would make cues lag the playhead for no
        // reason.
        let mut batches = Vec::new();
        let finished = loop {
            match load.rx.try_recv() {
                Ok(b) => batches.push(b),
                Err(TryRecvError::Empty) => break false,
                // The sender dropped: the read finished, was cancelled, or the worker
                // died. Whichever — there is nothing more coming, and what already
                // arrived is still good.
                Err(TryRecvError::Disconnected) => break true,
            }
        };

        let key = load.key;
        if !batches.is_empty() {
            let cues = self.cues.get_or_insert_with(CueTrack::default);
            for b in batches {
                cues.extend(b);
            }
            let n = cues.len();
            self.trace(|| format!("track {} now has {n} cue(s)", key.1));
        }
        if finished {
            self.loaded_for = Some(key);
            self.load = None;
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
        // `Some` but empty is NOT a wait state — it is a load that has started and not
        // reached this part of the film yet, which reads the same as "no cue right now".
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

/// The worker's job: read one track's cues and send them, in batches, until done or
/// cancelled.
///
/// The two tiers meet here and nowhere else — a sidecar is one batch, an embedded stream
/// is many — and both hand over the same [`SubtitleCue`], normalized by the same
/// [`CueTrack`]. Everything downstream is tier-blind.
///
/// Read-only and RAM-only (privacy #2): the bytes are parsed and dropped, and nothing
/// about which subtitle was shown is remembered.
#[allow(clippy::too_many_arguments)]
fn stream_cues(
    source: &dyn ItemSource,
    item: usize,
    locator: &TrackLocator,
    codec_raw: &str,
    forced: bool,
    // Only the embedded tier reads these, and it only exists with `ffprobe`. A build
    // without it still serves sidecars — which is a real configuration (`--no-ffvideo`),
    // not a hypothetical.
    #[cfg_attr(not(feature = "ffprobe"), allow(unused_variables))] path: Option<&std::path::Path>,
    #[cfg_attr(not(feature = "ffprobe"), allow(unused_variables))] cancel: Arc<AtomicBool>,
    tx: &std::sync::mpsc::Sender<Vec<SubtitleCue>>,
) {
    match locator {
        // Beside the file. Parsed in Rust, never through FFmpeg — which is also what
        // makes sidecars work on Windows, whose trimmed FFmpeg has no srt/webvtt/ass
        // *demuxer* and so cannot open a standalone `.srt` at all (task #100.8).
        TrackLocator::Sidecar(origin) => {
            let Some(bytes) = read_sidecar(source, item, origin) else {
                return;
            };
            let text = crate::sidecar::decode_sidecar_text(&bytes);
            let mut cues = crate::cues::parse_cues(&text, codec_raw);
            for c in &mut cues {
                c.forced = forced;
            }
            let _ = tx.send(cues);
        }

        // Inside the container (#90.2).
        #[cfg(feature = "ffprobe")]
        TrackLocator::FfStream(index) => {
            // A real path only. An archive'd video would mean decompressing the whole
            // entry into RAM to demux it — the same reason archive videos already fall
            // back elsewhere. Sidecars inside archives still work (above), so a `.srt` in
            // a ZIP is not affected.
            let Some(p) = path else {
                return;
            };
            let input = pb_decode::video::VideoInput::Path(p.to_path_buf());
            let mut order = 0usize;
            let _ = pb_decode::ff_stream_subtitle_cues(&input, *index, cancel, |batch| {
                let mut cues = crate::cues::text_cues_to_cues(batch, order);
                order += cues.len();
                for c in &mut cues {
                    c.forced = forced;
                }
                // A closed channel means the engine dropped the load — stop reading.
                // This is the second half of `CueLoad::drop`'s cancellation: the flag
                // aborts a blocking read, this ends a running one.
                tx.send(cues).is_ok()
            });
        }

        // MF/AVFoundation stream locators carry no text we can read, and a bitmap track
        // never resolves in the first place. Nothing to do.
        _ => {}
    }
}

/// Read a sidecar back through the source that found it — never by building a path
/// ourselves, so the source's own containment rules still apply.
fn read_sidecar(
    source: &dyn ItemSource,
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

    /// A load in flight, wired to a channel the test drives by hand — so the poll/append
    /// rules can be exercised without a worker, a container, or a clock.
    fn loading(
        key: TrackKey,
        gen: u64,
    ) -> (SubtitleEngine, std::sync::mpsc::Sender<Vec<SubtitleCue>>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let e = SubtitleEngine {
            cues: Some(CueTrack::default()),
            load: Some(CueLoad {
                key,
                gen,
                rx,
                cancel: Arc::new(AtomicBool::new(false)),
            }),
            ..Default::default()
        };
        (e, tx)
    }

    /// A catalog with no tracks yet, at `generation` — the shape the Details probe
    /// produces before anything is appended to it.
    fn empty_catalog(generation: u64) -> MediaTrackCatalog {
        use pb_decode::tracks::{MediaBackend, TrackSet};
        MediaTrackCatalog::new(
            generation,
            MediaBackend::FFmpeg,
            TrackSet::complete(vec![]),
            TrackSet::complete(vec![]),
        )
    }

    fn cue_at(secs: u64, text: &str) -> SubtitleCue {
        SubtitleCue {
            start: Duration::from_secs(secs),
            end: Duration::from_secs(secs + 1),
            lines: vec![text.into()],
            forced: false,
            source_order: 0,
        }
    }

    /// The whole sidecar load path over a **real** filesystem, driven the way the app
    /// drives it: a catalog with a sidecar track, resolved through `resolve_track`, read
    /// back through `ItemSource`, decoded, parsed, and delivered over the load channel.
    /// Every piece between "a file exists on disk" and "the engine has cues" is real —
    /// the parts a fake source would paper over.
    #[test]
    fn a_real_srt_beside_a_real_video_loads_end_to_end() {
        let dir = std::env::temp_dir().join(format!("pb-sub-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("Show.S01E01.1080p.mkv");
        std::fs::write(&video, b"not a real mkv, never decoded").unwrap();
        std::fs::write(
            dir.join("Show.S01E01.1080p.eng.srt"),
            "1\n00:00:01,000 --> 00:00:02,000\nHello there\n",
        )
        .unwrap();

        let source: Arc<dyn ItemSource> = Arc::new(pb_source::FsSource::new(vec![video]));
        // The catalog the Details probe would have built, sidecars and all.
        let found = crate::sidecar::discover(source.as_ref(), 0);
        assert_eq!(found.len(), 1, "the .srt is discovered");
        let mut catalog = empty_catalog(7);
        let mut next = 0u64;
        for (track, locator) in crate::sidecar::sidecar_tracks(&found, 7, &mut next) {
            let id = track.id.local_id;
            catalog.subtitles.tracks.push(track);
            catalog.set_locator(id, locator);
        }

        let mut e = SubtitleEngine {
            mode: SubtitleMode::Automatic,
            ..Default::default()
        };
        e.ensure_loaded(&source, 0, 7, Some(&catalog), Some("en"));
        assert!(e.load.is_some(), "a load was started");

        // Wait for the worker rather than sleeping a magic number.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while e.load.is_some() && std::time::Instant::now() < deadline {
            e.poll(7);
        }
        assert_eq!(e.loaded_for, Some((0, 0)), "the load finished");
        let track = e.cues.as_ref().expect("cues");
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
            loaded_for: Some((3, 0)),
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

    /// A load that outlives a deck rebuild describes a different file at that index —
    /// dropped, the same rule the Details probe applies.
    #[test]
    fn a_cue_load_landing_after_a_deck_rebuild_is_dropped() {
        let (mut e, tx) = loading((1, 0), 3);
        tx.send(vec![cue_at(1, "Hello there")]).unwrap();
        e.poll(4); // the deck was rebuilt while the worker ran
        assert!(e.cues.is_none(), "a stale load must not be installed");
        assert_eq!(e.loaded_for, None);
        assert!(e.load.is_none(), "and the read is cancelled");
    }

    /// Cues arrive in **batches** and accumulate — this is what lets an embedded stream
    /// show its first cue in under a second instead of after a 39 s full-container walk.
    #[test]
    fn streamed_batches_accumulate_and_stay_ordered() {
        let (mut e, tx) = loading((1, 2), 3);

        tx.send(vec![cue_at(1, "first")]).unwrap();
        e.poll(3);
        assert_eq!(e.cues.as_ref().unwrap().len(), 1, "usable before it's done");
        assert!(e.working(), "still reading");
        assert_eq!(e.loaded_for, None, "not finished, so not marked loaded");

        tx.send(vec![cue_at(2, "second"), cue_at(3, "third")])
            .unwrap();
        e.poll(3);
        assert_eq!(e.cues.as_ref().unwrap().len(), 3);

        // Dropping the sender is the worker finishing.
        drop(tx);
        e.poll(3);
        assert_eq!(e.loaded_for, Some((1, 2)));
        assert!(!e.working());

        let cues = e.cues.as_ref().unwrap();
        let texts: Vec<&str> = cues.cues().iter().map(|c| c.lines[0].as_str()).collect();
        assert_eq!(texts, ["first", "second", "third"], "presentation order");
    }

    /// Several batches can queue between two ticks; the poll must take them all, or cues
    /// lag the playhead for no reason.
    #[test]
    fn one_poll_drains_every_queued_batch() {
        let (mut e, tx) = loading((1, 0), 3);
        for i in 0..5 {
            tx.send(vec![cue_at(i, "x")]).unwrap();
        }
        e.poll(3);
        assert_eq!(e.cues.as_ref().unwrap().len(), 5);
    }

    /// Dropping the load cancels the read — the whole cancellation design. A nav or a
    /// track switch must not leave a worker walking 20 GB of a film nobody is watching.
    #[test]
    fn dropping_a_load_cancels_the_read() {
        let (mut e, _tx) = loading((1, 0), 3);
        let cancel = Arc::clone(&e.load.as_ref().unwrap().cancel);
        assert!(!cancel.load(Ordering::Relaxed));
        e.clear_item();
        assert!(
            cancel.load(Ordering::Relaxed),
            "clear_item must cancel the in-flight read"
        );
    }

    /// A track switch (#99's `Shift+C`) restarts the load even though the item didn't
    /// change — which is why the key is (item, track), not item.
    #[test]
    fn switching_track_on_the_same_item_restarts_the_load() {
        let (mut e, _tx) = loading((1, 0), 3);
        let old = Arc::clone(&e.load.as_ref().unwrap().cancel);

        let dir = std::env::temp_dir().join(format!("pb-sub-sw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("Clip.mkv");
        std::fs::write(&video, b"x").unwrap();
        std::fs::write(
            dir.join("Clip.fr.srt"),
            "1\n00:00:01,000 --> 00:00:02,000\nBonjour\n",
        )
        .unwrap();
        let source: Arc<dyn ItemSource> = Arc::new(pb_source::FsSource::new(vec![video]));
        let found = crate::sidecar::discover(source.as_ref(), 0);
        let mut catalog = empty_catalog(3);
        let mut next = 9u64; // a different local_id than the load in flight
        for (track, locator) in crate::sidecar::sidecar_tracks(&found, 3, &mut next) {
            let id = track.id.local_id;
            catalog.subtitles.tracks.push(track);
            catalog.set_locator(id, locator);
        }
        e.mode = SubtitleMode::Track(catalog.subtitles.tracks[0].id);
        e.ensure_loaded(&source, 1, 3, Some(&catalog), Some("en"));

        assert!(old.load(Ordering::Relaxed), "the old read is cancelled");
        assert_eq!(
            e.load.as_ref().unwrap().key,
            (1, 9),
            "and the new one started"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Resolving to nothing must forget a track that *was* showing — the "one exit that
    /// clears" rule, applied to selection rather than to the tick.
    #[test]
    fn resolving_to_no_track_forgets_the_previous_one() {
        let source: Arc<dyn ItemSource> = Arc::new(pb_source::FsSource::new(vec![]));
        let mut e = SubtitleEngine {
            mode: SubtitleMode::Automatic,
            cues: Some(cue_track()),
            loaded_for: Some((0, 0)),
            ..Default::default()
        };
        // An empty catalog resolves to nothing.
        let catalog = empty_catalog(0);
        e.ensure_loaded(&source, 0, 0, Some(&catalog), Some("en"));
        assert!(e.cues.is_none());
        assert_eq!(e.loaded_for, None);
    }

    /// No catalog yet is a wait, not a decision: it must not clear cues that are already
    /// good, and must not start a load it cannot aim.
    #[test]
    fn no_catalog_yet_starts_nothing() {
        let source: Arc<dyn ItemSource> = Arc::new(pb_source::FsSource::new(vec![]));
        let mut e = SubtitleEngine {
            mode: SubtitleMode::Automatic,
            ..Default::default()
        };
        e.ensure_loaded(&source, 0, 0, None, None);
        assert!(e.load.is_none());
        assert!(e.cues.is_none());
    }
}
