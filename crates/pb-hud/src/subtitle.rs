//! The **subtitle rasterizer** (task #90.3): cues + style → one RGBA8 bitmap.
//!
//! Shell-neutral CPU compute, like the rest of this crate: the winit shell hands the
//! bitmap to a wgpu overlay, the macOS host to a SwiftUI overlay above `AVPlayerLayer`.
//! One rasterizer means one look — and one implementation of the owner's style axes,
//! which is why this is not native `Text` on each platform (see
//! `.taskmaster/docs/90-presenter-and-style-contract.md`).
//!
//! **Everything here is in physical pixels.** [`SubtitleParams`] carries no percentages
//! and this crate never sees a viewport: the caller (`pb_app_core::subtitle`) resolves
//! `% of viewport height → px` before calling. That is not a style choice — rasterizing at
//! a logical size and letting a layer scale it up is exactly what makes text blurry, and
//! this project's known sharp edge is mixed 1×/2× displays. Making px the only unit that
//! crosses the boundary keeps the mistake unrepresentable.
//!
//! Runs on **cue change only**, never per frame (spike: ~0.15 ms/cue warm against an
//! 8.3 ms frame).

use cosmic_text::{Align, Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};

/// A rasterized overlay: tightly packed, **premultiplied** RGBA8.
///
/// Premultiplied because that is what both presenters want — a wgpu alpha-blended quad
/// and a `CALayer`/`CGImage` alike — and doing it here means neither shell has to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleBitmap {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

/// A drop shadow, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShadowParams {
    pub dx: f32,
    pub dy: f32,
    pub blur: f32,
    pub color: [u8; 4],
}

/// The style, resolved to physical pixels. The pb-app-core-side `SubtitleStyle` (with its
/// viewport percentages, serde, and clamps) converts into this.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleParams {
    pub font_family: Option<String>,
    pub size_px: f32,
    pub color: [u8; 4],
    /// Outline radius in px; 0 = off.
    pub outline_px: f32,
    pub outline_color: [u8; 4],
    pub shadow: Option<ShadowParams>,
    /// Alpha 0 = no background.
    pub background: [u8; 4],
    pub background_radius_px: f32,
    pub background_pad_px: f32,
    /// Wrap width in px.
    pub max_line_px: f32,
    /// Line height as a multiple of `size_px`.
    pub line_spacing: f32,
}

/// The cheapest thing that identifies a rendered bitmap, so an unchanged cue is not
/// re-shaped. The whole rebuild rule in one type: **cue, style, or viewport**.
#[derive(Debug, Clone, PartialEq)]
struct CacheKey {
    text: String,
    params: SubtitleParams,
}

/// Lays out and rasterizes subtitle text.
///
/// ⚠ **Construction is expensive and must not happen on the event loop.**
/// `FontSystem::new()` scans the system font database — **261 ms over 1114 faces** on the
/// spike machine. Build this on a worker when a subtitle track is first selected (or when
/// a video with one opens); a cue that appears 200 ms late on the first play is invisible,
/// a frozen window is not. This corrects the plan's "lazy-init on first subtitle use".
pub struct SubtitleRasterizer {
    fonts: FontSystem,
    swash: SwashCache,
    last: Option<(CacheKey, SubtitleBitmap)>,
}

impl SubtitleRasterizer {
    /// See the type docs: **off the event loop only**.
    pub fn new() -> Self {
        SubtitleRasterizer {
            fonts: FontSystem::new(),
            swash: SwashCache::new(),
            last: None,
        }
    }

