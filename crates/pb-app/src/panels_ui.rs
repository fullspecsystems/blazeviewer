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
use pb_app_core::{fs_tree, Action, AppCore, InspectorTab, LeftTab};
use pb_ui::icon::{Icon, Tone};
use pb_ui::Palette;
use std::collections::HashMap;
use std::path::PathBuf;

/// The inset of a panel from the window edge (SwiftUI `Layout.edge`).
const EDGE: f32 = 24.0;
/// The shared panel corner radius (SwiftUI `panelBackground(cornerRadius: 12)`).
const PANEL_RADIUS: f32 = 12.0;
/// Help panel fixed width (SwiftUI `width: 520`).
const HELP_WIDTH: f32 = 520.0;
/// Inspector default width (SwiftUI `inspectorWidth` default 360). Now user-resizable via the
/// shared drag handle on its **left** edge (task #83); the live width rides on
/// [`PanelFrame::inspector_width`] and this is only the startup default.
const INSPECTOR_WIDTH: f32 = 360.0;
/// Inspector resize bounds. Higher floor than the left pane — the Details table + tab bar want
/// the room, and the Inspector never needs to get as narrow (owner call).
const INSPECTOR_WIDTH_MIN: f32 = 300.0;
const INSPECTOR_WIDTH_MAX: f32 = 560.0;
/// Left-pane (Folders / Thumbnails) default width. Now user-resizable via the shared drag
/// handle (task #83) — the live width rides on [`PanelFrame::pane_width`]; this is only the
/// startup default. One knob for both tabs (the Inspector idiom).
const TREE_WIDTH: f32 = 280.0;
/// Left-pane resize bounds (owner-tuned). Below the floor the tab bar has no room to stay
/// legible; the ceiling keeps the pane from eating the photo.
const PANE_WIDTH_MIN: f32 = 212.0;
const PANE_WIDTH_MAX: f32 = 620.0;
/// Left-pane tab-bar mode thresholds (task #83). As the pane narrows the tab pills shed content
/// in two steps for a clean fit: icon **+ label** when wide, **label-only** (icons dropped) in
/// the middle band, **icon-only** (labels dropped) near the floor.
const TAB_ICON_LABEL_MIN: f32 = 264.0;
const TAB_LABEL_ONLY_MIN: f32 = 232.0;
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
    /// A left-pane tab-bar click (Folders | Thumbnails, task #83). Only pushed when the
    /// clicked tab isn't already showing, so applying it never toggle-closes the pane —
    /// it rides the same `⇧F`/`⇧T` actions the keyboard does (macOS `showLeftTab` parity).
    SelectLeftTab(LeftTab),
    /// The Thumbnails panel's ✕ — close the left pane (it's on the Thumbnails tab).
    CloseThumbs,
    /// The left pane's resize handle was dragged — the shell stores the new width (shared by
    /// both tabs) and re-renders at it next frame.
    SetPaneWidth(f32),
    /// The Inspector's resize handle was dragged — the shell stores the new width.
    SetInspectorWidth(f32),
    /// The left pane's resize-handle strip rect (egui **points**), reported every render so the
    /// shell can own the resize cursor geometrically (lag-free crossing in from the photo —
    /// egui's per-frame hover cursor was the source of the flicker).
    PaneResizeZone(egui::Rect),
    /// The Inspector's resize-handle strip rect (egui **points**), same purpose as
    /// [`PaneResizeZone`](Self::PaneResizeZone) for the right-anchored panel.
    InspectorResizeZone(egui::Rect),
    /// A thumbnail cell click: absolute jump + instant thumb-preview present (`thumb_jump`).
    ThumbClick(usize),
    /// The strip's visible + overscan inclusive index ranges — the demand window that fills
    /// and eviction protect (reported only when the range changes).
    ThumbViewport {
        visible: (usize, usize),
        overscan: (usize, usize),
    },
    /// The user grabbed the strip (not our own follow animation): detach auto-follow.
    ThumbUserScrolled,
    /// A programmatic follow-scroll landed — hand the generation back so FollowState knows
    /// its animation ended (and stale generations are ignored).
    ThumbScrollDone(u64),
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
    /// The info line's playback bar was clicked/dragged (task #79 follow-up):
    /// seek the active video to this fraction of its duration.
    SeekVideo(f32),
    /// The playback row's play/pause button — toggle the video (same as `P`).
    VideoPlayPause,
    /// The pointer moved onto (`true`) / off (`false`) the play hint — the shell pins its
    /// auto-fade open while hovered.
    PlayHintHover(bool),
    /// A windowed menu-bar item was clicked (the Linux egui bar — see [`menu_bar`]). The shell
    /// dispatches it through `App::dispatch_menu`, the same path the native muda bar uses.
    #[cfg(all(unix, not(target_os = "macos")))]
    Menu(crate::menu::MenuAction),
    /// A **toolbar** button (task #61) fired a one-shot action — the shell dispatches it through
    /// `AppCore::dispatch_action`, the same path a keypress uses.
    ToolbarAction(pb_app_core::Action),
    /// A toolbar nav/random button was **pressed** — begin pointer hold-to-blaze for the action
    /// (`AppCore::begin_pointer_nav`): an initial advance now, then the self-paced blaze while held.
    ToolbarNavPress(pb_app_core::Action),
    /// The held toolbar nav/random button was **released** (or the pointer left it) — stop blazing
    /// (`AppCore::end_pointer_nav`).
    ToolbarNavRelease,
}

/// The Inspector's visible state for one frame.
#[derive(Clone)]
pub struct InspectorFrame {
    pub tab: InspectorTab,
    pub snapshot: InspectorSnapshot,
}

/// The folder tree's visible state for one frame.
#[derive(Clone)]
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
    /// Whether the left pane has the Folders | Thumbnails tab bar (task #83) — true when a
    /// thumbnail strip presenter exists on this shell (`AppCore::native_thumbs`). When false
    /// the header is the plain "Folders" title.
    pub tabs: bool,
}

/// One flat archive-tree row — a folder inside a `.zip`/`.7z`, the archive root, a `…`
/// collapse marker, or the "up out of the archive" affordance. The winit skin's view of a
/// core `folder_tree_panel` row: no chevrons, because an archive folder **re-scopes** the
/// deck on click rather than expanding in place.
#[derive(Clone)]
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
/// animation mark — placed in a bottom corner per the alignment setting. While a video
/// session is live it grows a second row (`progress`): elapsed left, total right, a
/// progress bar filling the span between (task #79, owner design).
#[derive(Clone)]
pub struct InfoLine {
    pub main: String,
    pub codec: String,
    pub is_live: bool,
    pub is_animated: bool,
    pub align: pb_app_core::settings::InfoLineAlign,
    pub progress: Option<InfoProgress>,
    /// Fade factor (0..=1): the shell ramps it over ~100 ms on appearance and
    /// disappearance (it keeps the last line rendering briefly for the out leg).
    pub fade: f32,
}

/// The playback row's data: `0:42 ▰▰▰▱▱▱ 9:01`. The pill's width is the summary
/// row's natural width, floored so the bar stays usable — constant for a clip, so
/// the once-a-second refresh never jitters.
#[derive(Clone)]
pub struct InfoProgress {
    pub elapsed: String,
    /// `None` when the container reports no duration (bare track, no right label).
    pub total: Option<String>,
    /// 0..=1 of the bar filled.
    pub fraction: f32,
    /// Whether the video is playing right now (session Playing). Picks the
    /// play/pause button's glyph, and drives a timed egui repaint so the knob
    /// glides instead of stepping once a second (the retained overlay otherwise
    /// re-renders only on the second tick-over); paused/ended stays retained.
    pub playing: bool,
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
    /// `1` = Live Photo (livephoto glyph), `2` = animation (play ▶). An archive door has
    /// no pill: its whole affordance is the door card (task #105).
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
    /// The archive **door card** (task #105), when a door is presented. A door's frame is a
    /// 1×1 transparent sentinel, so this is the entire on-screen presence of an archive.
    pub door: Option<pb_app_core::app_core::DoorCard>,
    /// Whether the left pane is occupied — by the tree **or** its other tab, the thumbnail
    /// strip. The door card centres itself in what's left, and `tree.is_some()` alone would
    /// miss the strip.
    pub left_pane: bool,
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
    /// Show/hide fade ramps (0..=1) for the tree and Inspector — shell-owned like
    /// the info line's `fade` (the shell retains the last frame for the out leg).
    pub tree_fade: f32,
    pub inspector_fade: f32,
    /// The live left-pane width (Folders / Thumbnails), user-resizable via the shared drag
    /// handle (task #83). Shell-owned (`App::tree_width`), so `snapshot` seeds it with the
    /// default and the shell overrides it in `render_overlay_frame`.
    pub pane_width: f32,
    /// The live Inspector width, user-resizable via the drag handle on its left edge (task #83).
    /// Shell-owned (`App::inspector_width`); `snapshot` seeds the default and the shell overrides.
    pub inspector_width: f32,
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
                    tabs: core.native_thumbs,
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
                    tabs: core.native_thumbs,
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
                progress: core.video_progress_row().map(|r| InfoProgress {
                    elapsed: r.elapsed,
                    total: r.total,
                    fraction: r.fraction,
                    playing: core.video_playing(),
                }),
                fade: 1.0,
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
            door: core.door_card(),
            left_pane: core.tree_panel_visible() || core.thumbs_visible(),
            dark: core.hud_dark,
            panel_alpha: opacity_to_alpha(core.settings.info_opacity),
            // Shell-owned (menu-bar visibility lives in the shell); set in render_overlay_frame.
            top_inset: 0.0,
            // Shell-owned fade ramps; set in render_overlay_frame.
            tree_fade: 1.0,
            inspector_fade: 1.0,
            // Shell-owned live width (`App::tree_width`); seeded with the default here.
            pane_width: TREE_WIDTH,
            // Shell-owned live width (`App::inspector_width`); seeded with the default here.
            inspector_width: INSPECTOR_WIDTH,
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

