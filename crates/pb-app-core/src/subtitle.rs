//! **Subtitle selection + style + placement** (task #90.3/#90.4): the pure decisions
//! behind the overlay — *which* track shows, *how* it looks, and *where* it goes.
//!
//! No renderer, no clock, no I/O. The rasterizer consumes [`SubtitleStyle`] and the
//! presenters consume [`place`]'s rect; both shells get identical numbers because the
//! numbers are computed once, here.

use std::time::Duration;

use pb_decode::tracks::{language_display, MediaTrack, MediaTrackCatalog, TrackId};
use serde::{Deserialize, Serialize};

/// What the user asked for.
///
/// Owner decisions (2026-07-14), both chosen for predictability over cleverness:
///
/// - **`Off` means off.** Forced subtitles do *not* leak through. The alternative
///   (VLC-style: "off" still shows forced signs) means text appears on screen while the
///   UI says subtitles are off, with no way to get true silence.
/// - **`Automatic` prefers forced-and-matching-the-audio**, then falls back. It first
///   looks for the signs and alien dialogue *in the film you chose to watch*; failing
///   that, the container author's default track; failing that, anything renderable. It
///   deliberately does **not** follow the system language or OS accessibility settings:
///   turning subtitles on by itself is the kind of surprise that makes people hunt
///   through Settings.
///
///   ⚠ **The fallbacks extend the owner's frozen 2026-07-14 rule, which said forced-only.
///   Flagged for review.** The reasoning: that rule was written to stop `Automatic` from
///   enabling subtitles *by itself*, and it is exactly right for that. But `Off` is the
///   default, so the mode is only ever `Automatic` because the user pressed `C` and got a
///   toast reading "Subtitles on" — at which point strict forced-only shows **nothing**
///   for the overwhelmingly common case (an English film with a full English track and no
///   forced track), which is the same surprise pointing the other way. It would also have
///   silently regressed the behaviour the owner validated on 2026-07-14, where `C` showed
///   the first renderable sidecar. Nothing here can turn subtitles on by itself; the
///   fallback can only fire once you have asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubtitleMode {
    /// Nothing shows, ever.
    #[default]
    Off,
    /// A forced track whose language matches the audio, if there is one.
    Automatic,
    /// Exactly this track. Session-only: a per-file choice is never persisted (privacy
    /// #2 — it would be a record of what you watched).
    Track(TrackId),
}

/// Which track should be on screen, given the mode.
///
/// `audio_language` is the language of the audio actually playing — the tag as the
/// container reported it (`"en"` or `"eng"`; both resolve).
///
/// Returns `None` for "show nothing", which is a normal answer, not a failure.
pub fn resolve_track<'a>(
    mode: SubtitleMode,
    catalog: &'a MediaTrackCatalog,
    audio_language: Option<&str>,
) -> Option<&'a MediaTrack> {
    match mode {
        // Off means off. No forced exception.
        SubtitleMode::Off => None,

        // A preference chain, not a single rule. See `AUTOMATIC` below for why the
        // fallbacks exist and why they are safe.
        SubtitleMode::Automatic => {
            let renderable = || catalog.subtitles.tracks.iter().filter(|t| renderable(t));

            // 1. The owner's rule (frozen 2026-07-14): forced *and* the same language as
            //    what you're hearing — the signs and alien dialogue in the film you chose.
            //    An unknown audio language matches nothing: guessing here would show
            //    French signs over an English film.
            if let Some(audio) = audio_language {
                if let Some(t) = renderable().find(|t| t.flags.forced && same_language(t, audio)) {
                    return Some(t);
                }
            }
            // 2. The track the container's author marked default.
            // 3. Anything we can render.
            renderable()
                .find(|t| t.flags.default)
                .or_else(|| renderable().next())
        }

        SubtitleMode::Track(id) => {
            // The generation check is the stale guard: an id minted against a catalog the
            // deck has since replaced must not resolve to whatever now sits at that
            // local_id. Selecting the wrong file's track is worse than selecting none.
            if id.catalog_generation != catalog.generation {
                return None;
            }
            catalog
                .subtitles
                .tracks
                .iter()
                .find(|t| t.id == id)
                // A track we cannot render (PGS, VobSub) is listed in Details but must
                // never end up active — the picker won't offer it, but a stale id could.
                .filter(|t| renderable(t))
        }
    }
}

