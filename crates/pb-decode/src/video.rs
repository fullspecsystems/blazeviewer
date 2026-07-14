//! Video playback frame contract (task #79, phase 0) — the types every video producer
//! (Media Foundation / AVFoundation / FFmpeg) emits and the `VideoSession` consumes.
//!
//! This is a *contract module*: pure data, no decoding. The producers land in later
//! phases behind the same seam the Live Photo decoders use; the session state machine
//! lives in `pb-app-core` (shell-neutral). Defined here because the producers are
//! pb-decode backends and the frame carries pb-decode types ([`PixelFormat`],
//! [`ColorTransform`]).
//!
//! Design constraints baked into these types (plan rev2, `.taskmaster/plans/79-…md`):
//! - **Identity:** every frame carries `session_id` + `seek_generation` so a stale
//!   frame that raced a flush is discarded at the consumer, never presented.
//! - **Timing:** `pts` is session-relative (the producer normalizes nonzero/negative
//!   container start times) and rational-derived — a `Duration`, never an accumulated
//!   float delta.
//! - **Color:** frames carry [`VideoColorInfo`] + [`PixelFormat`] *now* so fp16/NV12
//!   backends slot in later without rewriting the session (tier-2 ships tested SDR
//!   RGBA8 only).

use std::time::Duration;

use crate::{ColorTransform, PixelFormat};

/// Where a video's container bytes come from — the input seam every platform
/// reader opens from. `Path` streams from disk (the loose-file case); `Bytes`
/// serves an **in-RAM** container (an archive entry — RAM-only per the privacy
/// guarantee, never extracted to disk). Cheap to clone: the bytes are shared
/// (`Arc`), so one copy feeds the producer, every seek-reopen, the poster/probe,
/// and the shell audio player.
#[derive(Clone)]
pub enum VideoInput {
    /// A file on disk, streamed by the platform reader.
    Path(std::path::PathBuf),
    /// An in-RAM container. `name` must end in the real extension — it routes
    /// the container handler lookup (byte streams have no URL to sniff) and
    /// names the item in error copy.
    Bytes {
        data: std::sync::Arc<Vec<u8>>,
        name: String,
    },
}

impl VideoInput {
    /// The item's display name, for error/log copy.
    pub fn display_name(&self) -> String {
        match self {
            VideoInput::Path(p) => p.display().to_string(),
            VideoInput::Bytes { name, .. } => name.clone(),
        }
    }
}

// Manual: never dump the container bytes into logs/assertions.
impl std::fmt::Debug for VideoInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VideoInput::Path(p) => f.debug_tuple("Path").field(p).finish(),
            VideoInput::Bytes { data, name } => f
                .debug_struct("Bytes")
                .field("len", &data.len())
                .field("name", name)
                .finish(),
        }
    }
}

/// The MIME content type for a video file/entry name, when one of the recognized
/// container extensions matches. Feeds the byte-stream container resolution on
/// both platform seams (MF byte-stream attributes, WinRT `MediaSource` streams).
pub fn video_content_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.').map(|(_, e)| e)?;
    Some(match ext.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mov" | "qt" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/avi",
        "wmv" => "video/x-ms-wmv",
        "asf" => "video/x-ms-asf",
        "mpg" | "mpeg" => "video/mpeg",
        "mts" | "m2ts" => "video/mp2t",
        "3gp" => "video/3gpp",
        "3g2" => "video/3gpp2",
        _ => return None,
    })
}

/// Identifies one playback session of one item. A new session (open, replay after
/// `Stopped`, navigate back to the item) gets a fresh id; frames from a dead session
/// are dropped at the consumer no matter how they raced teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoSessionId(pub u64);

/// Bumped on every seek. A frame stamped with an older generation than the session's
/// current one was decoded toward a superseded target and must never present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeekGeneration(pub u64);

impl SeekGeneration {
    pub const FIRST: SeekGeneration = SeekGeneration(0);

    /// The next generation. Saturating: 2^64 seeks is unreachable, but the contract
    /// never wraps back to a live generation.
    #[must_use]
    pub fn next(self) -> SeekGeneration {
        SeekGeneration(self.0.saturating_add(1))
    }
}

