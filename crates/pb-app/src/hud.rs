//! The info-panel text layer.
//!
//! Rasterizes overlay text into a translucent panel using the OS UI font (Segoe
//! UI on Windows, SF Pro on macOS — loaded at runtime, not bundled), plus a bolder
//! face for emphasis (synthesized from SF Pro on macOS; see [`embolden_glyph`]).
//! The result is an RGBA8 bitmap the renderer draws as a single
//! alpha-blended quad, so it's rebuilt only when the content changes, never per
//! frame. Two layouts: a one-line panel ([`Hud::render_panel`], the basic `i`
//! overlay) and a two-column table ([`Hud::render_table`], the full-EXIF "nerd"
//! panel and the help overlay). All text gets a soft black outline for
//! legibility over bright photos.

use std::path::PathBuf;

/// **HUD design tokens** — the single source of truth for the proportions, colors, and
/// rhythm every on-image overlay shares (the CPU-compositor analogue of `pb_ui`'s
/// `SPACE_*` / `RADIUS_*` / `Palette`). Sizes are **fractions of a context's base text
/// height `px`**, so one knob scales a whole component and everything stays crisp across
/// DPI. Re-express any new overlay in these rather than reaching for a fresh magic number,
/// so cards / toasts / panels can't drift apart.
pub mod tokens {
    // ── Color roles ─────────────────────────────────────────────────────────────────
    /// Default panel background: translucent black (≈60%). Toasts and the help overlay use
    /// this directly; the info / EXIF panels take a user-configurable alpha via
    /// [`super::bg_for_opacity`].
    pub const BG: [u8; 4] = [0, 0, 0, 153];
    /// All overlay text is white; the label column is distinguished by weight (semibold)
    /// rather than color, which reads far better over busy photos than a dim gray.
    /// Legibility comes from the per-glyph outline ([`SHADOW`]).
    pub const TEXT: [u8; 3] = [255, 255, 255];
    /// A dimmer white for secondary lines on a *card* (e.g. the scan card's count line).
    /// Only used where there's a panel background behind it — over a bare photo we still
    /// distinguish by weight, not color (see the note above).
    pub const TEXT_DIM: [u8; 3] = [188, 191, 198];
    /// Text/icon outline color — the soft black halo that keeps white glyphs legible over
    /// bright photos — and its peak alpha.
    pub const SHADOW: [u8; 3] = [0, 0, 0];
    pub const SHADOW_ALPHA: f32 = 0.65;

    // ── Proportions (fractions of the base text height `px`) ─────────────────────────
    /// Outline/halo thickness for text and icons.
    pub const OUTLINE: f32 = 0.06;
    /// Corner radius of a pill / panel (info pill, toast, EXIF table, hint).
    pub const RADIUS_PANEL: f32 = 0.5;
    /// Corner radius of the scan status card.
    pub const RADIUS_CARD: f32 = 0.6;

    /// Horizontal padding as a multiple of the caller's (vertical) `pad`, for the left-aligned
    /// panels — the info pill, toasts, the info/EXIF/help table, and the centered hint. The
    /// text's line box carries ascent/descent whitespace above and below the ink that a single
    /// `pad` can't see, so equal pads read as *tight sides*; `> 1` widens the left/right inset
    /// to visually match the top/bottom. (The square icon-only pill stays square; the
    /// center-aligned scan card already has generous side space and is exempt.)
    pub const PAD_X: f32 = 1.7;

    // ── Pill / toast leading icon ────────────────────────────────────────────────────
    /// Height of a toast's leading icon, relative to the text.
    pub const PILL_ICON: f32 = 0.92;
    /// Gap between a toast's leading icon and its text (floored at 3px).
    pub const PILL_ICON_GAP: f32 = 0.40;

    // ── Two-column table (info / EXIF / help) ────────────────────────────────────────
    /// Gap between the label and value columns (floored at 6px).
    pub const TABLE_COL_GAP: f32 = 0.7;

    // ── Scan status card ─────────────────────────────────────────────────────────────
    /// Secondary (dim) line size — the path + count lines — relative to the heading.
    pub const CARD_SUB: f32 = 0.82;
    /// Card inner padding (floored at 6px).
    pub const CARD_PAD: f32 = 0.85;
    /// Vertical gap between the card's text lines.
    pub const CARD_GAP_LINES: f32 = 0.14;
    /// Vertical gap between the last text line and the button (floored at 4px).
    pub const CARD_GAP_BUTTON: f32 = 0.6;

