//! [`TarSource`] — the tar family behind the `ItemSource` seam (task #102):
//! plain `.tar` plus `.tar.gz`/`.tgz`, `.tar.bz2`/`.tbz2`/`.tbz`,
//! `.tar.zst`/`.tzst`, and `.tar.xz`/`.txz`.
//!
//! Two access models, both already precedented in this crate:
//!
//! * **Plain `.tar` is lazy** (ZIP's shape). Tar headers sit at known offsets, so
//!   one index pass over [`tar::Archive::entries_with_seek`] — which *seeks* over
//!   file data, reading headers only — yields `(offset, size, name)` per entry.
//!   `bytes(i)` is then an open + seek + bounded read. No handle pool: unlike
//!   `ZipArchive` there is no parsed per-handle state worth reusing. The open is
//!   still O(entries) of file I/O, so it runs off-thread like the eager kinds
//!   ([`ArchiveKind::background_open`]) and reports determinate progress
//!   (header offset vs. file length).
//! * **Compressed tar is eager** (7z's shape). The whole archive is one solid
//!   stream — there is no random access without decompressing everything before
//!   the target — so the open streams every supported entry into RAM once,
//!   honoring the shared [`OpenProgress`] cancel plumbing. Unlike 7z, the header
//!   carries **no size table** (per-entry sizes are tar headers interleaved
//!   *inside* the compressed stream), so the RAM budget can't be pre-flighted;
//!   it is enforced *during* the stream instead — still predict-and-refuse per
//!   allocation ([`OpenError::TooLarge`] before any `try_reserve`), preserving
//!   the no-uncatchable-abort reasoning from the 7z path.
//!
//! Progress for an eager open is **compressed bytes consumed** (a counting
//! reader below the codec) against the file's length: a determinate bar with no
//! need for a decompressed total. The codec stack is pure Rust throughout — see
//! the crate manifest for the picks and why.
//!
//! ## Hostile-input hardening (plan #102 rev2)
//!
//! Archives are hostile bytes. Beyond the per-entry ceiling and the RAM budget,
//! the opens here bound every other allocation and work sink:
//!
//! * **Metadata bombs.** The `tar` crate reads GNU long-name / PAX payloads with
//!   `read_to_end` — unbounded by itself. All entry *data* here is read by our
//!   own code between iterator steps, so anything the crate pulls *inside*
//!   `next()` is headers + metadata only: a [`MeteredReader`] under the archive
//!   is armed with a small quota around each step and disarmed for our own data
//!   reads, so a hostile metadata header fails as `Corrupt` instead of aborting
//!   the process. (Raw-mode iteration was rejected: the non-raw iterator is what
//!   applies PAX `size` overrides to stream advancement — reimplementing that is
//!   how you get desync bugs.)
//! * **Index-table bombs.** A cap on entry count and on total recorded name
//!   bytes bounds the index tables a million-empty-file archive can force.
//! * **Work bombs.** A total expanded-work cap bounds the skip-and-drain CPU a
//!   tiny compressed file can demand (resident-byte accounting alone would let
//!   a bomb stream petabytes through the drain path).
//! * **Codec allocation bombs.** The zstd path pre-checks each frame's declared
//!   window before the decoder can allocate it, and verifies frame checksums;
//!   the xz path pre-checks the first block's declared LZMA2 dictionary size.
//! * **Names.** Entry names that are empty, contain NUL or `..` components, or
//!   exceed 4096 bytes are skipped (cosmetic here — entries are read by offset,
//!   never by path — but cheap defense in depth).
//!
//! Deliberate v1 limitations: hard links are skipped (they *can* name images,
//! but safe internal aliasing isn't worth the complexity); GNU sparse entries
//! are skipped via `is_file()`; PAX-sparse (format 1.0) entries index as regular
//! files and fail honestly at the image decoder.
//!
//! **Privacy:** RAM-only, read-only, like every source here. Nothing is ever
//! extracted to disk. The tar family has **no standard encryption**, so these
//! opens never return [`OpenError::PasswordRequired`].

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::{
    ext_of, normalize_entry_name, out_of_range, read_cancellable, zip_dir_of, ArchiveKind,
    ItemSource, OpenError, OpenProgress, MAX_ENTRY_BYTES,
};

/// Sanity limits for an archive open, injectable so tests can drive every
/// refusal path with small fixtures. [`OpenLimits::default`] is the ship config.
pub(crate) struct OpenLimits {
    /// Per-entry byte ceiling (the [`MAX_ENTRY_BYTES`] bomb guard).
    pub ceiling: u64,
    /// Maximum entries walked before the open refuses (index-table bomb).
    pub max_entries: usize,
    /// Maximum total recorded name bytes (the other half of the table bomb).
    pub max_name_bytes: u64,
    /// Maximum total decompressed bytes an eager open may process — kept,
    /// skipped, and drained alike (work bomb; ~64 GiB ≈ a minute of drain at
    /// pure-Rust codec speeds, vs. unbounded).
    pub max_expanded: u64,
}

impl Default for OpenLimits {
    fn default() -> Self {
        OpenLimits {
            ceiling: MAX_ENTRY_BYTES,
            max_entries: 1_000_000,
            max_name_bytes: 64 << 20,
            max_expanded: 64 << 30,
        }
    }
}

/// Decompressed bytes the `tar` crate may pull *inside* one iterator step —
/// headers, GNU long-name / PAX payloads, and inter-entry padding. Legitimate
/// metadata is a few KiB; this is the metadata-bomb ceiling.
const META_QUOTA: u64 = 512 * 1024;

/// One regular file recorded from the archive — the tar analog of a ZIP
/// central-directory row.
struct TarFile {
    /// Normalized archive-relative name (display + extension routing).
    name: String,
    /// Uncompressed size in bytes.
    size: u64,
    /// Byte offset of the entry's data in the plain `.tar` (lazy model only;
    /// unused — 0 — for an eager store, whose bytes are already resident).
    offset: u64,
}

/// Where an item's bytes come from.
enum Store {
    /// Plain `.tar`: `bytes(i)` re-opens the file and seeks.
    Lazy,
    /// Compressed tar: decompressed bytes, index-aligned with `files`.
    Eager(Vec<Vec<u8>>),
}

/// A tar-family archive as an [`ItemSource`]. See the module docs for the two
/// access models; which one a given file uses is decided by
/// [`archive_kind`](crate::archive_kind) (plain tar → [`TarSource::open_tar`],
/// compressed → [`TarSource::open_compressed`]).
pub struct TarSource {
    path: PathBuf,
    /// Every recorded file, duplicate names resolved last-wins (tar append-mode
    /// semantics, matching `tar -x`), sorted by name. For the lazy model this
    /// holds **all** regular files (so sidecar siblings resolve, like ZIP's
    /// directory); for the eager model only the kept entries (the rest were
    /// never decompressed — the same documented gap as 7z).
    files: Vec<TarFile>,
    /// Item index → `files` index for the supported, within-ceiling entries.
    items: Vec<usize>,
    store: Store,
}

impl TarSource {
    /// Open a plain `.tar` lazily: one header-only index pass (file data is
    /// seeked over, never read). `is_supported` is the entry predicate (the app
    /// passes its image+video union). Reports determinate progress (header
    /// offset vs. file length) and honors cancellation through `progress`.
    pub fn open_tar(
        path: impl Into<PathBuf>,
        is_supported: impl Fn(&str) -> bool,
        progress: Option<&OpenProgress>,
    ) -> Result<Self, OpenError> {
        Self::open_tar_with_limits(path, is_supported, progress, &OpenLimits::default())
    }

