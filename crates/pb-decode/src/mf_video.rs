//! Windows **Media Foundation** backend for the Live Photo motion `.mov` (tasks #39 / #69).
//!
//! The Windows sibling of the macOS AVAssetReader decoder (`livephoto.rs`) and the Linux
//! FFmpeg one (`ff_live.rs`), behind the same [`decode_live_motion_streaming`] seam: an
//! `IMFSourceReader` reads the `.mov`'s video track and the OS decodes each sample to
//! RGB32 (H.264 ships with Windows; HEVC — iPhone 7 and later — needs the **HEVC Video
//! Extensions** Store package). A missing codec surfaces as a decode error, never a crash
//! — exactly the WIC HEIC/AVIF pattern (`wic.rs`), whose codec-extension caveat this shares.
//!
//! **Streaming** (task #69): the Source Reader is already a strictly sequential decoder,
//! so each decoded frame is emitted through the [`MotionChunk`] contract the moment it's
//! converted — playback starts on the first frame instead of after the whole clip. The
//! batch [`decode_live_motion`] is a thin [`MotionCollector`] over the stream.
//!
//! What the Source Reader gives us for free (vs the macOS path, which hand-rolls each):
//! `MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING` inserts the OS video processor,
//! which applies the container **rotation** (a portrait clip comes out upright) and does
//! YUV→RGB32 — so no `rotate_rgba` / `transform_to_quadrant` here. Per-frame timing comes
//! from `IMFSample::GetSampleDuration`; color is read honestly from the *native* media
//! type (the processor fixes the matrix but not the primaries — see [`native_color`]).
//! RAM is bounded the same way everywhere: frames are downscaled to the `max_long_edge`
//! cap as they arrive, and the frame / byte budgets are checked before each emit. Reads
//! only, RAM-only — the no-trace guarantee (privacy #2) holds.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFSample, IMFSourceReader, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Video, MFStartup, MFVideoFormat_RGB32,
    MFVideoPrimaries, MFVideoPrimaries_BT2020, MFVideoPrimaries_BT709, MFVideoPrimaries_DCI_P3,
    MFVideoTransFunc_2084, MFVideoTransFunc_709, MFVideoTransFunc_HLG, MFVideoTransFunc_sRGB,
    MFVideoTransferFunction, MFSTARTUP_LITE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_PRIMARIES,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::animation::{MotionCollector, MAX_DECODED_BYTES};
use crate::{
    common, AnimFrame, Animation, ColorTransform, DecodeError, FitBox, MotionChunk, MotionHeader,
};

/// Safety cap on decoded frames — a real Live Photo is ~3 s, so this is only a
/// runaway guard (a malformed/long `.mov`). Hitting it flags the result truncated.
/// Mirrors the macOS decoder's cap.
const MAX_MOTION_FRAMES: usize = 600;

/// Fallback per-frame delay when a sample carries no usable duration (30 fps).
const FALLBACK_DELAY: Duration = Duration::from_nanos(33_333_333);

/// Streaming Live Photo motion decode (task #69): decode the `.mov` at `path` and hand each
/// frame to `emit` as it's ready, per the [`MotionChunk`] state-machine contract. Frames are
/// capped to `max_long_edge` (decode-to-fit) and bounded by [`MAX_MOTION_FRAMES`] /
/// [`MAX_DECODED_BYTES`]. Bails silently (emitting no terminal chunk) once `cancel` is set —
/// the consumer has already dropped the stream. Windows-only.
pub fn decode_live_motion_streaming(
    path: &Path,
    max_long_edge: u32,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(MotionChunk),
) {
    ensure_mf();

    // A COM failure that escapes `stream_inner` becomes a terminal `Failed` here (with the
    // friendly HEVC-extensions text). `stream_inner` never emits a terminal before returning
    // `Err`, so this can't produce a double terminal.
    if let Err(e) = unsafe { stream_inner(path, max_long_edge, cancel, emit) } {
        emit(MotionChunk::Failed(DecodeError::Corrupt(mf_msg(e))));
    }
}