    // The archive door card (task #105) is **content chrome**: it stands in for the photo
    // that isn't there, so it draws first — above the (empty) canvas, below every real
    // panel, which then overlaps it exactly as it would overlap a photo. Centred in the
    // *unobstructed* area: an open side pane shifts it rather than sitting on top of it.
    if let Some(card) = &frame.door {
        let left = if frame.left_pane {
            screen.left() + EDGE + frame.pane_width
        } else {
            screen.left()
        };
        let right = if frame.inspector.is_some() {
            screen.right() - EDGE - frame.pane_width
        } else {
            screen.right()
        };
        let content = egui::Rect::from_min_max(
            egui::pos2(left, screen.top() + frame.top_inset),
            egui::pos2(right.max(left + 1.0), screen.bottom()),
        );
        door_card(ctx, &p, alpha, content, card, actions);
    }
    if let Some(info) = &frame.info {
        info_line(ctx, &p, alpha, info, actions);
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
        let r = duck(
            screen.left() + EDGE,
            screen.left() + EDGE + frame.pane_width,
        );
        tree_panel(
            ctx,
            &p,
            alpha,
            top,
            r,
            tree,
            frame.pane_width,
            frame.tree_fade,
            actions,
        );
    }
    if let Some(insp) = &frame.inspector {
        let r = duck(
            screen.right() - EDGE - frame.inspector_width,
            screen.right() - EDGE,
        );
        inspector_panel(
            ctx,
            &p,
            alpha,
            top,
            r,
            insp,
            frame.inspector_width,
            frame.inspector_fade,
            actions,
        );
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
/// The playback row (task #79): time-label size, row gap, bar↔label gap, bar
/// thickness, and the bar-span floor that keeps a short filename's bar usable.
const INFO_TIME_SIZE: f32 = 11.5;
const INFO_ROW_GAP: f32 = 5.0;
const INFO_BAR_GAP: f32 = 7.0;
const INFO_BAR_H: f32 = 4.0;
const INFO_BAR_MIN: f32 = 110.0;
/// The playback bar's position-knob radius, and how far the click/drag hit band
/// extends above/below the 4 px track (the bar is a scrubber — task #79 follow-up).
const INFO_BAR_KNOB_R: f32 = 4.5;
const INFO_BAR_HIT_PAD: f32 = 6.0;
/// The playback row's play/pause button: the glyph square at the row's very left.
const INFO_PLAY_SIZE: f32 = 12.0;

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
    let mut h = text.y + 2.0 * INFO_INSET;
    // The playback row (task #79): floor the width so the bar is usable even under
    // a short filename; add the row's height. Same numbers as `info_line`'s layout.
    if let Some(pr) = &info.progress {
        let el = measure(&pr.elapsed, INFO_TIME_SIZE);
        let tot_w = pr
            .total
            .as_deref()
            .map(|t| measure(t, INFO_TIME_SIZE).x + INFO_BAR_GAP)
            .unwrap_or(0.0);
        let row_min = INFO_PLAY_SIZE + INFO_BAR_GAP + el.x + INFO_BAR_GAP + INFO_BAR_MIN + tot_w;
        w = w.max(2.0 * INFO_PAD + row_min);
        h += INFO_ROW_GAP + el.y;
    }
    (w, h)
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
/// alignment setting, laid out by hand so everything shares one vertical center. Non-interactive
/// on stills; with the playback row (a live video) the bar is a click/drag scrubber and the pill
/// joins the pointer-routing gate (`App::video_bar_interactive`).
fn info_line(
    ctx: &egui::Context,
    p: &Palette,
    alpha: u8,
    info: &InfoLine,
    actions: &mut Vec<PanelAction>,
) {
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
        // Interactive only while the playback bar is present (a live video): the
        // plain readout must never intercept clicks meant for the photo.
        .interactable(info.progress.is_some())
        .movable(false)
        .order(egui::Order::Middle)
        .show(ctx, |ui| {
            // Mid-fade: scale everything painted below and keep frames coming
            // until the ramp lands (the shell owns the fade clock).
            if info.fade < 1.0 {
                ui.set_opacity(info.fade.clamp(0.0, 1.0));
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(16));
            }
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

            // Content: a single row centers on the pill; with the playback row
            // (task #79), the summary owns the top band and the row sits below.
            let row1_h = text_g.size().y;
            let cy = if info.progress.is_some() {
                rect.top() + INFO_INSET + row1_h / 2.0
            } else {
                rect.center().y
            };
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
                // The badge belongs to the summary ROW, not the pill: with the
                // playback row present, `pill_h` spans both rows and a pill-height
                // badge bleeds down over the bar (owner-reported). `row1_h` equals
                // the old `pill_h - 2·INSET` in the single-row case exactly.
                let badge_h = row1_h;
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

            // The playback row (task #79): elapsed left, total right, the bar
            // filling the span between — brand-accent fill over a dim track.
            if let Some(pr) = &info.progress {
                // While the video plays, glide: ask egui for a ~30 Hz timed
                // repaint (the overlay honors `repaint_at`), so the knob moves
                // smoothly instead of jumping on the once-a-second text refresh.
                // Paused/ended requests nothing — the texture stays retained.
                if pr.playing {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(33));
                }
                let time_font = FontId::new(INFO_TIME_SIZE, FontFamily::Proportional);
                let el_g = galley(
                    ui,
                    &pr.elapsed,
                    time_font.clone(),
                    p.text_secondary,
                    f32::INFINITY,
                );
                let row_top = rect.top() + INFO_INSET + row1_h + INFO_ROW_GAP;
                let rcy = row_top + el_g.size().y / 2.0;
                let mut bar_x0 = rect.left() + INFO_PAD;
                // The play/pause button at the row's very left: pause while
                // playing, play while paused/ended (a click = the `P` key).
                let btn = snap_rect(sq(bar_x0 + INFO_PLAY_SIZE / 2.0, rcy, INFO_PLAY_SIZE), ppp);
                let bresp = ui.interact(
                    btn.expand(3.0),
                    egui::Id::new("pb_video_play"),
                    egui::Sense::click(),
                );
                if bresp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                let glyph = if pr.playing { Icon::Pause } else { Icon::Play };
                // Neutral like the time labels; lifts to full text color on hover.
                if bresp.hovered() {
                    pb_ui::icon::paint_tinted(ui, btn, glyph, p.text);
                } else {
                    pb_ui::icon::paint(ui, btn, glyph, Tone::Neutral, p);
                }
                if bresp.clicked() {
                    actions.push(PanelAction::VideoPlayPause);
                }
                bar_x0 += INFO_PLAY_SIZE + INFO_BAR_GAP;
                paint_vtext(ui, bar_x0, rcy, &el_g);
                bar_x0 += el_g.size().x + INFO_BAR_GAP;
                let mut bar_x1 = rect.right() - INFO_PAD;
                if let Some(total) = &pr.total {
                    let tot_g = galley(ui, total, time_font, p.text_secondary, f32::INFINITY);
                    let tx = rect.right() - INFO_PAD - tot_g.size().x;
                    paint_vtext(ui, tx, rcy, &tot_g);
                    bar_x1 = tx - INFO_BAR_GAP;
                }
                if bar_x1 - bar_x0 > 8.0 {
                    let bar = snap_rect(
                        egui::Rect::from_min_max(
                            egui::pos2(bar_x0, rcy - INFO_BAR_H / 2.0),
                            egui::pos2(bar_x1, rcy + INFO_BAR_H / 2.0),
                        ),
                        ppp,
                    );
                    // The bar is a scrubber (task #79 follow-up): click/drag seeks
                    // to that fraction. The hit band is taller than the 4 px track
                    // so the knob is grabbable without pixel-hunting.
                    let hit = bar.expand2(egui::vec2(INFO_BAR_KNOB_R, INFO_BAR_HIT_PAD));
                    let resp = ui.interact(
                        hit,
                        egui::Id::new("pb_video_bar"),
                        egui::Sense::click_and_drag(),
                    );
                    let engaged = resp.is_pointer_button_down_on() || resp.dragged();
                    // One cursor for hover AND drag: the seek-bar convention (the
                    // moving knob is the drag feedback). Grab/Grabbing has no native
                    // Windows cursor — winit falls back to a distracting crosshair.
                    if resp.hovered() || engaged {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    // While engaged, show (and seek to) the pointer's fraction —
                    // immediate feedback; the core's row catches up on landing.
                    let mut frac = pr.fraction.clamp(0.0, 1.0);
                    if let Some(pos) = resp.interact_pointer_pos() {
                        if engaged || resp.clicked() || resp.drag_stopped() {
                            frac = ((pos.x - bar.left()) / bar.width()).clamp(0.0, 1.0);
                            // Re-seek only on real movement (≥ 1 px), so holding
                            // still doesn't flood the producer with reseeks.
                            let sent = egui::Id::new("pb_video_bar_sent");
                            let last: Option<f32> = ui.data(|d| d.get_temp(sent));
                            if last.is_none_or(|l| (l - frac).abs() * bar.width() >= 1.0)
                                || resp.clicked()
                                || resp.drag_stopped()
                            {
                                ui.data_mut(|d| d.insert_temp(sent, frac));
                                actions.push(PanelAction::SeekVideo(frac));
                            }
                        }
                    }
                    let round = Rounding::same(bar.height() / 2.0);
                    ui.painter()
                        .rect_filled(bar, round, p.text_secondary.gamma_multiply(0.35));
                    if frac > 0.0 {
                        let fill_w = (bar.width() * frac).max(bar.height());
                        let fill =
                            egui::Rect::from_min_size(bar.min, egui::vec2(fill_w, bar.height()));
                        ui.painter().rect_filled(fill, round, p.accent);
                    }
                    // The position knob: a round grab handle riding the fill's
                    // leading edge, slightly enlarged under the pointer.
                    let knob_x = bar.left() + bar.width() * frac;
                    let knob_r = if resp.hovered() || engaged {
                        INFO_BAR_KNOB_R + 1.0
                    } else {
                        INFO_BAR_KNOB_R
                    };
                    ui.painter().circle_filled(
                        egui::pos2(knob_x, bar.center().y),
                        knob_r,
                        if resp.hovered() || engaged {
                            lighten(p.accent, 0.18)
                        } else {
                            p.accent
                        },
                    );
                }
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
        1.0,
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

/// The **door card** (task #105): an archive's whole on-screen presence — the folder
/// artwork, its name, and the `Open` button — centred in the content area.
///
/// A door's decoded frame is a 1×1 transparent sentinel, so without this the viewer sees an
/// empty letterbox. It is *content chrome*: ambient and non-modal, but unlike the play hint
/// it never fades and it survives blazing — suppressing it would show a blank screen, which
/// reads as broken rather than fast.
///
/// **Built on the panel primitives, not hand-rolled** (owner, 2026-07-17: *"a bordered card
/// with a header section using a separator just like the Keyboard Shortcuts panel"*). Same
/// [`sdf_panel`] frame, same [`panel_header`] type via [`TITLE_SIZE`], same [`groove`]
/// separator — so it is a sibling of Help and the Inspector rather than a lookalike that
/// drifts. It takes no ✕: a door isn't dismissible, you navigate off it.
///
/// **Sizing (plan 105 §5).** The artwork is capped at `DOOR_ART_PT` *and* at its native
/// resolution for this display (`asset_px / pixels_per_point`), so it is never magnified,
/// then shrinks to whatever the window allows. On a cramped window (the macOS minimum is
/// 520×360) the **art gives way first** and the name and button keep their readable sizes.
/// The asset arrives cropped to its content (`engine::door_artwork`), so the insets here
/// mean what they say.
fn door_card(
    ctx: &egui::Context,
    p: &Palette,
    base_alpha: u8,
    content: egui::Rect,
    card: &pb_app_core::app_core::DoorCard,
    actions: &mut Vec<PanelAction>,
) {
    /// The design cap for the artwork's **width**, in points — the folder itself, since the
    /// asset is cropped to its content.
    const DOOR_ART_PT: f32 = 148.0;
    /// The card **adapts** to its filename between these bounds. Most archive names are
    /// short, and a card sized for the worst case is a slab of empty space; but an
    /// unbounded one becomes a banner that collides with the tree and Inspector on a narrow
    /// window (owner, 2026-07-17). Past `DOOR_W_MAX` — or past whatever the window allows —
    /// the name middle-elides instead of widening it further.
    const DOOR_W_MIN: f32 = 216.0;
    const DOOR_W_MAX: f32 = 340.0;
    /// Inset around the body (art / name / button) — the **same** on every side, so the
    /// card's padding reads as one value rather than four guesses.
    const BODY_PAD: f32 = 16.0;
    /// Between the body's rows — one value, so the art→name and name→button gaps match.
    const GAP: f32 = 12.0;

    let art = door_art_texture(ctx);
    let name_font = FontId::new(15.0, FontFamily::Proportional);
    let title_font = FontId::new(TITLE_SIZE, FontFamily::Name(pb_ui::SEMIBOLD.into()));

    // Width follows the name, clamped: never narrower than the art + button need, never
    // wider than a card should be, and never wider than the room it sits in.
    let text_w = |s: &str, f: &FontId| -> f32 {
        ctx.fonts(|fonts| {
            fonts
                .layout_no_wrap(s.to_string(), f.clone(), p.text)
                .size()
                .x
        })
    };

    // Never magnify: cap at the design size *and* at the asset's native size for this
    // display's scale, then fit the window (art first). `EDGE` keeps the card off the
    // viewport's edges.
    let room = content.shrink(EDGE);
    let door_w = (text_w(&card.name, &name_font) + 2.0 * BODY_PAD)
        .max(text_w(&card.format, &title_font) + 2.0 * HEADER_PAD_H)
        .clamp(DOOR_W_MIN, DOOR_W_MAX)
        .min(room.width().max(DOOR_W_MIN));
    let aspect = art.map_or(1.0, |(_, w, h)| w as f32 / h.max(1) as f32);
    let native_pt = art
        .map(|(_, w, _)| w as f32 / ctx.pixels_per_point())
        .unwrap_or(DOOR_ART_PT);
    let name_h = ctx.fonts(|f| f.row_height(&name_font));
    // Everything but the artwork and its gap. `card_h` below adds those back, so the fit
    // and the placement can never disagree about the card's height.
    let chrome_h = HEADER_H + 1.0 /* the groove */ + 2.0 * BODY_PAD + name_h + GAP + OPEN_BTN_H;
    let art_pt = DOOR_ART_PT
        .min(native_pt)
        .min(door_w - 2.0 * BODY_PAD)
        .min((room.height() - chrome_h - GAP).max(0.0) * aspect)
        .max(0.0);
    let art_size = egui::vec2(art_pt, art_pt / aspect);
    let show_art = art_pt > 32.0;

    // Place it from a size we **computed**, not one egui measured last time.
    //
    // `sdf_panel`'s Window is auto-sized, and an anchored auto-sized Window is positioned
    // from the rect egui cached on its previous run (this file says so a few lines down:
    // "egui's auto-sized Window caches its rect and updates it a frame late"). Every other
    // panel is immune because its width is a constant — this card's width follows the
    // filename, so a `CENTER_CENTER` anchor centred each door using the *previous* door's
    // width. And because the retained overlay only rebuilds when the shell dirties it,
    // egui's settle frame never came: the error persisted for the whole door, cleared on
    // the next one, and re-appeared on the one after that (owner, 2026-07-17).
    //
    // Anchoring the **top-left** to a position we derive from `door_w`/`card_h` removes the
    // dependency entirely: no cached size, nothing to settle, correct on the first frame.
    let card_h = chrome_h + if show_art { art_size.y + GAP } else { 0.0 };
    let top_left = content.center() - egui::vec2(door_w, card_h) / 2.0;
    let offset = top_left - ctx.screen_rect().left_top();
    sdf_panel(
        ctx,
        p,
        base_alpha,
        "pb_door_card",
        Align2::LEFT_TOP,
        offset,
        room.height().max(120.0),
        PANEL_RADIUS,
        egui::Margin::ZERO,
        1.0,
        |ui| {
            ui.set_width(door_w);
            ui.set_max_width(door_w);
            ui.spacing_mut().item_spacing.y = 0.0;

            // The header: the panel type, centred (the card has no ✕ to balance against),
            // then the same groove every panel puts under its header.
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), HEADER_H),
                egui::Sense::hover(),
            );
            let g = galley(
                ui,
                &card.format,
                FontId::new(TITLE_SIZE, FontFamily::Name(pb_ui::SEMIBOLD.into())),
                p.text,
                f32::INFINITY,
            );
            paint_vtext(ui, rect.center().x - g.size().x / 2.0, rect.center().y, &g);
            groove(ui, p);

            egui::Frame::none()
                .inner_margin(egui::Margin::same(BODY_PAD))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        // The art is the first thing to go on a cramped window.
                        if show_art {
                            if let Some((tex, _, _)) = art {
                                ui.add(
                                    egui::Image::new((tex, art_size)).fit_to_exact_size(art_size),
                                );
                                ui.add_space(GAP);
                            }
                        }
                        // Middle-elided so a long name keeps its extension — the same rule
                        // the thumb strip's cells use.
                        let avail = (door_w - 2.0 * BODY_PAD).max(80.0);
                        let name =
                            middle_truncate(ui, &card.name, name_font.clone(), p.text, avail);
                        ui.label(name);
                        ui.add_space(GAP);

                        let w = open_button_width(ui, "Open", &card.shortcut);
                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(w, OPEN_BTN_H), egui::Sense::click());
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        let btn = if resp.hovered() {
                            p.control_hover
                        } else {
                            p.control
                        };
                        draw_open_button(
                            ui,
                            rect,
                            p,
                            Icon::Archive,
                            "Open",
                            &card.shortcut,
                            Color32::from_rgba_unmultiplied(btn.r(), btn.g(), btn.b(), 255),
                            1.0,
                        );
                        if resp.clicked() {
                            actions.push(PanelAction::PlayPause);
                        }
                    });
                });
        },
    );
}

