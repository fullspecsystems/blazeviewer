//! WASAPI shared-mode render engine for video audio (task #79, Windows).
//!
//! The decode half is `pb_decode::MfAudioDecoder` (MF Source Reader → interleaved
//! f32); this is the **render** half: a dedicated audio thread pulls decoded PCM
//! and writes it to a shared-mode, event-driven WASAPI endpoint. It replaces the
//! WinRT `MediaPlayer` — which opens but refuses to *play* legacy MJPEG-in-AVI
//! camera clips (clock frozen at 0) — with the same permissive MF layer that
//! already decodes their video, so audio and video stay format-compatible.
//!
//! Contract mirrors the old backend exactly (`video_audio.rs` calls these):
//! `open` (paused, prerolled) / `pause` / `resume` / `set_muted` / `seek` /
//! `sample`. The engine owns the WASAPI + MF objects on its own thread (COM
//! apartment safety); the event loop only reads a lock-free clock via `sample`.
//! Mute writes silence but keeps rendering, so the clock runs muted and A/V sync
//! is mute-independent.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use pb_app_core::video::{AudioClockSample, AudioClockState, VideoInput, VideoSessionId};
use pb_decode::MfAudioDecoder;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

// ── The lock-free clock the event loop samples ──────────────────────────────

// AudioClockState as a u8 (atomic). Kept in sync with the mapping below.
const ST_OPENING: u8 = 0;
const ST_PLAYING: u8 = 1;
const ST_PAUSED: u8 = 2;
const ST_FAILED: u8 = 3;

struct Shared {
    /// One of the `ST_*` constants.
    state: AtomicU8,
    /// Media position (time into the clip) in nanoseconds.
    position_nanos: AtomicU64,
    /// The **MF reader stream index** actually being decoded (`-1` = unknown) — the
    /// `TrackLocator::MfStream` currency, so the shell can tick the picker row that is
    /// really playing (task #99). Set at open and after every confirmed switch.
    active_track: AtomicI64,
    /// The sequence of the last completed `SetTrack`, and its outcome. The engine runs
    /// on its own thread, so the shell polls [`WasapiAudio::switch_result`] rather than
    /// blocking; `switch_ok` is only meaningful for the latest sequence, and the shell
    /// only ever has one switch in flight.
    switch_done: AtomicU64,
    switch_ok: AtomicBool,
}

impl Shared {
    fn set_state(&self, s: u8) {
        self.state.store(s, Ordering::Release);
    }
    fn set_position(&self, p: Duration) {
        self.position_nanos
            .store(p.as_nanos() as u64, Ordering::Release);
    }
}

/// Commands from the shell (event loop) to the audio thread.
enum Cmd {
    Resume,
    Pause,
    SetMuted(bool),
    Seek(Duration),
    /// Switch to the audio stream at MF reader stream index `stream` (task #99),
    /// reporting completion of `seq` through `Shared::switch_done`/`switch_ok`.
    SetTrack {
        stream: u32,
        seq: u64,
    },
}

/// The public handle — a thin front for the audio thread. Dropping it disconnects
/// the command channel, which the thread treats as Stop (tearing down WASAPI/MF).
pub struct WasapiAudio {
    cmd_tx: Sender<Cmd>,
    shared: Arc<Shared>,
    session_id: VideoSessionId,
    /// Sequence numbers for [`Self::set_track`] asks (the engine echoes them back).
    switch_seq: AtomicU64,
}