    /// Every font family the system offers, sorted and deduped — the Settings font
    /// picker's list. One implementation for every platform, since fontdb is the source.
    pub fn families(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .fonts
            .db()
            .faces()
            .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Rasterize `lines` (already plain text — the cue engine stripped the markup).
    ///
    /// `None` when there is nothing to draw. The result is cached on
    /// (text, params), so a repeat call while the same cue is on screen is free — and the
    /// params carry the pixel sizes, so a **backing-scale change invalidates it
    /// automatically** rather than silently reusing a bitmap for the wrong display.
    pub fn render(&mut self, lines: &[String], params: &SubtitleParams) -> Option<&SubtitleBitmap> {
        let text = lines.join("\n");
        if text.trim().is_empty() {
            self.last = None;
            return None;
        }
        let key = CacheKey {
            text,
            params: params.clone(),
        };
        if self.last.as_ref().is_none_or(|(k, _)| *k != key) {
            let bmp = self.rasterize(&key.text, params)?;
            self.last = Some((key, bmp));
        }
        self.last.as_ref().map(|(_, b)| b)
    }

    fn rasterize(&mut self, text: &str, p: &SubtitleParams) -> Option<SubtitleBitmap> {
        let line_h = (p.size_px * p.line_spacing).max(1.0);
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(p.size_px, line_h));
        let mut b = buffer.borrow_with(&mut self.fonts);
        b.set_size(Some(p.max_line_px.max(1.0)), None);
        let family = match &p.font_family {
            // An unknown family falls back to the system sans rather than failing — a
            // config naming a font this machine lacks must not mean "no subtitles".
            Some(name) => Family::Name(name),
            None => Family::SansSerif,
        };
        b.set_text(
            text,
            &Attrs::new().family(family),
            Shaping::Advanced, // the full shaper: contextual forms, bidi
            None,
        );
        // Center every line against the others — the universal subtitle convention, and
        // what a two-line cue like "My mother" / "was one of the greats." needs to not
        // read as ragged left. Alignment is per-`BufferLine`, so it's set after the text.
        // The block is still shifted to its own ink below, so this centers the lines
        // *relative to each other*, not against the wrap box.
        for line in b.lines.iter_mut() {
            line.set_align(Some(Align::Center));
        }
        b.shape_until_scroll(false);

        // --- measure ------------------------------------------------------
        // Measure the real glyph extents, NOT `run.line_w`.
        //
        // This is not fussiness: cosmic-text lays an **RTL** run out right-aligned within
        // the buffer's width, so Arabic in a 1600px-wide buffer starts near x=1400. A box
        // sized from `line_w` and drawn from x=0 clips every glyph away and renders
        // *nothing*. Taking the min/max of the actual glyph positions handles LTR and RTL
        // identically, with no special case.
        let mut runs = 0usize;
        let mut min_x = f32::MAX;
        let mut max_x = f32::MIN;
        for run in b.layout_runs() {
            runs += 1;
            for g in run.glyphs {
                min_x = min_x.min(g.x);
                max_x = max_x.max(g.x + g.w);
            }
        }
        if runs == 0 || min_x > max_x {
            return None;
        }
        let max_w = (max_x - min_x).max(1.0);
        let grow = p.outline_px.max(0.0)
            + p.shadow
                .map_or(0.0, |s| s.blur + s.dx.abs().max(s.dy.abs()))
            + p.background_pad_px.max(0.0);
        let text_w = max_w.ceil().max(1.0);
        let text_h = (runs as f32 * line_h).ceil().max(1.0);
        let pad = grow.ceil();
        let w = (text_w + pad * 2.0) as u32;
        let h = (text_h + pad * 2.0) as u32;
        if w == 0 || h == 0 || w > 16384 || h > 16384 {
            return None;
        }

        // --- glyph coverage mask -------------------------------------------
        // One 8-bit mask of all the text. Every later pass (fill, outline, shadow) is a
        // read of this, so the glyphs are shaped and scaled exactly once.
        let mut mask = vec![0u8; (w * h) as usize];
        // Shift the measured ink to the padded box's origin. `min_x` is ~0 for LTR and
        // large for a right-aligned RTL run, so subtracting it lands both at the same place.
        let ox = pad - min_x;
        let mut b = buffer.borrow_with(&mut self.fonts);
        b.draw(
            &mut self.swash,
            cosmic_text::Color::rgba(255, 255, 255, 255),
            |x, y, cw, ch, color| {
                let a = color.a();
                if a == 0 {
                    return;
                }
                for yy in y..y + ch as i32 {
                    for xx in x..x + cw as i32 {
                        let px = xx + ox as i32;
                        let py = yy + pad as i32;
                        if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                            continue;
                        }
                        let i = (py as u32 * w + px as u32) as usize;
                        // Max, not add: overlapping glyph boxes must not double-darken.
                        mask[i] = mask[i].max(a);
                    }
                }
            },
        );