/// The door artwork as an egui texture — uploaded **once** per context.
///
/// The pixels come from `pb_app_core::engine::door_artwork` (one decode for the process);
/// this caches the upload in egui's own texture manager, keyed by name, so a folder of
/// forty doors uploads nothing after the first. `None` if the art can't be decoded — the
/// card then degrades to text and a button rather than vanishing.
fn door_art_texture(ctx: &egui::Context) -> Option<(egui::TextureId, u32, u32)> {
    let art = pb_app_core::engine::door_artwork()?;
    let id = egui::Id::new("pb_door_art");

    // Read → load → insert, each as its own context access. **Never** call `load_texture`
    // inside `data_mut`: `data_mut` holds a write lock on the whole `Context`, and
    // `load_texture` re-enters it to reach the texture manager. egui's lock is not
    // reentrant, so that deadlocks the event loop the instant the first door renders —
    // which is exactly what it did (owner: "the app freezes and effectively crashes").
    // `pb_ui::icon::texture` had this right all along; this is the same shape.
    let handle = match ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        Some(h) => h,
        None => {
            let img = egui::ColorImage::from_rgba_unmultiplied(
                [art.width as usize, art.height as usize],
                &art.pixels,
            );
            let h = ctx.load_texture("pb_door_art", img, egui::TextureOptions::LINEAR);
            ctx.data_mut(|d| d.insert_temp(id, h.clone()));
            h
        }
    };
    Some((handle.id(), art.width, art.height))
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
    let (icon, label) = if frame.kind == 1 {
        (Icon::LivePhoto, "Play")
    } else {
        (Icon::Play, "Play")
    };
    egui::Area::new(egui::Id::new("pb_play_hint"))
        .anchor(Align2::CENTER_BOTTOM, egui::vec2(0.0, -bottom))
        .order(egui::Order::Middle)
        .show(ctx, |ui| {
            ui.set_opacity(frame.alpha);
            let w = open_button_width(ui, label, &frame.shortcut);
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
            draw_open_button(ui, rect, p, icon, label, &frame.shortcut, fill, frame.alpha);
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
    fade: f32,
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
            // Mid-fade (the tree/inspector show-hide ramp): scale everything the
            // content paints and keep frames coming until the ramp lands.
            if fade < 1.0 {
                ui.set_opacity(fade.clamp(0.0, 1.0));
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(16));
            }
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
    // The backfilled shadow + SDF background bypass the Ui's opacity (they're
    // set on the layer painter after the fact) — scale their colors by the
    // fade explicitly so the whole panel dissolves as one.
    painter.set(
        shadow_idx,
        egui::epaint::Shadow {
            offset: egui::vec2(0.0, 5.0),
            blur: 18.0,
            spread: 0.0,
            color: Color32::from_black_alpha(70).gamma_multiply(fade),
        }
        .as_shape(rect, Rounding::same(radius)),
    );
    let fill = panel_surface(p);
    painter.set(
        bg_idx,
        crate::sdf_rect::round_rect_shape(
            rect,
            radius,
            Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), alpha)
                .gamma_multiply(fade),
            1.0,
            separator(p).gamma_multiply(fade),
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
        1.0,
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

#[allow(clippy::too_many_arguments)]
fn inspector_panel(
    ctx: &egui::Context,
    p: &Palette,
    alpha: u8,
    top_inset: f32,
    duck: f32,
    insp: &InspectorFrame,
    width: f32,
    fade: f32,
    actions: &mut Vec<PanelAction>,
) {
    let max_h = panel_max_height(ctx, 200.0, top_inset, duck);
    // Content wrap width (pins long EXIF values / Markdown so they wrap instead of widening the
    // Window), tracking the live resizable width less the frame margins and scrollbar gutter.
    let content_w = width - 2.0 * 16.0 - 8.0;
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
        fade,
        |ui| {
            ui.set_width(width);
            ui.set_max_width(width);
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
                            InspectorSnapshot::Details(d) => details_body(ui, p, d, content_w),
                            InspectorSnapshot::Text(t) => text_body(ui, p, t),
                            InspectorSnapshot::Describe(d) => {
                                describe_body(ui, p, d, content_w, actions)
                            }
                        }
                    });
            });
            // Resize handle on the left edge (right-anchored panel: drag left → wider).
            let (w, zone) = resize_handle(
                ui,
                p,
                width,
                ResizeEdge::Left,
                INSPECTOR_WIDTH_MIN,
                INSPECTOR_WIDTH_MAX,
            );
            if let Some(w) = w {
                actions.push(PanelAction::SetInspectorWidth(w));
            }
            actions.push(PanelAction::InspectorResizeZone(zone));
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
    let icon_sz = 12.5;
    let icon_gap = 5.0;
    let track_h = 24.0;
    let track_left = header_rect.left() + HEADER_PAD_H;
    // Room before the right-hand copy/close icons — the tab bar never runs under them.
    let avail = (controls_left - 8.0 - track_left).max(40.0);
    // Measure each label once (reused for the fit decision and the natural widths).
    let label_ws: Vec<f32> = tabs
        .iter()
        .map(|(_, l, _)| {
            galley(ui, l, font.clone(), Color32::PLACEHOLDER, f32::INFINITY)
                .size()
                .x
        })
        .collect();
    // Fit-driven shedding (three tabs — tighter than the left pane's two, so decide by what
    // actually fits rather than a fixed width threshold, which also adapts to the copy button):
    // icon + label when it fits, else label-only (icons dropped) as the Inspector narrows, else
    // icon-only near the floor. Same three modes + drawing as `left_pane_tabs`.
    let mode_total = |show_icon: bool, show_label: bool| -> f32 {
        let seg_pad = if show_label { 10.0 } else { 8.0 };
        label_ws
            .iter()
            .map(|lw| {
                let icon_part = if show_icon { icon_sz } else { 0.0 };
                let label_part = if show_label {
                    let gap = if show_icon { icon_gap } else { 0.0 };
                    gap + lw
                } else {
                    0.0
                };
                icon_part + label_part + seg_pad * 2.0
            })
            .sum::<f32>()
            + 4.0
    };
    let (show_icon, show_label) = if mode_total(true, true) <= avail {
        (true, true)
    } else if mode_total(false, true) <= avail {
        (false, true)
    } else {
        (true, false)
    };
    let seg_pad = if show_label { 10.0 } else { 8.0 };
    // Natural segment widths for the chosen content, then scaled down so the track never
    // overflows even at the floor (the left-pane lesson — clamping alone let segments spill).
    let natural: Vec<f32> = label_ws
        .iter()
        .map(|lw| {
            let icon_part = if show_icon { icon_sz } else { 0.0 };
            let label_part = if show_label {
                let gap = if show_icon { icon_gap } else { 0.0 };
                gap + lw
            } else {
                0.0
            };
            icon_part + label_part + seg_pad * 2.0
        })
        .collect();
    let natural_total = natural.iter().sum::<f32>() + 4.0;
    let scale = (avail / natural_total).min(1.0);
    let widths: Vec<f32> = natural.iter().map(|w| w * scale).collect();
    let total = natural_total * scale;
    let track = egui::Rect::from_min_size(
        egui::pos2(track_left, cy - track_h / 2.0),
        egui::vec2(total, track_h),
    );
    crate::sdf_rect::round_rect(ui, track, 7.0, quaternary(p), 0.0, Color32::TRANSPARENT);
    let mut x = track_left + 2.0 * scale;
    for ((tab, label, icon), w) in tabs.iter().zip(&widths) {
        let seg = egui::Rect::from_min_size(
            egui::pos2(x, track.top() + 2.0),
            egui::vec2(*w, track_h - 4.0),
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
            p.on_accent
        } else {
            panel_secondary(p)
        };
        match (show_icon, show_label) {
            // Icon + label: center the [icon · gap · label] group in the segment.
            (true, true) => {
                let g = galley(ui, label, font.clone(), color, f32::INFINITY);
                let group_w = icon_sz + icon_gap + g.size().x;
                let gx = seg.center().x - group_w / 2.0;
                pb_ui::icon::paint_tinted(ui, sq(gx + icon_sz / 2.0, cy, icon_sz), *icon, color);
                paint_vtext(ui, gx + icon_sz + icon_gap, cy, &g);
            }
            // Label only (icons dropped as the Inspector narrows).
            (false, true) => {
                let g = galley(ui, label, font.clone(), color, f32::INFINITY);
                paint_vtext(ui, seg.center().x - g.size().x / 2.0, cy, &g);
            }
            // Icon only (labels dropped near the floor).
            _ => {
                pb_ui::icon::paint_tinted(ui, sq(seg.center().x, cy, icon_sz), *icon, color);
            }
        }
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

fn details_body(ui: &mut egui::Ui, p: &Palette, d: &DetailsPanel, content_w: f32) {
    if d.rows.is_empty() {
        ui.label(RichText::new("Nothing to show").color(panel_secondary(p)));
        return;
    }
    // Pin the content width so long EXIF values (e.g. the Flash string) **wrap** inside the
    // panel instead of widening the whole Window — egui grows an auto-sized Window to fit a
    // non-wrapping row, which is what blew the panel out to full width. Everything below
    // wraps at this width. (Leaves room for the vertical scrollbar the tab usually shows.)
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
    content_w: f32,
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
            crate::md::render(ui, p, text, content_w);
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

#[allow(clippy::too_many_arguments)]
fn tree_panel(
    ctx: &egui::Context,
    p: &Palette,
    alpha: u8,
    top_inset: f32,
    duck: f32,
    tree: &TreeFrame,
    pane_width: f32,
    fade: f32,
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
        fade,
        |ui| {
            ui.set_width(pane_width);
            ui.set_max_width(pane_width);
            // No inter-element gap: header, groove, and body stack flush, so the panel is
            // exactly its content height and honors the bottom edge inset (#1). Each region
            // owns its own internal padding.
            ui.spacing_mut().item_spacing.y = 0.0;
            // With a thumbnail strip available (task #83) the header is the shared left-pane
            // tab bar (Folders | Thumbnails), Folders selected; otherwise the plain title.
            let close = if tree.tabs {
                let (rect, cy, controls_left, _copy, close) = header_bar(ui, p, None);
                left_pane_tabs(ui, p, rect, cy, controls_left, LeftTab::Folders, actions);
                close
            } else {
                panel_header(ui, p, "Folders", None).1
            };
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
            let (w, zone) = resize_handle(
                ui,
                p,
                pane_width,
                ResizeEdge::Right,
                PANE_WIDTH_MIN,
                PANE_WIDTH_MAX,
            );
            if let Some(w) = w {
                actions.push(PanelAction::SetPaneWidth(w));
            }
            actions.push(PanelAction::PaneResizeZone(zone));
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

// ── Thumbnails strip (task #83) ───────────────────────────────────────────────
//
// The left pane's second tab: a scrollable vertical strip of neighbor thumbnails, click to
// jump, auto-follow, fixed uniform 3:2 cells, type badges, session rotation at draw, a
// broken-image glyph for failed items. Pixels come from the core's RAM-only thumb store
// (`core.thumbs.cache`), pulled once per cell into an egui texture (freed on leave). Unlike
// the other panels this reads `&AppCore` directly (not a pure snapshot) because it needs live
// pixel access + the shell's texture cache — so `render_overlay_frame` calls it AFTER `build`,
// inside the same egui frame. The per-cell metadata reads mirror the macOS FFI accessors
// (`pb-mac-ffi`'s `thumb_name`/`thumb_badge`/`thumb_rotation`/`thumb_failed`); keep the two in
// step until a future refactor promotes them to shared `AppCore` methods.

/// Strip side inset (also the gap that leaves room for the scrollbar on the right).
const STRIP_PAD: f32 = 8.0;
/// The cell's own inner padding around the photo (macOS parity: ONE cell card, the photo
/// breathes inside it).
const CELL_INNER_PAD: f32 = 7.0;
/// The label band's gaps run tighter than the image margin (4 vs 7): the caption's own leading
/// adds ~3pt of air, so equal constants read bottom-heavy (owner note, macOS round).
const LABEL_GAP: f32 = 4.0;
const LABEL_HEIGHT: f32 = 15.0;
/// Gap below each cell card (the strip's inter-cell rhythm).
const CELL_GAP: f32 = 6.0;
/// Cell card + inner image-box corner radii (macOS parity: 8 / 5).
const CELL_RADIUS: f32 = 8.0;
/// The caption + placeholder/badge glyph sizes.
const THUMB_LABEL_SIZE: f32 = 11.5;
const THUMB_PLACEHOLDER_ICON: f32 = 26.0;
const THUMB_BADGE_ICON: f32 = 11.0;

/// The winit shell's per-strip state (lives on `App`, survives across frames): an egui texture
/// cache keyed by `(item, entry generation)` — pull-once, freed when a cell leaves
/// visible+overscan (dropping a `TextureHandle` frees its GPU texture) — plus the scroll
/// bookkeeping the FollowState handshake and user-scroll detection need.
#[derive(Default)]
pub struct ThumbStripState {
    textures: HashMap<(usize, u64), egui::TextureHandle>,
    /// Last vertical scroll offset — a change we didn't program is the user grabbing the strip.
    last_offset: Option<f32>,
    /// The last item a follow-scroll centered (reserved for the smooth-vs-snap rule; v1 snaps).
    last_centered: Option<usize>,
    /// The last `(visible, overscan)` range reported, so the core is signalled only on change.
    last_viewport: Option<((usize, usize), (usize, usize))>,
}

/// The shared left-pane tab bar (Folders | Thumbnails) — the Inspector's segmented control
/// with two segments, drawn into the header so the pills, icons, and labels share one vertical
/// center. `selected` is the tab the hosting panel is showing (never stale: the mount
/// condition encodes it). Only a click on the *other* tab pushes an action, so applying it
/// never toggle-closes the pane (macOS `showLeftTab` parity). Below ~233pt the icons hide and
/// the labels stay (the macOS compact rule).
fn left_pane_tabs(
    ui: &mut egui::Ui,
    p: &Palette,
    header_rect: egui::Rect,
    cy: f32,
    controls_left: f32,
    selected: LeftTab,
    actions: &mut Vec<PanelAction>,
) {
    // Three modes as the pane narrows (owner request): icon + label (wide), label-only (middle
    // band — icons dropped), icon-only (near the floor — labels dropped). "Thumbnails" is the
    // long word that drives the steps.
    let w = header_rect.width();
    let show_label = w >= TAB_LABEL_ONLY_MIN;
    let show_icon = w >= TAB_ICON_LABEL_MIN || !show_label;
    let tabs = [
        (LeftTab::Folders, "Folders", Icon::Folder),
        (LeftTab::Thumbnails, "Thumbnails", Icon::Images),
    ];
    let font = FontId::new(12.5, FontFamily::Proportional);
    let seg_pad = if show_label { 10.0 } else { 8.0 };
    let icon_sz = 12.5;
    let icon_gap = 5.0;
    let track_h = 24.0;
    // Natural segment widths for the chosen content.
    let natural: Vec<f32> = tabs
        .iter()
        .map(|(_, l, _)| {
            let icon_part = if show_icon { icon_sz } else { 0.0 };
            let label_part = if show_label {
                let gap = if show_icon { icon_gap } else { 0.0 };
                gap + galley(ui, l, font.clone(), Color32::PLACEHOLDER, f32::INFINITY)
                    .size()
                    .x
            } else {
                0.0
            };
            icon_part + label_part + seg_pad * 2.0
        })
        .collect();
    let track_left = header_rect.left() + HEADER_PAD_H;
    // Scale everything down if it wouldn't fit before the ✕ — the tab bar never overflows the
    // panel, however narrow it's dragged (it just shrinks the pills).
    let avail = (controls_left - 8.0 - track_left).max(40.0);
    let natural_total = natural.iter().sum::<f32>() + 4.0;
    let scale = (avail / natural_total).min(1.0);
    let widths: Vec<f32> = natural.iter().map(|w| w * scale).collect();
    let total = natural_total * scale;
    let track = egui::Rect::from_min_size(
        egui::pos2(track_left, cy - track_h / 2.0),
        egui::vec2(total, track_h),
    );
    crate::sdf_rect::round_rect(ui, track, 7.0, quaternary(p), 0.0, Color32::TRANSPARENT);
    let mut x = track_left + 2.0 * scale;
    for ((tab, label, icon), w) in tabs.iter().zip(&widths) {
        let seg = egui::Rect::from_min_size(
            egui::pos2(x, track.top() + 2.0),
            egui::vec2(*w, track_h - 4.0),
        );
        let resp = ui.interact(seg, ui.id().with(("ltab", *label)), egui::Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let is_sel = *tab == selected;
        if is_sel {
            crate::sdf_rect::round_rect(ui, seg, 5.0, p.accent, 0.0, Color32::TRANSPARENT);
        }
        let color = if is_sel {
            p.on_accent
        } else {
            panel_secondary(p)
        };
        match (show_icon, show_label) {
            // Icon + label: center the [icon · gap · label] group.
            (true, true) => {
                let g = galley(ui, label, font.clone(), color, f32::INFINITY);
                let group_w = icon_sz + icon_gap + g.size().x;
                let gx = seg.center().x - group_w / 2.0;
                pb_ui::icon::paint_tinted(ui, sq(gx + icon_sz / 2.0, cy, icon_sz), *icon, color);
                paint_vtext(ui, gx + icon_sz + icon_gap, cy, &g);
            }
            // Label only (icons dropped in the middle band).
            (false, true) => {
                let g = galley(ui, label, font.clone(), color, f32::INFINITY);
                paint_vtext(ui, seg.center().x - g.size().x / 2.0, cy, &g);
            }
            // Icon only (labels dropped near the floor).
            _ => {
                pb_ui::icon::paint_tinted(ui, sq(seg.center().x, cy, icon_sz), *icon, color);
            }
        }
        if resp.clicked() && !is_sel {
            actions.push(PanelAction::SelectLeftTab(*tab));
        }
        x += w;
    }
}

/// Which edge of a panel carries the drag handle. The left pane grows from its **right** edge
/// (drag right → wider); the right-anchored Inspector grows from its **left** edge (drag left →
/// wider — the opposite sign).
#[derive(Clone, Copy, PartialEq)]
enum ResizeEdge {
    Right,
    Left,
}

/// A drag handle on a panel edge, shared by the left pane (right edge) and the Inspector (left
/// edge) — one knob idiom. Spans the panel's content height (call it **last** inside the content
/// so `ui.min_rect()` already covers the whole panel). Returns the new clamped width when
/// dragged, **and** the handle's strip rect (egui points) so the shell can own the resize cursor
/// geometrically — egui's per-frame hover cursor lags crossing in from the photo, which was the
/// flicker. A hairline grip shows on hover/drag. Sits on the outer few px so it beats the
/// scrollbar only at the very edge.
fn resize_handle(
    ui: &mut egui::Ui,
    p: &Palette,
    cur_width: f32,
    edge: ResizeEdge,
    min: f32,
    max: f32,
) -> (Option<f32>, egui::Rect) {
    let rect = ui.min_rect();
    let grab = 7.0;
    let strip = match edge {
        ResizeEdge::Right => egui::Rect::from_min_max(
            egui::pos2(rect.right() - grab, rect.top()),
            egui::pos2(rect.right(), rect.bottom()),
        ),
        ResizeEdge::Left => egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.top()),
            egui::pos2(rect.left() + grab, rect.bottom()),
        ),
    };
    let resp = ui.interact(
        strip,
        ui.id().with(("resize", edge == ResizeEdge::Left)),
        egui::Sense::drag(),
    );
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        let col = if resp.dragged() {
            p.accent
        } else {
            p.text_secondary.gamma_multiply(0.6)
        };
        let vx = match edge {
            ResizeEdge::Right => rect.right() - 2.0,
            ResizeEdge::Left => rect.left() + 2.0,
        };
        ui.painter().vline(
            vx,
            (rect.top() + 8.0)..=(rect.bottom() - 8.0),
            Stroke::new(2.0_f32, col),
        );
    }
    let new_width = resp.dragged().then(|| {
        // Right edge grows with a rightward drag; the left-anchored-on-the-right Inspector grows
        // with a leftward drag, so its delta is negated.
        let delta = match edge {
            ResizeEdge::Right => resp.drag_delta().x,
            ResizeEdge::Left => -resp.drag_delta().x,
        };
        (cur_width + delta).clamp(min, max)
    });
    (new_width, strip)
}

/// Render the Thumbnails strip. `pending_scroll` is the follow command the shell already took
/// from the core (target item + generation) — applied as a snap this frame.
#[allow(clippy::too_many_arguments)]
pub fn thumbs_panel(
    ctx: &egui::Context,
    core: &AppCore,
    state: &mut ThumbStripState,
    dark: bool,
    alpha: u8,
    top_inset: f32,
    pane_width: f32,
    fade: f32,
    pending_scroll: Option<(usize, u64)>,
    actions: &mut Vec<PanelAction>,
) {
    let p = Palette::new(dark);
    let count = core.source.len();
    let current = core.playlist.current();
    let max_h = panel_max_height(ctx, 200.0, top_inset, 0.0);

    // Fixed 3:2 landscape cell (macOS parity) so rotation never reflows the strip and
    // scroll↔index stays O(1). Width is derived from the live (resizable) pane width.
    let cell_width = (pane_width - 2.0 * STRIP_PAD).max(80.0);
    let box_width = cell_width - 2.0 * CELL_INNER_PAD;
    let box_height = (box_width * 2.0 / 3.0).round();
    let cell_height = box_height + CELL_INNER_PAD + LABEL_HEIGHT + 2.0 * LABEL_GAP;
    let row_pitch = cell_height + CELL_GAP;

    // Collected inside the scroll closure, applied after so `actions` isn't double-borrowed.
    let mut keep: std::collections::HashSet<(usize, u64)> = std::collections::HashSet::new();
    let mut clicked: Option<usize> = None;
    let mut new_range: Option<(usize, usize)> = None;

    sdf_panel(
        ctx,
        &p,
        alpha,
        "pb_thumbs",
        Align2::LEFT_TOP,
        egui::vec2(EDGE, EDGE + top_inset),
        max_h,
        PANEL_RADIUS,
        egui::Margin::ZERO,
        fade,
        |ui| {
            ui.set_width(pane_width);
            ui.set_max_width(pane_width);
            ui.spacing_mut().item_spacing.y = 0.0;
            let (rect, cy, controls_left, _copy, close) = header_bar(ui, &p, None);
            left_pane_tabs(
                ui,
                &p,
                rect,
                cy,
                controls_left,
                LeftTab::Thumbnails,
                actions,
            );
            if close {
                actions.push(PanelAction::CloseThumbs);
            }
            groove(ui, &p);

            // The strip body: a virtualized ScrollArea over uniform-pitch rows. Fit it to the
            // deck, capped at the available height, then scroll. `clip_rect_margin = 0` so
            // scrolled-out rows don't bleed a sliver past the panel (the tree's lesson).
            let content_h = count as f32 * row_pitch + 8.0;
            let body_h = content_h.min(max_h - HEADER_H - 1.0).max(60.0);
            ui.visuals_mut().clip_rect_margin = 0.0;

            // A pending follow-scroll centers the target row this frame (snap; smooth is a
            // later polish). We set the offset ourselves, so the resulting movement is NOT
            // treated as a user scroll below.
            let target_off = pending_scroll.map(|(item, _)| {
                (item as f32 * row_pitch + row_pitch / 2.0 - body_h / 2.0).max(0.0)
            });

            let mut area = egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height(body_h)
                .min_scrolled_height(body_h);
            if let Some(off) = target_off {
                area = area.vertical_scroll_offset(off);
            }
            let out = area.show_rows(ui, row_pitch, count.max(1), |ui, row_range| {
                if count == 0 {
                    return;
                }
                new_range = Some((row_range.start, row_range.end.saturating_sub(1)));
                for i in row_range {
                    let (row_rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), row_pitch),
                        egui::Sense::hover(),
                    );
                    let card = egui::Rect::from_min_size(
                        egui::pos2(row_rect.left() + STRIP_PAD, row_rect.top()),
                        egui::vec2(cell_width, cell_height),
                    );
                    thumb_cell(
                        ui,
                        &p,
                        core,
                        state,
                        i,
                        current == Some(i),
                        card,
                        box_width,
                        box_height,
                        &mut keep,
                        &mut clicked,
                    );
                }
            });

            // User-scroll detection: an offset change we did NOT program is the user grabbing
            // the strip → detach auto-follow (generation-fenced, like the SwiftUI shell).
            let off = out.state.offset.y;
            if target_off.is_none() {
                if let Some(prev) = state.last_offset {
                    if (prev - off).abs() > 0.5 {
                        actions.push(PanelAction::ThumbUserScrolled);
                    }
                }
            }
            state.last_offset = Some(off);

            let (w, zone) = resize_handle(
                ui,
                &p,
                pane_width,
                ResizeEdge::Right,
                PANE_WIDTH_MIN,
                PANE_WIDTH_MAX,
            );
            if let Some(w) = w {
                actions.push(PanelAction::SetPaneWidth(w));
            }
            actions.push(PanelAction::PaneResizeZone(zone));
        },
    );

    // The follow handshake: we centered the target this frame, so tell FollowState its
    // animation landed (stale generations are ignored by the core).
    if let Some((item, gen)) = pending_scroll {
        state.last_centered = Some(item);
        actions.push(PanelAction::ThumbScrollDone(gen));
    }
    if let Some(i) = clicked {
        actions.push(PanelAction::ThumbClick(i));
    }
    if let Some((lo, hi)) = new_range {
        let rows = (hi - lo) + 1;
        let over = rows * 2;
        let over_lo = lo.saturating_sub(over);
        let over_hi = (hi + over).min(count.saturating_sub(1));
        let vp = ((lo, hi), (over_lo, over_hi));
        if state.last_viewport != Some(vp) {
            state.last_viewport = Some(vp);
            actions.push(PanelAction::ThumbViewport {
                visible: (lo, hi),
                overscan: (over_lo, over_hi),
            });
        }
    }
    // Free textures for cells no longer in demand (kept = materialized this frame).
    state.textures.retain(|k, _| keep.contains(k));
}

