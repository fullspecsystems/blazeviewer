//! Image export (task #89) — the subsystem half (plan of record:
//! `.taskmaster/docs/export-plan.md`). The `AppCore` methods live in
//! `app_core_impl/export.rs` (the two-halves rule, `docs/where-code-goes.md`).
//!
//! Rung 1 is **Export as PNG**: the command freezes an [`ExportRequest`] (source, item,
//! rotation — so navigating, rotating or changing anything afterwards can't alter it), the
//! shell runs the native save dialog, and the [write worker](crate::export_worker) decodes,
//! prepares, encodes and writes off the event loop.
//!
//! Privacy (ADR-018): an export is an explicit user edit, never a passive write — and it
//! writes **nothing but the exported file**. Where you exported from each folder is
//! remembered in RAM for the session ([`ExportState::folder_memory`]) and forgotten at quit.
//! (Rung 1 first persisted one `last_export_folder` in settings; the owner reversed that on
//! 2026-11-14 — the dialog now starts beside the photo, see [`start_dir`].)

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pb_source::ItemSource;

/// An export, frozen at command time (review C11): later navigation, deck changes, rotation
/// or settings edits never reach a pending request. Holding the `Arc` keeps an archive entry
/// readable after the user leaves the archive.
#[derive(Clone)]
pub struct ExportRequest {
    pub id: u64,
    pub source: Arc<dyn ItemSource>,
    /// The item's index **within `source`** (stable for that source object).
    pub item: usize,
    /// The item's display name (a file name or an archive-relative entry path) — carries the
    /// extension the decode routes on.
    pub name: String,
    /// The item's file on disk (`None` for an archive entry) — the never-overwrite check.
    pub source_path: Option<PathBuf>,
    /// Clockwise quarter turns of the RAM rotation at command time.
    pub rotation_cw: u8,
    /// The frame to export, frozen at command time (task #81): `None` re-decodes the file
    /// itself (a photo — full fidelity: its own profile, 16-bit); `Some` captures a video
    /// frame, a video's poster or an animation frame.
    pub frame: Option<crate::frame_capture::FrameSource>,
}

impl std::fmt::Debug for ExportRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportRequest")
            .field("id", &self.id)
            .field("item", &self.item)
            .field("name", &self.name)
            .field("rotation_cw", &self.rotation_cw)
            .finish_non_exhaustive()
    }
}

/// An export the user confirmed a destination for, travelling through the write worker.
#[derive(Debug)]
pub struct InFlight {
    pub dest: PathBuf,
    /// The output file name, for the toast.
    pub file_name: String,
    /// The source's folder key ([`source_folder`]) — where this export is remembered.
    pub source_folder: Option<PathBuf>,
}

/// A suggestion being computed off the event loop (an existence check can block on a
/// sleeping network share — review F27).
pub struct Suggesting {
    pub request: ExportRequest,
    pub start_dir: PathBuf,
    pub rx: Receiver<String>,
}

/// All export state on `AppCore` — a single field (`AppCore::export`), so the winit shell's
/// struct literal changes once (CLAUDE.md's struct-literal trap).
#[derive(Default)]
pub struct ExportState {
    next_id: u64,
    /// Requests waiting for the save dialog, by request id.
    pub awaiting_dialog: HashMap<u64, ExportRequest>,
    /// Requests whose suggested name is still being computed.
    pub suggesting: Vec<Suggesting>,
    /// Exports handed to the worker, by request id.
    pub in_flight: HashMap<u64, InFlight>,
    /// The write worker, spawned on the first export.
    pub worker: Option<crate::export_worker::ExportWorker>,
    /// Where you last exported **from each source folder** this session — keyed by the
    /// image's folder (or the archive itself), valued by the folder the export went to. RAM
    /// only, forgotten at quit.
    pub folder_memory: HashMap<PathBuf, PathBuf>,
    /// The most recent successful export this session — File ▸ Show Last Export. RAM only,
    /// forgotten at quit.
    pub last_export: Option<PathBuf>,
    /// The "Exported name.png" notice on screen, if any (macOS draws it as a clickable pill).
    pub notice: Option<ExportNotice>,
    notice_seq: u64,
}

/// How long the export notice stays while the pointer isn't on it.
pub const NOTICE_HOLD: Duration = Duration::from_secs(5);

/// The "Exported name.png" notice — on the native shell a pill you click to show the file
/// in Finder. It lives [`NOTICE_HOLD`] (held while hovered), and navigating or any other
/// command dismisses it: it must never sit over the photo you've moved on to.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportNotice {
    /// Bumped per notice so the shell re-animates even for a repeated name.
    pub seq: u64,
    pub message: String,
    pub path: PathBuf,
    /// The item on screen when it appeared.
    pub item: Option<usize>,
    pub shown_at: Instant,
    pub hovered: bool,
}

