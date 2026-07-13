//! The cross-platform FFmpeg **video playback producer** (task #84, plan §5):
//! a demuxer+decoder thread speaking the [`VideoProducerEvent`]/[`VideoProducerMsg`]
//! protocol — the same observable contract as the Windows `mf_video_producer`,
//! against the same unit-tested `VideoSession` — with FFmpeg-idiomatic internals.
//!
//! Where the MF producer **recreates its reader** to seek (repositioning a warm
//! HEVC MF reader blocks ~1 s — spike-locked), FFmpeg seeks **in place**:
//! `avformat_seek_file` to a keyframe at/before the target, flush the decoder,
//! decode forward discarding to the landing frame (polling for a superseding
//! `SeekTo` between frames, latest-value-wins). After end-of-stream the
//! producer **parks** — the demuxer stays open — so `P`-replay is a seek to 0,
//! never a rebuild.
//!
//! Demand-driven: the only blocking point is `recv()` on the session's message
//! channel — one `Credit` buys one decoded frame, and a `Stop` (or channel
//! disconnect) gets through even with zero credits outstanding. *Inside* a
//! blocking libav call the AVIO interrupt watchdog (`io.rs`) bounds hostile
//! input; between calls the packet/error budgets below bound corrupt input
//! ("can't spin forever hunting one credited frame", plan §5).
//!
//! PTS discipline: `best_effort_timestamp` × stream time base, normalized so
//! the session clock starts at 0 (container start offsets subtracted via the
//! origin); missing timestamps synthesize deterministically from the frame
//! rate. B-frame reorder/delay is drained at EOF (`send_eof` + drain).
//!
//! Reads only, RAM-only: the no-trace guarantee holds on this path exactly as
//! it does for stills. Audio is a **separate subsystem** (plan §7): a second
//! FFmpeg instance (`audio_decoder.rs`) feeds the platform sink; `Opened`
//! reports the track's real presence, and a shell without a sink reports a
//! Failed clock so playback degrades to silent immediately.

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use ffmpeg_next as ff;

use super::convert::FrameConverter;
use super::io::FfInput;
use super::probe::{fit_dims, video_facts, VideoFacts};
use crate::video::{
    SeekGeneration, VideoColorInfo, VideoFrame, VideoInput, VideoProducerEvent, VideoProducerMsg,
    VideoSessionId,
};
use crate::{FitBox, PixelFormat};

/// Watchdog budget for one read/decode burst (one credited frame). Local
/// files/RAM resolve in milliseconds; only hostile input hits this.
const OP_DEADLINE: Duration = Duration::from_secs(10);
/// Packets fed without producing one frame before declaring the input stuck.
const MAX_PACKETS_PER_FRAME: usize = 4096;
/// Consecutive rejected packets (decoder said no) before giving up.
const MAX_BAD_PACKETS: usize = 512;

