//! Baseline JPEG over the `image` crate (4:2:2 chroma). Two callers:
//! - [`encode_jpeg_rgba8`] — AI Describe's downscaled, already-sRGB upload (moved here from
//!   `pb-decode` in rung 0c);
//! - [`JpegEncoder`] — the export format (rung 2): an `ExportImage` prepared with
//!   [`JpegEncoder::CAPS`] (8-bit, no alpha), its colour profile embedded.
//!
//! Rung 4 may swap mozjpeg in behind the same `Encoder` seam.

use crate::encoder::{Caps, Encoder};
use crate::types::{Channels, ExportImage, OutputColor, Samples};
use crate::EncodeError;

/// The JPEG export format at `quality` (1–100).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegEncoder {
    pub quality: u8,
}

impl JpegEncoder {
    /// 8-bit, no alpha — `prepare` flattens and narrows to match.
    pub const CAPS: Caps = Caps {
        alpha: false,
        sixteen_bit: false,
    };
}

impl Encoder for JpegEncoder {
    fn caps(&self) -> Caps {
        Self::CAPS
    }
    fn extension(&self) -> &'static str {
        "jpg"
    }
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError> {
        encode_jpeg(img, self.quality)
    }
}

/// Encode a prepared 8-bit gray or RGB image as JPEG, embedding its profile (an `sRGB` tag
/// embeds nothing — untagged JPEG is read as sRGB). Alpha or 16-bit samples are refused:
/// `prepare` with [`JpegEncoder::CAPS`] never produces them.
pub fn encode_jpeg(img: &ExportImage, quality: u8) -> Result<Vec<u8>, EncodeError> {
    use image::ImageEncoder as _;
    let color_type = match img.channels {
        Channels::Gray => image::ExtendedColorType::L8,
        Channels::Rgb => image::ExtendedColorType::Rgb8,
        Channels::GrayAlpha | Channels::Rgba => return Err(EncodeError::Malformed),
    };
    let Samples::U8(px) = &img.samples else {
        return Err(EncodeError::Malformed);
    };
    let expected = (img.width as usize)
        .checked_mul(img.height as usize)
        .and_then(|n| n.checked_mul(img.channels.count()))
        .ok_or(EncodeError::Malformed)?;
    if expected == 0 || px.len() != expected {
        return Err(EncodeError::Malformed);
    }
    let mut out = Vec::new();
    let mut enc =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100));
    if let OutputColor::Icc(p) = &img.color {
        enc.set_icc_profile(p.clone())
            .map_err(|e| EncodeError::Codec(format!("jpeg profile: {e}")))?;
    }
    enc.write_image(px, img.width, img.height, color_type)
        .map_err(|e| EncodeError::Codec(format!("jpeg encode failed: {e}")))?;
    Ok(out)
}

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

    fn image(channels: Channels, samples: Samples, color: OutputColor) -> ExportImage {
        ExportImage {
            width: 2,
            height: 2,
            channels,
            samples,
            color,
            notes: Vec::new(),
        }
    }

    /// The export encoder embeds the profile byte-for-byte and the file decodes back.
    #[test]
    fn export_jpeg_embeds_the_profile_and_round_trips() {
        use image::ImageDecoder as _;
        let p3 = crate::icc::display_p3_profile();
        let img = image(
            Channels::Rgb,
            Samples::U8([200, 30, 40].repeat(4)),
            OutputColor::Icc(p3.clone()),
        );
        let jpeg = JpegEncoder { quality: 90 }.encode(&img).unwrap();
        let mut dec = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&jpeg)).unwrap();
        assert_eq!(dec.icc_profile().unwrap().as_deref(), Some(p3.as_slice()));
        assert_eq!(dec.dimensions(), (2, 2));
        // sRGB embeds nothing; gray encodes as gray.
        let srgb = image(Channels::Gray, Samples::U8(vec![7; 4]), OutputColor::Srgb);
        let jpeg = encode_jpeg(&srgb, 90).unwrap();
        let mut dec = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&jpeg)).unwrap();
        assert_eq!(dec.icc_profile().unwrap(), None);
        assert_eq!(dec.color_type(), image::ColorType::L8);
    }

    /// Alpha and 16-bit never reach the codec — `prepare` with `CAPS` removes them first.
    #[test]
    fn export_jpeg_refuses_what_its_caps_exclude() {
        let rgba = image(Channels::Rgba, Samples::U8(vec![0; 16]), OutputColor::Srgb);
        assert_eq!(encode_jpeg(&rgba, 90), Err(EncodeError::Malformed));
        let deep = image(Channels::Rgb, Samples::U16(vec![0; 12]), OutputColor::Srgb);
        assert_eq!(encode_jpeg(&deep, 90), Err(EncodeError::Malformed));
        let short = image(Channels::Rgb, Samples::U8(vec![0; 5]), OutputColor::Srgb);
        assert_eq!(encode_jpeg(&short, 90), Err(EncodeError::Malformed));
        assert_eq!(JpegEncoder { quality: 1 }.caps(), JpegEncoder::CAPS);
        assert_eq!(JpegEncoder { quality: 1 }.extension(), "jpg");
    }

    #[test]
    fn rejects_a_size_mismatch() {
        assert!(encode_jpeg_rgba8(&[0; 3], 4, 4, 85).is_err());
        assert!(encode_jpeg_rgba8(&[], 0, 0, 85).is_err());
    }
}
