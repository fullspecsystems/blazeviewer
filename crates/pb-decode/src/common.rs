//! Shared decode finalization used by every backend.
//!
//! A backend's only job is to produce a full-resolution RGBA8 buffer; this module
//! turns that into the `DecodedImage` the app wants — applying EXIF orientation so
//! the photo is upright, recording the true resolution, then decode-to-fit
//! downscaling (high-quality Lanczos3) to the display size. Keeping it here means
//! JPEG, the `image` crate, JXL, SVG and RAW all share one correct path.

use std::io::Cursor;

use crate::orientation::apply_orientation;
use crate::{ColorTransform, DecodeError, DecodedImage, FitBox, PixelFormat};

/// Read the EXIF Orientation tag (1..=8) from a container's bytes; 1 (upright) if
/// absent or unparseable. Works for any container `exif` understands (JPEG, TIFF,
/// HEIF, PNG, WebP).
///
/// Scans every IFD rather than only `In::PRIMARY`: HEIC (and some RAW) place the
/// Orientation tag outside the primary IFD, so a PRIMARY-only lookup wrongly
/// returns 1 and the photo shows sideways. `fields()` yields primary IFD first, so
/// the first Orientation found is the most authoritative.
pub(crate) fn read_orientation(bytes: &[u8]) -> u32 {
    let mut cursor = Cursor::new(bytes);
    if let Ok(exif) = exif::Reader::new().read_from_container(&mut cursor) {
        for field in exif.fields() {
            if field.tag == exif::Tag::Orientation {
                if let Some(v) = field.value.get_uint(0) {
                    return v;
                }
            }
        }
    }
    1
}

/// Turn a freshly decoded full-res RGBA8 buffer into a `DecodedImage`: apply EXIF
/// orientation (when `exif` bytes carry it), record the true resolution, then
/// decode-to-fit downscale. `exif` is the container bytes to read orientation from
/// (`None` to skip, for formats that never carry it).
pub(crate) fn finalize(
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    exif: Option<&[u8]>,
    codec: &'static str,
    fit: Option<FitBox>,
    is_preview: bool,
) -> Result<DecodedImage, DecodeError> {
    let orientation = exif.map(read_orientation).unwrap_or(1);
    finalize_oriented(rgba, w, h, orientation, codec, fit, is_preview)
}

/// Like [`finalize`] but with an already-resolved EXIF orientation value (1..=8;
/// `<=1` means upright). For backends that know the orientation from a source
/// other than the decoded bytes (RAW's container IFD, or a backend that already
/// rotated the pixels — pass 1).
pub(crate) fn finalize_oriented(
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    orientation: u32,
    codec: &'static str,
    fit: Option<FitBox>,
    is_preview: bool,
) -> Result<DecodedImage, DecodeError> {
    if w == 0 || h == 0 {
        return Err(DecodeError::Corrupt("zero-size image".into()));
    }
    if rgba.len() != (w as usize) * (h as usize) * 4 {
        return Err(DecodeError::Corrupt(format!(
            "pixel buffer {} != {w}x{h}x4",
            rgba.len()
        )));
    }

    let (rgba, w, h) = if orientation > 1 {
        apply_orientation(&rgba, w, h, orientation)
    } else {
        (rgba, w, h)
    };
    // The photo's true resolution (oriented, before any decode-to-fit).
    let (orig_width, orig_height) = (w, h);

    let (pixels, width, height) = match fit {
        Some(fit) => downscale_to_fit(rgba, w, h, fit)?,
        None => (rgba, w, h),
    };

    Ok(DecodedImage {
        width,
        height,
        orig_width,
        orig_height,
        codec,
        format: PixelFormat::Rgba8,
        pixels,
        is_preview,
        // sRGB passthrough by default; a backend that extracted an ICC profile
        // overrides `color` on the returned image (see e.g. `zune`, `wic`).
        color: ColorTransform::srgb(),
        peak: 1.0,
        // The caller's decode entry sets this from a header sniff; decoders here only
        // ever produce the still first frame.
        animated: None,
    })
}