    // ── Button (fractions of the *button's own* text px) ─────────────────────────────
    /// Button inner padding, left/right (floored at 3px). Separate from [`BUTTON_PAD_Y`] so the
    /// horizontal and vertical insets tune independently — the label's line box already pads
    /// the top/bottom, so the two axes want different values to read balanced.
    pub const BUTTON_PAD_X: f32 = 0.5;
    /// Button inner padding, top/bottom (floored at 2px). Smaller than [`BUTTON_PAD_X`]: the
    /// line box adds its own vertical whitespace, so a little goes a long way.
    pub const BUTTON_PAD_Y: f32 = 0.36;
    /// Button icon height (floored at 1px).
    pub const BUTTON_ICON: f32 = 0.95;
    /// Gap between the button's icon and label (floored at 2px).
    pub const BUTTON_ICON_GAP: f32 = 0.38;
    /// Button corner radius.
    pub const BUTTON_RADIUS: f32 = 0.45;
    /// Button border thickness (floored at 1px).
    pub const BUTTON_BORDER: f32 = 0.1;
    /// Button background fill alpha (a barely-there wash).
    pub const BUTTON_FILL_ALPHA: f32 = 0.07;
    /// Button border alpha — white layered *over* the card (via [`super::Canvas::over`]), so it
    /// fades from invisible (0) through a faint hairline (~0.2) to a solid white outline (1).
    /// Tune to taste; low reads subtle.
    pub const BUTTON_BORDER_ALPHA: f32 = 0.2;
}

use tokens::{SHADOW, SHADOW_ALPHA, TEXT, TEXT_DIM};

/// Re-exported at the HUD root so existing `hud::BG` call sites keep resolving.
pub use tokens::BG;

/// A panel background at `opacity_pct` (0–100) percent — black at the given alpha.
/// Drives the "info panel opacity" setting for the info / EXIF panels.
pub fn bg_for_opacity(opacity_pct: u8) -> [u8; 4] {
    let a = ((opacity_pct.min(100) as f32 / 100.0) * 255.0).round() as u8;
    [0, 0, 0, a]
}

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

/// Which end of a string to keep when eliding it to fit a width ([`Hud::fit_text`]).
#[derive(Clone, Copy)]
enum Keep {
    /// Keep the head, ellipsize the tail: `"Long Name…"` (a heading).
    Start,
    /// Keep the tail, ellipsize the head: `"…/leaf"` (a path — the leaf is the useful part).
    End,
}

/// Faux-bold smear (horizontal dilation) as a fraction of the text px height, applied
/// only when no *real* heavier face is available: modern macOS ships SF Pro purely as a
/// variable font and fontdue can't instance its weight axis, so we synthesize emphasis
/// from SF Pro to keep the HUD one typeface. Mirrors the outline-thickness scale
/// (`px * 0.06`); 0 on Windows/Linux where real Segoe/DejaVu semibold+bold load.
const SEMIBOLD_SMEAR: f32 = 0.030;
const BOLD_SMEAR: f32 = 0.060;

/// A loaded heavier face plus how much faux-bold to add on top of it: `0.0` for a real
/// semibold/bold file (Segoe, DejaVu, an installed static SF Pro weight), `> 0` when the
/// face is actually the *regular* typeface standing in for a missing heavier weight.
struct Face {
    font: fontdue::Font,
    embolden: f32,
}

pub struct Hud {
    font: fontdue::Font,
    /// The semibold face (label column emphasis); falls back to bold then regular.
    semibold: Option<Face>,
    /// The bold face (filename / titles); falls back to semibold then regular.
    bold: Option<Face>,
}

/// One laid-out glyph: its metrics, coverage bitmap, and pen x-offset on the line.
type Glyph = (fontdue::Metrics, Vec<u8>, f32);

/// A laid-out button (from [`Hud::layout_button`]): everything [`Hud::button_size`] and
/// [`Hud::draw_button`] need, computed once so measure and draw agree exactly.
struct ButtonLayout {
    /// The label's glyphs (regular weight).
    glyphs: Vec<Glyph>,
    /// The rasterized leading icon, if any: `(rgba, w, h)`.
    icon: Option<(Vec<u8>, u32, u32)>,
    /// Gap between the icon and the label (0 when there's no icon).
    icon_gap: i32,
    /// Inner padding: left/right and top/bottom (tuned separately, see [`tokens::BUTTON_PAD_X`]).
    pad_x: i32,
    pad_y: i32,
    /// Total button width / height in px.
    w: i32,
    h: i32,
}

