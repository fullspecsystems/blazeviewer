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
use pb_app_core::{fs_tree, Action, AppCore, InspectorTab};
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
const HELP_ROW_H: f32 = 21.0;
const HELP_KEYCAP_H: f32 = 18.0;
/// Keycap corner radius (shared by the Help rows and the welcome buttons' nesting math).
const KEYCAP_RADIUS: f32 = 5.0;
const HELP_SECTION_H: f32 = 22.0;
/// Top/bottom breathing margin for the (tall) Help panel — smaller than the corner panels'
/// `EDGE` so its long content fits at ordinary window heights (see `help_panel`).
const HELP_EDGE: f32 = 8.0;
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
/// Height of one tree row (each `up_row` / `tree_row` allocates exactly this). The scroll
/// region is sized from the row count without egui's (circular) auto-measure — using the
/// pitch `TREE_ROW_H + 1` (this height + the 1px inter-row gap) so a fitting tree never clips
/// its last row or shows a needless scrollbar.
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
    /// An **archive** tree row was clicked (a `.zip`/`.7z` folder deck — task #66). Resolved
    /// by index through `AppCore::tree_activate`: re-scope the deck to that internal folder,
    /// back out to the whole archive (the root row), or open the disk folder containing the
    /// archive (the `up` row). The `Tree{Toggle,Open,ExtendUp}` actions above are the disk
    /// `FsTree` equivalents; archives have no chevron/expand model.
    TreeActivate(usize),
    /// The scan pill's **Cancel** button — stop the in-flight folder scan (keeps what's
    /// streamed in so far). Applied by the shell as `cancel_scan_command`.
    CancelScan,
    /// The welcome screen's **Open File** / **Open Folder** buttons.
    OpenFile,
    OpenFolder,
    /// The play hint was clicked — play the motion item (same as the `P` key).
    PlayPause,
    /// The pointer moved onto (`true`) / off (`false`) the play hint — the shell pins its
    /// auto-fade open while hovered.
    PlayHintHover(bool),
    /// A windowed menu-bar item was clicked (the Linux egui bar — see [`menu_bar`]). The shell
    /// dispatches it through `App::dispatch_menu`, the same path the native muda bar uses.
    #[cfg(all(unix, not(target_os = "macos")))]
    Menu(crate::menu::MenuAction),
}

/// The Inspector's visible state for one frame.
pub struct InspectorFrame {
    pub tab: InspectorTab,
    pub snapshot: InspectorSnapshot,
}

/// The folder tree's visible state for one frame.
pub struct TreeFrame {
    /// Disk-deck rows (the Finder-style [`fs_tree::FsTree`]); empty for archive/empty decks.
    pub rows: Vec<fs_tree::Row>,
    /// The disk deck's "up to parent" label; `None` for archive/empty decks (whose exit
    /// affordance rides inside [`archive`](Self::archive) as an `up`-flagged row).
    pub parent_name: Option<String>,
    /// Archive/empty-deck rows, from the core's v1 `folder_tree_panel`. When non-empty these
    /// render **instead of** [`rows`](Self::rows) — flat and click-to-activate. This is the
    /// winit skin's port of the macOS FFI's `folder_tree_panel` fallback: the panel used to
    /// read only `fs_tree_rows()`, which is disk-only, so a `.zip`/`.7z` deck (whose tree the
    /// core derives into `folder_tree_panel`) showed an empty "Folders" panel (task #66).
    pub archive: Vec<ArchiveTreeRow>,
}

