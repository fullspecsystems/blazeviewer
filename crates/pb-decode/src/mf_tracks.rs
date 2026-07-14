//! Media Foundation **media-track enumeration** (task #98, phase 2 / subtask 98.5) —
//! the Windows backend, and the most limited of the three.
//!
//! Walks the source reader's native media types into a [`MediaTrackCatalog`], pairing
//! each one with its stream descriptor for the language + title. Every claim below was
//! measured on this box against the committed fixtures; the findings live in
//! `.taskmaster/docs/98-phase0-spike-findings.md`. The four that shaped this code:
//!
//! 1. **The `GetNativeMediaType(i, 0)` loop terminates on `MF_E_INVALIDSTREAMNUMBER`**
//!    (`0xC00D36B3`) — measured, not assumed. See [`E_INVALIDSTREAMNUMBER`].
//! 2. **Language and title are *not* on `IMFMediaType`.** They are `MF_SD_LANGUAGE` /
//!    `MF_SD_STREAM_NAME` on the **stream descriptor**, which needs the media source's
//!    presentation descriptor. Dumping every attribute by index confirmed the media type
//!    carries neither.
//! 3. **MF exposes no subtitle streams at all** for the containers we ship against — the
//!    fixture MKV's 4 subtitle tracks and the MP4's 2 tx3g tracks both enumerate as
//!    *nothing*. Hence [`TrackCompleteness::Unavailable`] for subtitles: MF's silence is
//!    not evidence of absence, and rendering it as "No subtitles" would be a confident
//!    lie about the user's file.
//! 4. **`IMFPresentationDescriptor`'s `selected` flag is *not* the authored default.** On
//!    the fixture MP4, `ffprobe` reports the *English* track `default=1`, while MF marks
//!    the **French Director's Commentary** `selected=true` — it simply takes the first
//!    stream of the `MF_SD_MUTUALLY_EXCLUSIVE` group. Mapping it to
//!    [`TrackFlags::default`] would label a commentary track "Default", so this backend
//!    reports **no dispositions at all** rather than a plausible-looking guess.
//!
//! The upshot: MF genuinely sees less than FFmpeg does, and the catalog says so through
//! its completeness + `backend` rather than by pretending to a single truth.

use windows::core::{Interface, BOOL, GUID};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaSource, IMFSourceReader, IMFStreamDescriptor, MFAudioFormat_AAC,
    MFAudioFormat_ADTS, MFAudioFormat_ALAC, MFAudioFormat_AMR_NB, MFAudioFormat_AMR_WB,
    MFAudioFormat_AMR_WP, MFAudioFormat_DTS, MFAudioFormat_DTS_HD, MFAudioFormat_DTS_LBR,
    MFAudioFormat_DTS_RAW, MFAudioFormat_DTS_UHD, MFAudioFormat_DTS_UHDY, MFAudioFormat_DTS_XLL,
    MFAudioFormat_Dolby_AC3, MFAudioFormat_Dolby_AC3_SPDIF, MFAudioFormat_Dolby_AC4,
    MFAudioFormat_Dolby_DDPlus, MFAudioFormat_FLAC, MFAudioFormat_Float, MFAudioFormat_LPCM,
    MFAudioFormat_MP3, MFAudioFormat_MPEG, MFAudioFormat_MSP1, MFAudioFormat_Opus,
    MFAudioFormat_PCM, MFAudioFormat_Vorbis, MFAudioFormat_WMASPDIF, MFAudioFormat_WMAudioV8,
    MFAudioFormat_WMAudioV9, MFAudioFormat_WMAudio_Lossless, MFMediaType_Audio,
    MFMediaType_Subtitle, MFSubtitleFormat_ATSC, MFSubtitleFormat_PGS, MFSubtitleFormat_SRT,
    MFSubtitleFormat_SSA, MFSubtitleFormat_TTML, MFSubtitleFormat_VobSub, MFSubtitleFormat_WebVTT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SD_LANGUAGE, MF_SD_STREAM_NAME, MF_SOURCE_READER_MEDIASOURCE,
};

use crate::tracks::{
    self, AudioFormat, MediaBackend, MediaTrack, MediaTrackCatalog, TrackCapability, TrackFlags,
    TrackId, TrackKind, TrackLocator, TrackSet,
};

