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
use pb_source::{seven_z_projected_bytes, FsSource, PhotoSource, SevenZSource, ZipSource};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A resolved playlist: the concrete [`PhotoSource`] plus the framing the app needs (display
/// root, the scan root for `Ctrl+R`, recursive flag, start index). `Clone` is a cheap `Arc`
/// bump (the source is shared); `Debug` is manual since `dyn PhotoSource` isn't `Debug` — both
/// let it ride on `CoreEvent::ScanBatch` (NS0 5.6 Step 3).
#[derive(Clone)]
pub struct Resolved {
    pub source: Arc<dyn PhotoSource>,
    pub root: PathBuf,
    pub scan_root: Option<PathBuf>,
    pub recursive: bool,
    pub start: usize,
}

impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("len", &self.source.len())
            .field("root", &self.root)
            .field("scan_root", &self.scan_root)
            .field("recursive", &self.recursive)
            .field("start", &self.start)
            .finish()
    }
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

/// The **archive** entry predicate: a supported image extension, or a recognized
/// video container. Archived videos are indexed like loose ones (they play from
/// RAM through the `VideoInput::Bytes` seam — no longer path-only); recognition ≠
/// playability, same as the filesystem scanner's rule.
pub fn is_supported_archive_entry(ext: &str) -> bool {
    is_supported_extension(ext) || crate::video::VideoContainer::from_extension(ext).is_some()
}

/// Whether a path is a library item the **filesystem scanner** lists: a supported image
/// or a recognized video container (task #79 phase 1 — the predicate split's scanner
/// half; see [`crate::video::classify_library_file`]).
pub fn is_supported_library_file(p: &Path) -> bool {
    crate::video::classify_library_file(p).is_some()
}

/// Hide Live-Photo companions from a gathered path list: a `.mov`/`.qt` whose
/// (directory, stem) matches an image in the list is the motion half of a Live Photo
/// (`IMG_1234.HEIC` + `IMG_1234.MOV`) — it plays via the still's `P`, so listing it
/// separately would show every Live Photo twice. Order-preserving; matching is
/// per-directory (recursive scans never pair across folders). `exempt` is never hidden —
/// the file the user explicitly opened must appear even if it is a companion.
///
/// Documented false positive (plan rev2, kept for v1): an unrelated `IMG_1234.mov`
/// beside an `IMG_1234.jpg` is hidden too. Content-identifier validation is the future
/// refinement.
pub fn dedup_companions(mut paths: Vec<PathBuf>, exempt: Option<&Path>) -> Vec<PathBuf> {
    use crate::video::{companion_key, is_companion_candidate};
    use std::collections::HashSet;
    let image_keys: HashSet<(PathBuf, String)> = paths
        .iter()
        .filter(|p| {
            matches!(
                crate::video::classify_library_file(p),
                Some(crate::video::LibraryItemKind::Image)
            )
        })
        .filter_map(|p| companion_key(p))
        .collect();
    paths.retain(|p| {
        if !is_companion_candidate(p) || exempt == Some(p.as_path()) {
            return true;
        }
        companion_key(p).is_none_or(|k| !image_keys.contains(&k))
    });
    paths
}

/// Streaming companion dedup for the incremental scan: batches must **never publish a
/// companion and remove it later**, so a file is withheld until its same-stem run is
/// complete. Correct because [`image_walker`] yields a directory's files sorted
/// case-insensitively, which makes same-(dir, stem) files contiguous (`img_1.` sorts as
/// a block: `.` orders below every alphanumeric) — so a run ends the moment a file with
/// a different key arrives, and resolved entries publish in their original positions.
pub struct CompanionFilter {
    exempt: Option<PathBuf>,
    run_key: Option<(PathBuf, String)>,
    run: Vec<PathBuf>,
}

impl CompanionFilter {
    pub fn new(exempt: Option<PathBuf>) -> Self {
        CompanionFilter {
            exempt,
            run_key: None,
            run: Vec::new(),
        }
    }

    /// Feed the next file in walk order; returns the entries now fully resolved
    /// (possibly empty while a same-stem run is still open).
    pub fn push(&mut self, path: PathBuf) -> Vec<PathBuf> {
        let key = crate::video::companion_key(&path);
        let mut out = Vec::new();
        // A keyless path (no parent / non-UTF-8 stem) can't pair; give it its own run.
        if key.is_none() || key != self.run_key {
            out = self.flush();
            self.run_key = key;
        }
        self.run.push(path);
        out
    }

