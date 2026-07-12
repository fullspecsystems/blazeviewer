//! Decoded `AVFrame` → tightly-packed, fitted, upright RGBA8 — the conversion
//! stage the poster and the producer share, so geometry / rotation / color
//! policy is one implementation (poster ≡ playback by construction, the same
//! guarantee the Windows MF pair gets from sharing a reader config).
//!
//! Decode-to-fit happens **inside swscale** (scale during the YUV→RGB pass we
//! already run — the `ff_live` measurement: a separate Lanczos pass was ~44% of
//! decode time), with the swscale coefficients set from the source's real
//! matrix + range (see [`super::color::set_scaler_colorspace`]). Rotation
//! (display matrix) is applied after, on the small frame.

use ff::software::scaling::{Context as Scaler, Flags as ScaleFlags};
use ffmpeg_next as ff;

use super::color::{self, SourceColor};
use crate::common::rotate_rgba;

/// Lazily-initialized converter: swscale needs the first real frame's pixel
/// format (decoders may refine format/color after the first packet), so
/// construction records the *plan* (output geometry, rotation, the
/// decoder-reported color as fallback) and the first [`convert`](Self::convert)
/// resolves the rest.
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
    /// Decoder-reported color enums — the fallback when frames don't say.
    dec_primaries: ff::color::Primaries,
    dec_transfer: ff::color::TransferCharacteristic,
    dec_space: ff::color::Space,
    dec_range: ff::color::Range,
    /// Resolved at the first frame (frame metadata wins over decoder's).
    resolved: Option<SourceColor>,
    scaler: Option<(ff::format::Pixel, Scaler)>,
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
        FrameConverter {
            src_w: src.0,
            src_h: src.1,
            out_w: out.0.max(1),
            out_h: out.1.max(1),
            rotation,
            dec_primaries: decoder.color_primaries(),
            dec_transfer: decoder.color_transfer_characteristic(),
            dec_space: decoder.color_space(),
            dec_range: decoder.color_range(),
            resolved: None,
            scaler: None,
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

    /// Convert one decoded frame to tightly-packed upright RGBA8 at the fixed
    /// output geometry. Errors on a mid-stream size change (clean failure per
    /// plan §5). Returns `(rgba, display_w, display_h)`.
    pub fn convert(&mut self, frame: &ff::frame::Video) -> Result<(Vec<u8>, u32, u32), String> {
        if (frame.width(), frame.height()) != (self.src_w, self.src_h) {
            return Err("video changed size mid-stream".into());
        }
        let fmt = frame.format();
        // (Re)create the scaler when the source pixel format materializes or
        // changes mid-stream (output geometry never moves).
        if self.scaler.as_ref().map(|(f, _)| *f) != Some(fmt) {
            let scaler = Scaler::get(
                fmt,
                self.src_w,
                self.src_h,
                ff::format::Pixel::RGBA,
                self.out_w,
                self.out_h,
                ScaleFlags::BILINEAR,
            )
            .map_err(|e| format!("FFmpeg scaler: {e}"))?;
            self.scaler = Some((fmt, scaler));
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
            self.resolved = Some(sc);
        }
        let (_, scaler) = self.scaler.as_mut().expect("created above");
        let mut rgba_frame = ff::frame::Video::empty();
        scaler
            .run(frame, &mut rgba_frame)
            .map_err(|e| format!("FFmpeg scale: {e}"))?;
        let rgba = tight_rgba(&rgba_frame);
        let (rgba, fw, fh) = rotate_rgba(rgba, self.out_w, self.out_h, self.rotation);
        Ok((rgba, fw, fh))
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
