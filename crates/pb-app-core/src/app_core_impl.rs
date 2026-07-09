//! `impl AppCore` — the orchestration methods (NS0 5.5 / Phase B).
//!
//! The ~62 pure-core methods that used to live on the winit `impl App` (nav/prefetch/
//! residency, view zoom/pan/rotate/fit, HUD build, animation playback, undo/misc). They
//! reference only core-owned state (`self.*`, formerly `self.*`) + the relocated
//! `engine` helpers + the engine crates — never winit/muda/egui/rfd. Moved verbatim
//! (behavior-preserving); the shell now calls them as `self.<method>()`.

#![allow(clippy::too_many_arguments)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::HashSet;

use pb_core::{full_ring, prefetch_targets, prefetch_targets_scanning, Playlist, ResidentRing};
use pb_decode::{read_exif_fields, FitBox};
use pb_render::{test_pattern, Rotation, ScaleMode, ViewTransform, MAX_ZOOM, MIN_ZOOM};
use pb_source::{FsSource, PhotoSource};

use hud::Row;
use pb_hud::{hud, icon};

use crate::animation::{AnimDecode, AnimWant, Playback, Prepared};
use crate::contract;
use crate::decode_pool::Outcome;
use crate::engine::*;
use crate::keymap::Keymap;
use crate::panels::{
    DescribeBody, DescribePanel, DetailRow, DetailsPanel, HelpPanel, HelpSection, TextBody,
    TextPanel,
};
use crate::pb_key::PbKey;
use crate::{
    settings, slideshow, timing, Action, AppCore, InspectorTab, NativeToast, Nav, Panels,
    SlotContent, Toast, ToastIcon, UndoAction,
};

/// Interim adapter (task #54 Phase 0): a core [`DetailRow`] to the HUD table row it
/// projects onto. Retires with the HUD's Details tab.
fn hud_row(r: DetailRow) -> Row {
    match r {
        DetailRow::Span { text, bold } => Row::Span { text, bold },
        DetailRow::Pair { label, value } => Row::Pair { label, value },
    }
}

/// `PB_TRACE=1` → present/draw diagnostics to stderr (dev-only; zero cost when off
/// after the first check). Pairs with the Swift host's `pbTrace` size reports.
fn pb_trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PB_TRACE").is_some())
}

impl AppCore {
    /// A **headless** `AppCore`: an empty photo source, a 1-worker no-op decode pool, no
    /// renderer / HUD / window, default keymap + settings. This is the construction seam the
    /// macOS FFI bridge (NS1) builds on — enough to drive the pure input / menu / effect path
    /// through [`handle`](Self::handle) + the effect drain without a window, GPU, or photos, so
    /// the KeyDown→effect round-trip can be proven before a live surface + real source are
    /// layered on. The `handle` unit tests reuse it (via `test_core`), so there's one
    /// construction literal, not two. `now` is a starting stamp; every shell/host overwrites it
    /// per event (the core never reads the wall clock itself — NS0 0.3).
    pub fn headless(viewport: crate::Viewport) -> AppCore {
        // A 1-worker pool whose decode always errors: a headless core has no photos, so it's
        // never invoked. A real host installs a decode closure over its `PhotoSource`.
        let decode: Arc<crate::decode_pool::DecodeFn> = Arc::new(|_src, _item, _fit, _prev| {
            Err(pb_decode::DecodeError::Corrupt("headless".into()))
        });
        let (pool, results) = crate::decode_pool::DecodePool::new(1, 1 << 20, decode);
        let source: Arc<dyn PhotoSource> = Arc::new(FsSource::new(Vec::new()));
        let settings = settings::Settings::default();
        AppCore {
            now: Instant::now(),
            viewport,
            held: std::collections::HashMap::new(),
            pointer_nav: None,
            last_present: None,
            frame_interval: Duration::from_micros(8_333),
            hold_start: None,
            // Derived from the settings model like the real shell (main.rs), not a
            // separate literal that can drift from `Settings::default()`.
            initial_delay: Duration::from_millis(settings.hold_delay_ms as u64),
            slideshow: slideshow::Slideshow::default(),
            mods: contract::Modifiers::NONE,
            esc_guard_until: None,
            persist_prefs: false, // headless/tests: never write the real settings.toml
            os_dark: true,        // dark until the shell reports the OS theme (#46)
            hud_dark: true,       // matches the Hud's default Theme::DARK
            fit: None,
            view: ViewTransform::default(),
            last_cursor: None,
            dragging: false,
            rotations: std::collections::HashMap::new(),
            zoom_started: None,
            zoom_last: None,
            pan_started: None,
            pan_last: None,
            resize_settle_at: None,
            geometry_save_at: None,
            windowed: true,
            meta_cache: std::collections::HashMap::new(),
            current: None,
            exif_cache: std::collections::HashMap::new(),
            recognized_text: std::collections::HashMap::new(),
            text_scan: None,
            text_gen: 0,
            descriptions: std::collections::HashMap::new(),
            describe_scan: None,
            describe_gen: 0,
            pool,
            results,
            ring: ResidentRing::new(0),
            ahead: 8,
            behind: 2,
            failed: HashSet::new(),
            deleted: HashSet::new(),
            preview_resident: HashSet::new(),
            pending_uploads: Vec::new(),
            upgrade_done: HashSet::new(),
            last_upgrade_set: Vec::new(),
            full_requested_at: std::collections::HashMap::new(),
            live_motion_cache: std::collections::HashMap::new(),
            metrics: crate::metrics::StageTimes::default(),
            source,
            archive_scope: None,
            playlist: Playlist::new(0, 0),
            targets: Vec::new(),
            last_nav: Nav::Forward,
            displayed_item: None,
            target_item: None,
            compare_pin: None,
            compare_return: None,
            compare_pin_id: None,
            compare_carry: None,
            epoch: 1,
            root: PathBuf::new(),
            scan_root: None,
            recursive: false,
            scanning: false,
            launching: false,
            dialog_open: false,
            archive_loading: false,
            redraw_pending: false,
            scan_bootstrapped: false,
            password_archive: None,
            pending_delete: None,
            pending_confirm_delete: None,
            info_line: false,
            info_line_shown: false,
            info_line_item: None,
            info_line_w: 0,
            info_line_h: 0,
            panels: Panels::default(),
            native_help: false,
            last_help_visible: false,
            native_open: false,
            last_open_visible: false,
            native_inspector: false,
            last_inspector_snap: None,
            native_tree: false,
            last_tree_visible: false,
            overlay_shown: false,
            overlay_item: None,
            toast: None,
            native_toast: false,
            native_info: false,
            last_info_snap: None,
            native_play: false,
            play_hint_seq: 0,
            toast_native: None,
            toast_seq: 0,
            wait_started: None,
            pie_finish: None,
            pie_glow_started: None,
            decode_ewma: 0.25,
            pie_drawn: false,
            pie_pushed: None,
            chip_sig: None,
            chip_built: Instant::now(),
            folder_tree_open: false,
            folder_tree_sig: None,
            folder_tree_panel: None,
            folder_tree_counts: None,
            fs_tree: None,
            fs_tree_io: None,
            tree_io: None,
            climb_anchor: None,
            hud: None,
            renderer: None,
            undo_stack: Vec::new(),
            playback: None,
            anim_frame_shown_at: None,
            anim_decode: None,
            prepared: None,
            anim_gen: 0,
            anim_hint_shown_for: None,
            framestep_started: None,
            framestep_last: None,
            live_revert_at: None,
            keymap: Keymap::defaults(),
            settings,
            effects: Vec::new(),
        }
    }

    /// A **real, host-ready** `AppCore` for a native shell (the macOS SwiftUI host, NS1):
    /// [`headless`](Self::headless) plus the live engine — the real priority decode pool over
    /// [`decode_item`], the user's loaded [`Settings`](settings::Settings) + [`Keymap`] (and
    /// the settings-derived nav feel: hold delay, slideshow dwell, default scale mode), and
    /// the CPU HUD compositor. The deck starts empty; the host routes opens through
    /// [`open_plan`](Self::open_plan) (→ the `Begin*` effects) exactly like the winit shell.
    /// The winit `App::new` builds the same engine inline (its construction predates this).
    pub fn new_host(viewport: crate::Viewport) -> AppCore {
        let mut core = AppCore::headless(viewport);
        let decode: Arc<crate::decode_pool::DecodeFn> =
            Arc::new(|src, item, fit, allow_preview| decode_item(src, item, fit, allow_preview));
        let (pool, results) = crate::decode_pool::DecodePool::new(
            crate::decode_pool::recommended_workers(),
            POOL_BUDGET_BYTES,
            decode,
        );
        core.pool = pool;
        core.results = results;
        let settings = settings::Settings::load();
        core.initial_delay = Duration::from_millis(settings.hold_delay_ms as u64);
        core.slideshow.interval = Duration::from_secs_f64(settings.slideshow_interval_secs);
        core.view.mode = scale_mode_of(settings.scale_mode);
        core.info_line = settings.show_image_info; // the info readout's launch default (task #54)
        core.keymap = Keymap::load();
        core.settings = settings;
        core.hud = hud::Hud::load();
        core.persist_prefs = true; // a live host persists the remembered last_folder
        core
    }

    /// Whether prefetch/upload work is still outstanding (keep polling if so).
    pub fn work_pending(&self) -> bool {
        // A dropped frame (surface Lost/Outdated/Timeout) keeps the pump awake so the
        // retry in `tick` actually runs — without this, an idle host (empty screen,
        // paused pump) composites the stale frame forever.
        self.redraw_pending
            || self.archive_loading
            // A streaming dir scan keeps the loop polling too, so `poll_dir_scan` picks up
            // batches (and the delayed Scanning-dialog reveal) even when the event queue is
            // quiet — without this, a slow walk on an idle app waits for the next OS event.
            || self.scanning
            // An off-thread animation decode in flight keeps the loop polling so
            // `poll_anim_decode` picks it up promptly (active playback drives its own
            // precise next-frame wake via `tick_playback`, not this frame poll).
            || self.anim_decode.is_some()
            // A tree-io job (the folder tree's read_dir derivation / a Go sibling
            // lookup) keeps the loop polling so `tick` installs it when it lands.
            || self.tree_io.is_some()
            // An off-thread Finder-tree read_dir (expand / reveal) keeps the loop
            // polling so `drive_fs_tree` installs its children promptly.
            || self
                .fs_tree_io
                .as_ref()
                .is_some_and(|io| !io.pending.is_empty())
            // An off-thread text scan (OCR + QR, task #45) keeps polling so
            // `poll_text_scan` picks up the result promptly.
            || self.text_scan.is_some()
            // An off-thread describe (task #44) keeps polling so `poll_describe_scan`
            // installs the result promptly.
            || self.describe_scan.is_some()
            || self.displayed_item != self.target_item
            || self
                .targets
                .iter()
                .any(|&t| self.ring.slot_for(t).is_none() && !self.failed.contains(&t))
    }

