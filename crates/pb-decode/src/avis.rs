//! ISOBMFF `avis` (animated AVIF) demuxer + probe, and the dav1d decode
//! pipeline that consumes it — task #76 (plan phases 4-6).
//!
//! [`probe_avis`] is the single supported-file decision, shared by detection
//! (`detect_animation`) and decode — so a play hint is only ever shown for a
//! file this backend can actually attempt (the no-dead-hint rule). It parses
//! the real sample tables, not just the `ftyp` brand, and **rejects** rather
//! than guesses: `msf1`-only (HEVC — dav1d can't), fragmented `moof`, `stz2`,
//! encrypted sample entries, multiple `av01` sample descriptions, and HDR
//! (PQ/HLG — the WIC still path renders those to fp16; an 8-bit SDR clamp of
//! every frame would be strictly worse, ADR in the task 76 plan).
//!
//! The demux half is pure Rust with no dav1d dependency and is deliberately
//! self-contained (its own box walker and `colr` parsing via `crate::color`),
//! so it compiles and unit-tests on every platform — including a libheif-less
//! Linux `cargo test`, where `crate::isobmff` doesn't exist — and feeds the
//! fuzz harness. Only [`decode_avis`] (bottom of the file) needs the linked
//! dav1d and is gated on `av1_dav1d`.

use crate::ColorTransform;

/// Everything decode needs from the container, produced once by [`probe_avis`].
pub(crate) struct AvisInfo {
    /// `av1C` configOBUs (typically the sequence header). Legally may be empty
    /// — then the sequence header is in-band in the first sample.
    pub config_obus: Vec<u8>,
    /// Samples in decode order (which for AV1 shown frames is display order —
    /// the binding forbids composition offsets for `av01` tracks).
    pub samples: Vec<Sample>,
    /// `mdhd` timescale in ticks/second (validated nonzero).
    pub timescale: u32,
    /// Display transform from the color trak's own `colr` box (primaries/TRC →
    /// applied in-shader downstream). `None` = sRGB passthrough.
    pub color: Option<ColorTransform>,
    /// Track-scoped `nclx` CICP values, when present — drives the YUV→RGB
    /// matrix/range (a *display* transform alone can't: it's `None` for sRGB).
    pub nclx: Option<Nclx>,
    /// True when the sample table was cut at [`crate::animation::MAX_FRAMES`].
    pub truncated: bool,
}

/// Raw CICP code points from the trak's `nclx` `colr` box. Decode consumes
/// `matrix`/`full_range`; `primaries`/`transfer` are carried for the tests and
/// the display-transform bookkeeping (HDR transfer is rejected at parse time).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Nclx {
    #[allow(dead_code)]
    pub primaries: u8,
    #[allow(dead_code)]
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

/// One sample: an absolute, validated byte range in the file + its duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sample {
    pub offset: usize,
    pub size: usize,
    pub duration_ticks: u64,
}

/// CICP transfer characteristics that need the HDR float path: PQ / HLG.
fn is_hdr_transfer(t: u8) -> bool {
    matches!(t, 16 | 18)
}

// ── Bounded box reader ───────────────────────────────────────────────────────

/// Upper bound on boxes visited across the whole probe — a hostile file can't
/// make the walk quadratic (each visit is O(1) plus a header read).
const MAX_BOXES: usize = 4096;

/// One parsed box header: `(fourcc, payload_start, box_end)` — or `None` at
/// the clean end of the enclosing container.
type BoxHeader = Option<([u8; 4], usize, usize)>;

struct Walk {
    visited: usize,
}

impl Walk {
    fn new() -> Self {
        Self { visited: 0 }
    }