impl WasapiAudio {
    /// Spawn the render thread over `input`'s audio, **paused**. Returns `None`
    /// only if the thread can't be spawned; a device/decoder failure surfaces
    /// asynchronously as an `AudioClockState::Failed` sample (the session then
    /// degrades to silent), matching the old backend's contract.
    pub fn open(
        input: &VideoInput,
        session_id: VideoSessionId,
        muted: bool,
    ) -> Option<WasapiAudio> {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            state: AtomicU8::new(ST_OPENING),
            position_nanos: AtomicU64::new(0),
            active_track: AtomicI64::new(-1),
            switch_done: AtomicU64::new(0),
            switch_ok: AtomicBool::new(false),
        });
        let thread_shared = shared.clone();
        let input = input.clone();
        std::thread::Builder::new()
            .name("pb-video-audio".into())
            .spawn(move || {
                // COM per-thread; MF is started by the decoder's `ensure_mf`.
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                if let Err(e) = run_engine(&input, muted, &thread_shared, &cmd_rx) {
                    eprintln!("video audio (WASAPI) failed: {e}");
                    thread_shared.set_state(ST_FAILED);
                }
                unsafe {
                    CoUninitialize();
                }
            })
            .ok()?;
        Some(WasapiAudio {
            cmd_tx,
            shared,
            session_id,
            switch_seq: AtomicU64::new(0),
        })
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(Cmd::Pause);
    }
    pub fn resume(&self) {
        let _ = self.cmd_tx.send(Cmd::Resume);
    }
    pub fn set_muted(&self, muted: bool) {
        let _ = self.cmd_tx.send(Cmd::SetMuted(muted));
    }
    pub fn seek(&self, position: Duration) {
        let _ = self.cmd_tx.send(Cmd::Seek(position));
    }

    /// Ask the engine to switch to the audio stream at MF reader stream index `stream`
    /// (task #99). Asynchronous: returns a sequence to poll with
    /// [`Self::switch_result`]. A refused switch leaves the current track playing —
    /// the engine's rule, not the caller's to enforce.
    pub fn set_track(&self, stream: u32) -> u64 {
        let seq = self.switch_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.cmd_tx.send(Cmd::SetTrack { stream, seq });
        seq
    }

    /// The outcome of switch `seq`, once the engine has processed it (`None` = still
    /// pending). Only the latest sequence's answer is held, which matches the caller:
    /// the shell keeps at most one switch in flight.
    ///
    /// A **failed engine answers `false`** rather than pending forever: `Failed` means
    /// the render thread exited (e.g. an AC-3 main track MF refused to decode at open —
    /// real corpus films hit this), so nothing is left to serve the ask.
    pub fn switch_result(&self, seq: u64) -> Option<bool> {
        if self.shared.switch_done.load(Ordering::Acquire) >= seq {
            return Some(self.shared.switch_ok.load(Ordering::Acquire));
        }
        (self.shared.state.load(Ordering::Acquire) == ST_FAILED).then_some(false)
    }

    /// The MF reader stream index actually being decoded (`-1` = unknown) — what the
    /// picker's tick should report, via `AppCore::audio_row_for_mf_stream`.
    pub fn active_track(&self) -> i64 {
        self.shared.active_track.load(Ordering::Acquire)
    }

    /// The current clock sample (lock-free reads). The core drops stale session ids.
    pub fn sample(&self) -> Option<AudioClockSample> {
        let state = match self.shared.state.load(Ordering::Acquire) {
            ST_PLAYING => AudioClockState::Playing,
            ST_PAUSED => AudioClockState::Paused,
            ST_FAILED => AudioClockState::Failed,
            _ => AudioClockState::Opening,
        };
        Some(AudioClockSample {
            session_id: self.session_id,
            state,
            position: Duration::from_nanos(self.shared.position_nanos.load(Ordering::Acquire)),
            sampled_at_monotonic: Duration::ZERO, // delivered immediately after sampling
        })
    }
}

// Dropping `cmd_tx` disconnects the channel; the thread sees `Disconnected` and
// tears down. No explicit join — teardown is quick and must not stall the loop.

// ── The audio thread's engine ───────────────────────────────────────────────

/// The device endpoint's sample type — WASAPI shared mode is locked to the mix
/// format, nearly always 32-bit float, but 16/32-bit PCM is handled too.
#[derive(Clone, Copy)]
enum SampleKind {
    F32,
    I16,
    I32,
}

struct DeviceFormat {
    channels: u16,
    sample_rate: u32,
    kind: SampleKind,
}

