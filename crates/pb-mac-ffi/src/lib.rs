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
    /// The subtitle track picker's rows `(label, active)` (task #99) — snapshotted by
    /// `subtitle_picker_refresh` when the popover/menu is about to draw, then read by the
    /// indexed `subtitle_track_*` accessors. Same pull pattern as `help_snapshot`, for the
    /// same reason (`Vec<PickerRow>` can't cross back to Swift), plus one of its own: a
    /// snapshot is a *stable* list, so a probe landing between the host's `count()` and its
    /// `label(i)` calls can't shift the rows out from under a half-drawn menu.
    ///
    /// **A row's index is its index into `cycle_choices`** — that correspondence is what
    /// `select_subtitle_track(i)` relies on, and it's pinned by
    /// `rows_correspond_index_for_index_with_cycle_choices` in pb-app-core.
    subtitle_picker_snapshot: Vec<(String, bool)>,
    /// The audio track picker's rows `(label, active)` (task #99) — same snapshot-then-read
    /// shape as `subtitle_picker_snapshot`, and stable for the same reason: `menuNeedsUpdate`
    /// must build a menu synchronously, so the rows cannot shift under a half-drawn one.
    audio_picker_snapshot: Vec<(String, bool)>,
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
    /// The never-consumed copy of the CLI launch paths (see `launch_path_count`) —
    /// the host's Apple-Event echo filter reads it after the stash is consumed.
    launch_paths_record: Vec<String>,
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
        core.native_thumbs = true; // the SwiftUI Thumbnails strip (task #83)
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
            subtitle_picker_snapshot: Vec::new(),
            audio_picker_snapshot: Vec::new(),
            inspector_snapshot: Vec::new(),
            tree_snapshot: Vec::new(),
            pending_launch_paths: Vec::new(),
            launch_paths_record: Vec::new(),
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

    // ── The subtitle track picker (task #99) ──────────────────────────────
    //
    // The playback bar's popover and the Playback ▸ Subtitles flyout both draw from this
    // snapshot: refresh once as the surface opens (`menuNeedsUpdate:` / the popover's
    // appear), then read the indexed accessors.

    /// Snapshot the subtitle picker's rows — call before reading `subtitle_track_*`.
    ///
    /// Cheap enough to call on every menu open (a catalog lookup and a few `format!`s), and
    /// that is the point: the list is per-file and must never be a stale push. See
    /// `menuNeedsUpdate(_:)` in MenuBar.swift.
    fn subtitle_picker_refresh(&mut self) {
        self.subtitle_picker_snapshot = self
            .core
            .subtitle_picker_rows()
            .into_iter()
            .map(|r| (r.label, r.active))
            .collect();
    }

    /// How many rows the picker has — **0 means offer nothing**, which is *not* the same
    /// claim as "this file has no subtitles". Pair it with `subtitle_tracks_known()`: 0 rows
    /// plus not-known means the probe is still reading. A file that genuinely has none still
    /// reports 1 — the Off row — because Off is a real choice, not the absence of one.
    fn subtitle_track_count(&self) -> usize {
        self.subtitle_picker_snapshot.len()
    }

    /// Row `i`'s label: "Off", or the shared `track_summary` line ("English · SubRip ·
    /// Forced"). Out-of-range → "" (defensive, like the keymap accessors).
    fn subtitle_track_label(&self, i: usize) -> String {
        self.subtitle_picker_snapshot
            .get(i)
            .map(|r| r.0.clone())
            .unwrap_or_default()
    }

    /// Is row `i` what is on screen right now (the tick)? Exactly one row is ever active.
    fn subtitle_track_active(&self, i: usize) -> bool {
        self.subtitle_picker_snapshot
            .get(i)
            .map(|r| r.1)
            .unwrap_or(false)
    }

    /// Has the track probe landed for the video on screen? `false` = "still reading" — draw
    /// that as *reading*, never as "no tracks".
    fn subtitle_tracks_known(&self) -> bool {
        self.core.subtitle_tracks_known()
    }

    /// Are subtitles switched on (the `C` state)? The picker button fills its icon on this.
    fn subtitles_on(&self) -> bool {
        self.core.subtitles_on()
    }

    // ── The audio track picker (task #99) ─────────────────────────────────
    //
    // Same snapshot-then-read shape as the subtitle picker. The difference is the tick: the
    // core cannot know what is coming out of the speakers, so the HOST reports it
    // (`set_active_audio_row`) and the core only formats.

    /// Snapshot the Playback ▸ Audio rows — call before reading `audio_track_*`.
    fn audio_picker_refresh(&mut self) {
        self.audio_picker_snapshot = self
            .core
            .audio_picker_rows()
            .into_iter()
            .map(|r| (r.label, r.active))
            .collect();
    }

    fn audio_track_count(&self) -> usize {
        self.audio_picker_snapshot.len()
    }

    fn audio_track_label(&self, i: usize) -> String {
        self.audio_picker_snapshot
            .get(i)
            .map(|r| r.0.clone())
            .unwrap_or_default()
    }

    fn audio_track_active(&self, i: usize) -> bool {
        self.audio_picker_snapshot
            .get(i)
            .map(|r| r.1)
            .unwrap_or(false)
    }

    /// Row `i` as an FFmpeg stream index (`-1` = not FFmpeg-located) — the sample-buffer
    /// route feeds this straight to `session_audio_set_track`.
    fn audio_track_ff_stream(&self, i: usize) -> i64 {
        self.core.audio_row_ff_stream(i)
    }

    /// Row `i`'s serialized `AVMediaSelectionOption` (empty = not AVFoundation-located) —
    /// the AVPlayer route rebuilds the option from this.
    ///
    /// The two routes speak different currencies (a stream index vs. a property list), and
    /// even `local_id` means different things per backend, so the host must dispatch on
    /// which one answers rather than assuming an ordinal.
    fn audio_track_av_plist(&self, i: usize) -> Vec<u8> {
        self.core.audio_row_av_plist(i)
    }

    /// The host reports which row it is **actually playing** (`-1` = unknown). Called on
    /// open and after every switch — including a refused one, which re-states the unchanged
    /// track so the tick can never drift from the speakers.
    fn set_active_audio_row(&mut self, row: i64) {
        self.core.set_active_audio_row(row);
        self.audio_picker_refresh();
    }

    /// The host reports a switch's outcome: toast, and refresh the tick. `ok == false`
    /// means the previous track is still playing — #99's confirmed-switch rule.
    fn audio_track_switched(&mut self, row: usize, ok: bool) {
        self.core.audio_track_switched(row, ok);
        self.audio_picker_refresh();
    }

    /// Apply row `i` — the index into the list the accessors above just described. Refreshes
    /// the snapshot so the tick has already moved when the caller redraws.
    fn select_subtitle_track(&mut self, i: usize) {
        self.core.select_subtitle_row(i);
        self.subtitle_picker_refresh();
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

    // --- Thumbnails strip (task #83) ---

    /// Whether the Thumbnails tab is the visible left-pane content.
    fn thumbs_visible(&self) -> bool {
        self.core.thumbs_visible()
    }

    /// The left pane's active tab: 0 = Folders, 1 = Thumbnails (the tab-bar highlight).
    fn left_tab(&self) -> u8 {
        match self.core.left_tab {
            pb_app_core::LeftTab::Folders => 0,
            pb_app_core::LeftTab::Thumbnails => 1,
        }
    }

    /// Deck length — the strip's virtual row count.
    fn thumb_count(&self) -> usize {
        self.core.source.len()
    }

    /// The current photo's playlist index (the highlight), or -1 on an empty deck.
    fn thumb_current(&self) -> i64 {
        self.core.playlist.current().map(|i| i as i64).unwrap_or(-1)
    }

    /// The thumb store's change counter — cheap "anything new?" probe per pull.
    fn thumb_dirty(&self) -> u64 {
        self.core.thumbs.cache.dirty()
    }

    /// The entry's generation (0 = no thumb yet). Swift skips re-pulling pixels
    /// for a cell whose generation it already holds (pull-once, plan §8).
    fn thumb_gen(&self, i: usize) -> u64 {
        self.core.thumbs.cache.get(i).map(|e| e.gen).unwrap_or(0)
    }

    fn thumb_width(&self, i: usize) -> u32 {
        self.core.thumbs.cache.get(i).map(|e| e.w).unwrap_or(0)
    }

    fn thumb_height(&self, i: usize) -> u32 {
        self.core.thumbs.cache.get(i).map(|e| e.h).unwrap_or(0)
    }

    /// One entry's RGBA8 pixels, copied once on demand for exactly this cell
    /// (never a bulk clone of the store — plan §8).
    fn thumb_rgba(&self, i: usize) -> Vec<u8> {
        self.core
            .thumbs
            .cache
            .get(i)
            .map(|e| e.payload.rgba.clone())
            .unwrap_or_default()
    }

    /// The cell's display filename (the basename of the item's name/path).
    fn thumb_name(&self, i: usize) -> String {
        let name = self.core.source.name(i);
        name.rsplit(['/', '\\']).next().unwrap_or(name).to_string()
    }

    /// Whether the cell is an archive **door** — typed off the path, no I/O (task #105).
    ///
    /// A door has no thumbnail to decode: its frame is a 1×1 transparent sentinel, so a
    /// cell that went through the normal pixel path above would come out blank. The host
    /// draws the door artwork here instead, from the same one-time asset the card uses.
    /// It never touches the archive — the strip has no more right to decompress one than
    /// the prefetch ring does.
    fn thumb_archive(&self, i: usize) -> bool {
        self.core.item_archive_kind(i).is_some()
    }

    /// Item-type badge: 0 none, 1 video, 2 Live Photo, 3 animated. Live/animated
    /// appear once their (lazily filled) caches know; video is always known.
    ///
    /// A door gets none: the folder artwork it draws already says what it is, which is
    /// exactly the split the egui strip makes.
    fn thumb_badge(&self, i: usize) -> u8 {
        if matches!(
            pb_app_core::video::item_kind(self.core.source.as_ref(), i),
            pb_app_core::LibraryItemKind::Video(_)
        ) {
            return 1;
        }
        if matches!(self.core.live_motion_cache.get(&i), Some(Some(_))) {
            return 2;
        }
        if self
            .core
            .meta_cache
            .get(&i)
            .is_some_and(|m| m.animated.is_some())
        {
            return 3;
        }
        0
    }

    /// The item's session rotation override in clockwise quarter turns (0..=3) —
    /// applied at draw so the strip matches the viewer (fixed cells: no reflow).
    fn thumb_rotation(&self, i: usize) -> u8 {
        match self.core.rotations.get(&i) {
            Some(pb_render::Rotation::R90) => 1,
            Some(pb_render::Rotation::R180) => 2,
            Some(pb_render::Rotation::R270) => 3,
            _ => 0,
        }
    }

    /// Whether the item's decode failed (broken-image glyph, never a spinner).
    fn thumb_failed(&self, i: usize) -> bool {
        self.core.failed.contains(&i) || self.core.thumbs.failed.contains(&i)
    }

    /// A strip cell click: absolute jump + the instant thumb-preview present.
    fn thumb_click(&mut self, i: usize) {
        self.core.thumb_jump(i);
    }

    /// The shell's visible + overscan cell ranges (inclusive playlist indices) —
    /// the demand window fills and eviction protect. Reported on scroll.
    fn thumbs_set_viewport(
        &mut self,
        vis_lo: usize,
        vis_hi: usize,
        over_lo: usize,
        over_hi: usize,
    ) {
        self.core.thumbs.viewport = Some(((vis_lo, vis_hi), (over_lo, over_hi)));
        if let Some(cur) = self.core.playlist.current() {
            let demand = self.core.thumbs.demand(cur);
            self.core.thumbs.cache.rebalance(&demand);
        }
        self.core.request_prefetch();
    }

    /// The user grabbed the list (not our own animation): detach auto-follow.
    fn thumbs_user_scrolled(&mut self) {
        self.core.thumbs.follow.user_scrolled();
    }

    /// The pending follow-scroll command's target (-1 = none) + generation.
    /// Swift reads both, starts the scroll, calls `take_thumb_scroll`, and
    /// reports `thumbs_scroll_done(gen)` when the animation lands.
    fn thumb_scroll_item(&self) -> i64 {
        self.core
            .thumbs
            .pending_scroll
            .map(|c| c.item as i64)
            .unwrap_or(-1)
    }

    fn thumb_scroll_gen(&self) -> u64 {
        self.core.thumbs.pending_scroll.map(|c| c.gen).unwrap_or(0)
    }

    fn take_thumb_scroll(&mut self) {
        self.core.thumbs.pending_scroll = None;
    }

    fn thumbs_scroll_done(&mut self, gen: u64) {
        self.core.thumbs.follow.programmatic_done(gen);
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

    /// A toolbar nav/random button was pressed and **held** → begin hold-to-blaze for its
    /// action (task #55). The Swift host kicks the pump + drains afterward, exactly like a
    /// keypress, so the self-paced advance runs for as long as the button is held.
    fn begin_pointer_nav(&mut self, action_id: &str) {
        self.core.now = Instant::now();
        if let Some(action) = Action::from_id(action_id) {
            self.core.begin_pointer_nav(action);
        }
    }

    /// The held toolbar nav/random button was released → stop blazing.
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
        // `--metrics`: swap in a recording StageTimes (the winit shell passes
        // `StageTimes::enabled()` into `App::new` the same way). The core records the
        // stages itself (decode/upload/render/present/drain); the host prints
        // [`metrics_report`](Self::metrics_report) on quit.
        if self.core.launch.metrics {
            self.core.metrics = pb_app_core::metrics::StageTimes::enabled();
        }
        self.pending_launch_paths = cli
            .launch_paths()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        // A permanent (never-consumed) copy for the host's double-delivery dedup: a
        // bare-path launch delivers the same path twice — parsed argv AND an AppKit
        // document-open Apple Event — and Swift drops the echo against this record.
        self.launch_paths_record = self.pending_launch_paths.clone();
    }

    /// How many positional launch paths the CLI carried — the never-consumed record
    /// behind [`launch_path_at`](Self::launch_path_at) (unlike the open stash, which
    /// `open_launch_paths` consumes). The host reads these to build its Apple-Event
    /// echo filter for bare-path launches.
    fn launch_path_count(&self) -> usize {
        self.launch_paths_record.len()
    }

    /// The `i`th recorded launch path ("" out of range) — see `launch_path_count`.
    fn launch_path_at(&self, i: usize) -> String {
        self.launch_paths_record.get(i).cloned().unwrap_or_default()
    }

    /// The `--metrics` end-of-run summary (task #78): the core's per-stage p50/p95/p99
    /// table, or "" when metrics are off / nothing was recorded. The host prints it to
    /// stdout on quit — the winit shell's post-`run_app` report, minus that shell's
    /// pool-thread extras (its `POOL_DECODE_MS` is winit-local plumbing; the core's
    /// `decode` row covers the same stage here).
    fn metrics_report(&self) -> String {
        self.core.metrics.report()
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
    /// item changes, so the refresh stays off the hold-to-blaze hot path). RAM-only, never
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

    /// The current theme-aware letterbox/background fill (sRGB), packed `0x00RRGGBB` — the
    /// same color photos letterbox with. The video presentation uses it so a letterboxed /
    /// Original video sits on the user's background, consistent with stills (task 79.9).
    fn effective_letterbox_rgb(&self) -> u32 {
        let [r, g, b] = self.core.effective_letterbox();
        (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
    }

    /// Any motion playing — animation/Live Photo OR a video (task 79.9). The toolbar
    /// Play/Pause glyph reads this so it reflects a playing video, not just an animation.
    fn motion_playing(&self) -> bool {
        self.core.motion_playing()
    }

    // ── Native video callbacks (task 79.9 phase 2): the shell's AVPlayer reports its
    //    authoritative state back so the core's passive proxy advances (making P /
    //    toolbar pause/resume/replay work + failures return to the poster). All
    //    session-gated inside the proxy.
    /// The active native-video session id (0 = none) — the host reconciles its AVPlayer
    /// against this each pump so a missed teardown can't leave a second video playing.
    fn native_video_session_id(&self) -> u64 {
        self.core.native_video_session_id()
    }
    fn native_video_opened(&mut self, session_id: u64, duration_ms: i64, has_audio: bool) {
        self.core
            .native_video_opened(session_id, duration_ms, has_audio);
    }
    fn native_video_state_changed(&mut self, session_id: u64, state: u8) {
        self.core.native_video_state_changed(session_id, state);
    }
    fn native_video_ended(&mut self, session_id: u64) {
        self.core.native_video_ended(session_id);
    }
    fn native_video_seek_completed(&mut self, session_id: u64, generation: u64, finished: bool) {
        self.core
            .native_video_seek_completed(session_id, generation, finished);
    }
    /// `recoverable` = the Swift shell's error classification (task #84 §8a):
    /// demux/codec-shaped failures are FFmpeg-fallback-eligible; missing-file /
    /// permission / DRM / network failures are not.
    fn native_video_failed(&mut self, session_id: u64, error: String, recoverable: bool) {
        self.core
            .native_video_failed(session_id, error, recoverable);
    }
    /// The native player's current position + duration (seconds), reported each pump
    /// so the core can remember a session-only resume position (task #94.2). Cheap;
    /// session-gated core-side. `duration <= 0` is ignored (not yet known).
    fn native_video_progress(&mut self, session_id: u64, position_secs: f64, duration_secs: f64) {
        self.core
            .native_video_progress(session_id, position_secs, duration_secs);
    }

    // ── Session-backed video (task #84 §8): the FFmpeg fallback renders through
    // the wgpu canvas and reports through these instead of the AVPlayer observer.

    /// Whether the displayed item plays through the cross-platform `VideoSession`
    /// (the FFmpeg route) — the host keys controls visibility + scrubber routing
    /// on this alongside its own `nativeVideo` checks.
    fn video_session_active(&self) -> bool {
        self.core.video_session_active()
    }

    /// Is a video actually on screen — on **either** backend? Unlike
    /// `video_session_active`, this is true for the Native routes (AVPlayer and the
    /// sample-buffer presenter) too, which is what MKV and WebM actually take.
    ///
    /// The Playback ▸ Subtitles flyout enables on this. Without it the flyout can't tell a
    /// still from an unprobed video, and would offer "Reading tracks…" over a photograph.
    fn video_showing(&self) -> bool {
        self.core.video_showing()
    }

    /// The session's playhead / duration in seconds (`0` = none/unknown) and
    /// whether it's actively playing — the SwiftUI scrubber's raw inputs, read
    /// each pump while a session is active (~display rate; cheap).
    fn video_session_elapsed_secs(&self) -> f64 {
        self.core.video_session_elapsed_secs()
    }
    fn video_session_duration_secs(&self) -> f64 {
        self.core.video_session_duration_secs()
    }
    fn video_session_playing(&self) -> bool {
        self.core.video_playing()
    }

    /// Absolute scrubber seek to `frac` of the duration — the Session twin of the
    /// native player's `seek(toFraction:)`; no-op on the Native backend.
    fn video_seek_fraction(&mut self, frac: f32) {
        self.core.video_seek_fraction(frac);
    }

    // ── Session-video audio (task #84 §7, plan §7/1E): the FFmpeg audio decoder
    // that feeds the host's AVAudioEngine sink now lives behind an owned
    // usize-pointer handle (the free functions below), opened + driven on the
    // host's serial feeder queue — OFF the main actor (R5). Only the clock sample
    // still rides the core handle (main-actor, cheap).

    /// Host → core: one audio clock sample (the shell half of the clock bridge
    /// the winit shell implements in `video_audio.rs`). `state`: 0 Opening,
    /// 1 Playing, 2 Paused, 3 Buffering, 4 Ended, 5 Failed, 6 Absent.
    fn video_audio_clock(&mut self, session_id: u64, state: u8, position_secs: f64) {
        use pb_app_core::video::{AudioClockSample, AudioClockState, VideoSessionId};
        let state = match state {
            1 => AudioClockState::Playing,
            2 => AudioClockState::Paused,
            3 => AudioClockState::Buffering,
            4 => AudioClockState::Ended,
            5 => AudioClockState::Failed,
            6 => AudioClockState::Absent,
            _ => AudioClockState::Opening,
        };
        self.core.video_audio_clock(AudioClockSample {
            session_id: VideoSessionId(session_id),
            state,
            position: std::time::Duration::from_secs_f64(position_secs.max(0.0)),
            sampled_at_monotonic: std::time::Duration::ZERO, // delivered immediately
        });
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
            subtitles: s.subtitles,
            compare_pin_enabled: s.compare_pin_enabled,
            compare_pinned_here: s.compare_pinned_here,
            compare_toggle_enabled: s.compare_toggle_enabled,
            save_rotation_enabled: s.save_rotation_enabled,
            reveal_enabled: s.reveal_enabled,
            cancel_scan_enabled: s.cancel_scan_enabled,
            undo_enabled: s.undo.is_some(),
            undo_label: s.undo.as_deref().unwrap_or("Undo").to_string(),
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

    /// Re-arm the transient video-controls reveal — the host calls this when the user releases
    /// the info-line scrubber so the controls fade out gracefully rather than snapping away
    /// (a SwiftUI drag captures the pointer, so canvas hover moves stop mid-drag).
    fn flash_video_controls(&mut self) {
        self.core.flash_video_controls();
    }

    /// Pull the in-RAM archive-video container bytes stashed for a `PlayVideoBytes` effect
    /// (macOS archive playback). The host wraps them in a resource loader for `AVPlayer`.
    /// Consumes the stash; empty if none pending.
    fn take_pending_video_bytes(&mut self) -> Vec<u8> {
        self.core.take_pending_video_bytes()
    }

    /// Pull the archive-video **poster** bytes stashed for `request_id` (macOS). The host
    /// generates a poster frame from them and returns it via `video_poster_ready`.
    fn take_pending_poster_bytes(&mut self, request_id: u64) -> Vec<u8> {
        self.core.take_pending_poster_bytes(request_id)
    }

    /// Deliver a shell-generated archive-video poster (macOS): `ptr`/`len` point at the
    /// host's RGBA8 pixels (`w*h*4`), copied here into the resident ring via the normal
    /// upload path. `ptr` must be valid for `len` bytes for this synchronous call (the host
    /// passes it from `Data.withUnsafeBytes`, which outlives the call).
    fn video_poster_ready(
        &mut self,
        request_id: u64,
        item: u64,
        w: u32,
        h: u32,
        data_ptr: usize,
        len: usize,
    ) {
        let rgba = if data_ptr == 0 || len == 0 {
            Vec::new()
        } else {
            // SAFETY: the Swift host passes a live `Data` buffer pointer for the call's duration.
            unsafe { std::slice::from_raw_parts(data_ptr as *const u8, len).to_vec() }
        };
        self.core
            .video_poster_ready(request_id, item as usize, w, h, rgba);
    }

    /// Deliver an archive video's stream facts (macOS), probed by the host via AVFoundation
    /// since Rust can't open an `AVAsset` from bytes — populates the inspector's video rows.
    fn archive_video_meta_ready(
        &mut self,
        item: u64,
        codec: String,
        fps_milli: u32,
        duration_ms: i64,
        has_audio: bool,
    ) {
        self.core
            .archive_video_meta_ready(item as usize, codec, fps_milli, duration_ms, has_audio);
    }

    /// The current item's video-layer placement (task 79.9 phase 3) — the still
    /// renderer's geometry, so the host places the `AVPlayerLayer` identically to a photo.
    fn video_placement(&self) -> ffi::VideoPlacementFfi {
        match self.core.video_placement() {
            Some((x, y, w, h, rotation)) => ffi::VideoPlacementFfi {
                valid: true,
                x,
                y,
                w,
                h,
                rotation,
            },
            None => ffi::VideoPlacementFfi {
                valid: false,
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
                rotation: 0,
            },
        }
    }

    // ── Subtitles (task #90). The core rasterizes the cue to a premultiplied RGBA8 bitmap
    // (physical px, so it's Retina-sharp) and gives the rect to draw it in (logical points).
    // Swift pulls the pixels only when the generation changes — the `thumb_rgba` contract.──

    /// Bumped whenever the overlay changes; `0` = nothing showing. Swift caches its
    /// `NSImage` against this and skips the pixel pull when it's unchanged — which is
    /// almost every frame, since a cue lives for seconds.
    fn subtitle_gen(&self) -> u64 {
        self.core.subtitles.gen()
    }

    fn subtitle_width(&self) -> u32 {
        self.core.subtitles.bitmap().map(|b| b.w).unwrap_or(0)
    }

    fn subtitle_height(&self) -> u32 {
        self.core.subtitles.bitmap().map(|b| b.h).unwrap_or(0)
    }

    /// The overlay's **premultiplied** RGBA8 pixels — note the difference from
    /// `thumb_rgba`, which is straight alpha: the CGImage must be built with
    /// `.premultipliedLast`, or every antialiased glyph edge and the whole translucent
    /// background come out wrong.
    fn subtitle_rgba(&self) -> Vec<u8> {
        self.core
            .subtitles
            .bitmap()
            .map(|b| b.rgba.clone())
            .unwrap_or_default()
    }

    /// Where to draw it, in **logical points**, top-left origin (the `video_placement`
    /// convention — the Swift view flips to AppKit's bottom-left).
    fn subtitle_rect(&self) -> ffi::VideoPlacementFfi {
        match self.core.subtitles.rect() {
            Some(r) => ffi::VideoPlacementFfi {
                valid: true,
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
                rotation: 0,
            },
            None => ffi::VideoPlacementFfi {
                valid: false,
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
                rotation: 0,
            },
        }
    }

    // ── The native play hint (▶/Live Photo on a motion item): the last HUD overlay to go
    // native. The core signals *when* to flash it (seq) + *what* it is (kind); the host owns
    // the pill, its 3s fade, hover-to-hold, and click-to-play (via menu_action "play_pause").──

    /// 0 = none (still / already playing), 1 = Live Photo, 2 = another animation.
    /// An archive door has no pill — its affordance is the door card (task #105).
    fn play_hint_kind(&self) -> u8 {
        self.core.play_hint_kind()
    }

    /// Bumped when a fresh motion item settles — the host flashes the hint on a change.
    fn play_hint_seq(&self) -> u64 {
        self.core.play_hint_seq
    }

    // ── The archive door card (task #105). A door's frame is a 1×1 transparent sentinel,
    // so this card is an archive's ENTIRE on-screen presence: the host draws it as chrome
    // over the letterbox, and `P` (or a click on it) goes in. The artwork is deliberately
    // absent from the snapshot — it's a static asset the host pulls once via `door_art_*`
    // and caches, never ~2.5 MiB cloned per pump.──

    /// The presented door's card, or `visible: false` on anything else. One crossing rather
    /// than a gate plus three accessors: the gate would repeat the same work the card does
    /// (a `displayed_item` lookup + path typing — no I/O either way), and an absent card's
    /// three `String`s are empty, which allocate nothing.
    fn door_card(&self) -> ffi::DoorCardFfi {
        match self.core.door_card() {
            Some(c) => ffi::DoorCardFfi {
                visible: true,
                name: c.name,
                format: c.format,
                shortcut: c.shortcut,
            },
            None => ffi::DoorCardFfi {
                visible: false,
                name: String::new(),
                format: String::new(),
                shortcut: String::new(),
            },
        }
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

    // ---- The Subtitles tab (task #90.4) -------------------------------------------

    /// The saved style, for the pane to open on.
    fn subtitle_style_form(&self) -> ffi::SubtitleStyleFfi {
        subtitle_style_to_form(&self.core.settings.subtitle_style)
    }

    /// Settings ▸ Subtitles ▸ "Always show forced subtitles" (task #99), for the pane to
    /// open on.
    ///
    /// Its own pair rather than a field on `SubtitleStyleFfi`: it is **behaviour, not
    /// style**, so it must not ride the style form — that form feeds the live preview
    /// swatch, and a bool that changes nothing about how text *looks* has no business
    /// invalidating it.
    fn forced_subtitles(&self) -> bool {
        self.core.settings.forced_subtitles
    }

    /// Toggled → settings + the live engine.
    ///
    /// Diffs first and **hard no-ops when unchanged**, exactly like `subtitle_style_edited`:
    /// the pane echoes its state back on open, and that echo must never reach the disk.
    /// Routed through `DialogResolved` so `apply_settings` stays the one place a preference
    /// reaches both the engine and the file.
    fn set_forced_subtitles(&mut self, on: bool) {
        if on == self.core.settings.forced_subtitles {
            return;
        }
        self.core.now = Instant::now();
        let mut s = self.core.settings.clone();
        s.forced_subtitles = on;
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::SettingsEdited {
                settings: Some(Box::new(s)),
                keymap: None,
            },
        ));
    }

    /// An edited style → settings + the live engine.
    ///
    /// Diffs first and **hard no-ops when unchanged**, exactly like `settings_edited`:
    /// the pane echoes its form back on open, and that echo must never reach the disk.
    fn subtitle_style_edited(&mut self, form: ffi::SubtitleStyleFfi) {
        self.core.now = Instant::now();
        let style = fold_subtitle_style_form(&self.core.settings.subtitle_style, &form).clamped();
        if style == self.core.settings.subtitle_style {
            return;
        }
        let mut s = self.core.settings.clone();
        s.subtitle_style = style;
        // Through the same DialogResolved path as every other settings edit, so `apply_settings`
        // is the one place a style reaches the engine and the disk.
        self.core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::SettingsEdited {
                settings: Some(Box::new(s)),
                keymap: None,
            },
        ));
    }

    /// The live preview swatch. See the bridge declaration.
    fn subtitle_preview_rgba(&mut self, form: ffi::SubtitleStyleFfi, w: u32, h: u32) -> Vec<u8> {
        // The user's real letterbox colour for the current theme, so the bars match what
        // they will actually see behind a film rather than an invented black.
        let dark = self.core.effective_dark();
        let letterbox = self.core.settings.letterbox_for(dark);
        // Clamped, not raw: a draft mid-drag is bounded by the sliders anyway, but a
        // hand-edited config could have reached the pane.
        let style = fold_subtitle_style_form(&self.core.settings.subtitle_style, &form).clamped();
        let Some(raster) = self.core.subtitles.rasterizer_mut() else {
            return Vec::new(); // the font worker hasn't landed; the pane shows a placeholder
        };
        pb_app_core::subtitle_preview::render_preview(raster, &style, w, h, letterbox)
    }

    /// Has the font system landed? Also *starts* it — opening the Subtitles tab is the
    /// trigger, so the 261 ms is spent while the user is reading the pane rather than on
    /// the first cue of a film.
    fn subtitle_preview_ready(&mut self) -> bool {
        self.core.subtitles.rasterizer_mut().is_some()
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

    /// Whether the keymap binds this chord — the host's "did the core just take this key?"
    /// question, asked so it knows whether to **consume** the event.
    ///
    /// It exists for ⌘ chords. The host forwards those to AppKit rather than eating them,
    /// or the menu's own ⌘O / ⌘Q / ⌘, would never fire — but an unmatched ⌘ chord that
    /// reaches the responder chain is answered with a **beep**, and since ⌘↓ (Open — the
    /// Finder chord, task #105) is bound in the keymap and *not* in any menu, it would act
    /// and beep. Asking here keeps the rule where the bindings are: the host consumes what
    /// the core claims and passes on what it doesn't, without naming a single chord.
    ///
    /// Pure: a keymap lookup, no dispatch, no clock. Repeat is not a factor — an OS
    /// auto-repeat is still a key the core owns, it just declines to act on it.
    fn key_is_bound(&self, key: &str, ctrl: bool, shift: bool, alt: bool, logo: bool) -> bool {
        PbKey::from_name(key).is_some_and(|k| {
            let mods = Modifiers {
                ctrl,
                shift,
                alt,
                logo,
            };
            self.core.keymap.action_for(&mods.chord_with(k)).is_some()
        })
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
            self.core.settings.subtitles,
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
        // Show Archives (task #104) is a setting, defaulted off by the pure choke point, so
        // override it here the way the winit shell does (`current_menu_state`).
        let next = contract::MenuState {
            show_archives: self.core.settings.show_archives,
            ..next
        };
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
                C::ShellFlowAction(Action::ShowArchives) => self.toggle_show_archives(),
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
                // Session-video audio (task #84 §7, plan §7/1E): stash the container
                // input in the thread-safe global; the host pulls it OFF the main
                // actor via `open_stashed_session_audio(session_id)`. Without ffvideo
                // the arm is unreachable in practice (no session exists on macOS),
                // but stays total via map_effect's fallthrough.
                #[cfg(feature = "ffvideo")]
                C::StartVideoAudio {
                    input,
                    session_id,
                    muted,
                } => {
                    stash_audio_input(session_id.0, input);
                    return Some(ffi::CoreEffectFfi::StartVideoAudio(session_id.0, muted));
                }
                // Sample-buffer video (video-overhaul Phase 3): stash the container
                // input; the host pulls it OFF the main actor via
                // `open_stashed_demux(session_id)` on its serial reader queue, feeding
                // an AVSampleBufferDisplayLayer. Same stash rationale as StartVideoAudio
                // (a `VideoInput` can't cross the bridge). Emitted only on macOS+ffvideo.
                #[cfg(feature = "ffvideo")]
                C::PlaySampleBuffer {
                    input,
                    session_id,
                    muted,
                    start_secs,
                } => {
                    // Stash the same container for BOTH the video demux and the
                    // audio decoder: the presenter opens an owned FFmpeg audio
                    // decoder over it and feeds an AVSampleBufferAudioRenderer under
                    // the video's synchronizer (one clock — Phase 3 §3B/§3C).
                    stash_audio_input(session_id.0, input.clone());
                    stash_demux_input(session_id.0, input);
                    return Some(ffi::CoreEffectFfi::PlaySampleBuffer(
                        session_id.0,
                        muted,
                        start_secs,
                    ));
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

    /// Toggle View ▸ Show Archives (task #104) — the winit shell's `toggle_show_archives`
    /// mirrored: flip whether archives show as folder doors, persist it, and re-stream the
    /// current folder (preserving the current photo). A no-op for a non-folder deck.
    fn toggle_show_archives(&mut self) {
        let on = !self.core.settings.show_archives;
        self.core.settings.show_archives = on;
        self.core.settings.save();
        let Some(root) = self.core.scan_root.clone() else {
            self.core.show_toast(if on {
                "Show archives: on"
            } else {
                "Show archives: off"
            });
            return;
        };
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
                recursive: self.core.recursive,
            },
            cursor,
        );
        self.core.show_toast(if on {
            "Show archives: on"
        } else {
            "Show archives: off"
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
        // Live Show Archives preference (task #104): with it off, the walk drops archive doors.
        let show_archives = self.core.settings.show_archives;
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
                show_archives,
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

    /// Start opening an archive (`CoreEffect::BeginArchiveOpen`): a `.zip`
    /// synchronously; 7z and the tar family on a worker thread
    /// (`ArchiveKind::background_open` — even a lazy plain tar's index walk is
    /// O(entries) of file I/O). The per-kind dispatch, including the 7z RAM
    /// pre-flight, is `scan::load_archive`, shared with the winit shell.
    fn begin_archive_open(&mut self, path: PathBuf, password: Option<String>) {
        let kind = pb_source::archive_kind(&path).unwrap_or(pb_source::ArchiveKind::Zip);
        let was_password_attempt = password.is_some();
        // Anti-stacking: a newer open supersedes (and cancels) an in-flight one.
        if let Some(prev) = self.archive_load.as_ref() {
            prev.progress.request_cancel();
        }
        if !kind.background_open() {
            let result = scan::open_archive(&path, password);
            self.finish_archive_open(result, was_password_attempt, path);
            return;
        }
        self.archive_gen += 1;
        let generation = self.archive_gen;
        let progress = pb_source::OpenProgress::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        let worker_progress = progress.clone();
        std::thread::spawn(move || {
            let result = scan::load_archive(&worker_path, kind, password, &worker_progress);
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

// ---- The door artwork (task #105) ----------------------------------------------
//
// The zippered-folder asset every archive door draws — in the card, and in the strip's
// archive cells. Blue on macOS to match Finder (manila on Windows, for Explorer); the
// split lives in `engine::door_artwork`, which also decodes it exactly once per process
// and crops it to its content, so a shell's padding means what it says.
//
// **Free functions, pulled once.** Not a `PbCore` method and never per pump: these hand
// over ~2.5 MiB of straight-alpha RGBA8, which the host turns into one cached `NSImage`
// for the process. It is a static asset — there is no generation to watch, nothing to
// invalidate, and no reason for it to ride the frame path.

/// The door artwork's width in pixels, or `0` if the asset can't be decoded — in which
/// case the host draws a card of text and a button rather than hiding the door.
fn door_art_width() -> u32 {
    pb_app_core::engine::door_artwork()
        .map(|a| a.width)
        .unwrap_or(0)
}

/// The door artwork's height in pixels, `0` if it can't be decoded (see `door_art_width`).
fn door_art_height() -> u32 {
    pb_app_core::engine::door_artwork()
        .map(|a| a.height)
        .unwrap_or(0)
}

/// The artwork's **straight-alpha** RGBA8 pixels — the `thumb_rgba` convention, so the
/// `CGImage` wants `.last`, not `.premultipliedLast` (which `subtitle_rgba` uses). Empty
/// if it can't be decoded.
fn door_art_rgba() -> Vec<u8> {
    pb_app_core::engine::door_artwork()
        .map(|a| a.pixels.clone())
        .unwrap_or_default()
}

// ---- The Subtitles tab's form conversions (task #90.4) -------------------------
//
// Pure and total in both directions, so they are unit-testable without touching the
// user's real settings.toml — which matters here more than usual, because
// `apply_settings` does NOT check `persist_prefs` and a test that drove the FFI end to
// end WOULD overwrite the owner's config.

/// The curated font list's length — see [`pb_app_core::subtitle::FONT_CHOICES`].
///
/// Indexed accessors rather than a `Vec<String>`: that does not cross back to Swift (the
/// same constraint that shaped the keymap editor).
fn subtitle_font_count() -> usize {
    pb_app_core::subtitle::FONT_CHOICES.len()
}

/// The font at `i`, or `""` past the end (which the picker reads as the system font, so
/// an out-of-range index degrades to the default rather than panicking across the FFI).
fn subtitle_font_name(i: usize) -> String {
    pb_app_core::subtitle::FONT_CHOICES
        .get(i)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

// The Settings sliders' bounds. One definition each — these are the very constants
// `SubtitleStyle::clamped` enforces, so the pane and the clamp cannot disagree.

fn subtitle_max_size_pct() -> f32 {
    pb_app_core::subtitle::MAX_SIZE_PCT
}

fn subtitle_max_shadow_blur_px() -> f32 {
    pb_app_core::subtitle::ratio_to_px(pb_app_core::subtitle::MAX_SHADOW_BLUR_RATIO)
}

fn subtitle_max_shadow_offset_px() -> f32 {
    pb_app_core::subtitle::ratio_to_px(pb_app_core::subtitle::MAX_SHADOW_OFFSET_RATIO)
}

fn subtitle_max_line_spacing() -> f32 {
    pb_app_core::subtitle::MAX_LINE_SPACING
}

/// The shipped defaults — see the bridge declaration for why this is not a Swift constant.
fn subtitle_style_defaults() -> ffi::SubtitleStyleFfi {
    subtitle_style_to_form(&pb_app_core::subtitle::SubtitleStyle::default())
}

/// [`SubtitleStyle`](pb_app_core::subtitle::SubtitleStyle) → the flat form.
fn subtitle_style_to_form(s: &pb_app_core::subtitle::SubtitleStyle) -> ffi::SubtitleStyleFfi {
    use pb_app_core::subtitle::ratio_to_px;
    // A shadow that is off still carries its last values across, so toggling it off and
    // back on doesn't forget how it was tuned. `Default` supplies them when there has
    // never been one — and they are deliberately *visible* values, so a shadow you switch
    // on appears rather than being an invisible no-op at zero.
    let sh = s.shadow.unwrap_or_default();
    ffi::SubtitleStyleFfi {
        // The FFI cannot carry Option<String>; "" is the system font.
        font_family: s.font_family.clone().unwrap_or_default(),
        size_pct: s.size_pct,
        color_r: s.color[0],
        color_g: s.color[1],
        color_b: s.color[2],
        opacity: s.opacity,
        outline_px: ratio_to_px(s.outline_ratio),
        outline_r: s.outline_color[0],
        outline_g: s.outline_color[1],
        outline_b: s.outline_color[2],
        outline_a: s.outline_color[3],
        shadow_on: s.shadow.is_some(),
        shadow_dx_px: ratio_to_px(sh.dx_ratio),
        shadow_dy_px: ratio_to_px(sh.dy_ratio),
        shadow_blur_px: ratio_to_px(sh.blur_ratio),
        shadow_r: sh.color[0],
        shadow_g: sh.color[1],
        shadow_b: sh.color[2],
        shadow_a: sh.color[3],
        background_r: s.background[0],
        background_g: s.background[1],
        background_b: s.background[2],
        background_a: s.background[3],
        vertical_offset_pct: s.vertical_offset_pct,
        max_line_pct: s.max_line_pct,
        line_spacing: s.line_spacing,
    }
}

/// Fold an edited style form back onto `base`, preserving the fields the form doesn't
/// expose — the background's corner radius and padding, which are a *look* rather than a
/// preference (owner, 2026-07-15) and so have no controls, but stay config-editable.
///
/// The `base` parameter is the whole point: without it, every edit in the pane would
/// silently reset a hand-tuned radius to the default. Same shape, and the same reason, as
/// [`fold_settings_form`]. The caller clamps.
fn fold_subtitle_style_form(
    base: &pb_app_core::subtitle::SubtitleStyle,
    f: &ffi::SubtitleStyleFfi,
) -> pb_app_core::subtitle::SubtitleStyle {
    use pb_app_core::subtitle::px_to_ratio;
    pb_app_core::subtitle::SubtitleStyle {
        // "" (the picker's "System" row) means the system font, exactly as an absent
        // `font_family` key does — `SubtitleStyle::font` enforces the same rule again on
        // the read side, so a stray whitespace name can't reach the shaper either.
        font_family: Some(f.font_family.clone()).filter(|s| !s.trim().is_empty()),
        size_pct: f.size_pct,
        // The text's own alpha has no control and is preserved: the pane's Opacity is the
        // master below.
        color: [f.color_r, f.color_g, f.color_b, base.color[3]],
        opacity: f.opacity,
        outline_ratio: px_to_ratio(f.outline_px),
        outline_color: [f.outline_r, f.outline_g, f.outline_b, f.outline_a],
        shadow: f.shadow_on.then_some(pb_app_core::subtitle::Shadow {
            dx_ratio: px_to_ratio(f.shadow_dx_px),
            dy_ratio: px_to_ratio(f.shadow_dy_px),
            blur_ratio: px_to_ratio(f.shadow_blur_px),
            color: [f.shadow_r, f.shadow_g, f.shadow_b, f.shadow_a],
        }),
        background: [
            f.background_r,
            f.background_g,
            f.background_b,
            f.background_a,
        ],
        // Not in the form — preserved.
        background_radius_ratio: base.background_radius_ratio,
        background_pad_ratio: base.background_pad_ratio,
        vertical_offset_pct: f.vertical_offset_pct,
        max_line_pct: f.max_line_pct,
        line_spacing: f.line_spacing,
    }
}

/// Fold an edited Settings form back onto `base`, preserving the fields the form doesn't
/// expose (the remembered fullscreen state, window geometry), clamped to valid ranges —
/// the egui `SettingsDraft::to_settings` mirrored. Pure (no I/O), so it's unit-testable
/// without touching the user's real settings.toml (`SettingsEdited` → `apply_settings`
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
                        text: format!(
                            "{}: no such file or folder: {}",
                            pb_app_core::APP_NAME,
                            p.display()
                        ),
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

/// Is this path a viewable archive? The one classifier
/// (`pb_source::archive_kind`): zip, 7z, and the tar family. Mirrors the winit
/// shell's helper.
fn is_archive(p: &Path) -> bool {
    pb_source::archive_kind(p).is_some()
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
            start_secs,
        } => E::PlayVideo(
            path.to_string_lossy().into_owned(),
            session_id.0,
            muted,
            start_secs,
        ),
        C::PlayVideoBytes {
            name,
            session_id,
            muted,
            start_secs,
        } => E::PlayVideoBytes(name, session_id.0, muted, start_secs),
        C::RequestVideoPoster {
            request_id,
            item,
            name,
            max_edge,
        } => E::RequestVideoPoster(request_id, item as u64, name, max_edge),
        C::StopVideo { session_id } => E::StopVideo(session_id.0),
        C::PauseVideo { session_id } => E::PauseVideo(session_id.0),
        C::ResumeVideo { session_id } => E::ResumeVideo(session_id.0),
        C::SeekVideoBy {
            session_id,
            generation,
            delta_ms,
        } => E::SeekVideoBy(session_id.0, generation.0, delta_ms),
        C::StepVideo {
            session_id,
            forward,
        } => E::StepVideo(session_id.0, forward),
        C::SetVideoMuted { session_id, muted } => E::SetVideoMuted(session_id.0, muted),
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
        // Session-video audio (task #84 §7): the host owns the AVAudioEngine sink; the
        // Rust FFmpeg decoder behind it is driven through the video_audio_* accessors.
        // (StartVideoAudio is intercepted in `next_effect` — the input must be stashed.)
        C::StopVideoAudio => E::StopVideoAudio,
        C::SelectAudioTrack { row } => E::SelectAudioTrack(row),
        C::PauseVideoAudio => E::PauseVideoAudio,
        C::ResumeVideoAudio => E::ResumeVideoAudio,
        C::SeekVideoAudio { position } => E::SeekVideoAudio(position.as_secs_f64()),
        C::SetVideoAudioMuted(muted) => E::SetVideoAudioMuted(muted),
        _ => E::Other,
    }
}

// ── Session-video audio: the owned off-main decoder seam (task #84 §7, plan §7/1E) ──
//
// The FFmpeg audio decoder used to live in an `Option<FfAudioDecoder>` field on the
// `@MainActor`-bound `AppCoreHandle`, so every open/read/seek/refill contended with
// the UI + pump (R5). It now lives behind a raw `usize` pointer the host wraps in a
// Swift `OwnedAudioDecoder` and drives on a dedicated serial feeder queue — off the
// main actor. swift-bridge 0.1.59 cannot return an owned opaque type, hence the
// pointer (mirrors `attach_layer`'s `layer_ptr: usize`). A `VideoInput` also cannot
// cross the bridge, so `StartVideoAudio` stashes it in the thread-safe global below
// and the host pulls it via `open_stashed_session_audio` from the feeder queue.

/// Consecutive transient (watchdog/cancel) read/seek stalls tolerated before the
/// decoder is declared failed (plan 1G). At the 10 s op-deadline that's up to ~60 s
/// of a network share being unreachable before audio gives up honestly — long
/// enough to ride out a real hiccup, bounded so a dead share can't retry forever.
#[cfg(feature = "ffvideo")]
const MAX_TRANSIENT_STRIKES: u32 = 6;

/// The boxed, exclusively-owned session-audio decoder handed to Swift as a raw
/// `usize`. `failed` records a mid-stream decode/seek error so [`session_audio_state`]
/// reports it **distinctly from a clean EOF** (R12): a corrupt tail is no longer
/// indistinguishable from the end of the stream. `transient_strikes` counts
/// consecutive recoverable stalls (a slow network read) so playback rebuffers and
/// retries rather than dying, up to [`MAX_TRANSIENT_STRIKES`] (plan 1G).
#[cfg(feature = "ffvideo")]
struct SessionAudioDecoder {
    inner: pb_app_core::FfAudioDecoder,
    /// The container this was opened from, kept so a **track switch** (#99) can re-open
    /// without going back to `AUDIO_STASH` — that slot is consumed on success and the next
    /// video overwrites it, so it is not a session-lifetime store. Cheap either way: a
    /// `PathBuf`, or an `Arc` clone of bytes the video producer is already holding.
    input: pb_app_core::video::VideoInput,
    failed: bool,
    transient_strikes: u32,
}

/// One-slot stash for the `StartVideoAudio` container input, keyed by session id.
/// Written on the main actor (in `next_effect`), read on the host's feeder queue
/// (in `open_stashed_session_audio`) — the `Mutex` bridges the two. `VideoInput`
/// is `Send` (a path, or an `Arc<Vec<u8>>` for archive bytes).
#[cfg(feature = "ffvideo")]
static AUDIO_STASH: std::sync::Mutex<Option<(u64, pb_app_core::video::VideoInput)>> =
    std::sync::Mutex::new(None);

/// Stash the container input for `session_id`, replacing any prior slot (a new
/// session supersedes an old, never-opened one).
#[cfg(feature = "ffvideo")]
fn stash_audio_input(session_id: u64, input: pb_app_core::video::VideoInput) {
    *AUDIO_STASH.lock().unwrap_or_else(|e| e.into_inner()) = Some((session_id, input));
}

/// Open the audio decoder over the container `StartVideoAudio` stashed for
/// `session_id`, **off the main actor**. Returns a nonzero pointer to a boxed
/// [`SessionAudioDecoder`] on success, or `0` for no-audio / open failure. The
/// stash is **consumed on success and kept on failure** so the host can retry the
/// open without the core re-issuing the effect — fixing the old
/// consume-on-failure bug (the audit's "`video_audio_open` consumes the stashed
/// input on failure").
fn open_stashed_session_audio(session_id: u64) -> usize {
    #[cfg(feature = "ffvideo")]
    {
        let input = {
            let guard = AUDIO_STASH.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some((id, input)) if *id == session_id => input.clone(),
                _ => return 0,
            }
        };
        // Capped at stereo: AVAudioEngine's graph rejects wider standard formats
        // (the MKV-5.1 abort) — 5.1/7.1 folds down in the decoder. `track: None` =
        // the R10 policy picks; `session_audio_set_track` overrides it later (#99).
        match pb_app_core::FfAudioDecoder::open_track(&input, 2, None) {
            Ok(inner) => {
                let mut guard = AUDIO_STASH.lock().unwrap_or_else(|e| e.into_inner());
                if guard.as_ref().is_some_and(|(id, _)| *id == session_id) {
                    *guard = None; // consumed on success
                }
                Box::into_raw(Box::new(SessionAudioDecoder {
                    inner,
                    input,
                    failed: false,
                    transient_strikes: 0,
                })) as usize
            }
            Err(e) => {
                eprintln!("video audio open failed: {e}"); // stash survives for retry
                0
            }
        }
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = session_id;
        0
    }
}

/// Which stream the owned decoder is playing (`-1` if `ptr` is null) — the container's real
/// index, comparable to the catalog's `local_id` (task #99).
///
/// The tick in Playback ▸ Audio comes from **this**, never from re-running the selection
/// policy in the core: a guess that disagreed with what you are hearing would be worse than
/// no tick. It is also how a rejected switch stays honest — ask for a stale track, get the
/// policy's pick, and this reports which one that was.
fn session_audio_stream_index(ptr: usize) -> i64 {
    #[cfg(feature = "ffvideo")]
    {
        if ptr == 0 {
            return -1;
        }
        // SAFETY: nonzero `ptr` is a live `Box::into_raw(SessionAudioDecoder)`;
        // the host guarantees single-threaded, non-freed access (feeder queue).
        let d = unsafe { &*(ptr as *const SessionAudioDecoder) };
        d.inner.stream_index() as i64
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        -1
    }
}

/// Re-open the owned decoder on audio stream `track` (task #99), **in place**: the pointer
/// stays valid either way, so the host never has to swap or free anything.
///
/// `true` = switched; the caller must then rebuild its format (rate/channels can differ —
/// a 5.1 commentary beside a stereo main is the normal case, not an edge one), re-base its
/// PTS clock, and re-arm feeding.
///
/// `false` = **the old decoder is untouched and still playing**. That is the whole shape of
/// this call: a failed switch must cost you the choice, not the sound. (A `track` that is
/// stale or isn't audio doesn't even reach here as a failure — `open_track` falls back to
/// the policy and returns `true`, with `session_audio_stream_index` reporting what you
/// actually got.)
fn session_audio_set_track(ptr: usize, track: usize) -> bool {
    #[cfg(feature = "ffvideo")]
    {
        if ptr == 0 {
            return false;
        }
        // SAFETY: as above — the host serializes every pointer call on its feeder queue,
        // so there is no concurrent read while this replaces the decoder.
        let d = unsafe { &mut *(ptr as *mut SessionAudioDecoder) };
        match pb_app_core::FfAudioDecoder::open_track(&d.input, 2, Some(track)) {
            Ok(inner) => {
                d.inner = inner; // the old decoder drops here
                d.failed = false;
                d.transient_strikes = 0;
                true
            }
            Err(e) => {
                eprintln!("video audio: track switch failed, keeping the current track: {e}");
                false
            }
        }
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = (ptr, track);
        false
    }
}

/// Native sample rate of the owned decoder (`0` if `ptr` is null). `ptr` must come
/// from [`open_stashed_session_audio`] and not yet be freed; the host serializes
/// all pointer calls on its feeder queue.
fn session_audio_rate(ptr: usize) -> u32 {
    #[cfg(feature = "ffvideo")]
    {
        if ptr == 0 {
            return 0;
        }
        // SAFETY: nonzero `ptr` is a live `Box::into_raw(SessionAudioDecoder)`;
        // the host guarantees single-threaded, non-freed access (feeder queue).
        let d = unsafe { &*(ptr as *const SessionAudioDecoder) };
        d.inner.rate()
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0
    }
}

/// Emitted channel count of the owned decoder (native, or 2 when a wider source
/// folded down). `0` if `ptr` is null. Same pointer contract as [`session_audio_rate`].
fn session_audio_channels(ptr: usize) -> u32 {
    #[cfg(feature = "ffvideo")]
    {
        if ptr == 0 {
            return 0;
        }
        // SAFETY: see `session_audio_rate`.
        let d = unsafe { &*(ptr as *const SessionAudioDecoder) };
        u32::from(d.inner.channels())
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0
    }
}

/// Decode up to `max_frames` interleaved f32 sample frames. Empty = end of stream,
/// a **transient** stall (rebuffering), or a decode failure — the caller reads
/// [`session_audio_state`] to tell them apart. A fatal error latches `failed` (R12);
/// a transient watchdog abort (a slow network read) counts a strike and rebuffers,
/// only latching `failed` after [`MAX_TRANSIENT_STRIKES`] in a row (plan 1G). A
/// successful read clears the strike count. Same pointer contract as
/// [`session_audio_rate`], plus exclusive `&mut` (no concurrent read/seek).
fn session_audio_read(ptr: usize, max_frames: u32) -> Vec<f32> {
    #[cfg(feature = "ffvideo")]
    {
        if ptr == 0 {
            return Vec::new();
        }
        // SAFETY: see `session_audio_rate`; the feeder queue serializes read/seek.
        let d = unsafe { &mut *(ptr as *mut SessionAudioDecoder) };
        if d.failed {
            return Vec::new();
        }
        match d.inner.read(max_frames as usize) {
            Ok(chunk) => {
                d.transient_strikes = 0; // the read landed — the stall (if any) is over
                chunk
            }
            Err(e) if e.is_transient() => {
                // A slow read tripped the watchdog — rebuffer + retry, don't die.
                d.transient_strikes += 1;
                if d.transient_strikes >= MAX_TRANSIENT_STRIKES {
                    eprintln!(
                        "video audio: giving up after {} stalled reads: {e}",
                        d.transient_strikes
                    );
                    d.failed = true;
                }
                Vec::new()
            }
            Err(e) => {
                eprintln!("video audio read failed: {e}");
                d.failed = true;
                Vec::new()
            }
        }
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = (ptr, max_frames);
        Vec::new()
    }
}

/// The decoder's stream state: `0` Ok (more to read), `1` Eof (clean end), `2`
/// Failed (a latched decode/seek error, R12 — distinct from EOF), `3` Rebuffering (a
/// transient stall — empty now, but keep retrying, not EOF; plan 1G). A null pointer
/// reads as Failed, never as a clean EOF. Same pointer contract as
/// [`session_audio_rate`].
fn session_audio_state(ptr: usize) -> u8 {
    #[cfg(feature = "ffvideo")]
    {
        if ptr == 0 {
            return 2; // Failed — a missing decoder is an error, not a clean end
        }
        // SAFETY: see `session_audio_rate`.
        let d = unsafe { &*(ptr as *const SessionAudioDecoder) };
        if d.failed {
            2
        } else if d.inner.at_eof() {
            1
        } else if d.transient_strikes > 0 {
            3 // a transient stall is in progress — rebuffer, don't treat as EOF
        } else {
            0
        }
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        2
    }
}

/// Seek the owned decoder; returns the new clock anchor in seconds (the host's
/// scheduling epoch — landing is within one audio frame of the target). A seek
/// error latches `failed` (R12) and returns the requested position. Same pointer
/// contract as [`session_audio_read`].
fn session_audio_seek(ptr: usize, secs: f64) -> f64 {
    #[cfg(feature = "ffvideo")]
    {
        if ptr == 0 {
            return secs;
        }
        // SAFETY: see `session_audio_rate`; the feeder queue serializes read/seek.
        let d = unsafe { &mut *(ptr as *mut SessionAudioDecoder) };
        if d.failed {
            return secs;
        }
        match d
            .inner
            .seek(std::time::Duration::from_secs_f64(secs.max(0.0)))
        {
            Ok(anchor) => {
                d.transient_strikes = 0;
                anchor.as_secs_f64()
            }
            // A watchdog-aborted seek is transient (slow network) — don't latch
            // failed; the next read/seek retries. Return the requested position so
            // the host's clock epoch is sane meanwhile (plan 1G).
            Err(e) if e.is_transient() => {
                eprintln!("video audio: seek stalled (will retry): {e}");
                secs
            }
            Err(e) => {
                eprintln!("video audio seek failed: {e}");
                d.failed = true;
                secs
            }
        }
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = (ptr, secs);
        secs
    }
}

/// Free the owned decoder. Must be called **exactly once** per non-null pointer
/// from [`open_stashed_session_audio`] (the Swift `OwnedAudioDecoder` guards this
/// in `deinit`); a null pointer is a no-op. After this the pointer is dangling —
/// no accessor may touch it.
fn session_audio_free(ptr: usize) {
    #[cfg(feature = "ffvideo")]
    {
        if ptr != 0 {
            // SAFETY: nonzero `ptr` is a live box from `open_stashed_session_audio`,
            // freed exactly once by the host's `deinit`-once contract.
            drop(unsafe { Box::from_raw(ptr as *mut SessionAudioDecoder) });
        }
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
    }
}

// ── Sample-buffer video demux: the owned off-main packet source (video-overhaul Phase 3) ──
//
// The AVSampleBufferDisplayLayer presenter wants compressed access units, not decoded
// pixels. FFmpeg (Rust) demuxes the container; the host owns the demuxer behind a raw
// `usize` pointer — opened off the main actor via `open_stashed_demux(session_id)` (input
// stashed by the `PlaySampleBuffer` effect) on a serial reader queue, driven through the
// `demux_*(ptr, …)` free functions, and freed exactly once. Same rationale as the
// session-audio seam above: swift-bridge can't return an owned opaque type, and a
// `VideoInput` can't cross the bridge. `demux_read_packet` advances the demuxer, caches
// the packet's timing/keyframe flags on the handle, and returns the bytes; the host then
// reads the cached metadata via the `demux_packet_*` getters (mirrors the session-audio
// read + separate state-getter split). `demux_state` is 0 Ok / 1 Eof / 2 Failed.

/// The boxed, exclusively-owned demuxer handed to Swift as a raw `usize`. `last`
/// caches the most recently read packet's timing/keyframe flags so the host can read
/// them after `demux_read_packet` returns the bytes; `state` distinguishes a clean EOF
/// (1) from a demux error (2) so the presenter parks vs fails honestly.
#[cfg(feature = "ffvideo")]
struct DemuxHandle {
    demux: pb_app_core::VideoDemuxer,
    last_pts: i64,
    last_dts: i64,
    last_duration: i64,
    last_is_key: bool,
    state: u8,
}

/// One-slot stash for the `PlaySampleBuffer` container input, keyed by session id.
/// Written on the main actor (`next_effect`), read on the host's reader queue
/// (`open_stashed_demux`) — the `Mutex` bridges the two. `VideoInput` is `Send`.
#[cfg(feature = "ffvideo")]
static DEMUX_STASH: std::sync::Mutex<Option<(u64, pb_app_core::video::VideoInput)>> =
    std::sync::Mutex::new(None);

/// Stash the container input for `session_id`, replacing any prior slot.
#[cfg(feature = "ffvideo")]
fn stash_demux_input(session_id: u64, input: pb_app_core::video::VideoInput) {
    *DEMUX_STASH.lock().unwrap_or_else(|e| e.into_inner()) = Some((session_id, input));
}

/// Open the demuxer over the container `PlaySampleBuffer` stashed for `session_id`,
/// **off the main actor**. Returns a nonzero pointer to a boxed [`DemuxHandle`] on
/// success, or `0` on a missing stash / open failure (the presenter then reports a
/// classified failure so the core falls back to the Session route). The stash is
/// consumed on success and kept on failure (retry-safe), mirroring the audio seam.
fn open_stashed_demux(session_id: u64) -> usize {
    #[cfg(feature = "ffvideo")]
    {
        let input = {
            let guard = DEMUX_STASH.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some((id, input)) if *id == session_id => input.clone(),
                _ => return 0,
            }
        };
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        match pb_app_core::VideoDemuxer::open(&input, cancel) {
            Ok(demux) => {
                let mut guard = DEMUX_STASH.lock().unwrap_or_else(|e| e.into_inner());
                if guard.as_ref().is_some_and(|(id, _)| *id == session_id) {
                    *guard = None; // consumed on success
                }
                Box::into_raw(Box::new(DemuxHandle {
                    demux,
                    last_pts: i64::MIN,
                    last_dts: i64::MIN,
                    last_duration: 0,
                    last_is_key: false,
                    state: 0,
                })) as usize
            }
            Err(e) => {
                eprintln!("sample-buffer demux open failed: {e}"); // stash survives for retry
                0
            }
        }
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = session_id;
        0
    }
}

/// Shared borrow of a live [`DemuxHandle`] from a nonzero pointer. The host serializes
/// every pointer call on its reader queue, so `&`/`&mut` aliasing can't occur.
#[cfg(feature = "ffvideo")]
fn demux_ref<'a>(ptr: usize) -> Option<&'a DemuxHandle> {
    (ptr != 0).then(|| unsafe { &*(ptr as *const DemuxHandle) })
}

/// Video codec: 0 H.264, 1 HEVC, 2 other (routes to the Session fallback).
fn demux_codec(ptr: usize) -> u8 {
    #[cfg(feature = "ffvideo")]
    {
        match demux_ref(ptr).map(|h| h.demux.info().codec) {
            Some(pb_app_core::VideoCodec::H264) => 0,
            Some(pb_app_core::VideoCodec::Hevc) => 1,
            _ => 2,
        }
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        2
    }
}

/// Coded (pre-rotation) width / height.
fn demux_width(ptr: usize) -> u32 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(0, |h| h.demux.info().facts.width)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0
    }
}
fn demux_height(ptr: usize) -> u32 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(0, |h| h.demux.info().facts.height)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0
    }
}

/// Stream time base (numerator / denominator) — PTS/DTS units → `CMTime`.
fn demux_time_base_num(ptr: usize) -> i32 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(0, |h| h.demux.info().facts.time_base.0)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0
    }
}
fn demux_time_base_den(ptr: usize) -> i32 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(0, |h| h.demux.info().facts.time_base.1)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0
    }
}

