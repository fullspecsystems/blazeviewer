//! EXIF orientation correction.
//!
//! Cameras (especially phones) often store a photo in sensor order plus an EXIF
//! Orientation tag (1..=8) describing the rotation/flip needed to display it
//! upright. Applying it here, as a pure function over an RGBA8 buffer, keeps the
//! renderer simple and the logic fully unit-testable. The 90/270 rotations swap
//! width and height.

/// Apply EXIF `orientation` (1..=8) to an RGBA8 image, returning `(pixels, w, h)`.
/// Orientation 1 — or any out-of-range value — is identity.
pub fn apply_orientation(pixels: &[u8], w: u32, h: u32, orientation: u32) -> (Vec<u8>, u32, u32) {
    apply_orientation_bytes(pixels, w, h, orientation, 4)
}

/// Like [`apply_orientation`] but for an arbitrary pixel byte-width `bpp` (4 for
/// RGBA8, 16 for the HDR `Rgba32Float` scratch buffer). The remap is pure index
/// math, so it's identical regardless of channel layout — only the block size differs.
pub(crate) fn apply_orientation_bytes(
    pixels: &[u8],
    w: u32,
    h: u32,
    orientation: u32,
    bpp: usize,
) -> (Vec<u8>, u32, u32) {
    debug_assert_eq!(pixels.len(), (w as usize) * (h as usize) * bpp);
    if orientation <= 1 || orientation > 8 {
        return (pixels.to_vec(), w, h);
    }

    // Orientations 5..=8 are quarter-turns, so the output dimensions transpose.
    let (ow, oh) = if matches!(orientation, 5..=8) {
        (h, w)
    } else {
        (w, h)
    };

    let mut out = vec![0u8; (ow as usize) * (oh as usize) * bpp];
    for oy in 0..oh {
        for ox in 0..ow {
            // Map each output pixel back to its source pixel.
            let (sx, sy) = match orientation {
                2 => (w - 1 - ox, oy),         // flip horizontal
                3 => (w - 1 - ox, h - 1 - oy), // rotate 180
                4 => (ox, h - 1 - oy),         // flip vertical
                5 => (oy, ox),                 // transpose
                6 => (oy, h - 1 - ox),         // rotate 90 CW
                7 => (w - 1 - oy, h - 1 - ox), // transverse
                8 => (w - 1 - oy, ox),         // rotate 90 CCW
                _ => (ox, oy),
            };
            let s = (sy as usize * w as usize + sx as usize) * bpp;
            let d = (oy as usize * ow as usize + ox as usize) * bpp;
            out[d..d + bpp].copy_from_slice(&pixels[s..s + bpp]);
        }
    }
    (out, ow, oh)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 2x2 image with a distinct red-channel id per pixel:
    //   A B      (id 10 20)
    //   C D      (id 30 40)
    fn quad() -> Vec<u8> {
        let mk = |id: u8| [id, 0, 0, 255];
        let (a, b, c, d) = (mk(10), mk(20), mk(30), mk(40));
        [a, b, c, d].concat()
    }

    fn ids(pixels: &[u8]) -> Vec<u8> {
        pixels.chunks_exact(4).map(|p| p[0]).collect()
    }

    #[test]
    fn identity_is_unchanged() {
        let (out, w, h) = apply_orientation(&quad(), 2, 2, 1);
        assert_eq!((w, h), (2, 2));
        assert_eq!(ids(&out), vec![10, 20, 30, 40]);
    }

    #[test]
    fn flip_horizontal() {
        let (out, _, _) = apply_orientation(&quad(), 2, 2, 2);
        assert_eq!(ids(&out), vec![20, 10, 40, 30]);
    }

    #[test]
    fn rotate_180() {
        let (out, _, _) = apply_orientation(&quad(), 2, 2, 3);
        assert_eq!(ids(&out), vec![40, 30, 20, 10]);
    }

    #[test]
    fn rotate_90_cw() {
        // A B  ->  C A
        // C D      D B
        let (out, w, h) = apply_orientation(&quad(), 2, 2, 6);
        assert_eq!((w, h), (2, 2));
        assert_eq!(ids(&out), vec![30, 10, 40, 20]);
    }

    #[test]
    fn rotate_90_ccw() {
        // A B  ->  B D
        // C D      A C
        let (out, _, _) = apply_orientation(&quad(), 2, 2, 8);
        assert_eq!(ids(&out), vec![20, 40, 10, 30]);
    }

    #[test]
    fn quarter_turn_swaps_dimensions() {
        // 2 wide, 1 tall -> 1 wide, 2 tall.
        let row = [[10, 0, 0, 255], [20, 0, 0, 255]].concat();
        let (out, w, h) = apply_orientation(&row, 2, 1, 6);
        assert_eq!((w, h), (1, 2));
        assert_eq!(ids(&out), vec![10, 20]);
    }
}
