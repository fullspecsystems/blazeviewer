//! The **egui rich panels** for the winit shell (task #54 Phase 4): Help, the Inspector
//! (Details / Text / Describe), and the folder tree, laid out over the photo by the
//! [`egui_overlay`](crate::egui_overlay). These consume the same shell-neutral panel
//! models the macOS SwiftUI panels do (`pb_app_core::panels` / `fs_tree`), so the two
//! shells render one design in two skins — this file is the Windows/egui skin, aiming
//! for parity with the native SwiftUI panels.
//!
//! The build path is pure: [`PanelFrame::snapshot`] copies the panel data out of the
//! core so the egui closure holds no `&AppCore`, and [`build`] pushes any interaction
//! into a [`PanelAction`] list the shell applies afterward.

use egui::{Align2, Color32, FontFamily, FontId, RichText, Rounding, Stroke};
use pb_app_core::panels::{
    DescribeBody, DescribePanel, DetailRow, DetailsPanel, HelpPanel, InspectorSnapshot, TextBody,
    TextPanel,
};
use pb_app_core::settings::InfoLineAlign;
use pb_app_core::{fs_tree, AppCore, InspectorTab};
use pb_ui::icon::{Icon, Tone};
use pb_ui::Palette;
use std::path::PathBuf;

/// The inset of a panel from the window edge (SwiftUI `Layout.edge`).
const EDGE: f32 = 24.0;
/// The shared panel corner radius (SwiftUI `panelBackground(cornerRadius: 12)`).
const PANEL_RADIUS: f32 = 12.0;
/// Help panel fixed width (SwiftUI `width: 520`).
const HELP_WIDTH: f32 = 520.0;
/// Inspector default/min width (SwiftUI `inspectorWidth` default 360).
const INSPECTOR_WIDTH: f32 = 360.0;
/// Folder-tree default/min width (SwiftUI `treeWidth` default 280).
const TREE_WIDTH: f32 = 280.0;
/// Inspector pair-row label column width. Wider than SwiftUI's 116 so long EXIF field names
/// (PhotographicSensitivity, FocalLengthIn35mmFilm, DateTimeDigitized…) fit on one line before
/// wrapping — values are mostly short or extreme-and-wrapping anyway, so the label gets the
/// room. The smaller 13px label helps too.
const PAIR_LABEL_W: f32 = 156.0;
/// Inspector Details type size — the folder-tree name size (13) so the metadata table stays
/// dense and long values wrap less; bold spans (filename / section titles) get a hair more.
const DETAIL_SIZE: f32 = 13.0;
const DETAIL_HEADER_SIZE: f32 = 13.5;
/// Help panel metrics: descriptions + section headings at the title size (compact), keycaps a
/// touch smaller. Rows are a fixed height with tight spacing so the table doesn't sprawl.
const HELP_TEXT_SIZE: f32 = 13.5;
const HELP_KEY_SIZE: f32 = 12.0;
const HELP_ROW_H: f32 = 23.0;
const HELP_KEYCAP_H: f32 = 18.0;
const HELP_SECTION_H: f32 = 24.0;
/// Small left pad for help text (section headings + descriptions) inside the content box so
/// they don't jam against the section bar's left edge — applied equally so they stay aligned.
const HELP_TEXT_PAD: f32 = 4.0;
/// Tree indent per depth level + base leading (SwiftUI `depth * 14 + 10`).
const TREE_INDENT: f32 = 14.0;
const TREE_BASE_INDENT: f32 = 10.0;
/// Fixed width of the chevron / spinner column (so names align whether or not a row has a
/// disclosure triangle).
const CHEVRON_W: f32 = 16.0;
/// Folder-icon size in a tree row.
const TREE_ICON_SIZE: f32 = 15.0;
/// Folder-name (and up-row) type size — a touch smaller than body so the tree stays dense.
const TREE_NAME_SIZE: f32 = 13.0;
/// Pitch of one tree row (the 24px row + the 1px inter-row gap) — used to size the scroll
/// region without egui's (circular) auto-measure. Matches the real pitch (a hair generous)
/// so a fitting tree never clips its last row or shows a needless scrollbar.
const TREE_ROW_H: f32 = 25.0;

/// An interaction a panel produced this frame, applied to the core by the shell.
pub enum PanelAction {
    CloseHelp,
    CloseInspector,
    CloseTree,
    SelectTab(InspectorTab),
    CopyDetails,
    CopyText,
    CopyDescribe,
    Ask,
    TreeToggle(PathBuf),
    TreeOpen(PathBuf),
    TreeExtendUp,
}

/// The Inspector's visible state for one frame.
pub struct InspectorFrame {
    pub tab: InspectorTab,
    pub snapshot: InspectorSnapshot,
}

/// The folder tree's visible state for one frame.
pub struct TreeFrame {
    pub rows: Vec<fs_tree::Row>,
    pub parent_name: Option<String>,
}

/// The one-line info readout (`i`): `folder/name · W×H`, a codec badge, and a Live-Photo /
/// animation mark — placed in a bottom corner per the alignment setting.
pub struct InfoLine {
    pub main: String,
    pub codec: String,
    pub is_live: bool,
    pub is_animated: bool,
    pub align: pb_app_core::settings::InfoLineAlign,
}

/// A pure snapshot of whichever rich panels are open — copied out of the core so the
/// egui build closure borrows none of it.
pub struct PanelFrame {
    pub help: Option<HelpPanel>,
    pub inspector: Option<InspectorFrame>,
    pub tree: Option<TreeFrame>,
    pub info: Option<InfoLine>,
    pub dark: bool,
    /// The panel surface alpha (0–255), from Settings ▸ Appearance ▸ *Info panel opacity*
    /// (`info_opacity` — the old HUD opacity). The winit shell has no separate `panel_opacity`
    /// slider (that field is macOS-only), and the CPU HUD's winit panels used `info_opacity`
    /// too, so this restores that behavior *and* makes the existing slider actually move these
    /// panels. egui alpha is linear, so no material-style remap is needed.
    pub panel_alpha: u8,
}

impl PanelFrame {
    pub fn snapshot(core: &AppCore) -> Self {
        let help = core.help_panel_visible().then(|| core.help_panel());
        let inspector = core.inspector_panel_visible().then(|| InspectorFrame {
            tab: core.panels.inspector.unwrap_or(InspectorTab::Details),
            snapshot: core.inspector_snapshot(),
        });
        let tree = core.tree_panel_visible().then(|| TreeFrame {
            rows: core.fs_tree_rows(),
            parent_name: core.fs_tree_parent_name(),
        });
        let info = core
            .info_line_snapshot()
            .map(|(main, codec, is_live, is_animated)| InfoLine {
                main,
                codec,
                is_live,
                is_animated,
                align: core.settings.info_line_align,
            });
        PanelFrame {
            help,
            inspector,
            tree,
            info,
            dark: core.hud_dark,
            panel_alpha: opacity_to_alpha(core.settings.info_opacity),
        }
    }
}

