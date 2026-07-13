//! The FFmpeg video **poster + probe** (plan §8-poster): the clip's first
//! non-black frame as a [`DecodedImage`], and the reader-sourced stream facts
//! behind the inspector's video rows — for the containers/codecs the OS
//! backends can't open (ALL video on Linux; MKV/WebM/VP8/VP9/AV1 on macOS).
//!
//! Mirrors the Windows `mf_poster` policy exactly: the shared mean-luma walk
//! (`poster_frame_bright_enough`, ~1 s / 30 frames, last-sampled-frame
//! fallback), fitted decode, rotation + color identical to playback (the
//! producer shares `probe`/`convert`). Honors the decode pool's `cancel` flag
//! **inside** blocking libav work via the AVIO interrupt callback (plan §6) —
//! task #83's multi-consumer pool makes prompt cancellation matter.

use std::sync::atomic::AtomicBool;

use ffmpeg_next as ff;

use std::time::Duration;

use super::convert::FrameConverter;
use super::io::FfInput;
use super::probe::{fit_dims, video_facts, VideoFacts};
use crate::video::{poster_frame_score, VideoInput, POSTER_GOOD_SCORE, POSTER_MAX_FRAMES};
use crate::{DecodeError, DecodedImage, FitBox, PixelFormat, VideoStreamInfo};

/// Overall watchdog for one poster attempt — a poster is a background nicety,
/// never worth pinning a pool worker longer than this on hostile input.
const POSTER_DEADLINE: Duration = Duration::from_secs(15);

/// The cheap first pass: how many consecutive frames to decode from the *start*
/// before deciding the opening is too dark/dull to poster. Clips that open on
/// content (Live Photos, home video) settle here without any seeking; a feature
/// film's logo-on-black + fade-in falls through to the deep pass.
const POSTER_HEAD_FRAMES: usize = POSTER_MAX_FRAMES;

/// Only *seek* past the intro for clips at least this long — short clips are
/// already covered by the head walk (and the cap collapses the offsets to almost
/// nothing anyway).
const POSTER_DEEP_MIN: Duration = Duration::from_secs(10);

/// Deep-seek probe points, **shallow → deep**: where an intro-having clip is
/// sampled after the head walk falls through. Anchored to how long real intros
/// actually run (a studio-logo/fade sequence is usually done by ~20–40 s), NOT to
/// a fraction of the clip — so a 2-hour film is sampled at ~8 s first, not ~90 s.
/// Each is clamped to [`poster_deep_cap`] and the walk stops at the first good
/// frame, so it returns the *earliest* genuinely-visible frame.
const POSTER_SEEK_OFFSETS: [Duration; 4] = [
    Duration::from_secs(8),
    Duration::from_secs(20),
    Duration::from_secs(45),
    Duration::from_secs(90),
];

/// Never sample deeper than this: min(half the clip, 5 min). 5 minutes is well
/// past any title sequence; going further risks spoilers and wasted decode on a
/// long film (owner guidance: not past ~5 min / 50 %).
fn poster_deep_cap(duration: Duration) -> Duration {
    duration.mul_f64(0.5).min(Duration::from_secs(300))
}

/// Frames to decode-and-score at each deep seek point before moving to the next.
const POSTER_BURST_FRAMES: usize = 12;

/// Decode the clip's first non-black frame (capped walk, last-frame fallback)
/// at `fit`, from a path or in-RAM archive bytes. Read-only, RAM-only.
pub fn ff_decode_video_poster(
    input: &VideoInput,
    fit: Option<FitBox>,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    poster_inner(input, fit, cancel).map_err(DecodeError::Corrupt)
}

/// One read-only open's stream facts (inspector parity with the Windows
/// `probe_video_input` / macOS `probe_video_stream`).
pub fn ff_probe_video_input(input: &VideoInput) -> Result<VideoStreamInfo, DecodeError> {
    let mut opened = FfInput::open(input, None).map_err(DecodeError::Corrupt)?;
    let facts = video_facts(opened.ctx()).map_err(DecodeError::Corrupt)?;
    // Decoder-reported color under the shared fallback policy (no frame decode
    // here — the probe is a ~header-only read, like the Windows one).
    let decoder = decoder_for(opened.ctx(), facts.index)?;
    let conv = FrameConverter::new((facts.width, facts.height), (1, 1), 0, &decoder);
    let sc = conv.source_color();
    Ok(VideoStreamInfo {
        codec: facts.codec,
        width: facts.width,
        height: facts.height,
        rotation: facts.rotation.rem_euclid(360) as u32,
        fps: facts.fps,
        duration: facts.duration,
        has_audio: facts.has_audio,
        color: super::color::sdr_transform(&sc),
    })
}

