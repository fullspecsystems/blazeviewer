//! **The frame at a time** (task #81, export-plan.md rung 0a): one video frame at full
//! resolution at a playback position — what Copy / Copy Text / Describe / Export capture
//! from a paused (or playing) video instead of its poster.
//!
//! The playback timeline's zero is the stream's start time (the producer's `target_units`
//! base; its origin is the first decoded frame, which is the start time in practice), and the
//! frame **on screen** at `at` is the last one whose timestamp is at or before it — so that is
//! what this returns. 1 ms of slack absorbs `Duration` ↔ time-base rounding without ever
//! reaching the next frame. Seek to the preceding keyframe, decode forward (hardware decode
//! when available, only the winner leaves the GPU), convert at the display size with the
//! clip's rotation — HDR comes out fp16 scene-linear (1.0 = 203 nits, verified by #146's
//! audit), exactly like the poster and playback.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ffmpeg_next as ff;

use super::convert::FrameConverter;
use super::io::FfInput;
use super::probe::video_facts;
use crate::video::{VideoInput, POSTER_DEADLINE};
use crate::{DecodeError, DecodedImage, PixelFormat};

/// Decode the frame shown at `at` on `input`'s playback timeline, at full resolution.
/// Read-only, RAM-only; honours `cancel` inside blocking libav work.
pub fn ff_decode_frame_at(
    input: &VideoInput,
    at: Duration,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    frame_at_inner(input, at, cancel).map_err(DecodeError::Corrupt)
}

fn frame_at_inner(
    input: &VideoInput,
    at: Duration,
    cancel: &AtomicBool,
) -> Result<DecodedImage, String> {
    let mut opened = FfInput::open(input, Some(cancel))?;
    opened.set_op_deadline(Some(POSTER_DEADLINE));
    let deadline = std::time::Instant::now() + POSTER_DEADLINE;
    let facts = video_facts(opened.ctx())?;
    let (mut decoder, _hw) =
        super::poster::decoder_for(opened.ctx(), facts.index).map_err(|e| e.to_string())?;

    let (disp_w, disp_h) = facts.display_dims();
    let pre_rot = if facts.rotation % 180 == 90 {
        (disp_h, disp_w)
    } else {
        (disp_w, disp_h)
    };
    let mut conv = FrameConverter::new(
        (facts.width, facts.height),
        pre_rot,
        facts.rotation,
        &decoder,
        false, // a still: RGBA (or fp16 for HDR), never the planar GPU path
        false,
    );

    let base = facts.start_time.unwrap_or(0);
    let target = base.saturating_add(facts.duration_to_pts(at));
    let slack = facts.duration_to_pts(Duration::from_millis(1)).max(0);
    let bar = target.saturating_add(slack);

    // Keyframe at/before the target; if there is none (a target before the first
    // keyframe), allow landing past it — the first frame decoded then wins.
    let idx = facts.index as i32;
    let mut rc = unsafe {
        let ctx = opened.ctx().as_mut_ptr();
        ff::ffi::avformat_seek_file(ctx, idx, i64::MIN, target, target, 0)
    };
    if rc < 0 {
        rc = unsafe {
            let ctx = opened.ctx().as_mut_ptr();
            ff::ffi::avformat_seek_file(ctx, idx, i64::MIN, target, i64::MAX, 0)
        };
    }
    if rc < 0 {
        return Err(if opened.cancelled() {
            "frame capture cancelled".into()
        } else {
            "FFmpeg couldn't seek to that frame".into()
        });
    }
    decoder.flush();

    // Decode forward. `winner` is the last frame at/before the bar (a refcounted clone of
    // the raw — possibly hardware — frame); the first frame past it ends the walk, and wins
    // only if nothing came before it.
    let mut winner: Option<ff::frame::Video> = None;
    let mut packet = ff::Packet::empty();
    let mut eof_sent = false;
    'decode: loop {
        if opened.cancelled() {
            return Err("frame capture cancelled".into());
        }
        if std::time::Instant::now() >= deadline {
            break; // keep what we have; an empty winner errors below
        }
        let mut decoded = ff::frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            match decoded.timestamp() {
                Some(ts) if ts > bar => {
                    if winner.is_none() {
                        winner = Some(frame_ref(&decoded)?);
                    }
                    break 'decode;
                }
                // At/before the bar — or no timestamp at all (take it and keep going).
                _ => winner = Some(frame_ref(&decoded)?),
            }
        }
        if eof_sent {
            break; // drained: the last frame of the clip is the winner
        }
        match super::read_into_reused(&mut packet, opened.ctx()) {
            Ok(()) => {
                if packet.stream() == facts.index {
                    let _ = decoder.send_packet(&packet); // a corrupt packet is skipped
                }
            }
            Err(ff::Error::Eof) => {
                let _ = decoder.send_eof();
                eof_sent = true;
            }
            Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => {}
            Err(e) => return Err(format!("FFmpeg read: {e}")),
        }
    }
    drop(decoder); // the winner's reference keeps its surface alive on its own
    let raw = winner.ok_or("no frame decoded at that time")?;
    let transferred = super::hw::transfer_if_hw(&raw)?;
    let frame = transferred.as_ref().unwrap_or(&raw);
    let (pixels, w, h) = conv.convert(frame)?;
    let hdr = conv.output_format() == PixelFormat::Rgba16F;
    let sc = conv.source_color();
    Ok(DecodedImage {
        width: w,
        height: h,
        orig_width: disp_w,
        orig_height: disp_h,
        codec: facts.codec,
        format: conv.output_format(),
        pixels,
        is_preview: false,
        color: if hdr {
            crate::ColorTransform::srgb() // already scene-linear; passthrough
        } else {
            super::color::sdr_transform(&sc)
        },
        peak: conv.peak(),
        animated: None,
        recovered: None,
    })
}

