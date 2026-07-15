//! Windows video **poster + metadata** probes via Media Foundation (task #79 phase 2).
//!
//! Two read-only entry points over an `IMFSourceReader`, built exactly the way the
//! future playback producer will be (advanced video processing on, all streams
//! deselected except video, color read from the *native* media type) — so the poster
//! is bit-identical in rotation + color policy to playback by construction:
//!
//! * [`probe_video_stream`] — open the container and report what's in it (codec,
//!   dimensions, frame rate, duration, audio presence, color). ~15–25 ms (spike-
//!   measured); never decodes a frame, never reads the file into RAM.
//! * [`decode_video_poster`] — decode the clip's **first non-black frame** (a capped
//!   mean-luma walk: ≤ ~1 s of media / ≤ 30 frames, fallback = last sampled frame),
//!   fitted to the display via the MF video processor (spike-verified: the fitted
//!   `MF_MT_FRAME_SIZE` negotiation is honored; fallback = native size + our Lanczos).
//!
//! Failure is graceful and *diagnostic*: a container MF can't open reports a
//! different error than a missing codec (`MF_E_UNSUPPORTED_BYTESTREAM_TYPE` vs
//! `MF_E_TOPO_CODEC_NOT_FOUND`) — the caller shows the placeholder tile and the
//! panel/error copy can name the fix (the Store codec extensions), the same pattern
//! as WIC HEIC stills.
//!
//! Teardown: dropping a mid-stream reader blocks ~1 s on HEVC (Store MFT; spike D).
//! Posters stop mid-stream by design, so the reader is retired on a detached thread
//! (bounded — beyond a small cap we drop inline rather than grow threads), keeping
//! pool workers decoding instead of waiting on MFT shutdown.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFSourceReader, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromByteStream, MFCreateSourceReaderFromURL, MFMediaType_Video,
    MFVideoFormat_AV1, MFVideoFormat_H263, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_HEVC_ES, MFVideoFormat_MJPG, MFVideoFormat_MP43, MFVideoFormat_MP4V,
    MFVideoFormat_MPEG2, MFVideoFormat_RGB32, MFVideoFormat_VP80, MFVideoFormat_VP90,
    MFVideoFormat_WMV1, MFVideoFormat_WMV2, MFVideoFormat_WMV3, MFVideoFormat_WVC1,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_MT_VIDEO_ROTATION, MF_PD_DURATION, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::{VT_I8, VT_UI8};

use crate::mf_video::{ensure_mf, native_color, sample_to_rgba};
use crate::video::{poster_frame_bright_enough, POSTER_MAX_FRAMES, POSTER_MAX_MEDIA};
use crate::{common, DecodeError, DecodedImage, FitBox, PixelFormat};

// `VideoStreamInfo` is the platform-neutral probe result (`crate::video`); both this
// Media Foundation probe and the macOS AVFoundation one construct the same type.
pub use crate::video::VideoStreamInfo;

/// Open the container and report the video stream's facts. Read-only, no frame
/// decode, no RAM read of the file. ~15–25 ms; the panel caches the result.
pub fn probe_video_stream(path: &Path) -> Result<VideoStreamInfo, DecodeError> {
    probe_video_input(&crate::VideoInput::Path(path.to_path_buf()))
}

/// [`probe_video_stream`] over any [`VideoInput`](crate::VideoInput) — the archive
/// case probes the entry's in-RAM bytes (already fetched; no disk involved).
pub fn probe_video_input(input: &crate::VideoInput) -> Result<VideoStreamInfo, DecodeError> {
    ensure_mf();
    unsafe {
        let reader = open_video_reader(input)?;
        let info = stream_info(&reader);
        retire_reader(reader);
        info
    }
}

/// The **Details** probe (task #98): basic facts + the audio/subtitle track catalog.
/// Off-thread only — never on the poster path, which runs for every prefetched video
/// whether the Inspector opens or not.
///
/// The catalog comes from [`crate::mf_tracks`], whose module docs carry what MF was
/// measured to actually expose (audio: fully; subtitles: not at all; dispositions:
/// none). Audio lands `Complete` — including the `Complete` + zero that lets Details say
/// "Audio: No" about a silent clip — while subtitles stay
/// [`Unavailable`](crate::TrackCompleteness::Unavailable), because MF's silence about
/// them is not evidence of their absence.
pub fn probe_video_details(
    path: &Path,
    generation: u64,
) -> Result<crate::video::VideoDetailsProbe, DecodeError> {
    probe_video_details_input(&crate::VideoInput::Path(path.to_path_buf()), generation)
}

