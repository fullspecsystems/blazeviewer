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
//!
//! **HDR tracks (task #144)** take their own branch: the 8-bit stream would have
//! AVFoundation tone-map PQ/HLG down to SDR, so a parked HDR MP4 sat flat on an XDR
//! display until `P` (then macOS ramped its EDR headroom, which read as a fade-in), while
//! the same stream in an MKV (FFmpeg, 10-bit) got an HDR poster. The HDR branch reads
//! half-floats in the source curve and decodes them with the shared `crate::color` PQ/HLG
//! math into the same `Rgba16F` scene-linear shape the FFmpeg poster and HDR stills use.
//! SDR tracks keep the cheap 8-bit path.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::animation::MotionChunk;
use crate::color::{hlg_to_scrgb, pq_to_scrgb};
use crate::common::{finalize_hdr_scrgb, rotate_pixels};
use crate::livephoto::{
    decode_live_motion_streaming, stream_hdr_frames, video_track_is_hdr, HdrFrame,
};
use crate::video::{
    poster_frame_bright_enough, poster_judge_scrgb, POSTER_JUDGE_WIDTH, POSTER_MAX_FRAMES,
    POSTER_MAX_MEDIA,
};
use crate::{ColorTransform, DecodeError, DecodedImage, FitBox, PixelFormat};

/// Decode `path`'s poster frame, fitted to `fit`. The seam mirrors the Windows
/// `mf_poster::decode_video_poster`, so the engine's poster path is platform-uniform.
pub fn decode_video_poster(
    path: &Path,
    fit: Option<FitBox>,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    let max_long_edge = poster_long_edge(fit);

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

    // HDR (PQ/HLG) tracks get a real HDR poster (task #144). Anything that goes wrong
    // there falls back to the SDR poster below, never to no poster at all.
    if video_track_is_hdr(path) {
        match decode_hdr_poster(path, max_long_edge, cancel, codec, native_dims) {
            Ok(img) => return Ok(img),
            Err(e) if cancel.load(Ordering::Relaxed) => return Err(e),
            Err(e) => eprintln!("Blaze Viewer: HDR poster failed ({e}); using the SDR poster"),
        }
    }

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
        recovered: None,
    })
}

/// The long edge a poster decodes to: the fit box's long edge, or a sane 4K-ish cap for
/// Fill/Original (no fit) — posters are off the hot path. Shared by both branches.
fn poster_long_edge(fit: Option<FitBox>) -> u32 {
    fit.map(|f| f.max_width.max(f.max_height))
        .unwrap_or(3840)
        .max(1)
}

/// The HDR poster (task #144): the same first-bright-frame walk as the SDR path, over
/// half-float frames still in the source's PQ/HLG curve ([`stream_hdr_frames`]), judged
/// and decoded with [`HdrDecode`] into an `Rgba16F` scene-linear scRGB poster
/// (`finalize_hdr_scrgb`: linear-light downscale + peak). Rotation is applied to the one
/// kept frame, not every candidate.
fn decode_hdr_poster(
    path: &Path,
    max_long_edge: u32,
    cancel: &AtomicBool,
    codec: &'static str,
    native_dims: Option<(u32, u32)>,
) -> Result<DecodedImage, DecodeError> {
    let mut decode: Option<HdrDecode> = None; // built from the first frame's tags
    let mut rotation = 0;
    let mut chosen: Option<HdrFrame> = None; // first bright frame
    let mut last: Option<HdrFrame> = None; // fallback (dark clip / long black lead)
    let mut frames = 0usize;
    let mut elapsed = Duration::ZERO;
    stream_hdr_frames(path, cancel, &mut |info, frame, delay| {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let dec = decode.get_or_insert_with(|| HdrDecode::new(info.transfer, info.primaries));
        rotation = info.rotation;
        frames += 1;
        elapsed += delay;
        if poster_judge_scrgb(&dec.judge_sample(&frame)).0 {
            chosen = Some(frame);
            return false;
        }
        last = Some(frame);
        frames < POSTER_MAX_FRAMES && elapsed < POSTER_MAX_MEDIA
    })?;
    if cancel.load(Ordering::Relaxed) {
        return Err(DecodeError::Corrupt("cancelled".into()));
    }
    let (Some(frame), Some(dec)) = (chosen.or(last), decode) else {
        return Err(DecodeError::Corrupt("video decoded no frames".into()));
    };
    let (f16, w, h) = rotate_pixels(frame.f16, frame.width, frame.height, rotation, 8);
    let fit = FitBox {
        max_width: max_long_edge,
        max_height: max_long_edge,
    };
    let mut img = finalize_hdr_scrgb(dec.to_linear(&f16), w, h, codec, Some(fit))?;
    if let Some((ow, oh)) = native_dims {
        img.orig_width = ow;
        img.orig_height = oh;
    }
    Ok(img)
}

