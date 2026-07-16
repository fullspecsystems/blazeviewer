//! The one archive classifier (task #102). Every "is this an archive, and which
//! kind?" question routes through [`archive_kind`] — the shells' `is_archive`
//! predicates and `scan::open_archive`'s dispatch previously each hand-rolled a
//! `zip|7z` extension check and only agreed by luck.

use std::path::Path;

/// An archive format the app opens as a playlist, and which access model its
/// open uses (see [`ArchiveKind::eager`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    /// ZIP — lazy per-entry random access ([`crate::ZipSource`]).
    Zip,
    /// 7-Zip — eager decode-to-RAM ([`crate::SevenZSource`]).
    SevenZ,
    /// Plain (uncompressed) tar — lazy: headers sit at known offsets, so the
    /// index pass seeks over file data and `bytes(i)` is an open + seek + read.
    Tar,
    /// `.tar.gz` / `.tgz` — one solid DEFLATE stream: eager, like solid 7z.
    TarGz,
    /// `.tar.bz2` / `.tbz2` / `.tbz` — solid bzip2 stream: eager.
    TarBz2,
    /// `.tar.zst` / `.tzst` — solid zstd stream: eager.
    TarZst,
    /// `.tar.xz` / `.txz` — solid xz stream: eager.
    TarXz,
}

impl ArchiveKind {
    /// Whether opening this kind decompresses the whole archive into RAM up
    /// front (the RAM-budget model), rather than serving entries lazily (ZIP's
    /// central directory, plain tar's header index).
    pub fn eager(&self) -> bool {
        !matches!(self, ArchiveKind::Zip | ArchiveKind::Tar)
    }

    /// Whether the open must run off the event loop (the worker thread with the
    /// progress + Cancel dialog). Distinct from [`eager`](ArchiveKind::eager) —
    /// access model and open *scheduling* are different concepts: a plain tar is
    /// lazy, but its index pass is still O(entries) of file I/O and can stall
    /// for seconds on a huge or network-mounted archive.
    pub fn background_open(&self) -> bool {
        !matches!(self, ArchiveKind::Zip)
    }
}

/// Classify `path` as an archive we open as a playlist, or `None`.
///
/// Case-insensitive, and aware of the double extensions `Path::extension()`
/// misses: `.tar.gz` reports `gz`, so the compression suffix is checked first
/// and the remaining stem must end in `.tar`. A bare `photo.jpg.gz` is `None` —
/// single compressed images are a separate (phase 5) feature, and offering a
/// file we then refuse would be worse than not matching it.
pub fn archive_kind(path: &Path) -> Option<ArchiveKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "zip" => ArchiveKind::Zip,
        "7z" => ArchiveKind::SevenZ,
        "tar" => ArchiveKind::Tar,
        "tgz" => ArchiveKind::TarGz,
        "tbz2" | "tbz" => ArchiveKind::TarBz2,
        "tzst" => ArchiveKind::TarZst,
        "txz" => ArchiveKind::TarXz,
        "gz" if stem_is_tar(path) => ArchiveKind::TarGz,
        "bz2" if stem_is_tar(path) => ArchiveKind::TarBz2,
        "zst" if stem_is_tar(path) => ArchiveKind::TarZst,
        "xz" if stem_is_tar(path) => ArchiveKind::TarXz,
        _ => return None,
    })
}

/// Whether the path minus its final extension ends in `.tar` (so `a.tar.gz`
/// matches but `photo.jpg.gz` does not), case-insensitively.
fn stem_is_tar(path: &Path) -> bool {
    path.file_stem()
        .map(Path::new)
        .and_then(|s| s.extension())
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tar"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(p: &str) -> Option<ArchiveKind> {
        archive_kind(Path::new(p))
    }

    #[test]
    fn classifies_the_double_extension_matrix() {
        use ArchiveKind::*;
        for (path, want) in [
            ("a.zip", Zip),
            ("a.7z", SevenZ),
            ("a.tar", Tar),
            ("a.tar.gz", TarGz),
            ("a.tgz", TarGz),
            ("a.tar.bz2", TarBz2),
            ("a.tbz2", TarBz2),
            ("a.tbz", TarBz2),
            ("a.tar.zst", TarZst),
            ("a.tzst", TarZst),
            ("a.tar.xz", TarXz),
            ("a.txz", TarXz),
        ] {
            assert_eq!(kind(path), Some(want), "{path}");
        }
    }

    #[test]
    fn classification_is_case_insensitive() {
        use ArchiveKind::*;
        for (path, want) in [
            ("A.ZIP", Zip),
            ("photos.TAR", Tar),
            ("photos.tar.GZ", TarGz),
            ("photos.TAR.gz", TarGz),
            ("photos.Tar.Bz2", TarBz2),
            ("photos.TGZ", TarGz),
        ] {
            assert_eq!(kind(path), Some(want), "{path}");
        }
    }

    #[test]
    fn non_archives_and_bare_compressed_files_are_none() {
        for path in [
            "photo.jpg",
            "notes.txt",
            "noext",
            // Bare compressed single files: phase 5, not archives today.
            "photo.jpg.gz",
            "drawing.svgz",
            "data.bz2",
            "big.zst",
            "log.xz",
            "a.gz",
            // Only the FINAL extension pair counts.
            "a.tar.gz.txt",
            // A file literally named "tar.gz" has stem "tar" with no inner
            // extension — nothing says it is a tarball.
            "tar.gz",
        ] {
            assert_eq!(kind(path), None, "{path}");
        }
    }

    #[test]
    fn dotted_names_still_resolve_the_inner_tar() {
        // Extra dots in the stem must not confuse the `.tar` check.
        assert_eq!(kind("trip.2024.tar.gz"), Some(ArchiveKind::TarGz));
        assert_eq!(kind("trip.2024.jpg.gz"), None);
    }

    #[test]
    fn eagerness_matches_the_access_model() {
        use ArchiveKind::*;
        for k in [Zip, Tar] {
            assert!(!k.eager(), "{k:?} is lazy");
        }
        for k in [SevenZ, TarGz, TarBz2, TarZst, TarXz] {
            assert!(k.eager(), "{k:?} is eager");
        }
    }

    #[test]
    fn every_kind_but_zip_opens_in_the_background() {
        use ArchiveKind::*;
        assert!(!Zip.background_open(), "zip stays the cheap sync open");
        for k in [SevenZ, Tar, TarGz, TarBz2, TarZst, TarXz] {
            assert!(k.background_open(), "{k:?} opens off-thread");
        }
    }
}
