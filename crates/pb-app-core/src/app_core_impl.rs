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
use crate::pb_key::PbKey;
use crate::{
    keymap, settings, slideshow, timing, Action, AppCore, InfoMode, Nav, OpenButton, OpenPanel,
    PlayHint, Toast, UndoAction,
};

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
        AppCore {
            now: Instant::now(),
            viewport,
            held: std::collections::HashMap::new(),
            last_present: None,
            frame_interval: Duration::from_micros(8_333),
            hold_start: None,
            initial_delay: Duration::from_millis(120),
            slideshow: slideshow::Slideshow::default(),
            mods: contract::Modifiers::NONE,
            esc_guard_until: None,
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
            playlist: Playlist::new(0, 0),
            targets: Vec::new(),
            last_nav: Nav::Forward,
            displayed_item: None,
            target_item: None,
            epoch: 1,
            root: PathBuf::new(),
            scan_root: None,
            recursive: false,
            scanning: false,
            launching: false,
            dialog_open: false,
            archive_loading: false,
            scan_bootstrapped: false,
            password_archive: None,
            pending_delete: None,
            pending_confirm_delete: None,
            info: InfoMode::Off,
            overlay_shown: false,
            overlay_item: None,
            toast: None,
            wait_started: None,
            pie_finish: None,
            pie_glow_started: None,
            decode_ewma: 0.25,
            pie_drawn: false,
            pie_pushed: None,
            chip_sig: None,
            chip_built: Instant::now(),
            chip_rect: None,
            chip_hovered: false,
            open_panel: None,
            open_hover: None,
            play_hint: None,
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
            settings: settings::Settings::default(),
            effects: Vec::new(),
        }
    }

    /// Whether prefetch/upload work is still outstanding (keep polling if so).
    pub fn work_pending(&self) -> bool {
        self.archive_loading
            // An off-thread animation decode in flight keeps the loop polling so
            // `poll_anim_decode` picks it up promptly (active playback drives its own
            // precise next-frame wake via `tick_playback`, not this frame poll).
            || self.anim_decode.is_some()
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
            Action::Copy => self.copy_image(),
            Action::CopyPath => self.copy_path(),
            Action::CopyImageDetails => self.copy_image_details(),
            Action::RevealInFileManager => self.reveal_in_file_manager(),
            Action::OpenFile => self.open_picker(false),
            Action::OpenFolder => self.open_picker(true),
            Action::Info => self.toggle_info(false),
            Action::FullExif => self.toggle_info(true),
            Action::Help => self.toggle_help(),
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
                    self.show_toast_icon("", Some(icon::assets::VOLUME_SLASH));
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
                    self.show_toast_icon("", Some(icon::assets::VOLUME));
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
                self.show_toast_icon("", Some(icon::assets::FLOPPY));
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
                        self.show_toast_icon("Rotation undone", Some(icon::assets::UNDO));
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
            icon::assets::TRASH
        } else {
            icon::assets::RECYCLE
        };
        self.show_toast_icon("", Some(icon));
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
            CoreEvent::FocusLost => {
                self.held.clear();
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
            // Pointer moved: drag-to-pan (while the button is held) + refresh the on-image hover
            // state (chip / open-panel buttons / play hint) + the cursor shape.
            CoreEvent::PointerMoved { x, y } => {
                let p = [x, y];
                if self.dragging {
                    if let Some(prev) = self.last_cursor {
                        self.pan_by_pixels(p[0] - prev[0], p[1] - prev[1]);
                    }
                }
                self.last_cursor = Some(p);
                self.update_chip_hover();
                self.update_open_hover();
                self.update_play_hint_hover();
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
            // Settings: Save applies + persists the edited model; Cancel/Esc discard.
            R::SettingsSaved { settings, keymap } => {
                self.effects.push(E::CloseDialog);
                if let Some(new) = settings {
                    self.apply_settings(new);
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

    /// Install a resolved archive playlist ([`CoreEvent::ArchiveResolved`], NS0 5.6 Step 3): the
    /// open succeeded with a non-empty deck, so rebuild the playlist onto it and forget any pending
    /// password. The host closes the loading/password dialog (like the scan's Done); the failure
    /// cases (empty / password-required / cancelled / error) stay host-side.
    fn apply_archive(&mut self, resolved: crate::scan::Resolved) {
        self.password_archive = None;
        self.rebuild_playlist(
            resolved.source,
            resolved.root,
            resolved.scan_root,
            resolved.recursive,
            resolved.start,
        );
    }

    /// Route an opened source (NS0 5.6 Step 3c) — the entry point the picker / drag-drop / a
    /// deferred launch funnel through. An **archive** or a **folder scan** starts its off-thread
    /// worker via an effect (`BeginArchiveOpen` / `BeginDirScan`; the host owns the thread +
    /// progress dialog + generation, and feeds results back as `ArchiveResolved` / `ScanBatch`).
    /// A finite **explicit list** has no directory walk, so it resolves inline and installs now.
    pub fn open_plan(&mut self, source: pb_core::open::Source, cursor: pb_core::open::Cursor) {
        use pb_core::open::Source;
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
        // 1. Absorb finished decodes (uploads; presents the target if it arrived).
        self.drain_results();

        // 1b. Pick up a finished off-thread animation decode (kicked by `P` / frame-step) and
        // install playback — never on the still/keypress hot path (#37).
        self.poll_anim_decode();

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
        if self.info != InfoMode::Off {
            if flying {
                if self.overlay_shown {
                    self.hide_overlay();
                }
            } else if !transforming
                // Help is static; the info panels need a photo.
                && (self.info == InfoMode::Help || self.current.is_some())
                && (!self.overlay_shown || self.overlay_item != self.displayed_item)
            {
                self.show_overlay();
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

    /// Build the [`contract::MenuState`] for the given live state — the pure mapping from
    /// the app's view/edit state to the shell-neutral menu model. Takes no `self` and
    /// touches no muda, so it's unit-tested directly (`menu_state_*` tests). The two enum
    /// mappings it owns are the only non-trivial logic: `pb_render::ScaleMode` → the View
    /// scale group, and the 4-state [`InfoMode`] → the two info checkmarks (both `Help`
    /// and `Off` show *neither*, exactly as the menu does today).
    #[allow(clippy::too_many_arguments)]
    pub fn menu_state_from(
        scale: ScaleMode,
        info: InfoMode,
        recursive: bool,
        fullscreen: bool,
        slideshow: bool,
        mute_live_audio: bool,
        save_rotation_enabled: bool,
        reveal_enabled: bool,
        cancel_scan_enabled: bool,
        undo: Option<&'static str>,
        native_fullscreen_engaged: bool,
    ) -> contract::MenuState {
        contract::MenuState {
            scale: match scale {
                ScaleMode::Fit => contract::ScaleMode::Fit,
                ScaleMode::Fill => contract::ScaleMode::Fill,
                ScaleMode::Original => contract::ScaleMode::Original,
            },
            info: match info {
                InfoMode::Basic => contract::InfoOverlay::Basic,
                InfoMode::Full => contract::InfoOverlay::FullExif,
                // Help and Off both leave the two info checkmarks off (the menu can't
                // distinguish them — this is the faithful collapse of the 4-state enum).
                InfoMode::Help | InfoMode::Off => contract::InfoOverlay::Hidden,
            },
            recursive,
            fullscreen,
            slideshow,
            mute_live_audio,
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

    /// Toggle the keybindings help overlay (`/` or `?`). Shares the single overlay
    /// with the info panels, so it replaces whichever was showing.
    pub fn toggle_help(&mut self) {
        self.info = if self.info == InfoMode::Help {
            InfoMode::Off
        } else {
            InfoMode::Help
        };
        if self.info == InfoMode::Off {
            self.hide_overlay();
        } else {
            self.show_overlay();
        }
    }

    /// Apply the keymap edited in the Settings dialog: swap it in live (every keypress
    /// resolves through `self.keymap`, so future input uses it immediately) and persist
    /// `keymap.toml`. If the help overlay is open, rebuild it so its key labels — read
    /// from the live keymap — reflect the new bindings.
    pub fn apply_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
        self.keymap.save();
        if self.overlay_shown && self.info == InfoMode::Help {
            self.show_overlay();
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
        self.playlist = Playlist::new(0, 0);
        self.rotations.clear();
        self.meta_cache.clear();
        self.exif_cache.clear();
        self.live_motion_cache.clear();
        self.failed.clear();
        self.preview_resident.clear();
        self.upgrade_done.clear();
        self.last_upgrade_set.clear();
        self.undo_stack.clear();
        self.invalidate_geometry();
        self.displayed_item = None;
        self.target_item = None;
        self.current = None;
        if let Some(r) = self.renderer.as_mut() {
            r.clear_image();
            r.set_overlay(None, 0);
        }
        self.effects
            .push(contract::CoreEffect::SetTitle("PhotoBlaze".to_string()));
        // Blank background + the centered "Press O to open…" hint (mirrors a bare launch).
        self.show_open_hint();
        self.overlay_shown = false;
        self.overlay_item = None;
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
        self.source = source;
        self.root = root;
        self.scan_root = scan_root;
        self.recursive = recursive;
        self.playlist = Playlist::new(self.source.len(), 0).with_cursor(start);
        // Indices are reassigned — drop everything keyed by item index.
        self.rotations.clear();
        self.meta_cache.clear();
        self.exif_cache.clear();
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

    /// Show the empty-state open panel over the (blank) viewer — used when there are no images
    /// to display. Rebuilt against the current scale + hover; the renderer re-centers it on
    /// resize and drops it the moment a photo is shown. Records the button rects for hit-testing.
    pub fn show_open_hint(&mut self) {
        // Suppress the panel while a folder scan is pending (deferred startup launch) or
        // streaming in — the first photo is about to bootstrap, so the call to action would
        // flash briefly and mislead (it implies nothing is loading). If the scan turns out
        // empty, `poll_dir_scan`'s Done arm restores it.
        if self.scanning || self.launching {
            return;
        }
        let Some((bitmap, w, h, file, folder)) = self.open_panel_bitmap() else {
            self.open_panel = None;
            return;
        };
        self.open_panel = Some(OpenPanel { w, h, file, folder });
        if let Some(a) = self.renderer.as_mut() {
            a.set_message(Some((&bitmap, w, h)));
        }
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
        if self.source.is_empty() {
            self.show_open_hint(); // re-rasterize the "Press O to open" hint
        }
        self.effects.push(contract::CoreEffect::RequestRender);
    }

    /// Whether the empty-state open panel is the active overlay, so its buttons are
    /// hit-testable: the panel is built, no photo is loaded, and no scan is pending/streaming
    /// (which would suppress it). Mirrors [`show_open_hint`]'s show condition.
    ///
    /// [`show_open_hint`]: App::show_open_hint
    pub fn open_hint_active(&self) -> bool {
        self.open_panel.is_some() && self.source.is_empty() && !self.scanning && !self.launching
    }

    /// The on-screen `[x0, y0, x1, y1]` rect (physical px) of an open-panel button, derived
    /// from the live window size — the renderer centers the message layer, so this stays
    /// correct across resizes and DPI changes without re-storing absolute coordinates. `None`
    /// unless the open panel is the active overlay.
    pub fn open_button_rect(&self, which: OpenButton) -> Option<[f32; 4]> {
        if !self.open_hint_active() {
            return None;
        }
        let panel = self.open_panel?;
        let sz = self.viewport;
        // Same centering the renderer applies (see `center_quad_vertices`): clamped to ≥ 0.
        let x0 = ((sz.width as f32 - panel.w as f32) * 0.5).max(0.0);
        let y0 = ((sz.height as f32 - panel.h as f32) * 0.5).max(0.0);
        let [bx, by, bw, bh] = match which {
            OpenButton::File => panel.file,
            OpenButton::Folder => panel.folder,
        }
        .map(|v| v as f32);
        Some([x0 + bx, y0 + by, x0 + bx + bw, y0 + by + bh])
    }

    /// Which open-panel button (if any) the pointer is currently over.
    pub fn open_hovered_button(&self) -> Option<OpenButton> {
        let [x, y] = self.last_cursor?;
        [OpenButton::File, OpenButton::Folder]
            .into_iter()
            .find(|&b| {
                self.open_button_rect(b)
                    .is_some_and(|r| point_in_rect(r, x, y))
            })
    }

    /// Update the open panel's hover "lit" state from the latest cursor position, re-rendering
    /// the panel only when hover **changes** (one CPU composite on the enter/leave transition,
    /// never per move). Mirrors [`update_chip_hover`] for the empty-state call to action.
    ///
    /// [`update_chip_hover`]: App::update_chip_hover
    pub fn update_open_hover(&mut self) {
        if !self.open_hint_active() {
            return;
        }
        let hovered = self.open_hovered_button();
        if hovered == self.open_hover {
            return;
        }
        self.open_hover = hovered;
        // Rebuild the panel with the new lit button (content is otherwise unchanged), re-upload
        // it, and present so the change shows immediately.
        self.show_open_hint();
        self.draw();
    }

    /// Handle a nav keypress (space / backspace / enter). Tracks the held key for
    /// hold-to-fly, then either advances, or — when we're still catching up to the
    /// previous target, so the press can't be serviced yet — flashes the loading
    /// pie (brighten-on-keypress) so the input never feels dead.
    pub fn nav_press(&mut self, key: PbKey, action: Action) {
        self.held.insert(key, action);
        self.hold_start = Some(Instant::now());
        let Some(nav) = nav_of(action) else {
            return;
        };
        if self.target_item.is_some() && self.displayed_item != self.target_item {
            self.pie_glow_started = Some(Instant::now());
        } else {
            self.advance(nav);
        }
    }

    /// Advance one photo (sequential or random). The gated engine path: present on
    /// a ring hit, else hold the previous frame + prefetch while the decode lands.
    pub fn advance(&mut self, nav: Nav) {
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

    /// Reflect the pan affordance in the pointer: a pointing hand over the Cancel Scan button,
    /// a closed hand while dragging, an open hand when the image is pannable, the default arrow
    /// otherwise.
    pub fn refresh_cursor(&mut self) {
        let over_button = self.last_cursor.is_some_and(|[x, y]| self.chip_hit(x, y))
            || self.open_hovered_button().is_some()
            || self.play_hint_hit();
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
            // A Live Photo gets the livephoto mark; an animated still gets the play ▶. Styled
            // like the open-screen buttons: `▶ Play  P` with the shortcut dimmed at the right —
            // and it's a real button: hovering holds it open, a click plays (see the click /
            // hover wiring). Starts unhovered.
            let icon = if self.is_live_photo(item) {
                icon::assets::LIVE_PHOTO
            } else {
                icon::assets::PLAY
            };
            if let Some((w, h)) = self.build_play_hint(icon, false) {
                self.play_hint = Some(PlayHint {
                    w,
                    h,
                    icon,
                    hovered: false,
                });
            }
            self.draw();
            // If the pointer already happens to sit over it, light it up (and hold it) at once.
            self.update_play_hint_hover();
            self.refresh_cursor();
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
        let now = Instant::now();
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
                    if self.overlay_shown && self.info == InfoMode::Full {
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
        self.anim_decode = None;
        self.prepared = None;
        self.framestep_started = None;
        self.framestep_last = None;
        self.live_revert_at = None;
        self.effects.push(contract::CoreEffect::StopLiveAudio); // dropping the player stops it
                                                                // Leaving the animated still: drop the interactive play hint AND its toast **immediately**
                                                                // (not a fade), so a stale/irrelevant button never lingers over the next photo — which
                                                                // could even be a non-animated one. Only the play hint owns the toast here; a passive
                                                                // status toast (Copy/Save/…) has `play_hint == None` and is left to fade normally.
        if self.play_hint.take().is_some() {
            self.toast = None;
            if let Some(a) = self.renderer.as_mut() {
                a.set_toast(None, 0);
            }
            self.draw();
        }
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

    /// Toggle an info panel: the one-line basic panel with `i`, or the full-EXIF
    /// "nerd" table with `Shift+I`. Selecting the mode that's already showing hides
    /// it. When shown it appears immediately (idle); after navigation it reappears
    /// once you stop (see `about_to_wait`).
    pub fn toggle_info(&mut self, full: bool) {
        let target = if full {
            InfoMode::Full
        } else {
            InfoMode::Basic
        };
        self.info = if self.info == target {
            InfoMode::Off
        } else {
            target
        };
        if self.info == InfoMode::Off {
            self.hide_overlay();
        } else {
            self.show_overlay();
        }
    }

    /// Apply the settings the user saved in the dialog: swap in the new model, apply
    /// the parts that aren't read live (hold delay, letterbox color, default scale
    /// mode), then persist to disk (an explicit user action — privacy #2). The nav-feel
    /// rates (start speed / ramp / max) and the info-panel opacity are read live, so
    /// swapping `self.settings` is enough for those.
    pub fn apply_settings(&mut self, new: settings::Settings) {
        let old = std::mem::replace(&mut self.settings, new);
        let s = &self.settings;

        // Held-key repeat delay is cached on the struct (the curve below reads the
        // rates live, but this one is a Duration captured at construction).
        self.initial_delay = Duration::from_millis(s.hold_delay_ms as u64);

        // Default slideshow interval → the live timer. A running slideshow's deadline is
        // `last_present + interval`, recomputed each tick, so this takes effect at once
        // (the `[`/`]` live override is just a different write to the same field).
        self.slideshow.interval = Duration::from_secs_f64(s.slideshow_interval_secs);

        // Letterbox / background fill → renderer (takes effect on the next draw).
        if let Some(a) = self.renderer.as_mut() {
            a.set_letterbox(s.letterbox);
        }

        // Default scale mode: apply live if it changed (re-frames + reloads at the new
        // fit). `set_scale_mode` redraws for us.
        let scale_changed = old.scale_mode != s.scale_mode;
        if scale_changed {
            self.set_scale_mode(scale_mode_of(s.scale_mode));
        }

        // Persist the whole model (atomic write; best-effort).
        self.settings.save();

        // Redraw so the new letterbox shows even when the scale mode didn't change,
        // and rebuild the info panel so a new opacity takes effect immediately.
        if self.overlay_shown {
            self.show_overlay();
        } else if !scale_changed {
            self.draw();
        }
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
        if let Some(chord) = keymap::macos_menu_chord(action) {
            return chord.mac_symbol();
        }
        self.keymap
            .bindings_for(action)
            .iter()
            .find(|c| !c.code.is_numpad())
            .map(|c| c.shortcut_label())
            .unwrap_or_default()
    }

    /// The keyboard-help overlay as grouped sections (description + shortcut), sourced from the
    /// live keymap / menu so customized bindings and platform symbols stay correct. Drives
    /// [`hud::Hud::render_shortcuts`].
    pub fn help_sections(&self) -> Vec<hud::ShortcutSection> {
        let sc = |a: Action| self.help_shortcut(a);
        let two =
            |a: Action, b: Action| format!("{} / {}", self.help_shortcut(a), self.help_shortcut(b));
        let row = |desc: &str, shortcut: String| (desc.to_string(), shortcut);
        // Platform wording for the trash action (the shortcut itself comes from `help_shortcut`).
        #[cfg(target_os = "macos")]
        let trash = "Move to Trash";
        #[cfg(not(target_os = "macos"))]
        let trash = "Delete to Recycle Bin";

        let section = |title: &str, rows: Vec<(String, String)>| hud::ShortcutSection {
            title: title.to_string(),
            rows,
        };
        vec![
            section(
                "Browse",
                vec![
                    row("Next photo", sc(Action::Next)),
                    row("Previous photo", sc(Action::Prev)),
                    row("Random photo", sc(Action::Random)),
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
                    row("Fullscreen", sc(Action::Fullscreen)),
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
                    row("Open file", sc(Action::OpenFile)),
                    row("Open folder", sc(Action::OpenFolder)),
                    row("Copy image", sc(Action::Copy)),
                    row("Copy file path", sc(Action::CopyPath)),
                    row("Save rotation", sc(Action::SaveRotation)),
                    row("Undo", sc(Action::Undo)),
                    row(trash, sc(Action::Delete)),
                    row("Delete permanently", sc(Action::DeletePermanent)),
                    row("Recursive (this folder)", sc(Action::Recursive)),
                    row("Info panel", sc(Action::Info)),
                    row("Full EXIF panel", sc(Action::FullExif)),
                    row("Settings", sc(Action::Settings)),
                    row("Quit", sc(Action::Quit)),
                ],
            ),
        ]
    }

    /// Rasterize the active overlay (info panel or help) and draw it. The help
    /// overlay uses a larger font than the info panels.
    pub fn show_overlay(&mut self) {
        let px = (15.0 * self.viewport.scale_factor).max(8.0);
        let pad = (7.0 * self.viewport.scale_factor).round().max(2.0) as u32;
        // The info / EXIF panels honor the user's opacity setting; the help overlay
        // keeps the standard translucency.
        let info_bg = hud::bg_for_opacity(self.settings.info_opacity);
        // Resolve the Live Photo pairing (cached; one stat) up front so either panel can
        // label it — the basic line and the detailed table both read `is_live_photo`.
        if let Some(item) = self.displayed_item {
            self.live_motion_path(item);
        }
        let is_live = self.displayed_item.is_some_and(|i| self.is_live_photo(i));
        let panel = match self.info {
            InfoMode::Off => return,
            InfoMode::Basic => {
                let (Some(hud), Some(meta)) = (self.hud.as_ref(), self.current.as_ref()) else {
                    return;
                };
                let mut text = format!("{} · {}×{} · {}", meta.rel, meta.w, meta.h, meta.codec);
                if is_live {
                    text.push_str(" · Live"); // a Live Photo's motion is playable (P)
                }
                hud.render_panel(&text, px, pad, info_bg)
            }
            InfoMode::Full => {
                // Warm the EXIF read once so the table build (and its per-frame rebuilds
                // during playback) never re-read the file.
                if let Some(item) = self.displayed_item {
                    self.ensure_exif_cached(item);
                }
                let rows = self.exif_rows();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                if rows.is_empty() {
                    return;
                }
                hud.render_table(&rows, px, pad, info_bg)
            }
            InfoMode::Help => {
                let help_px = (15.0 * self.viewport.scale_factor).max(10.0);
                let sections = self.help_sections();
                let margin = self.overlay_margin();
                let win_h = self.viewport.height;
                let max_h = (win_h as i32 - 2 * margin as i32).max(1);
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                hud.render_shortcuts(&sections, help_px, hud::BG, max_h)
            }
        };
        let Some((bitmap, w, h)) = panel else {
            return;
        };
        let margin = self.overlay_margin();
        if let Some(a) = self.renderer.as_mut() {
            a.set_overlay(Some((&bitmap, w, h)), margin);
        }
        self.overlay_shown = true;
        self.overlay_item = self.displayed_item;
        self.draw();
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
        self.anim_frame_shown_at = Some(Instant::now());
        // Keep a shown detailed-EXIF panel's live "Frame X / N" in sync as the frame
        // changes. Off the hot path (only during user-engaged playback/stepping), and
        // the EXIF read is memoized so this never re-reads the file per frame.
        if self.overlay_shown && self.info == InfoMode::Full {
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
    /// map (upright if absent), zoom/pan reset to a fresh framing. Returns the
    /// view to push to the renderer. (Scaling mode is global and left unchanged.)
    pub fn view_for(&mut self, item: usize) -> ViewTransform {
        self.view.rotation = self.rotations.get(&item).copied().unwrap_or_default();
        self.view.zoom = 1.0;
        self.view.pan = [0.0, 0.0];
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
        self.view.rotation = new;
        self.push_view();
        // Flash a directional rotate icon (icon-only pill) as feedback.
        let ico = if ccw {
            icon::assets::ROTATE_LEFT
        } else {
            icon::assets::ROTATE_RIGHT
        };
        self.show_toast_icon("", Some(ico));
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
        // The shell writes the text and reports the success/failure toast (it can recover
        // the file name from `text` for the "Copied …" message).
        self.effects.push(contract::CoreEffect::WriteClipboard(
            contract::ClipboardPayload::Text(text),
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
            contract::ClipboardPayload::Text(lines.join("\n")),
        ));
    }

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
        self.last_present = Some(Instant::now());
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
        // The info panel belonged to the previous photo — drop it (and redraw to
        // remove it). Only touch the renderer if a panel was actually showing.
        if self.overlay_shown {
            if let Some(r) = self.renderer.as_mut() {
                r.set_overlay(None, 0);
            }
            self.overlay_shown = false;
            self.overlay_item = None;
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
                    r.set_overlay(None, 0);
                }
                self.effects.push(contract::CoreEffect::SetTitle(title));
                self.overlay_shown = false;
                self.overlay_item = None;
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
        self.last_present = Some(Instant::now());
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
            self.last_present = Some(Instant::now());
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
    pub fn exif_rows(&self) -> Vec<Row> {
        let Some(item) = self.displayed_item else {
            return Vec::new();
        };
        let name = self.source.name(item);
        let mut rows = Vec::new();
        // Identity header: filename (bold) over its folder (the filename is already
        // shown above, so the path row is the parent directory only).
        rows.push(Row::Span {
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
            rows.push(Row::Span {
                text: location,
                bold: false,
            });
        }
        if let Some(meta) = &self.current {
            rows.push(Row::Pair {
                label: "Dimensions".to_string(),
                value: format!("{} × {}", meta.w, meta.h),
            });
            rows.push(Row::Pair {
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
            rows.push(Row::Pair {
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
                rows.push(Row::Pair {
                    label: tag.clone(),
                    value: truncate_exif_value(val),
                });
            }
        }
        // Cap to what fits the screen height (~1.5x the font size per line).
        if let Some(fit) = self.fit {
            let line_h = ((15.0 * self.viewport.scale_factor).max(8.0) * 1.5).max(1.0);
            let max_rows = (((fit.max_height as f32) - 40.0) / line_h).max(1.0) as usize;
            if rows.len() > max_rows {
                rows.truncate(max_rows.saturating_sub(1));
                rows.push(Row::Span {
                    text: "…".to_string(),
                    bold: false,
                });
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
    pub fn animation_rows(&self, item: usize) -> Vec<Row> {
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
            rows.push(Row::Pair {
                label: "Live Photo".to_string(),
                value: detail.map_or(PENDING.to_string(), |(_, count, _, _)| {
                    format!("{count} frames")
                }),
            });
        }
        rows.push(Row::Pair {
            label: "Frame".to_string(),
            value: detail.map_or(PENDING.to_string(), |(idx, count, _, _)| {
                format!("{} / {}", idx + 1, count)
            }),
        });
        rows.push(Row::Pair {
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
        rows.push(Row::Pair {
            label: "Duration".to_string(),
            value: detail.map_or(PENDING.to_string(), |(_, _, total, _)| {
                format!("{:.2} s", total.as_secs_f64())
            }),
        });
        // A Live Photo always plays once; the loop count is only meaningful for a
        // GIF/APNG/WebP loop.
        if !is_live {
            rows.push(Row::Pair {
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

    /// Hide the info panel (clears the overlay quad).
    pub fn hide_overlay(&mut self) {
        if let Some(a) = self.renderer.as_mut() {
            a.set_overlay(None, 0);
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

    /// Build the empty-state **open panel** — two centered, interactive buttons ("Open File" and
    /// "Open Folder", each with its shortcut dimmed and right-aligned menu-style), lit per the
    /// current [`open_hover`]. `None` if no system font loaded. Returns the owned bitmap plus
    /// each button's bitmap-relative rect, so callers can apply it to a renderer they still own
    /// (e.g. mid-setup) and record the click targets.
    ///
    /// [`open_hover`]: App::open_hover
    pub fn open_panel_bitmap(&self) -> Option<hud::OpenPanelBitmap> {
        let hud = self.hud.as_ref()?;
        // A normal button size (like the scan card's Cancel button) — the call to action doesn't
        // need to shout; it's white text on an empty gray screen.
        let px = (16.0 * self.viewport.scale_factor).max(11.0);
        let file_key = self.shortcut_for(Action::OpenFile);
        let folder_key = self.shortcut_for(Action::OpenFolder);
        hud.render_open_panel(
            "Open File",
            &file_key,
            "Open Folder",
            &folder_key,
            px,
            hud::BG,
            true, // shortcut hint a notch heavier (Semibold) so the dimmed text keeps presence
            self.open_hover == Some(OpenButton::File),
            self.open_hover == Some(OpenButton::Folder),
        )
    }

    /// Flash a transient status message at the bottom-center (tasks.json #10) — for
    /// commands that otherwise give no visual feedback, e.g. the recursion toggle.
    /// A new toast replaces any current one.
    pub fn show_toast(&mut self, msg: &str) {
        self.show_toast_icon(msg, None);
    }

    /// Like [`show_toast`] but with an optional leading duotone icon (an SVG source
    /// from [`icon::assets`]) — e.g. the clipboard glyph on the Copy toast, or an
    /// icon-only pill (empty `msg`) for the rotate toasts. Always redraws, so a
    /// caller that also changed the view (e.g. `rotate`) renders even when there's
    /// no system font to build a toast from.
    pub fn show_toast_icon(&mut self, msg: &str, icon: Option<&str>) {
        let px = (26.0 * self.viewport.scale_factor).max(16.0);
        let pad = (12.0 * self.viewport.scale_factor).round().max(4.0) as u32;
        // A passive toast is not the interactive play hint — drop any play-hint state so it
        // doesn't respond to hover/click while a Copy/Save/… toast is up.
        self.play_hint = None;
        if let Some(hud) = self.hud.as_ref() {
            if let Some((rgba, w, h)) = hud.render_panel_icon(msg, px, pad, icon, hud::BG) {
                self.toast = Some(Toast {
                    rgba,
                    w,
                    h,
                    started: Instant::now(),
                    uploaded_alpha: -1.0,
                });
                self.push_toast(1.0);
            }
        }
        self.draw();
    }

    /// Build the **play hint** toast — the `▶ Play  P` button (leading icon, label, dimmed
    /// shortcut), at `hovered` fill/border — and push it. Returns the bitmap `(w, h)` so the
    /// caller can record the hit rect. Shared by the first flash and the hover re-render.
    pub fn build_play_hint(&mut self, icon: &str, hovered: bool) -> Option<(u32, u32)> {
        let px = (20.0 * self.viewport.scale_factor).max(13.0);
        let shortcut = self.shortcut_for(Action::PlayPause);
        let built = self.hud.as_ref().and_then(|hud| {
            let spec = hud::ButtonSpec {
                label: "Play",
                icon: Some(icon),
                shortcut: (!shortcut.is_empty()).then_some(shortcut.as_str()),
                shortcut_semibold: true,
                min_w: 0,
            };
            hud.render_button(&spec, px, hud::BG, hovered)
        });
        let (rgba, w, h) = built?;
        self.toast = Some(Toast {
            rgba,
            w,
            h,
            started: Instant::now(),
            uploaded_alpha: -1.0,
        });
        self.push_toast(1.0);
        Some((w, h))
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
        let margin = (64.0 * self.viewport.scale_factor).round().max(8.0) as u32;
        if let Some(a) = self.renderer.as_mut() {
            a.set_toast(Some((&faded, w, h)), margin);
        }
    }

    /// Advance the toast's hold/fade and return whether one is still active (so the
    /// event loop keeps ticking). Re-uploads only on a meaningful alpha change;
    /// clears the layer once expired.
    pub fn tick_toast(&mut self, now: Instant) -> bool {
        // A hovered play hint pauses the fade: keep its toast pinned in the full-opacity hold
        // window, so the button never vanishes out from under the pointer.
        if self.play_hint.is_some_and(|ph| ph.hovered) {
            if let Some(t) = self.toast.as_mut() {
                t.started = now;
            }
        }
        let Some(alpha) = self.toast.as_ref().and_then(|t| t.alpha(now)) else {
            self.play_hint = None;
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
        let (mut rgba, w, h) = hud::render_pie(diameter, progress, glow);
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

    /// Rasterize the scan status card and place it at the top-right with equal top/right insets.
    /// Records the centered **Cancel Scan button's** physical-px rect (the only click target).
    pub fn push_chip(&mut self, name: &str, path: &str, count: usize) {
        let heading = format!("Scanning \u{201c}{name}\u{201d}");
        let noun = if count == 1 { "image" } else { "images" };
        let count_line = format!("{} {noun} found", hud::format_thousands(count as u64));
        // Equal inset from the top and right edges; fixed card width, clamped to the window.
        let margin = (PIE_MARGIN * self.viewport.scale_factor).round().max(4.0) as u32;
        let win_w = self.viewport.width;
        let width = ((SCAN_CARD_WIDTH * self.viewport.scale_factor).round())
            .min((win_w as f32 - 2.0 * margin as f32).max(1.0))
            .max(1.0) as u32;
        let card = self.hud.as_ref().and_then(|hud| {
            let px = (15.0 * self.viewport.scale_factor).max(10.0);
            hud.render_scan_card(
                &heading,
                path,
                &count_line,
                "Cancel Scan",
                icon::assets::STOP,
                px,
                width,
                hud::BG,
                self.chip_hovered,
            )
        });
        let Some((rgba, w, h, btn)) = card else {
            self.chip_rect = None;
            return;
        };
        // Card top-left in physical px (right edge inset by `margin`, top inset by `margin`),
        // then the button rect offset within it → the click hit-target.
        let card_x0 = self.viewport.width as f32 - margin as f32 - w as f32;
        let card_y0 = margin as f32;
        let [bx, by, bw, bh] = btn.map(|v| v as f32);
        self.chip_rect = Some([
            card_x0 + bx,
            card_y0 + by,
            card_x0 + bx + bw,
            card_y0 + by + bh,
        ]);
        if let Some(a) = self.renderer.as_mut() {
            a.set_chip(Some((&rgba, w, h)), margin, margin);
        }
        self.draw();
    }

    /// Clear the scan card layer if it's up (and redraw to remove it).
    pub fn clear_chip(&mut self) {
        if self.chip_sig.is_some() {
            if let Some(a) = self.renderer.as_mut() {
                a.set_chip(None, 0, 0);
            }
            self.chip_rect = None;
            self.chip_hovered = false;
            self.draw();
        }
    }

    /// Hit-test a physical-px cursor position against the scan card's **Cancel Scan button**
    /// rect. The reusable overlay-click primitive: store a rect when you draw an interactive
    /// overlay, test it here before the click falls through to drag-to-pan. (Future EXIF copy
    /// buttons will register their own rects the same way.)
    pub fn chip_hit(&self, x: f32, y: f32) -> bool {
        self.chip_rect.is_some_and(|rect| point_in_rect(rect, x, y))
    }

    /// Update the Cancel Scan button's hover "lit" state from the latest cursor position, and —
    /// only when hover **changes** — re-rasterize the card so the button lights up / dims. This
    /// runs on every cursor-move, but the rebuild fires just on the enter/leave transition (one
    /// ~320px CPU composite), never per move or per frame, so it stays off the photo hot path.
    pub fn update_chip_hover(&mut self) {
        let hovered = self.last_cursor.is_some_and(|[x, y]| self.chip_hit(x, y));
        if hovered == self.chip_hovered {
            return;
        }
        self.chip_hovered = hovered;
        // Re-render the card in the new hover state; its content (name/path/count) is unchanged,
        // so this bypasses the content throttle and feels instant.
        if let Some((name, path, count)) = self.chip_sig.clone() {
            self.push_chip(&name, &path, count);
        }
    }

    /// The interactive play hint's on-screen `[x0, y0, x1, y1]` rect (physical px), derived from
    /// the live window size — the toast is bottom-center with a fixed margin, so this matches
    /// the renderer's placement and survives resizes. `None` unless the play hint is the current
    /// toast and still on screen.
    pub fn play_hint_rect(&self) -> Option<[f32; 4]> {
        let ph = self.play_hint?;
        self.toast.as_ref()?; // only while its toast is actually up
        let sz = self.viewport;
        let margin = (64.0 * self.viewport.scale_factor).round().max(8.0);
        let x0 = ((sz.width as f32 - ph.w as f32) * 0.5).max(0.0);
        let y1 = sz.height as f32 - margin;
        let y0 = y1 - ph.h as f32;
        Some([x0, y0, x0 + ph.w as f32, y1])
    }

    /// Whether the pointer is over the interactive play hint.
    pub fn play_hint_hit(&self) -> bool {
        match (self.last_cursor, self.play_hint_rect()) {
            (Some([x, y]), Some(rect)) => point_in_rect(rect, x, y),
            _ => false,
        }
    }

    /// Update the play hint's hover state from the cursor. On an enter/leave transition it
    /// re-renders the button lit/unlit (the fade itself is paused while hovered — see
    /// [`tick_toast`]). Cheap: one CPU composite per transition, never per move.
    ///
    /// [`tick_toast`]: App::tick_toast
    pub fn update_play_hint_hover(&mut self) {
        let hovered = self.play_hint_hit();
        let Some(ph) = self.play_hint else {
            return;
        };
        if ph.hovered == hovered {
            return;
        }
        // Re-render at the new state (size is unchanged; keep the recorded w/h). Rebuilding
        // resets the fade clock, which is what we want: hovering holds it at full, and leaving
        // gives it a fresh hold before it fades.
        self.build_play_hint(ph.icon, hovered);
        self.play_hint = Some(PlayHint { hovered, ..ph });
        self.draw();
    }

    /// Render one frame.
    pub fn draw(&mut self) {
        let t0 = Instant::now();
        let mut fatal = false;
        let drew = if let Some(a) = self.renderer.as_mut() {
            if let Err(e) = a.render() {
                eprintln!("fatal render error: {e:?}");
                fatal = true;
            }
            a.poll();
            true
        } else {
            false
        };
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
        for &action in self.held.values() {
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
    /// (task #38). Filesystem pairing, memoized per item and computed lazily — only ever
    /// reached when settled on a photo, never on the fly-through path. Always `None` off
    /// macOS (Windows Live Photos are task #39, since the decoder is macOS-only).
    pub fn live_motion_path(&mut self, item: usize) -> Option<PathBuf> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = item;
            return None;
        }
        #[cfg(target_os = "macos")]
        if let Some(cached) = self.live_motion_cache.get(&item) {
            return cached.clone();
        }
        #[cfg(target_os = "macos")]
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

    /// Kick the whole-sequence decode for `item` on a worker thread so a big GIF/WebP (or
    /// a Live Photo `.mov`) never stalls the event loop; the still first frame stays on
    /// screen until it lands (picked up by `poll_anim_decode`). `want` decides what
    /// happens on arrival — eager prep (stash ready), play (`P`), or step (frame-step).
    pub fn start_animation_decode(&mut self, item: usize, want: AnimWant) {
        self.anim_gen += 1;
        let gen = self.anim_gen;
        let epoch = self.epoch;
        let source = Arc::clone(&self.source);
        let fit = self.decode_fit();
        // A Live Photo decodes its companion `.mov` via AVFoundation; everything else
        // decodes the still's own bytes as a multi-frame animation.
        let live = self.live_motion_path(item);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = decode_motion_job(live, &source, item, fit);
            let _ = tx.send(result);
        });
        self.anim_decode = Some(AnimDecode {
            gen,
            item,
            epoch,
            want,
            rx,
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
