//! PhotoBlaze macOS FFI bridge (NS1, ADR-021) — a [`swift-bridge`] staticlib that exposes
//! the platform-neutral [`pb_app_core::AppCore`] to a SwiftUI/AppKit host.
//!
//! **Shape** (mirrors the winit shell in `crates/pb-app/src/main.rs`, the worked reference):
//! - The Swift host owns the `NSWindow` + `MTKView`/`CAMetalLayer` and the run loop.
//! - **Events in:** the host translates `NSEvent` / gesture recognizers / menu clicks into
//!   calls on [`AppCoreHandle`] (`key_down` / `key_up` / `focus_lost` / `tick` / …), which
//!   build the shell-neutral [`pb_app_core::contract::CoreEvent`]s and call `AppCore::handle`.
//! - **Effects out:** the host pulls [`AppCoreHandle::next_effect`] **on the main actor**
//!   until `None` and executes each [`ffi::CoreEffectFfi`] (render, set title, wake, quit, …).
//!   A worker thread may only *schedule* a main-thread drain — never touch AppKit/render
//!   directly.
//!
//! **macOS-only.** The `swift-bridge` dependency and this whole module are target-gated, so
//! on Windows/Linux the crate compiles to an empty staticlib and the winit `pb-app` build is
//! untouched.
//!
//! **Slice 1 (this file)** proves the event→effect round-trip against the real
//! `AppCore::handle` on a *headless* core (no surface, no photos yet). A live `CAMetalLayer`
//! surface, a real photo source, and the remaining effects/events are layered on in the
//! following NS1 slices — see `.taskmaster/docs/macos-native-ui-plan.md` (§NS1).
#![cfg(target_os = "macos")]
// The `#[swift_bridge::bridge]` macro emits `extern "C"` shims with same-type pointer casts
// (`*mut AppCoreHandle` → `*mut AppCoreHandle`); that's generated glue we can't edit, so allow
// the lint crate-wide rather than fail `clippy -D warnings`.
#![allow(clippy::unnecessary_cast)]

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use pb_app_core::archive::ArchiveOpenError;
use pb_app_core::contract::{self, CoreEvent, Modifiers};
use pb_app_core::engine;
use pb_app_core::scan::{self, Resolved, ScanProgress, ScanUpdate};
use pb_app_core::{Action, AppCore, PbKey, Viewport};
use pb_core::open::{self, Cursor, LaunchInput, Source};
use pb_core::ResidentRing;
use pb_render::Renderer as _;

/// How long a folder walk runs before the Scanning progress dialog is revealed — a fast
/// scan (the overwhelmingly common case) never flashes a dialog. Mirrors the winit shell's
/// `SCAN_DIALOG_DELAY`.
const SCAN_DIALOG_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

/// An in-flight streaming folder walk — the mirror of the winit shell's `DirScan`
/// (worker thread + generation guard + the Scanning dialog's display name/reveal timer).
struct DirScan {
    generation: u64,
    rx: Receiver<(u64, ScanUpdate)>,
    progress: ScanProgress,
    /// The scan root's display name for the Scanning dialog headline.
    name: String,
    /// When the walk started — the Scanning dialog reveals after [`SCAN_DIALOG_DELAY`].
    started: Instant,
}

/// An in-flight background archive open — the mirror of the winit shell's `ArchiveLoad`.
struct ArchiveLoad {
    generation: u64,
    rx: Receiver<(u64, Result<Resolved, ArchiveOpenError>)>,
    /// The archive being opened — kept for the password prompt (a `PasswordRequired`
    /// failure re-opens this path with the entered password).
    path: PathBuf,
    was_password_attempt: bool,
    progress: pb_source::OpenProgress,
}

/// The opaque handle the Swift host holds — it owns the entire `AppCore` engine **plus the
/// shell's Rust half**: the scan/archive worker threads the `Begin*` effects spawn. Those
/// effects never cross to Swift — this crate handles them exactly like the winit shell
/// (`std::thread` + mpsc + generation guards), and `tick` polls the results back into
/// `AppCore::handle`. Swift only sees the genuinely native effects (title, dialogs,
/// clipboard, …).
pub struct AppCoreHandle {
    core: AppCore,
    dir_scan: Option<DirScan>,
    scan_gen: u64,
    archive_load: Option<ArchiveLoad>,
    archive_gen: u64,
    /// The payload of an in-flight `WriteClipboard` effect. Stashed here (the drain emits a
    /// bare marker) because a `Vec<u8>` inside a transparent-enum payload is exactly the
    /// shape swift-bridge mis-generates (gotcha #3); the host pulls it via the
    /// `clipboard_*`/`take_clipboard_*` accessors instead.
    pending_clipboard: Option<contract::ClipboardPayload>,
    /// The latest `SetMenuState` payload (the drain emits a `MenuStateChanged` marker; the
    /// host pulls the whole struct via `menu_state()` — same stash pattern as the clipboard).
    last_menu_state: contract::MenuState,
    /// What chrome dialog the host is currently showing — the mirror of the winit shell's
    /// `self.dialog.kind()` checks, so kind-guarded closes (a finished scan closes only a
    /// *Scanning* dialog) and the password retry-in-place work identically. Maintained at
    /// the `ShowDialog`/`CloseDialog` emit sites in `next_effect`. About isn't tracked (it's
    /// the standalone NSApp panel, not a dialog the flow manages).
    shown_dialog: Option<contract::DialogKind>,
    /// The text payload of the most recent `ShowDialog` (the confirm question, the password
    /// prompt, the loading/scanning headline) — the host pulls it via `dialog_message()`
    /// right after the marker (stash + pull, like the clipboard; gotcha #3).
    dialog_message: String,
    /// The inline error under the password field after a wrong attempt ("" = none) —
    /// pulled via `dialog_password_error()` with each `ShowDialog("password")`.
    password_error: String,
    /// The Shortcuts editor's draft keymap (NS2.6) — begun when the Settings window
    /// opens, edited via `keymap_capture`/`keymap_clear_slot`, applied live by
    /// `keymap_commit` after each edit gesture (auto-save), dropped when the window closes.
    keymap_draft: Option<pb_app_core::keymap::Keymap>,
    keymap_dirty: bool,
    /// The transient editor note ("Moved ⌘C from Copy Image") — pulled after a capture.
    keymap_note: String,
    /// Flattened Help-panel rows `(is_header, text, shortcut)` (task #54, mac-first):
    /// snapshotted by `help_refresh` on a `PanelsChanged` marker, then read by the
    /// indexed `help_row_*` accessors — the keymap-editor pull pattern, avoiding a
    /// `Vec<struct>` FFI return. The native SwiftUI Help view renders from it.
    help_snapshot: Vec<(bool, String, String)>,
    /// Flattened Inspector rows `(kind, a, b)` for the active tab (task #54): `kind` is
    /// 0 header, 1 label/value pair, 2 body paragraph, 3 status/muted; `a`/`b` are the
    /// text (pair = label/value, else `a` = text, `b` = ""). Snapshotted by
    /// `inspector_refresh` on a `PanelsChanged` marker, read by the indexed
    /// `inspector_row_*` accessors — same pull pattern as `help_snapshot`.
    inspector_snapshot: Vec<(u8, String, String)>,
    /// Flattened folder-tree rows (task #54): the Finder tree ([`FsTree`]) for disk decks,
    /// or the v1 `folder_tree_panel` for archive/empty decks — snapshotted by `tree_refresh`
    /// on a `PanelsChanged` marker so the indexed accessors + actions share one stable view.
    tree_snapshot: Vec<TreeRowFfi>,
    /// The CLI's positional paths (task #78), stashed by [`apply_launch_args`]
    /// (Self::apply_launch_args) and consumed exactly once by
    /// [`open_launch_paths`](Self::open_launch_paths) after the canvas exists — the
    /// stash-pull pattern (clipboard/dialog), so `Vec<String>` never crosses back to Swift.
    pending_launch_paths: Vec<String>,
}

/// One flattened folder-tree row for the native list. `path` is the disk path (Finder
/// tree) used by the toggle/open actions; empty for v1 archive rows, which act by index.
struct TreeRowFfi {
    name: String,
    depth: u32,
    is_current: bool,
    count: i64,
    has_children: bool,
    expanded: bool,
    loading: bool,
    is_up: bool,
    has_target: bool,
    path: std::path::PathBuf,
}

impl AppCoreHandle {
    /// Construct the real engine (decode pool, user settings + keymap, HUD compositor) at
    /// the given drawable size (`width`×`height` in physical pixels, `scale` = backing
    /// scale factor) — see [`AppCore::new_host`]. The deck starts empty; `open_path` /
    /// `attach_layer` bring photos + the surface.
    fn new(width: u32, height: u32, scale: f32) -> AppCoreHandle {
        let mut core = AppCore::new_host(Viewport {
            width,
            height,
            scale_factor: scale,
        });
        // This host presents the Help panel + the empty-state Open panel natively
        // (task #54, mac-first): the core suppresses their HUD rasterization and signals
        // via PanelsChanged. The tree and Inspector opt in with their own flags as their
        // native presenters land.
        core.native_help = true;
        core.native_open = true;
        core.native_inspector = true;
        core.native_tree = true;
        core.native_toast = true;
        core.native_info = true;
        core.native_play = true;
        AppCoreHandle {
            core,
            dir_scan: None,
            scan_gen: 0,
            archive_load: None,
            archive_gen: 0,
            pending_clipboard: None,
            last_menu_state: contract::MenuState::default(),
            shown_dialog: None,
            dialog_message: String::new(),
            password_error: String::new(),
            keymap_draft: None,
            keymap_dirty: false,
            keymap_note: String::new(),
            help_snapshot: Vec::new(),
            inspector_snapshot: Vec::new(),
            tree_snapshot: Vec::new(),
            pending_launch_paths: Vec::new(),
        }
    }

    /// Snapshot the Help-panel model into `help_snapshot` for the indexed accessors —
    /// call on a `PanelsChanged` marker before reading `help_row_*`. Flattens the core's
    /// sections into header + shortcut rows.
    fn help_refresh(&mut self) {
        self.help_snapshot = self
            .core
            .help_panel()
            .sections
            .into_iter()
            .flat_map(|s| {
                std::iter::once((true, s.title, String::new()))
                    .chain(s.rows.into_iter().map(|(desc, sc)| (false, desc, sc)))
            })
            .collect();
    }

    /// Whether the native SwiftUI Help view should be shown (Help open, not Tab-hidden).
    fn help_visible(&self) -> bool {
        self.core.help_panel_visible()
    }

    /// Whether the native empty-state Open panel should be shown (no photos loaded).
    fn open_panel_visible(&self) -> bool {
        self.core.open_panel_visible()
    }

    /// The user-facing shortcut label for an action by its stable id (`"open_file"`,
    /// `"next"`, `"help"`, …) — the mac-symbol form (e.g. "⇧ O"), or "" if the id is
    /// unknown or the action is unbound. A generic primitive so native panels can show
    /// any binding (the empty-state welcome tips, and future surfaces) without a
    /// bespoke accessor each.
    fn action_shortcut(&self, id: &str) -> String {
        Action::from_id(id)
            .map(|a| self.core.help_shortcut(a))
            .unwrap_or_default()
    }

    fn help_row_count(&self) -> usize {
        self.help_snapshot.len()
    }

    /// A Help row is either a section header (`is_header`, `text` = section title) or a
    /// command row (`text` = description, `shortcut` = the key label). Out-of-range → a
    /// blank non-header row (defensive, like the keymap accessors).
    fn help_row_is_header(&self, i: usize) -> bool {
        self.help_snapshot.get(i).map(|r| r.0).unwrap_or(false)
    }

    fn help_row_text(&self, i: usize) -> String {
        self.help_snapshot
            .get(i)
            .map(|r| r.1.clone())
            .unwrap_or_default()
    }

    fn help_row_shortcut(&self, i: usize) -> String {
        self.help_snapshot
            .get(i)
            .map(|r| r.2.clone())
            .unwrap_or_default()
    }

    // ---- Inspector (Details / Text / Describe tabs) — the tabbed content panel (task
    // #54). Same pull pattern as Help: `PanelsChanged` → `inspector_refresh` → indexed
    // `inspector_row_*`. `inspector_visible` / `inspector_tab` drive the SwiftUI panel's
    // presence + selected segment; `inspector_show_tab` / `inspector_close` are the
    // tab-bar / ✕ actions.

    /// Whether the native SwiftUI Inspector should be shown (open on some tab, not hidden).
    fn inspector_visible(&self) -> bool {
        self.core.inspector_panel_visible()
    }

    /// The active tab: 0 = Details, 1 = Text, 2 = Describe (Details when closed).
    fn inspector_tab(&self) -> u8 {
        match self.core.panels.inspector {
            Some(pb_app_core::overlay::InspectorTab::Text) => 1,
            Some(pb_app_core::overlay::InspectorTab::Describe) => 2,
            _ => 0,
        }
    }

    /// Switch the Inspector to a tab (tab-bar click) — *opens* it there, never toggles
    /// closed (unlike the T / D / ⇧I keys), so clicking the active tab is a no-op. The
    /// next tick kicks that tab's scan and signals the host.
    fn inspector_show_tab(&mut self, tab: u8) {
        self.core.now = Instant::now();
        let t = match tab {
            1 => pb_app_core::overlay::InspectorTab::Text,
            2 => pb_app_core::overlay::InspectorTab::Describe,
            _ => pb_app_core::overlay::InspectorTab::Details,
        };
        self.core.panels.open_inspector(t);
    }

    /// Close the Inspector (its ✕ button). The tick's visibility diff signals the host.
    fn inspector_close(&mut self) {
        self.core.panels.inspector = None;
    }

    /// Snapshot the active tab's content into `inspector_snapshot` for the indexed
    /// accessors — call on a `PanelsChanged` marker before reading `inspector_row_*`.
    fn inspector_refresh(&mut self) {
        use pb_app_core::overlay::InspectorTab;
        use pb_app_core::panels::{DescribeBody, DetailRow, TextBody};
        let mut rows: Vec<(u8, String, String)> = Vec::new();
        match self.core.panels.inspector {
            // Text / Describe don't repeat a title row — the tab bar already labels them;
            // Details leads with the filename (its first Span).
            Some(InspectorTab::Text) => match self.core.text_panel().body {
                TextBody::NoPhoto => {}
                TextBody::Scanning => rows.push((3, "Reading text…".into(), String::new())),
                TextBody::Ready {
                    qr,
                    paragraphs,
                    ocr_error,
                } => {
                    for q in &qr {
                        rows.push((2, format!("QR code → {q}"), String::new()));
                    }
                    for p in &paragraphs {
                        rows.push((2, p.clone(), String::new()));
                    }
                    if paragraphs.is_empty() {
                        if let Some(e) = ocr_error {
                            rows.push((3, e, String::new()));
                        } else if qr.is_empty() {
                            rows.push((3, "No text found".into(), String::new()));
                        }
                    }
                }
            },
            Some(InspectorTab::Describe) => {
                rows.push((0, "Description".into(), String::new()));
                match self.core.describe_panel().body {
                    DescribeBody::NoPhoto => {}
                    DescribeBody::Idle => {
                        rows.push((3, "Press D to describe this image.".into(), String::new()))
                    }
                    DescribeBody::Busy => rows.push((3, "Describing…".into(), String::new())),
                    DescribeBody::Ready(text) => rows.push((2, text, String::new())),
                    DescribeBody::Error(msg) => rows.push((3, msg, String::new())),
                }
            }
            // Details (and the closed fallback, never read while closed).
            _ => {
                for r in self.core.details_panel().rows {
                    match r {
                        // A bold span is a section header (kind 0); a non-bold span is a
                        // sub-header — the folder path under the filename (kind 4), rendered
                        // regular-weight and tucked directly beneath its header.
                        DetailRow::Span { text, bold } => {
                            rows.push((if bold { 0 } else { 4 }, text, String::new()))
                        }
                        DetailRow::Pair { label, value } => rows.push((1, label, value)),
                    }
                }
            }
        }
        self.inspector_snapshot = rows;
    }

    fn inspector_row_count(&self) -> usize {
        self.inspector_snapshot.len()
    }

    /// Row kind: 0 header, 1 label/value pair, 2 body paragraph, 3 status/muted,
    /// 4 sub-header (regular-weight span under a header, e.g. the folder path).
    fn inspector_row_kind(&self, i: usize) -> u8 {
        self.inspector_snapshot.get(i).map(|r| r.0).unwrap_or(0)
    }

    /// Primary text: pair label, else the row's text.
    fn inspector_row_a(&self, i: usize) -> String {
        self.inspector_snapshot
            .get(i)
            .map(|r| r.1.clone())
            .unwrap_or_default()
    }

    /// Secondary text: pair value (empty for non-pairs).
    fn inspector_row_b(&self, i: usize) -> String {
        self.inspector_snapshot
            .get(i)
            .map(|r| r.2.clone())
            .unwrap_or_default()
    }

    // ---- Folder tree (⇧F, task #54) — the Finder-style resident browser (`FsTree`) for
    // disk decks, falling back to the v1 `folder_tree_panel` for archive/empty decks.
    // `tree_refresh` snapshots whichever is active into `tree_snapshot`; the indexed
    // `tree_row_*` accessors read it, and the toggle/open/extend actions resolve through
    // it. `tree_uses_fs` tells the host which interaction to render (chevrons vs flat).

    /// Whether the native SwiftUI folder tree should be shown (open, not Tab-hidden).
    fn tree_visible(&self) -> bool {
        self.core.tree_panel_visible()
    }

    /// Whether the Finder tree (chevron expand/collapse, name-to-open) is active — else
    /// the flat v1 archive tree (click-to-activate) is.
    fn tree_uses_fs(&self) -> bool {
        self.core.tree_is_fs()
    }

