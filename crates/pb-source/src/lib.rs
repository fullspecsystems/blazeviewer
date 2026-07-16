//! `pb-source` — where a playlist's image bytes come from.
//!
//! `pb-core` navigates purely by index; it never knows whether item `i` is a file
//! on disk or an entry inside an archive. This crate is the seam that turns an
//! index into the **raw encoded bytes** to hand `pb_decode::decode_named_bytes`,
//! plus a display name (and, for real files, a path the info panel can stat).
//!
//! Four implementations ship today:
//! * [`FsSource`] — a plain, already-scanned filesystem listing (today's behavior
//!   behind the seam): `bytes(i)` is a `std::fs::read`.
//! * [`ZipSource`] — a ZIP archive read **per entry, on demand, into RAM**. ZIP's
//!   central directory gives names + offsets up front (so the playlist is known
//!   instantly without extracting anything), and each entry is independently
//!   decompressed — preserving the random-access, parallel-prefetch model the
//!   viewer is built around. Optionally decrypts password-protected archives
//!   (ZipCrypto or WinZip-AES).
//! * [`SevenZSource`] — a 7-Zip archive **eagerly decompressed into RAM on open**.
//!   7z is usually *solid* (many files share one LZMA2 stream), so there is no
//!   cheap per-entry random access; instead every supported-image entry is decoded
//!   once up front into a resident `index → bytes` map. Pre-flight an open with
//!   [`seven_z_projected_bytes`] against a memory budget.
//! * [`TarSource`] — the tar family (task #102): a plain `.tar` is **lazy** like
//!   ZIP (headers indexed by seeking; `bytes(i)` = open + seek + read), while
//!   `.tar.gz`/`.tar.bz2`/`.tar.zst`/`.tar.xz` are solid streams and go **eager**
//!   like 7z — with the RAM budget enforced *mid-stream* (no size table exists up
//!   front). Which is which is decided by [`archive_kind`], the one classifier
//!   every "is this an archive?" question routes through.
//!
//! **Privacy.** Every read here is RAM-only; nothing is ever written to disk. This
//! is the archive analogue of the "on-disk I/O is read-only on the view path"
//! guarantee — opening a ZIP to view it never extracts it to a temp directory.
//!
//! ## Random access vs. eager sources
//! [`FsSource`], [`ZipSource`], and a plain-tar [`TarSource`] are *lazy*
//! random-access: any item is fetched cheaply and independently, which is what
//! the direction-biased prefetch ring assumes. A *solid* archive (7z, compressed
//! tar) can't seek to one entry without decompressing the block before it, so
//! those sources are instead *eager*: they pay the whole decompression once on
//! open and then serve random access from RAM. The trait is shaped so this is an
//! implementation choice, not a change to the seam — the only caller-visible cost
//! is a slower open (which the app runs off-thread behind a progress dialog) and
//! the resident memory.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sevenz_rust2::{
    Archive as SevenZArchive, BlockDecoder, EncoderMethod, Password as SevenZPassword,
};
use zip::ZipArchive;

mod kind;
mod rar;
mod rar_crypt;
mod tar_source;

pub use kind::{archive_kind, ArchiveKind};
#[cfg(feature = "fuzz-internals")]
pub use rar::fuzz as rar_fuzz;
pub use rar::RarSource;
#[cfg(feature = "fuzz-internals")]
pub use tar_source::fuzz;
pub use tar_source::TarSource;

/// A uniform, read-only source of encoded bytes addressed by item index.
///
/// **Items, not photos** (the trait was `PhotoSource` until task #101). An item is an
/// image *or* a video: task #79 made videos first-class, and an archive-hosted clip has
/// no path, so it plays from [`bytes`] through the `VideoInput::Bytes` seam exactly as an
/// image decodes from them. Nothing here is necessarily a *file* either — a ZIP/7z entry
/// is bytes in RAM, which is why [`path`] returns `Option`. Abstracting over both is the
/// whole point of the seam, so the name stays neutral.
///
/// Implementations must be `Send + Sync`: the decode pool calls [`bytes`] from
/// many worker threads concurrently.
///
/// [`bytes`]: ItemSource::bytes
/// [`path`]: ItemSource::path
pub trait ItemSource: Send + Sync {
    /// Number of items (images and videos) in this source.
    fn len(&self) -> usize;

    /// Whether the source has no items.
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
    /// [`path`]: ItemSource::path
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

    /// The (uncompressed) byte size of item `i`, when the source knows it without
    /// I/O: ZIP reads it from the central directory, 7z from its resident bytes.
    /// `None` for a filesystem listing (callers stat the [`path`] instead) or
    /// out-of-range `i`. Feeds the info panel's file-size row for archive entries
    /// that are never read whole (videos), and pre-flight size checks.
    ///
    /// [`path`]: ItemSource::path
    fn size_hint(&self, i: usize) -> Option<u64> {
        let _ = i;
        None
    }

    /// Names of the files sitting **beside** item `i` — including the ones that are not
    /// library items (task #90.1).
    ///
    /// This exists because a sidecar subtitle is invisible to every other method here: a
    /// `.srt` is neither an image nor a video container, so the scan/archive predicates
    /// never index it, so it has no item index, so `len()`/`name(i)`/`bytes(i)` cannot
    /// reach it. This is the only door to it.
    ///
    /// Returns names in the **same shape as [`name`](ItemSource::name)** for this source:
    /// bare file names for a filesystem listing, archive-relative names for an archive.
    /// Scoped to item `i`'s own directory — a sidecar is a *sibling*, not any file in the
    /// archive. The video itself and other library items may be included; callers filter.
    ///
    /// Deliberately unfiltered and uncached: it is called **off the event loop**, once per
    /// video item, and the result is memoized by the caller. A `read_dir` of a 50k-photo
    /// folder is the worst case and it costs a worker tens of milliseconds, once.
    ///
    /// Default: none — for a source with no notion of siblings.
    fn sibling_names(&self, i: usize) -> Vec<String> {
        let _ = i;
        Vec::new()
    }

    /// Read a sibling that [`sibling_names`](ItemSource::sibling_names) returned for item
    /// `i`. Read-only and RAM-only, like every other read here (privacy #2).
    ///
    /// `name` must be one the source itself produced; anything else is `NotFound`. The
    /// source resolves it — the caller never builds a path, so this cannot be walked out of
    /// the archive or the folder.
    fn sibling_bytes(&self, i: usize, name: &str) -> io::Result<Vec<u8>> {
        let _ = (i, name);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this source has no siblings",
        ))
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

impl ItemSource for FsSource {
    fn len(&self) -> usize {
        self.paths.len()
    }

    fn name(&self, i: usize) -> &str {
        self.names.get(i).map(String::as_str).unwrap_or("")
    }

    fn path(&self, i: usize) -> Option<&Path> {
        self.paths.get(i).map(PathBuf::as_path)
    }

    /// The file names in item `i`'s own directory. One `read_dir`; directories are
    /// skipped (a sidecar is a file).
    fn sibling_names(&self, i: usize) -> Vec<String> {
        let Some(dir) = self.paths.get(i).and_then(|p| p.parent()) else {
            return Vec::new();
        };
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        rd.filter_map(|e| {
            let e = e.ok()?;
            // `file_type` avoids a stat per entry on every platform that carries the
            // type in the directory record.
            if !e.file_type().ok()?.is_file() {
                return None;
            }
            e.file_name().to_str().map(str::to_string)
        })
        .collect()
    }

