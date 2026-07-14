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
    /// Whether to attempt the planar GPU color path (task #91 Phase 2). Gated
    /// further per-frame on rotation == 0 and a 4:2:0 source; a 10-bit source also
    /// needs `allow_p010`. When it doesn't apply, the RGBA8/fp16 path runs.
    attempt_planar: bool,
    /// Whether the renderer can display P010 (10-bit). `false` → 10-bit sources
    /// take the RGBA/fp16 fallback even when `attempt_planar`.
    allow_p010: bool,
    /// The negotiated output format, decided once on the first frame from the
    /// actual (post-HW-transfer) pixel format (Codex P0). `None` until then; then
    /// one of `Nv12` / `P010` (planar path) or `Rgba8` / `Rgba16F` (fallback).
    planar_out: Option<PixelFormat>,
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
        attempt_planar: bool,
        allow_p010: bool,
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
            // Planar is only ever eligible for upright clips in v1 (rotation is
            // baked into the RGBA path via rotate_*; planar rotation-in-geometry is
            // the documented follow-on). Rotated clips keep the parallel RGBA path.
            attempt_planar: attempt_planar && rotation % 360 == 0,
            allow_p010,
            planar_out: None,
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

    /// What [`convert`](Self::convert) emits — the negotiated format. Before the
    /// first frame it is the *default* (fp16 for HDR, RGBA8 for SDR), used only by
    /// callers that don't prime; after the first frame it is the resolved format
    /// (planar `Nv12`/`P010` when eligible, else the RGBA default). Fixed for the
    /// clip once resolved (Codex P0: negotiated from the actual decoded frame).
    pub fn output_format(&self) -> PixelFormat {
        self.planar_out.unwrap_or_else(|| self.rgba_default())
    }

    /// The non-planar fallback format for this clip's HDR-ness.
    fn rgba_default(&self) -> PixelFormat {
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
        // Negotiate the output format once, from the ACTUAL decoded pixel format
        // (Codex P0 — it isn't reliably known at open, esp. after HW transfer).
        if self.planar_out.is_none() {
            self.planar_out = Some(self.negotiate_format(fmt));
        }
        let out_fmt = self.planar_out.expect("negotiated above");
        let planar_ten = match out_fmt {
            PixelFormat::Nv12 => Some(false),
            PixelFormat::P010 => Some(true),
            _ => None,
        };
        let dst_fmt = match out_fmt {
            PixelFormat::Nv12 => ff::format::Pixel::NV12,
            PixelFormat::P010 => ff::format::Pixel::P010LE,
            // 16-bit RGB keeps the HDR code values intact for the LUT pass
            // (swscale applies matrix + range only, never the transfer).
            PixelFormat::Rgba16F => ff::format::Pixel::RGBA64LE,
            PixelFormat::Rgba8 => ff::format::Pixel::RGBA,
        };
        let planar = planar_ten.is_some();
        // (Re)create the scaler when the source pixel format materializes or
        // changes mid-stream (output geometry / negotiated format never move).
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
                self.apply_scaler_colorspace(sc, planar);
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
            self.apply_scaler_colorspace(sc, planar);
            self.hdr_matrix = color::linear_primaries_matrix(sc.primaries);
            self.resolved = Some(sc);
        }
        let (_, scaler) = self.scaler.as_mut().expect("created above");
        let mut out_frame = ff::frame::Video::empty();
        scaler
            .run(frame, &mut out_frame)
            .map_err(|e| format!("FFmpeg scale: {e}"))?;
        // Planar (the task #91 Phase 2 fast path): tight-pack Y + interleaved UV
        // and hand off raw — the GPU does YUV + range + transfer + primaries. No
        // CPU color pass (R6), no per-frame thread fan-out (R8), no rotate (the
        // path is gated on rotation == 0).
        if let Some(ten) = planar_ten {
            return Ok((tight_planar(&out_frame, ten), self.out_w, self.out_h));
        }
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

    /// Decide the output format for this clip from the first frame's actual pixel
    /// format (task #91 Phase 2). Planar (NV12 8-bit / P010 10-bit) for an
    /// eligible 4:2:0 source when [`attempt_planar`](Self::attempt_planar) and (for
    /// 10-bit) [`allow_p010`](Self::allow_p010); otherwise the RGBA/fp16 fallback.
    /// 12-bit and non-4:2:0 (4:2:2 / 4:4:4 / alpha) sources fall back to preserve
    /// precision/chroma.
    fn negotiate_format(&self, fmt: ff::format::Pixel) -> PixelFormat {
        if self.attempt_planar {
            match planar_kind(fmt) {
                Some(false) => return PixelFormat::Nv12,
                Some(true) if self.allow_p010 => return PixelFormat::P010,
                _ => {}
            }
        }
        self.rgba_default()
    }

    /// Teach the scaler the source matrix + range: value-preserving YUV→YUV for
    /// the planar path (the renderer converts), or YUV→full-range-RGB otherwise.
    fn apply_scaler_colorspace(&mut self, sc: SourceColor, planar: bool) {
        if let Some((_, s)) = self.scaler.as_mut() {
            unsafe {
                if planar {
                    color::set_planar_scaler_colorspace(s.as_mut_ptr(), sc.matrix, sc.full_range);
                } else {
                    color::set_scaler_colorspace(s.as_mut_ptr(), sc.matrix, sc.full_range);
                }
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
        let lut: &[f32] = lut;
        let m = self.hdr_matrix;
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let stride = frame.stride(0);
        let data = frame.data(0);
        let row_bytes = w * 8; // RGBA64: 4 × u16
        let one = half::f16::from_f32(1.0).to_le_bytes();
        let mut out = vec![0u8; w * h * 8]; // RGBA16F

        // This LUT + 3×3 matrix + f16 pack is the dominant per-frame HDR cost and
        // scales with output area — single-threaded it crossed the frame budget on
        // a large window (the playback stutter / queue rebuffering). Rows are
        // independent, so fan them out across cores in row-bands.
        // Leave a couple of cores for the (main-thread) audio decoder + event loop:
        // saturating every core during a post-seek convert burst starves a heavy
        // audio codec (TrueHD/Atmos) and crackles. Mirrors the decode pool's
        // "cores − 2" budget.
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(2))
            .unwrap_or(1)
            .clamp(1, h.max(1));
        let rows_per = h.div_ceil(threads);
        let base_peak = self.peak;
        let one_pixel = |src: &[u8], dst: &mut [u8], peak: &mut f32| {
            let r = lut[u16::from_le_bytes([src[0], src[1]]) as usize];
            let g = lut[u16::from_le_bytes([src[2], src[3]]) as usize];
            let b = lut[u16::from_le_bytes([src[4], src[5]]) as usize];
            // Source-linear → sRGB-linear primaries.
            let ro = m[0][0] * r + m[0][1] * g + m[0][2] * b;
            let go = m[1][0] * r + m[1][1] * g + m[1][2] * b;
            let bo = m[2][0] * r + m[2][1] * g + m[2][2] * b;
            *peak = peak.max(ro).max(go).max(bo);
            dst[0..2].copy_from_slice(&half::f16::from_f32(ro).to_le_bytes());
            dst[2..4].copy_from_slice(&half::f16::from_f32(go).to_le_bytes());
            dst[4..6].copy_from_slice(&half::f16::from_f32(bo).to_le_bytes());
            dst[6..8].copy_from_slice(&one);
        };
        let frame_peak = std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(threads);
            let mut rest: &mut [u8] = &mut out;
            let mut y0 = 0usize;
            while y0 < h {
                let band_rows = rows_per.min(h - y0);
                let (band, tail) = rest.split_at_mut(band_rows * row_bytes);
                rest = tail;
                let start = y0;
                handles.push(s.spawn(move || {
                    let mut peak = 0f32;
                    for i in 0..band_rows {
                        let src = &data[(start + i) * stride..(start + i) * stride + row_bytes];
                        let dst = &mut band[i * row_bytes..(i + 1) * row_bytes];
                        for x in 0..w {
                            one_pixel(
                                &src[x * 8..x * 8 + 8],
                                &mut dst[x * 8..x * 8 + 8],
                                &mut peak,
                            );
                        }
                    }
                    peak
                }));
                y0 += band_rows;
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or(0.0))
                .fold(base_peak, f32::max)
        });
        self.peak = frame_peak;
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

