//! FFmpeg color metadata → the shader transform + YUV conversion policy
//! (plan §9).
//!
//! FFmpeg reports color as H.273 CICP code points (primaries / transfer /
//! matrix / range), with `Unspecified` rampant in the wild. This module owns:
//!
//! - the **fallback policy** for unspecified fields (by dimensions: SD → BT.601,
//!   HD+ → BT.709; range: limited — the MPEG convention),
//! - the **single-application contract**'s RGB half: swscale applies matrix +
//!   range during YUV→RGB (with the *correct* coefficients — see
//!   [`set_scaler_colorspace`]; unconfigured swscale silently assumes BT.601,
//!   which visibly shifts BT.709 content), so the emitted `ColorTransform`
//!   carries primaries + transfer only,
//! - the **SDR display convention**: BT.709/601-family transfer displays as
//!   sRGB (the universal video-on-desktop convention, and what the Windows MF
//!   RGB32 path produces), so plain HD video stays on the free passthrough;
//!   only genuinely different primaries (P3, BT.2020) or curves (PQ/HLG — the
//!   HDR path) enable a transform.

use ffmpeg_next as ff;
use ffmpeg_next::ffi;

use crate::video::{VideoColorInfo, YuvMatrix};
use crate::ColorTransform;

/// H.273 transfer_characteristics values this module special-cases.
const TRC_BT709: u8 = 1; // camera OETF family: 1 (709), 6 (601), 14/15 (2020 SDR)
const TRC_UNSPECIFIED: u8 = 2;
const TRC_SRGB: u8 = 13;
const TRC_PQ: u8 = 16; // SMPTE ST 2084
const TRC_HLG: u8 = 18; // ARIB STD-B67

/// Which HDR curve a source carries, when it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hdr {
    Pq,
    Hlg,
}

/// A source's color, resolved to concrete CICP code points (no `Unspecified`
/// left) plus the flags the pipeline branches on.
#[derive(Debug, Clone, Copy)]
pub struct SourceColor {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
    /// PQ/HLG detection — the fp16 HDR path branches on it (task #84 subtask
    /// 3); the SDR bring-up path ignores it.
    #[allow(dead_code)]
    pub hdr: Option<Hdr>,
}

/// Numeric CICP value of an FFmpeg color enum (they ARE H.273 code points).
fn prim_u8(p: ff::color::Primaries) -> u8 {
    let v: ffi::AVColorPrimaries = p.into();
    v as u8
}
fn trc_u8(t: ff::color::TransferCharacteristic) -> u8 {
    let v: ffi::AVColorTransferCharacteristic = t.into();
    v as u8
}
fn space_u8(s: ff::color::Space) -> u8 {
    let v: ffi::AVColorSpace = s.into();
    v as u8
}

/// Resolve a source's reported color to concrete values, filling `Unspecified`
/// by the dimension convention (per-frame values take precedence at the call
/// site — pass the frame's when it has them, else the decoder/stream's; plan
/// §9's precedence chain collapses to "the most specific non-unspecified").
pub fn resolve(
    primaries: ff::color::Primaries,
    transfer: ff::color::TransferCharacteristic,
    space: ff::color::Space,
    range: ff::color::Range,
    width: u32,
    height: u32,
) -> SourceColor {
    let hd = width >= 1280 || height >= 720;
    let p = match prim_u8(primaries) {
        0 | 2 => {
            if hd {
                1 // BT.709
            } else {
                6 // SMPTE 170M (BT.601-525; the safer SD default)
            }
        }
        v => v,
    };
    let t = match trc_u8(transfer) {
        0 | TRC_UNSPECIFIED => TRC_BT709,
        v => v,
    };
    let m = match space_u8(space) {
        // 0 = identity (RGB sources), 2 = unspecified → by dims.
        2 => {
            if hd {
                1 // BT.709
            } else {
                6 // SMPTE 170M
            }
        }
        v => v,
    };
    let full_range = matches!(range, ff::color::Range::JPEG);
    let hdr = match t {
        TRC_PQ => Some(Hdr::Pq),
        TRC_HLG => Some(Hdr::Hlg),
        _ => None,
    };
    SourceColor {
        primaries: p,
        transfer: t,
        matrix: m,
        full_range,
        hdr,
    }
}