    /// Resolve and drain anything still buffered (end of walk, or cancellation).
    pub fn finish(&mut self) -> Vec<PathBuf> {
        self.run_key = None;
        self.flush()
    }

    fn flush(&mut self) -> Vec<PathBuf> {
        if self.run.is_empty() {
            return Vec::new();
        }
        dedup_companions(std::mem::take(&mut self.run), self.exempt.as_deref())
    }
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

/// Case-insensitive name comparison (lowercased, with a raw tiebreak for a total order),
/// so folders/files order like Finder — and like the folder tree, which sorts the same way.
/// Raw byte order (the old default) put every uppercase letter before every lowercase one,
/// so `Screenshots` sorted before `onlinethumbnailcache` and the deck's first photo landed
/// in a folder the tree listed much lower — disorienting, and it desynced ⌘←/→ from the tree.
pub(crate) fn ci_name_cmp(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> std::cmp::Ordering {
    let (al, bl) = (a.to_string_lossy(), b.to_string_lossy());
    al.to_lowercase()
        .cmp(&bl.to_lowercase())
        .then_with(|| al.cmp(&bl))
}

/// The component-wise path order for the deck: **a folder's own photos come before anything
/// in its subfolders** ("files before folders"), then case-insensitively by name. So the
/// deck reads like the tree — you finish a folder's photos, then descend into its subfolders,
/// instead of interleaving a folder's files with its subfolders' files by raw name (which
/// made ⌘←/→ bounce in and out of subfolders and the current-folder highlight thrash).
///
/// At the first differing component, whichever path **terminates** there (a file directly in
/// the shared parent) sorts before one that **descends** into a subfolder. This is the
/// global-sort analog of [`image_walker`]'s per-directory "files before dirs" order, so the
/// streaming walk reproduces exactly what a single `sort_by(ci_path_cmp)` would produce.
pub fn ci_path_cmp(a: &Path, b: &Path) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut ac, mut bc) = (a.components().peekable(), b.components().peekable());
    loop {
        match (ac.next(), bc.next()) {
            (Some(x), Some(y)) => {
                let c = ci_name_cmp(x.as_os_str(), y.as_os_str());
                if c != Ordering::Equal {
                    // A file here (no further components) sorts before a subfolder descent.
                    return match (ac.peek().is_none(), bc.peek().is_none()) {
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        _ => c,
                    };
                }
            }
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => return Ordering::Equal,
        }
    }
}

/// The configured directory walker shared by the streaming scan and its tests: depth-first,
/// each directory's entries **files before subdirectories, then case-insensitively by name**
/// — so it yields a folder's photos, *then* recurses into its subfolders, reproducing
/// `sort_by(ci_path_cmp)` order exactly. Symlinks are yielded but never followed, so the walk
/// can't cycle. `recursive` sets the depth.
pub fn image_walker(root: &Path, recursive: bool) -> walkdir::WalkDir {
    walkdir::WalkDir::new(root)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .sort_by(
            |a, b| match (a.file_type().is_dir(), b.file_type().is_dir()) {
                (false, true) => std::cmp::Ordering::Less,
                (true, false) => std::cmp::Ordering::Greater,
                _ => ci_name_cmp(a.file_name(), b.file_name()),
            },
        )
        .follow_links(false)
}

/// What [`dir_has_image`] concluded — `Aborted` is distinct from `Empty` so a
/// cancelled or over-budget probe never mislabels a folder as photo-less.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Probe {
    Found,
    Empty,
    Aborted,
}