    pub(crate) fn open_tar_with_limits(
        path: impl Into<PathBuf>,
        is_supported: impl Fn(&str) -> bool,
        progress: Option<&OpenProgress>,
        limits: &OpenLimits,
    ) -> Result<Self, OpenError> {
        let path = path.into();
        let file = File::open(&path)?;
        let file_len = file.metadata()?.len();
        if let Some(p) = progress {
            p.set_total(file_len);
        }
        let (files, items) = index_tar(
            BufReader::new(file),
            file_len,
            &is_supported,
            progress,
            limits,
        )?;
        Ok(Self {
            path,
            files,
            items,
            store: Store::Lazy,
        })
    }
    /// Open a compressed tarball (`kind` must be one of the eager tar kinds) by
    /// streaming every supported entry into RAM. Honors `progress` for the
    /// determinate bar (compressed bytes consumed vs. file length) and
    /// cancellation — checked between entries and between 64 KiB output chunks
    /// within them (a decoder may still process one internal block between
    /// checks); enforces `budget` — the caller's RAM ceiling for the resident
    /// decompressed entries — *before* each allocation, failing with
    /// [`OpenError::TooLarge`] rather than risking an uncatchable abort.
    pub fn open_compressed(
        path: impl Into<PathBuf>,
        kind: ArchiveKind,
        is_supported: impl Fn(&str) -> bool,
        progress: Option<&OpenProgress>,
        budget: u64,
    ) -> Result<Self, OpenError> {
        Self::open_compressed_with_limits(
            path,
            kind,
            is_supported,
            progress,
            budget,
            &OpenLimits::default(),
        )
    }

    pub(crate) fn open_compressed_with_limits(
        path: impl Into<PathBuf>,
        kind: ArchiveKind,
        is_supported: impl Fn(&str) -> bool,
        progress: Option<&OpenProgress>,
        budget: u64,
        limits: &OpenLimits,
    ) -> Result<Self, OpenError> {
        let path = path.into();
        let file = File::open(&path)?;
        let total = file.metadata()?.len();
        if let Some(p) = progress {
            p.set_total(total);
        }
        let counted = CountingReader {
            inner: BufReader::with_capacity(1 << 20, file),
            progress: progress.cloned(),
        };
        let (files, items, store) =
            stream_tarball(kind, counted, &is_supported, progress, budget, limits)?;
        // Snap the bar to complete (belt: padding rounding could leave a byte).
        if let Some(p) = progress {
            let done = p.done();
            if done < total {
                p.add_done(total - done);
            }
        }
        Ok(Self {
            path,
            files,
            items,
            store,
        })
    }

    /// Read a lazily-indexed file's bytes: open + seek + bounded read, with the
    /// same bomb guards as ZIP (`MAX_ENTRY_BYTES` ceiling, recoverable reserve),
    /// plus an exact-length check — the tar may have changed since indexing.
    fn read_lazy(&self, f: &TarFile) -> io::Result<Vec<u8>> {
        if f.size > MAX_ENTRY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive entry too large",
            ));
        }
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(f.offset))?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(f.size as usize).map_err(|_| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "archive entry too large to allocate",
            )
        })?;
        file.take(f.size).read_to_end(&mut buf)?;
        if buf.len() as u64 != f.size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "archive entry truncated",
            ));
        }
        Ok(buf)
    }
}

/// The lazy index pass over a plain tar byte stream (split from
/// [`TarSource::open_tar`] so the fuzz harness can drive it over raw bytes):
/// header-only, seeking over file data, with the metadata quota and the
/// index-table caps applied. Returns the recorded files + the supported-item
/// index map.
fn index_tar<R: Read + Seek>(
    reader: R,
    file_len: u64,
    is_supported: &dyn Fn(&str) -> bool,
    progress: Option<&OpenProgress>,
    limits: &OpenLimits,
) -> Result<(Vec<TarFile>, Vec<usize>), OpenError> {
    let quota = Arc::new(AtomicU64::new(META_QUOTA));
    let metered = MeteredReader {
        inner: reader,
        quota: Arc::clone(&quota),
        total: 0,
        max_total: limits.max_expanded,
    };
    let mut archive = tar::Archive::new(metered);
    // Last-wins by name (tar append mode): a BTreeMap both dedups and hands
    // back the same ascending name order ZIP's post-scan sort produces.
    let mut latest: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut walked = 0usize;
    let mut name_bytes = 0u64;
    let mut prev_pos = 0u64;
    let mut entries = archive
        .entries_with_seek()
        .map_err(|e| OpenError::Corrupt(e.to_string()))?;
    loop {
        if progress.is_some_and(|p| p.is_cancelled()) {
            return Err(OpenError::Cancelled);
        }
        // Metadata quota for this step (headers + long-name/PAX payloads);
        // the seek-based data skip doesn't read, so no disarm is needed.
        quota.store(META_QUOTA, Ordering::Relaxed);
        let Some(entry) = entries.next() else { break };
        let entry = entry.map_err(|e| OpenError::Corrupt(e.to_string()))?;
        walked += 1;
        if walked > limits.max_entries {
            return Err(OpenError::Corrupt(
                "the archive has too many entries".into(),
            ));
        }
        if let Some(p) = progress {
            let pos = entry.raw_file_position();
            p.add_done(pos.saturating_sub(prev_pos));
            prev_pos = pos;
        }
        if !entry.header().entry_type().is_file() {
            continue; // symlinks/hardlinks/devices/sparse — see module docs
        }
        // `path_bytes` resolves PAX/GNU long names; lossy is safe — entries
        // are only ever read back by offset, never looked up by name.
        let name = normalize_entry_name(&String::from_utf8_lossy(&entry.path_bytes()));
        if !sane_name(&name) {
            continue;
        }
        let (offset, size) = (entry.raw_file_position(), entry.size());
        // A truncated tail entry (its data runs past EOF) can never render;
        // skip it and keep the rest of the archive viewable.
        if offset.saturating_add(size) > file_len {
            continue;
        }
        name_bytes += name.len() as u64;
        if name_bytes > limits.max_name_bytes {
            return Err(OpenError::Corrupt(
                "the archive has too many entries".into(),
            ));
        }
        latest.insert(name, (offset, size));
    }
    if let Some(p) = progress {
        p.add_done(file_len.saturating_sub(prev_pos));
    }
    let files: Vec<TarFile> = latest
        .into_iter()
        .map(|(name, (offset, size))| TarFile { name, size, offset })
        .collect();
    let items = files
        .iter()
        .enumerate()
        .filter(|(_, f)| is_supported(&ext_of(&f.name)) && f.size <= limits.ceiling)
        .map(|(i, _)| i)
        .collect();
    Ok((files, items))
}

