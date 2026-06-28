//! The info-panel text layer.
//!
//! Rasterizes overlay text into a translucent panel using the OS UI font (Segoe
//! UI on Windows, loaded at runtime — not bundled), plus its bold face for
//! emphasis. The result is an RGBA8 bitmap the renderer draws as a single
//! alpha-blended quad, so it's rebuilt only when the content changes, never per
//! frame. Two layouts: a one-line panel ([`Hud::render_panel`], the basic `i`
//! overlay) and a two-column table ([`Hud::render_table`], the full-EXIF "nerd"
//! panel and the help overlay). All text gets a soft black outline for
//! legibility over bright photos.

use std::path::PathBuf;

/// Panel background: translucent black (≈60%).
const BG: [u8; 4] = [0, 0, 0, 153];
/// All overlay text is white; the label column is distinguished by weight
/// (semibold) rather than color, which reads far better over busy photos than a
/// dim gray. Legibility comes from the per-glyph outline (`SHADOW`).
const TEXT: [u8; 3] = [255, 255, 255];
/// Text outline/shadow color and its peak alpha (a soft black halo).
const SHADOW: [u8; 3] = [0, 0, 0];
const SHADOW_ALPHA: f32 = 0.65;

/// A row of the table layout ([`Hud::render_table`]).
pub enum Row {
    /// A full-width line spanning both columns — used for the filename (bold) /
    /// path header and section titles.
    Span { text: String, bold: bool },
    /// A two-column row: a semibold label on the left + a regular value.
    Pair { label: String, value: String },
}

/// Font weight to render a run of text in.
#[derive(Clone, Copy)]
enum Weight {
    Regular,
    Semibold,
    Bold,
}

pub struct Hud {
    font: fontdue::Font,
    /// The semibold face (label column emphasis); falls back to bold then regular.
    semibold: Option<fontdue::Font>,
    /// The bold face (filename / titles); falls back to semibold then regular.
    bold: Option<fontdue::Font>,
}

/// One laid-out glyph: its metrics, coverage bitmap, and pen x-offset on the line.
type Glyph = (fontdue::Metrics, Vec<u8>, f32);

impl Hud {
    /// Load the system UI font (and its semibold/bold faces if present). Returns
    /// `None` if no regular font is found, in which case the overlay is disabled.
    pub fn load() -> Option<Hud> {
        let load_face = |paths: &[PathBuf]| {
            first_readable(paths)
                .and_then(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok())
        };
        let font = load_face(&regular_font_paths())?;
        let semibold = load_face(&semibold_font_paths());
        let bold = load_face(&bold_font_paths());
        Some(Hud {
            font,
            semibold,
            bold,
        })
    }

    /// The face for a given weight, falling back gracefully when a face is absent.
    fn font_for(&self, weight: Weight) -> &fontdue::Font {
        match weight {
            Weight::Regular => &self.font,
            Weight::Semibold => self
                .semibold
                .as_ref()
                .or(self.bold.as_ref())
                .unwrap_or(&self.font),
            Weight::Bold => self
                .bold
                .as_ref()
                .or(self.semibold.as_ref())
                .unwrap_or(&self.font),
        }
    }

    /// Rasterize one line into the translucent panel with white, outlined text.
    /// Used for the basic `i` overlay. Returns `(rgba, w, h)`.
    pub fn render_panel(&self, text: &str, px: f32, pad: u32) -> Option<(Vec<u8>, u32, u32)> {
        self.render_panel_icon(text, px, pad, None)
    }