/// Map an opacity percentage (0–100) to a 0–255 alpha — straight linear, since egui just
/// alpha-blends (no frosted material to compensate for).
fn opacity_to_alpha(opacity: u8) -> u8 {
    ((opacity.min(100) as f32 / 100.0) * 255.0).round() as u8
}

/// Lay out the open panels into `ctx`, collecting interactions into `actions`.
pub fn build(ctx: &egui::Context, frame: &PanelFrame, actions: &mut Vec<PanelAction>) {
    let p = Palette::new(frame.dark);
    let alpha = frame.panel_alpha;
    let screen = ctx.screen_rect();
    // The info readout's bottom footprint `(x0, x1, height)` — panels above it that overlap
    // its horizontal span **duck** (shrink their height) so they never cover it.
    let info_span = frame.info.as_ref().map(|info| {
        let (w, h) = info_pill_size(ctx, info);
        let x0 = match info.align {
            InfoLineAlign::Left => screen.left() + EDGE,
            InfoLineAlign::Center => (screen.center().x - w / 2.0).max(screen.left()),
            InfoLineAlign::Right => (screen.right() - EDGE - w).max(screen.left()),
        };
        (x0, x0 + w, h)
    });
    // How much a panel spanning `[px0, px1]` must yield to clear the info line: the pill height
    // plus an `EDGE` gap — so the space between the panel and the line matches the line's own
    // `EDGE` inset from the viewport bottom. Only when the spans overlap (a small horizontal
    // tolerance); opposite-side panels don't move.
    let duck = |px0: f32, px1: f32| -> f32 {
        info_span.map_or(0.0, |(lx0, lx1, h)| {
            let touch = 6.0; // horizontal overlap tolerance
            if px0 < lx1 + touch && lx0 < px1 + touch {
                h + EDGE
            } else {
                0.0
            }
        })
    };

    if let Some(info) = &frame.info {
        info_line(ctx, &p, alpha, info);
    }
    // The tree (top-left) and Inspector (top-right) can coexist; Help (centered) is
    // topmost — draw it last so it sits above the others (SwiftUI z-order).
    if let Some(tree) = &frame.tree {
        let r = duck(screen.left() + EDGE, screen.left() + EDGE + TREE_WIDTH);
        tree_panel(ctx, &p, alpha, r, tree, actions);
    }
    if let Some(insp) = &frame.inspector {
        let r = duck(
            screen.right() - EDGE - INSPECTOR_WIDTH,
            screen.right() - EDGE,
        );
        inspector_panel(ctx, &p, alpha, r, insp, actions);
    }
    if let Some(help) = &frame.help {
        let r = duck(
            screen.center().x - HELP_WIDTH / 2.0,
            screen.center().x + HELP_WIDTH / 2.0,
        );
        help_panel(ctx, &p, alpha, r, help, actions);
    }
}

// ── Info readout (`i`) ───────────────────────────────────────────────────────

/// The info-pill corner radius (SwiftUI 11).
const INFO_RADIUS: f32 = 11.0;
/// The codec badge's inset from the pill's top/right/bottom so its corners run concentric with
/// the pill's (SwiftUI `inset`).
const INFO_INSET: f32 = 6.0;
/// The pill's leading (and no-badge trailing) padding.
const INFO_PAD: f32 = 11.0;
const INFO_TEXT_SIZE: f32 = 13.5;
const INFO_CODEC_SIZE: f32 = 11.0;

/// The info pill's `(width, height)` in points — used both to lay it out and to duck the panels
/// above it. Measured from `ctx` fonts (no `Ui`), matching `info_line`'s hand layout exactly.
fn info_pill_size(ctx: &egui::Context, info: &InfoLine) -> (f32, f32) {
    let measure = |s: &str, size: f32| {
        ctx.fonts(|f| {
            f.layout_no_wrap(
                s.to_owned(),
                FontId::new(size, FontFamily::Proportional),
                Color32::PLACEHOLDER,
            )
            .size()
        })
    };
    let text = measure(&info.main, INFO_TEXT_SIZE);
    let has_icon = info.is_live || info.is_animated;
    let has_codec = !info.codec.is_empty();
    let gap = 8.0;
    let mut w = INFO_PAD + text.x;
    if has_icon {
        w += gap + INFO_TEXT_SIZE + 1.0; // icon column
    }
    w += if has_codec {
        gap + measure(&info.codec, INFO_CODEC_SIZE).x + 12.0 + INFO_INSET
    } else {
        INFO_PAD
    };
    (w, text.y + 2.0 * INFO_INSET)
}

/// The one-line info readout: `folder/name · W×H`, an optional Live-Photo / animation mark, and
/// an optional codec badge (a nested round-rect concentric with the pill). Bottom-corner per the
/// alignment setting, non-interactive, laid out by hand so everything shares one vertical center.
fn info_line(ctx: &egui::Context, p: &Palette, alpha: u8, info: &InfoLine) {
    use pb_app_core::settings::InfoLineAlign as A;
    let (anchor, offset) = match info.align {
        A::Left => (Align2::LEFT_BOTTOM, egui::vec2(EDGE, -EDGE)),
        A::Center => (Align2::CENTER_BOTTOM, egui::vec2(0.0, -EDGE)),
        A::Right => (Align2::RIGHT_BOTTOM, egui::vec2(-EDGE, -EDGE)),
    };
    let (w, pill_h) = info_pill_size(ctx, info);
    egui::Area::new(egui::Id::new("pb_info_line"))
        .anchor(anchor, offset)
        .interactable(false)
        .order(egui::Order::Middle)
        .show(ctx, |ui| {
            let has_icon = info.is_live || info.is_animated;
            let icon_sz = INFO_TEXT_SIZE + 1.0;
            let gap = 8.0;
            let text_g = galley(
                ui,
                &info.main,
                FontId::new(INFO_TEXT_SIZE, FontFamily::Proportional),
                p.text,
                f32::INFINITY,
            );
            let text_w = text_g.size().x;
            let (badge_bg, badge_fg) = badge_colors(p);
            let codec_g = (!info.codec.is_empty()).then(|| {
                galley(
                    ui,
                    &info.codec,
                    FontId::new(INFO_CODEC_SIZE, FontFamily::Proportional),
                    badge_fg,
                    f32::INFINITY,
                )
            });
            let badge_w = codec_g.as_ref().map_or(0.0, |g| g.size().x + 12.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(w, pill_h), egui::Sense::hover());

            // Pill: soft shadow, then the translucent surface + hairline border.
            ui.painter().add(
                egui::epaint::Shadow {
                    offset: egui::vec2(0.0, 3.0),
                    blur: 12.0,
                    spread: 0.0,
                    color: Color32::from_black_alpha(60),
                }
                .as_shape(rect, Rounding::same(INFO_RADIUS)),
            );
            let fill = panel_surface(p);
            ui.painter().rect(
                rect,
                Rounding::same(INFO_RADIUS),
                Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), alpha),
                Stroke::new(0.5, separator(p)),
            );

            // Content, all on one vertical center.
            let cy = rect.center().y;
            let mut x = rect.left() + INFO_PAD;
            paint_vtext(ui, x, cy, &text_g);
            x += text_w + gap;
            if has_icon {
                let icon = if info.is_live {
                    Icon::LivePhoto
                } else {
                    Icon::Film
                };
                pb_ui::icon::paint(
                    ui,
                    sq(x + icon_sz / 2.0, cy, icon_sz),
                    icon,
                    Tone::Neutral,
                    p,
                );
                x += icon_sz + gap;
            }
            if let Some(cg) = codec_g {
                let badge_h = pill_h - 2.0 * INFO_INSET;
                let badge = egui::Rect::from_min_size(
                    egui::pos2(x, cy - badge_h / 2.0),
                    egui::vec2(badge_w, badge_h),
                );
                ui.painter().rect(
                    badge,
                    Rounding::same(INFO_RADIUS - INFO_INSET),
                    badge_bg,
                    Stroke::NONE,
                );
                paint_vtext(ui, badge.center().x - cg.size().x / 2.0, cy, &cg);
            }
        });
}