/// Average frame rate (0 when the container doesn't declare one).
fn demux_fps(ptr: usize) -> f64 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(0.0, |h| h.demux.info().facts.fps)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0.0
    }
}

/// Total duration in seconds (0 when unknown/unbounded).
fn demux_duration_secs(ptr: usize) -> f64 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(0.0, |h| {
            h.demux
                .info()
                .facts
                .duration
                .map_or(0.0, |d| d.as_secs_f64())
        })
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0.0
    }
}

/// The video stream's `start_time` in stream time-base units, or `i64::MIN` when the
/// container declares none.
///
/// ⚠ This is what the presentation timeline is measured FROM, and the reader must take it
/// from here rather than from the first packet it happens to see. Those are the same thing
/// only when feeding begins at the top of the file — and a resume (task #94.2) begins at a
/// keyframe minutes in, which would silently redefine "zero" as the resume point and shift
/// the whole timeline under the clock, the scrubber, and the subtitles.
fn demux_start_time_units(ptr: usize) -> i64 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(i64::MIN, |h| {
            h.demux.info().facts.start_time.unwrap_or(i64::MIN)
        })
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        i64::MIN
    }
}

/// Clockwise display rotation (0/90/180/270) from the display matrix.
fn demux_rotation(ptr: usize) -> i32 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(0, |h| h.demux.info().facts.rotation)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0
    }
}