impl Hud {
    /// Load the system UI font (and its semibold/bold faces if present). Returns
    /// `None` if no regular font is found, in which case the overlay is disabled.
    pub fn load() -> Option<Hud> {
        // First readable + parseable candidate wins. Each heavier candidate carries the
        // faux-bold amount to apply if *it* is the one that loads (0 for a real heavier
        // face, > 0 when SF Pro stands in for a missing static weight — see `Face`).
        let load_font = |faces: &[(PathBuf, f32)]| -> Option<fontdue::Font> {
            faces.iter().find_map(|(p, _)| {
                std::fs::read(p).ok().and_then(|b| {
                    fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok()
                })
            })
        };
        let load_face = |faces: &[(PathBuf, f32)]| -> Option<Face> {
            faces.iter().find_map(|(p, embolden)| {
                std::fs::read(p)
                    .ok()
                    .and_then(|b| {
                        fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok()
                    })
                    .map(|font| Face {
                        font,
                        embolden: *embolden,
                    })
            })
        };
        let font = load_font(&regular_font_faces())?;
        let semibold = load_face(&semibold_font_faces());
        let bold = load_face(&bold_font_faces());
        Some(Hud {
            font,
            semibold,
            bold,
        })
    }

    /// The face for a given weight plus the faux-bold smear to apply, falling back
    /// gracefully when a heavier face is absent (→ the regular face with a synthetic
    /// smear, so emphasis survives even on a system with only one weight).
    fn font_for(&self, weight: Weight) -> (&fontdue::Font, f32) {
        match weight {
            Weight::Regular => (&self.font, 0.0),
            Weight::Semibold => self
                .semibold
                .as_ref()
                .or(self.bold.as_ref())
                .map(|f| (&f.font, f.embolden))
                .unwrap_or((&self.font, SEMIBOLD_SMEAR)),
            Weight::Bold => self
                .bold
                .as_ref()
                .or(self.semibold.as_ref())
                .map(|f| (&f.font, f.embolden))
                .unwrap_or((&self.font, BOLD_SMEAR)),
        }
    }

    /// Rasterize one line into the translucent panel with white, outlined text.
    /// Used for the basic `i` overlay. Returns `(rgba, w, h)`.
    pub fn render_panel(
        &self,
        text: &str,
        px: f32,
        pad: u32,
        bg: [u8; 4],
    ) -> Option<(Vec<u8>, u32, u32)> {
        self.render_panel_icon(text, px, pad, None, bg)
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
        bg: [u8; 4],
    ) -> Option<(Vec<u8>, u32, u32)> {
        let line_h = self.line_height(px)?;
        let icon_h = (px * tokens::PILL_ICON).round().max(1.0) as u32;
        let rasterized = icon.and_then(|svg| crate::icon::rasterize(svg, icon_h, TEXT));

        // Icon-only pill (empty message): a perfectly square rounded rect with the
        // icon centered on both axes — e.g. the rotate toasts. The side matches a
        // text pill's height so icon-only and text toasts read at the same scale.
        if text.is_empty() {
            let (rgba, iw, ih) = rasterized.as_ref()?;
            let side = line_h + 2 * pad;
            let mut canvas = Canvas::new(side, side, bg, (px * tokens::RADIUS_PANEL).round());
            let ix = (side as i32 - *iw as i32) / 2;
            let iy = (side as i32 - *ih as i32) / 2;
            self.draw_icon(&mut canvas, rgba, *iw, *ih, ix, iy, px);
            return Some((canvas.into_rgba(), side, side));
        }

        let (glyphs, advance) = self.layout(text, px, Weight::Regular);
        let (icon_w, gap) = match &rasterized {
            Some((_, w, _)) => (*w, (px * tokens::PILL_ICON_GAP).round().max(3.0) as u32),
            None => (0, 0),
        };

        // Wider left/right inset than the vertical `pad` so the sides don't read tight against
        // the text (the line box pads the top/bottom for free; the sides don't). See `PAD_X`.
        let pad_x = ((pad as f32) * tokens::PAD_X).round() as u32;
        let text_x = pad_x + icon_w + gap;
        let pw = text_x + advance.ceil() as u32 + pad_x;
        let ph = line_h + 2 * pad;
        let mut canvas = Canvas::new(pw, ph, bg, (px * tokens::RADIUS_PANEL).round());

        if let Some((rgba, iw, ih)) = &rasterized {
            // Vertically center the icon on the text line.
            let iy = pad as i32 + (line_h as i32 - *ih as i32) / 2;
            self.draw_icon(&mut canvas, rgba, *iw, *ih, pad_x as i32, iy, px);
        }
        let baseline = pad as f32 + self.ascent(px)?;
        self.draw_line(&mut canvas, text_x as f32, baseline, &glyphs, TEXT, px);
        Some((canvas.into_rgba(), pw, ph))
    }