/// Finalize an HDR buffer that is already **scene-linear scRGB** (linear, BT.709
/// primaries, extended range — what WIC hands back for PQ/HLG sources) into an
/// `Rgba16F` [`DecodedImage`]: decode-to-fit downscale (in f32, since
/// `fast_image_resize` has no f16 path), then pack to half-floats. `peak` is the
/// max RGB value (the SDR-display tone-map white point). Orientation is assumed
/// applied already (WIC self-orients), so none is done here.
// Only the Windows (`wic.rs`) and macOS (`imageio.rs`) HDR backends call this; on other targets
// (Linux) the lib build has no caller, so allow the dead code there rather than gate the fn out
// (it's still used by the `#[cfg(test)]` tests on every platform).
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) fn finalize_hdr_scrgb(
    mut linear: Vec<f32>,
    mut w: u32,
    mut h: u32,
    codec: &'static str,
    fit: Option<FitBox>,
) -> Result<DecodedImage, DecodeError> {
    if w == 0 || h == 0 || linear.len() != (w as usize) * (h as usize) * 4 {
        return Err(DecodeError::Corrupt("bad HDR buffer".into()));
    }
    let (orig_width, orig_height) = (w, h);
    if let Some(fit) = fit {
        let (out, nw, nh) = downscale_to_fit_f32(linear, w, h, fit)?;
        linear = out;
        w = nw;
        h = nh;
    }
    // Peak (max RGB) before packing — the SDR tone-map white point.
    let mut peak = 1.0f32;
    for px in linear.chunks_exact(4) {
        for &v in &px[0..3] {
            if v > peak {
                peak = v;
            }
        }
    }
    // Pack f32 → f16 (little-endian) for an Rgba16Float texture.
    let mut pixels = vec![0u8; linear.len() * 2];
    for (dst, &v) in pixels.chunks_exact_mut(2).zip(linear.iter()) {
        dst.copy_from_slice(&half::f16::from_f32(v).to_le_bytes());
    }
    Ok(DecodedImage {
        width: w,
        height: h,
        orig_width,
        orig_height,
        codec,
        format: PixelFormat::Rgba16F,
        pixels,
        is_preview: false,
        color: ColorTransform::srgb(), // already scene-linear; shader passes through
        peak,
        animated: None,
    })
}

/// Downscale an RGBA **f32** image to fit `fit` (Lanczos3), for the HDR path.
// Dead on non-Windows/macOS for the same reason as its only caller `finalize_hdr_scrgb`.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn downscale_to_fit_f32(
    pixels: Vec<f32>,
    w: u32,
    h: u32,
    fit: FitBox,
) -> Result<(Vec<f32>, u32, u32), DecodeError> {
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

    let bytes = bytemuck::cast_slice::<f32, u8>(&pixels).to_vec();
    let src = Image::from_vec_u8(w, h, bytes, PixelType::F32x4)
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    let mut dst = Image::new(tw, th, PixelType::F32x4);
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    Resizer::new()
        .resize(&src, &mut dst, &opts)
        .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
    Ok((
        bytemuck::cast_slice::<u8, f32>(&dst.into_vec()).to_vec(),
        tw,
        th,
    ))
}

/// Rotate a tightly-packed RGBA8 buffer by `deg` (0/90/180/270, clockwise), returning the
/// rotated buffer and its new dimensions. Portrait iPhone Live Photos carry a 90°/270° display
/// matrix; ignoring it shows them sideways. Shared by the motion decoders (FFmpeg on Linux,
/// AVAssetReader on macOS — neither applies the container rotation itself).
///
/// Copies whole 4-byte pixels via raw pointers, iterating the destination in row order
/// (sequential writes) — this runs on every motion frame and was ~24% of decode time as a
/// bounds-checked scalar loop building a `[u8; 4]` per pixel.
// Only the motion decoders call this; on targets without one (e.g. Linux without the
// `livephoto` feature, Windows) the lib build has no caller, so allow the dead code there
// (it's still used by the `#[cfg(test)]` tests on every platform).
#[cfg_attr(
    not(any(target_os = "macos", all(unix, feature = "livephoto"))),
    allow(dead_code)
)]
pub(crate) fn rotate_rgba(src: Vec<u8>, w: u32, h: u32, deg: i32) -> (Vec<u8>, u32, u32) {
    let (w, h) = (w as usize, h as usize);
    let d = deg.rem_euclid(360);
    if d == 0 || w == 0 || h == 0 {
        return (src, w as u32, h as u32);
    }
    // 90°/270° swap the axes; 180° keeps them.
    let (nw, nh) = if d == 180 { (w, h) } else { (h, w) };
    let mut out = vec![0u8; nw * nh * 4];
    let sp = src.as_ptr();
    let dp = out.as_mut_ptr();
    for dy in 0..nh {
        for dx in 0..nw {
            // dst (dx,dy) ← src (x,y): each clockwise mapping, inverted. nw==h for 90/270.
            let (x, y) = match d {
                90 => (dy, nw - 1 - dx),
                180 => (w - 1 - dx, h - 1 - dy),
                _ => (nh - 1 - dy, dx), // 270 (nh == w)
            };
            let si = (y * w + x) * 4;
            let di = (dy * nw + dx) * 4;
            // SAFETY: by construction x<w, y<h ⇒ si+4 ≤ w·h·4 = src.len(); dx<nw, dy<nh ⇒
            // di+4 ≤ nw·nh·4 = out.len(). Ranges never overlap the ends.
            unsafe {
                std::ptr::copy_nonoverlapping(sp.add(si), dp.add(di), 4);
            }
        }
    }
    (out, nw as u32, nh as u32)
}