    /// Snapshot the active tree's rows — call on a `PanelsChanged` marker before reading.
    fn tree_refresh(&mut self) {
        let mut rows: Vec<TreeRowFfi> = Vec::new();
        if self.core.tree_is_fs() {
            // A leading "up to <parent>" row (climbs a level on click), when not at the
            // filesystem root — the in-list affordance replacing a header button. It sits
            // at depth 0 (a level *above* the root), so the root + its subtree shift one
            // indent right, reflecting the hierarchy.
            let parent = self.core.fs_tree_parent_name();
            let shift = if parent.is_some() { 1 } else { 0 };
            if let Some(parent) = parent {
                rows.push(TreeRowFfi {
                    name: parent,
                    depth: 0,
                    is_current: false,
                    count: -1,
                    has_children: false,
                    expanded: false,
                    loading: false,
                    is_up: true,
                    has_target: true,
                    path: std::path::PathBuf::new(),
                });
            }
            for r in self.core.fs_tree_rows() {
                rows.push(TreeRowFfi {
                    name: r.name,
                    depth: r.depth + shift,
                    is_current: r.is_current,
                    count: r.count.map(|c| c as i64).unwrap_or(-1),
                    has_children: r.has_children,
                    expanded: r.expanded,
                    loading: r.loading,
                    is_up: false,
                    has_target: true,
                    path: r.path,
                });
            }
        } else if let Some(p) = self.core.folder_tree_panel.as_ref() {
            for (i, row) in p.rows.iter().enumerate() {
                rows.push(TreeRowFfi {
                    name: row.name.clone(),
                    depth: row.depth,
                    is_current: row.current,
                    count: row.count.map(|c| c as i64).unwrap_or(-1),
                    has_children: false,
                    expanded: false,
                    loading: false,
                    is_up: row.up,
                    has_target: p.targets.get(i).map(|t| t.is_some()).unwrap_or(false),
                    path: std::path::PathBuf::new(),
                });
            }
        }
        self.tree_snapshot = rows;
    }

    fn tree_row_count(&self) -> usize {
        self.tree_snapshot.len()
    }

    /// A row's indent depth in the hierarchy (0 = root).
    fn tree_row_depth(&self, i: usize) -> u32 {
        self.tree_snapshot.get(i).map(|r| r.depth).unwrap_or(0)
    }

    fn tree_row_name(&self, i: usize) -> String {
        self.tree_snapshot
            .get(i)
            .map(|r| r.name.clone())
            .unwrap_or_default()
    }

    /// The current folder ("you are here") — highlighted.
    fn tree_row_is_current(&self, i: usize) -> bool {
        self.tree_snapshot
            .get(i)
            .map(|r| r.is_current)
            .unwrap_or(false)
    }

    /// The v1 "up to parent" affordance row (archive tree only).
    fn tree_row_is_up(&self, i: usize) -> bool {
        self.tree_snapshot.get(i).map(|r| r.is_up).unwrap_or(false)
    }

    /// Worth a disclosure chevron (Finder tree: has or may have subfolders).
    fn tree_row_has_children(&self, i: usize) -> bool {
        self.tree_snapshot
            .get(i)
            .map(|r| r.has_children)
            .unwrap_or(false)
    }

    /// Expanded right now (Finder tree).
    fn tree_row_expanded(&self, i: usize) -> bool {
        self.tree_snapshot
            .get(i)
            .map(|r| r.expanded)
            .unwrap_or(false)
    }

    /// Expanded but its children are still being read (show a spinner).
    fn tree_row_loading(&self, i: usize) -> bool {
        self.tree_snapshot
            .get(i)
            .map(|r| r.loading)
            .unwrap_or(false)
    }

    /// The photo-count badge, or -1 when the row has none.
    fn tree_row_count_badge(&self, i: usize) -> i64 {
        self.tree_snapshot.get(i).map(|r| r.count).unwrap_or(-1)
    }

    /// Whether the row is clickable / openable.
    fn tree_row_has_target(&self, i: usize) -> bool {
        self.tree_snapshot
            .get(i)
            .map(|r| r.has_target)
            .unwrap_or(false)
    }