    /// Read a sibling by name, resolved **inside item `i`'s directory**.
    ///
    /// The name is rejected unless it is a bare file name: a caller-supplied
    /// `../../etc/passwd` must not be readable through a sidecar lookup, however it got
    /// here.
    fn sibling_bytes(&self, i: usize, name: &str) -> io::Result<Vec<u8>> {
        let dir = self
            .paths
            .get(i)
            .and_then(|p| p.parent())
            .ok_or_else(out_of_range)?;
        if Path::new(name).components().count() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a sidecar name must be a bare file name",
            ));
        }
        std::fs::read(dir.join(name))
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
    /// The bytes are not a valid archive, are truncated, or use an unsupported
    /// compression method (carries the underlying reason).
    Corrupt(String),
    /// The archive (or its header) is encrypted and needs a password we don't have.
    /// Surfaced distinctly so the app can say "password protected" instead of
    /// misreporting it as corrupt. (ZIP exposes this via
    /// [`ZipSource::needs_password`] instead, since its directory is readable
    /// without one.)
    PasswordRequired,
    /// Decompressing the archive into RAM ran out of memory (the eager 7z load —
    /// a recoverable `try_reserve` failure, surfaced instead of an abort).
    OutOfMemory,
    /// The open was cancelled (via [`OpenProgress::request_cancel`]) before it
    /// finished. Distinct from an error so the app can quietly drop it rather than
    /// show a failure dialog.
    Cancelled,
    /// The archive's decompressed entries exceed the caller's RAM budget. Unlike
    /// 7z (whose header lists every size, so the app pre-flights and refuses
    /// before opening), a compressed tar's sizes are only discovered mid-stream —
    /// so the streaming open enforces the budget itself and reports it here.
    /// `needed` is a lower bound (the stream stopped as soon as it tripped).
    TooLarge { needed: u64, budget: u64 },
    /// The archive is recognized but deliberately not opened: a format tier we
    /// don't decode (RAR4, multi-volume RAR, encrypted RAR). Distinct from
    /// [`Corrupt`](OpenError::Corrupt) so the user learns *what* it is rather
    /// than "may be damaged" — and distinct from
    /// [`PasswordRequired`](OpenError::PasswordRequired), which the app answers
    /// with a password prompt: an encrypted RAR can't be decrypted no matter
    /// what is typed, so prompting would loop. Carries the ready-to-show line.
    Unsupported(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Io(e) => write!(f, "{e}"),
            OpenError::Corrupt(why) => write!(f, "the archive could not be opened: {why}"),
            OpenError::PasswordRequired => write!(f, "the archive is password protected"),
            OpenError::OutOfMemory => write!(f, "ran out of memory loading the archive"),
            OpenError::Cancelled => write!(f, "the archive open was cancelled"),
            OpenError::TooLarge { needed, budget } => write!(
                f,
                "the archive needs at least {needed} bytes of RAM, over the {budget}-byte budget"
            ),
            OpenError::Unsupported(msg) => write!(f, "{msg}"),
        }
    }
}

/// A shared, thread-safe progress + cancellation handle for an eager archive open
/// ([`SevenZSource::open_with_progress`]).
///
/// The eager 7z open runs on a background thread; this is how the UI thread talks to
/// it without locking the decode. The opener publishes the total decompressed size
/// once it's known, bumps `done` as each entry streams in, and polls
/// [`is_cancelled`](OpenProgress::is_cancelled) between entries; the UI thread reads
/// [`fraction`](OpenProgress::fraction) each frame to draw the bar and calls
/// [`request_cancel`](OpenProgress::request_cancel) for the Cancel button / `Esc`.
/// Cheap to [`clone`](Clone) — it's an `Arc` — so both threads hold one.
#[derive(Clone, Default)]
pub struct OpenProgress {
    inner: Arc<ProgressInner>,
}

#[derive(Default)]
struct ProgressInner {
    /// Decompressed bytes streamed so far.
    done: AtomicU64,
    /// Total decompressed bytes the open will stream (0 until the opener sets it).
    total: AtomicU64,
    /// Set by the UI to ask the opener to stop at the next entry boundary.
    cancel: AtomicBool,
}

impl OpenProgress {
    /// A fresh handle (nothing done, no total yet, not cancelled).
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish the total decompressed bytes this open will stream. Called once by the
    /// opener after it reads the archive header.
    pub fn set_total(&self, total: u64) {
        self.inner.total.store(total, Ordering::Relaxed);
    }

    /// Total decompressed bytes to stream (0 until [`set_total`](OpenProgress::set_total)).
    pub fn total(&self) -> u64 {
        self.inner.total.load(Ordering::Relaxed)
    }

    /// Decompressed bytes streamed so far.
    pub fn done(&self) -> u64 {
        self.inner.done.load(Ordering::Relaxed)
    }

    /// Progress in `[0.0, 1.0]`; `0.0` until a non-zero total is known.
    pub fn fraction(&self) -> f32 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        (self.done() as f64 / total as f64).clamp(0.0, 1.0) as f32
    }

    /// Ask the opener to stop at the next entry boundary (the Cancel button / `Esc`).
    pub fn request_cancel(&self) {
        self.inner.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancel.load(Ordering::Relaxed)
    }

    /// Opener-side: record `n` more streamed bytes (decompressed for 7z; the tar
    /// openers count *compressed* bytes consumed instead, since a compressed
    /// tar's decompressed total isn't knowable up front — the fraction stays a
    /// plain ratio either way).
    pub(crate) fn add_done(&self, n: u64) {
        self.inner.done.fetch_add(n, Ordering::Relaxed);
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
    /// Declared uncompressed size (central directory) — the panel's size row and
    /// the video play path's pre-flight, without inflating the entry.
    size: u64,
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
            ZipArchive::new(BufReader::new(file)).map_err(|e| OpenError::Corrupt(e.to_string()))?;

        let mut entries = Vec::new();
        for i in 0..archive.len() {
            // `by_index_raw` reads only the directory metadata — no decryption, so
            // it works for encrypted entries too (we just want names + flags here).
            let entry = archive
                .by_index_raw(i)
                .map_err(|e| OpenError::Corrupt(e.to_string()))?;
            if entry.is_dir() {
                continue;
            }
            let name = normalize_entry_name(entry.name());
            if !is_supported(&ext_of(&name)) {
                continue;
            }
            // An entry that could never be read (over the per-entry RAM ceiling —
            // `bytes` would refuse it) is not indexed at all: an item that can
            // never render or play shouldn't occupy a playlist slot.
            if entry.size() > MAX_ENTRY_BYTES {
                continue;
            }
            entries.push(ZipEntry {
                zip_index: i,
                name,
                encrypted: entry.encrypted(),
                size: entry.size(),
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

    /// Validate the currently-held password by actually decrypting the first
    /// encrypted entry. [`open`](ZipSource::open) succeeds even with a **wrong**
    /// password — the central directory is readable without one — so an entry read
    /// is the only real check (unlike 7z, whose eager decode fails on open). The
    /// app calls this after re-opening with a user-entered password to tell a wrong
    /// password (re-prompt) from a successful unlock. Returns `true` when nothing is
    /// encrypted, or the first encrypted entry decrypts cleanly; `false` otherwise.
    pub fn password_ok(&self) -> bool {
        match self.entries.iter().position(|e| e.encrypted) {
            Some(idx) => self.bytes(idx).is_ok(),
            None => true,
        }
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

impl ItemSource for ZipSource {
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
            let file = match &self.password {
                Some(pw) => archive
                    .by_index_decrypt(entry.zip_index, pw)
                    .map_err(zip_to_io)?,
                None => archive.by_index(entry.zip_index).map_err(zip_to_io)?,
            };
            // Bound the allocation + read so a decompression bomb (or a bogus huge entry,
            // including one hit by encrypted-ZIP password validation) can't OOM us:
            // refuse an absurd declared size, reserve *recoverably* (`try_reserve`, not an
            // aborting `reserve`), and cap the read to the declared size so a lying entry
            // can't inflate past its claim.
            let size = file.size();
            if size > MAX_ENTRY_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "archive entry too large",
                ));
            }
            buf.try_reserve_exact(size as usize).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "archive entry too large to allocate",
                )
            })?;
            file.take(size).read_to_end(&mut buf)?;
        }
        self.checkin(archive);
        Ok(buf)
    }

    fn size_hint(&self, i: usize) -> Option<u64> {
        self.entries.get(i).map(|e| e.size)
    }

    /// Every entry name in item `i`'s archive directory — **including the ones this source
    /// never indexed** (a `.srt` is not a library item, so it is not in `entries`).
    ///
    /// Free of I/O: a pooled handle already holds the parsed central directory.
    fn sibling_names(&self, i: usize) -> Vec<String> {
        let Some(entry) = self.entries.get(i) else {
            return Vec::new();
        };
        let dir = zip_dir_of(&entry.name);
        let Ok(archive) = self.checkout() else {
            return Vec::new();
        };
        let names: Vec<String> = archive
            .file_names()
            .filter(|n| !n.ends_with('/') && zip_dir_of(n) == dir)
            .map(str::to_string)
            .collect();
        self.checkin(archive);
        names
    }

    /// Read a sibling entry by its exact archive name. Same bomb guards as
    /// [`bytes`](ItemSource::bytes) — a sidecar is untrusted input like anything else in
    /// the archive.
    fn sibling_bytes(&self, i: usize, name: &str) -> io::Result<Vec<u8>> {
        // Scope the lookup to item `i`'s own directory, so a name from elsewhere in the
        // archive can't be pulled in through a sidecar read.
        let entry = self.entries.get(i).ok_or_else(out_of_range)?;
        if zip_dir_of(name) != zip_dir_of(&entry.name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sidecar is not a sibling of the item",
            ));
        }
        let mut archive = self.checkout()?;
        let mut buf = Vec::new();
        {
            let file = match &self.password {
                Some(pw) => archive.by_name_decrypt(name, pw).map_err(zip_to_io)?,
                None => archive.by_name(name).map_err(zip_to_io)?,
            };
            let size = file.size();
            if size > MAX_ENTRY_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "archive entry too large",
                ));
            }
            buf.try_reserve_exact(size as usize).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "archive entry too large to allocate",
                )
            })?;
            file.take(size).read_to_end(&mut buf)?;
        }
        self.checkin(archive);
        Ok(buf)
    }
}

