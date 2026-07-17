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
//! * **Encryption at the container layer** (both per-file encryption, `-p`, and
//!   full header encryption, `-hp`) → [`OpenError::PasswordRequired`] when no
//!   password is supplied or the one given fails the header's password-check
//!   value, routing to the app's password prompt; a correct one decrypts. RAR5
//!   uses standard PBKDF2-HMAC-SHA256 + AES-256-CBC (see [`crate::rar_crypt`]) —
//!   the tractable scheme, unlike RAR4's bespoke KDF. fstool's failure mode here
//!   (feed ciphertext to the decoder, report "corrupt") is exactly what this
//!   avoids.
//! * **Solid-group degradation**: a member the codec refuses (the Delta/ARM
//!   filters — WinRAR auto-picks Delta for BMP/RAW-shaped content) marks that
//!   member *and the rest of its group* unavailable with an honest per-entry
//!   error; the archive still opens and every other group still serves.
//!   (JPEGs are not delta-filtered — measured in the spike — so the common
//!   photo case decodes.)
//!
//! Out of scope, detected and refused honestly: RAR4 (a different container
//! *and* codec — the "20-year-old archive" case waits on upstream compcol) and
//! multi-volume sets. Stored members inside a solid group are unavailable (they
//! sit outside the LZ bitstream; decoding around them would desync — fstool's
//! rule, kept).
//!
//! **Privacy:** RAM-only, read-only, never extracted to disk — same as every
//! source in this crate.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use compcol::Decoder as _;

use crate::rar_crypt::{self, KeyCache, MAX_LG2_COUNT, SIZE_IV, SIZE_PSWCHECK, SIZE_SALT};
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
// Encryption record / crypt-header flags.
const ENC_FLAG_CHECK: u64 = 0x01; // password-check value is present
const ENC_FLAG_MAC: u64 = 0x02; // stored checksum is a MAC, not a plain CRC32
/// The only RAR5 encryption version (AES-256). Anything else is refused.
const ENC_VERSION_AES256: u64 = 0;
/// Bytes of the header's stored password-check field: 8-byte check + 4-byte CRC.
const ENC_CHECK_BYTES: usize = 12;

/// User-facing refusal lines (plain copy, reused by tests).
const MSG_RAR4: &str =
    "This is an older RAR4 archive, which is not supported yet. Only RAR5 archives open.";
const MSG_VOLUME: &str = "Multi-volume RAR archives are not supported yet.";

/// Why an entry's bytes cannot be produced (per-entry honest errors).
const UNAVAIL_STORED_SOLID: &str =
    "this entry is stored inside a solid group, which cannot be unpacked reliably";
const UNAVAIL_FILTER: &str =
    "this entry (or one before it in its solid group) uses a RAR feature that is not supported yet";
const UNAVAIL_DAMAGED: &str = "this entry is damaged (checksum mismatch)";
const UNAVAIL_TRUNCATED: &str = "this entry is cut off (the archive ends before its data)";

/// Where an entry's bytes come from.
enum EntryData {
    /// Non-solid: decode independently on demand (open + seek + decode).
    Lazy {
        offset: u64,
        pack: u64,
        window: usize,
        store: bool,
        /// When encrypted (`-p`), the AES key + IV for the packed run; the run
        /// is CBC-decrypted before it reaches the store/codec path.
        crypt: Option<RunKey>,
    },
    /// Solid-group member, decoded at open.
    Resident(Vec<u8>),
    /// Cannot be produced; `bytes(i)` reports the reason.
    Unavailable(&'static str),
}

/// The AES-256 key + IV that decrypt one encrypted packed run (per file — RAR5
/// uses a fresh salt/IV for every encrypted member).
#[derive(Clone)]
struct RunKey {
    key: [u8; 32],
    iv: [u8; SIZE_IV],
}

/// The encryption record (extra-area type 0x01) parsed off a file header: the
/// KDF cost and salt to derive the key, the IV for this run, and — if present —
/// the password-check value that distinguishes a wrong password from damage.
struct FileEnc {
    lg2_count: u8,
    salt: [u8; SIZE_SALT],
    iv: [u8; SIZE_IV],
    psw_check: Option<[u8; SIZE_PSWCHECK]>,
    /// Flag 0x02: the header's stored checksum is a password MAC, not a plain
    /// CRC32, so the plain-CRC check can't validate the decrypted bytes.
    mac_checksums: bool,
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
    /// The parsed encryption record when this member is password protected.
    enc: Option<FileEnc>,
    /// Whether this member may appear in the item index (sane name). A
    /// non-indexable member still occupies its slot in the solid stream.
    indexable: bool,
    /// Whether the member's data runs past EOF (necessarily the last member
    /// scanned; its group degrades from here).
    truncated: bool,
}

impl RarSource {
    /// Open `path` as a RAR5 archive: scan the block chain (headers only),
    /// then eagerly decode any solid groups (RAM-budgeted against `budget`,
    /// cancellable and reporting decode progress through `progress`).
    /// `is_supported` is the entry predicate (the app passes its image+video
    /// union). `password` decrypts an encrypted archive: `None` on the first
    /// open (an encrypted archive then returns [`OpenError::PasswordRequired`],
    /// which routes to the prompt), `Some` when re-opening with the entered one
    /// (a wrong one returns `PasswordRequired` again). RAR has no last-resort
    /// sync path concerns — the app always opens it off-thread
    /// ([`crate::ArchiveKind::background_open`]).
    pub fn open(
        path: impl Into<PathBuf>,
        is_supported: impl Fn(&str) -> bool,
        progress: Option<&OpenProgress>,
        budget: u64,
        password: Option<&str>,
    ) -> Result<Self, OpenError> {
        Self::open_with_limits(
            path,
            is_supported,
            progress,
            budget,
            password,
            &OpenLimits::default(),
        )
    }