    /// Rasterize `rows` into a two-column table inside the translucent panel: a
    /// semibold label column + a regular value column, with full-width `Span` rows
    /// on top. Used for the full-EXIF "nerd" panel (tasks.json #5) and help overlay.
    pub fn render_table(
        &self,
        rows: &[Row],
        px: f32,
        pad: u32,
        bg: [u8; 4],
    ) -> Option<(Vec<u8>, u32, u32)> {
        if rows.is_empty() {
            return None;
        }
        let line_h = self.line_height(px)?;
        let ascent = self.ascent(px)?;
        // Gap between the label column and the value column.
        let col_gap = (px * tokens::TABLE_COL_GAP).round().max(6.0);

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

        // Wider left/right inset than the vertical `pad` (the line box already pads the
        // top/bottom); the columns start at `pad_x`. See `PAD_X`.
        let pad_x = ((pad as f32) * tokens::PAD_X).round();
        let has_pairs = label_w > 0.0;
        let value_x = pad_x + label_w + if has_pairs { col_gap } else { 0.0 };
        let content_w = (value_x - pad_x + value_w).max(span_w);
        let pw = content_w.ceil() as u32 + 2 * pad_x as u32;
        let ph = rows.len() as u32 * line_h + 2 * pad;
        let mut canvas = Canvas::new(pw, ph, bg, (px * tokens::RADIUS_PANEL).round());

        for (i, item) in laid.iter().enumerate() {
            let row_top = pad as f32 + i as f32 * line_h as f32;
            let baseline = row_top + ascent;
            match item {
                Laid::Span(g) => {
                    self.draw_line(&mut canvas, pad_x, baseline, g, TEXT, px);
                }
                Laid::Pair(lg, vg) => {
                    self.draw_line(&mut canvas, pad_x, baseline, lg, TEXT, px);
                    self.draw_line(&mut canvas, value_x, baseline, vg, TEXT, px);
                }
            }
        }
        Some((canvas.into_rgba(), pw, ph))
    }

    /// Rasterize `lines` as **centered** text inside the translucent panel — each
    /// line horizontally centered within the panel (which is itself centered on
    /// screen). Used for the empty-state "Press O to open…" hint, where a left-aligned
    /// block looks off-center. Returns `(rgba, w, h)`; `None` if no font / empty.
    pub fn render_centered(
        &self,
        lines: &[&str],
        px: f32,
        pad: u32,
        bg: [u8; 4],
    ) -> Option<(Vec<u8>, u32, u32)> {
        if lines.is_empty() {
            return None;
        }
        let line_h = self.line_height(px)?;
        let ascent = self.ascent(px)?;
        // Lay every line out once; the widest sets the panel's content width.
        let laid: Vec<(Vec<Glyph>, f32)> = lines
            .iter()
            .map(|t| self.layout(t, px, Weight::Regular))
            .collect();
        let content_w = laid.iter().map(|(_, adv)| *adv).fold(0.0f32, f32::max);
        // Wider left/right inset than the vertical `pad` (the line box pads top/bottom). See
        // `PAD_X`.
        let pad_x = ((pad as f32) * tokens::PAD_X).round();
        let pw = content_w.ceil() as u32 + 2 * pad_x as u32;
        let ph = lines.len() as u32 * line_h + 2 * pad;
        let mut canvas = Canvas::new(pw, ph, bg, (px * tokens::RADIUS_PANEL).round());
        for (i, (glyphs, adv)) in laid.iter().enumerate() {
            let baseline = pad as f32 + i as f32 * line_h as f32 + ascent;
            // Center this line within the content box.
            let x = pad_x + (content_w - adv) * 0.5;
            self.draw_line(&mut canvas, x, baseline, glyphs, TEXT, px);
        }
        Some((canvas.into_rgba(), pw, ph))
    }

