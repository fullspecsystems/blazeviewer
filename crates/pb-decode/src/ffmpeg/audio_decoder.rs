//! The streaming **video-audio decoder** (task #84 plan §7): a second FFmpeg
//! demux/decoder instance over the same [`VideoInput`] the video producer
//! reads, pulling the audio track as channel-interleaved f32 PCM — the source
//! the platform sinks (macOS `AVAudioEngine`, Linux `pw-cat`) play.
//!
//! Deliberately **pull-based and thread-agnostic**: the sink's feeder calls
//! [`read`](FfAudioDecoder::read) for a chunk at a time (decoding ~100 ms of
//! audio costs ~a millisecond), so there is no ring buffer or thread in here —
//! backpressure is the caller's cadence, and a 2-hour clip stays
//! constant-memory (never a full-clip `Vec`, the `ff_live`/Live-Photo
//! anti-pattern this replaces for long videos).
//!
//! Output is the source's native rate/channel-count (the sinks convert to the
//! device — see `pcm.rs` for why swresample is avoided). PTS discipline
//! mirrors the video producer: normalized so 0 = the media's start, in-place
//! `avformat_seek_file` seeks with decode-forward discard to the target.
//! Read-only, RAM-only (privacy #2).

use std::time::Duration;

use ffmpeg_next as ff;

use super::io::FfInput;
use super::pcm::append_interleaved_f32;
use crate::video::VideoInput;

/// Watchdog for one blocking read/seek burst (hostile-input bound, plan §6).
const OP_DEADLINE: Duration = Duration::from_secs(10);
/// Packets fed without producing a frame before declaring the input stuck.
const MAX_PACKETS_PER_READ: usize = 4096;

pub struct FfAudioDecoder {
    input: FfInput<'static>,
    /// Selected audio stream index — the packet filter.
    index: usize,
    decoder: ff::decoder::Audio,
    rate: u32,
    channels: u16,
    /// Stream time base (num, den) for PTS↔Duration.
    time_base: (i32, i32),
    /// First-frame PTS (stream units) — the session-relative zero.
    origin: Option<i64>,
    /// Container-declared start time, the origin fallback for pre-read seeks.
    start_time: Option<i64>,
    packet: ff::Packet,
    eof_sent: bool,
    at_eof: bool,
    /// Interleaved samples decoded but not yet handed out.
    pending: Vec<f32>,
    /// Media position (stream units) of `pending[0]` — the seek-discard cursor
    /// and the landing report.
    pending_pts: i64,
    /// Post-seek: drop decoded audio before this target (stream units).
    discard_until: Option<i64>,
}

// SAFETY: the decoder is a single-owner value used strictly through `&mut self`
// — moving it whole to a sink's feeder thread (the Linux pw-cat path) is sound.
// Every field is owned heap state: the FFmpeg contexts (format/AVIO/codec/packet)
// carry no thread affinity (libav requires only external synchronization, which
// exclusive ownership provides), the in-RAM cursor owns an `Arc<Vec<u8>>` (Send),
// and the interrupt state's cancel pointer is null on this path (`open` passes
// no borrowed flag; the watchdog fields are atomics).
unsafe impl Send for FfAudioDecoder {}

impl FfAudioDecoder {
    /// Open the container's best audio stream. `Err` when there is none (the
    /// caller treats that as `AudioClockState::Absent`, not a failure).
    pub fn open(input: &VideoInput) -> Result<FfAudioDecoder, String> {
        let mut opened = FfInput::open(input, None)?;
        let (index, rate, channels, time_base, start_time) = {
            let ctx = opened.ctx();
            let stream = ctx
                .streams()
                .best(ff::media::Type::Audio)
                .ok_or("no audio track")?;
            let par = stream.parameters();
            let (rate, channels) = unsafe {
                let p = par.as_ptr();
                (
                    (*p).sample_rate.max(0) as u32,
                    (*p).ch_layout.nb_channels.max(0) as u16,
                )
            };
            let tb = stream.time_base();
            let st = stream.start_time();
            (
                stream.index(),
                rate,
                channels,
                (tb.numerator(), tb.denominator()),
                (st != ff::ffi::AV_NOPTS_VALUE).then_some(st),
            )
        };
        if rate == 0 || channels == 0 {
            return Err("audio track has no format".into());
        }
        let stream = opened
            .ctx()
            .streams()
            .find(|s| s.index() == index)
            .ok_or("audio stream vanished")?;
        let decoder = ff::codec::context::Context::from_parameters(stream.parameters())
            .and_then(|c| c.decoder().audio())
            .map_err(|e| format!("FFmpeg audio decoder: {e}"))?;
        Ok(FfAudioDecoder {
            input: opened,
            index,
            decoder,
            rate,
            channels,
            time_base,
            origin: None,
            start_time,
            packet: ff::Packet::empty(),
            eof_sent: false,
            at_eof: false,
            pending: Vec::new(),
            pending_pts: 0,
            discard_until: None,
        })
    }