    pub(crate) fn open_with_limits(
        path: impl Into<PathBuf>,
        is_supported: impl Fn(&str) -> bool,
        progress: Option<&OpenProgress>,
        budget: u64,
        password: Option<&str>,
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
            password,
            limits,
        )?;
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
        crypt: Option<&RunKey>,
    ) -> io::Result<Vec<u8>> {
        if e.unpack > MAX_ENTRY_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive entry too large",
            ));
        }
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::new();
        buf.try_reserve_exact(e.unpack as usize).map_err(|_| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "archive entry too large to allocate",
            )
        })?;
        // `decode` turns the plaintext packed run (a `Read`) into the entry
        // bytes: stored runs are the bytes verbatim; compressed runs go through
        // the RAR5 codec, both capped at the declared size so a lying stream
        // cannot inflate past it.
        let unpack = e.unpack;
        let mut decode = |run: &mut dyn Read| -> io::Result<()> {
            if store {
                run.take(unpack).read_to_end(&mut buf)?;
            } else {
                let dec = compcol::rar5::Decoder::with_unpack_size_and_window(unpack, window);
                compcol::io::DecoderReader::new(run, dec)
                    .take(unpack)
                    .read_to_end(&mut buf)
                    .map_err(remap_codec_refusal)?;
            }
            Ok(())
        };
        match crypt {
            // Encrypted (`-p`): read the whole padded ciphertext run, CBC-decrypt
            // it in RAM, then decode from the plaintext.
            Some(rk) => {
                let mut packed = Vec::new();
                packed.try_reserve_exact(pack as usize).map_err(|_| {
                    io::Error::new(io::ErrorKind::OutOfMemory, "encrypted run too large")
                })?;
                (&mut file).take(pack).read_to_end(&mut packed)?;
                if packed.len() as u64 != pack {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "archive entry truncated",
                    ));
                }
                let mut cipher = rar_crypt::new_cbc(&rk.key, &rk.iv);
                rar_crypt::cbc_decrypt_blocks(&mut cipher, &mut packed);
                decode(&mut io::Cursor::new(&packed[..]))?;
            }
            None => {
                let mut run = BufReader::with_capacity(1 << 16, file).take(pack);
                decode(&mut run)?;
            }
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
                crypt,
            } => self.read_lazy(e, *offset, *pack, *window, *store, crypt.as_ref()),
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

/// Parse a file header's extra area for an encryption record (type 0x01),
/// returning the KDF/salt/IV/check needed to decrypt the run. `Ok(None)` means
/// the file is not encrypted; an unsupported encryption *version* is refused
/// (`Unsupported`) rather than silently mis-decoded.
fn parse_file_encryption(extra: &[u8]) -> Result<Option<FileEnc>, OpenError> {
    let mut p = 0usize;
    while p < extra.len() {
        let (rec_size, n) = read_vint(extra, p)
            .ok_or_else(|| OpenError::Corrupt("truncated RAR extra record".into()))?;
        let body_start = p + n;
        // `rec_size` spans Type + Data (from just after the size vint).
        let rec_end = body_start
            .checked_add(rec_size as usize)
            .filter(|&e| e <= extra.len())
            .ok_or_else(|| OpenError::Corrupt("RAR extra record overruns header".into()))?;
        if rec_end <= body_start {
            return Err(OpenError::Corrupt(
                "RAR extra record does not advance".into(),
            ));
        }
        let (rec_type, tn) = read_vint(extra, body_start)
            .ok_or_else(|| OpenError::Corrupt("truncated RAR extra record".into()))?;
        if rec_type == XREC_ENCRYPTION {
            let mut c = Cur {
                b: &extra[..rec_end],
                p: body_start + tn,
            };
            let version = c.vint()?;
            if version != ENC_VERSION_AES256 {
                return Err(OpenError::Unsupported(
                    "This RAR archive uses an encryption version that is not supported yet.".into(),
                ));
            }
            let flags = c.vint()?;
            let lg2_count = c.bytes(1)?[0];
            if lg2_count > MAX_LG2_COUNT {
                return Err(OpenError::Corrupt(
                    "RAR encryption KDF cost out of range".into(),
                ));
            }
            let salt: [u8; SIZE_SALT] = c.bytes(SIZE_SALT)?.try_into().expect("16-byte salt");
            let iv: [u8; SIZE_IV] = c.bytes(SIZE_IV)?.try_into().expect("16-byte IV");
            let psw_check = if flags & ENC_FLAG_CHECK != 0 {
                let chk = c.bytes(ENC_CHECK_BYTES)?;
                let mut pc = [0u8; SIZE_PSWCHECK];
                pc.copy_from_slice(&chk[..SIZE_PSWCHECK]);
                Some(pc)
            } else {
                None
            };
            return Ok(Some(FileEnc {
                lg2_count,
                salt,
                iv,
                psw_check,
                mac_checksums: flags & ENC_FLAG_MAC != 0,
            }));
        }
        p = rec_end;
    }
    Ok(None)
}

/// Derive the AES key + IV for an encrypted member's packed run. An encrypted
/// member implies a password was supplied (the scan returns `PasswordRequired`
/// otherwise), so the unwrap can never fire.
fn run_key(keys: &mut KeyCache, password: Option<&str>, fe: &FileEnc) -> RunKey {
    let pw = password.expect("an encrypted member implies a password");
    let derived = keys.get(pw.as_bytes(), &fe.salt, fe.lg2_count);
    RunKey {
        key: derived.key,
        iv: fe.iv,
    }
}