/// One flat archive-tree row — a folder inside a `.zip`/`.7z`, the archive root, a `…`
/// collapse marker, or the "up out of the archive" affordance. The winit skin's view of a
/// core `folder_tree_panel` row: no chevrons, because an archive folder **re-scopes** the
/// deck on click rather than expanding in place.
pub struct ArchiveTreeRow {
    /// Position in the core `folder_tree_panel` — the argument to `AppCore::tree_activate`.
    pub index: usize,
    pub depth: u32,
    pub name: String,
    /// The current folder ("you are here"): accent open-folder icon + bold name.
    pub current: bool,
    /// The "up out of the archive" row (an up-arrow; opens the folder containing the archive).
    pub up: bool,
    /// A dim, inert `…` collapse marker for an over-deep chain.
    pub marker: bool,
    /// Whether the row carries a click target (`false` for markers).
    pub clickable: bool,
    pub count: Option<u64>,
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

/// The first-run **welcome / empty-state** surface (no photos open): centered Open File /
/// Open Folder buttons (each with its keyboard shortcut) over a "or drag and drop here" hint
/// — the egui equivalent of the macOS SwiftUI `EmptyStateView`.
pub struct WelcomePanel {
    /// The keyboard shortcut hint for Open File (e.g. `O`), or empty if unbound.
    pub file_key: String,
    /// The keyboard shortcut hint for Open Folder (e.g. `⇧O` / `Shift+O`), or empty.
    pub folder_key: String,
}

/// The **play hint** (bottom-center, above the info line): a `[icon] Play [P]` button that
/// flashes when you settle on a motion item — the egui skin of the SwiftUI `PlayHintView`,
/// reusing the welcome button design. The shell owns the flash/fade timing (`alpha`).
#[derive(Clone)]
pub struct PlayHintFrame {
    /// `1` = Live Photo (livephoto glyph), `2` = animation (play ▶).
    pub kind: u8,
    /// The keyboard shortcut hint for play/pause (e.g. `P`).
    pub shortcut: String,
    /// The current fade opacity (0–1) — the shell animates the flash/hold/fade-out.
    pub alpha: f32,
}

/// The ambient folder-scan pill (top-center): a spinner, `Scanning <Name>` + `<N> found`,
/// the sub-folder currently being walked, and a **Cancel** button — the egui equivalent of
/// the macOS SwiftUI `ScanPillView`. Shown while a directory scan streams the deck in.
pub struct ScanPill {
    /// The scanned root's display name (the pill heading: `Scanning <name>`).
    pub name: String,
    /// Images found so far (the browsable deck length).
    pub found: usize,
    /// The sub-folder currently being walked (blank while it's just the root).
    pub current: String,
}

/// A pure snapshot of whichever rich panels are open — copied out of the core so the
/// egui build closure borrows none of it.
pub struct PanelFrame {
    pub help: Option<HelpPanel>,
    pub inspector: Option<InspectorFrame>,
    pub tree: Option<TreeFrame>,
    pub info: Option<InfoLine>,
    /// The folder-scan pill, when a directory scan is streaming in. Owned by the shell
    /// (scan state lives in `App::dir_scan`, not the core), so `snapshot` leaves it `None`
    /// and the shell fills it in `render_overlay_frame`.
    pub scan: Option<ScanPill>,
    /// The welcome / empty-state surface, when no photos are open.
    pub welcome: Option<WelcomePanel>,
    /// The play hint (motion items). Shell-owned (flash/fade timing lives in the shell), so
    /// `snapshot` leaves it `None` and the shell fills it in `render_overlay_frame`.
    pub play_hint: Option<PlayHintFrame>,
    pub dark: bool,
    /// The panel surface alpha (0–255), from Settings ▸ Appearance ▸ *Info panel opacity*
    /// (`info_opacity` — the old HUD opacity). The winit shell has no separate `panel_opacity`
    /// slider (that field is macOS-only), and the CPU HUD's winit panels used `info_opacity`
    /// too, so this restores that behavior *and* makes the existing slider actually move these
    /// panels. egui alpha is linear, so no material-style remap is needed.
    pub panel_alpha: u8,
    /// Top strip (logical px) reserved by an in-client menu bar that the top-anchored panels
    /// must clear — the Linux egui menu bar ([`menu_bar`]). Shell-owned (menu visibility lives
    /// in the shell), so `snapshot` leaves it `0.0` and the shell fills it in
    /// `render_overlay_frame`. `0.0` on Windows/macOS (their menu is OS chrome, not in-client).
    pub top_inset: f32,
}

impl PanelFrame {
    pub fn snapshot(core: &AppCore) -> Self {
        let help = core.help_panel_visible().then(|| core.help_panel());
        let inspector = core.inspector_panel_visible().then(|| InspectorFrame {
            tab: core.panels.inspector.unwrap_or(InspectorTab::Details),
            snapshot: core.inspector_snapshot(),
        });
        // The folder tree has two data sources, mirroring the macOS FFI (`tree_refresh`): a
        // disk deck uses the Finder-style `FsTree`; an archive/empty deck falls back to the
        // v1 `folder_tree_panel` the core still derives. The winit skin previously read only
        // `fs_tree_rows()` (disk-only), so archive decks showed an empty panel (task #66).
        let tree = core.tree_panel_visible().then(|| {
            if core.tree_is_fs() {
                TreeFrame {
                    rows: core.fs_tree_rows(),
                    parent_name: core.fs_tree_parent_name(),
                    archive: Vec::new(),
                }
            } else {
                TreeFrame {
                    rows: Vec::new(),
                    parent_name: None,
                    archive: core
                        .folder_tree_panel
                        .as_ref()
                        .map(archive_tree_rows)
                        .unwrap_or_default(),
                }
            }
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
        let welcome = core.open_panel_visible().then(|| WelcomePanel {
            file_key: core.shortcut_for(Action::OpenFile),
            folder_key: core.shortcut_for(Action::OpenFolder),
        });
        PanelFrame {
            help,
            inspector,
            tree,
            info,
            // Scan state is shell-owned; the shell sets this in `render_overlay_frame`.
            scan: None,
            welcome,
            // Shell-owned (fade timing); the shell sets this in `render_overlay_frame`.
            play_hint: None,
            dark: core.hud_dark,
            panel_alpha: opacity_to_alpha(core.settings.info_opacity),
            // Shell-owned (menu-bar visibility lives in the shell); set in render_overlay_frame.
            top_inset: 0.0,
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
    // The play hint rides the bottom-center, `EDGE` above the info line (or `EDGE` off the
    // bottom when the line is hidden), sharing the toast's spot (SwiftUI parity).
    if let Some(ph) = &frame.play_hint {
        let info_h = info_span.map_or(0.0, |(_, _, h)| h);
        play_hint_panel(ctx, &p, alpha, info_h, ph, actions);
    }
    // The tree (top-left) and Inspector (top-right) can coexist; Help (centered) is
    // topmost — draw it last so it sits above the others (SwiftUI z-order).
    let top = frame.top_inset;
    if let Some(tree) = &frame.tree {
        let r = duck(screen.left() + EDGE, screen.left() + EDGE + TREE_WIDTH);
        tree_panel(ctx, &p, alpha, top, r, tree, actions);
    }
    if let Some(insp) = &frame.inspector {
        let r = duck(
            screen.right() - EDGE - INSPECTOR_WIDTH,
            screen.right() - EDGE,
        );
        inspector_panel(ctx, &p, alpha, top, r, insp, actions);
    }
    // The scan pill rides the top-center, above the corner panels but below Help
    // (SwiftUI z-order) — drawn before Help so Help composites over it.
    if let Some(scan) = &frame.scan {
        scan_pill(ctx, &p, alpha, top, scan, actions);
    }
    // The welcome screen (empty deck). Help takes the center when up, so hide it then
    // (mirrors the SwiftUI `openPanelVisible && !helpVisible`).
    if let (Some(welcome), None) = (&frame.welcome, &frame.help) {
        welcome_panel(ctx, &p, welcome, actions);
    }
    if let Some(help) = &frame.help {
        let r = duck(
            screen.center().x - HELP_WIDTH / 2.0,
            screen.center().x + HELP_WIDTH / 2.0,
        );
        help_panel(ctx, &p, alpha, r, help, actions);
    }
}

/// Fixed **logical** height of the windowed menu bar. The shell reserves this many
/// (DPI-scaled) pixels at the top of the photo via the renderer's `content_top_inset`, so the
/// image fits + centers *below* the bar instead of under it. Kept in lockstep with the
/// `exact_height` in [`menu_bar`] — they must match or the photo gaps/overlaps the bar.
#[cfg(all(unix, not(target_os = "macos")))]
pub const MENU_BAR_H: f32 = 30.0;

/// Draw the windowed **menu bar** — the Linux egui stand-in for the native muda bar, which
/// can't attach to winit's non-GTK window (see [`crate::menu::menu_bar_spec`]). A full-width
/// top strip of drop-down menus; a click pushes [`PanelAction::Menu`], which the shell
/// dispatches through the same `App::dispatch_menu` the native bar uses. Windowed mode only —
/// the shell omits it in the chrome-free fullscreen speed mode. A fixed-height
/// [`egui::TopBottomPanel`] so its height is deterministic ([`MENU_BAR_H`]) and the photo
/// inset the shell reserves matches it exactly.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn menu_bar(
    ctx: &egui::Context,
    dark: bool,
    alpha: u8,
    groups: &[crate::menu::MenuGroup],
    nav: &mut crate::menu::MenuNav,
    actions: &mut Vec<PanelAction>,
) {
    let p = Palette::new(dark);
    let base = panel_surface(&p);
    // A menu bar wants to stay legible, so floor the surface alpha well above the panel setting.
    let bg = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha.max(238));
    let font = egui::FontId::proportional(MENU_FONT);
    // Left x of each title, captured while laying the bar out — the dropdown anchors here.
    let mut title_x: Vec<f32> = Vec::with_capacity(groups.len());
    egui::TopBottomPanel::top("pb_menu_bar")
        .exact_height(MENU_BAR_H)
        .show_separator_line(false)
        .frame(
            egui::Frame::none()
                .fill(bg)
                .inner_margin(egui::Margin::symmetric(6.0, 0.0)),
        )
        .show(ctx, |ui| {
            pb_ui::apply_to_ui(ui, dark);
            ui.horizontal_centered(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (i, group) in groups.iter().enumerate() {
                    let w = text_width(ui, group.title, &font) + 2.0 * MENU_TITLE_PAD;
                    let (rect, resp) = ui
                        .allocate_exact_size(egui::vec2(w, MENU_BAR_H - 6.0), egui::Sense::click());
                    title_x.push(rect.left());
                    if resp.clicked() {
                        // Click toggles; opening by mouse leaves no row preselected.
                        nav.open = (nav.open != Some(i)).then_some(i);
                        nav.sel = None;
                    } else if resp.hovered() && nav.open.is_some() && nav.open != Some(i) {
                        // Native menus glide: with any dropdown open, hovering another
                        // title switches to it.
                        nav.open = Some(i);
                        nav.sel = None;
                    }
                    let open = nav.open == Some(i);
                    if open {
                        ui.painter().rect_filled(rect, 5.0, p.accent);
                    } else if resp.hovered() {
                        ui.painter()
                            .rect_filled(rect, 5.0, p.text_secondary.gamma_multiply(0.18));
                    }
                    let col = if open { Color32::WHITE } else { p.text };
                    let g = galley(ui, group.title, font.clone(), col, f32::INFINITY);
                    let tx = rect.left() + MENU_TITLE_PAD;
                    paint_vtext(ui, tx, rect.center().y, &g);
                    // GTK-style: holding Alt reveals the mnemonic underline (Alt+F → File).
                    if nav.alt_hint {
                        if let Some(first) = group.title.get(0..1) {
                            let fw = text_width(ui, first, &font);
                            let y = (rect.center().y + g.size().y / 2.0).round() + 0.5;
                            ui.painter()
                                .hline(tx..=(tx + fw), y, egui::Stroke::new(1.0_f32, col));
                        }
                    }
                }
            });
        });
    // The open dropdown, anchored under its title. Drawn by us (not egui's menu state)
    // so the keyboard — Alt+mnemonics, arrows, Enter, Esc — can drive it (`MenuNav`).
    if let Some(gi) = nav.open {
        let (Some(group), Some(&x)) = (groups.get(gi), title_x.get(gi)) else {
            nav.open = None;
            return;
        };
        let mut fired: Option<crate::menu::MenuAction> = None;
        let dd_rect = menu_dropdown(ctx, dark, x, group, &mut nav.sel, &mut fired);
        if let Some(action) = fired {
            actions.push(PanelAction::Menu(action));
            nav.open = None;
            nav.sel = None;
        } else {
            // A click below the bar and outside the dropdown closes it (clicks on the
            // bar toggle/switch above; clicks the photo would get are swallowed by the
            // shell while a menu is open — native menus eat their closing click).
            let clicked_out = ctx.input(|inp| {
                inp.pointer.any_pressed()
                    && inp
                        .pointer
                        .interact_pos()
                        .is_some_and(|pos| pos.y > MENU_BAR_H && !dd_rect.contains(pos))
            });
            if clicked_out {
                nav.open = None;
                nav.sel = None;
            }
        }
    }
}

// Menu-row metrics (logical px). A real menu layout: a left check gutter, the label, a
// gap, then a right-aligned shortcut — every row the same width so the column lines up.
#[cfg(all(unix, not(target_os = "macos")))]
const MENU_ROW_H: f32 = 25.0;
#[cfg(all(unix, not(target_os = "macos")))]
const MENU_PAD_X: f32 = 10.0;
#[cfg(all(unix, not(target_os = "macos")))]
const MENU_GUTTER: f32 = 18.0; // check column (only reserved when the menu has checkables)
#[cfg(all(unix, not(target_os = "macos")))]
const MENU_GAP: f32 = 18.0; // min space between label and shortcut
#[cfg(all(unix, not(target_os = "macos")))]
const MENU_FONT: f32 = 14.0;
#[cfg(all(unix, not(target_os = "macos")))]
const MENU_TITLE_PAD: f32 = 10.0; // horizontal padding inside a bar title's hover pill
#[cfg(all(unix, not(target_os = "macos")))]
const MENU_SEP_H: f32 = 7.0; // separator row height (matches `menu_separator`)

/// Render one menu's rows into an open dropdown `ui`. Rows are drawn by hand (not egui
/// `Button`s, which carry the card fill + center the text) so the menu looks like a menu:
/// left-aligned labels over a shared check gutter, dimmed right-aligned shortcuts, an
/// accent hover bar, disabled items greyed. The dropdown is pre-sized to its widest row so
/// the shortcut column is flush.
#[cfg(all(unix, not(target_os = "macos")))]
fn menu_group(
    ui: &mut egui::Ui,
    p: &Palette,
    group: &crate::menu::MenuGroup,
    sel: &mut Option<usize>,
    fired: &mut Option<crate::menu::MenuAction>,
) {
    use crate::menu::MenuRow;
    let font = egui::FontId::proportional(MENU_FONT);
    let (gutter, want_w) = menu_layout(ui, group, &font);
    // Pin the width exactly (min == max) so the popup is content-tight, not stretched to
    // egui's default popup width — and so the unconstrained preview Area sizes the same.
    ui.set_min_width(want_w);
    ui.set_max_width(want_w);
    // The pointer only steals the selection while it's actually moving — a resting
    // cursor must not pin the highlight against the arrow keys (GTK behavior).
    let mouse_moving = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);