/// [`probe_video_details`] over any [`VideoInput`](crate::VideoInput) — an archive entry
/// probes its in-RAM bytes through the same reader, so archived videos get the same
/// catalog as filesystem ones.
///
/// One container open serves both reads: the track enumeration borrows the reader the
/// stream facts were just read from, rather than opening the file a second time.
pub fn probe_video_details_input(
    input: &crate::VideoInput,
    generation: u64,
) -> Result<crate::video::VideoDetailsProbe, DecodeError> {
    ensure_mf();
    unsafe {
        let reader = open_video_reader(input)?;
        let result = stream_info(&reader).map(|video| crate::video::VideoDetailsProbe {
            tracks: crate::mf_tracks::catalog_from_reader(&reader, generation),
            video,
        });
        retire_reader(reader);
        result
    }
}

/// Decode the clip's poster frame — the first non-black frame within the capped
/// walk — fitted to `fit`. `cancel` stops the walk between samples (the pool's
/// per-job flag). The frame is display-oriented (processor-rotated) and carries
/// the native color transform, identical to the future playback path.
pub fn decode_video_poster(
    path: &Path,
    fit: Option<FitBox>,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    decode_video_poster_input(&crate::VideoInput::Path(path.to_path_buf()), fit, cancel)
}

/// [`decode_video_poster`] over any [`VideoInput`](crate::VideoInput) — how an
/// archived video (in-RAM bytes, no path) gets its poster.
pub fn decode_video_poster_input(
    input: &crate::VideoInput,
    fit: Option<FitBox>,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    ensure_mf();
    unsafe {
        let reader = open_video_reader(input)?;
        let result = poster_inner(&reader, fit, cancel);
        // Mid-stream reader teardown blocks ~1 s on HEVC — retire off-thread so the
        // decode worker moves on to the next item immediately.
        retire_reader(reader);
        result
    }
}

/// Source reader with the playback-identical configuration: advanced video
/// processing (YUV→RGB + rotation), all streams deselected, video selected.
/// A path opens by URL; in-RAM bytes (an archive entry) through a fresh tagged
/// byte stream (`mf_stream`) — everything downstream is identical.
pub(crate) unsafe fn open_video_reader(
    input: &crate::VideoInput,
) -> Result<IMFSourceReader, DecodeError> {
    let inner = || -> windows::core::Result<IMFSourceReader> {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.expect("MFCreateAttributes succeeded");
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
        let reader: IMFSourceReader = match crate::mf_stream::ReaderSource::new(input)? {
            crate::mf_stream::ReaderSource::Url(url) => MFCreateSourceReaderFromURL(&url, &attrs)?,
            crate::mf_stream::ReaderSource::Stream(bs) => {
                MFCreateSourceReaderFromByteStream(&bs, &attrs)?
            }
        };
        // Deselect everything, then select only video: a selected-but-unread stream
        // queues samples indefinitely (MF documented behavior; audio is played by a
        // separate player in later phases, never read from this reader).
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, true)?;
        Ok(reader)
    };
    inner().map_err(|e| DecodeError::Corrupt(mf_open_msg(e)))
}

/// Diagnostic open/negotiate error text: name the fix when one exists (the same
/// Store-extension pattern as HEIC stills / Live Photo HEVC).
pub(crate) fn mf_open_msg(e: windows::core::Error) -> String {
    match e.code().0 as u32 {
        // MF_E_TOPO_CODEC_NOT_FOUND — container opened, codec missing.
        0xC00D_5212 => "no codec for this video (a Store codec extension may add it, \
                        e.g. HEVC or AV1 Video Extensions)"
            .to_string(),
        // MF_E_UNSUPPORTED_BYTESTREAM_TYPE — no container handler at all.
        0xC00D_36C4 => {
            "this video container is not supported by Windows Media Foundation".to_string()
        }
        _ => format!("Media Foundation: {e}"),
    }
}

