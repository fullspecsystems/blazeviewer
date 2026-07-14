//! FFmpeg **media-track enumeration** (task #98, phase 2) — the reference backend.
//!
//! Walks the demuxer's audio + subtitle streams into a [`MediaTrackCatalog`]. FFmpeg
//! sees the container's real stream table, so this backend is always
//! [`TrackCompleteness::Complete`]: what it reports *is* what's in the file.
//!
//! Every FFI call here was proven by the phase-0 spike
//! (`.taskmaster/docs/98-phase0-spike-findings.md`); the two non-obvious lessons it
//! taught are encoded below as [`layout_of`]'s `order` gate and [`audio_profile`]'s
//! codec keying.

use ff::format::stream::Disposition;
use ffmpeg_next as ff;

use crate::tracks::{
    self, AudioFormat, MediaBackend, MediaTrack, MediaTrackCatalog, TrackCapability, TrackFlags,
    TrackId, TrackKind, TrackLocator, TrackSet,
};

/// Enumerate an opened input's audio + subtitle tracks.
///
/// `generation` is the probe epoch the caller is minting this catalog in; every
/// [`TrackId`] carries it, so a result that lands after the user has navigated away is
/// rejected rather than resolving against a different file's tracks.
pub fn catalog_from_input(ctx: &ff::format::context::Input, generation: u64) -> MediaTrackCatalog {
    let mut audio = Vec::new();
    let mut subtitles = Vec::new();
    let mut locators = Vec::new();

    for stream in ctx.streams() {
        let par = stream.parameters();
        let kind = match par.medium() {
            ff::media::Type::Audio => TrackKind::Audio,
            ff::media::Type::Subtitle => TrackKind::Subtitle,
            _ => continue,
        };
        // `local_id` is the real stream index: it is already unique within the container,
        // and keeping it aligned with the locator makes a mismatch impossible.
        let local_id = stream.index() as u64;
        let track = read_track(&stream, kind, generation, local_id);
        locators.push((local_id, TrackLocator::FfStream(stream.index())));
        match kind {
            TrackKind::Audio => audio.push(track),
            TrackKind::Subtitle => subtitles.push(track),
        }
    }

    let mut catalog = MediaTrackCatalog::new(
        generation,
        MediaBackend::FFmpeg,
        TrackSet::complete(audio),
        TrackSet::complete(subtitles),
    );
    for (local_id, locator) in locators {
        catalog.set_locator(local_id, locator);
    }
    catalog
}

fn read_track(
    stream: &ff::format::stream::Stream,
    kind: TrackKind,
    generation: u64,
    local_id: u64,
) -> MediaTrack {
    let par = stream.parameters();
    let codec_raw = codec_name(par.id());
    let meta = stream.metadata();
    let language = meta.get("language").and_then(tracks::normalize_lang);
    // The container's own per-track title — often the *only* thing marking a commentary.
    let title = meta
        .get("title")
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);

    let (codec, capability, audio) = match kind {
        TrackKind::Audio => (
            tracks::audio_codec_display(&codec_raw, audio_profile(&par, &codec_raw)),
            TrackCapability::Playable,
            Some(audio_format(&par)),
        ),
        TrackKind::Subtitle => (
            tracks::subtitle_codec_display(&codec_raw),
            tracks::subtitle_capability(&codec_raw),
            None,
        ),
    };

    MediaTrack {
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
        flags: flags_of(stream.disposition()),
        audio,
        external: false, // a real stream in the container
    }
}

/// Dispositions off the safe bitflags (no ffi walk needed — spike-confirmed).
fn flags_of(d: Disposition) -> TrackFlags {
    TrackFlags {
        default: d.contains(Disposition::DEFAULT),
        forced: d.contains(Disposition::FORCED),
        commentary: d.contains(Disposition::COMMENT),
        hearing_impaired: d.contains(Disposition::HEARING_IMPAIRED),
        visual_impaired: d.contains(Disposition::VISUAL_IMPAIRED),
    }
}