// ── Shared chrome ────────────────────────────────────────────────────────────

/// The panel surface base fill (before translucency) — a frosted-material stand-in,
/// lifted above `page`/`card` so text stays high-contrast over a photo (dark mode's
/// `card` at 0x2b reads too murky through the translucency).
fn panel_surface(p: &Palette) -> Color32 {
    if p.dark {
        Color32::from_gray(0x33)
    } else {
        Color32::from_gray(0xf8)
    }
}

/// The translucent panel surface (fill + hairline border + rounded corners + shadow),
/// matching SwiftUI `panelBackground`. `alpha` is the user's *Panel opacity* setting.
fn panel_frame(p: &Palette, alpha: u8) -> egui::Frame {
    let fill = panel_surface(p);
    egui::Frame::none()
        .fill(Color32::from_rgba_unmultiplied(
            fill.r(),
            fill.g(),
            fill.b(),
            alpha,
        ))
        .stroke(Stroke::new(0.5, separator(p)))
        .rounding(Rounding::same(PANEL_RADIUS))
        .shadow(egui::epaint::Shadow {
            offset: egui::vec2(0.0, 5.0),
            blur: 18.0,
            spread: 0.0,
            color: Color32::from_black_alpha(70),
        })
}

/// The opaque stand-in for `.secondary` used on icons / labels / ✕ so they stay legible
/// over the translucent material (SwiftUI `Color.panelSecondary`).
fn panel_secondary(p: &Palette) -> Color32 {
    if p.dark {
        Color32::from_gray(163) // white 0.64
    } else {
        Color32::from_gray(107) // white 0.42
    }
}

/// A 1px groove divider (SwiftUI `Color.primary.opacity(0.08)`).
fn separator(p: &Palette) -> Color32 {
    if p.dark {
        Color32::from_white_alpha(28)
    } else {
        Color32::from_black_alpha(28)
    }
}

/// The `quaternary`-style fill for keycaps / count badges / the tab track.
fn quaternary(p: &Palette) -> Color32 {
    if p.dark {
        Color32::from_white_alpha(28)
    } else {
        Color32::from_black_alpha(20)
    }
}

/// Fixed height of a panel header row — the title/tabs and the ✕/copy buttons are
/// vertically centered inside it, so the top and bottom gaps match (systematized across
/// the tree, Help, and the Inspector so no panel hand-rolls a taller/looser header).
const HEADER_H: f32 = 34.0;
/// Left/right inset of a header row.
const HEADER_PAD_H: f32 = 14.0;
/// The header title type size (semibold).
const TITLE_SIZE: f32 = 13.5;

/// Allocate the header row and place the right-hand ✕ (and optional copy) buttons — both
/// drawn geometrically, so they're truly vertically centered (no font-box drift). Returns
/// the header rect, its vertical center `cy`, the `x` where the right controls begin (so
/// left content stops there), and `(copy_clicked, close_clicked)`. The one place header
/// geometry lives; every panel builds its header on it.
fn header_bar(
    ui: &mut egui::Ui,
    p: &Palette,
    copy_tooltip: Option<&str>,
) -> (egui::Rect, f32, f32, bool, bool) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), HEADER_H),
        egui::Sense::hover(),
    );
    let cy = rect.center().y;
    let icon = 20.0;
    let mut cx = rect.right() - HEADER_PAD_H - icon / 2.0;
    let close_rect = sq(cx, cy, icon);
    let close_resp = icon_hit(ui, close_rect, "hdr_close");
    draw_x(ui.painter(), close_rect, icon_tone(close_resp.hovered(), p));
    let mut copy = false;
    cx -= icon + 4.0;
    if let Some(tip) = copy_tooltip {
        let copy_rect = sq(cx, cy, icon);
        let copy_resp = icon_hit(ui, copy_rect, "hdr_copy").on_hover_text(tip);
        draw_copy(
            ui.painter(),
            copy_rect,
            panel_surface(p),
            icon_tone(copy_resp.hovered(), p),
        );
        copy = copy_resp.clicked();
        cx -= icon + 4.0;
    }
    (rect, cy, cx - icon / 2.0, copy, close_resp.clicked())
}

/// A titled panel header (the tree / Help): `title` on the left (optically v-centered), ✕
/// (and optional copy) pinned right. Returns `(copy_clicked, close_clicked)`.
fn panel_header(
    ui: &mut egui::Ui,
    p: &Palette,
    title: &str,
    copy_tooltip: Option<&str>,
) -> (bool, bool) {
    let (rect, cy, _controls_left, copy, close) = header_bar(ui, p, copy_tooltip);
    let g = galley(
        ui,
        title,
        FontId::new(TITLE_SIZE, FontFamily::Name(pb_ui::SEMIBOLD.into())),
        p.text,
        f32::INFINITY,
    );
    paint_vtext(ui, rect.left() + HEADER_PAD_H, cy, &g);
    (copy, close)
}

// ── Systematic vertical text centering (the egui pain point) ──────────────────
// Placing text next to a geometrically-drawn icon and centering the galley *box* reads
// misaligned — the box carries font leading and pb_ui's `y_offset` tweak lifts the ink
// within it, so the ink's optical center is off the box center. Hand-placed text (via
// `galley` + `paint_vtext`) is nudged by TEXT_LIFT × its height so the ink lands on the
// target center. One knob, calibrated in the shot against the drawn icons.