/// One fixed-size strip cell drawn into `card`: the translucent card, the thumb fit-within a
/// letterbox box (session rotation at draw), a type badge, the current/hover highlight, and
/// the middle-truncated filename below. A cached thumb is uploaded once into `state.textures`
/// (keyed by the entry generation) and its key recorded in `keep` so it survives eviction.
#[allow(clippy::too_many_arguments)]
fn thumb_cell(
    ui: &mut egui::Ui,
    p: &Palette,
    core: &AppCore,
    state: &mut ThumbStripState,
    i: usize,
    is_current: bool,
    card: egui::Rect,
    box_width: f32,
    box_height: f32,
    keep: &mut std::collections::HashSet<(usize, u64)>,
    clicked: &mut Option<usize>,
) {
    let resp = ui.interact(card, ui.id().with(("thumbcell", i)), egui::Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        *clicked = Some(i);
    }

    // The card: one translucent rounded rect + hairline border; current tints accent.
    let (fill, border, border_w) = if is_current {
        (
            p.accent.gamma_multiply(0.24),
            p.accent.gamma_multiply(0.85),
            1.5,
        )
    } else if hovered {
        (p.text_secondary.gamma_multiply(0.12), separator(p), 1.0)
    } else {
        (quaternary(p).gamma_multiply(0.5), separator(p), 1.0)
    };
    crate::sdf_rect::round_rect(ui, card, CELL_RADIUS, fill, border_w, border);

    // The letterbox image box, inset by the inner pad.
    let box_rect = egui::Rect::from_min_size(
        egui::pos2(card.left() + CELL_INNER_PAD, card.top() + CELL_INNER_PAD),
        egui::vec2(box_width, box_height),
    );

    // An archive door has no thumbnail to decode — its frame is a 1×1 transparent
    // sentinel (task #105), so the normal path below would draw an empty cell. Draw the
    // same artwork the door card uses, from the shell's one cached texture; the strip
    // never sees the ring, so this costs nothing but a draw call.
    if core.item_archive_kind(i).is_some() {
        if let Some((tex, w, h)) = door_art_texture(ui.ctx()) {
            draw_thumb_image(ui, box_rect, tex, w, h, 0);
        }
    } else if let Some(e) = core.thumbs.cache.get(i) {
        let key = (i, e.gen);
        keep.insert(key);
        let tex_id = state
            .textures
            .entry(key)
            .or_insert_with(|| {
                let img = egui::ColorImage::from_rgba_unmultiplied(
                    [e.w as usize, e.h as usize],
                    &e.payload.rgba,
                );
                ui.ctx().load_texture(
                    format!("pb_thumb_{}_{}", i, e.gen),
                    img,
                    egui::TextureOptions::LINEAR,
                )
            })
            .id();
        draw_thumb_image(
            ui,
            box_rect,
            tex_id,
            e.w,
            e.h,
            thumb_rotation_quarter(core, i),
        );
    } else if thumb_is_failed(core, i) {
        // Broken-image glyph (approximate: the images icon in a warning tone).
        pb_ui::icon::paint(
            ui,
            sq(
                box_rect.center().x,
                box_rect.center().y,
                THUMB_PLACEHOLDER_ICON,
            ),
            Icon::Images,
            Tone::Warning,
            p,
        );
    } else {
        // Not decoded yet: a faint placeholder glyph (correct behavior while cold, plan §3).
        pb_ui::icon::paint_tinted(
            ui,
            sq(
                box_rect.center().x,
                box_rect.center().y,
                THUMB_PLACEHOLDER_ICON,
            ),
            Icon::Images,
            panel_secondary(p).gamma_multiply(0.35),
        );
    }

    // Type badge (video / Live Photo / animation), bottom-left of the image box.
    let badge = thumb_badge_kind(core, i);
    if badge != 0 {
        let icon = match badge {
            1 => Icon::Play,
            2 => Icon::LivePhoto,
            _ => Icon::Film,
        };
        let r = 10.0;
        let c = egui::pos2(box_rect.left() + r + 3.0, box_rect.bottom() - r - 3.0);
        ui.painter()
            .circle_filled(c, r, Color32::from_black_alpha(115));
        pb_ui::icon::paint_tinted(ui, sq(c.x, c.y, THUMB_BADGE_ICON), icon, Color32::WHITE);
    }

    // Filename band: middle-truncated, centered under the image, dimmer unless current.
    let label_cy = card.top() + CELL_INNER_PAD + box_height + LABEL_GAP + LABEL_HEIGHT / 2.0;
    let color = if is_current {
        p.text
    } else {
        panel_secondary(p)
    };
    let font = FontId::new(THUMB_LABEL_SIZE, FontFamily::Proportional);
    let name = thumb_display_name(core, i);
    let g = middle_truncate(ui, &name, font, color, box_width);
    let gx = card.left() + CELL_INNER_PAD + (box_width - g.size().x) / 2.0;
    paint_vtext(ui, gx, label_cy, &g);
}