/// Snap a video track's preferred display transform (the 2×2 linear part `[a b; c d]`,
/// translation already dropped) to a **clockwise** rotation quadrant for [`rotate_rgba`]:
/// `Some(0 | 90 | 180 | 270)`, or `None` for anything that isn't a pure quadrant rotation
/// (reflection, skew, scale). iPhone portrait Live Photos carry `(0, 1, -1, 0)` = 90° CW.
/// The caller decides the `None` degradation (PhotoBlaze plays those unrotated rather than
/// refusing to play — a mirrored front-camera clip shows un-mirrored).
// Only the macOS AVAssetReader motion decoder calls this (FFmpeg reads its rotation from
// the display matrix side data instead); the tests run everywhere.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn transform_to_quadrant(a: f64, b: f64, c: f64, d: f64) -> Option<i32> {
    const EPS: f64 = 1e-3;
    let m = |x: f64, want: f64| (x - want).abs() < EPS;
    // (a, b, c, d) patterns for each clockwise display rotation.
    for (deg, pa, pb, pc, pd) in [
        (0, 1.0, 0.0, 0.0, 1.0),
        (90, 0.0, 1.0, -1.0, 0.0),
        (180, -1.0, 0.0, 0.0, -1.0),
        (270, 0.0, -1.0, 1.0, 0.0),
    ] {
        if m(a, pa) && m(b, pb) && m(c, pc) && m(d, pd) {
            return Some(deg);
        }
    }
    None
}

/// Copy a BGRA8 pixel buffer whose rows are padded to `stride` bytes (a CVPixelBuffer /
/// Media Foundation layout) into tightly-packed straight-alpha RGBA8. `None` if the
/// geometry is inconsistent (zero dims, `stride < w*4`, buffer shorter than the last
/// row's end) or a size computation overflows.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn bgra_to_rgba_tight(src: &[u8], w: usize, h: usize, stride: usize) -> Option<Vec<u8>> {
    if w == 0 || h == 0 {
        return None;
    }
    let row_bytes = w.checked_mul(4)?;
    if stride < row_bytes {
        return None;
    }
    // The last row need not be padded out to `stride`.
    let needed = stride.checked_mul(h - 1)?.checked_add(row_bytes)?;
    if src.len() < needed {
        return None;
    }
    let mut out = vec![0u8; row_bytes.checked_mul(h)?];
    for y in 0..h {
        let src_row = &src[y * stride..y * stride + row_bytes];
        let dst_row = &mut out[y * row_bytes..(y + 1) * row_bytes];
        for (d, s) in dst_row.chunks_exact_mut(4).zip(src_row.chunks_exact(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = s[3];
        }
    }
    Some(out)
}

/// Expand tightly-packed RGB8 to RGBA8 (opaque). Used by backends that decode to
/// 3-channel (e.g. the RAW demosaic pipeline).
pub(crate) fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let n = rgb.len() / 3;
    let mut out = vec![0u8; n * 4];
    for i in 0..n {
        out[i * 4..i * 4 + 3].copy_from_slice(&rgb[i * 3..i * 3 + 3]);
        out[i * 4 + 3] = 255;
    }
    out
}

