//! The `VideoSession` — silent bounded video playback (task #79, phase 4).
//!
//! A forward-only streaming player with constant memory: a demand-driven producer
//! (one credit = one frame), a byte/frame-budgeted decoded-frame queue, real-PTS
//! pacing on an injected monotonic clock, preroll, and the locked
//! **rebuffer-don't-drift** underrun policy. Duration-independent by construction —
//! the queue never holds more than [`crate::video::VIDEO_QUEUE_MAX_FRAMES`] frames
//! within the byte budget, whether the clip is 10 s or 2 h.
//!
//! Deliberately separate from `Playback` (Live Photos / GIF / avis keep their
//! retained-frames path untouched). Audio joins in phase 5 (the clock master swaps
//! from monotonic to [`crate::video::AudioClockSample`]s); seeking in phase 6 (the
//! `seek_generation` plumbing is already honored on the discard side).
//!
//! Shell-neutral and codec-free: producers are anything that speaks
//! [`VideoProducerEvent`]/[`VideoProducerMsg`] over channels — fakes in the unit
//! tests, the Media Foundation reader thread (`pb_decode::run_video_producer`) in
//! the real app. Time is always passed in (`now: Instant`), never sampled here, so
//! every timing behavior is deterministic under test.

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use crate::video::{
    SeekGeneration, VideoProducerEvent, VideoProducerMsg, VideoQueueBudget, VideoSessionId,
    VideoSessionState,
};
use pb_decode::VideoFrame;

/// Frames that must be queued (or EOS known) before `Buffering` promotes to
/// `Playing` — the preroll. Phase 5 adds "audio ready-or-absent" to the condition.
pub const PREROLL_FRAMES: usize = 2;

/// The producer's ends of the session channels. Handed to the platform producer
/// thread (or a test fake). Dropping it fails the session cleanly (disconnect ⇒
/// `Failed` unless EOS already arrived).
pub struct VideoSessionIo {
    pub events: Sender<VideoProducerEvent>,
    pub msgs: Receiver<VideoProducerMsg>,
}

/// What one [`VideoSession::poll`] asks the shell to do.
#[derive(Debug, Default)]
pub struct SessionUpdate {
    /// Present this frame now (upload via the reusable present path, then drop —
    /// the CPU pixels' lifetime ends at upload). At most one per poll.
    pub present: Option<VideoFrame>,
    /// The state changed this poll (HUD/menu refresh hint).
    pub state_changed: bool,
}

/// Monotonic session clock (phase 4: no audio, so this *is* the master clock).
/// `position` is media time; it advances only while anchored (Playing), freezes on
/// pause/rebuffer, and re-anchors **hard** on resume — never smoothed, so paused or
/// rebuffered wall time can never leak into media position (no drift).
#[derive(Debug)]
struct SessionClock {
    position: Duration,
    /// `Some(at)` while running: position was `self.position` at instant `at`.
    anchor: Option<Instant>,
}

impl SessionClock {
    fn new() -> Self {
        SessionClock {
            position: Duration::ZERO,
            anchor: None,
        }
    }

    fn position(&self, now: Instant) -> Duration {
        match self.anchor {
            Some(at) => self.position + now.saturating_duration_since(at),
            None => self.position,
        }
    }

    /// Freeze at the current position (pause / rebuffer entry).
    fn freeze(&mut self, now: Instant) {
        self.position = self.position(now);
        self.anchor = None;
    }

    /// Hard re-anchor and run from `position` (resume / first play / post-rebuffer).
    fn run_from(&mut self, position: Duration, now: Instant) {
        self.position = position;
        self.anchor = Some(now);
    }
}

/// An in-app video playback: the session plus the item it plays (`AppCore::video`).
pub struct ActiveVideo {
    pub session: VideoSession,
    pub item: usize,
}