/// The poster walk's accept rule for an fp16 scene-linear frame — the linear
/// equivalent of [`poster_frame_bright_enough`]'s ~10% sRGB floor (sRGB 0.10
/// linearizes to ≈0.01). Subsampled like the RGBA8 walk.
fn scrgb_frame_bright_enough(f16_bytes: &[u8]) -> bool {
    let mut sum = 0.0f32;
    let mut n = 0u32;
    for px in f16_bytes.chunks_exact(8).step_by(8) {
        let ch = |i: usize| half::f16::from_le_bytes([px[i], px[i + 1]]).to_f32();
        // Rec.709 linear luma.
        sum += 0.2126 * ch(0) + 0.7152 * ch(2) + 0.0722 * ch(4);
        n += 1;
    }
    n > 0 && sum / n as f32 > 0.01
}

/// Poster score for an fp16 scene-linear frame, on the same scale as the RGBA8
/// [`poster_frame_score`] so the two share the [`POSTER_GOOD_SCORE`] bar. HDR
/// tone-mapping detail is hard to judge in scene-linear, so we keep the simple
/// bright/not-bright split: a visible frame is "good" (stops the walk, matching
/// the old first-bright behavior), a dark one isn't (seek deeper). Ordered by
/// brightness so the fallback still prefers the least-black frame.
fn scrgb_frame_score(f16_bytes: &[u8]) -> f32 {
    if scrgb_frame_bright_enough(f16_bytes) {
        1.0
    } else {
        let mut sum = 0.0f32;
        let mut n = 0u32;
        for px in f16_bytes.chunks_exact(8).step_by(8) {
            let ch = |i: usize| half::f16::from_le_bytes([px[i], px[i + 1]]).to_f32();
            sum += 0.2126 * ch(0) + 0.7152 * ch(2) + 0.0722 * ch(4);
            n += 1;
        }
        if n == 0 {
            0.0
        } else {
            (sum / n as f32).min(0.01) * 0.01 // below the good bar, ranked by brightness
        }
    }
}

/// The decoder for stream `index` of an opened input.
fn decoder_for(
    ctx: &mut ff::format::context::Input,
    index: usize,
) -> Result<ff::decoder::Video, DecodeError> {
    let stream = ctx
        .streams()
        .find(|s| s.index() == index)
        .ok_or_else(|| DecodeError::Corrupt("video stream vanished".into()))?;
    ff::codec::context::Context::from_parameters(stream.parameters())
        .and_then(|c| c.decoder().video())
        .map_err(|e| DecodeError::Corrupt(format!("FFmpeg decoder: {e}")))
}

/// The best-scoring poster candidate seen so far. Only the winning frame is
/// kept resident; every other decoded frame is scored and dropped.
struct Best {
    score: f32,
    frame: Option<(Vec<u8>, u32, u32)>,
}

impl Best {
    fn new() -> Self {
        Self {
            score: f32::NEG_INFINITY,
            frame: None,
        }
    }

    /// Offer a scored frame. Keeps it if it beats the current best (first max
    /// wins — deterministic, so the path and in-RAM posters stay bit-identical).
    /// Returns `true` when this frame clears [`POSTER_GOOD_SCORE`] — the signal
    /// that the walk has a clearly-good poster and can stop.
    fn consider(&mut self, score: f32, pixels: Vec<u8>, w: u32, h: u32) -> bool {
        if self.frame.is_none() || score > self.score {
            self.score = score;
            self.frame = Some((pixels, w, h));
        }
        score >= POSTER_GOOD_SCORE
    }
}

/// Holds the demux/decode state for one poster attempt so the head walk and each
/// deep-seek burst can share a single open context (the FFmpeg producer seeks
/// **in place** — no reopen, unlike the Windows MF reader).
struct PosterWalk<'a> {
    opened: FfInput<'a>,
    decoder: ff::decoder::Video,
    conv: FrameConverter,
    facts: VideoFacts,
    packet: ff::Packet,
    eof_sent: bool,
}

