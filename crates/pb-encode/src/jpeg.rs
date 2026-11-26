//! JPEG. Two callers:
//! - [`encode_jpeg_rgba8`] — AI Describe's downscaled, already-sRGB upload (moved here from
//!   `pb-decode` in rung 0c). Always the `image` crate: its size doesn't matter, and it needs no
//!   native build;
//! - [`JpegEncoder`] — the export format (rung 2): an `ExportImage` prepared with
//!   [`JpegEncoder::CAPS`] (8-bit, no alpha), its colour profile and EXIF embedded.
//!
//! Export encodes with **jpegli** when the `jpegli` feature is on (the shells' `export-codecs`
//! builds, task #157): progressive, 4:4:4, adaptive quantization, 8–10 % smaller than the
//! `image` crate's baseline encoder at equal SSIMULACRA2 across q70–q90 and faster
//! (export-plan.md rung 4). Without it (Linux, a plain build) export uses the baseline encoder,
//! which never subsamples chroma either. mozjpeg was measured too and isn't worth adding.

use crate::encoder::{Caps, Encoder};
use crate::types::{Channels, ExportImage, OutputColor, Samples};
use crate::EncodeError;

/// Whether JPEG export goes through jpegli in this build. Qualities aren't interchangeable
/// between the two encoders, so the default quality follows this.
pub const JPEGLI: bool = cfg!(feature = "jpegli");

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
        gray: true,
    };
}

impl Encoder for JpegEncoder {
    fn caps(&self) -> Caps {
        Self::CAPS
    }
    fn extension(&self) -> &'static str {
        "jpg"
    }
    fn lossless(&self) -> bool {
        false
    }
    fn writes_metadata(&self) -> bool {
        true
    }
    fn encode(&self, img: &ExportImage) -> Result<Vec<u8>, EncodeError> {
        encode_jpeg(img, self.quality)
    }
}

/// Encode a prepared 8-bit gray or RGB image as JPEG with this build's export encoder (jpegli
/// when [`JPEGLI`], else [`encode_jpeg_baseline`]), embedding its profile (an `sRGB` tag embeds
/// nothing — untagged JPEG is read as sRGB) and its EXIF block as an `APP1` `Exif` segment.
/// Alpha or 16-bit samples are refused: `prepare` with [`JpegEncoder::CAPS`] never produces them.
pub fn encode_jpeg(img: &ExportImage, quality: u8) -> Result<Vec<u8>, EncodeError> {
    #[cfg(feature = "jpegli")]
    {
        encode_jpegli(img, quality)
    }
    #[cfg(not(feature = "jpegli"))]
    {
        encode_jpeg_baseline(img, quality)
    }
}

/// The 8-bit samples of a prepared gray or RGB image, checked against its dimensions.
fn checked_pixels(img: &ExportImage) -> Result<&[u8], EncodeError> {
    if matches!(img.channels, Channels::GrayAlpha | Channels::Rgba) {
        return Err(EncodeError::Malformed);
    }
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
    Ok(px)
}

/// [`encode_jpeg`] through jpegli (`pb-jpegli`).
#[cfg(feature = "jpegli")]
fn encode_jpegli(img: &ExportImage, quality: u8) -> Result<Vec<u8>, EncodeError> {
    let pixels = checked_pixels(img)?;
    let icc = match &img.color {
        OutputColor::Icc(p) => Some(p.as_slice()),
        OutputColor::Srgb => None,
    };
    pb_jpegli::encode(
        pb_jpegli::Image {
            width: img.width,
            height: img.height,
            components: img.channels.count() as u32,
            pixels,
        },
        pb_jpegli::Options {
            quality,
            exif: img.metadata.exif.as_deref(),
            icc,
        },
    )
    .map_err(|e| EncodeError::Codec(e.to_string()))
}