/// The planar-video eligibility of a decoded pixel format: `Some(ten_bit)` for a
/// 4:2:0 8-bit (`false`) or 10-bit (`true`) source, `None` otherwise (4:2:2 /
/// 4:4:4 / alpha / 12-bit — those take the RGBA/fp16 fallback so no chroma or
/// precision is lost). Covers the real HW-transfer and software-decode outputs.
fn planar_kind(fmt: ff::format::Pixel) -> Option<bool> {
    use ff::format::Pixel::*;
    // NB: the glob brings `Pixel::None` into scope — qualify `Option::None`.
    match fmt {
        YUV420P | YUVJ420P | NV12 | NV21 => Some(false),
        YUV420P10LE | P010LE => Some(true),
        _ => Option::None,
    }
}

/// Copy an `ff` planar 4:2:0 frame (NV12 8-bit / P010 16-bit) out as the tight
/// `[Y plane][interleaved UV plane]` layout the renderer's planar path expects,
/// honoring each plane's row stride (swscale pads rows). `ten` selects 2 bytes
/// per sample (P010) vs 1 (NV12). The chroma plane is half-height, full-width
/// (interleaved U,V), matching [`PixelFormat::frame_bytes`].
fn tight_planar(frame: &ff::frame::Video, ten: bool) -> Vec<u8> {
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let bps = if ten { 2 } else { 1 };
    let y_row = w * bps;
    let uv_row = w * bps; // ⌊w/2⌋ UV pairs × 2 samples × bps = w·bps
    let uv_h = h / 2;
    let (y_stride, uv_stride) = (frame.stride(0), frame.stride(1));
    let (y_data, uv_data) = (frame.data(0), frame.data(1));
    let mut out = vec![0u8; y_row * h + uv_row * uv_h];
    for r in 0..h {
        out[r * y_row..(r + 1) * y_row]
            .copy_from_slice(&y_data[r * y_stride..r * y_stride + y_row]);
    }
    let base = y_row * h;
    for r in 0..uv_h {
        let d = base + r * uv_row;
        out[d..d + uv_row].copy_from_slice(&uv_data[r * uv_stride..r * uv_stride + uv_row]);
    }
    out
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