/// Signed fraction of a line's height by which hand-placed text is nudged *down* from a
/// naive box-center so its ink optically centers on a row. egui's glyph box already sits
/// high (pb_ui's `y_offset` tweak lifts the ink within the box), so the box-center reads
/// high and we push back down — hence negative. Calibrated against the geometrically-drawn
/// icons in `--egui-shot`: the title, folder names, and pill numbers all sit on the icon
/// centerline. One knob for every panel's vertical alignment.
const TEXT_LIFT: f32 = -0.075;

/// A single-line galley for `text`, ellipsis-truncated to `max_w` (`f32::INFINITY` = none).
/// The color is baked in; paint it with [`paint_vtext`].
fn galley(
    ui: &egui::Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_w: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: font,
            color,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping {
        max_width: max_w,
        max_rows: 1,
        overflow_character: Some('…'),
        ..Default::default()
    };
    ui.fonts(|f| f.layout_job(job))
}

/// Paint a colored galley `g` with its optical center on `cy`, left edge at `x`.
fn paint_vtext(ui: &egui::Ui, x: f32, cy: f32, g: &std::sync::Arc<egui::Galley>) {
    let y = cy - g.size().y * (0.5 + TEXT_LIFT);
    ui.painter()
        .galley(egui::pos2(x, y), g.clone(), Color32::PLACEHOLDER);
}

/// A hoverable drawn-icon color.
fn icon_tone(hovered: bool, p: &Palette) -> Color32 {
    if hovered {
        p.text
    } else {
        panel_secondary(p)
    }
}

/// Interact with a square icon button at `rect` (pointer cursor on hover). The caller draws
/// the glyph into the same rect with a `draw_*` helper — so geometry (and thus vertical
/// centering) is the caller's, not egui's. `salt` must be unique within the panel (include
/// the row path for per-row buttons, or egui reports an ID clash).
fn icon_hit(ui: &mut egui::Ui, rect: egui::Rect, salt: impl std::hash::Hash) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with(salt), egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// A square rect of side `s` centered on `(cx, cy)`.
fn sq(cx: f32, cy: f32, s: f32) -> egui::Rect {
    egui::Rect::from_center_size(egui::pos2(cx, cy), egui::vec2(s, s))
}

/// Draw a ✕ into `rect`.
fn draw_x(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.28);
    let s = Stroke::new(1.6, color);
    painter.line_segment([r.left_top(), r.right_bottom()], s);
    painter.line_segment([r.right_top(), r.left_bottom()], s);
}

/// Draw a copy (two-documents) glyph into `rect`; `bg` fills the front sheet so the overlap
/// reads.
fn draw_copy(painter: &egui::Painter, rect: egui::Rect, bg: Color32, color: Color32) {
    let s = Stroke::new(1.3, color);
    let back = egui::Rect::from_min_size(rect.min + egui::vec2(7.0, 3.0), egui::vec2(9.0, 11.0));
    let front = egui::Rect::from_min_size(rect.min + egui::vec2(3.0, 6.0), egui::vec2(9.0, 11.0));
    painter.rect_stroke(back, Rounding::same(2.0), s);
    painter.rect_filled(front, Rounding::same(2.0), bg);
    painter.rect_stroke(front, Rounding::same(2.0), s);
}

/// Draw a disclosure chevron into `rect` — right (collapsed) / down (expanded).
fn draw_chevron(painter: &egui::Painter, rect: egui::Rect, color: Color32, expanded: bool) {
    let c = rect.center();
    let s = 4.4;
    let pts = if expanded {
        vec![
            egui::pos2(c.x - s, c.y - s * 0.6),
            egui::pos2(c.x + s, c.y - s * 0.6),
            egui::pos2(c.x, c.y + s * 0.9),
        ]
    } else {
        vec![
            egui::pos2(c.x - s * 0.6, c.y - s),
            egui::pos2(c.x + s * 0.9, c.y),
            egui::pos2(c.x - s * 0.6, c.y + s),
        ]
    };
    painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// Draw an up-left arrow (go-to-parent) into `rect`.
fn draw_up_arrow(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.22);
    let s = Stroke::new(1.6, color);
    let tip = r.left_top();
    painter.line_segment([r.right_bottom(), tip], s);
    let a = 5.0;
    painter.line_segment([tip, egui::pos2(tip.x + a, tip.y)], s);
    painter.line_segment([tip, egui::pos2(tip.x, tip.y + a)], s);
}

/// A thin groove line spanning the panel width.
fn groove(ui: &mut egui::Ui, p: &Palette) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, separator(p));
}

/// The usable height for a scrolling panel body: the screen height minus the top+bottom
/// edge insets and the panel's own chrome height.
/// The tallest a whole panel may grow before it must scroll: the window height minus the
/// top+bottom edge insets. Applied as the `egui::Window`'s `max_height` (capping the
/// *window*, not a ScrollArea inside an auto-sizing window — that mis-measures and leaves
/// the panel far shorter than the space allows, task #54 #2).
fn panel_max_height(ctx: &egui::Context, floor: f32, duck: f32) -> f32 {
    (ctx.screen_rect().height() - 2.0 * EDGE - duck).max(floor)
}

/// A scroll body that fits its content up to `max_h`, then scrolls — usable inside an
/// auto-sizing `egui::Window` (where a plain `ScrollArea` can't measure its own available
/// height and so collapses far shorter than the space allows, task #54 #2). It remembers
/// the content height (keyed by `id`) and sizes to it the next frame, requesting a repaint
/// so the one-frame settle is instant. Also drops pb_ui's 32px control-height minimum so
/// dense rows hug their content.
fn scroll_body<R>(
    ui: &mut egui::Ui,
    id: &str,
    max_h: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mem = egui::Id::new(("pb_scroll_h", id));
    let measured = ui.ctx().data(|d| d.get_temp::<f32>(mem));
    let h = measured.map(|m| m.min(max_h)).unwrap_or(max_h).max(60.0);
    let out = egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(h)
        .min_scrolled_height(h)
        .show(ui, |ui| {
            ui.spacing_mut().interact_size.y = 0.0;
            add(ui)
        });
    let content = out.content_size.y;
    if measured.is_none_or(|m| (m - content).abs() > 0.5) {
        ui.ctx().data_mut(|d| d.insert_temp(mem, content));
        ui.ctx().request_repaint();
    }
    out.inner
}

// ── Help panel ───────────────────────────────────────────────────────────────

