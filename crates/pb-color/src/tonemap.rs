//! **HDR → SDR tone mapping that keeps hue** — the one CPU definition the export (`pb-encode`
//! `prepare`) and Copy / OCR / Describe (`pb-app-core` `to_clipboard_rgba8`) share. The
//! renderer's SDR present shader carries the same maths in WGSL (a shader can't import Rust).
//!
//! The previous mapping compressed R, G and B **each on its own curve**. On a bright,
//! saturated colour the large channel is squashed hard while the small ones barely move, so
//! their ratios — the hue — jump: a crimson highlight (P3-linear `(8, 0.5, 1)`) came out
//! `(0.96, 0.33, 0.50)`, and the owner's HDR10 test pattern's BT.2020 red bar (≈ 8,800 nits)
//! exported as `(255, 0, 94)` — pink-magenta (owner report, 2026-11-14). Now:
//!
//! 1. [`into_gamut`] — a colour outside the target gamut (a negative channel: BT.2020 red in
//!    P3 has a slightly negative green) has that channel **clipped to 0**, the usual
//!    relative-colorimetric clip. (Desaturating toward luminance was tried first and is
//!    worse here: the line from BT.2020 red to white leaves P3 through its red–blue edge, so
//!    the red picked up blue again.)
//! 2. [`tone_map`] — the **brightest channel** follows extended Reinhard with white point
//!    `white` (the image's peak), and all three channels scale by the same factor, so hue and
//!    saturation are kept and nothing exceeds 1. With `white == 1` (SDR content) it is the
//!    identity.

/// Extended Reinhard with white point `white` (≥ 1): `white` maps to 1, small values pass
/// nearly unchanged.
pub fn reinhard_extended(x: f32, white: f32) -> f32 {
    let x = x.max(0.0);
    x * (1.0 + x / (white * white)) / (1.0 + x)
}

/// Bring a linear RGB colour inside the target gamut by clipping negative channels to 0.
/// Non-finite input becomes black, never NaN.
pub fn into_gamut(rgb: [f32; 3]) -> [f32; 3] {
    if !rgb.iter().all(|c| c.is_finite()) {
        return [0.0; 3];
    }
    rgb.map(|c| c.max(0.0))
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
pub fn hdr_to_sdr(rgb: [f32; 3], white: f32) -> [f32; 3] {
    tone_map(into_gamut(rgb), white)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratios(c: [f32; 3]) -> (f32, f32) {
        (c[1] / c[0], c[2] / c[0])
    }

    /// A bright crimson highlight must stay crimson, not turn magenta. Per-channel Reinhard
    /// gave (0.96, 0.33, 0.50) — G/R and B/R jumped 5×.
    #[test]
    fn a_bright_saturated_red_keeps_its_hue() {
        let crimson = [8.0, 0.5, 1.0];
        let out = hdr_to_sdr(crimson, 10.0);
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

    /// The owner's HDR10 test pattern: its red bar is BT.2020 red at ≈ 43.6 × SDR white
    /// (≈ 8,800 nits), P3-linear (58.6, −2.83, 0.13), with the frame's peak ≈ 58.6. It must
    /// export as red — the per-channel curve gave (255, 0, 94); desaturating toward
    /// luminance would still have added blue (≈ 62/255).
    #[test]
    fn the_test_patterns_bt2020_red_bar_stays_red() {
        let red_bar_in_p3 = [58.6, -2.83, 0.13];
        let out = hdr_to_sdr(red_bar_in_p3, 58.6);
        assert!(out[0] > 0.99 && out[1] == 0.0, "{out:?}");
        let blue_8bit = 255.0 * (1.055 * out[2].powf(1.0 / 2.4) - 0.055);
        assert!(blue_8bit < 12.0, "no visible blue: {blue_8bit} (was 94)");
    }

    #[test]
    fn sdr_content_passes_through_unchanged() {
        for c in [
            [0.2, 0.5, 0.9],
            [1.0, 1.0, 1.0],
            [0.0, 0.0, 0.0],
            [0.7, 0.0, 0.3],
        ] {
            let out = hdr_to_sdr(c, 1.0);
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

    #[test]
    fn out_of_gamut_channels_clip_and_garbage_is_black() {
        assert_eq!(into_gamut([1.344, -0.065, 0.003]), [1.344, 0.0, 0.003]);
        assert_eq!(into_gamut([0.1, 0.2, 0.3]), [0.1, 0.2, 0.3]);
        assert_eq!(into_gamut([f32::NAN, 0.0, 0.0]), [0.0; 3]);
        assert_eq!(tone_map([f32::INFINITY, 0.0, 0.0], 4.0), [0.0; 3]);
    }
}
