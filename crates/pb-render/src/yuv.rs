//! CPU planar-YUV conversion (task 79.10 NV12; task #91 Phase 2 P010) — the
//! correctness reference and portability/`PB_VIDEO_CPU_CONVERT` fallback behind
//! [`crate::Renderer::set_video_planar`]. The wgpu shader path (`fs_scene_planar`)
//! must match this within a small tolerance (the golden tests), so the
//! coefficients live in exactly one place per crate boundary. The **golden tests
//! use an independent from-spec reference**, not these helpers, so a bug in the
//! shared math can't hide by matching itself.

/// YUV→RGB matrix coefficients (H.273 families) for planar video frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// Storage precision of a planar 4:2:0 frame the renderer uploads: 8-bit
/// (`R8Unorm`/`Rg8Unorm`) or 10/12-bit high-aligned (`R16Unorm`/`Rg16Unorm`).
/// Decoupled from the transfer function ([`PlanarTransfer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanarFormat {
    /// 8 bits/sample (NV12).
    Nv12,
    /// 16 bits/sample, 10 valid high-aligned (P010LE).
    P010,
}

impl PlanarFormat {
    /// True for the 10/12-bit format ([`P010`](Self::P010)) — the one whose
    /// samples need the high-aligned code recovery and `R16Unorm`/`Rg16Unorm`
    /// textures (gated on `TEXTURE_FORMAT_16BIT_NORM`).
    pub fn is_ten_bit(self) -> bool {
        matches!(self, PlanarFormat::P010)
    }
}

/// The transfer function the renderer inverts to reach scene-linear light for a
/// planar frame (task #91 Phase 2). Decoupled from [`PlanarFormat`]: 10-bit SDR
/// is `P010` + [`SrgbLike`](Self::SrgbLike)/[`Parametric`](Self::Parametric),
/// HDR is `P010` + [`Pq`](Self::Pq)/[`Hlg`](Self::Hlg). The renderer maps this to
/// its shader transfer mode (`ColorUniform` `mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanarTransfer {
    /// sRGB / BT.709-gamma-like — the built-in sRGB EOTF, no primaries matrix
    /// (source primaries assumed sRGB/BT.709).
    SrgbLike,
    /// A source-specific parametric curve in [`crate::ColorTransform::trc`], plus
    /// the primaries matrix.
    Parametric,
    /// SMPTE ST 2084 (PQ), plus the primaries matrix.
    Pq,
    /// Hybrid Log-Gamma (BT.2100), plus the primaries matrix.
    Hlg,
}

impl PlanarTransfer {
    /// True for the HDR transfers ([`Pq`](Self::Pq)/[`Hlg`](Self::Hlg)) — the ones
    /// that expand beyond 1.0 in scene-linear and drive the fp16/EDR present path.
    pub fn is_hdr(self) -> bool {
        matches!(self, PlanarTransfer::Pq | PlanarTransfer::Hlg)
    }
}

/// SDR reference white for the scene-linear normalization (BT.2408): 1.0 in
/// scene-linear scRGB = 203 nits. Matches `pb_decode::ffmpeg::color::SDR_WHITE_NITS`.
pub const SDR_WHITE_NITS: f32 = 203.0;

/// The Y/UV normalization constants `(y_black, y_scale, c_center, c_scale)` for a
/// planar frame, precomputed here so the shader (and the CPU convert) do
/// `yn = (y − y_black)·y_scale` and `cn = (c − c_center)·c_scale` with **no
/// bit-depth or range branch** (task #91 Phase 2 — Codex: normalization constants
/// from Rust, not approximate WGSL literals).
///
/// For P010 (`ten_bit`), an `R16Unorm` sample is `code10 << 6 / 65535`, so the
/// scales carry the `65535/64` factor that recovers the 10-bit code — **not** a
/// `×1023` shortcut (which is off by `64/65535`). For NV12 the sample is
/// `code8/255`.
pub fn planar_range(ten_bit: bool, full_range: bool) -> (f32, f32, f32, f32) {
    if ten_bit {
        const S: f32 = 65535.0 / 64.0; // sample → 10-bit code
        if full_range {
            (0.0, S / 1023.0, 32768.0 / 65535.0, S / 1023.0)
        } else {
            (4096.0 / 65535.0, S / 876.0, 32768.0 / 65535.0, S / 896.0)
        }
    } else if full_range {
        (0.0, 1.0, 128.0 / 255.0, 1.0)
    } else {
        (16.0 / 255.0, 255.0 / 219.0, 128.0 / 255.0, 255.0 / 224.0)
    }
}