pub(crate) unsafe fn stream_info(reader: &IMFSourceReader) -> Result<VideoStreamInfo, DecodeError> {
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let native = reader
        .GetNativeMediaType(video, 0)
        .map_err(|e| DecodeError::Corrupt(mf_open_msg(e)))?;
    let codec = native
        .GetGUID(&MF_MT_SUBTYPE)
        .map(|sub| codec_name(&sub))
        .unwrap_or("Video");
    let packed = native.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
    let (width, height) = ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32);
    let fps = native
        .GetUINT64(&MF_MT_FRAME_RATE)
        .map(|fr| {
            let (n, d) = ((fr >> 32) as u32, (fr & 0xFFFF_FFFF) as u32);
            if d == 0 {
                0.0
            } else {
                n as f64 / d as f64
            }
        })
        .unwrap_or(0.0);
    let rotation = native.GetUINT32(&MF_MT_VIDEO_ROTATION).unwrap_or(0) % 360;
    let duration = reader
        .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
        .ok()
        .and_then(|pv| propvariant_u64(&pv))
        .filter(|&hns| hns > 0)
        .map(|hns| Duration::from_nanos(hns * 100));
    let has_audio = reader
        .GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0)
        .is_ok();
    let color = native_color(reader, video);
    Ok(VideoStreamInfo {
        codec,
        width,
        height,
        rotation,
        fps,
        duration,
        has_audio,
        color,
    })
}

unsafe fn poster_inner(
    reader: &IMFSourceReader,
    fit: Option<FitBox>,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let info = stream_info(reader)?;
    let (disp_w, disp_h) = info.display_dims();

    // Ask the MF video processor for fitted output (spike-verified). If the fitted
    // negotiation is rejected, fall back to native size and downscale ourselves.
    let fitted = fit.map(|f| fit_dims(disp_w, disp_h, f));
    let negotiated = match fitted {
        Some(dims) => match negotiate_rgb32(reader, Some(dims)) {
            Ok(n) => n,
            Err(_) => {
                negotiate_rgb32(reader, None).map_err(|e| DecodeError::Corrupt(mf_open_msg(e)))?
            }
        },
        None => negotiate_rgb32(reader, None).map_err(|e| DecodeError::Corrupt(mf_open_msg(e)))?,
    };
    let (w, h, stride) = negotiated;
    if w == 0 || h == 0 {
        return Err(DecodeError::Corrupt("video has no frames".into()));
    }

    // The mean-luma walk: accept the first bright-enough frame; keep the last
    // sampled one as the fallback (a fully black lead longer than the cap, or a
    // genuinely dark clip, still gets *a* poster).
    let max_media_hns = POSTER_MAX_MEDIA.as_nanos() as i64 / 100;
    let mut last: Option<Vec<u8>> = None;
    for _ in 0..POSTER_MAX_FRAMES {
        if cancel.load(Ordering::Relaxed) {
            return Err(DecodeError::Corrupt("cancelled".into()));
        }
        let mut flags = 0u32;
        let mut ts = 0i64;
        let mut sample = None;
        reader
            .ReadSample(
                video,
                0,
                None,
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )
            .map_err(|e| DecodeError::Corrupt(mf_open_msg(e)))?;
        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
        let Some(sample) = sample else { continue };
        let rgba = sample_to_rgba(&sample, w, h, stride)
            .map_err(|e| DecodeError::Corrupt(format!("Media Foundation: {e}")))?;
        let bright = poster_frame_bright_enough(&rgba);
        last = Some(rgba);
        if bright || ts >= max_media_hns {
            break;
        }
    }
    let rgba = last.ok_or_else(|| DecodeError::Corrupt("video decoded no frames".into()))?;

    // If the processor already scaled, this fit is a no-op; the native-size
    // fallback path pays one Lanczos here (posters are off the hot path).
    let (rgba, fw, fh) = match fit {
        Some(f) => common::downscale_to_fit(rgba, w, h, f)?,
        None => (rgba, w, h),
    };
    Ok(DecodedImage {
        width: fw,
        height: fh,
        orig_width: disp_w,
        orig_height: disp_h,
        codec: info.codec,
        format: PixelFormat::Rgba8,
        pixels: rgba,
        is_preview: false,
        color: info.color,
        peak: 1.0,
        animated: None,
    })
}