/// The forward-slashed directory part of an archive entry name (`""` at the root).
/// Archive names are always `/`-separated, whatever host wrote them. (Named for
/// its first adopter; TarSource scopes its siblings with it too.)
pub(crate) fn zip_dir_of(name: &str) -> &str {
    match name.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
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

/// One supported-image entry from a 7z, with its **decompressed** bytes held in
/// RAM (the eager model — see [`SevenZSource`]).
struct SevenZEntry {
    name: String,
    bytes: Vec<u8>,
}

/// A 7-Zip archive, **eagerly decompressed into RAM** on open.
///
/// Unlike ZIP, a 7z is usually *solid*: many files share one LZMA2 stream, so
/// there is no cheap per-entry random access — to read any file you must
/// decompress the block before it. So this decodes every supported-image entry
/// once on open (a single sequential pass) into an in-RAM `index → bytes` map,
/// after which [`bytes`](ItemSource::bytes) is an instant RAM copy and prefetch /
/// navigation behave normally.
///
/// **Cost:** peak RAM ≈ the sum of the decompressed image bytes, held for the
/// session. The caller is expected to pre-flight with [`seven_z_projected_bytes`]
/// against a memory budget and refuse oversized archives *before* calling
/// [`open`](SevenZSource::open) — Rust aborts (uncatchable) on a true allocation
/// failure, so prediction is the real defense; the `try_reserve` here is a backstop.
///
/// **Privacy:** RAM-only. Nothing is extracted to disk.
pub struct SevenZSource {
    path: PathBuf,
    entries: Vec<SevenZEntry>,
}

/// Sum the **uncompressed** sizes of the supported-image entries in the 7z at
/// `path`, reading only the header — no decompression. This is the cheap pre-flight
/// the app compares against a RAM budget before committing to [`SevenZSource::open`]
/// (which would hold all of it resident).
///
/// `password` is only needed when the archive's *header* is encrypted (the
/// "encrypt file names" option); a plain or content-only-encrypted 7z reads its
/// header without one. Pass the same password you'll hand [`SevenZSource::open`] so
/// the pre-flight succeeds (and reports [`OpenError::PasswordRequired`] for a wrong
/// or missing one) instead of looping.
pub fn seven_z_projected_bytes(
    path: &Path,
    password: Option<&str>,
    is_supported: impl Fn(&str) -> bool,
) -> Result<u64, OpenError> {
    let pw = match password {
        Some(p) => SevenZPassword::from(p),
        None => SevenZPassword::empty(),
    };
    // Buffer the file: header parsing (and, for an encrypted header, AES decryption)
    // issues many small reads; see the throughput note in `SevenZSource::open`.
    let mut file = BufReader::new(File::open(path)?);
    let archive = SevenZArchive::read(&mut file, &pw).map_err(seven_z_open_err)?;
    let total = archive
        .files
        .iter()
        .filter(|f| {
            // Mirror the open's index rule exactly: oversized supported entries are
            // drained, never held resident, so they don't count toward the budget.
            !f.is_directory()
                && f.has_stream()
                && is_supported(&ext_of(f.name()))
                && f.size() <= MAX_ENTRY_BYTES
        })
        .map(|f| f.size())
        .sum();
    Ok(total)
}

impl SevenZSource {
    /// Open `path` as a 7z and eagerly decompress its supported-image entries into
    /// RAM, ordered by name. `is_supported` is the extension predicate (the app
    /// passes `pb_decode::is_supported_extension`); `password` decrypts an encrypted
    /// archive (`None` for a plain one).
    ///
    /// Output buffers use `try_reserve`, so an allocation shortfall returns
    /// [`OpenError::OutOfMemory`] instead of aborting the process. Pair this with
    /// [`seven_z_projected_bytes`] up front — once decompression starts, the
    /// `try_reserve` backstop can't cover the decoder's own internal allocations.
    ///
    /// This convenience passes `mt_headroom = 0` (never any within-block
    /// multithreading); see [`open_with_progress`](SevenZSource::open_with_progress)
    /// for the solid-archive speedup and what the headroom buys.
    pub fn open(
        path: impl Into<PathBuf>,
        password: Option<String>,
        is_supported: impl Fn(&str) -> bool + Sync,
    ) -> Result<Self, OpenError> {
        Self::open_with_progress(path, password, is_supported, None, 0)
    }

    /// Like [`open`](SevenZSource::open), but reports streaming progress and honors
    /// cancellation through a shared [`OpenProgress`] handle.
    ///
    /// The opener publishes the archive's total decompressed size up front (so the UI
    /// can draw a determinate bar), bumps the byte counter as each block decodes, and
    /// checks for cancellation between blocks — returning [`OpenError::Cancelled`] if the
    /// UI asked to stop. Cancel latency is at most one block per worker thread (a single
    /// image for a non-solid archive). Pass `None` for a plain, non-cancellable open.
    ///
    /// **Parallel, two levels.** A 7z block is self-contained and seekable, so
    /// independent blocks are decoded concurrently across the cores (each worker with
    /// its own file handle) — the way 7-Zip stays fast. This is decisive for a
    /// *non-solid* archive (one block per image), where every block also pays a full
    /// AES key derivation: ~28 ms × N images serially (minutes to an hour) collapses
    /// to seconds fanned across the cores (measured ~19× on a 3036-image encrypted
    /// archive). A *solid* archive is the opposite shape — one giant LZMA2 block —
    /// so the across-blocks fan-out has nothing to chew on and the open would run on
    /// a single core (measured ~320 MB/s; ~25 s for a 10 GB archive). For that case
    /// the leftover cores go *inside* the block: `lzma-rust2`'s multithreaded reader
    /// decodes the independent (dictionary-reset) chunks that 7-Zip's own
    /// multithreaded compressor emits (measured ~3×, plateauing at
    /// [`MT_INNER_CAP`] workers).
    ///
    /// **`mt_headroom` is the RAM gate for that within-block multithreading.** The MT
    /// reader holds decoded-ahead chunks in flight, and — the worst case — a stream
    /// *without* reset chunks (a single-threaded compressor wrote it) degenerates to
    /// buffering one whole block, compressed + decompressed, before entries stream
    /// out. A true allocation failure aborts uncatchably in Rust, so prediction is
    /// the only real defense (same reasoning as [`seven_z_projected_bytes`]): within-
    /// block MT is enabled only when that whole-block worst case fits under
    /// `mt_headroom` (transient bytes the caller can spare *beyond* the resident
    /// entries). `0` always keeps the proven one-thread-per-block path.
    pub fn open_with_progress(
        path: impl Into<PathBuf>,
        password: Option<String>,
        is_supported: impl Fn(&str) -> bool + Sync,
        progress: Option<&OpenProgress>,
        mt_headroom: u64,
    ) -> Result<Self, OpenError> {
        let path = path.into();
        let pw = match &password {
            Some(p) => SevenZPassword::from(p.as_str()),
            None => SevenZPassword::empty(),
        };

        // Read the header once (block layout + file metadata). Buffered, because
        // `sevenz_rust2`'s AES decoder reads its input in 512-byte chunks straight off the
        // file — unbuffered that's millions of tiny syscalls on an encrypted stream
        // (measured ~3.7x slower even from cache; catastrophic uncached on a multi-GB
        // archive). See `examples/aes_throughput.rs`.
        let archive = {
            let mut header = BufReader::with_capacity(1 << 20, File::open(&path)?);
            SevenZArchive::read(&mut header, &pw).map_err(seven_z_open_err)?
        };
        let block_count = archive.blocks.len();

        // Pre-scan (metadata only — no decode): which blocks hold a supported image, and
        // the total decompressed work in just those blocks. We skip decoding *and
        // decrypting* a block with no images (e.g. a mixed archive's video/doc blocks),
        // and the progress total counts only the work we'll actually do. `entries()`
        // reads the archive's stream map, not the file, so this stays cheap.
        let mut image_block = vec![false; block_count];
        let mut total = 0u64;
        {
            let mut probe = BufReader::with_capacity(1 << 20, File::open(&path)?);
            for (bi, has) in image_block.iter_mut().enumerate() {
                let dec = BlockDecoder::new(1, bi, &archive, &pw, &mut probe);
                let entries = dec.entries();
                let streamed = || {
                    entries
                        .iter()
                        .filter(|e| !e.is_directory() && e.has_stream())
                };
                // A block earns a decode only if it holds a supported entry that will
                // actually be kept — an oversized one (over the per-entry ceiling) is
                // drained, never indexed, so a block of nothing else stays skipped.
                if streamed()
                    .any(|e| is_supported(&ext_of(e.name())) && e.size() <= MAX_ENTRY_BYTES)
                {
                    *has = true;
                    total += streamed().map(|e| e.size()).sum::<u64>();
                }
            }
        }
        if let Some(p) = progress {
            p.set_total(total);
        }

        // Within-block thread count (see the `mt_headroom` docs above). Worth >1 only
        // when the image blocks are too few to saturate the cores (the solid-archive
        // shape) AND the caller's headroom covers the MT reader's worst-case transient
        // (each candidate block held compressed + decompressed). Non-LZMA2 blocks
        // ignore the count (only LZMA2 has an MT decoder), so they're excluded from
        // the worst-case sum.
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let image_blocks = image_block.iter().filter(|b| **b).count();
        let mt_worst: u64 = (0..block_count)
            .filter(|&bi| image_block[bi] && block_is_lzma2(&archive, bi))
            .map(|bi| block_pack_size(&archive, bi) + archive.blocks[bi].get_unpack_size())
            .sum();
        let inner: u32 = if image_blocks > 0
            && cores >= image_blocks * 2
            && mt_worst > 0
            && mt_worst <= mt_headroom
        {
            (cores / image_blocks).min(MT_INNER_CAP) as u32
        } else {
            1
        };

        // Decode blocks in parallel: a shared atomic cursor hands each worker the next
        // block; each worker has its own buffered file handle (the `BlockDecoder` seeks
        // per block and needs `&mut`). Outer workers shrink as inner threads grow so
        // the total stays at the core count.
        let threads = (cores / inner as usize).clamp(1, block_count.max(1));
        let next = AtomicUsize::new(0);
        let oom = AtomicBool::new(false);
        let cancel_hit = AtomicBool::new(false);
        let first_err: Mutex<Option<OpenError>> = Mutex::new(None);

        let collected: Vec<Vec<SevenZEntry>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    s.spawn(|| {
                        let mut local: Vec<SevenZEntry> = Vec::new();
                        let mut f = match File::open(&path) {
                            Ok(f) => BufReader::with_capacity(1 << 20, f),
                            Err(e) => {
                                first_err.lock().unwrap().get_or_insert(OpenError::Io(e));
                                return local;
                            }
                        };
                        loop {
                            if oom.load(Ordering::Relaxed) || cancel_hit.load(Ordering::Relaxed) {
                                break;
                            }
                            if progress.is_some_and(|p| p.is_cancelled()) {
                                cancel_hit.store(true, Ordering::Relaxed);
                                break;
                            }
                            let bi = next.fetch_add(1, Ordering::Relaxed);
                            if bi >= block_count {
                                break;
                            }
                            // Skip a block with no supported images — never decode or
                            // decrypt it (the pre-scan already excluded its bytes from the
                            // progress total).
                            if !image_block[bi] {
                                continue;
                            }
                            let cancel = || progress.is_some_and(|p| p.is_cancelled());
                            let res = BlockDecoder::new(inner, bi, &archive, &pw, &mut f)
                                .for_each_entries(&mut |entry, rd| {
                                    if entry.is_directory() || !entry.has_stream() {
                                        return Ok(true);
                                    }
                                    let size = entry.size();
                                    // Skip (never index) an unsupported entry — or a supported
                                    // one over the per-entry RAM ceiling (a decompression bomb,
                                    // or a video too large to ever hold resident): drain it in
                                    // cancellable chunks so the rest of a solid block stays
                                    // byte-aligned, bounded memory throughout. Skipping (not
                                    // refusing the whole archive) keeps the rest viewable; the
                                    // projection (`seven_z_projected_bytes`) excludes these too.
                                    if !is_supported(&ext_of(entry.name()))
                                        || size > MAX_ENTRY_BYTES
                                    {
                                        if !read_cancellable(rd, size, &mut io::sink(), &cancel)? {
                                            cancel_hit.store(true, Ordering::Relaxed);
                                            return Ok(false);
                                        }
                                        if let Some(p) = progress {
                                            p.add_done(size);
                                        }
                                        return Ok(true);
                                    }
                                    let mut buf = Vec::new();
                                    if buf.try_reserve_exact(size as usize).is_err() {
                                        oom.store(true, Ordering::Relaxed);
                                        return Ok(false);
                                    }
                                    if !read_cancellable(rd, size, &mut buf, &cancel)? {
                                        cancel_hit.store(true, Ordering::Relaxed);
                                        return Ok(false);
                                    }
                                    local.push(SevenZEntry {
                                        name: normalize_entry_name(entry.name()),
                                        bytes: buf,
                                    });
                                    if let Some(p) = progress {
                                        p.add_done(size);
                                    }
                                    Ok(true)
                                });
                            if let Err(e) = res {
                                first_err.lock().unwrap().get_or_insert(seven_z_open_err(e));
                                break;
                            }
                        }
                        local
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_default())
                .collect()
        });

        if let Some(e) = first_err.into_inner().unwrap() {
            return Err(e);
        }
        if cancel_hit.load(Ordering::Relaxed) {
            return Err(OpenError::Cancelled);
        }
        if oom.load(Ordering::Relaxed) {
            return Err(OpenError::OutOfMemory);
        }
        let mut entries: Vec<SevenZEntry> = collected.into_iter().flatten().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { path, entries })
    }
}