    /// Dispatch a one-shot [`Action`] — the single entry point shared by the keyboard
    /// (one-shot keys, via the keymap) and the menu (`MenuAction::to_action`). The pure
    /// view/nav/HUD/animation arms run here in the core; the **flow** arms (dialogs, window
    /// mode, scan, file edits, quit) are routed to the shell/host via
    /// [`CoreEffect::ShellFlowAction`] until 5.6 inverts them into specific effects/events.
    /// Navigation here is a single step (what the menu wants); the keyboard's held-to-fly nav
    /// and continuous pan/zoom are driven by the hold loop, not this path.
    pub fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::Next => self.advance(Nav::Forward),
            Action::Prev => self.advance(Nav::Backward),
            Action::Random => self.advance(Nav::Random),
            Action::RandomPrev => self.advance(Nav::RandomPrev),
            // Pan is continuous-while-held only (the hold loop); never single-dispatched.
            Action::PanLeft | Action::PanRight | Action::PanUp | Action::PanDown => {}
            Action::ZoomIn => self.zoom_step(1.25),
            Action::ZoomOut => self.zoom_step(0.8),
            Action::ScaleFit => self.set_scale_mode(ScaleMode::Fit),
            Action::ScaleFill => self.set_scale_mode(ScaleMode::Fill),
            Action::ScaleOriginal => self.set_scale_mode(ScaleMode::Original),
            Action::ToggleOriginal => {
                let next = if self.view.mode == ScaleMode::Original {
                    ScaleMode::Fit
                } else {
                    ScaleMode::Original
                };
                self.set_scale_mode(next);
            }
            Action::RotateCw => self.rotate(false),
            Action::RotateCcw => self.rotate(true),
            Action::ComparePin => self.compare_pin_cmd(),
            Action::CompareToggle => self.compare_toggle_cmd(),
            Action::Copy => self.copy_image(),
            Action::CopyPath => self.copy_path(),
            Action::CopyImageDetails => self.copy_image_details(),
            Action::CopyImageText => self.copy_image_text(),
            Action::ShowImageText => self.toggle_image_text(),
            Action::DescribeImage => self.describe_image(),
            Action::AskImage => self.ask_image(),
            Action::CopyDescription => self.copy_description(),
            Action::RevealInFileManager => self.reveal_in_file_manager(),
            Action::OpenFile => self.open_picker(false),
            Action::OpenFolder => self.open_picker(true),
            Action::Info => self.toggle_info(false),
            Action::FullExif => self.toggle_info(true),
            Action::Help => self.toggle_help(),
            Action::FolderTree => self.toggle_folder_tree(),
            Action::TogglePanels => self.toggle_panels(),
            Action::OpenParent => self.open_parent_cmd(),
            Action::PrevFolder => self.open_sibling_cmd(-1),
            Action::NextFolder => self.open_sibling_cmd(1),
            Action::SlideshowToggle => self.toggle_slideshow(),
            Action::SlideshowFaster => self.adjust_slideshow(-1),
            Action::SlideshowSlower => self.adjust_slideshow(1),
            Action::PlayPause => self.toggle_play_pause(),
            // A menu click is a single step; the keyboard's hold-to-scrub goes through
            // `frame_step_press` (the FrameStep press arm) instead.
            Action::FrameNext => self.frame_step(1),
            Action::FramePrev => self.frame_step(-1),
            // Mute / unmute the Live Photo audio (NS0 5.6, inverted): the mute *state* + its
            // toast are core; only the ObjC audio player is the shell's (Stop/StartLiveAudio
            // effects). The native menu's mute check re-asserts on the next per-tick
            // `apply_menu_state` diff, so no explicit refresh is needed here.
            Action::MuteLiveAudio => {
                let muted = !self.settings.mute_live_audio;
                self.settings.mute_live_audio = muted;
                self.settings.save();
                if muted {
                    // Silence any playing clip now; a slashed-speaker icon pill = muted.
                    self.effects.push(contract::CoreEffect::StopLiveAudio);
                    self.show_toast_icon("", ToastIcon::Mute);
                } else {
                    // Unmuting mid-playback: resume audio at the motion's current position.
                    if let (Some(pb), Some(item)) = (self.playback.as_ref(), self.displayed_item) {
                        if pb.is_playing() {
                            let at_secs = pb.index() as f64 * pb.total_duration().as_secs_f64()
                                / pb.frame_count().max(1) as f64;
                            if let Some(path) = self.live_motion_path(item) {
                                self.effects
                                    .push(contract::CoreEffect::StartLiveAudio { path, at_secs });
                            }
                        }
                    }
                    // A speaker-with-waves icon pill = now audible.
                    self.show_toast_icon("", ToastIcon::Unmute);
                }
            }
            // Open the About / Settings chrome dialog (NS0 5.6, inverted): a payload-free
            // `ShowDialog` effect the host services natively (a winit egui window now, a native
            // panel on macOS later). The other dialog kinds (Confirm/Message/Password/Loading/
            // Scanning) carry payloads still owned by the shell → they open via 5.6's flow paths.
            Action::About => self.effects.push(contract::CoreEffect::ShowDialog(
                contract::DialogKind::About,
            )),
            Action::Settings => self.effects.push(contract::CoreEffect::ShowDialog(
                contract::DialogKind::Settings,
            )),
            // Save the pending rotation to the file's EXIF / undo the last edit (NS0 5.6,
            // inverted): the EXIF-IO module moved into `pb-app-core`, so these are now pure core
            // arms (all the cache invalidation + undo-stack state is core; the disk write is
            // platform-neutral `std::fs`). An explicit user command — never the view path.
            Action::SaveRotation => self.save_rotation(),
            Action::Undo => self.undo(),
            // Delete-to-trash (`Del`) is a pure core arm now (recoverable, no prompt; the
            // cross-platform trash I/O moved into pb-app-core). `DeletePermanent` (`Shift+Del`)
            // stays a flow action — it opens the shell confirm dialog first, then the shell's
            // dialog-outcome handler calls the core `do_delete(.., true)`.
            Action::Delete => self.delete_to_trash(),
            // Toggle borderless fullscreen ⇄ windowed (NS0 5.6): flip the live mode + the
            // persistent preference; the shell applies the window ops (and snapshots/persists the
            // windowed geometry) when it drains the `SetWindowMode` effect (`apply_window_mode`).
            Action::Fullscreen => {
                self.windowed = !self.windowed;
                // Record the new mode as the remembered last state (settings.fullscreen is the
                // inverse of `windowed`), so `StartupMode::Remember` restores it + Settings stays
                // in sync. Persisted by the shell's `apply_window_mode` (an explicit user action).
                self.settings.fullscreen = !self.windowed;
                self.effects
                    .push(contract::CoreEffect::SetWindowMode(if self.windowed {
                        contract::WindowMode::Windowed
                    } else {
                        contract::WindowMode::Fullscreen
                    }));
            }
            // Host-side commands — the residue whose execution *is* a platform operation:
            // the permanent-delete confirm dialog, the off-thread directory-scan spawn / cancel,
            // and Quit's window teardown. Routed through the one `ShellFlowAction` seam so the
            // whole action vocabulary still dispatches here; the host runs the native op (see the
            // effect's doc). The core-owned commands were lifted out into their own arms above.
            Action::DeletePermanent | Action::Recursive | Action::CancelScan | Action::Quit => self
                .effects
                .push(contract::CoreEffect::ShellFlowAction(action)),
        }
    }

    /// Whether **Save Rotation** is available for the displayed photo: it has a pending in-RAM
    /// rotation *and* its source is a writable-orientation file (JPEG on disk, not an archive
    /// entry). Drives the Edit-menu item's enabled state (`apply_menu_state`).
    pub fn can_save_rotation(&self) -> bool {
        let Some(item) = self.displayed_item else {
            return false;
        };
        let rotated = self
            .rotations
            .get(&item)
            .is_some_and(|r| *r != Rotation::default());
        rotated
            && self
                .source
                .path(item)
                .is_some_and(crate::save_rotation::is_orientation_writable)
    }

    /// **Save Rotation** (`Ctrl+S` / Edit menu): bake the displayed photo's pending in-RAM
    /// rotation into its file's EXIF Orientation, then drop the RAM override + caches and re-read
    /// from disk so the pixels are re-oriented from the file (else a later re-decode would
    /// double-rotate). Records an undo entry. A deliberate, user-initiated write to the user's own
    /// file — never the passive view path (privacy #2). The EXIF write is platform-neutral
    /// `std::fs` (`crate::save_rotation`), so this is a pure core arm.
    pub fn save_rotation(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        let rot = self.rotations.get(&item).copied().unwrap_or_default();
        if rot == Rotation::default() {
            self.show_toast("No rotation to save");
            return;
        }
        let Some(path) = self.source.path(item).map(Path::to_path_buf) else {
            // Archive entry — no file on disk to write back to.
            self.show_toast("Can't save rotation here");
            return;
        };
        if !crate::save_rotation::is_orientation_writable(&path) {
            self.show_toast("Save rotation: JPEG only");
            return;
        }
        // Capture the file's orientation *before* the write so the save can be reversed
        // (Edit ▸ Undo) by restoring this exact value.
        let prev = crate::save_rotation::read_orientation(&path);
        match crate::save_rotation::write_orientation(&path, rot) {
            Ok(_) => {
                // The rotation is now baked into the file's EXIF: drop the RAM override and
                // re-read from disk so the pixels are re-oriented from the file.
                self.rotations.remove(&item);
                self.meta_cache.remove(&item);
                self.exif_cache.remove(&item); // the file's EXIF (Orientation) just changed
                self.failed.remove(&item);
                self.preview_resident.remove(&item);
                self.upgrade_done.remove(&item);
                self.invalidate_geometry();
                self.load_current_sync();
                self.target_item = self.playlist.current();
                self.request_prefetch();
                self.undo_stack.push(UndoAction::SaveRotation {
                    item,
                    path: path.clone(),
                    prev,
                });
                self.show_toast_icon("Saved rotation", ToastIcon::Save);
            }
            Err(e) => {
                eprintln!("save rotation failed: {}: {e}", path.display());
                self.show_toast("Save failed");
            }
        }
    }

    /// **Undo** (`Ctrl+Z` / Edit menu) the last reversible edit. Today that's a saved rotation:
    /// restore the file's previous EXIF Orientation and refresh the caches like the save did. On
    /// an I/O failure the file is untouched, so the entry is pushed back for a retry.
    pub fn undo(&mut self) {
        let Some(action) = self.undo_stack.pop() else {
            self.show_toast("Nothing to undo");
            return;
        };
        match action {
            UndoAction::SaveRotation { item, path, prev } => {
                match crate::save_rotation::set_orientation(&path, prev) {
                    Ok(()) => {
                        self.rotations.remove(&item);
                        self.meta_cache.remove(&item);
                        self.exif_cache.remove(&item); // EXIF Orientation reverted on disk
                        self.failed.remove(&item);
                        self.preview_resident.remove(&item);
                        self.upgrade_done.remove(&item);
                        self.invalidate_geometry();
                        self.load_current_sync();
                        self.target_item = self.playlist.current();
                        self.request_prefetch();
                        self.show_toast_icon("Rotation undone", ToastIcon::Undo);
                    }
                    Err(e) => {
                        eprintln!("undo rotation failed: {}: {e}", path.display());
                        self.show_toast("Undo failed");
                        // The file wasn't changed, so the edit is still reversible — keep it on
                        // the stack for a retry.
                        self.undo_stack
                            .push(UndoAction::SaveRotation { item, path, prev });
                    }
                }
            }
        }
    }

    /// **Delete to Trash** (`Del`): send the displayed photo to the OS Recycle Bin / Trash
    /// (recoverable, no prompt). Archive entries have no file on disk → a toast, no-op. The
    /// playlist advance is deferred a beat by [`do_delete`](Self::do_delete).
    pub fn delete_to_trash(&mut self) {
        // Settle any still-pending delete-advance first (e.g. a rapid second Del).
        self.flush_pending_delete();
        let Some(item) = self.displayed_item else {
            return;
        };
        let Some(path) = self.source.path(item).map(Path::to_path_buf) else {
            self.show_toast("Can't delete this"); // archive entry — no file
            return;
        };
        self.do_delete(item, &path, false);
    }

    /// Perform the actual deletion of `item` (`path`) — recoverable (Recycle Bin) or permanent —
    /// then flash an icon-only pill on the still-shown photo and defer the playlist advance a beat
    /// (`DELETE_ADVANCE_DELAY`) so the feedback registers first. The permanent path reaches here
    /// only after the shell's confirm dialog answers Yes (`do_delete(.., true)`). An explicit,
    /// user-initiated file removal — never the passive view path (privacy #2). The trash / remove
    /// I/O is cross-platform (`crate::delete`), so this is a pure core method.
    pub fn do_delete(&mut self, item: usize, path: &Path, permanent: bool) {
        let res = if permanent {
            crate::delete::delete_permanently(path)
        } else {
            crate::delete::send_to_trash(path)
        };
        if let Err(e) = res {
            eprintln!("delete failed: {}: {e}", path.display());
            self.show_toast("Delete failed");
            return;
        }
        // Deleting a playing animation stops playback so the doomed photo freezes on its current
        // frame under the trash icon (rather than animating until removal).
        self.stop_playback();
        // Recycle-bin icon for the recoverable delete, trash for a permanent one.
        let icon = if permanent {
            ToastIcon::Delete
        } else {
            ToastIcon::Recycle
        };
        self.show_toast_icon("", icon);
        self.pending_delete = Some((self.now + DELETE_ADVANCE_DELAY, item));
    }

    /// Drive the core from a single shell-neutral [`CoreEvent`] — the entry point a non-winit
    /// host (the macOS SwiftUI/AppKit bridge, NS1) uses to run the viewer *without* the winit
    /// shell. It mutates core state + accumulates [`CoreEffect`]s the host then drains. The host
    /// stamps `self.now` at each event before calling (and [`CoreEvent::Tick`] carries it), so
    /// the core never reads a wall clock — `handle` is deterministically unit-testable.
    ///
    /// **Coverage (NS0 5.5 Phase C1):** the **input + menu** events — KeyDown/KeyUp/FocusLost,
    /// MenuAction, KeymapSubmitted — the primary way a host drives the viewer. Escape is *not*
    /// special-cased here (it resolves through the keymap to `Quit` → `ShellFlowAction`); the
    /// host owns the dialog-dismiss-vs-quit decision. Tick/Redraw/Resized/pointer/scroll/pinch +
    /// the flow events (DroppedPaths/CancelDialog) are wired in **C2**, when the winit
    /// `window_event`/`about_to_wait` are rewritten as translators (they touch the still-shell
    /// tick / scan / pointer / flow paths); until then the shell keeps calling those directly.
    pub fn handle(&mut self, ev: contract::CoreEvent) {
        use contract::{CoreEvent, KeyResolution};
        match ev {
            CoreEvent::KeyDown { key, mods, repeat } => {
                self.mods = mods;
                match contract::resolve_key_down(&self.keymap, key, mods, repeat) {
                    KeyResolution::Ignore => {}
                    KeyResolution::OneShot(act) => self.dispatch_action(act),
                    KeyResolution::NavStart(act) => self.nav_press(key, act),
                    KeyResolution::HeldStart(act) => {
                        self.held.insert(key, act);
                    }
                    KeyResolution::FrameStepStart(act) => self.frame_step_press(key, act),
                }
            }
            CoreEvent::KeyUp { key } => {
                self.held.remove(&key);
            }
            // Focus loss can swallow the key-up (a known winit hazard) — clear the held set +
            // gesture accumulators so nav never sticks auto-advancing, and drop any stuck drag.
            CoreEvent::OsThemeChanged { dark } => {
                self.os_dark = dark;
                self.refresh_theme();
            }
            CoreEvent::FocusLost => {
                self.held.clear();
                self.pointer_nav = None; // mouse-up normally ends it; this is the safety net
                self.hold_start = None;
                self.mods = contract::Modifiers::NONE;
                self.zoom_started = None;
                self.zoom_last = None;
                self.pan_started = None;
                self.pan_last = None;
                self.pie_glow_started = None;
                self.dragging = false;
            }
            CoreEvent::MenuAction(action) => self.dispatch_action(action),
            CoreEvent::KeymapSubmitted(keymap) => self.apply_keymap(keymap),
            // Pointer moved: drag-to-pan (while the button is held) + refresh the folder-tree
            // hover state + the cursor shape.
            CoreEvent::PointerMoved { x, y } => {
                let p = [x, y];
                if self.dragging {
                    if let Some(prev) = self.last_cursor {
                        self.pan_by_pixels(p[0] - prev[0], p[1] - prev[1]);
                    }
                }
                self.last_cursor = Some(p);
                self.update_tree_hover();
                self.refresh_cursor();
            }
            // Trackpad pinch (macOS): magnify about the cursor (+ spread in / − pinch out).
            CoreEvent::Pinch { delta } => {
                self.zoom_about_cursor(1.0 + delta * PINCH_GAIN);
            }
            // Trackpad double-tap ("smart magnify"): toggle 100%, sharing the `0` / menu path.
            CoreEvent::DoubleTap => self.dispatch_action(Action::ToggleOriginal),
            // The per-tick core loop (hold-to-fly / slideshow / prefetch / animation), stamping
            // `now` from the event. Emits `SetWake` with the core's next deadline.
            CoreEvent::Tick(now) => {
                self.now = now;
                self.tick();
            }
            // A chrome dialog resolved — run the reaction + emit the close/cancel effects.
            CoreEvent::DialogResolved(result) => self.handle_dialog_resolved(result),
            // A streaming-scan snapshot / the scan's completion (the host owns the worker thread +
            // generation check; the core applies the result to the playlist).
            CoreEvent::ScanBatch(resolved) => self.apply_scan_batch(resolved),
            CoreEvent::ScanDone => self.finish_scan(),
            // A background archive open resolved to a non-empty playlist — install it.
            CoreEvent::ArchiveResolved(resolved) => self.apply_archive(resolved),
            // The surface resized / its scale changed — update viewport + fit, reconfigure the
            // swapchain, rescale overlays on a DPI change, and debounce the crisp re-decode.
            CoreEvent::Resized {
                width,
                height,
                scale,
            } => self.resize(width, height, scale),
            // Scroll wheel / trackpad two-finger swipe → zoom or pan (per the setting; Ctrl flips).
            CoreEvent::Scroll(delta) => self.scroll(delta),
            // Still shell-side (no clean core seam yet): the shell's own redraw scheduling, and
            // dropped paths (a flow the shell classifies). Ignored so a host sending them early is
            // a no-op, not a panic.
            CoreEvent::Redraw | CoreEvent::DroppedPaths(_) | CoreEvent::CancelDialog => {}
        }
    }

    /// React to a resolved chrome dialog (NS0 5.6): run the small core reaction (apply settings /
    /// keymap, confirm a delete, arm the Esc-guard, forget a pending confirm/password) and emit the
    /// uniform housekeeping effects — `CloseDialog`, and `CancelScan` / `CancelArchiveLoad` when a
    /// dismiss/cancel abandons an in-flight worker. The host drove the dialog UI + extracted the
    /// payloads; this owns the *reaction*. (The password-submit path spawns the archive worker, so
    /// it's still handled shell-side and never reaches here.)
    pub fn handle_dialog_resolved(&mut self, result: contract::DialogResult) {
        use contract::{CoreEffect as E, DialogKind, DialogResult as R};
        match result {
            R::Dismissed(kind) => {
                // The Esc that dismisses a focused dialog also leaks to the main window as a
                // trailing/synthetic press once focus snaps back — briefly guard quit-on-Esc so
                // closing a dialog never also exits the app.
                self.esc_guard_until = Some(self.now + Duration::from_millis(300));
                // Esc / close on the loading view cancels the in-flight open (harmless otherwise).
                self.effects.push(E::CancelArchiveLoad);
                // Guarded to the Scanning kind so closing a *different* dialog doesn't kill a fast
                // scan still running quietly in the background.
                if kind == Some(DialogKind::Scanning) {
                    self.effects.push(E::CancelScan);
                }
                self.effects.push(E::CloseDialog);
                self.pending_confirm_delete = None; // Esc / close = cancel the confirm
                self.password_archive = None; // Esc / close = abandon the password prompt
            }
            // A password was entered: show "Checking…" + re-open the pending archive with it (a
            // wrong one re-prompts in place, so the dialog isn't closed here — the archive-open
            // result drives that). Nothing pending (shouldn't happen) → just close.
            R::PasswordSubmitted(pw) => match (pw, self.password_archive.clone()) {
                (Some(pw), Some(path)) => {
                    self.effects.push(E::SetDialogChecking);
                    self.effects.push(E::BeginArchiveOpen {
                        path,
                        password: Some(pw),
                    });
                }
                _ => self.effects.push(E::CloseDialog),
            },
            R::PasswordCancelled => {
                self.effects.push(E::CloseDialog);
                self.password_archive = None;
            }
            // Ask about image (task #44): close the dialog, then run the question through the
            // describe backend for the current photo (a blank question is a no-op close).
            R::AskSubmitted(question) => {
                self.effects.push(E::CloseDialog);
                self.ask_describe(question);
            }
            // Settings: Save applies + persists the edited model; Cancel/Esc discard.
            R::SettingsSaved { settings, keymap } => {
                self.effects.push(E::CloseDialog);
                if let Some(new) = settings {
                    self.apply_settings(*new);
                }
                if let Some(km) = keymap {
                    self.apply_keymap(km);
                }
            }
            // A live edit from an auto-saving Settings window: apply + persist like Save,
            // but the window stays open — no CloseDialog.
            R::SettingsEdited { settings, keymap } => {
                if let Some(new) = settings {
                    self.apply_settings(*new);
                }
                if let Some(km) = keymap {
                    self.apply_keymap(km);
                }
            }
            R::SettingsCancelled => self.effects.push(E::CloseDialog),
            // Loading: the only button is Cancel; make sure the open stops, then close.
            R::LoadingCancelled => {
                self.effects.push(E::CancelArchiveLoad);
                self.effects.push(E::CloseDialog);
                self.password_archive = None;
            }
            // Scanning: stop the walk, discard its partial result, and close — a cancelled scan
            // keeps the current view, not a half-walked tree.
            R::ScanningCancelled => {
                self.effects.push(E::CancelScan);
                self.effects.push(E::CloseDialog);
            }
            // Confirm drives a permanent delete on the pending item.
            R::ConfirmAnswered(confirmed) => {
                self.effects.push(E::CloseDialog);
                let item = self.pending_confirm_delete.take();
                if confirmed {
                    if let Some(item) = item {
                        if let Some(path) = self.source.path(item).map(Path::to_path_buf) {
                            self.do_delete(item, &path, true);
                        }
                    }
                }
            }
            // Message / others just close.
            R::Closed => self.effects.push(E::CloseDialog),
        }
    }

    /// Drop any photos the user deleted mid-scan from a streaming snapshot (the worker's cumulative
    /// list still has them). A no-op — returns the snapshot untouched — when nothing was deleted,
    /// which is the common case. NS0 5.6 Step 3 (was the shell's `App::filter_deleted`).
    fn filter_deleted(&self, r: crate::scan::Resolved) -> crate::scan::Resolved {
        if self.deleted.is_empty() {
            return r;
        }
        let paths: Vec<PathBuf> = (0..r.source.len())
            .filter_map(|i| r.source.path(i).map(Path::to_path_buf))
            .filter(|p| !self.deleted.contains(p))
            .collect();
        let start = r.start.min(paths.len().saturating_sub(1));
        crate::scan::Resolved {
            source: Arc::new(FsSource::new(paths)),
            root: r.root,
            scan_root: r.scan_root,
            recursive: r.recursive,
            start,
        }
    }

    /// Apply a streaming directory-scan snapshot ([`CoreEvent::ScanBatch`], NS0 5.6 Step 3): filter
    /// out mid-scan deletes, then — for the first non-empty batch — **bootstrap** the playlist
    /// (display + decode a photo now, well before the whole tree is walked) and mark
    /// `scan_bootstrapped`; later batches **extend** it in place, keeping the displayed photo and
    /// every per-image cache (indices are append-only). An empty snapshot is skipped. The host owns
    /// the worker thread + generation check, so a superseded batch never reaches here.
    fn apply_scan_batch(&mut self, resolved: crate::scan::Resolved) {
        let resolved = self.filter_deleted(resolved);
        if resolved.source.is_empty() {
            return; // nothing to show yet (shouldn't happen — worker skips empties)
        }
        if !self.scan_bootstrapped {
            self.scan_bootstrapped = true;
            self.rebuild_playlist(
                resolved.source,
                resolved.root,
                resolved.scan_root,
                resolved.recursive,
                resolved.start,
            );
        } else {
            self.extend_playlist(resolved.source);
        }
    }

    /// The streaming directory scan finished ([`CoreEvent::ScanDone`], NS0 5.6 Step 3): the deck is
    /// final, so resume normal (random-ahead) prefetch. If nothing was ever shown and the deck is
    /// empty (a bare-folder launch onto an empty folder), restore the "Press O to open" hint the
    /// scan had suppressed — but never blank an existing photo. The host drops the worker handle +
    /// closes the progress dialog.
    fn finish_scan(&mut self) {
        self.scanning = false;
        if !self.scan_bootstrapped && self.source.is_empty() {
            self.show_open_hint();
        }
        self.request_prefetch();
    }

    /// The just-finished folder scan found **no** supported images. If a deck is already on
    /// screen, keep it untouched and surface a non-modal **toast** — never a blocking alert
    /// (③ keep-deck-until-photos, owner 2026-07-05): a mis-click into an empty or deep folder
    /// shouldn't interrupt browsing. With nothing loaded (a bare launch onto an empty folder),
    /// [`finish_scan`](Self::finish_scan) restores the "Press O to open" hint, so stay quiet
    /// here. Called by each shell's scan-`Done` arm in place of the old stderr log / NSAlert.
    pub fn scan_found_no_photos(&mut self, folder_name: &str) {
        if self.source.is_empty() {
            return; // the open hint (via finish_scan) covers the nothing-loaded case
        }
        let name = if folder_name.is_empty() {
            "that folder"
        } else {
            folder_name
        };
        self.show_toast(&format!("No images in \u{201c}{name}\u{201d}"));
    }

    /// Install a resolved archive playlist ([`CoreEvent::ArchiveResolved`], NS0 5.6 Step 3): the
    /// open succeeded with a non-empty deck, so rebuild the playlist onto it and forget any pending
    /// password. The host closes the loading/password dialog (like the scan's Done); the failure
    /// cases (empty / password-required / cancelled / error) stay host-side.
    fn apply_archive(&mut self, resolved: crate::scan::Resolved) {
        self.password_archive = None;
        let full = Arc::clone(&resolved.source);
        self.rebuild_playlist(
            resolved.source,
            resolved.root,
            resolved.scan_root,
            resolved.recursive,
            resolved.start,
        );
        // A fresh archive deck starts unscoped; the ⇧F tree / Go commands
        // re-scope by filtering this full source (never re-opening the file).
        // Stamp only if the rebuild actually installed (it refuses an empty deck).
        if Arc::ptr_eq(&self.source, &full) {
            self.archive_scope = Some(crate::ArchiveScope {
                full,
                prefix: String::new(),
            });
        }
    }

    /// Route an opened source (NS0 5.6 Step 3c) — the entry point the picker / drag-drop / a
    /// deferred launch funnel through. An **archive** or a **folder scan** starts its off-thread
    /// worker via an effect (`BeginArchiveOpen` / `BeginDirScan`; the host owns the thread +
    /// progress dialog + generation, and feeds results back as `ArchiveResolved` / `ScanBatch`).
    /// A finite **explicit list** has no directory walk, so it resolves inline and installs now.
    pub fn open_plan(&mut self, source: pb_core::open::Source, cursor: pb_core::open::Cursor) {
        use pb_core::open::Source;
        // Any explicit open breaks an Open-Parent climb — the next ⌘↑ restarts from the
        // current folder. `open_parent_cmd` re-sets the anchor *after* it calls through here.
        self.climb_anchor = None;
        match source {
            Source::Archive(path) => {
                self.effects.push(contract::CoreEffect::BeginArchiveOpen {
                    path,
                    password: None,
                });
            }
            src @ Source::Scan { .. } => {
                self.effects.push(contract::CoreEffect::BeginDirScan {
                    source: src,
                    cursor,
                });
            }
            src @ Source::Explicit(_) => {
                let r = crate::scan::resolve_playlist(&src, &cursor);
                if r.source.is_empty() {
                    eprintln!("PhotoBlaze: no supported images in that selection");
                    return;
                }
                self.rebuild_playlist(r.source, r.root, r.scan_root, r.recursive, r.start);
            }
        }
    }

    /// The surface resized (or its backing scale changed) — the core-owned part of the host's
    /// resize handling ([`CoreEvent::Resized`], NS0 loose-end). Update the viewport + (on a DPI
    /// change) rescale the CPU overlays, and — only when the *fit box* actually changes — swap it,
    /// reconfigure the swapchain (`renderer.resize`; the resident texture GPU-scales to the new
    /// size), and debounce the crisp decode-to-fit (a drag fires this many times a second, so the
    /// per-image CPU re-decode waits for the size to settle). The host does its platform surface
    /// bits *around* this — the macOS EDR re-assert (after `resize`, before draw) + the redraw,
    /// gated on the same fit-change the host computes from [`fit`](Self::fit) before calling here.
    pub fn resize(&mut self, width: u32, height: u32, scale: f32) {
        if (scale - self.viewport.scale_factor).abs() > f32::EPSILON {
            self.viewport.scale_factor = scale;
            self.rescale_overlays();
        }
        self.viewport.width = width.max(1);
        self.viewport.height = height.max(1);
        let new_fit = FitBox {
            max_width: width.max(1),
            max_height: height.max(1),
        };
        if Some(new_fit) == self.fit {
            return;
        }
        self.fit = Some(new_fit);
        if let Some(r) = self.renderer.as_mut() {
            r.resize(width, height);
        }
        // Deferred crisp decode-to-fit + ring refill once the size settles (`self.now` is stamped
        // by the host at the start of the event).
        self.resize_settle_at = Some(self.now + Duration::from_millis(180));
    }

    /// A scroll wheel / trackpad two-finger swipe ([`CoreEvent::Scroll`], NS0 loose-end): pan (the
    /// default) or zoom-about-the-cursor, per the `Scroll wheel` setting with **Ctrl** always
    /// flipping to the other action. A line-precise wheel and a pixel-precise trackpad swipe use
    /// different step sizes (a trackpad delivers tens of pixels per event), so the [`ScrollDelta`]
    /// carries which it is.
    pub fn scroll(&mut self, delta: contract::ScrollDelta) {
        use contract::ScrollDelta;
        let zooms = self.settings.scroll_action == settings::ScrollAction::Zoom;
        let zoom = zooms != self.mods.ctrl;
        match delta {
            ScrollDelta::Pixels { x, y } => {
                if zoom {
                    self.zoom_about_cursor((1.0 + y * PIXEL_ZOOM_STEP).max(0.05));
                } else {
                    self.pan_by_pixels(x * GESTURE_PAN_DIR, y * GESTURE_PAN_DIR);
                }
            }
            ScrollDelta::Lines { x, y } => {
                if zoom {
                    self.zoom_about_cursor((1.0 + y * WHEEL_ZOOM_STEP).max(0.05));
                } else {
                    self.pan_by_pixels(
                        x * WHEEL_PAN_STEP * GESTURE_PAN_DIR,
                        y * WHEEL_PAN_STEP * GESTURE_PAN_DIR,
                    );
                }
            }
        }
    }

    /// The per-tick core loop (NS0 5.5 Phase C2): absorb finished decodes + uploads, run held
    /// zoom/pan, the gated self-paced nav advance (hold-to-fly) and the slideshow, re-issue the
    /// sharpen/prefetch when parked, update the info panel / toast / pie, run the deferred
    /// resize decode + debounced geometry save, and drive on-demand animation playback + the
    /// Live Photo revert + eager-prep. Ends by emitting [`CoreEffect::SetWake`] with the earliest
    /// deadline the core wants to be ticked at (`None` = idle). `self.now` is stamped by the
    /// caller (`CoreEvent::Tick` carries it). A winit host mins the wake with its own
    /// dialog-repaint clock; the scan-count chip + dialog egui clock stay host-side.
    pub fn tick(&mut self) {
        let now = self.now;
        // 0a. Retry a dropped frame (surface Lost/Outdated/Timeout during resize/
        // fullscreen churn): the previous `draw` reconfigured the surface but nothing
        // reached the screen. One retry per tick — vsync-paced by the host pump, so a
        // persistently failing surface never spins.
        if self.redraw_pending {
            self.draw();
        }
        // 0. Deferred delete-advance: once the trash icon has shown for a beat, drop the
        // item. In the core (not the host) so every host that drives `Tick` gets it —
        // the wake condition below already polls while one is pending.
        if self.pending_delete.is_some_and(|(at, _)| now >= at) {
            self.flush_pending_delete();
        }
        // 1. Absorb finished decodes (uploads; presents the target if it arrived).
        self.drain_results();

        // 1a. Bound the regenerable per-item caches so browsing tens of thousands of photos
        // can't grow them without limit. Cheap when under the high-water mark (length checks).
        self.trim_caches();

        // 1b. Pick up a finished off-thread animation decode (kicked by `P` / frame-step) and
        // install playback — never on the still/keypress hot path (#37).
        self.poll_anim_decode();

        // 1c. Pick up a finished off-thread text scan (OCR + QR, task #45): cache it,
        // refresh the `T` panel's busy state, run a deferred copy.
        self.poll_text_scan();

        // 1d. Pick up a finished off-thread AI describe (task #44): cache it and refresh
        // the description panel's busy state.
        self.poll_describe_scan();

        // 2. Continuous zoom/pan while their keys are held (accelerating ramp).
        let transforming = self.apply_view_holds(now);

        // 3. Gated self-paced advance while a nav key (space/backspace) is held. The initial tap
        // delay gates *repeat*, not draining/presenting, so a first-press miss shows the moment
        // it decodes.
        let nav = self.held_nav();
        let past_delay = timing::elapsed_since(self.hold_start, now, self.initial_delay);
        if let Some(dir) = nav {
            // Advance only when caught up AND the (accelerating) interval elapsed, so every photo
            // is shown and a miss simply holds. The gap ramps slow→fast over `ramp_secs` of held
            // auto-repeat; the ceiling is the max-photos/sec cap (#20) or the refresh rate.
            let caught_up = self.displayed_item == self.target_item;
            let repeat_elapsed = match self.hold_start {
                Some(t) => now.saturating_duration_since(t + self.initial_delay),
                None => Duration::ZERO,
            };
            let interval = timing::advance_interval(
                repeat_elapsed,
                self.settings.start_speed,
                self.settings.ramp_secs,
                self.settings.max_advance_rate as f32,
                self.frame_interval,
            );
            let due = timing::elapsed_since(self.last_present, now, interval);
            if past_delay && caught_up && due {
                self.advance(dir);
            } else if !caught_up {
                self.try_present_target();
            }
        } else {
            self.hold_start = None;
        }

        // 3c. Slideshow auto-advance (task #23): on, not overridden by a held nav key or an open
        // dialog, and readiness-gated like hold-to-fly (a not-ready slide holds, never skips).
        let slideshow_running = self.slideshow.on && self.held_nav().is_none() && !self.dialog_open;
        if slideshow_running {
            let caught_up = self.displayed_item == self.target_item;
            let since_shown = self
                .last_present
                .map(|t| now.saturating_duration_since(t))
                .unwrap_or(Duration::ZERO);
            if caught_up && self.slideshow.is_due(since_shown) {
                self.advance(self.last_nav);
            }
        }

        // 3b. Sharpen / prefetch-ahead. When parked, re-issue the prefetch whenever the
        // wanted-fulls set changes (not every tick → no per-frame churn); keep ticking while any
        // sharpen is outstanding so `drain_results` catches it.
        let mut sharpen_pending = false;
        if self.held_nav().is_none() {
            let upgrade = self.fulls_wanted();
            if upgrade != self.last_upgrade_set {
                self.last_upgrade_set = upgrade.clone();
                self.request_prefetch();
            }
            sharpen_pending = !upgrade.is_empty();
        }

        // 4. Info panel visibility. "Blaze mode" = actually flying (a nav key held past the tap
        // delay): hide the panel so it isn't a strobing distraction. Otherwise keep it shown +
        // tracking the current photo. Left untouched mid zoom/pan.
        let flying = nav.is_some() && past_delay;
        // 4a′. Flash the "Press P to play" hint once on settling on an animated still.
        self.maybe_show_anim_hint(flying);
        // 4a. The basic info line (`i`) — its own ephemeral layer, so it runs before
        // the rich panel (whose bottom lift reads the line's shown state). Same
        // fly-hide + settle-track behavior as the panel, but never needs Help's
        // static exception since the line always describes a photo. Also suppressed
        // while `Tab`-hidden — the eager `refresh_info_line_visibility` applies that
        // the instant `hidden` flips, but this tick keeps it from popping back on its
        // own next-photo/settle logic while still hidden.
        if self.info_line {
            if flying || self.panels.hidden {
                if self.info_line_shown {
                    self.hide_info_line();
                }
            } else if !transforming
                && self.current.is_some()
                && (!self.info_line_shown || self.info_line_item != self.displayed_item)
            {
                self.show_info_line();
            }
        }
        if let Some(slot) = self.slot_content() {
            if flying {
                if self.overlay_shown {
                    self.hide_overlay();
                }
            } else if !transforming
                // Help is static; the info panels need a photo.
                && (slot == SlotContent::Help || self.current.is_some())
                && (!self.overlay_shown || self.overlay_item != self.displayed_item)
            {
                self.show_overlay();
            }
        }
        // 4b. Native-panel visibility markers (task #54, mac-first): fire only on a real
        // show/hide of a natively-presented panel, so the host re-pulls its model and
        // shows/hides its SwiftUI view. Winit (all `native_* == false`) never enters.
        if self.native_help {
            let vis = self.help_panel_visible();
            if vis != self.last_help_visible {
                self.last_help_visible = vis;
                self.emit_panels_changed();
            }
        }
        if self.native_open {
            let vis = self.open_panel_visible();
            if vis != self.last_open_visible {
                self.last_open_visible = vis;
                self.emit_panels_changed();
            }
        }
        if self.native_inspector {
            // Kick the active tab's scan (no-op when cached / already running): the panel
            // tracks the displayed photo, and on this native path `show_overlay`'s HUD
            // branch — which normally kicks these — is suppressed.
            //
            // NOT while flying: OCR and describe are expensive (a per-photo OCR thread, or a
            // describe network round-trip), and at fly speed they'd fire on every photo you
            // pass, starving decode and stuttering the flight. Only kick when settled — the
            // current photo gets scanned the moment you stop (the HUD path gets this for free
            // via its fly-suppressed `show_overlay`; the native path needs the guard explicitly).
            //
            // Also hard-cap concurrency: only auto-kick when no scan is already in flight.
            // Each scan `std::thread::spawn`s an uncancellable job that full-res-decodes +
            // OCRs/describes (holding a whole image), so without this a fast walk — or a slow
            // network volume where each job lives for seconds — piles up resident full-res
            // decodes until OOM. Deferring (vs. superseding) keeps at most one auto job alive;
            // the explicit Copy Text / D commands still supersede for responsiveness.
            if self.inspector_panel_visible() && self.current.is_some() && !flying {
                match self.panels.inspector {
                    // Warm the Details EXIF for the *displayed* photo. Cheap and safe (unlike
                    // the OCR/describe scans below): `ensure_exif_cached` is a synchronous
                    // metadata parse — bytes read + `read_exif_fields`, bytes dropped — no
                    // full-res decode, no thread, idempotent (returns early when cached). The
                    // native path needs this explicitly (the HUD path warmed it in
                    // `show_overlay`, suppressed here); without it the Details tab shows only
                    // the basic rows until a Describe round-trip warms the cache as a side
                    // effect. `!flying` keeps it off the fast-flick hot path.
                    Some(InspectorTab::Details) => {
                        if let Some(item) = self.displayed_item {
                            self.ensure_exif_cached(item);
                        }
                    }
                    Some(InspectorTab::Text) if self.text_scan.is_none() => self.ensure_text_scan(),
                    Some(InspectorTab::Describe)
                        if self.settings.describe_auto && self.describe_scan.is_none() =>
                    {
                        self.ensure_describe_scan(None)
                    }
                    _ => {}
                }
            }
            // Re-signal on any visibility / tab / content change — async OCR and describe
            // results land in the caches by now (the polls ran above), so the snapshot
            // diff catches them without a per-tick marker.
            let snap = self
                .inspector_panel_visible()
                .then(|| self.inspector_snapshot());
            if snap != self.last_inspector_snap {
                self.last_inspector_snap = snap;
                self.emit_panels_changed();
            }
        }
        if self.native_tree {
            if self.tree_panel_visible() {
                if self.tree_is_fs() {
                    // Disk deck → the resident Finder tree drives itself (off-thread reads,
                    // reveal, expansion). Content changes signal via `drive_fs_tree`.
                    self.drive_fs_tree();
                } else if !self.scanning {
                    // A settled archive/empty deck → drop any resident tree so the v1
                    // `folder_tree_panel` path (below) owns the display. During a scan
                    // (`displayed_item` momentarily gone as a new folder opens) we hold the
                    // tree so its expansion survives the open, then `drive_fs_tree` reveals
                    // the newly-landed folder in place.
                    self.fs_tree = None;
                    self.fs_tree_io = None;
                }
            }
            // Visibility (open/close, Tab-hide).
            let vis = self.tree_panel_visible();
            if vis != self.last_tree_visible {
                self.last_tree_visible = vis;
                self.emit_panels_changed();
            }
        }
        if self.native_info {
            // The natively-drawn info readout re-pulls only on a real content change (a photo
            // swap or a field toggle) — tracks during hold-to-fly like the tree, since the
            // readout answers "which photo is this".
            let snap = self.info_line_snapshot();
            if snap != self.last_info_snap {
                self.last_info_snap = snap;
                self.emit_panels_changed();
            }
        }

        // 4a″. Folder tree (⇧F): keep it tracking the displayed photo's folder — the
        // whole point is "you are here", so it tracks **during hold-to-fly too**
        // (owner call, 2026-07-03). The per-tick check is string ops on the
        // signature; a settled rebuild (with its two read_dirs on a disk deck) runs
        // only when the folder actually changes; a mid-flight rebuild uses the
        // no-I/O playlist derivation and is rate-limited so folder-per-frame
        // crossings can't eat the advance budget. An empty deck rebuilds too
        // (`@root`): the tree browses from the root itself, so a photo-less folder
        // is a navigation point rather than a dead end.
        // Install a landed tree-io result first (the off-thread read_dir derivation,
        // or a Go sibling target). A stale full-tree result (the folder moved on, or
        // the tree closed) is dropped — the signature check below re-derives.
        if let Some(io) = self.tree_io.as_ref() {
            match io.rx.try_recv() {
                Ok(crate::folder_tree::TreeIoResult::FullTree { sig, model }) => {
                    self.tree_io = None;
                    if self.panels.tree_visible(self.folder_tree_open)
                        && self.hud.is_some()
                        && self.folder_sig() == sig
                    {
                        self.push_folder_tree(model.rows, model.targets, 0, None);
                        self.folder_tree_sig = Some(sig);
                        self.update_tree_hover();
                    }
                }
                Ok(crate::folder_tree::TreeIoResult::Sibling { from_root, target }) => {
                    self.tree_io = None;
                    // The probe is anchored on the folder the search started from (the
                    // current photo's folder, or the deck root when there's none). If the
                    // user has since navigated to a different folder, the result is stale —
                    // it must not yank navigation somewhere they already left.
                    let anchor = self
                        .current_folder_abs()
                        .unwrap_or_else(|| self.root.clone());
                    if from_root != anchor {
                        // Stale — the folder moved on while the probe ran.
                    } else if let Some(d) = target {
                        self.open_dir(d);
                    } else {
                        // Nothing with photos in that direction (or the search
                        // hit its cap/budget): non-blocking feedback, never the
                        // modal — that stays for explicit opens (#49).
                        self.show_toast("No more folders with images");
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.tree_io = None,
            }
        }
        if self.panels.tree_visible(self.folder_tree_open)
            && self.hud.is_some()
            && !self.tree_is_fs()
        {
            let sig = self.folder_sig();
            let lite_sig = format!("{sig}|lite");
            let stored = self.folder_tree_sig.as_deref();
            if stored != Some(sig.as_str()) && stored != Some(lite_sig.as_str()) {
                let throttled = flying
                    && self
                        .folder_tree_panel
                        .as_ref()
                        .is_some_and(|p| now.duration_since(p.built) < Self::TREE_FLY_REBUILD);
                if !throttled {
                    self.show_folder_tree_mode(flying);
                }
            } else if !flying && stored == Some(lite_sig.as_str()) {
                // Flight settled on a folder last drawn by the lite pass — upgrade
                // to the full read_dir view (it adds photo-less folders), unless
                // that derivation is already in flight on the worker.
                let pending = self
                    .tree_io
                    .as_ref()
                    .is_some_and(|io| io.full_sig.as_deref() == Some(sig.as_str()));
                if !pending {
                    self.show_folder_tree_mode(false);
                }
            }
        }

        // 4b. Transient status toast: hold then fade; clears itself when expired.
        let toast_active = self.tick_toast(now);

        // 4c. The "not-ready" loading pie while the next photo is still decoding.
        let pie_active = self.tick_pie(now);

        // 4d. Once a resize/toggle has settled, run the deferred decode-to-fit: rebuild the ring
        // at the new slot size, re-show the current photo crisp, and refill neighbours.
        let resizing = match self.resize_settle_at {
            Some(at) if now >= at => {
                self.resize_settle_at = None;
                self.invalidate_geometry();
                self.load_current_sync();
                self.target_item = self.playlist.current();
                self.request_prefetch();
                // Re-place a visible panel against the settled surface size (a fullscreen toggle
                // otherwise leaves it jammed in the corner — #3).
                if self.overlay_shown {
                    self.show_overlay();
                }
                // The tree's height budget (max_h) tracks the surface too.
                if self.folder_tree_open {
                    self.folder_tree_sig = None; // rebuild against the settled size next tick
                }
                false
            }
            Some(_) => true, // still settling — keep ticking so it fires
            None => false,
        };

        // 4e. Persist the windowed geometry once the user stops moving/resizing (#1). An explicit
        // user action (positioning the window), never the view path.
        if let Some(at) = self.geometry_save_at {
            if now >= at {
                self.geometry_save_at = None;
                self.settings.save();
            }
        }

        // 4g. On-demand animation (task #37): `tick_playback` advances to the due frame + returns
        // the next frame's deadline; `tick_frame_step` drives the held `,`/`.` scrub.
        let anim_wake = self.tick_playback(now);
        let framestep_active = self.tick_frame_step(now);

        // 4g'. A finished Live Photo reverts to the crisp still once the linger beat elapsed.
        // Drops the finished playback but keeps the decoded motion (`prepared`) so replay is
        // instant. The motion (and its audio) is done → stop the audio (shell owns the player).
        let revert_wake = match self.live_revert_at {
            Some(at) if now >= at => {
                self.live_revert_at = None;
                self.playback = None;
                self.anim_frame_shown_at = None;
                self.effects.push(contract::CoreEffect::StopLiveAudio);
                self.restore_still();
                None
            }
            other => other,
        };

        // 4h. Eagerly prep an animated still for instant playback once the user has rested on it
        // — only when settled (never while flying), so it never competes with the fly hot path.
        let prep_wake = if flying {
            None
        } else {
            self.maybe_prepare_animation(now)
        };

        // 5. The core's next wake. Poll at the frame rate while interacting or work is
        // outstanding; else sleep to the slideshow's next-slide deadline when it's the only
        // thing pending; otherwise go idle (`None`) until a real event.
        let base_wake = if nav.is_some()
            || transforming
            || self.work_pending()
            || toast_active
            || pie_active
            || resizing
            || sharpen_pending
            || framestep_active
            || self.pending_delete.is_some()
            // Keep ticking until the debounced windowed-geometry save fires (#1).
            || self.geometry_save_at.is_some()
        {
            Some(now + self.frame_interval)
        } else if slideshow_running {
            // Sleep until the next slide is due; clamp into the future so a just-passed deadline
            // still schedules a wake (we advance on the following tick).
            Some(
                self.last_present
                    .map(|t| t + self.slideshow.interval)
                    .unwrap_or(now + self.frame_interval)
                    .max(now + Duration::from_millis(1)),
            )
        } else {
            None
        };
        // The earliest of the viewer's poll, the animation's next-frame deadline, the eager-prep
        // dwell, and the Live-Photo-revert deadline; `None` = idle. (The host mins in its own
        // dialog-repaint clock.)
        let wake = [base_wake, anim_wake, prep_wake, revert_wake]
            .into_iter()
            .flatten()
            .min();
        self.effects.push(contract::CoreEffect::SetWake(wake));
    }

    /// Bound the regenerable per-item caches (metadata / EXIF / OCR text / AI descriptions) so
    /// browsing tens of thousands of photos in one session can't grow them without limit. Keeps
    /// the entries **nearest the current photo** — the ones a fly-back or neighbor revisit will
    /// want (and always the current item + the resident window, which is well inside the cap) —
    /// and evicts the farthest. Deliberately does **not** touch `rotations` (unsaved user edits,
    /// not a cache). An evicted entry simply regenerates on revisit (a re-read/-OCR, or a fresh
    /// describe); with the cap this only happens for photos thousands of positions away.
    fn trim_caches(&mut self) {
        const CAP: usize = 4096;
        const HIGH: usize = CAP + CAP / 4; // only trim once ~25% over → bursty, off the hot path
        let Some(cur) = self.displayed_item else {
            return;
        };
        Self::trim_nearest(&mut self.meta_cache, cur, CAP, HIGH);
        Self::trim_nearest(&mut self.exif_cache, cur, CAP, HIGH);
        Self::trim_nearest(&mut self.recognized_text, cur, CAP, HIGH);
        Self::trim_nearest(&mut self.descriptions, cur, CAP, HIGH);
    }

    /// Evict entries farthest (by item index) from `cur` until `map` holds at most `cap` — but
    /// only once it passes `high`, so the sort runs in bursts rather than every tick.
    fn trim_nearest<V>(
        map: &mut std::collections::HashMap<usize, V>,
        cur: usize,
        cap: usize,
        high: usize,
    ) {
        if map.len() <= high {
            return;
        }
        let mut keys: Vec<usize> = map.keys().copied().collect();
        keys.sort_unstable_by_key(|&k| k.abs_diff(cur));
        for k in keys.into_iter().skip(cap) {
            map.remove(&k);
        }
    }

    /// Build the [`contract::MenuState`] for the given live state — the pure mapping from
    /// the app's view/edit state to the shell-neutral menu model. Takes no `self` and
    /// touches no muda, so it's unit-tested directly (`menu_state_*` tests). The
    /// mappings it owns are the only non-trivial logic: `pb_render::ScaleMode` → the
    /// View scale group, and the panel state → the info checkmarks (the basic line and
    /// the Inspector's Details tab check independently — they decoupled in task #54)
    /// plus the Hide Panels toggle (checked while hidden, enabled only with a panel
    /// open — matching the `Tab` no-op).
    #[allow(clippy::too_many_arguments)]
    pub fn menu_state_from(
        scale: ScaleMode,
        info_line: bool,
        panels: Panels,
        tree_open: bool,
        recursive: bool,
        fullscreen: bool,
        slideshow: bool,
        mute_live_audio: bool,
        save_rotation_enabled: bool,
        reveal_enabled: bool,
        cancel_scan_enabled: bool,
        undo: Option<&'static str>,
        native_fullscreen_engaged: bool,
        displayed_item: Option<usize>,
        compare_pin: Option<usize>,
    ) -> contract::MenuState {
        contract::MenuState {
            scale: match scale {
                ScaleMode::Fit => contract::ScaleMode::Fit,
                ScaleMode::Fill => contract::ScaleMode::Fill,
                ScaleMode::Original => contract::ScaleMode::Original,
            },
            info_basic: info_line,
            // The Details tab checks whether visible or Tab-hidden — hidden ≠ closed,
            // and the Hide Panels checkmark explains the invisibility.
            info_full: panels.inspector == Some(InspectorTab::Details),
            panels_hidden: panels.hidden,
            hide_panels_enabled: panels.any_open(tree_open, info_line),
            recursive,
            fullscreen,
            slideshow,
            mute_live_audio,
            // Compare (task #43): both raw states cross so the derivation lives HERE,
            // the one choke point, instead of drifting per shell.
            compare_pin_enabled: displayed_item.is_some(),
            compare_pinned_here: displayed_item.is_some() && displayed_item == compare_pin,
            compare_toggle_enabled: compare_pin.is_some() && displayed_item.is_some(),
            save_rotation_enabled,
            reveal_enabled,
            cancel_scan_enabled,
            undo,
            native_fullscreen_engaged,
        }
    }

    /// The saved windowed geometry to restore, but only when enough of it still lands
    /// on one of `monitors` — else `None`, so a window saved on a now-disconnected or
    /// rearranged monitor opens at the default spot instead of off-screen (#1).
    pub fn windowed_restore(
        &self,
        monitors: &[(i32, i32, u32, u32)],
    ) -> Option<settings::WindowGeometry> {
        let g = self.settings.window?;
        settings::geometry_on_screen(
            g,
            monitors,
            settings::MIN_VISIBLE_W,
            settings::MIN_VISIBLE_H,
        )
        .then_some(g)
    }

    /// Step the zoom by `factor` (menu Zoom In/Out — the keyboard zoom is the
    /// continuous hold-to-zoom). Multiplies the current zoom, clamps to the allowed
    /// range, and re-frames. `factor` > 1 zooms in, < 1 zooms out.
    pub fn zoom_step(&mut self, factor: f32) {
        self.view.zoom = (self.view.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.push_view();
        self.draw();
    }

    /// Toggle the keybindings help panel (`/` or `?`). Shares the single HUD overlay
    /// slot with the Inspector (interim), so opening it replaces whichever tab was
    /// showing; while `Tab`-hidden it reveals instead of closing (the reveal rule).
    pub fn toggle_help(&mut self) {
        self.panels.toggle_help();
        self.refresh_slot();
    }

    /// Toggle rich-panel visibility (`Tab`, task #54): hide the Inspector/Help/tree
    /// **and** the basic `i` info line without closing/un-toggling any of them, or
    /// reveal them all. No-op when nothing is open (including the line). Toasts and
    /// hints stay their own ephemeral layer, untouched by `Tab`.
    pub fn toggle_panels(&mut self) {
        if !self
            .panels
            .toggle_hidden(self.folder_tree_open, self.info_line)
        {
            return;
        }
        self.refresh_tree_visibility();
        self.refresh_slot();
    }

    /// Re-render or clear the shared overlay slot after panel state changed, and sync
    /// the info line's drawn state alongside it — both can flip on any action that
    /// touches `panels.hidden`, and this is the one choke point nearly all of them
    /// already call.
    fn refresh_slot(&mut self) {
        if self.slot_content().is_some() {
            self.show_overlay();
        } else {
            self.hide_overlay();
        }
        self.refresh_info_line_visibility();
    }

    /// Apply the info line's visibility after a hide/reveal: hide it while
    /// `Tab`-hidden (state stays "on" for when panels reveal), draw it when revealed
    /// — mirrors `refresh_tree_visibility`. Applied eagerly rather than left to the
    /// next tick, since the app sleeps when idle and a tick may not run again soon.
    fn refresh_info_line_visibility(&mut self) {
        if self.info_line && !self.panels.hidden {
            if !self.info_line_shown || self.info_line_item != self.displayed_item {
                self.show_info_line();
            }
        } else if self.info_line_shown {
            self.hide_info_line();
        }
    }

    /// Apply the tree's visibility after a hide/reveal: clear the bitmap when hidden,
    /// force a rebuild against fresh state when revealed (the signature gate re-runs
    /// the derivation next tick).
    fn refresh_tree_visibility(&mut self) {
        if self.panels.tree_visible(self.folder_tree_open) {
            self.folder_tree_sig = None; // rebuild + re-upload next tick
        } else {
            self.hide_folder_tree();
        }
    }

    /// Minimum interval between mid-flight folder-tree rebuilds: crossing a folder
    /// boundary every frame at full fly speed re-rasterizes at most ~10×/s (a ~1 ms
    /// CPU composite each), so the tree tracks live without denting the one-frame-
    /// per-vsync advance budget.
    pub const TREE_FLY_REBUILD: Duration = Duration::from_millis(100);

    /// Toggle the folder-tree overlay (`Shift+F`): the current photo's folder in its
    /// hierarchy — up affordance, root, ancestor chain, siblings, children — in the
    /// top-left corner. Rows are clickable (full Open Folder semantics) and the
    /// "… n more" windowing markers page the list. See
    /// `.taskmaster/docs/folder-tree-plan.md`.
    pub fn toggle_folder_tree(&mut self) {
        if self.panels.reveal() {
            // ⇧F while Tab-hidden reveals first and only ever *shows* (the reveal
            // rule): the tree opens/re-draws and any hidden Inspector/Help panel
            // comes back with it — `hidden` is one master flag, the Photoshop idiom.
            self.folder_tree_open = true;
            self.show_folder_tree();
            self.refresh_slot();
            return;
        }
        self.folder_tree_open = !self.folder_tree_open;
        if self.folder_tree_open {
            self.show_folder_tree();
        } else {
            self.hide_folder_tree();
        }
    }

    /// The displayed photo's containing folder as a forward-slashed path — the
    /// tree's cheap identity, no I/O. Root-relative (`""` = the root level) for
    /// photos under the root; the **absolute** parent for out-of-root photos
    /// (explicit multi-folder decks), so two different folders never collapse to
    /// the same rebuild signature.
    fn current_folder_rel(&self, item: usize) -> String {
        match self.source.path(item) {
            Some(p) => crate::folder_tree::folder_identity(p, &self.root),
            None => crate::folder_tree::folder_of(self.source.name(item)).to_string(),
        }
    }

    /// The drawn tree's rebuild signature: deck root + current folder (`@root` for
    /// an empty deck, which browses from the root itself). Compared per tick while
    /// the overlay is open (string ops only — the `read_dir`s in
    /// [`show_folder_tree`](Self::show_folder_tree) run only when this changes).
    fn folder_sig(&self) -> String {
        match self.displayed_item {
            Some(item) => format!("{}|{}", self.root.display(), self.current_folder_rel(item)),
            None => format!("{}|@root", self.root.display()),
        }
    }

    /// The per-deck folder-counts map (`disk_counts` over the playlist), cached by
    /// (root, deck length) so the badges and the flight fast path never re-walk an
    /// unchanged deck. One O(n) pass when the deck (or a streaming batch) changes.
    fn folder_counts(&mut self) -> Arc<std::collections::HashMap<PathBuf, u64>> {
        if let Some((r, n, map)) = &self.folder_tree_counts {
            if *r == self.root && *n == self.source.len() {
                return map.clone();
            }
        }
        let map = Arc::new(crate::folder_tree::disk_counts(
            (0..self.source.len()).filter_map(|i| self.source.path(i)),
            &self.root,
        ));
        self.folder_tree_counts = Some((self.root.clone(), self.source.len(), map.clone()));
        map
    }

    /// Derive + rasterize + draw the folder tree for the current deck state, and
    /// stamp [`folder_tree_sig`](crate::AppCore::folder_tree_sig). Hover and page
    /// state reset — this is the fresh-content path; transitions re-render through
    /// [`push_folder_tree`](Self::push_folder_tree) from the cached rows instead.
    pub fn show_folder_tree(&mut self) {
        self.show_folder_tree_mode(false);
    }

    /// [`show_folder_tree`](Self::show_folder_tree) with the derivation choice:
    /// `lite` = the no-I/O flight variant (its signature is stamped `|lite`, so
    /// settling upgrades to the full `read_dir` view).
    ///
    /// Archive decks group their in-RAM entry names — no I/O, drawn right here.
    /// Disk decks (and the empty deck, which browses from the root so a photo-less
    /// folder never strands you) paint the **lite** view immediately — sibling and
    /// child folders from the cached counts map, pure in-RAM — and, for the full
    /// view, hand the `read_dir` derivation to an off-thread worker that `tick`
    /// installs when it lands. The disk I/O never runs on this thread: a
    /// spun-down drive or a dead network share must not stall the event loop.
    fn show_folder_tree_mode(&mut self, lite: bool) {
        // Check the cheap gates before deriving rows, so a font-less host doesn't
        // pay the derivation on every retry tick. A Tab-hidden tree derives nothing
        // either — reveal forces the rebuild via the cleared signature. A disk deck on
        // the native host uses the resident Finder tree (`drive_fs_tree`), not this.
        if self.hud.is_none()
            || !self.panels.tree_visible(self.folder_tree_open)
            || self.tree_is_fs()
        {
            return;
        }
        let sig = self.folder_sig();
        let lite_stamp = format!("{sig}|lite");

        // An archive deck: entry names carry the internal folder paths; the
        // archive file labels the root, and the up row opens the folder on disk
        // containing it. The derivation reads the **full** (unscoped) source, so
        // a deck scoped to one internal folder still shows the whole archive
        // around it — the archive analog of the disk tree anchoring above the
        // opened root; clicking a row re-scopes to its prefix (the root row =
        // back to everything).
        if let Some(item) = self.displayed_item {
            if self.source.path(item).is_none() {
                let full = self
                    .archive_scope
                    .as_ref()
                    .map(|s| Arc::clone(&s.full))
                    .unwrap_or_else(|| Arc::clone(&self.source));
                let container = self.source.container().unwrap_or(&self.root);
                let label = crate::folder_tree::name_of(container);
                let current = self.current_folder_rel(item);
                let names = (0..full.len()).map(|i| full.name(i));
                let mut m = crate::folder_tree::rows_from_names(names, &current, &label);
                if let Some(par) = container.parent().filter(|p| !p.as_os_str().is_empty()) {
                    let par = par.to_path_buf();
                    m.push_up(&crate::folder_tree::name_of(&par), par.clone());
                }
                self.push_folder_tree(m.rows, m.targets, 0, None);
                self.folder_tree_sig = Some(sig);
                return;
            }
        }

        // A disk deck, anchored at the opened root (never above it — the up row is
        // the one deliberate exit); an empty deck browses from the root itself.
        let disk_dir: Option<PathBuf> = match self.displayed_item {
            Some(item) => self
                .source
                .path(item)
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            None => {
                (!self.root.as_os_str().is_empty() && self.root.is_dir()).then(|| self.root.clone())
            }
        };
        let Some(dir) = disk_dir else {
            // Nothing to show (bare launch): drop any stale quad, remember why.
            if self.folder_tree_panel.is_some() {
                self.hide_folder_tree();
            }
            self.folder_tree_sig = Some(if lite { lite_stamp } else { sig });
            return;
        };
        // Paint the lite view now — unless this folder's lite view is already up
        // (the settle-upgrade path), so upgrading doesn't reset hover/page.
        if self.folder_tree_sig.as_deref() != Some(lite_stamp.as_str()) {
            let counts = self.folder_counts();
            let model = crate::folder_tree::rows_from_paths(&self.root, &dir, &counts);
            self.push_folder_tree(model.rows, model.targets, 0, None);
        }
        self.folder_tree_sig = Some(lite_stamp);
        if !lite {
            // The settled read_dir view (it adds photo-less folders) derives
            // off-thread; the `|lite` stamp doubles as its "pending" marker.
            let counts = self.recursive.then(|| self.folder_counts());
            self.tree_io = Some(crate::folder_tree::spawn_full_tree(
                self.root.clone(),
                dir,
                counts,
                sig,
            ));
        }
    }

    /// Rasterize + upload the tree from prepared rows — the shared path for fresh
    /// derivations, hover transitions, and paging (the latter two reuse the cached
    /// rows, so they never re-derive and never touch the disk).
    fn push_folder_tree(
        &mut self,
        rows: Vec<hud::TreeRow>,
        targets: Vec<Option<crate::folder_tree::TreeTarget>>,
        page: i32,
        hovered: Option<hud::TreeHit>,
    ) {
        // Native host (task #54): don't rasterize — store the full rows/targets for the
        // SwiftUI list (which scrolls, so no HUD windowing / paging markers or hit rects)
        // and signal the shell to re-pull. Reached only on a real derivation change
        // (hover/paging are HUD-only and never fire on this path), so we always emit.
        if self.native_tree {
            self.folder_tree_panel = Some(crate::overlay::TreePanel {
                w: 0,
                h: 0,
                margin: 0,
                hits: Vec::new(),
                targets,
                rows,
                hovered: None,
                page: 0,
                built: self.now,
            });
            self.emit_panels_changed();
            return;
        }
        let px = (15.0 * self.viewport.scale_factor).max(8.0);
        let pad = (7.0 * self.viewport.scale_factor).round().max(2.0) as u32;
        let margin = self.overlay_margin();
        let full_max_h = (self.viewport.height as i32 - 2 * margin as i32).max(1);
        let render = |hud: &pb_hud::hud::Hud, max_h: i32| {
            hud.render_tree(
                &rows,
                px,
                pad,
                hud.theme().bg,
                max_h,
                page,
                hovered,
                hud::TreeCounts::Capsule,
            )
        };
        let Some(hud) = self.hud.as_ref() else {
            return;
        };
        let Some((mut bitmap, mut w, mut h, mut hits)) = render(hud, full_max_h) else {
            return;
        };
        // The tree is top-left-anchored and a full-height one reaches the bottom strip;
        // if the info line overlaps the tree's column `[margin, margin + w]`, cap the
        // height by the line strip and re-render so a tall tree pages one row shorter
        // and leaves the line room (task #54). Only left/center/wide lines trigger this
        // — the default right line clears a normal tree column, so no re-render.
        let reserve = self.info_line_reserve_for(margin as f32, margin as f32 + w as f32);
        if reserve > 0 {
            let capped = (full_max_h - reserve as i32).max(1);
            if let Some(hud) = self.hud.as_ref() {
                if let Some(re) = render(hud, capped) {
                    (bitmap, w, h, hits) = re;
                }
            }
        }
        if let Some(a) = self.renderer.as_mut() {
            a.set_tree(Some((&bitmap, w, h)), margin);
        }
        self.folder_tree_panel = Some(crate::overlay::TreePanel {
            w,
            h,
            margin,
            hits,
            targets,
            rows,
            hovered,
            page,
            built: self.now,
        });
        self.draw();
    }

    /// Whether the **native** folder tree should be visible — the signal the mac host
    /// reads to show/hide its SwiftUI list: the tree is open, not `Tab`-hidden, and the
    /// host presents it natively.
    pub fn tree_panel_visible(&self) -> bool {
        self.native_tree && self.panels.tree_visible(self.folder_tree_open)
    }

    /// Activate a native tree row by index (a SwiftUI list click): navigate its target —
    /// open the folder, or re-scope the archive — exactly like the HUD tree's row click.
    /// Rows without a target (the current folder, a bare label) are inert.
    pub fn tree_activate(&mut self, index: usize) {
        let target = self
            .folder_tree_panel
            .as_ref()
            .and_then(|p| p.targets.get(index).cloned().flatten());
        match target {
            Some(crate::folder_tree::TreeTarget::Dir(dir)) => self.open_dir(dir),
            Some(crate::folder_tree::TreeTarget::Scope(prefix)) => self.rescope_archive(prefix),
            None => {}
        }
    }

    // ── Finder-style resident folder browser (task #54, native disk decks) ──────────

    /// The current photo's containing folder (absolute), or `None` on an archive/empty
    /// deck (`source.path` is `None` for an archive entry). Gates the Finder tree.
    pub fn current_folder_abs(&self) -> Option<PathBuf> {
        let item = self.displayed_item?;
        self.source.path(item)?.parent().map(Path::to_path_buf)
    }

    /// Whether the native **Finder** tree (the resident [`FsTree`]) applies right now:
    /// the host presents the tree natively and the deck is a disk deck with a current
    /// folder. Archive/empty decks fall back to the v1 `folder_tree_panel`.
    pub fn tree_is_fs(&self) -> bool {
        self.native_tree && self.current_folder_abs().is_some()
    }

    /// The Finder tree's visible rows (empty when not built).
    pub fn fs_tree_rows(&self) -> Vec<crate::fs_tree::Row> {
        self.fs_tree.as_ref().map(|t| t.rows()).unwrap_or_default()
    }

    /// The name of the Finder tree root's parent — the label for the "up to parent" row
    /// (clicking it climbs a level). `None` at the filesystem root or when not built.
    pub fn fs_tree_parent_name(&self) -> Option<String> {
        self.fs_tree.as_ref().and_then(|t| t.parent_name())
    }

    /// Build (or re-root) the resident tree for the current disk deck and mark the current
    /// folder. Kept persistent while the current folder stays under the tree root (so
    /// browsing/expansion survives photo navigation); re-rooted only when the deck opens
    /// somewhere outside it. Fresh trees root one level above the deck root (so the deck
    /// root shows among its siblings).
    fn ensure_fs_tree(&mut self) {
        let Some(current) = self.current_folder_abs() else {
            self.fs_tree = None;
            self.fs_tree_io = None;
            return;
        };
        let rebuild = self
            .fs_tree
            .as_ref()
            .is_none_or(|t| current.strip_prefix(t.root()).is_err());
        if rebuild {
            let root = self
                .root
                .parent()
                .filter(|p| !p.as_os_str().is_empty() && current.starts_with(p))
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root.clone());
            let (tx, rx) = std::sync::mpsc::channel();
            self.fs_tree = Some(crate::fs_tree::FsTree::new(root));
            self.fs_tree_io = Some(crate::app_core::FsTreeIo {
                tx,
                rx,
                pending: std::collections::HashSet::new(),
            });
        }
    }

    /// Drive the resident tree each tick while it's shown: install finished off-thread
    /// `read_dir` results, reveal + mark the current folder, kick reads for any expanded-
    /// but-unread folder, and refresh count badges — signalling the host on change.
    fn drive_fs_tree(&mut self) {
        self.ensure_fs_tree();
        if self.fs_tree.is_none() {
            return;
        }
        let mut changed = false;
        // 1. Install finished reads.
        let done: Vec<(PathBuf, Vec<PathBuf>)> = self
            .fs_tree_io
            .as_ref()
            .map(|io| io.rx.try_iter().collect())
            .unwrap_or_default();
        for (path, subdirs) in done {
            if let Some(io) = self.fs_tree_io.as_mut() {
                io.pending.remove(&path);
            }
            if let Some(t) = self.fs_tree.as_mut() {
                t.set_children(&path, subdirs);
            }
            changed = true;
        }
        // 2. Reveal + mark the current folder (only when it moved).
        if let Some(folder) = self.current_folder_abs() {
            let moved = self
                .fs_tree
                .as_ref()
                .and_then(|t| t.current().map(Path::to_path_buf))
                != Some(folder.clone());
            if moved {
                if let Some(t) = self.fs_tree.as_mut() {
                    t.set_current(folder);
                }
                changed = true;
            }
        }
        // 3. Kick an off-thread read for each expanded-but-unread visible folder.
        let to_read: Vec<PathBuf> = self
            .fs_tree
            .as_ref()
            .map(|t| {
                t.rows()
                    .into_iter()
                    .filter(|r| r.loading)
                    .map(|r| r.path)
                    .collect()
            })
            .unwrap_or_default();
        for path in to_read {
            let in_flight = self
                .fs_tree_io
                .as_ref()
                .is_some_and(|io| io.pending.contains(&path));
            if in_flight {
                continue;
            }
            if let Some(io) = self.fs_tree_io.as_mut() {
                io.pending.insert(path.clone());
                let tx = io.tx.clone();
                std::thread::spawn(move || {
                    let subs = crate::folder_tree::subdirs(&path)
                        .into_iter()
                        .map(|n| path.join(n))
                        .collect();
                    let _ = tx.send((path, subs));
                });
            }
        }
        // 4. Refresh count badges from the deck's folder-counts (cheap when cached).
        if changed {
            let counts = self.folder_counts();
            if let Some(t) = self.fs_tree.as_mut() {
                for (p, c) in counts.iter() {
                    t.set_count(p, Some(*c));
                }
            }
            self.emit_panels_changed();
        }
    }

    /// Toggle a folder's expansion (the chevron) — browsing only, never loads photos.
    pub fn fs_tree_toggle(&mut self, path: &Path) {
        if let Some(t) = self.fs_tree.as_mut() {
            t.toggle(path);
            self.emit_panels_changed();
        }
    }

    /// Open a folder from the tree (a name click) — load its photos (increment 3 adds the
    /// keep-deck-until-photos safety; for now the shared recursive open).
    pub fn fs_tree_open(&mut self, path: PathBuf) {
        self.open_dir(path);
    }

    /// The up-affordance: re-root the tree one level higher (kicks its read next tick).
    pub fn fs_tree_extend_up(&mut self) {
        if self.fs_tree.as_mut().is_some_and(|t| t.extend_root_up()) {
            self.emit_panels_changed();
        }
    }

    /// Hide the folder tree (clears its quad + interactive state). The open/closed
    /// *state* stays with the caller — [`toggle_folder_tree`](Self::toggle_folder_tree)
    /// flips it.
    pub fn hide_folder_tree(&mut self) {
        if let Some(a) = self.renderer.as_mut() {
            a.set_tree(None, 0);
        }
        self.folder_tree_sig = None;
        self.folder_tree_panel = None;
        self.draw();
    }

    /// The interactive tree hit under a physical-px cursor point: a clickable folder
    /// row (one with a target) or a paging marker. The panel sits `margin` px in
    /// from the top-left, so screen rects derive from the live geometry — resize-
    /// and DPI-proof, like the other interactive overlays.
    pub fn folder_tree_hit(&self, x: f32, y: f32) -> Option<hud::TreeHit> {
        if !self.folder_tree_open {
            return None;
        }
        let p = self.folder_tree_panel.as_ref()?;
        let (x0, y0) = (p.margin as f32, p.margin as f32);
        for (hit, r) in &p.hits {
            let rect = [
                x0 + r[0] as f32,
                y0 + r[1] as f32,
                x0 + (r[0] + r[2]) as f32,
                y0 + (r[1] + r[3]) as f32,
            ];
            if point_in_rect(rect, x, y) {
                return match hit {
                    hud::TreeHit::Row(i) => p.targets.get(*i)?.is_some().then_some(*hit),
                    _ => Some(*hit),
                };
            }
        }
        None
    }

    /// Track the pointer over the tree: on a hover **transition** (enter/leave/move
    /// between hits), re-render the panel from its cached rows so the hovered row's
    /// band lights up — the chip-hover pattern; nothing runs per-move or per-frame.
    pub fn update_tree_hover(&mut self) {
        let hovered = self
            .last_cursor
            .and_then(|[x, y]| self.folder_tree_hit(x, y));
        let Some(panel) = self.folder_tree_panel.as_ref() else {
            return;
        };
        if panel.hovered == hovered {
            return;
        }
        let Some(panel) = self.folder_tree_panel.take() else {
            return;
        };
        self.push_folder_tree(panel.rows, panel.targets, panel.page, hovered);
    }

    /// A left-press over the folder tree: a "… n more" marker pages the window; a
    /// folder row opens that folder — full Open Folder semantics, the same plan the
    /// picker/drop path builds. Returns whether the press was consumed (the shells'
    /// click ladders fall through to drag-to-pan otherwise).
    pub fn folder_tree_click(&mut self) -> bool {
        let Some(hit) = self
            .last_cursor
            .and_then(|[x, y]| self.folder_tree_hit(x, y))
        else {
            return false;
        };
        match hit {
            hud::TreeHit::PageUp | hud::TreeHit::PageDown => {
                let delta = if hit == hud::TreeHit::PageUp { -1 } else { 1 };
                let Some(panel) = self.folder_tree_panel.take() else {
                    return true;
                };
                // Render the new page without hover; the immediate re-check lights
                // whatever now sits under the still cursor.
                self.push_folder_tree(panel.rows, panel.targets, panel.page + delta, None);
                self.update_tree_hover();
                true
            }
            hud::TreeHit::Row(i) => {
                let target = self
                    .folder_tree_panel
                    .as_ref()
                    .and_then(|p| p.targets.get(i).cloned().flatten());
                match target {
                    Some(crate::folder_tree::TreeTarget::Dir(dir)) => self.open_dir(dir),
                    Some(crate::folder_tree::TreeTarget::Scope(prefix)) => {
                        self.rescope_archive(prefix)
                    }
                    None => {}
                }
                true
            }
        }
    }

    /// Open `dir` exactly like choosing it in the Open Folder picker or dropping it
    /// on the window — the shared plan (recursive per the launch policy), so tree
    /// clicks and the Go commands can't drift from the canonical open path.
    pub fn open_dir(&mut self, dir: PathBuf) {
        let plan = pb_core::open::plan(pb_core::open::LaunchInput::Directory(dir));
        self.open_plan(plan.source, plan.cursor);
    }

    /// Re-scope the archive deck to the entries under the internal folder
    /// `prefix` (`""` = back to the whole archive) — the archive analog of
    /// [`open_dir`](Self::open_dir), sharing its rebuild semantics (cursor to
    /// the first item, caches dropped). Pure in-RAM: the full source is wrapped
    /// in a `ScopedSource` (one pass over the resident name list), never
    /// re-opened — a solid 7z's eager decode is paid once, and an unlocked
    /// encrypted archive stays unlocked. Silent no-op on a disk deck.
    pub fn rescope_archive(&mut self, prefix: String) {
        let Some(scope) = self.archive_scope.clone() else {
            return;
        };
        let source: Arc<dyn PhotoSource> = if prefix.is_empty() {
            Arc::clone(&scope.full)
        } else {
            Arc::new(pb_source::ScopedSource::new(
                Arc::clone(&scope.full),
                &prefix,
            ))
        };
        // A scope only ever comes from a tree row / sibling step, which derive
        // from the entry names — so it can't be empty; the guard is belt and
        // braces (rebuild_playlist refuses an empty deck anyway, un-stamped).
        if source.is_empty() {
            return;
        }
        self.rebuild_playlist(source, self.root.clone(), None, false, 0);
        self.archive_scope = Some(crate::ArchiveScope {
            full: scope.full,
            prefix,
        });
    }

    /// Go ▸ parent folder (⌘↑ / Alt+↑ — Finder's Enclosing Folder idiom): open the
    /// deck anchor's parent. A disk deck's anchor is the opened root. An archive
    /// deck scoped to an internal folder steps the scope up one level first
    /// (`a/b` → `a` → the whole archive); from the archive root, "up" opens the
    /// folder on disk containing the archive file. Silent no-op with nothing
    /// open or at a filesystem root.
    pub fn open_parent_cmd(&mut self) {
        if let Some(scope) = &self.archive_scope {
            if !scope.prefix.is_empty() {
                let parent = crate::folder_tree::folder_of(&scope.prefix).to_string();
                self.rescope_archive(parent);
                return;
            }
        }
        // An archive deck at its root goes up to the disk folder *containing* the archive.
        // A normal disk deck **climbs one level per press**: the first ⌘↑ goes up from the
        // current photo's folder; each subsequent ⌘↑ continues up from the folder the last
        // one opened (`climb_anchor`) — not the current photo's folder, which stays at the
        // deepest level (a parent with no direct photos re-lands it there), so anchoring on
        // it would get stuck oscillating. The climb resets the moment any other open happens.
        let anchor = self
            .source
            .container()
            .map(Path::to_path_buf)
            .or_else(|| self.climb_anchor.clone())
            .or_else(|| self.current_folder_abs())
            .unwrap_or_else(|| self.root.clone());
        if anchor.as_os_str().is_empty() {
            return;
        }
        if let Some(par) = anchor.parent().filter(|p| !p.as_os_str().is_empty()) {
            let par = par.to_path_buf();
            self.open_dir(par.clone()); // clears climb_anchor (via open_plan)…
            self.climb_anchor = Some(par); // …then remembers this rung for the next ⌘↑.
        }
    }

    /// Go ▸ previous / next folder (`dir` = ∓1; ⌘←/⌘→ / Alt+←/→): open the nearest
    /// sibling directory **with photos** in that direction — photo-less siblings
    /// are skipped (#49: name-adjacency dead-ended behind a "No supported images"
    /// modal, and since a failed open never moves the root, re-pressing retried
    /// the same empty folder forever). The search runs on the tree-io worker
    /// (per-candidate probes can walk entire subtrees, and even the `is_dir`
    /// stat can stall on a dead share); `tick` opens the target when it lands,
    /// or toasts when nothing that way has photos. A rapid re-press supersedes
    /// AND cancels the in-flight search. On an archive deck scoped to an
    /// internal folder, step to the adjacent sibling *prefix* in the same
    /// sorted row the tree shows — pure in-RAM, no worker, and never photo-less
    /// by construction (archive folders derive from image entry names); the
    /// row's ends toast the same way. Silent no-op with nothing open.
    pub fn open_sibling_cmd(&mut self, dir: i32) {
        // Stepping to a sibling folder ends an Open-Parent (⌘↑) climb — the next ⌘↑ restarts
        // from the folder you land on, not the stale climb rung.
        self.climb_anchor = None;
        if let Some(scope) = &self.archive_scope {
            let full = Arc::clone(&scope.full);
            let names = (0..full.len()).map(|i| full.name(i));
            match crate::folder_tree::sibling_scope(names, &scope.prefix, dir) {
                Some(sib) => self.rescope_archive(sib),
                None => self.show_toast("No more folders with images"),
            }
            return;
        }
        if self.source.container().is_some() {
            return;
        }
        // "Next/previous photo, but by folder": jump within the deck to the next/previous
        // folder *boundary* in the deck's (tree-order) sequence — entering subfolders,
        // stepping siblings, or climbing back up, exactly as the traversal runs. Instant,
        // and it can never hit "No photos" (every jump lands on a real deck item).
        if let Some(idx) = self.adjacent_folder_item(dir) {
            self.stop_playback();
            self.playlist.jump_to(idx);
            self.target_item = self.playlist.current();
            self.try_present_target();
            self.request_prefetch();
            return;
        }
        // No adjacent folder in the deck. A multi-folder deck means you're at its first /
        // last folder — toast (HUD-gated, so the host shows it). A single-folder deck opens
        // the next folder *on disk* (the disk sibling of the current folder, skipping
        // photo-less ones), so ⌘←/→ still browses when the deck is just one folder.
        if self.deck_spans_multiple_folders() {
            self.show_toast("No more folders");
            return;
        }
        let anchor = self
            .current_folder_abs()
            .unwrap_or_else(|| self.root.clone());
        if anchor.as_os_str().is_empty() {
            return;
        }
        self.tree_io = Some(crate::folder_tree::spawn_sibling(anchor, dir));
    }

    /// The source index at the next (`dir > 0`) / previous folder **boundary** in the deck
    /// sequence: forward → the first item after the current whose folder differs (the start
    /// of the next folder-run); backward → the start of the previous folder-run. `None` at
    /// the deck's last / first folder. Pure, RAM-only — the deck is already tree-ordered.
    fn adjacent_folder_item(&self, dir: i32) -> Option<usize> {
        let n = self.source.len();
        let c = self.displayed_item.filter(|&c| c < n)?;
        let folder = |i: usize| self.source.path(i).and_then(Path::parent);
        let cur = folder(c)?.to_path_buf();
        if dir > 0 {
            (c + 1..n).find(|&i| folder(i) != Some(cur.as_path()))
        } else {
            // Walk back to the start of the current run, then to the start of the one before.
            let mut s = c;
            while s > 0 && folder(s - 1) == Some(cur.as_path()) {
                s -= 1;
            }
            if s == 0 {
                return None; // already in the deck's first folder-run
            }
            let prev = folder(s - 1)?.to_path_buf();
            let mut p = s - 1;
            while p > 0 && folder(p - 1) == Some(prev.as_path()) {
                p -= 1;
            }
            Some(p)
        }
    }

    /// Whether the deck's photos span more than one folder (early-exits on the second).
    fn deck_spans_multiple_folders(&self) -> bool {
        let mut first: Option<&Path> = None;
        for i in 0..self.source.len() {
            if let Some(f) = self.source.path(i).and_then(Path::parent) {
                match first {
                    None => first = Some(f),
                    Some(f0) if f0 != f => return true,
                    _ => {}
                }
            }
        }
        false
    }

    /// Apply the keymap edited in the Settings dialog: swap it in live (every keypress
    /// resolves through `self.keymap`, so future input uses it immediately) and persist
    /// `keymap.toml`. If the help overlay is open, rebuild it so its key labels — read
    /// from the live keymap — reflect the new bindings.
    pub fn apply_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
        self.keymap.save();
        if self.overlay_shown && self.slot_content() == Some(SlotContent::Help) {
            self.show_overlay();
        }
        // The Help panel's shortcut labels just changed — nudge a native Help view to
        // re-pull (visibility didn't change, so the tick diff wouldn't catch it).
        if self.help_panel_visible() {
            self.emit_panels_changed();
        }
    }

    /// Refresh rate in Hz (rounded, ≥1) — caps the Settings fly-speed slider and is
    /// passed to every dialog window.
    pub fn refresh_hz(&self) -> u32 {
        (1.0 / self.frame_interval.as_secs_f32()).round().max(1.0) as u32
    }

    /// Perform a deferred delete's playlist advance: drop the removed item, rebuild the
    /// source from the remaining paths (indices shift, so index-keyed state resets —
    /// fine for an explicit, infrequent command), and advance to the next photo (the
    /// previous if it was the last; the empty state if none remain). Idempotent — a
    /// no-op when nothing is pending.
    pub fn flush_pending_delete(&mut self) {
        let Some((_, removed)) = self.pending_delete.take() else {
            return;
        };
        let len = self.source.len();
        // If a scan is still streaming in, tombstone the deleted path so a later batch (whose
        // cumulative list still has it) can't bring it back. (No-op once the scan finishes.)
        if self.scanning {
            if let Some(p) = self.source.path(removed).map(Path::to_path_buf) {
                self.deleted.insert(p);
            }
        }
        match cursor_after_removal(len, removed) {
            None => self.enter_empty_state(),
            Some(start) => {
                let remaining: Vec<PathBuf> = (0..len)
                    .filter(|&i| i != removed)
                    .filter_map(|i| self.source.path(i).map(Path::to_path_buf))
                    .collect();
                let src: Arc<dyn PhotoSource> = Arc::new(FsSource::new(remaining));
                let root = self.root.clone();
                let scan_root = self.scan_root.clone();
                let recursive = self.recursive;
                self.rebuild_playlist(src, root, scan_root, recursive, start);
            }
        }
    }

    /// Clear to the "no images" placeholder after the last photo is deleted. Mirrors
    /// the bare-launch empty state (a test pattern + title; `O`/drag-drop reopen).
    pub fn enter_empty_state(&mut self) {
        self.pending_delete = None;
        self.stop_playback(); // the deleted photo may have been playing (#37)
        self.source = Arc::new(FsSource::new(Vec::new()));
        self.archive_scope = None; // the empty deck is not an archive
        self.playlist = Playlist::new(0, 0);
        self.rotations.clear();
        self.meta_cache.clear();
        self.exif_cache.clear();
        self.recognized_text.clear();
        self.text_scan = None;
        self.text_gen += 1;
        self.descriptions.clear();
        self.describe_scan = None;
        self.describe_gen += 1;
        self.live_motion_cache.clear();
        self.failed.clear();
        self.preview_resident.clear();
        self.upgrade_done.clear();
        self.last_upgrade_set.clear();
        self.undo_stack.clear();
        self.invalidate_geometry();
        self.displayed_item = None;
        self.target_item = None;
        self.clear_compare_pin();
        self.current = None;
        if let Some(r) = self.renderer.as_mut() {
            r.clear_image();
            r.set_overlay(None, 0, 0);
            r.set_info_line(None, 0, pb_render::HAlign::Right);
        }
        self.effects
            .push(contract::CoreEffect::SetTitle("PhotoBlaze".to_string()));
        // Blank background + the centered "Press O to open…" hint (mirrors a bare launch).
        self.show_open_hint();
        self.overlay_shown = false;
        self.overlay_item = None;
        // Keep the `i` enabled preference; just drop the drawn strip (no photo to
        // describe). The tick re-shows it once a new photo lands.
        self.info_line_shown = false;
        self.info_line_item = None;
        self.info_line_h = 0;
        self.draw();
    }

    /// Replace the playlist with a new source and re-show at `start`. Every bit
    /// of index-keyed state (per-item rotation overrides, the metadata cache, the
    /// failed set, the resident ring) is dropped because the indices are
    /// reassigned; the geometry-epoch bump discards any in-flight decode for the
    /// old set.
    pub fn rebuild_playlist(
        &mut self,
        source: Arc<dyn PhotoSource>,
        root: PathBuf,
        scan_root: Option<PathBuf>,
        recursive: bool,
        start: usize,
    ) {
        if source.is_empty() {
            return;
        }
        let start = start.min(source.len() - 1);
        self.pending_delete = None; // any rebuild supersedes a deferred delete-advance
        self.stop_playback(); // a new source drops any playback of the old one (#2)
                              // A rebuild is a new deck: drop any archive scoping. The archive paths
                              // (`apply_archive`, `rescope_archive`) re-stamp it right after this call.
        self.archive_scope = None;
        self.source = source;
        self.root = root;
        self.scan_root = scan_root;
        self.recursive = recursive;
        // Remember the opened folder as the Open dialog's default start on a fresh
        // launch (settings::last_folder — the owner-approved exception to the
        // no-viewing-trace rule; it never auto-opens anything). Only folder-backed
        // opens record (an archive has no folder), only on an actual change, and the
        // write rides this explicit open action — never the view path. Gated by
        // `persist_prefs` so unit tests never write the real settings.toml.
        if let Some(dir) = &self.scan_root {
            if self.settings.last_folder.as_deref() != Some(dir.as_path()) {
                self.settings.last_folder = Some(dir.clone());
                if self.persist_prefs {
                    self.settings.save();
                }
            }
        }
        self.playlist = Playlist::new(self.source.len(), crate::engine::fresh_shuffle_seed())
            .with_cursor(start);
        // Re-resolve the compare pin by identity against the new source: it survives a
        // same-deck rebuild (delete-advance, recursive toggle — same paths, new
        // indices); a genuinely new deck can't match, so the pin clears silently. The
        // return position is transient and always drops with the old indices.
        self.compare_return = None;
        self.compare_pin = self
            .compare_pin_id
            .as_ref()
            .and_then(|id| (0..self.source.len()).find(|&i| &self.compare_identity(i) == id));
        if self.compare_pin.is_none() {
            self.compare_pin_id = None;
        }
        // Indices are reassigned — drop everything keyed by item index.
        self.rotations.clear();
        self.meta_cache.clear();
        self.exif_cache.clear();
        self.recognized_text.clear();
        self.text_scan = None;
        self.text_gen += 1;
        self.descriptions.clear();
        self.describe_scan = None;
        self.describe_gen += 1;
        self.live_motion_cache.clear();
        self.failed.clear();
        self.preview_resident.clear();
        self.upgrade_done.clear();
        self.last_upgrade_set.clear();
        // Undo entries reference the old source's indices/paths — drop them too.
        self.undo_stack.clear();
        // Invalidate the ring + bump the epoch (discards in-flight old decodes),
        // then synchronously show the new current photo and refill around it.
        self.invalidate_geometry();
        self.displayed_item = self.playlist.current();
        self.target_item = self.playlist.current();
        self.load_current_sync();
        self.request_prefetch();
        self.effects.push(contract::CoreEffect::RequestRender);
    }

    /// Signal the empty-state open panel — used when there are no images to display. Both
    /// shells present it natively (the winit egui overlay / the macOS SwiftUI panel), so the
    /// core only signals visibility here; the tick's visibility diff drives the host to
    /// show/hide it.
    pub fn show_open_hint(&mut self) {
        // Suppress the panel while a folder scan is pending (deferred startup launch) or
        // streaming in — the first photo is about to bootstrap, so the call to action would
        // flash briefly and mislead (it implies nothing is loading). If the scan turns out
        // empty, `poll_dir_scan`'s Done arm restores it.
        if self.scanning || self.launching {
            return;
        }
        self.emit_panels_changed();
    }

    /// A monitor/DPI change re-scaled the window (e.g. dragging from a 1× display to a 2×
    /// Retina one): every CPU-rasterized overlay was baked at the old [`scale_factor`] and is
    /// cached by *content* (which didn't change), so force each to rebuild at the new DPI —
    /// otherwise the overlay text looks soft / wrong-sized on the new monitor. The photo and
    /// the info/EXIF panel are re-decoded / re-shown by the `Resized` → settle path that
    /// follows this event (using the now-updated scale); here we invalidate the per-tick
    /// overlays (the loading pie and the scan card) and the empty-state hint.
    ///
    /// [`scale_factor`]: App::scale_factor
    pub fn rescale_overlays(&mut self) {
        self.pie_pushed = None; // re-rasterize the loading pie at the new scale next tick
        self.chip_sig = None; // re-rasterize the scan card next tick
        if self.overlay_shown {
            self.overlay_item = None; // force the info/EXIF/help panel to re-show next tick
        }
        if self.folder_tree_open {
            self.folder_tree_sig = None; // re-rasterize the folder tree next tick
        }
        if self.source.is_empty() {
            self.show_open_hint(); // re-rasterize the "Press O to open" hint
        }
        self.effects.push(contract::CoreEffect::RequestRender);
    }

    /// Handle a nav keypress (space / backspace / enter). Tracks the held key for
    /// hold-to-fly, then either advances, or — when we're still catching up to the
    /// previous target, so the press can't be serviced yet — flashes the loading
    /// pie (brighten-on-keypress) so the input never feels dead.
    pub fn nav_press(&mut self, key: PbKey, action: Action) {
        self.held.insert(key, action);
        self.hold_start = Some(self.now);
        let Some(nav) = nav_of(action) else {
            return;
        };
        if self.target_item.is_some() && self.displayed_item != self.target_item {
            self.pie_glow_started = Some(self.now);
        } else {
            self.advance(nav);
        }
    }

    /// Advance one photo (sequential or random). The gated engine path: present on
    /// a ring hit, else hold the previous frame + prefetch while the decode lands.
    pub fn advance(&mut self, nav: Nav) {
        // Any in-deck navigation ends an Open-Parent (⌘↑) climb: the next ⌘↑ must restart
        // from the folder you navigated to, not resume from the stale climb rung (which would
        // surprise-jump to a near-root folder). All photo nav — Next/Prev/Random and the
        // hold-to-fly re-advance — funnels through here.
        self.climb_anchor = None;
        // Settle a deferred delete-advance before navigating, so a keypress during the
        // brief post-delete delay lands cleanly on the rebuilt playlist (no yank-back).
        self.flush_pending_delete();
        // Never advance while the previous target is still pending (a miss in
        // flight): a fast second press would overwrite it and skip that photo.
        // Holding still flies — `about_to_wait` re-advances once it's caught up.
        if self.displayed_item != self.target_item {
            return;
        }
        // Navigating away from an animated image stops playback and reverts to the
        // still (the frames are RAM-only — privacy #2). A no-op on a still.
        self.stop_playback();
        // Remember the direction so the slideshow auto-advances the way the user last
        // moved (manual nav during a slideshow steers it). The slideshow's own
        // `advance(self.last_nav)` calls are then idempotent here.
        self.last_nav = nav;
        match nav {
            Nav::Forward => self.playlist.next(),
            Nav::Backward => self.playlist.prev(),
            Nav::Random => self.playlist.random_next(),
            Nav::RandomPrev => self.playlist.random_prev(),
        }
        self.target_item = self.playlist.current();
        // Both modes use the async engine: present on a ring hit, else hold the
        // previous frame while the decode (fit-sized or full-res) lands.
        self.try_present_target();
        self.request_prefetch();
    }

    // --- Flicker compare (task #43): pin one photo, `Y` flips between it and the
    // current one at full resolution — change detection at a fixed gaze point, the
    // culling tool. The pin rides the prefetch want-list at top-2 priority
    // (`request_prefetch`), so both directions of the flip are ring rebinds.

    /// `⇧Y` / Image ▸ Pin for Compare — pin the current photo, or unpin when it's
    /// already the pin. The whole pin-management surface.
    pub fn compare_pin_cmd(&mut self) {
        let Some(item) = self.displayed_item else {
            return; // empty state — nothing to pin
        };
        if self.compare_pin == Some(item) {
            self.clear_compare_pin();
            self.show_toast_icon("Unpinned", ToastIcon::Unpin);
        } else {
            self.set_compare_pin(item);
        }
    }

    /// `Y` / Image ▸ Compare with Pinned — flip between the pinned photo and the
    /// current one. With nothing pinned yet, pins the current photo instead, so a
    /// single key drives the whole feature.
    pub fn compare_toggle_cmd(&mut self) {
        let Some(current) = self.displayed_item else {
            return;
        };
        let Some(pin) = self.compare_pin else {
            self.set_compare_pin(current);
            return;
        };
        if current == pin {
            // Viewing the pin: flip back to the remembered position. No return point
            // yet (pinned, never flipped) → nothing to do.
            if let Some(ret) = self.compare_return {
                if ret != pin && ret < self.source.len() {
                    self.compare_jump(ret);
                }
            }
        } else {
            self.compare_return = Some(current);
            self.compare_jump(pin);
        }
    }

    fn set_compare_pin(&mut self, item: usize) {
        self.compare_pin = Some(item);
        self.compare_pin_id = Some(self.compare_identity(item));
        self.compare_return = None;
        self.show_toast_icon("Pinned for compare", ToastIcon::Pin);
        // Re-issue the want-list so the pin's eviction exemption takes effect now.
        self.request_prefetch();
    }

    /// Drop the pin and its bookkeeping (deleting the pinned photo, a new deck).
    pub fn clear_compare_pin(&mut self) {
        self.compare_pin = None;
        self.compare_return = None;
        self.compare_pin_id = None;
    }

    /// The pinned item's rebuild-stable identity: the full path where one exists,
    /// else the archive-entry name.
    fn compare_identity(&self, item: usize) -> String {
        match self.source.path(item) {
            Some(p) => p.to_string_lossy().into_owned(),
            None => self.source.name(item).to_string(),
        }
    }

    /// Jump the cursor to an absolute index and present it — `advance`'s gated engine
    /// path for the compare flip. The live zoom/pan carries across the flip when both
    /// photos share dimensions and rotation (the 100%-crop sharpness workflow: the
    /// same crop of the other frame lands under your gaze).
    fn compare_jump(&mut self, item: usize) {
        self.flush_pending_delete();
        // Never jump while the previous target is still pending (a miss in flight) —
        // mirroring `advance`, so a photo is never silently skipped.
        if self.displayed_item != self.target_item {
            return;
        }
        // Stage the carried zoom/pan for `view_for` to consume, so the flip's FIRST
        // presented frame already has the view — one set_view + one draw. (The first
        // cut presented at the reset view and re-imposed the carry afterwards: two
        // draws, and the incoming photo flashed centered for a frame.)
        self.compare_carry = self.compare_carry_view(item);
        self.stop_playback();
        self.playlist.jump_to(item);
        self.target_item = self.playlist.current();
        self.try_present_target();
        // Not consumed (a ring miss / failed target): drop it rather than let some
        // later unrelated present inherit a stale view.
        self.compare_carry = None;
        self.request_prefetch();
    }

    /// The live zoom/pan to carry across a flip to `to`, or `None` when the view is
    /// the default or the two photos don't share geometry (same pixel dimensions AND
    /// the same rotation override — otherwise the crop wouldn't map anyway).
    fn compare_carry_view(&self, to: usize) -> Option<(f32, [f32; 2])> {
        if self.view.zoom == 1.0 && self.view.pan == [0.0, 0.0] {
            return None; // default view — nothing worth carrying
        }
        let from = self.displayed_item?;
        let a = self.meta_cache.get(&from)?;
        let b = self.meta_cache.get(&to)?;
        let rot_a = self.rotations.get(&from).copied().unwrap_or_default();
        let rot_b = self.rotations.get(&to).copied().unwrap_or_default();
        ((a.w, a.h) == (b.w, b.h) && rot_a == rot_b).then_some((self.view.zoom, self.view.pan))
    }

    /// Reflect the pan affordance in the pointer: a pointing hand over the folder-tree,
    /// a closed hand while dragging, an open hand when the image is pannable, the default arrow
    /// otherwise.
    pub fn refresh_cursor(&mut self) {
        let over_button = self
            .last_cursor
            .is_some_and(|[x, y]| self.folder_tree_hit(x, y).is_some());
        let kind = if self.dragging {
            contract::CursorKind::Grabbing
        } else if over_button {
            contract::CursorKind::Pointer
        } else if self.pannable() {
            contract::CursorKind::Grab
        } else {
            contract::CursorKind::Default
        };
        self.effects.push(contract::CoreEffect::SetCursor(kind));
    }

    /// Zoom by `factor` (>1 in, <1 out) about the cursor — the shared effect for
    /// trackpad pinch and mouse-wheel zoom. Anchors on the last cursor position,
    /// falling back to the screen center before the pointer has moved.
    pub fn zoom_about_cursor(&mut self, factor: f32) {
        let Some((iw, ih, sw, sh)) = self.screen_and_image() else {
            return;
        };
        let anchor = self
            .last_cursor
            .unwrap_or([sw as f32 / 2.0, sh as f32 / 2.0]);
        self.view.zoom_about(factor, anchor, iw, ih, sw, sh);
        self.push_view();
        self.draw();
        // Zooming changes whether the image overflows — update the grab affordance
        // immediately (the pointer may not move after a wheel notch / pinch).
        self.refresh_cursor();
    }

    /// Flash the "▶ Press P to play" hint once when settling on an animated still —
    /// suppressed while flying (the nag the owner flagged) and once the user has engaged
    /// (P / step, tracked via `anim_hint_shown_for`). An eager prep decoding in the
    /// background does *not* suppress it — that's invisible work, and the hint is what
    /// invites the user to press P in the first place.
    pub fn maybe_show_anim_hint(&mut self, flying: bool) {
        if flying || self.playback.is_some() {
            return;
        }
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.anim_hint_shown_for == Some(item) {
            return;
        }
        if self.has_motion(item) {
            self.anim_hint_shown_for = Some(item);
            // Both shells present the play hint natively (the winit egui overlay / the macOS
            // SwiftUI pill): flash-signal it (bump the seq); the shell renders + fades the pill
            // and reads `play_hint_kind` for the icon. No HUD raster / colliders / cursor.
            self.play_hint_seq = self.play_hint_seq.wrapping_add(1);
            self.draw(); // wake the shell so it reads the new seq
        }
    }

    /// `P`: play/pause the current animation. Uses the eagerly-prepped sequence for
    /// instant playback when it's ready; otherwise (upgrading an in-flight eager prep,
    /// or kicking a fresh decode) it starts playing the moment frames land. On a still,
    /// `P` does nothing.
    pub fn toggle_play_pause(&mut self) {
        if self.playback.is_some() {
            // Was it parked at the end of a finite loop? Then toggling *restarts* from
            // frame 0 (so the audio must restart too, not resume mid-track).
            let was_finished = self.playback.as_ref().unwrap().is_finished();
            let playing = self.playback.as_mut().unwrap().toggle_play();
            if playing {
                // (Re)started — present the current frame (frame 0 when replaying a
                // finished loop, so the stale last frame doesn't linger) + anchor timing.
                self.present_anim_frame();
                if was_finished {
                    if let Some(item) = self.displayed_item {
                        self.start_live_audio(item); // replay from the top
                    }
                } else {
                    self.effects.push(contract::CoreEffect::ResumeLiveAudio);
                }
            } else {
                self.draw(); // paused — just redraw the held frame
                self.effects.push(contract::CoreEffect::PauseLiveAudio);
            }
            return;
        }
        let Some(item) = self.displayed_item else {
            return;
        };
        // Eagerly prepared on dwell → play instantly (no decode wait).
        if self.prepared.as_ref().is_some_and(|p| p.item == item) {
            let anim = self.prepared.take().unwrap().anim;
            self.anim_hint_shown_for = Some(item); // engaged
            self.install_animation(anim, true, 0);
            self.start_live_audio(item);
            return;
        }
        // An eager prep is already decoding → upgrade it to play on arrival.
        if let Some(d) = self.anim_decode.as_mut() {
            d.want = AnimWant::Play;
            self.anim_hint_shown_for = Some(item);
            return;
        }
        if self.has_motion(item) {
            self.start_animation_decode(item, AnimWant::Play);
        }
    }

    /// Step the current animation one frame (`delta`: `+1` next, `-1` previous),
    /// pausing playback. Uses the eager prep when ready; otherwise upgrades an in-flight
    /// prep (or kicks one) so the held-key scrub steps once frames land. No-op on a still.
    pub fn frame_step(&mut self, delta: i32) {
        // Scrubbing is not continuous playback — silence any Live Photo audio.
        self.effects.push(contract::CoreEffect::StopLiveAudio);
        if self.playback.is_some() {
            self.playback.as_mut().unwrap().step(delta);
            self.present_anim_frame();
            return;
        }
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.prepared.as_ref().is_some_and(|p| p.item == item) {
            let anim = self.prepared.take().unwrap().anim;
            self.anim_hint_shown_for = Some(item);
            self.install_animation(anim, false, delta); // paused, stepped
            return;
        }
        if let Some(d) = self.anim_decode.as_mut() {
            d.want = AnimWant::Step(delta);
            self.anim_hint_shown_for = Some(item);
            return;
        }
        if self.has_motion(item) {
            self.start_animation_decode(item, AnimWant::Step(delta));
        }
    }

    /// Keyboard frame-step press: track the key for hold-to-scrub, then step once now.
    pub fn frame_step_press(&mut self, key: PbKey, action: Action) {
        self.held.insert(key, action);
        let now = self.now;
        self.framestep_started = Some(now);
        self.framestep_last = Some(now);
        self.frame_step(frame_step_dir(action));
    }

    /// Pick up a finished off-thread animation decode (called each `about_to_wait`).
    /// Discards a stale result (superseded request, geometry change, or the user
    /// navigated away) and otherwise installs the [`Playback`] and shows frame 0.
    pub fn poll_anim_decode(&mut self) {
        use std::sync::mpsc::TryRecvError;
        // Receive (and copy out what we need) in a scope so the `anim_decode` borrow
        // ends before we mutate it / install the playback.
        let outcome = {
            let Some(d) = self.anim_decode.as_ref() else {
                return;
            };
            match d.rx.try_recv() {
                Ok(result) => Some((d.gen, d.epoch, d.item, d.want, result)),
                Err(TryRecvError::Empty) => return, // still decoding
                Err(TryRecvError::Disconnected) => None, // worker died
            }
        };
        self.anim_decode = None;
        let Some((gen, epoch, item, want, result)) = outcome else {
            return;
        };
        // Stale: a newer request superseded it, the fit changed, or we moved on.
        if gen != self.anim_gen || epoch != self.epoch || self.displayed_item != Some(item) {
            return;
        }
        match result {
            Ok(anim) => match want {
                // Eager prep: hold it ready; the still keeps showing (frame 0 == still),
                // so there's no visible change — `P` will play it instantly. If the
                // detailed panel is open, refresh it so the frame count/rate/loop appear.
                AnimWant::Eager => {
                    self.prepared = Some(Prepared { item, anim });
                    if self.overlay_shown && self.slot_content() == Some(SlotContent::Details) {
                        self.show_overlay();
                    }
                }
                AnimWant::Play => {
                    self.install_animation(anim, true, 0);
                    self.start_live_audio(item); // in sync with the first frame
                }
                AnimWant::Step(delta) => self.install_animation(anim, false, delta),
            },
            Err(e) => {
                // An eager prep that fails stays silent (the user never asked); only a
                // user-initiated P/step surfaces the error.
                eprintln!("animation decode failed for item {item}: {e}");
                if want != AnimWant::Eager {
                    self.show_toast("Can't play this animation");
                }
            }
        }
    }

    /// Stop and drop any playback / in-flight decode / eager prep, reverting to the
    /// still. Called when navigating away or changing source (the frames are RAM-only —
    /// privacy #2).
    pub fn stop_playback(&mut self) {
        self.playback = None;
        self.anim_frame_shown_at = None;
        self.cancel_anim_decode(); // stop an in-flight decode, don't just orphan it
        self.prepared = None;
        self.framestep_started = None;
        self.framestep_last = None;
        self.live_revert_at = None;
        self.effects.push(contract::CoreEffect::StopLiveAudio); // dropping the player stops it
    }

    /// Start the Live Photo's audio from the top (its `.mov` track), if `item` is a Live
    /// Photo with audio and audio isn't muted — the "cheap path" (task #38). A no-op for
    /// an animation (no audio track), a silent clip, or when muted. Called when the motion
    /// starts playing from frame 0.
    pub fn start_live_audio(&mut self, item: usize) {
        if self.settings.mute_live_audio {
            self.effects.push(contract::CoreEffect::StopLiveAudio);
            return;
        }
        // The core decides the motion path; the shell owns the ObjC player (drained effect).
        // No companion motion → clear any existing audio (mirrors the old `and_then` → None).
        match self.live_motion_path(item) {
            Some(path) => self
                .effects
                .push(contract::CoreEffect::StartLiveAudio { path, at_secs: 0.0 }),
            None => self.effects.push(contract::CoreEffect::StopLiveAudio),
        }
    }

    /// `i` (the basic one-line info readout) or `Shift+I` (the Inspector's Details
    /// tab). Independent (task #54): opening/closing one never touches the other —
    /// the two can be on at once, the line sitting below the panel. When shown the
    /// line appears immediately (idle); after navigation it reappears once you stop
    /// (see the tick). `Tab`-hidden is the one thing they share (it's a single master
    /// switch): `i` while hidden follows the same reveal rule as `Shift+I`/Help/tree —
    /// reveal everything first, and only ever end up *shown*, never toggled off with
    /// nothing visibly changing.
    pub fn toggle_info(&mut self, full: bool) {
        if full {
            self.panels.toggle_inspector(InspectorTab::Details);
            self.refresh_slot();
        } else if self.panels.reveal() {
            self.info_line = true;
            self.refresh_tree_visibility();
            self.refresh_slot();
        } else {
            self.info_line = !self.info_line;
            if self.info_line {
                self.show_info_line();
            } else {
                self.hide_info_line();
            }
        }
    }

    /// The **resolved** dark/light flag (task #46): the `Appearance` preference against
    /// the live OS theme — `System` follows [`os_dark`](AppCore::os_dark) (kept current
    /// by the shell's `OsThemeChanged` reports); `Light`/`Dark` pin it.
    pub fn effective_dark(&self) -> bool {
        match self.settings.appearance_mode {
            settings::AppearanceMode::System => self.os_dark,
            settings::AppearanceMode::Light => false,
            settings::AppearanceMode::Dark => true,
        }
    }

    /// The letterbox / background fill for the resolved theme (task #46) — what the
    /// shells hand `Renderer::set_letterbox` when the renderer first stands up.
    pub fn effective_letterbox(&self) -> [u8; 3] {
        self.settings.letterbox_for(self.effective_dark())
    }

    /// Re-apply the resolved appearance (task #46): retint the HUD's color scheme, set
    /// the effective letterbox fill, and — only when the resolved dark/light actually
    /// flipped — rebuild every visible overlay bitmap (they were composited with the
    /// old scheme) through the same invalidation a DPI change uses. Runs on
    /// `OsThemeChanged`, inside [`apply_settings`](Self::apply_settings), and from the
    /// shells right after the renderer stands up.
    pub fn refresh_theme(&mut self) {
        let dark = self.effective_dark();
        if let Some(r) = self.renderer.as_mut() {
            r.set_letterbox(self.settings.letterbox_for(dark));
        }
        if let Some(h) = self.hud.as_mut() {
            h.set_theme(hud::Theme::of(dark));
        }
        if dark != self.hud_dark {
            self.hud_dark = dark;
            // Pie / scan card / info panel / tree / open hint all re-rasterize with the
            // new scheme. A plain transient toast keeps its old-scheme bitmap (it fades
            // out within ~1.3 s and its source content isn't retained).
            self.rescale_overlays();
        }
    }

    /// Spike (task #59): forward the translucent-top-bar height (physical px) to the renderer
    /// so the photo fits below the bar while zoom/fill overflow shows under the glass. Shell-
    /// driven (the macOS glass-toolbar mode sets it on attach + resize); `0` = classic opaque
    /// bar. A no-op before the renderer stands up.
    pub fn set_content_top_inset(&mut self, px: u32) {
        if let Some(r) = self.renderer.as_mut() {
            r.set_content_top_inset(px);
        }
    }

    /// Apply the settings the user saved in the dialog: swap in the new model, apply
    /// the parts that aren't read live (hold delay, appearance + letterbox color,
    /// default scale mode), then persist to disk (an explicit user action — privacy
    /// #2). The nav-feel rates (start speed / ramp / max) and the info-panel opacity
    /// are read live, so swapping `self.settings` is enough for those.
    pub fn apply_settings(&mut self, new: settings::Settings) {
        let old = std::mem::replace(&mut self.settings, new);

        // Held-key repeat delay is cached on the struct (the curve below reads the
        // rates live, but this one is a Duration captured at construction).
        self.initial_delay = Duration::from_millis(self.settings.hold_delay_ms as u64);

        // Default slideshow interval → the live timer. A running slideshow's deadline is
        // `last_present + interval`, recomputed each tick, so this takes effect at once
        // (the `[`/`]` live override is just a different write to the same field).
        self.slideshow.interval = Duration::from_secs_f64(self.settings.slideshow_interval_secs);

        // Appearance + letterbox / background fill → HUD scheme + renderer (task #46);
        // rebuilds the overlay bitmaps when the resolved theme flipped.
        self.refresh_theme();

        // Default scale mode: apply live if it changed (re-frames + reloads at the new
        // fit). `set_scale_mode` redraws for us.
        let scale_changed = old.scale_mode != self.settings.scale_mode;
        if scale_changed {
            self.set_scale_mode(scale_mode_of(self.settings.scale_mode));
        }

        // Persist the whole model (atomic write; best-effort).
        self.settings.save();

        // A new info-line alignment (or opacity/theme) re-places the line at once, which
        // re-lifts/re-caps its colliders (panel + tree) for the new span.
        if old.info_line_align != self.settings.info_line_align && self.info_line_shown {
            self.show_info_line();
        }

        // The "show image info" default also applies live — flipping it in Settings shows or
        // hides the current line, not just the next launch.
        if old.show_image_info != self.settings.show_image_info {
            self.info_line = self.settings.show_image_info;
        }
        // The field toggles (filename / resolution / codec) are read live by info_line_*(), so
        // if the line is up, re-place it to reflect the new content — or hide it if the change
        // left no fields on (info_line_visible now returns false).
        let fields_changed = old.info_show_folder != self.settings.info_show_folder
            || old.info_show_filename != self.settings.info_show_filename
            || old.info_show_resolution != self.settings.info_show_resolution
            || old.info_show_codec != self.settings.info_show_codec;
        if self.info_line
            && (fields_changed || old.show_image_info != self.settings.show_image_info)
        {
            if self.info_line_visible() {
                self.show_info_line();
            } else {
                self.hide_info_line();
            }
        } else if !self.info_line && old.show_image_info != self.settings.show_image_info {
            self.hide_info_line();
        }

        // Redraw so the new letterbox shows even when the scale mode didn't change,
        // and rebuild the info panel so a new opacity takes effect immediately.
        if self.overlay_shown {
            self.show_overlay();
        } else if !scale_changed {
            self.draw();
        }

        // Re-pull the native panels: their opacity (and theme) come from settings but aren't
        // in the panel snapshot, so without this a *natively* presented tree/inspector (the
        // egui overlay on winit; the SwiftUI panels on mac) wouldn't pick up a Panel-opacity
        // change until the next unrelated repaint.
        self.emit_panels_changed();
    }

    /// The keybindings help table: a title row, then every hotkey → action as a
    /// shaded-key / description pair. The key labels are read from the live keymap
    /// (task #8 — single source of truth), so rebinding a key updates the help. A
    /// few rows stay curated: pan (shown as arrow glyphs), help (`/ or ?`), and the
    /// "hold to fly" hint (no single binding).
    /// The user-facing shortcut hint for an action, formatted for this platform: on macOS the
    /// menu's ⌘-accelerator ([`menu::macos_menu_chord`]) where one exists — so Copy shows ⌘C and
    /// Move to Trash shows ⌘⌫, matching the menu bar rather than the keymap's legacy binding —
    /// else the primary keymap binding as Mac symbols; on Windows/Linux the spelled-out primary
    /// binding. Empty when unbound.
    pub fn help_shortcut(&self, action: Action) -> String {
        #[cfg(target_os = "macos")]
        if let Some(chord) = crate::keymap::macos_menu_chord(action) {
            return chord.mac_symbol();
        }
        self.keymap_shortcut(action)
    }

    /// The primary *keymap* binding for an action (numpad alternates skipped), bypassing
    /// [`help_shortcut`](Self::help_shortcut)'s menu-accelerator preference. For the help
    /// rows that teach the viewer's bare-key habits — Open O / ⇧O, Quit Esc — where the
    /// ⌘-chord is the one every Mac user already knows (owner call, 2026-07-03).
    pub fn keymap_shortcut(&self, action: Action) -> String {
        self.keymap
            .bindings_for(action)
            .iter()
            .find(|c| !c.code.is_numpad())
            .map(|c| c.shortcut_label())
            .unwrap_or_default()
    }

    /// The Help panel model (task #54): grouped sections (description + shortcut),
    /// sourced from the live keymap / menu so customized bindings and platform
    /// symbols stay correct. The HUD projects it via `render_shortcuts`; presenters
    /// consume it directly.
    pub fn help_panel(&self) -> HelpPanel {
        let sc = |a: Action| self.help_shortcut(a);
        let two =
            |a: Action, b: Action| format!("{} / {}", self.help_shortcut(a), self.help_shortcut(b));
        let row = |desc: &str, shortcut: String| (desc.to_string(), shortcut);
        // Platform wording for the trash action (the shortcut itself comes from `help_shortcut`).
        #[cfg(target_os = "macos")]
        let trash = "Move to Trash";
        #[cfg(not(target_os = "macos"))]
        let trash = "Delete to Recycle Bin";

        let section = |title: &str, rows: Vec<(String, String)>| HelpSection {
            title: title.to_string(),
            rows,
        };
        let sections = vec![
            section(
                "Browse",
                vec![
                    row("Next image", sc(Action::Next)),
                    row("Previous image", sc(Action::Prev)),
                    row("Random image", sc(Action::Random)),
                    row("Previous random", sc(Action::RandomPrev)),
                    row("Slideshow", sc(Action::SlideshowToggle)),
                    row(
                        "Slideshow faster / slower",
                        two(Action::SlideshowFaster, Action::SlideshowSlower),
                    ),
                ],
            ),
            section(
                "View & Zoom",
                vec![
                    row("Fit to screen", sc(Action::ScaleFit)),
                    row("Crop to fill", sc(Action::ScaleFill)),
                    row("Toggle 1:1 and fit", sc(Action::ToggleOriginal)),
                    row("Zoom out / in", two(Action::ZoomOut, Action::ZoomIn)),
                    row("Pan", "\u{2190} \u{2191} \u{2193} \u{2192}".to_string()),
                    row(
                        "Rotate right / left",
                        two(Action::RotateCw, Action::RotateCcw),
                    ),
                    row(
                        "Flip / pin compare",
                        two(Action::CompareToggle, Action::ComparePin),
                    ),
                    row("Quick Full Screen", sc(Action::Fullscreen)),
                ],
            ),
            section(
                "Animation",
                vec![
                    row("Play / pause", sc(Action::PlayPause)),
                    row(
                        "Previous / next frame",
                        two(Action::FramePrev, Action::FrameNext),
                    ),
                    row("Mute Live Photo audio", sc(Action::MuteLiveAudio)),
                ],
            ),
            section(
                "Files & App",
                vec![
                    // Open and Quit show the *keymap* keys (O / ⇧O / Esc), not the menu's
                    // ⌘-chords `sc` would prefer — the bare keys are the ones this help
                    // exists to teach; every Mac user already knows ⌘O and ⌘Q.
                    row("Open file", self.keymap_shortcut(Action::OpenFile)),
                    row("Open folder", self.keymap_shortcut(Action::OpenFolder)),
                    row("Copy image", sc(Action::Copy)),
                    row("Copy file path", sc(Action::CopyPath)),
                    row("Save rotation", sc(Action::SaveRotation)),
                    row("Undo", sc(Action::Undo)),
                    row(trash, sc(Action::Delete)),
                    row("Delete permanently", sc(Action::DeletePermanent)),
                    row("Recursive (this folder)", sc(Action::Recursive)),
                    row("Info panel", sc(Action::Info)),
                    row("Detailed info panel", sc(Action::FullExif)),
                    row("Text in image", sc(Action::ShowImageText)),
                    row("Folder tree", sc(Action::FolderTree)),
                    row("Hide/show panels", sc(Action::TogglePanels)),
                    row("Parent folder", sc(Action::OpenParent)),
                    row(
                        "Previous / next folder",
                        two(Action::PrevFolder, Action::NextFolder),
                    ),
                    row("Settings", sc(Action::Settings)),
                    row("Quit", self.keymap_shortcut(Action::Quit)),
                    // Curated: the two real keys are "/" and "?" — `two()` would render
                    // the ⇧/ chord, and the keymap's names can't say "?" (the renderer
                    // dims only the spaced " / " separator, so the "/" key stays bright).
                    row("Help", "/ / ?".to_string()),
                ],
            ),
        ];
        HelpPanel { sections }
    }

    /// What the single **rich-panel** overlay slot shows right now, priority-resolved:
    /// Help > the Inspector's active tab. `None` = no rich panel (everything closed or
    /// `Tab`-hidden). The basic `i` line is a separate layer — see `info_line`.
    pub fn slot_content(&self) -> Option<SlotContent> {
        use crate::overlay::PanelContent;
        match self.panels.content() {
            Some(PanelContent::Help) => Some(SlotContent::Help),
            Some(PanelContent::Tab(InspectorTab::Details)) => Some(SlotContent::Details),
            Some(PanelContent::Tab(InspectorTab::Text)) => Some(SlotContent::Text),
            Some(PanelContent::Tab(InspectorTab::Describe)) => Some(SlotContent::Describe),
            None => None,
        }
    }

    /// Whether the current overlay-slot content is presented **natively** by the host
    /// (so the core suppresses its HUD rasterization): Help when `native_help`, and any
    /// Inspector tab when `native_inspector`. The tree joins as it goes native.
    fn slot_is_native(&self) -> bool {
        match self.slot_content() {
            Some(SlotContent::Help) => self.native_help,
            Some(SlotContent::Details | SlotContent::Text | SlotContent::Describe) => {
                self.native_inspector
            }
            None => false,
        }
    }

    /// Whether the **native** Inspector panel should be visible right now — the signal the
    /// mac host reads (via FFI) to show/hide its SwiftUI Inspector: the Inspector is open
    /// on some tab, not `Tab`-hidden, and the host presents it natively.
    pub fn inspector_panel_visible(&self) -> bool {
        self.native_inspector && self.panels.inspector.is_some() && !self.panels.hidden
    }

    /// A content snapshot of the Inspector's active tab (task #54) — for the tick's
    /// change diff and (indirectly) the host's re-pull. Details when the Inspector is
    /// closed (only read while visible, i.e. on some tab).
    pub fn inspector_snapshot(&self) -> crate::panels::InspectorSnapshot {
        use crate::panels::InspectorSnapshot;
        match self.panels.inspector {
            Some(InspectorTab::Text) => InspectorSnapshot::Text(self.text_panel()),
            Some(InspectorTab::Describe) => InspectorSnapshot::Describe(self.describe_panel()),
            _ => InspectorSnapshot::Details(self.details_panel()),
        }
    }

    /// Whether the **native** Help panel should be visible right now — the signal the
    /// mac host reads (via FFI) to show/hide its SwiftUI Help view. Help open, not
    /// `Tab`-hidden, and the host presents it natively.
    pub fn help_panel_visible(&self) -> bool {
        self.native_help && self.panels.help && !self.panels.hidden
    }

    /// Whether the **native** empty-state Open panel should be visible — the welcome
    /// surface shown when no photos are loaded (and no scan is bootstrapping). The host
    /// reads this to show/hide its native view, gated on the native flag.
    pub fn open_panel_visible(&self) -> bool {
        self.native_open && self.source.is_empty() && !self.scanning && !self.launching
    }

    /// Push the [`CoreEffect::PanelsChanged`] marker so the host re-pulls the native
    /// panel model — deduped (the drain can pull once for several mutations in a tick).
    fn emit_panels_changed(&mut self) {
        if !self
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::PanelsChanged))
        {
            self.effects.push(contract::CoreEffect::PanelsChanged);
        }
    }

    /// The Inspector ▸ Text tab model (task #54): the semantic scan state for the
    /// displayed photo, read from the RAM-only caches. Pure projection — building
    /// it kicks nothing off (the show path calls `ensure_text_scan` separately).
    pub fn text_panel(&self) -> TextPanel {
        let body = match self.displayed_item {
            None => TextBody::NoPhoto,
            Some(item) => match self.recognized_text.get(&item) {
                Some(r) => TextBody::Ready {
                    qr: r.qr.clone(),
                    paragraphs: r.lines.clone(),
                    ocr_error: r.ocr_error.clone(),
                },
                None => TextBody::Scanning,
            },
        };
        TextPanel { body }
    }

    /// The Inspector ▸ Describe tab model (task #54): the semantic describe state
    /// for the displayed photo. Pure projection of the RAM-only caches.
    pub fn describe_panel(&self) -> DescribePanel {
        let body = match self.displayed_item {
            None => DescribeBody::NoPhoto,
            Some(item) => match self.descriptions.get(&item) {
                Some(Ok(text)) => DescribeBody::Ready(text.clone()),
                Some(Err(msg)) => DescribeBody::Error(msg.clone()),
                None if self.describe_scan.as_ref().is_some_and(|s| s.item == item) => {
                    DescribeBody::Busy
                }
                None => DescribeBody::Idle,
            },
        };
        DescribePanel { body }
    }

    /// The Inspector ▸ Details tab model (task #54): the full metadata table.
    pub fn details_panel(&self) -> DetailsPanel {
        DetailsPanel {
            rows: self.exif_rows(),
        }
    }

    /// Rasterize the active **rich panel** (Inspector tab or Help) and draw it,
    /// lifted above the info-line strip if that line shares the corner. The help
    /// overlay uses a larger font than the info panels. The basic `i` line is drawn
    /// separately by [`show_info_line`](Self::show_info_line).
    pub fn show_overlay(&mut self) {
        // A natively-presented panel (Help on the mac host) is drawn by the shell, not
        // the HUD — suppress the CPU rasterization entirely. Clear any HUD panel left
        // from a previous slot (e.g. switching Details → Help) so it doesn't linger
        // under the native view; the tick's visibility diff emits the marker.
        if self.slot_is_native() {
            if self.overlay_shown {
                self.hide_overlay();
            }
            return;
        }
        let px = (15.0 * self.viewport.scale_factor).max(8.0);
        let pad = (7.0 * self.viewport.scale_factor).round().max(2.0) as u32;
        // The info / EXIF panels honor the user's opacity setting; the help overlay
        // keeps the standard translucency. Both take the active theme's panel color.
        let theme = self.hud.as_ref().map_or(hud::Theme::DARK, |h| h.theme());
        let info_bg = theme.bg_for_opacity(self.settings.info_opacity);
        // Resolve the Live Photo pairing (cached; one stat) so the detailed table can
        // label it.
        if let Some(item) = self.displayed_item {
            self.live_motion_path(item);
        }
        // Cap the paragraph panels to a readable column, never wider than the window
        // allows at this margin.
        let margin = self.overlay_margin();
        let para_max_w = ((self.viewport.width as i32 - 2 * margin as i32).max(1) as u32)
            .min((440.0 * self.viewport.scale_factor) as u32);
        let max_h = (self.viewport.height as i32 - 2 * margin as i32).max(1);
        let panel = match self.slot_content() {
            None => return,
            Some(SlotContent::Details) => {
                // Warm the EXIF read once so the table build (and its per-frame rebuilds
                // during playback) never re-read the file.
                if let Some(item) = self.displayed_item {
                    self.ensure_exif_cached(item);
                }
                let model = self.details_panel();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                if model.rows.is_empty() {
                    return;
                }
                // Interim HUD projection: core rows → the rasterizer's table rows.
                let rows: Vec<Row> = model.rows.into_iter().map(hud_row).collect();
                hud.render_table(&rows, px, pad, info_bg)
            }
            Some(SlotContent::Help) => {
                let help_px = (15.0 * self.viewport.scale_factor).max(10.0);
                let model = self.help_panel();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                let sections: Vec<hud::ShortcutSection> = model
                    .sections
                    .into_iter()
                    .map(|s| hud::ShortcutSection {
                        title: s.title,
                        rows: s.rows,
                    })
                    .collect();
                hud.render_shortcuts(&sections, help_px, theme.bg, max_h)
            }
            Some(SlotContent::Text) => {
                if self.current.is_none() {
                    return;
                }
                // The panel tracks the displayed photo while open, so settling on a
                // new item kicks its scan here (no-op when cached / already running).
                self.ensure_text_scan();
                let lines = self.text_panel().lines();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                hud.render_paragraph(&lines, px, pad, info_bg, para_max_w, max_h)
            }
            Some(SlotContent::Describe) => {
                if self.current.is_none() {
                    return;
                }
                // Auto-describe (opt-in, `describe_auto`): only while the panel is already
                // open (this arm), so it's never a passive background send — the user chose
                // to be looking at descriptions. Off by default for privacy + token cost; on,
                // settling on a new photo describes it without another `D`. (This is a settle
                // path, not a per-frame one, so hold-to-fly doesn't machine-gun the backend.)
                if self.settings.describe_auto {
                    self.ensure_describe_scan(None);
                }
                let lines = self.describe_panel().lines();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                hud.render_paragraph(&lines, px, pad, info_bg, para_max_w, max_h)
            }
        };
        let Some((bitmap, w, h)) = panel else {
            return;
        };
        // Lift the panel above the info line only when the line actually overlaps this
        // panel's horizontal span (task #54). The rich panel is bottom-right-anchored:
        // its span is `[sw - margin - w, sw - margin]`. Right inset stays `margin`.
        let sw = self.viewport.width as f32;
        let px1 = sw - margin as f32;
        let bottom = margin + self.info_line_reserve_for(px1 - w as f32, px1);
        if let Some(a) = self.renderer.as_mut() {
            a.set_overlay(Some((&bitmap, w, h)), margin, bottom);
        }
        self.overlay_shown = true;
        self.overlay_item = self.displayed_item;
        self.draw();
    }

    /// The info line's horizontal span `[x0, x1]` in physical px when it's drawn,
    /// from its alignment + rasterized width + the corner margin. `None` when the
    /// line isn't shown. The core-owned footprint every colliding layer reserves
    /// against (and, later, what the native presenters inset their layout by).
    fn info_line_span(&self) -> Option<(f32, f32)> {
        if !self.info_line_shown || self.info_line_w == 0 {
            return None;
        }
        let sw = self.viewport.width as f32;
        let w = self.info_line_w as f32;
        let m = self.overlay_margin() as f32;
        let x0 = match self.settings.info_line_align {
            settings::InfoLineAlign::Left => m,
            settings::InfoLineAlign::Center => ((sw - w) * 0.5).max(0.0),
            settings::InfoLineAlign::Right => (sw - m - w).max(0.0),
        };
        Some((x0, x0 + w))
    }

    /// The vertical strip (line height + gap) a layer must yield to clear the info
    /// line **iff** its horizontal `[px0, px1]` span overlaps the line's — so a panel
    /// on the opposite side reserves nothing, but a wide centered line that spans the
    /// whole width pushes both corner panels *and* the toast. `0` when there's no
    /// overlap or the line is hidden.
    fn info_line_reserve_for(&self, px0: f32, px1: f32) -> u32 {
        let Some((lx0, lx1)) = self.info_line_span() else {
            return 0;
        };
        // A small gap so touching (not overlapping) edges don't trigger a reserve.
        let gap = 6.0 * self.viewport.scale_factor;
        if px0 < lx1 + gap && lx0 < px1 + gap {
            self.info_line_h + gap.round() as u32
        } else {
            0
        }
    }

    /// The info line's alignment as the renderer's [`pb_render::HAlign`].
    fn info_line_halign(&self) -> pb_render::HAlign {
        match self.settings.info_line_align {
            settings::InfoLineAlign::Left => pb_render::HAlign::Left,
            settings::InfoLineAlign::Center => pb_render::HAlign::Center,
            settings::InfoLineAlign::Right => pb_render::HAlign::Right,
        }
    }

    /// Rasterize + upload the basic info line (`i`) into its own bottom-anchored layer
    /// at the configured alignment, then re-place any colliding panel/tree/toast above
    /// it. A no-op without a font/photo (the tick retries on settle). Mirrors
    /// [`show_overlay`](Self::show_overlay) but for the ephemeral line.
    /// The full one-line readout — `rel · W×H · CODEC[· Live]`, each field gated by its
    /// Settings toggle — or `None` with no photo. The HUD (winit) rasterizes this whole string;
    /// the native shell instead reads `info_line_main` + `info_line_codec` (codec as a pill).
    pub fn info_line_content(&self) -> Option<String> {
        let meta = self.current.as_ref()?;
        let mut parts = self.info_line_parts(meta);
        if self.settings.info_show_codec {
            parts.push(meta.codec.to_string());
        }
        if self.displayed_item.is_some_and(|i| self.is_live_photo(i)) {
            parts.push("Live".to_string()); // a Live Photo's motion is playable (P)
        }
        Some(parts.join(" · "))
    }

    /// The name (folder / filename) and resolution fields, each gated by its Settings toggle
    /// (shared by the full HUD string and the native main text). Folder is prepended to the
    /// filename with a `/` — the relative dir when the scan is recursive, else the containing
    /// folder's name.
    fn info_line_parts(&self, meta: &crate::meta::PhotoMeta) -> Vec<String> {
        let mut parts = Vec::new();
        // `rel` is the path relative to the scan root, so split its directory (nested scans)
        // from the file name.
        let (dir, file) = match meta.rel.rsplit_once('/') {
            Some((d, f)) => (Some(d.to_string()), f),
            None => (None, meta.rel.as_str()),
        };
        let name = match (
            self.settings.info_show_folder,
            self.settings.info_show_filename,
        ) {
            (true, true) => match dir.or_else(|| self.info_folder_name()) {
                Some(f) if !f.is_empty() => format!("{f}/{file}"),
                _ => file.to_string(),
            },
            (false, true) => file.to_string(),
            (true, false) => dir.or_else(|| self.info_folder_name()).unwrap_or_default(),
            (false, false) => String::new(),
        };
        if !name.is_empty() {
            parts.push(name);
        }
        if self.settings.info_show_resolution {
            parts.push(format!("{}×{}", meta.w, meta.h));
        }
        parts
    }

    /// The immediate containing folder's name — used for the Folder field when `rel` has no
    /// directory of its own (a flat, non-recursive scan). `None` for a source without a real
    /// path on disk (an archive entry).
    fn info_folder_name(&self) -> Option<String> {
        let item = self.displayed_item?;
        let path = self.source.path(item)?;
        path.parent()?
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }

    /// Whether the info readout should show: toggled on (`i`), not suppressed by
    /// `Tab`, and the enabled fields produce some text — an empty pill (all fields
    /// off, or a folder-only field that can't resolve) reads as a bug, so it hides
    /// instead. **The native macOS shell polls this directly** (`CoreModel.swift`)
    /// as its actual show/hide gate — it does not consult `info_line_shown` (that's
    /// the winit HUD rasterizer's own drawn-state bookkeeping) — so `panels.hidden`
    /// belongs here, not just in the HUD-side `refresh_info_line_visibility`.
    pub fn info_line_visible(&self) -> bool {
        self.info_line
            && !self.panels.hidden
            && self.info_line_content().is_some_and(|s| !s.is_empty())
    }

    /// The info readout's main text (native shell) — `rel · W×H[· Live]`, codec split out so the
    /// shell can pill it separately (like the folder-tree count badges). Each field is gated.
    pub fn info_line_main(&self) -> Option<String> {
        let meta = self.current.as_ref()?;
        // Note: no "Live" here — the native shell shows the livephoto *symbol* by the codec
        // (`info_line_is_live`) instead of the word. The HUD string (`info_line_content`) keeps
        // the text, since it can't draw a symbol.
        Some(self.info_line_parts(meta).join(" · "))
    }

    /// Whether the current photo is a Live Photo — the native shell renders the livephoto mark
    /// beside the codec in place of the "Live" text.
    pub fn info_line_is_live(&self) -> bool {
        self.current.is_some() && self.displayed_item.is_some_and(|i| self.is_live_photo(i))
    }

    /// Whether the current photo is an animated image (GIF / APNG / animated WebP / AVIF / …) —
    /// the native shell shows a motion mark by the codec. Distinct from a Live Photo, which has
    /// its own mark (`info_line_is_live`).
    pub fn info_line_is_animated(&self) -> bool {
        !self.info_line_is_live() && self.current.as_ref().is_some_and(|m| m.animated.is_some())
    }

    /// The current photo's codec label (e.g. `JPEG`) for the info readout's pill — empty when
    /// the codec field is toggled off (so the shell omits the badge).
    pub fn info_line_codec(&self) -> String {
        if !self.settings.info_show_codec {
            return String::new();
        }
        self.current
            .as_ref()
            .map(|m| m.codec.to_string())
            .unwrap_or_default()
    }

    /// A change-detection snapshot of the natively-drawn info readout — `(main, codec, live,
    /// animated)` when visible, `None` when hidden. The tick diffs it so a native info line
    /// re-pulls on a real content change (a photo swap), never per tick. Alignment/opacity
    /// changes come through `apply_settings` → `emit_panels_changed`.
    pub fn info_line_snapshot(&self) -> Option<(String, String, bool, bool)> {
        self.info_line_visible().then(|| {
            (
                self.info_line_main().unwrap_or_default(),
                self.info_line_codec(),
                self.info_line_is_live(),
                self.info_line_is_animated(),
            )
        })
    }

    pub fn show_info_line(&mut self) {
        // Native shell draws the line — just track the toggle state; no HUD raster / colliders.
        if self.native_info {
            self.info_line_shown = self.current.is_some();
            self.info_line_item = self.displayed_item;
            return;
        }
        let Some(hud) = self.hud.as_ref() else {
            return;
        };
        let Some(text) = self.info_line_content() else {
            return;
        };
        let px = (15.0 * self.viewport.scale_factor).max(8.0);
        let pad = (7.0 * self.viewport.scale_factor).round().max(2.0) as u32;
        let theme = hud.theme();
        let info_bg = theme.bg_for_opacity(self.settings.info_opacity);
        let Some((bitmap, w, h)) = hud.render_panel(&text, px, pad, info_bg) else {
            return;
        };
        let margin = self.overlay_margin();
        let align = self.info_line_halign();
        if let Some(a) = self.renderer.as_mut() {
            a.set_info_line(Some((&bitmap, w, h)), margin, align);
        }
        self.info_line_shown = true;
        self.info_line_item = self.displayed_item;
        self.info_line_w = w;
        self.info_line_h = h;
        self.replace_colliders(); // re-lift the panel / re-cap the tree if they overlap
    }

    /// Clear the info-line layer and drop any reservation it was causing.
    pub fn hide_info_line(&mut self) {
        // Native shell: nothing to tear down — just clear the tracking state.
        if self.native_info {
            self.info_line_shown = false;
            self.info_line_item = None;
            return;
        }
        if let Some(a) = self.renderer.as_mut() {
            a.set_info_line(None, 0, pb_render::HAlign::Right);
        }
        self.info_line_shown = false;
        self.info_line_item = None;
        self.info_line_w = 0;
        self.info_line_h = 0;
        self.replace_colliders();
    }

    /// Re-place the layers that reserve space against the info line — the rich panel
    /// (lifts) and the folder tree (caps its height) — after the line's presence,
    /// size, or alignment changed. The toast re-reads the reserve on its next build
    /// (it's transient). A redraw covers the case where nothing needed re-placing.
    fn replace_colliders(&mut self) {
        if self.overlay_shown {
            self.show_overlay();
        }
        if self.folder_tree_panel.is_some() {
            self.folder_tree_sig = None; // force a rebuild at the new tree height budget
        }
        if !self.overlay_shown {
            self.draw();
        }
    }

    /// Install a decoded animation as active playback and show its first (or stepped)
    /// frame. `play` starts continuous playback; a non-zero `step` lands paused on that
    /// frame (the frame-step path). Surfaces the truncation toast.
    pub fn install_animation(&mut self, anim: pb_decode::Animation, play: bool, step: i32) {
        let truncated = anim.truncated;
        let mut pb = Playback::new(anim, play);
        if step != 0 {
            pb.step(step);
        }
        self.playback = Some(pb);
        self.present_anim_frame();
        if truncated {
            self.show_toast("Animation truncated");
        }
    }

    /// Upload the current animation frame and redraw (the playback present path —
    /// `set_image`, never the prefetch ring). Resets the per-frame deadline anchor.
    pub fn present_anim_frame(&mut self) {
        {
            let Some(pb) = self.playback.as_ref() else {
                return;
            };
            let color = render_color(&pb.color());
            let frame = pb.current_frame();
            if let Some(a) = self.renderer.as_mut() {
                a.set_image(&frame.rgba, frame.width, frame.height, color, false, 1.0);
            }
        }
        self.anim_frame_shown_at = Some(self.now);
        // Keep a shown detailed-EXIF panel's live "Frame X / N" in sync as the frame
        // changes. Off the hot path (only during user-engaged playback/stepping), and
        // the EXIF read is memoized so this never re-reads the file per frame.
        if self.overlay_shown && self.slot_content() == Some(SlotContent::Details) {
            self.show_overlay(); // rebuilds the table + draws
        } else {
            self.draw();
        }
    }

    /// Advance playback to the due frame and return the next frame's wake deadline
    /// (None when not actively playing), so the loop sleeps exactly until then.
    pub fn tick_playback(&mut self, now: Instant) -> Option<Instant> {
        let shown = self.anim_frame_shown_at;
        let due = self.playback.as_ref().is_some_and(|pb| {
            let since = shown
                .map(|t| now.saturating_duration_since(t))
                .unwrap_or(Duration::ZERO);
            pb.is_due(since)
        });
        if due {
            self.playback.as_mut().unwrap().advance();
            self.present_anim_frame(); // updates anim_frame_shown_at + draws
        }
        // A finished Live Photo reverts to the crisp still after a beat (rather than
        // parking on the low-res last motion frame). Arm the timer once, on the finish.
        let live_finished = self
            .playback
            .as_ref()
            .is_some_and(|pb| pb.is_finished() && pb.kind() == pb_decode::AnimationKind::LivePhoto);
        if live_finished && self.live_revert_at.is_none() {
            self.live_revert_at = Some(now + LIVE_REVERT_DELAY);
        }
        let shown = self.anim_frame_shown_at;
        self.playback
            .as_ref()
            .filter(|pb| pb.is_playing())
            .map(|pb| shown.unwrap_or(now) + pb.current_delay())
    }

    /// Drive the held-key frame-step scrub (`,`/`.`). Returns whether a frame-step key
    /// is held (so the loop keeps polling). One step on press, then repeats at
    /// [`FRAME_STEP_REPEAT`] after the initial tap delay.
    pub fn tick_frame_step(&mut self, now: Instant) -> bool {
        let dir = self.held_frame_step();
        if dir == 0 {
            self.framestep_started = None;
            self.framestep_last = None;
            return false;
        }
        // Need a decoded sequence to scrub; while it's still decoding, keep ticking.
        if self.playback.is_none() {
            return true;
        }
        let past_delay = timing::elapsed_since(self.framestep_started, now, self.initial_delay);
        let due = timing::elapsed_since(self.framestep_last, now, FRAME_STEP_REPEAT);
        if past_delay && due {
            self.playback.as_mut().unwrap().step(dir);
            self.present_anim_frame();
            self.framestep_last = Some(now);
        }
        true
    }

    /// Apply a scaling mode (8 = fit, 9 = fill, 0 toggles original ↔ fill). Always
    /// resets zoom/pan back to the mode's natural framing — so tapping a mode key
    /// is also "reset my zoom." Only an actual mode *change* bumps the geometry
    /// epoch and re-buffers neighbors (the decode resolution can change); pressing
    /// the current mode's key just re-frames, no re-decode.
    pub fn set_scale_mode(&mut self, mode: ScaleMode) {
        let changed = self.view.mode != mode;
        self.view.mode = mode;
        self.view.zoom = 1.0;
        self.view.pan = [0.0, 0.0];
        self.push_view();
        if changed {
            self.invalidate_geometry();
            self.load_current_sync();
            self.target_item = self.playlist.current();
            self.request_prefetch();
        } else {
            self.draw();
        }
    }

    /// Grow the playlist in place as a streaming scan delivers more images: swap in the
    /// larger snapshot and extend the cursor's universe **without** resetting the displayed
    /// photo, the cursor, the resident ring, or any per-image cache. The contrast with
    /// [`rebuild_playlist`](App::rebuild_playlist) is the whole point — a fresh open nukes
    /// everything; a *grow* keeps it, because indices are append-only (index `i` is still
    /// the same photo). New neighbours become decodable, so we re-issue prefetch (still the
    /// scanning, anti-thrash variant — the scan isn't done yet), and the title's "X / N"
    /// total ticks up. A no-op if the snapshot isn't actually larger.
    pub fn extend_playlist(&mut self, source: Arc<dyn PhotoSource>) {
        let new_len = source.len();
        if new_len <= self.source.len() {
            return;
        }
        self.source = source;
        self.playlist.extend(new_len);
        self.request_prefetch();
        self.refresh_title();
    }

    /// Recompute the prefetch want-list and hand it to the decode pool. Two tiers:
    /// the whole window is fetched as fast **previews** (HEIC thumbnails etc.) so
    /// scrolling never outruns decode; then, once **settled**, a current-first ring
    /// of the resident window is re-fetched at full resolution and upgraded in place
    /// (see `upgrade_set`). While a nav key is held the upgrade set is empty, so fast
    /// scrolling stays entirely on the cheap preview tier — the parallel decoders
    /// aren't tied up on fulls you fly past. (Pre-libheif this was a single on-screen
    /// full because WIC's HEVC decoder serialized; libheif decodes in parallel, so we
    /// now fill a VRAM-bounded ring of fulls around the cursor.)
    pub fn request_prefetch(&mut self) {
        // While a folder scan is streaming in, the random deck regenerates on every batch,
        // so prefetching the random look-ahead would decode-then-evict photos the user never
        // sees (thrash). Use the sequential-only, no-wrap variant until the scan completes,
        // then normal prefetch (with its random hedges) resumes (`poll_dir_scan` Done arm).
        self.targets = if self.scanning {
            prefetch_targets_scanning(&self.playlist, self.ahead, self.behind)
        } else {
            prefetch_targets(&self.playlist, self.ahead, self.behind)
        };
        // The compare pin rides every want-list at top-2 priority — below only the
        // current target. `targets` is the ring's eviction keep-list (priority =
        // position), so this both exempts the pin from eviction as the window
        // recenters far away AND queues its decode if it was ever lost: the `Y`
        // flip must stay a rebind, never a decode (task #43). At capacity 1 the
        // current photo still wins (the pin ranks second) — the planned edge.
        if let Some(pin) = self.compare_pin {
            if self.targets.first() != Some(&pin) {
                self.targets.retain(|&t| t != pin);
                self.targets.insert(1.min(self.targets.len()), pin);
            }
        }
        let fit = self.decode_fit();
        // Drop tier bookkeeping for items no longer resident (evicted).
        self.preview_resident
            .retain(|i| self.ring.slot_for(*i).is_some());
        self.upgrade_done
            .retain(|i| self.ring.slot_for(*i).is_some());
        self.full_requested_at
            .retain(|i, _| self.ring.slot_for(*i).is_some());
        // Items decoded but not yet uploaded must not be re-requested (the pool no
        // longer tracks them, so it would decode them again).
        let pending: HashSet<usize> = self.pending_uploads.iter().map(|o| o.key.item).collect();
        let sharpen = self.sharpen_now();
        let ring: HashSet<usize> = self.prefetch_fulls().into_iter().collect();
        // Stamp when each full was first requested, for the `sharpen` latency metric.
        if let Some(d) = sharpen {
            self.full_requested_at.entry(d).or_insert_with(Instant::now);
        }
        for &t in &ring {
            self.full_requested_at.entry(t).or_insert_with(Instant::now);
        }

        // Build the job list in three priority tiers (the pool decodes by position):
        //   1. `sharpen` — the on-screen photo's full, so what you're looking at goes
        //      sharp ASAP the moment you park.
        //   2. previews — the whole window, so flying / re-flying is always instant.
        //   3. `ring` fulls — the sharp ring prefetched around the cursor, queued
        //      behind every preview, so a fast fly stays smooth (these decode only in
        //      the pool's spare capacity) and the fulls land ahead of where you're
        //      heading — a stop finds the photo already sharp.
        type Job = (usize, Option<FitBox>, bool);
        let (mut head, mut previews, mut fulls): (Vec<Job>, Vec<Job>, Vec<Job>) =
            (Vec::new(), Vec::new(), Vec::new());
        for &t in &self.targets {
            if self.failed.contains(&t) || pending.contains(&t) {
                continue;
            }
            let resident = self.ring.slot_for(t).is_some();
            let is_prev = resident && self.preview_resident.contains(&t);
            if resident && !is_prev {
                continue; // already full
            }
            if !resident {
                previews.push((t, fit, true));
            } else if Some(t) == sharpen {
                head.push((t, fit, false));
            } else if ring.contains(&t) {
                fulls.push((t, fit, false));
            }
            // else: resident preview not in the ring → leave it as a preview
        }
        let mut jobs = head;
        jobs.append(&mut previews);
        jobs.append(&mut fulls);
        self.pool.set_targets(self.epoch, &self.source, &jobs);
    }

    /// The decode-to-fit target for the current mode: the display size in Fit mode
    /// (downscale large photos), or full resolution for Fill / Original (so Fill
    /// isn't upscale-blurry and Original is pixel-exact).
    pub fn decode_fit(&self) -> Option<FitBox> {
        match self.view.mode {
            ScaleMode::Fit => self.fit,
            ScaleMode::Fill | ScaleMode::Original => None,
        }
    }

    /// Estimated bytes for one resident ring slot at the current scale mode: the
    /// decode-target box for bounded modes (Fit, and Fill later), or the current
    /// photo's true full-res size for Original. Sizes the ring so VRAM stays in
    /// budget even though full-res textures are much larger than fit ones.
    pub fn slot_bytes_estimate(&self) -> u64 {
        let fit = self.fit.unwrap_or(FitBox {
            max_width: 1,
            max_height: 1,
        });
        let fit_bytes = fit.max_width as u64 * fit.max_height as u64 * 4;
        match self.decode_fit() {
            // Bounded decode (Fit/Fill): a slot is at most the target box.
            Some(b) => (b.max_width as u64 * b.max_height as u64 * 4).max(1),
            // Full-res (Original): estimate from the current photo's true size,
            // never below a fit slot (and clamp_to_max bounds the real extreme).
            None => self
                .current
                .as_ref()
                .map(|m| (m.w as u64 * m.h as u64 * 4).max(fit_bytes))
                .unwrap_or(fit_bytes),
        }
    }

    /// The on-screen photo to sharpen FIRST (top decode priority): the displayed one,
    /// but only when parked (no nav key held) and currently a resident preview with a
    /// better decode to pull. `None` while flying (sharpening a frame that's about to
    /// change is pointless) and `None` once it's already full.
    pub fn sharpen_now(&self) -> Option<usize> {
        if self.held_nav().is_some() {
            return None;
        }
        let d = self.displayed_item?;
        (self.ring.slot_for(d).is_some()
            && self.preview_resident.contains(&d)
            && !self.upgrade_done.contains(&d))
        .then_some(d)
    }

    /// The full-res "sharp ring" to prefetch around the cursor at LOW priority (below
    /// every preview) — a VRAM-bounded, current-first prefix of the window, filtered
    /// to resident previews, minus `sharpen_now` (requested at high priority instead).
    ///
    /// Unlike `sharpen_now`, this runs EVEN WHILE FLYING: the fulls are queued behind
    /// all previews (see `request_prefetch`), so a fast fly stays preview-smooth — the
    /// pool decodes them only in spare capacity. But as you slow down or browse, the
    /// fulls for where you're heading land *ahead* of you, so a stop finds the photo
    /// already sharp instead of paying a cold ~115 ms–1 s decode after the fact. The
    /// workers that decode them would otherwise be idle, so it's near-free.
    pub fn prefetch_fulls(&self) -> Vec<usize> {
        let full_bytes = self.slot_bytes_estimate();
        let sharpen = self.sharpen_now();
        full_ring(
            &self.targets,
            full_bytes,
            RING_BUDGET_BYTES,
            self.ring.capacity().min(MAX_FULL_RING),
        )
        .into_iter()
        .filter(|&i| {
            Some(i) != sharpen
                && self.ring.slot_for(i).is_some()
                && self.preview_resident.contains(&i)
                && !self.upgrade_done.contains(&i)
                && !self.is_raw_item(i)
        })
        .collect()
    }

    /// Whether `item`'s full decode is a slow RAW demosaic (seconds, and once started
    /// it can't be cancelled). Excluded from the speculative ahead-ring so a few RAWs
    /// in the window can't tie up the decode workers — starving the previews a fly
    /// needs — for neighbours you may never visit. The displayed RAW still sharpens
    /// via `sharpen_now`, and a RAW's embedded preview is often near-full-res anyway.
    pub fn is_raw_item(&self, item: usize) -> bool {
        Path::new(self.source.name(item))
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| pb_decode::is_raw_extension(&e.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    /// All items we currently want decoded to full (sharpen + ahead-ring), for the
    /// idle pump's change-detection and "keep ticking while sharpening" gate.
    pub fn fulls_wanted(&self) -> Vec<usize> {
        let mut v = Vec::new();
        if let Some(d) = self.sharpen_now() {
            v.push(d);
        }
        v.extend(self.prefetch_fulls());
        v
    }

    /// Load the per-photo view state for `item`: rotation from the RAM override
    /// map (upright if absent), zoom/pan reset to a fresh framing — unless a compare
    /// flip staged a carry (`compare_carry`), consumed here so the flip's first
    /// frame lands already positioned (no reset-then-snap flicker). Returns the
    /// view to push to the renderer. (Scaling mode is global and left unchanged.)
    pub fn view_for(&mut self, item: usize) -> ViewTransform {
        self.view.rotation = self.rotations.get(&item).copied().unwrap_or_default();
        if let Some((zoom, pan)) = self.compare_carry.take() {
            self.view.zoom = zoom;
            self.view.pan = pan;
        } else {
            self.view.zoom = 1.0;
            self.view.pan = [0.0, 0.0];
        }
        self.view
    }

    /// Rotate the on-screen photo 90° clockwise (counter-clockwise on `Shift+R`).
    /// Per-image and RAM-only; returning to upright drops the override entry.
    pub fn rotate(&mut self, ccw: bool) {
        let Some(item) = self.displayed_item else {
            return;
        };
        let cur = self.rotations.get(&item).copied().unwrap_or_default();
        let new = if ccw { cur.ccw() } else { cur.cw() };
        if new == Rotation::default() {
            self.rotations.remove(&item);
        } else {
            self.rotations.insert(item, new);
        }
        // The text scan reads the pixels as displayed — a rotation changes them, so
        // drop this item's cached result (and any in-flight scan); an open `T` panel
        // rebuilds (and re-kicks the scan) on the next settled tick.
        self.recognized_text.remove(&item);
        if self.text_scan.as_ref().is_some_and(|s| s.item == item) {
            self.text_scan = None;
        }
        // Same for the AI description — the rotated pixels are a different image.
        self.descriptions.remove(&item);
        if self.describe_scan.as_ref().is_some_and(|s| s.item == item) {
            self.describe_scan = None;
        }
        if matches!(
            self.slot_content(),
            Some(SlotContent::Text) | Some(SlotContent::Describe)
        ) {
            self.overlay_shown = false;
        }
        self.view.rotation = new;
        self.push_view();
        // Flash a directional rotate icon (icon-only pill) as feedback.
        let ico = if ccw {
            ToastIcon::RotateLeft
        } else {
            ToastIcon::RotateRight
        };
        self.show_toast_icon("", ico);
    }

    /// Copy the current photo to the OS clipboard (`Ctrl+C` / Edit ▸ Copy, task #27).
    ///
    /// Decodes the original at **full resolution** here — not the fit-downscaled ring
    /// texture — so a paste lands at native size. This is a synchronous decode on the
    /// event-loop thread, which is fine: Copy is an explicit, infrequent user command
    /// (like the modal file picker), not the nav hot path. Any in-RAM rotation
    /// override is baked into the copied pixels so the clipboard is WYSIWYG.
    pub fn copy_image(&mut self) {
        let Some(item) = self.displayed_item else {
            return; // empty state — nothing to copy
        };
        let img = match decode_item(self.source.as_ref(), item, None, false) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("copy: decode failed: {}: {e}", self.source.name(item));
                self.show_toast("Copy failed");
                return;
            }
        };
        let rgba = to_clipboard_rgba8(&img);
        let rot = self.rotations.get(&item).copied().unwrap_or_default();
        let (rgba, w, h) = rotate_rgba8(&rgba, img.width, img.height, rot);
        // Offer the source file as CF_HDROP too when there is one; an archive entry
        // has no file on disk, so it gets an image-only copy (pixels still paste). The
        // pure decode + rotate prep stays here; the platform write is the shell's job.
        let file = self.source.path(item).map(|p| p.to_path_buf());
        self.effects.push(contract::CoreEffect::WriteClipboard(
            contract::ClipboardPayload::Image { rgba, w, h, file },
        ));
    }

    /// Copy the current photo's **file path** to the clipboard as text (Shift+Ctrl+C /
    /// Edit ▸ Copy File Path; ⇧⌘C on macOS). The full path for a filesystem source, or
    /// the entry name for an archive (which has no path on disk). An explicit user
    /// command — never the view path. Uses the cross-platform text clipboard (arboard),
    /// separate from the image clipboard (`clipboard.rs`).
    pub fn copy_path(&mut self) {
        let Some(item) = self.displayed_item else {
            return; // empty state — nothing to copy
        };
        let text = match self.source.path(item) {
            Some(p) => p.to_string_lossy().into_owned(),
            None => self.source.name(item).to_string(),
        };
        // A specific toast: it copies the **full path**, so "Copied file path" (not the bare
        // file name the shell's single-line fallback would show, which read as a name copy).
        self.effects.push(contract::CoreEffect::WriteClipboard(
            contract::ClipboardPayload::Text {
                text,
                toast: Some("Copied file path".to_string()),
            },
        ));
    }

    /// Whether **Show in Finder/Explorer** is available: the displayed photo is a real
    /// file on disk (not an archive entry, not the empty deck). Drives the File-menu
    /// item's enabled state (`menu_state_from` / `apply_menu_state`), mirroring
    /// [`can_save_rotation`](Self::can_save_rotation).
    pub fn can_reveal(&self) -> bool {
        self.displayed_item
            .is_some_and(|item| self.source.path(item).is_some())
    }

    /// **Show in Finder/Explorer** (File menu): reveal the displayed photo in the OS file
    /// manager — its containing folder open, the file selected. Only a real on-disk file can
    /// be revealed; an archive entry or the empty deck toasts instead. An explicit user
    /// command that only launches the file manager on a path already being viewed — no pixel
    /// read, no persistent trace (privacy #2, same category as Copy File Path). The platform
    /// launch is the shell's job (`CoreEffect::RevealPath`).
    pub fn reveal_in_file_manager(&mut self) {
        let path = self.displayed_item.and_then(|item| self.source.path(item));
        match path {
            Some(p) => self
                .effects
                .push(contract::CoreEffect::RevealPath(p.to_path_buf())),
            None => self.show_toast("Nothing to reveal"),
        }
    }

    /// **Copy EXIF data** (context menu): copy the displayed photo's metadata to the
    /// clipboard as text — the same facts the full-EXIF panel shows (filename, dimensions,
    /// codec, exact byte size, and every non-blob EXIF tag), read on-demand from RAM
    /// (privacy #2). Unlike the panel, this is *not* truncated to the screen — it copies the
    /// full set. The platform clipboard write is the shell's job (`WriteClipboard`).
    pub fn copy_image_details(&mut self) {
        let Some(item) = self.displayed_item else {
            self.show_toast("Nothing to copy");
            return;
        };
        self.ensure_exif_cached(item);
        let mut lines: Vec<String> = vec![file_name_of(self.source.name(item)).to_string()];
        if let Some(meta) = &self.current {
            lines.push(format!("Dimensions: {} × {}", meta.w, meta.h));
            lines.push(format!("Codec: {}", meta.codec.to_uppercase()));
        }
        if let Some((size, fields)) = self.exif_cache.get(&item) {
            lines.push(format!("File Size: {} bytes", hud::format_thousands(*size)));
            for (tag, val) in fields {
                // Skip binary blobs (Apple MakerNote/Padding) that render as meaningless hex.
                if is_exif_blob(tag, val) {
                    continue;
                }
                lines.push(format!("{tag}: {val}"));
            }
        }
        // Only the filename line means there was nothing worth copying.
        if lines.len() <= 1 {
            self.show_toast("No EXIF data");
            return;
        }
        self.effects.push(contract::CoreEffect::WriteClipboard(
            contract::ClipboardPayload::Text {
                text: lines.join("\n"),
                toast: Some("Copied details".to_string()),
            },
        ));
    }

    /// **Copy Text from Image** (Edit / context menu, task #45): put the text
    /// recognized *in* the displayed photo — on-device OCR lines plus QR-code
    /// payloads — on the clipboard. Uses the cached scan when present; otherwise
    /// kicks the off-thread scan and copies when it lands (`copy_when_done`). An
    /// explicit user command; the scan never leaves the machine and the result is
    /// RAM-only (privacy #2).
    pub fn copy_image_text(&mut self) {
        let Some(item) = self.displayed_item else {
            self.show_toast("Nothing to copy");
            return;
        };
        if self.recognized_text.contains_key(&item) {
            self.copy_recognized(item);
            return;
        }
        self.ensure_text_scan();
        if let Some(scan) = self.text_scan.as_mut() {
            if scan.item == item {
                scan.copy_when_done = true;
                // Feedback that a scan is running; the result toast replaces it.
                self.show_toast("Reading text…");
            }
        }
    }

    /// **Show text in image** (`T`, task #45): toggle the Inspector's Text tab.
    /// Opens the Inspector on Text, switches to Text if it's open elsewhere, closes
    /// it if Text is already showing; while `Tab`-hidden it reveals (never closes).
    pub fn toggle_image_text(&mut self) {
        self.panels.toggle_inspector(InspectorTab::Text);
        self.refresh_slot();
    }

    /// Kick the off-thread text scan for the displayed photo unless its result is
    /// already cached or that same scan is already in flight. Replacing a stale
    /// in-flight scan (another item's) drops its receiver — the worker's send fails
    /// and its thread exits quietly. Decode + OCR + QR all run on the worker; the
    /// event loop never blocks.
    fn ensure_text_scan(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.recognized_text.contains_key(&item)
            || self.text_scan.as_ref().is_some_and(|s| s.item == item)
        {
            return;
        }
        let gen = self.text_gen;
        let source = Arc::clone(&self.source);
        // Bake the in-RAM rotation override: OCR wants the pixels upright as shown.
        let rot = self.rotations.get(&item).copied().unwrap_or_default();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::image_text::scan_job(source.as_ref(), item, rot));
        });
        self.text_scan = Some(crate::image_text::TextScan {
            gen,
            item,
            copy_when_done: false,
            rx,
        });
    }

    /// Pick up a finished text scan (called each tick). A result from before a
    /// playlist rebuild is dropped — the indices were reassigned — but a result for
    /// an item the user merely navigated away from still caches (it's item-keyed, so
    /// the revisit is instant).
    pub fn poll_text_scan(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let outcome = {
            let Some(s) = self.text_scan.as_ref() else {
                return;
            };
            match s.rx.try_recv() {
                Ok(result) => Some((s.gen, s.item, s.copy_when_done, result)),
                Err(TryRecvError::Empty) => return, // still scanning
                Err(TryRecvError::Disconnected) => None, // worker died
            }
        };
        self.text_scan = None;
        let Some((gen, item, copy, result)) = outcome else {
            return;
        };
        if gen != self.text_gen {
            return; // deck rebuilt while scanning — stale indices
        }
        self.recognized_text.insert(item, result);
        // The `T` panel may be sitting on its "Reading text…" state for this item.
        if self.slot_content() == Some(SlotContent::Text) && self.displayed_item == Some(item) {
            self.show_overlay();
        }
        if copy {
            self.copy_recognized(item);
        }
    }

    /// Push a cached scan result to the clipboard seam with its specific toast
    /// ("Copied 214 characters" / "Copied text + 1 QR code"), or toast why there is
    /// nothing to copy.
    fn copy_recognized(&mut self, item: usize) {
        let Some(r) = self.recognized_text.get(&item) else {
            return;
        };
        if r.is_empty() {
            let msg = r
                .ocr_error
                .clone()
                .unwrap_or_else(|| "No text found".to_string());
            self.show_toast(&msg);
            return;
        }
        self.effects.push(contract::CoreEffect::WriteClipboard(
            contract::ClipboardPayload::Text {
                text: r.clipboard_text(),
                toast: Some(r.copy_toast()),
            },
        ));
    }

    // (The `T` panel's display lines moved to `panels::TextPanel::lines` — the
    // shell-neutral model owns the projection; see `Self::text_panel`.)

    // --- AI image description (task #44) ------------------------------------------

    /// **Describe image** (`D`, task #44): toggle the Inspector's Describe tab.
    /// Showing it kicks the vision-model describe for the displayed photo (no-op
    /// when cached / already running); `D` on an already-showing Describe tab closes
    /// the Inspector; while `Tab`-hidden it reveals (never closes).
    pub fn describe_image(&mut self) {
        let was_showing = self.slot_content() == Some(SlotContent::Describe);
        self.panels.toggle_inspector(InspectorTab::Describe);
        if was_showing {
            self.refresh_slot(); // closed it
            return;
        }
        // An explicit `D` retries a previously-failed describe (the endpoint may have come up,
        // or Local Network permission was just granted) — a cached *error* is cleared so the
        // scan re-runs; a cached success stays put (revisits are instant). This is why a
        // failure is never a dead end: press D again.
        if let Some(item) = self.displayed_item {
            if matches!(self.descriptions.get(&item), Some(Err(_))) {
                self.descriptions.remove(&item);
            }
        }
        self.ensure_describe_scan(None); // default (accessibility) prompt
        self.show_overlay();
        self.refresh_info_line_visibility(); // Tab-hidden reveals with the panel
    }

    /// **Ask about image** (`Shift+D`, task #44 subtask 9): open the text-input dialog for a
    /// question about the current photo. The shell collects the (multi-line) text and returns
    /// it as [`contract::DialogResult::AskSubmitted`], which drives [`Self::ask_describe`].
    /// Nothing to ask about on the empty deck.
    pub fn ask_image(&mut self) {
        if self.displayed_item.is_none() {
            self.show_toast("Nothing to ask about");
            return;
        }
        self.effects.push(contract::CoreEffect::ShowDialog(
            contract::DialogKind::AskImage,
        ));
    }

    /// Run a describe for the displayed photo with a caller-supplied question (from the
    /// Ask dialog). Bypasses the general-description cache so each question re-runs, and
    /// shows the answer in the same panel.
    pub fn ask_describe(&mut self, question: String) {
        let q = question.trim().to_string();
        if q.is_empty() {
            return;
        }
        if let Some(item) = self.displayed_item {
            // Force a fresh run for the question, replacing any cached general description.
            self.descriptions.remove(&item);
            if self.describe_scan.as_ref().is_some_and(|s| s.item == item) {
                self.describe_scan = None;
            }
        }
        // Showing an answer never *closes* the panel — open, not toggle.
        self.panels.open_inspector(InspectorTab::Describe);
        self.ensure_describe_scan(Some(q));
        self.show_overlay();
        self.refresh_info_line_visibility(); // Tab-hidden reveals with the panel
    }

    /// Kick the off-thread describe for the displayed photo unless its result is cached or
    /// that same describe is already running. `prompt_override` is the Ask question; `None`
    /// builds the default accessibility prompt from salient EXIF (`prompt::build_prompt`).
    /// A misconfigured backend caches a one-line error rather than a description.
    fn ensure_describe_scan(&mut self, prompt_override: Option<String>) {
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.descriptions.contains_key(&item)
            || self.describe_scan.as_ref().is_some_and(|s| s.item == item)
        {
            return;
        }
        let Some(describer) = self.describer_from_settings() else {
            self.descriptions.insert(
                item,
                Err("No description backend is set up (Settings ▸ AI Descriptions).".to_string()),
            );
            return;
        };
        let prompt = prompt_override.unwrap_or_else(|| self.default_describe_prompt(item));
        let gen = self.describe_gen;
        let source = Arc::clone(&self.source);
        // Bake the in-RAM rotation override: the model should see the pixels upright.
        let rot = self.rotations.get(&item).copied().unwrap_or_default();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::describe::describe_job(
                source.as_ref(),
                item,
                rot,
                &prompt,
                describer.as_ref(),
            ));
        });
        self.describe_scan = Some(crate::describe::DescribeScan {
            gen,
            item,
            copy_when_done: false,
            rx,
        });
    }

    /// Pick up a finished describe (called each tick). A result from before a playlist
    /// rebuild is dropped (indices reassigned); a result for an item merely navigated away
    /// from still caches (item-keyed, so a revisit is instant).
    pub fn poll_describe_scan(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let outcome = {
            let Some(s) = self.describe_scan.as_ref() else {
                return;
            };
            match s.rx.try_recv() {
                Ok(result) => Some((s.gen, s.item, s.copy_when_done, result)),
                Err(TryRecvError::Empty) => return, // still describing
                Err(TryRecvError::Disconnected) => None, // worker died
            }
        };
        self.describe_scan = None;
        let Some((gen, item, copy, result)) = outcome else {
            return;
        };
        if gen != self.describe_gen {
            return; // deck rebuilt while describing — stale indices
        }
        // Store the description, or the backend error as a one-line user message.
        self.descriptions
            .insert(item, result.map_err(|e| e.user_message()));
        if self.slot_content() == Some(SlotContent::Describe) && self.displayed_item == Some(item) {
            self.show_overlay();
        }
        // A deferred Copy AI description: copy the text now, or toast the error.
        if copy {
            match self.descriptions.get(&item) {
                Some(Ok(text)) => {
                    self.effects.push(contract::CoreEffect::WriteClipboard(
                        contract::ClipboardPayload::Text {
                            text: text.clone(),
                            toast: Some("Copied description".to_string()),
                        },
                    ));
                }
                Some(Err(msg)) => {
                    let msg = msg.clone();
                    self.show_toast(&msg);
                }
                None => {}
            }
        }
    }

    /// Build the describer from settings. Apple Foundation Models (Auto-when-available /
    /// AppleOnDevice) is delegated to the Swift host via a `CoreEffect` (subtask 5), so on
    /// this build the local endpoint is the only in-core backend and `Auto` resolves to it.
    /// `None` when no endpoint is configured — the caller surfaces the "set up a backend"
    /// message.
    fn describer_from_settings(&self) -> Option<Box<dyn crate::describe::Describer>> {
        use crate::settings::DescribeBackend;
        match self.settings.describe_backend {
            // Apple-only with no FM host wired yet → nothing in-core can serve it.
            DescribeBackend::AppleOnDevice => None,
            DescribeBackend::Auto | DescribeBackend::LocalEndpoint => {
                let url = self.settings.describe_endpoint.trim();
                if url.is_empty() {
                    return None;
                }
                Some(Box::new(crate::describe::LocalEndpoint::new(
                    url.to_string(),
                    self.settings.describe_model.clone(),
                    self.settings.describe_max_tokens,
                )))
            }
        }
    }

    /// The default describe prompt for `item`: salient EXIF + filename/folder framed as
    /// unverified (`prompt::build_prompt`), honoring a `describe_prompt` custom template.
    fn default_describe_prompt(&mut self, item: usize) -> String {
        self.ensure_exif_cached(item);
        let name = self.source.name(item).to_string();
        let exif: &[(String, String)] = self
            .exif_cache
            .get(&item)
            .map(|(_, f)| f.as_slice())
            .unwrap_or(&[]);
        // No calendar clock in the pure core → skip future-date filtering (the epoch-default
        // junk filter still applies); a stray future date is harmless (metadata is unverified).
        let ctx = crate::prompt::build_context(&name, exif, None);
        crate::prompt::build_prompt(&ctx, self.settings.describe_prompt.as_deref())
    }

    /// **Copy AI description** (Edit / context menu, task #44): put the current photo's
    /// description on the clipboard. Uses the cached description when present; otherwise
    /// kicks the describe off-thread and copies when it lands (`copy_when_done`, the
    /// Copy-Text-from-Image shape). A cached *error* is cleared and retried (conditions may
    /// have changed). An explicit user command; the result is RAM-only (privacy #2).
    pub fn copy_description(&mut self) {
        let Some(item) = self.displayed_item else {
            self.show_toast("Nothing to copy");
            return;
        };
        if let Some(Ok(text)) = self.descriptions.get(&item) {
            self.effects.push(contract::CoreEffect::WriteClipboard(
                contract::ClipboardPayload::Text {
                    text: text.clone(),
                    toast: Some("Copied description".to_string()),
                },
            ));
            return;
        }
        // No usable description yet (never generated, or a stale error) — generate one and
        // copy when it lands.
        self.descriptions.remove(&item);
        self.ensure_describe_scan(None);
        match self.describe_scan.as_mut() {
            Some(scan) if scan.item == item => {
                scan.copy_when_done = true;
                self.show_toast("Describing…");
            }
            // No backend → `ensure_describe_scan` cached the setup-hint error instead of
            // spawning; surface it rather than leaving the user without feedback.
            _ => {
                if let Some(Err(msg)) = self.descriptions.get(&item) {
                    let msg = msg.clone();
                    self.show_toast(&msg);
                }
            }
        }
    }

    // (The Describe panel's display lines moved to `panels::DescribePanel::lines` —
    // the shell-neutral model owns the projection; see `Self::describe_panel`.)

    /// Right-click over the photo (task #41): ask the shell to pop up the **photo context
    /// menu** at the cursor. Fills a shell-neutral [`contract::ContextMenuState`] from live
    /// state (Play only when the photo has motion, Show in Finder/Explorer only for a real
    /// on-disk file) and pushes [`contract::CoreEffect::ShowContextMenu`]. Over the empty
    /// deck there's nothing per-photo to offer, so no menu is shown.
    pub fn show_context_menu(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        let state = contract::ContextMenuState {
            has_image: true,
            has_motion: self.has_motion(item),
            can_reveal: self.source.path(item).is_some(),
            fullscreen: !self.windowed,
            compare_pinned: self.compare_pin.is_some(),
            compare_pinned_here: self.compare_pin == Some(item),
        };
        self.effects
            .push(contract::CoreEffect::ShowContextMenu(state));
    }

    /// Show ring `slot` (holding `item`): the keypress fast path — a rebind, no
    /// decode or upload. Updates the pin, title, and info panel.
    pub fn present_item(&mut self, item: usize, slot: usize) {
        // `present` = the whole event-loop-thread cost of one advance (rebind + title +
        // GPU-submit), the keypress fast path. It's the metric to watch for a hold-to-fly
        // regression: the NS0 inversion (renderer behind `Box<dyn Renderer>`, window ops as
        // effects) must leave this flat. `--metrics` only; a no-op branch otherwise.
        let t0 = Instant::now();
        let view = self.view_for(item);
        let title = title_for(self.source.name(item), item, self.source.len());
        if let Some(r) = self.renderer.as_mut() {
            r.set_view(view);
            r.present_slot(slot);
        }
        self.effects.push(contract::CoreEffect::SetTitle(title));
        self.ring.set_displayed(slot);
        // A fresh landing on a *different* photo re-arms the play hint. `anim_hint_shown_for`
        // is keyed to the item and only updated when landing on an animated one — so without
        // this, visiting a non-animated photo in between would leave it latched, and returning
        // to an animated photo (or arriving from a non-animated one) wouldn't re-show the hint.
        // Guarded on the item actually changing, so a re-present of the same photo (e.g. a play
        // reverting to its still) doesn't re-arm it.
        if self.displayed_item != Some(item) {
            self.anim_hint_shown_for = None;
        }
        self.displayed_item = Some(item);
        self.current = self.meta_cache.get(&item).cloned();
        // The panel (if shown) is now stale for the old photo; `about_to_wait`
        // rebuilds it for `item` next tick (or hides it while flying), so it
        // tracks the photo with no blank flash. The bitmap stays up meanwhile.
        self.last_present = Some(self.now);
        self.draw();
        self.metrics.record("present", t0.elapsed());
    }

    /// A target that failed to decode (corrupt/unreadable): count it as "shown"
    /// so the gated advance isn't stuck on it, but clear the previous frame's
    /// stale metadata — set a decode-error window title and drop the info panel so
    /// neither misreports the held-over pixels as the failed photo. The previous
    /// frame stays up rather than flashing black.
    pub fn present_failed(&mut self, item: usize) {
        self.displayed_item = Some(item);
        self.current = None;
        let name = file_name_of(self.source.name(item));
        let total = self.source.len();
        self.effects.push(contract::CoreEffect::SetTitle(format!(
            "{name} ({}/{total}) - decode error",
            item + 1
        )));
        // The info panel + line belonged to the previous photo — drop them (and redraw
        // to remove them). Only touch the renderer if something was actually showing.
        if self.overlay_shown || self.info_line_shown {
            if let Some(r) = self.renderer.as_mut() {
                if self.overlay_shown {
                    r.set_overlay(None, 0, 0);
                }
                if self.info_line_shown {
                    r.set_info_line(None, 0, pb_render::HAlign::Right);
                }
            }
            self.overlay_shown = false;
            self.overlay_item = None;
            self.info_line_shown = false;
            self.info_line_item = None;
            self.info_line_h = 0;
            self.draw();
        }
    }

    /// Try to show `target_item`: present it on a ring hit, otherwise keep the
    /// previous frame (a miss is a hold, never a skip). Returns whether shown.
    pub fn try_present_target(&mut self) -> bool {
        let Some(item) = self.target_item else {
            return false;
        };
        if self.displayed_item == Some(item) {
            return true;
        }
        if self.failed.contains(&item) {
            // Known-bad file: count it as shown (the previous frame stays up) so
            // navigation never stalls on a corrupt prefetched JPEG.
            self.present_failed(item);
            return true;
        }
        if let Some(slot) = self.ring.slot_for(item) {
            self.present_item(item, slot);
            true
        } else {
            false
        }
    }

    /// Drain finished decodes: discard stale/duplicate results, handle decode
    /// errors, then upload the highest-priority ready images (**current target
    /// first**) into ring slots — at most `UPLOADS_PER_TICK` per tick so a burst
    /// can't blow the frame budget. Lower-priority leftovers are stashed for the
    /// next tick (so the target never waits behind neighbors), keeping their pool
    /// byte-budget reservation as backpressure.
    pub fn drain_results(&mut self) {
        // Gather everything ready plus last tick's leftovers, dropping stale /
        // duplicate / errored results so only live decoded images remain.
        let mut ready: Vec<Outcome> = std::mem::take(&mut self.pending_uploads);
        while let Ok(o) = self.results.try_recv() {
            ready.push(o);
        }
        let mut target_failed: Option<usize> = None;
        ready.retain(|o| {
            if o.key.epoch != self.epoch {
                return false; // stale geometry
            }
            let item = o.key.item;
            let resident = self.ring.slot_for(item).is_some();
            if let Err(ref e) = o.result {
                if resident {
                    // A full-upgrade decode failed, but the resident preview is fine
                    // — keep it and stop retrying the upgrade.
                    self.upgrade_done.insert(item);
                    return false;
                }
                eprintln!("decode failed for item {item}: {e}");
                self.failed.insert(item);
                // Unstick the gated loop: a corrupt target counts as "shown".
                // (Deferred out of the closure — `present_failed` needs &mut self.)
                if self.target_item == Some(item) {
                    target_failed = Some(item);
                }
                return false;
            }
            if resident {
                // Already resident. The only outcome we still want is a *full*
                // decode upgrading a resident preview (uploaded in place below). A
                // preview-only upgrade result (e.g. RAW whose only image is its
                // preview) is marked done here so the idle pass stops retrying —
                // otherwise the upgrade loops forever, re-decoding every tick. Any
                // other already-resident duplicate is dropped.
                let is_prev = self.preview_resident.contains(&item);
                let img = o.result.as_ref().expect("Err handled above");
                if is_prev && img.is_preview {
                    self.upgrade_done.insert(item);
                }
                return is_prev && !img.is_preview;
            }
            true
        });
        if let Some(item) = target_failed {
            self.present_failed(item);
        }

        // Current target first, then by prefetch priority, unknowns last.
        let target = self.target_item;
        ready.sort_by_key(|o| {
            let item = o.key.item;
            if target == Some(item) {
                0usize
            } else {
                self.targets
                    .iter()
                    .position(|&t| t == item)
                    .map(|p| p + 1)
                    .unwrap_or(usize::MAX)
            }
        });

        let mut uploads = 0;
        let mut leftover = Vec::new();
        for outcome in ready {
            let item = outcome.key.item;
            let Ok(ref img) = outcome.result else {
                continue; // errors were already filtered out above
            };
            // A full decode for an item already resident as a preview is its
            // in-place upgrade (the retain above kept only real fulls; preview-only
            // upgrade results were already marked `upgrade_done` and dropped).
            let upgrade =
                self.preview_resident.contains(&item) && self.ring.slot_for(item).is_some();
            if uploads >= UPLOADS_PER_TICK {
                // Carry still-wanted leftovers to the next tick (in priority order);
                // drop now-obsolete ones so they don't pin pool byte-budget while
                // the loop idles (work_pending wouldn't keep polling for them).
                if self.targets.contains(&item) && (upgrade || self.ring.slot_for(item).is_none()) {
                    leftover.push(outcome);
                }
                continue;
            }
            if !self.meta_cache.contains_key(&item) {
                let m = meta_for(self.source.as_ref(), item, &self.root, img);
                self.meta_cache.insert(item, m);
            }
            let item_bytes = img.pixels.len() as u64;
            if upgrade {
                let slot = self.ring.slot_for(item).expect("resident as preview");
                if let Some(a) = self.renderer.as_mut() {
                    let t0 = Instant::now();
                    a.upload_slot(
                        slot,
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(img),
                        img.peak,
                    );
                    self.metrics.record("upload", t0.elapsed());
                }
                self.ring.set_slot_bytes(item, item_bytes);
                self.preview_resident.remove(&item);
                // Real end-to-end sharpen latency for the ON-SCREEN photo (what the
                // user actually waits on): full requested → full on screen. Ahead-ring
                // fulls land late by design (low priority), so they'd skew this — only
                // record the displayed one.
                let t0 = self.full_requested_at.remove(&item);
                if self.displayed_item == Some(item) {
                    if let Some(t0) = t0 {
                        self.metrics.record("sharpen", t0.elapsed());
                    }
                }
                uploads += 1;
                // If it's the photo on screen, re-present the slot so the renderer
                // picks up the full texture's dimensions/peak and re-places the quad
                // (it kept the preview's dims otherwise — visible in Original mode),
                // then redraw it now-sharp. `present_slot` keeps the current view, so
                // any zoom/pan is preserved.
                if self.displayed_item == Some(item) {
                    if let Some(a) = self.renderer.as_mut() {
                        a.present_slot(slot);
                    }
                    self.draw();
                }
                continue;
            }
            if let Some(res) = self
                .ring
                .reserve_bytes(item, self.epoch, item_bytes, &self.targets)
            {
                if let Some(a) = self.renderer.as_mut() {
                    let t0 = Instant::now();
                    a.upload_slot(
                        res.slot,
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(img),
                        img.peak,
                    );
                    self.metrics.record("upload", t0.elapsed());
                }
                self.ring.mark_resident(item, res.slot, self.epoch);
                if img.is_preview {
                    self.preview_resident.insert(item);
                } else {
                    self.preview_resident.remove(&item);
                }
                uploads += 1;
                if self.target_item == Some(item) && self.displayed_item != Some(item) {
                    self.present_item(item, res.slot);
                }
            }
            // reserve == None (no longer wanted): drop the outcome, freeing budget.
        }
        self.pending_uploads = leftover;
    }

    /// Synchronous decode + display of the current item — an instant frame on the
    /// first paint and on geometry changes (resize / scale-mode toggle), before the
    /// async ring re-fills neighbors at the new resolution.
    pub fn load_current_sync(&mut self) {
        let Some(idx) = self.playlist.current() else {
            return;
        };
        let t0 = Instant::now();
        // Preview-first (`allow_preview = true`): this decode runs **synchronously on
        // the event-loop thread**, so it must be fast. For RAW/HEIC that means the
        // embedded preview (tens of ms) instead of a full sensor demosaic — which on a
        // 40 MB NEF is ~20 s and froze the loop into a beachball on a Finder open. The
        // full-resolution decode lands off-thread: `request_prefetch` (called by every
        // caller right after this) re-decodes this item into the ring and `sharpen_now`
        // upgrades the on-screen preview to full in place (`drain_results`). This is the
        // documented "preview-first, then refine" model, now applied to the first frame
        // too. (JPEG/PNG/etc. have no cheaper preview, so this is a full decode anyway —
        // fast enough not to beachball, and faster still once dev builds optimize the
        // decoders; see the `[profile.dev]` note in the workspace Cargo.toml.)
        let decoded = decode_item(self.source.as_ref(), idx, self.decode_fit(), true);
        self.metrics.record("decode", t0.elapsed());
        match decoded {
            Ok(img) => {
                let meta = meta_for(self.source.as_ref(), idx, &self.root, &img);
                self.current = Some(meta.clone());
                self.meta_cache.insert(idx, meta);
                let view = self.view_for(idx);
                let title = title_for(self.source.name(idx), idx, self.source.len());
                if let Some(r) = self.renderer.as_mut() {
                    r.set_view(view);
                    r.set_image(
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(&img),
                        img.peak,
                    );
                    r.set_overlay(None, 0, 0);
                    r.set_info_line(None, 0, pb_render::HAlign::Right);
                }
                self.effects.push(contract::CoreEffect::SetTitle(title));
                self.overlay_shown = false;
                self.overlay_item = None;
                // New photo/size: clear the line and let the tick rebuild it fresh
                // (the dims in its text may have changed), like the panel above.
                self.info_line_shown = false;
                self.info_line_item = None;
                self.info_line_h = 0;
                self.displayed_item = Some(idx);
            }
            Err(e) => {
                eprintln!("decode failed: {}: {e}", self.source.name(idx));
                self.failed.insert(idx);
                // Keep the gate unstuck (count the bad file as "shown") and clear
                // the stale frame's title/panel so they don't misreport it.
                self.present_failed(idx);
            }
        }
        self.last_present = Some(self.now);
        self.draw();
    }

    /// Bump the geometry epoch and rebuild the (now-invalid) ring. Called on resize
    /// and fit/original toggle so in-flight decodes for the old size are discarded.
    pub fn invalidate_geometry(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        let fit = self.fit.unwrap_or(FitBox {
            max_width: 1,
            max_height: 1,
        });
        let cap = ring_capacity(self.slot_bytes_estimate());
        self.ring = ResidentRing::new_with_budget(cap, RING_BUDGET_BYTES);
        if let Some(a) = self.renderer.as_mut() {
            a.reserve_ring(cap, fit.max_width, fit.max_height);
        }
        let (ahead, behind) = window_for_capacity(cap);
        self.ahead = ahead;
        self.behind = behind;
        // Drop decodes staged for the old geometry; they free their pool budget.
        self.pending_uploads.clear();
    }

    /// Start / stop the slideshow (task #23, the `S` key + View ▸ Slideshow). Starting
    /// resets the timer (`last_present = now`) so the first slide shows for a full
    /// interval before advancing; `about_to_wait` drives the auto-advance from there.
    pub fn toggle_slideshow(&mut self) {
        let on = self.slideshow.toggle();
        if on {
            self.last_present = Some(self.now);
        }
        self.show_toast(if on { "Slideshow" } else { "Slideshow Stopped" });
    }

    /// Change the slideshow interval by `steps` × 0.5s (the `[` / `]` keys: `-1`
    /// shortens, `+1` lengthens), clamped, and flash the new value (e.g. `2.0s`). The
    /// change applies live: the deadline is `last_present + interval`, so a running
    /// slideshow's current slide gets more / less remaining time immediately.
    pub fn adjust_slideshow(&mut self, steps: i32) {
        let interval = self.slideshow.adjust(steps);
        self.show_toast(&slideshow::format_interval(interval));
    }

    /// The current slideshow interval, formatted for display (e.g. `4s`, `0.5s`) — the
    /// same formatting the `[`/`]` adjust toast uses. The macOS toolbar shows this on its
    /// slideshow control (task #55). Reflects live adjustments, not just the configured
    /// default, since it reads the running `slideshow.interval`.
    pub fn slideshow_interval_display(&self) -> String {
        slideshow::format_interval(self.slideshow.interval)
    }

    /// Request the native picker (`O` = file(s), `Shift+O` = folder). Computes the start
    /// directory from live state (core), then emits an [`CoreEffect::OpenFilePanel`] /
    /// [`OpenFolderPanel`](CoreEffect::OpenFolderPanel); the shell runs the modal panel in
    /// the drain and re-enters via [`App::finish_picker`]. Modal — it blocks the event loop
    /// while open, which is fine: the app isn't flying through photos with a dialog up.
    pub fn open_picker(&mut self, folder: bool) {
        let fallback = default_picker_dir();
        let mut start_dir = picker_start_dir(
            self.settings.picker_dir.as_deref(),
            self.source.container(),
            self.scan_root.as_deref(),
            &self.root,
            self.settings.last_folder.as_deref(),
            &fallback,
        );
        // If the chosen folder no longer exists (e.g. a pinned folder was deleted or
        // unmounted), use the safe default rather than letting the OS dialog surface its
        // own remembered last folder.
        if !start_dir.is_dir() {
            start_dir = fallback;
        }
        self.effects.push(if folder {
            contract::CoreEffect::OpenFolderPanel { start_dir }
        } else {
            contract::CoreEffect::OpenFilePanel { start_dir }
        });
    }

    /// Re-set the window title for the currently displayed photo (e.g. after a streaming
    /// grow bumps the "X / N" total). No-op if nothing is displayed.
    pub fn refresh_title(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        if item >= self.source.len() {
            return;
        }
        let title = title_for(self.source.name(item), item, self.source.len());
        self.effects.push(contract::CoreEffect::SetTitle(title));
    }

    /// Push the current view transform to the renderer (re-places the quad).
    pub fn push_view(&mut self) {
        let view = self.view;
        if let Some(a) = self.renderer.as_mut() {
            a.set_view(view);
        }
    }

    /// Decode the first image at the display size for an instant first frame.
    /// Returns `(pixels, w, h, color, hdr, peak, title)`.
    pub fn initial_image(
        &mut self,
    ) -> (
        Vec<u8>,
        u32,
        u32,
        pb_render::ColorTransform,
        bool,
        f32,
        String,
    ) {
        let srgb = pb_render::ColorTransform::srgb();
        match self.playlist.current() {
            // Preview-first (see `load_current_sync`): this runs synchronously while the
            // window is hidden during setup, so it grabs the fast embedded preview for
            // RAW/HEIC; the pool upgrades it to full once `resumed` kicks off prefetch.
            Some(idx) => match decode_item(self.source.as_ref(), idx, self.decode_fit(), true) {
                Ok(img) => {
                    let meta = meta_for(self.source.as_ref(), idx, &self.root, &img);
                    self.current = Some(meta.clone());
                    self.meta_cache.insert(idx, meta);
                    let title = title_for(self.source.name(idx), idx, self.source.len());
                    let (w, h, hdr, peak) = (img.width, img.height, is_hdr(&img), img.peak);
                    let color = render_color(&img.color);
                    (img.pixels, w, h, color, hdr, peak, title)
                }
                Err(e) => {
                    eprintln!("decode failed: {}: {e}", self.source.name(idx));
                    self.current = None;
                    let p = test_pattern(1600, 1000);
                    (
                        p,
                        1600,
                        1000,
                        srgb,
                        false,
                        1.0,
                        "PhotoBlaze (decode error)".to_string(),
                    )
                }
            },
            None => {
                // No images: hand the renderer a 1×1 dummy just to construct, then the
                // caller blanks it (`clear_image`) and shows the "Press O to open" hint.
                self.current = None;
                (
                    vec![0, 0, 0, 255],
                    1,
                    1,
                    srgb,
                    false,
                    1.0,
                    "PhotoBlaze".to_string(),
                )
            }
        }
    }

    /// The full-EXIF "nerd" panel rows for the displayed photo: a filename/path
    /// header (spanning both columns), then a two-column table of dimensions,
    /// codec, exact byte size, and every EXIF tag. Read on-demand from RAM
    /// (privacy task #2: nothing cached to disk). Capped to fit the screen height.
    pub fn exif_rows(&self) -> Vec<DetailRow> {
        let Some(item) = self.displayed_item else {
            return Vec::new();
        };
        let name = self.source.name(item);
        let mut rows = Vec::new();
        // Identity header: filename (bold) over its folder (the filename is already
        // shown above, so the path row is the parent directory only).
        rows.push(DetailRow::Span {
            text: file_name_of(name),
            bold: true,
        });
        // Location row. A real file shows its on-disk folder. An archive entry
        // shows the archive's path, with the in-archive folder appended (after a
        // `›`) when the entry lives in a subfolder — so a zip's photos report
        // *where the zip is* plus *where inside it they are*.
        let location = match (self.source.path(item), self.source.container()) {
            (Some(p), _) => p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.display().to_string()),
            (None, Some(zip)) => {
                let inner = Path::new(name)
                    .parent()
                    .map(|d| d.to_string_lossy().replace('\\', "/"))
                    .filter(|s| !s.is_empty());
                Some(match inner {
                    Some(dir) => format!("{} › {}", zip.display(), dir),
                    None => zip.display().to_string(),
                })
            }
            (None, None) => Path::new(name)
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.to_string_lossy().replace('\\', "/")),
        };
        if let Some(location) = location {
            rows.push(DetailRow::Span {
                text: location,
                bold: false,
            });
        }
        if let Some(meta) = &self.current {
            rows.push(DetailRow::Pair {
                label: "Dimensions".to_string(),
                value: format!("{} × {}", meta.w, meta.h),
            });
            rows.push(DetailRow::Pair {
                label: "Codec".to_string(),
                value: meta.codec.to_uppercase(),
            });
        }
        // Animation facts (frame count, live current frame, rate, loop) — right under
        // the codec so an animated file reads as one block.
        rows.extend(self.animation_rows(item));
        // File size + EXIF from the memoized read (populated by `ensure_exif_cached`
        // before this is called; a cold miss simply omits them until the next rebuild).
        if let Some((size, fields)) = self.exif_cache.get(&item) {
            rows.push(DetailRow::Pair {
                label: "File Size".to_string(),
                value: format!("{} bytes", hud::format_thousands(*size)),
            });
            for (tag, val) in fields {
                // Skip binary blobs that render as meaningless hex (Apple
                // MakerNote/Padding are kilobytes long); truncate anything else
                // that's overlong so one field can't blow out the panel width.
                if is_exif_blob(tag, val) {
                    continue;
                }
                rows.push(DetailRow::Pair {
                    label: tag.clone(),
                    value: truncate_exif_value(val),
                });
            }
        }
        // Cap to what fits the screen height (~1.5x the font size per line) — for the
        // fixed-height HUD table only. The native Inspector scrolls, so it shows every row.
        if !self.native_inspector {
            if let Some(fit) = self.fit {
                let line_h = ((15.0 * self.viewport.scale_factor).max(8.0) * 1.5).max(1.0);
                let max_rows = (((fit.max_height as f32) - 40.0) / line_h).max(1.0) as usize;
                if rows.len() > max_rows {
                    rows.truncate(max_rows.saturating_sub(1));
                    rows.push(DetailRow::Span {
                        text: "…".to_string(),
                        bold: false,
                    });
                }
            }
        }
        rows
    }

    /// Populate the per-item EXIF cache (file size + raw tag/value pairs) if absent, so
    /// the detailed panel — and its per-frame rebuilds while an animation plays — read
    /// the encoded bytes at most once. RAM-only, read-only (privacy #2).
    pub fn ensure_exif_cached(&mut self, item: usize) {
        if self.exif_cache.contains_key(&item) {
            return;
        }
        if let Ok(bytes) = self.source.bytes(item) {
            let fields = read_exif_fields(&bytes);
            self.exif_cache.insert(item, (bytes.len() as u64, fields));
        }
    }

    /// Animation facts for the detailed panel: empty for a still. Once the sequence is
    /// decoded (playing, or eagerly prepped) it reports the live current frame, the
    /// average frame rate, the duration, and the loop count; before that, just a hint
    /// that `P` will play it. The codec/format is already shown by the Codec row above.
    pub fn animation_rows(&self, item: usize) -> Vec<DetailRow> {
        // A Live Photo (its pairing is resolved into `live_motion_cache` when the panel
        // opens) or an animated container. Neither → nothing to add.
        let is_live = self.is_live_photo(item);
        let is_animated = self.current.as_ref().and_then(|m| m.animated).is_some();
        // Frame/timing detail needs a decoded sequence — the live playback, or the one
        // eagerly prepped for this item.
        let detail: Option<(usize, usize, Duration, u32)> = if let Some(pb) = &self.playback {
            Some((
                pb.index(),
                pb.frame_count(),
                pb.total_duration(),
                pb.loop_count(),
            ))
        } else if let Some(p) = self.prepared.as_ref().filter(|p| p.item == item) {
            let count = p.anim.frames.len();
            let total: Duration = p.anim.frames.iter().map(|f| f.delay).sum();
            Some((0, count, total, p.anim.loop_count))
        } else {
            None
        };
        if !is_live && !is_animated && detail.is_none() {
            return Vec::new();
        }
        // Reserve every row up front — the labels are known from the header sniff /
        // pairing, so a Live Photo or animation always shows the same rows. Values are a
        // pending placeholder until the sequence is decoded (eager prep on dwell), then
        // fill in **in place**, so the panel never reflows when the numbers land a beat
        // later. Playback then updates the live "Frame X / N" value with no row churn.
        const PENDING: &str = "…";
        let mut rows = Vec::new();
        // A Live Photo names itself + its frame count (the Codec row shows the still's
        // format); an animation's count lives in the Frame row below.
        if is_live {
            rows.push(DetailRow::Pair {
                label: "Live Photo".to_string(),
                value: detail.map_or(PENDING.to_string(), |(_, count, _, _)| {
                    format!("{count} frames")
                }),
            });
        }
        rows.push(DetailRow::Pair {
            label: "Frame".to_string(),
            value: detail.map_or(PENDING.to_string(), |(idx, count, _, _)| {
                format!("{} / {}", idx + 1, count)
            }),
        });
        rows.push(DetailRow::Pair {
            label: "Frame Rate".to_string(),
            value: detail.map_or(PENDING.to_string(), |(_, count, total, _)| {
                let secs = total.as_secs_f64();
                if secs > 0.0 {
                    format!("{:.1} fps", count as f64 / secs)
                } else {
                    PENDING.to_string()
                }
            }),
        });
        rows.push(DetailRow::Pair {
            label: "Duration".to_string(),
            value: detail.map_or(PENDING.to_string(), |(_, _, total, _)| {
                format!("{:.2} s", total.as_secs_f64())
            }),
        });
        // A Live Photo always plays once; the loop count is only meaningful for a
        // GIF/APNG/WebP loop.
        if !is_live {
            rows.push(DetailRow::Pair {
                label: "Loop".to_string(),
                value: detail.map_or(PENDING.to_string(), |(_, _, _, loops)| {
                    if loops == 0 {
                        "Forever".to_string()
                    } else {
                        format!("{loops}×")
                    }
                }),
            });
        }
        rows
    }

    /// Whether item `item` is a Live Photo, from the pairing cache (populated when the
    /// info panel opens / on dwell). A `&self` read — never triggers a stat — so it's
    /// safe from the render/rows path; the `&mut` [`live_motion_path`](App::live_motion_path)
    /// is what fills the cache.
    pub fn is_live_photo(&self, item: usize) -> bool {
        self.live_motion_cache
            .get(&item)
            .is_some_and(|paired| paired.is_some())
    }

    /// The native play-hint kind for the current item: `0` = none (a still, or already
    /// playing — the hint's job is done), `1` = Live Photo (the livephoto mark), `2` = another
    /// animation (play ▶). Stays consistent with `has_motion` (which bumps `play_hint_seq`):
    /// a fresh motion item is a Live Photo (→1) or has an `animated` container (→2).
    pub fn play_hint_kind(&self) -> u8 {
        if self.playback.is_some() {
            return 0; // engaged — no hint while it plays/pauses
        }
        let Some(item) = self.displayed_item else {
            return 0;
        };
        if self.is_live_photo(item) {
            1
        } else if self.current.as_ref().is_some_and(|m| m.animated.is_some()) {
            2
        } else {
            0
        }
    }

    /// Corner inset (physical px) for the info/EXIF/help panel. Scales with the
    /// surface's short edge so a fixed gap doesn't look jammed against the corner on a
    /// huge fullscreen display (#3), with a DPI-scaled floor for small windows. Read
    /// fresh on every (re)show, so toggling between window sizes always re-spaces it.
    pub fn overlay_margin(&self) -> u32 {
        let short_edge = self
            .fit
            .map(|f| f.max_width.min(f.max_height))
            .unwrap_or(800) as f32;
        let floor = 10.0 * self.viewport.scale_factor;
        (short_edge * 0.015).max(floor).round().max(1.0) as u32
    }

    /// Hide the rich panel (clears the overlay quad). The info line, a separate
    /// layer, is untouched.
    pub fn hide_overlay(&mut self) {
        if let Some(a) = self.renderer.as_mut() {
            a.set_overlay(None, 0, 0);
        }
        self.overlay_shown = false;
        self.overlay_item = None;
        self.draw();
    }

    /// The pre-formatted shortcut hint for an action's primary binding (empty if unbound) — the
    /// macOS symbol form (`⇧ O`) on macOS, the spelled-out form (`Shift+O`) elsewhere. Drives the
    /// open-screen buttons' shortcut hints, so they reflect any shortcut the user remapped in
    /// Settings.
    pub fn shortcut_for(&self, action: Action) -> String {
        self.keymap
            .bindings_for(action)
            .first()
            .map(|c| c.shortcut_label())
            .unwrap_or_default()
    }

    /// Flash a transient status message at the bottom-center (tasks.json #10) — for
    /// commands that otherwise give no visual feedback, e.g. the recursion toggle.
    /// A new toast replaces any current one.
    pub fn show_toast(&mut self, msg: &str) {
        self.show_toast_icon(msg, ToastIcon::None);
    }

    /// Like [`show_toast`] but with a leading semantic [`ToastIcon`] — e.g. the save glyph, or
    /// an icon-only pill (empty `msg`) for the rotate toasts. Each shell picks its own art:
    /// the HUD rasterizes a Font Awesome glyph; the native macOS shell (`native_toast`) instead
    /// gets the message + icon as data and draws a SwiftUI pill. Always redraws (HUD path), so a
    /// caller that also changed the view (e.g. `rotate`) renders even without a system font.
    pub fn show_toast_icon(&mut self, msg: &str, kind: ToastIcon) {
        // Native shell: hand the shell the data and let it render the pill; no CPU raster.
        if self.native_toast {
            self.toast_seq = self.toast_seq.wrapping_add(1);
            self.toast_native = Some(NativeToast {
                message: msg.to_string(),
                icon: kind,
                started: self.now,
                seq: self.toast_seq,
            });
            // Still redraw: some callers (e.g. `rotate`) change the *view* and rely on this to
            // render it — and it wakes the shell so the toast pill appears from idle.
            self.draw();
            return;
        }
        let px = (26.0 * self.viewport.scale_factor).max(16.0);
        let pad = (12.0 * self.viewport.scale_factor).round().max(4.0) as u32;
        // Map the semantic icon to the HUD's Font Awesome glyph.
        let fa = match kind {
            ToastIcon::None => None,
            ToastIcon::Mute => Some(icon::assets::VOLUME_SLASH),
            ToastIcon::Unmute => Some(icon::assets::VOLUME),
            ToastIcon::Save => Some(icon::assets::FLOPPY),
            ToastIcon::Undo => Some(icon::assets::UNDO),
            ToastIcon::Delete => Some(icon::assets::TRASH),
            ToastIcon::Recycle => Some(icon::assets::RECYCLE),
            ToastIcon::Pin => Some(icon::assets::THUMBTACK),
            ToastIcon::Unpin => Some(icon::assets::THUMBTACK_SLASH),
            ToastIcon::RotateLeft => Some(icon::assets::ROTATE_LEFT),
            ToastIcon::RotateRight => Some(icon::assets::ROTATE_RIGHT),
            ToastIcon::Copy => Some(icon::assets::CLIPBOARD),
        };
        if let Some(hud) = self.hud.as_ref() {
            if let Some((rgba, w, h)) = hud.render_panel_icon(msg, px, pad, fa, hud.theme().bg) {
                self.toast = Some(Toast {
                    rgba,
                    w,
                    h,
                    started: self.now,
                    uploaded_alpha: -1.0,
                });
                self.push_toast(1.0);
            }
        }
        self.draw();
    }

    /// Upload the current toast bitmap to the renderer at `alpha` (its alpha
    /// channel scaled), centered near the bottom.
    pub fn push_toast(&mut self, alpha: f32) {
        let (faded, w, h) = {
            let Some(t) = self.toast.as_mut() else {
                return;
            };
            t.uploaded_alpha = alpha;
            (scale_alpha(&t.rgba, alpha), t.w, t.h)
        };
        // The toast rides a fixed 64px bottom margin — always well above the info line
        // (which sits at the small `overlay_margin` inset), so the two never collide
        // vertically even when both are centered. No line reserve needed here.
        let margin = (64.0 * self.viewport.scale_factor).round().max(8.0) as u32;
        if let Some(a) = self.renderer.as_mut() {
            a.set_toast(Some((&faded, w, h)), margin);
        }
    }

    /// Advance the toast's hold/fade and return whether one is still active (so the
    /// event loop keeps ticking). Re-uploads only on a meaningful alpha change;
    /// clears the layer once expired.
    pub fn tick_toast(&mut self, now: Instant) -> bool {
        // Native path: the shell draws the pill and animates its own fade-out on removal — the
        // core just expires the data after the hold+fade window and keeps the pump ticking
        // (returning `true`) while it's live so the expiry actually fires.
        if self.native_toast {
            if let Some(t) = &self.toast_native {
                if now.saturating_duration_since(t.started) > Toast::HOLD + Toast::FADE {
                    self.toast_native = None;
                }
            }
            return self.toast_native.is_some();
        }
        let Some(alpha) = self.toast.as_ref().and_then(|t| t.alpha(now)) else {
            if self.toast.take().is_some() {
                if let Some(a) = self.renderer.as_mut() {
                    a.set_toast(None, 0);
                }
                self.draw();
            }
            return false;
        };
        let changed = self
            .toast
            .as_ref()
            .is_some_and(|t| (alpha - t.uploaded_alpha).abs() > 0.02);
        if changed {
            self.push_toast(alpha);
            self.draw();
        }
        true
    }

    /// The current keypress brighten-pulse intensity (0..=1), decaying to 0 over
    /// `PIE_GLOW_DUR` after the last dropped nav press.
    pub fn pie_glow(&self, now: Instant) -> f32 {
        match self.pie_glow_started {
            Some(t) => (1.0 - (now - t).as_secs_f32() / PIE_GLOW_DUR).clamp(0.0, 1.0),
            None => 0.0,
        }
    }

    /// Drive the top-right "not-ready" loading pie (#2). While the next photo is
    /// still decoding (a miss outlasting `PIE_SHOW_DELAY`), show a pie that eases
    /// asymptotically toward — but never reaches — full, on a time constant
    /// self-calibrated to how long misses usually take (`decode_ewma`). Once the
    /// photo lands, learn from the wait, then snap to full and fade. Returns
    /// whether the pie still needs the loop to keep ticking.
    pub fn tick_pie(&mut self, now: Instant) -> bool {
        let not_ready = self.target_item.is_some() && self.displayed_item != self.target_item;
        if not_ready {
            self.pie_finish = None;
            let start = *self.wait_started.get_or_insert(now);
            let elapsed = (now - start).as_secs_f32();
            if elapsed >= PIE_SHOW_DELAY {
                let tau = self.decode_ewma.max(PIE_TAU_MIN);
                // Asymptotic ease: ~half-full at one tau, approaching the cap but
                // never quite arriving (the deliberate, honest-ish "lie").
                let progress = (1.0 - 2f32.powf(-elapsed / tau)).min(PIE_FILL_CAP);
                let glow = self.pie_glow(now);
                self.push_pie(progress, glow, 1.0);
            } else {
                self.clear_pie();
            }
            return true; // keep ticking while we wait
        }
        // Caught up. If we were mid-wait, learn how long it took (so the estimate
        // tracks this machine + folder), and if the pie was up, play the finish.
        if let Some(start) = self.wait_started.take() {
            let waited = (now - start).as_secs_f32();
            self.decode_ewma = (self.decode_ewma * (1.0 - PIE_EWMA_ALPHA)
                + waited * PIE_EWMA_ALPHA)
                .clamp(PIE_TAU_MIN, 2.0);
            if self.pie_drawn {
                self.pie_finish = Some(now);
            }
        }
        if let Some(fstart) = self.pie_finish {
            let t = (now - fstart).as_secs_f32();
            if t < PIE_FINISH_FADE {
                let glow = self.pie_glow(now);
                self.push_pie(1.0, glow, 1.0 - t / PIE_FINISH_FADE);
                return true;
            }
            self.pie_finish = None;
        }
        self.clear_pie();
        false
    }

    /// Rasterize + upload the pie at `progress`/`glow`, scaled by a global `alpha`
    /// (the finish fade). Re-uploads + redraws only when the visible result
    /// changes (quantized), so the slow tail of the asymptote doesn't churn.
    pub fn push_pie(&mut self, progress: f32, glow: f32, alpha: f32) {
        let want = (progress, glow, alpha);
        let unchanged = self.pie_pushed.is_some_and(|(p, g, a)| {
            (p - progress).abs() < 0.01 && (g - glow).abs() < 0.04 && (a - alpha).abs() < 0.02
        });
        if unchanged && self.pie_drawn {
            return;
        }
        let diameter = (PIE_DIAMETER * self.viewport.scale_factor)
            .round()
            .max(12.0) as u32;
        let (mut rgba, w, h) = hud::render_pie(diameter, progress, glow, self.hud_dark);
        if alpha < 1.0 {
            rgba = scale_alpha(&rgba, alpha);
        }
        let margin = (PIE_MARGIN * self.viewport.scale_factor).round().max(4.0) as u32;
        if let Some(a) = self.renderer.as_mut() {
            a.set_pie(Some((&rgba, w, h)), margin);
        }
        self.pie_drawn = true;
        self.pie_pushed = Some(want);
        self.draw();
    }

    /// Clear the pie layer if it's up (and redraw to remove it).
    pub fn clear_pie(&mut self) {
        if self.pie_drawn {
            if let Some(a) = self.renderer.as_mut() {
                a.set_pie(None, 0);
            }
            self.pie_drawn = false;
            self.pie_pushed = None;
            self.draw();
        }
    }

    /// Render one frame.
    pub fn draw(&mut self) {
        let t0 = Instant::now();
        let mut fatal = false;
        let mut presented = false;
        let drew = if let Some(a) = self.renderer.as_mut() {
            match a.render() {
                Ok(p) => presented = p,
                Err(e) => {
                    eprintln!("fatal render error: {e:?}");
                    fatal = true;
                }
            }
            a.poll();
            true
        } else {
            false
        };
        // A dropped frame (`Ok(false)`) leaves the stale frame on screen — flag it so
        // `work_pending`/`tick` retry next frame; a presented frame clears any backlog.
        self.redraw_pending = drew && !fatal && !presented;
        // PB_TRACE=1: present-outcome diagnostics to stderr (dev-only; pairs with the
        // Swift host's pbTrace size reports when chasing resize/transition races).
        if pb_trace() {
            eprintln!(
                "PB draw presented={presented} viewport={}x{}",
                self.viewport.width, self.viewport.height
            );
        }
        // Push after the `self.renderer` borrow ends (can't touch `self.effects` inside it).
        if fatal {
            self.effects.push(contract::CoreEffect::Quit);
        }
        if drew {
            self.metrics.record("render", t0.elapsed());
        }
    }

    /// Which way we're currently paging, from the held nav actions (ambiguous/none =
    /// idle). Next (forward), Prev (backward), and Random / RandomPrev advance; two
    /// keys bound to the *same* direction (e.g. Enter + NumpadEnter) still count as
    /// one, but two *different* nav directions held at once is treated as idle.
    pub fn held_nav(&self) -> Option<Nav> {
        let mut dir: Option<Nav> = None;
        // Both hold sources: the keyboard's held-key map and the pointer's single held nav
        // (a toolbar ‹ › / shuffle button pressed and held) — so both blaze identically.
        for action in self.held.values().copied().chain(self.pointer_nav) {
            if let Some(n) = nav_of(action) {
                match dir {
                    None => dir = Some(n),
                    Some(d) if d == n => {}
                    Some(_) => return None, // two different directions → idle
                }
            }
        }
        dir
    }

    /// A toolbar nav/random button was pressed and **held**: begin hold-to-fly for `action`,
    /// reusing the exact keyboard path — the initial tap advance (or pie-glow while catching
    /// up) plus the self-paced fly timer. `end_pointer_nav` (mouse-up) stops it. A quick click
    /// is just begin→end with no fly, i.e. a single advance, matching a Space tap.
    pub fn begin_pointer_nav(&mut self, action: Action) {
        self.pointer_nav = Some(action);
        self.hold_start = Some(self.now);
        let Some(nav) = nav_of(action) else { return };
        if self.target_item.is_some() && self.displayed_item != self.target_item {
            self.pie_glow_started = Some(self.now);
        } else {
            self.advance(nav);
        }
    }

    /// The held toolbar nav/random button was released — stop flying.
    pub fn end_pointer_nav(&mut self) {
        self.pointer_nav = None;
    }

    /// Zoom direction from the held actions: `+1` in ([`Action::ZoomIn`]), `-1` out
    /// ([`Action::ZoomOut`]), `None` if neither or both.
    pub fn zoom_held(&self) -> Option<f32> {
        let mut zin = false;
        let mut zout = false;
        for &action in self.held.values() {
            match action {
                Action::ZoomIn => zin = true,
                Action::ZoomOut => zout = true,
                _ => {}
            }
        }
        match (zin, zout) {
            (true, false) => Some(1.0),
            (false, true) => Some(-1.0),
            _ => None,
        }
    }

    /// Pan velocity direction from the held pan actions (image-space; positive pan
    /// reveals the right/bottom). Diagonals combine. `(0, 0)` if none held.
    pub fn pan_held(&self) -> (f32, f32) {
        let mut x = 0.0;
        let mut y = 0.0;
        for &action in self.held.values() {
            match action {
                Action::PanLeft => x += 1.0,
                Action::PanRight => x -= 1.0,
                Action::PanUp => y += 1.0,
                Action::PanDown => y -= 1.0,
                _ => {}
            }
        }
        (x, y)
    }

    /// The current image texture + screen dimensions for pan-clamp math.
    pub fn screen_and_image(&self) -> Option<(u32, u32, u32, u32)> {
        let fit = self.fit?;
        let (iw, ih) = self.renderer.as_ref()?.image_size();
        Some((iw, ih, fit.max_width, fit.max_height))
    }

    /// Whether the image currently overflows the viewport (so panning does
    /// something). Drives the grab-hand cursor affordance.
    pub fn pannable(&self) -> bool {
        self.screen_and_image()
            .map(|(iw, ih, sw, sh)| {
                let mp = self.view.max_pan(iw, ih, sw, sh);
                mp[0] > 0.0 || mp[1] > 0.0
            })
            .unwrap_or(false)
    }

    /// Pan by a raw pixel delta (trackpad two-finger swipe), clamped to the image
    /// bounds. No effect when the image fits within the screen (nothing to pan).
    pub fn pan_by_pixels(&mut self, dx: f32, dy: f32) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.view.pan[0] += dx;
        self.view.pan[1] += dy;
        if let Some((iw, ih, sw, sh)) = self.screen_and_image() {
            let mp = self.view.max_pan(iw, ih, sw, sh);
            self.view.pan[0] = self.view.pan[0].clamp(-mp[0], mp[0]);
            self.view.pan[1] = self.view.pan[1].clamp(-mp[1], mp[1]);
        }
        self.push_view();
        self.draw();
    }

    /// Apply continuous zoom/pan while their keys are held, with a time-based
    /// acceleration ramp (gentle start for fine tuning, faster the longer held).
    /// Returns whether anything changed (so the loop keeps polling + redrawing).
    pub fn apply_view_holds(&mut self, now: Instant) -> bool {
        let mut changed = false;

        match self.zoom_held() {
            Some(dir) => {
                let start = *self.zoom_started.get_or_insert(now);
                let last = self.zoom_last.replace(now).unwrap_or(start);
                let dt = (now - last).as_secs_f32().min(0.1);
                let t = (now - start).as_secs_f32();
                let rate =
                    ZOOM_MIN_RATE + (ZOOM_MAX_RATE - ZOOM_MIN_RATE) * (t / ZOOM_RAMP_SECS).min(1.0);
                // Exponential (multiplicative) zoom about the screen center.
                self.view.zoom =
                    (self.view.zoom * (rate * dir * dt).exp()).clamp(MIN_ZOOM, MAX_ZOOM);
                changed = true;
            }
            None => {
                self.zoom_started = None;
                self.zoom_last = None;
            }
        }

        let (px, py) = self.pan_held();
        if px != 0.0 || py != 0.0 {
            let start = *self.pan_started.get_or_insert(now);
            let last = self.pan_last.replace(now).unwrap_or(start);
            let dt = (now - last).as_secs_f32().min(0.1);
            let t = (now - start).as_secs_f32();
            let speed =
                PAN_MIN_SPEED + (PAN_MAX_SPEED - PAN_MIN_SPEED) * (t / PAN_RAMP_SECS).min(1.0);
            self.view.pan[0] += px * speed * dt;
            self.view.pan[1] += py * speed * dt;
            if let Some((iw, ih, sw, sh)) = self.screen_and_image() {
                let mp = self.view.max_pan(iw, ih, sw, sh);
                self.view.pan[0] = self.view.pan[0].clamp(-mp[0], mp[0]);
                self.view.pan[1] = self.view.pan[1].clamp(-mp[1], mp[1]);
            }
            changed = true;
        } else {
            self.pan_started = None;
            self.pan_last = None;
        }

        if changed {
            self.push_view();
            self.draw();
        }
        changed
    }

    /// Whether item `item` is an animated container (from the cached header sniff).
    pub fn current_is_animated(&self, item: usize) -> bool {
        self.meta_cache
            .get(&item)
            .and_then(|m| m.animated)
            .is_some()
    }

    /// The companion motion `.mov` for item `item` if it's a Live Photo, else `None`
    /// (tasks #38 / #39). Filesystem pairing, memoized per item and computed lazily —
    /// only ever reached when settled on a photo, never on the fly-through path. Always
    /// `None` on platforms without a motion decoder (macOS + Windows have one).
    pub fn live_motion_path(&mut self, item: usize) -> Option<PathBuf> {
        #[cfg(not(any(
            target_os = "macos",
            windows,
            all(unix, not(target_os = "macos"), feature = "livephoto")
        )))]
        {
            let _ = item;
            None
        }
        #[cfg(any(
            target_os = "macos",
            windows,
            all(unix, not(target_os = "macos"), feature = "livephoto")
        ))]
        if let Some(cached) = self.live_motion_cache.get(&item) {
            return cached.clone();
        }
        #[cfg(any(
            target_os = "macos",
            windows,
            all(unix, not(target_os = "macos"), feature = "livephoto")
        ))]
        {
            let paired = self.source.path(item).and_then(companion_motion);
            self.live_motion_cache.insert(item, paired.clone());
            paired
        }
    }

    /// Whether item `item` has an on-demand motion component to play on `P` — either an
    /// animated container (GIF/APNG/WebP/HEIF sequence) or a Live Photo's `.mov`.
    pub fn has_motion(&mut self, item: usize) -> bool {
        self.current_is_animated(item) || self.live_motion_path(item).is_some()
    }

    /// Whether the currently displayed item has a playable motion component — the macOS
    /// toolbar dims its Play-Animation button on stills (task #55). `&mut` because Live-Photo
    /// pairing is resolved + cached on first check (cheap cache hit after; the display path
    /// has usually primed it already).
    pub fn current_has_motion(&mut self) -> bool {
        self.displayed_item.is_some_and(|i| self.has_motion(i))
    }

    /// Whether an animation / Live Photo is actively playing — the toolbar lights its
    /// Play-Animation button while it runs.
    pub fn animation_playing(&self) -> bool {
        self.playback.as_ref().is_some_and(|pb| pb.is_playing())
    }

    /// Kick the whole-sequence decode for `item` on a worker thread so a big GIF/WebP (or
    /// a Live Photo `.mov`) never stalls the event loop; the still first frame stays on
    /// screen until it lands (picked up by `poll_anim_decode`). `want` decides what
    /// happens on arrival — eager prep (stash ready), play (`P`), or step (frame-step).
    /// Signal any in-flight animation decode to stop and drop it. The worker checks the flag and
    /// bails early rather than decoding the whole clip onto a now-dropped channel — so navigating
    /// through Live Photos doesn't pile up orphaned decodes (wasted CPU + transient RAM).
    pub fn cancel_anim_decode(&mut self) {
        if let Some(d) = &self.anim_decode {
            d.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.anim_decode = None;
    }

    pub fn start_animation_decode(&mut self, item: usize, want: AnimWant) {
        // Supersede any in-flight decode so its orphaned worker stops promptly (see `cancel`).
        self.cancel_anim_decode();
        self.anim_gen += 1;
        let gen = self.anim_gen;
        let epoch = self.epoch;
        let source = Arc::clone(&self.source);
        let fit = self.decode_fit();
        // A Live Photo decodes its companion `.mov` via AVFoundation; everything else
        // decodes the still's own bytes as a multi-frame animation.
        let live = self.live_motion_path(item);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_job = std::sync::Arc::clone(&cancel);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = decode_motion_job(live, &source, item, fit, &cancel_job);
            let _ = tx.send(result);
        });
        self.anim_decode = Some(AnimDecode {
            gen,
            item,
            epoch,
            want,
            rx,
            cancel,
        });
        // A user-initiated decode (P / step) means they've engaged — suppress the "▶ P"
        // hint. An eager prep is invisible background work, so leave the hint alone.
        if want != AnimWant::Eager {
            self.anim_hint_shown_for = self.displayed_item;
        }
    }

    /// When the user has rested on an animated still, eagerly decode the whole sequence
    /// in the background so pressing `P` is instant (fixes the slow first-play on WebP /
    /// AVIF, ~0.6–2s to decode). Returns the wake deadline while the dwell elapses (so
    /// the idle loop wakes to kick it), else `None`. Strictly off the hot path — only
    /// when settled (never while flying), exactly when the prefetch pool is idle.
    pub fn maybe_prepare_animation(&mut self, now: Instant) -> Option<Instant> {
        if self.playback.is_some() || self.anim_decode.is_some() {
            return None; // already playing, or a decode is already in flight
        }
        let item = self.displayed_item?;
        if self.displayed_item != self.target_item {
            return None; // still catching up to the target — not settled yet
        }
        if self.prepared.as_ref().is_some_and(|p| p.item == item) {
            return None; // already prepped and ready
        }
        if !self.has_motion(item) {
            return None;
        }
        match self.last_present.map(|t| t + EAGER_PREP_DELAY) {
            // Still within the dwell window — wake at the deadline to kick it then.
            Some(due) if now < due => Some(due),
            _ => {
                self.start_animation_decode(item, AnimWant::Eager);
                None
            }
        }
    }

    /// Revert a finished Live Photo to its crisp full-res still: rebind the resident
    /// still texture (it was never evicted — playback draws via `set_image`, not the
    /// ring), re-decoding only in the rare case it's no longer resident.
    pub fn restore_still(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        if let Some(slot) = self.ring.slot_for(item) {
            self.present_item(item, slot);
        } else {
            self.load_current_sync();
        }
    }

    /// The held frame-step direction: `+1` ([`Action::FrameNext`]) / `-1`
    /// ([`Action::FramePrev`]) / `0` if neither or both.
    pub fn held_frame_step(&self) -> i32 {
        let mut dir = 0i32;
        for &action in self.held.values() {
            match action {
                Action::FrameNext => dir += 1,
                Action::FramePrev => dir -= 1,
                _ => {}
            }
        }
        dir.signum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{CoreEvent, Modifiers};
    use crate::{PbKey, Viewport};

    /// A minimal `AppCore` for driving `handle` in tests — the public [`AppCore::headless`]
    /// constructor at a 1×1 viewport (one construction literal, shared with the NS1 FFI bridge).
    fn test_core() -> AppCore {
        AppCore::headless(Viewport {
            width: 1,
            height: 1,
            scale_factor: 1.0,
        })
    }

    #[test]
    fn trim_caches_bounds_regenerable_caches_but_keeps_current_and_user_edits() {
        use crate::meta::PhotoMeta;
        let mut core = test_core();
        core.displayed_item = Some(5000);
        let meta = PhotoMeta {
            rel: String::new(),
            w: 1,
            h: 1,
            codec: "PNG",
            animated: None,
        };
        // Fill a regenerable cache past the high-water mark, and a user-edit map alongside it.
        for i in 0..6000 {
            core.meta_cache.insert(i, meta.clone());
            core.rotations.insert(i, Rotation::default().cw());
        }
        core.trim_caches();

        // Capped, current + neighbor kept, farthest evicted.
        assert!(core.meta_cache.len() <= 4096);
        assert!(core.meta_cache.contains_key(&5000), "current item survives");
        assert!(core.meta_cache.contains_key(&4999), "a neighbor survives");
        assert!(!core.meta_cache.contains_key(&0), "the farthest is evicted");
        // User edits (rotations) are never a cache — all 6000 survive.
        assert_eq!(core.rotations.len(), 6000, "rotations are never evicted");

        // Under the high-water mark → a no-op (doesn't churn while small).
        core.meta_cache.clear();
        for i in 0..100 {
            core.meta_cache.insert(i, meta.clone());
        }
        core.trim_caches();
        assert_eq!(core.meta_cache.len(), 100);
    }

    #[test]
    fn info_line_fields_respect_the_settings_toggles() {
        use crate::meta::PhotoMeta;
        let mut core = test_core();
        core.current = Some(PhotoMeta {
            rel: "folder/photo.jpg".to_string(),
            w: 4032,
            h: 3024,
            codec: "JPEG",
            animated: None,
        });
        core.info_line = true;

        // Default fields (folder off, filename/resolution/codec on): the file NAME only, not
        // the relative dir.
        assert_eq!(
            core.info_line_content().as_deref(),
            Some("photo.jpg · 4032×3024 · JPEG")
        );
        assert_eq!(
            core.info_line_main().as_deref(),
            Some("photo.jpg · 4032×3024")
        );
        assert_eq!(core.info_line_codec(), "JPEG");
        assert!(core.info_line_visible());

        // Folder on → the relative dir is prepended to the file name with a `/`.
        core.settings.info_show_folder = true;
        assert_eq!(
            core.info_line_content().as_deref(),
            Some("folder/photo.jpg · 4032×3024 · JPEG")
        );
        // Folder on, filename off → just the folder.
        core.settings.info_show_filename = false;
        assert_eq!(
            core.info_line_content().as_deref(),
            Some("folder · 4032×3024 · JPEG")
        );
        core.settings.info_show_filename = true;
        core.settings.info_show_folder = false;

        // Codec off → dropped from the string, and the pill accessor goes empty (folder is
        // back off, so it's the file name alone again).
        core.settings.info_show_codec = false;
        assert_eq!(
            core.info_line_content().as_deref(),
            Some("photo.jpg · 4032×3024")
        );
        assert_eq!(core.info_line_codec(), "");

        // Filename off too → only the resolution remains.
        core.settings.info_show_filename = false;
        assert_eq!(core.info_line_content().as_deref(), Some("4032×3024"));
        assert_eq!(core.info_line_main().as_deref(), Some("4032×3024"));

        // All fields off → the line hides (empty-pill guard) even though `i` is on.
        core.settings.info_show_resolution = false;
        assert!(!core.info_line_visible());
    }

    /// `info_line_visible()` is what the **native macOS shell** actually polls
    /// (`CoreModel.swift`) to show/hide its SwiftUI info-line view — unlike the
    /// winit HUD path, it never looks at `info_line_shown`. So `Tab` must suppress
    /// it here directly, or the native line ignores Tab-hide entirely.
    #[test]
    fn info_line_visible_respects_tab_hidden() {
        use crate::meta::PhotoMeta;
        let mut core = test_core();
        core.current = Some(PhotoMeta {
            rel: "a.jpg".to_string(),
            w: 100,
            h: 100,
            codec: "JPEG",
            animated: None,
        });
        core.info_line = true;
        assert!(core.info_line_visible());

        core.dispatch_action(Action::TogglePanels); // Tab: the line alone counts as open
        assert!(core.panels.hidden);
        assert!(
            !core.info_line_visible(),
            "the native shell's actual gate must hide too"
        );
        assert!(core.info_line, "…without turning the toggle off");

        core.dispatch_action(Action::TogglePanels); // Tab again reveals
        assert!(!core.panels.hidden);
        assert!(core.info_line_visible());
    }

    #[test]
    fn native_help_suppresses_the_hud_and_signals_visibility() {
        let mut core = test_core();
        core.native_help = true;
        // Opening Help does not rasterize a HUD overlay (the shell draws it).
        core.dispatch_action(Action::Help);
        assert!(core.panels.help, "Help is open in the model");
        assert!(!core.overlay_shown, "…but nothing is rasterized to the HUD");
        assert!(
            core.help_panel_visible(),
            "the native Help view should show"
        );
        // A tick emits the PanelsChanged marker on the show transition.
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "the tick signals the host to re-pull the panel model"
        );
        // Tab-hide hides the native view without closing it; a tick re-signals.
        core.dispatch_action(Action::TogglePanels);
        assert!(core.panels.help && core.panels.hidden);
        assert!(!core.help_panel_visible(), "hidden → the native view hides");
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)));
    }

    #[test]
    fn native_inspector_suppresses_the_hud_and_signals_on_tab_and_content() {
        use crate::overlay::InspectorTab;
        use crate::panels::InspectorSnapshot;
        let mut core = test_core();
        core.native_inspector = true;
        // Closed → not visible, no snapshot.
        assert!(!core.inspector_panel_visible());
        // Open the Details tab: visible, and the tick signals the host.
        core.panels.open_inspector(InspectorTab::Details);
        assert!(core.inspector_panel_visible());
        assert!(matches!(
            core.inspector_snapshot(),
            InspectorSnapshot::Details(_)
        ));
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "opening the Inspector signals the host"
        );
        // Switching tabs changes the snapshot → re-signals.
        core.panels.open_inspector(InspectorTab::Text);
        assert!(matches!(
            core.inspector_snapshot(),
            InspectorSnapshot::Text(_)
        ));
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "a tab switch re-signals"
        );
        // Tab-hidden → not visible (the master switch wins).
        core.panels.hidden = true;
        assert!(!core.inspector_panel_visible());
        // Winit (native_inspector off) never treats it as native-visible.
        core.panels.hidden = false;
        core.native_inspector = false;
        assert!(!core.inspector_panel_visible());
    }

    #[test]
    fn native_tree_visibility_and_safe_activate() {
        let mut core = test_core();
        assert!(!core.tree_panel_visible(), "off by default");
        core.native_tree = true;
        assert!(!core.tree_panel_visible(), "closed → not visible");
        core.folder_tree_open = true;
        assert!(core.tree_panel_visible(), "open + native → visible");
        core.panels.hidden = true;
        assert!(!core.tree_panel_visible(), "Tab-hidden → not visible");
        core.panels.hidden = false;
        // A tick signals the host on the visibility transition (no hud needed for the diff).
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "the tree's visibility change signals the host"
        );
        // Activate with nothing derived is a safe no-op (no target).
        core.tree_activate(0);
        // Winit (native_tree off) is never native-visible.
        core.native_tree = false;
        assert!(!core.tree_panel_visible());
    }

    #[test]
    fn native_open_suppresses_the_hud_and_signals() {
        let mut core = test_core(); // headless → empty source
        core.native_open = true;
        assert!(
            core.open_panel_visible(),
            "an empty deck shows the native welcome surface"
        );
        // show_open_hint must not rasterize a HUD panel (so its buttons are never
        // hit-tested beneath a native panel — the cursor fix).
        core.show_open_hint();
        // A tick signals the host on the visibility transition.
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "the empty-state visibility change signals the host"
        );
        // With native_open off (winit), the same deck is not a native-visible panel.
        core.native_open = false;
        assert!(!core.open_panel_visible());
    }

    #[test]
    fn winit_keeps_help_on_the_hud_no_native_signal() {
        // With native_help off (the winit shell), Help is a HUD panel and never a
        // native-visible one, and the marker never fires.
        let mut core = test_core();
        core.dispatch_action(Action::Help);
        assert!(core.panels.help);
        assert!(
            !core.help_panel_visible(),
            "no native presentation on winit"
        );
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(!core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)));
    }

    #[test]
    fn info_line_reserve_follows_the_horizontal_overlap() {
        let mut core = test_core();
        core.viewport = Viewport {
            width: 1000,
            height: 800,
            scale_factor: 1.0,
        };
        core.info_line_shown = true;
        core.info_line_w = 300;
        core.info_line_h = 30;
        let m = core.overlay_margin() as f32;
        // Panel spans for a right-anchored Inspector and a left-anchored tree — narrow
        // columns near the edges, so a short centered line clears both.
        let right_panel = (1000.0 - m - 200.0, 1000.0 - m); // bottom-right, 200px wide
        let left_tree = (m, m + 200.0); // top-left, 200px column

        // Right-aligned line: overlaps the right panel, clears the left tree.
        core.settings.info_line_align = settings::InfoLineAlign::Right;
        assert!(core.info_line_reserve_for(right_panel.0, right_panel.1) > 0);
        assert_eq!(core.info_line_reserve_for(left_tree.0, left_tree.1), 0);

        // Left-aligned line: overlaps the tree, clears the right panel.
        core.settings.info_line_align = settings::InfoLineAlign::Left;
        assert!(core.info_line_reserve_for(left_tree.0, left_tree.1) > 0);
        assert_eq!(core.info_line_reserve_for(right_panel.0, right_panel.1), 0);

        // Narrow, short centered line: reaches neither corner.
        core.settings.info_line_align = settings::InfoLineAlign::Center;
        core.info_line_w = 200;
        assert_eq!(core.info_line_reserve_for(left_tree.0, left_tree.1), 0);
        assert_eq!(core.info_line_reserve_for(right_panel.0, right_panel.1), 0);

        // The narrow-window case the owner flagged: a wide centered line (a long
        // filename spanning most of the width) overlaps BOTH corner panels.
        core.info_line_w = 900;
        assert!(
            core.info_line_reserve_for(left_tree.0, left_tree.1) > 0,
            "a wide centered line reaches the left tree"
        );
        assert!(
            core.info_line_reserve_for(right_panel.0, right_panel.1) > 0,
            "…and the right panel too"
        );

        // Hidden line reserves nothing regardless of alignment.
        core.info_line_shown = false;
        assert_eq!(core.info_line_reserve_for(left_tree.0, left_tree.1), 0);
        assert_eq!(core.info_line_reserve_for(right_panel.0, right_panel.1), 0);
    }

    #[test]
    fn folder_tree_action_toggles_the_open_state() {
        let mut core = test_core();
        assert!(!core.folder_tree_open);
        core.dispatch_action(Action::FolderTree);
        assert!(core.folder_tree_open, "Shift+F opens the tree");
        core.dispatch_action(Action::FolderTree);
        assert!(!core.folder_tree_open, "Shift+F again closes it");
        assert!(
            core.folder_tree_sig.is_none(),
            "nothing drawn on a headless core (no HUD/renderer), so no signature"
        );
    }

    #[test]
    fn rebuild_playlist_records_last_folder_but_only_for_folder_backed_opens() {
        let mut core = test_core();
        assert!(
            !core.persist_prefs,
            "test cores must never write the real settings.toml"
        );
        let dir = std::env::temp_dir();
        let source: Arc<dyn PhotoSource> = Arc::new(FsSource::new(vec![dir.join("a.png")]));
        core.rebuild_playlist(source, dir.clone(), Some(dir.clone()), true, 0);
        assert_eq!(core.settings.last_folder.as_deref(), Some(dir.as_path()));

        // An archive-style rebuild (no scan_root) must not clobber the remembered folder.
        let source: Arc<dyn PhotoSource> = Arc::new(FsSource::new(vec![dir.join("b.png")]));
        core.rebuild_playlist(source, dir.join("x.zip"), None, false, 0);
        assert_eq!(core.settings.last_folder.as_deref(), Some(dir.as_path()));
    }

    #[test]
    fn rebuild_playlist_reseeds_the_shuffle_so_repeated_opens_diverge() {
        // Regression test: `rebuild_playlist` used to reseed the random walk with the
        // hardcoded literal `0`, so opening a deck of the same size produced the exact
        // same "random" order every single time (same launch, next launch, always).
        // Two independent opens of an equally-sized deck must land on different
        // shuffle permutations.
        let mut core = test_core();
        let dir = std::env::temp_dir();
        let paths: Vec<PathBuf> = (0..32).map(|i| dir.join(format!("{i}.png"))).collect();

        let source: Arc<dyn PhotoSource> = Arc::new(FsSource::new(paths.clone()));
        core.rebuild_playlist(source, dir.clone(), Some(dir.clone()), true, 0);
        let first = core.playlist.shuffle().clone();

        let source: Arc<dyn PhotoSource> = Arc::new(FsSource::new(paths));
        core.rebuild_playlist(source, dir.clone(), Some(dir.clone()), true, 0);
        let second = core.playlist.shuffle().clone();

        assert_ne!(
            first, second,
            "two opens of the same-size deck must not shuffle identically"
        );
    }

    // --- "Text in image" state machine (task #45): drive `poll_text_scan` with a
    // hand-fed channel — the worker/OCR backend stays out of these tests entirely.

    fn text_result(qr: &[&str], lines: &[&str]) -> crate::image_text::ImageText {
        crate::image_text::ImageText {
            qr: qr.iter().map(|s| s.to_string()).collect(),
            lines: lines.iter().map(|s| s.to_string()).collect(),
            ocr_error: None,
        }
    }

    /// Install an in-flight scan whose result is already sitting in the channel.
    fn feed_scan(core: &mut AppCore, item: usize, copy: bool, r: crate::image_text::ImageText) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(r).unwrap();
        core.text_scan = Some(crate::image_text::TextScan {
            gen: core.text_gen,
            item,
            copy_when_done: copy,
            rx,
        });
    }

    fn clipboard_text_effects(core: &AppCore) -> Vec<(String, Option<String>)> {
        core.effects
            .iter()
            .filter_map(|e| match e {
                contract::CoreEffect::WriteClipboard(contract::ClipboardPayload::Text {
                    text,
                    toast,
                }) => Some((text.clone(), toast.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn show_image_text_toggles_the_panel_mode() {
        let mut core = test_core();
        core.dispatch_action(Action::ShowImageText);
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Text),
            "T opens the Inspector on Text"
        );
        core.dispatch_action(Action::ShowImageText);
        assert_eq!(core.panels.inspector, None, "T again closes it");
        // The basic `i` line is now fully independent (task #54 decouple): pressing
        // `i` while the Text panel is open turns the line on WITHOUT closing the
        // panel — they coexist, the line sitting below the panel.
        core.dispatch_action(Action::ShowImageText);
        core.dispatch_action(Action::Info);
        assert!(core.info_line, "i turns the line on");
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Text),
            "…and the Text panel stays open — no longer mutually exclusive"
        );
        assert_eq!(core.slot_content(), Some(SlotContent::Text));
    }

    #[test]
    fn info_line_and_inspector_are_independent() {
        // The bug the decouple fixes: i, Shift+I, i again used to be a dead no-op
        // (the line was occluded by the panel and i toggled its hidden state). Now
        // each press has a visible, independent effect.
        let mut core = test_core();
        core.dispatch_action(Action::Info); // line on
        assert!(core.info_line);
        core.dispatch_action(Action::FullExif); // Details on — line stays on
        assert!(core.info_line, "Shift+I does not touch the line");
        assert_eq!(core.panels.inspector, Some(InspectorTab::Details));
        core.dispatch_action(Action::Info); // i again — turns the LINE off
        assert!(!core.info_line, "i toggles only the line");
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Details),
            "…and leaves the Details panel open"
        );
        // Shift+I again closes the panel; the line is independent and stays off.
        core.dispatch_action(Action::FullExif);
        assert_eq!(core.panels.inspector, None);
        assert!(!core.info_line);
    }

    #[test]
    fn tab_hides_and_panel_toggles_reveal() {
        let mut core = test_core();
        core.dispatch_action(Action::TogglePanels);
        assert!(!core.panels.hidden, "Tab with nothing open is a no-op");
        core.dispatch_action(Action::ShowImageText);
        core.dispatch_action(Action::TogglePanels);
        assert!(core.panels.hidden, "Tab hides…");
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Text),
            "…without closing"
        );
        assert_eq!(core.slot_content(), None, "hidden panels draw nothing");
        // T while hidden reveals and keeps the panel open — never closes.
        core.dispatch_action(Action::ShowImageText);
        assert!(!core.panels.hidden);
        assert_eq!(core.panels.inspector, Some(InspectorTab::Text));
        // `hidden` is one master flag shared with the basic line (task #54 follow-up):
        // `i` while Tab-hidden follows the same reveal rule as `T`/Help/tree — it
        // reveals everything (not just the line) and only ever ends up shown.
        core.dispatch_action(Action::TogglePanels);
        assert!(core.panels.hidden);
        core.dispatch_action(Action::Info);
        assert!(core.info_line, "i turns the line on…");
        assert!(
            !core.panels.hidden,
            "…and reveals the rest too — same shared flag as Tab"
        );
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Text),
            "the Text panel comes back with it"
        );
    }

    #[test]
    fn tab_hides_the_drawn_info_line_and_reveal_restores_it() {
        use crate::meta::PhotoMeta;
        let mut core = test_core();
        core.native_info = true; // so show/hide_info_line() flip `info_line_shown` deterministically
        core.current = Some(PhotoMeta {
            rel: "a.jpg".to_string(),
            w: 100,
            h: 100,
            codec: "JPEG",
            animated: None,
        });
        core.displayed_item = Some(0);
        core.dispatch_action(Action::Info); // line on
        assert!(core.info_line_shown, "the line is drawn");

        core.dispatch_action(Action::TogglePanels); // Tab: nothing else open, but the line counts
        assert!(core.panels.hidden);
        assert!(!core.info_line_shown, "Tab hides the line too");
        assert!(
            core.info_line,
            "…without turning the toggle off (hidden ≠ closed)"
        );

        core.dispatch_action(Action::TogglePanels); // Tab again reveals
        assert!(!core.panels.hidden);
        assert!(core.info_line_shown, "revealed with the rest of the chrome");
    }

    #[test]
    fn text_scan_result_caches_by_item() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_scan(&mut core, 0, false, text_result(&[], &["Hello"]));
        core.poll_text_scan();
        assert!(core.text_scan.is_none(), "job consumed");
        assert_eq!(core.recognized_text[&0].lines, vec!["Hello"]);
        assert!(
            clipboard_text_effects(&core).is_empty(),
            "no copy was requested"
        );
    }

    #[test]
    fn a_result_from_before_a_rebuild_is_dropped() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_scan(&mut core, 0, false, text_result(&[], &["stale"]));
        core.text_gen += 1; // the deck was rebuilt while the scan ran
        core.poll_text_scan();
        assert!(
            core.recognized_text.is_empty(),
            "stale-generation result must not cache under a recycled index"
        );
    }

    #[test]
    fn a_result_for_a_left_item_still_caches_for_the_revisit() {
        let mut core = test_core();
        core.displayed_item = Some(3); // user moved on mid-scan
        feed_scan(&mut core, 0, false, text_result(&[], &["kept"]));
        core.poll_text_scan();
        assert_eq!(
            core.recognized_text[&0].lines,
            vec!["kept"],
            "item-keyed result is still valid — revisits are instant"
        );
    }

    #[test]
    fn copy_image_text_uses_the_cache_and_carries_the_specific_toast() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        core.recognized_text
            .insert(0, text_result(&["https://x"], &["Hello"]));
        core.dispatch_action(Action::CopyImageText);
        let got = clipboard_text_effects(&core);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "https://x\nHello", "QR payloads above the text");
        assert_eq!(got[0].1.as_deref(), Some("Copied text + 1 QR code"));
    }

    #[test]
    fn copy_requested_mid_scan_copies_when_the_result_lands() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_scan(&mut core, 0, true, text_result(&[], &["late"]));
        core.poll_text_scan();
        let got = clipboard_text_effects(&core);
        assert_eq!(got.len(), 1, "deferred copy fired on landing");
        assert_eq!(got[0].0, "late");
    }

    #[test]
    fn an_empty_scan_result_never_writes_the_clipboard() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_scan(&mut core, 0, true, text_result(&[], &[]));
        core.poll_text_scan();
        assert!(
            clipboard_text_effects(&core).is_empty(),
            "nothing found → toast only, no clipboard write"
        );
    }

    #[test]
    fn rebuild_playlist_drops_text_results_and_bumps_the_generation() {
        let mut core = test_core();
        core.recognized_text.insert(0, text_result(&[], &["old"]));
        let gen = core.text_gen;
        let dir = std::env::temp_dir();
        let source: Arc<dyn PhotoSource> = Arc::new(FsSource::new(vec![dir.join("a.png")]));
        core.rebuild_playlist(source, dir.clone(), Some(dir), true, 0);
        assert!(core.recognized_text.is_empty());
        assert!(core.text_gen > gen);
    }

    // --- AI describe state machine (task #44): drive `poll_describe_scan` with a
    // hand-fed channel; the worker/HTTP backend stays out of these tests entirely. The
    // endpoint is blanked so nothing can spawn a real network thread.

    fn feed_describe(
        core: &mut AppCore,
        item: usize,
        r: Result<String, crate::describe::DescribeError>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(r).unwrap();
        core.describe_scan = Some(crate::describe::DescribeScan {
            gen: core.describe_gen,
            item,
            copy_when_done: false,
            rx,
        });
    }

    #[test]
    fn describe_image_toggles_the_panel_mode() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        // Pre-cache so `D` is a pure toggle (no worker/network kicked).
        core.descriptions
            .insert(0, Ok("A cat on a sofa.".to_string()));
        core.dispatch_action(Action::DescribeImage);
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Describe),
            "D opens the Inspector on Describe"
        );
        core.dispatch_action(Action::DescribeImage);
        assert_eq!(core.panels.inspector, None, "D again closes it");
        // The basic line is independent — `i` while Describe is open turns the line
        // on and the panel stays (task #54 decouple).
        core.dispatch_action(Action::DescribeImage);
        core.dispatch_action(Action::Info);
        assert!(core.info_line);
        assert_eq!(core.panels.inspector, Some(InspectorTab::Describe));
        assert_eq!(core.slot_content(), Some(SlotContent::Describe));
    }

    #[test]
    fn describe_result_caches_by_item() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_describe(&mut core, 0, Ok("A red bicycle.".to_string()));
        core.poll_describe_scan();
        assert!(core.describe_scan.is_none(), "job consumed");
        assert_eq!(core.descriptions[&0].as_deref(), Ok("A red bicycle."));
    }

    #[test]
    fn describe_result_from_before_a_rebuild_is_dropped() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_describe(&mut core, 0, Ok("stale".to_string()));
        core.describe_gen += 1; // deck rebuilt while describing
        core.poll_describe_scan();
        assert!(
            core.descriptions.is_empty(),
            "stale-generation result must not cache under a recycled index"
        );
    }

    #[test]
    fn describe_backend_error_caches_a_one_line_user_message() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_describe(
            &mut core,
            0,
            Err(crate::describe::DescribeError::Unreachable),
        );
        core.poll_describe_scan();
        let msg = core.descriptions[&0].as_ref().unwrap_err();
        assert!(msg.contains("model server"), "actionable message: {msg}");
    }

    #[test]
    fn describe_with_no_endpoint_caches_the_setup_hint_without_spawning() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        core.settings.describe_endpoint = String::new(); // no backend resolvable
        core.dispatch_action(Action::DescribeImage);
        assert!(core.describe_scan.is_none(), "no worker kicked");
        let msg = core.descriptions[&0].as_ref().unwrap_err();
        assert!(
            msg.contains("backend is set up"),
            "points at Settings: {msg}"
        );
    }

    #[test]
    fn pressing_d_retries_a_previously_failed_describe() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        core.settings.describe_endpoint = String::new(); // keep it thread-free
                                                         // A stale failure is cached (e.g. from before Local Network permission was granted).
        core.descriptions
            .insert(0, Err("network error from before".to_string()));
        // Panel is closed → D opens it and must clear + retry the cached error.
        core.dispatch_action(Action::DescribeImage);
        assert_eq!(core.panels.inspector, Some(InspectorTab::Describe));
        let msg = core.descriptions[&0].as_ref().unwrap_err();
        assert!(
            msg.contains("backend is set up"),
            "the stale error was cleared and the describe re-ran (got: {msg})"
        );
    }

    #[test]
    fn copy_description_uses_the_cache_and_carries_a_toast() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        core.descriptions.insert(0, Ok("A calico cat.".to_string()));
        core.dispatch_action(Action::CopyDescription);
        let got = clipboard_text_effects(&core);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "A calico cat.");
        assert_eq!(got[0].1.as_deref(), Some("Copied description"));
    }

    #[test]
    fn copy_description_defers_the_copy_until_the_describe_lands() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        core.settings.describe_endpoint = "http://localhost:1234/v1".to_string();
        // Nothing cached → the copy arms `copy_when_done` on the in-flight scan.
        core.dispatch_action(Action::CopyDescription);
        assert!(
            core.describe_scan
                .as_ref()
                .is_some_and(|s| s.copy_when_done),
            "copy is deferred to the scan result"
        );
        // Simulate the result landing.
        feed_describe(&mut core, 0, Ok("A late description.".to_string()));
        core.describe_scan.as_mut().unwrap().copy_when_done = true;
        core.poll_describe_scan();
        let got = clipboard_text_effects(&core);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "A late description.");
        assert_eq!(got[0].1.as_deref(), Some("Copied description"));
    }

    #[test]
    fn ask_image_opens_the_dialog_and_submit_runs_the_question() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        core.dispatch_action(Action::AskImage);
        assert!(
            core.effects.iter().any(|e| matches!(
                e,
                contract::CoreEffect::ShowDialog(contract::DialogKind::AskImage)
            )),
            "Shift+D opens the ask dialog"
        );
        // The submitted question runs through describe (empty endpoint → setup hint, no thread).
        core.effects.clear();
        core.settings.describe_endpoint = String::new();
        core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::AskSubmitted("What year is this?".to_string()),
        ));
        assert_eq!(core.panels.inspector, Some(InspectorTab::Describe));
        assert!(
            core.descriptions[&0].is_err(),
            "the question re-ran the describe"
        );
    }

    #[test]
    fn ask_describe_bypasses_the_general_description_cache() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        core.settings.describe_endpoint = String::new(); // keep it thread-free
        core.descriptions
            .insert(0, Ok("old general description".to_string()));
        core.ask_describe("What year is this?".to_string());
        // The cached general description was dropped and a fresh run attempted (which,
        // with no endpoint, resolves to the setup hint rather than the stale text).
        assert!(
            core.descriptions[&0].is_err(),
            "the question re-ran instead of returning the cached description"
        );
        assert_eq!(core.panels.inspector, Some(InspectorTab::Describe));
    }

    /// A fake archive source: named entries with no fs paths and a container —
    /// exactly what a Zip/SevenZSource looks like to the core, without disk.
    struct FakeArchive {
        names: Vec<String>,
        container: PathBuf,
    }

    impl PhotoSource for FakeArchive {
        fn len(&self) -> usize {
            self.names.len()
        }
        fn name(&self, i: usize) -> &str {
            self.names.get(i).map(String::as_str).unwrap_or("")
        }
        fn container(&self) -> Option<&Path> {
            Some(&self.container)
        }
        fn bytes(&self, i: usize) -> std::io::Result<Vec<u8>> {
            self.names
                .get(i)
                .map(|_| vec![0u8])
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "oob"))
        }
    }

    /// A headless core over a fake archive deck, installed the way a real
    /// archive open lands ([`AppCore::apply_archive`]).
    fn archive_core(names: &[&str]) -> AppCore {
        let mut core = test_core();
        let container = std::env::temp_dir().join("deck.zip");
        let source: Arc<dyn PhotoSource> = Arc::new(FakeArchive {
            names: names.iter().map(|s| s.to_string()).collect(),
            container: container.clone(),
        });
        core.apply_archive(crate::scan::Resolved {
            root: container,
            source,
            scan_root: None,
            recursive: false,
            start: 0,
        });
        core
    }

    const ARCHIVE: &[&str] = &[
        "a/b/one.jpg",
        "a/b/c/two.jpg",
        "a/bc/three.jpg", // `a/bc` must never match a scope of `a/b`
        "a/four.jpg",
        "top.jpg",
    ];

    fn deck_names(core: &AppCore) -> Vec<&str> {
        (0..core.source.len())
            .map(|i| core.source.name(i))
            .collect()
    }

    #[test]
    fn apply_archive_stamps_the_unscoped_scope() {
        let core = archive_core(ARCHIVE);
        let scope = core.archive_scope.as_ref().expect("archive decks scope");
        assert_eq!(scope.prefix, "");
        assert!(
            Arc::ptr_eq(&scope.full, &core.source),
            "unscoped: the deck IS the full source, no wrapper"
        );
        assert_eq!(core.source.len(), ARCHIVE.len());
    }

    #[test]
    fn rescope_filters_the_deck_and_parent_steps_back_up() {
        let mut core = archive_core(ARCHIVE);
        core.rescope_archive("a/b".to_string());
        assert_eq!(deck_names(&core), vec!["a/b/one.jpg", "a/b/c/two.jpg"]);
        assert_eq!(core.archive_scope.as_ref().unwrap().prefix, "a/b");
        assert_eq!(core.displayed_item, Some(0), "cursor resets to the first");
        assert_eq!(
            core.source.container().map(crate::folder_tree::name_of),
            Some("deck.zip".to_string()),
            "the scoped deck still knows its archive (title, up row, Go anchor)"
        );

        // ⌘↑ steps the scope up one level at a time: a/b → a → the whole archive.
        core.open_parent_cmd();
        assert_eq!(core.archive_scope.as_ref().unwrap().prefix, "a");
        assert_eq!(core.source.len(), 4);
        core.open_parent_cmd();
        let scope = core.archive_scope.as_ref().unwrap();
        assert_eq!(scope.prefix, "");
        assert!(
            Arc::ptr_eq(&scope.full, &core.source),
            "back to the whole archive = the original source, unwrapped"
        );

        // From the archive root, ⌘↑ exits to the folder on disk containing the
        // archive — the pre-scoping behavior, now one level further up the ladder.
        core.effects.clear();
        core.open_parent_cmd();
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::BeginDirScan { .. })),
            "the containing folder opens as a normal dir scan"
        );
    }

    #[test]
    fn sibling_cmd_steps_scopes_in_ram_without_a_worker() {
        let mut core = archive_core(ARCHIVE);
        // Unscoped: the whole archive has no siblings inside itself — silent no-op.
        core.open_sibling_cmd(1);
        assert_eq!(core.archive_scope.as_ref().unwrap().prefix, "");
        assert!(core.tree_io.is_none(), "no disk worker for archive decks");

        core.rescope_archive("a/b".to_string());
        core.open_sibling_cmd(1);
        assert_eq!(core.archive_scope.as_ref().unwrap().prefix, "a/bc");
        assert_eq!(deck_names(&core), vec!["a/bc/three.jpg"]);
        core.open_sibling_cmd(1);
        assert_eq!(
            core.archive_scope.as_ref().unwrap().prefix,
            "a/bc",
            "at the end of the sorted row — nothing to step to"
        );
        core.open_sibling_cmd(-1);
        assert_eq!(core.archive_scope.as_ref().unwrap().prefix, "a/b");
        assert!(core.tree_io.is_none());
    }

    #[test]
    fn sibling_results_are_stale_guarded_and_only_matches_navigate() {
        let opened_dir = |core: &AppCore| {
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::BeginDirScan { .. }))
        };
        let feed = |core: &mut AppCore, from_root: PathBuf, target: Option<PathBuf>| {
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(crate::folder_tree::TreeIoResult::Sibling { from_root, target })
                .unwrap();
            core.tree_io = Some(crate::folder_tree::tree_io_for_tests(rx));
            core.effects.clear();
            let t = core.now;
            core.handle(CoreEvent::Tick(t));
        };

        let mut core = compare_core(2);
        // A result computed for a deck the user already left: dropped — it must
        // not yank navigation somewhere the user moved away from.
        feed(
            &mut core,
            PathBuf::from("/somewhere/else"),
            Some(PathBuf::from("/somewhere/else/next")),
        );
        assert!(!opened_dir(&core), "stale sibling results are dropped");
        assert!(core.tree_io.is_none(), "the finished job is released");

        // Nothing with photos in that direction: no navigation (the host shows
        // the toast — HUD-gated, so asserted in the live smoke, not here).
        let root = core.root.clone();
        feed(&mut core, root, None);
        assert!(
            !opened_dir(&core),
            "an exhausted search must not open anything"
        );

        // A live match opens exactly like Open Folder — the shared plan.
        let root = core.root.clone();
        let target = root.join("next-door");
        feed(&mut core, root, Some(target));
        assert!(opened_dir(&core), "a found sibling opens as a dir scan");
    }

    #[test]
    fn cmd_folder_jumps_within_the_deck_between_sibling_folders() {
        let mut core = test_core();
        let base = std::env::temp_dir().join("pb_sibling_jump");
        let paths = vec![
            base.join("alpha/1.png"),
            base.join("alpha/2.png"),
            base.join("bravo/3.png"),
            base.join("charlie/4.png"),
        ];
        let source: Arc<dyn PhotoSource> = Arc::new(FsSource::new(paths));
        core.rebuild_playlist(source, base.clone(), Some(base.clone()), true, 0);
        assert_eq!(core.current_folder_abs(), Some(base.join("alpha")));
        // ⌘→ jumps within the deck to bravo's first photo — no disk worker, no re-scan.
        core.effects.clear();
        core.open_sibling_cmd(1);
        assert_eq!(core.target_item, Some(2), "jumped to bravo/3.png");
        assert!(core.tree_io.is_none(), "in-deck jump uses no disk worker");
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::BeginDirScan { .. })),
            "an in-deck jump never re-roots the deck"
        );
        // The photo lands; ⌘→ again steps to charlie, and ⌘← steps back to bravo.
        core.displayed_item = Some(2);
        core.open_sibling_cmd(1);
        assert_eq!(core.target_item, Some(3), "→ charlie/4.png");
        core.displayed_item = Some(3);
        core.open_sibling_cmd(-1);
        assert_eq!(core.target_item, Some(2), "⌘← back to bravo");
        assert!(core.tree_io.is_none());
    }

    #[test]
    fn cmd_folder_traverses_by_boundaries_entering_subfolders() {
        let mut core = test_core();
        let base = std::env::temp_dir().join("pb_folder_traverse");
        // Deck order (case-insensitive path sort): a/1, a/2, a/sub/3, b/4.
        let paths = vec![
            base.join("a/1.png"),
            base.join("a/2.png"),
            base.join("a/sub/3.png"),
            base.join("b/4.png"),
        ];
        let source: Arc<dyn PhotoSource> = Arc::new(FsSource::new(paths));
        core.rebuild_playlist(source, base.clone(), Some(base.clone()), true, 0);
        // From a (index 0), ⌘→ enters the subfolder a/sub — the next different folder.
        core.open_sibling_cmd(1);
        assert_eq!(core.target_item, Some(2), "⌘→ enters a/sub");
        // From a/sub, ⌘→ climbs to b.
        core.displayed_item = Some(2);
        core.open_sibling_cmd(1);
        assert_eq!(core.target_item, Some(3), "⌘→ → b");
        // ⌘← from b returns to a/sub.
        core.displayed_item = Some(3);
        core.open_sibling_cmd(-1);
        assert_eq!(core.target_item, Some(2), "⌘← → a/sub");
        // ⌘← from a/sub returns to the *start* of the a run (index 0, not 1).
        core.displayed_item = Some(2);
        core.open_sibling_cmd(-1);
        assert_eq!(core.target_item, Some(0), "⌘← → start of the a run");
        assert!(core.tree_io.is_none(), "all in-deck — no disk worker");
    }

    #[test]
    fn open_parent_climbs_one_level_per_press_without_sticking() {
        let mut core = test_core();
        // Photos live deep (/base/a/b/c/*), and a, b, c have no direct photos — so every
        // re-root re-lands the current photo in c. A current-folder anchor would stick.
        let base = std::env::temp_dir().join("pb_climb_test");
        let deep = base.join("a/b/c");
        let deck = |root: PathBuf| -> (Arc<dyn PhotoSource>, PathBuf) {
            (Arc::new(FsSource::new(vec![deep.join("1.png")])), root)
        };
        let (src, root) = deck(deep.clone());
        core.rebuild_playlist(src, root.clone(), Some(root), true, 0);
        assert_eq!(core.current_folder_abs(), Some(deep.clone()));

        // The folder the most recent BeginDirScan targets.
        let scanned = |core: &AppCore| -> Option<PathBuf> {
            core.effects.iter().rev().find_map(|e| match e {
                contract::CoreEffect::BeginDirScan {
                    source: pb_core::open::Source::Scan { roots, .. },
                    ..
                } => roots.first().cloned(),
                _ => None,
            })
        };

        // ⌘↑ #1: up from the current folder /base/a/b/c → /base/a/b.
        core.effects.clear();
        core.open_parent_cmd();
        assert_eq!(scanned(&core), Some(base.join("a/b")));
        assert_eq!(core.climb_anchor, Some(base.join("a/b")));

        // The scan lands: the new deck (rooted at a/b) still lands the photo deep in c.
        let (src2, root2) = deck(base.join("a/b"));
        core.rebuild_playlist(src2, root2.clone(), Some(root2), true, 0);
        assert_eq!(
            core.current_folder_abs(),
            Some(deep.clone()),
            "photo still deep in c"
        );
        assert_eq!(
            core.climb_anchor,
            Some(base.join("a/b")),
            "the scan doesn't reset the climb"
        );

        // ⌘↑ #2: continues up from the climb anchor (a/b) → /base/a — NOT back down to c's parent.
        core.effects.clear();
        core.open_parent_cmd();
        assert_eq!(
            scanned(&core),
            Some(base.join("a")),
            "climbs, never oscillates"
        );
        assert_eq!(core.climb_anchor, Some(base.join("a")));

        // In-deck navigation ends the climb too (not just an explicit open): a stale rung
        // would surprise-jump ⌘↑ to a near-root folder after the user browsed elsewhere.
        assert_eq!(core.climb_anchor, Some(base.join("a")));
        core.advance(Nav::Forward);
        assert_eq!(core.climb_anchor, None, "a photo advance ends the climb");

        // And an explicit open resets it as well — the next ⌘↑ starts from the current folder.
        core.climb_anchor = Some(base.join("a"));
        core.open_plan(
            pb_core::open::Source::Explicit(vec![deep.join("1.png")]),
            pb_core::open::Cursor::First,
        );
        assert_eq!(core.climb_anchor, None, "an explicit open breaks the climb");
    }

    #[test]
    fn a_disk_rebuild_clears_the_archive_scope() {
        let mut core = archive_core(ARCHIVE);
        core.rescope_archive("a".to_string());
        assert!(core.archive_scope.is_some());
        let dir = std::env::temp_dir();
        let source: Arc<dyn PhotoSource> = Arc::new(FsSource::new(vec![dir.join("a.png")]));
        core.rebuild_playlist(source, dir.clone(), Some(dir), true, 0);
        assert!(
            core.archive_scope.is_none(),
            "a disk deck must not keep the old archive resident"
        );
    }

    #[test]
    fn os_theme_resolves_through_the_appearance_preference() {
        let mut core = test_core();
        // Default: System, dark until the shell reports (the pre-#46 look).
        assert!(core.effective_dark());
        assert_eq!(core.effective_letterbox(), core.settings.letterbox);

        // The OS flips to light → System follows, and refresh_theme tracks the flip.
        core.handle(CoreEvent::OsThemeChanged { dark: false });
        assert!(!core.effective_dark());
        assert!(!core.hud_dark, "refresh_theme applied the resolved theme");
        assert_eq!(core.effective_letterbox(), core.settings.letterbox_light);

        // Forced Light / Dark ignore the OS theme entirely.
        core.settings.appearance_mode = settings::AppearanceMode::Dark;
        assert!(core.effective_dark());
        assert_eq!(core.effective_letterbox(), core.settings.letterbox);
        core.settings.appearance_mode = settings::AppearanceMode::Light;
        core.os_dark = true;
        assert!(!core.effective_dark());

        // A redundant report is change-free (hud_dark only moves on a real flip).
        core.settings.appearance_mode = settings::AppearanceMode::System;
        core.refresh_theme();
        assert!(core.hud_dark);
        core.handle(CoreEvent::OsThemeChanged { dark: true });
        assert!(core.hud_dark);
    }

    #[test]
    fn tick_flushes_a_due_pending_delete() {
        // NS1 contract: a host that only drives `handle(Tick)` still gets the deferred
        // delete-advance — it must not depend on a shell-side flush (the winit shell's
        // was removed in favor of this core arm).
        let mut core = test_core();
        let t0 = core.now;
        core.pending_delete = Some((t0 + Duration::from_millis(200), 0));
        core.handle(CoreEvent::Tick(t0));
        assert!(
            core.pending_delete.is_some(),
            "before the deadline the delete must stay pending"
        );
        core.handle(CoreEvent::Tick(t0 + Duration::from_millis(200)));
        assert!(
            core.pending_delete.is_none(),
            "a Tick at/past the deadline must flush the pending delete"
        );
    }

    /// A headless core over an n-item deck of (nonexistent) temp paths — decode
    /// failures are tolerated everywhere off the hot path, and the compare tests
    /// assert on cursor/target/pin state, not on presentation.
    fn compare_core(n: usize) -> AppCore {
        let mut core = test_core();
        let dir = std::env::temp_dir();
        let paths: Vec<PathBuf> = (0..n).map(|i| dir.join(format!("cmp_{i}.png"))).collect();
        let source: Arc<dyn PhotoSource> = Arc::new(FsSource::new(paths));
        core.rebuild_playlist(source, dir.clone(), Some(dir), true, 0);
        core
    }

    /// Move the cursor to `i` and mark it settled (headless: no decode ever lands,
    /// so the `displayed == target` gate is satisfied by hand).
    fn settle_at(core: &mut AppCore, i: usize) {
        core.playlist.jump_to(i);
        core.target_item = Some(i);
        core.displayed_item = Some(i);
    }

    #[test]
    fn compare_toggle_pins_first_then_flips_and_returns() {
        let mut core = compare_core(5);
        // First Y with nothing pinned: pins the current photo, no navigation.
        core.dispatch_action(Action::CompareToggle);
        assert_eq!(core.compare_pin, Some(0));
        assert_eq!(core.target_item, Some(0), "pinning must not navigate");
        // Browse to 3, then Y: flips to the pin, remembering where we were.
        settle_at(&mut core, 3);
        core.dispatch_action(Action::CompareToggle);
        assert_eq!(core.target_item, Some(0), "Y flips to the pin");
        assert_eq!(core.compare_return, Some(3));
        // Y again from the pin: returns to the remembered position.
        core.displayed_item = core.target_item;
        core.dispatch_action(Action::CompareToggle);
        assert_eq!(core.target_item, Some(3), "Y on the pin returns");
        assert_eq!(core.compare_pin, Some(0), "the pin itself stays fixed");
    }

    #[test]
    fn compare_pin_moves_and_unpins() {
        let mut core = compare_core(4);
        core.dispatch_action(Action::ComparePin);
        assert_eq!(core.compare_pin, Some(0));
        // ⇧Y elsewhere moves the pin (and resets the return point).
        settle_at(&mut core, 2);
        core.compare_return = Some(1);
        core.dispatch_action(Action::ComparePin);
        assert_eq!(core.compare_pin, Some(2));
        assert_eq!(core.compare_return, None, "re-pin resets the return point");
        // ⇧Y on the pinned photo unpins.
        core.dispatch_action(Action::ComparePin);
        assert_eq!(core.compare_pin, None);
        assert_eq!(core.compare_pin_id, None);
    }

    #[test]
    fn compare_flip_never_interrupts_a_pending_target() {
        let mut core = compare_core(5);
        core.dispatch_action(Action::CompareToggle); // pin = 0
                                                     // The launch decode of (nonexistent) cmp_0 failed, and a failed target
                                                     // auto-settles via `present_failed` — clear it so the flip is a genuine
                                                     // ring MISS that stays pending, which is what this test is about.
        core.failed.clear();
        settle_at(&mut core, 3);
        core.dispatch_action(Action::CompareToggle); // flip to the pin...
        assert_eq!(core.target_item, Some(0));
        // ...but the present hasn't landed (displayed still 3). A second Y must not
        // clobber the in-flight target (mirrors `advance`'s never-skip gate).
        core.dispatch_action(Action::CompareToggle);
        assert_eq!(core.target_item, Some(0));
        assert_eq!(core.displayed_item, Some(3));
    }

    #[test]
    fn compare_pin_survives_a_same_deck_rebuild_and_clears_on_a_new_deck() {
        let dir = std::env::temp_dir();
        let mut core = compare_core(4);
        settle_at(&mut core, 2);
        core.dispatch_action(Action::ComparePin); // pin = cmp_2 at index 2
                                                  // The delete-advance shape: same paths minus cmp_1 → cmp_2 shifts to index 1.
        let remaining: Vec<PathBuf> = [0usize, 2, 3]
            .iter()
            .map(|i| dir.join(format!("cmp_{i}.png")))
            .collect();
        let src: Arc<dyn PhotoSource> = Arc::new(FsSource::new(remaining));
        core.rebuild_playlist(src, dir.clone(), Some(dir.clone()), true, 0);
        assert_eq!(
            core.compare_pin,
            Some(1),
            "the pin re-resolves by path across a same-deck rebuild"
        );
        assert_eq!(core.compare_return, None, "the return point never survives");
        // A genuinely new deck has no matching identity — the pin clears.
        let other: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("other_{i}.png"))).collect();
        let src: Arc<dyn PhotoSource> = Arc::new(FsSource::new(other));
        core.rebuild_playlist(src, dir.clone(), Some(dir), true, 0);
        assert_eq!(core.compare_pin, None);
        assert_eq!(core.compare_pin_id, None);
    }

    #[test]
    fn compare_pin_rides_the_prefetch_want_list_at_top_two() {
        let mut core = compare_core(50);
        core.dispatch_action(Action::ComparePin); // pin = 0
        settle_at(&mut core, 40);
        core.request_prefetch();
        assert_eq!(
            core.targets.first(),
            Some(&40),
            "current target stays first"
        );
        assert_eq!(
            core.targets.get(1),
            Some(&0),
            "the pin rides at top-2 priority so eviction can never drop it"
        );
        assert_eq!(
            core.targets.iter().filter(|&&t| t == 0).count(),
            1,
            "the pin appears exactly once"
        );
    }

    #[test]
    fn deleting_down_to_the_empty_state_clears_the_pin() {
        let mut core = compare_core(1);
        core.dispatch_action(Action::ComparePin);
        assert!(core.compare_pin.is_some());
        core.enter_empty_state();
        assert_eq!(core.compare_pin, None);
        assert_eq!(core.compare_pin_id, None);
    }

    #[test]
    fn compare_carry_applies_only_to_matching_geometry() {
        use crate::meta::PhotoMeta;
        let meta = |w: u32, h: u32| PhotoMeta {
            rel: String::new(),
            w,
            h,
            codec: "PNG",
            animated: None,
        };
        let mut core = compare_core(3);
        core.meta_cache.insert(0, meta(100, 80));
        core.meta_cache.insert(2, meta(100, 80));
        settle_at(&mut core, 2);
        core.view.zoom = 3.0;
        core.view.pan = [10.0, -4.0];
        assert_eq!(
            core.compare_carry_view(0),
            Some((3.0, [10.0, -4.0])),
            "same dims + same rotation → the crop carries"
        );
        // A rotation override on one side breaks the mapping.
        core.rotations.insert(0, Rotation::default().cw());
        assert_eq!(core.compare_carry_view(0), None);
        core.rotations.clear();
        // Dimension mismatch → no carry.
        core.meta_cache.insert(0, meta(99, 80));
        assert_eq!(core.compare_carry_view(0), None);
        // Default view → nothing worth carrying.
        core.meta_cache.insert(0, meta(100, 80));
        core.view.zoom = 1.0;
        core.view.pan = [0.0, 0.0];
        assert_eq!(core.compare_carry_view(0), None);
    }

    #[test]
    fn compare_carry_is_staged_for_the_flips_first_frame_and_is_one_shot() {
        // The owner-reported flicker: presenting at the reset view and re-imposing the
        // carry afterwards flashed the incoming photo centered for one frame. The carry
        // is now staged for `view_for` to consume, so the FIRST present lands
        // positioned — and it must be one-shot, never leaking into a later present.
        use crate::meta::PhotoMeta;
        let meta = |w: u32, h: u32| PhotoMeta {
            rel: String::new(),
            w,
            h,
            codec: "PNG",
            animated: None,
        };
        let mut core = compare_core(3);
        core.meta_cache.insert(0, meta(100, 80));
        core.meta_cache.insert(2, meta(100, 80));
        core.dispatch_action(Action::CompareToggle); // pin = 0
        core.failed.clear(); // cmp_0's launch decode failed; make the flip a clean MISS
        settle_at(&mut core, 2);
        core.view.zoom = 2.0;
        core.view.pan = [5.0, 6.0];
        core.dispatch_action(Action::CompareToggle); // flip stages the carry...
                                                     // ...but headless the present missed (no ring): the stash must be dropped so a
                                                     // later unrelated present resets instead of inheriting a stale view.
        assert_eq!(core.compare_carry, None);
        let v = core.view_for(2);
        assert_eq!((v.zoom, v.pan), (1.0, [0.0, 0.0]));
        // A staged carry is consumed by exactly ONE view_for (the flip's present).
        core.compare_carry = Some((2.0, [5.0, 6.0]));
        let v = core.view_for(0);
        assert_eq!((v.zoom, v.pan), (2.0, [5.0, 6.0]), "first frame carries");
        let v = core.view_for(0);
        assert_eq!((v.zoom, v.pan), (1.0, [0.0, 0.0]), "the carry is one-shot");
    }

    #[test]
    fn nav_press_stamps_hold_start_from_the_injected_clock() {
        // The core never reads the wall clock (NS0 0.3): timing state is stamped from
        // the injected `self.now`, so a host/test driving synthetic time stays coherent
        // (hold-to-fly gates against the same clock the Tick events carry).
        let mut core = test_core();
        let t = core.now + Duration::from_secs(1000);
        core.now = t;
        core.nav_press(PbKey::Space, Action::Next);
        assert_eq!(
            core.hold_start,
            Some(t),
            "hold_start must come from the injected clock, not Instant::now()"
        );
    }

    #[test]
    fn menu_flow_action_routes_through_shell_flow_effect() {
        // `Quit` is a still-shell flow action (window teardown) — it routes through the
        // catch-all `ShellFlowAction` seam. (Settings/About/Mute have been inverted onto their
        // own effects; see the tests below.)
        let mut core = test_core();
        core.handle(CoreEvent::MenuAction(Action::Quit));
        assert_eq!(core.effects.len(), 1);
        assert!(matches!(
            core.effects[0],
            contract::CoreEffect::ShellFlowAction(Action::Quit)
        ));
    }

    #[test]
    fn settings_action_emits_show_dialog_effect() {
        // NS0 5.6: About/Settings dispatch a payload-free `ShowDialog` effect, not ShellFlowAction.
        // (Mute isn't unit-tested here — its arm calls `Settings::save`, which writes the real
        // on-disk config; it's covered by the shell smoke instead.)
        let mut core = test_core();
        core.handle(CoreEvent::MenuAction(Action::Settings));
        assert_eq!(core.effects.len(), 1);
        assert!(matches!(
            core.effects[0],
            contract::CoreEffect::ShowDialog(contract::DialogKind::Settings)
        ));
    }

    #[test]
    fn dialog_closed_emits_close_only() {
        // NS0 5.6: a plain Message/OK close just emits CloseDialog — nothing else.
        let mut core = test_core();
        core.handle(CoreEvent::DialogResolved(contract::DialogResult::Closed));
        assert_eq!(core.effects.len(), 1);
        assert!(matches!(core.effects[0], contract::CoreEffect::CloseDialog));
    }

    #[test]
    fn dialog_dismiss_scanning_cancels_scan_and_archive_then_closes() {
        // Dismissing the Scanning dialog cancels the scan (+ the always-safe archive cancel),
        // closes, and clears the pending confirm/password — the Esc-guard is armed too.
        let mut core = test_core();
        core.pending_confirm_delete = Some(3);
        core.password_archive = Some(std::path::PathBuf::from("/tmp/a.zip"));
        core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::Dismissed(Some(contract::DialogKind::Scanning)),
        ));
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::CancelScan)));
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::CancelArchiveLoad)));
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::CloseDialog)));
        assert_eq!(core.pending_confirm_delete, None);
        assert_eq!(core.password_archive, None);
        assert!(core.esc_guard_until.is_some());
    }

    #[test]
    fn dialog_dismiss_non_scanning_does_not_cancel_scan() {
        // Closing a *different* dialog must not kill a scan running quietly in the background.
        let mut core = test_core();
        core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::Dismissed(Some(contract::DialogKind::About)),
        ));
        assert!(!core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::CancelScan)));
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::CloseDialog)));
    }

    #[test]
    fn confirm_answered_no_takes_pending_and_closes_without_delete() {
        // A "No"/cancel on the delete-confirm clears the pending item and closes — no delete.
        let mut core = test_core();
        core.pending_confirm_delete = Some(7);
        core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::ConfirmAnswered(false),
        ));
        assert_eq!(core.pending_confirm_delete, None);
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::CloseDialog)));
    }

    #[test]
    fn menu_pure_action_runs_in_core_without_a_flow_effect() {
        let mut core = test_core();
        core.handle(CoreEvent::MenuAction(Action::ScaleFill));
        // A pure arm runs in the core (mode flips) and never emits a ShellFlowAction.
        assert_eq!(core.view.mode, ScaleMode::Fill);
        assert!(!core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::ShellFlowAction(_))));
    }

    #[test]
    fn focus_lost_clears_held_keys_and_gesture_state() {
        let mut core = test_core();
        core.held.insert(PbKey::ArrowLeft, Action::PanLeft);
        core.dragging = true;
        core.hold_start = Some(core.now);
        core.handle(CoreEvent::FocusLost);
        assert!(core.held.is_empty());
        assert!(!core.dragging);
        assert!(core.hold_start.is_none());
        assert_eq!(core.mods, Modifiers::NONE);
    }

    #[test]
    fn key_up_releases_only_that_key() {
        let mut core = test_core();
        core.held.insert(PbKey::ArrowLeft, Action::PanLeft);
        core.held.insert(PbKey::ArrowRight, Action::PanRight);
        core.handle(CoreEvent::KeyUp {
            key: PbKey::ArrowLeft,
        });
        assert!(!core.held.contains_key(&PbKey::ArrowLeft));
        assert!(core.held.contains_key(&PbKey::ArrowRight));
    }

    #[test]
    fn pointer_nav_is_a_second_hold_to_fly_source() {
        let mut core = test_core();
        // A held toolbar nav button makes `held_nav` report a direction, exactly as a held
        // key would — that's what drives the self-paced advance each tick.
        assert!(core.held_nav().is_none());
        core.pointer_nav = Some(Action::Next);
        assert!(core.held_nav().is_some());
        // A key held the SAME direction is still that direction (not "two → idle").
        core.held.insert(PbKey::Space, Action::Next);
        assert!(core.held_nav().is_some());
        // The OPPOSITE direction held at the same time is idle (ambiguous) — same rule as two
        // keys held in opposite directions.
        core.held.clear();
        core.held.insert(PbKey::Backspace, Action::Prev);
        assert!(core.held_nav().is_none());
        // Release + the focus-loss safety net both clear the pointer hold.
        core.held.clear();
        core.end_pointer_nav();
        assert_eq!(core.pointer_nav, None);
        core.pointer_nav = Some(Action::Random);
        core.handle(CoreEvent::FocusLost);
        assert_eq!(core.pointer_nav, None);
    }

    #[test]
    fn os_key_repeat_is_ignored() {
        let mut core = test_core();
        // An OS auto-repeat (`repeat: true`) resolves to `Ignore` regardless of binding, so it
        // touches no state and emits no effect (the hold loop drives fly-speed, not repeats).
        core.handle(CoreEvent::KeyDown {
            key: PbKey::Space,
            mods: Modifiers::NONE,
            repeat: true,
        });
        assert!(core.held.is_empty());
        assert!(core.effects.is_empty());
    }
}