/// The eager streaming pass over a compressed tar byte stream (split from
/// [`TarSource::open_compressed`] so the fuzz harness can drive it over raw
/// bytes): decode via the codec seam, keep supported entries under the budget,
/// drain the rest, then drain the codec to EOF for trailer validation.
fn stream_tarball<R: Read + 'static>(
    kind: ArchiveKind,
    input: R,
    is_supported: &dyn Fn(&str) -> bool,
    progress: Option<&OpenProgress>,
    budget: u64,
    limits: &OpenLimits,
) -> Result<(Vec<TarFile>, Vec<usize>, Store), OpenError> {
    let quota = Arc::new(AtomicU64::new(META_QUOTA));
    let metered = MeteredReader {
        inner: decompressor(kind, input)?,
        quota: Arc::clone(&quota),
        total: 0,
        max_total: limits.max_expanded,
    };
    let mut archive = tar::Archive::new(metered);
    let cancel = || progress.is_some_and(|p| p.is_cancelled());

    let mut latest: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut resident = 0u64;
    let mut walked = 0usize;
    let mut name_bytes = 0u64;
    {
        let mut entries = archive
            .entries()
            .map_err(|e| OpenError::Corrupt(e.to_string()))?;
        loop {
            if cancel() {
                return Err(OpenError::Cancelled);
            }
            quota.store(META_QUOTA, Ordering::Relaxed);
            let Some(entry) = entries.next() else { break };
            let mut entry = entry.map_err(|e| OpenError::Corrupt(e.to_string()))?;
            walked += 1;
            if walked > limits.max_entries {
                return Err(OpenError::Corrupt(
                    "the archive has too many entries".into(),
                ));
            }
            // Our own data reads below are exempt from the metadata quota (the
            // expanded-work counter inside MeteredReader keeps running — it
            // caps every decompressed byte, whoever pulls it).
            quota.store(u64::MAX, Ordering::Relaxed);
            let size = entry.size();
            let name = normalize_entry_name(&String::from_utf8_lossy(&entry.path_bytes()));
            let keep = entry.header().entry_type().is_file()
                && sane_name(&name)
                && is_supported(&ext_of(&name))
                && size <= limits.ceiling;
            if !keep {
                // Skip (never index) an unsupported or oversized entry — but
                // drain it ourselves in cancellable chunks: the tar iterator
                // would otherwise read through it in one opaque pass, and for
                // a bomb that's the whole point of the chunked cancel check.
                if !read_cancellable(&mut entry, size, &mut io::sink(), &cancel)
                    .map_err(stream_read_err)?
                {
                    return Err(OpenError::Cancelled);
                }
                continue;
            }
            name_bytes += name.len() as u64;
            if name_bytes > limits.max_name_bytes {
                return Err(OpenError::Corrupt(
                    "the archive has too many entries".into(),
                ));
            }
            // The streaming budget gate: refuse before reserving (prediction
            // is the real defense; `try_reserve` is the backstop). A replaced
            // duplicate's bytes are released, so they don't double-count.
            let replaced = latest.get(&name).map_or(0, |b| b.len() as u64);
            let needed = (resident - replaced).saturating_add(size);
            if needed > budget {
                return Err(OpenError::TooLarge { needed, budget });
            }
            let mut buf = Vec::new();
            buf.try_reserve_exact(size as usize)
                .map_err(|_| OpenError::OutOfMemory)?;
            if !read_cancellable(&mut entry, size, &mut buf, &cancel).map_err(stream_read_err)? {
                return Err(OpenError::Cancelled);
            }
            // `read_cancellable` treats a short clean EOF as success (its 7z
            // callers bound-check elsewhere); here a short entry means the
            // stream ended mid-file — a truncated archive, not a viewable item.
            if buf.len() as u64 != size {
                return Err(OpenError::Corrupt("archive entry truncated".into()));
            }
            resident = (resident - replaced).saturating_add(buf.len() as u64);
            latest.insert(name, buf);
        }
    }
    // Drain the codec to EOF: the tar iterator stops at the end-of-archive
    // marker, but gzip/bzip2/zstd validate their trailing checksums only
    // when the stream is read through — and the progress bar's compressed
    // count only reaches the file length once everything is consumed.
    // Reads stay on the metered reader (quota disarmed), so the drain counts
    // toward the same expanded-work cap as everything else.
    {
        quota.store(u64::MAX, Ordering::Relaxed);
        let mut metered = archive.into_inner();
        let mut staging = [0u8; 64 * 1024];
        loop {
            if cancel() {
                return Err(OpenError::Cancelled);
            }
            let n = metered.read(&mut staging).map_err(stream_read_err)?;
            if n == 0 {
                break;
            }
        }
    }
    let mut files = Vec::with_capacity(latest.len());
    let mut store = Vec::with_capacity(latest.len());
    for (name, bytes) in latest {
        files.push(TarFile {
            name,
            size: bytes.len() as u64,
            offset: 0,
        });
        store.push(bytes);
    }
    let items = (0..files.len()).collect();
    Ok((files, items, Store::Eager(store)))
}

/// Raw-byte entry points for the cargo-fuzz harness (`fuzz/`): arbitrary bytes
/// must only ever produce `Ok`/`Err` — never a panic, an unbounded allocation,
/// or non-termination. Never compiled into a ship build.
#[cfg(feature = "fuzz-internals")]
pub mod fuzz {
    use super::*;

    fn limits() -> OpenLimits {
        OpenLimits {
            ceiling: 1 << 20,
            max_entries: 4096,
            max_name_bytes: 1 << 20,
            max_expanded: 1 << 24,
        }
    }

    /// The lazy plain-tar index pass over raw bytes.
    pub fn tar_index(data: &[u8]) {
        let _ = index_tar(
            io::Cursor::new(data),
            data.len() as u64,
            &|_| true,
            None,
            &limits(),
        );
    }

    /// The eager streaming pass over raw bytes, codec selected by `sel`.
    pub fn tar_stream(sel: u8, data: &[u8]) {
        let kind = match sel & 3 {
            0 => ArchiveKind::TarGz,
            1 => ArchiveKind::TarBz2,
            2 => ArchiveKind::TarZst,
            _ => ArchiveKind::TarXz,
        };
        let _ = stream_tarball(
            kind,
            io::Cursor::new(data.to_vec()),
            &|_| true,
            None,
            1 << 24,
            &limits(),
        );
    }
}

impl ItemSource for TarSource {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn name(&self, i: usize) -> &str {
        self.items
            .get(i)
            .map_or("", |&j| self.files[j].name.as_str())
    }

    fn container(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn bytes(&self, i: usize) -> io::Result<Vec<u8>> {
        let &j = self.items.get(i).ok_or_else(out_of_range)?;
        match &self.store {
            Store::Lazy => self.read_lazy(&self.files[j]),
            // Already decompressed and resident — hand back a copy (7z's shape).
            Store::Eager(store) => Ok(store[j].clone()),
        }
    }

    fn size_hint(&self, i: usize) -> Option<u64> {
        self.items.get(i).map(|&j| self.files[j].size)
    }

    /// Lazy model only: every file name in item `i`'s archive directory,
    /// including entries the index never listed (a `.srt` sidecar) — `files` is
    /// the full listing, exactly like ZIP's central directory. The eager model
    /// deliberately reports none, same reasoning as 7z: an unindexed entry's
    /// bytes were never decompressed, so a name without readable bytes would
    /// only mislead callers.
    fn sibling_names(&self, i: usize) -> Vec<String> {
        if !matches!(self.store, Store::Lazy) {
            return Vec::new();
        }
        let Some(&j) = self.items.get(i) else {
            return Vec::new();
        };
        let dir = zip_dir_of(&self.files[j].name);
        self.files
            .iter()
            .filter(|f| zip_dir_of(&f.name) == dir)
            .map(|f| f.name.clone())
            .collect()
    }

    /// Read a sibling by its exact archive name (lazy model only), scoped to
    /// item `i`'s own directory — same containment rule and bomb guards as ZIP.
    fn sibling_bytes(&self, i: usize, name: &str) -> io::Result<Vec<u8>> {
        if !matches!(self.store, Store::Lazy) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this source has no siblings",
            ));
        }
        let &j = self.items.get(i).ok_or_else(out_of_range)?;
        if zip_dir_of(name) != zip_dir_of(&self.files[j].name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sidecar is not a sibling of the item",
            ));
        }
        let f = self
            .files
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such archive entry"))?;
        self.read_lazy(f)
    }
}

/// A normalized entry name we are willing to record. Entries are read by
/// offset/index — never by path — so this is defense in depth for display and
/// sibling lookup, not a traversal gate.
pub(crate) fn sane_name(name: &str) -> bool {
    !name.is_empty()
        && !name.ends_with('/')
        && name.len() <= 4096
        && !name.contains('\0')
        && !name.split('/').any(|c| c == "..")
}

/// Classify an error from reading entry *data* out of the decode stream: a
/// truncated or checksum-failed stream surfaces as `UnexpectedEof`/`InvalidData`
/// (or, from flate2's gzip CRC check, `InvalidInput`) — all a damaged archive
/// (→ [`OpenError::Corrupt`], so the user sees "may be damaged"), not an I/O
/// failure of the file itself. OS-level reads never produce the two
/// `Invalid*` kinds.
fn stream_read_err(e: io::Error) -> OpenError {
    match e.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => {
            OpenError::Corrupt(e.to_string())
        }
        _ => OpenError::Io(e),
    }
}

/// The codec seam: stack the right pure-Rust decompressor for `kind` over the
/// (counting) compressed byte stream. One place to swap or A/B a codec.
fn decompressor<R: Read + 'static>(
    kind: ArchiveKind,
    inner: R,
) -> Result<Box<dyn Read>, OpenError> {
    Ok(match kind {
        // Multi-member/multi-stream variants so pigz/pbzip2 output decodes whole.
        ArchiveKind::TarGz => Box::new(flate2::read::MultiGzDecoder::new(inner)),
        ArchiveKind::TarBz2 => Box::new(bzip2::read::MultiBzDecoder::new(inner)),
        ArchiveKind::TarZst => Box::new(MultiFrameZstd::new(inner)),
        ArchiveKind::TarXz => {
            // Pre-check the first block's declared LZMA2 dictionary before the
            // reader can `vec![0; dict_size]` it (up to 4 GiB from one hostile
            // byte). Later blocks/streams re-declaring bigger dictionaries are a
            // documented residual — bounded at 4 GiB of mostly-untouched zeroed
            // pages — until lzma-rust2 grows a mem-limit parameter.
            let mut inner = inner;
            let prefix = xz_first_dict_precheck(&mut inner).map_err(stream_read_err)?;
            Box::new(lzma_rust2::XzReader::new(
                io::Cursor::new(prefix).chain(inner),
                true,
            ))
        }
        ArchiveKind::Zip | ArchiveKind::SevenZ | ArchiveKind::Tar | ArchiveKind::Rar => {
            return Err(OpenError::Corrupt(
                "not a compressed tar (wrong dispatch)".into(),
            ))
        }
    })
}

