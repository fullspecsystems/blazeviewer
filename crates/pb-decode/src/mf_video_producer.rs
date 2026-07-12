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
    IMFSample, IMFSourceReader, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
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
        has_audio: info.has_audio,
        frame_bytes: PixelFormat::Rgba8.frame_bytes(w, h) as u64,
    });
    let color = VideoColorInfo {
        transform: info.color,
        cicp: None,
        // RGB32 output: MF already applied the YUV matrix + range (see the
        // VideoColorInfo single-application contract) — these two are inert.
        full_range: true,
        yuv_matrix: crate::video::YuvMatrix::Bt709,
    };

    // The credit/command/seek loop. Blocking recv IS the select: a Stop or a
    // SeekTo (or the session dropping its sender) wakes us regardless of credit
    // starvation. A SeekTo zeroes the credit balance — only credits received
    // after it (which the session sends after flushing) count.
    let mut origin: Option<i64> = None;
    let mut gen = generation;
    let mut credits: usize = 0;
    let mut pending: Option<(Duration, crate::video::SeekGeneration)> = None;
    let mut active: Option<IMFSourceReader> = Some(reader);

    'outer: loop {
        // 1. Absorb messages; block only when there is nothing to do.
        loop {
            let msg = if credits == 0 && pending.is_none() {
                match msgs.recv() {
                    Ok(m) => m,
                    Err(_) => break 'outer,
                }
            } else {
                match msgs.try_recv() {
                    Ok(m) => m,
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(_) => break 'outer,
                }
            };
            match msg {
                VideoProducerMsg::Stop => break 'outer,
                VideoProducerMsg::Credit => credits += 1,
                VideoProducerMsg::SeekTo { target, generation } => {
                    pending = Some((target, generation));
                    credits = 0;
                }
            }
        }

        // 2. Land a pending seek: retire the old reader (repositioning a warm
        // HEVC reader blocks ~1 s; a fresh one positions before its first read in
        // ~0 ms — spike E), open at the target, decode forward to the first frame
        // ≥ it. A newer SeekTo supersedes every stage; a superseded landing never
        // publishes a frame.
        if let Some((target, g)) = pending.take() {
            gen = g;
            if let Some(r) = active.take() {
                retire_reader(r);
            }
            let abs_target = origin
                .unwrap_or(0)
                .saturating_add((target.as_nanos() / 100) as i64);
            let reader = match unsafe { reopen_at(path, (w, h), abs_target) } {
                Ok((r, s)) => {
                    stride = s;
                    r
                }
                Err(e) => {
                    fail(e);
                    break 'outer;
                }
            };
            active = Some(reader);
            let mut landed: Option<(i64, Vec<u8>)> = None;
            loop {
                // Watch for supersede/stop between reads (latest-value).
                match msgs.try_recv() {
                    Ok(VideoProducerMsg::Stop) => break 'outer,
                    Ok(VideoProducerMsg::Credit) => credits += 1,
                    Ok(VideoProducerMsg::SeekTo { target, generation }) => {
                        pending = Some((target, generation));
                        credits = 0;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(_) => break 'outer,
                }
                let reader = active.as_ref().expect("reader set above");
                match unsafe { read_one(reader, w, h, &mut stride) } {
                    Ok(Read1::Eos) => {
                        // Sought at/near the end: the stream is over under the
                        // new generation; the reader is spent.
                        let _ = events.send(VideoProducerEvent::EndOfStream {
                            session_id,
                            seek_generation: gen,
                        });
                        if let Some(r) = active.take() {
                            retire_reader(r);
                        }
                        break;
                    }
                    Ok(Read1::Gap) => {}
                    Ok(Read1::Frame { ts, rgba }) => {
                        if ts >= abs_target {
                            landed = Some((ts, rgba));
                            break;
                        }
                        // Keyframe→target run-up: discard, keep decoding forward.
                    }
                    Err(e) => {
                        fail(e);
                        break 'outer;
                    }
                }
            }
            if let Some((ts, rgba)) = landed {
                // The landing frame consumes a credit like any other (the session
                // granted fresh ones right behind the SeekTo). Block for one if
                // needed — Stop/SeekTo still interrupt.
                while credits == 0 && pending.is_none() {
                    match msgs.recv() {
                        Ok(VideoProducerMsg::Credit) => credits += 1,
                        Ok(VideoProducerMsg::SeekTo { target, generation }) => {
                            pending = Some((target, generation));
                            credits = 0;
                        }
                        Ok(VideoProducerMsg::Stop) | Err(_) => break 'outer,
                    }
                }
                if pending.is_some() {
                    continue 'outer; // superseded before publish — never flash it
                }
                origin.get_or_insert(abs_target - (target.as_nanos() / 100) as i64);
                let pts_hns = (ts - origin.unwrap_or(0)).max(0) as u64;
                let frame = VideoFrame {
                    session_id,
                    seek_generation: gen,
                    pts: Duration::from_nanos(pts_hns * 100),
                    width: w,
                    height: h,
                    format: PixelFormat::Rgba8,
                    pixels: rgba,
                    color: color.clone(),
                };
                if events.send(VideoProducerEvent::Frame(frame)).is_err() {
                    break 'outer;
                }
                credits -= 1;
            }
            continue 'outer;
        }

        // 3. Spend one credit on the next sequential frame.
        if credits > 0 {
            let Some(reader) = active.as_ref() else {
                // No reader (parked after EOS): these credits are stale — a seek
                // recreates the reader and resets the balance.
                credits = 0;
                continue;
            };
            match unsafe { read_one(reader, w, h, &mut stride) } {
                Ok(Read1::Eos) => {
                    let _ = events.send(VideoProducerEvent::EndOfStream {
                        session_id,
                        seek_generation: gen,
                    });
                    // Park (don't exit): a later SeekTo replays/rewinds by
                    // recreating the reader; Stop/disconnect ends the thread.
                    if let Some(r) = active.take() {
                        retire_reader(r);
                    }
                }
                Ok(Read1::Gap) => {}
                Ok(Read1::Frame { ts, rgba }) => {
                    let o = *origin.get_or_insert(ts);
                    let pts_hns = (ts - o).max(0) as u64;
                    let frame = VideoFrame {
                        session_id,
                        seek_generation: gen,
                        pts: Duration::from_nanos(pts_hns * 100),
                        width: w,
                        height: h,
                        format: PixelFormat::Rgba8,
                        pixels: rgba,
                        color: color.clone(),
                    };
                    if events.send(VideoProducerEvent::Frame(frame)).is_err() {
                        break 'outer;
                    }
                    credits -= 1;
                }
                Err(e) => {
                    fail(e);
                    break 'outer;
                }
            }
        }
    }
    if let Some(r) = active {
        retire_reader(r);
    }
}

