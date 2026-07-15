//! **Subtitle cues** (task #90.2): subtitle text → timed plain text.
//!
//! Pure — text in, cues out. No I/O, no clock, no renderer. The timing *engine* (seek
//! generations, the no-stale-flash rule, scheduling) is #90.3 and lives on top of this;
//! what this owns is the model, the parsing, and the normalization that makes a
//! hand-authored file safe to schedule against.
//!
//! **Both tiers land here.** Sidecar text (SubRip, WebVTT) is parsed *in this module*;
//! embedded streams (MKV `subrip`/`ass`/`webvtt`, MP4 `mov_text`) are demuxed by
//! `pb_decode::ffmpeg::cues` and arrive as [`pb_decode::text_cue::TextCue`] via
//! [`CueTrack::from_text_cues`]. They converge on the same [`strip_markup`] and the same
//! [`CueTrack::from_cues`] on purpose: the corpus has an MKV whose embedded English track
//! and `.eng.srt` are the same content, and any divergence would render as two different
//! versions of one line.
//!
//! **Subtitle files are hand-authored and hostile by accident.** Overlaps, out-of-order
//! blocks, ends before starts, missing indices, `.` where `,` belongs, stray markup,
//! text that was UTF-8 and got decoded as CP1252 somewhere upstream ([`crate::mojibake`])
//! — all of it is normal, none of it is an error worth refusing a file over. Every rule
//! below degrades toward *showing the text* rather than toward correctness theatre.

use std::time::Duration;

/// One subtitle cue: what to show, and the half-open window `[start, end)` to show it in.
///
/// Half-open deliberately: it makes "which cue is active at `t`" total and
/// non-overlapping at the boundary, so a cue ending at exactly the moment the next begins
/// cannot flash both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleCue {
    pub start: Duration,
    /// Exclusive. Always `> start` after normalization.
    pub end: Duration,
    /// The cue's lines, markup stripped, in order. Never empty after normalization.
    pub lines: Vec<String>,
    /// Whether this cue is part of a *forced* track. Neither SubRip nor WebVTT can say
    /// this per-cue — it comes from the track's flags — so the parser leaves it `false`
    /// and the loader stamps it.
    pub forced: bool,
    /// Position in the file, before sorting. The tie-break that keeps two cues starting on
    /// the same frame in the order the author wrote them.
    pub source_order: usize,
}

/// How long to show a cue whose end is missing or nonsensical, when nothing better is
/// known. Capped by the next cue's start.
const FALLBACK_CUE: Duration = Duration::from_secs(5);

/// The floor a degenerate cue is widened to, so `[start, end)` is never empty (which
/// would make it unshowable).
const MIN_CUE: Duration = Duration::from_millis(200);

/// A parsed, normalized, queryable subtitle track.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CueTrack {
    /// Sorted by `(start, source_order)`. Overlaps are preserved — see [`Self::active_at`].
    cues: Vec<SubtitleCue>,
}

impl CueTrack {
    /// Parse subtitle text in the format named by `codec_raw` (the shared vocabulary:
    /// `"subrip"`, `"webvtt"`). An unknown format yields an empty track rather than an
    /// error — there is nothing a caller could usefully do with the error that "no
    /// subtitles appeared" doesn't already say.
    pub fn parse(text: &str, codec_raw: &str) -> CueTrack {
        CueTrack::from_cues(parse_cues(text, codec_raw))
    }

    /// Append more cues and re-normalize.
    ///
    /// This is what lets an *embedded* stream stream (#90.2): the reader hands over cues
    /// in presentation order as it walks the container, far ahead of the playhead, rather
    /// than making the first cue wait for the last (39 s on the corpus MKV — see
    /// `pb_decode::ffmpeg::cues`).
    ///
    /// Re-normalizing the whole set on every batch is deliberate and cheap (a sort of a
    /// few thousand, a few dozen times). The alternative — normalizing each batch in
    /// isolation — gets the *repairs* wrong, because "how long does this cue last" is
    /// answered by the **next** cue, which may be in the next batch.
    ///
    /// One honest seam remains: a cue with a broken end that lands last in a batch is
    /// repaired against `FALLBACK_CUE` rather than its true successor, and re-normalizing
    /// cannot undo that (a repaired end is indistinguishable from an authored one). It
    /// needs a container that both omits the duration *and* splits that cue onto a batch
    /// boundary; the cost is one cue lingering, and only sidecars — which never stream —
    /// would notice.
    pub fn extend(&mut self, more: Vec<SubtitleCue>) {
        if more.is_empty() {
            return;
        }
        let mut all = std::mem::take(&mut self.cues);
        all.extend(more);
        *self = CueTrack::from_cues(all);
    }