fn help_panel(
    ctx: &egui::Context,
    p: &Palette,
    alpha: u8,
    duck: f32,
    help: &HelpPanel,
    actions: &mut Vec<PanelAction>,
) {
    let max_h = panel_max_height(ctx, 220.0, duck);
    egui::Window::new("pb_help")
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .max_height(max_h)
        .frame(panel_frame(p, alpha))
        .show(ctx, |ui| {
            ui.set_width(HELP_WIDTH);
            ui.set_max_width(HELP_WIDTH);
            ui.spacing_mut().item_spacing.y = 0.0;
            let (_, close) = panel_header(ui, p, "Keyboard Shortcuts", None);
            if close {
                actions.push(PanelAction::CloseHelp);
            }
            groove(ui, p);
            scroll_body(ui, "help", max_h - HEADER_H, |ui| {
                egui::Frame::none()
                    .inner_margin(egui::Margin::symmetric(18.0, 14.0))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 14.0;
                        for section in &help.sections {
                            help_section(ui, p, section);
                        }
                    });
            });
        });
}

fn help_section(ui: &mut egui::Ui, p: &Palette, section: &pb_app_core::panels::HelpSection) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 6.0; // heading bar → columns
                                               // The *real* content width — `available_width` when the body scrolls has the scrollbar
                                               // already subtracted, so the bar and the right column's keycaps share one right edge
                                               // (capped at the no-scrollbar width so an unbounded auto-size can't grow the panel).
        let content_w = ui.available_width().min(HELP_WIDTH - 2.0 * 18.0);
        // Section heading: a tint bar spanning exactly the content box — left edge on the
        // descriptions, right edge on the keycaps — with the heading text left-aligned to the
        // descriptions and v-centered (the systematized `paint_vtext`).
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(content_w, HELP_SECTION_H), egui::Sense::hover());
        ui.painter()
            .rect(rect, Rounding::same(6.0), quaternary(p), Stroke::NONE);
        let hg = galley(
            ui,
            &section.title,
            FontId::new(HELP_TEXT_SIZE, FontFamily::Name(pb_ui::SEMIBOLD.into())),
            p.text,
            f32::INFINITY,
        );
        paint_vtext(ui, rect.left() + HELP_TEXT_PAD, rect.center().y, &hg);
        // Two balanced columns (a simple even split reads like SwiftUI's column-major at this
        // size).
        let col_w = (content_w - 28.0) / 2.0;
        let rows = &section.rows;
        let mid = rows.len().div_ceil(2);
        ui.horizontal_top(|ui| {
            // Zero egui's inter-item gap so the only spacing is the explicit 28px below — else
            // the right column (and its right-aligned keycaps) sits `item_spacing.x` past
            // `content_w` while the section bar stops at `content_w`, so their right edges drift.
            ui.spacing_mut().item_spacing.x = 0.0;
            help_column(ui, p, &rows[..mid], col_w);
            ui.add_space(28.0);
            help_column(ui, p, &rows[mid..], col_w);
        });
    });
}

fn help_column(ui: &mut egui::Ui, p: &Palette, rows: &[(String, String)], width: f32) {
    // Force a vertical layout: the parent is a `horizontal_top` (the two columns), and a
    // plain `allocate_ui` would inherit that direction and lay the rows out side by side.
    ui.allocate_ui_with_layout(
        egui::vec2(width, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 2.0; // tight — rows are fixed-height
            ui.spacing_mut().interact_size.y = 0.0; // drop pb_ui's 32px control-height minimum
            for (label, shortcut) in rows {
                help_row(ui, p, label, shortcut, width);
            }
        },
    );
}

/// One shortcut row, laid out by hand so the description text, the keycaps, and the "/"
/// separators all share one vertical center: description left (truncated), keycaps
/// right-aligned.
fn help_row(ui: &mut egui::Ui, p: &Palette, label: &str, shortcut: &str, col_w: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(col_w, HELP_ROW_H), egui::Sense::hover());
    let cy = rect.center().y;
    let keys_w = draw_keycaps(ui, p, shortcut, rect.right(), cy);
    // Match the section heading's small left pad so headings and descriptions align.
    let avail = (rect.width() - HELP_TEXT_PAD - keys_w - 8.0).max(20.0);
    let g = galley(
        ui,
        label,
        FontId::new(HELP_TEXT_SIZE, FontFamily::Proportional),
        p.text,
        avail,
    );
    paint_vtext(ui, rect.left() + HELP_TEXT_PAD, cy, &g);
}

/// Draw a shortcut's keycaps (and "/" separators) **right-aligned** ending at `right_x`,
/// v-centered on `cy`. Returns the total width used. A shortcut is groups split on " / ",
/// each group's keys split on whitespace.
fn draw_keycaps(ui: &mut egui::Ui, p: &Palette, shortcut: &str, right_x: f32, cy: f32) -> f32 {
    if shortcut.trim().is_empty() {
        return 0.0;
    }
    // `Some(cap)` = one keycap's text; `None` = a "/" separator between alternatives. A **chord**
    // (an alternative carrying a modifier — the mac ⇧⌘⌥⌃ glyphs or a Windows "Shift+…") renders
    // as ONE keycap so the modifier stays glued to its key (⇧R, not ⇧ · R); a modifier-less
    // alternative (the Pan arrows) splits into one cap per key.
    let is_chord = |alt: &str| alt.contains(['\u{21e7}', '\u{2318}', '\u{2325}', '\u{2303}', '+']);
    let mut tokens: Vec<Option<String>> = Vec::new();
    for (gi, alt) in shortcut.split(" / ").enumerate() {
        if gi > 0 {
            tokens.push(None);
        }
        let alt = alt.trim();
        if is_chord(alt) {
            tokens.push(Some(alt.to_string()));
        } else {
            tokens.extend(alt.split_whitespace().map(|k| Some(k.to_string())));
        }
    }
    let gap = 5.0;
    let key_font = FontId::new(HELP_KEY_SIZE, FontFamily::Proportional);
    let (bg, key_col) = badge_colors(p);
    let text_w = |ui: &egui::Ui, s: &str| {
        galley(ui, s, key_font.clone(), Color32::PLACEHOLDER, f32::INFINITY)
            .size()
            .x
    };
    let widths: Vec<f32> = tokens
        .iter()
        .map(|t| match t {
            Some(cap) => text_w(ui, cap) + 14.0,
            None => text_w(ui, "/") + 2.0,
        })
        .collect();
    let total = widths.iter().sum::<f32>() + gap * tokens.len().saturating_sub(1) as f32;
    let mut x = right_x - total;
    for (t, w) in tokens.iter().zip(&widths) {
        match t {
            Some(cap) => {
                let rect = egui::Rect::from_min_size(
                    egui::pos2(x, cy - HELP_KEYCAP_H / 2.0),
                    egui::vec2(*w, HELP_KEYCAP_H),
                );
                ui.painter().rect(
                    rect,
                    Rounding::same(5.0),
                    bg,
                    Stroke::new(0.5, separator(p)),
                );
                let g = galley(ui, cap, key_font.clone(), key_col, f32::INFINITY);
                paint_vtext(ui, rect.center().x - g.size().x / 2.0, cy, &g);
            }
            None => {
                let g = galley(ui, "/", key_font.clone(), panel_secondary(p), f32::INFINITY);
                paint_vtext(ui, x + (*w - g.size().x) / 2.0, cy, &g);
            }
        }
        x += w + gap;
    }
    total
}

