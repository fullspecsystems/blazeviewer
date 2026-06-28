//! Launch intent -> playlist plan (pure policy + path math).
//!
//! Every way a user hands photos to PhotoBlaze -- a CLI path, a double-click via
//! a file association, drag-and-drop, the file picker, and later the macOS
//! `openFiles` Apple Event -- collapses to one question: *given this input, what
//! is the playlist and where does the cursor start?* This module answers it
//! without touching the disk, so it is fully unit-testable.
//!
//! The split that keeps `pb-core` I/O-free:
//! * the app's thin shim classifies the OS hand-off into [`LaunchInput`] (the
//!   only step that reads the filesystem -- an `fs::metadata` "file or folder?"),
//! * [`plan`] turns that into an [`OpenPlan`] (pure policy),
//! * the app performs the scan the plan asks for (`read_dir`/`walkdir` +
//!   extension filter -- also I/O), then
//! * [`resolve_cursor`] finds the start index in the final ordered paths (pure).
//!
//! This is the seam that makes the macOS port a delivery-layer change only: a
//! different shim builds the same [`LaunchInput`], and everything below is shared.

use std::path::PathBuf;

/// What the OS handed us, already classified by the app's I/O shim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchInput {
    /// Bare launch (Start menu / taskbar) with nothing to open.
    Empty,
    /// One or more concrete image files (double-click, multi-select, drop).
    Files(Vec<PathBuf>),
    /// A directory (folder picker, dropped folder, "Open with" on a folder).
    Directory(PathBuf),
    /// A single archive file (a `.zip`, double-clicked or dropped) to view the
    /// images inside. The shim classifies this; the bytes never hit disk.
    Archive(PathBuf),
}

/// Where a playlist's paths come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Scan these roots for supported images (recursively iff `recursive`).
    Scan {
        roots: Vec<PathBuf>,
        recursive: bool,
    },
    /// Use exactly these files, in order -- no scan.
    Explicit(Vec<PathBuf>),
    /// View the images inside this archive (e.g. a `.zip`), read entry-by-entry
    /// into RAM — never extracted to disk (the no-trace privacy guarantee).
    Archive(PathBuf),
}

/// Which item the cursor should land on once the playlist is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cursor {
    /// The first item in the built order.
    First,
    /// The item equal to this path (e.g. the photo that was double-clicked).
    At(PathBuf),
}

/// A resolved plan for turning a launch into a playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPlan {
    pub source: Source,
    pub cursor: Cursor,
}

/// The agreed launch policy (owner decision 2026-06-27):
///
/// * a **single file** -> scan its folder *flat*, cursor on that file (you
///   pointed at one photo in one place);
/// * **several files** -> exactly those files, cursor on the first;
/// * a **directory** -> scan it *recursively*, cursor on the first;
/// * **nothing** -> an empty plan (the app shows its empty state / picker).
///
/// A lone file with no parent directory (e.g. a bare root) falls back to an
/// explicit single-item list so it still opens.
pub fn plan(input: LaunchInput) -> OpenPlan {
    match input {
        LaunchInput::Empty => OpenPlan {
            source: Source::Explicit(Vec::new()),
            cursor: Cursor::First,
        },
        LaunchInput::Directory(dir) => OpenPlan {
            source: Source::Scan {
                roots: vec![dir],
                recursive: true,
            },
            cursor: Cursor::First,
        },
        LaunchInput::Archive(archive) => OpenPlan {
            source: Source::Archive(archive),
            cursor: Cursor::First,
        },
        LaunchInput::Files(mut files) => {
            if files.len() == 1 {
                let file = files.pop().expect("len == 1");
                match file.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => OpenPlan {
                        source: Source::Scan {
                            roots: vec![parent.to_path_buf()],
                            recursive: false,
                        },
                        cursor: Cursor::At(file),
                    },
                    // No usable parent (bare root, or a name with no dir): just
                    // open the one file rather than scanning the whole drive.
                    _ => OpenPlan {
                        source: Source::Explicit(vec![file]),
                        cursor: Cursor::First,
                    },
                }
            } else {
                OpenPlan {
                    source: Source::Explicit(files),
                    cursor: Cursor::First,
                }
            }
        }
    }
}

