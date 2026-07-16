//! macOS "native video" proxy + the cross-platform active-video facade (task 79.9).
//!
//! On macOS the media pipeline is the shell's native `AVPlayer` / `AVPlayerLayer`,
//! which owns decode, color, HDR, audio, timing, buffering, and seeking. The core
//! therefore does **not** run a [`VideoSession`](crate::video_session::VideoSession)
//! on macOS — that is the Windows/Linux engine. Instead it keeps a passive
//! [`NativeVideoProxy`]: a mirror of the player's authoritative state, updated only
//! by the shell's typed callbacks (each session- and, for seeks, generation-gated)
//! and read by the core's cross-cutting policy (slideshow suspend, deletion,
//! play-hint, menu/toolbar) through the shared [`ActiveVideoBackend`] facade.
//!
//! The proxy holds **no position**. `AVPlayer` is the single timing authority: the
//! info-line scrubber reads the player directly in Swift, and the core issues only
//! relative/fractional seek *intent* (`contract::CoreEffect::SeekVideoBy` /
//! `SeekVideoFraction`), which the shell resolves against the player's clock. That
//! is what removes the two-competing-clocks problem the early plan had.
//!
//! Everything here is pure (no AVFoundation, no I/O), so the whole state model is
//! unit-tested on synthetic callbacks — the "lock the model before implementation"
//! foundation for the macOS backend.

use crate::video::{SeekGeneration, VideoSessionId, VideoSessionState};
use crate::video_session::ActiveVideo;

/// The macOS active video: a passive mirror of the native player's state. Unlike
/// [`VideoSession`](crate::video_session::VideoSession) it runs no timing or
/// lifecycle logic — it only records what the shell's typed callbacks report.
#[derive(Debug, Clone)]
pub struct NativeVideoProxy {
    /// The library item this session plays.
    pub item: usize,
    /// Identity for every command/callback; a callback for a different session is
    /// ignored (a straggler from a torn-down player can never mutate this).
    pub session_id: VideoSessionId,
    /// The latest seek generation the core has issued. A `seek_completed` callback
    /// for any other generation is a superseded seek and is ignored.
    seek_generation: SeekGeneration,
    /// Whether a seek issued by the core has not yet been acknowledged by a
    /// matching-generation `seek_completed`. Lets the core know a seek is pending
    /// (e.g. to show a seeking affordance).
    seek_in_flight: bool,
    /// The playhead, as last reported by the shell (~20 Hz, `native_video_progress`).
    ///
    /// The shell owns the clock on this backend — AVPlayer's, or the sample-buffer
    /// route's render synchronizer — so this is a *mirror*, not the authority: it can
    /// only be as fresh as the last report. That is fine for what reads it (subtitle
    /// cue timing, ~50 ms granularity against cues that last seconds) and is why the
    /// core does not try to extrapolate between reports — an invented position that
    /// disagrees with the pixels on screen would be worse than a slightly stale one.
    position: Option<std::time::Duration>,
    /// Authoritative playback state, as last reported by the shell.
    state: VideoSessionState,
    /// Total duration, once the player reports it (`native_video_opened`).
    duration: Option<std::time::Duration>,
    /// Whether the clip has an audio track (from `native_video_opened`).
    has_audio: bool,
    /// The user's mute preference, applied to the player.
    muted: bool,
    /// Why the session failed, for the shell's error surface.
    pub error: Option<String>,
}

impl NativeVideoProxy {
    /// A freshly-commanded session: the core has emitted `PlayVideo` and is waiting
    /// for the shell's `native_video_opened` / `state_changed` callbacks.
    pub fn new(item: usize, session_id: VideoSessionId, muted: bool) -> Self {
        Self {
            item,
            session_id,
            seek_generation: SeekGeneration::FIRST,
            seek_in_flight: false,
            position: None,
            state: VideoSessionState::Opening,
            duration: None,
            has_audio: false,
            muted,
            error: None,
        }
    }

    pub fn state(&self) -> VideoSessionState {
        self.state
    }
    /// Not in a terminal state (`Failed`/`Stopped`) — the item still has a live
    /// player behind it (mirrors `VideoSession::is_active`).
    pub fn is_active(&self) -> bool {
        !self.state.is_terminal()
    }
    pub fn is_playing(&self) -> bool {
        self.state == VideoSessionState::Playing
    }
    /// The last playhead the shell reported; `None` before the first report.
    pub fn position(&self) -> Option<std::time::Duration> {
        self.position
    }

