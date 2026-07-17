//! RAR4 / RAR3 archive viewing (the pre-RAR5 container, format version 29).
//!
//! **The container parser here is ours**; the `compcol::rar3` crate supplies the
//! RAR3/RAR4 codec (LZ77+Huffman, PPMd-II variant H, in-band Delta/x86 filters,
//! solid multi-member — decode-only, MIT). RAR4's container is a completely
//! different shape from RAR5 (see [`crate::rar`]): fixed 7-byte block headers
//! (CRC-16 + type + flags + size, plus a 4-byte data size when the block carries
//! data), not RAR5's vint blocks. Everything else follows the same two access
//! models and the same [`RarEntry`] plumbing, so an opened RAR4 archive shares
//! [`RarSource`](crate::rar::RarSource)'s `ItemSource` implementation:
//!
//! * **Non-solid members decode lazily** — each is its own compressed stream.
//! * **Solid archives decode eagerly at open** — the members share one
//!   compression history (window, tables, PPMd model), so there is no cheap
//!   per-member access; the group decodes once, RAM-budgeted and cancellable.
//!   Unlike RAR5, each member's payload is its own byte-aligned stream, so the
//!   codec is driven with `with_solid()` + `begin_solid_member()` and there is no
//!   inter-file padding to strip.
//!
//! Same hardening as RAR5: the stored header CRC-16 is verified before any field
//! is trusted, each decoded entry's CRC-32 is checked, and a member the codec
//! refuses (an exotic VM filter, an unknown PPMd feature) degrades itself and the
//! rest of its solid group with an honest per-entry error while the archive still
//! opens.
//!
//! **Out of scope, refused honestly:** multi-volume sets and **encrypted RAR4**
//! (both `-p` and `-hp`) — RAR4 uses a bespoke SHA-1 key schedule, not the
//! tractable PBKDF2 + AES scheme RAR5 uses, so encrypted entries are marked
//! unavailable rather than decrypted.
//!
//! **Privacy:** RAM-only, read-only, never extracted to disk.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};

use compcol::Decoder as _;

use crate::rar::{
    read_fully, resident_under, upsert, verify_crc_raw, EntryData, RarCodec, RarEntry, MSG_VOLUME,
    UNAVAIL_FILTER, UNAVAIL_STORED_SOLID, UNAVAIL_TRUNCATED,
};
use crate::tar_source::{sane_name, OpenLimits};
use crate::{ext_of, normalize_entry_name, OpenError, OpenProgress};

/// Output staging chunk for the eager solid decode; also the cancel granularity.
const CHUNK: usize = 64 * 1024;

// RAR4 block types.
const BLK_MAIN: u8 = 0x73;
const BLK_FILE: u8 = 0x74;
const BLK_ENDARC: u8 = 0x7b;
// Common block flag: the block is followed by `add_size` bytes of data.
const HFLAG_LONG: u16 = 0x8000;
// Main-header flags. (The archive-wide solid bit 0x0008 is not needed: the
// per-file LHD_SOLID flag already marks which members continue a group.)
const MHD_VOLUME: u16 = 0x0001;
// File-header flags.
const LHD_SPLIT_BEFORE: u16 = 0x0001;
const LHD_SPLIT_AFTER: u16 = 0x0002;
const LHD_PASSWORD: u16 = 0x0004;
const LHD_SOLID: u16 = 0x0010;
const LHD_WINDOWMASK: u16 = 0x00e0; // == mask means a directory, not a file
const LHD_LARGE: u16 = 0x0100;
const LHD_UNICODE: u16 = 0x0200;

/// RAR4 store method byte (`'0'`); `'1'`..`'5'` are the compression levels.
const METHOD_STORE: u8 = 0x30;

const UNAVAIL_ENCRYPTED: &str =
    "this entry is password protected, and RAR4 encryption is not supported";

/// One file member collected during the block-chain scan.
struct Member {
    name: String,
    data_offset: u64,
    pack: u64,
    unpack: u64,
    crc: Option<u32>,
    store: bool,
    encrypted: bool,
    /// Whether this member may appear in the item index (sane name); a
    /// non-indexable member still occupies its slot in a solid group.
    indexable: bool,
    /// The RAR4 per-file solid flag: this member continues the solid stream.
    solid: bool,
    /// Whether the member's data runs past EOF (necessarily the last scanned).
    truncated: bool,
}

