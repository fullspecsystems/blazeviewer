//! **Subtitle selection + style + placement** (task #90.3/#90.4): the pure decisions
//! behind the overlay — *which* track shows, *how* it looks, and *where* it goes.
//!
//! No renderer, no clock, no I/O. The rasterizer consumes [`SubtitleStyle`] and the
//! presenters consume [`place`]'s rect; both shells get identical numbers because the
//! numbers are computed once, here.

use std::time::Duration;

use pb_decode::tracks::{language_display, MediaTrack, MediaTrackCatalog, TrackId};

/// What the user asked for.
///
/// Owner decisions (2026-07-14), both chosen for predictability over cleverness:
///
/// - **`Off` means off.** Forced subtitles do *not* leak through. The alternative
///   (VLC-style: "off" still shows forced signs) means text appears on screen while the
///   UI says subtitles are off, with no way to get true silence.
/// - **`Automatic` is forced-only, matching the audio.** It translates the signs and
///   alien dialogue *in the film you chose to watch* — and nothing else. It deliberately
///   does **not** follow the system language or OS accessibility settings: turning
///   subtitles on by itself is the kind of surprise that makes people hunt through
///   Settings.
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

        SubtitleMode::Automatic => {
            // Forced *and* the same language as what you're hearing. An unknown audio
            // language matches nothing: "Automatic" must never surprise, and guessing
            // here would show French signs over an English film.
            let audio = audio_language?;
            catalog
                .subtitles
                .tracks
                .iter()
                .find(|t| t.flags.forced && renderable(t) && same_language(t, audio))
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    /// Offset in **% of viewport height**, like every other size here.
    pub dx_pct: f32,
    pub dy_pct: f32,
    pub blur_pct: f32,
    pub color: [u8; 4],
}

/// The owner's eight customization axes (spec: 2026-07-14).
///
/// **Every size is a % of viewport height, never points.** A subtitle sized in points
/// reads differently on a 1× ultrawide and a 2× Studio; sized against the viewport it
/// looks the same everywhere, which is the whole point of a legibility setting.
///
/// Persisted (this is appearance, not a viewing trace). The *track choice* is not — see
/// [`SubtitleMode::Track`].
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleStyle {
    /// `None` = the system sans. A name the platform doesn't have falls back to it.
    pub font_family: Option<String>,
    pub size_pct: f32,
    pub color: [u8; 4],
    /// Outline thickness in % of viewport height; 0 = off.
    ///
    /// A true circular dilate of the glyph coverage — mathematically the outer half of a
    /// stroke — **not** the HUD's 8-way offset halo, which leaves gaps on diagonals and
    /// looks chewed at subtitle sizes.
    pub outline_pct: f32,
    pub outline_color: [u8; 4],
    pub shadow: Option<Shadow>,
    /// Alpha 0 = no background.
    pub background: [u8; 4],
    /// Corner radius in % of viewport height. Only meaningful with a background.
    pub background_radius_pct: f32,
    pub background_pad_pct: f32,
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
            outline_pct: 0.003,
            outline_color: [0, 0, 0, 255],
            shadow: None,
            // Off by default: the outline already carries legibility, and a box behind
            // every line is a lot of ink over the picture.
            background: [0, 0, 0, 0],
            background_radius_pct: 0.006,
            background_pad_pct: 0.008,
            // Just inside the picture's bottom edge — the classic position.
            vertical_offset_pct: 0.05,
            max_line_pct: 0.9,
            line_spacing: 1.2,
        }
    }
}

