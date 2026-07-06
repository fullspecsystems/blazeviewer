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
use pb_app_core::{fs_tree, AppCore, InspectorTab};
use pb_ui::icon::{Icon, Tone};
use pb_ui::Palette;
use std::path::PathBuf;

/// The inset of a panel from the window edge (SwiftUI `Layout.edge`).
const EDGE: f32 = 24.0;
/// The shared panel corner radius (SwiftUI `panelBackground(cornerRadius: 12)`).
const PANEL_RADIUS: f32 = 12.0;
/// Panel body opacity over the photo — legible but lets the image faintly through
/// (SwiftUI `.regularMaterial` at the default 0.92 panel opacity; egui has no blur, so
/// this is flat translucency per ADR-023, kept fairly opaque so text stays crisp).
const PANEL_ALPHA: u8 = 242;
/// Help panel fixed width (SwiftUI `width: 520`).
const HELP_WIDTH: f32 = 520.0;
/// Inspector default/min width (SwiftUI `inspectorWidth` default 360).
const INSPECTOR_WIDTH: f32 = 360.0;
/// Folder-tree default/min width (SwiftUI `treeWidth` default 280).
const TREE_WIDTH: f32 = 280.0;
/// Inspector pair-row label column width (SwiftUI `.frame(width: 116)`).
const PAIR_LABEL_W: f32 = 116.0;
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

/// A pure snapshot of whichever rich panels are open — copied out of the core so the
/// egui build closure borrows none of it.
pub struct PanelFrame {
    pub help: Option<HelpPanel>,
    pub inspector: Option<InspectorFrame>,
    pub tree: Option<TreeFrame>,
    pub dark: bool,
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
        PanelFrame {
            help,
            inspector,
            tree,
            dark: core.hud_dark,
        }
    }
}

/// Lay out the open panels into `ctx`, collecting interactions into `actions`.
pub fn build(ctx: &egui::Context, frame: &PanelFrame, actions: &mut Vec<PanelAction>) {
    let p = Palette::new(frame.dark);
    // The tree (top-left) and Inspector (top-right) can coexist; Help (centered) is
    // topmost — draw it last so it sits above the others (SwiftUI z-order).
    if let Some(tree) = &frame.tree {
        tree_panel(ctx, &p, tree, actions);
    }
    if let Some(insp) = &frame.inspector {
        inspector_panel(ctx, &p, insp, actions);
    }
    if let Some(help) = &frame.help {
        help_panel(ctx, &p, help, actions);
    }
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
/// matching SwiftUI `panelBackground`.
fn panel_frame(p: &Palette) -> egui::Frame {
    let fill = panel_surface(p);
    egui::Frame::none()
        .fill(Color32::from_rgba_unmultiplied(
            fill.r(),
            fill.g(),
            fill.b(),
            PANEL_ALPHA,
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
fn panel_max_height(ctx: &egui::Context, floor: f32) -> f32 {
    (ctx.screen_rect().height() - 2.0 * EDGE).max(floor)
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

fn help_panel(ctx: &egui::Context, p: &Palette, help: &HelpPanel, actions: &mut Vec<PanelAction>) {
    let max_h = panel_max_height(ctx, 220.0);
    egui::Window::new("pb_help")
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .max_height(max_h)
        .frame(panel_frame(p))
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
                        ui.spacing_mut().item_spacing.y = 20.0;
                        for section in &help.sections {
                            help_section(ui, p, section);
                        }
                    });
            });
        });
}

fn help_section(ui: &mut egui::Ui, p: &Palette, section: &pb_app_core::panels::HelpSection) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 9.0;
        // Section heading over a faint tint bar.
        egui::Frame::none()
            .fill(quaternary(p))
            .rounding(Rounding::same(6.0))
            .inner_margin(egui::Margin::symmetric(8.0, 4.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(&section.title)
                        .font(FontId::new(14.0, FontFamily::Name(pb_ui::SEMIBOLD.into())))
                        .color(p.text),
                );
            });
        // Two balanced columns (SwiftUI splits column-major; a simple even split reads the
        // same at this size). Column width is derived from the fixed panel width, NOT
        // `available_width` — inside an auto-sizing `Window` the latter is unbounded, so
        // a content-driven width would grow the whole panel past HELP_WIDTH.
        let content_w = HELP_WIDTH - 2.0 * 18.0; // minus the body frame's h-margins
        let col_w = (content_w - 28.0) / 2.0;
        let rows = &section.rows;
        let mid = rows.len().div_ceil(2);
        ui.horizontal_top(|ui| {
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
            ui.spacing_mut().item_spacing.y = 8.0;
            for (label, shortcut) in rows {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(label).color(p.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        keycaps(ui, p, shortcut);
                    });
                });
            }
        },
    );
}