    for (idx, row) in group.items.iter().enumerate() {
        match row {
            MenuRow::Separator => menu_separator(ui, p),
            MenuRow::Item {
                action,
                label,
                shortcut,
                enabled,
                checked,
            } => {
                let (clicked, hovered) = menu_item(
                    ui,
                    p,
                    &font,
                    gutter,
                    label,
                    shortcut,
                    *enabled,
                    *checked,
                    *sel == Some(idx),
                );
                if hovered && mouse_moving {
                    *sel = Some(idx);
                }
                if clicked {
                    *fired = Some(*action);
                }
            }
        }
    }
}

/// Width of `s` laid out in `font`, for sizing the menu to its content.
#[cfg(all(unix, not(target_os = "macos")))]
fn text_width(ui: &egui::Ui, s: &str, font: &egui::FontId) -> f32 {
    ui.fonts(|f| {
        f.layout_no_wrap(s.to_owned(), font.clone(), Color32::WHITE)
            .size()
            .x
    })
}

/// `(check-gutter width, total menu width)` for `group`: the gutter is only reserved when
/// the menu has checkable items (check-free menus stay tight), and the width is the widest
/// row — `[pad][gutter][label]···gap···[shortcut][pad]`. Shared by the live menu and the
/// `--egui-shot` preview so they size identically.
#[cfg(all(unix, not(target_os = "macos")))]
fn menu_layout(ui: &egui::Ui, group: &crate::menu::MenuGroup, font: &egui::FontId) -> (f32, f32) {
    use crate::menu::MenuRow;
    let gutter = if group.items.iter().any(|r| {
        matches!(
            r,
            MenuRow::Item {
                checked: Some(_),
                ..
            }
        )
    }) {
        MENU_GUTTER
    } else {
        0.0
    };
    let mut want_w = 120.0f32;
    for row in &group.items {
        if let MenuRow::Item {
            label, shortcut, ..
        } = row
        {
            let base = MENU_PAD_X + gutter + text_width(ui, label, font);
            let w = if shortcut.is_empty() {
                base + MENU_PAD_X
            } else {
                base + MENU_GAP + text_width(ui, shortcut, font) + MENU_PAD_X
            };
            want_w = want_w.max(w);
        }
    }
    (gutter, want_w)
}

/// One drop-down row. Returns `(clicked, hovered)`. The highlight follows `selected` —
/// the one [`crate::menu::MenuNav::sel`] state both the pointer and the arrow keys move —
/// so mouse and keyboard can never show two competing highlights. Text is placed with
/// [`paint_vtext`] (the same `TEXT_LIFT` optical-centering the panels use) so the ink
/// sits on the row centerline — egui's raw `Align2::CENTER` reads high with these fonts.
#[cfg(all(unix, not(target_os = "macos")))]
#[allow(clippy::too_many_arguments)]
fn menu_item(
    ui: &mut egui::Ui,
    p: &Palette,
    font: &egui::FontId,
    gutter: f32,
    label: &str,
    shortcut: &str,
    enabled: bool,
    checked: Option<bool>,
    selected: bool,
) -> (bool, bool) {
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), MENU_ROW_H),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let hovered = enabled && resp.hovered();
    let lit = enabled && selected;
    // Accent selection bar (inset a hair from the popup edges so it reads as a pill).
    if lit {
        ui.painter()
            .rect_filled(rect.shrink2(egui::vec2(4.0, 1.0)), 5.0, p.accent);
    }
    let text_col = if !enabled {
        p.text_secondary.gamma_multiply(0.55)
    } else if lit {
        Color32::WHITE
    } else {
        p.text
    };
    let short_col = if lit {
        Color32::from_white_alpha(210)
    } else {
        p.text_secondary
    };
    let cy = rect.center().y;
    if checked == Some(true) {
        let g = galley(ui, "\u{2713}", font.clone(), text_col, f32::INFINITY); // ✓
        paint_vtext(
            ui,
            rect.left() + MENU_PAD_X + (gutter - g.size().x) / 2.0,
            cy,
            &g,
        );
    }
    let lg = galley(ui, label, font.clone(), text_col, f32::INFINITY);
    paint_vtext(ui, rect.left() + MENU_PAD_X + gutter, cy, &lg);
    if !shortcut.is_empty() {
        let sg = galley(ui, shortcut, font.clone(), short_col, f32::INFINITY);
        paint_vtext(ui, rect.right() - MENU_PAD_X - sg.size().x, cy, &sg);
    }
    (resp.clicked(), hovered)
}

/// A thin inset divider between menu sections.
#[cfg(all(unix, not(target_os = "macos")))]
fn menu_separator(ui: &mut egui::Ui, p: &Palette) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 7.0), egui::Sense::hover());
    let y = rect.center().y.round() + 0.5;
    ui.painter().hline(
        (rect.left() + 8.0)..=(rect.right() - 8.0),
        y,
        egui::Stroke::new(1.0_f32, p.text_secondary.gamma_multiply(0.35)),
    );
}