/// Build + run the render engine to completion (returns on Stop / channel
/// disconnect / a fatal error). All WASAPI + MF objects are thread-local.
fn run_engine(
    input: &VideoInput,
    mut muted: bool,
    shared: &Arc<Shared>,
    cmd_rx: &Receiver<Cmd>,
) -> Result<(), String> {
    unsafe {
        // 1. Default render endpoint + audio client + its mix format.
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(ws("CoCreateInstance"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(ws("GetDefaultAudioEndpoint"))?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(ws("Activate(IAudioClient)"))?;
        let mix_ptr = client.GetMixFormat().map_err(ws("GetMixFormat"))?;
        if mix_ptr.is_null() {
            return Err("no device mix format".into());
        }
        let fmt = device_format(mix_ptr);

        // 2. Decode the clip's audio to f32 at the device rate (channels = source).
        let decoder = MfAudioDecoder::open(input, fmt.sample_rate).map_err(|e| e.to_string())?;
        let src_channels = decoder.format().channels as usize;
        // Report which stream is actually decoding (the default open resolves it), so
        // the picker's tick starts honest rather than blank.
        shared.active_track.store(
            decoder.reader_stream().map_or(-1, |s| s as i64),
            Ordering::Release,
        );

        // 3. Initialize shared-mode, event-driven, with a ~200 ms buffer for slack.
        let buffer_hns: i64 = 2_000_000; // 200 ms in 100-ns units
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                buffer_hns,
                0,
                mix_ptr,
                None,
            )
            .map_err(ws("Initialize"))?;
        // The mix format is owned by us now (CoTaskMemAlloc'd) — but we keep reading
        // it via `fmt` (copied out already), so it can be freed. Leave it; the
        // process outlives one clip and the leak is one WAVEFORMATEX per play.

        let event = CreateEventW(None, false, false, None).map_err(ws("CreateEventW"))?;
        client.SetEventHandle(event).map_err(ws("SetEventHandle"))?;
        let render: IAudioRenderClient = client.GetService().map_err(ws("GetService"))?;
        let buffer_frames = client.GetBufferSize().map_err(ws("GetBufferSize"))?;

        let mut engine = Engine {
            client: &client,
            render: &render,
            buffer_frames,
            fmt: &fmt,
            src_channels,
            decoder,
            pending: Vec::new(),
            pending_pos: 0,
            base_position: Duration::ZERO,
            frames_since_base: 0,
            eos: false,
        };

        // 4. Preroll one full buffer while stopped, then report Paused (ready).
        engine.fill(muted)?;
        shared.set_position(Duration::ZERO);
        shared.set_state(ST_PAUSED);

        // 5. Command + render loop.
        let mut playing = false;
        loop {
            // Drain commands (a disconnect = teardown).
            loop {
                match cmd_rx.try_recv() {
                    Ok(Cmd::Resume) => {
                        if !playing {
                            client.Start().map_err(w)?;
                            playing = true;
                        }
                        shared.set_state(ST_PLAYING);
                    }
                    Ok(Cmd::Pause) => {
                        if playing {
                            let _ = client.Stop();
                            playing = false;
                        }
                        shared.set_state(ST_PAUSED);
                    }
                    Ok(Cmd::SetMuted(m)) => muted = m,
                    Ok(Cmd::Seek(pos)) => {
                        let was_playing = playing;
                        if playing {
                            let _ = client.Stop();
                            playing = false;
                        }
                        let _ = client.Reset(); // flush the queued buffer (needs Stopped)
                        engine.reseek(pos)?;
                        engine.fill(muted)?; // preroll at the new position
                        shared.set_position(pos);
                        if was_playing {
                            client.Start().map_err(w)?;
                            playing = true;
                        }
                    }
                    // The audio track switch (task #99): the Seek dance, plus swapping
                    // the decoder. The new decoder opens and seeks BEFORE playback is
                    // touched — a failed switch costs the user the choice, never the
                    // sound (the old track plays on, untouched).
                    Ok(Cmd::SetTrack { stream, seq }) => {
                        let target = engine.playhead();
                        let ok = match MfAudioDecoder::open_reader_stream(
                            input,
                            fmt.sample_rate,
                            stream,
                        )
                        .and_then(|mut next| next.seek(target).map(|()| next))
                        {
                            Ok(next) => {
                                let was_playing = playing;
                                if playing {
                                    let _ = client.Stop();
                                    playing = false;
                                }
                                let _ = client.Reset();
                                engine.swap_decoder(next, target);
                                engine.fill(muted)?; // preroll the new track at the playhead
                                shared.set_position(target);
                                shared.active_track.store(stream as i64, Ordering::Release);
                                if was_playing {
                                    client.Start().map_err(w)?;
                                    playing = true;
                                }
                                true
                            }
                            Err(e) => {
                                // AC-3/E-AC-3 land here: MF finds the stream but its
                                // decoder declines (0xC00D36B4). Keep playing.
                                eprintln!(
                                    "audio track switch failed, keeping the current track: {e}"
                                );
                                false
                            }
                        };
                        shared.switch_ok.store(ok, Ordering::Release);
                        shared.switch_done.store(seq, Ordering::Release);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if playing {
                            let _ = client.Stop();
                        }
                        let _ = CloseHandle(event);
                        return Ok(());
                    }
                }
            }

            // Wait for buffer space (event) or a ~20 ms tick to re-poll commands
            // (when stopped the event never fires).
            let _ = WaitForSingleObject(event, 20);

            // Top up the buffer whenever there is space (no-op while stopped/full).
            engine.fill(muted)?;
            engine.publish_clock(shared, playing);
        }
    }
}

/// The per-buffer render state, split out so `fill`/`reseek` borrow cleanly. Owns the
/// decoder so a track switch (task #99) can swap it without a self-borrow fight.
struct Engine<'a> {
    client: &'a IAudioClient,
    render: &'a IAudioRenderClient,
    buffer_frames: u32,
    fmt: &'a DeviceFormat,
    src_channels: usize,
    decoder: MfAudioDecoder,
    /// Decoded source-interleaved f32 not yet written, from `pending_pos`.
    pending: Vec<f32>,
    pending_pos: usize,
    /// Media time at the last seek/start; the clock base.
    base_position: Duration,
    /// Frames written to the endpoint since `base_position`.
    frames_since_base: u64,
    eos: bool,
}

