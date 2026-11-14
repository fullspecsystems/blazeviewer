//! Image export (task #89) — the subsystem half (plan of record:
//! `.taskmaster/docs/export-plan.md`). The `AppCore` methods live in
//! `app_core_impl/export.rs` (the two-halves rule, `docs/where-code-goes.md`).
//!
//! Rung 1 is **Export as PNG**: the command freezes an [`ExportRequest`] (source, item,
//! rotation — so navigating, rotating or changing anything afterwards can't alter it), the
//! shell runs the native save dialog, and the [write worker](crate::export_worker) decodes,
//! prepares, encodes and writes off the event loop.
//!
//! Privacy (ADR-018): an export is an explicit user edit, never a passive write. The only
//! state that outlives the process is `Settings::last_export_folder` — one folder path, never
//! a file name, written only by a completed export and **never while "Open files in" is
//! pinned** (an ADR-022 addendum, owner-approved 2026-11-13). When pinned, the session's last
//! export folder lives here, in RAM.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

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
    /// While "Open files in" is pinned: this session's last export folder (RAM only).
    pub session_folder: Option<PathBuf>,
}

impl ExportState {
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

/// Where the save dialog starts (export-plan.md § 3.6), following "Open files in":
/// - **pinned**: this session's last export folder, else the pinned folder — nothing is
///   remembered across launches;
/// - **unpinned** (default): the remembered `last_export_folder`, else the image's own
///   folder (an archive entry: the archive's folder), else `fallback`.
pub fn start_dir(
    pinned: Option<&Path>,
    session_folder: Option<&Path>,
    last_export_folder: Option<&Path>,
    source_path: Option<&Path>,
    container: Option<&Path>,
    fallback: &Path,
) -> PathBuf {
    let usable = |p: Option<&Path>| {
        p.filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
    };
    if let Some(pinned) = usable(pinned) {
        return usable(session_folder).unwrap_or(pinned);
    }
    usable(last_export_folder)
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
        pinned: Option<&str>,
        session: Option<&str>,
        last: Option<&str>,
        src: Option<&str>,
        cont: Option<&str>,
    ) -> PathBuf {
        start_dir(
            pinned.map(Path::new),
            session.map(Path::new),
            last.map(Path::new),
            src.map(Path::new),
            cont.map(Path::new),
            Path::new(FALLBACK),
        )
    }

    #[test]
    fn unpinned_prefers_the_remembered_folder_then_the_image_folder() {
        assert_eq!(
            sd(None, None, Some("/out"), Some("/lib/a.jpg"), None),
            PathBuf::from("/out")
        );
        assert_eq!(
            sd(None, None, None, Some("/lib/a.jpg"), None),
            PathBuf::from("/lib")
        );
        // An archive entry has no path: the archive's folder.
        assert_eq!(
            sd(None, None, None, None, Some("/arch/set.zip")),
            PathBuf::from("/arch")
        );
        assert_eq!(sd(None, None, None, None, None), PathBuf::from(FALLBACK));
        // A session folder never applies when unpinned (unpinned remembers on disk instead).
        assert_eq!(
            sd(None, Some("/sess"), None, Some("/lib/a.jpg"), None),
            PathBuf::from("/lib")
        );
    }

    #[test]
    fn pinned_uses_the_session_folder_then_the_pin_and_ignores_the_remembered_one() {
        assert_eq!(
            sd(Some("/pin"), None, Some("/out"), Some("/lib/a.jpg"), None),
            PathBuf::from("/pin")
        );
        assert_eq!(
            sd(
                Some("/pin"),
                Some("/sess"),
                Some("/out"),
                Some("/lib/a.jpg"),
                None
            ),
            PathBuf::from("/sess")
        );
    }

    #[test]
    fn empty_paths_count_as_absent() {
        assert_eq!(
            sd(Some(""), None, Some(""), Some("/lib/a.jpg"), None),
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
            },
        );
        assert!(s.busy());
        assert!(s.pending_destinations().contains(Path::new("/o/a.png")));
        assert_ne!(s.next_id(), id, "ids are unique");
    }
}
