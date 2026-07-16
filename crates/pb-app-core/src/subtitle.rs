//! **Subtitle selection + style + placement** (task #90.3/#90.4): the pure decisions
//! behind the overlay — *which* track shows, *how* it looks, and *where* it goes.
//!
//! No renderer, no clock, no I/O. The rasterizer consumes [`SubtitleStyle`] and the
//! presenters consume [`place`]'s rect; both shells get identical numbers because the
//! numbers are computed once, here.

use std::time::Duration;

use pb_decode::tracks::{language_display, MediaTrack, MediaTrackCatalog, TrackId};
use serde::{Deserialize, Serialize};

/// What the user wants — **two questions, deliberately kept apart**.
///
/// `C` owns *whether* subtitles show. The picker owns *which*. Fusing them into one enum
/// (an `Off` variant sitting beside `Automatic` and `Track`) was a real defect, because it
/// left `C` no way to turn subtitles off without destroying the answer to the other
/// question: you picked Chinese on Ad Astra, pressed `C` twice, and got English back. The
/// two answers can only survive each other if they are stored separately.
///
/// Owner decisions, chosen for predictability:
///
/// - **`C` governs *dialogue* subtitles.** Forced signs are a separate layer — see
///   [`always_forced`](Self::always_forced).
/// - **`Automatic` means "the full dialogue track"**: the container's default, else any
///   non-forced track, else (only if nothing else exists) a forced one. It deliberately
///   does **not** follow the system language or OS accessibility settings: turning
///   subtitles on by itself is the kind of surprise that makes people hunt through
///   Settings. Nothing here can turn *dialogue* subtitles on — [`enabled`](Self::enabled)
///   is the user's, and only `C` and the picker set it.
///
/// > **Reversed 2026-07-16 (owner):** the 2026-07-14 rule was *"Off means off — forced
/// > subtitles do not leak through"*, on the reasoning that text appearing while the UI
/// > says off, with no way to get true silence, is worse. The owner reversed it — *"it
/// > should work the way every other movie-playing application works"* — and the original
/// > objection is answered by [`always_forced`](Self::always_forced) **being a setting**:
/// > the escape hatch exists, it is just not the default. See that field for the why.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubtitleSelection {
    /// `C`. Whether **dialogue** subtitles show. Persisted as `settings.subtitles` — an
    /// on/off preference is not a record of what you watched.
    ///
    /// Not the same question as "is anything on screen": with
    /// [`always_forced`](Self::always_forced) a film's forced signs show while this is
    /// `false`. That is the point — they answer different questions.
    pub enabled: bool,
    /// Show forced subtitles even when [`enabled`](Self::enabled) is `false` (task #99).
    /// Mirrors `settings.forced_subtitles`; default **on**.
    ///
    /// The forced flag means *"show this even if subtitles are off"* — it is how you read
    /// the Elvish in *Lord of the Rings*, which the director intends everyone to read. It
    /// is part of the film, not an aid you switch on, which is why Apple, mpv, VLC and
    /// every Blu-ray player show these without being asked.
    ///
    /// ⚠ **This is why [`automatic`] must NOT prefer forced tracks.** If the forced signs
    /// are already on screen passively, an `Automatic` that resolved to the forced track
    /// would make pressing `C` do **nothing visible** on exactly the films where forced
    /// tracks exist. The two rules only work as a pair.
    pub always_forced: bool,
    /// WHICH subtitles show, when [`enabled`](Self::enabled). Never expresses "off": that
    /// is the field above, and keeping them apart is the whole point.
    pub want: SubtitleWant,
    /// The intent behind the last track picked **by hand**, carried across files.
    ///
    /// A [`TrackId`] cannot survive a nav — it names exactly one file's catalog by
    /// construction, and must (sharing ids across files is what once showed Korean where
    /// Arabic was picked). But what you *meant* by choosing "Chinese · ASS" was **Chinese**,
    /// not *stream 5*. The index means nothing in the next film; the language means
    /// everything. So the id dies at the file boundary and this crosses it.
    ///
    /// RAM-only — never written to disk, so it is a preference rather than a viewing trace
    /// (privacy #2).
    pub preference: Option<SubtitlePreference>,
}

/// Which subtitles, when they are on.
///
/// There is deliberately no `Off` variant — that is [`SubtitleSelection::enabled`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubtitleWant {
    /// Let the app choose, per file.
    #[default]
    Automatic,
    /// Exactly this track. Bound to one file's catalog by the id's generation; on any
    /// *other* file it falls back to the preference chain rather than to nothing.
    Track(TrackId),
}

/// What the user last picked by hand, as an **intent** rather than a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitlePreference {
    /// The container's language tag, **verbatim** (`"cmn"`, `"zh-Hans"`, `"ara"`).
    ///
    /// Compared through [`language_display`], never by raw string equality: the *same file*
    /// reports `"en"` through AVFoundation and `"eng"` through FFmpeg, so a raw compare
    /// would fail to match a language against itself.
    pub language: String,
    /// Whether the picked track was SDH. Choosing "Arabic (SDH)" over plain Arabic is a
    /// deliberate accessibility choice, so it should survive to the next episode.
    pub hearing_impaired: bool,
    /// Whether the picked track was **forced**. Carried for the same reason as
    /// `hearing_impaired`, and it became load-bearing the moment forced tracks got a life
    /// of their own (task #99).
    ///
    /// Without it, a forced-English and a full-English track score **identically** here —
    /// same language, same SDH — so the tie broke on *track order*. Picking "English ·
    /// SubRip" on one film could silently land on "English · Forced" in the next, which
    /// reads as the subtitles having mysteriously stopped working.
    pub forced: bool,
}

/// One row of the track picker — a **command**, not the state.
///
/// `Off` is a row here even though the state expresses it as `enabled = false`, because the
/// playback bar's popover has no other off switch. The Playback menu, which has a real
/// toggle above its flyout, simply omits row 0 while keeping these indices. **One canonical
/// list, two presentations** — which is what lets a row cross the FFI as a bare index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleChoice {
    Off,
    Automatic,
    Track(TrackId),
}

impl SubtitleSelection {
    /// Subtitles off, nothing picked — the default.
    pub fn off() -> Self {
        Self::default()
    }

    /// Subtitles on, app-chosen: the state `C` produces when you have picked nothing.
    ///
    /// ⚠ Leaves [`always_forced`](Self::always_forced) at its `Default` (`false`) — it is a
    /// *setting*, not part of "what did the user pick", and only
    /// [`SubtitleEngine::from_settings`](crate::subtitle_engine::SubtitleEngine::from_settings)
    /// and `apply_settings` know its real value. So **never assign this over a live
    /// selection** in app code, or you silently switch the setting off; the live mutators
    /// ([`toggle`](Self::toggle) / [`apply`](Self::apply)) preserve it by construction,
    /// which is why they exist.
    pub fn automatic() -> Self {
        Self {
            enabled: true,
            want: SubtitleWant::Automatic,
            ..Self::default()
        }
    }

    /// Subtitles on, showing exactly `id`. (Carries no preference: only
    /// [`apply`](Self::apply) can build one, because only it has the catalog to read the
    /// track's language from.) Same `always_forced` caveat as [`automatic`](Self::automatic).
    pub fn track(id: TrackId) -> Self {
        Self {
            enabled: true,
            want: SubtitleWant::Track(id),
            ..Self::default()
        }
    }

    /// `C`. Flips on/off and **never touches `want` or `preference`** — that is the entire
    /// reason this type is a struct and not an enum. Toggling off and back on returns to the
    /// track you picked, not to whatever Automatic would have chosen.
    pub fn toggle(&mut self) {
        self.enabled = !self.enabled;
    }

    /// Apply a picker row.
    pub fn apply(&mut self, choice: SubtitleChoice, catalog: &MediaTrackCatalog) {
        match choice {
            // Off leaves `want` and `preference` untouched, so `C` (or the Off row again)
            // brings back exactly what you had.
            SubtitleChoice::Off => self.enabled = false,
            SubtitleChoice::Automatic => {
                self.enabled = true;
                self.want = SubtitleWant::Automatic;
                // Explicitly asking the app to choose retires the language you had pinned —
                // otherwise "Automatic" would go on obeying a preference you just cancelled,
                // and there would be no way to un-pin it.
                self.preference = None;
            }
            SubtitleChoice::Track(id) => {
                self.enabled = true;
                self.want = SubtitleWant::Track(id);
                // Remember the *intent*. A track with no language tag leaves nothing to
                // remember, and correctly clears any older preference: we cannot carry
                // "the untagged one" to another file.
                self.preference = catalog
                    .subtitles
                    .tracks
                    .iter()
                    .find(|t| t.id == id)
                    .and_then(preference_of);
            }
        }
    }

    /// The **dialogue** track this selection chooses for this file — the question the
    /// picker is asking. `None` = the user has dialogue subtitles off, or this file carries
    /// none; both are normal answers rather than failures.
    ///
    /// ⚠ **Not what is drawn** — that is [`resolve_display`](Self::resolve_display), which
    /// adds the passive forced layer. The split is deliberate: the picker's `Off` row must
    /// tick when the user has chosen off, and it would not if this function reported the
    /// forced signs showing underneath. One function per question.
    pub fn resolve<'a>(
        &self,
        catalog: &'a MediaTrackCatalog,
        audio_language: Option<&str>,
    ) -> Option<&'a MediaTrack> {
        if !self.enabled {
            return None; // no *dialogue*; forced signs are resolve_display's business
        }
        if let SubtitleWant::Track(id) = self.want {
            if let Some(t) = catalog
                .subtitles
                .tracks
                .iter()
                .find(|t| t.id == id)
                .filter(|t| renderable(t))
            {
                return Some(t);
            }
            // The id belongs to another file's catalog (it names exactly one, by design), or
            // to a track we cannot draw. Fall through to the chain below — **not** to
            // nothing: what you meant by picking Chinese was Chinese, and the next film
            // very likely has it.
        }
        automatic(catalog, audio_language, self.preference.as_ref())
    }

    /// What is **actually drawn** for this file: the chosen dialogue track if there is one,
    /// otherwise the passive forced layer. `None` = a genuinely clean picture.
    ///
    /// This is the whole of the 2026-07-16 model, and it is three lines because the two
    /// questions were kept apart:
    ///
    /// | `enabled` | `always_forced` | drawn |
    /// |---|---|---|
    /// | `false` | `true` | the film's forced signs, if any match the audio |
    /// | `false` | `false` | nothing — true silence, the escape hatch |
    /// | `true` | either | the dialogue track ([`resolve`](Self::resolve)) |
    ///
    /// Forced signs deliberately do **not** stack on top of a dialogue track: if you are
    /// already reading every line, the signs are in there too, and drawing both would
    /// double up.
    pub fn resolve_display<'a>(
        &self,
        catalog: &'a MediaTrackCatalog,
        audio_language: Option<&str>,
    ) -> Option<&'a MediaTrack> {
        self.resolve(catalog, audio_language).or_else(|| {
            self.always_forced
                .then(|| forced_for_audio(catalog, audio_language))?
        })
    }
}

/// The film's forced track for the language you are hearing, if it has one.
///
/// **An unknown audio language matches nothing**, deliberately: guessing would put French
/// signs over an English film. Forced subtitles only make sense *against* the audio they
/// were authored to annotate.
fn forced_for_audio<'a>(
    catalog: &'a MediaTrackCatalog,
    audio_language: Option<&str>,
) -> Option<&'a MediaTrack> {
    let audio = audio_language?;
    catalog
        .subtitles
        .tracks
        .iter()
        .filter(|t| renderable(t))
        .find(|t| t.flags.forced && same_language(t, audio))
}