/// What one read-only open of a video container reports — the reader-sourced facts
/// behind the inspector's video rows and the poster's [`crate::DecodedImage`] fields.
/// Platform-neutral so both the Windows Media Foundation probe (`mf_poster`) and the
/// macOS AVFoundation probe (`livephoto::probe_video_stream`) construct the same type.
/// Unknowns stay `None`/default; nothing here ever comes from a RAM read of the file.
#[derive(Debug, Clone)]
pub struct VideoStreamInfo {
    /// Codec display name for known subtypes ("H.264", "HEVC", …), else "Video".
    /// `&'static str` so it can ride [`crate::DecodedImage::codec`].
    pub codec: &'static str,
    /// Native (pre-rotation) pixel dimensions.
    pub width: u32,
    pub height: u32,
    /// Container rotation in degrees CW (0/90/180/270); pixels are already upright by
    /// the time they're decoded — this is for metadata display and the dimension swap.
    pub rotation: u32,
    /// Average frame rate as reported; 0.0 = unknown.
    pub fps: f64,
    pub duration: Option<Duration>,
    pub has_audio: bool,
    /// Source color read from the native media type (same policy the poster uses).
    pub color: ColorTransform,
}

impl VideoStreamInfo {
    /// Dimensions as displayed (after the container rotation is applied).
    pub fn display_dims(&self) -> (u32, u32) {
        if self.rotation % 180 == 90 {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }
}

/// What the **Details** probe reports (task #98): the basic facts *plus* the full
/// audio/subtitle [`MediaTrackCatalog`], from one open of the container.
///
/// Deliberately a separate type from [`VideoStreamInfo`], and produced by a separate
/// entry point, because the two have different callers and different costs.
/// `VideoStreamInfo` feeds the **poster** path, which runs for every *prefetched* video
/// whether or not the Inspector is ever opened — a media-selection-group walk or a full
/// stream enumeration must never be charged to it. This one runs only when the Inspector
/// actually asks, and only off the event loop.
// (No `Eq`: `VideoStreamInfo::fps` is an `f64`. The catalog itself is `Eq`, so track
// assertions compare directly.)
#[derive(Debug, Clone)]
pub struct VideoDetailsProbe {
    pub video: VideoStreamInfo,
    pub tracks: crate::tracks::MediaTrackCatalog,
}

/// YUV→RGB matrix coefficients for subsampled ([`PixelFormat::Nv12`] /
/// [`PixelFormat::P010`]) frames (task 79.10). Inert for RGB pixel formats (the
/// producer already converted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// The transfer function the renderer must invert to reach scene-linear light
/// for a planar video frame (task #91 Phase 2). **Decoupled from storage
/// precision** ([`PixelFormat`](crate::PixelFormat) `Nv12` vs `P010`): 10-bit
/// SDR is `P010` + [`SrgbLike`](Self::SrgbLike)/[`Parametric`](Self::Parametric),
/// while HDR is `P010` + [`Pq`](Self::Pq)/[`Hlg`](Self::Hlg). The renderer maps
/// this to its shader transfer mode; it never reads raw CICP integers.
///
/// Inert for RGB pixel formats — those carry their transfer in
/// [`ColorTransform`] already (the producer / OS converter applied it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoTransfer {
    /// sRGB / BT.709-gamma-like — the renderer's built-in sRGB EOTF. The default
    /// for ordinary 8-bit SDR video.
    SrgbLike,
    /// A source-specific parametric curve carried in [`ColorTransform::trc`].
    Parametric,
    /// SMPTE ST 2084 (PQ), absolute-luminance HDR.
    Pq,
    /// Hybrid Log-Gamma (ARIB STD-B67 / BT.2100), scene-referred HDR.
    Hlg,
}

impl VideoTransfer {
    /// True for the HDR transfers ([`Pq`](Self::Pq) / [`Hlg`](Self::Hlg)) — the
    /// ones that expand beyond 1.0 in scene-linear and drive the fp16/EDR present
    /// path rather than the SDR tone-map.
    pub fn is_hdr(self) -> bool {
        matches!(self, VideoTransfer::Pq | VideoTransfer::Hlg)
    }
}

