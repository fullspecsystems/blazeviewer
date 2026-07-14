//! The shared **media-track formatter** (task #98, phase 3): one place that turns a
//! [`MediaTrack`] into the line a human reads.
//!
//! Lives here, not in `panels.rs`, because that module is deliberately pb-decode-free.
//! Consumed by the Inspector ▸ Details listing now and by the #99 audio/subtitle picker
//! later — one function, so a track can never describe itself two different ways in two
//! places.
//!
//! ```text
//! Audio:    "English · DTS-HD MA 5.1 · 48 kHz · Default"
//! Subtitle: "English (SDH) · SubRip · Forced"
//! Unknown language leads with the codec: "AC-3 · 6 channels · 48 kHz"
//! ```

use pb_decode::tracks::{
    language_display, MediaTrack, MediaTrackCatalog, TrackCapability, TrackCompleteness,
    TrackFlags, TrackKind, TrackSet,
};

use crate::panels::DetailRow;

/// The separator between a summary's tokens.
const SEP: &str = " · ";

/// A one-line human summary of a track.
///
/// Ordering: language (when known) → codec (+ layout/channels for audio) → sample rate →
/// dispositions. An unknown language simply *leads with the codec* rather than emitting an
/// "Unknown" token, so the row stays informative instead of apologizing.
///
/// Every fact appears **at most once**: SDH annotates the language rather than also
/// adding a "Hearing impaired" tail, and a commentary marked by both title and
/// disposition says "Commentary" once. The raw stream index deliberately stays out — it's
/// in `codec_raw`/diagnostics, not in a line meant for a person.
pub fn track_summary(track: &MediaTrack) -> String {
    let mut parts: Vec<String> = Vec::new();

    // 1. Language, annotated with the accessibility variant it *is* (SDH/AD), so the
    //    disposition tail doesn't have to repeat it.
    if let Some(lang) = &track.language {
        let name = language_display(lang);
        parts.push(match track.kind {
            TrackKind::Subtitle if track.flags.hearing_impaired => format!("{name} (SDH)"),
            TrackKind::Audio if track.flags.visual_impaired => {
                format!("{name} (Audio description)")
            }
            _ => name,
        });
    }

    // 2. The codec token, with an audio track's layout folded onto it ("DTS-HD MA 5.1").
    let mut codec = track.codec.clone();
    if let Some(fmt) = &track.audio {
        // The layout name when the container actually named one; otherwise the channel
        // *count* — we never infer "5.1" from `channels == 6`.
        match (&fmt.layout, fmt.channels) {
            (Some(layout), _) if !layout.is_empty() => {
                if codec.is_empty() {
                    codec = layout.clone();
                } else {
                    codec = format!("{codec} {layout}");
                }
            }
            (_, 0) => {}
            (_, n) => {
                let count = if n == 1 {
                    "1 channel".to_string()
                } else {
                    format!("{n} channels")
                };
                if codec.is_empty() {
                    codec = count;
                } else {
                    parts.push(codec);
                    codec = count;
                }
            }
        }
    }
    if !codec.is_empty() {
        parts.push(codec);
    }

    // 3. Sample rate.
    if let Some(fmt) = &track.audio {
        if fmt.sample_rate > 0 {
            parts.push(format_sample_rate(fmt.sample_rate));
        }
    }

    // 4. A title, but only when it *adds* signal — a title that merely restates the
    //    commentary disposition would duplicate the tail below.
    if let Some(title) = title_token(track) {
        parts.push(title);
    }

    // 5. Stable-ordered disposition tail.
    parts.extend(disposition_tokens(track));

    // 6. Where it came from — but only when that is the thing telling two rows apart.
    //
    // Found on real data: a release ships an embedded English SubRip stream *and* an
    // `.eng.srt` of the same content beside it, and both rendered as exactly
    // "English · SubRip". Two identical rows read as a bug, and in the #99 picker they'd
    // be an unanswerable choice.
    if track.external {
        parts.push("External".to_string());
    }

    // 7. An honest note on what we cannot render, so a listed track never reads as an
    //    offer. Last, so it is the final word on the row. (Bitmap/unsupported subtitles
    //    are shown in Details but will never be selectable in the #99 picker.)
    if track.kind == TrackKind::Subtitle && !track.capability.is_renderable_text() {
        parts.push("Unsupported".to_string());
    }

    parts.join(SEP)
}