pub(crate) unsafe fn negotiate_rgb32(
    reader: &IMFSourceReader,
    size: Option<(u32, u32)>,
) -> windows::core::Result<(u32, u32, i32)> {
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let out = MFCreateMediaType()?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
    if let Some((w, h)) = size {
        out.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
    }
    reader.SetCurrentMediaType(video, None, &out)?;
    let cur = reader.GetCurrentMediaType(video)?;
    let packed = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
    let (w, h) = ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32);
    let stride = cur
        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
        .map(|s| s as i32)
        .unwrap_or((w * 4) as i32);
    Ok((w, h, stride))
}

/// Aspect-preserving fit of `(w, h)` into `fit`, never upscaling, even dims (keeps
/// YUV-derived pipelines happy — same rule the phase-0 spike used).
pub(crate) fn fit_dims(w: u32, h: u32, fit: FitBox) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (fit.max_width.max(2), fit.max_height.max(2));
    }
    let scale = (fit.max_width as f64 / w as f64)
        .min(fit.max_height as f64 / h as f64)
        .min(1.0);
    let fw = ((w as f64 * scale).round() as u32).max(2) & !1;
    let fh = ((h as f64 * scale).round() as u32).max(2) & !1;
    (fw, fh)
}

fn codec_name(sub: &windows::core::GUID) -> &'static str {
    let named = [
        (MFVideoFormat_H264, "H.264"),
        (MFVideoFormat_HEVC, "HEVC"),
        (MFVideoFormat_HEVC_ES, "HEVC"),
        (MFVideoFormat_VP80, "VP8"),
        (MFVideoFormat_VP90, "VP9"),
        (MFVideoFormat_AV1, "AV1"),
        (MFVideoFormat_WMV1, "WMV"),
        (MFVideoFormat_WMV2, "WMV"),
        (MFVideoFormat_WMV3, "WMV"),
        (MFVideoFormat_WVC1, "VC-1"),
        (MFVideoFormat_MPEG2, "MPEG-2"),
        (MFVideoFormat_MP4V, "MPEG-4"),
        (MFVideoFormat_MP43, "MPEG-4"),
        (MFVideoFormat_MJPG, "MJPEG"),
        (MFVideoFormat_H263, "H.263"),
    ];
    for (g, n) in named {
        if *sub == g {
            return n;
        }
    }
    "Video"
}

/// A `VT_I8` PROPVARIANT holding a 100 ns media position — what
/// `IMFSourceReader::SetCurrentPosition` takes (the video seek, task #79).
pub(crate) fn propvariant_i8(value: i64) -> PROPVARIANT {
    use windows::Win32::System::Com::StructuredStorage::{
        PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
    };
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_I8,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { hVal: value },
            }),
        },
    }
}

fn propvariant_u64(pv: &PROPVARIANT) -> Option<u64> {
    unsafe {
        let inner = &pv.Anonymous.Anonymous;
        match inner.vt {
            VT_UI8 => Some(inner.Anonymous.uhVal),
            VT_I8 => u64::try_from(inner.Anonymous.hVal).ok(),
            _ => None,
        }
    }
}

/// Bound on readers being torn down concurrently on detached threads. Beyond it we
/// drop inline (backpressure) rather than grow threads without limit under a fast
/// blaze-through of a video-heavy folder.
const MAX_PENDING_RETIREMENTS: usize = 8;
static PENDING_RETIREMENTS: AtomicUsize = AtomicUsize::new(0);