impl PosterWalk<'_> {
    /// Decode up to `limit` frames from the current position, scoring each into
    /// `best`. Returns `Ok(true)` as soon as a clearly-good frame is found (the
    /// caller stops), `Ok(false)` when the limit or EOF is reached first.
    fn scan(&mut self, limit: usize, best: &mut Best) -> Result<bool, String> {
        let mut decoded_count = 0usize;
        while decoded_count < limit {
            if self.opened.cancelled() {
                return Err("cancelled".into());
            }
            // Pull every ready frame before feeding more packets.
            let mut decoded = ff::frame::Video::empty();
            while decoded_count < limit && self.decoder.receive_frame(&mut decoded).is_ok() {
                let (pixels, w, h) = self.conv.convert(&decoded)?;
                decoded_count += 1;
                let score = if self.conv.output_format() == PixelFormat::Rgba16F {
                    scrgb_frame_score(&pixels)
                } else {
                    poster_frame_score(&pixels)
                };
                if best.consider(score, pixels, w, h) {
                    return Ok(true);
                }
            }
            if self.eof_sent {
                break; // decoder fully drained
            }
            match self.packet.read(self.opened.ctx()) {
                Ok(()) => {
                    if self.packet.stream() == self.facts.index {
                        // A corrupt packet is skipped, not fatal.
                        let _ = self.decoder.send_packet(&self.packet);
                    }
                }
                Err(ff::Error::Eof) => {
                    let _ = self.decoder.send_eof();
                    self.eof_sent = true;
                }
                Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => {}
                Err(e) => return Err(format!("FFmpeg read: {e}")),
            }
        }
        Ok(false)
    }

    /// Seek in place to a keyframe at/around `target` and reset decode state.
    /// Mirrors the producer's seek (start-time base, allow landing past the
    /// target on a start-offset edge).
    fn seek(&mut self, target: Duration) -> Result<(), String> {
        let base = self.facts.start_time.unwrap_or(0);
        let target_units = base.saturating_add(self.facts.duration_to_pts(target));
        let idx = self.facts.index as i32;
        let mut rc = unsafe {
            let ctx = self.opened.ctx().as_mut_ptr();
            ff::ffi::avformat_seek_file(ctx, idx, i64::MIN, target_units, target_units, 0)
        };
        if rc < 0 {
            // Nothing at/before the target — allow landing past it.
            rc = unsafe {
                let ctx = self.opened.ctx().as_mut_ptr();
                ff::ffi::avformat_seek_file(ctx, idx, i64::MIN, target_units, i64::MAX, 0)
            };
        }
        if rc < 0 {
            return Err(format!("poster seek failed: {}", ff::Error::from(rc)));
        }
        self.decoder.flush();
        self.eof_sent = false;
        Ok(())
    }
}

fn poster_inner(
    input: &VideoInput,
    fit: Option<FitBox>,
    cancel: &AtomicBool,
) -> Result<DecodedImage, String> {
    let mut opened = FfInput::open(input, Some(cancel))?;
    opened.set_op_deadline(Some(POSTER_DEADLINE));
    let facts = video_facts(opened.ctx())?;
    let decoder = decoder_for(opened.ctx(), facts.index).map_err(|e| e.to_string())?;

    // Output geometry: fit the SAR-corrected display dims, then map back to
    // pre-rotation axes for the scaler (the converter rotates after).
    let (disp_w, disp_h) = facts.display_dims();
    let (fw, fh) = match fit {
        Some(f) => fit_dims(disp_w, disp_h, f),
        None => (disp_w, disp_h),
    };
    let pre_rot = if facts.rotation % 180 == 90 {
        (fh, fw)
    } else {
        (fw, fh)
    };
    let conv = FrameConverter::new((facts.width, facts.height), pre_rot, facts.rotation, &decoder);

    let duration = facts.duration;
    let mut walk = PosterWalk {
        opened,
        decoder,
        conv,
        facts,
        packet: ff::Packet::empty(),
        eof_sent: false,
    };
    let mut best = Best::new();

    // Phase 1 — the cheap head walk from the start. A clip that opens on content
    // settles here; a dark/logo/fade opening leaves `best` weak and falls through.
    if walk.scan(POSTER_HEAD_FRAMES, &mut best)? {
        return walk.finish(best, disp_w, disp_h);
    }

    // Phase 2 — seek past the intro (feature-film case), shallow → deep, stopping
    // at the first good frame so the poster is as early as the intro allows.
    if let Some(dur) = duration {
        if dur >= POSTER_DEEP_MIN {
            let cap = poster_deep_cap(dur);
            let mut last = Duration::ZERO;
            for off in POSTER_SEEK_OFFSETS {
                let target = off.min(cap);
                // Skip offsets the head walk already covered (~1 s) and duplicates
                // (a short clip collapses the deeper offsets onto the cap).
                if target <= Duration::from_secs(1) || target <= last {
                    continue;
                }
                last = target;
                if walk.opened.cancelled() {
                    return Err("cancelled".into());
                }
                if walk.seek(target).is_ok() && walk.scan(POSTER_BURST_FRAMES, &mut best)? {
                    return walk.finish(best, disp_w, disp_h);
                }
            }
        }
    }

    walk.finish(best, disp_w, disp_h)
}