impl ItemSource for SevenZSource {
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
        // Already decompressed and resident — just hand back a copy.
        let entry = self.entries.get(i).ok_or_else(out_of_range)?;
        Ok(entry.bytes.clone())
    }

    fn size_hint(&self, i: usize) -> Option<u64> {
        self.entries.get(i).map(|e| e.bytes.len() as u64)
    }

    // `sibling_names`/`sibling_bytes` deliberately keep the default (no siblings), so a
    // sidecar subtitle inside a 7z is not found (task #90.1).
    //
    // This is a real gap, taken knowingly. 7z is **solid + eager**: `open` decompresses
    // exactly the entries its predicate accepted, and a `.srt` is not one of them, so its
    // bytes were never produced. Serving it would need either a second predicate at open
    // (widening a signature every caller passes, to eagerly hold files nothing may ask
    // for) or a **full re-decompress of the whole solid archive per sidecar read**. ZIP
    // has neither problem — its central directory is already parsed in RAM and entries are
    // independently readable — which is why ZIP is supported and this is not.
    //
    // The cost/benefit says stop: a 7z containing a video *and* its subtitle file is a
    // niche of a niche. The trait default makes this a small follow-up if that turns out
    // to be wrong.
}

/// A prefix-scoped view of another source — the deck for one internal archive
/// folder (the ⇧F tree's archive navigation). Wraps the **already-open** source
/// rather than re-opening the file, so re-scoping a solid 7z never re-pays its
/// eager decompression and an unlocked encrypted archive stays unlocked. Serves
/// the subset of entries whose folder chain sits at or under `prefix`,
/// boundary-aware (`a/b` matches `a/b/x.jpg` and `a/b/c/y.jpg`, never
/// `a/bc/z.jpg`). Names stay full archive-relative paths, so folder grouping,
/// the title, and the info panel read unchanged. RAM-only, like everything here.
pub struct ScopedSource {
    inner: Arc<dyn ItemSource>,
    index: Vec<usize>,
}

