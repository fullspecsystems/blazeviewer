//! **HDR → SDR tone mapping that keeps hue** — the one CPU definition the export (`pb-encode`
//! `prepare`) and Copy / OCR / Describe (`pb-app-core` `to_clipboard_rgba8`) share. The
//! renderer's SDR present shader carries the same maths in WGSL (a shader can't import Rust).
//!
//! The previous mapping compressed R, G and B **each on its own curve**. On a bright,
//! saturated colour the large channel is squashed hard while the small ones barely move, so
//! their ratios — the hue — jump: a crimson highlight (P3-linear `(8, 0.5, 1)`) came out
//! `(0.96, 0.33, 0.50)`, pink-magenta (owner report, 2026-11-14: "HDR red looks kind of
//! magenta" in an exported video frame). Now:
//!
//! 1. [`into_gamut`] — a colour outside the target gamut (a negative channel: BT.2020 red in
//!    P3) is **desaturated toward its own luminance** just enough to bring that channel to 0,
//!    instead of clipping it (which also shifts hue);
//! 2. [`tone_map`] — the **brightest channel** follows extended Reinhard with white point
//!    `white` (the image's peak), and all three channels scale by the same factor, so hue and
//!    saturation are kept and nothing exceeds 1. With `white == 1` (SDR content) it is the
//!    identity.

/// BT.709 / sRGB luminance weights.
pub const LUMA_BT709: [f32; 3] = [0.212_639, 0.715_169, 0.072_192];
/// Display P3 (D65) luminance weights — the Y row of its RGB→XYZ matrix.
pub const LUMA_DISPLAY_P3: [f32; 3] = [0.228_975, 0.691_739, 0.079_287];

/// Extended Reinhard with white point `white` (≥ 1): `white` maps to 1, small values pass
/// nearly unchanged.
pub fn reinhard_extended(x: f32, white: f32) -> f32 {
    let x = x.max(0.0);
    x * (1.0 + x / (white * white)) / (1.0 + x)
}

/// Bring a linear RGB colour inside the gamut (no negative channel) by moving it toward its
/// own luminance `Y = luma · rgb` until the smallest channel reaches 0. In-gamut colours are
/// returned unchanged; non-positive luminance or non-finite input becomes black.
pub fn into_gamut(rgb: [f32; 3], luma: [f32; 3]) -> [f32; 3] {
    if !rgb.iter().all(|c| c.is_finite()) {
        return [0.0; 3];
    }
    let min = rgb[0].min(rgb[1]).min(rgb[2]);
    if min >= 0.0 {
        return rgb;
    }
    let y = luma[0] * rgb[0] + luma[1] * rgb[1] + luma[2] * rgb[2];
    if y <= 0.0 {
        return [0.0; 3];
    }
    // y + t·(c − y) hits 0 at the smallest channel.
    let t = y / (y - min);
    rgb.map(|c| (y + t * (c - y)).max(0.0))
}

/// Tone-map a non-negative linear RGB colour into [0, 1] keeping its hue: the brightest
/// channel follows [`reinhard_extended`] and the others scale with it.
pub fn tone_map(rgb: [f32; 3], white: f32) -> [f32; 3] {
    let m = rgb[0].max(rgb[1]).max(rgb[2]);
    if !m.is_finite() || m <= 0.0 {
        return [0.0; 3];
    }
    let scale = reinhard_extended(m, white.max(1.0)) / m;
    rgb.map(|c| (c.max(0.0) * scale).min(1.0))
}

/// [`into_gamut`] then [`tone_map`]: HDR linear RGB (in the target's primaries) → SDR linear
/// RGB in [0, 1], ready for the transfer curve.
pub fn hdr_to_sdr(rgb: [f32; 3], white: f32, luma: [f32; 3]) -> [f32; 3] {
    tone_map(into_gamut(rgb, luma), white)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratios(c: [f32; 3]) -> (f32, f32) {
        (c[1] / c[0], c[2] / c[0])
    }

    /// The owner's report: a bright crimson highlight must stay crimson, not turn magenta.
    /// Per-channel Reinhard gave (0.96, 0.33, 0.50) — G/R and B/R jumped 5×.
    #[test]
    fn a_bright_saturated_red_keeps_its_hue() {
        let crimson = [8.0, 0.5, 1.0];
        let out = hdr_to_sdr(crimson, 10.0, LUMA_DISPLAY_P3);
        let (g, b) = ratios(out);
        assert!(
            (g - 0.0625).abs() < 1e-4 && (b - 0.125).abs() < 1e-4,
            "{out:?}"
        );
        assert!(out.iter().all(|c| (0.0..=1.0).contains(c)));
        // The old per-channel result, for the record: B/R ≈ 0.53 — magenta.
        let old = crimson.map(|c| reinhard_extended(c, 10.0));
        assert!(ratios(old).1 > 0.5, "{old:?}");
    }

    #[test]
    fn sdr_content_passes_through_unchanged() {
        for c in [
            [0.2, 0.5, 0.9],
            [1.0, 1.0, 1.0],
            [0.0, 0.0, 0.0],
            [0.7, 0.0, 0.3],
        ] {
            let out = hdr_to_sdr(c, 1.0, LUMA_BT709);
            for (a, b) in out.iter().zip(c) {
                assert!((a - b).abs() < 1e-6, "{c:?} → {out:?}");
            }
        }
    }

    #[test]
    fn the_peak_maps_to_white_and_greys_stay_grey() {
        let out = tone_map([10.0, 10.0, 10.0], 10.0);
        assert!(out.iter().all(|c| (c - 1.0).abs() < 1e-6), "{out:?}");
        let g = tone_map([2.0, 2.0, 2.0], 10.0);
        assert!(
            g[0] == g[1] && g[1] == g[2] && g[0] < 1.0 && g[0] > 0.6,
            "{g:?}"
        );
    }

    /// BT.2020 red in P3 has a slightly negative green: it is desaturated toward its own
    /// luminance (still red, hue kept), not clipped.
    #[test]
    fn out_of_gamut_colours_desaturate_toward_their_luminance() {
        let red_2020_in_p3 = [1.344, -0.065, 0.003];
        let out = into_gamut(red_2020_in_p3, LUMA_DISPLAY_P3);
        assert!(out.iter().all(|c| *c >= 0.0), "{out:?}");
        assert!(
            out[1].abs() < 1e-6,
            "the negative channel lands on 0: {out:?}"
        );
        let y = |c: [f32; 3]| {
            c.iter()
                .zip(LUMA_DISPLAY_P3)
                .map(|(a, w)| a * w)
                .sum::<f32>()
        };
        assert!((y(out) - y(red_2020_in_p3)).abs() < 1e-5, "luminance kept");
        assert!(
            out[0] > 1.0 && out[2] > 0.003,
            "red stays dominant, nudged toward grey"
        );
        // In-gamut colours are untouched; garbage becomes black, never NaN.
        assert_eq!(into_gamut([0.1, 0.2, 0.3], LUMA_BT709), [0.1, 0.2, 0.3]);
        assert_eq!(into_gamut([f32::NAN, 0.0, 0.0], LUMA_BT709), [0.0; 3]);
        assert_eq!(tone_map([f32::INFINITY, 0.0, 0.0], 4.0), [0.0; 3]);
    }
}
