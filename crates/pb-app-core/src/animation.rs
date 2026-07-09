//! Animation playback state (task #37) — the pure, testable core, mirroring
//! `slideshow.rs`. It owns a decoded sequence plus a cursor; the event loop drives it
//! by elapsed time (`is_due` / `advance`) and reads back the current frame to present.
//! No winit, no renderer, no I/O — kept deliberately shallow so it lifts cleanly into
//! the future `AppCore` (macos-native-ui-plan NS0).
//!
//! Privacy (task #2): the frames are a RAM-only cache, dropped when playback stops or
//! the user navigates away. Nothing here is serialized.
//!
//! Loop semantics: `loop_count == 0` means loop forever; a positive `N` plays the
//! whole sequence `N` times and then **parks on the last frame** (`finished`), the
//! GIF/APNG/WebP convention. Play/pause and the `,`/`.` frame-step never run off the
//! photo hot path — this only spins up when the user presses `P` (or steps a frame).

use std::time::Duration;

use pb_decode::{AnimFrame, Animation, AnimationKind, ColorTransform, DecodeError};

/// Playback cursor over a decoded [`Animation`].
pub struct Playback {
    anim: Animation,
    /// The frame currently shown.
    index: usize,
    /// Advancing on each `is_due` tick (false = paused).
    playing: bool,
    /// Completed full passes, for honoring a finite loop count.
    loops_done: u32,
    /// A finite loop count was reached: parked on the last frame, no more advancing
    /// until the user restarts (play/pause) or steps a frame.
    finished: bool,
    /// Whether the whole sequence has been decoded. Batch playback lands complete;
    /// **streaming** playback (task #69 — play a Live Photo `.mov` while it's still
    /// decoding) starts `false` and flips true on [`mark_complete`](Self::mark_complete).
    /// While incomplete, [`advance`](Self::advance) holds on the last *decoded* frame
    /// (the frontier) instead of wrapping or finishing — an underrun waits for more
    /// frames rather than ending the clip early.
    complete: bool,
}

impl Playback {
    /// Wrap a fully decoded animation. `playing` starts it immediately (the `P`-to-play
    /// path) or paused (the frame-step path), beginning on frame 0.
    pub fn new(anim: Animation, playing: bool) -> Self {
        Self {
            anim,
            index: 0,
            playing,
            loops_done: 0,
            finished: false,
            complete: true,
        }
    }

    /// Wrap a **streaming** animation whose frames are still arriving (task #69). `anim`
    /// holds the header plus however many frames have decoded so far (at least the first);
    /// feed later frames with [`push_frame`](Self::push_frame) and finalize the loop count
    /// with [`mark_complete`](Self::mark_complete). Playback begins on frame 0 and holds on
    /// the decoded frontier until either more frames arrive or the decode completes.
    pub fn new_streaming(anim: Animation, playing: bool) -> Self {
        Self {
            anim,
            index: 0,
            playing,
            loops_done: 0,
            finished: false,
            complete: false,
        }
    }

    /// Append a freshly decoded frame to a streaming sequence (task #69). No-op on the
    /// cursor; the next [`advance`](Self::advance) can now move past the old frontier.
    pub fn push_frame(&mut self, frame: AnimFrame) {
        self.anim.frames.push(frame);
    }