    /// Read the box header at `pos` within `[pos, end)`. Returns
    /// `(fourcc, payload_start, box_end)`; handles 32-bit, 64-bit (`size==1`)
    /// and to-end (`size==0`) forms with checked arithmetic.
    fn read_box(&mut self, data: &[u8], pos: usize, end: usize) -> Result<BoxHeader, &'static str> {
        if pos == end {
            return Ok(None);
        }
        self.visited += 1;
        if self.visited > MAX_BOXES {
            return Err("box walk limit exceeded");
        }
        if pos + 8 > end {
            return Err("truncated box header");
        }
        let size32 = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap());
        let typ: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
        let (payload, box_end) = match size32 {
            0 => (pos + 8, end), // "to end of enclosing container"
            1 => {
                if pos + 16 > end {
                    return Err("truncated largesize header");
                }
                let size64 = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().unwrap());
                if size64 < 16 {
                    return Err("largesize too small");
                }
                let box_end = pos
                    .checked_add(usize::try_from(size64).map_err(|_| "box size overflow")?)
                    .ok_or("box size overflow")?;
                (pos + 16, box_end)
            }
            s if s < 8 => return Err("box size too small"),
            s => (pos + 8, pos + s as usize),
        };
        if box_end > end {
            return Err("box overruns container");
        }
        Ok(Some((typ, payload, box_end)))
    }

    /// Find the first direct child of `[start, end)` with fourcc `want`.
    fn find(
        &mut self,
        data: &[u8],
        start: usize,
        end: usize,
        want: &[u8; 4],
    ) -> Result<Option<(usize, usize)>, &'static str> {
        let mut pos = start;
        while let Some((typ, payload, box_end)) = self.read_box(data, pos, end)? {
            if &typ == want {
                return Ok(Some((payload, box_end)));
            }
            pos = box_end;
        }
        Ok(None)
    }
}

/// FullBox header: returns (version, payload-after-version-and-flags).
fn full_box(data: &[u8], payload: usize, end: usize) -> Result<(u8, usize), &'static str> {
    if payload + 4 > end {
        return Err("truncated full box");
    }
    Ok((data[payload], payload + 4))
}

fn be_u16(data: &[u8], at: usize) -> Result<u16, &'static str> {
    data.get(at..at + 2)
        .map(|b| u16::from_be_bytes(b.try_into().unwrap()))
        .ok_or("truncated u16")
}

fn be_u32(data: &[u8], at: usize) -> Result<u32, &'static str> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        .ok_or("truncated u32")
}

fn be_u64(data: &[u8], at: usize) -> Result<u64, &'static str> {
    data.get(at..at + 8)
        .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
        .ok_or("truncated u64")
}

// ── The probe ────────────────────────────────────────────────────────────────

/// Parse `bytes` as a decodable `avis` animation. `Err(reason)` covers both
/// "not an avis at all" and "an avis this backend deliberately doesn't play" —
/// either way detection shows no hint and the still path handles the file.
pub(crate) fn probe_avis(bytes: &[u8]) -> Result<AvisInfo, &'static str> {
    let mut w = Walk::new();

    // Root pass: require an `ftyp` carrying the `avis` brand, reject `moof`
    // (fragmented — no `moov` sample tables to read), find `moov`.
    let mut pos = 0usize;
    let mut saw_avis_brand = false;
    let mut moov: Option<(usize, usize)> = None;
    while let Some((typ, payload, box_end)) = w.read_box(bytes, pos, bytes.len())? {
        match &typ {
            b"ftyp" => {
                // major brand + every compatible brand (4 bytes each after the
                // 4-byte minor version).
                let mut at = payload;
                let mut idx = 0;
                while at + 4 <= box_end {
                    // Skip the minor-version word (second entry).
                    if idx != 1 && &bytes[at..at + 4] == b"avis" {
                        saw_avis_brand = true;
                    }
                    at += 4;
                    idx += 1;
                }
            }
            b"moof" => return Err("fragmented (moof) not supported"),
            b"moov" => moov = Some((payload, box_end)),
            _ => {}
        }
        pos = box_end;
    }
    if !saw_avis_brand {
        // Covers stills (`avif`/`mif1`) and HEVC sequences (`msf1` without
        // `avis`) — dav1d cannot decode the latter, so no hint.
        return Err("no avis brand");
    }
    let (moov_start, moov_end) = moov.ok_or("no moov box")?;

    // Find the color track: handler `pict` with a single `av01` sample entry.
    // Alpha auxiliary tracks are handler `auxv` (also av01) and must never be
    // selected — the AVIF spec requires the color sequence's handler to be
    // `pict`, so selection is positive, not "skip auxv".
    let mut pos = moov_start;
    let mut track_err: Option<&'static str> = None;
    while let Some((typ, payload, box_end)) = w.read_box(bytes, pos, moov_end)? {
        if &typ == b"trak" {
            match parse_trak(bytes, payload, box_end, &mut w) {
                Ok(Some(info)) => return Ok(info),
                // A pict/av01 trak that *fails validation* is a hard reject
                // (encrypted, stz2, multi-stsd, HDR, malformed tables) — do
                // not fall through to some other track.
                Ok(None) => {}
                Err(e) => track_err = Some(e),
            }
        }
        pos = box_end;
    }
    Err(track_err.unwrap_or("no av01 pict track"))
}