/// NAL length-prefix size (1/2/4); 0 when the stream is Annex B.
fn demux_nal_length_size(ptr: usize) -> u8 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(0, |h| h.demux.info().nal_length_size)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0
    }
}

/// Whether packets are length-prefixed (MP4/MKV) — the representation the
/// CMSampleBuffer path consumes directly. `false` routes to the Session fallback.
fn demux_length_prefixed(ptr: usize) -> bool {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).is_some_and(|h| h.demux.info().length_prefixed)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        false
    }
}

/// Whether the container carries an audio track (drives the presenter's audio start).
fn demux_has_audio(ptr: usize) -> bool {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).is_some_and(|h| h.demux.info().facts.has_audio)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        false
    }
}

/// Codec-private data (the hvcC/avcC atom) for the `CMVideoFormatDescription`.
fn demux_extradata(ptr: usize) -> Vec<u8> {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or_else(Vec::new, |h| h.demux.info().extradata.clone())
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        Vec::new()
    }
}

/// The Dolby Vision configuration box four-CC (`dvcC`/`dvvC`), or empty if the
/// stream carries no DoVi config. Paired with [`demux_dovi_box`].
fn demux_dovi_atom(ptr: usize) -> Vec<u8> {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr)
            .and_then(|h| h.demux.info().dovi.as_ref())
            .map_or_else(Vec::new, |d| d.atom.to_vec())
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        Vec::new()
    }
}