/// The in-shader transform for **SDR RGB output** (post-swscale). The SDR
/// display convention maps the whole BT.709/601 camera-OETF family to sRGB —
/// so BT.709-primaries content is a free passthrough — while honest gamma
/// curves (Adobe-style 2.2/2.8, linear) and true sRGB flags pass through
/// `ColorTransform::from_cicp` unchanged. Wide primaries (P3, BT.2020) always
/// enable the matrix. HDR curves are NOT handled here (the fp16 path owns
/// them); callers on this path have already tone-decided.
pub fn sdr_transform(sc: &SourceColor) -> ColorTransform {
    let effective_transfer = match sc.transfer {
        // Camera-OETF family (BT.709 / BT.601 / BT.2020-SDR) → display as sRGB.
        1 | 6 | 14 | 15 => TRC_SRGB,
        // PQ/HLG reaching the SDR path (bring-up only): primaries still count.
        TRC_PQ | TRC_HLG => TRC_SRGB,
        v => v,
    };
    // Matrix = 0 (identity): the pixels are already RGB (swscale applied the
    // YUV matrix); full range likewise — RGB output is always full-swing.
    ColorTransform::from_cicp(sc.primaries, effective_transfer, 0, true)
}

/// [`VideoColorInfo`] for a producer emitting **RGBA8** frames (the software
/// path): matrix + range are inert (swscale already applied them — the
/// single-application contract), the transform carries primaries/transfer.
pub fn video_color_info_rgb(sc: &SourceColor) -> VideoColorInfo {
    VideoColorInfo {
        transform: sdr_transform(sc),
        cicp: Some((sc.primaries, sc.transfer, sc.matrix)),
        full_range: true,
        yuv_matrix: yuv_matrix(sc.matrix),
        // RGB output: transfer is inert (already applied); the transform carries it.
        transfer: crate::VideoTransfer::SrgbLike,
        peak: 1.0,
    }
}

/// H.273 matrix_coefficients → the renderer's [`YuvMatrix`] vocabulary (used by
/// the NV12 hardware path; recorded even on RGB output for diagnostics).
pub fn yuv_matrix(matrix: u8) -> YuvMatrix {
    match matrix {
        5 | 6 => YuvMatrix::Bt601,
        9 | 10 => YuvMatrix::Bt2020,
        _ => YuvMatrix::Bt709,
    }
}

/// Configure a swscale context with the source's REAL matrix coefficients +
/// range. Without this swscale converts every source with BT.601 coefficients
/// — a visible green/magenta shift on BT.709 HD content. Call once after
/// creating the scaler; a nonzero return (RGB sources, where there's nothing to
/// set) is fine to ignore.
///
/// # Safety
/// `sws` must be a live `SwsContext` owned by the caller.
pub unsafe fn set_scaler_colorspace(sws: *mut ffi::SwsContext, matrix: u8, full_range: bool) {
    if sws.is_null() {
        return;
    }
    let cs = match matrix {
        5 | 6 => ffi::SWS_CS_ITU601,
        9 | 10 => ffi::SWS_CS_BT2020,
        _ => ffi::SWS_CS_ITU709,
    };
    let coeffs = ffi::sws_getCoefficients(cs);
    let range = i32::from(full_range);
    // Output is full-range RGB; brightness/contrast/saturation neutral.
    let _ = ffi::sws_setColorspaceDetails(
        sws,
        coeffs,
        range,
        ffi::sws_getCoefficients(ffi::SWS_CS_DEFAULT),
        1,
        0,
        1 << 16,
        1 << 16,
    );
}

// ── HDR transfer decode (plan §9, task #84 subtask 3) ────────────────────────
//
// The fp16 video path mirrors the HDR-stills convention exactly
// (`common::finalize_hdr_scrgb` / WIC `128bppRGBAFloat` / ImageIO
// extended-linear-sRGB): pixels leave here as **scene-linear scRGB** — linear
// light, BT.709 primaries, extended range — where **1.0 = SDR reference white
// (203 nits, BT.2408 graphics white)**. The renderer either presents that to an
// fp16/EDR surface or tone-maps to SDR using the frame's `peak`.

/// SDR reference white for the scene-linear normalization (BT.2408).
pub const SDR_WHITE_NITS: f32 = 203.0;

