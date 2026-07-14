//! The **media-track catalog** (task #98, phase 1): the platform-neutral description of
//! a video's audio + subtitle tracks that every backend (FFmpeg / AVFoundation / Media
//! Foundation) produces and the Inspector, and later the #99 picker and the #90 cue
//! engine, consume.
//!
//! This is a *contract module*: pure data + pure maps, no decoding and no platform types.
//! The codec/language/capability maps are keyed on **strings** (FFmpeg's
//! `avcodec_get_name` vocabulary) rather than an `AVCodecID`, so they stay testable with
//! no FFmpeg in the build — the backends normalize into this vocabulary at the seam.
//!
//! Design constraints baked into these types (plan `.taskmaster/plans/`, Codex review, and
//! the phase-0 spikes in `.taskmaster/docs/98-phase0-spike-findings.md`):
//!
//! - **An empty vector cannot represent degradation.** "Silent", "the backend can't
//!   enumerate", "the probe failed", "we know the count but not the tracks", and "we got
//!   some of them" are five different facts, and rendering any of them as *no audio* is a
//!   lie. Hence [`TrackSet`] carries an explicit [`TrackCompleteness`] and a `total`
//!   separate from `tracks`.
//! - **Backends genuinely disagree, so the catalog names its backend.** The spike proved
//!   AVFoundation *synthesizes* options (2 authored subtitle tracks → 4 legible options,
//!   with its own `defaultOption` overriding the authored one) while FFmpeg reports the 2
//!   real streams. Neither is wrong; a catalog without [`MediaBackend`] would have to
//!   pretend one of them is.
//! - **Identity is scoped, never a bare index.** An FFmpeg stream index, an MF stream
//!   ordinal, and an AVFoundation option are different namespaces, so a stale catalog must
//!   not be able to select the wrong track after a re-probe. See [`TrackId`] /
//!   [`TrackLocator`].

use std::collections::BTreeMap;

/// Which backend enumerated a catalog. Not a diagnostic: the same file honestly
/// enumerates *differently* per backend (phase-0 spike), so the reader needs to know
/// which one spoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaBackend {
    FFmpeg,
    AVFoundation,
    MediaFoundation,
}

/// How much of a [`TrackSet`] the backend could actually describe. The Details rows and
/// the later picker branch on this — never on `tracks.is_empty()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackCompleteness {
    /// Every track of this kind is described. `total == Some(tracks.len())`, and
    /// `total == Some(0)` is the honest, useful "this file has no audio".
    Complete,
    /// The backend knows how many tracks exist but can't describe them.
    CountOnly,
    /// Some tracks are described, but the set is known to be incomplete.
    Partial,
    /// The backend cannot enumerate this kind at all, or the probe failed. **Never**
    /// render this as "No".
    Unavailable,
}

/// Audio or subtitle. (Video is [`crate::VideoStreamInfo`]'s job — the catalog is about
/// the tracks the user picks *between*.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackKind {
    Audio,
    Subtitle,
}

/// What a subtitle track's payload *is*, which decides whether #90 can render it and
/// whether #99 may offer it. Audio tracks are always [`TrackCapability::Playable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackCapability {
    /// An audio track we can play.
    Playable,
    /// Plain timed text (SubRip, WebVTT, tx3g) — the #90 render target.
    SupportedText,
    /// Markup/styled text (ASS/SSA). Renderable as text; styling is out of scope for #90.
    StyledText,
    /// Bitmap subtitles (PGS, VobSub, DVB). Would need image compositing, not text
    /// shaping — listed in Details, never offered as a functional picker row.
    Bitmap,
    /// Recognized but not renderable by us (embedded captions, teletext, unknown ids).
    Unsupported,
}

impl TrackCapability {
    /// Whether #90 can put this track's payload on screen as text. Drives the
    /// "Unsupported" note in Details and the picker's disabled rows.
    pub fn is_renderable_text(self) -> bool {
        matches!(
            self,
            TrackCapability::SupportedText | TrackCapability::StyledText
        )
    }
}

/// Container dispositions, normalized across backends. Kept as named bools rather than
/// bitflags: they're read individually by the formatter and there are five of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TrackFlags {
    /// The *authored* container default. On AVFoundation this is the group's
    /// `defaultOption`, which the spike proved is the backend's pick and may differ from
    /// the authored flag — it is still "what this backend calls default", and is
    /// deliberately **not** conflated with the player's runtime auto-selection (#99).
    pub default: bool,
    pub forced: bool,
    pub commentary: bool,
    pub hearing_impaired: bool,
    pub visual_impaired: bool,
}