/// See the module docs. Construct with [`VideoSession::new`], hand the returned
/// [`VideoSessionIo`] to a producer, then drive `poll` every tick and the command
/// methods from user input.
pub struct VideoSession {
    pub id: VideoSessionId,
    generation: SeekGeneration,
    state: VideoSessionState,
    budget: VideoQueueBudget,
    /// Expected bytes of one fitted frame — the credit-granting estimate (the
    /// session fixes output geometry, so this is constant and accurate).
    frame_bytes: u64,
    events: Receiver<VideoProducerEvent>,
    to_producer: Sender<VideoProducerMsg>,
    queue: VecDeque<VideoFrame>,
    queued_bytes: u64,
    /// Credits granted whose frames haven't arrived yet. They count against the
    /// budget as `frame_bytes` each, so credits + queue can never overshoot it.
    credits_out: usize,
    clock: SessionClock,
    /// PTS of the frame currently on screen (parked frame at EOS/pause).
    pub current_pts: Option<Duration>,
    /// Stream facts from `Opened` (duration drives the phase-6 HUD/seek clamp).
    pub duration: Option<Duration>,
    eos: bool,
    /// Why the session failed, for the shell's error surface.
    pub error: Option<String>,
    /// True once playback ever started — distinguishes an initial fill from a
    /// mid-play rebuffer when leaving `Buffering`.
    started: bool,
}

impl VideoSession {
    /// A session plus the producer-side channel ends. `frame_bytes` is the fitted
    /// output frame size (w×h×4 for RGBA8).
    pub fn new(id: VideoSessionId, frame_bytes: u64) -> (VideoSession, VideoSessionIo) {
        Self::with_budget(id, frame_bytes, VideoQueueBudget::default())
    }

    /// Test/tuning constructor with an explicit budget.
    pub fn with_budget(
        id: VideoSessionId,
        frame_bytes: u64,
        budget: VideoQueueBudget,
    ) -> (VideoSession, VideoSessionIo) {
        let (events_tx, events_rx) = channel();
        let (msgs_tx, msgs_rx) = channel();
        let session = VideoSession {
            id,
            generation: SeekGeneration::FIRST,
            state: VideoSessionState::Opening,
            budget,
            frame_bytes: frame_bytes.max(1),
            events: events_rx,
            to_producer: msgs_tx,
            queue: VecDeque::new(),
            queued_bytes: 0,
            credits_out: 0,
            clock: SessionClock::new(),
            current_pts: None,
            duration: None,
            eos: false,
            error: None,
            started: false,
        };
        (
            session,
            VideoSessionIo {
                events: events_tx,
                msgs: msgs_rx,
            },
        )
    }

    pub fn state(&self) -> VideoSessionState {
        self.state
    }

    /// Media position right now (frozen while paused/buffering/ended).
    pub fn position(&self, now: Instant) -> Duration {
        self.clock.position(now)
    }

    /// Whether the tick loop must keep polling (any non-terminal, non-parked state).
    pub fn is_active(&self) -> bool {
        !matches!(
            self.state,
            VideoSessionState::Failed | VideoSessionState::Stopped | VideoSessionState::Ended
        ) || !self.queue.is_empty()
    }

