//! The first real decode backend: `zune-jpeg` (pure Rust, no system deps).
//!
//! Decodes to RGBA8 and applies EXIF orientation so photos display upright.
//! `req.fit` is ignored — zune-jpeg has no scaled-decode path, so we decode at
//! full resolution and let the GPU letterbox. (turbojpeg, which *does* scale on
//! decode, slots in behind the same `ImageDecoder` seam later.)

use std::io::Cursor;
use std::path::Path;

use zune_jpeg::JpegDecoder;

use crate::orientation::apply_orientation;
use crate::{DecodeError, DecodeRequest, DecodedImage, FitBox, ImageDecoder, PixelFormat};

/// JPEG decoder backed by `zune-jpeg`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZuneJpegDecoder;

impl ImageDecoder for ZuneJpegDecoder {
    fn can_decode(&self, bytes: &[u8]) -> bool {
        // JPEG SOI marker.
        bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let bytes = req.bytes;
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
            other => {
                return Err(DecodeError::Corrupt(format!("unexpected {other} channels")));
            }
        };

        let (pixels, width, height) = match read_orientation(bytes) {
            o if o > 1 => apply_orientation(&rgba, w, h, o),
            _ => (rgba, w, h),
        };
        // The photo's true resolution (oriented, before any decode-to-fit).
        let (orig_width, orig_height) = (width, height);

        // Decode-to-fit: downscale to the display size with a high-quality filter
        // so large photos aren't minified (aliased/grainy) on the GPU.
        let (pixels, width, height) = match req.fit {
            Some(fit) => downscale_to_fit(pixels, width, height, fit)?,
            None => (pixels, width, height),
        };

        Ok(DecodedImage {
            width,
            height,
            orig_width,
            orig_height,
            codec: "JPEG",
            format: PixelFormat::Rgba8,
            pixels,
            is_preview: false,
        })
    }

    fn name(&self) -> &'static str {
        "zune-jpeg"
    }
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

/// Read the EXIF Orientation tag (1..=8); defaults to 1 (upright) if absent.
fn read_orientation(bytes: &[u8]) -> u32 {
    let mut cursor = Cursor::new(bytes);
    if let Ok(exif) = exif::Reader::new().read_from_container(&mut cursor) {
        if let Some(field) = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY) {
            if let Some(v) = field.value.get_uint(0) {
                return v;
            }
        }
    }
    1
}

/// Downscale an RGBA8 image to fit within `fit` (preserving aspect, never
/// upscaling) using a high-quality Lanczos3 filter. Returns the input unchanged
/// when it already fits.
fn downscale_to_fit(
    pixels: Vec<u8>,
    w: u32,
    h: u32,
    fit: FitBox,
) -> Result<(Vec<u8>, u32, u32), DecodeError> {
    let scale = (fit.max_width as f64 / w as f64)
        .min(fit.max_height as f64 / h as f64)
        .min(1.0);
    if scale >= 0.999 {
        return Ok((pixels, w, h));
    }
    let tw = ((w as f64 * scale).round() as u32).max(1);
    let th = ((h as f64 * scale).round() as u32).max(1);

    use fast_image_resize::images::Image;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    let src = Image::from_vec_u8(w, h, pixels, PixelType::U8x4)
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    let mut dst = Image::new(tw, th, PixelType::U8x4);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    Resizer::new()
        .resize(&src, &mut dst, &opts)
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    Ok((dst.into_vec(), tw, th))
}

/// Convenience: read a file from disk and decode it to an upright RGBA image,
/// downscaled to fit `fit` (the display size) when provided.
pub fn decode_image_file(path: &Path, fit: Option<FitBox>) -> Result<DecodedImage, DecodeError> {
    let bytes = std::fs::read(path).map_err(|e| DecodeError::Corrupt(format!("read error: {e}")))?;
    let req = DecodeRequest {
        bytes: &bytes,
        fit,
        allow_preview: false,
    };
    ZuneJpegDecoder.decode(&req)
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

    #[test]
    fn downscale_to_fit_leaves_fitting_images_untouched() {
        let pixels = vec![1u8; 8 * 8 * 4];
        let (out, w, h) =
            downscale_to_fit(pixels, 8, 8, FitBox { max_width: 100, max_height: 100 }).unwrap();
        assert_eq!((w, h), (8, 8));
        assert_eq!(out.len(), 8 * 8 * 4);
    }

    #[test]
    fn downscale_to_fit_resizes_and_preserves_solid_color() {
        let (w, h) = (100u32, 100u32);
        let red = [200u8, 30, 40, 255];
        let pixels: Vec<u8> = red
            .iter()
            .copied()
            .cycle()
            .take((w * h * 4) as usize)
            .collect();
        let (out, ow, oh) =
            downscale_to_fit(pixels, w, h, FitBox { max_width: 50, max_height: 50 }).unwrap();
        assert_eq!((ow, oh), (50, 50));
        assert_eq!(out.len(), (50 * 50 * 4) as usize);
        // A solid color survives the resize (interior is exact; allow tiny tol).
        let c = ((25 * 50 + 25) * 4) as usize;
        for k in 0..4 {
            assert!(
                (out[c + k] as i32 - red[k] as i32).abs() <= 2,
                "channel {k}: {} vs {}",
                out[c + k],
                red[k]
            );
        }
    }
}
