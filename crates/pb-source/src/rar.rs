//! [`RarSource`] — RAR5 archives (`.rar`, `.cbr`) behind the `ItemSource` seam
//! (task #103).
//!
//! **The container parser here is ours**; the `compcol` crate supplies only the
//! RAR5 LZ/Huffman codec (pure Rust, MIT, decode-only — audited and
//! differentially validated against `unrar` in the #102 plan's compcol spike;
//! the pinned fork rev carries our x86-filter correctness fix, upstream PR
//! #121). The `fstool` crate's RAR5 reader was the *reference implementation*
//! for the block-chain scan and the solid-group decode, deliberately not a
//! dependency (monolithic, no CRC checks, single-threaded reader model).
//!
//! Two access models, decided **per archive** by its solid layout:
//!
//! * **Non-solid members decode lazily** (ZIP's shape). The scan walks the
//!   vint-encoded block chain once — headers only — recording each file's
//!   packed run. `bytes(i)` seeks to the run and decodes it independently.
//! * **Solid groups decode eagerly at open** (7z's shape). A solid group's
//!   members share one continuous LZ window, so there is no cheap per-member
//!   access; the open decodes each group once, keeping supported members
//!   resident (RAM-budgeted, cancellable, with progress) and discarding the
//!   rest. (fstool's forward-cursor `LiveSolid` is the optimization to reach
//!   for if eager cost ever hurts on a real archive.)
//!
//! **What we add that neither compcol nor fstool do** (plan #103 §2):
//!
//! * **CRC32 verification** of every decoded entry against the header CRC —
//!   both upstreams skip it, which turns corruption into silent garbage.
//! * **A window cap** (64 MiB) *before* constructing a decoder — a hostile
//!   header can demand a 1 GiB window.
//! * **Encryption detection at the container layer** (the archive-encryption
//!   header and per-file encryption records) → an honest "password protected,
//!   not supported" message. Never [`OpenError::PasswordRequired`]: that routes
//!   to the password prompt, and no password would help — we do not decrypt.
//!   fstool's failure mode here (feed ciphertext to the decoder, report
//!   "corrupt") is exactly what this avoids.
//! * **Solid-group degradation**: a member the codec refuses (the Delta/ARM
//!   filters — WinRAR auto-picks Delta for BMP/RAW-shaped content) marks that
//!   member *and the rest of its group* unavailable with an honest per-entry
//!   error; the archive still opens and every other group still serves.
//!   (JPEGs are not delta-filtered — measured in the spike — so the common
//!   photo case decodes.)
//!
//! Out of scope, detected and refused honestly: RAR4 (a different container
//! *and* codec — the "20-year-old archive" case waits on upstream compcol),
//! multi-volume sets, encrypted archives. Stored members inside a solid group
//! are unavailable (they sit outside the LZ bitstream; decoding around them
//! would desync — fstool's rule, kept).
//!
//! **Privacy:** RAM-only, read-only, never extracted to disk — same as every
//! source in this crate.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use compcol::Decoder as _;

use crate::tar_source::{sane_name, OpenLimits};
use crate::{
    ext_of, normalize_entry_name, out_of_range, ItemSource, OpenError, OpenProgress,
    MAX_ENTRY_BYTES,
};

/// Ceiling on the LZ window allocated from a header's dict bits. RAR5 headers
/// can demand up to 1 GiB (and RAR7's extended dict bits far more); WinRAR's
/// defaults are 32-64 MiB. Same figure as the fstool reference.
const MAX_WINDOW: usize = 64 * 1024 * 1024;
/// Ceiling on one block header's declared size (a vint — a hostile value could
/// otherwise demand a giant read). Real headers are well under 4 KiB even with
/// extra areas; 1 MiB is generous for pathological-but-honest name lengths.
const MAX_HEADER: u64 = 1 << 20;
/// Output staging chunk for solid decode — also the cancel-check granularity.
const CHUNK: usize = 64 * 1024;

// RAR5 header types.
const HEAD_MAIN: u64 = 1;
const HEAD_FILE: u64 = 2;
const HEAD_CRYPT: u64 = 4;
const HEAD_END: u64 = 5;
// Common header flags.
const HFLAG_EXTRA: u64 = 0x01;
const HFLAG_DATA: u64 = 0x02;
// Main-archive flags.
const AFLAG_VOLUME: u64 = 0x01;
// File flags.
const FFLAG_DIR: u64 = 0x01;
const FFLAG_MTIME: u64 = 0x02;
const FFLAG_CRC: u64 = 0x04;
// File extra-area record types.
const XREC_ENCRYPTION: u64 = 0x01;

/// User-facing refusal lines (plain copy, reused by tests).
const MSG_RAR4: &str =
    "This is an older RAR4 archive, which is not supported yet. Only RAR5 archives open.";
const MSG_ENCRYPTED: &str =
    "This RAR archive is password protected, which is not supported for RAR yet.";
const MSG_VOLUME: &str = "Multi-volume RAR archives are not supported yet.";

/// Why an entry's bytes cannot be produced (per-entry honest errors).
const UNAVAIL_ENCRYPTED: &str = "this entry is password protected, which is not supported for RAR";
const UNAVAIL_STORED_SOLID: &str =
    "this entry is stored inside a solid group, which cannot be unpacked reliably";
const UNAVAIL_FILTER: &str =
    "this entry (or one before it in its solid group) uses a RAR feature that is not supported yet";
const UNAVAIL_DAMAGED: &str = "this entry is damaged (checksum mismatch)";

