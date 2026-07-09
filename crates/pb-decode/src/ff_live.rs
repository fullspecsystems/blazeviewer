//! Linux **Live Photo** motion + audio decoder, via FFmpeg (libavcodec / libavformat /
//! libswscale through the `ffmpeg-next` crate). The Linux mirror of the
//! Windows Media Foundation (`mf_video.rs`) and macOS AVFoundation (`livephoto.rs`)
//! backends, behind the same [`decode_live_motion`] seam.
//!
//! Feature-gated (`livephoto`, off by default) exactly like the `libheif` HEIC backend:
//! it links a native C toolchain (system FFmpeg — `apt install libavcodec-dev …`), which
//! must never block the pure-Rust core (ADR-015). H.264 and HEVC both decode through the
//! OS FFmpeg build, so no per-codec Rust.
//!
//! - **Video** ([`decode_live_motion`]): decode each frame, swscale YUV→RGBA at native
//!   size, apply the container's display-matrix rotation (portrait iPhone clips), then the
//!   shared Lanczos `downscale_to_fit` caps the long edge (decode-to-fit → bounds RAM), and
//!   true per-sample timestamps drive each frame's delay. Output is the same [`Animation`]
//!   the animated-image playback consumes.
//! - **Audio** ([`decode_motion_audio`]): decode the companion `.mov`'s audio track to
//!   channel-interleaved f32 PCM (frames converted by hand — see [`append_interleaved_f32`] —
//!   to sidestep FFmpeg 8's swresample channel-layout churn), for the Linux [`LiveAudio`]
//!   backend to play through `rodio`.
//!
//! Reads only, RAM-only — the no-trace guarantee (privacy #2) holds.

use std::path::Path;
use std::time::Duration;

use ff::format::Pixel;
use ff::media::Type;
use ff::software::scaling::{Context as Scaler, Flags as ScaleFlags};
use ffmpeg_next as ff;

use crate::{AnimFrame, Animation, AnimationKind, ColorTransform, DecodeError};

/// Safety cap on decoded frames — a real Live Photo is ~3 s, so this is only a runaway
/// guard (a malformed/long `.mov`). Hitting it flags the result truncated. Mirrors the
/// Windows/macOS backends' cap.
const MAX_MOTION_FRAMES: usize = 600;

/// Fallback per-frame delay when a frame carries no usable timestamp (30 fps).
const FALLBACK_DELAY: Duration = Duration::from_nanos(33_333_333);

/// Process-wide FFmpeg init (registers codecs/formats). Idempotent + cheap.
fn ff_init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = ff::init();
        // Quiet libav's stderr logging — a bad frame shouldn't spew to the console.
        ff::util::log::set_level(ff::util::log::Level::Fatal);
    });
}

/// Decode the Live Photo motion `.mov` at `path` into an [`Animation`], capping each
/// frame's long edge to `max_long_edge` px (decode-to-fit → bounds RAM). Linux-only.
pub fn decode_live_motion(path: &Path, max_long_edge: u32) -> Result<Animation, DecodeError> {
    ff_init();
    decode_video_inner(path, max_long_edge)
        .map_err(|e| DecodeError::Corrupt(format!("FFmpeg: {e}")))?
}

/// Interleaved f32 PCM decoded from a Live Photo's motion `.mov` audio track, for the
/// Linux [`LiveAudio`] backend (`rodio` plays it). `samples` is channel-interleaved.
pub struct MotionAudio {
    pub samples: Vec<f32>,
    pub channels: u16,
    pub sample_rate: u32,
}

/// Decode the `.mov`'s audio track to interleaved f32 PCM. Linux-only; feature-gated.
pub fn decode_motion_audio(path: &Path) -> Result<MotionAudio, DecodeError> {
    ff_init();
    decode_audio_inner(path).map_err(|e| DecodeError::Corrupt(format!("FFmpeg audio: {e}")))?
}

// ── Video ──────────────────────────────────────────────────────────────────────────