/// Decode the Live Photo motion `.mov` at `path` into a whole [`Animation`], capping each
/// frame's long edge to `max_long_edge` px. A collector over the streaming decoder (which
/// also validates the chunk contract) — kept for the batch callers (`decode_motion_job`,
/// the `live_probe` example).
pub fn decode_live_motion(path: &Path, max_long_edge: u32) -> Result<Animation, DecodeError> {
    let mut collector = MotionCollector::default();
    decode_live_motion_streaming(path, max_long_edge, &AtomicBool::new(false), &mut |chunk| {
        collector.push(chunk)
    });
    collector.finish()
}

/// COM + Media Foundation init for the calling worker thread. Shared by every MF
/// consumer in the crate (Live Photo motion, the video poster/metadata probes —
/// task #79). Both results are deliberately tolerated — S_FALSE / RPC_E_CHANGED_MODE
/// for COM (see `wic.rs`), and MFStartup is process-wide ref-counted (never shut
/// down; the process lifetime owns it).
pub(crate) fn ensure_mf() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    static MF_INIT: Once = Once::new();
    MF_INIT.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    });
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

/// Drive the Source Reader end-to-end, emitting the [`MotionChunk`] stream. `unsafe` because
/// it drives COM. Returns `Err` only for an *unexpected* COM failure (setup or `ReadSample`) —
/// the caller turns that into the terminal `Failed`; every semantic outcome (no frames, size
/// change, EOF, truncation) is emitted here and returns `Ok`.
unsafe fn stream_inner(
    path: &Path,
    max_long_edge: u32,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(MotionChunk),
) -> windows::core::Result<()> {
    // Eager-prep streams get superseded fast — don't even open the asset if already stale.
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }

    // Advanced video processing inserts the OS video processor: YUV→RGB32 conversion
    // plus the container's rotation applied for us (a portrait-shot Live Photo comes out
    // upright, like AVFoundation's `appliesPreferredTrackTransform`).
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.expect("MFCreateAttributes succeeded");
    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;

    // The source resolver takes a plain filesystem path — no file:// URI dance.
    let url = HSTRING::from(path.as_os_str());
    let reader: IMFSourceReader = MFCreateSourceReaderFromURL(&url, &attrs)?;

    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    // Read the honest color transform from the *native* type, before we force RGB32 — the
    // video processor fixes the YUV→RGB matrix but not the primaries (see `native_color`).
    let color = native_color(&reader, video);

    // Ask for RGB32 out; the reader inserts the decoder + processor to satisfy it.
    let out_type = MFCreateMediaType()?;
    out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
    reader.SetCurrentMediaType(video, None, &out_type)?;

    // The negotiated frame geometry: size packed as (width << 32) | height, and the stride
    // — negative = bottom-up rows (DIB legacy), absent = tightly packed.
    let cur = reader.GetCurrentMediaType(video)?;
    let packed = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
    let (w, h) = ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32);
    if w == 0 || h == 0 {
        emit(MotionChunk::Failed(DecodeError::Corrupt(
            "Live Photo motion has no video frames".into(),
        )));
        return Ok(());
    }
    let stride = cur
        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
        .map(|s| s as i32)
        .unwrap_or((w * 4) as i32);

    let fit = FitBox {
        max_width: max_long_edge.max(1),
        max_height: max_long_edge.max(1),
    };

    // Cancellation can arrive during setup (eager prep superseded before the first read).
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }

    // Sequential sample pump — no per-frame seeks. The header is derived from the first
    // decoded frame's dims (the source of truth); every later frame must match.
    let mut count: usize = 0;
    let mut bytes: u64 = 0;
    let mut header_dims: Option<(u32, u32)> = None;
    let mut truncated = false;
    loop {
        // Check cancel at the top of each read → silent return (no terminal chunk; the
        // consumer has already dropped the stream). We stop *emitting* immediately here.
        //
        // Dropping the `IMFSourceReader` is the cleanup, and it can block ~1 s on the Source
        // Reader's internal worker-queue shutdown (measured constant regardless of clip length
        // or DXVA — an idle wait, not decode; a media-source `Shutdown()` didn't shorten it).
        // That's harmless: this decode runs on the fire-and-forget worker `start_live_stream`
        // spawned, which nothing joins, so the teardown wait is fully off the event loop and
        // costs no CPU — it just delays the orphaned worker's exit, never a keypress or a draw.
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
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
        let _ = ts; // presentation timestamp unused — pacing is per-sample duration below
        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
        // A gap/format-change tick can deliver no sample without ending the stream.
        let Some(sample) = sample else { continue };

        let rgba = sample_to_rgba(&sample, w, h, stride)?;
        let (rgba, fw, fh) = match common::downscale_to_fit(rgba, w, h, fit) {
            Ok(r) => r,
            Err(e) => {
                emit(MotionChunk::Failed(e));
                return Ok(());
            }
        };

        // Header from the first decoded frame (not the negotiated media type — a mid-stream
        // `MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED` is real; deriving from decoded output
        // makes it a non-issue). Later frames must match, or the stream fails.
        if header_dims.is_none() {
            header_dims = Some((fw, fh));
            emit(MotionChunk::Header(MotionHeader {
                width: fw,
                height: fh,
                color,
                codec: "Live Photo",
            }));
        } else if header_dims != Some((fw, fh)) {
            emit(MotionChunk::Failed(DecodeError::Corrupt(
                "Live Photo motion frame size changed mid-stream".into(),
            )));
            return Ok(());
        }

        // Per-sample duration (100 ns units) — the direct analog of the macOS
        // `CMSampleBufferGetDuration` call. Clamp to sane bounds; fall back on error/zero.
        let delay = sample
            .GetSampleDuration()
            .ok()
            .filter(|&d| d > 0)
            .map(|d| {
                Duration::from_nanos(d as u64 * 100)
                    .clamp(Duration::from_millis(1), Duration::from_secs(2))
            })
            .unwrap_or(FALLBACK_DELAY);

        // Frame + byte budgets, checked *before* emitting — the consumer retains every frame,
        // so the caps bound what it can ever hold. Truncation drives `Done`, not `Failed`
        // (≥1 frame exists by construction — a single fit-capped frame can't exceed the byte
        // budget, and the frame cap only trips after that many frames emitted).
        let next_bytes = bytes.saturating_add(rgba.len() as u64);
        if count >= MAX_MOTION_FRAMES || next_bytes > MAX_DECODED_BYTES {
            truncated = true;
            break;
        }
        bytes = next_bytes;
        count += 1;
        emit(MotionChunk::Frame(AnimFrame {
            rgba,
            width: fw,
            height: fh,
            delay,
        }));
    }

    // Terminal.
    if truncated {
        emit(MotionChunk::Done {
            loop_count: 1,
            truncated: true,
        });
        return Ok(());
    }
    if count == 0 {
        emit(MotionChunk::Failed(DecodeError::Corrupt(
            "Live Photo motion decoded no frames".into(),
        )));
        return Ok(());
    }
    // A Live Photo plays once and stops (finite loop = 1), like every motion backend.
    emit(MotionChunk::Done {
        loop_count: 1,
        truncated: false,
    });
    Ok(())
}