/// Parse one `trak`. `Ok(None)` = not the color track (wrong handler / not
/// av01) — keep looking. `Err` = it IS the color track but unsupported or
/// malformed — reject the file.
fn parse_trak(
    bytes: &[u8],
    trak_start: usize,
    trak_end: usize,
    w: &mut Walk,
) -> Result<Option<AvisInfo>, &'static str> {
    let Some((mdia_start, mdia_end)) = w.find(bytes, trak_start, trak_end, b"mdia")? else {
        return Ok(None);
    };

    // hdlr: version/flags(4) + pre_defined(4) + handler_type(4).
    let Some((hdlr_start, hdlr_end)) = w.find(bytes, mdia_start, mdia_end, b"hdlr")? else {
        return Ok(None);
    };
    if hdlr_start + 12 > hdlr_end {
        return Ok(None);
    }
    if &bytes[hdlr_start + 8..hdlr_start + 12] != b"pict" {
        return Ok(None); // auxv alpha track, or something else entirely
    }

    // mdhd: timescale (version 0 and 1 layouts).
    let (mdhd_start, mdhd_end) = w
        .find(bytes, mdia_start, mdia_end, b"mdhd")?
        .ok_or("no mdhd")?;
    let (ver, after_vf) = full_box(bytes, mdhd_start, mdhd_end)?;
    let timescale = match ver {
        0 => be_u32(bytes, after_vf + 8)?, // creation(4) + modification(4)
        1 => be_u32(bytes, after_vf + 16)?, // creation(8) + modification(8)
        _ => return Err("unsupported mdhd version"),
    };
    if timescale == 0 {
        return Err("mdhd timescale is zero");
    }

    let (minf_start, minf_end) = w
        .find(bytes, mdia_start, mdia_end, b"minf")?
        .ok_or("no minf")?;
    let (stbl_start, stbl_end) = w
        .find(bytes, minf_start, minf_end, b"stbl")?
        .ok_or("no stbl")?;

    // stsd: exactly one sample description, and it must be `av01`. Anything
    // else — `encv` (encrypted), a second description (per-sample `av1C`
    // switching via stsc's sample_description_index) — is a reject.
    let (stsd_start, stsd_end) = w
        .find(bytes, stbl_start, stbl_end, b"stsd")?
        .ok_or("no stsd")?;
    let (_, after_vf) = full_box(bytes, stsd_start, stsd_end)?;
    let entry_count = be_u32(bytes, after_vf)?;
    if entry_count != 1 {
        return Err("multiple sample descriptions");
    }
    let (entry_typ, entry_payload, entry_end) = w
        .read_box(bytes, after_vf + 4, stsd_end)?
        .ok_or("empty stsd")?;
    if &entry_typ != b"av01" {
        return Err(if &entry_typ == b"encv" {
            "encrypted track"
        } else {
            "sample entry is not av01"
        });
    }
    // VisualSampleEntry fixed fields: 6 reserved + 2 data_reference_index +
    // 16 pre_defined + 2 width + 2 height + 8 resolution + 4 reserved +
    // 2 frame_count + 32 compressorname + 2 depth + 2 pre_defined = 78 bytes;
    // child boxes (av1C, colr, pasp, ...) follow.
    let children = entry_payload
        .checked_add(78)
        .filter(|&c| c <= entry_end)
        .ok_or("truncated av01 sample entry")?;

    // av1C: 4 fixed bytes (marker/version, profile/level, flags, delay), then
    // the configOBUs. Required by the AVIF spec, but its configOBUs may be
    // empty (sequence header in-band in sample 0).
    let (av1c_start, av1c_end) = w
        .find(bytes, children, entry_end, b"av1C")?
        .ok_or("no av1C box")?;
    if av1c_start + 4 > av1c_end {
        return Err("truncated av1C");
    }
    let config_obus = bytes[av1c_start + 4..av1c_end].to_vec();

    // colr, scoped to THIS sample entry (an alpha track's colr can never win).
    // nclx gives both the display transform and the YUV matrix/range; prof/
    // rICC gives only the display transform (matrix then comes from the
    // bitstream sequence header).
    let mut color: Option<ColorTransform> = None;
    let mut nclx: Option<Nclx> = None;
    if let Some((colr_start, colr_end)) = w.find(bytes, children, entry_end, b"colr")? {
        if colr_start + 4 > colr_end {
            return Err("truncated colr");
        }
        match &bytes[colr_start..colr_start + 4] {
            b"nclx" => {
                let primaries = be_u16(bytes, colr_start + 4)? as u8;
                let transfer = be_u16(bytes, colr_start + 6)? as u8;
                let matrix = be_u16(bytes, colr_start + 8)? as u8;
                let full_range = (*bytes.get(colr_start + 10).ok_or("truncated nclx")? & 0x80) != 0;
                if is_hdr_transfer(transfer) {
                    // HDR routes to the still path (fp16 WIC) — decided in the
                    // task 76 plan; an SDR clamp of every frame is worse.
                    return Err("hdr transfer (pq/hlg)");
                }
                nclx = Some(Nclx {
                    primaries,
                    transfer,
                    matrix,
                    full_range,
                });
                let t = ColorTransform::from_cicp(primaries, transfer, matrix, full_range);
                if t.enabled {
                    color = Some(t);
                }
            }
            b"prof" | b"rICC" => {
                let icc = &bytes[colr_start + 4..colr_end];
                // Validate against the ICC's own size header (same rule as
                // isobmff::parse_colr_at): a mismatch means a junk box.
                let icc_size = be_u32(icc, 0)? as usize;
                if icc_size == icc.len() {
                    let t = ColorTransform::from_icc(icc);
                    if t.enabled {
                        color = Some(t);
                    }
                }
            }
            _ => {}
        }
    }

    // ── Sample tables ────────────────────────────────────────────────────────

    // stts → per-sample durations (run expansion, capped at MAX_FRAMES).
    let (stts_start, stts_end) = w
        .find(bytes, stbl_start, stbl_end, b"stts")?
        .ok_or("no stts")?;
    let (_, after_vf) = full_box(bytes, stts_start, stts_end)?;
    let run_count = be_u32(bytes, after_vf)? as usize;
    let mut durations: Vec<u64> = Vec::new();
    let mut truncated = false;
    let cap = crate::animation::MAX_FRAMES;
    'runs: for i in 0..run_count {
        let at = after_vf + 4 + i * 8;
        let count = be_u32(bytes, at)? as usize;
        let delta = be_u32(bytes, at + 4)? as u64;
        for _ in 0..count {
            if durations.len() >= cap {
                truncated = true;
                break 'runs;
            }
            durations.push(delta);
        }
    }
    if durations.is_empty() {
        return Err("no samples in stts");
    }

    // stsz (stz2 = reject) → sizes.
    if w.find(bytes, stbl_start, stbl_end, b"stz2")?.is_some() {
        return Err("compact stz2 not supported");
    }
    let (stsz_start, stsz_end) = w
        .find(bytes, stbl_start, stbl_end, b"stsz")?
        .ok_or("no stsz")?;
    let (_, after_vf) = full_box(bytes, stsz_start, stsz_end)?;
    let const_size = be_u32(bytes, after_vf)? as usize;
    let stsz_count = be_u32(bytes, after_vf + 4)? as usize;
    if stsz_count == 0 {
        return Err("no samples in stsz");
    }
    // The playable sample count: agree across tables, bounded by MAX_FRAMES.
    let n = stsz_count.min(durations.len());
    let size_at = |i: usize| -> Result<usize, &'static str> {
        if const_size != 0 {
            Ok(const_size)
        } else {
            Ok(be_u32(bytes, after_vf + 8 + i * 4)? as usize)
        }
    };
    truncated |= stsz_count > n;

    // stsc runs (sample_description_index must reference the single stsd
    // entry) + stco/co64 chunk offsets → absolute per-sample ranges.
    let (stsc_start, stsc_end) = w
        .find(bytes, stbl_start, stbl_end, b"stsc")?
        .ok_or("no stsc")?;
    let (_, after_vf) = full_box(bytes, stsc_start, stsc_end)?;
    let stsc_runs = be_u32(bytes, after_vf)? as usize;
    let stsc_at = after_vf + 4;
    if stsc_runs == 0 {
        return Err("empty stsc");
    }

    // Materialize the chunk offsets up front. The declared count is validated
    // against the box's own byte range *before* the allocation, so a hostile
    // count can't reserve gigabytes — the vec is bounded by the file size.
    let chunk_offsets: Vec<u64> =
        if let Some((stco_start, stco_end)) = w.find(bytes, stbl_start, stbl_end, b"stco")? {
            let (_, a) = full_box(bytes, stco_start, stco_end)?;
            let count = be_u32(bytes, a)? as usize;
            let table = count.checked_mul(4).ok_or("stco count overflow")?;
            if (a + 4).checked_add(table).is_none_or(|end| end > stco_end) {
                return Err("stco count overruns box");
            }
            (0..count)
                .map(|i| Ok(be_u32(bytes, a + 4 + i * 4)? as u64))
                .collect::<Result<_, &'static str>>()?
        } else if let Some((co64_start, co64_end)) = w.find(bytes, stbl_start, stbl_end, b"co64")? {
            let (_, a) = full_box(bytes, co64_start, co64_end)?;
            let count = be_u32(bytes, a)? as usize;
            let table = count.checked_mul(8).ok_or("co64 count overflow")?;
            if (a + 4).checked_add(table).is_none_or(|end| end > co64_end) {
                return Err("co64 count overruns box");
            }
            (0..count)
                .map(|i| be_u64(bytes, a + 4 + i * 8))
                .collect::<Result<_, &'static str>>()?
        } else {
            return Err("no stco/co64");
        };
    let chunk_count = chunk_offsets.len();
    if chunk_count == 0 {
        return Err("no chunks");
    }

    // Walk chunks in order, pulling the samples-per-chunk from the applicable
    // stsc run. stsc first_chunk values are 1-based and strictly increasing.
    let mut samples: Vec<Sample> = Vec::with_capacity(n);
    let mut run_idx = 0usize;
    let mut sample_idx = 0usize;
    'chunks: for (chunk, &base) in chunk_offsets.iter().enumerate() {
        // Advance to the last run whose first_chunk <= this chunk (1-based).
        while run_idx + 1 < stsc_runs {
            let next_first = be_u32(bytes, stsc_at + (run_idx + 1) * 12)? as usize;
            if next_first <= chunk + 1 {
                run_idx += 1;
            } else {
                break;
            }
        }
        let first_chunk = be_u32(bytes, stsc_at + run_idx * 12)? as usize;
        if chunk == 0 && first_chunk != 1 {
            return Err("stsc does not start at chunk 1");
        }
        let per_chunk = be_u32(bytes, stsc_at + run_idx * 12 + 4)? as usize;
        let sdi = be_u32(bytes, stsc_at + run_idx * 12 + 8)?;
        if sdi != 1 {
            return Err("sample_description_index out of range");
        }
        let mut at = usize::try_from(base).map_err(|_| "chunk offset overflow")?;
        for _ in 0..per_chunk {
            if sample_idx >= n {
                break 'chunks;
            }
            let size = size_at(sample_idx)?;
            if size == 0 {
                return Err("empty sample");
            }
            let end = at.checked_add(size).ok_or("sample range overflow")?;
            if end > bytes.len() {
                return Err("sample out of range");
            }
            samples.push(Sample {
                offset: at,
                size,
                duration_ticks: durations[sample_idx],
            });
            at = end;
            sample_idx += 1;
        }
    }
    if samples.len() < n {
        return Err("sample table inconsistent (chunks exhausted)");
    }

    Ok(Some(AvisInfo {
        config_obus,
        samples,
        timescale,
        color,
        nclx,
        truncated,
    }))
}

