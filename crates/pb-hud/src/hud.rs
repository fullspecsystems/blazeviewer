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

    // ── Keyboard-shortcuts overlay (the `?` help) ────────────────────────────────────
    /// The "/" separator between a pair of shortcuts (e.g. `- / =`) is drawn in this dimmer
    /// gray — dimmer than the keys ([`TEXT_DIM`]) — so the two keys read as the emphasis.
    pub const SHORTCUT_SEP: [u8; 3] = [120, 123, 130];
    /// Translucent-white band behind each section heading, for visual separation.
    pub const SECTION_BAND_ALPHA: f32 = 0.08;

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
    /// Button corner radius, as a fraction of the button's text size (non-macOS —
    /// this token is cfg'd out on Mac; the macOS knob is `BUTTON_RADIUS_OF_HEIGHT`).
    pub const BUTTON_RADIUS: f32 = 0.45;
    /// macOS button corner radius, as a fraction of the button's HEIGHT — Tahoe's
    /// concentric principle (radii derive from control geometry, not point values).
    /// Matches the platform's bordered utility buttons (the screenshot-editor
    /// "Done" — the owner's reference): visibly rounder than classic AppKit,
    /// deliberately NOT a capsule. Sits near the nominal ⅓ (0.40 probe-matched the
    /// reference, but read a touch large in situ; the n = 2.2 gauge in
    /// `corner_distance` cuts slightly less than a circle at equal radius).
    pub const BUTTON_RADIUS_OF_HEIGHT: f32 = 0.33;
    /// Button border thickness (floored at 1px).
    pub const BUTTON_BORDER: f32 = 0.1;
    /// Button background fill alpha (a barely-there wash).
    pub const BUTTON_FILL_ALPHA: f32 = 0.07;
    /// Button border alpha — white layered *over* the card (via [`super::Canvas::over`]), so it
    /// fades from invisible (0) through a faint hairline (~0.2) to a solid white outline (1).
    /// Tune to taste; low reads subtle.
    pub const BUTTON_BORDER_ALPHA: f32 = 0.20;
    /// Button fill alpha when the pointer is **over** it — a stronger wash than
    /// [`BUTTON_FILL_ALPHA`] so the button visibly lights up on hover.
    pub const BUTTON_FILL_ALPHA_HOVER: f32 = 0.15;
    /// Button border alpha on hover — lifted from [`BUTTON_BORDER_ALPHA`] to match the brighter
    /// fill.
    pub const BUTTON_BORDER_ALPHA_HOVER: f32 = 0.25;
    /// Minimum gap between the button's label and a trailing shortcut hint (floored at 6px). A
    /// shortcut is drawn dimmed and **right-aligned** in the button — menu-accelerator style —
    /// so when the button is wider than its content the gap grows to push the hint to the edge.
    pub const BUTTON_SHORTCUT_GAP: f32 = 0.9;

    // ── Open-screen call to action (two stacked buttons) ──────────────────────────────
    /// Vertical gap between the stacked "Open File" / "Open Folder" buttons (floored at 4px).
    pub const OPEN_BUTTON_GAP: f32 = 0.45;

    // ── Folder-tree overlay (`Shift+F`) ──────────────────────────────────────────────
    /// Horizontal indent per tree depth level.
    pub const TREE_INDENT: f32 = 1.15;
    /// Folder icon height, relative to the text.
    pub const TREE_ICON: f32 = 0.85;
    /// Gap between a row's folder icon and its name (floored at 3px).
    pub const TREE_ICON_GAP: f32 = 0.40;
    /// Translucent-white highlight band behind the current folder's row.
    pub const TREE_CURRENT_ALPHA: f32 = 0.14;
    /// Highlight band behind the hovered clickable row — lighter than the current
    /// band (the two layer when the current row is hovered, so it still reads).
    pub const TREE_HOVER_ALPHA: f32 = 0.10;
    /// Max width of a folder name before it's tail-elided, in text heights.
    pub const TREE_NAME_MAX: f32 = 16.0;
    /// Photo-count text size, relative to the row text.
    pub const TREE_COUNT: f32 = 0.72;
    /// Capsule badge fill alpha. The fill is the theme's **shadow** color (near-
    /// solid black on the dark panel, white on light), so the badge reads as its
    /// own contrasty pill rather than a faint wash (owner call, 2026-07-03).
    pub const TREE_BADGE_ALPHA: f32 = 0.85;
}

use tokens::SHADOW_ALPHA;

/// Re-exported at the HUD root so existing `hud::BG` call sites keep resolving.
/// This is the **dark** panel background — theme-aware callers should reach for
/// [`Theme::of`]`(dark).bg` / [`Hud::theme`] instead.
pub use tokens::BG;

/// A panel background at `opacity_pct` (0–100) percent — black at the given alpha.
/// The dark-theme legacy helper; theme-aware callers use [`Theme::bg_for_opacity`].
pub fn bg_for_opacity(opacity_pct: u8) -> [u8; 4] {
    Theme::DARK.bg_for_opacity(opacity_pct)
}

/// The HUD's resolved **color scheme** (task #46): the five color roles every overlay
/// shares, in dark and light variants. Held on the [`Hud`] (set once by the app core's
/// theme resolution), so every `render_*` call composites with the active scheme — the
/// geometry tokens above stay theme-independent. [`Theme::DARK`] is pinned to the
/// legacy `tokens` constants, so the dark HUD is pixel-identical to the pre-theme look.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Theme {
    /// Standard panel background (toasts, help, tree, scan card, open buttons).
    pub bg: [u8; 4],
    /// Primary text + icon color.
    pub text: [u8; 3],
    /// Secondary/dim text on a panel (counts, shortcut hints, collapsed-tree markers).
    pub text_dim: [u8; 3],
    /// The legibility halo around glyphs and icons (dark: black; light: white).
    pub shadow: [u8; 3],
    /// The extra-dim " / " separator between paired shortcuts.
    pub shortcut_sep: [u8; 3],
}

impl Theme {
    /// The classic dark HUD: white outlined text on translucent black.
    pub const DARK: Theme = Theme {
        bg: tokens::BG,
        text: tokens::TEXT,
        text_dim: tokens::TEXT_DIM,
        shadow: tokens::SHADOW,
        shortcut_sep: tokens::SHORTCUT_SEP,
    };
    /// The light HUD: black text with a white halo on a translucent near-white panel.
    /// The grays mirror the dark roles' contrast against their panel.
    pub const LIGHT: Theme = Theme {
        bg: [244, 245, 247, 153],
        text: [0, 0, 0],
        text_dim: [88, 92, 100],
        shadow: [255, 255, 255],
        shortcut_sep: [145, 148, 155],
    };

    /// The scheme for a resolved dark/light flag.
    pub fn of(dark: bool) -> Theme {
        if dark {
            Theme::DARK
        } else {
            Theme::LIGHT
        }
    }

