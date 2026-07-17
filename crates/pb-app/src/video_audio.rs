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
        /// No backend, no switch: completes immediately as refused.
        pub fn set_track(&self, _stream: i64) -> u64 {
            0
        }
        pub fn switch_result(&self, _seq: u64) -> Option<bool> {
            Some(false)
        }
        pub fn active_stream(&self) -> i64 {
            -1
        }
    }
}

#[cfg(all(unix, not(target_os = "macos"), feature = "ffvideo"))]
mod imp {
    use super::*;
    use std::io::Write;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
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
        ctl: Sender<Ctl>,
        shared: Arc<Shared>,
        feeder: Option<std::thread::JoinHandle<()>>,
        /// Sequence numbers for [`Self::set_track`] asks (the feeder echoes them back).
        switch_seq: AtomicU64,
    }

    struct Shared {
        /// Interleaved frames written to the pipe since the last (re)anchor.
        frames_written: AtomicU64,
        /// Media position (µs) of the write counter's zero.
        anchor_us: AtomicU64,
        /// The playing track's sample rate — the clock's divisor. A track switch
        /// (task #99) can change it, which is why it lives here and not on the handle.
        rate: AtomicU32,
        paused: AtomicBool,
        muted: AtomicBool,
        /// The decoder drained — the tail is in the pipe/device queue.
        ended: AtomicBool,
        failed: AtomicBool,
        /// The **FFmpeg stream index** actually decoding (`-1` = unknown) — the
        /// `TrackLocator::FfStream` currency, for the picker's tick (task #99).
        active_track: AtomicI64,
        /// Last completed `SetTrack` sequence + outcome (see the Windows engine's twin).
        switch_done: AtomicU64,
        switch_ok: AtomicBool,
    }

    enum Ctl {
        Pause,
        Resume,
        Seek(Duration),
        /// Switch to FFmpeg audio stream `stream` (task #99), completing `seq`.
        SetTrack {
            stream: usize,
            seq: u64,
        },
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
                rate: AtomicU32::new(rate),
                paused: AtomicBool::new(true),
                muted: AtomicBool::new(muted),
                ended: AtomicBool::new(false),
                failed: AtomicBool::new(false),
                active_track: AtomicI64::new(decoder.stream_index() as i64),
                switch_done: AtomicU64::new(0),
                switch_ok: AtomicBool::new(false),
            });
            // Freeze immediately: opened-paused is the contract.
            signal(&child, libc::SIGSTOP);
            let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
            let feeder = std::thread::spawn({
                let shared = shared.clone();
                let input = input.clone();
                move || feeder_loop(decoder, child, input, rate, channels, shared, ctl_rx)
            });
            Some(VideoAudio {
                session_id,
                ctl: ctl_tx,
                shared,
                feeder: Some(feeder),
                switch_seq: AtomicU64::new(0),
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

        /// Switch to **FFmpeg audio stream** `stream` (task #99) — the
        /// `TrackLocator::FfStream` currency, straight from
        /// `AppCore::audio_row_ff_stream` (this backend is FFmpeg end-to-end, so no
        /// bridge is involved). Poll the returned sequence with
        /// [`Self::switch_result`]; a refused switch leaves the current track playing.
        pub fn set_track(&self, stream: i64) -> u64 {
            let seq = self.switch_seq.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = self.ctl.send(Ctl::SetTrack {
                stream: stream.max(0) as usize,
                seq,
            });
            seq
        }

        /// The outcome of switch `seq` (`None` = still in flight). A failed backend
        /// answers `false` rather than pending forever — the session treats `Failed`
        /// as terminal, so there is nothing a switch could recover.
        pub fn switch_result(&self, seq: u64) -> Option<bool> {
            if self.shared.switch_done.load(Ordering::Acquire) >= seq {
                return Some(self.shared.switch_ok.load(Ordering::Acquire));
            }
            self.shared.failed.load(Ordering::Relaxed).then_some(false)
        }

        /// The FFmpeg stream index actually decoding (`-1` = unknown) — reported to
        /// the core via `audio_row_for_ff_stream` so the picker ticks what is really
        /// playing.
        pub fn active_stream(&self) -> i64 {
            self.shared.active_track.load(Ordering::Acquire)
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
            let rate = s.rate.load(Ordering::Relaxed);
            let written = Duration::from_secs_f64(
                s.frames_written.load(Ordering::Relaxed) as f64 / rate.max(1) as f64,
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
    /// pw-cat (dropping the old child flushes its queued audio instantly); a track
    /// switch (task #99) additionally replaces the decoder — `input` is held for
    /// exactly that reopen.
    fn feeder_loop(
        mut decoder: FfAudioDecoder,
        mut child: Child,
        input: VideoInput,
        mut rate: u32,
        mut channels: u16,
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
                Some(Ctl::SetTrack { stream, seq }) => {
                    // The audio track switch (task #99): build the ENTIRE new pipeline
                    // — decoder, seek, sink — before touching the old one, so a failed
                    // switch costs the choice, never the sound. Note `open_track` with
                    // a stale/non-audio index falls back to the policy and still
                    // succeeds; `active_track` below reports what actually plays.
                    let target = {
                        let anchor =
                            Duration::from_micros(shared.anchor_us.load(Ordering::Relaxed));
                        let written = Duration::from_secs_f64(
                            shared.frames_written.load(Ordering::Relaxed) as f64
                                / rate.max(1) as f64,
                        );
                        anchor + written
                    };
                    let next = FfAudioDecoder::open_track(&input, u16::MAX, Some(stream))
                        .ok()
                        .and_then(|mut d| d.seek(target).ok().map(|_| d))
                        .and_then(|mut d| {
                            let landing = d.read(1).unwrap_or_default();
                            let sink = spawn_pw_cat(d.rate(), d.channels())?;
                            Some((d, landing, sink))
                        });
                    let ok = match next {
                        Some((d, landing, mut sink)) => {
                            // The new pipeline is whole — now retire the old one.
                            let _ = stdin.take();
                            signal(&child, libc::SIGCONT);
                            let _ = child.kill();
                            let _ = child.wait();
                            stdin = sink.stdin.take();
                            child = sink;
                            if shared.paused.load(Ordering::Relaxed) {
                                signal(&child, libc::SIGSTOP);
                            }
                            decoder = d;
                            rate = decoder.rate();
                            channels = decoder.channels();
                            let anchor = decoder.position();
                            shared.rate.store(rate.max(1), Ordering::Relaxed);
                            shared
                                .anchor_us
                                .store(anchor.as_micros() as u64, Ordering::Relaxed);
                            shared.frames_written.store(0, Ordering::Relaxed);
                            shared.ended.store(false, Ordering::Relaxed);
                            shared
                                .active_track
                                .store(decoder.stream_index() as i64, Ordering::Relaxed);
                            if !landing.is_empty()
                                && write_chunk(&mut stdin, &landing, &shared, channels).is_err()
                            {
                                shared.failed.store(true, Ordering::Relaxed);
                            }
                            true
                        }
                        None => {
                            eprintln!(
                                "video audio: track switch failed, keeping the current track"
                            );
                            false
                        }
                    };
                    shared.switch_ok.store(ok, Ordering::Release);
                    shared.switch_done.store(seq, Ordering::Release);
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

        /// Switch to the audio stream at **MF reader stream index** `stream` (task
        /// #99) — the `TrackLocator::MfStream` currency, from
        /// `AppCore::audio_row_mf_stream`. Poll the returned sequence with
        /// [`Self::switch_result`]; a refused switch leaves the current track playing.
        pub fn set_track(&self, stream: i64) -> u64 {
            self.0.set_track(stream.max(0) as u32)
        }

        /// The outcome of switch `seq` (`None` = still in flight).
        pub fn switch_result(&self, seq: u64) -> Option<bool> {
            self.0.switch_result(seq)
        }

        /// The MF reader stream index actually decoding (`-1` = unknown) — reported
        /// to the core via `audio_row_for_mf_stream` so the picker ticks what is
        /// really playing.
        pub fn active_stream(&self) -> i64 {
            self.0.active_track()
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

    /// Audio track switching end to end (task #99): open the multitrack fixture
    /// **muted**, switch to its second MF audio stream, and require a *confirmed*
    /// outcome — then ask for a stream that doesn't exist and require a confirmed
    /// refusal with the previous track still active. Skips gracefully on a host with
    /// no audio endpoint (the engine reports `Failed`; RDP/headless).
    #[test]
    fn video_audio_switches_tracks_and_refuses_bogus_streams() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video/multitrack.mp4");
        let input = VideoInput::Path(fixture);
        let audio = VideoAudio::open(&input, VideoSessionId(3), true).expect("engine spawns");
        let t0 = std::time::Instant::now();
        loop {
            match audio.sample().map(|s| s.state) {
                Some(AudioClockState::Paused) => break,
                Some(AudioClockState::Failed) => {
                    eprintln!("no audio endpoint on this host — skipping the switch test");
                    return;
                }
                _ => {}
            }
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(10),
                "never became ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // The engine reports which stream the default open resolved to.
        let s0 = audio.active_stream();
        assert!(s0 >= 0, "the default open reports its stream");
        // The clip's OTHER audio stream, in the decoder's own (MF) namespace.
        let s1 = pb_decode::MfAudioDecoder::open_track(&input, 48_000, Some(1))
            .expect("ordinal 1 opens")
            .reader_stream()
            .expect("ordinal 1 resolves") as i64;
        assert_ne!(s0, s1);

        let wait = |seq: u64| {
            let t0 = std::time::Instant::now();
            loop {
                if let Some(ok) = audio.switch_result(seq) {
                    return ok;
                }
                assert!(
                    t0.elapsed() < std::time::Duration::from_secs(10),
                    "a switch must complete"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };

        assert!(
            wait(audio.set_track(s1)),
            "switching to a real AAC stream must be confirmed"
        );
        assert_eq!(
            audio.active_stream(),
            s1,
            "the engine reports the new stream"
        );

        assert!(
            !wait(audio.set_track(99)),
            "a stream the file hasn't got must be refused, not half-taken"
        );
        assert_eq!(
            audio.active_stream(),
            s1,
            "a refused switch leaves the previous track active"
        );
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