    /// The current photo's folder path — the host keys "scroll the current row into view"
    /// off changes to this, so it fires only when the folder actually changes (not on an
    /// unrelated expand/collapse). Empty when nothing is loaded.
    fn tree_current_path(&self) -> String {
        self.core
            .current_folder_abs()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Activate a row (a name click): the "up" row climbs a level; a Finder folder opens
    /// (loads its photos); a v1 archive row re-scopes/opens.
    fn tree_activate(&mut self, i: usize) {
        self.core.now = Instant::now();
        let Some(row) = self.tree_snapshot.get(i) else {
            return;
        };
        if self.core.tree_is_fs() {
            if row.is_up {
                self.core.fs_tree_extend_up();
            } else {
                let path = row.path.clone();
                self.core.fs_tree_open(path);
            }
        } else {
            self.core.tree_activate(i);
        }
    }

    /// Toggle a row's expansion (the chevron) — Finder tree only; browsing, no load.
    fn tree_toggle(&mut self, i: usize) {
        self.core.now = Instant::now();
        if self.core.tree_is_fs() {
            if let Some(row) = self.tree_snapshot.get(i) {
                let path = row.path.clone();
                self.core.fs_tree_toggle(&path);
            }
        }
    }

    /// A native menu item fired, by stable [`Action`] id (`"open_file"`, `"rotate_cw"`, …)
    /// — the same dispatch path as the keyboard. Unknown ids are ignored.
    fn menu_action(&mut self, id: &str) {
        self.core.now = Instant::now();
        if let Some(action) = Action::from_id(id) {
            self.core.handle(CoreEvent::MenuAction(action));
        }
    }

    /// A toolbar nav/random button was pressed and **held** → begin hold-to-fly for its
    /// action (task #55). The Swift host kicks the pump + drains afterward, exactly like a
    /// keypress, so the self-paced advance runs for as long as the button is held.
    fn begin_pointer_nav(&mut self, action_id: &str) {
        self.core.now = Instant::now();
        if let Some(action) = Action::from_id(action_id) {
            self.core.begin_pointer_nav(action);
        }
    }

    /// The held toolbar nav/random button was released → stop flying.
    fn end_pointer_nav(&mut self) {
        self.core.end_pointer_nav();
    }

    /// Right-click over the photo: the core decides the context-menu item set from live
    /// state and answers with `ShowContextMenu` (task #41); the host pops the NSMenu.
    fn context_menu(&mut self) {
        self.core.now = Instant::now();
        self.core.show_context_menu();
    }

    /// Open a single file / folder / archive path — the `--pb-open` dev argument, or a
    /// one-item drop. See [`open_paths`](Self::open_paths).
    fn open_path(&mut self, path: &str) {
        self.open_paths(vec![path.to_string()]);
    }

    /// Parse the launch command line (task #78) and apply it: session-only overrides land
    /// on the core immediately (`AppCore::apply_launch_overrides` — safe pre-window; later
    /// reads like `startup_fullscreen` / `effective_appearance` consume them), and the
    /// positional paths are stashed for [`open_launch_paths`](Self::open_launch_paths).
    ///
    /// `argv` is the **full** `ProcessInfo.processInfo.arguments` — argv[0] included; clap
    /// consumes the first element as the program name (dropping it would eat the first real
    /// flag or path). `version` is the bundle's version string (`CFBundleShortVersionString`
    /// + build id — this crate's own `CARGO_PKG_VERSION` is meaningless here).
    ///
    /// A parse error is a no-op: [`cli_preflight`] already gated help/version/usage errors
    /// before the engine was built, so an `Err` here means the host skipped the preflight —
    /// the launch degrades to "no CLI", never a crash.
    fn apply_launch_args(&mut self, argv: Vec<String>, version: String) {
        let Ok(cli) = pb_cli::parse_from(argv, &version) else {
            return;
        };
        self.core.apply_launch_overrides(&cli.to_overrides());
        self.pending_launch_paths = cli
            .launch_paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
    }

    /// Open the paths stashed by [`apply_launch_args`](Self::apply_launch_args) — called by
    /// the host once the canvas exists (the winit shell defers its launch open into
    /// `resumed()` the same way). Consumed exactly once: a second call returns `false` and
    /// opens nothing (the idempotence guard the double-delivery arbitration leans on).
    ///
    /// A directory launch honors `--recursive` / `--no-recursive`, else the saved
    /// preference — the winit shell's exact startup logic (its `main()` mutates
    /// `Source::Scan.recursive` the same way). Non-launch opens (drop, panel, Finder)
    /// keep `open::plan`'s own policy, matching Windows.
    fn open_launch_paths(&mut self) -> bool {
        if self.pending_launch_paths.is_empty() {
            return false;
        }
        let paths = std::mem::take(&mut self.pending_launch_paths);
        let recursive = self
            .core
            .launch
            .recursive
            .unwrap_or(self.core.settings.recursive);
        self.open_paths_inner(paths, Some(recursive));
        true
    }

    /// Open launch/drop/panel paths — the winit shell's `classify_inputs` mirrored (ADR-019):
    /// a lone directory scans recursively, a lone `.zip`/`.7z` opens to its contents, files
    /// scan/list per the launch policy (a single file → its folder flat, cursor on it). Routed
    /// through the core's `open_plan`, whose `Begin*` effects this crate executes on its
    /// worker threads. Empty / all-empty input is ignored (never blanks the current photo).
    fn open_paths(&mut self, paths: Vec<String>) {
        self.open_paths_inner(paths, None);
    }

    /// The shared open-plan body: `recursive_override = Some(r)` forces a directory scan's
    /// recursion (the CLI launch, where a flag or the saved preference decides — see
    /// [`open_launch_paths`](Self::open_launch_paths)); `None` keeps `open::plan`'s own
    /// policy (every non-launch entry point).
    fn open_paths_inner(&mut self, paths: Vec<String>, recursive_override: Option<bool>) {
        self.core.now = Instant::now();
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect();
        if paths.is_empty() {
            return;
        }
        let input = if paths.len() == 1 && paths[0].is_dir() {
            LaunchInput::Directory(paths.into_iter().next().expect("len == 1"))
        } else if paths.len() == 1 && is_archive(&paths[0]) {
            LaunchInput::Archive(paths.into_iter().next().expect("len == 1"))
        } else {
            // One or more files (a directory inside a multi-selection is uncommon and is
            // ignored). If somehow every path is a directory, open the first.
            let files: Vec<PathBuf> = paths.iter().filter(|p| !p.is_dir()).cloned().collect();
            if files.is_empty() {
                LaunchInput::Directory(paths.into_iter().next().expect("non-empty"))
            } else {
                LaunchInput::Files(files)
            }
        };
        let mut plan = open::plan(input);
        if let (Some(r), Source::Scan { recursive, .. }) = (recursive_override, &mut plan.source) {
            *recursive = r;
        }
        self.core.open_plan(plan.source, plan.cursor);
    }

    /// The pointer moved (physical px, top-left origin — the winit `CursorMoved` convention;
    /// the Swift view flips AppKit's bottom-left origin and scales by the backing factor).
    /// Anchors pinch/wheel zoom, drives drag-to-pan while the left button is down, and
    /// un-hides the cursor.
    fn pointer_moved(&mut self, x: f32, y: f32) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::PointerMoved { x, y });
    }

    /// Left mouse button pressed/released — the winit shell's `MouseInput(Left)` arm
    /// mirrored: a press on a folder-tree row fires that control (the open-panel / play-hint /
    /// scan-pill affordances are native SwiftUI views and handle their own clicks); anywhere
    /// else it toggles drag-to-pan.
    fn mouse_left(&mut self, pressed: bool) {
        self.core.now = Instant::now();
        if pressed && self.core.folder_tree_click() {
            // A folder-tree row opened a folder / a "… n more" marker paged; the
            // open plan's Begin* effects drain below like any other press.
        } else {
            self.core.dragging = pressed;
            self.core.refresh_cursor();
        }
    }

    /// Line-precise scroll (a mouse wheel notch) — pan or zoom per the Scroll-wheel setting.
    fn scroll_lines(&mut self, x: f32, y: f32) {
        self.core.now = Instant::now();
        self.core
            .handle(CoreEvent::Scroll(contract::ScrollDelta::Lines { x, y }));
    }

    /// Pixel-precise scroll (a trackpad two-finger swipe), in **physical px** (the winit
    /// convention — the Swift view scales AppKit's points by the backing factor).
    fn scroll_pixels(&mut self, x: f32, y: f32) {
        self.core.now = Instant::now();
        self.core
            .handle(CoreEvent::Scroll(contract::ScrollDelta::Pixels { x, y }));
    }

    /// Trackpad pinch: `delta` is the incremental magnification (`NSEvent.magnification`,
    /// + spread to zoom in, − pinch to zoom out) — zooms about the cursor.
    fn pinch(&mut self, delta: f32) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::Pinch { delta });
    }

    /// Trackpad two-finger double-tap ("smart magnify"): toggle 100% ↔ fit, sharing the
    /// keyboard's `0` toggle so they can't drift.
    fn double_tap(&mut self) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::DoubleTap);
    }

    /// Whether engine work is still outstanding (prefetch/decode/scan/archive in flight) —
    /// the host keeps the display link running while true, pauses when idle (the frame
    /// pump's continuous-vs-on-demand decision, NS1 item 7).
    fn work_pending(&self) -> bool {
        self.core.work_pending() || self.dir_scan.is_some() || self.archive_load.is_some()
    }

    /// The displayed photo's on-disk path ("" for an archive entry or the empty deck) —
    /// the host's title-bar **proxy icon** source (`NSWindow.representedURL`, macOS port
    /// task #12). Pulled on each `SetTitle` (that effect fires exactly when the displayed
    /// item changes, so the refresh stays off the hold-to-fly hot path). RAM-only, never
    /// `noteNewRecentDocumentURL:` (no Recents → privacy #2 holds).
    fn current_photo_path(&self) -> String {
        self.core
            .displayed_item
            .and_then(|i| self.core.source.path(i))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Show a HUD toast — the host's feedback line after it executes a platform op
    /// (clipboard written, etc.), mirroring the winit shell's post-write toasts.
    fn toast(&mut self, msg: &str) {
        self.core.show_toast(msg);
    }

    // ---- The WriteClipboard payload accessors (see `pending_clipboard`). The host, on
    // the `WriteClipboard` marker: try `take_clipboard_text()` first; if empty, read the
    // image dimensions/file and `take_clipboard_image()`.

    /// The pending text payload's custom toast ("" = none; the host derives its
    /// default). Read BEFORE `take_clipboard_text` — taking the payload consumes
    /// both. Set by the recognized-text copy (task #45, "Copied 214 characters" /
    /// "Copied text + 1 QR code"); the Swift host adopts it when the mac backend
    /// lands (until then its generic toast stands).
    fn clipboard_text_toast(&self) -> String {
        match &self.pending_clipboard {
            Some(contract::ClipboardPayload::Text { toast: Some(t), .. }) => t.clone(),
            _ => String::new(),
        }
    }

    /// The pending text payload ("" if none / it's an image). Consumes it.
    fn take_clipboard_text(&mut self) -> String {
        match self.pending_clipboard.take() {
            Some(contract::ClipboardPayload::Text { text, .. }) => text,
            other => {
                self.pending_clipboard = other; // put a non-text payload back
                String::new()
            }
        }
    }

    /// Pending image width in px (0 = no image payload).
    fn clipboard_image_width(&self) -> u32 {
        match &self.pending_clipboard {
            Some(contract::ClipboardPayload::Image { w, .. }) => *w,
            _ => 0,
        }
    }

    /// Pending image height in px (0 = no image payload).
    fn clipboard_image_height(&self) -> u32 {
        match &self.pending_clipboard {
            Some(contract::ClipboardPayload::Image { h, .. }) => *h,
            _ => 0,
        }
    }

    /// The on-disk file to offer alongside the pixels ("" for an archive entry).
    fn clipboard_image_file(&self) -> String {
        match &self.pending_clipboard {
            Some(contract::ClipboardPayload::Image { file: Some(p), .. }) => {
                p.to_string_lossy().into_owned()
            }
            _ => String::new(),
        }
    }

    /// The pending image's RGBA8 pixels (`w*h*4`; empty if none). Consumes the payload.
    fn take_clipboard_image(&mut self) -> Vec<u8> {
        match self.pending_clipboard.take() {
            Some(contract::ClipboardPayload::Image { rgba, .. }) => rgba,
            other => {
                self.pending_clipboard = other;
                Vec::new()
            }
        }
    }

    /// The latest menu check/enabled state (pull after a `MenuStateChanged` marker).
    /// The live slideshow interval, formatted (`4s`, `0.5s`) — the macOS toolbar's
    /// slideshow control shows it (task #55). Read on each `syncToolbar`.
    fn slideshow_interval_display(&self) -> String {
        self.core.slideshow_interval_display()
    }

    /// Spike (task #59): height (physical px) of the translucent glass toolbar the surface
    /// extends under, so the photo fits below it and zoom/fill overflow shows under the glass.
    /// `0` = classic opaque bar. Set on attach + resize by the glass-toolbar mode.
    fn set_content_top_inset(&mut self, px: u32) {
        self.core.set_content_top_inset(px);
    }

    /// Whether the current item has motion (dims the toolbar's Play-Animation button) and
    /// whether it's actively playing (lights it). Both read on each `syncToolbar` (task #55).
    fn current_has_motion(&mut self) -> bool {
        self.core.current_has_motion()
    }

    fn animation_playing(&self) -> bool {
        self.core.animation_playing()
    }

    /// Any motion playing — animation/Live Photo OR a video (task 79.9). The toolbar
    /// Play/Pause glyph reads this so it reflects a playing video, not just an animation.
    fn motion_playing(&self) -> bool {
        self.core.motion_playing()
    }

    fn menu_state(&self) -> ffi::MenuStateFfi {
        let s = &self.last_menu_state;
        ffi::MenuStateFfi {
            scale: match s.scale {
                contract::ScaleMode::Fit => 0,
                contract::ScaleMode::Fill => 1,
                contract::ScaleMode::Original => 2,
            },
            info_basic: s.info_basic,
            info_full: s.info_full,
            panels_hidden: s.panels_hidden,
            hide_panels_enabled: s.hide_panels_enabled,
            recursive: s.recursive,
            fullscreen: s.fullscreen,
            slideshow: s.slideshow,
            mute_live_audio: s.mute_live_audio,
            compare_pin_enabled: s.compare_pin_enabled,
            compare_pinned_here: s.compare_pinned_here,
            compare_toggle_enabled: s.compare_toggle_enabled,
            save_rotation_enabled: s.save_rotation_enabled,
            reveal_enabled: s.reveal_enabled,
            cancel_scan_enabled: s.cancel_scan_enabled,
            undo_enabled: s.undo.is_some(),
            undo_label: s.undo.unwrap_or("Undo").to_string(),
        }
    }

    // ---- Startup window state + geometry persistence (finalize item 2): the core owns
    // the debounced save (`geometry_save_at` → tick 4e flushes `settings.save()`); the
    // host captures/restores real frames, in winit's stored convention (PHYSICAL px,
    // top-left virtual-desktop origin — the same settings.toml the egui build writes).

    /// Resolve the startup window mode from settings (`StartupMode` + the remembered
    /// last mode) — call once right after attach. `true` = enter the borderless speed
    /// mode; the core's `windowed` mirror is set here WITHOUT re-saving settings (this
    /// restores state, unlike the F toggle which changes it). A `--windowed` /
    /// `--fullscreen` launch override wins over the saved preference (task #78 — the
    /// winit shell resolves `overrides.windowed` the same way, pre-window); requires
    /// `apply_launch_args` to have run first (it has: the host preflights before attach).
    fn startup_fullscreen(&mut self) -> bool {
        let fs = match self.core.launch.windowed {
            Some(windowed) => !windowed,
            None => self.core.settings.start_fullscreen(),
        };
        self.core.windowed = !fs;
        fs
    }

    /// The appearance the app should ACTUALLY wear right now — the saved preference
    /// unless a `--theme` launch override is live (task #78). Same 0 system / 1 light /
    /// 2 dark encoding as `SettingsFormFfi::appearance_mode`. The host's
    /// `applyAppearancePreference` reads THIS; `settings_form()` stays the raw saved
    /// value (it edits the preference, and must not show a session override as saved).
    fn effective_appearance(&self) -> u8 {
        match self.core.effective_appearance() {
            pb_app_core::settings::AppearanceMode::System => 0,
            pb_app_core::settings::AppearanceMode::Light => 1,
            pb_app_core::settings::AppearanceMode::Dark => 2,
        }
    }

    /// The saved windowed geometry (`present == false` when none was ever saved).
    fn saved_geometry(&self) -> ffi::WindowGeometryFfi {
        match self.core.settings.window {
            Some(g) => ffi::WindowGeometryFfi {
                present: true,
                x: g.x,
                y: g.y,
                w: g.w,
                h: g.h,
            },
            None => ffi::WindowGeometryFfi {
                present: false,
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
        }
    }

    /// The host's Moved/Resized tracker — winit's `track_windowed_geometry` mirrored:
    /// refresh the remembered geometry and arm the debounced save (the core tick
    /// flushes it once the user stops; a drag isn't a write storm). No-op in
    /// fullscreen — that geometry is the monitor, not a user-chosen spot.
    fn note_window_geometry(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if !self.core.windowed {
            return;
        }
        let g = pb_app_core::settings::WindowGeometry { x, y, w, h };
        if self.core.settings.window != Some(g) {
            self.core.settings.window = Some(g);
            self.core.geometry_save_at =
                Some(Instant::now() + std::time::Duration::from_millis(500));
        }
    }

    // ---- The NS2 dialog seam: `ShowDialog`/`CloseDialog`/`SetDialogChecking` effects out
    // (with the text payload stashed — see `dialog_message`), `DialogResolved` results in.
    // Each entry point maps one user gesture in a native dialog to the shell-neutral
    // `contract::DialogResult` and lets `AppCore::handle_dialog_resolved` own the reaction
    // (close, cancel workers, run the delete, re-open the archive) — the same seam the
    // winit shell's `route_dialog_outcome` feeds.

    /// The text payload of the most recent `ShowDialog` (pull right after the marker).
    fn dialog_message(&self) -> String {
        self.dialog_message.clone()
    }

    /// The inline password-field error ("" = none) — set on a wrong-password retry.
    fn dialog_password_error(&self) -> String {
        self.password_error.clone()
    }

    /// Esc / the close button dismissed the current dialog (whatever kind is up). The core
    /// arms the esc-guard, cancels a matching in-flight op, and closes.
    fn dialog_dismissed(&mut self) {
        self.core.now = Instant::now();
        let kind = self.shown_dialog;
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::Dismissed(kind),
        ));
    }

    /// A Message (or any other single-button) dialog's OK.
    fn dialog_closed(&mut self) {
        self.core.now = Instant::now();
        self.core
            .handle(CoreEvent::DialogResolved(contract::DialogResult::Closed));
    }

    /// The delete Confirm dialog answered (`true` = Delete). The core runs the permanent
    /// delete on the armed `pending_confirm_delete` item.
    fn dialog_confirm_answered(&mut self, confirmed: bool) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::ConfirmAnswered(confirmed),
        ));
    }

    /// The password prompt submitted an entry: the core shows "Checking…" and re-opens the
    /// pending archive with it (`BeginArchiveOpen` — intercepted onto this crate's worker).
    fn password_submitted(&mut self, password: String) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::PasswordSubmitted(Some(password)),
        ));
    }

    /// The "Ask about image" dialog submitted a question (task #44): the core runs it through
    /// the describe backend for the current photo and shows the answer in the description panel.
    fn ask_submitted(&mut self, question: String) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::AskSubmitted(question),
        ));
    }

    /// The password prompt's Cancel — abandon the pending archive.
    fn password_cancelled(&mut self) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::PasswordCancelled,
        ));
    }

    /// The archive "Opening…" dialog's Cancel.
    fn loading_cancelled(&mut self) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::LoadingCancelled,
        ));
    }

    /// The folder "Scanning…" dialog's Cancel (stops the walk, discards the partial result).
    fn scanning_cancelled(&mut self) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::ScanningCancelled,
        ));
    }

    // ── The ambient scan pill (④): a non-blocking, top-center native progress element that
    // replaces the modal Scanning dialog AND the in-canvas HUD chip on this host. The host
    // polls these each pump while a scan is in flight; they read the Rust-side worker handle
    // directly (no core state), and cover both the pre- and post-bootstrap phases. ──

    /// Whether the ambient scan pill should show: a walk is in flight and has outlasted the
    /// reveal delay (a fast folder never flashes a pill).
    fn scan_pill_visible(&self) -> bool {
        self.dir_scan
            .as_ref()
            .is_some_and(|s| s.started.elapsed() >= SCAN_DIALOG_DELAY)
    }

    /// The scanned folder's display name (the pill's headline).
    fn scan_pill_name(&self) -> String {
        self.dir_scan
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_default()
    }

    /// Images found so far (the pill's live count).
    fn scan_pill_found(&self) -> i64 {
        self.dir_scan
            .as_ref()
            .map(|s| s.progress.found() as i64)
            .unwrap_or(0)
    }

    /// The sub-folder currently being walked (blank while it's still the root, which would
    /// just duplicate the headline).
    fn scan_pill_current(&self) -> String {
        match self.dir_scan.as_ref() {
            Some(s) => {
                let cur = s.progress.current();
                if cur == s.name {
                    String::new()
                } else {
                    cur
                }
            }
            None => String::new(),
        }
    }

    /// The pill's Cancel — stop the walk but **keep what streamed in** (File ▸ Stop Scanning's
    /// path: cancel, resume prefetch, "Scan stopped" toast). Never blanks the current view.
    fn scan_pill_cancel(&mut self) {
        self.core.now = Instant::now();
        self.cancel_scan_command();
    }

    /// The Settings window closed. Edits were already applied live (`settings_edited` /
    /// `keymap_commit`), so this only drops the Shortcuts draft and clears the core's
    /// dialog-open state (the Cancel reaction — close, nothing further to apply).
    fn settings_closed(&mut self) {
        self.core.now = Instant::now();
        self.keymap_draft = None;
        self.keymap_dirty = false;
        self.keymap_note.clear();
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::SettingsCancelled,
        ));
    }

    /// A live snapshot of whichever progress the shown dialog tracks — the host polls this
    /// each pump while a Loading (fraction) or Scanning (found + current dir) dialog is up.
    /// The handles live in this crate (the workers are Rust-side), so this is just a read.
    fn dialog_progress(&self) -> ffi::DialogProgressFfi {
        ffi::DialogProgressFfi {
            fraction: self
                .archive_load
                .as_ref()
                .map(|l| l.progress.fraction())
                .unwrap_or(0.0),
            found: self
                .dir_scan
                .as_ref()
                .map(|s| s.progress.found() as u64)
                .unwrap_or(0),
            current_dir: self
                .dir_scan
                .as_ref()
                .map(|s| s.progress.current())
                .unwrap_or_default(),
        }
    }

    /// The native panels' background opacity setting (50–100) — the host polls this to feed
    /// the shared `panelBackground` so a live slider drag updates the panels immediately.
    fn panel_opacity(&self) -> u8 {
        self.core.settings.panel_opacity
    }

    // ── The unified native toast (④-style pill for every `show_toast`): the core holds the
    // data (native_toast suppresses the HUD raster); the host polls these each pump and draws
    // a SwiftUI pill. `toast_seq` re-triggers the entrance even for a repeated message. ──

    /// Whether a toast is currently live.
    fn toast_visible(&self) -> bool {
        self.core.toast_native.is_some()
    }

    /// The toast message (may be empty for an icon-only pill, e.g. rotate).
    fn toast_message(&self) -> String {
        self.core
            .toast_native
            .as_ref()
            .map(|t| t.message.clone())
            .unwrap_or_default()
    }

    /// The toast's semantic icon (0 = none) — the host maps it to an SF Symbol.
    fn toast_icon(&self) -> u8 {
        self.core
            .toast_native
            .as_ref()
            .map(|t| t.icon.to_u8())
            .unwrap_or(0)
    }

    /// Monotonic per-toast counter — the host keys its entrance animation off a change here,
    /// so an identical message firing twice still re-animates.
    fn toast_seq(&self) -> u64 {
        self.core.toast_native.as_ref().map(|t| t.seq).unwrap_or(0)
    }

    // ── The native one-line info readout (`i`): the last HUD element to go native. The core
    // owns the toggle + the content; the host draws a small bottom-corner SwiftUI pill. ──

    /// Whether the info line should show (`i` on + a photo loaded).
    fn info_line_visible(&self) -> bool {
        self.core.info_line_visible()
    }

    /// The readout's main text: `rel · W×H[· Live]` (the codec is a separate pill).
    fn info_line_text(&self) -> String {
        self.core.info_line_main().unwrap_or_default()
    }

    /// The codec label (e.g. `JPEG`) — the host renders it in a small capsule.
    fn info_line_codec(&self) -> String {
        self.core.info_line_codec()
    }

    /// Whether the current photo is a Live Photo — the host shows the livephoto mark by the
    /// codec instead of "Live" text.
    fn info_line_is_live(&self) -> bool {
        self.core.info_line_is_live()
    }

    /// Whether the current photo is an animated image (GIF/APNG/…) — the host shows a motion
    /// mark by the codec. Mutually exclusive with `info_line_is_live`.
    fn info_line_is_animated(&self) -> bool {
        self.core.info_line_is_animated()
    }

    /// Whether the current item is a video (task 79.9) — the host shows a film mark by the
    /// codec. Mutually exclusive with the live/animated marks.
    fn info_line_is_video(&self) -> bool {
        self.core.info_line_is_video()
    }

    // ── The native play hint (▶/Live Photo on a motion item): the last HUD overlay to go
    // native. The core signals *when* to flash it (seq) + *what* it is (kind); the host owns
    // the pill, its 3s fade, hover-to-hold, and click-to-play (via menu_action "play_pause").──

    /// 0 = none (still / already playing), 1 = Live Photo, 2 = another animation.
    fn play_hint_kind(&self) -> u8 {
        self.core.play_hint_kind()
    }

    /// Bumped when a fresh motion item settles — the host flashes the hint on a change.
    fn play_hint_seq(&self) -> u64 {
        self.core.play_hint_seq
    }

    /// Horizontal placement from Settings: 0 = left, 1 = center, 2 = right (default).
    fn info_line_align(&self) -> u8 {
        use pb_app_core::settings::InfoLineAlign;
        match self.core.settings.info_line_align {
            InfoLineAlign::Left => 0,
            InfoLineAlign::Center => 1,
            InfoLineAlign::Right => 2,
        }
    }

    /// The current settings as the flat form the Settings window binds to (NS2 item 5).
    /// `refresh_hz` rides along as the max-speed slider's ceiling (out-only).
    fn settings_form(&self) -> ffi::SettingsFormFfi {
        use pb_app_core::settings::{
            AppearanceMode, DescribeBackend, InfoLineAlign, ScaleModePref, ScrollAction,
            StartupMode,
        };
        let s = &self.core.settings;
        let hz = self.core.refresh_hz().max(1);
        // An uncapped (0) or ≥refresh saved rate shows pinned at the ceiling (egui parity).
        let max_fps = if s.max_advance_rate == 0 || s.max_advance_rate >= hz {
            hz
        } else {
            s.max_advance_rate
        };
        ffi::SettingsFormFfi {
            start_speed: s.start_speed,
            ramp_secs: s.ramp_secs,
            max_fps,
            refresh_hz: hz,
            hold_delay_ms: s.hold_delay_ms,
            scroll_action: match s.scroll_action {
                ScrollAction::Pan => 0,
                ScrollAction::Zoom => 1,
            },
            recursive: s.recursive,
            scale_mode: match s.scale_mode {
                ScaleModePref::Fit => 0,
                ScaleModePref::Fill => 1,
                ScaleModePref::Original => 2,
            },
            appearance_mode: match s.appearance_mode {
                AppearanceMode::System => 0,
                AppearanceMode::Light => 1,
                AppearanceMode::Dark => 2,
            },
            info_line_align: match s.info_line_align {
                InfoLineAlign::Left => 0,
                InfoLineAlign::Center => 1,
                InfoLineAlign::Right => 2,
            },
            show_image_info: s.show_image_info,
            glass_toolbar: s.glass_toolbar,
            info_show_folder: s.info_show_folder,
            info_show_filename: s.info_show_filename,
            info_show_resolution: s.info_show_resolution,
            info_show_codec: s.info_show_codec,
            letterbox_r: s.letterbox[0],
            letterbox_g: s.letterbox[1],
            letterbox_b: s.letterbox[2],
            letterbox_light_r: s.letterbox_light[0],
            letterbox_light_g: s.letterbox_light[1],
            letterbox_light_b: s.letterbox_light[2],
            info_opacity: s.info_opacity,
            panel_opacity: s.panel_opacity,
            startup_mode: match s.startup_mode {
                StartupMode::Fullscreen => 0,
                StartupMode::Windowed => 1,
                StartupMode::Remember => 2,
            },
            slideshow_interval_secs: s.slideshow_interval_secs,
            picker_fixed: s.picker_dir.is_some(),
            picker_dir: s
                .picker_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            mute_live_audio: s.mute_live_audio,
            describe_backend: match s.describe_backend {
                DescribeBackend::Auto => 0,
                DescribeBackend::AppleOnDevice => 1,
                DescribeBackend::LocalEndpoint => 2,
            },
            describe_endpoint: s.describe_endpoint.clone(),
            describe_model: s.describe_model.clone(),
            describe_prompt: s.describe_prompt.clone().unwrap_or_default(),
            describe_max_tokens: s.describe_max_tokens,
            describe_auto: s.describe_auto,
            speak_descriptions: s.speak_descriptions,
        }
    }

    /// A live edit from the auto-saving Settings window (macOS idiom — no Save button):
    /// fold the form onto the current settings and, when something actually changed,
    /// hand it to the core to apply + persist (`SettingsEdited`, window stays open).
    /// The unchanged case is a hard no-op, so the window's initial load echo and the
    /// close-time flush never touch disk.
    fn settings_edited(&mut self, form: ffi::SettingsFormFfi) {
        self.core.now = Instant::now();
        let s = fold_settings_form(&self.core.settings, &form, self.core.refresh_hz());
        if s == self.core.settings {
            return;
        }
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::SettingsEdited {
                settings: Some(Box::new(s)),
                keymap: None,
            },
        ));
    }

    // ---- The Shortcuts editor (NS2.6): a Rust-side draft keymap edited through
    // per-gesture calls, mirroring the egui Shortcuts tab exactly (same
    // `EDITOR_GROUPS`, two slots, steal-with-note, reset). The host renders rows and
    // captures chords (an AppKit local event monitor + the KeyMap Carbon table); every
    // edit lands here so the model/steal semantics can't drift between shells.

    /// Start (or restart) editing: draft = the live keymap. Call when the Settings
    /// window opens; each edit gesture then lands via `keymap_commit` (auto-save)
    /// and closing the window drops the draft.
    fn keymap_begin_edit(&mut self) {
        self.keymap_draft = Some(self.core.keymap.clone());
        self.keymap_dirty = false;
        self.keymap_note.clear();
    }

    /// The editor's section count (the shared `EDITOR_GROUPS` shape).
    fn keymap_group_count(&self) -> usize {
        pb_app_core::keymap::EDITOR_GROUPS.len()
    }

    fn keymap_group_title(&self, group: usize) -> String {
        pb_app_core::keymap::EDITOR_GROUPS
            .get(group)
            .map(|(t, _)| t.to_string())
            .unwrap_or_default()
    }

    fn keymap_group_len(&self, group: usize) -> usize {
        pb_app_core::keymap::EDITOR_GROUPS
            .get(group)
            .map(|(_, a)| a.len())
            .unwrap_or(0)
    }

    /// The stable action id at (group, index) — the currency the edit calls take.
    fn keymap_action_id(&self, group: usize, index: usize) -> String {
        pb_app_core::keymap::EDITOR_GROUPS
            .get(group)
            .and_then(|(_, a)| a.get(index))
            .map(|a| a.id().to_string())
            .unwrap_or_default()
    }

    fn keymap_action_label(&self, group: usize, index: usize) -> String {
        pb_app_core::keymap::EDITOR_GROUPS
            .get(group)
            .and_then(|(_, a)| a.get(index))
            .map(|a| a.label().to_string())
            .unwrap_or_default()
    }

    /// The chord in `slot` (0 primary / 1 secondary) of `action_id`, as macOS glyphs
    /// (`⌃⇧ R`, `→`, "" = unbound; the editor style, so Esc shows ⎋) — read from the
    /// draft while editing.
    fn keymap_slot_display(&self, action_id: &str, slot: usize) -> String {
        let Some(action) = Action::from_id(action_id) else {
            return String::new();
        };
        let map = self.keymap_draft.as_ref().unwrap_or(&self.core.keymap);
        map.slot(action, slot)
            .map(|c| c.mac_symbol_editor())
            .unwrap_or_default()
    }

    /// The macOS menu bar's own accelerator for this action ("" = none) — the Shortcuts
    /// editor shows it as a read-only hint beside the editable slots. ⌘-chords live in
    /// the menu (not the keymap: its defaults keep real-Ctrl chords), so without this
    /// the editor implies the ⌃ alternate is the only shortcut when e.g. ⌘C also copies.
    fn keymap_menu_chord(&self, action_id: &str) -> String {
        Action::from_id(action_id)
            .and_then(pb_app_core::keymap::macos_menu_chord)
            .map(|c| c.mac_symbol_editor())
            .unwrap_or_default()
    }

    /// A captured chord for `slot` of `action_id` (key = a `PbKey::from_name` name from
    /// the host's Carbon table). Steals the chord from any prior owner — the note
    /// ("Moved ⌘C from Copy Image") lands in `keymap_last_note`. Returns false for a
    /// key the keymap can't express (the host stays armed and waits, egui parity).
    #[allow(clippy::too_many_arguments)]
    fn keymap_capture(
        &mut self,
        action_id: &str,
        slot: usize,
        key: &str,
        ctrl: bool,
        shift: bool,
        alt: bool,
        logo: bool,
    ) -> bool {
        let (Some(action), Some(key)) = (Action::from_id(action_id), PbKey::from_name(key)) else {
            return false;
        };
        let Some(draft) = self.keymap_draft.as_mut() else {
            return false;
        };
        let chord = pb_app_core::keymap::KeyChord::new(key, ctrl, shift, alt, logo);
        let stolen = draft.set_slot(action, slot, chord);
        self.keymap_dirty = true;
        self.keymap_note = stolen
            .map(|a| {
                format!(
                    "Moved {} from \u{201c}{}\u{201d}",
                    chord.mac_symbol_editor(),
                    a.label()
                )
            })
            .unwrap_or_default();
        true
    }

    /// The transient "moved from …" note from the last capture ("" = none).
    fn keymap_last_note(&self) -> String {
        self.keymap_note.clone()
    }

    fn keymap_clear_slot(&mut self, action_id: &str, slot: usize) {
        let Some(action) = Action::from_id(action_id) else {
            return;
        };
        if let Some(draft) = self.keymap_draft.as_mut() {
            draft.clear_slot(action, slot);
            self.keymap_dirty = true;
            self.keymap_note.clear();
        }
    }

    fn keymap_reset_defaults(&mut self) {
        if let Some(draft) = self.keymap_draft.as_mut() {
            draft.reset_to_defaults();
            self.keymap_dirty = true;
            self.keymap_note.clear();
        }
    }

    fn keymap_is_dirty(&self) -> bool {
        self.keymap_dirty
    }

    /// Commit the Shortcuts draft live (auto-save): when a binding actually changed,
    /// apply + persist the draft as the live keymap (`SettingsEdited`, window stays
    /// open). The draft itself stays put so further edits continue from it; the
    /// not-dirty case is a hard no-op (never touches keymap.toml). The draft edits
    /// themselves stay pure — the host calls this after each successful gesture.
    fn keymap_commit(&mut self) {
        if !self.keymap_dirty {
            return;
        }
        let Some(km) = self.keymap_draft.clone() else {
            return;
        };
        self.keymap_dirty = false;
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::SettingsEdited {
                settings: None,
                keymap: Some(km),
            },
        ));
    }

    /// `ShellFlowAction(DeletePermanent)` intercepted — the winit shell's
    /// `confirm_delete_permanent` mirrored: settle any pending delete-advance, refuse an
    /// archive entry with a toast, else arm `pending_confirm_delete` and open the native
    /// confirm dialog with the file name in the question.
    fn confirm_delete_permanent(&mut self) {
        self.core.flush_pending_delete();
        let Some(item) = self.core.displayed_item else {
            return;
        };
        if self.core.source.path(item).is_none() {
            self.core.show_toast("Can't delete this"); // archive entry — no file
            return;
        }
        let name = engine::file_name_of(self.core.source.name(item));
        self.core.pending_confirm_delete = Some(item);
        // Finder's exact delete-immediately wording (owner request): headline on the
        // first line, the informative sentence after the newline — the host splits
        // them into NSAlert's messageText/informativeText.
        self.dialog_message = format!(
            "Are you sure you want to delete \u{201c}{name}\u{201d}?\n\
             This item will be deleted immediately. You can\u{2019}t undo this action."
        );
        self.core.effects.push(contract::CoreEffect::ShowDialog(
            contract::DialogKind::Confirm,
        ));
    }

    /// Prompt for an archive's password (or re-prompt after a wrong one) — the winit
    /// shell's `prompt_archive_password` mirrored. Remembers `path` so a submitted entry
    /// re-opens it; a retry sets the inline error (the host re-shows the same sheet in
    /// place — state-driven SwiftUI makes winit's promote-in-place dance implicit).
    fn prompt_archive_password(&mut self, path: PathBuf, wrong: bool) {
        self.core.password_archive = Some(path.clone());
        self.password_error = if wrong && self.shown_dialog == Some(contract::DialogKind::Password)
        {
            "Incorrect password. Please try again.".to_string()
        } else {
            String::new()
        };
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("this archive");
        // Two lines: the lead on one, the (possibly long) file name on its own.
        self.dialog_message = format!("Enter the password for\n\u{201c}{name}\u{201d}");
        self.core.effects.push(contract::CoreEffect::ShowDialog(
            contract::DialogKind::Password,
        ));
    }

    /// Push a `CloseDialog` (so the host dismisses its sheet) only when the shown dialog is
    /// one of `kinds` — the winit shell's kind-guarded closes (`close_scanning_dialog`, the
    /// archive success's loading/password close) so an unrelated dialog is never stolen.
    fn close_dialog_kinds(&mut self, kinds: &[contract::DialogKind]) {
        if self.shown_dialog.is_some_and(|k| kinds.contains(&k)) {
            self.core.effects.push(contract::CoreEffect::CloseDialog);
        }
    }

    /// A physical key went down. `key` is a [`PbKey`] name accepted by `PbKey::from_name`
    /// (e.g. `"Space"`, `"Escape"`, `"Right"`, `"C"` — NOT winit's `"ArrowRight"`/`"KeyC"`
    /// spellings) — the Swift host maps `NSEvent` → this name (the input-adapter job, NS1).
    /// Unknown names are ignored. OS auto-repeat is passed via `is_repeat` (named to dodge
    /// the Swift keyword `repeat` — swift-bridge gotcha #4: a Rust param named after a Swift
    /// keyword generates Swift glue that doesn't compile); the core drops repeats for held
    /// actions, exactly as the winit shell does.
    fn key_down(
        &mut self,
        key: &str,
        ctrl: bool,
        shift: bool,
        alt: bool,
        logo: bool,
        is_repeat: bool,
    ) {
        // The FFI is host/shell code, so it stamps the clock (the core never reads it — NS0).
        self.core.now = Instant::now();
        if let Some(key) = PbKey::from_name(key) {
            self.core.handle(CoreEvent::KeyDown {
                key,
                mods: Modifiers {
                    ctrl,
                    shift,
                    alt,
                    logo,
                },
                repeat: is_repeat,
            });
        }
    }

    /// A physical key was released.
    fn key_up(&mut self, key: &str) {
        self.core.now = Instant::now();
        if let Some(key) = PbKey::from_name(key) {
            self.core.handle(CoreEvent::KeyUp { key });
        }
    }

    /// The window lost key focus — the core clears held keys (the focus-loss release net).
    fn focus_lost(&mut self) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::FocusLost);
    }

    /// The effective macOS appearance changed (or its initial value at attach) — the
    /// host reports it from `viewDidChangeEffectiveAppearance`; the core re-resolves
    /// the Appearance preference (#46) and re-themes the HUD + letterbox on a flip.
    fn os_theme_changed(&mut self, dark: bool) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::OsThemeChanged { dark });
    }

    /// A frame / idle tick: drives held-key pacing, slideshow dwell, prefetch, and animation.
    /// The host calls this each frame it draws and on the scheduled wake deadlines returned
    /// via `SetWake` (a `MTKViewDelegate.draw(in:)` + timer, per the plan). Like the winit
    /// `about_to_wait`, the shell's flow-polling runs first: finished scan/archive workers
    /// feed their results into the core before the tick evaluates pacing/prefetch.
    fn tick(&mut self) {
        self.core.now = Instant::now();
        self.poll_dir_scan();
        self.poll_archive_load();
        self.core.handle(CoreEvent::Tick(self.core.now));
        self.apply_menu_state();
    }

    /// Mirror the live menu check/enabled state — the winit shell's `apply_menu_state`,
    /// which pushes `SetMenuState` per tick when something changed. This was MISSING on
    /// this host: nothing ever produced the effect, so the Swift menu bar sat on
    /// `MenuState::default()` forever — Save Rotation permanently disabled (the
    /// owner-reported "not implemented"), stale checkmarks everywhere. Compare-gated:
    /// nothing is pushed when nothing moved.
    fn apply_menu_state(&mut self) {
        let next = AppCore::menu_state_from(
            self.core.view.mode,
            self.core.info_line,
            self.core.panels,
            self.core.folder_tree_open,
            self.core.recursive,
            !self.core.windowed, // `windowed` is the inverse of the fullscreen checkbox
            self.core.slideshow.on,
            // Effective, not raw: a `--mute` launch override must show as a checked
            // menu item (the winit shell passes `effective_mute()` here too).
            self.core.effective_mute(),
            self.core.can_save_rotation(),
            self.core.can_reveal(),
            self.dir_scan.is_some(),
            self.core
                .undo_stack
                .last()
                .map(pb_app_core::UndoAction::menu_label),
            false, // native (Spaces) fullscreen: AppKit manages that item's title itself
            self.core.displayed_item,
            self.core.compare_pin,
        );
        if self.last_menu_state == next {
            return;
        }
        self.core
            .effects
            .push(contract::CoreEffect::SetMenuState(next));
    }

    /// Attach the host's **retained `CAMetalLayer`** (passed as its raw pointer bits — the
    /// slice of NS1 item 2) and stand the wgpu renderer up on it: create the surface, route
    /// the real size through the standard `Resized` path, and — on an empty deck — show the
    /// blank letterbox + the "Press O to open" hint exactly like the winit shell's
    /// `resumed()`. The host then pokes the layer's EDR colorspace (see `wants_edr`) and
    /// calls `render`.
    ///
    /// Safety contract (upheld by the Swift host; see `WgpuRenderer::new_from_ca_layer`):
    /// the layer is valid + retained and outlives the renderer (`detach_layer` runs before
    /// the view/layer dies), and every call happens on the main actor.
    fn attach_layer(&mut self, layer_ptr: usize, width: u32, height: u32, scale: f32) {
        self.core.now = Instant::now();
        // Viewport + fit first (through the standard path; renderer is still None so the
        // swapchain half no-ops) so the initial decode-to-fit below uses the real size.
        self.core.handle(CoreEvent::Resized {
            width,
            height,
            scale,
        });
        // Decode the first image (or the test pattern / blank) at the display size —
        // the same synchronous preview-first path the winit `resumed` uses.
        let (rgba, iw, ih, color, hdr, peak, title) = self.core.initial_image();
        // SAFETY: the host passes a valid retained CAMetalLayer and guarantees the
        // lifetime + main-thread rules (documented on `new_from_ca_layer`).
        let mut renderer = unsafe {
            pb_render::WgpuRenderer::new_from_ca_layer(
                layer_ptr as *mut std::ffi::c_void,
                width,
                height,
                &rgba,
                iw,
                ih,
                color,
                hdr,
                peak,
            )
        };
        // The resolved theme's fill (#46) — the host reports the OS appearance just
        // before attaching, so System resolves against the real desktop theme here.
        renderer.set_letterbox(self.core.effective_letterbox());
        // Size the resident ring to the display and start filling it — navigation is a
        // rebind into this ring, never a decode (mirrors the winit `resumed`).
        let cap = engine::ring_capacity(self.core.slot_bytes_estimate());
        self.core.ring = ResidentRing::new_with_budget(cap, engine::RING_BUDGET_BYTES);
        renderer.reserve_ring(cap, width.max(1), height.max(1));
        let (ahead, behind) = engine::window_for_capacity(cap);
        self.core.ahead = ahead;
        self.core.behind = behind;
        self.core.displayed_item = self.core.playlist.current();
        self.core.target_item = self.core.playlist.current();
        self.core.last_present = Some(Instant::now());
        self.core.renderer = Some(Box::new(renderer));
        // Retint the HUD compositor for the resolved theme (#46) — it defaults to
        // dark; a light desktop / forced-Light preference re-themes it here, before
        // any overlay bitmap is built.
        self.core.refresh_theme();
        self.core
            .effects
            .push(contract::CoreEffect::SetTitle(title));
        self.core.request_prefetch();
        // Empty deck (bare launch): blank letterbox + the Open File / Open Folder call
        // to action. `show_open_hint` signals the native welcome surface (native_open,
        // task #54); the host presents it as a SwiftUI view.
        if self.core.playlist.current().is_none() {
            if let Some(r) = self.core.renderer.as_mut() {
                r.clear_image();
            }
            self.core.show_open_hint();
        }
    }

    /// Drop the renderer (and its wgpu surface). The host MUST call this before the
    /// hosting view/layer is destroyed — the other half of the layer-lifetime contract.
    fn detach_layer(&mut self) {
        self.core.renderer = None;
    }

    /// The surface resized (or moved to a display with a different backing scale):
    /// `width`×`height` in physical pixels. The host calls `render` afterwards.
    fn resized(&mut self, width: u32, height: u32, scale: f32) {
        self.core.now = Instant::now();
        self.core.handle(CoreEvent::Resized {
            width,
            height,
            scale,
        });
    }

    /// Draw a frame into the attached layer. No-op when no layer is attached.
    /// Routes through `AppCore::draw` (not the renderer directly) so a frame the
    /// surface DROPS (Lost/Outdated/Timeout — routine mid resize/fullscreen churn)
    /// sets `redraw_pending` and gets retried by the tick loop, instead of leaving
    /// the stale frame composited forever (the "unfilled background" bug, 2026-07-04).
    fn render(&mut self) {
        if self.core.renderer.is_some() {
            self.core.draw();
        }
    }

    /// Whether the surface came up fp16 scRGB (HDR/wide-gamut capable) — when true the
    /// host must set the layer's colorspace to extended-linear-sRGB + enable EDR (the
    /// macOS layer poke `pb-app/src/hdr_surface.rs` does on the winit target; the host
    /// owns the layer here) and report the panel headroom via `set_edr_headroom`.
    fn wants_edr(&self) -> bool {
        self.core
            .renderer
            .as_ref()
            .and_then(|r| r.hdr_surface_wants_edr())
            .is_some()
    }

    /// The display's EDR headroom (max EDR color component value; ≥ 1.0) for the
    /// highlight roll-off — macOS hard-clips above it (unlike Windows' DWM tone-map).
    fn set_edr_headroom(&mut self, headroom: f32) {
        if let Some(r) = self.core.renderer.as_mut() {
            r.set_edr_headroom(headroom);
        }
    }

    /// Pull the next effect the core produced, or `None` when the queue is drained — the host
    /// loops this on the main actor after each event/tick (`while let e = next_effect() { … }`).
    /// The **shell-internal effects are intercepted here** and never surface to Swift: the
    /// `Begin*`/`Cancel*` scan+archive flow runs on this crate's worker threads (the winit
    /// shell's drain does the same). Everything not yet bridged arrives as
    /// [`ffi::CoreEffectFfi::Other`].
    ///
    /// Pull-style rather than `-> Vec<CoreEffectFfi>` (swift-bridge gotcha #3): 0.1.59
    /// generates the *Rust* half of a `Vec<transparent enum>` return but not the Swift-side
    /// `Vectorizable` conformance or the `Vec_…` C shims, so the generated Swift doesn't
    /// compile. `Option<transparent enum>` is fully supported — and a handful of nanosecond
    /// FFI calls per event is free anyway.
    fn next_effect(&mut self) -> Option<ffi::CoreEffectFfi> {
        use contract::CoreEffect as C;
        // Intercepted effects can enqueue follow-ups (a sync zip open resolves inline →
        // `ArchiveResolved` → more effects); re-reading the queue each pass keeps this the
        // same loop-until-quiescent shape as the winit drain.
        while !self.core.effects.is_empty() {
            match self.core.effects.remove(0) {
                C::BeginDirScan { source, cursor } => self.begin_dir_scan(source, cursor),
                C::BeginArchiveOpen { path, password } => self.begin_archive_open(path, password),
                C::CancelScan => self.cancel_dir_scan(),
                C::CancelArchiveLoad => self.cancel_archive_load(),
                // Flow commands whose execution is shell-*Rust* (the scan worker lives in
                // this crate) — run here; only the genuinely Swift-native flows surface.
                C::ShellFlowAction(Action::Recursive) => self.toggle_recursive(),
                C::ShellFlowAction(Action::CancelScan) => self.cancel_scan_command(),
                // NS2: the permanent-delete confirm opens Rust-side (arms the pending item
                // + composes the question), surfacing as ShowDialog("confirm").
                C::ShellFlowAction(Action::DeletePermanent) => self.confirm_delete_permanent(),
                // Track what's shown (the winit shell's `self.dialog.kind()` mirror) and
                // keep the core's `dialog_open` in sync — it pauses the slideshow while a
                // dialog is up. About is the standalone NSApp panel, not tracked.
                C::ShowDialog(kind) => {
                    if kind != contract::DialogKind::About {
                        self.shown_dialog = Some(kind);
                        self.core.dialog_open = true;
                    }
                    return Some(map_effect(C::ShowDialog(kind)));
                }
                C::CloseDialog => {
                    self.shown_dialog = None;
                    self.core.dialog_open = false;
                    self.password_error.clear();
                    return Some(ffi::CoreEffectFfi::CloseDialog);
                }
                // The image payload can be tens of MB — stash it and surface a bare
                // marker; the host pulls it via the clipboard accessors (gotcha #3).
                C::WriteClipboard(payload) => {
                    self.pending_clipboard = Some(payload);
                    return Some(ffi::CoreEffectFfi::WriteClipboard);
                }
                // Same stash pattern: the host pulls the struct via `menu_state()`.
                C::SetMenuState(state) => {
                    self.last_menu_state = state;
                    return Some(ffi::CoreEffectFfi::MenuStateChanged);
                }
                // A natively-presented panel changed (task #54) — the host calls
                // `help_refresh()` + `help_visible()` and updates its SwiftUI view.
                C::PanelsChanged => return Some(ffi::CoreEffectFfi::PanelsChanged),
                // The F toggle persists the remembered mode + windowed geometry together
                // (the winit shell's `apply_window_mode` twin — task #42's missing save;
                // `settings.window` is already fresh via `note_window_geometry`). Startup
                // restore never passes here (the host calls its setWindowMode directly),
                // so this only fires on a real user toggle. `persist_prefs` keeps unit
                // tests from writing the real settings.toml.
                C::SetWindowMode(mode) => {
                    self.core.geometry_save_at = None;
                    if self.core.persist_prefs {
                        self.core.settings.save();
                    }
                    return Some(map_effect(C::SetWindowMode(mode)));
                }
                other => return Some(map_effect(other)),
            }
        }
        None
    }

    /// Toggle recursive scanning of the current folder — the winit shell's
    /// `toggle_recursive` mirrored: re-stream the walk with the flag flipped, preserving
    /// the current photo by path. A no-op for an explicit list / archive (no scan root).
    fn toggle_recursive(&mut self) {
        let Some(root) = self.core.scan_root.clone() else {
            return;
        };
        let recursive = !self.core.recursive;
        let cursor = self
            .core
            .displayed_item
            .and_then(|i| self.core.source.path(i))
            .map(Path::to_path_buf)
            .map(Cursor::At)
            .unwrap_or(Cursor::First);
        self.begin_dir_scan(
            Source::Scan {
                roots: vec![root],
                recursive,
            },
            cursor,
        );
        self.core.show_toast(if recursive {
            "Recursive folders: on"
        } else {
            "Recursive folders: off"
        });
    }

    /// File ▸ Stop Scanning — stop the in-flight walk, **keeping what streamed in**
    /// (the winit `cancel_scan_command` mirrored). No-op when no scan is running.
    fn cancel_scan_command(&mut self) {
        if self.dir_scan.is_none() {
            return;
        }
        self.cancel_dir_scan();
        self.close_dialog_kinds(&[contract::DialogKind::Scanning]);
        self.core.request_prefetch();
        self.core.show_toast("Scan stopped");
    }

    // ---- The shell's Rust half: the scan/archive worker flow (mirrors the winit shell's
    // `begin_*`/`poll_*`, including the Loading/Scanning/Password dialogs those drive —
    // opened by pushing `ShowDialog` with the text stashed in `dialog_message`).

    /// Start a streaming folder walk on a worker thread (`CoreEffect::BeginDirScan`).
    fn begin_dir_scan(&mut self, source: Source, cursor: Cursor) {
        self.cancel_dir_scan();
        self.core.deleted.clear(); // fresh scan → fresh universe, no stale tombstones
        self.scan_gen += 1;
        let generation = self.scan_gen;
        let progress = ScanProgress::new();
        let (roots, recursive) = match source {
            Source::Scan { roots, recursive } => (roots, recursive),
            _ => return, // open_plan routes explicit lists + archives elsewhere
        };
        let root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
        let scan_root = roots.first().cloned();
        // The scan root's display name for the Scanning dialog headline (winit's
        // `scan_display_name`).
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        let worker_progress = progress.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            scan::stream_scan(
                roots,
                recursive,
                cursor,
                root,
                scan_root,
                generation,
                worker_progress,
                tx,
            );
        });
        // A Scanning dialog already up (a previous slow scan the user re-opened over):
        // re-point it at this walk in place — the host updates the same sheet, no flicker
        // (winit's `set_scan`).
        if self.shown_dialog == Some(contract::DialogKind::Scanning) {
            self.dialog_message = format!("Scanning \u{201c}{name}\u{201d}\u{2026}");
            self.core.effects.push(contract::CoreEffect::ShowDialog(
                contract::DialogKind::Scanning,
            ));
        }
        self.core.scanning = true; // sequential-only prefetch while streaming
        self.core.scan_bootstrapped = false; // first non-empty batch bootstraps
        self.dir_scan = Some(DirScan {
            generation,
            rx,
            progress,
            name,
            started: Instant::now(),
        });
    }

    /// Drain finished scan snapshots into the core (each `tick`). The first non-empty batch
    /// bootstraps the view, the rest extend it; `Done` resumes normal prefetch.
    fn poll_dir_scan(&mut self) {
        use std::sync::mpsc::TryRecvError;
        loop {
            let (cur_gen, recv) = match self.dir_scan.as_ref() {
                Some(s) => (s.generation, s.rx.try_recv()),
                None => return,
            };
            match recv {
                Ok((generation, ScanUpdate::Batch(resolved))) => {
                    if generation != cur_gen {
                        continue; // superseded by a newer open
                    }
                    self.core.handle(CoreEvent::ScanBatch(resolved));
                    // A photo is on screen now — a revealed Scanning sheet has served its
                    // purpose, and on this host it blocks every key while it's up. Drop it
                    // so browsing starts at the FIRST image, not the end of the walk; the
                    // scan-count chip takes over as the (non-blocking) progress display.
                    if self.core.scan_bootstrapped {
                        self.close_dialog_kinds(&[contract::DialogKind::Scanning]);
                    }
                }
                Ok((generation, ScanUpdate::Done)) => {
                    if generation != cur_gen {
                        continue;
                    }
                    // Capture the scanned folder's name before dropping the handle — an empty
                    // folder toasts with it (③) instead of the old blocking NSAlert.
                    let scanned = self
                        .dir_scan
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.dir_scan = None;
                    let never_bootstrapped = !self.core.scan_bootstrapped;
                    self.core.handle(CoreEvent::ScanDone);
                    // Walk finished — drop the Scanning progress dialog (if it revealed).
                    self.close_dialog_kinds(&[contract::DialogKind::Scanning]);
                    if never_bootstrapped {
                        // No images: keep whatever's on screen; a non-modal toast (never the
                        // blocking alert) if a deck is already up (③ keep-deck-until-photos).
                        self.core.scan_found_no_photos(&scanned);
                    }
                    return;
                }
                Err(TryRecvError::Empty) => {
                    // Nothing more queued this tick. The ambient scan pill (④, native
                    // top-center) shows non-blocking progress instead of a modal Scanning
                    // dialog — it's driven by the `scan_pill_*` accessors + the Swift pump,
                    // so there's nothing to reveal here.
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    self.dir_scan = None;
                    self.core.scanning = false;
                    // Worker died — don't strand its progress dialog.
                    self.close_dialog_kinds(&[contract::DialogKind::Scanning]);
                    return;
                }
            }
        }
    }

    /// Ask the in-flight walk (if any) to stop, and resume normal prefetch.
    fn cancel_dir_scan(&mut self) {
        if let Some(s) = self.dir_scan.as_ref() {
            s.progress.request_cancel();
        }
        self.dir_scan = None;
        self.core.scanning = false;
    }

    /// Start opening an archive (`CoreEffect::BeginArchiveOpen`): a `.zip` synchronously, a
    /// `.7z` on a worker thread after the RAM pre-flight (a real OOM aborts uncatchably, so
    /// over-budget is refused up front).
    fn begin_archive_open(&mut self, path: PathBuf, password: Option<String>) {
        let is_7z = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("7z"));
        let was_password_attempt = password.is_some();
        // Anti-stacking: a newer open supersedes (and cancels) an in-flight one.
        if let Some(prev) = self.archive_load.as_ref() {
            prev.progress.request_cancel();
        }
        if !is_7z {
            let result = scan::open_archive(&path, password);
            self.finish_archive_open(result, was_password_attempt, path);
            return;
        }
        let projected = match scan::seven_z_preflight(&path, password.as_deref()) {
            Ok(projected) => projected,
            Err(e) => {
                self.finish_archive_open(Err(e), was_password_attempt, path);
                return;
            }
        };
        // The budget the resident entries won't use is what the open may spend
        // transiently on within-block MT decode (the solid-archive ~3x speedup).
        let mt_headroom = pb_app_core::archive::ram_budget().saturating_sub(projected);
        self.archive_gen += 1;
        let generation = self.archive_gen;
        let progress = pb_source::OpenProgress::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        let worker_progress = progress.clone();
        std::thread::spawn(move || {
            let result = scan::load_seven_z(&worker_path, password, &worker_progress, mt_headroom);
            let _ = tx.send((generation, result));
        });
        // The determinate "Opening…" progress + Cancel dialog. If the password prompt is
        // still up (a just-verified entry), the same ShowDialog replaces its content in
        // place — SwiftUI's state-driven sheet makes winit's `become_loading` implicit.
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("archive");
        self.dialog_message = format!("Opening \u{201c}{name}\u{201d}\u{2026}");
        self.core.effects.push(contract::CoreEffect::ShowDialog(
            contract::DialogKind::Loading,
        ));
        self.archive_load = Some(ArchiveLoad {
            generation,
            rx,
            path,
            was_password_attempt,
            progress,
        });
    }

    /// Pick up a finished background archive open (each `tick`).
    fn poll_archive_load(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let (load_gen, recv) = match self.archive_load.as_ref() {
            Some(load) => (load.generation, load.rx.try_recv()),
            None => return,
        };
        match recv {
            Ok((generation, result)) => {
                let load = self.archive_load.take();
                if generation != load_gen {
                    return; // superseded by a newer open
                }
                let (path, was_attempt) = match load {
                    Some(l) => (l.path, l.was_password_attempt),
                    None => return,
                };
                self.finish_archive_open(result, was_attempt, path);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                // Worker died without a result (a panic inside the decompressor). Drop the
                // handle and don't strand its Loading dialog (winit parity).
                self.archive_load = None;
                self.close_dialog_kinds(&[contract::DialogKind::Loading]);
            }
        }
    }

    /// Act on a finished archive open (zip-sync or 7z-async) — the winit shell's
    /// `finish_archive_open` mirrored: a non-empty success closes the loading/password
    /// dialog and installs the playlist via `ArchiveResolved`; `PasswordRequired` opens
    /// (or re-prompts, after a wrong attempt) the password dialog; any other failure
    /// replaces the dialog with the error notice.
    fn finish_archive_open(
        &mut self,
        result: Result<Resolved, ArchiveOpenError>,
        was_password_attempt: bool,
        path: PathBuf,
    ) {
        use contract::DialogKind as K;
        match result {
            Ok(r) if !r.source.is_empty() => {
                self.close_dialog_kinds(&[K::Loading, K::Password]);
                self.core.handle(CoreEvent::ArchiveResolved(r));
            }
            Ok(_) => self.fail_archive_open(&ArchiveOpenError::Empty),
            Err(ArchiveOpenError::PasswordRequired) => {
                self.prompt_archive_password(path, was_password_attempt);
            }
            // User cancelled: drop quietly, keeping whatever is on screen.
            Err(ArchiveOpenError::Cancelled) => {
                self.core.password_archive = None;
                self.close_dialog_kinds(&[K::Loading]);
            }
            Err(e) => self.fail_archive_open(&e),
        }
    }

    /// A terminal archive-open failure (not a password retry): forget the pending archive
    /// and replace any loading/password dialog with the error notice.
    fn fail_archive_open(&mut self, e: &ArchiveOpenError) {
        self.core.password_archive = None;
        self.close_dialog_kinds(&[
            contract::DialogKind::Loading,
            contract::DialogKind::Password,
        ]);
        self.report_error(e.user_message());
    }

    /// Ask the in-flight archive open (if any) to stop; the poll drops it quietly.
    fn cancel_archive_load(&mut self) {
        if let Some(load) = self.archive_load.as_ref() {
            load.progress.request_cancel();
        }
    }

    fn report_error(&mut self, msg: String) {
        self.core
            .effects
            .push(contract::CoreEffect::ReportError(msg));
    }
}