impl ExportState {
    /// Show the notice for a finished export (replacing any earlier one).
    pub fn show_notice(
        &mut self,
        message: String,
        path: PathBuf,
        item: Option<usize>,
        now: Instant,
    ) {
        self.notice_seq = self.notice_seq.wrapping_add(1);
        self.notice = Some(ExportNotice {
            seq: self.notice_seq,
            message,
            path,
            item,
            shown_at: now,
            hovered: false,
        });
    }

    /// Expire the notice: past its hold (unless hovered), or the photo changed. Returns
    /// whether it changed.
    pub fn tick_notice(&mut self, now: Instant, displayed: Option<usize>) -> bool {
        let expired = self.notice.as_ref().is_some_and(|n| {
            n.item != displayed
                || (!n.hovered && now.saturating_duration_since(n.shown_at) >= NOTICE_HOLD)
        });
        if expired {
            self.notice = None;
        }
        expired
    }

    /// When the notice next needs a tick to expire (`None` while hovered or absent).
    pub fn notice_deadline(&self) -> Option<Instant> {
        self.notice
            .as_ref()
            .filter(|n| !n.hovered)
            .map(|n| n.shown_at + NOTICE_HOLD)
    }

    /// The pointer entered or left the notice. Leaving restarts the hold, so it doesn't
    /// vanish the instant you move off it.
    pub fn hover_notice(&mut self, hovering: bool, now: Instant) {
        if let Some(n) = self.notice.as_mut() {
            if n.hovered && !hovering {
                n.shown_at = now;
            }
            n.hovered = hovering;
        }
    }

    /// A fresh request id.
    pub fn next_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    /// Whether any export work is outstanding — drives the core's low-rate wake (never
    /// `work_pending`, which would keep the frame pump hot for seconds — review F8).
    pub fn busy(&self) -> bool {
        !self.suggesting.is_empty() || !self.in_flight.is_empty()
    }

    /// Destinations of exports not yet committed — never offered or accepted again until
    /// they land (two queued exports must not race to one new name — review F7).
    pub fn pending_destinations(&self) -> HashSet<PathBuf> {
        self.in_flight.values().map(|f| f.dest.clone()).collect()
    }
}

/// The key the per-folder memory uses: the image's own folder, or — for an archive entry,
/// which has no path — the archive itself.
pub fn source_folder(source_path: Option<&Path>, container: Option<&Path>) -> Option<PathBuf> {
    source_path
        .and_then(Path::parent)
        .or(container)
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// Where the save dialog starts (export-plan.md § 3.6, revised by the owner 2026-11-14 —
/// "opening on the last folder I was in doesn't feel like what I'd want most of the time"):
/// 1. where you last exported **from this same folder** this session (`remembered`, RAM) —
///    so a run of exports from one folder keeps landing together;
/// 2. else the pinned "Open files in" folder;
/// 3. else **beside the photo** — its own folder (an archive entry: the archive's folder),
///    as Preview's Export does;
/// 4. else `fallback`.
pub fn start_dir(
    remembered: Option<&Path>,
    pinned: Option<&Path>,
    source_path: Option<&Path>,
    container: Option<&Path>,
    fallback: &Path,
) -> PathBuf {
    let usable = |p: Option<&Path>| {
        p.filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };
    usable(remembered)
        .or_else(|| usable(pinned))
        .or_else(|| usable(source_path.and_then(Path::parent)))
        .or_else(|| usable(container.and_then(Path::parent)))
        .unwrap_or_else(|| fallback.to_path_buf())
}

/// The suggested output name: the source's stem + `.png`, suffixed (`-1`, `-2`, …) past any
/// name `is_taken` reports (an existing file or another export's pending destination).
pub fn suggested_name(display_name: &str, is_taken: impl Fn(&str) -> bool) -> String {
    let file = crate::engine::file_name_of(display_name);
    let stem = Path::new(&file)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("export");
    crate::quick_sort::unique_name(&format!("{stem}.png"), is_taken)
}

/// Make sure the chosen path ends in `.png` (Linux's dialog doesn't enforce the filter).
pub fn ensure_png_extension(path: PathBuf) -> PathBuf {
    let is_png = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("png"));
    if is_png {
        path
    } else {
        let mut s = path.into_os_string();
        s.push(".png");
        PathBuf::from(s)
    }
}