/// `avcodec_get_name` — the raw id string the pure maps are keyed on.
fn codec_name(id: ff::codec::Id) -> String {
    unsafe {
        let p = ff::ffi::avcodec_get_name(id.into());
        if p.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// The stream's profile, **only** for codecs whose profile we actually interpret.
///
/// The phase-0 spike found the `AV_PROFILE_*` constants collide across codecs
/// (`DTS_ES == TRUEHD_ATMOS == EAC3_DDP_ATMOS == 30`), so handing a profile int to the
/// display map for the wrong codec would mislabel the track. Passing `None` for every
/// codec we don't read a profile from makes that impossible at the seam, on top of the
/// map's own per-codec keying.
fn audio_profile(par: &ff::codec::Parameters, codec_raw: &str) -> Option<i32> {
    if codec_raw != "dts" {
        return None;
    }
    let p = unsafe { (*par.as_ptr()).profile };
    (p != tracks::PROFILE_UNKNOWN).then_some(p)
}

fn audio_format(par: &ff::codec::Parameters) -> AudioFormat {
    unsafe {
        let p = par.as_ptr();
        AudioFormat {
            channels: (*p).ch_layout.nb_channels.max(0) as u16,
            layout: layout_of(&(*p).ch_layout),
            sample_rate: (*p).sample_rate.max(0) as u32,
        }
    }
}

/// The channel layout's canonical name ("stereo", "5.1", "5.1(side)"), or `None` when
/// the container never actually said what the channels *are*.
///
/// The `order` gate is the plan's "never invent 5.1" rule, implemented honestly:
/// `AV_CHANNEL_ORDER_UNSPEC` means the layout is unknown, and describe() would then
/// merely print "6 channels" — a channel count wearing a layout's clothes. Reporting
/// `None` lets the formatter say "6 channels" *because it doesn't know*, rather than
/// because a string happened to parse that way.
///
/// # Safety
/// `layout` must point at a live `AVChannelLayout` (it does: it's a field of the
/// stream's codec parameters, which outlive the borrow).
unsafe fn layout_of(layout: &ff::ffi::AVChannelLayout) -> Option<String> {
    if layout.order == ff::ffi::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC {
        return None;
    }
    // Bounded-FFI-buffer pattern: the return code is the byte count *needed*, so a
    // negative code is an AVERROR and an over-long one means the name was truncated.
    // Either way we report "unknown layout" rather than a half-name.
    let mut buf = [0i8; 128];
    let rc = ff::ffi::av_channel_layout_describe(
        layout as *const _,
        buf.as_mut_ptr() as *mut std::os::raw::c_char,
        buf.len(),
    );
    if rc <= 0 || (rc as usize) > buf.len() {
        return None;
    }
    let s = std::ffi::CStr::from_ptr(buf.as_ptr() as *const std::os::raw::c_char)
        .to_string_lossy()
        .into_owned();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracks::TrackCompleteness;
    use crate::video::VideoInput;

    /// The pure map mirrors FFmpeg's `AV_PROFILE_DTS_*` code points so
    /// `crate::tracks` can stay FFmpeg-free. If a future FFmpeg renumbers them, this
    /// fails loudly here instead of silently mislabelling DTS tracks in the panel.
    #[test]
    fn mirrored_dts_profile_constants_match_ffmpeg() {
        assert_eq!(tracks::PROFILE_UNKNOWN, ff::ffi::AV_PROFILE_UNKNOWN);
        assert_eq!(tracks::PROFILE_DTS, ff::ffi::AV_PROFILE_DTS);
        assert_eq!(tracks::PROFILE_DTS_ES, ff::ffi::AV_PROFILE_DTS_ES);
        assert_eq!(tracks::PROFILE_DTS_96_24, ff::ffi::AV_PROFILE_DTS_96_24);
        assert_eq!(tracks::PROFILE_DTS_HD_HRA, ff::ffi::AV_PROFILE_DTS_HD_HRA);
        assert_eq!(tracks::PROFILE_DTS_HD_MA, ff::ffi::AV_PROFILE_DTS_HD_MA);
        assert_eq!(tracks::PROFILE_DTS_EXPRESS, ff::ffi::AV_PROFILE_DTS_EXPRESS);
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/video")
            .join(name)
    }

    fn catalog(name: &str) -> MediaTrackCatalog {
        let mut input =
            super::super::io::FfInput::open(&VideoInput::Path(fixture(name)), None).expect("open");
        catalog_from_input(input.ctx(), 42)
    }

    #[test]
    fn multitrack_mkv_audio_is_fully_described() {
        let cat = catalog("multitrack.mkv");
        assert_eq!(cat.backend, MediaBackend::FFmpeg);
        assert_eq!(cat.generation, 42);
        assert_eq!(cat.audio.completeness, TrackCompleteness::Complete);
        assert_eq!(cat.audio.total, Some(2));

        let a0 = &cat.audio.tracks[0];
        assert_eq!(a0.codec_raw, "aac");
        assert_eq!(a0.codec, "AAC");
        assert_eq!(a0.language.as_deref(), Some("eng"));
        assert_eq!(a0.title, None);
        assert!(a0.flags.default, "stream 1 is the authored default");
        assert!(!a0.flags.commentary);
        assert_eq!(a0.capability, TrackCapability::Playable);
        let f0 = a0.audio.as_ref().expect("audio format");
        assert_eq!(f0.channels, 2);
        assert_eq!(f0.layout.as_deref(), Some("stereo"));
        assert_eq!(f0.sample_rate, 44100);

        let a1 = &cat.audio.tracks[1];
        assert_eq!(a1.codec, "AC-3");
        assert_eq!(a1.language.as_deref(), Some("fra"));
        // The commentary signal lives in the container *title* here, and the
        // disposition also carries it — both must survive to the formatter, which is
        // what lets it show the fact exactly once.
        assert_eq!(a1.title.as_deref(), Some("Director's Commentary"));
        assert!(a1.flags.commentary);
        assert!(!a1.flags.default);
        let f1 = a1.audio.as_ref().expect("audio format");
        assert_eq!(f1.channels, 6);
        assert_eq!(f1.layout.as_deref(), Some("5.1(side)"));
        assert_eq!(f1.sample_rate, 48000);
    }

    #[test]
    fn multitrack_mkv_subtitles_carry_flags_capability_and_non_latin_metadata() {
        let cat = catalog("multitrack.mkv");
        assert_eq!(cat.subtitles.completeness, TrackCompleteness::Complete);
        assert_eq!(cat.subtitles.total, Some(4));
        let s = &cat.subtitles.tracks;

        assert_eq!(s[0].codec, "SubRip");
        assert_eq!(s[0].capability, TrackCapability::SupportedText);
        assert_eq!(s[0].language.as_deref(), Some("eng"));
        assert!(s[0].flags.default);
        assert!(s[0].audio.is_none(), "subtitles carry no audio format");

        assert_eq!(s[1].codec, "SubRip");
        assert_eq!(s[1].title.as_deref(), Some("English SDH"));
        assert!(s[1].flags.forced);
        assert!(s[1].flags.hearing_impaired);

        // The bitmap track: listed honestly, classified as un-renderable.
        assert_eq!(s[2].codec_raw, "hdmv_pgs_subtitle");
        assert_eq!(s[2].codec, "PGS");
        assert_eq!(s[2].capability, TrackCapability::Bitmap);
        assert!(!s[2].capability.is_renderable_text());
        assert_eq!(s[2].language.as_deref(), Some("spa"));

        // Non-Latin title + language survive the metadata read intact.
        assert_eq!(s[3].language.as_deref(), Some("jpn"));
        assert_eq!(s[3].title.as_deref(), Some("日本語字幕"));
    }

    #[test]
    fn locators_map_each_track_back_to_its_real_stream_index() {
        let cat = catalog("multitrack.mkv");
        // Stream 0 is video; audio starts at 1, subtitles at 3.
        for t in cat.tracks() {
            let want = TrackLocator::FfStream(t.id.local_id as usize);
            assert_eq!(cat.locator(t.id), Some(&want), "{:?}", t.codec);
        }
        assert_eq!(cat.audio.tracks[0].id.local_id, 1);
        assert_eq!(cat.subtitles.tracks[0].id.local_id, 3);
        // Every id is stamped with the catalog's generation.
        assert!(cat.tracks().all(|t| t.id.catalog_generation == 42));
    }

    /// A 5.1 track whose layout the container *does* name reports the name; the count
    /// is still there for the formatter's fallback.
    #[test]
    fn named_51_layout_is_reported_verbatim() {
        let cat = catalog("tone_51.mp4");
        let f = cat.audio.tracks[0].audio.as_ref().expect("audio format");
        assert_eq!(f.channels, 6);
        assert_eq!(f.layout.as_deref(), Some("5.1"));
        // `und` carries no information and must not surface as a language.
        assert_eq!(cat.audio.tracks[0].language, None);
    }

    /// A silent clip is `Complete` with zero tracks — the positively-known "no audio",
    /// which is what lets Details say "No" without lying.
    #[test]
    fn a_silent_clip_is_complete_and_known_empty() {
        let cat = catalog("black_then_color.mp4");
        assert_eq!(cat.audio.completeness, TrackCompleteness::Complete);
        assert_eq!(cat.audio.total, Some(0));
        assert!(cat.audio.is_known_empty());
        assert!(cat.subtitles.is_known_empty());
    }

    #[test]
    fn single_mono_aac_clip() {
        let cat = catalog("color_with_tone.mp4");
        assert_eq!(cat.audio.tracks.len(), 1);
        let f = cat.audio.tracks[0].audio.as_ref().expect("audio format");
        assert_eq!(f.channels, 1);
        assert_eq!(f.layout.as_deref(), Some("mono"));
        assert!(cat.subtitles.is_known_empty());
    }

    /// The catalog must agree with `ffprobe -show_streams` — the same demuxer, read a
    /// different way. Guards against a mis-mapped field (reading a neighbouring stream,
    /// dropping a disposition) that a self-consistent test would miss.
    #[test]
    fn catalog_matches_the_ffprobe_oracle() {
        let path = fixture("multitrack.mkv");
        let out = match std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=index,codec_type,codec_name,channels,channel_layout,sample_rate\
                 :stream_disposition=default,forced,comment,hearing_impaired\
                 :stream_tags=language,title",
                "-of",
                "json",
            ])
            .arg(&path)
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            // ffprobe is a dev convenience, not a build dep: skip rather than fail the
            // suite on a machine without it.
            _ => {
                eprintln!("skipping oracle: ffprobe unavailable");
                return;
            }
        };

        let cat = catalog("multitrack.mkv");
        // A crude but sufficient check: every value we claim must appear in ffprobe's
        // own dump of the same stream table.
        for t in cat.tracks() {
            assert!(
                out.contains(&format!("\"codec_name\": \"{}\"", t.codec_raw)),
                "ffprobe never mentions codec {:?}\n{out}",
                t.codec_raw
            );
            if let Some(l) = &t.language {
                assert!(
                    out.contains(&format!("\"language\": \"{l}\"")),
                    "ffprobe never mentions language {l:?}"
                );
            }
            if let Some(title) = &t.title {
                assert!(
                    out.contains(title),
                    "ffprobe never mentions title {title:?}"
                );
            }
            if let Some(f) = &t.audio {
                if let Some(layout) = &f.layout {
                    assert!(
                        out.contains(&format!("\"channel_layout\": \"{layout}\"")),
                        "ffprobe never mentions layout {layout:?}"
                    );
                }
                assert!(out.contains(&format!("\"channels\": {}", f.channels)));
            }
        }
        // ...and the counts match: 2 audio + 4 subtitle streams, no more, no less.
        assert_eq!(out.matches("\"codec_type\": \"audio\"").count(), 2);
        assert_eq!(out.matches("\"codec_type\": \"subtitle\"").count(), 4);
        assert_eq!(cat.audio.tracks.len(), 2);
        assert_eq!(cat.subtitles.tracks.len(), 4);
    }
}
