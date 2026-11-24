//! **Export mode's live preview** (task #89, rung 3; plan of record:
//! `.taskmaster/docs/export-plan.md` § Rung 3) — the subsystem half: the preview's identity,
//! the flick's spring machine, and the preview worker. The `AppCore` half (what to preview
//! when, uploading and presenting the two sides, the keys) is
//! `app_core_impl/export_preview.rs` (the two-halves rule).
//!
//! Both sides of the flick come from the export pipeline (review F2): **the original** is the
//! prepared image before resize and encode; **the export** is the encoded file read back (for
//! PNG, which is lossless, the prepared pixels themselves). Both are uploaded the same way —
//! full resolution, mipped — so what changes on screen is exactly the codec and the resize.
//! The encoded bytes are the file's bytes (F10): Export… with the settings on screen writes
//! them as they are, and the size the panel shows is exact.
//!
//! RAM-only, like every cache here (privacy #2): the decoded source of the item on screen and
//! a small budget of encoded files live on the worker thread and die with it when Export mode
//! closes.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pb_decode::{DecodedImage, PixelFormat};
use pb_encode::{Caps, ExportNote, ExportSource, PrepareSpec, Samples};
use pb_source::ItemSource;

use crate::export_panel::{ExportSettings, FormatChoice};
use crate::frame_capture::{FrameKey, FrameSource};
use crate::pb_key::PbKey;

/// How long the item and settings must hold still before a preview is made: a slider drag or
/// a run of `]` presses makes one preview, not one per step, and blazing never makes any.
pub const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(120);
/// How often the loop polls while a preview is being made. A low-rate wake, never
/// `work_pending` — that would hold the pump at the frame rate for the whole encode (F8).
pub const PREVIEW_POLL: Duration = Duration::from_millis(16);
/// Sources larger than this aren't previewed: the worker holds several full-size copies at
/// once, so refuse before reserving. 120 MP covers every camera; it stops a 400 MP panorama.
pub const PREVIEW_MAX_PIXELS: u64 = 120_000_000;
/// A `Y` press held at least this long flicks back on release (a peek); shorter is a tap that
/// stays (plan § 3.11).
pub const SPRING_HOLD: Duration = Duration::from_millis(300);
/// Encoded files the worker keeps, so returning to a setting reads the file back instead of
/// encoding it again (a 30 MP PNG encode can take seconds).
const FILES_BUDGET: usize = 192 * 1024 * 1024;

/// Which side of the flick is on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Side {
    /// The decoded export — what Export mode shows by default.
    #[default]
    Export,
    /// The prepared original, before the codec and the resize.
    Original,
}

impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::Export => Side::Original,
            Side::Original => Side::Export,
        }
    }

    /// The name the panel and the flick's toast use.
    pub fn label(self) -> &'static str {
        match self {
            Side::Export => "Export",
            Side::Original => "Original",
        }
    }
}

/// Who is holding the flick down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpringOwner {
    Key(PbKey),
    Pointer,
}

/// The flick (plan § 3.11): a tap switches sides and stays there; a hold peeks at the other
/// side and flicks back on release. Only the press that started it can end it — a second key
/// or the pointer meanwhile is ignored, and so is auto-repeat (the shells filter it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spring {
    shown: Side,
    owner: Option<SpringOwner>,
    pressed_at: Option<Instant>,
}

impl Spring {
    pub fn shown(&self) -> Side {
        self.shown
    }

    /// Whether a press is holding it.
    pub fn held(&self) -> bool {
        self.owner.is_some()
    }

    /// A press: switch sides, owned by `who` until its release. Returns whether it switched.
    pub fn down(&mut self, who: SpringOwner, at: Instant) -> bool {
        if self.owner.is_some() {
            return false;
        }
        self.owner = Some(who);
        self.pressed_at = Some(at);
        self.shown = self.shown.other();
        true
    }

    /// A release: a short press stays (a tap); a long one flicks back (a peek). Only the
    /// owner's release counts. Returns whether it switched.
    pub fn up(&mut self, who: SpringOwner, at: Instant) -> bool {
        if self.owner != Some(who) {
            return false;
        }
        let held = self
            .pressed_at
            .map_or(Duration::ZERO, |t| at.saturating_duration_since(t));
        self.owner = None;
        self.pressed_at = None;
        if held >= SPRING_HOLD {
            self.shown = self.shown.other();
            true
        } else {
            false
        }
    }

