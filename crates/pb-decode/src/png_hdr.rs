//! **PNG `cICP`** — HDR and code-point-tagged PNGs (PNG Third Edition, 2025).
//!
//! A `cICP` chunk names the PNG's colour by H.273 code points, and it **outranks `iCCP`**.
//! With a PQ (16) or HLG (18) transfer the PNG is **HDR**: the samples are PQ/HLG code
//! values, and shown as if sRGB they come out flat and dim — what Blaze did before (#146's
//! audit found it). Here they become scene-linear scRGB (1.0 = 203 nits, BT.709 primaries),
//! the same convention as every other HDR still, through the shared `finalize_hdr_scrgb`.
//! This is also the read-back that HDR PNG *export* needs (#89 rung 5, Rule 4).
//!
//! An SDR `cICP` (e.g. Display P3 + the sRGB transfer) just sets the colour transform.
//!
//! The `image` crate decodes the samples; it ignores `cICP`, so the chunk is read here.

use std::sync::OnceLock;

use crate::color::{hlg_to_scrgb, pq_to_scrgb, ColorTransform};

const SIGNATURE: [u8; 8] = *b"\x89PNG\r\n\x1a\n";
const TRC_PQ: u8 = 16;
const TRC_HLG: u8 = 18;

/// A PNG's `cICP` code points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PngCicp {
    pub primaries: u8,
    pub transfer: u8,
    pub full_range: bool,
}

impl PngCicp {
    /// PQ or HLG — the PNG holds HDR code values.
    pub(crate) fn is_hdr(&self) -> bool {
        matches!(self.transfer, TRC_PQ | TRC_HLG)
    }

    /// The display transform for an **SDR** `cICP` (PNG's matrix is always 0, RGB).
    pub(crate) fn sdr_transform(&self) -> ColorTransform {
        ColorTransform::from_cicp(self.primaries, self.transfer, 0, self.full_range)
    }

    /// The export colour description for an SDR `cICP`.
    pub(crate) fn export_cicp(&self) -> pb_encode::Cicp {
        pb_encode::Cicp {
            primaries: self.primaries,
            transfer: self.transfer,
            matrix: 0,
            full_range: self.full_range,
        }
    }
}

/// The `cICP` chunk, when a valid one precedes the image data (the spec requires it to). A
/// bad CRC, a wrong length or a non-zero matrix (PNG is RGB) is ignored — the image then
/// decodes as an ordinary PNG. Walks chunk headers only; reads no image data.
pub(crate) fn read_cicp(bytes: &[u8]) -> Option<PngCicp> {
    if bytes.len() < SIGNATURE.len() || bytes[..SIGNATURE.len()] != SIGNATURE {
        return None;
    }
    let mut pos = SIGNATURE.len();
    while pos + 8 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().ok()?) as usize;
        let kind = &bytes[pos + 4..pos + 8];
        // Checked: a hostile length must end the walk, not wrap into a valid offset.
        let end = pos.checked_add(12)?.checked_add(len)?;
        if end > bytes.len() {
            return None;
        }
        match kind {
            b"cICP" => {
                let data = &bytes[pos + 8..pos + 8 + len];
                let crc = u32::from_be_bytes(bytes[end - 4..end].try_into().ok()?);
                if len != 4 || crate::png_text::crc32(kind, data) != crc || data[2] != 0 {
                    return None;
                }
                return Some(PngCicp {
                    primaries: data[0],
                    transfer: data[1],
                    full_range: data[3] != 0,
                });
            }
            // cICP must come before the image data; one after it doesn't count.
            b"IDAT" | b"IEND" => return None,
            _ => {}
        }
        pos = end;
    }
    None
}

/// Code value (16-bit, full range) → scene-linear scRGB, one table per curve, built once
/// per process (256 KB each): a lookup per sample instead of a `powf`.
fn lut(transfer: u8) -> &'static [f32] {
    static PQ: OnceLock<Vec<f32>> = OnceLock::new();
    static HLG: OnceLock<Vec<f32>> = OnceLock::new();
    let (cell, curve): (_, fn(f32) -> f32) = if transfer == TRC_HLG {
        (&HLG, hlg_to_scrgb)
    } else {
        (&PQ, pq_to_scrgb)
    };
    cell.get_or_init(|| (0..=u16::MAX).map(|v| curve(v as f32 / 65535.0)).collect())
}

/// Narrow-range ("video") RGB → full range: 16..235 (scaled to 16 bits) → 0..65535.
fn widen(v: u16) -> u16 {
    let e = (v as f32 / 65535.0 - 16.0 / 255.0) * (255.0 / 219.0);
    (e.clamp(0.0, 1.0) * 65535.0).round() as u16
}