    /// Mirror the shell's clock (~20 Hz). Also clears a pending seek's staleness: a
    /// report only arrives once the player is actually there.
    pub fn set_position(&mut self, pos: std::time::Duration) {
        self.position = Some(pos);
    }

    pub fn duration(&self) -> Option<std::time::Duration> {
        self.duration
    }
    pub fn has_audio(&self) -> bool {
        self.has_audio
    }
    pub fn muted(&self) -> bool {
        self.muted
    }
    pub fn seek_generation(&self) -> SeekGeneration {
        self.seek_generation
    }
    pub fn seek_in_flight(&self) -> bool {
        self.seek_in_flight
    }

    /// Record the user's mute preference (the shell applies it to the player).
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    /// Begin a new seek: bump the generation and mark one in flight. The returned
    /// generation is what the core stamps on the emitted `SeekVideoBy` /
    /// `SeekVideoFraction` command, so the matching `seek_completed` can be told
    /// apart from any superseded one.
    #[must_use]
    pub fn begin_seek(&mut self) -> SeekGeneration {
        self.seek_generation = self.seek_generation.next();
        self.seek_in_flight = true;
        self.seek_generation
    }

    // ── Typed callbacks from the shell (session-/generation-gated) ────────────
    // Each returns `true` when it applied (matched this session), so callers/tests
    // can assert a straggler was dropped.

    /// The player opened: record the clip facts. Ignored if the session id differs.
    pub fn on_opened(
        &mut self,
        session_id: VideoSessionId,
        duration: Option<std::time::Duration>,
        has_audio: bool,
    ) -> bool {
        if session_id != self.session_id {
            return false;
        }
        self.duration = duration;
        self.has_audio = has_audio;
        true
    }

    /// The player's authoritative state changed. `Ended`/`Failed` have their own
    /// callbacks, so a `state_changed` never carries them (finding #7); if one
    /// arrives it is treated as a no-op transition guard rather than a terminal.
    pub fn on_state_changed(
        &mut self,
        session_id: VideoSessionId,
        state: VideoSessionState,
    ) -> bool {
        if session_id != self.session_id {
            return false;
        }
        // Don't let a `state_changed` resurrect a terminal session (a late
        // callback racing teardown): terminals only enter via `on_ended`/`on_failed`.
        if self.state.is_terminal() || state.is_terminal() {
            return false;
        }
        self.state = state;
        true
    }

    /// A seek completed (`finished = true`) or was interrupted by a newer seek
    /// (`finished = false`). Only the current generation clears the in-flight flag;
    /// a completion for a superseded generation is ignored (findings #7/#8).
    pub fn on_seek_completed(
        &mut self,
        session_id: VideoSessionId,
        generation: SeekGeneration,
        finished: bool,
    ) -> bool {
        if session_id != self.session_id || generation != self.seek_generation {
            return false;
        }
        // The current generation landing (or being cleanly interrupted) ends the
        // pending state; a newer seek would already have bumped `seek_generation`.
        if finished {
            self.seek_in_flight = false;
        }
        true
    }

    /// The clip reached end-of-stream: park in `Ended` (the shell keeps the last
    /// frame on screen — never re-shows the poster). Ignored for a stale session.
    pub fn on_ended(&mut self, session_id: VideoSessionId) -> bool {
        if session_id != self.session_id {
            return false;
        }
        self.state = VideoSessionState::Ended;
        self.seek_in_flight = false;
        true
    }

    /// The player failed. Ignored for a stale session.
    pub fn on_failed(&mut self, session_id: VideoSessionId, error: String) -> bool {
        if session_id != self.session_id {
            return false;
        }
        self.state = VideoSessionState::Failed;
        self.error = Some(error);
        self.seek_in_flight = false;
        true
    }
}

/// The cross-platform active-video backend behind `AppCore::video` (task 79.9,
/// finding #3). Windows/Linux drive a real [`VideoSession`] (wrapped in
/// [`ActiveVideo`]); macOS keeps a passive [`NativeVideoProxy`] fed by the native
/// player. Cross-cutting policy uses the common helpers below so it stays
/// platform-agnostic; genuinely platform-specific operations `match` the variant.
///
/// The backend is always `Session` on Windows/Linux and always `Native` on macOS,
/// so both arms compile everywhere and the executing arm is decided by which
/// variant `start_video_session` constructed — no `#[cfg]` sprawl at the call
/// sites, only at that one construction point.
// No `Debug`: `ActiveVideo` wraps a `VideoSession` whose mpsc channels aren't `Debug`.
// `large_enum_variant`: `AppCore::video` holds exactly one `Option<ActiveVideoBackend>`,
// never a collection, so the ~360-byte `Session` payload costs one field; boxing it would
// add heap indirection to the Windows/Linux producer-driving path for no real saving.
#[allow(clippy::large_enum_variant)]
pub enum ActiveVideoBackend {
    /// Windows/Linux: the streaming `VideoSession` fed by a decode producer.
    Session(ActiveVideo),
    /// macOS: the passive mirror of the shell's native `AVPlayer`.
    Native(NativeVideoProxy),
}