/// Whether a track's title is worth showing, and as what.
///
/// A container title is the *only* commentary signal on many files, so it must survive —
/// but when it plainly restates a disposition we already print ("Commentary"), showing
/// both would read as duplication.
fn title_token(track: &MediaTrack) -> Option<String> {
    let title = track.title.as_deref()?.trim();
    if title.is_empty() {
        return None;
    }
    // A commentary title is always restated by the tail's "Commentary" — which the tail
    // emits off the title itself when no disposition carries it — so the title would be
    // a duplicate either way. (Gating this on `flags.commentary` was a bug: a
    // title-only commentary printed "Director's Commentary · Commentary".)
    if looks_like_commentary(title) {
        return None;
    }
    // Likewise a title that only echoes the language ("English") or the SDH annotation.
    if let Some(lang) = &track.language {
        if title.eq_ignore_ascii_case(&language_display(lang)) {
            return None;
        }
    }
    if track.flags.hearing_impaired && looks_like_sdh(title) {
        return None;
    }
    Some(title.to_string())
}

fn looks_like_commentary(title: &str) -> bool {
    title.to_ascii_lowercase().contains("commentary")
}

fn looks_like_sdh(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    t.contains("sdh") || t.contains("hearing impaired") || t.contains("hearing-impaired")
}

/// The disposition tail, in a stable order so two tracks never reorder relative to each
/// other. SDH / audio-description are **not** here: they already annotated the language.
fn disposition_tokens(track: &MediaTrack) -> Vec<String> {
    let TrackFlags {
        default,
        forced,
        commentary,
        hearing_impaired,
        visual_impaired,
    } = track.flags;
    let mut out = Vec::new();
    if default {
        out.push("Default".to_string());
    }
    if forced {
        out.push("Forced".to_string());
    }
    if commentary || track.title.as_deref().is_some_and(looks_like_commentary) {
        out.push("Commentary".to_string());
    }
    // The annotation above only fires when there *is* a language to annotate; with no
    // language the fact would otherwise be lost, so report it here instead.
    if hearing_impaired && (track.language.is_none() || track.kind != TrackKind::Subtitle) {
        out.push("Hearing impaired".to_string());
    }
    if visual_impaired && (track.language.is_none() || track.kind != TrackKind::Audio) {
        out.push("Audio description".to_string());
    }
    out
}

/// A sample rate in kHz — derived, not looked up in a table of the rates we thought of:
/// 48000 → "48 kHz", 44100 → "44.1 kHz", 22050 → "22.05 kHz", 176400 → "176.4 kHz".
///
/// Two decimals is what the family actually needs (22.05 is the shortest rate that loses
/// information at one), and trailing zeros are trimmed so the common rates stay clean.
pub fn format_sample_rate(hz: u32) -> String {
    let khz = hz as f64 / 1000.0;
    let mut s = format!("{khz:.2}");
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    format!("{s} kHz")
}

