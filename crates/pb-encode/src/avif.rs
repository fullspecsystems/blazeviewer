//! **AVIF** over `ravif` (BSD-3) and rav1e (BSD-2) — pure Rust, so the same encoder on every
//! platform (export-plan.md rung 4). 8-bit input with alpha; ravif encodes 10-bit AV1, full
//! range, 4:4:4.
//!
//! **The colour profile.** ravif (and the `avif-serialize` container writer under it) can only
//! tag sRGB: one `nclx` `colr` property saying BT.709 primaries and the sRGB curve. A Display P3
//! photo — every iPhone picture — needs its profile, so [`add_icc`] edits the finished file the
//! way libavif writes one with a profile: a `colr` `prof` property holding the ICC, associated
//! with the primary item, and the `nclx` primaries and transfer set to *unspecified* (2) so the
//! profile governs while its matrix still describes the YUV. Only the `meta` box changes; the
//! AV1 payload is moved, never touched, and `iloc` is re-pointed at it.

use crate::encoder::{Caps, Encoder};
use crate::types::{ExportImage, OutputColor};
use crate::EncodeError;

/// rav1e's speed (1 slowest – 10 fastest). Chosen from corpus timings (export-plan.md rung 4):
/// AVIF is the slow codec, and the live preview re-encodes on every settings change.
pub const SPEED: u8 = 6;

/// The AVIF export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvifEncoder {
    /// 1–100, ravif's scale.
    pub quality: u8,
}

impl Encoder for AvifEncoder {
    fn caps(&self) -> Caps {
        Caps::EIGHT_BIT_ALPHA
    }
    fn extension(&self) -> &'static str {
        "avif"
    }
    fn lossless(&self) -> bool {
        false
    }
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError> {
        encode_avif(img, self.quality)
    }
}

fn codec(why: impl std::fmt::Display) -> EncodeError {
    EncodeError::Codec(format!("avif: {why}"))
}

/// Half the machine: the export runs at background priority beside browsing, and rav1e would
/// otherwise take every core for seconds.
fn threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| (n.get() / 2).max(1))
}

/// Encode a prepared 8-bit image as AVIF.
pub fn encode_avif(img: &ExportImage, quality: u8) -> Result<Vec<u8>, EncodeError> {
    encode_avif_at(img, quality, SPEED)
}

/// [`encode_avif`] at another rav1e `speed` (1–10) — for measuring, not the app.
pub fn encode_avif_at(img: &ExportImage, quality: u8, speed: u8) -> Result<Vec<u8>, EncodeError> {
    let (px, alpha) = crate::px::rgb8(img)?;
    let q = quality.clamp(1, 100) as f32;
    // With alpha, ravif encodes colour and alpha at once, each with this many threads: split
    // the budget so the two together still take half the machine.
    let threads = if alpha {
        (threads() / 2).max(1)
    } else {
        threads()
    };
    let enc = ravif::Encoder::new()
        .with_quality(q)
        .with_alpha_quality(q.max(90.0))
        .with_speed(speed.clamp(1, 10))
        .with_num_threads(Some(threads));
    let (w, h) = (img.width as usize, img.height as usize);
    let encoded = if alpha {
        let px: &[ravif::RGBA8] =
            bytemuck::try_cast_slice(&px).map_err(|_| EncodeError::Malformed)?;
        enc.encode_rgba(ravif::Img::new(px, w, h))
    } else {
        let px: &[ravif::RGB8] =
            bytemuck::try_cast_slice(&px).map_err(|_| EncodeError::Malformed)?;
        enc.encode_rgb(ravif::Img::new(px, w, h))
    }
    .map_err(codec)?;
    match &img.color {
        OutputColor::Srgb => Ok(encoded.avif_file),
        OutputColor::Icc(profile) => add_icc(&encoded.avif_file, profile),
    }
}

// ---- The ISOBMFF edit -------------------------------------------------------------------------

/// A box: where it starts, its total size, its type, and its header length (8, or 16 with a
/// 64-bit size).
#[derive(Debug, Clone, Copy)]
struct Bx {
    pos: usize,
    size: usize,
    typ: [u8; 4],
    hdr: usize,
}

impl Bx {
    fn end(&self) -> usize {
        self.pos + self.size
    }
    fn body(&self) -> usize {
        self.pos + self.hdr
    }
}

fn be(b: &[u8], at: usize, n: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(n)?)?;
    Some(s.iter().fold(0u64, |acc, &x| acc << 8 | x as u64))
}