impl ActiveVideoBackend {
    /// The library item currently backed by a player/session.
    pub fn item(&self) -> usize {
        match self {
            Self::Session(v) => v.item,
            Self::Native(p) => p.item,
        }
    }

    pub fn session_id(&self) -> VideoSessionId {
        match self {
            Self::Session(v) => v.session.id,
            Self::Native(p) => p.session_id,
        }
    }

    pub fn state(&self) -> VideoSessionState {
        match self {
            Self::Session(v) => v.session.state(),
            Self::Native(p) => p.state(),
        }
    }

    /// Not in a terminal state — there is still a live player/session for the item.
    pub fn is_active(&self) -> bool {
        match self {
            Self::Session(v) => v.session.is_active(),
            Self::Native(p) => p.is_active(),
        }
    }

    /// Whether this backend needs the host loop to keep ticking **every frame** while it
    /// plays. Only the **Session** route does: `poll_video` pulls its decoded frames off the
    /// loop into the wgpu renderer. The **Native** route (macOS sample-buffer / AVPlayer) is
    /// presented by the OS compositor — the loop pulls nothing per frame, so keeping
    /// `work_pending` true for it just spins `pump()` at the display refresh (120 Hz) on the
    /// main thread, contending with the very presentation it isn't driving (owner-measured
    /// stutter, 2026-07-15). State changes, progress, relayout, and teardown are all
    /// kick-driven, so idling the loop between them is safe.
    pub fn needs_frame_pacing(&self) -> bool {
        match self {
            Self::Session(v) => v.session.is_active(),
            Self::Native(_) => false,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.state() == VideoSessionState::Playing
    }

    /// The playhead, whichever backend is live — the **one** place that knows how to ask.
    ///
    /// `Session` owns its clock, so it answers exactly (`now` paces the frames). `Native`
    /// mirrors the shell's clock from ~20 Hz reports, so it answers with the last one.
    /// Callers that need "where is the picture right now" (subtitle cues) must go through
    /// here rather than reaching for the session — on macOS the sample-buffer route is a
    /// `Native` backend, and a session-only check silently reports nothing.
    pub fn position(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        match self {
            Self::Session(v) => Some(v.session.desired_position(now)),
            Self::Native(p) => p.position(),
        }
    }

    pub fn duration(&self) -> Option<std::time::Duration> {
        match self {
            Self::Session(v) => v.session.duration,
            Self::Native(p) => p.duration(),
        }
    }

    /// The Windows/Linux session, if this is a `Session` backend (the producer-driving
    /// methods use it; `None` on macOS, where those methods emit commands instead).
    pub fn as_session(&self) -> Option<&ActiveVideo> {
        match self {
            Self::Session(v) => Some(v),
            Self::Native(_) => None,
        }
    }
    pub fn as_session_mut(&mut self) -> Option<&mut ActiveVideo> {
        match self {
            Self::Session(v) => Some(v),
            Self::Native(_) => None,
        }
    }

    /// The macOS proxy, if this is a `Native` backend (`None` on Windows/Linux).
    pub fn as_native(&self) -> Option<&NativeVideoProxy> {
        match self {
            Self::Native(p) => Some(p),
            Self::Session(_) => None,
        }
    }
    pub fn as_native_mut(&mut self) -> Option<&mut NativeVideoProxy> {
        match self {
            Self::Native(p) => Some(p),
            Self::Session(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::VideoSessionState::*;
    use std::time::Duration;

    fn proxy() -> NativeVideoProxy {
        NativeVideoProxy::new(7, VideoSessionId(1), false)
    }

    #[test]
    fn starts_opening_active_and_not_playing() {
        let p = proxy();
        assert_eq!(p.state(), Opening);
        assert!(p.is_active());
        assert!(!p.is_playing());
        assert_eq!(p.item, 7);
        assert_eq!(p.seek_generation(), SeekGeneration::FIRST);
    }

    #[test]
    fn opened_records_facts_only_for_the_matching_session() {
        let mut p = proxy();
        // A straggler from a different session is dropped.
        assert!(!p.on_opened(VideoSessionId(99), Some(Duration::from_secs(9)), true));
        assert_eq!(p.duration(), None);
        assert!(!p.has_audio());
        // The matching session records the facts.
        assert!(p.on_opened(VideoSessionId(1), Some(Duration::from_secs(9)), true));
        assert_eq!(p.duration(), Some(Duration::from_secs(9)));
        assert!(p.has_audio());
    }

    #[test]
    fn state_changes_track_the_player() {
        let mut p = proxy();
        assert!(p.on_state_changed(VideoSessionId(1), Buffering));
        assert_eq!(p.state(), Buffering);
        assert!(p.on_state_changed(VideoSessionId(1), Playing));
        assert!(p.is_playing());
        assert!(p.on_state_changed(VideoSessionId(1), Paused));
        assert_eq!(p.state(), Paused);
    }

    #[test]
    fn ended_is_not_terminal_so_replay_can_resume() {
        // EOS parks in `Ended` (not terminal); `P` replays via seek-to-0, so the
        // player legitimately reports `Ended → Playing` — the proxy must accept it.
        let mut p = proxy();
        assert!(p.on_ended(VideoSessionId(1)));
        assert_eq!(p.state(), Ended);
        assert!(p.is_active(), "Ended is replayable, not terminal");
        assert!(p.on_state_changed(VideoSessionId(1), Playing));
        assert!(p.is_playing());
    }

    #[test]
    fn a_failed_session_is_never_resurrected_by_a_late_state_change() {
        let mut p = proxy();
        assert!(p.on_failed(VideoSessionId(1), "boom".into()));
        assert_eq!(p.state(), Failed);
        // A state_changed racing teardown must not revive a truly-terminal session.
        assert!(!p.on_state_changed(VideoSessionId(1), Playing));
        assert_eq!(p.state(), Failed);
    }

    #[test]
    fn state_changed_ignores_a_foreign_session() {
        let mut p = proxy();
        assert!(!p.on_state_changed(VideoSessionId(2), Playing));
        assert_eq!(p.state(), Opening);
    }

    #[test]
    fn seek_generations_supersede_and_only_the_current_one_lands() {
        let mut p = proxy();
        let g1 = p.begin_seek();
        assert!(p.seek_in_flight());
        // A newer seek supersedes the first.
        let g2 = p.begin_seek();
        assert!(g2 > g1);
        // A completion for the superseded generation is ignored.
        assert!(!p.on_seek_completed(VideoSessionId(1), g1, true));
        assert!(p.seek_in_flight());
        // An interruption report (finished=false) for the current gen keeps it pending.
        assert!(p.on_seek_completed(VideoSessionId(1), g2, false));
        assert!(p.seek_in_flight());
        // The current generation landing clears the pending state.
        assert!(p.on_seek_completed(VideoSessionId(1), g2, true));
        assert!(!p.seek_in_flight());
    }

    #[test]
    fn seek_completed_ignores_a_foreign_session() {
        let mut p = proxy();
        let g = p.begin_seek();
        assert!(!p.on_seek_completed(VideoSessionId(2), g, true));
        assert!(p.seek_in_flight());
    }

    #[test]
    fn failure_is_terminal_and_carries_the_error() {
        let mut p = proxy();
        assert!(p.on_failed(VideoSessionId(1), "no codec".into()));
        assert_eq!(p.state(), Failed);
        assert!(!p.is_active());
        assert_eq!(p.error.as_deref(), Some("no codec"));
        // A foreign failure never touches a live session.
        let mut q = proxy();
        assert!(!q.on_failed(VideoSessionId(5), "x".into()));
        assert!(q.is_active());
    }

    #[test]
    fn mute_preference_is_recorded() {
        let mut p = proxy();
        assert!(!p.muted());
        p.set_muted(true);
        assert!(p.muted());
    }

    #[test]
    fn facade_native_helpers_reflect_the_proxy() {
        let mut p = proxy();
        p.on_opened(VideoSessionId(1), Some(Duration::from_secs(42)), true);
        p.on_state_changed(VideoSessionId(1), Playing);
        let backend = ActiveVideoBackend::Native(p);
        assert_eq!(backend.item(), 7);
        assert_eq!(backend.session_id(), VideoSessionId(1));
        assert!(backend.is_active());
        assert!(backend.is_playing());
        assert_eq!(backend.duration(), Some(Duration::from_secs(42)));
        assert!(backend.as_native().is_some());
        assert!(backend.as_session().is_none());
    }
}