/// Fold an edited Settings form back onto `base`, preserving the fields the form doesn't
/// expose (the remembered fullscreen state, window geometry), clamped to valid ranges —
/// the egui `SettingsDraft::to_settings` mirrored. Pure (no I/O), so it's unit-testable
/// without touching the user's real settings.toml (`SettingsSaved` → `apply_settings`
/// persists).
fn fold_settings_form(
    base: &pb_app_core::settings::Settings,
    form: &ffi::SettingsFormFfi,
    refresh_hz: u32,
) -> pb_app_core::settings::Settings {
    use pb_app_core::settings::{
        AppearanceMode, DescribeBackend, InfoLineAlign, ScaleModePref, ScrollAction, StartupMode,
    };
    let mut s = base.clone();
    s.start_speed = form.start_speed;
    s.ramp_secs = form.ramp_secs;
    // The slider tops out at the refresh rate; that ceiling means "uncapped" (0).
    s.max_advance_rate = if form.max_fps >= refresh_hz.max(1) {
        0
    } else {
        form.max_fps
    };
    s.hold_delay_ms = form.hold_delay_ms;
    s.scroll_action = match form.scroll_action {
        1 => ScrollAction::Zoom,
        _ => ScrollAction::Pan,
    };
    s.recursive = form.recursive;
    s.scale_mode = match form.scale_mode {
        1 => ScaleModePref::Fill,
        2 => ScaleModePref::Original,
        _ => ScaleModePref::Fit,
    };
    s.appearance_mode = match form.appearance_mode {
        1 => AppearanceMode::Light,
        2 => AppearanceMode::Dark,
        _ => AppearanceMode::System,
    };
    s.info_line_align = match form.info_line_align {
        0 => InfoLineAlign::Left,
        1 => InfoLineAlign::Center,
        _ => InfoLineAlign::Right,
    };
    s.show_image_info = form.show_image_info;
    s.glass_toolbar = form.glass_toolbar;
    s.info_show_folder = form.info_show_folder;
    s.info_show_filename = form.info_show_filename;
    s.info_show_resolution = form.info_show_resolution;
    s.info_show_codec = form.info_show_codec;
    s.letterbox = [form.letterbox_r, form.letterbox_g, form.letterbox_b];
    s.letterbox_light = [
        form.letterbox_light_r,
        form.letterbox_light_g,
        form.letterbox_light_b,
    ];
    s.info_opacity = form.info_opacity;
    s.panel_opacity = form.panel_opacity;
    s.startup_mode = match form.startup_mode {
        0 => StartupMode::Fullscreen,
        1 => StartupMode::Windowed,
        _ => StartupMode::Remember,
    };
    s.slideshow_interval_secs = form.slideshow_interval_secs;
    // Pin a folder only when the toggle is on *and* one was chosen (egui parity).
    s.picker_dir = if form.picker_fixed && !form.picker_dir.is_empty() {
        Some(PathBuf::from(form.picker_dir.clone()))
    } else {
        None
    };
    s.mute_live_audio = form.mute_live_audio;
    s.describe_backend = match form.describe_backend {
        1 => DescribeBackend::AppleOnDevice,
        2 => DescribeBackend::LocalEndpoint,
        _ => DescribeBackend::Auto,
    };
    s.describe_endpoint = form.describe_endpoint.trim().to_string();
    s.describe_model = form.describe_model.trim().to_string();
    let prompt = form.describe_prompt.trim();
    s.describe_prompt = (!prompt.is_empty()).then(|| prompt.to_string());
    s.describe_max_tokens = form.describe_max_tokens;
    s.describe_auto = form.describe_auto;
    s.speak_descriptions = form.speak_descriptions;
    s.clamp();
    s
}

