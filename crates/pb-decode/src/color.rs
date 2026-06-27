//! ICC color management, reduced to a **shader-ready transform**.
//!
//! Per `CLAUDE.md` ("Color … 3×3 matrix + TRC in-shader") the conversion runs on
//! the GPU, so this module's whole job is to turn a source ICC profile into the
//! handful of numbers the fragment shader needs:
//!
//! 1. a **3×3 matrix** mapping source-*linear* RGB → sRGB-*linear* RGB (source
//!    primaries → sRGB primaries, white-adapted in the D50 PCS), and
//! 2. a **transfer curve** (the source EOTF, encoded→linear), expressed in
//!    moxcms's unified 7-parameter parametric form so the shader can evaluate it
//!    exactly the way [`moxcms::ParametricCurve::eval`] does.
//!
//! The shader then linearizes the sampled texel, applies the matrix, and
//! re-encodes to sRGB (the surface is non-sRGB, so the monitor — assumed sRGB for
//! v1 — receives sRGB-encoded bytes). When the source is already sRGB (or unknown)
//! the transform is **disabled**: the renderer does a straight passthrough, so the
//! overwhelmingly common case is bit-exact and free.
//!
//! Scope (v1): RGB **matrix** profiles with a parametric/gamma TRC — which covers
//! the cases that actually matter in a photo library (Display-P3 HEIC, Adobe RGB /
//! ProPhoto exports, sRGB). LUT/CLUT and CMYK profiles fall back to passthrough
//! (the documented `lcms2`-behind-a-flag escalation handles those later).

use moxcms::{
    CicpColorPrimaries, CicpProfile, ColorProfile, DataColorSpace, MatrixCoefficients,
    ParametricCurve, ToneReprCurve, TransferCharacteristics,
};

/// A source→sRGB color conversion, pre-reduced to shader inputs.
///
/// `matrix` is row-major: `out[i] = Σ_j matrix[i][j] * in[j]` for linear RGB.
/// `trc` is `(g, a, b, c, d, e, f)`: `eval(x) = if x < d { c*x + f } else
/// { (a*x + b)^g + e }` (encoded→linear). `enabled == false` means "source is
/// sRGB or unknown — render the texel unchanged".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorTransform {
    pub matrix: [[f32; 3]; 3],
    pub trc: [f32; 7],
    pub enabled: bool,
}

const IDENTITY: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
/// `eval(x) = x` (g=1, a=1, rest 0): a linear/no-op transfer curve.
const LINEAR_TRC: [f32; 7] = [1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0];

/// Canonical sRGB EOTF (IEC 61966-2.1) in the 7-param form, used to detect when a
/// parsed profile is "really sRGB" so the conversion can be skipped.
const SRGB_TRC: [f32; 7] = [
    2.4,
    1.0 / 1.055,
    0.055 / 1.055,
    1.0 / 12.92,
    0.04045,
    0.0,
    0.0,
];

impl ColorTransform {
    /// The identity transform: source is sRGB (or unknown) → renderer passthrough.
    pub const fn srgb() -> Self {
        Self {
            matrix: IDENTITY,
            trc: LINEAR_TRC,
            enabled: false,
        }
    }

    /// Build from raw ICC profile bytes. Any parse failure (or an unsupported
    /// profile class) yields the sRGB passthrough — never an error, since wrong
    /// colors are a far smaller sin than refusing to show the photo.
    pub fn from_icc(icc: &[u8]) -> Self {
        match ColorProfile::new_from_slice(icc) {
            Ok(profile) => Self::from_profile(&profile),
            Err(_) => Self::srgb(),
        }
    }

    /// Map a WIC `IWICColorContext` "EXIF color space" code to a transform: 1 =
    /// sRGB (passthrough), 2 = Adobe RGB, anything else (uncalibrated/unknown) =
    /// passthrough. WIC hands these back for HEIF/JPEG frames that carry a color
    /// space tag instead of a full embedded profile.
    pub fn from_exif_color_space(code: u32) -> Self {
        match code {
            2 => Self::from_profile(&ColorProfile::new_adobe_rgb()),
            _ => Self::srgb(),
        }
    }

