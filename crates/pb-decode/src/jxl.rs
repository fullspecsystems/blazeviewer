//! JPEG XL backend: `jxl-oxide` (pure Rust, no system deps).
//!
//! Renders the first keyframe to interleaved f32 channels, then quantizes to
//! RGBA8. Animation is ignored (first frame only) — this is a still viewer.

use std::io::Cursor;

use jxl_oxide::{JxlImage, PixelFormat};

use crate::{common, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

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
        // jxl-oxide renders in the image's own color space (sRGB-encoded f32).
        common::finalize(rgba, w, h, None, "JXL", req.fit, false)
    }

    fn name(&self) -> &'static str {
        "jxl-oxide"
    }
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
