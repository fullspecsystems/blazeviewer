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
//! * [`decode_video_poster`] — decode a genuinely-visible poster frame (a **scored
//!   best-so-far walk**: a head pass over the first [`POSTER_HEAD_FRAMES`] frames,
//!   then, for a clip whose intro is black/logo/fade, a **deep seek past the intro**
//!   at [`POSTER_SEEK_OFFSETS`] — recreating the reader per offset, since a warm HEVC
//!   reposition blocks ~1 s while a fresh open is ~86 ms even over SMB), fitted to the
//!   display via the MF video processor. Fallback is the best-scoring frame seen, never
//!   the last. Mirrors the FFmpeg backend (`ffmpeg::poster`); both read one policy from
//!   [`crate::video`].
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
use std::time::{Duration, Instant};

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
use crate::video::{
    poster_deep_cap, POSTER_BURST_FRAMES, POSTER_DEADLINE, POSTER_DEEP_MIN, POSTER_HEAD_FRAMES,
    POSTER_SEEK_OFFSETS,
};
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
    poster_selected_input(input, fit, false, cancel).map(|(img, _)| img)
}

/// The walk plus its choice — shared plumbing for the legacy poster entry
/// points and the task-#114 selection. `native_walk` (phase-2 variant A)
/// negotiates within the [`POSTER_NATIVE_CAP_EDGE`] box instead of the display
/// fit and returns the winner at that size (the caller cuts the artifacts).
fn poster_selected_input(
    input: &crate::VideoInput,
    fit: Option<FitBox>,
    native_walk: bool,
    cancel: &AtomicBool,
) -> Result<(DecodedImage, crate::PosterChoice), DecodeError> {
    ensure_mf();
    unsafe {
        let reader = open_video_reader(input)?;
        let result = poster_inner(&reader, input, fit, native_walk, cancel);
        // Mid-stream reader teardown blocks ~1 s on HEVC — retire off-thread so the
        // decode worker moves on to the next item immediately.
        retire_reader(reader);
        result
    }
}

/// The ONE scored walk per movie (task #114): choose the poster frame, remember
/// WHERE it lives (`PosterChoice`, absolute locator), and cut every consumer's
/// artifact from the same winner on this worker thread — the display-fit poster
/// and the thumbnail tile. `fit` is the display fit (or the thumb fit for a
/// thumb-only selection); the thumb is always cut so poster == thumb by
/// construction.
pub fn decode_video_poster_select(
    input: &crate::VideoInput,
    fit: Option<FitBox>,
    thumb_fit: FitBox,
    native_walk: bool,
    want_native: bool,
    cancel: &AtomicBool,
) -> Result<crate::PosterSelection, DecodeError> {
    let (img, choice) = poster_selected_input(input, fit, native_walk, cancel)?;
    cut_selection(img, choice, fit, thumb_fit, native_walk, want_native)
}

/// Cut every consumer's artifact from one winning frame (`img` is at walk size:
/// the fitted poster in the fitted variant, native-capped in the native variant
/// and in a replay). Resize failures are REAL failures (phase-1 review f9) —
/// a success-with-no-tile reads as an eviction later and re-walks. Transient
/// clones (thumb cut, native-mode fit cut) are what the phase-2 native permit
/// bounds.
fn cut_selection(
    img: DecodedImage,
    choice: crate::PosterChoice,
    fit: Option<FitBox>,
    thumb_fit: FitBox,
    native_walk: bool,
    want_native: bool,
) -> Result<crate::PosterSelection, DecodeError> {
    let cut = |src: &DecodedImage, to: FitBox| -> Result<DecodedImage, DecodeError> {
        let (px, w, h) = common::downscale_to_fit(src.pixels.clone(), src.width, src.height, to)?;
        Ok(DecodedImage {
            width: w,
            height: h,
            orig_width: src.orig_width,
            orig_height: src.orig_height,
            codec: src.codec,
            format: PixelFormat::Rgba8,
            pixels: px,
            is_preview: false,
            color: src.color,
            peak: src.peak,
            animated: None,
        })
    };
    let thumb_img = Some(cut(&img, thumb_fit)?);
    if !native_walk {
        // Fitted variant: the winner IS the display Fit; no native retained.
        return Ok(crate::PosterSelection {
            choice,
            fit_img: Some(img),
            thumb_img,
            native: None,
        });
    }
    // Native variant / replay: the winner is native-capped. Cut the display Fit
    // from it; keep the native only when the consumer union wants it (a
    // thumb-only selection drops it — plan §3, demand-gated admission).
    let (fit_img, native) = match fit {
        Some(f) => {
            let fitted = Some(cut(&img, f)?);
            (fitted, want_native.then_some(img))
        }
        // Fill/Original mode: the native IS the display artifact and installs
        // as the display's Original rep directly — a second copy in `native`
        // would be redundant bytes (phases-2/3 review f1b).
        None => (Some(img), None),
    };
    Ok(crate::PosterSelection {
        choice,
        fit_img,
        thumb_img,
        native,
    })
}