impl ScopedSource {
    /// Scope `inner` to the entries under the forward-slashed folder `prefix`
    /// (`""` = everything). One pass over the in-RAM name list — no I/O.
    pub fn new(inner: Arc<dyn ItemSource>, prefix: &str) -> Self {
        let index = (0..inner.len())
            .filter(|&i| name_in_scope(inner.name(i), prefix))
            .collect();
        Self { inner, index }
    }
}

impl ItemSource for ScopedSource {
    fn len(&self) -> usize {
        self.index.len()
    }

    fn name(&self, i: usize) -> &str {
        self.index.get(i).map_or("", |&j| self.inner.name(j))
    }

    fn path(&self, i: usize) -> Option<&Path> {
        self.inner.path(*self.index.get(i)?)
    }

    fn container(&self) -> Option<&Path> {
        self.inner.container()
    }

    fn bytes(&self, i: usize) -> io::Result<Vec<u8>> {
        let &j = self.index.get(i).ok_or_else(out_of_range)?;
        self.inner.bytes(j)
    }

    fn random_access(&self) -> bool {
        self.inner.random_access()
    }

    fn size_hint(&self, i: usize) -> Option<u64> {
        self.inner.size_hint(*self.index.get(i)?)
    }

    /// Delegate through the index map. The **scope must not filter these**: a sidecar is
    /// not a library item, so it was never in `index` to begin with — and the inner source
    /// already scopes siblings to the item's own directory, which is at or under this
    /// scope's prefix by construction.
    fn sibling_names(&self, i: usize) -> Vec<String> {
        match self.index.get(i) {
            Some(&j) => self.inner.sibling_names(j),
            None => Vec::new(),
        }
    }

    fn sibling_bytes(&self, i: usize, name: &str) -> io::Result<Vec<u8>> {
        let &j = self.index.get(i).ok_or_else(out_of_range)?;
        self.inner.sibling_bytes(j, name)
    }
}

