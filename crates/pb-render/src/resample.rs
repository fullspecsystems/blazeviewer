//! Separable Lanczos resampling coefficients (task #110).
//!
//! The GPU derives an exact-size, high-quality **Fit** from the retained full-res (mipped) Original
//! by a two-pass separable resample. The per-axis tap coefficients are computed **once per derive on
//! the CPU** here — never per fragment — then uploaded for the shader to read (`starts[d]` + the
//! `taps` normalized `weights` for output pixel `d`). This module is pure math: it holds no GPU
//! state, so it is exactly unit-testable (Codex review of the #110 plan: keep a pure-CPU coefficient
//! test; GPU float/`sin`/fp16 readback is not deterministic across adapters).
//!
//! **Scale-aware widening (the correctness point Codex flagged):** an antialiased *minification*
//! widens the kernel's source-space support by the residual scale `s = src/dst`. So a tap at source
//! distance `d` from the sample center has weight `lanczos((i − center) / s, a)` — i.e. the kernel is
//! stretched by `s` — and the support radius is `a·s` source pixels (≈ `2·a·s + 1` taps), not `a`.
//! At residual 2× that's ≈ 12–13 taps/axis for Lanczos-3, not 7. Upscaling (`s < 1`) never widens
//! (clamped to 1), matching decode-to-fit's no-upscale rule; magnification quality is out of scope.
//!
//! Edge policy is **clamp-to-edge**: taps whose source index falls outside `[0, src)` are clamped by
//! the shader to the border texel; weights are normalized over the full (pre-clamp) tap set so total
//! weight stays 1 and brightness is preserved.

/// A separable resampling kernel for one axis. For each of `dst` output samples there is a
/// contiguous run of `taps` source taps beginning at `starts[d]` (which may be negative or run past
/// `src` — the shader clamps to the edge), weighted by `weights[d*taps .. d*taps + taps]`.
#[derive(Debug, Clone, PartialEq)]
pub struct AxisKernel {
    /// Number of output samples along this axis.
    pub dst: u32,
    /// Fixed tap count per output sample (covers the widest sub-pixel phase; off-support taps are 0).
    pub taps: u32,
    /// Source index of the first tap for each output sample (`len == dst`). May be `< 0` or `>= src`.
    pub starts: Vec<i32>,
    /// Normalized weights, `dst`-major: `weights[d as usize * taps as usize + t]` (`len == dst*taps`).
    pub weights: Vec<f32>,
}

/// The Lanczos-`a` window function `L(x) = sinc(x)·sinc(x/a)` for `|x| < a`, else `0`.
/// `a` is the lobe count (2 or 3 for the #110 A/B).
fn lanczos(x: f32, a: f32) -> f32 {
    if x == 0.0 {
        return 1.0;
    }
    if x.abs() >= a {
        return 0.0;
    }
    let px = std::f32::consts::PI * x;
    // sinc(x)·sinc(x/a) = a·sin(πx)·sin(πx/a) / (π²x²)
    a * px.sin() * (px / a).sin() / (px * px)
}