/// Decode-forward REPLAY of an already-chosen poster frame (task #114 phase 3):
/// fresh reader, absolute seek to `origin + relative` (which lands on the
/// preceding keyframe), then decode forward until the target timestamp — the
/// playback seek algorithm, reproducing the SAME frame by timestamp match,
/// never "first frame after seek". One GOP of decode at most in practice;
/// deadline-capped for hostile indexes. The frame is negotiated native-capped
/// so it can serve every artifact, including the Original install.
pub fn decode_video_poster_replay(
    input: &crate::VideoInput,
    origin_hns: i64,
    relative_hns: i64,
    fit: Option<FitBox>,
    thumb_fit: FitBox,
    want_native: bool,
    cancel: &AtomicBool,
) -> Result<crate::PosterSelection, DecodeError> {
    ensure_mf();
    let deadline = Instant::now() + REPLAY_DEADLINE;
    let target = origin_hns.saturating_add(relative_hns);
    unsafe {
        let reader = open_video_reader(input)?;
        let result = (|| {
            let info = stream_info(&reader)?;
            let (disp_w, disp_h) = info.display_dims();
            let cap = FitBox {
                max_width: crate::video::POSTER_NATIVE_CAP_EDGE,
                max_height: crate::video::POSTER_NATIVE_CAP_EDGE,
            };
            let dims = fit_dims(disp_w, disp_h, cap);
            let (w, h, stride) = negotiate_rgb32(&reader, Some(dims))
                .or_else(|_| negotiate_rgb32(&reader, None))
                .map_err(|e| DecodeError::Corrupt(mf_open_msg(e)))?;
            let pos = propvariant_i8(target.max(0));
            reader
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &pos)
                .map_err(|e| DecodeError::Corrupt(mf_open_msg(e)))?;
            let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let mut last: Option<(Vec<u8>, i64)> = None;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(DecodeError::Corrupt("cancelled".into()));
                }
                if Instant::now() >= deadline {
                    break; // hostile index: settle for the closest frame seen
                }
                let mut flags = 0u32;
                let mut ts_hns = 0i64;
                let mut sample = None;
                reader
                    .ReadSample(
                        video,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut ts_hns),
                        Some(&mut sample),
                    )
                    .map_err(map_read_err)?;
                if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                    break;
                }
                let Some(sample) = sample else { continue };
                let rgba = sample_to_rgba(&sample, w, h, stride)
                    .map_err(|e| DecodeError::Corrupt(format!("Media Foundation: {e}")))?;
                let reached = ts_hns >= target;
                last = Some((rgba, ts_hns));
                if reached {
                    break;
                }
            }
            // Replay IDENTITY (phases-2/3 review f2): only a frame that actually
            // reached the stored timestamp — within a small tolerance for
            // container rounding — may claim the locator. Deadline/EOF short of
            // the target, or a sparse stream overshooting far past it, is an
            // ERROR: the caller falls back to a fresh scored walk rather than
            // silently attaching the choice to different pixels.
            let (rgba, ts) =
                last.ok_or_else(|| DecodeError::Corrupt("replay decoded no frames".into()))?;
            if ts < target || ts.saturating_sub(target) > REPLAY_TOLERANCE_HNS {
                return Err(DecodeError::Corrupt(
                    "replay missed the chosen frame".into(),
                ));
            }
            let img = DecodedImage {
                width: w,
                height: h,
                orig_width: disp_w,
                orig_height: disp_h,
                codec: info.codec,
                format: PixelFormat::Rgba8,
                pixels: rgba,
                is_preview: false,
                color: info.color,
                peak: 1.0,
                animated: None,
            };
            let choice = crate::PosterChoice {
                origin_hns,
                relative_hns,
                native_w: disp_w,
                native_h: disp_h,
                content_hdr: false,
            };
            cut_selection(img, choice, fit, thumb_fit, true, want_native)
        })();
        retire_reader(reader);
        result
    }
}

