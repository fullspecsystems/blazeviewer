//! macOS video **poster** — the first non-black frame of a clip (task 79.9 phase 4),
//! so the library shows a thumbnail when you flip past a video instead of blackness.
//!
//! Pure Rust: it drives the existing Live-Photo streaming reader
//! ([`decode_live_motion_streaming`](crate::livephoto::decode_live_motion_streaming)) —
//! which is generic AVFoundation video decode, not Live-Photo-specific — with a
//! poster-walk callback, and builds a [`DecodedImage`] from the first bright frame.
//! Because it's the *same* AVAssetReader path playback uses, the poster's rotation,
//! color, and decode-to-fit are identical to playback by construction (poster ≡ playback,
//! a plan requirement). All AVFoundation/objc lives in `livephoto`; there is none here.
//!
//! Runs in the decode pool (off the main thread), cancellable, and bounded by the shared
//! [`POSTER_MAX_FRAMES`] / [`POSTER_MAX_MEDIA`] policy — it stops at the first bright frame
//! (or the cap), never decoding the whole clip.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::animation::MotionChunk;
use crate::livephoto::decode_live_motion_streaming;
use crate::video::{poster_frame_bright_enough, POSTER_MAX_FRAMES, POSTER_MAX_MEDIA};
use crate::{ColorTransform, DecodeError, DecodedImage, FitBox, PixelFormat};

/// Decode `path`'s poster frame, fitted to `fit`. The seam mirrors the Windows
/// `mf_poster::decode_video_poster`, so the engine's poster path is platform-uniform.
pub fn decode_video_poster(
    path: &Path,
    fit: Option<FitBox>,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    // The streaming reader decode-to-fits by long edge; use the fit box's long edge
    // (Fill/Original modes pass no fit → a sane 4K-ish cap, posters being off the hot path).
    let max_long_edge = fit
        .map(|f| f.max_width.max(f.max_height))
        .unwrap_or(3840)
        .max(1);

    let mut chosen: Option<(Vec<u8>, u32, u32)> = None; // first bright frame
    let mut last: Option<(Vec<u8>, u32, u32)> = None; // fallback (dark clip / long black lead)
    let mut color = ColorTransform::default();
    let mut codec: &'static str = "Video";
    let mut frames = 0usize;
    let mut elapsed = Duration::ZERO;
    let mut failure: Option<DecodeError> = None;

    // One stop flag drives the reader: set it on the first bright frame, at the frame/
    // media cap, or when the caller cancels (mirrored in each callback). The reader
    // checks it between frames, so we never walk the whole clip.
    let stop = AtomicBool::new(false);
    {
        let mut emit = |chunk: MotionChunk| {
            if cancel.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
            }
            match chunk {
                MotionChunk::Header(h) => {
                    color = h.color;
                    codec = h.codec;
                }
                MotionChunk::Frame(af) => {
                    frames += 1;
                    elapsed += af.delay;
                    if poster_frame_bright_enough(&af.rgba) {
                        chosen = Some((af.rgba, af.width, af.height));
                        stop.store(true, Ordering::Relaxed);
                    } else {
                        last = Some((af.rgba, af.width, af.height));
                        if frames >= POSTER_MAX_FRAMES || elapsed >= POSTER_MAX_MEDIA {
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                }
                MotionChunk::Failed(e) => failure = Some(e),
                MotionChunk::Done { .. } => {}
            }
        };
        decode_live_motion_streaming(path, max_long_edge, &stop, &mut emit);
    }

    // A real cancellation (not our early-stop) discards the result — the decode pool
    // moved on. Distinguish it via the caller's own flag, not the combined `stop`.
    if cancel.load(Ordering::Relaxed) {
        return Err(DecodeError::Corrupt("cancelled".into()));
    }

    let (pixels, width, height) = match chosen.or(last) {
        Some(frame) => frame,
        // No frame at all: surface the reader's failure if it had one, else a generic.
        None => {
            return Err(
                failure.unwrap_or_else(|| DecodeError::Corrupt("video decoded no frames".into()))
            )
        }
    };

    Ok(DecodedImage {
        width,
        height,
        // The streaming reader already fitted the frame; report those dims. Precise
        // native resolution rides the (later) macOS `probe_video_stream`.
        orig_width: width,
        orig_height: height,
        codec,
        format: PixelFormat::Rgba8,
        pixels,
        is_preview: false,
        color,
        peak: 1.0,
        animated: None,
    })
}