/// `MF_E_INVALIDSTREAMNUMBER` — the documented "you have walked off the end of the
/// stream table" code, and **measured** to be exactly what terminates the
/// `GetNativeMediaType` loop on every fixture container (MP4, MKV, WebM). The loop stops
/// on *any* error regardless; this constant exists so an unexpected code can be told
/// apart from the ordinary end (see [`catalog_from_reader`]).
const E_INVALIDSTREAMNUMBER: u32 = 0xC00D_36B3;

/// A hard cap on the stream walk. The loop's real terminator is the error above; this
/// only bounds a hypothetical source that never reports one, so a details probe can't
/// spin forever on a hostile file.
const MAX_STREAMS: u32 = 128;

/// Enumerate a reader's audio + subtitle tracks.
///
/// Takes the **already-open reader** the caller is probing with, so Details costs one
/// container open, not two. Read-only: nothing here selects streams, reads samples, or
/// mutates the source.
///
/// `generation` is the probe epoch; every [`TrackId`] carries it so a result landing
/// after the user has navigated away can be dropped rather than resolved against a
/// different file.
///
/// # Safety
/// `reader` must be a live `IMFSourceReader` on an MF-initialized thread (every caller
/// runs `ensure_mf`).
pub(crate) unsafe fn catalog_from_reader(
    reader: &IMFSourceReader,
    generation: u64,
) -> MediaTrackCatalog {
    // Language/title live on the stream descriptors, not the media types (finding 2).
    // `None` if the source won't hand them over — the tracks still enumerate, they just
    // carry no language, which is honest rather than fatal.
    let descriptors = stream_descriptors(reader);

    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    let mut locators = Vec::new();
    // Did the walk reach the end of the stream table, or merely stop? Only the former
    // earns `Complete` — see the `audio` set below.
    let mut walked_to_the_end = false;

    for ordinal in 0..MAX_STREAMS {
        let mt = match reader.GetNativeMediaType(ordinal, 0) {
            Ok(mt) => mt,
            Err(e) => {
                // Finding 1: this is `MF_E_INVALIDSTREAMNUMBER`, the real end of the
                // table. Any *other* error means the walk stopped early for a reason we
                // don't understand, and a set that may be missing tracks must not be
                // called complete.
                walked_to_the_end = e.code().0 as u32 == E_INVALIDSTREAMNUMBER;
                break;
            }
        };
        let Ok(major) = mt.GetGUID(&MF_MT_MAJOR_TYPE) else {
            continue;
        };
        let kind = if major == MFMediaType_Audio {
            TrackKind::Audio
        } else if major == MFMediaType_Subtitle {
            TrackKind::Subtitle
        } else {
            continue; // video (its own VideoStreamInfo) and everything else
        };

        // The stream descriptor at the same index — the pairing is documented, and was
        // cross-checked against `ffprobe` bitrates on the fixture MP4, whose MF ordinals
        // are *reordered* relative to the container (MF ordinal 0 is the container's
        // stream 2). `sd_major` guards the pairing: if the two ever disagreed we would be
        // stapling one track's language onto another, so we drop the metadata instead.
        let sd = descriptors
            .as_ref()
            .and_then(|d| d.get(ordinal as usize))
            .filter(|sd| sd_major(sd) == Some(major));
        let language = sd
            .and_then(|sd| sd_string(sd, &MF_SD_LANGUAGE))
            .and_then(|t| tracks::normalize_lang(&t));
        let title = sd
            .and_then(|sd| sd_string(sd, &MF_SD_STREAM_NAME))
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());

        let codec_raw = mt
            .GetGUID(&MF_MT_SUBTYPE)
            .map(|sub| codec_raw_of(&sub, kind))
            .unwrap_or_default();

        let (codec, capability, audio_fmt) = match kind {
            TrackKind::Audio => (
                // `profile: None` always — MF has no FFmpeg profile int, and the
                // constants collide across codecs, so there is nothing safe to pass.
                tracks::audio_codec_display(&codec_raw, None),
                TrackCapability::Playable,
                Some(AudioFormat {
                    channels: mt
                        .GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
                        .unwrap_or(0)
                        .min(u16::MAX as u32) as u16,
                    // MF's native audio types carry no *named* channel layout (measured:
                    // not one fixture, including the 5.1 AC-3 and the 5.1 AAC, exposes
                    // MF_MT_AUDIO_CHANNEL_MASK). `None` makes the formatter report the
                    // channel count, which is the "never invent 5.1" rule working as
                    // designed — a real, honest degradation vs FFmpeg.
                    layout: None,
                    sample_rate: mt.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(0),
                }),
            ),
            TrackKind::Subtitle => (
                tracks::subtitle_codec_display(&codec_raw),
                tracks::subtitle_capability(&codec_raw),
                None,
            ),
        };

        let local_id = ordinal as u64;
        let track = MediaTrack {
            id: TrackId {
                catalog_generation: generation,
                local_id,
            },
            kind,
            language,
            title,
            codec_raw,
            codec,
            capability,
            // Finding 4: MF exposes no authored dispositions. `selected` is its own
            // pick, not the container's default, so claiming any flag here would be a
            // guess wearing a fact's clothes. The commentary signal still survives via
            // the title, which the formatter reads.
            flags: TrackFlags::none(),
            audio: audio_fmt,
            // Every stream MF walks came out of the container itself; sidecars (#90.1)
            // are discovered beside the file and appended by the caller, never here.
            external: false,
        };
        locators.push((local_id, TrackLocator::MfStream(ordinal)));
        match kind {
            TrackKind::Audio => audio.push(track),
            TrackKind::Subtitle => subtitles.push(track),
        }
    }

    // Audio: MF's stream table matched `ffprobe`'s audio streams exactly on every fixture
    // (2/2 on both multitrack containers, 0 on the silent clip, 1 elsewhere), so a walk
    // that reached the end genuinely saw everything — including the `Complete` +
    // `total: Some(0)` that lets Details say "Audio: No" about a silent clip without
    // lying. A walk that stopped for any other reason gets no such claim: it degrades to
    // what it can back, which is `Partial`, or nothing at all when it found none.
    let audio = match (walked_to_the_end, audio.is_empty()) {
        (true, _) => TrackSet::complete(audio),
        (false, false) => TrackSet::partial(audio, None),
        (false, true) => TrackSet::unavailable(),
    };

    // Subtitles: finding 3 — MF hid every subtitle track in both fixtures, so an empty
    // set here is *not* evidence of absence and must never render as "No". A non-empty
    // one can't be called complete either: having proven MF drops subtitle tracks that
    // exist, we cannot claim it showed us all of them.
    let subtitles = if subtitles.is_empty() {
        TrackSet::unavailable()
    } else {
        TrackSet::partial(subtitles, None)
    };

    let mut catalog =
        MediaTrackCatalog::new(generation, MediaBackend::MediaFoundation, audio, subtitles);
    for (local_id, locator) in locators {
        catalog.set_locator(local_id, locator);
    }
    catalog
}