    /// Whether every frame has been decoded (batch playback, or a stream that has been
    /// [`mark_complete`](Self::mark_complete)d).
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Mark a streaming sequence fully decoded with its final `loop_count` (task #69).
    /// If the cursor is already parked on the now-final last frame (the stream caught up
    /// to the decoder before it finished), that pass is completed here — the same
    /// bookkeeping [`advance`](Self::advance) does when it lands on the last frame — so a
    /// finite clip finishes instead of looping.
    pub fn mark_complete(&mut self, loop_count: u32) {
        self.anim.loop_count = loop_count;
        self.complete = true;
        let n = self.frame_count();
        if n > 0 && self.index + 1 == n && !self.finished {
            self.loops_done += 1;
            if self.anim.loop_count != 0 && self.loops_done >= self.anim.loop_count {
                self.finished = true;
            }
        }
    }

    pub fn frame_count(&self) -> usize {
        self.anim.frames.len()
    }

    /// What kind of motion this is — notably [`AnimationKind::LivePhoto`], which reverts
    /// to the crisp still after finishing rather than parking on the low-res last frame.
    pub fn kind(&self) -> AnimationKind {
        self.anim.kind
    }

    /// The current frame index (0-based) — the EXIF panel's live "Frame X / N".
    pub fn index(&self) -> usize {
        self.index
    }

    /// The frame to display right now.
    pub fn current_frame(&self) -> &AnimFrame {
        &self.anim.frames[self.index]
    }

    /// The source→sRGB color transform every frame shares (P3 on the Image I/O path).
    pub fn color(&self) -> ColorTransform {
        self.anim.color
    }

    /// How long the current frame is held before advancing.
    pub fn current_delay(&self) -> Duration {
        self.anim.frames[self.index].delay
    }

    /// The loop count (0 = loop forever) — surfaced in the EXIF panel.
    pub fn loop_count(&self) -> u32 {
        self.anim.loop_count
    }

    /// Total play time of one full pass (sum of every frame's delay) — the EXIF
    /// panel's duration + average frame-rate figures.
    pub fn total_duration(&self) -> Duration {
        self.anim.frames.iter().map(|f| f.delay).sum()
    }

    /// Whether playback is actively advancing (playing and not parked at the end of a
    /// finite loop).
    pub fn is_playing(&self) -> bool {
        self.playing && !self.finished
    }

    /// Whether a finite loop count has been exhausted (parked on the last frame).
    /// Exercised by the unit tests (it pins the finite-loop park/replay behavior); not
    /// yet read by the event loop, which only needs [`is_playing`](Self::is_playing).
    #[allow(dead_code)]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Toggle play/pause; returns the new playing state. Pressing `P` on a finite loop
    /// that has finished (parked on the last frame) replays it from the top.
    pub fn toggle_play(&mut self) -> bool {
        if self.finished {
            self.finished = false;
            self.loops_done = 0;
            self.index = 0; // replay from the first frame
            self.playing = true;
        } else {
            self.playing = !self.playing;
        }
        self.playing
    }

    /// Whether the current frame's display time has elapsed and we should advance.
    /// `since_shown` is `now − frame_shown_at`, supplied by the caller (kept
    /// `Duration`-based so this is trivially testable, no `Instant` needed).
    pub fn is_due(&self, since_shown: Duration) -> bool {
        self.is_playing() && self.frame_count() > 1 && since_shown >= self.current_delay()
    }

    /// Advance to the next frame for playback, honoring the loop count. A pass
    /// completes the moment its **last frame is shown** (we land on `n-1`): that's when
    /// `loops_done` ticks, and when a finite count is reached we set `finished` and park
    /// right there on the last frame (rather than wrapping off it first). Otherwise we
    /// wrap to frame 0 to begin the next pass. Returns the new frame index.
    pub fn advance(&mut self) -> usize {
        if self.finished {
            return self.index; // parked on the last frame — no-op until restart
        }
        let n = self.frame_count();
        if n <= 1 {
            // A single frame so far. If the sequence is complete this is a degenerate
            // one-frame "animation" (a finite loop is immediately done); if it's still
            // streaming, hold here until the next frame arrives.
            if self.complete {
                self.finished = self.anim.loop_count != 0;
            }
            return self.index;
        }
        if self.index + 1 < n {
            self.index += 1;
            // Landing on the last frame completes this pass (it has now been shown) — but
            // only when the sequence is complete. While streaming, `n-1` is just the
            // decoded frontier, not the real end, so we don't count the pass yet.
            if self.complete && self.index + 1 == n {
                self.loops_done += 1;
                if self.anim.loop_count != 0 && self.loops_done >= self.anim.loop_count {
                    self.finished = true; // park on the last frame
                }
            }
        } else if self.complete {
            // We were parked-but-not-finished on the last frame (an infinite loop, or a
            // finite one with passes left): wrap to start the next pass.
            self.index = 0;
        }
        // else: streaming and parked on the decoded frontier — underrun. Hold on the last
        // decoded frame until `push_frame` extends the sequence or `mark_complete` ends it.
        self.index
    }

    /// Manual single-frame step (the `,`/`.` scrub): pause, then move by `delta`
    /// frames with wrap-around (and clear any finished/loop state so stepping past the
    /// end keeps working). Returns the new frame index.
    pub fn step(&mut self, delta: i32) -> usize {
        self.playing = false;
        self.finished = false;
        self.loops_done = 0;
        let n = self.frame_count() as i64;
        if n > 0 {
            self.index = (self.index as i64 + delta as i64).rem_euclid(n) as usize;
        }
        self.index
    }
}

/// What to do with an animation decode once it lands.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AnimWant {
    /// Eager prep on dwell: stash it ready (no visible change; `P` is then instant).
    Eager,
    /// `P` was pressed: start playing on arrival.
    Play,
    /// A frame-step key was pressed: land paused and step by this delta.
    Step(i32),
}