/// A new reference to `src`'s buffers (`av_frame_ref`) — **not** `Clone`, which copies
/// into a fresh CPU buffer and so can't carry a hardware (VideoToolbox / VAAPI) surface:
/// the copy loses its frames context and the later transfer fails. A reference keeps the
/// surface valid after the decoder reuses `src`, so only the winner leaves the GPU.
fn frame_ref(src: &ff::frame::Video) -> Result<ff::frame::Video, String> {
    let mut dst = ff::frame::Video::empty();
    let rc = unsafe { ff::ffi::av_frame_ref(dst.as_mut_ptr(), src.as_ptr()) };
    if rc < 0 {
        return Err(format!("FFmpeg frame ref failed ({rc})"));
    }
    Ok(dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    /// Mean luma (0–255) of an RGBA8 frame.
    fn luma(img: &DecodedImage) -> f32 {
        assert_eq!(img.format, PixelFormat::Rgba8);
        let px = img.pixels.chunks_exact(4);
        let n = px.len() as f32;
        px.map(|p| (p[0] as f32 + p[1] as f32 + p[2] as f32) / 3.0)
            .sum::<f32>()
            / n
    }

    /// `frame_index.mp4`: 30 frames at 30 fps, frame *k* a flat grey of 8·k (+16), H.264
    /// with B-frames — so the frame at `k / 30 s` is identifiable by its brightness, and
    /// a seek has to decode forward from a keyframe through reordered frames to reach it.
    #[test]
    fn decodes_the_frame_shown_at_a_time() {
        let input = VideoInput::Path(fixture("frame_index.mp4"));
        let cancel = AtomicBool::new(false);
        for k in [0u32, 1, 7, 15, 29] {
            let at = Duration::from_secs_f64(k as f64 / 30.0);
            let img = ff_decode_frame_at(&input, at, &cancel).unwrap();
            let want = 16.0 + 8.0 * k as f32;
            let got = luma(&img);
            assert!(
                (got - want).abs() < 4.0,
                "frame {k}: luma {got}, want ≈ {want}"
            );
            assert_eq!((img.width, img.height), (64, 48), "full resolution");
        }
        // Between frames, the earlier one is on screen; past the end, the last frame.
        let mid =
            ff_decode_frame_at(&input, Duration::from_millis(7 * 1000 / 30 + 20), &cancel).unwrap();
        assert!((luma(&mid) - (16.0 + 56.0)).abs() < 4.0, "{}", luma(&mid));
        let end = ff_decode_frame_at(&input, Duration::from_secs(60), &cancel).unwrap();
        assert!(
            (luma(&end) - (16.0 + 8.0 * 29.0)).abs() < 4.0,
            "{}",
            luma(&end)
        );
    }

    /// An HDR clip's frame comes out fp16 scene-linear on the 203-nit scale (#146's card).
    #[test]
    fn an_hdr_frame_is_scene_linear() {
        let card =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hdr/pq-card.mp4");
        let img = ff_decode_frame_at(
            &VideoInput::Path(card),
            Duration::from_millis(40),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(img.format, PixelFormat::Rgba16F);
        // The 203-nit patch (third of six) at its centre.
        let x = img.width * 5 / 12;
        let i = ((img.height / 2 * img.width + x) * 8) as usize;
        let r = half::f16::from_le_bytes([img.pixels[i], img.pixels[i + 1]]).to_f32();
        assert!((r - 1.0).abs() < 0.03, "203 nits → 1.0, got {r}");
    }

    /// Wall time to capture frames across a real clip: `PB_FRAME_AT_CLIP=<path> cargo test
    /// -p pb-decode --features ffvideo frame_at_timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing report on a local clip (PB_FRAME_AT_CLIP)"]
    fn frame_at_timing() {
        let Ok(path) = std::env::var("PB_FRAME_AT_CLIP") else {
            return;
        };
        let input = VideoInput::Path(path.into());
        let cancel = AtomicBool::new(false);
        for frac in [0.0, 0.25, 0.5, 0.75, 0.99] {
            let dur = 3.0 * frac; // seconds; clamped to the clip by the decoder
            let t = std::time::Instant::now();
            let img = ff_decode_frame_at(&input, Duration::from_secs_f64(dur), &cancel).unwrap();
            println!(
                "at {dur:.2}s: {}x{} {:?} in {:.0} ms",
                img.width,
                img.height,
                img.format,
                t.elapsed().as_secs_f64() * 1e3
            );
        }
    }

    #[test]
    fn a_cancelled_capture_errors() {
        let cancel = AtomicBool::new(true);
        let r = ff_decode_frame_at(
            &VideoInput::Path(fixture("frame_index.mp4")),
            Duration::ZERO,
            &cancel,
        );
        assert!(r.is_err());
    }
}
