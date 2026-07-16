//! The chrome **icon system**.
//!
//! Font Awesome SVGs, rasterized once into a **white sprite** and **tinted at draw
//! time** — so a single texture serves every tone and both themes (no re-rasterizing
//! per color). Each glyph is rendered **centered into a square**, so a narrow icon
//! (lock is 384×512) and a square one (circles are 512×512) occupy the same box and
//! align identically — egui's own `fa-fw`. That, plus the placement helpers
//! ([`lead_row`], [`inline`]) which bake the gutter + vertical centering, is what makes
//! icons sit consistently with text with no clipping or per-call nudging.
//!
//! Icons are **vendored per family** (`icons/<family>/<name>.svg`); flip
//! [`ACTIVE_FAMILY`] to re-skin the whole UI. Semantic [`Icon`] names (what it *means*,
//! not which glyph) keep the swap to one place. Rasterization runs once per (icon,
//! family, size) and is cached in the egui context — off the photo hot path, RAM-only.

use egui::Color32;
use resvg::{tiny_skia, usvg};

use crate::Palette;

/// Font Awesome family/weight. Add a variant **and** vendor `icons/<family>/*.svg` for
/// every [`Icon`] to make it selectable.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Family {
    Solid,
    Regular,
}

/// The family every chrome icon uses. **Flip this one line to re-skin the UI** (the
/// chosen family must be vendored for every [`Icon`]).
pub const ACTIVE_FAMILY: Family = Family::Solid;

/// The active family's lowercase name (for the gallery / diagnostics).
pub fn family_name() -> &'static str {
    match ACTIVE_FAMILY {
        Family::Solid => "solid",
        Family::Regular => "regular",
    }
}

/// Default size (logical px) of a dialog/notice **lead** icon.
pub const LEAD_SIZE: f32 = 20.0;
/// Default size of an **inline**-with-text icon.
pub const INLINE_SIZE: f32 = 16.0;

/// A **semantic** icon — named for meaning, not glyph, so changing the glyph (or
/// family) is a one-line edit here, never at the call sites.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Lock,
    Warning,
    Error,
    Info,
    Success,
    Help,
    Trash,
    Folder,
    FolderOpen,
    /// Recognized-text / OCR (the Inspector Text tab — matches SF `text.viewfinder`).
    Text,
    /// AI description (the Inspector Describe tab — matches SF `sparkles`).
    Sparkles,
    /// Ask-a-question (the Describe tab's Ask button — matches SF `questionmark.bubble`).
    MessageQuestion,
    /// Thumbnails strip (the left pane's second tab — matches SF `photo.on.rectangle`).
    Images,
    /// Live Photo mark (info readout — the concentric-ring glyph, matches SF `livephoto`).
    LivePhoto,
    /// Animated-image mark (GIF/APNG/… in the info readout) — a film strip.
    Film,
    /// Settings ▸ General tab (matches SF `slider.horizontal.3`).
    Sliders,
    /// Settings ▸ Appearance tab (matches SF `paintbrush`).
    Brush,
    /// Settings ▸ Shortcuts tab (matches SF `keyboard`).
    Keyboard,
    /// Settings ▸ Subtitles tab (matches the macOS pane's SF `captions.bubble`).
    Captions,
    /// Open-file action (the welcome screen's Open File button — matches SF `doc`).
    File,
    /// Play action (the play hint on an animated item — matches SF `play.fill`).
    Play,
    /// Pause action (the playback bar's play/pause button while a video plays —
    /// matches SF `pause.fill`).
    Pause,
    /// Toolbar: previous photo (matches SF `chevron.left`).
    ChevronLeft,
    /// Toolbar: next photo (matches SF `chevron.right`).
    ChevronRight,
    /// Toolbar: previous folder (matches SF `chevron.left.2`).
    AnglesLeft,
    /// Toolbar: next folder (matches SF `chevron.right.2`).
    AnglesRight,
    /// Toolbar: random photo (matches SF `shuffle`; the random-prev button paints it
    /// mirrored — see [`paint_tinted_flipped`]).
    Shuffle,
    /// Toolbar: rotate counter-clockwise (matches SF `rotate.left`).
    RotateLeft,
    /// Toolbar: rotate clockwise (matches SF `rotate.right`).
    RotateRight,
    /// Toolbar: Details / all-EXIF panel toggle (matches SF `info.circle`, but a table
    /// glyph so it reads distinct from the basic info line's [`Info`](Self::Info)).
    TableList,
    /// Toolbar: the basic bottom **info line** toggle — a plain, uncircled `i` (FA `info`),
    /// deliberately distinct from the circled [`Info`](Self::Info) that toggles the info panel.
    InfoLine,
    /// Toolbar: folder-tree panel toggle (matches SF `folder.circle`).
    FolderTree,
    /// Toolbar: enter fullscreen (matches SF `arrow.up.left.and.arrow.down.right`).
    Expand,
}

