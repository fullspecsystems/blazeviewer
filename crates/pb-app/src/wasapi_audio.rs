//! WASAPI shared-mode render engine for video audio (task #79, Windows).
//!
//! The **render** half of video audio: a dedicated audio thread pulls decoded PCM
//! from a decoder and writes it to a shared-mode, event-driven WASAPI endpoint.
//! The engine owns the WASAPI + decoder objects on its own thread (COM apartment
//! safety); the event loop only reads a lock-free clock via `sample`. Mute writes
//! silence but keeps rendering, so the clock runs muted and A/V sync is
//! mute-independent.
//!
//! ## Two decode backends, FFmpeg first (task #99 follow-up, 2026-07-17)
//!
//! Media Foundation cannot decode the codecs most films carry — AC-3, E-AC-3 and
//! DTS all fail `SetCurrentMediaType` with `0xC00D36B4` — while the Windows FFmpeg
//! tree ships every audio decoder already (the #100 trim kept them for
//! channel-layout naming; decoding Apollo 13's AC-3 5.1 was measured at ~100×
//! real-time over SMB). So under the `ffprobe` feature the engine decodes with
//! [`FfAudioDecoder`] — which also brings the R10 track policy (the *authored
//! default* track, not "whatever enumerates first") — and falls back to
//! [`MfAudioDecoder`] for anything the trimmed demuxer list can't open. Without
//! `ffprobe` (a plain dev build) MF remains the only decoder, as before. MF's
//! WinRT predecessor is long gone: the Source Reader decodes the legacy
//! MJPEG-in-AVI/PCM clips the `MediaPlayer` refused to play, and FFmpeg decodes
//! them too.
//!
//! ## The sink speaks the decoder's format
//!
//! `MfAudioDecoder` resamples to any requested rate, but `FfAudioDecoder` emits
//! the source's own rate — so the client is initialized with the **source's**
//! rate/channels plus `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`, letting the OS audio
//! engine insert the sample-rate converter and channel matrix (a proper
//! center-aware downmix on a stereo device — better than the front-L/R-only map
//! the mix-format path uses). If the device refuses that format, the engine falls
//! back to the mix format + our own channel map, exactly as it always did. A
//! track switch that changes the source format rebuilds the sink; a failed
//! rebuild refuses the switch and keeps the old track playing.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use pb_app_core::video::{AudioClockSample, AudioClockState, VideoInput, VideoSessionId};
#[cfg(feature = "ffprobe")]
use pb_decode::FfAudioDecoder;
use pb_decode::MfAudioDecoder;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// `PB_AUDIO_TRACE=1` — one-line diagnostics for the render engine: decoder
/// choice, sink mode, underruns, switch commits. A native audio pipeline is
/// otherwise invisible to everything but a listener.
fn trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PB_AUDIO_TRACE").is_ok_and(|v| v != "0"))
}

fn atrace(msg: impl FnOnce() -> String) {
    if trace_on() {
        eprintln!("[pb-audio] {}", msg());
    }
}

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
    /// The **FFmpeg stream index** actually being decoded (`-1` = the decoder isn't
    /// FFmpeg) — one of the two currencies the picker's tick resolves through
    /// (task #99). Set at open and after every confirmed switch.
    active_ff: AtomicI64,
    /// The **MF reader stream index** actually being decoded (`-1` = the decoder
    /// isn't MF) — the other currency.
    active_mf: AtomicI64,
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
    fn set_active(&self, (ff, mf): (i64, i64)) {
        self.active_ff.store(ff, Ordering::Release);
        self.active_mf.store(mf, Ordering::Release);
    }
}

/// Commands from the shell (event loop) to the audio thread.
enum Cmd {
    Resume,
    Pause,
    SetMuted(bool),
    Seek(Duration),
    /// Switch to the audio stream `ff` (FFmpeg index) / `mf` (MF reader stream) —
    /// whichever currency the row carries; `-1` = not located in that currency
    /// (task #99). Completion of `seq` lands in `Shared::switch_done`/`switch_ok`.
    SetTrack {
        ff: i64,
        mf: i64,
        seq: u64,
    },
}

