//! The FFmpeg video **poster + probe** (plan §8-poster): the clip's first
//! non-black frame as a [`DecodedImage`], and the reader-sourced stream facts
//! behind the inspector's video rows — for the containers/codecs the OS
//! backends can't open (ALL video on Linux; MKV/WebM/VP8/VP9/AV1 on macOS).
//!
//! Mirrors the Windows `mf_poster` policy exactly: the shared mean-luma walk
//! (`poster_frame_bright_enough`, ~1 s / 30 frames, last-sampled-frame
//! fallback), fitted decode, rotation + color identical to playback (the
//! producer shares `probe`/`convert`). Honors the decode pool's `cancel` flag
//! **inside** blocking libav work via the AVIO interrupt callback (plan §6) —
//! task #83's multi-consumer pool makes prompt cancellation matter.

use std::sync::atomic::AtomicBool;

use ffmpeg_next as ff;

use super::convert::FrameConverter;
use super::io::FfInput;
use super::probe::{fit_dims, video_facts};
use crate::video::{poster_frame_bright_enough, VideoInput, POSTER_MAX_FRAMES, POSTER_MAX_MEDIA};
use crate::{DecodeError, DecodedImage, FitBox, PixelFormat, VideoStreamInfo};

/// Overall watchdog for one poster attempt — a poster is a background nicety,
/// never worth pinning a pool worker longer than this on hostile input.
const POSTER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);

/// Decode the clip's first non-black frame (capped walk, last-frame fallback)
/// at `fit`, from a path or in-RAM archive bytes. Read-only, RAM-only.
pub fn ff_decode_video_poster(
    input: &VideoInput,
    fit: Option<FitBox>,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    poster_inner(input, fit, cancel).map_err(DecodeError::Corrupt)
}