/// Source color for a video frame: the resolved shader transform for the pixels as
/// emitted, plus the container/decoder-reported CICP code points kept verbatim so a
/// future fp16/P010 backend (or a diagnostics panel) can re-derive without re-probing.
///
/// **Single-application contract (task 79.10):** for RGB pixel formats the producer
/// (or the OS converter) has already applied the YUV matrix + range — `yuv_matrix` /
/// `full_range` are inert. For [`PixelFormat::Nv12`] *nothing* upstream has: the
/// consumer (the renderer's convert) applies them exactly once. `transform` always
/// carries primaries + transfer only, never the matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoColorInfo {
    /// Source→sRGB transform for the emitted pixels, applied in-shader — same
    /// mechanism as stills ([`crate::DecodedImage::color`]); primaries + transfer.
    pub transform: ColorTransform,
    /// CICP code points as reported by the source (H.273): colour_primaries,
    /// transfer_characteristics, matrix_coefficients. `None` = the container didn't
    /// say (the transform then documents the assumption the producer made).
    pub cicp: Option<(u8, u8, u8)>,
    /// Full-range flag as reported (or assumed) for the source YUV.
    pub full_range: bool,
    /// Matrix coefficients for planar (NV12 / P010) frames (see the contract above).
    pub yuv_matrix: YuvMatrix,
    /// Transfer function the renderer inverts for planar frames (task #91 Phase 2).
    /// Decoupled from storage precision; see [`VideoTransfer`]. Inert for RGB
    /// pixel formats (they carry their transfer in `transform`). `SrgbLike` for
    /// the srgb() default.
    pub transfer: VideoTransfer,
    /// Scene-linear peak for HDR ([`PixelFormat::Rgba16F`]) frames — the
    /// tone-map white point when presenting on an SDR display, exactly like
    /// [`crate::DecodedImage::peak`] for HDR stills. `1.0` for SDR frames
    /// (task #84 §9: the fp16 video path mirrors the stills convention).
    pub peak: f32,
}

impl VideoColorInfo {
    /// sRGB passthrough — the assumption when a source carries no color metadata.
    pub fn srgb() -> Self {
        VideoColorInfo {
            transform: ColorTransform::srgb(),
            cicp: None,
            full_range: true,
            yuv_matrix: YuvMatrix::Bt709,
            transfer: VideoTransfer::SrgbLike,
            peak: 1.0,
        }
    }
}

/// One decoded, display-ready video frame. The producer→session queue element.
///
/// Pixels are tightly packed `format.frame_bytes(width, height)` bytes (for NV12:
/// the Y plane then the interleaved half-res UV plane), already fitted to the
/// session's fixed output geometry (never re-fitted mid session; window resizes
/// rescale on the GPU and apply a new fit on next play).
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub session_id: VideoSessionId,
    pub seek_generation: SeekGeneration,
    /// Presentation time, session-relative (0 = first frame of the media, after
    /// the producer normalized any nonzero/negative container start offset).
    pub pts: Duration,
    pub width: u32,
    pub height: u32,
    /// Tier 2 producers emit `Rgba8` only; the field exists so NV12/fp16 backends
    /// slot in behind the same queue without a contract change.
    pub format: PixelFormat,
    pub pixels: Vec<u8>,
    pub color: VideoColorInfo,
}

impl VideoFrame {
    /// The byte count this frame charges against the session's queue budget.
    pub fn byte_len(&self) -> u64 {
        self.pixels.len() as u64
    }

    /// Structural sanity: pixel buffer matches the declared geometry/format (and
    /// the planar formats' even-dimension requirement holds). Uses checked
    /// arithmetic — a geometry that overflows `usize` fails the check rather than
    /// wrapping to a length a hostile buffer could match.
    pub fn is_well_formed(&self) -> bool {
        let even_ok = !self.format.is_planar_video()
            || (self.width.is_multiple_of(2) && self.height.is_multiple_of(2));
        let len_ok = self
            .format
            .checked_frame_bytes(self.width, self.height)
            .is_some_and(|n| self.pixels.len() == n);
        self.width > 0 && self.height > 0 && even_ok && len_ok
    }
}

// ---------------------------------------------------------------------------
// Producer ⇄ session protocol (task #79 phase 4).
// ---------------------------------------------------------------------------

/// Producer → session events. Every event carries the session id (and frames the
/// seek generation) so a straggler from a torn-down producer can never touch a
/// newer session, no matter how channels are (mis)wired.
#[derive(Debug)]
pub enum VideoProducerEvent {
    /// The reader opened; stream facts the session may use (duration for the
    /// HUD/seek clamp — `None` stays honest for unbounded/unknown streams;
    /// `has_audio` decides whether the shell audio player starts at all and
    /// whether preroll waits for audio readiness).
    Opened {
        session_id: VideoSessionId,
        duration: Option<Duration>,
        width: u32,
        height: u32,
        has_audio: bool,
        /// Bytes of one decoded frame at the producer's REAL negotiated output —
        /// the session's credit-granting size (task 79.10: format-aware, so an
        /// NV12 producer isn't under-credited 2.67× by a `w·h·4` assumption).
        frame_bytes: u64,
    },
    Frame(VideoFrame),
    EndOfStream {
        session_id: VideoSessionId,
        seek_generation: SeekGeneration,
    },
    /// Producer is dead (open failure, decode error). Terminal for the session.
    Failed {
        session_id: VideoSessionId,
        error: String,
    },
}