    /// Build from CICP code points (an ISOBMFF `nclx` colour box: HEIF/AVIF). The
    /// common iPhone case is `primaries=12` (Display-P3 / SMPTE-432) with
    /// `transfer=13` (sRGB). Unsupported/unspecified codes → sRGB passthrough.
    pub fn from_cicp(primaries: u8, transfer: u8, matrix: u8, full_range: bool) -> Self {
        let (Ok(color_primaries), Ok(transfer_characteristics), Ok(matrix_coefficients)) = (
            CicpColorPrimaries::try_from(primaries),
            TransferCharacteristics::try_from(transfer),
            MatrixCoefficients::try_from(matrix),
        ) else {
            return Self::srgb();
        };
        let cicp = CicpProfile {
            color_primaries,
            transfer_characteristics,
            matrix_coefficients,
            full_range,
        };
        let mut profile = ColorProfile::new_srgb();
        profile.update_rgb_colorimetry_from_cicp(cicp);
        Self::from_profile(&profile)
    }

    /// Reduce a parsed [`ColorProfile`] to the shader transform. Non-RGB or
    /// non-matrix profiles (no usable colorants) fall back to passthrough.
    pub fn from_profile(profile: &ColorProfile) -> Self {
        if profile.color_space != DataColorSpace::Rgb {
            return Self::srgb();
        }
        // A matrix profile must have RGB colorants; if their matrix is singular
        // it's a LUT-only profile we don't handle in-shader (v1) → passthrough.
        let src_to_xyz = profile.rgb_to_xyz_matrix();
        if src_to_xyz.determinant().map(f64::abs).unwrap_or(0.0) < 1e-9 {
            return Self::srgb();
        }
        let srgb = ColorProfile::new_srgb();
        let m = profile.transform_matrix(&srgb).to_f32();
        let matrix = [
            [m.v[0][0], m.v[0][1], m.v[0][2]],
            [m.v[1][0], m.v[1][1], m.v[1][2]],
            [m.v[2][0], m.v[2][1], m.v[2][2]],
        ];

        // Representative TRC (R≈G≈B for display RGB profiles). No curve at all
        // means an unusual profile we can't safely linearize → passthrough.
        let curve = profile
            .red_trc
            .as_ref()
            .or(profile.green_trc.as_ref())
            .or(profile.blue_trc.as_ref());
        let Some(curve) = curve else {
            return Self::srgb();
        };
        let trc = trc_from_curve(curve);

        // Skip the work when the primaries are sRGB and the curve is sRGB-like:
        // the conversion would be a near no-op, and ordinary sRGB JPEGs (whose
        // profiles often store the curve as a LUT we fit to ~2.2, not as the exact
        // sRGB parametric) then stay bit-exact and free.
        let enabled = !(is_identity(&matrix) && trc_is_srgb_like(&trc));
        Self {
            matrix,
            trc,
            enabled,
        }
    }
}

impl Default for ColorTransform {
    fn default() -> Self {
        Self::srgb()
    }
}

/// Convert a moxcms tone curve to the unified 7-param form the shader evaluates.
/// Parametric curves map directly; a `curveType` is stored as a `Lut`: empty =
/// linear, a single u16 = a pure gamma in u8.8 fixed point (the ICC special case),
/// and a longer table is a sampled curve we approximate with a fitted gamma (the
/// rare camera-LUT case — good enough for v1's matrix+parametric scope).
fn trc_from_curve(curve: &ToneReprCurve) -> [f32; 7] {
    match curve {
        ToneReprCurve::Parametric(params) => match ParametricCurve::new(params) {
            Some(p) => [p.g, p.a, p.b, p.c, p.d, p.e, p.f],
            None => SRGB_TRC,
        },
        ToneReprCurve::Lut(table) => match table.as_slice() {
            [] => LINEAR_TRC,
            [gamma] => gamma_trc(*gamma as f32 / 256.0),
            samples => gamma_trc(fit_gamma(samples)),
        },
    }
}

/// A pure-gamma transfer curve `y = x^g` in the 7-param form.
fn gamma_trc(g: f32) -> [f32; 7] {
    [g, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0]
}