/// One `ReadSample` step, handling gap ticks, mid-stream stride requery, and the
/// size-change failure. `Frame`'s `ts` is the reader's raw (container) timestamp.
enum Read1 {
    Frame { ts: i64, rgba: Vec<u8> },
    Eos,
    Gap,
}

unsafe fn read_one(
    reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
    w: u32,
    h: u32,
    stride: &mut i32,
) -> Result<Read1, String> {
    let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut flags = 0u32;
    let mut ts = 0i64;
    let mut sample: Option<IMFSample> = None;
    reader
        .ReadSample(
            video,
            0,
            None,
            Some(&mut flags),
            Some(&mut ts),
            Some(&mut sample),
        )
        .map_err(mf_open_msg)?;
    if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
        return Ok(Read1::Eos);
    }
    // A mid-stream media-type change can move the stride (the size is fixed by
    // our negotiated output type); re-query it.
    if flags != 0 {
        if let Ok((nw, nh, ns)) = negotiate_current(reader) {
            if (nw, nh) != (w, h) {
                return Err("video changed size mid-stream".into());
            }
            *stride = ns;
        }
    }
    let Some(sample) = sample else {
        return Ok(Read1::Gap);
    };
    let rgba =
        sample_to_rgba(&sample, w, h, *stride).map_err(|e| format!("Media Foundation: {e}"))?;
    Ok(Read1::Frame { ts, rgba })
}

