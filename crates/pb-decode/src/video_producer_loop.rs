//! The shared video-producer credit/seek/select loop (task #130, plan §4).
//!
//! Both platform producers — Media Foundation (`mf_video_producer`) and FFmpeg
//! (`ffmpeg::video_producer`) — used to carry their own ~180-line copy of the same
//! hand-rolled loop: absorb messages (block only when idle), land a pending seek
//! (keyframe/recreate ≤ target, then decode forward discarding the run-up, watching
//! for a superseding `SeekTo`), and spend one credit on the next sequential frame.
//! The two copies were character-for-character identical apart from a ~9-operation
//! reader seam. That seam is [`VideoProducerBackend`]; this module owns the one loop.
//!
//! The loop is deliberately unaware of Media Foundation, libav, pixel formats, HDR,
//! and hardware decode — every backend-specific decision lives behind the trait, so
//! the credit/generation/seek-epoch *protocol* (the part the `VideoSession` and the
//! producer integration tests pin) can't drift between platforms. It is monomorphized
//! per backend (`run<B>`, not `dyn`): both backends own `!Send` COM/libav handles on
//! their own thread, and the associated `Raw` type rules out a trait object anyway.
//!
//! **Safety model (plan §3):** this is a behavioural refactor, not a byte-preserving
//! move, so the primary net is the deterministic [`mock`](tests) test in this file —
//! a fake backend drives the loop with no real decoder and no video file, asserting
//! the emitted `VideoProducerEvent` sequence *and* the backend call order. The
//! secondary net is each producer's existing real-video integration suite, which must
//! pass unchanged.

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::video::{
    SeekGeneration, VideoFrame, VideoProducerEvent, VideoProducerMsg, VideoSessionId,
};

/// The reader-sourced facts a backend reports at open, turned into
/// [`VideoProducerEvent::Opened`] by the shared loop. A backend computes these
/// *after* any format negotiation/prime, so `frame_bytes` matches the frames it
/// will actually emit (task 79.10: an NV12/P010 producer must not be credited as
/// if it emitted `w·h·4`).
pub(crate) struct Opened {
    pub duration: Option<Duration>,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
    pub frame_bytes: u64,
}

/// The per-backend seam the shared [`run`] loop drives. Every method is the *only*
/// thing that differs between the two producers at that point in the loop (plan §4's
/// differing-operation table). The backend owns its reader/decoder handle, PTS
/// origin, tracked position, park/EOS state, any primed frame, geometry, format, and
/// color; the loop owns only `gen`/`credits`/`pending` and the event channel.
///
/// **Construction is not on the trait.** Each backend has its own `open` (the FFmpeg
/// one takes an interrupt `cancel` flag the MF one has no analog for), constructed on
/// the producer thread by the entry wrapper — never moved across `thread::spawn`,
/// since the handles are `!Send`. `open` returns `(Self, Opened)`; the wrapper hands
/// the backend here. Mandatory teardown (MF's off-thread reader retire) lives in
/// `impl Drop`, so no early return / error / superseded landing can skip it.
pub(crate) trait VideoProducerBackend {
    /// An owned pre-conversion frame — `ff::frame::Video` (FFmpeg) / `IMFSample`
    /// (MF). Owned so no lifetime entangles the loop and so `run<B>` monomorphizes.
    type Raw;

    /// Decode + convert the next sequential frame: `Ok(Some((ts, pixels)))`,
    /// `Ok(None)` at end-of-stream, `Err` on unrecoverable input. Contract:
    /// - serves any primed/initial frame first (the FFmpeg planar prime), then decodes;
    /// - hides gap ticks internally — only Frame/EOS ever surface;
    /// - updates the backend's tracked position (the forward-hop reference);
    /// - on `Ok(None)` the backend **parks itself** ([`is_parked`](Self::is_parked)
    ///   returns true afterward) — the loop emits `EndOfStream`, never a second one.
    fn read_frame(&mut self) -> Result<Option<(i64, Vec<u8>)>, String>;

    /// Decode the next frame **without converting it** (the seek run-up discards most
    /// of what it decodes, so it must not pay readback + downscale + tone-map on
    /// frames it throws away). Same None=EOS / gap-hidden / position-tracking /
    /// self-park contract as [`read_frame`](Self::read_frame), but never serves the
    /// primed frame — a seek [`invalidate`](Self::invalidate_primed)s it first.
    fn decode_raw(&mut self) -> Result<Option<(i64, Self::Raw)>, String>;

    /// Read back (if a hardware surface) + color-convert one raw frame to output
    /// pixels. **By value** — single-use ownership; the run-up drops the ones it
    /// never converts.
    fn convert(&mut self, raw: Self::Raw) -> Result<Vec<u8>, String>;