/// Run the producer to completion on the **current thread** (the app spawns a
/// dedicated thread for it — never the event loop). Returns when the stream
/// ends and the session stops it, the session is dropped, or decoding fails.
/// The FFmpeg mirror of `run_video_producer` (Windows MF) behind the same
/// protocol; `input` is a filesystem path or an archive entry's in-RAM bytes.
pub fn run_ff_video_producer(
    input: &VideoInput,
    fit: Option<FitBox>,
    session_id: VideoSessionId,
    generation: SeekGeneration,
    events: Sender<VideoProducerEvent>,
    msgs: Receiver<VideoProducerMsg>,
) {
    let fail = |error: String| {
        let _ = events.send(VideoProducerEvent::Failed { session_id, error });
    };
    let mut reader = match Reader::open(input, fit) {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let (out_w, out_h) = reader.conv.display_dims();
    let _ = events.send(VideoProducerEvent::Opened {
        session_id,
        duration: reader.facts.duration,
        width: out_w,
        height: out_h,
        // Honest track presence (plan §7 — the muted interim ended when the
        // FFmpeg audio decoder + platform sinks landed): the session starts the
        // shell audio player for real tracks; a shell with no sink reports a
        // Failed clock immediately and playback degrades to silent.
        has_audio: reader.facts.has_audio,
        // Format-aware (task 79.10): an fp16 HDR clip charges 8 bytes/px, so
        // the session's byte budget isn't under-credited 2× on PQ/HLG sources.
        frame_bytes: reader.conv.output_format().frame_bytes(out_w, out_h) as u64,
    });

    // The credit/command/seek loop — the same shape as the MF producer (the
    // protocol tests hold both to it). Blocking recv IS the select.
    let mut gen = generation;
    let mut credits: usize = 0;
    let mut pending: Option<(Duration, SeekGeneration)> = None;

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

        // 2. Land a pending seek: keyframe ≤ target, flush, decode forward to
        // the first frame ≥ it. A newer SeekTo supersedes every stage; a
        // superseded landing never publishes a frame.
        if let Some((target, g)) = pending.take() {
            gen = g;
            let target_units = match reader.seek(target) {
                Ok(t) => t,
                Err(e) => {
                    fail(e);
                    break 'outer;
                }
            };
            let mut landed: Option<(i64, Vec<u8>)> = None;
            loop {
                // Watch for supersede/stop between decodes (latest-value).
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
                match reader.next_frame() {
                    Ok(Some((ts, rgba))) => {
                        if ts >= target_units {
                            landed = Some((ts, rgba));
                            break;
                        }
                        // Keyframe→target run-up: discard, keep decoding.
                    }
                    Ok(None) => {
                        // Sought at/near the end: the stream is over under the
                        // new generation; park for the next seek.
                        let _ = events.send(VideoProducerEvent::EndOfStream {
                            session_id,
                            seek_generation: gen,
                        });
                        reader.parked = true;
                        break;
                    }
                    Err(e) => {
                        fail(e);
                        break 'outer;
                    }
                }
            }
            if let Some((ts, rgba)) = landed {
                // The landing frame consumes a credit like any other (the
                // session granted fresh ones right behind the SeekTo). Block
                // for one if needed — Stop/SeekTo still interrupt.
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
                // Seek before any normal frame: anchor the origin so the
                // landing stamps exactly `target` (the MF fallback, mirrored).
                reader
                    .origin
                    .get_or_insert(target_units - reader.facts.duration_to_pts(target));
                let frame = reader.make_frame(session_id, gen, ts, rgba);
                if events.send(VideoProducerEvent::Frame(frame)).is_err() {
                    break 'outer;
                }
                credits -= 1;
            }
            continue 'outer;
        }

        // 3. Spend one credit on the next sequential frame.
        if credits > 0 {
            if reader.parked {
                // Parked after EOS: these credits are stale — a seek resets.
                credits = 0;
                continue;
            }
            match reader.next_frame() {
                Ok(Some((ts, rgba))) => {
                    reader.origin.get_or_insert(ts);
                    let frame = reader.make_frame(session_id, gen, ts, rgba);
                    if events.send(VideoProducerEvent::Frame(frame)).is_err() {
                        break 'outer;
                    }
                    credits -= 1;
                }
                Ok(None) => {
                    let _ = events.send(VideoProducerEvent::EndOfStream {
                        session_id,
                        seek_generation: gen,
                    });
                    // Park (don't exit): a later SeekTo replays/rewinds in
                    // place; Stop/disconnect ends the thread.
                    reader.parked = true;
                }
                Err(e) => {
                    fail(e);
                    break 'outer;
                }
            }
        }
    }
    // Everything (decoder, demuxer, AVIO, bytes refcount) drops with `reader`.
}

/// The open demuxer + decoder + converter and the state a session's worth of
/// reads/seeks mutates. One per producer run; never rebuilt (seeks are
/// in-place — the whole point of the FFmpeg-native internals).
struct Reader {
    input: FfInput<'static>,
    facts: VideoFacts,
    decoder: ff::decoder::Video,
    conv: FrameConverter,
    packet: ff::Packet,
    /// `send_eof` delivered — the decoder is draining its B-frame tail.
    eof_sent: bool,
    /// EndOfStream published; only a SeekTo revives reads.
    parked: bool,
    /// First-published-frame PTS (stream units) — the session-relative zero.
    origin: Option<i64>,
    /// Deterministic PTS synthesis for frames with no timestamp.
    last_ts: i64,
    /// Per-frame color, cached once the first frame resolves it.
    color: Option<VideoColorInfo>,
    /// Hardware decode device (VideoToolbox/VAAPI), kept alive for the decoder's
    /// lifetime; `None` when decoding in software. Declared after `decoder` so
    /// it drops after it (both hold refcounted device refs — order-independent —
    /// this is for tidiness).
    _hw: Option<super::hw::HwSession>,
}

