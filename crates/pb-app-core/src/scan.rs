//! Playlist resolution + the streaming-scan message type (NS0 5.6 Step 2). The compute that
//! turns a folder walk / archive into a [`Resolved`] playlist snapshot the core installs via
//! `rebuild_playlist` / `extend_playlist`. Shell-neutral (`pb-core::open` + `pb-source` + std),
//! so the macOS host reuses it; the winit shell keeps only the thread + progress-dialog plumbing.
//!
//! Step 2a lands the `Resolved` currency, its pure builders, and the `ScanUpdate` stream message.
//! The walkdir-driven resolvers + `ScanProgress` (Step 2b) and the archive resolvers (Step 2c)
//! follow — they relocate here too, keeping only the `std::thread::spawn` + `mpsc` in the shell.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pb_core::open::{self, Source};
use pb_decode::is_supported_extension;
use pb_source::{FsSource, PhotoSource};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A resolved playlist: the concrete [`PhotoSource`] plus the framing the app needs (display
/// root, the scan root for `Ctrl+R`, recursive flag, start index).
pub struct Resolved {
    pub source: Arc<dyn PhotoSource>,
    pub root: PathBuf,
    pub scan_root: Option<PathBuf>,
    pub recursive: bool,
    pub start: usize,
}

impl Resolved {
    /// The "nothing to show" fallback — an empty filesystem source. Callers treat
    /// `source.is_empty()` uniformly (empty folder, or an archive that failed / needs a
    /// password), so an open failure never blanks a currently-shown photo.
    pub fn empty() -> Self {
        Resolved {
            source: Arc::new(FsSource::new(Vec::new())),
            root: PathBuf::from("."),
            scan_root: None,
            recursive: false,
            start: 0,
        }
    }
}

/// Build a playlist snapshot from the paths gathered so far. Runs on the **scan worker thread**
/// so constructing the `FsSource` (which rebuilds the display-name list, O(N)) never touches the
/// event loop — the UI just swaps the resulting `Arc`. `start` is resolved against this snapshot;
/// it's only used by the bootstrap (first) batch (later batches keep the app's own cursor via
/// `extend_playlist`).
pub fn build_resolved(
    paths: Vec<PathBuf>,
    cursor: &open::Cursor,
    root: PathBuf,
    scan_root: Option<PathBuf>,
    recursive: bool,
) -> Resolved {
    let start = open::resolve_cursor(&paths, cursor);
    Resolved {
        source: Arc::new(FsSource::new(paths)),
        root,
        scan_root,
        recursive,
        start,
    }
}

/// A [`Resolved`] for an archive `source`: the archive path is the display root, and entry names
/// are already archive-relative (so the info panel uses them).
pub fn archive_resolved(path: &Path, source: Arc<dyn PhotoSource>) -> Resolved {
    Resolved {
        root: path.to_path_buf(),
        source,
        scan_root: None,
        recursive: false,
        start: 0,
    }
}

/// A message from the streaming scan worker (`stream_scan`). The walk runs off the event loop and
/// **streams** the playlist in: each `Batch` carries a growing [`Resolved`] snapshot (the
/// cumulative `FsSource` so far, built off-thread so the UI swap is O(1)); `Done` ends the walk.
/// The app bootstraps the playlist on the first non-empty batch (showing a photo almost
/// immediately) and extends it in place on the rest — so browsing starts before the whole tree is
/// scanned.
pub enum ScanUpdate {
    Batch(Resolved),
    Done,
}

// ── Directory-scan resolvers + progress (NS0 5.6 Step 2b, migrated off the shell) ──

/// How often the streaming scan worker publishes a growing playlist snapshot. Time-bounded
/// (not per-count) so the number of snapshots — and thus the per-snapshot O(N) `FsSource`
/// rebuild — stays small (≈ scan_duration / this) regardless of folder size.
const SCAN_BATCH_INTERVAL: Duration = Duration::from_millis(150);

/// Whether a path's extension is a supported image format (the decoder's single
/// source of truth — see `pb_decode::is_supported_extension`).
pub fn is_supported_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(is_supported_extension)
        .unwrap_or(false)
}

/// A scanned directory's path relative to the scan root, as a display string for the
/// Scanning dialog's "current folder" caption. The root itself (empty relative path)
/// shows as its own folder name so the caption is never blank.
pub fn rel_display(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string()),
    }
}

/// The configured directory walker shared by the streaming scan and its tests: depth-first,
/// each directory's entries **sorted by file name** — which reproduces `Vec<PathBuf>::sort()`
/// order exactly (`Path`'s `Ord` is component-wise, not byte-string — verified), so streaming
/// changes nothing about the order today's walk-then-`paths.sort()` produces. Symlinks are
/// yielded but never followed, so the walk can't cycle. `recursive` sets the depth.
pub fn image_walker(root: &Path, recursive: bool) -> walkdir::WalkDir {
    walkdir::WalkDir::new(root)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .sort_by_file_name()
        .follow_links(false)
}