/// Read the source clip's real color from the **native** (pre-conversion) media type and map
/// it to a [`ColorTransform`]. The video processor converts YUV→RGB with the correct matrix
/// but does **not** remap primaries, so a Display-P3 clip (both HEVC corpus clips are P3)
/// would look oversaturated if tagged plain sRGB. The `MFVideoPrimaries`/`MFVideoTransferFunction`
/// enum values are **not** CICP code points, so map them explicitly; anything unknown (or a
/// missing attribute) falls back to sRGB passthrough, as does anything `from_cicp`/moxcms can't
/// build (including the PQ/HLG transfers this 8-bit RGB32 path SDR-clamps by design).
pub(crate) unsafe fn native_color(reader: &IMFSourceReader, video: u32) -> ColorTransform {
    let Ok(native) = reader.GetNativeMediaType(video, 0) else {
        return ColorTransform::srgb();
    };
    let (Ok(p), Ok(t)) = (
        native.GetUINT32(&MF_MT_VIDEO_PRIMARIES),
        native.GetUINT32(&MF_MT_TRANSFER_FUNCTION),
    ) else {
        return ColorTransform::srgb();
    };
    let primaries = MFVideoPrimaries(p as i32);
    let transfer = MFVideoTransferFunction(t as i32);
    // Map the `MFVideoPrimaries`/`MFVideoTransferFunction` enum values to CICP code points —
    // they are NOT the same numbering. Match on the *named* constants, never a hardcoded
    // integer (the CICP↔MF values diverge, e.g. MF's PQ/HLG are 15/16, not CICP's 16/18), so
    // this stays correct if the crate's enum is regenerated.
    let cicp_p: u8 = if primaries == MFVideoPrimaries_BT709 {
        1
    } else if primaries == MFVideoPrimaries_BT2020 {
        9
    } else if primaries == MFVideoPrimaries_DCI_P3 || p == 13 {
        // Display-P3 (D65). Windows 11's HEVC decoder reports the raw value **13** for Apple
        // Display-P3 Live Photos; the (stale) windows-crate 0.62 enum only names 13 as
        // `MFVideoPrimaries_Last`, one past `ACES(12)`, so match the literal. Verified on the
        // corpus: the paired still HEIC decodes to the Display-P3 (CICP 12) primaries matrix,
        // so tagging the motion sRGB here would oversaturate it. MF's named `DCI_P3` (CICP 11)
        // maps here too — same RGB primaries, DCI vs D65 white, near-identical for SDR.
        12
    } else {
        return ColorTransform::srgb();
    };
    let cicp_t: u8 = if transfer == MFVideoTransFunc_709 {
        1
    } else if transfer == MFVideoTransFunc_sRGB {
        13
    } else if transfer == MFVideoTransFunc_2084 {
        16
    } else if transfer == MFVideoTransFunc_HLG {
        18
    } else {
        return ColorTransform::srgb();
    };
    // Matrix 0 (identity/RGB) + full range: the buffer is already RGB — only primaries and
    // transfer matter for the shader transform.
    ColorTransform::from_cicp(cicp_p, cicp_t, 0, true)
}

