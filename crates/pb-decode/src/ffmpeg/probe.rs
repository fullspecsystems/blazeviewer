//! Stream selection + container facts for an opened FFmpeg input (plan §5).
//!
//! One read of the demuxer answers everything the producer, poster, and
//! inspector need: which stream to decode (the best **non-attached-picture**
//! video stream — MP4 cover art is a "video" stream with the ATTACHED_PIC
//! disposition and must never be chosen), its geometry, the container rotation
//! (display matrix), sample aspect ratio, frame rate, duration, and whether an
//! audio track exists. Kept in one place so the producer and the poster can
//! never drift on selection/timing/rotation policy.

use std::time::Duration;

use ff::media::Type;
use ffmpeg_next as ff;

/// The facts one open of a container yields, in FFmpeg-native units where the
/// producer needs them (time base, stream start time) and display units where
/// the UI does (rotation, fps, duration).
#[derive(Debug, Clone)]
pub struct VideoFacts {
    /// Index of the selected video stream — the packet filter.
    pub index: usize,
    /// Coded (pre-rotation, pre-SAR) pixel dimensions.
    pub width: u32,
    pub height: u32,
    /// Clockwise display rotation in degrees (0/90/180/270) from the stream's
    /// display-matrix side data.
    pub rotation: i32,
    /// Sample aspect ratio as a width multiplier (1.0 = square pixels).
    /// Anamorphic sources (DV, some MPEG-2) carry e.g. 16:15 here; ignoring it
    /// distorts the image (plan §5).
    pub sar: f64,
    /// Average frame rate; 0.0 when the container doesn't say.
    pub fps: f64,
    pub duration: Option<Duration>,
    pub has_audio: bool,
    /// Codec display name for the inspector ("H.264", "VP9", …).
    pub codec: &'static str,
    /// The selected stream's time base (num, den) — PTS units.
    pub time_base: (i32, i32),
    /// The stream's first PTS in time-base units, when the container declares
    /// one — nonzero start offsets exist in the wild (MPEG-TS starts ~766 ms in
    /// the Windows phase-0 spike); the producer's session-relative clock
    /// subtracts it (or the first frame's real PTS) as the origin.
    pub start_time: Option<i64>,
}

impl VideoFacts {
    /// Square-pixel dimensions as displayed: SAR applied (width scaled), then
    /// the rotation's axis swap. This is the geometry the fit box sees.
    pub fn display_dims(&self) -> (u32, u32) {
        let w = ((self.width as f64 * self.sar).round() as u32).max(1);
        let h = self.height.max(1);
        if self.rotation % 180 == 90 {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// Convert a PTS in this stream's time base to a `Duration` from zero.
    /// Negative timestamps clamp to zero (the producer normalizes via its
    /// origin before calling this).
    pub fn pts_to_duration(&self, pts: i64) -> Duration {
        let (num, den) = self.time_base;
        if pts <= 0 || num <= 0 || den <= 0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(pts as f64 * num as f64 / den as f64)
    }

    /// Convert a session-relative `Duration` to this stream's time-base units.
    pub fn duration_to_pts(&self, d: Duration) -> i64 {
        let (num, den) = self.time_base;
        if num <= 0 || den <= 0 {
            return 0;
        }
        (d.as_secs_f64() * den as f64 / num as f64).round() as i64
    }
}

/// Read the facts from an opened input. `Err` when there is no usable
/// (non-attached-picture) video stream.
pub fn video_facts(ctx: &ff::format::context::Input) -> Result<VideoFacts, String> {
    let stream = select_video_stream(ctx).ok_or("no video stream")?;
    let index = stream.index();
    let par = stream.parameters();
    let (width, height, sar) = unsafe {
        let p = par.as_ptr();
        let sar_r = (*p).sample_aspect_ratio;
        let sar = if sar_r.num > 0 && sar_r.den > 0 {
            sar_r.num as f64 / sar_r.den as f64
        } else {
            1.0
        };
        ((*p).width.max(0) as u32, (*p).height.max(0) as u32, sar)
    };
    let fr = stream.avg_frame_rate();
    let fps = if fr.numerator() > 0 && fr.denominator() > 0 {
        fr.numerator() as f64 / fr.denominator() as f64
    } else {
        0.0
    };
    let tb = stream.time_base();
    let st = stream.start_time();
    let start_time = (st != ffmpeg_next::ffi::AV_NOPTS_VALUE).then_some(st);
    let duration = stream_duration(ctx, &stream);
    let has_audio = ctx
        .streams()
        .any(|s| s.parameters().medium() == Type::Audio);
    Ok(VideoFacts {
        index,
        width,
        height,
        rotation: rotation_degrees(&stream),
        sar,
        fps,
        duration,
        has_audio,
        codec: codec_display_name(par.id()),
        time_base: (tb.numerator(), tb.denominator()),
        start_time,
    })
}

/// The best **non-attached-picture** video stream: `best()`'s pick when it
/// qualifies, else the first real video stream (covers the container where the
/// only thing `best()` likes is the cover art).
fn select_video_stream<'c>(
    ctx: &'c ff::format::context::Input,
) -> Option<ff::format::stream::Stream<'c>> {
    let attached = ff::format::stream::Disposition::ATTACHED_PIC;
    let qualifies = |s: &ff::format::stream::Stream| {
        s.parameters().medium() == Type::Video && !s.disposition().contains(attached)
    };
    if let Some(best) = ctx.streams().best(Type::Video) {
        if qualifies(&best) {
            return Some(best);
        }
    }
    ctx.streams().find(qualifies)
}