/// An HDR PNG's pixels as scene-linear scRGB RGBA f32 (straight alpha, like the display
/// path's RGBA8), in BT.709 primaries. `cicp` must be HDR ([`PngCicp::is_hdr`]).
pub(crate) fn linear_scrgb(img: &image::DynamicImage, cicp: &PngCicp) -> (Vec<f32>, u32, u32) {
    let rgba = img.to_rgba16();
    let (w, h) = rgba.dimensions();
    let lut = lut(cicp.transfer);
    // Source-linear → BT.709-linear: the CICP primaries with a linear transfer (8).
    let m = ColorTransform::from_cicp(cicp.primaries, 8, 0, true).matrix;
    let code = |v: u16| {
        let v = if cicp.full_range { v } else { widen(v) };
        lut[v as usize]
    };
    let mut out = Vec::with_capacity(rgba.as_raw().len());
    for px in rgba.pixels() {
        let [r, g, b, a] = px.0;
        let (r, g, b) = (code(r), code(g), code(b));
        out.extend_from_slice(&[
            m[0][0] * r + m[0][1] * g + m[0][2] * b,
            m[1][0] * r + m[1][1] * g + m[1][2] * b,
            m[2][0] * r + m[2][1] * g + m[2][2] * b,
            a as f32 / 65535.0,
        ]);
    }
    (out, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16-bit RGB PNG of `px`, optionally with `cicp` = `[primaries, transfer, matrix,
    /// full_range]`, written by the `png` crate (independent of the reader under test). ⚠ png
    /// 0.18 *reads* `cICP` but never writes it from `Info`, so it goes in as a raw chunk —
    /// after the header, before the image data, where the spec wants it.
    fn png_with(cicp: Option<[u8; 4]>, w: u32, h: u32, px: &[u16]) -> Vec<u8> {
        let mut info = png::Info::with_size(w, h);
        info.color_type = png::ColorType::Rgb;
        info.bit_depth = png::BitDepth::Sixteen;
        let mut out = Vec::new();
        {
            let enc = png::Encoder::with_info(&mut out, info).unwrap();
            let mut wr = enc.write_header().unwrap();
            if let Some(c) = cicp {
                wr.write_chunk(png::chunk::cICP, &c).unwrap();
            }
            let data: Vec<u8> = px.iter().flat_map(|v| v.to_be_bytes()).collect();
            wr.write_image_data(&data).unwrap();
        }
        out
    }

    #[test]
    fn reads_the_cicp_chunk_and_ignores_bad_ones() {
        let hdr = png_with(Some([9, 16, 0, 1]), 1, 1, &[0, 0, 0]);
        assert_eq!(
            read_cicp(&hdr),
            Some(PngCicp {
                primaries: 9,
                transfer: 16,
                full_range: true
            })
        );
        assert!(read_cicp(&hdr).unwrap().is_hdr());
        assert_eq!(
            read_cicp(&png_with(None, 1, 1, &[0, 0, 0])),
            None,
            "untagged"
        );
        // PNG is RGB: a non-zero matrix is an invalid chunk, ignored.
        assert_eq!(
            read_cicp(&png_with(Some([9, 16, 9, 1]), 1, 1, &[0; 3])),
            None
        );
        // A flipped bit in the payload fails the CRC.
        let mut bad = hdr.clone();
        let i = bad.windows(4).position(|w| w == b"cICP").unwrap();
        bad[i + 5] ^= 1;
        assert_eq!(read_cicp(&bad), None, "bad CRC");
        // Truncated / hostile lengths end the walk without panicking.
        assert_eq!(read_cicp(&hdr[..i + 2]), None);
        let mut huge = hdr;
        huge[i - 4..i].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(read_cicp(&huge), None);
        assert_eq!(read_cicp(b"not a png"), None);
    }

    /// The 203-nit PQ code value decodes to 1.0 — through the full-range table and, from
    /// its narrow-range encoding, through `widen`.
    #[test]
    fn hdr_samples_become_scene_linear_in_both_ranges() {
        let white = (0.5807 * 65535.0f32).round() as u16;
        let narrow = ((16.0 + 0.5807 * 219.0) / 255.0 * 65535.0f32).round() as u16;
        for (full, v) in [(1u8, white), (0, narrow)] {
            let bytes = png_with(Some([1, 16, 0, full]), 1, 1, &[v, v, v]);
            let cicp = read_cicp(&bytes).unwrap();
            let img = image::load_from_memory(&bytes).unwrap();
            let (px, w, h) = linear_scrgb(&img, &cicp);
            assert_eq!((w, h), (1, 1));
            for c in &px[..3] {
                assert!((c - 1.0).abs() < 0.01, "full_range={full}: {c}");
            }
            assert_eq!(px[3], 1.0, "opaque");
        }
    }

    /// BT.2020 primaries reach BT.709: grey stays grey, and a saturated 2020 green leaves the
    /// 709 gamut (negative red/blue — scRGB carries out-of-gamut colour that way).
    #[test]
    fn bt2020_primaries_map_to_bt709() {
        let c = (0.5807 * 65535.0f32).round() as u16;
        let bytes = png_with(Some([9, 16, 0, 1]), 2, 1, &[c, c, c, 0, c, 0]);
        let cicp = read_cicp(&bytes).unwrap();
        let (px, ..) = linear_scrgb(&image::load_from_memory(&bytes).unwrap(), &cicp);
        for v in &px[..3] {
            assert!((v - 1.0).abs() < 0.02, "grey preserved: {:?}", &px[..3]);
        }
        assert!(
            px[4] < 0.0 && px[6] < 0.0,
            "2020 green is out of 709: {:?}",
            &px[4..7]
        );
    }

    /// An SDR cICP (Display P3 + sRGB transfer) sets the display transform — and outranks
    /// an embedded profile — instead of being ignored.
    #[test]
    fn an_sdr_cicp_sets_the_colour_transform() {
        let bytes = png_with(Some([12, 13, 0, 1]), 1, 1, &[30000, 20000, 10000]);
        let cicp = read_cicp(&bytes).unwrap();
        assert!(!cicp.is_hdr());
        let img = crate::decode_named_bytes("p3.png", &bytes, None, false).unwrap();
        assert_eq!(img.color, cicp.sdr_transform());
        assert_ne!(img.color, ColorTransform::srgb(), "P3 isn't passthrough");
    }
}