/// Render one menu's drop-down open at `x` in a popup-styled frame — the **live**
/// dropdown ([`menu_bar`] anchors it under the open title) and the `--egui-shot`
/// preview share this. `sel` is the selected row (pointer + arrow keys); a click sets
/// `fired`. Returns the popup rect (for the caller's click-outside-closes test).
#[cfg(all(unix, not(target_os = "macos")))]
fn menu_dropdown(
    ctx: &egui::Context,
    dark: bool,
    x: f32,
    group: &crate::menu::MenuGroup,
    sel: &mut Option<usize>,
    fired: &mut Option<crate::menu::MenuAction>,
) -> egui::Rect {
    use crate::menu::MenuRow;
    let p = Palette::new(dark);
    let surf = panel_surface(&p);
    let font = egui::FontId::proportional(MENU_FONT);
    let top = egui::pos2(x, MENU_BAR_H + 1.0);
    let mut rect = egui::Rect::from_min_size(top, egui::Vec2::ZERO);
    egui::Area::new(egui::Id::new("pb_menu_dropdown"))
        .order(egui::Order::Foreground)
        .fixed_pos(top)
        .show(ctx, |ui| {
            pb_ui::apply_to_ui(ui, dark);
            ui.set_opacity(1.0); // menus open instantly — no Area fade-in
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            // Size the box up front (a floating Area is otherwise unconstrained, stretching
            // rows full-width) so `set_min_size` establishes the rect + clip, then paint an
            // opaque rounded background before the rows.
            let (_g, w) = menu_layout(ui, group, &font);
            let n_item = group
                .items
                .iter()
                .filter(|r| matches!(r, MenuRow::Item { .. }))
                .count() as f32;
            let n_sep = group.items.len() as f32 - n_item;
            let h = n_item * MENU_ROW_H + n_sep * MENU_SEP_H + 8.0;
            ui.set_min_size(egui::vec2(w, h));
            rect = egui::Rect::from_min_size(top, egui::vec2(w, h));
            ui.painter().rect(
                rect,
                8.0,
                surf,
                egui::Stroke::new(1.0_f32, p.text_secondary.gamma_multiply(0.3)),
            );
            ui.add_space(4.0);
            menu_group(ui, &p, group, sel, fired);
        });
    rect
}

/// Render one menu's drop-down open at `x` — a **dev preview** for `--egui-shot` so the
/// menu can be eyeballed headlessly; not called on the live path (which goes through
/// [`menu_bar`] → [`menu_dropdown`] with real [`crate::menu::MenuNav`] state).
#[cfg(all(unix, not(target_os = "macos")))]
pub fn menu_dropdown_preview(
    ctx: &egui::Context,
    dark: bool,
    x: f32,
    group: &crate::menu::MenuGroup,
) {
    menu_dropdown(ctx, dark, x, group, &mut None, &mut None);
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

/// Snap a rect's origin and size to the physical pixel grid at `ppp`, so egui's rounded
/// corners and thin borders land on whole pixels instead of smearing across fractional ones
/// — noticeably crisper corners on HiDPI, where a fractionally-positioned pill otherwise
/// feathers its arcs unevenly.
fn snap_rect(rect: egui::Rect, ppp: f32) -> egui::Rect {
    let snap = |v: f32| (v * ppp).round() / ppp;
    egui::Rect::from_min_size(
        egui::pos2(snap(rect.min.x), snap(rect.min.y)),
        egui::vec2(snap(rect.width()), snap(rect.height())),
    )
}

/// The one-line info readout: `folder/name · W×H`, an optional Live-Photo / animation mark, and
/// an optional codec badge (a nested round-rect concentric with the pill). Bottom-corner per the
/// alignment setting, non-interactive, laid out by hand so everything shares one vertical center.
fn info_line(ctx: &egui::Context, p: &Palette, alpha: u8, info: &InfoLine) {
    use pb_app_core::settings::InfoLineAlign as A;
    let (w, pill_h) = info_pill_size(ctx, info);
    // Position by an explicit `fixed_pos` from the *known* pill width — NOT `Area::anchor`,
    // which positions off the previous frame's size (immediate-mode). Since the overlay is
    // retained (re-renders once per photo) and the width changes per photo, a width-dependent
    // anchor (center / right) would be placed using the *previous* photo's width and never
    // correct — the info line bounced between photos. Matches `build`'s `info_span` x0 exactly.
    let screen = ctx.screen_rect();
    let x0 = match info.align {
        A::Left => screen.left() + EDGE,
        A::Center => (screen.center().x - w / 2.0).max(screen.left()),
        A::Right => (screen.right() - EDGE - w).max(screen.left()),
    };
    let y0 = screen.bottom() - EDGE - pill_h;
    egui::Area::new(egui::Id::new("pb_info_line"))
        .fixed_pos(egui::pos2(x0, y0))
        // Authoritative fixed position. `constrain: true` (egui's default) re-clamps the area to
        // the screen using the *previous* frame's stored size — so a right-aligned pill (right edge
        // at the screen edge) whose line shrank vs the last photo got shoved left and bounced.
        // Center never triggered the clamp (far from any edge), which is why only right/edge
        // alignment was affected. We size the content to `w` ourselves, so no clamp is needed.
        .constrain(false)
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
            // Snap the pill to the physical pixel grid so its rounded corners + hairline
            // border land on whole pixels (crisper on HiDPI). All content below is placed
            // off this snapped rect.
            let ppp = ui.ctx().pixels_per_point();
            let rect = snap_rect(rect, ppp);

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
            // Crisp analytic corners (task #57 follow-up): draw the pill fill + hairline
            // border with an SDF shader instead of egui's tessellated rounded rect, which
            // feathers its arcs unevenly on HiDPI. The shadow above stays egui (its blur
            // hides any corner imperfection).
            crate::sdf_rect::round_rect(
                ui,
                rect,
                INFO_RADIUS,
                Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), alpha),
                1.0,
                separator(p),
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
                let badge = snap_rect(
                    egui::Rect::from_min_size(
                        egui::pos2(x, cy - badge_h / 2.0),
                        egui::vec2(badge_w, badge_h),
                    ),
                    ppp,
                );
                crate::sdf_rect::round_rect(
                    ui,
                    badge,
                    INFO_RADIUS - INFO_INSET,
                    badge_bg,
                    0.0,
                    Color32::TRANSPARENT,
                );
                // The codec text is all-caps (no descenders), so the shared `TEXT_LIFT`
                // box-centering reads high in the badge — center on the galley's ink bounds
                // instead, which is exact for any case.
                paint_text_centered(ui, badge.center(), &cg);
            }
        });
}

// ── Scan pill (folder-scan progress) ─────────────────────────────────────────

/// The pill corner radius (SwiftUI `panelBackground(cornerRadius: 14)`).
const SCAN_RADIUS: f32 = 14.0;
/// The pill's inner padding (SwiftUI `.padding(.horizontal, 16).padding(.vertical, 10)`).
const SCAN_PAD_H: f32 = 16.0;
const SCAN_PAD_V: f32 = 10.0;
/// Fixed width of the middle text column so the spinner + Cancel don't shift as the count /
/// sub-folder tick (SwiftUI `textWidth: 300`).
const SCAN_TEXT_W: f32 = 300.0;
/// Spinner square (SwiftUI `frame(width: 16, height: 16)`).
const SCAN_SPINNER: f32 = 16.0;
/// Gap between the pill's columns (SwiftUI `HStack(spacing: 12)`).
const SCAN_GAP: f32 = 12.0;
/// Gap between the two text rows (SwiftUI `VStack(spacing: 2)`).
const SCAN_LINE_GAP: f32 = 2.0;
/// Type sizes: heading + count at `.callout` (~13.5), the sub-folder at `.caption` (~11.5).
const SCAN_HEADING_SIZE: f32 = 13.5;
const SCAN_COUNT_SIZE: f32 = 13.0;
const SCAN_SUB_SIZE: f32 = 11.5;
const SCAN_CANCEL_SIZE: f32 = 13.0;

