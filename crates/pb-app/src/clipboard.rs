//! Copy the current photo to the OS clipboard (tasks.json #27).
//!
//! The pure pixel transforms — format conversion (`to_clipboard_rgba8`) and the
//! 90° rotation bake (`rotate_rgba8`) — are unit-tested here; the [`set_image`]
//! `arboard` write is the thin I/O shell. We copy the **full-resolution** decode,
//! not the fit-downscaled ring texture (see `App::copy_image`), so a paste lands at
//! the photo's native size.
//!
//! Privacy (tasks.json #2): the clipboard is an explicit, user-initiated command —
//! not a passive viewing trace — so it's outside the no-trace guarantee. (Windows
//! clipboard history / Win+V may retain it, but that's the user's own OS setting.)

use pb_decode::{DecodedImage, PixelFormat};
use pb_render::Rotation;
use std::borrow::Cow;

/// sRGB OETF (scene-linear → sRGB-encoded), matching the present shader's
/// `srgb_oetf` in `pb-render/src/gpu.rs`.
fn srgb_oetf(c: f32) -> f32 {
    let x = c.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Extended-Reinhard tone-map with white point `lw`, matching the present shader's
/// `reinhard`. `lw = 1` is the identity (faithful SDR); a larger `lw` rolls HDR
/// highlights into the displayable range.
fn reinhard(v: f32, lw: f32) -> f32 {
    let x = v.max(0.0);
    x * (1.0 + x / (lw * lw)) / (1.0 + x)
}

/// Convert a decoded image to a straight-alpha RGBA8 buffer for the clipboard.
///
/// - `Rgba8` is taken as-is (source-encoded sRGB). The DIB clipboard format carries
///   no ICC profile, so a wide-gamut source pastes interpreted as sRGB — a
///   documented v1 limitation, the same one the on-screen path would have without
///   its in-shader color transform.
/// - `Rgba16F` (HDR scene-linear scRGB) is tone-mapped to SDR sRGB8 exactly as the
///   SDR present pass does: extended-Reinhard at the image `peak`, then sRGB-encode.
///   The clipboard is always 8-bit, so this is correct regardless of desktop HDR.
pub fn to_clipboard_rgba8(img: &DecodedImage) -> Vec<u8> {
    match img.format {
        PixelFormat::Rgba8 => img.pixels.clone(),
        PixelFormat::Rgba16F => {
            let lw = img.peak.max(1.0);
            let px_count = (img.width as usize) * (img.height as usize);
            let mut out = Vec::with_capacity(px_count * 4);
            // 4 half-floats (8 bytes) per pixel, little-endian.
            for px in img.pixels.chunks_exact(8) {
                for ch in 0..3 {
                    let h = half::f16::from_le_bytes([px[ch * 2], px[ch * 2 + 1]]);
                    let v = srgb_oetf(reinhard(h.to_f32(), lw));
                    out.push((v * 255.0 + 0.5) as u8);
                }
                out.push(255); // opaque; HDR sources have no meaningful alpha here
            }
            out
        }
    }
}

/// Rotate a tightly-packed RGBA8 buffer by a 90° quadrant (clockwise), returning the
/// rotated buffer and its new dimensions. `R0` clones unchanged. Used to bake the
/// in-RAM rotation override (the `r` / `Shift+R` overlay transform, which is a GPU
/// transform — not baked into the decoded pixels) into the copied image so the
/// clipboard is WYSIWYG.
pub fn rotate_rgba8(pixels: &[u8], w: u32, h: u32, rot: Rotation) -> (Vec<u8>, u32, u32) {
    if rot == Rotation::R0 {
        return (pixels.to_vec(), w, h);
    }
    let (wu, hu) = (w as usize, h as usize);
    let (new_w, new_h) = if rot.swaps_axes() { (h, w) } else { (w, h) };
    let nwu = new_w as usize;
    let mut out = vec![0u8; wu * hu * 4];
    for sy in 0..hu {
        for sx in 0..wu {
            // Destination pixel coordinates for this source pixel after the turn.
            let (dx, dy) = match rot {
                Rotation::R90 => (hu - 1 - sy, sx),
                Rotation::R180 => (wu - 1 - sx, hu - 1 - sy),
                Rotation::R270 => (sy, wu - 1 - sx),
                Rotation::R0 => unreachable!(),
            };
            let src = (sy * wu + sx) * 4;
            let dst = (dy * nwu + dx) * 4;
            out[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
        }
    }
    (out, new_w, new_h)
}

/// Write an RGBA8 image to the OS clipboard. On Windows `arboard` sets CF_DIB /
/// CF_DIBV5; on macOS (later) it sets the equivalent pasteboard image.
pub fn set_image(width: u32, height: u32, rgba: Vec<u8>) -> Result<(), arboard::Error> {
    let mut cb = arboard::Clipboard::new()?;
    cb.set_image(arboard::ImageData {
        width: width as usize,
        height: height as usize,
        bytes: Cow::Owned(rgba),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb_decode::ColorTransform;

    fn img_rgba8(width: u32, height: u32, pixels: Vec<u8>) -> DecodedImage {
        DecodedImage {
            width,
            height,
            orig_width: width,
            orig_height: height,
            codec: "TEST",
            format: PixelFormat::Rgba8,
            pixels,
            is_preview: false,
            color: ColorTransform::srgb(),
            peak: 1.0,
        }
    }

    #[test]
    fn rgba8_is_copied_verbatim() {
        let px = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let img = img_rgba8(2, 1, px.clone());
        assert_eq!(to_clipboard_rgba8(&img), px);
    }

    #[test]
    fn rgba16f_tonemaps_to_srgb8_opaque() {
        // One scene-linear pixel: R=1.0, G=0.0, B=0.5, A=1.0 packed as f16 LE.
        let mut bytes = Vec::new();
        for v in [1.0f32, 0.0, 0.5, 1.0] {
            bytes.extend_from_slice(&half::f16::from_f32(v).to_le_bytes());
        }
        let mut img = img_rgba8(1, 1, Vec::new());
        img.format = PixelFormat::Rgba16F;
        img.pixels = bytes;
        let out = to_clipboard_rgba8(&img);
        assert_eq!(out.len(), 4);
        // peak=1 ⇒ reinhard is identity; sRGB-encode 1.0/0.0/0.5.
        assert_eq!(out[0], 255); // 1.0 → white
        assert_eq!(out[1], 0); //   0.0 → black
        assert_eq!(out[3], 255); // forced opaque
        let mid = (srgb_oetf(0.5) * 255.0 + 0.5) as u8;
        assert_eq!(out[2], mid); // ~188
    }

    /// A 2×2 image with four distinct, identifiable pixels (R, G, B, W).
    fn quad() -> (Vec<u8>, u32, u32) {
        #[rustfmt::skip]
        let px = vec![
            255,0,0,255,   0,255,0,255,   // (0,0)=R (1,0)=G
            0,0,255,255,   255,255,255,255 // (0,1)=B (1,1)=W
        ];
        (px, 2, 2)
    }

    fn pixel_at(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn rotate_r0_is_identity() {
        let (px, w, h) = quad();
        let (out, nw, nh) = rotate_rgba8(&px, w, h, Rotation::R0);
        assert_eq!((nw, nh), (2, 2));
        assert_eq!(out, px);
    }

    #[test]
    fn rotate_r90_cw_moves_topleft_to_topright() {
        let (px, w, h) = quad();
        let (out, nw, nh) = rotate_rgba8(&px, w, h, Rotation::R90);
        assert_eq!((nw, nh), (2, 2));
        // 90° CW: top-left R → top-right; top-right G → bottom-right;
        // bottom-right W → bottom-left; bottom-left B → top-left.
        assert_eq!(pixel_at(&out, nw, 1, 0), [255, 0, 0, 255]); // R
        assert_eq!(pixel_at(&out, nw, 0, 0), [0, 0, 255, 255]); // B
        assert_eq!(pixel_at(&out, nw, 1, 1), [0, 255, 0, 255]); // G
        assert_eq!(pixel_at(&out, nw, 0, 1), [255, 255, 255, 255]); // W
    }

    #[test]
    fn rotate_dims_swap_on_quarter_turns() {
        // A 3×2 image becomes 2×3 after a quarter turn, unchanged after a half turn.
        let px = vec![0u8; 3 * 2 * 4];
        let (_, w90, h90) = rotate_rgba8(&px, 3, 2, Rotation::R90);
        assert_eq!((w90, h90), (2, 3));
        let (_, w270, h270) = rotate_rgba8(&px, 3, 2, Rotation::R270);
        assert_eq!((w270, h270), (2, 3));
        let (_, w180, h180) = rotate_rgba8(&px, 3, 2, Rotation::R180);
        assert_eq!((w180, h180), (3, 2));
    }

    #[test]
    fn four_cw_rotations_return_to_original() {
        let (px, w, h) = quad();
        let (mut cur, mut cw, mut ch) = (px.clone(), w, h);
        for _ in 0..4 {
            let (n, nw, nh) = rotate_rgba8(&cur, cw, ch, Rotation::R90);
            cur = n;
            cw = nw;
            ch = nh;
        }
        assert_eq!((cw, ch), (w, h));
        assert_eq!(cur, px);
    }
}