/// Where an entry's bytes come from.
enum EntryData {
    /// Non-solid: decode independently on demand (open + seek + decode).
    Lazy {
        offset: u64,
        pack: u64,
        window: usize,
        store: bool,
    },
    /// Solid-group member, decoded at open.
    Resident(Vec<u8>),
    /// Cannot be produced; `bytes(i)` reports the reason.
    Unavailable(&'static str),
}

struct RarEntry {
    /// Normalized archive-relative name.
    name: String,
    /// Declared decompressed size.
    unpack: u64,
    /// Header CRC32 of the decompressed bytes, when the header carried one.
    crc: Option<u32>,
    data: EntryData,
}

/// A RAR5 archive as an [`ItemSource`]. See the module docs.
pub struct RarSource {
    path: PathBuf,
    /// Recorded regular files, duplicate names last-wins, sorted by name.
    entries: Vec<RarEntry>,
    /// Item index → `entries` index for the supported, within-ceiling entries.
    items: Vec<usize>,
}

/// One file member as collected during the block-chain scan.
struct Member {
    name: String,
    data_offset: u64,
    pack: u64,
    unpack: u64,
    crc: Option<u32>,
    method: u64,
    window: usize,
    encrypted: bool,
}

impl RarSource {
    /// Open `path` as a RAR5 archive: scan the block chain (headers only),
    /// then eagerly decode any solid groups (RAM-budgeted against `budget`,
    /// cancellable and reporting decode progress through `progress`).
    /// `is_supported` is the entry predicate (the app passes its image+video
    /// union). RAR has no last-resort sync path concerns — the app always opens
    /// it off-thread ([`crate::ArchiveKind::background_open`]).
    pub fn open(
        path: impl Into<PathBuf>,
        is_supported: impl Fn(&str) -> bool,
        progress: Option<&OpenProgress>,
        budget: u64,
    ) -> Result<Self, OpenError> {
        Self::open_with_limits(path, is_supported, progress, budget, &OpenLimits::default())
    }

    pub(crate) fn open_with_limits(
        path: impl Into<PathBuf>,
        is_supported: impl Fn(&str) -> bool,
        progress: Option<&OpenProgress>,
        budget: u64,
        limits: &OpenLimits,
    ) -> Result<Self, OpenError> {
        let path = path.into();
        let file = File::open(&path)?;
        let file_len = file.metadata()?.len();
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let (entries, items) = scan_and_load(
            &mut reader,
            file_len,
            &is_supported,
            progress,
            budget,
            limits,
        )?;
        // An archive whose every viewable item is unavailable for one shared
        // reason (all encrypted, all stored-in-solid) reads better as a single
        // honest refusal than as a deck of N items that all fail to decode.
        if !items.is_empty() {
            let all_unavailable = items
                .iter()
                .all(|&j| matches!(entries[j].data, EntryData::Unavailable(_)));
            if all_unavailable {
                if let EntryData::Unavailable(why) = entries[items[0]].data {
                    if why == UNAVAIL_ENCRYPTED {
                        return Err(OpenError::Unsupported(MSG_ENCRYPTED.into()));
                    }
                    return Err(OpenError::Unsupported(format!(
                        "This RAR archive cannot be shown: {why}."
                    )));
                }
            }
        }
        Ok(Self {
            path,
            entries,
            items,
        })
    }

    /// Decode a lazy (non-solid) entry: open + seek + bounded streaming decode
    /// with CRC verification. Independent per call — the decode pool reads
    /// from many workers at once.
    fn read_lazy(
        &self,
        e: &RarEntry,
        offset: u64,
        pack: u64,
        window: usize,
        store: bool,
    ) -> io::Result<Vec<u8>> {
        if e.unpack > MAX_ENTRY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive entry too large",
            ));
        }
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(offset))?;
        let run = BufReader::with_capacity(1 << 16, file).take(pack);
        let mut buf = Vec::new();
        buf.try_reserve_exact(e.unpack as usize).map_err(|_| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "archive entry too large to allocate",
            )
        })?;
        if store {
            // Stored verbatim: the packed run IS the bytes.
            run.take(e.unpack).read_to_end(&mut buf)?;
        } else {
            let dec = compcol::rar5::Decoder::with_unpack_size_and_window(e.unpack, window);
            // Cap at the declared size so a lying stream cannot inflate past it.
            compcol::io::DecoderReader::new(run, dec)
                .take(e.unpack)
                .read_to_end(&mut buf)
                .map_err(remap_codec_refusal)?;
        }
        if buf.len() as u64 != e.unpack {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "archive entry truncated",
            ));
        }
        verify_crc(e, &buf).map_err(|why| io::Error::new(io::ErrorKind::InvalidData, why))?;
        Ok(buf)
    }
}

impl ItemSource for RarSource {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn name(&self, i: usize) -> &str {
        self.items
            .get(i)
            .map_or("", |&j| self.entries[j].name.as_str())
    }

    fn container(&self) -> Option<&Path> {
        Some(&self.path)
    }

    fn bytes(&self, i: usize) -> io::Result<Vec<u8>> {
        let &j = self.items.get(i).ok_or_else(out_of_range)?;
        let e = &self.entries[j];
        match &e.data {
            EntryData::Lazy {
                offset,
                pack,
                window,
                store,
            } => self.read_lazy(e, *offset, *pack, *window, *store),
            EntryData::Resident(bytes) => Ok(bytes.clone()),
            EntryData::Unavailable(why) => Err(io::Error::new(io::ErrorKind::Unsupported, *why)),
        }
    }

    fn size_hint(&self, i: usize) -> Option<u64> {
        self.items.get(i).map(|&j| self.entries[j].unpack)
    }