/// Early-exit "does this folder contain at least one supported image anywhere
/// under it?" — the per-candidate check of the ⌘←/→ empty-folder skip (#49).
/// Deliberately NOT the streaming scan: it stops at the **first** hit, drives
/// no playlist/dialog machinery, and bails at `cancel`/`deadline` (checked per
/// entry, so a dead share aborts within one entry's latency). Shares
/// [`image_walker`]'s hostile-tree hardening: iterative, symlinks never
/// followed, unreadable entries skipped rather than fatal.
pub fn dir_has_image(dir: &Path, cancel: &AtomicBool, deadline: Instant) -> Probe {
    for entry in image_walker(dir, true) {
        if cancel.load(Ordering::Relaxed) || Instant::now() > deadline {
            return Probe::Aborted;
        }
        let Ok(entry) = entry else {
            continue; // permission-denied / vanished mid-walk — skip, don't abort
        };
        if entry.file_type().is_file() && is_supported_library_file(entry.path()) {
            return Probe::Found;
        }
    }
    Probe::Empty
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
        } else if ft.is_file() && is_supported_library_file(entry.path()) {
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
            paths.sort_by(|a, b| ci_path_cmp(a, b));
            let root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
            (paths, root, roots.first().cloned(), *recursive)
        }
        Source::Explicit(files) => {
            let paths: Vec<PathBuf> = files
                .iter()
                .filter(|p| is_supported_library_file(p.as_path()))
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
    // Hide Live-Photo companion .movs — except an explicitly-opened one (Cursor::At),
    // which must appear even if a same-stem still sits beside it.
    let exempt = match cursor {
        open::Cursor::At(p) => Some(p.as_path()),
        open::Cursor::First => None,
    };
    let paths = dedup_companions(paths, exempt);
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
    // Live-Photo companion dedup, batch-safe: entries publish only once their same-stem
    // run resolves, so a companion .mov is never in a batch and later gone (task #79).
    // The explicitly-opened target is exempt — it must appear even if it's a companion.
    let mut filter = CompanionFilter::new(target.clone());
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
            } else if ft.is_file() && is_supported_library_file(entry.path()) {
                for p in filter.push(entry.into_path()) {
                    progress.incr_found();
                    if gated && target.as_ref() == Some(&p) {
                        gated = false; // the opened file is now in the snapshot — emits may start
                    }
                    paths.push(p);
                }
                if !gated && paths.len() > sent_len && last_emit.elapsed() >= SCAN_BATCH_INTERVAL {
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
    // Resolve whatever the last same-stem run still holds (end of walk / cancellation).
    // No gate update needed: the final batch below is unconditional.
    for p in filter.finish() {
        progress.incr_found();
        paths.push(p);
    }
    // Final batch: the un-emitted remainder, or the only batch for a fast folder.
    if !paths.is_empty() && (paths.len() > sent_len || sent_len == 0) {
        let snap = build_resolved(paths, &cursor, root, scan_root, recursive);
        let _ = tx.send((generation, ScanUpdate::Batch(snap)));
    }
    let _ = tx.send((generation, ScanUpdate::Done));
}

// ── Archive open + top-level playlist resolution (NS0 5.6 Step 2c) ──

/// Open `path` as an archive playlist, dispatching by extension: `.7z` ->
/// [`SevenZSource`] (eager, RAM-budget pre-flight), anything else -> [`ZipSource`]
/// (lazy per-entry). Returns a structured [`ArchiveOpenError`](crate::archive::ArchiveOpenError)
/// so the caller can show the right message; entries are read into RAM, never
/// extracted to disk.
///
/// `password` decrypts an encrypted archive (`None` on the first open; an encrypted
/// archive then returns [`PasswordRequired`](crate::archive::ArchiveOpenError::PasswordRequired)
/// so the app can prompt, and a re-open carries the entered password). A ZIP's
/// directory reads without one, and a *wrong* password still opens — so an actual
/// entry decrypt ([`ZipSource::password_ok`]) is what catches it.
pub fn open_archive(
    path: &Path,
    password: Option<String>,
) -> Result<Resolved, crate::archive::ArchiveOpenError> {
    let is_7z = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("7z"));
    if is_7z {
        let projected = seven_z_preflight(path, password.as_deref())?;
        let mt_headroom = crate::archive::ram_budget().saturating_sub(projected);
        // Synchronous safety-net path (the interactive paths use the async, cancellable
        // `begin_archive_open`): a throwaway progress handle no UI reads.
        load_seven_z(path, password, &pb_source::OpenProgress::new(), mt_headroom)
    } else {
        let has_password = password.is_some();
        let zs = ZipSource::open(path, password, is_supported_archive_entry)?;
        // Encrypted but no password supplied -> prompt for one.
        if zs.needs_password() {
            return Err(crate::archive::ArchiveOpenError::PasswordRequired);
        }
        // A password was supplied but it doesn't decrypt -> prompt again (open
        // succeeds regardless of the password; the entry read is the real check).
        if has_password && zs.is_encrypted() && !zs.password_ok() {
            return Err(crate::archive::ArchiveOpenError::PasswordRequired);
        }
        if zs.is_empty() {
            return Err(crate::archive::ArchiveOpenError::Empty);
        }
        Ok(archive_resolved(path, Arc::new(zs)))
    }
}