/// A dark, translucent badge fill + a bright label — the high-contrast look shared by the
/// folder-tree count pills and the help keycaps (the mid-gray `quaternary` washes out over a
/// photo). Heavier alpha in light mode: a translucent dark fill over a light panel lands far
/// lighter than its alpha (linear-light compositing).
fn badge_colors(p: &Palette) -> (Color32, Color32) {
    if p.dark {
        (
            Color32::from_rgba_unmultiplied(0, 0, 0, 130),
            Color32::from_gray(228),
        )
    } else {
        (
            Color32::from_rgba_unmultiplied(0, 0, 0, 210),
            Color32::from_gray(250),
        )
    }
}

// ── Inspector panel ──────────────────────────────────────────────────────────

fn inspector_panel(
    ctx: &egui::Context,
    p: &Palette,
    alpha: u8,
    duck: f32,
    insp: &InspectorFrame,
    actions: &mut Vec<PanelAction>,
) {
    let max_h = panel_max_height(ctx, 200.0, duck);
    egui::Window::new("pb_inspector")
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .anchor(Align2::RIGHT_TOP, [-EDGE, EDGE])
        .max_height(max_h)
        .frame(panel_frame(p, alpha))
        .show(ctx, |ui| {
            ui.set_width(INSPECTOR_WIDTH);
            ui.set_max_width(INSPECTOR_WIDTH);
            ui.spacing_mut().item_spacing.y = 0.0;
            // The tab bar is the header (no redundant title); it rides the shared header
            // scaffold so its height/centering/close match the tree + Help.
            let tab = insp.tab;
            let (rect, cy, controls_left, copy, close) = header_bar(ui, p, Some(copy_label(tab)));
            inspector_tabs(ui, p, rect, cy, controls_left, tab, actions);
            if close {
                actions.push(PanelAction::CloseInspector);
            }
            if copy {
                actions.push(copy_action(tab));
            }
            groove(ui, p);
            // Key the remembered content height by tab, so switching tabs re-measures.
            let id = match tab {
                InspectorTab::Details => "insp_details",
                InspectorTab::Text => "insp_text",
                InspectorTab::Describe => "insp_describe",
            };
            scroll_body(ui, id, max_h - HEADER_H, |ui| {
                egui::Frame::none()
                    .inner_margin(egui::Margin::symmetric(16.0, 14.0))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 8.0;
                        match &insp.snapshot {
                            InspectorSnapshot::Details(d) => details_body(ui, p, d),
                            InspectorSnapshot::Text(t) => text_body(ui, p, t),
                            InspectorSnapshot::Describe(d) => describe_body(ui, p, d, actions),
                        }
                    });
            });
        });
}

/// The segmented tab control (Details / Text / Describe), drawn manually into the header so
/// the selected pill, the icons, and the label text all share one vertical center — a
/// rounded track, the selected segment filled accent with white text. Sized to its labels
/// and placed at `header_rect.left()+pad`, vertically centered on `cy`.
fn inspector_tabs(
    ui: &mut egui::Ui,
    p: &Palette,
    header_rect: egui::Rect,
    cy: f32,
    controls_left: f32,
    current: InspectorTab,
    actions: &mut Vec<PanelAction>,
) {
    // Icons roughly match the SwiftUI SF Symbols (info.circle / text.viewfinder / sparkles).
    let tabs = [
        (InspectorTab::Details, "Details", Icon::Info),
        (InspectorTab::Text, "Text", Icon::Text),
        (InspectorTab::Describe, "Describe", Icon::Sparkles),
    ];
    let font = FontId::new(12.5, FontFamily::Proportional);
    let seg_pad = 10.0;
    let icon_sz = 12.5;
    let icon_gap = 5.0;
    let track_h = 24.0;
    // Each segment sizes to its icon + gap + label; clamp the whole track so it never runs
    // into the right-hand controls.
    let widths: Vec<f32> = tabs
        .iter()
        .map(|(_, l, _)| {
            icon_sz
                + icon_gap
                + galley(ui, l, font.clone(), Color32::PLACEHOLDER, f32::INFINITY)
                    .size()
                    .x
                + seg_pad * 2.0
        })
        .collect();
    let track_left = header_rect.left() + HEADER_PAD_H;
    let total = widths.iter().sum::<f32>() + 4.0;
    let total = total.min((controls_left - 8.0 - track_left).max(60.0));
    let track = egui::Rect::from_min_size(
        egui::pos2(track_left, cy - track_h / 2.0),
        egui::vec2(total, track_h),
    );
    ui.painter()
        .rect(track, Rounding::same(7.0), quaternary(p), Stroke::NONE);
    let mut x = track_left + 2.0;
    for ((tab, label, icon), w) in tabs.iter().zip(widths) {
        let seg = egui::Rect::from_min_size(
            egui::pos2(x, track.top() + 2.0),
            egui::vec2(w, track_h - 4.0),
        );
        let resp = ui.interact(seg, ui.id().with(("tab", *label)), egui::Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let selected = *tab == current;
        if selected {
            ui.painter()
                .rect(seg, Rounding::same(5.0), p.accent, Stroke::NONE);
        }
        let color = if selected {
            Color32::WHITE
        } else {
            panel_secondary(p)
        };
        // Center the [icon · gap · label] group in the segment; both share `cy`.
        let g = galley(ui, label, font.clone(), color, f32::INFINITY);
        let group_w = icon_sz + icon_gap + g.size().x;
        let gx = seg.center().x - group_w / 2.0;
        pb_ui::icon::paint_tinted(ui, sq(gx + icon_sz / 2.0, cy, icon_sz), *icon, color);
        paint_vtext(ui, gx + icon_sz + icon_gap, cy, &g);
        if resp.clicked() && !selected {
            actions.push(PanelAction::SelectTab(*tab));
        }
        x += w;
    }
}

fn copy_label(tab: InspectorTab) -> &'static str {
    match tab {
        InspectorTab::Details => "Copy details",
        InspectorTab::Text => "Copy all text",
        InspectorTab::Describe => "Copy description",
    }
}

fn copy_action(tab: InspectorTab) -> PanelAction {
    match tab {
        InspectorTab::Details => PanelAction::CopyDetails,
        InspectorTab::Text => PanelAction::CopyText,
        InspectorTab::Describe => PanelAction::CopyDescribe,
    }
}

