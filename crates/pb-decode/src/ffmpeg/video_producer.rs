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

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use ffmpeg_next as ff;

use super::convert::FrameConverter;
use super::io::FfInput;
use super::probe::{fit_dims, video_facts, VideoFacts};
use crate::video::{
    SeekGeneration, VideoColorInfo, VideoFrame, VideoInput, VideoProducerEvent, VideoProducerMsg,
    VideoSessionId,
};
use crate::video_producer_loop::{Opened, VideoProducerBackend};
use crate::{FitBox, PixelFormat};

/// Watchdog budget for one read/decode burst (one credited frame). Local
/// files/RAM resolve in milliseconds; only hostile input hits this.
const OP_DEADLINE: Duration = Duration::from_secs(10);

/// How far ahead we'll **decode forward** from the current position instead of
/// re-seeking to a keyframe. A short *forward* hop (arrow-key ±2 s) lands far
/// faster by just decoding the few frames to the target than by seeking back to
/// the previous keyframe and running up the whole GOP — which on long-GOP 4K/HDR
/// can be many seconds of frames. Beyond this a keyframe seek is the better bet.
const FORWARD_DECODE_MAX: Duration = Duration::from_secs(4);

/// Temporary perf diagnostics (task #84 follow-up): `PB_VIDEO_DIAG=1` prints the
/// decode backend (VideoToolbox vs software), the decode resolution, and per-seek
/// timing to stderr so we can measure the 4K-HDR seek stall instead of guessing.
fn diag() -> bool {
    std::env::var_os("PB_VIDEO_DIAG").is_some()
}
/// Packets fed without producing one frame before declaring the input stuck.
const MAX_PACKETS_PER_FRAME: usize = 4096;
/// Consecutive rejected packets (decoder said no) before giving up.
const MAX_BAD_PACKETS: usize = 512;

/// Run the producer to completion on the **current thread** (the app spawns a
/// dedicated thread for it — never the event loop). Returns when the stream
/// ends and the session stops it, the session is dropped, or decoding fails.
/// The FFmpeg mirror of `run_video_producer` (Windows MF): both now drive the
/// **one** shared credit/seek loop ([`crate::video_producer_loop::run`]) behind
/// the [`VideoProducerBackend`] seam (task #130) — this wrapper only opens the
/// backend (on this producer thread, since libav handles are `!Send`) and hands
/// it off. `input` is a filesystem path or an archive entry's in-RAM bytes.
#[allow(clippy::too_many_arguments)]
pub fn run_ff_video_producer(
    input: &VideoInput,
    fit: Option<FitBox>,
    session_id: VideoSessionId,
    generation: SeekGeneration,
    events: Sender<VideoProducerEvent>,
    msgs: Receiver<VideoProducerMsg>,
    cancel: Arc<AtomicBool>,
    options: crate::VideoProducerOptions,
) {
    // The session sets `cancel` on stop/teardown; the interrupt callback then
    // aborts a blocking read *inside* libav (plan 1F) — so a stuck network read
    // retires this thread promptly instead of lingering on the per-op watchdog.
    match Reader::open(input, fit, cancel, options) {
        Ok((backend, opened)) => {
            crate::video_producer_loop::run(backend, opened, session_id, generation, events, msgs)
        }
        Err(error) => {
            let _ = events.send(VideoProducerEvent::Failed { session_id, error });
        }
    }
}

/// The FFmpeg planar-prime state ([`Reader::initial`]). FFmpeg must decode the
/// first frame *before* it can pick NV12/P010/fp16 (the format isn't reliably
/// known until the actual decoded pixel format after HW transfer — Codex P0), so
/// when the planar path is a candidate `open` primes that frame and stashes it
/// here to serve on the first credit. This keeps the shared loop unaware of
/// pixel-format probing: `read_frame` serves the primed frame first, then decodes
/// normally (task #130, plan §5.1). A seek before the first credit invalidates it
/// (`invalidate_primed` / `seek` reset to `NeedRead`).
enum InitialState {
    /// No primed frame — decode normally. The state for a non-planar clip and for
    /// every read after the primed frame is served or invalidated.
    NeedRead,
    /// The negotiation-primed first frame, served on the first credit.
    Ready { ts: i64, pixels: Vec<u8> },
    /// The prime hit end-of-stream before any frame (a zero-frame clip): the first
    /// `read_frame` reports EOS. Deferred to the first credit rather than emitted
    /// eagerly after `Opened`, so the shared loop needs no empty-clip special case.
    Eos,
}

