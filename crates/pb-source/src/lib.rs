//! `pb-source` — where a playlist's image bytes come from.
//!
//! `pb-core` navigates purely by index; it never knows whether item `i` is a file
//! on disk or an entry inside an archive. This crate is the seam that turns an
//! index into the **raw encoded bytes** to hand `pb_decode::decode_named_bytes`,
//! plus a display name (and, for real files, a path the info panel can stat).
//!
//! Two implementations ship today:
//! * [`FsSource`] — a plain, already-scanned filesystem listing (today's behavior
//!   behind the seam): `bytes(i)` is a `std::fs::read`.
//! * [`ZipSource`] — a ZIP archive read **per entry, on demand, into RAM**. ZIP's
//!   central directory gives names + offsets up front (so the playlist is known
//!   instantly without extracting anything), and each entry is independently
//!   decompressed — preserving the random-access, parallel-prefetch model the
//!   viewer is built around. Optionally decrypts password-protected archives
//!   (ZipCrypto or WinZip-AES).
//!
//! **Privacy.** Every read here is RAM-only; nothing is ever written to disk. This
//! is the archive analogue of the "on-disk I/O is read-only on the view path"
//! guarantee — opening a ZIP to view it never extracts it to a temp directory.
//!
//! ## Random access vs. eager sources
//! Both current sources are *random-access*: any item can be fetched cheaply and
//! independently, which is what the direction-biased prefetch ring assumes. A
//! future *solid* archive (solid 7z, tar.gz) can't seek to one entry without
//! decompressing the block before it; such a source would set
//! [`PhotoSource::random_access`] to `false` and pre-load the whole archive into
//! RAM on open. The trait is shaped so that's an implementation choice, not a
//! change to the seam.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use zip::ZipArchive;

/// A uniform, read-only source of encoded image bytes addressed by item index.
///
/// Implementations must be `Send + Sync`: the decode pool calls [`bytes`] from
/// many worker threads concurrently.
///
/// [`bytes`]: PhotoSource::bytes
pub trait PhotoSource: Send + Sync {
    /// Number of playable images in this source.
    fn len(&self) -> usize;

    /// Whether the source has no images.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A display name for item `i` (a file name, or the archive-relative entry
    /// path) for the title bar and info panel. It also carries the extension that
    /// `pb_decode::decode_named_bytes` routes the ambiguous formats (RAW/SVG/TGA)
    /// on, so it must end in the real extension. Out-of-range `i` returns `""`.
    fn name(&self, i: usize) -> &str;

    /// The real filesystem path of item `i`, when one exists: `Some` for
    /// [`FsSource`] (the info panel reads `fs::metadata` from it), `None` for
    /// archive entries, which have no standalone path on disk.
    fn path(&self, i: usize) -> Option<&Path> {
        let _ = i;
        None
    }

    /// The on-disk archive this source reads from, if it is one (e.g. the `.zip`),
    /// rather than loose files. `None` for a filesystem listing. The info panel
    /// shows it as the location for an entry that has no standalone [`path`].
    ///
    /// [`path`]: PhotoSource::path
    fn container(&self) -> Option<&Path> {
        None
    }

    /// Read the encoded bytes of item `i` into memory. Called off the event loop,
    /// on decode-pool worker threads. Out-of-range `i` is a `NotFound` error.
    fn bytes(&self, i: usize) -> io::Result<Vec<u8>>;

    /// Whether items can be fetched independently and cheaply (filesystem, ZIP)
    /// rather than requiring a one-time whole-archive decompression (solid 7z,
    /// tar.gz). The prefetch model assumes `true`; both current impls satisfy it.
    fn random_access(&self) -> bool {
        true
    }
}

/// A plain filesystem listing — today's behavior behind the seam. Holds the
/// already-scanned, already-ordered image paths; `bytes` is a `std::fs::read`.
pub struct FsSource {
    paths: Vec<PathBuf>,
    names: Vec<String>,
}

impl FsSource {
    /// Wrap an already-scanned, already-ordered list of image paths.
    pub fn new(paths: Vec<PathBuf>) -> Self {
        let names = paths.iter().map(|p| display_name(p)).collect();
        Self { paths, names }
    }
}

impl PhotoSource for FsSource {
    fn len(&self) -> usize {
        self.paths.len()
    }

    fn name(&self, i: usize) -> &str {
        self.names.get(i).map(String::as_str).unwrap_or("")
    }