/// Draw a thumbnail texture aspect-fit inside `box_rect`, applying the session rotation
/// (`quarter` clockwise 90° turns) at draw. Fixed cells: the rotated image still letterboxes
/// inside the box (its fitting box swaps for odd quarters), so nothing reflows.
fn draw_thumb_image(
    ui: &egui::Ui,
    box_rect: egui::Rect,
    tex_id: egui::TextureId,
    w: u32,
    h: u32,
    quarter: u8,
) {
    let aspect = w.max(1) as f32 / h.max(1) as f32;
    // For an odd quarter turn the image is fit into a box with swapped extents first, so once
    // rotated it fills the real box (macOS: frame(swapped) → rotate → frame(box)).
    let (avail_w, avail_h) = if quarter % 2 == 1 {
        (box_rect.height(), box_rect.width())
    } else {
        (box_rect.width(), box_rect.height())
    };
    let (fw, fh) = fit_within(aspect, avail_w, avail_h);
    let unrot = egui::Rect::from_center_size(box_rect.center(), egui::vec2(fw, fh));
    if quarter == 0 {
        ui.painter().image(
            tex_id,
            unrot,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        let angle = quarter as f32 * std::f32::consts::FRAC_PI_2;
        egui::Image::new(egui::load::SizedTexture::new(tex_id, unrot.size()))
            .rotate(angle, egui::Vec2::splat(0.5))
            .paint_at(ui, unrot);
    }
}

/// Fit an `aspect` (w/h) rectangle within `max_w × max_h`, preserving aspect (letterbox).
fn fit_within(aspect: f32, max_w: f32, max_h: f32) -> (f32, f32) {
    let mut w = max_w;
    let mut h = w / aspect;
    if h > max_h {
        h = max_h;
        w = h * aspect;
    }
    (w, h)
}

/// A single-line galley for `name`, **middle**-truncated with an ellipsis to `max_w` (so a
/// file's extension survives, unlike egui's end-ellipsis). Names are short, so the shrink loop
/// is cheap; the overlay is retained (re-rendered on change, not per frame) regardless.
fn middle_truncate(
    ui: &egui::Ui,
    name: &str,
    font: FontId,
    color: Color32,
    max_w: f32,
) -> std::sync::Arc<egui::Galley> {
    let full = galley(ui, name, font.clone(), color, f32::INFINITY);
    if full.size().x <= max_w {
        return full;
    }
    let chars: Vec<char> = name.chars().collect();
    let n = chars.len();
    for keep in (2..n).rev() {
        let head = keep.div_ceil(2);
        let tail = keep / 2;
        let s: String = chars[..head]
            .iter()
            .chain(std::iter::once(&'…'))
            .chain(chars[n - tail..].iter())
            .collect();
        let cand = galley(ui, &s, font.clone(), color, f32::INFINITY);
        if cand.size().x <= max_w {
            return cand;
        }
    }
    galley(ui, "…", font, color, f32::INFINITY)
}

/// The cell's display filename — the basename of the item's name/path (mirrors the macOS FFI
/// `thumb_name`).
fn thumb_display_name(core: &AppCore, i: usize) -> String {
    let name = core.source.name(i);
    name.rsplit(['/', '\\']).next().unwrap_or(name).to_string()
}

/// Item-type badge: 0 none, 1 video, 2 Live Photo, 3 animated (mirrors the macOS FFI
/// `thumb_badge`). Live/animated appear once their lazily-filled caches know; video is always
/// known from the item kind.
fn thumb_badge_kind(core: &AppCore, i: usize) -> u8 {
    if matches!(
        pb_app_core::video::item_kind(core.source.as_ref(), i),
        pb_app_core::LibraryItemKind::Video(_)
    ) {
        return 1;
    }
    if matches!(core.live_motion_cache.get(&i), Some(Some(_))) {
        return 2;
    }
    if core
        .meta_cache
        .get(&i)
        .is_some_and(|m| m.animated.is_some())
    {
        return 3;
    }
    0
}

/// The item's session rotation override in clockwise quarter turns (0..=3) — applied at draw
/// (mirrors the macOS FFI `thumb_rotation`).
fn thumb_rotation_quarter(core: &AppCore, i: usize) -> u8 {
    match core.rotations.get(&i) {
        Some(pb_render::Rotation::R90) => 1,
        Some(pb_render::Rotation::R180) => 2,
        Some(pb_render::Rotation::R270) => 3,
        _ => 0,
    }
}

/// Whether the item's decode failed — the broken-image glyph, never a spinner (mirrors the
/// macOS FFI `thumb_failed`: display failures OR thumb-fill failures).
fn thumb_is_failed(core: &AppCore, i: usize) -> bool {
    core.failed.contains(&i) || core.thumbs.failed.contains(&i)
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
            fade: 1.0,
            align,
            progress: None,
        }
    }

    /// A `.zip`/`.7z` deck derives its tree into the core's v1 `folder_tree_panel`, not the
    /// disk-only `FsTree`. Regression for task #66: `PanelFrame::snapshot` must surface those
    /// rows as `TreeFrame::archive` — the winit skin read only `fs_tree_rows()` before, so an
    /// archive's "Folders" panel came up empty even though the tree was correctly derived.
    #[test]
    fn archive_deck_snapshots_folder_rows_not_empty() {
        use pb_app_core::folder_tree::TreeTarget;
        use pb_app_core::overlay::TreePanel;
        use pb_app_core::Viewport;
        use pb_hud::hud::TreeRow;
        use std::time::Instant;

        let mut core = AppCore::headless(Viewport {
            width: 1,
            height: 1,
            scale_factor: 1.0,
        });
        core.native_tree = true;
        core.folder_tree_open = true;
        // No displayed disk photo (an archive/empty deck): tree_is_fs() is false, so snapshot
        // must fall back to folder_tree_panel — the branch the winit skin was missing.
        assert!(core.tree_panel_visible(), "open + native → the panel shows");
        assert!(
            !core.tree_is_fs(),
            "an archive/empty deck is not the FsTree"
        );

        let mk = |depth, name: &str, current| TreeRow {
            depth,
            name: name.to_string(),
            open: false,
            current,
            marker: false,
            up: false,
            count: Some(1),
        };
        core.folder_tree_panel = Some(TreePanel {
            w: 0,
            h: 0,
            margin: 0,
            hits: Vec::new(),
            rows: vec![mk(0, "trip.7z", false), mk(1, "a", false), mk(1, "b", true)],
            targets: vec![
                Some(TreeTarget::Scope(String::new())),
                Some(TreeTarget::Scope("a".into())),
                Some(TreeTarget::Scope("b".into())),
            ],
            hovered: None,
            page: 0,
            built: Instant::now(),
        });

        let frame = PanelFrame::snapshot(&core);
        let tree = frame.tree.expect("the tree is visible");
        assert!(
            tree.rows.is_empty(),
            "an archive deck renders the archive rows, not the (empty) FsTree rows"
        );
        assert_eq!(
            tree.archive.len(),
            3,
            "all three folder rows surface (was 0 before the fix)"
        );
        assert_eq!(tree.archive[0].name, "trip.7z");
        assert_eq!(tree.archive[2].name, "b");
        assert!(tree.archive[2].current, "the current folder is marked");
        assert_eq!(
            tree.archive[1].index, 1,
            "the row index aligns with folder_tree_panel for tree_activate"
        );
        assert!(
            tree.archive.iter().all(|r| r.clickable),
            "every targeted row is clickable"
        );
    }

    /// The pure `folder_tree_panel` → [`ArchiveTreeRow`] mapping: a `…` collapse marker (no
    /// target) is inert; targeted rows are clickable and keep their panel index for activation.
    #[test]
    fn archive_tree_rows_marks_only_targeted_rows_clickable() {
        use pb_app_core::folder_tree::TreeTarget;
        use pb_app_core::overlay::TreePanel;
        use pb_hud::hud::TreeRow;
        use std::time::Instant;

        let row = |depth, name: &str, marker| TreeRow {
            depth,
            name: name.to_string(),
            open: false,
            current: false,
            marker,
            up: false,
            count: None,
        };
        let panel = TreePanel {
            w: 0,
            h: 0,
            margin: 0,
            hits: Vec::new(),
            rows: vec![
                row(0, "root", false),
                row(1, "\u{2026}", true),
                row(2, "deep", false),
            ],
            targets: vec![
                Some(TreeTarget::Scope(String::new())),
                None,
                Some(TreeTarget::Scope("deep".into())),
            ],
            hovered: None,
            page: 0,
            built: Instant::now(),
        };
        let rows = archive_tree_rows(&panel);
        assert_eq!(rows.len(), 3);
        assert!(
            rows[0].clickable,
            "the root row re-scopes to the whole archive"
        );
        assert!(
            rows[1].marker && !rows[1].clickable,
            "the … marker is inert"
        );
        assert!(rows[2].clickable);
        assert_eq!(rows[2].index, 2, "index tracks position for tree_activate");
    }

    /// Lay out just the info line in one egui frame at a fixed screen size.
    fn run_info(ctx: &egui::Context, screen: egui::Rect, line: &InfoLine) {
        let p = Palette::new(true);
        let raw = egui::RawInput {
            screen_rect: Some(screen),
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| info_line(ctx, &p, 255, line, &mut Vec::new()));
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

    /// **Regression: this deadlocked the app.** The door artwork's texture cache called
    /// `ctx.load_texture` from inside `ctx.data_mut`, which re-enters the Context's
    /// non-reentrant lock — so the event loop froze the instant the first door rendered
    /// (owner: "the app freezes and effectively crashes"). Every existing door test was
    /// core-side with no egui context, so nothing caught it.
    ///
    /// A revival hangs rather than fails, which is ugly but unmissable — and far better
    /// than shipping the freeze again.
    #[test]
    fn door_art_texture_does_not_deadlock_the_context() {
        let ctx = egui::Context::default();
        // First call: decodes + uploads. Second: must hit the cache, not re-upload.
        let a = door_art_texture(&ctx).expect("the artwork decodes and uploads");
        let b = door_art_texture(&ctx).expect("cached");
        assert_eq!(a.0, b.0, "one texture, cached in the ctx");
        // Identify it by what it is, not a size that moves with the asset or the crop.
        let art = pb_app_core::engine::door_artwork().expect("artwork decodes");
        assert_eq!((a.1, a.2), (art.width, art.height));
    }

    /// **Regression: the card centred itself using the *previous* door's width.**
    ///
    /// `sdf_panel`'s Window is auto-sized, and an anchored auto-sized Window is placed from
    /// the rect egui cached on its last run. Every other panel has a constant width so it
    /// settles once and never drifts; this card's width follows the filename, so each size
    /// change centred with the stale one — and since the retained overlay only rebuilds when
    /// the shell dirties it, egui's settle frame never arrived. A long name was off-centre,
    /// the next door was off-centre too, the third was fine (owner, 2026-07-17).
    ///
    /// Simulates that exactly: build a narrow card and a wide one **in the same context**,
    /// back to back, and require each to be centred on its own first frame. Against the old
    /// `CENTER_CENTER` anchoring the second assert fails, because the wide card is placed
    /// with the narrow one's width.
    #[test]
    fn a_door_card_centres_on_its_first_frame_at_any_width() {
        let ctx = egui::Context::default();
        pb_ui::install_fonts(&ctx);

        let card_rect = |name: &str| -> egui::Rect {
            let card = pb_app_core::app_core::DoorCard {
                name: name.to_string(),
                format: "ZIP Archive".into(),
                shortcut: "P".into(),
            };
            let mut actions = Vec::new();
            let _ = ctx.run(Default::default(), |ctx| {
                let p = Palette::new(true);
                door_card(ctx, &p, 242, ctx.screen_rect(), &card, &mut actions);
            });
            // The window's own rect. Deliberately **not** a shape's clip rect: the SDF
            // shadow paints into an expanded, deliberately off-centre clip, so reading that
            // reports a centred card as 62 px out — which cost me a while.
            ctx.memory(|m| m.area_rect(egui::Id::new("pb_door_card")))
                .expect("the card painted")
        };

        let screen_cx = ctx.screen_rect().center().x;
        let narrow = card_rect("a.zip");
        assert!(
            (narrow.center().x - screen_cx).abs() < 2.0,
            "narrow card off-centre by {}",
            narrow.center().x - screen_cx
        );

        // The size changes here. This is the frame that used to be wrong.
        let wide = card_rect("a-really-quite-unreasonably-long-archive-name-2019.tar.zst");
        assert!(
            wide.width() > narrow.width(),
            "the long name should widen the card ({} vs {})",
            wide.width(),
            narrow.width()
        );
        assert!(
            (wide.center().x - screen_cx).abs() < 2.0,
            "wide card off-centre by {} — placed with the previous card's width",
            wide.center().x - screen_cx
        );
    }
}