    /// Like [`render_panel`] but with an optional leading duotone icon (an SVG source
    /// from [`crate::icon::assets`]) — used by command toasts (e.g. the clipboard
    /// icon on Copy). The icon is rasterized at ~the text height, outlined like the
    /// text, and laid out left of the message and vertically centered. With `None`
    /// it's identical to the plain text panel.
    pub fn render_panel_icon(
        &self,
        text: &str,
        px: f32,
        pad: u32,
        icon: Option<&str>,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let line_h = self.line_height(px)?;
        let icon_h = (px * 0.92).round().max(1.0) as u32;
        let rasterized = icon.and_then(|svg| crate::icon::rasterize(svg, icon_h, TEXT));

        // Icon-only pill (empty message): a perfectly square rounded rect with the
        // icon centered on both axes — e.g. the rotate toasts. The side matches a
        // text pill's height so icon-only and text toasts read at the same scale.
        if text.is_empty() {
            let (rgba, iw, ih) = rasterized.as_ref()?;
            let side = line_h + 2 * pad;
            let mut canvas = Canvas::new(side, side, BG, (px * 0.5).round());
            let ix = (side as i32 - *iw as i32) / 2;
            let iy = (side as i32 - *ih as i32) / 2;
            self.draw_icon(&mut canvas, rgba, *iw, *ih, ix, iy, px);
            return Some((canvas.into_rgba(), side, side));
        }

        let (glyphs, advance) = self.layout(text, px, Weight::Regular);
        let (icon_w, gap) = match &rasterized {
            Some((_, w, _)) => (*w, (px * 0.40).round().max(3.0) as u32),
            None => (0, 0),
        };

        let text_x = pad + icon_w + gap;
        let pw = text_x + advance.ceil() as u32 + pad;
        let ph = line_h + 2 * pad;
        let mut canvas = Canvas::new(pw, ph, BG, (px * 0.5).round());

        if let Some((rgba, iw, ih)) = &rasterized {
            // Vertically center the icon on the text line.
            let iy = pad as i32 + (line_h as i32 - *ih as i32) / 2;
            self.draw_icon(&mut canvas, rgba, *iw, *ih, pad as i32, iy, px);
        }
        let baseline = pad as f32 + self.ascent(px)?;
        self.draw_line(&mut canvas, text_x as f32, baseline, &glyphs, TEXT, px);
        Some((canvas.into_rgba(), pw, ph))
    }

    /// Rasterize `rows` into a two-column table inside the translucent panel: a
    /// semibold label column + a regular value column, with full-width `Span` rows
    /// on top. Used for the full-EXIF "nerd" panel (tasks.json #5) and help overlay.
    pub fn render_table(&self, rows: &[Row], px: f32, pad: u32) -> Option<(Vec<u8>, u32, u32)> {
        if rows.is_empty() {
            return None;
        }
        let line_h = self.line_height(px)?;
        let ascent = self.ascent(px)?;
        // Gap between the label column and the value column.
        let col_gap = (px * 0.7).round().max(6.0);

        // Lay every row out once, measuring column widths as we go. Labels are
        // semibold, values regular, spans bold (filename) or regular (path).
        enum Laid {
            Span(Vec<Glyph>),
            Pair(Vec<Glyph>, Vec<Glyph>),
        }
        let mut laid = Vec::with_capacity(rows.len());
        let mut label_w = 0.0f32;
        let mut value_w = 0.0f32;
        let mut span_w = 0.0f32;
        for row in rows {
            match row {
                Row::Span { text, bold } => {
                    let weight = if *bold { Weight::Bold } else { Weight::Regular };
                    let (g, adv) = self.layout(text, px, weight);
                    span_w = span_w.max(adv);
                    laid.push(Laid::Span(g));
                }
                Row::Pair { label, value } => {
                    let (lg, la) = self.layout(label, px, Weight::Semibold);
                    let (vg, va) = self.layout(value, px, Weight::Regular);
                    label_w = label_w.max(la);
                    value_w = value_w.max(va);
                    laid.push(Laid::Pair(lg, vg));
                }
            }
        }

        let has_pairs = label_w > 0.0;
        let value_x = pad as f32 + label_w + if has_pairs { col_gap } else { 0.0 };
        let content_w = (value_x - pad as f32 + value_w).max(span_w);
        let pw = content_w.ceil() as u32 + 2 * pad;
        let ph = rows.len() as u32 * line_h + 2 * pad;
        let mut canvas = Canvas::new(pw, ph, BG, (px * 0.5).round());

        for (i, item) in laid.iter().enumerate() {
            let row_top = pad as f32 + i as f32 * line_h as f32;
            let baseline = row_top + ascent;
            match item {
                Laid::Span(g) => {
                    self.draw_line(&mut canvas, pad as f32, baseline, g, TEXT, px);
                }
                Laid::Pair(lg, vg) => {
                    self.draw_line(&mut canvas, pad as f32, baseline, lg, TEXT, px);
                    self.draw_line(&mut canvas, value_x, baseline, vg, TEXT, px);
                }
            }
        }
        Some((canvas.into_rgba(), pw, ph))
    }