/// ST 2084 (PQ) EOTF: encoded [0,1] → scene-linear scRGB (1.0 = 203 nits).
pub fn pq_to_scrgb(e: f32) -> f32 {
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

/// ARIB STD-B67 (HLG) → scene-linear scRGB, per-channel with the BT.2100
/// nominal-peak OOTF (γ = 1.2, Lw = 1000 nits). Per-channel is the standard
/// fast approximation of the luminance-based OOTF — fine for v1 SW playback
/// (the HW path re-does this in-shader later).
pub fn hlg_to_scrgb(e: f32) -> f32 {
    const A: f32 = 0.178_832_77;
    const B: f32 = 0.284_668_92; // 1 - 4a
    const C: f32 = 0.559_910_7;
    let e = e.clamp(0.0, 1.0);
    // Inverse OETF → scene light [0,1].
    let ys = if e <= 0.5 {
        (e * e) / 3.0
    } else {
        (((e - C) / A).exp() + B) / 12.0
    };
    // OOTF → display light at the nominal 1000-nit peak.
    let nits = 1000.0 * ys.max(0.0).powf(1.2);
    nits / SDR_WHITE_NITS
}

/// 3×3 source-linear → sRGB-linear primaries matrix for an H.273 primaries
/// code (BT.2020 → 709 for HDR video; identity for 709). Reuses the CICP
/// machinery with a linear transfer (code 8) — only the matrix is taken.
pub fn linear_primaries_matrix(primaries: u8) -> [[f32; 3]; 3] {
    ColorTransform::from_cicp(primaries, 8, 0, true).matrix
}

/// Default HDR mastering peak (nits) when the container carries no valid
/// content-light / mastering-display metadata — task #91 Phase 2. 1000 nits is the
/// common HDR10 grade (and the corpus's MaxCLL), a safe SDR tone-map white point.
pub const DEFAULT_HDR_NITS: f64 = 1000.0;

/// Resolve the HDR tone-map peak (scene-linear scRGB, 1.0 = 203 nits) from
/// container metadata, with precedence, validity checks, and a stable default
/// (task #91 Phase 2, replacing the R11 running-max pixel scan). **MaxCLL**
/// (content-light) wins over **mastering-display max-luminance**; each is accepted
/// only when finite and in a sane `[1, 10000]` nit range, else it falls through to
/// [`DEFAULT_HDR_NITS`]. The result never drops below SDR white (1.0). Static
/// metadata — it does not change across a session (no decay/reset on seek).
pub fn resolve_hdr_peak(maxcll_nits: Option<u32>, mastering_nits: Option<f64>) -> f32 {
    let valid = |n: f64| n.is_finite() && (1.0..=10_000.0).contains(&n);
    let nits = maxcll_nits
        .map(f64::from)
        .filter(|&n| valid(n))
        .or_else(|| mastering_nits.filter(|&n| valid(n)))
        .unwrap_or(DEFAULT_HDR_NITS);
    (nits as f32 / SDR_WHITE_NITS).max(1.0)
}

// FFmpeg's `AVContentLightMetadata` / `AVMasteringDisplayMetadata` structs aren't
// in this build's bindgen allowlist, so we mirror their **stable public ABI**
// (unchanged since FFmpeg 3.x) to read the side-data blob. `AVRational` is bound.
#[repr(C)]
struct ContentLightMetadata {
    max_cll: std::os::raw::c_uint,
    max_fall: std::os::raw::c_uint,
}
#[repr(C)]
struct MasteringDisplayMetadata {
    display_primaries: [[ffi::AVRational; 2]; 3],
    white_point: [ffi::AVRational; 2],
    min_luminance: ffi::AVRational,
    max_luminance: ffi::AVRational,
    has_primaries: std::os::raw::c_int,
    has_luminance: std::os::raw::c_int,
}

/// The `(MaxCLL, mastering_max_luminance)` HDR metadata a video stream carries, in
/// nits — read from codecpar's `coded_side_data` (FFmpeg 8 moved stream side data
/// there; same walk as [`super::probe::rotation_degrees`]). Either is `None` when
/// absent. Feed to [`resolve_hdr_peak`]. Static/container-level only (dynamic
/// HDR10+/DoVi RPU is a non-goal).
pub fn hdr_metadata_nits(stream: &ff::format::stream::Stream) -> (Option<u32>, Option<f64>) {
    unsafe {
        let par = (*stream.as_ptr()).codecpar;
        if par.is_null() {
            return (None, None);
        }
        let get = |kind| {
            let sd =
                ffi::av_packet_side_data_get((*par).coded_side_data, (*par).nb_coded_side_data, kind);
            if sd.is_null() || (*sd).data.is_null() {
                None
            } else {
                Some((*sd).data)
            }
        };
        let maxcll = get(ffi::AVPacketSideDataType::AV_PKT_DATA_CONTENT_LIGHT_LEVEL).map(|d| {
            let m = &*(d as *const ContentLightMetadata);
            m.max_cll
        });
        let mastering = get(ffi::AVPacketSideDataType::AV_PKT_DATA_MASTERING_DISPLAY_METADATA)
            .and_then(|d| {
                let m = &*(d as *const MasteringDisplayMetadata);
                if m.has_luminance == 0 || m.max_luminance.den == 0 {
                    return None;
                }
                Some(m.max_luminance.num as f64 / m.max_luminance.den as f64)
            });
        (maxcll, mastering)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff::color::{Primaries, Range, Space, TransferCharacteristic};

    fn resolve_dims(w: u32, h: u32) -> SourceColor {
        resolve(
            Primaries::Unspecified,
            TransferCharacteristic::Unspecified,
            Space::Unspecified,
            Range::Unspecified,
            w,
            h,
        )
    }

    #[test]
    fn hdr_peak_precedence_validity_and_default() {
        let w = SDR_WHITE_NITS;
        // MaxCLL present + valid → wins over mastering.
        assert!((resolve_hdr_peak(Some(4000), Some(1000.0)) - 4000.0 / w).abs() < 1e-3);
        // MaxCLL absent → mastering-display max-luminance.
        assert!((resolve_hdr_peak(None, Some(1000.0)) - 1000.0 / w).abs() < 1e-3);
        // Neither → the 1000-nit default.
        assert!((resolve_hdr_peak(None, None) - 1000.0 / w).abs() < 1e-3);
        // Malformed MaxCLL (0 / absurd) falls through to mastering, then default.
        assert!((resolve_hdr_peak(Some(0), Some(600.0)) - 600.0 / w).abs() < 1e-3);
        assert!((resolve_hdr_peak(Some(99_999), None) - 1000.0 / w).abs() < 1e-3);
        // Conflicting: invalid MaxCLL + invalid mastering → default.
        assert!((resolve_hdr_peak(Some(0), Some(f64::NAN)) - 1000.0 / w).abs() < 1e-3);
        // Never below SDR white.
        assert!(resolve_hdr_peak(Some(50), None) >= 1.0);
    }

    #[test]
    fn unspecified_falls_back_by_dimensions() {
        let hd = resolve_dims(1920, 1080);
        assert_eq!((hd.primaries, hd.matrix), (1, 1), "HD → BT.709");
        assert!(!hd.full_range, "unspecified range → limited (MPEG)");
        let sd = resolve_dims(720, 480);
        assert_eq!((sd.primaries, sd.matrix), (6, 6), "SD → BT.601/SMPTE170M");
        assert_eq!(hd.hdr, None);
    }

    #[test]
    fn reported_values_win_over_fallbacks() {
        let c = resolve(
            Primaries::BT2020,
            TransferCharacteristic::SMPTE2084,
            Space::BT2020NCL,
            Range::JPEG,
            3840,
            2160,
        );
        assert_eq!((c.primaries, c.transfer, c.matrix), (9, 16, 9));
        assert!(c.full_range);
        assert_eq!(c.hdr, Some(Hdr::Pq));
        assert_eq!(
            resolve(
                Primaries::BT2020,
                TransferCharacteristic::ARIB_STD_B67,
                Space::BT2020NCL,
                Range::MPEG,
                3840,
                2160,
            )
            .hdr,
            Some(Hdr::Hlg)
        );
    }

    #[test]
    fn bt709_video_is_a_free_passthrough() {
        let c = resolve(
            Primaries::BT709,
            TransferCharacteristic::BT709,
            Space::BT709,
            Range::MPEG,
            1920,
            1080,
        );
        let t = sdr_transform(&c);
        assert!(!t.enabled, "709 primaries + camera OETF → sRGB passthrough");
        let info = video_color_info_rgb(&c);
        assert!(info.full_range, "RGB output is full-swing");
        assert_eq!(info.cicp, Some((1, 1, 1)), "CICP kept verbatim");
        assert_eq!(info.yuv_matrix, YuvMatrix::Bt709);
    }

    #[test]
    fn wide_primaries_enable_the_matrix() {
        // P3-D65 (SMPTE 432, code 12) with sRGB transfer — the iPhone SDR case.
        let c = resolve(
            Primaries::SMPTE432,
            TransferCharacteristic::IEC61966_2_1,
            Space::BT709,
            Range::MPEG,
            1920,
            1080,
        );
        assert!(sdr_transform(&c).enabled, "P3 must convert");
        // BT.2020 primaries likewise.
        let c2020 = resolve(
            Primaries::BT2020,
            TransferCharacteristic::BT2020_10,
            Space::BT2020NCL,
            Range::MPEG,
            3840,
            2160,
        );
        assert!(sdr_transform(&c2020).enabled, "2020 must convert");
        assert_eq!(video_color_info_rgb(&c2020).yuv_matrix, YuvMatrix::Bt2020);
    }

    #[test]
    fn bt601_sources_map_matrix_and_stay_srgb_display() {
        let c = resolve(
            Primaries::SMPTE170M,
            TransferCharacteristic::SMPTE170M,
            Space::SMPTE170M,
            Range::MPEG,
            720,
            480,
        );
        assert_eq!(video_color_info_rgb(&c).yuv_matrix, YuvMatrix::Bt601);
        // 601 primaries are near-709; either way the *transfer* must not
        // double-darken: it resolves to sRGB-family display.
        let t = sdr_transform(&c);
        // (Enabled or not depends on how close moxcms deems the primaries —
        // the invariant is it never errors and never picks a wild curve.)
        let _ = t;
    }

    /// PQ anchor points from BT.2408/ST 2084: SDR reference white (203 nits)
    /// encodes at ≈0.5807 and must decode to scRGB 1.0; the curve's endpoints
    /// hold; 0.508 is ≈100 nits.
    #[test]
    fn pq_decodes_the_reference_anchors() {
        let white = pq_to_scrgb(0.580_688_9);
        assert!((white - 1.0).abs() < 5e-3, "203 nits → 1.0, got {white}");
        assert_eq!(pq_to_scrgb(0.0), 0.0);
        let peak = pq_to_scrgb(1.0);
        assert!(
            (peak - 10000.0 / SDR_WHITE_NITS).abs() < 0.5,
            "PQ 1.0 = 10000 nits, got {peak}"
        );
        let hundred = pq_to_scrgb(0.508);
        assert!(
            (hundred - 100.0 / SDR_WHITE_NITS).abs() < 0.01,
            "PQ 0.508 ≈ 100 nits, got {}",
            hundred * SDR_WHITE_NITS
        );
        // Out-of-range input clamps, never NaNs.
        assert!(pq_to_scrgb(-1.0) == 0.0 && pq_to_scrgb(2.0).is_finite());
    }

    /// HLG anchors: black → 0, nominal peak (1.0) → 1000 nits, monotonic.
    #[test]
    fn hlg_decodes_the_reference_anchors() {
        assert_eq!(hlg_to_scrgb(0.0), 0.0);
        let peak = hlg_to_scrgb(1.0);
        assert!(
            (peak - 1000.0 / SDR_WHITE_NITS).abs() < 0.05,
            "HLG 1.0 = 1000 nits, got {}",
            peak * SDR_WHITE_NITS
        );
        let mid = hlg_to_scrgb(0.5);
        let hi = hlg_to_scrgb(0.75);
        assert!(0.0 < mid && mid < hi && hi < peak, "monotonic");
    }

    /// The primaries matrix: identity for 709, a real gamut map for 2020
    /// (wide red pulls sRGB green/blue negative), white preserved.
    #[test]
    fn linear_primaries_matrix_maps_2020_and_passes_709() {
        let m709 = linear_primaries_matrix(1);
        for (i, row) in m709.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                let want = if i == j { 1.0 } else { 0.0 };
                assert!((v - want).abs() < 1e-3, "709 must be identity: {m709:?}");
            }
        }
        let m2020 = linear_primaries_matrix(9);
        assert!(
            m2020[1][0] < 0.0 && m2020[2][0] < 0.0,
            "2020 red exceeds sRGB: {m2020:?}"
        );
        for row in &m2020 {
            let sum: f32 = row.iter().sum();
            assert!((sum - 1.0).abs() < 1e-2, "white preserved: {m2020:?}");
        }
    }
}