/// The source's stream descriptors, in presentation order (index == the source reader's
/// stream ordinal).
///
/// Requires the underlying media source — the reader alone cannot answer this, which is
/// the whole of finding 2. `GetServiceForStream(MF_SOURCE_READER_MEDIASOURCE, …)` hands
/// it over without a second open. We only *read* the descriptors: the reader owns the
/// source's lifetime and must be the one to shut it down.
unsafe fn stream_descriptors(reader: &IMFSourceReader) -> Option<Vec<IMFStreamDescriptor>> {
    let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
    reader
        .GetServiceForStream(
            MF_SOURCE_READER_MEDIASOURCE.0 as u32,
            &GUID::zeroed(),
            &IMFMediaSource::IID,
            &mut ptr,
        )
        .ok()?;
    let source = IMFMediaSource::from_raw(ptr);
    // A fresh descriptor each call, reflecting the *source's* authored state — measured
    // to be unaffected by the reader's `SetStreamSelection(ALL, false)`, which
    // `open_video_reader` does before we get here.
    let pd = source.CreatePresentationDescriptor().ok()?;
    let count = pd.GetStreamDescriptorCount().ok()?;
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut selected = BOOL(0);
        let mut sd = None;
        // `selected` is deliberately discarded (finding 4): it is MF's own pick, not the
        // container's authored default.
        pd.GetStreamDescriptorByIndex(i, &mut selected, &mut sd)
            .ok()?;
        out.push(sd?);
    }
    Some(out)
}