// ── Decode pipeline (needs the linked dav1d) ─────────────────────────────────

#[cfg(av1_dav1d)]
mod decode {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::{is_hdr_transfer, probe_avis, AvisInfo};
    use crate::animation::{normalize_delay, AnimFrame, Animation, AnimationKind};
    use crate::animation::{MAX_DECODED_BYTES, MAX_FRAMES};
    use crate::dav1d::{Data, Decoder, Picture, SendStatus};
    use crate::yuv::{self, Matrix, PixelLayout, Plane};
    use crate::{common, ColorTransform, DecodeError, FitBox};

    /// dav1d threads for the sequence decode. Small on purpose: the decode
    /// pool already parallelizes across images, so a wide dav1d would
    /// oversubscribe. `max_frame_delay = 1` keeps output sequential and the
    /// per-picture memory low. A/B'd in plan phase 9 before shipping numbers.
    const N_THREADS: i32 = 4;
    const MAX_FRAME_DELAY: i32 = 1;

    /// Ceiling on one decoded picture's RGBA footprint before conversion is
    /// even attempted (a hostile sequence header can't make us allocate
    /// gigabytes for a single frame): 1 GiB = e.g. ~16k × 16k.
    const MAX_SOURCE_RGBA_BYTES: u64 = 1 << 30;