    // `sibling_names`/`sibling_bytes` keep the default (none): a solid member's
    // sidecar was never decoded (the 7z gap, same reasoning), and mixing
    // "siblings work for non-solid entries only" would be a trap. Small
    // follow-up behind the trait default if it ever matters.
}

/// The CRC stored for `e`, checked against the decoded bytes. RAR5 uses plain
/// CRC32; entries without a stored CRC (rare) pass.
fn verify_crc(e: &RarEntry, bytes: &[u8]) -> Result<(), &'static str> {
    verify_crc_raw(e.crc, bytes)
}

impl RarEntry {
    fn new(name: String, unpack: u64, crc: Option<u32>, data: EntryData) -> Self {
        RarEntry {
            name,
            unpack,
            crc,
            data,
        }
    }
}

/// Read a RAR5 vint (base-128 LE, high bit = continue) from `buf` at `pos`.
fn read_vint(buf: &[u8], pos: usize) -> Option<(u64, usize)> {
    let mut val = 0u64;
    let mut shift = 0u32;
    let mut i = pos;
    loop {
        let b = *buf.get(i)?;
        i += 1;
        val |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some((val, i - pos));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

/// Cursor over one header's bytes.
struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn vint(&mut self) -> Result<u64, OpenError> {
        let (v, n) = read_vint(self.b, self.p)
            .ok_or_else(|| OpenError::Corrupt("truncated RAR header field".into()))?;
        self.p += n;
        Ok(v)
    }
    fn u32le(&mut self) -> Result<u32, OpenError> {
        let end = self.p + 4;
        let s = self
            .b
            .get(self.p..end)
            .ok_or_else(|| OpenError::Corrupt("truncated RAR header field".into()))?;
        self.p = end;
        Ok(u32::from_le_bytes(s.try_into().expect("4-byte slice")))
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], OpenError> {
        let end = self.p.checked_add(n).filter(|&e| e <= self.b.len());
        let end = end.ok_or_else(|| OpenError::Corrupt("truncated RAR header field".into()))?;
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }
}

/// Whether a file header's extra area carries an encryption record.
fn extra_has_encryption(extra: &[u8]) -> bool {
    let mut p = 0usize;
    while p < extra.len() {
        let Some((rec_size, n)) = read_vint(extra, p) else {
            return false;
        };
        p += n;
        let Some((rec_type, _)) = read_vint(extra, p) else {
            return false;
        };
        if rec_type == XREC_ENCRYPTION {
            return true;
        }
        let Some(next) = p.checked_add(rec_size as usize) else {
            return false;
        };
        if next <= p {
            return false; // zero-size record: refuse to spin
        }
        p = next;
    }
    false
}

/// Scan the block chain, then eagerly decode solid groups. Split from
/// [`RarSource::open`] over a generic reader so the fuzz harness drives it on
/// raw bytes.
fn scan_and_load<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    is_supported: &dyn Fn(&str) -> bool,
    progress: Option<&OpenProgress>,
    budget: u64,
    limits: &OpenLimits,
) -> Result<(Vec<RarEntry>, Vec<usize>), OpenError> {
    // Signature: RAR5 is `Rar!\x1A\x07\x01\x00`; RAR4 is `Rar!\x1A\x07\x00`.
    let mut sig = [0u8; 8];
    let got = read_fully(reader, &mut sig)?;
    if got < 7 || sig[..6] != *b"Rar!\x1a\x07" {
        return Err(OpenError::Corrupt("not a RAR archive".into()));
    }
    if sig[6] == 0x00 {
        return Err(OpenError::Unsupported(MSG_RAR4.into()));
    }
    if sig[6] != 0x01 || got < 8 {
        return Err(OpenError::Unsupported(
            "This RAR archive uses a format version that is not supported yet.".into(),
        ));
    }

    // ── Pass 1: walk the headers (no data reads — data runs are seeked over).
    let mut groups: Vec<Vec<Member>> = Vec::new();
    let mut walked = 0usize;
    let mut name_bytes = 0u64;
    let mut pos: u64 = 8;
    while pos + 5 <= file_len {
        reader.seek(SeekFrom::Start(pos))?;
        // CRC32 (4) + HeaderSize vint (≤ 3 bytes for our cap) + header.
        let mut pre = [0u8; 8];
        let pre_got = read_fully(reader, &mut pre)?;
        let Some((head_size, hs_len)) = read_vint(&pre[..pre_got], 4) else {
            break; // ragged tail: serve what was scanned
        };
        if head_size > MAX_HEADER {
            return Err(OpenError::Corrupt("RAR header too large".into()));
        }
        let header_start = pos + 4 + hs_len as u64;
        let header_end = header_start + head_size;
        if header_end > file_len {
            break; // truncated final header: serve what was scanned
        }
        let mut hdr = vec![0u8; head_size as usize];
        reader.seek(SeekFrom::Start(header_start))?;
        if read_fully(reader, &mut hdr)? != hdr.len() {
            break;
        }
        let mut c = Cur { b: &hdr, p: 0 };
        let htype = c.vint()?;
        let hflags = c.vint()?;
        let extra_size = if hflags & HFLAG_EXTRA != 0 {
            c.vint()?
        } else {
            0
        };
        let data_size = if hflags & HFLAG_DATA != 0 {
            c.vint()?
        } else {
            0
        };
        if extra_size > head_size {
            return Err(OpenError::Corrupt("RAR extra area overruns header".into()));
        }

        match htype {
            HEAD_END => break,
            HEAD_CRYPT => return Err(OpenError::Unsupported(MSG_ENCRYPTED.into())),
            HEAD_MAIN => {
                let aflags = c.vint()?;
                if aflags & AFLAG_VOLUME != 0 {
                    return Err(OpenError::Unsupported(MSG_VOLUME.into()));
                }
            }
            HEAD_FILE => {
                walked += 1;
                if walked > limits.max_entries {
                    return Err(OpenError::Corrupt(
                        "the archive has too many entries".into(),
                    ));
                }
                let fflags = c.vint()?;
                let unpack = c.vint()?;
                let _attributes = c.vint()?;
                if fflags & FFLAG_MTIME != 0 {
                    c.u32le()?;
                }
                let crc = if fflags & FFLAG_CRC != 0 {
                    Some(c.u32le()?)
                } else {
                    None
                };
                let comp = c.vint()?;
                let _host_os = c.vint()?;
                let name_len = c.vint()? as usize;
                let raw_name = c.bytes(name_len)?;
                let name = normalize_entry_name(&String::from_utf8_lossy(raw_name));
                let is_dir = fflags & FFLAG_DIR != 0;
                let extra = &hdr[(head_size - extra_size) as usize..];
                let encrypted = extra_size > 0 && extra_has_encryption(extra);

                if !is_dir && sane_name(&name) {
                    name_bytes += name.len() as u64;
                    if name_bytes > limits.max_name_bytes {
                        return Err(OpenError::Corrupt(
                            "the archive has too many entries".into(),
                        ));
                    }
                    // Compression info: bit 6 solid, bits 7..=9 method,
                    // bits 10..=13 dict (window = 128 KiB << N). The cap keeps
                    // a hostile dict_n from demanding a giant allocation; the
                    // file-length bound keeps tiny archives cheap.
                    let solid = comp & 0x40 != 0;
                    let method = (comp >> 7) & 0x7;
                    let dict_n = (comp >> 10) & 0xf;
                    let window = 0x20000u64
                        .checked_shl(dict_n as u32)
                        .map(|w| w as usize)
                        .unwrap_or(MAX_WINDOW)
                        .min(MAX_WINDOW)
                        .min((file_len as usize).max(0x20000));
                    // Data must actually fit the file (a truncated tail entry
                    // can never decode — skip it, keep the archive viewable).
                    if header_end.saturating_add(data_size) <= file_len {
                        if !solid || groups.is_empty() {
                            groups.push(Vec::new());
                        }
                        groups.last_mut().expect("just pushed").push(Member {
                            name,
                            data_offset: header_end,
                            pack: data_size,
                            unpack,
                            crc,
                            method,
                            window,
                            encrypted,
                        });
                    }
                }
            }
            _ => {} // service/unknown headers: skip via data_size
        }

        let Some(next) = header_end.checked_add(data_size) else {
            return Err(OpenError::Corrupt("RAR block overruns the file".into()));
        };
        if next <= pos {
            return Err(OpenError::Corrupt(
                "RAR block chain does not advance".into(),
            ));
        }
        pos = next;
    }

    // ── Pass 2: resolve groups. Non-solid members go lazy; solid groups are
    // decoded now (this is the slow, budgeted, cancellable part).
    let solid_work: u64 = groups
        .iter()
        .filter(|g| g.len() > 1)
        .flat_map(|g| g.iter())
        .map(|m| m.unpack)
        .sum();
    if solid_work > limits.max_expanded {
        return Err(OpenError::Corrupt(
            "the archive expands past the sanity limit".into(),
        ));
    }
    if let Some(p) = progress {
        p.set_total(solid_work);
    }

    // Last-wins by name (an updated archive can carry the same name twice).
    let mut latest: BTreeMap<String, (u64, Option<u32>, EntryData)> = BTreeMap::new();
    let mut resident = 0u64;
    for members in groups {
        if members.len() == 1 {
            let m = members.into_iter().next().expect("len checked");
            let data = if m.encrypted {
                EntryData::Unavailable(UNAVAIL_ENCRYPTED)
            } else {
                EntryData::Lazy {
                    offset: m.data_offset,
                    pack: m.pack,
                    window: m.window,
                    store: m.method == 0,
                }
            };
            latest.insert(m.name, (m.unpack, m.crc, data));
            continue;
        }
        // Multi-member solid group.
        let poisoned: Option<&'static str> = if members.iter().any(|m| m.encrypted) {
            Some(UNAVAIL_ENCRYPTED)
        } else if members.iter().any(|m| m.method == 0) {
            // A stored member sits outside the LZ bitstream; decoding around
            // it would desync the shared window (fstool's rule, kept).
            Some(UNAVAIL_STORED_SOLID)
        } else {
            None
        };
        if let Some(why) = poisoned {
            for m in members {
                latest.insert(m.name, (m.unpack, m.crc, EntryData::Unavailable(why)));
            }
            continue;
        }
        decode_solid_group(
            reader,
            members,
            is_supported,
            progress,
            budget,
            limits,
            &mut resident,
            &mut latest,
        )?;
    }

    let entries: Vec<RarEntry> = latest
        .into_iter()
        .map(|(name, (unpack, crc, data))| RarEntry::new(name, unpack, crc, data))
        .collect();
    let items = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| is_supported(&ext_of(&e.name)) && e.unpack <= limits.ceiling)
        .map(|(i, _)| i)
        .collect();
    Ok((entries, items))
}

