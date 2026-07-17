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

/// Opt-in per-open diagnostics (`PB_VIDEO_DIAG=1`) — shares the video producer's
/// switch so a single env var lights up the whole session pipeline. Kept off the
/// read/seek hot path (open-time only).
fn diag() -> bool {
    std::env::var_os("PB_VIDEO_DIAG").is_some()
}

/// A [`read`](FfAudioDecoder::read) / [`seek`](FfAudioDecoder::seek) failure,
/// classified for recovery (plan 1G). `Interrupted` is the per-operation
/// watchdog/cancel abort — **transient**: a slow network read tripped the deadline,
/// and retrying may succeed (the sink rebuffers rather than dying). `Fatal` is a
/// genuine decode/format/corruption error — terminal. `Display` renders both, so
/// callers that only log/`.to_string()` the error are unaffected by the type change.
#[derive(Debug)]
pub enum AudioError {
    /// The op-deadline watchdog (or a cancel flag) aborted a blocking read/seek —
    /// recoverable; retry with a fresh deadline.
    Interrupted,
    /// A terminal decode/format error — do not retry.
    Fatal(String),
}

impl AudioError {
    /// A transient watchdog/cancel abort the caller may retry (vs. a `Fatal` error).
    pub fn is_transient(&self) -> bool {
        matches!(self, AudioError::Interrupted)
    }
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::Interrupted => write!(f, "audio read/seek interrupted (watchdog/cancel)"),
            AudioError::Fatal(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// One audio stream's selection-relevant facts (R10). Pure data so the
/// [`choose_audio`] policy is unit-testable without a multi-track fixture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AudioCandidate {
    index: usize,
    default: bool,
    forced: bool,
}

/// The R10 audio track-selection policy: an explicit, tested choice among a
/// container's audio streams instead of FFmpeg's blind `best(Audio)`.
///
/// Order of intent: an explicitly **forced** track wins (a rare but deliberate
/// "play this" signal), then the container's **default** track, then FFmpeg's
/// `best` heuristic (channel count / bitrate — passed in as `best`), then the
/// first audio stream as a last resort. Returns the chosen index plus a short
/// reason for `PB_VIDEO_DIAG`.
///
/// The **cost-based downgrade** (prefer a cheaper Apple-decodable codec — AC-3 /
/// FLAC over TrueHD — when the premium track is a measured reliability/cost
/// problem) is deliberately deferred: it is route-level and measurement-gated
/// (plan §5.2), and we never silently downgrade a track that plays fine. This
/// seam picks one stream index at open; in-session track switching stays a
/// non-goal but is not precluded.
fn choose_audio(cands: &[AudioCandidate], best: Option<usize>) -> Option<(usize, &'static str)> {
    if let Some(c) = cands.iter().find(|c| c.forced) {
        return Some((c.index, "forced disposition"));
    }
    if let Some(c) = cands.iter().find(|c| c.default) {
        return Some((c.index, "default disposition"));
    }
    if let Some(b) = best {
        return Some((b, "best heuristic"));
    }
    cands.first().map(|c| (c.index, "first audio track"))
}

/// Apply [`choose_audio`] over a live FFmpeg input: collect the audio streams'
/// dispositions in stream order and fold in FFmpeg's `best(Audio)` pick as the
/// heuristic fallback. `None` when the container carries no audio.
fn select_audio_stream(ctx: &ff::format::context::Input) -> Option<(usize, &'static str)> {
    use ff::format::stream::Disposition;
    let cands: Vec<AudioCandidate> = ctx
        .streams()
        .filter(|s| s.parameters().medium() == ff::media::Type::Audio)
        .map(|s| {
            let d = s.disposition();
            AudioCandidate {
                index: s.index(),
                default: d.contains(Disposition::DEFAULT),
                forced: d.contains(Disposition::FORCED),
            }
        })
        .collect();
    let best = ctx
        .streams()
        .best(ff::media::Type::Audio)
        .map(|s| s.index());
    choose_audio(&cands, best)
}

pub struct FfAudioDecoder {
    input: FfInput<'static>,
    /// Selected audio stream index — the packet filter.
    index: usize,
    decoder: ff::decoder::Audio,
    rate: u32,
    /// The source's native channel count (the decode/interleave stride).
    channels: u16,
    /// Channels actually emitted: `min(channels, cap)` — >2-channel sources
    /// fold to stereo for sinks whose graphs only take mono/stereo
    /// (AVAudioEngine; the MKV-5.1 crash). Native when uncapped (PipeWire
    /// downmixes itself).
    out_channels: u16,
    /// The **speaker mask** of the emitted layout (WAVE/`KSAUDIO_SPEAKER` bit
    /// values — FFmpeg's native order uses the same ones), or `None` when the
    /// container never said. `Some(0x3)` when folded to stereo.
    layout_mask: Option<u64>,
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
    /// Open the container's best audio stream at its native channel count.
    /// `Err` when there is none (the caller treats that as
    /// `AudioClockState::Absent`, not a failure).
    pub fn open(input: &VideoInput) -> Result<FfAudioDecoder, String> {
        Self::open_capped(input, u16::MAX)
    }

