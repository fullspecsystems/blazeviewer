//! `prepare`: an [`ExportSource`] → an [`ExportImage`] an encoder can write (export-plan.md
//! § 3.2, rung 0b's minimal set — review F13).
//!
//! In order: un-premultiply, resolve float samples (HDR is tone-mapped to SDR Display P3;
//! encoded floats are quantized to 16-bit), resolve the colour tag (sanitize the profile
//! under *Strip metadata*, synthesize one for CICP, fall back to sRGB when a profile can't
//! describe these pixels), bake the RAM rotation, then clear the hidden colour under fully
//! transparent pixels when stripping. Resize and alpha flattening arrive with rung 2.

use std::sync::Arc;

use crate::icc;
use crate::types::{
    AlphaMode, Channels, ColorDesc, ExportImage, ExportNote, ExportSource, OutputColor,
    SampleEncoding, Samples,
};

/// What the target wants from preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrepareSpec {
    /// Clockwise quarter turns to bake in (the RAM rotation): 0..=3.
    pub rotation_cw: u8,
    /// *Strip metadata*: sanitize the profile and clear colour under transparency.
    pub strip_metadata: bool,
}

/// Why preparation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    /// The buffer's length doesn't match its dimensions.
    Malformed,
    /// The source's colour model can't be exported yet (e.g. CMYK).
    UnsupportedColor(&'static str),
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrepareError::Malformed => write!(f, "image buffer is malformed"),
            PrepareError::UnsupportedColor(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for PrepareError {}

/// sRGB / BT.709 linear → Display P3 linear (D65 both; rows sum to 1 so white is kept).
const SRGB_TO_P3: [[f32; 3]; 3] = [
    [0.822_462_1, 0.177_538, 0.0],
    [0.033_194_1, 0.966_805_8, 0.0],
    [0.017_082_7, 0.072_397_4, 0.910_519_9],
];

/// Extended Reinhard with white point `lw` — the same curve the SDR present pass and the
/// clipboard use (`engine::to_clipboard_rgba8`), so an SDR export matches an SDR screen.
fn reinhard(v: f32, lw: f32) -> f32 {
    let x = v.max(0.0);
    x * (1.0 + x / (lw * lw)) / (1.0 + x)
}

/// The sRGB (and Display P3) transfer curve, linear → encoded.
fn srgb_oetf(c: f32) -> f32 {
    let x = c.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

fn q16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16
}

/// Run the full preparation. Consumes the source so peak memory isn't 2× (review F27).
pub fn prepare(src: ExportSource, spec: &PrepareSpec) -> Result<ExportImage, PrepareError> {
    if !src.is_well_formed() {
        return Err(PrepareError::Malformed);
    }
    let ExportSource {
        width,
        height,
        mut channels,
        mut samples,
        encoding,
        alpha,
        color,
        provenance: _,
    } = src;
    let mut notes = Vec::new();

    if channels.has_alpha() && alpha == AlphaMode::Premultiplied {
        unpremultiply(&mut samples, channels);
    }

    // Floats never reach an encoder.
    let mut color = color;
    if let Samples::F32(f) = &samples {
        if encoding == SampleEncoding::Linear || color == ColorDesc::LinearScRgb {
            let (s, ch, tone_mapped) = tone_map_to_p3(f, channels);
            samples = s;
            channels = ch;
            color = ColorDesc::Icc(Arc::from(icc::display_p3_profile()));
            if tone_mapped {
                notes.push(ExportNote::HdrToneMapped);
            }
        } else {
            samples = Samples::U16(f.iter().map(|&v| q16(v)).collect());
        }
    }

    let output = resolve_color(
        &color,
        spec.strip_metadata,
        &mut channels,
        &mut samples,
        &mut notes,
    )?;

    let (samples, w, h) = rotate(
        samples,
        width as usize,
        height as usize,
        channels.count(),
        spec.rotation_cw % 4,
    );

    let mut samples = samples;
    if spec.strip_metadata
        && channels.has_alpha()
        && clear_under_transparency(&mut samples, channels)
    {
        notes.push(ExportNote::TransparentColorCleared);
    }

    Ok(ExportImage {
        width: w as u32,
        height: h as u32,
        channels,
        samples,
        color: output,
        notes,
    })
}

fn unpremultiply(samples: &mut Samples, channels: Channels) {
    let n = channels.count();
    let a = n - 1;
    match samples {
        Samples::U8(v) => {
            for px in v.chunks_exact_mut(n) {
                let al = px[a] as u32;
                for c in &mut px[..a] {
                    *c = (*c as u32 * 255 + al / 2)
                        .checked_div(al)
                        .map_or(0, |v| v.min(255) as u8);
                }
            }
        }
        Samples::U16(v) => {
            for px in v.chunks_exact_mut(n) {
                let al = px[a] as u64;
                for c in &mut px[..a] {
                    *c = (*c as u64 * 65535 + al / 2)
                        .checked_div(al)
                        .map_or(0, |v| v.min(65535) as u16);
                }
            }
        }
        Samples::F32(v) => {
            for px in v.chunks_exact_mut(n) {
                let al = px[a];
                for c in &mut px[..a] {
                    *c = if al <= 0.0 { 0.0 } else { *c / al };
                }
            }
        }
    }
}

/// Tone-map linear scRGB (BT.709 primaries, 1.0 = SDR white) to Display P3 16-bit
/// (export-plan.md § 3.8): convert to P3-linear first so wide-gamut colour isn't clamped
/// away, take the white point from the full-resolution peak, apply extended Reinhard, encode
/// with the sRGB/P3 curve. Gray input is expanded to RGB (the P3 profile is RGB). Returns the
/// samples, their layout, and whether anything exceeded SDR white (i.e. real tone-mapping
/// happened rather than an identity pass).
fn tone_map_to_p3(f: &[f32], channels: Channels) -> (Samples, Channels, bool) {
    let n = channels.count();
    let has_alpha = channels.has_alpha();
    let px_count = f.len() / n;
    let rgb_of = |px: &[f32]| -> [f32; 3] {
        if channels.is_gray() {
            [px[0]; 3]
        } else {
            [px[0], px[1], px[2]]
        }
    };
    let to_p3 = |c: [f32; 3]| -> [f32; 3] {
        let m = SRGB_TO_P3;
        [
            m[0][0] * c[0] + m[0][1] * c[1] + m[0][2] * c[2],
            m[1][0] * c[0] + m[1][1] * c[1] + m[1][2] * c[2],
            m[2][0] * c[0] + m[2][1] * c[1] + m[2][2] * c[2],
        ]
    };
    let mut peak = 1.0f32;
    for px in f.chunks_exact(n) {
        for v in to_p3(rgb_of(px)) {
            if v.is_finite() && v > peak {
                peak = v;
            }
        }
    }
    let out_ch = if has_alpha {
        Channels::Rgba
    } else {
        Channels::Rgb
    };
    let mut out = Vec::with_capacity(px_count * out_ch.count());
    for px in f.chunks_exact(n) {
        for v in to_p3(rgb_of(px)) {
            let v = if v.is_finite() { v } else { 0.0 };
            out.push(q16(srgb_oetf(reinhard(v, peak))));
        }
        if has_alpha {
            out.push(q16(px[n - 1]));
        }
    }
    (Samples::U16(out), out_ch, peak > 1.0 + 1e-4)
}

/// Decide the output colour tag, reconciling the profile with the channel layout.
fn resolve_color(
    color: &ColorDesc,
    strip: bool,
    channels: &mut Channels,
    samples: &mut Samples,
    notes: &mut Vec<ExportNote>,
) -> Result<OutputColor, PrepareError> {
    let icc_bytes: Vec<u8> = match color {
        ColorDesc::Untagged | ColorDesc::LinearScRgb => return Ok(OutputColor::Srgb),
        ColorDesc::Cicp(c) => match icc::profile_for_cicp(*c) {
            Some(p) => p,
            None => {
                notes.push(ExportNote::ProfileReplacedWithSrgb);
                return Ok(OutputColor::Srgb);
            }
        },
        ColorDesc::Icc(b) => b.to_vec(),
    };
    match icc::data_color_space(&icc_bytes) {
        Some(icc::IccSpace::Rgb) if channels.is_gray() => expand_gray(channels, samples),
        Some(icc::IccSpace::Gray) if !channels.is_gray() => {
            if !reduce_to_gray(channels, samples) {
                notes.push(ExportNote::ProfileReplacedWithSrgb);
                return Ok(OutputColor::Srgb);
            }
        }
        Some(icc::IccSpace::Rgb) | Some(icc::IccSpace::Gray) => {}
        Some(icc::IccSpace::Cmyk) => {
            return Err(PrepareError::UnsupportedColor(
                "CMYK images can't be exported yet",
            ))
        }
        _ => {
            notes.push(ExportNote::ProfileReplacedWithSrgb);
            return Ok(OutputColor::Srgb);
        }
    }
    if !strip {
        return Ok(OutputColor::Icc(icc_bytes));
    }
    match icc::sanitize(&icc_bytes) {
        Ok(clean) => {
            let identifying = icc::description(&icc_bytes) != icc::description(&clean);
            if icc::is_srgb(&clean) {
                Ok(OutputColor::Srgb)
            } else {
                if identifying && matches!(color, ColorDesc::Icc(_)) {
                    notes.push(ExportNote::ProfileSanitized);
                }
                Ok(OutputColor::Icc(clean))
            }
        }
        Err(_) => {
            notes.push(ExportNote::ProfileReplacedWithSrgb);
            Ok(OutputColor::Srgb)
        }
    }
}

fn expand_gray(channels: &mut Channels, samples: &mut Samples) {
    fn go<T: Copy>(v: &[T], alpha: bool) -> Vec<T> {
        let n = if alpha { 2 } else { 1 };
        let mut out = Vec::with_capacity(v.len() / n * if alpha { 4 } else { 3 });
        for px in v.chunks_exact(n) {
            out.extend_from_slice(&[px[0], px[0], px[0]]);
            if alpha {
                out.push(px[1]);
            }
        }
        out
    }
    let alpha = channels.has_alpha();
    *samples = match samples {
        Samples::U8(v) => Samples::U8(go(v, alpha)),
        Samples::U16(v) => Samples::U16(go(v, alpha)),
        Samples::F32(v) => Samples::F32(go(v, alpha)),
    };
    *channels = if alpha { Channels::Rgba } else { Channels::Rgb };
}

/// RGB(A) → Gray(A) when every pixel is neutral (R = G = B). `false` (unchanged) otherwise.
fn reduce_to_gray(channels: &mut Channels, samples: &mut Samples) -> bool {
    fn go<T: Copy + PartialEq>(v: &[T], alpha: bool) -> Option<Vec<T>> {
        let n = if alpha { 4 } else { 3 };
        let mut out = Vec::with_capacity(v.len() / n * if alpha { 2 } else { 1 });
        for px in v.chunks_exact(n) {
            if px[0] != px[1] || px[1] != px[2] {
                return None;
            }
            out.push(px[0]);
            if alpha {
                out.push(px[3]);
            }
        }
        Some(out)
    }
    let alpha = channels.has_alpha();
    let reduced = match samples {
        Samples::U8(v) => go(v, alpha).map(Samples::U8),
        Samples::U16(v) => go(v, alpha).map(Samples::U16),
        Samples::F32(v) => go(v, alpha).map(Samples::F32),
    };
    match reduced {
        Some(s) => {
            *samples = s;
            *channels = if alpha {
                Channels::GrayAlpha
            } else {
                Channels::Gray
            };
            true
        }
        None => false,
    }
}

/// Rotate an interleaved buffer by `turns` clockwise quarter turns. Same mapping as
/// `engine::rotate_rgba8`, generic over sample type and channel count.
pub fn rotate(
    samples: Samples,
    w: usize,
    h: usize,
    ch: usize,
    turns: u8,
) -> (Samples, usize, usize) {
    fn go<T: Copy>(
        src: Vec<T>,
        w: usize,
        h: usize,
        ch: usize,
        turns: u8,
    ) -> (Vec<T>, usize, usize) {
        if turns == 0 {
            return (src, w, h);
        }
        let (nw, nh) = if turns % 2 == 1 { (h, w) } else { (w, h) };
        let mut out = src.clone();
        for y in 0..h {
            for x in 0..w {
                let (dx, dy) = match turns {
                    1 => (h - 1 - y, x),
                    2 => (w - 1 - x, h - 1 - y),
                    _ => (y, w - 1 - x),
                };
                let s = (y * w + x) * ch;
                let d = (dy * nw + dx) * ch;
                out[d..d + ch].copy_from_slice(&src[s..s + ch]);
            }
        }
        (out, nw, nh)
    }
    match samples {
        Samples::U8(v) => {
            let (v, w, h) = go(v, w, h, ch, turns);
            (Samples::U8(v), w, h)
        }
        Samples::U16(v) => {
            let (v, w, h) = go(v, w, h, ch, turns);
            (Samples::U16(v), w, h)
        }
        Samples::F32(v) => {
            let (v, w, h) = go(v, w, h, ch, turns);
            (Samples::F32(v), w, h)
        }
    }
}

/// Zero the colour channels of fully transparent pixels — hidden pixel data under
/// transparency can't leak (review F25). Returns whether anything changed.
fn clear_under_transparency(samples: &mut Samples, channels: Channels) -> bool {
    let n = channels.count();
    let mut changed = false;
    match samples {
        Samples::U8(v) => {
            for px in v.chunks_exact_mut(n) {
                if px[n - 1] == 0 && px[..n - 1].iter().any(|&c| c != 0) {
                    px[..n - 1].fill(0);
                    changed = true;
                }
            }
        }
        Samples::U16(v) => {
            for px in v.chunks_exact_mut(n) {
                if px[n - 1] == 0 && px[..n - 1].iter().any(|&c| c != 0) {
                    px[..n - 1].fill(0);
                    changed = true;
                }
            }
        }
        Samples::F32(v) => {
            for px in v.chunks_exact_mut(n) {
                if px[n - 1] <= 0.0 && px[..n - 1].iter().any(|&c| c != 0.0) {
                    px[..n - 1].fill(0.0);
                    changed = true;
                }
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Provenance;
    use proptest::prelude::*;

    fn src(w: u32, h: u32, channels: Channels, samples: Samples) -> ExportSource {
        ExportSource {
            width: w,
            height: h,
            channels,
            samples,
            encoding: SampleEncoding::Encoded,
            alpha: AlphaMode::Straight,
            color: ColorDesc::Untagged,
            provenance: Provenance::default(),
        }
    }

    const STRIP: PrepareSpec = PrepareSpec {
        rotation_cw: 0,
        strip_metadata: true,
    };

    #[test]
    fn untagged_8bit_passes_through_untouched_and_tags_srgb() {
        let px: Vec<u8> = (0..2 * 3 * 3).map(|i| i as u8 * 7).collect();
        let out = prepare(src(2, 3, Channels::Rgb, Samples::U8(px.clone())), &STRIP).unwrap();
        assert_eq!(out.samples, Samples::U8(px));
        assert_eq!(out.color, OutputColor::Srgb);
        assert!(out.notes.is_empty());
    }

    #[test]
    fn rotation_matches_the_clipboard_mapping() {
        // 2x1 image: [A, B] → 90° CW → 1x2 column [A; B].
        let px = vec![1u8, 2, 3, 4, 5, 6];
        let (r, w, h) = rotate(Samples::U8(px), 2, 1, 3, 1);
        assert_eq!((w, h), (1, 2));
        assert_eq!(r, Samples::U8(vec![1, 2, 3, 4, 5, 6]));
        // 180° of [A, B] is [B, A].
        let (r, w, h) = rotate(Samples::U8(vec![1, 2, 3, 4, 5, 6]), 2, 1, 3, 2);
        assert_eq!((w, h), (2, 1));
        assert_eq!(r, Samples::U8(vec![4, 5, 6, 1, 2, 3]));
    }

    proptest! {
        #[test]
        fn four_quarter_turns_are_identity(w in 1usize..9, h in 1usize..9, seed in any::<u16>()) {
            let px: Vec<u16> = (0..w * h * 4).map(|i| (i as u16).wrapping_mul(seed | 1)).collect();
            let mut s = Samples::U16(px.clone());
            let (mut cw, mut ch) = (w, h);
            for _ in 0..4 {
                let (r, nw, nh) = rotate(s, cw, ch, 4, 1);
                s = r; cw = nw; ch = nh;
            }
            prop_assert_eq!((cw, ch), (w, h));
            prop_assert_eq!(s, Samples::U16(px));
        }

        #[test]
        fn one_turn_then_three_is_identity(w in 1usize..9, h in 1usize..9) {
            let px: Vec<u8> = (0..w * h * 2).map(|i| i as u8).collect();
            let (a, aw, ah) = rotate(Samples::U8(px.clone()), w, h, 2, 1);
            let (b, bw, bh) = rotate(a, aw, ah, 2, 3);
            prop_assert_eq!((bw, bh), (w, h));
            prop_assert_eq!(b, Samples::U8(px));
        }
    }

    #[test]
    fn unpremultiply_restores_straight_alpha() {
        // 50%-alpha red, premultiplied: (128, 0, 0, 128) → (255, 0, 0, 128).
        let mut s = src(1, 1, Channels::Rgba, Samples::U8(vec![128, 0, 0, 128]));
        s.alpha = AlphaMode::Premultiplied;
        let out = prepare(
            s,
            &PrepareSpec {
                strip_metadata: false,
                ..STRIP
            },
        )
        .unwrap();
        assert_eq!(out.samples, Samples::U8(vec![255, 0, 0, 128]));
        // 16-bit keeps precision.
        let mut s = src(1, 1, Channels::Rgba, Samples::U16(vec![10000, 0, 0, 20000]));
        s.alpha = AlphaMode::Premultiplied;
        let out = prepare(s, &STRIP).unwrap();
        assert_eq!(out.samples, Samples::U16(vec![32768, 0, 0, 20000]));
    }

    #[test]
    fn strip_clears_hidden_colour_under_transparency() {
        let s = src(
            2,
            1,
            Channels::Rgba,
            Samples::U8(vec![9, 8, 7, 0, 1, 2, 3, 255]),
        );
        let out = prepare(s.clone(), &STRIP).unwrap();
        assert_eq!(out.samples, Samples::U8(vec![0, 0, 0, 0, 1, 2, 3, 255]));
        assert!(out.notes.contains(&ExportNote::TransparentColorCleared));
        let kept = prepare(
            s,
            &PrepareSpec {
                strip_metadata: false,
                ..STRIP
            },
        )
        .unwrap();
        assert_eq!(kept.samples, Samples::U8(vec![9, 8, 7, 0, 1, 2, 3, 255]));
    }

    #[test]
    fn hdr_is_tone_mapped_to_p3_16bit() {
        // SDR white (1.0) and a 4x-white highlight.
        let mut s = src(
            2,
            1,
            Channels::Rgb,
            Samples::F32(vec![1.0, 1.0, 1.0, 4.0, 4.0, 4.0]),
        );
        s.encoding = SampleEncoding::Linear;
        s.color = ColorDesc::LinearScRgb;
        let out = prepare(s, &STRIP).unwrap();
        let Samples::U16(v) = &out.samples else {
            panic!("16-bit expected")
        };
        // The highlight maps to white (peak → 1.0); SDR white rolls below it.
        assert_eq!(v[3], 65535);
        assert!(v[0] < 65535 && v[0] > 30000, "{}", v[0]);
        assert!(out.notes.contains(&ExportNote::HdrToneMapped));
        let OutputColor::Icc(p) = &out.color else {
            panic!("P3 tag expected")
        };
        assert_eq!(icc::data_color_space(p), Some(icc::IccSpace::Rgb));
        assert!(!icc::is_srgb(p));
    }

    #[test]
    fn sdr_range_linear_input_is_not_reported_as_tone_mapped() {
        let mut s = src(1, 1, Channels::Rgb, Samples::F32(vec![0.5, 0.5, 0.5]));
        s.encoding = SampleEncoding::Linear;
        let out = prepare(s, &STRIP).unwrap();
        assert!(!out.notes.contains(&ExportNote::HdrToneMapped));
    }

    #[test]
    fn wide_gamut_hdr_survives_into_p3_instead_of_clamping() {
        // P3's pure green, expressed in BT.709 linear: it has negative red and blue there,
        // so clamping in BT.709 (the clipboard path) would desaturate it. Converting to
        // P3-linear first keeps it exactly P3 green.
        let mut s = src(
            1,
            1,
            Channels::Rgb,
            Samples::F32(vec![-0.224_7, 1.041_9, -0.078_6]),
        );
        s.encoding = SampleEncoding::Linear;
        let out = prepare(s, &STRIP).unwrap();
        let Samples::U16(v) = out.samples else {
            panic!()
        };
        assert!(v[1] > 65000 && v[0] < 1500 && v[2] < 1500, "{v:?}");
    }

    #[test]
    fn a_gray_profile_on_neutral_rgb_reduces_to_gray() {
        let gray_icc = {
            let mut p = moxcms::ColorProfile::new_gray_with_gamma(2.2);
            p.color_space = moxcms::DataColorSpace::Gray;
            p.encode().unwrap()
        };
        let mut s = src(
            2,
            1,
            Channels::Rgb,
            Samples::U8(vec![10, 10, 10, 200, 200, 200]),
        );
        s.color = ColorDesc::Icc(Arc::from(gray_icc));
        let out = prepare(s, &STRIP).unwrap();
        assert_eq!(out.channels, Channels::Gray);
        assert_eq!(out.samples, Samples::U8(vec![10, 200]));
        assert!(matches!(out.color, OutputColor::Icc(_)));
    }

    #[test]
    fn an_rgb_profile_on_gray_pixels_expands_them() {
        let p3 = moxcms::ColorProfile::new_display_p3().encode().unwrap();
        let mut s = src(1, 1, Channels::GrayAlpha, Samples::U8(vec![50, 255]));
        s.color = ColorDesc::Icc(Arc::from(p3));
        let out = prepare(s, &STRIP).unwrap();
        assert_eq!(out.channels, Channels::Rgba);
        assert_eq!(out.samples, Samples::U8(vec![50, 50, 50, 255]));
    }

    #[test]
    fn an_unreadable_profile_falls_back_to_srgb_with_a_note() {
        let mut s = src(1, 1, Channels::Rgb, Samples::U8(vec![1, 2, 3]));
        s.color = ColorDesc::Icc(Arc::from(vec![0u8; 40]));
        let out = prepare(s, &STRIP).unwrap();
        assert_eq!(out.color, OutputColor::Srgb);
        assert!(out.notes.contains(&ExportNote::ProfileReplacedWithSrgb));
    }

    #[test]
    fn an_srgb_profile_becomes_the_srgb_tag() {
        let srgb = moxcms::ColorProfile::new_srgb().encode().unwrap();
        let mut s = src(1, 1, Channels::Rgb, Samples::U8(vec![1, 2, 3]));
        s.color = ColorDesc::Icc(Arc::from(srgb));
        let out = prepare(s, &STRIP).unwrap();
        assert_eq!(out.color, OutputColor::Srgb);
    }

    #[test]
    fn malformed_buffers_are_rejected() {
        let s = src(4, 4, Channels::Rgb, Samples::U8(vec![0; 5]));
        assert_eq!(prepare(s, &STRIP), Err(PrepareError::Malformed));
    }
}