impl TrackFlags {
    pub fn none() -> Self {
        Self::default()
    }
}

/// An audio track's format facts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    pub channels: u16,
    /// The container's *named* layout ("stereo", "5.1", "5.1(side)"), or `None` when the
    /// layout is genuinely unknown (FFmpeg `AV_CHANNEL_ORDER_UNSPEC`). `None` means the
    /// formatter reports the channel **count** — we never invent "5.1" from `channels == 6`.
    pub layout: Option<String>,
    /// Sample rate in Hz; 0 = unknown.
    pub sample_rate: u32,
}

/// Stable, catalog-scoped track identity. `local_id` is only meaningful within the
/// catalog whose `generation` it carries, so a selection command minted against a stale
/// catalog is rejected rather than silently resolving to a different track.
///
/// **Never persisted**: it is both stale-by-construction across runs and a viewing trace
/// (privacy #2). Selection commands (#99) always carry `session_id` + `catalog_generation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackId {
    pub catalog_generation: u64,
    pub local_id: u64,
}

/// Which AVFoundation media-selection group an option came from — the group is required
/// to resolve a `propertyList` back to an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AvGroup {
    Audible,
    Legible,
}

/// Where a **sidecar** subtitle lives (task #90.1) — a *reopenable* locator, deliberately
/// not a `bool`.
///
/// A sidecar is not a stream in the video's container, so it has no stream index, and it is
/// not a library item either (the archive/scan predicates index images and video containers
/// only), so it has no item index. What it has is a place we can go back to and read again
/// — and that is all this needs to be.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SidecarOrigin {
    /// A sibling file on disk, by absolute path.
    Path(std::path::PathBuf),
    /// An entry in the same archive as the video, by its **exact entry name**.
    ///
    /// By name, not index: a `.srt` is not indexed as a library item, so no item index for
    /// it exists — and a name also survives a re-scan, where an index would not.
    ArchiveEntry {
        archive: std::path::PathBuf,
        entry: String,
    },
}

/// How the **owning backend** re-finds a track at selection time. Opaque to the UI and
/// the formatter — those see only [`TrackId`] — and held on the catalog in a side map
/// (see [`MediaTrackCatalog::locator`]) rather than on [`MediaTrack`], so no display or
/// test code has to carry it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TrackLocator {
    /// A real FFmpeg stream index.
    FfStream(usize),
    /// A Media Foundation stream ordinal.
    MfStream(u32),
    /// An `AVMediaSelectionOption`, as its `propertyList` serialized to a binary plist.
    /// The phase-0 spike proved this round-trips through
    /// `mediaSelectionOptionWithPropertyList:` to an option that `isEqual:` the original.
    AvOption {
        group: AvGroup,
        property_list: Vec<u8>,
    },
    /// A subtitle file beside the video, rather than a stream inside it (task #90.1).
    Sidecar(SidecarOrigin),
}

/// One audio or subtitle track.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MediaTrack {
    pub id: TrackId,
    pub kind: TrackKind,
    /// The language tag **exactly as the container reported it**, after dropping the
    /// no-op values (`und`/`mis`/`zxx`/empty). Deliberately not canonicalized to one
    /// form: FFmpeg says `"eng"` and AVFoundation says `"en"` for the same file
    /// (phase-0 spike), and rewriting either would discard what the source said.
    /// [`language_display`] resolves both.
    pub language: Option<String>,
    /// The container's per-track title. This is where "Director's Commentary" usually
    /// lives, and it is often the *only* signal that a track is a commentary. Never the
    /// AVFoundation `displayName` (that's a localized language name, not a title).
    pub title: Option<String>,
    /// The backend's raw codec id (`"dts"`, `"hdmv_pgs_subtitle"`) — diagnostics, and the
    /// key the display/capability maps read.
    pub codec_raw: String,
    /// Friendly display name ("DTS-HD MA", "SubRip").
    ///
    /// *Deviation from the plan's `&'static str`:* an unmapped codec must still show
    /// **its own name** (`"COOK"`) rather than collapse to `"Audio"` and throw away what
    /// `codec_raw` knows. A `String` costs nothing here — a catalog is built once per
    /// probe, off the hot path entirely.
    pub codec: String,
    pub capability: TrackCapability,
    pub flags: TrackFlags,
    /// `Some` for audio, `None` for subtitles.
    pub audio: Option<AudioFormat>,
}

/// The tracks of one kind, plus how much of them we actually know.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackSet {
    pub completeness: TrackCompleteness,
    /// The count the backend reports, which may exceed `tracks.len()` when the backend
    /// can count but not describe. `None` = not even the count is known.
    pub total: Option<u16>,
    pub tracks: Vec<MediaTrack>,
}

