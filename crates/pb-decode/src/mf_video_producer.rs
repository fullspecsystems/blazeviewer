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
    SeekGeneration, VideoColorInfo, VideoFrame, VideoInput, VideoProducerEvent, VideoProducerMsg,
    VideoSessionId,
};
use crate::{FitBox, PixelFormat};

/// Run the producer to completion on the **current thread** (the app spawns a
/// dedicated thread for it — never the event loop). Returns when the stream ends,
/// the session says stop, the session is dropped, or decoding fails; every exit
/// path retires the reader off-thread (HEVC teardown blocks ~1 s).
///
/// `input` is the container: a filesystem path, or an archive entry's in-RAM
/// bytes (`Arc`-shared, so the seek reopens below cost a refcount, not a copy).
pub fn run_video_producer(
    input: &VideoInput,
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

    let reader = match unsafe { open_video_reader(input) } {
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

    // 79.10: heavy **SDR** clips take the hardware path (NVDEC via the DXGI
    // device manager + NV12 output + in-shader YUV) when the whole setup chain
    // succeeds. Anything else — the pixel-rate policy saying no, a PQ/HLG
    // transfer (the shader must not mis-map HDR; MF SDR-converts it on the RGB32
    // path), a device/negotiation failure, odd output dims — stays on the
    // software RGB32 path byte-for-byte unchanged. `PB_VIDEO_FORCE_HW` overrides
    // the policy (A/B + tests), never the failure fallbacks.
    let (yuv_matrix, full_range, sdr) = unsafe { crate::mf_hw::native_yuv(&reader, info.height) };
    let want_hw = crate::mf_hw::hw_override()
        .unwrap_or_else(|| crate::mf_hw::pixel_rate_wants_hw(info.width, info.height, info.fps))
        && sdr;
    let mut manager = None;
    let mut hw_open = None;
    if want_hw {
        if let Some(mgr) = unsafe { crate::mf_hw::dxgi_manager() } {
            match unsafe { crate::mf_hw::open_nv12_reader(input, &mgr, fitted) } {
                Ok((r, hw_w, hw_h)) if hw_w > 0 && hw_h > 0 && hw_w % 2 == 0 && hw_h % 2 == 0 => {
                    hw_open = Some((r, hw_w, hw_h));
                    manager = Some(mgr);
                }
                Ok((r, _, _)) => retire_reader(r), // odd/zero dims → software
                Err(_) => {}
            }
        }
    }
    let (active_reader, mut kind, w, h) = match hw_open {
        Some((hw_reader, hw_w, hw_h)) => {
            retire_reader(reader); // the plain probe reader is done
            (hw_reader, OutKind::Nv12, hw_w, hw_h)
        }
        None => {
            let negotiated = unsafe {
                match fitted {
                    Some(dims) => negotiate_rgb32(&reader, Some(dims))
                        .or_else(|_| negotiate_rgb32(&reader, None)),
                    None => negotiate_rgb32(&reader, None),
                }
            };
            match negotiated {
                Ok((w, h, stride)) => (reader, OutKind::Rgb32 { stride }, w, h),
                Err(e) => {
                    retire_reader(reader);
                    return fail(mf_open_msg(e));
                }
            }
        }
    };
    if w == 0 || h == 0 {
        retire_reader(active_reader);
        return fail("video has no frames".into());
    }
    let format = kind.format();
    let _ = events.send(VideoProducerEvent::Opened {
        session_id,
        duration: info.duration,
        width: w,
        height: h,
        has_audio: info.has_audio,
        frame_bytes: format.frame_bytes(w, h) as u64,
    });
    let color = VideoColorInfo {
        transform: info.color,
        cicp: None,
        // The single-application contract: RGB32 output arrives with the YUV
        // matrix + range already applied by MF (fields inert); NV12 arrives raw
        // and the renderer's convert applies exactly these, exactly once.
        full_range: match kind {
            OutKind::Nv12 => full_range,
            OutKind::Rgb32 { .. } => true,
        },
        yuv_matrix,
    };

    // The credit/command/seek loop. Blocking recv IS the select: a Stop or a
    // SeekTo (or the session dropping its sender) wakes us regardless of credit
    // starvation. A SeekTo zeroes the credit balance — only credits received
    // after it (which the session sends after flushing) count.
    let mut origin: Option<i64> = None;
    let mut gen = generation;
    let mut credits: usize = 0;
    let mut pending: Option<(Duration, crate::video::SeekGeneration)> = None;
    let mut active: Option<IMFSourceReader> = Some(active_reader);

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
            let reader = match unsafe { reopen_at(input, (w, h), abs_target, manager.as_ref()) } {
                Ok((r, k)) => {
                    kind = k;
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
                match unsafe { read_one(reader, w, h, &mut kind) } {
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
                    Ok(Read1::Frame { ts, pixels }) => {
                        if ts >= abs_target {
                            landed = Some((ts, pixels));
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
            if let Some((ts, pixels)) = landed {
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
                    format,
                    pixels,
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
            match unsafe { read_one(reader, w, h, &mut kind) } {
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
                Ok(Read1::Frame { ts, pixels }) => {
                    let o = *origin.get_or_insert(ts);
                    let pts_hns = (ts - o).max(0) as u64;
                    let frame = VideoFrame {
                        session_id,
                        seek_generation: gen,
                        pts: Duration::from_nanos(pts_hns * 100),
                        width: w,
                        height: h,
                        format,
                        pixels,
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

/// Which decoded output this producer negotiated (task 79.10) — fixed for the
/// session; a seek recreates the reader in the same kind.
#[derive(Clone, Copy)]
enum OutKind {
    /// Software decode, RGB32 output, BGRX→RGBA swizzle (the shipping path).
    Rgb32 { stride: i32 },
    /// Hardware decode (DXGI manager), NV12 planes via `Lock2DSize`.
    Nv12,
}

impl OutKind {
    fn format(self) -> PixelFormat {
        match self {
            OutKind::Rgb32 { .. } => PixelFormat::Rgba8,
            OutKind::Nv12 => PixelFormat::Nv12,
        }
    }
}

/// One `ReadSample` step, handling gap ticks, mid-stream stride requery, and the
/// size-change failure. `Frame`'s `ts` is the reader's raw (container) timestamp;
/// `pixels` is packed per `OutKind` (RGBA8, or Y+UV planes).
enum Read1 {
    Frame { ts: i64, pixels: Vec<u8> },
    Eos,
    Gap,
}

unsafe fn read_one(
    reader: &windows::Win32::Media::MediaFoundation::IMFSourceReader,
    w: u32,
    h: u32,
    kind: &mut OutKind,
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
    // our negotiated output type); re-query it. NV12 reads its pitch per sample
    // (`Lock2DSize`), so only the size check applies there.
    if flags != 0 {
        if let Ok((nw, nh, ns)) = negotiate_current(reader) {
            if (nw, nh) != (w, h) {
                return Err("video changed size mid-stream".into());
            }
            if let OutKind::Rgb32 { stride } = kind {
                *stride = ns;
            }
        }
    }
    let Some(sample) = sample else {
        return Ok(Read1::Gap);
    };
    let pixels = match *kind {
        OutKind::Rgb32 { stride } => {
            sample_to_rgba(&sample, w, h, stride).map_err(|e| format!("Media Foundation: {e}"))?
        }
        OutKind::Nv12 => crate::mf_hw::sample_to_nv12(&sample, w, h)
            .map_err(|e| format!("Media Foundation: {e}"))?,
    };
    Ok(Read1::Frame { ts, pixels })
}

/// Fresh reader for a seek landing: open + negotiate the SAME output kind and
/// geometry the session fixed at start, then position **before the first read**
/// (~0 ms; spike E). The hw path reuses the producer's one DXGI manager. An
/// in-RAM input reopens over the same shared bytes (a fresh stream instance —
/// no re-read, no copy).
unsafe fn reopen_at(
    input: &VideoInput,
    dims: (u32, u32),
    position_hns: i64,
    manager: Option<&windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager>,
) -> Result<
    (
        windows::Win32::Media::MediaFoundation::IMFSourceReader,
        OutKind,
    ),
    String,
> {
    let (reader, kind) = match manager {
        Some(mgr) => {
            let (reader, nw, nh) =
                crate::mf_hw::open_nv12_reader(input, mgr, Some(dims)).map_err(mf_open_msg)?;
            if (nw, nh) != dims {
                retire_reader(reader);
                return Err("video output size changed across a seek".into());
            }
            (reader, OutKind::Nv12)
        }
        None => {
            let reader = open_video_reader(input).map_err(|e| e.to_string())?;
            let (nw, nh, stride) = negotiate_rgb32(&reader, Some(dims))
                .or_else(|_| negotiate_rgb32(&reader, None))
                .map_err(mf_open_msg)?;
            if (nw, nh) != dims {
                retire_reader(reader);
                return Err("video output size changed across a seek".into());
            }
            (reader, OutKind::Rgb32 { stride })
        }
    };
    let pos = crate::mf_poster::propvariant_i8(position_hns.max(0));
    if let Err(e) = reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &pos) {
        retire_reader(reader);
        return Err(mf_open_msg(e));
    }
    Ok((reader, kind))
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
        spawn_input(VideoInput::Path(path))
    }

    fn spawn_input(input: VideoInput) -> (Sender<VideoProducerMsg>, Receiver<VideoProducerEvent>) {
        let (events_tx, events_rx) = channel();
        let (msgs_tx, msgs_rx) = channel();
        std::thread::spawn(move || {
            run_video_producer(&input, None, SID, GEN, events_tx, msgs_rx);
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

    /// Task 79.10: the hardware reader plumbing decodes the committed fixture to
    /// well-formed NV12 planes (even dims, packed Y+UV). Skips gracefully where
    /// no D3D11 hardware device exists (CI VM without a GPU) — the producer's
    /// fallback covers that case in production.
    #[test]
    fn hw_reader_decodes_the_fixture_to_nv12() {
        ensure_mf();
        unsafe {
            let Some(mgr) = crate::mf_hw::dxgi_manager() else {
                eprintln!("no D3D11 hardware device — skipping");
                return;
            };
            let input = VideoInput::Path(fixture());
            let (reader, w, h) = match crate::mf_hw::open_nv12_reader(&input, &mgr, None) {
                Ok(x) => x,
                Err(e) => {
                    eprintln!("hw NV12 open failed ({e}) — skipping (fallback covers this)");
                    return;
                }
            };
            assert!(w > 0 && h > 0 && w % 2 == 0 && h % 2 == 0, "{w}x{h}");
            let mut kind = OutKind::Nv12;
            let mut frames = 0usize;
            for _ in 0..40 {
                match read_one(&reader, w, h, &mut kind) {
                    Ok(Read1::Frame { pixels, .. }) => {
                        assert_eq!(
                            pixels.len(),
                            PixelFormat::Nv12.frame_bytes(w, h),
                            "packed NV12 planes"
                        );
                        frames += 1;
                        if frames >= 5 {
                            break;
                        }
                    }
                    Ok(Read1::Eos) => break,
                    Ok(Read1::Gap) => {}
                    Err(e) => panic!("hw read failed: {e}"),
                }
            }
            assert!(frames >= 1, "the hw path produced NV12 frames");
            retire_reader(reader);
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

    /// Archive playback's core claim: the producer streams, seeks (a reopen over the
    /// SAME shared bytes — no path anywhere), and reports EOS from an in-RAM
    /// container exactly like a file.
    #[test]
    fn producer_streams_and_seeks_from_in_ram_bytes() {
        let data = std::sync::Arc::new(std::fs::read(fixture()).expect("fixture bytes"));
        let (msgs, events) = spawn_input(VideoInput::Bytes {
            data,
            name: "sub/clip.mp4".into(),
        });
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened")
        {
            VideoProducerEvent::Opened { width, height, .. } => {
                assert_eq!((width, height), (64, 64), "bytes open ≡ path open");
            }
            other => panic!("expected Opened, got {other:?}"),
        }
        // One sequential frame…
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events.recv_timeout(Duration::from_secs(10)).expect("frame") {
            VideoProducerEvent::Frame(f) => {
                assert!(f.is_well_formed());
                assert_eq!(f.pts, Duration::ZERO);
            }
            other => panic!("expected a frame, got {other:?}"),
        }
        // …then a seek, which exercises the fresh-reader reopen from the bytes.
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
                assert_eq!(f.seek_generation, g1);
                assert!(f.pts >= Duration::from_millis(500), "landed at {:?}", f.pts);
            }
            other => panic!("expected the landing frame, got {other:?}"),
        }
    }

    /// Hostile in-RAM bytes fail with a structured error, never a hang or crash.
    #[test]
    fn garbage_bytes_fail_cleanly() {
        let (_msgs, events) = spawn_input(VideoInput::Bytes {
            data: std::sync::Arc::new(vec![0x55u8; 4096]),
            name: "junk.mp4".into(),
        });
        match events.recv_timeout(Duration::from_secs(10)).expect("event") {
            VideoProducerEvent::Failed { session_id, .. } => assert_eq!(session_id, SID),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