/// Copy one RGB32 sample out as straight-alpha RGBA8, honoring the row stride
/// (padding and bottom-up both occur in the wild) and forcing alpha opaque
/// (RGB32's fourth byte is undefined).
pub(crate) unsafe fn sample_to_rgba(
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn collect(path: &Path, cancel: &AtomicBool) -> Vec<MotionChunk> {
        let mut chunks = Vec::new();
        decode_live_motion_streaming(path, 1440, cancel, &mut |c| chunks.push(c));
        chunks
    }

    /// A missing file must produce exactly one `Failed` and nothing else.
    #[test]
    fn missing_file_fails_cleanly() {
        let chunks = collect(
            Path::new(r"C:\nonexistent\definitely\not\here.mov"),
            &AtomicBool::new(false),
        );
        assert_eq!(chunks.len(), 1);
        assert!(matches!(chunks[0], MotionChunk::Failed(_)));
    }

    /// Garbage bytes must produce exactly one `Failed` — no Header, no Frame, no panic.
    #[test]
    fn garbage_file_fails_cleanly() {
        let dir = std::env::temp_dir().join("pb_mf_video_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("garbage.mov");
        std::fs::write(&path, b"this is definitely not a quicktime movie").unwrap();
        let chunks = collect(&path, &AtomicBool::new(false));
        let _ = std::fs::remove_file(&path);
        assert_eq!(chunks.len(), 1, "expected a single Failed chunk");
        assert!(matches!(chunks[0], MotionChunk::Failed(_)));
    }

    /// A pre-set cancel flag must emit nothing at all (silent non-terminal return).
    #[test]
    fn preset_cancel_emits_nothing() {
        let mov = match std::env::var("PB_LIVE_TEST_MOV") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("PB_LIVE_TEST_MOV not set — skipping");
                return;
            }
        };
        let chunks = collect(Path::new(&mov), &AtomicBool::new(true));
        assert!(chunks.is_empty(), "cancelled stream must emit no chunks");
    }

    /// Cancelling after a few frames stops the stream with no terminal chunk.
    #[test]
    fn midstream_cancel_has_no_terminal() {
        let mov = match std::env::var("PB_LIVE_TEST_MOV") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("PB_LIVE_TEST_MOV not set — skipping");
                return;
            }
        };
        let cancel = AtomicBool::new(false);
        let mut chunks: Vec<MotionChunk> = Vec::new();
        let mut frames = 0usize;
        decode_live_motion_streaming(Path::new(&mov), 1440, &cancel, &mut |c| {
            if matches!(c, MotionChunk::Frame(_)) {
                frames += 1;
                if frames == 3 {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            chunks.push(c);
        });
        assert!(frames >= 3, "expected at least 3 frames before cancel");
        assert!(
            !matches!(
                chunks.last(),
                Some(MotionChunk::Done { .. }) | Some(MotionChunk::Failed(_))
            ),
            "a cancelled stream must not emit a terminal chunk"
        );
        assert!(frames <= 4, "cancel must stop the stream within a frame");
    }

    /// The full ordering contract on a real clip: Header first, Done last, ≥2 frames,
    /// loop_count 1, nothing after the terminal. Mirrors the livephoto (macOS) test.
    #[test]
    fn streaming_emits_header_then_frames_then_done_in_order() {
        let mov = match std::env::var("PB_LIVE_TEST_MOV") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("PB_LIVE_TEST_MOV not set — skipping");
                return;
            }
        };
        let chunks = collect(Path::new(&mov), &AtomicBool::new(false));
        assert!(chunks.len() >= 4, "expected header + ≥2 frames + done");
        assert!(
            matches!(chunks.first(), Some(MotionChunk::Header(_))),
            "first chunk must be the Header"
        );
        let (mut frames, mut headers) = (0usize, 0usize);
        for (i, c) in chunks.iter().enumerate() {
            match c {
                MotionChunk::Header(_) => headers += 1,
                MotionChunk::Frame(f) => {
                    frames += 1;
                    assert!(!f.rgba.is_empty() && f.width > 0 && f.height > 0);
                }
                MotionChunk::Done {
                    loop_count,
                    truncated,
                } => {
                    assert_eq!(i, chunks.len() - 1, "Done must be the final chunk");
                    assert_eq!(*loop_count, 1, "a Live Photo plays once");
                    assert!(!truncated, "a real Live Photo must not truncate");
                }
                MotionChunk::Failed(e) => panic!("stream failed: {e}"),
            }
        }
        assert_eq!(headers, 1, "exactly one Header");
        assert!(frames >= 2, "expected at least 2 frames, got {frames}");
    }

    /// The batch wrapper agrees with the stream (same clip through both paths).
    #[test]
    fn batch_wrapper_collects_the_stream() {
        let mov = match std::env::var("PB_LIVE_TEST_MOV") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("PB_LIVE_TEST_MOV not set — skipping");
                return;
            }
        };
        let anim = decode_live_motion(Path::new(&mov), 1440).expect("batch decode");
        assert!(anim.frame_count() >= 2);
        assert_eq!(anim.loop_count, 1);
        assert!(anim.width > 0 && anim.height > 0);
        assert!(anim.width.max(anim.height) <= 1440, "decode-to-fit cap");
        let f = &anim.frames[0];
        assert_eq!((f.width, f.height), (anim.width, anim.height));
    }
}
