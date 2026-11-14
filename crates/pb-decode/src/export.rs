//! The **export decode** (export-plan.md rung 0b): full resolution, full precision, and the
//! samples' true colour description — decode-to-purpose for writing a file, never for
//! display. The display decode (`decode_named_bytes`) is untouched and pays nothing.
//!
//! Routing mirrors [`decode_named_bytes`](crate::decode_named_bytes): RAW / SVG / TGA by
//! extension, everything else by content sniff, then the same recovery ladder (task #127).
//! What differs per backend (§ 3.10):
//!
//! - JPEG (zune): 8-bit, gray stays gray, the embedded ICC verbatim; **CMYK is refused**.
//! - The `image` crate: 16-bit sources stay 16-bit, gray stays gray, ICC verbatim; EXR /
//!   Radiance come back as linear float (HDR in the app convention).
//! - JPEG XL: SDR at the image's own bit depth with its rendered ICC; HDR waits for rung 5.
//! - macOS Image I/O (HEIC/AVIF, and the recovery rung): its **own** draw in the image's
//!   colour space at 16 bpc when the source has more than 8 bits (§ 3.8 Rule 1) — never the
//!   display path's 8-bit Display-P3 draw. HDR comes back as linear float.
//! - Windows WIC / libheif: the display decode at full size plus the container's real `colr`
//!   (ICC bytes or nclx code points).
//! - RAW / SVG / PSD: the display decode at full size (sRGB, or P3 for PSD's Image I/O
//!   fallback).
//!
//! Every path is panic-safe, like the display decode.

use std::path::Path;
use std::sync::Arc;

use pb_encode::{
    AlphaMode, Channels, ColorDesc, ExportSource, Provenance, SampleEncoding, Samples,
};

use crate::{DecodeError, DecodeRequest, DecodedImage, ImageDecoder, PixelFormat};

/// Why an export decode produced nothing. `Refused` carries a user-facing reason for a
/// source the export can't handle *yet*; `Decode` is an ordinary decode failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportDecodeError {
    Refused(&'static str),
    Decode(DecodeError),
}

impl std::fmt::Display for ExportDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportDecodeError::Refused(why) => write!(f, "{why}"),
            ExportDecodeError::Decode(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ExportDecodeError {}

impl From<DecodeError> for ExportDecodeError {
    fn from(e: DecodeError) -> Self {
        ExportDecodeError::Decode(e)
    }
}

/// Decode `bytes` (named `name`, for extension routing) for export. Panic-safe.
pub fn decode_named_bytes_for_export(
    name: &str,
    bytes: &[u8],
) -> Result<ExportSource, ExportDecodeError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        export_named_inner(name, bytes)
    })) {
        Ok(r) => r,
        Err(_) => Err(ExportDecodeError::Decode(DecodeError::Corrupt(
            "decoder panicked".into(),
        ))),
    }
}

fn full_res(bytes: &[u8]) -> DecodeRequest<'_> {
    DecodeRequest {
        bytes,
        fit: None,
        allow_preview: false,
    }
}

fn export_named_inner(name: &str, bytes: &[u8]) -> Result<ExportSource, ExportDecodeError> {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if crate::raw::is_raw_extension(&ext) {
        let img = crate::RawPreviewDecoder.decode(&full_res(bytes))?;
        return Ok(from_decoded(img, "raw"));
    }
    if matches!(ext.as_str(), "svg" | "svgz") {
        let img = crate::SvgDecoder.decode(&full_res(bytes))?;
        return Ok(from_decoded(img, "resvg"));
    }
    if ext == "tga" {
        return crate::image_backend::export_format(bytes, image::ImageFormat::Tga);
    }
    export_bytes_inner(bytes)
}