/// The ambient scan pill: a spinner, `Scanning <Name>` with the running count, the current
/// sub-folder, and a **Cancel** button — hand-laid so the spinner, text, divider, and button
/// share one vertical center (egui won't center a two-line column against a widget for us).
fn scan_pill(
    ctx: &egui::Context,
    p: &Palette,
    alpha: u8,
    top_inset: f32,
    pill: &ScanPill,
    actions: &mut Vec<PanelAction>,
) {
    let heading = format!("Scanning {}", pill.name);
    // Group the count with thousands separators (like the Inspector's "1,234 bytes") — a deep
    // recursive scan reaches six or seven digits, and "1232945 found" is hard to parse.
    let count = format!("{} found", pb_hud::hud::format_thousands(pill.found as u64));

    let head_font = FontId::new(SCAN_HEADING_SIZE, FontFamily::Name(pb_ui::SEMIBOLD.into()));
    let count_font = FontId::new(SCAN_COUNT_SIZE, FontFamily::Proportional);
    let sub_font = FontId::new(SCAN_SUB_SIZE, FontFamily::Proportional);
    let cancel_font = FontId::new(SCAN_CANCEL_SIZE, FontFamily::Proportional);

    // Measure row heights + the Cancel width off `ctx` (no `Ui`) so the pill can be sized
    // before the window body. Always reserve both text rows (blank sub-folder → a held
    // height) so the pill doesn't jump when the folder line appears/disappears.
    let measure_h = |size: f32| {
        ctx.fonts(|f| {
            f.layout_no_wrap(
                "Ag".to_owned(),
                FontId::new(size, FontFamily::Proportional),
                Color32::PLACEHOLDER,
            )
            .size()
            .y
        })
    };
    let line1_h = measure_h(SCAN_HEADING_SIZE);
    let line2_h = measure_h(SCAN_SUB_SIZE);
    let col_h = line1_h + SCAN_LINE_GAP + line2_h;
    let content_h = col_h.max(SCAN_SPINNER);
    let cancel_w = ctx.fonts(|f| {
        f.layout_no_wrap(
            "Cancel".to_owned(),
            cancel_font.clone(),
            Color32::PLACEHOLDER,
        )
        .size()
        .x
    });
    let content_w = SCAN_SPINNER + SCAN_GAP + SCAN_TEXT_W + SCAN_GAP + 1.0 + SCAN_GAP + cancel_w;

    sdf_panel(
        ctx,
        p,
        alpha,
        "pb_scan_pill",
        Align2::CENTER_TOP,
        egui::vec2(0.0, EDGE + top_inset),
        f32::INFINITY,
        SCAN_RADIUS,
        egui::Margin::symmetric(SCAN_PAD_H, SCAN_PAD_V),
        |ui| {
            ui.set_width(content_w);
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(content_w, content_h), egui::Sense::hover());
            let cy = rect.center().y;
            let mut x = rect.left();

            // Spinner — hand-drawn (like the panel's other geometric glyphs) and advanced by
            // frame time at a throttled ~30 fps, so it spins without `egui::Spinner`'s
            // per-frame repaint churning the overlay for a whole large scan.
            draw_spinner(
                ui,
                egui::pos2(x + SCAN_SPINNER / 2.0, cy),
                SCAN_SPINNER / 2.0,
                p,
            );
            x += SCAN_SPINNER + SCAN_GAP;

            // Text column: line 1 = heading (left, truncated) + count (right); line 2 = the
            // sub-folder being walked, both centered as a block on the pill's centerline.
            let col_l = x;
            let col_r = x + SCAN_TEXT_W;
            let col_top = cy - col_h / 2.0;
            let line1_cy = col_top + line1_h / 2.0;
            let line2_cy = col_top + line1_h + SCAN_LINE_GAP + line2_h / 2.0;
            let count_g = galley(ui, &count, count_font, panel_secondary(p), f32::INFINITY);
            let count_w = count_g.size().x;
            let head_max = (SCAN_TEXT_W - count_w - 8.0).max(24.0);
            let head_g = galley(ui, &heading, head_font, p.text, head_max);
            paint_vtext(ui, col_l, line1_cy, &head_g);
            paint_vtext(ui, col_r - count_w, line1_cy, &count_g);
            if !pill.current.is_empty() {
                let sub_g = galley(ui, &pill.current, sub_font, panel_secondary(p), SCAN_TEXT_W);
                paint_vtext(ui, col_l, line2_cy, &sub_g);
            }
            x = col_r + SCAN_GAP;

            // Divider rule — spans the full pill height, edge to edge. It reaches into the
            // pill's vertical padding, which `ui.painter()` would crop (that painter is clipped
            // to the content rect, inside the inner margin), so paint it on the unclipped layer
            // painter from the true pill top to bottom (content rect grown by the padding).
            let div = egui::Rect::from_min_max(
                egui::pos2(x, rect.top() - SCAN_PAD_V),
                egui::pos2(x + 1.0, rect.bottom() + SCAN_PAD_V),
            );
            ui.ctx()
                .layer_painter(ui.layer_id())
                .rect_filled(div, Rounding::ZERO, separator(p));
            x += 1.0 + SCAN_GAP;

            // Cancel button — accent text, a full-height hit target, pointer cursor on hover.
            let btn = egui::Rect::from_min_size(
                egui::pos2(x, rect.top()),
                egui::vec2(cancel_w, content_h),
            );
            let resp = ui.interact(btn, ui.id().with("scan_cancel"), egui::Sense::click());
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let color = if resp.hovered() {
                lighten(p.accent, 0.18)
            } else {
                p.accent
            };
            let cancel_g = galley(ui, "Cancel", cancel_font, color, f32::INFINITY);
            paint_vtext(ui, x, cy, &cancel_g);
            if resp.clicked() {
                actions.push(PanelAction::CancelScan);
            }
        },
    );
}