/// Duration from the stream (its own time base) falling back to the container
/// (`AV_TIME_BASE` micro-units). `None` for genuinely unbounded/unknown streams
/// — the session's HUD/seek clamp stays honest.
fn stream_duration(
    ctx: &ff::format::context::Input,
    stream: &ff::format::stream::Stream,
) -> Option<Duration> {
    let d = stream.duration();
    let tb = stream.time_base();
    if d > 0 && tb.numerator() > 0 && tb.denominator() > 0 {
        return Some(Duration::from_secs_f64(
            d as f64 * tb.numerator() as f64 / tb.denominator() as f64,
        ));
    }
    let cd = ctx.duration();
    if cd > 0 {
        return Some(Duration::from_micros(cd as u64));
    }
    None
}

/// The clockwise display rotation (0/90/180/270) from the video stream's
/// DISPLAYMATRIX side data, if any. iPhone portrait clips carry 90°/270°.
/// Returns 0 when absent. (FFmpeg 8 moved stream side data into codecpar's
/// `coded_side_data`, hence the ffi walk.)
pub fn rotation_degrees(stream: &ff::format::stream::Stream) -> i32 {
    use ffmpeg_next::ffi;
    unsafe {
        let par = (*stream.as_ptr()).codecpar;
        if par.is_null() {
            return 0;
        }
        let sd = ffi::av_packet_side_data_get(
            (*par).coded_side_data,
            (*par).nb_coded_side_data,
            ffi::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
        );
        if sd.is_null() || (*sd).data.is_null() {
            return 0;
        }
        // av_display_rotation_get returns the CCW angle in degrees; negate for CW,
        // then snap to the nearest quadrant.
        let angle = ffi::av_display_rotation_get((*sd).data as *const i32);
        if angle.is_nan() {
            return 0;
        }
        let cw = (-angle).round() as i32;
        ((cw % 360) + 360) % 360
    }
}

/// The stream's Dolby Vision summary (`AV_PKT_DATA_DOVI_CONF` coded side data),
/// or `None` for the common SDR/HDR10 case — the same walk as
/// [`rotation_degrees`] above. The record's first eight fields are the stable
/// `AVDOVIDecoderConfigurationRecord` ABI prefix (all `uint8_t`, no padding):
/// major, minor, profile, level, rpu, el, bl, bl_signal_compatibility_id — we
/// read only profile and compat-id here; the sample-buffer demux reads the full
/// record separately (it feeds VideoToolbox the packed box). Demuxer-only-safe:
/// no decoder is touched, so the `ffprobe` build can call it.
pub fn dovi_summary(stream: &ff::format::stream::Stream) -> Option<crate::video::DoviSummary> {
    use ffmpeg_next::ffi;
    unsafe {
        let par = (*stream.as_ptr()).codecpar;
        if par.is_null() {
            return None;
        }
        let sd = ffi::av_packet_side_data_get(
            (*par).coded_side_data,
            (*par).nb_coded_side_data,
            ffi::AVPacketSideDataType::AV_PKT_DATA_DOVI_CONF,
        );
        if sd.is_null() || (*sd).data.is_null() || (*sd).size < 8 {
            return None;
        }
        let d = std::slice::from_raw_parts((*sd).data, (*sd).size as usize);
        Some(crate::video::DoviSummary {
            profile: d[2],
            bl_compat_id: d[7],
        })
    }
}

/// Codec display names for the inspector panel — the same vocabulary the
/// Windows (MF subtype) and macOS (AVFoundation) probes use.
/// Mark every stream except `keep` as `AVDISCARD_ALL` (task #133 slice 3).
///
/// Scope guard: call this **only where the kept stream is the video stream**
/// (the producer, `VideoDemuxer`) — setting it around a kept *audio* stream
/// would risk the audio decoder's deliberate `stream_index = -1` seek (Matroska
/// Cues index the video track; see `audio_decoder.rs::seek`). Honest yield:
/// this saves demux-side parse/alloc/queue work everywhere, and skips unread
/// sample bytes on mov-family containers — on Matroska the payload bytes are
/// read before the discard check, so it is hygiene there, not traffic.
pub fn discard_all_except(ctx: &mut ff::format::context::Input, keep: usize) {
    unsafe {
        let fmt = ctx.as_mut_ptr();
        for i in 0..(*fmt).nb_streams as usize {
            if i == keep {
                continue;
            }
            let st = *(*fmt).streams.add(i);
            if !st.is_null() {
                (*st).discard = ff::ffi::AVDiscard::AVDISCARD_ALL;
            }
        }
    }
}

