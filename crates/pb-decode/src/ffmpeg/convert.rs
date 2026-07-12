//! Decoded `AVFrame` → tightly-packed, fitted, upright pixels — the conversion
//! stage the poster and the producer share, so geometry / rotation / color
//! policy is one implementation (poster ≡ playback by construction, the same
//! guarantee the Windows MF pair gets from sharing a reader config).
//!
//! Two output modes, decided per clip at construction (plan §9):
//!
//! - **SDR → RGBA8.** Decode-to-fit happens **inside swscale** (scale during
//!   the YUV→RGB pass we already run — the `ff_live` measurement: a separate
//!   Lanczos pass was ~44% of decode time), with the swscale coefficients set
//!   from the source's real matrix + range.
//! - **HDR (PQ/HLG) → fp16 scene-linear scRGB.** swscale converts YUV→RGB in
//!   16-bit *preserving the transfer encoding* (matrix + range only), then a
//!   LUT-driven CPU pass decodes PQ/HLG to linear light (1.0 = 203-nit SDR
//!   white), maps the primaries (BT.2020 → 709) and packs f16 — the exact
//!   convention of `common::finalize_hdr_scrgb`, so HDR video rides the same
//!   renderer path as HDR stills. Never RGBA8-clipped (owner decision #1).
//!
//! Rotation (display matrix) is applied after, on the small frame.

use ff::software::scaling::{Context as Scaler, Flags as ScaleFlags};
use ffmpeg_next as ff;

use super::color::{self, Hdr, SourceColor};
use crate::common::rotate_rgba;
use crate::PixelFormat;

/// Lazily-initialized converter: swscale needs the first real frame's pixel
/// format (decoders may refine format/color after the first packet), so
/// construction records the *plan* (output geometry, rotation, HDR-or-not from
/// the decoder-reported transfer, the decoder color as fallback) and the first
/// [`convert`](Self::convert) resolves the rest.
pub struct FrameConverter {
    /// Coded input dims the plan was made for — a mid-stream change is a clean
    /// failure (plan §5), never silent stale geometry.
    src_w: u32,
    src_h: u32,
    /// Pre-rotation scaled output dims (SAR correction baked in).
    out_w: u32,
    out_h: u32,
    /// Clockwise display rotation applied post-scale.
    rotation: i32,
    /// HDR mode, fixed per clip from the decoder-reported transfer (frames
    /// only *refine* primaries/matrix — a clip can't flip SDR↔HDR mid-session
    /// without also failing the geometry checks).
    hdr: Option<Hdr>,
    /// Decoder-reported color enums — the fallback when frames don't say.
    dec_primaries: ff::color::Primaries,
    dec_transfer: ff::color::TransferCharacteristic,
    dec_space: ff::color::Space,
    dec_range: ff::color::Range,
    /// Resolved at the first frame (frame metadata wins over decoder's).
    resolved: Option<SourceColor>,
    scaler: Option<(ff::format::Pixel, Scaler)>,
    /// HDR: encoded-u16 → scene-linear LUT (256 KiB, built once per clip).
    hdr_lut: Option<Box<[f32]>>,
    /// HDR: source-linear → sRGB-linear primaries matrix.
    hdr_matrix: [[f32; 3]; 3],
    /// HDR: running scene-linear peak across converted frames (≥ 1.0).
    peak: f32,
}

impl FrameConverter {
    /// `out` is the **pre-rotation** scaled output size (the caller computed it
    /// from the fitted display dims); `rotation` the CW display rotation.
    pub fn new(
        src: (u32, u32),
        out: (u32, u32),
        rotation: i32,
        decoder: &ff::decoder::Video,
    ) -> Self {
        let dec_sc = color::resolve(
            decoder.color_primaries(),
            decoder.color_transfer_characteristic(),
            decoder.color_space(),
            decoder.color_range(),
            src.0,
            src.1,
        );
        FrameConverter {
            src_w: src.0,
            src_h: src.1,
            out_w: out.0.max(1),
            out_h: out.1.max(1),
            rotation,
            hdr: dec_sc.hdr,
            dec_primaries: decoder.color_primaries(),
            dec_transfer: decoder.color_transfer_characteristic(),
            dec_space: decoder.color_space(),
            dec_range: decoder.color_range(),
            resolved: None,
            scaler: None,
            hdr_lut: None,
            hdr_matrix: color::linear_primaries_matrix(dec_sc.primaries),
            peak: 1.0,
        }
    }

    /// Post-rotation (display) output dimensions — the session's fixed geometry.
    pub fn display_dims(&self) -> (u32, u32) {
        if self.rotation % 180 == 90 {
            (self.out_h, self.out_w)
        } else {
            (self.out_w, self.out_h)
        }
    }

    /// What [`convert`](Self::convert) emits: fp16 scene-linear for HDR
    /// sources, RGBA8 otherwise. Fixed for the clip.
    pub fn output_format(&self) -> PixelFormat {
        if self.hdr.is_some() {
            PixelFormat::Rgba16F
        } else {
            PixelFormat::Rgba8
        }
    }