    /// The per-tick drive: absorb producer events, run the state machine, grant
    /// credits, and return at most one frame to present.
    pub fn poll(&mut self, now: Instant) -> SessionUpdate {
        let mut update = SessionUpdate::default();
        if self.state.is_terminal() {
            return update;
        }
        self.drain_events(&mut update);
        if self.state.is_terminal() {
            return update;
        }

        use VideoSessionState::*;
        // A bounded fall-through loop so a promotion chains into its consequence
        // within ONE poll (Opening→Buffering when events arrived before the first
        // poll; Buffering→Playing presents the first frame immediately instead of
        // one tick late). At most one frame presents per poll either way.
        for _ in 0..3 {
            match self.state {
                Opening => {
                    // First signs of life move us into the fill; `Opened` alone is
                    // enough (facts arrived, decode underway).
                    if self.duration.is_some() || !self.queue.is_empty() || self.eos {
                        self.transition(Buffering, &mut update);
                        continue;
                    }
                }
                Buffering => {
                    if self.preroll_satisfied() {
                        if let Some(front_pts) = self.queue.front().map(|f| f.pts) {
                            // Initial fill starts the clock at the first frame's
                            // PTS; a rebuffer resumes from the frozen position
                            // (hard re-anchor — the stall never advances media
                            // time).
                            let from = if self.started {
                                self.clock.position
                            } else {
                                front_pts
                            };
                            self.clock.run_from(from, now);
                            self.started = true;
                            self.transition(Playing, &mut update);
                            continue;
                        } else if self.eos {
                            // EOS with nothing queued: an empty stream (or a
                            // rebuffer that drained straight into EOS) — park.
                            self.clock.freeze(now);
                            self.transition(Ended, &mut update);
                        }
                    }
                }
                Playing => {
                    // Present the next frame once its PTS is due. One per poll:
                    // the tick loop runs at display rate, which bounds catch-up
                    // bursts without dropping frames (the drop engine stays a
                    // future seam).
                    let due = self
                        .queue
                        .front()
                        .is_some_and(|f| f.pts <= self.clock.position(now));
                    if due {
                        let frame = self.queue.pop_front().expect("front checked");
                        self.queued_bytes = self.queued_bytes.saturating_sub(frame.byte_len());
                        self.current_pts = Some(frame.pts);
                        update.present = Some(frame);
                    } else if self.queue.is_empty() {
                        if self.eos {
                            // True end: park on the last presented frame.
                            self.clock.freeze(now);
                            self.transition(Ended, &mut update);
                        } else {
                            // Underrun: rebuffer, don't drift. Freeze the clock,
                            // hold the frame, refill to preroll, hard re-anchor
                            // on resume.
                            self.clock.freeze(now);
                            self.transition(Buffering, &mut update);
                        }
                    }
                }
                Paused | Seeking | Ended | Failed | Stopped => {}
            }
            break;
        }

        self.grant_credits();
        update
    }

    /// Pause (freeze the clock; the current frame holds).
    pub fn pause(&mut self, now: Instant) {
        if self.state == VideoSessionState::Playing {
            self.clock.freeze(now);
            self.state = VideoSessionState::Paused;
        }
    }

    /// Resume from pause (hard re-anchor at the frozen position).
    pub fn resume(&mut self, now: Instant) {
        if self.state == VideoSessionState::Paused {
            let pos = self.clock.position;
            self.clock.run_from(pos, now);
            self.state = VideoSessionState::Playing;
        }
    }

    /// Tear down: tell the producer to stop and go terminal. The producer also
    /// treats channel disconnect (this session being dropped) as `Stop`, so
    /// resources retire even without this being called.
    pub fn stop(&mut self) {
        if !self.state.is_terminal() {
            let _ = self.to_producer.send(VideoProducerMsg::Stop);
            self.state = VideoSessionState::Stopped;
            self.queue.clear();
            self.queued_bytes = 0;
        }
    }

    fn preroll_satisfied(&self) -> bool {
        self.queue.len() >= PREROLL_FRAMES || (self.eos && !self.queue.is_empty()) || self.eos
    }

    fn transition(&mut self, to: VideoSessionState, update: &mut SessionUpdate) {
        debug_assert!(
            self.state.can_transition_to(to),
            "illegal video session transition {:?} → {to:?}",
            self.state
        );
        self.state = to;
        update.state_changed = true;
    }