    /// Native sample rate — the sinks hand this to the device layer verbatim.
    pub fn rate(&self) -> u32 {
        self.rate
    }
    /// Native channel count (interleaved stride of [`read`](Self::read)'s output).
    pub fn channels(&self) -> u16 {
        self.channels
    }
    /// The stream is fully drained — an empty [`read`](Self::read) is final.
    pub fn at_eof(&self) -> bool {
        self.at_eof && self.pending.is_empty()
    }

    /// Decode and return up to `max_frames` interleaved sample frames
    /// (`len == n * channels`). Empty only at end-of-stream. Bounded on
    /// hostile input by the interrupt watchdog + a packet budget.
    pub fn read(&mut self, max_frames: usize) -> Result<Vec<f32>, String> {
        self.input.set_op_deadline(Some(OP_DEADLINE));
        let r = self.read_inner(max_frames);
        self.input.set_op_deadline(None);
        r
    }

    fn read_inner(&mut self, max_frames: usize) -> Result<Vec<f32>, String> {
        let want = max_frames.saturating_mul(self.channels as usize);
        let mut fed = 0usize;
        while self.pending.len() < want && !self.at_eof {
            let mut decoded = ff::frame::Audio::empty();
            if self.decoder.receive_frame(&mut decoded).is_ok() {
                self.absorb(&decoded)?;
                continue;
            }
            if self.eof_sent {
                self.at_eof = true;
                break;
            }
            if fed >= MAX_PACKETS_PER_READ {
                return Err("audio stream produced no frame (corrupt input?)".into());
            }
            match self.packet.read(self.input.ctx()) {
                Ok(()) => {
                    if self.packet.stream() == self.index {
                        fed += 1;
                        let _ = self.decoder.send_packet(&self.packet); // bad packet → skip
                    }
                }
                Err(ff::Error::Eof) => {
                    let _ = self.decoder.send_eof();
                    self.eof_sent = true;
                }
                Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => {}
                Err(e) => return Err(format!("FFmpeg audio read: {e}")),
            }
        }
        let take = want.min(self.pending.len());
        let out: Vec<f32> = self.pending.drain(..take).collect();
        self.pending_pts += self.frames_to_units(take / self.channels.max(1) as usize);
        Ok(out)
    }

    /// Fold one decoded frame into `pending`, honoring a post-seek discard.
    fn absorb(&mut self, decoded: &ff::frame::Audio) -> Result<(), String> {
        let ts = decoded.timestamp().unwrap_or(self.pending_pts);
        if self.pending.is_empty() {
            self.pending_pts = ts;
        }
        if let Some(target) = self.discard_until {
            let end = ts + self.frames_to_units(decoded.samples());
            if end <= target {
                return Ok(()); // wholly before the seek target — drop
            }
            self.discard_until = None;
            self.pending.clear();
            self.pending_pts = ts;
        }
        append_interleaved_f32(decoded, self.channels as usize, &mut self.pending)
            .map_err(|fmt| format!("unsupported audio sample format: {fmt}"))
    }

    /// In-place seek: demuxer to ≤ `to`, decoder flushed, forward discard to
    /// the target inside [`read`](Self::read). Returns the target's normalized
    /// position (the sink's new clock epoch — audio frames are ~20 ms, so the
    /// landing is within one frame of it).
    pub fn seek(&mut self, to: Duration) -> Result<Duration, String> {
        let base = self.origin.or(self.start_time).unwrap_or(0);
        let target = base.saturating_add(self.duration_to_units(to));
        self.input.set_op_deadline(Some(OP_DEADLINE));
        let rc = unsafe {
            ff::ffi::avformat_seek_file(
                self.input.ctx().as_mut_ptr(),
                self.index as i32,
                i64::MIN,
                target,
                target,
                0,
            )
        };
        let rc = if rc < 0 {
            unsafe {
                ff::ffi::avformat_seek_file(
                    self.input.ctx().as_mut_ptr(),
                    self.index as i32,
                    i64::MIN,
                    target,
                    i64::MAX,
                    0,
                )
            }
        } else {
            rc
        };
        self.input.set_op_deadline(None);
        if rc < 0 {
            return Err(format!("audio seek failed: {}", ff::Error::from(rc)));
        }
        self.decoder.flush();
        self.eof_sent = false;
        self.at_eof = false;
        self.pending.clear();
        self.discard_until = Some(target);
        Ok(to)
    }

    /// Normalized media position of the NEXT sample [`read`](Self::read) will
    /// return — the sink anchors its played-position clock here after open/seek.
    pub fn position(&self) -> Duration {
        let base = self.origin.or(self.start_time).unwrap_or(0);
        let units = self.pending_pts.saturating_sub(base).max(0);
        let (num, den) = self.time_base;
        if num <= 0 || den <= 0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(units as f64 * num as f64 / den as f64)
    }