/// Scan a RAR4 archive's block chain, then eagerly decode any solid groups.
/// Produces the same [`RarEntry`]/item plumbing as the RAR5 path so
/// [`RarSource`](crate::rar::RarSource) serves both. Generic over the reader so
/// the fuzz harness can drive it on raw bytes.
pub(crate) fn scan_and_load<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    is_supported: &dyn Fn(&str) -> bool,
    progress: Option<&OpenProgress>,
    budget: u64,
    limits: &OpenLimits,
) -> Result<(Vec<RarEntry>, Vec<usize>), OpenError> {
    reader.seek(SeekFrom::Start(0))?;
    let mut sig = [0u8; 7];
    if read_fully(reader, &mut sig)? < 7 || sig != *b"Rar!\x1a\x07\x00" {
        return Err(OpenError::Corrupt("not a RAR4 archive".into()));
    }

    // ── Pass 1: walk the 7-byte block headers (data runs are seeked over).
    let mut groups: Vec<Vec<Member>> = Vec::new();
    let mut walked = 0usize;
    let mut name_bytes = 0u64;
    let mut pos: u64 = 7;
    while pos + 7 <= file_len {
        reader.seek(SeekFrom::Start(pos))?;
        let mut base = [0u8; 7];
        if read_fully(reader, &mut base)? < 7 {
            break; // ragged tail: serve what was scanned
        }
        let head_crc = u16::from_le_bytes([base[0], base[1]]);
        let htype = base[2];
        let hflags = u16::from_le_bytes([base[3], base[4]]);
        let head_size = u16::from_le_bytes([base[5], base[6]]) as u64;
        if head_size < 7 {
            return Err(OpenError::Corrupt("RAR4 header too small".into()));
        }
        if pos + head_size > file_len {
            break; // truncated final header
        }
        let mut hdr = vec![0u8; head_size as usize];
        reader.seek(SeekFrom::Start(pos))?;
        if read_fully(reader, &mut hdr)? != hdr.len() {
            break;
        }
        // RAR4 stores the low 16 bits of a CRC-32 over the header from the type
        // byte onward. Verify before trusting any parsed field.
        if (crc32fast::hash(&hdr[2..]) as u16) != head_crc {
            return Err(OpenError::Corrupt("RAR4 header checksum mismatch".into()));
        }
        // Data (a file's packed run) follows the header iff LONG is set.
        let add_size = if hflags & HFLAG_LONG != 0 {
            if head_size < 11 {
                return Err(OpenError::Corrupt("RAR4 long header too small".into()));
            }
            u32::from_le_bytes([hdr[7], hdr[8], hdr[9], hdr[10]]) as u64
        } else {
            0
        };
        let data_offset = pos + head_size;
        // Bytes of data following this header. For most blocks that is `add_size`
        // (the 32-bit ADD_SIZE field), but a FILE header with the LARGE flag has a
        // high dword too — a >4 GiB packed run — so a FILE block advances by its
        // member's full 64-bit packed length instead.
        let mut data_len = add_size;

        match htype {
            BLK_ENDARC => break,
            BLK_MAIN => {
                if hflags & MHD_VOLUME != 0 {
                    return Err(OpenError::Unsupported(MSG_VOLUME.into()));
                }
                // The archive-wide solid flag isn't needed directly: the
                // per-file LHD_SOLID flag already marks which files continue a
                // group, so grouping keys off that alone (mirrors RAR5).
            }
            BLK_FILE => {
                walked += 1;
                if walked > limits.max_entries {
                    return Err(OpenError::Corrupt(
                        "the archive has too many entries".into(),
                    ));
                }
                let m = parse_file_header(&hdr, head_size, hflags, data_offset, file_len)?;
                if let Some(mut m) = m {
                    data_len = m.pack;
                    if m.indexable {
                        name_bytes += m.name.len() as u64;
                        if name_bytes > limits.max_name_bytes {
                            return Err(OpenError::Corrupt(
                                "the archive has too many entries".into(),
                            ));
                        }
                    } else {
                        m.name.clear();
                    }
                    if !m.solid || groups.is_empty() {
                        groups.push(Vec::new());
                    }
                    groups.last_mut().expect("just pushed").push(m);
                }
            }
            _ => {} // comment / recovery / sub / unknown: skip via add_size
        }

        let Some(next) = data_offset.checked_add(data_len) else {
            return Err(OpenError::Corrupt("RAR4 block overruns the file".into()));
        };
        if next <= pos {
            return Err(OpenError::Corrupt(
                "RAR4 block chain does not advance".into(),
            ));
        }
        pos = next;
    }

    // ── Pass 2: resolve groups. Non-solid members go lazy; solid groups decode
    // eagerly now (the slow, budgeted, cancellable part).
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

    let mut latest: BTreeMap<String, (u64, Option<u32>, EntryData)> = BTreeMap::new();
    let mut resident = 0u64;
    for members in groups {
        if members.len() == 1 {
            let m = members.into_iter().next().expect("len checked");
            if !m.indexable || m.truncated {
                continue;
            }
            let data = if m.encrypted {
                EntryData::Unavailable(UNAVAIL_ENCRYPTED)
            } else {
                EntryData::Lazy {
                    offset: m.data_offset,
                    pack: m.pack,
                    window: 0, // rar3 discovers its window from the stream
                    store: m.store,
                    codec: RarCodec::Rar3,
                    crypt: None,
                }
            };
            upsert(&mut latest, &mut resident, m.name, m.unpack, m.crc, data);
            continue;
        }
        // Multi-member solid group. A stored or encrypted member can't be
        // decoded within the shared stream, so it poisons the group (the RAR5
        // rule, kept). Encrypted RAR4 is out of scope entirely.
        let poison = if members.iter().any(|m| m.encrypted) {
            Some(UNAVAIL_ENCRYPTED)
        } else if members.iter().any(|m| m.store) {
            Some(UNAVAIL_STORED_SOLID)
        } else {
            None
        };
        if let Some(why) = poison {
            for m in members {
                if m.indexable {
                    upsert(
                        &mut latest,
                        &mut resident,
                        m.name,
                        m.unpack,
                        m.crc,
                        EntryData::Unavailable(why),
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
        .filter(|(_, e)| is_supported(&ext_of(e.name())) && e.unpack() <= limits.ceiling)
        .map(|(i, _)| i)
        .collect();
    Ok((entries, items))
}

/// Parse one FILE header body into a [`Member`], or `None` for a directory
/// entry (no data to view). Offsets are into `hdr`, the full `head_size`-byte
/// header starting at the block's first byte.
fn parse_file_header(
    hdr: &[u8],
    head_size: u64,
    hflags: u16,
    data_offset: u64,
    file_len: u64,
) -> Result<Option<Member>, OpenError> {
    // Fixed fields after the 7-byte base: pack(4) unp(4) host(1) crc(4) time(4)
    // ver(1) method(1) namesize(2) attr(4) = 25 bytes, so the name starts at 32
    // (or 40 when the LARGE high dwords are present).
    let corrupt = || OpenError::Corrupt("truncated RAR4 file header".into());
    if head_size < 32 {
        return Err(corrupt());
    }
    let pack_lo = u32::from_le_bytes([hdr[7], hdr[8], hdr[9], hdr[10]]) as u64;
    let unp_lo = u32::from_le_bytes([hdr[11], hdr[12], hdr[13], hdr[14]]) as u64;
    let crc = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
    let method = hdr[25];
    let name_size = u16::from_le_bytes([hdr[26], hdr[27]]) as usize;

    let mut off = 32usize;
    let (pack, unpack) = if hflags & LHD_LARGE != 0 {
        if head_size < 40 {
            return Err(corrupt());
        }
        let hi_pack = u32::from_le_bytes([hdr[32], hdr[33], hdr[34], hdr[35]]) as u64;
        let hi_unp = u32::from_le_bytes([hdr[36], hdr[37], hdr[38], hdr[39]]) as u64;
        off = 40;
        ((hi_pack << 32) | pack_lo, (hi_unp << 32) | unp_lo)
    } else {
        (pack_lo, unp_lo)
    };
    let name_end = off.checked_add(name_size).filter(|&e| e <= hdr.len());
    let Some(name_end) = name_end else {
        return Err(corrupt());
    };
    // A directory (all window bits set) has no viewable data.
    if hflags & LHD_WINDOWMASK == LHD_WINDOWMASK {
        return Ok(None);
    }
    // A split file (spanning volumes) can't be served whole; we already refuse
    // multi-volume archives, but guard the flags too.
    if hflags & (LHD_SPLIT_BEFORE | LHD_SPLIT_AFTER) != 0 {
        return Err(OpenError::Unsupported(MSG_VOLUME.into()));
    }

    let raw_name = &hdr[off..name_end];
    // RAR4 Unicode names store an ASCII fallback, a NUL, then a compressed wide
    // form. We use the ASCII fallback (before the NUL) — correct for
    // ASCII-representable names, a reasonable degrade otherwise.
    let ascii = match raw_name.iter().position(|&b| b == 0) {
        Some(nul) if hflags & LHD_UNICODE != 0 => &raw_name[..nul],
        _ => raw_name,
    };
    let name = normalize_entry_name(&String::from_utf8_lossy(ascii));
    let indexable = sane_name(&name);

    Ok(Some(Member {
        name,
        data_offset,
        pack,
        unpack,
        crc: Some(crc),
        store: method == METHOD_STORE,
        encrypted: hflags & LHD_PASSWORD != 0,
        indexable,
        solid: hflags & LHD_SOLID != 0,
        truncated: data_offset.saturating_add(pack) > file_len,
    }))
}

/// Decode one multi-member solid group with a single resumable `rar3` decoder:
/// each member's byte-aligned payload is fed in turn (`begin_solid_member`
/// between them), supported members kept resident, the rest discarded. A member
/// the codec refuses marks itself and the group's tail unavailable; a truncated
/// member (necessarily the tail) is excluded and marked cut off.
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

    let mut dec = compcol::rar3::Decoder::with_unpack_size(members[0].unpack).with_solid();
    let mut give_up_at: Option<usize> = None;
    let mut in_buf = Vec::new();
    let mut out_chunk = vec![0u8; CHUNK];

    'members: for (mi, m) in members.iter().enumerate() {
        if mi > 0 {
            dec.begin_solid_member(m.unpack)
                .map_err(|_| OpenError::Corrupt("RAR4 solid resume failed".into()))?;
        }
        // Read this member's whole (byte-aligned) compressed payload into RAM.
        in_buf.clear();
        in_buf
            .try_reserve(m.pack as usize)
            .map_err(|_| OpenError::OutOfMemory)?;
        reader.seek(SeekFrom::Start(m.data_offset))?;
        in_buf.resize(m.pack as usize, 0);
        if read_fully(reader, &mut in_buf)? != in_buf.len() {
            return Err(OpenError::Corrupt("RAR4 data run truncated".into()));
        }

        let keep = m.indexable && is_supported(&ext_of(&m.name)) && m.unpack <= limits.ceiling;
        let mut buf = Vec::new();
        if keep {
            let replaced = resident_under(latest, &m.name);
            let needed = resident.saturating_sub(replaced).saturating_add(m.unpack);
            if needed > budget {
                return Err(OpenError::TooLarge { needed, budget });
            }
            buf.try_reserve_exact(m.unpack as usize)
                .map_err(|_| OpenError::OutOfMemory)?;
        }

        let mut consumed = 0usize;
        let mut produced = 0u64;
        while produced < m.unpack {
            if progress.is_some_and(|p| p.is_cancelled()) {
                return Err(OpenError::Cancelled);
            }
            let want = (m.unpack - produced).min(CHUNK as u64) as usize;
            let step = if keep {
                dec.decode(&in_buf[consumed..], &mut out_chunk[..want])
            } else {
                dec.discard_output(&in_buf[consumed..], want)
            };
            let (p, _status) = match step {
                Ok(v) => v,
                Err(compcol::Error::Unsupported) => {
                    give_up_at = Some(mi);
                    break 'members;
                }
                Err(e) => return Err(OpenError::Corrupt(format!("RAR4 decode failed: {e}"))),
            };
            consumed += p.consumed;
            if keep && p.written > 0 {
                buf.extend_from_slice(&out_chunk[..p.written]);
            }
            produced += p.written as u64;
            if let Some(pr) = progress {
                pr.add_done(p.written as u64);
            }
            if p.consumed == 0 && p.written == 0 {
                // No forward progress on `decode`: the member's whole payload is
                // in RAM, so this means the decoder wants its tail flushed.
                let (pf, _s) = match dec.finish(&mut out_chunk[..want]) {
                    Ok(v) => v,
                    Err(compcol::Error::Unsupported) => {
                        give_up_at = Some(mi);
                        break 'members;
                    }
                    Err(e) => return Err(OpenError::Corrupt(format!("RAR4 decode failed: {e}"))),
                };
                if keep && pf.written > 0 {
                    buf.extend_from_slice(&out_chunk[..pf.written]);
                }
                produced += pf.written as u64;
                if let Some(pr) = progress {
                    pr.add_done(pf.written as u64);
                }
                if pf.written == 0 {
                    return Err(OpenError::Corrupt(
                        "RAR4 solid stream ended before its declared size".into(),
                    ));
                }
            }
        }

        let data = if keep {
            match verify_crc_raw(m.crc, &buf) {
                Ok(()) => EntryData::Resident(buf),
                Err(why) => EntryData::Unavailable(why),
            }
        } else {
            EntryData::Unavailable(UNAVAIL_FILTER)
        };
        if m.indexable {
            upsert(latest, resident, m.name.clone(), m.unpack, m.crc, data);
        }
    }

    if let Some(from) = give_up_at {
        for m in members.iter().skip(from) {
            if m.indexable {
                upsert(
                    latest,
                    resident,
                    m.name.clone(),
                    m.unpack,
                    m.crc,
                    EntryData::Unavailable(UNAVAIL_FILTER),
                );
            }
        }
    }
    Ok(())
}

/// Raw-byte entry point for the cargo-fuzz harness (`fuzz/`): scan + solid
/// decode over arbitrary bytes must only ever produce `Ok`/`Err`.
#[cfg(feature = "fuzz-internals")]
pub mod fuzz {
    use super::*;

    /// Scan (and eagerly decode any solid groups of) arbitrary bytes as RAR4.
    pub fn rar4_open(data: &[u8]) {
        let limits = OpenLimits {
            ceiling: 1 << 20,
            max_entries: 4096,
            max_name_bytes: 1 << 20,
            max_expanded: 1 << 24,
        };
        let mut cur = std::io::Cursor::new(data);
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
