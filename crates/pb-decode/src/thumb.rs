//! Thumbnail derivation (task #83): the T0 `derive_thumbnail` contract — turn a
//! decoded viewer image (any pixel format the viewer produces) into a small
//! **display-ready sRGB8** thumb — and the T1 `exif_thumbnail` fast path (the
//! EXIF IFD1 embedded JPEG: header-parse only, no full decode).
//!
//! `derive_thumbnail` runs on the dedicated derive thread (never the event
//! loop) and **consumes** its input — the post-upload buffer handoff gives it
//! ownership, so no full-frame clone ever happens. Because SwiftUI and egui
//! both show plain sRGB pixels, everything the renderer would do in-shader is
//! baked here: the source TRC + 3×3 gamut matrix for tagged SDR images, and
//! the extended-Reinhard tone-map + sRGB OETF for fp16 HDR (the exact CPU
//! mirror of `pb-render`'s present pass).

use crate::color::ColorTransform;
use crate::common::downscale_to_fit;
use crate::{DecodeError, DecodedImage, FitBox, PixelFormat};

/// Derive a display-ready sRGB8 thumbnail (long edge ≤ `max_edge`) from a
/// decoded image, consuming it. Returns `None` for pixel formats that never
/// carry a still photo (`Nv12` — video thumbs come from the poster path).
pub fn derive_thumbnail(img: DecodedImage, max_edge: u32) -> Option<DecodedImage> {
    let fit = FitBox {
        max_width: max_edge.max(1),
        max_height: max_edge.max(1),
    };
    match img.format {
        PixelFormat::Rgba8 => derive_rgba8(img, fit).ok(),
        PixelFormat::Rgba16F => derive_f16(img, fit),
        PixelFormat::Nv12 => None,
    }
}

fn derive_rgba8(img: DecodedImage, fit: FitBox) -> Result<DecodedImage, DecodeError> {
    let (mut pixels, w, h) = downscale_to_fit(img.pixels, img.width, img.height, fit)?;
    // Bake the source→sRGB conversion the shader would do (TRC decode → 3×3
    // matrix → sRGB encode), on the small buffer only. Untagged/sRGB sources
    // pass through untouched.
    if img.color.enabled {
        bake_color(&mut pixels, &img.color);
    }
    Ok(DecodedImage {
        width: w,
        height: h,
        orig_width: img.orig_width,
        orig_height: img.orig_height,
        codec: img.codec,
        format: PixelFormat::Rgba8,
        pixels,
        is_preview: img.is_preview,
        color: ColorTransform::srgb(),
        peak: 1.0,
        animated: img.animated,
    })
}

/// fp16 scRGB scene-linear (HDR AVIF/HEIC): downscale in linear f32, then
/// tone-map with the image's peak (the shader's per-channel extended Reinhard)
/// and sRGB-encode. A box filter is plenty at thumbnail scale.
fn derive_f16(img: DecodedImage, fit: FitBox) -> Option<DecodedImage> {
    let (w, h) = (img.width as usize, img.height as usize);
    if img.pixels.len() != w * h * 8 {
        return None;
    }
    let scale = (fit.max_width as f64 / w as f64)
        .min(fit.max_height as f64 / h as f64)
        .min(1.0);
    let tw = ((w as f64 * scale).round() as usize).max(1);
    let th = ((h as f64 * scale).round() as usize).max(1);
    let lw = img.peak.max(1.0);

    let px = |x: usize, y: usize, c: usize| -> f32 {
        let i = (y * w + x) * 8 + c * 2;
        half::f16::from_le_bytes([img.pixels[i], img.pixels[i + 1]]).to_f32()
    };
    let mut out = vec![0u8; tw * th * 4];
    for ty in 0..th {
        // Source row span this output row averages over (box filter).
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).max(y0 + 1).min(h);
        for tx in 0..tw {
            let x0 = tx * w / tw;
            let x1 = ((tx + 1) * w / tw).max(x0 + 1).min(w);
            let n = ((x1 - x0) * (y1 - y0)) as f32;
            let mut acc = [0f32; 3];
            for y in y0..y1 {
                for x in x0..x1 {
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += px(x, y, c);
                    }
                }
            }
            let o = (ty * tw + tx) * 4;
            for c in 0..3 {
                out[o + c] = to_u8(srgb_oetf(reinhard(acc[c] / n, lw)));
            }
            out[o + 3] = 255;
        }
    }
    Some(DecodedImage {
        width: tw as u32,
        height: th as u32,
        orig_width: img.orig_width,
        orig_height: img.orig_height,
        codec: img.codec,
        format: PixelFormat::Rgba8,
        pixels: out,
        is_preview: img.is_preview,
        color: ColorTransform::srgb(),
        peak: 1.0,
        animated: img.animated,
    })
}