/// The public handle — a thin front for the audio thread. Dropping it disconnects
/// the command channel, which the thread treats as Stop (tearing down WASAPI and
/// the decoder).
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
            active_ff: AtomicI64::new(-1),
            active_mf: AtomicI64::new(-1),
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

    /// Ask the engine to switch to the audio stream a picker row named — `ff` and
    /// `mf` are the row's two possible currencies (`-1` = not located in that one);
    /// the engine uses whichever its decoders can serve, FFmpeg first (task #99).
    /// Asynchronous: poll the returned sequence with [`Self::switch_result`]. A
    /// refused switch leaves the current track playing — the engine's rule.
    pub fn set_track(&self, ff: i64, mf: i64) -> u64 {
        let seq = self.switch_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.cmd_tx.send(Cmd::SetTrack { ff, mf, seq });
        seq
    }

    /// The outcome of switch `seq`, once the engine has processed it (`None` = still
    /// pending). Only the latest sequence's answer is held, which matches the caller:
    /// the shell keeps at most one switch in flight.
    ///
    /// A **failed engine answers `false`** rather than pending forever: `Failed` means
    /// the render thread exited (no decoder could open the clip's audio at all), so
    /// nothing is left to serve the ask.
    pub fn switch_result(&self, seq: u64) -> Option<bool> {
        if self.shared.switch_done.load(Ordering::Acquire) >= seq {
            return Some(self.shared.switch_ok.load(Ordering::Acquire));
        }
        (self.shared.state.load(Ordering::Acquire) == ST_FAILED).then_some(false)
    }

    /// The FFmpeg stream index actually being decoded (`-1` = the active decoder
    /// isn't FFmpeg) — resolved to a picker row via `AppCore::audio_row_for_ff_stream`.
    pub fn active_ff_stream(&self) -> i64 {
        self.shared.active_ff.load(Ordering::Acquire)
    }

    /// The MF reader stream index actually being decoded (`-1` = the active decoder
    /// isn't MF) — resolved via `AppCore::audio_row_for_mf_stream`.
    pub fn active_mf_stream(&self) -> i64 {
        self.shared.active_mf.load(Ordering::Acquire)
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

// ── The decoder seam (task #99: FFmpeg first, MF fallback) ──────────────────

/// One frame-pull audio decoder, whichever backend opened the clip. Lives on the
/// engine thread only. (The FFmpeg decoder is boxed for the clippy size lint —
/// one allocation per clip, nowhere near any hot path.)
enum AudioDecoder {
    Mf(MfAudioDecoder),
    #[cfg(feature = "ffprobe")]
    Ff(Box<FfAudioDecoder>),
}

/// Frames per FFmpeg pull (~50 ms @ 48 kHz) — the same chunking the Linux feeder
/// uses; MF picks its own sample sizes.
#[cfg(feature = "ffprobe")]
const FF_CHUNK_FRAMES: usize = 2400;

impl AudioDecoder {
    fn rate(&self) -> u32 {
        match self {
            AudioDecoder::Mf(d) => d.format().sample_rate,
            #[cfg(feature = "ffprobe")]
            AudioDecoder::Ff(d) => d.rate(),
        }
    }

    fn channels(&self) -> u16 {
        match self {
            AudioDecoder::Mf(d) => d.format().channels,
            #[cfg(feature = "ffprobe")]
            AudioDecoder::Ff(d) => d.channels(),
        }
    }

    /// Interleaved f32. `Ok(None)` = EOS; an empty `Some` is a gap tick (MF only).
    fn next_chunk(&mut self) -> Result<Option<Vec<f32>>, String> {
        match self {
            AudioDecoder::Mf(d) => d.next_chunk().map_err(|e| e.to_string()),
            #[cfg(feature = "ffprobe")]
            AudioDecoder::Ff(d) => match d.read(FF_CHUNK_FRAMES) {
                Ok(chunk) if chunk.is_empty() => Ok(None), // FFmpeg's EOS shape
                Ok(chunk) => Ok(Some(chunk)),
                Err(e) => Err(e.to_string()),
            },
        }
    }

    fn seek(&mut self, pos: Duration) -> Result<(), String> {
        match self {
            AudioDecoder::Mf(d) => d.seek(pos).map_err(|e| e.to_string()),
            #[cfg(feature = "ffprobe")]
            AudioDecoder::Ff(d) => d.seek(pos).map(|_| ()).map_err(|e| e.to_string()),
        }
    }

    /// The playing stream in each currency: `(ffmpeg_index, mf_reader_stream)`,
    /// `-1` for the currency this decoder doesn't speak. Read back from the
    /// decoder, never remembered — the tick must report what *plays*.
    fn active(&self) -> (i64, i64) {
        match self {
            AudioDecoder::Mf(d) => (-1, d.reader_stream().map_or(-1, |s| s as i64)),
            #[cfg(feature = "ffprobe")]
            AudioDecoder::Ff(d) => (d.stream_index() as i64, -1),
        }
    }

    /// The emitted layout's speaker mask (WAVE bit values), where the container
    /// declared one — what the direct sink's `WAVEFORMATEXTENSIBLE` carries so
    /// >2-channel audio is unambiguous to the OS converter.
    fn layout_mask(&self) -> Option<u64> {
        match self {
            AudioDecoder::Mf(_) => None, // MF's native types expose no mask (measured)
            #[cfg(feature = "ffprobe")]
            AudioDecoder::Ff(d) => d.layout_mask(),
        }
    }
}

/// Carries a freshly opened decoder from the switch worker back to the engine
/// thread. Soundness mirrors `mf_poster::retire_reader`: both threads are
/// MTA-initialized and MF source readers are free-threaded, so the cross-thread
/// move is within the COM rules; the FFmpeg decoder is `Send` in its own right.
struct SendDecoder(AudioDecoder);
unsafe impl Send for SendDecoder {}

/// An off-thread track-switch open in flight (task #99). The open can block for
/// seconds on a network share, so the engine keeps rendering the current track
/// and commits when this resolves — against a *fresh* playhead, so the master
/// clock (which the video follows) never jumps back by the open's duration.
struct PendingOpen {
    rx: Receiver<Result<SendDecoder, String>>,
    seq: u64,
}

/// Open the clip's **default** audio: FFmpeg first (it decodes the film codecs MF
/// refuses, and its R10 policy picks the *authored default* track rather than the
/// first stream), MF as the fallback for containers the trimmed demuxer list
/// can't open. `mf_rate` is the resample target the MF decoder needs.
fn open_default_decoder(input: &VideoInput, mf_rate: u32) -> Result<AudioDecoder, String> {
    #[cfg(feature = "ffprobe")]
    match FfAudioDecoder::open(input) {
        Ok(d) if d.rate() > 0 && d.channels() > 0 => return Ok(AudioDecoder::Ff(Box::new(d))),
        Ok(_) => eprintln!("video audio: FFmpeg reported a zero format, trying Media Foundation"),
        Err(e) => {
            eprintln!("video audio: FFmpeg can't open this audio, trying Media Foundation: {e}")
        }
    }
    MfAudioDecoder::open(input, mf_rate)
        .map(AudioDecoder::Mf)
        .map_err(|e| e.to_string())
}

/// Open the audio stream a picker row named, in whichever currency it carries:
/// FFmpeg (`ff` ≥ 0) first, MF (`mf` ≥ 0) as the fallback. `Err` when neither
/// currency can be served — the switch is then refused.
fn open_track_decoder(
    input: &VideoInput,
    mf_rate: u32,
    ff: i64,
    mf: i64,
) -> Result<AudioDecoder, String> {
    #[cfg(feature = "ffprobe")]
    if ff >= 0 {
        // A stale index falls back to the policy inside `open_track` (a failed
        // switch should cost the choice, not the sound); `active()` reports what
        // actually plays either way.
        match FfAudioDecoder::open_track(input, u16::MAX, Some(ff as usize)) {
            Ok(d) if d.rate() > 0 && d.channels() > 0 => return Ok(AudioDecoder::Ff(Box::new(d))),
            Ok(_) => eprintln!("video audio: FFmpeg reported a zero format for the switch"),
            Err(e) => eprintln!("video audio: FFmpeg switch failed, trying Media Foundation: {e}"),
        }
    }
    #[cfg(not(feature = "ffprobe"))]
    let _ = ff;
    if mf >= 0 {
        return MfAudioDecoder::open_reader_stream(input, mf_rate, mf as u32)
            .map(AudioDecoder::Mf)
            .map_err(|e| e.to_string());
    }
    Err("this track is not reachable by any available decoder".into())
}

// ── The audio thread's engine ───────────────────────────────────────────────

/// The sink endpoint's sample type. The direct (source-format) path is always
/// f32; the mix-format fallback is nearly always f32 too, but 16/32-bit PCM
/// endpoints are handled.
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

/// One initialized WASAPI render client. Rebuilt on a track switch that changes
/// the source format (the client's format is fixed at `Initialize`).
struct Sink {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    buffer_frames: u32,
    /// The format frames are **written** in — the source's own on the direct
    /// path, the device mix format on the fallback.
    fmt: DeviceFormat,
    /// Direct = initialized with the source format + `AUTOCONVERTPCM` (the OS
    /// resamples and channel-mixes). False = mix-format fallback (we map
    /// channels ourselves; no resampling — the MF decoder covers that case by
    /// resampling decoder-side).
    direct: bool,
}

impl Sink {
    /// Build a sink for a source of `rate`/`channels` (+ its speaker `mask`, if
    /// declared): the direct path first, the mix-format fallback if the device
    /// refuses.
    unsafe fn build(
        device: &IMMDevice,
        rate: u32,
        channels: u16,
        mask: Option<u64>,
    ) -> Result<Sink, String> {
        // `PB_AUDIO_MIX_SINK=1` — diagnostic lever: skip the direct path entirely,
        // so a suspected OS-converter problem can be A/B'd in one relaunch.
        if std::env::var("PB_AUDIO_MIX_SINK").is_ok_and(|v| v != "0") {
            atrace(|| "PB_AUDIO_MIX_SINK set: forcing the mix-format sink".into());
            return Self::build_mix(device);
        }
        match Self::build_direct(device, rate, channels, mask) {
            Ok(sink) => Ok(sink),
            Err(e) => {
                eprintln!("video audio: source-format sink refused ({e}); using the mix format");
                Self::build_mix(device)
            }
        }
    }

    /// The direct path: our own f32 format at the source rate/channel count,
    /// with the OS audio engine converting to the device (sample-rate converter
    /// + channel matrix — a proper center-aware downmix on stereo devices).
    ///
    /// Anything past stereo goes as **`WAVEFORMATEXTENSIBLE` with a speaker
    /// mask** — a bare `WAVEFORMATEX` is ambiguous about which speaker each
    /// channel is, and the converter's guess is exactly the kind of subtle wrong
    /// that reads as glitchy/garbled multichannel (owner-hit 2026-07-17). The
    /// mask is the container's own where declared (FFmpeg's native-order bits
    /// are WAVE bits), else the standard layout for the count.
    unsafe fn build_direct(
        device: &IMMDevice,
        rate: u32,
        channels: u16,
        mask: Option<u64>,
    ) -> Result<Sink, String> {
        if rate == 0 || channels == 0 {
            return Err("zero source format".into());
        }
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(ws("Activate(IAudioClient)"))?;
        let block_align = channels as u32 * 4;
        let fmt = DeviceFormat {
            channels,
            sample_rate: rate,
            kind: SampleKind::F32,
        };
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if channels <= 2 {
            let wf = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
                nChannels: channels,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * block_align,
                nBlockAlign: block_align as u16,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            return Self::finish(client, flags, std::ptr::addr_of!(wf), fmt, true);
        }
        let ext = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                nChannels: channels,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * block_align,
                nBlockAlign: block_align as u16,
                wBitsPerSample: 32,
                cbSize: 22, // sizeof(WAVEFORMATEXTENSIBLE) - sizeof(WAVEFORMATEX)
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            dwChannelMask: mask.unwrap_or_else(|| default_speaker_mask(channels)) as u32,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };
        Self::finish(client, flags, std::ptr::addr_of!(ext.Format), fmt, true)
    }

    /// The fallback: the device mix format, channels mapped by us (as this
    /// engine always did before the direct path existed).
    unsafe fn build_mix(device: &IMMDevice) -> Result<Sink, String> {
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(ws("Activate(IAudioClient)"))?;
        let mix_ptr = client.GetMixFormat().map_err(ws("GetMixFormat"))?;
        if mix_ptr.is_null() {
            return Err("no device mix format".into());
        }
        let fmt = device_format(mix_ptr);
        // The mix format is CoTaskMemAlloc'd and technically ours to free; it is
        // deliberately leaked (one small allocation per sink) as before.
        Self::finish(
            client,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            mix_ptr,
            fmt,
            false,
        )
    }

    unsafe fn finish(
        client: IAudioClient,
        flags: u32,
        wf: *const WAVEFORMATEX,
        fmt: DeviceFormat,
        direct: bool,
    ) -> Result<Sink, String> {
        // ~200 ms buffer for slack, in 100-ns units.
        let buffer_hns: i64 = 2_000_000;
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, buffer_hns, 0, wf, None)
            .map_err(ws("Initialize"))?;
        let event = CreateEventW(None, false, false, None).map_err(ws("CreateEventW"))?;
        client.SetEventHandle(event).map_err(ws("SetEventHandle"))?;
        let render: IAudioRenderClient = client.GetService().map_err(ws("GetService"))?;
        let buffer_frames = client.GetBufferSize().map_err(ws("GetBufferSize"))?;
        Ok(Sink {
            client,
            render,
            event,
            buffer_frames,
            fmt,
            direct,
        })
    }

    /// Stop + release. The COM objects drop with the struct; the event handle is
    /// the one raw resource to close.
    unsafe fn close(self) {
        let _ = self.client.Stop();
        let _ = CloseHandle(self.event);
    }
}