/// Walk `dir` for supported images, appending them to `out` (unsorted — the caller
/// sorts once across all roots). `recursive` descends every subfolder; otherwise only
/// the immediate children are listed.
///
/// This is deliberately **crash-proof on hostile trees**, the bug that made opening a
/// large nested folder (e.g. macOS's `~/Library`) beachball then die:
/// * **iterative** (walkdir, not recursion) — a tree thousands of levels deep can't
///   overflow the stack the old recursive walk did, and open directory handles stay
///   bounded instead of one-per-level;
/// * **never follows symlinks** (walkdir's default) — a directory symlink/alias that
///   points back at an ancestor can't send it into an infinite loop;
/// * **error-tolerant** — a permission-denied folder (macOS TCC guards much of
///   `~/Library`) or a file that vanished mid-walk is skipped, not fatal.
///
/// `cancel`, if set, stops the walk at the next entry so a superseding open can abandon
/// a huge in-flight scan; whatever was gathered so far is left in `out`.
pub fn collect_images(
    dir: &Path,
    recursive: bool,
    progress: Option<&ScanProgress>,
    out: &mut Vec<PathBuf>,
) {
    let max_depth = if recursive { usize::MAX } else { 1 };
    // follow_links(false) is the default, but state it: symlinked dirs are yielded yet
    // never descended, so the walk stays inside the intended tree and can't cycle.
    let walker = walkdir::WalkDir::new(dir)
        .max_depth(max_depth)
        .follow_links(false);
    for entry in walker {
        if progress.is_some_and(|p| p.is_cancelled()) {
            return;
        }
        // Skip unreadable entries (permissions, races) rather than aborting the scan.
        let Ok(entry) = entry else {
            continue;
        };
        // file_type() here does not traverse symlinks (matches follow_links(false)), so
        // a symlinked file/dir is not mistaken for a real one.
        let ft = entry.file_type();
        if ft.is_dir() {
            // Publish the directory now being walked so the Scanning dialog shows real
            // motion. Cheap: once per directory (a mutex write), not per file.
            if let Some(p) = progress {
                p.set_current(rel_display(entry.path(), dir));
            }
        } else if ft.is_file() && is_supported_image(entry.path()) {
            if let Some(p) = progress {
                p.incr_found();
            }
            out.push(entry.into_path());
        }
    }
}