impl Default for TrackCompleteness {
    /// The safe default: claim nothing.
    fn default() -> Self {
        TrackCompleteness::Unavailable
    }
}

impl TrackSet {
    /// A fully-described set — `total` is derived, so it can't disagree with `tracks`.
    pub fn complete(tracks: Vec<MediaTrack>) -> Self {
        TrackSet {
            completeness: TrackCompleteness::Complete,
            total: Some(tracks.len().min(u16::MAX as usize) as u16),
            tracks,
        }
    }

    /// The backend counted `n` tracks but can describe none of them.
    pub fn count_only(n: u16) -> Self {
        TrackSet {
            completeness: TrackCompleteness::CountOnly,
            total: Some(n),
            tracks: Vec::new(),
        }
    }

    /// The backend cannot enumerate this kind, or the probe failed.
    pub fn unavailable() -> Self {
        TrackSet {
            completeness: TrackCompleteness::Unavailable,
            total: None,
            tracks: Vec::new(),
        }
    }

    /// Some tracks are described but the set is known incomplete. `total` is the real
    /// count when known.
    pub fn partial(tracks: Vec<MediaTrack>, total: Option<u16>) -> Self {
        TrackSet {
            completeness: TrackCompleteness::Partial,
            total,
            tracks,
        }
    }

    /// The positively-known "this file has none of this kind" — *only* a `Complete` set
    /// with a zero total. An `Unavailable`/`CountOnly` set is never "none", which is the
    /// whole point of tracking completeness.
    pub fn is_known_empty(&self) -> bool {
        self.completeness == TrackCompleteness::Complete && self.total == Some(0)
    }
}

/// Everything one details probe learned about a container's selectable tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaTrackCatalog {
    /// The probe epoch this catalog was minted in — the key that makes a stale result
    /// droppable and every [`TrackId`] in it unambiguous.
    pub generation: u64,
    pub backend: MediaBackend,
    pub audio: TrackSet,
    pub subtitles: TrackSet,
    /// `local_id` → how the owning backend re-finds the track. Kept out of [`MediaTrack`]
    /// so display code and hand-built test tracks never carry it.
    locators: BTreeMap<u64, TrackLocator>,
}

impl MediaTrackCatalog {
    pub fn new(
        generation: u64,
        backend: MediaBackend,
        audio: TrackSet,
        subtitles: TrackSet,
    ) -> Self {
        MediaTrackCatalog {
            generation,
            backend,
            audio,
            subtitles,
            locators: BTreeMap::new(),
        }
    }

    /// A catalog from a backend that enumerated nothing — the honest failure value.
    pub fn unavailable(generation: u64, backend: MediaBackend) -> Self {
        Self::new(
            generation,
            backend,
            TrackSet::unavailable(),
            TrackSet::unavailable(),
        )
    }

    /// Record how to re-find `local_id`. Backends call this as they enumerate.
    pub fn set_locator(&mut self, local_id: u64, locator: TrackLocator) {
        self.locators.insert(local_id, locator);
    }

    /// Resolve a track id to its locator — **only** for the backend that owns this
    /// catalog, and only when the id was minted against this generation. A mismatched
    /// generation is the stale-selection case and yields `None` rather than a wrong track.
    pub fn locator(&self, id: TrackId) -> Option<&TrackLocator> {
        if id.catalog_generation != self.generation {
            return None;
        }
        self.locators.get(&id.local_id)
    }

    /// Mint an id in this catalog's generation.
    pub fn track_id(&self, local_id: u64) -> TrackId {
        TrackId {
            catalog_generation: self.generation,
            local_id,
        }
    }

    /// Every described track, audio then subtitles.
    pub fn tracks(&self) -> impl Iterator<Item = &MediaTrack> {
        self.audio.tracks.iter().chain(self.subtitles.tracks.iter())
    }
}

// ---------------------------------------------------------------------------
// Pure maps: codec ids -> friendly names / capability, and language tags.
// Keyed on FFmpeg's `avcodec_get_name` vocabulary; other backends normalize into it.
// ---------------------------------------------------------------------------

/// FFmpeg's `AVCodecContext.profile` sentinel for "not reported".
pub const PROFILE_UNKNOWN: i32 = -99;

/// DTS profile code points (`AV_PROFILE_DTS_*`). Mirrored here so this module stays
/// FFmpeg-free and unit-testable; `ffmpeg::tracks` static-asserts they still match the
/// real constants, so a header change fails a test rather than mislabels a track.
pub const PROFILE_DTS: i32 = 20;
pub const PROFILE_DTS_ES: i32 = 30;
pub const PROFILE_DTS_96_24: i32 = 40;
pub const PROFILE_DTS_HD_HRA: i32 = 50;
pub const PROFILE_DTS_HD_MA: i32 = 60;
pub const PROFILE_DTS_EXPRESS: i32 = 70;