/// Read one RAR5 block header at `pos`, returning its plaintext bytes and the
/// on-disk offset where the block's data run begins, or `None` at a ragged /
/// truncated tail (serve what was scanned). When `header_key` is set (a `-hp`
/// archive), the block is 16-byte-IV-prefixed and AES-CBC-encrypted; it is
/// decrypted here. Either way the stored header CRC32 is verified before
/// returning — a bit-flipped (or wrongly-keyed) header reads as damage.
fn read_header_block<R: Read + Seek>(
    reader: &mut R,
    pos: u64,
    file_len: u64,
    header_key: Option<&[u8; 32]>,
) -> Result<Option<(Vec<u8>, u64)>, OpenError> {
    if let Some(key) = header_key {
        return read_encrypted_header(reader, pos, file_len, key);
    }
    reader.seek(SeekFrom::Start(pos))?;
    // CRC32 (4) + HeaderSize vint (≤ 3 bytes for our cap) + header.
    let mut pre = [0u8; 8];
    let pre_got = read_fully(reader, &mut pre)?;
    let Some((head_size, hs_len)) = read_vint(&pre[..pre_got], 4) else {
        return Ok(None); // ragged tail
    };
    if head_size > MAX_HEADER {
        return Err(OpenError::Corrupt("RAR header too large".into()));
    }
    let header_start = pos + 4 + hs_len as u64;
    let header_end = header_start + head_size;
    if header_end > file_len {
        return Ok(None); // truncated final header
    }
    let mut hdr = vec![0u8; head_size as usize];
    reader.seek(SeekFrom::Start(header_start))?;
    if read_fully(reader, &mut hdr)? != hdr.len() {
        return Ok(None);
    }
    // RAR5 stores a CRC32 of (HeaderSize vint || header data).
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&pre[4..4 + hs_len]);
    hasher.update(&hdr);
    if hasher.finalize() != u32::from_le_bytes([pre[0], pre[1], pre[2], pre[3]]) {
        return Err(OpenError::Corrupt("RAR header checksum mismatch".into()));
    }
    Ok(Some((hdr, header_end)))
}

/// Read and decrypt one `-hp` block header. Layout on disk: a 16-byte AES IV,
/// then the CBC ciphertext of the plaintext block (CRC32 || HeaderSize vint ||
/// header) padded up to a 16-byte boundary. The data run (if any) follows the
/// ciphertext and is *not* part of the header alignment.
fn read_encrypted_header<R: Read + Seek>(
    reader: &mut R,
    pos: u64,
    file_len: u64,
    key: &[u8; 32],
) -> Result<Option<(Vec<u8>, u64)>, OpenError> {
    reader.seek(SeekFrom::Start(pos))?;
    let mut iv = [0u8; SIZE_IV];
    if read_fully(reader, &mut iv)? != SIZE_IV {
        return Ok(None);
    }
    let mut cipher = rar_crypt::new_cbc(key, &iv);
    // Decrypt the first block to learn CRC + HeaderSize (both fit in 16 bytes:
    // CRC is 4, HeaderSize ≤ 3 vint bytes for the 1 MiB header cap).
    let mut first = [0u8; 16];
    if read_fully(reader, &mut first)? != 16 {
        return Ok(None);
    }
    rar_crypt::cbc_decrypt_blocks(&mut cipher, &mut first);
    let Some((head_size, hs_len)) = read_vint(&first, 4) else {
        return Err(OpenError::Corrupt("RAR encrypted header unreadable".into()));
    };
    if head_size > MAX_HEADER {
        return Err(OpenError::Corrupt("RAR header too large".into()));
    }
    let plain_len = 4 + hs_len + head_size as usize;
    let cipher_len = plain_len.div_ceil(16) * 16;
    if pos + SIZE_IV as u64 + cipher_len as u64 > file_len {
        return Ok(None);
    }
    let mut plain = Vec::with_capacity(cipher_len);
    plain.extend_from_slice(&first);
    if cipher_len > 16 {
        let mut rest = vec![0u8; cipher_len - 16];
        if read_fully(reader, &mut rest)? != rest.len() {
            return Ok(None);
        }
        rar_crypt::cbc_decrypt_blocks(&mut cipher, &mut rest);
        plain.extend_from_slice(&rest);
    }
    let stored_crc = u32::from_le_bytes([plain[0], plain[1], plain[2], plain[3]]);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&plain[4..plain_len]);
    if hasher.finalize() != stored_crc {
        // The crypt-header password-check already passed, so a mismatch here is
        // genuine damage rather than a wrong password.
        return Err(OpenError::Corrupt("RAR header checksum mismatch".into()));
    }
    let hdr = plain[4 + hs_len..plain_len].to_vec();
    let data_offset = pos + SIZE_IV as u64 + cipher_len as u64;
    Ok(Some((hdr, data_offset)))
}

