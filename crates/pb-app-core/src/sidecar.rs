//! **Sidecar subtitle discovery** (task #90.1): finding `movie.en.forced.srt` next to
//! `movie.mkv`, and reading what its name is trying to tell us.
//!
//! Pure: the rules operate on a **list of sibling names**, never on the filesystem. That is
//! the seam that makes one implementation serve both sources — `FsSource` supplies names
//! from `read_dir`, `ZipSource` from its archive's entry list — and it makes every rule
//! below unit-testable with no I/O, no temp dirs, and no archive.
//!
//! Two decisions worth stating up front:
//!
//! - **Discovery enumerates; it never chooses.** Every matching sibling becomes its own
//!   track. This is why case-collisions need no tie-break: if an archive really holds both
//!   `movie.en.srt` and `Movie.EN.srt`, those are two different files, and listing both is
//!   correct rather than ambiguous.
//! - **Case-insensitive matching, everywhere, by rule.** Not "whatever the host filesystem
//!   does": a ZIP authored on Linux must find the same sidecars on Windows and macOS, and
//!   host behaviour would make that depend on where you happened to open it. A fixed rule
//!   is both source- and host-independent — which is what the plan's "case-sensitivity by
//!   source, not host OS" is protecting.

use pb_decode::tracks::{SidecarOrigin, TrackFlags};

/// A sibling that really is a subtitle for the video, and what its name said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarMatch {
    /// Where to read it from, again, later.
    pub origin: SidecarOrigin,
    /// The shared codec vocabulary (`"subrip"`, `"webvtt"`, `"ass"`) — the same keys
    /// [`pb_decode::tracks::subtitle_codec_display`] and `subtitle_capability` take, so a
    /// sidecar track describes itself exactly like an embedded one.
    pub codec_raw: &'static str,
    /// The language tag **as written in the filename**, normalized only for the no-op tags
    /// (`und`/`mis`/…). `movie.en.srt` → `Some("en")`; `movie.srt` → `None`.
    pub language: Option<String>,
    pub flags: TrackFlags,
    /// Anything in the name that wasn't a language or a flag — a human label the author
    /// chose (`movie.Director Commentary.srt`).
    pub title: Option<String>,
}

/// Subtitle file extensions → the shared codec vocabulary. `None` = not a subtitle file.
pub fn sidecar_codec(ext: &str) -> Option<&'static str> {
    Some(match ext.to_ascii_lowercase().as_str() {
        "srt" => "subrip",
        "vtt" | "webvtt" => "webvtt",
        "ass" => "ass",
        "ssa" => "ssa",
        "sub" => "microdvd",
        "smi" | "sami" => "sami",
        "ttml" | "dfxp" | "xml" => return None, // XML subs: real, but not v1 (and .xml is ambiguous)
        _ => return None,
    })
}

/// Filename tokens that mean a disposition rather than a language or a title.
///
/// Deliberately a fixed list: an unknown token becomes part of the **title**, never a
/// silently-dropped flag. Guessing wrong here would mislabel someone's track.
fn flag_token(t: &str) -> Option<fn(&mut TrackFlags)> {
    Some(match t {
        "forced" | "force" => |f: &mut TrackFlags| f.forced = true,
        // SDH and the caption/hearing-impaired abbreviations all mean the same thing here.
        "sdh" | "cc" | "hi" | "hearing-impaired" | "hearing_impaired" | "captions" => {
            |f: &mut TrackFlags| f.hearing_impaired = true
        }
        "ad" | "audio-description" | "描述" => |f: &mut TrackFlags| f.visual_impaired = true,
        "commentary" | "comment" => |f: &mut TrackFlags| f.commentary = true,
        "default" => |f: &mut TrackFlags| f.default = true,
        _ => return None,
    })
}