/// The launch-preflight CLI parse (task #78): the Swift host calls this FIRST — before
/// any window, before Sparkle, before the engine is built — with the **full**
/// `ProcessInfo.processInfo.arguments` (argv[0] included; clap consumes the first element
/// as the program name) and the bundle's version string. A free function on purpose: a
/// terminal `--help` must never construct the decode pool just to print text.
///
/// The outcome mirrors the winit shell's `report_cli_error_and_exit` split:
/// - `proceed = true` → run the app normally (`text` is empty). The host later feeds the
///   same argv through [`AppCoreHandle::apply_launch_args`] to apply the overrides.
/// - `proceed = false` → render `text` (help / version / a usage error) to the stream
///   `use_stderr` picks and exit with `exit_code` (clap's own: 0 help/version, 2 usage).
///
/// Mixed strictness (the winit contract): a nonexistent positional path is a usage error
/// here — same message text, exit 2 — so a typo'd path never launches an empty viewer.
///
/// `stdout_tty` / `stderr_tty`: whether each stream is a terminal (Swift's `isatty`).
/// The winit shell gets colored help for free (`clap::Error::print` styles at print
/// time), but here the text crosses the FFI as a plain string — so the render picks
/// ANSI or plain per the stream the host will actually write to. A pipe / redirect
/// stays clean; a terminal gets the same bold-yellow/cyan/green help Windows shows.
fn cli_preflight(
    argv: Vec<String>,
    version: String,
    stdout_tty: bool,
    stderr_tty: bool,
) -> ffi::LaunchPreflightFfi {
    match pb_cli::parse_from(argv, &version) {
        Ok(cli) => {
            for p in cli.launch_paths() {
                if !p.exists() {
                    return ffi::LaunchPreflightFfi {
                        proceed: false,
                        text: format!("photoblaze: no such file or folder: {}", p.display()),
                        use_stderr: true,
                        exit_code: 2,
                    };
                }
            }
            ffi::LaunchPreflightFfi {
                proceed: true,
                text: String::new(),
                use_stderr: false,
                exit_code: 0,
            }
        }
        Err(e) => {
            let rendered = e.render();
            let tty = if e.use_stderr() {
                stderr_tty
            } else {
                stdout_tty
            };
            ffi::LaunchPreflightFfi {
                proceed: false,
                text: if tty {
                    rendered.ansi().to_string()
                } else {
                    rendered.to_string()
                },
                use_stderr: e.use_stderr(),
                exit_code: e.exit_code(),
            }
        }
    }
}