/// Decode one multi-member solid group: a single resumable decoder over the
/// concatenation of the members' packed runs, keeping supported members
/// resident and discarding the rest. A member the codec refuses (an
/// unsupported filter) marks itself and everything after it in the group
/// unavailable — the shared window means nothing later can be trusted — but
/// never fails the archive.
#[allow(clippy::too_many_arguments)]
fn decode_solid_group<R: Read + Seek>(
    reader: &mut R,
    members: Vec<Member>,
    is_supported: &dyn Fn(&str) -> bool,
    progress: Option<&OpenProgress>,
    budget: u64,
    limits: &OpenLimits,
    resident: &mut u64,
    latest: &mut BTreeMap<String, (u64, Option<u32>, EntryData)>,
) -> Result<(), OpenError> {
    // The shared window must fit every member's declaration.
    let window = members.iter().map(|m| m.window).max().unwrap_or(0x20000);
    let total: u64 = members.iter().map(|m| m.unpack).sum();
    let mut dec = compcol::rar5::Decoder::with_unpack_size_and_window(total, window);
    // Known limitation of the pinned compcol rev: the x86 filter computes call
    // targets relative to the solid *stream*, but unrar computes them relative
    // to the containing *file* — so an x86-filtered member after the first in
    // a solid group decodes with wrong call-target bytes. Our CRC check
    // catches it (per-entry "damaged", archive still opens), and x86 filters
    // never apply to photos. The in-flight upstream `rar3-standard-filters`
    // work adds `Decoder::add_file_boundary` (and Delta!) — register member
    // offsets here when the pin advances past it.

    // Compressed-input cursor over the members' packed runs, in order.
    let areas: Vec<(u64, u64)> = members.iter().map(|m| (m.data_offset, m.pack)).collect();
    let mut in_area = 0usize;
    let mut in_off = 0u64;
    let mut in_buf = vec![0u8; CHUNK];
    let mut in_filled = 0usize;
    let mut in_consumed = 0usize;
    let refill = |in_area: &mut usize,
                  in_off: &mut u64,
                  in_buf: &mut [u8],
                  reader: &mut R|
     -> Result<usize, OpenError> {
        while *in_area < areas.len() && *in_off >= areas[*in_area].1 {
            *in_area += 1;
            *in_off = 0;
        }
        if *in_area >= areas.len() {
            return Ok(0);
        }
        let (offset, pack) = areas[*in_area];
        let want = (in_buf.len() as u64).min(pack - *in_off) as usize;
        reader.seek(SeekFrom::Start(offset + *in_off))?;
        let got = read_fully(reader, &mut in_buf[..want])?;
        if got == 0 {
            return Err(OpenError::Corrupt("RAR data run truncated".into()));
        }
        *in_off += got as u64;
        Ok(got)
    };

    let cancel = || progress.is_some_and(|p| p.is_cancelled());
    // Decode member by member. On an unsupported-feature refusal, mark the
    // rest of the group unavailable and stop decoding it.
    let mut give_up_at: Option<(usize, &'static str)> = None;
    let mut out_chunk = vec![0u8; CHUNK];
    'members: for (mi, m) in members.iter().enumerate() {
        let keep = is_supported(&ext_of(&m.name)) && m.unpack <= limits.ceiling;
        let mut buf = Vec::new();
        if keep {
            let needed = (*resident).saturating_add(m.unpack);
            if needed > budget {
                return Err(OpenError::TooLarge { needed, budget });
            }
            buf.try_reserve_exact(m.unpack as usize)
                .map_err(|_| OpenError::OutOfMemory)?;
        }
        let mut produced = 0u64;
        while produced < m.unpack {
            if cancel() {
                return Err(OpenError::Cancelled);
            }
            if in_consumed >= in_filled {
                in_filled = refill(&mut in_area, &mut in_off, &mut in_buf, reader)?;
                in_consumed = 0;
            }
            let want = (m.unpack - produced).min(CHUNK as u64) as usize;
            let step = if keep {
                dec.decode(&in_buf[in_consumed..in_filled], &mut out_chunk[..want])
            } else {
                dec.discard_output(&in_buf[in_consumed..in_filled], want)
            };
            let (p, _status) = match step {
                Ok(v) => v,
                Err(compcol::Error::Unsupported) => {
                    give_up_at = Some((mi, UNAVAIL_FILTER));
                    break 'members;
                }
                Err(e) => return Err(OpenError::Corrupt(format!("RAR decode failed: {e}"))),
            };
            in_consumed += p.consumed;
            if keep && p.written > 0 {
                buf.extend_from_slice(&out_chunk[..p.written]);
            }
            produced += p.written as u64;
            if let Some(pr) = progress {
                pr.add_done(p.written as u64);
            }
            if p.consumed == 0 && p.written == 0 {
                if in_filled == 0 {
                    // No compressed input left: flush the decoder's tail.
                    let (pf, _s) = match dec.finish(&mut out_chunk[..want]) {
                        Ok(v) => v,
                        Err(compcol::Error::Unsupported) => {
                            give_up_at = Some((mi, UNAVAIL_FILTER));
                            break 'members;
                        }
                        Err(e) => {
                            return Err(OpenError::Corrupt(format!("RAR decode failed: {e}")))
                        }
                    };
                    if pf.written == 0 {
                        return Err(OpenError::Corrupt(
                            "RAR solid stream ended before its declared size".into(),
                        ));
                    }
                    if keep {
                        buf.extend_from_slice(&out_chunk[..pf.written]);
                    }
                    produced += pf.written as u64;
                    if let Some(pr) = progress {
                        pr.add_done(pf.written as u64);
                    }
                } else if in_consumed >= in_filled {
                    continue; // buffered an incomplete block; refill next loop
                } else {
                    return Err(OpenError::Corrupt("RAR solid decode stalled".into()));
                }
            }
        }
        let data = if keep {
            match verify_crc_raw(m.crc, &buf) {
                Ok(()) => {
                    *resident = (*resident).saturating_add(buf.len() as u64);
                    EntryData::Resident(buf)
                }
                Err(why) => EntryData::Unavailable(why),
            }
        } else {
            // Never kept (unsupported extension or over the ceiling): recorded
            // for completeness; `items` won't index an unsupported name.
            EntryData::Unavailable(UNAVAIL_FILTER)
        };
        latest.insert(m.name.clone(), (m.unpack, m.crc, data));
    }
    if let Some((from, why)) = give_up_at {
        for m in members.iter().skip(from) {
            latest.insert(
                m.name.clone(),
                (m.unpack, m.crc, EntryData::Unavailable(why)),
            );
        }
    }
    Ok(())
}

