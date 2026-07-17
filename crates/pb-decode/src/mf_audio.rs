//! Windows audio decode via the Media Foundation **Source Reader** — the decode
//! half of video-audio playback (task #79 phase 5, Windows).
//!
//! The WinRT `MediaPlayer` (the old high-level audio backend) opens but refuses to
//! *play* legacy camera clips (MJPEG-in-AVI + PCM: the clock never advances). The
//! low-level `IMFSourceReader` — the same permissive API that decodes their video —
//! decodes their **audio** to PCM fine (verified). So this is the Windows twin of
//! `FfAudioDecoder`: pull-based interleaved-f32 chunks + in-place seek, feeding the
//! WASAPI render engine in the shell (`pb-app/src/wasapi_audio.rs`).
//!
//! Output is always **32-bit float, interleaved, at a requested sample rate and the
//! source's own channel count**. MF's auto-inserted resampler does sample-rate +
//! bit-depth conversion reliably; **channel** conversion (mono→stereo, 5.1→stereo)
//! it does *not* configure dependably in the reader topology, so the render engine
//! maps channels itself — trivial and correct, where MF is flaky.

use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{
    IMFSourceReader, MFAudioFormat_Float, MFCreateMediaType, MFCreateSourceReaderFromByteStream,
    MFCreateSourceReaderFromURL, MFMediaType_Audio, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
    MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
};

use crate::mf_poster::propvariant_i8;
use crate::mf_stream::ReaderSource;
use crate::mf_video::ensure_mf;
use crate::video::VideoInput;
use crate::DecodeError;

/// The decoder's negotiated output layout: 32-bit float interleaved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MfAudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// A pull-based MF audio decoder over one of a clip's audio streams. Created and
/// used entirely on the owning audio thread (the WASAPI render thread in the
/// shell), so it never crosses threads — no `Send` needed.
pub struct MfAudioDecoder {
    reader: IMFSourceReader,
    format: MfAudioFormat,
    /// The reader stream this decodes. Held because a track-selected decoder can't
    /// keep using `MF_SOURCE_READER_FIRST_AUDIO_STREAM` for `ReadSample` — that
    /// constant means "the first audio stream", not "the one I selected".
    stream: u32,
    /// Which **audio ordinal** [`Self::stream`] is: 0 = the clip's first audio
    /// stream, 1 = its second. See [`Self::open_track`].
    track: usize,
    /// [`Self::stream`] as a **real reader stream index** — resolved even when
    /// `stream` holds the `FIRST_AUDIO_STREAM` constant, so callers can compare it
    /// to `TrackLocator::MfStream` values. `None` only if resolution failed.
    resolved_stream: Option<u32>,
    eos: bool,
}

impl MfAudioDecoder {
    /// Open the clip's **first** audio stream. See [`Self::open_track`].
    pub fn open(input: &VideoInput, sample_rate: u32) -> Result<MfAudioDecoder, DecodeError> {
        Self::open_track(input, sample_rate, None)
    }