    /// A panel background at `opacity_pct` (0–100) percent — this theme's panel color
    /// at the given alpha. Drives the "info panel opacity" setting.
    pub fn bg_for_opacity(&self, opacity_pct: u8) -> [u8; 4] {
        let a = ((opacity_pct.min(100) as f32 / 100.0) * 255.0).round() as u8;
        [self.bg[0], self.bg[1], self.bg[2], a]
    }
}

/// A row of the table layout ([`Hud::render_table`]).
pub enum Row {
    /// A full-width line spanning both columns — used for the filename (bold) /
    /// path header and section titles.
    Span { text: String, bold: bool },
    /// A two-column row: a semibold label on the left + a regular value.
    Pair { label: String, value: String },
}

/// One section of the keyboard-shortcuts overlay ([`Hud::render_shortcuts`]): a semibold
/// heading over its rows. Each row is `(description, shortcut)`; the description is left-aligned
/// (white) and the shortcut is right-aligned and dimmed (menu-accelerator style). An empty
/// shortcut renders the description alone.
pub struct ShortcutSection {
    pub title: String,
    pub rows: Vec<(String, String)>,
}

/// One row of the folder-tree overlay ([`Hud::render_tree`]): a folder name at `depth`
/// indent levels. `open` rows draw the open-folder glyph (the current folder and its
/// ancestors); `current` marks the folder containing the on-screen photo (highlight
/// band + semibold); a `marker` row is a dim, glyph-less stand-in for collapsed
/// levels (a deep ancestor chain's "…"); an `up` row is the "open the parent"
/// affordance above the root (folder-with-up-arrow glyph); `count` is an optional
/// photo count rendered per [`TreeCounts`]. Pure data, so the app core can build and
/// unit-test row lists without touching the rasterizer.
pub struct TreeRow {
    pub depth: u32,
    pub name: String,
    pub open: bool,
    pub current: bool,
    pub marker: bool,
    pub up: bool,
    pub count: Option<u64>,
}

/// What a point in the folder-tree panel hits: a folder row (an index into the
/// caller's row list — the caller decides clickability from its click targets), or
/// one of the "… n more" windowing markers, which **page** the visible window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TreeHit {
    Row(usize),
    PageUp,
    PageDown,
}

/// How a row's photo `count` renders — two candidate looks A/B'd in the gallery.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TreeCounts {
    /// A small capsule badge just left of the folder glyph.
    Capsule,
    /// Dim right-aligned numbers at the panel's right edge (the sidebar idiom).
    Trailing,
}

/// The rasterized folder tree: `(rgba, w, h)` plus the interactive hit rects —
/// `[x, y, w, h]` **within the bitmap**; the caller offsets them to screen px.
pub type TreeBitmap = (Vec<u8>, u32, u32, Vec<(TreeHit, [u32; 4])>);

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
    /// The active color scheme (task #46). Defaults to [`Theme::DARK`]; the app core's
    /// theme resolution sets it, and every `render_*` call composites with it.
    theme: Theme,
}

/// One laid-out glyph: its metrics, coverage bitmap, and pen x-offset on the line.
type Glyph = (fontdue::Metrics, Vec<u8>, f32);

/// The rasterized open panel from [`Hud::render_open_panel`]: the bitmap `(rgba, w, h)` plus
/// each button's `[x, y, w, h]` rect within it (file, then folder — the click targets).
pub type OpenPanelBitmap = (Vec<u8>, u32, u32, [u32; 4], [u32; 4]);

/// A HUD **button's content + sizing** — a label, an optional leading icon, and an optional
/// trailing **shortcut hint** (drawn dimmed and right-aligned, menu-accelerator style) — plus a
/// `min_w` so a stack of buttons can share one width. Passed to [`Hud::button_size`] /
/// [`Hud::draw_button`] so measure and draw always agree, and so a new variant is one field
/// rather than another positional argument.
#[derive(Clone, Copy, Default)]
pub struct ButtonSpec<'a> {
    /// The button's text label (regular weight).
    pub label: &'a str,
    /// An optional leading icon — an SVG source from [`crate::icon::assets`].
    pub icon: Option<&'a str>,
    /// An optional trailing shortcut hint: already-formatted accelerator text (e.g. `"O"` /
    /// `"⇧ O"`), drawn dimmed and right-aligned like a menu shortcut — no box, just the text.
    pub shortcut: Option<&'a str>,
    /// Draw the shortcut hint one weight heavier (Semibold instead of Regular), so the dimmed
    /// text keeps a bit more presence. `false` = Regular (matches the label's weight).
    pub shortcut_semibold: bool,
    /// Minimum width in px (`0` = fit content). A larger value keeps the label at the left and
    /// pushes the shortcut to the right edge, so several buttons at a shared width line up.
    pub min_w: i32,
}

