//! Planar YUV → straight-alpha RGBA8 conversion for the dav1d avis backend —
//! task #76 (plan phase 6). Pure Rust and dav1d-free, so the math unit-tests
//! run on every platform.
//!
//! Scope (documented, deliberate): nearest-neighbor (co-sited) chroma
//! upsampling — visually fine at photo scale, bilinear is a measured
//! follow-up; 10/12-bit narrowed to 8 after the matrix with round-to-nearest;
//! the result is **source-gamut** RGB — primaries/TRC ride the
//! `ColorTransform` into the shader (the second, separate color stage).

use crate::DecodeError;

/// A borrowed planar view: `rows × stride` bytes, samples `u8` (bpc 8) or
/// little-endian `u16` (bpc 10/12) starting each row at `y * stride`.
pub(crate) struct Plane<'a> {
    pub data: &'a [u8],
    /// Row pitch in **bytes** (dav1d convention).
    pub stride: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PixelLayout {
    I400,
    I420,
    I422,
    I444,
}

impl PixelLayout {
    /// (log2 horizontal, log2 vertical) chroma subsampling.
    fn subsampling(self) -> (u32, u32) {
        match self {
            PixelLayout::I420 => (1, 1),
            PixelLayout::I422 => (1, 0),
            PixelLayout::I400 | PixelLayout::I444 => (0, 0),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Matrix {
    /// CICP 0: planes are G (Y-slot), B (U-slot), R (V-slot) — no matrix.
    Identity,
    /// CICP 5/6 (and 2 "unspecified" by our documented convention).
    Bt601,
    /// CICP 1.
    Bt709,
    /// CICP 9 (non-constant luminance).
    Bt2020,
}

impl Matrix {
    /// (Kr, Kb) luma coefficients.
    fn kr_kb(self) -> (f32, f32) {
        match self {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
            Matrix::Identity => unreachable!("identity has no matrix"),
        }
    }
}

/// Convert one picture's planes to straight-alpha RGBA8 (`w*h*4` bytes).
///
/// `u`/`v` are required for every layout except I400 (gray). Identity
/// (RGB-coded) requires I444. All plane access is bounds-checked against the
/// borrowed slices, so a hostile stride/geometry combination is an `Err`,
/// never UB or a panic.
#[allow(clippy::too_many_arguments)] // a focused one-shot converter, not an API surface
pub(crate) fn to_rgba8(
    y: Plane<'_>,
    u: Option<Plane<'_>>,
    v: Option<Plane<'_>>,
    w: u32,
    h: u32,
    bpc: u32,
    layout: PixelLayout,
    matrix: Matrix,
    full_range: bool,
) -> Result<Vec<u8>, DecodeError> {
    if w == 0 || h == 0 {
        return Err(DecodeError::Corrupt("yuv: empty image".into()));
    }
    if !matches!(bpc, 8 | 10 | 12) {
        return Err(DecodeError::Corrupt(format!("yuv: bpc {bpc}")));
    }
    if matrix == Matrix::Identity && layout != PixelLayout::I444 {
        return Err(DecodeError::Corrupt(
            "yuv: identity matrix requires 4:4:4".into(),
        ));
    }
    if layout != PixelLayout::I400 && (u.is_none() || v.is_none()) {
        return Err(DecodeError::Corrupt("yuv: missing chroma planes".into()));
    }

    let (w, h) = (w as usize, h as usize);
    let (ssx, ssy) = layout.subsampling();
    let cw = w.div_ceil(1 << ssx);
    let bytes_per = if bpc == 8 { 1 } else { 2 };

    // Validate every row range up front so the hot loop can slice unchecked-
    // by-construction (still safe indexing — just no per-pixel Option).
    let row = |p: &Plane<'_>, row_idx: usize, width: usize| -> Result<(), DecodeError> {
        let start = row_idx
            .checked_mul(p.stride)
            .ok_or_else(|| DecodeError::Corrupt("yuv: stride overflow".into()))?;
        let end = start
            .checked_add(width * bytes_per)
            .ok_or_else(|| DecodeError::Corrupt("yuv: row overflow".into()))?;
        if end > p.data.len() {
            return Err(DecodeError::Corrupt("yuv: plane too small".into()));
        }
        Ok(())
    };
    let ch = h.div_ceil(1 << ssy);
    for r in 0..h {
        row(&y, r, w)?;
    }
    if let (Some(u), Some(v)) = (u.as_ref(), v.as_ref()) {
        for r in 0..ch {
            row(u, r, cw)?;
            row(v, r, cw)?;
        }
    }

    // Normalization constants. Limited (video) range: Y in [16..235]<<(bpc-8),
    // C in [16..240]<<(bpc-8); full range: [0..2^bpc-1] with chroma centered.
    let shift = bpc - 8;
    let (y_off, y_scale, c_scale) = if full_range {
        let max = ((1u32 << bpc) - 1) as f32;
        (0.0f32, max, max)
    } else {
        (
            (16u32 << shift) as f32,
            (219u32 << shift) as f32,
            (224u32 << shift) as f32,
        )
    };
    let c_off = (1u32 << (bpc - 1)) as f32;

    let sample = |p: &Plane<'_>, x: usize, row_idx: usize| -> f32 {
        let at = row_idx * p.stride + x * bytes_per;
        if bpc == 8 {
            p.data[at] as f32
        } else {
            u16::from_le_bytes([p.data[at], p.data[at + 1]]) as f32
        }
    };

    let mut out = Vec::new();
    out.try_reserve_exact(w * h * 4)
        .map_err(|_| DecodeError::Corrupt("yuv: rgba allocation failed".into()))?;

    match matrix {
        Matrix::Identity => {
            // GBR plane order per CICP identity: Y-slot=G, U-slot=B, V-slot=R.
            let (u, v) = (u.unwrap(), v.unwrap());
            let max = ((1u32 << bpc) - 1) as f32;
            for r in 0..h {
                for x in 0..w {
                    // Identity content is overwhelmingly full-range; apply the
                    // luma normalization to all three when limited.
                    let norm = |val: f32| -> u8 {
                        let f = if full_range {
                            val / max
                        } else {
                            (val - y_off) / y_scale
                        };
                        (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
                    };
                    let g = norm(sample(&y, x, r));
                    let b = norm(sample(&u, x, r));
                    let rr = norm(sample(&v, x, r));
                    out.extend_from_slice(&[rr, g, b, 255]);
                }
            }
        }
        m => {
            let (kr, kb) = m.kr_kb();
            let kg = 1.0 - kr - kb;
            let rc = 2.0 * (1.0 - kr);
            let bc = 2.0 * (1.0 - kb);
            for r in 0..h {
                let cr_row = r >> ssy;
                for x in 0..w {
                    let yy = ((sample(&y, x, r) - y_off) / y_scale).clamp(0.0, 1.0);
                    let (pb, pr) = match (&u, &v) {
                        (Some(u), Some(v)) => {
                            let cx = x >> ssx;
                            (
                                (sample(u, cx, cr_row) - c_off) / c_scale,
                                (sample(v, cx, cr_row) - c_off) / c_scale,
                            )
                        }
                        _ => (0.0, 0.0), // I400: gray
                    };
                    let red = yy + rc * pr;
                    let blue = yy + bc * pb;
                    let green = (yy - kr * red - kb * blue) / kg;
                    let to8 = |f: f32| (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                    out.extend_from_slice(&[to8(red), to8(green), to8(blue), 255]);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(samples: &[u8]) -> Plane<'_> {
        Plane {
            data: samples,
            stride: samples.len(),
        }
    }

    /// 1×1 helper for known-vector tests.
    fn one(y: &[u8], u: &[u8], v: &[u8], bpc: u32, matrix: Matrix, full: bool) -> [u8; 3] {
        let rgba = to_rgba8(
            plane(y),
            Some(plane(u)),
            Some(plane(v)),
            1,
            1,
            bpc,
            PixelLayout::I444,
            matrix,
            full,
        )
        .expect("convert");
        [rgba[0], rgba[1], rgba[2]]
    }

    fn assert_close(got: [u8; 3], want: [u8; 3], tol: u8) {
        for i in 0..3 {
            assert!(
                got[i].abs_diff(want[i]) <= tol,
                "channel {i}: got {got:?}, want {want:?} ±{tol}"
            );
        }
    }

    #[test]
    fn bt601_limited_primaries() {
        // Classic limited-range BT.601 vectors.
        assert_close(
            one(&[81], &[90], &[240], 8, Matrix::Bt601, false),
            [255, 0, 0],
            3,
        );
        assert_close(
            one(&[16], &[128], &[128], 8, Matrix::Bt601, false),
            [0, 0, 0],
            2,
        );
        assert_close(
            one(&[235], &[128], &[128], 8, Matrix::Bt601, false),
            [255, 255, 255],
            2,
        );
        assert_close(
            one(&[145], &[54], &[34], 8, Matrix::Bt601, false),
            [0, 255, 0],
            4,
        );
        assert_close(
            one(&[41], &[240], &[110], 8, Matrix::Bt601, false),
            [0, 0, 255],
            4,
        );
    }

    #[test]
    fn bt601_full_range_red() {
        // Full-range red: Y = 0.299*255 ≈ 76, Cr = 128 + 0.5*255 ≈ 255.
        assert_close(
            one(&[76], &[85], &[255], 8, Matrix::Bt601, true),
            [255, 0, 0],
            4,
        );
    }

    #[test]
    fn bt709_limited_gray_and_white() {
        assert_close(
            one(&[126], &[128], &[128], 8, Matrix::Bt709, false),
            [128, 128, 128],
            2,
        );
        assert_close(
            one(&[235], &[128], &[128], 8, Matrix::Bt709, false),
            [255, 255, 255],
            2,
        );
        // Red differs from 601 measurably: limited 709 red is Y≈63, Cr≈240.
        assert_close(
            one(&[63], &[102], &[240], 8, Matrix::Bt709, false),
            [255, 0, 0],
            4,
        );
    }

    #[test]
    fn bt2020_limited_white() {
        assert_close(
            one(&[235], &[128], &[128], 8, Matrix::Bt2020, false),
            [255, 255, 255],
            2,
        );
    }

    #[test]
    fn ten_bit_limited_red() {
        // 10-bit limited red = 8-bit vector << 2: Y=324, Cb=360, Cr=960 (LE u16).
        let y = 324u16.to_le_bytes();
        let u = 360u16.to_le_bytes();
        let v = 960u16.to_le_bytes();
        assert_close(one(&y, &u, &v, 10, Matrix::Bt601, false), [255, 0, 0], 3);
    }

    #[test]
    fn twelve_bit_limited_white() {
        let y = (235u16 << 4).to_le_bytes();
        let c = (128u16 << 4).to_le_bytes();
        assert_close(
            one(&y, &c, &c, 12, Matrix::Bt601, false),
            [255, 255, 255],
            2,
        );
    }

    #[test]
    fn i400_is_gray() {
        let rgba = to_rgba8(
            plane(&[126]),
            None,
            None,
            1,
            1,
            8,
            PixelLayout::I400,
            Matrix::Bt601,
            false,
        )
        .expect("convert");
        assert_close([rgba[0], rgba[1], rgba[2]], [128, 128, 128], 2);
    }

    #[test]
    fn identity_full_range_is_exact_gbr() {
        // Identity planes are G, B, R — full range passes bytes through.
        let rgba = to_rgba8(
            plane(&[10]),       // G
            Some(plane(&[20])), // B
            Some(plane(&[30])), // R
            1,
            1,
            8,
            PixelLayout::I444,
            Matrix::Identity,
            true,
        )
        .expect("convert");
        assert_eq!(&rgba[..3], &[30, 10, 20]);
    }

    #[test]
    fn i420_odd_dimensions_upsample_nearest() {
        // 3×3 image, 2×2 chroma (ceil). Solid limited-range red everywhere.
        let y = [81u8; 9];
        let u = [90u8; 4];
        let v = [240u8; 4];
        let rgba = to_rgba8(
            Plane {
                data: &y,
                stride: 3,
            },
            Some(Plane {
                data: &u,
                stride: 2,
            }),
            Some(Plane {
                data: &v,
                stride: 2,
            }),
            3,
            3,
            8,
            PixelLayout::I420,
            Matrix::Bt601,
            false,
        )
        .expect("convert");
        assert_eq!(rgba.len(), 3 * 3 * 4);
        for px in rgba.chunks_exact(4) {
            assert!(
                px[0] > 250 && px[1] < 5 && px[2] < 5 && px[3] == 255,
                "{px:?}"
            );
        }
    }

    #[test]
    fn i422_uses_full_chroma_rows() {
        // 2×2 I422: chroma is 1×2 (half width, full height). Distinct rows
        // must not bleed: row 0 red, row 1 blue.
        let y = [81, 81, 41, 41];
        let u = [90, 240];
        let v = [240, 110];
        let rgba = to_rgba8(
            Plane {
                data: &y,
                stride: 2,
            },
            Some(Plane {
                data: &u,
                stride: 1,
            }),
            Some(Plane {
                data: &v,
                stride: 1,
            }),
            2,
            2,
            8,
            PixelLayout::I422,
            Matrix::Bt601,
            false,
        )
        .expect("convert");
        assert!(rgba[0] > 250 && rgba[2] < 5, "row 0 red: {:?}", &rgba[0..4]);
        assert!(
            rgba[8 + 2] > 250 && rgba[8] < 5,
            "row 1 blue: {:?}",
            &rgba[8..12]
        );
    }

    #[test]
    fn hostile_geometry_is_an_error_not_a_panic() {
        // Plane far too small for the claimed dimensions.
        let r = to_rgba8(
            Plane {
                data: &[0u8; 4],
                stride: 1000,
            },
            Some(plane(&[128])),
            Some(plane(&[128])),
            100,
            100,
            8,
            PixelLayout::I420,
            Matrix::Bt601,
            false,
        );
        assert!(r.is_err());
        // Identity demands 4:4:4.
        let r = to_rgba8(
            plane(&[0]),
            Some(plane(&[0])),
            Some(plane(&[0])),
            1,
            1,
            8,
            PixelLayout::I420,
            Matrix::Identity,
            true,
        );
        assert!(r.is_err());
    }
}