        // --- composite ------------------------------------------------------
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        if p.background[3] > 0 {
            fill_rounded_rect(&mut rgba, w, h, p.background_radius_px, p.background);
        }
        if let Some(s) = p.shadow {
            let blurred = blur(&mask, w, h, s.blur);
            let shifted = offset(&blurred, w, h, s.dx as i32, s.dy as i32);
            over(&mut rgba, &shifted, s.color);
        }
        if p.outline_px > 0.0 {
            // A true circular dilate: grow the glyph shape by `outline_px` in every
            // direction. That IS the outer half of a stroke — and unlike the HUD's 8-way
            // offset halo it has no gaps on diagonals, which is what makes a thin outline
            // look chewed at subtitle sizes.
            let grown = dilate(&mask, w, h, p.outline_px);
            over(&mut rgba, &grown, p.outline_color);
        }
        over(&mut rgba, &mask, p.color);

        Some(SubtitleBitmap { rgba, w, h })
    }
}

impl Default for SubtitleRasterizer {
    fn default() -> Self {
        Self::new()
    }
}

/// Alpha-composite a `color`-tinted coverage mask over `dst` (premultiplied, src-over).
fn over(dst: &mut [u8], mask: &[u8], color: [u8; 4]) {
    for (i, &m) in mask.iter().enumerate() {
        if m == 0 {
            continue;
        }
        // The source's alpha is the mask scaled by the color's own alpha.
        let sa = (m as u32 * color[3] as u32) / 255;
        if sa == 0 {
            continue;
        }
        let inv = 255 - sa;
        let d = &mut dst[i * 4..i * 4 + 4];
        for c in 0..3 {
            // Premultiplied src-over: dst = src*sa + dst*(1-sa).
            let src = (color[c] as u32 * sa) / 255;
            d[c] = (src + (d[c] as u32 * inv) / 255).min(255) as u8;
        }
        d[3] = (sa + (d[3] as u32 * inv) / 255).min(255) as u8;
    }
}

/// Grow a coverage mask by `r` px with an exact circular kernel.
fn dilate(mask: &[u8], w: u32, h: u32, r: f32) -> Vec<u8> {
    let ri = r.ceil() as i32;
    if ri <= 0 {
        return mask.to_vec();
    }
    // Precompute the disc once; at subtitle sizes this is a few dozen offsets.
    let mut disc = Vec::new();
    for dy in -ri..=ri {
        for dx in -ri..=ri {
            if ((dx * dx + dy * dy) as f32).sqrt() <= r {
                disc.push((dx, dy));
            }
        }
    }
    let mut out = vec![0u8; mask.len()];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let mut m = 0u8;
            for (dx, dy) in &disc {
                let (sx, sy) = (x + dx, y + dy);
                if sx < 0 || sy < 0 || sx >= w as i32 || sy >= h as i32 {
                    continue;
                }
                m = m.max(mask[(sy as u32 * w + sx as u32) as usize]);
                if m == 255 {
                    break;
                }
            }
            out[(y as u32 * w + x as u32) as usize] = m;
        }
    }
    out
}

/// A separable box blur, run 3× to approximate a Gaussian (the standard trick — three
/// boxes are visually indistinguishable from a true Gaussian and are O(n) per pass).
fn blur(mask: &[u8], w: u32, h: u32, radius: f32) -> Vec<u8> {
    let r = radius.round() as i32;
    if r <= 0 {
        return mask.to_vec();
    }
    let mut buf = mask.to_vec();
    for _ in 0..3 {
        buf = box_pass(&buf, w, h, r, true);
        buf = box_pass(&buf, w, h, r, false);
    }
    buf
}