    /// Ceiling on a single frame's display duration — matches the Media
    /// Foundation motion path's clamp; hostile `stts` deltas can't freeze
    /// playback for minutes on one frame.
    const MAX_FRAME_DELAY_SECS: f64 = 2.0;

    /// Decode a probed avis into a looping [`Animation`] (task #76).
    ///
    /// Cancellation: checked before every sample send and every picture
    /// conversion; a set flag returns an error (the caller is navigating away
    /// and discards the result — same contract as the streaming producers).
    pub(crate) fn decode_avis(
        bytes: &[u8],
        fit: Option<FitBox>,
        cancel: &AtomicBool,
    ) -> Result<Animation, DecodeError> {
        let info =
            probe_avis(bytes).map_err(|e| DecodeError::Corrupt(format!("avis probe: {e}")))?;
        let mut dec = Decoder::open(N_THREADS, MAX_FRAME_DELAY)?;

        let mut out = Collector::new(&info, fit);

        // Feed the av1C configOBUs first (sequence header), when present.
        // Cookie -1: a config feed should yield no picture; if one somehow
        // appears, the collector falls back to the first sample's duration.
        if !info.config_obus.is_empty() {
            feed(&mut dec, &info.config_obus, -1, cancel, &mut out)?;
        }

        'samples: for (idx, s) in info.samples.iter().enumerate() {
            if out.done() {
                break 'samples;
            }
            if cancel.load(Ordering::Relaxed) {
                return Err(DecodeError::Corrupt("cancelled".into()));
            }
            let tu = &bytes[s.offset..s.offset + s.size]; // probe-validated range
            feed(&mut dec, tu, idx as i64, cancel, &mut out)?;
        }