/// CRC check shared by the eager path (the lazy path goes through
/// [`verify_crc`] on the entry).
fn verify_crc_raw(crc: Option<u32>, bytes: &[u8]) -> Result<(), &'static str> {
    match crc {
        Some(want) if crc32fast::hash(bytes) != want => Err(UNAVAIL_DAMAGED),
        _ => Ok(()),
    }
}

/// Distinguish the codec's *deliberate* refusal (an unsupported RAR feature —
/// a Delta/ARM-filtered entry) from damage, in the lazy decode path: compcol's
/// `From<Error> for io::Error` wraps everything as `Other`, but the viewer
/// should show the honest per-entry reason, exactly like a solid group's
/// degraded members do.
fn remap_codec_refusal(e: io::Error) -> io::Error {
    let refusal = e
        .get_ref()
        .and_then(|s| s.downcast_ref::<compcol::Error>())
        .is_some_and(|c| matches!(c, compcol::Error::Unsupported));
    if refusal {
        io::Error::new(io::ErrorKind::Unsupported, UNAVAIL_FILTER)
    } else {
        e
    }
}

/// Read as many bytes as `buf` holds, tolerating a short tail. Returns bytes read.
fn read_fully<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Raw-byte entry point for the cargo-fuzz harness (`fuzz/`): the scan +
/// solid decode over arbitrary bytes must only ever produce `Ok`/`Err`.
#[cfg(feature = "fuzz-internals")]
pub mod fuzz {
    use super::*;