/// A **theme-aware** icon color. `Neutral` is the quiet default (a gray that varies
/// light/dark); the rest are semantic and reserved for when they mean something.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Accent,
    Warning,
    Danger,
    Success,
}

/// Resolve a [`Tone`] to a concrete color for the current theme.
pub fn tone_color(tone: Tone, p: &Palette) -> Color32 {
    match tone {
        Tone::Neutral => p.text_secondary,
        Tone::Accent => p.accent,
        // Vivid bright yellow on dark; a saturated amber on light (a deeper amber reads
        // brown, a pure yellow has no contrast on white — this stays clearly amber).
        Tone::Warning if p.dark => Color32::from_rgb(0xff, 0xd2, 0x3f),
        Tone::Warning => Color32::from_rgb(0xf0, 0xa5, 0x00),
        // The destructive tone tracks the palette's themed `danger` (single source of truth
        // shared with `danger_button`).
        Tone::Danger => p.danger,
        Tone::Success if p.dark => Color32::from_rgb(0x4e, 0xc7, 0x7a),
        Tone::Success => Color32::from_rgb(0x2f, 0x9e, 0x54),
    }
}

/// The vendored SVG source for `icon` in `family` (compiled in via `include_str!` — no
/// runtime file I/O, no viewing trace).
fn svg(icon: Icon, family: Family) -> &'static str {
    // Each arm includes both families' bytes; `ACTIVE_FAMILY` picks at the call.
    macro_rules! glyph {
        ($name:literal) => {
            match family {
                Family::Solid => include_str!(concat!("../icons/solid/", $name, ".svg")),
                Family::Regular => include_str!(concat!("../icons/regular/", $name, ".svg")),
            }
        };
    }
    match icon {
        Icon::Lock => glyph!("lock"),
        Icon::Warning => glyph!("triangle-exclamation"),
        Icon::Error => glyph!("circle-exclamation"),
        Icon::Info => glyph!("circle-info"),
        Icon::Success => glyph!("circle-check"),
        Icon::Help => glyph!("circle-question"),
        Icon::Trash => glyph!("trash-can"),
        Icon::Folder => glyph!("folder"),
        Icon::FolderOpen => glyph!("folder-open"),
        Icon::Text => glyph!("text"),
        Icon::Sparkles => glyph!("sparkles"),
        Icon::MessageQuestion => glyph!("message-question"),
        Icon::Images => glyph!("images"),
        Icon::LivePhoto => glyph!("livephoto"),
        Icon::Film => glyph!("film"),
        Icon::Sliders => glyph!("sliders"),
        Icon::Brush => glyph!("brush"),
        Icon::Keyboard => glyph!("keyboard"),
        Icon::Captions => glyph!("message-captions"),
        Icon::File => glyph!("file"),
        Icon::Play => glyph!("play"),
        Icon::Pause => glyph!("pause"),
        Icon::ChevronLeft => glyph!("chevron-left"),
        Icon::ChevronRight => glyph!("chevron-right"),
        Icon::AnglesLeft => glyph!("angles-left"),
        Icon::AnglesRight => glyph!("angles-right"),
        Icon::Shuffle => glyph!("shuffle"),
        Icon::RotateLeft => glyph!("rotate-left"),
        Icon::RotateRight => glyph!("rotate-right"),
        Icon::TableList => glyph!("table-list"),
        Icon::InfoLine => glyph!("info"),
        Icon::FolderTree => glyph!("folder-tree"),
        Icon::Expand => glyph!("up-right-and-down-left-from-center"),
    }
}

/// Rasterize an SVG to a **white**, straight-alpha `px`×`px` sprite, the glyph scaled to
/// fit and **centered** (so any source aspect ratio yields the same square box).
///
/// We fit the **union of the viewBox and the ink bounding box**, not just the viewBox:
/// FA glyphs can spill past their viewBox (the lock's shackle reaches `y=-32` above its
/// `0..512` box), and a viewBox-only fit clips that overflow at the top. The union keeps
/// normal icons sized exactly as FA intends (their ink stays inside the viewBox, so the
/// union *is* the viewBox) and shrinks an overflowing glyph just enough to show whole.
fn rasterize_white(svg_src: &str, px: u32) -> Option<egui::ColorImage> {
    let px = px.max(1);
    // White fill so the texture can be tinted to any tone at draw time.
    let src = svg_src.replace("currentColor", "#ffffff");
    let tree = usvg::Tree::from_data(src.as_bytes(), &usvg::Options::default()).ok()?;
    let size = tree.size();
    let (vw, vh) = (size.width(), size.height());
    let bbox = tree.root().abs_bounding_box();
    let min_x = bbox.left().min(0.0);
    let min_y = bbox.top().min(0.0);
    let max_x = bbox.right().max(vw);
    let max_y = bbox.bottom().max(vh);
    let (rw, rh) = (max_x - min_x, max_y - min_y);
    if rw <= 0.0 || rh <= 0.0 {
        return None;
    }
    let scale = px as f32 / rw.max(rh);
    let (tw, th) = (rw * scale, rh * scale);
    // Center the region in the square, offset so its top-left maps to the centered box.
    let dx = (px as f32 - tw) / 2.0 - min_x * scale;
    let dy = (px as f32 - th) / 2.0 - min_y * scale;
    let mut pixmap = tiny_skia::Pixmap::new(px, px)?;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [px as usize, px as usize],
        &pixmap.take_demultiplied(),
    ))
}