/// Whether entry `name` lives at or under the folder `prefix` — i.e. `name` is
/// `prefix` + `/` + anything. The required separator is what makes the match
/// boundary-aware (and excludes a *file* named exactly `prefix`).
fn name_in_scope(name: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    name.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Ceiling on within-block decoder threads. Measured on a 2 GB solid LZMA2 photo
/// archive (32-core machine): 4 workers → 2.6×, 8 → 2.9×, 16 → 3.0×, 32 → no
/// further gain — the stream's independent-chunk count, not the cores, is the
/// limit. More workers past the plateau only hold more chunks in RAM.
const MT_INNER_CAP: usize = 16;

/// Whether block `bi`'s coder chain includes LZMA2 — the only codec with a
/// multithreaded decoder (`BlockDecoder`'s thread count is ignored by the rest).
fn block_is_lzma2(archive: &SevenZArchive, bi: usize) -> bool {
    archive.blocks[bi]
        .coders
        .iter()
        .any(|c| c.encoder_method_id() == EncoderMethod::ID_LZMA2)
}

/// Compressed (packed) bytes of block `bi` — the sum of its pack streams.
fn block_pack_size(archive: &SevenZArchive, bi: usize) -> u64 {
    let firsts = archive.stream_map.block_first_pack_stream_index();
    let sizes = archive.pack_sizes();
    let start = firsts[bi];
    let end = firsts.get(bi + 1).copied().unwrap_or(sizes.len());
    sizes[start..end].iter().sum()
}

/// Skip a single archived entry larger than this — a sanity ceiling against a
/// decompression bomb or a bogus entry that would otherwise reserve/read gigabytes
/// before the decoder ever sees it. Generous: real encoded photos are far smaller
/// (RAW ~100 MB; even a huge TIFF/PSD rarely approaches this), and it leaves real
/// headroom for archived *videos*, which play from RAM (the whole container must be
/// resident). Oversized entries are never indexed — the item could never render —
/// so a genuinely larger file must be extracted and opened directly.
pub const MAX_ENTRY_BYTES: u64 = 1 << 30; // 1 GiB

/// Read up to `size` bytes from `rd` into `out` in chunks, checking `cancel` between
/// chunks so a large entry aborts within one chunk's latency rather than only at the
/// next block boundary (which, for a solid archive, can mean after the whole thing).
/// `Ok(true)` = read to `size` (or a short EOF); `Ok(false)` = cancelled mid-stream.
pub(crate) fn read_cancellable(
    rd: &mut dyn Read,
    size: u64,
    out: &mut dyn io::Write,
    cancel: &dyn Fn() -> bool,
) -> io::Result<bool> {
    let mut staging = [0u8; 64 * 1024];
    let mut remaining = size;
    while remaining > 0 {
        if cancel() {
            return Ok(false);
        }
        let want = remaining.min(staging.len() as u64) as usize;
        let n = rd.read(&mut staging[..want])?;
        if n == 0 {
            break; // short stream (EOF before `size`)
        }
        out.write_all(&staging[..n])?;
        remaining -= n as u64;
    }
    Ok(true)
}

/// The display name of a path: its final component, or `"?"` if it has none.
fn display_name(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string()
}

/// Normalize a stored archive entry name for display and folder grouping:
/// legacy Windows archivers wrote backslash separators (non-spec, but common in
/// old zips), and `zip .`-style invocations prefix `./` — both defeat the
/// `/`-based grouping (the ⇧F folder tree) and read wrong in the title/EXIF
/// panel. Purely a *name* transformation: entries are only ever read from RAM
/// by index, never extracted to a path, so this can't affect what's opened.
pub(crate) fn normalize_entry_name(raw: &str) -> String {
    let s = raw.replace('\\', "/");
    let mut t = s.as_str();
    loop {
        if let Some(rest) = t.strip_prefix("./") {
            t = rest;
        } else if let Some(rest) = t.strip_prefix('/') {
            t = rest;
        } else {
            break;
        }
    }
    t.to_string()
}

/// The lowercased extension (no dot) of a name or path, or `""` if none. Uses
/// `Path` so only the last component's extension counts (a ZIP entry name like
/// `trip/day1/IMG.JPG` → `jpg`).
pub(crate) fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

pub(crate) fn out_of_range() -> io::Error {
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

/// Map a 7z error onto [`OpenError`], distinguishing the "needs a password" cases
/// (an encrypted archive or encrypted header) from a genuinely unreadable one, so
/// the app shows the right message.
fn seven_z_open_err(e: sevenz_rust2::Error) -> OpenError {
    use sevenz_rust2::Error as E;
    match e {
        E::PasswordRequired | E::MaybeBadPassword(_) => OpenError::PasswordRequired,
        other => OpenError::Corrupt(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sevenz_rust2::{ArchiveEntry as SevenZArchiveEntry, ArchiveWriter as SevenZWriter};
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
    fn read_cancellable_completes_and_aborts_promptly() {
        let data = vec![7u8; 5000];

        // Never cancelled → reads exactly `size`.
        let mut out = Vec::new();
        assert!(read_cancellable(&mut &data[..], data.len() as u64, &mut out, &|| false).unwrap());
        assert_eq!(out, data);

        // Always cancelled → returns false before reading anything.
        let mut out = Vec::new();
        assert!(!read_cancellable(&mut &data[..], data.len() as u64, &mut out, &|| true).unwrap());
        assert!(out.is_empty());

        // Cancel after the first 64 KiB chunk → stops mid-stream, not at the end.
        let big = vec![1u8; 200 * 1024];
        let mut out = Vec::new();
        let calls = AtomicUsize::new(0);
        let done = read_cancellable(&mut &big[..], big.len() as u64, &mut out, &|| {
            calls.fetch_add(1, Ordering::Relaxed) >= 1
        })
        .unwrap();
        assert!(!done, "cancelled mid-stream");
        assert_eq!(
            out.len(),
            64 * 1024,
            "exactly one chunk read before the cancel"
        );
    }

    #[test]
    fn entry_names_normalize_legacy_separators() {
        // The pure helper: backslashes → '/', leading "./" and '/' stripped.
        assert_eq!(
            normalize_entry_name(r"dir\sub\photo.jpg"),
            "dir/sub/photo.jpg"
        );
        assert_eq!(normalize_entry_name("./dir/photo.jpg"), "dir/photo.jpg");
        assert_eq!(normalize_entry_name("././x.png"), "x.png");
        assert_eq!(normalize_entry_name("/abs/x.png"), "abs/x.png");
        assert_eq!(normalize_entry_name("plain.jpg"), "plain.jpg");

        // End to end: a legacy Windows-built zip with backslash entry names still
        // groups into folders (the ⇧F tree splits on '/') and reads correctly.
        let zip = write_zip(
            "legacy-seps",
            &[(r"dir\photo.jpg", b"A".as_slice()), ("./top.png", b"B")],
            None,
        );
        let src = ZipSource::open(&zip, None, is_img).unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["dir/photo.jpg", "top.png"]);
        assert_eq!(src.bytes(0).unwrap(), b"A", "reads still go by index");
    }

    #[test]
    fn scoped_source_serves_the_subtree_boundary_aware() {
        let zip = write_zip(
            "scope",
            &[
                ("a/b/one.jpg", b"1".as_slice()),
                ("a/b/c/two.jpg", b"2"),
                ("a/bc/three.jpg", b"3"), // `a/bc` must NOT match the scope `a/b`
                ("a/four.jpg", b"4"),
                ("top.png", b"5"),
            ],
            None,
        );
        let full: Arc<dyn ItemSource> = Arc::new(ZipSource::open(&zip, None, is_img).unwrap());

        let scoped = ScopedSource::new(Arc::clone(&full), "a/b");
        let names: Vec<&str> = (0..scoped.len()).map(|i| scoped.name(i)).collect();
        assert_eq!(
            names,
            vec!["a/b/c/two.jpg", "a/b/one.jpg"],
            "direct + nested entries, full archive-relative names, boundary-aware"
        );
        // Delegation: bytes by remapped index, container passthrough, no fs path.
        assert_eq!(scoped.bytes(1).unwrap(), b"1");
        assert_eq!(scoped.bytes(0).unwrap(), b"2");
        assert!(scoped.bytes(2).is_err(), "out-of-range read errors");
        assert_eq!(scoped.container(), Some(zip.as_path()));
        assert!(scoped.path(0).is_none());
        assert_eq!(scoped.name(99), "", "out-of-range name is empty");
        assert!(scoped.random_access());

        // The empty prefix is the whole archive; a bogus prefix is empty.
        assert_eq!(ScopedSource::new(Arc::clone(&full), "").len(), full.len());
        assert!(ScopedSource::new(Arc::clone(&full), "nope").is_empty());
        // A prefix that names a file (not a folder) matches nothing.
        assert!(ScopedSource::new(Arc::clone(&full), "top.png").is_empty());

        let _ = std::fs::remove_file(&zip);
    }

    /// Archive video playback (the predicate union): a video-extension entry is
    /// indexed when the caller's predicate admits it, `size_hint` reports the
    /// directory's uncompressed size without inflating anything, and `bytes`
    /// round-trips the container for the RAM-only playback path.
    #[test]
    fn zip_indexes_videos_under_a_union_predicate_with_size_hints() {
        let is_lib = |ext: &str| is_img(ext) || ext == "mp4" || ext == "mov";
        let zip = write_zip(
            "vids",
            &[
                ("clip.mp4", b"fake mp4 container bytes".as_slice()),
                ("photo.jpg", b"J"),
                ("notes.txt", b"text"),
            ],
            None,
        );
        let src = ZipSource::open(&zip, None, is_lib).unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(
            names,
            vec!["clip.mp4", "photo.jpg"],
            "video indexed, txt not"
        );
        assert_eq!(
            src.size_hint(0),
            Some(24),
            "central-directory size, no inflate"
        );
        assert_eq!(src.size_hint(1), Some(1));
        assert_eq!(src.size_hint(99), None, "out of range");
        assert_eq!(src.bytes(0).unwrap(), b"fake mp4 container bytes");

        // The images-only predicate still excludes videos (nothing regressed).
        let imgs_only = ZipSource::open(&zip, None, is_img).unwrap();
        assert_eq!(imgs_only.len(), 1);
        assert_eq!(imgs_only.name(0), "photo.jpg");

        let _ = std::fs::remove_file(&zip);
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
        assert!(src.password_ok(), "a plain archive validates trivially");
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

        // Correct password: content decrypts, and `password_ok` confirms it.
        let open = ZipSource::open(&zip, Some("hunter2".into()), is_img).unwrap();
        assert!(!open.needs_password());
        assert!(open.password_ok(), "correct password validates");
        assert_eq!(open.bytes(0).unwrap(), b"secret-A");
        assert_eq!(open.bytes(1).unwrap(), b"secret-B");

        // Wrong password: `open` still succeeds (the directory is readable), so
        // `password_ok` — not the open result — is what catches it; reads fail.
        let wrong = ZipSource::open(&zip, Some("nope".into()), is_img).unwrap();
        assert!(!wrong.password_ok(), "wrong password fails validation");
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
            Err(OpenError::Corrupt(_)) => {}
            Err(other) => panic!("expected Corrupt, got {other:?}"),
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

    /// Build a .7z in a temp file from (name, bytes) pairs; return its path.
    fn write_7z(tag: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let path = temp_path(tag, "7z");
        let mut sz = SevenZWriter::create(&path).expect("create 7z");
        for &(name, bytes) in files {
            sz.push_archive_entry(SevenZArchiveEntry::new_file(name), Some(bytes))
                .expect("push entry");
        }
        sz.finish().expect("finish 7z");
        path
    }

    /// Build an AES-encrypted .7z (solid AES+LZMA2 block) from (name, bytes) pairs;
    /// return its path. Uses the crate's proven high-level `compress_encrypted`
    /// helper (the only round-trip-verified write path) rather than hand-driving the
    /// low-level writer. It encrypts the header too, so even the listing needs the
    /// key — the strictest case for the open path to handle.
    fn write_encrypted_7z(tag: &str, files: &[(&str, &[u8])], password: &str) -> PathBuf {
        use sevenz_rust2::compress_encrypted;
        let n = NONCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pb_src_{tag}_src_{}_{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for &(name, bytes) in files {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        let path = temp_path(tag, "7z");
        let dest = File::create(&path).unwrap();
        compress_encrypted(&dir, dest, SevenZPassword::from(password)).expect("compress encrypted");
        let _ = std::fs::remove_dir_all(&dir);
        path
    }

    /// Archive video playback, 7z half: a union predicate indexes the video into
    /// resident RAM (the eager model), `size_hint` reports its decompressed size,
    /// and the pre-flight projection counts it — projection ≡ what open keeps.
    #[test]
    fn seven_z_indexes_videos_under_a_union_predicate_and_projects_them() {
        let is_lib = |ext: &str| is_img(ext) || ext == "mp4";
        let z = write_7z(
            "vids",
            &[
                ("clip.mp4", b"fake mp4 container".as_slice()),
                ("photo.jpg", b"JJ"),
                ("notes.txt", b"text"),
            ],
        );
        let projected = seven_z_projected_bytes(&z, None, is_lib).unwrap();
        assert_eq!(projected, 18 + 2, "video + image, never the txt");
        let src = SevenZSource::open(&z, None, is_lib).unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["clip.mp4", "photo.jpg"]);
        assert_eq!(src.bytes(0).unwrap(), b"fake mp4 container");
        assert_eq!(src.size_hint(0), Some(18));
        let _ = std::fs::remove_file(&z);
    }

    #[test]
    fn seven_z_lists_supported_images_sorted_and_reads_bytes() {
        let z = write_7z(
            "list",
            &[
                ("b.png", b"BBBB"),
                ("a.jpg", b"AAAA"),
                ("notes.txt", b"text"),
                ("sub/c.webp", b"CCCC"),
            ],
        );
        let src = SevenZSource::open(&z, None, is_img).unwrap();
        assert_eq!(src.len(), 3, "the .txt is excluded");
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["a.jpg", "b.png", "sub/c.webp"]);
        // Eagerly decompressed: bytes match, in sorted order.
        assert_eq!(src.bytes(0).unwrap(), b"AAAA");
        assert_eq!(src.bytes(1).unwrap(), b"BBBB");
        assert_eq!(src.bytes(2).unwrap(), b"CCCC");
        assert!(src.bytes(99).is_err());
        assert_eq!(
            src.container(),
            Some(z.as_path()),
            "container is the archive"
        );
        let _ = std::fs::remove_file(&z);
    }

    #[test]
    fn seven_z_open_reports_progress_to_completion() {
        let z = write_7z(
            "prog",
            &[("a.jpg", b"AAAA"), ("b.png", b"BBBBBB"), ("c.webp", b"CC")],
        );
        let progress = OpenProgress::new();
        let src = SevenZSource::open_with_progress(&z, None, is_img, Some(&progress), 0).unwrap();
        assert_eq!(src.len(), 3);
        // Total = sum of every streamed entry; done reached it; fraction is full.
        assert_eq!(
            progress.total(),
            4 + 6 + 2,
            "total counts all streamed entries"
        );
        assert_eq!(
            progress.done(),
            progress.total(),
            "done reaches total on success"
        );
        assert_eq!(progress.fraction(), 1.0);
        let _ = std::fs::remove_file(&z);
    }

    #[test]
    fn seven_z_open_cancels_before_decoding() {
        let z = write_7z("cancel", &[("a.jpg", b"AAAA"), ("b.png", b"BBBB")]);
        // Cancel up front: the very first entry boundary sees it and bails out, so
        // nothing is decoded and the open reports Cancelled (not a corrupt/IO error).
        let progress = OpenProgress::new();
        progress.request_cancel();
        match SevenZSource::open_with_progress(&z, None, is_img, Some(&progress), 0) {
            Err(OpenError::Cancelled) => {}
            other => panic!("expected Cancelled, got {:?}", other.err()),
        }
        assert_eq!(progress.done(), 0, "cancelled before any entry decoded");
        let _ = std::fs::remove_file(&z);
    }

    #[test]
    fn seven_z_open_with_mt_headroom_reads_identically() {
        // The sevenz-rust2 writer emits NO dictionary-reset chunks, so a huge
        // headroom routes these through the MT reader's degenerate whole-block
        // fallback — the correctness (not speed) risk of the within-block MT path.
        // Entries big enough to span several 64 KiB read_cancellable chunks.
        let files: Vec<(String, Vec<u8>)> = (0..6)
            .map(|i| {
                let bytes = (0..200_000 + i)
                    .map(|j| (j % 251) as u8 ^ i as u8)
                    .collect();
                (format!("img{i}.jpg"), bytes)
            })
            .collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let z = write_7z("mt", &refs);

        let plain = SevenZSource::open(&z, None, is_img).unwrap();
        let progress = OpenProgress::new();
        let mt =
            SevenZSource::open_with_progress(&z, None, is_img, Some(&progress), u64::MAX).unwrap();
        assert_eq!(plain.len(), mt.len());
        for i in 0..plain.len() {
            assert_eq!(plain.name(i), mt.name(i));
            assert_eq!(plain.bytes(i).unwrap(), mt.bytes(i).unwrap(), "entry {i}");
        }
        assert_eq!(progress.done(), progress.total());
        let _ = std::fs::remove_file(&z);
    }

    #[test]
    fn seven_z_encrypted_open_with_mt_headroom_reads_identically() {
        // AES + LZMA2 coder chain with within-block MT enabled — the decrypt layer
        // feeds the MT LZMA2 reader; bytes must round-trip exactly.
        let files: Vec<(String, Vec<u8>)> = (0..4)
            .map(|i| {
                let bytes = (0..150_000 + i)
                    .map(|j| (j % 241) as u8 ^ i as u8)
                    .collect();
                (format!("img{i}.png"), bytes)
            })
            .collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let z = write_encrypted_7z("mt_enc", &refs, "hunter2");

        let plain = SevenZSource::open(&z, Some("hunter2".into()), is_img).unwrap();
        let mt =
            SevenZSource::open_with_progress(&z, Some("hunter2".into()), is_img, None, u64::MAX)
                .unwrap();
        assert_eq!(plain.len(), mt.len());
        for i in 0..plain.len() {
            assert_eq!(plain.bytes(i).unwrap(), mt.bytes(i).unwrap(), "entry {i}");
        }
        let _ = std::fs::remove_file(&z);
    }

    #[test]
    fn seven_z_projected_bytes_sums_image_sizes_without_decompressing() {
        // 4 + 4 + 4 image bytes (the 9-byte .txt is excluded).
        let z = write_7z(
            "proj",
            &[
                ("a.jpg", b"AAAA"),
                ("b.png", b"BBBB"),
                ("c.webp", b"CCCC"),
                ("readme.md", b"ignore me"),
            ],
        );
        let projected = seven_z_projected_bytes(&z, None, is_img).unwrap();
        assert_eq!(projected, 12, "sum of the three 4-byte images");
        let _ = std::fs::remove_file(&z);
    }

    #[test]
    fn seven_z_with_no_images_is_empty() {
        let z = write_7z("none", &[("readme.txt", b"x"), ("data.bin", b"y")]);
        let src = SevenZSource::open(&z, None, is_img).unwrap();
        assert!(src.is_empty());
        assert_eq!(seven_z_projected_bytes(&z, None, is_img).unwrap(), 0);
        let _ = std::fs::remove_file(&z);
    }

    #[test]
    fn seven_z_open_err_maps_password_cases() {
        use sevenz_rust2::Error as E;
        // Encrypted archive / encrypted header -> a distinct "password" error so the
        // app can say "password protected" rather than "corrupt".
        assert!(matches!(
            seven_z_open_err(E::PasswordRequired),
            OpenError::PasswordRequired
        ));
        let io = std::io::Error::new(std::io::ErrorKind::InvalidData, "bad pw");
        assert!(matches!(
            seven_z_open_err(E::MaybeBadPassword(io)),
            OpenError::PasswordRequired
        ));
        // Anything else is a generic unreadable/corrupt archive.
        assert!(matches!(
            seven_z_open_err(E::FileNotFound),
            OpenError::Corrupt(_)
        ));
    }

    #[test]
    fn seven_z_rejects_a_non_archive_file() {
        let bogus = temp_path("bogus7z", "7z");
        std::fs::write(&bogus, b"this is not a 7z archive at all").unwrap();
        match SevenZSource::open(&bogus, None, is_img) {
            Err(OpenError::Corrupt(_)) => {}
            Err(other) => panic!("expected Corrupt, got {other:?}"),
            Ok(_) => panic!("expected an error opening a non-7z file"),
        }
        let _ = std::fs::remove_file(&bogus);
    }

    #[test]
    fn seven_z_encrypted_needs_password_then_decrypts() {
        let z = write_encrypted_7z(
            "enc7z",
            &[("a.jpg", b"secret-A"), ("b.png", b"secret-B")],
            "hunter2",
        );

        // No password: unlike ZIP, a 7z is eager, so the missing key surfaces as a
        // distinct PasswordRequired right at open (not a deferred read failure).
        match SevenZSource::open(&z, None, is_img) {
            Err(OpenError::PasswordRequired) => {}
            other => panic!("expected PasswordRequired, got {:?}", other.err()),
        }

        // Correct password: content decrypts, in sorted order.
        let open = SevenZSource::open(&z, Some("hunter2".into()), is_img).unwrap();
        assert_eq!(open.len(), 2);
        assert_eq!(open.bytes(0).unwrap(), b"secret-A");
        assert_eq!(open.bytes(1).unwrap(), b"secret-B");

        // Wrong password: the decode of garbage fails -> MaybeBadPassword ->
        // PasswordRequired (so the app re-prompts rather than crying "corrupt").
        match SevenZSource::open(&z, Some("nope".into()), is_img) {
            Err(OpenError::PasswordRequired) => {}
            other => panic!(
                "expected PasswordRequired for wrong pw, got {:?}",
                other.err()
            ),
        }

        // The header is encrypted here, so the RAM pre-flight needs the password too
        // (proving the threaded-through password): right key sums the image bytes,
        // no key reports PasswordRequired rather than looping.
        assert_eq!(
            seven_z_projected_bytes(&z, Some("hunter2"), is_img).unwrap(),
            16
        );
        assert!(matches!(
            seven_z_projected_bytes(&z, None, is_img),
            Err(OpenError::PasswordRequired)
        ));

        let _ = std::fs::remove_file(&z);
    }

    // -- sidecar siblings (task #90.1) --------------------------------------

    /// The whole point: a `.srt` is NOT a library item, so it never appears in
    /// `len()`/`name(i)` — `sibling_names` is the only way to see it.
    #[test]
    fn fs_siblings_see_files_the_index_never_listed() {
        let dir = temp_path("sib", "d");
        std::fs::create_dir_all(&dir).unwrap();
        for (n, b) in [
            ("movie.mp4", &b"vid"[..]),
            (
                "movie.en.srt",
                &b"1\n00:00:01,000 --> 00:00:02,000\nHi\n"[..],
            ),
            ("notes.txt", &b"x"[..]),
        ] {
            std::fs::write(dir.join(n), b).unwrap();
        }
        std::fs::create_dir_all(dir.join("subdir")).unwrap();

        let src = FsSource::new(vec![dir.join("movie.mp4")]);
        assert_eq!(src.len(), 1, "only the video is a library item");

        let mut names = src.sibling_names(0);
        names.sort();
        assert_eq!(names, vec!["movie.en.srt", "movie.mp4", "notes.txt"]);
        assert!(
            !names.iter().any(|n| n == "subdir"),
            "directories are not siblings"
        );

        let bytes = src
            .sibling_bytes(0, "movie.en.srt")
            .expect("read the sidecar");
        assert!(String::from_utf8_lossy(&bytes).contains("Hi"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sidecar name must be a bare file name — a lookup must not be walkable out of the
    /// item's own directory, however the name got there.
    #[test]
    fn fs_sibling_reads_cannot_escape_the_items_directory() {
        let dir = temp_path("esc", "d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("movie.mp4"), b"v").unwrap();
        let src = FsSource::new(vec![dir.join("movie.mp4")]);

        for bad in ["../secret.srt", "sub/deep.srt", "/etc/passwd"] {
            let e = src.sibling_bytes(0, bad).expect_err(bad);
            assert!(
                matches!(
                    e.kind(),
                    io::ErrorKind::InvalidInput | io::ErrorKind::NotFound
                ),
                "{bad} -> {e:?}"
            );
        }
        assert!(
            src.sibling_bytes(9, "movie.mp4").is_err(),
            "out-of-range item"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The ZIP case the seam exists for: the `.srt` is not indexed, but the central
    /// directory (already in RAM on a pooled handle) still knows about it.
    #[test]
    fn zip_siblings_reach_unindexed_entries() {
        let z = write_zip(
            "sib",
            &[
                ("a.jpg", b"img"),
                ("movie.mkv", b"vid"),
                ("movie.en.srt", b"hello subs"),
                ("readme.txt", b"x"),
            ],
            None,
        );
        // Index images only — so neither the mkv nor the srt is a library item.
        let src = ZipSource::open(&z, None, is_img).unwrap();
        assert_eq!(src.len(), 1);
        assert_eq!(src.name(0), "a.jpg");

        let mut names = src.sibling_names(0);
        names.sort();
        assert_eq!(
            names,
            vec!["a.jpg", "movie.en.srt", "movie.mkv", "readme.txt"],
            "siblings include entries the index rejected"
        );
        let bytes = src.sibling_bytes(0, "movie.en.srt").expect("read");
        assert_eq!(bytes, b"hello subs");
        let _ = std::fs::remove_file(&z);
    }

    /// Siblings are scoped to the item's own archive folder, both ways: the listing only
    /// shows that folder, and a read of an entry elsewhere is refused.
    #[test]
    fn zip_siblings_are_scoped_to_the_items_folder() {
        let z = write_zip(
            "scope",
            &[
                ("S1/movie.mkv", b"vid"),
                ("S1/movie.en.srt", b"s1 subs"),
                ("S1/a.jpg", b"img"),
                ("S2/other.en.srt", b"s2 subs"),
                ("root.jpg", b"img"),
            ],
            None,
        );
        let src = ZipSource::open(&z, None, is_img).unwrap();
        let s1 = (0..src.len()).find(|&i| src.name(i) == "S1/a.jpg").unwrap();

        let mut names = src.sibling_names(s1);
        names.sort();
        assert_eq!(names, vec!["S1/a.jpg", "S1/movie.en.srt", "S1/movie.mkv"]);
        assert!(
            !names.iter().any(|n| n.starts_with("S2/")),
            "another folder is not a sibling"
        );
        assert!(
            !names.iter().any(|n| n == "root.jpg"),
            "the root is not a sibling"
        );

        assert_eq!(
            src.sibling_bytes(s1, "S1/movie.en.srt").unwrap(),
            b"s1 subs"
        );
        assert!(
            src.sibling_bytes(s1, "S2/other.en.srt").is_err(),
            "a read outside the item's folder is refused"
        );
        let _ = std::fs::remove_file(&z);
    }

    /// 7z deliberately reports none — it is solid + eager, so an unindexed entry's bytes
    /// were never produced. Pinned so the gap is a decision, not a surprise.
    #[test]
    fn sevenz_reports_no_siblings_by_design() {
        let path = temp_path("sib7z", "7z");
        {
            let mut w = SevenZWriter::create(&path).unwrap();
            w.push_archive_entry(SevenZArchiveEntry::new_file("a.jpg"), Some(&b"img"[..]))
                .unwrap();
            w.push_archive_entry(SevenZArchiveEntry::new_file("a.en.srt"), Some(&b"subs"[..]))
                .unwrap();
            w.finish().unwrap();
        }
        let src = SevenZSource::open(&path, None, is_img).unwrap();
        assert_eq!(src.len(), 1);
        assert!(src.sibling_names(0).is_empty());
        assert!(src.sibling_bytes(0, "a.en.srt").is_err());
        let _ = std::fs::remove_file(&path);
    }

    /// A scoped view must not hide siblings — it maps the index and delegates.
    #[test]
    fn scoped_source_delegates_siblings() {
        let z = write_zip(
            "scoped",
            &[
                ("S1/a.jpg", b"img"),
                ("S1/movie.en.srt", b"subs"),
                ("S2/b.jpg", b"img"),
            ],
            None,
        );
        let inner: Arc<dyn ItemSource> = Arc::new(ZipSource::open(&z, None, is_img).unwrap());
        let scoped = ScopedSource::new(inner, "S1");
        assert_eq!(scoped.len(), 1);
        assert!(scoped
            .sibling_names(0)
            .iter()
            .any(|n| n == "S1/movie.en.srt"));
        assert_eq!(scoped.sibling_bytes(0, "S1/movie.en.srt").unwrap(), b"subs");
        assert!(
            scoped.sibling_names(9).is_empty(),
            "out-of-range is empty, not a panic"
        );
        let _ = std::fs::remove_file(&z);
    }
}
