//! Baseline JPEG encoder over the `image` crate — moved here from `pb-decode` (export-plan.md
//! rung 0c), where AI Describe used it to shrink an image for an OpenAI-compatible endpoint.
//! Rung 2 puts it behind the `Encoder` trait as the panel's JPEG option; rung 4 may swap in
//! mozjpeg behind the same seam.

use crate::EncodeError;

/// Encode a tightly-packed RGBA8 buffer to baseline JPEG at `quality` (1–100). Alpha is
/// dropped — JPEG has none; the describe caller feeds opaque, already-tone-mapped sRGB8.
/// Never on the view path (privacy #2: only an explicit command reaches it).
pub fn encode_jpeg_rgba8(rgba: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>, EncodeError> {
    let expected = (w as usize) * (h as usize) * 4;
    if w == 0 || h == 0 || rgba.len() != expected {
        return Err(EncodeError::Malformed);
    }
    let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
    for px in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
        .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
        .map_err(|e| EncodeError::Codec(format!("jpeg encode failed: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_a_valid_jpeg_that_decodes_back() {
        let (w, h) = (4u32, 4u32);
        let rgba: Vec<u8> = std::iter::repeat_n([128u8, 130, 132, 255], (w * h) as usize)
            .flatten()
            .collect();
        let jpeg = encode_jpeg_rgba8(&rgba, w, h, 85).expect("encode");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "SOI");
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "EOI");
        let back = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        assert_eq!((back.width(), back.height()), (w, h));
    }

    #[test]
    fn rejects_a_size_mismatch() {
        assert!(encode_jpeg_rgba8(&[0; 3], 4, 4, 85).is_err());
        assert!(encode_jpeg_rgba8(&[], 0, 0, 85).is_err());
    }
}