fn export_bytes_inner(bytes: &[u8]) -> Result<ExportSource, ExportDecodeError> {
    #[cfg(heic_libheif)]
    if crate::libheif::route_full_heic(bytes, false) {
        let img = crate::libheif::LibHeifDecoder.decode(&full_res(bytes))?;
        return Ok(with_colr(from_decoded(img, "libheif"), bytes));
    }
    let primary: Result<ExportSource, ExportDecodeError> =
        if crate::ZuneJpegDecoder.can_decode(bytes) {
            crate::zune::export_jpeg(bytes)
        } else if crate::JxlDecoder.can_decode(bytes) {
            crate::jxl::export_jxl(bytes)
        } else if crate::PsdDecoder.can_decode(bytes) {
            crate::PsdDecoder
                .decode(&full_res(bytes))
                .map(|img| from_decoded(img, "psd"))
                .map_err(Into::into)
        } else if os_codec_claims(bytes) {
            export_os_codec(bytes)
        } else if crate::ImageCrateDecoder.can_decode(bytes) {
            let fmt = image::guess_format(bytes).map_err(|_| DecodeError::Unsupported)?;
            crate::image_backend::export_format(bytes, fmt)
        } else {
            return Err(DecodeError::Unsupported.into());
        };
    match primary {
        Ok(s) => Ok(s),
        // A refusal is a policy answer, not a malformed file — don't "recover" past it.
        Err(e @ ExportDecodeError::Refused(_)) => Err(e),
        Err(ExportDecodeError::Decode(primary_err)) => recover(bytes, primary_err),
    }
}

/// Whether the platform's OS imaging codec claims these bytes (HEIC/AVIF).
fn os_codec_claims(bytes: &[u8]) -> bool {
    #[cfg(windows)]
    if crate::WicDecoder.can_decode(bytes) {
        return true;
    }
    #[cfg(target_os = "macos")]
    if crate::ImageIoDecoder.can_decode(bytes) {
        return true;
    }
    let _ = bytes;
    false
}

/// The OS codec's export decode.
fn export_os_codec(bytes: &[u8]) -> Result<ExportSource, ExportDecodeError> {
    #[cfg(target_os = "macos")]
    {
        crate::imageio::export_imageio(bytes)
    }
    #[cfg(windows)]
    {
        let img = crate::WicDecoder.decode(&full_res(bytes))?;
        Ok(with_colr(from_decoded(img, "wic"), bytes))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = bytes;
        Err(DecodeError::Unsupported.into())
    }
}

/// Recovery ladder rungs 2–3 for export, mirroring the display decode's.
fn recover(bytes: &[u8], primary_err: DecodeError) -> Result<ExportSource, ExportDecodeError> {
    let mark = |mut s: ExportSource| {
        s.provenance.recovered = true;
        s
    };
    #[cfg(target_os = "macos")]
    if let Ok(s) = crate::imageio::export_imageio(bytes) {
        return Ok(mark(s));
    }
    #[cfg(windows)]
    if let Ok(img) = crate::WicDecoder.decode(&full_res(bytes)) {
        return Ok(mark(from_decoded(img, "wic")));
    }
    if let Ok(fmt) = image::guess_format(bytes) {
        if let Ok(s) = crate::image_backend::export_format(bytes, fmt) {
            return Ok(mark(s));
        }
    }
    Err(primary_err.into())
}

/// Replace a display decode's colour with the container's real `colr` box (ICC bytes or
/// nclx code points) — the WIC/libheif paths, whose `DecodedImage` only carries the
/// display reduction.
#[cfg(any(windows, heic_libheif))]
fn with_colr(mut s: ExportSource, bytes: &[u8]) -> ExportSource {
    if s.encoding == SampleEncoding::Encoded {
        if let Some(desc) = crate::isobmff::colr_desc(bytes) {
            s.color = desc;
        }
    }
    s
}

