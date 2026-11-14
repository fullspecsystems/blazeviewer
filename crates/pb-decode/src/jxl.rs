//! JPEG XL backend: `jxl-oxide` (pure Rust, no system deps).
//!
//! Renders the first keyframe to interleaved f32 channels, then quantizes to
//! RGBA8. Animation is ignored (first frame only) — this is a still viewer.

use std::io::Cursor;

use jxl_oxide::{JxlImage, PixelFormat};

use crate::{common, ColorTransform, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

/// JXL signature box that opens a container-format `.jxl` file.
const JXL_BOX_SIG: [u8; 12] = [
    0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
];

#[derive(Debug, Clone, Copy, Default)]
pub struct JxlDecoder;

impl ImageDecoder for JxlDecoder {
    fn can_decode(&self, bytes: &[u8]) -> bool {
        // Bare codestream (FF 0A) or the ISOBMFF container signature box.
        bytes.starts_with(&[0xFF, 0x0A]) || bytes.starts_with(&JXL_BOX_SIG)
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let image = JxlImage::read_with_defaults(Cursor::new(req.bytes))
            .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
        let (w, h) = (image.width(), image.height());
        let fmt = image.pixel_format();
        let render = image
            .render_frame(0)
            .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
        let fb = render.image_all_channels();
        let ch = fb.channels();
        let n = (w as usize) * (h as usize);
        if ch < fmt.channels() || fb.buf().len() < n * ch {
            return Err(DecodeError::Corrupt("jxl framebuffer too small".into()));
        }
        let rgba = to_rgba8(fb.buf(), n, ch, fmt)?;
        // jxl-oxide renders in the image's own color space; `rendered_icc` is the
        // profile describing exactly the buffer we just quantized. Feed it to the
        // in-shader color path (sRGB images resolve to a no-op passthrough).
        let icc = image.rendered_icc();
        let mut img = common::finalize(rgba, w, h, None, "JXL", req.fit, false)?;
        img.color = ColorTransform::from_icc(&icc);
        Ok(img)
    }

    fn name(&self) -> &'static str {
        "jxl-oxide"
    }
}

/// The JPEG XL **export** decode (rung 0b): SDR at the image's own bit depth (8 or 16) with
/// the rendered ICC — the display decode quantizes everything to 8-bit. HDR JPEG XL is
/// refused until rung 5 adds its linearization (review C18).
pub(crate) fn export_jxl(
    bytes: &[u8],
) -> Result<pb_encode::ExportSource, crate::export::ExportDecodeError> {
    use pb_encode::{AlphaMode, Channels, ExportSource, Provenance, SampleEncoding, Samples};
    let image = JxlImage::read_with_defaults(Cursor::new(bytes))
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    if image.hdr_type().is_some() {
        return Err(crate::export::ExportDecodeError::Refused(
            "HDR JPEG XL images can't be exported yet",
        ));
    }
    let (w, h) = (image.width(), image.height());
    let fmt = image.pixel_format();
    let (channels, take) = match fmt {
        PixelFormat::Gray => (Channels::Gray, 1),
        PixelFormat::Graya => (Channels::GrayAlpha, 2),
        PixelFormat::Rgb => (Channels::Rgb, 3),
        PixelFormat::Rgba => (Channels::Rgba, 4),
        PixelFormat::Cmyk | PixelFormat::Cmyka => {
            return Err(crate::export::ExportDecodeError::Refused(
                "CMYK images can't be exported yet",
            ))
        }
    };
    let bits = image.image_header().metadata.bit_depth.bits_per_sample();
    let render = image
        .render_frame(0)
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    let fb = render.image_all_channels();
    let ch = fb.channels();
    let n = (w as usize) * (h as usize);
    if ch < take || fb.buf().len() < n * ch {
        return Err(DecodeError::Corrupt("jxl framebuffer too small".into()).into());
    }
    let buf = fb.buf();
    let samples = if bits <= 8 {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        Samples::U8(
            (0..n)
                .flat_map(|i| (0..take).map(move |c| q(buf[i * ch + c])))
                .collect(),
        )
    } else {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 65535.0).round() as u16;
        Samples::U16(
            (0..n)
                .flat_map(|i| (0..take).map(move |c| q(buf[i * ch + c])))
                .collect(),
        )
    };
    Ok(ExportSource {
        width: w,
        height: h,
        channels,
        samples,
        encoding: SampleEncoding::Encoded,
        alpha: AlphaMode::Straight,
        color: crate::export::icc_desc(Some(image.rendered_icc())),
        provenance: Provenance {
            codec: "JXL",
            path: "jxl-oxide",
            recovered: false,
            raw_preview: false,
        },
    })
}

/// Quantize interleaved f32 channels (stride `ch`, the framebuffer's channel
/// count) to RGBA8, mapping by the image's `PixelFormat`. Color channels come
/// first, then alpha, then any extra channels (ignored).
fn to_rgba8(buf: &[f32], n: usize, ch: usize, fmt: PixelFormat) -> Result<Vec<u8>, DecodeError> {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut out = vec![0u8; n * 4];
    for i in 0..n {
        let s = &buf[i * ch..i * ch + ch];
        let px = match fmt {
            PixelFormat::Gray => {
                let g = q(s[0]);
                [g, g, g, 255]
            }
            PixelFormat::Graya => {
                let g = q(s[0]);
                [g, g, g, q(s[1])]
            }
            PixelFormat::Rgb => [q(s[0]), q(s[1]), q(s[2]), 255],
            PixelFormat::Rgba => [q(s[0]), q(s[1]), q(s[2]), q(s[3])],
            PixelFormat::Cmyk | PixelFormat::Cmyka => {
                return Err(DecodeError::Corrupt("JXL CMYK unsupported".into()));
            }
        };
        out[i * 4..i * 4 + 4].copy_from_slice(&px);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_jxl_signatures() {
        let d = JxlDecoder;
        assert!(d.can_decode(&[0xFF, 0x0A, 0x00])); // bare codestream
        assert!(d.can_decode(&JXL_BOX_SIG)); // container
        assert!(!d.can_decode(&[0xFF, 0xD8, 0xFF])); // JPEG
        assert!(!d.can_decode(&[0x89, b'P', b'N', b'G']));
    }
}