/// Find the index the cursor should start at within the final, ordered playlist.
///
/// The caller passes paths it has already normalized to a consistent form -- the
/// scan and the launch target come from the same place (the same directory
/// listing), so exact comparison is reliable. An unfound target -- or
/// [`Cursor::First`] -- starts at 0.
pub fn resolve_cursor(ordered: &[PathBuf], cursor: &Cursor) -> usize {
    match cursor {
        Cursor::First => 0,
        Cursor::At(target) => ordered.iter().position(|p| p == target).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn empty_launch_yields_empty_explicit_plan() {
        let p = plan(LaunchInput::Empty);
        assert_eq!(p.source, Source::Explicit(Vec::new()));
        assert_eq!(p.cursor, Cursor::First);
    }

    #[test]
    fn directory_scans_recursively_from_first() {
        let p = plan(LaunchInput::Directory(PathBuf::from("/photos/trip")));
        assert_eq!(
            p.source,
            Source::Scan {
                roots: vec![PathBuf::from("/photos/trip")],
                recursive: true,
            }
        );
        assert_eq!(p.cursor, Cursor::First);
    }

    #[test]
    fn single_file_scans_parent_flat_with_cursor_on_it() {
        let file = PathBuf::from("/photos/trip/IMG_0042.jpg");
        let p = plan(LaunchInput::Files(vec![file.clone()]));
        assert_eq!(
            p.source,
            Source::Scan {
                roots: vec![PathBuf::from("/photos/trip")],
                recursive: false,
            }
        );
        assert_eq!(p.cursor, Cursor::At(file));
    }

    #[test]
    fn archive_becomes_an_archive_source_from_first() {
        let zip = PathBuf::from("/photos/trip.zip");
        let p = plan(LaunchInput::Archive(zip.clone()));
        assert_eq!(p.source, Source::Archive(zip));
        assert_eq!(p.cursor, Cursor::First);
    }

    #[test]
    fn single_file_without_parent_falls_back_to_explicit() {
        let p = plan(LaunchInput::Files(vec![PathBuf::from("solo.jpg")]));
        assert_eq!(p.source, Source::Explicit(vec![PathBuf::from("solo.jpg")]));
        assert_eq!(p.cursor, Cursor::First);
    }

    #[test]
    fn multiple_files_become_an_explicit_playlist_from_first() {
        let files = vec![
            PathBuf::from("/a/one.jpg"),
            PathBuf::from("/b/two.png"),
            PathBuf::from("/c/three.webp"),
        ];
        let p = plan(LaunchInput::Files(files.clone()));
        assert_eq!(p.source, Source::Explicit(files));
        assert_eq!(p.cursor, Cursor::First);
    }

    #[test]
    fn resolve_cursor_first_is_zero_even_when_empty() {
        let ordered: Vec<PathBuf> = Vec::new();
        assert_eq!(resolve_cursor(&ordered, &Cursor::First), 0);
    }

    #[test]
    fn resolve_cursor_finds_the_target() {
        let ordered = vec![
            PathBuf::from("/photos/a.jpg"),
            PathBuf::from("/photos/b.jpg"),
            PathBuf::from("/photos/c.jpg"),
        ];
        let cur = Cursor::At(PathBuf::from("/photos/b.jpg"));
        assert_eq!(resolve_cursor(&ordered, &cur), 1);
    }

    #[test]
    fn resolve_cursor_missing_target_starts_at_zero() {
        let ordered = vec![PathBuf::from("/photos/a.jpg")];
        let cur = Cursor::At(PathBuf::from("/photos/gone.jpg"));
        assert_eq!(resolve_cursor(&ordered, &cur), 0);
    }

    #[test]
    fn end_to_end_double_click_then_locate_in_scanned_folder() {
        // Simulate: user double-clicks b.jpg; the app scans the parent (flat),
        // sorts, then asks pb-core where the cursor goes.
        let clicked = PathBuf::from("/photos/trip/b.jpg");
        let p = plan(LaunchInput::Files(vec![clicked.clone()]));
        let Source::Scan { recursive, .. } = &p.source else {
            panic!("a single file should scan its parent");
        };
        assert!(!recursive, "double-clicking one photo stays flat");

        // What the app's scan-of-parent would produce, sorted by path.
        let scanned = vec![
            PathBuf::from("/photos/trip/a.jpg"),
            PathBuf::from("/photos/trip/b.jpg"),
            PathBuf::from("/photos/trip/c.jpg"),
        ];
        assert_eq!(resolve_cursor(&scanned, &p.cursor), 1);
    }

    #[test]
    fn cursor_at_is_path_typed() {
        // Guard the Cursor::At payload is a path we can compare against a scan.
        let cur = Cursor::At(PathBuf::from("/x/y.jpg"));
        if let Cursor::At(p) = &cur {
            assert_eq!(p.as_path(), Path::new("/x/y.jpg"));
        } else {
            panic!("expected Cursor::At");
        }
    }
}