/// A laid-out button (from [`Hud::layout_button`]): everything [`Hud::button_size`] and
/// [`Hud::draw_button`] need, computed once so measure and draw agree exactly.
struct ButtonLayout {
    /// The label's glyphs (regular weight).
    glyphs: Vec<Glyph>,
    /// The rasterized leading icon, if any: `(rgba, w, h)`.
    icon: Option<(Vec<u8>, u32, u32)>,
    /// Gap between the icon and the label (0 when there's no icon).
    icon_gap: i32,
    /// The trailing shortcut hint's glyphs + advance width (dimmed, right-aligned). `None` when
    /// there's no shortcut. (The gap before it is implied by the right-alignment against the
    /// inner edge, so it isn't stored — only reserved in `content_w`.)
    shortcut: Option<(Vec<Glyph>, f32)>,
    /// Inner padding: left/right and top/bottom (tuned separately, see [`tokens::BUTTON_PAD_X`]).
    pad_x: i32,
    pad_y: i32,
    /// Natural content width (before any `min_w` expansion) and the final box size.
    content_w: i32,
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
            theme: Theme::DARK,
        })
    }

    /// The active color scheme — call sites read panel backgrounds off it
    /// (`hud.theme().bg`, `hud.theme().bg_for_opacity(..)`).
    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Swap the color scheme (the app core's theme resolution). Cached overlay bitmaps
    /// were composited with the old scheme — the caller rebuilds them.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
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
        let rasterized = icon.and_then(|svg| crate::icon::rasterize(svg, icon_h, self.theme.text));

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
        self.draw_line(
            &mut canvas,
            text_x as f32,
            baseline,
            &glyphs,
            self.theme.text,
            px,
        );
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
                    self.draw_line(&mut canvas, pad_x, baseline, g, self.theme.text, px);
                }
                Laid::Pair(lg, vg) => {
                    self.draw_line(&mut canvas, pad_x, baseline, lg, self.theme.text, px);
                    self.draw_line(&mut canvas, value_x, baseline, vg, self.theme.text, px);
                }
            }
        }
        Some((canvas.into_rgba(), pw, ph))
    }

    /// Rasterize the **keyboard-shortcuts overlay** (the `?` help): `sections`, each a semibold
    /// heading over `(description, shortcut)` rows — description left in white, shortcut
    /// right-aligned and dimmed (menu-accelerator style, matching the open-screen buttons).
    /// Sections pack top-to-bottom into as many **columns** as needed to keep each column within
    /// `max_h` px, so a long list stays on one screen. Returns `(rgba, w, h)`.
    pub fn render_shortcuts(
        &self,
        sections: &[ShortcutSection],
        px: f32,
        bg: [u8; 4],
        max_h: i32,
    ) -> Option<(Vec<u8>, u32, u32)> {
        if sections.is_empty() {
            return None;
        }
        let line_h = self.line_height(px)? as i32;
        let asc = self.ascent(px)?;
        let pad = (px * 1.1).round().max(6.0) as i32; // outer inset
        let col_gap = (px * 2.2).round().max(12.0) as i32; // between columns
        let sec_gap = (px * 0.85).round().max(8.0) as i32; // between sections stacked in a column
        let title_gap = (px * 0.3).round() as i32; // breathing room under a heading band
        let title_pad = (px * 0.2).round().max(1.0) as i32; // vertical padding inside the band
        let band_pad = (px * 0.5).round().max(3.0) as i32; // band overhang past the column edges
        let band_h = line_h + 2 * title_pad;
        let pair_gap = (px * 1.6).round().max(10.0) as i32; // min gap between a description + shortcut

        // One shortcut, split into runs so the "/" separator can be dimmed more than the keys:
        // each run is (glyphs, advance, is_separator).
        type ShortcutRun = (Vec<Glyph>, f32, bool);
        struct LaidRow {
            desc: Vec<Glyph>,
            shortcut: Vec<ShortcutRun>,
            shortcut_w: f32,
        }
        struct Laid {
            title: Vec<Glyph>,
            rows: Vec<LaidRow>,
            w: i32,
            h: i32,
        }
        let mut laid: Vec<Laid> = Vec::with_capacity(sections.len());
        for s in sections {
            let (title, title_w) = self.layout(&s.title, px, Weight::Semibold);
            let mut content_w = title_w;
            let mut rows = Vec::with_capacity(s.rows.len());
            for (desc, sc) in &s.rows {
                let (dg, dw) = self.layout(desc, px, Weight::Regular);
                // Split a combined "a / b" shortcut on its spaced " / " separator so the
                // slash is its own dim run. Splitting on the spaced form (never a bare
                // '/') lets "/" itself be a key: the Help row's "/ / ?" is slash key,
                // dim separator, question-mark key.
                let mut shortcut = Vec::new();
                let mut sw = 0.0f32;
                for (i, part) in sc.split(" / ").enumerate() {
                    if i > 0 {
                        let (g, w) = self.layout(" / ", px, Weight::Semibold);
                        sw += w;
                        shortcut.push((g, w, true));
                    }
                    let (g, w) = self.layout(part, px, Weight::Semibold);
                    sw += w;
                    shortcut.push((g, w, false));
                }
                content_w = content_w.max(dw + pair_gap as f32 + sw);
                rows.push(LaidRow {
                    desc: dg,
                    shortcut,
                    shortcut_w: sw,
                });
            }
            let h = band_h + title_gap + rows.len() as i32 * line_h;
            laid.push(Laid {
                title,
                rows,
                w: content_w.ceil() as i32,
                h,
            });
        }
        let col_w = laid.iter().map(|s| s.w).max().unwrap_or(1);

        // Pack sections into columns, each kept within `max_h`; record each section's (col, y).
        let mut placements: Vec<(usize, i32)> = Vec::with_capacity(laid.len());
        let (mut col, mut y, mut col_h_max) = (0usize, 0i32, 0i32);
        for s in &laid {
            let start = if y == 0 { 0 } else { y + sec_gap };
            if y > 0 && start + s.h > max_h {
                col += 1;
                placements.push((col, 0));
                y = s.h;
            } else {
                placements.push((col, start));
                y = start + s.h;
            }
            col_h_max = col_h_max.max(y);
        }
        let n_cols = col as i32 + 1;

        let total_w = pad + n_cols * col_w + (n_cols - 1) * col_gap + pad;
        let total_h = pad + col_h_max + pad;
        let mut canvas = Canvas::new(
            total_w as u32,
            total_h as u32,
            bg,
            (px * tokens::RADIUS_PANEL).round(),
        );
        for (s, &(c, sy)) in laid.iter().zip(&placements) {
            let col_x = pad + c as i32 * (col_w + col_gap);
            let top = pad + sy;
            // A translucent band behind the heading (full column width + a little overhang).
            canvas.fill_round_rect(
                col_x - band_pad,
                top,
                col_w + 2 * band_pad,
                band_h,
                (px * 0.25).round(),
                self.theme.text,
                tokens::SECTION_BAND_ALPHA,
            );
            // Section heading (semibold, white), vertically centered in the band.
            self.draw_line(
                &mut canvas,
                col_x as f32,
                (top + title_pad) as f32 + asc,
                &s.title,
                self.theme.text,
                px,
            );
            // Rows: description left (white), shortcut right-aligned (keys dimmed, "/" dimmer).
            for (i, r) in s.rows.iter().enumerate() {
                let ry = (top + band_h + title_gap + i as i32 * line_h) as f32 + asc;
                self.draw_line(&mut canvas, col_x as f32, ry, &r.desc, self.theme.text, px);
                let mut sx = (col_x + col_w) as f32 - r.shortcut_w;
                for (glyphs, w, is_sep) in &r.shortcut {
                    let rgb = if *is_sep {
                        self.theme.shortcut_sep
                    } else {
                        self.theme.text_dim
                    };
                    self.draw_line(&mut canvas, sx, ry, glyphs, rgb, px);
                    sx += w;
                }
            }
        }
        Some((canvas.into_rgba(), total_w as u32, total_h as u32))
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
            self.draw_line(&mut canvas, x, baseline, glyphs, self.theme.text, px);
        }
        Some((canvas.into_rgba(), pw, ph))
    }

    /// Rasterize the **folder-tree overlay** (`Shift+F`): an indented list of folder
    /// rows, each with a Font Awesome folder glyph (open for the current folder and
    /// its ancestors, up-arrow for the parent affordance, closed otherwise), hairline
    /// indent guides, optional photo counts (`counts` picks the look), the current
    /// folder highlighted by a translucent band + semibold name, and the `hovered`
    /// hit lit by a lighter band (the click affordance). When the full list would
    /// exceed `max_h`, the rows are windowed around the current folder shifted by
    /// `page`, and the hidden runs collapse into clickable "… n more" markers that
    /// page the window. Returns the bitmap plus the hit rects; `None` on an empty
    /// list / no font.
    #[allow(clippy::too_many_arguments)]
    pub fn render_tree(
        &self,
        rows: &[TreeRow],
        px: f32,
        pad: u32,
        bg: [u8; 4],
        max_h: i32,
        page: i32,
        hovered: Option<TreeHit>,
        counts: TreeCounts,
    ) -> Option<TreeBitmap> {
        if rows.is_empty() {
            return None;
        }
        let line_h = self.line_height(px)? as i32;
        let ascent = self.ascent(px)?;

        // Window the rows to what fits in `max_h`: centered on the current row, then
        // shifted by `page` windows (the clickable "… n more" markers), clamped.
        // When windowing, two lines are reserved for the markers.
        let avail = (((max_h - 2 * pad as i32) / line_h).max(3)) as usize;
        let total = rows.len();
        let (start, end, above, below) = if total <= avail {
            (0, total, 0, 0)
        } else {
            let inner = avail.saturating_sub(2).max(1);
            let cur = rows.iter().position(|r| r.current).unwrap_or(0);
            let auto = cur.saturating_sub(inner / 2).min(total - inner) as i64;
            let start =
                (auto + page as i64 * inner as i64).clamp(0, (total - inner) as i64) as usize;
            (start, start + inner, start, total - (start + inner))
        };

        // The row glyphs, rasterized once; every row's name starts after a fixed
        // icon cell as wide as the widest folder glyph, so names align per depth.
        // The paging carets are dim (they match their marker text) and point the
        // way the click pages — the affordance the plain ellipsis lacked.
        let icon_h = (px * tokens::TREE_ICON).round().max(1.0) as u32;
        let closed = crate::icon::rasterize(crate::icon::assets::FOLDER, icon_h, self.theme.text);
        let open =
            crate::icon::rasterize(crate::icon::assets::FOLDER_OPEN, icon_h, self.theme.text);
        let up_glyph =
            crate::icon::rasterize(crate::icon::assets::FOLDER_UP, icon_h, self.theme.text);
        let caret_up =
            crate::icon::rasterize(crate::icon::assets::CARET_UP, icon_h, self.theme.text_dim);
        let caret_down =
            crate::icon::rasterize(crate::icon::assets::CARET_DOWN, icon_h, self.theme.text_dim);
        let cell = closed
            .iter()
            .chain(open.iter())
            .chain(up_glyph.iter())
            .map(|(_, w, _)| *w as i32)
            .max()
            .unwrap_or(0);
        let icon_gap = (px * tokens::TREE_ICON_GAP).round().max(3.0) as i32;
        let indent = (px * tokens::TREE_INDENT).round() as i32;
        let name_max = px * tokens::TREE_NAME_MAX;
        let count_px = (px * tokens::TREE_COUNT).max(6.0);

        /// Which glyph a line draws.
        #[derive(Clone, Copy)]
        enum Glyf {
            Closed,
            Open,
            Up,
            CaretUp,
            CaretDown,
        }
        // Lay out every visible line once, measuring the content width as we go.
        // A `None` icon slot is a "… n more" marker (dim, no glyph, no indent).
        struct Line {
            glyphs: Vec<Glyph>,
            text_x: i32,
            rgb: [u8; 3],
            band: bool,
            icon: Option<(Glyf, i32)>, // (glyph, icon x before the badge shift)
            hit: Option<TreeHit>,
            count: Option<(Vec<Glyph>, f32)>, // laid-out count text + advance
        }
        let mut lines: Vec<Line> = Vec::with_capacity(end - start + 2);
        let mut content_w = 0i32;
        let mut count_w = 0i32; // widest count text among visible rows
        let push_marker = |lines: &mut Vec<Line>, content_w: &mut i32, n: usize, hit: TreeHit| {
            let (glyphs, adv) = self.layout(&format!("{n} more"), px, Weight::Regular);
            let text_x = cell + icon_gap;
            *content_w = (*content_w).max(text_x + adv.ceil() as i32);
            let glyf = if hit == TreeHit::PageUp {
                Glyf::CaretUp
            } else {
                Glyf::CaretDown
            };
            lines.push(Line {
                glyphs,
                text_x,
                rgb: self.theme.text_dim,
                band: false,
                icon: Some((glyf, 0)),
                hit: Some(hit),
                count: None,
            });
        };
        if above > 0 {
            push_marker(&mut lines, &mut content_w, above, TreeHit::PageUp);
        }
        for (idx, r) in rows.iter().enumerate().take(end).skip(start) {
            let weight = if r.current {
                Weight::Semibold
            } else {
                Weight::Regular
            };
            let x0 = r.depth as i32 * indent;
            let name = self.fit_text(&r.name, px, weight, name_max, Keep::Start);
            let (glyphs, adv) = self.layout(&name, px, weight);
            let text_x = x0 + cell + icon_gap;
            content_w = content_w.max(text_x + adv.ceil() as i32);
            let count = r.count.map(|n| {
                let (g, a) = self.layout(&format_thousands(n), count_px, Weight::Regular);
                count_w = count_w.max(a.ceil() as i32);
                (g, a)
            });
            let glyf = if r.up {
                Glyf::Up
            } else if r.open {
                Glyf::Open
            } else {
                Glyf::Closed
            };
            lines.push(Line {
                glyphs,
                text_x,
                // A marker row (collapsed ancestor levels) is dim and glyph-less,
                // aligned with the name column at its depth.
                rgb: if r.marker {
                    self.theme.text_dim
                } else {
                    self.theme.text
                },
                band: r.current,
                icon: (!r.marker).then_some((glyf, x0)),
                hit: (!r.marker).then_some(TreeHit::Row(idx)),
                count,
            });
        }
        if below > 0 {
            push_marker(&mut lines, &mut content_w, below, TreeHit::PageDown);
        }

        // Count-badge geometry. Capsule: a fixed cell left of every icon (rows shift
        // right, badges right-align against the icons). Trailing: the panel widens to
        // the right and counts right-align at its edge.
        let has_counts = count_w > 0;
        let badge_pad = (count_px * 0.45).round().max(2.0) as i32;
        let badge_h = (count_px * 1.25).round() as i32;
        let badge_gap = (count_px * 0.5).round().max(3.0) as i32;
        let badge_cell = match counts {
            TreeCounts::Capsule if has_counts => count_w + 2 * badge_pad + badge_gap,
            _ => 0,
        };
        let trail_w = match counts {
            TreeCounts::Trailing if has_counts => count_w + badge_gap * 2,
            _ => 0,
        };

        // Wider left/right inset than the vertical `pad` (the line box pads top/bottom
        // for free; the sides don't). See `PAD_X`.
        let pad_x = ((pad as f32) * tokens::PAD_X).round() as i32;
        let left = pad_x + badge_cell; // where indent level 0's icon column starts
        let pw = (badge_cell + content_w + trail_w + 2 * pad_x) as u32;
        let ph = (lines.len() as i32 * line_h + 2 * pad as i32) as u32;
        let mut canvas = Canvas::new(pw, ph, bg, (px * tokens::RADIUS_PANEL).round());

        let mut hits: Vec<(TreeHit, [u32; 4])> = Vec::new();
        let count_ascent = self.ascent(count_px).unwrap_or(count_px * 0.8);
        for (i, line) in lines.iter().enumerate() {
            let row_top = pad as i32 + i as i32 * line_h;
            if let Some(hit) = line.hit {
                hits.push((hit, [0, row_top.max(0) as u32, pw, line_h as u32]));
                if hovered == Some(hit) {
                    // The hover affordance: a lighter band than the current row's
                    // (they layer when the current row itself is hovered).
                    let inset = (px * 0.35).round() as i32;
                    canvas.fill_round_rect(
                        inset,
                        row_top,
                        pw as i32 - 2 * inset,
                        line_h,
                        (px * 0.25).round(),
                        self.theme.text,
                        tokens::TREE_HOVER_ALPHA,
                    );
                }
            }
            if line.band {
                // Highlight band across the panel, inset a hair from the edges.
                let inset = (px * 0.35).round() as i32;
                canvas.fill_round_rect(
                    inset,
                    row_top,
                    pw as i32 - 2 * inset,
                    line_h,
                    (px * 0.25).round(),
                    self.theme.text,
                    tokens::TREE_CURRENT_ALPHA,
                );
            }
            if let Some((glyf, x0)) = line.icon {
                let glyph = match glyf {
                    Glyf::Closed => &closed,
                    Glyf::Open => &open,
                    Glyf::Up => &up_glyph,
                    Glyf::CaretUp => &caret_up,
                    Glyf::CaretDown => &caret_down,
                };
                if let Some((rgba, iw, ih)) = glyph {
                    // Right-align the glyph in its cell so folder fronts line up.
                    let ix = left + x0 + cell - *iw as i32;
                    let iy = row_top + (line_h - *ih as i32) / 2;
                    self.draw_icon(&mut canvas, rgba, *iw, *ih, ix, iy, px);
                }
                if let Some((cg, ca)) = &line.count {
                    match counts {
                        TreeCounts::Capsule => {
                            // Capsule badge right-aligned against the icon column: a
                            // near-solid pill in the theme's shadow color (black on
                            // dark, white on light) so the count pops off the panel.
                            let bw = ca.ceil() as i32 + 2 * badge_pad;
                            let bx = left + x0 - badge_gap - bw;
                            let by = row_top + (line_h - badge_h) / 2;
                            canvas.fill_round_rect(
                                bx,
                                by,
                                bw,
                                badge_h,
                                badge_h as f32 / 2.0,
                                self.theme.shadow,
                                tokens::TREE_BADGE_ALPHA,
                            );
                            let ty = by as f32 + (badge_h as f32 - count_px) / 2.0 + count_ascent;
                            self.draw_line(
                                &mut canvas,
                                (bx + badge_pad) as f32,
                                ty,
                                cg,
                                self.theme.text,
                                count_px,
                            );
                        }
                        TreeCounts::Trailing => {
                            // Dim numbers right-aligned at the panel's edge.
                            let tx = pw as f32 - pad_x as f32 - ca;
                            let ty =
                                row_top as f32 + (line_h as f32 - count_px) / 2.0 + count_ascent;
                            self.draw_line(&mut canvas, tx, ty, cg, self.theme.text_dim, count_px);
                        }
                    }
                }
            }
            self.draw_line(
                &mut canvas,
                (left + line.text_x) as f32,
                row_top as f32 + ascent,
                &line.glyphs,
                line.rgb,
                px,
            );
        }
        Some((canvas.into_rgba(), pw, ph, hits))
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
        button_hovered: bool,
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
        let button = ButtonSpec {
            label: button_label,
            icon: Some(button_icon),
            shortcut: None,
            shortcut_semibold: false,
            min_w: 0,
        };
        let (bw, bh) = self.button_size(&button, px_sub)?;
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
            self.theme.text,
            px,
        );
        y += head_lh + gap_lines;
        if has_path {
            self.draw_line(
                &mut canvas,
                center(path_adv),
                y as f32 + sub_asc,
                &path_g,
                self.theme.text_dim,
                px_sub,
            );
            y += sub_lh + gap_lines;
        }
        self.draw_line(
            &mut canvas,
            center(count_adv),
            y as f32 + sub_asc,
            &count_g,
            self.theme.text_dim,
            px_sub,
        );
        y += sub_lh + gap_button;

        // Centered button (stop icon + label, faint fill, subtle border; lifts on hover).
        let bx = (cw - bw) / 2;
        let button_rect = self.draw_button(&mut canvas, bx, y, &button, px_sub, button_hovered)?;
        Some((canvas.into_rgba(), cw as u32, ch as u32, button_rect))
    }

    /// Measure the reusable HUD **button**: the `(w, h)` `spec` occupies at text size `px`.
    /// Pair with [`draw_button`] — measure to *place* (e.g. the scan card centers it with this
    /// width; the open panel takes the max of two to align them), then draw.
    pub fn button_size(&self, spec: &ButtonSpec, px: f32) -> Option<(u32, u32)> {
        let b = self.layout_button(spec, px)?;
        Some((b.w.max(0) as u32, b.h.max(0) as u32))
    }

    /// Draw the reusable HUD **button** into `canvas` at `(x, y)` — a faint fill, a subtle
    /// rounded border, then the optional leading icon, the label, and an optional trailing
    /// dimmed shortcut hint (right-aligned, menu-style), all at text size `px`. When `hovered`,
    /// the fill + border lift to their `*_HOVER` alphas so the button lights up under the
    /// pointer. Returns its `[x, y, w, h]` rect (the click target).
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
        spec: &ButtonSpec,
        px: f32,
        hovered: bool,
    ) -> Option<[u32; 4]> {
        let b = self.layout_button(spec, px)?;
        let asc = self.ascent(px)?;
        let r = button_radius(px, b.h as f32);
        let (fill_a, border_a) = if hovered {
            (
                tokens::BUTTON_FILL_ALPHA_HOVER,
                tokens::BUTTON_BORDER_ALPHA_HOVER,
            )
        } else {
            (tokens::BUTTON_FILL_ALPHA, tokens::BUTTON_BORDER_ALPHA)
        };
        canvas.fill_round_rect(x, y, b.w, b.h, r, self.theme.text, fill_a);
        let t = (px * tokens::BUTTON_BORDER).round().max(1.0) as i32;
        canvas.stroke_round_rect(x, y, b.w, b.h, r, t, self.theme.text, border_a);
        let baseline = (y + b.pad_y) as f32 + asc;
        // With a shortcut hint the button is justified (label left, hint right) like a menu row;
        // without one the content block is centered when widened past its natural size (min_w).
        let mut cx = if b.shortcut.is_some() {
            x + b.pad_x
        } else {
            x + b.pad_x + ((b.w - b.content_w) / 2).max(0)
        };
        if let Some((rgba, iw, ih)) = &b.icon {
            let iy = y + (b.h - *ih as i32) / 2;
            self.draw_icon(canvas, rgba, *iw, *ih, cx, iy, px);
            cx += *iw as i32 + b.icon_gap;
        }
        self.draw_line(canvas, cx as f32, baseline, &b.glyphs, self.theme.text, px);
        // Trailing shortcut hint: dimmed, right-aligned against the button's inner edge.
        if let Some((glyphs, adv)) = &b.shortcut {
            let sx = (x + b.w - b.pad_x) as f32 - adv;
            self.draw_line(canvas, sx, baseline, glyphs, self.theme.text_dim, px);
        }
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
        spec: &ButtonSpec,
        px: f32,
        bg: [u8; 4],
        hovered: bool,
    ) -> Option<(Vec<u8>, u32, u32)> {
        let (w, h) = self.button_size(spec, px)?;
        let mut canvas = Canvas::new(w, h, bg, button_radius(px, h as f32));
        self.draw_button(&mut canvas, 0, 0, spec, px, hovered)?;
        Some((canvas.into_rgba(), w, h))
    }

    /// Lay out a button once — its label glyphs, rasterized icon, optional shortcut hint,
    /// paddings, and resulting `(w, h)` — shared by [`button_size`] and [`draw_button`] so
    /// measure and draw can never disagree. All sizing flows from the button's own text height
    /// `px` via the `BUTTON_*` [`tokens`], so a button is self-contained: the same call reads
    /// identically on a card, in a toast, or as a gallery swatch.
    fn layout_button(&self, spec: &ButtonSpec, px: f32) -> Option<ButtonLayout> {
        let (glyphs, label_adv) = self.layout(spec.label, px, Weight::Regular);
        let line_h = self.line_height(px)? as i32;
        let pad_x = (px * tokens::BUTTON_PAD_X).round().max(3.0) as i32;
        let pad_y = (px * tokens::BUTTON_PAD_Y).round().max(2.0) as i32;
        let icon = spec.icon.and_then(|svg| {
            let h = (px * tokens::BUTTON_ICON).round().max(1.0) as u32;
            crate::icon::rasterize(svg, h, self.theme.text)
        });
        let (icon_w, icon_gap) = match &icon {
            Some((_, w, _)) => (
                *w as i32,
                (px * tokens::BUTTON_ICON_GAP).round().max(2.0) as i32,
            ),
            None => (0, 0),
        };
        // The shortcut hint is drawn at the same size as the label, just dimmed (menu style) —
        // optionally a weight heavier for a touch more presence.
        let shortcut_weight = if spec.shortcut_semibold {
            Weight::Semibold
        } else {
            Weight::Regular
        };
        let shortcut = spec.shortcut.map(|t| self.layout(t, px, shortcut_weight));
        let (shortcut_w, shortcut_gap) = match &shortcut {
            Some((_, adv)) => (
                adv.ceil() as i32,
                (px * tokens::BUTTON_SHORTCUT_GAP).round().max(6.0) as i32,
            ),
            None => (0, 0),
        };
        let content_w =
            icon_w + icon_gap + label_adv.ceil() as i32 + shortcut_gap + shortcut_w + 2 * pad_x;
        let w = content_w.max(spec.min_w);
        let h = line_h + 2 * pad_y;
        Some(ButtonLayout {
            glyphs,
            icon,
            icon_gap,
            shortcut,
            pad_x,
            pad_y,
            content_w,
            w,
            h,
        })
    }

    /// Rasterize the **open-screen call to action**: two centered, interactive buttons stacked
    /// vertically — "Open File" and "Open Folder", each with its keyboard shortcut dimmed and
    /// right-aligned in menu-accelerator style. Each sits on its own translucent pill (`bg`), so
    /// it reads over any photo, and both share the wider one's width so the shortcuts line up.
    /// `file_key` / `folder_key` are the pre-formatted shortcut hints (empty → none);
    /// `file_hovered` / `folder_hovered` light the respective button. Returns
    /// `(rgba, w, h, file_rect, folder_rect)` where the rects are each button's `[x, y, w, h]`
    /// **within the bitmap** — the click targets; the caller offsets them to screen px.
    #[allow(clippy::too_many_arguments)]
    pub fn render_open_panel(
        &self,
        file_label: &str,
        file_key: &str,
        folder_label: &str,
        folder_key: &str,
        px: f32,
        bg: [u8; 4],
        shortcut_semibold: bool,
        file_hovered: bool,
        folder_hovered: bool,
    ) -> Option<OpenPanelBitmap> {
        let file = ButtonSpec {
            label: file_label,
            icon: None,
            shortcut: (!file_key.is_empty()).then_some(file_key),
            shortcut_semibold,
            min_w: 0,
        };
        let folder = ButtonSpec {
            label: folder_label,
            icon: None,
            shortcut: (!folder_key.is_empty()).then_some(folder_key),
            shortcut_semibold,
            min_w: 0,
        };
        // Size both, then give each the wider one's width so the stack aligns.
        let (fw, fh) = self.button_size(&file, px)?;
        let (dw, dh) = self.button_size(&folder, px)?;
        let bw = fw.max(dw) as i32;
        let file = ButtonSpec { min_w: bw, ..file };
        let folder = ButtonSpec {
            min_w: bw,
            ..folder
        };

        let gap = (px * tokens::OPEN_BUTTON_GAP).round().max(4.0) as i32;
        let (fh, dh) = (fh as i32, dh as i32);
        let w = bw;
        let h = fh + gap + dh;
        let pill = [bg[0], bg[1], bg[2]];
        let pill_a = bg[3] as f32 / 255.0;

        // A fully transparent canvas: each button paints its own dark pill (radius from
        // its own height, matching the button drawn over it), so the gap between them
        // (and around them) stays clear for the photo to show through.
        let mut canvas = Canvas::new(w as u32, h as u32, [0, 0, 0, 0], 0.0);
        canvas.fill_round_rect(0, 0, bw, fh, button_radius(px, fh as f32), pill, pill_a);
        let file_rect = self.draw_button(&mut canvas, 0, 0, &file, px, file_hovered)?;
        let y2 = fh + gap;
        canvas.fill_round_rect(0, y2, bw, dh, button_radius(px, dh as f32), pill, pill_a);
        let folder_rect = self.draw_button(&mut canvas, 0, y2, &folder, px, folder_hovered)?;
        Some((
            canvas.into_rgba(),
            w as u32,
            h as u32,
            file_rect,
            folder_rect,
        ))
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
                    self.theme.shadow,
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
            canvas.blit_silhouette(
                rgba,
                iw,
                ih,
                x0 + dx,
                y0 + dy,
                self.theme.shadow,
                SHADOW_ALPHA,
            );
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

/// The corner radius for a button of text size `px` and pixel height `h`: macOS derives
/// it from the height (Tahoe's concentric principle — see
/// [`tokens::BUTTON_RADIUS_OF_HEIGHT`]); elsewhere it stays the text-relative
/// [`tokens::BUTTON_RADIUS`], keeping Windows rendering unchanged.
#[allow(unused_variables)]
fn button_radius(px: f32, h: f32) -> f32 {
    #[cfg(target_os = "macos")]
    {
        (h * tokens::BUTTON_RADIUS_OF_HEIGHT).round()
    }
    #[cfg(not(target_os = "macos"))]
    {
        (px * tokens::BUTTON_RADIUS).round()
    }
}

/// Coverage (0..=1) of pixel `(x, y)` inside a `w×h` rectangle with `r`-px rounded
/// corners — 1.0 everywhere except the four corners, where it feathers over ~1px for
/// anti-aliasing. The corner *shape* comes from [`corner_distance`]'s norm (circular
/// arcs on Windows, superellipse on macOS); fills and the button border stroke both
/// build on this, so every HUD surface gets the same corner family.
fn corner_coverage(x: i32, y: i32, w: i32, h: i32, r: f32) -> f32 {
    if r <= 0.0 {
        return 1.0;
    }
    let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5); // pixel center
                                                     // Nearest point on the inner rect whose corners are the arc centers; distance
                                                     // to it is 0 except in the corner regions.
    let cx = fx.clamp(r, w as f32 - r);
    let cy = fy.clamp(r, h as f32 - r);
    let d = corner_distance(fx - cx, fy - cy);
    if d <= 0.0 {
        1.0
    } else {
        (r - d + 0.5).clamp(0.0, 1.0)
    }
}

