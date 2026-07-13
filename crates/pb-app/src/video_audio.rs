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
//! Two real backends: Windows (an MF Source Reader → **WASAPI** render engine,
//! `wasapi_audio.rs` — decodes and plays any audio MF can, including legacy
//! MJPEG-in-AVI/PCM the old WinRT `MediaPlayer` opened but refused to play) and
//! Linux with `ffvideo` (task #84 §7: the FFmpeg audio decoder streamed to
//! PipeWire's `pw-cat` — the proven Live-Photo output path, now streaming instead
//! of whole-clip). The stub keeps call sites cfg-free — `open` returning `None`
//! makes the shell report `Failed`, and the session degrades to silent playback on
//! its monotonic clock.

use pb_app_core::video::{AudioClockSample, VideoInput, VideoSessionId};

pub use imp::VideoAudio;

#[cfg(not(any(windows, all(unix, not(target_os = "macos"), feature = "ffvideo"))))]
mod imp {
    use super::*;

    /// No-op stub where there's no video audio backend (macOS uses the SwiftUI
    /// shell's AVAudioEngine sink; Linux needs the `ffvideo` feature).
    pub struct VideoAudio;

    impl VideoAudio {
        pub fn open(
            _input: &VideoInput,
            _session_id: VideoSessionId,
            _muted: bool,
        ) -> Option<VideoAudio> {
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

#[cfg(all(unix, not(target_os = "macos"), feature = "ffvideo"))]
mod imp {
    use super::*;
    use std::io::Write;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, Sender, TryRecvError};
    use std::sync::Arc;
    use std::time::Duration;

    use pb_app_core::video::AudioClockState;
    use pb_decode::FfAudioDecoder;

    /// The plan's §7 option (b) clock: `pw-cat` reports nothing, so the played
    /// position is frames-written minus a **characterized queue estimate** — the
    /// ~64 KiB stdin pipe plus pw-cat's quantum (`--latency` below). Constant,
    /// so A/V sync error is a fixed offset, not drift. Second-class-platform
    /// honest: documented estimate, not a measured device clock (pipewire-rs is
    /// the upgrade path if a Linux tester reports audible offset).
    const QUEUE_ESTIMATE: Duration = Duration::from_millis(150);
    /// One write chunk (~50 ms @ 48 kHz) — also the control-latency bound: the
    /// feeder polls its control channel between chunk writes.
    const CHUNK_FRAMES: usize = 2400;

    /// Streaming video audio for Linux: a feeder thread pulls PCM from the
    /// FFmpeg decoder and pipes it to `pw-cat` (see `live_audio.rs` for why
    /// pw-cat beats cpal/rodio on a PipeWire desktop); pipe backpressure paces
    /// it, so a 2-hour clip stays constant-memory.
    pub struct VideoAudio {
        session_id: VideoSessionId,
        rate: u32,
        ctl: Sender<Ctl>,
        shared: Arc<Shared>,
        feeder: Option<std::thread::JoinHandle<()>>,
    }

    struct Shared {
        /// Interleaved frames written to the pipe since the last (re)anchor.
        frames_written: AtomicU64,
        /// Media position (µs) of the write counter's zero.
        anchor_us: AtomicU64,
        paused: AtomicBool,
        muted: AtomicBool,
        /// The decoder drained — the tail is in the pipe/device queue.
        ended: AtomicBool,
        failed: AtomicBool,
    }

    enum Ctl {
        Pause,
        Resume,
        Seek(Duration),
        Stop,
    }

    impl VideoAudio {
        /// Open the decoder + spawn `pw-cat` **paused** (SIGSTOP right after
        /// spawn; the core's `ResumeVideoAudio` starts it with the video
        /// preroll). `None` when there's no track, the decoder fails, or
        /// `pw-cat` isn't on PATH — the caller reports `Failed` and the session
        /// plays silently.
        pub fn open(
            input: &VideoInput,
            session_id: VideoSessionId,
            muted: bool,
        ) -> Option<VideoAudio> {
            let decoder = FfAudioDecoder::open(input).ok()?;
            let (rate, channels) = (decoder.rate(), decoder.channels());
            if rate == 0 || channels == 0 {
                return None;
            }
            let child = spawn_pw_cat(rate, channels)?;
            let shared = Arc::new(Shared {
                frames_written: AtomicU64::new(0),
                anchor_us: AtomicU64::new(0),
                paused: AtomicBool::new(true),
                muted: AtomicBool::new(muted),
                ended: AtomicBool::new(false),
                failed: AtomicBool::new(false),
            });
            // Freeze immediately: opened-paused is the contract.
            signal(&child, libc::SIGSTOP);
            let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
            let feeder = std::thread::spawn({
                let shared = shared.clone();
                move || feeder_loop(decoder, child, rate, channels, shared, ctl_rx)
            });
            Some(VideoAudio {
                session_id,
                rate,
                ctl: ctl_tx,
                shared,
                feeder: Some(feeder),
            })
        }

        pub fn pause(&self) {
            self.shared.paused.store(true, Ordering::Relaxed);
            let _ = self.ctl.send(Ctl::Pause);
        }
        pub fn resume(&self) {
            self.shared.paused.store(false, Ordering::Relaxed);
            let _ = self.ctl.send(Ctl::Resume);
        }
        /// Mute = the feeder zeroes samples; the stream (and clock) keeps running.
        pub fn set_muted(&self, muted: bool) {
            self.shared.muted.store(muted, Ordering::Relaxed);
        }
        pub fn seek(&self, position: Duration) {
            let _ = self.ctl.send(Ctl::Seek(position));
        }

        /// One clock sample: anchor + written-frames time, minus the queue
        /// estimate (never below the anchor — right after a seek nothing of the
        /// new position has actually played yet).
        pub fn sample(&self) -> Option<AudioClockSample> {
            let s = &self.shared;
            let state = if s.failed.load(Ordering::Relaxed) {
                AudioClockState::Failed
            } else if s.ended.load(Ordering::Relaxed) {
                AudioClockState::Ended
            } else if s.paused.load(Ordering::Relaxed) {
                AudioClockState::Paused
            } else {
                AudioClockState::Playing
            };
            let anchor = Duration::from_micros(s.anchor_us.load(Ordering::Relaxed));
            let written = Duration::from_secs_f64(
                s.frames_written.load(Ordering::Relaxed) as f64 / self.rate.max(1) as f64,
            );
            let position = anchor + written.saturating_sub(QUEUE_ESTIMATE.min(written));
            Some(AudioClockSample {
                session_id: self.session_id,
                state,
                position,
                sampled_at_monotonic: Duration::ZERO, // delivered immediately
            })
        }
    }

    impl Drop for VideoAudio {
        fn drop(&mut self) {
            let _ = self.ctl.send(Ctl::Stop);
            if let Some(f) = self.feeder.take() {
                let _ = f.join(); // bounded: the feeder polls ctl every ~50 ms chunk
            }
        }
    }

    /// The feeder: pull ~50 ms PCM chunks, apply mute, pipe to pw-cat (blocking
    /// writes = pacing), polling the control channel between chunks. Owns the
    /// decoder and the child; a seek repositions the decoder and **respawns**
    /// pw-cat (dropping the old child flushes its queued audio instantly).
    fn feeder_loop(
        mut decoder: FfAudioDecoder,
        mut child: Child,
        rate: u32,
        channels: u16,
        shared: Arc<Shared>,
        ctl: Receiver<Ctl>,
    ) {
        let mut stdin = child.stdin.take();
        'outer: loop {
            // 1. Control (block while paused/ended — no busy spin).
            let blocking = shared.paused.load(Ordering::Relaxed)
                || (shared.ended.load(Ordering::Relaxed))
                || shared.failed.load(Ordering::Relaxed);
            let msg = if blocking {
                match ctl.recv() {
                    Ok(m) => Some(m),
                    Err(_) => break 'outer,
                }
            } else {
                match ctl.try_recv() {
                    Ok(m) => Some(m),
                    Err(TryRecvError::Empty) => None,
                    Err(_) => break 'outer,
                }
            };
            match msg {
                Some(Ctl::Stop) => break 'outer,
                Some(Ctl::Pause) => {
                    signal(&child, libc::SIGSTOP);
                    continue;
                }
                Some(Ctl::Resume) => {
                    signal(&child, libc::SIGCONT);
                    continue;
                }
                Some(Ctl::Seek(target)) => {
                    // Kill the old sink (drops its queued audio), reposition,
                    // respawn, re-anchor the clock at the landing.
                    let _ = stdin.take();
                    signal(&child, libc::SIGCONT);
                    let _ = child.kill();
                    let _ = child.wait();
                    if decoder.seek(target).is_err() {
                        shared.failed.store(true, Ordering::Relaxed);
                        continue;
                    }
                    // Land the discard so `position()` reports the real anchor.
                    let landing = decoder.read(1).unwrap_or_default();
                    let anchor = decoder.position();
                    match spawn_pw_cat(rate, channels) {
                        Some(mut c) => {
                            stdin = c.stdin.take();
                            child = c;
                            if shared.paused.load(Ordering::Relaxed) {
                                signal(&child, libc::SIGSTOP);
                            }
                        }
                        None => {
                            shared.failed.store(true, Ordering::Relaxed);
                            continue;
                        }
                    }
                    shared
                        .anchor_us
                        .store(anchor.as_micros() as u64, Ordering::Relaxed);
                    shared.frames_written.store(0, Ordering::Relaxed);
                    shared.ended.store(false, Ordering::Relaxed);
                    // Don't lose the landing frame.
                    if !landing.is_empty()
                        && write_chunk(&mut stdin, &landing, &shared, channels).is_err()
                    {
                        shared.failed.store(true, Ordering::Relaxed);
                    }
                    continue;
                }
                None => {}
            }

            // 2. One chunk of audio.
            match decoder.read(CHUNK_FRAMES) {
                Ok(chunk) if chunk.is_empty() => {
                    shared.ended.store(true, Ordering::Relaxed);
                }
                Ok(chunk) => {
                    if write_chunk(&mut stdin, &chunk, &shared, channels).is_err() {
                        shared.failed.store(true, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    eprintln!("video audio decode failed: {e}");
                    shared.failed.store(true, Ordering::Relaxed);
                }
            }
        }
        signal(&child, libc::SIGCONT);
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Write one interleaved chunk (muted → zeros; the clock advances either way).
    fn write_chunk(
        stdin: &mut Option<std::process::ChildStdin>,
        chunk: &[f32],
        shared: &Shared,
        channels: u16,
    ) -> std::io::Result<()> {
        let Some(w) = stdin.as_mut() else {
            return Err(std::io::Error::other("pw-cat stdin gone"));
        };
        let muted = shared.muted.load(Ordering::Relaxed);
        let mut bytes = Vec::with_capacity(chunk.len() * 4);
        for &s in chunk {
            let v = if muted { 0.0f32 } else { s };
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        w.write_all(&bytes)?;
        shared.frames_written.fetch_add(
            (chunk.len() / channels.max(1) as usize) as u64,
            Ordering::Relaxed,
        );
        Ok(())
    }

    fn spawn_pw_cat(rate: u32, channels: u16) -> Option<Child> {
        Command::new("pw-cat")
            .args([
                "--playback",
                "--raw",
                "--format",
                "f32",
                "--rate",
                &rate.to_string(),
                "--channels",
                &channels.max(1).to_string(),
                // A small quantum keeps pw-cat's own queue (part of the clock's
                // QUEUE_ESTIMATE) tight.
                "--latency",
                "50ms",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
    }

    fn signal(child: &Child, sig: libc::c_int) {
        // SIGSTOP/SIGCONT need no state tracking (redundant deliveries no-op).
        unsafe {
            libc::kill(child.id() as libc::pid_t, sig);
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::*;

    /// The Windows video-audio backend: an MF Source Reader → WASAPI render engine
    /// (`crate::wasapi_audio`). It replaced the WinRT `MediaPlayer`, which opens but
    /// won't *play* legacy MJPEG-in-AVI camera clips (clock frozen at 0 → silent);
    /// the Source Reader decodes their audio like it already decodes their video, so
    /// audio and video share one permissive MF layer. This is a thin front — all the
    /// WASAPI/MF work runs on the engine's own thread. Dropped on stop, which
    /// disconnects the command channel and tears the engine down.
    pub struct VideoAudio(crate::wasapi_audio::WasapiAudio);

    impl VideoAudio {
        /// Open the container's audio **paused** (the engine prerolls, then the
        /// core's `ResumeVideoAudio` starts it with the video). A path opens by
        /// URL; an archive entry's in-RAM bytes through the same `Arc`-shared
        /// buffer the video producer reads (RAM-only, one resident copy).
        pub fn open(
            input: &VideoInput,
            session_id: VideoSessionId,
            muted: bool,
        ) -> Option<VideoAudio> {
            crate::wasapi_audio::WasapiAudio::open(input, session_id, muted).map(VideoAudio)
        }

        /// Pause (session paused / rebuffering) — keeps the position.
        pub fn pause(&self) {
            self.0.pause();
        }

        /// Start/resume (session entered `Playing`).
        pub fn resume(&self) {
            self.0.resume();
        }

        /// Mute in place — rendering (and the clock) keeps running, so A/V sync is
        /// mute-independent.
        pub fn set_muted(&self, muted: bool) {
            self.0.set_muted(muted);
        }

        /// Seek to `position` (task #79 phase 6). The session treats the next
        /// near-target sample as the ack, so no completion event is needed here.
        pub fn seek(&self, position: std::time::Duration) {
            self.0.seek(position);
        }

        /// One audio clock sample: the engine's state + position right now. The
        /// core routes it to the active session (stale session ids are dropped
        /// there).
        pub fn sample(&self) -> Option<AudioClockSample> {
            self.0.sample()
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use pb_app_core::video::AudioClockState;

    /// The archive audio path: the WASAPI backend over the tone fixture's **in-RAM
    /// bytes** must construct (the MF Source Reader opens the byte stream, the
    /// engine inits) and leave `Opening` within a bounded time — no path, no disk,
    /// no hang. It settles at `Paused` (ready) on a machine with an audio endpoint,
    /// or `Failed` on a headless one (both valid; the session degrades to silent on
    /// `Failed`). The point is the byte-stream plumbing never stalls or panics.
    #[test]
    fn video_audio_opens_from_in_ram_bytes() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video/color_with_tone.mp4");
        let data = std::sync::Arc::new(std::fs::read(fixture).expect("fixture bytes"));
        let input = VideoInput::Bytes {
            data,
            name: "sub/clip.mp4".into(),
        };
        // Muted: the test must not make noise; the clock runs regardless.
        let audio = VideoAudio::open(&input, VideoSessionId(9), true).expect("engine spawns");
        let t0 = std::time::Instant::now();
        loop {
            if let Some(s) = audio.sample() {
                if s.state != AudioClockState::Opening {
                    assert!(
                        matches!(s.state, AudioClockState::Paused | AudioClockState::Failed),
                        "byte-stream audio must settle Paused (ready) or Failed, got {:?}",
                        s.state
                    );
                    break;
                }
            }
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(10),
                "the in-RAM stream source must finish opening"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Opt-in real-playback check (needs an audio endpoint + makes sound):
    /// resume the tone fixture and confirm the clock actually advances — the whole
    /// decode → WASAPI render → clock loop end to end.
    /// `PB_AUDIO_PLAY_TEST=1 cargo test -p pb-app video_audio_clock_advances -- --nocapture`
    #[test]
    fn video_audio_clock_advances_on_resume() {
        if std::env::var("PB_AUDIO_PLAY_TEST").is_err() {
            eprintln!("PB_AUDIO_PLAY_TEST not set — skipping (needs an audio device)");
            return;
        }
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video/color_with_tone.mp4");
        let input = VideoInput::Path(fixture);
        let audio = VideoAudio::open(&input, VideoSessionId(1), true).expect("engine");
        // Wait until it's ready (Paused), then resume. Skip if the engine reports
        // Failed — this host has no default render endpoint (headless / RDP), so
        // there is nothing to play; that's the environment, not a code fault.
        let t0 = std::time::Instant::now();
        loop {
            match audio.sample().map(|s| s.state) {
                Some(AudioClockState::Paused) => break,
                Some(AudioClockState::Failed) => {
                    eprintln!("no audio endpoint on this host — skipping playback assertion");
                    return;
                }
                _ => {}
            }
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(5),
                "never became ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        audio.resume();
        std::thread::sleep(std::time::Duration::from_millis(600));
        let s = audio.sample().expect("sample");
        assert_eq!(s.state, AudioClockState::Playing, "resumed → Playing");
        assert!(
            s.position >= std::time::Duration::from_millis(200),
            "the clock must advance during playback, got {:?}",
            s.position
        );
    }
}