    fn line_height(&self, px: f32) -> Option<u32> {
        let lm = self.font.horizontal_line_metrics(px)?;
        Some((lm.ascent - lm.descent + lm.line_gap).ceil().max(1.0) as u32)
    }

    fn ascent(&self, px: f32) -> Option<f32> {
        Some(self.font.horizontal_line_metrics(px)?.ascent)
    }

    /// Rasterize `text` into glyphs with their pen offsets; returns them plus the
    /// total advance width.
    fn layout(&self, text: &str, px: f32, weight: Weight) -> (Vec<Glyph>, f32) {
        let font = self.font_for(weight);
        let mut glyphs = Vec::new();
        let mut pen = 0.0f32;
        for ch in text.chars() {
            let (m, bitmap) = font.rasterize(ch, px);
            glyphs.push((m, bitmap, pen));
            pen += m.advance_width;
        }
        (glyphs, pen)
    }

    /// Composite one laid-out line at `origin_x`/`baseline`: a soft black outline
    /// first (for legibility over photos), then the colored glyphs on top.
    fn draw_line(
        &self,
        canvas: &mut Canvas,
        origin_x: f32,
        baseline: f32,
        glyphs: &[Glyph],
        rgb: [u8; 3],
        px: f32,
    ) {
        let s = (px * 0.06).round().max(1.0) as i32; // outline thickness
        for &(dx, dy) in &[(s, 0), (-s, 0), (0, s), (0, -s)] {
            for (m, bitmap, gx) in glyphs {
                canvas.blit_glyph(
                    m,
                    bitmap,
                    origin_x + *gx,
                    baseline,
                    dx,
                    dy,
                    SHADOW,
                    SHADOW_ALPHA,
                );
            }
        }
        for (m, bitmap, gx) in glyphs {
            canvas.blit_glyph(m, bitmap, origin_x + *gx, baseline, 0, 0, rgb, 1.0);
        }
    }

    /// Composite a rasterized icon at `(x0, y0)`: a soft black outline first (the
    /// same legibility halo text gets), then the icon on top. `rgba` is straight-
    /// alpha `iw×ih` (from [`crate::icon::rasterize`]).
    #[allow(clippy::too_many_arguments)]
    fn draw_icon(
        &self,
        canvas: &mut Canvas,
        rgba: &[u8],
        iw: u32,
        ih: u32,
        x0: i32,
        y0: i32,
        px: f32,
    ) {
        let s = (px * 0.06).round().max(1.0) as i32; // outline thickness (matches text)
        for &(dx, dy) in &[(s, 0), (-s, 0), (0, s), (0, -s)] {
            canvas.blit_silhouette(rgba, iw, ih, x0 + dx, y0 + dy, SHADOW, SHADOW_ALPHA);
        }
        canvas.blit_rgba(rgba, iw, ih, x0, y0);
    }
}

/// A straight-alpha RGBA8 software canvas for compositing the panel.
struct Canvas {
    px: Vec<u8>,
    w: i32,
    h: i32,
}

impl Canvas {
    /// A panel filled with `bg`, with anti-aliased rounded corners of `radius` px
    /// (corner pixels outside the rounded rect fade to transparent). The renderer
    /// draws it as an alpha-blended quad, so the rounding just lives in the alpha.
    fn new(w: u32, h: u32, bg: [u8; 4], radius: f32) -> Self {
        let (wi, hi) = (w as i32, h as i32);
        let r = radius.clamp(0.0, (w.min(h) as f32) / 2.0);
        let mut px = vec![0u8; (w as usize) * (h as usize) * 4];
        for y in 0..hi {
            for x in 0..wi {
                let cov = corner_coverage(x, y, wi, hi, r);
                if cov <= 0.0 {
                    continue; // fully outside a rounded corner → leave transparent
                }
                let idx = ((y * wi + x) * 4) as usize;
                px[idx] = bg[0];
                px[idx + 1] = bg[1];
                px[idx + 2] = bg[2];
                px[idx + 3] = (bg[3] as f32 * cov).round().clamp(0.0, 255.0) as u8;
            }
        }
        Self { px, w: wi, h: hi }
    }