    fn drain_events(&mut self, update: &mut SessionUpdate) {
        loop {
            match self.events.try_recv() {
                Ok(VideoProducerEvent::Opened {
                    session_id,
                    duration,
                    ..
                }) => {
                    if session_id == self.id {
                        self.duration = duration;
                    }
                }
                Ok(VideoProducerEvent::Frame(frame)) => {
                    // Identity gate: a stale session or superseded seek generation
                    // must never present, even if it raced a flush.
                    if frame.session_id != self.id || frame.seek_generation != self.generation {
                        continue;
                    }
                    self.credits_out = self.credits_out.saturating_sub(1);
                    self.queued_bytes += frame.byte_len();
                    self.queue.push_back(frame);
                }
                Ok(VideoProducerEvent::EndOfStream {
                    session_id,
                    seek_generation,
                }) => {
                    if session_id == self.id && seek_generation == self.generation {
                        self.eos = true;
                    }
                }
                Ok(VideoProducerEvent::Failed { session_id, error }) => {
                    if session_id == self.id {
                        self.error = Some(error);
                        self.transition(VideoSessionState::Failed, update);
                        return;
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    // Producer died without a terminal event. EOS already seen is
                    // fine (it just exited); otherwise that's a failure.
                    if !self.eos && !self.state.is_terminal() {
                        self.error = Some("video decoder stopped unexpectedly".into());
                        self.transition(VideoSessionState::Failed, update);
                    }
                    return;
                }
            }
        }
    }

    /// Grant one credit per admissible future frame. Credits count against the
    /// budget at the expected frame size, so `queued + in-flight` respects the
    /// byte/frame invariant at all times.
    fn grant_credits(&mut self) {
        if self.eos || self.state.is_terminal() {
            return;
        }
        loop {
            let effective_bytes = self.queued_bytes + self.credits_out as u64 * self.frame_bytes;
            let effective_frames = self.queue.len() + self.credits_out;
            if !self
                .budget
                .admits(effective_bytes, effective_frames, self.frame_bytes)
            {
                return;
            }
            if self.to_producer.send(VideoProducerMsg::Credit).is_err() {
                return; // producer gone; the disconnect path reports it
            }
            self.credits_out += 1;
        }
    }

    /// Structural invariant, asserted by tests after every step: CPU-queued bytes
    /// (including granted credits) within `max(budget, one frame)`, frame count
    /// (including credits) within the frame cap.
    #[cfg(test)]
    fn assert_budget_invariant(&self) {
        let bytes = self.queued_bytes + self.credits_out as u64 * self.frame_bytes;
        let frames = self.queue.len() + self.credits_out;
        assert!(
            bytes <= self.budget.max_bytes.max(self.frame_bytes),
            "byte invariant broken: {bytes}"
        );
        assert!(
            frames <= self.budget.max_frames,
            "frame invariant: {frames}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb_decode::video::VideoColorInfo;
    use pb_decode::PixelFormat;

    const SID: VideoSessionId = VideoSessionId(7);
    /// 2×2 RGBA8 test frames.
    const FRAME_BYTES: u64 = 2 * 2 * 4;

    fn frame(pts_ms: u64) -> VideoFrame {
        VideoFrame {
            session_id: SID,
            seek_generation: SeekGeneration::FIRST,
            pts: Duration::from_millis(pts_ms),
            width: 2,
            height: 2,
            format: PixelFormat::Rgba8,
            pixels: vec![0; FRAME_BYTES as usize],
            color: VideoColorInfo::srgb(),
        }
    }

    fn opened(io: &VideoSessionIo, dur_ms: u64) {
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: SID,
                duration: Some(Duration::from_millis(dur_ms)),
                width: 2,
                height: 2,
            })
            .unwrap();
    }

    fn eos(io: &VideoSessionIo) {
        io.events
            .send(VideoProducerEvent::EndOfStream {
                session_id: SID,
                seek_generation: SeekGeneration::FIRST,
            })
            .unwrap();
    }

    /// Count credits currently sitting in the producer's inbox (drains them).
    fn drain_credits(io: &VideoSessionIo) -> usize {
        let mut n = 0;
        while let Ok(msg) = io.msgs.try_recv() {
            if msg == VideoProducerMsg::Credit {
                n += 1;
            }
        }
        n
    }

    #[test]
    fn preroll_then_paced_playback_then_eos_parks_the_last_frame() {
        let (mut s, io) = VideoSession::new(SID, FRAME_BYTES);
        let t0 = Instant::now();
        opened(&io, 100);

        // Opening → Buffering; credits flow immediately (frame cap = 3).
        let u = s.poll(t0);
        assert!(u.state_changed);
        assert_eq!(s.state(), VideoSessionState::Buffering);
        s.assert_budget_invariant();
        assert_eq!(drain_credits(&io), 3, "initial credits up to the frame cap");

        // One frame is not preroll.
        io.events.send(VideoProducerEvent::Frame(frame(0))).unwrap();
        let u = s.poll(t0);
        assert_eq!(s.state(), VideoSessionState::Buffering);
        assert!(u.present.is_none());

        // Second frame satisfies preroll → Playing, first frame presents at once.
        io.events
            .send(VideoProducerEvent::Frame(frame(33)))
            .unwrap();
        let u = s.poll(t0);
        assert_eq!(s.state(), VideoSessionState::Playing);
        let f = s.poll(t0).present.or(u.present).expect("first frame");
        assert_eq!(f.pts, Duration::ZERO);
        s.assert_budget_invariant();

        // The 33 ms frame is not due yet…
        assert!(s.poll(t0 + Duration::from_millis(10)).present.is_none());
        // …and presents once the clock passes its PTS.
        let f = s
            .poll(t0 + Duration::from_millis(34))
            .present
            .expect("second frame due");
        assert_eq!(f.pts, Duration::from_millis(33));

        // EOS with the queue drained → Ended, parked on the last frame.
        eos(&io);
        let u = s.poll(t0 + Duration::from_millis(40));
        assert_eq!(s.state(), VideoSessionState::Ended);
        assert!(u.state_changed);
        assert!(u.present.is_none());
        assert_eq!(s.current_pts, Some(Duration::from_millis(33)));
    }

    #[test]
    fn credits_never_exceed_the_budget_and_replenish_as_frames_present() {
        let (mut s, io) = VideoSession::new(SID, FRAME_BYTES);
        let t0 = Instant::now();
        opened(&io, 10_000);
        s.poll(t0);
        assert_eq!(drain_credits(&io), 3);

        // Producer honors all 3 credits. The fill poll goes straight to Playing
        // and presents the first frame — which frees exactly one slot.
        for pts in [0u64, 33, 66] {
            io.events
                .send(VideoProducerEvent::Frame(frame(pts)))
                .unwrap();
        }
        let u = s.poll(t0);
        assert!(u.present.is_some(), "first frame presents in the fill poll");
        s.assert_budget_invariant();
        assert_eq!(drain_credits(&io), 1, "one slot freed → one credit");

        // Each later present frees exactly one more credit.
        let f = s
            .poll(t0 + Duration::from_millis(34))
            .present
            .expect("pts 33 due");
        assert_eq!(f.pts, Duration::from_millis(33));
        s.assert_budget_invariant();
        assert_eq!(drain_credits(&io), 1);
        let f = s
            .poll(t0 + Duration::from_millis(67))
            .present
            .expect("pts 66 due");
        assert_eq!(f.pts, Duration::from_millis(66));
        assert_eq!(drain_credits(&io), 1);
    }

    #[test]
    fn oversized_frames_use_the_one_frame_exception() {
        // A "frame" bigger than the whole budget: exactly one credit at a time.
        let budget = VideoQueueBudget {
            max_bytes: 100,
            max_frames: 3,
        };
        let (mut s, io) = VideoSession::with_budget(SID, 1000, budget);
        let t0 = Instant::now();
        opened(&io, 1_000);
        s.poll(t0);
        assert_eq!(drain_credits(&io), 1, "one-frame exception: single credit");
    }

    #[test]
    fn underrun_rebuffers_without_drift_and_resumes_on_refill() {
        let (mut s, io) = VideoSession::new(SID, FRAME_BYTES);
        let t0 = Instant::now();
        opened(&io, 10_000);
        s.poll(t0);
        io.events.send(VideoProducerEvent::Frame(frame(0))).unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33)))
            .unwrap();
        s.poll(t0); // → Playing, presents pts 0
        let _ = s.poll(t0 + Duration::from_millis(34)); // presents pts 33

        // Queue empty, no EOS → rebuffer at the *frozen* position.
        let u = s.poll(t0 + Duration::from_millis(40));
        assert_eq!(s.state(), VideoSessionState::Buffering);
        assert!(u.state_changed);
        let frozen = s.position(t0 + Duration::from_millis(500));
        assert_eq!(
            frozen,
            s.position(t0 + Duration::from_millis(40)),
            "clock must freeze during rebuffer"
        );

        // A long stall later, two frames arrive → resume; the stall added zero
        // media time (rebuffer, don't drift) and no frame was skipped.
        io.events
            .send(VideoProducerEvent::Frame(frame(66)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(100)))
            .unwrap();
        let resume_at = t0 + Duration::from_secs(5);
        let u = s.poll(resume_at);
        assert_eq!(s.state(), VideoSessionState::Playing);
        assert!(u.state_changed);
        // The next due frame is 66 ms — presented once the re-anchored clock
        // reaches it (frozen position was ~40 ms, so ~26 ms after resume).
        assert!(s
            .poll(resume_at + Duration::from_millis(10))
            .present
            .is_none());
        let f = s
            .poll(resume_at + Duration::from_millis(30))
            .present
            .expect("resumes in order");
        assert_eq!(f.pts, Duration::from_millis(66));
    }

    #[test]
    fn pause_freezes_and_resume_reanchors_hard() {
        let (mut s, io) = VideoSession::new(SID, FRAME_BYTES);
        let t0 = Instant::now();
        opened(&io, 10_000);
        s.poll(t0);
        io.events.send(VideoProducerEvent::Frame(frame(0))).unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33)))
            .unwrap();
        s.poll(t0);

        s.pause(t0 + Duration::from_millis(5));
        assert_eq!(s.state(), VideoSessionState::Paused);
        let held = s.position(t0 + Duration::from_millis(5));
        assert_eq!(
            s.position(t0 + Duration::from_secs(60)),
            held,
            "a minute of pause adds no media time"
        );
        assert!(s.poll(t0 + Duration::from_secs(60)).present.is_none());

        // Resume 60 s later: pts 33 presents ~28 ms after resume, not instantly-late.
        let resume = t0 + Duration::from_secs(60);
        s.resume(resume);
        assert_eq!(s.state(), VideoSessionState::Playing);
        assert!(s.poll(resume + Duration::from_millis(10)).present.is_none());
        assert!(s.poll(resume + Duration::from_millis(30)).present.is_some());
    }

    #[test]
    fn vfr_deltas_pace_correctly() {
        let (mut s, io) = VideoSession::new(SID, FRAME_BYTES);
        let t0 = Instant::now();
        opened(&io, 10_000);
        s.poll(t0);
        // VFR: 0, 20, 120 (a long hold), 130.
        for pts in [0u64, 20, 120, 130] {
            // budget is 3 frames — feed as space allows below instead.
            let _ = pts;
        }
        io.events.send(VideoProducerEvent::Frame(frame(0))).unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(20)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(120)))
            .unwrap();
        s.poll(t0); // presents 0
        assert!(s.poll(t0 + Duration::from_millis(21)).present.is_some());
        // The 120 ms frame must NOT present early during the long hold…
        assert!(s.poll(t0 + Duration::from_millis(80)).present.is_none());
        assert_eq!(
            s.state(),
            VideoSessionState::Playing,
            "a hold is not an underrun"
        );
        // …and lands on time.
        assert!(s.poll(t0 + Duration::from_millis(121)).present.is_some());
    }

    #[test]
    fn stale_generation_and_foreign_session_frames_are_discarded() {
        let (mut s, io) = VideoSession::new(SID, FRAME_BYTES);
        let t0 = Instant::now();
        opened(&io, 10_000);
        s.poll(t0);

        let mut foreign = frame(0);
        foreign.session_id = VideoSessionId(999);
        io.events.send(VideoProducerEvent::Frame(foreign)).unwrap();
        let mut stale = frame(0);
        stale.seek_generation = SeekGeneration(5);
        io.events.send(VideoProducerEvent::Frame(stale)).unwrap();

        s.poll(t0);
        assert_eq!(s.state(), VideoSessionState::Buffering, "nothing counted");
        assert_eq!(
            s.queue.len(),
            0,
            "stale/foreign frames never enter the queue"
        );
    }

    #[test]
    fn stop_notifies_the_producer_and_goes_terminal() {
        let (mut s, io) = VideoSession::new(SID, FRAME_BYTES);
        let t0 = Instant::now();
        opened(&io, 10_000);
        s.poll(t0);
        drain_credits(&io);
        s.stop();
        assert_eq!(s.state(), VideoSessionState::Stopped);
        // The Stop rides the same channel as credits — it can't be deafened.
        let got_stop =
            std::iter::from_fn(|| io.msgs.try_recv().ok()).any(|m| m == VideoProducerMsg::Stop);
        assert!(got_stop, "producer must be told to stop");
        assert!(s.poll(t0).present.is_none(), "terminal: polls are inert");
    }

    #[test]
    fn producer_failure_and_silent_death_both_fail_the_session() {
        let (mut s, io) = VideoSession::new(SID, FRAME_BYTES);
        let t0 = Instant::now();
        io.events
            .send(VideoProducerEvent::Failed {
                session_id: SID,
                error: "no codec".into(),
            })
            .unwrap();
        s.poll(t0);
        assert_eq!(s.state(), VideoSessionState::Failed);
        assert!(s.error.as_deref().unwrap().contains("no codec"));

        // Silent death: channel dropped with no EOS.
        let (mut s2, io2) = VideoSession::new(SID, FRAME_BYTES);
        opened(&io2, 100);
        s2.poll(t0);
        drop(io2);
        s2.poll(t0);
        assert_eq!(s2.state(), VideoSessionState::Failed);
    }

    /// End to end on Windows: the real MF producer streaming the committed H.264
    /// fixture (~1 s, 30 fps, black lead-in) into a real session, in real time.
    /// The CFR acceptance in miniature: all frames present in order and the clip
    /// completes at roughly wall duration, with the budget invariant held on
    /// every poll.
    #[cfg(windows)]
    #[test]
    fn real_producer_plays_the_fixture_to_ended_at_wall_duration() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video/black_then_color.mp4");
        let (mut s, io) = VideoSession::new(SID, 64 * 64 * 4);
        let path = fixture.clone();
        std::thread::spawn(move || {
            pb_decode::run_video_producer(
                &path,
                None,
                SID,
                SeekGeneration::FIRST,
                io.events,
                io.msgs,
            );
        });

        let t0 = Instant::now();
        let mut presented = 0usize;
        let mut last_pts = None::<Duration>;
        loop {
            let now = Instant::now();
            let u = s.poll(now);
            s.assert_budget_invariant();
            if let Some(f) = u.present {
                if let Some(prev) = last_pts {
                    assert!(f.pts > prev, "frames present in PTS order");
                }
                last_pts = Some(f.pts);
                presented += 1;
            }
            match s.state() {
                VideoSessionState::Ended => break,
                VideoSessionState::Failed => panic!("failed: {:?}", s.error),
                _ => {}
            }
            assert!(
                t0.elapsed() < Duration::from_secs(15),
                "fixture must finish (presented {presented})"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let wall = t0.elapsed();
        assert!(
            (25..=35).contains(&presented),
            "~30 fixture frames, got {presented}"
        );
        // ~1 s of media completes in roughly wall duration (pacing is real).
        assert!(
            wall >= Duration::from_millis(800) && wall < Duration::from_secs(5),
            "wall {wall:?}"
        );
        assert!(s.duration.is_some(), "Opened carried the duration");
    }

    /// Duration independence as a structural property: stream 10 000 frames
    /// through and assert the queue + credit budget invariant after every poll —
    /// the plateau, not a fixed delta.
    #[test]
    fn ten_thousand_frames_hold_the_plateau() {
        let (mut s, io) = VideoSession::new(SID, FRAME_BYTES);
        let t0 = Instant::now();
        opened(&io, u64::MAX / 2);
        s.poll(t0);

        let mut sent = 0u64;
        let mut presented = 0u64;
        let mut credits = drain_credits(&io);
        let mut now = t0;
        while presented < 10_000 {
            // Producer: honor every credit immediately (fastest producer).
            while credits > 0 {
                io.events
                    .send(VideoProducerEvent::Frame(frame(sent * 10)))
                    .unwrap();
                sent += 1;
                credits -= 1;
            }
            // Consumer: tick 10 ms (frames are 10 ms apart → one due per tick).
            now += Duration::from_millis(10);
            if s.poll(now).present.is_some() {
                presented += 1;
            }
            s.assert_budget_invariant();
            credits += drain_credits(&io);
        }
        assert!(sent - presented <= 3, "lookahead stays within the cap");
    }
}