/// Friendly display name for an audio codec.
///
/// `profile` is FFmpeg's raw `(*par).profile`, and is interpreted **only** in the arm of
/// the codec it belongs to. This keying is not stylistic: the phase-0 spike found the
/// constants genuinely collide — `AV_PROFILE_DTS_ES == AV_PROFILE_TRUEHD_ATMOS ==
/// AV_PROFILE_EAC3_DDP_ATMOS == 30` — so a codec-blind profile map would render an Atmos
/// TrueHD track as "DTS-ES".
///
/// An unmapped codec returns its own id uppercased, never a generic "Audio".
pub fn audio_codec_display(codec_raw: &str, profile: Option<i32>) -> String {
    let p = profile.unwrap_or(PROFILE_UNKNOWN);
    let known = match codec_raw {
        "aac" | "aac_latm" => "AAC",
        "ac3" => "AC-3",
        "eac3" => "E-AC-3",
        "dts" => match p {
            PROFILE_DTS_ES => "DTS-ES",
            PROFILE_DTS_96_24 => "DTS 96/24",
            PROFILE_DTS_HD_HRA => "DTS-HD HRA",
            PROFILE_DTS_HD_MA => "DTS-HD MA",
            PROFILE_DTS_EXPRESS => "DTS Express",
            // PROFILE_DTS and anything unrecognized (incl. PROFILE_UNKNOWN) → plain DTS.
            _ => "DTS",
        },
        // TrueHD and MLP share a lineage but are distinct codecs; the plan requires they
        // stay distinguishable rather than both reading "TrueHD".
        "truehd" => "TrueHD",
        "mlp" => "MLP",
        "flac" => "FLAC",
        "alac" => "ALAC",
        "opus" => "Opus",
        "vorbis" => "Vorbis",
        "mp3" | "mp3float" => "MP3",
        "mp2" | "mp2float" => "MP2",
        "mp1" | "mp1float" => "MP1",
        "wmav1" | "wmav2" | "wmapro" | "wmalossless" => "WMA",
        "amr_nb" | "amr_wb" => "AMR",
        "speex" => "Speex",
        "atrac3" | "atrac3p" | "atrac9" => "ATRAC",
        "dsd_lsbf" | "dsd_msbf" | "dsd_lsbf_planar" | "dsd_msbf_planar" => "DSD",
        // Every PCM flavour (pcm_s16le, pcm_f32be, pcm_bluray, …) reads as "PCM"; the
        // exact one stays in `codec_raw` for diagnostics.
        c if c.starts_with("pcm_") => "PCM",
        c if c.starts_with("adpcm_") => "ADPCM",
        _ => return upper_fallback(codec_raw),
    };
    known.to_string()
}

/// Friendly display name for a subtitle codec. Unmapped → its own id uppercased.
pub fn subtitle_codec_display(codec_raw: &str) -> String {
    let known = match codec_raw {
        "subrip" | "srt" => "SubRip",
        "ass" => "ASS",
        "ssa" => "SSA",
        "webvtt" => "WebVTT",
        // tx3g, the MP4/MOV-native subtitle codec.
        "mov_text" => "Timed Text",
        "text" => "Text",
        "hdmv_pgs_subtitle" => "PGS",
        "hdmv_text_subtitle" => "HDMV Text",
        "dvd_subtitle" => "VobSub",
        "dvb_subtitle" => "DVB Subtitle",
        "dvb_teletext" => "Teletext",
        "xsub" => "XSUB",
        "eia_608" => "EIA-608",
        "cea_708" => "CEA-708",
        "microdvd" => "MicroDVD",
        "mpl2" => "MPL2",
        "subviewer" | "subviewer1" => "SubViewer",
        "sami" => "SAMI",
        "realtext" => "RealText",
        "stl" => "Spruce STL",
        "jacosub" => "JACOsub",
        "pjs" => "PJS",
        "vplayer" => "VPlayer",
        _ => return upper_fallback(codec_raw),
    };
    known.to_string()
}