    /// Rasterize the **scan status card** — the ambient overlay shown while a folder streams
    /// in. A **fixed-width** card (so it doesn't jitter as the count or the live path change),
    /// **center-aligned**, with: a semibold heading (`Scanning "Folder"`), the folder currently
    /// being walked (`path_line`, dim — empty to omit it; **left-elided** to fit), the count
    /// (`8,230 images found`, dim), and a centered, subtly-bordered **button** (a stop icon +
    /// `button_label`) whose corner radius is a touch tighter than the card's. Returns
    /// `(rgba, w, h, button_rect)` where `button_rect` is the **button's** `[x, y, w, h]` within
    /// the card (the only click target — the caller offsets it to screen px).
    #[allow(clippy::too_many_arguments)]
    pub fn render_scan_card(
        &self,
        heading: &str,
        path_line: &str,
        count_line: &str,
        button_label: &str,
        button_icon: &str,
        px: f32,
        width: u32,
        bg: [u8; 4],
    ) -> Option<(Vec<u8>, u32, u32, [u32; 4])> {
        let px_sub = (px * tokens::CARD_SUB).max(1.0);
        let pad = (px * tokens::CARD_PAD).round().max(6.0) as i32;
        let cw = width.max(1) as i32;
        let avail = (cw - 2 * pad).max(1) as f32;
        let card_r = (px * tokens::RADIUS_CARD).round();

        // Text runs, each elided to the fixed width: the heading keeps its start; the live
        // path keeps its *tail* (the leaf folder is the useful bit).
        let heading = self.fit_text(heading, px, Weight::Semibold, avail, Keep::Start);
        let (head_g, head_adv) = self.layout(&heading, px, Weight::Semibold);
        let has_path = !path_line.is_empty();
        let path = self.fit_text(path_line, px_sub, Weight::Regular, avail, Keep::End);
        let (path_g, path_adv) = self.layout(&path, px_sub, Weight::Regular);
        let (count_g, count_adv) = self.layout(count_line, px_sub, Weight::Regular);
        let head_lh = self.line_height(px)? as i32;
        let sub_lh = self.line_height(px_sub)? as i32;
        let head_asc = self.ascent(px)?;
        let sub_asc = self.ascent(px_sub)?;

        // The Cancel button is a shared HUD primitive ([`draw_button`]); measure it now so we
        // can size the card's height and horizontally center it.
        let (bw, bh) = self.button_size(button_label, Some(button_icon), px_sub)?;
        let (bw, bh) = (bw as i32, bh as i32);

        // Vertical rhythm.
        let gap_lines = (px * tokens::CARD_GAP_LINES).round() as i32;
        let gap_button = (px * tokens::CARD_GAP_BUTTON).round().max(4.0) as i32;
        let path_block = if has_path { sub_lh + gap_lines } else { 0 };
        let ch = (pad + head_lh + gap_lines + path_block + sub_lh + gap_button + bh + pad).max(1);

        let mut canvas = Canvas::new(cw as u32, ch as u32, bg, card_r);
        let center = |adv: f32| (cw as f32 - adv) * 0.5;

        // Heading, (optional) path, count — all centered.
        let mut y = pad;
        self.draw_line(
            &mut canvas,
            center(head_adv),
            y as f32 + head_asc,
            &head_g,
            TEXT,
            px,
        );
        y += head_lh + gap_lines;
        if has_path {
            self.draw_line(
                &mut canvas,
                center(path_adv),
                y as f32 + sub_asc,
                &path_g,
                TEXT_DIM,
                px_sub,
            );
            y += sub_lh + gap_lines;
        }
        self.draw_line(
            &mut canvas,
            center(count_adv),
            y as f32 + sub_asc,
            &count_g,
            TEXT_DIM,
            px_sub,
        );
        y += sub_lh + gap_button;

        // Centered button (stop icon + label, faint fill, subtle border).
        let bx = (cw - bw) / 2;
        let button_rect =
            self.draw_button(&mut canvas, bx, y, button_label, Some(button_icon), px_sub)?;
        Some((canvas.into_rgba(), cw as u32, ch as u32, button_rect))
    }

    /// Measure the reusable HUD **button**: the `(w, h)` a button with `label` (plus an
    /// optional leading `icon`) occupies at text size `px`. Pair with [`draw_button`] —
    /// measure to *place* (e.g. the scan card centers it with this width), then draw.
    pub fn button_size(&self, label: &str, icon: Option<&str>, px: f32) -> Option<(u32, u32)> {
        let b = self.layout_button(label, icon, px)?;
        Some((b.w.max(0) as u32, b.h.max(0) as u32))
    }

    /// Draw the reusable HUD **button** into `canvas` at `(x, y)` — a faint fill, a subtle
    /// rounded border, then the optional leading icon and the `label`, all at text size `px`.
    /// Returns its `[x, y, w, h]` rect (the click target).
    ///
    /// Draws **directly into the destination** canvas — the fill and the border ([`tokens`]
    /// `BUTTON_FILL_ALPHA` / `BUTTON_BORDER_ALPHA`) composite *over* the panel it lands on, so
    /// they read consistently on the scan card or a `BG` swatch. (For a freestanding swatch use
    /// [`render_button`], which supplies that backing canvas.)
    fn draw_button(
        &self,
        canvas: &mut Canvas,
        x: i32,
        y: i32,
        label: &str,
        icon: Option<&str>,
        px: f32,
    ) -> Option<[u32; 4]> {
        let b = self.layout_button(label, icon, px)?;
        let asc = self.ascent(px)?;
        let r = (px * tokens::BUTTON_RADIUS).round();
        canvas.fill_round_rect(x, y, b.w, b.h, r, TEXT, tokens::BUTTON_FILL_ALPHA);
        let t = (px * tokens::BUTTON_BORDER).round().max(1.0) as i32;
        canvas.stroke_round_rect(x, y, b.w, b.h, r, t, TEXT, tokens::BUTTON_BORDER_ALPHA);
        let mut cx = x + b.pad_x;
        if let Some((rgba, iw, ih)) = &b.icon {
            let iy = y + (b.h - *ih as i32) / 2;
            self.draw_icon(canvas, rgba, *iw, *ih, cx, iy, px);
            cx += *iw as i32 + b.icon_gap;
        }
        self.draw_line(
            canvas,
            cx as f32,
            (y + b.pad_y) as f32 + asc,
            &b.glyphs,
            TEXT,
            px,
        );
        Some([
            x.max(0) as u32,
            y.max(0) as u32,
            b.w.max(0) as u32,
            b.h.max(0) as u32,
        ])
    }

