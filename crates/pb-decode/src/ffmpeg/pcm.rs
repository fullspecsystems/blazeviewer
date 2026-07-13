//! Decoded audio frame → channel-interleaved f32 PCM, **by hand** — shared by
//! the Live Photo audio decoder (`ff_live`) and the streaming video-audio
//! decoder (`audio_decoder`).
//!
//! Why not swresample: under FFmpeg 8 (libavcodec 62) the deprecated
//! `channel_layout()` accessor reports an empty layout, so a swresample context
//! built from it rejects the real frames with `AVERROR_INPUT_CHANGED`. Manual
//! interleaving sidesteps the channel-layout API churn entirely, and the
//! platform sinks (AVAudioEngine, PipeWire) resample/mix to the device
//! themselves, so no rate conversion is needed here either.

use ffmpeg_next as ff;

/// Convert one decoded audio frame to channel-interleaved f32 (`[-1, 1]`),
/// appending to `out`. Handles the sample formats real containers produce
/// (AAC/Opus/Vorbis → FLTP) plus the common integer/packed variants. Returns
/// `Err(format-name)` for anything else. Bytes are read with `from_le_bytes`
/// (not a `cast_slice`) so no alignment assumption is made.
///
/// Plane access goes through the raw `extended_data` pointers with lengths we
/// compute from `samples()` — NOT `ffmpeg-next`'s `frame.data(i)`, which sizes
/// every plane from `linesize[i]`, and FFmpeg only fills `linesize[0]` for
/// audio: planes 1+ read as empty there, which silently zeroed every channel
/// but the first (stereo played left-only; a 5.1 center-channel track played
/// as silence — the owner's MKV bug, 2026-07-12).
pub fn append_interleaved_f32(
    frame: &ff::frame::Audio,
    ch: usize,
    out: &mut Vec<f32>,
) -> Result<(), String> {
    use ff::format::sample::{Sample, Type};
    let n = frame.samples();
    if ch == 0 || n == 0 {
        return Ok(());
    }
    let planar = matches!(
        frame.format(),
        Sample::U8(Type::Planar)
            | Sample::I16(Type::Planar)
            | Sample::I32(Type::Planar)
            | Sample::F32(Type::Planar)
    );
    let sample_bytes = match frame.format() {
        Sample::U8(_) => 1,
        Sample::I16(_) => 2,
        Sample::I32(_) | Sample::F32(_) => 4,
        other => return Err(format!("{other:?}")),
    };
    // A plane's valid bytes: per-channel for planar, all channels for packed.
    let plane_len = if planar {
        n * sample_bytes
    } else {
        n * ch * sample_bytes
    };
    // SAFETY: `extended_data` has one live pointer per plane (== `data` for ≤8
    // channels, heap-extended beyond); each planar plane holds ≥ `samples()`
    // samples and a packed plane `samples() × channels` — the lengths FFmpeg
    // guarantees for a decoded frame. A null plane (never seen from a real
    // decoder; belt-and-suspenders) reads as silence.
    let plane = |c: usize| -> Option<&[u8]> {
        unsafe {
            let ptr = (*frame.as_ptr()).extended_data.add(c).read();
            if ptr.is_null() {
                None
            } else {
                Some(std::slice::from_raw_parts(ptr, plane_len))
            }
        }
    };
    let decode = |b: &[u8]| -> f32 {
        match frame.format() {
            Sample::U8(_) => (b[0] as f32 - 128.0) / 128.0,
            Sample::I16(_) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            Sample::I32(_) => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            _ => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        }
    };
    out.reserve(n * ch);
    for i in 0..n {
        for c in 0..ch {
            let (plane_idx, idx) = if planar { (c, i) } else { (0, i * ch + c) };
            let off = idx * sample_bytes;
            match plane(plane_idx) {
                Some(data) if off + sample_bytes <= data.len() => {
                    out.push(decode(&data[off..off + sample_bytes]));
                }
                _ => out.push(0.0),
            }
        }
    }
    Ok(())
}