/// Blend `c` toward white by `t` (a subtle hover lift for the Cancel button).
fn lighten(c: Color32, t: f32) -> Color32 {
    let mix = |v: u8| (v as f32 + (255.0 - v as f32) * t).round() as u8;
    Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

/// Scale a straight (unmultiplied) color's alpha by `f` (clamped 0..=1) — used to fade
/// SDF-shader fills/borders, which `egui::Ui::set_opacity` can't touch (see `draw_open_button`).
fn mul_alpha(c: Color32, f: f32) -> Color32 {
    let a = (c.a() as f32 * f.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// A rotating arc spinner centered on `center`, radius `r`, advanced by egui's frame time —
/// a fading tail (full alpha at the head → transparent at the tail). Requests a throttled
/// ~30 fps repaint so a long scan doesn't re-render the overlay every frame.
fn draw_spinner(ui: &egui::Ui, center: egui::Pos2, r: f32, p: &Palette) {
    use std::f32::consts::TAU;
    let base = panel_secondary(p);
    let t = ui.input(|i| i.time) as f32;
    let head = (t * 3.2) % TAU; // angular speed
    let sweep = TAU * 0.72; // ~260° visible arc
    let segs = 20;
    let painter = ui.painter();
    for k in 0..segs {
        let f = k as f32 / segs as f32;
        let a0 = head - sweep * f;
        let a1 = head - sweep * (k + 1) as f32 / segs as f32;
        // Fade the tail so the leading end reads brightest (the spin direction).
        let alpha = ((1.0 - f) * base.a() as f32).round() as u8;
        let c = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), alpha);
        painter.line_segment(
            [
                egui::pos2(center.x + r * a0.cos(), center.y + r * a0.sin()),
                egui::pos2(center.x + r * a1.cos(), center.y + r * a1.sin()),
            ],
            Stroke::new(2.0_f32, c),
        );
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(33));
}

// ── Welcome / empty-state ─────────────────────────────────────────────────────

/// The open button's inner padding — the **right** inset (keycap → button edge) and the
/// keycap's top/bottom margin, so the keycap nests in the button's right end with equal margin
/// on three sides. The left inset is [`OPEN_PAD_LEFT`] (a touch more, so the icon isn't jammed
/// against the rounded corner).
const OPEN_PAD: f32 = 8.0;
/// The left inset (button edge → icon) — a few px more than [`OPEN_PAD`] so the icon has room
/// off the (larger, concentric) left corner.
const OPEN_PAD_LEFT: f32 = OPEN_PAD + 4.0;
/// The open button's leading icon size + the gap after it (SwiftUI `.padding(.leading, 8)`).
const OPEN_ICON: f32 = 16.0;
const OPEN_ICON_GAP: f32 = 8.0;
/// The minimum gap between the label and the trailing keycap (SwiftUI `Spacer(minLength: 14)`).
const OPEN_SPACER_MIN: f32 = 14.0;
/// Button height = the keycap height + `OPEN_PAD` above and below it, so the keycap sits with
/// equal margin on three sides (the nesting math).
const OPEN_BTN_H: f32 = HELP_KEYCAP_H + 2.0 * OPEN_PAD;
/// Button radius = the keycap radius + `OPEN_PAD`, so the button's right corners run
/// **concentric** with the keycap's — the keycap nests perfectly.
const OPEN_BTN_RADIUS: f32 = KEYCAP_RADIUS + OPEN_PAD;
/// The open button label type size.
const OPEN_LABEL_SIZE: f32 = 14.0;
/// The uniform vertical gap in the welcome stack — between the two buttons **and** between the
/// buttons and the hint, so the spacing reads consistent.
const OPEN_STACK_GAP: f32 = 18.0;
const OPEN_HINT_SIZE: f32 = 13.0;

/// The welcome / empty-state surface: two equal-width Open File / Open Folder buttons stacked
/// over a "or drag and drop here" hint, centered on screen — the egui skin of the SwiftUI
/// `EmptyStateView`. Transparent (no card): it sits directly on the empty canvas.
fn welcome_panel(
    ctx: &egui::Context,
    p: &Palette,
    welcome: &WelcomePanel,
    actions: &mut Vec<PanelAction>,
) {
    egui::Area::new(egui::Id::new("pb_welcome"))
        .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .order(egui::Order::Middle)
        .show(ctx, |ui| {
            // Both buttons take the wider one's natural content width, so they line up
            // (SwiftUI's `OpenButtonWidth` preference key).
            let w = open_button_width(ui, "Open File", &welcome.file_key).max(open_button_width(
                ui,
                "Open Folder",
                &welcome.folder_key,
            ));
            ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            if open_button(ui, p, Icon::File, "Open File", &welcome.file_key, w) {
                actions.push(PanelAction::OpenFile);
            }
            ui.add_space(OPEN_STACK_GAP);
            if open_button(ui, p, Icon::Folder, "Open Folder", &welcome.folder_key, w) {
                actions.push(PanelAction::OpenFolder);
            }
            ui.add_space(OPEN_STACK_GAP);
            // "or drag and drop here" — secondary, centered under the button column.
            let hint = galley(
                ui,
                "or drag and drop here",
                FontId::new(OPEN_HINT_SIZE, FontFamily::Proportional),
                panel_secondary(p),
                f32::INFINITY,
            );
            let (hrect, _) =
                ui.allocate_exact_size(egui::vec2(w, hint.size().y), egui::Sense::hover());
            paint_vtext(
                ui,
                hrect.center().x - hint.size().x / 2.0,
                hrect.center().y,
                &hint,
            );
        });
}

/// The natural content width of an open button (`[icon] label ···· [keycap]`) — the welcome
/// buttons size to the wider one, and the play hint sizes to its own.
fn open_button_width(ui: &egui::Ui, label: &str, shortcut: &str) -> f32 {
    let lw = galley(
        ui,
        label,
        FontId::new(OPEN_LABEL_SIZE, FontFamily::Proportional),
        Color32::PLACEHOLDER,
        f32::INFINITY,
    )
    .size()
    .x;
    OPEN_PAD_LEFT
        + OPEN_ICON
        + OPEN_ICON_GAP
        + lw
        + OPEN_SPACER_MIN
        + keycaps_width(ui, shortcut)
        + OPEN_PAD
}

/// Paint an open button (`[icon] label ···· [keycap]`) into `rect` with the given `fill` — the
/// shared body of the welcome buttons and the play hint. The nesting math (height, radius) is
/// baked into `rect` / [`OPEN_BTN_RADIUS`] by the caller.
#[allow(clippy::too_many_arguments)]
fn draw_open_button(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    p: &Palette,
    icon: Icon,
    label: &str,
    shortcut: &str,
    fill: Color32,
    fade: f32,
) {
    // The pill's fill + hairline border are drawn by an SDF shader (an egui paint *callback*),
    // which `ui.set_opacity` — how the play hint fades its text/icons — cannot reach. So bake the
    // fade factor into their alpha here; the text/icons below are ordinary egui shapes and fade
    // via `set_opacity`. `fade` is 1.0 for the always-solid welcome buttons.
    let fill = mul_alpha(fill, fade);
    let border = mul_alpha(separator(p), fade);
    crate::sdf_rect::round_rect(ui, rect, OPEN_BTN_RADIUS, fill, 1.0, border);
    let cy = rect.center().y;
    let ix = rect.left() + OPEN_PAD_LEFT;
    pb_ui::icon::paint_tinted(ui, sq(ix + OPEN_ICON / 2.0, cy, OPEN_ICON), icon, p.text);
    let lx = ix + OPEN_ICON + OPEN_ICON_GAP;
    let g = galley(
        ui,
        label,
        FontId::new(OPEN_LABEL_SIZE, FontFamily::Proportional),
        p.text,
        f32::INFINITY,
    );
    paint_vtext(ui, lx, cy, &g);
    draw_keycaps(ui, p, shortcut, rect.right() - OPEN_PAD, cy, fade);
}

/// One welcome open button at fixed `width`, brightening on hover. Returns whether it was
/// clicked.
fn open_button(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: Icon,
    label: &str,
    shortcut: &str,
    width: f32,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, OPEN_BTN_H), egui::Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let fill = if resp.hovered() {
        p.control_hover
    } else {
        p.control
    };
    draw_open_button(ui, rect, p, icon, label, shortcut, fill, 1.0);
    resp.clicked()
}

