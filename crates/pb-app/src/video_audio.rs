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
        pub fn seek(&self, _position: std::time::Duration) {}
        pub fn sample(&self) -> Option<AudioClockSample> {
            None
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::time::Duration;

    use std::cell::Cell;

    use pb_app_core::video::AudioClockState;
    use windows::core::{Interface, HSTRING};
    use windows::Foundation::Uri;
    use windows::Media::Core::{ISingleSelectMediaTrackList, MediaSource};
    use windows::Media::Playback::{MediaPlaybackItem, MediaPlaybackState, MediaPlayer};

    /// A WinRT `MediaPlayer` over the video file, playing its audio track ONLY —
    /// the picture is the `VideoSession`'s job, so the item's video tracks are
    /// deselected (below) to avoid a second, wasted 4K decode that starves both
    /// pipelines (owner-observed: stuttering audio + slow video on a 4K60 clip).
    /// Closed on drop, which stops playback and releases the pipeline.
    pub struct VideoAudio {
        player: MediaPlayer,
        /// Kept for the video-track deselection: track lists populate async after
        /// the source opens, so [`Self::sample`] retries until it lands.
        item: MediaPlaybackItem,
        session_id: VideoSessionId,
        video_deselected: Cell<bool>,
    }

    impl VideoAudio {
        /// Open the file's audio **paused** (`AutoPlay` off, no `Play()` yet): the
        /// source loads on Media Foundation's own threads while the video preroll
        /// fills, and the core's `ResumeVideoAudio` starts the two together.
        pub fn open(path: &Path, session_id: VideoSessionId, muted: bool) -> Option<VideoAudio> {
            let uri = Uri::CreateUri(&HSTRING::from(crate::live_audio::file_uri(path)?)).ok()?;
            let source = MediaSource::CreateFromUri(&uri).ok()?;
            let item = MediaPlaybackItem::Create(&source).ok()?;
            let player = MediaPlayer::new().ok()?;
            player.SetAutoPlay(false).ok()?;
            player.SetIsMuted(muted).ok()?;
            player.SetSource(&item).ok()?;
            Some(VideoAudio {
                player,
                item,
                session_id,
                video_deselected: Cell::new(false),
            })
        }

        /// Deselect the item's video track(s) so the player decodes audio only.
        /// The track list is empty until the source finishes opening, so this is
        /// retried from [`Self::sample`] (fast cadence while opening) until it
        /// takes. Harmless when the item has no video tracks.
        fn try_deselect_video(&self) {
            if self.video_deselected.get() {
                return;
            }
            let Ok(tracks) = self.item.VideoTracks() else {
                return;
            };
            let populated = tracks.Size().map(|n| n > 0).unwrap_or(false);
            if !populated {
                return; // not opened yet — retry on the next sample
            }
            if let Ok(select) = tracks.cast::<ISingleSelectMediaTrackList>() {
                if select.SetSelectedIndex(-1).is_ok() {
                    self.video_deselected.set(true);
                }
            }
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

        /// Seek to `position` (task #79 phase 6). The session treats the next
        /// near-target sample as the ack, so no completion event is needed here.
        pub fn seek(&self, position: std::time::Duration) {
            if let Ok(session) = self.player.PlaybackSession() {
                let _ = session.SetPosition(windows::Foundation::TimeSpan {
                    Duration: (position.as_nanos() / 100) as i64,
                });
            }
        }

        /// One audio clock sample: the player's state + position right now. The
        /// core routes it to the active session (stale session ids are dropped
        /// there).
        pub fn sample(&self) -> Option<AudioClockSample> {
            self.try_deselect_video();
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