/// The AI settings tab's **Test connection** probe (task #44): GET the endpoint's model
/// list, summarize reachability + model count, and warn when no served model looks
/// vision-capable (describe needs a VLM). Stateless + blocking — Swift calls it off the
/// main thread. Mirrors the egui shell's `render_conn_test` wording.
fn probe_describe_endpoint(url: String) -> ffi::ProbeResultFfi {
    match pb_app_core::describe::probe_endpoint(&url) {
        Ok(models) if models.is_empty() => ffi::ProbeResultFfi {
            ok: false,
            message: "Reachable, but no models are loaded.".to_string(),
            models: String::new(),
        },
        Ok(models) => {
            let n = models.len();
            let plural = if n == 1 { "" } else { "s" };
            let has_vision = models
                .iter()
                .any(|m| pb_app_core::describe::looks_like_vision_model(m));
            // Vision-first, newline-joined for the host to split into the Model picker.
            let list = pb_app_core::describe::sort_models_vision_first(models).join("\n");
            let message = if has_vision {
                format!("Reachable · {n} model{plural} · vision model present")
            } else {
                format!(
                    "Reachable · {n} model{plural}, but none look vision-capable — \
                     describe needs a VLM (e.g. qwen2.5-vl)."
                )
            };
            ffi::ProbeResultFfi {
                ok: has_vision,
                message,
                models: list,
            }
        }
        Err(e) => ffi::ProbeResultFfi {
            ok: false,
            message: e.user_message(),
            models: String::new(),
        },
    }
}

/// Is this path a viewable archive (`.zip` / `.7z`)? Mirrors the winit shell's helper.
fn is_archive(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("zip") || e.eq_ignore_ascii_case("7z"))
        .unwrap_or(false)
}

/// Map a core [`contract::CoreEffect`] to the FFI enum the Swift host switches on.
fn map_effect(e: contract::CoreEffect) -> ffi::CoreEffectFfi {
    use contract::CoreEffect as C;
    use ffi::CoreEffectFfi as E;
    match e {
        C::RequestRender => E::RequestRender,
        C::SetTitle(title) => E::SetTitle(title),
        C::Quit => E::Quit,
        C::SetWake(None) => E::ClearWake,
        // The wake deadline crosses as seconds-from-now (an `Instant` has no meaning across
        // the FFI): 0.0 = wake immediately. Converted at drain time — the host schedules its
        // timer in the same event turn, so the relative delay stays accurate.
        C::SetWake(Some(at)) => {
            E::SetWake(at.saturating_duration_since(Instant::now()).as_secs_f64())
        }
        // A genuinely host-side command (DeletePermanent confirm / Recursive / CancelScan /
        // Quit teardown — see `CoreEffect::ShellFlowAction`), carried by its stable snake_case
        // action id. Esc quits through THIS (the keymap resolves Escape → Action::Quit → a
        // host-side flow action), not through `CoreEffect::Quit` — the host matches "quit".
        C::ShellFlowAction(action) => E::ShellFlowAction(action.id().to_string()),
        // A user-facing error (bad open, refused archive, …) — an NSAlert once the NS2
        // dialogs land; the host logs it until then.
        C::ReportError(msg) => E::ReportError(msg),
        // The native open panels (the empty-deck buttons + the O / ⇧O keys + the future
        // File menu): the host runs an NSOpenPanel at `start_dir` and feeds the picked
        // paths back through `open_paths` — the same classify-and-open as a drop.
        C::OpenFilePanel { start_dir } => {
            E::OpenFilePanel(start_dir.to_string_lossy().into_owned())
        }
        C::OpenFolderPanel { start_dir } => {
            E::OpenFolderPanel(start_dir.to_string_lossy().into_owned())
        }
        // Pointer cursor, by kind name — the host maps to NSCursor (hover feedback for the
        // on-canvas controls, the pan hand, the hidden viewer cursor).
        C::SetCursor(kind) => {
            use contract::CursorKind as K;
            E::SetCursor(
                match kind {
                    K::Default => "default",
                    K::Hidden => "hidden",
                    K::Grab => "grab",
                    K::Grabbing => "grabbing",
                    K::Pointer => "pointer",
                }
                .to_string(),
            )
        }
        // Reveal in Finder (File ▸ Show in Finder / the context menu) — NSWorkspace.
        C::RevealPath(path) => E::RevealPath(path.to_string_lossy().into_owned()),
        // The Live Photo's audio track (its companion .mov) — the host owns AVAudioPlayer.
        C::StartLiveAudio { path, at_secs } => {
            E::StartLiveAudio(path.to_string_lossy().into_owned(), at_secs)
        }
        C::StopLiveAudio => E::StopLiveAudio,
        C::PauseLiveAudio => E::PauseLiveAudio,
        C::ResumeLiveAudio => E::ResumeLiveAudio,
        // macOS native video (task 79.9): the shell owns AVPlayer + AVPlayerLayer.
        C::PlayVideo {
            path,
            session_id,
            muted,
        } => E::PlayVideo(path.to_string_lossy().into_owned(), session_id.0, muted),
        C::StopVideo { session_id } => E::StopVideo(session_id.0),
        // The borderless fullscreen speed mode (F) ↔ windowed. `true` = fullscreen.
        C::SetWindowMode(mode) => {
            E::SetWindowMode(matches!(mode, contract::WindowMode::Fullscreen))
        }
        // Esc-teardown step 1: hide before quitting so nothing flashes.
        C::HideWindow => E::HideWindow,
        // A chrome dialog, by kind name. "about" maps to the standard NSApplication
        // about panel (ADR-021's resolved choice); the rest arrive with the NS2 dialogs.
        C::ShowDialog(kind) => {
            use contract::DialogKind as D;
            E::ShowDialog(
                match kind {
                    D::About => "about",
                    D::Settings => "settings",
                    D::Confirm => "confirm",
                    D::Message => "message",
                    D::Password => "password",
                    D::AskImage => "ask_image",
                    D::Loading => "loading",
                    D::Scanning => "scanning",
                }
                .to_string(),
            )
        }
        // The right-click photo context menu (task #41): the host builds the popup from
        // these flags (has_image, has_motion, can_reveal, fullscreen).
        C::ShowContextMenu(s) => E::ShowContextMenu(
            s.has_image,
            s.has_motion,
            s.can_reveal,
            s.fullscreen,
            s.compare_pinned,
            s.compare_pinned_here,
        ),
        // The password sheet's "Checking…" state while a submitted entry re-opens the
        // archive. (CloseDialog is handled in `next_effect` — it updates the shown-dialog
        // mirror there.)
        C::SetDialogChecking => E::SetDialogChecking,
        _ => E::Other,
    }
}

// NOTE: inside `#[swift_bridge::bridge]`, use `//` comments only — a `///` doc comment
// becomes a `#[doc]` attribute that swift-bridge-ir's parser rejects (panics in codegen).
#[swift_bridge::bridge]
mod ffi {
    // The subset of `CoreEffect` bridged so far (NS1 slice 1). It grows as each effect is
    // wired to a native handler; anything not yet mapped arrives as `Other`.
    enum CoreEffectFfi {
        RequestRender,
        SetTitle(String),
        // Tick again in this many seconds (0.0 = now): held-key pacing, slideshow dwell,
        // the animation's next frame. ClearWake = go idle until the next real event.
        SetWake(f64),
        ClearWake,
        Quit,
        // A host-side flow command, by stable Action id — in practice only "quit"
        // reaches Swift (recursive / cancel_scan / delete_permanent are intercepted
        // Rust-side; delete opens the confirm via ShowDialog).
        ShellFlowAction(String),
        // A user-facing error message — the host presents it natively (NSAlert).
        ReportError(String),
        // Run the native NSOpenPanel at this start directory; picked paths return
        // via open_paths (files+archives / folders respectively).
        OpenFilePanel(String),
        OpenFolderPanel(String),
        // Pointer cursor by kind: "default" | "hidden" | "grab" | "grabbing" | "pointer".
        SetCursor(String),
        // A clipboard write is pending — pull it via take_clipboard_text() /
        // clipboard_image_*() + take_clipboard_image(), write to NSPasteboard, toast().
        WriteClipboard,
        // Reveal this path in Finder (select it in a new window).
        RevealPath(String),
        // Live Photo audio (path to the companion .mov, start offset in seconds).
        StartLiveAudio(String, f64),
        StopLiveAudio,
        PauseLiveAudio,
        ResumeLiveAudio,
        // macOS native video (task 79.9): the whole media pipeline is the host's
        // AVPlayer + AVPlayerLayer. PlayVideo(path, session_id, muted) opens the clip
        // and presents it over the Metal canvas (revealed on the first frame; the
        // poster shows until then). StopVideo(session_id) tears the player down
        // (navigate / delete / failure); stale callbacks are rejected by session id.
        PlayVideo(String, u64, bool),
        StopVideo(u64),
        // true = enter the borderless fullscreen speed mode; false = restore windowed.
        SetWindowMode(bool),
        // Hide the window (the Esc-teardown step before Quit).
        HideWindow,
        // The menu check/enabled state changed — pull the new one via menu_state().
        MenuStateChanged,
        // A natively-presented rich panel (Help) changed visibility/content — call
        // help_refresh() then read help_visible() + help_row_* and update the view.
        PanelsChanged,
        // Pop the photo context menu at the cursor: (has_image, has_motion, can_reveal,
        // fullscreen) — the curated item set mirrors menu.rs build_context_menu.
        ShowContextMenu(bool, bool, bool, bool, bool, bool),
        // Present a chrome dialog by kind ("about" | "settings" | "confirm" | "message" |
        // "password" | "loading" | "scanning"). "about" = the standard NSApp panel;
        // "settings" opens the Settings window; the rest carry their text via
        // dialog_message() (+ dialog_password_error() for "password") — pull right after
        // the marker, then present. Re-delivery of the SAME kind updates the sheet in
        // place (a scanning re-point, a wrong-password retry).
        ShowDialog(String),
        // Dismiss the shown dialog/sheet (the user's answer was processed, the scan/open
        // finished or failed, …). Also clears the "checking" state.
        CloseDialog,
        // The password sheet's "Checking…" state: a submitted entry is being verified
        // against the archive (disable the field + Unlock until the next ShowDialog/
        // CloseDialog resolves it).
        SetDialogChecking,
        Other,
    }

    // The saved windowed geometry (settings.window): physical px, top-left
    // virtual-desktop origin (winit's convention — shared with the egui build's writes).
    #[swift_bridge(swift_repr = "struct")]
    struct WindowGeometryFfi {
        present: bool,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    }

    // A live progress snapshot for the shown dialog — poll via dialog_progress() each
    // pump while a Loading (fraction 0..1) or Scanning (found + current_dir) sheet is up.
    #[swift_bridge(swift_repr = "struct")]
    struct DialogProgressFfi {
        fraction: f32,
        found: u64,
        current_dir: String,
    }

    // The Settings window's flat form — a mirror of pb_app_core::settings::Settings
    // (NS2 item 5). Encodings: scroll_action 0 pan / 1 zoom; scale_mode 0 fit / 1 fill /
    // 2 original; appearance_mode 0 system / 1 light / 2 dark (#46); startup_mode
    // 0 fullscreen / 1 windowed / 2 remember. max_fps rides in [1, refresh_hz]; at the
    // ceiling it means "uncapped" (stored 0). refresh_hz is out-only (the slider
    // ceiling); picker_dir "" = none. letterbox is the dark-theme fill, letterbox_light
    // the light-theme one (#46).
    #[swift_bridge(swift_repr = "struct")]
    struct SettingsFormFfi {
        start_speed: f32,
        ramp_secs: f32,
        max_fps: u32,
        refresh_hz: u32,
        hold_delay_ms: u32,
        scroll_action: u8,
        recursive: bool,
        scale_mode: u8,
        appearance_mode: u8,
        // 0 left / 1 center / 2 right (task #54).
        info_line_align: u8,
        // Info line: launch default + which fields show (task #54).
        show_image_info: bool,
        // Transparent toolbar (task #59): photo extends under a translucent glass toolbar.
        glass_toolbar: bool,
        info_show_folder: bool,
        info_show_filename: bool,
        info_show_resolution: bool,
        info_show_codec: bool,
        letterbox_r: u8,
        letterbox_g: u8,
        letterbox_b: u8,
        letterbox_light_r: u8,
        letterbox_light_g: u8,
        letterbox_light_b: u8,
        info_opacity: u8,
        panel_opacity: u8,
        startup_mode: u8,
        slideshow_interval_secs: f64,
        picker_fixed: bool,
        picker_dir: String,
        mute_live_audio: bool,
        // AI image description (task #44). `describe_backend`: 0 Auto / 1 Apple / 2 Local.
        // `describe_prompt` empty = the built-in instruction.
        describe_backend: u8,
        describe_endpoint: String,
        describe_model: String,
        describe_prompt: String,
        describe_max_tokens: u32,
        describe_auto: bool,
        speak_descriptions: bool,
    }

    // The Test-connection result for the AI settings tab: reachability + a one-line
    // summary (model count, or the reason it failed). `ok` colors the line. `models` is the
    // served model ids (vision-capable first), newline-joined — the host splits it to fill
    // the Model picker (a plain String avoids a Vec across swift-bridge).
    #[swift_bridge(swift_repr = "struct")]
    struct ProbeResultFfi {
        ok: bool,
        message: String,
        models: String,
    }

    // The launch-preflight CLI parse outcome (task #78) — see `cli_preflight`.
    // proceed=false means: write `text` to stdout/stderr (per `use_stderr`) when the
    // process has a shell, alert only for a real error on a GUI launch, exit(exit_code).
    #[swift_bridge(swift_repr = "struct")]
    struct LaunchPreflightFfi {
        proceed: bool,
        text: String,
        use_stderr: bool,
        exit_code: i32,
    }

    // The native menu's check/enabled state — the mirror of contract::MenuState (scale:
    // 0 fit / 1 fill / 2 original; info: 0 hidden / 1 basic / 2 full-exif).
    #[swift_bridge(swift_repr = "struct")]
    struct MenuStateFfi {
        scale: u8,
        info_basic: bool,
        info_full: bool,
        panels_hidden: bool,
        hide_panels_enabled: bool,
        recursive: bool,
        fullscreen: bool,
        slideshow: bool,
        mute_live_audio: bool,
        compare_pin_enabled: bool,
        compare_pinned_here: bool,
        compare_toggle_enabled: bool,
        save_rotation_enabled: bool,
        reveal_enabled: bool,
        cancel_scan_enabled: bool,
        undo_enabled: bool,
        undo_label: String,
    }