    /// [`open`](Self::open) with an output channel ceiling: sources wider than
    /// `max_channels` (5.1/7.1) are folded to stereo (center/LFE to both sides,
    /// surrounds to their side, ATSC-style clip guard). The macOS sink uses 2 —
    /// AVAudioEngine's graph rejects wider-than-stereo standard formats there.
    pub fn open_capped(input: &VideoInput, max_channels: u16) -> Result<FfAudioDecoder, String> {
        Self::open_track(input, max_channels, None)
    }

    /// [`open_capped`](Self::open_capped) with an explicit **stream index** (task #99): the
    /// user's chosen audio track, or `None` to apply the automatic policy.
    ///
    /// The track choice used to be baked *inside* this function, which is why nothing could
    /// switch audio: there was no way to ask for a different one, and no way to learn which
    /// one you got. Both are parameters/facts now — see [`stream_index`](Self::stream_index).
    ///
    /// A `track` naming a stream that is missing or isn't audio **falls back to the policy**
    /// rather than failing: a stale selection should cost you the *choice*, not the sound.
    pub fn open_track(
        input: &VideoInput,
        max_channels: u16,
        track: Option<usize>,
    ) -> Result<FfAudioDecoder, String> {
        let mut opened = FfInput::open(input, None)?;
        let (index, rate, channels, native_mask, time_base, start_time) = {
            let ctx = opened.ctx();
            // The user's pick, but only if it really is an audio stream in *this* container.
            let chosen = track.filter(|i| {
                ctx.streams()
                    .find(|s| s.index() == *i)
                    .is_some_and(|s| s.parameters().medium() == ff::media::Type::Audio)
            });
            // R10: an explicit, tested track-selection policy, not blind `best(Audio)`.
            let (index, reason) = match chosen {
                Some(i) => (i, "user selection"),
                None => select_audio_stream(ctx).ok_or("no audio track")?,
            };
            if diag() {
                eprintln!("[pb-video] audio track: stream #{index} ({reason})");
            }
            let stream = ctx
                .streams()
                .find(|s| s.index() == index)
                .ok_or("selected audio stream vanished")?;
            let par = stream.parameters();
            let (rate, channels, native_mask) = unsafe {
                let p = par.as_ptr();
                // FFmpeg's NATIVE channel-order mask uses the same bit values as
                // Windows' `KSAUDIO_SPEAKER` / WAVEFORMATEXTENSIBLE masks — the
                // fact `layout_mask` exists to carry. Any other order (unspec,
                // custom, ambisonic) has no mask to claim.
                let mask = ((*p).ch_layout.order
                    == ff::ffi::AVChannelOrder::AV_CHANNEL_ORDER_NATIVE)
                    .then(|| (*p).ch_layout.u.mask)
                    .filter(|&m| m != 0);
                (
                    (*p).sample_rate.max(0) as u32,
                    (*p).ch_layout.nb_channels.max(0) as u16,
                    mask,
                )
            };
            let tb = stream.time_base();
            let st = stream.start_time();
            (
                index,
                rate,
                channels,
                native_mask,
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
        let out_channels = if channels > max_channels.max(2) {
            2
        } else {
            channels
        };
        // The layout of what this decoder EMITS: folded output is plain stereo;
        // native output carries the container's own speaker mask (when it has one).
        let layout_mask = if out_channels != channels {
            Some(0x3) // FL | FR
        } else {
            native_mask
        };
        Ok(FfAudioDecoder {
            input: opened,
            index,
            decoder,
            rate,
            channels,
            out_channels,
            layout_mask,
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

    /// Arm an owned cancel flag (plan 1F): the owner (the macOS feeder queue's
    /// `SessionAudioDecoder`) flips this shared `Arc` on teardown so a blocking
    /// network read aborts inside libav instead of holding the queue for the full
    /// op-deadline. No-op-safe to skip (the watchdog is the fallback).
    pub fn arm_cancel(&mut self, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.input.set_cancel(cancel);
    }

    /// **Which stream this decoder actually opened** (task #99) — the container's real
    /// index, matching the catalog's `local_id` and its `TrackLocator::FfStream`.
    ///
    /// This is the fact that makes an honest audio picker possible. The policy inside
    /// [`open_track`](Self::open_track) is a *decision*, and until it was reported the app
    /// genuinely did not know which track it was playing — so a picker could not tick a row
    /// without guessing, and a guess that disagreed with the sound would be worse than no
    /// tick. It is also how a fallback stays honest: ask for a stale track, get the policy's
    /// pick, and this says which one that was.
    pub fn stream_index(&self) -> usize {
        self.index
    }

    /// The speaker mask of the emitted layout, in WAVE/`KSAUDIO_SPEAKER` bit
    /// values (FFmpeg's native channel order uses the same bits) — what a
    /// `WAVEFORMATEXTENSIBLE` sink declares so >2-channel audio is unambiguous.
    /// `None` when the container never named its layout.
    pub fn layout_mask(&self) -> Option<u64> {
        self.layout_mask
    }

    /// Native sample rate — the sinks hand this to the device layer verbatim.
    pub fn rate(&self) -> u32 {
        self.rate
    }
    /// Emitted channel count (the interleaved stride of [`read`](Self::read)'s
    /// output) — native, or 2 when a wider source is folded down. Deliberately
    /// NOT the `channels` field (clippy misreads the name-mismatch): the
    /// sinks must size buffers by what leaves this decoder, not the source.
    #[allow(clippy::misnamed_getters)]
    pub fn channels(&self) -> u16 {
        self.out_channels
    }
    /// The stream is fully drained — an empty [`read`](Self::read) is final.
    pub fn at_eof(&self) -> bool {
        self.at_eof && self.pending.is_empty()
    }

    /// Decode and return up to `max_frames` interleaved sample frames
    /// (`len == n * channels`). Empty only at end-of-stream. Bounded on hostile
    /// input by the interrupt watchdog + a packet budget; a watchdog abort surfaces
    /// as [`AudioError::Interrupted`] (transient — the caller may retry).
    pub fn read(&mut self, max_frames: usize) -> Result<Vec<f32>, AudioError> {
        self.input.set_op_deadline(Some(OP_DEADLINE));
        let r = self.read_inner(max_frames);
        self.input.set_op_deadline(None);
        r
    }

    fn read_inner(&mut self, max_frames: usize) -> Result<Vec<f32>, AudioError> {
        let want = max_frames.saturating_mul(self.out_channels as usize);
        // Packets fed to the decoder SINCE THE LAST PRODUCED FRAME — the true
        // "decoder is stuck / corrupt" signal. It must NOT count packets during a
        // long-but-healthy post-seek discard: a keyframe seek lands before the
        // target and decodes-and-discards audio forward to land precisely, which on
        // a sparse-keyframe 4K encode is far more than a total-packet bound like
        // 4096 (the owner's "backward seek breaks audio" — the discard window
        // overran the guard and faked a corrupt-input error). Reset on every frame.
        let mut stuck = 0usize;
        while self.pending.len() < want && !self.at_eof {
            let mut decoded = ff::frame::Audio::empty();
            if self.decoder.receive_frame(&mut decoded).is_ok() {
                stuck = 0; // the decoder is alive (this frame may be discarded)
                self.absorb(&decoded)?;
                continue;
            }
            if self.eof_sent {
                self.at_eof = true;
                break;
            }
            if stuck >= MAX_PACKETS_PER_READ {
                return Err(AudioError::Fatal(
                    "audio stream produced no frame (corrupt input?)".into(),
                ));
            }
            match self.packet.read(self.input.ctx()) {
                Ok(()) => {
                    if self.packet.stream() == self.index {
                        stuck += 1;
                        let _ = self.decoder.send_packet(&self.packet); // bad packet → skip
                    }
                }
                Err(ff::Error::Eof) => {
                    let _ = self.decoder.send_eof();
                    self.eof_sent = true;
                }
                Err(ff::Error::Other { errno }) if errno == ff::util::error::EAGAIN => {}
                // The op-deadline watchdog (or a cancel flag) aborted the read —
                // transient (a slow network read), not corruption. Let the caller
                // rebuffer and retry (plan 1G) rather than treat it as terminal.
                Err(ff::Error::Exit) => return Err(AudioError::Interrupted),
                Err(e) => return Err(AudioError::Fatal(format!("FFmpeg audio read: {e}"))),
            }
        }
        let take = want.min(self.pending.len());
        if take > 0 {
            // Anchor the session-relative zero at the first delivered sample's PTS,
            // before we advance the cursor. Explicit now — the unit converter below
            // is side-effect-free (was fused into `frames_to_units`, a refactor trap).
            self.anchor_origin();
        }
        let out: Vec<f32> = self.pending.drain(..take).collect();
        self.pending_pts += self.frames_to_units(take / self.out_channels.max(1) as usize);
        Ok(out)
    }

    /// Fold one decoded frame into `pending`, honoring a post-seek discard.
    fn absorb(&mut self, decoded: &ff::frame::Audio) -> Result<(), AudioError> {
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
        if self.out_channels == self.channels {
            append_interleaved_f32(decoded, self.channels as usize, &mut self.pending)
                .map_err(|fmt| AudioError::Fatal(format!("unsupported audio sample format: {fmt}")))
        } else {
            let mut native = Vec::new();
            append_interleaved_f32(decoded, self.channels as usize, &mut native).map_err(
                |fmt| AudioError::Fatal(format!("unsupported audio sample format: {fmt}")),
            )?;
            fold_to_stereo(&native, self.channels as usize, &mut self.pending);
            Ok(())
        }
    }

    /// In-place seek: demuxer to ≤ `to`, decoder flushed, forward discard to
    /// the target inside [`read`](Self::read). Returns the target's normalized
    /// position (the sink's new clock epoch — audio frames are ~20 ms, so the
    /// landing is within one frame of it).
    pub fn seek(&mut self, to: Duration) -> Result<Duration, AudioError> {
        let base = self.origin.or(self.start_time).unwrap_or(0);
        // A seek BEFORE the first read (a resume, or the re-position after a track
        // switch) must pin the media-zero base NOW: otherwise `anchor_origin` later
        // anchors `origin` at this seek's landing, and every subsequent seek and
        // `position()` is offset by it — resume at 20:00 then scrub to 5:00 made the
        // discard target 25:00, silently chewing 20 minutes of audio (or EOF-ing).
        // Found by the track-switch tests (macos-video-smoothness §2/§4).
        if self.origin.is_none() {
            self.origin = Some(base);
        }
        let target = base.saturating_add(self.duration_to_units(to));
        // Seek the DEFAULT stream (index -1, AV_TIME_BASE units), NOT the audio
        // stream. Matroska Cues index the VIDEO track, so `avformat_seek_file` by
        // the audio stream index finds no index entries and falls back to a
        // byte-position linear scan — measured ~73 s over SMB on a 16 GB 4K clip,
        // which then blew the op-deadline watchdog and corrupted the demuxer (the
        // owner's "audio never recovers after a network seek", plan §7/1E). Seeking
        // by -1 uses the video Cues (~20-40 ms) and the forward-discard below still
        // lands the audio precisely on `target` (audio units, within a GOP). The
        // coarse target ignores a container start offset — harmless: a keyframe ≤
        // av_target lands at or before the audio target, and the discard corrects.
        let av_target = (to.as_secs_f64() * f64::from(ff::ffi::AV_TIME_BASE)) as i64;
        self.input.set_op_deadline(Some(OP_DEADLINE));
        let rc = unsafe {
            ff::ffi::avformat_seek_file(
                self.input.ctx().as_mut_ptr(),
                -1,
                i64::MIN,
                av_target,
                av_target,
                0,
            )
        };
        let rc = if rc < 0 {
            unsafe {
                ff::ffi::avformat_seek_file(
                    self.input.ctx().as_mut_ptr(),
                    -1,
                    i64::MIN,
                    av_target,
                    i64::MAX,
                    0,
                )
            }
        } else {
            rc
        };
        self.input.set_op_deadline(None);
        if rc < 0 {
            // A watchdog/cancel abort mid-seek is transient (a slow network seek) —
            // recoverable; anything else is a real seek failure (plan 1G).
            return Err(if ff::Error::from(rc) == ff::Error::Exit {
                AudioError::Interrupted
            } else {
                AudioError::Fatal(format!("audio seek failed: {}", ff::Error::from(rc)))
            });
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

    /// Anchor the session-relative zero (`origin`) at the current `pending_pts` —
    /// the PTS of the first sample actually delivered. Idempotent (sets `origin`
    /// once, only when there is pending audio to anchor to). Split out of
    /// `frames_to_units` so unit conversion carries no hidden clock side effect.
    fn anchor_origin(&mut self) {
        if self.origin.is_none() && !self.pending.is_empty() {
            self.origin = Some(self.pending_pts);
        }
    }

    /// Pure conversion: `frames` (per-channel sample count) → stream time-base
    /// units. No side effects.
    fn frames_to_units(&self, frames: usize) -> i64 {
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

/// Fold interleaved multichannel PCM to stereo, appending to `out`. FFmpeg's
/// default channel order is FL FR FC LFE BL BR (then side/extra pairs): center
/// and LFE feed both sides, surrounds their own side at −3 dB, with the
/// ATSC-style 1/(1+√½+√½) ≈ 0.414 scale as the clip guard. Deliberately simple
/// — correct-sounding beats bit-exact for a v1 downmix.
fn fold_to_stereo(samples: &[f32], src_ch: usize, out: &mut Vec<f32>) {
    const SIDE: f32 = std::f32::consts::FRAC_1_SQRT_2;
    // Worst 5.1 sum per side: front(1) + center(√½) + LFE(0.5) + surround(√½).
    const GUARD: f32 =
        1.0 / (1.0 + std::f32::consts::FRAC_1_SQRT_2 + 0.5 + std::f32::consts::FRAC_1_SQRT_2);
    if src_ch < 2 {
        out.extend_from_slice(samples);
        return;
    }
    out.reserve(samples.len() / src_ch * 2);
    for frame in samples.chunks_exact(src_ch) {
        let mut l = frame[0];
        let mut r = frame[1];
        for (i, &s) in frame.iter().enumerate().skip(2) {
            match i {
                2 => {
                    // Front center — both sides.
                    l += SIDE * s;
                    r += SIDE * s;
                }
                3 => {
                    // LFE — both sides, quieter.
                    l += 0.5 * s;
                    r += 0.5 * s;
                }
                // Surround/side pairs: even index = left of the pair.
                i if i % 2 == 0 => l += SIDE * s,
                _ => r += SIDE * s,
            }
        }
        // 7.1's extra pair can still exceed the 5.1-sized guard — hard-clamp.
        out.push((l * GUARD).clamp(-1.0, 1.0));
        out.push((r * GUARD).clamp(-1.0, 1.0));
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

    /// Diagnostic (opt-in): can this build's FFmpeg decode a real clip's audio? On
    /// Windows "this build" is the **trimmed demux tree** (task #100), whose audio
    /// decoders were deliberately kept for channel-layout naming — this probes the
    /// deciding fact for the movie-audio fallback (AC-3/E-AC-3/DTS, which MF refuses).
    /// `PB_FF_AUDIO_CLIP=<path> cargo test -p pb-decode --features ffprobe --lib diag_ff_audio_decode -- --ignored --nocapture`
    #[test]
    #[ignore = "diagnostic — needs PB_FF_AUDIO_CLIP"]
    fn diag_ff_audio_decode() {
        let Ok(clip) = std::env::var("PB_FF_AUDIO_CLIP") else {
            eprintln!("PB_FF_AUDIO_CLIP not set — skipping");
            return;
        };
        let t0 = std::time::Instant::now();
        let mut d = FfAudioDecoder::open(&VideoInput::Path(clip.clone().into())).expect("open");
        eprintln!(
            "{clip}\n  stream #{}, {} Hz, {} ch (open {:?})",
            d.stream_index(),
            d.rate(),
            d.channels(),
            t0.elapsed()
        );
        let mut total = 0usize;
        let mut nonzero = false;
        for _ in 0..200 {
            let chunk = d.read(2400).expect("decode");
            if chunk.is_empty() {
                break;
            }
            nonzero |= chunk.iter().any(|s| s.abs() > 1e-6);
            total += chunk.len();
        }
        eprintln!(
            "  decoded {total} f32 samples ({:.1}s of audio), nonzero={nonzero}, wall {:?}",
            total as f64 / (d.rate().max(1) as u64 * d.channels().max(1) as u64) as f64,
            t0.elapsed()
        );
        assert!(total > 0, "must decode samples");
        assert!(nonzero, "must decode real audio, not silence");
    }

    /// 0D headless audio-decode margin (plan §6.0D): how much faster than
    /// real-time the selected audio track (e.g. TrueHD folded to stereo) decodes
    /// off a (network) share. Drains ~30 s of media and reports the speed-up.
    /// `PB_NET_TEST_MKV=/path cargo test -p pb-decode --features ffvideo \
    ///   net_audio_throughput -- --nocapture --ignored`
    #[test]
    #[ignore = "needs PB_NET_TEST_MKV pointing at a large (network) container"]
    fn net_audio_throughput() {
        use std::time::Instant;
        let Ok(path) = std::env::var("PB_NET_TEST_MKV") else {
            eprintln!("skipping: set PB_NET_TEST_MKV");
            return;
        };
        let mut a =
            FfAudioDecoder::open_capped(&VideoInput::Path(PathBuf::from(path)), 2).expect("open");
        let rate = a.rate() as f64;
        let ch = a.channels() as f64;
        let want_frames = (rate * 30.0) as usize; // ~30 s of media
        let t = Instant::now();
        let mut got = 0usize;
        while got < want_frames {
            match a.read(48_000) {
                Ok(c) if c.is_empty() => break,
                Ok(c) => got += c.len() / ch as usize,
                Err(e) => {
                    eprintln!("[0d] audio read error: {e}");
                    break;
                }
            }
        }
        let wall = t.elapsed().as_secs_f64();
        let media = got as f64 / rate;
        eprintln!(
            "[0d] audio: {media:.1}s media ({ch} ch @ {rate:.0} Hz) in {wall:.2}s wall → {:.1}x real-time",
            media / wall.max(1e-6)
        );
    }

    /// Manual perf repro for the network-share post-seek stall (plan §7/1E owner
    /// report; 0D characterization). Point `PB_NET_TEST_MKV` at a large (network)
    /// container and it times open+probe, a first read, a seek to the midpoint,
    /// and the post-seek read — the exact `FfAudioDecoder` path the macOS sink
    /// drives — printing whether the post-seek read errors (the watchdog `Exit`,
    /// an SMB I/O error, …). Not run in CI (no fixture). Run with:
    /// `PB_NET_TEST_MKV=/path cargo test -p pb-decode --features ffvideo \
    ///   net_seek_read_timing -- --nocapture --ignored`
    #[test]
    #[ignore = "needs PB_NET_TEST_MKV pointing at a large (network) container"]
    fn net_seek_read_timing() {
        use std::time::Instant;
        let Ok(path) = std::env::var("PB_NET_TEST_MKV") else {
            eprintln!("skipping: set PB_NET_TEST_MKV to a large (network) container");
            return;
        };
        let input = VideoInput::Path(PathBuf::from(&path));
        let t = Instant::now();
        let mut a = FfAudioDecoder::open_capped(&input, 2).expect("open");
        eprintln!(
            "[net] open+probe {:?} — rate {} ch {} (stream #{})",
            t.elapsed(),
            a.rate(),
            a.channels(),
            a.index
        );
        let t = Instant::now();
        let first = a.read(4800);
        eprintln!(
            "[net] first read {:?} — {:?} samples",
            t.elapsed(),
            first.as_ref().map(|c| c.len())
        );
        // Seek to the midpoint (the owner's "seek into the middle" repro).
        let target = Duration::from_secs(
            std::env::var("PB_NET_FWD")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3600),
        );
        let t = Instant::now();
        let landed = a.seek(target);
        eprintln!("[net] seek({target:?}) -> {landed:?} in {:?}", t.elapsed());
        let t = Instant::now();
        let post = a.read(4800);
        eprintln!(
            "[net] post-seek read {:?} — {:?}, lands at {:?}",
            t.elapsed(),
            post.as_ref().map(|c| format!("{} samples", c.len())),
            a.position()
        );
        // The seek must be fast (video-Cues path, not the ~73 s audio-index linear
        // scan), error-free, and land within a GOP of the target.
        assert!(landed.is_ok(), "seek errored: {landed:?}");
        assert!(
            post.as_ref().is_ok_and(|c| !c.is_empty()),
            "post-seek read failed: {post:?}"
        );
        let pos = a.position();
        assert!(
            pos.abs_diff(target) <= Duration::from_secs(10),
            "landed at {pos:?}, expected ~{target:?}"
        );

        // A large BACKWARD seek (owner report 2026-07-13: backward still breaks).
        let back = Duration::from_secs(
            std::env::var("PB_NET_BACK")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(600),
        );
        let t = Instant::now();
        let landed_b = a.seek(back);
        eprintln!(
            "[net] BACK seek({back:?}) -> {landed_b:?} in {:?}",
            t.elapsed()
        );
        let t = Instant::now();
        let post_b = a.read(4800);
        eprintln!(
            "[net] post-BACK read {:?} — {:?}, lands at {:?}",
            t.elapsed(),
            post_b.as_ref().map(|c| format!("{} samples", c.len())),
            a.position()
        );
        assert!(landed_b.is_ok(), "back-seek errored: {landed_b:?}");
        assert!(
            post_b.as_ref().is_ok_and(|c| !c.is_empty()),
            "post-back-seek read failed: {post_b:?}"
        );
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

    // ── Track-switch semantics (task #99 / macos-video-smoothness §2) ──
    // The macOS FFI's switch (`session_audio_set_track`) is exactly "a fresh
    // `open_track(Some(i))` from the same input replaces the old decoder"; these
    // pin the decoder-level half of the switch-as-rebuffer transaction contract.

    /// Switching the multitrack fixture's default AAC (stream 1, 44.1 kHz stereo)
    /// to the AC-3 commentary (stream 2, 48 kHz 6-ch) rebuilds the format: the
    /// replacement decoder reports the new rate, folds 5.1 to the stereo cap, and
    /// reports the **actual** stream. And it starts at ZERO — the transaction's
    /// seek-to-playhead (step 4) is mandatory, not belt-and-suspenders.
    #[test]
    fn track_switch_rebuilds_format_and_starts_at_zero() {
        let input = VideoInput::Path(fixture("multitrack.mkv"));
        let mut a = FfAudioDecoder::open_capped(&input, 2).expect("open default");
        assert_eq!(a.stream_index(), 1, "policy picks the authored default");
        assert_eq!(a.rate(), 44_100);
        assert_eq!(a.channels(), 2);
        let _ = a.read(4800).expect("play a bit before switching");

        // The switch: a fresh decoder on the chosen stream replaces the old one.
        let mut b = FfAudioDecoder::open_track(&input, 2, Some(2)).expect("switch");
        assert_eq!(b.stream_index(), 2, "the actual stream, not a guess");
        assert_eq!(b.rate(), 48_000, "the new track's own rate — never the old");
        assert_eq!(b.channels(), 2, "6-ch folds to the stereo cap");
        let chunk = b.read(2400).expect("post-switch chunk");
        assert!(!chunk.is_empty(), "the new track produces frames");
        assert_eq!(chunk.len() % 2, 0, "interleaved at the folded stride");
        assert!(
            b.position() < Duration::from_millis(300),
            "a fresh decoder starts at zero (got {:?}) — the caller must seek to \
             the playhead after a switch",
            b.position()
        );
    }

    /// A stale or non-audio `track` falls back to the policy and *reports* the
    /// policy's pick — a bad selection costs the choice, not the sound (the FFI
    /// then answers the picker via `stream_index`, so the tick stays honest).
    #[test]
    fn stale_track_falls_back_to_policy_and_reports_honestly() {
        let input = VideoInput::Path(fixture("multitrack.mkv"));
        // Stream 3 is a subtitle; stream 99 doesn't exist.
        for stale in [3usize, 99] {
            let a = FfAudioDecoder::open_track(&input, 2, Some(stale)).expect("fallback");
            assert_eq!(
                a.stream_index(),
                1,
                "track {stale}: fell back to the policy default and said so"
            );
        }
    }

    /// The refusal path: an unopenable input errors, so the FFI returns `false`
    /// and — by its match structure — never touches the old decoder, which keeps
    /// playing (task #99's "a failed switch costs the choice, not the sound").
    #[test]
    fn switch_to_an_unopenable_input_is_refused() {
        let r = FfAudioDecoder::open_track(
            &VideoInput::Path(PathBuf::from("/nope/never/exists.mkv")),
            2,
            Some(1),
        );
        assert!(r.is_err(), "unopenable input must refuse, not panic");
    }

    /// Post-switch seek+read (transaction steps 2→4 in miniature): the fresh
    /// decoder on the new track seeks to the "playhead" and produces frames
    /// positioned there — resume-where-the-picture-is works on the switched track.
    #[test]
    fn post_switch_seek_and_read_yield_frames_at_the_target() {
        let input = VideoInput::Path(fixture("multitrack.mkv"));
        let mut b = FfAudioDecoder::open_track(&input, 2, Some(2)).expect("switch");
        b.seek(Duration::from_millis(500))
            .expect("seek to playhead");
        let chunk = b.read(2400).expect("landing chunk");
        assert!(!chunk.is_empty(), "audio remains after the seek");
        let pos = b.position();
        assert!(
            pos >= Duration::from_millis(300) && pos <= Duration::from_millis(900),
            "landed near the 0.5s playhead, position reads {pos:?}"
        );
    }

    /// Regression (found by the switch tests): a seek BEFORE the first read (a
    /// resume, or the post-switch re-position) must not corrupt the media-zero
    /// base. Pre-fix, `anchor_origin` pinned `origin` at the pre-read seek's
    /// landing, so a LATER seek targeted `landing + to` — resume at 20:00 then
    /// scrub to 5:00 aimed the discard at 25:00 and silently chewed 20 minutes of
    /// audio (or EOF'd). Both seeks must be media-absolute.
    #[test]
    fn a_pre_read_seek_does_not_offset_later_seeks() {
        let mut a = open("tone_vp9_opus.webm"); // 2 s fixture
                                                // "Resume" at 1 s — no read has happened yet.
        a.seek(Duration::from_secs(1)).expect("pre-read seek");
        let first = a.read(480).expect("resume landing");
        assert!(!first.is_empty());
        assert!(
            a.position() >= Duration::from_millis(850),
            "resume landed near 1s, position reads {:?}",
            a.position()
        );
        // Now "scrub back" to 0.2 s — media-absolute, NOT resume-relative (which
        // would target 1.2 s and leave only ~0.8 s of audio).
        a.seek(Duration::from_millis(200)).expect("scrub back");
        let chunk = a.read(480).expect("post-scrub chunk");
        assert!(!chunk.is_empty());
        let pos = a.position();
        assert!(
            pos <= Duration::from_millis(700),
            "scrub-back landed media-absolute near 0.2s, position reads {pos:?}"
        );
        // Draining from 0.2 s leaves ~1.8 s — impossible if the seek was offset.
        let mut total = chunk.len();
        loop {
            let c = a.read(48_000).expect("read");
            if c.is_empty() {
                break;
            }
            total += c.len();
        }
        let secs = total as f64 / (a.rate() as f64 * a.channels() as f64);
        assert!(
            secs >= 1.4,
            "remaining {secs:.2}s — the scrub-back was offset"
        );
    }

    /// The MKV-5.1 crash regression (owner crash report, 2026-07-12): a
    /// 6-channel source opened with the macOS cap folds to stereo — the engine
    /// graph never sees a wider-than-stereo format — and keeps real energy
    /// (the fixture's tone sits in the center channel, which feeds both sides).
    /// Uncapped (the Linux/pw-cat path) it stays native 6-channel.
    #[test]
    fn surround_source_folds_to_stereo_when_capped() {
        let mut capped = FfAudioDecoder::open_capped(&VideoInput::Path(fixture("tone_51.mp4")), 2)
            .expect("open capped");
        assert_eq!(capped.channels(), 2, "5.1 folds to stereo");
        let chunk = capped.read(4800).expect("chunk");
        assert_eq!(chunk.len(), 4800 * 2);
        // The tone lives in the center channel (plane 2) — energy in BOTH fold
        // sides proves the extended_data plane read (planes 1+ used to read as
        // empty via linesize[i], silencing everything but channel 0).
        let left: Vec<f32> = chunk.iter().step_by(2).copied().collect();
        let right: Vec<f32> = chunk.iter().skip(1).step_by(2).copied().collect();
        assert!(rms(&left) > 0.01, "left carries the center: {}", rms(&left));
        assert!(
            rms(&right) > 0.01,
            "right carries the center: {}",
            rms(&right)
        );

        let native = open("tone_51.mp4");
        assert_eq!(native.channels(), 6, "uncapped stays native");
    }

    /// The fold math: center feeds both sides equally; a hard-left source stays
    /// left; stereo/mono inputs pass through untouched.
    #[test]
    fn fold_to_stereo_routes_channels() {
        // One 5.1 frame with only FC lit.
        let mut out = Vec::new();
        fold_to_stereo(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], 6, &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0] - out[1]).abs() < 1e-6, "center is equal in both");
        assert!(out[0] > 0.2, "audibly present: {}", out[0]);
        // Only FL lit → right stays silent.
        out.clear();
        fold_to_stereo(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0], 6, &mut out);
        assert!(out[0] > 0.3 && out[1].abs() < 1e-6);
        // Full-scale everywhere never exceeds ±1 (the clip guard).
        out.clear();
        fold_to_stereo(&[1.0; 6], 6, &mut out);
        assert!(out[0] <= 1.0 && out[1] <= 1.0, "{out:?}");
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

    /// R10 track-selection policy: forced beats default beats best-heuristic
    /// beats first; a container with one audio track just picks it.
    #[test]
    fn choose_audio_honors_disposition_intent() {
        let cand = |index, default, forced| AudioCandidate {
            index,
            default,
            forced,
        };
        // Forced wins even over a later default track.
        assert_eq!(
            choose_audio(&[cand(1, true, false), cand(2, false, true)], Some(1),),
            Some((2, "forced disposition"))
        );
        // No forced → the container's default track, over FFmpeg's `best`.
        assert_eq!(
            choose_audio(&[cand(1, false, false), cand(3, true, false)], Some(1),),
            Some((3, "default disposition"))
        );
        // No disposition → fall back to FFmpeg's best-heuristic pick.
        assert_eq!(
            choose_audio(&[cand(1, false, false), cand(2, false, false)], Some(2)),
            Some((2, "best heuristic"))
        );
        // No disposition and no best → the first audio stream.
        assert_eq!(
            choose_audio(&[cand(4, false, false), cand(5, false, false)], None),
            Some((4, "first audio track"))
        );
        // No audio at all.
        assert_eq!(choose_audio(&[], None), None);
    }

    /// `AudioError` classifies recovery (plan 1G): a watchdog/cancel abort is
    /// transient (retry), a decode/format error is fatal. Display renders both so
    /// log-only callers are unaffected by the typed error.
    #[test]
    fn audio_error_classifies_transient_vs_fatal() {
        assert!(AudioError::Interrupted.is_transient());
        assert!(!AudioError::Fatal("boom".into()).is_transient());
        assert_eq!(AudioError::Fatal("boom".into()).to_string(), "boom");
        assert!(AudioError::Interrupted.to_string().contains("interrupted"));
    }

    /// The single-track fixtures select their one audio stream (exercises the
    /// live `select_audio_stream` glue end-to-end, not just the pure policy).
    #[test]
    fn select_audio_stream_picks_the_lone_track() {
        let mut opened = FfInput::open(&VideoInput::Path(fixture("color_with_tone.mp4")), None)
            .expect("open input");
        let picked = select_audio_stream(opened.ctx());
        assert!(matches!(picked, Some((_, _))), "found an audio stream");
    }

    /// **The fact an honest picker needs** (#99): the decoder reports which stream it
    /// opened. Until it did, nothing in the app knew which audio track was playing — the
    /// policy decided privately — so a picker could only have *guessed* which row to tick.
    #[test]
    fn the_decoder_reports_the_stream_it_opened() {
        let d = FfAudioDecoder::open(&VideoInput::Path(fixture("color_with_tone.mp4")))
            .expect("open audio");
        let mut opened = FfInput::open(&VideoInput::Path(fixture("color_with_tone.mp4")), None)
            .expect("open input");
        let (want, _) = select_audio_stream(opened.ctx()).expect("a policy pick");
        assert_eq!(
            d.stream_index(),
            want,
            "the reported index must be the stream the policy actually chose"
        );
        // It is a real stream index in the container, not an ordinal among audio streams —
        // that is what makes it comparable to the catalog's `local_id`.
        assert!(opened
            .ctx()
            .streams()
            .any(|s| s.index() == d.stream_index()
                && s.parameters().medium() == ff::media::Type::Audio));
    }

    /// Asking for a track explicitly opens *that* stream.
    #[test]
    fn an_explicit_track_is_honoured() {
        let mut opened = FfInput::open(&VideoInput::Path(fixture("color_with_tone.mp4")), None)
            .expect("open input");
        let want = opened
            .ctx()
            .streams()
            .find(|s| s.parameters().medium() == ff::media::Type::Audio)
            .map(|s| s.index())
            .expect("an audio stream");
        let d = FfAudioDecoder::open_track(
            &VideoInput::Path(fixture("color_with_tone.mp4")),
            u16::MAX,
            Some(want),
        )
        .expect("open audio");
        assert_eq!(d.stream_index(), want);
    }

    /// A selection naming a stream that is missing — or that is the *video* stream — must
    /// cost you the choice, not the sound: it falls back to the policy rather than failing.
    /// A stale pick reaching a re-open is the expected case, not an exotic one.
    #[test]
    fn a_bogus_track_falls_back_to_the_policy_rather_than_failing() {
        let path = fixture("color_with_tone.mp4");
        let expected = {
            let mut o = FfInput::open(&VideoInput::Path(path.clone()), None).expect("open");
            select_audio_stream(o.ctx()).expect("policy").0
        };
        // Way past the end of the stream table.
        let d = FfAudioDecoder::open_track(&VideoInput::Path(path.clone()), u16::MAX, Some(99))
            .expect("still opens");
        assert_eq!(d.stream_index(), expected, "fell back to the policy's pick");

        // ...and a real stream index that isn't audio is refused the same way.
        let video = {
            let mut o = FfInput::open(&VideoInput::Path(path.clone()), None).expect("open");
            o.ctx()
                .streams()
                .find(|s| s.parameters().medium() == ff::media::Type::Video)
                .map(|s| s.index())
                .expect("a video stream")
        };
        let d = FfAudioDecoder::open_track(&VideoInput::Path(path), u16::MAX, Some(video))
            .expect("still opens");
        assert_eq!(
            d.stream_index(),
            expected,
            "a video stream is not an audio pick"
        );
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
