//! The JPEG decode backend: `zune-jpeg` (pure Rust, no system deps).
//!
//! Decodes to RGBA8; shared finalization (`common::finalize`) then applies EXIF
//! orientation so photos display upright and decode-to-fit downscales to the
//! display size. zune-jpeg has no native scaled-decode path, so we decode at full
//! resolution and downscale. (turbojpeg, which *does* scale on decode, slots in
//! behind the same `ImageDecoder` seam later.)

use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::{common, ColorTransform, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

/// JPEG decoder backed by `zune-jpeg`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZuneJpegDecoder;

impl ImageDecoder for ZuneJpegDecoder {
    fn can_decode(&self, bytes: &[u8]) -> bool {
        // JPEG SOI marker.
        bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let (rgba, w, h, icc, recovered) = decode_rgba_with_icc(req.bytes)?;
        // Shared finalize: EXIF orientation (from the JPEG bytes) + decode-to-fit.
        let mut img = common::finalize(rgba, w, h, Some(req.bytes), "JPEG", req.fit, false)?;
        // An embedded ICC profile (Adobe RGB / ProPhoto exports) drives in-shader
        // color management; untagged JPEGs stay sRGB passthrough.
        if let Some(icc) = icc {
            img.color = ColorTransform::from_icc(&icc);
        }
        // Malformed-but-recovered (strict mode rejected it, lenient decoded it anyway):
        // carry the reason for the details-panel notice (task #127).
        img.recovered = recovered;
        Ok(img)
    }

    fn name(&self) -> &'static str {
        "zune-jpeg"
    }
}

/// Decode a JPEG to full-resolution RGBA8 with **no** orientation or fit applied
/// (the shared `finalize` does those). Exposed for the RAW backend, which decodes
/// an embedded preview JPEG but must apply orientation from the RAW *container*,
/// not the preview's own (often absent) EXIF.
pub(crate) fn decode_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), DecodeError> {
    let (rgba, w, h, _icc, _recovered) = decode_rgba_with_icc(bytes)?;
    Ok((rgba, w, h))
}

/// Like [`decode_rgba`] but also returns the embedded ICC profile (APP2 chunks,
/// reassembled by zune) when present, for in-shader color management, plus a
/// `recovered` reason when the file was **malformed but salvaged**.
///
/// **The recovery ladder, rung 1 (task #127).** A viewer is a renderer, not a
/// validator: zune's default *strict* mode rejects spec-violating-but-decodable
/// files (e.g. a JPEG from some encoders that leaves junk *between* segments —
/// `"Extra bytes between headers"`), which macOS ImageIO / `sips` / Preview all
/// decode without complaint. So we try **strict first** (a pass proves the file is
/// clean and costs a good file nothing extra — the common case), and only on a
/// strict error retry **lenient**, flagging the result `recovered` with the strict
/// error as the human-readable reason. A file lenient *also* can't salvage (a hard
/// truncation → `"Exhausted data"`) errors out here and the dispatch layer's next
/// rung (the tolerant OS codec) takes over.
#[allow(clippy::type_complexity)]
pub(crate) fn decode_rgba_with_icc(
    bytes: &[u8],
) -> Result<(Vec<u8>, u32, u32, Option<Vec<u8>>, Option<String>), DecodeError> {
    match decode_rgba_with_opts(bytes, DecoderOptions::default()) {
        Ok((rgba, w, h, icc)) => Ok((rgba, w, h, icc, None)),
        Err(strict_err) => {
            // Strict rejected it. Retry lenient; if that salvages the pixels, report
            // it as recovered so the details panel can tell the full story. Keep the
            // reason concise and human — the bare zune message without our
            // `DecodeError` wrapper or its internal `[strict-mode]:` prefix.
            let reason = match &strict_err {
                DecodeError::Corrupt(s) => s.clone(),
                other => other.to_string(),
            };
            let reason = reason
                .trim()
                .trim_matches('"')
                .trim()
                .trim_start_matches("[strict-mode]:")
                .trim()
                .to_string();
            let lenient = DecoderOptions::default().set_strict_mode(false);
            let (rgba, w, h, icc) = decode_rgba_with_opts(bytes, lenient)?;
            Ok((rgba, w, h, icc, Some(reason)))
        }
    }
}