/// Adapt a full-size display decode into an export source. Used only for backends whose
/// display decode is already full precision for their sources (RAW previews, SVG, PSD, the
/// Windows OS codec) — the colour is mapped from the display transform, so only sRGB and
/// the known Display-P3 case are exact; anything else is tagged sRGB with a log line (no
/// such backend exists today — see the module docs).
pub(crate) fn from_decoded(img: DecodedImage, path: &'static str) -> ExportSource {
    let provenance = Provenance {
        codec: img.codec,
        path,
        recovered: img.recovered.is_some(),
        raw_preview: false,
    };
    match img.format {
        PixelFormat::Rgba16F => {
            let f: Vec<f32> = img
                .pixels
                .chunks_exact(2)
                .map(|b| half::f16::from_le_bytes([b[0], b[1]]).to_f32())
                .collect();
            ExportSource {
                width: img.width,
                height: img.height,
                channels: Channels::Rgba,
                samples: Samples::F32(f),
                encoding: SampleEncoding::Linear,
                alpha: AlphaMode::Straight,
                color: ColorDesc::LinearScRgb,
                provenance,
            }
        }
        _ => {
            let color = if !img.color.enabled {
                ColorDesc::Untagged
            } else if img.color == crate::ColorTransform::from_cicp(12, 13, 0, true) {
                ColorDesc::Cicp(pb_encode::Cicp {
                    primaries: 12,
                    transfer: 13,
                    matrix: 0,
                    full_range: true,
                })
            } else {
                eprintln!("export: no exact colour description from the {path} path; tagging sRGB");
                ColorDesc::Untagged
            };
            ExportSource {
                width: img.width,
                height: img.height,
                channels: Channels::Rgba,
                samples: Samples::U8(img.pixels),
                encoding: SampleEncoding::Encoded,
                alpha: AlphaMode::Straight,
                color,
                provenance,
            }
        }
    }
}

/// An ICC profile as a shared colour description (empty → untagged).
pub(crate) fn icc_desc(icc: Option<Vec<u8>>) -> ColorDesc {
    match icc {
        Some(b) if !b.is_empty() => ColorDesc::Icc(Arc::from(b)),
        _ => ColorDesc::Untagged,
    }
}