    /// Open one audio stream of `input`, decoding to interleaved **f32** at
    /// `sample_rate` (MF resamples) and the source's channel count. `Err` if the
    /// clip has no such audio stream, or MF can't decode it.
    ///
    /// ## ⚠ `track` is an ordinal in **MF's** namespace — NOT a picker row
    ///
    /// It counts audio streams in **the order MF enumerates them**, which is *not*
    /// the container's order and *not* the track catalog's. Measured on
    /// `multitrack.mp4`: the file holds a 440 Hz `eng` track then a 220 Hz `fra`
    /// one, and **MF hands back the `fra` track as ordinal 0**. `mf_tracks.rs` found
    /// the same thing from the other side in the phase-0 spike (MF marks the French
    /// commentary `selected`, "the first stream of the `MF_SD_MUTUALLY_EXCLUSIVE`
    /// group"). Two independent measurements, one conclusion.
    ///
    /// So this **cannot** be fed a `subtitle_picker_rows`-style row index. On Windows
    /// the runtime catalog is **FFmpeg's** (it supersedes MF's in `media_details`,
    /// because MF models no subtitle tracks at all), so its rows are in FFmpeg's
    /// order and its locators are `TrackLocator::FfStream`. Crossing the two
    /// namespaces selects the wrong language while confidently ticking the right one.
    ///
    /// `TrackLocator::MfStream` is the variant that carries the MF-side identity — as a
    /// **reader stream index**, not this audio ordinal — minted only by MF's *own*
    /// catalog (`mf_tracks`, the no-ffprobe fallback) and consumed by
    /// [`Self::open_reader_stream`]. The only safe callers *here* are `None`/`Some(0)`
    /// ("whatever plays by default") and code that got its ordinal *from MF*.
    ///
    /// `None` = the first audio stream, which is what MF's `FIRST_AUDIO_STREAM` picks
    /// and therefore what has always played when nobody chose.
    pub fn open_track(
        input: &VideoInput,
        sample_rate: u32,
        track: Option<usize>,
    ) -> Result<MfAudioDecoder, DecodeError> {
        ensure_mf();
        unsafe {
            let reader = open_reader(input)?;
            // Resolve the ordinal to a real reader stream. Ordinal 0 stays on the
            // `FIRST_AUDIO_STREAM` constant it has always used — same call, same
            // behaviour, so the default path can't regress on a file whose streams
            // enumerate in some order we didn't expect. The *resolved* index rides
            // along purely for reporting ([`Self::reader_stream`]); selection never
            // depends on it.
            let (audio, ordinal, resolved) = match track {
                None | Some(0) => (
                    MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
                    0,
                    nth_audio_stream(&reader, 0).ok(),
                ),
                Some(n) => {
                    let s = nth_audio_stream(&reader, n)?;
                    (s, n, Some(s))
                }
            };
            Self::open_on(reader, sample_rate, audio, ordinal, resolved)
        }
    }

    /// Open the audio stream at **reader stream index** `stream` — the currency of
    /// [`crate::tracks::TrackLocator::MfStream`], i.e. the same numbering
    /// `mf_tracks` enumerates onto its own catalog's audio rows (the no-ffprobe
    /// fallback path). `Err` if `stream` doesn't exist or isn't audio — refusing
    /// beats silently decoding a stream the user didn't pick.
    pub fn open_reader_stream(
        input: &VideoInput,
        sample_rate: u32,
        stream: u32,
    ) -> Result<MfAudioDecoder, DecodeError> {
        ensure_mf();
        unsafe {
            let reader = open_reader(input)?;
            let ty = reader
                .GetNativeMediaType(stream, 0)
                .map_err(|_| DecodeError::Corrupt(format!("this video has no stream {stream}")))?;
            if ty.GetGUID(&MF_MT_MAJOR_TYPE) != Ok(MFMediaType_Audio) {
                return Err(DecodeError::Corrupt(format!(
                    "stream {stream} is not an audio stream"
                )));
            }
            // The audio ordinal is derivable (count the audio streams below), and the
            // `track()` report must stay honest either way.
            let mut ordinal = 0usize;
            for i in 0..stream {
                if reader
                    .GetNativeMediaType(i, 0)
                    .is_ok_and(|t| t.GetGUID(&MF_MT_MAJOR_TYPE) == Ok(MFMediaType_Audio))
                {
                    ordinal += 1;
                }
            }
            Self::open_on(reader, sample_rate, stream, ordinal, Some(stream))
        }
    }