/// The marker message for `MF_E_INVALID_POSITION` (0xC00D36E5) — MF refusing a
/// positioned read on a raw stream (`BDMV\STREAM\*.m2ts`). Classified at the
/// COM layer, where the HRESULT still exists (task #114 phase 4): the deep walk
/// degrades to its accumulated head best on exactly this code, and NO other
/// (masking real corruption behind a fallback poster is worse than no poster).
const INVALID_POSITION_MSG: &str = "positioned reads not permitted (raw stream)";

/// Map an MF read/seek error, preserving the invalid-position class.
fn map_read_err(e: windows::core::Error) -> DecodeError {
    if e.code().0 as u32 == 0xC00D_36E5 {
        DecodeError::Corrupt(INVALID_POSITION_MSG.into())
    } else {
        DecodeError::Corrupt(mf_open_msg(e))
    }
}

/// Whether a walk error is the typed invalid-position class.
fn is_invalid_position(e: &DecodeError) -> bool {
    matches!(e, DecodeError::Corrupt(m) if m == INVALID_POSITION_MSG)
}

/// Replay watchdog: a healthy replay decodes one GOP (well under a second);
/// this only bounds hostile/corrupt indexes.
const REPLAY_DEADLINE: Duration = Duration::from_secs(10);

/// How far past the stored timestamp a replayed frame may land and still count
/// as THE chosen frame (container timestamp rounding; half a second is far
/// beyond any real rounding and far below any visually different scene cut
/// being silently substituted).
const REPLAY_TOLERANCE_HNS: i64 = 5_000_000;

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
        // MF doesn't surface the DoVi record; the Windows FFmpeg build is
        // demuxers-only for tracks, so the summary stays unprobed there.
        dovi: None,
    })
}

/// The best-scoring poster candidate seen so far, with its dimensions (a deep-seek
/// reader could in principle negotiate a different size, so the winner carries its
/// own dims for the final fit). Only the winning frame is kept resident.
struct Best {
    score: f32,
    frame: Option<(Vec<u8>, u32, u32)>,
    /// The retained frame's RAW MF timestamp (100 ns units, absolute — the
    /// stream's origin has NOT been subtracted). Task #114: this is what makes
    /// the choice replayable.
    ts_hns: i64,
}

impl Best {
    fn new() -> Self {
        Self {
            score: f32::NEG_INFINITY,
            frame: None,
            ts_hns: 0,
        }
    }

    /// Offer a scored frame for the *fallback* ranking; keep it if it beats the
    /// current best (first max wins — deterministic, so path and in-RAM posters
    /// stay bit-identical). Used only until a genuinely-good frame is found.
    fn consider(&mut self, score: f32, rgba: Vec<u8>, w: u32, h: u32, ts_hns: i64) {
        if self.frame.is_none() || score > self.score {
            self.score = score;
            self.frame = Some((rgba, w, h));
            self.ts_hns = ts_hns;
        }
    }

    /// Take this frame as the winner outright — the walk found a genuinely-good
    /// poster and stops here, so it is the result regardless of what earlier
    /// frames out-*ranked* it (ranking only decides the all-bad fallback).
    fn win(&mut self, rgba: Vec<u8>, w: u32, h: u32, ts_hns: i64) {
        self.score = f32::INFINITY;
        self.frame = Some((rgba, w, h));
        self.ts_hns = ts_hns;
    }
}