fn details_body(ui: &mut egui::Ui, p: &Palette, d: &DetailsPanel) {
    if d.rows.is_empty() {
        ui.label(RichText::new("Nothing to show").color(panel_secondary(p)));
        return;
    }
    // Pin the content width so long EXIF values (e.g. the Flash string) **wrap** inside the
    // panel instead of widening the whole Window — egui grows an auto-sized Window to fit a
    // non-wrapping row, which is what blew the panel out to full width. Everything below
    // wraps at this width. (Leaves room for the vertical scrollbar the tab usually shows.)
    let content_w = INSPECTOR_WIDTH - 2.0 * 16.0 - 8.0;
    ui.set_width(content_w);
    for row in &d.rows {
        match row {
            DetailRow::Span { text, bold } => {
                let font = if *bold {
                    FontId::new(DETAIL_HEADER_SIZE, FontFamily::Name(pb_ui::SEMIBOLD.into()))
                } else {
                    FontId::new(DETAIL_SIZE, FontFamily::Proportional)
                };
                ui.add(
                    egui::Label::new(RichText::new(text).font(font).color(p.text))
                        .wrap()
                        .selectable(true),
                );
            }
            DetailRow::Pair { label, value } => {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    // Fixed leading label column; a long label (ComponentsConfiguration…)
                    // wraps within it rather than shoving the value column right.
                    ui.allocate_ui(egui::vec2(PAIR_LABEL_W, 0.0), |ui| {
                        ui.set_width(PAIR_LABEL_W);
                        ui.add(
                            egui::Label::new(
                                RichText::new(label)
                                    .size(DETAIL_SIZE)
                                    .color(panel_secondary(p)),
                            )
                            .wrap(),
                        );
                    });
                    // Value fills the remaining width and wraps.
                    ui.add(
                        egui::Label::new(RichText::new(value).size(DETAIL_SIZE).color(p.text))
                            .wrap()
                            .selectable(true),
                    );
                });
            }
        }
    }
}

fn text_body(ui: &mut egui::Ui, p: &Palette, t: &TextPanel) {
    match &t.body {
        TextBody::NoPhoto => {}
        TextBody::Scanning => {
            ui.label(RichText::new("Reading text…").color(panel_secondary(p)));
        }
        TextBody::Ready {
            qr,
            paragraphs,
            ocr_error,
        } => {
            for q in qr {
                ui.add(
                    egui::Label::new(RichText::new(format!("QR code → {q}")).color(p.text))
                        .selectable(true),
                );
            }
            for para in paragraphs {
                ui.add(egui::Label::new(RichText::new(para).color(p.text)).selectable(true));
            }
            if paragraphs.is_empty() {
                if let Some(e) = ocr_error {
                    ui.label(RichText::new(e).color(panel_secondary(p)));
                } else if qr.is_empty() {
                    ui.label(RichText::new("No text found").color(panel_secondary(p)));
                }
            }
        }
    }
}

fn describe_body(
    ui: &mut egui::Ui,
    p: &Palette,
    d: &DescribePanel,
    actions: &mut Vec<PanelAction>,
) {
    // Empty deck → nothing (no header over a photo-less state).
    if matches!(d.body, DescribeBody::NoPhoto) {
        return;
    }
    // A stable "Description" heading carrying the **Ask** button (matches the SwiftUI kind-0
    // header): always present so the Ask affordance doesn't jump as the body state changes.
    describe_header(ui, p, actions);
    match &d.body {
        DescribeBody::NoPhoto => {}
        DescribeBody::Idle => {
            ui.label(RichText::new("Press D to describe this image.").color(panel_secondary(p)));
        }
        DescribeBody::Busy => {
            ui.label(RichText::new("Describing…").color(panel_secondary(p)));
        }
        DescribeBody::Ready(text) => {
            // Render the AI text as block-level Markdown (headings / lists / emphasis), like
            // the SwiftUI Describe tab. Wrap at the pinned content width so nothing widens the
            // panel (the Details-tab lesson).
            let wrap_w = INSPECTOR_WIDTH - 2.0 * 16.0 - 8.0;
            crate::md::render(ui, p, text, wrap_w);
        }
        DescribeBody::Error(text) => {
            ui.add(
                egui::Label::new(RichText::new(text).color(panel_secondary(p)))
                    .wrap()
                    .selectable(true),
            );
        }
    }
}

/// The Describe tab's "Description" heading + right-aligned **Ask** button (a
/// message-question icon + "Ask" in the accent). Clicking opens the ask-a-question dialog
/// (`Action::AskImage`). Mirrors the SwiftUI header's Ask affordance. Laid out **by hand**
/// (like the panel headers) so the 14pt heading, the 13pt "Ask", and the icon all sit on one
/// centerline — egui's `horizontal` won't center mixed sizes + an icon.
fn describe_header(ui: &mut egui::Ui, p: &Palette, actions: &mut Vec<PanelAction>) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::hover());
    let cy = rect.center().y;
    let title = galley(
        ui,
        "Description",
        FontId::new(14.0, FontFamily::Name(pb_ui::SEMIBOLD.into())),
        p.text,
        f32::INFINITY,
    );
    paint_vtext(ui, rect.left(), cy, &title);
    // Ask affordance on the right: [icon · "Ask"], both accent, one click target.
    let ask = galley(
        ui,
        "Ask",
        FontId::new(13.0, FontFamily::Proportional),
        p.accent,
        f32::INFINITY,
    );
    let icon_sz = 13.0;
    let gap = 5.0;
    let group_w = icon_sz + gap + ask.size().x;
    let gx = rect.right() - group_w;
    pb_ui::icon::paint(
        ui,
        sq(gx + icon_sz / 2.0, cy, icon_sz),
        Icon::MessageQuestion,
        Tone::Accent,
        p,
    );
    paint_vtext(ui, gx + icon_sz + gap, cy, &ask);
    let hit = egui::Rect::from_min_max(egui::pos2(gx - 3.0, rect.top()), rect.right_bottom());
    let resp = ui.interact(hit, ui.id().with("ask_btn"), egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        actions.push(PanelAction::Ask);
    }
}

// ── Folder-tree panel ────────────────────────────────────────────────────────