    /// The next `source_order` to hand out, so a streamed batch continues the sequence
    /// instead of restarting it and re-tying every tie at zero.
    pub fn next_source_order(&self) -> usize {
        self.cues
            .iter()
            .map(|c| c.source_order + 1)
            .max()
            .unwrap_or(0)
    }

    /// Every embedded stream's cues (#90.2), normalized exactly like a parsed sidecar's.
    ///
    /// [`pb_decode::text_cue::TextCue`] arrives de-enveloped but not de-marked-up — see
    /// that type's docs for why the split falls there. This is where the two tiers
    /// converge: an `.srt` and the same content muxed into an MKV go through the same
    /// [`strip_markup`] and the same [`Self::from_cues`], so they cannot render
    /// differently. (The corpus has exactly that pair, which is how the double-shown
    /// duplicate was caught in the first place.)
    pub fn from_text_cues(cues: Vec<pb_decode::text_cue::TextCue>) -> CueTrack {
        CueTrack::from_cues(text_cues_to_cues(cues, 0))
    }

    /// Normalize an arbitrary cue list into a track: drop the empty, fix the impossible,
    /// repair the mis-encoded, sort stably. The one entry point, so a hand-built cue list
    /// (tests, a demuxer, a parsed file) gets the same guarantees.
    pub fn from_cues(mut cues: Vec<SubtitleCue>) -> CueTrack {
        // Undo double-encoded UTF-8 (`â™ª` → `♪`) — here, at the single point BOTH tiers
        // pass through, so a sidecar and an embedded stream can never disagree about the
        // same text. Provable and per-run; see `crate::mojibake`. A no-op (and a borrow)
        // on the clean text that is the overwhelming majority.
        for c in &mut cues {
            for l in &mut c.lines {
                if let std::borrow::Cow::Owned(fixed) = crate::mojibake::repair(l) {
                    *l = fixed;
                }
            }
        }
        // A cue with no text after stripping is invisible; keeping it would only make
        // `active_at` return nothing to draw.
        cues.retain(|c| !c.lines.is_empty());
        // Out-of-order blocks are common in hand-edited files. Sort by start, breaking
        // ties by the order the author wrote them.
        cues.sort_by_key(|c| (c.start, c.source_order));

        // Fix ends that the file got wrong, now that neighbours are known.
        for i in 0..cues.len() {
            if cues[i].end > cues[i].start {
                continue;
            }
            // `end <= start`: missing, zero, or reversed. Show it until the next cue
            // starts, bounded — rather than dropping the author's text on a typo.
            let next_start = cues.get(i + 1).map(|n| n.start);
            let want = cues[i].start + FALLBACK_CUE;
            let end = match next_start {
                Some(ns) if ns > cues[i].start => want.min(ns),
                _ => want,
            };
            cues[i].end = end.max(cues[i].start + MIN_CUE);
        }
        CueTrack { cues }
    }

    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }

    pub fn len(&self) -> usize {
        self.cues.len()
    }

    pub fn cues(&self) -> &[SubtitleCue] {
        &self.cues
    }

    /// Stamp every cue's `forced` — the track knows, the file doesn't.
    pub fn set_forced(&mut self, forced: bool) {
        for c in &mut self.cues {
            c.forced = forced;
        }
    }

    /// Shift every cue by `offset`, for a container whose subtitle stream doesn't start at
    /// zero (the plan's container start-offset). Saturating: a cue can't be dragged before
    /// zero, and the session clock is session-relative anyway.
    pub fn shift(&mut self, offset: Duration) {
        for c in &mut self.cues {
            c.start = c.start.saturating_add(offset);
            c.end = c.end.saturating_add(offset);
        }
    }

    /// The cues showing at `t`, in source order.
    ///
    /// **Overlaps are kept, not resolved.** Two cues live at once is a real thing authors
    /// do (a sign and a line of dialogue), and picking one would silently drop the other;
    /// the renderer stacks them. `[start, end)` means a cue ending exactly when the next
    /// begins never double-shows.
    pub fn active_at(&self, t: Duration) -> impl Iterator<Item = &SubtitleCue> {
        // Linear, but bounded by the overlap depth in practice and only re-run on a
        // boundary (see `next_boundary_after`) — never per frame.
        self.cues.iter().filter(move |c| c.start <= t && t < c.end)
    }

    /// The next time the visible set could change after `t`, or `None` when nothing else
    /// happens.
    ///
    /// This is what makes #90.3 able to *schedule* instead of scanning every frame: wake
    /// at the boundary, rebuild, sleep. Both a start and an end are boundaries.
    pub fn next_boundary_after(&self, t: Duration) -> Option<Duration> {
        self.cues
            .iter()
            .flat_map(|c| [c.start, c.end])
            .filter(|&b| b > t)
            .min()
    }
}