/// An in-flight off-thread animation decode (the whole sequence for `item`), kicked by `P` /
/// frame-step, or eagerly on dwell, so a big GIF/WebP never stalls the event loop. The still
/// first frame stays on screen until it lands.
pub struct AnimDecode {
    pub gen: u64,
    pub item: usize,
    /// The geometry epoch at kick time; a resize in between invalidates the fit, so a late
    /// result for the old size is discarded.
    pub epoch: u64,
    /// What to do when it lands — can be upgraded in flight (an eager prep becomes `Play` if
    /// the user presses `P` before it finishes).
    pub want: AnimWant,
    pub rx: std::sync::mpsc::Receiver<Result<Animation, DecodeError>>,
    /// Cooperative cancel flag: set it (navigate away / supersede) and the worker's decoder
    /// checks it and bails early instead of decoding the whole clip on an orphaned thread.
    /// Honored by the Linux FFmpeg motion decoder; a no-op for the OS players / still animations.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// A whole animation decoded ahead of the user (eager prep) and held ready for instant
/// playback. RAM-only; dropped on navigate.
pub struct Prepared {
    pub item: usize,
    pub anim: Animation,
}

/// One message from a **streaming** Live Photo motion decode (task #69). The
/// platform-neutral mirror of `pb_decode::MotionChunk` — the (Linux-only) worker maps the
/// decoder's chunks onto this so the core wiring (`AnimStream`, `poll_anim_stream`) carries
/// no platform-specific type. Order is always: one `Header`, then `Frame`s, then a terminal
/// `Done` or `Failed`.
pub enum StreamMsg {
    /// Sent once before any frame: the display size + color to build the `Animation` header.
    Header {
        width: u32,
        height: u32,
        color: ColorTransform,
        codec: &'static str,
    },
    /// A decoded frame, in playback order.
    Frame(AnimFrame),
    /// The stream finished cleanly (`truncated` if the frame cap was hit).
    Done { loop_count: u32, truncated: bool },
    /// The stream failed. If a Playback is already installed this is a truncated finish;
    /// otherwise it's a play failure to surface.
    Failed(String),
}

/// An in-flight **streaming** Live Photo motion decode (task #69) — frames arrive and play
/// while the rest of the `.mov` is still decoding, instead of waiting for the whole clip.
/// Only ever set on the Linux FFmpeg path; the macOS/Windows OS players stay on the batch
/// [`AnimDecode`] path (they decode the whole clip in one call).
pub struct AnimStream {
    pub gen: u64,
    pub item: usize,
    pub epoch: u64,
    /// What to do as frames land — `Play` installs a playing Playback on the first frame;
    /// `Eager`/`Step` accumulate into `pending` and install/stash on `Done`. Upgradable
    /// (an eager stream becomes `Play` when the user presses `P`).
    pub want: AnimWant,
    pub rx: std::sync::mpsc::Receiver<StreamMsg>,
    /// Cooperative cancel flag (navigate away / supersede) — the worker's decoder checks it
    /// per packet and stops mid-clip.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The header once received (dims/color/codec) — needed to build the [`Animation`].
    pub header: Option<StreamHeader>,
    /// Frames accumulated before a Playback is installed: the eager/step path holds them
    /// until `Done`; the play path only uses this for the header-before-first-frame gap.
    pub pending: Vec<AnimFrame>,
    /// Whether a streaming Playback has been installed (Play mode, first frame seen). Once
    /// true, later frames are pushed straight onto `playback`.
    pub installed: bool,
}

/// The header fields of a streaming motion decode, stashed until the frames arrive.
pub struct StreamHeader {
    pub width: u32,
    pub height: u32,
    pub color: ColorTransform,
    pub codec: &'static str,
}

impl AnimStream {
    /// Build the accumulated header + `pending` frames into a complete [`Animation`] (the
    /// eager/step finish path, once `Done` lands). Returns `None` if no header/frames arrived.
    pub fn into_animation(self, loop_count: u32, truncated: bool) -> Option<Animation> {
        let h = self.header?;
        if self.pending.is_empty() {
            return None;
        }
        Some(Animation {
            kind: AnimationKind::LivePhoto,
            width: h.width,
            height: h.height,
            frames: self.pending,
            loop_count,
            codec: h.codec,
            color: h.color,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `n`-frame animation, each frame `delay_ms`, looping `loop_count` times.
    fn anim(n: usize, delay_ms: u64, loop_count: u32) -> Animation {
        let frames = (0..n)
            .map(|i| AnimFrame {
                rgba: vec![i as u8; 4],
                width: 1,
                height: 1,
                delay: Duration::from_millis(delay_ms),
            })
            .collect();
        Animation {
            kind: AnimationKind::Gif,
            width: 1,
            height: 1,
            frames,
            loop_count,
            codec: "GIF",
            color: ColorTransform::srgb(),
            truncated: false,
        }
    }

    #[test]
    fn new_starts_on_frame_zero() {
        let pb = Playback::new(anim(3, 100, 0), true);
        assert_eq!(pb.index(), 0);
        assert!(pb.is_playing());
        assert_eq!(pb.frame_count(), 3);
    }

    #[test]
    fn advance_wraps_an_infinite_loop_forever() {
        let mut pb = Playback::new(anim(3, 100, 0), true);
        assert_eq!(pb.advance(), 1);
        assert_eq!(pb.advance(), 2);
        assert_eq!(pb.advance(), 0); // wrap
        assert_eq!(pb.advance(), 1);
        assert!(!pb.is_finished());
        assert!(pb.is_playing());
    }

    #[test]
    fn advance_parks_on_last_frame_after_a_finite_count() {
        // 2 frames, play twice: 0→1→0→1, then finished, parked on the last frame.
        let mut pb = Playback::new(anim(2, 100, 2), true);
        assert_eq!(pb.advance(), 1); // end of pass 1
        assert_eq!(pb.advance(), 0); // start of pass 2 (loops_done = 1)
        assert_eq!(pb.advance(), 1); // end of pass 2 (loops_done = 2 → finished)
        assert!(pb.is_finished());
        assert!(!pb.is_playing());
        // Further advances are no-ops while parked on the last frame.
        assert_eq!(pb.advance(), 1);
        assert_eq!(pb.index(), 1);
    }

    #[test]
    fn is_due_respects_playing_delay_and_finish() {
        let mut pb = Playback::new(anim(3, 100, 0), true);
        assert!(!pb.is_due(Duration::from_millis(99)));
        assert!(pb.is_due(Duration::from_millis(100)));
        assert!(pb.is_due(Duration::from_millis(250)));
        // Paused → never due.
        pb.toggle_play();
        assert!(!pb.is_due(Duration::from_secs(10)));
        // A single-frame animation is never due (nothing to advance to).
        let solo = Playback::new(anim(1, 100, 0), true);
        assert!(!solo.is_due(Duration::from_secs(10)));
    }

    #[test]
    fn step_pauses_and_wraps_both_directions() {
        let mut pb = Playback::new(anim(4, 100, 0), true);
        assert_eq!(pb.step(1), 1);
        assert!(!pb.is_playing(), "stepping pauses");
        assert_eq!(pb.step(-1), 0);
        assert_eq!(pb.step(-1), 3, "wraps below zero to the last frame");
        assert_eq!(pb.step(2), 1, "wraps above the end");
    }

    #[test]
    fn toggle_play_restarts_a_finished_finite_loop() {
        let mut pb = Playback::new(anim(2, 100, 1), true);
        pb.advance(); // 0→1, end of the single pass → finished
        assert!(pb.is_finished());
        assert!(pb.toggle_play(), "toggling a finished loop replays it");
        assert!(pb.is_playing());
        assert!(!pb.is_finished());
    }

    #[test]
    fn total_duration_and_loop_count_report_the_whole_sequence() {
        let pb = Playback::new(anim(4, 40, 3), true);
        assert_eq!(pb.total_duration(), Duration::from_millis(160)); // 4 × 40ms
        assert_eq!(pb.loop_count(), 3);
        // Total duration is the full pass, independent of the current cursor.
        let mut pb = pb;
        pb.advance();
        assert_eq!(pb.total_duration(), Duration::from_millis(160));
        assert_eq!(Playback::new(anim(2, 100, 0), true).loop_count(), 0); // infinite
    }

    #[test]
    fn current_frame_and_delay_track_the_cursor() {
        let mut pb = Playback::new(anim(3, 40, 0), true);
        assert_eq!(pb.current_frame().rgba, vec![0u8; 4]);
        assert_eq!(pb.current_delay(), Duration::from_millis(40));
        pb.advance();
        assert_eq!(pb.current_frame().rgba, vec![1u8; 4]);
    }

    /// One `AnimFrame` tagged `id` so streaming pushes are distinguishable.
    fn frame(id: u8, delay_ms: u64) -> AnimFrame {
        AnimFrame {
            rgba: vec![id; 4],
            width: 1,
            height: 1,
            delay: Duration::from_millis(delay_ms),
        }
    }

    // --- Streaming playback (task #69: play a Live Photo while it's still decoding) ---

    #[test]
    fn streaming_holds_on_the_frontier_until_more_frames_arrive() {
        // Start streaming with two decoded frames; the rest are still "decoding".
        let mut pb = Playback::new_streaming(anim(2, 40, 0), true);
        assert!(!pb.is_complete());
        assert_eq!(pb.advance(), 1); // play up to the decoded frontier
        // Underrun: no more frames yet, and we're *not* complete, so hold — never wrap to 0
        // and never finish (unlike a complete infinite loop, which would wrap here).
        assert_eq!(pb.advance(), 1);
        assert_eq!(pb.advance(), 1);
        assert!(!pb.is_finished());
        assert!(pb.is_playing());
        // A freshly decoded frame lands → the frontier moves and playback resumes.
        pb.push_frame(frame(2, 40));
        assert_eq!(pb.advance(), 2);
        assert_eq!(pb.advance(), 2, "hold again on the new frontier");
    }

    #[test]
    fn streaming_finishes_a_live_photo_once_complete() {
        // A Live Photo streams in and plays through once (loop_count = 1).
        let mut pb = Playback::new_streaming(anim(2, 40, 0), true);
        pb.push_frame(frame(2, 40)); // 3 frames total
        assert_eq!(pb.advance(), 1);
        assert_eq!(pb.advance(), 2); // frontier
        assert!(!pb.is_finished(), "not finished until decode completes");
        pb.mark_complete(1);
        assert!(pb.is_complete());
        // Parked on the last frame when completion arrived → that pass is done.
        assert!(pb.is_finished());
        assert!(!pb.is_playing());
    }

    #[test]
    fn streaming_completes_mid_playback_then_plays_out_and_finishes() {
        // Completion can arrive while frames are still ahead of the cursor.
        let mut pb = Playback::new_streaming(anim(3, 40, 0), true);
        assert_eq!(pb.advance(), 1); // cursor mid-sequence
        pb.mark_complete(1); // real last frame is now index 2, still ahead
        assert!(!pb.is_finished(), "still frames to show");
        assert_eq!(pb.advance(), 2); // land on the true last frame → pass completes
        assert!(pb.is_finished());
        assert!(!pb.is_playing());
    }

    #[test]
    fn streaming_is_due_only_once_a_second_frame_exists() {
        // A one-frame stream can't advance yet, so it's never due (mirrors a solo still).
        let mut pb = Playback::new_streaming(
            Animation {
                frames: vec![frame(0, 40)],
                ..anim(1, 40, 0)
            },
            true,
        );
        assert!(!pb.is_due(Duration::from_secs(10)));
        pb.push_frame(frame(1, 40));
        assert!(pb.is_due(Duration::from_millis(40)));
    }
}