/// Clockwise quarter turns for a RAM rotation.
pub fn quarter_turns(rot: pb_render::Rotation) -> u8 {
    match rot {
        pb_render::Rotation::R0 => 0,
        pb_render::Rotation::R90 => 1,
        pb_render::Rotation::R180 => 2,
        pb_render::Rotation::R270 => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FALLBACK: &str = "/home/u/Pictures";

    fn sd(
        remembered: Option<&str>,
        pinned: Option<&str>,
        src: Option<&str>,
        cont: Option<&str>,
    ) -> PathBuf {
        start_dir(
            remembered.map(Path::new),
            pinned.map(Path::new),
            src.map(Path::new),
            cont.map(Path::new),
            Path::new(FALLBACK),
        )
    }

    #[test]
    fn the_dialog_starts_beside_the_photo() {
        assert_eq!(
            sd(None, None, Some("/lib/a.jpg"), None),
            PathBuf::from("/lib")
        );
        // An archive entry has no path: the archive's folder.
        assert_eq!(
            sd(None, None, None, Some("/arch/set.zip")),
            PathBuf::from("/arch")
        );
        assert_eq!(sd(None, None, None, None), PathBuf::from(FALLBACK));
    }

    #[test]
    fn this_folders_last_export_wins_then_the_pin() {
        // Exported from this folder before this session → back there.
        assert_eq!(
            sd(Some("/out"), Some("/pin"), Some("/lib/a.jpg"), None),
            PathBuf::from("/out")
        );
        // Not yet: a pinned "Open files in" folder, else beside the photo.
        assert_eq!(
            sd(None, Some("/pin"), Some("/lib/a.jpg"), None),
            PathBuf::from("/pin")
        );
    }

    #[test]
    fn the_memory_key_is_the_photos_folder_or_the_archive() {
        assert_eq!(
            source_folder(Some(Path::new("/lib/a.jpg")), Some(Path::new("/x.zip"))),
            Some(PathBuf::from("/lib"))
        );
        assert_eq!(
            source_folder(None, Some(Path::new("/arch/set.zip"))),
            Some(PathBuf::from("/arch/set.zip"))
        );
        assert_eq!(source_folder(None, None), None);
    }

    /// The notice: held 5 s, held longer while hovered (leaving restarts the hold), and
    /// gone the moment the photo changes.
    #[test]
    fn the_export_notice_lifetime() {
        let t0 = Instant::now();
        let mut st = ExportState::default();
        st.show_notice(
            "Exported a.png".into(),
            PathBuf::from("/o/a.png"),
            Some(3),
            t0,
        );
        let seq = st.notice.as_ref().unwrap().seq;
        assert!(
            !st.tick_notice(t0 + Duration::from_secs(4), Some(3)),
            "still up at 4 s"
        );
        assert_eq!(st.notice_deadline(), Some(t0 + NOTICE_HOLD));
        st.hover_notice(true, t0 + Duration::from_secs(4));
        assert_eq!(st.notice_deadline(), None, "no expiry while hovered");
        assert!(
            !st.tick_notice(t0 + Duration::from_secs(30), Some(3)),
            "held by hover"
        );
        st.hover_notice(false, t0 + Duration::from_secs(30));
        assert!(
            !st.tick_notice(t0 + Duration::from_secs(33), Some(3)),
            "a fresh hold"
        );
        assert!(
            st.tick_notice(t0 + Duration::from_secs(35), Some(3)),
            "then gone"
        );
        // Navigating away dismisses at once; a new notice gets a new seq.
        st.show_notice(
            "Exported b.png".into(),
            PathBuf::from("/o/b.png"),
            Some(3),
            t0,
        );
        assert!(st.notice.as_ref().unwrap().seq > seq);
        assert!(st.tick_notice(t0, Some(4)));
        assert!(st.notice.is_none());
    }

    #[test]
    fn empty_paths_count_as_absent() {
        assert_eq!(
            sd(Some(""), Some(""), Some("/lib/a.jpg"), None),
            PathBuf::from("/lib")
        );
    }

    #[test]
    fn suggested_names_use_the_stem_and_avoid_taken_names() {
        assert_eq!(suggested_name("IMG_1234.HEIC", |_| false), "IMG_1234.png");
        assert_eq!(
            suggested_name("trip/day1/beach.jpg", |_| false),
            "beach.png"
        );
        let taken = ["beach.png", "beach-1.png"];
        assert_eq!(
            suggested_name("beach.jpg", |n| taken.contains(&n)),
            "beach-2.png"
        );
        assert_eq!(suggested_name("", |_| false), "export.png");
        assert_eq!(suggested_name("noext", |_| false), "noext.png");
    }

    #[test]
    fn png_extension_is_enforced_without_doubling() {
        assert_eq!(
            ensure_png_extension(PathBuf::from("/o/a.png")),
            PathBuf::from("/o/a.png")
        );
        assert_eq!(
            ensure_png_extension(PathBuf::from("/o/a.PNG")),
            PathBuf::from("/o/a.PNG")
        );
        assert_eq!(
            ensure_png_extension(PathBuf::from("/o/a")),
            PathBuf::from("/o/a.png")
        );
        assert_eq!(
            ensure_png_extension(PathBuf::from("/o/a.jpg")),
            PathBuf::from("/o/a.jpg.png")
        );
    }

    #[test]
    fn busy_tracks_suggestions_and_in_flight_exports() {
        let mut s = ExportState::default();
        assert!(!s.busy());
        let id = s.next_id();
        s.in_flight.insert(
            id,
            InFlight {
                dest: PathBuf::from("/o/a.png"),
                file_name: "a.png".into(),
                source_folder: None,
            },
        );
        assert!(s.busy());
        assert!(s.pending_destinations().contains(Path::new("/o/a.png")));
        assert_ne!(s.next_id(), id, "ids are unique");
    }
}