/// Does `token` name a language? Accepts a tag (`en`, `eng`, `pt-BR`) **or** an English
/// language name (`English`) — both appear in the wild.
///
/// Returns the tag to record. For a name we keep the *name*: the plan's rule is to preserve
/// what the source said, and `language_display` passes an unrecognized tag through, so
/// "English" displays as "English" either way.
fn language_token(token: &str) -> Option<String> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    // A tag: 2-3 letters, optionally with a region (`pt-BR`). Only accept it if the
    // language map actually knows it — otherwise `movie.v2.srt` would read as a language.
    let primary = t.split(['-', '_']).next().unwrap_or(t);
    let looks_like_tag = (2..=3).contains(&primary.chars().count())
        && primary.chars().all(|c| c.is_ascii_alphabetic());
    if looks_like_tag && pb_decode::tracks::language_display(t) != t {
        // `language_display` returning something *different* means it resolved the tag.
        return pb_decode::tracks::normalize_lang(t);
    }
    // `und`/`zxx` are tags we recognize as *no* information — consume them as a language
    // token (so they don't land in the title) but record nothing.
    if looks_like_tag && pb_decode::tracks::normalize_lang(t).is_none() {
        return Some(String::new()); // sentinel: "was a language token, but says nothing"
    }
    // A written-out name: "English", "Japanese".
    if t.chars().count() > 3 && known_language_name(t) {
        return Some(t.to_string());
    }
    None
}

/// Is `t` an English language name we'd display? Round-trips through the shared map's
/// values so this list can't drift from `language_display`.
fn known_language_name(t: &str) -> bool {
    // The tags whose display names cover the languages the map knows. Cheap and exact:
    // ask the map what each common tag displays as, and compare.
    const TAGS: &[&str] = &[
        "en", "fr", "es", "de", "it", "pt", "nl", "ru", "ja", "zh", "ko", "ar", "hi", "bn", "pa",
        "ta", "te", "th", "vi", "id", "ms", "tr", "pl", "uk", "cs", "sk", "hu", "ro", "bg", "el",
        "he", "fa", "sv", "no", "da", "fi", "is", "et", "lv", "lt", "sl", "hr", "sr", "bs", "mk",
        "sq", "ca", "eu", "gl", "cy", "ga", "sw", "af", "ur", "ml", "kn", "mr", "gu", "fil", "la",
    ];
    TAGS.iter()
        .any(|tag| pb_decode::tracks::language_display(tag).eq_ignore_ascii_case(t))
}

/// Split `name` into `(stem, ext)`. `ext` is `""` when there is no dot.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((s, e)) => (s, e),
        None => (name, ""),
    }
}

/// Does `candidate` name a sidecar for the video called `video_name`, and if so, what do
/// its tags say?
///
/// The stem rule is exact-or-dot-separated: `movie.mkv` matches `movie.srt` and
/// `movie.en.srt`, but **not** `movie2.srt` — a bare prefix test would claim the sequel's
/// subtitles.
pub fn parse_sidecar(video_name: &str, candidate: &str) -> Option<SidecarMatch> {
    let (video_stem, _) = split_ext(base_name(video_name));
    let cand_base = base_name(candidate);
    let (cand_stem, cand_ext) = split_ext(cand_base);
    let codec_raw = sidecar_codec(cand_ext)?;

    // Case-insensitive by rule (see the module docs), and the tail must be empty or start
    // at a dot boundary.
    let lower_stem = cand_stem.to_ascii_lowercase();
    let lower_video = video_stem.to_ascii_lowercase();
    if lower_stem.len() < lower_video.len() || !lower_stem.starts_with(&lower_video) {
        return None;
    }
    let tail = &cand_stem[video_stem.len()..];
    if !tail.is_empty() && !tail.starts_with('.') {
        return None; // `movie2.srt` is not `movie`'s sidecar
    }

    let mut flags = TrackFlags::none();
    let mut language: Option<String> = None;
    let mut title_parts: Vec<&str> = Vec::new();
    for token in tail.split('.').filter(|t| !t.is_empty()) {
        let lower = token.to_ascii_lowercase();
        if let Some(apply) = flag_token(&lower) {
            apply(&mut flags);
            continue;
        }
        // The first language-ish token wins; a later one is just a title word.
        if language.is_none() {
            if let Some(tag) = language_token(token) {
                if !tag.is_empty() {
                    language = Some(tag);
                }
                continue; // consumed, even for the says-nothing sentinel
            }
        }
        title_parts.push(token);
    }

    let title = (!title_parts.is_empty()).then(|| title_parts.join(" "));
    Some(SidecarMatch {
        // The caller owns *where* it lives; this function only reads the name.
        origin: SidecarOrigin::Path(std::path::PathBuf::new()),
        codec_raw,
        language,
        flags,
        title,
    })
}