/// A stream descriptor's major type, via its media type handler — the guard that keeps a
/// descriptor's language from ever landing on a different stream's track.
unsafe fn sd_major(sd: &IMFStreamDescriptor) -> Option<GUID> {
    sd.GetMediaTypeHandler().ok()?.GetMajorType().ok()
}

/// Read a string attribute off a stream descriptor. Two-call pattern: the length first,
/// then the buffer — MF has no "give me a String" call.
unsafe fn sd_string(sd: &IMFStreamDescriptor, key: &GUID) -> Option<String> {
    let attrs: IMFAttributes = sd.cast().ok()?;
    let len = attrs.GetStringLength(key).ok()?;
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize + 1];
    attrs.GetString(key, &mut buf, None).ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// MF subtype GUID → FFmpeg's `avcodec_get_name` vocabulary, the string the pure maps in
/// [`crate::tracks`] are keyed on. One codec map serves every backend; this is the seam
/// that normalizes into it, exactly as `livephoto::fourcc_to_codec_raw` does for
/// CoreMedia FourCCs.
///
/// An unrecognized subtype returns `""` rather than a wrong guess — the display map then
/// yields an empty codec and the track still lists whatever else it knows.
fn codec_raw_of(sub: &GUID, kind: TrackKind) -> String {
    let named: &[(GUID, &str)] = match kind {
        TrackKind::Audio => &[
            (MFAudioFormat_AAC, "aac"),
            (MFAudioFormat_ADTS, "aac"),
            (MFAudioFormat_Dolby_AC3, "ac3"),
            (MFAudioFormat_Dolby_AC3_SPDIF, "ac3"),
            (MFAudioFormat_Dolby_DDPlus, "eac3"),
            (MFAudioFormat_Dolby_AC4, "ac4"),
            // Every DTS flavour collapses to plain "dts": the variant names
            // ("DTS-HD MA") come from FFmpeg's *profile* int, which MF does not have.
            // Synthesizing one from the GUID would be inventing a fact — "DTS" is the
            // part we can stand behind.
            (MFAudioFormat_DTS, "dts"),
            (MFAudioFormat_DTS_HD, "dts"),
            (MFAudioFormat_DTS_RAW, "dts"),
            (MFAudioFormat_DTS_XLL, "dts"),
            (MFAudioFormat_DTS_LBR, "dts"),
            (MFAudioFormat_DTS_UHD, "dts"),
            (MFAudioFormat_DTS_UHDY, "dts"),
            (MFAudioFormat_MP3, "mp3"),
            (MFAudioFormat_MPEG, "mp2"),
            (MFAudioFormat_FLAC, "flac"),
            (MFAudioFormat_ALAC, "alac"),
            (MFAudioFormat_Opus, "opus"),
            (MFAudioFormat_Vorbis, "vorbis"),
            (MFAudioFormat_PCM, "pcm_s16le"),
            (MFAudioFormat_LPCM, "pcm_s16le"),
            (MFAudioFormat_Float, "pcm_f32le"),
            (MFAudioFormat_WMAudioV8, "wmav2"),
            (MFAudioFormat_WMAudioV9, "wmapro"),
            (MFAudioFormat_WMAudio_Lossless, "wmalossless"),
            (MFAudioFormat_WMASPDIF, "wmav2"),
            (MFAudioFormat_MSP1, "wmavoice"),
            (MFAudioFormat_AMR_NB, "amr_nb"),
            (MFAudioFormat_AMR_WB, "amr_wb"),
            (MFAudioFormat_AMR_WP, "amr_wb"),
        ],
        TrackKind::Subtitle => &[
            (MFSubtitleFormat_SRT, "subrip"),
            (MFSubtitleFormat_SSA, "ssa"),
            (MFSubtitleFormat_PGS, "hdmv_pgs_subtitle"),
            (MFSubtitleFormat_VobSub, "dvd_subtitle"),
            (MFSubtitleFormat_WebVTT, "webvtt"),
            (MFSubtitleFormat_TTML, "ttml"),
            (MFSubtitleFormat_ATSC, "eia_608"),
        ],
    };
    for (g, n) in named {
        if *g == *sub {
            return (*n).to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracks::TrackCompleteness;
    use crate::video::VideoInput;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    fn catalog(name: &str) -> MediaTrackCatalog {
        crate::mf_poster::probe_video_details(&fixture(name), 42)
            .expect("details probe")
            .tracks
    }

    /// Finding 1, pinned: the walk's terminator is exactly `MF_E_INVALIDSTREAMNUMBER`.
    /// Measured on every fixture container; if a future Windows changes the code, this
    /// fails loudly rather than silently truncating (or never ending) the enumeration.
    #[test]
    fn the_native_media_type_walk_ends_on_invalid_stream_number() {
        crate::mf_video::ensure_mf();
        unsafe {
            let reader =
                crate::mf_poster::open_video_reader(&VideoInput::Path(fixture("multitrack.mkv")))
                    .expect("open");
            // The MKV has exactly 3 MF streams (1 video + 2 audio); the 4th must be the
            // documented end, not some other failure.
            assert!(reader.GetNativeMediaType(2, 0).is_ok(), "stream 2 exists");
            let err = reader
                .GetNativeMediaType(3, 0)
                .expect_err("stream 3 must not exist");
            assert_eq!(err.code().0 as u32, E_INVALIDSTREAMNUMBER, "{err}");
            crate::mf_poster::retire_reader(reader);
        }
    }

    /// The MKV's two audio tracks, as MF actually reports them: codecs and channel
    /// counts real, language present but in MF's **BCP-47 short** form ("en"/"fr", where
    /// FFmpeg says "eng"/"fra" for the same file), and the title carrying the commentary.
    #[test]
    fn multitrack_mkv_audio_is_fully_described() {
        let cat = catalog("multitrack.mkv");
        assert_eq!(cat.backend, MediaBackend::MediaFoundation);
        assert_eq!(cat.generation, 42);
        assert_eq!(cat.audio.completeness, TrackCompleteness::Complete);
        assert_eq!(cat.audio.total, Some(2));

        let a0 = &cat.audio.tracks[0];
        assert_eq!(a0.codec_raw, "aac");
        assert_eq!(a0.codec, "AAC");
        assert_eq!(a0.language.as_deref(), Some("en"));
        assert_eq!(a0.capability, TrackCapability::Playable);
        let f0 = a0.audio.as_ref().expect("audio format");
        assert_eq!(f0.channels, 2);
        assert_eq!(f0.sample_rate, 44100);

        let a1 = &cat.audio.tracks[1];
        assert_eq!(a1.codec_raw, "ac3");
        assert_eq!(a1.codec, "AC-3");
        assert_eq!(a1.language.as_deref(), Some("fr"));
        // MF_SD_STREAM_NAME carries the container title — and with no disposition
        // available (finding 4) it is the *only* surviving commentary signal.
        assert_eq!(a1.title.as_deref(), Some("Director's Commentary"));
        let f1 = a1.audio.as_ref().expect("audio format");
        assert_eq!(f1.channels, 6);
        assert_eq!(f1.sample_rate, 48000);
    }

    /// Finding 3, pinned. The fixture has **4** subtitle tracks (SubRip ×3 + PGS) and MF
    /// reports none of them. That must read as "we cannot say", never as "No subtitles" —
    /// the single most important assertion in this file.
    #[test]
    fn mf_sees_no_subtitles_and_refuses_to_call_that_none() {
        for name in ["multitrack.mkv", "multitrack.mp4"] {
            let cat = catalog(name);
            assert_eq!(
                cat.subtitles.completeness,
                TrackCompleteness::Unavailable,
                "{name}"
            );
            // The contract: an empty set is not an empty file.
            assert!(!cat.subtitles.is_known_empty(), "{name}");
            assert_eq!(cat.subtitles.total, None, "{name}");
        }
    }

    /// Finding 4, pinned — the trap this backend exists to avoid. `ffprobe` says the
    /// MP4's **English** track is `default=1`, but MF marks the French *commentary*
    /// `selected=true` (it takes the first of the mutually-exclusive group). Claiming any
    /// disposition off that would label a commentary "Default", so we claim none.
    #[test]
    fn no_dispositions_are_claimed_because_mf_exposes_none() {
        for name in ["multitrack.mkv", "multitrack.mp4"] {
            let cat = catalog(name);
            for t in cat.tracks() {
                assert_eq!(t.flags, TrackFlags::none(), "{name} {:?}", t.codec);
            }
        }
        // ...and specifically: the commentary track is never the one marked Default.
        let cat = catalog("multitrack.mp4");
        let commentary = cat
            .audio
            .tracks
            .iter()
            .find(|t| t.title.as_deref() == Some("Director's Commentary"))
            .expect("the mp4 has a titled commentary track");
        assert!(!commentary.flags.default);
    }

    /// MF **reorders** streams relative to the container: `ffprobe` has the MP4's
    /// English track at index 1 and French at 2, while MF's ordinal 0 is the French one
    /// (cross-checked against the two tracks' bitrates during the spike). That is why the
    /// locator is its own namespace — an FFmpeg index would resolve to the wrong track.
    #[test]
    fn locators_are_mf_ordinals_not_container_indices() {
        let cat = catalog("multitrack.mp4");
        for t in cat.tracks() {
            let want = TrackLocator::MfStream(t.id.local_id as u32);
            assert_eq!(cat.locator(t.id), Some(&want), "{:?}", t.codec);
            assert_eq!(t.id.catalog_generation, 42);
        }
        // The French commentary is MF's stream 0 — the container's stream 2.
        assert_eq!(cat.audio.tracks[0].language.as_deref(), Some("fr"));
        assert_eq!(cat.audio.tracks[0].id.local_id, 0);
        assert_eq!(cat.audio.tracks[1].language.as_deref(), Some("en"));
    }

    /// `Complete` is earned by *reaching the end of the table*, not by "the loop
    /// stopped". Every fixture walks to `MF_E_INVALIDSTREAMNUMBER`, so every fixture is
    /// allowed the claim — an unexpected error mid-walk would drop the set to `Partial`
    /// (or `Unavailable` with nothing found) rather than assert a possibly-truncated list
    /// is the whole file.
    #[test]
    fn complete_audio_is_only_claimed_after_a_clean_end_of_walk() {
        for name in [
            "multitrack.mkv",
            "multitrack.mp4",
            "tone_51.mp4",
            "color_with_tone.mp4",
            "black_then_color.mp4",
            "tone_vp9_opus.webm",
        ] {
            let cat = catalog(name);
            assert_eq!(
                cat.audio.completeness,
                TrackCompleteness::Complete,
                "{name} walks cleanly to the end of its stream table"
            );
            // A `Complete` set's total is derived, so it can never disagree with the list.
            assert_eq!(
                cat.audio.total,
                Some(cat.audio.tracks.len() as u16),
                "{name}"
            );
        }
    }

    /// A silent clip is the one case allowed to say "No": `Complete` + zero.
    #[test]
    fn a_silent_clip_is_complete_and_known_empty() {
        let cat = catalog("black_then_color.mp4");
        assert_eq!(cat.audio.completeness, TrackCompleteness::Complete);
        assert_eq!(cat.audio.total, Some(0));
        assert!(cat.audio.is_known_empty());
        // Subtitles stay unknown even here — MF cannot see them either way, and this
        // clip having none is not something MF is in a position to tell us.
        assert!(!cat.subtitles.is_known_empty());
    }

    #[test]
    fn single_mono_aac_clip() {
        let cat = catalog("color_with_tone.mp4");
        assert_eq!(cat.audio.tracks.len(), 1);
        let f = cat.audio.tracks[0].audio.as_ref().expect("audio format");
        assert_eq!(f.channels, 1);
        // `und` carries no information and must not surface as a language.
        assert_eq!(cat.audio.tracks[0].language, None);
    }

    /// The "never invent 5.1" rule, MF edition: a real 5.1 track reports **6 channels**
    /// and no layout, because MF's native type genuinely never names one. The formatter
    /// then prints the count — an honest degradation vs FFmpeg's "5.1", not a guess.
    #[test]
    fn a_51_track_reports_channels_but_never_an_invented_layout() {
        let cat = catalog("tone_51.mp4");
        let f = cat.audio.tracks[0].audio.as_ref().expect("audio format");
        assert_eq!(f.channels, 6);
        assert_eq!(f.layout, None, "MF names no layout; we must not invent one");
    }

    /// Opus in WebM — a non-MP4/MKV container and a codec that only resolves through the
    /// GUID map.
    #[test]
    fn webm_opus_audio_maps_through_the_guid_table() {
        let cat = catalog("tone_vp9_opus.webm");
        assert_eq!(cat.audio.tracks.len(), 1);
        assert_eq!(cat.audio.tracks[0].codec_raw, "opus");
        assert_eq!(cat.audio.tracks[0].codec, "Opus");
        assert_eq!(
            cat.audio.tracks[0].audio.as_ref().expect("fmt").sample_rate,
            48000
        );
    }

    /// An unmapped subtype must not fabricate a codec.
    #[test]
    fn an_unknown_subtype_maps_to_an_empty_codec_not_a_guess() {
        assert_eq!(codec_raw_of(&GUID::zeroed(), TrackKind::Audio), "");
        assert_eq!(codec_raw_of(&GUID::zeroed(), TrackKind::Subtitle), "");
        // ...and the audio/subtitle tables are read per-kind, so a subtitle GUID can
        // never resolve to an audio codec name.
        assert_eq!(codec_raw_of(&MFSubtitleFormat_SRT, TrackKind::Audio), "");
        assert_eq!(codec_raw_of(&MFAudioFormat_AAC, TrackKind::Subtitle), "");
        assert_eq!(codec_raw_of(&MFAudioFormat_AAC, TrackKind::Audio), "aac");
    }

    /// Every DTS subtype collapses to plain "DTS" — MF has no profile int, and the
    /// marketing names are profile-derived. Better plain-and-right than "DTS-HD MA"
    /// invented from a GUID.
    #[test]
    fn dts_subtypes_collapse_to_plain_dts_rather_than_invent_a_profile() {
        for g in [
            MFAudioFormat_DTS,
            MFAudioFormat_DTS_HD,
            MFAudioFormat_DTS_XLL,
            MFAudioFormat_DTS_RAW,
        ] {
            let raw = codec_raw_of(&g, TrackKind::Audio);
            assert_eq!(raw, "dts");
            assert_eq!(tracks::audio_codec_display(&raw, None), "DTS");
        }
    }

    /// The catalog must agree with the `ffprobe` oracle on the facts MF *does* report —
    /// the same file read by a different demuxer. Guards a mis-paired stream descriptor
    /// (the language of one track landing on another), which a self-consistent test
    /// cannot see. Deliberately compares *sets*, not order: MF reorders streams, and
    /// that disagreement is allowed.
    #[test]
    fn audio_tracks_match_the_ffprobe_oracle() {
        let path = fixture("multitrack.mkv");
        let out = match std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "a",
                "-show_entries",
                "stream=channels,sample_rate:stream_tags=language,title",
                "-of",
                "json",
            ])
            .arg(&path)
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            // ffprobe is a dev convenience, not a build dep.
            _ => {
                eprintln!("skipping oracle: ffprobe unavailable");
                return;
            }
        };

        let cat = catalog("multitrack.mkv");
        assert_eq!(out.matches("\"channels\"").count(), cat.audio.tracks.len());
        for t in cat.tracks() {
            if let Some(f) = &t.audio {
                assert!(
                    out.contains(&format!("\"channels\": {}", f.channels)),
                    "ffprobe never mentions {} channels",
                    f.channels
                );
                assert!(out.contains(&format!("\"sample_rate\": \"{}\"", f.sample_rate)));
            }
            if let Some(title) = &t.title {
                assert!(
                    out.contains(title),
                    "ffprobe never mentions title {title:?}"
                );
            }
            // MF says "en"/"fr" where ffprobe says "eng"/"fra": the *language* must
            // agree, not the tag spelling. Both must resolve to the same display name.
            if let Some(l) = &t.language {
                let display = tracks::language_display(l);
                assert!(
                    ["English", "French"].contains(&display.as_str()),
                    "unexpected language {l:?} -> {display}"
                );
            }
        }
    }
}