    /// The shared open body: select `audio` on `reader`, negotiate f32 output.
    unsafe fn open_on(
        reader: IMFSourceReader,
        sample_rate: u32,
        audio: u32,
        ordinal: usize,
        resolved: Option<u32>,
    ) -> Result<MfAudioDecoder, DecodeError> {
        unsafe {
            // Decode only the audio stream (leave video untouched — it's the
            // VideoSession's job; a selected-but-unread stream queues forever).
            reader
                .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
                .map_err(mf_err)?;
            reader
                .SetStreamSelection(audio, true)
                .map_err(|_| DecodeError::Corrupt("this video has no audio stream".into()))?;

            // Source channel count, from the native type (before we impose ours).
            let native = reader
                .GetNativeMediaType(audio, 0)
                .map_err(|_| DecodeError::Corrupt("this video has no audio stream".into()))?;
            let channels = native
                .GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
                .map(|c| c.clamp(1, 8) as u16)
                .unwrap_or(2);

            // Request f32 interleaved at `sample_rate`, keeping the source channels.
            let out = MFCreateMediaType().map_err(mf_err)?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(mf_err)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)
                .map_err(mf_err)?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32)
                .map_err(mf_err)?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)
                .map_err(mf_err)?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)
                .map_err(mf_err)?;
            let block_align = channels as u32 * 4;
            out.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)
                .map_err(mf_err)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, sample_rate * block_align)
                .map_err(mf_err)?;
            reader
                .SetCurrentMediaType(audio, None, &out)
                .map_err(|e| DecodeError::Corrupt(format!("audio format not decodable: {e}")))?;

            Ok(MfAudioDecoder {
                reader,
                format: MfAudioFormat {
                    sample_rate,
                    channels,
                },
                stream: audio,
                track: ordinal,
                resolved_stream: resolved,
                eos: false,
            })
        }
    }

    pub fn format(&self) -> MfAudioFormat {
        self.format
    }

    /// The audio ordinal this is decoding — what the picker ticks. Read back from the
    /// decoder rather than remembered by the caller, so the tick reports what is
    /// **playing** rather than what was asked for.
    pub fn track(&self) -> usize {
        self.track
    }

    /// The **reader stream index** being decoded — the `TrackLocator::MfStream`
    /// currency, comparable to the track catalog's locators (task #99). Resolved even
    /// for a default open (which selects via the `FIRST_AUDIO_STREAM` constant);
    /// `None` only if that resolution failed, which no real container has produced.
    pub fn reader_stream(&self) -> Option<u32> {
        self.resolved_stream
    }

    /// Pull the next decoded chunk as interleaved f32 samples. `Ok(None)` = the
    /// stream ended (further calls keep returning `None`). A gap tick (no sample,
    /// not EOS) yields an empty vec so the caller loops without treating it as EOS.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<f32>>, DecodeError> {
        if self.eos {
            return Ok(None);
        }
        // The stream we actually selected — NOT `FIRST_AUDIO_STREAM`, which would
        // quietly read the wrong track's samples on a switched decoder (the picker
        // would tick "Commentary" while the main mix kept playing).
        let audio = self.stream;
        unsafe {
            let mut flags = 0u32;
            let mut sample = None;
            self.reader
                .ReadSample(audio, 0, None, Some(&mut flags), None, Some(&mut sample))
                .map_err(mf_err)?;
            if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                self.eos = true;
                return Ok(None);
            }
            let Some(sample) = sample else {
                return Ok(Some(Vec::new())); // a gap tick — keep pulling
            };
            let buffer = sample.ConvertToContiguousBuffer().map_err(mf_err)?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut ptr, None, Some(&mut len))
                .map_err(mf_err)?;
            // `len` is a byte count of f32 lanes; copy out before unlocking.
            let n = (len as usize) / 4;
            let mut out = vec![0f32; n];
            if n > 0 {
                std::ptr::copy_nonoverlapping(ptr as *const f32, out.as_mut_ptr(), n);
            }
            let _ = buffer.Unlock();
            Ok(Some(out))
        }
    }

    /// Seek so the next [`next_chunk`](Self::next_chunk) returns audio at/after `position`.
    /// Clears the EOS latch (a replay-from-target after the stream ended works).
    pub fn seek(&mut self, position: Duration) -> Result<(), DecodeError> {
        let hns = (position.as_nanos() / 100) as i64;
        let pv = propvariant_i8(hns.max(0));
        unsafe {
            self.reader
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &pv)
                .map_err(mf_err)?;
        }
        self.eos = false;
        Ok(())
    }
}

/// A source reader over `input` (URL for a path, byte stream for in-RAM bytes).
unsafe fn open_reader(input: &VideoInput) -> Result<IMFSourceReader, DecodeError> {
    unsafe {
        match ReaderSource::new(input)
            .map_err(|e| DecodeError::Corrupt(format!("Media Foundation: {e}")))?
        {
            ReaderSource::Url(url) => MFCreateSourceReaderFromURL(&url, None),
            ReaderSource::Stream(bs) => MFCreateSourceReaderFromByteStream(&bs, None),
        }
        .map_err(|e| DecodeError::Corrupt(format!("Media Foundation: {e}")))
    }
}