/// Session → producer messages, on a **single merged channel** — the design that
/// makes backpressure unable to deafen control (plan rev2): the producer's only
/// blocking point is `recv()` on this channel, so a `Stop` gets through even when
/// zero credits are outstanding. A credit is permission to decode + send exactly
/// one frame; the session grants them only while the byte/frame budget admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoProducerMsg {
    /// Decode and send one frame (or `EndOfStream` if the stream is done).
    Credit,
    /// Land a seek (task #79 phase 6): recreate the reader positioned at `target`
    /// (session-relative; spike-locked — repositioning a warm HEVC reader blocks
    /// ~1 s, a fresh one positions in ~0 ms), decode forward discarding frames
    /// before it, and stamp everything after with `generation`. Latest-value: a
    /// newer `SeekTo` supersedes every stage of an older one, and a `SeekTo`
    /// **zeroes the producer's credit balance** — only credits received after it
    /// count, which makes the session's flush + regrant race-free by channel order.
    SeekTo {
        target: Duration,
        generation: SeekGeneration,
    },
    /// Tear down now. Channel disconnection means the same thing.
    Stop,
}

// ---------------------------------------------------------------------------
// Poster selection (task #79 phase 2): the first-non-black mean-luma walk.
// Pure math, shared by every platform's poster backend and unit-tested here.
// ---------------------------------------------------------------------------

/// How far into the clip the poster walk samples before giving up on finding a
/// non-black frame: ~1 s of media or [`POSTER_MAX_FRAMES`] frames, whichever first.
pub const POSTER_MAX_MEDIA: Duration = Duration::from_secs(1);
pub const POSTER_MAX_FRAMES: usize = 30;

/// Mean-luma floor for "not black": fade-ins and lead-in black frames sit near 0;
/// this is ~10% gray. Raised from 0.06 (owner feedback, 2026-07-12): frames barely
/// past the old floor were accepted and still read as near-black tiles, so the walk
/// now skips those and lands on a genuinely visible frame. A dark-throughout clip
/// still gets *a* poster — the walk's fallback is the last sampled frame — so
/// raising the floor never costs a poster, only walks a little further.
pub const POSTER_LUMA_MIN: f32 = 0.10;