/// Counts compressed bytes as the codec consumes them, feeding the determinate
/// progress bar. Sits *below* the codec and above the file's `BufReader`, so it
/// sees exactly what the decoder pulled (not the readahead).
struct CountingReader<R: Read> {
    inner: R,
    progress: Option<OpenProgress>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Some(p) = &self.progress {
            p.add_done(n as u64);
        }
        Ok(n)
    }
}

/// The metadata-bomb guard (module docs): bounds what the `tar` crate can read
/// — and therefore allocate — *inside* one iterator step. The open arms `quota`
/// with [`META_QUOTA`] before each `next()` and disarms (`u64::MAX`) for its own
/// entry-data reads; seeks (the lazy model's data skip) bypass it entirely.
///
/// It is also the **expanded-work counter**: `total` accumulates every byte the
/// consumer pulls — headers, long-name/PAX metadata, padding, entry data, and
/// the trailing drain alike — against `max_total`. Counting at this one choke
/// point (rather than summing declared entry sizes) is what stops a
/// zero-payload metadata bomb: a million entries of pure header/metadata bytes
/// expand real decompressor output without ever appearing in an entry size.
struct MeteredReader<R> {
    inner: R,
    quota: Arc<AtomicU64>,
    total: u64,
    max_total: u64,
}

impl<R: Read> Read for MeteredReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let quota = self.quota.load(Ordering::Relaxed);
        if quota == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive metadata exceeds sanity limits",
            ));
        }
        let want = buf.len().min(usize::try_from(quota).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        if quota != u64::MAX {
            self.quota.fetch_sub(n as u64, Ordering::Relaxed);
        }
        self.total = self.total.saturating_add(n as u64);
        if self.total > self.max_total {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the archive expands past the sanity limit",
            ));
        }
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for MeteredReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// Parse enough of an xz container to find the first block's LZMA2 dictionary
/// size and refuse a hostile declaration (cap: 256 MiB; the xz CLI's biggest
/// preset uses 64 MiB). Returns the bytes consumed, which the caller replays
/// ahead of the real reader. An archive with no blocks (index comes first) or
/// an unparseable prefix is passed through — the reader produces its own error.
fn xz_first_dict_precheck<R: Read>(inner: &mut R) -> io::Result<Vec<u8>> {
    const DICT_CAP: u64 = 256 << 20;
    let mut buf = Vec::with_capacity(64);
    let mut take = |buf: &mut Vec<u8>, n: usize| -> io::Result<Option<usize>> {
        let start = buf.len();
        buf.resize(start + n, 0);
        let mut filled = 0;
        while filled < n {
            let m = inner.read(&mut buf[start + filled..])?;
            if m == 0 {
                buf.truncate(start + filled);
                return Ok(None); // truncated: let the real reader error
            }
            filled += m;
        }
        Ok(Some(start))
    };
    // Stream header: 6-byte magic + 2 flag bytes + CRC32.
    let Some(_) = take(&mut buf, 12)? else {
        return Ok(buf);
    };
    if buf[..6] != [0xFD, b'7', b'z', b'X', b'Z', 0x00] {
        return Ok(buf); // not xz: the reader will say so
    }
    // Block header, byte 0: encoded size. 0x00 means the index — no blocks.
    let Some(b0_at) = take(&mut buf, 1)? else {
        return Ok(buf);
    };
    let header_size = (u64::from(buf[b0_at]) + 1) * 4;
    if buf[b0_at] == 0 {
        return Ok(buf);
    }
    // The rest of the block header is small (≤ 1024 bytes); buffer it whole.
    let Some(rest_at) = take(&mut buf, header_size as usize - 1)? else {
        return Ok(buf);
    };
    let hdr = &buf[rest_at..];
    let flags = hdr[0];
    let filter_count = (flags & 0x03) + 1;
    let mut at = 1usize;
    let vli = |at: &mut usize| -> Option<u64> {
        let mut v = 0u64;
        for k in 0..9 {
            let b = *hdr.get(*at)?;
            *at += 1;
            v |= u64::from(b & 0x7F) << (7 * k);
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    };
    if flags & 0x40 != 0 && vli(&mut at).is_none() {
        return Ok(buf); // unparseable compressed size: defer to the reader
    }
    if flags & 0x80 != 0 && vli(&mut at).is_none() {
        return Ok(buf); // unparseable uncompressed size: defer to the reader
    }
    for _ in 0..filter_count {
        let Some(id) = vli(&mut at) else {
            return Ok(buf);
        };
        let Some(props_len) = vli(&mut at) else {
            return Ok(buf);
        };
        if id == 0x21 {
            // LZMA2: one property byte encodes the dictionary size.
            let Some(&p) = hdr.get(at) else {
                return Ok(buf);
            };
            let v = p & 0x3F;
            let dict = if v > 40 {
                return Ok(buf); // invalid: the reader will reject it
            } else if v == 40 {
                u64::from(u32::MAX)
            } else {
                u64::from(2 | (v & 1)) << (v / 2 + 11)
            };
            if dict > DICT_CAP {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "xz dictionary too large",
                ));
            }
            return Ok(buf);
        }
        at += props_len as usize; // a BCJ/delta filter's properties
    }
    Ok(buf)
}

/// A multi-frame zstd reader over `ruzstd`, whose [`StreamingDecoder`] is
/// single-frame by design (its docs tell callers to handle frame boundaries and
/// skippable frames themselves — <https://github.com/KillingSpark/zstd-rs/issues/57>).
/// Real `.tar.zst` files can be multi-frame (`zstd --rsyncable`, `pzstd`) and may
/// interleave skippable frames, so this drives one reusable [`FrameDecoder`]
/// across frames: at each frame boundary it probes for EOF, replays the probed
/// bytes into the header parse, and drains skippable frames.
///
/// Two integrity gaps in ruzstd 0.8.3 are closed here (plan #102 rev2 §4):
/// the declared window is pre-checked **before** `init` can allocate it (the
/// upstream 100 MB cap is enforced on `reset` but not on a decoder's first
/// frame), and each frame's stored checksum is compared against the calculated
/// one (upstream computes both but never compares).
///
/// [`StreamingDecoder`]: ruzstd::decoding::StreamingDecoder
/// [`FrameDecoder`]: ruzstd::decoding::FrameDecoder
struct MultiFrameZstd<R: Read> {
    source: R,
    decoder: ruzstd::decoding::FrameDecoder,
    /// Bytes probed past a frame's end (EOF probe + header pre-check), owed to
    /// the next header parse.
    pending: VecDeque<u8>,
    /// Whether `decoder` holds an initialized frame that hasn't been drained.
    in_frame: bool,
    done: bool,
}

/// ruzstd's own `MAXIMUM_ALLOWED_WINDOW_SIZE`, mirrored so our pre-check and its
/// `reset`-path check agree.
const ZSTD_WINDOW_CAP: u64 = 100 * 1024 * 1024;

impl<R: Read> MultiFrameZstd<R> {
    fn new(source: R) -> Self {
        Self {
            source,
            decoder: ruzstd::decoding::FrameDecoder::new(),
            pending: VecDeque::new(),
            in_frame: false,
            done: false,
        }
    }