/// The boxes laid end to end in `buf[start..end]`.
fn children(buf: &[u8], start: usize, end: usize) -> Option<Vec<Bx>> {
    let mut out = Vec::new();
    let mut pos = start;
    while pos < end {
        let size32 = be(buf, pos, 4)? as usize;
        let typ: [u8; 4] = buf.get(pos + 4..pos + 8)?.try_into().ok()?;
        let (size, hdr) = match size32 {
            0 => (end - pos, 8),
            1 => (usize::try_from(be(buf, pos + 8, 8)?).ok()?, 16),
            n => (n, 8),
        };
        if size < hdr || pos + size > end {
            return None;
        }
        out.push(Bx {
            pos,
            size,
            typ,
            hdr,
        });
        pos += size;
    }
    Some(out)
}

fn find(boxes: &[Bx], typ: &[u8; 4]) -> Option<Bx> {
    boxes.iter().copied().find(|b| &b.typ == typ)
}

/// A box around `payload`.
fn boxed(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(typ);
    out.extend_from_slice(payload);
    out
}

/// One item's property associations: (essential, 1-based property index).
type Assoc = Vec<(bool, u16)>;

/// `ipma` as read: version, flags, and (item ID, associations) per entry.
type Ipma = (u8, u32, Vec<(u32, Assoc)>);

/// `ipma`'s version, flags and entries.
fn parse_ipma(buf: &[u8], b: Bx) -> Option<Ipma> {
    let body = b.body();
    let version = *buf.get(body)?;
    let flags = be(buf, body + 1, 3)? as u32;
    let mut at = body + 4;
    let count = be(buf, at, 4)? as usize;
    at += 4;
    let mut entries = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let id_len = if version < 1 { 2 } else { 4 };
        let id = be(buf, at, id_len)? as u32;
        at += id_len;
        let n = *buf.get(at)? as usize;
        at += 1;
        let mut assoc = Vec::with_capacity(n);
        for _ in 0..n {
            if flags & 1 == 1 {
                let v = be(buf, at, 2)? as u16;
                assoc.push((v & 0x8000 != 0, v & 0x7FFF));
                at += 2;
            } else {
                let v = *buf.get(at)?;
                assoc.push((v & 0x80 != 0, (v & 0x7F) as u16));
                at += 1;
            }
        }
        entries.push((id, assoc));
    }
    (at <= b.end()).then_some((version, flags, entries))
}

fn write_ipma(version: u8, flags: u32, entries: &[(u32, Assoc)]) -> Vec<u8> {
    // Indices past 127 need the 16-bit form.
    let wide = flags & 1 == 1 || entries.iter().flat_map(|(_, a)| a).any(|&(_, i)| i > 0x7F);
    let flags = if wide { flags | 1 } else { flags };
    let mut p = vec![version];
    p.extend_from_slice(&flags.to_be_bytes()[1..]);
    p.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (id, assoc) in entries {
        if version < 1 {
            p.extend_from_slice(&(*id as u16).to_be_bytes());
        } else {
            p.extend_from_slice(&id.to_be_bytes());
        }
        p.push(assoc.len() as u8);
        for &(essential, index) in assoc {
            if wide {
                p.extend_from_slice(&((essential as u16) << 15 | index).to_be_bytes());
            } else {
                p.push((essential as u8) << 7 | index as u8);
            }
        }
    }
    boxed(b"ipma", &p)
}