/// Retire a source reader without blocking the calling decode worker: HEVC MFT
/// shutdown inside the drop blocks ~1 s (spike D). The thread is detached and
/// counted; at the cap we accept the inline block instead of unbounded threads.
///
/// Soundness of the `Send` wrapper: the reader is created on an MTA-initialized
/// worker (every caller runs [`ensure_mf`], which `CoInitializeEx(MULTITHREADED)`s
/// the thread), MF objects are free-threaded, and the retirement thread joins the
/// MTA before releasing — so cross-thread release is within the COM rules.
pub(crate) fn retire_reader(reader: IMFSourceReader) {
    struct SendReader {
        _reader: IMFSourceReader, // held only for its Drop (the MFT shutdown)
    }
    unsafe impl Send for SendReader {}

    let pending = PENDING_RETIREMENTS.fetch_add(1, Ordering::AcqRel);
    if pending >= MAX_PENDING_RETIREMENTS {
        PENDING_RETIREMENTS.fetch_sub(1, Ordering::AcqRel);
        drop(reader);
        return;
    }
    let carried = SendReader { _reader: reader };
    std::thread::spawn(move || {
        ensure_mf(); // join the MTA on this thread before the release
        drop(carried);
        PENDING_RETIREMENTS.fetch_sub(1, Ordering::AcqRel);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    #[test]
    fn probe_reports_the_fixtures_stream_facts() {
        let info = probe_video_stream(&fixture("black_then_color.mp4")).expect("probe");
        assert_eq!(info.codec, "H.264");
        assert_eq!((info.width, info.height), (64, 64));
        assert!(info.fps > 29.0 && info.fps < 31.0, "fps {}", info.fps);
        let dur = info.duration.expect("mp4 reports duration");
        assert!(dur > Duration::from_millis(800) && dur < Duration::from_millis(1500));
        assert!(!info.has_audio);
    }

    #[test]
    fn poster_walks_past_the_black_lead_in() {
        let img = decode_video_poster(
            &fixture("black_then_color.mp4"),
            Some(FitBox {
                max_width: 64,
                max_height: 64,
            }),
            &AtomicBool::new(false),
        )
        .expect("poster");
        assert!(img.is_well_formed());
        assert_eq!(img.codec, "H.264");
        // The fixture's first ~10 frames are pure black; the accepted poster must be
        // one of the bright frames after them.
        assert!(
            crate::video::poster_frame_bright_enough(&img.pixels),
            "poster must skip the black lead-in (mean luma {})",
            crate::video::mean_luma_rgba8(&img.pixels, 1),
        );
    }

    /// The archive seam: probing + poster-decoding the same fixture from in-RAM
    /// bytes (no path) must agree with the path versions — configuration-identical
    /// readers by construction.
    #[test]
    fn probe_and_poster_from_bytes_match_the_path_versions() {
        let data =
            std::sync::Arc::new(std::fs::read(fixture("black_then_color.mp4")).expect("bytes"));
        let input = crate::VideoInput::Bytes {
            data,
            name: "folder/black_then_color.mp4".into(),
        };
        let info = probe_video_input(&input).expect("probe from bytes");
        assert_eq!(info.codec, "H.264");
        assert_eq!((info.width, info.height), (64, 64));
        let img = decode_video_poster_input(
            &input,
            Some(FitBox {
                max_width: 64,
                max_height: 64,
            }),
            &AtomicBool::new(false),
        )
        .expect("poster from bytes");
        assert!(img.is_well_formed());
        assert!(
            crate::video::poster_frame_bright_enough(&img.pixels),
            "bytes poster must skip the black lead-in too"
        );
    }

    #[test]
    fn a_missing_file_and_garbage_bytes_fail_cleanly() {
        let missing = probe_video_stream(std::path::Path::new(r"C:\nope\missing.mp4"));
        assert!(missing.is_err());
        let dir = std::env::temp_dir().join("pb_mf_poster_test");
        let _ = std::fs::create_dir_all(&dir);
        let junk = dir.join("junk.mp4");
        std::fs::write(&junk, b"definitely not an mp4").unwrap();
        assert!(probe_video_stream(&junk).is_err());
        assert!(decode_video_poster(&junk, None, &AtomicBool::new(false)).is_err());
        let _ = std::fs::remove_file(&junk);
    }

    /// Opt-in corpus check (codec extensions vary by machine): set
    /// `PB_VIDEO_POSTER_CLIP` to a real clip to run the full poster + probe path
    /// against it. Prints the outcome for eyeballing.
    #[test]
    fn opt_in_real_clip_poster() {
        let Ok(clip) = std::env::var("PB_VIDEO_POSTER_CLIP") else {
            eprintln!("PB_VIDEO_POSTER_CLIP not set — skipping");
            return;
        };
        let path = std::path::PathBuf::from(clip);
        let t0 = std::time::Instant::now();
        let info = probe_video_stream(&path).expect("probe");
        let t_probe = t0.elapsed();
        let t0 = std::time::Instant::now();
        let img = decode_video_poster(
            &path,
            Some(FitBox {
                max_width: 3840,
                max_height: 2160,
            }),
            &AtomicBool::new(false),
        )
        .expect("poster");
        eprintln!(
            "clip: {} {}x{} {:.2}fps dur={:?} audio={} | poster {}x{} luma={:.3} probe={:?} poster={:?}",
            info.codec,
            info.width,
            info.height,
            info.fps,
            info.duration,
            info.has_audio,
            img.width,
            img.height,
            crate::video::mean_luma_rgba8(&img.pixels, 8),
            t_probe,
            t0.elapsed(),
        );
        assert!(img.is_well_formed());
        assert_eq!((img.orig_width, img.orig_height), info.display_dims());
    }

    #[test]
    fn a_preset_cancel_stops_the_walk() {
        let err = decode_video_poster(
            &fixture("black_then_color.mp4"),
            None,
            &AtomicBool::new(true),
        )
        .expect_err("cancel must abort");
        assert!(err.to_string().contains("cancelled"));
    }

    /// Diagnostic (opt-in): can the low-level `IMFSourceReader` decode a clip's
    /// AUDIO stream to PCM? The WinRT `MediaPlayer` refuses to *play* old
    /// MJPEG-in-AVI camera clips (opens, but the clock never advances → no sound),
    /// while the Source Reader plays their *video* fine. This probes whether the
    /// same permissive API can decode their audio — the deciding fact for an
    /// MF-native audio fix (Source Reader → PCM → WASAPI) vs pulling FFmpeg onto
    /// the Windows build.
    /// `PB_AUDIO_PROBE_CLIP=<path> cargo test -p pb-decode opt_in_source_reader_audio -- --nocapture`
    #[test]
    fn opt_in_source_reader_audio() {
        use windows::core::HSTRING;
        use windows::Win32::Media::MediaFoundation::{
            IMFSourceReader, MFAudioFormat_PCM, MFCreateMediaType, MFCreateSourceReaderFromURL,
            MFMediaType_Audio, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM,
            MF_SOURCE_READER_ALL_STREAMS,
        };

        let Ok(clip) = std::env::var("PB_AUDIO_PROBE_CLIP") else {
            eprintln!("PB_AUDIO_PROBE_CLIP not set — skipping");
            return;
        };
        ensure_mf();
        unsafe {
            let reader: IMFSourceReader =
                MFCreateSourceReaderFromURL(&HSTRING::from(clip.as_str()), None)
                    .expect("open source reader");
            let audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
            reader
                .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
                .unwrap();
            reader.SetStreamSelection(audio, true).unwrap();

            // Ask for uncompressed PCM out (MF inserts the decoder + resampler).
            let out = MFCreateMediaType().unwrap();
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).unwrap();
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).unwrap();
            match reader.SetCurrentMediaType(audio, None, &out) {
                Ok(()) => eprintln!("PCM output negotiated ✓"),
                Err(e) => {
                    eprintln!(
                        "PCM negotiation FAILED: {e} — Source Reader can't decode this audio"
                    );
                    return;
                }
            }

            let mut total = 0usize;
            let mut reads = 0;
            while reads < 40 {
                let mut flags = 0u32;
                let mut ts = 0i64;
                let mut sample = None;
                reader
                    .ReadSample(
                        audio,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut ts),
                        Some(&mut sample),
                    )
                    .expect("read audio sample");
                if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                    eprintln!("EOS after {reads} reads");
                    break;
                }
                if let Some(s) = sample {
                    let buf = s.ConvertToContiguousBuffer().unwrap();
                    let mut ptr = std::ptr::null_mut();
                    let mut len = 0u32;
                    buf.Lock(&mut ptr, None, Some(&mut len)).unwrap();
                    total += len as usize;
                    buf.Unlock().unwrap();
                }
                reads += 1;
            }
            eprintln!(
                "Source Reader decoded {total} PCM bytes over {reads} reads — audio decode {}",
                if total > 0 {
                    "WORKS ✓"
                } else {
                    "produced NOTHING ✗"
                }
            );
        }
    }
}