fn decode_video_inner(
    path: &Path,
    max_long_edge: u32,
) -> Result<Result<Animation, DecodeError>, ff::Error> {
    let mut ictx = ff::format::input(&path)?;
    let stream = ictx
        .streams()
        .best(Type::Video)
        .ok_or(ff::Error::StreamNotFound)?;
    let vindex = stream.index();
    let time_base = f64::from(stream.time_base());
    let rotation = rotation_degrees(&stream);

    let ctx = ff::codec::context::Context::from_parameters(stream.parameters())?;
    let mut decoder = ctx.decoder().video()?;
    let (dw, dh) = (decoder.width(), decoder.height());
    if dw == 0 || dh == 0 {
        return Ok(Err(DecodeError::Corrupt(
            "Live Photo motion has no video frames".into(),
        )));
    }

    // Decode-to-fit **in swscale**: scale straight to the target during the YUV→RGBA pass
    // (cheap, SIMD, one pass we already run) instead of decoding at native res and then doing a
    // separate per-frame Lanczos downscale — which measured ~44% of decode time. Rotation only
    // swaps w/h, so the rotated long edge is `max(dw, dh)` either way; pick the scale that lands
    // it within `max_long_edge` (never upscale). The rotate below then runs on the smaller frame.
    let long = dw.max(dh);
    let s = if max_long_edge > 0 && long > max_long_edge {
        max_long_edge as f64 / long as f64
    } else {
        1.0
    };
    let sw = ((dw as f64 * s).round() as u32).max(1);
    let sh = ((dh as f64 * s).round() as u32).max(1);
    let mut scaler = Scaler::get(
        decoder.format(),
        dw,
        dh,
        Pixel::RGBA,
        sw,
        sh,
        ScaleFlags::BILINEAR,
    )?;

    let mut frames: Vec<AnimFrame> = Vec::new();
    let mut timestamps: Vec<f64> = Vec::new();
    let mut truncated = false;

    // Pull every RGBA frame out of the decoder into `frames`, honoring the cap.
    let drain = |decoder: &mut ff::decoder::Video,
                 scaler: &mut Scaler,
                 frames: &mut Vec<AnimFrame>,
                 timestamps: &mut Vec<f64>|
     -> Result<bool, ff::Error> {
        let mut decoded = ff::frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if frames.len() >= MAX_MOTION_FRAMES {
                return Ok(true); // truncated
            }
            let mut rgba_frame = ff::frame::Video::empty();
            scaler.run(&decoded, &mut rgba_frame)?;
            let rgba = tight_rgba(&rgba_frame);
            // swscale already scaled to fit, so rotate is the only post-step (no Lanczos pass).
            let (rgba, fw, fh) = rotate_rgba(rgba, sw, sh, rotation);
            let ts = decoded.timestamp().map(|t| t as f64 * time_base);
            timestamps.push(ts.unwrap_or(f64::NAN));
            frames.push(AnimFrame {
                rgba,
                width: fw,
                height: fh,
                delay: FALLBACK_DELAY, // real gaps assigned below
            });
        }
        Ok(false)
    };

    for (s, packet) in ictx.packets() {
        if s.index() != vindex {
            continue;
        }
        decoder.send_packet(&packet)?;
        if drain(&mut decoder, &mut scaler, &mut frames, &mut timestamps)? {
            truncated = true;
            break;
        }
    }
    if !truncated {
        decoder.send_eof()?;
        truncated = drain(&mut decoder, &mut scaler, &mut frames, &mut timestamps)?;
    }

    if frames.is_empty() {
        return Ok(Err(DecodeError::Corrupt(
            "Live Photo motion decoded no frames".into(),
        )));
    }

    // True per-frame pacing: frame i shows until frame i+1's timestamp. Fall back to the
    // previous gap for a missing/non-monotonic stamp (last frame reuses the prior gap).
    let mut last_gap = FALLBACK_DELAY;
    for (i, frame) in frames.iter_mut().enumerate() {
        let gap = match (timestamps.get(i), timestamps.get(i + 1)) {
            (Some(a), Some(b)) if a.is_finite() && b.is_finite() && b > a => {
                Duration::from_secs_f64((b - a).min(2.0))
            }
            _ => last_gap,
        };
        frame.delay = gap;
        last_gap = gap;
    }

    let (width, height) = (frames[0].width, frames[0].height);
    Ok(Ok(Animation {
        kind: AnimationKind::LivePhoto,
        width,
        height,
        frames,
        // Plays once and stops (finite loop = 1), like the Windows/macOS backends.
        loop_count: 1,
        codec: "Live Photo",
        // swscale's RGBA is nominal BT.709 (sRGB primaries/curve to within a hair) — pass
        // through, matching the Windows Media Foundation backend.
        color: ColorTransform::srgb(),
        truncated,
    }))
}