/// Add `delta` to every file offset in `iloc` (inside `meta`, at `meta_buf[b]`) that points past
/// `after`: the payload moved down by `delta` when `meta` grew. Refuses layouts it can't vouch
/// for rather than guess.
fn shift_iloc(meta_buf: &mut [u8], b: Bx, after: u64, delta: u64) -> Option<()> {
    let body = b.body();
    let version = *meta_buf.get(body)?;
    if version > 2 {
        return None;
    }
    let mut at = body + 4;
    let sizes = *meta_buf.get(at)?;
    let (offset_size, length_size) = ((sizes >> 4) as usize, (sizes & 0xF) as usize);
    let sizes2 = *meta_buf.get(at + 1)?;
    let base_size = (sizes2 >> 4) as usize;
    let index_size = if version >= 1 {
        (sizes2 & 0xF) as usize
    } else {
        0
    };
    at += 2;
    let count_len = if version < 2 { 2 } else { 4 };
    let count = be(meta_buf, at, count_len)? as usize;
    at += count_len;

    let put = |buf: &mut [u8], at: usize, n: usize, v: u64| -> Option<()> {
        if n < 8 && v >> (8 * n) != 0 {
            return None; // the shifted offset no longer fits
        }
        for i in 0..n {
            *buf.get_mut(at + i)? = (v >> (8 * (n - 1 - i))) as u8;
        }
        Some(())
    };

    for _ in 0..count {
        at += count_len; // item_ID
        let method = if version >= 1 {
            let m = be(meta_buf, at, 2)? & 0xF;
            at += 2;
            m
        } else {
            0
        };
        at += 2; // data_reference_index
        let base = if base_size > 0 {
            be(meta_buf, at, base_size)?
        } else {
            0
        };
        let base_at = at;
        at += base_size;
        let extents = be(meta_buf, at, 2)? as usize;
        at += 2;
        // Only file offsets (method 0) move; `idat` (1) and item (2) offsets are relative.
        let moves = method == 0;
        if moves && base > 0 {
            if base < after {
                return None; // data inside meta: not a layout we write
            }
            put(meta_buf, base_at, base_size, base + delta)?;
        }
        for _ in 0..extents {
            at += index_size;
            let off = if offset_size > 0 {
                be(meta_buf, at, offset_size)?
            } else {
                0
            };
            if moves && base == 0 {
                if offset_size == 0 || off < after {
                    return None;
                }
                put(meta_buf, at, offset_size, off + delta)?;
            }
            at += offset_size + length_size;
        }
    }
    (at <= b.end()).then_some(())
}

/// Give the AVIF's primary image the ICC `profile` (see the module docs).
pub fn add_icc(file: &[u8], profile: &[u8]) -> Result<Vec<u8>, EncodeError> {
    patch_icc(file, profile).ok_or_else(|| codec("couldn't add the color profile"))
}

