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

    // The streaming reader is the Live-Photo decoder, whose header hardcodes
    // codec = "Live Photo" — wrong for a plain video. A cheap read-only metadata probe
    // (no frame decode) reports the real codec (H.264/HEVC/…) and precise native
    // dimensions, matching the Windows poster; fall back to the container label if the
    // probe can't open the stream (a container AVFoundation won't parse). Posters are off
    // the hot path (decode pool, cached), so the extra open is fine.
    let probe = crate::livephoto::probe_video_stream(path).ok();
    let codec = probe
        .as_ref()
        .map_or_else(|| container_label(path), |p| p.codec);
    let native_dims = probe.as_ref().map(pb_probe_dims);

    let mut chosen: Option<(Vec<u8>, u32, u32)> = None; // first bright frame
    let mut last: Option<(Vec<u8>, u32, u32)> = None; // fallback (dark clip / long black lead)
    let mut color = ColorTransform::default();
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
                    color = h.color; // honest; only the codec label is wrong for a video
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

    let (orig_width, orig_height) = native_dims.unwrap_or((width, height));
    Ok(DecodedImage {
        width,
        height,
        // The streaming reader already fitted the frame; the probe supplies the precise
        // native (display-oriented) resolution, falling back to the fitted dims.
        orig_width,
        orig_height,
        codec,
        format: PixelFormat::Rgba8,
        pixels,
        is_preview: false,
        color,
        peak: 1.0,
        animated: None,
    })
}

/// The probe's display-oriented (rotation-applied) native dimensions, for `orig_*`.
fn pb_probe_dims(p: &crate::video::VideoStreamInfo) -> (u32, u32) {
    p.display_dims()
}

/// A container label for the codec badge, from the file extension — the fallback when the
/// metadata probe can't open the stream. Never "Live Photo".
fn container_label(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mov" | "qt") => "MOV",
        Some("mp4" | "m4v") => "MP4",
        Some("avi") => "AVI",
        Some("mkv") => "MKV",
        Some("webm") => "WebM",
        Some("wmv" | "asf") => "WMV",
        Some("mpg" | "mpeg" | "m2v") => "MPEG",
        Some("3gp" | "3g2") => "3GP",
        Some("mts" | "m2ts" | "ts") => "AVCHD",
        _ => "Video",
    }
}

/// The macOS twin of `mf_poster`'s `poster_deep_seeks_past_a_long_black_intro`, and the
/// regression pin for **#92.2 / task #121 phase 4b**.
///
/// `deep_seek_black_lead.mp4` is 16 s and black for its first 7 s — well past the head
/// walk — with high-contrast content after. On Windows the deep seek finds that content.
/// On macOS an `.mp4` routes to THIS backend (`engine.rs`: AVFoundation is primary for the
/// containers it can open), which has no deep seek at all: it walks ~30 frames / ~1 s from
/// the start and hands back the least-black frame it saw. So this currently returns BLACK —
/// the user-visible "feature films get black posters on the Mac" bug.
///
/// `#[ignore]` because it documents a known gap rather than guarding a fixed one; phase 4b
/// (`av_poster` onto the shared walk driver) must make it pass and drop the attribute.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::FitBox;

    #[test]
    #[ignore = "#92.2: AVFoundation has no deep walk yet — passes when task #121 phase 4b lands"]
    fn poster_deep_seeks_past_a_long_black_intro() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video/deep_seek_black_lead.mp4");
        let img = decode_video_poster(
            &path,
            Some(FitBox {
                max_width: 64,
                max_height: 64,
            }),
            &AtomicBool::new(false),
        )
        .expect("poster");
        assert!(img.is_well_formed());
        assert!(
            crate::video::poster_frame_bright_enough(&img.pixels),
            "deep seek must land on the content past the 7 s black intro (mean luma {})",
            crate::video::mean_luma_rgba8(&img.pixels, 1),
        );
    }
}