impl Reader {
    fn open(input: &VideoInput, fit: Option<FitBox>) -> Result<Reader, String> {
        // No borrowed cancel flag on the producer path (the channel + the
        // per-op watchdog are its control levers), so the input is 'static.
        let mut opened = FfInput::open(input, None)?;
        let facts = video_facts(opened.ctx())?;
        if facts.width == 0 || facts.height == 0 {
            return Err("video has no frames".into());
        }
        // Build the decoder context from stream parameters, then try to attach
        // a hardware decode device (VideoToolbox/VAAPI) before opening. The
        // parameters are copied into the context, so no borrow of `opened`
        // outlives this block.
        let (mut ctx, codec_id) = {
            let stream = opened
                .ctx()
                .streams()
                .find(|s| s.index() == facts.index)
                .ok_or("video stream vanished")?;
            let params = stream.parameters();
            let id = params.id();
            let ctx = ff::codec::context::Context::from_parameters(params)
                .map_err(|e| format!("FFmpeg decoder: {e}"))?;
            (ctx, id)
        };
        let mut hw = super::hw::try_enable(&mut ctx, codec_id);
        let decoder = match ctx.decoder().video() {
            Ok(d) => d,
            // A rare open failure with hardware attached: drop the device and
            // retry pure software (libavcodec normally degrades internally, so
            // this is belt-and-suspenders rather than the expected path).
            Err(_) if hw.is_some() => {
                hw = None;
                let stream = opened
                    .ctx()
                    .streams()
                    .find(|s| s.index() == facts.index)
                    .ok_or("video stream vanished")?;
                ff::codec::context::Context::from_parameters(stream.parameters())
                    .and_then(|c| c.decoder().video())
                    .map_err(|e| format!("FFmpeg decoder: {e}"))?
            }
            Err(e) => return Err(format!("FFmpeg decoder: {e}")),
        };
        // Output geometry: fit the SAR-corrected display dims, mapped back to
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
        let conv = FrameConverter::new(
            (facts.width, facts.height),
            pre_rot,
            facts.rotation,
            &decoder,
        );
        Ok(Reader {
            input: opened,
            facts,
            decoder,
            conv,
            packet: ff::Packet::empty(),
            eof_sent: false,
            parked: false,
            origin: None,
            last_ts: 0,
            color: None,
            _hw: hw,
        })
    }

    /// Decode the next frame: `Ok(Some((pts_units, rgba)))`, `Ok(None)` at
    /// end-of-stream (B-frame tail drained), `Err` on unrecoverable input.
    /// Bounded: the watchdog covers blocking libav calls; the packet/error
    /// budgets cover corrupt streams that never yield a frame.
    fn next_frame(&mut self) -> Result<Option<(i64, Vec<u8>)>, String> {
        self.input.set_op_deadline(Some(OP_DEADLINE));
        let result = self.next_frame_inner();
        self.input.set_op_deadline(None);
        result
    }

    fn next_frame_inner(&mut self) -> Result<Option<(i64, Vec<u8>)>, String> {
        let mut decoded = ff::frame::Video::empty();
        let mut fed = 0usize;
        let mut bad = 0usize;
        loop {
            if self.decoder.receive_frame(&mut decoded).is_ok() {
                let ts = self.stamp(decoded.timestamp());
                // A hardware decode leaves the frame on the GPU (VideoToolbox /
                // VAAPI surface) — pull it to a CPU NV12/P010 frame before
                // conversion; software frames pass straight through.
                let (rgba, _, _) = match super::hw::transfer_if_hw(&decoded)? {
                    Some(sw) => self.conv.convert(&sw)?,
                    None => self.conv.convert(&decoded)?,
                };
                return Ok(Some((ts, rgba)));
            }
            if self.eof_sent {
                return Ok(None); // tail fully drained
            }
            if fed >= MAX_PACKETS_PER_FRAME {
                return Err("video stream produced no frame (corrupt input?)".into());
            }
            match self.packet.read(self.input.ctx()) {
                Ok(()) => {
                    if self.packet.stream() == self.facts.index {
                        fed += 1;
                        if self.decoder.send_packet(&self.packet).is_err() {
                            // A damaged packet is skipped, not fatal — but a
                            // stream that's ALL damage is.
                            bad += 1;
                            if bad >= MAX_BAD_PACKETS {
                                return Err("video stream is unreadable (corrupt input)".into());
                            }
                        }
                    }
                }
                Err(ff::Error::Eof) => {
                    let _ = self.decoder.send_eof();
                    self.eof_sent = true;
                }
                Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => {}
                Err(e) => return Err(format!("FFmpeg read: {e}")),
            }
        }
    }