    /// A seek target `Duration` in this backend's absolute time base — the landing
    /// bar, directly comparable with the timestamps [`decode_raw`](Self::decode_raw)
    /// and [`read_frame`](Self::read_frame) return.
    fn target_units(&self, target: Duration) -> i64;

    /// Whether a seek to `target_units` should **decode forward in place** rather
    /// than [`seek`](Self::seek): only a forward hop within the backend's budget from
    /// a *known* live position. Backward, too-far, no-live-reader, or parked → false
    /// → keyframe seek (which always lands correctly).
    fn can_decode_forward(&self, target_units: i64) -> bool;

    /// Reposition to a keyframe ≤ `target_units` (FFmpeg: an in-place
    /// `avformat_seek_file` then a decoder flush; MF: retire and recreate the reader
    /// positioned at the target). Postconditions the run-up relies on: the decoder is
    /// flushed/fresh, EOS/parked state is cleared, and any primed frame is discarded.
    /// Called only on the non-forward path.
    fn seek(&mut self, target_units: i64) -> Result<(), String>;

    /// Discard any primed/initial frame not yet delivered. A seek before the first
    /// credit supersedes it — it must never flash after the seek lands. The loop
    /// calls this when a pending seek is taken, on **both** the forward and keyframe
    /// paths (the forward path never calls [`seek`](Self::seek)). Idempotent; a no-op
    /// for backends that never prime (MF).
    fn invalidate_primed(&mut self);

    /// Whether the backend is parked after end-of-stream — no live frames until a
    /// seek revives it. The loop drops stale credits rather than read a parked
    /// backend (which would spuriously re-emit `EndOfStream`).
    fn is_parked(&self) -> bool;

    /// Anchor the PTS origin to a seek landing so the landing frame stamps at exactly
    /// `target`, used only when no normal frame set the origin first. Idempotent.
    fn anchor_origin_seek(&mut self, target_units: i64, target: Duration);

    /// Anchor the PTS origin to the first sequential frame's timestamp. Idempotent.
    fn anchor_origin_seq(&mut self, ts: i64);

    /// Assemble the protocol [`VideoFrame`] for converted `pixels` at `ts`. Stays in
    /// the seam because color is computed per backend (MF: a session constant; FFmpeg:
    /// per-format per-frame) — the one place unifying the frame build would force the
    /// two color models together.
    fn make_frame(
        &mut self,
        session_id: VideoSessionId,
        gen: SeekGeneration,
        ts: i64,
        pixels: Vec<u8>,
    ) -> VideoFrame;

    /// One `PB_VIDEO_DIAG` line per landed seek. Both backends gate on the same env
    /// var; the *format* differs (hop-vs-recreate wording, ms-vs-Duration), so it
    /// stays in the seam to preserve each producer's current diagnostic output.
    fn seek_diag(
        &self,
        forward: bool,
        runup: u32,
        demux: Duration,
        total: Duration,
        target: Duration,
    );
}