    /// Render a freestanding HUD **button swatch** to its own translucent pill bitmap — for
    /// the HUD gallery, which previews button variants in isolation. The swatch *is* its own
    /// canvas (filled with `bg`), so the fill and border land on the same backing they'd have
    /// on a card. Returns `(rgba, w, h)`.
    pub fn render_button(
        &self,
        label: &str,
        icon: Option<&str>,
        px: f32,
        bg: [u8; 4],
    ) -> Option<(Vec<u8>, u32, u32)> {
        let (w, h) = self.button_size(label, icon, px)?;
        let mut canvas = Canvas::new(w, h, bg, (px * tokens::BUTTON_RADIUS).round());
        self.draw_button(&mut canvas, 0, 0, label, icon, px)?;
        Some((canvas.into_rgba(), w, h))
    }

    /// Lay out a button once — its label glyphs, rasterized icon, paddings, and resulting
    /// `(w, h)` — shared by [`button_size`] and [`draw_button`] so measure and draw can never
    /// disagree. All sizing flows from the button's own text height `px` via the `BUTTON_*`
    /// [`tokens`], so a button is self-contained: the same call reads identically on a card,
    /// in a toast, or as a gallery swatch.
    fn layout_button(&self, label: &str, icon: Option<&str>, px: f32) -> Option<ButtonLayout> {
        let (glyphs, label_adv) = self.layout(label, px, Weight::Regular);
        let line_h = self.line_height(px)? as i32;
        let pad_x = (px * tokens::BUTTON_PAD_X).round().max(3.0) as i32;
        let pad_y = (px * tokens::BUTTON_PAD_Y).round().max(2.0) as i32;
        let icon = icon.and_then(|svg| {
            let h = (px * tokens::BUTTON_ICON).round().max(1.0) as u32;
            crate::icon::rasterize(svg, h, TEXT)
        });
        let (icon_w, icon_gap) = match &icon {
            Some((_, w, _)) => (
                *w as i32,
                (px * tokens::BUTTON_ICON_GAP).round().max(2.0) as i32,
            ),
            None => (0, 0),
        };
        let w = icon_w + icon_gap + label_adv.ceil() as i32 + 2 * pad_x;
        let h = line_h + 2 * pad_y;
        Some(ButtonLayout {
            glyphs,
            icon,
            icon_gap,
            pad_x,
            pad_y,
            w,
            h,
        })
    }

