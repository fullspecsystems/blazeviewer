//! CPU NV12 → RGBA8 conversion (task 79.10) — the correctness reference and
//! portability fallback behind [`crate::Renderer::set_video_nv12`]'s default
//! implementation. The wgpu two-plane shader path must match this within a small
//! tolerance (the phase-B golden test), so the coefficients live in exactly one
//! place per crate boundary and are unit-tested by encode/decode round-trips.

/// YUV→RGB matrix coefficients (H.273 families) for NV12 video frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// Everything the convert (shader or CPU) needs: the matrix family plus whether
/// the source is full-range (JPEG-style) or limited/video-range (16–235 luma).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YuvParams {
    pub matrix: YuvMatrix,
    pub full_range: bool,
}

impl YuvMatrix {
    /// The (Kr, Kb) luma coefficients this family is defined by.
    pub fn kr_kb(self) -> (f32, f32) {
        match self {
            YuvMatrix::Bt601 => (0.299, 0.114),
            YuvMatrix::Bt709 => (0.2126, 0.0722),
            YuvMatrix::Bt2020 => (0.2627, 0.0593),
        }
    }

    /// The four derived convert coefficients `(a, b, c, d)` for
    /// `r = y + a·v; g = y − b·u − c·v; b = y + d·u` — computed once here so the
    /// CPU reference and the shader uniform can never drift apart.
    pub(crate) fn coeffs(self) -> (f32, f32, f32, f32) {
        let (kr, kb) = self.kr_kb();
        let kg = 1.0 - kr - kb;
        (
            2.0 * (1.0 - kr),
            2.0 * kb * (1.0 - kb) / kg,
            2.0 * kr * (1.0 - kr) / kg,
            2.0 * (1.0 - kb),
        )
    }
}

/// Convert a tightly packed NV12 frame (full-res Y plane, then the interleaved
/// half-res UV plane) to straight-alpha RGBA8. Chroma is nearest-sampled (each
/// 2×2 luma block shares its UV sample) — the GPU path's bilinear differs below
/// visual threshold at fit scale, and nearest keeps this reference exact.
/// `width`/`height` must be even (NV12's requirement; the producer guarantees it).
pub fn nv12_to_rgba(y: &[u8], uv: &[u8], width: u32, height: u32, p: YuvParams) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    debug_assert_eq!(y.len(), w * h, "Y plane size");
    debug_assert_eq!(uv.len(), w * h / 2, "UV plane size");
    // r = y + a·v ; g = y − b·u − c·v ; b = y + d·u  (normalized y, centered u/v)
    let (a, bq, cq, d) = p.matrix.coeffs();

    let mut out = vec![255u8; w * h * 4];
    for row in 0..h {
        let uv_row = &uv[(row / 2) * w..(row / 2) * w + w];
        for col in 0..w {
            let yv = y[row * w + col] as f32;
            let u8v = uv_row[col & !1] as f32;
            let v8v = uv_row[(col & !1) + 1] as f32;
            let (yn, un, vn) = if p.full_range {
                (yv / 255.0, (u8v - 128.0) / 255.0, (v8v - 128.0) / 255.0)
            } else {
                (
                    (yv - 16.0) / 219.0,
                    (u8v - 128.0) / 224.0,
                    (v8v - 128.0) / 224.0,
                )
            };
            let r = yn + a * vn;
            let g = yn - bq * un - cq * vn;
            let b = yn + d * un;
            let q = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
            let o = (row * w + col) * 4;
            out[o] = q(r);
            out[o + 1] = q(g);
            out[o + 2] = q(b);
            // alpha stays 255 from the fill
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode one RGB value to (Y, U, V) bytes with the same coefficients, per range.
    fn encode(rgb: [f32; 3], m: YuvMatrix, full: bool) -> (u8, u8, u8) {
        let (kr, kb) = m.kr_kb();
        let kg = 1.0 - kr - kb;
        let [r, g, b] = rgb;
        let y = kr * r + kg * g + kb * b;
        let u = (b - y) / (2.0 * (1.0 - kb));
        let v = (r - y) / (2.0 * (1.0 - kr));
        if full {
            (
                (y * 255.0).round() as u8,
                (u * 255.0 + 128.0).round() as u8,
                (v * 255.0 + 128.0).round() as u8,
            )
        } else {
            (
                (y * 219.0 + 16.0).round() as u8,
                (u * 224.0 + 128.0).round() as u8,
                (v * 224.0 + 128.0).round() as u8,
            )
        }
    }

    /// A 2×2 single-color NV12 frame decoded back must round-trip the RGB within
    /// quantization error, for every matrix × range combination.
    #[test]
    fn round_trips_primaries_within_quantization() {
        let colors: [[f32; 3]; 6] = [
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.25, 0.5, 0.75],
        ];
        for m in [YuvMatrix::Bt601, YuvMatrix::Bt709, YuvMatrix::Bt2020] {
            for full in [false, true] {
                for c in colors {
                    let (y, u, v) = encode(c, m, full);
                    let yp = [y; 4];
                    let uv = [u, v]; // one shared UV sample for the 2×2 block
                    let out = nv12_to_rgba(
                        &yp,
                        &uv,
                        2,
                        2,
                        YuvParams {
                            matrix: m,
                            full_range: full,
                        },
                    );
                    let want = [
                        (c[0] * 255.0).round() as i32,
                        (c[1] * 255.0).round() as i32,
                        (c[2] * 255.0).round() as i32,
                    ];
                    for px in 0..4 {
                        for ch in 0..3 {
                            let got = out[px * 4 + ch] as i32;
                            assert!(
                                (got - want[ch]).abs() <= 2,
                                "{m:?} full={full} {c:?}: ch{ch} got {got} want {}",
                                want[ch]
                            );
                        }
                        assert_eq!(out[px * 4 + 3], 255, "alpha forced opaque");
                    }
                }
            }
        }
    }

    /// Limited-range black (Y=16) and white (Y=235) hit exactly 0 / 255, and
    /// values below/above the range clamp instead of wrapping.
    #[test]
    fn limited_range_endpoints_and_clamping() {
        let p = YuvParams {
            matrix: YuvMatrix::Bt709,
            full_range: false,
        };
        let out = nv12_to_rgba(&[16, 235, 8, 250], &[128, 128], 2, 2, p);
        assert_eq!(&out[0..3], &[0, 0, 0], "Y=16 is black");
        assert_eq!(&out[4..7], &[255, 255, 255], "Y=235 is white");
        assert_eq!(&out[8..11], &[0, 0, 0], "sub-black clamps");
        assert_eq!(&out[12..15], &[255, 255, 255], "super-white clamps");
    }
}
