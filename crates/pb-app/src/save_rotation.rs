//! Save the current photo's in-RAM rotation to the file, **losslessly** (tasks.json
//! #29): we only rewrite the EXIF **Orientation** tag — the compressed pixel data is
//! left byte-for-byte untouched (no re-encode).
//!
//! The pure parts — composing the new orientation from the file's existing
//! orientation plus the applied quarter-turns ([`compose_orientation`]) and the
//! format-eligibility gate ([`is_orientation_writable`]) — are unit-tested here. The
//! actual EXIF write (atomic temp-write → rename) is the I/O shell ([`write_orientation`]).
//!
//! Policy: this **modifies the user's file**, so it only ever runs on an explicit
//! command (File ▸ Save Rotation / `Ctrl+S`), never as a byproduct of viewing — the
//! "modify only on explicit command" boundary documented in CLAUDE.md.

use little_exif::exif_tag::ExifTag;
use little_exif::metadata::Metadata;
use pb_render::Rotation;
use std::path::{Path, PathBuf};

/// Apply one 90° **clockwise** turn to an EXIF orientation (1..=8). The eight
/// orientations split into two 4-cycles under a quarter turn: the rotation-only set
/// `1→6→3→8→1` and the mirrored set `2→7→4→5→2`. Unknown values pass through (treated
/// as `1` = normal by the caller).
fn orientation_cw(o: u8) -> u8 {
    match o {
        1 => 6,
        6 => 3,
        3 => 8,
        8 => 1,
        2 => 7,
        7 => 4,
        4 => 5,
        5 => 2,
        other => other,
    }
}

/// Compose the file's existing EXIF orientation (`base`, 1..=8) with the in-RAM
/// rotation override (clockwise quarter-turns) into the orientation to write back —
/// so the stored pixels stay identical and only the tag changes. A non-1..=8 `base`
/// is treated as `1` (normal). Pure + unit-tested.
pub fn compose_orientation(base: u8, rot: Rotation) -> u8 {
    let mut o = if (1..=8).contains(&base) { base } else { 1 };
    let turns = match rot {
        Rotation::R0 => 0,
        Rotation::R90 => 1,
        Rotation::R180 => 2,
        Rotation::R270 => 3,
    };
    for _ in 0..turns {
        o = orientation_cw(o);
    }
    o
}

/// Whether `path`'s format supports a lossless EXIF Orientation rewrite. v1 is **JPEG
/// only** (the file carries an EXIF/APP1 segment and the pixel data is independent of
/// it). PNG/BMP/GIF have no standard EXIF orientation, and RAW must never be
/// rewritten — those are greyed out + toast "can't save" in the UI. Extendable to
/// TIFF/WebP/HEIC/AVIF later.
pub fn is_orientation_writable(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    matches!(ext.as_deref(), Some("jpg" | "jpeg" | "jpe" | "jfif"))
}

/// Read the file's current EXIF Orientation from already-parsed `md` (default `1` =
/// normal if absent).
fn base_orientation(md: &Metadata) -> u8 {
    md.get_tag(&ExifTag::Orientation(Vec::new()))
        .next()
        .and_then(|t| match t {
            ExifTag::Orientation(v) => v.first().copied(),
            _ => None,
        })
        .unwrap_or(1) as u8
}