unsafe fn poster_inner(
    reader: &IMFSourceReader,
    input: &crate::VideoInput,
    fit: Option<FitBox>,
    native_walk: bool,
    cancel: &AtomicBool,
) -> Result<(DecodedImage, crate::PosterChoice), DecodeError> {
    let deadline = Instant::now() + POSTER_DEADLINE;
    let info = stream_info(reader)?;
    let (disp_w, disp_h) = info.display_dims();

    // Ask the MF video processor for fitted output (spike-verified). If the fitted
    // negotiation is rejected, fall back to native size and downscale ourselves.
    // The native-variant walk (phase 2, task #114) negotiates within the
    // POSTER_NATIVE_CAP_EDGE box instead: the winner comes out ready to BE the
    // parked Original, and the caller cuts the display/thumb artifacts from it.
    let fitted = if native_walk {
        Some(fit_dims(
            disp_w,
            disp_h,
            FitBox {
                max_width: crate::video::POSTER_NATIVE_CAP_EDGE,
                max_height: crate::video::POSTER_NATIVE_CAP_EDGE,
            },
        ))
    } else {
        fit.map(|f| fit_dims(disp_w, disp_h, f))
    };
    let (w, h, stride) = match fitted {
        Some(dims) => match negotiate_rgb32(reader, Some(dims)) {
            Ok(n) => n,
            Err(_) => {
                negotiate_rgb32(reader, None).map_err(|e| DecodeError::Corrupt(mf_open_msg(e)))?
            }
        },
        None => negotiate_rgb32(reader, None).map_err(|e| DecodeError::Corrupt(mf_open_msg(e)))?,
    };
    if w == 0 || h == 0 {
        return Err(DecodeError::Corrupt("video has no frames".into()));
    }

    let mut best = Best::new();
    // The stream's first-sample timestamp — the ABSOLUTE origin every seek and
    // the stored choice are anchored to (MPEG-TS files start nonzero; a bare
    // relative offset seeks the wrong place there — task #114 / playback's
    // `origin + relative` lesson).
    let mut origin: Option<i64> = None;
    // Phase 1 — the cheap head walk from the start. A clip that opens on content
    // settles here; a dark/logo/fade opening leaves `best` weak and falls through.
    let good = scan(
        reader,
        (w, h, stride),
        POSTER_HEAD_FRAMES,
        &mut best,
        &mut origin,
        cancel,
        deadline,
    )?;
    // Phase 2 — seek past the intro (feature-film case), shallow → deep, stopping at
    // the first good frame so the poster is as early as the intro allows.
    if !good {
        deep_scan(
            input,
            (w, h),
            info.duration,
            &mut best,
            origin.unwrap_or(0),
            cancel,
            deadline,
        )?;
    }

    let best_ts = best.ts_hns;
    let (rgba, bw, bh) = best
        .frame
        .ok_or_else(|| DecodeError::Corrupt("video decoded no frames".into()))?;

    // If the processor already scaled, this fit is a no-op; the native-size
    // fallback path pays one Lanczos here (posters are off the hot path). The
    // native walk returns the winner AT WALK SIZE — the caller cuts artifacts.
    let (rgba, fw, fh) = if native_walk {
        (rgba, bw, bh)
    } else {
        match fit {
            Some(f) => common::downscale_to_fit(rgba, bw, bh, f)?,
            None => (rgba, bw, bh),
        }
    };
    let origin = origin.unwrap_or(0);
    let choice = crate::PosterChoice {
        origin_hns: origin,
        relative_hns: (best_ts - origin).max(0),
        native_w: disp_w,
        native_h: disp_h,
        // MF posters are RGB32: PQ/HLG content is SDR-clamped by the processor
        // by design (mf_video.rs) — the pixels here are never HDR scene values.
        content_hdr: false,
    };
    Ok((
        DecodedImage {
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
        },
        choice,
    ))
}

