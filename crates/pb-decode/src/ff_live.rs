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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ff::format::Pixel;
use ff::media::Type;
use ff::software::scaling::{Context as Scaler, Flags as ScaleFlags};
use ffmpeg_next as ff;

use crate::animation::MAX_DECODED_BYTES;
use crate::common::rotate_rgba;
use crate::ffmpeg::pcm::append_interleaved_f32;
use crate::{
    AnimFrame, Animation, AnimationKind, ColorTransform, DecodeError, MotionChunk, MotionHeader,
};

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
    decode_live_motion_cancellable(path, max_long_edge, &AtomicBool::new(false))
}

/// Like [`decode_live_motion`], but bails early (with an error, discarded by the caller) once
/// `cancel` is set — so navigating away from a Live Photo stops its in-flight `.mov` decode
/// instead of running it to completion on an orphaned thread (wasted CPU + transient RAM).
pub fn decode_live_motion_cancellable(
    path: &Path,
    max_long_edge: u32,
    cancel: &AtomicBool,
) -> Result<Animation, DecodeError> {
    ff_init();
    // A Live Photo plays its motion once and reverts to the still (finite loop = 1).
    decode_video_inner(path, max_long_edge, cancel, VideoKind::live())
        .map_err(|e| DecodeError::Corrupt(format!("FFmpeg: {e}")))?
}

/// Decode an animated ISOBMFF image **sequence** (animated AVIF `avis` / HEIC `msf1`) at
/// `path` into an [`Animation`], via the same FFmpeg video pipeline as Live Photo motion.
/// Linux's answer to the macOS Image I/O sequence backend — there's no pure-Rust AV1/HEVC
/// sequence decoder, so we reuse FFmpeg (already linked for `livephoto`). Loops like a GIF
/// (`loop_count = 0`), so it plays until the user pauses/navigates rather than reverting.
pub fn decode_image_sequence(path: &Path, max_long_edge: u32) -> Result<Animation, DecodeError> {
    decode_image_sequence_cancellable(path, max_long_edge, &AtomicBool::new(false))
}

/// Cancellable [`decode_image_sequence`] — bails early (discarded error) once `cancel` is set.
pub fn decode_image_sequence_cancellable(
    path: &Path,
    max_long_edge: u32,
    cancel: &AtomicBool,
) -> Result<Animation, DecodeError> {
    ff_init();
    decode_video_inner(path, max_long_edge, cancel, VideoKind::sequence())
        .map_err(|e| DecodeError::Corrupt(format!("FFmpeg: {e}")))?
}

/// How to label a decoded FFmpeg video sequence — a Live Photo's motion (plays once, reverts
/// to the still) vs an animated image sequence (loops like a GIF).
struct VideoKind {
    kind: AnimationKind,
    loop_count: u32,
    codec: &'static str,
}

impl VideoKind {
    fn live() -> Self {
        Self {
            kind: AnimationKind::LivePhoto,
            loop_count: 1,
            codec: "Live Photo",
        }
    }
    fn sequence() -> Self {
        Self {
            kind: AnimationKind::Heif,
            loop_count: 0, // animated AVIF/HEIC loops (GIF convention)
            codec: "AVIF",
        }
    }
}

// ── Streaming video (task #69: play while decoding) ──────────────────────────────────
// `MotionChunk`/`MotionHeader` (the message vocabulary + its state-machine contract) live
// in `crate::animation` — shared with the macOS AVAssetReader backend.

/// Streaming counterpart of [`decode_live_motion`] (task #69): decode the `.mov` at `path`
/// and hand each frame to `emit` as it's ready, rather than returning the whole [`Animation`]
/// at the end. Emits exactly one [`Header`](MotionChunk::Header), then a [`Frame`](
/// MotionChunk::Frame) per decoded frame, then a terminal [`Done`](MotionChunk::Done) or
/// [`Failed`](MotionChunk::Failed). Bails (emitting nothing further) once `cancel` is set.
///
/// Per-frame pacing uses the stream's nominal frame rate (Live Photos are constant-fps), so a
/// frame can be emitted immediately without waiting to see the next frame's timestamp.
pub fn decode_live_motion_streaming(
    path: &Path,
    max_long_edge: u32,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(MotionChunk),
) {
    ff_init();
    if let Err(e) = stream_video_inner(path, max_long_edge, cancel, emit) {
        emit(MotionChunk::Failed(DecodeError::Corrupt(format!(
            "FFmpeg: {e}"
        ))));
    }
}