    /// Scan (and eagerly decode any solid groups of) arbitrary bytes.
    pub fn rar_open(data: &[u8]) {
        let limits = OpenLimits {
            ceiling: 1 << 20,
            max_entries: 4096,
            max_name_bytes: 1 << 20,
            max_expanded: 1 << 24,
        };
        let mut cur = io::Cursor::new(data);
        let _ = scan_and_load(
            &mut cur,
            data.len() as u64,
            &|_| true,
            None,
            1 << 24,
            &limits,
        );
    }
}

// Fixtures live in `tests/fixtures/rar/`, generated once with WinRAR 7.23's
// Rar.exe over deterministic content reproduced by the helpers below.
// Regenerate with (from a staging dir holding the files):
//   rar a -ma5 -m0 store_nonsolid.rar a.jpg b.png notes.txt sub/c.webp
//   rar a -ma5 -m3 lz_nonsolid.rar    a.jpg b.png notes.txt sub/c.webp
//   rar a -ma5 -m3 -s lz_solid.rar    a.jpg b.png sub/c.webp
//   rar a -ma5 -m5 -s -ds delta_solid.rar a.jpg gradient.bmp z.jpg
//     (-ds keeps the given order: WinRAR otherwise sorts solid input by
//      extension, which would put the Delta-triggering .bmp first and poison
//      the whole group instead of just its tail)
//   rar a -ma5 -m3 -phunter2 encrypted.rar a.jpg b.png
//   rar a -ma5 -m3 -hphunter2 hdr_encrypted.rar a.jpg b.png
// rar4.rar is the corpus's `rar4_signature_only.rar` (WinRAR 7 cannot write
// RAR4; the corpus one came from rar 6.24 in WSL).
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn is_img(ext: &str) -> bool {
        matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "tga")
    }

    // The exact bytes the fixtures archive (mirrors the generation script).
    // Aperiodic on purpose: fixed-stride repetition reads as channel-interleaved
    // data to WinRAR's content analysis, which then Delta-filters it — that
    // trigger is reserved for the gradient BMP in `delta_solid.rar`.
    fn gen(fmt: impl Fn(usize) -> String, n: usize) -> Vec<u8> {
        (0..n).map(fmt).collect::<String>().into_bytes()
    }
    fn jpg_a() -> Vec<u8> {
        gen(
            |i| format!("JPEG block {i:04} alpha rendering pipeline sample. "),
            40,
        )
    }
    fn png_b() -> Vec<u8> {
        gen(
            |i| format!("PNG chunk {i:04} bravo with palette and gamma data. "),
            50,
        )
    }
    fn webp_c() -> Vec<u8> {
        gen(
            |i| format!("WEBP frame {i:04} charlie lossy luma plane row. "),
            45,
        )
    }

    static NONCE: AtomicUsize = AtomicUsize::new(0);

    /// Materialize an embedded fixture as a temp file (opens take paths).
    fn fixture(tag: &str, bytes: &[u8]) -> PathBuf {
        let n = NONCE.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("pb_rar_{tag}_{}_{n}.rar", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    const STORE_NONSOLID: &[u8] = include_bytes!("../tests/fixtures/rar/store_nonsolid.rar");
    const LZ_NONSOLID: &[u8] = include_bytes!("../tests/fixtures/rar/lz_nonsolid.rar");
    const LZ_SOLID: &[u8] = include_bytes!("../tests/fixtures/rar/lz_solid.rar");
    const DELTA_SOLID: &[u8] = include_bytes!("../tests/fixtures/rar/delta_solid.rar");
    const ENCRYPTED: &[u8] = include_bytes!("../tests/fixtures/rar/encrypted.rar");
    const HDR_ENCRYPTED: &[u8] = include_bytes!("../tests/fixtures/rar/hdr_encrypted.rar");
    const RAR4: &[u8] = include_bytes!("../tests/fixtures/rar/rar4.rar");

    fn open(bytes: &[u8], tag: &str) -> Result<RarSource, OpenError> {
        let path = fixture(tag, bytes);
        let src = RarSource::open(&path, is_img, None, u64::MAX);
        // The lazy model re-reads from the path, so keep the file until the
        // source drops; tests clean up via the OS temp dir.
        src
    }

    #[test]
    fn store_nonsolid_lists_supported_sorted_and_reads_bytes() {
        let path = fixture("store", STORE_NONSOLID);
        let src = RarSource::open(&path, is_img, None, u64::MAX).unwrap();
        assert_eq!(src.len(), 3, "the .txt is excluded");
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["a.jpg", "b.png", "sub/c.webp"]);
        assert_eq!(src.bytes(0).unwrap(), jpg_a());
        assert_eq!(src.bytes(1).unwrap(), png_b());
        assert_eq!(src.bytes(2).unwrap(), webp_c());
        assert!(src.bytes(99).is_err(), "out-of-range read errors");
        assert_eq!(src.name(99), "");
        assert_eq!(src.size_hint(0), Some(jpg_a().len() as u64));
        assert!(src.path(0).is_none(), "archive entries have no fs path");
        assert_eq!(src.container(), Some(path.as_path()));
        assert!(src.random_access());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lz_nonsolid_decodes_each_entry_lazily() {
        let path = fixture("lz", LZ_NONSOLID);
        let src = RarSource::open(&path, is_img, None, u64::MAX).unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["a.jpg", "b.png", "sub/c.webp"]);
        // Reads are independent + repeatable (the decode pool hits these from
        // many workers).
        assert_eq!(src.bytes(2).unwrap(), webp_c());
        assert_eq!(src.bytes(0).unwrap(), jpg_a());
        assert_eq!(src.bytes(0).unwrap(), jpg_a());
        assert_eq!(src.bytes(1).unwrap(), png_b());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lz_solid_decodes_eagerly_with_progress() {
        let path = fixture("solid", LZ_SOLID);
        let progress = OpenProgress::new();
        let src = RarSource::open(&path, is_img, Some(&progress), u64::MAX).unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["a.jpg", "b.png", "sub/c.webp"]);
        assert_eq!(src.bytes(0).unwrap(), jpg_a());
        assert_eq!(src.bytes(1).unwrap(), png_b());
        assert_eq!(src.bytes(2).unwrap(), webp_c());
        let total = (jpg_a().len() + png_b().len() + webp_c().len()) as u64;
        assert_eq!(progress.total(), total, "total = the solid decode work");
        assert_eq!(progress.done(), total, "decode ran to completion");
        // Solid entries are resident: deleting the archive doesn't break reads.
        let _ = std::fs::remove_file(&path);
        assert_eq!(src.bytes(0).unwrap(), jpg_a());
    }

    /// The Delta degradation (plan #103 §2): WinRAR auto-picks the Delta filter
    /// for gradient-BMP content, which the pinned compcol refuses. The member
    /// and everything after it in its solid group turn unavailable with an
    /// honest error; earlier members and the archive itself still serve.
    #[test]
    fn delta_member_degrades_its_solid_group_not_the_archive() {
        let path = fixture("delta", DELTA_SOLID);
        let src = RarSource::open(&path, is_img, None, u64::MAX).unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["a.jpg", "gradient.bmp", "z.jpg"]);
        assert_eq!(src.bytes(0).unwrap(), jpg_a(), "before the Delta member");
        let bmp = src.bytes(1);
        let z = src.bytes(2);
        assert!(bmp.is_err(), "the Delta-filtered member is unavailable");
        assert!(z.is_err(), "members after it in the group are unavailable");
        let msg = bmp.unwrap_err().to_string();
        assert!(msg.contains("not supported"), "honest reason: {msg}");
        let _ = std::fs::remove_file(&path);
    }

    /// Encrypted RARs refuse with an honest message — NOT PasswordRequired,
    /// which would route to a password prompt no password can satisfy (we
    /// detect encryption; we do not decrypt).
    #[test]
    fn encrypted_archives_refuse_honestly() {
        for (tag, bytes) in [("enc", ENCRYPTED), ("hdrenc", HDR_ENCRYPTED)] {
            match open(bytes, tag) {
                Err(OpenError::Unsupported(msg)) => {
                    assert!(msg.contains("password protected"), "{tag}: {msg}");
                }
                other => panic!("{tag}: expected Unsupported, got {:?}", other.err()),
            }
        }
    }

    #[test]
    fn rar4_is_detected_with_an_honest_message() {
        match open(RAR4, "rar4") {
            Err(OpenError::Unsupported(msg)) => {
                assert!(msg.contains("RAR4"), "{msg}");
                assert!(msg.contains("RAR5"), "tells the user what does work: {msg}");
            }
            other => panic!("expected Unsupported, got {:?}", other.err()),
        }
    }

    #[test]
    fn garbage_and_missing_files_error_cleanly() {
        match open(b"definitely not a rar archive....", "garbage") {
            Err(OpenError::Corrupt(_)) => {}
            other => panic!("expected Corrupt, got {:?}", other.err()),
        }
        let missing = std::env::temp_dir().join("pb_rar_missing_never_written.rar");
        match RarSource::open(&missing, is_img, None, u64::MAX) {
            Err(OpenError::Io(_)) => {}
            other => panic!("expected Io, got {:?}", other.err()),
        }
    }

    /// The CRC check (which neither compcol nor fstool perform): corrupting a
    /// stored entry's bytes makes that entry fail with "damaged" — it must
    /// never be served as valid image bytes.
    #[test]
    fn crc_verification_catches_corrupted_bytes() {
        let mut bytes = STORE_NONSOLID.to_vec();
        // Store = verbatim: the file content is findable in the archive.
        let needle = b"JPEG block 0001 alpha";
        let at = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("stored content present");
        bytes[at + 4] ^= 0xFF;
        let path = fixture("crc", &bytes);
        let src = RarSource::open(&path, is_img, None, u64::MAX).unwrap();
        let a = (0..src.len()).find(|&i| src.name(i) == "a.jpg").unwrap();
        let err = src.bytes(a).expect_err("corrupt bytes must not be served");
        assert!(err.to_string().contains("damaged"), "{err}");
        // The untouched entries still read fine.
        let b = (0..src.len()).find(|&i| src.name(i) == "b.png").unwrap();
        assert_eq!(src.bytes(b).unwrap(), png_b());
        let _ = std::fs::remove_file(&path);
    }

    /// The solid eager decode honors the RAM budget with the structured
    /// refusal, and the same archive passes under a sufficient budget.
    #[test]
    fn solid_open_refuses_past_the_budget() {
        let path = fixture("budget", LZ_SOLID);
        match RarSource::open(&path, is_img, None, 100) {
            Err(OpenError::TooLarge { needed, budget }) => {
                assert_eq!(budget, 100);
                assert!(needed > 100);
            }
            other => panic!("expected TooLarge, got {:?}", other.err()),
        }
        assert!(RarSource::open(&path, is_img, None, u64::MAX).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn solid_open_cancels() {
        let path = fixture("cancel", LZ_SOLID);
        let progress = OpenProgress::new();
        progress.request_cancel();
        match RarSource::open(&path, is_img, Some(&progress), u64::MAX) {
            Err(OpenError::Cancelled) => {}
            other => panic!("expected Cancelled, got {:?}", other.err()),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Entries over the per-entry ceiling are not indexed (an item that can
    /// never render must not occupy a playlist slot).
    #[test]
    fn over_ceiling_entries_are_not_indexed() {
        let path = fixture("ceiling", LZ_NONSOLID);
        let limits = OpenLimits {
            ceiling: 10,
            ..OpenLimits::default()
        };
        let src = RarSource::open_with_limits(&path, is_img, None, u64::MAX, &limits).unwrap();
        assert!(src.is_empty(), "every fixture entry exceeds 10 bytes");
        let _ = std::fs::remove_file(&path);
    }

    /// Differential against the reference unrar over the spike's WinRAR corpus
    /// (the plan's test bar). Run:
    /// `PB_RAR_CORPUS=C:\Users\jdlien\code\compcol-rar-corpus\corpus \
    ///    cargo test -p pb-source rar_corpus -- --ignored --nocapture`
    #[test]
    #[ignore = "needs PB_RAR_CORPUS pointing at the WinRAR corpus + unrar on PATH"]
    fn rar_corpus_matches_unrar() {
        use std::process::Command;
        let Ok(dir) = std::env::var("PB_RAR_CORPUS") else {
            eprintln!("skipping: set PB_RAR_CORPUS");
            return;
        };
        let unrar = [
            "unrar".to_string(),
            r"C:\Program Files\WinRAR\UnRAR.exe".to_string(),
        ]
        .into_iter()
        .find(|c| Command::new(c).arg("-inul").output().is_ok())
        .expect("unrar not found");
        let mut archives = 0;
        let mut entries = 0;
        for entry in std::fs::read_dir(&dir).expect("corpus dir") {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            // RAR5, unencrypted only — the supported subset.
            if !name.starts_with("rar5_") || !name.ends_with(".rar") || name.contains("enc") {
                continue;
            }
            let src = match RarSource::open(&path, |_| true, None, u64::MAX) {
                Ok(s) => s,
                Err(e) => panic!("{name}: open failed: {e}"),
            };
            archives += 1;
            for i in 0..src.len() {
                let entry_name = src.name(i).to_string();
                let ours = match src.bytes(i) {
                    Ok(b) => b,
                    // Honest per-entry refusals (Delta-filtered members) are
                    // allowed; silent mismatches are not.
                    Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                        eprintln!("{name}/{entry_name}: unsupported ({e}) — allowed");
                        continue;
                    }
                    Err(e) => panic!("{name}/{entry_name}: read failed: {e}"),
                };
                let out = Command::new(&unrar)
                    .args(["p", "-inul", path.to_str().unwrap(), &entry_name])
                    .output()
                    .expect("run unrar");
                assert!(out.status.success(), "{name}/{entry_name}: unrar p failed");
                assert_eq!(
                    ours, out.stdout,
                    "{name}/{entry_name}: byte mismatch vs unrar"
                );
                entries += 1;
            }
        }
        eprintln!("[rar corpus] {archives} archives, {entries} entries byte-identical to unrar");
        assert!(archives > 0, "corpus dir had no rar5 archives");
    }
}