/// The 24-byte DoVi configuration record the host attaches to the sample
/// description under the `demux_dovi_atom` four-CC. Empty when no DoVi config.
fn demux_dovi_box(ptr: usize) -> Vec<u8> {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr)
            .and_then(|h| h.demux.info().dovi.as_ref())
            .map_or_else(Vec::new, |d| d.box_payload.to_vec())
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        Vec::new()
    }
}

/// Dolby Vision profile (0 = none), for diagnostics/routing.
fn demux_dovi_profile(ptr: usize) -> u8 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr)
            .and_then(|h| h.demux.info().dovi.as_ref())
            .map_or(0, |d| d.profile)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0
    }
}

/// Advance the demuxer to the next video access unit and return its compressed
/// bytes. An **empty** result means no packet this call — read [`demux_state`] to
/// tell EOF (1) from a demux error (2). On success the packet's PTS/DTS/duration/
/// keyframe flags are cached for the `demux_packet_*` getters. Exclusive `&mut`
/// (the reader queue serializes read/seek).
fn demux_read_packet(ptr: usize) -> Vec<u8> {
    #[cfg(feature = "ffvideo")]
    {
        if ptr == 0 {
            return Vec::new();
        }
        // SAFETY: nonzero `ptr` is a live box from `open_stashed_demux`; the reader
        // queue guarantees single-threaded, non-freed access.
        let h = unsafe { &mut *(ptr as *mut DemuxHandle) };
        match h.demux.read_packet() {
            Ok(Some(pkt)) => {
                h.last_pts = pkt.pts.unwrap_or(i64::MIN);
                h.last_dts = pkt.dts.unwrap_or(i64::MIN);
                h.last_duration = pkt.duration;
                h.last_is_key = pkt.is_key;
                h.state = 0;
                pkt.data
            }
            Ok(None) => {
                h.state = 1; // clean EOF — the presenter parks the last frame
                Vec::new()
            }
            Err(e) => {
                eprintln!("sample-buffer demux read failed: {e}");
                h.state = 2;
                Vec::new()
            }
        }
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        Vec::new()
    }
}