/// Build the per-output-pixel Lanczos taps for resampling `src` samples to `dst` along one axis.
/// `a` is the Lanczos lobe count (2 or 3). Panics only on a zero axis (`src == 0 || dst == 0`),
/// which the caller must not produce.
pub fn lanczos_axis_kernel(src: u32, dst: u32, a: u32) -> AxisKernel {
    assert!(src > 0 && dst > 0, "resample axis must be non-empty");
    let a = a as f32;
    // Residual minification (>= 1; never widen for upscale — decode-to-fit doesn't upscale).
    let ratio = src as f32 / dst as f32;
    let s = ratio.max(1.0);
    let support = a * s; // kernel radius in SOURCE pixels
    // Fixed tap count covering the widest phase; boundary taps land at |x| = a where L = 0.
    let taps = (2.0 * support).ceil() as u32 + 1;
    let mut starts = Vec::with_capacity(dst as usize);
    let mut weights = vec![0.0f32; dst as usize * taps as usize];
    for d in 0..dst {
        // Center-of-pixel mapping from destination to source (the correct phase, per Codex).
        let center = (d as f32 + 0.5) * ratio - 0.5;
        let first = (center - support).ceil() as i32;
        starts.push(first);
        let row = d as usize * taps as usize;
        let mut sum = 0.0f32;
        for t in 0..taps as usize {
            let i = first + t as i32;
            // Scaled distance: dividing by `s` is the antialias widening of the kernel.
            let x = (i as f32 - center) / s;
            let w = lanczos(x, a);
            weights[row + t] = w;
            sum += w;
        }
        // Normalize so the taps sum to 1 (preserves brightness at any phase / at the clamped edges).
        if sum != 0.0 {
            let inv = 1.0 / sum;
            for t in 0..taps as usize {
                weights[row + t] *= inv;
            }
        }
    }
    AxisKernel {
        dst,
        taps,
        starts,
        weights,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The weights for every output pixel sum to 1 (brightness preserved), for interior AND edge
    /// pixels (where taps clamp off the border — normalization over the full tap set handles it).
    #[test]
    fn weights_normalize_to_one_everywhere() {
        for &(src, dst, a) in &[(6000u32, 1440u32, 3u32), (7360, 1440, 2), (100, 37, 3), (8, 4, 3)] {
            let k = lanczos_axis_kernel(src, dst, a);
            let taps = k.taps as usize;
            for d in 0..k.dst as usize {
                let sum: f32 = k.weights[d * taps..(d + 1) * taps].iter().sum();
                assert!(
                    (sum - 1.0).abs() < 1e-4,
                    "pixel {d} of {src}->{dst} (a={a}) weights sum {sum}, want 1"
                );
            }
        }
    }

    /// At 1:1 the resample is the identity: each output pixel's kernel is a unit impulse at the
    /// aligned source index (integer phase → Lanczos zeros at every non-zero integer offset).
    #[test]
    fn identity_at_one_to_one() {
        let k = lanczos_axis_kernel(64, 64, 3);
        let taps = k.taps as usize;
        for d in 0..k.dst as usize {
            let start = k.starts[d];
            for t in 0..taps {
                let src_i = start + t as i32;
                let w = k.weights[d * taps + t];
                if src_i == d as i32 {
                    assert!((w - 1.0).abs() < 1e-4, "identity: pixel {d} tap on itself w={w}");
                } else {
                    assert!(w.abs() < 1e-4, "identity: pixel {d} off-tap {src_i} w={w} (want 0)");
                }
            }
        }
    }

    /// Support (and thus tap count) widens with the downscale ratio — the scale-aware correction.
    /// Codex: Lanczos-3 needs ~12–13 taps at residual 2×, not 7.
    #[test]
    fn support_widens_with_downscale() {
        let t1 = lanczos_axis_kernel(64, 64, 3).taps; // 1x
        let t2 = lanczos_axis_kernel(128, 64, 3).taps; // 2x
        let t4 = lanczos_axis_kernel(256, 64, 3).taps; // 4x
        assert!(t1 < t2 && t2 < t4, "taps must grow with ratio: {t1} < {t2} < {t4}");
        assert_eq!(t1, 7, "Lanczos-3 at 1x is a·2+1 = 7 taps");
        assert!((12..=13).contains(&t2), "Lanczos-3 at 2x is ~12-13 taps, got {t2}");
    }

    /// The kernel is correctly centered: its weighted-mean source index (centroid / first moment)
    /// equals the sample center, so there's no sub-pixel phase bias (which would shift the image).
    /// Interior pixels only — edge pixels clamp taps off the border, which shifts the centroid toward
    /// the edge by design (clamp-to-edge).
    #[test]
    fn kernel_centroid_matches_the_sample_center() {
        for &(src, dst, a) in &[(6000u32, 1440u32, 3u32), (128, 64, 3), (100, 37, 2)] {
            let k = lanczos_axis_kernel(src, dst, a);
            let taps = k.taps as usize;
            let ratio = src as f32 / dst as f32;
            for d in taps..(k.dst as usize).saturating_sub(taps) {
                let center = (d as f32 + 0.5) * ratio - 0.5;
                let row = &k.weights[d * taps..(d + 1) * taps];
                // Centroid as an offset from `start` (weights sum to 1) — keeps the accumulation at
                // small magnitude so it doesn't inherit f32 error from large source indices.
                let offset: f32 = row.iter().enumerate().map(|(t, &w)| w * t as f32).sum();
                let centroid = k.starts[d] as f32 + offset;
                assert!(
                    (centroid - center).abs() < 2e-3,
                    "pixel {d} of {src}->{dst}: centroid {centroid} != center {center}"
                );
            }
        }
    }

    /// Weights are finite and the kernel is center-weighted (the dominant tap is nearest the sample
    /// center) — no NaN, no negative blow-up from the sinc lobes.
    #[test]
    fn weights_are_finite_and_center_weighted() {
        let k = lanczos_axis_kernel(6000, 1440, 3);
        let taps = k.taps as usize;
        for d in 0..k.dst as usize {
            let center = (d as f32 + 0.5) * (6000.0 / 1440.0) - 0.5;
            let row = &k.weights[d * taps..(d + 1) * taps];
            assert!(row.iter().all(|w| w.is_finite()), "pixel {d} has a non-finite weight");
            let (mut best_t, mut best_w) = (0usize, f32::NEG_INFINITY);
            for (t, &w) in row.iter().enumerate() {
                if w > best_w {
                    best_w = w;
                    best_t = t;
                }
            }
            let best_src = (k.starts[d] + best_t as i32) as f32;
            assert!(
                (best_src - center).abs() <= 1.0,
                "pixel {d}: heaviest tap {best_src} not adjacent to center {center}"
            );
        }
    }

    /// Lanczos-2 uses fewer taps than Lanczos-3 at the same ratio (the cheaper A/B candidate).
    #[test]
    fn lanczos2_is_narrower_than_lanczos3() {
        let a2 = lanczos_axis_kernel(128, 64, 2).taps;
        let a3 = lanczos_axis_kernel(128, 64, 3).taps;
        assert!(a2 < a3, "L2 ({a2} taps) should be narrower than L3 ({a3} taps)");
    }
}