    /// A frame's PTS in stream units, synthesized deterministically (previous +
    /// one frame interval) when the container carries none.
    fn stamp(&mut self, ts: Option<i64>) -> i64 {
        let v = ts.unwrap_or_else(|| {
            let step = if self.facts.fps > 0.0 {
                self.facts
                    .duration_to_pts(Duration::from_secs_f64(1.0 / self.facts.fps))
                    .max(1)
            } else {
                1
            };
            self.last_ts + step
        });
        self.last_ts = v;
        v
    }

    /// In-place seek: demuxer to a keyframe ≤ `target`, decoder flushed, EOS
    /// state cleared. Returns the target in stream units (the landing bar).
    fn seek(&mut self, target: Duration) -> Result<i64, String> {
        let base = self.origin.or(self.facts.start_time).unwrap_or(0);
        let target_units = base.saturating_add(self.facts.duration_to_pts(target));
        self.input.set_op_deadline(Some(OP_DEADLINE));
        let rc = unsafe {
            ff::ffi::avformat_seek_file(
                self.input.ctx().as_mut_ptr(),
                self.facts.index as i32,
                i64::MIN,
                target_units,
                target_units,
                0,
            )
        };
        let rc = if rc < 0 {
            // Nothing at/before the target (e.g. a start-offset edge): allow
            // landing past it — the forward decode publishes the first frame.
            unsafe {
                ff::ffi::avformat_seek_file(
                    self.input.ctx().as_mut_ptr(),
                    self.facts.index as i32,
                    i64::MIN,
                    target_units,
                    i64::MAX,
                    0,
                )
            }
        } else {
            rc
        };
        self.input.set_op_deadline(None);
        if rc < 0 {
            return Err(format!("video seek failed: {}", ff::Error::from(rc)));
        }
        self.decoder.flush();
        self.eof_sent = false;
        self.parked = false;
        Ok(target_units)
    }