/// The reader stream number of the `n`th **audio** stream (0-based), counting in the
/// reader's own enumeration order.
///
/// Walks streams from 0 asking each for its major type; anything that isn't audio is
/// skipped, so the count is over audio streams only — the ordinal
/// [`MfAudioDecoder::open_track`] takes. Enumeration ends when MF reports no such
/// stream (`GetNativeMediaType` errors), which is the documented way to find the end;
/// there is no stream *count* on `IMFSourceReader`.
///
/// A hard cap stops a misbehaving source spinning forever. 128 is far past any real
/// file (a Blu-ray remux tops out around a dozen audio tracks) and is a backstop, not
/// a limit anyone should hit.
unsafe fn nth_audio_stream(reader: &IMFSourceReader, n: usize) -> Result<u32, DecodeError> {
    let mut seen = 0usize;
    for i in 0u32..128 {
        // No such stream → we've walked past the end. This is the terminator, not an
        // error to report: it just means the file has fewer audio streams than asked.
        let Ok(ty) = reader.GetNativeMediaType(i, 0) else {
            break;
        };
        if ty.GetGUID(&MF_MT_MAJOR_TYPE) != Ok(MFMediaType_Audio) {
            continue;
        }
        if seen == n {
            return Ok(i);
        }
        seen += 1;
    }
    Err(DecodeError::Corrupt(format!(
        "this video has no audio track {n} (found {seen})"
    )))
}

