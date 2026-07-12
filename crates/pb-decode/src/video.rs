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

/// YUV→RGB matrix coefficients for subsampled ([`PixelFormat::Nv12`]) frames
/// (task 79.10). Inert for RGB pixel formats (the producer already converted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020,
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
    /// Matrix coefficients for NV12 frames (see the contract above).
    pub yuv_matrix: YuvMatrix,
}

impl VideoColorInfo {
    /// sRGB passthrough — the assumption when a source carries no color metadata.
    pub fn srgb() -> Self {
        VideoColorInfo {
            transform: ColorTransform::srgb(),
            cicp: None,
            full_range: true,
            yuv_matrix: YuvMatrix::Bt709,
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

    /// Structural sanity: pixel buffer matches the declared geometry/format
    /// (and NV12's even-dimension requirement holds).
    pub fn is_well_formed(&self) -> bool {
        let even_ok = self.format != crate::PixelFormat::Nv12
            || (self.width.is_multiple_of(2) && self.height.is_multiple_of(2));
        self.width > 0
            && self.height > 0
            && even_ok
            && self.pixels.len() == self.format.frame_bytes(self.width, self.height)
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
/// this is ~6% gray. A genuinely dark first scene can still be picked (documented
/// limitation — the fallback is the last sampled frame, and night-clip fixtures in
/// the corpus keep the trade-off deliberate).
pub const POSTER_LUMA_MIN: f32 = 0.06;

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
    fn srgb_color_info_is_passthrough_assumption() {
        let c = VideoColorInfo::srgb();
        assert_eq!(c.cicp, None);
        assert!(c.full_range);
        assert_eq!(c.transform, ColorTransform::srgb());
    }
}