/// Scan the block chain, then eagerly decode solid groups. Split from
/// [`RarSource::open`] over a generic reader so the fuzz harness drives it on
/// raw bytes. `password` decrypts an encrypted archive; a missing or wrong one
/// (checked against the header's password-check value) returns
/// [`OpenError::PasswordRequired`].
fn scan_and_load<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    is_supported: &dyn Fn(&str) -> bool,
    progress: Option<&OpenProgress>,
    budget: u64,
    password: Option<&str>,
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
    // `-hp` archives encrypt every block after the crypt header; once we parse
    // that header (and validate the password) this holds the header key so
    // subsequent blocks are decrypted before parsing.
    let mut header_key: Option<[u8; 32]> = None;
    // Cache of derived keys for this open, and whether a password-check value has
    // matched yet (so we only pay validation once for `-p`).
    let mut keys = KeyCache::default();
    let mut pw_validated = false;
    let mut pos: u64 = 8;
    while pos + 5 <= file_len {
        let Some((hdr, data_offset)) =
            read_header_block(reader, pos, file_len, header_key.as_ref())?
        else {
            break; // ragged/truncated tail: serve what was scanned
        };
        let head_size = hdr.len() as u64;
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
            HEAD_CRYPT => {
                // Full header encryption (`-hp`): the crypt header itself is
                // plaintext and carries the KDF params for every later block.
                if header_key.is_some() {
                    return Err(OpenError::Corrupt("duplicate RAR crypt header".into()));
                }
                let version = c.vint()?;
                if version != ENC_VERSION_AES256 {
                    return Err(OpenError::Unsupported(
                        "This RAR archive uses an encryption version that is not supported yet."
                            .into(),
                    ));
                }
                let cflags = c.vint()?;
                let lg2_count = c.bytes(1)?[0];
                if lg2_count > MAX_LG2_COUNT {
                    return Err(OpenError::Corrupt(
                        "RAR encryption KDF cost out of range".into(),
                    ));
                }
                let salt: [u8; SIZE_SALT] = c.bytes(SIZE_SALT)?.try_into().expect("16-byte salt");
                let stored_check = if cflags & ENC_FLAG_CHECK != 0 {
                    Some(c.bytes(ENC_CHECK_BYTES)?[..SIZE_PSWCHECK].to_vec())
                } else {
                    None
                };
                // Without a password we cannot read any further header. Prompt.
                let Some(pw) = password else {
                    return Err(OpenError::PasswordRequired);
                };
                let derived = keys.get(pw.as_bytes(), &salt, lg2_count);
                if let Some(want) = stored_check {
                    if derived.psw_check.as_slice() != want.as_slice() {
                        return Err(OpenError::PasswordRequired);
                    }
                    pw_validated = true;
                }
                header_key = Some(derived.key);
            }
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
                let enc = if extra_size > 0 {
                    parse_file_encryption(extra)?
                } else {
                    None
                };
                // An encrypted entry needs a password. `-hp` validated it at the
                // crypt header; for `-p`, validate against the first entry that
                // carries a check value, so a wrong password prompts rather than
                // decoding to garbage.
                if let Some(fe) = &enc {
                    let Some(pw) = password else {
                        return Err(OpenError::PasswordRequired);
                    };
                    if !pw_validated {
                        if let Some(want) = fe.psw_check {
                            let derived = keys.get(pw.as_bytes(), &fe.salt, fe.lg2_count);
                            if derived.psw_check != want {
                                return Err(OpenError::PasswordRequired);
                            }
                            pw_validated = true;
                        }
                    }
                }
                // A MAC'd checksum (flag 0x02) is not a plain CRC32, so drop it —
                // the plain-CRC check can't validate the decrypted bytes.
                let crc = if enc.as_ref().is_some_and(|e| e.mac_checksums) {
                    None
                } else {
                    crc
                };

                if !is_dir {
                    // An unsafe/overlong name means the entry is never shown or
                    // indexed — but its packed bytes are still part of its solid
                    // group's LZ stream, so the member must stay in the group
                    // model or every later member would desync. Drop the name
                    // (it is never displayed) so a hostile name can't bloat the
                    // tables either.
                    let indexable = sane_name(&name);
                    let name = if indexable { name } else { String::new() };
                    if indexable {
                        name_bytes += name.len() as u64;
                        if name_bytes > limits.max_name_bytes {
                            return Err(OpenError::Corrupt(
                                "the archive has too many entries".into(),
                            ));
                        }
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
                    // A member whose data runs past EOF can never decode — and
                    // since blocks are sequential, it is necessarily the last
                    // one scanned. Kept (flagged) so its group can degrade
                    // honestly instead of the member silently vanishing.
                    let truncated = data_offset.saturating_add(data_size) > file_len;
                    if !solid || groups.is_empty() {
                        groups.push(Vec::new());
                    }
                    groups.last_mut().expect("just pushed").push(Member {
                        name,
                        data_offset,
                        pack: data_size,
                        unpack,
                        crc,
                        method,
                        window,
                        enc,
                        indexable,
                        truncated,
                    });
                }
            }
            _ => {} // service/unknown headers: skip via data_size
        }

        let Some(next) = data_offset.checked_add(data_size) else {
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
    // decoded now (this is the slow, budgeted, cancellable part). Saturating
    // accumulation: hostile declared sizes must trip the cap, never overflow
    // past it.
    let solid_work: u64 = groups
        .iter()
        .filter(|g| g.len() > 1)
        .flat_map(|g| g.iter())
        .filter(|m| !m.truncated)
        .fold(0u64, |acc, m| acc.saturating_add(m.unpack));
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
            if !m.indexable || m.truncated {
                // Unshowable name, or data past EOF: an independent entry that
                // can never render doesn't occupy a playlist slot (the tar
                // family's rule for its truncated tails, kept).
                continue;
            }
            // An encrypted non-solid run carries its own key + IV; the lazy read
            // CBC-decrypts before the store/codec.
            let crypt = m.enc.as_ref().map(|fe| run_key(&mut keys, password, fe));
            let data = EntryData::Lazy {
                offset: m.data_offset,
                pack: m.pack,
                window: m.window,
                store: m.method == 0,
                crypt,
            };
            upsert(&mut latest, &mut resident, m.name, m.unpack, m.crc, data);
            continue;
        }
        // Multi-member solid group. A stored member sits outside the LZ
        // bitstream; decoding around it would desync the shared window (fstool's
        // rule, kept) — poison the whole group. Encryption is handled inside the
        // decode (each member's run is CBC-decrypted before the codec).
        if members.iter().any(|m| m.method == 0) {
            for m in members {
                if m.indexable {
                    upsert(
                        &mut latest,
                        &mut resident,
                        m.name,
                        m.unpack,
                        m.crc,
                        EntryData::Unavailable(UNAVAIL_STORED_SOLID),
                    );
                }
            }
            continue;
        }
        decode_solid_group(
            reader,
            members,
            is_supported,
            progress,
            budget,
            password,
            &mut keys,
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

/// Last-wins insert that keeps the resident-byte count honest: a replaced
/// entry's `Resident` bytes drop with it, so they must leave the budget too
/// (an append/update archive that fits after last-wins must not refuse as
/// `TooLarge` on stale accounting).
fn upsert(
    latest: &mut BTreeMap<String, (u64, Option<u32>, EntryData)>,
    resident: &mut u64,
    name: String,
    unpack: u64,
    crc: Option<u32>,
    data: EntryData,
) {
    if let EntryData::Resident(b) = &data {
        *resident = resident.saturating_add(b.len() as u64);
    }
    if let Some((_, _, EntryData::Resident(old))) = latest.insert(name, (unpack, crc, data)) {
        *resident = resident.saturating_sub(old.len() as u64);
    }
}

/// The `Resident` bytes currently held under `name`, if any — what an insert
/// of the same name would release (the budget check subtracts it up front).
fn resident_under(latest: &BTreeMap<String, (u64, Option<u32>, EntryData)>, name: &str) -> u64 {
    match latest.get(name) {
        Some((_, _, EntryData::Resident(b))) => b.len() as u64,
        _ => 0,
    }
}

/// Decode one multi-member solid group: a single resumable decoder over the
/// concatenation of the members' packed runs, keeping supported members
/// resident and discarding the rest. A member the codec refuses (an
/// unsupported filter) marks itself and everything after it in the group
/// unavailable — the shared window means nothing later can be trusted — but
/// never fails the archive. A truncated member (data past EOF — necessarily
/// the group's tail) is excluded from the stream and marked cut off, so the
/// intact members still serve.
/// An encrypted solid group is CBC-decrypted per member: each run is padded to a
/// 16-byte boundary and keyed by that member's own salt/IV, so the input cursor
/// snaps to each member's run and starts a fresh CBC chain (skipping the padding
/// tail of the previous one), while the single LZ decoder keeps the shared solid
/// window across all of them.
#[allow(clippy::too_many_arguments)]
fn decode_solid_group<R: Read + Seek>(
    reader: &mut R,
    members: Vec<Member>,
    is_supported: &dyn Fn(&str) -> bool,
    progress: Option<&OpenProgress>,
    budget: u64,
    password: Option<&str>,
    keys: &mut KeyCache,
    limits: &OpenLimits,
    resident: &mut u64,
    latest: &mut BTreeMap<String, (u64, Option<u32>, EntryData)>,
) -> Result<(), OpenError> {
    // A truncated member is necessarily the last one scanned; the stream is
    // decodable up to it. Mark the cut-off tail honestly and decode the rest.
    let cut = members
        .iter()
        .position(|m| m.truncated)
        .unwrap_or(members.len());
    for m in &members[cut..] {
        if m.indexable {
            upsert(
                latest,
                resident,
                m.name.clone(),
                m.unpack,
                m.crc,
                EntryData::Unavailable(UNAVAIL_TRUNCATED),
            );
        }
    }
    let members = &members[..cut];
    if members.is_empty() {
        return Ok(());
    }

    // The shared window must fit every member's declaration.
    let window = members.iter().map(|m| m.window).max().unwrap_or(0x20000);
    let total = members
        .iter()
        .fold(0u64, |acc, m| acc.saturating_add(m.unpack));
    let mut dec = compcol::rar5::Decoder::with_unpack_size_and_window(total, window);
    // Register each member's start offset (the cumulative unpacked size of the
    // members before it) so position-dependent filters (x86 E8/E9) compute call
    // targets relative to the containing *file*, the way unrar does — not the
    // solid stream. Without this, an x86-filtered member after the first in a
    // solid group decodes with wrong call targets (our CRC catches it as
    // "damaged"). Delta is position-independent and needs no boundary.
    let mut boundary = 0u64;
    for m in members {
        dec.add_file_boundary(boundary);
        boundary = boundary.saturating_add(m.unpack);
    }

    // Encrypted members are materialized up front: each run is CBC-decrypted
    // (its own key/IV) and stripped of the 16-byte-boundary padding that would
    // otherwise sit *between* files in the shared LZ stream — the decoder reads
    // block framing eagerly and a padding byte reads as a bogus next-block
    // header. Stripped runs are byte-exact continuations, so the continuous feed
    // below is identical to the unencrypted path. `None` = read lazily from file.
    let enc_runs: Vec<Option<Vec<u8>>> = members
        .iter()
        .map(|m| match m.enc.as_ref() {
            Some(fe) => {
                let rk = run_key(keys, password, fe);
                let mut run = vec![0u8; m.pack as usize];
                reader.seek(SeekFrom::Start(m.data_offset))?;
                if read_fully(reader, &mut run)? != run.len() {
                    return Err(OpenError::Corrupt("RAR data run truncated".into()));
                }
                let mut cipher = rar_crypt::new_cbc(&rk.key, &rk.iv);
                rar_crypt::cbc_decrypt_blocks(&mut cipher, &mut run);
                let real = rar5_stream_len(&run).ok_or_else(|| {
                    OpenError::Corrupt("RAR encrypted block framing malformed".into())
                })?;
                run.truncate(real);
                Ok(Some(run))
            }
            None => Ok(None),
        })
        .collect::<Result<_, OpenError>>()?;

    // Compressed-input cursor over the members' runs, in order: an encrypted
    // member reads from its stripped in-memory buffer, an unencrypted one from
    // the file. Either way the runs concatenate to the exact solid stream.
    let areas: Vec<(u64, u64)> = members
        .iter()
        .enumerate()
        .map(|(i, m)| match &enc_runs[i] {
            Some(v) => (0, v.len() as u64),
            None => (m.data_offset, m.pack),
        })
        .collect();
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
        let got = match &enc_runs[*in_area] {
            Some(v) => {
                let from = *in_off as usize;
                in_buf[..want].copy_from_slice(&v[from..from + want]);
                want
            }
            None => {
                reader.seek(SeekFrom::Start(offset + *in_off))?;
                read_fully(reader, &mut in_buf[..want])?
            }
        };
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
        let keep = m.indexable && is_supported(&ext_of(&m.name)) && m.unpack <= limits.ceiling;
        let mut buf = Vec::new();
        if keep {
            // A duplicate name replaces (and releases) the bytes already held
            // under it, so only the delta counts against the budget.
            let replaced = resident_under(latest, &m.name);
            let needed = resident.saturating_sub(replaced).saturating_add(m.unpack);
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
                Ok(()) => EntryData::Resident(buf),
                Err(why) => EntryData::Unavailable(why),
            }
        } else {
            // Never kept (unsupported extension or over the ceiling): recorded
            // for completeness; `items` won't index an unsupported name.
            EntryData::Unavailable(UNAVAIL_FILTER)
        };
        if m.indexable {
            // `upsert` owns the resident accounting for both the new bytes and
            // anything the duplicate name replaces.
            upsert(latest, resident, m.name.clone(), m.unpack, m.crc, data);
        }
    }
    if let Some((from, why)) = give_up_at {
        for m in members.iter().skip(from) {
            if m.indexable {
                upsert(
                    latest,
                    resident,
                    m.name.clone(),
                    m.unpack,
                    m.crc,
                    EntryData::Unavailable(why),
                );
            }
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

/// Byte length of a RAR5 compressed run's real block data: walk the block
/// framing — a 2-byte header (`flags`, `cksum`) + a `byte_count+1`-byte
/// little-endian `block_size` + `block_size` data bytes — until a block with the
/// `last_block` flag is consumed, and return the offset just past it. An
/// encrypted run is padded past this length to a 16-byte boundary; that padding
/// must be dropped before the LZ decoder sees it, since the decoder reads block
/// headers eagerly and a padding byte parses as a bogus one. `None` if the
/// framing is malformed (matches compcol's own block-header validation).
fn rar5_stream_len(data: &[u8]) -> Option<usize> {
    let mut pos = 0usize;
    loop {
        if pos + 2 > data.len() {
            return None;
        }
        let flags = data[pos];
        let byte_count = ((flags >> 3) & 7) as usize;
        if byte_count > 2 {
            return None;
        }
        let last_block = flags & 0x40 != 0;
        let cksum = data[pos + 1];
        let size_len = byte_count + 1;
        if pos + 2 + size_len > data.len() {
            return None;
        }
        let mut size_bytes = [0u8; 3];
        for (i, sb) in size_bytes.iter_mut().enumerate().take(size_len) {
            *sb = data[pos + 2 + i];
        }
        if 0x5A ^ flags ^ size_bytes[0] ^ size_bytes[1] ^ size_bytes[2] != cksum {
            return None;
        }
        let block_size =
            u32::from_le_bytes([size_bytes[0], size_bytes[1], size_bytes[2], 0]) as usize;
        if block_size == 0 {
            return None;
        }
        pos = pos.checked_add(2 + size_len + block_size)?;
        if pos > data.len() {
            return None;
        }
        if last_block {
            return Some(pos);
        }
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
            None,
            &limits,
        );
        // Also exercise the decrypt paths with a fixed password.
        let mut cur = io::Cursor::new(data);
        let _ = scan_and_load(
            &mut cur,
            data.len() as u64,
            &|_| true,
            None,
            1 << 24,
            Some("password"),
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
//   rar a -ma5 -m3 -s -phunter2 encrypted_solid.rar a.jpg b.png sub/c.webp
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
    const ENCRYPTED_SOLID: &[u8] = include_bytes!("../tests/fixtures/rar/encrypted_solid.rar");
    const HDR_ENCRYPTED: &[u8] = include_bytes!("../tests/fixtures/rar/hdr_encrypted.rar");
    const RAR4: &[u8] = include_bytes!("../tests/fixtures/rar/rar4.rar");

    fn open(bytes: &[u8], tag: &str) -> Result<RarSource, OpenError> {
        let path = fixture(tag, bytes);
        let src = RarSource::open(&path, is_img, None, u64::MAX, None);
        // The lazy model re-reads from the path, so keep the file until the
        // source drops; tests clean up via the OS temp dir.
        src
    }

    fn open_pw(bytes: &[u8], tag: &str, password: &str) -> Result<RarSource, OpenError> {
        let path = fixture(tag, bytes);
        RarSource::open(&path, is_img, None, u64::MAX, Some(password))
    }

    #[test]
    fn store_nonsolid_lists_supported_sorted_and_reads_bytes() {
        let path = fixture("store", STORE_NONSOLID);
        let src = RarSource::open(&path, is_img, None, u64::MAX, None).unwrap();
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
        let src = RarSource::open(&path, is_img, None, u64::MAX, None).unwrap();
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
        let src = RarSource::open(&path, is_img, Some(&progress), u64::MAX, None).unwrap();
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

    /// The Delta filter now decodes (compcol's `rar3-standard-filters` work):
    /// WinRAR auto-picks Delta for gradient-BMP content, and every member of the
    /// solid group — including the Delta member and the one after it — reads
    /// back. `bytes()` CRC-verifies against the header, so a successful read is
    /// byte-correct by construction. Delta is position-independent, so it needs
    /// no `add_file_boundary` registration (unlike the x86 filter).
    #[test]
    fn delta_member_and_its_solid_group_decode() {
        let path = fixture("delta", DELTA_SOLID);
        let src = RarSource::open(&path, is_img, None, u64::MAX, None).unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["a.jpg", "gradient.bmp", "z.jpg"]);
        assert_eq!(src.bytes(0).unwrap(), jpg_a(), "before the Delta member");
        assert!(src.bytes(1).is_ok(), "the Delta-filtered member decodes");
        assert!(src.bytes(2).is_ok(), "the member after it decodes");
        let _ = std::fs::remove_file(&path);
    }

    /// Encrypted RARs prompt (PasswordRequired) with no password or a wrong one,
    /// and decrypt byte-perfect with the right one — covering both per-file
    /// (`-p`) and full-header (`-hp`) encryption. Password: "hunter2".
    #[test]
    fn encrypted_archives_prompt_then_decrypt() {
        for (tag, bytes) in [("enc", ENCRYPTED), ("hdrenc", HDR_ENCRYPTED)] {
            match open(bytes, tag) {
                Err(OpenError::PasswordRequired) => {}
                other => {
                    panic!(
                        "{tag} no-pw: expected PasswordRequired, got {:?}",
                        other.err()
                    )
                }
            }
            match open_pw(bytes, tag, "wrongpass") {
                Err(OpenError::PasswordRequired) => {}
                other => panic!(
                    "{tag} wrong-pw: expected PasswordRequired, got {:?}",
                    other.err()
                ),
            }
            let src = open_pw(bytes, tag, "hunter2")
                .unwrap_or_else(|e| panic!("{tag}: correct password should open: {e:?}"));
            let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
            assert_eq!(names, vec!["a.jpg", "b.png"], "{tag}: names");
            assert_eq!(src.bytes(0).unwrap(), jpg_a(), "{tag}: a.jpg decrypts");
            assert_eq!(src.bytes(1).unwrap(), png_b(), "{tag}: b.png decrypts");
        }
    }

    /// A solid *and* encrypted group (`-m3 -s -phunter2`): each member's run is
    /// CBC-decrypted with its own key/IV (padded to 16 bytes, so the decode
    /// snaps past each run's padding) while the single LZ window carries across —
    /// every member must decode byte-perfect.
    #[test]
    fn encrypted_solid_group_decrypts_every_member() {
        assert!(
            matches!(
                open(ENCRYPTED_SOLID, "esolid"),
                Err(OpenError::PasswordRequired)
            ),
            "no password prompts"
        );
        assert!(
            matches!(
                open_pw(ENCRYPTED_SOLID, "esolid", "nope"),
                Err(OpenError::PasswordRequired)
            ),
            "wrong password re-prompts"
        );
        let src = open_pw(ENCRYPTED_SOLID, "esolid", "hunter2").expect("correct password opens");
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(names, vec!["a.jpg", "b.png", "sub/c.webp"]);
        assert_eq!(src.bytes(0).unwrap(), jpg_a());
        assert_eq!(src.bytes(1).unwrap(), png_b());
        assert_eq!(src.bytes(2).unwrap(), webp_c());
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
        match RarSource::open(&missing, is_img, None, u64::MAX, None) {
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
        let src = RarSource::open(&path, is_img, None, u64::MAX, None).unwrap();
        let a = (0..src.len()).find(|&i| src.name(i) == "a.jpg").unwrap();
        let err = src.bytes(a).expect_err("corrupt bytes must not be served");
        assert!(err.to_string().contains("damaged"), "{err}");
        // The untouched entries still read fine.
        let b = (0..src.len()).find(|&i| src.name(i) == "b.png").unwrap();
        assert_eq!(src.bytes(b).unwrap(), png_b());
        let _ = std::fs::remove_file(&path);
    }

    /// A bit-flipped block header must read as damage before any parsed field
    /// is trusted (codex review of the implementation, P2).
    #[test]
    fn header_crc_catches_a_corrupted_block_header() {
        let mut bytes = LZ_NONSOLID.to_vec();
        // Offset 8..12 is the first block header's CRC32; flipping it (or any
        // header byte) must mismatch.
        bytes[8] ^= 0xFF;
        match open(&bytes, "hdrcrc") {
            Err(OpenError::Corrupt(msg)) => assert!(msg.contains("header"), "{msg}"),
            other => panic!("expected Corrupt, got {:?}", other.err()),
        }
    }

    /// Test-local block walker: patch a same-length byte string inside one
    /// header and recompute that header's CRC, so container-shape tests can
    /// craft inputs WinRAR would never write.
    fn patch_header(bytes: &mut [u8], old: &[u8], new: &[u8]) {
        assert_eq!(old.len(), new.len(), "in-place patch only");
        let mut pos = 8usize;
        while pos + 5 <= bytes.len() {
            let (head_size, hs_len) = read_vint(&bytes[pos + 4..], 0).expect("size vint");
            let hstart = pos + 4 + hs_len;
            let hend = hstart + head_size as usize;
            if let Some(at) = bytes[hstart..hend]
                .windows(old.len())
                .position(|w| w == old)
            {
                bytes[hstart + at..hstart + at + new.len()].copy_from_slice(new);
                let mut h = crc32fast::Hasher::new();
                h.update(&bytes[pos + 4..hend]);
                bytes[pos..pos + 4].copy_from_slice(&h.finalize().to_le_bytes());
                return;
            }
            // Advance: parse enough of the header for data_size.
            let mut c = Cur {
                b: &bytes[hstart..hend],
                p: 0,
            };
            let _htype = c.vint().unwrap();
            let hflags = c.vint().unwrap();
            let _extra = if hflags & HFLAG_EXTRA != 0 {
                c.vint().unwrap()
            } else {
                0
            };
            let data = if hflags & HFLAG_DATA != 0 {
                c.vint().unwrap()
            } else {
                0
            };
            pos = hend + data as usize;
        }
        panic!("pattern not found in any header");
    }

    /// A member with a traversal-shaped name stays in its solid group's stream
    /// model (dropping it would desync every later member); it just never
    /// appears in the index (codex review of the implementation, P2).
    #[test]
    fn insane_named_member_does_not_desync_its_solid_group() {
        let mut bytes = LZ_SOLID.to_vec();
        // Same length as "a.jpg" — the archive's first solid member.
        patch_header(&mut bytes, b"a.jpg", b"../.j");
        let path = fixture("insane", &bytes);
        let src = RarSource::open(&path, is_img, None, u64::MAX, None).unwrap();
        let names: Vec<&str> = (0..src.len()).map(|i| src.name(i)).collect();
        assert_eq!(
            names,
            vec!["b.png", "sub/c.webp"],
            "the unsafe name is not indexed"
        );
        // The later members decode byte-perfect: the renamed member's packed
        // bytes are still part of the shared stream.
        assert_eq!(src.bytes(0).unwrap(), png_b());
        assert_eq!(src.bytes(1).unwrap(), webp_c());
        let _ = std::fs::remove_file(&path);
    }

    /// A truncated solid tail degrades that member honestly; the intact
    /// members before it still serve (the tar family's truncation posture).
    #[test]
    fn truncated_solid_tail_degrades_gracefully() {
        // Chop into the last member's packed data (the end block is 7 bytes;
        // 60 lands well inside the final data run of the 463-byte fixture).
        let chopped = &LZ_SOLID[..LZ_SOLID.len() - 60];
        let path = fixture("solidtrunc", chopped);
        let src = RarSource::open(&path, is_img, None, u64::MAX, None).unwrap();
        let a = (0..src.len())
            .find(|&i| src.name(i) == "a.jpg")
            .expect("a.jpg listed");
        assert_eq!(src.bytes(a).unwrap(), jpg_a(), "intact members decode");
        // Whatever the chop clipped reports an honest per-entry error (cut
        // off / damaged), never silent wrong bytes.
        for i in 0..src.len() {
            match src.bytes(i) {
                Ok(b) => {
                    let want = match src.name(i) {
                        "a.jpg" => jpg_a(),
                        "b.png" => png_b(),
                        "sub/c.webp" => webp_c(),
                        other => panic!("unexpected entry {other}"),
                    };
                    assert_eq!(b, want, "{}: decoded bytes must be exact", src.name(i));
                }
                Err(e) => {
                    let msg = e.to_string();
                    assert!(
                        msg.contains("cut off") || msg.contains("damaged"),
                        "{}: honest reason expected, got {msg}",
                        src.name(i)
                    );
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    /// The solid eager decode honors the RAM budget with the structured
    /// refusal, and the same archive passes under a sufficient budget.
    #[test]
    fn solid_open_refuses_past_the_budget() {
        let path = fixture("budget", LZ_SOLID);
        match RarSource::open(&path, is_img, None, 100, None) {
            Err(OpenError::TooLarge { needed, budget }) => {
                assert_eq!(budget, 100);
                assert!(needed > 100);
            }
            other => panic!("expected TooLarge, got {:?}", other.err()),
        }
        assert!(RarSource::open(&path, is_img, None, u64::MAX, None).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn solid_open_cancels() {
        let path = fixture("cancel", LZ_SOLID);
        let progress = OpenProgress::new();
        progress.request_cancel();
        match RarSource::open(&path, is_img, Some(&progress), u64::MAX, None) {
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
        let src =
            RarSource::open_with_limits(&path, is_img, None, u64::MAX, None, &limits).unwrap();
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
            if !name.starts_with("rar5_") || !name.ends_with(".rar") {
                continue;
            }
            // The corpus's encrypted archives (`-ptestpass` / `-hptestpass`) are
            // now decoded too — pass the password to both us and the oracle.
            let password = name.contains("enc").then_some("testpass");
            let src = match RarSource::open(&path, |_| true, None, u64::MAX, password) {
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
                let mut args = vec!["p".to_string(), "-inul".to_string()];
                if let Some(pw) = password {
                    args.push(format!("-p{pw}"));
                }
                args.push(path.to_str().unwrap().to_string());
                args.push(entry_name.clone());
                let out = Command::new(&unrar)
                    .args(&args)
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