/// Downscale an RGBA8 image to fit within `fit` (preserving aspect, never
/// upscaling) using a high-quality Lanczos3 filter. Returns the input unchanged
/// when it already fits.
pub(crate) fn downscale_to_fit(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscale_to_fit_leaves_fitting_images_untouched() {
        let pixels = vec![1u8; 8 * 8 * 4];
        let (out, w, h) = downscale_to_fit(
            pixels,
            8,
            8,
            FitBox {
                max_width: 100,
                max_height: 100,
            },
        )
        .unwrap();
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
        let (out, ow, oh) = downscale_to_fit(
            pixels,
            w,
            h,
            FitBox {
                max_width: 50,
                max_height: 50,
            },
        )
        .unwrap();
        assert_eq!((ow, oh), (50, 50));
        assert_eq!(out.len(), (50 * 50 * 4) as usize);
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

    #[test]
    fn finalize_hdr_packs_f16_and_records_peak() {
        // 1x1 scRGB-linear HDR pixel with an 8x highlight in green.
        let img = finalize_hdr_scrgb(vec![0.5f32, 8.0, 0.25, 1.0], 1, 1, "AVIF", None).unwrap();
        assert_eq!(img.format, PixelFormat::Rgba16F);
        assert_eq!(img.pixels.len(), 8); // 1px * 4 channels * 2 bytes
        assert!((img.peak - 8.0).abs() < 0.1, "peak {}", img.peak);
        // Round-trip the green half-float back to f32.
        let g = half::f16::from_le_bytes([img.pixels[2], img.pixels[3]]).to_f32();
        assert!((g - 8.0).abs() < 0.05, "green {g}");
    }

    #[test]
    fn finalize_hdr_downscales_to_fit() {
        let big = vec![1.0f32; 100 * 100 * 4];
        let img = finalize_hdr_scrgb(
            big,
            100,
            100,
            "AVIF",
            Some(FitBox {
                max_width: 40,
                max_height: 40,
            }),
        )
        .unwrap();
        assert_eq!((img.width, img.height), (40, 40));
        assert_eq!((img.orig_width, img.orig_height), (100, 100));
        assert_eq!(img.pixels.len(), 40 * 40 * 8);
    }

    /// A 2×3 buffer of distinct pixels; each pixel's R channel encodes its index.
    fn px_grid(w: usize, h: usize) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| [i as u8, 100 + i as u8, 200u8, 255])
            .collect()
    }

    /// The R channel of pixel (x, y) in a tightly-packed RGBA buffer of width `w`.
    fn r_at(buf: &[u8], w: usize, x: usize, y: usize) -> u8 {
        buf[(y * w + x) * 4]
    }

    #[test]
    fn rotate_rgba_0_is_identity() {
        let src = px_grid(2, 3);
        let (out, w, h) = rotate_rgba(src.clone(), 2, 3, 0);
        assert_eq!((w, h), (2, 3));
        assert_eq!(out, src);
    }

    #[test]
    fn rotate_rgba_90_cw_swaps_dims_and_maps_pixels() {
        // src (2 wide × 3 tall), indices:   0 1        90° CW:   4 2 0
        //                                   2 3   →              5 3 1
        //                                   4 5
        let (out, w, h) = rotate_rgba(px_grid(2, 3), 2, 3, 90);
        assert_eq!((w, h), (3, 2));
        assert_eq!(r_at(&out, 3, 0, 0), 4);
        assert_eq!(r_at(&out, 3, 1, 0), 2);
        assert_eq!(r_at(&out, 3, 2, 0), 0);
        assert_eq!(r_at(&out, 3, 0, 1), 5);
        assert_eq!(r_at(&out, 3, 2, 1), 1);
    }

    #[test]
    fn rotate_rgba_180_reverses() {
        let (out, w, h) = rotate_rgba(px_grid(2, 3), 2, 3, 180);
        assert_eq!((w, h), (2, 3));
        assert_eq!(r_at(&out, 2, 0, 0), 5);
        assert_eq!(r_at(&out, 2, 1, 2), 0);
    }

    #[test]
    fn rotate_rgba_270_cw_swaps_dims_and_maps_pixels() {
        // 270° CW (= 90° CCW):   1 3 5
        //                        0 2 4
        let (out, w, h) = rotate_rgba(px_grid(2, 3), 2, 3, 270);
        assert_eq!((w, h), (3, 2));
        assert_eq!(r_at(&out, 3, 0, 0), 1);
        assert_eq!(r_at(&out, 3, 2, 0), 5);
        assert_eq!(r_at(&out, 3, 0, 1), 0);
        assert_eq!(r_at(&out, 3, 2, 1), 4);
    }

    #[test]
    fn rotate_rgba_four_quarters_round_trip() {
        let src = px_grid(2, 3);
        let (a, w, h) = rotate_rgba(src.clone(), 2, 3, 90);
        let (b, w, h) = rotate_rgba(a, w, h, 90);
        let (c, w, h) = rotate_rgba(b, w, h, 90);
        let (d, w, h) = rotate_rgba(c, w, h, 90);
        assert_eq!((w, h), (2, 3));
        assert_eq!(d, src);
    }

    #[test]
    fn transform_to_quadrant_maps_the_four_rotations() {
        assert_eq!(transform_to_quadrant(1.0, 0.0, 0.0, 1.0), Some(0));
        // iPhone portrait (UIImageOrientationRight): rotate 90° CW to display upright.
        assert_eq!(transform_to_quadrant(0.0, 1.0, -1.0, 0.0), Some(90));
        assert_eq!(transform_to_quadrant(-1.0, 0.0, 0.0, -1.0), Some(180));
        assert_eq!(transform_to_quadrant(0.0, -1.0, 1.0, 0.0), Some(270));
    }

    #[test]
    fn transform_to_quadrant_tolerates_float_noise() {
        assert_eq!(
            transform_to_quadrant(0.0002, 0.9999, -1.0001, -0.0003),
            Some(90)
        );
    }

    #[test]
    fn transform_to_quadrant_rejects_non_rotations() {
        // Reflection (horizontal mirror), skew, and scale are not quadrant rotations.
        assert_eq!(transform_to_quadrant(-1.0, 0.0, 0.0, 1.0), None);
        assert_eq!(transform_to_quadrant(1.0, 0.5, 0.0, 1.0), None);
        assert_eq!(transform_to_quadrant(2.0, 0.0, 0.0, 2.0), None);
    }

    #[test]
    fn bgra_to_rgba_swizzles_and_honors_stride() {
        // 2×2 BGRA with 4 bytes of row padding (stride 12): unique channel values.
        #[rustfmt::skip]
        let src = [
            10, 20, 30, 40,   50, 60, 70, 80,   0, 0, 0, 0, // row 0 + pad
            90, 100, 110, 120, 130, 140, 150, 160,          // row 1, unpadded tail
        ];
        let out = bgra_to_rgba_tight(&src, 2, 2, 12).unwrap();
        // BGRA (10,20,30,40) → RGBA (30,20,10,40); padding never read.
        assert_eq!(
            out,
            vec![30, 20, 10, 40, 70, 60, 50, 80, 110, 100, 90, 120, 150, 140, 130, 160]
        );
    }

    #[test]
    fn bgra_to_rgba_rejects_bad_geometry() {
        assert!(
            bgra_to_rgba_tight(&[0; 16], 0, 2, 8).is_none(),
            "zero width"
        );
        assert!(
            bgra_to_rgba_tight(&[0; 16], 2, 0, 8).is_none(),
            "zero height"
        );
        assert!(
            bgra_to_rgba_tight(&[0; 16], 2, 2, 4).is_none(),
            "stride < w*4"
        );
        assert!(
            bgra_to_rgba_tight(&[0; 15], 2, 2, 8).is_none(),
            "buffer short"
        );
        assert!(
            bgra_to_rgba_tight(&[0; 16], usize::MAX / 2, 2, usize::MAX).is_none(),
            "overflow"
        );
    }

    #[test]
    fn finalize_rejects_mismatched_buffer() {
        let err = finalize(vec![0u8; 10], 4, 4, None, "X", None, false);
        assert!(matches!(err, Err(DecodeError::Corrupt(_))));
    }

    #[test]
    fn finalize_passes_through_full_res() {
        let img = finalize(vec![9u8; 2 * 2 * 4], 2, 2, None, "X", None, false).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!((img.orig_width, img.orig_height), (2, 2));
        assert!(img.is_well_formed());
        assert_eq!(img.codec, "X");
    }
}