fn box_pass(src: &[u8], w: u32, h: u32, r: i32, horizontal: bool) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    let (outer, inner) = if horizontal { (h, w) } else { (w, h) };
    for o in 0..outer as i32 {
        for i in 0..inner as i32 {
            let mut sum = 0u32;
            let mut n = 0u32;
            for k in -r..=r {
                let p = i + k;
                if p < 0 || p >= inner as i32 {
                    continue;
                }
                let idx = if horizontal {
                    (o as u32 * w + p as u32) as usize
                } else {
                    (p as u32 * w + o as u32) as usize
                };
                sum += src[idx] as u32;
                n += 1;
            }
            let idx = if horizontal {
                (o as u32 * w + i as u32) as usize
            } else {
                (i as u32 * w + o as u32) as usize
            };
            out[idx] = (sum / n.max(1)) as u8;
        }
    }
    out
}

fn offset(mask: &[u8], w: u32, h: u32, dx: i32, dy: i32) -> Vec<u8> {
    if dx == 0 && dy == 0 {
        return mask.to_vec();
    }
    let mut out = vec![0u8; mask.len()];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let (sx, sy) = (x - dx, y - dy);
            if sx < 0 || sy < 0 || sx >= w as i32 || sy >= h as i32 {
                continue;
            }
            out[(y as u32 * w + x as u32) as usize] = mask[(sy as u32 * w + sx as u32) as usize];
        }
    }
    out
}