    /// Show `side` (the panel's control, a menu, an accessibility action) — ignored while a
    /// press is holding it. Returns whether it switched.
    pub fn set(&mut self, side: Side) -> bool {
        if self.owner.is_some() || self.shown == side {
            return false;
        }
        self.shown = side;
        true
    }

    /// Back to the export, nobody holding: on a settings change, navigation, a rotation,
    /// focus loss, or leaving the mode — you always see the effect of what you changed.
    /// Returns whether it switched.
    pub fn reset(&mut self) -> bool {
        let switched = self.shown != Side::Export;
        *self = Spring::default();
        switched
    }
}

/// What a preview is *of*, whatever the settings: the item — by its file path, or for an
/// archive entry its container and name (never its deck index, which a delete shifts; never
/// the bare name, which two folders of a recursive scan can share) — the frame, and the deck's
/// content generation (a Save Rotation rewrites the file under the same name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceKey {
    pub path: Option<PathBuf>,
    pub container: Option<PathBuf>,
    /// The item's name as the source gives it — also what the export decode routes on.
    pub name: String,
    pub frame: FrameKey,
    pub content_gen: u64,
}

/// A preview's full identity: the source, the rotation baked in, and the settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewKey {
    pub source: SourceKey,
    pub rotation_cw: u8,
    pub settings: ExportSettings,
}

/// One preview to make.
pub struct PreviewRequest {
    pub gen: u64,
    pub key: PreviewKey,
    pub source: Arc<dyn ItemSource>,
    pub item: usize,
    /// A frame to capture instead of decoding the file (a video, animation, Live Photo or
    /// turned 3D model) — exactly what an export would take.
    pub frame: Option<FrameSource>,
    /// Make the original side too (it isn't already on the GPU).
    pub need_original: bool,
    /// Quarter turns the view applies beyond what's baked into the file — 0 for a photo
    /// (its file carries the rotation), R for a turned 3D model's view (its capture ignores
    /// R but the view doesn't). Both sides are turned back by it so the view lands them
    /// upright.
    pub display_turns: u8,
    pub queued_at: Instant,
}

/// Per-stage wall time, printed under `PB_PERF`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PreviewTimings {
    pub queue: Duration,
    pub decode: Duration,
    pub original: Duration,
    pub prepare: Duration,
    pub encode: Duration,
    pub read_back: Duration,
    pub total: Duration,
}

/// A finished preview.
pub struct PreviewResult {
    pub gen: u64,
    pub key: PreviewKey,
    /// The original side, when asked for (`None` if it couldn't be made).
    pub original: Option<DecodedImage>,
    pub export: Result<PreviewExport, String>,
    pub timings: PreviewTimings,
}

/// The export side of a preview.
pub struct PreviewExport {
    /// What the file shows, turned back to the source's own orientation (the view applies
    /// the rotation, as it does for the photo itself).
    pub image: DecodedImage,
    /// The encoded file, byte for byte.
    pub bytes: Arc<Vec<u8>>,
    pub notes: Vec<ExportNote>,
    /// The file's pixel size (as displayed: rotation baked in).
    pub dims: (u32, u32),
    /// The layout both sides are shown at: the source's own pixel size, turned back by
    /// [`PreviewRequest::display_turns`].
    pub source_dims: (u32, u32),
}

/// The renderer's aux slots: the original side and the export side.
pub const AUX_ORIGINAL: usize = 0;
pub const AUX_EXPORT: usize = 1;

/// What the screen should preview, compared every tick without allocating (built by
/// `AppCore::preview_tick_key`). Two keys with
/// the same source part show the same pixels, whatever the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickKey {
    /// The deck's content generation: any change that can make an index name different
    /// pixels (a delete, a new folder, a Save Rotation) bumps it — a streaming scan only
    /// appends, so it doesn't, and the preview survives it.
    pub content_gen: u64,
    pub item: usize,
    pub frame: FrameKey,
    /// Quarter turns baked into the exported file.
    pub rotation_cw: u8,
    /// [`PreviewRequest::display_turns`].
    pub display_turns: u8,
    pub settings: ExportSettings,
}

impl TickKey {
    /// Same pixels on screen, whatever the settings (and a photo's rotation, which the view
    /// applies to the unrotated textures).
    pub fn same_source(&self, other: &TickKey) -> bool {
        (self.content_gen, self.item, self.frame, self.display_turns)
            == (other.content_gen, other.item, other.frame, other.display_turns)
    }
}

/// The original side, on the GPU in aux 0.
pub struct OriginalShown {
    pub key: SourceKey,
    pub tick: TickKey,
    pub dims: (u32, u32),
}