/// The standard speaker layout for a channel count (WAVE mask bits), used when
/// the container declared none. Side-vs-back surround ambiguity is harmless —
/// either way the surrounds land on surround speakers.
fn default_speaker_mask(channels: u16) -> u64 {
    match channels {
        1 => 0x4,   // FC
        2 => 0x3,   // FL FR
        3 => 0x7,   // FL FR FC
        4 => 0x33,  // quad
        5 => 0x37,  // 5.0
        6 => 0x3F,  // 5.1
        7 => 0x13F, // 6.1
        8 => 0x63F, // 7.1
        _ => (1 << channels.min(18)) - 1,
    }
}

/// The device mix rate — the resample target handed to the MF decoder (which
/// converts decoder-side; the FFmpeg decoder doesn't need one, the direct sink
/// takes its native rate).
unsafe fn mix_rate(device: &IMMDevice) -> Result<u32, String> {
    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(ws("Activate(IAudioClient)"))?;
    let mix_ptr = client.GetMixFormat().map_err(ws("GetMixFormat"))?;
    if mix_ptr.is_null() {
        return Err("no device mix format".into());
    }
    Ok((*mix_ptr).nSamplesPerSec)
}

/// Build + run the render engine to completion (returns on Stop / channel
/// disconnect / a fatal error). All WASAPI + decoder objects are thread-local.
fn run_engine(
    input: &VideoInput,
    mut muted: bool,
    shared: &Arc<Shared>,
    cmd_rx: &Receiver<Cmd>,
) -> Result<(), String> {
    unsafe {
        // 1. Default render endpoint.
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(ws("CoCreateInstance"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(ws("GetDefaultAudioEndpoint"))?;
        let mf_rate = mix_rate(&device)?;

        // 2. The clip's default audio track (FFmpeg first — see module docs).
        let decoder = open_default_decoder(input, mf_rate)?;
        shared.set_active(decoder.active());

        // 3. A sink in the decoder's own format (mix-format fallback inside).
        let mut sink = Sink::build(
            &device,
            decoder.rate(),
            decoder.channels(),
            decoder.layout_mask(),
        )?;
        atrace(|| {
            let (ff, mf) = decoder.active();
            format!(
                "open: decoder={} ff={ff} mf={mf} {} Hz {} ch mask={:#x?} sink={}",
                match &decoder {
                    AudioDecoder::Mf(_) => "MF",
                    #[cfg(feature = "ffprobe")]
                    AudioDecoder::Ff(_) => "FFmpeg",
                },
                decoder.rate(),
                decoder.channels(),
                decoder.layout_mask(),
                if sink.direct { "direct" } else { "mix" },
            )
        });

        let mut engine = Engine {
            src_channels: decoder.channels() as usize,
            decoder,
            pending: Vec::new(),
            pending_pos: 0,
            base_position: Duration::ZERO,
            frames_since_base: 0,
            eos: false,
            underruns: 0,
        };

        // 4. Preroll one full buffer while stopped, then report Paused (ready).
        engine.fill(&sink, muted)?;
        shared.set_position(Duration::ZERO);
        shared.set_state(ST_PAUSED);

        // 5. Command + render loop.
        let mut playing = false;
        let mut pending_open: Option<PendingOpen> = None;
        loop {
            // Drain commands (a disconnect = teardown).
            loop {
                match cmd_rx.try_recv() {
                    Ok(Cmd::Resume) => {
                        if !playing {
                            sink.client.Start().map_err(w)?;
                            playing = true;
                        }
                        shared.set_state(ST_PLAYING);
                    }
                    Ok(Cmd::Pause) => {
                        if playing {
                            let _ = sink.client.Stop();
                            playing = false;
                        }
                        shared.set_state(ST_PAUSED);
                    }
                    Ok(Cmd::SetMuted(m)) => muted = m,
                    Ok(Cmd::Seek(pos)) => {
                        // Wall time of the reseek — the term that stacks on the core's
                        // settle residual (PB_AV_SYNC) to make the felt A/V gap. The
                        // decoder seek is where SMB shows up (FfAudioDecoder does an
                        // avformat_seek_file + decode-forward to the target).
                        let t0 = trace_on().then(std::time::Instant::now);
                        let was_playing = playing;
                        if playing {
                            let _ = sink.client.Stop();
                            playing = false;
                        }
                        let _ = sink.client.Reset(); // flush the queued buffer (needs Stopped)
                        let t_seek = trace_on().then(std::time::Instant::now);
                        engine.reseek(pos)?;
                        let seek_wall = t_seek.map(|t| t.elapsed());
                        engine.fill(&sink, muted)?; // preroll at the new position
                        shared.set_position(pos);
                        if was_playing {
                            sink.client.Start().map_err(w)?;
                            playing = true;
                        }
                        if let Some(t0) = t0 {
                            atrace(|| {
                                format!(
                                    "reseek to {:.2}s: decoder seek {:?}, total (stop+reset+seek+preroll) {:?}",
                                    pos.as_secs_f64(),
                                    seek_wall.unwrap_or_default(),
                                    t0.elapsed(),
                                )
                            });
                        }
                    }
                    // The audio track switch (task #99). The open can block for
                    // SECONDS on a network share (avformat open + stream info), so
                    // it runs on a scratch thread while the CURRENT track keeps
                    // rendering — blocking here drained the buffer (audible glitch)
                    // AND froze the master clock the video follows (owner-hit:
                    // jerky playback after every switch). The commit below re-aims
                    // at a FRESH playhead for the same reason.
                    Ok(Cmd::SetTrack { ff, mf, seq }) => {
                        // Supersede an older in-flight open: the shell only ever
                        // tracks its newest ask. Dropping the receiver makes the
                        // worker's send fail; its decoder drops on its own thread.
                        if let Some(old) = pending_open.take() {
                            shared.switch_ok.store(false, Ordering::Release);
                            shared.switch_done.store(old.seq, Ordering::Release);
                        }
                        atrace(|| format!("switch asked: ff={ff} mf={mf} (seq {seq})"));
                        let (tx, rx) = std::sync::mpsc::channel();
                        let worker_input = input.clone();
                        let spawned = std::thread::Builder::new()
                            .name("pb-audio-switch".into())
                            .spawn(move || {
                                // MTA for the MF-fallback open; the decoder is
                                // carried back under the retire_reader rules.
                                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                                let _ = tx.send(
                                    open_track_decoder(&worker_input, mf_rate, ff, mf)
                                        .map(SendDecoder),
                                );
                                CoUninitialize();
                            });
                        match spawned {
                            Ok(_) => pending_open = Some(PendingOpen { rx, seq }),
                            Err(e) => {
                                eprintln!("audio track switch failed to start: {e}");
                                shared.switch_ok.store(false, Ordering::Release);
                                shared.switch_done.store(seq, Ordering::Release);
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        sink.close();
                        return Ok(());
                    }
                }
            }

            // A finished off-thread switch open? Commit it — against the playhead
            // as it is NOW, not as it was when the switch was asked.
            if let Some(p) = &pending_open {
                match p.rx.try_recv() {
                    Ok(result) => {
                        let seq = p.seq;
                        pending_open = None;
                        let ok = match result {
                            Ok(SendDecoder(next)) => match commit_switch(
                                &device,
                                next,
                                &mut sink,
                                &mut engine,
                                &mut playing,
                                muted,
                                shared,
                            ) {
                                Ok(()) => true,
                                Err(e) => {
                                    eprintln!(
                                        "audio track switch failed, keeping the current track: {e}"
                                    );
                                    false
                                }
                            },
                            Err(e) => {
                                eprintln!(
                                    "audio track switch failed, keeping the current track: {e}"
                                );
                                false
                            }
                        };
                        atrace(|| format!("switch seq {seq}: ok={ok}"));
                        shared.switch_ok.store(ok, Ordering::Release);
                        shared.switch_done.store(seq, Ordering::Release);
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        let seq = p.seq;
                        pending_open = None;
                        shared.switch_ok.store(false, Ordering::Release);
                        shared.switch_done.store(seq, Ordering::Release);
                    }
                }
            }

            // Wait for buffer space (event) or a ~20 ms tick to re-poll commands
            // (when stopped the event never fires).
            let _ = WaitForSingleObject(sink.event, 20);

            // Top up the buffer whenever there is space (no-op while stopped/full).
            engine.fill(&sink, muted)?;
            engine.publish_clock(&sink, shared);
        }
    }
}

/// Commit a switch whose decoder finished opening off-thread: seek it to the
/// **current** playhead (the old track kept playing during the open), rebuild
/// the sink if the source format changed — built *before* anything old is torn
/// down — then swap, preroll, and restart. `Err` = the switch is refused and the
/// old track plays on untouched (except a `fill` failure after the swap, which
/// is a device-level fault the next loop iteration surfaces anyway).
unsafe fn commit_switch(
    device: &IMMDevice,
    mut next: AudioDecoder,
    sink: &mut Sink,
    engine: &mut Engine,
    playing: &mut bool,
    muted: bool,
    shared: &Arc<Shared>,
) -> Result<(), String> {
    let target = engine.playhead(sink);
    next.seek(target)?;
    let fmt_changed =
        (next.rate(), next.channels()) != (engine.decoder.rate(), engine.decoder.channels());
    // The direct sink speaks the old track's format; a format change needs a new
    // one. The mix-format fallback adapts (its channel map reads `src_channels`).
    let new_sink = if fmt_changed && sink.direct {
        Some(Sink::build(
            device,
            next.rate(),
            next.channels(),
            next.layout_mask(),
        )?)
    } else {
        None
    };
    let was_playing = *playing;
    if *playing {
        let _ = sink.client.Stop();
        *playing = false;
    }
    if let Some(fresh) = new_sink {
        let old = std::mem::replace(sink, fresh);
        old.close();
    } else {
        let _ = sink.client.Reset();
    }
    engine.swap_decoder(next, target);
    engine.fill(sink, muted)?; // preroll the new track at the playhead
    shared.set_position(target);
    shared.set_active(engine.decoder.active());
    atrace(|| {
        let (ff, mf) = engine.decoder.active();
        format!(
            "switch committed at {target:?}: ff={ff} mf={mf} {} Hz {} ch sink={}",
            engine.decoder.rate(),
            engine.decoder.channels(),
            if sink.direct { "direct" } else { "mix" },
        )
    });
    if was_playing {
        sink.client.Start().map_err(w)?;
        *playing = true;
    }
    Ok(())
}

/// The per-buffer render state. Owns the decoder so a track switch can swap it;
/// the sink is passed in because a switch can swap *that* too.
struct Engine {
    src_channels: usize,
    decoder: AudioDecoder,
    /// Decoded source-interleaved f32 not yet written, from `pending_pos`.
    pending: Vec<f32>,
    pending_pos: usize,
    /// Media time at the last seek/start; the clock base.
    base_position: Duration,
    /// Frames written to the endpoint since `base_position`.
    frames_since_base: u64,
    eos: bool,
    /// Fills that came up short of the available space with the decoder still
    /// live — the decoder not keeping real-time. Diagnostic (`PB_AUDIO_TRACE`).
    underruns: u64,
}

impl Engine {
    /// Fill all currently-available buffer space with channel-mapped audio (zeros
    /// past EOS, or when muted). Safe to call while stopped — space is then 0.
    unsafe fn fill(&mut self, sink: &Sink, muted: bool) -> Result<(), String> {
        let padding = sink.client.GetCurrentPadding().map_err(w)?;
        let avail = sink.buffer_frames.saturating_sub(padding);
        if avail == 0 {
            return Ok(());
        }
        let ptr = sink.render.GetBuffer(avail).map_err(w)?;
        let mut produced = 0u32;
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
                    Ok(None) | Err(_) => {
                        self.eos = true;
                        break;
                    }
                }
                continue;
            }
            let src = &self.pending[self.pending_pos..self.pending_pos + self.src_channels];
            write_frame(sink, ptr, produced, src, muted);
            self.pending_pos += self.src_channels;
            produced += 1;
        }
        // Zero any remainder (EOS/underrun) so the endpoint never repeats stale data.
        if produced < avail {
            zero_frames(sink, ptr, produced, avail - produced);
            if !self.eos {
                self.underruns += 1;
                let n = self.underruns;
                if n == 1 || n.is_multiple_of(50) {
                    atrace(|| format!("underrun #{n}: decoder produced {produced}/{avail} frames"));
                }
            }
        }
        let flags = if muted {
            AUDCLNT_BUFFERFLAGS_SILENT.0 as u32
        } else {
            0
        };
        sink.render.ReleaseBuffer(avail, flags).map_err(w)?;
        self.frames_since_base += avail as u64;
        Ok(())
    }

    /// Reposition the decoder + reset the clock base for a seek.
    fn reseek(&mut self, pos: Duration) -> Result<(), String> {
        self.decoder.seek(pos)?;
        self.pending.clear();
        self.pending_pos = 0;
        self.base_position = pos;
        self.frames_since_base = 0;
        self.eos = false;
        Ok(())
    }

    /// Replace the decoder with one already positioned at `at` (a track switch,
    /// #99): the clock re-bases exactly as a seek does, and the channel count
    /// follows the new track — a 5.1 main beside a stereo commentary is the
    /// normal case, not an edge.
    fn swap_decoder(&mut self, next: AudioDecoder, at: Duration) {
        self.src_channels = next.channels() as usize;
        self.decoder = next;
        self.pending.clear();
        self.pending_pos = 0;
        self.base_position = at;
        self.frames_since_base = 0;
        self.eos = false;
    }

    /// The media position right now: base + (written − still-queued) / rate.
    fn playhead(&self, sink: &Sink) -> Duration {
        let padding = unsafe { sink.client.GetCurrentPadding() }.unwrap_or(0);
        let rendered = self.frames_since_base.saturating_sub(padding as u64);
        self.base_position
            + Duration::from_secs_f64(rendered as f64 / sink.fmt.sample_rate.max(1) as f64)
    }

    /// Publish the media position.
    fn publish_clock(&self, sink: &Sink, shared: &Arc<Shared>) {
        shared.set_position(self.playhead(sink));
    }
}

/// Write one output frame at `frame`, mapping source channels → sink channels
/// (`muted` → zeros). On the direct path the two counts are equal and this is a
/// pass-through. `ptr` is the raw endpoint buffer.
unsafe fn write_frame(sink: &Sink, ptr: *mut u8, frame: u32, src: &[f32], muted: bool) {
    let dev_ch = sink.fmt.channels as usize;
    for c in 0..dev_ch {
        let v = if muted { 0.0 } else { map_sample(src, c) };
        write_sample(sink, ptr, (frame as usize) * dev_ch + c, v);
    }
}

unsafe fn zero_frames(sink: &Sink, ptr: *mut u8, start_frame: u32, count: u32) {
    let dev_ch = sink.fmt.channels as usize;
    for f in 0..count as usize {
        for c in 0..dev_ch {
            write_sample(sink, ptr, (start_frame as usize + f) * dev_ch + c, 0.0);
        }
    }
}

unsafe fn write_sample(sink: &Sink, ptr: *mut u8, index: usize, v: f32) {
    match sink.fmt.kind {
        SampleKind::F32 => *(ptr as *mut f32).add(index) = v,
        SampleKind::I16 => *(ptr as *mut i16).add(index) = (v.clamp(-1.0, 1.0) * 32767.0) as i16,
        SampleKind::I32 => {
            *(ptr as *mut i32).add(index) = (v.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32
        }
    }
}

/// Map a source frame's channel `out_ch` for the sink: mono → both fronts,
/// otherwise pass matching channels through and silence any extra sink channels.
/// Only the mix-format fallback ever maps down (a surround source into a stereo
/// mix takes the front L/R); the direct path hands the full source frame to the
/// OS matrix instead.
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

/// Read an endpoint mix format into our own `DeviceFormat` (so the raw pointer
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