/// The background: a rounded rect filling the bitmap, premultiplied.
fn fill_rounded_rect(dst: &mut [u8], w: u32, h: u32, radius: f32, color: [u8; 4]) {
    let r = radius.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);
    for y in 0..h {
        for x in 0..w {
            // Distance outside the inner (radius-shrunk) rect — the standard rounded-rect
            // SDF, antialiased over one pixel so the corners aren't stair-stepped.
            let cx = (r - x as f32).max(x as f32 - (w as f32 - 1.0 - r)).max(0.0);
            let cy = (r - y as f32).max(y as f32 - (h as f32 - 1.0 - r)).max(0.0);
            let d = (cx * cx + cy * cy).sqrt() - r;
            let cov = (1.0 - d).clamp(0.0, 1.0);
            if cov <= 0.0 {
                continue;
            }
            let a = (color[3] as f32 * cov) as u32;
            let i = ((y * w + x) * 4) as usize;
            for c in 0..3 {
                dst[i + c] = ((color[c] as u32 * a) / 255) as u8;
            }
            dst[i + 3] = a as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> SubtitleParams {
        SubtitleParams {
            font_family: None,
            size_px: 32.0,
            color: [255, 255, 255, 255],
            outline_px: 0.0,
            outline_color: [0, 0, 0, 255],
            shadow: None,
            background: [0, 0, 0, 0],
            background_radius_px: 0.0,
            background_pad_px: 0.0,
            max_line_px: 1600.0,
            line_spacing: 1.2,
        }
    }

    /// Any opaque pixel in the bitmap.
    fn ink(b: &SubtitleBitmap) -> usize {
        b.rgba.chunks_exact(4).filter(|p| p[3] > 0).count()
    }

    // These share one rasterizer where they can: `new()` is a ~260ms font-db scan.
    #[test]
    fn renders_text_to_premultiplied_rgba() {
        let mut r = SubtitleRasterizer::new();
        let b = r
            .render(&["Hello there".to_string()], &params())
            .expect("bitmap")
            .clone();
        assert!(b.w > 0 && b.h > 0);
        assert_eq!(b.rgba.len(), (b.w * b.h * 4) as usize);
        assert!(ink(&b) > 0, "something was drawn");
        // Premultiplied: no channel may exceed its own alpha.
        for p in b.rgba.chunks_exact(4) {
            assert!(
                p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3],
                "not premultiplied: {p:?}"
            );
        }
    }

    #[test]
    fn empty_and_blank_text_draw_nothing() {
        let mut r = SubtitleRasterizer::new();
        assert!(r.render(&[], &params()).is_none());
        assert!(r.render(&["".to_string()], &params()).is_none());
        assert!(r
            .render(&["   ".to_string(), "\t".to_string()], &params())
            .is_none());
    }

    /// The sharpness contract: params are in PIXELS, so the same cue at 2× is a bigger
    /// bitmap — not the same bitmap scaled up (which is what makes text blurry).
    #[test]
    fn a_larger_size_px_produces_a_larger_bitmap_not_a_scaled_one() {
        let mut r = SubtitleRasterizer::new();
        let small = r.render(&["Hello".into()], &params()).unwrap().clone();
        let big = r
            .render(
                &["Hello".into()],
                &SubtitleParams {
                    size_px: 64.0,
                    ..params()
                },
            )
            .unwrap()
            .clone();
        assert!(big.h > small.h, "2x size => ~2x height");
        assert!(big.w > small.w);
        assert!(ink(&big) > ink(&small));
    }

    /// The cache key includes the params, so a backing-scale change (which changes
    /// `size_px`) invalidates automatically — no shell has to remember to invalidate.
    #[test]
    fn the_cache_reuses_an_unchanged_cue_and_drops_a_rescaled_one() {
        let mut r = SubtitleRasterizer::new();
        let a = r.render(&["Same".into()], &params()).unwrap().clone();
        let b = r.render(&["Same".into()], &params()).unwrap().clone();
        assert_eq!(
            a, b,
            "an unchanged cue re-renders identically (and from cache)"
        );

        let scaled = r
            .render(
                &["Same".into()],
                &SubtitleParams {
                    size_px: 64.0,
                    ..params()
                },
            )
            .unwrap();
        assert_ne!(
            scaled.h, a.h,
            "a scale change must not reuse the old bitmap"
        );
    }

    #[test]
    fn multiple_lines_stack() {
        let mut r = SubtitleRasterizer::new();
        let one = r.render(&["One line".into()], &params()).unwrap().clone();
        let two = r
            .render(&["One line".into(), "Two lines".into()], &params())
            .unwrap()
            .clone();
        assert!(two.h > one.h, "a second line is taller");
        assert!(two.h as f32 >= one.h as f32 * 1.5);
    }

    /// The style axes must actually do something — each is checked against the same text
    /// with the axis off.
    #[test]
    fn outline_shadow_and_background_each_add_ink() {
        let mut r = SubtitleRasterizer::new();
        let plain = ink(r.render(&["Hi".into()], &params()).unwrap());

        let outlined = r
            .render(
                &["Hi".into()],
                &SubtitleParams {
                    outline_px: 3.0,
                    ..params()
                },
            )
            .unwrap();
        assert!(ink(outlined) > plain, "an outline grows the covered area");

        let shadowed = r
            .render(
                &["Hi".into()],
                &SubtitleParams {
                    shadow: Some(ShadowParams {
                        dx: 2.0,
                        dy: 2.0,
                        blur: 3.0,
                        color: [0, 0, 0, 255],
                    }),
                    ..params()
                },
            )
            .unwrap();
        assert!(ink(shadowed) > plain, "a shadow adds ink");

        let boxed = r
            .render(
                &["Hi".into()],
                &SubtitleParams {
                    background: [0, 0, 0, 180],
                    background_pad_px: 8.0,
                    ..params()
                },
            )
            .unwrap();
        // A background fills the whole bitmap.
        assert_eq!(ink(boxed), (boxed.w * boxed.h) as usize);
    }

    /// A background's corners must actually be rounded — the owner asked for it
    /// explicitly, and a radius that silently did nothing would look like a square box.
    #[test]
    fn a_background_radius_rounds_the_corners() {
        let mut r = SubtitleRasterizer::new();
        let square = r
            .render(
                &["Hi".into()],
                &SubtitleParams {
                    background: [0, 0, 0, 255],
                    background_pad_px: 12.0,
                    background_radius_px: 0.0,
                    ..params()
                },
            )
            .unwrap()
            .clone();
        let rounded = r
            .render(
                &["Hi".into()],
                &SubtitleParams {
                    background: [0, 0, 0, 255],
                    background_pad_px: 12.0,
                    background_radius_px: 10.0,
                    ..params()
                },
            )
            .unwrap()
            .clone();
        assert_eq!((square.w, square.h), (rounded.w, rounded.h));
        // The top-left pixel: opaque when square, transparent when rounded away.
        assert_eq!(square.rgba[3], 255, "a square box fills its corner");
        assert_eq!(rounded.rgba[3], 0, "a radius cuts the corner");
        assert!(ink(&rounded) < ink(&square));
    }

    /// The spike's scripts, through the real path: fallback must find them, and shaping
    /// must not collapse them to nothing.
    #[test]
    fn non_latin_scripts_rasterize_via_font_fallback() {
        let mut r = SubtitleRasterizer::new();
        for s in ["日本語字幕のテスト", "السلام عليكم", "हिन्दी", "Привет"]
        {
            let b = r
                .render(&[s.to_string()], &params())
                .unwrap_or_else(|| panic!("{s}"));
            assert!(ink(b) > 0, "{s} rendered nothing — fallback failed");
        }
    }

    /// A multi-line cue centers its lines against each other — the subtitle convention
    /// every player follows. cosmic-text defaults to left, which rendered a short line
    /// over a long one as ragged-left and looked plainly wrong.
    ///
    /// Measured, not eyeballed: the short line's ink must be centered in the bitmap, so
    /// its left and right margins match. Left alignment leaves the whole gap on the right.
    #[test]
    fn a_short_line_over_a_long_one_is_centered_not_ragged_left() {
        let mut r = SubtitleRasterizer::new();
        let b = r
            .render(
                &[
                    "My mother".to_string(),
                    "was one of the greats.".to_string(),
                ],
                &params(),
            )
            .expect("rendered");

        // Column extents of the *first* line's ink (the top half of the block).
        let (w, h) = (b.w as usize, b.h as usize);
        let (mut lo, mut hi) = (w, 0usize);
        for y in 0..h / 2 {
            for x in 0..w {
                if b.rgba[(y * w + x) * 4 + 3] > 0 {
                    lo = lo.min(x);
                    hi = hi.max(x);
                }
            }
        }
        assert!(lo < hi, "the short line drew nothing");
        let (left, right) = (lo as i32, (w - 1 - hi) as i32);
        assert!(
            (left - right).abs() <= 4,
            "short line not centered: {left}px left vs {right}px right margin"
        );
    }

    /// Regression: cosmic-text lays an **RTL** run out *right-aligned within the buffer
    /// width*, so Arabic in a wide wrap box starts near the right edge. Sizing the bitmap
    /// from `run.line_w` and drawing from x=0 clipped every glyph and rendered **nothing**.
    /// A wide wrap box must not change whether RTL text appears.
    #[test]
    fn rtl_text_is_not_clipped_by_a_wide_wrap_width() {
        let mut r = SubtitleRasterizer::new();
        let arabic = "السلام عليكم";
        let narrow = r
            .render(
                &[arabic.to_string()],
                &SubtitleParams {
                    max_line_px: 300.0,
                    ..params()
                },
            )
            .expect("narrow")
            .clone();
        let wide = r
            .render(
                &[arabic.to_string()],
                &SubtitleParams {
                    max_line_px: 4000.0, // the case that used to clip it away entirely
                    ..params()
                },
            )
            .expect("wide")
            .clone();
        assert!(
            ink(&narrow) > 0 && ink(&wide) > 0,
            "RTL must render at any wrap width"
        );
        assert_eq!(
            (narrow.w, narrow.h),
            (wide.w, wide.h),
            "the box tracks the ink, not the wrap width"
        );
        assert_eq!(ink(&narrow), ink(&wide));
    }

    /// The same rule for LTR: the bitmap is sized to the text, so a huge wrap width does
    /// not produce a mostly-empty bitmap (which would also mis-center the overlay).
    #[test]
    fn the_bitmap_tracks_the_ink_not_the_wrap_width() {
        let mut r = SubtitleRasterizer::new();
        let a = r.render(&["Hi".into()], &params()).unwrap().clone();
        let b = r
            .render(
                &["Hi".into()],
                &SubtitleParams {
                    max_line_px: 8000.0,
                    ..params()
                },
            )
            .unwrap()
            .clone();
        assert_eq!((a.w, a.h), (b.w, b.h));
        assert!(
            (a.w as f32) < 400.0,
            "not 8000px of mostly nothing: {}",
            a.w
        );
    }

    #[test]
    fn an_unknown_font_family_falls_back_rather_than_failing() {
        let mut r = SubtitleRasterizer::new();
        let b = r.render(
            &["Hello".into()],
            &SubtitleParams {
                font_family: Some("No Such Font 12345".into()),
                ..params()
            },
        );
        assert!(
            b.is_some_and(|b| ink(b) > 0),
            "a missing font must not mean no subtitles"
        );
    }

    /// The Settings picker's list — one implementation, every platform, from fontdb.
    #[test]
    fn families_lists_system_fonts_sorted_and_deduped() {
        let r = SubtitleRasterizer::new();
        let f = r.families();
        assert!(f.len() > 5, "a real system has fonts: {}", f.len());
        let mut sorted = f.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(f, sorted, "sorted + deduped");
    }

    // -- the compositing primitives (pure, no fonts) ------------------------

    #[test]
    fn dilate_grows_a_dot_by_the_radius_in_every_direction() {
        // A single lit pixel in the middle of an 11x11 field.
        let (w, h) = (11u32, 11u32);
        let mut m = vec![0u8; (w * h) as usize];
        m[(5 * w + 5) as usize] = 255;
        let g = dilate(&m, w, h, 2.0);
        // Grown exactly `r` along the axes...
        assert_eq!(g[(5 * w + 7) as usize], 255, "2px right");
        assert_eq!(g[(5 * w + 8) as usize], 0, "but not 3px");
        assert_eq!(g[(3 * w + 5) as usize], 255, "2px up");
        // ...and it's a DISC, not a square: the diagonal corner is outside radius 2.
        assert_eq!(g[(3 * w + 3) as usize], 0, "circular, not square");
        assert_eq!(dilate(&m, w, h, 0.0), m, "radius 0 is a no-op");
    }

    #[test]
    fn blur_spreads_and_conserves_roughly() {
        let (w, h) = (21u32, 21u32);
        let mut m = vec![0u8; (w * h) as usize];
        m[(10 * w + 10) as usize] = 255;
        let b = blur(&m, w, h, 3.0);
        assert!(b[(10 * w + 10) as usize] < 255, "the peak spreads out");
        assert!(b[(10 * w + 12) as usize] > 0, "into the neighbourhood");
        assert!(b.iter().filter(|&&v| v > 0).count() > 20, "over an area");
        assert_eq!(blur(&m, w, h, 0.0), m, "radius 0 is a no-op");
    }

    #[test]
    fn offset_shifts_and_clips_at_the_edges() {
        let (w, h) = (5u32, 5u32);
        let mut m = vec![0u8; (w * h) as usize];
        m[(2 * w + 2) as usize] = 255;
        let o = offset(&m, w, h, 1, 1);
        assert_eq!(o[(3 * w + 3) as usize], 255);
        assert_eq!(o[(2 * w + 2) as usize], 0);
        // Shifted off the edge: gone, not wrapped.
        let far = offset(&m, w, h, 99, 0);
        assert!(far.iter().all(|&v| v == 0), "no wraparound");
        assert_eq!(offset(&m, w, h, 0, 0), m);
    }

    #[test]
    fn over_composites_premultiplied_and_respects_color_alpha() {
        let mut dst = vec![0u8; 4];
        over(&mut dst, &[255], [255, 0, 0, 255]);
        assert_eq!(dst, vec![255, 0, 0, 255], "opaque red");

        // Half-coverage of an opaque color => half alpha, premultiplied channels.
        let mut dst = vec![0u8; 4];
        over(&mut dst, &[128], [255, 255, 255, 255]);
        assert_eq!(dst[3], 128);
        assert!(dst[0] <= dst[3], "premultiplied");

        // A zero mask changes nothing.
        let mut dst = vec![9u8, 9, 9, 9];
        over(&mut dst, &[0], [255, 0, 0, 255]);
        assert_eq!(dst, vec![9, 9, 9, 9]);
    }
}