/// The label for a listed-but-unusable subtitle track — kept next to the formatter so
/// Details and the #99 picker agree on the wording.
pub fn capability_note(cap: TrackCapability) -> Option<&'static str> {
    match cap {
        TrackCapability::Bitmap | TrackCapability::Unsupported => Some("Unsupported"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Details rows (task #98, phase 3).
// ---------------------------------------------------------------------------

/// What we say about a track we know exists but cannot describe.
const NO_DETAIL: &str = "Details unavailable";

/// The Inspector ▸ Details rows for a catalog: an `Audio` section then a `Subtitles`
/// one.
///
/// **Driven by completeness, never by `tracks.is_empty()`** — that is the whole reason
/// the catalog carries it. Only a positively-`Complete` empty set is allowed to say
/// "No"; a probe that failed, or a backend that can't enumerate, says so. Turning an
/// enumeration error into "No audio" would be a confident lie about the user's file.
///
/// `audio_present` is the basic probe's **independent** audio-presence fact — the
/// fallback that lets an un-enumerable container still report something true. It must
/// not be derived from the catalog: an un-enumerable set is empty either way, so that
/// would just echo whatever the emptiness implied. `None` = we don't know.
pub fn track_rows(catalog: &MediaTrackCatalog, audio_present: Option<bool>) -> Vec<DetailRow> {
    let mut rows = track_section("Audio", &catalog.audio, audio_present);
    // No independent subtitle-presence fact exists, so an un-enumerable subtitle set
    // says nothing at all rather than guessing in either direction.
    rows.extend(track_section("Subtitles", &catalog.subtitles, None));
    rows
}

/// One kind's rows. `presence_hint` is an independently-known "does this file have any
/// of these", used only when the set itself is [`TrackCompleteness::Unavailable`].
fn track_section(label: &str, set: &TrackSet, presence_hint: Option<bool>) -> Vec<DetailRow> {
    let pair = |l: &str, v: &str| DetailRow::Pair {
        label: l.to_string(),
        value: v.to_string(),
    };

    // The positively-known "none" — useful information, so it's kept as a plain row.
    if set.is_known_empty() {
        return vec![pair(label, "No")];
    }

    match set.completeness {
        TrackCompleteness::Unavailable => match presence_hint {
            // We know it's there, we just couldn't describe it. Say exactly that.
            Some(true) => vec![pair(label, "Present — details unavailable")],
            Some(false) => vec![pair(label, "No")],
            None => Vec::new(),
        },
        TrackCompleteness::CountOnly => {
            let n = set.total.unwrap_or(0);
            if n == 0 {
                // "I counted, but I can't say what" with a zero count says nothing at
                // all — it is *not* the same as a Complete zero, so it must not say "No".
                return Vec::new();
            }
            let mut rows = vec![heading(label)];
            for i in 1..=n {
                rows.push(pair(&format!("Track {i}"), NO_DETAIL));
            }
            rows
        }
        TrackCompleteness::Complete | TrackCompleteness::Partial => {
            let mut rows = vec![heading(label)];
            for (i, t) in set.tracks.iter().enumerate() {
                rows.push(pair(&format!("Track {}", i + 1), &track_summary(t)));
            }
            if set.completeness == TrackCompleteness::Partial {
                // Say what we don't know, rather than let the list imply completeness.
                // A full-width note, not a `Pair` with an empty label (which would draw
                // a blank left column).
                let more = set
                    .total
                    .map(|total| total as usize)
                    .filter(|total| *total > set.tracks.len())
                    .map(|total| format!("{} more — {NO_DETAIL}", total - set.tracks.len()))
                    .unwrap_or_else(|| format!("More tracks exist — {NO_DETAIL}"));
                rows.push(DetailRow::Span {
                    text: more,
                    bold: false,
                });
            }
            rows
        }
    }
}

fn heading(label: &str) -> DetailRow {
    DetailRow::Span {
        text: label.to_string(),
        bold: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb_decode::tracks::{
        AudioFormat, MediaTrack, TrackCapability, TrackFlags, TrackId, TrackKind,
    };

    fn audio(codec: &str, channels: u16, layout: Option<&str>, rate: u32) -> MediaTrack {
        MediaTrack {
            id: TrackId {
                catalog_generation: 1,
                local_id: 0,
            },
            kind: TrackKind::Audio,
            language: Some("eng".into()),
            title: None,
            codec_raw: codec.to_ascii_lowercase(),
            codec: codec.into(),
            capability: TrackCapability::Playable,
            flags: TrackFlags::none(),
            audio: Some(AudioFormat {
                channels,
                layout: layout.map(str::to_string),
                sample_rate: rate,
            }),
            external: false,
        }
    }

    fn subtitle(codec: &str, cap: TrackCapability) -> MediaTrack {
        MediaTrack {
            id: TrackId {
                catalog_generation: 1,
                local_id: 0,
            },
            kind: TrackKind::Subtitle,
            language: Some("eng".into()),
            title: None,
            codec_raw: codec.to_ascii_lowercase(),
            codec: codec.into(),
            capability: cap,
            flags: TrackFlags::none(),
            audio: None,
            external: false,
        }
    }

    #[test]
    fn full_audio_track_reads_language_codec_layout_rate_default() {
        let mut t = audio("DTS-HD MA", 6, Some("5.1"), 48000);
        t.flags.default = true;
        assert_eq!(
            track_summary(&t),
            "English · DTS-HD MA 5.1 · 48 kHz · Default"
        );
    }

    #[test]
    fn subtitle_sdh_annotates_the_language_and_never_repeats_itself() {
        let mut t = subtitle("SubRip", TrackCapability::SupportedText);
        t.flags.hearing_impaired = true;
        t.flags.forced = true;
        let s = track_summary(&t);
        assert_eq!(s, "English (SDH) · SubRip · Forced");
        // The fact appears exactly once — annotated on the language, not also in the tail.
        assert_eq!(s.matches("SDH").count(), 1);
        assert!(!s.contains("Hearing impaired"));
    }

    /// The plan's headline example: with no language, the codec leads rather than an
    /// "Unknown" token being invented.
    #[test]
    fn unknown_language_leads_with_the_codec() {
        let mut t = audio("AC-3", 6, None, 48000);
        t.language = None;
        assert_eq!(track_summary(&t), "AC-3 · 6 channels · 48 kHz");
    }

    /// The "never invent 5.1" rule at the display end: 6 channels with no *named* layout
    /// reports the count, and never the layout it might have been.
    #[test]
    fn an_unknown_layout_degrades_to_the_channel_count() {
        let t = audio("AC-3", 6, None, 48000);
        assert_eq!(track_summary(&t), "English · AC-3 · 6 channels · 48 kHz");
        assert!(!track_summary(&t).contains("5.1"));
        // A named layout folds onto the codec token instead.
        let named = audio("AC-3", 6, Some("5.1(side)"), 48000);
        assert_eq!(track_summary(&named), "English · AC-3 5.1(side) · 48 kHz");
        // Mono is singular.
        assert_eq!(
            track_summary(&audio("AAC", 1, None, 44100)),
            "English · AAC · 1 channel · 44.1 kHz"
        );
    }

    #[test]
    fn commentary_via_disposition_shows_once() {
        let mut t = audio("AC-3", 6, Some("5.1"), 48000);
        t.flags.commentary = true;
        let s = track_summary(&t);
        assert_eq!(s, "English · AC-3 5.1 · 48 kHz · Commentary");
        assert_eq!(s.matches("Commentary").count(), 1);
    }

    /// The real MKV case: the container marks the track BOTH with a COMMENT disposition
    /// and a "Director's Commentary" title. Saying it twice is the bug this guards.
    #[test]
    fn commentary_via_title_and_disposition_shows_once() {
        let mut t = audio("AC-3", 6, Some("5.1(side)"), 48000);
        t.language = Some("fra".into());
        t.title = Some("Director's Commentary".into());
        t.flags.commentary = true;
        let s = track_summary(&t);
        assert_eq!(s, "French · AC-3 5.1(side) · 48 kHz · Commentary");
        assert_eq!(s.matches("Commentary").count(), 1, "{s}");
    }

    /// ...but a title that is the *only* commentary signal must still surface it.
    #[test]
    fn commentary_via_title_alone_is_still_reported() {
        let mut t = audio("AC-3", 6, Some("5.1"), 48000);
        t.title = Some("Director's Commentary".into());
        assert!(!t.flags.commentary);
        let s = track_summary(&t);
        assert!(s.contains("Commentary"), "{s}");
        assert_eq!(s.matches("Commentary").count(), 1, "{s}");
    }

    /// A title carrying real signal (not a restatement) is kept.
    #[test]
    fn an_informative_title_survives() {
        let mut t = audio("AAC", 2, Some("stereo"), 48000);
        t.title = Some("Isolated Score".into());
        assert_eq!(
            track_summary(&t),
            "English · AAC stereo · 48 kHz · Isolated Score"
        );
    }

    /// A title that merely echoes the language adds nothing.
    #[test]
    fn a_title_that_restates_the_language_is_dropped() {
        let mut t = audio("AAC", 2, Some("stereo"), 48000);
        t.title = Some("English".into());
        assert_eq!(track_summary(&t), "English · AAC stereo · 48 kHz");
    }

    #[test]
    fn sample_rates_format_generally_not_from_a_hardcoded_set() {
        for (hz, want) in [
            (22050, "22.05 kHz"),
            (44100, "44.1 kHz"),
            (48000, "48 kHz"),
            (88200, "88.2 kHz"),
            (176400, "176.4 kHz"),
            (192000, "192 kHz"),
            (8000, "8 kHz"),
            (96000, "96 kHz"),
        ] {
            assert_eq!(format_sample_rate(hz), want, "{hz}");
        }
    }

    /// A listed bitmap/unsupported subtitle must never read as an offer.
    #[test]
    fn unsupported_subtitles_are_marked() {
        let pgs = subtitle("PGS", TrackCapability::Bitmap);
        assert_eq!(track_summary(&pgs), "English · PGS · Unsupported");
        let odd = subtitle("EIA-608", TrackCapability::Unsupported);
        assert!(track_summary(&odd).ends_with("· Unsupported"));
        // Renderable text carries no note.
        let srt = subtitle("SubRip", TrackCapability::SupportedText);
        assert_eq!(track_summary(&srt), "English · SubRip");
        assert!(!track_summary(&srt).contains("Unsupported"));
        let ass = subtitle("ASS", TrackCapability::StyledText);
        assert!(!track_summary(&ass).contains("Unsupported"));

        assert_eq!(
            capability_note(TrackCapability::Bitmap),
            Some("Unsupported")
        );
        assert_eq!(capability_note(TrackCapability::SupportedText), None);
        assert_eq!(capability_note(TrackCapability::Playable), None);
    }

    #[test]
    fn empty_flags_leave_no_trailing_separator() {
        let t = subtitle("SubRip", TrackCapability::SupportedText);
        let s = track_summary(&t);
        assert_eq!(s, "English · SubRip");
        assert!(!s.ends_with(SEP.trim_end()));
        assert!(!s.ends_with(' '));
        assert!(!s.starts_with(' '));
    }

    /// `audio: None` (the AVFoundation backend, which exposes no channel/rate facts)
    /// degrades to the tokens it does have rather than emitting empty ones.
    #[test]
    fn audio_none_degrades_cleanly() {
        let mut t = audio("AAC", 0, None, 0);
        t.audio = None;
        assert_eq!(track_summary(&t), "English · AAC");
        t.flags.default = true;
        assert_eq!(track_summary(&t), "English · AAC · Default");
    }

    /// A track with nothing known at all must not produce a lone separator.
    #[test]
    fn a_track_with_no_facts_produces_no_separators() {
        let mut t = audio("", 0, None, 0);
        t.language = None;
        t.audio = None;
        t.codec = String::new();
        assert_eq!(track_summary(&t), "");
    }

    #[test]
    fn dispositions_keep_a_stable_order() {
        let mut t = audio("AAC", 2, Some("stereo"), 48000);
        t.flags = TrackFlags {
            default: true,
            forced: true,
            commentary: true,
            hearing_impaired: false,
            visual_impaired: false,
        };
        assert_eq!(
            track_summary(&t),
            "English · AAC stereo · 48 kHz · Default · Forced · Commentary"
        );
    }

    /// Audio description annotates the language on an audio track (mirroring SDH on a
    /// subtitle), and says it once.
    #[test]
    fn audio_description_annotates_the_language() {
        let mut t = audio("AAC", 2, Some("stereo"), 48000);
        t.flags.visual_impaired = true;
        let s = track_summary(&t);
        assert_eq!(s, "English (Audio description) · AAC stereo · 48 kHz");
        assert_eq!(s.matches("Audio description").count(), 1);
    }

    /// With no language there is nothing to annotate, so the accessibility fact must
    /// still reach the tail rather than vanish.
    #[test]
    fn accessibility_flags_survive_a_missing_language() {
        let mut t = subtitle("SubRip", TrackCapability::SupportedText);
        t.language = None;
        t.flags.hearing_impaired = true;
        assert_eq!(track_summary(&t), "SubRip · Hearing impaired");

        let mut a = audio("AAC", 2, Some("stereo"), 48000);
        a.language = None;
        a.flags.visual_impaired = true;
        assert_eq!(track_summary(&a), "AAC stereo · 48 kHz · Audio description");
    }

    /// Both macOS backends report the same file's language differently ("eng" vs "en");
    /// the summary must read identically either way.
    #[test]
    fn both_language_tag_forms_render_the_same_summary() {
        let mut ff = audio("AAC", 2, Some("stereo"), 48000);
        ff.language = Some("eng".into());
        let mut av = audio("AAC", 2, Some("stereo"), 48000);
        av.language = Some("en".into());
        assert_eq!(track_summary(&ff), track_summary(&av));
    }

    /// The stream index is a diagnostic, not something a person reads in a panel.
    #[test]
    fn the_raw_stream_index_never_appears_in_the_summary() {
        let mut t = audio("AAC", 2, Some("stereo"), 48000);
        t.id.local_id = 7;
        assert!(!track_summary(&t).contains('7'));
    }

    // -- completeness -> rows (all five states) -----------------------------

    use pb_decode::tracks::{MediaBackend, MediaTrackCatalog, TrackSet};

    fn catalog(audio_set: TrackSet, subs: TrackSet) -> MediaTrackCatalog {
        MediaTrackCatalog::new(1, MediaBackend::FFmpeg, audio_set, subs)
    }

    fn labels(rows: &[DetailRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                DetailRow::Span { text, .. } => format!("[{text}]"),
                DetailRow::Pair { label, value } => format!("{label}: {value}"),
            })
            .collect()
    }

    /// State 1 — `Complete` with zero tracks is the one case allowed to say "No".
    #[test]
    fn complete_and_empty_says_no() {
        let rows = track_rows(
            &catalog(TrackSet::complete(vec![]), TrackSet::complete(vec![])),
            Some(false),
        );
        assert_eq!(labels(&rows), vec!["Audio: No", "Subtitles: No"]);
    }

    /// State 2 — described tracks get a bold heading + one numbered row each.
    #[test]
    fn described_tracks_get_a_heading_and_numbered_rows() {
        let mut a = audio("AAC", 2, Some("stereo"), 48000);
        a.flags.default = true;
        let b = audio("AC-3", 6, Some("5.1"), 48000);
        let s = subtitle("SubRip", TrackCapability::SupportedText);
        let rows = track_rows(
            &catalog(TrackSet::complete(vec![a, b]), TrackSet::complete(vec![s])),
            Some(true),
        );
        assert_eq!(
            labels(&rows),
            vec![
                "[Audio]",
                "Track 1: English · AAC stereo · 48 kHz · Default",
                "Track 2: English · AC-3 5.1 · 48 kHz",
                "[Subtitles]",
                "Track 1: English · SubRip",
            ]
        );
        assert!(matches!(rows[0], DetailRow::Span { bold: true, .. }));
    }

    /// State 3 — `CountOnly`: we know how many, not what. Never "No", never silence.
    #[test]
    fn count_only_lists_the_count_with_an_honest_note() {
        let rows = track_rows(
            &catalog(TrackSet::count_only(2), TrackSet::unavailable()),
            Some(true),
        );
        assert_eq!(
            labels(&rows),
            vec![
                "[Audio]",
                "Track 1: Details unavailable",
                "Track 2: Details unavailable",
            ]
        );
    }

    /// State 4 — `Partial`: list what we have, and say the list isn't the whole story.
    #[test]
    fn partial_lists_known_tracks_and_marks_the_section_partial() {
        let a = audio("AAC", 2, Some("stereo"), 48000);
        let rows = track_rows(
            &catalog(TrackSet::partial(vec![a], Some(3)), TrackSet::unavailable()),
            Some(true),
        );
        assert_eq!(
            labels(&rows),
            vec![
                "[Audio]",
                "Track 1: English · AAC stereo · 48 kHz",
                "[2 more — Details unavailable]",
            ]
        );
        // ...and with no known total, it still refuses to imply completeness.
        let a2 = audio("AAC", 2, Some("stereo"), 48000);
        let rows = track_rows(
            &catalog(TrackSet::partial(vec![a2], None), TrackSet::unavailable()),
            Some(true),
        );
        assert_eq!(
            labels(&rows).last().unwrap(),
            "[More tracks exist — Details unavailable]"
        );
    }

    /// State 5 — the headline rule: an enumeration failure on a file that *does* have
    /// audio must never render as "No audio".
    #[test]
    fn unavailable_with_audio_present_never_says_no() {
        let rows = track_rows(
            &catalog(TrackSet::unavailable(), TrackSet::unavailable()),
            Some(true),
        );
        assert_eq!(labels(&rows), vec!["Audio: Present — details unavailable"]);
        assert!(!labels(&rows).iter().any(|r| r.contains("No")));
    }

    /// `Unavailable` + independently-known *silence* is allowed to say "No" — the fact
    /// comes from the basic probe, not from the empty vector.
    #[test]
    fn unavailable_with_known_silence_says_no() {
        let rows = track_rows(
            &catalog(TrackSet::unavailable(), TrackSet::unavailable()),
            Some(false),
        );
        assert_eq!(labels(&rows), vec!["Audio: No"]);
    }

    /// Subtitles have no independent presence fact, so an un-enumerable subtitle set
    /// says nothing rather than guessing "No".
    #[test]
    fn unavailable_subtitles_say_nothing_at_all() {
        let rows = track_rows(
            &catalog(TrackSet::complete(vec![]), TrackSet::unavailable()),
            Some(false),
        );
        assert_eq!(labels(&rows), vec!["Audio: No"]);
        // A CountOnly zero is likewise not a "No".
        let rows = track_rows(
            &catalog(TrackSet::unavailable(), TrackSet::count_only(0)),
            Some(true),
        );
        assert_eq!(labels(&rows), vec!["Audio: Present — details unavailable"]);
    }

    /// Every row shape stays `Span`/`Pair`, so `DetailsPanel::copy_text()` (#32) keeps
    /// working with no changes on either shell.
    #[test]
    fn rows_round_trip_through_the_details_copy_payload() {
        let a = audio("DTS-HD MA", 6, Some("5.1"), 48000);
        let s = subtitle("PGS", TrackCapability::Bitmap);
        let rows = track_rows(
            &catalog(TrackSet::complete(vec![a]), TrackSet::complete(vec![s])),
            Some(true),
        );
        let panel = crate::panels::DetailsPanel { rows };
        assert_eq!(
            panel.copy_text(),
            "Audio\n\
             Track 1: English · DTS-HD MA 5.1 · 48 kHz\n\
             Subtitles\n\
             Track 1: English · PGS · Unsupported"
        );
    }
}