/// Estimate an effective gamma for a sampled (`u16`, encoded→linear) tone curve by
/// averaging `ln(y)/ln(x)` over the interior samples. A coarse but stable fit for
/// the uncommon profiles that store their TRC as a table rather than parametrically.
fn fit_gamma(samples: &[u16]) -> f32 {
    let n = samples.len();
    let mut sum = 0.0f64;
    let mut count = 0u32;
    for (i, &s) in samples.iter().enumerate() {
        let x = i as f64 / (n - 1) as f64;
        let y = s as f64 / 65535.0;
        if x > 1e-3 && x < 1.0 - 1e-3 && y > 1e-6 {
            sum += y.ln() / x.ln();
            count += 1;
        }
    }
    if count == 0 {
        2.2
    } else {
        (sum / count as f64) as f32
    }
}

fn is_identity(m: &[[f32; 3]; 3]) -> bool {
    m.iter().enumerate().all(|(i, row)| {
        row.iter().enumerate().all(|(j, &v)| {
            let want = if i == j { 1.0 } else { 0.0 };
            (v - want).abs() <= 1e-3
        })
    })
}

fn is_srgb_trc(trc: &[f32; 7]) -> bool {
    trc.iter()
        .zip(SRGB_TRC.iter())
        .all(|(a, b)| (a - b).abs() < 1e-3)
}

