//! Windows **Media Foundation** backend for the Live Photo motion `.mov` (task #39).
//!
//! The Windows mirror of the macOS AVFoundation decoder (`livephoto.rs`), behind the
//! same [`decode_live_motion`] seam: an `IMFSourceReader` reads the `.mov`'s video
//! track and the OS decodes each sample to RGB32 (H.264 ships with Windows; HEVC —
//! iPhone 7 and later — needs the **HEVC Video Extensions** Store package). A missing
//! codec surfaces as a decode error, never a crash — exactly the WIC HEIC/AVIF
//! pattern (`wic.rs`), whose codec-extension caveat this shares.
//!
//! Differences from the macOS spike, both improvements the Source Reader gives for
//! free: frames carry their **true per-sample timestamps** (AVFoundation sampled at a
//! constant nominal rate), and decode is strictly sequential (no per-frame seek).
//! RAM is bounded the same way: frames are downscaled to the `max_long_edge` cap
//! (via the shared Lanczos `downscale_to_fit`) as they arrive, so the resident RGBA
//! sequence never exceeds ~0.5 GB. Reads only, RAM-only — the no-trace guarantee
//! (privacy #2) holds.

use std::path::Path;
use std::sync::Once;
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFSample, IMFSourceReader, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Video, MFStartup, MFVideoFormat_RGB32, MFSTARTUP_LITE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::{common, AnimFrame, Animation, AnimationKind, ColorTransform, DecodeError, FitBox};

/// Safety cap on decoded frames — a real Live Photo is ~3 s, so this is only a
/// runaway guard (a malformed/long `.mov`). Hitting it flags the result truncated.
/// Mirrors the macOS decoder's cap.
const MAX_MOTION_FRAMES: usize = 600;

/// Fallback per-frame delay when a sample carries no usable timestamp (30 fps).
const FALLBACK_DELAY: Duration = Duration::from_nanos(33_333_333);

/// Decode the Live Photo motion `.mov` at `path` into an [`Animation`], capping each
/// frame's long edge to `max_long_edge` px (decode-to-fit → bounds RAM). Windows-only.
pub fn decode_live_motion(path: &Path, max_long_edge: u32) -> Result<Animation, DecodeError> {
    // COM + MF init per call: the animation decode runs on a worker thread that
    // starts uninitialized. Both results are deliberately tolerated — S_FALSE /
    // RPC_E_CHANGED_MODE for COM (see `wic.rs`), and MFStartup is process-wide
    // ref-counted (never shut down; the process lifetime owns it).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    static MF_INIT: Once = Once::new();
    MF_INIT.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    });

    unsafe { decode_inner(path, max_long_edge) }.map_err(|e| DecodeError::Corrupt(mf_msg(e)))?
}

/// Friendlier error text: a missing HEVC decoder surfaces as
/// `MF_E_TOPO_CODEC_NOT_FOUND`, which is the common real-world failure (iPhone 7+
/// Live Photos are HEVC; Windows ships only H.264 in the box).
fn mf_msg(e: windows::core::Error) -> String {
    // MF_E_TOPO_CODEC_NOT_FOUND
    if e.code().0 as u32 == 0xC00D_5212 {
        "Media Foundation: no codec for this video (install the HEVC Video Extensions)".to_string()
    } else {
        format!("Media Foundation: {e}")
    }
}