    /// Record the origin from the first delivered audio (called implicitly:
    /// the first `absorb` sets `pending_pts`; `origin` anchors on first read).
    fn frames_to_units(&mut self, frames: usize) -> i64 {
        if self.origin.is_none() && (frames > 0 || !self.pending.is_empty()) {
            self.origin = Some(self.pending_pts);
        }
        let (num, den) = self.time_base;
        if num <= 0 || den <= 0 || self.rate == 0 {
            return 0;
        }
        (frames as f64 / self.rate as f64 * den as f64 / num as f64).round() as i64
    }

    fn duration_to_units(&self, d: Duration) -> i64 {
        let (num, den) = self.time_base;
        if num <= 0 || den <= 0 {
            return 0;
        }
        (d.as_secs_f64() * den as f64 / num as f64).round() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    fn open(name: &str) -> FfAudioDecoder {
        FfAudioDecoder::open(&VideoInput::Path(fixture(name))).expect("open audio")
    }

    /// RMS of interleaved samples — a sine has substantial energy (~0.7 × peak).
    fn rms(s: &[f32]) -> f32 {
        if s.is_empty() {
            return 0.0;
        }
        (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt()
    }

    /// The AAC tone fixture end-to-end: real rate/channels, a first chunk with
    /// real signal energy, EOS after ~the clip length, then empty-final reads.
    #[test]
    fn aac_tone_streams_with_real_energy_to_eos() {
        let mut a = open("color_with_tone.mp4");
        assert!(a.rate() >= 22_050, "rate {}", a.rate());
        assert!(a.channels() >= 1);
        let chunk = a.read(4800).expect("first chunk");
        assert_eq!(chunk.len(), 4800 * a.channels() as usize);
        assert!(rms(&chunk) > 0.05, "tone energy, got rms {}", rms(&chunk));
        // Drain: the 1 s fixture is 1 ± 0.2 s of frames.
        let mut total = chunk.len();
        loop {
            let c = a.read(48_000).expect("read");
            if c.is_empty() {
                break;
            }
            total += c.len();
        }
        assert!(a.at_eof());
        let secs = total as f64 / (a.rate() as f64 * a.channels() as f64);
        assert!((0.8..=1.3).contains(&secs), "drained {secs:.2}s");
        assert!(a.read(4800).expect("post-eos read").is_empty());
    }

    /// The macOS-fallback shape: Opus in WebM (AVFoundation can play neither).
    #[test]
    fn opus_webm_streams_with_real_energy() {
        let mut a = open("tone_vp9_opus.webm");
        assert_eq!(a.rate(), 48_000);
        let chunk = a.read(9600).expect("chunk");
        assert!(rms(&chunk) > 0.05, "rms {}", rms(&chunk));
    }

    /// In-place seek: after seek(1s) into the 2 s fixture, roughly one second
    /// of audio remains and the reported position tracks the target.
    #[test]
    fn seek_lands_near_the_target() {
        let mut a = open("tone_vp9_opus.webm");
        let _ = a.read(4800).expect("establish origin");
        a.seek(Duration::from_secs(1)).expect("seek");
        // A small read lands the discard; `position` then reports the next
        // sample — just past the target.
        let first = a.read(480).expect("landing chunk");
        assert!(!first.is_empty(), "audio remains after the seek");
        let pos = a.position();
        assert!(
            pos >= Duration::from_millis(850) && pos <= Duration::from_millis(1400),
            "landed near 1s, position reads {pos:?}"
        );
        let mut total = first.len();
        loop {
            let c = a.read(48_000).expect("read");
            if c.is_empty() {
                break;
            }
            total += c.len();
        }
        let secs = total as f64 / (a.rate() as f64 * a.channels() as f64);
        assert!((0.6..=1.3).contains(&secs), "remaining {secs:.2}s");
    }

    /// Silent clips are `Err` at open — the sink layer maps that to
    /// `AudioClockState::Absent`, never a toast.
    #[test]
    fn a_silent_clip_reports_no_audio_track() {
        let r = FfAudioDecoder::open(&VideoInput::Path(fixture("black_then_color.mp4")));
        assert!(r.is_err());
    }

    /// In-RAM archive bytes stream identically (no path anywhere).
    #[test]
    fn bytes_input_streams_audio() {
        let data =
            std::sync::Arc::new(std::fs::read(fixture("color_with_tone.mp4")).expect("bytes"));
        let mut a = FfAudioDecoder::open(&VideoInput::Bytes {
            data,
            name: "clip.mp4".into(),
        })
        .expect("open");
        let chunk = a.read(4800).expect("chunk");
        assert!(rms(&chunk) > 0.05);
    }

    /// Hostile bytes fail bounded, never hang (watchdog + packet budget).
    #[test]
    fn garbage_bytes_fail_cleanly() {
        let r = FfAudioDecoder::open(&VideoInput::Bytes {
            data: std::sync::Arc::new(vec![0x77u8; 4096]),
            name: "junk.webm".into(),
        });
        assert!(r.is_err());
    }
}