/// Render a shortcut string as keycaps: alternatives split on " / ", keys split on space.
fn keycaps(ui: &mut egui::Ui, p: &Palette, shortcut: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        let groups: Vec<&str> = shortcut.split(" / ").collect();
        for (gi, group) in groups.iter().enumerate() {
            if gi > 0 {
                ui.label(RichText::new("/").color(panel_secondary(p)));
            }
            for key in group.split_whitespace() {
                keycap(ui, p, key);
            }
        }
    });
}

fn keycap(ui: &mut egui::Ui, p: &Palette, key: &str) {
    egui::Frame::none()
        .fill(quaternary(p))
        .stroke(Stroke::new(0.5, separator(p)))
        .rounding(Rounding::same(5.0))
        .inner_margin(egui::Margin::symmetric(6.0, 2.0))
        .show(ui, |ui| {
            ui.label(
                RichText::new(key)
                    .font(FontId::new(12.0, FontFamily::Proportional))
                    .color(p.text),
            );
        });
}

// ── Inspector panel ──────────────────────────────────────────────────────────

fn inspector_panel(
    ctx: &egui::Context,
    p: &Palette,
    insp: &InspectorFrame,
    actions: &mut Vec<PanelAction>,
) {
    let max_h = panel_max_height(ctx, 200.0);
    egui::Window::new("pb_inspector")
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .anchor(Align2::RIGHT_TOP, [-EDGE, EDGE])
        .max_height(max_h)
        .frame(panel_frame(p))
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
    let tabs = [
        (InspectorTab::Details, "Details"),
        (InspectorTab::Text, "Text"),
        (InspectorTab::Describe, "Describe"),
    ];
    let font = FontId::new(12.5, FontFamily::Proportional);
    let seg_pad = 11.0;
    let track_h = 24.0;
    // Each segment sizes to its label; clamp the whole track so it never runs into the
    // right-hand controls.
    let widths: Vec<f32> = tabs
        .iter()
        .map(|(_, l)| {
            galley(ui, l, font.clone(), Color32::PLACEHOLDER, f32::INFINITY)
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
    for ((tab, label), w) in tabs.iter().zip(widths) {
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
        let g = galley(ui, label, font.clone(), color, f32::INFINITY);
        paint_vtext(ui, seg.center().x - g.size().x / 2.0, cy, &g);
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
    for row in &d.rows {
        match row {
            DetailRow::Span { text, bold } => {
                let font = if *bold {
                    FontId::new(14.0, FontFamily::Name(pb_ui::SEMIBOLD.into()))
                } else {
                    FontId::new(14.0, FontFamily::Proportional)
                };
                ui.add(
                    egui::Label::new(RichText::new(text).font(font).color(p.text)).selectable(true),
                );
            }
            DetailRow::Pair { label, value } => {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui(egui::vec2(PAIR_LABEL_W, 0.0), |ui| {
                        ui.set_width(PAIR_LABEL_W);
                        ui.label(RichText::new(label).color(panel_secondary(p)));
                    });
                    ui.add(egui::Label::new(RichText::new(value).color(p.text)).selectable(true));
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
    match &d.body {
        DescribeBody::NoPhoto => {}
        DescribeBody::Idle => {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Press D to describe this image.").color(panel_secondary(p)),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("Ask").color(p.accent)).frame(false))
                        .clicked()
                    {
                        actions.push(PanelAction::Ask);
                    }
                });
            });
        }
        DescribeBody::Busy => {
            ui.label(RichText::new("Describing…").color(panel_secondary(p)));
        }
        DescribeBody::Ready(text) => {
            // TODO(egui-panels): block-level Markdown to match the SwiftUI MarkdownBlocksView;
            // wrapped selectable text for now.
            ui.add(egui::Label::new(RichText::new(text).color(p.text)).selectable(true));
        }
        DescribeBody::Error(text) => {
            ui.add(
                egui::Label::new(RichText::new(text).color(panel_secondary(p))).selectable(true),
            );
        }
    }
}

// ── Folder-tree panel ────────────────────────────────────────────────────────

fn tree_panel(ctx: &egui::Context, p: &Palette, tree: &TreeFrame, actions: &mut Vec<PanelAction>) {
    let max_h = panel_max_height(ctx, 200.0);
    egui::Window::new("pb_tree")
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .anchor(Align2::LEFT_TOP, [EDGE, EDGE])
        .max_height(max_h)
        .frame(panel_frame(p))
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
    // A dark, translucent capsule with a bright number in *both* themes (over a light panel
    // a low-alpha dark fill only reaches mid-gray and the number washes out — hence the
    // heavier light-mode alpha).
    let (bg, num_color) = if p.dark {
        (
            Color32::from_rgba_unmultiplied(0, 0, 0, 130),
            Color32::from_gray(228),
        )
    } else {
        // The composite is in linear light, where a translucent dark fill over a light panel
        // lands far lighter than its alpha suggests — so light mode needs a heavy alpha to
        // read as a genuinely dark capsule against the bright number.
        (
            Color32::from_rgba_unmultiplied(0, 0, 0, 230),
            Color32::from_gray(250),
        )
    };
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