/// True if the TRC is the exact sRGB curve or a plain ~gamma-2.2 power curve. Both
/// are visually sRGB, so when the primaries are also sRGB the whole conversion can
/// be skipped. Crucially this catches sRGB profiles that store their curve as a
/// sampled LUT (fit to ~2.17), which the exact [`is_srgb_trc`] check would miss. A
/// genuinely linear curve (g≈1) is *not* sRGB-like, so it still gets converted.
fn trc_is_srgb_like(trc: &[f32; 7]) -> bool {
    let plain_gamma_2_2 = (trc[0] - 2.2).abs() <= 0.3 // gamma in ~[1.9, 2.5]
        && (trc[1] - 1.0).abs() < 1e-3 // a = 1
        && trc[2].abs() < 1e-3 // b = 0
        && trc[3].abs() < 1e-3; // c = 0
    is_srgb_trc(trc) || plain_gamma_2_2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each matrix row should sum to ~1: D65 source white must map to sRGB white
    /// (the white-adaptation invariant of a correct primaries conversion).
    fn rows_preserve_white(m: &[[f32; 3]; 3]) -> bool {
        (0..3).all(|i| ((m[i][0] + m[i][1] + m[i][2]) - 1.0).abs() < 1e-2)
    }

    #[test]
    fn srgb_profile_is_passthrough() {
        let t = ColorTransform::from_profile(&ColorProfile::new_srgb());
        assert!(!t.enabled, "sRGB should disable conversion");
    }

    #[test]
    fn default_and_srgb_are_passthrough() {
        assert!(!ColorTransform::default().enabled);
        assert!(!ColorTransform::srgb().enabled);
    }

    #[test]
    fn display_p3_enables_with_srgb_curve_and_white_preserved() {
        let t = ColorTransform::from_profile(&ColorProfile::new_display_p3());
        assert!(t.enabled, "Display-P3 must convert");
        // P3 shares the sRGB transfer function, only the primaries differ.
        assert!(
            is_srgb_trc(&t.trc),
            "P3 TRC should be sRGB, got {:?}",
            t.trc
        );
        assert!(!is_identity(&t.matrix), "P3 primaries are not sRGB");
        assert!(
            rows_preserve_white(&t.matrix),
            "white not preserved: {:?}",
            t.matrix
        );
        // P3's red is wider than sRGB's, so converting P3 red toward sRGB pulls the
        // green/blue components negative (out of the sRGB gamut).
        assert!(
            t.matrix[1][0] < 0.0 && t.matrix[2][0] < 0.0,
            "matrix {:?}",
            t.matrix
        );
    }

    #[test]
    fn adobe_rgb_has_gamma_curve() {
        let t = ColorTransform::from_profile(&ColorProfile::new_adobe_rgb());
        assert!(t.enabled, "Adobe RGB must convert");
        // Adobe RGB uses a ~2.2 pure gamma (a=1, b=0), not the sRGB piecewise curve.
        assert!(t.trc[0] > 2.0 && t.trc[0] < 2.4, "gamma {:?}", t.trc);
        assert!(
            (t.trc[1] - 1.0).abs() < 1e-3 && t.trc[2].abs() < 1e-3,
            "pure gamma {:?}",
            t.trc
        );
        assert!(
            rows_preserve_white(&t.matrix),
            "white not preserved: {:?}",
            t.matrix
        );
    }

    #[test]
    fn garbage_icc_is_passthrough() {
        assert!(!ColorTransform::from_icc(&[0, 1, 2, 3, 4]).enabled);
        assert!(!ColorTransform::from_icc(&[]).enabled);
    }

    #[test]
    fn exif_color_space_maps_adobe_and_srgb() {
        assert!(!ColorTransform::from_exif_color_space(1).enabled); // sRGB
        assert!(!ColorTransform::from_exif_color_space(0xFFFF).enabled); // uncalibrated
        assert!(ColorTransform::from_exif_color_space(2).enabled); // Adobe RGB
    }

    #[test]
    fn cicp_display_p3_matches_the_p3_profile() {
        // nclx Display-P3: primaries=12 (SMPTE-432), transfer=13 (sRGB), matrix=0.
        let t = ColorTransform::from_cicp(12, 13, 0, true);
        assert!(t.enabled, "P3 CICP must convert");
        assert!(is_srgb_trc(&t.trc), "P3 transfer is sRGB, got {:?}", t.trc);
        // Same gamut as the Display-P3 ICC: red's green/blue components go negative.
        assert!(
            t.matrix[1][0] < 0.0 && t.matrix[2][0] < 0.0,
            "matrix {:?}",
            t.matrix
        );
    }

    #[test]
    fn cicp_bt709_srgb_is_passthrough() {
        // primaries=1, transfer=13 → plain sRGB → disabled.
        assert!(!ColorTransform::from_cicp(1, 13, 0, true).enabled);
    }

    #[test]
    fn cicp_unspecified_is_passthrough() {
        assert!(!ColorTransform::from_cicp(2, 2, 2, true).enabled);
    }

    #[test]
    fn empty_lut_is_linear() {
        assert_eq!(trc_from_curve(&ToneReprCurve::Lut(vec![])), LINEAR_TRC);
    }

    #[test]
    fn single_entry_lut_is_pure_gamma() {
        // u8.8 fixed point: 2.2 * 256 = 563.2 → 563.
        let trc = trc_from_curve(&ToneReprCurve::Lut(vec![563]));
        assert!((trc[0] - 563.0 / 256.0).abs() < 1e-4, "{:?}", trc);
        assert_eq!(trc[1], 1.0);
    }

    #[test]
    fn sampled_lut_fits_a_plausible_gamma() {
        // Build a 256-entry y = x^2.2 table and recover ~2.2.
        let table: Vec<u16> = (0..256)
            .map(|i| {
                let x = i as f64 / 255.0;
                (x.powf(2.2) * 65535.0).round() as u16
            })
            .collect();
        let g = fit_gamma(&table);
        assert!((g - 2.2).abs() < 0.05, "fitted gamma {g}");
    }

    #[test]
    fn srgb_like_curve_detection() {
        assert!(trc_is_srgb_like(&SRGB_TRC), "exact sRGB");
        assert!(
            trc_is_srgb_like(&gamma_trc(2.17)),
            "LUT-fit sRGB ~2.2 gamma"
        );
        assert!(trc_is_srgb_like(&gamma_trc(2.2)), "plain 2.2");
        assert!(!trc_is_srgb_like(&LINEAR_TRC), "linear is not sRGB-like");
        assert!(
            !trc_is_srgb_like(&gamma_trc(1.8)),
            "1.8 gamma is not sRGB-like"
        );
    }

    #[test]
    fn parametric_srgb_curve_is_detected_as_srgb() {
        // The ICC type-3 sRGB parametric vector: [g, a, b, c, d].
        let params = vec![2.4f32, 1.0 / 1.055, 0.055 / 1.055, 1.0 / 12.92, 0.04045];
        let trc = trc_from_curve(&ToneReprCurve::Parametric(params));
        assert!(is_srgb_trc(&trc), "should match sRGB, got {:?}", trc);
    }
}
