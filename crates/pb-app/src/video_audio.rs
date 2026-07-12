//! Video audio playback + the shell half of the audio clock bridge (task #79
//! phase 5).
//!
//! The shell owns a platform media player over the **video file itself** (its audio
//! track; the unrendered video track costs nothing user-visible — same posture as
//! the Live Photo audio player it mirrors). The player opens **paused** so it loads
//! in parallel with the `VideoSession`'s frame preroll; the core resumes the two
//! together and keeps them in lockstep through the session-state effects. The shell
//! samples the player's position ~4×/s into [`AudioClockSample`]s — the master
//! clock while audio plays. Mute is applied on the player only, so the clock keeps
//! running muted and A/V sync is mute-independent.
//!
//! Windows-only for now (WinRT `MediaPlayer`); the stub keeps call sites cfg-free —
//! `open` returning `None` makes the shell report `Failed`, and the session
//! degrades to silent playback on its monotonic clock.

use std::path::Path;

use pb_app_core::video::{AudioClockSample, VideoSessionId};

pub use imp::VideoAudio;

#[cfg(not(windows))]
mod imp {
    use super::*;

    /// No-op stub where there's no video audio backend yet (macOS/Linux video
    /// playback is phase-7 parity work).
    pub struct VideoAudio;

    impl VideoAudio {
        pub fn open(_path: &Path, _session_id: VideoSessionId, _muted: bool) -> Option<VideoAudio> {
            None
        }
        pub fn pause(&self) {}
        pub fn resume(&self) {}
        pub fn set_muted(&self, _muted: bool) {}
        pub fn sample(&self) -> Option<AudioClockSample> {
            None
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::time::Duration;

    use pb_app_core::video::AudioClockState;
    use windows::core::HSTRING;
    use windows::Foundation::Uri;
    use windows::Media::Core::MediaSource;
    use windows::Media::Playback::{MediaPlaybackState, MediaPlayer};

    /// A WinRT `MediaPlayer` over the video file, playing (only audibly) its audio
    /// track. Closed on drop, which stops playback and releases the pipeline.
    pub struct VideoAudio {
        player: MediaPlayer,
        session_id: VideoSessionId,
    }

    impl VideoAudio {
        /// Open the file's audio **paused** (`AutoPlay` off, no `Play()` yet): the
        /// source loads on Media Foundation's own threads while the video preroll
        /// fills, and the core's `ResumeVideoAudio` starts the two together.
        pub fn open(path: &Path, session_id: VideoSessionId, muted: bool) -> Option<VideoAudio> {
            let uri = Uri::CreateUri(&HSTRING::from(crate::live_audio::file_uri(path)?)).ok()?;
            let source = MediaSource::CreateFromUri(&uri).ok()?;
            let player = MediaPlayer::new().ok()?;
            player.SetAutoPlay(false).ok()?;
            player.SetIsMuted(muted).ok()?;
            player.SetSource(&source).ok()?;
            Some(VideoAudio { player, session_id })
        }

        /// Pause (session paused / rebuffering) — keeps the position.
        pub fn pause(&self) {
            let _ = self.player.Pause();
        }

        /// Start/resume (session entered `Playing`).
        pub fn resume(&self) {
            let _ = self.player.Play();
        }

        /// Mute in place — playback (and the clock) keeps running.
        pub fn set_muted(&self, muted: bool) {
            let _ = self.player.SetIsMuted(muted);
        }

        /// One audio clock sample: the player's state + position right now. The
        /// core routes it to the active session (stale session ids are dropped
        /// there).
        pub fn sample(&self) -> Option<AudioClockSample> {
            let session = self.player.PlaybackSession().ok()?;
            let raw = session.PlaybackState().ok()?;
            let state = if raw == MediaPlaybackState::Playing {
                AudioClockState::Playing
            } else if raw == MediaPlaybackState::Paused {
                AudioClockState::Paused
            } else if raw == MediaPlaybackState::Buffering {
                AudioClockState::Buffering
            } else {
                // Opening / None: not ready yet.
                AudioClockState::Opening
            };
            let pos = session.Position().ok()?;
            Some(AudioClockSample {
                session_id: self.session_id,
                state,
                position: Duration::from_nanos(pos.Duration.max(0) as u64 * 100),
                sampled_at_monotonic: Duration::ZERO, // delivered immediately after sampling
            })
        }
    }

    impl Drop for VideoAudio {
        fn drop(&mut self) {
            let _ = self.player.Pause();
            let _ = self.player.Close(); // IClosable — tears down the media pipeline
        }
    }
}