impl PosterWalk<'_> {
    /// Assemble the winning candidate into a `DecodedImage` (poster ≡ HDR-still
    /// shape for the renderer). `Err` only when no frame decoded at all.
    fn finish(&self, best: Best, disp_w: u32, disp_h: u32) -> Result<DecodedImage, String> {
        let (pixels, w, h) = best.frame.ok_or("video decoded no frames")?;
        let sc = self.conv.source_color();
        // HDR (PQ/HLG) posters leave as fp16 scene-linear scRGB — the exact shape
        // `common::finalize_hdr_scrgb` gives HDR stills, so the renderer treats a
        // video poster and an HDR photo identically (plan §9).
        let hdr = self.conv.output_format() == PixelFormat::Rgba16F;
        Ok(DecodedImage {
            width: w,
            height: h,
            orig_width: disp_w,
            orig_height: disp_h,
            codec: self.facts.codec,
            format: self.conv.output_format(),
            pixels,
            is_preview: false,
            color: if hdr {
                crate::ColorTransform::srgb() // already scene-linear; passthrough
            } else {
                super::color::sdr_transform(&sc)
            },
            peak: self.conv.peak(),
            animated: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    fn poster(name: &str, fit: Option<FitBox>) -> Result<DecodedImage, DecodeError> {
        ff_decode_video_poster(
            &VideoInput::Path(fixture(name)),
            fit,
            &AtomicBool::new(false),
        )
    }

    /// The fixture opens on black frames; the walk must skip them and land on
    /// the color section — the exact contract the Windows poster test locks.
    #[test]
    fn poster_walks_past_the_black_lead() {
        let img = poster("black_then_color.mp4", None).expect("poster");
        assert!(img.is_well_formed());
        assert_eq!((img.width, img.height), (64, 64));
        assert_eq!(img.codec, "H.264");
        assert!(
            crate::video::mean_luma_rgba8(&img.pixels, 1) > crate::video::POSTER_LUMA_MIN,
            "poster landed on a black frame"
        );
    }

    #[test]
    fn poster_fits_and_keeps_even_dims() {
        let img = poster(
            "black_then_color.mp4",
            Some(FitBox {
                max_width: 32,
                max_height: 32,
            }),
        )
        .expect("poster");
        assert_eq!((img.width, img.height), (32, 32));
        assert_eq!((img.orig_width, img.orig_height), (64, 64));
        assert!(img.is_well_formed());
    }

    #[test]
    fn poster_from_in_ram_bytes_matches_the_path_poster() {
        let data = std::sync::Arc::new(std::fs::read(fixture("black_then_color.mp4")).unwrap());
        let by_bytes = ff_decode_video_poster(
            &VideoInput::Bytes {
                data,
                name: "clip.mp4".into(),
            },
            None,
            &AtomicBool::new(false),
        )
        .expect("bytes poster");
        let by_path = poster("black_then_color.mp4", None).expect("path poster");
        assert_eq!(by_bytes.pixels, by_path.pixels, "bytes ≡ path, bit-exact");
    }

    #[test]
    fn cancel_aborts_the_poster() {
        let r = ff_decode_video_poster(
            &VideoInput::Path(fixture("black_then_color.mp4")),
            None,
            &AtomicBool::new(true),
        );
        assert!(r.is_err(), "a cancelled poster must not decode");
    }

    #[test]
    fn probe_reports_the_stream_facts() {
        let info =
            ff_probe_video_input(&VideoInput::Path(fixture("color_with_tone.mp4"))).expect("probe");
        assert_eq!(info.codec, "H.264");
        assert!(info.has_audio);
        assert!(info.duration.is_some());
        assert_eq!(
            info.display_dims(),
            (info.width, info.height),
            "no rotation"
        );
    }

    /// HDR posters mirror HDR stills: fp16 scene-linear, passthrough color,
    /// a real peak — the same `DecodedImage` shape `finalize_hdr_scrgb` makes.
    #[test]
    fn hdr_poster_is_fp16_scene_linear() {
        let img = poster("hdr_pq.mp4", None).expect("hdr poster");
        assert_eq!(img.format, PixelFormat::Rgba16F);
        assert!(img.is_well_formed());
        assert!(!img.color.enabled, "scene-linear → passthrough");
        assert!(img.peak >= 1.0);
    }

    #[test]
    fn garbage_bytes_fail_cleanly() {
        let r = ff_decode_video_poster(
            &VideoInput::Bytes {
                data: std::sync::Arc::new(vec![0xAAu8; 2048]),
                name: "junk.webm".into(),
            },
            None,
            &AtomicBool::new(false),
        );
        assert!(r.is_err());
    }
}