fn stream_video_inner(
    path: &Path,
    max_long_edge: u32,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(MotionChunk),
) -> Result<(), ff::Error> {
    let mut ictx = ff::format::input(&path)?;
    let stream = ictx
        .streams()
        .best(Type::Video)
        .ok_or(ff::Error::StreamNotFound)?;
    let vindex = stream.index();
    let rotation = rotation_degrees(&stream);
    // Nominal per-frame delay from the stream's average frame rate (constant-fps clip).
    let fr = stream.avg_frame_rate();
    let nominal = if fr.numerator() > 0 && fr.denominator() > 0 {
        Duration::from_secs_f64(fr.denominator() as f64 / fr.numerator() as f64)
    } else {
        FALLBACK_DELAY
    };

    let ctx = ff::codec::context::Context::from_parameters(stream.parameters())?;
    let mut decoder = ctx.decoder().video()?;
    let (dw, dh) = (decoder.width(), decoder.height());
    if dw == 0 || dh == 0 {
        emit(MotionChunk::Failed(DecodeError::Corrupt(
            "Live Photo motion has no video frames".into(),
        )));
        return Ok(());
    }

    // Same decode-to-fit-in-swscale scale as the batch path.
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

    // Display dims are post-rotation (90°/270° swap the axes).
    let (disp_w, disp_h) = if rotation == 90 || rotation == 270 {
        (sh, sw)
    } else {
        (sw, sh)
    };
    emit(MotionChunk::Header(MotionHeader {
        width: disp_w,
        height: disp_h,
        // swscale RGBA is nominal BT.709 (≈ sRGB) — pass through, like the batch path.
        color: ColorTransform::srgb(),
        codec: "Live Photo",
    }));

    let mut count: usize = 0;
    let mut bytes: u64 = 0;
    let mut truncated = false;
    // Pull every ready RGBA frame out of the decoder and emit it. Returns whether a cap hit.
    let mut drain = |decoder: &mut ff::decoder::Video,
                     scaler: &mut Scaler,
                     emit: &mut dyn FnMut(MotionChunk)|
     -> Result<bool, ff::Error> {
        let mut decoded = ff::frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            if count >= MAX_MOTION_FRAMES {
                return Ok(true);
            }
            let mut rgba_frame = ff::frame::Video::empty();
            scaler.run(&decoded, &mut rgba_frame)?;
            let rgba = tight_rgba(&rgba_frame);
            let (rgba, fw, fh) = rotate_rgba(rgba, sw, sh, rotation);
            // Byte budget, checked *before* emitting: the consumer retains every frame,
            // so stop when the next frame would push it past the cap.
            let next_bytes = bytes.saturating_add(rgba.len() as u64);
            if next_bytes > MAX_DECODED_BYTES {
                return Ok(true);
            }
            bytes = next_bytes;
            count += 1;
            emit(MotionChunk::Frame(AnimFrame {
                rgba,
                width: fw,
                height: fh,
                delay: nominal,
            }));
        }
        Ok(false)
    };

    for (s, packet) in ictx.packets() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(()); // navigated away — stop; the consumer drops the stream
        }
        if s.index() != vindex {
            continue;
        }
        decoder.send_packet(&packet)?;
        if drain(&mut decoder, &mut scaler, emit)? {
            truncated = true;
            break;
        }
    }
    if !truncated && !cancel.load(Ordering::Relaxed) {
        decoder.send_eof()?;
        truncated = drain(&mut decoder, &mut scaler, emit)?;
    }

    if count == 0 {
        emit(MotionChunk::Failed(DecodeError::Corrupt(
            "Live Photo motion decoded no frames".into(),
        )));
    } else {
        // Plays once and stops (finite loop = 1), like the batch/Windows/macOS backends.
        emit(MotionChunk::Done {
            loop_count: 1,
            truncated,
        });
    }
    Ok(())
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
    cancel: &AtomicBool,
    label: VideoKind,
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
        // Bail promptly if the caller navigated away — checked per packet (≈ per frame), so an
        // abandoned decode stops within a frame instead of finishing the whole clip.
        if cancel.load(Ordering::Relaxed) {
            return Ok(Err(DecodeError::Corrupt("motion decode cancelled".into())));
        }
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
        kind: label.kind,
        width,
        height,
        frames,
        // Live Photo = play once (loop 1); animated AVIF/HEIC = loop forever (0).
        loop_count: label.loop_count,
        codec: label.codec,
        // swscale's RGBA is nominal BT.709 (sRGB primaries/curve to within a hair) — pass
        // through, matching the Windows Media Foundation backend. (P3/HDR AVIF sequences will
        // read a touch oversaturated until per-sequence color management lands — a follow-up.)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The streaming decoder emits its chunks in order — exactly one `Header` first, then a
    /// run of `Frame`s, then a terminal `Done` — which is what lets playback start before the
    /// whole `.mov` decodes (task #69). Point `PB_LIVE_TEST_MOV` at a Live Photo `.mov` to run
    /// it (e.g. `test-images/live/IMG_0031.mov`); skipped when unset so it stays CI-safe.
    #[test]
    fn streaming_emits_header_then_frames_then_done_in_order() {
        let Ok(path) = std::env::var("PB_LIVE_TEST_MOV") else {
            eprintln!("skipping: set PB_LIVE_TEST_MOV to a Live Photo .mov to run this");
            return;
        };
        let cancel = AtomicBool::new(false);
        let mut kinds: Vec<&'static str> = Vec::new();
        let mut frames = 0usize;
        let mut first_dims = None;
        decode_live_motion_streaming(Path::new(&path), 1440, &cancel, &mut |chunk| match chunk {
            MotionChunk::Header(h) => {
                first_dims = Some((h.width, h.height));
                kinds.push("header");
            }
            MotionChunk::Frame(f) => {
                assert!(f.width > 0 && f.height > 0, "frame has zero dimensions");
                frames += 1;
                kinds.push("frame");
            }
            MotionChunk::Done { loop_count, .. } => {
                assert_eq!(loop_count, 1, "a Live Photo plays once");
                kinds.push("done");
            }
            MotionChunk::Failed(e) => panic!("stream failed: {e}"),
        });

        assert_eq!(kinds.first(), Some(&"header"), "Header must come first");
        assert_eq!(kinds.last(), Some(&"done"), "Done must come last");
        assert!(frames >= 2, "expected multiple frames, got {frames}");
        // Everything between the header and the terminal done is a frame — no interleaving.
        assert!(
            kinds[1..kinds.len() - 1].iter().all(|k| *k == "frame"),
            "frames must be contiguous between header and done"
        );
        assert!(first_dims.is_some_and(|(w, h)| w > 0 && h > 0));
    }

    /// The FFmpeg sequence decoder turns an animated AVIF/HEIC into a multi-frame, looping
    /// [`Animation`] (task: animated AVIF on Linux). Point `PB_SEQ_TEST_AVIF` at an animated
    /// `.avif` (e.g. `test-images/animated/3.avif`) to run it; skipped when unset.
    #[test]
    fn image_sequence_decodes_multiple_looping_frames() {
        let Ok(path) = std::env::var("PB_SEQ_TEST_AVIF") else {
            eprintln!("skipping: set PB_SEQ_TEST_AVIF to an animated .avif to run this");
            return;
        };
        let anim = decode_image_sequence(Path::new(&path), 1440).expect("sequence decode");
        assert_eq!(anim.kind, AnimationKind::Heif, "an image sequence");
        assert_eq!(anim.loop_count, 0, "loops like a GIF");
        assert!(anim.frames.len() > 1, "expected >1 frame");
        assert!(anim.frames.iter().all(|f| f.width > 0 && f.height > 0));
        assert!(anim.width > 0 && anim.height > 0);
    }
}
