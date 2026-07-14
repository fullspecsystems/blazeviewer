//! The FFmpeg **read-only probes**: one open's stream facts, and the Inspector's
//! richer facts-plus-track-catalog read.
//!
//! Split out of [`super::poster`] (task #100) so a build can have FFmpeg's *container
//! reading* without its *decoding*. That is exactly what Windows wants: Media Foundation
//! owns poster + playback (hardware decode, low latency), while FFmpeg is here only
//! because MF's media sources do not model subtitle tracks at all — a source's
//! presentation descriptor reports 3 streams for a file with 7. See the `ffprobe`
//! feature.
//!
//! Nothing here decodes a frame, so it is safe against a demuxers-only FFmpeg build
//! (`--disable-everything --enable-demuxer=…`), which `poster.rs` would not be.
//!
//! Read-only and RAM-only on every path: the no-trace guarantee (privacy task #2) holds.

use ffmpeg_next as ff;

use super::io::FfInput;
use super::probe::video_facts;
use crate::video::{VideoDetailsProbe, VideoInput};
use crate::{DecodeError, VideoStreamInfo};

/// One read-only open's stream facts (inspector parity with the Windows
/// `probe_video_input` / macOS `probe_video_stream`).
///
/// **This is the poster/prefetch path** — it runs for every prefetched video, opened or
/// not, so it stays exactly as cheap as it was: no track enumeration happens here. The
/// Inspector's richer read is [`ff_probe_video_details`].
pub fn ff_probe_video_input(input: &VideoInput) -> Result<VideoStreamInfo, DecodeError> {
    let mut opened = FfInput::open(input, None).map_err(DecodeError::Corrupt)?;
    let facts = video_facts(opened.ctx()).map_err(DecodeError::Corrupt)?;
    stream_info(&mut opened, &facts)
}

/// **The track catalog alone** (task #100) — the only thing Windows wants from FFmpeg.
///
/// Deliberately *not* [`ff_probe_video_details`], and the difference is load-bearing
/// rather than tidiness: that one also builds a [`VideoStreamInfo`], which needs
/// [`decoder_for`] to read the video codec's colour metadata. A demuxers-only FFmpeg
/// (`--disable-everything`, no video decoders — the +3.06 MB Windows build) **has no
/// H.264 decoder**, so it fails there and takes the catalog down with it. Reading the
/// stream table needs no decoder at all.
///
/// On Windows the facts come from Media Foundation's reader anyway — the same one that
/// decodes the poster, which is what keeps rotation/colour identical to it. So this asks
/// FFmpeg for exactly the part MF cannot answer, and nothing else.
///
/// Off-thread only.
pub fn ff_probe_tracks(
    input: &VideoInput,
    generation: u64,
) -> Result<crate::tracks::MediaTrackCatalog, DecodeError> {
    // `FfInput::open` already ran avformat_find_stream_info, so the stream table is
    // parsed and the catalog is a pure read over it.
    let mut opened = FfInput::open(input, None).map_err(DecodeError::Corrupt)?;
    Ok(super::tracks::catalog_from_input(opened.ctx(), generation))
}

/// The **Details** probe (task #98): basic facts + the full track catalog, from one
/// open. Off-thread only — never call this from the event loop or the poster path.
///
/// ⚠ Needs a **video decoder** (via [`stream_info`] → [`decoder_for`]) for the colour
/// metadata, so it is not usable on a demuxers-only build — see [`ff_probe_tracks`],
/// which is what Windows calls.
pub fn ff_probe_video_details(
    input: &VideoInput,
    generation: u64,
) -> Result<VideoDetailsProbe, DecodeError> {
    let mut opened = FfInput::open(input, None).map_err(DecodeError::Corrupt)?;
    let facts = video_facts(opened.ctx()).map_err(DecodeError::Corrupt)?;
    // One open serves both reads: the catalog walks the same already-parsed stream
    // table `video_facts` selected from.
    let tracks = super::tracks::catalog_from_input(opened.ctx(), generation);
    let video = stream_info(&mut opened, &facts)?;
    Ok(VideoDetailsProbe { video, tracks })
}

/// The shared `VideoFacts` → [`VideoStreamInfo`] read, so the basic and details probes
/// can never drift on codec/rotation/color policy.
fn stream_info(
    opened: &mut FfInput<'static>,
    facts: &super::probe::VideoFacts,
) -> Result<VideoStreamInfo, DecodeError> {
    // Decoder-reported color under the shared fallback policy (no frame decode
    // here — the probe is a ~header-only read, like the Windows one).
    let decoder = decoder_for(opened.ctx(), facts.index)?;
    // Posters are stills → always RGBA (no planar GPU path).
    let conv = super::convert::FrameConverter::new(
        (facts.width, facts.height),
        (1, 1),
        0,
        &decoder,
        false,
        false,
    );
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

/// A decoder *context* for the stream — built from the container's parameters, never
/// opened against a frame. On a demuxers-only build this still yields the codec's
/// declared color/format metadata, which is all [`stream_info`] reads.
///
/// **Not** `poster::decoder_for`, and the near-duplication is deliberate: that
/// one attaches hardware decode (VideoToolbox / VAAPI) because it decodes real frames
/// for the poster walk. This one must not. `super::hw` is `#[cfg(feature = "ffvideo")]`
/// and this module compiles on a bare `ffprobe` build, so it cannot even name it — and a
/// demuxers-only FFmpeg has no decoder to accelerate anyway. Merging the two would break
/// the Windows build (task #100).
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