    extern "Rust" {
        type AppCoreHandle;

        #[swift_bridge(init)]
        fn new(width: u32, height: u32, scale: f32) -> AppCoreHandle;

        fn key_down(
            &mut self,
            key: &str,
            ctrl: bool,
            shift: bool,
            alt: bool,
            logo: bool,
            is_repeat: bool,
        );
        fn key_up(&mut self, key: &str);
        fn focus_lost(&mut self);
        fn os_theme_changed(&mut self, dark: bool);
        fn tick(&mut self);
        fn next_effect(&mut self) -> Option<CoreEffectFfi>;
        fn open_path(&mut self, path: &str);
        fn open_paths(&mut self, paths: Vec<String>);

        // Pointer + gestures (NS1 item 4) — same conventions as the winit shell:
        // physical px, top-left origin; pixel scrolls scaled by the backing factor.
        fn pointer_moved(&mut self, x: f32, y: f32);
        fn mouse_left(&mut self, pressed: bool);
        fn scroll_lines(&mut self, x: f32, y: f32);
        fn scroll_pixels(&mut self, x: f32, y: f32);
        fn pinch(&mut self, delta: f32);
        fn double_tap(&mut self);
        fn work_pending(&self) -> bool;
        fn toast(&mut self, msg: &str);
        fn current_photo_path(&self) -> String;

        // The WriteClipboard payload accessors (marker effect + pull — see the field doc).
        // clipboard_text_toast BEFORE take_clipboard_text (the take consumes both).
        fn clipboard_text_toast(&self) -> String;
        fn take_clipboard_text(&mut self) -> String;
        fn clipboard_image_width(&self) -> u32;
        fn clipboard_image_height(&self) -> u32;
        fn clipboard_image_file(&self) -> String;
        fn take_clipboard_image(&mut self) -> Vec<u8>;

        // The native menu (NS1 item 8): clicks in by Action id, state out by pull.
        fn menu_action(&mut self, id: &str);
        // Toolbar hold-to-fly (task #55): press-and-hold a nav/random button to blaze.
        fn begin_pointer_nav(&mut self, action_id: &str);
        fn end_pointer_nav(&mut self);
        fn menu_state(&self) -> MenuStateFfi;
        // The live slideshow interval, formatted for the macOS toolbar (task #55).
        fn slideshow_interval_display(&self) -> String;
        // Translucent glass-toolbar spike (task #59): the top inset the photo fits under.
        fn set_content_top_inset(&mut self, px: u32);
        // Motion state for the toolbar's Play-Animation button (task #55).
        fn current_has_motion(&mut self) -> bool;
        fn animation_playing(&self) -> bool;
        fn motion_playing(&self) -> bool;
        fn context_menu(&mut self);

        // The native Help panel (task #54, mac-first): on a PanelsChanged marker call
        // help_refresh(), then read help_visible() and the indexed help_row_* accessors
        // to (re)build the SwiftUI Help view.
        fn help_refresh(&mut self);
        fn help_visible(&self) -> bool;
        fn help_row_count(&self) -> usize;
        fn help_row_is_header(&self, i: usize) -> bool;
        fn help_row_text(&self, i: usize) -> String;
        fn help_row_shortcut(&self, i: usize) -> String;

        // The native empty-state Open panel (task #54): its visibility, plus a generic
        // shortcut lookup by Action id for the welcome surface's tips.
        fn open_panel_visible(&self) -> bool;
        fn action_shortcut(&self, id: &str) -> String;

        // The native Inspector (Details / Text / Describe tabs, task #54).
        fn inspector_visible(&self) -> bool;
        fn inspector_tab(&self) -> u8;
        fn inspector_show_tab(&mut self, tab: u8);
        fn inspector_close(&mut self);
        fn inspector_refresh(&mut self);
        fn inspector_row_count(&self) -> usize;
        fn inspector_row_kind(&self, i: usize) -> u8;
        fn inspector_row_a(&self, i: usize) -> String;
        fn inspector_row_b(&self, i: usize) -> String;

        // The native folder tree (⇧F, task #54) — Finder browser (disk) / v1 flat (archive).
        fn tree_visible(&self) -> bool;
        fn tree_uses_fs(&self) -> bool;
        fn tree_refresh(&mut self);
        fn tree_row_count(&self) -> usize;
        fn tree_row_depth(&self, i: usize) -> u32;
        fn tree_row_name(&self, i: usize) -> String;
        fn tree_row_is_current(&self, i: usize) -> bool;
        fn tree_row_is_up(&self, i: usize) -> bool;
        fn tree_row_has_children(&self, i: usize) -> bool;
        fn tree_row_expanded(&self, i: usize) -> bool;
        fn tree_row_loading(&self, i: usize) -> bool;
        fn tree_row_count_badge(&self, i: usize) -> i64;
        fn tree_row_has_target(&self, i: usize) -> bool;
        fn tree_current_path(&self) -> String;
        fn tree_activate(&mut self, i: usize);
        fn tree_toggle(&mut self, i: usize);

        // The NS2 dialog seam: payload pulls (after a ShowDialog marker), the
        // DialogResolved results (one entry point per user gesture), the live progress
        // snapshot, and the Settings form round-trip.
        fn dialog_message(&self) -> String;
        fn dialog_password_error(&self) -> String;
        fn dialog_dismissed(&mut self);
        fn dialog_closed(&mut self);
        fn dialog_confirm_answered(&mut self, confirmed: bool);
        fn password_submitted(&mut self, password: String);
        fn password_cancelled(&mut self);
        fn ask_submitted(&mut self, question: String);
        fn loading_cancelled(&mut self);
        fn scanning_cancelled(&mut self);
        fn scan_pill_visible(&self) -> bool;
        fn scan_pill_name(&self) -> String;
        fn scan_pill_found(&self) -> i64;
        fn scan_pill_current(&self) -> String;
        fn scan_pill_cancel(&mut self);
        fn settings_closed(&mut self);
        fn dialog_progress(&self) -> DialogProgressFfi;
        fn panel_opacity(&self) -> u8;
        fn toast_visible(&self) -> bool;
        fn toast_message(&self) -> String;
        fn toast_icon(&self) -> u8;
        fn toast_seq(&self) -> u64;
        fn info_line_visible(&self) -> bool;
        fn info_line_text(&self) -> String;
        fn info_line_codec(&self) -> String;
        fn info_line_is_live(&self) -> bool;
        fn info_line_is_animated(&self) -> bool;
        fn info_line_is_video(&self) -> bool;
        fn info_line_align(&self) -> u8;
        fn play_hint_kind(&self) -> u8;
        fn play_hint_seq(&self) -> u64;
        fn settings_form(&self) -> SettingsFormFfi;
        fn settings_edited(&mut self, form: SettingsFormFfi);

        // The AI tab's Test-connection probe (task #44). A free function — stateless (just
        // an HTTP GET /models), so it's safe to call from a Swift background task without
        // touching the core. Blocking; the caller runs it off the main thread.
        fn probe_describe_endpoint(url: String) -> ProbeResultFfi;

        // The CLI (task #78). cli_preflight runs FIRST (free fn — no engine for --help);
        // apply_launch_args re-parses on the built handle (overrides + path stash);
        // open_launch_paths consumes the stash once the canvas exists (consumed-once).
        fn cli_preflight(
            argv: Vec<String>,
            version: String,
            stdout_tty: bool,
            stderr_tty: bool,
        ) -> LaunchPreflightFfi;
        fn apply_launch_args(&mut self, argv: Vec<String>, version: String);
        fn open_launch_paths(&mut self) -> bool;

        // The Shortcuts editor (NS2.6): a Rust-side draft keymap; rows by (group, index),
        // edits by stable action id; chords display as macOS glyphs.
        fn keymap_begin_edit(&mut self);
        fn keymap_group_count(&self) -> usize;
        fn keymap_group_title(&self, group: usize) -> String;
        fn keymap_group_len(&self, group: usize) -> usize;
        fn keymap_action_id(&self, group: usize, index: usize) -> String;
        fn keymap_action_label(&self, group: usize, index: usize) -> String;
        fn keymap_slot_display(&self, action_id: &str, slot: usize) -> String;
        fn keymap_menu_chord(&self, action_id: &str) -> String;
        fn keymap_capture(
            &mut self,
            action_id: &str,
            slot: usize,
            key: &str,
            ctrl: bool,
            shift: bool,
            alt: bool,
            logo: bool,
        ) -> bool;
        fn keymap_last_note(&self) -> String;
        fn keymap_clear_slot(&mut self, action_id: &str, slot: usize);
        fn keymap_reset_defaults(&mut self);
        fn keymap_is_dirty(&self) -> bool;
        fn keymap_commit(&mut self);

        // Startup window state + geometry persistence (finalize item 2).
        fn startup_fullscreen(&mut self) -> bool;
        // The live appearance (saved preference, or the --theme launch override):
        // 0 system / 1 light / 2 dark — what applyAppearancePreference wears.
        fn effective_appearance(&self) -> u8;
        fn saved_geometry(&self) -> WindowGeometryFfi;
        fn note_window_geometry(&mut self, x: i32, y: i32, w: u32, h: u32);

        // The canvas surface (NS1 item 2). `layer_ptr` = the retained CAMetalLayer's
        // pointer bits (swift-bridge has no raw-pointer type; usize crosses as UInt).
        fn attach_layer(&mut self, layer_ptr: usize, width: u32, height: u32, scale: f32);
        fn detach_layer(&mut self);
        fn resized(&mut self, width: u32, height: u32, scale: f32);
        fn render(&mut self);
        fn wants_edr(&self) -> bool;
        fn set_edr_headroom(&mut self, headroom: f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FFI constructor with test hygiene applied: `AppCoreHandle::new` builds a real
    /// host (`persist_prefs = true`), so a test that opens a deck would write the
    /// developer's actual settings.toml (`last_folder`). Every test constructs through
    /// here instead.
    fn test_handle(width: u32, height: u32, scale: f32) -> AppCoreHandle {
        let mut h = AppCoreHandle::new(width, height, scale);
        h.core.persist_prefs = false;
        h
    }

    /// Pull the queue dry, as the Swift host's drain loop does.
    fn drain(h: &mut AppCoreHandle) -> Vec<ffi::CoreEffectFfi> {
        std::iter::from_fn(|| h.next_effect()).collect()
    }

    /// Slice-1 proof: an event driven in through the FFI produces effects the host can pull
    /// out — the full round-trip through the real `AppCore::handle`, and the queue is emptied
    /// by a drain. Escape resolves (default keymap) to `Action::Quit`, a HOST-side flow
    /// command — so the drain must contain `ShellFlowAction("quit")` (NOT `CoreEffect::Quit`;
    /// the host runs the quit teardown), even on a headless core with no photos.
    #[test]
    fn event_in_produces_effects_out() {
        let mut h = test_handle(1920, 1080, 2.0);
        h.key_down("Escape", false, false, false, false, false);
        let effects = drain(&mut h);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, ffi::CoreEffectFfi::ShellFlowAction(id) if id == "quit")),
            "Escape should resolve to the host-side quit flow action"
        );
        assert!(
            drain(&mut h).is_empty(),
            "draining empties the effect queue"
        );
    }

    /// An unknown key name is ignored (no panic, no effects) — the host can be liberal in
    /// what it forwards.
    #[test]
    fn unknown_key_name_is_ignored() {
        let mut h = test_handle(800, 600, 1.0);
        h.key_down("NotAKey", false, false, false, false, false);
        assert!(drain(&mut h).is_empty());
    }

    /// The launch-preflight outcome mapping (task #78): help/version/usage-error/missing
    /// path each produce the right (proceed, stream, exit code) triple — and the argv[0]
    /// regression guard: clap consumes element 0 as the program name, so the first REAL
    /// flag must still be seen.
    #[test]
    fn cli_preflight_outcomes() {
        // Piped/redirected streams (both flags false) — the plain-text render.
        let pf = |args: &[&str]| {
            cli_preflight(
                args.iter().map(|s| s.to_string()).collect(),
                "9.9.9-test".to_string(),
                false,
                false,
            )
        };
        // argv[0] regression: --help is argv[1] and must parse as help, not the bin name.
        let help = pf(&["photoblaze", "--help"]);
        assert!(!help.proceed);
        assert_eq!(help.exit_code, 0);
        assert!(!help.use_stderr, "help goes to stdout");
        assert!(help.text.contains("--slideshow"), "renders the real help");
        assert!(
            !help.text.contains('\u{1b}'),
            "no ANSI styling into a pipe/redirect"
        );

        // A terminal stdout gets the colored help (the styling Windows shows).
        let colored = cli_preflight(
            vec!["photoblaze".into(), "--help".into()],
            "9.9.9-test".into(),
            true,
            false,
        );
        assert!(
            colored.text.contains('\u{1b}'),
            "TTY stdout renders ANSI-styled help"
        );

        let ver = pf(&["photoblaze", "--version"]);
        assert!(!ver.proceed);
        assert_eq!(ver.exit_code, 0);
        assert!(
            ver.text.contains("9.9.9-test"),
            "--version prints the host-supplied bundle string"
        );
        assert!(
            ver.text.contains("PhotoBlaze"),
            "--version wears the product name (display_name), not the bin name"
        );

        let bad = pf(&["photoblaze", "--nope"]);
        assert!(!bad.proceed);
        assert_eq!(bad.exit_code, 2);
        assert!(bad.use_stderr, "usage errors go to stderr");

        // Mixed strictness: a nonexistent path is a usage error with the winit shell's
        // exact message (argv[0] regression for positionals too — the path is argv[1]).
        let missing = pf(&["photoblaze", "/definitely/not/here.jpg"]);
        assert!(!missing.proceed);
        assert_eq!(missing.exit_code, 2);
        assert!(missing.use_stderr);
        assert!(missing
            .text
            .contains("no such file or folder: /definitely/not/here.jpg"));

        // A clean flag-only launch proceeds with no text.
        let ok = pf(&["photoblaze", "--shuffle", "--theme", "dark"]);
        assert!(ok.proceed);
        assert!(ok.text.is_empty());
        assert_eq!(ok.exit_code, 0);
    }

    /// `apply_launch_args` applies the session overrides to the core (theme folds into
    /// `effective_appearance`, --shuffle into the launch nav) and stashes the paths —
    /// including the hidden `--pb-open` back-compat alias.
    #[test]
    fn apply_launch_args_applies_overrides_and_stashes_paths() {
        let mut h = test_handle(800, 600, 1.0);
        // The handle loads the developer's real settings; snapshot to prove the override
        // never lands there (whatever the saved values are).
        let saved_appearance = h.core.settings.appearance_mode;
        let saved_mute = h.core.settings.mute_live_audio;
        let dir = std::env::temp_dir();
        h.apply_launch_args(
            vec![
                "photoblaze".into(),
                "--theme".into(),
                "dark".into(),
                "--shuffle".into(),
                "--mute".into(),
                "--pb-open".into(),
                dir.to_string_lossy().into_owned(),
            ],
            "9.9.9-test".into(),
        );
        assert_eq!(
            h.core.effective_appearance(),
            pb_app_core::settings::AppearanceMode::Dark,
            "--theme is a live override, not a settings write"
        );
        assert!(h.core.effective_mute(), "--mute folds into effective_mute");
        assert_eq!(h.core.last_nav, pb_app_core::Nav::Random);
        assert_eq!(
            h.pending_launch_paths,
            vec![dir.to_string_lossy().into_owned()]
        );
        // And the no-trace guarantee: the raw settings were not mutated.
        assert_eq!(
            h.core.settings.appearance_mode, saved_appearance,
            "overrides never land in Settings"
        );
        assert_eq!(h.core.settings.mute_live_audio, saved_mute);
    }

    /// A `--windowed` / `--fullscreen` launch override wins over the saved startup mode
    /// (deterministic in both directions, whatever the developer's real settings say),
    /// and the `effective_appearance` accessor folds a `--theme` override in the
    /// settings-form encoding (0 system / 1 light / 2 dark).
    #[test]
    fn launch_overrides_fold_into_startup_reads() {
        let mut h = test_handle(800, 600, 1.0);
        h.apply_launch_args(
            vec![
                "photoblaze".into(),
                "--fullscreen".into(),
                "--theme".into(),
                "dark".into(),
            ],
            "v".into(),
        );
        assert!(
            h.startup_fullscreen(),
            "--fullscreen wins over the saved mode"
        );
        assert!(!h.core.windowed, "the windowed mirror follows");
        assert_eq!(h.effective_appearance(), 2, "--theme dark reads as 2");

        let mut w = test_handle(800, 600, 1.0);
        w.apply_launch_args(
            vec![
                "photoblaze".into(),
                "-w".into(),
                "--theme".into(),
                "light".into(),
            ],
            "v".into(),
        );
        assert!(
            !w.startup_fullscreen(),
            "--windowed wins over the saved mode"
        );
        assert!(w.core.windowed);
        assert_eq!(w.effective_appearance(), 1);

        // No override: the accessor mirrors the saved preference's encoding.
        let plain = test_handle(800, 600, 1.0);
        let saved = match plain.core.settings.appearance_mode {
            pb_app_core::settings::AppearanceMode::System => 0,
            pb_app_core::settings::AppearanceMode::Light => 1,
            pb_app_core::settings::AppearanceMode::Dark => 2,
        };
        assert_eq!(plain.effective_appearance(), saved);
    }

    /// `open_launch_paths` consumes the stash exactly once — the idempotence guard the
    /// bare-path double-delivery arbitration leans on. (No drain: the BeginDirScan effect
    /// stays queued, so no walker thread is spawned in the test.)
    #[test]
    fn open_launch_paths_is_consumed_once() {
        let mut h = test_handle(800, 600, 1.0);
        let dir = std::env::temp_dir().to_string_lossy().into_owned();
        h.apply_launch_args(
            vec!["photoblaze".into(), "--pb-open".into(), dir],
            "v".into(),
        );
        assert!(h.open_launch_paths(), "first call opens the stashed paths");
        assert!(!h.open_launch_paths(), "second call is a no-op");
        // With nothing stashed at all, it is also a no-op.
        let mut empty = test_handle(800, 600, 1.0);
        empty.apply_launch_args(vec!["photoblaze".into()], "v".into());
        assert!(!empty.open_launch_paths());
    }

    /// NS1 item 3 end-to-end (no Swift, no GPU): `open_path` on a real folder → the core's
    /// `BeginDirScan` effect is intercepted by the drain → the worker thread streams the
    /// walk → `tick` polls the batches into `ScanBatch`/`ScanDone` → the playlist
    /// bootstraps on the fixture image. The whole open flow, driven exactly as the Swift
    /// host drives it (open_path + a tick/drain loop).
    #[test]
    fn open_path_scans_a_folder_and_bootstraps_the_playlist() {
        const FIXTURE: &[u8] = include_bytes!("../../pb-app-core/tests/fixtures/orient6.jpg");
        let dir = std::env::temp_dir().join(format!("pb-mac-ffi-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("one.jpg"), FIXTURE).unwrap();

        let mut h = test_handle(800, 600, 1.0);
        h.open_path(dir.to_str().unwrap());
        let _ = drain(&mut h); // executes the intercepted BeginDirScan (spawns the worker)
        assert!(h.dir_scan.is_some(), "the scan worker should be in flight");

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while h.core.playlist.current().is_none() && Instant::now() < deadline {
            h.tick(); // polls the worker + applies ScanBatch/ScanDone, like the host's pump
            let _ = drain(&mut h);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            h.core.playlist.current(),
            Some(0),
            "the scan should bootstrap the playlist on the fixture image"
        );
        assert_eq!(h.core.source.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A Scanning dialog that revealed (slow walk, nothing on screen yet) must close the
    /// moment the first image lands — the sheet blocks every key on the Swift host, so
    /// leaving it up until `ScanDone` blocked browsing for the whole walk (the owner-
    /// reported "blocking wait spinner" regression). The chip takes over as progress.
    #[test]
    fn first_scan_batch_closes_a_revealed_scanning_dialog() {
        const FIXTURE: &[u8] = include_bytes!("../../pb-app-core/tests/fixtures/orient6.jpg");
        let dir = std::env::temp_dir().join(format!("pb-mac-ffi-reveal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("one.jpg"), FIXTURE).unwrap();

        let mut h = test_handle(800, 600, 1.0);
        h.open_path(dir.to_str().unwrap());
        let _ = drain(&mut h); // executes the intercepted BeginDirScan (spawns the worker)
                               // Pretend the walk outlasted the reveal delay before finding anything: the
                               // Scanning dialog is up, exactly as the host would show it.
        h.shown_dialog = Some(contract::DialogKind::Scanning);

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let mut effects = Vec::new();
        while !h.core.scan_bootstrapped && Instant::now() < deadline {
            h.tick();
            effects.extend(drain(&mut h));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(h.core.scan_bootstrapped, "fixture image should bootstrap");
        assert!(
            has_close(&effects),
            "the first batch must emit CloseDialog for the revealed Scanning sheet"
        );
        assert_eq!(
            h.shown_dialog, None,
            "the Scanning dialog must be gone as soon as a photo is on screen"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    fn has_dialog(effects: &[ffi::CoreEffectFfi], kind: &str) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, ffi::CoreEffectFfi::ShowDialog(k) if k == kind))
    }

    fn has_close(effects: &[ffi::CoreEffectFfi]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, ffi::CoreEffectFfi::CloseDialog))
    }

    /// Bootstrap a one-photo playlist in a temp dir (the open_path pattern), returning the
    /// handle + the photo's path.
    fn handle_with_photo(tag: &str) -> (AppCoreHandle, PathBuf) {
        const FIXTURE: &[u8] = include_bytes!("../../pb-app-core/tests/fixtures/orient6.jpg");
        let dir = std::env::temp_dir().join(format!("pb-mac-ffi-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("one.jpg");
        std::fs::write(&file, FIXTURE).unwrap();
        let mut h = test_handle(800, 600, 1.0);
        h.open_path(dir.to_str().unwrap());
        let _ = drain(&mut h);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while h.core.playlist.current().is_none() && Instant::now() < deadline {
            h.tick();
            let _ = drain(&mut h);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(h.core.playlist.current(), Some(0), "fixture bootstraps");
        (h, file)
    }

    /// NS2: DeletePermanent is intercepted Rust-side — it arms the pending item and opens
    /// the confirm dialog (question carries the file name); No keeps the file, Yes runs the
    /// permanent delete. The whole loop through `handle_dialog_resolved`, no Swift.
    #[test]
    fn startup_mode_and_geometry_notes_are_honored() {
        // ⚠ This test must never tick past the 500 ms debounce — the core's tick 4e
        // would flush settings.save() to the user's REAL settings.toml.
        let mut h = test_handle(800, 600, 1.0);

        // Remember + last-mode-fullscreen → start fullscreen; the core mirror flips
        // without a settings write.
        h.core.settings.fullscreen = true;
        h.core.settings.startup_mode = pb_app_core::settings::StartupMode::Remember;
        assert!(h.startup_fullscreen());
        assert!(!h.core.windowed);

        // Fullscreen → geometry notes are ignored (the monitor isn't a user spot).
        // (new_host loaded the REAL settings.toml, which may carry a saved geometry —
        // clear it so the assertion tests the note, not the user's config.)
        h.core.settings.window = None;
        h.note_window_geometry(1, 2, 300, 200);
        assert!(h.core.settings.window.is_none());

        // Windowed → the note lands + arms the debounced save.
        h.core.windowed = true;
        h.note_window_geometry(10, 20, 800, 600);
        let g = h.core.settings.window.expect("geometry noted");
        assert_eq!((g.x, g.y, g.w, g.h), (10, 20, 800, 600));
        assert!(h.core.geometry_save_at.is_some(), "debounced save armed");
        let out = h.saved_geometry();
        assert!(out.present);
        assert_eq!((out.x, out.y, out.w, out.h), (10, 20, 800, 600));
    }

    /// Task #42's missing persistence: the F toggle is the explicit action that persists
    /// the remembered mode + windowed geometry (the winit shell's `apply_window_mode`
    /// twin). The drain arm must surface `SetWindowMode` to the host AND disarm the
    /// debounced geometry save (it persists now — on a live host; the write itself is
    /// gated off here by `persist_prefs = false`).
    #[test]
    fn fullscreen_toggle_surfaces_set_window_mode_and_disarms_the_debounce() {
        let mut h = test_handle(800, 600, 1.0);
        h.core.windowed = true;
        h.core.settings.window = None; // new_host loaded the REAL settings.toml
        h.note_window_geometry(10, 20, 800, 600);
        assert!(h.core.geometry_save_at.is_some(), "debounced save armed");

        h.menu_action("fullscreen");
        let effects = drain(&mut h);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, ffi::CoreEffectFfi::SetWindowMode(true))),
            "the F toggle should surface SetWindowMode(fullscreen) to the host"
        );
        assert!(!h.core.windowed);
        assert!(h.core.settings.fullscreen, "the remembered last mode flips");
        assert!(
            h.core.geometry_save_at.is_none(),
            "the toggle owns the save — the debounce must not fire later"
        );
    }

    /// Owner report: "Save Rotation not implemented on the Mac version." Prove the FFI
    /// chain — rotate via the menu id, save via the menu id, and the JPEG's bytes on
    /// disk actually change (the pure core arm from NS0 5.6 writes the EXIF orientation).
    #[test]
    fn save_rotation_writes_the_exif_orientation() {
        let (mut h, file) = handle_with_photo("saverot");
        let before = std::fs::read(&file).unwrap();

        h.menu_action("rotate_cw");
        // The next tick must notice the unsaved rotation and re-mirror the menu state
        // (Save Rotation enables) — the missing half of the owner's report: the item sat
        // on MenuState::default() (disabled) because nothing ever emitted SetMenuState.
        h.tick();
        let effects = drain(&mut h);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, ffi::CoreEffectFfi::MenuStateChanged)),
            "a rotation should re-mirror the menu state"
        );
        assert!(
            h.menu_state().save_rotation_enabled,
            "Save Rotation should enable once a rotation is pending"
        );

        h.menu_action("save_rotation");
        let _ = drain(&mut h);

        let after = std::fs::read(&file).unwrap();
        assert_ne!(
            before, after,
            "the JPEG on disk should carry the new orientation"
        );
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn copy_image_details_lands_exif_text_on_the_clipboard_seam() {
        // Owner report "doesn't seem to be implemented" — prove the whole chain:
        // menu id → dispatch → core copy_image_details → WriteClipboard marker →
        // the pull accessor the Swift pasteboard writer uses.
        let (mut h, file) = handle_with_photo("copydetails");
        h.menu_action("copy_image_details");
        let effects = drain(&mut h);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, ffi::CoreEffectFfi::WriteClipboard)),
            "copy_image_details should emit the clipboard marker"
        );
        let text = h.take_clipboard_text();
        assert!(text.contains("one.jpg"), "filename line present: {text:?}");
        assert!(text.contains("Dimensions:"), "metadata present: {text:?}");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn delete_permanent_confirms_then_deletes() {
        let (mut h, file) = handle_with_photo("delete");

        h.menu_action("delete_permanent");
        let effects = drain(&mut h);
        assert!(has_dialog(&effects, "confirm"), "confirm dialog opens");
        assert!(
            h.dialog_message().contains("one.jpg"),
            "the question names the file: {:?}",
            h.dialog_message()
        );

        h.dialog_confirm_answered(false);
        let effects = drain(&mut h);
        assert!(has_close(&effects), "No closes the dialog");
        assert!(file.exists(), "No leaves the file alone");

        h.menu_action("delete_permanent");
        let _ = drain(&mut h);
        h.dialog_confirm_answered(true);
        let _ = drain(&mut h);
        assert!(!file.exists(), "Yes permanently deletes the file");

        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// The proxy-icon source: a real on-disk photo reports its path; an empty deck (and an
    /// archive entry — no file) reports "".
    #[test]
    fn current_photo_path_names_the_displayed_file() {
        let h = test_handle(800, 600, 1.0);
        assert_eq!(h.current_photo_path(), "", "empty deck → no proxy icon");

        let (h, file) = handle_with_photo("proxy");
        assert_eq!(h.current_photo_path(), file.to_string_lossy());
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// NS2: the password flow — PasswordRequired prompts (message names the archive), a
    /// wrong attempt re-prompts in place with the inline error, and a submitted entry
    /// drives Checking + the re-open (which fails here — the path doesn't exist — so the
    /// prompt closes and the error surfaces natively).
    #[test]
    fn password_required_prompts_and_a_submit_rechecks() {
        let mut h = test_handle(800, 600, 1.0);
        let path = PathBuf::from("/nonexistent/locked.7z");

        h.finish_archive_open(Err(ArchiveOpenError::PasswordRequired), false, path.clone());
        let effects = drain(&mut h);
        assert!(has_dialog(&effects, "password"));
        assert!(h.dialog_message().contains("locked.7z"));
        assert!(
            h.dialog_password_error().is_empty(),
            "fresh prompt, no error"
        );
        assert_eq!(h.core.password_archive.as_deref(), Some(path.as_path()));

        // A wrong attempt (was_password_attempt) re-prompts with the inline error.
        h.finish_archive_open(Err(ArchiveOpenError::PasswordRequired), true, path.clone());
        let effects = drain(&mut h);
        assert!(has_dialog(&effects, "password"));
        assert!(
            !h.dialog_password_error().is_empty(),
            "retry shows the error"
        );

        // Submit → Checking + BeginArchiveOpen (intercepted; the bogus path errors out,
        // which closes the prompt and reports).
        h.password_submitted("hunter2".to_string());
        let effects = drain(&mut h);
        assert!(effects
            .iter()
            .any(|e| matches!(e, ffi::CoreEffectFfi::SetDialogChecking)));
        assert!(has_close(&effects));
        assert!(effects
            .iter()
            .any(|e| matches!(e, ffi::CoreEffectFfi::ReportError(_))));
        assert!(
            h.core.password_archive.is_none(),
            "failed open forgets the archive"
        );
    }

    /// NS2: Esc on a shown dialog routes through the core's Dismissed reaction — the
    /// dialog closes, the mirrors clear, and the esc-guard arms (so the Esc can't leak
    /// into quit).
    #[test]
    fn dismissing_the_shown_dialog_closes_and_guards() {
        let mut h = test_handle(800, 600, 1.0);
        h.shown_dialog = Some(contract::DialogKind::Scanning);
        h.core.dialog_open = true;

        h.dialog_dismissed();
        let effects = drain(&mut h);
        assert!(has_close(&effects));
        assert!(h.shown_dialog.is_none());
        assert!(!h.core.dialog_open);
        assert!(h.core.esc_guard_until.is_some(), "esc-guard armed");
    }

    /// NS2.6: the Shortcuts-editor draft — capture binds (with the steal note when the
    /// chord had another owner), clear unbinds, dirty tracks edits, and none of it
    /// touches the live keymap until `keymap_commit`. Pure draft ops — no keymap.toml
    /// I/O (the commit itself applies + persists REAL config, so it's never exercised
    /// here; only its clean no-op path is).
    #[test]
    fn keymap_draft_captures_steals_and_clears() {
        let mut h = test_handle(800, 600, 1.0);
        h.keymap_begin_edit();
        assert!(!h.keymap_is_dirty());
        assert!(h.keymap_group_count() > 0);
        assert_eq!(h.keymap_group_title(0), "Navigation");
        assert_eq!(h.keymap_action_id(0, 0), "next");

        // Hermetic: the draft opens as a clone of the LIVE keymap — on a dev machine
        // that's the real keymap.toml, which the auto-saving editor now rewrites
        // freely. Reset the draft to the built-in defaults so the steal/clear
        // assertions never depend on whatever the developer last bound in the app.
        h.keymap_reset_defaults();
        let next = Action::from_id("next").unwrap();
        let live_next_before = h.core.keymap.slot(next, 0);

        // Space is Next's default primary → binding it to Prev steals it (note names Next).
        assert!(h.keymap_capture("prev", 0, "Space", false, false, false, false));
        assert!(h.keymap_is_dirty());
        assert!(
            h.keymap_last_note().contains("Next"),
            "steal note names the prior owner: {:?}",
            h.keymap_last_note()
        );
        assert_eq!(h.keymap_slot_display("prev", 0), "Space");

        // Unknown key names are refused (host stays armed) and don't dirty further state.
        assert!(!h.keymap_capture("prev", 1, "NotAKey", false, false, false, false));

        h.keymap_clear_slot("prev", 0);
        assert_eq!(h.keymap_slot_display("prev", 0), "");

        // The LIVE keymap is untouched throughout (draft-only until commit).
        assert_eq!(h.core.keymap.slot(next, 0), live_next_before);

        // Reset restores defaults in the draft.
        h.keymap_reset_defaults();
        assert_eq!(h.keymap_slot_display("next", 0), "Space");
        assert!(h.keymap_is_dirty());
    }

    /// Auto-save guards: an unchanged Settings form and a clean Shortcuts draft are hard
    /// no-ops — no `SettingsEdited` event, no effects, nothing applied. This is what keeps
    /// the window's initial `onChange` echo and the close-time flush from touching disk.
    /// (The *changed* path applies + persists the REAL settings.toml/keymap.toml, so it's
    /// deliberately not exercised end-to-end; the fold itself is covered below.)
    #[test]
    fn unchanged_settings_and_clean_keymap_commit_are_noops() {
        let mut h = test_handle(800, 600, 1.0);
        // Pin known-good settings so the form→fold round-trip is exact regardless of
        // whatever the dev machine's real settings.toml contains.
        h.core.settings = pb_app_core::settings::Settings::default();
        let before = h.core.settings.clone();

        h.settings_edited(h.settings_form());
        assert_eq!(h.core.settings, before, "unchanged form applies nothing");
        assert!(drain(&mut h).is_empty(), "unchanged form emits no effects");

        h.keymap_begin_edit();
        h.keymap_commit(); // not dirty → no-op
        assert!(
            drain(&mut h).is_empty(),
            "clean draft commit emits no effects"
        );
        assert!(
            h.keymap_draft.is_some(),
            "draft stays open for further edits"
        );
    }

    /// NS2 item 5: the Settings form fold — encodings map both ways, out-of-range values
    /// clamp, the max-speed ceiling means "uncapped", and fields the form doesn't expose
    /// (the remembered fullscreen state) are preserved. Pure — never touches settings.toml.
    #[test]
    fn settings_form_folds_back_with_clamps() {
        use pb_app_core::settings::ScrollAction;
        let h = test_handle(800, 600, 1.0);
        let mut form = h.settings_form();

        form.recursive = !form.recursive;
        form.start_speed = 999.0; // clamps to 60
        form.scroll_action = 1; // zoom
        form.max_fps = form.refresh_hz; // ceiling = uncapped
        form.picker_fixed = true;
        form.picker_dir = "/photos".to_string();

        let folded = fold_settings_form(&h.core.settings, &form, form.refresh_hz);
        assert_eq!(folded.start_speed, 60.0);
        assert_eq!(folded.scroll_action, ScrollAction::Zoom);
        assert_eq!(folded.max_advance_rate, 0, "slider ceiling = uncapped");
        assert_eq!(folded.picker_dir.as_deref(), Some(Path::new("/photos")));
        assert_ne!(folded.recursive, h.core.settings.recursive);
        assert_eq!(
            folded.fullscreen, h.core.settings.fullscreen,
            "unexposed field preserved"
        );
    }

    /// Task #46: the appearance preference + both letterbox fills cross the form both
    /// ways, and an out-of-range appearance byte falls back to System.
    #[test]
    fn settings_form_carries_the_appearance_fields() {
        use pb_app_core::settings::AppearanceMode;
        let mut h = test_handle(800, 600, 1.0);
        // Pin the field: `new_host` loads the REAL settings.toml, so asserting the
        // shipped default here would fail on any machine where the owner has picked
        // a theme (it did — the 2026-07-03 light-mode smoke).
        h.core.settings.appearance_mode = AppearanceMode::System;
        let mut form = h.settings_form();
        assert_eq!(form.appearance_mode, 0, "System crosses the form as 0");

        form.appearance_mode = 1; // light
        form.letterbox_r = 5;
        form.letterbox_light_r = 250;
        let folded = fold_settings_form(&h.core.settings, &form, form.refresh_hz);
        assert_eq!(folded.appearance_mode, AppearanceMode::Light);
        assert_eq!(folded.letterbox[0], 5);
        assert_eq!(folded.letterbox_light[0], 250);
        // And back out through the getter.
        let mut h2 = test_handle(800, 600, 1.0);
        h2.core.settings = folded;
        let out = h2.settings_form();
        assert_eq!(out.appearance_mode, 1);
        assert_eq!(out.letterbox_light_r, 250);

        form.appearance_mode = 99; // garbage byte → System
        let folded = fold_settings_form(&h.core.settings, &form, form.refresh_hz);
        assert_eq!(folded.appearance_mode, AppearanceMode::System);
    }

    /// Task #44: the AI-describe fields cross the FFI form and fold back — the backend enum
    /// encoding, the endpoint/model/prompt strings (empty prompt → `None`), and the response
    /// cap. Pure — never touches settings.toml.
    #[test]
    fn settings_form_carries_the_describe_fields() {
        use pb_app_core::settings::DescribeBackend;
        let mut h = test_handle(800, 600, 1.0);
        h.core.settings.describe_backend = DescribeBackend::Auto;
        let mut form = h.settings_form();
        assert_eq!(form.describe_backend, 0, "Auto crosses as 0");

        form.describe_backend = 2; // local endpoint
        form.describe_endpoint = "http://gremlin:1234/v1".to_string();
        form.describe_model = "qwen2.5-vl".to_string();
        form.describe_prompt = "  Custom prompt  ".to_string();
        form.describe_max_tokens = 1024;
        form.describe_auto = true;
        let folded = fold_settings_form(&h.core.settings, &form, form.refresh_hz);
        assert_eq!(folded.describe_backend, DescribeBackend::LocalEndpoint);
        assert_eq!(folded.describe_endpoint, "http://gremlin:1234/v1");
        assert_eq!(folded.describe_model, "qwen2.5-vl");
        assert_eq!(
            folded.describe_prompt.as_deref(),
            Some("Custom prompt"),
            "trimmed"
        );
        assert_eq!(folded.describe_max_tokens, 1024);
        assert!(folded.describe_auto);

        // Empty prompt folds to None (use the built-in instruction).
        form.describe_prompt = "   ".to_string();
        let folded = fold_settings_form(&h.core.settings, &form, form.refresh_hz);
        assert_eq!(folded.describe_prompt, None);

        // Round-trips back out through the getter.
        let mut h2 = test_handle(800, 600, 1.0);
        h2.core.settings = folded;
        let out = h2.settings_form();
        assert_eq!(out.describe_backend, 2);
        assert_eq!(out.describe_endpoint, "http://gremlin:1234/v1");
        assert_eq!(out.describe_prompt, "", "None crosses as empty");
    }
}