/// The app's own choice of **dialogue** track, in preference order.
///
/// ⚠ **Nothing here prefers a forced track, and that is load-bearing** (task #99). Forced
/// signs show passively via [`SubtitleSelection::always_forced`], so an `Automatic` that
/// picked the forced track would make pressing `C` do nothing visible on precisely the
/// films that have one. `Automatic` answers "which track do I read", and a forced track is
/// not an answer to that — it is a handful of signs.
fn automatic<'a>(
    catalog: &'a MediaTrackCatalog,
    _audio_language: Option<&str>,
    preference: Option<&SubtitlePreference>,
) -> Option<&'a MediaTrack> {
    let renderable = || catalog.subtitles.tracks.iter().filter(|t| renderable(t));

    // 0. The language you last picked by hand. An explicit choice outranks every heuristic.
    if let Some(pref) = preference {
        if let Some(t) = best_for_preference(catalog, pref) {
            return Some(t);
        }
    }
    // 1. The container author's pick — unless it is the forced track, which answers a
    //    different question than "which dialogue track".
    // 2. Any full track.
    // 3. Only forced tracks exist. Something you can read beats nothing, and it is what you
    //    asked for by pressing `C`.
    renderable()
        .find(|t| t.flags.default && !t.flags.forced)
        .or_else(|| renderable().find(|t| !t.flags.forced))
        .or_else(|| renderable().next())
}

/// The renderable track best matching a remembered preference, or `None` when this film
/// simply does not carry that language.
///
/// **Ranked, not exact-matched.** A "Portuguese (BR)" pick must still land on a plain
/// "Portuguese" track next episode rather than falling through to English, and an SDH pick
/// should *prefer* an SDH track without *requiring* one.
///
/// The **forced** match outranks the SDH one: a forced/full mix-up changes *how much text
/// you see* (a few signs vs every line), which reads as the subtitles being broken, whereas
/// an SDH mix-up only adds or drops sound cues.
fn best_for_preference<'a>(
    catalog: &'a MediaTrackCatalog,
    pref: &SubtitlePreference,
) -> Option<&'a MediaTrack> {
    let mut best: Option<(&MediaTrack, u8)> = None;
    for t in catalog.subtitles.tracks.iter().filter(|t| renderable(t)) {
        let Some(rank) = language_rank(t, &pref.language) else {
            continue;
        };
        // Language dominates; forced then SDH break ties within one language. Without the
        // forced term, a forced-English and a full-English track score the same and the tie
        // breaks on *track order* — so "I picked full English" could land on forced English
        // in the next film.
        let score = rank * 4
            + u8::from(t.flags.forced == pref.forced) * 2
            + u8::from(t.flags.hearing_impaired == pref.hearing_impaired);
        if best.is_none_or(|(_, b)| score > b) {
            best = Some((t, score));
        }
    }
    best.map(|(t, _)| t)
}

/// How well a track's language matches a remembered tag: `2` = the same display name
/// ("Portuguese (BR)"), `1` = the same primary subtag ("Portuguese" against
/// "Portuguese (BR)"), `None` = a different language.
///
/// Both tiers compare **display names, not tags**, so `"pt-BR"` and `"por"` are one
/// language — the raw tags share no substring at all.
fn language_rank(track: &MediaTrack, want: &str) -> Option<u8> {
    let have = track.language.as_deref()?;
    if language_display(have).eq_ignore_ascii_case(&language_display(want)) {
        return Some(2);
    }
    if language_display(primary_subtag(have))
        .eq_ignore_ascii_case(&language_display(primary_subtag(want)))
    {
        return Some(1);
    }
    None
}

/// The primary subtag of a language tag: `"pt-BR"` → `"pt"`, `"por"` → `"por"`.
fn primary_subtag(tag: &str) -> &str {
    tag.split(['-', '_']).next().unwrap_or(tag)
}

/// What to remember about a track the user picked. `None` when it carries no language —
/// there is no intent to carry to another file.
fn preference_of(t: &MediaTrack) -> Option<SubtitlePreference> {
    Some(SubtitlePreference {
        language: t.language.clone()?,
        hearing_impaired: t.flags.hearing_impaired,
        forced: t.flags.forced,
    })
}

/// The picker's canonical rows: `Off`, `Automatic`, then every renderable track.
///
/// **A row's index is its index into this list** — that correspondence is what lets a
/// selection cross the FFI as a bare index, and it is why the popover, the menu flyout and
/// `Shift+C` cannot drift apart. Surfaces may *hide* rows (the menu omits `Off`, which its
/// own toggle owns) but must never *renumber* them.
///
/// Only renderable tracks appear: a PGS track we cannot draw would be an offer we can't
/// honour. When there are none, the list is just `[Off]` — `Automatic` with nothing to
/// resolve to would be a row that does nothing.
pub fn picker_choices(catalog: &MediaTrackCatalog) -> Vec<SubtitleChoice> {
    let tracks: Vec<SubtitleChoice> = catalog
        .subtitles
        .tracks
        .iter()
        .filter(|t| renderable(t))
        .map(|t| SubtitleChoice::Track(t.id))
        .collect();
    if tracks.is_empty() {
        return vec![SubtitleChoice::Off];
    }
    std::iter::once(SubtitleChoice::Off)
        .chain(std::iter::once(SubtitleChoice::Automatic))
        .chain(tracks)
        .collect()
}

/// The track after the one showing, wrapping — `Shift+C`.
///
/// **Tracks only.** `Off` is `C`'s job now, and `Automatic` is not a step: it resolves to
/// one of these tracks anyway, so including it would show the same subtitles twice under
/// two names. `None` when the file has no renderable track — there is nothing to step to.
///
/// `showing` is the track *on screen*, not the mode, so `Shift+C` under `Automatic` advances
/// from what you can see rather than jumping back to it.
pub fn next_track(catalog: &MediaTrackCatalog, showing: Option<TrackId>) -> Option<TrackId> {
    let ids: Vec<TrackId> = catalog
        .subtitles
        .tracks
        .iter()
        .filter(|t| renderable(t))
        .map(|t| t.id)
        .collect();
    let here = showing.and_then(|id| ids.iter().position(|x| *x == id));
    match here {
        Some(i) => Some(ids[(i + 1) % ids.len()]),
        // Nothing on screen (off, or a stale pick): start at the first rather than nowhere.
        None => ids.first().copied(),
    }
}

/// The language of the audio the user is hearing — what [`automatic`]'s forced rule matches
/// against.
///
/// *Which* audio track is playing is #99's business (nothing switches them yet), so this
/// answers with the container author's default and falls back to the first. That is the
/// honest best answer rather than a placeholder: it is exactly right for the overwhelming
/// majority of files, which carry one audio track — and when it is wrong, the cost is that
/// the forced rule misses and we fall through to the ordinary chain, not that we show the
/// wrong language's signs.
pub fn audio_language_of(catalog: &MediaTrackCatalog) -> Option<&str> {
    catalog
        .audio
        .tracks
        .iter()
        .find(|t| t.flags.default)
        .or_else(|| catalog.audio.tracks.first())
        .and_then(|t| t.language.as_deref())
}

fn renderable(t: &MediaTrack) -> bool {
    t.capability.is_renderable_text()
}

/// Do the track and the audio speak the same language? Compared through the shared display
/// map, so `"en"` and `"eng"` — which different backends report for the *same file* — are
/// one language rather than two.
fn same_language(track: &MediaTrack, audio: &str) -> bool {
    match &track.language {
        Some(l) => language_display(l).eq_ignore_ascii_case(&language_display(audio)),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Style (#90.4)
// ---------------------------------------------------------------------------

/// A drop shadow behind the text.
///
/// `#[serde(default)]` + [`Default`] are **required**, not decoration: this rides inside
/// `settings.toml` as an optional table, and a human who writes `[subtitle_style.shadow]`
/// with only `blur_pct` in it must get a shadow, not a settings file that fails to parse
/// and silently resets everything else. Same rule as [`SubtitleStyle`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shadow {
    /// Offset as a **fraction of the font size** — see [`SubtitleStyle`]'s unit rule.
    /// `0.05` = a twentieth of the text's height, down-right at the default.
    pub dx_ratio: f32,
    pub dy_ratio: f32,
    /// Blur radius as a fraction of the font size.
    pub blur_ratio: f32,
    pub color: [u8; 4],
}

impl Default for Shadow {
    /// A soft drop shadow, straight down — owner-tuned (2026-07-15): blur 2 px, offset
    /// (0, 2) px on the [`REFERENCE_FONT_PX`] scale, 80% black.
    ///
    /// These are what the pane offers for a shadow you are **about to switch on**, and what
    /// a config that names the table without filling it in gets. The shipped default is no
    /// shadow at all (see [`SubtitleStyle::default`]) — the outline already carries
    /// legibility — so this is the "on" preset, not the "on by default" one. It has to look
    /// right immediately: a shadow that switches on invisible reads as a broken toggle.
    fn default() -> Self {
        Shadow {
            dx_ratio: 0.0,
            dy_ratio: 2.0 / REFERENCE_FONT_PX,
            blur_ratio: 2.0 / REFERENCE_FONT_PX,
            color: [0, 0, 0, 204], // 80%
        }
    }
}

/// The owner's eight customization axes (spec: 2026-07-14).
///
/// # The unit rule (read this before adding a field)
///
/// **Position and size are relative to the VIEWPORT. Decoration is relative to the TEXT.**
///
/// - `size_pct`, `vertical_offset_pct` → % of viewport height; `max_line_pct` → % of
///   viewport width. **Never points**: a subtitle sized in points reads differently on a
///   1× ultrawide and a 2× Studio, and sized against the viewport it looks the same
///   everywhere, which is the whole point of a legibility setting.
/// - `outline_ratio`, the shadow's offsets and blur, and the background's radius and
///   padding → **fractions of the font size**.
///
/// The second half is the correction the owner made on 2026-07-15: as viewport
/// percentages, an outline that looked right on 44 px text was a hairline on 100 px text
/// and a blob on 20 px, so *every* decoration had to be re-tuned each time the size moved
/// — "it looks wildly different depending on the size, so it's hard to dial in". Tied to
/// the text, they hold their proportions and the size slider becomes a size slider instead
/// of a re-tune-everything slider. (This is also what ASS does — `ScaledBorderAndShadow`.)
///
/// Persisted (this is appearance, not a viewing trace). The *track choice* is not — see
/// [`SubtitleMode::Track`].
///
/// ⚠ **`#[serde(default)]` is load-bearing.** This lives *by value* inside `Settings`, so
/// unlike `Option<WindowGeometry>` (which is absent-or-whole) a partial
/// `[subtitle_style]` table has to fill the gaps from [`Default`] — otherwise a
/// hand-edited config that sets only `size_pct` fails to parse, and `Settings::load`'s
/// "malformed → defaults" rule would throw away **every other setting in the file** to
/// punish one typo in this one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubtitleStyle {
    /// `None` = the system sans. A name the platform doesn't have falls back to it.
    pub font_family: Option<String>,
    /// % of viewport height.
    pub size_pct: f32,
    /// RGBA — the alpha is the opacity the pane's slider drives.
    pub color: [u8; 4],
    /// Outline thickness as a **fraction of the font size**; 0 = off.
    ///
    /// A true circular dilate of the glyph coverage — mathematically the outer half of a
    /// stroke — **not** the HUD's 8-way offset halo, which leaves gaps on diagonals and
    /// looks chewed at subtitle sizes.
    pub outline_ratio: f32,
    pub outline_color: [u8; 4],
    pub shadow: Option<Shadow>,
    /// Alpha 0 = no background. The alpha IS the on/off — one control, so there is no
    /// toggle that can disagree with the colour it guards.
    pub background: [u8; 4],
    /// Corner radius as a fraction of the font size. Only meaningful with a background.
    ///
    /// Not in the Settings pane: the owner's call (2026-07-15) is that a good rounded
    /// corner is a *look*, not a preference — "make it look modern, not like 1988-era
    /// closed captioning". Tied to the font size it stays right at every text size, so
    /// there is nothing left to tune. Still config-editable for anyone who insists.
    pub background_radius_ratio: f32,
    /// Padding around the text as a fraction of the font size. Also not in the pane, for
    /// the same reason.
    pub background_pad_ratio: f32,
    /// **Signed**, from the *video's bottom edge*, in % of viewport height. `0` = the
    /// block's bottom sits on the edge; `> 0` = up into the picture; `< 0` = **down into
    /// the letterbox**.
    ///
    /// It positions the block's **bottom edge**, so a negative offset only clears the
    /// picture entirely once it exceeds the block's own height — below that the text
    /// straddles the edge, which is itself a reasonable place to put it. See [`place`].
    pub vertical_offset_pct: f32,
    /// Max line width as a % of viewport **width**.
    pub max_line_pct: f32,
    /// Line height as a multiple of the font size.
    pub line_spacing: f32,
    /// **Master opacity, 0..=1** — the whole subtitle, faded as one object.
    ///
    /// ⚠ Deliberately NOT `color`'s alpha, which is the wrong tool for the job: fading the
    /// glyphs alone makes them translucent *onto their own outline*, which shows straight
    /// through, so the subtitle never actually gets closer to the picture — it just goes
    /// muddy (owner, 2026-07-15). This is applied to the finished composite instead. The
    /// per-element alphas remain, and remain useful: they balance the parts against each
    /// other, this balances the whole against the film.
    pub opacity: f32,
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        SubtitleStyle {
            font_family: None,
            // 4% of a 1080p height ≈ 43 px. Owner-tuned against the live preview
            // (2026-07-15); the earlier 4.4% read as "slightly large".
            size_pct: 0.04,
            color: [255, 255, 255, 255],
            // 2 px on the reference scale — owner-tuned. Written as the division so it
            // stays tied to the number they actually chose rather than to a ratio nobody
            // could check.
            outline_ratio: 2.0 / REFERENCE_FONT_PX,
            outline_color: [0, 0, 0, 255],
            shadow: None,
            // Off by default: the outline already carries legibility, and a box behind
            // every line is a lot of ink over the picture.
            background: [0, 0, 0, 0],
            // Not user-facing — a good modern look, tied to the font so it stays right at
            // every size. Radius ≈ 0.22 of a line reads as a soft pill rather than either
            // a 1988 caption block or a lozenge.
            background_radius_ratio: 0.22,
            background_pad_ratio: 0.28,
            // Just inside the picture's bottom edge — the classic position. 7%, not the
            // textbook 5%: it lifts the text clear of the info line and the scrubber, which
            // live in the same strip (owner, 2026-07-15).
            vertical_offset_pct: 0.07,
            opacity: 1.0,
            max_line_pct: 0.9,
            line_spacing: 1.2,
        }
    }
}