fn patch_icc(file: &[u8], profile: &[u8]) -> Option<Vec<u8>> {
    let top = children(file, 0, file.len())?;
    let meta = find(&top, b"meta")?;
    let meta_kids = children(file, meta.body() + 4, meta.end())?;
    let pitm = find(&meta_kids, b"pitm")?;
    let primary = match *file.get(pitm.body())? {
        0 => be(file, pitm.body() + 4, 2)?,
        _ => be(file, pitm.body() + 4, 4)?,
    } as u32;
    let iprp = find(&meta_kids, b"iprp")?;
    let iprp_kids = children(file, iprp.body(), iprp.end())?;
    let ipco = find(&iprp_kids, b"ipco")?;
    let ipma = find(&iprp_kids, b"ipma")?;
    let props = children(file, ipco.body(), ipco.end())?;

    // The profile goes just before the nclx colr (first wins for readers that take one), or
    // last when there is none. `k` is its 1-based property index.
    let is_nclx = |b: &Bx| &b.typ == b"colr" && file.get(b.body()..b.body() + 4) == Some(b"nclx");
    let k = props.iter().position(is_nclx).unwrap_or(props.len()) + 1;
    let mut prof_payload = b"prof".to_vec();
    prof_payload.extend_from_slice(profile);
    let prof = boxed(b"colr", &prof_payload);

    let mut new_ipco_payload = Vec::new();
    for (i, p) in props.iter().enumerate() {
        if i + 1 == k {
            new_ipco_payload.extend_from_slice(&prof);
        }
        let mut bytes = file[p.pos..p.end()].to_vec();
        if is_nclx(p) {
            // colour_primaries and transfer_characteristics → unspecified; keep the matrix.
            let at = p.hdr + 4;
            bytes.get_mut(at..at + 4)?.copy_from_slice(&[0, 2, 0, 2]);
        }
        new_ipco_payload.extend_from_slice(&bytes);
    }
    if k == props.len() + 1 {
        new_ipco_payload.extend_from_slice(&prof);
    }
    let new_ipco = boxed(b"ipco", &new_ipco_payload);

    let (version, flags, mut entries) = parse_ipma(file, ipma)?;
    for (_, assoc) in &mut entries {
        for (_, index) in assoc.iter_mut() {
            if *index as usize >= k {
                *index += 1;
            }
        }
    }
    let (_, assoc) = entries.iter_mut().find(|(id, _)| *id == primary)?;
    if assoc.len() >= 255 {
        return None;
    }
    // In association order too, the profile comes before the nclx (now at k + 1).
    let at = assoc
        .iter()
        .position(|&(_, i)| i as usize == k + 1)
        .unwrap_or(assoc.len());
    assoc.insert(at, (false, k as u16));
    let new_ipma = write_ipma(version, flags, &entries);

    let mut new_iprp_payload = Vec::new();
    for c in &iprp_kids {
        match &c.typ {
            b"ipco" => new_iprp_payload.extend_from_slice(&new_ipco),
            b"ipma" => new_iprp_payload.extend_from_slice(&new_ipma),
            _ => new_iprp_payload.extend_from_slice(&file[c.pos..c.end()]),
        }
    }
    let new_iprp = boxed(b"iprp", &new_iprp_payload);

    let mut new_meta_payload = file[meta.body()..meta.body() + 4].to_vec(); // version + flags
    for c in &meta_kids {
        if &c.typ == b"iprp" {
            new_meta_payload.extend_from_slice(&new_iprp);
        } else {
            new_meta_payload.extend_from_slice(&file[c.pos..c.end()]);
        }
    }
    let mut new_meta = boxed(b"meta", &new_meta_payload);
    let delta = new_meta.len().checked_sub(meta.size)? as u64;

    // Re-point iloc: everything after the old meta moved down by `delta`.
    let new_kids = children(&new_meta, 12, new_meta.len())?;
    let iloc = find(&new_kids, b"iloc")?;
    shift_iloc(&mut new_meta, iloc, meta.end() as u64, delta)?;

    let mut out = Vec::with_capacity(file.len() + delta as usize);
    out.extend_from_slice(&file[..meta.pos]);
    out.extend_from_slice(&new_meta);
    out.extend_from_slice(&file[meta.end()..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Channels, Samples};

    fn image(w: u32, h: u32, channels: Channels, px: Vec<u8>, color: OutputColor) -> ExportImage {
        ExportImage {
            width: w,
            height: h,
            channels,
            samples: Samples::U8(px),
            color,
            notes: Vec::new(),
        }
    }

    fn gradient(w: u32, h: u32, n: usize) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [(x * 255 / w) as u8, (y * 255 / h) as u8, 90, 200]
                    .into_iter()
                    .take(n)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// The structure a reader walks: (primary item's property boxes, the bytes each `iloc`
    /// extent points at).
    fn walk(file: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let top = children(file, 0, file.len()).unwrap();
        let meta = find(&top, b"meta").unwrap();
        let kids = children(file, meta.body() + 4, meta.end()).unwrap();
        let pitm = find(&kids, b"pitm").unwrap();
        let primary = be(file, pitm.body() + 4, 2).unwrap() as u32;
        let iprp = find(&kids, b"iprp").unwrap();
        let ik = children(file, iprp.body(), iprp.end()).unwrap();
        let ipco = find(&ik, b"ipco").unwrap();
        let props = children(file, ipco.body(), ipco.end()).unwrap();
        let (_, _, entries) = parse_ipma(file, find(&ik, b"ipma").unwrap()).unwrap();
        let (_, assoc) = entries.iter().find(|(id, _)| *id == primary).unwrap();
        let prim_props = assoc
            .iter()
            .map(|&(_, i)| {
                let p = props[i as usize - 1];
                file[p.pos..p.end()].to_vec()
            })
            .collect();
        // iloc v0, 4-byte offsets and lengths, no base (what avif-serialize writes).
        let iloc = find(&kids, b"iloc").unwrap();
        let mut at = iloc.body() + 6;
        let count = be(file, at, 2).unwrap() as usize;
        at += 2;
        let mut extents = Vec::new();
        for _ in 0..count {
            at += 4;
            let n = be(file, at, 2).unwrap() as usize;
            at += 2;
            for _ in 0..n {
                let off = be(file, at, 4).unwrap() as usize;
                let len = be(file, at + 4, 4).unwrap() as usize;
                extents.push(file[off..off + len].to_vec());
                at += 8;
            }
        }
        (prim_props, extents)
    }

    fn colr(props: &[Vec<u8>], kind: &[u8; 4]) -> Option<Vec<u8>> {
        props
            .iter()
            .find(|p| &p[4..8] == b"colr" && &p[8..12] == kind)
            .cloned()
    }

    /// sRGB stays as ravif writes it: no `colr` at all, which AVIF readers take as sRGB with
    /// the BT.601 matrix — exactly what was encoded (the AV1 header says the same).
    #[test]
    fn srgb_carries_no_colour_box() {
        let avif = encode_avif(
            &image(
                32,
                24,
                Channels::Rgb,
                gradient(32, 24, 3),
                OutputColor::Srgb,
            ),
            70,
        )
        .unwrap();
        assert_eq!(&avif[4..12], b"ftypavif");
        let (props, extents) = walk(&avif);
        assert!(props.iter().all(|p| &p[4..8] != b"colr"), "no colr");
        assert_eq!(extents.len(), 1, "one image, no alpha");
    }

    /// A file that does carry an nclx (here built with a BT.709 matrix): the profile goes in
    /// front of it, the nclx keeps its matrix but leaves primaries and transfer to the profile,
    /// and the payload is re-pointed.
    #[test]
    fn an_existing_nclx_defers_to_the_profile() {
        let mut a = avif_serialize::Aviffy::new();
        a.matrix_coefficients(avif_serialize::constants::MatrixCoefficients::Bt709);
        let payload = [0x12u8, 0, 0x0A, 1, 2, 3, 4, 5, 6, 7];
        let file = a.to_vec(&payload, None, 4, 4, 8);
        let (before, _) = walk(&file);
        assert_eq!(
            &colr(&before, b"nclx").unwrap()[12..18],
            &[0, 1, 0, 13, 0, 1]
        );
        let icc = crate::icc::display_p3_profile();
        let out = add_icc(&file, &icc).unwrap();
        let (props, extents) = walk(&out);
        assert_eq!(
            &colr(&props, b"nclx").unwrap()[12..18],
            &[0, 2, 0, 2, 0, 1],
            "matrix kept"
        );
        assert_eq!(&colr(&props, b"prof").unwrap()[12..], icc.as_slice());
        let first = props.iter().position(|p| &p[4..8] == b"colr").unwrap();
        assert_eq!(&props[first][8..12], b"prof", "the profile comes first");
        assert_eq!(extents, vec![payload.to_vec()]);
    }

    /// With a profile: the primary item carries it byte for byte, the nclx defers to it, and
    /// every payload (colour and alpha) is exactly where `iloc` now says.
    #[test]
    fn a_profile_is_added_and_the_payload_is_repointed() {
        let p3 = crate::icc::display_p3_profile();
        let src = image(
            32,
            24,
            Channels::Rgba,
            gradient(32, 24, 4),
            OutputColor::Srgb,
        );
        let plain = encode_avif(&src, 70).unwrap();
        let tagged = add_icc(&plain, &p3).unwrap();
        let (props, extents) = walk(&tagged);
        let prof = colr(&props, b"prof").expect("prof on the primary item");
        assert_eq!(&prof[12..], p3.as_slice());
        assert!(
            colr(&props, b"nclx").is_none(),
            "ravif writes none for sRGB + BT.601"
        );
        let (_, before) = walk(&plain);
        assert_eq!(extents, before, "colour and alpha payloads untouched");
        assert_eq!(extents.len(), 2);
        // The whole thing through `encode_avif` matches.
        let direct = encode_avif(
            &ExportImage {
                color: OutputColor::Icc(p3),
                ..src
            },
            70,
        )
        .unwrap();
        assert_eq!(direct, tagged);
    }

    #[test]
    fn quality_trades_size_and_garbage_is_refused() {
        let src = image(
            48,
            32,
            Channels::Rgb,
            gradient(48, 32, 3),
            OutputColor::Srgb,
        );
        let hi = encode_avif(&src, 90).unwrap();
        let lo = encode_avif(&src, 20).unwrap();
        assert!(lo.len() < hi.len(), "{} < {}", lo.len(), hi.len());
        assert!(add_icc(b"not an avif", b"icc").is_err());
        let deep = ExportImage {
            samples: Samples::U16(vec![0; 3]),
            ..image(1, 1, Channels::Rgb, vec![], OutputColor::Srgb)
        };
        assert_eq!(encode_avif(&deep, 70), Err(EncodeError::Malformed));
    }

    /// Writing more than 127 properties switches ipma to its 16-bit form instead of wrapping.
    #[test]
    fn ipma_widens_when_an_index_needs_it() {
        let entries = vec![(1u32, vec![(true, 3u16), (false, 200)])];
        let b = write_ipma(0, 0, &entries);
        let bx = children(&b, 0, b.len()).unwrap()[0];
        let (v, flags, back) = parse_ipma(&b, bx).unwrap();
        assert_eq!((v, flags & 1), (0, 1));
        assert_eq!(back, entries);
    }
}
