//! The JPEG decode backend: `zune-jpeg` (pure Rust, no system deps).
//!
//! Decodes to RGBA8; shared finalization (`common::finalize`) then applies EXIF
//! orientation so photos display upright and decode-to-fit downscales to the
//! display size. zune-jpeg has no native scaled-decode path, so we decode at full
//! resolution and downscale. (turbojpeg, which *does* scale on decode, slots in
//! behind the same `ImageDecoder` seam later.)

use zune_jpeg::JpegDecoder;

use crate::{common, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

/// JPEG decoder backed by `zune-jpeg`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZuneJpegDecoder;

impl ImageDecoder for ZuneJpegDecoder {
    fn can_decode(&self, bytes: &[u8]) -> bool {
        // JPEG SOI marker.
        bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let (rgba, w, h) = decode_rgba(req.bytes)?;
        // Shared finalize: EXIF orientation (from the JPEG bytes) + decode-to-fit.
        common::finalize(rgba, w, h, Some(req.bytes), "JPEG", req.fit, false)
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
    let mut decoder = JpegDecoder::new(bytes);
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
    // zune emits RGB for color JPEGs and Luma for grayscale; normalize to RGBA.
    let rgba = match out.len() / n {
        4 => out,
        3 => rgb_to_rgba(&out, n),
        1 => luma_to_rgba(&out, n),
        other => return Err(DecodeError::Corrupt(format!("unexpected {other} channels"))),
    };
    Ok((rgba, w, h))
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
}