// ---------------------------------------------------------------------------
// The bounds the Settings pane and the clamp share
// ---------------------------------------------------------------------------
//
// One definition each, so a slider can never offer a value the clamp will quietly take
// back — a control that snaps when you let go is worse than one that never went there.

/// 17% of viewport height. Past this a line fits about two words (owner, 2026-07-15).
pub const MAX_SIZE_PCT: f32 = 0.17;

/// The font size every decoration's **px** readout is calibrated against: a **fixed
/// reference** of 47.5 px, which is a 4.4%-of-1080p subtitle.
///
/// Storage is a fraction of the *actual* font size, so decoration holds its proportions as
/// the text resizes — that is the whole point of the unit rule. This constant only turns
/// that fraction into the human number a slider shows, because "outline 0.06" is not a
/// quantity anyone can picture and "2 px" is. The px shown is therefore a px-*equivalent*,
/// and that equivalent — not any particular measurement — is the stable thing the user is
/// choosing. A live readout against the real font size would make the handle jump every
/// time the size moved, which is the exact confusion this removes.
///
/// ⚠ **It is a constant, not a function of the default size, and must stay one.** It began
/// as `0.044 × 1080` back when 4.4% was the default, and the default has since moved to 4%
/// — deliberately leaving this behind. If it tracked the default instead, changing that
/// default would silently renumber every decoration slider *and re-label every user's
/// already-saved settings*: an outline someone chose as "2 px" would start calling itself
/// 1.8 px, having not changed at all. The scale has to be an anchor or it is not a scale.
pub const REFERENCE_FONT_PX: f32 = 47.5;

/// A font-relative ratio → the px number a slider shows. See [`REFERENCE_FONT_PX`].
pub fn ratio_to_px(ratio: f32) -> f32 {
    ratio * REFERENCE_FONT_PX
}

/// ...and back.
pub fn px_to_ratio(px: f32) -> f32 {
    px / REFERENCE_FONT_PX
}

/// ~4.7 px on default-size text. Past this the outline eats the letterforms.
pub const MAX_OUTLINE_RATIO: f32 = 0.10;

/// ~10 px on default-size text (owner, 2026-07-15). Past this a shadow is a smudge rather
/// than depth — the original 5%-of-viewport ceiling could fill the screen.
///
/// Derived from the px the owner asked for, like [`MAX_SHADOW_OFFSET_RATIO`], so the cap
/// keeps meaning "10 px worth" at every text size instead of 10 literal pixels that would
/// swamp small text and vanish on large.
pub const MAX_SHADOW_BLUR_RATIO: f32 = 10.0 / REFERENCE_FONT_PX;

/// ~5 px on default-size text (owner, 2026-07-15). Past this the shadow stops reading as
/// depth and starts reading as a second, broken copy of the text.
///
/// Expressed as the ratio it really is, derived from the px the owner asked for — so the
/// two never drift, and so the cap keeps meaning "5 px worth" at every text size rather
/// than 5 literal pixels that would look enormous on small text and invisible on large.
pub const MAX_SHADOW_OFFSET_RATIO: f32 = 5.0 / REFERENCE_FONT_PX;

/// Above ~2, lines read as unrelated rather than as one cue.
pub const MAX_LINE_SPACING: f32 = 2.0;

/// The fonts the Settings picker offers, in order. `None`/empty = the system sans.
///
/// A **curated shortlist, deliberately not an enumeration** (owner call, 2026-07-15).
/// cosmic-text's fontdb finds ~1114 faces on a real machine, the overwhelming majority of
/// which are unusable as subtitles (symbol, emoji, and single-script faces), and a list
/// that long needs a search field and an indexed-accessor pull across the FFI to be usable
/// at all. These are the faces people actually pick, they ship on both macOS and Windows,
/// and every one is legible at speed against moving pictures.
///
/// This costs nothing later: the stored value is a font *name*, so growing this into a
/// full picker never invalidates a saved setting. A name the platform lacks falls back to
/// the system sans (cosmic-text's own rule), so the list being wrong on some machine
/// degrades to "the default font" rather than to no subtitles.
pub const FONT_CHOICES: &[&str] = &[
    "Helvetica",
    "Arial",
    "Verdana",
    "Tahoma",
    "Trebuchet MS",
    "Georgia",
    "Times New Roman",
    "Courier New",
];

impl SubtitleStyle {
    /// The font to shape with: the user's choice, or the system sans.
    ///
    /// Empty-string-as-none matters because the FFI cannot carry `Option<String>` — the
    /// Swift side sends `""` for "System", and that must mean the same thing as a config
    /// with no `font_family` key at all.
    pub fn font(&self) -> Option<&str> {
        self.font_family.as_deref().filter(|f| !f.trim().is_empty())
    }

    /// Clamp every field into a sane range, in place. See [`Self::clamped`].
    pub fn clamp(&mut self) {
        let c = std::mem::take(self).clamped();
        *self = c;
    }

    /// Clamp every field into a sane range. Settings come from a TOML file a human can
    /// edit, so "size 40" (they meant points) must produce a large subtitle, not a
    /// full-screen wall that hides the film with no way back.
    ///
    /// Non-finite is handled before the clamp: `f32::clamp` **panics** on a NaN bound and
    /// a NaN input passes straight through it, so a `size_pct = nan` in a config would
    /// otherwise reach the rasterizer. TOML can hold `nan`.
    pub fn clamped(mut self) -> Self {
        let d = SubtitleStyle::default();
        for (v, dv) in [
            (&mut self.size_pct, d.size_pct),
            (&mut self.outline_ratio, d.outline_ratio),
            (&mut self.background_radius_ratio, d.background_radius_ratio),
            (&mut self.background_pad_ratio, d.background_pad_ratio),
            (&mut self.vertical_offset_pct, d.vertical_offset_pct),
            (&mut self.max_line_pct, d.max_line_pct),
            (&mut self.line_spacing, d.line_spacing),
            (&mut self.opacity, d.opacity),
        ] {
            if !v.is_finite() {
                *v = dv;
            }
        }
        if let Some(sh) = &mut self.shadow {
            let ds = Shadow::default();
            for (v, dv) in [
                (&mut sh.dx_ratio, ds.dx_ratio),
                (&mut sh.dy_ratio, ds.dy_ratio),
                (&mut sh.blur_ratio, ds.blur_ratio),
            ] {
                if !v.is_finite() {
                    *v = dv;
                }
            }
        }
        // 17% of the viewport is already ~2 words a line (owner, 2026-07-15: "anything
        // bigger than that is comically massive"). The old 25% ceiling was reachable only
        // by a hand-edited config and was never a size anyone wanted.
        self.size_pct = self.size_pct.clamp(0.01, MAX_SIZE_PCT);
        // ~4.7 px on default-size text: past this an outline stops reading as an outline
        // and starts eating the letterforms.
        self.outline_ratio = self.outline_ratio.clamp(0.0, MAX_OUTLINE_RATIO);
        self.background_radius_ratio = self.background_radius_ratio.clamp(0.0, 1.0);
        self.background_pad_ratio = self.background_pad_ratio.clamp(0.0, 1.0);
        // Generous but bounded: far enough up to sit mid-screen, far enough down to reach
        // the bar on any letterbox.
        self.vertical_offset_pct = self.vertical_offset_pct.clamp(-0.5, 0.9);
        self.max_line_pct = self.max_line_pct.clamp(0.2, 1.0);
        // Above ~2 the lines read as unrelated rather than as one cue (owner: "much above
        // 2 looks pretty silly").
        self.line_spacing = self.line_spacing.clamp(0.8, MAX_LINE_SPACING);
        self.opacity = self.opacity.clamp(0.0, 1.0);
        if let Some(s) = &mut self.shadow {
            s.dx_ratio = s
                .dx_ratio
                .clamp(-MAX_SHADOW_OFFSET_RATIO, MAX_SHADOW_OFFSET_RATIO);
            s.dy_ratio = s
                .dy_ratio
                .clamp(-MAX_SHADOW_OFFSET_RATIO, MAX_SHADOW_OFFSET_RATIO);
            s.blur_ratio = s.blur_ratio.clamp(0.0, MAX_SHADOW_BLUR_RATIO);
        }
        self
    }

    /// Font size in **physical pixels** for this viewport.
    ///
    /// ⚠ Physical, not logical: rasterizing at a logical size and letting the layer scale
    /// it up is what makes text blurry, and this project's known sharp edge is exactly
    /// that (mixed 1×/2× displays — dragging the window between them must re-rasterize).
    pub fn size_px(&self, viewport_h_px: f32) -> f32 {
        (self.size_pct * viewport_h_px).max(1.0)
    }
}

