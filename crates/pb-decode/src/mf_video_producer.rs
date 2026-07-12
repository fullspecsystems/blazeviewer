//! The Windows **video playback producer** (task #79 phase 4): a Media Foundation
//! reader thread speaking the [`VideoProducerEvent`]/[`VideoProducerMsg`] protocol.
//!
//! Demand-driven: the only blocking point is `recv()` on the session's message
//! channel — one [`Credit`](VideoProducerMsg::Credit) buys one decoded frame, and a
//! [`Stop`](VideoProducerMsg::Stop) (or channel disconnect: the session was dropped)
//! gets through even when zero credits are outstanding, so backpressure can never
//! deafen control. The reader is configured identically to the poster/metadata
//! probes (`mf_poster`): advanced video processing (rotation + YUV→RGB), all streams
//! deselected but video, color from the native media type, fitted RGB32 output —
//! poster ≡ playback in geometry, rotation, and color by construction.
//!
//! PTS discipline: timestamps are the reader's real sample times, normalized so the
//! first decoded frame is 0 (nonzero container start offsets exist in the wild —
//! MPEG-TS starts at ~766 ms in the phase-0 spike). Reads only, RAM-only: the
//! no-trace guarantee holds on the playback path exactly as it does for stills.

use std::path::Path;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{
    IMFSample, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};

use crate::mf_poster::{
    fit_dims, mf_open_msg, negotiate_rgb32, open_video_reader, retire_reader, stream_info,
};
use crate::mf_video::{ensure_mf, sample_to_rgba};
use crate::video::{
    SeekGeneration, VideoColorInfo, VideoFrame, VideoProducerEvent, VideoProducerMsg,
    VideoSessionId,
};
use crate::{FitBox, PixelFormat};

/// Run the producer to completion on the **current thread** (the app spawns a
/// dedicated thread for it — never the event loop). Returns when the stream ends,
/// the session says stop, the session is dropped, or decoding fails; every exit
/// path retires the reader off-thread (HEVC teardown blocks ~1 s).
pub fn run_video_producer(
    path: &Path,
    fit: Option<FitBox>,
    session_id: VideoSessionId,
    generation: SeekGeneration,
    events: Sender<VideoProducerEvent>,
    msgs: Receiver<VideoProducerMsg>,
) {
    ensure_mf();
    let fail = |error: String| {
        let _ = events.send(VideoProducerEvent::Failed { session_id, error });
    };

    let reader = match unsafe { open_video_reader(path) } {
        Ok(r) => r,
        Err(e) => return fail(e.to_string()),
    };
    let info = match unsafe { stream_info(&reader) } {
        Ok(i) => i,
        Err(e) => {
            retire_reader(reader);
            return fail(e.to_string());
        }
    };
    let (disp_w, disp_h) = info.display_dims();
    let fitted = fit.map(|f| fit_dims(disp_w, disp_h, f));
    let negotiated = unsafe {
        match fitted {
            Some(dims) => {
                negotiate_rgb32(&reader, Some(dims)).or_else(|_| negotiate_rgb32(&reader, None))
            }
            None => negotiate_rgb32(&reader, None),
        }
    };
    let (w, h, mut stride) = match negotiated {
        Ok(n) => n,
        Err(e) => {
            retire_reader(reader);
            return fail(mf_open_msg(e));
        }
    };
    if w == 0 || h == 0 {
        retire_reader(reader);
        return fail("video has no frames".into());
    }
    let _ = events.send(VideoProducerEvent::Opened {
        session_id,
        duration: info.duration,
        width: w,
        height: h,
    });
    let color = VideoColorInfo {
        transform: info.color,
        cicp: None,
        full_range: true,
    };

    // The credit/command loop. Blocking recv IS the select: a Stop (or the
    // session dropping its sender) wakes us regardless of credit starvation.
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut first_pts: Option<i64> = None;
    'outer: loop {
        match msgs.recv() {
            Err(_) | Ok(VideoProducerMsg::Stop) => break 'outer,
            Ok(VideoProducerMsg::Credit) => {
                // One credit = one frame (or the terminal event).
                loop {
                    let mut flags = 0u32;
                    let mut ts = 0i64;
                    let mut sample: Option<IMFSample> = None;
                    let read = unsafe {
                        reader.ReadSample(
                            video,
                            0,
                            None,
                            Some(&mut flags),
                            Some(&mut ts),
                            Some(&mut sample),
                        )
                    };
                    if let Err(e) = read {
                        fail(mf_open_msg(e));
                        break 'outer;
                    }
                    if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                        let _ = events.send(VideoProducerEvent::EndOfStream {
                            session_id,
                            seek_generation: generation,
                        });
                        break 'outer;
                    }
                    // A mid-stream media-type change can move the stride (the size
                    // is fixed by our negotiated output type); re-query it.
                    if flags != 0 {
                        if let Ok((nw, nh, ns)) = unsafe { negotiate_current(&reader) } {
                            if (nw, nh) != (w, h) {
                                fail("video changed size mid-stream".into());
                                break 'outer;
                            }
                            stride = ns;
                        }
                    }
                    let Some(sample) = sample else {
                        continue; // gap tick — read again for this credit
                    };
                    let rgba = match unsafe { sample_to_rgba(&sample, w, h, stride) } {
                        Ok(px) => px,
                        Err(e) => {
                            fail(format!("Media Foundation: {e}"));
                            break 'outer;
                        }
                    };
                    // Session-relative PTS: first decoded frame = 0.
                    let origin = *first_pts.get_or_insert(ts);
                    let pts_hns = (ts - origin).max(0) as u64;
                    let frame = VideoFrame {
                        session_id,
                        seek_generation: generation,
                        pts: Duration::from_nanos(pts_hns * 100),
                        width: w,
                        height: h,
                        format: PixelFormat::Rgba8,
                        pixels: rgba,
                        color: color.clone(),
                    };
                    if events.send(VideoProducerEvent::Frame(frame)).is_err() {
                        break 'outer; // session gone
                    }
                    break; // credit spent
                }
            }
        }
    }
    retire_reader(reader);
}