    /// Scene-linear peak across all frames converted so far (1.0 for SDR) —
    /// the tone-map white point for SDR presentation.
    pub fn peak(&self) -> f32 {
        self.peak
    }

    /// The source color as resolved at the first converted frame; before that,
    /// the decoder-reported values under the same fallback policy.
    pub fn source_color(&self) -> SourceColor {
        self.resolved.unwrap_or_else(|| {
            color::resolve(
                self.dec_primaries,
                self.dec_transfer,
                self.dec_space,
                self.dec_range,
                self.src_w,
                self.src_h,
            )
        })
    }

    /// Convert one decoded frame to tightly-packed upright pixels
    /// ([`output_format`](Self::output_format)) at the fixed output geometry.
    /// Errors on a mid-stream size change (clean failure per plan §5).
    /// Returns `(pixels, display_w, display_h)`.
    pub fn convert(&mut self, frame: &ff::frame::Video) -> Result<(Vec<u8>, u32, u32), String> {
        if (frame.width(), frame.height()) != (self.src_w, self.src_h) {
            return Err("video changed size mid-stream".into());
        }
        let fmt = frame.format();
        let dst_fmt = if self.hdr.is_some() {
            // 16-bit RGB keeps the HDR code values intact for the LUT pass
            // (swscale applies matrix + range only, never the transfer).
            ff::format::Pixel::RGBA64LE
        } else {
            ff::format::Pixel::RGBA
        };
        // (Re)create the scaler when the source pixel format materializes or
        // changes mid-stream (output geometry never moves).
        if self.scaler.as_ref().map(|(f, _)| *f) != Some(fmt) {
            let scaler = Scaler::get(
                fmt,
                self.src_w,
                self.src_h,
                dst_fmt,
                self.out_w,
                self.out_h,
                ScaleFlags::BILINEAR,
            )
            .map_err(|e| format!("FFmpeg scaler: {e}"))?;
            self.scaler = Some((fmt, scaler));
            // A recreated scaler needs the colorspace details re-asserted.
            if let Some(sc) = self.resolved {
                if let Some((_, s)) = self.scaler.as_mut() {
                    unsafe {
                        color::set_scaler_colorspace(s.as_mut_ptr(), sc.matrix, sc.full_range);
                    }
                }
            }
        }
        // First frame: resolve color, frame metadata over decoder's (plan §9
        // precedence), and teach swscale the real matrix + range.
        if self.resolved.is_none() {
            let pick_prim = non_unspec(frame.color_primaries(), self.dec_primaries, |v| {
                v == ff::color::Primaries::Unspecified
            });
            let pick_trc = non_unspec(
                frame.color_transfer_characteristic(),
                self.dec_transfer,
                |v| v == ff::color::TransferCharacteristic::Unspecified,
            );
            let pick_space = non_unspec(frame.color_space(), self.dec_space, |v| {
                v == ff::color::Space::Unspecified
            });
            let pick_range = non_unspec(frame.color_range(), self.dec_range, |v| {
                v == ff::color::Range::Unspecified
            });
            let sc = color::resolve(
                pick_prim, pick_trc, pick_space, pick_range, self.src_w, self.src_h,
            );
            if let Some((_, scaler)) = self.scaler.as_mut() {
                unsafe {
                    color::set_scaler_colorspace(scaler.as_mut_ptr(), sc.matrix, sc.full_range);
                }
            }
            self.hdr_matrix = color::linear_primaries_matrix(sc.primaries);
            self.resolved = Some(sc);
        }
        let (_, scaler) = self.scaler.as_mut().expect("created above");
        let mut out_frame = ff::frame::Video::empty();
        scaler
            .run(frame, &mut out_frame)
            .map_err(|e| format!("FFmpeg scale: {e}"))?;
        match self.hdr {
            None => {
                let rgba = tight_rgba(&out_frame);
                let (rgba, fw, fh) = rotate_rgba(rgba, self.out_w, self.out_h, self.rotation);
                Ok((rgba, fw, fh))
            }
            Some(curve) => {
                let f16 = self.pack_scrgb_f16(&out_frame, curve);
                let (f16, fw, fh) = rotate_bytes(f16, self.out_w, self.out_h, self.rotation, 8);
                Ok((f16, fw, fh))
            }
        }
    }