/// One read-only open's stream facts (inspector parity with the Windows
/// `probe_video_input` / macOS `probe_video_stream`).
pub fn ff_probe_video_input(input: &VideoInput) -> Result<VideoStreamInfo, DecodeError> {
    let mut opened = FfInput::open(input, None).map_err(DecodeError::Corrupt)?;
    let facts = video_facts(opened.ctx()).map_err(DecodeError::Corrupt)?;
    // Decoder-reported color under the shared fallback policy (no frame decode
    // here — the probe is a ~header-only read, like the Windows one).
    let decoder = decoder_for(opened.ctx(), facts.index)?;
    let conv = FrameConverter::new((facts.width, facts.height), (1, 1), 0, &decoder);
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

/// The decoder for stream `index` of an opened input.
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

fn poster_inner(
    input: &VideoInput,
    fit: Option<FitBox>,
    cancel: &AtomicBool,
) -> Result<DecodedImage, String> {
    let mut opened = FfInput::open(input, Some(cancel))?;
    opened.set_op_deadline(Some(POSTER_DEADLINE));
    let facts = video_facts(opened.ctx())?;
    let mut decoder = decoder_for(opened.ctx(), facts.index).map_err(|e| e.to_string())?;

    // Output geometry: fit the SAR-corrected display dims, then map back to
    // pre-rotation axes for the scaler (the converter rotates after).
    let (disp_w, disp_h) = facts.display_dims();
    let (fw, fh) = match fit {
        Some(f) => fit_dims(disp_w, disp_h, f),
        None => (disp_w, disp_h),
    };
    let pre_rot = if facts.rotation % 180 == 90 {
        (fh, fw)
    } else {
        (fw, fh)
    };
    let mut conv = FrameConverter::new(
        (facts.width, facts.height),
        pre_rot,
        facts.rotation,
        &decoder,
    );

    // The mean-luma walk: accept the first bright-enough frame; keep the last
    // sampled one as the fallback (a dark-throughout clip still gets a poster).
    let mut last: Option<(Vec<u8>, u32, u32)> = None;
    let mut sampled = 0usize;
    let mut first_ts: Option<i64> = None;
    let mut packet = ff::Packet::empty();
    let mut eof_sent = false;
    let max_media = facts.duration_to_pts(POSTER_MAX_MEDIA);
    'walk: while sampled < POSTER_MAX_FRAMES {
        if opened.cancelled() {
            return Err("cancelled".into());
        }
        // Pull every ready frame before feeding more packets.
        let mut decoded = ff::frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            let (rgba, w, h) = conv.convert(&decoded)?;
            sampled += 1;
            let ts = decoded.timestamp().unwrap_or(0);
            let origin = *first_ts.get_or_insert(ts);
            let bright = poster_frame_bright_enough(&rgba);
            let deep_enough = ts.saturating_sub(origin) >= max_media && max_media > 0;
            last = Some((rgba, w, h));
            if bright || deep_enough || sampled >= POSTER_MAX_FRAMES {
                break 'walk;
            }
            if opened.cancelled() {
                return Err("cancelled".into());
            }
        }
        if eof_sent {
            break; // decoder fully drained
        }
        match packet.read(opened.ctx()) {
            Ok(()) => {
                if packet.stream() == facts.index {
                    // A corrupt packet is skipped, not fatal — the walk only
                    // needs one good frame.
                    let _ = decoder.send_packet(&packet);
                }
            }
            Err(ff::Error::Eof) => {
                let _ = decoder.send_eof();
                eof_sent = true;
            }
            Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => {}
            Err(e) => return Err(format!("FFmpeg read: {e}")),
        }
    }
    let (rgba, w, h) = last.ok_or("video decoded no frames")?;
    let sc = conv.source_color();
    Ok(DecodedImage {
        width: w,
        height: h,
        orig_width: disp_w,
        orig_height: disp_h,
        codec: facts.codec,
        format: PixelFormat::Rgba8,
        pixels: rgba,
        is_preview: false,
        color: super::color::sdr_transform(&sc),
        peak: 1.0,
        animated: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    fn poster(name: &str, fit: Option<FitBox>) -> Result<DecodedImage, DecodeError> {
        ff_decode_video_poster(
            &VideoInput::Path(fixture(name)),
            fit,
            &AtomicBool::new(false),
        )
    }

    /// The fixture opens on black frames; the walk must skip them and land on
    /// the color section — the exact contract the Windows poster test locks.
    #[test]
    fn poster_walks_past_the_black_lead() {
        let img = poster("black_then_color.mp4", None).expect("poster");
        assert!(img.is_well_formed());
        assert_eq!((img.width, img.height), (64, 64));
        assert_eq!(img.codec, "H.264");
        assert!(
            crate::video::mean_luma_rgba8(&img.pixels, 1) > crate::video::POSTER_LUMA_MIN,
            "poster landed on a black frame"
        );
    }

    #[test]
    fn poster_fits_and_keeps_even_dims() {
        let img = poster(
            "black_then_color.mp4",
            Some(FitBox {
                max_width: 32,
                max_height: 32,
            }),
        )
        .expect("poster");
        assert_eq!((img.width, img.height), (32, 32));
        assert_eq!((img.orig_width, img.orig_height), (64, 64));
        assert!(img.is_well_formed());
    }

    #[test]
    fn poster_from_in_ram_bytes_matches_the_path_poster() {
        let data = std::sync::Arc::new(std::fs::read(fixture("black_then_color.mp4")).unwrap());
        let by_bytes = ff_decode_video_poster(
            &VideoInput::Bytes {
                data,
                name: "clip.mp4".into(),
            },
            None,
            &AtomicBool::new(false),
        )
        .expect("bytes poster");
        let by_path = poster("black_then_color.mp4", None).expect("path poster");
        assert_eq!(by_bytes.pixels, by_path.pixels, "bytes ≡ path, bit-exact");
    }

    #[test]
    fn cancel_aborts_the_poster() {
        let r = ff_decode_video_poster(
            &VideoInput::Path(fixture("black_then_color.mp4")),
            None,
            &AtomicBool::new(true),
        );
        assert!(r.is_err(), "a cancelled poster must not decode");
    }

    #[test]
    fn probe_reports_the_stream_facts() {
        let info =
            ff_probe_video_input(&VideoInput::Path(fixture("color_with_tone.mp4"))).expect("probe");
        assert_eq!(info.codec, "H.264");
        assert!(info.has_audio);
        assert!(info.duration.is_some());
        assert_eq!(
            info.display_dims(),
            (info.width, info.height),
            "no rotation"
        );
    }

    #[test]
    fn garbage_bytes_fail_cleanly() {
        let r = ff_decode_video_poster(
            &VideoInput::Bytes {
                data: std::sync::Arc::new(vec![0xAAu8; 2048]),
                name: "junk.webm".into(),
            },
            None,
            &AtomicBool::new(false),
        );
        assert!(r.is_err());
    }
}
