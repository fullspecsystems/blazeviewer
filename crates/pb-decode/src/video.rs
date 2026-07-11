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

/// Source color for a video frame: the resolved shader transform for the pixels as
/// emitted, plus the container/decoder-reported CICP code points kept verbatim so a
/// future fp16/NV12 backend (or a diagnostics panel) can re-derive without re-probing.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoColorInfo {
    /// Source→sRGB transform for the emitted pixels, applied in-shader — same
    /// mechanism as stills ([`crate::DecodedImage::color`]). For tier-2 SDR output
    /// the producer has already applied (or had the OS apply) the YUV matrix; this
    /// carries primaries + transfer.
    pub transform: ColorTransform,
    /// CICP code points as reported by the source (H.273): colour_primaries,
    /// transfer_characteristics, matrix_coefficients. `None` = the container didn't
    /// say (the transform then documents the assumption the producer made).
    pub cicp: Option<(u8, u8, u8)>,
    /// Full-range flag as reported (or assumed) for the source YUV.
    pub full_range: bool,
}

impl VideoColorInfo {
    /// sRGB passthrough — the assumption when a source carries no color metadata.
    pub fn srgb() -> Self {
        VideoColorInfo {
            transform: ColorTransform::srgb(),
            cicp: None,
            full_range: true,
        }
    }
}

/// One decoded, display-ready video frame. The producer→session queue element.
///
/// Pixels are tightly packed `width * height * format.bytes_per_pixel()` bytes,
/// already fitted to the session's fixed output geometry (never re-fitted mid
/// session; window resizes rescale on the GPU and apply a new fit on next play).
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

    /// Structural sanity: pixel buffer matches the declared geometry/format.
    pub fn is_well_formed(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.pixels.len()
                == self.width as usize * self.height as usize * self.format.bytes_per_pixel()
    }
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
    fn srgb_color_info_is_passthrough_assumption() {
        let c = VideoColorInfo::srgb();
        assert_eq!(c.cicp, None);
        assert!(c.full_range);
        assert_eq!(c.transform, ColorTransform::srgb());
    }
}