    fn into_rgba(self) -> Vec<u8> {
        self.px
    }

    /// Composite `rgba` (straight-alpha) over the pixel at `(x, y)`, if in bounds.
    fn over(&mut self, x: i32, y: i32, rgb: [u8; 3], a: f32) {
        if a <= 0.0 || x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let idx = ((y * self.w + x) * 4) as usize;
        let dst = &mut self.px[idx..idx + 4];
        let ad = dst[3] as f32 / 255.0;
        let ao = a + ad * (1.0 - a);
        if ao <= 0.0 {
            return;
        }
        for (d, &c) in dst[..3].iter_mut().zip(rgb.iter()) {
            let cd = *d as f32;
            *d = ((c as f32 * a + cd * ad * (1.0 - a)) / ao)
                .round()
                .clamp(0.0, 255.0) as u8;
        }
        dst[3] = (ao * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    /// Composite a straight-alpha RGBA8 image (`iw×ih`) with its top-left at
    /// `(x0, y0)`, blending each pixel with its own color and alpha.
    fn blit_rgba(&mut self, rgba: &[u8], iw: u32, ih: u32, x0: i32, y0: i32) {
        let iw = iw as i32;
        for ry in 0..ih as i32 {
            for rx in 0..iw {
                let si = ((ry * iw + rx) * 4) as usize;
                let a = rgba[si + 3] as f32 / 255.0;
                if a <= 0.0 {
                    continue;
                }
                self.over(x0 + rx, y0 + ry, [rgba[si], rgba[si + 1], rgba[si + 2]], a);
            }
        }
    }

    /// Composite an image's *shape* (its alpha channel) as a flat `rgb` halo scaled
    /// by `a_scale` — the icon equivalent of the per-glyph text outline.
    #[allow(clippy::too_many_arguments)]
    fn blit_silhouette(
        &mut self,
        rgba: &[u8],
        iw: u32,
        ih: u32,
        x0: i32,
        y0: i32,
        rgb: [u8; 3],
        a_scale: f32,
    ) {
        let iw = iw as i32;
        for ry in 0..ih as i32 {
            for rx in 0..iw {
                let si = ((ry * iw + rx) * 4) as usize;
                let a = rgba[si + 3] as f32 / 255.0 * a_scale;
                if a <= 0.0 {
                    continue;
                }
                self.over(x0 + rx, y0 + ry, rgb, a);
            }
        }
    }

    /// Composite a single glyph's coverage at the given pen position, offset by
    /// `(dx, dy)`, in `rgb` scaled by `alpha`.
    #[allow(clippy::too_many_arguments)]
    fn blit_glyph(
        &mut self,
        m: &fontdue::Metrics,
        bitmap: &[u8],
        pen_x: f32,
        baseline: f32,
        dx: i32,
        dy: i32,
        rgb: [u8; 3],
        alpha: f32,
    ) {
        if m.width == 0 || m.height == 0 {
            return; // e.g. a space — advance only, no pixels
        }
        let x0 = (pen_x + m.xmin as f32).round() as i32 + dx;
        let y0 = (baseline - m.ymin as f32 - m.height as f32).round() as i32 + dy;
        for row in 0..m.height {
            for col in 0..m.width {
                let cov = bitmap[row * m.width + col];
                if cov == 0 {
                    continue;
                }
                self.over(
                    x0 + col as i32,
                    y0 + row as i32,
                    rgb,
                    (cov as f32 / 255.0) * alpha,
                );
            }
        }
    }
}

/// Coverage (0..=1) of pixel `(x, y)` inside a `w×h` rectangle with `r`-px rounded
/// corners — 1.0 everywhere except the four corner arcs, where it feathers over
/// ~1px for anti-aliasing.
fn corner_coverage(x: i32, y: i32, w: i32, h: i32, r: f32) -> f32 {
    if r <= 0.0 {
        return 1.0;
    }
    let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5); // pixel center
                                                     // Nearest point on the inner rect whose corners are the arc centers; distance
                                                     // to it is 0 except in the corner regions.
    let cx = fx.clamp(r, w as f32 - r);
    let cy = fy.clamp(r, h as f32 - r);
    let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
    if d <= 0.0 {
        1.0
    } else {
        (r - d + 0.5).clamp(0.0, 1.0)
    }
}

/// Rasterize the "not-ready" loading pie: a clean translucent dark disc with a
/// bright wedge that fills clockwise from 12 o'clock to `progress` (0..=1) — no
/// outer ring. `glow` (0..=1) brightens it (the keypress flash when the UI can't
/// service input yet). Edges are anti-aliased by `SS×SS` supersampling, so the
/// disc rim and the wedge edge read smooth. Returns straight-alpha RGBA8
/// `(pixels, diameter, diameter)`. No font needed, so it works without the HUD.
pub fn render_pie(diameter: u32, progress: f32, glow: f32) -> (Vec<u8>, u32, u32) {
    let d = diameter.max(8);
    let di = d as i32;
    let mut px = vec![0u8; (d as usize) * (d as usize) * 4];
    let c = d as f32 / 2.0;
    let r_outer = c - 1.0; // leave ~1px so the disc rim has room to feather
    let g = glow.clamp(0.0, 1.0);
    let sweep = progress.clamp(0.0, 1.0) * std::f32::consts::TAU;
    // The wedge (white) sits on the translucent dark track (black); both fade in
    // alpha at the edges via supersampling. `glow` lifts both a touch.
    let track_a = 0.42 + 0.18 * g;
    let fill_a = 0.85 + 0.15 * g;
    const SS: i32 = 4; // supersample grid (16 samples/pixel) for smooth edges
    let step = 1.0 / SS as f32;
    let inv = 1.0 / (SS * SS) as f32;
    for y in 0..di {
        for x in 0..di {
            // Accumulate straight-alpha color premultiplied so the white wedge
            // anti-aliases against the dark track and the transparent outside.
            let (mut sr, mut sg, mut sb, mut sa) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = x as f32 + (sx as f32 + 0.5) * step - c;
                    let fy = y as f32 + (sy as f32 + 0.5) * step - c;
                    if fx * fx + fy * fy > r_outer * r_outer {
                        continue; // outside the disc → transparent
                    }
                    let mut ang = fx.atan2(-fy); // 0 at 12 o'clock, +clockwise
                    if ang < 0.0 {
                        ang += std::f32::consts::TAU;
                    }
                    if ang <= sweep {
                        sr += fill_a; // white wedge
                        sg += fill_a;
                        sb += fill_a;
                        sa += fill_a;
                    } else {
                        sa += track_a; // black track contributes 0 to color
                    }
                }
            }
            let a = sa * inv;
            if a <= 0.0 {
                continue;
            }
            let idx = ((y * di + x) * 4) as usize;
            px[idx] = ((sr / sa) * 255.0).round().clamp(0.0, 255.0) as u8;
            px[idx + 1] = ((sg / sa) * 255.0).round().clamp(0.0, 255.0) as u8;
            px[idx + 2] = ((sb / sa) * 255.0).round().clamp(0.0, 255.0) as u8;
            px[idx + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    (px, d, d)
}

/// Group an integer's digits with thousands separators: `6000123` → `6,000,123`.
/// Counter-based (no `% 3`) so it stays clear of the 1.87-only `is_multiple_of`
/// the lint would otherwise push us toward (MSRV is 1.80).
pub fn format_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out: Vec<u8> = Vec::with_capacity(digits.len() + digits.len() / 3);
    let mut since_comma = 0u8;
    for &b in digits.as_bytes().iter().rev() {
        if since_comma == 3 {
            out.push(b',');
            since_comma = 0;
        }
        out.push(b);
        since_comma += 1;
    }
    out.reverse();
    String::from_utf8(out).expect("digits and commas are ASCII")
}

fn first_readable(paths: &[PathBuf]) -> Option<Vec<u8>> {
    paths.iter().find_map(|p| std::fs::read(p).ok())
}

fn fonts_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let windir = std::env::var("WINDIR")
            .or_else(|_| std::env::var("SystemRoot"))
            .unwrap_or_else(|_| "C:\\Windows".to_string());
        PathBuf::from(windir).join("Fonts")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/")
    }
}