/// Fresh reader for a seek landing: open + negotiate the SAME output geometry the
/// session fixed at start, then position **before the first read** (~0 ms; spike E).
unsafe fn reopen_at(
    path: &Path,
    dims: (u32, u32),
    position_hns: i64,
) -> Result<(windows::Win32::Media::MediaFoundation::IMFSourceReader, i32), String> {
    let reader = open_video_reader(path).map_err(|e| e.to_string())?;
    let (nw, nh, stride) = negotiate_rgb32(&reader, Some(dims))
        .or_else(|_| negotiate_rgb32(&reader, None))
        .map_err(mf_open_msg)?;
    if (nw, nh) != dims {
        retire_reader(reader);
        return Err("video output size changed across a seek".into());
    }
    let pos = crate::mf_poster::propvariant_i8(position_hns.max(0));
    if let Err(e) = reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &pos) {
        retire_reader(reader);
        return Err(mf_open_msg(e));
    }
    Ok((reader, stride))
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
                has_audio,
                frame_bytes,
            } => {
                assert_eq!(session_id, SID);
                assert_eq!((width, height), (64, 64));
                assert!(duration.expect("mp4 duration") > Duration::from_millis(800));
                assert!(!has_audio, "the black/color fixture is silent");
                assert_eq!(frame_bytes, 64 * 64 * 4, "credit size = negotiated output");
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

    /// Phase 6: a SeekTo recreates the reader at the target and the next published
    /// frame carries the new generation with pts ≥ the target; a superseding
    /// SeekTo sent before the credit means the older landing never publishes.
    #[test]
    fn seek_lands_at_the_target_with_the_new_generation() {
        let (msgs, events) = spawn(fixture());
        let _ = events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened");
        // Pull one normal frame first (establishes the PTS origin).
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events.recv_timeout(Duration::from_secs(10)).expect("frame") {
            VideoProducerEvent::Frame(f) => assert_eq!(f.pts, Duration::ZERO),
            other => panic!("expected a frame, got {other:?}"),
        }

        // Seek to 0.5 s (fixture is ~1 s @ 30 fps).
        let g1 = SeekGeneration(1);
        msgs.send(VideoProducerMsg::SeekTo {
            target: Duration::from_millis(500),
            generation: g1,
        })
        .unwrap();
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("landing")
        {
            VideoProducerEvent::Frame(f) => {
                assert_eq!(f.seek_generation, g1, "landing carries the new generation");
                assert!(
                    f.pts >= Duration::from_millis(500) && f.pts < Duration::from_millis(700),
                    "landed at {:?}",
                    f.pts
                );
            }
            other => panic!("expected the landing frame, got {other:?}"),
        }

        // Supersede: two seeks, credits only after — the first must never flash.
        let g2 = SeekGeneration(2);
        let g3 = SeekGeneration(3);
        msgs.send(VideoProducerMsg::SeekTo {
            target: Duration::from_millis(100),
            generation: g2,
        })
        .unwrap();
        msgs.send(VideoProducerMsg::SeekTo {
            target: Duration::from_millis(700),
            generation: g3,
        })
        .unwrap();
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("landing")
        {
            VideoProducerEvent::Frame(f) => {
                assert_eq!(f.seek_generation, g3, "only the newest seek publishes");
                assert!(f.pts >= Duration::from_millis(700), "landed at {:?}", f.pts);
            }
            other => panic!("expected the superseding landing, got {other:?}"),
        }
    }

    /// Phase 6: after EOS the producer parks (doesn't exit) so a replay/rewind
    /// SeekTo still works on the same session.
    #[test]
    fn seek_after_eos_replays_from_the_target() {
        let (msgs, events) = spawn(fixture());
        let _ = events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened");
        // Drain the whole stream.
        loop {
            msgs.send(VideoProducerMsg::Credit).unwrap();
            match events.recv_timeout(Duration::from_secs(10)).expect("event") {
                VideoProducerEvent::Frame(_) => {}
                VideoProducerEvent::EndOfStream { .. } => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        // Replay: seek to 0 on the parked producer.
        let g1 = SeekGeneration(1);
        msgs.send(VideoProducerMsg::SeekTo {
            target: Duration::ZERO,
            generation: g1,
        })
        .unwrap();
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("replay")
        {
            VideoProducerEvent::Frame(f) => {
                assert_eq!(f.seek_generation, g1);
                assert_eq!(f.pts, Duration::ZERO, "replay starts at zero");
            }
            other => panic!("expected the replay frame, got {other:?}"),
        }
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

    /// Phase 5: `Opened` reports the audio track's presence — the signal that
    /// starts the shell audio player (silent clips never get one).
    #[test]
    fn opened_reports_an_audio_track_when_one_exists() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video/color_with_tone.mp4");
        let (_msgs, events) = spawn(path);
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened")
        {
            VideoProducerEvent::Opened { has_audio, .. } => {
                assert!(has_audio, "the tone fixture has an AAC track");
            }
            other => panic!("expected Opened, got {other:?}"),
        }
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