/// [`encode_jpeg`] through the `image` crate's baseline encoder (4:4:4, sequential). Export's
/// encoder in builds without jpegli, and `codec_bench`'s comparison point in builds with it.
pub fn encode_jpeg_baseline(img: &ExportImage, quality: u8) -> Result<Vec<u8>, EncodeError> {
    use image::ImageEncoder as _;
    let px = checked_pixels(img)?;
    let color_type = match img.channels {
        Channels::Gray => image::ExtendedColorType::L8,
        _ => image::ExtendedColorType::Rgb8,
    };
    let mut out = Vec::new();
    let mut enc =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100));
    if let OutputColor::Icc(p) = &img.color {
        enc.set_icc_profile(p.clone())
            .map_err(|e| EncodeError::Codec(format!("jpeg profile: {e}")))?;
    }
    if let Some(exif) = &img.metadata.exif {
        enc.set_exif_metadata(exif.clone())
            .map_err(|e| EncodeError::Codec(format!("jpeg metadata: {e}")))?;
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

    type Encode = fn(&ExportImage, u8) -> Result<Vec<u8>, EncodeError>;

    /// The export encoders this build has: the baseline always, jpegli with the feature.
    fn encoders() -> Vec<(&'static str, Encode)> {
        let mut v: Vec<(&'static str, Encode)> = vec![("baseline", encode_jpeg_baseline)];
        if JPEGLI {
            v.push(("jpegli", encode_jpeg));
        }
        v
    }

    fn image(channels: Channels, samples: Samples, color: OutputColor) -> ExportImage {
        ExportImage {
            width: 2,
            height: 2,
            channels,
            samples,
            color,
            notes: Vec::new(),
            metadata: Default::default(),
        }
    }

    /// Required test 2 for JPEG: the EXIF goes in `APP1` beside the profile and reads back
    /// clean; without metadata there is no `APP1` at all. Both encoders.
    #[test]
    fn metadata_round_trips() {
        use crate::metadata::tests::{assert_clean, dirty_source};
        use crate::metadata::MetadataChoice;
        use image::ImageDecoder as _;
        let p3 = crate::icc::display_p3_profile();
        for (name, encode) in encoders() {
            let mut img = image(
                Channels::Rgb,
                Samples::U8([200, 30, 40].repeat(4)),
                OutputColor::Icc(p3.clone()),
            );
            img.metadata = dirty_source().output(MetadataChoice::REVEALED);
            let jpeg = encode(&img, 90).unwrap();
            let exif = exif::Reader::new()
                .read_from_container(&mut std::io::Cursor::new(&jpeg))
                .unwrap_or_else(|e| panic!("{name}: no APP1 Exif segment: {e}"));
            assert_clean(exif.buf(), false);
            let mut dec =
                image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&jpeg)).unwrap();
            assert_eq!(
                dec.icc_profile().unwrap().as_deref(),
                Some(p3.as_slice()),
                "{name}"
            );

            img.metadata = Default::default();
            let plain = encode(&img, 90).unwrap();
            assert!(!plain.windows(6).any(|w| w == b"Exif\0\0"), "{name}");
        }
    }

    /// The export encoders embed the profile byte-for-byte and the file decodes back.
    #[test]
    fn export_jpeg_embeds_the_profile_and_round_trips() {
        use image::ImageDecoder as _;
        let p3 = crate::icc::display_p3_profile();
        for (name, encode) in encoders() {
            let img = image(
                Channels::Rgb,
                Samples::U8([200, 30, 40].repeat(4)),
                OutputColor::Icc(p3.clone()),
            );
            let jpeg = encode(&img, 90).unwrap();
            let mut dec =
                image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&jpeg)).unwrap();
            assert_eq!(
                dec.icc_profile().unwrap().as_deref(),
                Some(p3.as_slice()),
                "{name}"
            );
            assert_eq!(dec.dimensions(), (2, 2), "{name}");
            // sRGB embeds nothing; gray encodes as gray.
            let srgb = image(Channels::Gray, Samples::U8(vec![7; 4]), OutputColor::Srgb);
            let jpeg = encode(&srgb, 90).unwrap();
            let mut dec =
                image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&jpeg)).unwrap();
            assert_eq!(dec.icc_profile().unwrap(), None, "{name}");
            assert_eq!(dec.color_type(), image::ColorType::L8, "{name}");
        }
        let jpeg = JpegEncoder { quality: 90 }
            .encode(&image(
                Channels::Gray,
                Samples::U8(vec![7; 4]),
                OutputColor::Srgb,
            ))
            .unwrap();
        // jpegli writes progressive (SOF2); the baseline encoder sequential (SOF0).
        let sof2 = jpeg.windows(2).any(|w| w == [0xFF, 0xC2]);
        assert_eq!(
            sof2, JPEGLI,
            "the export encoder is jpegli exactly when it's built"
        );
    }

    /// Alpha and 16-bit never reach the codec — `prepare` with `CAPS` removes them first.
    #[test]
    fn export_jpeg_refuses_what_its_caps_exclude() {
        for (name, encode) in encoders() {
            let rgba = image(Channels::Rgba, Samples::U8(vec![0; 16]), OutputColor::Srgb);
            assert_eq!(encode(&rgba, 90), Err(EncodeError::Malformed), "{name}");
            let deep = image(Channels::Rgb, Samples::U16(vec![0; 12]), OutputColor::Srgb);
            assert_eq!(encode(&deep, 90), Err(EncodeError::Malformed), "{name}");
            let short = image(Channels::Rgb, Samples::U8(vec![0; 5]), OutputColor::Srgb);
            assert_eq!(encode(&short, 90), Err(EncodeError::Malformed), "{name}");
        }
        assert_eq!(JpegEncoder { quality: 1 }.caps(), JpegEncoder::CAPS);
        assert_eq!(JpegEncoder { quality: 1 }.extension(), "jpg");
    }

    #[test]
    fn rejects_a_size_mismatch() {
        assert!(encode_jpeg_rgba8(&[0; 3], 4, 4, 85).is_err());
        assert!(encode_jpeg_rgba8(&[], 0, 0, 85).is_err());
    }
}