/// The export side, on the GPU in aux 1.
pub struct ExportShown {
    pub key: PreviewKey,
    pub tick: TickKey,
    pub bytes: Arc<Vec<u8>>,
    pub notes: Vec<ExportNote>,
    /// The file's pixel size (as displayed).
    pub dims: (u32, u32),
    /// The source's own size — the layout the preview is shown at.
    pub source_dims: (u32, u32),
    /// The aux texture's size (the decoded export, turned back).
    pub texture_dims: (u32, u32),
}

/// Export mode's preview state, held in [`crate::export::ExportState`]. RAM-only.
#[derive(Default)]
pub struct PreviewState {
    pub worker: Option<PreviewWorker>,
    pub gen: u64,
    /// The key last seen, and when it was first seen (the debounce).
    pub seen: Option<TickKey>,
    pub seen_since: Option<Instant>,
    /// The settled key a request was last made for — each settled key asks once.
    pub asked: Option<TickKey>,
    /// The request in flight.
    pub in_flight: Option<(u64, TickKey)>,
    pub original: Option<OriginalShown>,
    pub export: Option<ExportShown>,
    pub spring: Spring,
    /// Why the last preview couldn't be made, for the key it failed on.
    pub failed: Option<(TickKey, String)>,
    /// The aux slot the last draw presented — a change seen by the tick means a draw.
    pub last_target: Option<(usize, u32, u32)>,
}

impl PreviewState {
    /// VRAM the aux textures hold (mipped RGBA8: × 4/3), charged to the ring's budget.
    pub fn vram_bytes(&self) -> u64 {
        let tex = |(w, h): (u32, u32)| w as u64 * h as u64 * 4 * 4 / 3;
        self.original.as_ref().map_or(0, |o| tex(o.dims))
            + self.export.as_ref().map_or(0, |e| tex(e.texture_dims))
    }

    /// Anything to tear down.
    pub fn is_idle(&self) -> bool {
        self.worker.is_none() && self.original.is_none() && self.export.is_none()
    }
}

/// What the panel says about the preview.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum PreviewStatus {
    /// Nothing to preview (no item, a door, a moving frame).
    #[default]
    Off,
    /// Being made (or the settings changed since the one on screen).
    Working,
    /// Ready: the exported file is exactly this many bytes.
    Ready { bytes: u64 },
    /// Couldn't be made, and why.
    Failed(String),
    /// Too big to preview (it still exports).
    TooLarge,
}

impl PreviewStatus {
    /// A cheap summary for the panel's change key: the state, and the size when ready.
    pub fn key(&self) -> (u8, u64) {
        match self {
            PreviewStatus::Off => (0, 0),
            PreviewStatus::Working => (1, 0),
            PreviewStatus::Ready { bytes } => (2, *bytes),
            PreviewStatus::Failed(_) => (3, 0),
            PreviewStatus::TooLarge => (4, 0),
        }
    }
}

/// Handle to the preview thread. Dropping it ends the thread and frees its caches.
pub struct PreviewWorker {
    jobs: Sender<PreviewRequest>,
    done: Receiver<PreviewResult>,
    latest: Arc<AtomicU64>,
}

impl PreviewWorker {
    pub fn new() -> PreviewWorker {
        let (jobs, job_rx) = std::sync::mpsc::channel::<PreviewRequest>();
        let (done_tx, done) = std::sync::mpsc::channel::<PreviewResult>();
        let latest = Arc::new(AtomicU64::new(0));
        let latest_w = Arc::clone(&latest);
        std::thread::Builder::new()
            .name("pb-export-preview".into())
            .spawn(move || worker_loop(&job_rx, &done_tx, &latest_w))
            .expect("spawn the export preview worker");
        PreviewWorker { jobs, done, latest }
    }

    /// Queue a preview; it supersedes every earlier one (latest wins). `false` if the worker
    /// is gone.
    pub fn submit(&self, req: PreviewRequest) -> bool {
        self.latest.store(req.gen, Ordering::Release);
        self.jobs.send(req).is_ok()
    }

    /// Drop whatever is queued or being made (it would land for settings already left).
    pub fn cancel(&self, gen: u64) {
        self.latest.store(gen, Ordering::Release);
    }