/// Copy an `ff` RGBA frame out as tightly-packed straight-alpha RGBA8, honoring the row
/// stride (swscale pads rows to an alignment).
fn tight_rgba(frame: &ff::frame::Video) -> Vec<u8> {
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

/// Rotate a tightly-packed RGBA8 buffer by `deg` (0/90/180/270, clockwise), returning the
/// rotated buffer and its new dimensions. Portrait iPhone Live Photos carry a 90°/270° display
/// matrix; ignoring it shows them sideways.
///
/// Copies whole 4-byte pixels via raw pointers, iterating the destination in row order
/// (sequential writes) — this runs on every motion frame and was ~24% of decode time as a
/// bounds-checked scalar loop building a `[u8; 4]` per pixel.
fn rotate_rgba(src: Vec<u8>, w: u32, h: u32, deg: i32) -> (Vec<u8>, u32, u32) {
    let (w, h) = (w as usize, h as usize);
    let d = deg.rem_euclid(360);
    if d == 0 || w == 0 || h == 0 {
        return (src, w as u32, h as u32);
    }
    // 90°/270° swap the axes; 180° keeps them.
    let (nw, nh) = if d == 180 { (w, h) } else { (h, w) };
    let mut out = vec![0u8; nw * nh * 4];
    let sp = src.as_ptr();
    let dp = out.as_mut_ptr();
    for dy in 0..nh {
        for dx in 0..nw {
            // dst (dx,dy) ← src (x,y): each clockwise mapping, inverted. nw==h for 90/270.
            let (x, y) = match d {
                90 => (dy, nw - 1 - dx),
                180 => (w - 1 - dx, h - 1 - dy),
                _ => (nh - 1 - dy, dx), // 270 (nh == w)
            };
            let si = (y * w + x) * 4;
            let di = (dy * nw + dx) * 4;
            // SAFETY: by construction x<w, y<h ⇒ si+4 ≤ w·h·4 = src.len(); dx<nw, dy<nh ⇒
            // di+4 ≤ nw·nh·4 = out.len(). Ranges never overlap the ends.
            unsafe {
                std::ptr::copy_nonoverlapping(sp.add(si), dp.add(di), 4);
            }
        }
    }
    (out, nw as u32, nh as u32)
}

/// The clockwise display rotation (0/90/180/270) from the video stream's DISPLAYMATRIX
/// side data, if any. iPhone portrait clips carry 90°/270°. Returns 0 when absent.
fn rotation_degrees(stream: &ff::format::stream::Stream) -> i32 {
    use ffmpeg_next::ffi;
    unsafe {
        let par = (*stream.as_ptr()).codecpar;
        if par.is_null() {
            return 0;
        }
        let sd = ffi::av_packet_side_data_get(
            (*par).coded_side_data,
            (*par).nb_coded_side_data,
            ffi::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
        );
        if sd.is_null() || (*sd).data.is_null() {
            return 0;
        }
        // av_display_rotation_get returns the CCW angle in degrees; negate for CW, then
        // snap to the nearest quadrant.
        let angle = ffi::av_display_rotation_get((*sd).data as *const i32);
        if angle.is_nan() {
            return 0;
        }
        let cw = (-angle).round() as i32;
        ((cw % 360) + 360) % 360
    }
}

// ── Audio ──────────────────────────────────────────────────────────────────────────

fn decode_audio_inner(path: &Path) -> Result<Result<MotionAudio, DecodeError>, ff::Error> {
    let mut ictx = ff::format::input(&path)?;
    let stream = match ictx.streams().best(Type::Audio) {
        Some(s) => s,
        None => {
            return Ok(Err(DecodeError::Corrupt(
                "Live Photo motion has no audio track".into(),
            )))
        }
    };
    let aindex = stream.index();
    let ctx = ff::codec::context::Context::from_parameters(stream.parameters())?;
    let mut decoder = ctx.decoder().audio()?;

    // Convert decoded frames to interleaved f32 PCM **by hand** rather than via swresample:
    // under FFmpeg 8 (libavcodec 62) the deprecated `channel_layout()` accessor reports an
    // empty layout, so a swresample context built from it rejects the real frames with
    // `AVERROR_INPUT_CHANGED`. Live Photo audio is AAC-LC (planar float), and manual
    // interleaving sidesteps the channel-layout API churn entirely.
    let mut channels: u16 = 0;
    let mut sample_rate: u32 = 0;
    let mut samples: Vec<f32> = Vec::new();
    let mut unsupported: Option<String> = None;

    let mut pump = |decoder: &mut ff::decoder::Audio| {
        let mut decoded = ff::frame::Audio::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if channels == 0 {
                channels = decoded.channels().max(1);
                sample_rate = decoded.rate();
            }
            if let Err(fmt) = append_interleaved_f32(&decoded, channels as usize, &mut samples) {
                unsupported.get_or_insert(fmt);
            }
        }
    };

    for (s, packet) in ictx.packets() {
        if s.index() != aindex {
            continue;
        }
        decoder.send_packet(&packet)?;
        pump(&mut decoder);
    }
    decoder.send_eof()?;
    pump(&mut decoder);

    if let Some(fmt) = unsupported {
        return Ok(Err(DecodeError::Corrupt(format!(
            "Live Photo audio in unsupported sample format: {fmt}"
        ))));
    }
    if samples.is_empty() || channels == 0 || sample_rate == 0 {
        return Ok(Err(DecodeError::Corrupt(
            "Live Photo motion decoded no audio".into(),
        )));
    }
    Ok(Ok(MotionAudio {
        samples,
        channels,
        sample_rate,
    }))
}