/// Execute an [`open::OpenPlan`]'s [`Source`]: scan the roots (or filter the
/// explicit list) into the ordered image paths to play. Returns the paths, the
/// root for relative-path display, the scan root (for `Ctrl+R`; `None` for an
/// explicit list), and whether the scan was recursive.
pub fn resolve_source(
    source: &Source,
    progress: Option<&ScanProgress>,
) -> (Vec<PathBuf>, PathBuf, Option<PathBuf>, bool) {
    match source {
        Source::Scan { roots, recursive } => {
            let mut paths = Vec::new();
            for r in roots {
                collect_images(r, *recursive, progress, &mut paths);
            }
            paths.sort();
            let root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
            (paths, root, roots.first().cloned(), *recursive)
        }
        Source::Explicit(files) => {
            let paths: Vec<PathBuf> = files
                .iter()
                .filter(|p| is_supported_image(p.as_path()))
                .cloned()
                .collect();
            let root = files
                .first()
                .and_then(|p| p.parent())
                .filter(|d| !d.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            (paths, root, None, false)
        }
        // Archives don't resolve to a path list; `resolve_playlist` routes them to
        // `open_archive` instead. This arm only keeps the match exhaustive.
        Source::Archive(_) => (Vec::new(), PathBuf::from("."), None, false),
    }
}

/// Resolve a filesystem [`Source`] (folder scan or explicit list) into a playlist,
/// driving `progress` (image count + current folder) and honoring its cancel flag so a
/// superseding open / the Scanning dialog can abandon a huge in-flight scan. The cursor
/// math + `FsSource` build is shared with [`resolve_playlist`]; this carries the
/// (cancellable) directory I/O, so it's what the off-thread scan worker runs.
pub fn resolve_scan(
    source: &Source,
    cursor: &open::Cursor,
    progress: Option<&ScanProgress>,
) -> Resolved {
    let (paths, root, scan_root, recursive) = resolve_source(source, progress);
    let start = open::resolve_cursor(&paths, cursor);
    Resolved {
        source: Arc::new(FsSource::new(paths)),
        root,
        scan_root,
        recursive,
        start,
    }
}

/// Shared, thread-safe progress + cancellation for an off-thread directory scan — the
/// folder-walk analogue of [`pb_source::OpenProgress`]. A folder walk has no knowable
/// total (you'd have to walk the tree twice), so this carries *indeterminate* progress:
/// a running count of images found and the directory currently being walked, plus the
/// cancel flag the Scanning dialog's Cancel / Esc (and a superseding open / teardown)
/// set. Cheap to [`clone`](Clone) — it's an `Arc` — so the walk worker and the UI thread
/// each hold one.
#[derive(Clone, Default)]
pub struct ScanProgress {
    inner: Arc<ScanProgressInner>,
}

#[derive(Default)]
struct ScanProgressInner {
    /// Supported images found so far (bumped per match by the walk worker).
    found: AtomicUsize,
    /// Set by the UI to stop the walk at its next entry (Cancel / Esc / a superseding open).
    cancel: AtomicBool,
    /// The directory currently being walked, relative to the scan root (display string).
    current: std::sync::Mutex<String>,
}

impl ScanProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Supported images found so far (read by the Scanning dialog each frame).
    pub fn found(&self) -> usize {
        self.inner.found.load(Ordering::Relaxed)
    }

    /// Worker-side: record one more supported image.
    fn incr_found(&self) {
        self.inner.found.fetch_add(1, Ordering::Relaxed);
    }

    /// Ask the walk to stop at its next entry (the Cancel button / Esc / a superseding open).
    pub fn request_cancel(&self) {
        self.inner.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested (polled by the walk loop).
    fn is_cancelled(&self) -> bool {
        self.inner.cancel.load(Ordering::Relaxed)
    }

    /// Worker-side: publish the directory now being walked (relative to the scan root).
    /// A poisoned lock just means a prior writer panicked mid-update — drop the value
    /// rather than propagate; a stale caption is harmless.
    fn set_current(&self, dir: String) {
        if let Ok(mut g) = self.inner.current.lock() {
            *g = dir;
        }
    }

    /// The directory currently being walked (empty until the worker sets one).
    pub fn current(&self) -> String {
        self.inner
            .current
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

/// Walk `roots` off the event loop, **streaming** the playlist in: emit a growing snapshot
/// every [`SCAN_BATCH_INTERVAL`] (and a final one), then [`ScanUpdate::Done`]. The first
/// non-empty batch lets the app show a photo almost immediately; later batches extend the
/// playlist in place, so the user browses while the rest of a big tree is still being walked.
/// Drives `progress` (image count + current folder) and bails at the next entry once its
/// cancel flag is set. Each snapshot is built here (off-thread) so the UI swap is O(1).
/// Sending stops early if the receiver is gone (a superseding open dropped it).
#[allow(clippy::too_many_arguments)]
pub fn stream_scan(
    roots: Vec<PathBuf>,
    recursive: bool,
    cursor: open::Cursor,
    root: PathBuf,
    scan_root: Option<PathBuf>,
    generation: u64,
    progress: ScanProgress,
    tx: std::sync::mpsc::Sender<(u64, ScanUpdate)>,
) {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut last_emit = Instant::now();
    let mut sent_len = 0usize;
    // For a single-file open (`Cursor::At`) we must not bootstrap until the opened file is
    // in the snapshot — otherwise `resolve_cursor` falls back to index 0 and we'd show the
    // wrong photo. So hold interval emits until the target is found (it always will be: it's
    // in the flat parent dir being scanned). `Cursor::First` never gates. The *final* emit
    // below is unconditional, so a target that's since been deleted still shows the folder.
    let target = match &cursor {
        open::Cursor::At(p) => Some(p.clone()),
        open::Cursor::First => None,
    };
    let mut gated = target.is_some();
    'outer: for r in &roots {
        for entry in image_walker(r, recursive) {
            if progress.is_cancelled() {
                break 'outer;
            }
            let Ok(entry) = entry else {
                continue; // skip unreadable entries (permissions, races) — don't abort
            };
            let ft = entry.file_type();
            if ft.is_dir() {
                // Publish the directory now being walked (relative to its root) for the chip.
                progress.set_current(rel_display(entry.path(), r));
            } else if ft.is_file() && is_supported_image(entry.path()) {
                let p = entry.into_path();
                progress.incr_found();
                if gated && target.as_ref() == Some(&p) {
                    gated = false; // the opened file is now in the snapshot — emits may start
                }
                paths.push(p);
                if !gated && last_emit.elapsed() >= SCAN_BATCH_INTERVAL {
                    let snap = build_resolved(
                        paths.clone(),
                        &cursor,
                        root.clone(),
                        scan_root.clone(),
                        recursive,
                    );
                    if tx.send((generation, ScanUpdate::Batch(snap))).is_err() {
                        return; // receiver dropped — superseded; stop and free our buffers
                    }
                    sent_len = paths.len();
                    last_emit = Instant::now();
                }
            }
        }
    }
    // Final batch: the un-emitted remainder, or the only batch for a fast folder.
    if !paths.is_empty() && (paths.len() > sent_len || sent_len == 0) {
        let snap = build_resolved(paths, &cursor, root, scan_root, recursive);
        let _ = tx.send((generation, ScanUpdate::Batch(snap)));
    }
    let _ = tx.send((generation, ScanUpdate::Done));
}