/// The raw parse, before normalization — what a streaming loader hands to
/// [`CueTrack::extend`], which normalizes the accumulated whole.
///
/// An unknown format yields nothing rather than an error: there is nothing a caller could
/// usefully do with the error that "no subtitles appeared" doesn't already say.
pub fn parse_cues(text: &str, codec_raw: &str) -> Vec<SubtitleCue> {
    match codec_raw {
        "subrip" | "srt" => parse_blocks(text, false),
        "webvtt" => parse_blocks(text, true),
        _ => Vec::new(),
    }
}

/// An embedded stream's [`pb_decode::text_cue::TextCue`]s → cues, continuing the
/// `source_order` sequence at `order_base` so a streamed batch doesn't restart it.
///
/// This is where the two tiers converge: the text goes through the very same
/// [`strip_markup`] a sidecar's does, so an `.srt` and the same content muxed into an MKV
/// cannot render differently. (The corpus has exactly that pair.)
pub fn text_cues_to_cues(
    cues: Vec<pb_decode::text_cue::TextCue>,
    order_base: usize,
) -> Vec<SubtitleCue> {
    cues.into_iter()
        .enumerate()
        .map(|(i, c)| SubtitleCue {
            start: c.start,
            end: c.end,
            // A container cue carries its own hard breaks; the renderer re-wraps to the
            // viewport but must honour an authored break.
            lines: c
                .text
                .lines()
                .map(strip_markup)
                .filter(|l| !l.is_empty())
                .collect(),
            // Per-cue `forced` is not a thing a container says — the *track* says it, and
            // the loader stamps it. Same rule as the sidecar path.
            forced: false,
            source_order: order_base + i,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Both formats are "blocks separated by blank lines, one of which holds `-->`", so one
/// parser serves them; `vtt` only switches on the extras WebVTT adds.
fn parse_blocks(text: &str, vtt: bool) -> Vec<SubtitleCue> {
    let mut out = Vec::new();
    // Normalize newlines once; \r\n is the norm in the wild and a stray \r would otherwise
    // ride into the rendered text.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    for raw in text.split("\n\n") {
        let block = raw.trim_matches('\n');
        if block.is_empty() {
            continue;
        }
        // WebVTT's non-cue blocks. NOTE/STYLE/REGION and the header carry no cues, and the
        // header line can also be `WEBVTT - some title`.
        if vtt {
            let first = block.lines().next().unwrap_or("");
            if first.starts_with("WEBVTT")
                || first.starts_with("NOTE")
                || first.starts_with("STYLE")
                || first.starts_with("REGION")
            {
                continue;
            }
        }
        // Find the timing line. It is not always the first: SubRip usually puts an index
        // above it, and WebVTT allows a cue identifier there.
        let lines: Vec<&str> = block.lines().collect();
        let Some(ti) = lines.iter().position(|l| l.contains("-->")) else {
            continue; // no timing = not a cue (a stray index, a comment, trailing junk)
        };
        let Some((start, end)) = parse_timing(lines[ti]) else {
            continue;
        };
        let body: Vec<String> = lines[ti + 1..]
            .iter()
            .map(|l| strip_markup(l))
            .filter(|l| !l.is_empty())
            .collect();
        out.push(SubtitleCue {
            start,
            end,
            lines: body,
            forced: false,
            source_order: out.len(),
        });
    }
    out
}

/// `00:00:01,000 --> 00:00:02,000` (SubRip) or `00:01.000 --> 00:02.000 align:start`
/// (WebVTT). Trailing cue settings are ignored; an `end` we can't read becomes zero and
/// normalization repairs it from the neighbours.
fn parse_timing(line: &str) -> Option<(Duration, Duration)> {
    let (l, r) = line.split_once("-->")?;
    let start = parse_timestamp(l.trim())?;
    // The right side may carry WebVTT settings (`align:middle line:90%`) or SubRip's old
    // coordinates (`X1:0 X2:0 ...`) — the timestamp is the first token either way.
    let end_tok = r.split_whitespace().next().unwrap_or("");
    let end = parse_timestamp(end_tok).unwrap_or(Duration::ZERO);
    Some((start, end))
}

/// `HH:MM:SS,mmm` / `HH:MM:SS.mmm` / `MM:SS.mmm` — hours are optional in WebVTT, and the
/// comma-vs-period split is not reliable in the wild (SubRip files with periods are
/// everywhere), so both separators are accepted for both formats.
fn parse_timestamp(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (hms, frac) = match s.rsplit_once([',', '.']) {
        Some((a, b)) if b.chars().all(|c| c.is_ascii_digit()) && !b.is_empty() => (a, b),
        // No fractional part at all is still a valid time.
        _ => (s, ""),
    };
    let mut secs: u64 = 0;
    let mut parts = hms.split(':').collect::<Vec<_>>();
    if parts.len() > 3 || parts.is_empty() {
        return None;
    }
    // Right-align: [SS] / [MM, SS] / [HH, MM, SS].
    while parts.len() < 3 {
        parts.insert(0, "0");
    }
    for p in &parts {
        let v: u64 = p.trim().parse().ok()?;
        secs = secs.checked_mul(60)?.checked_add(v)?;
    }
    // Milliseconds, tolerating 1-3 (or more) fractional digits: `.5` is 500 ms.
    let millis = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        let scale = 10u64.pow(3 - digits.len() as u32);
        digits.parse::<u64>().ok()? * scale
    };
    Some(Duration::from_millis(
        secs.checked_mul(1000)?.checked_add(millis)?,
    ))
}

/// Strip the markup both formats carry, leaving plain text (#90.2's job — "timed plain
/// text"). Unknown tags are dropped rather than shown: a stray `<font color="#fff">` in
/// the rendered line is worse than losing the styling we weren't going to honour anyway.
fn strip_markup(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // `<i>`, `</i>`, `<font …>`, WebVTT's `<v Speaker>` and `<00:01.000>`.
            '<' => {
                for c in chars.by_ref() {
                    if c == '>' {
                        break;
                    }
                }
            }
            // ASS/SSA override blocks that leak into .srt: `{\an8}`, `{\i1}`.
            '{' if chars.peek() == Some(&'\\') => {
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                }
            }
            '&' => {
                // An entity, or a literal ampersand. Bounded lookahead so `A & B` stays.
                let rest: String = chars.clone().take(8).collect();
                match entity(&rest) {
                    Some((decoded, len)) => {
                        out.push(decoded);
                        for _ in 0..len {
                            chars.next();
                        }
                    }
                    None => out.push('&'),
                }
            }
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Decode the entity beginning just after an `&`. Returns `(char, chars_consumed)`.
fn entity(rest: &str) -> Option<(char, usize)> {
    for (name, ch) in [
        ("amp;", '&'),
        ("lt;", '<'),
        ("gt;", '>'),
        ("quot;", '"'),
        ("apos;", '\''),
        ("nbsp;", ' '),
        ("#39;", '\''),
        ("#160;", ' '),
    ] {
        if rest.starts_with(name) {
            return Some((ch, name.chars().count()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // -- timestamps ---------------------------------------------------------

    #[test]
    fn timestamps_accept_both_separators_and_optional_hours() {
        assert_eq!(parse_timestamp("00:00:01,500"), Some(ms(1500))); // SubRip
        assert_eq!(parse_timestamp("00:00:01.500"), Some(ms(1500))); // WebVTT
                                                                     // A SubRip file written with periods is extremely common — accept it.
        assert_eq!(parse_timestamp("01:02:03,004"), Some(ms(3_723_004)));
        // WebVTT allows the hours off.
        assert_eq!(parse_timestamp("02:03.004"), Some(ms(123_004)));
        assert_eq!(parse_timestamp("00:00:00,000"), Some(Duration::ZERO));
        // Odd but legal fractional widths.
        assert_eq!(parse_timestamp("00:00:01.5"), Some(ms(1500)));
        assert_eq!(parse_timestamp("00:00:01.05"), Some(ms(1050)));
        assert_eq!(parse_timestamp("00:00:01"), Some(ms(1000)));
    }

    #[test]
    fn nonsense_timestamps_are_rejected_not_guessed() {
        for s in ["", "  ", "abc", "1:2:3:4", "aa:bb:cc,ddd"] {
            assert_eq!(parse_timestamp(s), None, "{s:?}");
        }
    }

    // -- SubRip -------------------------------------------------------------

    const SRT: &str = "1\n\
        00:00:01,000 --> 00:00:02,000\n\
        Hello there\n\
        \n\
        2\n\
        00:00:03,000 --> 00:00:04,500\n\
        Two lines\n\
        of dialogue\n";

    #[test]
    fn parses_subrip() {
        let t = CueTrack::parse(SRT, "subrip");
        assert_eq!(t.len(), 2);
        assert_eq!(t.cues()[0].start, ms(1000));
        assert_eq!(t.cues()[0].end, ms(2000));
        assert_eq!(t.cues()[0].lines, vec!["Hello there"]);
        assert_eq!(t.cues()[1].lines, vec!["Two lines", "of dialogue"]);
        assert_eq!(t.cues()[1].source_order, 1);
    }

    /// CRLF is the norm in the wild; a stray `\r` would otherwise ride into the text.
    #[test]
    fn crlf_files_parse_and_leave_no_carriage_returns() {
        let t = CueTrack::parse(&SRT.replace('\n', "\r\n"), "subrip");
        assert_eq!(t.len(), 2);
        assert_eq!(t.cues()[0].lines, vec!["Hello there"]);
        assert!(!t
            .cues()
            .iter()
            .any(|c| c.lines.iter().any(|l| l.contains('\r'))));
    }

    /// The index line is conventional, not required — plenty of files omit it.
    #[test]
    fn subrip_without_index_lines_still_parses() {
        let t = CueTrack::parse("00:00:01,000 --> 00:00:02,000\nNo index\n", "subrip");
        assert_eq!(t.len(), 1);
        assert_eq!(t.cues()[0].lines, vec!["No index"]);
    }

    /// Old SubRip carries display coordinates after the end time.
    #[test]
    fn subrip_trailing_coordinates_are_ignored() {
        let t = CueTrack::parse(
            "1\n00:00:01,000 --> 00:00:02,000  X1:100 X2:200 Y1:1 Y2:2\nText\n",
            "subrip",
        );
        assert_eq!(t.len(), 1);
        assert_eq!(t.cues()[0].end, ms(2000));
    }

    // -- WebVTT -------------------------------------------------------------

    const VTT: &str = "WEBVTT - My subtitles\n\
        \n\
        NOTE this is a comment\n\
        that spans lines\n\
        \n\
        intro\n\
        00:00:01.000 --> 00:00:02.000 align:start line:90%\n\
        Hello there\n\
        \n\
        00:03.000 --> 00:04.000\n\
        <v Narrator>Short form time\n";

    #[test]
    fn parses_webvtt_skipping_header_notes_ids_and_settings() {
        let t = CueTrack::parse(VTT, "webvtt");
        assert_eq!(t.len(), 2, "the header and NOTE are not cues");
        assert_eq!(t.cues()[0].start, ms(1000));
        assert_eq!(
            t.cues()[0].end,
            ms(2000),
            "cue settings are not part of the time"
        );
        assert_eq!(
            t.cues()[0].lines,
            vec!["Hello there"],
            "the cue id is not text"
        );
        assert_eq!(t.cues()[1].start, ms(3000));
        assert_eq!(
            t.cues()[1].lines,
            vec!["Short form time"],
            "the voice tag is stripped"
        );
    }

    #[test]
    fn an_unknown_format_is_empty_not_an_error() {
        assert!(CueTrack::parse(SRT, "hdmv_pgs_subtitle").is_empty());
        assert!(CueTrack::parse("", "subrip").is_empty());
        assert!(CueTrack::parse("total garbage\nwith no timings", "subrip").is_empty());
    }

    // -- markup -------------------------------------------------------------

    #[test]
    fn markup_is_stripped_to_plain_text() {
        let cases = [
            ("<i>Italic</i>", "Italic"),
            ("<b>Bold</b> and <u>under</u>", "Bold and under"),
            ("<font color=\"#ffffff\">Colored</font>", "Colored"),
            ("{\\an8}Top positioned", "Top positioned"),
            ("{\\i1}Italic ASS-style{\\i0}", "Italic ASS-style"),
            ("<v Speaker>Voice", "Voice"),
            ("<00:00:01.000>Timestamp tag", "Timestamp tag"),
        ];
        for (input, want) in cases {
            let t = CueTrack::parse(
                &format!("00:00:01,000 --> 00:00:02,000\n{input}\n"),
                "subrip",
            );
            assert_eq!(t.cues()[0].lines, vec![want.to_string()], "{input:?}");
        }
    }

    #[test]
    fn entities_decode_and_a_bare_ampersand_survives() {
        let cases = [
            ("R&amp;D", "R&D"),
            ("&lt;tag&gt;", "<tag>"),
            ("&quot;quoted&quot;", "\"quoted\""),
            ("Tom&apos;s", "Tom's"),
            ("Tom&#39;s", "Tom's"),
            // Not an entity — must not eat the text after it.
            ("A & B", "A & B"),
            ("Fish&Chips", "Fish&Chips"),
        ];
        for (input, want) in cases {
            let t = CueTrack::parse(
                &format!("00:00:01,000 --> 00:00:02,000\n{input}\n"),
                "subrip",
            );
            assert_eq!(t.cues()[0].lines, vec![want.to_string()], "{input:?}");
        }
    }

    /// A cue that is *only* markup has nothing to show; keeping it would make `active_at`
    /// return a cue with no text.
    #[test]
    fn a_cue_with_no_text_after_stripping_is_dropped() {
        let t = CueTrack::parse("00:00:01,000 --> 00:00:02,000\n<i></i>\n", "subrip");
        assert!(t.is_empty());
    }

    // -- normalization ------------------------------------------------------

    fn cue(start: u64, end: u64, order: usize) -> SubtitleCue {
        SubtitleCue {
            start: ms(start),
            end: ms(end),
            lines: vec![format!("cue {order}")],
            forced: false,
            source_order: order,
        }
    }

    #[test]
    fn out_of_order_cues_are_sorted_stably() {
        let t = CueTrack::from_cues(vec![cue(3000, 4000, 0), cue(1000, 2000, 1)]);
        assert_eq!(t.cues()[0].start, ms(1000));
        assert_eq!(t.cues()[1].start, ms(3000));
        // Same start → the author's order wins.
        let t = CueTrack::from_cues(vec![cue(1000, 2000, 5), cue(1000, 2000, 2)]);
        assert_eq!(t.cues()[0].source_order, 2);
        assert_eq!(t.cues()[1].source_order, 5);
    }

    /// A typo'd end must not drop the author's text — show it until the next cue.
    #[test]
    fn a_missing_or_reversed_end_is_repaired_from_the_neighbours() {
        // end == start.
        let t = CueTrack::from_cues(vec![cue(1000, 1000, 0), cue(2000, 3000, 1)]);
        assert_eq!(t.cues()[0].end, ms(2000), "clamped to the next cue's start");

        // end < start (reversed).
        let t = CueTrack::from_cues(vec![cue(5000, 1000, 0), cue(9000, 9500, 1)]);
        assert_eq!(t.cues()[0].end, ms(9000).min(ms(5000) + FALLBACK_CUE));

        // No next cue → the bounded fallback.
        let t = CueTrack::from_cues(vec![cue(1000, 0, 0)]);
        assert_eq!(t.cues()[0].end, ms(1000) + FALLBACK_CUE);

        // A next cue starting a hair later would clamp to an unreadable window, so the
        // floor widens it — accepting an overlap, which is legal, over a 50 ms flash.
        let t = CueTrack::from_cues(vec![cue(1000, 1000, 0), cue(1050, 2000, 1)]);
        assert_eq!(t.cues()[0].end, ms(1000) + MIN_CUE);
        assert!(t.cues()[0].end > t.cues()[0].start, "never an empty window");

        // A next cue at the *same* start is no room at all, so there is nothing to clamp
        // to: fall back rather than invent a 0-length cue.
        let t = CueTrack::from_cues(vec![cue(1000, 1000, 0), cue(1000, 2000, 1)]);
        assert_eq!(t.cues()[0].end, ms(1000) + FALLBACK_CUE);
        assert!(t.cues()[0].end > t.cues()[0].start);
    }

    #[test]
    fn a_missing_end_in_a_real_file_is_repaired() {
        let t = CueTrack::parse(
            "1\n00:00:01,000 --> \nOrphan\n\n2\n00:00:02,000 --> 00:00:03,000\nNext\n",
            "subrip",
        );
        assert_eq!(t.len(), 2);
        assert_eq!(t.cues()[0].end, ms(2000), "runs until the next cue");
    }

    // -- querying -----------------------------------------------------------

    #[test]
    fn active_at_uses_a_half_open_window() {
        let t = CueTrack::from_cues(vec![cue(1000, 2000, 0), cue(2000, 3000, 1)]);
        let at = |n| {
            t.active_at(ms(n))
                .map(|c| c.source_order)
                .collect::<Vec<_>>()
        };
        assert_eq!(at(999), Vec::<usize>::new());
        assert_eq!(at(1000), vec![0], "start is inclusive");
        assert_eq!(at(1999), vec![0]);
        // The boundary: the first cue's end IS the second's start — exactly one shows.
        assert_eq!(at(2000), vec![1], "end is exclusive, so no double-show");
        assert_eq!(at(3000), Vec::<usize>::new());
    }

    /// Overlaps are kept, not resolved: authors really do run a sign and a line together,
    /// and picking one would silently drop the other.
    #[test]
    fn overlapping_cues_are_both_active_in_source_order() {
        let t = CueTrack::from_cues(vec![cue(1000, 5000, 0), cue(2000, 3000, 1)]);
        let at = |n| {
            t.active_at(ms(n))
                .map(|c| c.source_order)
                .collect::<Vec<_>>()
        };
        assert_eq!(at(1500), vec![0]);
        assert_eq!(at(2500), vec![0, 1], "both, in source order");
        assert_eq!(at(4000), vec![0]);
    }

    /// The scheduling primitive #90.3 needs: wake on the boundary, never scan per frame.
    #[test]
    fn next_boundary_after_finds_every_edge() {
        let t = CueTrack::from_cues(vec![cue(1000, 2000, 0), cue(2500, 3000, 1)]);
        let nb = |n| t.next_boundary_after(ms(n));
        assert_eq!(nb(0), Some(ms(1000)), "the first start");
        assert_eq!(nb(1000), Some(ms(2000)), "its end — a boundary too");
        assert_eq!(nb(2000), Some(ms(2500)));
        assert_eq!(nb(2999), Some(ms(3000)));
        assert_eq!(nb(3000), None, "nothing left to do");
        assert_eq!(CueTrack::default().next_boundary_after(ms(0)), None);
    }

    #[test]
    fn overlapping_boundaries_are_still_ordered() {
        let t = CueTrack::from_cues(vec![cue(1000, 5000, 0), cue(2000, 3000, 1)]);
        assert_eq!(t.next_boundary_after(ms(1500)), Some(ms(2000)));
        assert_eq!(t.next_boundary_after(ms(2000)), Some(ms(3000)));
        assert_eq!(t.next_boundary_after(ms(3000)), Some(ms(5000)));
    }

    #[test]
    fn shift_moves_the_whole_track_and_cannot_go_negative() {
        let mut t = CueTrack::from_cues(vec![cue(1000, 2000, 0)]);
        t.shift(ms(500));
        assert_eq!(t.cues()[0].start, ms(1500));
        assert_eq!(t.cues()[0].end, ms(2500));
        // Saturating at zero (a container offset can't drag a cue before the session start).
        let mut t = CueTrack::from_cues(vec![cue(0, 1000, 0)]);
        t.shift(Duration::ZERO);
        assert_eq!(t.cues()[0].start, Duration::ZERO);
    }

    #[test]
    fn set_forced_stamps_the_track_since_the_file_cannot() {
        let mut t = CueTrack::parse(SRT, "subrip");
        assert!(
            t.cues().iter().all(|c| !c.forced),
            "the parser never invents this"
        );
        t.set_forced(true);
        assert!(t.cues().iter().all(|c| c.forced));
    }

    /// A real file, end to end, through the #90.1 decode path.
    #[test]
    fn a_utf16_bom_file_round_trips_from_bytes_to_cues() {
        let text = "1\n00:00:01,000 --> 00:00:02,000\nこんにちは\n";
        let mut bytes = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let decoded = crate::sidecar::decode_sidecar_text(&bytes);
        let t = CueTrack::parse(&decoded, "subrip");
        assert_eq!(t.len(), 1);
        assert_eq!(t.cues()[0].lines, vec!["こんにちは"]);
    }

    // -- the embedded tier (#90.2) -----------------------------------------

    use pb_decode::text_cue::TextCue;

    fn tc(start_ms: u64, end_ms: u64, text: &str) -> TextCue {
        TextCue {
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(end_ms),
            text: text.into(),
        }
    }

    /// An embedded stream's cues become a track exactly like a parsed sidecar's.
    #[test]
    fn embedded_text_cues_become_a_track() {
        let t = CueTrack::from_text_cues(vec![
            tc(1000, 2000, "Hello there"),
            tc(2000, 3000, "Line one\nLine two"),
        ]);
        assert_eq!(t.len(), 2);
        assert_eq!(t.cues()[0].lines, vec!["Hello there"]);
        assert_eq!(t.cues()[1].lines, vec!["Line one", "Line two"]);
    }

    /// The two tiers share ONE stripper. An embedded cue's markup must come off exactly
    /// as a sidecar's does — the corpus has the same content in both, so any divergence
    /// shows up as two differently-rendered versions of the same line.
    #[test]
    fn embedded_cues_go_through_the_same_stripper_as_a_sidecar() {
        let embedded = CueTrack::from_text_cues(vec![tc(1000, 2000, "{\\an8}<i>A sign</i>")]);
        let sidecar = CueTrack::parse(
            "1\n00:00:01,000 --> 00:00:02,000\n{\\an8}<i>A sign</i>\n",
            "subrip",
        );
        assert_eq!(embedded.cues()[0].lines, vec!["A sign"]);
        assert_eq!(embedded.cues()[0].lines, sidecar.cues()[0].lines);
    }

    /// A container that reports no duration leaves end == start; the normalizer repairs
    /// it from the next cue rather than dropping the author's text.
    #[test]
    fn an_embedded_cue_with_no_duration_is_repaired_from_its_neighbour() {
        let t =
            CueTrack::from_text_cues(vec![tc(1000, 1000, "no duration"), tc(3000, 4000, "next")]);
        assert_eq!(
            t.cues()[0].end,
            Duration::from_secs(3),
            "bounded by the next start"
        );
    }

    // -- mojibake, at the choke point --------------------------------------

    /// The corpus defect, through the real path: the Grey's Anatomy MKV's embedded subrip
    /// track carries the music note double-encoded. Repair happens in `from_cues`, so
    /// BOTH tiers get it and neither can drift.
    #[test]
    fn double_encoded_text_is_repaired_for_both_tiers() {
        let embedded = CueTrack::from_text_cues(vec![tc(
            1000,
            2000,
            "\u{e2}\u{2122}\u{aa} WAKE UP \u{e2}\u{2122}\u{aa}",
        )]);
        assert_eq!(embedded.cues()[0].lines, vec!["\u{266a} WAKE UP \u{266a}"]);

        let sidecar = CueTrack::parse(
            "1\n00:00:01,000 --> 00:00:02,000\n\u{e2}\u{2122}\u{aa} WAKE UP \u{e2}\u{2122}\u{aa}\n",
            "subrip",
        );
        assert_eq!(sidecar.cues()[0].lines, embedded.cues()[0].lines);
    }

    /// ...and correct text is never "repaired". The guard that matters most.
    #[test]
    fn correctly_encoded_text_is_untouched_by_the_repair() {
        let t = CueTrack::from_text_cues(vec![
            tc(1000, 2000, "caf\u{e9} na\u{ef}ve se\u{f1}or"),
            tc(2000, 3000, "\u{266a} already correct \u{266a}"),
        ]);
        assert_eq!(t.cues()[0].lines, vec!["caf\u{e9} na\u{ef}ve se\u{f1}or"]);
        assert_eq!(t.cues()[1].lines, vec!["\u{266a} already correct \u{266a}"]);
    }

    // -- streaming (#90.2) --------------------------------------------------

    /// `extend` is what lets an embedded stream stream. Batches accumulate, and the whole
    /// set re-sorts — the reader walks the container in presentation order, but the
    /// normalizer must not depend on that.
    #[test]
    fn extend_accumulates_batches_and_keeps_them_ordered() {
        let mut t = CueTrack::default();
        t.extend(vec![SubtitleCue {
            start: Duration::from_secs(3),
            end: Duration::from_secs(4),
            lines: vec!["third".into()],
            forced: false,
            source_order: 0,
        }]);
        t.extend(vec![SubtitleCue {
            start: Duration::from_secs(1),
            end: Duration::from_secs(2),
            lines: vec!["first".into()],
            forced: false,
            source_order: 1,
        }]);
        assert_eq!(t.len(), 2);
        let texts: Vec<&str> = t.cues().iter().map(|c| c.lines[0].as_str()).collect();
        assert_eq!(texts, ["first", "third"], "sorted by start, not arrival");
    }

    /// A streamed batch continues the source_order sequence rather than restarting it —
    /// otherwise every batch would re-tie at zero and two cues starting on the same frame
    /// could swap.
    #[test]
    fn source_order_continues_across_batches() {
        let t = CueTrack::from_text_cues(vec![tc(1000, 2000, "a"), tc(2000, 3000, "b")]);
        assert_eq!(t.next_source_order(), 2);
        let more = text_cues_to_cues(vec![tc(3000, 4000, "c")], t.next_source_order());
        assert_eq!(more[0].source_order, 2);
    }
}