/// The subtitle choices a user can cycle through with `Shift+C`, in order (#99).
///
/// `Off` is a **real row**, not the absence of a selection — it is first, so the cycle
/// always has a way back to no subtitles without hunting. Only renderable tracks appear:
/// a PGS track we cannot draw would be a dead stop in the rotation.
///
/// `Automatic` is deliberately **not** a step. It resolves to one of these tracks anyway,
/// so including it would show the same subtitles twice under two names — and once you are
/// explicitly picking, "let the app choose" is not a choice you are trying to make.
pub fn cycle_choices(catalog: &MediaTrackCatalog) -> Vec<SubtitleMode> {
    std::iter::once(SubtitleMode::Off)
        .chain(
            catalog
                .subtitles
                .tracks
                .iter()
                .filter(|t| renderable(t))
                .map(|t| SubtitleMode::Track(t.id)),
        )
        .collect()
}

/// The next choice after `current`, wrapping.
///
/// `current` is matched by *identity*, so `Automatic` — which is not itself a step — lands
/// on whatever it currently resolves to and advances from there. Pressing `Shift+C` while
/// Automatic is showing the English track therefore moves to the *next* track, not back to
/// the one already on screen. That is the whole reason this takes the resolved track
/// rather than just the mode.
pub fn next_choice(
    choices: &[SubtitleMode],
    current: SubtitleMode,
    resolved: Option<TrackId>,
) -> Option<SubtitleMode> {
    if choices.is_empty() {
        return None;
    }
    // Where are we now? An explicit track, else whatever Automatic resolved to, else Off.
    let here = match current {
        SubtitleMode::Off => Some(SubtitleMode::Off),
        SubtitleMode::Track(id) => Some(SubtitleMode::Track(id)),
        SubtitleMode::Automatic => resolved.map(SubtitleMode::Track),
    };
    let i = here
        .and_then(|h| choices.iter().position(|c| *c == h))
        // Not in the list (a stale id, or Automatic resolving to nothing) — start over.
        .unwrap_or(choices.len() - 1);
    Some(choices[(i + 1) % choices.len()])
}

/// The language of the audio the user is hearing — what [`resolve_track`]'s `Automatic`
/// forced rule matches against.
///
/// *Which* audio track is playing is #99's business (nothing switches them yet), so this
/// answers with the container author's default and falls back to the first. That is the
/// honest best answer rather than a placeholder: it is exactly right for the
/// overwhelming majority of files, which carry one audio track — and when it is wrong,
/// the cost is that `Automatic` misses a forced track and falls through to its ordinary
/// fallback, not that it shows the wrong language's signs.
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