/// PTS of the last [`demux_read_packet`] in time-base units; `i64::MIN` if none.
fn demux_packet_pts(ptr: usize) -> i64 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(i64::MIN, |h| h.last_pts)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        i64::MIN
    }
}
/// DTS of the last read packet in time-base units; `i64::MIN` if none.
fn demux_packet_dts(ptr: usize) -> i64 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(i64::MIN, |h| h.last_dts)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        i64::MIN
    }
}
/// Duration of the last read packet in time-base units (0 if unknown).
fn demux_packet_duration(ptr: usize) -> i64 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(0, |h| h.last_duration)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        0
    }
}
/// Whether the last read packet is a sync sample (keyframe).
fn demux_packet_is_key(ptr: usize) -> bool {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).is_some_and(|h| h.last_is_key)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        false
    }
}

/// Demux state after the last read: 0 Ok, 1 Eof, 2 Failed.
fn demux_state(ptr: usize) -> u8 {
    #[cfg(feature = "ffvideo")]
    {
        demux_ref(ptr).map_or(2, |h| h.state)
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
        2
    }
}

/// Seek the demuxer to a keyframe at or before `secs`, clearing EOF. The host
/// flushes its renderer and re-enqueues from the next keyframe. Exclusive `&mut`.
fn demux_seek(ptr: usize, secs: f64) {
    #[cfg(feature = "ffvideo")]
    {
        if ptr == 0 {
            return;
        }
        // SAFETY: see `demux_read_packet`; the reader queue serializes read/seek.
        let h = unsafe { &mut *(ptr as *mut DemuxHandle) };
        let target = std::time::Duration::from_secs_f64(secs.max(0.0));
        match h.demux.seek(target) {
            Ok(()) => h.state = 0,
            Err(e) => {
                eprintln!("sample-buffer demux seek failed: {e}");
                h.state = 2;
            }
        }
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = (ptr, secs);
    }
}