fn tree_panel(
    ctx: &egui::Context,
    p: &Palette,
    alpha: u8,
    duck: f32,
    tree: &TreeFrame,
    actions: &mut Vec<PanelAction>,
) {
    let max_h = panel_max_height(ctx, 200.0, duck);
    egui::Window::new("pb_tree")
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .anchor(Align2::LEFT_TOP, [EDGE, EDGE])
        .max_height(max_h)
        .frame(panel_frame(p, alpha))
        .show(ctx, |ui| {
            ui.set_width(TREE_WIDTH);
            ui.set_max_width(TREE_WIDTH);
            // No inter-element gap: header, groove, and body stack flush, so the panel is
            // exactly its content height and honors the bottom edge inset (#1). Each region
            // owns its own internal padding.
            ui.spacing_mut().item_spacing.y = 0.0;
            let (_, close) = panel_header(ui, p, "Folders", None);
            if close {
                actions.push(PanelAction::CloseTree);
            }
            groove(ui, p);
            // A ScrollArea inside an auto-sizing Window can't measure its own available
            // height (circular), so it collapses far shorter than the space allows (#2).
            // Give it a *definite* height: estimate the content from the row count, cap at
            // the available height, and fill exactly that (auto_shrink off) — fit-to-content
            // when it's short, scroll when it overflows.
            let rows = tree.rows.len() + tree.parent_name.is_some() as usize;
            let body_top = HEADER_H + 1.0; // header + groove
            let est = rows as f32 * TREE_ROW_H + 12.0; // + the list's top/bottom padding
            let body_h = est.min((max_h - body_top).max(80.0));
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(body_h)
                .min_scrolled_height(body_h)
                .show(ui, |ui| {
                    egui::Frame::none()
                        .inner_margin(egui::Margin::symmetric(0.0, 6.0))
                        .show(ui, |ui| {
                            // Dense list: drop pb_ui's 32px control-height minimum so rows
                            // hug their content instead of each nested layout reserving 32px.
                            ui.spacing_mut().interact_size.y = 0.0;
                            ui.spacing_mut().item_spacing.y = 1.0;
                            if let Some(parent) = &tree.parent_name {
                                up_row(ui, p, parent, actions);
                            }
                            for row in &tree.rows {
                                tree_row(ui, p, row, actions);
                            }
                        });
                });
        });
}

/// The "up to parent" affordance at the top of the tree — a drawn up-left arrow + the parent
/// folder's name; clicking climbs a level. **Outdented** a level left of the depth-0 rows
/// (the arrow drops the chevron-column offset the folders carry) so the parent reads as a
/// level up, not a sibling.
fn up_row(ui: &mut egui::Ui, p: &Palette, parent: &str, actions: &mut Vec<PanelAction>) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), TREE_ROW_H),
        egui::Sense::hover(),
    );
    let cy = rect.center().y;
    let icon_x = rect.left() + TREE_BASE_INDENT;
    draw_up_arrow(
        ui.painter(),
        sq(icon_x + TREE_ICON_SIZE / 2.0, cy, TREE_ICON_SIZE),
        panel_secondary(p),
    );
    let nx = icon_x + TREE_ICON_SIZE + 6.0;
    let g = galley(
        ui,
        parent,
        FontId::new(TREE_NAME_SIZE, FontFamily::Proportional),
        p.text,
        rect.right() - 10.0 - nx,
    );
    paint_vtext(ui, nx, cy, &g);
    let resp = ui.interact(rect, ui.id().with("tree_up"), egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        actions.push(PanelAction::TreeExtendUp);
    }
}

/// One folder row, laid out **by hand** so the chevron, folder icon, name, and count pill
/// all share one vertical center (egui's row auto-centering reads low) and the icon sits in
/// a fixed-width column (so the current folder's open-folder glyph — a wider FA viewBox —
/// still aligns with the closed folders). No selection band (the owner's call): the current
/// folder reads via its accent open-folder icon + a bold name.
fn tree_row(ui: &mut egui::Ui, p: &Palette, row: &fs_tree::Row, actions: &mut Vec<PanelAction>) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), TREE_ROW_H),
        egui::Sense::hover(),
    );
    let cy = rect.center().y;
    let indent = row.depth as f32 * TREE_INDENT + TREE_BASE_INDENT;
    let chev_x = rect.left() + indent;

    // Chevron column (fixed width so names align with/without a triangle).
    let chev_rect = sq(chev_x + CHEVRON_W / 2.0, cy, CHEVRON_W);
    if row.loading {
        ui.put(chev_rect, egui::Spinner::new().size(11.0));
    } else if row.has_children {
        let resp = icon_hit(ui, chev_rect, ("chev", &row.path)).on_hover_text(if row.expanded {
            "Collapse"
        } else {
            "Expand"
        });
        draw_chevron(
            ui.painter(),
            chev_rect,
            icon_tone(resp.hovered(), p),
            row.expanded,
        );
        if resp.clicked() {
            actions.push(PanelAction::TreeToggle(row.path.clone()));
        }
    }

    // Folder icon in a fixed column.
    let icon_x = chev_x + CHEVRON_W + 4.0;
    let open = row.is_current || row.expanded;
    let icon = if open { Icon::FolderOpen } else { Icon::Folder };
    let tone = if row.is_current {
        Tone::Accent
    } else {
        Tone::Neutral
    };
    pb_ui::icon::paint(
        ui,
        sq(icon_x + TREE_ICON_SIZE / 2.0, cy, TREE_ICON_SIZE),
        icon,
        tone,
        p,
    );

    // Count pill on the right; the name truncates into what's left.
    let mut name_right = rect.right() - 10.0;
    if let Some(count) = row.count {
        name_right -= tree_pill(ui, p, count, name_right, cy) + 6.0;
    }
    let name_x = icon_x + TREE_ICON_SIZE + 6.0;
    let font = if row.is_current {
        FontId::new(TREE_NAME_SIZE, FontFamily::Name(pb_ui::SEMIBOLD.into()))
    } else {
        FontId::new(TREE_NAME_SIZE, FontFamily::Proportional)
    };
    let g = galley(ui, &row.name, font, p.text, (name_right - name_x).max(10.0));
    paint_vtext(ui, name_x, cy, &g);

    // Clicking the icon or name opens the folder (loads its photos); the chevron above only
    // browses.
    let open_rect = egui::Rect::from_min_max(
        egui::pos2(icon_x, rect.top()),
        egui::pos2(name_right, rect.bottom()),
    );
    let resp = ui.interact(
        open_rect,
        ui.id().with(("tree_open", &row.path)),
        egui::Sense::click(),
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        actions.push(PanelAction::TreeOpen(row.path.clone()));
    }
}

/// Draw a photo-count pill ending at `right_x`, its number optically v-centered on `cy` —
/// a **dark, translucent** capsule (high contrast for the light number, unlike the washed
/// `.quaternary` look). Returns its width so the caller reserves the name's room.
fn tree_pill(ui: &egui::Ui, p: &Palette, count: u64, right_x: f32, cy: f32) -> f32 {
    let num = count.to_string();
    // The shared dark-translucent badge look (same as the help keycaps).
    let (bg, num_color) = badge_colors(p);
    let g = galley(
        ui,
        &num,
        FontId::new(10.5, FontFamily::Proportional),
        num_color,
        f32::INFINITY,
    );
    let pill_w = g.size().x + 12.0;
    let pill_h = 16.0;
    let pill = egui::Rect::from_min_size(
        egui::pos2(right_x - pill_w, cy - pill_h / 2.0),
        egui::vec2(pill_w, pill_h),
    );
    ui.painter()
        .rect(pill, Rounding::same(pill_h / 2.0), bg, Stroke::NONE);
    paint_vtext(ui, pill.center().x - g.size().x / 2.0, cy, &g);
    pill_w
}