    /// Finished previews since the last drain; `None` once the thread is gone (it died), so
    /// the caller can start over instead of waiting forever.
    pub fn drain(&self) -> Option<Vec<PreviewResult>> {
        let mut out = Vec::new();
        loop {
            match self.done.try_recv() {
                Ok(r) => out.push(r),
                Err(std::sync::mpsc::TryRecvError::Empty) => return Some(out),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
            }
        }
    }
}

impl Default for PreviewWorker {
    fn default() -> Self {
        Self::new()
    }
}

/// The worker's RAM-only caches.
#[derive(Default)]
struct WorkerCache {
    /// The decoded source of the item being previewed — a setting change never re-decodes
    /// (vital for RAW and captured frames).
    source: Option<(SourceKey, ExportSource)>,
    /// Encoded files, newest last, within [`FILES_BUDGET`].
    files: VecDeque<CachedFile>,
    files_bytes: usize,
}

struct CachedFile {
    key: PreviewKey,
    bytes: Arc<Vec<u8>>,
    notes: Vec<ExportNote>,
    dims: (u32, u32),
}

impl WorkerCache {
    fn file(&self, key: &PreviewKey) -> Option<&CachedFile> {
        self.files.iter().find(|f| &f.key == key)
    }

    fn remember(&mut self, f: CachedFile) {
        self.files_bytes += f.bytes.len();
        self.files.push_back(f);
        while self.files_bytes > FILES_BUDGET && self.files.len() > 1 {
            if let Some(old) = self.files.pop_front() {
                self.files_bytes -= old.bytes.len();
            }
        }
    }
}

fn worker_loop(rx: &Receiver<PreviewRequest>, tx: &Sender<PreviewResult>, latest: &AtomicU64) {
    let mut cache = WorkerCache::default();
    while let Ok(mut req) = rx.recv() {
        // Latest wins: skip straight to the newest request.
        while let Ok(newer) = rx.try_recv() {
            req = newer;
        }
        let stale = || latest.load(Ordering::Acquire) != req.gen;
        if stale() {
            continue;
        }
        let Some(result) = run_preview(&req, &mut cache, &stale) else {
            continue; // superseded mid-way
        };
        if tx.send(result).is_err() {
            break;
        }
    }
}

/// Why a stage stopped: superseded (drop silently) or failed (tell the user).
enum Stop {
    Stale,
    Failed(String),
}

fn run_preview(
    req: &PreviewRequest,
    cache: &mut WorkerCache,
    stale: &dyn Fn() -> bool,
) -> Option<PreviewResult> {
    let start = Instant::now();
    let mut t = PreviewTimings {
        queue: req.queued_at.elapsed(),
        ..PreviewTimings::default()
    };
    let finish = |original, export, mut t: PreviewTimings| {
        t.total = start.elapsed();
        Some(PreviewResult {
            gen: req.gen,
            key: req.key.clone(),
            original,
            export,
            timings: t,
        })
    };

    // 1. The source, decoded once per item and frame.
    if cache.source.as_ref().map(|(k, _)| k) != Some(&req.key.source) {
        cache.source = None; // free the last one before decoding the next
        let s = Instant::now();
        match decode_source(req) {
            Ok(src) => cache.source = Some((req.key.source.clone(), src)),
            Err(why) => return finish(None, Err(why), t),
        }
        t.decode = s.elapsed();
    }
    if stale() {
        return None;
    }
    let (_, src) = cache.source.as_ref()?;
    let source_dims = if req.display_turns % 2 == 1 {
        (src.height, src.width)
    } else {
        (src.width, src.height)
    };

    // 2. The original side: every capability, no resize — what a lossless full-size export
    // would hold — in the source's own orientation.
    let original = if req.need_original {
        let s = Instant::now();
        let spec = PrepareSpec {
            caps: Caps::ALL,
            ..PrepareSpec::full(0, true)
        };
        let img = pb_encode::catch_panics(|| {
            pb_encode::prepare(src.clone(), &spec)
                .map_err(|e| pb_encode::EncodeError::Codec(e.to_string()))
        })
        .ok()
        .map(|i| turn_back(pb_decode::export::display_from_export_image(&i), req.display_turns));
        t.original = s.elapsed();
        img
    } else {
        None
    };
    if stale() {
        return None;
    }

    // 3. The export side.
    match export_side(req, cache, source_dims, stale, &mut t) {
        Ok(e) => finish(original, Ok(e), t),
        Err(Stop::Stale) => None,
        Err(Stop::Failed(why)) => finish(original, Err(why), t),
    }
}