/// Persist the in-RAM rotation `rot` to `path`'s EXIF **Orientation** tag, losslessly
/// and atomically. Reads the file's current orientation, composes the new one, writes
/// it to a sibling temp copy (so the compressed scan + other segments — ICC, etc. —
/// ride along untouched), then renames the temp over the original (atomic on the same
/// volume, so a crash mid-write can't corrupt the photo). Returns the orientation
/// value written. Caller must gate on [`is_orientation_writable`] (JPEG only).
pub fn write_orientation(path: &Path, rot: Rotation) -> std::io::Result<u8> {
    // Read existing metadata. little_exif errors ("No EXIF data found!") on a JPEG
    // with no EXIF segment — that's not a failure for us, just base orientation 1; we
    // start fresh and write_to_file inserts a new APP1/EXIF segment. A genuinely
    // missing/unreadable file surfaces at the copy step below.
    let mut md = Metadata::new_from_path(path).unwrap_or_else(|_| Metadata::new());
    let base = base_orientation(&md);
    let new = compose_orientation(base, rot);
    md.set_tag(ExifTag::Orientation(vec![new as u16]));

    // Operate on a temp copy in the same directory, then rename over the original.
    let tmp = temp_sibling(path);
    std::fs::copy(path, &tmp)?;
    if let Err(e) = md.write_to_file(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(new)
}

/// A sibling temp path in the same directory as `path`, **preserving the original
/// extension** (e.g. `IMG.jpg` → `IMG.pbsave.jpg`) so `little_exif` still recognizes
/// it as JPEG (it dispatches by extension) and the final rename is same-volume, hence
/// atomic. A fixed infix is fine: Save is interactive and one-at-a-time, and the copy
/// overwrites any stale leftover.
fn temp_sibling(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut name = std::ffi::OsString::new();
    if let Some(stem) = path.file_stem() {
        name.push(stem);
    }
    name.push(".pbsave.");
    match path.extension() {
        Some(ext) => name.push(ext),
        None => name.push("jpg"),
    }
    parent.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The JPEG's compressed entropy data: everything from the first Start-Of-Scan
    /// (`FF DA`) marker to EOF. Orientation lives in the EXIF/APP1 segment *before*
    /// SOS, so a lossless rewrite must leave this byte-identical.
    fn jpeg_scan(b: &[u8]) -> &[u8] {
        for i in 0..b.len().saturating_sub(1) {
            if b[i] == 0xFF && b[i + 1] == 0xDA {
                return &b[i..];
            }
        }
        &[]
    }

    /// Integration test: writing the orientation reads the file's current value,
    /// composes the new one, round-trips it, and leaves the compressed scan
    /// byte-identical (lossless). Chaining two saves exercises reading a non-default
    /// base on the second write. (ICC-profile preservation was verified separately
    /// against a real Display-P3 JPEG; the crate has no codec so pixels can't change.)
    #[test]
    fn write_orientation_round_trips_losslessly() {
        const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/orient6.jpg");
        let tmp = std::env::temp_dir().join(format!("pb_save_test_{}.jpg", std::process::id()));
        std::fs::write(&tmp, FIXTURE).unwrap();

        let orig = std::fs::read(&tmp).unwrap();
        // The fixture has no EXIF, so base is 1 — the first save *creates* the APP1
        // segment; the second save then *reads* the orientation it wrote.
        let base = Metadata::new_from_path(&tmp)
            .map(|m| base_orientation(&m))
            .unwrap_or(1);

        // First save: base → compose(base, R90).
        let w1 = write_orientation(&tmp, Rotation::R90).unwrap();
        assert_eq!(w1, compose_orientation(base, Rotation::R90));
        assert_eq!(
            base_orientation(&Metadata::new_from_path(&tmp).unwrap()),
            w1
        );
        assert_eq!(
            jpeg_scan(&orig),
            jpeg_scan(&std::fs::read(&tmp).unwrap()),
            "scan must stay byte-identical (lossless)"
        );

        // Second save reads the just-written (non-default) orientation as its base.
        let w2 = write_orientation(&tmp, Rotation::R90).unwrap();
        assert_eq!(w2, compose_orientation(w1, Rotation::R90));
        assert_eq!(
            base_orientation(&Metadata::new_from_path(&tmp).unwrap()),
            w2
        );
        assert_eq!(
            jpeg_scan(&orig),
            jpeg_scan(&std::fs::read(&tmp).unwrap()),
            "scan still identical after a second save"
        );

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn compose_from_normal_base() {
        assert_eq!(compose_orientation(1, Rotation::R0), 1);
        assert_eq!(compose_orientation(1, Rotation::R90), 6); // 90° CW
        assert_eq!(compose_orientation(1, Rotation::R180), 3);
        assert_eq!(compose_orientation(1, Rotation::R270), 8); // 270° CW = 90° CCW
    }

    #[test]
    fn compose_from_rotated_base() {
        // A portrait phone JPEG stored as 90°-CW (orientation 6): one more CW turn
        // lands on 180 (orientation 3); reaching back to 1 takes the full cycle.
        assert_eq!(compose_orientation(6, Rotation::R90), 3);
        assert_eq!(compose_orientation(6, Rotation::R180), 8);
        assert_eq!(compose_orientation(6, Rotation::R270), 1);
    }

    #[test]
    fn quarter_turn_is_a_4_cycle_for_every_orientation() {
        // Four clockwise quarter-turns return every orientation to itself.
        for base in 1u8..=8 {
            let mut o = base;
            for _ in 0..4 {
                o = orientation_cw(o);
            }
            assert_eq!(
                o, base,
                "orientation {base} should cycle back after 4 turns"
            );
        }
    }

    #[test]
    fn mirrored_orientations_stay_mirrored() {
        // The mirrored set {2,7,4,5} maps within itself under a quarter turn (a save
        // never silently un-mirrors a flipped source).
        for base in [2u8, 7, 4, 5] {
            assert!(matches!(
                compose_orientation(base, Rotation::R90),
                2 | 7 | 4 | 5
            ));
        }
    }

    #[test]
    fn invalid_base_treated_as_normal() {
        assert_eq!(compose_orientation(0, Rotation::R90), 6);
        assert_eq!(compose_orientation(99, Rotation::R0), 1);
    }

    #[test]
    fn eligibility_is_jpeg_only() {
        for ok in ["a.jpg", "a.JPEG", "photo.jpe", "x.JFIF", "/d/p/IMG_1.jpg"] {
            assert!(
                is_orientation_writable(Path::new(ok)),
                "{ok} should be writable"
            );
        }
        for no in [
            "a.png", "a.tif", "a.tiff", "a.webp", "a.heic", "a.cr2", "a.arw", "a",
        ] {
            assert!(
                !is_orientation_writable(Path::new(no)),
                "{no} should be blocked"
            );
        }
    }
}