    /// Buffer source bytes into `pending` until it holds `n`; `Ok(false)` on a
    /// clean EOF first (whatever was read stays buffered for the error path).
    fn fill_pending(&mut self, n: usize) -> io::Result<bool> {
        let mut chunk = [0u8; 16];
        while self.pending.len() < n {
            let want = (n - self.pending.len()).min(chunk.len());
            match self.source.read(&mut chunk[..want]) {
                Ok(0) => return Ok(false),
                Ok(m) => self.pending.extend(&chunk[..m]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Position `decoder` at the next data frame. `Ok(false)` = clean EOF.
    fn next_frame(&mut self) -> io::Result<bool> {
        use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
        loop {
            if self.pending.is_empty() && !self.fill_pending(1)? {
                return Ok(false); // clean EOF: no next frame
            }
            // Pre-check a data frame's declared window BEFORE init can allocate
            // it. A short fill here is a truncated header — fall through and let
            // init produce the precise error.
            if self.fill_pending(5)? && self.peek_u32_le() == 0xFD2F_B528 {
                self.check_window()?;
            }
            let mut replay = Replay {
                pending: &mut self.pending,
                source: &mut self.source,
            };
            match self.decoder.init(&mut replay) {
                Ok(()) => {
                    self.in_frame = true;
                    return Ok(true);
                }
                // A skippable frame: drain its payload and look again.
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                    length,
                    ..
                })) => {
                    let mut rest = Replay {
                        pending: &mut self.pending,
                        source: &mut self.source,
                    };
                    let copied =
                        io::copy(&mut (&mut rest).take(u64::from(length)), &mut io::sink())?;
                    // A short drain means the declared payload runs past EOF —
                    // a truncated file, not a clean end (the next probe would
                    // otherwise mistake it for one).
                    if copied != u64::from(length) {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "truncated zstd skippable frame",
                        ));
                    }
                    continue;
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            }
        }
    }

    fn peek_u32_le(&self) -> u32 {
        let b: Vec<u8> = self.pending.iter().take(4).copied().collect();
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    /// Parse the frame header far enough to learn the declared window and
    /// refuse one over [`ZSTD_WINDOW_CAP`]. `pending` holds ≥ 5 bytes (magic +
    /// descriptor) on entry; everything read stays buffered for `init`.
    fn check_window(&mut self) -> io::Result<()> {
        let desc = self.pending[4];
        let single_segment = desc & 0x20 != 0;
        let window = if single_segment {
            // No window descriptor: the window is the frame content size.
            let dict_len = [0usize, 1, 2, 4][(desc & 0x03) as usize];
            let fcs_len = match desc >> 6 {
                0 => 1,
                1 => 2,
                2 => 4,
                _ => 8,
            };
            if !self.fill_pending(5 + dict_len + fcs_len)? {
                return Ok(()); // truncated: init reports it
            }
            let at = 5 + dict_len;
            let mut v = 0u64;
            for k in 0..fcs_len {
                v |= u64::from(self.pending[at + k]) << (8 * k);
            }
            if fcs_len == 2 {
                v += 256;
            }
            v
        } else {
            if !self.fill_pending(6)? {
                return Ok(());
            }
            let wd = self.pending[5];
            let base = 1u64 << (10 + (wd >> 3));
            base + (base / 8) * u64::from(wd & 0x07)
        };
        if window > ZSTD_WINDOW_CAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "zstd window too large",
            ));
        }
        Ok(())
    }
}

/// Serves the probed/pre-checked bytes first, then the real source — so the
/// frame-header parse never notices the lookahead.
struct Replay<'a, R: Read> {
    pending: &'a mut VecDeque<u8>,
    source: &'a mut R,
}

impl<R: Read> Read for Replay<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if !self.pending.is_empty() {
            let n = buf.len().min(self.pending.len());
            for slot in buf.iter_mut().take(n) {
                *slot = self.pending.pop_front().expect("len checked");
            }
            return Ok(n);
        }
        self.source.read(buf)
    }
}