/// The RAM pre-flight for a 7z: predict-and-refuse before the (uncatchable) eager
/// decompress, so an archive whose resident image bytes won't fit the budget is
/// rejected instantly rather than aborting partway in. `password` is only needed
/// for a header-encrypted archive (else the header reads without one); a wrong /
/// missing one surfaces as `PasswordRequired` here, routing to the prompt.
///
/// On success returns the projected resident bytes, so the caller can hand the
/// *unused* budget to [`load_seven_z`] as MT headroom without a second header read
/// (an encrypted header pays a full AES key derivation per read).
pub fn seven_z_preflight(
    path: &Path,
    password: Option<&str>,
) -> Result<u64, crate::archive::ArchiveOpenError> {
    seven_z_preflight_within(path, password, crate::archive::ram_budget())
}

/// The pre-flight comparison against an explicit `budget`, split out from
/// [`seven_z_preflight`] so tests can drive the over-budget refusal path
/// *deterministically* with an injected ceiling — rather than racing on the
/// process-global `PB_ARCHIVE_RAM_BUDGET` env var (Rust runs tests in parallel
/// threads, so mutating the environment from one test corrupts the others).
pub fn seven_z_preflight_within(
    path: &Path,
    password: Option<&str>,
    budget: u64,
) -> Result<u64, crate::archive::ArchiveOpenError> {
    let needed = seven_z_projected_bytes(path, password, is_supported_archive_entry)?;
    if needed > budget {
        return Err(crate::archive::ArchiveOpenError::TooLarge { needed, budget });
    }
    Ok(needed)
}

/// Eager-decompress a 7z into a [`Resolved`] (no pre-flight here — the caller runs
/// [`seven_z_preflight`] first). This is the slow step the runtime path runs on a
/// background thread (see `App::begin_archive_open`). `password` decrypts an
/// encrypted archive; a wrong one fails decode and surfaces as `PasswordRequired`.
///
/// `mt_headroom` is the transient RAM (beyond the resident entries) the open may
/// spend on within-block multithreaded LZMA2 decode — the solid-archive speedup
/// (see `SevenZSource::open_with_progress`). Callers pass the budget the pre-flight
/// left unused (`ram_budget() - projected`); `0` keeps the single-threaded path.
pub fn load_seven_z(
    path: &Path,
    password: Option<String>,
    progress: &pb_source::OpenProgress,
    mt_headroom: u64,
) -> Result<Resolved, crate::archive::ArchiveOpenError> {
    let src = SevenZSource::open_with_progress(
        path,
        password,
        is_supported_archive_entry,
        Some(progress),
        mt_headroom,
    )?;
    if src.is_empty() {
        return Err(crate::archive::ArchiveOpenError::Empty);
    }
    Ok(archive_resolved(path, Arc::new(src)))
}

/// Turn a planned [`Source`] into a concrete [`PhotoSource`] plus playlist framing.
/// Scans and explicit lists become an [`FsSource`]; an archive opens a
/// [`ZipSource`] (entries read into RAM on demand, never extracted to disk). On a
/// hard archive failure it logs and falls back to an empty source.
pub fn resolve_playlist(source: &Source, cursor: &open::Cursor) -> Resolved {
    match source {
        Source::Scan { .. } | Source::Explicit(_) => resolve_scan(source, cursor, None),
        // The launch / picker / drop paths open archives via the async-aware
        // `App::begin_archive_open` (which surfaces failures through the egui
        // dialog), so this arm is only a safety net: log and show empty on failure.
        Source::Archive(path) => match open_archive(path, None) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("PhotoBlaze: {}", e.user_message());
                Resolved::empty()
            }
        },
    }
}

#[cfg(test)]
mod archive_video_tests {
    use super::*;
    use std::io::Write;