fn export_side(
    req: &PreviewRequest,
    cache: &mut WorkerCache,
    source_dims: (u32, u32),
    stale: &dyn Fn() -> bool,
    t: &mut PreviewTimings,
) -> Result<PreviewExport, Stop> {
    let settings = req.key.settings;
    let encoder = settings.export_format().encoder();
    let label = settings.format.label();
    let (bytes, notes, dims, prepared) = match cache.file(&req.key) {
        Some(f) => (Arc::clone(&f.bytes), f.notes.clone(), f.dims, None),
        None => {
            let (_, src) = cache
                .source
                .as_ref()
                .ok_or_else(|| Stop::Failed("the image went away".into()))?;
            // Exactly the export worker's spec (`export_worker::run_stages`).
            let spec = PrepareSpec {
                caps: encoder.caps(),
                resize: settings.size.resize(),
                ..PrepareSpec::full(req.key.rotation_cw, settings.strip_metadata)
            };
            let s = Instant::now();
            let image = pb_encode::catch_panics(|| {
                pb_encode::prepare(src.clone(), &spec)
                    .map_err(|e| pb_encode::EncodeError::Codec(e.to_string()))
            })
            .map_err(|e| Stop::Failed(e.to_string()))?;
            t.prepare = s.elapsed();
            if stale() {
                return Err(Stop::Stale);
            }
            let s = Instant::now();
            let bytes = pb_encode::catch_panics(|| encoder.encode(&image))
                .map_err(|e| Stop::Failed(format!("couldn't encode the {label} ({e})")))?;
            t.encode = s.elapsed();
            let bytes = Arc::new(bytes);
            let dims = (image.width, image.height);
            cache.remember(CachedFile {
                key: req.key.clone(),
                bytes: Arc::clone(&bytes),
                notes: image.notes.clone(),
                dims,
            });
            (bytes, image.notes.clone(), dims, Some(image))
        }
    };
    if stale() {
        return Err(Stop::Stale);
    }

    // What the file shows. PNG is lossless, so its prepared pixels are exactly what the file
    // decodes to (and skip a decode); anything lossy is read back from the bytes.
    let s = Instant::now();
    let shown = match (settings.format, prepared) {
        (FormatChoice::Png, Some(img)) => pb_decode::export::display_from_export_image(&img),
        _ => {
            let d = pb_decode::decode_bytes(&bytes, None, false)
                .map_err(|e| Stop::Failed(format!("couldn't read the {label} back ({e})")))?;
            if d.format != PixelFormat::Rgba8 || !d.is_well_formed() {
                return Err(Stop::Failed(format!("couldn't read the {label} back")));
            }
            d
        }
    };
    let shown = turn_back(shown, (req.key.rotation_cw + req.display_turns) % 4);
    t.read_back = s.elapsed();
    Ok(PreviewExport {
        image: shown,
        bytes,
        notes,
        dims,
        source_dims,
    })
}

/// Decode the request's source at full resolution — the same paths an export takes.
fn decode_source(req: &PreviewRequest) -> Result<ExportSource, String> {
    match &req.frame {
        Some(frame) => {
            let cancel = AtomicBool::new(false);
            let img = crate::frame_capture::capture(req.source.as_ref(), req.item, frame, &cancel)
                .map_err(|e| format!("couldn't read that frame ({e})"))?;
            Ok(pb_decode::export_source_from_decoded(img))
        }
        None => {
            let bytes = req
                .source
                .bytes(req.item)
                .map_err(|e| format!("couldn't read the image ({e})"))?;
            pb_decode::decode_named_bytes_for_export(&req.key.source.name, &bytes).map_err(|e| {
                match e {
                    pb_decode::ExportDecodeError::Refused(why) => why.to_string(),
                    pb_decode::ExportDecodeError::Decode(e) => {
                        format!("couldn't decode the image ({e})")
                    }
                }
            })
        }
    }
}

/// Undo `rotation_cw` clockwise quarter turns, so the image is in the source's orientation.
fn turn_back(mut img: DecodedImage, rotation_cw: u8) -> DecodedImage {
    let back = (4 - rotation_cw % 4) % 4;
    if back == 0 {
        return img;
    }
    let pixels = std::mem::take(&mut img.pixels);
    let (s, w, h) = pb_encode::prepare::rotate(
        Samples::U8(pixels),
        img.width as usize,
        img.height as usize,
        4,
        back,
    );
    if let Samples::U8(p) = s {
        img.pixels = p;
    }
    img.width = w as u32;
    img.height = h as u32;
    img.orig_width = img.width;
    img.orig_height = img.height;
    img
}