impl Engine<'_> {
    /// Fill all currently-available buffer space with channel-mapped audio (zeros
    /// past EOS, or when muted). Safe to call while stopped — space is then 0.
    unsafe fn fill(&mut self, muted: bool) -> Result<(), String> {
        let padding = self.client.GetCurrentPadding().map_err(w)?;
        let avail = self.buffer_frames.saturating_sub(padding);
        if avail == 0 {
            return Ok(());
        }
        let ptr = self.render.GetBuffer(avail).map_err(w)?;
        let mut produced = 0u32;
        let mut hit_eos = false;
        let mut empty_pulls = 0;
        while produced < avail {
            let have = (self.pending.len() - self.pending_pos) / self.src_channels;
            if have == 0 {
                if self.eos {
                    break;
                }
                match self.decoder.next_chunk() {
                    Ok(Some(chunk)) if !chunk.is_empty() => {
                        // Compact and append (drop the consumed prefix).
                        self.pending.drain(..self.pending_pos);
                        self.pending_pos = 0;
                        self.pending.extend_from_slice(&chunk);
                    }
                    Ok(Some(_)) => {
                        // A gap tick (no data yet) — bounded retry, else silence.
                        empty_pulls += 1;
                        if empty_pulls > 8 {
                            break;
                        }
                    }
                    Ok(None) => {
                        self.eos = true;
                        hit_eos = true;
                        break;
                    }
                    Err(_) => {
                        self.eos = true;
                        hit_eos = true;
                        break;
                    }
                }
                continue;
            }
            let src = &self.pending[self.pending_pos..self.pending_pos + self.src_channels];
            self.write_frame(ptr, produced, src, muted);
            self.pending_pos += self.src_channels;
            produced += 1;
        }
        // Zero any remainder (EOS/underrun) so the endpoint never repeats stale data.
        if produced < avail {
            self.zero_frames(ptr, produced, avail - produced);
        }
        let flags = if muted {
            AUDCLNT_BUFFERFLAGS_SILENT.0 as u32
        } else {
            0
        };
        self.render.ReleaseBuffer(avail, flags).map_err(w)?;
        self.frames_since_base += avail as u64;
        let _ = hit_eos;
        Ok(())
    }

    /// Reposition the decoder + reset the clock base for a seek.
    unsafe fn reseek(&mut self, pos: Duration) -> Result<(), String> {
        self.decoder.seek(pos).map_err(|e| e.to_string())?;
        self.pending.clear();
        self.pending_pos = 0;
        self.base_position = pos;
        self.frames_since_base = 0;
        self.eos = false;
        Ok(())
    }

    /// Replace the decoder with one already positioned at `at` (a track switch, #99):
    /// the clock re-bases exactly as a seek does, and the channel map follows the new
    /// track — a 5.1 commentary beside a stereo main is the normal case, not an edge.
    fn swap_decoder(&mut self, next: MfAudioDecoder, at: Duration) {
        self.src_channels = next.format().channels as usize;
        self.decoder = next;
        self.pending.clear();
        self.pending_pos = 0;
        self.base_position = at;
        self.frames_since_base = 0;
        self.eos = false;
    }

    /// The media position right now: base + (written − still-queued) / rate.
    fn playhead(&self) -> Duration {
        let padding = unsafe { self.client.GetCurrentPadding() }.unwrap_or(0);
        let rendered = self.frames_since_base.saturating_sub(padding as u64);
        self.base_position
            + Duration::from_secs_f64(rendered as f64 / self.fmt.sample_rate.max(1) as f64)
    }

    /// Publish the media position.
    fn publish_clock(&self, shared: &Arc<Shared>, playing: bool) {
        shared.set_position(self.playhead());
        let _ = playing;
    }

    /// Write one output frame at `frame`, mapping source channels → device
    /// channels (`muted` → zeros). `ptr` is the raw endpoint buffer.
    unsafe fn write_frame(&self, ptr: *mut u8, frame: u32, src: &[f32], muted: bool) {
        let dev_ch = self.fmt.channels as usize;
        for c in 0..dev_ch {
            let v = if muted { 0.0 } else { map_sample(src, c) };
            self.write_sample(ptr, (frame as usize) * dev_ch + c, v);
        }
    }

    unsafe fn zero_frames(&self, ptr: *mut u8, start_frame: u32, count: u32) {
        let dev_ch = self.fmt.channels as usize;
        for f in 0..count as usize {
            for c in 0..dev_ch {
                self.write_sample(ptr, (start_frame as usize + f) * dev_ch + c, 0.0);
            }
        }
    }

    unsafe fn write_sample(&self, ptr: *mut u8, index: usize, v: f32) {
        match self.fmt.kind {
            SampleKind::F32 => *(ptr as *mut f32).add(index) = v,
            SampleKind::I16 => {
                *(ptr as *mut i16).add(index) = (v.clamp(-1.0, 1.0) * 32767.0) as i16
            }
            SampleKind::I32 => {
                *(ptr as *mut i32).add(index) = (v.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32
            }
        }
    }
}