/// Apply EXIF `orientation` (1..=8) to an interleaved buffer of any sample type, via the
/// shared tested byte remap (`orientation::apply_orientation_bytes`).
pub(crate) fn orient(
    samples: Samples,
    w: u32,
    h: u32,
    channels: Channels,
    orientation: u32,
) -> (Samples, u32, u32) {
    if orientation <= 1 || orientation > 8 {
        return (samples, w, h);
    }
    let ch = channels.count();
    match samples {
        Samples::U8(v) => {
            let (o, ow, oh) =
                crate::orientation::apply_orientation_bytes(&v, w, h, orientation, ch);
            (Samples::U8(o), ow, oh)
        }
        Samples::U16(v) => {
            let bytes: Vec<u8> = v.iter().flat_map(|s| s.to_ne_bytes()).collect();
            let (o, ow, oh) =
                crate::orientation::apply_orientation_bytes(&bytes, w, h, orientation, ch * 2);
            let back = o
                .chunks_exact(2)
                .map(|b| u16::from_ne_bytes([b[0], b[1]]))
                .collect();
            (Samples::U16(back), ow, oh)
        }
        Samples::F32(v) => {
            let bytes: Vec<u8> = v.iter().flat_map(|s| s.to_ne_bytes()).collect();
            let (o, ow, oh) =
                crate::orientation::apply_orientation_bytes(&bytes, w, h, orientation, ch * 4);
            let back = o
                .chunks_exact(4)
                .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            (Samples::F32(back), ow, oh)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG written by the `png` crate directly — 16-bit, RGBA, with an iCCP profile.
    fn png16_with_icc(w: u32, h: u32, px: &[u16], icc: &[u8]) -> Vec<u8> {
        let mut info = png::Info::with_size(w, h);
        info.color_type = png::ColorType::Rgba;
        info.bit_depth = png::BitDepth::Sixteen;
        info.icc_profile = Some(std::borrow::Cow::Borrowed(icc));
        let mut out = Vec::new();
        {
            let enc = png::Encoder::with_info(&mut out, info).unwrap();
            let mut wr = enc.write_header().unwrap();
            let data: Vec<u8> = px.iter().flat_map(|s| s.to_be_bytes()).collect();
            wr.write_image_data(&data).unwrap();
        }
        out
    }

    #[test]
    fn a_16bit_png_keeps_16_bits_and_its_icc_byte_for_byte() {
        let icc = pb_encode::icc::display_p3_profile();
        let px: Vec<u16> = (0..3 * 2 * 4).map(|i| (i as u16) * 2111 + 7).collect();
        let png = png16_with_icc(3, 2, &px, &icc);
        let s = decode_named_bytes_for_export("x.png", &png).unwrap();
        assert_eq!((s.width, s.height, s.channels), (3, 2, Channels::Rgba));
        assert_eq!(
            s.samples,
            Samples::U16(px),
            "precision kept (the display decode narrows)"
        );
        assert_eq!(
            s.color,
            ColorDesc::Icc(Arc::from(icc)),
            "profile bytes identical"
        );
        assert_eq!(s.encoding, SampleEncoding::Encoded);
    }

    #[test]
    fn a_gray_png_stays_gray() {
        let mut info = png::Info::with_size(2, 1);
        info.color_type = png::ColorType::Grayscale;
        info.bit_depth = png::BitDepth::Eight;
        let mut out = Vec::new();
        {
            let mut wr = png::Encoder::with_info(&mut out, info)
                .unwrap()
                .write_header()
                .unwrap();
            wr.write_image_data(&[17, 200]).unwrap();
        }
        let s = decode_named_bytes_for_export("g.png", &out).unwrap();
        assert_eq!(s.channels, Channels::Gray);
        assert_eq!(s.samples, Samples::U8(vec![17, 200]));
        assert_eq!(s.color, ColorDesc::Untagged);
    }

    #[test]
    fn a_jpeg_exports_at_full_size_8bit_untagged() {
        let rgba = vec![90u8; 8 * 6 * 4];
        let jpeg = pb_encode::encode_jpeg_rgba8(&rgba, 8, 6, 92).unwrap();
        let s = decode_named_bytes_for_export("a.jpg", &jpeg).unwrap();
        assert_eq!((s.width, s.height), (8, 6));
        assert_eq!(s.channels, Channels::Rgb);
        assert!(matches!(s.samples, Samples::U8(_)));
        assert_eq!(s.color, ColorDesc::Untagged);
        assert!(s.is_well_formed());
    }

    #[test]
    fn an_svg_exports_at_its_intrinsic_size() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="40" height="20" fill="red"/></svg>"#;
        let s = decode_named_bytes_for_export("v.svg", svg).unwrap();
        assert_eq!((s.width, s.height), (40, 20));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(decode_named_bytes_for_export("x.bin", &[1, 2, 3, 4]).is_err());
        assert!(decode_named_bytes_for_export("x.png", &[0x89, b'P', b'N', b'G', 0, 0]).is_err());
    }

    #[test]
    fn orient_rotates_any_sample_type() {
        // 2x1 [A, B] with orientation 6 (rotate 90 CW) → 1x2 [A; B].
        let (s, w, h) = orient(Samples::U16(vec![1, 2, 3, 4]), 2, 1, Channels::GrayAlpha, 6);
        assert_eq!((w, h), (1, 2));
        assert_eq!(s, Samples::U16(vec![1, 2, 3, 4]));
        let (s, w, h) = orient(Samples::F32(vec![1.0, 2.0]), 2, 1, Channels::Gray, 3);
        assert_eq!((w, h), (2, 1));
        assert_eq!(s, Samples::F32(vec![2.0, 1.0]));
    }
}