/// Drive the Source Reader end-to-end. `unsafe` because it drives COM.
unsafe fn decode_inner(
    path: &Path,
    max_long_edge: u32,
) -> windows::core::Result<Result<Animation, DecodeError>> {
    // Advanced video processing inserts the OS video processor: YUV→RGB32
    // conversion plus the container's rotation metadata applied for us (a
    // portrait-shot Live Photo comes out upright, like AVFoundation's
    // `appliesPreferredTrackTransform`).
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.expect("MFCreateAttributes succeeded");
    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;

    // The source resolver takes a plain filesystem path — no file:// URI dance.
    let url = HSTRING::from(path.as_os_str());
    let reader: IMFSourceReader = MFCreateSourceReaderFromURL(&url, &attrs)?;

    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    // Ask for RGB32 out; the reader inserts the decoder + processor to satisfy it.
    let out_type = MFCreateMediaType()?;
    out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
    reader.SetCurrentMediaType(video, None, &out_type)?;

    // The negotiated frame geometry: size packed as (width << 32) | height, and the
    // stride — negative = bottom-up rows (DIB legacy), absent = tightly packed.
    let cur = reader.GetCurrentMediaType(video)?;
    let packed = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
    let (w, h) = ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32);
    if w == 0 || h == 0 {
        return Ok(Err(DecodeError::Corrupt(
            "Live Photo motion has no video frames".into(),
        )));
    }
    let stride = cur
        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
        .map(|s| s as i32)
        .unwrap_or((w * 4) as i32);

    let fit = FitBox {
        max_width: max_long_edge.max(1),
        max_height: max_long_edge.max(1),
    };

    // Sequential sample pump. Timestamps are 100 ns units; each frame's display
    // time is the gap to the next sample (the last frame reuses the previous gap).
    let mut frames: Vec<AnimFrame> = Vec::new();
    let mut timestamps: Vec<i64> = Vec::new();
    let mut truncated = false;
    loop {
        if frames.len() >= MAX_MOTION_FRAMES {
            truncated = true;
            break;
        }
        let mut flags: u32 = 0;
        let mut ts: i64 = 0;
        let mut sample: Option<IMFSample> = None;
        reader.ReadSample(
            video,
            0,
            None,
            Some(&mut flags),
            Some(&mut ts),
            Some(&mut sample),
        )?;
        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
        // A gap/format-change tick can deliver no sample without ending the stream.
        let Some(sample) = sample else { continue };

        let rgba = sample_to_rgba(&sample, w, h, stride)?;
        let (rgba, fw, fh) = match common::downscale_to_fit(rgba, w, h, fit) {
            Ok(r) => r,
            Err(e) => return Ok(Err(e)),
        };
        timestamps.push(ts);
        frames.push(AnimFrame {
            rgba,
            width: fw,
            height: fh,
            delay: FALLBACK_DELAY, // real per-sample gaps assigned below
        });
    }
    if frames.is_empty() {
        return Ok(Err(DecodeError::Corrupt(
            "Live Photo motion decoded no frames".into(),
        )));
    }

    // True per-sample pacing: frame i shows until sample i+1's timestamp.
    let mut last_gap = FALLBACK_DELAY;
    for i in 0..frames.len() {
        let gap = timestamps
            .get(i + 1)
            .map(|next| (next - timestamps[i]).max(0) as u64 * 100)
            .map(Duration::from_nanos)
            .filter(|d| !d.is_zero())
            .unwrap_or(last_gap);
        frames[i].delay = gap;
        last_gap = gap;
    }

    let (width, height) = (frames[0].width, frames[0].height);
    Ok(Ok(Animation {
        kind: AnimationKind::LivePhoto,
        width,
        height,
        frames,
        // A Live Photo plays once and stops (finite loop = 1), not looping forever.
        loop_count: 1,
        codec: "Live Photo",
        // The video processor's RGB32 is nominal BT.709 — sRGB primaries/curve to
        // within a hair, so pass through (the macOS path draws into P3 instead).
        color: ColorTransform::srgb(),
        truncated,
    }))
}

/// Copy one RGB32 sample out as straight-alpha RGBA8, honoring the row stride
/// (padding and bottom-up both occur in the wild) and forcing alpha opaque
/// (RGB32's fourth byte is undefined).
unsafe fn sample_to_rgba(
    sample: &IMFSample,
    w: u32,
    h: u32,
    stride: i32,
) -> windows::core::Result<Vec<u8>> {
    let buffer = sample.ConvertToContiguousBuffer()?;
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut len: u32 = 0;
    buffer.Lock(&mut data, None, Some(&mut len))?;
    let src = std::slice::from_raw_parts(data, len as usize);

    let (w, h) = (w as usize, h as usize);
    let row_bytes = w * 4;
    let abs_stride = stride.unsigned_abs() as usize;
    let mut out = vec![0u8; row_bytes * h];
    for y in 0..h {
        // Bottom-up frames (negative stride) store row 0 last.
        let src_y = if stride < 0 { h - 1 - y } else { y };
        let Some(row) = src.get(src_y * abs_stride..src_y * abs_stride + row_bytes) else {
            break; // short buffer — keep the rows we got rather than fail the frame
        };
        let dst = &mut out[y * row_bytes..(y + 1) * row_bytes];
        for x in 0..w {
            // RGB32 is little-endian BGRX in memory.
            dst[x * 4] = row[x * 4 + 2];
            dst[x * 4 + 1] = row[x * 4 + 1];
            dst[x * 4 + 2] = row[x * 4];
            dst[x * 4 + 3] = 255;
        }
    }
    buffer.Unlock()?;
    Ok(out)
}