/// SMPTE ST 2084 (PQ) EOTF: encoded `[0,1]` → scene-linear scRGB (1.0 = 203
/// nits). The input is clamped to `[0,1]` (PQ code 1.0 → ≈`10000/203` in
/// scene-linear); the caller keeps the encoded clamp and never clamps after.
/// Bit-identical to `pb_decode::ffmpeg::color::pq_to_scrgb` and the WGSL `pq_eotf`.
pub fn pq_eotf(e: f32) -> f32 {
    const M1: f32 = 2610.0 / 16384.0;
    const M2: f32 = 2523.0 / 4096.0 * 128.0;
    const C1: f32 = 3424.0 / 4096.0;
    const C2: f32 = 2413.0 / 4096.0 * 32.0;
    const C3: f32 = 2392.0 / 4096.0 * 32.0;
    let e = e.clamp(0.0, 1.0);
    let ep = e.powf(1.0 / M2);
    let num = (ep - C1).max(0.0);
    let den = C2 - C3 * ep;
    if den <= 0.0 {
        return 0.0;
    }
    let y = (num / den).powf(1.0 / M1); // display luminance / 10000
    y * 10000.0 / SDR_WHITE_NITS
}

/// Hybrid Log-Gamma EOTF (inverse OETF + the nominal 1000-nit OOTF): encoded
/// `[0,1]` → scene-linear scRGB (1.0 = 203 nits). Bit-identical to
/// `pb_decode::ffmpeg::color::hlg_to_scrgb` and the WGSL `hlg_eotf`.
pub fn hlg_eotf(e: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92; // 1 - 4a
    const C: f32 = 0.559_910_7;
    let e = e.clamp(0.0, 1.0);
    let ys = if e <= 0.5 {
        (e * e) / 3.0
    } else {
        (((e - C) / A).exp() + B) / 12.0
    };
    let nits = 1000.0 * ys.max(0.0).powf(1.2);
    nits / SDR_WHITE_NITS
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

/// A CPU-converted planar frame ready for [`crate::Renderer::set_image`]: either
/// straight-alpha RGBA8 (SDR, `hdr == false`) or `Rgba16Float` scene-linear scRGB
/// (HDR, `hdr == true`), with the tone-map `peak`.
pub struct PlanarCpuFrame {
    pub bytes: Vec<u8>,
    pub hdr: bool,
    pub peak: f32,
}

/// CPU planar → scene conversion — the `PB_VIDEO_CPU_CONVERT` A/B hatch and the
/// non-wgpu [`crate::Renderer::set_video_planar`] fallback (task #91 Phase 2). SDR
/// transfers return source-encoded RGBA8 for `set_image(hdr=false)` (the shader
/// then linearizes via `color`); HDR transfers return `Rgba16Float` scene-linear
/// scRGB for `set_image(hdr=true)`. Handles both NV12 (8-bit) and P010 (16-bit LE,
/// high-aligned) planes via the shared [`planar_range`] constants; the wgpu shader
/// must match this.
#[allow(clippy::too_many_arguments)]
pub fn planar_to_scene(
    y: &[u8],
    uv: &[u8],
    width: u32,
    height: u32,
    format: PlanarFormat,
    yuv: YuvParams,
    transfer: PlanarTransfer,
    color: &crate::ColorTransform,
    peak: f32,
) -> PlanarCpuFrame {
    let (w, h) = (width as usize, height as usize);
    let ten = format.is_ten_bit();
    let (y_black, y_scale, c_center, c_scale) = planar_range(ten, yuv.full_range);
    let (a, bq, cq, d) = yuv.matrix.coeffs();
    let m = &color.matrix; // primaries (source-linear → sRGB-linear), row-major

    // Read one normalized sample from a plane at u16/u8 index `i`.
    let read = |plane: &[u8], i: usize| -> f32 {
        if ten {
            u16::from_le_bytes([plane[i * 2], plane[i * 2 + 1]]) as f32 / 65535.0
        } else {
            plane[i] as f32 / 255.0
        }
    };

    let hdr = transfer.is_hdr();
    let mut out: Vec<u8> = if hdr {
        Vec::with_capacity(w * h * 8)
    } else {
        vec![255u8; w * h * 4]
    };

    for row in 0..h {
        let uv_base = (row / 2) * w; // interleaved u16/u8 samples per chroma row
        for col in 0..w {
            let yv = read(y, row * w + col);
            let uidx = uv_base + (col & !1);
            let un = (read(uv, uidx) - c_center) * c_scale;
            let vn = (read(uv, uidx + 1) - c_center) * c_scale;
            let yn = (yv - y_black) * y_scale;
            // YUV → encoded R'G'B', clamped to the encoded domain.
            let er = (yn + a * vn).clamp(0.0, 1.0);
            let eg = (yn - bq * un - cq * vn).clamp(0.0, 1.0);
            let eb = (yn + d * un).clamp(0.0, 1.0);
            if hdr {
                // Per-channel EOTF → source-linear, then primaries matrix → scRGB.
                let eotf = |e: f32| match transfer {
                    PlanarTransfer::Pq => pq_eotf(e),
                    PlanarTransfer::Hlg => hlg_eotf(e),
                    _ => unreachable!("is_hdr() guards this"),
                };
                let (lr, lg, lb) = (eotf(er), eotf(eg), eotf(eb));
                let sr = m[0][0] * lr + m[0][1] * lg + m[0][2] * lb;
                let sg = m[1][0] * lr + m[1][1] * lg + m[1][2] * lb;
                let sb = m[2][0] * lr + m[2][1] * lg + m[2][2] * lb;
                for v in [sr, sg, sb, 1.0] {
                    out.extend_from_slice(&half::f16::from_f32(v).to_le_bytes());
                }
            } else {
                // SDR: source-encoded R'G'B' → RGBA8; set_image linearizes via color.
                let q = |x: f32| (x * 255.0).round() as u8;
                let o = (row * w + col) * 4;
                out[o] = q(er);
                out[o + 1] = q(eg);
                out[o + 2] = q(eb);
            }
        }
    }
    PlanarCpuFrame {
        bytes: out,
        hdr,
        peak: if hdr { peak } else { 1.0 },
    }
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

    /// Task #91 Phase 2: the P010 range constants recover the 10-bit code
    /// endpoints, cross-checked against an **independent** explicit-code formula
    /// (a bug in the `65535/64` high-alignment factor would show here). An
    /// `R16Unorm` sample for 10-bit code `c` is `c·64/65535`.
    #[test]
    fn planar_range_p010_endpoints() {
        let s = |code10: u32| (code10 * 64) as f32 / 65535.0; // stored sample
        // limited: Y 64→0, 940→1; C 64→−0.5, 960→+0.5 (nominal endpoints).
        let (yb, ys, cc, cs) = planar_range(true, false);
        assert!(((s(64) - yb) * ys - 0.0).abs() < 1e-4, "limited Y=64 → 0");
        assert!(((s(940) - yb) * ys - 1.0).abs() < 1e-3, "limited Y=940 → 1");
        assert!(((s(512) - cc) * cs - 0.0).abs() < 1e-4, "limited C=512 → 0");
        // full: Y 0→0, 1023→1; C 512→0.
        let (yb, ys, cc, cs) = planar_range(true, true);
        assert!(((s(0) - yb) * ys).abs() < 1e-4, "full Y=0 → 0");
        assert!(((s(1023) - yb) * ys - 1.0).abs() < 1e-3, "full Y=1023 → 1");
        assert!(((s(512) - cc) * cs).abs() < 1e-4, "full C=512 → 0");
    }

    /// PQ/HLG EOTFs hit their defining reference points (independent constants),
    /// so the shader (which copies these) and the CPU convert agree with the spec.
    #[test]
    fn pq_hlg_reference_points() {
        // PQ: code 0 → 0; the SMPTE-2084 code for 100 nits ≈ 0.508 → 100/203.
        assert!(pq_eotf(0.0).abs() < 1e-4);
        assert!((pq_eotf(1.0) - 10000.0 / 203.0).abs() < 0.5, "PQ 1.0 → 10000 nits");
        assert!((pq_eotf(0.5081) - 100.0 / 203.0).abs() < 0.02, "PQ ~0.508 → 100 nits");
        // HLG: 0 → 0; 1.0 → 1000/203 (the baked 1000-nit OOTF peak).
        assert!(hlg_eotf(0.0).abs() < 1e-4);
        assert!((hlg_eotf(1.0) - 1000.0 / 203.0).abs() < 0.05, "HLG 1.0 → 1000 nits");
    }

    /// A uniform P010 SDR frame round-trips through `planar_to_scene` (the CPU
    /// fallback) to source-encoded RGBA8 that matches the 10-bit code, for full
    /// and limited range.
    #[test]
    fn planar_to_scene_p010_sdr_round_trip() {
        // 2×2 full-range gray (code 512 → 0.5006 → ~128) with neutral chroma (512).
        let store = |c: u32| ((c << 6) as u16).to_le_bytes();
        let mut y = Vec::new();
        for _ in 0..4 {
            y.extend_from_slice(&store(512));
        }
        let mut uv = Vec::new();
        for _ in 0..2 {
            uv.extend_from_slice(&store(512)); // U then V, one shared 2×2 sample
        }
        let f = planar_to_scene(
            &y,
            &uv,
            2,
            2,
            PlanarFormat::P010,
            YuvParams {
                matrix: YuvMatrix::Bt709,
                full_range: true,
            },
            PlanarTransfer::SrgbLike,
            &crate::ColorTransform::srgb(),
            1.0,
        );
        assert!(!f.hdr, "SDR transfer → RGBA8");
        assert_eq!(f.bytes.len(), 2 * 2 * 4);
        // 512/1023 ≈ 0.5006 → 128; neutral chroma keeps it gray.
        for px in f.bytes.chunks_exact(4) {
            assert!((px[0] as i32 - 128).abs() <= 1, "gray R ~128, got {}", px[0]);
            assert_eq!(px[0], px[1]);
            assert_eq!(px[1], px[2]);
        }
    }
}