/// One zune decode pass under the given options → RGBA8 + dimensions + ICC.
#[allow(clippy::type_complexity)]
fn decode_rgba_with_opts(
    bytes: &[u8],
    opts: DecoderOptions,
) -> Result<(Vec<u8>, u32, u32, Option<Vec<u8>>), DecodeError> {
    let mut decoder = JpegDecoder::new_with_options(bytes, opts);
    let out = decoder
        .decode()
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    let (w, h) = decoder
        .dimensions()
        .ok_or_else(|| DecodeError::Corrupt("missing dimensions".into()))?;
    let (w, h) = (w as u32, h as u32);
    let n = (w as usize) * (h as usize);
    if n == 0 {
        return Err(DecodeError::Corrupt("zero-size image".into()));
    }
    let icc = decoder.icc_profile();
    // zune emits RGB for color JPEGs and Luma for grayscale; normalize to RGBA.
    let rgba = match out.len() / n {
        4 => out,
        3 => rgb_to_rgba(&out, n),
        1 => luma_to_rgba(&out, n),
        other => return Err(DecodeError::Corrupt(format!("unexpected {other} channels"))),
    };
    Ok((rgba, w, h, icc))
}

fn rgb_to_rgba(rgb: &[u8], n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n * 4];
    for i in 0..n {
        out[i * 4..i * 4 + 3].copy_from_slice(&rgb[i * 3..i * 3 + 3]);
        out[i * 4 + 3] = 255;
    }
    out
}

fn luma_to_rgba(luma: &[u8], n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n * 4];
    for i in 0..n {
        let g = luma[i];
        out[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, 255]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_decode_sniffs_jpeg_magic() {
        let d = ZuneJpegDecoder;
        assert!(d.can_decode(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(!d.can_decode(&[0x89, 0x50, 0x4E, 0x47])); // PNG
        assert!(!d.can_decode(&[0xFF]));
    }

    #[test]
    fn rgb_expands_to_rgba() {
        let rgb = [1, 2, 3, 4, 5, 6];
        let out = rgb_to_rgba(&rgb, 2);
        assert_eq!(out, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn luma_expands_to_rgba() {
        let out = luma_to_rgba(&[7, 9], 2);
        assert_eq!(out, vec![7, 7, 7, 255, 9, 9, 9, 255]);
    }

    /// Inject stray non-marker bytes at the boundary *after* the first marker
    /// segment — the "Extra bytes between headers" malformation a real
    /// government-export JPEG hit (task #127), which strict mode rejects and lenient
    /// recovers. The leading `FF D8 FF` stays intact so the content sniff (and the
    /// real dispatch) still route it to the JPEG backend, exactly like the real file.
    fn stray_bytes_between_headers(good: &[u8]) -> Vec<u8> {
        assert_eq!(&good[..2], &[0xFF, 0xD8], "SOI");
        assert_eq!(good[2], 0xFF, "first marker");
        let seg_len = u16::from_be_bytes([good[4], good[5]]) as usize; // incl. the 2 len bytes
        let boundary = 2 + 2 + seg_len; // start of the next marker
        let mut bad = Vec::with_capacity(good.len() + 3);
        bad.extend_from_slice(&good[..boundary]);
        bad.extend_from_slice(&[0x00, 0x00, 0x00]); // junk between headers
        bad.extend_from_slice(&good[boundary..]);
        bad
    }

    #[test]
    fn a_clean_jpeg_decodes_strict_and_is_not_flagged_recovered() {
        let rgba = vec![120u8; 16 * 16 * 4];
        let jpeg = crate::encode_jpeg_rgba8(&rgba, 16, 16, 90).expect("encode");
        let (_rgba, w, h, _icc, recovered) = decode_rgba_with_icc(&jpeg).expect("decode");
        assert_eq!((w, h), (16, 16));
        assert!(
            recovered.is_none(),
            "a well-formed JPEG must not be flagged recovered"
        );
    }

    #[test]
    fn a_malformed_jpeg_recovers_lenient_and_carries_the_reason() {
        let rgba = vec![200u8; 16 * 16 * 4];
        let good = crate::encode_jpeg_rgba8(&rgba, 16, 16, 90).expect("encode");
        let bad = stray_bytes_between_headers(&good);
        // Strict rejects it (that's the whole point); the ladder falls to lenient.
        assert!(
            JpegDecoder::new_with_options(&bad, DecoderOptions::default())
                .decode()
                .is_err(),
            "fixture must actually trip strict mode"
        );
        let (rgba_out, w, h, _icc, recovered) =
            decode_rgba_with_icc(&bad).expect("lenient must salvage it");
        assert_eq!((w, h), (16, 16));
        assert_eq!(rgba_out.len(), 16 * 16 * 4);
        assert!(
            recovered.is_some(),
            "a salvaged malformed JPEG must be flagged recovered"
        );
    }
}