    /// Shorten `text` so it lays out within `max_w` px, inserting an ellipsis on the dropped
    /// end ([`Keep::Start`] keeps the head — `"Long Name…"`; [`Keep::End`] keeps the tail —
    /// `"…/leaf"`, right for a path). Returns the original when it already fits.
    fn fit_text(&self, text: &str, px: f32, weight: Weight, max_w: f32, keep: Keep) -> String {
        let (_, full) = self.layout(text, px, weight);
        if full <= max_w {
            return text.to_string();
        }
        let chars: Vec<char> = text.chars().collect();
        let ell = '\u{2026}';
        let fits = |s: &str| self.layout(s, px, weight).1 <= max_w;
        match keep {
            Keep::Start => {
                for end in (1..chars.len()).rev() {
                    let cand: String = chars[..end].iter().collect::<String>() + &ell.to_string();
                    if fits(&cand) {
                        return cand;
                    }
                }
            }
            Keep::End => {
                for start in 1..chars.len() {
                    let cand: String = ell.to_string() + &chars[start..].iter().collect::<String>();
                    if fits(&cand) {
                        return cand;
                    }
                }
            }
        }
        ell.to_string()
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
        let (font, embolden) = self.font_for(weight);
        // Faux-bold width in whole px (≥1 when synthesizing), 0 for a real heavier face.
        let extra = if embolden > 0.0 {
            (px * embolden).round().max(1.0) as usize
        } else {
            0
        };
        let mut glyphs = Vec::new();
        let mut pen = 0.0f32;
        for ch in text.chars() {
            let (m, bitmap) = font.rasterize(ch, px);
            let (m, bitmap) = if extra > 0 {
                embolden_glyph(&m, &bitmap, extra)
            } else {
                (m, bitmap)
            };
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
        let s = (px * tokens::OUTLINE).round().max(1.0) as i32; // outline thickness
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
        let s = (px * tokens::OUTLINE).round().max(1.0) as i32; // outline thickness (matches text)
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

    /// Composite a filled rounded rect (the scan card's subtle button fill): the box
    /// `[x0, y0, w, h]` with `r`-px rounded corners, in `rgb` at `alpha`, AA at the corners.
    #[allow(clippy::too_many_arguments)]
    fn fill_round_rect(
        &mut self,
        x0: i32,
        y0: i32,
        w: i32,
        h: i32,
        r: f32,
        rgb: [u8; 3],
        alpha: f32,
    ) {
        for py in 0..h {
            for px_ in 0..w {
                let cov = corner_coverage(px_, py, w, h, r);
                if cov > 0.0 {
                    self.over(x0 + px_, y0 + py, rgb, cov * alpha);
                }
            }
        }
    }

    /// Draw a rounded-rect **outline** of `t`-px thickness (the button border): the ring between
    /// the box `[x0, y0, w, h]` (radius `r`) and the same box inset by `t` (radius `r - t`),
    /// composited **over** the destination in `(rgb, alpha)`, anti-aliased at the corners
    /// (coverage = outer − inner). Layering over the panel — rather than *setting* the ring's
    /// alpha — keeps `alpha` intuitive: it fades the border from invisible (0) to a solid line
    /// (1) against the card. (Setting the alpha instead carved a translucent hole that showed
    /// the brighter background through the border, so it never read subtle no matter how low.)
    #[allow(clippy::too_many_arguments)]
    fn stroke_round_rect(
        &mut self,
        x0: i32,
        y0: i32,
        w: i32,
        h: i32,
        r: f32,
        t: i32,
        rgb: [u8; 3],
        alpha: f32,
    ) {
        let (iw, ih) = (w - 2 * t, h - 2 * t);
        let inner_r = (r - t as f32).max(0.0);
        for py in 0..h {
            for px_ in 0..w {
                let outer = corner_coverage(px_, py, w, h, r);
                if outer <= 0.0 {
                    continue;
                }
                let inner = if iw > 0 && ih > 0 {
                    corner_coverage(px_ - t, py - t, iw, ih, inner_r)
                } else {
                    0.0
                };
                let ring = (outer - inner).clamp(0.0, 1.0);
                if ring > 0.0 {
                    self.over(x0 + px_, y0 + py, rgb, alpha * ring);
                }
            }
        }
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

/// Faux-bold a coverage bitmap by horizontal dilation — `out[x] = max(src[x-extra..=x])`
/// — widening the glyph `extra` px to the right (advance bumped to match, so spacing
/// holds). Done at layout time, before the outline pass, so the legibility halo wraps
/// the bolder shape. Used only when synthesizing a heavier weight from the regular face
/// (see [`Face`] / [`SEMIBOLD_SMEAR`]). A real semibold/bold face passes `extra == 0`.
fn embolden_glyph(
    m: &fontdue::Metrics,
    bitmap: &[u8],
    extra: usize,
) -> (fontdue::Metrics, Vec<u8>) {
    if extra == 0 || m.width == 0 || m.height == 0 {
        return (*m, bitmap.to_vec());
    }
    let new_w = m.width + extra;
    let mut out = vec![0u8; new_w * m.height];
    for y in 0..m.height {
        let src = &bitmap[y * m.width..(y + 1) * m.width];
        let dst = &mut out[y * new_w..(y + 1) * new_w];
        for (x, d) in dst.iter_mut().enumerate() {
            // Max of the source over the window [x-extra, x], clamped to the source row.
            let lo = x.saturating_sub(extra);
            let hi = x.min(m.width - 1);
            *d = src[lo..=hi].iter().copied().max().unwrap_or(0);
        }
    }
    let mut nm = *m;
    nm.width = new_w;
    nm.advance_width += extra as f32;
    (nm, out)
}

/// The Windows fonts directory (from `WINDIR`/`SystemRoot`). Windows-only: macOS and
/// Linux use absolute font paths directly in the `*_font_faces()` helpers below, so
/// this would be dead code there.
#[cfg(windows)]
fn fonts_dir() -> PathBuf {
    let windir = std::env::var("WINDIR")
        .or_else(|_| std::env::var("SystemRoot"))
        .unwrap_or_else(|_| "C:\\Windows".to_string());
    PathBuf::from(windir).join("Fonts")
}

/// Regular-face candidates as `(path, faux-bold)` — always `0.0` for the body weight.
fn regular_font_faces() -> Vec<(PathBuf, f32)> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        let f = fonts_dir();
        v.push((f.join("segoeui.ttf"), 0.0));
        v.push((f.join("arial.ttf"), 0.0));
    }
    #[cfg(target_os = "macos")]
    {
        // SF Pro — the macOS system UI font (variable; ships on every modern macOS).
        v.push((PathBuf::from("/System/Library/Fonts/SFNS.ttf"), 0.0));
        v.push((
            PathBuf::from("/System/Library/Fonts/Supplemental/Arial.ttf"),
            0.0,
        ));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        v.push((
            PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
            0.0,
        ));
        v.push((PathBuf::from("/usr/share/fonts/TTF/DejaVuSans.ttf"), 0.0));
    }
    v
}

/// Semibold candidates as `(path, faux-bold)`: a real face carries `0.0`; SF Pro stands
/// in with [`SEMIBOLD_SMEAR`] because modern macOS ships no static SF semibold.
fn semibold_font_faces() -> Vec<(PathBuf, f32)> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        let f = fonts_dir();
        v.push((f.join("seguisb.ttf"), 0.0)); // Segoe UI Semibold (real)
        v.push((f.join("segoeuib.ttf"), 0.0)); // fall back to real Bold
    }
    #[cfg(target_os = "macos")]
    {
        // Prefer a real static SF Pro Text Semibold if a dev/user installed Apple's SF
        // Pro family; otherwise synthesize semibold from the variable SF Pro.
        v.push((
            PathBuf::from("/Library/Fonts/SF-Pro-Text-Semibold.otf"),
            0.0,
        ));
        v.push((
            PathBuf::from("/System/Library/Fonts/SFNS.ttf"),
            SEMIBOLD_SMEAR,
        ));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        v.push((
            PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"),
            0.0,
        ));
    }
    v
}