/// Map a source frame's channel `out_ch` for the device: mono → both fronts,
/// otherwise pass matching channels through and silence any extra device channels.
/// A surround source into a stereo device takes the front L/R (no matrix downmix —
/// safe, no clipping; a proper Lo/Ro downmix is a later refinement).
fn map_sample(src: &[f32], out_ch: usize) -> f32 {
    match src.len() {
        0 => 0.0,
        1 => {
            if out_ch < 2 {
                src[0]
            } else {
                0.0
            }
        }
        _ => *src.get(out_ch).unwrap_or(&0.0),
    }
}

/// Read the endpoint mix format into our own `DeviceFormat` (so the raw pointer
/// need not outlive this call). Handles `WAVEFORMATEXTENSIBLE` (the common shape)
/// and the plain tags.
unsafe fn device_format(p: *const WAVEFORMATEX) -> DeviceFormat {
    let wf = &*p;
    let channels = wf.nChannels;
    let sample_rate = wf.nSamplesPerSec;
    let bits = wf.wBitsPerSample;
    let is_float = if wf.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 {
        // WAVEFORMATEXTENSIBLE is 1-byte packed; read the GUID unaligned rather
        // than taking a reference to the packed field (UB / E0793).
        let ext = p as *const WAVEFORMATEXTENSIBLE;
        let sub = std::ptr::addr_of!((*ext).SubFormat).read_unaligned();
        sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        wf.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16
    };
    let kind = if is_float {
        SampleKind::F32
    } else if bits == 16 {
        SampleKind::I16
    } else {
        SampleKind::I32
    };
    DeviceFormat {
        channels,
        sample_rate,
        kind,
    }
}

fn w(e: windows::core::Error) -> String {
    format!("WASAPI: {e}")
}

/// Step-labeled error mapper: names the WASAPI call that failed, so a device
/// setup failure points at the exact stage rather than a bare HRESULT.
fn ws(step: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |e| format!("WASAPI {step}: {e}")
}
