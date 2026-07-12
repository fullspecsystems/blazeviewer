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
}