/// Encoded half-float → scene-linear scRGB (1.0 = 203 nits, BT.709 primaries) for one
/// HDR stream: a 65536-entry table over every f16 bit pattern, so the PQ/HLG curve runs
/// once per poster rather than per pixel (the FFmpeg path's LUT trick), plus the source →
/// BT.709 primaries matrix (out-of-gamut colors go negative, as scRGB allows).
struct HdrDecode {
    lut: Vec<f32>,
    matrix: [[f32; 3]; 3],
}

impl HdrDecode {
    /// `transfer` is the CICP code (18 = HLG, otherwise PQ); `primaries` the CICP code.
    fn new(transfer: u8, primaries: u8) -> Self {
        let lut = (0..=u16::MAX)
            .map(|bits| {
                let e = half::f16::from_bits(bits).to_f32();
                // NaN/inf bit patterns never come from a decoder; map them to black.
                let e = if e.is_finite() { e } else { 0.0 };
                if transfer == 18 {
                    hlg_to_scrgb(e)
                } else {
                    pq_to_scrgb(e)
                }
            })
            .collect();
        // Linear transfer (code 8) so only the primaries matrix is taken — exactly the
        // FFmpeg path's `linear_primaries_matrix`.
        let matrix = ColorTransform::from_cicp(primaries, 8, 0, true).matrix;
        Self { lut, matrix }
    }

    /// One encoded RGBA16F pixel (8 bytes) → scene-linear BT.709 RGB.
    fn pixel(&self, px: &[u8]) -> [f32; 3] {
        let ch = |i: usize| self.lut[u16::from_le_bytes([px[i], px[i + 1]]) as usize];
        let (r, g, b) = (ch(0), ch(2), ch(4));
        let m = &self.matrix;
        [
            m[0][0] * r + m[0][1] * g + m[0][2] * b,
            m[1][0] * r + m[1][1] * g + m[1][2] * b,
            m[2][0] * r + m[2][1] * g + m[2][2] * b,
        ]
    }

    /// A whole frame as scene-linear f32 RGBA (opaque), for `finalize_hdr_scrgb`.
    fn to_linear(&self, f16: &[u8]) -> Vec<f32> {
        let mut out = Vec::with_capacity(f16.len() / 2);
        for px in f16.chunks_exact(8) {
            let [r, g, b] = self.pixel(px);
            out.extend_from_slice(&[r, g, b, 1.0]);
        }
        out
    }

    /// A point sample of `frame` at most [`POSTER_JUDGE_WIDTH`] wide, as scene-linear
    /// RGBA16F bytes for [`poster_judge_scrgb`]. Only a bright/not-bright call rides on
    /// it, so nearest-neighbour is plenty, and it keeps a 4K candidate to ~36k pixels.
    fn judge_sample(&self, frame: &HdrFrame) -> Vec<u8> {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let step = w.div_ceil(POSTER_JUDGE_WIDTH as usize).max(1);
        let one = half::f16::from_f32(1.0).to_le_bytes();
        let mut out = Vec::with_capacity(w.div_ceil(step) * h.div_ceil(step) * 8);
        for y in (0..h).step_by(step) {
            for x in (0..w).step_by(step) {
                let i = (y * w + x) * 8;
                let [r, g, b] = self.pixel(&frame.f16[i..i + 8]);
                for v in [r, g, b] {
                    out.extend_from_slice(&half::f16::from_f32(v).to_le_bytes());
                }
                out.extend_from_slice(&one);
            }
        }
        out
    }
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

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    fn f16_px(r: f32, g: f32, b: f32) -> Vec<u8> {
        [r, g, b, 1.0]
            .iter()
            .flat_map(|v| half::f16::from_f32(*v).to_le_bytes())
            .collect()
    }

    /// The PQ table hits the BT.2408 anchors (203 nits → 1.0, 0 → 0) and maps a
    /// decoder's YCbCr overshoot (negative, >1) to the clamped curve, never NaN.
    #[test]
    fn hdr_decode_pq_hits_the_reference_anchors() {
        // BT.709 primaries: an identity matrix up to the color library's round-off
        // (~2e-5 off the diagonal), hence the tolerances below.
        let dec = HdrDecode::new(16, 1);
        let [r, g, b] = dec.pixel(&f16_px(0.5807, 0.0, -0.2));
        assert!(
            (r - 1.0).abs() < 0.01,
            "PQ 0.5807 ≈ 203 nits → 1.0, got {r}"
        );
        assert!(g.abs() < 1e-3, "PQ 0 → black, got {g}");
        assert!(
            b.abs() < 1e-3,
            "negative overshoot clamps to black, got {b}"
        );
        let [hi, ..] = dec.pixel(&f16_px(1.2, 0.0, 0.0));
        assert!(
            hi.is_finite() && hi > 40.0,
            "overshoot clamps to PQ peak, got {hi}"
        );
    }