pub fn codec_display_name(id: ff::codec::Id) -> &'static str {
    use ff::codec::Id;
    match id {
        Id::H264 => "H.264",
        Id::HEVC => "HEVC",
        Id::VP8 => "VP8",
        Id::VP9 => "VP9",
        Id::AV1 => "AV1",
        Id::MPEG1VIDEO => "MPEG-1",
        Id::MPEG2VIDEO => "MPEG-2",
        Id::MPEG4 => "MPEG-4",
        Id::MJPEG => "MJPEG",
        Id::PRORES => "ProRes",
        Id::THEORA => "Theora",
        Id::WMV1 | Id::WMV2 | Id::WMV3 => "WMV",
        Id::VC1 => "VC-1",
        Id::DVVIDEO => "DV",
        Id::FFV1 => "FFV1",
        _ => "Video",
    }
}

/// Decode-to-fit output geometry: scale `display_dims` to fit inside `fit`
/// (never upscale), forced even (YUV→RGB converters and future NV12 need it).
/// The same rule as the Windows `mf_poster::fit_dims`, so poster ≡ playback
/// geometry across backends.
pub fn fit_dims(w: u32, h: u32, fit: crate::FitBox) -> (u32, u32) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::VideoInput;
    use crate::FitBox;

    fn open_fixture(name: &str) -> super::super::io::FfInput<'static> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name);
        super::super::io::FfInput::open(&VideoInput::Path(p), None).expect("open fixture")
    }

    #[test]
    fn facts_from_the_silent_fixture() {
        let mut input = open_fixture("black_then_color.mp4");
        let f = video_facts(input.ctx()).expect("facts");
        assert_eq!((f.width, f.height), (64, 64));
        assert_eq!(f.rotation, 0);
        assert!((f.sar - 1.0).abs() < 1e-9, "square pixels");
        assert!(!f.has_audio);
        assert_eq!(f.codec, "H.264");
        assert!(f.duration.expect("duration") > Duration::from_millis(800));
        assert!(f.fps > 25.0 && f.fps < 35.0, "fps {}", f.fps);
        assert_eq!(f.display_dims(), (64, 64));
    }

    #[test]
    fn facts_see_the_audio_track() {
        let mut input = open_fixture("color_with_tone.mp4");
        let f = video_facts(input.ctx()).expect("facts");
        assert!(f.has_audio, "the tone fixture has an AAC track");
    }

    #[test]
    fn pts_round_trips_through_the_time_base() {
        let mut input = open_fixture("black_then_color.mp4");
        let f = video_facts(input.ctx()).expect("facts");
        let half = Duration::from_millis(500);
        let pts = f.duration_to_pts(half);
        let back = f.pts_to_duration(pts);
        let err = back.abs_diff(half);
        assert!(err < Duration::from_millis(2), "round trip drifted {err:?}");
    }

    #[test]
    fn display_dims_apply_sar_and_rotation() {
        let f = VideoFacts {
            index: 0,
            width: 720,
            height: 576,
            rotation: 0,
            sar: 16.0 / 15.0, // PAL DV anamorphic
            fps: 25.0,
            duration: None,
            has_audio: false,
            codec: "DV",
            time_base: (1, 25),
            start_time: None,
        };
        assert_eq!(f.display_dims(), (768, 576), "SAR widens the display");
        let rotated = VideoFacts {
            rotation: 90,
            sar: 1.0,
            ..f
        };
        assert_eq!(rotated.display_dims(), (576, 720), "90° swaps the axes");
    }

    #[test]
    fn fit_dims_matches_the_mf_rule() {
        assert_eq!(
            fit_dims(
                3840,
                2160,
                FitBox {
                    max_width: 1920,
                    max_height: 1200
                }
            ),
            (1920, 1080)
        );
        // Never upscale; always even.
        assert_eq!(
            fit_dims(
                64,
                64,
                FitBox {
                    max_width: 1920,
                    max_height: 1080
                }
            ),
            (64, 64)
        );
        assert_eq!(
            fit_dims(
                101,
                67,
                FitBox {
                    max_width: 50,
                    max_height: 50
                }
            ),
            (50, 32)
        );
        assert_eq!(
            fit_dims(
                0,
                0,
                FitBox {
                    max_width: 8,
                    max_height: 6
                }
            ),
            (8, 6)
        );
    }
}