/// Bold candidates as `(path, faux-bold)`: a real face carries `0.0`; SF Pro stands in
/// with [`BOLD_SMEAR`] because modern macOS ships no static SF bold.
fn bold_font_faces() -> Vec<(PathBuf, f32)> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        let f = fonts_dir();
        v.push((f.join("segoeuib.ttf"), 0.0));
        v.push((f.join("arialbd.ttf"), 0.0));
    }
    #[cfg(target_os = "macos")]
    {
        v.push((PathBuf::from("/Library/Fonts/SF-Pro-Text-Bold.otf"), 0.0));
        v.push((PathBuf::from("/System/Library/Fonts/SFNS.ttf"), BOLD_SMEAR));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        v.push((
            PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"),
            0.0,
        ));
        v.push((
            PathBuf::from("/usr/share/fonts/TTF/DejaVuSans-Bold.ttf"),
            0.0,
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embolden_widens_glyph_and_advance() {
        // A 1px-wide, 2-row vertical stroke at full coverage.
        let m = fontdue::Metrics {
            width: 1,
            height: 2,
            advance_width: 5.0,
            ..Default::default()
        };
        let bitmap = vec![255u8, 255];
        let (nm, out) = embolden_glyph(&m, &bitmap, 1);
        assert_eq!(nm.width, 2, "grew 1px to the right");
        assert_eq!(nm.advance_width, 6.0, "advance bumped to keep spacing");
        // The source pixel smears into the new column → both columns now covered.
        assert_eq!(out, vec![255, 255, 255, 255]);
    }

    #[test]
    fn embolden_zero_extra_is_identity() {
        let m = fontdue::Metrics {
            width: 2,
            height: 1,
            advance_width: 4.0,
            ..Default::default()
        };
        let bitmap = vec![10u8, 20];
        let (nm, out) = embolden_glyph(&m, &bitmap, 0);
        assert_eq!(nm.width, 2);
        assert_eq!(nm.advance_width, 4.0);
        assert_eq!(out, bitmap);
    }

    #[test]
    fn embolden_empty_glyph_is_safe() {
        // A space-like glyph (no pixels): emboldening must not panic or allocate pixels.
        let m = fontdue::Metrics {
            width: 0,
            height: 0,
            advance_width: 7.0,
            ..Default::default()
        };
        let (nm, out) = embolden_glyph(&m, &[], 2);
        assert_eq!(nm.width, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn embolden_takes_window_max_not_sum() {
        // Coverage is a max over the window (no overflow/brightening past 255).
        let m = fontdue::Metrics {
            width: 3,
            height: 1,
            advance_width: 3.0,
            ..Default::default()
        };
        let bitmap = vec![200u8, 0, 100];
        let (_, out) = embolden_glyph(&m, &bitmap, 1);
        // out[x] = max(src[x-1], src[x]); trailing col picks the last source pixel.
        assert_eq!(out, vec![200, 200, 100, 100]);
    }

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