/// Bake a tagged SDR source's color into the buffer in place: per channel,
/// TRC-decode to linear, 3×3 matrix to BT.709 primaries, sRGB-encode. The CPU
/// mirror of the scene shader's path for RGBA8 sources.
fn bake_color(pixels: &mut [u8], ct: &ColorTransform) {
    for px in pixels.chunks_exact_mut(4) {
        let lin = [
            trc_eval(px[0] as f32 / 255.0, &ct.trc),
            trc_eval(px[1] as f32 / 255.0, &ct.trc),
            trc_eval(px[2] as f32 / 255.0, &ct.trc),
        ];
        for (i, row) in ct.matrix.iter().enumerate() {
            let v = row[0] * lin[0] + row[1] * lin[1] + row[2] * lin[2];
            px[i] = to_u8(srgb_oetf(v));
        }
    }
}

/// The 7-param TRC `(g, a, b, c, d, e, f)`: encoded → linear (see `color.rs`).
fn trc_eval(x: f32, t: &[f32; 7]) -> f32 {
    let (g, a, b, c, d, e, f) = (t[0], t[1], t[2], t[3], t[4], t[5], t[6]);
    if x < d {
        c * x + f
    } else {
        (a * x + b).max(0.0).powf(g) + e
    }
}

/// sRGB OETF — the exact mirror of the present shader's `srgb_oetf`.
fn srgb_oetf(c: f32) -> f32 {
    let x = c.clamp(0.0, 1.0);
    if x <= 0.0031308 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Per-channel extended Reinhard — the mirror of the present shader's tone-map.
fn reinhard(v: f32, lw: f32) -> f32 {
    let x = v.max(0.0);
    x * (1.0 + x / (lw * lw)) / (1.0 + x)
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// Extract and decode the EXIF **IFD1 thumbnail** (the ~160px JPEG cameras and
/// phones embed) without decoding the main image — a header parse plus a tiny
/// JPEG decode, so a cold thumbnail cell fills in ~a millisecond (task #83 T1).
/// The container's own orientation is applied (thumbnails are stored in sensor
/// order, like RAW previews). `None` when there is no usable thumbnail — the
/// caller falls through to a normal fitted decode.
pub fn exif_thumbnail(bytes: &[u8]) -> Option<DecodedImage> {
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()?;
    let offset = exif
        .get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let len = exif
        .get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let buf = exif.buf();
    let jpeg = buf.get(offset..offset.checked_add(len)?)?;
    let (rgba, w, h) = crate::zune::decode_rgba(jpeg).ok()?;
    if w < 16 || h < 16 {
        return None; // degenerate stub — not worth showing
    }
    // Orientation comes from the CONTAINER (IFD0 et al.), not the thumbnail's
    // own (usually absent) EXIF — the same convention as the RAW preview path.
    let mut img = crate::common::finalize_oriented(
        rgba,
        w,
        h,
        crate::common::read_orientation(bytes),
        "JPEG",
        None,
        true,
    )
    .ok()?;
    img.is_preview = true;
    Some(img)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba8(w: u32, h: u32, rgba: [u8; 4]) -> DecodedImage {
        DecodedImage {
            width: w,
            height: h,
            orig_width: w,
            orig_height: h,
            codec: "test",
            format: PixelFormat::Rgba8,
            pixels: rgba.repeat((w * h) as usize),
            is_preview: false,
            color: ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        }
    }

    #[test]
    fn rgba8_downscales_to_edge_and_stays_srgb() {
        let img = rgba8(4096, 2048, [200, 100, 50, 255]);
        let t = derive_thumbnail(img, 512).unwrap();
        assert_eq!((t.width, t.height), (512, 256));
        assert_eq!(t.format, PixelFormat::Rgba8);
        assert!(!t.color.enabled, "color baked → passthrough");
        // Solid color survives the resize.
        assert_eq!(&t.pixels[..4], &[200, 100, 50, 255]);
    }

    #[test]
    fn small_source_passes_through_unresized() {
        let img = rgba8(320, 240, [10, 20, 30, 255]);
        let t = derive_thumbnail(img, 512).unwrap();
        assert_eq!((t.width, t.height), (320, 240));
    }

    #[test]
    fn portrait_long_edge_is_bounded() {
        let img = rgba8(2000, 6000, [1, 2, 3, 255]);
        let t = derive_thumbnail(img, 512).unwrap();
        assert_eq!(t.height, 512, "long edge = the cap");
        assert!(t.width < 512);
    }

    #[test]
    fn tagged_color_is_baked_to_srgb() {
        // A "linear TRC + halving matrix" fake profile: encoded 255 → linear 1.0
        // → matrix 0.5 → sRGB-encode(0.5) ≈ 188.
        let mut img = rgba8(64, 64, [255, 255, 255, 255]);
        img.color = ColorTransform {
            matrix: [[0.5, 0.0, 0.0], [0.0, 0.5, 0.0], [0.0, 0.0, 0.5]],
            trc: [1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            enabled: true,
        };
        let t = derive_thumbnail(img, 512).unwrap();
        let v = t.pixels[0];
        assert!((186..=190).contains(&v), "srgb(0.5) ≈ 188, got {v}");
        assert!(!t.color.enabled);
    }

    #[test]
    fn f16_hdr_tone_maps_to_srgb8() {
        // A 2×2 fp16 image at scene-linear 1.0 (SDR white), peak 4: reinhard
        // maps 1.0 → (1 + 1/16)/2 = 0.53125 → srgb(0.53125) ≈ 0.7557 → ≈193.
        let one = half::f16::from_f32(1.0).to_le_bytes();
        let mut pixels = Vec::new();
        for _ in 0..4 {
            for _ in 0..4 {
                pixels.extend_from_slice(&one); // r,g,b,a all 1.0
            }
        }
        let img = DecodedImage {
            width: 2,
            height: 2,
            orig_width: 2,
            orig_height: 2,
            codec: "test",
            format: PixelFormat::Rgba16F,
            pixels,
            is_preview: false,
            color: ColorTransform::srgb(),
            peak: 4.0,
            animated: None,
        };
        let t = derive_thumbnail(img, 512).unwrap();
        assert_eq!(t.format, PixelFormat::Rgba8);
        assert_eq!((t.width, t.height), (2, 2));
        let v = t.pixels[0];
        assert!((191..=195).contains(&v), "tone-mapped SDR white ≈193, got {v}");
        assert_eq!(t.pixels[3], 255);
        assert_eq!(t.peak, 1.0);
    }

    #[test]
    fn f16_downscale_averages_in_linear() {
        // 2×1: linear 0.0 and 1.0 → 1×1 box average 0.5 → srgb ≈ 188.
        let z = half::f16::from_f32(0.0).to_le_bytes();
        let o = half::f16::from_f32(1.0).to_le_bytes();
        let mut pixels = Vec::new();
        for _ in 0..4 {
            pixels.extend_from_slice(&z);
        }
        for _ in 0..4 {
            pixels.extend_from_slice(&o);
        }
        let img = DecodedImage {
            width: 2,
            height: 1,
            orig_width: 2,
            orig_height: 1,
            codec: "test",
            format: PixelFormat::Rgba16F,
            pixels,
            is_preview: false,
            color: ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        };
        let t = derive_f16(
            img,
            FitBox {
                max_width: 1,
                max_height: 1,
            },
        )
        .unwrap();
        assert_eq!((t.width, t.height), (1, 1));
        let v = t.pixels[0];
        // reinhard(0.5, 1) = 0.5*(1.5)/1.5 = 0.5 → srgb(0.5) ≈ 188
        assert!((186..=190).contains(&v), "expected ≈188, got {v}");
    }

    #[test]
    fn nv12_is_refused() {
        let img = DecodedImage {
            width: 2,
            height: 2,
            orig_width: 2,
            orig_height: 2,
            codec: "test",
            format: PixelFormat::Nv12,
            pixels: vec![0; 6],
            is_preview: false,
            color: ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        };
        assert!(derive_thumbnail(img, 512).is_none());
    }

    #[test]
    fn exif_thumbnail_absent_is_none() {
        assert!(exif_thumbnail(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]).is_none());
        assert!(exif_thumbnail(b"not an image").is_none());
    }
}