/// Re-read the negotiated output geometry/stride after a media-type-change tick.
unsafe fn negotiate_current(
    reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
) -> windows::core::Result<(u32, u32, i32)> {
    use windows::Win32::Media::MediaFoundation::{MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE};
    let cur = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)?;
    let packed = cur.GetUINT64(&MF_MT_FRAME_SIZE)?;
    let (w, h) = ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32);
    let stride = cur
        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
        .map(|s| s as i32)
        .unwrap_or((w * 4) as i32);
    Ok((w, h, stride))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Instant;

    const SID: VideoSessionId = VideoSessionId(42);
    const GEN: SeekGeneration = SeekGeneration::FIRST;

    fn fixture() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video/black_then_color.mp4")
    }

    fn spawn(path: std::path::PathBuf) -> (Sender<VideoProducerMsg>, Receiver<VideoProducerEvent>) {
        let (events_tx, events_rx) = channel();
        let (msgs_tx, msgs_rx) = channel();
        std::thread::spawn(move || {
            run_video_producer(&path, None, SID, GEN, events_tx, msgs_rx);
        });
        (msgs_tx, events_rx)
    }

    #[test]
    fn producer_streams_the_fixture_on_credits_and_reports_eos() {
        let (msgs, events) = spawn(fixture());
        // Opened arrives unprompted with the stream facts.
        let opened = events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened");
        match opened {
            VideoProducerEvent::Opened {
                session_id,
                duration,
                width,
                height,
            } => {
                assert_eq!(session_id, SID);
                assert_eq!((width, height), (64, 64));
                assert!(duration.expect("mp4 duration") > Duration::from_millis(800));
            }
            other => panic!("expected Opened, got {other:?}"),
        }

        // Credits pull frames one at a time, PTS normalized + monotonic.
        let mut last_pts = None;
        let mut frames = 0usize;
        loop {
            msgs.send(VideoProducerMsg::Credit).unwrap();
            match events.recv_timeout(Duration::from_secs(10)).expect("event") {
                VideoProducerEvent::Frame(f) => {
                    assert_eq!(f.session_id, SID);
                    assert!(f.is_well_formed());
                    if frames == 0 {
                        assert_eq!(f.pts, Duration::ZERO, "first PTS normalized to 0");
                    }
                    if let Some(prev) = last_pts {
                        assert!(f.pts > prev, "PTS must be monotonic");
                    }
                    last_pts = Some(f.pts);
                    frames += 1;
                }
                VideoProducerEvent::EndOfStream {
                    session_id,
                    seek_generation,
                } => {
                    assert_eq!(session_id, SID);
                    assert_eq!(seek_generation, GEN);
                    break;
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        // ~30 frames (1 s @ 30 fps).
        assert!(
            (25..=35).contains(&frames),
            "expected ~30 fixture frames, got {frames}"
        );
    }

    #[test]
    fn stop_interrupts_a_credit_starved_producer_quickly() {
        let (msgs, events) = spawn(fixture());
        let _ = events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened");
        // No credits outstanding: the producer is blocked on recv. Stop must
        // reach it and the channel must close promptly.
        let t0 = Instant::now();
        msgs.send(VideoProducerMsg::Stop).unwrap();
        // Drain any in-flight events; a receive error = disconnect = producer exited.
        while events.recv_timeout(Duration::from_secs(10)).is_ok() {}
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "stop must not hang behind backpressure"
        );
    }

    #[test]
    fn a_bad_path_fails_cleanly() {
        let (_msgs, events) = spawn(std::path::PathBuf::from(r"C:\nope\missing.mp4"));
        match events.recv_timeout(Duration::from_secs(10)).expect("event") {
            VideoProducerEvent::Failed { session_id, .. } => assert_eq!(session_id, SID),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