        // Flush the delayed-frame queue.
        while !out.done() {
            if cancel.load(Ordering::Relaxed) {
                return Err(DecodeError::Corrupt("cancelled".into()));
            }
            match dec.next_picture()? {
                Some(pic) => out.push(pic)?,
                None => break,
            }
        }

        out.finish(info.truncated)
    }

    /// Send one temporal unit through the robust loop: re-send the same
    /// `Data` while unconsumed (dav1d may advance it in place across EAGAIN),
    /// draining pictures on every pass. Bounded so a pathological
    /// EAGAIN-without-progress can't spin forever.
    fn feed(
        dec: &mut Decoder,
        tu: &[u8],
        cookie: i64,
        cancel: &AtomicBool,
        out: &mut Collector<'_>,
    ) -> Result<(), DecodeError> {
        let mut data = Data::new(tu, cookie)?;
        let mut stalls = 0u32;
        while data.remaining() > 0 && !out.done() {
            if cancel.load(Ordering::Relaxed) {
                return Err(DecodeError::Corrupt("cancelled".into()));
            }
            let before = data.remaining();
            let status = dec.send(&mut data)?;
            let mut drained = false;
            while let Some(pic) = dec.next_picture()? {
                out.push(pic)?;
                drained = true;
                if out.done() {
                    return Ok(());
                }
            }
            if status == SendStatus::Again && !drained && data.remaining() == before {
                stalls += 1;
                if stalls > 64 {
                    return Err(DecodeError::Corrupt("dav1d made no progress".into()));
                }
            } else {
                stalls = 0;
            }
        }
        Ok(())
    }

    /// Accumulates converted frames under the projected caps (a frame that
    /// would cross [`MAX_DECODED_BYTES`] is never retained — the byte cap is
    /// hard, unlike the historical `collect_frames` push-then-check).
    struct Collector<'a> {
        info: &'a AvisInfo,
        fit: Option<FitBox>,
        frames: Vec<AnimFrame>,
        total_bytes: u64,
        canvas: Option<(u32, u32)>,
        truncated: bool,
    }

    impl<'a> Collector<'a> {
        fn new(info: &'a AvisInfo, fit: Option<FitBox>) -> Self {
            Self {
                info,
                fit,
                frames: Vec::new(),
                total_bytes: 0,
                canvas: None,
                truncated: false,
            }
        }

        /// True once the caps say "stop decoding" — the bounded prefix plays.
        fn done(&self) -> bool {
            self.truncated || self.frames.len() >= MAX_FRAMES
        }

        fn push(&mut self, pic: Picture) -> Result<(), DecodeError> {
            // HDR backstop: a colr-less HDR file slips the probe; the first
            // picture's sequence header tells the truth. Route to the still.
            if self.info.nclx.is_none() {
                let trc = pic.transfer();
                if trc >= 0 && is_hdr_transfer(trc as u8) {
                    return Err(DecodeError::Corrupt("hdr avis (seq header)".into()));
                }
            }
            let rgba = convert(&pic, self.info)?;
            let (w, h) = (pic.width(), pic.height());
            let cookie = pic.cookie();
            drop(pic); // release the dav1d frame buffer before the downscale

            let (rgba, w, h) = match self.fit {
                Some(fit) => common::downscale_to_fit(rgba, w, h, fit)?,
                None => (rgba, w, h),
            };
            match self.canvas {
                None => self.canvas = Some((w, h)),
                Some(c) if c != (w, h) => {
                    return Err(DecodeError::Corrupt(
                        "avis render size changed mid-sequence".into(),
                    ));
                }
                Some(_) => {}
            }
            // Projected byte cap: never retain the frame that crosses it.
            let projected = self.total_bytes.saturating_add(rgba.len() as u64);
            if projected > MAX_DECODED_BYTES {
                self.truncated = true;
                return Ok(());
            }
            self.total_bytes = projected;

            // Duration via the sample cookie (robust to zero-picture TUs).
            let ticks = usize::try_from(cookie)
                .ok()
                .and_then(|i| self.info.samples.get(i))
                .map(|s| s.duration_ticks)
                .unwrap_or_else(|| self.info.samples.first().map_or(0, |s| s.duration_ticks));
            let secs = (ticks as f64 / self.info.timescale as f64).clamp(0.0, MAX_FRAME_DELAY_SECS);
            let delay = normalize_delay(AnimationKind::Heif, Duration::from_secs_f64(secs));

            self.frames.push(AnimFrame {
                rgba,
                width: w,
                height: h,
                delay,
            });
            Ok(())
        }

        fn finish(self, demux_truncated: bool) -> Result<Animation, DecodeError> {
            if self.frames.is_empty() {
                return Err(DecodeError::Corrupt("avis decoded no frames".into()));
            }
            let (width, height) = self.canvas.unwrap();
            Ok(Animation {
                kind: AnimationKind::Heif,
                width,
                height,
                frames: self.frames,
                // Loops forever, the GIF convention — avis has no loop
                // metadata; matches the Linux FFmpeg sequence path.
                loop_count: 0,
                codec: "AVIF",
                color: self.info.color.unwrap_or_else(ColorTransform::srgb),
                truncated: self.truncated || demux_truncated,
            })
        }
    }

    /// One picture → straight-alpha RGBA8 in the source gamut. The YUV matrix
    /// and range come from the trak `colr` nclx when present (MIAF: colr
    /// overrides the bitstream), else the picture's own sequence header.
    fn convert(pic: &Picture, info: &AvisInfo) -> Result<Vec<u8>, DecodeError> {
        let (w, h) = (pic.width(), pic.height());
        if w == 0 || h == 0 {
            return Err(DecodeError::Corrupt("empty avis picture".into()));
        }
        if (w as u64) * (h as u64) * 4 > MAX_SOURCE_RGBA_BYTES {
            return Err(DecodeError::Corrupt("avis frame too large".into()));
        }
        let bpc = pic.bpc();
        if !matches!(bpc, 8 | 10 | 12) {
            return Err(DecodeError::Corrupt(format!("avis bpc {bpc}")));
        }
        let layout = match pic.layout() {
            crate::dav1d::LAYOUT_I400 => PixelLayout::I400,
            crate::dav1d::LAYOUT_I420 => PixelLayout::I420,
            crate::dav1d::LAYOUT_I422 => PixelLayout::I422,
            crate::dav1d::LAYOUT_I444 => PixelLayout::I444,
            other => return Err(DecodeError::Corrupt(format!("avis layout {other}"))),
        };
        let (mc, full_range) = match info.nclx {
            Some(n) => (n.matrix as i32, n.full_range),
            None => (pic.matrix(), pic.full_range()),
        };
        let matrix = match mc {
            0 => Matrix::Identity,
            1 => Matrix::Bt709,
            // 2 = unspecified: BT.601 by convention for image files (decided
            // in the plan; tested). 5/6 are the 601 family proper.
            2 | 5 | 6 => Matrix::Bt601,
            9 => Matrix::Bt2020,
            other => {
                return Err(DecodeError::Corrupt(format!(
                    "avis matrix coefficients {other} unsupported"
                )));
            }
        };

        let plane = |idx: i32| -> Result<Plane<'_>, DecodeError> {
            let (ptr, stride) = pic
                .plane(idx)
                .ok_or_else(|| DecodeError::Corrupt("missing avis plane".into()))?;
            if stride <= 0 {
                return Err(DecodeError::Corrupt("negative avis plane stride".into()));
            }
            let stride = stride as usize;
            // Chroma rows are halved only for I420; luma, I422 and I444
            // planes all span the full height.
            let rows = if idx > 0 && layout == PixelLayout::I420 {
                (h as usize).div_ceil(2)
            } else {
                h as usize
            };
            // SAFETY: dav1d allocates stride×rows readable bytes per plane
            // (aligned picture allocator); the Picture keeps them alive for
            // this borrow.
            let data = unsafe {
                std::slice::from_raw_parts(
                    ptr,
                    stride
                        .checked_mul(rows)
                        .ok_or_else(|| DecodeError::Corrupt("avis plane size overflow".into()))?,
                )
            };
            Ok(Plane { data, stride })
        };

        let y = plane(0)?;
        let (u, v) = if layout == PixelLayout::I400 {
            (None, None)
        } else {
            (Some(plane(1)?), Some(plane(2)?))
        };
        yuv::to_rgba8(y, u, v, w, h, bpc as u32, layout, matrix, full_range)
    }
}

#[cfg(av1_dav1d)]
pub(crate) use decode::decode_avis;

// Unit tests live in `tests` below for the demux half (every platform) and in
// the integration section gated on the linked decoder.
#[cfg(test)]
mod tests;