/// Run the producer to completion on the **current thread** (the entry wrapper spawns
/// the dedicated producer thread and constructs `backend` on it — never the event
/// loop). Returns when the stream ends and the session stops it, the session is
/// dropped (channel disconnect), or the backend fails. `backend` is dropped on the way
/// out, which is where mandatory teardown runs (`impl Drop`).
pub(crate) fn run<B: VideoProducerBackend>(
    mut backend: B,
    opened: Opened,
    session_id: VideoSessionId,
    generation: SeekGeneration,
    events: Sender<VideoProducerEvent>,
    msgs: Receiver<VideoProducerMsg>,
) {
    let fail = |error: String| {
        let _ = events.send(VideoProducerEvent::Failed { session_id, error });
    };
    let _ = events.send(VideoProducerEvent::Opened {
        session_id,
        duration: opened.duration,
        width: opened.width,
        height: opened.height,
        has_audio: opened.has_audio,
        frame_bytes: opened.frame_bytes,
    });

    // The credit/command/seek loop. Blocking recv IS the select: a Stop or a SeekTo
    // (or the session dropping its sender) wakes us regardless of credit starvation. A
    // SeekTo zeroes the credit balance — only credits received after it (which the
    // session sends after flushing) count, which makes the flush + regrant race-free
    // by channel order.
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

        // 2. Land a pending seek: keyframe ≤ target (or a short forward hop), decode
        // forward to the first frame ≥ it. A newer SeekTo supersedes every stage; a
        // superseded landing never publishes a frame.
        if let Some((target, g)) = pending.take() {
            gen = g;
            // A seek supersedes any negotiation-primed first frame — never flash it.
            backend.invalidate_primed();
            let seek_t0 = Instant::now();
            let target_units = backend.target_units(target);
            // Short forward hop → decode forward from here (no keyframe seek/flush,
            // far less run-up). Backward/far → seek to the keyframe ≤ target.
            let forward = backend.can_decode_forward(target_units);
            if !forward {
                if let Err(e) = backend.seek(target_units) {
                    fail(e);
                    break 'outer;
                }
            }
            let demux = seek_t0.elapsed();
            let mut runup = 0u32; // frames decoded then discarded en route to target
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
                // Decode WITHOUT converting: run-up frames are discarded, so we never
                // pay their readback + downscale + tone-map (the seek stall). Only the
                // landing frame is converted.
                match backend.decode_raw() {
                    Ok(Some((ts, raw))) => {
                        if ts >= target_units {
                            let pixels = match backend.convert(raw) {
                                Ok(p) => p,
                                Err(e) => {
                                    fail(e);
                                    break 'outer;
                                }
                            };
                            backend.seek_diag(forward, runup, demux, seek_t0.elapsed(), target);
                            landed = Some((ts, pixels));
                            break;
                        }
                        // Keyframe→target run-up: drop the raw frame (no convert).
                        runup += 1;
                    }
                    Ok(None) => {
                        // Sought at/near the end: the stream is over under the new
                        // generation; the backend parked itself.
                        let _ = events.send(VideoProducerEvent::EndOfStream {
                            session_id,
                            seek_generation: gen,
                        });
                        break;
                    }
                    Err(e) => {
                        fail(e);
                        break 'outer;
                    }
                }
            }
            if let Some((ts, pixels)) = landed {
                // The landing frame consumes a credit like any other (the session
                // granted fresh ones right behind the SeekTo). Block for one if needed
                // — Stop/SeekTo still interrupt.
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
                // Seek before any normal frame: anchor the origin so the landing
                // stamps exactly `target`.
                backend.anchor_origin_seek(target_units, target);
                let frame = backend.make_frame(session_id, gen, ts, pixels);
                if events.send(VideoProducerEvent::Frame(frame)).is_err() {
                    break 'outer;
                }
                credits -= 1;
            }
            continue 'outer;
        }

        // 3. Spend one credit on the next sequential frame.
        if credits > 0 {
            if backend.is_parked() {
                // Parked after EOS: these credits are stale — a seek resets.
                credits = 0;
                continue;
            }
            match backend.read_frame() {
                Ok(Some((ts, pixels))) => {
                    backend.anchor_origin_seq(ts);
                    let frame = backend.make_frame(session_id, gen, ts, pixels);
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
                    // The backend parked itself; a later SeekTo revives it, Stop/
                    // disconnect ends the thread.
                }
                Err(e) => {
                    fail(e);
                    break 'outer;
                }
            }
        }
    }
    // `backend` drops here — where mandatory teardown (MF's off-thread retire) runs.
}

#[cfg(test)]
mod tests {
    //! The primary safety net (task #130, plan §3 layer 1): a `MockBackend` drives
    //! the shared loop with **no real decoder and no video file**, so the
    //! credit/generation/seek/park protocol is pinned deterministically and
    //! cross-platform — the thing a green `cargo test` on the flaky real-video suites
    //! could never guarantee.
    //!
    //! Two things the plan (Codex) insists on and that a naive mock would miss:
    //! - **Call order, not just emitted events** — the extracted code is a protocol
    //!   state machine; a regression can emit a plausible event stream while calling
    //!   `seek`/`convert`/`make_frame` in the wrong order. Every test asserts on the
    //!   backend's recorded call log.
    //! - **Timing + non-trivial timestamps** — the two subtlest holes (a one-frame-
    //!   wrong seek landing, and a supersede that races the run-up) only reproduce
    //!   with a superseding `SeekTo` injected *inside* the run-up (the mock self-sends
    //!   via a cloned channel) and with reordered / keyframe-before-target timestamps
    //!   that a clean CFR `0,1,2,3` script would hide.