    /// HDR pack: RGBA64LE (transfer-encoded u16) → f16 scene-linear scRGB.
    /// LUT-driven (one 65536-entry table per clip) so the per-pixel cost is a
    /// lookup + a 3×3 multiply, not three `powf`s.
    fn pack_scrgb_f16(&mut self, frame: &ff::frame::Video, curve: Hdr) -> Vec<u8> {
        let lut = self.hdr_lut.get_or_insert_with(|| {
            (0..=u16::MAX)
                .map(|v| {
                    let e = v as f32 / 65535.0;
                    match curve {
                        Hdr::Pq => color::pq_to_scrgb(e),
                        Hdr::Hlg => color::hlg_to_scrgb(e),
                    }
                })
                .collect()
        });
        let m = self.hdr_matrix;
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let stride = frame.stride(0);
        let data = frame.data(0);
        let row_bytes = w * 8; // RGBA64: 4 × u16
        let one = half::f16::from_f32(1.0).to_le_bytes();
        let mut out = vec![0u8; w * h * 8]; // RGBA16F
        let mut peak = self.peak;
        for y in 0..h {
            let src = &data[y * stride..y * stride + row_bytes];
            let dst = &mut out[y * row_bytes..(y + 1) * row_bytes];
            for x in 0..w {
                let s = &src[x * 8..x * 8 + 8];
                let r = lut[u16::from_le_bytes([s[0], s[1]]) as usize];
                let g = lut[u16::from_le_bytes([s[2], s[3]]) as usize];
                let b = lut[u16::from_le_bytes([s[4], s[5]]) as usize];
                // Source-linear → sRGB-linear primaries.
                let ro = m[0][0] * r + m[0][1] * g + m[0][2] * b;
                let go = m[1][0] * r + m[1][1] * g + m[1][2] * b;
                let bo = m[2][0] * r + m[2][1] * g + m[2][2] * b;
                peak = peak.max(ro).max(go).max(bo);
                let d = &mut dst[x * 8..x * 8 + 8];
                d[0..2].copy_from_slice(&half::f16::from_f32(ro).to_le_bytes());
                d[2..4].copy_from_slice(&half::f16::from_f32(go).to_le_bytes());
                d[4..6].copy_from_slice(&half::f16::from_f32(bo).to_le_bytes());
                d[6..8].copy_from_slice(&one);
            }
        }
        self.peak = peak;
        out
    }
}

/// Frame value unless it's the format's Unspecified, else the decoder's.
fn non_unspec<T: Copy>(frame_v: T, dec_v: T, is_unspec: impl Fn(T) -> bool) -> T {
    if is_unspec(frame_v) {
        dec_v
    } else {
        frame_v
    }
}

/// Copy an `ff` RGBA frame out as tightly-packed straight-alpha RGBA8, honoring
/// the row stride (swscale pads rows to an alignment).
pub fn tight_rgba(frame: &ff::frame::Video) -> Vec<u8> {
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let stride = frame.stride(0);
    let data = frame.data(0);
    let row_bytes = w * 4;
    let mut out = vec![0u8; row_bytes * h];
    for y in 0..h {
        let src = &data[y * stride..y * stride + row_bytes];
        out[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(src);
    }
    out
}

/// [`rotate_rgba`] generalized to any per-pixel byte width (the fp16 path is
/// 8 bytes/px). Same clockwise quadrant mappings.
fn rotate_bytes(src: Vec<u8>, w: u32, h: u32, deg: i32, bpp: usize) -> (Vec<u8>, u32, u32) {
    let (w, h) = (w as usize, h as usize);
    let d = deg.rem_euclid(360);
    if d == 0 || w == 0 || h == 0 {
        return (src, w as u32, h as u32);
    }
    let (nw, nh) = if d == 180 { (w, h) } else { (h, w) };
    let mut out = vec![0u8; nw * nh * bpp];
    for dy in 0..nh {
        for dx in 0..nw {
            let (x, y) = match d {
                90 => (dy, nw - 1 - dx),
                180 => (w - 1 - dx, h - 1 - dy),
                _ => (nh - 1 - dy, dx), // 270 (nh == w)
            };
            let si = (y * w + x) * bpp;
            let di = (dy * nw + dx) * bpp;
            out[di..di + bpp].copy_from_slice(&src[si..si + bpp]);
        }
    }
    (out, nw as u32, nh as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_bytes_matches_rotate_rgba_at_4bpp_and_handles_8bpp() {
        // A 2×3 grid of distinct 4-byte pixels: both rotators must agree.
        let src: Vec<u8> = (0..2 * 3 * 4).map(|i| i as u8).collect();
        for deg in [0, 90, 180, 270] {
            let (a, aw, ah) = rotate_rgba(src.clone(), 2, 3, deg);
            let (b, bw, bh) = rotate_bytes(src.clone(), 2, 3, deg, 4);
            assert_eq!((aw, ah), (bw, bh), "{deg}°");
            assert_eq!(a, b, "{deg}°");
        }
        // 8 bpp 90°: the 2×1 image [P0 P1] becomes 1×2 [P0 / P1] rotated CW —
        // P0 (top-left) moves to the top-right = the only column's top.
        let p0 = [0u8; 8];
        let p1 = [1u8; 8];
        let src: Vec<u8> = [p0, p1].concat();
        let (out, w, h) = rotate_bytes(src, 2, 1, 90, 8);
        assert_eq!((w, h), (1, 2));
        assert_eq!(&out[0..8], &p0, "left pixel rotates to the top");
        assert_eq!(&out[8..16], &p1);
    }
}