/// A file size as the panel prints it: `412 KB`, `2.4 MB`. Decimal units on macOS (as the
/// Finder counts), binary elsewhere (as File Explorer does), so the number matches the one
/// the user will see next to the file.
pub fn size_label(bytes: u64) -> String {
    let unit: f64 = if cfg!(target_os = "macos") {
        1000.0
    } else {
        1024.0
    };
    let b = bytes as f64;
    if b < unit {
        format!("{bytes} bytes")
    } else if b < unit * unit {
        format!("{:.0} KB", b / unit)
    } else if b < unit * unit * unit {
        let mb = b / (unit * unit);
        if mb < 10.0 {
            format!("{mb:.1} MB")
        } else {
            format!("{mb:.0} MB")
        }
    } else {
        format!("{:.2} GB", b / (unit * unit * unit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_tap_switches_and_stays() {
        let mut s = Spring::default();
        let at = t0();
        let y = SpringOwner::Key(PbKey::KeyY);
        assert!(s.down(y, at));
        assert_eq!(s.shown(), Side::Original);
        assert!(!s.up(y, at + Duration::from_millis(80)), "a tap stays");
        assert_eq!(s.shown(), Side::Original);
        assert!(s.down(y, at + Duration::from_millis(500)));
        s.up(y, at + Duration::from_millis(560));
        assert_eq!(s.shown(), Side::Export, "a second tap comes back");
    }

    #[test]
    fn a_hold_peeks_and_flicks_back() {
        let mut s = Spring::default();
        let at = t0();
        let y = SpringOwner::Key(PbKey::KeyY);
        s.down(y, at);
        assert_eq!(s.shown(), Side::Original, "the press shows the other side at once");
        assert!(s.up(y, at + SPRING_HOLD));
        assert_eq!(s.shown(), Side::Export, "the release flicks back");
    }

    #[test]
    fn only_the_owner_ends_a_press() {
        let mut s = Spring::default();
        let at = t0();
        let y = SpringOwner::Key(PbKey::KeyY);
        s.down(y, at);
        assert!(!s.down(SpringOwner::Pointer, at), "a second press is ignored");
        assert!(!s.up(SpringOwner::Pointer, at + SPRING_HOLD), "so is its release");
        assert!(!s.set(Side::Export), "a toggle waits for the press to end");
        assert_eq!(s.shown(), Side::Original);
        assert!(s.up(y, at + SPRING_HOLD));
        assert_eq!(s.shown(), Side::Export);
    }

    #[test]
    fn a_change_resets_to_the_export() {
        let mut s = Spring::default();
        assert!(s.set(Side::Original));
        assert!(s.reset());
        assert_eq!(s.shown(), Side::Export);
        // Mid-press too: the press no longer owns anything.
        let y = SpringOwner::Key(PbKey::KeyY);
        s.down(y, t0());
        assert!(s.reset());
        assert!(!s.held());
        assert!(!s.up(y, t0() + SPRING_HOLD), "the stale release does nothing");
        assert_eq!(s.shown(), Side::Export);
    }

    #[test]
    fn sizes_read_like_the_file_browser() {
        assert_eq!(size_label(999), "999 bytes");
        if cfg!(target_os = "macos") {
            assert_eq!(size_label(412_000), "412 KB");
            assert_eq!(size_label(2_400_000), "2.4 MB");
            assert_eq!(size_label(24_000_000), "24 MB");
        } else {
            assert_eq!(size_label(412 * 1024), "412 KB");
            assert_eq!(size_label(2_516_582), "2.4 MB");
        }
    }

    #[test]
    fn turning_back_undoes_the_baked_rotation() {
        // A 2 × 1 [red, blue] baked at every rotation turns back to itself.
        let img = |px: Vec<u8>, w, h| DecodedImage {
            width: w,
            height: h,
            orig_width: w,
            orig_height: h,
            codec: "t",
            format: PixelFormat::Rgba8,
            pixels: px,
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
            recovered: None,
        };
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        let src: Vec<u8> = [red, blue].concat();
        for turns in 0..4u8 {
            let (rot, w, h) =
                pb_encode::prepare::rotate(Samples::U8(src.clone()), 2, 1, 4, turns);
            let Samples::U8(rot) = rot else { panic!() };
            let back = turn_back(img(rot, w as u32, h as u32), turns);
            assert_eq!((back.width, back.height), (2, 1), "turns {turns}");
            assert_eq!(back.pixels, src, "turns {turns}");
        }
    }
}