// ---------------------------------------------------------------------------
// Placement (#90.4's headline feature)
// ---------------------------------------------------------------------------

/// A rectangle in physical pixels, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

/// Where the rasterized subtitle block goes, in physical pixels.
///
/// **Vertical anchors to the video's bottom edge; horizontal centers on the viewport.**
/// Both are the useful choice rather than the consistent one: anchoring vertically to the
/// video is what lets the text track the picture across clips of different aspect *and*
/// reach the letterbox, while centering horizontally on the viewport keeps the text on
/// screen when the picture is zoomed or panned off-center.
///
/// The signed offset is the feature almost no player gets right. Players pick one anchor
/// and lose: anchor to the video and you clamp at the edge (you can never reach the bar);
/// anchor to the window and the text drifts relative to the picture every time a clip's
/// aspect changes. Signed-from-the-edge does both.
///
/// `controls_h` > 0 lifts the block above the playback controls — including one parked in
/// the bottom bar. The controls auto-hide, so the lift is transient.
///
/// Everything is clamped into the viewport, so a negative offset with no letterbox (Fill,
/// zoom, a clip matching the display aspect) lands at the picture's bottom rather than
/// off-screen. The setting never becomes invalid; it just runs out of room.
/// `block` is the **bitmap's** size and `block_pad` its symmetric inset to the text's ink
/// ([`pb_hud::subtitle::SubtitleBitmap::pad`]).
///
/// ⚠ **Everything below positions the TEXT, then converts back to a bitmap origin at the
/// very end.** The bitmap is grown to hold the outline, shadow, and background, so its
/// size is a function of the *decoration* — anchoring it directly meant switching on a
/// drop shadow visibly shoved the subtitles upward (owner, 2026-07-15). The text's own box
/// is the only stable thing to hang position off.
pub fn place(
    viewport: (f32, f32),
    video: Rect,
    block: (f32, f32),
    block_pad: f32,
    style: &SubtitleStyle,
    controls_h: f32,
) -> Rect {
    let (vw, vh) = viewport;
    let (bw, bh) = block;
    // The text's own box inside the bitmap. Everything from here on is about this.
    let pad = block_pad.max(0.0).min(bh / 2.0).min(bw / 2.0);
    let text_h = (bh - pad * 2.0).max(0.0);

    // Positive offset = up into the picture, so it subtracts.
    let off = style.vertical_offset_pct * vh;
    let mut bottom = video.bottom() - off;

    // Zoomed (or Crop-to-Fill), the video's bottom edge is off-screen — so "6% up from the
    // bottom of the picture" would pin the block against the window edge. Hold the same gap
    // from the screen edge instead: the offset is a *legibility margin*, and it should mean
    // the same thing whether the nearer edge is the video's or the window's.
    //
    // Only for a positive offset: a negative one deliberately parks the block BELOW the
    // video in the letterbox, and must not be pushed back up by its own value.
    if off > 0.0 {
        bottom = bottom.min(vh - off);
    }

    // Lift above the controls while they're on screen.
    if controls_h > 0.0 {
        bottom = bottom.min(vh - controls_h);
    }
    // On screen, always: never past the bottom, never pushed off the top by a tall block.
    // `max` last so a block taller than the viewport still starts at the top rather than
    // resolving to a negative y. Both bounds are the TEXT's, so a big soft shadow can
    // still bleed off-screen — which is right: clamping the bitmap would let a shadow
    // nobody can see push the words nobody can miss.
    bottom = bottom.min(vh).max(text_h);

    Rect {
        // The text is centred in the bitmap (the pad is symmetric), so centring the
        // bitmap centres the text — no pad term needed here.
        x: ((vw - bw) / 2.0).max(0.0),
        // Back out to the bitmap's origin: the text's bottom sits `pad` above the
        // bitmap's.
        y: bottom - text_h - pad,
        w: bw,
        h: bh,
    }
}

/// How long a cue transition may take before it reads as late. The plan's bar: one frame
/// or 50 ms, whichever is looser — a subtitle appearing a frame late is invisible, a
/// tenth of a second late is not.
pub const TRANSITION_BUDGET: Duration = Duration::from_millis(50);

impl SubtitleStyle {
    /// Resolve this style against a viewport into the rasterizer's **physical-pixel**
    /// params.
    ///
    /// This conversion is the *only* place percentages become pixels, and it exists
    /// because `pb-hud` deliberately cannot see a viewport: px is the only unit that
    /// crosses that boundary, which makes "rasterized at the wrong scale" unrepresentable
    /// rather than merely discouraged.
    ///
    /// `viewport` is in physical px. Because `size_px` and friends land in the
    /// rasterizer's cache key, a backing-scale change (1× ↔ 2×) invalidates the cached
    /// bitmap for free — no shell has to remember to.
    pub fn to_params(&self, viewport: (f32, f32)) -> pb_hud::subtitle::SubtitleParams {
        let (vw, vh) = viewport;
        // The unit rule, in one line: size comes from the viewport, decoration comes from
        // the size. Everything below that multiplies by `font` instead of `vh` is a
        // decoration holding its proportions as the text resizes.
        let font = self.size_px(vh);
        pb_hud::subtitle::SubtitleParams {
            // `font()`, not the raw field: `""` (what the Settings FFI sends for "System",
            // since it cannot carry an `Option<String>`) must mean the system font, not a
            // hunt for a face literally named "".
            font_family: self.font().map(str::to_string),
            size_px: font,
            color: self.color,
            outline_px: (self.outline_ratio * font).max(0.0),
            outline_color: self.outline_color,
            shadow: self.shadow.map(|s| pb_hud::subtitle::ShadowParams {
                dx: s.dx_ratio * font,
                dy: s.dy_ratio * font,
                blur: (s.blur_ratio * font).max(0.0),
                color: s.color,
            }),
            background: self.background,
            background_radius_px: (self.background_radius_ratio * font).max(0.0),
            background_pad_px: (self.background_pad_ratio * font).max(0.0),
            // Width, not height — a max line length is about how far across the screen
            // the text runs. The one decoration that is genuinely about the viewport:
            // "don't run more than 90% across the screen" is a screen fact.
            max_line_px: (self.max_line_pct * vw).max(1.0),
            line_spacing: self.line_spacing,
            opacity: self.opacity,
        }
    }

    /// The outline as the **px number the Settings slider shows**, and back.
    ///
    /// See [`OUTLINE_PX_AT_DEFAULT_SIZE`] for why this indirection exists: storage scales
    /// with the text (so the look is stable), but "0.06" is not a quantity anyone can
    /// picture and "3 px" is.
    pub fn outline_px_scale(&self) -> f32 {
        ratio_to_px(self.outline_ratio)
    }