    fn write_zip(tag: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "pb_scan_av_{tag}_{}_{}.zip",
            std::process::id(),
            std::thread::current().name().unwrap_or("t").len()
        ));
        let f = std::fs::File::create(&path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        for (name, bytes) in files {
            zw.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap();
        path
    }

    #[test]
    fn the_archive_predicate_unions_images_and_video_containers() {
        for ext in ["jpg", "PNG", "mp4", "MOV", "mkv", "webm"] {
            assert!(is_supported_archive_entry(ext), "{ext} must be indexable");
        }
        for ext in ["txt", "exe", "pdf", ""] {
            assert!(!is_supported_archive_entry(ext), "{ext} must be excluded");
        }
    }

    /// The reported bug: an archive containing ONLY videos refused to open
    /// (`ArchiveOpenError::Empty`). It must open as a playlist of typed video items.
    #[test]
    fn a_video_only_zip_opens_with_typed_video_items() {
        let zip = write_zip(
            "vidonly",
            &[
                ("b_clip.MOV", b"fake mov".as_slice()),
                ("a_clip.mp4", b"fake mp4"),
            ],
        );
        let r = open_archive(&zip, None).expect("a video-only archive must open");
        assert_eq!(r.source.len(), 2);
        let names: Vec<&str> = (0..2).map(|i| r.source.name(i)).collect();
        assert_eq!(names, vec!["a_clip.mp4", "b_clip.MOV"]);
        for i in 0..2 {
            assert!(
                matches!(
                    crate::video::item_kind(r.source.as_ref(), i),
                    crate::video::LibraryItemKind::Video(_)
                ),
                "entry {i} must classify as a video"
            );
        }
        let _ = std::fs::remove_file(&zip);
    }

    /// Mixed archives interleave images and videos in one sorted deck, and the
    /// classification per entry follows the name.
    #[test]
    fn a_mixed_zip_lists_images_and_videos_together() {
        let zip = write_zip(
            "mixed",
            &[
                ("photo.jpg", b"J".as_slice()),
                ("clip.mp4", b"fake mp4"),
                ("notes.txt", b"never indexed"),
            ],
        );
        let r = open_archive(&zip, None).expect("open");
        let names: Vec<&str> = (0..r.source.len()).map(|i| r.source.name(i)).collect();
        assert_eq!(names, vec!["clip.mp4", "photo.jpg"]);
        assert!(matches!(
            crate::video::item_kind(r.source.as_ref(), 0),
            crate::video::LibraryItemKind::Video(crate::video::VideoContainer::Mp4)
        ));
        assert!(matches!(
            crate::video::item_kind(r.source.as_ref(), 1),
            crate::video::LibraryItemKind::Image
        ));
        let _ = std::fs::remove_file(&zip);
    }

    /// A truly empty archive (nothing indexable at all) still refuses with `Empty`.
    #[test]
    fn a_zip_with_no_indexable_entries_still_reports_empty() {
        let zip = write_zip("empty", &[("readme.txt", b"nope".as_slice())]);
        match open_archive(&zip, None) {
            Err(crate::archive::ArchiveOpenError::Empty) => {}
            other => panic!("expected Empty, got {other:?}"),
        }
        let _ = std::fs::remove_file(&zip);
    }
}

#[cfg(test)]
mod dedup_tests {
    use super::{dedup_companions, CompanionFilter};
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn companion_mov_is_hidden_next_to_a_same_stem_image() {
        // The still is a `.jpg` so the pair resolves in every build config: HEIC is an
        // image only where a still decoder exists (WIC/ImageIO, or Linux + `libheif`),
        // and this test asserts the companion-dedup mechanics, not the still's format.
        let got = dedup_companions(
            vec![
                p(r"D:\x\IMG_1.jpg"),
                p(r"D:\x\IMG_1.MOV"),
                p(r"D:\x\IMG_2.jpg"),
            ],
            None,
        );
        assert_eq!(got, vec![p(r"D:\x\IMG_1.jpg"), p(r"D:\x\IMG_2.jpg")]);
    }

    #[test]
    fn only_mov_qt_are_companions_and_solo_videos_stay() {
        // An .mp4 beside a same-stem image is NOT a Live pair; a solo .mov stays.
        let got = dedup_companions(
            vec![
                p(r"D:\x\IMG_1.jpg"),
                p(r"D:\x\IMG_1.mp4"),
                p(r"D:\x\clip.mov"),
                p(r"D:\x\IMG_9.qt"),
                p(r"D:\x\IMG_9.png"),
            ],
            None,
        );
        assert_eq!(
            got,
            vec![
                p(r"D:\x\IMG_1.jpg"),
                p(r"D:\x\IMG_1.mp4"),
                p(r"D:\x\clip.mov"),
                p(r"D:\x\IMG_9.png"),
            ],
            ".qt companions hide; .mp4 and solo .mov stay"
        );
    }