/// The open demuxer + decoder + converter and the state a session's worth of
/// reads/seeks mutates. One per producer run; never rebuilt (seeks are
/// in-place — the whole point of the FFmpeg-native internals).
struct Reader {
    input: FfInput<'static>,
    facts: VideoFacts,
    decoder: ff::decoder::Video,
    conv: FrameConverter,
    /// HDR tone-map peak (scene-linear scRGB) from container metadata, resolved
    /// once at open (task #91 Phase 2 §2D). `≥ 1.0`; drives HDR frames' `peak`.
    hdr_peak: f32,
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
    /// The negotiation-primed first frame, if any (task #91 Phase 2 / #130 §5.1) —
    /// served on the first credit, invalidated by a seek. See [`InitialState`].
    initial: InitialState,
    /// Hardware decode device (VideoToolbox/VAAPI), kept alive for the decoder's
    /// lifetime; `None` when decoding in software. Declared after `decoder` so
    /// it drops after it (both hold refcounted device refs — order-independent —
    /// this is for tidiness).
    _hw: Option<super::hw::HwSession>,
}

impl Reader {
    fn open(
        input: &VideoInput,
        fit: Option<FitBox>,
        cancel: Arc<AtomicBool>,
        options: crate::VideoProducerOptions,
    ) -> Result<(Reader, Opened), String> {
        let mut opened = FfInput::open(input, None)?;
        // Arm the interrupt cancel flag (plan 1F): the session flips this shared
        // `Arc` on stop/teardown, and the interrupt callback aborts a blocking read
        // inside libav — so a stuck network read retires this thread promptly
        // instead of pinning it to the per-op watchdog.
        opened.set_cancel(cancel);
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
            options.planar,
            options.supports_p010,
        );
        // HDR tone-map peak from container metadata (task #91 Phase 2 §2D): the
        // MaxCLL / mastering-display max-luminance, resolved once at open — this
        // replaces the per-frame running-max pixel scan (R11) that lived in the CPU
        // convert. Static per clip; harmless for SDR (SDR frames present at peak 1).
        let hdr_peak = {
            let (cll, mastering) = opened
                .ctx()
                .streams()
                .find(|s| s.index() == facts.index)
                .map(|s| super::color::hdr_metadata_nits(&s))
                .unwrap_or((None, None));
            super::color::resolve_hdr_peak(cll, mastering)
        };
        let mut reader = Reader {
            input: opened,
            facts,
            decoder,
            conv,
            hdr_peak,
            packet: ff::Packet::empty(),
            eof_sent: false,
            parked: false,
            origin: None,
            last_ts: 0,
            color: None,
            initial: InitialState::NeedRead,
            _hw: hw,
        };
        // Negotiate the output format (task #91 Phase 2, Codex P0): the planar-vs-RGBA
        // decision needs the ACTUAL first decoded frame (pixel format after HW
        // transfer), so — only when the planar path is even a candidate — decode +
        // convert the first frame now, retain it in `initial`, and serve it on the
        // first credit. When planar is off, `output_format` is known at open
        // (RGBA/fp16), so this is skipped and `Opened` reports it directly (no added
        // latency). Runs here, on the producer thread, so the shared loop never sees
        // pixel-format probing (task #130, plan §5.1).
        if options.planar {
            match reader.next_frame() {
                Ok(Some((ts, px))) => {
                    reader.origin.get_or_insert(ts);
                    reader.initial = InitialState::Ready { ts, pixels: px };
                }
                // A zero-frame clip: report EOS on the first `read_frame` rather than
                // eagerly (deferred so the shared loop needs no empty-clip case).
                Ok(None) => reader.initial = InitialState::Eos,
                Err(e) => return Err(e),
            }
        }
        let (out_w, out_h) = reader.conv.display_dims();
        if diag() {
            let fit_desc = match fit {
                Some(f) => format!("fit {}x{}", f.max_width, f.max_height),
                None => "NATIVE".to_string(),
            };
            eprintln!(
                "[pb-video] open codec={} coded={}x{} display={}x{} decode={} hwaccel={} out_fmt={:?} rot={}",
                reader.facts.codec,
                reader.facts.width,
                reader.facts.height,
                out_w,
                out_h,
                fit_desc,
                if reader._hw.is_some() { "VideoToolbox" } else { "SOFTWARE" },
                reader.conv.output_format(),
                reader.facts.rotation,
            );
        }
        let opened_info = Opened {
            duration: reader.facts.duration,
            width: out_w,
            height: out_h,
            // Honest track presence (plan §7 — the muted interim ended when the
            // FFmpeg audio decoder + platform sinks landed): the session starts the
            // shell audio player for real tracks; a shell with no sink reports a
            // Failed clock immediately and playback degrades to silent.
            has_audio: reader.facts.has_audio,
            // Format-aware (task 79.10 / #91): an fp16 HDR clip charges 8 bytes/px, a
            // P010 clip 3 bytes/px — the negotiated (post-prime) format, so the
            // session's byte budget matches the frames it will receive.
            frame_bytes: reader.conv.output_format().frame_bytes(out_w, out_h) as u64,
        };
        Ok((reader, opened_info))
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
        match self.decode_next_raw_inner()? {
            Some((ts, frame)) => Ok(Some((ts, self.convert_frame(&frame)?))),
            None => Ok(None),
        }
    }

    /// Decode the next frame **without converting it**: returns its stamped PTS
    /// and the raw decoded frame (a hardware surface when hwaccel is on). The seek
    /// run-up uses this to advance to the target without paying the readback +
    /// downscale + tone-map (`convert_frame`) on frames it will discard — the
    /// dominant cost of a long-GOP 4K/HDR seek (measured ~25 ms/frame). Bounded by
    /// the same watchdog as [`Reader::next_frame`].
    fn decode_next_raw(&mut self) -> Result<Option<(i64, ff::frame::Video)>, String> {
        self.input.set_op_deadline(Some(OP_DEADLINE));
        let result = self.decode_next_raw_inner();
        self.input.set_op_deadline(None);
        result
    }

    /// Readback (if the frame is a hardware surface) + color-convert to the output
    /// RGBA8/fp16 buffer. Split out of the decode so the seek run-up can skip it.
    fn convert_frame(&mut self, frame: &ff::frame::Video) -> Result<Vec<u8>, String> {
        // A hardware decode leaves the frame on the GPU (VideoToolbox / VAAPI
        // surface) — pull it to a CPU NV12/P010 frame before conversion; software
        // frames pass straight through.
        let (rgba, _, _) = match super::hw::transfer_if_hw(frame)? {
            Some(sw) => self.conv.convert(&sw)?,
            None => self.conv.convert(frame)?,
        };
        Ok(rgba)
    }

    fn decode_next_raw_inner(&mut self) -> Result<Option<(i64, ff::frame::Video)>, String> {
        let mut decoded = ff::frame::Video::empty();
        let mut fed = 0usize;
        let mut bad = 0usize;
        loop {
            if self.decoder.receive_frame(&mut decoded).is_ok() {
                let ts = self.stamp(decoded.timestamp());
                return Ok(Some((ts, decoded)));
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

    /// The forward-decode budget ([`FORWARD_DECODE_MAX`]) in stream units.
    fn forward_decode_max_units(&self) -> i64 {
        self.facts.duration_to_pts(FORWARD_DECODE_MAX)
    }

    /// In-place keyframe seek to `target_units`: demuxer to a keyframe ≤ target,
    /// decoder flushed, EOS/parked state cleared.
    fn seek_to_keyframe(&mut self, target_units: i64) -> Result<(), String> {
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
        Ok(())
    }
}

/// The FFmpeg backend on the shared credit/seek loop (task #130, plan §4). The
/// `Reader`'s free functions and fields become the trait's ~9-operation seam; the
/// loop machinery lives once in [`crate::video_producer_loop`].
impl VideoProducerBackend for Reader {
    /// An owned libav frame — a hardware surface when hwaccel is on, pulled to CPU
    /// in [`convert`](Self::convert).
    type Raw = ff::frame::Video;

    fn read_frame(&mut self) -> Result<Option<(i64, Vec<u8>)>, String> {
        // Serve the negotiation-primed first frame ahead of any fresh decode (task
        // #91 Phase 2); a seek already reset this to `NeedRead`.
        match std::mem::replace(&mut self.initial, InitialState::NeedRead) {
            InitialState::Ready { ts, pixels } => return Ok(Some((ts, pixels))),
            InitialState::Eos => {
                self.parked = true;
                return Ok(None);
            }
            InitialState::NeedRead => {}
        }
        match self.next_frame()? {
            Some(frame) => Ok(Some(frame)),
            None => {
                // Park (don't exit): a later SeekTo replays/rewinds in place.
                self.parked = true;
                Ok(None)
            }
        }
    }

    fn decode_raw(&mut self) -> Result<Option<(i64, ff::frame::Video)>, String> {
        match self.decode_next_raw()? {
            Some(frame) => Ok(Some(frame)),
            None => {
                self.parked = true;
                Ok(None)
            }
        }
    }

    fn convert(&mut self, raw: ff::frame::Video) -> Result<Vec<u8>, String> {
        self.convert_frame(&raw)
    }

    /// A seek target `Duration` in this stream's PTS units (the landing bar),
    /// relative to the same origin/start-time base `last_ts` uses — so the two are
    /// directly comparable for the forward-hop decision.
    fn target_units(&self, target: Duration) -> i64 {
        let base = self.origin.or(self.facts.start_time).unwrap_or(0);
        base.saturating_add(self.facts.duration_to_pts(target))
    }

    /// Whether a seek to `target_units` should **decode forward** from the current
    /// position instead of seeking to a keyframe: only a *forward* hop within the
    /// budget, and not while parked/drained (where there's nothing to decode
    /// forward from). Backward and far seeks return `false` → keyframe seek.
    fn can_decode_forward(&self, target_units: i64) -> bool {
        !self.parked
            && !self.eof_sent
            && target_units > self.last_ts
            && target_units - self.last_ts <= self.forward_decode_max_units()
    }

    fn seek(&mut self, target_units: i64) -> Result<(), String> {
        // `seek_to_keyframe` clears EOS/parked; also drop any primed frame (a seek
        // before the first credit invalidates it — plan §5.1).
        self.initial = InitialState::NeedRead;
        self.seek_to_keyframe(target_units)
    }

    fn invalidate_primed(&mut self) {
        self.initial = InitialState::NeedRead;
    }

    fn is_parked(&self) -> bool {
        self.parked
    }

    fn anchor_origin_seek(&mut self, target_units: i64, target: Duration) {
        self.origin
            .get_or_insert(target_units - self.facts.duration_to_pts(target));
    }

    fn anchor_origin_seq(&mut self, ts: i64) {
        self.origin.get_or_insert(ts);
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
            // Planar NV12/P010 (task #91 Phase 2): pixels are raw YUV — the renderer
            // applies matrix + range + transfer + primaries in-shader exactly once.
            PixelFormat::Nv12 | PixelFormat::P010 => {
                super::color::video_color_info_planar(&self.conv.source_color(), self.hdr_peak)
            }
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
                    // fp16 path: pixels are already scene-linear scRGB; transfer inert.
                    transfer: crate::VideoTransfer::SrgbLike,
                    // Metadata-driven peak (task #91 Phase 2 §2D) — no longer the
                    // per-frame running-max (`conv.peak()`, R11).
                    peak: self.hdr_peak,
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

    fn seek_diag(
        &self,
        forward: bool,
        runup: u32,
        demux: Duration,
        total: Duration,
        target: Duration,
    ) {
        if diag() {
            eprintln!(
                "[pb-video] seek target={:.2}s mode={} demux={}ms runup={} frames total={}ms",
                target.as_secs_f64(),
                if forward { "forward" } else { "keyframe" },
                demux.as_millis(),
                runup,
                total.as_millis(),
            );
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
        spawn_input_opts(input, crate::VideoProducerOptions::default())
    }

    fn spawn_input_opts(
        input: VideoInput,
        options: crate::VideoProducerOptions,
    ) -> (Sender<VideoProducerMsg>, Receiver<VideoProducerEvent>) {
        let (events_tx, events_rx) = channel();
        let (msgs_tx, msgs_rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        std::thread::spawn(move || {
            run_ff_video_producer(&input, None, SID, GEN, events_tx, msgs_rx, cancel, options);
        });
        (msgs_tx, events_rx)
    }

    /// Drain frames to ~30 s of media at the given producer options and report
    /// `media/wall` (real-time multiple) + fps. The 0D margin metric (task #91:
    /// >~1.5× = comfortable headroom). Returns `(real_time_multiple, first_format)`.
    fn throughput_trace(
        input: VideoInput,
        options: crate::VideoProducerOptions,
        label: &str,
    ) -> (f64, Option<PixelFormat>) {
        let t_open = Instant::now();
        let (msgs, events) = spawn_input_opts(input, options);
        match events
            .recv_timeout(Duration::from_secs(20))
            .expect("opened")
        {
            VideoProducerEvent::Opened { width, height, .. } => {
                eprintln!(
                    "[0d {label}] open+probe {:?} — {width}x{height}",
                    t_open.elapsed()
                );
            }
            other => panic!("expected Opened, got {other:?}"),
        }
        const PIPELINE: usize = 6; // credits outstanding (mimics the session's ring)
        const TARGET: Duration = Duration::from_secs(30);
        for _ in 0..PIPELINE {
            msgs.send(VideoProducerMsg::Credit).unwrap();
        }
        let t = Instant::now();
        let (mut frames, mut last_pts, mut fmt) = (0u64, Duration::ZERO, None);
        loop {
            match events.recv_timeout(Duration::from_secs(20)) {
                Ok(VideoProducerEvent::Frame(f)) => {
                    frames += 1;
                    last_pts = f.pts;
                    fmt.get_or_insert(f.format);
                    let _ = msgs.send(VideoProducerMsg::Credit); // refill the ring
                    if f.pts >= TARGET {
                        break;
                    }
                }
                Ok(VideoProducerEvent::EndOfStream { .. }) => break,
                Ok(_) => {}
                Err(_) => {
                    eprintln!("[0d {label}] STALLED after {frames} frames / {last_pts:?}");
                    break;
                }
            }
        }
        let wall = t.elapsed().as_secs_f64();
        let media = last_pts.as_secs_f64();
        let rt = media / wall.max(1e-6);
        eprintln!(
            "[0d {label}] {frames} frames = {media:.1}s media in {wall:.2}s wall → {rt:.2}x real-time, {:.1} fps ({:?})",
            frames as f64 / wall.max(1e-6),
            fmt,
        );
        let _ = msgs.send(VideoProducerMsg::Stop);
        (rt, fmt)
    }

    /// 0D headless margin trace (plan §6.0D / task #91 Phase 2), **A/B**: the
    /// RGBA/fp16 fallback (pre-Phase-2, CPU convert + R8 threads) vs the planar GPU
    /// color path (P010/NV12, the CPU convert removed). Prints both real-time
    /// multiples so the Phase-2 decode-headroom win is directly visible. Point
    /// `PB_NET_TEST_MKV` at a large clip; run:
    /// `PB_NET_TEST_MKV=/path cargo test -p pb-decode --features ffvideo \
    ///   net_decode_throughput -- --nocapture --ignored`
    #[test]
    #[ignore = "needs PB_NET_TEST_MKV pointing at a large (network) container"]
    fn net_decode_throughput() {
        let Ok(path) = std::env::var("PB_NET_TEST_MKV") else {
            eprintln!("skipping: set PB_NET_TEST_MKV to a large (network) container");
            return;
        };
        let mk = || VideoInput::Path(std::path::PathBuf::from(&path));
        let (baseline, _) = throughput_trace(mk(), opts(false, false), "RGBA/fp16");
        let (planar, fmt) = throughput_trace(mk(), opts(true, true), "planar");
        eprintln!(
            "[0d] Phase-2 win: {baseline:.2}x → {planar:.2}x real-time ({:.2}× faster) on {fmt:?}",
            planar / baseline.max(1e-6)
        );
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

    /// The shared fp16 HDR contract (plan §9, owner decision #1): a PQ/BT.2020
    /// clip emits scene-linear Rgba16F frames — never tone-mapped RGBA8 — with a
    /// format-aware credit size and a real peak for the SDR tone-map.
    fn assert_hdr_pq_contract(name: &str) {
        let (msgs, events) = spawn(fixture(name));
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
                assert_eq!((width, height), (64, 64), "{name}");
                assert_eq!(frame_bytes, 64 * 64 * 8, "{name}: fp16 charges 8 bytes/px");
            }
            other => panic!("{name}: expected Opened, got {other:?}"),
        }
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events.recv_timeout(Duration::from_secs(10)).expect("frame") {
            VideoProducerEvent::Frame(f) => {
                assert_eq!(f.format, PixelFormat::Rgba16F, "{name}");
                assert!(f.is_well_formed(), "{name}: fp16 geometry/buffer contract");
                assert!(
                    !f.color.transform.enabled,
                    "{name}: scene-linear scRGB is a shader passthrough"
                );
                assert!(f.color.peak >= 1.0, "{name}: peak {}", f.color.peak);
                assert_eq!(
                    f.color.cicp,
                    Some((9, 16, 9)),
                    "{name}: BT.2020 PQ kept verbatim"
                );
                // Spot-check a pixel decodes to finite positive linear light.
                let ch = half::f16::from_le_bytes([f.pixels[0], f.pixels[1]]).to_f32();
                assert!(ch.is_finite() && ch >= 0.0, "{name}: linear R = {ch}");
            }
            other => panic!("{name}: expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn hdr_pq_clip_emits_fp16_scene_linear_frames() {
        assert_hdr_pq_contract("hdr_pq.mp4");
    }

    /// Container parity (macos-video-smoothness §4): the SAME HEVC PQ stream in
    /// **Matroska** carries its HDR metadata identically — MKV is what the Session
    /// route plays on macOS now, so the contract needs an MKV witness, not only MP4.
    #[test]
    fn hdr_pq_mkv_carries_the_same_fp16_contract() {
        assert_hdr_pq_contract("hdr_pq.mkv");
    }

    fn opts(planar: bool, p010: bool) -> crate::VideoProducerOptions {
        crate::VideoProducerOptions {
            planar,
            supports_p010: p010,
        }
    }

    fn spawn_planar(
        name: &str,
        planar: bool,
        p010: bool,
    ) -> (Sender<VideoProducerMsg>, Receiver<VideoProducerEvent>) {
        spawn_input_opts(VideoInput::Path(fixture(name)), opts(planar, p010))
    }

    /// Drain `Opened`, request one credit, return the first `Frame`.
    fn first_frame(
        msgs: &Sender<VideoProducerMsg>,
        events: &Receiver<VideoProducerEvent>,
    ) -> VideoFrame {
        loop {
            match events.recv_timeout(Duration::from_secs(10)).expect("event") {
                VideoProducerEvent::Opened { .. } => msgs.send(VideoProducerMsg::Credit).unwrap(),
                VideoProducerEvent::Frame(f) => return f,
                other => panic!("expected Opened/Frame, got {other:?}"),
            }
        }
    }

    /// Task #91 Phase 2: with the planar path on and P010 supported, an HDR PQ
    /// clip negotiates to **P010** frames (3 bytes/px) carrying the PQ transfer
    /// and a real peak — the raw YUV the GPU shader converts, not a CPU fp16 pack.
    #[test]
    fn planar_hdr_clip_emits_p010_pq() {
        let (msgs, events) = spawn_planar("hdr_pq.mp4", true, true);
        // Opened must already carry the negotiated P010 budget (3 bytes/px).
        match events
            .recv_timeout(Duration::from_secs(10))
            .expect("opened")
        {
            VideoProducerEvent::Opened { frame_bytes, .. } => {
                assert_eq!(frame_bytes, 64 * 64 * 3, "P010 charges 3 bytes/px");
            }
            other => panic!("expected Opened, got {other:?}"),
        }
        msgs.send(VideoProducerMsg::Credit).unwrap();
        match events.recv_timeout(Duration::from_secs(10)).expect("frame") {
            VideoProducerEvent::Frame(f) => {
                assert_eq!(f.format, PixelFormat::P010);
                assert!(f.is_well_formed(), "P010 geometry/buffer contract");
                assert_eq!(f.color.transfer, crate::VideoTransfer::Pq);
                assert!(f.color.peak >= 1.0, "HDR peak {}", f.color.peak);
                assert_eq!(f.color.cicp, Some((9, 16, 9)), "BT.2020 PQ kept");
            }
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    /// An HDR clip on an adapter WITHOUT 16-bit-norm falls back to the fp16 RGBA
    /// path (no P010) — the capability gate (Codex Q3).
    #[test]
    fn planar_hdr_falls_back_to_fp16_without_p010_support() {
        let (msgs, events) = spawn_planar("hdr_pq.mp4", true, false);
        let f = first_frame(&msgs, &events);
        assert_eq!(f.format, PixelFormat::Rgba16F, "no P010 support → fp16");
    }

    /// An SDR clip with the planar path on negotiates to **NV12** (raw 8-bit YUV).
    #[test]
    fn planar_sdr_clip_emits_nv12() {
        let (msgs, events) = spawn_planar("black_then_color.mp4", true, true);
        let f = first_frame(&msgs, &events);
        assert_eq!(f.format, PixelFormat::Nv12);
        assert!(f.is_well_formed());
        assert!(!f.color.transfer.is_hdr(), "SDR transfer");
    }

    /// A rotated clip keeps the RGBA (parallel-convert) path — planar rotation is
    /// handled in geometry as a follow-on; v1 gates it out (Codex P1: the
    /// performant fallback stays, never demoted to serial).
    #[test]
    fn planar_rotated_clip_falls_back_to_rgba() {
        let (msgs, events) = spawn_planar("rotated90.mp4", true, true);
        let f = first_frame(&msgs, &events);
        assert!(
            !f.format.is_planar_video(),
            "rotated clip must not take the planar path, got {:?}",
            f.format
        );
    }

    /// Planar OFF (the default / `PB_VIDEO_NO_PLANAR`) preserves the pre-Phase-2
    /// behavior: the HDR clip still emits fp16, and `Opened` fires without priming.
    #[test]
    fn planar_off_emits_rgba_as_before() {
        let (msgs, events) = spawn_planar("hdr_pq.mp4", false, true);
        let f = first_frame(&msgs, &events);
        assert_eq!(f.format, PixelFormat::Rgba16F);
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