/// Do the track and the audio speak the same language? Compared through the shared
/// display map, so `"en"` and `"eng"` — which different backends report for the *same
/// file* — are one language rather than two.
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
    /// A soft drop shadow, slightly down-right — the classic. Only reached when a config
    /// names the table without filling it in, or when the pane needs values to offer for a
    /// shadow you are about to switch on; the shipped default is no shadow at all (see
    /// [`SubtitleStyle::default`]), because the outline already carries legibility.
    fn default() -> Self {
        Shadow {
            dx_ratio: 0.04,
            dy_ratio: 0.04,
            blur_ratio: 0.08,
            color: [0, 0, 0, 200],
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
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        SubtitleStyle {
            font_family: None,
            // ~4.4% of a 1080p height ≈ 48 px — the broadcast-ish default.
            size_pct: 0.044,
            color: [255, 255, 255, 255],
            // ~6% of the font = ~2.9 px on 48 px text: the OUTLINE_PX_AT_DEFAULT_SIZE
            // scale's "3 px".
            outline_ratio: 0.06,
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
            // Just inside the picture's bottom edge — the classic position.
            vertical_offset_pct: 0.05,
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

/// The font size every decoration's **px** readout is calibrated against: the default text
/// size on a 1080p-tall viewport (`0.044 × 1080 ≈ 47.5`).
///
/// Storage is a fraction of the *actual* font size, so decoration holds its proportions as
/// the text resizes — that is the whole point of the unit rule. This constant only turns
/// that fraction into the human number a slider shows, because "outline 0.06" is not a
/// quantity anyone can picture and "3 px" is.
///
/// At the default text size the label is literally true; at other sizes it is the
/// px-*equivalent* — and that equivalent, not any particular measurement, is the stable
/// thing the user is actually choosing. The alternative (a live px readout against the
/// real font size) would make the slider handle jump every time the size moved, which is
/// the exact confusion this indirection removes.
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
        assert!(resolve_track(SubtitleMode::Off, &c, Some("en")).is_none());
    }

    /// Owner decision: Automatic = forced, matching the audio language.
    #[test]
    fn automatic_shows_a_forced_track_matching_the_audio() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText), // full English
            sub(1, Some("fr"), true, TrackCapability::SupportedText),  // French forced
            sub(2, Some("en"), true, TrackCapability::SupportedText),  // English forced ✓
        ]);
        let got = resolve_track(SubtitleMode::Automatic, &c, Some("en")).expect("match");
        assert_eq!(got.id.local_id, 2, "forced AND the audio's language");
    }

    /// The forced rule is a *preference*, not a filter. When it doesn't match, Automatic
    /// falls back rather than showing nothing — because the mode is only ever Automatic
    /// because the user pressed `C` and was told "Subtitles on". See `SubtitleMode`.
    #[test]
    fn automatic_falls_back_when_no_forced_track_matches() {
        // A full English track is not forced — but it is what the user asked for.
        let c = catalog(vec![sub(
            0,
            Some("en"),
            false,
            TrackCapability::SupportedText,
        )]);
        let got = resolve_track(SubtitleMode::Automatic, &c, Some("en")).expect("fallback");
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
        assert!(resolve_track(SubtitleMode::Automatic, &c, Some("en")).is_some());
    }

    /// Rule 1 beats the fallbacks: a forced track matching the audio wins even when
    /// another track is flagged default.
    #[test]
    fn the_forced_rule_outranks_the_default_flag() {
        let mut tracks = vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("en"), true, TrackCapability::SupportedText), // forced ✓
        ];
        tracks[0].flags.default = true;
        let c = catalog(tracks);
        let got = resolve_track(SubtitleMode::Automatic, &c, Some("en")).expect("match");
        assert_eq!(got.id.local_id, 1, "forced+matching outranks default");
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
        let got = resolve_track(SubtitleMode::Automatic, &c, Some("en")).expect("default");
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
        assert!(resolve_track(SubtitleMode::Automatic, &c, Some("en")).is_none());
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
        assert!(resolve_track(SubtitleMode::Automatic, &c, Some("en")).is_some());
        let c = catalog(vec![sub(
            0,
            Some("en"),
            true,
            TrackCapability::SupportedText,
        )]);
        assert!(resolve_track(SubtitleMode::Automatic, &c, Some("eng")).is_some());
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
        let got = resolve_track(SubtitleMode::Automatic, &c, None).expect("fallback");
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
        let got = resolve_track(SubtitleMode::Automatic, &c, Some("en")).expect("fallback");
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

    // -- the Shift+C cycle (#99) -------------------------------------------

    /// Off is a real row and it is first: the cycle must always have a way back to no
    /// subtitles. And an unrenderable track is never a step — it would be a dead stop.
    #[test]
    fn the_cycle_offers_off_first_and_skips_what_it_cannot_draw() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("fr"), false, TrackCapability::Bitmap), // PGS — not a step
            sub(2, Some("de"), false, TrackCapability::SupportedText),
        ]);
        let choices = cycle_choices(&c);
        assert_eq!(choices.len(), 3, "Off + the two renderable ones");
        assert_eq!(choices[0], SubtitleMode::Off);
        assert!(matches!(choices[1], SubtitleMode::Track(id) if id.local_id == 0));
        assert!(matches!(choices[2], SubtitleMode::Track(id) if id.local_id == 2));
    }

    /// A clip with no renderable track has nothing to cycle — Off alone is not a rotation.
    #[test]
    fn a_clip_with_no_renderable_track_has_nothing_to_cycle() {
        let c = catalog(vec![sub(0, Some("en"), false, TrackCapability::Bitmap)]);
        assert_eq!(cycle_choices(&c).len(), 1, "just Off");
    }

    #[test]
    fn the_cycle_advances_and_wraps_through_off() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("de"), false, TrackCapability::SupportedText),
        ]);
        let ch = cycle_choices(&c);
        let t0 = ch[1];
        let t1 = ch[2];
        assert_eq!(next_choice(&ch, SubtitleMode::Off, None), Some(t0));
        assert_eq!(next_choice(&ch, t0, None), Some(t1));
        // ...and back round to Off, so you're never trapped in the rotation.
        assert_eq!(next_choice(&ch, t1, None), Some(SubtitleMode::Off));
    }

    /// The reason `next_choice` takes the *resolved* track: cycling from `Automatic` must
    /// move past what is currently on screen, not land back on it.
    #[test]
    fn cycling_from_automatic_advances_past_what_it_is_showing() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("de"), false, TrackCapability::SupportedText),
        ]);
        let ch = cycle_choices(&c);
        // Automatic is showing track 0 (the fallback's first renderable).
        let showing = resolve_track(SubtitleMode::Automatic, &c, Some("en"))
            .unwrap()
            .id;
        assert_eq!(showing.local_id, 0);
        assert_eq!(
            next_choice(&ch, SubtitleMode::Automatic, Some(showing)),
            Some(ch[2]),
            "must advance to track 1, not re-select track 0"
        );
    }

    /// A stale id (or an Automatic resolving to nothing) must not strand the cycle —
    /// it restarts rather than doing nothing forever.
    #[test]
    fn an_unknown_current_choice_restarts_the_cycle() {
        let c = catalog(vec![sub(
            0,
            Some("en"),
            false,
            TrackCapability::SupportedText,
        )]);
        let ch = cycle_choices(&c);
        let stale = SubtitleMode::Track(TrackId {
            catalog_generation: 999,
            local_id: 42,
        });
        assert_eq!(next_choice(&ch, stale, None), Some(ch[0]));
        assert_eq!(next_choice(&ch, SubtitleMode::Automatic, None), Some(ch[0]));
    }

    #[test]
    fn an_explicit_track_resolves() {
        let c = catalog(vec![
            sub(0, Some("en"), false, TrackCapability::SupportedText),
            sub(1, Some("fr"), false, TrackCapability::StyledText),
        ]);
        let id = c.subtitles.tracks[1].id;
        let got = resolve_track(SubtitleMode::Track(id), &c, Some("en")).expect("explicit");
        assert_eq!(
            got.id.local_id, 1,
            "an explicit pick ignores the audio language"
        );
    }

    /// The stale guard: an id from a catalog the deck has replaced must not resolve to
    /// whatever now sits at that local_id. The wrong file's track is worse than none.
    #[test]
    fn a_track_id_from_a_stale_catalog_never_resolves() {
        let c = catalog(vec![sub(
            0,
            Some("en"),
            false,
            TrackCapability::SupportedText,
        )]);
        let stale = TrackId {
            catalog_generation: 99,
            local_id: 0,
        };
        assert!(resolve_track(SubtitleMode::Track(stale), &c, Some("en")).is_none());
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
        assert!(resolve_track(SubtitleMode::Track(id), &c, Some("en")).is_none());
        assert!(
            resolve_track(SubtitleMode::Automatic, &c, Some("en")).is_none(),
            "Automatic must not pick a PGS track just because it's forced"
        );
        // ...but styled text is renderable.
        let c = catalog(vec![sub(0, Some("en"), true, TrackCapability::StyledText)]);
        assert!(resolve_track(SubtitleMode::Automatic, &c, Some("en")).is_some());
    }

    #[test]
    fn an_empty_catalog_resolves_to_nothing() {
        let c = catalog(vec![]);
        assert!(resolve_track(SubtitleMode::Automatic, &c, Some("en")).is_none());
        assert!(resolve_track(SubtitleMode::Off, &c, Some("en")).is_none());
    }

    // -- style --------------------------------------------------------------

    #[test]
    fn size_is_physical_pixels_scaled_from_the_viewport() {
        let s = SubtitleStyle::default();
        // The same style on a 1x and a 2x display yields different PIXEL sizes — which is
        // exactly what keeps it looking the same and staying sharp.
        assert!(
            (s.size_px(1080.0) - 47.5).abs() < 1.0,
            "{}",
            s.size_px(1080.0)
        );
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