    #[test]
    fn pairing_is_per_directory_and_case_insensitive() {
        // Forward-slash paths so the directory split is real on every platform (Windows
        // Path treats `/` as a separator too; `D:\a\…` would collapse to one component on
        // Unix and defeat the per-directory check). A `.jpg` still resolves in every config.
        let got = dedup_companions(
            vec![
                p("/a/img_1.jpg"),
                p("/a/IMG_1.MOV"), // pairs (case-insensitive stem, same dir)
                p("/b/IMG_1.MOV"), // different dir — no still there, stays
            ],
            None,
        );
        assert_eq!(got, vec![p("/a/img_1.jpg"), p("/b/IMG_1.MOV")]);
    }

    #[test]
    fn the_explicitly_opened_companion_is_exempt() {
        let target = p(r"D:\x\IMG_1.MOV");
        let got = dedup_companions(
            vec![p(r"D:\x\IMG_1.HEIC"), p(r"D:\x\IMG_1.MOV")],
            Some(target.as_path()),
        );
        assert_eq!(got, vec![p(r"D:\x\IMG_1.HEIC"), p(r"D:\x\IMG_1.MOV")]);
    }

    #[test]
    fn streaming_filter_matches_the_batch_dedup_even_when_the_mov_sorts_first() {
        // "AAA.MOV" < "AAA.PNG": the companion arrives BEFORE the image that hides it.
        // Feeding sorted walk order must give exactly the batch result, entry by entry,
        // and no published entry may ever be one the batch pass would drop
        // (batches never publish-then-remove).
        let sorted = vec![
            p(r"D:\x\AAA.MOV"),
            p(r"D:\x\AAA.PNG"),
            p(r"D:\x\IMG_1.HEIC"),
            p(r"D:\x\IMG_1.MOV"),
            p(r"D:\x\IMG_2.MOV"),
            p(r"D:\x\ZZZ.jpg"),
            p(r"D:\y\IMG_1.MOV"),
        ];
        let expect = dedup_companions(sorted.clone(), None);

        let mut filter = CompanionFilter::new(None);
        let mut streamed = Vec::new();
        for path in sorted {
            streamed.extend(filter.push(path));
            // Invariant: everything published so far survives the batch dedup.
            for s in &streamed {
                assert!(expect.contains(s), "{s:?} published but batch-dropped");
            }
        }
        streamed.extend(filter.finish());
        assert_eq!(streamed, expect);
    }
}

#[cfg(test)]
mod stream_video_tests {
    use super::*;
    use std::sync::mpsc;