/// Convert one decoded audio frame to channel-interleaved f32 (`[-1, 1]`), appending to
/// `out`. Handles the sample formats a Live Photo `.mov` realistically produces (AAC → FLTP)
/// plus the common integer/packed variants. Returns `Err(format-name)` for anything else.
/// Bytes are read with `from_le_bytes` (not a `cast_slice`) so no alignment assumption is made.
fn append_interleaved_f32(
    frame: &ff::frame::Audio,
    ch: usize,
    out: &mut Vec<f32>,
) -> Result<(), String> {
    use ff::format::sample::{Sample, Type};
    let n = frame.samples();
    if ch == 0 || n == 0 {
        return Ok(());
    }
    // Read the i-th sample of channel `c` from a plane, given the per-sample stride in bytes
    // and a byte→f32 decoder. Planar: one plane per channel; packed: plane 0, channels
    // interleaved.
    let planar = matches!(
        frame.format(),
        Sample::U8(Type::Planar)
            | Sample::I16(Type::Planar)
            | Sample::I32(Type::Planar)
            | Sample::F32(Type::Planar)
    );
    let sample_bytes = match frame.format() {
        Sample::U8(_) => 1,
        Sample::I16(_) => 2,
        Sample::I32(_) | Sample::F32(_) => 4,
        other => return Err(format!("{other:?}")),
    };
    let decode = |b: &[u8]| -> f32 {
        match frame.format() {
            Sample::U8(_) => (b[0] as f32 - 128.0) / 128.0,
            Sample::I16(_) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            Sample::I32(_) => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            _ => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        }
    };
    out.reserve(n * ch);
    for i in 0..n {
        for c in 0..ch {
            let (plane, idx) = if planar { (c, i) } else { (0, i * ch + c) };
            let data = frame.data(plane);
            let off = idx * sample_bytes;
            if off + sample_bytes <= data.len() {
                out.push(decode(&data[off..off + sample_bytes]));
            } else {
                out.push(0.0);
            }
        }
    }
    Ok(())
}