impl<R: Read> Read for MultiFrameZstd<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use ruzstd::decoding::BlockDecodingStrategy;
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.done {
                return Ok(0);
            }
            if !self.in_frame && !self.next_frame()? {
                self.done = true;
                return Ok(0);
            }
            // Decode until something is collectable or the frame ends. (The
            // header was fully parsed by `init`, so reads here come straight
            // from the source — the replay bytes are long consumed.)
            while self.decoder.can_collect() == 0 && !self.decoder.is_finished() {
                self.decoder
                    .decode_blocks(
                        &mut self.source,
                        BlockDecodingStrategy::UptoBytes(buf.len()),
                    )
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            }
            let n = self.decoder.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            // Frame drained: verify its checksum (ruzstd computes both sides
            // but never compares them), then look for the next frame.
            if let (Some(stored), Some(calculated)) = (
                self.decoder.get_checksum_from_data(),
                self.decoder.get_calculated_checksum(),
            ) {
                if stored != calculated {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "zstd frame checksum mismatch",
                    ));
                }
            }
            self.in_frame = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The same test-local predicate the lib.rs suites use.
    fn is_img(ext: &str) -> bool {
        matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tga")
    }

    static NONCE: AtomicUsize = AtomicUsize::new(0);

    fn temp_path(tag: &str, ext: &str) -> PathBuf {
        let n = NONCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("pb_tar_{tag}_{}_{n}.{ext}", std::process::id()))
    }

    /// A plain tar (in memory) from (name, bytes) file entries.
    fn tar_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for &(name, data) in files {
            append_file(&mut b, name, data);
        }
        b.into_inner().unwrap()
    }

    fn append_file(b: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8]) {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, name, data).unwrap();
    }

    fn write_file(tag: &str, ext: &str, bytes: &[u8]) -> PathBuf {
        let path = temp_path(tag, ext);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn open_tar(path: &Path) -> Result<TarSource, OpenError> {
        TarSource::open_tar(path, is_img, None)
    }

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn bz2(data: &[u8]) -> Vec<u8> {
        let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn zst(data: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn xz(data: &[u8]) -> Vec<u8> {
        let mut w =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
        w.write_all(data).unwrap();
        w.finish().unwrap()
    }

    fn compress(kind: ArchiveKind, data: &[u8]) -> Vec<u8> {
        match kind {
            ArchiveKind::TarGz => gz(data),
            ArchiveKind::TarBz2 => bz2(data),
            ArchiveKind::TarZst => zst(data),
            ArchiveKind::TarXz => xz(data),
            _ => unreachable!("not a compressed tar kind"),
        }
    }

    const EAGER_KINDS: [(ArchiveKind, &str); 4] = [
        (ArchiveKind::TarGz, "tar.gz"),
        (ArchiveKind::TarBz2, "tar.bz2"),
        (ArchiveKind::TarZst, "tar.zst"),
        (ArchiveKind::TarXz, "tar.xz"),
    ];

    // ── lazy (plain .tar) ────────────────────────────────────────────────

    #[test]
    fn tar_lists_supported_sorted_and_reads_bytes_by_offset() {
        let tar = tar_bytes(&[
            ("b.png", b"BB"),
            ("a.jpg", b"AAAA"),
            ("notes.txt", b"text"),
            ("sub/c.webp", b"CCC"),
        ]);
        let path = write_file("list", "tar", &tar);
        let src = open_tar(&path).unwrap();
        assert_eq!(src.len(), 3, "the .txt is excluded");
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["a.jpg", "b.png", "sub/c.webp"]);
        assert_eq!(src.bytes(0).unwrap(), b"AAAA");
        assert_eq!(src.bytes(1).unwrap(), b"BB");
        assert_eq!(src.bytes(2).unwrap(), b"CCC");
        assert!(src.bytes(99).is_err(), "out-of-range read errors");
        assert_eq!(src.name(99), "", "out-of-range name is empty");
        assert_eq!(src.size_hint(0), Some(4));
        assert!(src.path(0).is_none(), "archive entries have no fs path");
        assert_eq!(src.container(), Some(path.as_path()));
        assert!(src.random_access(), "plain tar is lazy random-access");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tar_open_reports_progress_and_honors_cancel() {
        let tar = tar_bytes(&[("a.jpg", &[1u8; 4000]), ("b.png", &[2u8; 3000])]);
        let path = write_file("lazyprog", "tar", &tar);
        let progress = OpenProgress::new();
        let src = TarSource::open_tar(&path, is_img, Some(&progress)).unwrap();
        assert_eq!(src.len(), 2);
        assert_eq!(progress.total(), tar.len() as u64);
        assert_eq!(progress.done(), progress.total(), "index walk completes");

        let cancelled = OpenProgress::new();
        cancelled.request_cancel();
        match TarSource::open_tar(&path, is_img, Some(&cancelled)) {
            Err(OpenError::Cancelled) => {}
            other => panic!("expected Cancelled, got {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tar_skips_non_regular_entries_and_oversized_ones() {
        let mut b = tar::Builder::new(Vec::new());
        append_file(&mut b, "a.jpg", b"A");
        // A symlink pointing at the image: an aliasing hazard, never an image.
        let mut h = tar::Header::new_gnu();
        h.set_size(0);
        h.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut h, "link.jpg", "a.jpg").unwrap();
        // A directory entry.
        let mut h = tar::Header::new_gnu();
        h.set_size(0);
        h.set_mode(0o755);
        h.set_entry_type(tar::EntryType::Directory);
        b.append_data(&mut h, "sub/", io::empty()).unwrap();
        // A supported-extension entry over the (injected) ceiling.
        append_file(&mut b, "huge.jpg", &[7u8; 64]);
        let path = write_file("skip", "tar", &b.into_inner().unwrap());

        let limits = OpenLimits {
            ceiling: 32,
            ..OpenLimits::default()
        };
        let src = TarSource::open_tar_with_limits(&path, is_img, None, &limits).unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(
            names,
            vec!["a.jpg"],
            "symlink, directory, and over-ceiling entries are not indexed"
        );
        // The oversized file still exists in the sibling listing (it is a real
        // file in the directory, like ZIP's unindexed entries).
        assert!(src.sibling_names(0).iter().any(|n| n == "huge.jpg"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tar_duplicate_names_keep_the_last_occurrence() {
        // tar append mode: the same name twice; `tar -x` extracts the second.
        let tar = tar_bytes(&[("a.jpg", b"old"), ("a.jpg", b"new")]);
        let path = write_file("dup", "tar", &tar);
        let src = open_tar(&path).unwrap();
        assert_eq!(src.len(), 1, "one item, not two");
        assert_eq!(src.bytes(0).unwrap(), b"new", "last occurrence wins");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tar_skips_traversal_shaped_names() {
        // Names with `..` components are skipped outright (defense in depth —
        // nothing here ever resolves them to a path, but nothing should record
        // them either). The tar Builder itself refuses `..`, so craft the
        // header directly.
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(4);
        h.set_mode(0o644);
        h.as_gnu_mut().unwrap().name[..12].copy_from_slice(b"../evil.jpg\0");
        h.set_cksum();
        b.append(&h, &b"EVIL"[..]).unwrap();
        append_file(&mut b, "ok.jpg", b"OK");
        let path = write_file("traversal", "tar", &b.into_inner().unwrap());
        let src = open_tar(&path).unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["ok.jpg"], "the ..-name entry is not recorded");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tar_with_a_truncated_last_payload_serves_the_intact_entries() {
        let tar = tar_bytes(&[("a.jpg", b"AAAA"), ("zz.png", &[5u8; 1000])]);
        // Layout: hdr@0, data@512(4→512), hdr@1024, data@1536(1000→1024), end
        // marker @2560 — 3584 total. Assert it so builder drift fails loudly.
        assert_eq!(tar.len(), 3584, "fixture layout drifted");
        // Truncate at a 512 block boundary inside the last entry's data: the
        // iterator ends cleanly at EOF, and the entry whose (offset, size) runs
        // past EOF is skipped at index time; the intact entry stays viewable.
        let path = write_file("trunctail", "tar", &tar[..2048]);
        let src = open_tar(&path).unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["a.jpg"], "truncated tail entry is not indexed");
        assert_eq!(src.bytes(0).unwrap(), b"AAAA");
        let _ = std::fs::remove_file(&path);

        // A mid-block truncation leaves a partial header/block: the iterator
        // errors and the open reports the archive damaged — also honest.
        let ragged = write_file("truncmid", "tar", &tar[..tar.len() - 700]);
        match open_tar(&ragged) {
            Err(OpenError::Corrupt(_)) => {}
            other => panic!("expected Corrupt, got {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&ragged);
    }

    #[test]
    fn tar_refuses_an_entry_count_bomb() {
        let files: Vec<(String, Vec<u8>)> =
            (0..10).map(|i| (format!("f{i}.txt"), Vec::new())).collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let path = write_file("countbomb", "tar", &tar_bytes(&refs));
        let limits = OpenLimits {
            max_entries: 5,
            ..OpenLimits::default()
        };
        match TarSource::open_tar_with_limits(&path, is_img, None, &limits) {
            Err(OpenError::Corrupt(msg)) => assert!(msg.contains("too many"), "{msg}"),
            other => panic!("expected Corrupt, got {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tar_with_no_supported_entries_is_empty() {
        let tar = tar_bytes(&[("readme.txt", b"x"), ("data.bin", b"y")]);
        let path = write_file("none", "tar", &tar);
        let src = open_tar(&path).unwrap();
        assert!(src.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_rejects_a_non_tar_file() {
        let path = write_file("bogus", "tar", b"this is definitely not a tar");
        match open_tar(&path) {
            Err(OpenError::Corrupt(_)) => {}
            Err(other) => panic!("expected Corrupt, got {other:?}"),
            Ok(_) => panic!("expected an error opening a non-tar file"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_missing_file_is_an_io_error() {
        let missing = temp_path("missing", "tar");
        match open_tar(&missing) {
            Err(OpenError::Io(_)) => {}
            Err(other) => panic!("expected Io, got {other:?}"),
            Ok(_) => panic!("expected an error opening a missing file"),
        }
    }

    /// Sidecar siblings (task #90.1), lazy tar: names scoped to the item's own
    /// archive folder, unindexed entries visible, cross-folder reads refused.
    #[test]
    fn tar_siblings_reach_unindexed_entries_scoped_to_the_folder() {
        let tar = tar_bytes(&[
            ("S1/movie.mkv", b"vid"),
            ("S1/movie.en.srt", b"s1 subs"),
            ("S1/a.jpg", b"img"),
            ("S2/other.en.srt", b"s2 subs"),
            ("root.jpg", b"img"),
        ]);
        let path = write_file("sib", "tar", &tar);
        let src = open_tar(&path).unwrap();
        let s1 = (0..src.len()).find(|&i| src.name(i) == "S1/a.jpg").unwrap();

        let mut names = src.sibling_names(s1);
        names.sort();
        assert_eq!(names, vec!["S1/a.jpg", "S1/movie.en.srt", "S1/movie.mkv"]);
        assert_eq!(
            src.sibling_bytes(s1, "S1/movie.en.srt").unwrap(),
            b"s1 subs"
        );
        assert!(
            src.sibling_bytes(s1, "S2/other.en.srt").is_err(),
            "a read outside the item's folder is refused"
        );
        assert!(
            src.sibling_bytes(s1, "S1/nope.srt").is_err(),
            "an unknown sibling name is NotFound"
        );
        assert!(src.sibling_names(99).is_empty(), "out-of-range is empty");
        let _ = std::fs::remove_file(&path);
    }

    /// The decode pool reads `bytes(i)` from many workers at once; each open +
    /// seek + read must be independent.
    #[test]
    fn tar_concurrent_reads_return_correct_bytes() {
        use std::sync::Arc;
        let files: Vec<(String, Vec<u8>)> = (0..8)
            .map(|i| (format!("img{i:02}.png"), vec![i as u8; 32 + i]))
            .collect();
        let refs: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(n, b)| (n.as_str(), b.as_slice()))
            .collect();
        let path = write_file("concurrent", "tar", &tar_bytes(&refs));
        let src = Arc::new(open_tar(&path).unwrap());
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
        let _ = std::fs::remove_file(&path);
    }

    // ── eager (compressed tarballs) ──────────────────────────────────────

    #[test]
    fn compressed_tars_round_trip_across_all_codecs() {
        let tar = tar_bytes(&[
            ("b.png", b"BBBB"),
            ("a.jpg", b"AA"),
            ("notes.txt", b"text"),
            ("sub/c.webp", b"CCCCC"),
        ]);
        for (kind, ext) in EAGER_KINDS {
            let path = write_file("rt", ext, &compress(kind, &tar));
            let src = TarSource::open_compressed(&path, kind, is_img, None, u64::MAX)
                .unwrap_or_else(|e| panic!("{ext}: {e:?}"));
            let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
            assert_eq!(names, vec!["a.jpg", "b.png", "sub/c.webp"], "{ext}");
            assert_eq!(src.bytes(0).unwrap(), b"AA", "{ext}");
            assert_eq!(src.bytes(1).unwrap(), b"BBBB", "{ext}");
            assert_eq!(src.bytes(2).unwrap(), b"CCCCC", "{ext}");
            assert_eq!(src.size_hint(2), Some(5), "{ext}");
            assert_eq!(src.container(), Some(path.as_path()), "{ext}");
            // Eager sources deliberately report no siblings (the 7z gap).
            assert!(src.sibling_names(0).is_empty(), "{ext}");
            assert!(src.sibling_bytes(0, "notes.txt").is_err(), "{ext}");
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Concatenated compressed members (pigz / pbzip2 / pzstd / multi-stream xz
    /// output) must decode whole — the single-member decoders would silently
    /// stop at the first boundary and truncate the archive.
    #[test]
    fn concatenated_streams_decode_across_the_member_boundary() {
        let tar = tar_bytes(&[("a.jpg", &[3u8; 40_000]), ("b.png", &[5u8; 30_000])]);
        let split = tar.len() / 2;
        for (kind, ext) in EAGER_KINDS {
            let mut joined = compress(kind, &tar[..split]);
            joined.extend(compress(kind, &tar[split..]));
            let path = write_file("multi", ext, &joined);
            let src = TarSource::open_compressed(&path, kind, is_img, None, u64::MAX).unwrap();
            assert_eq!(src.len(), 2, "{ext}");
            assert_eq!(src.bytes(0).unwrap(), vec![3u8; 40_000], "{ext}");
            assert_eq!(src.bytes(1).unwrap(), vec![5u8; 30_000], "{ext}");
            let _ = std::fs::remove_file(&path);
        }
    }

    /// A zstd *skippable* frame (magic 0x184D2A5x) between data frames — pzstd
    /// writes these — must be skipped, not treated as corruption.
    #[test]
    fn zstd_skippable_frames_are_skipped() {
        let tar = tar_bytes(&[("a.jpg", b"AAAA")]);
        let mut bytes = Vec::new();
        // A 12-byte skippable frame up front: magic, 4-byte payload length, payload.
        bytes.extend_from_slice(&0x184D2A50u32.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(b"SKIP");
        bytes.extend(zst(&tar));
        let path = write_file("skipf", "tar.zst", &bytes);
        let src =
            TarSource::open_compressed(&path, ArchiveKind::TarZst, is_img, None, u64::MAX).unwrap();
        assert_eq!(src.len(), 1);
        assert_eq!(src.bytes(0).unwrap(), b"AAAA");
        let _ = std::fs::remove_file(&path);
    }

    /// A hostile frame header demanding a huge window must be refused before
    /// the decoder can allocate it (ruzstd 0.8.3 only caps windows on `reset`,
    /// not on a decoder's first frame).
    #[test]
    fn zstd_huge_declared_window_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0xFD2FB528u32.to_le_bytes());
        bytes.push(0x00); // descriptor: no single-segment, no dict, no checksum
        bytes.push(0xFF); // window descriptor: exponent 31, mantissa 7 → ~3.9 TB
        bytes.extend_from_slice(&[0u8; 32]); // garbage tail
        let path = write_file("hugewin", "tar.zst", &bytes);
        match TarSource::open_compressed(&path, ArchiveKind::TarZst, is_img, None, u64::MAX) {
            Err(OpenError::Corrupt(msg)) => {
                assert!(msg.contains("window"), "{msg}");
            }
            other => panic!("expected Corrupt, got {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// The xz analog: a first block declaring an over-cap LZMA2 dictionary is
    /// refused before `XzReader` can `vec![0; dict]` it.
    #[test]
    fn xz_huge_declared_dictionary_is_refused() {
        // Take a real tiny .xz and patch its dict-size property byte to the
        // 4 GiB encoding (0x28 = value 40). The block-header CRC no longer
        // matches, but the pre-check runs before any CRC validation.
        let real = xz(&tar_bytes(&[("a.jpg", b"AAAA")]));
        let mut patched = real.clone();
        // Stream header = 12 bytes; block header: size byte, flags byte, then
        // (no sizes in our writer's output) filter id 0x21, props len 0x01,
        // dict byte. Verify the shape before patching so a future lzma-rust2
        // layout change fails loudly here.
        assert_eq!(patched[14], 0x21, "expected the LZMA2 filter id");
        assert_eq!(patched[15], 0x01, "expected a 1-byte property");
        patched[16] = 40; // dict = 4 GiB - 1
        let path = write_file("hugedict", "tar.xz", &patched);
        match TarSource::open_compressed(&path, ArchiveKind::TarXz, is_img, None, u64::MAX) {
            Err(OpenError::Corrupt(msg)) => assert!(msg.contains("dictionary"), "{msg}"),
            other => panic!("expected Corrupt, got {:?}", other.err()),
        }
        // The unpatched original still opens (the pre-check passes it through).
        let ok_path = write_file("okdict", "tar.xz", &real);
        let src = TarSource::open_compressed(&ok_path, ArchiveKind::TarXz, is_img, None, u64::MAX)
            .unwrap();
        assert_eq!(src.len(), 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&ok_path);
    }

    /// The metadata bomb (plan rev2 §3): a GNU long-name entry declaring a huge
    /// payload must fail as Corrupt via the metered quota — the `tar` crate
    /// would otherwise `read_to_end` (allocate) all of it inside `next()`.
    #[test]
    fn eager_open_refuses_a_metadata_bomb() {
        // Craft: an 'L' (GNU long name) entry declaring 8 MB of name payload.
        let mut raw = Vec::new();
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::GNULongName);
        h.set_size(8 * 1024 * 1024);
        h.set_mode(0o644);
        h.as_gnu_mut().unwrap().name[..13].copy_from_slice(b"././@LongLink");
        h.set_cksum();
        raw.extend_from_slice(h.as_bytes());
        raw.extend(std::iter::repeat_n(b'a', 8 * 1024 * 1024));
        raw.extend_from_slice(&[0u8; 1024]); // end-of-archive
        let path = write_file("metabomb", "tar.gz", &gz(&raw));
        match TarSource::open_compressed(&path, ArchiveKind::TarGz, is_img, None, u64::MAX) {
            Err(OpenError::Corrupt(msg)) => {
                assert!(msg.contains("metadata"), "quota error expected, got: {msg}");
            }
            other => panic!("expected Corrupt, got {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Header and metadata bytes count toward the work cap too (codex review of
    /// the implementation, P1): a bomb of zero-payload entries expands real
    /// decompressor output that per-entry size accounting never sees.
    #[test]
    fn eager_open_counts_header_bytes_toward_the_work_cap() {
        // Four empty files = 4 x 512-byte headers + the 1024-byte end marker:
        // ~3 KiB of pure header output, zero payload bytes.
        let tar = tar_bytes(&[
            ("a.txt", b""),
            ("b.txt", b""),
            ("c.txt", b""),
            ("d.txt", b""),
        ]);
        let path = write_file("metawork", "tar.gz", &gz(&tar));
        let limits = OpenLimits {
            max_expanded: 1024,
            ..OpenLimits::default()
        };
        match TarSource::open_compressed_with_limits(
            &path,
            ArchiveKind::TarGz,
            is_img,
            None,
            u64::MAX,
            &limits,
        ) {
            Err(OpenError::Corrupt(msg)) => assert!(msg.contains("expands"), "{msg}"),
            other => panic!("expected Corrupt, got {:?}", other.err()),
        }
        // A sane cap admits the same archive (it just has no images).
        let src =
            TarSource::open_compressed(&path, ArchiveKind::TarGz, is_img, None, u64::MAX).unwrap();
        assert!(src.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// A skippable frame whose declared payload runs past EOF is a truncated
    /// file, not a clean end (codex review of the implementation, P2).
    #[test]
    fn zstd_truncated_skippable_frame_is_corrupt() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x184D2A50u32.to_le_bytes());
        bytes.extend_from_slice(&100u32.to_le_bytes()); // declares 100 bytes...
        bytes.extend_from_slice(b"SHORT"); // ...provides 5
        let path = write_file("skiptrunc", "tar.zst", &bytes);
        match TarSource::open_compressed(&path, ArchiveKind::TarZst, is_img, None, u64::MAX) {
            Err(OpenError::Corrupt(msg)) => assert!(msg.contains("skippable"), "{msg}"),
            other => panic!("expected Corrupt, got {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// The work bomb (plan rev2 §5): total decompressed work — including drained
    /// entries that are never kept — is capped.
    #[test]
    fn eager_open_refuses_an_expanded_work_bomb() {
        let tar = tar_bytes(&[("big.bin", &[0u8; 100_000]), ("a.jpg", b"AA")]);
        let path = write_file("workbomb", "tar.gz", &gz(&tar));
        let limits = OpenLimits {
            max_expanded: 50_000,
            ..OpenLimits::default()
        };
        match TarSource::open_compressed_with_limits(
            &path,
            ArchiveKind::TarGz,
            is_img,
            None,
            u64::MAX,
            &limits,
        ) {
            Err(OpenError::Corrupt(msg)) => assert!(msg.contains("expands"), "{msg}"),
            other => panic!("expected Corrupt, got {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn eager_open_reports_progress_to_completion() {
        let tar = tar_bytes(&[("a.jpg", &[1u8; 5000]), ("b.png", &[2u8; 3000])]);
        let compressed = gz(&tar);
        let path = write_file("prog", "tar.gz", &compressed);
        let progress = OpenProgress::new();
        let src = TarSource::open_compressed(
            &path,
            ArchiveKind::TarGz,
            is_img,
            Some(&progress),
            u64::MAX,
        )
        .unwrap();
        assert_eq!(src.len(), 2);
        assert_eq!(
            progress.total(),
            compressed.len() as u64,
            "total is the compressed file length"
        );
        assert_eq!(progress.done(), progress.total(), "done reaches total");
        assert_eq!(progress.fraction(), 1.0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn eager_open_cancels_before_decoding() {
        let tar = tar_bytes(&[("a.jpg", b"AAAA"), ("b.png", b"BBBB")]);
        let path = write_file("cancel", "tar.gz", &gz(&tar));
        let progress = OpenProgress::new();
        progress.request_cancel();
        match TarSource::open_compressed(
            &path,
            ArchiveKind::TarGz,
            is_img,
            Some(&progress),
            u64::MAX,
        ) {
            Err(OpenError::Cancelled) => {}
            other => panic!("expected Cancelled, got {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// The streaming RAM budget: the open refuses (with a structured, "at least"
    /// figure) the moment the next entry would not fit — before reserving.
    #[test]
    fn eager_open_refuses_past_the_budget_with_too_large() {
        let tar = tar_bytes(&[("a.jpg", &[1u8; 100]), ("b.png", &[2u8; 100])]);
        let path = write_file("budget", "tar.gz", &gz(&tar));
        match TarSource::open_compressed(&path, ArchiveKind::TarGz, is_img, None, 150) {
            Err(OpenError::TooLarge { needed, budget }) => {
                assert_eq!(budget, 150);
                assert!(needed >= 200, "needed is a lower bound: {needed}");
            }
            other => panic!("expected TooLarge, got {:?}", other.err()),
        }
        // The same archive passes under a sufficient budget — the budget is the
        // only gate.
        let src = TarSource::open_compressed(&path, ArchiveKind::TarGz, is_img, None, 200).unwrap();
        assert_eq!(src.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    /// A replaced duplicate releases its bytes from the running budget — the
    /// counter tracks resident bytes, not gross stream bytes.
    #[test]
    fn eager_duplicates_keep_the_last_and_release_the_replaced_bytes() {
        let tar = tar_bytes(&[("a.jpg", &[1u8; 100]), ("a.jpg", &[2u8; 100])]);
        let path = write_file("dupe", "tar.gz", &gz(&tar));
        // 100 resident + 100 incoming - 100 replaced = 100 ≤ 150: must fit.
        let src = TarSource::open_compressed(&path, ArchiveKind::TarGz, is_img, None, 150).unwrap();
        assert_eq!(src.len(), 1);
        assert_eq!(
            src.bytes(0).unwrap(),
            vec![2u8; 100],
            "last occurrence wins"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A bomb-shaped entry (huge declared size, unsupported or over the ceiling)
    /// is skipped and drained in cancellable chunks; the rest of the archive
    /// still opens.
    #[test]
    fn eager_open_skips_and_drains_oversized_entries() {
        let tar = tar_bytes(&[
            ("bomb.jpg", &[9u8; 200_000]), // supported ext, over the injected ceiling
            ("data.bin", &[8u8; 100_000]), // unsupported
            ("a.jpg", b"AA"),
        ]);
        let path = write_file("bomb", "tar.gz", &gz(&tar));
        let progress = OpenProgress::new();
        let limits = OpenLimits {
            ceiling: 1000,
            ..OpenLimits::default()
        };
        let src = TarSource::open_compressed_with_limits(
            &path,
            ArchiveKind::TarGz,
            is_img,
            Some(&progress),
            u64::MAX,
            &limits,
        )
        .unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(
            names,
            vec!["a.jpg"],
            "bomb + unsupported skipped, rest kept"
        );
        assert_eq!(
            progress.done(),
            progress.total(),
            "progress still completes"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn eager_open_rejects_a_truncated_stream_as_corrupt() {
        let tar = tar_bytes(&[("a.jpg", &[1u8; 50_000]), ("b.png", &[2u8; 50_000])]);
        let compressed = gz(&tar);
        let path = write_file("trunc", "tar.gz", &compressed[..compressed.len() / 2]);
        match TarSource::open_compressed(&path, ArchiveKind::TarGz, is_img, None, u64::MAX) {
            Err(OpenError::Corrupt(_)) => {}
            Err(other) => panic!("expected Corrupt, got {other:?}"),
            Ok(_) => panic!("expected an error opening a truncated tarball"),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Corruption *after* the tar terminator (a damaged gzip trailer CRC) is
    /// still surfaced — the post-iteration drain forces trailer validation.
    #[test]
    fn eager_open_detects_a_corrupt_trailing_checksum() {
        let tar = tar_bytes(&[("a.jpg", b"AAAA")]);
        let mut compressed = gz(&tar);
        // The gzip member CRC32 is the 8th-from-last..4th-from-last bytes; flip one.
        let at = compressed.len() - 6;
        compressed[at] ^= 0xFF;
        let path = write_file("crc", "tar.gz", &compressed);
        match TarSource::open_compressed(&path, ArchiveKind::TarGz, is_img, None, u64::MAX) {
            Err(OpenError::Corrupt(_)) => {}
            other => panic!("expected Corrupt, got {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn eager_open_rejects_garbage_as_corrupt() {
        for (kind, ext) in EAGER_KINDS {
            let path = write_file("garbage", ext, b"not a compressed stream at all");
            match TarSource::open_compressed(&path, kind, is_img, None, u64::MAX) {
                Err(OpenError::Corrupt(_)) | Err(OpenError::Io(_)) => {}
                Err(other) => panic!("{ext}: expected Corrupt/Io, got {other:?}"),
                Ok(_) => panic!("{ext}: expected an error opening garbage"),
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    /// The ⇧F folder tree drives a ScopedSource over the archive; it must work
    /// over a nested tar exactly as it does over ZIP.
    #[test]
    fn scoped_source_over_a_tar_serves_the_subtree() {
        use crate::ScopedSource;
        use std::sync::Arc;
        let tar = tar_bytes(&[
            ("a/b/one.jpg", b"1"),
            ("a/b/c/two.jpg", b"2"),
            ("a/bc/three.jpg", b"3"),
            ("top.png", b"4"),
        ]);
        let path = write_file("scope", "tar", &tar);
        let full: Arc<dyn ItemSource> = Arc::new(open_tar(&path).unwrap());
        let scoped = ScopedSource::new(Arc::clone(&full), "a/b");
        let names: Vec<&str> = (0..scoped.len()).map(|i| scoped.name(i)).collect();
        assert_eq!(names, vec!["a/b/c/two.jpg", "a/b/one.jpg"]);
        assert_eq!(scoped.bytes(1).unwrap(), b"1");
        let _ = std::fs::remove_file(&path);
    }
}
