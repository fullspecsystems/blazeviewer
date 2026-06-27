//! The bulk pure-Rust backend: the `image` crate (image-rs).
//!
//! One dependency covers PNG, GIF, BMP, ICO, TIFF, WebP (lossy + lossless), TGA,
//! QOI, PNM, HDR, OpenEXR and farbfeld — all pure Rust, zero system libraries, so
//! it builds anywhere with no vcpkg/DLL risk. JPEG is intentionally left to the
//! tuned `zune` backend, and AVIF/HEIC/JXL to their own backends, so this one
//! only claims the formats it actually decodes well.

use std::io::Cursor;

use image::{DynamicImage, ImageFormat};

use crate::{
    common, ColorTransform, DecodeError, DecodeRequest, DecodedImage, FitBox, ImageDecoder,
};

/// Decoder over the `image` crate's pure-Rust formats.
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageCrateDecoder;

/// Formats we route here. JPEG/AVIF (handled elsewhere or C-gated) are excluded
/// even though `image` can sniff them, so dispatch stays unambiguous.
fn supported(fmt: ImageFormat) -> bool {
    matches!(
        fmt,
        ImageFormat::Png
            | ImageFormat::Gif
            | ImageFormat::Bmp
            | ImageFormat::Ico
            | ImageFormat::Tiff
            | ImageFormat::WebP
            | ImageFormat::Tga
            | ImageFormat::Qoi
            | ImageFormat::Pnm
            | ImageFormat::Hdr
            | ImageFormat::OpenExr
            | ImageFormat::Farbfeld
            | ImageFormat::Dds
    )
}

/// A stable, content-derived codec label for the info panel.
fn codec_name(fmt: ImageFormat) -> &'static str {
    match fmt {
        ImageFormat::Png => "PNG",
        ImageFormat::Gif => "GIF",
        ImageFormat::Bmp => "BMP",
        ImageFormat::Ico => "ICO",
        ImageFormat::Tiff => "TIFF",
        ImageFormat::WebP => "WebP",
        ImageFormat::Tga => "TGA",
        ImageFormat::Qoi => "QOI",
        ImageFormat::Pnm => "PNM",
        ImageFormat::Hdr => "HDR",
        ImageFormat::OpenExr => "EXR",
        ImageFormat::Farbfeld => "farbfeld",
        ImageFormat::Dds => "DDS",
        _ => "image",
    }
}

impl ImageDecoder for ImageCrateDecoder {
    fn can_decode(&self, bytes: &[u8]) -> bool {
        image::guess_format(bytes).map(supported).unwrap_or(false)
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let fmt = image::guess_format(req.bytes).map_err(|_| DecodeError::Unsupported)?;
        decode_format(req.bytes, fmt, req.fit)
    }

    fn name(&self) -> &'static str {
        "image"
    }
}

/// Decode `bytes` as a specific `image`-crate format. Used both by the content-
/// sniff path (with a guessed format) and by the extension-routed TGA path —
/// Targa has no magic number, so `guess_format` can't find it.
pub(crate) fn decode_format(
    bytes: &[u8],
    fmt: ImageFormat,
    fit: Option<FitBox>,
) -> Result<DecodedImage, DecodeError> {
    if !supported(fmt) {
        return Err(DecodeError::Unsupported);
    }
    let (img, icc) = load_with_icc(bytes, fmt)?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    // Only TIFF and WebP carry EXIF orientation among these; for the rest, skip
    // the parse (read_orientation would just return 1).
    let exif = matches!(fmt, ImageFormat::Tiff | ImageFormat::WebP).then_some(bytes);
    let mut decoded = common::finalize(rgba.into_raw(), w, h, exif, codec_name(fmt), fit, false)?;
    // Wide-gamut PNG/TIFF/WebP carry an ICC profile → in-shader color management.
    if let Some(icc) = icc {
        decoded.color = ColorTransform::from_icc(&icc);
    }
    Ok(decoded)
}

/// Decode to a [`DynamicImage`], also returning the embedded ICC profile for the
/// formats that carry one (PNG `iCCP`, TIFF ICC tag, WebP `ICCP`). The plain
/// [`image::load_from_memory_with_format`] path drops the profile, so those three
/// go through their concrete decoders (which expose `icc_profile`) first.
fn load_with_icc(
    bytes: &[u8],
    fmt: ImageFormat,
) -> Result<(DynamicImage, Option<Vec<u8>>), DecodeError> {
    use image::ImageDecoder as _;
    let corrupt = |e: image::ImageError| DecodeError::Corrupt(e.to_string());
    match fmt {
        ImageFormat::Png => {
            let mut d = image::codecs::png::PngDecoder::new(Cursor::new(bytes)).map_err(corrupt)?;
            let icc = d.icc_profile().ok().flatten();
            Ok((DynamicImage::from_decoder(d).map_err(corrupt)?, icc))
        }
        ImageFormat::Tiff => {
            let mut d =
                image::codecs::tiff::TiffDecoder::new(Cursor::new(bytes)).map_err(corrupt)?;
            let icc = d.icc_profile().ok().flatten();
            Ok((DynamicImage::from_decoder(d).map_err(corrupt)?, icc))
        }
        ImageFormat::WebP => {
            let mut d =
                image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(corrupt)?;
            let icc = d.icc_profile().ok().flatten();
            Ok((DynamicImage::from_decoder(d).map_err(corrupt)?, icc))
        }
        _ => {
            let img = image::load_from_memory_with_format(bytes, fmt).map_err(corrupt)?;
            Ok((img, None))
        }
    }
}

/// Decode a Targa file (routed by extension, since TGA carries no magic number).
pub(crate) fn decode_tga(bytes: &[u8], fit: Option<FitBox>) -> Result<DecodedImage, DecodeError> {
    decode_format(bytes, ImageFormat::Tga, fit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_png_magic() {
        let d = ImageCrateDecoder;
        let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert!(d.can_decode(&png));
        assert!(!d.can_decode(&[0xFF, 0xD8, 0xFF])); // JPEG → not ours
        assert!(!d.can_decode(&[0x00, 0x01, 0x02]));
    }

    #[test]
    fn jpeg_is_not_claimed_here() {
        // guess_format recognizes JPEG, but we leave it to the zune backend.
        assert!(!supported(ImageFormat::Jpeg));
        assert!(supported(ImageFormat::Qoi));
        assert!(supported(ImageFormat::WebP));
    }
}