    /// Assemble the protocol frame for a converted pixel buffer at `ts`.
    fn make_frame(
        &mut self,
        session_id: VideoSessionId,
        gen: SeekGeneration,
        ts: i64,
        pixels: Vec<u8>,
    ) -> VideoFrame {
        let (w, h) = self.conv.display_dims();
        let format = self.conv.output_format();
        let color = match format {
            // fp16 scene-linear scRGB (plan §9): the transform is passthrough
            // (linearization + primaries already applied); `peak` rides the
            // running max so SDR presentation tone-maps like HDR stills do.
            PixelFormat::Rgba16F => {
                let sc = self.conv.source_color();
                VideoColorInfo {
                    transform: crate::ColorTransform::srgb(),
                    cicp: Some((sc.primaries, sc.transfer, sc.matrix)),
                    full_range: true,
                    yuv_matrix: super::color::yuv_matrix(sc.matrix),
                    peak: self.conv.peak(),
                }
            }
            _ => self
                .color
                .get_or_insert_with(|| {
                    super::color::video_color_info_rgb(&self.conv.source_color())
                })
                .clone(),
        };
        let origin = self.origin.unwrap_or(0);
        VideoFrame {
            session_id,
            seek_generation: gen,
            pts: self.facts.pts_to_duration(ts.saturating_sub(origin)),
            width: w,
            height: h,
            format,
            pixels,
            color,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Instant;

    const SID: VideoSessionId = VideoSessionId(42);
    const GEN: SeekGeneration = SeekGeneration::FIRST;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    fn spawn(path: std::path::PathBuf) -> (Sender<VideoProducerMsg>, Receiver<VideoProducerEvent>) {
        spawn_input(VideoInput::Path(path))
    }

    fn spawn_input(input: VideoInput) -> (Sender<VideoProducerMsg>, Receiver<VideoProducerEvent>) {
        let (events_tx, events_rx) = channel();
        let (msgs_tx, msgs_rx) = channel();
        std::thread::spawn(move || {
            run_ff_video_producer(&input, None, SID, GEN, events_tx, msgs_rx);
        });
        (msgs_tx, events_rx)
    }

    /// The MF producer's flagship protocol test, ported verbatim: Opened facts,
    /// credit-driven frames with normalized monotonic PTS, EOS at the end.
    #[test]
    fn producer_streams_the_fixture_on_credits_and_reports_eos() {
        let (msgs, events) = spawn(fixture("black_then_color.mp4"));
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened")
        {
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
        assert!(
            (25..=35).contains(&frames),
            "expected ~30 fixture frames, got {frames}"
        );
    }

    /// In-place seek: the landing frame carries the new generation with
    /// pts ≥ the target; a superseding SeekTo means the older landing never
    /// publishes.
    #[test]
    fn seek_lands_at_the_target_with_the_new_generation() {
        let (msgs, events) = spawn(fixture("black_then_color.mp4"));
        let _ = events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened");
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events.recv_timeout(Duration::from_secs(10)).expect("frame") {
            VideoProducerEvent::Frame(f) => assert_eq!(f.pts, Duration::ZERO),
            other => panic!("expected a frame, got {other:?}"),
        }

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

    /// After EOS the producer parks (demuxer open) so a replay/rewind SeekTo
    /// works on the same session — in place, no reopen.
    #[test]
    fn seek_after_eos_replays_from_the_target() {
        let (msgs, events) = spawn(fixture("black_then_color.mp4"));
        let _ = events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened");
        loop {
            msgs.send(VideoProducerMsg::Credit).unwrap();
            match events.recv_timeout(Duration::from_secs(10)).expect("event") {
                VideoProducerEvent::Frame(_) => {}
                VideoProducerEvent::EndOfStream { .. } => break,
                other => panic!("unexpected {other:?}"),
            }
        }
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

    /// Drive a fixture to EOS, returning (frames, first-frame check ran).
    fn stream_to_eos(name: &str, expect_dims: (u32, u32)) -> usize {
        let (msgs, events) = spawn(fixture(name));
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened")
        {
            VideoProducerEvent::Opened { width, height, .. } => {
                assert_eq!((width, height), expect_dims, "{name}: display dims");
            }
            other => panic!("{name}: expected Opened, got {other:?}"),
        }
        let mut frames = 0usize;
        loop {
            msgs.send(VideoProducerMsg::Credit).unwrap();
            match events.recv_timeout(Duration::from_secs(10)).expect("event") {
                VideoProducerEvent::Frame(f) => {
                    assert!(f.is_well_formed(), "{name}: malformed frame");
                    assert_eq!((f.width, f.height), expect_dims, "{name}: frame dims");
                    frames += 1;
                }
                VideoProducerEvent::EndOfStream { .. } => break,
                other => panic!("{name}: unexpected {other:?}"),
            }
        }
        frames
    }

    /// The macOS-fallback flagship formats — VP9 and VP8 in WebM (AVFoundation
    /// can't demux the container or decode the codecs) — stream end-to-end
    /// through the FFmpeg producer.
    #[test]
    fn vp9_webm_streams_to_eos() {
        let frames = stream_to_eos("color_vp9.webm", (64, 64));
        assert!((25..=35).contains(&frames), "~30 VP9 frames, got {frames}");
    }

    #[test]
    fn vp8_webm_streams_to_eos() {
        let frames = stream_to_eos("color_vp8.webm", (64, 64));
        assert!((25..=35).contains(&frames), "~30 VP8 frames, got {frames}");
    }

    /// The "MKV commonly wraps H.264" case — the container AVFoundation
    /// refuses around a codec it supports.
    #[test]
    fn h264_mkv_streams_to_eos() {
        let frames = stream_to_eos("black_then_color.mkv", (64, 64));
        assert!((25..=35).contains(&frames), "~30 MKV frames, got {frames}");
    }

    /// A 90° display-matrix clip (portrait phone video) emits upright frames:
    /// the 64×32 coded stream presents as 32×64.
    #[test]
    fn rotated_clip_emits_upright_frames() {
        let frames = stream_to_eos("rotated90.mp4", (32, 64));
        assert!(frames >= 25, "rotated clip decoded {frames} frames");
    }

    /// The fp16 HDR contract (plan §9, owner decision #1): a PQ/BT.2020 clip
    /// emits scene-linear Rgba16F frames — never tone-mapped RGBA8 — with a
    /// format-aware credit size and a real peak for the SDR tone-map.
    #[test]
    fn hdr_pq_clip_emits_fp16_scene_linear_frames() {
        let (msgs, events) = spawn(fixture("hdr_pq.mp4"));
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened")
        {
            VideoProducerEvent::Opened {
                width,
                height,
                frame_bytes,
                ..
            } => {
                assert_eq!((width, height), (64, 64));
                assert_eq!(frame_bytes, 64 * 64 * 8, "fp16 charges 8 bytes/px");
            }
            other => panic!("expected Opened, got {other:?}"),
        }
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events.recv_timeout(Duration::from_secs(10)).expect("frame") {
            VideoProducerEvent::Frame(f) => {
                assert_eq!(f.format, PixelFormat::Rgba16F);
                assert!(f.is_well_formed(), "fp16 geometry/buffer contract");
                assert!(
                    !f.color.transform.enabled,
                    "scene-linear scRGB is a shader passthrough"
                );
                assert!(f.color.peak >= 1.0, "peak {}", f.color.peak);
                assert_eq!(f.color.cicp, Some((9, 16, 9)), "BT.2020 PQ kept verbatim");
                // Spot-check a pixel decodes to finite positive linear light.
                let ch = half::f16::from_le_bytes([f.pixels[0], f.pixels[1]]).to_f32();
                assert!(ch.is_finite() && ch >= 0.0, "linear R = {ch}");
            }
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn stop_interrupts_a_credit_starved_producer_quickly() {
        let (msgs, events) = spawn(fixture("black_then_color.mp4"));
        let _ = events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened");
        let t0 = Instant::now();
        msgs.send(VideoProducerMsg::Stop).unwrap();
        while events.recv_timeout(Duration::from_secs(10)).is_ok() {}
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "stop must not hang behind backpressure"
        );
    }

    /// `Opened` reports the audio track's presence honestly (plan §7) — the
    /// signal that starts the shell audio sink; silent clips never get one.
    #[test]
    fn opened_reports_audio_presence_honestly() {
        let (_msgs, events) = spawn(fixture("color_with_tone.mp4"));
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened")
        {
            VideoProducerEvent::Opened { has_audio, .. } => {
                assert!(has_audio, "the tone fixture has an AAC track");
            }
            other => panic!("expected Opened, got {other:?}"),
        }
        let (_msgs, events) = spawn(fixture("black_then_color.mp4"));
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened")
        {
            VideoProducerEvent::Opened { has_audio, .. } => {
                assert!(!has_audio, "the silent fixture reports none");
            }
            other => panic!("expected Opened, got {other:?}"),
        }
    }

    #[test]
    fn a_bad_path_fails_cleanly() {
        let (_msgs, events) = spawn(std::path::PathBuf::from("/nope/missing.mp4"));
        match events.recv_timeout(Duration::from_secs(10)).expect("event") {
            VideoProducerEvent::Failed { session_id, .. } => assert_eq!(session_id, SID),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// Archive playback's core claim, ported from the MF suite: stream, seek
    /// (an in-place demuxer seek over the SAME shared bytes — no path
    /// anywhere), from an in-RAM container exactly like a file.
    #[test]
    fn producer_streams_and_seeks_from_in_ram_bytes() {
        let data =
            std::sync::Arc::new(std::fs::read(fixture("black_then_color.mp4")).expect("bytes"));
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
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events.recv_timeout(Duration::from_secs(10)).expect("frame") {
            VideoProducerEvent::Frame(f) => {
                assert!(f.is_well_formed());
                assert_eq!(f.pts, Duration::ZERO);
            }
            other => panic!("expected a frame, got {other:?}"),
        }
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

    /// Hostile in-RAM bytes fail with a structured error, never a hang/crash.
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

    /// A truncated container either plays what it has or fails — bounded
    /// either way (the watchdog + packet budgets, plan §5/§6).
    #[test]
    fn truncated_bytes_are_bounded() {
        let full = std::fs::read(fixture("black_then_color.mp4")).expect("bytes");
        let cut = std::sync::Arc::new(full[..full.len() / 3].to_vec());
        let (msgs, events) = spawn_input(VideoInput::Bytes {
            data: cut,
            name: "cut.mp4".into(),
        });
        let t0 = Instant::now();
        // Drive it: whatever arrives, keep feeding credits until it terminates.
        let mut done = false;
        while !done && t0.elapsed() < Duration::from_secs(60) {
            let _ = msgs.send(VideoProducerMsg::Credit);
            match events.recv_timeout(Duration::from_secs(30)) {
                Ok(VideoProducerEvent::Frame(_)) | Ok(VideoProducerEvent::Opened { .. }) => {}
                Ok(VideoProducerEvent::EndOfStream { .. })
                | Ok(VideoProducerEvent::Failed { .. })
                | Err(_) => done = true,
            }
        }
        assert!(done, "truncated input must terminate, not spin");
    }
}