/// Decode up to `limit` frames from `reader`'s current position, scoring each into
/// `best`. `Ok(true)` as soon as a clearly-good frame is found (caller stops),
/// `Ok(false)` when the limit / EOF / the overall deadline is reached first. The
/// deadline is a best-so-far fallback, not an error (a poster is a background
/// nicety); `cancel` is (the pool retiring the job).
unsafe fn scan(
    reader: &IMFSourceReader,
    size: (u32, u32, i32),
    limit: usize,
    best: &mut Best,
    origin: &mut Option<i64>,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<bool, DecodeError> {
    let (w, h, stride) = size;
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    for _ in 0..limit {
        if cancel.load(Ordering::Relaxed) {
            return Err(DecodeError::Corrupt("cancelled".into()));
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        let mut flags = 0u32;
        let mut ts_hns = 0i64;
        let mut sample = None;
        reader
            .ReadSample(
                video,
                0,
                None,
                Some(&mut flags),
                Some(&mut ts_hns),
                Some(&mut sample),
            )
            .map_err(map_read_err)?;
        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
        let Some(sample) = sample else { continue };
        // The head scan's first sample defines the stream origin (deep bursts
        // arrive with it already set — get_or_insert keeps the first).
        origin.get_or_insert(ts_hns);
        let rgba = sample_to_rgba(&sample, w, h, stride)
            .map_err(|e| DecodeError::Corrupt(format!("Media Foundation: {e}")))?;
        // Judge at the fixed judge size (task #114): resolution-independent, so
        // the pick is the same whether this walk serves a thumb or a 7680-wide
        // display. Stop on the first genuinely-good frame — bright AND textured,
        // so a white/vignette title card never ends the walk.
        let (good, score) = crate::video::poster_judge(&rgba, w, h);
        if good {
            best.win(rgba, w, h, ts_hns);
            return Ok(true);
        }
        best.consider(score, rgba, w, h, ts_hns);
    }
    Ok(false)
}

/// Phase 2 of the walk: seek past the intro (feature-film case), shallow → deep,
/// stopping at the first good frame. Each offset **recreates the reader positioned
/// there** — a warm HEVC reposition blocks ~1 s (spike), a fresh open is ~86 ms even
/// over SMB — and retires it off-thread. Returns whether a clearly-good frame landed.
unsafe fn deep_scan(
    input: &crate::VideoInput,
    dims: (u32, u32),
    duration: Option<Duration>,
    best: &mut Best,
    origin_hns: i64,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<bool, DecodeError> {
    let Some(dur) = duration else {
        return Ok(false);
    };
    if dur < POSTER_DEEP_MIN {
        return Ok(false);
    }
    let cap = poster_deep_cap(dur);
    let mut last = Duration::ZERO;
    for off in POSTER_SEEK_OFFSETS {
        let target = off.min(cap);
        // Skip offsets the head walk already covered (~1 s) and duplicates (a short
        // clip collapses the deeper offsets onto the cap).
        if target <= Duration::from_secs(1) || target <= last {
            continue;
        }
        last = target;
        if cancel.load(Ordering::Relaxed) {
            return Err(DecodeError::Corrupt("cancelled".into()));
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        // A seek that won't open (a bad offset) moves to the next offset —
        // EXCEPT the typed invalid-position refusal (a raw BDMV stream, task
        // #114 phase 4): no deeper offset can ever succeed there, so stop and
        // let the accumulated head best serve as the poster.
        let (reader, w, h, stride) = match reopen_at_rgb32(input, dims, target, origin_hns) {
            Ok(v) => v,
            Err(e) if is_invalid_position(&e) => return Ok(false),
            Err(_) => continue,
        };
        let mut deep_origin = Some(origin_hns);
        let r = scan(
            &reader,
            (w, h, stride),
            POSTER_BURST_FRAMES,
            best,
            &mut deep_origin,
            cancel,
            deadline,
        );
        retire_reader(reader);
        match r {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            // The post-seek ReadSample refusing positioned reads: same class,
            // same degrade — the head best survives instead of being discarded
            // (this `?` used to throw the whole walk's work away).
            Err(e) if is_invalid_position(&e) => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

/// Open a fresh reader, negotiate the same fitted RGB32 output, and position it at
/// `target` — the poster deep-seek's "recreate, don't reposition-warm" step. Mirrors
/// the playback producer's `reopen_at` (RGB32 branch); the fresh reader's seek is the
/// cheap ~86 ms open, not the ~1 s warm HEVC reposition.
unsafe fn reopen_at_rgb32(
    input: &crate::VideoInput,
    dims: (u32, u32),
    target: Duration,
    origin_hns: i64,
) -> Result<(IMFSourceReader, u32, u32, i32), DecodeError> {
    let reader = open_video_reader(input)?;
    let (w, h, stride) = negotiate_rgb32(&reader, Some(dims))
        .or_else(|_| negotiate_rgb32(&reader, None))
        .map_err(|e| DecodeError::Corrupt(mf_open_msg(e)))?;
    // ABSOLUTE seek: origin + offset, exactly as playback seeks (task #114 —
    // MPEG-TS streams start nonzero; a bare relative offset lands short there).
    let hns = origin_hns.saturating_add((target.as_nanos() / 100) as i64);
    let pos = propvariant_i8(hns.max(0));
    reader
        .SetCurrentPosition(&windows::core::GUID::zeroed(), &pos)
        .map_err(map_read_err)?;
    Ok((reader, w, h, stride))
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

    /// Phase 2 (deep seek). A 16 s clip that is black for its first 7 s — well past
    /// the head walk (`POSTER_HEAD_FRAMES` ≈ 1 s at 30 fps) — with high-contrast
    /// content after. The head walk finds only black, so a poster that isn't black
    /// PROVES the deep seek ran (offset 8 s, under `poster_deep_cap(16 s)` = 8 s).
    #[test]
    fn poster_deep_seeks_past_a_long_black_intro() {
        let img = decode_video_poster(
            &fixture("deep_seek_black_lead.mp4"),
            Some(FitBox {
                max_width: 64,
                max_height: 64,
            }),
            &AtomicBool::new(false),
        )
        .expect("poster");
        assert!(img.is_well_formed());
        // Only reachable by seeking past the 7 s black intro (the head walk sees
        // black-only and could never produce a bright frame).
        assert!(
            crate::video::poster_frame_bright_enough(&img.pixels),
            "deep seek must land on the content past the black intro (mean luma {})",
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
        let (mean, std) = crate::video::luma_stats_rgba8(&img.pixels, 8);
        eprintln!(
            "clip: {} {}x{} {:.2}fps dur={:?} audio={} | poster {}x{} mean={:.3} std={:.3} score={:.3} detail={:.3} probe={:?} poster={:?}",
            info.codec,
            info.width,
            info.height,
            info.fps,
            info.duration,
            info.has_audio,
            img.width,
            img.height,
            mean,
            std,
            crate::video::poster_frame_score(&img.pixels, img.width),
            crate::video::luma_detail_rgba8(&img.pixels, img.width),
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

    #[test]
    fn invalid_position_is_classified_and_nothing_else_is() {
        let e = windows::core::Error::from_hresult(windows::core::HRESULT(0xC00D_36E5u32 as i32));
        assert!(is_invalid_position(&map_read_err(e)));
        let other =
            windows::core::Error::from_hresult(windows::core::HRESULT(0xC00D_36B4u32 as i32));
        assert!(!is_invalid_position(&map_read_err(other)));
        assert!(!is_invalid_position(&DecodeError::Corrupt(
            "positioned reads refused".into()
        )));
    }

    /// Focused one-file probe (owner bug reports): walk at the thumb fit vs the
    /// display fit (do the picks agree?), then replay the thumb walk's choice
    /// at display fit (identity + cost — the re-visit path).
    ///   PB_PROBE_FILE='\\beenas\media\Movies\<file>.mkv' cargo test -p \
    ///     pb-decode --release -- --ignored probe_one_file --nocapture
    #[test]
    #[ignore]
    fn probe_one_file() {
        let Some(f) = std::env::var_os("PB_PROBE_FILE") else {
            eprintln!("set PB_PROBE_FILE");
            return;
        };
        let input = crate::VideoInput::Path(f.into());
        let cancel = AtomicBool::new(false);
        let thumb = FitBox {
            max_width: 512,
            max_height: 512,
        };
        let disp = Some(FitBox {
            max_width: 2560,
            max_height: 1440,
        });
        let t0 = Instant::now();
        let a = decode_video_poster_select(&input, Some(thumb), thumb, false, false, &cancel)
            .expect("thumb-fit walk");
        eprintln!(
            "thumb-fit walk:   ts={:.2}s  ({} ms)",
            a.choice.relative_hns as f64 / 1e7,
            t0.elapsed().as_millis()
        );
        let t0 = Instant::now();
        let b = decode_video_poster_select(&input, disp, thumb, false, false, &cancel)
            .expect("display-fit walk");
        eprintln!(
            "display-fit walk: ts={:.2}s  ({} ms){}",
            b.choice.relative_hns as f64 / 1e7,
            t0.elapsed().as_millis(),
            if a.choice.relative_hns != b.choice.relative_hns {
                "  << PICK DIVERGES"
            } else {
                ""
            }
        );
        let t0 = Instant::now();
        match decode_video_poster_replay(
            &input,
            a.choice.origin_hns,
            a.choice.relative_hns,
            disp,
            thumb,
            true,
            &cancel,
        ) {
            Ok(r) => eprintln!(
                "replay:           ts={:.2}s  ({} ms) native={}",
                r.choice.relative_hns as f64 / 1e7,
                t0.elapsed().as_millis(),
                r.native.is_some()
            ),
            Err(e) => eprintln!("replay ERR: {e}  ({} ms)", t0.elapsed().as_millis()),
        }
    }

    /// The phase-2 walk-variant A/B (task #114 plan §2, "measure don't guess"):
    /// run BOTH variants over real clips, print per-file walk latency + the
    /// chosen timestamp (the shared judge should make the pick identical).
    ///
    ///   PB_POSTER_AB_DIR='\\beenas\media\Movies' cargo test -p pb-decode \
    ///     --release -- --ignored ab_poster_walk --nocapture
    #[test]
    #[ignore]
    fn ab_poster_walk() {
        let Some(dir) = std::env::var_os("PB_POSTER_AB_DIR") else {
            eprintln!("set PB_POSTER_AB_DIR to the corpus directory");
            return;
        };
        let n: usize = std::env::var("PB_POSTER_AB_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);
        let fit = Some(FitBox {
            max_width: 2560,
            max_height: 1440,
        });
        let thumb = FitBox {
            max_width: 512,
            max_height: 512,
        };
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .expect("corpus dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("mkv" | "mp4" | "m4v" | "avi" | "webm")
                )
            })
            .collect();
        files.sort();
        files.truncate(n);
        let cancel = AtomicBool::new(false);
        let (mut sum_fitted, mut sum_native) = (0u128, 0u128);
        let (mut compared, mut mismatches, mut failures) = (0usize, 0usize, 0usize);
        for (fi, p) in files.iter().enumerate() {
            let input = crate::VideoInput::Path(p.clone());
            let mut ts = [None::<i64>; 2];
            let mut ok = [false; 2];
            let mut row = p.file_name().unwrap().to_string_lossy().to_string();
            // Counterbalanced order (review p2/3 f6): alternate which variant
            // gets the cold cache position, per file.
            let order: [(&str, bool); 2] = if fi % 2 == 0 {
                [("fitted", false), ("native", true)]
            } else {
                [("native", true), ("fitted", false)]
            };
            for (label, native) in order {
                let t0 = Instant::now();
                let r = decode_video_poster_select(&input, fit, thumb, native, native, &cancel);
                let ms = t0.elapsed().as_millis();
                let slot = usize::from(native);
                match r {
                    Ok(sel) => {
                        ok[slot] = true;
                        ts[slot] = Some(sel.choice.relative_hns);
                        // Failures never pollute the totals.
                        if native {
                            sum_native += ms;
                        } else {
                            sum_fitted += ms;
                        }
                        row.push_str(&format!(
                            " | {label}: {ms} ms ts={:.1}s",
                            sel.choice.relative_hns as f64 / 10_000_000.0,
                        ));
                    }
                    Err(e) => {
                        failures += 1;
                        row.push_str(&format!(" | {label}: ERR {e} ({ms} ms)"));
                    }
                }
            }
            // A pick comparison is only meaningful when BOTH variants succeeded.
            if ok[0] && ok[1] {
                compared += 1;
                if ts[0] != ts[1] {
                    mismatches += 1;
                    row.push_str("  << PICK MISMATCH");
                }
            }
            eprintln!("{row}");
        }
        eprintln!(
            "TOTAL fitted={sum_fitted} ms native={sum_native} ms | {compared} compared, {mismatches} pick mismatches, {failures} failed runs"
        );
    }
}