impl SubtitleStyle {
    /// Clamp every field into a sane range. Settings come from a TOML file a human can
    /// edit, so "size 40" (they meant points) must produce a large subtitle, not a
    /// full-screen wall that hides the film with no way back.
    pub fn clamped(mut self) -> Self {
        self.size_pct = self.size_pct.clamp(0.01, 0.25);
        self.outline_pct = self.outline_pct.clamp(0.0, 0.02);
        self.background_radius_pct = self.background_radius_pct.clamp(0.0, 0.05);
        self.background_pad_pct = self.background_pad_pct.clamp(0.0, 0.05);
        // Generous but bounded: far enough up to sit mid-screen, far enough down to reach
        // the bar on any letterbox.
        self.vertical_offset_pct = self.vertical_offset_pct.clamp(-0.5, 0.9);
        self.max_line_pct = self.max_line_pct.clamp(0.2, 1.0);
        self.line_spacing = self.line_spacing.clamp(0.8, 3.0);
        if let Some(s) = &mut self.shadow {
            s.dx_pct = s.dx_pct.clamp(-0.05, 0.05);
            s.dy_pct = s.dy_pct.clamp(-0.05, 0.05);
            s.blur_pct = s.blur_pct.clamp(0.0, 0.05);
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
pub fn place(
    viewport: (f32, f32),
    video: Rect,
    block: (f32, f32),
    style: &SubtitleStyle,
    controls_h: f32,
) -> Rect {
    let (vw, vh) = viewport;
    let (bw, bh) = block;

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
    // resolving to a negative y.
    bottom = bottom.min(vh).max(bh);

    Rect {
        x: ((vw - bw) / 2.0).max(0.0),
        y: bottom - bh,
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
        pb_hud::subtitle::SubtitleParams {
            font_family: self.font_family.clone(),
            size_px: self.size_px(vh),
            color: self.color,
            outline_px: (self.outline_pct * vh).max(0.0),
            outline_color: self.outline_color,
            shadow: self.shadow.map(|s| pb_hud::subtitle::ShadowParams {
                dx: s.dx_pct * vh,
                dy: s.dy_pct * vh,
                blur: (s.blur_pct * vh).max(0.0),
                color: s.color,
            }),
            background: self.background,
            background_radius_px: (self.background_radius_pct * vh).max(0.0),
            background_pad_px: (self.background_pad_pct * vh).max(0.0),
            // Width, not height — a max line length is about how far across the screen
            // the text runs.
            max_line_px: (self.max_line_pct * vw).max(1.0),
            line_spacing: self.line_spacing,
        }
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

    #[test]
    fn automatic_shows_nothing_when_no_forced_track_matches() {
        // French forced over English audio: not our film's signs.
        let c = catalog(vec![sub(
            0,
            Some("fr"),
            true,
            TrackCapability::SupportedText,
        )]);
        assert!(resolve_track(SubtitleMode::Automatic, &c, Some("en")).is_none());
        // A full English track is not forced, so Automatic leaves it alone.
        let c = catalog(vec![sub(
            0,
            Some("en"),
            false,
            TrackCapability::SupportedText,
        )]);
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

    /// Unknown audio language matches nothing: Automatic must never surprise, and guessing
    /// would show one language's signs over another language's film.
    #[test]
    fn automatic_is_conservative_when_the_audio_language_is_unknown() {
        let c = catalog(vec![sub(
            0,
            Some("en"),
            true,
            TrackCapability::SupportedText,
        )]);
        assert!(resolve_track(SubtitleMode::Automatic, &c, None).is_none());
        // ...and a forced track with no language of its own can't be matched either.
        let c = catalog(vec![sub(0, None, true, TrackCapability::SupportedText)]);
        assert!(resolve_track(SubtitleMode::Automatic, &c, Some("en")).is_none());
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
            outline_pct: 99.0,
            vertical_offset_pct: -50.0,
            max_line_pct: 0.0,
            line_spacing: 0.0,
            shadow: Some(Shadow {
                dx_pct: 9.0,
                dy_pct: -9.0,
                blur_pct: 9.0,
                color: [0, 0, 0, 255],
            }),
            ..SubtitleStyle::default()
        }
        .clamped();
        assert_eq!(s.size_pct, 0.25);
        assert_eq!(s.outline_pct, 0.02);
        assert_eq!(s.vertical_offset_pct, -0.5);
        assert_eq!(s.max_line_pct, 0.2);
        assert_eq!(s.line_spacing, 0.8);
        let sh = s.shadow.unwrap();
        assert_eq!((sh.dx_pct, sh.dy_pct, sh.blur_pct), (0.05, -0.05, 0.05));
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
        let r = place(vp, video, (300.0, 40.0), &s, 0.0);
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
        let r = place(vp, video, (300.0, 40.0), &s, 0.0);
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
        let r = place(viewport, video, (300.0, 40.0), &s, 0.0);
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
        let a = place(viewport, wide, (300.0, 40.0), &s, 0.0);
        let b = place(viewport, tall, (300.0, 40.0), &s, 0.0);
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
        let free = place(vp, video, (300.0, 40.0), &s, 0.0);
        let lifted = place(vp, video, (300.0, 40.0), &s, 80.0);
        assert_eq!(free.bottom(), 480.0);
        assert_eq!(lifted.bottom(), 420.0, "lifted above the 80px controls");
        assert!(lifted.bottom() < free.bottom());
        // Controls never push a block that's already above them.
        let high = SubtitleStyle {
            vertical_offset_pct: 0.3,
            ..SubtitleStyle::default()
        };
        assert_eq!(
            place(vp, video, (300.0, 40.0), &high, 80.0).bottom(),
            place(vp, video, (300.0, 40.0), &high, 0.0).bottom()
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
            outline_pct: 0.004,
            background_radius_pct: 0.01,
            background_pad_pct: 0.02,
            max_line_pct: 0.8,
            shadow: Some(Shadow {
                dx_pct: 0.002,
                dy_pct: 0.004,
                blur_pct: 0.006,
                color: [0, 0, 0, 200],
            }),
            ..SubtitleStyle::default()
        };
        let p1 = s.to_params((1920.0, 1080.0));
        assert_eq!(p1.size_px, 54.0);
        assert!((p1.outline_px - 4.32).abs() < 0.01);
        assert!((p1.background_radius_px - 10.8).abs() < 0.01);
        assert_eq!(p1.max_line_px, 1536.0, "max line is % of WIDTH");
        let sh = p1.shadow.unwrap();
        assert!((sh.dy - 4.32).abs() < 0.01);

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
        let r = place(vp, video, (2000.0, 900.0), &s, 0.0);
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