/// Free the owned demuxer. Must be called **exactly once** per non-null pointer
/// from [`open_stashed_demux`] (the Swift wrapper guards this in `deinit`); a null
/// pointer is a no-op. After this the pointer is dangling.
fn demux_free(ptr: usize) {
    #[cfg(feature = "ffvideo")]
    {
        if ptr != 0 {
            // SAFETY: nonzero `ptr` is a live box from `open_stashed_demux`, freed
            // exactly once by the host's deinit-once contract.
            drop(unsafe { Box::from_raw(ptr as *mut DemuxHandle) });
        }
    }
    #[cfg(not(feature = "ffvideo"))]
    {
        let _ = ptr;
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
        // AVPlayer + AVPlayerLayer. PlayVideo(path, session_id, muted, start_secs) opens
        // the clip and presents it over the Metal canvas (revealed on the first frame;
        // the poster shows until then). start_secs>0 = seek there before revealing (the
        // session-only resume position, task #94.2). StopVideo(session_id) tears the
        // player down (navigate / delete / failure); stale callbacks rejected by id.
        PlayVideo(String, u64, bool, f64),
        // Play an archive (ZIP/7z) entry from in-RAM bytes (name, session_id, muted,
        // start_secs): no file URL, so the host pulls the container bytes via
        // take_pending_video_bytes() and serves them to AVPlayer through a custom
        // resource loader (never to disk). start_secs as in PlayVideo.
        PlayVideoBytes(String, u64, bool, f64),
        // Sample-buffer video (video-overhaul Phase 3): open the clip in the
        // AVSampleBufferDisplayLayer presenter (session_id, muted, start_secs). FFmpeg
        // (Rust) demuxes; the host opens the demuxer via open_stashed_demux(session_id)
        // off the main actor, wraps compressed packets into CMSampleBuffers, and hands
        // decode to VideoToolbox — the DoVi/HDR end-state for MKV/WebM that AVPlayer
        // can't demux. Reveal/stop/pause/seek reuse the native_video_* callback contract.
        PlaySampleBuffer(u64, bool, f64),
        // Generate a poster for a macOS archive video (request_id, item, name, max_edge): the
        // host pulls the entry's bytes via take_pending_poster_bytes(request_id), grabs a
        // frame with AVAssetImageGenerator, and returns it via video_poster_ready().
        RequestVideoPoster(u64, u64, String, u32),
        StopVideo(u64),
        // Pause / resume the native player (session_id). ResumeVideo also serves replay:
        // when the player is parked at EOS the host seeks to 0 before playing.
        PauseVideo(u64),
        ResumeVideo(u64),
        // A / Shift+A picked audio picker row N (task #99). The host routes it to whichever
        // presenter owns the file and reports the outcome back via audio_track_switched --
        // the same path the Playback > Audio menu uses, so a key and a click cannot drift.
        SelectAudioTrack(usize),
        // Seek the native player by a signed millisecond delta (session_id, generation,
        // delta_ms) — the ±2 s / Shift ±10 s arrow-seek. The host resolves it against
        // AVPlayer's clock, clamps to the seekable range, and reports back via
        // native_video_seek_completed(session_id, generation, finished).
        SeekVideoBy(u64, u64, i64),
        // Frame-step one frame forward/back (session_id, forward). The host pauses first
        // and no-ops when the item can't step that direction.
        StepVideo(u64, bool),
        // Mute/unmute the native player in place (session_id, muted).
        SetVideoMuted(u64, bool),
        // Session-video audio (task #84 §7): open the FFmpeg audio decoder for the
        // playing VideoSession (session_id, muted) — the host calls video_audio_open()
        // (the input is stashed Rust-side), builds its AVAudioEngine sink over the
        // video_audio_read/seek accessors, and reports the played-position clock back
        // ~4x/s via video_audio_clock(). Opens PAUSED; ResumeVideoAudio starts it with
        // the video preroll.
        StartVideoAudio(u64, bool),
        StopVideoAudio,
        PauseVideoAudio,
        ResumeVideoAudio,
        SeekVideoAudio(f64),
        SetVideoAudioMuted(bool),
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

    // What the host draws over the letterbox when an archive **door** is presented
    // (task #105) — a mirror of `pb_app_core::app_core::DoorCard`, plus the `visible` flag
    // its `Option` can't cross as. Semantic fields only: the artwork is a static asset the
    // host pulls once (`door_art_*`), never cloned into a per-pump snapshot.
    #[swift_bridge(swift_repr = "struct")]
    struct DoorCardFfi {
        // False on anything but a door — the other fields are then empty.
        visible: bool,
        // The full file name, e.g. `wedding-photos.zip`; the host middle-elides to fit and
        // keeps the whole of it for the tooltip.
        name: String,
        // The heading, e.g. `ZIP Archive`.
        format: String,
        // The Open key, from the live keymap — never hard-code `P`, it is rebindable.
        shortcut: String,
    }

    // The current item's on-screen placement for the native video layer (task 79.9
    // phase 3) — the same geometry the wgpu still renderer uses, so the AVPlayerLayer
    // tracks Fit/Fill/Original + zoom + pan + rotation like a photo. x/y/w/h are physical
    // px, top-left origin; w/h are the *rotated* footprint; rotation is the CW quadrant
    // (0/1/2/3 = 0/90/180/270). valid = false before the renderer/fit exist.
    #[swift_bridge(swift_repr = "struct")]
    struct VideoPlacementFfi {
        valid: bool,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        rotation: u8,
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

    // The Subtitles tab's flat form — a mirror of pb_app_core::subtitle::SubtitleStyle
    // (task #90.4).
    //
    // DELIBERATELY SEPARATE from SettingsFormFfi rather than 28 more fields on it. Three
    // reasons: that struct is already 37 fields; the preview needs the DRAFT style on
    // every slider tick, which would mean shipping all 65 fields per frame to render one
    // swatch; and the two panes then debounce independently.
    //
    // Every size is a % of VIEWPORT HEIGHT, never points — sized against the viewport it
    // reads the same on a 1x ultrawide and a 2x Studio, which is the whole point of a
    // legibility setting. The Swift side shows them as percentages; nothing converts to
    // pixels until `to_params`.
    //
    // Shapes swift-bridge forces: `font_family` "" = the system font (no Option<String>);
    // `shadow_on` + flat dx/dy/blur/rgba stands in for Option<Shadow>; every [u8; 4]
    // colour is four fields (no arrays cross).
    #[swift_bridge(swift_repr = "struct")]
    struct SubtitleStyleFfi {
        // "" = system font. Otherwise one of pb_app_core::subtitle::FONT_CHOICES — a
        // curated shortlist, not an enumeration (owner call: fontdb finds ~1114 faces,
        // nearly all unusable as subtitles). The stored value is a NAME, so growing this
        // into a full picker later never invalidates a saved setting.
        font_family: String,
        // % of viewport height, as a fraction. The ONE size that is viewport-relative.
        size_pct: f32,
        color_r: u8,
        color_g: u8,
        color_b: u8,
        // MASTER opacity 0..=1 — the whole subtitle faded as one object, NOT the text
        // colour's alpha (which stays out of the form and is preserved from the base).
        // Fading the glyphs alone makes them translucent onto their own outline, which
        // shows through and defeats the point.
        opacity: f32,
        // Every field below in px is on the REFERENCE_FONT_PX scale: stored as a fraction
        // of the real font size (so it holds its proportions as the text resizes) and
        // shown as the px it would be at the default text size, because "0.06" is not a
        // quantity anyone can picture. See pb_app_core::subtitle's unit rule.
        outline_px: f32,
        outline_r: u8,
        outline_g: u8,
        outline_b: u8,
        outline_a: u8,
        // Option<Shadow>, flattened. `shadow_on` false = None; the rest keep their last
        // values so toggling a shadow off and on again doesn't forget how it was tuned.
        shadow_on: bool,
        shadow_dx_px: f32,
        shadow_dy_px: f32,
        shadow_blur_px: f32,
        shadow_r: u8,
        shadow_g: u8,
        shadow_b: u8,
        shadow_a: u8,
        // background_a 0 = no background: the alpha IS the on/off, so there is no toggle
        // that can disagree with the colour it guards.
        background_r: u8,
        background_g: u8,
        background_b: u8,
        background_a: u8,
        // SIGNED, from the video's bottom edge: 0 = on the edge, >0 up into the picture,
        // <0 DOWN INTO THE LETTERBOX (the owner's ask — the thing almost no player gets
        // right). See pb_app_core::subtitle::place.
        vertical_offset_pct: f32,
        max_line_pct: f32,
        line_spacing: f32,
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
        subtitles: bool,
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
        // Does the core own this chord? The host consumes the event if so — see the impl:
        // it is how a bound ⌘ chord acts without AppKit beeping at the unmatched equivalent.
        fn key_is_bound(&self, key: &str, ctrl: bool, shift: bool, alt: bool, logo: bool) -> bool;
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
        // Toolbar hold-to-blaze (task #55): press-and-hold a nav/random button to blaze.
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
        fn effective_letterbox_rgb(&self) -> u32;
        fn native_video_session_id(&self) -> u64;
        fn native_video_opened(&mut self, session_id: u64, duration_ms: i64, has_audio: bool);
        fn native_video_state_changed(&mut self, session_id: u64, state: u8);
        fn native_video_ended(&mut self, session_id: u64);
        fn native_video_seek_completed(&mut self, session_id: u64, generation: u64, finished: bool);
        fn native_video_failed(&mut self, session_id: u64, error: String, recoverable: bool);
        // Native player position + duration (seconds), reported each pump for the
        // session-only resume map (task #94.2).
        fn native_video_progress(
            &mut self,
            session_id: u64,
            position_secs: f64,
            duration_secs: f64,
        );
        fn video_session_active(&self) -> bool;
        // Is a video on screen on EITHER backend (Native included) -- the Playback menu's
        // enable check. video_session_active() is false for MKV/WebM and would disable the
        // flyout for exactly the files that carry subtitle tracks.
        fn video_showing(&self) -> bool;
        fn video_session_elapsed_secs(&self) -> f64;
        fn video_session_duration_secs(&self) -> f64;
        fn video_session_playing(&self) -> bool;
        fn video_seek_fraction(&mut self, frac: f32);
        // Session-video audio (task #84 §7, plan §7/1E): the FFmpeg decoder is
        // owned OFF the main actor behind a usize pointer — the host opens it via
        // open_stashed_session_audio() (input stashed Rust-side by StartVideoAudio)
        // on its serial feeder queue, drives it through the session_audio_*(ptr,…)
        // free functions, and frees it exactly once. session_audio_state(ptr)
        // returns 0 Ok / 1 Eof / 2 Failed so a decode error is distinct from EOF.
        // Only the clock sample still rides the core handle (main-actor, cheap).
        fn open_stashed_session_audio(session_id: u64) -> usize;
        fn session_audio_rate(ptr: usize) -> u32;
        fn session_audio_channels(ptr: usize) -> u32;
        // Audio track selection (task #99). set_track re-opens IN PLACE, so the pointer
        // stays valid; false = the switch failed and the old track is still playing (a
        // failed switch must cost the choice, not the sound). stream_index reports what is
        // ACTUALLY playing -- the picker's tick reads this rather than re-deriving the
        // policy, so it can never disagree with what you hear. After a true, the caller
        // must rebuild its format: rate/channels differ between tracks.
        fn session_audio_set_track(ptr: usize, track: usize) -> bool;
        fn session_audio_stream_index(ptr: usize) -> i64;
        fn session_audio_read(ptr: usize, max_frames: u32) -> Vec<f32>;
        fn session_audio_state(ptr: usize) -> u8;
        fn session_audio_seek(ptr: usize, secs: f64) -> f64;
        fn session_audio_free(ptr: usize);
        // Sample-buffer video demux (video-overhaul Phase 3): the host opens the
        // FFmpeg demuxer via open_stashed_demux(session_id) (input stashed Rust-side by
        // PlaySampleBuffer) on its serial reader queue, reads the format-description
        // facts once, then pulls compressed packets to enqueue into an
        // AVSampleBufferDisplayLayer. demux_read_packet advances + caches the packet's
        // timing/keyframe flags (read via demux_packet_*); demux_state is 0 Ok/1 Eof/2
        // Failed. Freed exactly once via demux_free.
        fn open_stashed_demux(session_id: u64) -> usize;
        fn demux_codec(ptr: usize) -> u8;
        fn demux_width(ptr: usize) -> u32;
        fn demux_height(ptr: usize) -> u32;
        fn demux_time_base_num(ptr: usize) -> i32;
        fn demux_time_base_den(ptr: usize) -> i32;
        fn demux_fps(ptr: usize) -> f64;
        fn demux_duration_secs(ptr: usize) -> f64;
        fn demux_start_time_units(ptr: usize) -> i64;
        fn demux_rotation(ptr: usize) -> i32;
        fn demux_nal_length_size(ptr: usize) -> u8;
        fn demux_length_prefixed(ptr: usize) -> bool;
        fn demux_has_audio(ptr: usize) -> bool;
        fn demux_extradata(ptr: usize) -> Vec<u8>;
        fn demux_dovi_atom(ptr: usize) -> Vec<u8>;
        fn demux_dovi_box(ptr: usize) -> Vec<u8>;
        fn demux_dovi_profile(ptr: usize) -> u8;
        fn demux_read_packet(ptr: usize) -> Vec<u8>;
        fn demux_packet_pts(ptr: usize) -> i64;
        fn demux_packet_dts(ptr: usize) -> i64;
        fn demux_packet_duration(ptr: usize) -> i64;
        fn demux_packet_is_key(ptr: usize) -> bool;
        fn demux_state(ptr: usize) -> u8;
        fn demux_seek(ptr: usize, secs: f64);
        fn demux_free(ptr: usize);
        fn video_audio_clock(&mut self, session_id: u64, state: u8, position_secs: f64);
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

        // The subtitle track picker (task #99): subtitle_picker_refresh() as the popover /
        // the Playback > Subtitles flyout opens, then read the indexed accessors. Row i is
        // the argument select_subtitle_track takes. Row 0 is always Off (a real choice), so
        // a count of 1 means "this file has no readable subtitle tracks" -- but only once
        // subtitle_tracks_known() is true; before that, 0 rows means "still reading".
        fn subtitle_picker_refresh(&mut self);
        fn subtitle_track_count(&self) -> usize;
        fn subtitle_track_label(&self, i: usize) -> String;
        fn subtitle_track_active(&self, i: usize) -> bool;
        fn subtitle_tracks_known(&self) -> bool;
        fn subtitles_on(&self) -> bool;
        fn select_subtitle_track(&mut self, i: usize);

        // The audio track picker (task #99). Same snapshot-then-read shape, but the TICK is
        // the host's to report: only it knows what is coming out of the speakers, so the
        // core formats and the host answers via set_active_audio_row. The two routes speak
        // different currencies -- ff_stream for the sample-buffer/FFmpeg route, av_plist for
        // AVPlayer -- so the host dispatches on whichever answers.
        fn audio_picker_refresh(&mut self);
        fn audio_track_count(&self) -> usize;
        fn audio_track_label(&self, i: usize) -> String;
        fn audio_track_active(&self, i: usize) -> bool;
        fn audio_track_ff_stream(&self, i: usize) -> i64;
        fn audio_track_av_plist(&self, i: usize) -> Vec<u8>;
        fn set_active_audio_row(&mut self, row: i64);
        fn audio_track_switched(&mut self, row: usize, ok: bool);

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
        // The Thumbnails strip (task #83).
        fn thumbs_visible(&self) -> bool;
        fn left_tab(&self) -> u8;
        fn thumb_count(&self) -> usize;
        fn thumb_current(&self) -> i64;
        fn thumb_dirty(&self) -> u64;
        fn thumb_gen(&self, i: usize) -> u64;
        fn thumb_width(&self, i: usize) -> u32;
        fn thumb_height(&self, i: usize) -> u32;
        fn thumb_rgba(&self, i: usize) -> Vec<u8>;
        fn thumb_name(&self, i: usize) -> String;
        fn thumb_archive(&self, i: usize) -> bool;
        fn thumb_badge(&self, i: usize) -> u8;
        fn thumb_rotation(&self, i: usize) -> u8;
        fn thumb_failed(&self, i: usize) -> bool;
        fn thumb_click(&mut self, i: usize);
        fn thumbs_set_viewport(
            &mut self,
            vis_lo: usize,
            vis_hi: usize,
            over_lo: usize,
            over_hi: usize,
        );
        fn thumbs_user_scrolled(&mut self);
        fn thumb_scroll_item(&self) -> i64;
        fn thumb_scroll_gen(&self) -> u64;
        fn take_thumb_scroll(&mut self);
        fn thumbs_scroll_done(&mut self, gen: u64);
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
        fn flash_video_controls(&mut self);
        fn take_pending_video_bytes(&mut self) -> Vec<u8>;
        fn take_pending_poster_bytes(&mut self, request_id: u64) -> Vec<u8>;
        fn video_poster_ready(
            &mut self,
            request_id: u64,
            item: u64,
            w: u32,
            h: u32,
            data_ptr: usize,
            len: usize,
        );
        fn archive_video_meta_ready(
            &mut self,
            item: u64,
            codec: String,
            fps_milli: u32,
            duration_ms: i64,
            has_audio: bool,
        );
        fn video_placement(&self) -> VideoPlacementFfi;

        // Subtitles (task #90): a premultiplied RGBA8 overlay + where it goes. Pulled on
        // a generation change only, like the thumbnail pixels.
        fn subtitle_gen(&self) -> u64;
        fn subtitle_width(&self) -> u32;
        fn subtitle_height(&self) -> u32;
        fn subtitle_rgba(&self) -> Vec<u8>;
        fn subtitle_rect(&self) -> VideoPlacementFfi;
        fn info_line_align(&self) -> u8;
        fn play_hint_kind(&self) -> u8;
        fn play_hint_seq(&self) -> u64;
        // The archive door card (task #105) + its artwork. The card is a per-pump snapshot;
        // the artwork is a static asset the host pulls exactly once and caches.
        fn door_card(&self) -> DoorCardFfi;
        fn door_art_width() -> u32;
        fn door_art_height() -> u32;
        fn door_art_rgba() -> Vec<u8>;
        fn settings_form(&self) -> SettingsFormFfi;
        fn settings_edited(&mut self, form: SettingsFormFfi);

        // The Subtitles settings tab (task #90.4). Its own pull/push pair, separate from
        // the 37-field settings form — see SubtitleStyleFfi.
        fn subtitle_style_form(&self) -> SubtitleStyleFfi;
        fn subtitle_style_edited(&mut self, form: SubtitleStyleFfi);
        // Behaviour, not style — deliberately NOT on SubtitleStyleFfi, which drives the
        // preview swatch (task #99).
        fn forced_subtitles(&self) -> bool;
        fn set_forced_subtitles(&mut self, on: bool);
        // The curated font list the picker shows. Indexed accessors, not a Vec<String>:
        // that does not cross back to Swift (same reason the keymap editor is indexed).
        fn subtitle_font_count() -> usize;
        fn subtitle_font_name(i: usize) -> String;
        // The slider bounds, from the SAME constants `SubtitleStyle::clamped` uses — so a
        // control can never offer a value the clamp quietly takes back. A slider that
        // snaps when you let go is worse than one that never went there.
        fn subtitle_max_size_pct() -> f32;
        fn subtitle_max_shadow_blur_px() -> f32;
        fn subtitle_max_shadow_offset_px() -> f32;
        fn subtitle_max_line_spacing() -> f32;
        // The shipped defaults, for the pane's Restore Defaults button.
        //
        // ⚠ A free function, NOT a Swift-side copy of the numbers. The draft used to
        // hard-code them, which meant Restore Defaults and a fresh config could hand back
        // different looks the moment either side was tuned — and tuning is the whole point
        // of this pane. One source, so they cannot drift.
        fn subtitle_style_defaults() -> SubtitleStyleFfi;
        // The live preview swatch: RGBA8, w*h, top-left origin. Takes the DRAFT style so
        // it tracks a slider drag with no save round-trip. Drawn with the SAME rasterizer,
        // to_params, and place() the real overlay uses, so it cannot drift from what a
        // film actually shows. Costs one shape+raster (~0.15 ms) plus the backdrop fill.
        fn subtitle_preview_rgba(&mut self, form: SubtitleStyleFfi, w: u32, h: u32) -> Vec<u8>;
        // Whether the preview can draw yet. FontSystem::new() is 261 ms, so it is built on
        // a worker; until it lands the preview would be a blank frame. The pane shows a
        // placeholder rather than a lie.
        fn subtitle_preview_ready(&mut self) -> bool;

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
        // The never-consumed launch-path record — the host's Apple-Event echo filter
        // (a bare-path launch delivers the same path via argv AND a document-open).
        fn launch_path_count(&self) -> usize;
        fn launch_path_at(&self, i: usize) -> String;
        // The --metrics end-of-run summary ("" when off) — printed by the host on quit.
        fn metrics_report(&self) -> String;

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

    /// The host consumes a key the core owns and passes on one it doesn't — the rule that
    /// lets ⌘↓ (Open) act without AppKit beeping at an unmatched key equivalent, while the
    /// menu's own ⌘O still reaches the menu.
    #[test]
    fn key_is_bound_answers_for_the_hosts_consume_decision() {
        let h = test_handle(1920, 1080, 2.0);
        // ⌘↓ / ⌥↓: the keymap's Open aliases — the core's, so the host eats them. The name
        // is `"Down"`, the string `KeyMap.pbKeyName` hands over for keyCode 0x7D — this
        // query is only ever right if it speaks the host's vocabulary.
        assert!(h.key_is_bound("Down", false, false, false, true));
        assert!(h.key_is_bound("Down", false, false, true, false));
        // ⌘O is a *menu* accelerator, deliberately not in the keymap — the host must pass
        // it on, or Open File would stop working.
        assert!(!h.key_is_bound("O", false, false, false, true));
        // Bare P is bound (and consumed); an unknown key name never claims anything.
        assert!(h.key_is_bound("P", false, false, false, false));
        assert!(!h.key_is_bound("NotAKey", false, false, false, true));
    }

    /// A door on a deck of two items, presented — the fixture the door-card tests share.
    /// `FsSource` types items off the path, so no archive has to exist on disk for the
    /// card to describe one, which is the whole point: naming a door costs no read.
    fn handle_on_a_door() -> AppCoreHandle {
        let mut h = test_handle(1920, 1080, 2.0);
        let dir = std::env::temp_dir().join("pb_ffi_door_card");
        let src: std::sync::Arc<dyn pb_source::ItemSource> =
            std::sync::Arc::new(pb_source::FsSource::new(vec![
                dir.join("photo.jpg"),
                dir.join("wedding-photos.zip"),
            ]));
        h.core
            .rebuild_playlist(src, dir.clone(), Some(dir), false, 0);
        h.core.displayed_item = Some(1); // the door
        h
    }

    /// The card the host draws over the letterbox (task #105): visible only on a door, and
    /// carrying everything the SwiftUI view needs — because a door's frame is a 1×1
    /// transparent sentinel, so if the card is wrong or missing, the screen is *blank*.
    #[test]
    fn the_door_card_crosses_with_its_name_format_and_live_shortcut() {
        let mut h = handle_on_a_door();

        let card = h.door_card();
        assert!(card.visible);
        assert_eq!(card.name, "wedding-photos.zip", "the name, not the path");
        assert_eq!(card.format, "ZIP Archive");
        assert!(
            !card.shortcut.is_empty(),
            "the live keymap's Open key — the view must never hard-code P"
        );

        // On a photo the card is absent, not merely empty: the host gates its whole
        // overlay on `visible`, and a card floating over a picture would be a bug.
        h.core.displayed_item = Some(0);
        let card = h.door_card();
        assert!(!card.visible);
        assert!(card.name.is_empty() && card.format.is_empty());
    }

    /// The artwork the card and the strip's archive cells draw. It is the **only** thing
    /// on a door's screen with pixels in it, so "it decodes" is load-bearing — and it
    /// crosses as straight-alpha RGBA8, which is what tells the host to build its CGImage
    /// with `.last` rather than the subtitle overlay's `.premultipliedLast`.
    #[test]
    fn the_door_artwork_decodes_and_crosses_as_straight_alpha_rgba8() {
        let (w, h) = (door_art_width(), door_art_height());
        assert!(w > 0 && h > 0, "the asset decodes");
        let rgba = door_art_rgba();
        assert_eq!(
            rgba.len(),
            (w as usize) * (h as usize) * 4,
            "RGBA8, four bytes a pixel — the host builds a CGImage straight off this"
        );
        // Cropped to its content, so the card's padding means what it says: an edge that
        // is entirely transparent means the crop didn't run.
        assert!(
            rgba.chunks_exact(4).any(|px| px[3] > 0),
            "the artwork has ink in it"
        );
    }

    /// An archive cell in the Thumbnails strip draws the artwork instead of a thumbnail —
    /// it has none, and asking for one would mean decompressing an archive nobody opened.
    /// The strip has no more right to do that than the prefetch ring does.
    #[test]
    fn the_strip_types_a_door_without_reading_it() {
        let h = handle_on_a_door();
        assert!(h.thumb_archive(1), "the door");
        assert!(!h.thumb_archive(0), "the photo");
        assert_eq!(
            h.thumb_badge(1),
            0,
            "no badge on a door — the folder artwork already says what it is"
        );
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
        let help = pf(&["blazeviewer", "--help"]);
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
            vec!["blazeviewer".into(), "--help".into()],
            "9.9.9-test".into(),
            true,
            false,
        );
        assert!(
            colored.text.contains('\u{1b}'),
            "TTY stdout renders ANSI-styled help"
        );

        let ver = pf(&["blazeviewer", "--version"]);
        assert!(!ver.proceed);
        assert_eq!(ver.exit_code, 0);
        assert!(
            ver.text.contains("9.9.9-test"),
            "--version prints the host-supplied bundle string"
        );
        assert!(
            ver.text.contains("Blaze Viewer"),
            "--version wears the product name (display_name), not the bin name"
        );

        let bad = pf(&["blazeviewer", "--nope"]);
        assert!(!bad.proceed);
        assert_eq!(bad.exit_code, 2);
        assert!(bad.use_stderr, "usage errors go to stderr");

        // Mixed strictness: a nonexistent path is a usage error with the winit shell's
        // exact message (argv[0] regression for positionals too — the path is argv[1]).
        let missing = pf(&["blazeviewer", "/definitely/not/here.jpg"]);
        assert!(!missing.proceed);
        assert_eq!(missing.exit_code, 2);
        assert!(missing.use_stderr);
        assert!(missing
            .text
            .contains("no such file or folder: /definitely/not/here.jpg"));

        // A clean flag-only launch proceeds with no text.
        let ok = pf(&["blazeviewer", "--shuffle", "--theme", "dark"]);
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
                "blazeviewer".into(),
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
                "blazeviewer".into(),
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
                "blazeviewer".into(),
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
            vec!["blazeviewer".into(), "--pb-open".into(), dir],
            "v".into(),
        );
        assert!(h.open_launch_paths(), "first call opens the stashed paths");
        assert!(!h.open_launch_paths(), "second call is a no-op");
        // With nothing stashed at all, it is also a no-op.
        let mut empty = test_handle(800, 600, 1.0);
        empty.apply_launch_args(vec!["blazeviewer".into()], "v".into());
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
        // Wait for the photo to bootstrap AND its metadata to load. Decodes moved
        // off the event loop (#18.5), so `current` (the displayed photo's meta) now
        // populates asynchronously via a decode-outcome drain in `tick` — the same
        // precondition the real app has before "Copy Image Details" has dimensions.
        while (h.core.playlist.current().is_none() || h.core.current.is_none())
            && Instant::now() < deadline
        {
            h.tick();
            let _ = drain(&mut h);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(h.core.playlist.current(), Some(0), "fixture bootstraps");
        assert!(h.core.current.is_some(), "photo metadata loaded");
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

        // Submit → Checking now, then BeginArchiveOpen (intercepted onto this crate's
        // worker during the drain — `next_effect` calls `begin_archive_open`).
        h.password_submitted("hunter2".to_string());
        let effects = drain(&mut h);
        assert!(effects
            .iter()
            .any(|e| matches!(e, ffi::CoreEffectFfi::SetDialogChecking)));

        // A 7z opens on a worker thread (`ArchiveKind::background_open`), so the failure —
        // the bogus path can't be opened — lands via `poll_archive_load` on a later tick,
        // not inline (this is why the old drain-only assertion broke once 7z went async in
        // #102). Pump `tick()` until the load clears; bounded so a genuine hang fails the
        // test instead of blocking CI. The worker errors on a missing file almost at once,
        // so this is one or two iterations in practice.
        let mut effects = Vec::new();
        let mut settled = false;
        for _ in 0..400 {
            if h.archive_load.is_none() {
                settled = true;
                break;
            }
            h.tick();
            effects.extend(drain(&mut h));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            settled,
            "the worker never reported — the archive load is stuck"
        );
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

    // ── Session-video audio: the owned off-main decoder seam (plan §7/1E) ──
    //
    // These exercise the usize-pointer FFI (open/read/seek/state/free) + the
    // global stash directly, the way the Swift `OwnedAudioDecoder` will on its
    // feeder queue. The `AUDIO_STASH` is one global slot, so the tests serialize
    // through `AUDIO_TEST_LOCK` (cargo runs tests in parallel by default).
    #[cfg(feature = "ffvideo")]
    mod session_audio {
        use super::*;
        use pb_app_core::video::VideoInput;
        use std::sync::{Arc, Mutex};

        static AUDIO_TEST_LOCK: Mutex<()> = Mutex::new(());

        fn stash_present(session_id: u64) -> bool {
            AUDIO_STASH
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|(id, _)| *id == session_id)
        }

        /// A valid audio container (the pb-decode AAC-tone fixture) as in-RAM
        /// bytes — no path, so the test is location-independent.
        fn valid_input() -> VideoInput {
            let bytes = include_bytes!("../../pb-decode/tests/fixtures/video/color_with_tone.mp4");
            VideoInput::Bytes {
                data: Arc::new(bytes.to_vec()),
                name: "clip.mp4".into(),
            }
        }

        /// The audit's consume-on-failure bug (plan §3/§7/1E): a failed open must
        /// LEAVE the stash so the host can retry without the core re-issuing the
        /// effect. Garbage bytes fail to open; the stash survives.
        #[test]
        fn open_failure_keeps_the_stash_for_retry() {
            let _g = AUDIO_TEST_LOCK.lock().unwrap();
            let junk = VideoInput::Bytes {
                data: Arc::new(vec![0x77u8; 4096]),
                name: "junk.mp4".into(),
            };
            stash_audio_input(101, junk);
            assert_eq!(open_stashed_session_audio(101), 0, "junk fails to open");
            assert!(stash_present(101), "stash survives a failed open");
            // A mismatched session id never opens someone else's stash.
            assert_eq!(open_stashed_session_audio(999), 0);
            assert!(stash_present(101), "and leaves it in place");
            *AUDIO_STASH.lock().unwrap() = None; // clean up the slot
        }

        /// Success consumes the stash and yields a live pointer; the full
        /// lifecycle (rate/channels/read → Eof → free) runs off the handle.
        #[test]
        fn open_success_consumes_stash_and_streams_to_eof() {
            let _g = AUDIO_TEST_LOCK.lock().unwrap();
            stash_audio_input(202, valid_input());
            let ptr = open_stashed_session_audio(202);
            assert_ne!(ptr, 0, "the fixture opens");
            assert!(!stash_present(202), "consumed on success");

            assert!(session_audio_rate(ptr) >= 22_050);
            assert!(session_audio_channels(ptr) >= 1);
            assert_eq!(session_audio_state(ptr), 0, "Ok: more to read");

            let first = session_audio_read(ptr, 4800);
            assert!(!first.is_empty(), "real audio");
            // Drain to the end.
            while !session_audio_read(ptr, 48_000).is_empty() {}
            assert_eq!(session_audio_state(ptr), 1, "clean EOF, not Failed");

            session_audio_free(ptr); // exactly once
        }

        /// R12: a decoder error is a DISTINCT state, never a clean EOF. A null
        /// pointer (the host's "no decoder" sentinel) reads as Failed, not Eof —
        /// so a corrupt/absent tail is never mistaken for the end of the stream.
        #[test]
        fn state_maps_failure_apart_from_eof() {
            let _g = AUDIO_TEST_LOCK.lock().unwrap();
            assert_eq!(session_audio_state(0), 2, "null → Failed");
            assert_eq!(session_audio_read(0, 4800).len(), 0, "null read is empty");
            assert_eq!(session_audio_rate(0), 0);
            assert_eq!(session_audio_channels(0), 0);
            session_audio_free(0); // null free is a no-op
        }

        /// Seek repositions the owned decoder and returns a plausible landing
        /// anchor; reads continue afterward (the post-seek clock epoch).
        #[test]
        fn owned_decoder_seeks_and_continues() {
            let _g = AUDIO_TEST_LOCK.lock().unwrap();
            stash_audio_input(303, valid_input());
            let ptr = open_stashed_session_audio(303);
            assert_ne!(ptr, 0);
            let _ = session_audio_read(ptr, 4800); // establish origin
            let anchor = session_audio_seek(ptr, 0.5);
            assert!(anchor >= 0.0, "landing anchor is a real position");
            assert_eq!(session_audio_state(ptr), 0, "still Ok after seek");
            session_audio_free(ptr);
        }
    }

    // ---- The Subtitles tab's form (task #90.4) --------------------------------

    /// The round trip that keeps a tuned look tuned. Every axis, including the ones the
    /// FFI has to reshape (Option<String>, Option<Shadow>, four [u8; 4] colours).
    #[test]
    fn a_subtitle_style_survives_the_form_round_trip() {
        let want = pb_app_core::subtitle::SubtitleStyle {
            font_family: Some("Verdana".into()),
            size_pct: 0.061,
            color: [255, 240, 10, 220],
            outline_ratio: 0.04,
            outline_color: [10, 20, 30, 250],
            shadow: Some(pb_app_core::subtitle::Shadow {
                dx_ratio: 0.03,
                dy_ratio: 0.042,
                blur_ratio: 0.051,
                color: [1, 2, 3, 180],
            }),
            background: [4, 5, 6, 153],
            background_radius_ratio: 0.007,
            background_pad_ratio: 0.009,
            vertical_offset_pct: -0.11,
            max_line_pct: 0.83,
            line_spacing: 1.35,
            opacity: 0.65,
        };
        let got = fold_subtitle_style_form(&want, &subtitle_style_to_form(&want));
        assert_eq!(got, want);
    }

    /// The master opacity crosses the form; the text colour's own alpha does not (it has
    /// no control) and is preserved from the base. Fading the glyphs alone would make them
    /// translucent onto their own outline — see `SubtitleStyle::opacity`.
    #[test]
    fn the_master_opacity_crosses_and_the_text_alpha_is_preserved() {
        let base = pb_app_core::subtitle::SubtitleStyle {
            color: [255, 255, 255, 200], // a config-set text alpha, no control for it
            opacity: 1.0,
            ..Default::default()
        };
        let mut form = subtitle_style_to_form(&base);
        assert_eq!(form.opacity, 1.0);
        form.opacity = 0.4; // the user drags Opacity
        let got = fold_subtitle_style_form(&base, &form);
        assert_eq!(got.opacity, 0.4, "the master lands");
        assert_eq!(
            got.color[3], 200,
            "...and the text's own alpha is untouched"
        );
    }

    /// ⚠ The reason the fold takes a `base`. The radius and padding have no controls (a
    /// good rounded corner is a look, not a preference — owner, 2026-07-15), so without
    /// the base every edit in the pane would silently reset a hand-tuned config value.
    #[test]
    fn fields_with_no_control_survive_an_edit() {
        let tuned = pb_app_core::subtitle::SubtitleStyle {
            background_radius_ratio: 0.44,
            background_pad_ratio: 0.55,
            ..Default::default()
        };
        let mut form = subtitle_style_to_form(&tuned);
        form.size_pct = 0.09; // the user drags the size slider
        let got = fold_subtitle_style_form(&tuned, &form);
        assert_eq!(got.size_pct, 0.09, "the edit lands");
        assert_eq!(
            got.background_radius_ratio, 0.44,
            "...and the rest is preserved"
        );
        assert_eq!(got.background_pad_ratio, 0.55);
    }

    /// The px readouts the sliders show are on the REFERENCE_FONT_PX scale, not raw
    /// ratios — a slider labelled "0.06" is not a control anyone can use.
    #[test]
    fn the_form_carries_px_not_ratios() {
        let s = pb_app_core::subtitle::SubtitleStyle {
            outline_ratio: 0.06,
            ..Default::default()
        };
        let f = subtitle_style_to_form(&s);
        assert!(
            (f.outline_px - 0.06 * pb_app_core::subtitle::REFERENCE_FONT_PX).abs() < 0.01,
            "outline crosses as ~2.85 px, not 0.06: {}",
            f.outline_px
        );
    }

    /// The FFI cannot carry `Option<String>`, so "System" is `""` on the wire. It must
    /// mean exactly what an absent `font_family` means — not a hunt for a face named "".
    #[test]
    fn the_empty_font_name_round_trips_as_the_system_font() {
        let mut s = pb_app_core::subtitle::SubtitleStyle::default();
        assert_eq!(s.font_family, None);
        assert_eq!(subtitle_style_to_form(&s).font_family, "", "None -> \"\"");

        // ...and back the other way, including the whitespace a text field could produce.
        for name in ["", "   "] {
            let mut f = subtitle_style_to_form(&s);
            f.font_family = name.into();
            assert_eq!(
                fold_subtitle_style_form(&s, &f).font(),
                None,
                "{name:?} = System"
            );
        }
        s.font_family = Some("Georgia".into());
        assert_eq!(subtitle_style_to_form(&s).font_family, "Georgia");
    }

    /// Turning a shadow off must not forget how it was tuned — the values ride across so
    /// toggling it back on restores the shadow you had, not the default one.
    #[test]
    fn a_shadow_toggled_off_keeps_its_values_for_when_it_comes_back() {
        let tuned = pb_app_core::subtitle::Shadow {
            dx_ratio: 0.1,
            dy_ratio: 0.02,
            blur_ratio: 0.03,
            color: [9, 8, 7, 111],
        };
        let on = pb_app_core::subtitle::SubtitleStyle {
            shadow: Some(tuned),
            ..Default::default()
        };
        let mut form = subtitle_style_to_form(&on);
        form.shadow_on = false;
        // Off means None...
        assert_eq!(fold_subtitle_style_form(&on, &form).shadow, None);
        // ...but the tuning survived the trip, so flipping it back returns what you had.
        form.shadow_on = true;
        assert_eq!(fold_subtitle_style_form(&on, &form).shadow, Some(tuned));
    }

    /// A style with no shadow still hands the pane usable slider values, rather than
    /// zeroes that would make "turn the shadow on" produce an invisible shadow.
    #[test]
    fn a_style_with_no_shadow_still_offers_default_shadow_values() {
        let none = pb_app_core::subtitle::SubtitleStyle::default();
        assert_eq!(none.shadow, None);
        let f = subtitle_style_to_form(&none);
        assert!(!f.shadow_on);
        assert!(
            f.shadow_blur_px > 0.0,
            "a shadow you switch on must be visible, not an invisible no-op at zero"
        );
        assert!(f.shadow_a > 0);
    }

    /// The font list the picker renders: indexed, and out-of-range degrades to "" (the
    /// system font) rather than panicking across the FFI.
    #[test]
    fn the_font_list_is_indexable_and_total() {
        let n = subtitle_font_count();
        assert!(n > 0);
        for i in 0..n {
            assert!(!subtitle_font_name(i).is_empty());
        }
        assert_eq!(
            subtitle_font_name(n),
            "",
            "past the end = System, not a panic"
        );
        assert_eq!(subtitle_font_name(9999), "");
    }

    /// The pane echoes its form back on open; that echo must never reach the disk. The
    /// no-op guard is what stops it — and it has to survive the clamp, since the form
    /// arrives unclamped and the saved value is clamped.
    #[test]
    fn a_saved_style_round_trips_to_itself_so_the_open_echo_is_a_no_op() {
        let saved = pb_app_core::subtitle::SubtitleStyle::default().clamped();
        let echoed = fold_subtitle_style_form(&saved, &subtitle_style_to_form(&saved)).clamped();
        assert_eq!(echoed, saved, "opening the pane must not look like an edit");
    }
}