fn regular_font_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        let f = fonts_dir();
        v.push(f.join("segoeui.ttf"));
        v.push(f.join("arial.ttf"));
    }
    #[cfg(target_os = "macos")]
    {
        v.push(PathBuf::from("/Library/Fonts/Arial.ttf"));
        v.push(PathBuf::from(
            "/System/Library/Fonts/Supplemental/Arial.ttf",
        ));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        v.push(PathBuf::from(
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ));
        v.push(PathBuf::from("/usr/share/fonts/TTF/DejaVuSans.ttf"));
    }
    v
}

fn semibold_font_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        let f = fonts_dir();
        v.push(f.join("seguisb.ttf")); // Segoe UI Semibold
        v.push(f.join("segoeuib.ttf")); // fall back to bold
    }
    #[cfg(target_os = "macos")]
    {
        v.push(PathBuf::from("/System/Library/Fonts/SFNSText-Semibold.otf"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        v.push(PathBuf::from(
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        ));
    }
    v
}

fn bold_font_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        let f = fonts_dir();
        v.push(f.join("segoeuib.ttf"));
        v.push(f.join("arialbd.ttf"));
    }
    #[cfg(target_os = "macos")]
    {
        v.push(PathBuf::from("/Library/Fonts/Arial Bold.ttf"));
        v.push(PathBuf::from(
            "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
        ));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        v.push(PathBuf::from(
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        ));
        v.push(PathBuf::from("/usr/share/fonts/TTF/DejaVuSans-Bold.ttf"));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separates_every_three_digits() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(7), "7");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1000), "1,000");
        assert_eq!(format_thousands(6_000_123), "6,000,123");
        assert_eq!(format_thousands(1_000_000_000), "1,000,000,000");
    }

    #[test]
    fn pie_fills_with_progress_and_is_round() {
        let (p0, _, _) = render_pie(48, 0.0, 0.0);
        let (p1, _, _) = render_pie(48, 1.0, 0.0);
        // The top-left corner is outside the disc → fully transparent.
        assert_eq!(p0[3], 0, "corner should be transparent (round, not square)");
        let bright = |px: &[u8]| {
            px.chunks_exact(4)
                .filter(|c| c[3] > 0 && c[0] > 200)
                .count()
        };
        assert!(
            bright(&p1) > bright(&p0),
            "a full pie has more bright (wedge) pixels than an empty one"
        );
        // The translucent track disc is drawn even at progress 0.
        let opaque = p0.chunks_exact(4).filter(|c| c[3] > 0).count();
        assert!(opaque > 0, "the track disc should be present at progress 0");
    }

    #[test]
    fn corner_coverage_rounds_only_the_corners() {
        let r = 10.0;
        // Interior and straight edges are fully covered.
        assert!((corner_coverage(50, 50, 100, 100, r) - 1.0).abs() < 1e-6);
        assert!((corner_coverage(50, 0, 100, 100, r) - 1.0).abs() < 1e-6); // top edge
        assert!((corner_coverage(0, 50, 100, 100, r) - 1.0).abs() < 1e-6); // left edge
                                                                           // The extreme corner pixel is outside the arc → transparent.
        assert_eq!(corner_coverage(0, 0, 100, 100, r), 0.0);
        assert_eq!(corner_coverage(99, 99, 100, 100, r), 0.0);
        // Radius 0 → no rounding anywhere.
        assert_eq!(corner_coverage(0, 0, 100, 100, 0.0), 1.0);
    }
}