/// Mean Rec.601 luma of a tightly packed RGBA8 buffer, in 0..=1. Samples every
/// `stride`-th pixel (pass 1 to read them all) — a poster-sized frame subsampled
/// at 8 is plenty to classify black-vs-content and costs microseconds.
pub fn mean_luma_rgba8(pixels: &[u8], stride: usize) -> f32 {
    let stride = stride.max(1);
    let mut sum = 0u64;
    let mut n = 0u64;
    for px in pixels.chunks_exact(4).step_by(stride) {
        // Integer Rec.601: (77 R + 150 G + 29 B) >> 8 ≈ luma in 0..=255.
        sum += (77 * px[0] as u64 + 150 * px[1] as u64 + 29 * px[2] as u64) >> 8;
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    (sum as f32 / n as f32) / 255.0
}

/// The poster walk's accept rule for one sampled frame.
pub fn poster_frame_bright_enough(pixels: &[u8]) -> bool {
    mean_luma_rgba8(pixels, 8) > POSTER_LUMA_MIN
}

/// Mean and standard deviation of Rec.601 luma over a tightly packed RGBA8
/// buffer, both in 0..=1, subsampled every `stride`-th pixel. One pass
/// (sum + sum-of-squares). The std-dev is the frame's *contrast* — near-zero for
/// a black or flat single-color frame, high for a detailed scene — which is what
/// separates a real poster frame from a studio-logo-on-black or a fade.
pub fn luma_stats_rgba8(pixels: &[u8], stride: usize) -> (f32, f32) {
    let stride = stride.max(1);
    let mut sum = 0f64;
    let mut sum_sq = 0f64;
    let mut n = 0u64;
    for px in pixels.chunks_exact(4).step_by(stride) {
        let y = ((77 * px[0] as u32 + 150 * px[1] as u32 + 29 * px[2] as u32) >> 8) as f64 / 255.0;
        sum += y;
        sum_sq += y * y;
        n += 1;
    }
    if n == 0 {
        return (0.0, 0.0);
    }
    let mean = sum / n as f64;
    let var = (sum_sq / n as f64 - mean * mean).max(0.0);
    (mean as f32, var.sqrt() as f32)
}

/// Score a candidate poster frame — higher is a better poster. Combines
/// *brightness* (must clear the black floor) with *contrast* (luma std-dev, the
/// prior-art "interestingness" measure ffmpegthumbnailer / imagorvideo use to
/// dodge black and flat frames). A frame below [`POSTER_LUMA_MIN`] scores near
/// zero so a black lead-in never wins; among visible frames the one with the
/// most detail wins, with a light bonus for brightness. Range ~0..~0.4.
pub fn poster_frame_score(pixels: &[u8]) -> f32 {
    let (mean, std) = luma_stats_rgba8(pixels, 8);
    if mean < POSTER_LUMA_MIN {
        // Rank black/fade frames strictly below any visible one, but keep them
        // ordered by brightness so the fallback still picks the least-black.
        return mean * 0.01;
    }
    std + 0.15 * mean
}

/// Score at/above which a frame is a *clearly good* poster and the walk stops
/// early (no need to seek deeper). A detailed scene's luma std-dev is typically
/// 0.15–0.3; a near-flat or dim frame sits well below this.
pub const POSTER_GOOD_SCORE: f32 = 0.12;

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u32, h: u32) -> VideoFrame {
        VideoFrame {
            session_id: VideoSessionId(1),
            seek_generation: SeekGeneration::FIRST,
            pts: Duration::ZERO,
            width: w,
            height: h,
            format: PixelFormat::Rgba8,
            pixels: vec![0; (w * h * 4) as usize],
            color: VideoColorInfo::srgb(),
        }
    }

    #[test]
    fn well_formed_checks_geometry_against_buffer() {
        assert!(frame(4, 3).is_well_formed());
        let mut bad = frame(4, 3);
        bad.pixels.pop();
        assert!(!bad.is_well_formed());
        let zero = frame(4, 0);
        assert!(!zero.is_well_formed());
    }

    #[test]
    fn byte_len_matches_rgba8_geometry() {
        assert_eq!(frame(1920, 1080).byte_len(), 1920 * 1080 * 4);
    }

    /// Task 79.10: the NV12 contract — 12 bpp packed planes, even dims required.
    #[test]
    fn nv12_frame_bytes_and_well_formedness() {
        assert_eq!(
            PixelFormat::Nv12.frame_bytes(3840, 2160),
            3840 * 2160 * 3 / 2
        );
        assert_eq!(PixelFormat::Rgba8.frame_bytes(2, 2), 16);
        assert_eq!(PixelFormat::Rgba16F.frame_bytes(2, 2), 32);

        let nv12 = |w: u32, h: u32| VideoFrame {
            format: PixelFormat::Nv12,
            pixels: vec![0; PixelFormat::Nv12.frame_bytes(w, h)],
            width: w,
            height: h,
            ..frame(w, h)
        };
        assert!(nv12(4, 2).is_well_formed());
        assert!(
            !nv12(3, 2).is_well_formed() && !nv12(4, 3).is_well_formed(),
            "odd dimensions are rejected"
        );
        let mut short = nv12(4, 2);
        short.pixels.pop();
        assert!(!short.is_well_formed());
    }

    #[test]
    fn seek_generation_orders_and_never_wraps() {
        let g0 = SeekGeneration::FIRST;
        let g1 = g0.next();
        assert!(g1 > g0);
        // Saturation at the ceiling (unreachable in practice, but the contract holds).
        let max = SeekGeneration(u64::MAX);
        assert_eq!(max.next(), max);
    }

    #[test]
    fn mean_luma_classifies_black_and_bright_frames() {
        let black = vec![0u8; 64 * 64 * 4];
        assert_eq!(mean_luma_rgba8(&black, 1), 0.0);
        assert!(!poster_frame_bright_enough(&black));

        let white: Vec<u8> = [255u8, 255, 255, 255].repeat(64 * 64);
        assert!(mean_luma_rgba8(&white, 1) > 0.95);
        assert!(poster_frame_bright_enough(&white));

        // Mid-gray sits well above the black floor.
        let gray: Vec<u8> = [128u8, 128, 128, 255].repeat(64 * 64);
        let l = mean_luma_rgba8(&gray, 1);
        assert!((0.4..0.6).contains(&l), "{l}");

        // Near-black (fade-in) stays under the floor.
        let faint: Vec<u8> = [8u8, 8, 8, 255].repeat(64 * 64);
        assert!(!poster_frame_bright_enough(&faint));

        // The raised floor (0.06 → 0.10): a frame at ~8% gray used to pass and
        // read as a near-black tile — it must be walked past now, while ~12%
        // (dim but visible) is accepted. Uniform (v,v,v) has mean luma v/255.
        let dim_8pct: Vec<u8> = [20u8, 20, 20, 255].repeat(64 * 64);
        assert!(
            !poster_frame_bright_enough(&dim_8pct),
            "~8% gray is still a near-black tile — keep walking"
        );
        let dim_12pct: Vec<u8> = [30u8, 30, 30, 255].repeat(64 * 64);
        assert!(
            poster_frame_bright_enough(&dim_12pct),
            "~12% gray is visible content — accept"
        );
    }

    #[test]
    fn mean_luma_subsampling_matches_full_scan_on_uniform_frames() {
        let px: Vec<u8> = [200u8, 100, 50, 255].repeat(128 * 128);
        let full = mean_luma_rgba8(&px, 1);
        let sub = mean_luma_rgba8(&px, 8);
        assert!((full - sub).abs() < 0.01);
        // Degenerate inputs never panic.
        assert_eq!(mean_luma_rgba8(&[], 8), 0.0);
        assert_eq!(mean_luma_rgba8(&[1, 2, 3], 8), 0.0);
    }

    #[test]
    fn poster_score_prefers_detailed_scenes_over_flat_or_black() {
        let w = 64usize;
        let h = 64usize;

        // Black and near-black (fade) frames score essentially zero.
        let black = vec![0u8; w * h * 4];
        assert!(poster_frame_score(&black) < 0.01);
        let faint: Vec<u8> = [8u8, 8, 8, 255].repeat(w * h);
        assert!(poster_frame_score(&faint) < 0.01);

        // A studio-logo-on-black: a small bright patch on an otherwise black
        // field. Mean luma stays under the floor, so it is *not* a good poster
        // and scores near zero — the whole point of the contrast+brightness gate.
        let mut logo = vec![0u8; w * h * 4];
        for y in 28..34 {
            for x in 28..34 {
                let i = (y * w + x) * 4;
                logo[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        assert!(
            poster_frame_score(&logo) < POSTER_GOOD_SCORE,
            "logo-on-black must not read as a good poster"
        );

        // A flat mid-gray frame is visible but dull (no contrast): above black,
        // but below the good-poster bar, so the walk keeps looking.
        let gray: Vec<u8> = [128u8, 128, 128, 255].repeat(w * h);
        let gray_score = poster_frame_score(&gray);
        assert!(gray_score > poster_frame_score(&black));
        assert!(
            gray_score < POSTER_GOOD_SCORE,
            "flat gray is not 'good' {gray_score}"
        );

        // A high-contrast, detailed frame (checkerboard) clears the good bar and
        // outscores every frame above.
        let mut scene = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let v = if (x + y) % 2 == 0 { 220 } else { 20 };
                let i = (y * w + x) * 4;
                scene[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let scene_score = poster_frame_score(&scene);
        assert!(
            scene_score >= POSTER_GOOD_SCORE,
            "detailed scene should be 'good' {scene_score}"
        );
        assert!(scene_score > gray_score && scene_score > poster_frame_score(&logo));
    }

    #[test]
    fn luma_stats_zero_for_flat_frames_nonzero_for_varied() {
        let flat: Vec<u8> = [100u8, 100, 100, 255].repeat(32 * 32);
        let (mean, std) = luma_stats_rgba8(&flat, 1);
        assert!((mean - 100.0 / 255.0).abs() < 0.01);
        assert!(std < 0.001, "a flat frame has ~zero contrast, got {std}");
        // Degenerate inputs never panic.
        assert_eq!(luma_stats_rgba8(&[], 8), (0.0, 0.0));
    }

    #[test]
    fn srgb_color_info_is_passthrough_assumption() {
        let c = VideoColorInfo::srgb();
        assert_eq!(c.cicp, None);
        assert!(c.full_range);
        assert_eq!(c.transform, ColorTransform::srgb());
    }
}