    /// End-to-end task #79 phase 1 scan behavior on a real tree: loose videos are listed,
    /// a Live-Photo companion .mov never appears in ANY batch (not published-then-removed
    /// — never published), batches only grow, and the streamed result equals the sync
    /// resolver's.
    #[test]
    fn scanning_lists_videos_hides_companions_and_batches_only_grow() {
        let dir = std::env::temp_dir().join(format!("pb_scanvid_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        // `.jpg` still (not `.heic`) so the companion pairs in every build config — HEIC
        // is only a recognized image where a still decoder exists (WIC/ImageIO, or Linux
        // + `libheif`); this test is about the scanner hiding the companion, not the format.
        for name in ["IMG_1.jpg", "IMG_1.mov", "clip.mp4", "photo.jpg"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        std::fs::write(dir.join("sub").join("solo.mov"), b"x").unwrap();

        let (tx, rx) = mpsc::channel();
        stream_scan(
            vec![dir.clone()],
            true,
            open::Cursor::First,
            dir.clone(),
            Some(dir.clone()),
            1,
            ScanProgress::new(),
            tx, // consumed: dropped when stream_scan returns, ending rx.iter()
        );
        let companion = dir.join("IMG_1.mov");
        let mut prev: Vec<PathBuf> = Vec::new();
        let mut finished = false;
        for (_, upd) in rx.iter() {
            match upd {
                ScanUpdate::Batch(r) => {
                    let paths: Vec<PathBuf> = (0..r.source.len())
                        .filter_map(|i| r.source.path(i).map(Path::to_path_buf))
                        .collect();
                    assert!(
                        !paths.contains(&companion),
                        "companion .mov must never be published"
                    );
                    assert_eq!(
                        &paths[..prev.len()],
                        &prev[..],
                        "batches must only ever grow (append-only)"
                    );
                    prev = paths;
                }
                ScanUpdate::Done => finished = true,
            }
        }
        assert!(finished, "scan must complete");
        // Case-insensitive name order: clip < IMG_1 < photo, then the subfolder.
        let expect = [
            dir.join("clip.mp4"),
            dir.join("IMG_1.jpg"),
            dir.join("photo.jpg"),
            dir.join("sub").join("solo.mov"),
        ];
        assert_eq!(
            prev, expect,
            "videos listed, companion hidden, sorted order"
        );

        // The sync resolver agrees with the stream.
        let sync = resolve_scan(
            &Source::Scan {
                roots: vec![dir.clone()],
                recursive: true,
            },
            &open::Cursor::First,
            None,
        );
        let sync_paths: Vec<PathBuf> = (0..sync.source.len())
            .filter_map(|i| sync.source.path(i).map(Path::to_path_buf))
            .collect();
        assert_eq!(sync_paths, expect);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Opening the companion itself (Cursor::At) keeps it visible — the user asked for
    /// that exact file.
    #[test]
    fn an_explicitly_opened_companion_stays_visible_in_the_stream() {
        let dir = std::env::temp_dir().join(format!("pb_scanvid_at_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("IMG_1.heic"), b"x").unwrap();
        std::fs::write(dir.join("IMG_1.mov"), b"x").unwrap();
        let target = dir.join("IMG_1.mov");

        let (tx, rx) = mpsc::channel();
        stream_scan(
            vec![dir.clone()],
            false,
            open::Cursor::At(target.clone()),
            dir.clone(),
            Some(dir.clone()),
            1,
            ScanProgress::new(),
            tx,
        );
        let mut last: Vec<PathBuf> = Vec::new();
        let mut start = 0usize;
        for (_, upd) in rx.iter() {
            if let ScanUpdate::Batch(r) = upd {
                last = (0..r.source.len())
                    .filter_map(|i| r.source.path(i).map(Path::to_path_buf))
                    .collect();
                start = r.start;
            }
        }
        assert!(last.contains(&target), "the opened companion must appear");
        assert_eq!(last[start], target, "and the cursor must land on it");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod order_tests {
    use super::{ci_name_cmp, ci_path_cmp};
    use std::cmp::Ordering;
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    #[test]
    fn case_insensitive_name_order_beats_raw_bytes() {
        // The reported case: lowercase "onlinethumbnailcache" (o) sorts BEFORE uppercase
        // "Screenshots" (S) — raw byte order (S=0x53 < o=0x6F) wrongly reverses them.
        assert_eq!(
            ci_name_cmp(
                OsStr::new("onlinethumbnailcache"),
                OsStr::new("Screenshots")
            ),
            Ordering::Less
        );
        // Deterministic tiebreak on exact-but-for-case names (total order).
        assert_eq!(
            ci_name_cmp(OsStr::new("Foo"), OsStr::new("foo")),
            Ordering::Less
        );
    }

    #[test]
    fn ci_path_sort_orders_folders_like_finder() {
        let mut v = [
            PathBuf::from("d/Screenshots/1.jpg"),
            PathBuf::from("d/apple/2.jpg"),
            PathBuf::from("d/Banana/3.jpg"),
        ];
        v.sort_by(|a, b| ci_path_cmp(a, b));
        let names: Vec<&str> = v
            .iter()
            .map(|p| p.parent().unwrap().file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["apple", "Banana", "Screenshots"]);
    }

    #[test]
    fn ci_path_cmp_is_component_wise_not_string() {
        // A shallower path sorts before a deeper one sharing its prefix.
        assert_eq!(
            ci_path_cmp(Path::new("a/b"), Path::new("a/b/c")),
            Ordering::Less
        );
    }

    #[test]
    fn files_sort_before_subfolders_in_the_same_parent() {
        // A folder's own photos — even "zebra" — come before anything in its subfolders,
        // even a subfolder named "sub" (which would otherwise interleave by name).
        let mut v = [
            PathBuf::from("a/sub/1.jpg"),
            PathBuf::from("a/zebra.jpg"),
            PathBuf::from("a/apple.jpg"),
        ];
        v.sort_by(|x, y| ci_path_cmp(x, y));
        let got: Vec<&str> = v.iter().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(got, ["a/apple.jpg", "a/zebra.jpg", "a/sub/1.jpg"]);
    }
}
