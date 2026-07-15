//! The shared FFmpeg media backend (task #84, plan §3).
//!
//! Submodule map (mirrors the plan's `ffmpeg/{…}` layout):
//! - [`init`] — process-wide `av_*` init + log quieting (idempotent).
//! - [`io`] — opening a [`crate::VideoInput`]: path or **in-RAM archive bytes**
//!   (a custom `AVIOContext`, plan §6), with interrupt-callback cancellation
//!   and a per-operation watchdog so hostile input can't block forever.
//! - [`probe`] — stream selection (best non-attached-picture video stream),
//!   rotation / SAR / duration / fps / audio-presence facts.
//! - [`tracks`] — the audio/subtitle [`crate::MediaTrackCatalog`] (task #98); the
//!   reference backend, since FFmpeg reads the container's real stream table.
//! - [`cues`] — an *embedded* subtitle stream's timed text (task #90.2). Like
//!   [`tracks`], it needs no video decoder, so it works on the trimmed Windows build
//!   too — which is exactly why it is here and not beside [`demux`].
//! - [`color`] — FFmpeg color metadata (H.273 CICP) → [`crate::ColorTransform`]
//!   / [`crate::video::VideoColorInfo`], plus correct swscale coefficients.
//! - [`hw`] *(`ffvideo`)* — hardware decode setup (VideoToolbox / VAAPI) +
//!   the GPU-surface → CPU NV12/P010 transfer the producer feeds the converter.
//! - [`details`] — the read-only probes: one open's stream facts, and the
//!   Inspector's facts + [`tracks`] catalog. Split from [`poster`] so a build can
//!   read containers without decoding them (the `ffprobe` feature — Windows, where
//!   MF owns decode but cannot see subtitle tracks at all; task #100).
//! - [`poster`] *(`ffvideo`)* — the first-non-black poster walk.
//! - [`video_producer`] *(`ffvideo`)* — the streaming producer speaking the
//!   `VideoProducerEvent`/`Msg` protocol.
//!
//! Everything here is read-only and RAM-only: the no-trace guarantee (privacy
//! task #2) holds on every path in this tree.

// A metadata-only (`ffprobe`, no `ffvideo`) build compiles the shared helpers but uses
// only their container-reading half — the HDR / planar / scaler-colorspace paths exist
// for the producer and poster, and are genuinely unused when nothing decodes. Scoped to
// exactly that configuration, so `ffvideo` builds still get full dead-code checking;
// the alternative was cfg-gating ~20 items one by one, which would rot.
#![cfg_attr(not(feature = "ffvideo"), allow(dead_code))]

pub mod color;
pub mod convert;
pub mod cues;
pub mod details;
pub mod init;
pub mod io;
pub mod pcm;
pub mod probe;
pub mod tracks;

#[cfg(feature = "ffvideo")]
pub mod audio_decoder;
#[cfg(feature = "ffvideo")]
pub mod demux;
#[cfg(feature = "ffvideo")]
pub mod hw;
#[cfg(feature = "ffvideo")]
pub mod poster;
#[cfg(feature = "ffvideo")]
pub mod video_producer;
