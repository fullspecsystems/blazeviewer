//! **Container-neutral timed text** (task #90.2): the cue an *embedded* subtitle stream
//! produces, and the ASS envelope FFmpeg wraps every text decoder's output in.
//!
//! Pure — no FFmpeg, no I/O — so the envelope rules below are unit-testable in any build,
//! including the ones with no FFmpeg linked at all. [`super::ffmpeg::cues`] does the
//! demuxing and produces these; `pb_app_core::cues` normalizes them into a `CueTrack`
//! exactly as it does a parsed sidecar.
//!
//! The split is deliberate: *what the wire format says* is this crate's business, *what
//! the text means* is the app's. So the envelope comes off here and the markup comes off
//! there — the same `strip_markup` a sidecar goes through, rather than a second one.

use std::time::Duration;

/// One cue as the container reported it: a window and the author's text.
///
/// The text is **de-enveloped but not de-marked-up**: the ASS dialogue fields are gone
/// (they are transport, not content) and `\N` has become a real newline, but override
/// tags (`{\an8}`), HTML-ish markup (`<i>`), and entities are all still there. Stripping
/// those is `pb_app_core::cues`' job, which already does it for sidecars — routing
/// embedded text through the same stripper is what keeps an `.srt` and the same content
/// muxed into an MKV rendering identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCue {
    pub start: Duration,
    /// Exclusive. May be `<= start` when the container lied; the normalizer repairs it.
    pub end: Duration,
    pub text: String,
}

/// Strip FFmpeg's ASS dialogue envelope, leaving the author's text.
///
/// **Every** text subtitle decoder in FFmpeg — `subrip`, `webvtt`, `mov_text`, `text`,
/// and the `ass`/`ssa` pass-through — emits its rect as ASS, built by `ff_ass_get_dialog`:
///
/// ```text
/// ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text
/// 0,0,Default,,0,0,0,,Hello there
/// ```
///
/// Eight commas, then the text — and **the text may itself contain commas**, so this
/// counts separators rather than splitting. That single shape is why one function covers
/// every text codec we render: FFmpeg normalized them for us before we ever see them.
///
/// Two robustness rules, both chosen to degrade toward *showing the text*:
/// - A leading `Dialogue:` is tolerated (classic SSA lines carry one; FFmpeg's own rects
///   do not).
/// - Fewer than eight commas means this is not the shape we thought — the whole string is
///   returned rather than a truncated fragment or nothing. A stray comma count must never
///   silently eat a line of dialogue.
pub fn ass_event_text(payload: &str) -> String {
    let body = payload
        .strip_prefix("Dialogue:")
        .map_or(payload, str::trim_start);
    let text = match body.match_indices(',').nth(7) {
        Some((i, _)) => &body[i + 1..],
        // Not the expected envelope. Show what we have.
        None => body,
    };
    unescape_ass_breaks(text)
}

/// `\N` (hard break) and `\n` (soft break) → real newlines; `\h` → a non-breaking space.
///
/// These are ASS *transport* escapes, not the author's characters, so they come off with
/// the envelope. A backslash before anything else is left alone — it is far more likely to
/// be literal text than a tag we failed to recognize.
fn unescape_ass_breaks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // Both breaks become a newline: the distinction is "wrap here if needed" vs
            // "always wrap", and we are re-wrapping to the viewport anyway.
            Some('N') | Some('n') => out.push('\n'),
            Some('h') => out.push('\u{a0}'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_ffmpeg_envelope_comes_off() {
        assert_eq!(
            ass_event_text("0,0,Default,,0,0,0,,Hello there"),
            "Hello there"
        );
    }

    /// The reason this counts commas instead of splitting on them. Dialogue is full of
    /// commas; a `split(',').nth(8)` would hand back "wait" and drop the rest of the line.
    #[test]
    fn commas_in_the_dialogue_survive() {
        assert_eq!(
            ass_event_text("12,0,Default,Bob,0,0,0,,Well, no, wait, that's wrong"),
            "Well, no, wait, that's wrong"
        );
    }

    #[test]
    fn a_classic_ssa_dialogue_prefix_is_tolerated() {
        assert_eq!(
            ass_event_text("Dialogue: 0,0,Default,,0,0,0,,Marked line"),
            "Marked line"
        );
    }

    /// Degrade toward showing the text: an unexpected shape returns everything rather
    /// than a fragment. Losing a line of dialogue is worse than showing a little noise.
    #[test]
    fn an_unexpected_shape_returns_the_whole_string() {
        assert_eq!(ass_event_text("just some text"), "just some text");
        assert_eq!(ass_event_text("0,0,Default,,short"), "0,0,Default,,short");
    }

    #[test]
    fn ass_breaks_become_newlines() {
        assert_eq!(
            ass_event_text(r"0,0,Default,,0,0,0,,First line\NSecond line"),
            "First line\nSecond line"
        );
        assert_eq!(
            ass_event_text(r"0,0,Default,,0,0,0,,Soft\nbreak"),
            "Soft\nbreak"
        );
        assert_eq!(ass_event_text(r"0,0,Default,,0,0,0,,a\hb"), "a\u{a0}b");
    }

    /// A backslash is more often literal text than a tag we don't know. Windows paths and
    /// emoticons are both real things people put in subtitles.
    #[test]
    fn an_unknown_backslash_escape_is_left_alone() {
        assert_eq!(
            ass_event_text(r"0,0,Default,,0,0,0,,C:\Users\jd"),
            r"C:\Users\jd"
        );
        assert_eq!(
            ass_event_text(r"0,0,Default,,0,0,0,,trailing\"),
            r"trailing\"
        );
    }

    /// Override tags are NOT stripped here — that is the app's shared `strip_markup`,
    /// the same one a sidecar goes through. Pinned so nobody "helpfully" adds a second
    /// stripper here and lets the two drift.
    #[test]
    fn override_tags_are_left_for_the_shared_stripper() {
        assert_eq!(
            ass_event_text(r"0,0,Default,,0,0,0,,{\an8}A sign"),
            r"{\an8}A sign"
        );
    }

    #[test]
    fn an_empty_text_field_stays_empty() {
        assert_eq!(ass_event_text("0,0,Default,,0,0,0,,"), "");
    }
}