    pub fn set_outline_px_scale(&mut self, px: f32) {
        self.outline_ratio = px_to_ratio(px).clamp(0.0, MAX_OUTLINE_RATIO);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb_decode::tracks::{MediaBackend, TrackCapability, TrackFlags, TrackKind, TrackSet};

    fn sub(local_id: u64, lang: Option<&str>, forced: bool, cap: TrackCapability) -> MediaTrack {
        MediaTrack {
            id: TrackId {
                catalog_generation: 1,
                local_id,
            },
            kind: TrackKind::Subtitle,
            language: lang.map(str::to_string),
            title: None,
            codec_raw: "subrip".into(),
            codec: "SubRip".into(),
            capability: cap,
            flags: TrackFlags {
                forced,
                ..TrackFlags::none()
            },
            audio: None,
            external: false,
        }
    }

    fn catalog(tracks: Vec<MediaTrack>) -> MediaTrackCatalog {
        MediaTrackCatalog::new(
            1,
            MediaBackend::FFmpeg,
            TrackSet::complete(vec![]),
            TrackSet::complete(tracks),
        )
    }

    // -- selection ----------------------------------------------------------

    /// Owner decision: Off means off. A forced track does NOT leak through.
    #[test]
    fn off_shows_nothing_not_even_forced() {
        let c = catalog(vec![sub(
            0,
            Some("en"),
            true,
            TrackCapability::SupportedText,
        )]);
        assert!(SubtitleSelection::off().resolve(&c, Some("en")).is_none());
    }

    /// Owner decision (2026-07-16, reversing 2026-07-14): **Automatic = the full dialogue
    /// track**, never the forced one.
    ///
    /// This is only safe because forced signs show *passively* now (`always_forced`). If
    /// Automatic still preferred forced, pressing `C` on a film with a forced track would
    /// change **nothing on screen** — you were already seeing those signs. The two rules
    /// only work as a pair.
    #[test]
    fn automatic_picks_the_dialogue_track_never_the_forced_one() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText), // full English ✓
            sub(1, Some("fr"), true, TrackCapability::SupportedText),  // French forced
            sub(2, Some("en"), true, TrackCapability::SupportedText),  // English forced
        ]);
        let got = SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .expect("match");
        assert_eq!(
            got.id.local_id, 0,
            "you pressed C to READ — the English forced track is a handful of signs"
        );
    }

    /// …but a forced track beats nothing. If it is all the film has, `C` shows it: the user
    /// asked, and a toast saying "Subtitles on" over a blank screen is the worse answer.
    #[test]
    fn automatic_falls_back_to_a_forced_track_when_it_is_the_only_one() {
        let c = catalog(vec![sub(
            0,
            Some("en"),
            true,
            TrackCapability::SupportedText,
        )]);
        let got = SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .expect("something to read beats nothing");
        assert_eq!(got.id.local_id, 0);
    }

    // ── The passive forced layer (task #99, owner 2026-07-16) ───────────
    //
    // "It should work the way every other movie-playing application works": forced signs
    // are part of the film, so they show without being asked for. `resolve` answers "which
    // dialogue track" (what the picker asks); `resolve_display` answers "what is drawn".

    /// The whole model, as one table — the four states the owner signed off.
    #[test]
    fn the_forced_layer_shows_only_when_dialogue_subtitles_are_off() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText), // full English
            sub(1, Some("en"), true, TrackCapability::SupportedText),  // English forced
        ]);
        let forced_on = |enabled| SubtitleSelection {
            enabled,
            always_forced: true,
            ..Default::default()
        };

        // Off + setting on → the film's forced signs. THIS is the reversal of "Off means
        // off": you never asked, and you see them anyway, because that is what the flag
        // means and what every other player does.
        let got = forced_on(false)
            .resolve_display(&c, Some("en"))
            .expect("forced signs are part of the film");
        assert_eq!(got.id.local_id, 1);

        // …but the PICKER still reports off, which is why `resolve` and `resolve_display`
        // are two functions. Collapsing them would leave the Off row unticked while the
        // user is sitting on Off.
        assert!(
            forced_on(false).resolve(&c, Some("en")).is_none(),
            "the picker asks which DIALOGUE track — and the answer is none"
        );

        // On → the dialogue track. The signs are not drawn on top: if you are reading every
        // line, they are already in there, and both at once would double up.
        let got = forced_on(true)
            .resolve_display(&c, Some("en"))
            .expect("dialogue");
        assert_eq!(got.id.local_id, 0);

        // Setting off + off → a genuinely clean picture. The escape hatch that answers the
        // owner's original objection to reversing the rule.
        let silent = SubtitleSelection {
            enabled: false,
            always_forced: false,
            ..Default::default()
        };
        assert!(silent.resolve_display(&c, Some("en")).is_none());
    }

    /// The forced layer is matched **against the audio you are hearing**, because that is
    /// the only thing it annotates. Guessing would put French signs over an English film.
    #[test]
    fn the_forced_layer_needs_a_track_matching_the_audio() {
        let c = catalog(vec![sub(
            0,
            Some("fr"),
            true,
            TrackCapability::SupportedText,
        )]);
        let sel = SubtitleSelection {
            enabled: false,
            always_forced: true,
            ..Default::default()
        };
        // French forced signs over English audio: not this film's signs.
        assert!(sel.resolve_display(&c, Some("en")).is_none());
        // Unknown audio language matches nothing rather than guessing.
        assert!(sel.resolve_display(&c, None).is_none());
        // The film you are actually watching.
        assert!(sel.resolve_display(&c, Some("fr")).is_some());

        // A film with no forced track at all shows nothing — correct, not a failure.
        let plain = catalog(vec![sub(
            0,
            Some("en"),
            false,
            TrackCapability::SupportedText,
        )]);
        assert!(sel.resolve_display(&plain, Some("en")).is_none());
    }

    /// A forced track we cannot draw (PGS) is not an offer here either — the same rule that
    /// keeps it out of the picker. Otherwise "forced signs on" would silently mean nothing.
    #[test]
    fn the_forced_layer_skips_a_track_it_cannot_render() {
        let c = catalog(vec![sub(0, Some("en"), true, TrackCapability::Bitmap)]);
        let sel = SubtitleSelection {
            enabled: false,
            always_forced: true,
            ..Default::default()
        };
        assert!(sel.resolve_display(&c, Some("en")).is_none());
    }

    /// Automatic falls back rather than showing nothing — because the mode is only ever
    /// Automatic because the user pressed `C` and was told "Subtitles on".
    #[test]
    fn automatic_falls_back_when_no_forced_track_matches() {
        // A full English track is not forced — but it is what the user asked for.
        let c = catalog(vec![sub(
            0,
            Some("en"),
            false,
            TrackCapability::SupportedText,
        )]);
        let got = SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .expect("fallback");
        assert_eq!(got.id.local_id, 0);

        // French forced over English audio: not our film's signs, so rule 1 declines it —
        // but it is still the only thing we could show, and showing it beats a toast that
        // says "Subtitles on" over a blank screen.
        let c = catalog(vec![sub(
            0,
            Some("fr"),
            true,
            TrackCapability::SupportedText,
        )]);
        assert!(SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .is_some());
    }

    /// A **forced** track flagged `default` is still not a dialogue track. Some rips mark
    /// the forced track default, and honouring that literally would hand `C` a few signs
    /// while a full track sat right there.
    #[test]
    fn a_forced_track_marked_default_does_not_win_automatic() {
        let mut tracks = vec![
            sub(0, Some("en"), true, TrackCapability::SupportedText), // forced AND default
            sub(1, Some("en"), false, TrackCapability::SupportedText), // the full track ✓
        ];
        tracks[0].flags.default = true;
        let c = catalog(tracks);
        let got = SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .expect("match");
        assert_eq!(
            got.id.local_id, 1,
            "the author's default flag answers 'which track', not 'signs or dialogue'"
        );
    }

    /// Rule 2: absent a forced match, the container author's default track is the best
    /// guess available — better than "whichever stream happened to be first".
    #[test]
    fn automatic_prefers_the_containers_default_track() {
        let mut tracks = vec![
            sub(0, Some("fr"), false, TrackCapability::SupportedText),
            sub(1, Some("en"), false, TrackCapability::SupportedText),
        ];
        tracks[1].flags.default = true;
        let c = catalog(tracks);
        let got = SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .expect("default");
        assert_eq!(got.id.local_id, 1);
    }

    /// The fallback never reaches for something it cannot draw. A PGS-only clip shows
    /// nothing — which is the honest answer, not a bug.
    #[test]
    fn automatic_never_falls_back_onto_an_unrenderable_track() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::Bitmap),
            sub(1, Some("en"), true, TrackCapability::Bitmap),
        ]);
        assert!(SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .is_none());
    }

    /// The backends disagree on tag form for the same file ("en" vs "eng"); Automatic must
    /// not depend on which one probed.
    #[test]
    fn automatic_matches_across_tag_forms() {
        let c = catalog(vec![sub(
            0,
            Some("eng"),
            true,
            TrackCapability::SupportedText,
        )]);
        assert!(SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .is_some());
        let c = catalog(vec![sub(
            0,
            Some("en"),
            true,
            TrackCapability::SupportedText,
        )]);
        assert!(SubtitleSelection::automatic()
            .resolve(&c, Some("eng"))
            .is_some());
    }

    /// The language guard survives the fallback chain: an unknown audio language must
    /// never *match* a forced track by guessing. What it may do is fall through to the
    /// ordinary fallbacks — which is a different, weaker claim than "shows nothing", and
    /// the distinction is the whole point of rule 1.
    #[test]
    fn an_unknown_audio_language_never_matches_a_forced_track() {
        // Two forced tracks in different languages, audio language unknown. Rule 1 must
        // decline (it cannot know which film's signs these are) — so the pick comes from
        // the fallback, i.e. the first renderable, NOT from a language guess.
        let c = catalog(vec![
            sub(0, Some("fr"), true, TrackCapability::SupportedText),
            sub(1, Some("en"), true, TrackCapability::SupportedText),
        ]);
        let got = SubtitleSelection::automatic()
            .resolve(&c, None)
            .expect("fallback");
        assert_eq!(
            got.id.local_id, 0,
            "the fallback's order, not a language guess"
        );

        // A forced track with no language of its own cannot be matched either.
        let mut tracks = vec![
            sub(0, None, true, TrackCapability::SupportedText),
            sub(1, Some("en"), false, TrackCapability::SupportedText),
        ];
        tracks[1].flags.default = true;
        let c = catalog(tracks);
        let got = SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .expect("fallback");
        assert_eq!(
            got.id.local_id, 1,
            "rule 1 can't match, so the default wins"
        );
    }

    // -- decoration must not move the text (owner, 2026-07-15) ---------------

    /// ⚠ THE regression. Switching on a drop shadow **visibly shoved the subtitles
    /// upward**, because the rasterizer grows the bitmap to hold the shadow and `place`
    /// anchored the bitmap's bottom edge. Decoration is not position; it must never move
    /// the words.
    #[test]
    fn a_drop_shadow_does_not_move_the_text() {
        let (vp, video) = letterboxed();
        let s = SubtitleStyle::default();
        // Same TEXT (300x40), two bitmaps: one bare, one grown 12 px all round by a
        // shadow. The text's bottom must land in the identical place.
        let bare = place(vp, video, (300.0, 40.0), 0.0, &s, 0.0);
        let shadowed = place(vp, video, (324.0, 64.0), 12.0, &s, 0.0);
        assert_eq!(
            bare.bottom(),
            shadowed.bottom() - 12.0,
            "the TEXT's bottom edge must not move when a shadow is added"
        );
    }

    /// The same rule, stated as the property that matters: for a fixed text box, growing
    /// the decoration in every direction leaves the text where it was.
    #[test]
    fn growing_the_decoration_leaves_the_text_where_it_was() {
        let (vp, video) = letterboxed();
        let s = SubtitleStyle::default();
        // The text's bottom = the rect's bottom minus the pad below it.
        let text_bottom = |pad: f32| {
            place(
                vp,
                video,
                (300.0 + pad * 2.0, 40.0 + pad * 2.0),
                pad,
                &s,
                0.0,
            )
            .bottom()
                - pad
        };
        let want = text_bottom(0.0);
        for pad in [1.0, 4.0, 12.0, 40.0] {
            assert_eq!(text_bottom(pad), want, "pad {pad} moved the text");
        }
    }

    /// ...and horizontally too: the pad is symmetric, so the text stays centred.
    #[test]
    fn decoration_does_not_move_the_text_horizontally() {
        let (vp, video) = letterboxed();
        let s = SubtitleStyle::default();
        let text_centre = |pad: f32| {
            let r = place(
                vp,
                video,
                (300.0 + pad * 2.0, 40.0 + pad * 2.0),
                pad,
                &s,
                0.0,
            );
            r.x + r.w / 2.0
        };
        let want = text_centre(0.0);
        for pad in [1.0, 12.0, 40.0] {
            assert_eq!(text_centre(pad), want, "pad {pad} moved the text sideways");
        }
    }

    // -- the unit rule (owner, 2026-07-15) ---------------------------------

    /// The correction that made the size slider usable: decoration holds its proportion
    /// as the text resizes. Double the size, and the outline/shadow/radius double with it
    /// — so you tune the look once instead of re-tuning it at every size.
    #[test]
    fn decoration_scales_with_the_text_not_the_viewport() {
        let vp = (1920.0, 1080.0);
        let small = SubtitleStyle {
            size_pct: 0.04,
            outline_ratio: 0.06,
            shadow: Some(Shadow::default()),
            background: [0, 0, 0, 200],
            ..Default::default()
        };
        let big = SubtitleStyle {
            size_pct: 0.08,
            ..small.clone()
        };
        let (a, b) = (small.to_params(vp), big.to_params(vp));
        assert_eq!(b.size_px, a.size_px * 2.0);
        assert!((b.outline_px - a.outline_px * 2.0).abs() < 0.01);
        assert!((b.background_radius_px - a.background_radius_px * 2.0).abs() < 0.01);
        assert!((b.background_pad_px - a.background_pad_px * 2.0).abs() < 0.01);
        let (sa, sb) = (a.shadow.unwrap(), b.shadow.unwrap());
        assert!((sb.blur - sa.blur * 2.0).abs() < 0.01);
        assert!((sb.dy - sa.dy * 2.0).abs() < 0.01);

        // ...and the outline's RATIO to the text is what stayed constant. That ratio is
        // the thing the user actually chose.
        assert!(((a.outline_px / a.size_px) - (b.outline_px / b.size_px)).abs() < 1e-4);
    }

    /// The px readout the slider shows is a stable number: it does not move when the text
    /// resizes, because it names the ratio, not a measurement.
    #[test]
    fn the_outline_px_scale_round_trips_and_is_size_independent() {
        let mut s = SubtitleStyle::default();
        for px in [0.0f32, 1.0, 2.5, 4.0] {
            s.set_outline_px_scale(px);
            assert!(
                (s.outline_px_scale() - px).abs() < 0.001,
                "{px} px round-trips"
            );
        }
        s.set_outline_px_scale(3.0);
        let at_default = s.outline_px_scale();
        s.size_pct = 0.12; // a much bigger subtitle
        assert_eq!(s.outline_px_scale(), at_default, "the slider must not jump");
    }

    /// The slider's ceiling and the clamp's ceiling are the same number, so a control can
    /// never offer a value the clamp quietly takes back.
    #[test]
    fn the_px_scale_cannot_exceed_the_clamp() {
        let mut s = SubtitleStyle::default();
        s.set_outline_px_scale(999.0);
        assert_eq!(s.outline_ratio, MAX_OUTLINE_RATIO);
        assert_eq!(s.clone().clamped().outline_ratio, s.outline_ratio);
    }

    #[test]
    fn the_shadow_caps_are_the_px_the_owner_asked_for() {
        // Owner, 2026-07-15: offsets ±5 px, blur 10 px — on the reference scale, so they
        // keep meaning "5 px worth" at every text size.
        assert!((ratio_to_px(MAX_SHADOW_OFFSET_RATIO) - 5.0).abs() < 0.01);
        assert!((ratio_to_px(MAX_SHADOW_BLUR_RATIO) - 10.0).abs() < 0.01);
        // ...and the outline slider's 0..4 fits inside its clamp with room to spare.
        assert!(ratio_to_px(MAX_OUTLINE_RATIO) >= 4.0);
    }

    /// The shipped defaults, owner-tuned against the live preview (2026-07-15). Pinned in
    /// the units the owner actually chose them in — a ratio drifting silently is exactly
    /// what a reader of `SubtitleStyle::default()` cannot notice.
    #[test]
    fn the_shipped_defaults_are_the_owner_tuned_numbers() {
        let d = SubtitleStyle::default();
        assert_eq!(d.size_pct, 0.04, "4% of viewport height");
        assert!(
            (d.outline_px_scale() - 2.0).abs() < 0.001,
            "a 2.00 px outline, not {}",
            d.outline_px_scale()
        );
        assert_eq!(d.outline_color[3], 255, "solid black — opacity stays 100%");
        assert_eq!(
            d.vertical_offset_pct, 0.07,
            "7%: clear of the info line and the scrubber"
        );
        // And the defaults must survive their own clamp — a default the clamp would move
        // is a default nobody actually ships.
        assert_eq!(d.clone().clamped(), d);
    }

    /// The shadow preset a user gets when they switch the toggle on — owner-tuned
    /// (2026-07-15). It is NOT on by default; this is what "on" looks like, and it has to
    /// look right immediately, because a shadow that switches on invisible reads as a
    /// broken toggle.
    #[test]
    fn the_shadow_preset_is_visible_the_moment_it_is_switched_on() {
        let sh = Shadow::default();
        assert!(SubtitleStyle::default().shadow.is_none(), "off by default");
        assert!(
            (ratio_to_px(sh.blur_ratio) - 2.0).abs() < 0.001,
            "2 px blur"
        );
        assert_eq!(sh.dx_ratio, 0.0, "straight down, no sideways drift");
        assert!((ratio_to_px(sh.dy_ratio) - 2.0).abs() < 0.001, "2 px down");
        assert_eq!(sh.color, [0, 0, 0, 204], "80% black");
        // ...and it must survive its own clamp, or it is not the preset that ships.
        let styled = SubtitleStyle {
            shadow: Some(sh),
            ..Default::default()
        };
        assert_eq!(styled.clone().clamped().shadow, Some(sh));
    }

    // -- persistence (#90.4) -----------------------------------------------

    /// The round trip that keeps a saved look saved.
    #[test]
    fn the_style_survives_a_toml_round_trip() {
        let want = SubtitleStyle {
            font_family: Some("Verdana".into()),
            size_pct: 0.06,
            color: [255, 240, 0, 220],
            shadow: Some(Shadow {
                dx_ratio: 0.003,
                dy_ratio: 0.004,
                blur_ratio: 0.005,
                color: [0, 0, 0, 180],
            }),
            background: [0, 0, 0, 153],
            ..Default::default()
        };
        let toml = toml::to_string_pretty(&want).expect("serialize");
        let got: SubtitleStyle = toml::from_str(&toml).expect("deserialize");
        assert_eq!(got, want);
    }

    /// ⚠ THE reason `#[serde(default)]` is on this struct. A hand-edited config that sets
    /// one key must not fail to parse — `Settings::load` turns a parse failure into "use
    /// the defaults", so without this, one typo here silently discards **every other
    /// setting in the file**.
    #[test]
    fn a_partial_table_fills_the_gaps_from_default() {
        let got: SubtitleStyle = toml::from_str("size_pct = 0.08").expect("partial parses");
        assert_eq!(got.size_pct, 0.08);
        assert_eq!(got.outline_ratio, SubtitleStyle::default().outline_ratio);
        assert_eq!(got.line_spacing, SubtitleStyle::default().line_spacing);
    }

    /// Same rule one level down: a shadow table naming only its blur is a shadow.
    #[test]
    fn a_partial_shadow_table_fills_the_gaps_from_default() {
        let got: SubtitleStyle =
            toml::from_str("[shadow]\nblur_ratio = 0.01").expect("partial shadow parses");
        let sh = got.shadow.expect("a named shadow table means a shadow");
        assert_eq!(sh.blur_ratio, 0.01);
        assert_eq!(sh.color, Shadow::default().color);
    }

    /// An absent shadow table is no shadow — which is the shipped default, and distinct
    /// from a present-but-empty one.
    #[test]
    fn no_shadow_table_means_no_shadow() {
        let got: SubtitleStyle = toml::from_str("size_pct = 0.05").unwrap();
        assert!(got.shadow.is_none());
        assert!(SubtitleStyle::default().shadow.is_none());
    }

    /// ⚠ TOML can hold `nan`, and `f32::clamp` PANICS on a NaN bound while a NaN input
    /// sails straight through it — so a non-finite must be reset before any clamp runs,
    /// not by it.
    #[test]
    fn non_finite_values_reset_instead_of_panicking() {
        let s: SubtitleStyle =
            toml::from_str("size_pct = nan\noutline_pct = inf\nline_spacing = -inf")
                .expect("nan parses; it is a real TOML float");
        let c = s.clamped();
        let d = SubtitleStyle::default();
        assert_eq!(c.size_pct, d.size_pct);
        assert_eq!(c.outline_ratio, d.outline_ratio);
        assert_eq!(c.line_spacing, d.line_spacing);
    }

    /// The FFI cannot carry `Option<String>`, so Swift sends `""` for "System". That must
    /// mean exactly what a config with no `font_family` key means.
    #[test]
    fn an_empty_font_name_means_the_system_font() {
        let mut s = SubtitleStyle::default();
        assert_eq!(s.font(), None);
        s.font_family = Some(String::new());
        assert_eq!(s.font(), None, "empty = System, same as absent");
        s.font_family = Some("   ".into());
        assert_eq!(s.font(), None, "whitespace too");
        s.font_family = Some("Verdana".into());
        assert_eq!(s.font(), Some("Verdana"));
    }

    /// The picker's list must be usable as-is: no blanks, no duplicates.
    #[test]
    fn the_font_shortlist_is_sane() {
        assert!(!FONT_CHOICES.is_empty());
        for f in FONT_CHOICES {
            assert!(!f.trim().is_empty());
        }
        let mut sorted = FONT_CHOICES.to_vec();
        sorted.sort_unstable();
        let n = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), n, "no duplicate font names");
    }

    // -- the picker's rows + the Shift+C cycle (#99) -----------------------

    /// The canonical list: Off, Automatic, then the renderable tracks. An unrenderable
    /// track is never offered — it would be an offer we cannot honour.
    #[test]
    fn the_picker_offers_off_then_automatic_then_what_it_can_draw() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("fr"), false, TrackCapability::Bitmap), // PGS — never a row
            sub(2, Some("de"), false, TrackCapability::SupportedText),
        ]);
        let choices = picker_choices(&c);
        assert_eq!(
            choices.len(),
            4,
            "Off + Automatic + the two renderable ones"
        );
        assert_eq!(choices[0], SubtitleChoice::Off);
        assert_eq!(choices[1], SubtitleChoice::Automatic);
        assert!(matches!(choices[2], SubtitleChoice::Track(id) if id.local_id == 0));
        assert!(matches!(choices[3], SubtitleChoice::Track(id) if id.local_id == 2));
    }

    /// With nothing renderable there is only Off — an `Automatic` row that could resolve to
    /// nothing would be a row that does nothing.
    #[test]
    fn a_clip_with_no_renderable_track_offers_only_off() {
        let c = catalog(vec![sub(0, Some("en"), false, TrackCapability::Bitmap)]);
        assert_eq!(picker_choices(&c), vec![SubtitleChoice::Off]);
    }

    /// `Shift+C` steps through tracks and wraps. No Off step (that is `C`'s) and no
    /// Automatic step (it resolves to one of these anyway).
    #[test]
    fn the_cycle_advances_through_tracks_and_wraps() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("de"), false, TrackCapability::SupportedText),
        ]);
        let t0 = c.subtitles.tracks[0].id;
        let t1 = c.subtitles.tracks[1].id;
        assert_eq!(
            next_track(&c, None),
            Some(t0),
            "nothing showing → the first"
        );
        assert_eq!(next_track(&c, Some(t0)), Some(t1));
        assert_eq!(next_track(&c, Some(t1)), Some(t0), "wraps");
    }

    /// The reason `next_track` takes the *showing* track rather than the selection: cycling
    /// while Automatic is on must move past what is on screen, not land back on it.
    #[test]
    fn cycling_from_automatic_advances_past_what_it_is_showing() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("de"), false, TrackCapability::SupportedText),
        ]);
        let showing = SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .unwrap()
            .id;
        assert_eq!(showing.local_id, 0, "Automatic fell back to the first");
        assert_eq!(
            next_track(&c, Some(showing)).map(|id| id.local_id),
            Some(1),
            "must advance to track 1, not re-select track 0"
        );
    }

    /// A file with nothing renderable has nothing to step to — the caller says so rather
    /// than silently doing nothing.
    #[test]
    fn the_cycle_has_no_answer_with_no_renderable_tracks() {
        let c = catalog(vec![sub(0, Some("en"), false, TrackCapability::Bitmap)]);
        assert_eq!(next_track(&c, None), None);
    }

    // -- `C` and the picker own different questions ------------------------

    /// **The owner's bug** (2026-07-15): pick Chinese, press `C` twice, get English. `C`
    /// must flip on/off and touch *nothing* else.
    #[test]
    fn toggling_off_and_on_never_forgets_the_picked_track() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("zh"), false, TrackCapability::SupportedText),
        ]);
        let zh = c.subtitles.tracks[1].id;
        let mut sel = SubtitleSelection::off();
        sel.apply(SubtitleChoice::Track(zh), &c);
        assert_eq!(sel.resolve(&c, Some("en")).unwrap().id, zh);

        sel.toggle();
        assert!(!sel.enabled);
        assert!(sel.resolve(&c, Some("en")).is_none(), "off means off");

        sel.toggle();
        assert_eq!(
            sel.resolve(&c, Some("en")).unwrap().id,
            zh,
            "back to Chinese — not to whatever Automatic would have chosen"
        );
    }

    /// Off must not disturb the choice, so the Off row and `C` agree.
    #[test]
    fn the_off_row_leaves_the_choice_intact() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("zh"), false, TrackCapability::SupportedText),
        ]);
        let zh = c.subtitles.tracks[1].id;
        let mut sel = SubtitleSelection::off();
        sel.apply(SubtitleChoice::Track(zh), &c);
        sel.apply(SubtitleChoice::Off, &c);
        assert!(!sel.enabled);
        assert_eq!(sel.want, SubtitleWant::Track(zh), "the choice survives Off");
        sel.toggle();
        assert_eq!(sel.resolve(&c, Some("en")).unwrap().id, zh);
    }

    // -- the language memory -----------------------------------------------

    /// **The headline.** A `TrackId` names one file's catalog by construction, so it cannot
    /// survive a nav — but the *intent* must. What you meant by picking Chinese was Chinese,
    /// not stream 1.
    #[test]
    fn a_picked_language_carries_to_the_next_film() {
        let film_a = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("zh"), false, TrackCapability::SupportedText),
        ]);
        let mut sel = SubtitleSelection::off();
        sel.apply(
            SubtitleChoice::Track(film_a.subtitles.tracks[1].id),
            &film_a,
        );

        // The next film: a different catalog (different generation), Chinese elsewhere in
        // the list, and English first — which is what a naive fallback would pick.
        let mut film_b = catalog(vec![
            sub(0, Some("en"), true, TrackCapability::SupportedText), // default!
            sub(1, Some("fr"), false, TrackCapability::SupportedText),
            sub(2, Some("zho"), false, TrackCapability::SupportedText), // the 639-2 spelling
        ]);
        film_b.generation = 999;
        for t in film_b.subtitles.tracks.iter_mut() {
            t.id.catalog_generation = 999;
        }

        let got = sel
            .resolve(&film_b, Some("en"))
            .expect("Chinese, found again");
        assert_eq!(
            got.language.as_deref(),
            Some("zho"),
            "the remembered language wins over the container's own default track — and \
             matches across spellings (picked \"zh\", found \"zho\")"
        );
    }

    /// The memory carries **forced-or-not**, not just the language.
    ///
    /// Without it a forced-English and a full-English track score identically — same
    /// language, same SDH — so the tie broke on *track order*. Picking the full English
    /// track on one film could land on English-forced in the next, which reads as the
    /// subtitles having mysteriously stopped working halfway through a box set. Latent
    /// until task #99 gave forced tracks a life of their own; the ordering below is the
    /// exact trap (forced sits first).
    #[test]
    fn the_memory_carries_forced_not_just_the_language() {
        let film = |generation| {
            let mut c = catalog(vec![
                sub(0, Some("en"), true, TrackCapability::SupportedText), // forced, FIRST
                sub(1, Some("en"), false, TrackCapability::SupportedText), // the full track
            ]);
            // ⚠ A DIFFERENT catalog generation is what makes this test mean anything: with
            // the same one, film B's TrackIds still match the pick and `resolve` returns
            // through the `want` branch, never reaching the memory at all. (Written without
            // this first — it passed against deliberately-broken scoring.)
            c.generation = generation;
            for t in c.subtitles.tracks.iter_mut() {
                t.id.catalog_generation = generation;
            }
            c
        };
        // Pick the FULL English track on film A…
        let (a, b) = (film(1), film(999));
        let mut sel = SubtitleSelection::off();
        sel.apply(SubtitleChoice::Track(a.subtitles.tracks[1].id), &a);
        assert!(matches!(&sel.preference, Some(p) if !p.forced));
        // …and film B must give you the full track again, not the forced one sitting first.
        let got = sel.resolve(&b, Some("en")).expect("English, found again");
        assert_eq!(
            got.id.local_id, 1,
            "a full-track pick must not decay into a forced track on the next film"
        );

        // And the reverse holds: pick forced, keep forced.
        let mut sel = SubtitleSelection::off();
        sel.apply(SubtitleChoice::Track(a.subtitles.tracks[0].id), &a);
        assert!(matches!(&sel.preference, Some(p) if p.forced));
        assert_eq!(sel.resolve(&b, Some("en")).unwrap().id.local_id, 0);
    }

    /// ...and when the next film simply hasn't got it, the ordinary chain takes over rather
    /// than showing nothing.
    #[test]
    fn a_remembered_language_absent_from_the_film_falls_through() {
        let film_a = catalog(vec![sub(
            0,
            Some("zh"),
            false,
            TrackCapability::SupportedText,
        )]);
        let mut sel = SubtitleSelection::off();
        sel.apply(
            SubtitleChoice::Track(film_a.subtitles.tracks[0].id),
            &film_a,
        );

        // The English track is the container's DEFAULT. (It used to be `forced` here, and
        // the test passed on the old forced-first rule while its own assertion said
        // "default" — so it never exercised what it claimed. Task #99 removed that rule and
        // exposed it.)
        let mut tracks = vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("fr"), false, TrackCapability::SupportedText),
        ];
        tracks[0].flags.default = true;
        let mut film_b = catalog(tracks);
        film_b.generation = 999;
        for t in film_b.subtitles.tracks.iter_mut() {
            t.id.catalog_generation = 999;
        }
        let got = sel.resolve(&film_b, Some("en")).expect("the default track");
        assert_eq!(got.language.as_deref(), Some("en"));
    }

    /// The tags must be compared through `language_display`, never raw: the *same file*
    /// reports "en" through AVFoundation and "eng" through FFmpeg, and "pt-BR" must still
    /// find a plain "por" track rather than fall through.
    #[test]
    fn the_memory_matches_languages_not_tag_spellings() {
        let a = catalog(vec![sub(
            0,
            Some("pt-BR"),
            false,
            TrackCapability::SupportedText,
        )]);
        let mut sel = SubtitleSelection::off();
        sel.apply(SubtitleChoice::Track(a.subtitles.tracks[0].id), &a);

        let mut b = catalog(vec![
            sub(0, Some("eng"), true, TrackCapability::SupportedText),
            sub(1, Some("por"), false, TrackCapability::SupportedText),
        ]);
        b.generation = 5;
        for t in b.subtitles.tracks.iter_mut() {
            t.id.catalog_generation = 5;
        }
        assert_eq!(
            sel.resolve(&b, Some("eng")).unwrap().language.as_deref(),
            Some("por"),
            "pt-BR is Portuguese; por is Portuguese — the raw tags share nothing"
        );
    }

    /// An exact region match beats a bare primary-subtag match.
    #[test]
    fn an_exact_region_match_wins_over_the_bare_language() {
        let a = catalog(vec![sub(
            0,
            Some("pt-BR"),
            false,
            TrackCapability::SupportedText,
        )]);
        let mut sel = SubtitleSelection::off();
        sel.apply(SubtitleChoice::Track(a.subtitles.tracks[0].id), &a);

        let mut b = catalog(vec![
            sub(0, Some("pt"), false, TrackCapability::SupportedText),
            sub(1, Some("pt-BR"), false, TrackCapability::SupportedText),
        ]);
        b.generation = 5;
        for t in b.subtitles.tracks.iter_mut() {
            t.id.catalog_generation = 5;
        }
        assert_eq!(
            sel.resolve(&b, None).unwrap().language.as_deref(),
            Some("pt-BR")
        );
    }

    /// Picking "Arabic (SDH)" over plain Arabic is a deliberate accessibility choice, so an
    /// SDH track should win the tie on the next film.
    #[test]
    fn an_sdh_pick_prefers_an_sdh_track_next_time() {
        let mut sdh = sub(1, Some("ar"), false, TrackCapability::SupportedText);
        sdh.flags.hearing_impaired = true;
        let a = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sdh,
        ]);
        let mut sel = SubtitleSelection::off();
        sel.apply(SubtitleChoice::Track(a.subtitles.tracks[1].id), &a);
        assert!(sel.preference.as_ref().unwrap().hearing_impaired);

        let mut b_sdh = sub(2, Some("ara"), false, TrackCapability::SupportedText);
        b_sdh.flags.hearing_impaired = true;
        let mut b = catalog(vec![
            sub(1, Some("ara"), false, TrackCapability::SupportedText),
            b_sdh,
        ]);
        b.generation = 7;
        for t in b.subtitles.tracks.iter_mut() {
            t.id.catalog_generation = 7;
        }
        let got = sel.resolve(&b, None).expect("Arabic");
        assert!(
            got.flags.hearing_impaired,
            "the SDH Arabic track, not the plain one"
        );
        // ...but SDH only breaks a tie *within* the language — it never drags in another.
        assert_eq!(got.language.as_deref(), Some("ara"));
    }

    /// Explicitly asking for Automatic retires the pinned language — otherwise there would
    /// be no way to un-pin it, and "Automatic" would go on obeying a cancelled choice.
    #[test]
    fn choosing_automatic_clears_the_remembered_language() {
        // English is the container's DEFAULT (was `forced` here, which is what the old
        // forced-first rule matched — while the assertion below said "default". See
        // `a_remembered_language_absent_from_the_film_falls_through`.)
        let mut tracks = vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("zh"), false, TrackCapability::SupportedText),
        ];
        tracks[0].flags.default = true;
        let c = catalog(tracks);
        let mut sel = SubtitleSelection::off();
        sel.apply(SubtitleChoice::Track(c.subtitles.tracks[1].id), &c);
        assert!(sel.preference.is_some());

        sel.apply(SubtitleChoice::Automatic, &c);
        assert!(sel.preference.is_none(), "the pin is released");
        assert_eq!(
            sel.resolve(&c, Some("en")).unwrap().language.as_deref(),
            Some("en"),
            "back to the container's default"
        );
    }

    /// A track with no language tag leaves nothing to carry — and must clear any older
    /// preference rather than let it linger over an unrelated pick.
    #[test]
    fn picking_an_untagged_track_carries_no_language() {
        let c = catalog(vec![
            sub(0, Some("zh"), false, TrackCapability::SupportedText),
            sub(1, None, false, TrackCapability::SupportedText),
        ]);
        let mut sel = SubtitleSelection::off();
        sel.apply(SubtitleChoice::Track(c.subtitles.tracks[0].id), &c);
        assert!(sel.preference.is_some());
        sel.apply(SubtitleChoice::Track(c.subtitles.tracks[1].id), &c);
        assert!(
            sel.preference.is_none(),
            "there is no intent to carry, and the old one must not survive"
        );
    }

    #[test]
    fn an_explicit_track_resolves() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("fr"), false, TrackCapability::StyledText),
        ]);
        let id = c.subtitles.tracks[1].id;
        let got = SubtitleSelection::track(id)
            .resolve(&c, Some("en"))
            .expect("explicit");
        assert_eq!(
            got.id.local_id, 1,
            "an explicit pick ignores the audio language"
        );
    }

    /// The stale guard, and what now happens *after* it fires.
    ///
    /// An id from another catalog must never resolve to whatever now sits at that
    /// `local_id` — that is the roulette that showed Korean where Arabic was picked. But it
    /// must not resolve to *nothing* either: a `TrackId` names one file by construction, so
    /// every nav would silently lose subtitles. It falls through to the ordinary chain, and
    /// (when one was remembered) to the picked language first.
    #[test]
    fn a_track_id_from_another_catalog_falls_back_rather_than_matching_or_vanishing() {
        // English is the container's DEFAULT (was `forced`; see
        // `a_remembered_language_absent_from_the_film_falls_through` for why that mattered).
        let mut tracks = vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("fr"), false, TrackCapability::SupportedText),
        ];
        tracks[0].flags.default = true;
        let c = catalog(tracks);
        // Same local_id as the French track, minted against a catalog that is not this one.
        let stale = TrackId {
            catalog_generation: 99,
            local_id: 1,
        };
        let got = SubtitleSelection::track(stale)
            .resolve(&c, Some("en"))
            .expect("falls back — never to nothing");
        assert_eq!(
            got.language.as_deref(),
            Some("en"),
            "the default track, NOT the French one that happens to share local_id 1"
        );
    }

    /// A bitmap track is listed in Details but can never be active — the picker won't
    /// offer it, but a stale id could still name it.
    #[test]
    fn an_unrenderable_track_never_becomes_active() {
        let c = catalog(vec![
            sub(0, Some("en"), true, TrackCapability::Bitmap),
            sub(1, Some("en"), true, TrackCapability::Unsupported),
        ]);
        let id = c.subtitles.tracks[0].id;
        assert!(SubtitleSelection::track(id)
            .resolve(&c, Some("en"))
            .is_none());
        assert!(
            SubtitleSelection::automatic()
                .resolve(&c, Some("en"))
                .is_none(),
            "Automatic must not pick a PGS track just because it's forced"
        );
        // ...but styled text is renderable.
        let c = catalog(vec![sub(0, Some("en"), true, TrackCapability::StyledText)]);
        assert!(SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .is_some());
    }

    #[test]
    fn an_empty_catalog_resolves_to_nothing() {
        let c = catalog(vec![]);
        assert!(SubtitleSelection::automatic()
            .resolve(&c, Some("en"))
            .is_none());
        assert!(SubtitleSelection::off().resolve(&c, Some("en")).is_none());
    }

    // -- style --------------------------------------------------------------

    #[test]
    fn size_is_physical_pixels_scaled_from_the_viewport() {
        let s = SubtitleStyle::default();
        // The property, derived — not the default's current number, which is owner-tuned
        // and pinned by `the_shipped_defaults_are_the_owner_tuned_numbers`. A test that
        // restates a tunable constant fails every time someone tunes it, and teaches the
        // next reader nothing about what it was guarding.
        assert!((s.size_px(1080.0) - s.size_pct * 1080.0).abs() < 0.01);
        // The same style on a 1x and a 2x display yields different PIXEL sizes — which is
        // exactly what keeps it looking the same and staying sharp.
        assert!((s.size_px(2160.0) - s.size_px(1080.0) * 2.0).abs() < 0.01);
        assert!(s.size_px(0.0) >= 1.0, "never zero or negative");
    }

    /// Settings come from a TOML a human edits — "40" (meaning points) must not produce a
    /// wall of text with no way back.
    #[test]
    fn absurd_settings_clamp_instead_of_breaking_the_screen() {
        let s = SubtitleStyle {
            size_pct: 40.0,
            outline_ratio: 99.0,
            vertical_offset_pct: -50.0,
            max_line_pct: 0.0,
            line_spacing: 0.0,
            shadow: Some(Shadow {
                dx_ratio: 9.0,
                dy_ratio: -9.0,
                blur_ratio: 9.0,
                color: [0, 0, 0, 255],
            }),
            ..SubtitleStyle::default()
        }
        .clamped();
        assert_eq!(s.size_pct, MAX_SIZE_PCT);
        assert_eq!(s.outline_ratio, MAX_OUTLINE_RATIO);
        assert_eq!(s.vertical_offset_pct, -0.5);
        assert_eq!(s.max_line_pct, 0.2);
        assert_eq!(s.line_spacing, 0.8);
        let sh = s.shadow.unwrap();
        assert_eq!(
            (sh.dx_ratio, sh.dy_ratio, sh.blur_ratio),
            (
                MAX_SHADOW_OFFSET_RATIO,
                -MAX_SHADOW_OFFSET_RATIO,
                MAX_SHADOW_BLUR_RATIO
            )
        );
        // The default is already sane and survives clamping unchanged.
        assert_eq!(SubtitleStyle::default().clamped(), SubtitleStyle::default());
    }

    // -- placement ----------------------------------------------------------

    /// A 16:9 picture letterboxed into a 2:1 viewport: bars top and bottom.
    fn letterboxed() -> ((f32, f32), Rect) {
        let viewport = (1000.0, 500.0);
        // 1000x562 doesn't fit; a 16:9 picture 1000 wide is 562 high > 500, so fit by
        // height instead: 888x500 pillarboxed... use an explicit letterbox instead.
        let video = Rect {
            x: 0.0,
            y: 50.0,
            w: 1000.0,
            h: 400.0, // 50px bars top and bottom
        };
        (viewport, video)
    }

    #[test]
    fn offset_zero_puts_the_block_on_the_videos_bottom_edge() {
        let (vp, video) = letterboxed();
        let s = SubtitleStyle {
            vertical_offset_pct: 0.0,
            ..SubtitleStyle::default()
        };
        let r = place(vp, video, (300.0, 40.0), 0.0, &s, 0.0);
        assert_eq!(r.bottom(), 450.0, "the video's bottom edge");
        assert_eq!(r.y, 410.0);
        assert_eq!(r.x, 350.0, "centered on the viewport");
    }

    #[test]
    fn a_positive_offset_moves_up_into_the_picture() {
        let (vp, video) = letterboxed();
        let s = SubtitleStyle {
            vertical_offset_pct: 0.1, // 10% of 500 = 50px up
            ..SubtitleStyle::default()
        };
        let r = place(vp, video, (300.0, 40.0), 0.0, &s, 0.0);
        assert_eq!(r.bottom(), 400.0);
        assert!(r.bottom() < video.bottom(), "inside the picture");
    }

    /// **The owner's feature**: a negative offset puts the text down in the black bar.
    ///
    /// The offset positions the block's **bottom edge**, so clearing the picture entirely
    /// takes an offset a little larger than the block's own height — the slider is tuned
    /// by eye, and this pins both sides of that.
    #[test]
    fn a_negative_offset_puts_the_block_in_the_letterbox() {
        let (vp, video) = letterboxed(); // picture 50..450, bars 0..50 and 450..500
        let block = (300.0, 40.0);
        let at = |pct: f32| {
            place(
                vp,
                video,
                block,
                0.0,
                &SubtitleStyle {
                    vertical_offset_pct: pct,
                    ..SubtitleStyle::default()
                },
                0.0,
            )
        };

        // -6% of 500 = 30px below the edge: the bottom is in the bar, the top still
        // overlaps the picture. A real, useful position — just not "clear of it".
        let straddling = at(-0.06);
        assert_eq!(straddling.bottom(), 480.0);
        assert!(
            straddling.y < video.bottom(),
            "the block is 40px tall, so it straddles"
        );

        // -9% = 45px: now the whole block sits in the bar, clear of the picture.
        let clear = at(-0.09);
        assert_eq!(clear.bottom(), 495.0);
        assert!(
            clear.y > video.bottom(),
            "entirely below the picture, in the bar"
        );
        assert!(clear.bottom() <= vp.1, "and still on screen");
    }

    /// With no letterbox to sit in (Fill, zoom, a clip matching the display aspect), a
    /// negative offset runs out of room and lands at the picture's bottom — it never goes
    /// off-screen, and the setting never becomes invalid.
    #[test]
    fn a_negative_offset_with_no_letterbox_degrades_to_the_screen_bottom() {
        let viewport = (1000.0, 500.0);
        let video = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 500.0, // fills — no bars
        };
        let s = SubtitleStyle {
            vertical_offset_pct: -0.2,
            ..SubtitleStyle::default()
        };
        let r = place(viewport, video, (300.0, 40.0), 0.0, &s, 0.0);
        assert_eq!(r.bottom(), 500.0, "clamped to the viewport");
        assert_eq!(r.y, 460.0);
    }

    /// It tracks the picture across clips of different aspect — the thing anchoring to the
    /// window loses.
    #[test]
    fn placement_tracks_the_video_edge_across_aspect_changes() {
        let viewport = (1000.0, 500.0);
        let s = SubtitleStyle {
            vertical_offset_pct: 0.04,
            ..SubtitleStyle::default()
        };
        let wide = Rect {
            x: 0.0,
            y: 100.0,
            w: 1000.0,
            h: 300.0,
        }; // big bars
        let tall = Rect {
            x: 0.0,
            y: 10.0,
            w: 1000.0,
            h: 480.0,
        }; // small bars
        let a = place(viewport, wide, (300.0, 40.0), 0.0, &s, 0.0);
        let b = place(viewport, tall, (300.0, 40.0), 0.0, &s, 0.0);
        assert_ne!(a.y, b.y, "the text follows the picture, not the window");
        assert_eq!(a.bottom(), wide.bottom() - 20.0);
        assert_eq!(b.bottom(), tall.bottom() - 20.0);
    }

    #[test]
    fn visible_controls_lift_the_block_including_one_parked_in_the_bar() {
        let (vp, video) = letterboxed();
        let s = SubtitleStyle {
            vertical_offset_pct: -0.06, // down in the bar, where the controls are
            ..SubtitleStyle::default()
        };
        let free = place(vp, video, (300.0, 40.0), 0.0, &s, 0.0);
        let lifted = place(vp, video, (300.0, 40.0), 0.0, &s, 80.0);
        assert_eq!(free.bottom(), 480.0);
        assert_eq!(lifted.bottom(), 420.0, "lifted above the 80px controls");
        assert!(lifted.bottom() < free.bottom());
        // Controls never push a block that's already above them.
        let high = SubtitleStyle {
            vertical_offset_pct: 0.3,
            ..SubtitleStyle::default()
        };
        assert_eq!(
            place(vp, video, (300.0, 40.0), 0.0, &high, 80.0).bottom(),
            place(vp, video, (300.0, 40.0), 0.0, &high, 0.0).bottom()
        );
    }

    // -- style -> params ----------------------------------------------------

    /// The conversion that keeps subtitles sharp: percentages in, PHYSICAL PIXELS out.
    /// The same style on a 1× and a 2× display must produce different pixel numbers —
    /// that is what makes the rasterizer redraw instead of a layer scaling it up.
    #[test]
    fn to_params_resolves_percentages_to_physical_pixels() {
        let s = SubtitleStyle {
            size_pct: 0.05,
            outline_ratio: 0.004,
            background_radius_ratio: 0.01,
            background_pad_ratio: 0.02,
            max_line_pct: 0.8,
            shadow: Some(Shadow {
                dx_ratio: 0.002,
                dy_ratio: 0.004,
                blur_ratio: 0.006,
                color: [0, 0, 0, 200],
            }),
            ..SubtitleStyle::default()
        };
        let p1 = s.to_params((1920.0, 1080.0));
        // Size is % of viewport HEIGHT...
        assert_eq!(p1.size_px, 54.0);
        // ...and every decoration is a fraction of THAT, not of the viewport. This is the
        // unit rule: change the size and the outline keeps its proportion, so the size
        // slider stops being a re-tune-everything slider.
        assert!((p1.outline_px - 0.004 * 54.0).abs() < 0.01);
        assert!((p1.background_radius_px - 0.01 * 54.0).abs() < 0.01);
        assert_eq!(p1.max_line_px, 1536.0, "max line is % of WIDTH");
        let sh = p1.shadow.unwrap();
        assert!((sh.dy - 0.004 * 54.0).abs() < 0.01);

        // The same style at 2x: every pixel number doubles (except width-derived ones,
        // which follow the width).
        let p2 = s.to_params((3840.0, 2160.0));
        assert_eq!(p2.size_px, p1.size_px * 2.0);
        assert!((p2.outline_px - p1.outline_px * 2.0).abs() < 0.01);
        assert_eq!(p2.max_line_px, p1.max_line_px * 2.0);
        assert_ne!(
            p1, p2,
            "so the rasterizer's cache key differs => it re-renders"
        );
    }

    #[test]
    fn to_params_never_emits_a_degenerate_size() {
        let p = SubtitleStyle::default().to_params((0.0, 0.0));
        assert!(p.size_px >= 1.0);
        assert!(p.max_line_px >= 1.0);
        assert!(p.outline_px >= 0.0);
    }

    /// A block wider or taller than the viewport still lands on screen rather than at a
    /// negative coordinate.
    #[test]
    fn oversized_blocks_never_resolve_off_screen() {
        let (vp, video) = letterboxed();
        let s = SubtitleStyle::default();
        let r = place(vp, video, (2000.0, 900.0), 0.0, &s, 0.0);
        assert_eq!(r.x, 0.0, "wider than the viewport pins to the left");
        assert_eq!(r.y, 0.0, "taller than the viewport pins to the top");
    }
}

#[cfg(test)]
mod zoom_placement_tests {
    use super::*;

    /// Zooming in pushes the video's bottom edge past the window. The block anchors to
    /// that edge — so without a clamp it would resolve off-screen and get cut. It must
    /// stay fully visible: subtitles are for reading.
    #[test]
    fn a_zoomed_video_keeps_its_subtitles_on_screen() {
        let viewport = (1000.0, 500.0);
        // Zoomed 2x: the video overflows the viewport top and bottom.
        let video = Rect {
            x: -500.0,
            y: -250.0,
            w: 2000.0,
            h: 1000.0,
        };
        assert!(
            video.bottom() > viewport.1,
            "the fixture must actually overflow"
        );

        let r = place(
            viewport,
            video,
            (600.0, 120.0),
            0.0,
            &SubtitleStyle::default(),
            0.0,
        );
        assert!(
            r.bottom() <= viewport.1,
            "the block ran past the window bottom: {r:?}"
        );
        assert!(r.y >= 0.0, "and it must not be pushed off the top: {r:?}");
    }
}