/// The play hint: a `[icon] Play [P]` button (the welcome button design, translucent so it
/// reads over a photo), bottom-center **above the info line**, at the shell-computed fade
/// `alpha`. Reports hover (to pin the fade) and a click (to play). `info_h` is the info line's
/// pill height (0 when it's hidden) — the hint's bottom sits `EDGE` above the line's top.
fn play_hint_panel(
    ctx: &egui::Context,
    p: &Palette,
    base_alpha: u8,
    info_h: f32,
    frame: &PlayHintFrame,
    actions: &mut Vec<PanelAction>,
) {
    // Bottom offset: EDGE above the info line's top (= 2·EDGE + info height), or EDGE when the
    // line is hidden — so the hint→line and line→bottom gaps both equal EDGE (SwiftUI parity).
    let bottom = if info_h > 0.0 {
        2.0 * EDGE + info_h
    } else {
        EDGE
    };
    let icon = if frame.kind == 1 {
        Icon::LivePhoto
    } else {
        Icon::Play
    };
    egui::Area::new(egui::Id::new("pb_play_hint"))
        .anchor(Align2::CENTER_BOTTOM, egui::vec2(0.0, -bottom))
        .order(egui::Order::Middle)
        .show(ctx, |ui| {
            ui.set_opacity(frame.alpha);
            let w = open_button_width(ui, "Play", &frame.shortcut);
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(w, OPEN_BTN_H), egui::Sense::click());
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            // Translucent so it sits over the photo like the info line; brighten on hover.
            let base = if resp.hovered() {
                p.control_hover
            } else {
                p.control
            };
            let fill = Color32::from_rgba_unmultiplied(base.r(), base.g(), base.b(), base_alpha);
            // The pill's SDF fill/border fade via the explicit `frame.alpha` (set_opacity, applied
            // above for the text/icons, can't reach the shader callback).
            draw_open_button(
                ui,
                rect,
                p,
                icon,
                "Play",
                &frame.shortcut,
                fill,
                frame.alpha,
            );
            actions.push(PanelAction::PlayHintHover(resp.hovered()));
            if resp.clicked() {
                actions.push(PanelAction::PlayPause);
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

/// Show a chrome-free panel `Window` whose background is a crisp **SDF** rounded rect (fill +
/// hairline border + shadow) instead of egui's tessellated `Frame` — the same look as
/// [`panel_frame`] but with analytic, resolution-independent corners. Two shape slots are
/// reserved at the top of the window's layer and backfilled (shadow, then fill + border) once
/// the window rect is known, so they land *behind* the content. The panels ignore the
/// response, so this returns nothing.
#[allow(clippy::too_many_arguments)]
fn sdf_panel(
    ctx: &egui::Context,
    p: &Palette,
    alpha: u8,
    id: &str,
    anchor: Align2,
    offset: egui::Vec2,
    max_h: f32,
    radius: f32,
    margin: egui::Margin,
    content: impl FnOnce(&mut egui::Ui),
) {
    let shown = egui::Window::new(id)
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .movable(false)
        .anchor(anchor, offset)
        .max_height(max_h)
        .frame(egui::Frame::none().inner_margin(margin))
        .show(ctx, |ui| {
            let shadow = ui.painter().add(egui::Shape::Noop);
            let bg = ui.painter().add(egui::Shape::Noop);
            content(ui);
            // Capture the *actual* content bounds this frame. egui's auto-sized Window caches
            // its rect and updates it a frame late, so when a panel's content grows every frame
            // — the folder tree while a scan streams rows in — `response.rect` lags behind and a
            // background sized to it ends above the last rows (they bleed past the panel). The
            // live content rect doesn't lag, so we union the two below.
            (shadow, bg, ui.min_rect())
        });
    let Some(shown) = shown else {
        return;
    };
    let Some((shadow_idx, bg_idx, content_rect)) = shown.inner else {
        return;
    };
    // Enclose whichever is larger: the window's (possibly stale) frame or the real content.
    let rect = shown.response.rect.union(content_rect);
    let painter = ctx.layer_painter(shown.response.layer_id);
    painter.set(
        shadow_idx,
        egui::epaint::Shadow {
            offset: egui::vec2(0.0, 5.0),
            blur: 18.0,
            spread: 0.0,
            color: Color32::from_black_alpha(70),
        }
        .as_shape(rect, Rounding::same(radius)),
    );
    let fill = panel_surface(p);
    painter.set(
        bg_idx,
        crate::sdf_rect::round_rect_shape(
            rect,
            radius,
            Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), alpha),
            1.0,
            separator(p),
        ),
    );
}

/// The opaque stand-in for `.secondary` used on icons / labels / ✕ so they stay legible
/// over the translucent material (SwiftUI `Color.panelSecondary`). The dark value is lifted
/// (was 163 / white 0.64) so secondary text keeps enough contrast over a bright photo behind
/// the translucent panels — the scan pill's count / sub-folder was the worst case.
fn panel_secondary(p: &Palette) -> Color32 {
    if p.dark {
        Color32::from_gray(190) // white 0.745
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

/// Paint galley `g` with its **ink** (mesh) bounds centered on `c`, both axes. Unlike the
/// box-height `TEXT_LIFT` centering, this is exact for all-caps text (no descenders), where
/// the galley box carries empty descender space that reads as a downward bias — used for the
/// codec badge so its `JPEG` / `BMP` sits dead-center.
fn paint_text_centered(ui: &egui::Ui, c: egui::Pos2, g: &std::sync::Arc<egui::Galley>) {
    let pos = c - g.mesh_bounds.center().to_vec2();
    ui.painter().galley(pos, g.clone(), Color32::PLACEHOLDER);
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
    let s = Stroke::new(1.6_f32, color);
    painter.line_segment([r.left_top(), r.right_bottom()], s);
    painter.line_segment([r.right_top(), r.left_bottom()], s);
}

/// Draw a copy (two-documents) glyph into `rect`; `bg` fills the front sheet so the overlap
/// reads.
fn draw_copy(painter: &egui::Painter, rect: egui::Rect, bg: Color32, color: Color32) {
    let s = Stroke::new(1.3_f32, color);
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
    let s = Stroke::new(1.6_f32, color);
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
fn panel_max_height(ctx: &egui::Context, floor: f32, top_inset: f32, duck: f32) -> f32 {
    (ctx.screen_rect().height() - 2.0 * EDGE - top_inset - duck).max(floor)
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
    // egui's ScrollArea clips its content to the viewport *expanded by*
    // `Visuals::clip_rect_margin` (3px default) — meant to keep edge shadows/focus
    // rings from being cut inside a padded container. These panel bodies run flush
    // against the panel background's edge, so that margin let every scrolled-out row
    // paint a 3px sliver *outside* the panel — over the header above and the photo
    // below (the tree/Inspector "bleed", task #54). Clip exactly at the viewport.
    ui.visuals_mut().clip_rect_margin = 0.0;
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
    // Help is centered (not top-anchored), so it doesn't move for the menu bar. It's the
    // tallest panel (the long "Files & App" section), so give it a smaller top/bottom
    // breathing margin than the corner panels' `EDGE` — otherwise 2·EDGE (48px) plus the
    // menu-bar strip leaves too little for the content and it clips at ordinary window
    // heights. `HELP_EDGE` still keeps it off the very edges; `scroll_body` handles any
    // remaining overflow on a genuinely short window.
    let max_h = (ctx.screen_rect().height() - 2.0 * HELP_EDGE - duck).max(220.0);
    sdf_panel(
        ctx,
        p,
        alpha,
        "pb_help",
        Align2::CENTER_CENTER,
        egui::vec2(0.0, 0.0),
        max_h,
        PANEL_RADIUS,
        egui::Margin::ZERO,
        |ui| {
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
                    .inner_margin(egui::Margin::symmetric(18.0, 10.0))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 10.0;
                        for section in &help.sections {
                            help_section(ui, p, section);
                        }
                    });
            });
        },
    );
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
        crate::sdf_rect::round_rect(ui, rect, 6.0, quaternary(p), 0.0, Color32::TRANSPARENT);
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
    let keys_w = draw_keycaps(ui, p, shortcut, rect.right(), cy, 1.0);
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

/// Tokenize a shortcut string: `Some(cap)` = one keycap's text, `None` = a "/" separator
/// between alternatives. A **chord** (an alternative carrying a modifier — the mac ⇧⌘⌥⌃
/// glyphs or a Windows "Shift+…") stays ONE keycap so the modifier stays glued to its key
/// (⇧R, not ⇧ · R); a modifier-less alternative (the Pan arrows) splits into one cap per key.
fn keycap_tokens(shortcut: &str) -> Vec<Option<String>> {
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
    tokens
}

/// The pixel width [`draw_keycaps`] would use for `shortcut` (0 if empty), without drawing —
/// so the welcome buttons can be sized to equal width before layout.
fn keycaps_width(ui: &egui::Ui, shortcut: &str) -> f32 {
    if shortcut.trim().is_empty() {
        return 0.0;
    }
    let tokens = keycap_tokens(shortcut);
    let key_font = FontId::new(HELP_KEY_SIZE, FontFamily::Proportional);
    let text_w = |s: &str| {
        galley(ui, s, key_font.clone(), Color32::PLACEHOLDER, f32::INFINITY)
            .size()
            .x
    };
    let sum: f32 = tokens
        .iter()
        .map(|t| match t {
            Some(cap) => text_w(cap) + 14.0,
            None => text_w("/") + 2.0,
        })
        .sum();
    sum + 5.0 * tokens.len().saturating_sub(1) as f32
}

/// Draw a shortcut's keycaps (and "/" separators) **right-aligned** ending at `right_x`,
/// v-centered on `cy`. Returns the total width used. A shortcut is groups split on " / ",
/// each group's keys split on whitespace.
fn draw_keycaps(
    ui: &mut egui::Ui,
    p: &Palette,
    shortcut: &str,
    right_x: f32,
    cy: f32,
    fade: f32,
) -> f32 {
    if shortcut.trim().is_empty() {
        return 0.0;
    }
    let tokens = keycap_tokens(shortcut);
    let gap = 5.0;
    let key_font = FontId::new(HELP_KEY_SIZE, FontFamily::Proportional);
    let (bg, key_col) = badge_colors(p);
    // The keycap's fill + border are SDF-shader shapes, so — like the pill itself — they don't
    // fade through `set_opacity`; bake `fade` in (1.0 for the static help panel).
    let bg = mul_alpha(bg, fade);
    let border = mul_alpha(separator(p), fade);
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
                crate::sdf_rect::round_rect(ui, rect, KEYCAP_RADIUS, bg, 1.0, border);
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
    top_inset: f32,
    duck: f32,
    insp: &InspectorFrame,
    actions: &mut Vec<PanelAction>,
) {
    let max_h = panel_max_height(ctx, 200.0, top_inset, duck);
    sdf_panel(
        ctx,
        p,
        alpha,
        "pb_inspector",
        Align2::RIGHT_TOP,
        egui::vec2(-EDGE, EDGE + top_inset),
        max_h,
        PANEL_RADIUS,
        egui::Margin::ZERO,
        |ui| {
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
        },
    );
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
    crate::sdf_rect::round_rect(ui, track, 7.0, quaternary(p), 0.0, Color32::TRANSPARENT);
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
            crate::sdf_rect::round_rect(ui, seg, 5.0, p.accent, 0.0, Color32::TRANSPARENT);
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
    top_inset: f32,
    duck: f32,
    tree: &TreeFrame,
    actions: &mut Vec<PanelAction>,
) {
    let max_h = panel_max_height(ctx, 200.0, top_inset, duck);
    sdf_panel(
        ctx,
        p,
        alpha,
        "pb_tree",
        Align2::LEFT_TOP,
        egui::vec2(EDGE, EDGE + top_inset),
        max_h,
        PANEL_RADIUS,
        egui::Margin::ZERO,
        |ui| {
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
            // The folder list scrolls once it overflows. Use the shared `scroll_body` helper —
            // the SAME definite-height ScrollArea (`max_height` + `min_scrolled_height`, sized
            // from the remembered content height) the Inspector and Help panels use. This is
            // load-bearing: a bare `ScrollArea` inside this auto-sizing Window — or one merely
            // wrapped in `allocate_ui` — never gets a definite height, so it lays the rows out
            // at *full* content height and never enters scroll/clip mode. The rows then render
            // past the panel and bleed over the photo (top into the header, bottom over the
            // image) even though the layout geometry and background are correct — the clip is
            // what's missing, and `max_height` is what turns it on. The panel background (sized
            // to the real content bounds in `sdf_panel`) then encloses exactly this body.
            let body_top = HEADER_H + 1.0; // header + groove
            scroll_body(ui, "pb_tree", max_h - body_top, |ui| {
                egui::Frame::none()
                    .inner_margin(egui::Margin::symmetric(0.0, 6.0))
                    .show(ui, |ui| {
                        // Dense list: drop pb_ui's 32px control-height minimum so rows
                        // hug their content instead of each nested layout reserving 32px.
                        ui.spacing_mut().interact_size.y = 0.0;
                        ui.spacing_mut().item_spacing.y = 1.0;
                        if tree.archive.is_empty() {
                            // Disk deck: the Finder-style FsTree (chevron expand/collapse).
                            if let Some(parent) = &tree.parent_name {
                                up_row(ui, p, parent, actions);
                            }
                            for row in &tree.rows {
                                tree_row(ui, p, row, actions);
                            }
                        } else {
                            // Archive/empty deck: flat, click-to-activate rows (task #66).
                            for row in &tree.archive {
                                archive_tree_row(ui, p, row, actions);
                            }
                        }
                    });
            });
        },
    );
}

/// Map the core's v1 `folder_tree_panel` (an archive/empty deck) into flat [`ArchiveTreeRow`]s
/// — the winit port of the macOS FFI's archive branch (`tree_refresh`). A row is clickable
/// exactly when it carries a target (a `…` marker doesn't); its `index` is what a click hands
/// `AppCore::tree_activate`. Pure, so the mapping is unit-tested without a core or egui.
fn archive_tree_rows(panel: &pb_app_core::overlay::TreePanel) -> Vec<ArchiveTreeRow> {
    panel
        .rows
        .iter()
        .enumerate()
        .map(|(index, r)| ArchiveTreeRow {
            index,
            depth: r.depth,
            name: r.name.clone(),
            current: r.current,
            up: r.up,
            marker: r.marker,
            clickable: panel.targets.get(index).is_some_and(Option::is_some),
            count: r.count,
        })
        .collect()
}

/// One **archive** tree row — flat, no chevron: a folder re-scopes the deck on click, the
/// root row backs out to the whole archive, the `up` row exits to the containing disk folder,
/// and a `…` marker is inert. Activation resolves by index through `AppCore::tree_activate`,
/// so this mirrors the macOS FFI's flat archive list (task #66). The geometry matches
/// [`tree_row`] minus the chevron column, so names line up whichever deck is open.
fn archive_tree_row(
    ui: &mut egui::Ui,
    p: &Palette,
    row: &ArchiveTreeRow,
    actions: &mut Vec<PanelAction>,
) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), TREE_ROW_H),
        egui::Sense::hover(),
    );
    let cy = rect.center().y;
    let indent = row.depth as f32 * TREE_INDENT + TREE_BASE_INDENT;
    let icon_x = rect.left() + indent;

    if row.marker {
        // A dim, inert "…" collapse marker (an over-deep chain folded its middle levels).
        let g = galley(
            ui,
            &row.name,
            FontId::new(TREE_NAME_SIZE, FontFamily::Proportional),
            panel_secondary(p),
            (rect.right() - 10.0 - icon_x).max(10.0),
        );
        paint_vtext(ui, icon_x, cy, &g);
        return;
    }

    // Lead glyph: an up-arrow for the exit row, else a folder icon (open + accent = current).
    let icon_center = sq(icon_x + TREE_ICON_SIZE / 2.0, cy, TREE_ICON_SIZE);
    if row.up {
        draw_up_arrow(ui.painter(), icon_center, panel_secondary(p));
    } else {
        let icon = if row.current {
            Icon::FolderOpen
        } else {
            Icon::Folder
        };
        let tone = if row.current {
            Tone::Accent
        } else {
            Tone::Neutral
        };
        pb_ui::icon::paint(ui, icon_center, icon, tone, p);
    }

    // Count pill on the right; the name truncates into what's left.
    let mut name_right = rect.right() - 10.0;
    if let Some(count) = row.count {
        name_right -= tree_pill(ui, p, count, name_right, cy) + 6.0;
    }
    let name_x = icon_x + TREE_ICON_SIZE + 6.0;
    let font = if row.current {
        FontId::new(TREE_NAME_SIZE, FontFamily::Name(pb_ui::SEMIBOLD.into()))
    } else {
        FontId::new(TREE_NAME_SIZE, FontFamily::Proportional)
    };
    let g = galley(ui, &row.name, font, p.text, (name_right - name_x).max(10.0));
    paint_vtext(ui, name_x, cy, &g);

    // Clicking a targeted row re-scopes / opens by index. The current folder keeps a
    // (self) target so it stays clickable — harmless, and matches the macOS host.
    if row.clickable {
        let hit = egui::Rect::from_min_max(
            egui::pos2(icon_x, rect.top()),
            egui::pos2(name_right, rect.bottom()),
        );
        let resp = ui.interact(
            hit,
            ui.id().with(("arc_tree", row.index)),
            egui::Sense::click(),
        );
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            actions.push(PanelAction::TreeActivate(row.index));
        }
    }
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
    crate::sdf_rect::round_rect(ui, pill, pill_h / 2.0, bg, 0.0, Color32::TRANSPARENT);
    paint_vtext(ui, pill.center().x - g.size().x / 2.0, cy, &g);
    pill_w
}