/// The corner gauge: the offset from the arc-center rect measured in an **n-norm**,
/// whose level sets are superellipses — the exponent picks the corner family.
///
/// **macOS: n = 2.2** — just above circular, matching the *fullness* of Apple's
/// continuous corners with a gently softened entry into the straight edges. The
/// first attempt used n = 5 ("the squircle exponent"), which was measurably wrong:
/// an n=5 corner hugs its box so tightly that a 25 px radius silhouettes like a
/// ~10 px circle — the owner-reported "still too sharp". Apple's curve is nearly
/// circular in how much it cuts away; its character comes from the smooth curvature
/// transition, so a low exponent (+ the height-derived radius in [`button_radius`])
/// is the faithful in-box approximation. Tuned visually against Tahoe's bordered
/// buttons; this exponent and `BUTTON_RADIUS_OF_HEIGHT` are the two knobs.
#[cfg(target_os = "macos")]
fn corner_distance(dx: f32, dy: f32) -> f32 {
    const N: f32 = 2.2;
    (dx.abs().powf(N) + dy.abs().powf(N)).powf(1.0 / N)
}

/// **Elsewhere: n = 2** — the Euclidean norm, i.e. conventional circular arcs
/// (Windows 11's own corner family), bit-identical to the pre-superellipse behavior.
#[cfg(not(target_os = "macos"))]
fn corner_distance(dx: f32, dy: f32) -> f32 {
    (dx * dx + dy * dy).sqrt()
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
        // Interior and straight edges are fully covered (in ANY n-norm: on-axis the
        // n-norm equals the Euclidean one, so these hold on every platform).
        assert!((corner_coverage(50, 50, 100, 100, r) - 1.0).abs() < 1e-6);
        assert!((corner_coverage(50, 0, 100, 100, r) - 1.0).abs() < 1e-6); // top edge
        assert!((corner_coverage(0, 50, 100, 100, r) - 1.0).abs() < 1e-6); // left edge
                                                                           // The extreme corner pixel is outside the arc → transparent.
        assert_eq!(corner_coverage(0, 0, 100, 100, r), 0.0);
        assert_eq!(corner_coverage(99, 99, 100, 100, r), 0.0);
        // Radius 0 → no rounding anywhere.
        assert_eq!(corner_coverage(0, 0, 100, 100, 0.0), 1.0);
    }

    /// The macOS corner is a (mild, n = 2.2) superellipse: on the corner diagonal it
    /// is *fuller* than a circular arc — pixel (2,2) at r=10 sits fully outside the
    /// circle (Euclidean distance ≈ 10.6 from the arc center → coverage exactly 0)
    /// but partially inside the n=2.2 gauge (norm ≈ 10.3 → coverage > 0). All four
    /// corners agree by symmetry; the extreme corner pixel still rounds away
    /// (asserted in `corner_coverage_rounds_only_the_corners`).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_corners_are_superelliptical() {
        let r = 10.0;
        for (x, y) in [(2, 2), (97, 2), (2, 97), (97, 97)] {
            let cov = corner_coverage(x, y, 100, 100, r);
            assert!(
                cov > 0.0,
                "({x},{y}) should be inside the superellipse (a circle excludes it), got {cov}"
            );
        }
    }

    /// The dark theme is pinned to the legacy `tokens` constants and stays the default,
    /// so pre-theme rendering is pixel-identical (task #46's regression guard).
    #[test]
    fn dark_theme_matches_legacy_constants_and_is_default() {
        assert_eq!(Theme::DARK.bg, BG);
        assert_eq!(Theme::DARK.text, tokens::TEXT);
        assert_eq!(Theme::DARK.text_dim, tokens::TEXT_DIM);
        assert_eq!(Theme::DARK.shadow, tokens::SHADOW);
        assert_eq!(Theme::DARK.shortcut_sep, tokens::SHORTCUT_SEP);
        assert_eq!(Theme::of(true), Theme::DARK);
        assert_eq!(Theme::of(false), Theme::LIGHT);
        if let Some(hud) = Hud::load() {
            assert_eq!(hud.theme(), Theme::DARK, "dark is the default scheme");
        }
        // The legacy helper and the dark theme's agree on every opacity.
        for pct in [0u8, 37, 60, 100] {
            assert_eq!(bg_for_opacity(pct), Theme::DARK.bg_for_opacity(pct));
        }
        assert_eq!(Theme::LIGHT.bg_for_opacity(100)[3], 255);
        assert_eq!(Theme::LIGHT.bg_for_opacity(0)[3], 0);
    }

    /// A light-themed panel composites inverted: a bright translucent background with
    /// dark glyphs (vs. the dark theme's black panel + white glyphs).
    #[test]
    fn light_theme_flips_panel_colors() {
        let Some(mut hud) = Hud::load() else {
            return;
        };
        let render = |hud: &Hud| {
            let bg = hud.theme().bg;
            hud.render_panel("Theme probe", 20.0, 8, bg)
                .expect("renders")
        };
        let (dark_px, w, h) = render(&hud);
        hud.set_theme(Theme::LIGHT);
        let (light_px, lw, lh) = render(&hud);
        assert_eq!((w, h), (lw, lh), "geometry is theme-independent");
        // Panel-corner-adjacent background pixel: dark theme ≈ black, light ≈ white.
        let idx = ((h / 2) * w) as usize * 4 + 4; // left edge, mid height
        assert!(dark_px[idx] < 60, "dark panel bg is near-black");
        assert!(light_px[idx] > 200, "light panel bg is near-white");
        // The glyph ink flips the other way: darkest pixel in light mode is near-black.
        let min_rgb = |px: &[u8]| {
            px.chunks_exact(4)
                .filter(|c| c[3] > 200)
                .map(|c| c[0])
                .min()
                .unwrap_or(255)
        };
        let max_rgb = |px: &[u8]| {
            px.chunks_exact(4)
                .filter(|c| c[3] > 200)
                .map(|c| c[0])
                .max()
                .unwrap_or(0)
        };
        assert!(
            max_rgb(&dark_px) > 200,
            "dark theme has bright (white) glyph ink"
        );
        assert!(
            min_rgb(&light_px) < 60,
            "light theme has dark (black) glyph ink"
        );
    }

    #[test]
    fn open_panel_stacks_two_width_aligned_buttons() {
        // A font is needed to lay the panel out; skip on a headless box without one.
        let Some(hud) = Hud::load() else {
            return;
        };
        let (_rgba, w, h, file, folder) = hud
            .render_open_panel(
                "Open File",
                "O",
                "Open Folder",
                "\u{21e7}\u{2009}O",
                20.0,
                BG,
                true,
                false,
                false,
            )
            .expect("open panel renders");
        // Both buttons span the panel width (min_w = the wider button), left-aligned at 0.
        assert_eq!(file[0], 0);
        assert_eq!(folder[0], 0);
        assert_eq!(file[2], w, "file button spans the panel width");
        assert_eq!(folder[2], w, "folder button spans the panel width");
        // The folder button is stacked strictly below the file button, within the bitmap.
        assert!(folder[1] >= file[1] + file[3], "folder sits below file");
        assert!(h >= folder[1] + folder[3], "panel is tall enough for both");
    }

    /// Shared shorthand: render with no paging/hover, capsule counts.
    fn tree(hud: &Hud, rows: &[TreeRow], max_h: i32, page: i32) -> Option<TreeBitmap> {
        hud.render_tree(rows, 15.0, 7, BG, max_h, page, None, TreeCounts::Capsule)
    }

    #[test]
    fn render_tree_windows_around_the_current_row_and_pages() {
        let Some(hud) = Hud::load() else {
            return;
        };
        let rows: Vec<TreeRow> = (0..40)
            .map(|i| TreeRow {
                depth: 1,
                name: format!("folder-{i:02}"),
                open: i == 20,
                current: i == 20,
                marker: false,
                up: false,
                count: None,
            })
            .collect();
        // Generous height → every row renders, no markers, one hit rect per row.
        let (rgba, w, h, hits) = tree(&hud, &rows, 10_000, 0).expect("renders");
        assert!(w > 0 && h > 0);
        assert_eq!(rgba.len(), (w * h * 4) as usize, "buffer matches w*h*4");
        assert_eq!(hits.len(), 40, "every folder row is a hit target");
        assert!(hits.iter().all(|(t, _)| matches!(t, TreeHit::Row(_))));
        // A tight height windows the list: far fewer lines, a much shorter panel,
        // and both paging markers among the hits.
        let (_, _, h2, hits2) = tree(&hud, &rows, 200, 0).expect("renders");
        assert!(h2 < h / 2, "tight max_h windows the rows: {h2} vs {h}");
        assert!((h2 as i32) <= 200, "windowed panel fits max_h, got {h2}");
        assert!(hits2.iter().any(|(t, _)| *t == TreeHit::PageUp));
        assert!(hits2.iter().any(|(t, _)| *t == TreeHit::PageDown));
        // Paging far down clamps to the tail: the PageDown marker disappears and
        // the last row becomes visible.
        let (_, _, _, hits3) = tree(&hud, &rows, 200, 100).expect("renders");
        assert!(!hits3.iter().any(|(t, _)| *t == TreeHit::PageDown));
        assert!(hits3.iter().any(|(t, _)| *t == TreeHit::Row(39)));
        // Hit rects span the panel width and don't overlap vertically.
        for pair in hits2.windows(2) {
            let (a, b) = (pair[0].1, pair[1].1);
            assert!(a[1] + a[3] <= b[1], "rects stack without overlap");
        }
    }

    #[test]
    fn render_tree_indents_counts_and_variants() {
        let Some(hud) = Hud::load() else {
            return;
        };
        let row = |depth: u32, marker: bool, up: bool, count: Option<u64>| TreeRow {
            depth,
            name: "aaa".into(),
            open: false,
            current: false,
            marker,
            up,
            count,
        };
        let (_, w0, ..) = tree(&hud, &[row(0, false, false, None)], 10_000, 0).expect("renders");
        let (_, w3, ..) = tree(&hud, &[row(3, false, false, None)], 10_000, 0).expect("renders");
        assert!(w3 > w0, "depth widens the panel: {w3} vs {w0}");
        // A count widens the panel in either style; the two styles both render.
        let counted = [row(0, false, false, Some(1_234))];
        let (_, wc, ..) = hud
            .render_tree(&counted, 15.0, 7, BG, 10_000, 0, None, TreeCounts::Capsule)
            .expect("renders");
        let (_, wt, ..) = hud
            .render_tree(&counted, 15.0, 7, BG, 10_000, 0, None, TreeCounts::Trailing)
            .expect("renders");
        assert!(wc > w0 && wt > w0, "counts widen: {wc}/{wt} vs {w0}");
        // A collapsed-levels marker renders and is NOT a hit target; an up row is.
        let (_, _, _, hits) = tree(
            &hud,
            &[row(0, false, true, None), row(1, true, false, None)],
            10_000,
            0,
        )
        .expect("renders");
        assert_eq!(hits.len(), 1, "marker rows aren't hits");
        assert_eq!(hits[0].0, TreeHit::Row(0));
        assert!(tree(&hud, &[], 10_000, 0).is_none());
    }

    #[test]
    fn render_shortcuts_packs_into_columns_when_short() {
        let Some(hud) = Hud::load() else {
            return;
        };
        let sections = vec![
            ShortcutSection {
                title: "Browse".into(),
                rows: vec![
                    ("Next image".into(), "Space".into()),
                    ("Previous image".into(), "\u{232b}".into()),
                ],
            },
            ShortcutSection {
                title: "Files".into(),
                rows: vec![("Copy image".into(), "\u{2318}\u{2009}C".into())],
            },
        ];
        // Generous height → one column.
        let (rgba, w, h) = hud
            .render_shortcuts(&sections, 18.0, BG, 10_000)
            .expect("renders");
        assert!(w > 0 && h > 0);
        assert_eq!(rgba.len(), (w * h * 4) as usize, "buffer matches w*h*4");
        // A tight max height forces each section into its own column: wider, shorter.
        let (_, w2, h2) = hud
            .render_shortcuts(&sections, 18.0, BG, 1)
            .expect("renders");
        assert!(
            w2 > w,
            "a tight height forces more columns (wider): {w2} vs {w}"
        );
        assert!(h2 < h, "…and a shorter panel: {h2} vs {h}");
    }
}