/// What we could do with a subtitle track's payload.
pub fn subtitle_capability(codec_raw: &str) -> TrackCapability {
    match codec_raw {
        // Plain timed text — what #90 renders.
        "subrip" | "srt" | "webvtt" | "mov_text" | "text" | "subviewer" | "subviewer1"
        | "microdvd" | "mpl2" | "vplayer" | "pjs" | "jacosub" | "realtext" | "stl"
        | "hdmv_text_subtitle" => TrackCapability::SupportedText,
        // Markup/styled text: renderable as text, styling out of #90's scope.
        "ass" | "ssa" | "sami" => TrackCapability::StyledText,
        // Bitmap: image compositing, not text shaping.
        "hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle" | "xsub" => TrackCapability::Bitmap,
        // Embedded caption/teletext streams need their own pipeline; honestly unsupported.
        _ => TrackCapability::Unsupported,
    }
}

/// Uppercase an unmapped codec id for display: `"cook"` → `"COOK"`, and the common
/// `_`-separated ids read better with spaces (`"binkaudio_dct"` → `"BINKAUDIO DCT"`).
fn upper_fallback(codec_raw: &str) -> String {
    if codec_raw.is_empty() {
        return String::new();
    }
    codec_raw.replace('_', " ").to_uppercase()
}

/// The language tags that carry no information and must read as "unknown" rather than be
/// shown: ISO-639-2 `und` (undetermined), `mis` (uncoded), `mul` (multiple), `zxx` (no
/// linguistic content), and the empty tag. FFmpeg stamps `und` on most untagged streams.
pub fn normalize_lang(tag: &str) -> Option<String> {
    let t = tag.trim();
    if t.is_empty() {
        return None;
    }
    let primary = t.split(['-', '_']).next().unwrap_or(t).to_ascii_lowercase();
    if matches!(primary.as_str(), "und" | "mis" | "mul" | "zxx" | "qaa") {
        return None;
    }
    Some(t.to_string())
}

/// A human display name for a language tag, resolving the **primary subtag** so both
/// FFmpeg's ISO-639-2 (`"eng"`) and AVFoundation's BCP-47 (`"en"`) forms — which the
/// phase-0 spike proved appear for the same content — land on "English". Regional tags
/// keep their qualifier (`"pt-BR"` → "Portuguese (BR)").
///
/// **Passthrough for the rest**: an unrecognized tag is returned as given, never dropped
/// and never guessed at.
pub fn language_display(tag: &str) -> String {
    let t = tag.trim();
    let mut parts = t.split(['-', '_']);
    let primary = parts.next().unwrap_or(t).to_ascii_lowercase();
    let region: Option<&str> = parts.next();
    let Some(name) = language_name(&primary) else {
        return t.to_string();
    };
    match region {
        Some(r) if !r.is_empty() => format!("{name} ({})", r.to_ascii_uppercase()),
        _ => name.to_string(),
    }
}