/// The final path/entry component of a name (archive entries carry `dir/sub/name.srt`).
fn base_name(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// Match every sibling against the video, in one place, for every source.
///
/// `siblings` is `(name, origin)` — `FsSource` passes `read_dir` results, `ZipSource` its
/// archive entry names. Results are sorted by name so the listing is stable regardless of
/// what order the source enumerated in.
pub fn match_sidecars(
    video_name: &str,
    siblings: impl IntoIterator<Item = (String, SidecarOrigin)>,
) -> Vec<SidecarMatch> {
    let mut out: Vec<(String, SidecarMatch)> = siblings
        .into_iter()
        .filter_map(|(name, origin)| {
            let mut m = parse_sidecar(video_name, &name)?;
            m.origin = origin;
            Some((name, m))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.into_iter().map(|(_, m)| m).collect()
}

/// Discover the sidecars beside item `i`, ready to merge into the catalog.
///
/// The only I/O-touching function here, and it is thin by design: the source lists the
/// sibling names, the pure rules above decide which are subtitles for this video. Runs on
/// the Details worker (`media_details::probe_job`), never the event loop.
///
/// Read-only and RAM-only; nothing is written and nothing is remembered (privacy #2 — a
/// per-file subtitle choice would be a record of what you watched).
pub fn discover(source: &dyn pb_source::PhotoSource, i: usize) -> Vec<SidecarMatch> {
    let video_name = source.name(i).to_string();
    // How to get *back* to a sibling later: a real path for a loose file, the archive +
    // entry name for an archive entry. Neither → nothing to reopen, so nothing to offer.
    enum Home {
        Dir(std::path::PathBuf),
        Archive(std::path::PathBuf),
    }
    let home = match (source.path(i).and_then(|p| p.parent()), source.container()) {
        (Some(dir), _) => Home::Dir(dir.to_path_buf()),
        (None, Some(archive)) => Home::Archive(archive.to_path_buf()),
        (None, None) => return Vec::new(),
    };
    let siblings = source.sibling_names(i).into_iter().map(|name| {
        let origin = match &home {
            Home::Dir(dir) => SidecarOrigin::Path(dir.join(&name)),
            Home::Archive(archive) => SidecarOrigin::ArchiveEntry {
                archive: archive.clone(),
                entry: name.clone(),
            },
        };
        (name, origin)
    });
    match_sidecars(&video_name, siblings)
}

/// Turn discovered sidecars into catalog tracks, appended after the container's own
/// subtitle streams.
///
/// `next_local_id` continues the catalog's id sequence, so a sidecar's [`TrackId`] can
/// never collide with an embedded stream's — they share one namespace per catalog.
pub fn sidecar_tracks(
    matches: &[SidecarMatch],
    generation: u64,
    next_local_id: &mut u64,
) -> Vec<(pb_decode::MediaTrack, pb_decode::TrackLocator)> {
    matches
        .iter()
        .map(|m| {
            let local_id = *next_local_id;
            *next_local_id += 1;
            let track = pb_decode::MediaTrack {
                id: pb_decode::TrackId {
                    catalog_generation: generation,
                    local_id,
                },
                kind: pb_decode::TrackKind::Subtitle,
                language: m.language.clone(),
                title: m.title.clone(),
                codec_raw: m.codec_raw.to_string(),
                codec: pb_decode::tracks::subtitle_codec_display(m.codec_raw),
                capability: pb_decode::tracks::subtitle_capability(m.codec_raw),
                flags: m.flags,
                audio: None,
                external: true, // a file beside the video, not a stream inside it
            };
            (track, pb_decode::TrackLocator::Sidecar(m.origin.clone()))
        })
        .collect()
}

/// Decode a sidecar's bytes to text.
///
/// Subtitle files in the wild are a mess of encodings. This handles what the plan requires
/// — **UTF-8 (with or without BOM) and UTF-16 with a BOM** — and falls back to UTF-8-lossy
/// rather than failing: a cue with a few replacement characters is far more useful than no
/// subtitles at all. A BOM is never left in the text (it would show as a stray glyph in
/// the first cue).
pub fn decode_sidecar_text(bytes: &[u8]) -> String {
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xFF, 0xFE, rest @ ..] => decode_utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => decode_utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn decode_utf16(bytes: &[u8], to_u16: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| to_u16([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(video: &str, cand: &str) -> Option<SidecarMatch> {
        parse_sidecar(video, cand)
    }

    #[test]
    fn the_plans_four_named_cases() {
        // movie.srt — no tags at all.
        let m = parse("movie.mkv", "movie.srt").expect("plain");
        assert_eq!(m.codec_raw, "subrip");
        assert_eq!(m.language, None);
        assert_eq!(m.flags, TrackFlags::none());
        assert_eq!(m.title, None);

        // movie.en.srt
        let m = parse("movie.mkv", "movie.en.srt").expect("lang");
        assert_eq!(m.language.as_deref(), Some("en"));
        assert_eq!(m.flags, TrackFlags::none());

        // movie.eng.forced.srt
        let m = parse("movie.mkv", "movie.eng.forced.srt").expect("lang+forced");
        assert_eq!(m.language.as_deref(), Some("eng"));
        assert!(m.flags.forced);
        assert_eq!(m.title, None, "'forced' is a flag, never a title");

        // movie.en.sdh.vtt
        let m = parse("movie.mkv", "movie.en.sdh.vtt").expect("lang+sdh");
        assert_eq!(m.codec_raw, "webvtt");
        assert_eq!(m.language.as_deref(), Some("en"));
        assert!(m.flags.hearing_impaired);
        assert_eq!(m.title, None);
    }

    /// The bug a bare `starts_with` would ship: claiming the sequel's subtitles.
    #[test]
    fn a_longer_stem_is_not_a_sidecar() {
        assert!(parse("movie.mkv", "movie2.srt").is_none());
        assert!(parse("movie.mkv", "movie-extras.srt").is_none());
        assert!(parse("movie.mkv", "movies.en.srt").is_none());
        // ...but the dot boundary is fine.
        assert!(parse("movie.mkv", "movie.en.srt").is_some());
        // A shorter stem never matches either.
        assert!(parse("movie.mkv", "mov.srt").is_none());
    }

    /// Case-insensitive **by rule**, so a ZIP finds the same sidecars on every host.
    #[test]
    fn matching_is_case_insensitive_by_rule_not_by_host() {
        assert!(parse("Movie.MKV", "movie.srt").is_some());
        assert!(parse("movie.mkv", "MOVIE.EN.SRT").is_some());
        let m = parse("movie.mkv", "MOVIE.EN.FORCED.SRT").expect("upper");
        assert_eq!(
            m.language.as_deref(),
            Some("EN"),
            "the tag is preserved as written"
        );
        assert!(m.flags.forced, "flags are matched case-insensitively");
    }

    #[test]
    fn only_subtitle_extensions_match() {
        for (ext, codec) in [
            ("srt", "subrip"),
            ("vtt", "webvtt"),
            ("ass", "ass"),
            ("ssa", "ssa"),
            ("sub", "microdvd"),
        ] {
            let m = parse("movie.mkv", &format!("movie.{ext}")).unwrap_or_else(|| panic!("{ext}"));
            assert_eq!(m.codec_raw, codec, "{ext}");
        }
        // Not subtitles — and notably not the video itself, or a sibling video.
        for ext in ["mkv", "mp4", "jpg", "txt", "nfo", "ttml", "xml"] {
            assert!(
                parse("movie.mkv", &format!("movie.{ext}")).is_none(),
                "{ext}"
            );
        }
    }

    /// An unrecognized token is a *title*, never a silently-dropped flag — guessing would
    /// mislabel someone's track.
    #[test]
    fn unknown_tokens_become_the_title() {
        let m = parse("movie.mkv", "movie.en.Directors Cut.srt").expect("title");
        assert_eq!(m.language.as_deref(), Some("en"));
        assert_eq!(m.title.as_deref(), Some("Directors Cut"));
        assert_eq!(m.flags, TrackFlags::none());

        // A commentary keyword IS known, so it flags rather than titles.
        let m = parse("movie.mkv", "movie.commentary.srt").expect("commentary");
        assert!(m.flags.commentary);
        assert_eq!(m.title, None);
    }

    /// `movie.v2.srt` must not read `v2` as a language just because it's short.
    #[test]
    fn short_non_language_tokens_are_not_languages() {
        let m = parse("movie.mkv", "movie.v2.srt").expect("v2");
        assert_eq!(m.language, None);
        assert_eq!(m.title.as_deref(), Some("v2"));
    }

    #[test]
    fn written_out_language_names_resolve() {
        let m = parse("movie.mkv", "movie.English.srt").expect("English");
        assert_eq!(m.language.as_deref(), Some("English"));
        assert_eq!(m.title, None, "the name was consumed as the language");
        let m = parse("movie.mkv", "movie.Japanese.forced.srt").expect("Japanese");
        assert_eq!(m.language.as_deref(), Some("Japanese"));
        assert!(m.flags.forced);
    }

    /// A no-information tag is consumed (so it can't become a title) but records nothing.
    #[test]
    fn und_is_consumed_but_records_no_language() {
        let m = parse("movie.mkv", "movie.und.srt").expect("und");
        assert_eq!(m.language, None);
        assert_eq!(m.title, None, "'und' must not leak into the title");
    }

    #[test]
    fn regional_tags_survive() {
        let m = parse("movie.mkv", "movie.pt-BR.srt").expect("pt-BR");
        assert_eq!(m.language.as_deref(), Some("pt-BR"));
        assert_eq!(m.title, None);
    }

    #[test]
    fn all_the_hearing_impaired_spellings_mean_one_thing() {
        for t in ["sdh", "cc", "hi", "hearing-impaired", "captions"] {
            let m =
                parse("movie.mkv", &format!("movie.en.{t}.srt")).unwrap_or_else(|| panic!("{t}"));
            assert!(m.flags.hearing_impaired, "{t}");
            assert_eq!(m.title, None, "{t} must not land in the title");
        }
    }

    /// Archive entries carry a directory prefix; the video and the sidecar are compared on
    /// their base names.
    #[test]
    fn archive_entry_paths_compare_on_the_base_name() {
        let m = parse("Season 1/movie.mkv", "Season 1/movie.en.srt").expect("subdir");
        assert_eq!(m.language.as_deref(), Some("en"));
        assert!(
            parse("dir/movie.mkv", "dir\\movie.en.srt").is_some(),
            "windows separators"
        );
    }

    // -- match_sidecars ------------------------------------------------------

    fn origin(name: &str) -> SidecarOrigin {
        SidecarOrigin::Path(std::path::PathBuf::from(name))
    }

    #[test]
    fn match_sidecars_enumerates_every_sibling_and_sorts_them() {
        let siblings = [
            "movie.fr.srt",
            "movie.en.srt",
            "movie.en.forced.srt",
            "other.en.srt", // a different movie
            "movie.mkv",    // the video itself
            "notes.txt",
        ]
        .map(|n| (n.to_string(), origin(n)));

        let found = match_sidecars("movie.mkv", siblings);
        let langs: Vec<Option<&str>> = found.iter().map(|m| m.language.as_deref()).collect();
        // Sorted by name: en.forced, en, fr.
        assert_eq!(langs, vec![Some("en"), Some("en"), Some("fr")]);
        assert!(found[0].flags.forced, "movie.en.forced.srt sorts first");
        assert!(!found[1].flags.forced);
        assert_eq!(
            found.len(),
            3,
            "the video, the txt, and other.en.srt are not sidecars"
        );
        assert_eq!(
            found[2].origin,
            origin("movie.fr.srt"),
            "the origin is carried through"
        );
    }

    /// Discovery enumerates, it never chooses — so a case-collision inside an archive is
    /// two files and therefore two tracks, not an ambiguity needing a tie-break.
    #[test]
    fn a_case_collision_yields_two_tracks_not_a_conflict() {
        let siblings = ["movie.en.srt", "Movie.EN.srt"].map(|n| (n.to_string(), origin(n)));
        let found = match_sidecars("movie.mkv", siblings);
        assert_eq!(found.len(), 2, "two real files = two real tracks");
    }

    #[test]
    fn no_siblings_is_simply_no_sidecars() {
        assert!(match_sidecars("movie.mkv", []).is_empty());
    }

    // -- encodings -----------------------------------------------------------

    #[test]
    fn decodes_utf8_with_and_without_a_bom() {
        assert_eq!(
            decode_sidecar_text("Hello — こんにちは".as_bytes()),
            "Hello — こんにちは"
        );
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice("Hello".as_bytes());
        assert_eq!(
            decode_sidecar_text(&with_bom),
            "Hello",
            "the BOM must not survive"
        );
    }

    #[test]
    fn decodes_utf16_both_endians_by_bom() {
        let text = "Héllo — 日本";
        let mut le = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode_sidecar_text(&le), text);

        let mut be = vec![0xFE, 0xFF];
        for u in text.encode_utf16() {
            be.extend_from_slice(&u.to_be_bytes());
        }
        assert_eq!(decode_sidecar_text(&be), text);
    }

    // -- discovery over a real source ---------------------------------------

    /// End to end over a real `FsSource`: the `.srt` is not a library item, but discovery
    /// finds it, tags it, and hands back an origin that can be read again.
    #[test]
    fn discover_finds_sidecars_beside_a_real_video() {
        let dir = std::env::temp_dir().join(format!("pb_sc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in [
            "clip.mkv",
            "clip.en.srt",
            "clip.fr.forced.srt",
            "clip.jpg",     // a sibling library item, not a subtitle
            "other.en.srt", // another video's subtitle
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        use pb_source::PhotoSource;
        let src = pb_source::FsSource::new(vec![dir.join("clip.mkv")]);
        assert_eq!(src.len(), 1, "only the video is a library item");

        let found = discover(&src, 0);
        assert_eq!(
            found.len(),
            2,
            "the jpg and the other movie's srt are not ours"
        );
        // Sorted by name: clip.en.srt, clip.fr.forced.srt.
        assert_eq!(found[0].language.as_deref(), Some("en"));
        assert!(!found[0].flags.forced);
        assert_eq!(found[1].language.as_deref(), Some("fr"));
        assert!(found[1].flags.forced);
        // The origin is reopenable, and points at the real file.
        match &found[0].origin {
            SidecarOrigin::Path(p) => assert_eq!(p, &dir.join("clip.en.srt")),
            o => panic!("expected a Path origin, got {o:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sidecar tracks continue the catalog's id namespace, so a sidecar's `TrackId` can
    /// never collide with an embedded stream's.
    #[test]
    fn sidecar_tracks_continue_the_catalogs_id_namespace() {
        let m = parse("movie.mkv", "movie.en.sdh.srt").expect("match");
        let mut next = 7; // the backend already minted 0..=6
        let out = sidecar_tracks(&[m], 3, &mut next);
        assert_eq!(out.len(), 1);
        let (track, locator) = &out[0];
        assert_eq!(track.id.local_id, 7);
        assert_eq!(track.id.catalog_generation, 3);
        assert_eq!(next, 8, "the counter advances for the next caller");
        assert_eq!(track.kind, pb_decode::TrackKind::Subtitle);
        assert_eq!(track.codec, "SubRip");
        assert_eq!(track.capability, pb_decode::TrackCapability::SupportedText);
        assert!(track.flags.hearing_impaired);
        assert!(track.audio.is_none());
        assert!(matches!(locator, pb_decode::TrackLocator::Sidecar(_)));
    }

    /// A sidecar describes itself through the same maps as an embedded track — but says
    /// **External**, so a person can tell them apart.
    ///
    /// Found on real data (a Grey's Anatomy WEBRip): the release ships an embedded English
    /// SubRip stream *and* an `.eng.srt` of the same content. Without the marker both
    /// rendered as exactly "English · SubRip" — two identical rows, and an unanswerable
    /// choice in the #99 picker.
    #[test]
    fn a_sidecar_is_marked_external_so_it_is_distinguishable() {
        let m = parse("movie.mkv", "movie.en.forced.srt").expect("match");
        let mut next = 0;
        let (sidecar, _) = sidecar_tracks(&[m], 1, &mut next).remove(0);
        assert!(sidecar.external);
        assert_eq!(
            crate::tracks::track_summary(&sidecar),
            "English · SubRip · Forced · External"
        );

        // The same track as an embedded stream: identical but for the marker.
        let embedded = pb_decode::MediaTrack {
            external: false,
            ..sidecar.clone()
        };
        assert_eq!(
            crate::tracks::track_summary(&embedded),
            "English · SubRip · Forced"
        );
        assert_ne!(
            crate::tracks::track_summary(&embedded),
            crate::tracks::track_summary(&sidecar),
            "an embedded track and a sidecar must never read identically"
        );
    }

    /// The real-world shape that exposed this: one embedded `eng` SubRip stream plus one
    /// `.eng.srt` beside it, neither tagged beyond its language.
    #[test]
    fn the_greys_anatomy_case_yields_two_distinguishable_rows() {
        let m = parse(
            "Grey's.Anatomy.S01E01.1080p.AMZN.WEBRip.DD5.1.H.264-GA.mkv",
            "Grey's.Anatomy.S01E01.1080p.AMZN.WEBRip.DD5.1.H.264-GA.eng.srt",
        )
        .expect("a heavily dotted release name must still match");
        assert_eq!(m.language.as_deref(), Some("eng"));
        assert_eq!(
            m.title, None,
            "the release name's dots are in the STEM, so DD5.1/H.264 never reach the tail"
        );

        let mut next = 3;
        let (sidecar, _) = sidecar_tracks(&[m], 1, &mut next).remove(0);
        let embedded = pb_decode::MediaTrack {
            external: false,
            ..sidecar.clone()
        };
        assert_eq!(crate::tracks::track_summary(&embedded), "English · SubRip");
        assert_eq!(
            crate::tracks::track_summary(&sidecar),
            "English · SubRip · External"
        );
    }

    /// Hostile/mis-encoded bytes must degrade to text, not fail: a cue with a few
    /// replacement chars beats no subtitles.
    #[test]
    fn bad_bytes_degrade_rather_than_fail() {
        let latin1 = *b"caf\xe9"; // café in latin-1 — invalid UTF-8
        let out = decode_sidecar_text(&latin1);
        assert!(out.starts_with("caf"), "{out:?}");
        assert_eq!(decode_sidecar_text(&[]), "");
        // An odd-length UTF-16 body drops the stray byte instead of panicking.
        assert_eq!(decode_sidecar_text(&[0xFF, 0xFE, 0x41, 0x00, 0x42]), "A");
    }
}