#[cfg(test)]
mod tests {
    use super::*;
    use pb_app_core::settings::InfoLineAlign;

    fn info(main: &str, align: InfoLineAlign) -> InfoLine {
        InfoLine {
            main: main.to_owned(),
            codec: String::new(),
            is_live: false,
            is_animated: false,
            align,
        }
    }

    /// Lay out just the info line in one egui frame at a fixed screen size.
    fn run_info(ctx: &egui::Context, screen: egui::Rect, line: &InfoLine) {
        let p = Palette::new(true);
        let raw = egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| info_line(ctx, &p, 255, line));
    }

    fn info_rect(ctx: &egui::Context) -> egui::Rect {
        egui::AreaState::load(ctx, egui::Id::new("pb_info_line"))
            .expect("info-line area laid out")
            .rect()
    }

    /// Regression: a right-aligned info line must pin to the right edge on every photo,
    /// independent of the *previous* photo's width. egui's default `Area` re-clamps the area to
    /// the screen using the previous frame's stored size, so before `constrain(false)` a photo
    /// whose line was narrower than the last got shoved left — the "bounce". (Center was immune
    /// since it never sits near an edge, which is the asymmetry the fix targets.) The overlay is
    /// retained, so a photo change is a *single* frame — reproduced here as one narrow frame after
    /// settling on a wide one. Without the fix the narrow line's right edge lands ~377px inboard.
    #[test]
    fn right_aligned_info_line_pins_to_edge_regardless_of_previous_width() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(2000.0, 1000.0));
        let want_right = screen.right() - EDGE;

        // Settle on a wide line (egui's first frame for an area is a sizing pass).
        let wide = info(
            "2001-a-very-long-folder/a-long-name.heic \u{b7} 8192 \u{d7} 6144 extra width",
            InfoLineAlign::Right,
        );
        run_info(&ctx, screen, &wide);
        run_info(&ctx, screen, &wide);
        assert!(
            (info_rect(&ctx).right() - want_right).abs() < 2.0,
            "wide line should already sit flush at the right edge"
        );

        // One narrow frame — exactly what a photo change does. Its right edge must stay flush,
        // not shift left by the wide->narrow width delta.
        run_info(&ctx, screen, &info("x", InfoLineAlign::Right));
        let got = info_rect(&ctx).right();
        assert!(
            (got - want_right).abs() < 2.0,
            "narrow line bounced: right edge {got}, expected {want_right}"
        );
    }
}
