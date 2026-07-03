//! `pb-source` — where a playlist's image bytes come from.
//!
//! `pb-core` navigates purely by index; it never knows whether item `i` is a file
//! on disk or an entry inside an archive. This crate is the seam that turns an
//! index into the **raw encoded bytes** to hand `pb_decode::decode_named_bytes`,
//! plus a display name (and, for real files, a path the info panel can stat).
//!
//! Three implementations ship today:
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
//!
//! **Privacy.** Every read here is RAM-only; nothing is ever written to disk. This
//! is the archive analogue of the "on-disk I/O is read-only on the view path"
//! guarantee — opening a ZIP to view it never extracts it to a temp directory.
//!
//! ## Random access vs. eager sources
//! [`FsSource`] and [`ZipSource`] are *lazy* random-access: any item is fetched
//! cheaply and independently, which is what the direction-biased prefetch ring
//! assumes. A *solid* archive (7z, and later tar.gz) can't seek to one entry
//! without decompressing the block before it, so [`SevenZSource`] is instead
//! *eager*: it pays the whole decompression once on open and then serves random
//! access from RAM. The trait is shaped so this is an implementation choice, not a
//! change to the seam — the only caller-visible cost is a slower open (which the
//! app runs off-thread behind a spinner) and the resident memory.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use sevenz_rust2::{Archive as SevenZArchive, BlockDecoder, Password as SevenZPassword};
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
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Io(e) => write!(f, "{e}"),
            OpenError::Corrupt(why) => write!(f, "the archive could not be opened: {why}"),
            OpenError::PasswordRequired => write!(f, "the archive is password protected"),
            OpenError::OutOfMemory => write!(f, "ran out of memory loading the archive"),
            OpenError::Cancelled => write!(f, "the archive open was cancelled"),
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

    /// Opener-side: record `n` more decompressed bytes streamed.
    fn add_done(&self, n: u64) {
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
/// after which [`bytes`](PhotoSource::bytes) is an instant RAM copy and prefetch /
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
        .filter(|f| !f.is_directory() && f.has_stream() && is_supported(&ext_of(f.name())))
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
    pub fn open(
        path: impl Into<PathBuf>,
        password: Option<String>,
        is_supported: impl Fn(&str) -> bool + Sync,
    ) -> Result<Self, OpenError> {
        Self::open_with_progress(path, password, is_supported, None)
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
    /// **Parallel.** A 7z block is self-contained and seekable, so independent blocks are
    /// decoded concurrently across the cores (each worker with its own file handle) — the
    /// way 7-Zip stays fast. This is decisive for a *non-solid* archive (one block per
    /// image), where every block also pays a full AES key derivation: ~28 ms × N images
    /// serially (minutes to an hour) collapses to seconds fanned across the cores
    /// (measured ~19× on a 3036-image encrypted archive). A *solid* archive is one big
    /// block — a single task, decoded on one thread (fastest for already-compressed
    /// photos, and memory-bounded).
    pub fn open_with_progress(
        path: impl Into<PathBuf>,
        password: Option<String>,
        is_supported: impl Fn(&str) -> bool + Sync,
        progress: Option<&OpenProgress>,
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
                if streamed().any(|e| is_supported(&ext_of(e.name()))) {
                    *has = true;
                    total += streamed().map(|e| e.size()).sum::<u64>();
                }
            }
        }
        if let Some(p) = progress {
            p.set_total(total);
        }

        // Decode blocks in parallel: a shared atomic cursor hands each worker the next
        // block; each worker has its own buffered file handle (the `BlockDecoder` seeks
        // per block and needs `&mut`). The within-block thread count is 1 — already
        // measured fastest for incompressible photos, and it keeps per-block memory small.
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(block_count.max(1));
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
                            let res = BlockDecoder::new(1, bi, &archive, &pw, &mut f)
                                .for_each_entries(&mut |entry, rd| {
                                    if entry.is_directory() || !entry.has_stream() {
                                        return Ok(true);
                                    }
                                    let size = entry.size();
                                    if !is_supported(&ext_of(entry.name())) {
                                        // Drain a non-image entry (in cancellable chunks) so
                                        // the rest of a solid block stays byte-aligned.
                                        if !read_cancellable(rd, size, &mut io::sink(), &cancel)? {
                                            cancel_hit.store(true, Ordering::Relaxed);
                                            return Ok(false);
                                        }
                                        if let Some(p) = progress {
                                            p.add_done(size);
                                        }
                                        return Ok(true);
                                    }
                                    // Refuse an absurd declared size (a decompression bomb),
                                    // reserve recoverably, and read in cancellable chunks so a
                                    // large entry can be aborted mid-stream (not only between
                                    // blocks — which for a solid archive could mean "never").
                                    if size > MAX_ENTRY_BYTES {
                                        oom.store(true, Ordering::Relaxed);
                                        return Ok(false);
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

impl PhotoSource for SevenZSource {
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
}

/// Refuse a single archived image larger than this — a sanity ceiling against a
/// decompression bomb or a bogus entry that would otherwise reserve/read gigabytes
/// before the decoder ever sees it. Generous: real encoded photos are far smaller
/// (RAW ~100 MB; even a huge TIFF/PSD rarely approaches this). A genuinely larger file
/// can be extracted and opened directly.
const MAX_ENTRY_BYTES: u64 = 1 << 30; // 1 GiB

/// Read up to `size` bytes from `rd` into `out` in chunks, checking `cancel` between
/// chunks so a large entry aborts within one chunk's latency rather than only at the
/// next block boundary (which, for a solid archive, can mean after the whole thing).
/// `Ok(true)` = read to `size` (or a short EOF); `Ok(false)` = cancelled mid-stream.
fn read_cancellable(
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
fn normalize_entry_name(raw: &str) -> String {
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
        let src = SevenZSource::open_with_progress(&z, None, is_img, Some(&progress)).unwrap();
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
        match SevenZSource::open_with_progress(&z, None, is_img, Some(&progress)) {
            Err(OpenError::Cancelled) => {}
            other => panic!("expected Cancelled, got {:?}", other.err()),
        }
        assert_eq!(progress.done(), 0, "cancelled before any entry decoded");
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
}