    /// BT.2020 → BT.709: white stays white, and a pure 2020 red is outside 709, so its
    /// green/blue go negative (scRGB carries out-of-gamut color that way).
    #[test]
    fn hdr_decode_maps_bt2020_primaries() {
        let dec = HdrDecode::new(16, 9);
        let [r, g, b] = dec.pixel(&f16_px(0.5807, 0.5807, 0.5807));
        for v in [r, g, b] {
            assert!((v - 1.0).abs() < 0.02, "white preserved, got {r} {g} {b}");
        }
        let [_, g, b] = dec.pixel(&f16_px(0.5807, 0.0, 0.0));
        assert!(
            g < 0.0 && b < 0.0,
            "2020 red is out of 709 gamut: g={g} b={b}"
        );
    }

    /// The judge sample is capped at POSTER_JUDGE_WIDTH wide and is linear RGBA16F.
    #[test]
    fn judge_sample_is_capped_and_linear() {
        let dec = HdrDecode::new(16, 1);
        let (w, h) = (1024u32, 4u32);
        let frame = HdrFrame {
            f16: f16_px(0.5807, 0.5807, 0.5807).repeat((w * h) as usize),
            width: w,
            height: h,
        };
        let sample = dec.judge_sample(&frame);
        assert_eq!(sample.len() / 8, 256, "1024 wide / step 4 = 256 px × 1 row");
        let r = half::f16::from_le_bytes([sample[0], sample[1]]).to_f32();
        assert!((r - 1.0).abs() < 0.01, "linear, not encoded: {r}");
    }

    /// The bug (task #144): an HDR10 MP4's poster was 8-bit SDR. Now it's Rgba16F
    /// scene-linear with headroom above SDR white.
    #[test]
    fn hdr_mp4_poster_is_scene_linear_fp16() {
        let img = decode_video_poster(&fixture("hdr_pq.mp4"), None, &AtomicBool::new(false))
            .expect("poster");
        assert_eq!(
            img.format,
            PixelFormat::Rgba16F,
            "HDR poster must not be tone-mapped"
        );
        assert!(img.is_well_formed());
        assert_eq!((img.width, img.height), (64, 64));
        assert!(img.peak > 1.0, "real HDR headroom, peak {}", img.peak);
    }

    /// SDR clips keep the cheap 8-bit path.
    #[test]
    fn sdr_mp4_poster_stays_rgba8() {
        let img = decode_video_poster(
            &fixture("color_with_tone.mp4"),
            None,
            &AtomicBool::new(false),
        )
        .expect("poster");
        assert_eq!(img.format, PixelFormat::Rgba8);
        assert!(img.is_well_formed());
    }

    /// The point of the fix: one HDR stream gets the same poster whether it's in an MP4
    /// (AVFoundation, this module) or an MKV (FFmpeg) — same curve, same scale, same
    /// gamut. `hdr_pq.mkv` is a stream copy of `hdr_pq.mp4`.
    #[cfg(feature = "ffvideo")]
    #[test]
    fn hdr_mp4_poster_matches_the_ffmpeg_mkv_poster() {
        let none = AtomicBool::new(false);
        let mp4 = decode_video_poster(&fixture("hdr_pq.mp4"), None, &none).expect("mp4");
        let mkv = crate::ffmpeg::poster::ff_decode_video_poster(
            &crate::video::VideoInput::Path(fixture("hdr_pq.mkv")),
            None,
            &none,
        )
        .expect("mkv");
        assert_eq!(mkv.format, PixelFormat::Rgba16F);
        assert_eq!((mp4.width, mp4.height), (mkv.width, mkv.height));
        let mean = |img: &DecodedImage| {
            let mut sum = [0f64; 3];
            let n = img.pixels.len() / 8;
            for px in img.pixels.chunks_exact(8) {
                for (c, s) in sum.iter_mut().enumerate() {
                    *s += half::f16::from_le_bytes([px[c * 2], px[c * 2 + 1]]).to_f32() as f64;
                }
            }
            sum.map(|s| s / n as f64)
        };
        let (a, b) = (mean(&mp4), mean(&mkv));
        for c in 0..3 {
            let tol = 0.03 * b[c].abs().max(1.0);
            assert!(
                (a[c] - b[c]).abs() < tol,
                "channel {c}: AVFoundation {:.3} vs FFmpeg {:.3} (all: {a:?} vs {b:?})",
                a[c],
                b[c]
            );
        }
    }

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
