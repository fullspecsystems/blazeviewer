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

use pb_core::open;
use pb_source::{FsSource, PhotoSource};

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
