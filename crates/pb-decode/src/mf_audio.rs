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

/// A pull-based MF audio decoder over one clip's first audio stream. Created and
/// used entirely on the owning audio thread (the WASAPI render thread in the
/// shell), so it never crosses threads — no `Send` needed.
pub struct MfAudioDecoder {
    reader: IMFSourceReader,
    format: MfAudioFormat,
    eos: bool,
}

impl MfAudioDecoder {
    /// Open the audio stream of `input`, decoding to interleaved **f32** at
    /// `sample_rate` (MF resamples) and the source's channel count. `Err` if the
    /// clip has no audio stream, or MF can't decode it.
    pub fn open(input: &VideoInput, sample_rate: u32) -> Result<MfAudioDecoder, DecodeError> {
        ensure_mf();
        unsafe {
            let reader: IMFSourceReader = match ReaderSource::new(input)
                .map_err(|e| DecodeError::Corrupt(format!("Media Foundation: {e}")))?
            {
                ReaderSource::Url(url) => MFCreateSourceReaderFromURL(&url, None),
                ReaderSource::Stream(bs) => MFCreateSourceReaderFromByteStream(&bs, None),
            }
            .map_err(|e| DecodeError::Corrupt(format!("Media Foundation: {e}")))?;

            let audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
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
                eos: false,
            })
        }
    }

    pub fn format(&self) -> MfAudioFormat {
        self.format
    }

    /// Pull the next decoded chunk as interleaved f32 samples. `Ok(None)` = the
    /// stream ended (further calls keep returning `None`). A gap tick (no sample,
    /// not EOS) yields an empty vec so the caller loops without treating it as EOS.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<f32>>, DecodeError> {
        if self.eos {
            return Ok(None);
        }
        let audio = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
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