    use super::*;
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Arc, Mutex};

    use crate::video::{VideoColorInfo, VideoSessionId};
    use crate::PixelFormat;

    const SID: VideoSessionId = VideoSessionId(7);

    /// One backend call the loop made, in the order it made them. The *order* is the
    /// contract — a valid event stream produced by the wrong call sequence is still a
    /// regression (Codex).
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        ReadFrame,
        DecodeRaw,
        Convert(i64),
        TargetUnits,
        CanDecodeForward(i64),
        Seek(i64),
        InvalidatePrimed,
        IsParked,
        AnchorSeek(i64),
        AnchorSeq(i64),
        MakeFrame(i64),
        SeekDiag,
    }

    /// A scripted decoded frame, presented in **decode order** (which is not display
    /// order for B-frames — the timestamps can go backwards).
    #[derive(Clone, Copy)]
    struct Frame {
        ts: i64,
        keyframe: bool,
    }

    /// A fake decoder driving the shared loop. Time is in integer "ms" units:
    /// `target_units(d) = origin + d.as_millis()`, and frame `ts` are the same units,
    /// so a target of 500 ms lands on the first decode-order frame with `ts >= 500`.
    struct MockBackend {
        /// Decode-order script; `pos` is the next frame to yield.
        frames: Vec<Frame>,
        pos: usize,
        /// A negotiation-primed first frame (the FFmpeg planar path), served before
        /// any fresh decode; a seek invalidates it.
        primed: Option<i64>,
        /// The prime hit EOS before any frame (a zero-frame clip).
        primed_eos: bool,
        parked: bool,
        origin: Option<i64>,
        last_ts: i64,
        forward_budget: i64,
        log: Arc<Mutex<Vec<Call>>>,
        /// Timing hook: after the Nth (1-based) `decode_raw` call, self-send this
        /// message — how a superseding `SeekTo` is injected *inside* the run-up.
        inject: Vec<(usize, VideoProducerMsg)>,
        decode_calls: usize,
        injector: Sender<VideoProducerMsg>,
    }

    impl MockBackend {
        fn note(&self, c: Call) {
            self.log.lock().unwrap().push(c);
        }
    }

    fn pixels() -> Vec<u8> {
        vec![0u8; 16] // a 2×2 RGBA8 frame — well-formed, contents irrelevant
    }

    impl VideoProducerBackend for MockBackend {
        type Raw = i64; // the frame's ts stands in for an owned decoded frame

        fn read_frame(&mut self) -> Result<Option<(i64, Vec<u8>)>, String> {
            self.note(Call::ReadFrame);
            if self.primed_eos {
                self.primed_eos = false;
                self.parked = true;
                return Ok(None);
            }
            if let Some(ts) = self.primed.take() {
                self.last_ts = ts;
                return Ok(Some((ts, pixels())));
            }
            match self.frames.get(self.pos).copied() {
                Some(f) => {
                    self.pos += 1;
                    self.last_ts = f.ts;
                    Ok(Some((f.ts, pixels())))
                }
                None => {
                    self.parked = true;
                    Ok(None)
                }
            }
        }

        fn decode_raw(&mut self) -> Result<Option<(i64, i64)>, String> {
            self.note(Call::DecodeRaw);
            self.decode_calls += 1;
            for (n, msg) in &self.inject {
                if *n == self.decode_calls {
                    let _ = self.injector.send(*msg);
                }
            }
            match self.frames.get(self.pos).copied() {
                Some(f) => {
                    self.pos += 1;
                    self.last_ts = f.ts;
                    Ok(Some((f.ts, f.ts)))
                }
                None => {
                    self.parked = true;
                    Ok(None)
                }
            }
        }

        fn convert(&mut self, raw: i64) -> Result<Vec<u8>, String> {
            self.note(Call::Convert(raw));
            Ok(pixels())
        }

        fn target_units(&self, target: Duration) -> i64 {
            self.note(Call::TargetUnits);
            self.origin.unwrap_or(0) + target.as_millis() as i64
        }

        fn can_decode_forward(&self, target_units: i64) -> bool {
            self.note(Call::CanDecodeForward(target_units));
            !self.parked
                && target_units > self.last_ts
                && target_units - self.last_ts <= self.forward_budget
        }

        fn seek(&mut self, target_units: i64) -> Result<(), String> {
            self.note(Call::Seek(target_units));
            self.primed = None;
            self.primed_eos = false;
            self.parked = false;
            // Reposition to the last keyframe with ts ≤ target (the run-up decodes
            // forward from there); if none, the start.
            self.pos = self
                .frames
                .iter()
                .enumerate()
                .rfind(|(_, f)| f.keyframe && f.ts <= target_units)
                .map(|(i, _)| i)
                .unwrap_or(0);
            Ok(())
        }

        fn invalidate_primed(&mut self) {
            self.note(Call::InvalidatePrimed);
            self.primed = None;
            self.primed_eos = false;
        }

        fn is_parked(&self) -> bool {
            self.note(Call::IsParked);
            self.parked
        }

        fn anchor_origin_seek(&mut self, target_units: i64, target: Duration) {
            self.note(Call::AnchorSeek(target_units));
            self.origin
                .get_or_insert(target_units - target.as_millis() as i64);
        }

        fn anchor_origin_seq(&mut self, ts: i64) {
            self.note(Call::AnchorSeq(ts));
            self.origin.get_or_insert(ts);
        }

        fn make_frame(
            &mut self,
            session_id: VideoSessionId,
            gen: SeekGeneration,
            ts: i64,
            _pixels: Vec<u8>,
        ) -> VideoFrame {
            self.note(Call::MakeFrame(ts));
            let origin = self.origin.unwrap_or(0);
            VideoFrame {
                session_id,
                seek_generation: gen,
                pts: Duration::from_millis((ts - origin).max(0) as u64),
                width: 2,
                height: 2,
                format: PixelFormat::Rgba8,
                pixels: pixels(),
                color: VideoColorInfo::srgb(),
            }
        }

        fn seek_diag(&self, _f: bool, _r: u32, _d: Duration, _t: Duration, _tg: Duration) {
            self.note(Call::SeekDiag);
        }
    }

    /// A running loop under test: the producer on its own thread (like the real
    /// session drives it), a message sender, an event receiver, and the shared call
    /// log. Messages are sent **incrementally** — pre-loading them all would be wrong,
    /// because the loop's absorb phase greedily drains the channel, so a queued `Stop`
    /// would be consumed before any work runs. The request/response cadence (send a
    /// credit, receive its frame) makes the outcome deterministic; the run-up-supersede
    /// race, which timing alone can't pin, is driven by the mock's injection hook.
    struct Harness {
        tx: Sender<VideoProducerMsg>,
        rx: Receiver<VideoProducerEvent>,
        log: Arc<Mutex<Vec<Call>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Harness {
        fn send(&self, m: VideoProducerMsg) {
            self.tx.send(m).unwrap();
        }
        fn recv(&self) -> VideoProducerEvent {
            self.rx
                .recv_timeout(Duration::from_secs(5))
                .expect("an event within 5s")
        }
        fn recv_frame(&self) -> VideoFrame {
            match self.recv() {
                VideoProducerEvent::Frame(f) => f,
                other => panic!("expected a Frame, got {other:?}"),
            }
        }
        fn expect_opened(&self) {
            match self.recv() {
                VideoProducerEvent::Opened { .. } => {}
                other => panic!("expected Opened, got {other:?}"),
            }
        }
        /// Assert nothing arrives within a short window — a parked/idle producer that
        /// must NOT publish (a dropped stale credit, an invalidated primed frame).
        fn expect_idle(&self) {
            match self.rx.recv_timeout(Duration::from_millis(250)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                other => panic!("expected the producer to stay idle, got {other:?}"),
            }
        }
        /// Stop, join the producer thread, and return the recorded call log.
        fn finish(mut self) -> Vec<Call> {
            self.send(VideoProducerMsg::Stop);
            self.handle.take().unwrap().join().unwrap();
            Arc::try_unwrap(self.log).unwrap().into_inner().unwrap()
        }
    }

    /// Spawn the shared loop on a thread with a `MockBackend` built from `mock`.
    fn spawn(mock: MockBackendSpec) -> Harness {
        let (tx, msg_rx) = channel();
        let (ev_tx, rx) = channel();
        let log = Arc::new(Mutex::new(Vec::new()));
        let backend = MockBackend {
            frames: mock.frames,
            pos: 0,
            primed: mock.primed,
            primed_eos: mock.primed_eos,
            parked: false,
            origin: mock.primed, // a primed frame anchors the origin, like FFmpeg
            last_ts: mock.primed.unwrap_or(0),
            forward_budget: mock.forward_budget,
            log: log.clone(),
            inject: mock.inject,
            decode_calls: 0,
            injector: tx.clone(),
        };
        let opened = Opened {
            duration: None,
            width: 2,
            height: 2,
            has_audio: false,
            frame_bytes: 16,
        };
        let handle = std::thread::spawn(move || {
            run(backend, opened, SID, SeekGeneration::FIRST, ev_tx, msg_rx);
        });
        Harness {
            tx,
            rx,
            log,
            handle: Some(handle),
        }
    }

    /// The knobs a test sets; the rest of the backend is fixed.
    struct MockBackendSpec {
        frames: Vec<Frame>,
        primed: Option<i64>,
        primed_eos: bool,
        forward_budget: i64,
        inject: Vec<(usize, VideoProducerMsg)>,
    }

    impl MockBackendSpec {
        /// A plain SDR-ish clip: CFR keyframe-less-after-0 frames at the given ts,
        /// no prime, a generous forward budget.
        fn clip(ts: &[i64]) -> MockBackendSpec {
            MockBackendSpec {
                frames: ts
                    .iter()
                    .enumerate()
                    .map(|(i, &ts)| Frame {
                        ts,
                        keyframe: i == 0, // frame 0 is the only keyframe unless overridden
                    })
                    .collect(),
                primed: None,
                primed_eos: false,
                forward_budget: 4_000,
                inject: Vec::new(),
            }
        }
        fn frames(mut self, frames: Vec<Frame>) -> Self {
            self.frames = frames;
            self
        }
        fn primed(mut self, ts: i64) -> Self {
            self.primed = Some(ts);
            self
        }
        fn forward_budget(mut self, budget: i64) -> Self {
            self.forward_budget = budget;
            self
        }
        fn inject(mut self, inject: Vec<(usize, VideoProducerMsg)>) -> Self {
            self.inject = inject;
            self
        }
    }

    fn credit() -> VideoProducerMsg {
        VideoProducerMsg::Credit
    }
    fn seek(ms: u64, g: u64) -> VideoProducerMsg {
        VideoProducerMsg::SeekTo {
            target: Duration::from_millis(ms),
            generation: SeekGeneration(g),
        }
    }
    /// Frames whose only keyframe is frame 0 (the `MockBackendSpec::clip` default),
    /// each at a fixed 100 ms step — the common script for the seek tests.
    fn every_100ms(n: i64) -> Vec<Frame> {
        (0..n)
            .map(|i| Frame {
                ts: i * 100,
                keyframe: i == 0,
            })
            .collect()
    }

    /// Basic streaming: one credit → one frame, PTS normalized to the first frame and
    /// monotonic, generation stamped, and the per-frame call order is exactly
    /// `IsParked → ReadFrame → AnchorSeq → MakeFrame`.
    #[test]
    fn streams_one_frame_per_credit_with_normalized_pts_and_call_order() {
        let h = spawn(MockBackendSpec::clip(&[100, 133, 166]));
        h.expect_opened();
        h.send(credit());
        let f0 = h.recv_frame();
        assert_eq!(f0.pts, Duration::ZERO, "origin normalizes the first PTS");
        assert_eq!(f0.seek_generation, SeekGeneration::FIRST);
        assert!(f0.is_well_formed());
        h.send(credit());
        assert_eq!(h.recv_frame().pts, Duration::from_millis(33));
        h.send(credit());
        assert_eq!(h.recv_frame().pts, Duration::from_millis(66));
        let calls = h.finish();
        // The step-3 call sequence, repeated once per credited frame.
        assert_eq!(
            &calls[0..4],
            &[
                Call::IsParked,
                Call::ReadFrame,
                Call::AnchorSeq(100),
                Call::MakeFrame(100)
            ]
        );
        assert_eq!(
            &calls[4..8],
            &[
                Call::IsParked,
                Call::ReadFrame,
                Call::AnchorSeq(133),
                Call::MakeFrame(133)
            ]
        );
    }

    /// End-of-stream: after the frames run out, `read_frame` returns `None` → the loop
    /// emits exactly one `EndOfStream` with the live generation and parks. A further
    /// stale credit is dropped after a single `is_parked` — no second `EndOfStream`,
    /// no `read_frame`, no busy-spin.
    #[test]
    fn eos_emits_once_then_parks_and_drops_stale_credits_without_respin() {
        let h = spawn(MockBackendSpec::clip(&[0, 33]));
        h.expect_opened();
        h.send(credit());
        assert_eq!(h.recv_frame().pts, Duration::ZERO);
        h.send(credit());
        assert_eq!(h.recv_frame().pts, Duration::from_millis(33));
        // The next credit reaches EOS: exactly one EndOfStream at the live generation.
        h.send(credit());
        match h.recv() {
            VideoProducerEvent::EndOfStream {
                seek_generation, ..
            } => {
                assert_eq!(seek_generation, SeekGeneration::FIRST);
            }
            other => panic!("expected EndOfStream, got {other:?}"),
        }
        // A stale credit against the parked producer: no event, no re-emitted EOS.
        h.send(credit());
        h.expect_idle();
        let calls = h.finish();
        assert_eq!(
            calls.last(),
            Some(&Call::IsParked),
            "a stale parked credit only probes is_parked — no ReadFrame, no re-decode"
        );
    }

    /// Seek landing (Codex #1 — the top risk): with **reordered decode-order
    /// timestamps** and a **keyframe before the target**, the loop must land on the
    /// first frame whose `ts >= target` *in decode order*, convert only that one, and
    /// discard every run-up frame. A clean CFR script would hide a one-frame-wrong
    /// landing here.
    #[test]
    fn seek_lands_on_first_decode_order_ts_at_or_after_target_and_discards_the_runup() {
        // Decode order (B-frames): keyframe at 0, then 500 arrives before 480/490.
        // Target 450 → the first ts ≥ 450 in decode order is 500 (not 480).
        let frames = vec![
            Frame {
                ts: 0,
                keyframe: true,
            },
            Frame {
                ts: 200,
                keyframe: false,
            },
            Frame {
                ts: 400,
                keyframe: true,
            }, // a keyframe before the target
            Frame {
                ts: 500,
                keyframe: false,
            }, // decode-order: lands here
            Frame {
                ts: 480,
                keyframe: false,
            },
            Frame {
                ts: 490,
                keyframe: false,
            },
        ];
        // A tiny forward budget forces the keyframe-seek path (not a forward hop),
        // which is the branch this test exercises; the run-up then starts at the
        // keyframe ≤ target (ts 400) and lands on the first decode-order ts ≥ 450.
        let h = spawn(
            MockBackendSpec::clip(&[0])
                .frames(frames)
                .forward_budget(100),
        );
        h.expect_opened();
        h.send(seek(450, 1));
        h.send(credit());
        let f = h.recv_frame();
        assert_eq!(
            f.seek_generation,
            SeekGeneration(1),
            "landing carries the seek gen"
        );
        // origin anchored at target_units(450) - 450 = 0, so pts == landing ts.
        assert_eq!(
            f.pts,
            Duration::from_millis(500),
            "landed at the first ts ≥ target"
        );
        h.expect_idle(); // the run-up frames never publish
        let calls = h.finish();
        // Only the landing frame (ts 500) was converted — the run-up (0→400) wasn't.
        let converts: Vec<_> = calls
            .iter()
            .filter(|c| matches!(c, Call::Convert(_)))
            .collect();
        assert_eq!(
            converts,
            [&Call::Convert(500)],
            "only the landing frame converts"
        );
        // The seek call order: invalidate → target → forward-decision → keyframe seek
        // → decode run-up → … → convert → diag → anchor → make.
        assert_eq!(calls[0], Call::InvalidatePrimed);
        assert_eq!(calls[1], Call::TargetUnits);
        assert_eq!(calls[2], Call::CanDecodeForward(450));
        assert_eq!(
            calls[3],
            Call::Seek(450),
            "a large seek recreates to a keyframe"
        );
        assert!(matches!(calls[4], Call::DecodeRaw));
        // find the landing convert and assert the tail order from there.
        let ci = calls
            .iter()
            .position(|c| matches!(c, Call::Convert(_)))
            .unwrap();
        // The origin anchors on `target_units` (450), the landing frame stamps at its
        // own ts (500).
        assert_eq!(
            &calls[ci..ci + 4],
            &[
                Call::Convert(500),
                Call::SeekDiag,
                Call::AnchorSeek(450),
                Call::MakeFrame(500)
            ]
        );
    }

    /// A short **forward** seek from a known live position decodes forward in place —
    /// `can_decode_forward` is true, so `seek` (the keyframe recreate) is never called.
    #[test]
    fn short_forward_seek_decodes_forward_without_a_keyframe_seek() {
        // Stream two frames (live position advances to ts 33), then a +200 ms tap.
        let h = spawn(MockBackendSpec::clip(&[0, 33, 66, 100, 133, 200, 233]));
        h.expect_opened();
        h.send(credit());
        assert_eq!(h.recv_frame().pts, Duration::ZERO);
        h.send(credit());
        assert_eq!(h.recv_frame().pts, Duration::from_millis(33));
        h.send(seek(200, 1));
        h.send(credit());
        let f = h.recv_frame();
        assert_eq!(f.seek_generation, SeekGeneration(1));
        assert_eq!(
            f.pts,
            Duration::from_millis(200),
            "landed at the first ts ≥ 200"
        );
        let calls = h.finish();
        assert!(
            !calls.iter().any(|c| matches!(c, Call::Seek(_))),
            "a forward hop must never call seek(): {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, Call::CanDecodeForward(200))),
            "the forward decision was taken"
        );
    }

    /// The generation race (Codex #2): a newer `SeekTo` injected **inside the run-up**
    /// (via the mock's self-send hook, after the first decode) supersedes the older
    /// seek — the stale landing must never publish, and only the newest generation
    /// reaches the consumer. The hook also injects g2's credit so the outcome is
    /// deterministic without thread timing.
    #[test]
    fn newer_seek_during_runup_discards_the_stale_landing() {
        // g1 seeks to 900 (a run-up). During the run-up's first decode, inject a newer
        // g2 seek to 100 (+ its credit) — the next run-up poll sees g2 and abandons g1.
        let h = spawn(
            MockBackendSpec::clip(&[0])
                .frames(every_100ms(10))
                .inject(vec![(1, seek(100, 2)), (1, credit())]),
        );
        h.expect_opened();
        h.send(seek(900, 1));
        let f = h.recv_frame();
        assert_eq!(
            f.seek_generation,
            SeekGeneration(2),
            "the stale g1 landing never flashed"
        );
        assert_eq!(f.pts, Duration::from_millis(100), "g2 landed at its target");
        h.expect_idle();
        let calls = h.finish();
        // g1 converted nothing (abandoned mid-run-up); g2 converted exactly one landing.
        let converts = calls
            .iter()
            .filter(|c| matches!(c, Call::Convert(_)))
            .count();
        assert_eq!(converts, 1, "only the surviving seek converts a landing");
    }

    /// The supersede-before-publish race: a newer `SeekTo` arrives before the older
    /// seek's landing can be credited — only the newer generation publishes.
    #[test]
    fn seek_superseded_before_publish_never_flashes_the_stale_frame() {
        let h = spawn(MockBackendSpec::clip(&[0]).frames(every_100ms(8)));
        h.expect_opened();
        // g1, then g2 supersedes, then a credit — g1's landing must never flash.
        h.send(seek(300, 1));
        h.send(seek(500, 2));
        h.send(credit());
        let f = h.recv_frame();
        assert_eq!(f.seek_generation, SeekGeneration(2));
        assert!(f.pts >= Duration::from_millis(500));
        h.expect_idle();
        h.finish();
    }

    /// The FFmpeg planar prime (plan §5.1): a primed initial frame is served on the
    /// first credit *before* any fresh decode.
    #[test]
    fn primed_frame_is_served_before_any_fresh_decode() {
        // The primed frame (ts 0) publishes before the first scripted frame (ts 40).
        let h = spawn(MockBackendSpec::clip(&[40, 80]).primed(0));
        h.expect_opened();
        h.send(credit());
        assert_eq!(
            h.recv_frame().pts,
            Duration::ZERO,
            "the primed frame is served first"
        );
        h.send(credit());
        assert_eq!(
            h.recv_frame().pts,
            Duration::from_millis(40),
            "then the fresh decode"
        );
        h.finish();
    }

    /// …and a seek before that first credit **invalidates** the primed frame — it must
    /// never flash after the seek lands (the forward-path bug §5.1 guards).
    #[test]
    fn a_seek_before_the_first_credit_invalidates_the_primed_frame() {
        let h = spawn(MockBackendSpec::clip(&[0]).frames(every_100ms(4)).primed(0));
        h.expect_opened();
        h.send(seek(200, 1));
        h.send(credit());
        let f = h.recv_frame();
        assert_eq!(f.seek_generation, SeekGeneration(1));
        assert_eq!(
            f.pts,
            Duration::from_millis(200),
            "the landing, not the primed frame"
        );
        h.expect_idle(); // the invalidated primed frame does not also publish
        let calls = h.finish();
        assert_eq!(
            calls[0],
            Call::InvalidatePrimed,
            "the seek invalidates the prime first"
        );
    }

    /// After EOS the loop parks; a later `SeekTo` revives it (a replay/rewind) and the
    /// landing carries the new generation — no reopen at the loop level.
    #[test]
    fn seek_after_eos_replays_from_the_target() {
        let h = spawn(
            MockBackendSpec::clip(&[0]).frames(
                (0..3)
                    .map(|i| Frame {
                        ts: i * 100,
                        keyframe: true,
                    })
                    .collect(),
            ),
        );
        h.expect_opened();
        for _ in 0..3 {
            h.send(credit());
            let _ = h.recv_frame();
        }
        h.send(credit()); // drive to EOS + park
        match h.recv() {
            VideoProducerEvent::EndOfStream { .. } => {}
            other => panic!("expected EndOfStream, got {other:?}"),
        }
        // Replay: a seek to 0 revives the parked producer.
        h.send(seek(0, 1));
        h.send(credit());
        let replay = h.recv_frame();
        assert_eq!(
            replay.seek_generation,
            SeekGeneration(1),
            "the replay carries the seek gen"
        );
        assert_eq!(
            replay.pts,
            Duration::ZERO,
            "replay starts at the target (0)"
        );
        h.finish();
    }

    /// Stop with zero credits outstanding: the loop is blocked on `recv`, Stop reaches
    /// it, and it exits having produced no frame (the pacing invariant) — the backend
    /// is never even read.
    #[test]
    fn stop_with_no_credits_produces_no_frames() {
        let h = spawn(MockBackendSpec::clip(&[0, 100]));
        h.expect_opened();
        h.expect_idle(); // no credit → nothing produced
        let calls = h.finish();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, Call::ReadFrame | Call::DecodeRaw)),
            "a credit-starved producer never reads the backend: {calls:?}"
        );
    }
}