/// ISO-639-1 / -2/T / -2/B primary subtag → English display name. Covers the languages
/// that actually show up on subtitle/audio tracks; everything else passes through
/// [`language_display`] verbatim.
fn language_name(primary: &str) -> Option<&'static str> {
    Some(match primary {
        "en" | "eng" => "English",
        "fr" | "fra" | "fre" => "French",
        "es" | "spa" => "Spanish",
        "de" | "deu" | "ger" => "German",
        "it" | "ita" => "Italian",
        "pt" | "por" => "Portuguese",
        "nl" | "nld" | "dut" => "Dutch",
        "ru" | "rus" => "Russian",
        "ja" | "jpn" => "Japanese",
        "zh" | "zho" | "chi" => "Chinese",
        "ko" | "kor" => "Korean",
        "ar" | "ara" => "Arabic",
        "hi" | "hin" => "Hindi",
        "bn" | "ben" => "Bengali",
        "pa" | "pan" => "Punjabi",
        "ta" | "tam" => "Tamil",
        "te" | "tel" => "Telugu",
        "th" | "tha" => "Thai",
        "vi" | "vie" => "Vietnamese",
        "id" | "ind" => "Indonesian",
        "ms" | "msa" | "may" => "Malay",
        "tr" | "tur" => "Turkish",
        "pl" | "pol" => "Polish",
        "uk" | "ukr" => "Ukrainian",
        "cs" | "ces" | "cze" => "Czech",
        "sk" | "slk" | "slo" => "Slovak",
        "hu" | "hun" => "Hungarian",
        "ro" | "ron" | "rum" => "Romanian",
        "bg" | "bul" => "Bulgarian",
        "el" | "ell" | "gre" => "Greek",
        "he" | "heb" => "Hebrew",
        "fa" | "fas" | "per" => "Persian",
        "sv" | "swe" => "Swedish",
        "no" | "nor" | "nb" | "nob" => "Norwegian",
        "da" | "dan" => "Danish",
        "fi" | "fin" => "Finnish",
        "is" | "isl" | "ice" => "Icelandic",
        "et" | "est" => "Estonian",
        "lv" | "lav" => "Latvian",
        "lt" | "lit" => "Lithuanian",
        "sl" | "slv" => "Slovenian",
        "hr" | "hrv" => "Croatian",
        "sr" | "srp" => "Serbian",
        "bs" | "bos" => "Bosnian",
        "mk" | "mkd" | "mac" => "Macedonian",
        "sq" | "sqi" | "alb" => "Albanian",
        "ca" | "cat" => "Catalan",
        "eu" | "eus" | "baq" => "Basque",
        "gl" | "glg" => "Galician",
        "cy" | "cym" | "wel" => "Welsh",
        "ga" | "gle" => "Irish",
        "sw" | "swa" => "Swahili",
        "af" | "afr" => "Afrikaans",
        "ur" | "urd" => "Urdu",
        "ml" | "mal" => "Malayalam",
        "kn" | "kan" => "Kannada",
        "mr" | "mar" => "Marathi",
        "gu" | "guj" => "Gujarati",
        "fil" | "tl" | "tgl" => "Filipino",
        "la" | "lat" => "Latin",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- codec maps ---------------------------------------------------------

    #[test]
    fn dts_profiles_map_to_their_marketing_names() {
        let dts = |p: i32| audio_codec_display("dts", Some(p));
        assert_eq!(dts(PROFILE_DTS), "DTS");
        assert_eq!(dts(PROFILE_DTS_ES), "DTS-ES");
        assert_eq!(dts(PROFILE_DTS_96_24), "DTS 96/24");
        assert_eq!(dts(PROFILE_DTS_HD_HRA), "DTS-HD HRA");
        assert_eq!(dts(PROFILE_DTS_HD_MA), "DTS-HD MA");
        assert_eq!(dts(PROFILE_DTS_EXPRESS), "DTS Express");
        // An unreported or unrecognized profile degrades to plain DTS, never to a
        // neighbouring profile's name.
        assert_eq!(dts(PROFILE_UNKNOWN), "DTS");
        assert_eq!(audio_codec_display("dts", None), "DTS");
        assert_eq!(dts(12345), "DTS");
    }

    /// The phase-0 spike's headline hazard: `AV_PROFILE_DTS_ES`,
    /// `AV_PROFILE_TRUEHD_ATMOS` and `AV_PROFILE_EAC3_DDP_ATMOS` are **all 30**. A
    /// codec-blind profile map would call an Atmos TrueHD track "DTS-ES".
    #[test]
    fn profile_ints_are_never_read_outside_their_own_codec() {
        assert_eq!(
            audio_codec_display("truehd", Some(PROFILE_DTS_ES)),
            "TrueHD"
        );
        assert_eq!(audio_codec_display("eac3", Some(PROFILE_DTS_ES)), "E-AC-3");
        assert_eq!(audio_codec_display("aac", Some(PROFILE_DTS_HD_MA)), "AAC");
        // ...and the real DTS still resolves.
        assert_eq!(audio_codec_display("dts", Some(PROFILE_DTS_ES)), "DTS-ES");
    }

    #[test]
    fn truehd_and_mlp_stay_distinguishable() {
        assert_eq!(audio_codec_display("truehd", None), "TrueHD");
        assert_eq!(audio_codec_display("mlp", None), "MLP");
    }

    #[test]
    fn every_pcm_flavour_reads_as_pcm() {
        for raw in [
            "pcm_s16le",
            "pcm_s24be",
            "pcm_f32le",
            "pcm_bluray",
            "pcm_dvd",
        ] {
            assert_eq!(audio_codec_display(raw, None), "PCM", "{raw}");
        }
        assert_eq!(audio_codec_display("adpcm_ima_qt", None), "ADPCM");
    }

    #[test]
    fn common_audio_codecs_map() {
        for (raw, want) in [
            ("aac", "AAC"),
            ("ac3", "AC-3"),
            ("eac3", "E-AC-3"),
            ("flac", "FLAC"),
            ("alac", "ALAC"),
            ("opus", "Opus"),
            ("vorbis", "Vorbis"),
            ("mp3", "MP3"),
            ("wmapro", "WMA"),
        ] {
            assert_eq!(audio_codec_display(raw, None), want, "{raw}");
        }
    }

    /// An unmapped codec must show *its own name*, not collapse to a generic label that
    /// throws away what `codec_raw` already knows.
    #[test]
    fn unmapped_codecs_fall_back_to_their_own_id() {
        assert_eq!(audio_codec_display("cook", None), "COOK");
        assert_eq!(audio_codec_display("binkaudio_dct", None), "BINKAUDIO DCT");
        assert_eq!(subtitle_codec_display("some_new_codec"), "SOME NEW CODEC");
        assert_eq!(audio_codec_display("", None), "");
    }

    #[test]
    fn subtitle_codecs_map_and_classify() {
        for (raw, name, cap) in [
            ("subrip", "SubRip", TrackCapability::SupportedText),
            ("webvtt", "WebVTT", TrackCapability::SupportedText),
            ("mov_text", "Timed Text", TrackCapability::SupportedText),
            ("ass", "ASS", TrackCapability::StyledText),
            ("ssa", "SSA", TrackCapability::StyledText),
            ("hdmv_pgs_subtitle", "PGS", TrackCapability::Bitmap),
            ("dvd_subtitle", "VobSub", TrackCapability::Bitmap),
            ("dvb_subtitle", "DVB Subtitle", TrackCapability::Bitmap),
            ("eia_608", "EIA-608", TrackCapability::Unsupported),
            ("dvb_teletext", "Teletext", TrackCapability::Unsupported),
        ] {
            assert_eq!(subtitle_codec_display(raw), name, "{raw}");
            assert_eq!(subtitle_capability(raw), cap, "{raw}");
        }
    }

    #[test]
    fn only_text_capabilities_are_renderable() {
        assert!(TrackCapability::SupportedText.is_renderable_text());
        assert!(TrackCapability::StyledText.is_renderable_text());
        assert!(!TrackCapability::Bitmap.is_renderable_text());
        assert!(!TrackCapability::Unsupported.is_renderable_text());
        assert!(!TrackCapability::Playable.is_renderable_text());
    }

    // -- language -----------------------------------------------------------

    #[test]
    fn normalize_lang_drops_the_no_information_tags() {
        for t in ["und", "UND", "mis", "mul", "zxx", "qaa", "", "   "] {
            assert_eq!(normalize_lang(t), None, "{t:?}");
        }
        assert_eq!(normalize_lang("eng"), Some("eng".to_string()));
        assert_eq!(normalize_lang("en"), Some("en".to_string()));
        // The raw tag is preserved verbatim — case, region and all.
        assert_eq!(normalize_lang("pt-BR"), Some("pt-BR".to_string()));
        assert_eq!(normalize_lang(" fr "), Some("fr".to_string()));
        // `und-US` is still undetermined.
        assert_eq!(normalize_lang("und-US"), None);
    }

    /// The spike's correction: AVFoundation reports `"en"`/`"fr"` where FFmpeg reports
    /// `"eng"`/`"fra"` for the same file. Both must resolve, or the same MP4 reads
    /// differently on the two macOS backends.
    #[test]
    fn language_display_resolves_both_iso_639_1_and_639_2() {
        assert_eq!(language_display("en"), "English");
        assert_eq!(language_display("eng"), "English");
        assert_eq!(language_display("fr"), "French");
        assert_eq!(language_display("fra"), "French");
        // 639-2/B (bibliographic) variants exist in the wild alongside 639-2/T.
        assert_eq!(language_display("fre"), "French");
        assert_eq!(language_display("ger"), "German");
        assert_eq!(language_display("deu"), "German");
        assert_eq!(language_display("jpn"), "Japanese");
        assert_eq!(language_display("ja"), "Japanese");
    }

    #[test]
    fn language_display_keeps_regional_qualifiers() {
        assert_eq!(language_display("pt-BR"), "Portuguese (BR)");
        assert_eq!(language_display("en_GB"), "English (GB)");
        assert_eq!(language_display("zh-Hant"), "Chinese (HANT)");
    }

    /// Never hardcode only en/eng: an unrecognized tag passes through verbatim rather
    /// than being dropped or guessed at.
    #[test]
    fn language_display_passes_unknown_tags_through() {
        assert_eq!(language_display("tlh"), "tlh");
        assert_eq!(language_display("x-custom"), "x-custom");
        assert_eq!(language_display("Klingon"), "Klingon");
    }

    // -- completeness / sets ------------------------------------------------

    fn track(local_id: u64, kind: TrackKind) -> MediaTrack {
        MediaTrack {
            id: TrackId {
                catalog_generation: 7,
                local_id,
            },
            kind,
            language: Some("eng".into()),
            title: None,
            codec_raw: "aac".into(),
            codec: "AAC".into(),
            capability: TrackCapability::Playable,
            flags: TrackFlags::none(),
            audio: None,
        }
    }

    /// The core reason completeness exists: only a `Complete` set with zero tracks means
    /// "this file has no audio". Every other empty set means "we don't know".
    #[test]
    fn only_a_complete_zero_set_is_known_empty() {
        assert!(TrackSet::complete(vec![]).is_known_empty());
        assert!(!TrackSet::unavailable().is_known_empty());
        assert!(!TrackSet::count_only(3).is_known_empty());
        assert!(!TrackSet::count_only(0).is_known_empty());
        assert!(!TrackSet::partial(vec![], Some(2)).is_known_empty());
        assert!(!TrackSet::complete(vec![track(0, TrackKind::Audio)]).is_known_empty());
    }

    #[test]
    fn complete_sets_derive_a_consistent_total() {
        let set = TrackSet::complete(vec![track(0, TrackKind::Audio), track(1, TrackKind::Audio)]);
        assert_eq!(set.total, Some(2));
        assert_eq!(set.completeness, TrackCompleteness::Complete);
        assert_eq!(TrackSet::count_only(5).total, Some(5));
        assert_eq!(TrackSet::unavailable().total, None);
    }

    /// The default must claim nothing — a `TrackSet::default()` that read as `Complete`
    /// would silently assert "no audio".
    #[test]
    fn default_track_set_claims_nothing() {
        let d = TrackSet::default();
        assert_eq!(d.completeness, TrackCompleteness::Unavailable);
        assert!(!d.is_known_empty());
        assert_eq!(d.total, None);
    }

    // -- identity / locators ------------------------------------------------

    #[test]
    fn locators_resolve_only_within_their_generation() {
        let mut cat = MediaTrackCatalog::new(
            7,
            MediaBackend::FFmpeg,
            TrackSet::complete(vec![track(0, TrackKind::Audio)]),
            TrackSet::complete(vec![]),
        );
        cat.set_locator(0, TrackLocator::FfStream(3));

        let id = cat.track_id(0);
        assert_eq!(id.catalog_generation, 7);
        assert_eq!(cat.locator(id), Some(&TrackLocator::FfStream(3)));

        // A id minted against a *different* (stale) catalog generation must not resolve,
        // even though `local_id` 0 exists here — that is exactly the "selected the wrong
        // track after a re-probe" bug the scoping prevents.
        let stale = TrackId {
            catalog_generation: 6,
            local_id: 0,
        };
        assert_eq!(cat.locator(stale), None);

        // An unknown local id in the right generation is simply absent.
        assert_eq!(cat.locator(cat.track_id(99)), None);
    }

    #[test]
    fn av_option_locator_carries_its_group_and_plist() {
        let mut cat = MediaTrackCatalog::unavailable(1, MediaBackend::AVFoundation);
        cat.set_locator(
            0,
            TrackLocator::AvOption {
                group: AvGroup::Audible,
                property_list: vec![b'b', b'p', b'l', b'i', b's', b't'],
            },
        );
        match cat.locator(cat.track_id(0)) {
            Some(TrackLocator::AvOption {
                group,
                property_list,
            }) => {
                assert_eq!(*group, AvGroup::Audible);
                assert_eq!(property_list.len(), 6);
            }
            other => panic!("expected an AvOption locator, got {other:?}"),
        }
    }

    #[test]
    fn an_unavailable_catalog_asserts_nothing_about_either_kind() {
        let cat = MediaTrackCatalog::unavailable(1, MediaBackend::MediaFoundation);
        assert!(!cat.audio.is_known_empty());
        assert!(!cat.subtitles.is_known_empty());
        assert_eq!(cat.tracks().count(), 0);
        assert_eq!(cat.backend, MediaBackend::MediaFoundation);
    }

    #[test]
    fn tracks_iterates_audio_then_subtitles() {
        let cat = MediaTrackCatalog::new(
            1,
            MediaBackend::FFmpeg,
            TrackSet::complete(vec![track(0, TrackKind::Audio)]),
            TrackSet::complete(vec![
                track(1, TrackKind::Subtitle),
                track(2, TrackKind::Subtitle),
            ]),
        );
        let kinds: Vec<TrackKind> = cat.tracks().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![TrackKind::Audio, TrackKind::Subtitle, TrackKind::Subtitle]
        );
    }

    /// Catalogs compare structurally, so tests can assert on a whole probe result
    /// without ad-hoc projections.
    #[test]
    fn catalogs_compare_deterministically() {
        let build = || {
            MediaTrackCatalog::new(
                2,
                MediaBackend::FFmpeg,
                TrackSet::complete(vec![track(0, TrackKind::Audio)]),
                TrackSet::unavailable(),
            )
        };
        assert_eq!(build(), build());
        let mut other = build();
        other.generation = 3;
        assert_ne!(build(), other);
    }
}