fn mf_err(e: windows::core::Error) -> DecodeError {
    DecodeError::Corrupt(format!("Media Foundation: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    /// The tone fixture (AAC in MP4) decodes to non-empty f32 at the requested rate.
    #[test]
    fn decodes_the_tone_fixture_to_f32() {
        let input = VideoInput::Path(fixture("color_with_tone.mp4"));
        let mut dec = MfAudioDecoder::open(&input, 48_000).expect("open audio");
        assert_eq!(dec.format().sample_rate, 48_000);
        assert!((1..=2).contains(&dec.format().channels));
        let mut total = 0usize;
        for _ in 0..500 {
            match dec.next_chunk().expect("decode") {
                Some(chunk) => total += chunk.len(),
                None => break,
            }
        }
        assert!(total > 0, "the tone fixture must decode to some audio");
    }

    /// A silent clip (no audio stream) fails to open cleanly, not a panic.
    #[test]
    fn a_clip_without_audio_fails_to_open() {
        let input = VideoInput::Path(fixture("black_then_color.mp4"));
        assert!(MfAudioDecoder::open(&input, 48_000).is_err());
    }

    // ── Track selection (task #99, the Windows half) ──
    //
    // `multitrack.mp4` is the fixture here, not the `.mkv`: its two audio tracks are a
    // **440 Hz** sine then a **220 Hz** sine (see the fixtures README), so the decoded
    // *tone* says which stream the samples actually came from. That is the assertion that
    // matters — "some audio came out" would pass just as happily while reading the first
    // track twice, which is precisely the bug worth catching.
    //
    // ⚠ The `.mkv`'s second track is **AC-3, and MF refuses to decode it** on this machine
    // (0xC00D36B4 out of `SetCurrentMediaType`). That is a real, pre-existing Windows
    // limitation — it is why a DD/DD+ film plays silently — and nothing to do with track
    // selection: `nth_audio_stream` finds the stream and reads its 6 channels fine, then
    // the *decoder* declines. Don't "fix" it by reaching for the mkv here.

    /// The dominant-tone proxy: sign changes on channel 0. A 440 Hz sine crosses zero
    /// twice per cycle, so it has ~2× the crossings of a 220 Hz one — a ratio no amount of
    /// AAC noise turns upside down.
    fn zero_crossings(samples: &[f32], channels: usize) -> usize {
        samples
            .iter()
            .step_by(channels)
            .zip(samples.iter().step_by(channels).skip(1))
            .filter(|(a, b)| (**a < 0.0) != (**b < 0.0))
            .count()
    }

    /// Decode up to `n` samples of one track, returning (channels, samples).
    fn decode_track(track: Option<usize>) -> (usize, Vec<f32>) {
        let input = VideoInput::Path(fixture("multitrack.mp4"));
        let mut d = MfAudioDecoder::open_track(&input, 48_000, track).expect("open track");
        assert_eq!(
            d.track(),
            track.unwrap_or(0),
            "reports the ordinal it opened"
        );
        let ch = d.format().channels as usize;
        let mut out = Vec::new();
        for _ in 0..500 {
            match d.next_chunk().expect("decode") {
                Some(c) => out.extend(c),
                None => break,
            }
        }
        (ch, out)
    }

    /// **The test that proves the feature**, and the one that caught the assumption behind
    /// it being false.
    ///
    /// Ordinal 0 and ordinal 1 must decode *different* streams — that is what `next_chunk`
    /// reading `self.stream` rather than `FIRST_AUDIO_STREAM` buys, and the mistake is
    /// invisible to any "did audio come out?" check.
    ///
    /// ⚠ **Which tone lands on which ordinal is MF's business, not the container's.** The
    /// file's audio streams are, in container order, a 440 Hz `eng` track then a 220 Hz
    /// `fra` one — and MF enumerates them **the other way round**: ordinal 0 decodes the
    /// 220 Hz `fra` track. That is not a bug here and not a quirk of this fixture; it is
    /// the same finding `mf_tracks.rs` recorded from the phase-0 spike ("MF marks the
    /// **French Director's Commentary** `selected=true` … it simply takes the first stream
    /// of the `MF_SD_MUTUALLY_EXCLUSIVE` group"), measured independently here. It is
    /// exactly why [`MfAudioDecoder::open_track`]'s ordinal is **not** interchangeable with
    /// a picker row.
    #[test]
    fn a_selected_track_decodes_its_own_samples() {
        let (ch0, first) = decode_track(Some(0));
        let (ch1, second) = decode_track(Some(1));
        assert!(!first.is_empty() && !second.is_empty(), "both decode");
        let (a, b) = (zero_crossings(&first, ch0), zero_crossings(&second, ch1));
        // ~450 vs ~960 crossings over the 1.02s clip (220 Hz vs 440 Hz). Compare as a
        // ratio, not to absolute counts, so this doesn't care about the decoded length.
        assert!(
            (a as f64) < (b as f64) * 0.75,
            "the two ordinals must decode different streams — got {a} crossings for ordinal \
             0 and {b} for ordinal 1; near-equal counts mean both read the same stream"
        );
        // Pin MF's order, so the day it changes is a test failure and not a user reporting
        // that the wrong language plays.
        assert!(
            (a as f64) < 600.0,
            "ordinal 0 is MF's first audio stream = the 220 Hz `fra` track (~450 crossings), \
             NOT the container's first (440 Hz `eng`, ~920). Got {a}."
        );
    }

    /// `open` (and `open_track(None)`) must keep meaning "the first audio stream": the
    /// default path stays on MF's own `FIRST_AUDIO_STREAM` constant, so it cannot regress
    /// on a file whose streams enumerate in an order we didn't expect.
    #[test]
    fn open_defaults_to_the_first_audio_track() {
        let input = VideoInput::Path(fixture("multitrack.mp4"));
        let d = MfAudioDecoder::open(&input, 48_000).expect("open default");
        assert_eq!(d.track(), 0);
        // ...and it still reports the *real* reader stream index it resolved to, the
        // `MfStream` currency the shell compares against the catalog's locators.
        assert_eq!(
            d.reader_stream(),
            MfAudioDecoder::open_track(&input, 48_000, Some(0))
                .expect("open ordinal 0")
                .reader_stream(),
            "a default open and an explicit ordinal-0 open are the same stream"
        );
    }

    /// `open_reader_stream` — the track-switch entry (task #99): a reader stream index
    /// (the `TrackLocator::MfStream` currency) opens exactly that stream. Cross-checked
    /// against `open_track`'s ordinals via the decoded tone, and pinned to decode
    /// *different* streams for the two indices.
    #[test]
    fn open_reader_stream_opens_the_stream_the_locator_names() {
        let input = VideoInput::Path(fixture("multitrack.mp4"));
        // The two audio ordinals' real reader stream indices, from the ordinal path.
        let s0 = MfAudioDecoder::open_track(&input, 48_000, Some(0))
            .expect("ordinal 0")
            .reader_stream()
            .expect("ordinal 0 resolves");
        let s1 = MfAudioDecoder::open_track(&input, 48_000, Some(1))
            .expect("ordinal 1")
            .reader_stream()
            .expect("ordinal 1 resolves");
        assert_ne!(s0, s1);

        let decode = |stream: u32| {
            let mut d = MfAudioDecoder::open_reader_stream(&input, 48_000, stream)
                .expect("open by reader stream");
            assert_eq!(d.reader_stream(), Some(stream));
            let ch = d.format().channels as usize;
            let mut out = Vec::new();
            for _ in 0..500 {
                match d.next_chunk().expect("decode") {
                    Some(c) => out.extend(c),
                    None => break,
                }
            }
            (ch, out)
        };
        let (ch0, first) = decode(s0);
        let (ch1, second) = decode(s1);
        assert!(!first.is_empty() && !second.is_empty(), "both decode");
        let (a, b) = (zero_crossings(&first, ch0), zero_crossings(&second, ch1));
        assert!(
            (a as f64) < (b as f64) * 0.75,
            "the two reader streams must decode different tracks — {a} vs {b} crossings"
        );
    }

    /// A reader stream that isn't audio (the fixture's video stream) is refused, not
    /// silently swapped for something else; so is a stream index past the table.
    #[test]
    fn open_reader_stream_refuses_non_audio_streams() {
        let input = VideoInput::Path(fixture("multitrack.mp4"));
        // The fixture has 3 streams (2 audio + 1 video); find the video one.
        let audio: Vec<u32> = (0..2)
            .map(|n| {
                MfAudioDecoder::open_track(&input, 48_000, Some(n))
                    .expect("ordinal opens")
                    .reader_stream()
                    .expect("resolves")
            })
            .collect();
        let video = (0..3u32)
            .find(|s| !audio.contains(s))
            .expect("video stream");
        let Err(e) = MfAudioDecoder::open_reader_stream(&input, 48_000, video) else {
            panic!("a video stream must not open as audio");
        };
        assert!(
            format!("{e}").contains("not an audio stream"),
            "unexpected error: {e}"
        );
        assert!(MfAudioDecoder::open_reader_stream(&input, 48_000, 99).is_err());
    }

    /// Diagnostic: what does MF actually enumerate, and does `FIRST_AUDIO_STREAM` agree
    /// with "the 0th audio stream"?
    #[test]
    #[ignore = "diagnostic"]
    fn diag_mf_stream_order() {
        let input = VideoInput::Path(fixture("multitrack.mp4"));
        ensure_mf();
        unsafe {
            let reader: IMFSourceReader = match ReaderSource::new(&input).unwrap() {
                ReaderSource::Url(url) => MFCreateSourceReaderFromURL(&url, None),
                ReaderSource::Stream(bs) => MFCreateSourceReaderFromByteStream(&bs, None),
            }
            .unwrap();
            for i in 0u32..6 {
                match reader.GetNativeMediaType(i, 0) {
                    Ok(ty) => {
                        let major = ty.GetGUID(&MF_MT_MAJOR_TYPE);
                        let is_audio = major == Ok(MFMediaType_Audio);
                        println!("stream {i}: audio={is_audio} major={major:?}");
                    }
                    Err(e) => println!("stream {i}: <none> ({e})"),
                }
            }
            println!("nth_audio_stream(0) = {:?}", nth_audio_stream(&reader, 0));
            println!("nth_audio_stream(1) = {:?}", nth_audio_stream(&reader, 1));
            println!(
                "FIRST_AUDIO_STREAM constant = {:#x}",
                MF_SOURCE_READER_FIRST_AUDIO_STREAM.0
            );
        }
        for t in [None, Some(0), Some(1)] {
            let (ch, s) = decode_track(t);
            let x = zero_crossings(&s, ch);
            println!(
                "track {t:?}: {} samples, {ch}ch, {x} crossings (~{:.0} Hz over 1.02s)",
                s.len(),
                x as f64 / 2.0 / 1.02
            );
        }
    }

    /// Asking for a track the file hasn't got fails cleanly and **names what it found**,
    /// rather than silently falling back to a track the user didn't choose.
    #[test]
    fn a_missing_track_fails_cleanly() {
        let input = VideoInput::Path(fixture("multitrack.mp4"));
        let Err(e) = MfAudioDecoder::open_track(&input, 48_000, Some(9)) else {
            panic!("track 9 does not exist — opening it must fail");
        };
        assert!(
            format!("{e}").contains("no audio track 9"),
            "unexpected error: {e}"
        );
    }

    /// Seek then decode still yields audio (the replay/scrub path).
    #[test]
    fn seek_then_decode_yields_audio() {
        let input = VideoInput::Path(fixture("color_with_tone.mp4"));
        let mut dec = MfAudioDecoder::open(&input, 48_000).expect("open");
        dec.seek(Duration::from_millis(200)).expect("seek");
        let mut total = 0usize;
        for _ in 0..200 {
            match dec.next_chunk().expect("decode") {
                Some(c) => total += c.len(),
                None => break,
            }
        }
        assert!(total > 0, "audio must decode after a seek");
    }
}