    fn path(&self, i: usize) -> Option<&Path> {
        self.paths.get(i).map(PathBuf::as_path)
    }

    fn bytes(&self, i: usize) -> io::Result<Vec<u8>> {
        let path = self.paths.get(i).ok_or_else(out_of_range)?;
        std::fs::read(path)
    }
}

/// Why opening an archive failed.
#[derive(Debug)]
pub enum OpenError {
    /// The archive file could not be opened or read.
    Io(io::Error),
    /// The bytes are not a valid ZIP (bad signature, truncated central directory…).
    NotAZip(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Io(e) => write!(f, "{e}"),
            OpenError::NotAZip(why) => write!(f, "not a valid zip archive: {why}"),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<io::Error> for OpenError {
    fn from(e: io::Error) -> Self {
        OpenError::Io(e)
    }
}

/// One indexed image entry within a ZIP's central directory.
struct ZipEntry {
    /// Index of this entry in the ZIP's own directory order (what `by_index` takes).
    zip_index: usize,
    /// Archive-relative name — display + extension routing hint.
    name: String,
    /// Whether this entry's *contents* are encrypted.
    encrypted: bool,
}

/// A ZIP archive read per entry, on demand, into RAM.
///
/// Concurrency: ZIP random access needs `&mut` on a `ZipArchive`, so instead of a
/// single mutex that would serialize every read, this keeps a small **pool of
/// archive handles** (each its own file descriptor). A worker checks one out,
/// reads its entry, and returns it — so up to one handle exists per concurrent
/// caller and decode-pool threads read different entries truly in parallel.
pub struct ZipSource {
    path: PathBuf,
    /// Decryption password (raw bytes), if the archive is encrypted. Scrubbed on
    /// drop (best-effort — see `Drop`).
    password: Option<Vec<u8>>,
    entries: Vec<ZipEntry>,
    pool: Mutex<Vec<ZipArchive<BufReader<File>>>>,
}

impl ZipSource {
    /// Open `path` as a ZIP and index its supported-image entries.
    ///
    /// `is_supported` is the extension predicate (the app passes
    /// `pb_decode::is_supported_extension`, keeping a single source of truth);
    /// entries whose lowercased extension it rejects — and directory entries — are
    /// skipped. Surviving entries are ordered by name, matching a directory scan's
    /// sort, so navigation order is stable.
    ///
    /// `password` decrypts encrypted entries; pass `None` for a plain archive.
    /// Reading the central directory (names, count, encrypted flags) never needs a
    /// password, so the playlist is known even for an encrypted archive — see
    /// [`needs_password`](ZipSource::needs_password).
    pub fn open(
        path: impl Into<PathBuf>,
        password: Option<String>,
        is_supported: impl Fn(&str) -> bool,
    ) -> Result<Self, OpenError> {
        let path = path.into();
        let file = File::open(&path)?;
        let mut archive =
            ZipArchive::new(BufReader::new(file)).map_err(|e| OpenError::NotAZip(e.to_string()))?;

        let mut entries = Vec::new();
        for i in 0..archive.len() {
            // `by_index_raw` reads only the directory metadata — no decryption, so
            // it works for encrypted entries too (we just want names + flags here).
            let entry = archive
                .by_index_raw(i)
                .map_err(|e| OpenError::NotAZip(e.to_string()))?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().to_string();
            if !is_supported(&ext_of(&name)) {
                continue;
            }
            entries.push(ZipEntry {
                zip_index: i,
                name,
                encrypted: entry.encrypted(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(Self {
            path,
            password: password.map(String::into_bytes),
            entries,
            // Reuse the handle we already built for the central-directory scan.
            pool: Mutex::new(vec![archive]),
        })
    }

    /// Whether any indexed image entry is encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.entries.iter().any(|e| e.encrypted)
    }

    /// Whether reads will fail for lack of a password: the archive has encrypted
    /// content but none was supplied. The app uses this to surface a clear
    /// prompt/toast rather than a stream of decode errors.
    pub fn needs_password(&self) -> bool {
        self.password.is_none() && self.is_encrypted()
    }

    /// Borrow an archive handle from the pool, or open a fresh one (its own file
    /// descriptor) if the pool is empty.
    fn checkout(&self) -> io::Result<ZipArchive<BufReader<File>>> {
        if let Some(archive) = self.pool.lock().unwrap().pop() {
            return Ok(archive);
        }
        let file = File::open(&self.path)?;
        ZipArchive::new(BufReader::new(file)).map_err(zip_to_io)
    }

    fn checkin(&self, archive: ZipArchive<BufReader<File>>) {
        self.pool.lock().unwrap().push(archive);
    }
}

impl PhotoSource for ZipSource {
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn name(&self, i: usize) -> &str {
        self.entries.get(i).map(|e| e.name.as_str()).unwrap_or("")
    }

    fn container(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn bytes(&self, i: usize) -> io::Result<Vec<u8>> {
        let entry = self.entries.get(i).ok_or_else(out_of_range)?;
        let mut archive = self.checkout()?;
        let mut buf = Vec::new();
        {
            // Scope the entry borrow so the handle is free to return to the pool.
            let mut file = match &self.password {
                Some(pw) => archive
                    .by_index_decrypt(entry.zip_index, pw)
                    .map_err(zip_to_io)?,
                None => archive.by_index(entry.zip_index).map_err(zip_to_io)?,
            };
            buf.reserve(file.size() as usize);
            file.read_to_end(&mut buf)?;
        }
        self.checkin(archive);
        Ok(buf)
    }
}

impl Drop for ZipSource {
    fn drop(&mut self) {
        // Best-effort scrub of the password from RAM on teardown (privacy
        // guarantee). A proper volatile zeroize (the `zeroize` crate) is a
        // follow-up; this at least overwrites the bytes in the common build.
        if let Some(pw) = self.password.as_mut() {
            for b in pw.iter_mut() {
                *b = 0;
            }
        }
    }
}

/// The display name of a path: its final component, or `"?"` if it has none.
fn display_name(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string()
}

/// The lowercased extension (no dot) of a name or path, or `""` if none. Uses
/// `Path` so only the last component's extension counts (a ZIP entry name like
/// `trip/day1/IMG.JPG` → `jpg`).
fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

fn out_of_range() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "item index out of range")
}

/// Map a ZIP error onto `io::Error`, unwrapping a real I/O cause where present
/// (e.g. a wrong password becomes `InvalidData`).
fn zip_to_io(e: zip::result::ZipError) -> io::Error {
    match e {
        zip::result::ZipError::Io(io) => io,
        other => io::Error::new(io::ErrorKind::InvalidData, other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use zip::write::SimpleFileOptions;
    use zip::{AesMode, CompressionMethod, ZipWriter};

    // A test-local image-extension predicate (stands in for
    // `pb_decode::is_supported_extension`, which pb-source doesn't depend on).
    fn is_img(ext: &str) -> bool {
        matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tga")
    }

    static NONCE: AtomicUsize = AtomicUsize::new(0);

    fn temp_path(tag: &str, ext: &str) -> PathBuf {
        let n = NONCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("pb_src_{tag}_{}_{n}.{ext}", std::process::id()))
    }

    /// Build a ZIP in a temp file from (name, bytes) pairs; return its path.
    fn write_zip(tag: &str, files: &[(&str, &[u8])], password: Option<&str>) -> PathBuf {
        let path = temp_path(tag, "zip");
        let f = File::create(&path).unwrap();
        let mut zw = ZipWriter::new(f);
        for (name, bytes) in files {
            let mut opts =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            if let Some(pw) = password {
                opts = opts.with_aes_encryption(AesMode::Aes256, pw);
            }
            zw.start_file(*name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap();
        path
    }

    #[test]
    fn fs_source_exposes_names_paths_and_bytes() {
        let a = temp_path("fs_a", "jpg");
        let b = temp_path("fs_b", "png");
        std::fs::write(&a, b"alpha").unwrap();
        std::fs::write(&b, b"bravo").unwrap();

        let src = FsSource::new(vec![a.clone(), b.clone()]);
        assert_eq!(src.len(), 2);
        assert!(!src.is_empty());
        assert!(src.name(0).ends_with(".jpg"));
        assert_eq!(src.path(1), Some(b.as_path()));
        assert_eq!(src.container(), None, "a filesystem listing has no archive");
        assert_eq!(src.bytes(0).unwrap(), b"alpha");
        assert_eq!(src.bytes(1).unwrap(), b"bravo");
        assert!(src.bytes(2).is_err(), "out-of-range read errors");

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn zip_lists_supported_images_sorted_excluding_others() {
        let zip = write_zip(
            "list",
            &[
                ("b.png", b"B"),
                ("a.jpg", b"A"),
                ("notes.txt", b"text"),
                ("sub/c.webp", b"C"),
            ],
            None,
        );
        let src = ZipSource::open(&zip, None, is_img).unwrap();
        assert_eq!(src.len(), 3, "the .txt is excluded");
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["a.jpg", "b.png", "sub/c.webp"]);
        assert!(src.path(0).is_none(), "archive entries have no fs path");
        assert_eq!(
            src.container(),
            Some(zip.as_path()),
            "container is the archive"
        );
        assert!(!src.needs_password());
        let _ = std::fs::remove_file(&zip);
    }

    #[test]
    fn zip_reads_entry_bytes_by_index() {
        let zip = write_zip("read", &[("a.jpg", b"first"), ("b.png", b"second")], None);
        let src = ZipSource::open(&zip, None, is_img).unwrap();
        // Sorted order: a.jpg = 0, b.png = 1.
        assert_eq!(src.bytes(0).unwrap(), b"first");
        assert_eq!(src.bytes(1).unwrap(), b"second");
        assert!(src.bytes(99).is_err());
        let _ = std::fs::remove_file(&zip);
    }

    #[test]
    fn zip_with_no_images_is_empty() {
        let zip = write_zip("none", &[("readme.txt", b"x"), ("data.bin", b"y")], None);
        let src = ZipSource::open(&zip, None, is_img).unwrap();
        assert_eq!(src.len(), 0);
        assert!(src.is_empty());
        let _ = std::fs::remove_file(&zip);
    }

    #[test]
    fn zip_concurrent_reads_return_correct_bytes() {
        // Distinct contents per entry; many threads hammer different indices to
        // exercise the handle pool. Each must get exactly its entry's bytes.
        let files: Vec<(String, Vec<u8>)> = (0..8)
            .map(|i| (format!("img{i:02}.png"), vec![i as u8; 32 + i]))
            .collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let zip = write_zip("concurrent", &refs, None);

        let src = Arc::new(ZipSource::open(&zip, None, is_img).unwrap());
        assert_eq!(src.len(), 8);

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let src = src.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        let got = src.bytes(i).unwrap();
                        assert_eq!(got, vec![i as u8; 32 + i], "entry {i} mismatched");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let _ = std::fs::remove_file(&zip);
    }

    #[test]
    fn zip_encrypted_needs_password_then_decrypts() {
        let zip = write_zip(
            "enc",
            &[("a.jpg", b"secret-A"), ("b.png", b"secret-B")],
            Some("hunter2"),
        );

        // No password: the playlist is still readable, but content reads fail.
        let locked = ZipSource::open(&zip, None, is_img).unwrap();
        assert_eq!(locked.len(), 2, "names are readable without the password");
        assert!(locked.is_encrypted());
        assert!(locked.needs_password());
        assert!(locked.bytes(0).is_err(), "no password -> read fails");

        // Correct password: content decrypts.
        let open = ZipSource::open(&zip, Some("hunter2".into()), is_img).unwrap();
        assert!(!open.needs_password());
        assert_eq!(open.bytes(0).unwrap(), b"secret-A");
        assert_eq!(open.bytes(1).unwrap(), b"secret-B");

        // Wrong password: read fails rather than yielding garbage.
        let wrong = ZipSource::open(&zip, Some("nope".into()), is_img).unwrap();
        assert!(wrong.bytes(0).is_err(), "wrong password -> read fails");

        let _ = std::fs::remove_file(&zip);
    }

    #[test]
    fn open_rejects_a_non_zip_file() {
        let bogus = temp_path("bogus", "zip");
        std::fs::write(&bogus, b"this is definitely not a zip archive").unwrap();
        // Avoid `unwrap_err` (it needs the Ok type, ZipSource, to be Debug — the
        // handle pool isn't) by matching the error directly.
        match ZipSource::open(&bogus, None, is_img) {
            Err(OpenError::NotAZip(_)) => {}
            Err(other) => panic!("expected NotAZip, got {other:?}"),
            Ok(_) => panic!("expected an error opening a non-zip file"),
        }
        let _ = std::fs::remove_file(&bogus);
    }

    #[test]
    fn open_missing_file_is_an_io_error() {
        let missing = temp_path("missing", "zip");
        match ZipSource::open(&missing, None, is_img) {
            Err(OpenError::Io(_)) => {}
            Err(other) => panic!("expected Io, got {other:?}"),
            Ok(_) => panic!("expected an error opening a missing file"),
        }
    }
}