/// Get (or build + cache) the white sprite texture for `(icon, family, px)`. Cached in
/// the egui context's temp data, so it's rasterized once and reused every frame; the
/// handle is dropped with the context (RAM-only).
fn texture(ctx: &egui::Context, icon: Icon, family: Family, px: u32) -> egui::TextureHandle {
    let id = egui::Id::new(("pb_ui::icon", icon, family, px));
    if let Some(handle) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return handle;
    }
    let image = rasterize_white(svg(icon, family), px)
        .unwrap_or_else(|| egui::ColorImage::new([px as usize, px as usize], Color32::TRANSPARENT));
    let handle = ctx.load_texture("pb_ui_icon", image, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, handle.clone()));
    handle
}

/// Draw `icon` as a `size`×`size` square sprite, tinted for `tone`. Rasterized crisp at
/// the current DPI and centered, so it places predictably anywhere. The atom the
/// placement helpers build on; use it directly for a free-standing icon.
pub fn image(ui: &mut egui::Ui, icon: Icon, size: f32, tone: Tone, p: &Palette) -> egui::Response {
    let px = (size * ui.ctx().pixels_per_point()).round().max(1.0) as u32;
    let tex = texture(ui.ctx(), icon, ACTIVE_FAMILY, px);
    let src = egui::load::SizedTexture::new(tex.id(), egui::vec2(size, size));
    ui.add(egui::Image::new(src).tint(tone_color(tone, p)))
}

/// Paint `icon` tinted for `tone` **into `rect`** — for manual/computed layouts (e.g. a
/// fixed-width icon column) that place icons at exact positions instead of flowing them.
/// The sprite is square and centered, so pass a square `rect` (the glyph fits inside).
/// Complements [`image`], which adds a flowing widget. Draws nothing new to the layout —
/// pure painting — so the caller owns the geometry (and thus vertical centering).
pub fn paint(ui: &egui::Ui, rect: egui::Rect, icon: Icon, tone: Tone, p: &Palette) {
    paint_tinted(ui, rect, icon, tone_color(tone, p));
}

/// Like [`paint`], but with an **explicit** tint color — for callers whose color isn't one
/// of the semantic [`Tone`]s (e.g. an inspector tab's icon that goes white when selected and
/// panel-secondary otherwise).
pub fn paint_tinted(ui: &egui::Ui, rect: egui::Rect, icon: Icon, tint: Color32) {
    paint_sprite(ui, rect, icon, tint, false);
}

/// Like [`paint_tinted`], but **mirrored horizontally** — the sprite is drawn flipped
/// left-to-right by swapping the U texture coordinates. The toolbar's random-*previous*
/// button uses it so the `shuffle` glyph points the other way (the macOS
/// `shuffleImage(flipped:)` trick), giving a symmetric back/forward pair from one asset.
pub fn paint_tinted_flipped(ui: &egui::Ui, rect: egui::Rect, icon: Icon, tint: Color32) {
    paint_sprite(ui, rect, icon, tint, true);
}

/// Shared painter for [`paint_tinted`] / [`paint_tinted_flipped`]: rasterize the sprite at the
/// **exact physical-pixel size** of the destination and draw it into a **pixel-snapped** square,
/// so the texture maps 1:1 to pixels. Without this, a 17 pt icon at 150% DPI wants 25.5 physical
/// px but the sprite rounds to 26 and lands in a fractionally-placed box — the GPU then minifies
/// 26→25.5 with linear filtering (blur) and clamps the edge texel row (flattening the bottom of
/// round glyphs). Snapping makes both go away; at 100% it was already ~1:1 (why it looked fine
/// locally but got compounded by Remote Desktop's own frame compression).
fn paint_sprite(ui: &egui::Ui, rect: egui::Rect, icon: Icon, tint: Color32, mirror: bool) {
    let ppp = ui.ctx().pixels_per_point();
    // Whole-pixel sprite size, and a destination sized to exactly that many physical pixels.
    let px = (rect.width().max(rect.height()) * ppp).round().max(1.0);
    // Snap the top-left onto the physical grid so texel centers land on pixel centers.
    let min = egui::pos2(
        (rect.min.x * ppp).round() / ppp,
        (rect.min.y * ppp).round() / ppp,
    );
    let dst = egui::Rect::from_min_size(min, egui::Vec2::splat(px / ppp));
    let tex = texture(ui.ctx(), icon, ACTIVE_FAMILY, px as u32);
    // U runs 1→0 across the rect for the mirror, 0→1 otherwise.
    let uv = if mirror {
        egui::Rect::from_min_max(egui::pos2(1.0, 0.0), egui::pos2(0.0, 1.0))
    } else {
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
    };
    ui.painter().image(tex.id(), dst, uv, tint);
}

