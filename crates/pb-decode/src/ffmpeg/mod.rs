//! The shared FFmpeg media backend (task #84, plan §3).
//!
//! Submodule map (mirrors the plan's `ffmpeg/{…}` layout):
//! - [`init`] — process-wide `av_*` init + log quieting (idempotent).
//! - [`io`] — opening a [`crate::VideoInput`]: path or **in-RAM archive bytes**
//!   (a custom `AVIOContext`, plan §6), with interrupt-callback cancellation
//!   and a per-operation watchdog so hostile input can't block forever.
//! - [`probe`] — stream selection (best non-attached-picture video stream),
//!   rotation / SAR / duration / fps / audio-presence facts.
//! - [`color`] — FFmpeg color metadata (H.273 CICP) → [`crate::ColorTransform`]
//!   / [`crate::video::VideoColorInfo`], plus correct swscale coefficients.
//! - [`poster`] *(`ffvideo`)* — the first-non-black poster walk + input probe.
//! - [`video_producer`] *(`ffvideo`)* — the streaming producer speaking the
//!   `VideoProducerEvent`/`Msg` protocol.
//!
//! Everything here is read-only and RAM-only: the no-trace guarantee (privacy
//! task #2) holds on every path in this tree.

pub mod color;
pub mod convert;
pub mod init;
pub mod io;
pub mod probe;

#[cfg(feature = "ffvideo")]
pub mod poster;
#[cfg(feature = "ffvideo")]
pub mod video_producer;