/// An icon **inline** with a single line of text — sized to the body text. Add it before
/// a label in a horizontal row (egui centers it against the line).
pub fn inline(ui: &mut egui::Ui, icon: Icon, tone: Tone, p: &Palette) -> egui::Response {
    image(ui, icon, INLINE_SIZE, tone, p)
}

/// A **lead-icon body**: a status `icon` in a left gutter, vertically centered against
/// the first line of `content`. This is the dialog/notice shape — the gutter gap and the
/// centering nudge live here, so callers never hand-tune the icon's position.
pub fn lead_row<R>(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: Icon,
    tone: Tone,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mut out = None;
    ui.horizontal_top(|ui| {
        // Center the icon on the first text line (top-aligning it reads too high).
        let line = ui.text_style_height(&egui::TextStyle::Body);
        let nudge = ((line - LEAD_SIZE) / 2.0).max(0.0);
        ui.vertical(|ui| {
            ui.add_space(nudge);
            image(ui, icon, LEAD_SIZE, tone, p);
        });
        ui.add_space(crate::SPACE_3);
        ui.vertical(|ui| {
            out = Some(content(ui));
        });
    });
    out.expect("lead_row content always runs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_rasterizes_square_and_white() {
        for icon in [
            Icon::Lock,
            Icon::Warning,
            Icon::Error,
            Icon::Info,
            Icon::Success,
            Icon::Help,
            Icon::Trash,
            Icon::Folder,
            Icon::FolderOpen,
            Icon::Text,
            Icon::Sparkles,
            Icon::MessageQuestion,
            Icon::LivePhoto,
            Icon::Film,
            Icon::Sliders,
            Icon::Brush,
            Icon::Keyboard,
            Icon::File,
            Icon::Play,
            Icon::Pause,
            Icon::ChevronLeft,
            Icon::ChevronRight,
            Icon::AnglesLeft,
            Icon::AnglesRight,
            Icon::Shuffle,
            Icon::RotateLeft,
            Icon::RotateRight,
            Icon::TableList,
            Icon::InfoLine,
            Icon::FolderTree,
            Icon::Expand,
        ] {
            for family in [Family::Solid, Family::Regular] {
                let img = rasterize_white(svg(icon, family), 32).expect("icon should rasterize");
                assert_eq!(img.size, [32, 32], "sprite is square");
                // The glyph paints some opaque white pixels (tinted later at draw).
                let opaque_white = img
                    .pixels
                    .iter()
                    .filter(|p| p.a() == 255)
                    .all(|p| p.r() == 255 && p.g() == 255 && p.b() == 255);
                let any_opaque = img.pixels.iter().any(|p| p.a() == 255);
                assert!(any_opaque, "glyph produced opaque pixels");
                assert!(opaque_white, "sprite is white (tint happens at draw)");
            }
        }
    }

    #[test]
    fn lock_shackle_top_not_clipped() {
        // The lock glyph overflows its viewBox at the top (shackle tip at y=-32). With
        // the union fit, that tip lands in the very top rows of the sprite — so if it's
        // present, the overflow rendered and nothing was cropped. (A viewBox-only fit, or
        // resvg clipping to the viewBox, would leave the top rows empty.)
        let px = 64usize;
        let img = rasterize_white(svg(Icon::Lock, Family::Solid), px as u32).unwrap();
        let row_has_ink = |y: usize| (0..px).any(|x| img.pixels[y * px + x].a() > 0);
        assert!(
            row_has_ink(0) || row_has_ink(1),
            "lock shackle tip should reach the top rows (not clipped)"
        );
    }

    #[test]
    fn tone_varies_with_theme() {
        let dark = Palette::new(true);
        let light = Palette::new(false);
        // Neutral follows the palette's secondary text (theme-aware).
        assert_eq!(tone_color(Tone::Neutral, &dark), dark.text_secondary);
        assert_ne!(
            tone_color(Tone::Warning, &dark),
            tone_color(Tone::Warning, &light)
        );
    }
}
