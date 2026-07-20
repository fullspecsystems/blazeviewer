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

use pb_core::{
    full_ring, prefetch_targets, prefetch_targets_scanning, Playlist, ResidentRing, ShuffleOrder,
};
use pb_decode::{read_exif_fields, FitBox};
use pb_render::{test_pattern, Rotation, ScaleMode, ViewTransform, MAX_ZOOM, MIN_ZOOM};
use pb_source::{FsSource, ItemSource};

use hud::Row;
use pb_hud::{hud, icon};

use crate::animation::{AnimDecode, AnimWant, Playback, Prepared, StreamHeader, StreamMsg};
use crate::contract;
use crate::decode_pool::Outcome;
use crate::engine::*;
use crate::keymap::Keymap;
use crate::launch::{LaunchOverrides, StartAt};
use crate::panels::{
    DescribeBody, DescribePanel, DetailRow, DetailsPanel, HelpPanel, HelpSection, TextBody,
    TextPanel,
};
use crate::pb_key::PbKey;
use crate::video_native::ActiveVideoBackend;
use crate::{
    settings, slideshow, timing, Action, AppCore, FitStash, InspectorTab, NativeToast, Nav, Panels,
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

/// `PB_DOOR_DIAG=1` → per-draw archive-door state to stderr (dev-only; zero cost when off).
/// Pairs with the renderer's `[door-diag] render` line: if the core says a door is presented
/// while the renderer's draw source is `Held`/`Single` (a stale photo), it is a ring desync
/// (the "card over a photo" defect); a mismatch the other way points at the shell overlay.
fn door_diag() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PB_DOOR_DIAG").is_some())
}

/// `PB_SHARP_DIAG=1` → the preview→full "sharpen" lifecycle to stderr (dev-only; zero cost when
/// off; event-driven, so low noise). For the "photo loads blurry and never sharpens until a
/// resize" bug (#111): if a stuck photo logs a `sharpen request` but no `full landed` line, its
/// full decode was never requested/never arrived; a `full landed … dropped|upgrade_done` means it
/// arrived and was rejected — each points at a different fix.
fn sharp_diag() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PB_SHARP_DIAG").is_some())
}

/// The ONE staleness predicate for a decode outcome (#119): judged by the job's declared
/// [`Validity`](crate::decode_pool::Validity) domain, applied at every gate — channel
/// ingestion (`absorb_results` / `drain_results`), `rebuild_ring` retention, and the drain
/// admit — never re-derived inline. Geometry work dies with the epoch OR the deck; content
/// work (Originals, thumbs, poster selections) dies only with the deck, which is what lets
/// a parked Original survive a fullscreen toggle (the #119 storm).
fn outcome_stale(epoch: u64, content_gen: u64, o: &crate::decode_pool::Outcome) -> bool {
    match crate::decode_pool::validity(o.key.purpose, o.key.rep_kind) {
        crate::decode_pool::Validity::Geometry => {
            o.key.epoch != epoch || o.key.content_gen != content_gen
        }
        crate::decode_pool::Validity::Content => o.key.content_gen != content_gen,
    }
}

/// `PB_PERF=1` → live one-shot latency lines to stderr (open→first-photo, open→all-cached,
/// resize→on-screen). Shell-agnostic (works on the macOS host too, read via a captured
/// stderr), unlike the winit-only `--metrics` summary. Zero cost when off (see [`perf`]).
pub(crate) fn perf_on() -> bool {
    crate::perf::env_enabled()
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
        // never invoked. A real host installs a decode closure over its `ItemSource`.
        let decode: Arc<crate::decode_pool::DecodeFn> =
            Arc::new(|_src, _item, _fit, _prev, _purpose, _cancel| {
                Err(pb_decode::DecodeError::Corrupt("headless".into()))
            });
        let (pool, results) = crate::decode_pool::DecodePool::new(1, 1 << 20, decode);
        let (poster_read_tx, poster_read_rx) = std::sync::mpsc::channel();
        let (video_read_tx, video_read_rx) = std::sync::mpsc::channel();
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(Vec::new()));
        let settings = settings::Settings::default();
        let mut core = AppCore {
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
            content_top_inset: 0,
            pending_video_bytes: None,
            pending_poster_bytes: std::collections::HashMap::new(),
            poster_req_seq: 0,
            poster_inflight: std::collections::HashMap::new(),
            poster_read_tx,
            poster_read_rx,
            video_read_tx,
            video_read_rx,
            dragging: false,
            rotations: std::collections::HashMap::new(),
            video_resume: std::collections::HashMap::new(),
            zoom_started: None,
            zoom_last: None,
            pan_started: None,
            pan_last: None,
            resize_settle_at: None,
            fullscreen_toggled_at: None,
            geometry_save_at: None,
            windowed: true,
            meta_cache: std::collections::HashMap::new(),
            current: None,
            exif_cache: std::collections::HashMap::new(),
            details_probe: None,
            details_gen: 0,
            catalog_seq: 0,
            audio_active: None,
            subtitles: crate::subtitle_engine::SubtitleEngine::from_settings(&settings),
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
            preview_watchdog: None,
            live_motion_cache: std::collections::HashMap::new(),
            metrics: crate::metrics::StageTimes::default(),
            perf: crate::perf::Perf::new(perf_on()),
            source,
            archive_scope: None,
            playlist: Playlist::new(0, 0),
            targets: Vec::new(),
            last_nav: Nav::Forward,
            displayed_item: None,
            presented_epoch: None,
            presented_kind: None,
            bg: crate::background::BackgroundOps::new(),
            dir_scan: None,
            scan_wire_gen: 0,
            archive_load: None,
            archive_wire_gen: 0,
            #[cfg(test)]
            rebind_count: 0,
            target_item: None,
            compare_pin: None,
            compare_return: None,
            compare_pin_id: None,
            compare_carry: None,
            epoch: 1,
            content_gen: 1,
            root: PathBuf::new(),
            scan_root: None,
            recursive: false,
            scanning: false,
            launching: false,
            dialog_open: false,
            archive_loading: false,
            redraw_pending: false,
            resize_hold: None,
            fit_stash: [None, None],
            scan_bootstrapped: false,
            password_archive: None,
            archive_passwords: Vec::new(),
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
            native_thumbs: false,
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
            left_tab: Default::default(),
            thumbs: Default::default(),
            poster_sel: Default::default(),
            retry: Default::default(),
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
            anim_stream: None,
            video: None,
            video_seq: 0,
            video_ffmpeg_fallback: None,
            // Hermetic default; the real host constructor (`new_host`) reads the env.
            sample_buffer_opt_in: false,
            dovi_warned: std::collections::HashSet::new(),
            video_diag_last: None,
            video_seek_last: None,
            pending_delete_retry: None,
            video_pill_text: None,
            video_osd_until: None,
            video_geometry_stale: false,
            video_paused_by_resize: false,
            prepared: None,
            anim_gen: 0,
            anim_hint_shown_for: None,
            framestep_started: None,
            framestep_last: None,
            live_revert_at: None,
            keymap: Keymap::defaults(),
            settings,
            launch: LaunchOverrides::default(),
            effects: Vec::new(),
        };
        // The selector's fence must match the literal's starting content_gen
        // (phase-1 review f6: a default selector at gen 0 against content gen 1
        // refused every install and re-walked the first video forever).
        core.poster_sel.reset(core.content_gen);
        core
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
            Arc::new(|src, item, fit, allow_preview, purpose, cancel| {
                crate::engine::decode_item_for(src, item, fit, allow_preview, purpose, cancel)
            });
        let select: Arc<crate::decode_pool::SelectFn> =
            Arc::new(|src, item, fit, display_class, replay, cancel| {
                crate::engine::select_item(src, item, fit, display_class, replay, cancel)
            });
        let (pool, results) = crate::decode_pool::DecodePool::new_with_select(
            crate::decode_pool::recommended_workers(),
            POOL_BUDGET_BYTES,
            decode,
            select,
        );
        core.pool = pool;
        core.results = results;
        let settings = settings::Settings::load();
        core.initial_delay = Duration::from_millis(settings.hold_delay_ms as u64);
        core.slideshow.interval = Duration::from_secs_f64(settings.slideshow_interval_secs);
        core.view.mode = scale_mode_of(settings.scale_mode);
        core.info_line = settings.show_image_info; // the info readout's launch default (task #54)
                                                   // The engine was built from the *default* settings above (this constructor loads the
                                                   // real ones only now), so its mode AND style have to be re-derived — otherwise
                                                   // `subtitles = true` on disk launches with captions off, and the preference looks
                                                   // like it never saved. `from_settings` takes the whole struct precisely so this
                                                   // line cannot go stale as fields are added.
        core.subtitles = crate::subtitle_engine::SubtitleEngine::from_settings(&settings);
        core.keymap = Keymap::load();
        core.settings = settings;
        core.hud = hud::Hud::load();
        core.persist_prefs = true; // a live host persists the remembered last_folder
                                   // Opt-in to the parked macOS sample-buffer presenter (the DoVi reference
                                   // renderer) — read once here so routing never touches process-global env.
        core.sample_buffer_opt_in = std::env::var("PB_SAMPLE_BUFFER").is_ok_and(|v| v == "1");
        core
    }

    /// Whether prefetch/upload work is still outstanding (keep polling if so).
    pub fn work_pending(&self) -> bool {
        // A dropped frame (surface Lost/Outdated/Timeout) keeps the pump awake so the
        // retry in `tick` actually runs — without this, an idle host (empty screen,
        // paused pump) composites the stale frame forever.
        self.redraw_pending
            // Live pool work — queued, in-flight, or sent-but-undrained (#119, Codex
            // r2 h1): a parked Original can still be decoding (or sitting in the
            // results channel) after every display slot is resident and caught-up;
            // without this arm the pump sleeps on it and the texture lands only on
            // the next input.
            || self.pool.has_work()
            // Staged outcomes awaiting the next drain must keep the pump awake for
            // the same reason (they hold pool byte-budget until uploaded, too).
            || !self.pending_uploads.is_empty()
            || self.archive_loading
            // A streaming dir scan keeps the loop polling too, so `poll_dir_scan` picks up
            // batches (and the delayed Scanning-dialog reveal) even when the event queue is
            // quiet — without this, a slow walk on an idle app waits for the next OS event.
            || self.scanning
            // An archive open in flight (task #126 step 2) keeps the loop polling so
            // `poll_archive_load` lands its result — and its progress chrome stays live —
            // without waiting for the next OS event. Both shells had this as
            // `archive_load.is_some()` in their own `work_pending` override.
            || self.archive_load.is_some()
            // A thumbnail derive in flight (task #83) keeps the loop polling so
            // `tick` lands it into the strip promptly.
            || self.thumbs.working()
            // An off-thread animation decode in flight keeps the loop polling so
            // `poll_anim_decode` picks it up promptly (active playback drives its own
            // precise next-frame wake via `tick_playback`, not this frame poll).
            || self.anim_decode.is_some()
            // A streaming Live Photo decode (task #69) likewise keeps the loop polling so
            // `poll_anim_stream` drains newly decoded frames as they arrive.
            || self.anim_stream.is_some()
            // Active video playback (task #79): the **Session** route's `poll_video` paces
            // frames off this loop. The **Native** route (macOS sample-buffer / AVPlayer) is
            // OS-presented and pulls nothing per frame, so it must NOT keep the loop hot — that
            // spun `pump()` at 120 Hz on the main thread and dropped presentation frames
            // (owner-measured, 2026-07-15). `needs_frame_pacing` draws that line.
            || self.video.as_ref().is_some_and(|v| v.needs_frame_pacing())
            // A delete waiting out a retiring video reader (bounded retry).
            || self.pending_delete_retry.is_some()
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
            // An off-thread video Details probe (task #98) keeps polling so
            // `poll_details_probe` swaps the panel off "Reading…" promptly. Without this
            // an idle app could stop ticking with the result sitting in the channel.
            || self.details_probe.is_some()
            // A subtitle worker (the font system / a sidecar read, task #90) keeps
            // polling so its result installs promptly.
            || self.subtitles.working()
            // The target isn't on screen at the current fit yet (incl. a same-index
            // re-present pending after a geometry change bumped the epoch) — keep polling
            // so `drain_results` presents it (task #18 finding #5).
            || self.target_pending()
            || self
                .targets
                .iter()
                .any(|&t| self.display_slot(t).is_none() && !self.failed.contains(&t))
    }

    /// Dispatch a one-shot [`Action`] — the single entry point shared by the keyboard
    /// (one-shot keys, via the keymap) and the menu (`MenuAction::to_action`). The pure
    /// view/nav/HUD/animation arms run here in the core; the **flow** arms (dialogs, window
    /// mode, scan, file edits, quit) are routed to the shell/host via
    /// [`CoreEffect::ShellFlowAction`] until 5.6 inverts them into specific effects/events.
    /// Navigation here is a single step (what the menu wants); the keyboard's held-to-blaze nav
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
            Action::Thumbnails => self.toggle_thumbnails(),
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
            // Captions on/off (task #90). A preference, not a viewing trace (privacy #2):
            // it records that the user likes subtitles, never which video or which track.
            // The engine picks the change up on the next tick — no reload, because the cue
            // track stays loaded while it's off; only drawing stops.
            Action::ToggleSubtitles => {
                // Flips on/off ONLY. It must not touch which track you picked — that was
                // the defect: `C` used to set the mode to Automatic on the way back on, so
                // picking Chinese and pressing `C` twice returned English.
                self.subtitles.selection.toggle();
                let on = self.subtitles.selection.enabled;
                self.settings.subtitles = on;
                // Gated on `persist_prefs`, unlike the older toggles that call `save()`
                // straight — so a unit test can dispatch this without writing the real
                // settings.toml. A live host sets the flag; headless/tests don't.
                if self.persist_prefs {
                    self.settings.save();
                }
                let (msg, icon) = if on {
                    ("Subtitles on", ToastIcon::Captions)
                } else {
                    ("Subtitles off", ToastIcon::CaptionsOff)
                };
                self.show_toast_icon(msg, icon);
            }
            Action::SubtitleCycle => self.cycle_subtitle_track(),
            Action::AudioNext => self.cycle_audio_track(true),
            Action::AudioPrev => self.cycle_audio_track(false),
            Action::MuteLiveAudio => {
                // An explicit toggle supersedes a `--mute` launch override and persists the
                // user's choice (clearing the session override so it no longer masks the setting).
                let muted = !self.effective_mute();
                self.launch.mute = None;
                self.settings.mute_live_audio = muted;
                self.settings.save();
                if muted {
                    // Silence any playing clip now; a slashed-speaker icon pill = muted.
                    self.effects.push(contract::CoreEffect::StopLiveAudio);
                    // A playing video mutes in place — its clock keeps running, so
                    // A/V sync is unaffected (task #79 phase 5). Native (macOS): the
                    // AVPlayer owns audio, so mute the player itself.
                    if let Some(p) = self
                        .video
                        .as_mut()
                        .and_then(ActiveVideoBackend::as_native_mut)
                    {
                        p.set_muted(true);
                        self.effects.push(contract::CoreEffect::SetVideoMuted {
                            session_id: p.session_id,
                            muted: true,
                        });
                    } else if self.video.is_some() {
                        self.effects
                            .push(contract::CoreEffect::SetVideoAudioMuted(true));
                    }
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
                    // A muted video unmutes in place (its audio kept pace muted). Native
                    // (macOS): unmute the AVPlayer itself.
                    if let Some(p) = self
                        .video
                        .as_mut()
                        .and_then(ActiveVideoBackend::as_native_mut)
                    {
                        p.set_muted(false);
                        self.effects.push(contract::CoreEffect::SetVideoMuted {
                            session_id: p.session_id,
                            muted: false,
                        });
                    } else if self.video.is_some() {
                        self.effects
                            .push(contract::CoreEffect::SetVideoAudioMuted(false));
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
                self.fullscreen_toggled_at = Some(self.now); // → short resize settle (#110 §4)
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
            // `ToggleToolbar` (#61) also routes here: the docked toolbar is a Windows/Linux-shell
            // concept (macOS has its native toolbar), so the shell owns flipping `show_toolbar`,
            // persisting it, and re-reserving the photo's top inset — the core stays agnostic.
            Action::DeletePermanent
            | Action::Recursive
            | Action::ShowArchives
            | Action::CancelScan
            | Action::Quit
            | Action::ToggleToolbar => self
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
        // Videos never persist a rotation (task #79 action matrix): footage can rotate
        // mid-clip, so there is no single correct value to write. The in-memory display
        // rotation stays available (and stays live during playback).
        match crate::video::item_kind(self.source.as_ref(), item) {
            crate::video::LibraryItemKind::Video(_) => {
                self.show_toast("Can't save rotation for video");
                return;
            }
            // A door is a place, not a picture — there is no orientation to write.
            crate::video::LibraryItemKind::Archive(_) => {
                self.show_toast("Can't save rotation for an archive");
                return;
            }
            // Exhaustive so a new kind states its own answer: what follows assumes a
            // rotatable image file on disk.
            crate::video::LibraryItemKind::Image => {}
        }
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
                // A saved rotation rewrites the file's pixels-as-displayed → content change.
                self.invalidate_content();
                self.load_current_sync();
                self.target_item = self.playlist.current();
                self.request_prefetch();
                self.undo_stack.push(UndoAction::SaveRotation {
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
            UndoAction::SaveRotation { path, prev } => {
                match crate::save_rotation::set_orientation(&path, prev) {
                    Ok(()) => {
                        // Re-resolve the photo's *current* index by path — an intervening delete
                        // may have reshaped the deck since the save — to drop its stale cached
                        // decode so it re-reads the reverted orientation.
                        let idx = self.index_of_path(&path);
                        if let Some(idx) = idx {
                            self.rotations.remove(&idx);
                            self.meta_cache.remove(&idx);
                            self.exif_cache.remove(&idx); // EXIF Orientation reverted on disk
                            self.failed.remove(&idx);
                            self.preview_resident.remove(&idx);
                            self.upgrade_done.remove(&idx);
                        }
                        // The reverted orientation rewrote the file's pixels → content change,
                        // REGARDLESS of whether the photo is on screen (item-6 spec Part B): the
                        // ring retains Originals across geometry changes now, so a navigated-away
                        // neighbour's resident Original would otherwise keep the stale
                        // orientation. (The pre-item-6 code only invalidated when displayed — a
                        // latent hole that retention would have promoted to a visible bug.) The
                        // purge + re-prefetch run for any in-deck photo; the synchronous reload
                        // stays displayed-only.
                        if idx.is_some() {
                            self.invalidate_content();
                            if idx == self.displayed_item {
                                self.load_current_sync();
                            }
                            self.target_item = self.playlist.current();
                            self.request_prefetch();
                        }
                        self.show_toast_icon("Rotation undone", ToastIcon::Undo);
                    }
                    Err(e) => {
                        eprintln!("undo rotation failed: {}: {e}", path.display());
                        self.show_toast("Undo failed");
                        // A transient I/O error leaves the file unchanged, so keep the entry for a
                        // retry. But a *vanished* file (e.g. permanently deleted after the
                        // rotation) is unrecoverable — drop it rather than jam the stack.
                        if path.exists() {
                            self.undo_stack
                                .push(UndoAction::SaveRotation { path, prev });
                        }
                    }
                }
            }
            // Undo a delete: restore the file from the Recycle Bin and re-insert it into the
            // playlist at its old position, navigating to it so the "Restored …" toast lands on
            // the recovered photo.
            UndoAction::Deletion {
                index,
                path,
                name,
                handle,
            } => match crate::delete::restore(handle) {
                Ok(()) => {
                    self.reinsert_after_restore(index, &path);
                    self.show_toast_icon(&format!("Restored {name}"), ToastIcon::Undo);
                }
                Err(e) => {
                    eprintln!("undo delete failed: {}: {e}", path.display());
                    // A collision (a file already occupies the original path) is the usual
                    // failure; either way the file stays recoverable in the Recycle Bin, so the
                    // entry is spent (the handle was consumed) and we just report it.
                    self.show_toast("Couldn't restore");
                }
            },
        }
    }

    /// Restore a just-undeleted file to the playlist: rebuild the `FsSource` from the current
    /// paths with `path` re-inserted at `index` (its position when deleted, clamped to the current
    /// length), and navigate to it. A same-deck rebuild — the root is unchanged — so any *other*
    /// pending Deletion undo entries survive.
    fn reinsert_after_restore(&mut self, index: usize, path: &Path) {
        let mut paths: Vec<PathBuf> = (0..self.source.len())
            .filter_map(|i| self.source.path(i).map(Path::to_path_buf))
            .collect();
        let at = index.min(paths.len());
        paths.insert(at, path.to_path_buf());
        let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(paths));
        let root = self.root.clone();
        let scan_root = self.scan_root.clone();
        let recursive = self.recursive;
        self.rebuild_playlist(src, root, scan_root, recursive, at);
    }

    /// The current playlist index of the photo at `path`, if it's still in the deck. Undo entries
    /// are keyed by stable path (see [`crate::undo`]); this re-resolves the transient index they
    /// need at apply time, since a rebuild between record and undo reassigns indices.
    fn index_of_path(&self, path: &Path) -> Option<usize> {
        (0..self.source.len()).find(|&i| self.source.path(i) == Some(path))
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
        // On a drive configured to bypass the Recycle Bin, a "recycle" would silently permanently
        // delete (the shell honors the per-volume NukeOnDelete policy) — no undo, and (via the
        // trash crate's FOF_NO_UI) no warning. Route Del through the permanent-delete confirmation
        // instead: the shell opens the themed confirm dialog, whose Yes calls do_delete(.., true).
        if !crate::delete::will_recycle(&path) {
            self.effects.push(contract::CoreEffect::ShellFlowAction(
                Action::DeletePermanent,
            ));
            return;
        }
        self.do_delete(item, &path, false);
    }

    /// Perform the actual deletion of `item` (`path`) — recoverable (Recycle Bin) or permanent —
    /// then flash an icon-only pill on the still-shown photo and defer the playlist advance a beat
    /// (`DELETE_ADVANCE_DELAY`) so the feedback registers first. The permanent path reaches here
    /// only after the shell's confirm dialog answers Yes (`do_delete(.., true)`). An explicit,
    /// user-initiated file removal — never the passive view path (privacy #2). The trash / remove
    /// I/O is cross-platform (`crate::delete`), so this is a pure core method.
    pub fn do_delete(&mut self, item: usize, path: &Path, permanent: bool) {
        // Release media handles FIRST (task #79 action matrix): a playing video's
        // reader holds the file open; stopping starts its (async) retirement so
        // the delete — or its brief retry below — can succeed.
        if self.video.as_ref().is_some_and(|v| v.item() == item) {
            self.stop_video();
        }
        let res = if permanent {
            crate::delete::delete_permanently(path).map(|()| None)
        } else {
            crate::delete::recycle(path).map(Some)
        };
        match res {
            Ok(outcome) => self.finish_delete(item, path, permanent, outcome),
            Err(e) => {
                // A video's decoder can still be retiring (~1 s on HEVC) — the handle
                // clears momentarily, so retry off the event loop instead of failing.
                if self.item_is_video(item) {
                    eprintln!(
                        "delete blocked (retrying while the reader retires): {}: {e}",
                        path.display()
                    );
                    self.pending_delete_retry = Some(crate::app_core::DeleteRetry {
                        at: self.now + DELETE_RETRY_INTERVAL,
                        item,
                        path: path.to_path_buf(),
                        permanent,
                        tries_left: DELETE_RETRY_MAX,
                    });
                    return;
                }
                // A recoverable delete the OS refused (a read-only / no-Trash volume — common on
                // macOS network shares) would otherwise dead-end on "Delete failed". Offer the
                // permanent-delete confirmation instead (the same themed dialog Shift+Del uses),
                // so the user can still remove the file deliberately. A *permanent* delete that
                // fails is a genuine error with nowhere left to escalate.
                if !permanent {
                    eprintln!(
                        "trash refused, offering permanent delete: {}: {e}",
                        path.display()
                    );
                    self.effects.push(contract::CoreEffect::ShellFlowAction(
                        Action::DeletePermanent,
                    ));
                    return;
                }
                eprintln!("delete failed: {}: {e}", path.display());
                self.show_toast("Delete failed");
            }
        }
    }

    /// The post-I/O half of a successful delete: freeze playback, flash the icon
    /// pill, defer the playlist advance a beat. `outcome` is `None` for a permanent delete;
    /// for the recoverable path it carries whether the file actually reached a restorable Recycle
    /// Bin / Trash (from [`crate::delete::recycle`]) — captured at delete time because macOS can't
    /// re-derive the Trash location from the original path afterward. When restorable, records the
    /// Edit ▸ Undo entry.
    fn finish_delete(
        &mut self,
        item: usize,
        path: &Path,
        permanent: bool,
        outcome: Option<crate::delete::RecycleOutcome>,
    ) {
        // Deleting a playing animation stops playback so the doomed photo freezes on its current
        // frame under the trash icon (rather than animating until removal).
        self.stop_playback();
        debug_assert_eq!(
            permanent,
            outcome.is_none(),
            "a permanent delete carries no recycle outcome; a recoverable one always does"
        );
        let _ = permanent;
        let icon = match outcome {
            // Explicit Shift+Del / confirmed permanent delete: trash icon, no undo.
            None => ToastIcon::Delete,
            // Recoverable delete that reached a restorable bin: record an undo entry
            // (Ctrl+Z / Edit ▸ Undo) and show the recycle icon.
            Some(crate::delete::RecycleOutcome::Recycled(handle)) => {
                let name = crate::engine::file_name_of(&path.to_string_lossy());
                self.undo_stack.push(UndoAction::Deletion {
                    index: item,
                    path: path.to_path_buf(),
                    name,
                    handle,
                });
                ToastIcon::Recycle
            }
            // A bypass-the-bin volume slipped past `will_recycle` and nuked it (Windows/Linux):
            // show the permanent icon rather than a misleading recycle one, and record no undo.
            Some(crate::delete::RecycleOutcome::Permanent) => ToastIcon::Delete,
        };
        self.show_toast_icon("", icon);
        self.pending_delete = Some((self.now + DELETE_ADVANCE_DELAY, item));
    }

    /// Drive a scheduled delete retry (a video whose reader was still retiring).
    /// Called from `tick`; bounded — after the tries run out it reports honestly.
    pub fn poll_delete_retry(&mut self) {
        let due = self
            .pending_delete_retry
            .as_ref()
            .is_some_and(|r| self.now >= r.at);
        if !due {
            return;
        }
        let mut retry = self.pending_delete_retry.take().expect("checked above");
        let res = if retry.permanent {
            crate::delete::delete_permanently(&retry.path).map(|()| None)
        } else {
            crate::delete::recycle(&retry.path).map(Some)
        };
        match res {
            Ok(outcome) => self.finish_delete(retry.item, &retry.path, retry.permanent, outcome),
            Err(e) => {
                retry.tries_left = retry.tries_left.saturating_sub(1);
                if retry.tries_left == 0 {
                    eprintln!("delete failed: {}: {e}", retry.path.display());
                    self.show_toast("Delete failed");
                } else {
                    retry.at = self.now + DELETE_RETRY_INTERVAL;
                    self.pending_delete_retry = Some(retry);
                }
            }
        }
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
                self.video_hover_reveal(y);
            }
            // Trackpad pinch (macOS): magnify about the cursor (+ spread in / − pinch out).
            CoreEvent::Pinch { delta } => {
                self.zoom_about_cursor(1.0 + delta * PINCH_GAIN);
            }
            // Trackpad double-tap ("smart magnify"): toggle 100%, sharing the `0` / menu path.
            CoreEvent::DoubleTap => self.dispatch_action(Action::ToggleOriginal),
            // The per-tick core loop (hold-to-blaze / slideshow / prefetch / animation), stamping
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
            // A live edit from an auto-saving Settings window (both shells now): apply +
            // persist the edited model immediately; the window stays open — no CloseDialog.
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
        // PB_DOOR_DIAG: which branch a folder-scan batch takes, and the state it decides on.
        // A `BOOTSTRAP` while `archive_scope=true` is the still-open "mode B" hole (a stale scan's
        // first batch clobbering an archive deck — the extend guard below doesn't cover bootstrap).
        if door_diag() {
            let branch = if !self.scan_bootstrapped {
                "BOOTSTRAP"
            } else if self.archive_scope.is_some() || resolved.scan_root != self.scan_root {
                "REJECT"
            } else {
                "EXTEND"
            };
            eprintln!(
                "[door-diag] scan_batch -> {branch} batch_len={} batch_scan_root={:?} | scan_bootstrapped={} archive_scope={} cur_scan_root={:?} cur_src_len={}",
                resolved.source.len(),
                resolved.scan_root,
                self.scan_bootstrapped,
                self.archive_scope.is_some(),
                self.scan_root,
                self.source.len(),
            );
        }
        if !self.scan_bootstrapped {
            self.scan_bootstrapped = true;
            // A `--start-at` / `--shuffle` / `--reverse` launch chooses the first photo shown over
            // this bootstrap batch (the deck wasn't listed at construction); else the plan's cursor.
            let start = self
                .launch_start_index(&*resolved.source)
                .unwrap_or(resolved.start);
            self.rebuild_playlist(
                resolved.source,
                resolved.root,
                resolved.scan_root,
                resolved.recursive,
                start,
            );
        } else if self.archive_scope.is_some() || resolved.scan_root != self.scan_root {
            // Cross-deck open race (Codex-diagnosed 2026-07-17): a folder-scan worker keeps
            // running when an *archive* open (a different worker type, so it doesn't supersede
            // the scan) installs a new deck. A late cumulative batch from that still-alive scan
            // must NOT extend the archive deck: `extend_playlist` swaps `self.source` without
            // touching the ring, epoch, or `content_gen`, so index N would name a folder item
            // while both rings still hold the *archive* texture for N — `present_slot` returns
            // true with the wrong occupant, producing the "title advances but the view is frozen,
            // door card over a photo" corruption (a resize heals it by rebuilding the rings).
            // A legitimate extend is always the SAME folder scan continuing: no archive scope,
            // and a matching `scan_root`. Anything else is a stale/cross-type batch — drop it.
            // The shells also drop the other worker on each open (belt), but this keeps the core
            // correct on its own (and covers macOS).
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
    /// Cap on session passwords auto-tried per encrypted archive
    /// (session-archive-password-cache). Bounds the worst case (a differently-encrypted
    /// archive against a cache full of wrong passwords) while covering the realistic "a few
    /// distinct passwords this session" case; MRU-first, so the common same-folder password
    /// is attempt #1.
    pub const MAX_ARCHIVE_PASSWORDS: usize = 8;

    /// Remember a password that just unlocked an encrypted archive — used for BOTH **harvest**
    /// (a new user-entered password) and **MRU promotion** (a cached password that just
    /// worked). Deduped (an existing equal entry moves to the front), empty ignored, truncated
    /// to [`MAX_ARCHIVE_PASSWORDS`](Self::MAX_ARCHIVE_PASSWORDS). RAM-only, never persisted.
    pub fn remember_archive_password(&mut self, pw: &crate::SecretString) {
        if pw.is_empty() {
            return;
        }
        self.archive_passwords.retain(|p| p != pw);
        self.archive_passwords.insert(0, pw.clone());
        self.archive_passwords.truncate(Self::MAX_ARCHIVE_PASSWORDS);
    }

    /// A MRU-ordered snapshot of the session passwords for the shell's archive-open worker to
    /// auto-try before prompting. Cheap — at most [`MAX_ARCHIVE_PASSWORDS`](Self::MAX_ARCHIVE_PASSWORDS)
    /// short strings.
    pub fn archive_passwords_snapshot(&self) -> Vec<crate::SecretString> {
        self.archive_passwords.clone()
    }

    /// Wipe the session password cache (teardown). The `Vec` drop zeroizes each entry; doing
    /// it explicitly keeps the privacy guarantee auditable and covers a shell that terminates
    /// via `exit()` without running `Drop` (macOS).
    pub fn clear_archive_passwords(&mut self) {
        self.archive_passwords.clear();
    }

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
        // Paint the first frame **synchronously**, the way startup (`initial_image`) and a
        // resize/rotation (`load_current_sync`) do — so an archive deck never depends on the
        // async prefetch decode landing to show anything. Without it there's an intermittent
        // blank-deck race: a geometry-epoch bump (the window settling to its real size, the
        // just-closed password/loading dialog) can land between `rebuild_playlist`'s prefetch
        // request and that first decode, so the decode is dropped as stale (`drain_results`) and
        // nothing re-issues it — the deck sits blank until a resize forces a sync re-decode
        // (owner-reported, first-archive-in-a-session). The synchronous decode closes the window
        // the race lives in; the async prefetch still upgrades neighbours. Guarded on a live
        // renderer so headless / unit contexts (no renderer) skip the decode entirely.
        if self.renderer.is_some() {
            self.load_current_sync();
        }
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
        // Perf (PB_PERF): start the open→first-photo clock now, *before* the archive/scan
        // worker — that wait (central-directory read, a networked ZIP, a big scan) is part of
        // what the user feels, so metric 1 has to include it.
        self.perf.open_begin(self.now);
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
                let r = crate::scan::resolve_playlist(&src, &cursor, self.settings.show_archives);
                if r.source.is_empty() {
                    eprintln!("{}: no supported images in that selection", crate::APP_NAME);
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
        // A minimized window reports a 0×0 client area. That is not a geometry change to
        // *react* to: clamping it to a 1×1 fit box (below) would reconfigure the swapchain and
        // decode-to-fit at a single pixel, so on restore that 1-px frame is upscaled to a solid
        // color until the full re-decode lands — the "flash on restore". Leave every bit of
        // geometry untouched, so restoring to the same size is a no-op that rebinds the resident
        // texture. (A real DPI change arrives via `ScaleFactorChanged` with the live, non-zero
        // viewport, so this never swallows one.)
        if width == 0 || height == 0 {
            return;
        }
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
        // DIAGNOSTIC (stuck-preview desync): a transient SMALL viewport during a fullscreen
        // transition makes decode_fit() tiny, so a "full" decode-to-fit lands at ~256px
        // (is_preview=false) and strands as an untracked, never-sharpened slot. The 0x0 guard
        // above only catches a fully-minimized window, not a transient small-but-nonzero size.
        if sharp_diag() && new_fit.max_width.min(new_fit.max_height) < 400 {
            eprintln!(
                "[sharp-diag] resize to SMALL viewport {}x{} (scale={scale}) — fit-collapse suspect for the stuck-preview bug",
                width, height
            );
        }
        if Some(new_fit) == self.fit {
            return;
        }
        // #123 fix 2: FIRST event of a resize burst — stash the outgoing on-screen Fit
        // before `self.fit` mutates and before the `resize_hold` Original rebind below
        // (capture-once: later debounced events must not retag the texture with
        // transient mid-transition geometries).
        if self.resize_settle_at.is_none() {
            self.capture_fit_stash(new_fit);
        }
        self.fit = Some(new_fit);
        if let Some(r) = self.renderer.as_mut() {
            r.resize(width, height);
        }
        // #106.7 §6 — instant, SHARP resize/fullscreen (the Fit↔1:1 toggle already rebinds like
        // this; only a true window resize was left on the old road). The fit box just changed, so
        // the fit-sized texture the renderer GPU-refit above is now the WRONG size: upscaling a
        // windowed fit to fullscreen is blurry, and the deferred settle then re-decodes
        // preview-first (the EXIF-thumbnail flash the owner sees). If the current photo's full-res
        // `Original` is resident (the parked tier holds it while parked), rebind to it NOW — the
        // GPU downscales full-res to ANY size sharply, so the frame is crisp immediately, and it
        // becomes the renderer's `held` across the settle's ring rebuild, bridging to the fresh
        // Lanczos Fit with no preview flash. `resize_hold` makes the settle's preview quality-
        // monotonic (see `drain_results`). Falls through to the old upscale+re-decode when no
        // Original is held (radius 0, just-blazed, or excluded: RAW/SVG/video/gigapixel).
        if let Some(item) = self.displayed_item {
            if self.target_item == Some(item) {
                if let Some(orig) = self.ring.original_slot(item) {
                    self.resize_hold = Some(item);
                    self.present_item(item, orig);
                }
            }
        }
        // A resize in flight pauses playback — freeze together, resume together.
        // The modal drag loop stalls the presenter while audio plays on; every
        // clock-catch-up scheme afterward either races the backlog or churns
        // seeks (tried, regressed, reverted). Pausing both sides loses nothing:
        // the settle arm below resumes exactly where playback froze.
        // Session-backed only (Windows/Linux presenter stall). The macOS native AVPlayer is
        // composited by the window server and keeps playing across a resize — no pause needed.
        if self
            .video
            .as_ref()
            .and_then(ActiveVideoBackend::as_session)
            .is_some_and(|s| s.session.state() == crate::video::VideoSessionState::Playing)
        {
            let now = self.now;
            self.video
                .as_mut()
                .and_then(ActiveVideoBackend::as_session_mut)
                .expect("checked above")
                .session
                .pause(now);
            self.effects.push(contract::CoreEffect::PauseVideoAudio);
            self.video_paused_by_resize = true;
        }
        // Deferred crisp decode-to-fit + ring refill once the size settles (`self.now` is stamped
        // by the host at the start of the event). A resize caused by a discrete fullscreen
        // toggle settles fast — the 180 ms debounce exists for drag-resize streams (#110 §4).
        // A transition that emits straggler size events >50 ms apart can settle more than once
        // (Codex 110b review P2); that is tolerable BECAUSE item-6 retains the Original across
        // each invalidation, so every extra settle is a ms-scale rebind+derive, never a CPU
        // re-decode.
        let settle = if self
            .fullscreen_toggled_at
            .is_some_and(|t| self.now.saturating_duration_since(t) < Duration::from_millis(500))
        {
            FULLSCREEN_SETTLE
        } else {
            RESIZE_SETTLE
        };
        self.resize_settle_at = Some(self.now + settle);
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
    /// zoom/pan, the gated self-paced nav advance (hold-to-blaze) and the slideshow, re-issue the
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
        // 0'. A delete blocked by a retiring video reader retries here (bounded).
        self.poll_delete_retry();
        // 1. Absorb finished decodes (uploads; presents the target if it arrived).
        self.drain_results();
        // 1'. macOS archive video (off the event loop): land a finished playback byte read
        // (→ PlayVideoBytes), then keep Swift-generated posters flowing for the displayed
        // clip + the prefetch window ahead. See .taskmaster/plans/macos-archive-video-posters.md.
        #[cfg(target_os = "macos")]
        {
            self.drain_archive_video_read();
            self.request_archive_posters();
        }

        // 1a'. Land finished thumbnail derives (task #83); re-signal the strip so
        // the shell re-pulls tiles as they stream in.
        if self.thumbs.poll(self.playlist.current().unwrap_or(0)) && self.thumbs_visible() {
            self.emit_panels_changed();
        }

        // 1a. Bound the regenerable per-item caches so browsing tens of thousands of photos
        // can't grow them without limit. Cheap when under the high-water mark (length checks).
        self.trim_caches();

        // 1b. Pick up a finished off-thread animation decode (kicked by `P` / frame-step) and
        // install playback — never on the still/keypress hot path (#37).
        self.poll_anim_decode();
        // 1b'. Drain any newly decoded frames from a streaming Live Photo decode (task #69):
        // install/extend the playing sequence so it plays while the rest still decodes.
        self.poll_anim_stream();
        // 1b''. Drive active video playback (task #79 phase 4): absorb producer frames,
        // present the due one, run the preroll/rebuffer state machine, keep the
        // position pill's second in step.
        self.poll_video();
        self.update_video_progress();

        // 1c. Pick up a finished off-thread text scan (OCR + QR, task #45): cache it,
        // refresh the `T` panel's busy state, run a deferred copy.
        self.poll_text_scan();
        self.poll_details_probe();
        // 1c'. Rebuild the subtitle overlay for the playhead (task #90). Cheap when nothing
        // changed, and free unless a *video* is showing — with `always_forced` on (the
        // default) "subtitles off" no longer means free, because the tick has to get far
        // enough to look for a forced track (task #99). Still nothing on the photo path.
        self.tick_subtitles();

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
            let caught_up = self.target_caught_up();
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
        // dialog, and readiness-gated like hold-to-blaze (a not-ready slide holds, never skips).
        // An explicitly-played video suspends the advance until playback ends/stops (task #79
        // action matrix) — the slideshow otherwise lands on posters and moves on normally.
        let video_playing = self.video.as_ref().is_some_and(|v| {
            !matches!(
                v.state(),
                crate::video::VideoSessionState::Ended
                    | crate::video::VideoSessionState::Failed
                    | crate::video::VideoSessionState::Stopped
            )
        });
        let slideshow_running =
            self.slideshow.on && self.held_nav().is_none() && !self.dialog_open && !video_playing;
        if slideshow_running {
            let caught_up = self.target_caught_up();
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
        // sharpen is outstanding so `drain_results` catches it. The ADR-024 watchdog also opens
        // this gate when a displayed preview has lingered past the deadline with `held_nav`
        // stuck `Some` (the lost-key-up race) — its firing edge forces one re-issue because the
        // change-detection can't see an eligibility change that leaves the wanted-set equal.
        let watchdog_fired_now = self.update_preview_watchdog();
        let mut sharpen_pending = false;
        // Sharpen-via-derive first ("+1 never waits"): a displayed preview whose Original
        // is resident sharpens on the GPU in ~a millisecond, right here — the CPU sharpen
        // below then never gets queued for it (`fulls_wanted` sees the preview gone).
        // OUTSIDE the held gate (#122 item 1): the derive is rebind-class GPU cost, so a
        // tap's advance sharpens on the same tick with the key still down — no preview
        // flash. It self-defers during the actual auto-repeat (blaze) phase.
        self.try_gpu_sharpen();
        if self.held_nav().is_none() || self.preview_watchdog_fired() {
            let upgrade = self.fulls_wanted();
            if upgrade != self.last_upgrade_set || watchdog_fired_now {
                self.last_upgrade_set = upgrade.clone();
                self.request_prefetch();
            }
            sharpen_pending = !upgrade.is_empty();
        }

        // 4. Info panel visibility. "Blaze mode" = actually blazing (a nav key held past the tap
        // delay): hide the panel so it isn't a strobing distraction. Otherwise keep it shown +
        // tracking the current photo. Left untouched mid zoom/pan.
        let blazing = nav.is_some() && past_delay;
        // 4a′. Flash the "Press P to play" hint once on settling on an animated still.
        self.maybe_show_anim_hint(blazing);
        // 4a. The basic info line (`i`) — its own ephemeral layer, so it runs before
        // the rich panel (whose bottom lift reads the line's shown state). Same
        // blaze-hide + settle-track behavior as the panel, but never needs Help's
        // static exception since the line always describes a photo. Also suppressed
        // while `Tab`-hidden — the eager `refresh_info_line_visibility` applies that
        // the instant `hidden` flips, but this tick keeps it from popping back on its
        // own next-photo/settle logic while still hidden.
        if self.info_line {
            if blazing || self.panels.hidden {
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
            if blazing {
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
            // NOT while blazing: OCR and describe are expensive (a per-photo OCR thread, or a
            // describe network round-trip), and at blaze speed they'd fire on every photo you
            // pass, starving decode and stuttering the flight. Only kick when settled — the
            // current photo gets scanned the moment you stop (the HUD path gets this for free
            // via its blaze-suppressed `show_overlay`; the native path needs the guard explicitly).
            //
            // Also hard-cap concurrency: only auto-kick when no scan is already in flight.
            // Each scan `std::thread::spawn`s an uncancellable job that full-res-decodes +
            // OCRs/describes (holding a whole image), so without this a fast walk — or a slow
            // network volume where each job lives for seconds — piles up resident full-res
            // decodes until OOM. Deferring (vs. superseding) keeps at most one auto job alive;
            // the explicit Copy Text / D commands still supersede for responsiveness.
            if self.inspector_panel_visible() && self.current.is_some() && !blazing {
                match self.panels.inspector {
                    // Warm the Details EXIF for the *displayed* photo. Cheap and safe (unlike
                    // the OCR/describe scans below): for a still `ensure_exif_cached` is a
                    // synchronous metadata parse — bytes read + `read_exif_fields`, bytes
                    // dropped — no full-res decode; for a video it hands the container open to
                    // a worker (task 98.6) and returns immediately. Either way it never blocks
                    // on a decode and is idempotent (returns early when cached). The
                    // native path needs this explicitly (the HUD path warmed it in
                    // `show_overlay`, suppressed here); without it the Details tab shows only
                    // the basic rows until a Describe round-trip warms the cache as a side
                    // effect. `!blazing` keeps it off the fast-flick hot path.
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
            // swap or a field toggle) — tracks during hold-to-blaze like the tree, since the
            // readout answers "which photo is this".
            let snap = self.info_line_snapshot();
            if snap != self.last_info_snap {
                self.last_info_snap = snap;
                self.emit_panels_changed();
            }
        }

        // 4a″. Folder tree (⇧F): keep it tracking the displayed photo's folder — the
        // whole point is "you are here", so it tracks **during hold-to-blaze too**
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
                    } else if let Some(t) = target {
                        // A folder re-roots the deck; an archive opens as its own deck (task #108).
                        self.open_disk_target(t);
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
                let throttled = blazing
                    && self
                        .folder_tree_panel
                        .as_ref()
                        .is_some_and(|p| now.duration_since(p.built) < Self::TREE_FLY_REBUILD);
                if !throttled {
                    self.show_folder_tree_mode(blazing);
                }
            } else if !blazing && stored == Some(lite_sig.as_str()) {
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
                self.refresh_after_geometry_change();
                // Resume the playback the resize paused — unless the user paused
                // it themselves meanwhile (the state must still be Paused).
                if std::mem::take(&mut self.video_paused_by_resize) {
                    if let Some(s) = self
                        .video
                        .as_mut()
                        .and_then(ActiveVideoBackend::as_session_mut)
                    {
                        if s.session.state() == crate::video::VideoSessionState::Paused {
                            s.session.resume(now);
                            self.effects.push(contract::CoreEffect::ResumeVideoAudio);
                            self.draw();
                        }
                    }
                }
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

        // 4f'. The flashed video-seek OSD (the info line standing in for the old
        // position toast) lapses at its deadline — the shell re-pulls and drops it.
        let osd_wake = match self.video_osd_until {
            Some(at) if now >= at => {
                self.video_osd_until = None;
                self.emit_panels_changed();
                None
            }
            other => other,
        };

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
        // — only when settled (never while blazing), so it never competes with the blaze hot path.
        let prep_wake = if blazing {
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
        // The ADR-024 watchdog schedules its OWN deadline: parked on a poisoned photo nothing
        // else keeps the loop ticking (`fulls_wanted` is empty → `sharpen_pending` false), and
        // an armed-but-unfired watchdog would sleep straight past its 2 s (Codex P1 — the
        // fake-clock tests tick manually and masked this).
        let watchdog_wake = self
            .preview_watchdog
            .and_then(|w| (!w.fired).then(|| w.since + PREVIEW_WATCHDOG_AFTER));
        // The earliest of the viewer's poll, the animation's next-frame deadline, the eager-prep
        // dwell, the Live-Photo-revert deadline, and the watchdog's; `None` = idle. (The host
        // mins in its own dialog-repaint clock.)
        let wake = [
            base_wake,
            anim_wake,
            prep_wake,
            revert_wake,
            osd_wake,
            watchdog_wake,
        ]
        .into_iter()
        .flatten()
        .min();
        self.effects.push(contract::CoreEffect::SetWake(wake));
    }

    /// Bound the regenerable per-item caches (metadata / EXIF / OCR text / AI descriptions) so
    /// browsing tens of thousands of photos in one session can't grow them without limit. Keeps
    /// the entries **nearest the current photo** — the ones a blaze-back or neighbor revisit will
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
        subtitles: bool,
        save_rotation_enabled: bool,
        reveal_enabled: bool,
        cancel_scan_enabled: bool,
        undo: Option<String>,
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
            // The docked toolbar (#61) is a shell-honored setting, not derived from view state,
            // so the choke point defaults it off and the shell overrides it from `settings`.
            show_toolbar: false,
            // Show Archives (task #104) is likewise a setting, not derived view state: default
            // it off here and let each shell override it from `settings.show_archives`.
            show_archives: false,
            mute_live_audio,
            subtitles,
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
        // #124: past 1:1 the fit-sized texture no longer has the pixels; bind the resident
        // full-res Original. After the zoom math, never before (see `reconcile_zoom_rep`).
        self.reconcile_zoom_rep();
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
    /// boundary every frame at full blaze speed re-rasterizes at most ~10×/s (a ~1 ms
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
            self.left_tab = crate::overlay::LeftTab::Folders;
            self.show_folder_tree();
            self.refresh_slot();
            return;
        }
        if self.folder_tree_open && self.left_tab == crate::overlay::LeftTab::Thumbnails {
            // ⇧F while the Thumbnails tab is showing: switch tabs, don't close —
            // the Inspector's per-tab semantics for the left pane (task #83).
            self.left_tab = crate::overlay::LeftTab::Folders;
            self.show_folder_tree();
            self.emit_panels_changed();
            return;
        }
        self.folder_tree_open = !self.folder_tree_open;
        self.left_tab = crate::overlay::LeftTab::Folders;
        if self.folder_tree_open {
            self.show_folder_tree();
        } else {
            self.hide_folder_tree();
        }
        self.emit_panels_changed();
    }

    /// Whether the Thumbnails strip is the visible left-pane content (task #83).
    pub fn thumbs_visible(&self) -> bool {
        self.folder_tree_open
            && self.left_tab == crate::overlay::LeftTab::Thumbnails
            && !self.panels.hidden
    }

    /// `Shift+T` (task #83) — the Inspector's per-tab semantics for the left
    /// pane's Thumbnails tab: open the pane on it, switch to it if Folders is
    /// showing, close the pane if it's already showing. While Tab-hidden:
    /// reveal + show, never close (the reveal rule).
    pub fn toggle_thumbnails(&mut self) {
        use crate::overlay::LeftTab;
        if !self.native_thumbs {
            return; // no strip presenter on this shell yet (winit: task #83 phase 7)
        }
        if self.panels.reveal() {
            self.folder_tree_open = true;
            self.left_tab = LeftTab::Thumbnails;
            self.on_thumbs_opened();
            return;
        }
        if self.folder_tree_open && self.left_tab == LeftTab::Thumbnails {
            self.folder_tree_open = false;
            self.hide_folder_tree();
            self.emit_panels_changed();
            return;
        }
        self.folder_tree_open = true;
        self.left_tab = LeftTab::Thumbnails;
        // The HUD/native tree yields the pane; its bitmaps clear here (the strip
        // is the pane's content now).
        self.hide_folder_tree_visuals_for_tab_switch();
        self.on_thumbs_opened();
    }

    /// Clear the drawn tree's visuals without closing the pane (a tab switch to
    /// Thumbnails): the CPU tree quad / panel state drops; `folder_tree_open`
    /// stays true because the pane is still open — on the Thumbnails tab.
    fn hide_folder_tree_visuals_for_tab_switch(&mut self) {
        let was_open = self.folder_tree_open;
        self.hide_folder_tree();
        self.folder_tree_open = was_open;
    }

    /// First-open / re-open bookkeeping for the strip (task #83): enable capture
    /// (the T0 byproduct hook costs nothing until this), land the follow scroll
    /// on the current item, and kick fills.
    fn on_thumbs_opened(&mut self) {
        self.thumbs.enable();
        if let Some(cur) = self.playlist.current() {
            if let Some(cmd) = self.thumbs.follow.panel_opened(cur) {
                self.thumbs.pending_scroll = Some(cmd);
            }
        }
        self.request_prefetch();
        self.emit_panels_changed();
    }

    /// A strip click (task #83): absolute jump + the instant thumb-preview
    /// present for cold targets — preview-first applied to jumps. The cached
    /// thumb rides the normal synthetic-outcome upload path (the macOS
    /// archive-poster pattern): it lands as a resident *preview*, presents, and
    /// the real decode — queued at top priority by `request_prefetch` — upgrades
    /// it in place. No flash of black, no wait, and the ring is never evicted
    /// out-of-policy (the target legitimately owns a slot now).
    pub fn thumb_jump(&mut self, item: usize) {
        self.flush_pending_delete();
        if item >= self.source.len() {
            return;
        }
        if self.displayed_item != Some(item) {
            self.stop_playback();
            self.playlist.jump_to(item);
            self.target_item = self.playlist.current();
            if !self.try_present_target() {
                if let Some(e) = self.thumbs.cache.get(item) {
                    let img = pb_decode::DecodedImage {
                        width: e.w,
                        height: e.h,
                        orig_width: e.payload.orig_w,
                        orig_height: e.payload.orig_h,
                        codec: e.payload.codec,
                        format: pb_decode::PixelFormat::Rgba8,
                        pixels: e.payload.rgba.clone(),
                        is_preview: true,
                        color: pb_decode::ColorTransform::srgb(),
                        peak: 1.0,
                        animated: None,
                    };
                    let rep_kind = self.display_kind();
                    self.pending_uploads
                        .push(crate::decode_pool::Outcome::synthetic(
                            item,
                            self.epoch,
                            self.content_gen,
                            rep_kind,
                            Ok(img),
                        ));
                    // A click is a discrete pointer action, not the keypress
                    // frame: a <=1 MiB upload lands now (plan §4).
                    self.drain_results();
                }
            }
            self.request_prefetch();
        }
        if let Some(cmd) = self.thumbs.follow.jump(item) {
            self.thumbs.pending_scroll = Some(cmd);
        }
        self.emit_panels_changed();
    }

    /// T0 capture (task #83): the ring upload just finished with this outcome's
    /// CPU buffer — hand it to the derive thread instead of dropping it. O(1)
    /// (a bounded `try_send`); a no-op until the strip is first opened.
    fn thumbs_capture(&mut self, o: crate::decode_pool::Outcome) {
        if o.key.purpose != crate::decode_pool::Purpose::Display {
            return;
        }
        let item = o.key.item;
        // A VIDEO's displayed image IS its poster — the product of a multi-second scored
        // walk (300–1600 ms over SMB). Retain it even when the strip has never been opened,
        // exactly like the Windows selection path does (`land_selection_tile`): the walk is
        // already paid for, so discarding the tile is pure waste, and refilling it later
        // costs another whole walk at the bottom of the priority list. That is the "open the
        // strip and wait" report, and on the platforms with no #114 selection pipeline
        // (macOS, Linux) this hook is the ONLY thing that can retain a poster tile.
        //
        // The asymmetry vs photos is deliberate and is the whole reason `enabled` exists:
        // a photo thumb is a cheap local re-decode, so paying a derive for every displayed
        // photo would put one on every frame of a blaze. A video thumb is a network walk.
        // `enable_capture()` (not `enable()`) keeps fill planning + the photo byproduct
        // derive gated on the panel actually being opened.
        let is_video = matches!(
            crate::video::item_kind(self.source.as_ref(), item),
            crate::video::LibraryItemKind::Video(_)
        );
        if is_video {
            self.thumbs.enable_capture();
        } else if !self.thumbs.enabled {
            return;
        }
        if let Some(img) = o.into_image() {
            self.thumbs.offer(item, img);
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
        self.native_tree
            && self.panels.tree_visible(self.folder_tree_open)
            && self.left_tab == crate::overlay::LeftTab::Folders
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
        // Rebuild when there's no tree, the current folder left its root, OR the Show Archives
        // setting no longer matches what the tree was read with (task #108) — a live toggle then
        // refreshes the rows (its already-loaded children were read under the old setting).
        let show_archives = self.settings.show_archives;
        let rebuild = self.fs_tree.as_ref().is_none_or(|t| {
            current.strip_prefix(t.root()).is_err() || t.show_archives() != show_archives
        });
        if rebuild {
            let root = self
                .root
                .parent()
                .filter(|p| !p.as_os_str().is_empty() && current.starts_with(p))
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root.clone());
            let (tx, rx) = std::sync::mpsc::channel();
            let mut tree = crate::fs_tree::FsTree::new(root);
            tree.set_show_archives(show_archives);
            self.fs_tree = Some(tree);
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
        let done: Vec<(PathBuf, Vec<crate::folder_tree::DiskTarget>)> = self
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
                // Read subfolders always, and archive files too when Show Archives is on
                // (task #108) — an archive shows as a leaf zipper row inside the folder.
                let show_archives = self.settings.show_archives;
                std::thread::spawn(move || {
                    let children = crate::folder_tree::dir_children(&path, show_archives);
                    let _ = tx.send((path, children));
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

    /// Open a row from the tree (a name click): a folder re-roots the deck; an **archive** row
    /// (task #108) opens the archive as its own deck (the door / File-open path). The row's
    /// kind is taken from the resident tree — never re-classified from the extension, so a real
    /// folder that happens to be named `foo.zip` still opens as a folder.
    pub fn fs_tree_open(&mut self, path: PathBuf) {
        let is_archive = self
            .fs_tree
            .as_ref()
            .is_some_and(|t| t.is_archive_row(&path));
        if is_archive {
            self.open_disk_target(crate::folder_tree::DiskTarget::Archive(path));
        } else {
            self.open_dir(path);
        }
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
        self.open_dir_at(dir, None);
    }

    /// Like [`open_dir`](Self::open_dir), but land the deck cursor on `at` (a file in the
    /// folder) instead of the first item — Open Parent uses it to land on the archive **door**
    /// you climbed out of (task #108). `at` must be a file the scan will actually surface (an
    /// archive is only surfaced when `show_archives` is on), or the streaming scanner would gate
    /// its first batch on an unreachable target and show nothing — the caller gates that.
    pub fn open_dir_at(&mut self, dir: PathBuf, at: Option<PathBuf>) {
        let plan = pb_core::open::plan(pb_core::open::LaunchInput::Directory(dir));
        let cursor = match at {
            Some(p) => pb_core::open::Cursor::At(p),
            None => plan.cursor,
        };
        self.open_plan(plan.source, cursor);
    }

    /// Open a typed [`DiskTarget`](crate::folder_tree::DiskTarget) (task #108): a folder as a
    /// folder deck, or an archive as its own deck (the full door / File-open path — password
    /// prompt, RAM pre-flight, progress). The shared activation for the Go-sibling walk and the
    /// tree rows, so neither can drift on how an archive opens.
    pub fn open_disk_target(&mut self, target: crate::folder_tree::DiskTarget) {
        match target {
            crate::folder_tree::DiskTarget::Directory(p) => self.open_dir(p),
            crate::folder_tree::DiskTarget::Archive(p) => self.open_plan(
                pb_core::open::Source::Archive(p),
                pb_core::open::Cursor::First,
            ),
        }
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
        let source: Arc<dyn ItemSource> = if prefix.is_empty() {
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
            // Climbing out of an archive **root**, land on that archive's door (task #108), so
            // `space` continues past it instead of restarting at the folder's first item — the
            // owner's "more consistent" fix. `self.root` is the archive file for an archive deck.
            // Gate on `show_archives`: with archives hidden the door isn't in the scan, and the
            // streaming scanner would wait for that unreachable target before showing anything
            // (Codex review) — so fall back to the first item there.
            let at = (self.archive_scope.is_some() && self.settings.show_archives)
                .then(|| self.root.clone());
            self.open_dir_at(par.clone(), at); // clears climb_anchor (via open_plan)…
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
            // Scoped into an internal folder: step the archive's internal sibling folders
            // (in-RAM over the entry names), exactly as before.
            if !scope.prefix.is_empty() {
                let full = Arc::clone(&scope.full);
                let names = (0..full.len()).map(|i| full.name(i));
                match crate::folder_tree::sibling_scope(names, &scope.prefix, dir) {
                    Some(sib) => self.rescope_archive(sib),
                    None => self.show_toast("No more folders with images"),
                }
                return;
            }
            // At the archive **root** (the whole-archive deck): step through the archive's own
            // internal folders first — jump the cursor to the next internal-folder boundary, like
            // Go Next Folder on a disk deck (task #108). Only when there is **no more internal
            // folder** that way do we step to the adjacent archive on disk.
            if let Some(idx) = self.archive_adjacent_folder_item(dir) {
                self.stop_playback();
                self.playlist.jump_to(idx);
                self.target_item = self.playlist.current();
                self.try_present_target();
                self.request_prefetch();
                return;
            }
            // No more internal folders in that direction → the adjacent archive on disk (anchor on
            // the archive file, archives-only, off-thread — the containing folder's `read_dir` can
            // stall on a share). Only when Show Archives is on; otherwise nothing to step to.
            if self.settings.show_archives {
                self.tree_io = Some(crate::folder_tree::spawn_sibling(
                    self.root.clone(),
                    dir,
                    true, // show_archives
                    true, // archives_only — flick archive → archive
                ));
            } else {
                self.show_toast("No more folders with images");
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
        self.tree_io = Some(crate::folder_tree::spawn_sibling(
            anchor,
            dir,
            self.settings.show_archives,
            false, // archives_only=false — a folder deck steps to folder-or-archive siblings
        ));
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

    /// The deck index at the next (`dir > 0`) / previous internal-folder boundary of an
    /// **archive** deck — the archive analog of [`adjacent_folder_item`](Self::adjacent_folder_item),
    /// grouping by the entry name's internal folder ([`folder_of`](crate::folder_tree::folder_of))
    /// since archive entries have no filesystem path (task #108). `None` at the deck's first /
    /// last internal folder — the caller then steps to an adjacent archive on disk.
    fn archive_adjacent_folder_item(&self, dir: i32) -> Option<usize> {
        let n = self.source.len();
        let c = self.displayed_item.filter(|&c| c < n)?;
        let folder = |i: usize| crate::folder_tree::folder_of(self.source.name(i)).to_string();
        let cur = folder(c);
        if dir > 0 {
            (c + 1..n).find(|&i| folder(i) != cur)
        } else {
            let mut s = c;
            while s > 0 && folder(s - 1) == cur {
                s -= 1;
            }
            if s == 0 {
                return None; // already in the archive's first internal folder
            }
            let prev = folder(s - 1);
            let mut p = s - 1;
            while p > 0 && folder(p - 1) == prev {
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

    /// Refresh rate in Hz (rounded, ≥1) — caps the Settings blaze-speed slider and is
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
                let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(remaining));
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
        self.video_resume.clear();
        self.poster_sel.reset(self.content_gen);
        self.retry.reset(); // task #114: index-keyed, deck-scoped
        self.meta_cache.clear();
        self.exif_cache.clear();
        self.dovi_warned.clear();
        self.recognized_text.clear();
        self.text_scan = None;
        self.text_gen += 1;
        self.details_probe = None;
        self.details_gen += 1;
        self.descriptions.clear();
        self.describe_scan = None;
        self.describe_gen += 1;
        self.live_motion_cache.clear();
        self.failed.clear();
        self.thumbs.clear_deck();
        self.emit_panels_changed();
        self.preview_resident.clear();
        // Indices are deck-relative: a fired watchdog for old-item-N must not carry into a new
        // deck where N names a different photo (it would sharpen instantly instead of re-arming).
        self.preview_watchdog = None;
        self.resize_hold = None; // indices reassigned — any resize hold is meaningless now
                                 // Drop any in-flight archive-video poster requests: item indices are deck-relative,
                                 // so a straggler callback must not upgrade a same-index item in the new deck.
        self.poster_inflight.clear();
        self.pending_poster_bytes.clear();
        self.upgrade_done.clear();
        self.last_upgrade_set.clear();
        // Keep every undo entry (all path-keyed, deck-independent) so the delete that emptied the
        // deck — and any rotation recorded before it — stay undoable; the restore rebuilds a
        // one-photo deck.
        self.invalidate_content();
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
            .push(contract::CoreEffect::SetTitle(crate::APP_NAME.to_string()));
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
        source: Arc<dyn ItemSource>,
        root: PathBuf,
        scan_root: Option<PathBuf>,
        recursive: bool,
        start: usize,
    ) {
        if source.is_empty() {
            return;
        }
        let start = start.min(source.len() - 1);
        // Whether this is the *same* deck reshaped (a delete-advance, recursive toggle, or the
        // undo-restore reinsert) vs a genuinely new one (open, archive, folder switch). Deletion
        // undo entries survive the former (the delete's own rebuild would otherwise wipe the entry
        // it just recorded) but not the latter. Captured before `self.root` is reassigned below.
        let same_root = root == self.root;
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
        // Perf (PB_PERF): the deck size is the open→all-cached target. Doors decode to a flat
        // tile (never read), so a folder of only doors "caches" the instant they present —
        // that's fine, the metric is about photos and a door isn't one.
        self.perf.deck_ready(self.source.len());
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
        self.video_resume.clear();
        self.poster_sel.reset(self.content_gen);
        self.retry.reset(); // task #114: index-keyed, deck-scoped
        self.meta_cache.clear();
        self.exif_cache.clear();
        self.dovi_warned.clear();
        self.recognized_text.clear();
        self.text_scan = None;
        self.text_gen += 1;
        self.details_probe = None;
        self.details_gen += 1;
        self.descriptions.clear();
        self.describe_scan = None;
        self.describe_gen += 1;
        self.live_motion_cache.clear();
        self.failed.clear();
        self.thumbs.clear_deck();
        self.emit_panels_changed();
        self.preview_resident.clear();
        // Indices are deck-relative: a fired watchdog for old-item-N must not carry into a new
        // deck where N names a different photo (it would sharpen instantly instead of re-arming).
        self.preview_watchdog = None;
        self.resize_hold = None; // indices reassigned — any resize hold is meaningless now
                                 // Drop any in-flight archive-video poster requests: item indices are deck-relative,
                                 // so a straggler callback must not upgrade a same-index item in the new deck.
        self.poster_inflight.clear();
        self.pending_poster_bytes.clear();
        self.upgrade_done.clear();
        self.last_upgrade_set.clear();
        // Every undo entry is keyed by stable path (see `crate::undo`), so all survive a same-deck
        // rebuild — the delete-advance that just recorded a Deletion, a recursive toggle, an
        // undo-restore reinsert. This is what lets rotation- and delete-undo entries coexist: a
        // delete's rebuild no longer wipes a rotation recorded before it. A genuinely new deck
        // (different root) clears the whole stack.
        if !same_root {
            self.undo_stack.clear();
        }
        // Invalidate the ring + bump the epoch (discards in-flight old decodes), then refill
        // around the new current photo. No synchronous decode on the event loop (task #18
        // finding #5): the async prefetch decodes the new current preview-first and presents
        // it when ready. `invalidate_geometry` (above) still reads the *old* `current` dims
        // for its ring-size estimate, so clear the stale metadata only afterward. Nothing is
        // presented yet (`displayed_item = None`), so readiness holds the old deck's frame
        // (kept by the renderer) with the loading pie until the first new frame lands.
        // A new deck reassigns every index → content change (purges retained Originals, #106.7).
        self.invalidate_content();
        // Drop the old deck's metadata (a genuinely new frame is incoming) and mark it
        // un-presented at this epoch: `displayed_item` still names the logical current index,
        // but `presented_epoch = None` makes `target_caught_up` false, so `drain_results`
        // presents the new current when its async decode lands. The renderer holds the old
        // frame (with the loading pie) until then — no synchronous decode on the loop.
        self.current = None;
        self.displayed_item = self.playlist.current();
        self.target_item = self.playlist.current();
        self.presented_epoch = None;
        if door_diag() {
            eprintln!(
                "[door-diag] rebuild_playlist src_len={} first={:?} scan_root={:?} recursive={} start={} epoch={} content_gen={}",
                self.source.len(),
                (!self.source.is_empty()).then(|| self.source.name(0)),
                self.scan_root,
                self.recursive,
                self.displayed_item.unwrap_or(0),
                self.epoch,
                self.content_gen,
            );
        }
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
    /// hold-to-blaze, then either advances, or — when we're still catching up to the
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
    /// Apply the session-only CLI launch overrides (task #78) to live/launch state. Called once
    /// by the shell right after constructing the core. **Never mutates [`Self::settings`]**, so no
    /// override can leak to disk on a later save (privacy #2): the settings-shaped values that are
    /// read *live* (`--theme`, `--mute`) are served from `self.launch` via [`Self::effective_appearance`]
    /// / [`Self::effective_mute`]; the rest map to transient live state (view mode, info line, nav
    /// direction, panels, slideshow). `--start-at` and the shuffle/reverse *start position* are
    /// resolved against the deck later (they need the — possibly deferred — scan to list its entries).
    pub fn apply_launch_overrides(&mut self, overrides: &LaunchOverrides) {
        self.launch = overrides.clone();
        // Initial scale mode (8/9/0 still switch it live afterward).
        if let Some(scale) = self.launch.scale {
            self.view.mode = scale_mode_of(scale);
        }
        // Info line launch state (--info / --no-info, else the saved default).
        self.info_line = self
            .launch
            .show_info
            .unwrap_or(self.settings.show_image_info);
        // --shuffle / --reverse pick the launch nav; the slideshow + hold-to-blaze auto-advance in
        // it (manual Next/Prev still steer normally). See `LaunchOverrides::launch_nav`.
        self.last_nav = self.launch.launch_nav();
        if self.launch.open_details {
            self.panels.open_inspector(InspectorTab::Details);
        }
        if self.launch.open_folders {
            self.folder_tree_open = true;
        }
        if let Some(ss) = self.launch.slideshow {
            if let Some(secs) = ss.interval_secs {
                self.slideshow.interval = slideshow::clamp_interval(Duration::from_secs_f64(secs));
            }
            self.slideshow.on = true;
            self.last_present = Some(self.now);
        }
        // --theme / --mute deliberately stay OUT of `self.settings` (see the doc comment); the
        // effective_* helpers below fold them in for live reads.

        // Start position for a deck already resolved at construction (an explicit file list). A
        // deferred folder / archive scan is empty here, so it applies the same start when its first
        // batch bootstraps (`apply_scan_batch`), where the deck is finally listed.
        if !self.playlist.is_empty() {
            if let Some(idx) = self.launch_start_index(&*self.source) {
                self.playlist.jump_to(idx);
            }
        }
    }

    /// The initial cursor a `--start-at` / `--shuffle` / `--reverse` launch wants over `source`
    /// (task #78), or `None` when no start override is set (the plan's own cursor stands).
    /// Precedence: an explicit `--start-at` wins; else `--shuffle` starts on a random photo; else
    /// `--reverse` starts on the last. For a **streamed** folder scan `source` is the first batch,
    /// so a large `--start-at N` (or the random pick) is over what has loaded so far.
    fn launch_start_index(&self, source: &dyn ItemSource) -> Option<usize> {
        let len = source.len();
        if len == 0 {
            return None;
        }
        if let Some(start_at) = &self.launch.start_at {
            return Some(match start_at {
                // 1-based on the command line; clamp into the deck.
                StartAt::Index(n) => n.saturating_sub(1).min(len - 1),
                // First file whose base name matches (case-insensitive); fall back to the first
                // photo if not found.
                StartAt::Name(name) => (0..len)
                    .find(|&i| {
                        let full = source.name(i);
                        let base = full.rsplit(['/', '\\']).next().unwrap_or(full);
                        base.eq_ignore_ascii_case(name.trim())
                    })
                    .unwrap_or(0),
            });
        }
        if self.launch.shuffle {
            // Random start: position 0 of a fresh shuffle order over the deck (a random index).
            return ShuffleOrder::new(len, crate::engine::fresh_shuffle_seed())
                .at(0)
                .map(|i| i as usize);
        }
        if self.launch.reverse {
            return Some(len - 1);
        }
        None
    }

    /// The light/dark preference in effect: a `--theme` launch override wins for this session,
    /// else the saved [`settings::AppearanceMode`]. Read at every place the theme is applied so a
    /// scripted `--theme dark` holds without ever touching (or persisting) the saved value.
    pub fn effective_appearance(&self) -> settings::AppearanceMode {
        self.launch.theme.unwrap_or(self.settings.appearance_mode)
    }

    /// Whether Live Photo audio is muted right now: a `--mute` launch override wins for this
    /// session, else the saved `mute_live_audio`. Cleared by an explicit user mute toggle.
    pub fn effective_mute(&self) -> bool {
        self.launch.mute.unwrap_or(self.settings.mute_live_audio)
    }

    pub fn advance(&mut self, nav: Nav) {
        // Any in-deck navigation ends an Open-Parent (⌘↑) climb: the next ⌘↑ must restart
        // from the folder you navigated to, not resume from the stale climb rung (which would
        // surprise-jump to a near-root folder). All photo nav — Next/Prev/Random and the
        // hold-to-blaze re-advance — funnels through here.
        self.climb_anchor = None;
        // Settle a deferred delete-advance before navigating, so a keypress during the
        // brief post-delete delay lands cleanly on the rebuilt playlist (no yank-back).
        self.flush_pending_delete();
        // Never advance while the previous target is still pending (a miss in
        // flight): a fast second press would overwrite it and skip that photo.
        // Holding still blazes — `about_to_wait` re-advances once it's caught up.
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
        // The strip follows the nav (task #83) — same signal cadence as the
        // info line's per-photo refresh.
        if self.thumbs_visible() {
            if let Some(cur) = self.playlist.current() {
                if let Some(cmd) = self.thumbs.follow.navigation(cur) {
                    self.thumbs.pending_scroll = Some(cmd);
                }
            }
            self.emit_panels_changed();
        }
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
        // #124: AFTER `zoom_about` -- it reads the currently bound texture's dims to keep the
        // cursor anchor pinned, so a rebind first would anchor against the wrong dims.
        self.reconcile_zoom_rep();
        self.push_view();
        self.draw();
        // Zooming changes whether the image overflows — update the grab affordance
        // immediately (the pointer may not move after a wheel notch / pinch).
        self.refresh_cursor();
    }

    /// Arm the play hint when settling on an item `P` acts on — suppressed while blazing
    /// (the nag the owner flagged) and once the user has engaged (P / step, tracked via
    /// `anim_hint_shown_for`). An eager prep decoding in the background does *not* suppress
    /// it — that's invisible work, and the hint is what invites the user to press P in the
    /// first place.
    ///
    pub fn maybe_show_anim_hint(&mut self, blazing: bool) {
        if blazing || self.playback.is_some() {
            return;
        }
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.anim_hint_shown_for == Some(item) {
            return;
        }
        // Videos show the hint too (task #79: poster + play badge is the UX shape) —
        // deliberately NOT via has_motion, which still gates the animation decode
        // machinery videos must never enter (their bytes never enter RAM). An archive
        // door gets no pill: its affordance is the door card (task #105).
        if self.has_motion(item) || self.item_is_video(item) {
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
        // A video item (task #79 phase 4): its own streaming session — never the
        // animation decode machinery (that would read the file into RAM).
        if self.item_is_video(item) {
            self.video_play_pause(item);
            return;
        }
        // An archive door (task #104): `P` **enters** it. Consistent rather than
        // cute — `P` already acts on whatever the current item contains (play a
        // clip, play an animation, play a Live Photo), and an archive's contents
        // are simply reached by going in. This is the *only* path that ever reads
        // an archive: browsing past a door costs a tile (see `engine`'s dispatch).
        //
        // Routed through `open_plan`, not a hand-pushed effect, so entering is the
        // same operation as opening the archive from the picker: it ends an
        // Open-Parent climb, and the RAM pre-flight, progress dialog and password
        // prompt all come along unchanged. `Alt+Up` climbs back out to the folder
        // of doors (`open_parent_cmd` anchors on the source's container).
        if self.item_archive_kind(item).is_some() {
            if let Some(path) = self.source.path(item).map(Path::to_path_buf) {
                self.open_plan(
                    pb_core::open::Source::Archive(path),
                    pb_core::open::Cursor::First,
                );
            }
            return;
        }
        // Eagerly prepared on dwell → play instantly (no decode wait).
        if self.prepared.as_ref().is_some_and(|p| p.item == item) {
            let anim = self.prepared.take().unwrap().anim;
            self.anim_hint_shown_for = Some(item); // engaged
            self.install_animation(anim, true, 0);
            self.start_live_audio(item);
            return;
        }
        // An eager stream (task #69) is decoding → upgrade it to play and start playing
        // whatever's decoded so far (the rest keeps streaming in).
        if self.anim_stream.is_some() {
            if let Some(s) = self.anim_stream.as_mut() {
                s.want = AnimWant::Play;
            }
            self.anim_hint_shown_for = Some(item);
            self.install_stream_playback(); // no-op until the first frame lands, then installs
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
        // A live video session steps through the session, not `playback` — and a
        // video can't have Live Photo audio, so the silencing below stays animation's.
        if self.video_frame_step(delta) {
            return;
        }
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
        if let Some(s) = self.anim_stream.as_mut() {
            s.want = AnimWant::Step(delta);
            self.anim_hint_shown_for = Some(item);
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

    /// Drain a streaming Live Photo motion decode (task #69): install a playing Playback on
    /// the first frame, extend it as frames arrive, and finalize on `Done` — so the clip
    /// starts within a frame or two instead of after the whole `.mov` decodes. Called each
    /// tick alongside [`poll_anim_decode`](Self::poll_anim_decode). A no-op where no
    /// streaming producer exists (`anim_stream` is only set on the Linux FFmpeg and macOS
    /// AVAssetReader paths).
    pub fn poll_anim_stream(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some((gen, epoch, item)) = self.anim_stream.as_ref().map(|s| (s.gen, s.epoch, s.item))
        else {
            return;
        };
        // Stale (superseded / geometry changed / navigated away): cancel + drop.
        if gen != self.anim_gen || epoch != self.epoch || self.displayed_item != Some(item) {
            self.cancel_anim_stream();
            return;
        }
        // Drain everything available now without holding the receiver borrow.
        let mut msgs = Vec::new();
        let mut disconnected = false;
        {
            let s = self.anim_stream.as_ref().unwrap();
            loop {
                match s.rx.try_recv() {
                    Ok(m) => msgs.push(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for msg in msgs {
            self.apply_stream_msg(msg);
            if self.anim_stream.is_none() {
                return; // a terminal Done/Failed cleared it
            }
        }
        if disconnected {
            // The worker vanished without a terminal message (a panic, or a producer bug).
            // Treat it as a stream failure rather than silently dropping: if a Playback is
            // already installed and incomplete, `stream_failed` marks it complete — else it
            // would park on the decoded frontier forever while the audio played on.
            self.stream_failed("Live Photo stream worker vanished".into());
        }
    }

    fn apply_stream_msg(&mut self, msg: StreamMsg) {
        match msg {
            StreamMsg::Header {
                width,
                height,
                color,
                codec,
            } => {
                if let Some(s) = self.anim_stream.as_mut() {
                    s.header = Some(StreamHeader {
                        width,
                        height,
                        color,
                        codec,
                    });
                }
            }
            StreamMsg::Frame(frame) => self.stream_frame(frame),
            StreamMsg::Done {
                loop_count,
                truncated,
            } => self.stream_done(loop_count, truncated),
            StreamMsg::Failed(e) => self.stream_failed(e),
        }
    }

    /// A streaming frame arrived: extend the live Playback if one's installed, else buffer it
    /// (and, in `Play` mode, install a playing streaming Playback as soon as we can).
    fn stream_frame(&mut self, frame: pb_decode::AnimFrame) {
        let Some((installed, want)) = self.anim_stream.as_ref().map(|s| (s.installed, s.want))
        else {
            return;
        };
        if installed {
            if let Some(pb) = self.playback.as_mut() {
                pb.push_frame(frame);
            }
            return;
        }
        if let Some(s) = self.anim_stream.as_mut() {
            s.pending.push(frame);
        }
        // Eager/Step accumulate until `Done`; Play starts the moment a frame + header exist.
        if matches!(want, AnimWant::Play) {
            self.install_stream_playback();
        }
    }

    /// Install a **playing** streaming [`Playback`] from the stream's header + all frames
    /// buffered so far, and start its audio. Returns whether it installed (needs the header
    /// and at least one buffered frame). Used both for the first `Play` frame and the
    /// eager→`Play` upgrade (play whatever's decoded so far, then keep extending it).
    fn install_stream_playback(&mut self) -> bool {
        let Some(s) = self.anim_stream.as_mut() else {
            return false;
        };
        if s.installed {
            return true;
        }
        if s.header.is_none() || s.pending.is_empty() {
            return false; // header not here yet, or nothing decoded — wait for the first frame
        }
        let header = s.header.as_ref().unwrap();
        let (width, height, codec, color) =
            (header.width, header.height, header.codec, header.color);
        let item = s.item;
        let frames = std::mem::take(&mut s.pending);
        s.installed = true;
        let anim = pb_decode::Animation {
            kind: pb_decode::AnimationKind::LivePhoto,
            width,
            height,
            frames,
            loop_count: 0, // provisional; the real count lands with `Done` → `mark_complete`
            codec,
            color,
            truncated: false,
        };
        self.playback = Some(Playback::new_streaming(anim, true));
        self.present_anim_frame();
        self.start_live_audio(item);
        true
    }

    /// A streaming decode finished. If it's already playing, finalize the live Playback's loop
    /// count (so a finite Live Photo ends instead of looping); otherwise build the accumulated
    /// frames into a complete [`Animation`] and route it by `want` (eager → stash, step → step).
    fn stream_done(&mut self, loop_count: u32, truncated: bool) {
        let Some(installed) = self.anim_stream.as_ref().map(|s| s.installed) else {
            return;
        };
        if installed {
            if let Some(pb) = self.playback.as_mut() {
                pb.mark_complete(loop_count);
            }
            self.anim_stream = None;
            if truncated {
                self.show_toast("Animation truncated");
            }
            return;
        }
        let Some(stream) = self.anim_stream.take() else {
            return;
        };
        let (item, want) = (stream.item, stream.want);
        let Some(anim) = stream.into_animation(loop_count, truncated) else {
            return; // no header/frames — nothing to show
        };
        match want {
            AnimWant::Eager => {
                self.prepared = Some(Prepared { item, anim });
                if self.overlay_shown && self.slot_content() == Some(SlotContent::Details) {
                    self.show_overlay();
                }
            }
            // A Play stream installs on its first frame, so reaching here means it completed
            // before any frame was consumed as "installed" — play the whole thing now.
            AnimWant::Play => {
                self.install_animation(anim, true, 0);
                self.start_live_audio(item);
            }
            AnimWant::Step(delta) => self.install_animation(anim, false, delta),
        }
    }

    /// A streaming decode failed. Mid-playback, treat it as a truncated finish (don't yank the
    /// video away); before any frame, surface it like a batch decode failure (silent for an
    /// eager prep the user never asked for).
    fn stream_failed(&mut self, err: String) {
        let Some((installed, want)) = self.anim_stream.as_ref().map(|s| (s.installed, s.want))
        else {
            return;
        };
        if installed {
            if let Some(pb) = self.playback.as_mut() {
                pb.mark_complete(1);
            }
        } else {
            eprintln!("live photo stream failed: {err}");
            if want != AnimWant::Eager {
                self.show_toast("Can't play this animation");
            }
        }
        self.anim_stream = None;
    }

    /// Stop and drop any playback / in-flight decode / eager prep, reverting to the
    /// still. Called when navigating away or changing source (the frames are RAM-only —
    /// privacy #2).
    pub fn stop_playback(&mut self) {
        self.playback = None;
        self.anim_frame_shown_at = None;
        self.cancel_anim_decode(); // stop an in-flight decode, don't just orphan it
                                   // Video rides the same teardown points (navigate / delete / new source):
                                   // stop the session; the producer exits on the Stop/disconnect and its
                                   // reader retires on a detached thread (never joined here).
        self.stop_video();
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
        if self.effective_mute() {
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
        match self.effective_appearance() {
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
        self.content_top_inset = px;
        if let Some(r) = self.renderer.as_mut() {
            r.set_content_top_inset(px);
        }
    }

    /// The current item's on-screen placement for the **macOS native video layer**
    /// (task 79.9 phase 3): the same geometry the wgpu still renderer computes in
    /// `quad_vertices` — `ViewTransform::placement` against the content region below the
    /// top-bar inset, then slid down by the inset — so the `AVPlayerLayer` tracks Fit/Fill/
    /// Original, zoom, pan, and rotation identically to a photo. Returns
    /// `(x, y, w, h, rotation)` in **physical px, top-left origin**; `w`/`h` are the
    /// *rotated* footprint and `rotation` is the CW quadrant (0/1/2/3 = 0/90/180/270) the
    /// shell rotates the layer by about its center. `None` before the renderer/fit exist.
    /// Take the in-RAM container bytes stashed for a `PlayVideoBytes` (macOS archive
    /// video) — the shell pulls them once and serves them to `AVPlayer` via a resource
    /// loader. Empty if none pending. Consumes the stash.
    pub fn take_pending_video_bytes(&mut self) -> Vec<u8> {
        self.pending_video_bytes.take().unwrap_or_default()
    }

    /// Take the archive-video **poster** bytes stashed for `request_id` (macOS). The shell
    /// pulls them, generates a poster frame, and returns it via `video_poster_ready`. Consumes.
    pub fn take_pending_poster_bytes(&mut self, request_id: u64) -> Vec<u8> {
        self.pending_poster_bytes
            .remove(&request_id)
            .unwrap_or_default()
    }

    /// macOS: keep Swift-generated posters flowing for archive videos — the displayed one
    /// **and the prefetch window ahead**, so advancing to the next clip shows its poster
    /// with no placeholder gap. Two steps each tick:
    ///
    /// 1. Drain finished off-thread byte reads → stash + emit `RequestVideoPoster` (or clear
    ///    the in-flight guard if the read failed).
    /// 2. For archive-video placeholders (resident *previews*) not already in flight — the
    ///    displayed item first, then the direction-biased prefetch targets — spawn an
    ///    off-thread byte read, up to a small concurrency cap (bounds transient RAM: each
    ///    read holds one container copy until Swift consumes it).
    ///
    /// A poster upgrades its placeholder in place and the item leaves `preview_resident`, so
    /// it stops being a candidate; a ring eviction re-placeholders it and it re-qualifies.
    /// macOS: land a finished off-thread archive-video **playback** read — if the session is
    /// still current, stash the bytes and emit `PlayVideoBytes`; a stale read (the user
    /// navigated away) is dropped, an empty one (read error) surfaces a toast.
    #[cfg(target_os = "macos")]
    pub fn drain_archive_video_read(&mut self) {
        while let Ok((id, name, muted, bytes)) = self.video_read_rx.try_recv() {
            let current = self
                .video
                .as_ref()
                .and_then(ActiveVideoBackend::as_native)
                .map(|p| p.session_id)
                == Some(id);
            if !current {
                continue; // a newer session (or none) — the read is stale
            }
            if bytes.is_empty() {
                self.show_toast("couldn't read the video from the archive");
                self.video = None;
                self.update_video_progress();
                continue;
            }
            // A remembered position for this archive item (task #94.2).
            let item = self
                .video
                .as_ref()
                .and_then(ActiveVideoBackend::as_native)
                .map(|p| p.item);
            let start_secs = item
                .and_then(|i| self.video_resume.get(&i))
                .map_or(0.0, |d| d.as_secs_f64());
            self.pending_video_bytes = Some(bytes);
            self.effects.push(contract::CoreEffect::PlayVideoBytes {
                name,
                session_id: id,
                muted,
                start_secs,
            });
        }
    }

    #[cfg(target_os = "macos")]
    pub fn request_archive_posters(&mut self) {
        // Cap concurrent reads/generations: the displayed clip + a couple ahead covers the
        // advance gap without holding many full containers in RAM at once.
        const MAX_INFLIGHT: usize = 3;

        // 1. Finished reads: stash the bytes + ask the shell to generate the poster.
        while let Ok((request_id, item, bytes)) = self.poster_read_rx.try_recv() {
            // A read whose request is no longer the tracked one (the deck changed and a
            // new-deck request re-used the index) is a straggler — drop it whole rather
            // than stash bytes / clear a marker that now belongs to the replacement.
            if self.poster_inflight.get(&item) != Some(&request_id) {
                continue;
            }
            if bytes.is_empty() {
                self.poster_inflight.remove(&item); // read failed — allow a later retry
                continue;
            }
            let name = self.source.name(item).to_string();
            let max_edge = self
                .decode_fit()
                .map(|f| f.max_width.max(f.max_height))
                .unwrap_or(2048)
                .max(1);
            self.pending_poster_bytes.insert(request_id, bytes);
            self.effects.push(contract::CoreEffect::RequestVideoPoster {
                request_id,
                item,
                name,
                max_edge,
            });
        }

        // 2. Spawn reads for the next placeholders, in priority order, up to the cap.
        if self.poster_inflight.len() >= MAX_INFLIGHT {
            return;
        }
        let mut candidates: Vec<usize> = Vec::new();
        if let Some(d) = self.displayed_item {
            candidates.push(d);
        }
        candidates.extend(self.targets.iter().copied());
        for item in candidates {
            if self.poster_inflight.len() >= MAX_INFLIGHT {
                break;
            }
            if self.poster_inflight.contains_key(&item) // already in flight (also dedups this list)
                || self.source.path(item).is_some() // loose file — the pool posters those
                || !self.preview_resident.contains(&item) // placeholder not resident yet
                || !self.item_is_video(item)
            {
                continue;
            }
            self.poster_req_seq += 1;
            let request_id = self.poster_req_seq;
            self.poster_inflight.insert(item, request_id);
            let source = self.source.clone();
            let tx = self.poster_read_tx.clone();
            // Off the event loop: a ZIP entry inflates here (7z copies from resident RAM).
            std::thread::spawn(move || {
                let bytes = source.bytes(item).unwrap_or_default();
                let _ = tx.send((request_id, item, bytes));
            });
        }
    }

    /// A macOS archive-video poster the shell generated (via `AVAssetImageGenerator`) — feed
    /// it into the resident ring as a synthetic full-decode [`Outcome`](crate::decode_pool::Outcome),
    /// upgrading the preview placeholder in place through the normal `drain_results` path
    /// (so retention + prefetch come for free). Dropped if the pixel count is wrong.
    pub fn video_poster_ready(
        &mut self,
        request_id: u64,
        item: usize,
        w: u32,
        h: u32,
        rgba: Vec<u8>,
    ) {
        // Drop a straggler whose request we no longer expect — item indices are
        // deck-relative, so it must not upgrade a same-index item in a new deck. The id
        // check (not just item membership, #119 diff review) is what makes this hold when
        // the NEW deck has already re-requested the same index: the straggler's stale id
        // no longer matches the marker's owner, so the replacement's marker survives.
        if self.poster_inflight.get(&item) != Some(&request_id) {
            return;
        }
        self.poster_inflight.remove(&item);
        if w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
            return;
        }
        let img = pb_decode::DecodedImage {
            width: w,
            height: h,
            orig_width: w,
            orig_height: h,
            codec: "Video",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: rgba,
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        };
        let rep_kind = self.display_kind();
        self.pending_uploads
            .push(crate::decode_pool::Outcome::synthetic(
                item,
                self.epoch,
                self.content_gen,
                rep_kind,
                Ok(img),
            ));
    }

    /// Cache the inspector's video-fact rows for a macOS archive video, probed by the shell
    /// via AVFoundation (Rust can't build an `AVAsset` from bytes). Mirrors the row set the
    /// Windows/loose-file probe produces: Duration / Video codec / Frame rate / Audio.
    /// Re-signals the panel so an already-open inspector refreshes. `fps_milli` = fps×1000;
    /// `duration_ms` < 0 = unknown.
    ///
    /// **This is the thin path.** It carries no track catalog, so on a build where the
    /// FFmpeg backend can read the entry's bytes itself (`media_details::probe_job`, task
    /// 98.7) that probe produces strictly more — and this must not overwrite it. Hence the
    /// richer-wins guard below rather than a blind `insert`: the two race by construction
    /// (a detached Swift `Task` vs. a Rust worker) and whichever lands second would
    /// otherwise win on timing alone.
    pub fn archive_video_meta_ready(
        &mut self,
        item: usize,
        codec: String,
        fps_milli: u32,
        duration_ms: i64,
        has_audio: bool,
    ) {
        // A catalog-bearing entry is strictly richer than anything this path can build.
        if self
            .exif_cache
            .get(&item)
            .is_some_and(|d| d.media.is_some())
        {
            return;
        }
        let mut rows: Vec<(String, String)> = Vec::new();
        if duration_ms > 0 {
            let d = std::time::Duration::from_millis(duration_ms as u64);
            rows.push(("Duration".into(), crate::video::format_video_duration(d)));
        }
        if !codec.is_empty() {
            rows.push(("Video codec".into(), codec));
        }
        if fps_milli > 0 {
            rows.push((
                "Frame rate".into(),
                format!("{:.2} fps", f64::from(fps_milli) / 1000.0),
            ));
        }
        rows.push(("Audio".into(), if has_audio { "Yes" } else { "No" }.into()));
        let size = self.source.size_hint(item).unwrap_or(0);
        self.exif_cache.insert(
            item,
            crate::app_core::ItemDetails {
                size,
                fields: rows,
                // No catalog on this path. That is **not** a gap in practice: every macOS
                // build that can play an archived video also links FFmpeg — the shipped DMG
                // (`release-macos.sh` → `--bundle-ffmpeg`, which implies `--ffvideo`) and
                // dev builds (`build-swift-host.sh`, `--ffvideo` by default) — so
                // `media_details::probe_job` reads the entry's bytes and produces the real
                // catalog, and the guard above keeps it. This path is the fallback for the
                // one build without FFmpeg (`release-macos.sh --no-video`), where MKV/WebM
                // don't play at all. Not worth an FFI to carry a catalog across.
                media: None,
                has_audio: Some(has_audio),
                // The shell already probed this one; there is no worker to wait on.
                probe_state: crate::media_details::ProbeState::Ready,
                // AVFoundation probed it — that path doesn't parse the DoVi record.
                dovi_incompatible: false,
            },
        );
        self.emit_panels_changed();
    }

    pub fn video_placement(&self) -> Option<(f32, f32, f32, f32, u8)> {
        let (iw, ih, sw, sh) = self.screen_and_image()?;
        let content_h = sh.saturating_sub(self.content_top_inset).max(1);
        let mut p = self.view.placement(iw, ih, sw, content_h);
        p.y += self.content_top_inset as f32;
        let rotation = match self.view.rotation {
            Rotation::R0 => 0,
            Rotation::R90 => 1,
            Rotation::R180 => 2,
            Rotation::R270 => 3,
        };
        Some((p.x, p.y, p.w, p.h, rotation))
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

        // An explicit Settings change to theme / mute supersedes a CLI session override
        // (--theme / --mute) for the rest of this launch, so the dialog choice takes effect and
        // the override no longer masks it (and the saved value below is the user's real choice).
        if old.appearance_mode != self.settings.appearance_mode {
            self.launch.theme = None;
        }
        if old.mute_live_audio != self.settings.mute_live_audio {
            self.launch.mute = None;
        }

        // Subtitle appearance → the live engine (task #90.4). The rasterizer caches on
        // (text, params), so a changed style rebuilds the bitmap on the very next tick —
        // which is what makes the Settings preview and a playing film agree.
        self.subtitles.style = self.settings.subtitle_style.clone();
        // …and the forced-subtitles preference (task #99). Same lesson as post-mortem bug
        // #2: a preference that only reaches the engine at construction saves to disk and
        // does nothing until relaunch, which reads as the setting being broken. The next
        // tick re-resolves through `resolve_display`, so turning it off drops the signs
        // immediately (`tick_subtitles`'s single clearing exit) rather than at next launch.
        self.subtitles.selection.always_forced = self.settings.forced_subtitles;

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
    /// "hold to blaze" hint (no single binding).
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
                    row("Subtitles on/off", sc(Action::ToggleSubtitles)),
                    row("Next subtitle track", sc(Action::SubtitleCycle)),
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
                    row("Thumbnails", sc(Action::Thumbnails)),
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
                // path, not a per-frame one, so hold-to-blaze doesn't machine-gun the backend.)
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
            // An archive door has no pixels to report — its frame is a 1×1 transparent
            // sentinel (task #105), so `w`/`h` would read `1 × 1`. Its size is the fact
            // that matters, and it rides `PhotoMeta` from `ItemSource::size_hint`
            // (resolved on the scan worker) precisely because this runs on the frame
            // path and may never touch the disk.
            match meta.size {
                Some(bytes) => parts.push(crate::meta::human_bytes(bytes)),
                None => parts.push(format!("{}×{}", meta.w, meta.h)),
            }
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
        // `video_osd_until` flashes the line as the video seek/step position OSD
        // even when the user's `i` toggle is off (it replaces the position toast).
        (self.info_line || self.video_osd_until.is_some())
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

    /// Whether the displayed item is a video (task 79.9): the info line shows a film
    /// mark by the codec, the way a Live Photo shows the livephoto glyph. Distinct
    /// from `info_line_is_animated` (GIF/APNG) — a video is a `LibraryItemKind::Video`,
    /// not a `PhotoMeta.animated`.
    pub fn info_line_is_video(&self) -> bool {
        self.current.is_some() && self.displayed_item.is_some_and(|i| self.item_is_video(i))
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
        // While a video session is live on the displayed item, the line gains its
        // playback row (`0:42 ▰▰▰▱▱ 9:01`, task #79 — owner design): one block,
        // one `i` toggle, the bar filling whatever width the summary establishes.
        let Some((bitmap, w, h)) = (match self.video_progress_row() {
            Some(row) => hud.render_panel_progress(&text, &row, px, pad, info_bg),
            None => hud.render_panel(&text, px, pad, info_bg),
        }) else {
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
        // A live video session scrubs through the session, not `playback` (task
        // #79 follow-up). Forward repeats serve queued frames; backward repeats
        // chain paused seeks (latest-value coalescing absorbs any landing lag).
        if self
            .video
            .as_ref()
            .is_some_and(|v| Some(v.item()) == self.displayed_item)
        {
            let past_delay = timing::elapsed_since(self.framestep_started, now, self.initial_delay);
            let due = timing::elapsed_since(self.framestep_last, now, FRAME_STEP_REPEAT);
            if past_delay && due {
                self.video_frame_step(dir);
                self.framestep_last = Some(now);
            }
            return true;
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
        if !changed {
            self.draw();
            return;
        }
        // #106.7 — the instant Fit↔1:1: if the current photo is already resident in the NEW
        // mode's display representation (the parked full-res tier pre-decoded its Original), a
        // mode toggle is a pure REBIND — no epoch bump, no re-decode, no ring rebuild. Both
        // reps stay resident (the ring is untouched), so toggling straight back is instant too.
        // `display_slot` reads the just-updated mode. Falls through to the async re-decode only
        // when the other rep isn't held (radius 0, a just-blazed-to photo, an excluded item).
        if let Some(item) = self.displayed_item {
            if self.target_item == Some(item) {
                if let Some(slot) = self.display_slot(item) {
                    self.present_item(item, slot);
                    // Re-warm: the parked tier now wants the *previous* display rep held for
                    // this window (so toggling back stays instant), plus fulls in the new mode.
                    self.request_prefetch();
                    return;
                }
            }
        }
        self.invalidate_geometry();
        self.refresh_after_geometry_change();
    }

    /// Re-decode the display for a settled geometry change (resize / scale-mode) —
    /// **unless a live video owns it**. The session's output geometry is fixed by
    /// design (plan #79: the GPU rescales during a resize; a new fit applies on the
    /// next play), so there is nothing to re-decode *now* — and doing it anyway was
    /// the owner-reported fullscreen→windowed jerkiness: a synchronous poster
    /// decode of the playing clip over the live frame, plus a ring refill whose
    /// neighbor poster decodes (30 frames of 4K each, in a video folder) saturate
    /// every core mid-playback. Deferred instead: [`stop_video`] re-issues the
    /// prefetch once playback ends (navigation stops the video before loading, so
    /// nothing is ever missed).
    fn refresh_after_geometry_change(&mut self) {
        use crate::video::VideoSessionState::*;
        let video_live = self.video.as_ref().is_some_and(|v| {
            Some(v.item()) == self.displayed_item && !matches!(v.state(), Failed | Stopped)
        });
        if video_live {
            self.video_geometry_stale = true;
            return;
        }
        // Perf (PB_PERF): a Fit↔1:1 / settled-resize re-decode of the current photo begins
        // here — start the resize→on-screen clock; the next present of this item stops it.
        self.perf.resize_begin(self.now);
        // No synchronous decode on the event loop (task #18 finding #5). `invalidate_geometry`
        // just bumped the epoch, so `target_caught_up` is now false for the current item even
        // though its index is unchanged: the async prefetch re-decodes it at the new fit and
        // `drain_results` presents it when ready. Meanwhile the renderer holds the current
        // frame across the ring rebuild and the GPU refits it (via `set_view`) to the new
        // viewport / scale mode — so the switch is instant, with no freeze and no blank.
        self.target_item = self.playlist.current();
        if let Some(item) = self.target_item {
            let view = self.view_for(item);
            if let Some(r) = self.renderer.as_mut() {
                r.set_view(view);
            }
        }
        // item-6 Part D: a RETAINED current Original re-presents immediately at the new epoch
        // (the remap left the renderer's present cleared), so the frame stays live-and-sharp
        // with zero decode. Canonical `present_item` path (title/pin/mark_resolved). Without a
        // retained hit the old frame holds via the renderer's `held` fallback — today's
        // behaviour for that one change.
        // #123 fix 2: an exact-identity stash hit re-presents the pixels we had at this
        // geometry — zero decode, superseding both the Original rebind and the derive.
        // The prefetch below still runs (neighbour refill + the parked tier), but the
        // display-Fit want is suppressed by `fit_stash_covers`.
        if self.try_present_fit_stash() {
            self.request_prefetch();
            self.draw();
            return;
        }
        if let Some(item) = self.target_item {
            if self.displayed_item == Some(item) {
                if let Some(slot) = self.ring.original_slot(item) {
                    self.present_item(item, slot);
                }
            }
        }
        // #110 Phase 110b: before queueing the CPU re-decode, try to GPU-derive the current
        // photo's Fit from its retained Original (the compacted ring slot, or the `held`
        // fallback when it wasn't relocated). On success the Fit is resident + presented before
        // the prefetch below runs, so the ~1 s CPU Lanczos re-decode never gets queued — the
        // owner-felt win. On any miss this is a no-op and the incumbent path proceeds.
        self.try_gpu_derive_fit();
        self.request_prefetch();
        self.draw();
    }

    /// #123 fix 2: whether a quarter-turn session rotation is in effect for `item`
    /// (R90/R270 swap the effective decode axes — part of the stash identity).
    fn stash_quarter_turned(&self, item: usize) -> bool {
        matches!(
            self.rotations.get(&item),
            Some(Rotation::R90 | Rotation::R270)
        )
    }

    /// #123 fix 2: push the mirror's total unique-stash bytes into the ring's budget
    /// arithmetic. Called after every mirror mutation. (Brief double-count window: at
    /// capture time the texture is still ring-committed too, until the settle's rebuild
    /// drops the Fit slots — conservative, ~one settle long.)
    fn sync_stash_external(&mut self) {
        let sum = self.fit_stash.iter().flatten().map(|s| s.bytes).sum();
        self.ring.set_external_bytes(sum);
    }

    /// #123 fix 2: drop both stash sides — mirror AND renderer texture (renderer first;
    /// the mirror never outlives what it mirrors).
    fn clear_fit_stash(&mut self) {
        for i in 0..self.fit_stash.len() {
            if self.fit_stash[i].take().is_some() {
                if let Some(r) = self.renderer.as_mut() {
                    r.clear_stash(i);
                }
            }
        }
        self.sync_stash_external();
    }

    /// #123 fix 2: whether an exact-identity stash covers `item` at the CURRENT effective
    /// geometry — the want-suppression predicate (re-evaluated every pass, never cached):
    /// a covered current photo needs no display-Fit decode, the stash IS its definitive Fit.
    fn fit_stash_covers(&self, item: usize) -> bool {
        let Some(fit) = self.decode_fit() else {
            return false;
        };
        let q = self.stash_quarter_turned(item);
        self.fit_stash.iter().flatten().any(|s| {
            s.item == item
                && s.content_gen == self.content_gen
                && s.fit == fit
                && s.top_inset == self.content_top_inset
                && s.quarter_turned == q
        })
    }

    /// #123 fix 2, the CAPTURE side: called on the FIRST event of a resize burst, BEFORE
    /// `self.fit` mutates and before the `resize_hold` Original rebind (Codex r1 f1 — a
    /// later capture would stash the Original mislabeled as the old Fit). Aliases the
    /// on-screen definitive Fit into a renderer stash slot so toggling back to this exact
    /// geometry is a rebind. Fulls only (Codex Q3); renderer-verified (#109.4).
    fn capture_fit_stash(&mut self, incoming: FitBox) {
        if self.view.mode != ScaleMode::Fit {
            return; // Fill/Original display the Original rep — nothing fit-sized on screen
        }
        let Some(outgoing) = self.fit else {
            return;
        };
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.target_item != Some(item) || self.preview_resident.contains(&item) {
            return; // mid-nav, or a preview on screen — only definitive fulls stash
        }
        let Some(ring_slot) = self.ring.slot_for_rep(item, pb_core::RepKind::Fit) else {
            return; // displayed via the held/single-image path — nothing to alias
        };
        let q = self.stash_quarter_turned(item);
        let entry = FitStash {
            item,
            content_gen: self.content_gen,
            fit: outgoing,
            top_inset: self.content_top_inset,
            quarter_turned: q,
            bytes: self
                .ring
                .slot_bytes_of(item, pb_core::RepKind::Fit)
                .unwrap_or(0),
        };
        // Slot rotation (Codex Q1): overwrite a slot already holding this OUTGOING
        // geometry (dedup), else any slot NOT holding the INCOMING geometry (that one is
        // about to become presentable and must survive) — A→B→A→B keeps both sides live.
        let same = |s: &FitStash, fit: FitBox| {
            s.item == item && s.content_gen == self.content_gen && s.fit == fit
        };
        let idx = (0..self.fit_stash.len())
            .find(|&i| {
                self.fit_stash[i]
                    .as_ref()
                    .is_some_and(|s| same(s, outgoing))
            })
            .or_else(|| {
                (0..self.fit_stash.len()).find(|&i| {
                    !self.fit_stash[i]
                        .as_ref()
                        .is_some_and(|s| same(s, incoming))
                })
            })
            .unwrap_or(0);
        let ok = self
            .renderer
            .as_mut()
            .is_some_and(|r| r.stash_fit(idx, ring_slot));
        if ok {
            self.fit_stash[idx] = Some(entry);
            self.sync_stash_external();
            if sharp_diag() {
                eprintln!(
                    "[sharp-diag] fit-stash captured item={item} {}x{} (slot {idx})",
                    outgoing.max_width, outgoing.max_height
                );
            }
        }
        // A renderer refusal is already loud (`[pb-render] stash_fit refused`); the mirror
        // simply records nothing — never a stash the renderer lacks (#109.4).
    }

    /// #123 fix 2, the PRESENT side: on the geometry settle, an exact-identity stash hit
    /// re-presents the pixels we had at this geometry — zero decode, superseding both the
    /// Original rebind and the GPU derive. Transactional around the un-landed #109.5 hole:
    /// `mark_resolved` is gated on the renderer's own confirmation.
    fn try_present_fit_stash(&mut self) -> bool {
        let Some(item) = self.target_item else {
            return false;
        };
        if !self.fit_stash_covers(item) {
            return false;
        }
        let Some(fit) = self.decode_fit() else {
            return false;
        };
        let q = self.stash_quarter_turned(item);
        let Some(idx) = (0..self.fit_stash.len()).find(|&i| {
            self.fit_stash[i].as_ref().is_some_and(|s| {
                s.item == item
                    && s.content_gen == self.content_gen
                    && s.fit == fit
                    && s.top_inset == self.content_top_inset
                    && s.quarter_turned == q
            })
        }) else {
            return false;
        };
        let ok = self.renderer.as_mut().is_some_and(|r| r.present_stash(idx));
        if !ok {
            // A mirror entry the renderer can't honour — drop it loudly and fall through
            // to the decode ladder (#109.4: never trust, never silently proceed).
            eprintln!(
                "[ring-desync] fit-stash mirror without a renderer texture (slot {idx}) — dropped"
            );
            self.fit_stash[idx] = None;
            self.sync_stash_external();
            return false;
        }
        // Renderer-confirmed: the exact definitive Fit is on screen. Clear the hold +
        // sharpen bookkeeping, then resolve.
        self.resize_hold = None;
        self.preview_resident.remove(&item);
        self.upgrade_done.remove(&item);
        self.full_requested_at.remove(&item);
        self.mark_resolved(item);
        if sharp_diag() {
            eprintln!(
                "[sharp-diag] fit-stash re-present item={item} {}x{} — zero decode",
                fit.max_width, fit.max_height
            );
        }
        true
    }

    /// #110 Phase 110b + item-6 6b: satisfy the TARGET photo's Fit display by GPU-deriving the
    /// exact-size Fit from its retained Original instead of a CPU decode. Two callers: the
    /// settle after a geometry change (the current photo — retires the ~1 s re-decode), and
    /// `try_present_target` on a nav miss (a neighbour whose Original survived the last toggle
    /// — advance-after-toggle stays sharp, never a preview). Parked-only (`held_nav` none, §4),
    /// Fit display only, at most one dispatch per call site. Reserve-then-derive (§4/§7): the
    /// destination Fit slot is reserved BEFORE dispatch with a worst-case byte bound (fit box ×
    /// fp16), corrected to the real size after; on any ineligibility (headless / mode-1 /
    /// clamped — the renderer re-checks) the reservation is released so the CPU fallback can
    /// take the very slot. Returns whether the derived Fit is resident + presented.
    fn try_gpu_derive_fit(&mut self) -> bool {
        if !gpu_derive_enabled() || self.held_nav().is_some() {
            return false;
        }
        let Some(item) = self.target_item else {
            return false;
        };
        let Some(fit) = self.decode_fit() else {
            return false;
        };
        if self
            .ring
            .slot_for_rep(item, pb_core::RepKind::Fit)
            .is_some()
        {
            return false; // already resident (e.g. the instant-toggle rebind path)
        }
        let (fw, fh) = self.derive_fit_box(item, fit);
        // Source resolution BEFORE reserving (cheap miss, no rollback churn): the retained ring
        // Original, else the held fallback — which holds the last-PRESENTED photo's texture, so
        // it is a valid source ONLY when that photo is `item` (the settle case; `present_slot`
        // clears `held` on every later present, so held-exists ⟹ it is the displayed frame).
        // On nav it would be the WRONG PHOTO's pixels — refuse instead.
        let source = match self.ring.original_slot(item) {
            Some(s) => pb_render::DeriveSource::Ring(s),
            None if self.displayed_item == Some(item) => pb_render::DeriveSource::Held,
            None => return false,
        };
        let est = fw as u64 * fh as u64 * 8;
        let rep = self.rep_of(pb_core::RepKind::Fit);
        let cg = self.content_gen;
        let Some(res) = self.ring.reserve_bytes(item, cg, rep, est, &self.targets) else {
            return false;
        };
        let derived = self.renderer.as_mut().and_then(|r| {
            r.derive_fit(source, res.slot, fw, fh, derive_kernel(), derive_mip_bias())
        });
        let Some(d) = derived else {
            // Ineligible (headless / no held Original / clamped / mode 1): roll back so the
            // CPU Fit's own reservation isn't blocked by a stale Pending.
            self.ring.release_pending(item, res.slot, cg, rep);
            return false;
        };
        if !self.ring.mark_resident(item, res.slot, cg, rep) {
            // Unreachable while reserve→derive→mark is one synchronous stretch — but if it
            // ever fires, the derived texture is unbound: shout and fall back to the CPU
            // decode path rather than present a slot the core doesn't track (#109.4).
            eprintln!(
                "[ring-desync] mark_resident refused the derive slot {} item {item}",
                res.slot
            );
            return false;
        }
        self.retry_recover(item);
        self.ring
            .set_slot_bytes(item, pb_core::RepKind::Fit, d.bytes);
        // The derived Fit is a definitive full: clear stale preview/upgrade bookkeeping so the
        // sharpen loop doesn't CPU-decode what is already sharp, and release the
        // quality-monotonic resize hold — this IS the fresh full it was waiting for. The
        // full-request stamp clears too (a cancelled in-flight CPU full must not leave a stale
        // sharpen-latency anchor), and the perf tracker learns the item reached full residency
        // (open→all-cached would otherwise never complete) — Codex 110b review P2.
        self.preview_resident.remove(&item);
        self.upgrade_done.remove(&item);
        self.full_requested_at.remove(&item);
        self.perf_note_full(item);
        if self.resize_hold == Some(item) {
            self.resize_hold = None;
        }
        if sharp_diag() {
            eprintln!(
                "[sharp-diag] GPU-derived Fit item={item} {}x{} ({} B) — CPU re-decode skipped",
                d.w, d.h, d.bytes
            );
        }
        // #124: the user may be zoomed past 1:1 and deliberately bound to the full-res
        // `Original`. Presenting the derived Fit here would revert the picture to the softer
        // texture AND -- via `present_item` -> `view_for` -- reset the zoom to 1.0 mid-gesture.
        // Keep the derive (it is exactly what a zoom back out wants); just don't bind it.
        // House rule: background work may change residency or quality, never the presented
        // representation.
        if self.presented_kind == Some(pb_core::RepKind::Original)
            && self.displayed_item == Some(item)
        {
            return true;
        }
        // Present the derived Fit explicitly (canonical `present_item` path). Not
        // `try_present_target`: item-6's Part D re-present may have already resolved the target
        // with the ORIGINAL slot, and the caught-up shortcut would then leave the exact-size
        // Lanczos Fit resident but never bound.
        self.present_item(item, res.slot);
        true
    }

    /// The box the derived pixels must fill (Codex 110b review P1): the photo displays into
    /// the content region BELOW the translucent top bar, and a 90°/270° view rotation swaps
    /// which viewport axis bounds the UNROTATED texture — deriving against the raw viewport
    /// would produce a texture the rotated display then UPSCALES ~1.41× (blurrier than the
    /// CPU path it replaces). Zoom is not a factor: `view_for` resets it on every present.
    fn derive_fit_box(&self, item: usize, fit: FitBox) -> (u32, u32) {
        let mut fw = fit.max_width;
        let mut fh = fit.max_height.saturating_sub(self.content_top_inset).max(1);
        if matches!(
            self.rotations.get(&item),
            Some(Rotation::R90 | Rotation::R270)
        ) {
            std::mem::swap(&mut fw, &mut fh);
        }
        (fw, fh)
    }

    /// Sharpen-via-derive — the "+1 never waits" rule (owner, 2026-07-19): when the displayed
    /// photo is a resident PREVIEW and its full-res `Original` is also resident (the parked
    /// tier held it), the sharp Fit is a millisecond GPU derive — so replace the CPU sharpen
    /// (a full decode + an SMB re-read, 100s of ms) entirely. This is what makes backing up
    /// one right after a blaze land sharp within a frame or two instead of showing the preview
    /// while a decode grinds. In-place upgrade semantics (mirrors the CPU upgrade path): the
    /// derive writes over the item's EXISTING Fit slot, then the preview bookkeeping clears
    /// and the slot re-presents at its new dims. The transient byte overshoot between the
    /// derive and `make_room_for_upgrade` is at most one Fit texture for under a tick.
    /// Falls back silently (returns false) whenever ineligible — the CPU sharpen proceeds.
    fn try_gpu_sharpen(&mut self) -> bool {
        if !gpu_derive_enabled() {
            return false;
        }
        // #122 item 1: unlike the CPU sharpen (`sharpen_now`), the derive does NOT wait
        // for key-up — a tap's advance sharpens with the key still down (rebind-class
        // GPU cost). Only the actual auto-repeat (blaze) phase defers it: those frames
        // are replaced too fast to be worth deriving. A fired ADR-024 watchdog
        // overrides even that (the "held" key is a lie — the lost-key-up race).
        if self.blaze_repeating() && !self.preview_watchdog_fired() {
            return false;
        }
        let Some(item) = self.sharpen_candidate() else {
            return false;
        };
        let Some(fit) = self.decode_fit() else {
            return false;
        };
        let Some(dst) = self.ring.slot_for_rep(item, pb_core::RepKind::Fit) else {
            return false;
        };
        let Some(src) = self.ring.original_slot(item) else {
            return false; // no resident Original → the CPU sharpen path handles it
        };
        if src == dst {
            return false;
        }
        let (fw, fh) = self.derive_fit_box(item, fit);
        // VRAM admission BEFORE the risky allocation (Codex): at a full ring the derive's
        // output + scratch would otherwise stack on top of an already-at-budget ring. The
        // fp16 worst-case estimate over-evicts slightly; `set_slot_bytes` corrects after.
        // A derive that then fails wastes at most one eviction pass — and only until the CPU
        // sharpen lands (which ends the per-tick attempts).
        self.ring.make_room_for_upgrade(
            item,
            pb_core::RepKind::Fit,
            fw as u64 * fh as u64 * 8,
            &self.targets,
        );
        let derived = self.renderer.as_mut().and_then(|r| {
            r.derive_fit(
                pb_render::DeriveSource::Ring(src),
                dst,
                fw,
                fh,
                derive_kernel(),
                derive_mip_bias(),
            )
        });
        let Some(d) = derived else {
            return false; // ineligible (clamped / mode 1 / headless) — CPU sharpen proceeds
        };
        // In-place upgrade bookkeeping, mirroring the CPU upgrade path in `drain_results`.
        self.ring
            .set_slot_bytes(item, pb_core::RepKind::Fit, d.bytes);
        self.preview_resident.remove(&item);
        if self.resize_hold == Some(item) {
            self.resize_hold = None;
        }
        let t0 = self.full_requested_at.remove(&item);
        if self.displayed_item == Some(item) {
            if let Some(t0) = t0 {
                self.metrics.record("sharpen", t0.elapsed());
            }
        }
        self.perf_note_full(item);
        if sharp_diag() {
            eprintln!(
                "[sharp-diag] GPU-sharpened item={item} {}x{} from its resident Original — CPU sharpen skipped",
                d.w, d.h
            );
        }
        // Re-bind so the renderer picks up the sharp texture's dims — via `present_slot`
        // directly, exactly like the CPU upgrade path: `present_item` would run `view_for`
        // and RESET the user's zoom/pan (and re-stamp `last_present`, skewing slideshow
        // dwell) — an in-place quality upgrade of the already-presented photo must not
        // touch the view (Codex).
        // #124: same guard as `try_gpu_derive_fit` -- a zoom-selected `Original` outranks a
        // freshly-sharpened `Fit`. The sharpen still happened and is banked for zoom-out.
        if self.displayed_item == Some(item)
            && self.presented_kind != Some(pb_core::RepKind::Original)
        {
            if let Some(a) = self.renderer.as_mut() {
                a.present_slot(dst);
            }
            self.presented_kind = Some(pb_core::RepKind::Fit);
            self.draw();
        }
        true
    }

    /// Grow the playlist in place as a streaming scan delivers more images: swap in the
    /// larger snapshot and extend the cursor's universe **without** resetting the displayed
    /// photo, the cursor, the resident ring, or any per-image cache. The contrast with
    /// [`rebuild_playlist`](App::rebuild_playlist) is the whole point — a fresh open nukes
    /// everything; a *grow* keeps it, because indices are append-only (index `i` is still
    /// the same photo). New neighbours become decodable, so we re-issue prefetch (still the
    /// scanning, anti-thrash variant — the scan isn't done yet), and the title's "X / N"
    /// total ticks up. A no-op if the snapshot isn't actually larger.
    pub fn extend_playlist(&mut self, source: Arc<dyn ItemSource>) {
        let new_len = source.len();
        if new_len <= self.source.len() {
            return;
        }
        if door_diag() {
            eprintln!(
                "[door-diag] extend_playlist {}->{} first={:?} archive_scope={}",
                self.source.len(),
                new_len,
                (new_len > 0).then(|| source.name(0)),
                self.archive_scope.is_some(),
            );
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
    /// aren't tied up on fulls you blaze past. (Pre-libheif this was a single on-screen
    /// full because WIC's HEVC decoder serialized; libheif decodes in parallel, so we
    /// now fill a VRAM-bounded ring of fulls around the cursor.)
    /// Whether `item` sits in the thumb strip's CURRENT demand (the overscan
    /// window plus the current±warm tier — review f4: the fill plan demands
    /// both, so an edge computed from overscan alone was not a real
    /// leave-and-return). False whenever the strip is closed.
    fn thumb_demand_contains(&self, item: usize) -> bool {
        if !(self.thumbs.enabled && self.thumbs_visible()) {
            return false;
        }
        let Some(cur) = self.playlist.current() else {
            return false;
        };
        let d = self.thumbs.demand(cur);
        (item >= d.overscan.0 && item <= d.overscan.1) || item.abs_diff(d.current) <= d.warm
    }

    /// Record a real decode/selection failure against the retry budget, primed
    /// with the item's demand membership RIGHT NOW (review f2).
    fn retry_fail(&mut self, item: usize) {
        let display_now = self.targets.contains(&item);
        let thumb_now = self.thumb_demand_contains(item);
        self.retry.fail(item, display_now, thumb_now);
    }

    /// A decode/selection landed for the item: clear the budget AND both
    /// domains' failed gates (review f1: a thumb success clearing only the
    /// ledger left `failed` stranded forever — no fail count means no edge can
    /// ever lift it). Success anywhere proves the file decodes.
    fn retry_recover(&mut self, item: usize) {
        self.retry.recover(item);
        self.failed.remove(&item);
        self.thumbs.failed.remove(&item);
    }

    /// Pull every finished result out of the channel and fan the SELECTION
    /// payloads out immediately (phases-2/3 review f5): between a walk's send
    /// and the next drain, the ledger still says Selecting-with-no-choice, and
    /// an emission pass in that window would start a SECOND full walk (the pool
    /// entry is already untracked). Ordinary outcomes just wait in
    /// `pending_uploads` for the normal drain, exactly as before.
    fn absorb_results(&mut self) {
        while let Ok(o) = self.results.try_recv() {
            // #119: staleness is judged AT INGESTION, by the job's validity domain —
            // a stale outcome staged into `pending_uploads` would suppress the very
            // want that replaces it (`pending_reps` in `request_prefetch`).
            if outcome_stale(self.epoch, self.content_gen, &o) {
                continue;
            }
            self.pending_uploads.push(o);
        }
        let mut staged: Vec<crate::decode_pool::Outcome> = Vec::new();
        let mut i = 0;
        while i < self.pending_uploads.len() {
            if self.pending_uploads[i].key.purpose == crate::decode_pool::Purpose::PosterSelect {
                let o = self.pending_uploads.remove(i);
                self.route_poster_selection(o, &mut staged);
            } else {
                i += 1;
            }
        }
        self.pending_uploads.append(&mut staged);
    }

    pub fn request_prefetch(&mut self) {
        self.absorb_results();
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
        // Phase-4 bounded retry: a failed item RE-ENTERING a domain's demand
        // (after leaving it) gets one second chance — the gate lifts and the
        // normal machinery re-requests it. Transient SMB hiccups heal; corrupt
        // files fail twice and stay failed.
        {
            let retry = &mut self.retry;
            let targets = &self.targets;
            let display_edges: Vec<usize> = self
                .failed
                .iter()
                .copied()
                .filter(|&it| retry.edge(it, crate::retry::Domain::Display, targets.contains(&it)))
                .collect();
            for it in display_edges {
                self.failed.remove(&it);
                // The failure stamped the target as shown; a healed decode must
                // actually present (the round-3 recovery contract).
                if self.displayed_item == Some(it) || self.target_item == Some(it) {
                    self.presented_epoch = None;
                    self.presented_kind = None; // #124: see `invalidate_geometry`
                }
            }
            let thumb_candidates: Vec<usize> = self.thumbs.failed.iter().copied().collect();
            let thumb_edges: Vec<usize> = thumb_candidates
                .into_iter()
                .filter(|&it| {
                    let present = self.thumb_demand_contains(it);
                    self.retry.edge(it, crate::retry::Domain::Thumb, present)
                })
                .collect();
            for it in thumb_edges {
                self.thumbs.failed.remove(&it);
            }
        }
        let fit = self.decode_fit();
        let dk = self.display_kind();
        // Drop tier bookkeeping for items no longer resident (evicted) in the display rep.
        self.preview_resident
            .retain(|i| self.ring.slot_for_rep(*i, dk).is_some());
        self.upgrade_done
            .retain(|i| self.ring.slot_for_rep(*i, dk).is_some());
        self.full_requested_at
            .retain(|i, _| self.ring.slot_for_rep(*i, dk).is_some());
        // Items decoded but not yet uploaded must not be re-requested (the pool no
        // longer tracks them, so it would decode them again). Rep-aware (#106.7): an item's
        // Original being in-flight must not block a request for its Fit, and vice-versa.
        // DISPLAY outcomes only (#119 diff review, Codex bug 1): a staged Thumb shares the
        // `(item, Fit)` shape but routes to the thumb cache, never the ring — counting it
        // here would suppress the display decode it can't satisfy. (Reachable since #119:
        // thumbs survive geometry rebuilds, so one can be staged across a toggle.)
        let pending_reps: HashSet<(usize, pb_core::RepKind)> = self
            .pending_uploads
            .iter()
            .filter(|o| o.key.purpose == crate::decode_pool::Purpose::Display)
            .map(|o| (o.key.item, o.key.rep_kind))
            .collect();
        let pending_items: HashSet<usize> = pending_reps.iter().map(|&(i, _)| i).collect();
        let sharpen = self.sharpen_now();
        // `ring_order` IS the fulls decode priority (nearest-first when parked) — the job list
        // below must be built from THIS order, not from `self.targets` (Codex caught the sort
        // being collected into a set and never reaching the decoder).
        let ring_order: Vec<usize> = self.prefetch_fulls();
        // Stamp when each full was first requested, for the `sharpen` latency metric.
        if let Some(d) = sharpen {
            let first = !self.full_requested_at.contains_key(&d);
            self.full_requested_at.entry(d).or_insert_with(Instant::now);
            if first && sharp_diag() {
                eprintln!("[sharp-diag] sharpen requested item={d} (full decode wanted)");
            }
        } else if sharp_diag() {
            // The on-screen photo is NOT eligible to sharpen — the "never requested" case. Show why
            // (only when it's a resident preview, i.e. blurry, so this stays quiet on a full photo).
            if let Some(d) = self.displayed_item {
                if self.preview_resident.contains(&d) || self.upgrade_done.contains(&d) {
                    eprintln!(
                        "[sharp-diag] NO sharpen for displayed item={d}: display_slot={} preview_resident={} upgrade_done={} held_nav={}",
                        self.display_slot(d).is_some(),
                        self.preview_resident.contains(&d),
                        self.upgrade_done.contains(&d),
                        self.held_nav().is_some(),
                    );
                }
            }
        }
        for &t in &ring_order {
            self.full_requested_at.entry(t).or_insert_with(Instant::now);
        }

        // Build the job list in three priority tiers (the pool decodes by position):
        //   1. `sharpen` — the on-screen photo's full, so what you're looking at goes
        //      sharp ASAP the moment you park.
        //   2. previews — the whole window, so blazing / re-blazing is always instant.
        //   3. `ring` fulls — the sharp ring prefetched around the cursor, queued
        //      behind every preview, so a fast blaze stays smooth (these decode only in
        //      the pool's spare capacity) and the fulls land ahead of where you're
        //      heading — a stop finds the photo already sharp.
        type Job = crate::decode_pool::Want;
        let (mut head, mut previews, mut fulls): (Vec<Job>, Vec<Job>, Vec<Job>) =
            (Vec::new(), Vec::new(), Vec::new());
        // Video items whose poster-selection want is already in this pass's list
        // (task #114): the thumb tier must union its demand into the ledger, not
        // push a second want for the same identity. The pass brackets rebuild
        // each selection's demand union from the LIVE consumers (review f3).
        let mut sel_pushed: HashSet<usize> = HashSet::new();
        self.poster_sel.begin_pass();
        for &t in &self.targets {
            if self.failed.contains(&t) {
                continue;
            }
            // Pending display artifacts suppress duplicate REQUESTS below, but
            // must NOT starve a video's level-triggered selection re-emission
            // (owner smoke report: the staged placeholder made a pass skip the
            // selection want, which CANCELLED the in-flight walk/replay — then
            // the next pass restarted it from scratch, seconds of churn).
            let pending_display = pending_reps.contains(&(t, dk));
            let resident = self.ring.slot_for_rep(t, dk).is_some();
            let is_prev = resident && self.preview_resident.contains(&t);
            if resident && !is_prev {
                continue; // already the definitive full (see `decode_is_definitive_full`)
            }
            if !resident {
                // A video's display want IS its poster want. On selection-capable
                // platforms that becomes the ONE walk (task #114): purpose-neutral,
                // level-triggered (re-emitted every pass while selecting), shared
                // with the thumb tier below. `Chosen` with no resident pixels means
                // the artifact was evicted — the hint makes the recut a replay.
                if crate::engine::poster_select_supported()
                    && matches!(
                        crate::video::item_kind(self.source.as_ref(), t),
                        crate::video::LibraryItemKind::Video(_)
                    )
                {
                    // A remembered choice makes the re-need a cheap replay
                    // (phase 3) instead of a fresh walk; the hint rides the
                    // want and the ledger reopens either way.
                    // A Chosen selection whose artifact is already STAGED
                    // (review f3): nothing to do — reopening would enqueue a
                    // duplicate replay the untracked pool happily accepts.
                    if pending_display && self.poster_sel.choice(t).is_some() {
                        continue;
                    }
                    let hint = self
                        .poster_sel
                        .choice(t)
                        .map(|c| (c.origin_hns, c.relative_hns));
                    if !self
                        .poster_sel
                        .want(t, crate::poster_select::Demand::Display)
                    {
                        self.poster_sel.reopen(t);
                        let _ = self
                            .poster_sel
                            .want(t, crate::poster_select::Demand::Display);
                    }
                    sel_pushed.insert(t);
                    if !pending_display {
                        // The INSTANT tier (phase 1e + owner's cache insight):
                        // a cached thumb TILE is a far better stand-in than the
                        // dark placeholder — recognizable at once, upgraded in
                        // place by the selection. RAM-only reuse; zero decode,
                        // zero I/O — it goes straight into the upload queue.
                        let tile = self.thumbs.cache.get(t).map(|e| {
                            let p = &e.payload;
                            pb_decode::DecodedImage {
                                width: e.w,
                                height: e.h,
                                orig_width: p.orig_w,
                                orig_height: p.orig_h,
                                codec: p.codec,
                                format: pb_decode::PixelFormat::Rgba8,
                                pixels: p.rgba.clone(),
                                is_preview: true,
                                color: pb_decode::ColorTransform::srgb(),
                                peak: 1.0,
                                animated: None,
                            }
                        });
                        match tile {
                            Some(img) if fit.is_some() => self.pending_uploads.push(
                                crate::decode_pool::Outcome::synthetic(
                                    t,
                                    self.epoch,
                                    self.content_gen,
                                    pb_core::RepKind::Fit,
                                    Ok(img),
                                )
                                .from_preview_request(),
                            ),
                            _ if fit.is_some() => {
                                // No tile yet: the synthesized flat placeholder.
                                previews.push(Job::display(t, fit, true));
                            }
                            _ => {}
                        }
                    }
                    previews.push(
                        Job::poster_select(t, fit, true)
                            .with_replay(hint)
                            .with_native_class(
                                crate::engine::poster_walk_native() || fit.is_none(),
                            ),
                    );
                    continue;
                }
                if pending_display {
                    continue;
                }
                // #123 fix 2: a stash-covered photo needs no display decode — the stashed
                // texture IS its definitive Fit at this exact geometry (checked fresh
                // every pass; any identity drift re-opens the want).
                if self.fit_stash_covers(t) {
                    continue;
                }
                // Preview-first (`allow_preview`) is a FIT-scale concept: an embedded ~256px
                // thumbnail is a fine instant stand-in for a fit-to-window view, but it is NEVER a
                // valid `Original` (1:1 / Fill decodes at native res). Gating on `fit.is_some()`
                // (true only in Fit mode) keeps a preview out of the Original ring slot — otherwise
                // switching scale modes lands a thumbnail in the native tier and the rest of the
                // engine trusts it as the full image. Off-thread, so no beachball risk (unlike the
                // sync first-paint in `load_current_sync`, which stays preview-first on purpose).
                previews.push(Job::display(t, fit, fit.is_some()));
            } else if Some(t) == sharpen {
                // A resident-placeholder VIDEO sharpens via its selection (the
                // walk IS the full), never a bare display full (which would run
                // the legacy walk beside it — the double-walk through a back
                // door). Photos keep the normal sharpen full.
                if crate::engine::poster_select_supported()
                    && matches!(
                        crate::video::item_kind(self.source.as_ref(), t),
                        crate::video::LibraryItemKind::Video(_)
                    )
                {
                    if pending_display && self.poster_sel.choice(t).is_some() {
                        continue; // staged artifact incoming (review f3)
                    }
                    let hint = self
                        .poster_sel
                        .choice(t)
                        .map(|c| (c.origin_hns, c.relative_hns));
                    if !self
                        .poster_sel
                        .want(t, crate::poster_select::Demand::Display)
                    {
                        self.poster_sel.reopen(t);
                        let _ = self
                            .poster_sel
                            .want(t, crate::poster_select::Demand::Display);
                    }
                    if sel_pushed.insert(t) {
                        head.push(
                            Job::poster_select(t, fit, true)
                                .with_replay(hint)
                                .with_native_class(
                                    crate::engine::poster_walk_native() || fit.is_none(),
                                ),
                        );
                    }
                } else if !pending_display {
                    head.push(Job::display(t, fit, false));
                }
            }
            // else: resident-preview fulls are queued below IN `ring_order` (their decode
            // priority), not in `targets` order — "+1 never waits".
        }
        // The fulls tier, in `prefetch_fulls`' order (nearest-first when parked; window order
        // while blazing / in Random mode). `ring_order` is already filtered to resident
        // previews minus the sharpen; only the per-tick job guards repeat here.
        for &t in &ring_order {
            if self.failed.contains(&t) {
                continue;
            }
            let pending_display = pending_reps.contains(&(t, dk));
            // Resident-placeholder videos upgrade via their selection here too.
            if crate::engine::poster_select_supported()
                && matches!(
                    crate::video::item_kind(self.source.as_ref(), t),
                    crate::video::LibraryItemKind::Video(_)
                )
            {
                if pending_display && self.poster_sel.choice(t).is_some() {
                    continue; // staged artifact incoming (review f3)
                }
                let hint = self
                    .poster_sel
                    .choice(t)
                    .map(|c| (c.origin_hns, c.relative_hns));
                if !self
                    .poster_sel
                    .want(t, crate::poster_select::Demand::Display)
                {
                    self.poster_sel.reopen(t);
                    let _ = self
                        .poster_sel
                        .want(t, crate::poster_select::Demand::Display);
                }
                if sel_pushed.insert(t) {
                    fulls.push(
                        Job::poster_select(t, fit, true)
                            .with_replay(hint)
                            .with_native_class(
                                crate::engine::poster_walk_native() || fit.is_none(),
                            ),
                    );
                }
                continue;
            }
            if !pending_display && !self.fit_stash_covers(t) {
                fulls.push(Job::display(t, fit, false));
            }
        }
        let mut jobs = head;
        let head_len = jobs.len();
        jobs.append(&mut previews);
        jobs.append(&mut fulls);
        // Parked full-res tier (#106.7): when PARKED (no nav key held), keep a small
        // sequential window's *other* representation resident — the full-res Original while
        // displaying Fit, or the Fit while displaying Original — so a Fit↔1:1 toggle / zoom on
        // those photos is an instant rebind, not a re-decode. Radius from `full_res_radius`;
        // order is current → compare-pin → sequential neighbours (§ pin). Excludes video /
        // archive doors / SVG / RAW and anything past the gigapixel ceiling (§8/§9). While a key
        // is held this whole tier is empty (blazing stays lean).
        //
        // Priority split (#123, owner 2026-07-19): the CURRENT photo's own job ranks directly
        // after the current display ladder — parked means F / 1:1 / zoom is the next likely
        // action, and its prerequisite must not queue behind dozens of neighbour refills (each
        // F re-queues them all, which kept the Original perpetually at the back — the "way
        // after the pie" wait). The REST of the tier stays appended **below the thumb fills**
        // (§5): that below-thumbs calibration was about the whole tier's big originals starving
        // visible thumbnails (owner-reported); one bounded job does not reopen it.
        let mut parked: Vec<Job> = Vec::new();
        let mut parked_current: Vec<Job> = Vec::new();
        if self.held_nav().is_none() {
            let (other_kind, other_fit) = match dk {
                pb_core::RepKind::Fit => (pb_core::RepKind::Original, None),
                pb_core::RepKind::Original => (pb_core::RepKind::Fit, self.fit),
            };
            // Only run when the other rep is a genuinely different decode: in Fit mode the
            // Original always is; in Original/Fill mode the Fit only differs when there is a
            // fit box (a bare full-res viewport would collapse both to the same decode).
            if matches!(dk, pb_core::RepKind::Fit) || self.fit.is_some() {
                let radius = self.full_res_radius();
                for it in self.full_res_window(radius) {
                    if self.failed.contains(&it) || pending_reps.contains(&(it, other_kind)) {
                        continue;
                    }
                    // Task #114 phase 3: a PARKED video with a chosen poster but
                    // no resident Original pre-installs it via a cheap replay in
                    // spare capacity — so the first fullscreen toggle GPU-derives
                    // instantly, exactly like a photo. The A/B kept the browse
                    // walks fitted (fastest time-to-poster); this is where the
                    // native frame gets fetched for the photos you actually sit
                    // on. Fit-mode only (Original mode already displays native).
                    if matches!(dk, pb_core::RepKind::Fit)
                        && crate::engine::poster_select_supported()
                        && matches!(
                            crate::video::item_kind(self.source.as_ref(), it),
                            crate::video::LibraryItemKind::Video(_)
                        )
                    {
                        if self
                            .ring
                            .slot_for_rep(it, pb_core::RepKind::Original)
                            .is_none()
                            && !self.poster_sel.original_blocked(it)
                        {
                            let hint = self
                                .poster_sel
                                .choice(it)
                                .map(|c| (c.origin_hns, c.relative_hns));
                            if hint.is_some() {
                                self.poster_sel.reopen(it);
                            }
                            let selecting = self
                                .poster_sel
                                .want(it, crate::poster_select::Demand::Display);
                            if selecting && sel_pushed.insert(it) {
                                parked.push(
                                    Job::poster_select(it, fit, true)
                                        .with_replay(hint)
                                        .with_native_class(true),
                                );
                            }
                        }
                        continue;
                    }
                    if self.ring.is_tracked_rep(it, other_kind)
                        || !self.full_res_eligible(it)
                        // #122: the ring refused this very reservation since the last
                        // rebuild/nav — re-requesting it burns one full native decode
                        // per prefetch pass (the owner's 30×-one-item livelock log).
                        || self.ring.denied(it, other_kind)
                    {
                        continue;
                    }
                    // #122 pre-flight (refuse-before-request): when the item's dims are
                    // known, dry-run the reservation — a want the ring must refuse at
                    // landing wastes the whole decode. Unknown meta passes (the first
                    // landing teaches meta); an estimate that under-shoots the real
                    // charge (rare: HDR sources at 8 B/px) costs one refused landing,
                    // which the `denied` latch above then makes terminal.
                    if other_kind == pb_core::RepKind::Original {
                        if let Some(m) = self.meta_cache.get(&it) {
                            let est = crate::engine::mip_chain_bytes(m.w, m.h, 4);
                            if !self.ring.admittable(
                                it,
                                pb_core::RepKind::Original,
                                est,
                                &self.targets,
                            ) {
                                continue;
                            }
                        }
                    }
                    // #123: the current photo's job jumps to the head split; neighbours
                    // (and the pin) stay in the below-thumbs tail. (A parked VIDEO's
                    // poster pre-install above keeps its own #114 pacing — photos only.)
                    if Some(it) == self.playlist.current() {
                        parked_current.push(Job::display(it, other_fit, false));
                    } else {
                        parked.push(Job::display(it, other_fit, false));
                    }
                }
            }
        }
        // #123: the current photo's parked job (at most one) lands directly after its
        // own display ladder — ahead of every neighbour preview/full and the thumbs.
        for (i, j) in parked_current.into_iter().enumerate() {
            jobs.insert(head_len + i, j);
        }
        // Thumbnails fills (task #83): appended BELOW every display want (the
        // merged-scheduler order), only while the strip is visible and the user
        // is parked — an expensive cold fill must never race a blaze. T0 capture
        // covers the strip during flight anyway.
        if self.thumbs.enabled && self.thumbs_visible() && self.held_nav().is_none() {
            // Items already carrying a DISPLAY want in this pass — the head ladder,
            // previews and fulls. A video's display poster becomes its tile via
            // `thumbs_capture`, so a thumb walk for one of these would be a second
            // concurrent walk of the same film.
            //
            // Deliberately NOT the parked tier: it is appended after this block, and it
            // never contains a video anyway (`full_res_eligible` excludes them). If that
            // ever changes, this set must move below the append.
            let display_wanted: std::collections::HashSet<usize> = jobs
                .iter()
                .filter(|w| w.purpose == crate::decode_pool::Purpose::Display)
                .map(|w| w.item)
                .collect();
            if let Some(cur) = self.playlist.current() {
                let demand = self.thumbs.demand(cur);
                for it in self.thumbs.cache.fill_plan(
                    &demand,
                    self.playlist.len(),
                    crate::thumbs::THUMB_MAX_FILL_JOBS,
                ) {
                    if self.failed.contains(&it)
                        || self.thumbs.failed.contains(&it)
                        || pending_items.contains(&it)
                    {
                        continue;
                    }
                    // A VIDEO whose display poster walk is already in THIS pass needs no
                    // thumb walk: `thumbs_capture` turns that poster into the tile when it
                    // lands. Without this the same film is walked twice concurrently — the
                    // double walk #114's selection removes on Windows, which has no
                    // equivalent off it.
                    //
                    // `pending_items` cannot cover this: it is built from
                    // `pending_uploads`, i.e. decodes that have already RETURNED, so a walk
                    // still in flight — precisely the multi-second network case — is
                    // invisible to it.
                    //
                    // Keyed on "a display want exists for this item in this pass", NOT on
                    // "it is a video": the strip's warm range is far wider than the display
                    // window, and films outside it must still fill themselves. The emission
                    // is level-triggered, so a cancelled display walk simply re-plans the
                    // thumb next pass.
                    //
                    // Scoped to the platforms with NO selection pipeline. On Windows the
                    // selection branch below owns this — and it must keep running, because
                    // it also unions `Demand::Thumb` into the ledger, which is what routes a
                    // FAILED walk into `thumbs.failed`. Suppressing ahead of it would leave
                    // that attribution unrecorded on a Windows placeholder pass.
                    if !crate::engine::poster_select_supported()
                        && display_wanted.contains(&it)
                        && matches!(
                            crate::video::item_kind(self.source.as_ref(), it),
                            crate::video::LibraryItemKind::Video(_)
                        )
                    {
                        continue;
                    }
                    // A video's thumb IS its poster (task #114): union the thumb
                    // demand into the selection instead of a second walk. If the
                    // display tier already pushed this pass, the ledger union is
                    // all that's needed; a thumb-only selection parks under the
                    // thumb cap (display_class stays false).
                    if crate::engine::poster_select_supported()
                        && matches!(
                            crate::video::item_kind(self.source.as_ref(), it),
                            crate::video::LibraryItemKind::Video(_)
                        )
                    {
                        // A tile still in the derive queue is NOT evicted
                        // (review f8): skip until it lands or is dropped.
                        if self.thumbs.pending_now(it) {
                            continue;
                        }
                        // `fill_plan` only yields items with no tile, so a
                        // `Chosen` here means the tile is genuinely gone. The
                        // hint makes the refill a REPLAY of the same frame —
                        // never a fresh walk that could pick differently (the
                        // missing hint here was the "thumbnail arrives way
                        // after the video" report: a dropped tile re-walked the
                        // whole film at bottom priority).
                        let hint = self
                            .poster_sel
                            .choice(it)
                            .map(|c| (c.origin_hns, c.relative_hns));
                        if !self
                            .poster_sel
                            .want(it, crate::poster_select::Demand::Thumb)
                        {
                            self.poster_sel.reopen(it);
                            let _ = self
                                .poster_sel
                                .want(it, crate::poster_select::Demand::Thumb);
                        }
                        if sel_pushed.insert(it) {
                            jobs.push(
                                Job::poster_select(
                                    it,
                                    Some(crate::thumbs::thumb_fit()),
                                    self.poster_sel.display_class(it),
                                )
                                .with_replay(hint),
                            );
                        }
                        continue;
                    }
                    jobs.push(Job::thumb(it, crate::thumbs::thumb_fit()));
                }
            }
        }
        // Parked full-res originals go LAST — below the display ladder and the thumb fills — so
        // they only ever decode in the pool's genuinely spare capacity (#106.7 §5).
        jobs.append(&mut parked);
        // Selections no live consumer re-asked for this pass drop to Absent —
        // their pool jobs die by level-triggered non-re-emission (review f3).
        self.poster_sel.end_pass();
        self.pool
            .set_targets(self.epoch, self.content_gen, &self.source, &jobs);
    }

    // ── Directory-scan worker lifecycle (task #126 step 1) ────────────────────────────────
    //
    // Moved off the two shells, which each carried a byte-similar copy. The shells keep only
    // dialog realisation; everything below is shell-neutral and unit-tested.

    /// Start a folder walk on a worker thread — **the production entry point** both shells
    /// call in place of their own `begin_dir_scan` copies.
    ///
    /// Walking a large or deeply nested tree (the worst case: someone opens `~/Library`) takes
    /// many seconds, and doing it synchronously froze the run loop and could get the app killed
    /// as unresponsive. So the walk streams over a channel and the current view stays up until
    /// its first batch lands.
    ///
    /// Returns the operation this **superseded**, if any. Today the caller must still stop a
    /// displaced *archive open* itself, because that worker is the shell's until step 2 moves
    /// it; a displaced walk is already stopped here.
    ///
    /// Non-`Source::Scan` input is rejected **before** any state is touched. The shell copies
    /// bumped the generation and cleared tombstones first and returned late, which was harmless
    /// with one caller but is a latent bug in a generally callable core transition (plan §5a).
    pub fn begin_dir_scan(
        &mut self,
        source: pb_core::open::Source,
        cursor: pb_core::open::Cursor,
    ) -> Option<(crate::background::OpId, crate::background::OpKind)> {
        let pb_core::open::Source::Scan { roots, recursive } = source else {
            return None; // explicit lists and archives are routed elsewhere by `open_plan`
        };
        let name = crate::dir_scan::scan_display_name(&roots);
        let root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
        let scan_root = roots.first().cloned();
        // The live Show Archives preference (task #104), read at spawn time: with it off the
        // walk drops archive "doors" so the deck never lists them.
        let show_archives = self.settings.show_archives;
        let progress = crate::scan::ScanProgress::new();
        let worker_progress = progress.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        // The wire generation is the worker's own tag on each update. It stays distinct from
        // the `OpId` because `stream_scan` stamps it, and only `arm_dir_scan` knows the id.
        let wire_gen = self.scan_wire_gen.wrapping_add(1);
        self.scan_wire_gen = wire_gen;
        std::thread::spawn(move || {
            crate::scan::stream_scan(
                roots,
                recursive,
                show_archives,
                cursor,
                root,
                scan_root,
                wire_gen,
                worker_progress,
                tx,
            );
        });
        self.arm_dir_scan(wire_gen, rx, progress, name)
    }

    /// A live view of the walk in flight, for whatever scan chrome a shell draws. `None` when
    /// no walk is running. Cheap and non-mutating, so it is safe to call every frame.
    pub fn scan_status(&self) -> Option<crate::dir_scan::ScanStatus> {
        let scan = self.dir_scan.as_ref()?;
        let current = scan.progress.current();
        Some(crate::dir_scan::ScanStatus {
            found: scan.progress.found(),
            current_dir: if current == scan.name {
                String::new()
            } else {
                current
            },
            slow: self
                .bg
                .is_slow(self.now, crate::dir_scan::SCAN_DIALOG_DELAY),
            bootstrapped: self.scan_bootstrapped,
            name: scan.name.clone(),
        })
    }

    /// Install an already-spawned walk. Returns the operation it **superseded**, if any, so
    /// the caller can stop that worker.
    ///
    /// The supersession *policy* lives in [`BackgroundOps`](crate::background::BackgroundOps)
    /// — one generation space across both operation kinds — while the *mechanism* for an
    /// archive open stays with the shell until step 2 moves that worker too. That split is
    /// deliberate: the invariant has a single owner even though the two workers do not yet.
    ///
    /// Separated from [`begin_dir_scan`](Self::begin_dir_scan) so tests can arm a scan with
    /// their own channel and drive it deterministically, with no thread and no sleeps.
    pub fn arm_dir_scan(
        &mut self,
        wire_gen: u64,
        rx: std::sync::mpsc::Receiver<(u64, crate::scan::ScanUpdate)>,
        progress: crate::scan::ScanProgress,
        name: String,
    ) -> Option<(crate::background::OpId, crate::background::OpKind)> {
        // A fresh scan is a fresh universe: no stale tombstones from the previous deck.
        self.deleted.clear();
        let (id, superseded) = self.bg.begin(crate::background::OpKind::DirScan, self.now);
        // Stop whatever was displaced *here*, inside the transition, rather than trusting each
        // call site to remember. The winit shell's `cancel_dir_scan` relied on callers clearing
        // the handle afterwards and its own comment overstated that they all do (two of five do
        // not); the macOS copy cleared it internally. This adopts the macOS shape, which is
        // correct by construction (task #126 §11.2).
        //
        // Since step 2 this also stops a displaced ARCHIVE OPEN, because the core owns that
        // worker too. Handling only the walk here was the exact §12.6 asymmetry — it type-checks
        // and reads fine, and silently drops the cross-type cancel in one direction.
        self.supersede(superseded);
        self.scanning = true; // sequential-only prefetch while streaming
        self.scan_bootstrapped = false; // the first non-empty batch bootstraps the view
        self.dir_scan = Some(crate::dir_scan::DirScanState::armed(
            id, wire_gen, rx, progress, name,
        ));
        superseded
    }

    /// **User-initiated** stop of a folder scan (the pill's Cancel, File ▸ Stop Scanning, or a
    /// bound key), keeping whatever streamed in so far — the partial playlist is already live.
    ///
    /// Distinct from the bare [`cancel_dir_scan`](Self::cancel_dir_scan), which is the
    /// *mechanism* and is also used for teardown and for cross-type supersession where a new
    /// deck is about to arrive. Only the user-initiated path restores the welcome hint,
    /// because only it leaves the user looking at nothing on purpose.
    ///
    /// ⚠ The hint restore is the fix for a real gap (task #126 ledger item 3, found 2026-07-20):
    /// [`finish_scan`](Self::finish_scan) restores the "Press O to open" hint when a walk ends
    /// naturally with an empty deck, but **no cancel path did**. `show_open_hint` early-returns
    /// while `scanning` is true, and cancelling never called it afterwards — so a cold launch
    /// into a slow folder, cancelled before the first photo, left an empty canvas with the hint
    /// still suppressed. Both shells had the same hole; fixing it here fixes both.
    ///
    /// Returns whether a scan was actually running, so the shell can skip its toast.
    pub fn cancel_scan_command(&mut self) -> bool {
        if self.dir_scan.is_none() {
            return false;
        }
        let nothing_shown = !self.scan_bootstrapped && self.source.is_empty();
        self.cancel_dir_scan();
        // `cancel_dir_scan` cleared `scanning`, so `show_open_hint` will no longer suppress
        // itself. Symmetric with `finish_scan`'s restore, and gated the same way: never blank
        // an existing photo.
        if nothing_shown {
            self.show_open_hint();
        }
        self.request_prefetch();
        true
    }

    /// Cancel any in-flight walk. Idempotent, and — unlike the winit shell's version — it
    /// clears the handle itself, so no call site has to remember (task #126 §11.2).
    pub fn cancel_dir_scan(&mut self) {
        if let Some(scan) = self.dir_scan.take() {
            scan.request_cancel();
        }
        self.bg.cancel();
        self.scanning = false;
    }

    /// Pump the walk's channel, applying every snapshot queued this tick. Returns what the
    /// shell should do with its Scanning dialog.
    ///
    /// Mirrors the shipped shell logic: the first non-empty batch bootstraps the view and the
    /// rest extend it; a `Done` for the current generation ends the walk (toasting when it
    /// found nothing); a slow walk with nothing on screen asks for the progress dialog; a
    /// dead worker never strands its dialog.
    pub fn poll_dir_scan(&mut self) -> crate::dir_scan::ScanPoll {
        use crate::dir_scan::{ScanDialogRequest, ScanPoll};
        use crate::scan::ScanUpdate;
        use std::sync::mpsc::TryRecvError;
        loop {
            let (wire_gen, id, recv) = match self.dir_scan.as_ref() {
                Some(s) => (s.wire_gen, s.id, s.rx.try_recv()),
                None => return ScanPoll::idle(),
            };
            // One staleness gate for both flows: a walk superseded by a newer scan *or* by an
            // archive open fails this, so its late batches can never touch the deck.
            if !self.bg.is_current(id) {
                self.dir_scan = None;
                self.scanning = false;
                return ScanPoll::dialog(ScanDialogRequest::Close);
            }
            match recv {
                Ok((g, ScanUpdate::Batch(resolved))) => {
                    if g != wire_gen {
                        continue; // superseded (defensive; the channel is per-scan)
                    }
                    self.handle(contract::CoreEvent::ScanBatch(resolved));
                    // A photo is on screen, so a revealed dialog has served its purpose —
                    // browsing should start at the first image, not the end of the walk.
                    if self.scan_bootstrapped {
                        return ScanPoll::dialog(ScanDialogRequest::Close);
                    }
                }
                Ok((g, ScanUpdate::Done)) => {
                    if g != wire_gen {
                        continue;
                    }
                    let scanned = self
                        .dir_scan
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    self.dir_scan = None;
                    self.bg.finish(id);
                    let never_bootstrapped = !self.scan_bootstrapped;
                    self.handle(contract::CoreEvent::ScanDone);
                    return ScanPoll {
                        dialog: ScanDialogRequest::Close,
                        found_no_photos: never_bootstrapped.then_some(scanned),
                    };
                }
                Err(TryRecvError::Empty) => {
                    // Reveal only once the walk is slow enough to notice, and never over a
                    // photo that is already up. `should_reveal` latches, so this fires once.
                    if self.scan_bootstrapped {
                        return ScanPoll::idle();
                    }
                    let due = self
                        .bg
                        .should_reveal(self.now, crate::dir_scan::SCAN_DIALOG_DELAY)
                        .is_some();
                    if !due {
                        return ScanPoll::idle();
                    }
                    return match self.dir_scan.as_ref() {
                        Some(s) => ScanPoll::dialog(ScanDialogRequest::Reveal {
                            name: s.name.clone(),
                            progress: s.progress.clone(),
                        }),
                        None => ScanPoll::idle(),
                    };
                }
                Err(TryRecvError::Disconnected) => {
                    // The worker died (panic, or dropped its sender without a terminal Done).
                    // Never strand its dialog.
                    self.dir_scan = None;
                    self.bg.finish(id);
                    self.scanning = false;
                    return ScanPoll::dialog(ScanDialogRequest::Close);
                }
            }
        }
    }

    // ── Archive-open worker lifecycle (task #126 step 2) ──────────────────────────────────
    //
    // The companion to the dir-scan block above. Moved off the two shells, which each carried
    // a byte-similar copy. Read `crate::archive_open`'s privacy note before touching the
    // password path.

    /// Start opening an archive — **the production entry point** both shells call.
    ///
    /// A plain `.zip` with no cached passwords to auto-try opens *synchronously* (that reads a
    /// central directory, not entry data) and returns its terminal outcome without ever
    /// spawning a worker or showing chrome. Everything else — 7z, the tar family, or any open
    /// that will auto-try cached passwords — goes to a worker thread, returns
    /// [`ArchiveOutcome::Pending`], and lands through [`poll_archive_load`](Self::poll_archive_load).
    ///
    /// The auto-try only runs on an *initial* open (`password.is_none()`), so a user-entered
    /// password is never silently replaced by a cached one.
    pub fn begin_archive_open(
        &mut self,
        path: std::path::PathBuf,
        password: Option<crate::SecretString>,
    ) -> crate::archive_open::ArchiveOutcome {
        use crate::archive_open::ArchiveOutcome;

        let kind = pb_source::archive_kind(&path).unwrap_or(pb_source::ArchiveKind::Zip);
        // Auto-try cached session passwords (MRU-first) only on an INITIAL open, so a
        // same-password folder asks once (session-archive-password-cache).
        let cached = if password.is_none() {
            self.archive_passwords_snapshot()
        } else {
            Vec::new()
        };
        let attempted_password = password.clone();

        // Claim the shared generation space. Both flows are registered here now, which is what
        // lets the core cancel a displaced walk ITSELF rather than each shell remembering to
        // (task #126 §12.6 — the interim unconditional cancel in the shells retires with this).
        let (id, superseded) = self
            .bg
            .begin(crate::background::OpKind::ArchiveOpen, self.now);
        self.supersede(superseded);

        // A wrong-password ZIP attempt decrypts the whole first entry, so it must go
        // off-thread; an empty cache with no user password is the synchronous fast path.
        let will_autotry = password.is_none() && !cached.is_empty();
        if !kind.background_open() && !will_autotry {
            let pw = password.as_ref().map(|p| p.expose().to_owned());
            let result =
                crate::scan::load_archive(&path, kind, pw, &pb_source::OpenProgress::new());
            self.bg.finish(id);
            return self.finish_archive_open((result, None), attempted_password, path);
        }

        let wire_gen = self.archive_wire_gen.wrapping_add(1);
        self.archive_wire_gen = wire_gen;
        let progress = pb_source::OpenProgress::new();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker_path = path.clone();
        let worker_progress = progress.clone();
        std::thread::spawn(move || {
            let out = match password {
                Some(pw) => (
                    crate::scan::load_archive(
                        &worker_path,
                        kind,
                        Some(pw.expose().to_owned()),
                        &worker_progress,
                    ),
                    None,
                ),
                None => crate::scan::load_archive_with_cache(
                    &worker_path,
                    kind,
                    &cached,
                    &worker_progress,
                ),
            };
            let _ = tx.send((wire_gen, out));
        });
        self.archive_load = Some(crate::archive_open::ArchiveOpenState {
            id,
            rx,
            wire_gen,
            path,
            attempted_password,
            progress,
        });
        ArchiveOutcome::Pending
    }

    /// Stop whatever operation `superseded` names. The core owns **both** workers now, so this
    /// is the one place cross-type supersession is performed as well as decided — the split
    /// that made it a per-call-site convention (and a recurring bug) is gone.
    fn supersede(
        &mut self,
        superseded: Option<(crate::background::OpId, crate::background::OpKind)>,
    ) {
        match superseded {
            Some((_, crate::background::OpKind::DirScan)) => {
                if let Some(prev) = self.dir_scan.take() {
                    prev.request_cancel();
                }
                self.scanning = false;
            }
            Some((_, crate::background::OpKind::ArchiveOpen)) => {
                if let Some(prev) = self.archive_load.take() {
                    prev.request_cancel();
                }
            }
            None => {}
        }
    }

    /// Install an archive open that is already running (or, in tests, one that never will),
    /// so a test can drive the worker's channel itself — no thread, no filesystem, no sleeps.
    /// The deterministic completion point Codex asked an injectable runtime for; the mpsc
    /// channel already was one (§11.3).
    #[doc(hidden)]
    pub fn arm_archive_open(
        &mut self,
        wire_gen: u64,
        rx: std::sync::mpsc::Receiver<(u64, crate::archive_open::ArchiveResult)>,
        progress: pb_source::OpenProgress,
        path: std::path::PathBuf,
        attempted_password: Option<crate::SecretString>,
    ) -> Option<(crate::background::OpId, crate::background::OpKind)> {
        let (id, superseded) = self
            .bg
            .begin(crate::background::OpKind::ArchiveOpen, self.now);
        self.supersede(superseded);
        self.archive_load = Some(crate::archive_open::ArchiveOpenState {
            id,
            rx,
            wire_gen,
            path,
            attempted_password,
            progress,
        });
        superseded
    }

    /// Pick up a finished background archive open (called each `tick`).
    pub fn poll_archive_load(&mut self) -> crate::archive_open::ArchiveOutcome {
        use crate::archive_open::ArchiveOutcome;
        use std::sync::mpsc::TryRecvError;

        let (id, wire_gen, recv) = match self.archive_load.as_ref() {
            Some(l) => (l.id, l.wire_gen, l.rx.try_recv()),
            None => return ArchiveOutcome::Pending,
        };
        // One staleness gate for both flows: an open superseded by a newer open *or* by a
        // folder scan fails this, so its result can never rebuild the deck underneath.
        if !self.bg.is_current(id) {
            self.archive_load = None;
            return ArchiveOutcome::Cancelled;
        }
        match recv {
            Ok((g, result)) => {
                if g != wire_gen {
                    return ArchiveOutcome::Pending; // defensive; the channel is per-open
                }
                let load = self.archive_load.take().expect("checked above");
                self.bg.finish(id);
                self.finish_archive_open(result, load.attempted_password, load.path)
            }
            Err(TryRecvError::Empty) => ArchiveOutcome::Pending,
            Err(TryRecvError::Disconnected) => {
                // The worker died without sending a terminal result. Never strand its chrome.
                self.archive_load = None;
                self.bg.finish(id);
                ArchiveOutcome::Cancelled
            }
        }
    }

    /// Apply a completed open. Private: the password handling below must not be reachable from
    /// a shell (`crate::archive_open`'s privacy note).
    fn finish_archive_open(
        &mut self,
        result: crate::archive_open::ArchiveResult,
        attempted: Option<crate::SecretString>,
        path: std::path::PathBuf,
    ) -> crate::archive_open::ArchiveOutcome {
        use crate::archive::ArchiveOpenError;
        use crate::archive_open::ArchiveOutcome;

        let (outcome, winner) = result;
        // An archive that opened at all — even to find nothing viewable — proves its password.
        // Promote it here, inside the core: the winning secret is never returned to a shell.
        if matches!(outcome, Ok(_) | Err(ArchiveOpenError::Empty)) {
            if let Some(pw) = attempted.as_ref().or(winner.as_ref()) {
                self.remember_archive_password(pw);
            }
        }
        match outcome {
            Ok(resolved) if !resolved.source.is_empty() => {
                self.password_archive = None;
                self.handle(contract::CoreEvent::ArchiveResolved(resolved));
                ArchiveOutcome::Opened
            }
            Ok(_) => {
                self.password_archive = None;
                ArchiveOutcome::Failed(ArchiveOpenError::Empty)
            }
            Err(ArchiveOpenError::PasswordRequired) => {
                // Remember the path so a submitted password re-opens it. `wrong` is true only
                // when THIS attempt carried a password and it was rejected — a first prompt
                // opens fresh chrome, a retry corrects the chrome already up.
                self.password_archive = Some(path.clone());
                ArchiveOutcome::NeedPassword {
                    path,
                    wrong: attempted.is_some(),
                }
            }
            Err(ArchiveOpenError::Cancelled) => {
                self.password_archive = None;
                ArchiveOutcome::Cancelled
            }
            Err(e) => {
                self.password_archive = None;
                ArchiveOutcome::Failed(e)
            }
        }
    }

    /// Ask an in-flight archive open to stop. Idempotent, and it clears the handle itself so no
    /// call site has to remember (the dir-scan lesson, §11.2).
    pub fn cancel_archive_load(&mut self) {
        if let Some(load) = self.archive_load.take() {
            load.request_cancel();
        }
        if self.bg.active_is_archive() {
            self.bg.cancel();
        }
    }

    /// The in-flight open's shared progress handle, for chrome that polls it directly (the
    /// winit Loading dialog's determinate bar owns one and reads it per frame).
    ///
    /// Handing out a clone is safe and carries nothing sensitive: `OpenProgress` is a shared
    /// counter plus a cancel flag, not part of the `SecretString` path. Prefer
    /// [`archive_status`](Self::archive_status) for a one-shot read; this exists only for
    /// chrome that must keep polling.
    pub fn archive_progress(&self) -> Option<pb_source::OpenProgress> {
        self.archive_load.as_ref().map(|l| l.progress.clone())
    }

    /// A live view of the open in flight, for whatever chrome a shell draws. `None` when none
    /// is running. Cheap and non-mutating, so it is safe to call every frame.
    pub fn archive_status(&self) -> Option<crate::archive_open::ArchiveStatus> {
        let load = self.archive_load.as_ref()?;
        Some(crate::archive_open::ArchiveStatus {
            name: crate::archive_open::archive_display_name(&load.path),
            fraction: load.progress.fraction(),
            slow: self
                .bg
                .is_slow(self.now, crate::archive_open::LOADING_DIALOG_DELAY),
        })
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

    /// Whether a landed **display-rep** decode is the DEFINITIVE full for the current fit — i.e. it
    /// should end the sharpen loop (`preview_resident` cleared). A real preview never is. A
    /// decode-to-fit "full" is definitive UNLESS it was decoded at a stale, smaller fit than the
    /// current viewport while more source detail exists: a transient tiny viewport during a
    /// fullscreen toggle yields a ~256px "full" (`is_preview=false`), and treating that as final
    /// strands the photo low-res forever — the job loop then reads the resident-but-untracked slot
    /// as "already full" and never re-decodes (the stuck-preview bug, #111). Such an undersized
    /// frame must stay sharpen-eligible so the real full re-decodes at the current fit. A native-size
    /// decode (source no larger than the output — a genuinely small photo, or a video/door
    /// placeholder) is always definitive, so those never loop.
    fn decode_is_definitive_full(&self, img: &pb_decode::DecodedImage) -> bool {
        if img.is_preview {
            return false;
        }
        let Some(fit) = self.decode_fit() else {
            return true; // Original / Fill: native, geometry-independent decode
        };
        // Downscaled below the current fit on BOTH edges (SLACK absorbs decode-to-fit rounding on
        // the constraining edge, which normally lands exactly on the fit) while the source has more
        // pixels than we kept ⇒ this was decoded at a stale/smaller fit, not the current one.
        const SLACK: u32 = 4;
        let undersized = img.width + SLACK < fit.max_width && img.height + SLACK < fit.max_height;
        let has_more = img.width < img.orig_width || img.height < img.orig_height;
        !(undersized && has_more)
    }

    /// The ring [`Representation`] the current scale mode's decode produces: a
    /// viewport-sized `Fit` (Fit mode) or the full-resolution `Original` (Fill /
    /// Original — those decode at native size, geometry-independent). The parked
    /// full-res tier (#106.7) later requests `Original` explicitly even in Fit mode;
    /// this is only the *display* mode's own decode.
    pub fn display_rep(&self) -> pb_core::Representation {
        match self.decode_fit() {
            Some(_) => pb_core::Representation::Fit {
                geometry_epoch: self.epoch,
            },
            None => pb_core::Representation::Original,
        }
    }

    /// The representation kind the current scale mode displays from — the key for every
    /// "is `item` resident / which slot do I rebind" query on the display path.
    pub fn display_kind(&self) -> pb_core::RepKind {
        self.display_rep().kind()
    }

    /// Build the full [`Representation`] for a decode outcome's `kind` at the current epoch:
    /// a `Fit` carries the geometry epoch (so a stale-geometry completion is rejected), an
    /// `Original` is geometry-independent. The parked full-res tier (#106.7) produces
    /// `Original` outcomes even while the display rep is `Fit`.
    pub fn rep_of(&self, kind: pb_core::RepKind) -> pb_core::Representation {
        match kind {
            pb_core::RepKind::Fit => pb_core::Representation::Fit {
                geometry_epoch: self.epoch,
            },
            pb_core::RepKind::Original => pb_core::Representation::Original,
        }
    }

    /// Zoom deadband around 1.0 for the representation choice (task #124). Without it a zoom
    /// that lands on 1.0000001 and back would flap the bound texture every tick.
    const ZOOM_REP_EPS: f32 = 1e-3;

    /// **Which representation `item` should be PRESENTED from, accounting for zoom** (task #124).
    ///
    /// This is deliberately separate from [`display_kind`](Self::display_kind): that one is
    /// mode-derived and drives the *decode* path (`request_prefetch`, the sharpen loop, thumbs,
    /// `slot_bytes_estimate`), where zoom must change nothing. This one is a *display-time*
    /// choice over what is **already resident** — it never causes a decode.
    ///
    /// Why it exists: in Fit mode the resident texture is viewport-sized, so zooming past 1.0
    /// magnified it and showed roughly `k`× less detail than the file holds (`k` = the
    /// decode-to-fit scale). The full-res `Original` is already retained for the parked window
    /// by the #106.7 tier — precisely so a Fit↔1:1 toggle is a rebind — and smooth zoom simply
    /// had no path to it. Binding it is a pure rebind: no decode, no upload, no epoch bump.
    ///
    /// The swap is geometrically invisible because [`ViewTransform::base_scale`] is computed
    /// from the *bound texture's own* dims, so the Fit rep's `1/k` exactly cancels the decode
    /// scale `k` and both reps display at the same size (pinned by
    /// `fit_and_original_reps_display_at_the_same_size` in `pb-render`).
    pub fn present_kind(&self, item: usize) -> pb_core::RepKind {
        // 1. Fill/Original already display the Original; only Fit mode has anything to switch.
        if self.view.mode != ScaleMode::Fit {
            return self.display_kind();
        }
        // 2. At or below 1:1 the fit texture is exactly right — and it is the cheaper binding.
        if self.view.zoom <= 1.0 + Self::ZOOM_REP_EPS {
            return pb_core::RepKind::Fit;
        }
        // 3. Nothing to switch to. Graceful: this is today's behaviour, just soft.
        let Some(orig) = self.ring.original_slot(item) else {
            return pb_core::RepKind::Fit;
        };
        // 4. A same-slot Original is the same pixels — switching buys nothing.
        if self.ring.slot_for_rep(item, pb_core::RepKind::Fit) == Some(orig) {
            return pb_core::RepKind::Fit;
        }
        // 5. A photo the fit box never downscaled (`k == 1.0`, so `downscale_to_fit` clamped)
        //    has an Original identical to its Fit. Rebinding would churn for no pixels. Meta is
        //    the cheap way to know the true size; when it's unknown, allow the swap (worst case
        //    the two reps match and the rebind is a visual no-op).
        if let (Some(m), Some(fit)) = (self.meta_cache.get(&item), self.fit) {
            if m.w <= fit.max_width && m.h <= fit.max_height {
                return pb_core::RepKind::Fit;
            }
        }
        pb_core::RepKind::Original
    }

    /// The resident slot for [`present_kind`], falling back to the display slot.
    fn present_slot_for(&self, item: usize) -> Option<usize> {
        match self.present_kind(item) {
            pb_core::RepKind::Original => self
                .ring
                .original_slot(item)
                .or_else(|| self.display_slot(item)),
            pb_core::RepKind::Fit => self.display_slot(item),
        }
    }

    /// **Rebind the SAME item to a different resident representation, preserving the view**
    /// (task #124). Returns whether the renderer took it.
    ///
    /// Deliberately NOT [`present_item`]: that is the *fresh landing* path and runs
    /// [`view_for`](Self::view_for), which resets zoom/pan to the mode's natural framing —
    /// it would cancel the very zoom that asked for this rebind. It also re-stamps
    /// `last_present` (skewing slideshow dwell), re-emits the title, and records the `present`
    /// metric, none of which apply to an in-place representation swap of the photo already on
    /// screen. `try_gpu_sharpen` established this discipline for quality upgrades; this is the
    /// same shape for representation swaps.
    ///
    /// State is committed only on a successful bind: `present_slot` returns `false` on a
    /// core↔renderer ring desync, and recording a `presented_kind` we did not actually bind
    /// would make the §3.6 guards lie about what is on screen.
    fn rebind_same_item(&mut self, item: usize, slot: usize, kind: pb_core::RepKind) -> bool {
        let view = self.view; // the CURRENT view — never `view_for`
        let bound = match self.renderer.as_mut() {
            Some(r) => {
                r.set_view(view);
                r.present_slot(slot)
            }
            // Headless (unit tests) counts as bound, matching `present_item`.
            None => true,
        };
        if !bound {
            eprintln!("[ring-desync] rebind_same_item({slot}) missed for item {item}");
            return false;
        }
        self.ring.set_displayed(slot);
        self.presented_kind = Some(kind);
        #[cfg(test)]
        {
            self.rebind_count += 1;
        }
        self.draw();
        true
    }

    /// Re-select the presented representation after a **zoom** change (task #124) — the one
    /// place the three zoom mutators (`zoom_step`, `zoom_about_cursor`, the `apply_view_holds`
    /// ramp) share.
    ///
    /// ⚠ Must run **after** the caller's zoom/pan math: [`ViewTransform::zoom_about`] reads the
    /// *currently bound* texture dims via `placement()` to keep the cursor anchor pinned, so
    /// rebinding first would do that math against the wrong dims.
    ///
    /// A no-op unless the decision actually flips, so a hold-to-zoom ramp rebinds once on the
    /// way past 1:1 rather than every tick.
    pub fn reconcile_zoom_rep(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        // Don't fight an in-flight nav: the target's own present will pick the right rep.
        if self.target_item != Some(item) {
            return;
        }
        // A live video/animation draws via `set_image`, not the ring — never rebind under it.
        if self.playback.is_some() {
            return;
        }
        let want = self.present_kind(item);
        if self.presented_kind == Some(want) {
            return;
        }
        let Some(slot) = self.present_slot_for(item) else {
            return;
        };
        self.rebind_same_item(item, slot, want);
    }

    /// The resident ring slot to display `item` from in the current scale mode: its slot in
    /// the display [`Representation`] (`Fit` in Fit mode, `Original` in Fill/Original). The
    /// rebind hit-test — `Some` means "present without decoding." Distinct from the parked
    /// full-res `Original` slot (#106.7), which the display path rebinds only after a Fit↔1:1
    /// toggle flips the display kind.
    pub fn display_slot(&self, item: usize) -> Option<usize> {
        self.ring.slot_for_rep(item, self.display_kind())
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
    /// better decode to pull. `None` while blazing (sharpening a frame that's about to
    /// change is pointless) and `None` once it's already full.
    pub fn sharpen_now(&self) -> Option<usize> {
        // A fired watchdog overrides the blazing gate (ADR-024, level-triggered): a photo that
        // has lingered as a resident preview past the deadline means the "held" key is a lie
        // (the lost-key-up race) — a real blaze advances long before the watchdog fires.
        if self.held_nav().is_some() && !self.preview_watchdog_fired() {
            return None;
        }
        self.sharpen_candidate()
    }

    /// The sharpen-eligibility core minus the pacing gates (#122): the displayed photo
    /// is a resident preview that hasn't been upgraded. `sharpen_now` (the CPU decode
    /// want) adds the key-up gate; `try_gpu_sharpen` adds only the blaze-repeat gate —
    /// a GPU derive is cheap enough to run with a key still down on a tap.
    fn sharpen_candidate(&self) -> Option<usize> {
        let d = self.displayed_item?;
        (self.display_slot(d).is_some()
            && self.preview_resident.contains(&d)
            && !self.upgrade_done.contains(&d))
        .then_some(d)
    }

    /// Whether a held nav key is in the auto-repeat (blaze) phase — held AND past the
    /// initial tap delay. The distinction #122 item 1 turns on: during the pre-repeat
    /// window a press is a tap in progress, and the tap's photo deserves its instant
    /// GPU sharpen; once auto-repeat runs, frames are replaced too fast to bother.
    fn blaze_repeating(&self) -> bool {
        self.held_nav().is_some()
            && timing::elapsed_since(self.hold_start, self.now, self.initial_delay)
    }

    /// Drive the ADR-024 lingering-preview watchdog (level-triggered, from the tick): stamp when
    /// the displayed photo first shows as a resident preview, re-arm on a new displayed item,
    /// clear the moment the display isn't a resident preview (upgraded, navigated away, evicted),
    /// and mark it fired once it has lingered past [`PREVIEW_WATCHDOG_AFTER`]. Returns `true` on
    /// the firing edge only — the caller forces one prefetch re-issue then, because the tick's
    /// `last_upgrade_set` change-detection can't see an eligibility change that doesn't change
    /// the wanted-set. Two set lookups per tick, so blazing never notices it.
    fn update_preview_watchdog(&mut self) -> bool {
        // Arming requires more than "a resident preview is displayed":
        //  - `target_caught_up`: mid-blaze with the ring outrun, the *old* photo legitimately
        //    lingers while the next target decodes — forcing its full then would put a decode
        //    ahead of the previews the blaze is waiting on. The stuck race always ends caught-up
        //    (the stuck "hold" auto-advances until it parks on a presented preview).
        //  - still images only, never RAW: a video/door placeholder has no sharper full to force
        //    (its upgrade path is the poster pipeline), and a RAW's forced "full" is a
        //    seconds-long uncancellable demosaic — its embedded preview is near-full-res anyway.
        //    Cheap-first: the kind/extension checks only run while a preview is displayed.
        // Deliberately NOT gated on `upgrade_done`: that flag can be poisoned (a decode error,
        // or historically the duplicate-preview race) and a poisoned flag would gate off the
        // very safety net meant to catch it. The fire edge clears it instead — one forced
        // retry per arming cycle (the latched `fired` prevents a retry loop; navigating away
        // and back re-arms, giving the next deliberate visit one more chance).
        let lingering = self.displayed_item.filter(|d| {
            self.display_slot(*d).is_some()
                && self.preview_resident.contains(d)
                && self.target_caught_up()
                && matches!(
                    crate::video::item_kind(self.source.as_ref(), *d),
                    crate::video::LibraryItemKind::Image
                )
                && !self.is_raw_item(*d)
        });
        let Some(d) = lingering else {
            self.preview_watchdog = None;
            return false;
        };
        match &mut self.preview_watchdog {
            Some(w) if w.item == d => {
                if !w.fired && self.now.saturating_duration_since(w.since) >= PREVIEW_WATCHDOG_AFTER
                {
                    w.fired = true;
                    w.retries = w.retries.saturating_add(1);
                    let lingered = self.now.saturating_duration_since(w.since);
                    let attempt = w.retries;
                    // Second chance (ADR-024 "converge or self-correct"): a lingering preview
                    // with `upgrade_done` set means the flag lied — clear it so the sharpen
                    // path reopens. Once per arming cycle (see above); a post-fire decode
                    // ERROR may re-arm, bounded by `retries` (`rearm_watchdog_after_error`).
                    let was_poisoned = self.upgrade_done.remove(&d);
                    if sharp_diag() {
                        eprintln!(
                            "[sharp-diag] preview watchdog FIRED item={d} (lingered {lingered:?}, attempt {attempt}, held_nav={}, cleared_upgrade_done={was_poisoned}) — forcing sharpen",
                            self.held_nav().is_some(),
                        );
                    }
                    return true;
                }
                false
            }
            _ => {
                self.preview_watchdog = Some(crate::PreviewWatchdog {
                    item: d,
                    since: self.now,
                    fired: false,
                    retries: 0,
                });
                false
            }
        }
    }

    /// A full-decode ERROR just poisoned `upgrade_done` for `item` (the drain's Err branch). If
    /// the watchdog already spent its fire on this arming cycle — e.g. it fired while that very
    /// decode was still in flight, so the forced re-issue deduped into the job that then
    /// errored — re-arm it for another cycle, bounded by [`MAX_WATCHDOG_RETRIES`]: a transient
    /// SMB hiccup still converges to sharp instead of dying with the fire latched (Codex review
    /// of the duplicate-preview fix), while a permanently corrupt full stops after a few
    /// attempts. An unfired watchdog keeps its running clock untouched.
    fn rearm_watchdog_after_error(&mut self, item: usize) {
        if let Some(w) = &mut self.preview_watchdog {
            if w.item == item && w.fired && w.retries < MAX_WATCHDOG_RETRIES {
                w.fired = false;
                w.since = self.now;
            }
        }
    }

    /// Whether the watchdog is currently fired: the displayed photo has been a resident preview
    /// for over [`PREVIEW_WATCHDOG_AFTER`]. While `true`, [`sharpen_now`](Self::sharpen_now)
    /// ignores the `held_nav` blazing gate so the display converges to its full (ADR-024's
    /// invariant enforced regardless of what stuck `held_nav`).
    fn preview_watchdog_fired(&self) -> bool {
        self.preview_watchdog.is_some_and(|w| w.fired)
    }

    /// The full-res "sharp ring" to prefetch around the cursor at LOW priority (below
    /// every preview) — a VRAM-bounded, current-first prefix of the window, filtered
    /// to resident previews, minus `sharpen_now` (requested at high priority instead).
    ///
    /// Unlike `sharpen_now`, this runs EVEN WHILE BLAZING: the fulls are queued behind
    /// all previews (see `request_prefetch`), so a fast blaze stays preview-smooth — the
    /// pool decodes them only in spare capacity. But as you slow down or browse, the
    /// fulls for where you're heading land *ahead* of you, so a stop finds the photo
    /// already sharp instead of paying a cold ~115 ms–1 s decode after the fact. The
    /// workers that decode them would otherwise be idle, so it's near-free.
    pub fn prefetch_fulls(&self) -> Vec<usize> {
        let full_bytes = self.slot_bytes_estimate();
        let sharpen = self.sharpen_now();
        let mut ring: Vec<usize> = full_ring(
            &self.targets,
            full_bytes,
            RING_BUDGET_BYTES,
            self.ring.capacity().min(MAX_FULL_RING),
        )
        .into_iter()
        .filter(|&i| {
            Some(i) != sharpen
                && self.display_slot(i).is_some()
                && self.preview_resident.contains(&i)
                && !self.upgrade_done.contains(&i)
                && !self.is_raw_item(i)
        })
        .collect();
        // "+1 never waits" (owner, 2026-07-19): fulls decode NEAREST-first around the cursor,
        // not in the ahead-biased window order — after a forward blaze `targets` ranks the item
        // just BEHIND you ~10th, so backing up one within seconds of parking found a preview
        // still waiting on its full. The membership (budget prefix) is unchanged; only the
        // decode ORDER changes. The stable sort keeps ahead-of-cursor ahead of behind-of-cursor
        // at equal distance. Three gates (Codex review of d443751f):
        //  - PARKED only: mid-blaze the fulls should land AHEAD, where you're heading — the
        //    window order already encodes that.
        //  - Sequential travel only: in Random mode the likely next view is the next SHUFFLE
        //    item (already first in window order) — sequential distance would misprioritize it.
        //  - Wrap-aware distance: with wrap on, the last item is one Backspace from item 0.
        if self.held_nav().is_none()
            && !matches!(self.playlist.last_direction(), pb_core::Direction::Random)
        {
            if let Some(cur) = self.playlist.current() {
                let len = self.playlist.len();
                let wraps = self.playlist.wraps() && len > 0;
                ring.sort_by_key(|&i| {
                    let d = i.abs_diff(cur);
                    if wraps {
                        d.min(len - d)
                    } else {
                        d
                    }
                });
            }
        }
        ring
    }

    /// Whether `item`'s full decode is a slow RAW demosaic (seconds, and once started
    /// it can't be cancelled). Excluded from the speculative ahead-ring so a few RAWs
    /// in the window can't tie up the decode workers — starving the previews a blaze
    /// needs — for neighbours you may never visit. The displayed RAW still sharpens
    /// via `sharpen_now`, and a RAW's embedded preview is often near-full-res anyway.
    pub fn is_raw_item(&self, item: usize) -> bool {
        Path::new(self.source.name(item))
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| pb_decode::is_raw_extension(&e.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    /// The parked full-res radius in effect — the setting, clamped to the hard cap.
    pub fn full_res_radius(&self) -> usize {
        self.settings
            .full_res_radius
            .min(settings::FULL_RES_RADIUS_MAX) as usize
    }

    /// The sequential window whose *other* representation the parked full-res tier holds
    /// (#106.7): **current → compare-pin → ±`radius` neighbours**. The pin rides at priority 2
    /// (a promised instant rebind that may be a distant item), then the sequential neighbours
    /// outward. De-duplicated, order preserved.
    pub fn full_res_window(&self, radius: usize) -> Vec<usize> {
        let Some(cur) = self.playlist.current() else {
            return Vec::new();
        };
        let len = self.source.len();
        let mut out = Vec::new();
        let push = |v: &mut Vec<usize>, i: usize| {
            if i < len && !v.contains(&i) {
                v.push(i);
            }
        };
        push(&mut out, cur);
        if let Some(pin) = self.compare_pin {
            push(&mut out, pin);
        }
        for d in 1..=radius {
            push(&mut out, cur + d);
            if cur >= d {
                push(&mut out, cur - d);
            }
        }
        out
    }

    /// Whether `item` may be held at full resolution by the parked tier (#106.7 §8/§9). Only a
    /// real still image has a geometry-independent "original": excludes videos and archive
    /// doors (no still full-res), SVG (rasterised per-viewport — no fixed original satisfies an
    /// arbitrary fit), RAW (a slow, uncancellable demosaic — like the ahead-ring), and anything
    /// past the gigapixel ceiling (kept fit-only, since 1:1 of a gigapixel is a meaningless crop
    /// and its RGBA8 decode buffer would be multiple GB). The ceiling reads the true resolution
    /// from `meta_cache`; an item not yet metadata-known is admitted and re-checked once known.
    pub fn full_res_eligible(&self, item: usize) -> bool {
        if !matches!(
            crate::video::item_kind(self.source.as_ref(), item),
            crate::video::LibraryItemKind::Image
        ) {
            return false; // video or archive door
        }
        let ext = Path::new(self.source.name(item))
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if ext.as_deref() == Some("svg") || self.is_raw_item(item) {
            return false;
        }
        // Gigapixel ceiling: never request/retain an original past the pixel cap.
        if let Some(m) = self.meta_cache.get(&item) {
            if (m.w as u64) * (m.h as u64) > FULL_RES_MAX_PIXELS {
                return false;
            }
        }
        true
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
        // The strip draws session rotation per cell (task #83) — re-signal it.
        if self.thumbs_visible() {
            self.emit_panels_changed();
        }
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
        // A video's container probe may be in flight (task 98.6). Copying now would hand
        // the user a table missing its Duration/codec/track rows, so mark the probe
        // copy-when-done and let `poll_details_probe` re-enter — the same contract
        // `copy_image_text` uses for a scan that hasn't finished.
        if self
            .exif_cache
            .get(&item)
            .is_some_and(|d| d.probe_state == crate::media_details::ProbeState::Loading)
        {
            if let Some(p) = self.details_probe.as_mut().filter(|p| p.item == item) {
                p.copy_when_done = true;
                self.show_toast("Reading video details…");
                return;
            }
        }
        let mut lines: Vec<String> = vec![file_name_of(self.source.name(item)).to_string()];
        if let Some(meta) = &self.current {
            lines.push(format!("Dimensions: {} × {}", meta.w, meta.h));
            lines.push(format!("Codec: {}", meta.codec.to_uppercase()));
        }
        if let Some(details) = self.exif_cache.get(&item) {
            lines.push(format!(
                "File Size: {} bytes",
                hud::format_thousands(details.size)
            ));
            for (tag, val) in &details.fields {
                // Skip binary blobs (Apple MakerNote/Padding) that render as meaningless hex.
                if is_exif_blob(tag, val) {
                    continue;
                }
                lines.push(format!("{tag}: {val}"));
            }
            // The audio/subtitle tracks (task #98) — the same rows the panel shows, so
            // "Copy Image Details" and the panel can't disagree about the same file.
            if let Some(catalog) = &details.media {
                for row in crate::tracks::track_rows(catalog, details.has_audio) {
                    lines.push(match row {
                        DetailRow::Span { text, .. } => text,
                        DetailRow::Pair { label, value } => format!("{label}: {value}"),
                    });
                }
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
            .map(|d| d.fields.as_slice())
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
            has_motion: self.has_motion(item) || self.item_is_video(item),
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
        // GPU-submit), the keypress fast path. It's the metric to watch for a hold-to-blaze
        // regression: the NS0 inversion (renderer behind `Box<dyn Renderer>`, window ops as
        // effects) must leave this flat. `--metrics` only; a no-op branch otherwise.
        let t0 = Instant::now();
        let view = self.view_for(item);
        let title = title_for(self.source.name(item), item, self.source.len());
        // Ask the renderer to rebind the slot. It returns `false` when that slot isn't uploaded
        // in its ring (it keeps the previous frame): a core↔renderer ring desync. That is NOT the
        // "archive card over a photo" bug — that one is a cross-deck open race where `present_slot`
        // returns *true* with the wrong occupant (see `apply_scan_batch`). So this is a loud
        // diagnostic only, never a control-flow branch: the earlier invalidate-on-miss repair
        // (cff70ca0 / c383107a) was unsafe — it bumped the epoch mid `drain_results` loop and
        // purged the retained full-res tier (regressing instant fullscreen to a preview flash) —
        // and is deliberately reverted here. Headless (`renderer = None`, unit tests) counts as
        // presented so the pure-core assertions hold; the follow-up to propagate this result
        // properly (abort the drain, resync once after the loop) is tracked, not done inline.
        let presented = match self.renderer.as_mut() {
            Some(r) => {
                r.set_view(view);
                r.present_slot(slot)
            }
            None => true,
        };
        if !presented && door_diag() {
            eprintln!(
                "[door-diag] present_slot({slot}) missed for item {item} (archive_kind={:?})",
                self.item_archive_kind(item),
            );
        }
        // #123 fix 2: the stash is current-photo-scoped — a DIFFERENT photo successfully
        // on screen retires it. Present-success, not mere target churn (a failed present
        // must not orphan pixels we may still return to).
        if presented && self.fit_stash.iter().flatten().any(|s| s.item != item) {
            self.clear_fit_stash();
        }
        self.effects.push(contract::CoreEffect::SetTitle(title));
        self.ring.set_displayed(slot);
        // #124: record WHICH representation is now on screen. `present_item` is the fresh
        // landing path and `view_for` above reset zoom to 1.0, so the mode-derived answer is
        // the correct one here. The background quality paths read this to avoid rebinding
        // their derived `Fit` over a zoom-selected `Original`.
        self.presented_kind = Some(self.display_kind());
        // A fresh landing on a *different* photo re-arms the play hint. `anim_hint_shown_for`
        // is keyed to the item and only updated when landing on an animated one — so without
        // this, visiting a non-animated photo in between would leave it latched, and returning
        // to an animated photo (or arriving from a non-animated one) wouldn't re-show the hint.
        // Guarded on the item actually changing, so a re-present of the same photo (e.g. a play
        // reverting to its still) doesn't re-arm it.
        if self.displayed_item != Some(item) {
            self.anim_hint_shown_for = None;
            // Navigating to a different photo ends any resize hold on the previous one (its
            // quality-monotonic preview guard no longer applies).
            if self.resize_hold.is_some() && self.resize_hold != Some(item) {
                self.resize_hold = None;
            }
        }
        self.mark_resolved(item);
        self.current = self.meta_cache.get(&item).cloned();
        // Diagnosis affordance (the 2026-07-19 stuck repro was SILENT in the logs — nothing
        // prints while parked on a blurry photo whose sharpen is gated off): one line per
        // parked landing on a resident preview, naming the gate state. Event-driven, diag-only.
        if sharp_diag() && self.held_nav().is_none() && self.preview_resident.contains(&item) {
            eprintln!(
                "[sharp-diag] parked on item={item} as a resident PREVIEW: upgrade_done={} ({})",
                self.upgrade_done.contains(&item),
                if self.upgrade_done.contains(&item) {
                    "sharpen BLOCKED — watchdog second-chance due in ~2s"
                } else {
                    "sharpen expected now"
                }
            );
        }
        // The panel (if shown) is now stale for the old photo; `about_to_wait`
        // rebuilds it for `item` next tick (or hides it while blazing), so it
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
        // Terminal at this epoch: a corrupt target counts as "resolved" so readiness
        // (`target_caught_up`) doesn't leave the loading pie spinning forever on it.
        self.mark_resolved(item);
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

    /// Record that `item` was **resolved** — presented or terminally failed — at the
    /// *current* geometry epoch. Stamping `presented_epoch` here is what lets
    /// [`target_caught_up`](Self::target_caught_up) tell a fresh frame from a stale one
    /// after a geometry change bumps the epoch (task #18 finding #5).
    fn mark_resolved(&mut self, item: usize) {
        self.displayed_item = Some(item);
        self.presented_epoch = Some(self.epoch);
        // Perf (PB_PERF): the current photo just went on screen — close out whichever
        // episode was waiting on it (open→first-photo, or a resize→on-screen).
        if let Some((ep, d)) = self.perf.presented(self.now) {
            eprintln!("[perf] {}: {} ms", ep.label(), d.as_millis());
            self.metrics.record(ep.label(), d);
        }
    }

    /// Perf (PB_PERF): note that `item` reached full residency, and report open→all-cached
    /// the moment the last one lands. A no-op when `PB_PERF` is unset.
    fn perf_note_full(&mut self, item: usize) {
        if let Some((n, d)) = self.perf.full_resident(item, self.now) {
            eprintln!("[perf] open->all-cached ({n} photos): {} ms", d.as_millis());
            self.metrics.record("open->all-cached", d);
        }
    }

    /// Whether the on-screen frame is the current target **at the current fit** — i.e.
    /// nothing more needs presenting. Item identity alone isn't enough: a resize /
    /// scale-mode change / deck rebuild bumps `epoch` (see [`invalidate_geometry`]), so
    /// the same item must be re-presented at the new geometry before it's "caught up."
    /// Drives the present guards, the loading pie, and the nav/slideshow readiness gates.
    pub fn target_caught_up(&self) -> bool {
        self.target_item.is_some()
            && self.displayed_item == self.target_item
            && self.presented_epoch == Some(self.epoch)
    }

    /// The inverse used by the readiness gates: there **is** a target and it isn't yet
    /// on screen at the current fit. Distinct from `!target_caught_up()` in the idle
    /// case — with no target there is nothing pending (so the loop can sleep).
    pub fn target_pending(&self) -> bool {
        self.target_item.is_some() && !self.target_caught_up()
    }

    /// Try to show `target_item`: present it on a ring hit, otherwise keep the
    /// previous frame (a miss is a hold, never a skip). Returns whether shown.
    pub fn try_present_target(&mut self) -> bool {
        let Some(item) = self.target_item else {
            return false;
        };
        if self.target_caught_up() {
            return true;
        }
        if self.failed.contains(&item) {
            // Known-bad file: count it as shown (the previous frame stays up) so
            // navigation never stalls on a corrupt prefetched JPEG.
            self.present_failed(item);
            return true;
        }
        if let Some(slot) = self.display_slot(item) {
            self.present_item(item, slot);
            true
        } else if self.try_gpu_derive_fit() {
            // item-6 6b: no Fit resident, but the target's Original survived the last geometry
            // change (retain-and-remap) — the GPU derives + presents its exact-size Fit in a
            // couple of milliseconds, so advancing right after a fullscreen toggle lands sharp
            // instead of flashing the ~256 px preview for seconds. Parked-only by the derive's
            // own gate; a blaze keeps the preview-first hold-don't-skip behaviour.
            true
        } else {
            false
        }
    }

    /// Land a selection's ready-made tile. It is already thumb-sized (cut on
    /// the worker from the chosen frame), so a passthrough-color tile inserts
    /// STRAIGHT into the cache — same tick as the poster. It used to ride the
    /// derive queue, which is bounded and silently DROPS under a browse burst
    /// (every photo's T0 capture shares it): the app was throwing away a tile
    /// it was literally holding, then re-walking the whole film for it later at
    /// bottom priority — the owner's "the thumbnail arrives way after the
    /// video" report. An enabled-transform tile still routes through the
    /// derive thread for its color bake.
    fn land_selection_tile(&mut self, item: usize, img: pb_decode::DecodedImage) {
        // Retain the tile even when the strip has never been opened. `thumbs.enabled`
        // gates TWO different costs, and only one of them is worth gating here:
        //
        //  - scheduling thumb-FILL walks for a feature that may never be turned on —
        //    genuinely expensive, and still gated (the fill planner checks
        //    `thumbs_visible()`, so nothing extra is scheduled by this call);
        //  - retaining a tile the poster walk ALREADY cut — nearly free, and dropping
        //    it is pure waste.
        //
        // `cut_selection` cuts `thumb_img` on every selection regardless (the decode
        // layer can't see the strip), and it is already counted against the pool's
        // byte budget on the way back. Returning early here therefore threw away a
        // thumbnail we had decoded and paid for — and every one discarded before the
        // first panel open came back later as a REPLAY decode (~273 ms over SMB, vs
        // ~7 ms to bake the tile we already had) at the bottom of the priority list.
        // That is the "open the strip late and wait 30+ seconds" report.
        //
        // `enable_capture`, NOT `enable`: this must not unlock the strip's own
        // scheduled work. `enabled` still gates fill planning and the T0 photo
        // byproduct derive — the latter matters, since flipping it here would make
        // every displayed photo in a folder containing one video pay a derive on
        // every frame of a blaze.
        self.thumbs.enable_capture();
        if img.color.enabled {
            self.thumbs.offer(item, img);
            return;
        }
        let Some(cur) = self.playlist.current() else {
            return;
        };
        let demand = self.thumbs.demand(cur);
        let bytes = img.pixels.len() as u64;
        self.thumbs.cache.insert(
            item,
            pb_core::ThumbTier::Full,
            img.width,
            img.height,
            bytes,
            crate::thumbs::ThumbPixels {
                rgba: img.pixels,
                orig_w: img.orig_width,
                orig_h: img.orig_height,
                codec: img.codec,
            },
            &demand,
        );
        if self.thumbs_visible() {
            self.emit_panels_changed(); // the strip re-pulls tiles on this signal
        }
    }

    /// Fan out one finished poster-selection payload (task #114): install the
    /// choice, offer the thumb tile, and hand the geometry-fresh Fit artifact to
    /// the normal display upload path as a synthetic outcome. A stale-deck
    /// payload drops wholesale; a stale-GEOMETRY Fit drops alone (the choice
    /// survives — phase 1 recuts via one fresh walk). Failures map to the legacy
    /// per-domain failed sets until the phase-4 retry machine lands.
    fn route_poster_selection(
        &mut self,
        mut o: crate::decode_pool::Outcome,
        ready: &mut Vec<crate::decode_pool::Outcome>,
    ) {
        let item = o.key.item;
        // #119: the deck identity is a real field now (`key.epoch` no longer smuggles
        // it). The ingestion/drain gates already reject cross-deck outcomes; this stays
        // as the authoritative back-stop for the direct-routing paths.
        let gen = o.key.content_gen;
        if gen != self.content_gen {
            return; // another deck's walk — nothing here may touch this deck
        }
        let (thumb_want, display_want) = self.poster_sel.demands(item);
        let selection = o.selection.take();
        match selection {
            Some(Ok(sel)) => {
                // The generation fence is AUTHORITATIVE (review f6): a refused
                // install (e.g. a mis-fenced selector on a fresh core) must drop
                // the payload wholesale, or pixels display while the ledger
                // stays `Selecting` and re-walks forever.
                if !self.poster_sel.choose(item, gen, sel.choice) {
                    return;
                }
                self.retry_recover(item);
                if let Some(t) = sel.thumb_img {
                    self.land_selection_tile(item, t);
                }
                if display_want {
                    // BOTH halves of the artifact tag must match (review f1): the
                    // epoch alone would admit a promoted thumb-only walk's
                    // ~thumb-sized output as the display poster.
                    let tag_fresh = o.fit_tag_epoch == self.epoch && o.fit_tag == self.decode_fit();
                    match sel.fit_img {
                        Some(img) if tag_fresh => {
                            // Representation-aware (review f2): Fill/Original
                            // mode displays the Original rep, and the fit=None
                            // walk produced native pixels for exactly that. Each
                            // artifact carries its SHARE of the pool byte-budget
                            // (review f4 + phase 3): backpressure follows the
                            // pixels through pending_uploads.
                            let bytes = img.pixels.len();
                            ready.push(crate::decode_pool::Outcome::synthetic_carved(
                                &mut o,
                                item,
                                self.epoch,
                                self.display_kind(),
                                Ok(img),
                                bytes,
                            ));
                        }
                        _ => {
                            // Resized/mode-switched mid-walk: the artifact alone
                            // is stale. The CHOICE survives (review p2/3 f1) —
                            // the entry stays Chosen, and the next emission pass
                            // captures it as a replay hint before reopening, so
                            // the recut is a single seek-decode, never a fresh
                            // scored walk.
                        }
                    }
                    // Phase 3: the native winner becomes the video's Original,
                    // so a resize GPU-derives the new Fit like a photo (#110 —
                    // the resize-spinner kill). Mode-0 only: an enabled color
                    // transform would store mode-1 (deliberately unmipped and
                    // derive-rejected, gpu.rs) — those posters keep the replay
                    // path, color-correct; their fp16 bake lands with 110d.
                    // Fill/Original mode already consumed the native as its
                    // display artifact, so this is Fit-mode work.
                    if self.display_kind() == pb_core::RepKind::Fit {
                        if let Some(native) = sel.native {
                            if native.color.enabled {
                                // Mode-1: remember, so the parked pre-install
                                // stops replaying this video forever.
                                self.poster_sel.block_original(item);
                            } else if native.width.max(native.height)
                                > pb_decode::video::POSTER_NATIVE_CAP_EDGE
                            {
                                // The capped negotiation was rejected and the
                                // walk fell back to full native above the
                                // ceiling (review p2/3 f4): never admit it.
                                self.poster_sel.block_original(item);
                            } else if self
                                .ring
                                .slot_for_rep(item, pb_core::RepKind::Original)
                                .is_none()
                            {
                                let bytes = native.pixels.len();
                                ready.push(crate::decode_pool::Outcome::synthetic_carved(
                                    &mut o,
                                    item,
                                    self.epoch,
                                    pb_core::RepKind::Original,
                                    Ok(native),
                                    bytes,
                                ));
                            }
                        }
                    }
                }
            }
            Some(Err(e)) => {
                if e.is_cancelled() {
                    return; // belt-and-braces; the pool discards these upstream
                }
                // Legacy failure mapping per LIVE demand domain (phase 4
                // replaces this with the WaitingForReentry retry machine).
                if display_want {
                    eprintln!("poster selection failed for item {item}: {e}");
                    self.failed.insert(item);
                    if self.target_item == Some(item) {
                        self.present_failed(item);
                    }
                }
                if thumb_want {
                    self.thumbs.failed.insert(item);
                }
                self.retry_fail(item);
                self.poster_sel.forget(item);
            }
            None => {} // unreachable by construction (PosterSelect always carries it)
        }
    }

    /// Drain finished decodes: discard stale/duplicate results, handle decode
    /// errors, then upload the highest-priority ready images (**current target
    /// first**) into ring slots — at most `UPLOADS_PER_TICK` per tick so a burst
    /// can't blow the frame budget. Lower-priority leftovers are stashed for the
    /// next tick (so the target never waits behind neighbors), keeping their pool
    /// byte-budget reservation as backpressure.
    pub fn drain_results(&mut self) {
        // The representation kind the display path rebinds from this epoch (#106.7): every
        // "is item resident / which slot" query below is against the display rep.
        let dk = self.display_kind();
        // Gather everything ready plus last tick's leftovers, dropping stale /
        // duplicate / errored results so only live decoded images remain. The
        // staleness gate runs BEFORE any purpose-specific routing (#119, Codex r1
        // f1): PosterSelect and Thumb are peeled off below, so a gate after them
        // would never judge them at all.
        let mut ready: Vec<Outcome> = std::mem::take(&mut self.pending_uploads);
        while let Ok(o) = self.results.try_recv() {
            ready.push(o);
        }
        ready.retain(|o| !outcome_stale(self.epoch, self.content_gen, o));
        // Poster-selection payloads (task #114) are matched FIRST — before the
        // thumb branch and the display routing. They carry multiple artifacts,
        // their key.epoch is the CONTENT generation (not geometry), and their
        // image slot is a placeholder no consumer may read; the router fans the
        // artifacts out (and may push a synthetic display outcome back into
        // `ready` for the normal upload path below).
        let mut i = 0;
        while i < ready.len() {
            if ready[i].key.purpose == crate::decode_pool::Purpose::PosterSelect {
                let o = ready.remove(i);
                self.route_poster_selection(o, &mut ready);
            } else {
                i += 1;
            }
        }
        // Thumb-purpose results (task #83) feed the thumb store, never the ring.
        // Geometry-epoch staleness doesn't apply to them (thumbs are display-
        // geometry-independent); deck staleness is fenced by the cache's own
        // deck generation at insert.
        let mut i = 0;
        while i < ready.len() {
            if ready[i].key.purpose == crate::decode_pool::Purpose::Thumb {
                let o = ready.remove(i);
                let item = o.key.item;
                match o.into_image() {
                    Some(img) => {
                        self.retry_recover(item);
                        self.thumbs.offer(item, img);
                    }
                    // A failed thumb fill gates the strip — with ONE bounded
                    // demand-re-entry second chance (phase 4).
                    None => {
                        self.retry_fail(item);
                        self.thumbs.failed.insert(item);
                    }
                }
            } else {
                i += 1;
            }
        }
        let mut target_failed: Option<usize> = None;
        ready.retain(|o| {
            if outcome_stale(self.epoch, self.content_gen, o) {
                return false; // stale per its validity domain (#119)
            }
            if sharp_diag() && o.key.epoch != self.epoch {
                // The #119 fix firing in the wild: a viewport-independent decode that
                // outlived a geometry change (a fullscreen toggle) is being admitted
                // instead of thrown away and restarted.
                eprintln!(
                    "[sharp-diag] cross-epoch {:?} admitted item={} (content-valid survivor)",
                    o.key.rep_kind, o.key.item
                );
            }
            let item = o.key.item;
            let rk = o.key.rep_kind;
            // Residency is per-representation (#106.7): a Fit and an Original of the same
            // item are independent slots.
            let resident = self.ring.slot_for_rep(item, rk).is_some();
            if let Err(ref e) = o.result {
                if resident {
                    // A full-upgrade decode failed while its preview is resident. Poisoning
                    // `upgrade_done` stops a tight retry loop, but ONLY a genuine display-tier
                    // full may do it (Codex review of the duplicate-preview fix):
                    //  - never a preview-request job (a failed preview — duplicate or not —
                    //    proves nothing about the full);
                    //  - never a non-Fit rep (`upgrade_done` is display-tier bookkeeping; a
                    //    duplicate parked-Original erroring must not gate the Fit sharpen);
                    //  - only while the resident Fit IS still a preview (a late failed full
                    //    after the GPU sharpen already replaced it would otherwise leave a
                    //    stale flag on a sharp photo).
                    // And when it does poison, the fired watchdog re-arms (bounded) so a
                    // transient error converges instead of dying latched.
                    if !o.preview
                        && rk == pb_core::RepKind::Fit
                        && self.preview_resident.contains(&item)
                    {
                        if sharp_diag() && self.displayed_item == Some(item) {
                            eprintln!(
                                "[sharp-diag] full landed item={item} ERROR ({e}) -> upgrade_done (watchdog retry if budget remains)"
                            );
                        }
                        self.upgrade_done.insert(item);
                        self.rearm_watchdog_after_error(item);
                    }
                    return false;
                }
                // A parked Original decode failing is not the display photo failing — the
                // Fit is unaffected. Only a *display-rep* decode failure marks the item bad.
                if rk != dk {
                    return false;
                }
                eprintln!("decode failed for item {item}: {e}");
                self.retry_fail(item);
                self.failed.insert(item);
                // Unstick the gated loop: a corrupt target counts as "shown".
                // (Deferred out of the closure — `present_failed` needs &mut self.)
                if self.target_item == Some(item) {
                    target_failed = Some(item);
                }
                return false;
            }
            if resident {
                // An Original is full-res only (never a preview), so an already-resident
                // Original outcome is a pure duplicate — drop it.
                if rk == pb_core::RepKind::Original {
                    return false;
                }
                // Fit already resident. The only outcome we still want is a *full* decode
                // upgrading a resident preview (uploaded in place below). A preview-only
                // upgrade result (e.g. RAW whose only image is its preview) is marked done
                // here so the idle pass stops retrying — otherwise the upgrade loops
                // forever. Any other already-resident duplicate is dropped.
                let is_prev = self.preview_resident.contains(&item);
                let img = o.result.as_ref().expect("Err handled above");
                // A preview image from a preview REQUEST landing on an already-resident preview
                // is a DUPLICATE — the pool untracks a finished job before its outcome is
                // drained, so a blaze-time `request_prefetch` re-issue can decode the same
                // preview twice. It must be dropped WITHOUT touching `upgrade_done`: treating
                // it as "the full came back a preview" gated off both the sharpen loop and the
                // watchdog, leaving the photo stuck blurry until a geometry change purged the
                // flag (the owner-hit back-up-one-after-a-blaze repro, 2026-07-19). Only a
                // preview image from a FULL request (`o.preview == false` — e.g. a RAW whose
                // only image IS its preview) may end the sharpen loop.
                let duplicate_preview = is_prev && img.is_preview && o.preview;
                if sharp_diag() && (self.displayed_item == Some(item) || duplicate_preview) {
                    let decision = if duplicate_preview {
                        "DROPPED (duplicate preview outcome — not an upgrade verdict)"
                    } else if is_prev && img.is_preview {
                        "upgrade_done (a FULL request came back a preview — stays as-is)"
                    } else if is_prev {
                        "UPGRADE (sharpen applied)"
                    } else {
                        "DROPPED (item not preview_resident when the full landed)"
                    };
                    eprintln!(
                        "[sharp-diag] full landed item={item} is_preview={} is_prev={} job_preview={} rk={:?} -> {decision}",
                        img.is_preview, is_prev, o.preview, rk
                    );
                }
                if is_prev && img.is_preview && !o.preview {
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
            // The representation this decode belongs to (#106.7): its own Fit/Original slot.
            // A parked Original lands here while the display rep is Fit — it reserves its own
            // slot and is NOT presented (only `rk == dk` outcomes present).
            let rk = outcome.key.rep_kind;
            let Ok(ref img) = outcome.result else {
                continue; // errors were already filtered out above
            };
            // A full decode for an item already resident as a preview is its in-place upgrade
            // (the retain above kept only real fulls; preview-only upgrade results were already
            // marked `upgrade_done` and dropped). Originals never have a preview, so this is
            // false for them — they take the reserve path.
            let upgrade =
                self.preview_resident.contains(&item) && self.ring.slot_for_rep(item, rk).is_some();
            if uploads >= UPLOADS_PER_TICK {
                // Carry still-wanted leftovers to the next tick (in priority order);
                // drop now-obsolete ones so they don't pin pool byte-budget while
                // the loop idles (work_pending wouldn't keep polling for them).
                if self.targets.contains(&item)
                    && (upgrade || self.ring.slot_for_rep(item, rk).is_none())
                {
                    leftover.push(outcome);
                }
                continue;
            }
            if !self.meta_cache.contains_key(&item) {
                let m = meta_for(self.source.as_ref(), item, &self.root, img);
                self.meta_cache.insert(item, m);
            }
            // Byte accounting (mip plan §4d): an Original uploads WITH its mip chain (unless
            // source-ICC mode 1, which is never mipped), so its true VRAM is ~4/3× L0 — record
            // the exact chain sum, not `pixels.len()`. Fit/preview textures stay single-level.
            let mode1 = render_color(&img.color).enabled && !is_hdr(img);
            let item_bytes = if rk == pb_core::RepKind::Original && !mode1 {
                mip_chain_bytes(img.width, img.height, if is_hdr(img) { 8 } else { 4 })
            } else {
                img.pixels.len() as u64
            };
            let rep = self.rep_of(rk);
            let cg = self.content_gen;
            if upgrade {
                let slot = self
                    .ring
                    .slot_for_rep(item, rk)
                    .expect("resident as preview");
                // §4d: an in-place preview→full upgrade GROWS the slot — evict lower-priority
                // slots first so the ring is never left over budget (`set_slot_bytes` below
                // adjusts the count but performs no eviction itself).
                self.ring
                    .make_room_for_upgrade(item, rk, item_bytes, &self.targets);
                let mut uploaded = true;
                if let Some(a) = self.renderer.as_mut() {
                    let t0 = Instant::now();
                    uploaded = a.upload_slot(
                        slot,
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(img),
                        img.peak,
                        rk == pb_core::RepKind::Original,
                    );
                    self.metrics.record("upload", t0.elapsed());
                }
                if !uploaded {
                    // #109.4 fail-loud bridge: the renderer refused the upgrade upload (an
                    // out-of-bounds slot — its ring and this one have desynced capacities).
                    // The preview texture is still what it shows, so every flag stays as-is
                    // (`preview_resident` in particular keeps the photo sharpen-eligible);
                    // recording the upgrade anyway is how a desync hides until a frozen view.
                    eprintln!(
                        "[ring-desync] upgrade upload refused: slot {slot} item {item} — \
                         core/renderer ring capacities out of sync"
                    );
                    self.thumbs_capture(outcome);
                    continue;
                }
                self.ring.set_slot_bytes(item, rep.kind(), item_bytes);
                if self.decode_is_definitive_full(img) {
                    self.preview_resident.remove(&item);
                    // The fresh full for a resize-held photo has landed — release the quality-monotonic
                    // hold so the sharp Fit presents below (upgrade path) and normal preview behaviour
                    // resumes.
                    if self.resize_hold == Some(item) {
                        self.resize_hold = None;
                    }
                    self.perf_note_full(item); // preview upgraded to full → one more cached
                } else if sharp_diag() {
                    // An undersized "full" (decoded at a stale/tiny fit) upgraded the preview in place
                    // but is still low-res: keep `preview_resident` so it re-decodes at the current fit
                    // instead of sticking blurry forever (#111).
                    eprintln!(
                        "[sharp-diag] upgrade got UNDERSIZED full item={item} dims={}x{} — kept sharpen-eligible",
                        img.width, img.height
                    );
                }
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
                // #124: don't bind the landed Fit over a zoom-selected `Original` (the third
                // of the three background rebind paths). The comment above is right that
                // `present_slot` preserves zoom/pan -- but it says nothing about the
                // REPRESENTATION, which is exactly what a zoom chose.
                if self.displayed_item == Some(item)
                    && self.presented_kind != Some(pb_core::RepKind::Original)
                {
                    if let Some(a) = self.renderer.as_mut() {
                        a.present_slot(slot);
                    }
                    self.presented_kind = Some(pb_core::RepKind::Fit);
                    self.draw();
                }
                self.thumbs_capture(outcome);
                continue;
            }
            if let Some(res) = self
                .ring
                .reserve_bytes(item, cg, rep, item_bytes, &self.targets)
            {
                let mut uploaded = true;
                if let Some(a) = self.renderer.as_mut() {
                    let t0 = Instant::now();
                    uploaded = a.upload_slot(
                        res.slot,
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(img),
                        img.peak,
                        rk == pb_core::RepKind::Original,
                    );
                    self.metrics.record("upload", t0.elapsed());
                }
                if !uploaded {
                    // #109.4 fail-loud bridge: the renderer refused the upload (an out-of-
                    // bounds slot — its ring and this one have desynced capacities). Marking
                    // residency anyway is exactly the mirror-says-resident / renderer-has-
                    // nothing drift; roll the reservation back instead (a stuck Pending
                    // would block this item's next decode) and surface the divergence here.
                    eprintln!(
                        "[ring-desync] upload refused: slot {} item {item} — core/renderer \
                         ring capacities out of sync",
                        res.slot
                    );
                    self.ring.release_pending(item, res.slot, cg, rep);
                    self.thumbs_capture(outcome);
                    continue;
                }
                if !self.ring.mark_resident(item, res.slot, cg, rep) {
                    // Unreachable while reserve→upload→mark is one synchronous stretch
                    // (nothing can invalidate the reservation in between) — but if it ever
                    // fires, the renderer holds a texture the core refuses to track: shout
                    // and skip the residency bookkeeping rather than present an untracked
                    // slot (#109.4 — the check this call ignored for its whole life).
                    eprintln!(
                        "[ring-desync] mark_resident refused a just-reserved slot {} item {item}",
                        res.slot
                    );
                    self.thumbs_capture(outcome);
                    continue;
                }
                self.retry_recover(item);
                // `preview_resident` and the cache metrics track the DISPLAY texture only; a
                // parked Original (rk != dk) is held silently and must not touch either.
                if rk == dk {
                    if self.decode_is_definitive_full(img) {
                        self.preview_resident.remove(&item);
                        // A fresh full for a resize-held photo landed directly (no preview step) —
                        // release the hold so it presents below.
                        if self.resize_hold == Some(item) {
                            self.resize_hold = None;
                        }
                        self.perf_note_full(item); // a fresh full landed directly (no preview step)
                    } else {
                        // A real preview, OR an undersized "full" decoded at a stale/tiny fit (a
                        // transient viewport during a fullscreen toggle → ~256px): keep it
                        // sharpen-eligible so the real full re-decodes at the current fit, instead of
                        // freezing the photo low-res forever (the stuck-preview bug, #111).
                        self.preview_resident.insert(item);
                    }
                    // DIAGNOSTIC (stuck-preview desync): what landed in a FRESH slot for the ON-SCREEN
                    // photo and how it was classified. A tiny (~256px) image arriving is_preview=false
                    // is an undersized full kept sharpen-eligible by `decode_is_definitive_full`.
                    if sharp_diag() && self.displayed_item == Some(item) {
                        eprintln!(
                            "[sharp-diag] reserve upload item={item} is_preview={} dims={}x{} -> preview_resident={}",
                            img.is_preview,
                            img.width,
                            img.height,
                            self.preview_resident.contains(&item),
                        );
                    }
                }
                uploads += 1;
                // Present the target when it lands in the DISPLAY rep — including a *re-present
                // of the same item* after a geometry change (epoch bumped ⇒ `target_caught_up`
                // is false even though the index matches), so a resize / scale-mode / rebuild
                // swaps the held stale-scale frame for the fresh one (task #18 finding #5). A
                // parked Original (rk != dk) is held, never presented.
                //
                // Quality-monotonic (#106.7 §6): while a resize is holding this photo's full-res
                // `Original` on screen (`resize_hold`), do NOT present the settle re-decode's
                // *preview* — it would flash the low-res EXIF frame over the sharp held one. The
                // preview still lands in the ring (upgraded in place when its full arrives, which
                // clears the hold above and presents sharp); a preview for any *other* item, or the
                // normal preview-first first frame of a fresh photo, is unaffected.
                let hold_preview = img.is_preview && self.resize_hold == Some(item);
                if rk == dk && self.target_item == Some(item) && !self.target_caught_up() {
                    if hold_preview {
                        // Keep the sharp held Original on screen instead of flashing this
                        // preview — but the photo IS on screen (via the renderer's `held`), so it
                        // must be marked resolved at the new epoch. Otherwise `target_pending`
                        // stays true forever and the loading pie spins even though nothing is
                        // loading (owner-reported "spinner that never clears"). No `present_item`:
                        // the display stays the held Original; the full Fit upgrades it in place
                        // when it lands (clearing `resize_hold`), a rebind at the same epoch.
                        self.mark_resolved(item);
                    } else {
                        self.present_item(item, res.slot);
                    }
                }
            } else if rk != dk && sharp_diag() {
                // #122: the ring refused a parked-tier reservation (rank beyond the
                // window, or budget with nothing lower-priority evictable). The ring
                // latched the refusal (`ResidentRing::denied`), so the emitter stops
                // re-requesting it until a rebuild/nav re-ranks — without the latch this
                // exact decode repeated every prefetch pass (the owner's 30×-item-7 log).
                eprintln!(
                    "[sharp-diag] ring refused parked {rk:?} item={item} ({item_bytes} B) — denied until rebuild/nav"
                );
            }
            // reserve == None (no longer wanted): the thumb store still gets the
            // pixels we paid to decode (the blaze-past case IS the behind-strip).
            self.thumbs_capture(outcome);
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
                if sharp_diag() && img.is_preview {
                    // The first frame is a PREVIEW shown via the single-image path (outside the
                    // ring), so `display_slot` is None until the async prefetch re-decodes it into
                    // the ring — `sharpen_now` can't fire meanwhile. If it never migrates, it's stuck.
                    eprintln!(
                        "[sharp-diag] preview shown (single-image, load_current_sync) item={idx} {}x{}",
                        img.width, img.height
                    );
                }
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
                // Resolved at the current epoch (this sync path still serves the kept
                // edit/restore exceptions), so readiness reads caught-up with no pie.
                self.mark_resolved(idx);
            }
            Err(e) => {
                eprintln!("decode failed: {}: {e}", self.source.name(idx));
                self.retry_fail(idx);
                self.failed.insert(idx);
                // Keep the gate unstuck (count the bad file as "shown") and clear
                // the stale frame's title/panel so they don't misreport it.
                self.present_failed(idx);
            }
        }
        self.last_present = Some(self.now);
        self.draw();
    }

    /// Bump the geometry epoch and rebuild the ring **retaining resident Originals** (item-6):
    /// a resize / Fit↔1:1 toggle only invalidates `Fit` textures (sized for the old viewport) —
    /// the full-res Originals are geometry-independent, so they compact into the new ring
    /// (`drop_fit_slots` + `compact_to`) and the renderer relocates their textures
    /// (`remap_ring`) instead of dropping them. This is what lets the settle re-present the
    /// current photo's Original instantly and lets advance-after-toggle find a neighbour's
    /// Original resident instead of re-decoding its preview. In-flight FIT decodes for the
    /// old geometry are discarded (Geometry validity); in-flight Originals/thumbs/selections
    /// survive the epoch change and land normally (#119 — killing them was the
    /// fullscreen-toggle blur storm).
    pub fn invalidate_geometry(&mut self) {
        self.rebuild_ring(true);
    }

    /// A **content** change (as opposed to a bare geometry change): the pixels behind one or
    /// more indices changed — a deck rebuild (indices reassigned), source replacement, a saved
    /// EXIF rotation, delete/undo, or teardown. Bumps `content_gen` and **fully resets the
    /// ring** — retention is geometry-only. INVARIANT (item-6 spec §4.1): this must never call
    /// the retaining `invalidate_geometry`, or a stale Original crosses the content change and
    /// index N shows another deck's pixels. Use this — not bare `invalidate_geometry` —
    /// anywhere index `N` may now name different pixels.
    pub fn invalidate_content(&mut self) {
        self.content_gen = self.content_gen.wrapping_add(1);
        // Poster selections are content-scoped (task #114): every choice and
        // in-flight walk from the old generation is wiped here (the pool's
        // level-triggered emission stops re-asking, which cancels the walks).
        self.poster_sel.reset(self.content_gen);
        self.retry.reset();
        self.rebuild_ring(false);
        // #119: a content boundary explicitly quiesces the pool (an empty want-set
        // cancels every queued/in-flight job in both validity domains). Content jobs
        // no longer die by the epoch coincidence, and paths that never re-prefetch
        // (`enter_empty_state`) must not leave old-deck decodes running.
        self.pool
            .set_targets(self.epoch, self.content_gen, &self.source, &[]);
        // ...and advances the thumb derive fence (single owner — `clear_deck`'s own
        // bump on deck rebuilds is redundant-but-harmless): an in-flight derive from
        // pre-change pixels is rejected on landing; the strip re-derives. View/follow
        // state survives — a save-rotation must not yank the strip.
        self.thumbs.invalidate_content();
        // #123 fix 2: stash identities are content-scoped — index N may name different
        // pixels now, so both stash sides die with the deck.
        self.clear_fit_stash();
    }

    /// The shared geometry-rebuild tail: bump the epoch, size the new ring, then either RETAIN
    /// (geometry change: keep + compact resident Originals, remap their GPU textures) or PURGE
    /// (content change: everything goes). The retain keep-list is the parked full-res window
    /// (current → compare-pin → neighbours) — the same priority order that decoded them.
    fn rebuild_ring(&mut self, retain: bool) {
        self.epoch = self.epoch.wrapping_add(1);
        // #124: the representation we were bound to may not survive the rebuild (a retaining
        // pass drops every Fit slot; a content pass resets outright). A stale `presented_kind`
        // would make the background-rebind guards lie about what is actually on screen, so
        // clear it here — the next present re-establishes it.
        self.presented_kind = None;
        let fit = self.fit.unwrap_or(FitBox {
            max_width: 1,
            max_height: 1,
        });
        let cap = ring_capacity(self.slot_bytes_estimate());
        if retain {
            self.ring.drop_fit_slots();
            let keep = self.full_res_window(self.full_res_radius());
            let remaps = self.ring.compact_to(cap, &keep);
            if let Some(a) = self.renderer.as_mut() {
                a.remap_ring(cap, &remaps);
            }
        } else {
            self.ring = ResidentRing::new_with_budget(cap, RING_BUDGET_BYTES);
            if let Some(a) = self.renderer.as_mut() {
                a.reserve_ring(cap, fit.max_width, fit.max_height);
            }
        }
        let (ahead, behind) = window_for_capacity(cap);
        self.ahead = ahead;
        self.behind = behind;
        // Staged-outcome retention by validity domain (#119): a geometry rebuild keeps
        // content-valid staged work (an already-paid-for Original/thumb/selection
        // artifact survives a toggle); geometry-bound Fits drop. A content rebuild's
        // generation mismatch clears everything through the same predicate. Dropped
        // outcomes free their pool budget as they drop.
        let (epoch, content_gen) = (self.epoch, self.content_gen);
        self.pending_uploads
            .retain(|o| !outcome_stale(epoch, content_gen, o));
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
    /// while open, which is fine: the app isn't blazing through photos with a dialog up.
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
                        format!("{} (decode error)", crate::APP_NAME),
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
                    crate::APP_NAME.to_string(),
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
        if let Some(details) = self.exif_cache.get(&item) {
            // A video's container probe runs on a worker (task #98). Until it lands the
            // panel says so, rather than showing a table that looks complete but isn't;
            // `poll_details_probe` re-signals the Inspector when the result arrives.
            if details.probe_state == crate::media_details::ProbeState::Loading {
                rows.push(DetailRow::Span {
                    text: "Reading video details…".to_string(),
                    bold: false,
                });
                return rows;
            }
            rows.push(DetailRow::Pair {
                label: "File Size".to_string(),
                value: format!("{} bytes", hud::format_thousands(details.size)),
            });
            for (tag, val) in &details.fields {
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
            // The video's audio + subtitle tracks (task #98), under the basic facts.
            // Completeness-driven, so a probe that failed reads as "details
            // unavailable", never as "No audio".
            if let Some(catalog) = &details.media {
                rows.extend(crate::tracks::track_rows(catalog, details.has_audio));
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
        // A video's encoded bytes never enter RAM here (only playback fetches them):
        // the panel's file size comes from a stat (or the archive directory's size
        // hint for an entry), and its facts (duration/codec/fps/audio) from a
        // reader-metadata probe — container headers only, ~15-25 ms once, cached for
        // the item's lifetime (comparable to the sync fs::read the image path below
        // already does here). An archive entry skips the probe: it would inflate the
        // whole entry on the event loop; playback's `Opened` carries duration anyway.
        // Exhaustive on purpose: the `Image` arm reads the item's **entire** encoded
        // bytes synchronously, on the event loop. That is only a bounded cost for a
        // photo, so every kind must state its own answer here rather than inherit the
        // read by falling past a video-shaped `if let`.
        match crate::video::item_kind(self.source.as_ref(), item) {
            crate::video::LibraryItemKind::Video(_) => {
                // Off to a worker: opening a container is an unbounded wait (damaged file,
                // network share, a codec the OS reader labours over), and the event loop must
                // never take it. Record `Loading` — which the panel shows honestly, and which
                // also stops a second worker being spawned for this item — and let `tick`
                // pick the result up.
                self.exif_cache
                    .insert(item, crate::app_core::ItemDetails::loading());
                // Two generations, deliberately: the deck's (has index `item` been reassigned
                // under us?) and a fresh one for this catalog alone (which file's tracks are
                // these?). See `AppCore::catalog_seq` — handing the deck's to both is what let a
                // picked track resolve against the next film's catalog.
                self.catalog_seq += 1;
                self.details_probe = Some(crate::media_details::spawn(
                    &self.source,
                    item,
                    self.details_gen,
                    self.catalog_seq,
                    self.source.name(item).to_string(),
                ));
            }
            // A door's facts are its size and its format — both free. The size comes
            // from a **stat**, never a read (`media_details::probe_job` uses the same
            // rule); reading a 2 GB archive here, on the event loop, just to fill a
            // panel is exactly what the door exists to avoid. No EXIF, no probe: what
            // is inside is unknown until the viewer enters it, and saying so honestly
            // beats opening it to find out.
            crate::video::LibraryItemKind::Archive(kind) => {
                let size = self
                    .source
                    .path(item)
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map(|m| m.len())
                    .or_else(|| self.source.size_hint(item))
                    .unwrap_or(0);
                self.exif_cache.insert(
                    item,
                    crate::app_core::ItemDetails::ready(
                        size,
                        vec![("Format".to_string(), format!("{} archive", kind.name()))],
                    ),
                );
            }
            crate::video::LibraryItemKind::Image => {
                if let Ok(bytes) = self.source.bytes(item) {
                    let fields = read_exif_fields(&bytes);
                    self.exif_cache.insert(
                        item,
                        crate::app_core::ItemDetails::ready(bytes.len() as u64, fields),
                    );
                }
            }
        }
    }

    /// Pick up a finished Details probe (called each tick).
    ///
    /// Accepts the result only if the deck generation **and** the item's identity still
    /// match what was requested: a rebuild reassigns indices, so an older result names a
    /// different file and is dropped rather than cached against the wrong photo. A dead
    /// worker marks the entry `Failed` — otherwise its `Loading` placeholder would sit on
    /// "Reading…" forever, and never re-probe (the placeholder is also the spawn guard).
    pub fn poll_details_probe(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let outcome = {
            let Some(p) = self.details_probe.as_ref() else {
                return;
            };
            match p.rx.try_recv() {
                Ok(details) => Some((
                    p.gen,
                    p.item,
                    p.identity.clone(),
                    p.copy_when_done,
                    Some(details),
                )),
                Err(TryRecvError::Empty) => return, // still probing
                Err(TryRecvError::Disconnected) => {
                    Some((p.gen, p.item, p.identity.clone(), p.copy_when_done, None))
                }
            }
        };
        self.details_probe = None;
        let Some((gen, item, identity, copy, details)) = outcome else {
            return;
        };
        if gen != self.details_gen {
            return; // deck rebuilt while probing — the indices were reassigned
        }
        if self.source.name(item) != identity {
            return; // same index, different file — not our result
        }
        match details {
            Some(d) => {
                self.exif_cache.insert(item, d);
            }
            None => {
                // The worker died. Keep the entry (so we don't respawn in a loop) but say so.
                if let Some(e) = self.exif_cache.get_mut(&item) {
                    e.probe_state = crate::media_details::ProbeState::Failed;
                }
            }
        }
        // The open Inspector may be sitting on this item's "Reading…" row.
        self.emit_panels_changed();
        // The probe may have landed mid-playback for the very video it describes —
        // the DoVi warning's second chance (the first is at session start).
        self.maybe_warn_dovi(item);
        if self.slot_content() == Some(SlotContent::Details) && self.displayed_item == Some(item) {
            self.show_overlay();
        }
        // The cache is warm now, so this re-entry takes the normal path (it cannot loop).
        if copy && self.displayed_item == Some(item) {
            self.copy_image_details();
        }
    }

    /// The old synchronous body, kept only for tests that need a probed entry without a
    /// tick loop. Never call this from the event loop — that is what
    /// [`Self::poll_details_probe`] exists to prevent.
    #[cfg(test)]
    pub fn probe_details_blocking(&mut self, item: usize) {
        self.catalog_seq += 1;
        let d = crate::media_details::probe_job(self.source.as_ref(), item, self.catalog_seq);
        self.exif_cache.insert(item, d);
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
    /// animation (play ▶). An archive door has **no pill** — its affordance is the door card
    /// (task #105), which is the only thing on screen for it. Stays consistent with
    /// `has_motion` (which bumps `play_hint_seq`): a fresh motion item is a Live Photo (→1) or
    /// has an `animated` container (→2).
    pub fn play_hint_kind(&self) -> u8 {
        if self.playback.is_some() {
            return 0; // engaged — no hint while it plays/pauses
        }
        let Some(item) = self.displayed_item else {
            return 0;
        };
        if self.is_live_photo(item) {
            1
        } else if self.current.as_ref().is_some_and(|m| m.animated.is_some())
            || self.item_is_video(item)
        {
            2
        } else {
            0
        }
    }

    /// Whether item `item` is a video (task #79) — typed off the path, no I/O.
    pub fn item_is_video(&self, item: usize) -> bool {
        matches!(
            crate::video::item_kind(self.source.as_ref(), item),
            crate::video::LibraryItemKind::Video(_)
        )
    }

    /// Whether an archive **door** is on screen right now — the cheap predicate the
    /// shells poll each frame to gate their overlay and spot a change.
    ///
    /// Allocation-free, unlike [`door_card`](Self::door_card), which builds Strings: a
    /// per-frame visibility gate must not allocate.
    pub fn door_presented(&self) -> bool {
        // Gate on the frame being **actually on screen** at the current epoch, not merely named:
        // `rebuild_playlist` sets `displayed_item` to the new current index with
        // `presented_epoch = None` (nothing presented yet — the renderer still holds the old
        // frame). Without this check the door card would flash over that held photo the instant a
        // door becomes the current item, before its own (transparent) frame is presented — the
        // owner-reported "card on top of a photo" (and the archive-open card-with-no-image).
        self.presented_epoch == Some(self.epoch)
            && self
                .displayed_item
                .is_some_and(|i| self.item_archive_kind(i).is_some())
    }

    /// The **door card** to draw over the letterbox, or `None` when the presented item
    /// isn't a door (task #105).
    ///
    /// A door's frame is a 1×1 transparent sentinel — it draws nothing — so this card is
    /// the entire on-screen presence of an archive: its artwork, its name, and the key
    /// that opens it. The shells snapshot it into their panel frame and render it as
    /// chrome, which is what a door is.
    ///
    /// Keyed off `displayed_item` — the item **actually on screen** — never the playlist
    /// cursor, or the card would name an archive the viewer isn't looking at yet. Pure:
    /// no I/O, safe on the frame path.
    pub fn door_card(&self) -> Option<crate::app_core::DoorCard> {
        // Only once the door's own frame is actually presented (see `door_presented`) — never over
        // a still-held previous photo during a deck rebuild.
        if !self.door_presented() {
            return None;
        }
        let item = self.displayed_item?;
        let kind = self.item_archive_kind(item)?;
        Some(crate::app_core::DoorCard {
            name: self
                .source
                .path(item)
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.source.name(item).to_string()),
            format: format!("{} Archive", kind.name()),
            shortcut: self.shortcut_for(Action::PlayPause),
        })
    }

    /// The format of item `item` if it is an archive **door** (task #104), else
    /// `None` — typed off the path, no I/O. A door is an archive sitting on disk
    /// that the viewer can enter with `P`; an archive *entry* is never one, so
    /// this answers `None` inside an open archive.
    pub fn item_archive_kind(&self, item: usize) -> Option<pb_source::ArchiveKind> {
        match crate::video::item_kind(self.source.as_ref(), item) {
            crate::video::LibraryItemKind::Archive(kind) => Some(kind),
            crate::video::LibraryItemKind::Image | crate::video::LibraryItemKind::Video(_) => None,
        }
    }

    /// The active video's Windows/Linux [`VideoSession`] bundle, if the backend is
    /// `Session` (`None` on macOS, where playback is the shell's native `AVPlayer`
    /// and there is no session to drive). The producer-driving methods below funnel
    /// through these so they naturally no-op on the `Native` backend.
    fn session_ref(&self) -> Option<&crate::video_session::ActiveVideo> {
        self.video.as_ref().and_then(ActiveVideoBackend::as_session)
    }
    fn session_mut(&mut self) -> Option<&mut crate::video_session::ActiveVideo> {
        self.video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
    }

    /// `P` on a video item (task #79 phase 4): toggle the streaming session —
    /// pause/resume while it runs, start (or restart after end/failure) otherwise.
    /// On macOS (the `Native` backend) the session-op arms are inert — pause/
    /// resume/replay of a live native player are wired with input parity (79.9
    /// phase 3) via the `PauseVideo`/`ResumeVideo`/`SeekVideoFraction` commands;
    /// the "start fresh" default still opens playback through `start_video_session`.
    pub fn video_play_pause(&mut self, item: usize) {
        use crate::video::VideoSessionState::*;
        let existing = self.video.as_ref().map(|v| (v.item(), v.state()));
        match existing {
            Some((playing_item, state)) if playing_item == item => match state {
                Playing => self.video_pause_current(),
                Paused => self.video_resume_current(),
                Ended => self.video_replay_current(),
                Failed | Stopped => self.start_video_session(item),
                // Starting up (or mid-seek later): let it be.
                Opening | Buffering | Seeking => {}
            },
            // A different item's session (stale) or none: start fresh.
            _ => self.start_video_session(item),
        }
    }

    /// Pause the current video — the Session path pauses the `VideoSession` (+ its
    /// audio player); the Native (macOS) path commands the shell's `AVPlayer`. Each
    /// arm returns the effect to emit so the `self.video` borrow is released before
    /// `self.effects`/`self.draw` (disjoint-field borrows aside, this stays clean).
    fn video_pause_current(&mut self) {
        let now = self.now;
        let cmd = match self.video.as_mut() {
            Some(ActiveVideoBackend::Session(v)) => {
                v.session.pause(now);
                Some(contract::CoreEffect::PauseVideoAudio)
            }
            Some(ActiveVideoBackend::Native(p)) => Some(contract::CoreEffect::PauseVideo {
                session_id: p.session_id,
            }),
            None => None,
        };
        if let Some(cmd) = cmd {
            self.effects.push(cmd);
            self.draw();
        }
    }

    /// Resume the current video (from `Paused`).
    fn video_resume_current(&mut self) {
        let now = self.now;
        let mut flush: Option<Duration> = None;
        let cmd = match self.video.as_mut() {
            Some(ActiveVideoBackend::Session(v)) => {
                let was_paused = v.session.state() == crate::video::VideoSessionState::Paused;
                v.session.resume(now);
                if was_paused {
                    // Flush a landed-but-uncommitted seek BEFORE the resume, so
                    // audio rejoins at the seeked position, never the stale one.
                    flush = v.pending_audio_commit.take();
                    v.scrub_audio_paused = false;
                    v.last_seek_intent = None;
                }
                Some(contract::CoreEffect::ResumeVideoAudio)
            }
            Some(ActiveVideoBackend::Native(p)) => Some(contract::CoreEffect::ResumeVideo {
                session_id: p.session_id,
            }),
            None => None,
        };
        if let Some(position) = flush {
            self.effects
                .push(contract::CoreEffect::SeekVideoAudio { position });
        }
        if let Some(cmd) = cmd {
            self.effects.push(cmd);
            self.draw();
        }
    }

    /// Replay from the top (`P` at `Ended`). Session: a seek to 0 on the SAME session
    /// (the producer parks after EOS for this). Native: `ResumeVideo`, which the shell
    /// resolves as seek-to-0-then-play when the player is parked at the end.
    fn video_replay_current(&mut self) {
        let now = self.now;
        enum Replay {
            Session,
            Native(contract::CoreEffect),
        }
        let action = match self.video.as_mut() {
            Some(ActiveVideoBackend::Session(v)) => v.session.replay(now).map(|_| Replay::Session),
            Some(ActiveVideoBackend::Native(p)) => {
                Some(Replay::Native(contract::CoreEffect::ResumeVideo {
                    session_id: p.session_id,
                }))
            }
            None => None,
        };
        match action {
            Some(Replay::Session) => {
                // 1D: audio pauses now; the landing at 0 commits the audio seek
                // and the resume follows it (in order), via the coordinator.
                self.note_video_seek_intent();
                self.draw();
            }
            Some(Replay::Native(cmd)) => {
                self.effects.push(cmd);
                self.draw();
            }
            None => {}
        }
    }

    // Shell → core callbacks for the macOS native player (task 79.9 phase 2). The
    // shell's `AVPlayer` is the timing/lifecycle authority; these advance the passive
    // `NativeVideoProxy` so the core's play/pause/replay dispatch + policy see real
    // state. Each is session-gated inside the proxy (a stale player is ignored).

    /// The session id of the active macOS native video (`0` = none). The shell reconciles
    /// its `AVPlayer` against this each pump: if it holds a player the core no longer has
    /// (a torn-down/replaced session), it tears that player down — a belt-and-suspenders
    /// against a missed `StopVideo` leaving a second video playing.
    pub fn native_video_session_id(&self) -> u64 {
        self.video
            .as_ref()
            .and_then(ActiveVideoBackend::as_native)
            .map(|p| p.session_id.0)
            .unwrap_or(0)
    }

    /// The player finished opening: record duration + audio presence.
    pub fn native_video_opened(&mut self, session_id: u64, duration_ms: i64, has_audio: bool) {
        let sid = crate::video::VideoSessionId(session_id);
        let duration = (duration_ms >= 0).then(|| Duration::from_millis(duration_ms as u64));
        if let Some(p) = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
        {
            p.on_opened(sid, duration, has_audio);
        }
        self.update_video_progress();
    }

    /// The player's playback state changed (`state`: 0 Opening / 1 Buffering / 2 Playing
    /// / 3 Paused — `Ended`/`Failed` have their own callbacks).
    pub fn native_video_state_changed(&mut self, session_id: u64, state: u8) {
        let sid = crate::video::VideoSessionId(session_id);
        let st = match state {
            1 => crate::video::VideoSessionState::Buffering,
            2 => crate::video::VideoSessionState::Playing,
            3 => crate::video::VideoSessionState::Paused,
            _ => crate::video::VideoSessionState::Opening,
        };
        let changed = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
            .is_some_and(|p| p.on_state_changed(sid, st));
        if changed {
            self.update_video_progress();
            self.draw();
        }
    }

    /// The player reached end-of-stream (parks the last frame; `P` replays).
    pub fn native_video_ended(&mut self, session_id: u64) {
        let sid = crate::video::VideoSessionId(session_id);
        let applied = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
            .is_some_and(|p| p.on_ended(sid));
        if applied {
            self.update_video_progress();
            self.draw();
        }
    }

    /// A seek acknowledged (`finished` = landed cleanly; `false` = superseded by a
    /// newer seek). Clears the proxy's in-flight flag for the current generation.
    pub fn native_video_seek_completed(
        &mut self,
        session_id: u64,
        generation: u64,
        finished: bool,
    ) {
        let sid = crate::video::VideoSessionId(session_id);
        let gen = crate::video::SeekGeneration(generation);
        if let Some(p) = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
        {
            p.on_seek_completed(sid, gen, finished);
        }
    }

    /// The player failed. `recoverable` is the shell's error classification
    /// (task #84 §8a level 2): `true` for demux/codec-shaped failures where the
    /// FFmpeg fallback is worth attempting; `false` for missing-file /
    /// permission / DRM / network errors, which no other backend can fix.
    ///
    /// With `ffvideo` built in, a recoverable failure on the displayed item
    /// **retries through the FFmpeg session before any error surfaces** — the
    /// user sees exactly one final error only if both backends fail (the
    /// session's own failure path owns that toast). Otherwise: surface the
    /// error and return to the poster, mirroring the Session `poll_video`
    /// failure path.
    pub fn native_video_failed(&mut self, session_id: u64, error: String, recoverable: bool) {
        let sid = crate::video::VideoSessionId(session_id);
        let failed_item = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
            .and_then(|p| p.on_failed(sid, error.clone()).then_some(p.item));
        let Some(item) = failed_item else { return };
        // Stop + detach the failed AVPlayer either way.
        self.effects
            .push(contract::CoreEffect::StopVideo { session_id: sid });
        self.video = None;
        self.update_video_progress();
        #[cfg(all(target_os = "macos", feature = "ffvideo"))]
        if recoverable && Some(item) == self.displayed_item {
            // §8a fallback: no toast before the FFmpeg attempt; the flag makes
            // the very next start route to the session, consumed there.
            self.video_ffmpeg_fallback = Some(item);
            self.start_video_session(item);
            return;
        }
        #[cfg(not(all(target_os = "macos", feature = "ffvideo")))]
        let _ = (recoverable, item);
        let msg = if error.is_empty() {
            "Video playback failed".to_string()
        } else {
            error
        };
        self.show_toast(&msg);
    }

    /// Start video playback of `item` (task #79 phase 4 / task #84 §8a): a fresh
    /// `VideoSession` fed by a dedicated reader thread — Media Foundation on
    /// Windows, the FFmpeg producer on Linux and on macOS for everything
    /// `AVPlayer` doesn't handle (MKV/WebM included; smoothness plan) — or, on
    /// macOS for nominally-native containers, the shell's `AVPlayer`. The
    /// producer thread is never joined — teardown is a Stop message / channel
    /// disconnect.
    // The early return is load-bearing when the session block below compiles in
    // (macOS + ffvideo); without the feature it's the trailing statement and
    // clippy calls it needless — allow rather than fork the body per cfg.
    #[allow(clippy::needless_return)]
    pub fn start_video_session(&mut self, item: usize) {
        self.stop_video();
        // macOS routing (§8a; smoothness plan): AVPlayer for what it handles well
        // (MP4/MOV); the Session route (FFmpeg → wgpu → Metal) for everything else,
        // including MKV/WebM — it presents smoothly where the sample-buffer
        // presenter drops frames. The presenter is parked opt-in
        // (`sample_buffer_opt_in`, the DoVi reference renderer) and still level-2
        // falls back to Session on a classified failure.
        #[cfg(target_os = "macos")]
        {
            // A classified native/sample-buffer failure forces the Session route
            // exactly once (level 2): consume the flag here so neither Apple route
            // is retried for this same item.
            #[cfg(feature = "ffvideo")]
            let forced_session = self.video_ffmpeg_fallback.take() == Some(item);
            #[cfg(not(feature = "ffvideo"))]
            let forced_session = false;
            if !forced_session {
                if self.macos_native_route(item) {
                    self.start_native_video(item);
                    return;
                }
                #[cfg(feature = "ffvideo")]
                if self.macos_sample_buffer_route(item) {
                    self.start_sample_buffer_video(item);
                    return;
                }
            }
        }
        // Session platforms: Windows (MF), Linux (FFmpeg), and the macOS FFmpeg
        // route above falling through (task #84 §8a).
        #[cfg(any(windows, all(unix, feature = "ffvideo")))]
        {
            let fit = self.decode_fit();
            // Credit-granting estimate of one fitted RGBA8 frame. The fit box is a
            // conservative bound (aspect makes real frames smaller); no fit
            // (Fill/Original modes) assumes 4K.
            let frame_bytes = fit
                .map(|f| f.max_width as u64 * f.max_height as u64 * 4)
                .unwrap_or(3840 * 2160 * 4);
            self.video_seq += 1;
            let id = crate::video::VideoSessionId(self.video_seq);
            let (session, io) = crate::video_session::VideoSession::new(id, frame_bytes);
            let generation = crate::video::SeekGeneration::FIRST;
            // Planar GPU color path (task #91 Phase 2): the producer emits NV12/P010
            // for eligible clips when the renderer supports it and the escape hatch
            // isn't set. Captured here (off the producer thread) from the renderer's
            // real device capability.
            let planar_opts = self.planar_video_options();
            // The media slot `poll_video`'s audio start reads; the producer thread
            // shares the same Arc, so both pipelines read ONE copy of the container.
            let media: std::sync::Arc<std::sync::OnceLock<crate::video::VideoInput>> =
                std::sync::Arc::new(std::sync::OnceLock::new());
            if let Some(path) = self.source.path(item).map(Path::to_path_buf) {
                let input = crate::video::VideoInput::Path(path);
                let _ = media.set(input.clone());
                std::thread::spawn(move || {
                    run_platform_video_producer(
                        &input,
                        fit,
                        id,
                        generation,
                        io.events,
                        io.msgs,
                        io.cancel,
                        planar_opts,
                    );
                });
            } else {
                // Archive entry: fetch the container bytes OFF the event loop (a
                // ZIP entry inflates; a 7z copies from its resident RAM), publish
                // them through the media slot — before the producer can report
                // `Opened`, so the audio start always finds them — then run the
                // same producer through the bytes seam. RAM-only end to end:
                // nothing is ever extracted to disk (privacy #2).
                let source = self.source.clone();
                let name = self.source.name(item).to_string();
                let media_slot = media.clone();
                std::thread::spawn(move || {
                    let data = match source.bytes(item) {
                        Ok(data) => std::sync::Arc::new(data),
                        Err(e) => {
                            let _ = io.events.send(crate::video::VideoProducerEvent::Failed {
                                session_id: id,
                                error: format!("couldn't read the video from the archive: {e}"),
                            });
                            return;
                        }
                    };
                    let input = crate::video::VideoInput::Bytes { data, name };
                    let _ = media_slot.set(input.clone());
                    run_platform_video_producer(
                        &input,
                        fit,
                        id,
                        generation,
                        io.events,
                        io.msgs,
                        io.cancel,
                        planar_opts,
                    );
                });
            }
            // A remembered position for this item (task #94.2) → resume there once
            // the session can seek (`poll_video`), held from a stale re-scan clear.
            let resume_to = self.video_resume.get(&item).copied();
            self.video = Some(ActiveVideoBackend::Session(
                crate::video_session::ActiveVideo {
                    session,
                    item,
                    audio_started: false,
                    media,
                    scrub_audio_paused: false,
                    pending_audio_commit: None,
                    last_seek_intent: None,
                    dbg_seek_land_at: None,
                    resume_to,
                },
            ));
            self.anim_hint_shown_for = Some(item); // engaged — retire the hint
                                                   // Honest DoVi UX (macos-video-smoothness §2): warm the container probe
                                                   // (async, never blocks) and warn now if it already landed; otherwise
                                                   // `poll_details_probe` warns when it does. Also warms the track
                                                   // pickers, which read the same catalog.
            self.ensure_exif_cached(item);
            self.maybe_warn_dovi(item);
            self.draw();
        }
        #[cfg(not(any(windows, target_os = "macos", all(unix, feature = "ffvideo"))))]
        {
            let _ = item;
            self.show_toast("Video playback is not available yet on this platform");
        }
    }

    /// One-time honest-UX warning (macos-video-smoothness §2): the item playing on
    /// the **Session route** carries a Dolby Vision stream whose base layer cannot
    /// show correct color without RPU reshaping (Profile 5 / compat-id 0 — the
    /// green/purple tint). AVPlayer and the opted-in sample-buffer presenter decode
    /// DoVi natively, so only a Session backend warns. Called from
    /// `start_video_session` (probe already cached) and `poll_details_probe` (probe
    /// landing mid-playback); `dovi_warned` makes it once per item.
    fn maybe_warn_dovi(&mut self, item: usize) {
        let session_here = self
            .video
            .as_ref()
            .and_then(|v| v.as_session())
            .is_some_and(|s| s.item == item);
        let incompatible = self
            .exif_cache
            .get(&item)
            .is_some_and(|d| d.dovi_incompatible);
        if session_here && incompatible && self.dovi_warned.insert(item) {
            self.show_toast("Dolby Vision (Profile 5) — colors can't be shown correctly");
        }
    }

    /// macOS §8a routing: `true` = try the shell's `AVPlayer`. Known-unsupported
    /// containers (MKV/WebM/WMV/MPEG-PS/AVCHD) route to the FFmpeg session
    /// (level 1), and a just-failed classified native attempt forces the
    /// session exactly once (level 2 — the flag is consumed here, so a later
    /// fresh open retries native first). Without `ffvideo` there is no FFmpeg
    /// backend and everything stays native (the failure toast is the outcome).
    #[cfg(target_os = "macos")]
    fn macos_native_route(&mut self, item: usize) -> bool {
        // The level-2 fallback flag is consumed by the caller (start_video_session)
        // so it can skip both Apple routes at once; this is now a pure container test.
        #[cfg(not(feature = "ffvideo"))]
        {
            let _ = item;
            true
        }
        #[cfg(feature = "ffvideo")]
        {
            match crate::video::item_kind(self.source.as_ref(), item) {
                crate::video::LibraryItemKind::Video(c) => c.macos_native(),
                // Not a video (unreachable from the play paths) — native no-op.
                // A door reaches `P` but enters an archive rather than playing,
                // so it never gets here either.
                crate::video::LibraryItemKind::Image
                | crate::video::LibraryItemKind::Archive(_) => true,
            }
        }
    }

    /// `true` = use the macOS **sample-buffer presenter** (FFmpeg demux →
    /// `AVSampleBufferDisplayLayer`) for this item. **Default OFF** — the presenter
    /// drops ~3 frames/sec on steady-state playback that both `AVPlayer` and the
    /// Session route play flawlessly (measured; see
    /// `.taskmaster/plans/macos-video-smoothness.md`), so MKV/WebM route to the
    /// Session route (FFmpeg → wgpu → Metal). The presenter is **parked, not
    /// deleted**: it is the on-device Dolby-Vision reference renderer, opt-in via
    /// `PB_SAMPLE_BUFFER=1` ([`AppCore::sample_buffer_opt_in`], read once at host
    /// construction — tests set the field directly). When opted in it keeps the old
    /// restrictions: loose-file **MKV/WebM** only, self-probing the codec and
    /// falling back to Session (level 2) for anything it can't sample-decode.
    /// Reached only for non-native containers (`macos_native_route` runs first).
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    fn macos_sample_buffer_route(&self, item: usize) -> bool {
        if !self.sample_buffer_opt_in || self.source.path(item).is_none() {
            return false;
        }
        match crate::video::item_kind(self.source.as_ref(), item) {
            crate::video::LibraryItemKind::Video(c) => matches!(
                c,
                crate::video::VideoContainer::Mkv | crate::video::VideoContainer::Webm
            ),
            // Neither is a video, so neither uses the presenter.
            crate::video::LibraryItemKind::Image | crate::video::LibraryItemKind::Archive(_) => {
                false
            }
        }
    }

    /// Start the macOS sample-buffer presenter for `item` (Phase 3). Mirrors
    /// [`Self::start_native_video`]: the core keeps only a passive `Native` proxy
    /// (the presenter fires the same `native_video_*` callbacks), and the demux
    /// container input is carried on the effect for the host to stash + open off
    /// the main actor. Loose-file only for now; an archive item (no file URL)
    /// falls back to the Session route.
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    fn start_sample_buffer_video(&mut self, item: usize) {
        let Some(path) = self.source.path(item).map(Path::to_path_buf) else {
            // No file URL (archive) — not yet supported on this route. Force the
            // Session route (the flag is consumed there, so no re-entry here).
            self.video_ffmpeg_fallback = Some(item);
            self.start_video_session(item);
            return;
        };
        self.video_seq += 1;
        let id = crate::video::VideoSessionId(self.video_seq);
        let muted = self.effective_mute();
        let start_secs = self
            .video_resume
            .get(&item)
            .map_or(0.0, |d| d.as_secs_f64());
        self.video = Some(ActiveVideoBackend::Native(
            crate::video_native::NativeVideoProxy::new(item, id, muted),
        ));
        self.effects.push(contract::CoreEffect::PlaySampleBuffer {
            input: crate::video::VideoInput::Path(path),
            session_id: id,
            muted,
            start_secs,
        });
        self.anim_hint_shown_for = Some(item); // engaged — retire the hint
        self.draw();
    }

    /// macOS native playback (task 79.9): the shell's `AVPlayer` owns the whole
    /// pipeline. The core keeps only a passive `Native` proxy and commands the
    /// player via `PlayVideo`; the shell reveals the layer on the first frame and
    /// reports state back through the `native_video_*` callbacks (79.9 phase 2).
    #[cfg(target_os = "macos")]
    fn start_native_video(&mut self, item: usize) {
        self.video_seq += 1;
        let id = crate::video::VideoSessionId(self.video_seq);
        let muted = self.effective_mute();
        // A remembered position for this item (task #94.2) → the shell seeks the
        // player here before revealing/playing. `0.0` = from the start.
        let start_secs = self
            .video_resume
            .get(&item)
            .map_or(0.0, |d| d.as_secs_f64());
        if let Some(path) = self.source.path(item).map(Path::to_path_buf) {
            self.video = Some(ActiveVideoBackend::Native(
                crate::video_native::NativeVideoProxy::new(item, id, muted),
            ));
            self.effects.push(contract::CoreEffect::PlayVideo {
                path,
                session_id: id,
                muted,
                start_secs,
            });
        } else {
            // Archive entry: no file URL. Read the container bytes OFF the event loop (a
            // large ZIP inflates; 7z copies from resident RAM), then — once they arrive
            // (drained in the tick) — hand them to the shell, which serves them to
            // `AVPlayer` through a resource loader. RAM-only, never to disk (privacy #2).
            // The proxy is live now so the session is gated; the poster holds until play.
            self.video = Some(ActiveVideoBackend::Native(
                crate::video_native::NativeVideoProxy::new(item, id, muted),
            ));
            let name = self.source.name(item).to_string();
            let source = self.source.clone();
            let tx = self.video_read_tx.clone();
            std::thread::spawn(move || {
                let bytes = source.bytes(item).unwrap_or_default();
                let _ = tx.send((id, name, muted, bytes));
            });
        }
        self.anim_hint_shown_for = Some(item); // engaged — retire the hint
        self.draw();
    }

    /// One seek step on the active video (task #79 phase 6): ±2 s, Shift ±10 s,
    /// relative to the **desired** target so a held key scrubs the intent. Seeks
    /// audio alongside and surfaces the position feedback.
    pub fn video_seek(&mut self, back: bool) {
        let step = if self.mods.shift {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(2)
        };
        // Native backend (macOS): AVPlayer owns the clock, so the core issues only a
        // relative, generation-gated seek *intent*; the shell resolves it against the
        // player and clamps to the seekable range (the proxy holds no position). The
        // live position comes back through the periodic progress observer.
        if let Some(p) = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
        {
            let session_id = p.session_id;
            let generation = p.begin_seek();
            let delta = i64::try_from(step.as_millis()).unwrap_or(i64::MAX);
            self.effects.push(contract::CoreEffect::SeekVideoBy {
                session_id,
                generation,
                delta_ms: if back { -delta } else { delta },
            });
            self.arm_video_line_flash(); // reveal the controls during a keyboard seek
            return;
        }
        let now = self.now;
        let Some(v) = self.session_mut() else {
            return; // no active session backend
        };
        let Some(target) = v.session.seek_by(back, step, now) else {
            return;
        };
        // 1D: audio pauses once per seek run; the ONE audio seek (+ resume)
        // commits in `poll_video` after the run settles — never per step.
        self.note_video_seek_intent();
        self.video_position_feedback(target);
    }

    /// Register a Session-backend seek intent with the 1D audio coordinator:
    /// pause the shell audio player once per run, supersede any landed-but-
    /// uncommitted position, and restart the settle window. `poll_video` emits
    /// the single `SeekVideoAudio` (+ resume if the clip plays on) once no new
    /// intent has arrived for [`VIDEO_SEEK_AUDIO_SETTLE`] — so a held key or a
    /// scrubber drag never stops/seeks/refills the audio decoder per target (R4).
    fn note_video_seek_intent(&mut self) {
        let now = self.now;
        let Some(v) = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
        else {
            return;
        };
        v.pending_audio_commit = None;
        v.last_seek_intent = Some(now);
        let first_of_run = !v.scrub_audio_paused;
        v.scrub_audio_paused = true;
        if first_of_run {
            self.effects.push(contract::CoreEffect::PauseVideoAudio);
        }
    }

    /// Absolute seek from the playback bar (a click/drag on the info line's bar,
    /// task #79 follow-up): `frac` of the clip's duration. Same seek semantics as
    /// the keyboard — playing resumes at the target, paused shows it and stays.
    pub fn video_seek_fraction(&mut self, frac: f32) {
        let now = self.now;
        let Some(v) = self.session_mut() else {
            return; // macOS drives seeks through the native player (79.9 phase 3)
        };
        let Some(d) = v.session.duration else {
            return; // no duration → no bar to click
        };
        let target = Duration::from_secs_f64(d.as_secs_f64() * f64::from(frac.clamp(0.0, 1.0)));
        if v.session.seek_to(target, now, None).is_none() {
            return;
        }
        // 1D: drag spam coalesces exactly like held keys — commit on settle.
        self.note_video_seek_intent();
        self.update_video_progress();
    }

    /// `,`/`.` on a video item (task #79 follow-up): step one frame, pausing
    /// playback first (the same contract as animation frame-step). Forward serves
    /// the next decoded frame straight from the session queue — instant; backward
    /// is a paused one-frame seek (the reader re-runs the GOP, a normal seek
    /// landing). Returns whether an active video consumed the step.
    pub fn video_frame_step(&mut self, delta: i32) -> bool {
        use crate::video::VideoSessionState::*;
        // Native backend (macOS): the shell drives AVPlayerItem.step(byCount:) — it pauses
        // first and no-ops when the item can't step that direction. The proxy's paused state
        // syncs back via the state_changed callback.
        if let Some(p) = self.video.as_ref().and_then(ActiveVideoBackend::as_native) {
            if Some(p.item) != self.displayed_item || matches!(p.state(), Failed | Stopped) {
                return false;
            }
            let session_id = p.session_id;
            self.effects.push(contract::CoreEffect::StepVideo {
                session_id,
                forward: delta > 0,
            });
            self.arm_video_line_flash();
            return true;
        }
        let now = self.now;
        let displayed = self.displayed_item;
        let outcome = {
            // Inline the field borrow (not the `session_mut` helper): `v` must borrow
            // only `self.video` so `self.effects` stays usable alongside it below.
            let Some(v) = self
                .video
                .as_mut()
                .and_then(ActiveVideoBackend::as_session_mut)
            else {
                return false; // macOS: native player frame-step (79.9 phase 3)
            };
            if Some(v.item) != displayed || matches!(v.session.state(), Failed | Stopped) {
                return false;
            }
            // Stepping is scrubbing, not playback: pause first (like animations).
            if v.session.state() == Playing {
                v.session.pause(now);
                self.effects.push(contract::CoreEffect::PauseVideoAudio);
            }
            v.session.step_frame(delta > 0, now)
        };
        match outcome {
            crate::video_session::FrameStep::Present(frame) => {
                let pts = frame.pts;
                self.present_video_frame(&frame);
                // A settled instant step: sync the paused audio player directly
                // (so a later resume doesn't yank the clock back) and supersede
                // any landed-but-uncommitted seek — this step is newer intent.
                if let Some(v) = self
                    .video
                    .as_mut()
                    .and_then(ActiveVideoBackend::as_session_mut)
                {
                    v.pending_audio_commit = None;
                    v.last_seek_intent = None;
                }
                self.effects
                    .push(contract::CoreEffect::SeekVideoAudio { position: pts });
                self.draw();
                self.video_position_feedback(pts);
            }
            crate::video_session::FrameStep::Seeking(target) => {
                // 1D: the landing commits the audio seek at the landed PTS.
                self.note_video_seek_intent();
                self.video_position_feedback(target);
            }
            crate::video_session::FrameStep::None => {}
        }
        true
    }

    /// Surface a video seek/step position to the user. The info line's playback
    /// row is the readout (owner call 2026-07-11): if the line is on it already
    /// tracks the target; if it's off, **flash the line** for a beat instead of
    /// the old `m:ss / m:ss` toast — the line looks better and does more. The
    /// toast survives only where the line can't flash (HUD shells, Tab-hidden).
    fn video_position_feedback(&mut self, target: Duration) {
        if self.info_line && self.info_line_visible() {
            return; // the persistent line's row tracks the seek already
        }
        if self.arm_video_line_flash() {
            return;
        }
        let osd = match self.video.as_ref().and_then(|v| v.duration()) {
            Some(d) => format!(
                "{} / {}",
                crate::video::format_video_duration(target),
                crate::video::format_video_duration(d)
            ),
            None => crate::video::format_video_duration(target),
        };
        self.show_toast(&osd);
    }

    /// Stop and drop any active video session (navigation, delete, teardown). The
    /// currently displayed frame stays on screen; the caller decides what replaces it.
    /// Record — or forget — a video's session-only resume position (task #94.2),
    /// applying the [`video_resume_target`] policy: a spot meaningfully into a
    /// long-enough clip is remembered (rewound a touch); a near-start / near-end /
    /// watched-to-the-end position FORGETS any prior entry so returning restarts.
    /// Keyed by item index; RAM-only. Both backends funnel their position here.
    fn note_video_position(&mut self, item: usize, pos: Duration, dur: Duration) {
        match video_resume_target(pos, dur) {
            Some(target) => {
                self.video_resume.insert(item, target);
            }
            None => {
                self.video_resume.remove(&item);
            }
        }
    }

    /// Shell → core: the macOS native player's current position (task #94.2). The
    /// core holds no native clock, so the shell reports it each pump; this folds it
    /// into the resume map so returning to the item resumes where it left off. Only
    /// the live session's reports count (session-gated).
    pub fn native_video_progress(
        &mut self,
        session_id: u64,
        position_secs: f64,
        duration_secs: f64,
    ) {
        let sid = crate::video::VideoSessionId(session_id);
        // Mirror the playhead onto the proxy (task #90): on this backend the shell owns the
        // clock, so this ~20 Hz report is the core's only view of where the picture is —
        // and subtitle cues need it. Stored on every report, independent of the resume
        // bookkeeping below, which deliberately ignores a near-start/end position.
        if position_secs >= 0.0 {
            if let Some(p) = self
                .video
                .as_mut()
                .and_then(ActiveVideoBackend::as_native_mut)
                .filter(|p| p.session_id == sid)
            {
                p.set_position(Duration::from_secs_f64(position_secs));
            }
        }
        let item = self
            .video
            .as_ref()
            .and_then(ActiveVideoBackend::as_native)
            .filter(|p| p.session_id == sid)
            .map(|p| p.item);
        if let Some(item) = item {
            if duration_secs > 0.0 && position_secs >= 0.0 {
                self.note_video_position(
                    item,
                    Duration::from_secs_f64(position_secs),
                    Duration::from_secs_f64(duration_secs),
                );
            }
        }
    }

    pub fn stop_video(&mut self) {
        let now = self.now;
        if let Some(v) = self.video.take() {
            match v {
                ActiveVideoBackend::Session(mut s) => {
                    // Remember where we're leaving off (task #94.2) before teardown,
                    // so returning to this item resumes near here (or forgets a
                    // watched-to-the-end clip so it restarts). RAM-only.
                    if let Some(dur) = s.session.duration {
                        let pos = s.session.desired_position(now);
                        self.note_video_position(s.item, pos, dur);
                    }
                    s.session.stop();
                    self.effects.push(contract::CoreEffect::StopVideoAudio);
                }
                // macOS: tear down the native player (which owns its own audio);
                // stale callbacks are rejected by session id.
                ActiveVideoBackend::Native(p) => {
                    self.effects.push(contract::CoreEffect::StopVideo {
                        session_id: p.session_id,
                    });
                }
            }
            self.update_video_progress(); // drops the playback row promptly
                                          // A flashed seek OSD dies with its session (don't linger a bare line).
            if self.video_osd_until.take().is_some() {
                self.emit_panels_changed();
            }
            // A geometry change deferred while this video played: refill the ring
            // now that the decode pool can't jerk the playback (the displayed
            // frame stays; navigation's own load handles the current item).
            if std::mem::take(&mut self.video_geometry_stale) {
                self.target_item = self.playlist.current();
                self.request_prefetch();
            }
            // A resize-pause dies with its session (never resume a later one).
            self.video_paused_by_resize = false;
        }
    }

    /// Per-tick video drive (task #79 phases 4+5): poll the session, present the
    /// due frame through the reusable present path, keep the shell audio player in
    /// lockstep with the session state, surface failures.
    pub fn poll_video(&mut self) {
        // Session backends only (Windows/Linux, and macOS for the containers
        // AVPlayer doesn't handle — MKV/WebM since the smoothness plan). The
        // macOS `AVPlayer` route has no session to pump — it runs itself and
        // reports back via callbacks.
        // Inline the field borrow (not the helper) so `v` borrows only `self.video`,
        // leaving `self.now`/`self.effects`/`self.source` usable below.
        let now = self.now;
        // Is the keyboard seek key released? `video_seek_last` is set on every held
        // repeat and cleared the tick a horizontal-seek key lifts (`apply_view_holds`,
        // which runs AFTER this in the tick — so this reads last tick's state, ~8 ms
        // stale, which is fine). Drives the adaptive audio-commit below. Captured here
        // because `v` borrows `self.video` exclusively.
        let seek_key_released = self.video_seek_last.is_none();
        let Some(v) = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
        else {
            return;
        };
        let mut update = v.session.poll(now);
        let state = v.session.state();
        let started = v.session.has_started();
        let session_id = v.session.id;
        // PB_TRACE: the Session route's objective smoothness numbers
        // (macos-video-smoothness §4) — the analog of the sample-buffer route's
        // `sb-play diag`. `dropped` counts late frames the catch-up drain
        // discarded (plan 1C/0B); `rebuf` counts mid-play starvation freezes
        // (the other stutter flavor — a network read spike that empties the
        // queue freezes rather than drops). A healthy clip holds both at 0.
        // ~Every 2 s while playing, with the dropped delta since the last line.
        if pb_trace() && state == crate::video::VideoSessionState::Playing {
            let stale = self.video_diag_last.is_none_or(|(id, t, _)| {
                id != session_id || now.saturating_duration_since(t) >= Duration::from_secs(2)
            });
            if stale {
                let dropped = v.session.dropped_frames();
                let prev = self
                    .video_diag_last
                    .filter(|(id, _, _)| *id == session_id)
                    .map_or(0, |(_, _, n)| n);
                eprintln!(
                    "[pb-video] session diag: dropped={dropped} (+{}) rebuf={} pos={:.1}s",
                    dropped.saturating_sub(prev),
                    v.session.rebuffers(),
                    v.session.position(now).as_secs_f64()
                );
                self.video_diag_last = Some((session_id, now, dropped));
            }
        }
        // One-shot resume seek (task #94.2): once the fresh session can accept a
        // seek, jump to the remembered position. The poster is held (present
        // suppressed) until then, and the seek flushes the pre-resume frames by
        // generation, so returning to a video lands where you left off with no
        // start-flash. Wired into the 1D audio coordinator below (same fields a
        // user seek sets), so audio lands at the resume point too.
        let mut resume_pause_audio = false;
        if v.resume_to.is_some() {
            if state != crate::video::VideoSessionState::Opening {
                if let Some(target) = v.resume_to.take() {
                    if v.session.seek_to(target, now, None).is_some() {
                        v.pending_audio_commit = None;
                        resume_pause_audio = !v.scrub_audio_paused;
                        v.scrub_audio_paused = true;
                        v.last_seek_intent = Some(now);
                    }
                }
            }
            update.present = None; // hold the poster until the resume frame lands
        }
        // 1D audio-seek coordinator: a landing stores the commit position (only
        // the latest generation lands, so this is inherently supersede-safe);
        // the commit fires once the run settles — no new seek intent for
        // VIDEO_SEEK_AUDIO_SETTLE — producing exactly ONE audio seek per run.
        if let Some(pos) = update.seek_landed {
            v.pending_audio_commit = Some(pos);
            // The picture is now at the target; stamp it so the audio commit below
            // can report how long after that it fired (the A/V-gap settle residual).
            v.dbg_seek_land_at = Some(now);
        }
        // Adaptive settle (task #4 follow-up): a discrete tap commits its audio seek
        // fast — once the seek key is released and a brief quiet has passed — instead
        // of always waiting out the full window. Held scrubbing keeps the key down, so
        // it only ever clears the full-window fallback (which its 200 ms repeats keep
        // resetting until release), never the fast path. The full window also covers
        // any input that doesn't drive `video_seek_last` (a scrubber drag), where
        // `seek_key_released` reads true — but there the frequent drag intents keep
        // resetting even the short quiet, so it still coalesces.
        let elapsed = |win: Duration| {
            v.last_seek_intent
                .is_none_or(|t| now.saturating_duration_since(t) >= win)
        };
        let settled = elapsed(VIDEO_SEEK_AUDIO_SETTLE)
            || (seek_key_released && elapsed(VIDEO_SEEK_AUDIO_QUIET));
        let commit = if settled {
            v.pending_audio_commit.take()
        } else {
            None
        };
        // A/V-gap measurement (PB_AV_SYNC): capture the delays now, while `v` is
        // borrowed and BEFORE `resume_with_commit` below clears `last_seek_intent`.
        // The land delay is the settle residual the user perceives as "audio catching
        // up"; the WASAPI reseek (PB_AUDIO_TRACE) stacks on top.
        let dbg_av_delays = commit.map(|_| {
            (
                v.dbg_seek_land_at
                    .take()
                    .map(|l| now.saturating_duration_since(l)),
                v.last_seek_intent.map(|t| now.saturating_duration_since(t)),
            )
        });
        let resume_with_commit = if commit.is_some() {
            let was_scrub_paused = v.scrub_audio_paused;
            v.scrub_audio_paused = false;
            v.last_seek_intent = None;
            was_scrub_paused && state == crate::video::VideoSessionState::Playing
        } else {
            false
        };
        let scrub_paused = v.scrub_audio_paused;
        // Start the shell audio player the moment the producer reports a track
        // (Opened), paused — it opens in parallel with the video preroll and the
        // two resume together. Silent clips never create a player. The player
        // opens the SAME container the producer reads (the media slot): a path,
        // or an archive entry's shared in-RAM bytes. The slot is always set by
        // the time `Opened` lands (the producer thread fills it first).
        let start_audio = !v.audio_started && v.session.has_audio() == Some(true);
        if start_audio {
            v.audio_started = true;
        }
        let audio_input = if start_audio {
            v.media.get().cloned()
        } else {
            None
        };
        if let Some(input) = audio_input {
            let muted = self.effective_mute();
            self.effects.push(contract::CoreEffect::StartVideoAudio {
                input,
                session_id,
                muted,
            });
        }
        // The resume seek pauses audio once for its run (after StartVideoAudio so
        // the player exists; it opens paused anyway). Its landing commits the ONE
        // SeekVideoAudio (+ resume) below via the same 1D path as a user seek.
        if resume_pause_audio {
            self.effects.push(contract::CoreEffect::PauseVideoAudio);
        }
        // The settled seek run's ONE audio commit: seek, then resume (in that
        // order) if the clip plays on. Emitted before the state bridge below so
        // audio can never resume at a pre-seek position.
        if let Some(pos) = commit {
            if let (Some((land_delay, intent_delay)), true) =
                (dbg_av_delays, std::env::var_os("PB_AV_SYNC").is_some())
            {
                eprintln!(
                    "[pb-avsync] audio seek committed to {:.2}s — {:?} after the picture landed, {:?} after the last seek input",
                    pos.as_secs_f64(),
                    land_delay,
                    intent_delay,
                );
            }
            self.effects
                .push(contract::CoreEffect::SeekVideoAudio { position: pos });
            if resume_with_commit {
                self.effects.push(contract::CoreEffect::ResumeVideoAudio);
            }
        }
        // Session state drives the audio player (freeze together, resume
        // together): Playing = resume; a mid-play rebuffer or the end = pause.
        // While a seek run holds audio paused (`scrub_paused`), Playing
        // promotions do NOT resume — the commit above owns the resume, so
        // intermediate landings of a held/scrubbed run stay silent (1D).
        if update.state_changed {
            use crate::video::VideoSessionState::*;
            match state {
                Playing if !scrub_paused && commit.is_none() => {
                    self.effects.push(contract::CoreEffect::ResumeVideoAudio);
                }
                // A landing's Seeking→Buffering hop while the run already holds
                // audio paused would just spam redundant pauses — skip those.
                Buffering if started && !scrub_paused => {
                    self.effects.push(contract::CoreEffect::PauseVideoAudio);
                }
                Ended => self.effects.push(contract::CoreEffect::PauseVideoAudio),
                _ => {}
            }
        }
        if let Some(frame) = update.present {
            self.present_video_frame(&frame);
            // The frame (and its CPU pixels) drops here — released after upload.
            self.draw();
            return;
        }
        if update.state_changed {
            match state {
                crate::video::VideoSessionState::Failed => {
                    let msg = self
                        .session_ref()
                        .and_then(|v| v.session.error.clone())
                        .unwrap_or_else(|| "Video playback failed".into());
                    self.video = None;
                    self.effects.push(contract::CoreEffect::StopVideoAudio);
                    self.show_toast(&msg);
                }
                // Ended parks on the last presented frame; P replays.
                _ => self.draw(),
            }
        }
    }

    /// Arm (or refresh) the transient info-line reveal while a video is active —
    /// shared by the seek/step OSD and the pointer hover reveal. `true` when the
    /// flash path applies (native-line shells, chrome not Tab-hidden); the tick
    /// arm drops the line at the deadline.
    fn arm_video_line_flash(&mut self) -> bool {
        if !self.native_info || self.panels.hidden || self.current.is_none() {
            return false;
        }
        let fresh = self.video_osd_until.is_none();
        self.video_osd_until = Some(self.now + VIDEO_OSD_HOLD);
        if fresh {
            self.show_info_line();
            self.emit_panels_changed();
        }
        true
    }

    /// Pointer hover over the **controls zone** — the bottom quarter of the
    /// window, where the info line lives — reveals the playback controls while a
    /// video is active, like every video player (owner request). It's the same
    /// transient reveal the seek OSD uses: refreshed on every pointer move inside
    /// the zone, decaying via the tick arm once the pointer leaves. Shell-neutral
    /// policy: pointer moves arrive as `CoreEvent::PointerMoved` from every shell
    /// (the macOS SwiftUI shell shares this the moment it forwards its hovers).
    pub fn video_hover_reveal(&mut self, y: f32) {
        use crate::video::VideoSessionState::*;
        if self.info_line {
            return; // the persistent line already shows the controls
        }
        if y < self.viewport.height as f32 * (1.0 - VIDEO_HOVER_ZONE) {
            return;
        }
        let active = self.video.as_ref().is_some_and(|v| {
            Some(v.item()) == self.displayed_item && !matches!(v.state(), Failed | Stopped)
        });
        if active {
            self.arm_video_line_flash();
        }
    }

    /// Re-arm the transient controls reveal directly (no hover geometry). The macOS shell
    /// calls this when the user releases the info-line scrubber: a SwiftUI drag captures the
    /// pointer, so canvas pointer-moves — and thus [`video_hover_reveal`] — stop firing, and
    /// the flash would snap away the instant the drag ends. This lets it fade out gracefully
    /// instead. Same active guard as the hover path so it can't flash the line for a still.
    pub fn flash_video_controls(&mut self) {
        use crate::video::VideoSessionState::*;
        if self.info_line {
            return; // the persistent line is already up
        }
        let active = self.video.as_ref().is_some_and(|v| {
            Some(v.item()) == self.displayed_item && !matches!(v.state(), Failed | Stopped)
        });
        if active {
            self.arm_video_line_flash();
        }
    }

    /// The planar-video producer options for a new session (task #91 Phase 2):
    /// attempt the planar GPU color path unless `PB_VIDEO_NO_PLANAR` disables it
    /// (the A/B lever / safety hatch), reporting the renderer's real P010
    /// capability so 10-bit sources fall back to RGBA/fp16 on adapters without
    /// `TEXTURE_FORMAT_16BIT_NORM`. No renderer (headless) → no planar path.
    ///
    /// Gated to match its only call site (the session-platform block in
    /// `start_video_playback`): without `ffvideo` on macOS, video is the native
    /// AVFoundation player, so there is no producer to hand options to and this is dead
    /// code — which `cargo clippy --all-targets -- -D warnings`, the documented lint
    /// command (it passes no features), rejects.
    #[cfg(any(windows, all(unix, feature = "ffvideo")))]
    fn planar_video_options(&self) -> pb_decode::VideoProducerOptions {
        let planar = std::env::var_os("PB_VIDEO_NO_PLANAR").is_none() && self.renderer.is_some();
        let supports_p010 = self.renderer.as_ref().is_some_and(|r| r.supports_p010());
        pb_decode::VideoProducerOptions {
            planar,
            supports_p010,
        }
    }

    /// Upload one decoded video frame through the reusable present path,
    /// dispatching on its pixel format (task 79.10): RGBA8 rides `set_image`
    /// exactly as before; NV12 splits its planes and goes through the renderer's
    /// `set_video_nv12` (in-shader YUV on wgpu; a CPU convert on fallback shells);
    /// fp16 HDR frames (task #84 plan §9) ride `set_image`'s HDR arm with the
    /// frame's scene-linear `peak` — the same fp16 scRGB present path as HDR
    /// stills, so PQ/HLG video gets real headroom on an EDR/HDR surface and a
    /// correct tone-map on SDR, never an RGBA8 clip.
    fn present_video_frame(&mut self, frame: &pb_decode::VideoFrame) {
        let item = self.video.as_ref().map(|v| v.item());
        // The metadata half of a present (owner report 2026-07-16): video frames
        // stream around `present_item`, and the first frame's `mark_resolved`
        // below makes a still-decoding poster skip its own present when it lands
        // — so a video started before the poster (P beats a slow SMB poster
        // decode every time) left `current` unset for the whole session, and
        // with it the info line, the `i` toggle, the hover reveal, and the
        // playback controls (`arm_video_line_flash` requires metadata). The
        // poster's meta still reaches `meta_cache` when its decode completes
        // (`drain_results` caches it unconditionally) — adopt it the moment it
        // exists.
        if let Some(item) = item {
            if self.current.is_none() && self.displayed_item == Some(item) {
                self.current = self.meta_cache.get(&item).cloned();
            }
        }
        {
            let Some(a) = self.renderer.as_mut() else {
                return;
            };
            if frame.format.is_planar_video() {
                // NV12 / P010 (task 79.10 / #91 Phase 2): split at the checked Y-plane
                // span (never a raw `split_at`, which would panic on a short buffer —
                // though `VideoSession` already rejects malformed frames) and hand the
                // two planes to the in-shader planar path.
                if let Some((y_len, _uv_off, _uv_len)) =
                    frame.format.planar_plane_spans(frame.width, frame.height)
                {
                    let (y, uv) = frame.pixels.split_at(y_len.min(frame.pixels.len()));
                    a.set_video_planar(
                        y,
                        uv,
                        frame.width,
                        frame.height,
                        crate::engine::render_planar_present(frame.format, &frame.color),
                    );
                }
            } else {
                a.set_image(
                    &frame.pixels,
                    frame.width,
                    frame.height,
                    render_color(&frame.color.transform),
                    frame.format == pb_decode::PixelFormat::Rgba16F,
                    frame.color.peak,
                );
            }
        }
        // Each presented frame re-resolves the video item at the current epoch, so a
        // resize during playback keeps `target_caught_up` true (no loading pie over live
        // video) — `present_video_frame` streams frames without going through
        // `present_item` (task #18 finding #5).
        if let Some(item) = item {
            self.mark_resolved(item);
        }
    }

    /// Shell → core: the platform video-audio player's latest clock sample
    /// (task #79 phase 5). Routed to the active session, which uses it as the
    /// master clock while both sides play.
    pub fn video_audio_clock(&mut self, sample: crate::video::AudioClockSample) {
        // Session backends only — on macOS the native `AVPlayer` is its own clock.
        let now = self.now;
        if let Some(v) = self.session_mut() {
            v.session.on_audio_clock(sample, now);
        }
    }

    /// The info line's playback row for the displayed item's live video session
    /// (`None` on stills / dead sessions — the line renders single-row as always).
    /// Public: the winit shell's egui info line (and later the macOS SwiftUI one)
    /// reads it to draw the `elapsed ▰▰▰▱▱ total` row natively.
    pub fn video_progress_row(&self) -> Option<hud::ProgressRow> {
        // Session backends only: the row is computed from the session's clock. On
        // macOS the SwiftUI info row reads the native `AVPlayer` directly (79.9
        // phase 5), so the core provides no progress there.
        let v = self.session_ref()?;
        if Some(v.item) != self.displayed_item {
            return None;
        }
        use crate::video::VideoSessionState::*;
        if matches!(v.session.state(), Failed | Stopped) {
            return None;
        }
        let pos = v.session.desired_position(self.now);
        let (total, fraction) = match v.session.duration {
            Some(d) if !d.is_zero() => (
                Some(crate::video::format_video_duration(d)),
                (pos.as_secs_f32() / d.as_secs_f32()).clamp(0.0, 1.0),
            ),
            _ => (None, 0.0),
        };
        Some(hud::ProgressRow {
            elapsed: crate::video::format_video_duration(pos),
            total,
            fraction,
        })
    }

    /// Whether the DISPLAYED item plays through the cross-platform
    /// `VideoSession` backend — on macOS that's the FFmpeg route (task #84 §8),
    /// and the SwiftUI shell keys its controls visibility + scrubber routing on
    /// this (its `nativeVideo` checks cover only the `Native` backend).
    pub fn video_session_active(&self) -> bool {
        use crate::video::VideoSessionState::*;
        self.session_ref().is_some_and(|v| {
            Some(v.item) == self.displayed_item && !matches!(v.session.state(), Failed | Stopped)
        })
    }

    /// The active session's playhead in seconds — raw numbers for the SwiftUI
    /// scrubber (the winit shell reads the formatted [`Self::video_progress_row`]
    /// instead). `0.0` when no session is active.
    pub fn video_session_elapsed_secs(&self) -> f64 {
        match self.session_ref() {
            Some(v) if self.video_session_active() => {
                v.session.desired_position(self.now).as_secs_f64()
            }
            _ => 0.0,
        }
    }

    /// Is a video actually on screen right now — **on either backend**?
    ///
    /// [`video_session_active`](Self::video_session_active) answers only for the
    /// `VideoSession` route. On macOS the sample-buffer presenter (the default for
    /// MKV/WebM since Phase 3F) and AVPlayer are both `Native` backends, so a
    /// session-only check reads false while a video plays — which silently disabled
    /// subtitles the moment that route became the default. Anything asking "is the user
    /// watching a video" wants this, not that.
    pub fn video_showing(&self) -> bool {
        use crate::video::VideoSessionState::*;
        self.video.as_ref().is_some_and(|b| {
            Some(b.item()) == self.displayed_item && !matches!(b.state(), Failed | Stopped)
        })
    }

    /// The playhead of whatever is playing, on either backend. `None` when nothing is, or
    /// before the shell's first position report on the `Native` route.
    pub fn video_position(&self) -> Option<Duration> {
        self.video
            .as_ref()
            .filter(|_| self.video_showing())
            .and_then(|b| b.position(self.now))
    }

    /// Rebuild the subtitle overlay against the playhead (task #90) — the one call that
    /// joins discovery, the cue track, placement, and the rasterizer to the screen. Runs
    /// every tick; both shells then read [`AppCore::subtitles`] and composite.
    ///
    /// The clock is [`video_position`](Self::video_position) — the playhead of whichever
    /// backend is live, so a subtitle can't drift from the picture and can't be silently
    /// switched off by a routing change. On `Session` that's the session's own
    /// `desired_position`; on `Native` (macOS AVPlayer *and* the sample-buffer route) it's
    /// the shell's ~20 Hz report, which is ~50 ms granular against cues that last seconds.
    /// `Shift+C` — the next subtitle *track* (#99).
    ///
    /// Tracks only: `Off` belongs to `C` now, and `Automatic` is not a step (it resolves to
    /// one of these tracks anyway, so it would show the same subtitles twice under two
    /// names). It also switches subtitles **on** — asking for the next track when they are
    /// off can only mean you want to see one.
    ///
    /// Toasts **optimistically**, which is safe here and would not be for audio: swapping a
    /// subtitle track only re-aims a cue reader, so there is no clock to re-prime and
    /// nothing to fail silently. (Task #99's rule that audio must toast only on a
    /// *confirmed* switch stands — it is a different risk, not the same one.)
    pub fn cycle_subtitle_track(&mut self) {
        let Some(item) = self.displayed_item.filter(|_| self.video_showing()) else {
            return; // not a video: `Shift+C` says nothing rather than lying
        };
        self.ensure_exif_cached(item);
        let Some(catalog) = self.exif_cache.get(&item).and_then(|d| d.media.as_ref()) else {
            self.show_toast_icon("Reading tracks…", ToastIcon::Captions);
            return;
        };
        // What is *currently showing* — so the cycle advances from the track on screen
        // rather than jumping back to it (which is what Automatic would otherwise do).
        let showing = self
            .subtitles
            .selection
            .resolve(catalog, crate::subtitle::audio_language_of(catalog))
            .map(|t| t.id);
        let Some(next) = crate::subtitle::next_track(catalog, showing) else {
            // Nothing to step through. Say so rather than no-op'ing: a key that does
            // nothing is indistinguishable from a key that is broken.
            self.show_toast_icon("No subtitle tracks", ToastIcon::CaptionsOff);
            return;
        };
        self.apply_subtitle_choice(crate::subtitle::SubtitleChoice::Track(next));
    }

    /// Commit a picker choice: the state, the persisted on/off preference, and the toast.
    ///
    /// Shared by `Shift+C` and the picker (#99) so a choice made either way behaves
    /// identically — the alternative is two paths that agree until one of them is edited.
    ///
    /// The picker toasts too, even though its tick already moved: a track can be silent for
    /// half a minute, so without it "did that work?" has no answer until a cue happens to be
    /// due.
    fn apply_subtitle_choice(&mut self, choice: crate::subtitle::SubtitleChoice) {
        use crate::subtitle::SubtitleChoice;

        let Some(catalog) = self
            .displayed_item
            .and_then(|item| self.exif_cache.get(&item))
            .and_then(|d| d.media.as_ref())
        else {
            return; // nothing to choose from, and no catalog to read a preference out of
        };
        // The label through the same `track_summary` the Details panel uses (#98) — two
        // formatters would drift. Built before the mutable borrows below.
        let label = match choice {
            SubtitleChoice::Off => "Subtitles off".to_string(),
            SubtitleChoice::Automatic => "Subtitles automatic".to_string(),
            SubtitleChoice::Track(id) => catalog
                .subtitles
                .tracks
                .iter()
                .find(|t| t.id == id)
                .map(crate::tracks::track_summary)
                .unwrap_or_else(|| "Subtitles on".into()),
        };
        let catalog = catalog.clone(); // ends the borrow of `exif_cache`
        self.subtitles.selection.apply(choice, &catalog);

        let on = self.subtitles.selection.enabled;
        // `settings.subtitles` is the on/off preference `C` persists; keep it honest when a
        // picker row turns them off, or `C` would come back on to a state the file no
        // longer describes.
        self.settings.subtitles = on;
        if self.persist_prefs {
            self.settings.save();
        }
        self.show_toast_icon(
            &label,
            if on {
                ToastIcon::Captions
            } else {
                ToastIcon::CaptionsOff
            },
        );
    }

    /// The subtitle picker's rows for the video on screen (#99) — the playback bar's
    /// popover and the Playback ▸ Subtitles flyout read this.
    ///
    /// Empty means "offer nothing": no video, or the track probe hasn't landed yet. The
    /// shells distinguish those with [`subtitle_tracks_known`](Self::subtitle_tracks_known)
    /// rather than reading an empty list as "this file has none".
    pub fn subtitle_picker_rows(&mut self) -> Vec<crate::tracks::PickerRow> {
        let Some(item) = self.displayed_item.filter(|_| self.video_showing()) else {
            return Vec::new();
        };
        self.ensure_exif_cached(item);
        let Some(catalog) = self.exif_cache.get(&item).and_then(|d| d.media.as_ref()) else {
            return Vec::new();
        };
        crate::tracks::subtitle_picker_rows(
            catalog,
            &self.subtitles.selection,
            crate::subtitle::audio_language_of(catalog),
        )
    }

    // ── The audio track picker (task #99) ────────────────────────────────
    //
    // Audio differs from subtitles in a way that shapes all of this: the core does not own
    // the choice. The decoder picks a track at open, the shell owns the player, and only
    // the shell can say what is coming out of the speakers. So the core formats the rows and
    // hands out locators; the shell acts and reports back.

    /// The displayed item when it is a **video** — by its own kind, or because a video
    /// session is showing for it. The audio picker's gate (owner, 2026-07-17): the track
    /// catalog belongs to the *item* (the details probe), so it must not require a
    /// running session — gating on `video_showing()` made the flyout claim "No Video"
    /// over a film sitting at its poster.
    fn displayed_video_item(&self) -> Option<usize> {
        self.displayed_item
            .filter(|&i| self.item_is_video(i) || self.video_showing())
    }

    /// Is the displayed item a video (playing or not)? The shell's flyout gate.
    pub fn displayed_is_video(&self) -> bool {
        self.displayed_video_item().is_some()
    }

    /// Has the track probe landed for the displayed video? The audio twin of
    /// [`subtitle_tracks_known`](Self::subtitle_tracks_known), minus its session gate.
    pub fn audio_tracks_known(&self) -> bool {
        self.displayed_video_item()
            .and_then(|item| self.exif_cache.get(&item))
            .is_some_and(|d| d.media.is_some())
    }

    /// The catalog for the displayed video, if its probe has landed.
    fn showing_catalog(&self) -> Option<&pb_decode::MediaTrackCatalog> {
        self.displayed_video_item()
            .and_then(|item| self.exif_cache.get(&item))
            .and_then(|d| d.media.as_ref())
    }

    /// The Playback ▸ Audio flyout's rows. Empty = offer nothing (no video, or the probe
    /// hasn't landed — [`audio_tracks_known`](Self::audio_tracks_known) tells those
    /// apart).
    pub fn audio_picker_rows(&mut self) -> Vec<crate::tracks::PickerRow> {
        let Some(item) = self.displayed_video_item() else {
            return Vec::new();
        };
        self.ensure_exif_cached(item);
        let active = self.audio_active;
        self.showing_catalog()
            .map(|c| crate::tracks::audio_picker_rows(c, active))
            .unwrap_or_default()
    }

    /// Row `row`'s track id, if it exists.
    fn audio_row_id(&self, row: usize) -> Option<pb_decode::TrackId> {
        self.showing_catalog()
            .and_then(|c| c.audio.tracks.get(row))
            .map(|t| t.id)
    }

    /// Row `row`'s locator — **how the shell reaches this track on its route**.
    ///
    /// The two backends speak different currencies and this is the seam that hides it: the
    /// FFmpeg catalog locates a track by container stream index, AVFoundation by a
    /// serialized `AVMediaSelectionOption`. Even `local_id` means different things (a real
    /// stream index vs. a running counter), which is exactly why nothing outside may treat
    /// an id as a stream number.
    fn audio_row_locator(&self, row: usize) -> Option<&pb_decode::tracks::TrackLocator> {
        let id = self.audio_row_id(row)?;
        self.showing_catalog()?.locator(id)
    }

    /// Row `row` as an FFmpeg stream index (`-1` = this row isn't FFmpeg-located) — the
    /// sample-buffer route's currency, fed straight to `session_audio_set_track`.
    pub fn audio_row_ff_stream(&self, row: usize) -> i64 {
        match self.audio_row_locator(row) {
            Some(pb_decode::tracks::TrackLocator::FfStream(i)) => *i as i64,
            _ => -1,
        }
    }

    /// Row `row`'s serialized `AVMediaSelectionOption` (empty = this row isn't
    /// AVFoundation-located) — the AVPlayer route's currency. The spike proved this
    /// round-trips through `mediaSelectionOptionWithPropertyList:` to an option that
    /// `isEqual:` the original, which is what lets the shell re-find this exact option
    /// without trusting an ordinal.
    pub fn audio_row_av_plist(&self, row: usize) -> Vec<u8> {
        match self.audio_row_locator(row) {
            Some(pb_decode::tracks::TrackLocator::AvOption { property_list, .. }) => {
                property_list.clone()
            }
            _ => Vec::new(),
        }
    }

    /// Row `row` as a **Media Foundation reader stream index** (`-1` = this row isn't
    /// MF-located) — one of the two currencies the Windows WASAPI engine accepts.
    /// Only MF's *own* catalog mints these, which on Windows means a no-`ffprobe`
    /// build: the usual `ffprobe` build runs FFmpeg's catalog with `FfStream`
    /// locators and the engine decodes those directly (FFmpeg-first). MF's stream
    /// order differs from the container's, which is why an MF index can never be a
    /// row or an FFmpeg index in disguise.
    pub fn audio_row_mf_stream(&self, row: usize) -> i64 {
        match self.audio_row_locator(row) {
            Some(pb_decode::tracks::TrackLocator::MfStream(s)) => *s as i64,
            _ => -1,
        }
    }

    /// The picker row whose locator names MF reader stream `stream` (`-1` = none) —
    /// how the Windows shell translates the engine's "this is what is actually
    /// decoding" report into a row for [`set_active_audio_row`](Self::set_active_audio_row).
    /// The winit twin of the macOS host's `reportActiveAudioStream`.
    pub fn audio_row_for_mf_stream(&self, stream: i64) -> i64 {
        self.audio_row_for(|loc| {
            matches!(loc, pb_decode::tracks::TrackLocator::MfStream(s) if *s as i64 == stream)
        })
    }

    /// The picker row whose locator names FFmpeg stream `stream` (`-1` = none) — the
    /// same translation for the Linux engine, whose currency is container stream
    /// indices end-to-end.
    pub fn audio_row_for_ff_stream(&self, stream: i64) -> i64 {
        self.audio_row_for(|loc| {
            matches!(loc, pb_decode::tracks::TrackLocator::FfStream(i) if *i as i64 == stream)
        })
    }

    fn audio_row_for(&self, mut hit: impl FnMut(&pb_decode::tracks::TrackLocator) -> bool) -> i64 {
        let Some(catalog) = self.showing_catalog() else {
            return -1;
        };
        catalog
            .audio
            .tracks
            .iter()
            .position(|t| catalog.locator(t.id).is_some_and(&mut hit))
            .map_or(-1, |r| r as i64)
    }

    /// The shell reports which row it is **actually playing** (`-1` = unknown/none).
    ///
    /// Called on open and after every switch — including a *refused* one, where it re-states
    /// the unchanged track. That is what keeps the tick honest rather than optimistic.
    pub fn set_active_audio_row(&mut self, row: i64) {
        self.audio_active = usize::try_from(row).ok().and_then(|r| self.audio_row_id(r));
    }

    /// `A` / `Shift+A` — step to the next/previous audio track (task #99).
    ///
    /// **Emits an effect rather than switching**, because the core cannot switch audio: the
    /// player lives in the shell, the two routes reach a track by different locators, and
    /// the switch can fail. So this only decides *which row*, and the shell does it and
    /// reports back through the same `audio_track_switched` path the menu uses — one route
    /// in, one route out, so a key and a menu click cannot behave differently.
    ///
    /// Steps from **what is playing**, not from a remembered request: `audio_active` is the
    /// shell's report, so a stale pick that quietly fell back to the policy still advances
    /// from the track you can actually hear.
    pub fn cycle_audio_track(&mut self, forward: bool) {
        let Some(item) = self.displayed_video_item() else {
            return; // not a video: the key says nothing rather than lying
        };
        self.ensure_exif_cached(item);
        let Some(catalog) = self.showing_catalog() else {
            self.show_toast_icon("Reading tracks…", ToastIcon::AudioTrack);
            return;
        };
        // Only tracks we can actually play are steps — one we'd refuse would be a dead stop
        // in the rotation.
        let rows: Vec<usize> = catalog
            .audio
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.capability != pb_decode::TrackCapability::Unsupported)
            .map(|(i, _)| i)
            .collect();
        if rows.len() < 2 {
            // Nothing to step to. Say so rather than no-op: a key that does nothing is
            // indistinguishable from a key that is broken.
            let msg = if rows.is_empty() {
                "No audio tracks"
            } else {
                "Only one audio track"
            };
            self.show_toast_icon(msg, ToastIcon::AudioTrackFailed);
            return;
        }
        let here = self
            .audio_active
            .and_then(|id| catalog.audio.tracks.iter().position(|t| t.id == id))
            .and_then(|i| rows.iter().position(|r| *r == i));
        let next = match here {
            Some(i) if forward => rows[(i + 1) % rows.len()],
            Some(i) => rows[(i + rows.len() - 1) % rows.len()],
            // Not told what is playing yet (the probe or the open is still landing) — start
            // at the first rather than guessing at the policy's pick.
            None => rows[0],
        };
        self.effects
            .push(crate::contract::CoreEffect::SelectAudioTrack { row: next });
    }

    /// The shell reports the outcome of a switch it attempted: toast, and re-state the tick.
    ///
    /// **Only on a confirmed switch** (#99's rule, and the asymmetry with subtitles is
    /// deliberate). Swapping a subtitle track only re-aims a cue reader, so it can toast
    /// optimistically; audio re-opens a decoder and rebuilds a format, and either can fail
    /// while the previous track plays on. A toast naming a track over unchanged audio would
    /// teach the user to distrust every other toast in the app.
    pub fn audio_track_switched(&mut self, row: usize, ok: bool) {
        if !ok {
            self.show_toast_icon("Couldn't switch audio track", ToastIcon::AudioTrackFailed);
            return;
        }
        // The row the shell actually landed on is re-reported separately by
        // `set_active_audio_row`, so read the label back from the catalog rather than
        // trusting the request: a stale pick falls back to the policy inside the decoder,
        // and the toast must name what is *playing*.
        let label = self
            .audio_active
            .and_then(|id| {
                self.showing_catalog()?
                    .audio
                    .tracks
                    .iter()
                    .find(|t| t.id == id)
            })
            .map(crate::tracks::track_summary)
            .or_else(|| {
                self.showing_catalog()?
                    .audio
                    .tracks
                    .get(row)
                    .map(crate::tracks::track_summary)
            })
            .unwrap_or_else(|| "Audio track changed".into());
        self.show_toast_icon(&label, ToastIcon::AudioTrack);
    }

    /// Are subtitles switched on — the `C` state?
    ///
    /// The playback bar's picker button fills its icon on this, so the control reports its
    /// own state the way play/pause does. Not the same question as "is a cue on screen":
    /// subtitles are on through every silent gap between cues.
    pub fn subtitles_on(&self) -> bool {
        self.subtitles.selection.enabled
    }

    /// Has the track probe landed for the video on screen? `false` = "still reading", which
    /// is not the same answer as "no tracks" and must not be drawn as one.
    pub fn subtitle_tracks_known(&self) -> bool {
        self.displayed_item
            .filter(|_| self.video_showing())
            .and_then(|item| self.exif_cache.get(&item))
            .is_some_and(|d| d.media.is_some())
    }

    /// Apply picker row `row` — an index into the very list
    /// [`subtitle_picker_rows`](Self::subtitle_picker_rows) returned.
    ///
    /// Out-of-range is ignored rather than clamped: the only way to get one is a list that
    /// changed under the user (a nav mid-popover), and silently selecting *a different
    /// track than the one clicked* is the worst available answer.
    pub fn select_subtitle_row(&mut self, row: usize) {
        let Some(item) = self.displayed_item.filter(|_| self.video_showing()) else {
            return;
        };
        let Some(catalog) = self.exif_cache.get(&item).and_then(|d| d.media.as_ref()) else {
            return;
        };
        let Some(&next) = crate::subtitle::picker_choices(catalog).get(row) else {
            return;
        };
        self.apply_subtitle_choice(next);
    }

    pub fn tick_subtitles(&mut self) {
        self.subtitles.poll(self.details_gen);

        // Every "nothing should be on screen" case leaves through HERE, and this exit
        // always clears. Off used to get its own early `return` — which skipped the clear,
        // so pressing `C` left the last cue frozen on screen forever. One exit, one rule:
        // an overlay can never outlive the state that produced it.
        let (displayed, active) = (self.displayed_item, self.video_showing());
        let on = self.subtitles.selection.enabled;
        // `|| always_forced` (task #99): with dialogue subtitles off we must still get far
        // enough to look for a *forced* track, because forced signs are part of the film —
        // `resolve_display` is what decides, and it can't decide from behind this gate.
        let forced = self.subtitles.selection.always_forced;
        let Some(item) = displayed.filter(|_| active && (on || forced)) else {
            self.subtitles.trace(|| {
                format!(
                    "idle: displayed_item={displayed:?} session_active={active} on={on} \
                     always_forced={forced}"
                )
            });
            self.subtitles.clear_item();
            return;
        };
        // Cost of `always_forced` being on by default, stated honestly: a video now pays the
        // ~20 ms header probe below and the one-per-session 261 ms rasterizer build even with
        // subtitles off. Both are off-thread and behind `video_showing()`, so **the photo path
        // is untouched** — which is the property that actually matters here. Turning the
        // setting off restores "off costs nothing" exactly.
        //
        // The track catalog is the Details probe's, and it is what holds the container's
        // own subtitle streams *and* the sidecars beside it in one id namespace — so
        // subtitles cannot select anything until it lands. Drive the probe rather than
        // waiting for someone to open the Inspector: idempotent, guarded by its own
        // `Loading` placeholder, ~20 ms of container header, and only ever reached with
        // subtitles on and a video playing.
        self.ensure_exif_cached(item);
        let source = Arc::clone(&self.source);
        let deck_gen = self.details_gen;
        // Split the borrow: `ensure_loaded` needs `&mut subtitles` while it reads the
        // catalog out of `exif_cache`, and they are disjoint fields.
        let Self {
            subtitles,
            exif_cache,
            ..
        } = self;
        let catalog = exif_cache.get(&item).and_then(|d| d.media.as_ref());
        let audio_lang = catalog.and_then(crate::subtitle::audio_language_of);
        subtitles.ensure_loaded(&source, item, deck_gen, catalog, audio_lang);

        let Some((x, y, w, h, _rot)) = self.video_placement() else {
            self.subtitles
                .trace(|| "no video_placement — the still geometry isn't up yet".into());
            // Hide, don't just leave: with no geometry there is nowhere correct to draw,
            // and a kept overlay would hang at its last position — the same defect the
            // exit above had.
            self.subtitles.hide();
            return;
        };
        let Some(t) = self.video_position() else {
            self.subtitles
                .trace(|| "no playhead yet — the shell hasn't reported one".into());
            self.subtitles.hide();
            return;
        };
        let vp = (self.viewport.width as f32, self.viewport.height as f32);
        let video = crate::subtitle::Rect { x, y, w, h };
        // `controls_h` is 0 until a shell reports its transport bar's height — the lift
        // exists in `place()`, nothing measures it yet.
        //
        // No redraw is requested on a change: a playing video already draws every frame,
        // which is the only state this runs in.
        self.subtitles
            .update(t, vp, video, 0.0, self.viewport.scale_factor);
    }

    /// The active session's duration in seconds; `0.0` when unknown/none (the
    /// scrubber renders duration-less streams without a bar, like the native path).
    pub fn video_session_duration_secs(&self) -> f64 {
        match self.session_ref() {
            Some(v) if self.video_session_active() => {
                v.session.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0)
            }
            _ => 0.0,
        }
    }

    /// Whether the displayed item's video is playing right now (session
    /// `Playing`). The winit shell's playback row uses it two ways: the
    /// play/pause button's glyph, and a timed egui repaint so the knob glides
    /// between the once-a-second text refreshes; anything paused/parked keeps
    /// the overlay fully retained.
    pub fn video_playing(&self) -> bool {
        self.video
            .as_ref()
            .is_some_and(|v| Some(v.item()) == self.displayed_item && v.is_playing())
    }

    /// Keep the info line's playback row in step with the session (task #79):
    /// refresh the line only when the displayed second (or the row's presence)
    /// changes — once per second while playing, never per frame. A natively-drawn
    /// line (the winit egui overlay, macOS) gets a panels-changed marker so the
    /// shell re-pulls; the HUD path re-rasterizes directly.
    pub fn update_video_progress(&mut self) {
        let desired = self
            .video_progress_row()
            .map(|r| (r.elapsed, r.total.unwrap_or_default()))
            .map(|(a, b)| format!("{a}/{b}"));
        if desired == self.video_pill_text {
            return;
        }
        self.video_pill_text = desired;
        if !self.info_line_visible() {
            return;
        }
        if self.native_info {
            self.emit_panels_changed(); // the shell re-renders its info line
        } else {
            self.show_info_line(); // re-raster with (or without) the playback row
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
            ToastIcon::Captions => Some(icon::assets::CAPTIONS),
            ToastIcon::CaptionsOff => Some(icon::assets::CAPTIONS_SLASH),
            // The winit HUD has no dedicated audio-track glyph; the speaker reads correctly
            // for "you're now hearing a different track" and the set already carries it.
            ToastIcon::AudioTrack => Some(icon::assets::VOLUME),
            ToastIcon::AudioTrackFailed => Some(icon::assets::VOLUME_SLASH),
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
        // Epoch-aware so the pie also shows while a same-index frame is being re-decoded
        // at a new fit (resize / scale-mode). Live video re-resolves every frame
        // (`present_video_frame`), so it stays caught-up and never shows the pie.
        //
        // Also keep the pie up while the on-screen photo is only a **preview** and its full
        // decode is still coming (#106.5): preview-first paints an instant blurry thumbnail, so
        // `target_caught_up` flips true immediately — but without the pie a slow big-photo open
        // looks *finished* at the blurry stage, and the owner's fear is a user seeing soft
        // photos, thinking "these are terrible," and deleting them before they sharpen.
        // `sharpen_now()` is exactly this state: parked, displayed is a resident preview not yet
        // upgraded (and it is `None` while blazing and once the full lands, so the pie doesn't
        // spin during a fast blaze or after sharpening).
        let not_ready = self.target_pending() || self.sharpen_now().is_some();
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
        // PB_DOOR_DIAG=1: the core's door belief this frame, next to the renderer's draw
        // source (logged in `render` above). "card over a photo" = door_presented=true here
        // but the renderer drew Held/Single. `door_card` allocates, so only build it when on.
        if door_diag() {
            let di = self.displayed_item;
            // Source identity is the tell: inside album.zip the source is its 8 photos and
            // `door_card` is None (a stale card => shell overlay bug); if the source is the
            // parent folder with `displayed` back on the archive door, `door_card` names the
            // archive (=> the deck/source was swapped back, a core bug). `name`/`door_card`
            // allocate — gated, so off the hot path.
            eprintln!(
                "[door-diag] draw displayed={di:?} name={:?} src_len={} archive_scope={} archive_kind={:?} presented_epoch={:?} epoch={} door_presented={} door_card={:?}",
                di.map(|i| self.source.name(i)),
                self.source.len(),
                self.archive_scope.is_some(),
                di.and_then(|i| self.item_archive_kind(i)),
                self.presented_epoch,
                self.epoch,
                self.door_presented(),
                self.door_card().map(|c| c.name),
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

    /// A toolbar nav/random button was pressed and **held**: begin hold-to-blaze for `action`,
    /// reusing the exact keyboard path — the initial tap advance (or pie-glow while catching
    /// up) plus the self-paced blaze timer. `end_pointer_nav` (mouse-up) stops it. A quick click
    /// is just begin→end with no blaze, i.e. a single advance, matching a Space tap.
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

    /// The held toolbar nav/random button was released — stop blazing.
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

    /// Whether the image overflows the viewport **horizontally** — the condition
    /// under which the horizontal pan keys keep panning during video playback
    /// instead of seeking (task #79 phase 6: pan wins when zoomed).
    pub fn pannable_horizontally(&self) -> bool {
        self.screen_and_image()
            .map(|(iw, ih, sw, sh)| self.view.pannable_axes(iw, ih, sw, sh)[0])
            .unwrap_or(false)
    }

    /// Whether the image currently overflows the viewport (so panning does
    /// something). Drives the grab-hand cursor affordance. Uses the same
    /// rounding deadzone as the seek-vs-pan choice, so a sub-pixel fit overflow
    /// doesn't show a misleading grab hand on an image that effectively fits.
    pub fn pannable(&self) -> bool {
        self.screen_and_image()
            .map(|(iw, ih, sw, sh)| {
                let p = self.view.pannable_axes(iw, ih, sw, sh);
                p[0] || p[1]
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
                let rate = ZOOM_MIN_RATE
                    + (ZOOM_MAX_RATE - ZOOM_MIN_RATE) * crate::engine::hold_ramp(t, ZOOM_RAMP_SECS);
                // Exponential (multiplicative) zoom about the screen center.
                self.view.zoom =
                    (self.view.zoom * (rate * dir * dt).exp()).clamp(MIN_ZOOM, MAX_ZOOM);
                // #124: cheap -- `reconcile_zoom_rep` returns immediately unless the
                // representation decision actually flips, so a held ramp rebinds once on the
                // way past 1:1 rather than every tick.
                self.reconcile_zoom_rep();
                changed = true;
            }
            None => {
                self.zoom_started = None;
                self.zoom_last = None;
            }
        }

        // Contextual seek (task #79 phase 6): while a video plays and the image has
        // no horizontal overflow to pan, the horizontal pan actions seek instead
        // (±2 s, Shift ±10 s, self-timed repeat — OS key-repeat stays ignored).
        // Pan wins when zoomed with horizontal overflow; vertical pan is untouched.
        let (raw_px, py) = self.pan_held();
        let mut px = raw_px;
        if raw_px != 0.0 && self.video.is_some() && !self.pannable_horizontally() {
            let due = self
                .video_seek_last
                .is_none_or(|t| now.saturating_duration_since(t) >= VIDEO_SEEK_REPEAT);
            if due {
                self.video_seek_last = Some(now);
                self.video_seek(raw_px > 0.0); // PanLeft (+1) = seek backward
            }
            px = 0.0; // consumed — never also pans
            changed = true; // keep ticking so the held repeat fires
        } else {
            self.video_seek_last = None;
        }
        if px != 0.0 || py != 0.0 {
            let start = *self.pan_started.get_or_insert(now);
            let last = self.pan_last.replace(now).unwrap_or(start);
            let dt = (now - last).as_secs_f32().min(0.1);
            let t = (now - start).as_secs_f32();
            let speed = PAN_MIN_SPEED
                + (PAN_MAX_SPEED - PAN_MIN_SPEED) * crate::engine::hold_ramp(t, PAN_RAMP_SECS);
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
    /// only ever reached when settled on a photo, never on the blaze-through path. Always
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
            // Only image items Live-pair (task #79): a *video* item with a same-stem
            // .mov sibling (IMG_1.MP4 + IMG_1.MOV) is two videos, not a Live Photo.
            let paired = match crate::video::item_kind(self.source.as_ref(), item) {
                crate::video::LibraryItemKind::Image => {
                    self.source.path(item).and_then(companion_motion)
                }
                // Neither a video nor a door Live-pairs: a door is not half of
                // anything, and a same-stem .mov beside `holiday.zip` is unrelated.
                crate::video::LibraryItemKind::Video(_)
                | crate::video::LibraryItemKind::Archive(_) => None,
            };
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
    /// toolbar dims its Play button on stills (task #55). Includes **video** (task 79.9):
    /// a video is playable motion too, so the toolbar Play button enables on it. `&mut`
    /// because Live-Photo pairing is resolved + cached on first check (cheap cache hit
    /// after; the display path has usually primed it already).
    pub fn current_has_motion(&mut self) -> bool {
        self.displayed_item
            .is_some_and(|i| self.has_motion(i) || self.item_is_video(i))
    }

    /// Whether an animation / Live Photo is actively playing — the toolbar lights its
    /// Play-Animation button while it runs.
    pub fn animation_playing(&self) -> bool {
        self.playback.as_ref().is_some_and(|pb| pb.is_playing())
    }

    /// Whether *any* motion is playing — an animation/Live Photo **or** a video (task
    /// 79.9). The toolbar's Play/Pause glyph reads this so it reflects a playing video,
    /// not just an animation (`animation_playing` is video-blind by design).
    pub fn motion_playing(&self) -> bool {
        self.animation_playing() || self.video_playing()
    }

    /// The **displayed** photo's 1-based position and total count, for the toolbar counter
    /// (task #61) — mirrors the window title's `(idx+1/n)` (`title_for`). Derived from
    /// [`displayed_item`](Self::displayed_item), the *present-truth* index, **not** the nav
    /// target: during a resident-ring miss the target advances while the old photo is still
    /// on screen, so a target-based counter would lie. `None` until the first image is
    /// presented (the counter hides on a cold start / empty deck).
    pub fn display_counter(&self) -> Option<(usize, usize)> {
        self.displayed_item.map(|i| (i + 1, self.source.len()))
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
        self.cancel_anim_stream();
    }

    /// Signal any in-flight streaming Live Photo decode (task #69) to stop and drop it — the
    /// worker checks the flag per packet and bails. Called on navigate/supersede (via
    /// [`cancel_anim_decode`](Self::cancel_anim_decode)) so streams don't pile up.
    pub fn cancel_anim_stream(&mut self) {
        if let Some(s) = &self.anim_stream {
            s.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.anim_stream = None;
    }

    pub fn start_animation_decode(&mut self, item: usize, want: AnimWant) {
        // A Live Photo streams its motion (task #69) — play the `.mov` while it's still
        // decoding rather than waiting for the whole clip. Wired on every platform with a
        // motion decoder: the Linux FFmpeg path, the macOS AVAssetReader path, and the
        // Windows Media Foundation path. (GIF/APNG/WebP stay on the batch path below
        // everywhere — decoded from the still bytes.)
        #[cfg(any(
            target_os = "macos",
            windows,
            all(unix, not(target_os = "macos"), feature = "livephoto")
        ))]
        if self.live_motion_path(item).is_some() {
            self.start_live_stream(item, want);
            return;
        }
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

    /// Kick a **streaming** Live Photo motion decode (task #69) — FFmpeg on Linux,
    /// AVAssetReader on macOS, Media Foundation on Windows: the worker emits each frame as
    /// it's decoded (mapped onto the platform-neutral [`StreamMsg`]), and
    /// [`poll_anim_stream`](Self::poll_anim_stream) installs/extends the playing sequence so
    /// the clip starts within a frame or two instead of after the whole `.mov`. Same cancel /
    /// generation / epoch discipline as [`start_animation_decode`](Self::start_animation_decode).
    #[cfg(any(
        target_os = "macos",
        windows,
        all(unix, not(target_os = "macos"), feature = "livephoto")
    ))]
    pub fn start_live_stream(&mut self, item: usize, want: AnimWant) {
        // Supersede any in-flight decode/stream so its orphaned worker stops promptly.
        self.cancel_anim_decode();
        self.anim_gen += 1;
        let gen = self.anim_gen;
        let epoch = self.epoch;
        let Some(path) = self.live_motion_path(item) else {
            return;
        };
        // Cap the motion's long edge to the display fit (decode-to-fit), never above the RAM
        // ceiling — the same bound the batch `decode_motion_job` uses.
        let edge = self
            .decode_fit()
            .map(|f| f.max_width.max(f.max_height))
            .unwrap_or(crate::engine::MOTION_MAX_LONG_EDGE)
            .min(crate::engine::MOTION_MAX_LONG_EDGE);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_job = std::sync::Arc::clone(&cancel);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Map the decoder's chunks onto the neutral `StreamMsg` the core wiring consumes.
            let mut emit = |chunk: pb_decode::MotionChunk| {
                let msg = match chunk {
                    pb_decode::MotionChunk::Header(h) => StreamMsg::Header {
                        width: h.width,
                        height: h.height,
                        color: h.color,
                        codec: h.codec,
                    },
                    pb_decode::MotionChunk::Frame(f) => StreamMsg::Frame(f),
                    pb_decode::MotionChunk::Done {
                        loop_count,
                        truncated,
                    } => StreamMsg::Done {
                        loop_count,
                        truncated,
                    },
                    pb_decode::MotionChunk::Failed(e) => StreamMsg::Failed(e.to_string()),
                };
                let _ = tx.send(msg);
            };
            pb_decode::decode_live_motion_streaming(&path, edge, &cancel_job, &mut emit);
        });
        self.anim_stream = Some(crate::animation::AnimStream {
            gen,
            item,
            epoch,
            want,
            rx,
            cancel,
            header: None,
            pending: Vec::new(),
            installed: false,
        });
        if want != AnimWant::Eager {
            self.anim_hint_shown_for = self.displayed_item;
        }
    }

    /// When the user has rested on an animated still, eagerly decode the whole sequence
    /// in the background so pressing `P` is instant (fixes the slow first-play on WebP /
    /// AVIF, ~0.6–2s to decode). Returns the wake deadline while the dwell elapses (so
    /// the idle loop wakes to kick it), else `None`. Strictly off the hot path — only
    /// when settled (never while blazing), exactly when the prefetch pool is idle.
    pub fn maybe_prepare_animation(&mut self, now: Instant) -> Option<Instant> {
        if self.playback.is_some() || self.anim_decode.is_some() || self.anim_stream.is_some() {
            return None; // already playing, or a decode/stream is already in flight
        }
        let item = self.displayed_item?;
        if !self.target_caught_up() {
            return None; // still catching up to the target (incl. a geometry re-present) — not settled
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
        if let Some(slot) = self.display_slot(item) {
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

/// The platform's streaming video producer behind one name (task #84): the
/// Windows Media Foundation reader, or the FFmpeg producer everywhere else the
/// `ffvideo` feature reaches — ALL Linux video, and the macOS containers/codecs
/// AVFoundation refuses (§8a routing). Both speak the identical
/// `VideoProducerEvent`/`Msg` protocol, so `start_video_session` and the whole
/// `VideoSession` state machine are backend-blind.
#[cfg(any(windows, all(unix, feature = "ffvideo")))]
fn run_platform_video_producer(
    input: &crate::video::VideoInput,
    fit: Option<pb_decode::FitBox>,
    id: crate::video::VideoSessionId,
    generation: crate::video::SeekGeneration,
    events: std::sync::mpsc::Sender<crate::video::VideoProducerEvent>,
    msgs: std::sync::mpsc::Receiver<crate::video::VideoProducerMsg>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    options: pb_decode::VideoProducerOptions,
) {
    // The FFmpeg reader honors `cancel` via its interrupt callback (plan 1F). The
    // Windows MF reader has its own Stop/disconnect teardown and isn't wired to
    // this flag yet — its reads are local and don't block on network the way SMB
    // does; revisit if MF network sources need it. `options` now drives the MF
    // producer too: `supports_p010` (+ `planar`) selects the HDR P010 path
    // (task 79.10 Track B).
    #[cfg(windows)]
    {
        let _ = cancel;
        pb_decode::run_video_producer(input, fit, id, generation, events, msgs, options);
    }
    #[cfg(all(unix, feature = "ffvideo"))]
    pb_decode::run_ff_video_producer(input, fit, id, generation, events, msgs, cancel, options);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{CoreEvent, Modifiers};
    use crate::{PbKey, Viewport};

    /// A minimal `AppCore` for driving `handle` in tests — the public [`AppCore::headless`]
    /// constructor at a 1×1 viewport (one construction literal, shared with the NS1 FFI bridge).
    /// **Regression.** `new_host` builds the core from *default* settings and only then
    /// loads the real ones off disk, hand-copying the derived state across. The subtitle
    /// engine was left out, so `subtitles = true` on disk launched with captions off —
    /// which reads exactly like the preference never saved.
    ///
    /// A headless core can't call `new_host` (it would read the user's real config), so
    /// this pins the derivation itself: whatever the settings say, the engine must agree.
    #[test]
    fn the_engine_mode_is_derived_from_the_loaded_settings_not_the_defaults() {
        use crate::subtitle::SubtitleWant;
        let mut core = test_core();
        assert!(!core.subtitles.selection.enabled);

        // What `new_host` does after `Settings::load()` returns captions-on.
        let loaded = settings::Settings {
            subtitles: true,
            ..Default::default()
        };
        core.subtitles = crate::subtitle_engine::SubtitleEngine::from_settings(&loaded);
        core.settings = loaded;

        assert_eq!(
            core.subtitles.selection.want,
            SubtitleWant::Automatic,
            "the engine must follow the settings that were actually loaded"
        );
    }

    /// A core with a live **Native** backend on item 0 — the macOS sample-buffer /
    /// AVPlayer shape, which is what MKV and WebM actually take since Phase 3F.
    fn core_with_a_native_video() -> AppCore {
        let mut core = test_core();
        let sid = pb_decode::VideoSessionId(7);
        let mut proxy = crate::video_native::NativeVideoProxy::new(0, sid, false);
        proxy.on_state_changed(sid, crate::video::VideoSessionState::Playing);
        core.video = Some(crate::video_native::ActiveVideoBackend::Native(proxy));
        core.displayed_item = Some(0);
        core
    }

    /// **Regression.** Subtitles gated on `video_session_active()`, which is false for the
    /// Native backend. When the macOS sample-buffer route became the default for MKV/WebM,
    /// that silently turned subtitles off for exactly the files they were built for — no
    /// error, no failing test, just nothing on screen.
    ///
    /// "Is a video on screen" must not depend on which backend is drawing it.
    #[test]
    fn a_native_backed_video_counts_as_showing() {
        let core = core_with_a_native_video();
        assert!(
            core.video_showing(),
            "the sample-buffer / AVPlayer route is still a video playing"
        );
        assert!(
            !core.video_session_active(),
            "and it is NOT session-backed — which is exactly why the old check failed"
        );
    }

    /// The playhead has to come from whichever backend is live. On Native the shell owns
    /// the clock and reports it ~20 Hz; before the first report there is simply no answer,
    /// and inventing one would put cues out of step with the picture.
    #[test]
    fn the_native_playhead_comes_from_the_shells_reports() {
        let mut core = core_with_a_native_video();
        assert_eq!(core.video_position(), None, "no report yet — no answer");

        core.native_video_progress(7, 12.5, 100.0);
        assert_eq!(core.video_position(), Some(Duration::from_secs_f64(12.5)));

        core.native_video_progress(7, 13.0, 100.0);
        assert_eq!(core.video_position(), Some(Duration::from_secs_f64(13.0)));
    }

    /// A report from a torn-down player must never move the live one's clock — the same
    /// session-identity rule every other native callback follows.
    #[test]
    fn a_stale_sessions_progress_does_not_move_the_playhead() {
        let mut core = core_with_a_native_video();
        core.native_video_progress(7, 12.5, 100.0);
        core.native_video_progress(999, 88.0, 100.0); // a straggler from a dead session
        assert_eq!(
            core.video_position(),
            Some(Duration::from_secs_f64(12.5)),
            "a straggler must not be believed"
        );
    }

    /// A core with a live `VideoSession` on item 0 — the state `tick_subtitles` only
    /// does real work in, and the state the switched-off bug needed to appear.
    fn core_with_a_playing_video() -> AppCore {
        let mut core = test_core();
        let (session, _io) =
            crate::video_session::VideoSession::new(pb_decode::VideoSessionId(1), 1 << 20);
        core.video = Some(crate::video_native::ActiveVideoBackend::Session(
            crate::video_session::ActiveVideo::new(session, 0),
        ));
        core.displayed_item = Some(0);
        // Leak the producer end: dropping it would fail the session, and this core never
        // decodes anything — it exists to make `video_session_active()` true.
        std::mem::forget(_io);
        assert!(
            core.video_session_active(),
            "the fixture must actually be active"
        );
        core
    }

    /// **Regression.** Pressing `C` with a cue on screen left it frozen there forever.
    ///
    /// `update()` hides correctly when the mode is Off — and a unit test proved it. But
    /// the tick had its own `if Off { return }` fast path that never called `update()`, so
    /// the bitmap and its generation just sat there and the shell kept drawing the last
    /// cue. The test passed; the feature was broken. This one drives `tick_subtitles`,
    /// which is where the bug actually lived.
    #[test]
    fn switching_subtitles_off_clears_a_cue_that_is_on_screen() {
        use crate::subtitle::SubtitleSelection;
        let mut core = core_with_a_playing_video();
        core.subtitles.selection = SubtitleSelection::automatic();
        core.subtitles.force_showing_for_test();
        let before = core.subtitles.gen();

        core.subtitles.selection = SubtitleSelection::off();
        core.tick_subtitles();

        assert!(
            core.subtitles.bitmap().is_none(),
            "the last cue must not survive being switched off"
        );
        assert!(
            core.subtitles.gen() > before,
            "the shell only stops drawing when the generation moves"
        );
    }

    /// The same rule for the other way out: a video that stops must not leave its last cue
    /// hanging over whatever is on screen next.
    #[test]
    fn a_stale_overlay_is_cleared_when_nothing_is_playing() {
        let mut core = test_core(); // no session
        core.subtitles.selection = crate::subtitle::SubtitleSelection::automatic();
        core.subtitles.force_showing_for_test();
        core.tick_subtitles();
        assert!(core.subtitles.bitmap().is_none());
    }

    /// `C` / View ▸ Subtitles flips the engine's mode, records the preference, and says
    /// which way it went — the whole switch, replacing the old dev env flag.
    #[test]
    fn toggle_subtitles_flips_the_mode_and_the_preference() {
        let mut core = test_core();
        // The native toast carries the message + icon as data; the CPU one needs a HUD
        // rasterizer a headless core has no reason to build.
        core.native_toast = true;
        assert!(!core.subtitles.selection.enabled, "off by default");
        assert!(!core.settings.subtitles);

        core.dispatch_action(Action::ToggleSubtitles);
        assert!(core.subtitles.selection.enabled);
        assert!(core.settings.subtitles, "the preference records the choice");
        let t = core.toast_native.as_ref().expect("the user is told");
        assert_eq!(
            (t.message.as_str(), t.icon),
            ("Subtitles on", ToastIcon::Captions)
        );

        core.dispatch_action(Action::ToggleSubtitles);
        assert!(!core.subtitles.selection.enabled);
        assert!(!core.settings.subtitles);
        let t = core.toast_native.as_ref().expect("told again");
        assert_eq!(
            (t.message.as_str(), t.icon),
            ("Subtitles off", ToastIcon::CaptionsOff),
            "the toast must say which way it went, not just that it changed"
        );
    }

    // -- the track picker (#99) ---------------------------------------------

    /// A subtitle track on the catalog the picker reads.
    fn sub(local_id: u64, lang: &str) -> pb_decode::MediaTrack {
        pb_decode::MediaTrack {
            id: pb_decode::TrackId {
                catalog_generation: 1,
                local_id,
            },
            kind: pb_decode::TrackKind::Subtitle,
            language: Some(lang.into()),
            title: None,
            codec_raw: "subrip".into(),
            codec: "SubRip".into(),
            capability: pb_decode::TrackCapability::SupportedText,
            flags: pb_decode::TrackFlags::none(),
            audio: None,
            external: false,
        }
    }

    /// A playing MKV whose catalog carries `subs`.
    fn core_with_subtitle_tracks(subs: Vec<pb_decode::MediaTrack>) -> AppCore {
        let mut core = core_with_a_native_video();
        core.native_toast = true;
        let catalog = pb_decode::MediaTrackCatalog::new(
            1,
            pb_decode::MediaBackend::FFmpeg,
            pb_decode::TrackSet::complete(vec![track("AAC", "eng")]),
            pb_decode::TrackSet::complete(subs),
        );
        seed_details(&mut core, 0, Some(catalog), Some(true));
        core
    }

    fn labels(core: &mut AppCore) -> Vec<String> {
        core.subtitle_picker_rows()
            .iter()
            .map(|r| format!("{}{}", if r.active { "✓ " } else { "" }, r.label))
            .collect()
    }

    // -- A / Shift+A audio cycling (#99) ------------------------------------

    /// A playing video whose catalog carries `n` audio tracks.
    fn core_with_audio_tracks(n: u64) -> AppCore {
        let mut core = core_with_a_native_video();
        core.native_toast = true;
        let tracks: Vec<pb_decode::MediaTrack> = (0..n)
            .map(|i| {
                let mut t = track("AAC", if i == 0 { "eng" } else { "fra" });
                t.id.local_id = i;
                t
            })
            .collect();
        let catalog = pb_decode::MediaTrackCatalog::new(
            1,
            pb_decode::MediaBackend::FFmpeg,
            pb_decode::TrackSet::complete(tracks),
            pb_decode::TrackSet::complete(vec![]),
        );
        seed_details(&mut core, 0, Some(catalog), Some(true));
        core
    }

    fn selected_audio_rows(core: &mut AppCore) -> Vec<usize> {
        core.effects
            .drain(..)
            .filter_map(|e| match e {
                crate::contract::CoreEffect::SelectAudioTrack { row } => Some(row),
                _ => None,
            })
            .collect()
    }

    /// **The core cannot switch audio — only ask for it.** The player is the shell's, the
    /// two routes reach a track by different locators, and the switch can be refused. So the
    /// key emits an effect and nothing else; the shell reports the outcome back through the
    /// same path the menu uses.
    #[test]
    fn audio_cycling_asks_the_shell_rather_than_switching() {
        let mut core = core_with_audio_tracks(3);
        core.set_active_audio_row(0);

        core.dispatch_action(Action::AudioNext);
        assert_eq!(selected_audio_rows(&mut core), vec![1], "asked for row 1");
        assert!(
            core.toast_native.is_none(),
            "and said NOTHING — the toast is the shell's to trigger once the switch is real"
        );
    }

    /// Steps wrap in both directions, from **what is playing** rather than a remembered ask.
    #[test]
    fn audio_cycling_steps_from_what_is_playing_and_wraps() {
        let mut core = core_with_audio_tracks(3);

        core.set_active_audio_row(2); // the shell reports the last track
        core.dispatch_action(Action::AudioNext);
        assert_eq!(selected_audio_rows(&mut core), vec![0], "wraps forward");

        core.set_active_audio_row(0);
        core.dispatch_action(Action::AudioPrev);
        assert_eq!(selected_audio_rows(&mut core), vec![2], "wraps backward");

        core.set_active_audio_row(1);
        core.dispatch_action(Action::AudioPrev);
        assert_eq!(selected_audio_rows(&mut core), vec![0]);
    }

    /// Before the shell has reported anything, start at the first track rather than guess
    /// which one the decoder's policy chose.
    #[test]
    fn audio_cycling_with_nothing_reported_starts_at_the_first() {
        let mut core = core_with_audio_tracks(2);
        assert!(core.audio_active.is_none());
        core.dispatch_action(Action::AudioNext);
        assert_eq!(selected_audio_rows(&mut core), vec![0]);
    }

    /// One track is not a rotation. Say so — a key that silently does nothing is
    /// indistinguishable from a key that is broken.
    #[test]
    fn audio_cycling_a_single_track_says_so_instead_of_no_oping() {
        let mut core = core_with_audio_tracks(1);
        core.dispatch_action(Action::AudioNext);
        assert!(
            selected_audio_rows(&mut core).is_empty(),
            "asks for nothing"
        );
        let t = core.toast_native.as_ref().expect("the user is told");
        assert_eq!(t.message, "Only one audio track");
    }

    /// The Windows (WASAPI) currency accessors (task #99): each row resolves in
    /// whichever currency its locator carries, a stream resolves back to its row, and a
    /// row without a locator in the asked currency answers `-1` — the shell's cue to try
    /// the other currency or refuse. Both currencies coexist because the engine takes
    /// either (FFmpeg's own catalog carries `FfStream`; MF's fallback catalog `MfStream`).
    #[test]
    fn audio_stream_accessors_round_trip_in_both_currencies() {
        let mut core = core_with_a_native_video();
        let mut a0 = track("AAC", "eng");
        a0.id.local_id = 1;
        let mut a1 = track("AC-3", "fra");
        a1.id.local_id = 2;
        let mut catalog = pb_decode::MediaTrackCatalog::new(
            1,
            pb_decode::MediaBackend::FFmpeg,
            pb_decode::TrackSet::complete(vec![a0, a1]),
            pb_decode::TrackSet::complete(vec![]),
        );
        // Row 0 is MF-located (the fallback-catalog shape), row 1 FFmpeg-located.
        catalog.set_locator(1, pb_decode::tracks::TrackLocator::MfStream(3));
        catalog.set_locator(2, pb_decode::tracks::TrackLocator::FfStream(2));
        seed_details(&mut core, 0, Some(catalog), Some(true));

        assert_eq!(core.audio_row_mf_stream(0), 3);
        assert_eq!(core.audio_row_mf_stream(1), -1, "no MF twin → refuse");
        assert_eq!(core.audio_row_mf_stream(9), -1, "out of range → refuse");

        assert_eq!(core.audio_row_for_mf_stream(3), 0);
        assert_eq!(core.audio_row_for_mf_stream(9), -1);
        assert_eq!(core.audio_row_for_ff_stream(2), 1);
        assert_eq!(core.audio_row_for_ff_stream(0), -1);
    }

    /// The flyout must list tracks over the **poster** too (owner, 2026-07-17): the
    /// catalog belongs to the item, not to a session, so a video that isn't playing
    /// still offers its tracks — gating on `video_showing()` claimed "No Video" over
    /// a film sitting at its poster.
    #[test]
    fn audio_rows_show_for_a_displayed_video_without_a_session() {
        let mut core = test_core();
        core.source = Arc::new(FsSource::new(vec![PathBuf::from("films/movie.mkv")]));
        core.displayed_item = Some(0);
        assert!(core.video.is_none(), "no session in this test");
        let mut a0 = track("AAC", "eng");
        a0.id.local_id = 1;
        let mut a1 = track("AC-3", "fra");
        a1.id.local_id = 2;
        let catalog = pb_decode::MediaTrackCatalog::new(
            1,
            pb_decode::MediaBackend::FFmpeg,
            pb_decode::TrackSet::complete(vec![a0, a1]),
            pb_decode::TrackSet::complete(vec![]),
        );
        seed_details(&mut core, 0, Some(catalog), Some(true));

        assert!(core.displayed_is_video());
        assert!(core.audio_tracks_known());
        assert_eq!(core.audio_picker_rows().len(), 2);
        // ...and a still keeps offering nothing.
        core.source = five_photos();
        assert!(!core.displayed_is_video());
        assert!(core.audio_picker_rows().is_empty());
    }

    /// A still is not a video: the key says nothing rather than lying about a photo.
    #[test]
    fn audio_cycling_on_a_still_is_silent() {
        let mut core = test_core();
        core.native_toast = true;
        core.displayed_item = Some(0);
        core.dispatch_action(Action::AudioNext);
        assert!(selected_audio_rows(&mut core).is_empty());
        assert!(core.toast_native.is_none());
    }

    /// **Regression — the silent one.** A `TrackId`'s whole contract is that `local_id` means
    /// something only within the generation it carries. The probe was handed the **deck**
    /// generation, which every film in a folder shares, so an id minted on one film compared
    /// equal against the next film's catalog and matched whatever stream sat at that
    /// `local_id`: a subtitle track picked as Arabic came back as **Korean** on the next
    /// episode — ticked, no error, no way to tell. `resolve_track`'s stale guard could never
    /// fire, because the two generations were the same number.
    ///
    /// Each catalog must mint its own generation, and the deck's must stay a separate count.
    /// (Verified to fail without the fix: both sides read 0.)
    #[test]
    fn each_probe_mints_its_own_catalog_generation() {
        let mut core = test_core();
        core.source = five_photos();

        core.probe_details_blocking(0);
        let first = core.catalog_seq;
        core.probe_details_blocking(1);
        let second = core.catalog_seq;

        assert_ne!(
            first, second,
            "two files in one deck must not share a catalog generation — that is the bug"
        );
        assert!(second > first, "and it advances rather than cycling");
        // The deck did not change, so its own generation must not have moved: conflating
        // these two counts is exactly what caused the defect.
        assert_eq!(
            core.details_gen, 0,
            "the deck generation is a separate question"
        );
    }

    /// The picker lists Off, Automatic, then the file's tracks; selecting a row applies it.
    #[test]
    fn selecting_a_picker_row_applies_that_track() {
        use crate::subtitle::SubtitleWant;
        let mut core = core_with_subtitle_tracks(vec![sub(0, "eng"), sub(1, "fra")]);
        assert_eq!(
            labels(&mut core),
            vec!["✓ Off", "Automatic", "English · SubRip", "French · SubRip"]
        );

        core.select_subtitle_row(3); // French
        assert!(core.subtitles.selection.enabled);
        assert_eq!(
            core.subtitles.selection.want,
            SubtitleWant::Track(pb_decode::TrackId {
                catalog_generation: 1,
                local_id: 1
            }),
        );
        assert!(core.settings.subtitles, "the preference follows the choice");
        // The tick moves with it, and the toast names what was picked.
        assert_eq!(
            labels(&mut core),
            vec!["Off", "Automatic", "English · SubRip", "✓ French · SubRip"]
        );
        let t = core.toast_native.as_ref().expect("the user is told");
        assert_eq!(
            (t.message.as_str(), t.icon),
            ("French · SubRip", ToastIcon::Captions)
        );

        // ...and row 0 is a real way back to off.
        core.select_subtitle_row(0);
        assert!(!core.subtitles.selection.enabled);
        assert!(!core.settings.subtitles);
        assert_eq!(labels(&mut core)[0], "✓ Off");
    }

    /// **The owner's bug, pinned** (2026-07-15, on Ad Astra): pick Chinese, press `C` twice,
    /// and English came back. `C` set the mode to `Automatic` on the way back on, because
    /// one enum held both "are subtitles on" and "which one" — so turning them off *had* to
    /// destroy the choice.
    ///
    /// `C` must flip exactly one of those and leave the other alone.
    #[test]
    fn c_toggles_without_forgetting_the_picked_track() {
        use crate::subtitle::SubtitleWant;
        let mut core = core_with_subtitle_tracks(vec![sub(0, "eng"), sub(1, "zho")]);
        core.select_subtitle_row(3); // Chinese — not what Automatic would choose
        let picked = core.subtitles.selection.want;
        assert!(matches!(picked, SubtitleWant::Track(_)));

        core.dispatch_action(Action::ToggleSubtitles); // off
        assert!(!core.subtitles.selection.enabled);
        assert_eq!(
            core.subtitles.selection.want, picked,
            "off must not destroy the choice"
        );

        core.dispatch_action(Action::ToggleSubtitles); // on again
        assert!(core.subtitles.selection.enabled);
        assert_eq!(
            core.subtitles.selection.want, picked,
            "and on must bring back what was picked, not Automatic"
        );
        assert_eq!(
            labels(&mut core),
            vec!["Off", "Automatic", "English · SubRip", "✓ Chinese · SubRip"],
            "Chinese is back on screen — this is the bug the owner reported"
        );
    }

    /// **The point of the shared list.** `Shift+C` steps through *tracks* — the very rows the
    /// picker draws, in its order. Off is `C`'s job and Automatic is not a step (it resolves
    /// to one of these tracks, so it would show the same subtitles twice under two names).
    #[test]
    fn shift_c_walks_the_pickers_track_rows() {
        let mut core = core_with_subtitle_tracks(vec![sub(0, "eng"), sub(1, "fra")]);
        let rows: Vec<String> = core
            .subtitle_picker_rows()
            .iter()
            .map(|r| r.label.clone())
            .collect();

        // From off, three presses: first track, second track, wrap to the first.
        let mut visited = Vec::new();
        for _ in 0..3 {
            core.dispatch_action(Action::SubtitleCycle);
            let ticked = core
                .subtitle_picker_rows()
                .into_iter()
                .find(|r| r.active)
                .expect("something is always ticked");
            visited.push(ticked.label);
        }
        assert_eq!(
            visited,
            vec![rows[2].clone(), rows[3].clone(), rows[2].clone()],
            "the track rows, in the picker's order, wrapping"
        );
        assert!(
            core.subtitles.selection.enabled,
            "asking for the next track when they're off can only mean you want to see one"
        );
    }

    /// An index that no longer exists (the list changed under an open popover) must be
    /// ignored — never clamped onto a neighbouring track the user did not click.
    #[test]
    fn an_out_of_range_row_selects_nothing() {
        let mut core = core_with_subtitle_tracks(vec![sub(0, "eng")]);
        core.select_subtitle_row(99);
        assert!(!core.subtitles.selection.enabled, "unchanged");
        assert!(core.toast_native.is_none(), "and says nothing");
    }

    /// "Still reading the tracks" and "this file has none" are different answers, and an
    /// empty list must not be drawn as the second one.
    #[test]
    fn an_unprobed_video_is_not_the_same_as_a_video_with_no_tracks() {
        let mut core = core_with_a_native_video();
        assert!(core.subtitle_picker_rows().is_empty());
        assert!(!core.subtitle_tracks_known(), "the probe has not landed");

        let mut probed = core_with_subtitle_tracks(vec![]);
        assert!(probed.subtitle_tracks_known(), "it landed, and said none");
        assert_eq!(labels(&mut probed), vec!["✓ Off"], "just the Off row");
    }

    /// A still is not a video: the picker offers nothing rather than the last film's tracks.
    #[test]
    fn a_still_offers_no_picker_rows() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        assert!(core.subtitle_picker_rows().is_empty());
        assert!(!core.subtitle_tracks_known());
    }

    /// A test must never write the real settings.toml (the `persist_prefs` rule) — this
    /// toggle is dispatched by the test above, so pin that it stays gated.
    #[test]
    fn toggle_subtitles_does_not_persist_in_a_headless_core() {
        let core = test_core();
        assert!(
            !core.persist_prefs,
            "a headless core must not write the user's config"
        );
    }

    /// The saved preferences are what the engine starts in — a toggle that forgets across
    /// launches is the flag we just removed, wearing a menu item.
    ///
    /// The **style** half is the one worth pinning: appearance that silently reverts to
    /// the default on every launch would look exactly like "Settings didn't save", which
    /// is post-mortem bug #2 wearing a different hat.
    #[test]
    fn the_engine_starts_from_the_saved_preferences() {
        use crate::subtitle::SubtitleSelection;
        let mut s = settings::Settings {
            subtitles: true,
            ..Default::default()
        };
        s.subtitle_style.size_pct = 0.077;
        let e = crate::subtitle_engine::SubtitleEngine::from_settings(&s);
        assert!(e.selection.enabled);
        assert_eq!(e.style.size_pct, 0.077, "the saved style must come with it");

        s.subtitles = false;
        assert_eq!(
            crate::subtitle_engine::SubtitleEngine::from_settings(&s).selection,
            SubtitleSelection {
                always_forced: true, // the shipped default — forced signs are part of the film
                ..SubtitleSelection::off()
            }
        );

        // The forced preference must ride along too (task #99) — this is the field the
        // post-mortem-bug-#2 lesson is about: one that reaches the engine only at
        // construction saves to disk and does nothing, and the preference looks broken.
        s.forced_subtitles = false;
        let e = crate::subtitle_engine::SubtitleEngine::from_settings(&s);
        assert!(
            !e.selection.always_forced,
            "turning the setting off must reach the engine, not just the file"
        );
    }

    fn test_core() -> AppCore {
        AppCore::headless(Viewport {
            width: 1,
            height: 1,
            scale_factor: 1.0,
        })
    }

    /// A five-item source named `a.jpg`..`e.jpg` under a folder, for the launch-start tests.
    fn five_photos() -> Arc<dyn ItemSource> {
        Arc::new(FsSource::new(
            ["a", "b", "c", "d", "e"]
                .iter()
                .map(|n| PathBuf::from(format!("photos/{n}.jpg")))
                .collect(),
        ))
    }

    #[test]
    fn reinsert_after_restore_puts_the_photo_back_at_its_original_index() {
        // After deleting index 1 from [a,b,c,d] the live deck is [a,c,d]; undoing the delete
        // restores "b" at index 1, giving [a,b,c,d] again, and lands on it (so the "Restored …"
        // toast shows on the recovered photo).
        let mut core = test_core();
        let root = PathBuf::from("photos");
        let deck = |names: &[&str]| -> Arc<dyn ItemSource> {
            Arc::new(FsSource::new(
                names
                    .iter()
                    .map(|n| root.join(format!("{n}.jpg")))
                    .collect(),
            ))
        };
        core.rebuild_playlist(deck(&["a", "c", "d"]), root.clone(), None, false, 1);
        core.reinsert_after_restore(1, &root.join("b.jpg"));
        let paths: Vec<_> = (0..core.source.len())
            .map(|i| core.source.path(i).unwrap().to_path_buf())
            .collect();
        assert_eq!(
            paths,
            vec![
                root.join("a.jpg"),
                root.join("b.jpg"),
                root.join("c.jpg"),
                root.join("d.jpg"),
            ]
        );
        assert_eq!(core.displayed_item, Some(1));
    }

    #[test]
    fn reinsert_after_restore_clamps_a_now_out_of_range_index_to_the_end() {
        // The deck shrank since the delete, so the recorded original index no longer fits — the
        // restored photo is appended rather than lost or panicking.
        let mut core = test_core();
        let root = PathBuf::from("photos");
        let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![root.join("a.jpg")]));
        core.rebuild_playlist(src, root.clone(), None, false, 0);
        core.reinsert_after_restore(9, &root.join("z.jpg"));
        assert_eq!(core.source.len(), 2);
        assert_eq!(core.source.path(1).unwrap(), root.join("z.jpg").as_path());
        assert_eq!(core.displayed_item, Some(1));
    }

    #[test]
    fn launch_start_index_none_without_a_start_override() {
        let core = test_core();
        assert_eq!(core.launch_start_index(&*five_photos()), None);
    }

    #[test]
    fn launch_start_index_start_at_is_one_based_and_clamped() {
        let src = five_photos();
        let idx = |n| {
            let mut core = test_core();
            core.launch.start_at = Some(StartAt::Index(n));
            core.launch_start_index(&*src)
        };
        assert_eq!(idx(1), Some(0)); // 1-based → first
        assert_eq!(idx(3), Some(2));
        assert_eq!(idx(99), Some(4)); // clamps to the last
        assert_eq!(idx(0), Some(0)); // degenerate 0 → first
    }

    #[test]
    fn launch_start_index_start_at_name_matches_basename_case_insensitively() {
        let src = five_photos();
        let by_name = |name: &str| {
            let mut core = test_core();
            core.launch.start_at = Some(StartAt::Name(name.to_string()));
            core.launch_start_index(&*src)
        };
        assert_eq!(by_name("c.jpg"), Some(2));
        assert_eq!(by_name("C.JPG"), Some(2)); // case-insensitive
        assert_eq!(by_name("missing.jpg"), Some(0)); // not found → first
    }

    #[test]
    fn launch_start_index_reverse_starts_on_the_last_photo() {
        let mut core = test_core();
        core.launch.reverse = true;
        assert_eq!(core.launch_start_index(&*five_photos()), Some(4));
    }

    #[test]
    fn launch_start_index_shuffle_picks_an_in_range_photo() {
        let mut core = test_core();
        core.launch.shuffle = true;
        let idx = core.launch_start_index(&*five_photos());
        assert!(matches!(idx, Some(i) if i < 5));
    }

    #[test]
    fn launch_start_index_start_at_wins_over_shuffle_and_reverse() {
        let mut core = test_core();
        core.launch.start_at = Some(StartAt::Index(2));
        core.launch.shuffle = true;
        core.launch.reverse = true;
        assert_eq!(core.launch_start_index(&*five_photos()), Some(1));
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
            size: None,
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
            size: None,
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
            size: None,
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
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![dir.join("a.png")]));
        core.rebuild_playlist(source, dir.clone(), Some(dir.clone()), true, 0);
        assert_eq!(core.settings.last_folder.as_deref(), Some(dir.as_path()));

        // An archive-style rebuild (no scan_root) must not clobber the remembered folder.
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![dir.join("b.png")]));
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

        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(paths.clone()));
        core.rebuild_playlist(source, dir.clone(), Some(dir.clone()), true, 0);
        let first = core.playlist.shuffle().clone();

        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(paths));
        core.rebuild_playlist(source, dir.clone(), Some(dir.clone()), true, 0);
        let second = core.playlist.shuffle().clone();

        assert_ne!(
            first, second,
            "two opens of the same-size deck must not shuffle identically"
        );
    }

    #[test]
    fn a_rotation_undo_survives_a_same_root_rebuild_but_clears_on_a_new_deck() {
        // Regression: a SaveRotation undo entry used to be keyed by playlist *index*, so *any*
        // rebuild dropped it — deleting a photo after a Save Rotation silently wiped the
        // rotation-undo (the "rotate→save, delete, delete, Ctrl+Z ×3" report: the 3rd undo was
        // gone). Now every undo entry is path-keyed and survives a same-deck rebuild; only a
        // genuinely new deck (different root) clears the stack.
        let mut core = test_core();
        let dir = std::env::temp_dir();
        let paths: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("{i}.jpg"))).collect();
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(paths));
        core.rebuild_playlist(source, dir.clone(), Some(dir.clone()), true, 0);

        core.undo_stack.push(crate::undo::UndoAction::SaveRotation {
            path: dir.join("1.jpg"),
            prev: 1,
        });

        // A same-root rebuild — e.g. the advance after deleting a *different* photo — keeps it,
        // and the label still names the (path-resolved) file.
        let remaining: Arc<dyn ItemSource> =
            Arc::new(FsSource::new(vec![dir.join("0.jpg"), dir.join("1.jpg")]));
        core.rebuild_playlist(remaining, dir.clone(), Some(dir.clone()), true, 0);
        assert_eq!(
            core.undo_stack.len(),
            1,
            "a path-keyed rotation undo survives a same-root rebuild"
        );
        assert_eq!(core.undo_stack[0].menu_label(), "Undo Rotate 1.jpg");

        // A genuinely new deck (different root) clears the whole stack.
        let other = dir.join("pb_other_deck");
        let fresh: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![other.join("z.jpg")]));
        core.rebuild_playlist(fresh, other.clone(), Some(other), false, 0);
        assert!(
            core.undo_stack.is_empty(),
            "opening a new deck clears the undo stack"
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
            size: None,
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
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![dir.join("a.png")]));
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

    impl ItemSource for FakeArchive {
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
        let source: Arc<dyn ItemSource> = Arc::new(FakeArchive {
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
    fn a_stale_folder_scan_batch_never_extends_an_archive_deck() {
        // The cross-deck open race (Codex-diagnosed 2026-07-17): a folder-scan worker keeps
        // streaming after an *archive* open installs a new deck — the archive open doesn't
        // supersede the scan (different worker type). A late cumulative batch from that scan must
        // be DROPPED, never fed to `extend_playlist`, which would swap `self.source` back to the
        // folder while both GPU rings still hold the archive's textures for those indices: the
        // "title advances but the view is frozen, door card over a photo" corruption a resize heals.
        let mut core = test_core();
        let folder = PathBuf::from("/some/folder");
        let folder_src = |names: &[&str]| -> Arc<dyn ItemSource> {
            Arc::new(FsSource::new(
                names.iter().map(|n| folder.join(n)).collect(),
            ))
        };

        // 1. A folder scan bootstraps its deck.
        core.apply_scan_batch(crate::scan::Resolved {
            source: folder_src(&["a.jpg", "b.jpg"]),
            root: folder.clone(),
            scan_root: Some(folder.clone()),
            recursive: true,
            start: 0,
        });
        assert!(core.scan_bootstrapped, "first non-empty batch bootstraps");
        assert_eq!(core.source.len(), 2);

        // 2. An archive opens over it: a full rebuild onto the archive deck (scope stamped).
        let container = std::env::temp_dir().join("race.zip");
        let archive: Arc<dyn ItemSource> = Arc::new(FakeArchive {
            names: vec!["one.jpg".into(), "two.jpg".into(), "three.jpg".into()],
            container: container.clone(),
        });
        core.apply_archive(crate::scan::Resolved {
            source: archive,
            root: container,
            scan_root: None,
            recursive: false,
            start: 0,
        });
        assert!(
            core.archive_scope.is_some(),
            "we're on the archive deck now"
        );
        assert_eq!(core.source.len(), 3);

        // 3. The still-alive folder scan delivers a LARGER cumulative batch. It must be rejected —
        //    not extended over the archive.
        core.apply_scan_batch(crate::scan::Resolved {
            source: folder_src(&["a.jpg", "b.jpg", "c.jpg", "d.jpg"]),
            root: folder.clone(),
            scan_root: Some(folder),
            recursive: true,
            start: 0,
        });

        assert!(
            core.archive_scope.is_some(),
            "a stale folder batch must not clobber the archive scope"
        );
        assert_eq!(
            core.source.len(),
            3,
            "a stale folder batch must not extend the archive deck"
        );
        assert_eq!(core.source.name(0), "one.jpg", "still the archive's items");
    }

    #[test]
    fn a_matching_folder_scan_batch_still_extends_its_own_deck() {
        // The guard must not break the normal case: a later cumulative batch from the *same*
        // folder scan (no archive scope, matching `scan_root`) still grows the deck in place.
        let mut core = test_core();
        let folder = PathBuf::from("/some/folder");
        let folder_src = |names: &[&str]| -> Arc<dyn ItemSource> {
            Arc::new(FsSource::new(
                names.iter().map(|n| folder.join(n)).collect(),
            ))
        };
        core.apply_scan_batch(crate::scan::Resolved {
            source: folder_src(&["a.jpg", "b.jpg"]),
            root: folder.clone(),
            scan_root: Some(folder.clone()),
            recursive: true,
            start: 0,
        });
        assert_eq!(core.source.len(), 2);
        core.apply_scan_batch(crate::scan::Resolved {
            source: folder_src(&["a.jpg", "b.jpg", "c.jpg"]),
            root: folder.clone(),
            scan_root: Some(folder),
            recursive: true,
            start: 0,
        });
        assert_eq!(core.source.len(), 3, "the same scan's later batch extends");
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
        core.settings.show_archives = true;
        // At the archive ROOT, ⌘←/→ steps the archive's own internal folders first — a cursor
        // jump, no disk worker (task #108). Displayed on the first internal folder ("a/b") →
        // jump to the next one ("a/b/c" at index 1).
        core.displayed_item = Some(0);
        core.open_sibling_cmd(1);
        assert_eq!(
            core.playlist.current(),
            Some(1),
            "stepped to the next internal folder"
        );
        assert!(
            core.tree_io.is_none(),
            "an internal-folder step is a cursor jump, not a disk worker"
        );

        // Only past the LAST internal folder ("" at index 4) does the root step to the adjacent
        // archive on disk (a worker).
        core.displayed_item = Some(4);
        core.open_sibling_cmd(1);
        assert!(
            core.tree_io.is_some(),
            "no more internal folders → adjacent archive via a disk worker"
        );
        core.tree_io = None; // cancels the fire-and-forget probe
                             // With Show Archives off, past the last internal folder there's nothing to step to.
        core.settings.show_archives = false;
        core.displayed_item = Some(4);
        core.open_sibling_cmd(1);
        assert!(
            core.tree_io.is_none(),
            "archives hidden → no disk worker at the archive root"
        );
        core.settings.show_archives = true;

        // Scoped into an internal folder: stepping stays in-RAM, no disk worker (the subject).
        core.rescope_archive("a/b".to_string());
        core.open_sibling_cmd(1);
        assert_eq!(core.archive_scope.as_ref().unwrap().prefix, "a/bc");
        assert_eq!(deck_names(&core), vec!["a/bc/three.jpg"]);
        assert!(core.tree_io.is_none(), "internal-folder stepping is in-RAM");
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
        let feed = |core: &mut AppCore,
                    from_root: PathBuf,
                    target: Option<crate::folder_tree::DiskTarget>| {
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
            Some(crate::folder_tree::DiskTarget::Directory(PathBuf::from(
                "/somewhere/else/next",
            ))),
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
        feed(
            &mut core,
            root,
            Some(crate::folder_tree::DiskTarget::Directory(target)),
        );
        assert!(opened_dir(&core), "a found sibling opens as a dir scan");
    }

    /// Open Parent out of an archive lands the deck cursor on that archive's **door** when Show
    /// Archives is on (so `space` continues past it), and on the folder's first item when it's
    /// off (task #108 — the off case avoids the streaming-scan stall on a filtered-out target).
    #[test]
    fn open_parent_out_of_an_archive_lands_on_its_door_when_archives_shown() {
        let door = std::env::temp_dir().join("deck.zip"); // archive_core's root/container
        let begin_cursor = |core: &AppCore| {
            core.effects.iter().find_map(|e| match e {
                contract::CoreEffect::BeginDirScan { cursor, .. } => Some(cursor.clone()),
                _ => None,
            })
        };

        let mut shown = archive_core(ARCHIVE);
        shown.settings.show_archives = true;
        shown.effects.clear();
        shown.open_parent_cmd();
        assert_eq!(
            begin_cursor(&shown),
            Some(pb_core::open::Cursor::At(door.clone())),
            "archives shown → land on the archive door"
        );

        let mut hidden = archive_core(ARCHIVE);
        hidden.settings.show_archives = false;
        hidden.effects.clear();
        hidden.open_parent_cmd();
        assert_eq!(
            begin_cursor(&hidden),
            Some(pb_core::open::Cursor::First),
            "archives hidden → first item (no stall on a filtered-out door)"
        );
    }

    /// `open_disk_target` (task #108): a `Directory` re-roots as a folder scan; an `Archive`
    /// opens as its own deck (the door / File-open path), never a folder scan.
    #[test]
    fn open_disk_target_routes_folders_and_archives_apart() {
        let mut core = test_core();
        core.effects.clear();
        core.open_disk_target(crate::folder_tree::DiskTarget::Archive(PathBuf::from(
            "/p/a.zip",
        )));
        assert!(
            core.effects.iter().any(|e| matches!(
                e,
                contract::CoreEffect::BeginArchiveOpen { path, .. } if path.as_path() == Path::new("/p/a.zip")
            )),
            "an archive target opens the archive"
        );
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::BeginDirScan { .. })),
            "an archive target is never a folder scan"
        );

        core.effects.clear();
        core.open_disk_target(crate::folder_tree::DiskTarget::Directory(PathBuf::from(
            "/p/dir",
        )));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::BeginDirScan { .. })),
            "a directory target re-roots as a folder scan"
        );
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
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(paths));
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
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(paths));
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
        let deck = |root: PathBuf| -> (Arc<dyn ItemSource>, PathBuf) {
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
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![dir.join("a.png")]));
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
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(paths));
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
        let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(remaining));
        core.rebuild_playlist(src, dir.clone(), Some(dir.clone()), true, 0);
        assert_eq!(
            core.compare_pin,
            Some(1),
            "the pin re-resolves by path across a same-deck rebuild"
        );
        assert_eq!(core.compare_return, None, "the return point never survives");
        // A genuinely new deck has no matching identity — the pin clears.
        let other: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("other_{i}.png"))).collect();
        let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(other));
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
            size: None,
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
            size: None,
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
        // (hold-to-blaze gates against the same clock the Tick events carry).
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
    fn display_counter_tracks_the_displayed_item_not_the_nav_target() {
        // The toolbar counter (task #61) must read *present-truth* (`displayed_item`), so a
        // resident-ring miss — where the target has advanced but the old photo is still on
        // screen — never makes the counter lie.
        let mut core = test_core();
        core.source = five_photos();
        // Cold start: nothing presented yet → the counter hides.
        core.displayed_item = None;
        assert_eq!(core.display_counter(), None);
        // Presenting item 2 (0-based) shows "3 / 5".
        core.displayed_item = Some(2);
        assert_eq!(core.display_counter(), Some((3, 5)));
        // The nav target races ahead to item 4 while the ring is still catching up — the counter
        // stays on the *displayed* item, not the target.
        core.target_item = Some(4);
        assert_eq!(core.display_counter(), Some((3, 5)));
    }

    #[test]
    fn pointer_nav_is_a_second_hold_to_blaze_source() {
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
        // touches no state and emits no effect (the hold loop drives blaze-speed, not repeats).
        core.handle(CoreEvent::KeyDown {
            key: PbKey::Space,
            mods: Modifiers::NONE,
            repeat: true,
        });
        assert!(core.held.is_empty());
        assert!(core.effects.is_empty());
    }

    // --- Streaming Live Photo lifecycle (task #69) ---------------------------------------
    // The consumer half is platform-neutral (only the *producers* are gated), so these run
    // everywhere: they inject an `AnimStream` by hand — exactly what `start_live_stream`
    // builds — and drive `poll_anim_stream` through install / extend / finish / failure.

    use crate::animation::{AnimStream, StreamMsg};
    use std::sync::mpsc;

    /// Wire a synthetic streaming decode for `item`, exactly like `start_live_stream` does,
    /// returning the producer's sender and the shared cancel flag.
    fn inject_stream(
        core: &mut AppCore,
        item: usize,
        want: AnimWant,
    ) -> (
        mpsc::Sender<StreamMsg>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let (tx, rx) = mpsc::channel();
        core.displayed_item = Some(item);
        core.target_item = Some(item);
        core.anim_gen += 1;
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        core.anim_stream = Some(AnimStream {
            gen: core.anim_gen,
            item,
            epoch: core.epoch,
            want,
            rx,
            cancel: std::sync::Arc::clone(&cancel),
            header: None,
            pending: Vec::new(),
            installed: false,
        });
        (tx, cancel)
    }

    fn stream_header() -> StreamMsg {
        StreamMsg::Header {
            width: 1,
            height: 1,
            color: pb_decode::ColorTransform::srgb(),
            codec: "Live Photo",
        }
    }

    fn stream_frame() -> StreamMsg {
        StreamMsg::Frame(pb_decode::AnimFrame {
            rgba: vec![1, 2, 3, 255],
            width: 1,
            height: 1,
            delay: Duration::from_millis(33),
        })
    }

    /// Task #79 phase 4: `P` on a video item starts a `VideoSession` — never the
    /// animation decode machinery (which would read the file into RAM). A producer
    /// that can't open the file fails the session cleanly through `poll_video`,
    /// which surfaces a toast and clears the session.
    #[test]
    fn p_on_a_video_item_starts_a_session_never_the_animation_machinery() {
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.native_toast = true; // headless has no HUD raster; the native path retains text
        assert!(core.item_is_video(0));
        assert_eq!(core.play_hint_kind(), 2, "video badge is the play glyph");

        core.toggle_play_pause();
        assert!(
            core.playback.is_none(),
            "video never uses the animation playback"
        );
        assert!(core.anim_decode.is_none(), "no batch decode kicked");
        assert!(core.anim_stream.is_none(), "no stream kicked");
        // Session platforms: Windows (MF) and Linux with the FFmpeg producer
        // (task #84) — same protocol, same failure contract.
        #[cfg(any(windows, all(unix, not(target_os = "macos"), feature = "ffvideo")))]
        {
            assert!(core.video.is_some(), "P starts the video session");
            // The missing file fails the producer; the session surfaces it via
            // poll (bounded wait — the producer thread races this assert).
            let deadline = Instant::now() + Duration::from_secs(10);
            while core.video.is_some() && Instant::now() < deadline {
                core.now = Instant::now();
                core.poll_video();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(core.video.is_none(), "failure clears the session");
            assert!(
                core.toast_native.is_some(),
                "the failure surfaces to the user"
            );
        }
        // macOS (task 79.9): `P` starts a `Native` backend and commands the shell's
        // AVPlayer via `PlayVideo` — no Rust producer/session.
        #[cfg(target_os = "macos")]
        {
            assert!(core.video.is_some(), "P starts a native video session");
            assert!(
                core.video.as_ref().unwrap().as_native().is_some(),
                "macOS uses the Native backend, not a VideoSession"
            );
            assert!(
                core.effects
                    .iter()
                    .any(|e| matches!(e, contract::CoreEffect::PlayVideo { .. })),
                "the native player is commanded to open the clip"
            );
        }
        #[cfg(not(any(windows, target_os = "macos", all(unix, feature = "ffvideo"))))]
        assert!(core.video.is_none(), "no producer on this platform yet");
    }

    /// macOS smoothness-plan routing: a loose-file MKV/WebM goes to the **Session
    /// route** (FFmpeg → wgpu → Metal) — the sample-buffer presenter drops ~3
    /// frames/sec that the Session route presents smoothly, so it is parked
    /// opt-in (`sample_buffer_opt_in`; see the test below). MP4/MOV keep AVPlayer
    /// (covered by `recoverable_native_failure_falls_back_to_the_ffmpeg_session`).
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn mkv_and_webm_route_to_the_session_on_macos() {
        for name in ["/nope/clip.mkv", "/nope/clip.webm"] {
            let mut core = test_core();
            core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
                name,
            )]));
            core.displayed_item = Some(0);
            core.native_toast = true;
            core.toggle_play_pause();
            let v = core.video.as_ref().expect("P starts playback");
            assert!(
                v.as_session().is_some(),
                "{name}: routes to the Session (FFmpeg) route"
            );
            assert!(
                !core
                    .effects
                    .iter()
                    .any(|e| matches!(e, contract::CoreEffect::PlaySampleBuffer { .. })),
                "{name}: the parked sample-buffer presenter is not commanded"
            );
            assert!(
                !core
                    .effects
                    .iter()
                    .any(|e| matches!(e, contract::CoreEffect::PlayVideo { .. })),
                "{name}: no AVPlayer is commanded"
            );
        }
    }

    /// The owner's missing-chrome report (2026-07-16): pressing `P` before the
    /// poster decode lands (an SMB movie's poster takes seconds; `P` always wins)
    /// left `current` unset for the WHOLE playback — the Session route streams
    /// frames around `present_item`, and the first frame's `mark_resolved` makes
    /// the late poster skip its present. With no `current` there is no info line,
    /// no `i`, no hover reveal, no playback controls. The fix: a presented video
    /// frame adopts the poster's metadata from `meta_cache` the moment it lands.
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn a_video_frame_adopts_late_poster_meta_so_the_controls_can_show() {
        let mut core = test_core();
        core.native_info = true;
        core.info_line = false;
        core.viewport.width = 800;
        core.viewport.height = 1000;
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            "/nope/movie.mkv",
        )]));
        core.displayed_item = Some(0);
        core.toggle_play_pause(); // P wins the race: no poster has presented yet
        assert!(core
            .video
            .as_ref()
            .is_some_and(|v| v.as_session().is_some()));
        assert!(core.current.is_none(), "the poster hasn't landed");

        let frame = crate::video::VideoFrame {
            session_id: crate::video::VideoSessionId(core.video_seq),
            seek_generation: crate::video::SeekGeneration::FIRST,
            pts: Duration::ZERO,
            width: 2,
            height: 2,
            pixels: vec![0; 16],
            format: pb_decode::PixelFormat::Rgba8,
            color: crate::video::VideoColorInfo::srgb(),
        };
        // Frames present before the poster lands: nothing to adopt, no controls yet.
        core.present_video_frame(&frame);
        assert!(core.current.is_none());
        core.video_hover_reveal(900.0);
        assert!(
            !core.info_line_visible(),
            "no metadata yet — nothing to show"
        );

        // The poster decode completes off-thread; drain_results caches its meta.
        core.meta_cache.insert(
            0,
            crate::meta::PhotoMeta {
                rel: "movie.mkv".into(),
                w: 1920,
                h: 1080,
                size: None,
                codec: "MKV",
                animated: None,
            },
        );
        // The very next presented frame adopts it — chrome comes alive mid-play.
        core.present_video_frame(&frame);
        assert!(core.current.is_some(), "the late poster's meta is adopted");
        core.video_hover_reveal(900.0);
        assert!(
            core.info_line_visible(),
            "hover now reveals the playback controls"
        );
    }

    /// Chrome parity on the new default route (owner report 2026-07-16): an MKV
    /// playing on the Session route must still (a) report `video_session_active`
    /// — the SwiftUI shell's gate for the playback row/scrubber — and (b) reveal
    /// the controls line on a bottom-zone hover, exactly like the old
    /// sample-buffer route did.
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn session_mkv_reports_active_and_reveals_the_controls() {
        let mut core = test_core();
        core.native_info = true;
        core.info_line = false;
        core.viewport.width = 800;
        core.viewport.height = 1000;
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            "/nope/clip.mkv",
        )]));
        core.displayed_item = Some(0);
        core.current = Some(crate::meta::PhotoMeta {
            rel: "clip.mkv".into(),
            w: 64,
            h: 64,
            size: None,
            codec: "MKV",
            animated: None,
        });
        core.toggle_play_pause(); // the real routing → Session backend
        assert!(
            core.video
                .as_ref()
                .is_some_and(|v| v.as_session().is_some()),
            "MKV plays on the Session route"
        );
        assert!(
            core.video_session_active(),
            "the SwiftUI chrome gate must see the session"
        );
        core.video_hover_reveal(900.0);
        assert!(
            core.info_line_visible(),
            "bottom-zone hover reveals the controls line"
        );
    }

    /// The parked sample-buffer presenter stays reachable — `PB_SAMPLE_BUFFER=1`
    /// (the `sample_buffer_opt_in` field; env is read once at host construction,
    /// so the test sets the field rather than racing process-global env). This is
    /// what keeps the Dolby-Vision reference renderer from rotting.
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn sample_buffer_opt_in_routes_mkv_to_the_presenter() {
        let mut core = test_core();
        core.sample_buffer_opt_in = true;
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            "/nope/clip.mkv",
        )]));
        core.displayed_item = Some(0);
        core.native_toast = true;
        core.toggle_play_pause();
        let v = core.video.as_ref().expect("P starts playback");
        assert!(
            v.as_native().is_some(),
            "opted in: MKV routes to the sample-buffer presenter (a Native-proxy backend)"
        );
        assert!(v.as_session().is_none(), "not the FFmpeg session");
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PlaySampleBuffer { .. })),
            "the sample-buffer presenter is commanded"
        );
    }

    /// The honest-UX DoVi warning (macos-video-smoothness §2): a Session-route
    /// video whose probe flagged a non-backward-compatible Dolby Vision stream
    /// (Profile 5 / compat-id 0) toasts once — and only once per item.
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn an_incompatible_dovi_video_warns_once_on_the_session_route() {
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            "/nope/dovi5.mkv",
        )]));
        core.displayed_item = Some(0);
        core.native_toast = true;
        // The container probe already landed (say, the panel was open earlier).
        let mut d = crate::app_core::ItemDetails::ready(0, Vec::new());
        d.dovi_incompatible = true;
        core.exif_cache.insert(0, d);
        core.toggle_play_pause();
        assert!(
            core.video
                .as_ref()
                .is_some_and(|v| v.as_session().is_some()),
            "MKV plays on the Session route"
        );
        let toast = core.toast_native.as_ref().expect("the warning toast fires");
        assert!(
            toast.message.contains("Dolby Vision"),
            "names the culprit: {}",
            toast.message
        );
        // A replay / probe re-landing must not nag: warned once per item.
        core.toast_native = None;
        core.maybe_warn_dovi(0);
        assert!(core.toast_native.is_none(), "warned once per item");
    }

    /// The same flagged file on the AVPlayer route stays quiet — AVPlayer decodes
    /// Dolby Vision natively, so there is nothing to warn about.
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn the_dovi_warning_stays_quiet_off_the_session_route() {
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            "/nope/dovi.mp4",
        )]));
        core.displayed_item = Some(0);
        core.native_toast = true;
        let mut d = crate::app_core::ItemDetails::ready(0, Vec::new());
        d.dovi_incompatible = true;
        core.exif_cache.insert(0, d);
        core.toggle_play_pause();
        assert!(
            core.video.as_ref().is_some_and(|v| v.as_native().is_some()),
            "MP4 plays on the AVPlayer route"
        );
        core.maybe_warn_dovi(0); // even asked directly, the route gate holds
        assert!(
            core.toast_native.is_none(),
            "AVPlayer does real DoVi — no warning"
        );
    }

    /// macOS §8a level-2 fallback (task #84): a *recoverable* native failure on
    /// a nominally-native container retries through the FFmpeg session with no
    /// toast before the attempt; an unrecoverable one surfaces immediately.
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn recoverable_native_failure_falls_back_to_the_ffmpeg_session() {
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            "/nope/clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.native_toast = true;
        core.toggle_play_pause();
        assert!(
            core.video.as_ref().unwrap().as_native().is_some(),
            "MP4 tries AVPlayer first"
        );
        let sid = core.native_video_session_id();
        assert!(sid > 0);
        // The shell classifies a demux/codec failure as recoverable.
        core.native_video_failed(sid, "no codec for this video".into(), true);
        assert!(
            core.video
                .as_ref()
                .is_some_and(|v| v.as_session().is_some()),
            "fallback started the FFmpeg session"
        );
        assert!(
            core.toast_native.is_none(),
            "no error surfaces before the fallback attempt"
        );
        // The flag was consumed — it never loops.
        assert_eq!(core.video_ffmpeg_fallback, None);
    }

    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn unrecoverable_native_failure_surfaces_without_fallback() {
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            "/nope/clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.native_toast = true;
        core.toggle_play_pause();
        let sid = core.native_video_session_id();
        core.native_video_failed(sid, "The file couldn't be opened".into(), false);
        assert!(core.video.is_none(), "no fallback for missing-file/DRM");
        assert!(core.toast_native.is_some(), "the error surfaces at once");
    }

    /// Owner-reported: the info line showed no playback row during video playback.
    /// This drives the real chain — session → update_video_progress →
    /// show_info_line — and asserts each link.
    #[test]
    fn video_playback_grows_the_info_line_row() {
        use crate::video::{VideoProducerEvent, VideoSessionId};
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        core.hud = pb_hud::hud::Hud::load();
        if core.hud.is_none() {
            eprintln!("no system UI font — skipping");
            return;
        }
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.info_line = true;
        core.current = Some(crate::meta::PhotoMeta {
            rel: "clip.mp4".into(),
            w: 64,
            h: 64,
            size: None,
            codec: "MP4",
            animated: None,
        });
        assert!(core.info_line_visible(), "precondition: the line is on");

        let (session, io) = VideoSession::new(VideoSessionId(1), 1024);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(1),
                duration: Some(Duration::from_secs(10)),
                width: 64,
                height: 64,
                has_audio: false,
                frame_bytes: 64 * 64 * 4,
            })
            .unwrap();
        core.poll_video();
        assert!(
            core.video_progress_row().is_some(),
            "a live session on the displayed item must yield a progress row"
        );
        core.update_video_progress();
        assert!(
            core.video_pill_text.is_some(),
            "the row text must be computed"
        );
        assert!(
            core.info_line_shown,
            "update_video_progress must re-raster the info line"
        );
    }

    /// `,`/`.` on a playing video (task #79 follow-up): stepping pauses the
    /// session, serves the next queued frame, keeps the paused audio player in
    /// step, and — with the `i` toggle off on a native-info shell — flashes the
    /// info line as the position OSD instead of toasting. A backward step then
    /// launches a paused seek.
    #[test]
    fn frame_step_on_video_pauses_steps_and_flashes_the_info_line() {
        use crate::video::{VideoProducerEvent, VideoSessionId, VideoSessionState};
        use crate::video_session::{ActiveVideo, VideoSession};
        use pb_decode::video::VideoColorInfo;

        let mut core = test_core();
        core.native_info = true;
        core.info_line = false; // the toggle is OFF — feedback must flash the line
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.current = Some(crate::meta::PhotoMeta {
            rel: "clip.mp4".into(),
            w: 64,
            h: 64,
            size: None,
            codec: "MP4",
            animated: None,
        });

        let sid = VideoSessionId(1);
        let (session, io) = VideoSession::new(sid, 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(10)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        let frame = |pts_ms: u64| pb_decode::VideoFrame {
            session_id: sid,
            seek_generation: crate::video::SeekGeneration::FIRST,
            pts: Duration::from_millis(pts_ms),
            width: 1,
            height: 1,
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 4],
            color: VideoColorInfo::srgb(),
        };
        io.events.send(VideoProducerEvent::Frame(frame(0))).unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33)))
            .unwrap();
        core.poll_video(); // → Playing, presents pts 0
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Playing
        );

        core.effects.clear();
        core.frame_step(1);
        let v = core.video.as_ref().unwrap().as_session().unwrap();
        assert_eq!(
            v.session.state(),
            VideoSessionState::Paused,
            "stepping pauses playback, like animations"
        );
        assert_eq!(v.session.current_pts, Some(Duration::from_millis(33)));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PauseVideoAudio)),
            "the shell audio player pauses with the session"
        );
        assert!(
            core.effects.iter().any(|e| matches!(
                e,
                contract::CoreEffect::SeekVideoAudio { position } if *position == Duration::from_millis(33)
            )),
            "the paused audio player follows the stepped position"
        );
        assert!(
            core.video_osd_until.is_some() && core.info_line_visible(),
            "with `i` off, the position feedback flashes the info line"
        );
        assert!(
            core.toast_native.is_none() && core.toast.is_none(),
            "no `m:ss / m:ss` toast when the line is the readout"
        );

        // A backward step launches a paused one-frame seek.
        core.frame_step(-1);
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Seeking
        );

        // The flash lapses at its deadline (tick clears it + notifies the shell).
        core.video_osd_until = Some(core.now - Duration::from_millis(1));
        core.handle(contract::CoreEvent::Tick(Instant::now()));
        assert!(core.video_osd_until.is_none(), "the OSD flash expires");
        assert!(!core.info_line_visible(), "the flashed line drops");
    }

    /// R4 (overhaul plan 1D): a held-seek run pauses audio ONCE and commits ONE
    /// audio seek + resume (in that order) at the settled final landing — never
    /// stopping/seeking/refilling the audio decoder per intermediate target.
    #[test]
    fn held_seek_run_coalesces_to_one_audio_commit() {
        use crate::video::{
            SeekGeneration, VideoProducerEvent, VideoProducerMsg, VideoSessionId, VideoSessionState,
        };
        use crate::video_session::{ActiveVideo, VideoSession, VideoSessionIo};
        use pb_decode::video::VideoColorInfo;

        let mut core = test_core();
        let sid = VideoSessionId(9);
        let (session, io) = VideoSession::new(sid, 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        let frame = |pts_ms: u64, generation: SeekGeneration| pb_decode::VideoFrame {
            session_id: sid,
            seek_generation: generation,
            pts: Duration::from_millis(pts_ms),
            width: 1,
            height: 1,
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 4],
            color: VideoColorInfo::srgb(),
        };
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(60)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(0, SeekGeneration::FIRST)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33, SeekGeneration::FIRST)))
            .unwrap();
        core.poll_video();
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Playing
        );

        // The generation of the last SeekTo the producer saw (drains the inbox).
        let last_seek_gen = |io: &VideoSessionIo| {
            let mut generation = None;
            while let Ok(msg) = io.msgs.try_recv() {
                if let VideoProducerMsg::SeekTo { generation: g, .. } = msg {
                    generation = Some(g);
                }
            }
            generation.expect("a SeekTo reached the producer")
        };

        core.effects.clear();
        // Model the seek key being HELD for the whole run — the real held-key path
        // sets `video_seek_last` each repeat (`apply_view_holds`), and the adaptive
        // audio commit keys off it: while the key is down, only the full settle window
        // applies, so intermediate targets never commit (below). A bare `video_seek`
        // wouldn't set it, so set it explicitly to model the hold.
        core.video_seek_last = Some(core.now);
        // Held repeat: two forward seeks 200 ms apart, each landing quickly.
        core.video_seek(false);
        let gen1 = last_seek_gen(&io);
        io.events
            .send(VideoProducerEvent::Frame(frame(2000, gen1)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(2033, gen1)))
            .unwrap();
        core.now += Duration::from_millis(200);
        core.poll_video(); // gen1 lands mid-run
        core.video_seek(false);
        let gen2 = last_seek_gen(&io);
        io.events
            .send(VideoProducerEvent::Frame(frame(4000, gen2)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(4033, gen2)))
            .unwrap();
        core.now += Duration::from_millis(100);
        core.poll_video(); // gen2 lands; run not settled yet (100 < 250 ms)

        let pauses = |core: &AppCore| {
            core.effects
                .iter()
                .filter(|e| matches!(e, contract::CoreEffect::PauseVideoAudio))
                .count()
        };
        let seeks = |core: &AppCore| {
            core.effects
                .iter()
                .filter(|e| matches!(e, contract::CoreEffect::SeekVideoAudio { .. }))
                .count()
        };
        let resumes = |core: &AppCore| {
            core.effects
                .iter()
                .filter(|e| matches!(e, contract::CoreEffect::ResumeVideoAudio))
                .count()
        };
        assert_eq!(pauses(&core), 1, "audio pauses once at run begin");
        assert_eq!(seeks(&core), 0, "no audio seek for intermediate targets");
        assert_eq!(resumes(&core), 0, "audio stays paused mid-run");

        // The run settles → exactly one commit: seek to the LANDED position,
        // then resume, in that order.
        core.now += VIDEO_SEEK_AUDIO_SETTLE;
        core.poll_video();
        assert_eq!(pauses(&core), 1);
        assert_eq!(seeks(&core), 1, "one audio seek per run");
        assert_eq!(resumes(&core), 1, "one resume per run");
        let seek_at = core.effects.iter().position(
            |e| matches!(e, contract::CoreEffect::SeekVideoAudio { position } if *position == Duration::from_millis(4000)),
        );
        let resume_at = core
            .effects
            .iter()
            .position(|e| matches!(e, contract::CoreEffect::ResumeVideoAudio));
        assert!(
            seek_at.expect("seek to the landed pts") < resume_at.expect("resume"),
            "audio seeks before it resumes"
        );

        // A later poll adds nothing — the commit is one-shot.
        core.now += Duration::from_millis(100);
        core.poll_video();
        assert_eq!((seeks(&core), resumes(&core)), (1, 1));
    }

    /// Task #4 follow-up: a DISCRETE tap — the seek key already released — commits its
    /// audio seek after the short [`VIDEO_SEEK_AUDIO_QUIET`], NOT the full settle
    /// window, so audio lands with the picture instead of ~172 ms behind it (measured).
    /// The held run above proves the slow path still coalesces; this proves a tap is
    /// fast, and the two differ only by whether the key is down.
    #[test]
    fn a_released_tap_commits_audio_after_the_short_quiet() {
        use crate::video::{
            SeekGeneration, VideoProducerEvent, VideoProducerMsg, VideoSessionId, VideoSessionState,
        };
        use crate::video_session::{ActiveVideo, VideoSession};
        use pb_decode::video::VideoColorInfo;

        let mut core = test_core();
        let sid = VideoSessionId(9);
        let (session, io) = VideoSession::new(sid, 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        let frame = |pts_ms: u64, generation: SeekGeneration| pb_decode::VideoFrame {
            session_id: sid,
            seek_generation: generation,
            pts: Duration::from_millis(pts_ms),
            width: 1,
            height: 1,
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 4],
            color: VideoColorInfo::srgb(),
        };
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(60)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(0, SeekGeneration::FIRST)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33, SeekGeneration::FIRST)))
            .unwrap();
        core.poll_video();
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Playing
        );

        core.effects.clear();
        // The key is UP (a single tap already released) — the release signal.
        core.video_seek_last = None;
        core.video_seek(false);
        let generation = {
            let mut g = None;
            while let Ok(msg) = io.msgs.try_recv() {
                if let VideoProducerMsg::SeekTo { generation, .. } = msg {
                    g = Some(generation);
                }
            }
            g.expect("a SeekTo reached the producer")
        };
        // Two frames satisfy preroll (PREROLL_FRAMES) so the seek lands, as the held
        // test does; the landing anchors at the first frame's pts (2000).
        io.events
            .send(VideoProducerEvent::Frame(frame(2000, generation)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(2033, generation)))
            .unwrap();

        // Just past the short quiet — and well below the full settle window.
        assert!(VIDEO_SEEK_AUDIO_QUIET < VIDEO_SEEK_AUDIO_SETTLE);
        core.now += VIDEO_SEEK_AUDIO_QUIET + Duration::from_millis(1);
        core.poll_video();
        let seeks = core
            .effects
            .iter()
            .filter(|e| matches!(e, contract::CoreEffect::SeekVideoAudio { position } if *position == Duration::from_millis(2000)))
            .count();
        assert_eq!(
            seeks, 1,
            "a released tap commits after the short quiet, not the full window"
        );
    }

    /// Task #94.2: leaving a video far enough into a long-enough clip remembers a
    /// (rewound) resume position keyed by item; a near-start leave remembers nothing.
    #[test]
    fn stop_video_remembers_a_mid_clip_position() {
        use crate::video::{VideoProducerEvent, VideoSessionId};
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        let sid = VideoSessionId(20);
        let (session, io) = VideoSession::new(sid, 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 3)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(60)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        core.poll_video();
        // Playhead at ~10 s (a seek sets desired_position without needing frames).
        if let Some(v) = core
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
        {
            v.session.seek_to(Duration::from_secs(10), core.now, None);
        }
        core.stop_video();
        assert_eq!(
            core.video_resume.get(&3).copied(),
            Some(Duration::from_secs(8)), // 10 s − RESUME_REWIND
            "leaving mid-clip remembers the rewound position"
        );

        // A near-start leave remembers nothing (item 3's entry stays as-is; a new
        // item 4 left at 2 s is not recorded).
        let (session2, io2) = VideoSession::new(VideoSessionId(21), 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session2, 4)));
        io2.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(21),
                duration: Some(Duration::from_secs(60)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        core.poll_video();
        if let Some(v) = core
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
        {
            v.session.seek_to(Duration::from_secs(2), core.now, None);
        }
        core.stop_video();
        assert_eq!(
            core.video_resume.get(&4),
            None,
            "near-start is not remembered"
        );
    }

    /// Task #94.2: a session started for an item with a remembered position seeks
    /// there once it can (leaving Opening), holds the poster until then, and pauses
    /// audio for the resume run.
    #[test]
    fn returning_to_a_video_resumes_at_the_remembered_position() {
        use crate::video::{VideoProducerEvent, VideoProducerMsg, VideoSessionId};
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        let sid = VideoSessionId(22);
        let (session, io) = VideoSession::new(sid, 4);
        let mut av = ActiveVideo::new(session, 5);
        av.resume_to = Some(Duration::from_secs(30)); // as start_video_session would set it
        core.video = Some(ActiveVideoBackend::Session(av));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(120)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        core.poll_video();

        // The resume seek fired to 30 s, and the one-shot target was consumed.
        let v = core
            .video
            .as_ref()
            .and_then(ActiveVideoBackend::as_session)
            .expect("session");
        assert_eq!(
            v.resume_to, None,
            "the resume target is consumed once applied"
        );
        assert_eq!(
            v.session.desired_position(core.now),
            Duration::from_secs(30),
            "the session sought to the remembered position"
        );
        // The producer was told to seek to ~30 s (video Cues path).
        let sought = std::iter::from_fn(|| io.msgs.try_recv().ok())
            .any(|m| matches!(m, VideoProducerMsg::SeekTo { target, .. } if target == Duration::from_secs(30)));
        assert!(sought, "a SeekTo(30s) reached the producer");
    }

    /// Task #94.2 (native path): the shell's periodic position report folds into
    /// the resume map — mid-clip remembered (rewound), watched-to-end forgotten,
    /// a stale session ignored.
    #[test]
    fn native_video_progress_records_and_forgets_resume() {
        use crate::video::VideoSessionId;
        use crate::video_native::NativeVideoProxy;

        let mut core = test_core();
        core.video = Some(ActiveVideoBackend::Native(NativeVideoProxy::new(
            7,
            VideoSessionId(30),
            false,
        )));
        // Mid-clip → remembered, rewound by RESUME_REWIND.
        core.native_video_progress(30, 40.0, 100.0);
        assert_eq!(
            core.video_resume.get(&7).copied(),
            Some(Duration::from_secs(38))
        );
        // Near the end → forgotten, so returning restarts.
        core.native_video_progress(30, 99.0, 100.0);
        assert_eq!(core.video_resume.get(&7), None);
        // Re-record mid-clip, then a wrong-session report must NOT touch it.
        core.native_video_progress(30, 50.0, 100.0);
        core.native_video_progress(999, 10.0, 100.0);
        assert_eq!(
            core.video_resume.get(&7).copied(),
            Some(Duration::from_secs(48))
        );
    }

    /// A paused seek commits the audio position on settle but never resumes —
    /// paused stays paused (plan 1D).
    #[test]
    fn paused_seek_commits_audio_position_without_resume() {
        use crate::video::{
            SeekGeneration, VideoProducerEvent, VideoProducerMsg, VideoSessionId, VideoSessionState,
        };
        use crate::video_session::{ActiveVideo, VideoSession};
        use pb_decode::video::VideoColorInfo;

        let mut core = test_core();
        let sid = VideoSessionId(10);
        let (session, io) = VideoSession::new(sid, 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        let frame = |pts_ms: u64, generation: SeekGeneration| pb_decode::VideoFrame {
            session_id: sid,
            seek_generation: generation,
            pts: Duration::from_millis(pts_ms),
            width: 1,
            height: 1,
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 4],
            color: VideoColorInfo::srgb(),
        };
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(60)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(0, SeekGeneration::FIRST)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33, SeekGeneration::FIRST)))
            .unwrap();
        core.poll_video();

        // Pause, then seek: the landing presents once and stays paused.
        if let Some(v) = core
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
        {
            v.session.pause(core.now);
        }
        core.effects.clear();
        core.video_seek(false);
        let generation = {
            let mut generation = None;
            while let Ok(msg) = io.msgs.try_recv() {
                if let VideoProducerMsg::SeekTo { generation: g, .. } = msg {
                    generation = Some(g);
                }
            }
            generation.expect("a SeekTo reached the producer")
        };
        io.events
            .send(VideoProducerEvent::Frame(frame(2000, generation)))
            .unwrap();
        core.now += VIDEO_SEEK_AUDIO_SETTLE;
        core.poll_video();
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Paused
        );
        assert!(
            core.effects.iter().any(|e| matches!(
                e,
                contract::CoreEffect::SeekVideoAudio { position } if *position == Duration::from_millis(2000)
            )),
            "the paused audio player follows the landed position"
        );
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ResumeVideoAudio)),
            "paused stays paused"
        );
    }

    /// Archive video playback: when the producer reports an audio track, the
    /// `StartVideoAudio` effect carries the SAME `Arc`-shared in-RAM container the
    /// producer reads (the `ActiveVideo::media` slot) — an archive entry has no
    /// path, and the one-copy contract is the point of the slot.
    #[test]
    fn archive_video_audio_starts_from_the_shared_bytes() {
        use crate::video::{VideoInput, VideoProducerEvent, VideoSessionId};
        use crate::video_session::{ActiveVideo, VideoSession};

        struct FakeArchive;
        impl pb_source::ItemSource for FakeArchive {
            fn len(&self) -> usize {
                1
            }
            fn name(&self, _i: usize) -> &str {
                "folder/clip.mp4"
            }
            fn bytes(&self, _i: usize) -> std::io::Result<Vec<u8>> {
                Ok(b"fake".to_vec())
            }
        }

        let mut core = test_core();
        core.source = Arc::new(FakeArchive);
        core.displayed_item = Some(0);

        let sid = VideoSessionId(1);
        let (session, io) = VideoSession::new(sid, 4);
        let av = ActiveVideo::new(session, 0);
        let data = std::sync::Arc::new(b"fake mp4 container".to_vec());
        av.media
            .set(VideoInput::Bytes {
                data: data.clone(),
                name: "folder/clip.mp4".into(),
            })
            .expect("fresh slot");
        core.video = Some(ActiveVideoBackend::Session(av));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(2)),
                width: 1,
                height: 1,
                has_audio: true,
                frame_bytes: 4,
            })
            .unwrap();
        core.effects.clear();
        core.poll_video();

        let started = core.effects.iter().find_map(|e| match e {
            contract::CoreEffect::StartVideoAudio {
                input, session_id, ..
            } => Some((input.clone(), *session_id)),
            _ => None,
        });
        let (input, got_sid) =
            started.expect("Opened(has_audio) must start the shell audio player");
        assert_eq!(got_sid, sid);
        match input {
            VideoInput::Bytes { data: d, name } => {
                assert!(
                    std::sync::Arc::ptr_eq(&d, &data),
                    "the audio player must read the SAME buffer (one resident copy)"
                );
                assert_eq!(name, "folder/clip.mp4");
            }
            VideoInput::Path(_) => {
                panic!("an archive entry must start audio from bytes, not a path")
            }
        }
    }

    /// Hovering the bottom controls zone reveals the playback controls while a
    /// video is active (owner request — the video-player convention); the top of
    /// the window doesn't, and the persistent `i` line needs no flash.
    #[test]
    fn hovering_the_controls_zone_reveals_the_playback_line() {
        use crate::video::{VideoProducerEvent, VideoSessionId};
        use crate::video_native::ActiveVideoBackend;
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        core.native_info = true;
        core.info_line = false;
        core.viewport.width = 800;
        core.viewport.height = 1000;
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.current = Some(crate::meta::PhotoMeta {
            rel: "clip.mp4".into(),
            w: 64,
            h: 64,
            size: None,
            codec: "MP4",
            animated: None,
        });
        let (session, io) = VideoSession::new(VideoSessionId(1), 16);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(1),
                duration: Some(Duration::from_secs(10)),
                width: 2,
                height: 2,
                has_audio: false,
                frame_bytes: 16,
            })
            .unwrap();
        core.poll_video();

        // Above the zone: nothing.
        core.video_hover_reveal(100.0);
        assert!(core.video_osd_until.is_none(), "top hover reveals nothing");
        // Inside the bottom quarter: the line flashes on.
        core.video_hover_reveal(900.0);
        assert!(core.video_osd_until.is_some() && core.info_line_visible());

        // With the persistent line on, hover never arms the flash.
        core.video_osd_until = None;
        core.info_line = true;
        core.video_hover_reveal(900.0);
        assert!(
            core.video_osd_until.is_none(),
            "persistent line needs no flash"
        );
        drop(io);
    }

    /// `flash_video_controls` (the scrubber-release re-arm) reveals the line for an active
    /// video regardless of pointer position, but never for a still or when `i` is already on.
    #[test]
    fn flash_video_controls_re_arms_the_reveal() {
        use crate::video::{VideoProducerEvent, VideoSessionId};
        use crate::video_native::ActiveVideoBackend;
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        core.native_info = true;
        core.info_line = false;
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.current = Some(crate::meta::PhotoMeta {
            rel: "clip.mp4".into(),
            w: 64,
            h: 64,
            size: None,
            codec: "MP4",
            animated: None,
        });
        let (session, io) = VideoSession::new(VideoSessionId(1), 16);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(1),
                duration: Some(Duration::from_secs(10)),
                width: 2,
                height: 2,
                has_audio: false,
                frame_bytes: 16,
            })
            .unwrap();
        core.poll_video();

        // Active video: the flash arms with no geometry (a mid-drag re-arm).
        core.flash_video_controls();
        assert!(core.video_osd_until.is_some() && core.info_line_visible());

        // Persistent `i` line up: nothing to re-arm.
        core.video_osd_until = None;
        core.info_line = true;
        core.flash_video_controls();
        assert!(
            core.video_osd_until.is_none(),
            "persistent line needs no flash"
        );

        // No active video: never arms (can't flash a still's line).
        core.info_line = false;
        core.video = None;
        core.flash_video_controls();
        assert!(core.video_osd_until.is_none(), "no video → no flash");
        drop(io);
    }

    /// Arrow-seek on the macOS **native** backend emits a relative, generation-bumped
    /// `SeekVideoBy` intent (±2 s, Shift ±10 s; the shell resolves it against AVPlayer).
    #[test]
    fn native_arrow_seek_emits_relative_seek_intent() {
        use crate::video::VideoSessionId;
        use crate::video_native::{ActiveVideoBackend, NativeVideoProxy};

        fn seek_of(core: &AppCore) -> Option<(u64, u64, i64)> {
            core.effects.iter().find_map(|e| match e {
                contract::CoreEffect::SeekVideoBy {
                    session_id,
                    generation,
                    delta_ms,
                } => Some((session_id.0, generation.0, *delta_ms)),
                _ => None,
            })
        }

        let mut core = test_core();
        core.displayed_item = Some(0);
        core.video = Some(ActiveVideoBackend::Native(NativeVideoProxy::new(
            0,
            VideoSessionId(7),
            false,
        )));

        // Forward ±2 s; generation bumps off FIRST(0) → 1.
        core.effects.clear();
        core.video_seek(false);
        assert_eq!(seek_of(&core), Some((7, 1, 2000)));

        // Backward is a negative delta; generation keeps climbing.
        core.effects.clear();
        core.video_seek(true);
        assert_eq!(seek_of(&core), Some((7, 2, -2000)));

        // Shift widens the step to ±10 s.
        core.mods.shift = true;
        core.effects.clear();
        core.video_seek(false);
        assert_eq!(seek_of(&core), Some((7, 3, 10_000)));
    }

    /// The macOS archive-video byte stash is pulled exactly once — a second pull (a stale
    /// or superseded session) gets nothing, never another session's container.
    #[test]
    fn pending_video_bytes_is_taken_once() {
        let mut core = test_core();
        assert!(
            core.take_pending_video_bytes().is_empty(),
            "none by default"
        );
        core.pending_video_bytes = Some(vec![1, 2, 3, 4]);
        assert_eq!(core.take_pending_video_bytes(), vec![1, 2, 3, 4]);
        assert!(core.take_pending_video_bytes().is_empty(), "consumed once");
    }

    /// A shell-generated archive-video poster becomes a synthetic full-decode `Outcome`
    /// queued for the ring; a wrong-sized frame is dropped, but the in-flight guard always
    /// clears so a later revisit can re-request.
    #[test]
    fn video_poster_ready_queues_a_synthetic_outcome() {
        let mut core = test_core();

        // Wrong pixel count (claims 4x4 but sends 10 bytes) → dropped, guard still cleared.
        core.poster_inflight.insert(3, 1);
        core.video_poster_ready(1, 3, 4, 4, vec![0u8; 10]);
        assert!(
            !core.poster_inflight.contains_key(&3),
            "in-flight cleared even on a bad frame"
        );
        assert!(
            core.pending_uploads.is_empty(),
            "bad pixel count is dropped"
        );

        // A STALE request id (the marker now belongs to a newer request, #119 diff
        // review): dropped whole — the replacement's marker survives.
        core.poster_inflight.insert(4, 9);
        core.video_poster_ready(2, 4, 2, 2, vec![255u8; 16]);
        assert!(
            core.poster_inflight.contains_key(&4),
            "a straggler with a stale id must not consume the replacement's marker"
        );
        assert!(core.pending_uploads.is_empty(), "and installs nothing");
        core.poster_inflight.remove(&4);

        // Correct 2x2 RGBA8 (16 bytes) → queued as a full (non-preview) outcome for item 5.
        core.poster_inflight.insert(5, 2);
        core.video_poster_ready(2, 5, 2, 2, vec![255u8; 16]);
        assert!(!core.poster_inflight.contains_key(&5));
        assert_eq!(core.pending_uploads.len(), 1);
        let o = &core.pending_uploads[0];
        assert_eq!(o.key.item, 5);
        assert_eq!(o.key.epoch, core.epoch);
        assert!(o
            .result
            .as_ref()
            .is_ok_and(|img| img.width == 2 && img.height == 2 && !img.is_preview));
    }

    // -- media-track Details rows (task #98) --------------------------------

    /// Seed the Details cache for `item` with a catalog, as a probe would.
    fn seed_details(
        core: &mut AppCore,
        item: usize,
        media: Option<pb_decode::MediaTrackCatalog>,
        has_audio: Option<bool>,
    ) {
        core.exif_cache.insert(
            item,
            crate::app_core::ItemDetails {
                size: 1234,
                fields: vec![("Video codec".into(), "HEVC".into())],
                media,
                has_audio,
                probe_state: crate::media_details::ProbeState::Ready,
                dovi_incompatible: false,
            },
        );
    }

    fn seeded_rows(core: &AppCore, item: usize) -> Vec<String> {
        let d = core.exif_cache.get(&item).expect("seeded");
        let mut rows = Vec::new();
        if let Some(cat) = &d.media {
            rows = crate::tracks::track_rows(cat, d.has_audio);
        }
        rows.iter()
            .map(|r| match r {
                DetailRow::Span { text, .. } => format!("[{text}]"),
                DetailRow::Pair { label, value } => format!("{label}: {value}"),
            })
            .collect()
    }

    fn track(codec: &str, lang: &str) -> pb_decode::MediaTrack {
        pb_decode::MediaTrack {
            id: pb_decode::TrackId {
                catalog_generation: 1,
                local_id: 0,
            },
            kind: pb_decode::TrackKind::Audio,
            language: Some(lang.into()),
            title: None,
            codec_raw: codec.to_ascii_lowercase(),
            codec: codec.into(),
            capability: pb_decode::TrackCapability::Playable,
            flags: pb_decode::TrackFlags::none(),
            audio: Some(pb_decode::AudioFormat {
                channels: 2,
                layout: Some("stereo".into()),
                sample_rate: 48000,
            }),
            external: false,
        }
    }

    /// A described catalog reaches the Details table as real per-track rows — the
    /// user-visible point of task #98 (this is what retires the `Audio: Yes` placeholder).
    #[test]
    fn a_described_catalog_becomes_per_track_details_rows() {
        let mut core = test_core();
        let cat = pb_decode::MediaTrackCatalog::new(
            1,
            pb_decode::MediaBackend::FFmpeg,
            pb_decode::TrackSet::complete(vec![track("AAC", "eng")]),
            pb_decode::TrackSet::complete(vec![]),
        );
        seed_details(&mut core, 0, Some(cat), Some(true));
        assert_eq!(
            seeded_rows(&core, 0),
            vec![
                "[Audio]",
                "Track 1: English · AAC stereo · 48 kHz",
                "Subtitles: No",
            ]
        );
    }

    /// The rule that matters most in the panel: a probe that could not enumerate a file
    /// which *does* have audio must never render as "No audio".
    #[test]
    fn an_unenumerable_catalog_never_renders_as_no_audio() {
        let mut core = test_core();
        let cat =
            pb_decode::MediaTrackCatalog::unavailable(1, pb_decode::MediaBackend::MediaFoundation);
        seed_details(&mut core, 0, Some(cat), Some(true));
        let rows = seeded_rows(&core, 0);
        assert_eq!(rows, vec!["Audio: Present — details unavailable"]);
        assert!(!rows.iter().any(|r| r == "Audio: No"));
    }

    /// A still (no catalog) adds no track rows at all.
    #[test]
    fn a_still_adds_no_track_rows() {
        let mut core = test_core();
        seed_details(&mut core, 0, None, None);
        assert!(seeded_rows(&core, 0).is_empty());
    }

    // -- the off-thread Details probe (task 98.6) ---------------------------

    /// Land a probe result as the worker would, so the staleness rules can be driven
    /// without a real container.
    fn fake_probe(core: &mut AppCore, gen: u64, item: usize, identity: &str) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(crate::app_core::ItemDetails {
            size: 99,
            fields: vec![("Video codec".into(), "HEVC".into())],
            media: None,
            has_audio: Some(true),
            probe_state: crate::media_details::ProbeState::Ready,
            dovi_incompatible: false,
        })
        .unwrap();
        core.exif_cache
            .insert(item, crate::app_core::ItemDetails::loading());
        core.details_probe = Some(crate::media_details::DetailsProbe {
            gen,
            item,
            identity: identity.to_string(),
            copy_when_done: false,
            rx,
        });
    }

    #[test]
    fn a_landed_probe_replaces_the_loading_entry_and_refreshes_the_panel() {
        let mut core = test_core();
        core.source = five_photos();
        core.details_gen = 3;
        let name = core.source.name(1).to_string();
        fake_probe(&mut core, 3, 1, &name);

        core.effects.clear();
        core.poll_details_probe();
        let d = core.exif_cache.get(&1).expect("cached");
        assert_eq!(d.probe_state, crate::media_details::ProbeState::Ready);
        assert_eq!(d.size, 99);
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)));
        assert!(core.details_probe.is_none());
    }

    /// The headline staleness rule: a probe that lands after a deck rebuild describes a
    /// *different file* at that index, so it must be dropped, not cached.
    #[test]
    fn a_probe_landing_after_a_deck_rebuild_is_rejected() {
        let mut core = test_core();
        core.source = five_photos();
        let name = core.source.name(1).to_string();
        fake_probe(&mut core, 3, 1, &name);
        core.details_gen = 4; // the deck was rebuilt while the worker ran

        core.poll_details_probe();
        assert_eq!(
            core.exif_cache.get(&1).map(|d| d.probe_state),
            Some(crate::media_details::ProbeState::Loading),
            "the stale result must not overwrite the entry"
        );
        assert!(core.details_probe.is_none());
    }

    /// The subtler one the generation alone can't catch: same deck generation, but index
    /// `item` now names a different file.
    #[test]
    fn a_probe_whose_item_now_names_a_different_file_is_rejected() {
        let mut core = test_core();
        core.source = five_photos();
        fake_probe(&mut core, 0, 1, "some-other-file.mp4");

        core.poll_details_probe();
        assert_eq!(
            core.exif_cache.get(&1).map(|d| d.probe_state),
            Some(crate::media_details::ProbeState::Loading),
            "identity mismatch must reject the result"
        );
    }

    /// A dead worker must not leave the entry stuck on "Reading…" forever — the
    /// placeholder is also the spawn guard, so a stuck `Loading` would never re-probe.
    #[test]
    fn a_dead_worker_marks_the_entry_failed_rather_than_hanging_on_loading() {
        let mut core = test_core();
        core.source = five_photos();
        let name = core.source.name(1).to_string();
        let (tx, rx) = std::sync::mpsc::channel::<crate::app_core::ItemDetails>();
        drop(tx); // the worker died without sending
        core.exif_cache
            .insert(1, crate::app_core::ItemDetails::loading());
        core.details_probe = Some(crate::media_details::DetailsProbe {
            gen: core.details_gen,
            item: 1,
            identity: name,
            copy_when_done: false,
            rx,
        });

        core.poll_details_probe();
        assert_eq!(
            core.exif_cache.get(&1).map(|d| d.probe_state),
            Some(crate::media_details::ProbeState::Failed)
        );
        assert!(core.details_probe.is_none());
    }

    /// An in-flight probe keeps the loop ticking, or the result could sit unread in the
    /// channel on an otherwise idle app.
    #[test]
    fn an_in_flight_probe_keeps_the_loop_polling() {
        let mut core = test_core();
        core.source = five_photos();
        let quiet = core.work_pending();
        let (_tx, rx) = std::sync::mpsc::channel::<crate::app_core::ItemDetails>();
        core.details_probe = Some(crate::media_details::DetailsProbe {
            gen: core.details_gen,
            item: 1,
            identity: core.source.name(1).to_string(),
            copy_when_done: false,
            rx,
        });
        assert!(
            core.work_pending(),
            "work_pending was {quiet} before the probe"
        );
    }

    /// A deck rebuild drops the in-flight probe and bumps the generation, so nothing from
    /// the old deck can land.
    #[test]
    fn entering_the_empty_state_cancels_the_probe_and_bumps_the_generation() {
        let mut core = test_core();
        core.source = five_photos();
        let (_tx, rx) = std::sync::mpsc::channel::<crate::app_core::ItemDetails>();
        core.details_probe = Some(crate::media_details::DetailsProbe {
            gen: core.details_gen,
            item: 1,
            identity: "x".into(),
            copy_when_done: false,
            rx,
        });
        let gen = core.details_gen;
        core.enter_empty_state();
        assert!(core.details_probe.is_none());
        assert!(core.details_gen > gen);
        assert!(core.exif_cache.is_empty());
    }

    /// The Details generation must NOT be the geometry epoch: a window resize bumps that
    /// one, and would throw away perfectly good catalogs (and, worse, a rebuild would not
    /// bump it at all).
    #[test]
    fn the_details_generation_is_a_deck_generation_not_the_geometry_epoch() {
        let mut core = test_core();
        core.source = five_photos();
        let gen = core.details_gen;
        core.epoch += 1; // a resize
        assert_eq!(
            core.details_gen, gen,
            "geometry must not touch the deck gen"
        );
    }

    /// "Copy Image Details" during an in-flight probe must not hand over a table missing
    /// the video's rows — it defers and copies the complete set when the probe lands.
    #[cfg(any(windows, target_os = "macos", all(unix, feature = "ffvideo")))]
    #[test]
    fn copy_details_mid_probe_defers_and_copies_the_complete_set() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video/multitrack.mp4");
        let mut core = test_core();
        core.source = Arc::new(FsSource::new(vec![fixture]));
        core.playlist = Playlist::new(1, 0);
        core.displayed_item = Some(0);

        core.copy_image_details(); // cold: the probe has only just started
        assert!(
            core.details_probe
                .as_ref()
                .is_some_and(|p| p.copy_when_done),
            "the copy must be deferred, not served from a half-empty cache"
        );
        assert!(
            !core.effects.iter().any(|e| matches!(
                e,
                contract::CoreEffect::WriteClipboard(contract::ClipboardPayload::Text { .. })
            )),
            "nothing may reach the clipboard before the probe lands"
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while core.details_probe.is_some() && std::time::Instant::now() < deadline {
            core.poll_details_probe();
            std::thread::sleep(Duration::from_millis(5));
        }
        let copied = core
            .effects
            .iter()
            .find_map(|e| match e {
                contract::CoreEffect::WriteClipboard(contract::ClipboardPayload::Text {
                    text,
                    ..
                }) => Some(text.clone()),
                _ => None,
            })
            .expect("the deferred copy landed");
        assert!(copied.contains("Video codec: H.264"), "{copied}");
        assert!(
            copied.contains("Audio"),
            "the tracks are in the copy: {copied}"
        );
        assert!(copied.contains("Track 1"), "{copied}");
    }

    /// **The plan's acceptance criterion for 98.7**: a loose MKV and *the same MKV inside a
    /// ZIP* must show identical Details. Before this, the archived one had no path, so every
    /// probe skipped it and the panel showed nothing.
    ///
    /// `ffvideo`-gated because that is the build where FFmpeg can read the entry's bytes —
    /// which is every build that ships or plays video: the DMG (`--bundle-ffmpeg` implies
    /// `--ffvideo`) and dev builds (`--ffvideo` by default).
    #[cfg(feature = "ffvideo")]
    #[test]
    fn an_archived_video_reports_the_same_details_as_the_loose_file() {
        use std::io::Write;
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video/multitrack.mkv");
        let bytes = std::fs::read(&fixture).expect("fixture");

        // The loose file's catalog.
        let loose = crate::media_details::probe_job(&FsSource::new(vec![fixture]), 0, 1);
        let loose_cat = loose.media.as_ref().expect("loose catalog");

        // The same bytes inside a ZIP.
        let zip_path = std::env::temp_dir().join(format!("pb_98_7_{}.zip", std::process::id()));
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            zw.start_file("clip.mkv", zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(&bytes).unwrap();
            zw.finish().unwrap();
        }
        let zs =
            pb_source::ZipSource::open(&zip_path, None, |ext| ext == "mkv").expect("open the zip");
        let archived = crate::media_details::probe_job(&zs, 0, 1);
        let _ = std::fs::remove_file(&zip_path);

        // The archive entry genuinely has no path — this is the case that used to be skipped.
        assert!(zs.path(0).is_none(), "the premise: an entry has no path");

        let arch_cat = archived
            .media
            .as_ref()
            .expect("the archived video must get a catalog too");
        assert_eq!(
            arch_cat, loose_cat,
            "a loose MKV and the same MKV in a ZIP must enumerate identically"
        );
        assert_eq!(archived.fields, loose.fields, "same basic facts");
        assert_eq!(archived.has_audio, loose.has_audio);
        // ...and it renders as the same rows.
        assert_eq!(
            crate::tracks::track_rows(arch_cat, archived.has_audio),
            crate::tracks::track_rows(loose_cat, loose.has_audio)
        );
        assert_eq!(arch_cat.audio.tracks.len(), 2);
        assert_eq!(arch_cat.subtitles.tracks.len(), 4);
    }

    /// The thin Swift round-trip races the Rust worker by construction, so it must never
    /// overwrite a richer catalog-bearing entry just by landing second.
    #[test]
    fn the_shell_archive_round_trip_never_clobbers_a_richer_catalog() {
        let mut core = test_core();
        let cat = pb_decode::MediaTrackCatalog::new(
            1,
            pb_decode::MediaBackend::FFmpeg,
            pb_decode::TrackSet::complete(vec![track("AAC", "eng")]),
            pb_decode::TrackSet::complete(vec![]),
        );
        seed_details(&mut core, 2, Some(cat), Some(true));

        core.archive_video_meta_ready(2, "HEVC".to_string(), 30_000, 5_000, true);

        let d = core.exif_cache.get(&2).expect("still cached");
        assert!(d.media.is_some(), "the catalog must survive");
        assert!(
            !d.fields.iter().any(|(k, _)| k == "Audio"),
            "the placeholder Audio row must not come back"
        );
        // ...but it still populates an entry that has no catalog.
        core.exif_cache.remove(&2);
        core.archive_video_meta_ready(2, "HEVC".to_string(), 30_000, 5_000, true);
        assert!(core
            .exif_cache
            .get(&2)
            .expect("cached")
            .fields
            .iter()
            .any(|(k, v)| k == "Video codec" && v == "HEVC"));
    }

    /// The real thing, end to end: a real container, the real worker, the real poll.
    /// `ensure_exif_cached` must return **without** the catalog (it did not block), and
    /// the catalog must arrive on a later tick.
    #[cfg(any(windows, target_os = "macos", all(unix, feature = "ffvideo")))]
    #[test]
    fn a_real_video_probes_off_thread_and_lands_its_catalog() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video/multitrack.mp4");
        assert!(fixture.exists(), "fixture missing: {}", fixture.display());

        let mut core = test_core();
        core.source = Arc::new(FsSource::new(vec![fixture]));
        core.playlist = Playlist::new(1, 0);
        core.displayed_item = Some(0);

        core.ensure_exif_cached(0);
        // The event loop was not made to wait for the container open.
        assert_eq!(
            core.exif_cache.get(&0).map(|d| d.probe_state),
            Some(crate::media_details::ProbeState::Loading),
            "ensure_exif_cached must not block on the probe"
        );
        assert!(core.details_probe.is_some());
        assert!(core.work_pending(), "the probe must keep the loop ticking");

        // Spin the poll as `tick` would, with a generous bound so a slow machine can't
        // flake but a genuine hang still fails.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while core.details_probe.is_some() && std::time::Instant::now() < deadline {
            core.poll_details_probe();
            std::thread::sleep(Duration::from_millis(5));
        }
        let d = core.exif_cache.get(&0).expect("cached");
        assert_eq!(
            d.probe_state,
            crate::media_details::ProbeState::Ready,
            "probe never landed"
        );
        let cat = d.media.as_ref().expect("catalog landed");
        assert_eq!(cat.audio.tracks.len(), 2, "the fixture's two audio tracks");
        assert_eq!(d.has_audio, Some(true));
        assert!(d
            .fields
            .iter()
            .any(|(k, v)| k == "Video codec" && v == "H.264"));
        // ...and it renders as real rows.
        let rows = crate::tracks::track_rows(cat, d.has_audio);
        assert!(matches!(&rows[0], DetailRow::Span { text, bold: true } if text == "Audio"));
    }

    /// The rows stay `Span`/`Pair`, so the Details copy payload (#32) keeps working
    /// with no shell changes.
    #[test]
    fn track_rows_round_trip_through_the_details_copy_payload() {
        let mut core = test_core();
        let cat = pb_decode::MediaTrackCatalog::new(
            1,
            pb_decode::MediaBackend::FFmpeg,
            pb_decode::TrackSet::complete(vec![track("AAC", "eng")]),
            pb_decode::TrackSet::unavailable(),
        );
        seed_details(&mut core, 0, Some(cat.clone()), Some(true));
        let panel = crate::panels::DetailsPanel {
            rows: crate::tracks::track_rows(&cat, Some(true)),
        };
        assert_eq!(
            panel.copy_text(),
            "Audio\nTrack 1: English · AAC stereo · 48 kHz"
        );
    }

    /// A shell-probed archive-video's facts become the inspector's rows (codec/fps/duration/
    /// audio) and re-signal the panel; unknown duration is omitted.
    #[test]
    fn archive_video_meta_ready_builds_inspector_rows() {
        let mut core = test_core();
        core.archive_video_meta_ready(2, "HEVC".to_string(), 30_000, 5_000, true);
        let rows = &core
            .exif_cache
            .get(&2)
            .expect("rows cached for item 2")
            .fields;
        assert!(rows.iter().any(|(k, v)| k == "Video codec" && v == "HEVC"));
        assert!(rows
            .iter()
            .any(|(k, v)| k == "Frame rate" && v == "30.00 fps"));
        assert!(rows.iter().any(|(k, _)| k == "Duration"));
        assert!(rows.iter().any(|(k, v)| k == "Audio" && v == "Yes"));
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)));

        // Unknown duration (-1) is omitted; no audio reads "No".
        core.archive_video_meta_ready(3, "H.264".to_string(), 0, -1, false);
        let rows = &core.exif_cache.get(&3).unwrap().fields;
        assert!(
            !rows.iter().any(|(k, _)| k == "Duration"),
            "unknown duration omitted"
        );
        assert!(
            !rows.iter().any(|(k, _)| k == "Frame rate"),
            "unknown fps omitted"
        );
        assert!(rows.iter().any(|(k, v)| k == "Audio" && v == "No"));
    }

    /// Poster byte stashes are keyed by request id and consumed once.
    #[test]
    fn pending_poster_bytes_keyed_and_taken_once() {
        let mut core = test_core();
        core.pending_poster_bytes.insert(7, vec![9, 8, 7]);
        assert!(
            core.take_pending_poster_bytes(99).is_empty(),
            "wrong id → nothing"
        );
        assert_eq!(core.take_pending_poster_bytes(7), vec![9, 8, 7]);
        assert!(
            core.take_pending_poster_bytes(7).is_empty(),
            "consumed once"
        );
    }

    /// Frame-step on the native backend emits a `StepVideo` intent for the displayed item,
    /// and no-ops for a stale/mismatched item.
    #[test]
    fn native_frame_step_emits_step_intent() {
        use crate::video::VideoSessionId;
        use crate::video_native::{ActiveVideoBackend, NativeVideoProxy};

        let mut core = test_core();
        core.displayed_item = Some(0);
        core.video = Some(ActiveVideoBackend::Native(NativeVideoProxy::new(
            0,
            VideoSessionId(9),
            false,
        )));

        core.effects.clear();
        assert!(core.video_frame_step(1));
        assert!(core.effects.iter().any(|e| matches!(e,
            contract::CoreEffect::StepVideo { session_id, forward: true } if session_id.0 == 9)));

        // Displayed item moved on: the step is dropped (a stale key press).
        core.displayed_item = Some(5);
        core.effects.clear();
        assert!(!core.video_frame_step(-1));
        assert!(!core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::StepVideo { .. })));
    }

    /// Owner-reported (79.10 smoke): a resize drag stalls the presenter (the OS
    /// modal loop) while audio plays on — playback must freeze *together* and
    /// resume together at settle, exactly where it froze. (The clock-catch-up
    /// alternative raced or seek-churned — tried, regressed, reverted.)
    #[test]
    fn resize_pauses_playback_and_settle_resumes_it() {
        use crate::video::{VideoProducerEvent, VideoSessionId, VideoSessionState};
        use crate::video_native::ActiveVideoBackend;
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.playlist = Playlist::new(1, 0);
        core.displayed_item = Some(0);
        let (session, io) = VideoSession::new(VideoSessionId(1), 16);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(1),
                duration: Some(Duration::from_secs(10)),
                width: 2,
                height: 2,
                has_audio: false,
                frame_bytes: 16,
            })
            .unwrap();
        let frame = |pts_ms: u64| pb_decode::VideoFrame {
            session_id: VideoSessionId(1),
            seek_generation: crate::video::SeekGeneration::FIRST,
            pts: Duration::from_millis(pts_ms),
            width: 2,
            height: 2,
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 16],
            color: pb_decode::video::VideoColorInfo::srgb(),
        };
        io.events.send(VideoProducerEvent::Frame(frame(0))).unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33)))
            .unwrap();
        core.poll_video();
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Playing
        );

        // A resize lands mid-playback: freeze together.
        core.effects.clear();
        core.resize(320, 200, 1.0);
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Paused,
            "resize pauses the session"
        );
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PauseVideoAudio)),
            "…and the audio with it"
        );
        assert!(core.video_paused_by_resize);

        // The settle deadline passes: resume together, exactly where frozen.
        core.effects.clear();
        core.resize_settle_at = Some(core.now - Duration::from_millis(1));
        core.handle(contract::CoreEvent::Tick(Instant::now()));
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Playing,
            "settle resumes the session"
        );
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ResumeVideoAudio)),
            "…and the audio with it"
        );
        assert!(!core.video_paused_by_resize, "one-shot");
        drop(io);
    }

    /// Minimizing reports a 0×0 client area. Treating it as a real resize clamps the fit to
    /// 1×1, decodes the photo to one pixel, and flashes that (upscaled to a solid color) when
    /// the window is restored. The fit must survive a 0×0 so a restore to the same size is a
    /// no-op rebind, not a re-decode.
    #[test]
    fn a_minimize_to_zero_size_leaves_the_fit_untouched() {
        let mut core = test_core();
        core.resize(3840, 2160, 1.0);
        let fit = core.fit;
        assert_eq!(
            fit,
            Some(FitBox {
                max_width: 3840,
                max_height: 2160,
            })
        );

        core.resize(0, 0, 1.0); // minimize
        assert_eq!(core.fit, fit, "a 0×0 minimize must not clobber the fit");
        assert_eq!(core.viewport.width, 3840, "…nor the viewport");
        assert_eq!(core.viewport.height, 2160);

        // Restore to the same size: fit is unchanged, so `resize` short-circuits and never
        // schedules a re-decode — which is what keeps the restore instant instead of flashing.
        core.resize_settle_at = None;
        core.resize(3840, 2160, 1.0);
        assert_eq!(core.fit, fit);
        assert!(
            core.resize_settle_at.is_none(),
            "restore to the same size must not schedule a re-decode"
        );
    }

    // ── task #18 finding #5: epoch-aware readiness / off-loop geometry re-decode ──

    #[test]
    fn target_caught_up_requires_the_item_and_the_current_epoch() {
        let mut core = test_core();
        // No target: never caught up, nothing pending (the loop can sleep).
        assert!(!core.target_caught_up());
        assert!(!core.target_pending());

        core.target_item = Some(0);
        // Target set but not shown yet: pending, not caught up.
        assert!(!core.target_caught_up());
        assert!(core.target_pending());

        // Shown at the current epoch: caught up.
        core.mark_resolved(0);
        assert!(core.target_caught_up());
        assert!(!core.target_pending());

        // A geometry change bumps the epoch, so the SAME on-screen item is stale — pending
        // again even though displayed_item == target_item (the finding #5 bug: it used to
        // read as caught-up and the fresh decode was never presented).
        core.invalidate_geometry();
        assert_eq!(
            core.displayed_item,
            Some(0),
            "invalidate_geometry must not drop the shown item"
        );
        assert!(!core.target_caught_up());
        assert!(core.target_pending());

        // Re-presenting at the new epoch catches up again.
        core.mark_resolved(0);
        assert!(core.target_caught_up());
    }

    #[test]
    fn invalidate_geometry_preserves_current_metadata() {
        // A resize / scale-mode change (incl. the video-resize path, which reads
        // `displayed_item`/`current`) must NOT drop the current photo's metadata — only a
        // genuinely new deck (`rebuild_playlist`) clears it.
        use crate::meta::PhotoMeta;
        let mut core = test_core();
        core.current = Some(PhotoMeta {
            rel: "a.jpg".into(),
            w: 100,
            h: 80,
            size: None,
            codec: "PNG",
            animated: None,
        });
        core.invalidate_geometry();
        assert_eq!(
            core.current.as_ref().map(|m| (m.w, m.h)),
            Some((100, 80)),
            "invalidate_geometry must keep `current`"
        );
    }

    // ── #106.7 §2: content_gen is split from the geometry epoch ──

    #[test]
    fn bare_geometry_invalidation_keeps_the_content_generation() {
        // A resize / fit-toggle bumps only the geometry epoch; the content generation is
        // unchanged, so a retained full-res Original decoded for the same pixels survives it.
        let mut core = test_core();
        let (e0, c0) = (core.epoch, core.content_gen);
        core.invalidate_geometry();
        assert_eq!(core.epoch, e0.wrapping_add(1), "geometry epoch advances");
        assert_eq!(
            core.content_gen, c0,
            "content generation is unchanged by a resize"
        );
    }

    #[test]
    fn scale_mode_change_is_geometry_only_not_content() {
        // Fit↔1:1 is the central #106.7 toggle: it must NOT purge retained Originals.
        let mut core = test_core();
        let c0 = core.content_gen;
        let e0 = core.epoch;
        core.set_scale_mode(ScaleMode::Original);
        assert_eq!(core.content_gen, c0, "a scale-mode change is geometry-only");
        assert_eq!(
            core.epoch,
            e0.wrapping_add(1),
            "…but still bumps the geometry epoch"
        );
    }

    #[test]
    fn invalidate_content_bumps_both_generations() {
        let mut core = test_core();
        let (e0, c0) = (core.epoch, core.content_gen);
        core.invalidate_content();
        assert_eq!(
            core.content_gen,
            c0.wrapping_add(1),
            "content generation advances"
        );
        assert_eq!(
            core.epoch,
            e0.wrapping_add(1),
            "a content change also invalidates geometry"
        );
    }

    #[test]
    fn a_new_deck_advances_the_content_generation() {
        // A deck rebuild reassigns every index — index N now names different pixels, so the
        // content generation must advance (this is what purges retained Originals).
        let mut core = test_core();
        let c0 = core.content_gen;
        let root = PathBuf::from("photos");
        let src: Arc<dyn ItemSource> =
            Arc::new(FsSource::new(vec![root.join("a.jpg"), root.join("b.jpg")]));
        core.rebuild_playlist(src, root, None, false, 0);
        assert_eq!(
            core.content_gen,
            c0.wrapping_add(1),
            "a new deck is a content change"
        );
    }

    // ── #106.7: parked full-res tier + instant Fit↔1:1 rebind ──

    fn photos_named(names: &[&str]) -> Arc<dyn ItemSource> {
        Arc::new(FsSource::new(
            names
                .iter()
                .map(|n| PathBuf::from(format!("p/{n}")))
                .collect(),
        ))
    }

    fn meta_dims(rel: &str, w: u32, h: u32) -> crate::meta::PhotoMeta {
        crate::meta::PhotoMeta {
            rel: rel.into(),
            w,
            h,
            size: None,
            codec: "JPEG",
            animated: None,
        }
    }

    // --- Poster selection (task #114, phase 1) ------------------------------

    fn poster_payload(item: usize, fitted: (u32, u32)) -> pb_decode::PosterSelection {
        let img = |w: u32, h: u32| pb_decode::DecodedImage {
            width: w,
            height: h,
            orig_width: 3840,
            orig_height: 2160,
            codec: "HEVC",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![128; (w * h * 4) as usize],
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        };
        pb_decode::PosterSelection {
            choice: pb_decode::PosterChoice {
                origin_hns: 0,
                relative_hns: item as i64 * 10_000_000,
                native_w: 3840,
                native_h: 2160,
                content_hdr: false,
            },
            fit_img: Some(img(fitted.0, fitted.1)),
            thumb_img: Some(img(64, 36)),
            native: None,
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_video_display_want_becomes_one_selection_not_an_image_decode() {
        // Emission (Windows-gated: poster_select_supported): a non-resident video
        // in the window records display demand in the ledger; photos never enter.
        let mut core = test_core();
        core.source = photos_named(&["a.jpg", "clip.mkv", "b.jpg"]);
        core.playlist = Playlist::new(3, 0).with_cursor(1);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.request_prefetch();
        assert_eq!(
            core.poster_sel.demands(1),
            (false, true),
            "the video is selecting with display demand"
        );
        assert_eq!(
            core.poster_sel.demands(0),
            (false, false),
            "photos never enter the selection ledger"
        );
        // Level-triggered: a second pass keeps it selecting (no panic, no dupe).
        core.request_prefetch();
        assert_eq!(core.poster_sel.demands(1), (false, true));
    }

    #[test]
    fn a_selection_payload_fans_out_choice_and_display_fit() {
        let mut core = test_core();
        core.source = photos_named(&["clip.mkv"]);
        core.playlist = Playlist::new(1, 0);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.targets = vec![0];
        core.ring = ResidentRing::new(4); // headless cores start with 0 slots
        core.poster_sel.reset(core.content_gen);
        core.poster_sel
            .want(0, crate::poster_select::Demand::Display);
        core.pending_uploads.push(Outcome::synthetic_selection(
            0,
            core.content_gen,
            core.epoch,
            core.decode_fit(),
            Ok(poster_payload(0, (800, 450))),
        ));
        core.thumbs.enabled = true; // the tile must land the same tick
        core.drain_results();
        assert!(core.poster_sel.choice(0).is_some(), "the choice installed");
        assert!(
            core.ring.slot_for_rep(0, pb_core::RepKind::Fit).is_some(),
            "the Fit artifact rode the normal upload path into the ring"
        );
        assert!(
            !core.preview_resident.contains(&0),
            "a selected poster is a definitive full, never a preview"
        );
        assert_eq!(
            core.thumbs.cache.tier(0),
            Some(pb_core::ThumbTier::Full),
            "the ready-made tile lands DIRECTLY in the cache (no droppable \
             derive-queue round trip — the poster and its thumb arrive together)"
        );
    }

    /// A poster walk cuts its thumb whether or not the strip was ever opened, and
    /// that tile must be KEPT. Discarding it was the "open the thumbnail panel late
    /// and wait 30+ seconds" report: every tile thrown away before the first open
    /// came back as a replay decode (~273 ms over SMB) at the bottom of the priority
    /// list, when we had already decoded and paid for it.
    ///
    /// The other half is just as load-bearing: retaining must NOT unlock the strip's
    /// own scheduled work. `enabled` gates thumb-fill planning and the T0 photo
    /// byproduct derive — flipping that here would make every displayed photo in a
    /// folder containing one video pay a derive on every frame of a blaze, which is
    /// the cost the gate exists to avoid.
    #[test]
    fn a_selection_tile_is_retained_before_the_strip_is_ever_opened() {
        let mut core = test_core();
        core.source = photos_named(&["clip.mkv"]);
        core.playlist = Playlist::new(1, 0);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.targets = vec![0];
        core.ring = ResidentRing::new(4);
        core.poster_sel.reset(core.content_gen);
        core.poster_sel
            .want(0, crate::poster_select::Demand::Display);
        core.pending_uploads.push(Outcome::synthetic_selection(
            0,
            core.content_gen,
            core.epoch,
            core.decode_fit(),
            Ok(poster_payload(0, (800, 450))),
        ));
        // The strip has NEVER been opened — the state a fresh session is in while
        // the ring prefetches posters.
        assert!(!core.thumbs.enabled);
        assert!(!core.thumbs.capture);
        assert!(!core.thumbs_visible());

        core.drain_results();

        assert_eq!(
            core.thumbs.cache.tier(0),
            Some(pb_core::ThumbTier::Full),
            "the tile the walk already cut is retained, not discarded — opening the \
             strip later must be a rebind, never a re-decode"
        );
        assert!(core.thumbs.capture, "retention is live");
        assert!(
            !core.thumbs.enabled,
            "retaining a free tile must NOT unlock the strip's scheduled work — \
             `enabled` still means 'the user opened the panel'"
        );
        assert!(!core.thumbs_visible(), "and the strip is still closed");
    }

    #[cfg(windows)]
    #[test]
    fn a_video_preview_want_is_an_instant_upgradeable_placeholder() {
        // Phase 1e (owner: "I get stuck if a poster hasn't landed"): a video's
        // preview want returns the flat tile instantly (the nonexistent path
        // proves zero I/O) and marked is_preview — so nav presents it at once
        // and the selection's poster upgrades it in place.
        let src = pb_source::FsSource::new(vec![PathBuf::from(r"C:\definitely\not\here\clip.mkv")]);
        let img = crate::engine::decode_item_for(
            &src,
            0,
            Some(FitBox {
                max_width: 800,
                max_height: 600,
            }),
            true,
            crate::decode_pool::Purpose::Display,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect("the placeholder needs no read");
        assert!(img.is_preview, "must stay upgradeable (never definitive)");
        assert!(img.is_well_formed());
    }

    #[test]
    fn a_selection_payload_upgrades_a_resident_placeholder_in_place() {
        // The blaze shape: the placeholder presented instantly; the walk lands
        // later and its fitted poster replaces the placeholder through the
        // normal preview->full upgrade (same slot, preview_resident cleared).
        let mut core = test_core();
        core.source = photos_named(&["clip.mkv"]);
        core.playlist = Playlist::new(1, 0);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.targets = vec![0];
        core.ring = ResidentRing::new(4); // headless cores start with 0 slots
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        core.preview_resident.insert(0);
        core.poster_sel.reset(core.content_gen);
        core.poster_sel
            .want(0, crate::poster_select::Demand::Display);
        core.pending_uploads.push(Outcome::synthetic_selection(
            0,
            core.content_gen,
            core.epoch,
            core.decode_fit(),
            Ok(poster_payload(0, (800, 450))),
        ));
        core.drain_results();
        assert!(
            core.ring.slot_for_rep(0, pb_core::RepKind::Fit).is_some(),
            "still resident after the in-place upgrade"
        );
        assert!(
            !core.preview_resident.contains(&0),
            "the fitted poster is the definitive full - placeholder retired"
        );
        assert!(core.poster_sel.choice(0).is_some());
    }

    #[cfg(windows)]
    #[test]
    fn a_cached_tile_serves_as_the_instant_video_preview() {
        // Owner insight (phase-4 smoke): when the strip already has a tile, the
        // instant stand-in should be THAT (recognizable), not the dark
        // placeholder — RAM-only reuse, straight into the upload queue with no
        // decode round-trip.
        let mut core = test_core();
        core.source = photos_named(&["clip.mkv"]);
        core.playlist = Playlist::new(1, 0);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        let demand = pb_core::ThumbDemand::centered(0, 4);
        core.thumbs.cache.insert(
            0,
            pb_core::ThumbTier::Full,
            64,
            36,
            64 * 36 * 4,
            crate::thumbs::ThumbPixels {
                rgba: vec![200; 64 * 36 * 4],
                orig_w: 3840,
                orig_h: 2160,
                codec: "HEVC",
            },
            &demand,
        );
        core.request_prefetch();
        let staged: Vec<_> = core
            .pending_uploads
            .iter()
            .filter(|o| {
                o.key.item == 0
                    && o.key.purpose == crate::decode_pool::Purpose::Display
                    && o.key.rep_kind == pb_core::RepKind::Fit
            })
            .collect();
        assert_eq!(staged.len(), 1, "the tile staged as the instant preview");
        let img = staged[0].result.as_ref().expect("tile pixels");
        assert!(img.is_preview, "upgradeable, never definitive");
        assert_eq!((img.width, img.height), (64, 36));
    }

    #[test]
    fn a_transient_display_failure_heals_on_demand_reentry_once() {
        // Phase 4: fail a photo in-window -> it stays failed while continuously
        // in demand; leave and come back -> the gate lifts (one retry); fail it
        // again -> terminal for the session.
        let mut core = test_core();
        let names: Vec<String> = (0..30).map(|i| format!("{i}.jpg")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        core.source = photos_named(&refs);
        core.playlist = Playlist::new(30, 0).with_cursor(0);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        // The failure event (as the drain records it): item 2 is in-window.
        core.retry.fail(2, true, true);
        core.failed.insert(2);
        core.request_prefetch();
        assert!(
            core.failed.contains(&2),
            "continuously in demand: no retry (that would loop)"
        );
        // Navigate far away (item 2 leaves the prefetch window)...
        core.playlist = core.playlist.clone().with_cursor(20);
        core.request_prefetch();
        assert!(core.failed.contains(&2), "absent: still gated");
        // ...and come back: the re-entry edge lifts the gate.
        core.playlist = core.playlist.clone().with_cursor(0);
        core.request_prefetch();
        assert!(
            !core.failed.contains(&2),
            "the re-entry edge lifted the failed gate (the one retry)"
        );
        // The retry also fails: terminal — no further edges ever fire.
        core.retry.fail(2, true, true);
        core.failed.insert(2);
        core.playlist = core.playlist.clone().with_cursor(20);
        core.request_prefetch();
        core.playlist = core.playlist.clone().with_cursor(0);
        core.request_prefetch();
        assert!(
            core.failed.contains(&2),
            "a second failure is terminal for the session"
        );
        assert!(core.retry.terminal(2));
    }

    #[cfg(windows)]
    #[test]
    fn a_parked_video_with_a_choice_preinstalls_its_original_via_replay() {
        // Phase 3: parked on a film with a sharp poster but no Original — the
        // parked tier asks for a replay pre-install (so the first fullscreen
        // toggle derives instantly). A mode-1-blocked video never asks.
        let mut core = test_core();
        core.source = photos_named(&["clip.mkv"]);
        core.playlist = Playlist::new(1, 0);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.ring = ResidentRing::new(4);
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]); // sharp poster resident
        core.poster_sel.reset(core.content_gen);
        let choice = poster_payload(0, (8, 8)).choice;
        assert!(core.poster_sel.choose(0, core.content_gen, choice));
        core.request_prefetch();
        assert_eq!(
            core.poster_sel.demands(0),
            (false, true),
            "the parked tier reopened the selection for the Original pre-install"
        );
        // A blocked video (mode-1 native) must stay quiet.
        let mut core = test_core();
        core.source = photos_named(&["clip.mkv"]);
        core.playlist = Playlist::new(1, 0);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.ring = ResidentRing::new(4);
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        core.poster_sel.reset(core.content_gen);
        assert!(core.poster_sel.choose(0, core.content_gen, choice));
        core.poster_sel.block_original(0);
        core.request_prefetch();
        assert_eq!(
            core.poster_sel.demands(0),
            (false, false),
            "a mode-1-blocked video never replays for an Original"
        );
    }

    #[test]
    fn a_native_winner_installs_as_the_videos_original_mode_0_only() {
        // Phase 3: a mode-0 native winner rides the drain as the video's
        // Original (so a resize GPU-derives like a photo); an enabled color
        // transform (mode 1 = unmipped, derive-rejected) must NOT install.
        let mut core = test_core();
        core.source = photos_named(&["clip.mkv", "clip2.mkv"]);
        core.playlist = Playlist::new(2, 0);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.targets = vec![0, 1];
        core.ring = ResidentRing::new(8);
        core.poster_sel.reset(core.content_gen);
        core.poster_sel
            .want(0, crate::poster_select::Demand::Display);
        core.poster_sel
            .want(1, crate::poster_select::Demand::Display);
        let native_img = |enabled: bool| {
            let mut c = pb_decode::ColorTransform::srgb();
            c.enabled = enabled;
            pb_decode::DecodedImage {
                width: 320,
                height: 180,
                orig_width: 3840,
                orig_height: 2160,
                codec: "HEVC",
                format: pb_decode::PixelFormat::Rgba8,
                pixels: vec![64; 320 * 180 * 4],
                is_preview: false,
                color: c,
                peak: 1.0,
                animated: None,
            }
        };
        let mut p0 = poster_payload(0, (800, 450));
        p0.native = Some(native_img(false)); // mode 0: installable
        let mut p1 = poster_payload(1, (800, 450));
        p1.native = Some(native_img(true)); // enabled transform: mode 1, skip
        core.pending_uploads.push(Outcome::synthetic_selection(
            0,
            core.content_gen,
            core.epoch,
            core.decode_fit(),
            Ok(p0),
        ));
        core.pending_uploads.push(Outcome::synthetic_selection(
            1,
            core.content_gen,
            core.epoch,
            core.decode_fit(),
            Ok(p1),
        ));
        core.drain_results();
        assert!(
            core.ring.slot_for_rep(0, pb_core::RepKind::Fit).is_some(),
            "the fitted poster landed"
        );
        assert!(
            core.ring
                .slot_for_rep(0, pb_core::RepKind::Original)
                .is_some(),
            "the mode-0 native installed as the Original (the resize fix)"
        );
        assert!(
            core.ring
                .slot_for_rep(1, pb_core::RepKind::Original)
                .is_none(),
            "an enabled-transform native must not install (mode-1 can't derive)"
        );
    }

    #[test]
    fn a_fresh_core_fences_selections_to_its_starting_content_gen() {
        // Review f6: a default (gen-0) selector against content_gen 1 refused
        // every install and re-walked the first video forever.
        let core = test_core();
        assert_eq!(core.poster_sel.content_gen(), core.content_gen);
    }

    #[test]
    fn a_thumb_sized_fit_artifact_never_lands_as_the_display_poster() {
        // Review f1: a thumb-only walk promoted mid-flight has the RIGHT epoch
        // but a ~thumb-sized Fit. Both tag halves must match; a mismatch drops
        // the artifact and reopens for a recut at the display fit.
        let mut core = test_core();
        core.source = photos_named(&["clip.mkv"]);
        core.playlist = Playlist::new(1, 0);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.targets = vec![0];
        core.ring = ResidentRing::new(4);
        core.poster_sel.reset(core.content_gen);
        core.poster_sel
            .want(0, crate::poster_select::Demand::Display);
        let thumb_fit = Some(crate::thumbs::thumb_fit()); // NOT the display fit
        core.pending_uploads.push(Outcome::synthetic_selection(
            0,
            core.content_gen,
            core.epoch, // epoch alone says "fresh" — the FitBox half must veto
            thumb_fit,
            Ok(poster_payload(0, (160, 90))),
        ));
        core.drain_results();
        assert!(
            core.ring.slot_for_rep(0, pb_core::RepKind::Fit).is_none(),
            "the thumb-sized artifact must not become the display poster"
        );
        // The CHOICE survives the stale artifact (phases-2/3 review f1): the
        // next emission pass captures it as a replay hint, so the recut is a
        // single seek-decode, never a fresh scored walk.
        assert!(core.poster_sel.choice(0).is_some(), "the choice survives");
    }

    #[test]
    fn a_stale_deck_selection_payload_is_dropped_wholesale() {
        let mut core = test_core();
        core.source = photos_named(&["clip.mkv"]);
        core.playlist = Playlist::new(1, 0);
        core.poster_sel.reset(core.content_gen);
        core.poster_sel
            .want(0, crate::poster_select::Demand::Display);
        let stale_gen = core.content_gen.wrapping_sub(1);
        core.pending_uploads.push(Outcome::synthetic_selection(
            0,
            stale_gen,
            core.epoch,
            core.decode_fit(),
            Ok(poster_payload(0, (800, 450))),
        ));
        core.drain_results();
        assert!(
            core.poster_sel.choice(0).is_none(),
            "another deck's walk must not install a choice under a recycled index"
        );
        assert!(
            core.ring.slot_for_rep(0, pb_core::RepKind::Fit).is_none(),
            "and its pixels must not reach the ring"
        );
    }

    #[test]
    fn a_stale_geometry_fit_artifact_drops_alone_and_reopens() {
        // The selection survives a resize; its Fit artifact does not. The choice
        // installs, the stale Fit is dropped, and the selector reopens so the
        // next pass runs one recut.
        let mut core = test_core();
        core.source = photos_named(&["clip.mkv"]);
        core.playlist = Playlist::new(1, 0);
        core.poster_sel.reset(core.content_gen);
        core.poster_sel
            .want(0, crate::poster_select::Demand::Display);
        let stale_epoch = core.epoch.wrapping_sub(1);
        core.pending_uploads.push(Outcome::synthetic_selection(
            0,
            core.content_gen,
            stale_epoch,
            core.decode_fit(),
            Ok(poster_payload(0, (800, 450))),
        ));
        core.drain_results();
        assert!(
            core.ring.slot_for_rep(0, pb_core::RepKind::Fit).is_none(),
            "the old-viewport Fit never uploads"
        );
        // The choice SURVIVES (phases-2/3 review f1) so the recut replays.
        assert!(core.poster_sel.choice(0).is_some(), "the choice survives");
        assert!(
            !core
                .poster_sel
                .want(0, crate::poster_select::Demand::Display),
            "Chosen: the emission dance (hint capture + reopen) owns the recut"
        );
    }

    #[test]
    fn a_failed_selection_maps_to_the_demand_domains_and_forgets() {
        let mut core = test_core();
        core.source = photos_named(&["clip.mkv"]);
        core.playlist = Playlist::new(1, 0);
        core.poster_sel.reset(core.content_gen);
        core.poster_sel.want(0, crate::poster_select::Demand::Thumb);
        core.poster_sel
            .want(0, crate::poster_select::Demand::Display);
        core.pending_uploads.push(Outcome::synthetic_selection(
            0,
            core.content_gen,
            core.epoch,
            core.decode_fit(),
            Err(pb_decode::DecodeError::Corrupt("truncated".into())),
        ));
        core.drain_results();
        assert!(
            core.failed.contains(&0),
            "display domain: legacy failed set"
        );
        assert!(core.thumbs.failed.contains(&0), "thumb domain: legacy set");
        assert_eq!(
            core.poster_sel.demands(0),
            (false, false),
            "the ledger forgot the item (the failed sets gate re-emission)"
        );
    }

    #[test]
    fn full_res_window_is_current_then_pin_then_neighbours() {
        let mut core = test_core();
        core.source = photos_named(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]);
        core.playlist = Playlist::new(10, 0).with_cursor(5); // current = 5
        core.compare_pin = Some(9); // a distant pin rides at priority 2
        assert_eq!(
            core.full_res_window(0),
            vec![5, 9],
            "radius 0 = current + pin"
        );
        assert_eq!(
            core.full_res_window(1),
            vec![5, 9, 6, 4],
            "current → pin → +1 → -1"
        );
        assert_eq!(core.full_res_window(2), vec![5, 9, 6, 4, 7, 3]);
    }

    #[test]
    fn full_res_window_dedupes_and_clamps_at_the_ends() {
        let mut core = test_core();
        core.source = photos_named(&["a", "b", "c"]);
        core.playlist = Playlist::new(3, 0).with_cursor(0); // current = 0 (no -1 neighbour)
        core.compare_pin = Some(1); // pin coincides with the +1 neighbour → deduped
        assert_eq!(
            core.full_res_window(1),
            vec![0, 1],
            "no wrap below 0; pin==+1 deduped"
        );
    }

    #[test]
    fn full_res_eligible_excludes_video_svg_and_gigapixel() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg", "vector.svg", "big.jpg", "clip.mkv"]);
        // A normal-size JPEG is eligible.
        core.meta_cache.insert(0, meta_dims("a.jpg", 8000, 6000)); // 48 MP
        assert!(core.full_res_eligible(0));
        // SVG rasterises per-viewport — never a fixed original.
        assert!(!core.full_res_eligible(1));
        // A gigapixel image stays fit-only (600 MP > the 200 MP ceiling).
        core.meta_cache
            .insert(2, meta_dims("big.jpg", 30000, 20000));
        assert!(!core.full_res_eligible(2));
        // A video has no still full-res.
        assert!(!core.full_res_eligible(3));
    }

    #[test]
    fn full_res_radius_is_clamped_to_the_cap() {
        let mut core = test_core();
        core.settings.full_res_radius = 250; // absurd
        assert_eq!(
            core.full_res_radius(),
            settings::FULL_RES_RADIUS_MAX as usize
        );
        core.settings.full_res_radius = 0;
        assert_eq!(core.full_res_radius(), 0);
    }

    /// Populate the ring so `item` is resident in `rep` at the core's current content gen.
    fn make_resident(
        core: &mut AppCore,
        item: usize,
        rep: pb_core::Representation,
        keep: &[usize],
    ) {
        let cg = core.content_gen;
        let res = core
            .ring
            .reserve_bytes(item, cg, rep, 64, keep)
            .expect("a free slot");
        assert!(core.ring.mark_resident(item, res.slot, cg, rep));
    }

    // ── #109 item 4: the fail-loud ring bridge ──

    /// A `Renderer` double whose `upload_slot` REFUSES every upload — the answer a real
    /// renderer gives for an out-of-bounds slot (its ring and the core's have desynced
    /// capacities). Everything else is inert; `device`/`queue` are never reached headless.
    struct RefusingUploads;

    impl pb_render::Renderer for RefusingUploads {
        fn resize(&mut self, _: u32, _: u32) {}
        fn set_image(
            &mut self,
            _: &[u8],
            _: u32,
            _: u32,
            _: pb_render::ColorTransform,
            _: bool,
            _: f32,
        ) {
        }
        fn clear_image(&mut self) {}
        fn set_view(&mut self, _: pb_render::ViewTransform) {}
        fn set_overlay(&mut self, _: Option<(&[u8], u32, u32)>, _: u32, _: u32) {}
        fn set_info_line(&mut self, _: Option<(&[u8], u32, u32)>, _: u32, _: pb_render::HAlign) {}
        fn reserve_ring(&mut self, _: usize, _: u32, _: u32) {}
        #[allow(clippy::too_many_arguments)]
        fn upload_slot(
            &mut self,
            _: usize,
            _: &[u8],
            _: u32,
            _: u32,
            _: pb_render::ColorTransform,
            _: bool,
            _: f32,
            _: bool,
        ) -> bool {
            false
        }
        fn present_slot(&mut self, _: usize) -> bool {
            true
        }
        fn surface_size(&self) -> (u32, u32) {
            (0, 0)
        }
        fn set_letterbox(&mut self, _: [u8; 3]) {}
        fn set_toast(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
        fn set_pie(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
        fn set_tree(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
        fn set_subtitle_overlay(&mut self, _: Option<(&[u8], u32, u32)>, _: f32, _: f32) {}
        fn device(&self) -> &pb_render::wgpu::Device {
            unreachable!("headless test double")
        }
        fn queue(&self) -> &pb_render::wgpu::Queue {
            unreachable!("headless test double")
        }
        fn set_egui_overlay(&mut self, _: Option<&pb_render::wgpu::Texture>) {}
        fn image_size(&self) -> (u32, u32) {
            (0, 0)
        }
        fn set_edr_headroom(&mut self, _: f32) {}
        fn hdr_surface_wants_edr(&self) -> Option<bool> {
            None
        }
        fn poll(&self) {}
        fn render(&mut self) -> Result<bool, pb_render::RenderError> {
            Ok(true)
        }
    }

    /// A definitive full-quality decode (`is_preview: false`, sized to the fit) for the
    /// #109.4 refused-upload tests.
    fn rgba_full(w: u32, h: u32, orig_w: u32, orig_h: u32) -> pb_decode::DecodedImage {
        pb_decode::DecodedImage {
            width: w,
            height: h,
            orig_width: orig_w,
            orig_height: orig_h,
            codec: "JPEG",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; (w * h * 4) as usize],
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        }
    }

    /// #109 item 4 (the fail-loud ring bridge): a refused upload must never leave the core
    /// believing the slot is resident. Before the fix `mark_resident` ran unconditionally
    /// after `upload_slot`'s silent no-op — the mirror said "resident", `present_slot` had
    /// nothing to rebind, and the desync surfaced navigations later as a frozen view.
    #[test]
    fn a_refused_upload_is_never_marked_resident_and_rolls_back() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.target_item = Some(0);
        core.renderer = Some(Box::new(RefusingUploads));
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(rgba_full(100, 67, 6000, 4000)),
        ));

        core.drain_results();

        assert!(
            core.ring.slot_for_rep(0, pb_core::RepKind::Fit).is_none(),
            "a refused upload must never be recorded resident (the silent-desync class)"
        );
        assert!(
            !core.ring.is_tracked_rep(0, pb_core::RepKind::Fit),
            "the reservation must roll back — a stuck Pending would block this item's next decode"
        );
    }

    // ── #119: decode validity domains (one staleness law) ──

    #[test]
    fn outcome_stale_truth_table() {
        use crate::decode_pool::Purpose;
        let err = || Err(pb_decode::DecodeError::Corrupt("x".into()));
        let mk = |purpose, rep, epoch, cg| {
            let mut o = Outcome::synthetic(0, epoch, cg, rep, err());
            o.key.purpose = purpose;
            o
        };
        let (e, cg) = (5u64, 3u64); // the current generations
                                    // Stale epoch, current content: Fit stale; Original/Thumb/PosterSelect valid.
        assert!(outcome_stale(
            e,
            cg,
            &mk(Purpose::Display, pb_core::RepKind::Fit, 4, 3)
        ));
        assert!(!outcome_stale(
            e,
            cg,
            &mk(Purpose::Display, pb_core::RepKind::Original, 4, 3)
        ));
        assert!(!outcome_stale(
            e,
            cg,
            &mk(Purpose::Thumb, pb_core::RepKind::Fit, 4, 3)
        ));
        assert!(!outcome_stale(
            e,
            cg,
            &mk(Purpose::PosterSelect, pb_core::RepKind::Fit, 4, 3)
        ));
        // Stale content: all four stale, current epoch or not.
        assert!(outcome_stale(
            e,
            cg,
            &mk(Purpose::Display, pb_core::RepKind::Fit, 5, 2)
        ));
        assert!(outcome_stale(
            e,
            cg,
            &mk(Purpose::Display, pb_core::RepKind::Original, 5, 2)
        ));
        assert!(outcome_stale(
            e,
            cg,
            &mk(Purpose::Thumb, pb_core::RepKind::Fit, 5, 2)
        ));
        assert!(outcome_stale(
            e,
            cg,
            &mk(Purpose::PosterSelect, pb_core::RepKind::Fit, 5, 2)
        ));
        // Current everything: valid.
        assert!(!outcome_stale(
            e,
            cg,
            &mk(Purpose::Display, pb_core::RepKind::Fit, 5, 3)
        ));
        assert!(!outcome_stale(
            e,
            cg,
            &mk(Purpose::Display, pb_core::RepKind::Original, 5, 3)
        ));
    }

    /// THE #119 repro pin: the parked Original decoded before a fullscreen toggle lands
    /// AFTER it — stale `key.epoch`, same deck. It must be admitted, reserved, and marked
    /// resident. Before the fix the drain discarded it as "stale geometry", so every
    /// toggle re-blurred until an Original finally squeaked through between presses.
    #[test]
    fn a_cross_epoch_original_is_admitted_and_marked_resident() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit; // display rep = Fit; the Original is the parked tier
        core.targets = vec![0];
        let stale_epoch = core.epoch;
        core.epoch = core.epoch.wrapping_add(1); // the toggle happened mid-decode
        core.pending_uploads.push(Outcome::synthetic(
            0,
            stale_epoch,
            core.content_gen,
            pb_core::RepKind::Original,
            Ok(rgba_full(600, 400, 600, 400)),
        ));

        core.drain_results();

        assert!(
            core.ring
                .slot_for_rep(0, pb_core::RepKind::Original)
                .is_some(),
            "the survivor lands — later toggles are a derive/rebind, never a re-decode"
        );
    }

    /// The counterpart pin: a stale-epoch FIT is wrong-size garbage and must still drop.
    #[test]
    fn a_stale_epoch_fit_is_still_dropped() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        let stale_epoch = core.epoch;
        core.epoch = core.epoch.wrapping_add(1);
        core.pending_uploads.push(Outcome::synthetic(
            0,
            stale_epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(rgba_full(50, 34, 600, 400)),
        ));
        core.drain_results();
        assert!(
            core.ring.slot_for_rep(0, pb_core::RepKind::Fit).is_none(),
            "a Fit decoded for the old viewport never installs"
        );
    }

    /// The cross-deck pin (#109.3): a stale-content Original — even at the current epoch —
    /// is another deck's pixels and must drop.
    #[test]
    fn a_cross_deck_original_is_dropped() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen.wrapping_sub(1),
            pb_core::RepKind::Original,
            Ok(rgba_full(600, 400, 600, 400)),
        ));
        core.drain_results();
        assert!(
            core.ring
                .slot_for_rep(0, pb_core::RepKind::Original)
                .is_none(),
            "another deck's pixels never install under this deck's index"
        );
    }

    /// Codex r1 f1: thumb outcomes are judged by the shared gate BEFORE the thumb routing —
    /// a cross-deck thumb can no longer be relabeled current by `offer`'s arrival-time
    /// generation stamp.
    #[test]
    fn a_cross_deck_thumb_outcome_never_reaches_the_thumb_store() {
        let mut core = test_core();
        core.thumbs.enable();
        let mut stale = Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen.wrapping_sub(1),
            pb_core::RepKind::Fit,
            Ok(rgba_full(64, 64, 64, 64)),
        );
        stale.key.purpose = crate::decode_pool::Purpose::Thumb;
        core.pending_uploads.push(stale);
        core.drain_results();
        assert!(
            !core.thumbs.working(),
            "the gate dropped it before any derive was offered"
        );
        let mut current = Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(rgba_full(64, 64, 64, 64)),
        );
        current.key.purpose = crate::decode_pool::Purpose::Thumb;
        core.pending_uploads.push(current);
        core.drain_results();
        assert!(
            core.thumbs.working(),
            "a current-deck thumb still flows to the derive thread"
        );
    }

    /// Codex r1 f2: `rebuild_ring` retention follows the validity domains — a geometry
    /// rebuild keeps content-valid staged outcomes (paid-for work) and drops staged Fits;
    /// a content rebuild clears all four kinds.
    #[test]
    fn rebuild_retention_follows_the_validity_domains() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg", "b.jpg"]);
        core.playlist = Playlist::new(2, 0).with_cursor(0);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        let (e, cg) = (core.epoch, core.content_gen);
        let img = || Ok(rgba_full(32, 32, 64, 64));
        core.pending_uploads
            .push(Outcome::synthetic(0, e, cg, pb_core::RepKind::Fit, img()));
        core.pending_uploads.push(Outcome::synthetic(
            0,
            e,
            cg,
            pb_core::RepKind::Original,
            img(),
        ));
        let mut thumb = Outcome::synthetic(1, e, cg, pb_core::RepKind::Fit, img());
        thumb.key.purpose = crate::decode_pool::Purpose::Thumb;
        core.pending_uploads.push(thumb);
        core.pending_uploads.push(Outcome::synthetic_selection(
            1,
            cg,
            e,
            None,
            Ok(poster_payload(1, (32, 32))),
        ));

        core.invalidate_geometry();
        let kinds: Vec<_> = core
            .pending_uploads
            .iter()
            .map(|o| (o.key.purpose, o.key.rep_kind))
            .collect();
        assert_eq!(
            core.pending_uploads.len(),
            3,
            "the staged Fit died with its epoch; Original/Thumb/selection survive: {kinds:?}"
        );
        assert!(!kinds.contains(&(crate::decode_pool::Purpose::Display, pb_core::RepKind::Fit)));

        // Content rebuild: re-stage ALL FOUR kinds fresh at the current generations, then
        // prove every one drops (Codex diff review: the first phase already removed the
        // Fit, so without a re-stage this wouldn't test all four).
        let (e2, cg2) = (core.epoch, core.content_gen);
        core.pending_uploads.clear();
        core.pending_uploads
            .push(Outcome::synthetic(0, e2, cg2, pb_core::RepKind::Fit, img()));
        core.pending_uploads.push(Outcome::synthetic(
            0,
            e2,
            cg2,
            pb_core::RepKind::Original,
            img(),
        ));
        let mut thumb2 = Outcome::synthetic(1, e2, cg2, pb_core::RepKind::Fit, img());
        thumb2.key.purpose = crate::decode_pool::Purpose::Thumb;
        core.pending_uploads.push(thumb2);
        core.pending_uploads.push(Outcome::synthetic_selection(
            1,
            cg2,
            e2,
            None,
            Ok(poster_payload(1, (32, 32))),
        ));
        core.invalidate_content();
        assert!(
            core.pending_uploads.is_empty(),
            "a content boundary clears every staged outcome — all four kinds"
        );
    }

    /// Codex r2: Fill and Original modes DISPLAY the Original rep — a cross-epoch survivor
    /// must not just install, it must present (table-driven over both modes).
    #[test]
    fn fill_and_original_modes_present_a_cross_epoch_original() {
        for mode in [ScaleMode::Fill, ScaleMode::Original] {
            let mut core = test_core();
            core.source = photos_named(&["a.jpg"]);
            core.playlist = Playlist::new(1, 0).with_cursor(0);
            core.ring = ResidentRing::new(4);
            core.fit = Some(FitBox {
                max_width: 100,
                max_height: 100,
            });
            core.view.mode = mode; // decode_fit() = None → display rep = Original
            core.targets = vec![0];
            core.target_item = Some(0);
            let stale_epoch = core.epoch;
            core.epoch = core.epoch.wrapping_add(1);
            core.pending_uploads.push(Outcome::synthetic(
                0,
                stale_epoch,
                core.content_gen,
                pb_core::RepKind::Original,
                Ok(rgba_full(600, 400, 600, 400)),
            ));
            core.drain_results();
            assert!(core.ring.original_slot(0).is_some(), "{mode:?}: resident");
            assert_eq!(core.displayed_item, Some(0), "{mode:?}: presented");
            assert!(core.target_caught_up(), "{mode:?}: resolved");
        }
    }

    /// Codex r2 (channel-fed, not hand-staged): stale outcomes are judged AT INGESTION —
    /// staged stale work would suppress the very want that replaces it (`pending_reps`).
    #[test]
    fn stale_channel_outcomes_are_dropped_at_ingestion() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        let tx = core.pool.test_sender();
        tx.send(Outcome::synthetic(
            0,
            core.epoch.wrapping_sub(1),
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(rgba_full(50, 34, 600, 400)),
        ))
        .unwrap();
        tx.send(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen.wrapping_sub(1),
            pb_core::RepKind::Original,
            Ok(rgba_full(600, 400, 600, 400)),
        ))
        .unwrap();
        core.request_prefetch(); // absorbs the channel before building wants
        assert!(
            core.pending_uploads.is_empty(),
            "a stale Fit (old epoch) and a cross-deck Original both die at the door"
        );
    }

    /// #119 diff review (Codex bug 1): a content-valid THUMB outcome staged across a
    /// toggle shares the `(item, Fit)` shape with the display decode but can never feed
    /// the ring — it must not suppress the display want. Pinned through the real pool:
    /// the emitted display job's (headless, erroring) outcome is the proof the want
    /// actually went out.
    #[test]
    fn a_staged_thumb_never_suppresses_the_display_want() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        let mut thumb = Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(rgba_full(64, 64, 64, 64)),
        );
        thumb.key.purpose = crate::decode_pool::Purpose::Thumb;
        core.pending_uploads.push(thumb);
        core.request_prefetch(); // must still emit the display want for item 0
        for _ in 0..200 {
            core.drain_results();
            if core.failed.contains(&0) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            core.failed.contains(&0),
            "the display want was emitted despite the staged thumb (its erroring decode landed)"
        );
    }

    /// Codex r1 f5: a staged outcome keeps the pump awake until drained — without this a
    /// parked Original landing after every Fit is resident sleeps in `pending_uploads`
    /// until the next input.
    #[test]
    fn staged_outcomes_keep_the_pump_awake() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        // The real scenario: the parked photo is sharp on screen (its display Fit is
        // resident, so the unresident-target pump arm is quiet) while the parked-tier
        // Original arrives late.
        core.targets = vec![0];
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        assert!(!core.work_pending(), "an idle core lets the pump sleep");
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Original,
            Ok(rgba_full(600, 400, 600, 400)),
        ));
        assert!(
            core.work_pending(),
            "a staged outcome must keep the pump polling to its drain"
        );
        core.drain_results();
        assert!(core.ring.original_slot(0).is_some(), "…which lands it");
        assert!(
            !core.work_pending(),
            "drained and resident — the pump may sleep"
        );
    }

    /// Codex r1 f4: every content boundary explicitly quiesces the pool — pinned through
    /// the exact path with no follow-up prefetch, `enter_empty_state` (the last-photo-
    /// deleted flow), which routes through `invalidate_content`.
    #[test]
    fn invalidate_content_quiesces_the_pool() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg", "b.jpg"]);
        core.playlist = Playlist::new(2, 0).with_cursor(0);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.request_prefetch(); // the real pool now holds display jobs

        core.enter_empty_state();

        // Already-sent (error) outcomes may still sit in the channel holding their
        // guards; drain + briefly poll until the flagged worker finishes discarding.
        for _ in 0..200 {
            core.drain_results();
            if !core.pool.has_work() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !core.pool.has_work(),
            "the content boundary cancelled every queued/in-flight pool job"
        );
        assert!(
            core.pending_uploads.is_empty(),
            "and nothing stale re-staged through the drain"
        );
    }

    // ── #122: parked-tier livelock guard + derive-before-preview on nav ──

    /// A `Renderer` double whose uploads succeed and whose `derive_fit` always works —
    /// the shape a real GPU gives when a mipped Original is resident. `device`/`queue`
    /// are never reached headless.
    struct DeriveOk;

    impl pb_render::Renderer for DeriveOk {
        fn resize(&mut self, _: u32, _: u32) {}
        fn set_image(
            &mut self,
            _: &[u8],
            _: u32,
            _: u32,
            _: pb_render::ColorTransform,
            _: bool,
            _: f32,
        ) {
        }
        fn clear_image(&mut self) {}
        fn set_view(&mut self, _: pb_render::ViewTransform) {}
        fn set_overlay(&mut self, _: Option<(&[u8], u32, u32)>, _: u32, _: u32) {}
        fn set_info_line(&mut self, _: Option<(&[u8], u32, u32)>, _: u32, _: pb_render::HAlign) {}
        fn reserve_ring(&mut self, _: usize, _: u32, _: u32) {}
        #[allow(clippy::too_many_arguments)]
        fn upload_slot(
            &mut self,
            _: usize,
            _: &[u8],
            _: u32,
            _: u32,
            _: pb_render::ColorTransform,
            _: bool,
            _: f32,
            _: bool,
        ) -> bool {
            true
        }
        fn derive_fit(
            &mut self,
            _source: pb_render::DeriveSource,
            _dst_slot: usize,
            fit_w: u32,
            fit_h: u32,
            _kernel: u32,
            _mip_bias: i32,
        ) -> Option<pb_render::DerivedFit> {
            Some(pb_render::DerivedFit {
                w: fit_w,
                h: fit_h,
                bytes: fit_w as u64 * fit_h as u64 * 8,
            })
        }
        fn present_slot(&mut self, _: usize) -> bool {
            true
        }
        fn surface_size(&self) -> (u32, u32) {
            (0, 0)
        }
        fn set_letterbox(&mut self, _: [u8; 3]) {}
        fn set_toast(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
        fn set_pie(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
        fn set_tree(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
        fn set_subtitle_overlay(&mut self, _: Option<(&[u8], u32, u32)>, _: f32, _: f32) {}
        fn device(&self) -> &pb_render::wgpu::Device {
            unreachable!("headless test double")
        }
        fn queue(&self) -> &pb_render::wgpu::Queue {
            unreachable!("headless test double")
        }
        fn set_egui_overlay(&mut self, _: Option<&pb_render::wgpu::Texture>) {}
        fn image_size(&self) -> (u32, u32) {
            (0, 0)
        }
        fn set_edr_headroom(&mut self, _: f32) {}
        fn hdr_surface_wants_edr(&self) -> Option<bool> {
            None
        }
        fn poll(&self) {}
        fn render(&mut self) -> Result<bool, pb_render::RenderError> {
            Ok(true)
        }
    }

    /// #122 item 2, the livelock pin: an Original the ring REFUSED at landing must not
    /// be re-requested by the next prefetch pass — before the `denied` latch, the same
    /// native decode repeated every pass (the owner's 30×-item-7 log).
    #[test]
    fn a_ring_refused_parked_original_is_not_re_requested() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg", "b.jpg"]);
        core.playlist = Playlist::new(2, 0).with_cursor(0);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        core.settings.full_res_radius = 1; // item 1 is in the parked window
        core.targets = vec![0, 1];
        // A tiny byte budget: item 0's displayed Fit fills it and is pinned, so item 1's
        // Original can never be admitted (nothing lower-priority to evict).
        core.ring = ResidentRing::new_with_budget(2, 2_000);
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        let res = core
            .ring
            .reserve_bytes(0, core.content_gen, fit_rep, 1_900, &[0, 1])
            .expect("seed");
        assert!(core
            .ring
            .mark_resident(0, res.slot, core.content_gen, fit_rep));
        core.ring.set_displayed(res.slot);

        // The parked-tier decode for item 1's Original lands (as if requested by an
        // earlier pass)…
        core.pending_uploads.push(Outcome::synthetic(
            1,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Original,
            Ok(rgba_full(600, 400, 600, 400)),
        ));
        core.drain_results();
        // …and the ring refuses + latches it.
        assert!(
            core.ring
                .slot_for_rep(1, pb_core::RepKind::Original)
                .is_none(),
            "over budget with only the pinned display resident — refused"
        );
        assert!(
            core.ring.denied(1, pb_core::RepKind::Original),
            "the refusal is latched"
        );

        core.request_prefetch();
        assert!(
            !core.pool.enqueued().contains(&(
                1,
                crate::decode_pool::Purpose::Display,
                pb_core::RepKind::Original
            )),
            "the refused Original is not re-requested — the livelock is broken"
        );
    }

    /// #122 item 2, the pre-flight: when the item's dims are already known (meta), a
    /// reservation the ring cannot admit is never even requested — the first wasted
    /// decode goes away too, not just the loop.
    #[test]
    fn an_inadmissible_original_want_is_never_emitted() {
        let build = |budget: u64| {
            let mut core = test_core();
            core.source = photos_named(&["a.jpg", "b.jpg"]);
            core.playlist = Playlist::new(2, 0).with_cursor(0);
            core.fit = Some(FitBox {
                max_width: 100,
                max_height: 100,
            });
            core.view.mode = ScaleMode::Fit;
            core.settings.full_res_radius = 1;
            core.ring = ResidentRing::new_with_budget(2, budget);
            let fit_rep = core.rep_of(pb_core::RepKind::Fit);
            let res = core
                .ring
                .reserve_bytes(0, core.content_gen, fit_rep, 1_900, &[0, 1])
                .expect("seed");
            assert!(core
                .ring
                .mark_resident(0, res.slot, core.content_gen, fit_rep));
            core.ring.set_displayed(res.slot);
            core.meta_cache.insert(1, meta_dims("b.jpg", 600, 400));
            core.request_prefetch();
            core.pool.enqueued().contains(&(
                1,
                crate::decode_pool::Purpose::Display,
                pb_core::RepKind::Original,
            ))
        };
        assert!(
            !build(2_000),
            "tiny budget: the dry-run refuses, the want is never emitted"
        );
        assert!(
            build(u64::MAX),
            "ample budget: the same want IS emitted (no over-suppression)"
        );
    }

    /// A movie must never be walked TWICE in one pass — once for the display poster and
    /// again for its thumbnail tile.
    ///
    /// On Windows the #114 selection unions the two demands into one job. Off Windows there
    /// is no selection, and the only guard (`pending_items`) is built from `pending_uploads`
    /// — decodes that have already RETURNED. A display poster walk still *in flight* is
    /// invisible to it, so the thumb tier scheduled a second concurrent walk of the same
    /// film. Since `thumbs_capture` now retains a video's displayed image as its tile, that
    /// second walk produces nothing the first one wasn't already going to produce: it is
    /// pure duplicated network work, competing for the very workers the first walk needs.
    #[test]
    fn a_video_is_never_walked_twice_in_one_pass_for_display_and_thumb() {
        let mut core = thumb_test_core();
        core.source = photos_named(&["film0.mkv", "film1.mkv", "film2.mkv"]);
        core.playlist = Playlist::new(3, 0).with_cursor(0);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        // Viewport BEFORE the toggle: `toggle_thumbnails` itself calls `request_prefetch`,
        // and `pool.enqueued()` is cumulative — setting it afterward would mix two planning
        // passes into one assertion.
        core.thumbs.viewport = Some(((0, 2), (0, 2)));
        core.toggle_thumbnails();

        let log = core.pool.enqueued();
        for item in 0..3 {
            let display = log
                .iter()
                .any(|&(i, p, _)| i == item && p == crate::decode_pool::Purpose::Display);
            let thumb = log
                .iter()
                .any(|&(i, p, _)| i == item && p == crate::decode_pool::Purpose::Thumb);
            assert!(
                !(display && thumb),
                "item {item} got BOTH a display and a thumb walk in one pass — \
                 the display poster already becomes the tile; log: {log:?}"
            );
        }
    }

    /// …but a film with NO display want still gets its own thumb walk. The suppression above
    /// must key on "a display walk is coming", never on "it is a video" — otherwise films
    /// outside the display window (the strip's warm range is far wider) would never fill.
    #[test]
    fn a_video_outside_the_display_window_still_gets_its_own_thumb_walk() {
        let mut core = thumb_test_core();
        let names: Vec<String> = (0..40).map(|i| format!("film{i}.mkv")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        core.source = photos_named(&refs);
        core.playlist = Playlist::new(40, 0).with_cursor(0);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.thumbs.viewport = Some(((0, 39), (0, 39)));
        core.toggle_thumbnails();

        let log = core.pool.enqueued();
        // Accept either purpose: off Windows a far film is a `Thumb` fill, while the
        // selection platform routes the same demand through `PosterSelect`. Asserting
        // `Thumb` alone would pass here and fail on Windows, where this crate also builds.
        let far_thumb_work = log
            .iter()
            .filter(|&&(i, p, _)| {
                i > 8
                    && matches!(
                        p,
                        crate::decode_pool::Purpose::Thumb
                            | crate::decode_pool::Purpose::PosterSelect
                    )
            })
            .count();
        assert!(
            far_thumb_work > 0,
            "films beyond the display window must still be walked for a tile; log: {log:?}"
        );
    }

    /// #123 fix 1: parked, the CURRENT photo's Original ranks directly after its own
    /// display ladder — never behind the neighbour refill (each F re-queues dozens of
    /// neighbour decodes, which kept the Original perpetually at the back: the owner's
    /// "way after the pie" wait). Neighbour/pin parked jobs stay in the below-thumbs tail.
    #[test]
    fn the_current_photos_parked_original_outranks_the_neighbour_refill() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg", "b.jpg", "c.jpg"]);
        core.playlist = Playlist::new(3, 0).with_cursor(0);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        core.settings.full_res_radius = 1;

        core.request_prefetch();

        let log = core.pool.enqueued();
        let orig0 = log
            .iter()
            .position(|&k| {
                k == (
                    0,
                    crate::decode_pool::Purpose::Display,
                    pb_core::RepKind::Original,
                )
            })
            .expect("the current photo's Original is wanted");
        let first_neighbour = log
            .iter()
            .position(|&k| k.0 != 0)
            .expect("neighbour work is wanted");
        assert!(
            orig0 < first_neighbour,
            "parked: the current photo's Original must outrank the neighbour refill; log: {log:?}"
        );
        let orig1 = log.iter().position(|&k| {
            k == (
                1,
                crate::decode_pool::Purpose::Display,
                pb_core::RepKind::Original,
            )
        });
        if let Some(orig1) = orig1 {
            assert!(
                orig1 > first_neighbour,
                "…while NEIGHBOUR Originals stay in the low-priority tail; log: {log:?}"
            );
        }
    }

    // ── #123 fix 2: the geometry-pair Fit stash ──

    /// A `Renderer` double with working stash semantics: tracks the presented ring slot
    /// (so `stash_fit`'s verify-the-presented-slot rule is real) and two stash slots.
    #[derive(Default)]
    struct StashOk {
        presented: Option<usize>,
        stashed: [bool; 2],
    }

    impl pb_render::Renderer for StashOk {
        fn resize(&mut self, _: u32, _: u32) {}
        fn set_image(
            &mut self,
            _: &[u8],
            _: u32,
            _: u32,
            _: pb_render::ColorTransform,
            _: bool,
            _: f32,
        ) {
        }
        fn clear_image(&mut self) {}
        fn set_view(&mut self, _: pb_render::ViewTransform) {}
        fn set_overlay(&mut self, _: Option<(&[u8], u32, u32)>, _: u32, _: u32) {}
        fn set_info_line(&mut self, _: Option<(&[u8], u32, u32)>, _: u32, _: pb_render::HAlign) {}
        fn reserve_ring(&mut self, _: usize, _: u32, _: u32) {}
        #[allow(clippy::too_many_arguments)]
        fn upload_slot(
            &mut self,
            _: usize,
            _: &[u8],
            _: u32,
            _: u32,
            _: pb_render::ColorTransform,
            _: bool,
            _: f32,
            _: bool,
        ) -> bool {
            true
        }
        fn present_slot(&mut self, slot: usize) -> bool {
            self.presented = Some(slot);
            true
        }
        fn stash_fit(&mut self, stash_idx: usize, ring_slot: usize) -> bool {
            if stash_idx < 2 && self.presented == Some(ring_slot) {
                self.stashed[stash_idx] = true;
                true
            } else {
                false
            }
        }
        fn present_stash(&mut self, stash_idx: usize) -> bool {
            if stash_idx < 2 && self.stashed[stash_idx] {
                self.presented = None;
                true
            } else {
                false
            }
        }
        fn clear_stash(&mut self, stash_idx: usize) {
            if stash_idx < 2 {
                self.stashed[stash_idx] = false;
            }
        }
        fn surface_size(&self) -> (u32, u32) {
            (0, 0)
        }
        fn set_letterbox(&mut self, _: [u8; 3]) {}
        fn set_toast(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
        fn set_pie(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
        fn set_tree(&mut self, _: Option<(&[u8], u32, u32)>, _: u32) {}
        fn set_subtitle_overlay(&mut self, _: Option<(&[u8], u32, u32)>, _: f32, _: f32) {}
        fn device(&self) -> &pb_render::wgpu::Device {
            unreachable!("headless test double")
        }
        fn queue(&self) -> &pb_render::wgpu::Queue {
            unreachable!("headless test double")
        }
        fn set_egui_overlay(&mut self, _: Option<&pb_render::wgpu::Texture>) {}
        fn image_size(&self) -> (u32, u32) {
            (0, 0)
        }
        fn set_edr_headroom(&mut self, _: f32) {}
        fn hdr_surface_wants_edr(&self) -> Option<bool> {
            None
        }
        fn poll(&self) {}
        fn render(&mut self) -> Result<bool, pb_render::RenderError> {
            Ok(true)
        }
    }

    /// A parked core with item 0's definitive Fit resident and presented via `StashOk`.
    fn stash_test_core(fit: FitBox) -> AppCore {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg", "b.jpg"]);
        core.playlist = Playlist::new(2, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(fit);
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.target_item = Some(0);
        core.renderer = Some(Box::new(StashOk::default()));
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        let slot = core.ring.slot_for(0).expect("resident");
        core.present_item(0, slot); // displayed + the mock records the presented slot
        core
    }

    // -- #126 step 2: the archive-open lifecycle, now core-owned -----------------------------

    use crate::archive_open::ArchiveOutcome;

    /// Arm an archive open on a core with a channel the TEST drives.
    fn armed_archive_core(
        attempted: Option<crate::SecretString>,
    ) -> (
        AppCore,
        std::sync::mpsc::Sender<(u64, crate::archive_open::ArchiveResult)>,
    ) {
        let mut core = test_core();
        let (tx, rx) = std::sync::mpsc::channel();
        core.arm_archive_open(
            1,
            rx,
            pb_source::OpenProgress::new(),
            std::path::PathBuf::from("/vault/holiday.7z"),
            attempted,
        );
        (core, tx)
    }

    /// §12.6's whole point, now provable: the core cancels the displaced walk ITSELF, because
    /// both flows share one generation space. The shells used to do this by hand, and the
    /// interim step-1 code had to do it unconditionally because the open was not registered.
    #[test]
    fn an_archive_open_cancels_the_displaced_walk_inside_the_core() {
        let mut core = test_core();
        let (_tx, rx) = std::sync::mpsc::channel();
        let progress = crate::scan::ScanProgress::new();
        core.arm_dir_scan(1, rx, progress.clone(), "Photos".into());
        assert!(core.scanning);

        let (_tx2, rx2) = std::sync::mpsc::channel();
        let superseded = core.arm_archive_open(
            1,
            rx2,
            pb_source::OpenProgress::new(),
            std::path::PathBuf::from("/a.7z"),
            None,
        );

        assert!(
            matches!(superseded, Some((_, crate::background::OpKind::DirScan))),
            "the walk must be reported as displaced"
        );
        assert!(core.dir_scan.is_none(), "and dropped by the core");
        assert!(
            progress.is_cancelled(),
            "and TOLD TO STOP — forgetting the handle leaves a real walk streaming batches"
        );
        assert!(!core.scanning, "and the prefetch mirror must follow");
    }

    /// The symmetric half: a folder scan displaces an in-flight open, through the same gate.
    #[test]
    fn a_walk_cancels_the_displaced_archive_open_inside_the_core() {
        let (mut core, _tx) = armed_archive_core(None);
        let progress = core
            .archive_load
            .as_ref()
            .map(|l| l.progress.clone())
            .unwrap();

        let (_tx2, rx2) = std::sync::mpsc::channel();
        core.arm_dir_scan(2, rx2, crate::scan::ScanProgress::new(), "Photos".into());

        assert!(core.archive_load.is_none(), "the open is dropped");
        assert!(progress.is_cancelled(), "and its worker told to stop");
    }

    /// A wrong password re-prompts for the SAME archive, and says so, so a shell corrects the
    /// dialog already up instead of re-opening one.
    #[test]
    fn a_wrong_password_reprompts_for_the_same_operation() {
        let (mut core, tx) = armed_archive_core(Some(crate::SecretString::from("wrong")));
        tx.send((
            1,
            (
                Err(crate::archive::ArchiveOpenError::PasswordRequired),
                None,
            ),
        ))
        .unwrap();

        match core.poll_archive_load() {
            ArchiveOutcome::NeedPassword { path, wrong } => {
                assert!(wrong, "this attempt carried a password and it was rejected");
                assert_eq!(path, std::path::PathBuf::from("/vault/holiday.7z"));
            }
            other => panic!("expected a re-prompt, got {other:?}"),
        }
        assert_eq!(
            core.password_archive,
            Some(std::path::PathBuf::from("/vault/holiday.7z")),
            "the path is remembered so a submitted password re-opens it"
        );
    }

    /// A FIRST prompt is not a retry — the distinction the inline-error chrome depends on.
    #[test]
    fn a_first_prompt_is_not_marked_wrong() {
        let (mut core, tx) = armed_archive_core(None);
        tx.send((
            1,
            (
                Err(crate::archive::ArchiveOpenError::PasswordRequired),
                None,
            ),
        ))
        .unwrap();
        assert!(matches!(
            core.poll_archive_load(),
            ArchiveOutcome::NeedPassword { wrong: false, .. }
        ));
    }

    /// PRIVACY (plan §6): a winning password is promoted into the session cache exactly once,
    /// and is NEVER handed back to the shell — the outcome carries no secret at all.
    #[test]
    fn a_winning_password_is_promoted_once_and_never_returned() {
        let (mut core, tx) = armed_archive_core(Some(crate::SecretString::from("hunter2")));
        // An archive that opened but held nothing viewable still proves its password.
        tx.send((1, (Err(crate::archive::ArchiveOpenError::Empty), None)))
            .unwrap();
        let outcome = core.poll_archive_load();

        assert_eq!(
            core.archive_passwords.len(),
            1,
            "promoted exactly once, not per poll"
        );
        assert!(!format!("{outcome:?}").contains("hunter2"));
        assert!(
            !format!("{:?}", core.archive_passwords).contains("hunter2"),
            "the session cache must not render its secrets even in Debug"
        );
    }

    /// A superseded open's result must not rebuild the deck underneath whatever replaced it.
    #[test]
    fn a_superseded_open_applies_nothing() {
        let (mut core, tx) = armed_archive_core(None);
        // A walk supersedes it; the worker then finishes anyway.
        let (_tx2, rx2) = std::sync::mpsc::channel();
        core.arm_dir_scan(9, rx2, crate::scan::ScanProgress::new(), "Photos".into());
        let _ = tx.send((1, (Err(crate::archive::ArchiveOpenError::Empty), None)));

        assert!(matches!(core.poll_archive_load(), ArchiveOutcome::Pending));
        assert!(
            core.archive_passwords.is_empty(),
            "a stale result must not promote anything either"
        );
    }

    /// Cancel clears the handle itself and makes an in-flight result stale.
    #[test]
    fn cancelling_an_open_clears_it_and_is_idempotent() {
        let (mut core, tx) = armed_archive_core(None);
        let progress = core
            .archive_load
            .as_ref()
            .map(|l| l.progress.clone())
            .unwrap();

        core.cancel_archive_load();
        assert!(core.archive_load.is_none());
        assert!(progress.is_cancelled());
        assert_eq!(core.bg.active(), None, "the operation slot is released");

        let _ = tx.send((1, (Err(crate::archive::ArchiveOpenError::Empty), None)));
        assert!(matches!(core.poll_archive_load(), ArchiveOutcome::Pending));
        core.cancel_archive_load(); // idempotent
    }

    /// A dead worker (sender dropped with no terminal message) must not strand chrome.
    #[test]
    fn a_dead_worker_ends_the_operation_rather_than_hanging() {
        let (mut core, tx) = armed_archive_core(None);
        drop(tx);
        assert!(matches!(
            core.poll_archive_load(),
            ArchiveOutcome::Cancelled
        ));
        assert!(core.archive_load.is_none());
        assert_eq!(
            core.bg.active(),
            None,
            "every terminal path clears the slot"
        );
    }

    /// The reconciliation query, matching `scan_status`: present while in flight, gone after.
    #[test]
    fn archive_status_tracks_the_open() {
        let (mut core, _tx) = armed_archive_core(None);
        let s = core.archive_status().expect("an open is in flight");
        assert_eq!(s.name, "holiday.7z");
        assert!(!s.slow, "a fresh open warrants no chrome yet");

        core.now += crate::dir_scan::SCAN_DIALOG_DELAY;
        assert!(core.archive_status().unwrap().slow);

        core.cancel_archive_load();
        assert!(core.archive_status().is_none());
    }

    // -- #126 step 1: the directory-scan lifecycle, now core-owned --------------------------

    use crate::dir_scan::ScanDialogRequest;

    /// Arm a scan on a core with a channel the TEST drives - no thread, no sleeps. This is
    /// the deterministic completion point Codex asked an injectable runtime for; the mpsc
    /// channel already was one.
    fn armed_scan_core() -> (
        AppCore,
        std::sync::mpsc::Sender<(u64, crate::scan::ScanUpdate)>,
    ) {
        let mut core = test_core();
        let (tx, rx) = std::sync::mpsc::channel();
        core.arm_dir_scan(1, rx, crate::scan::ScanProgress::new(), "Photos".into());
        (core, tx)
    }

    /// `begin_dir_scan` must reject a non-scan source **before touching any state**. The shell
    /// copies bumped the generation and cleared tombstones first and returned late — harmless
    /// with exactly one caller, a latent bug in a generally callable core transition (§5a).
    #[test]
    fn begin_dir_scan_validates_before_it_mutates() {
        let mut core = test_core();
        core.deleted.insert(std::path::PathBuf::from("/gone.jpg"));

        let superseded = core.begin_dir_scan(
            pb_core::open::Source::Archive(std::path::PathBuf::from("/a.zip")),
            pb_core::open::Cursor::First,
        );

        assert_eq!(superseded, None);
        assert!(core.dir_scan.is_none(), "no walk was armed");
        assert_eq!(core.bg.active(), None, "no generation was claimed");
        assert!(!core.scanning);
        assert!(
            core.deleted.contains(std::path::Path::new("/gone.jpg")),
            "tombstones survive a rejected open - the shells cleared them first"
        );
    }

    /// The status query is what lets one core drive two different scan chromes: it reports
    /// `slow` and `bootstrapped` as SEPARATE facts, so a shell can show ambient chrome for the
    /// whole walk (macOS, and winit's pill) while blocking chrome hides once a photo is up.
    #[test]
    fn scan_status_reports_slow_and_bootstrapped_independently() {
        let (mut core, _tx) = armed_scan_core();
        let start = core.now;

        let s = core.scan_status().expect("a walk is in flight");
        assert_eq!(s.name, "Photos");
        assert!(!s.slow, "a fresh walk is not yet worth any chrome");
        assert!(!s.bootstrapped);

        core.now = start + crate::dir_scan::SCAN_DIALOG_DELAY;
        assert!(core.scan_status().unwrap().slow, "past the delay");

        // A photo lands. `slow` must NOT be cleared by it - they answer different questions.
        core.scan_bootstrapped = true;
        let s = core.scan_status().unwrap();
        assert!(s.slow && s.bootstrapped);

        // Unlike `should_reveal`'s latch, the query keeps answering for the whole walk.
        core.now = start + Duration::from_secs(30);
        assert!(core.scan_status().unwrap().slow);
    }

    /// No walk, no status - so chrome driven by it disappears the moment the walk ends.
    #[test]
    fn scan_status_is_none_once_the_walk_ends() {
        let (mut core, tx) = armed_scan_core();
        assert!(core.scan_status().is_some());
        tx.send((1, crate::scan::ScanUpdate::Done)).unwrap();
        core.poll_dir_scan();
        assert!(core.scan_status().is_none());
    }

    /// The sub-folder line is blanked while the walk is still in the root, so chrome does not
    /// print the headline twice. Both shells did this independently; the core does it once.
    #[test]
    fn scan_status_blanks_the_subfolder_while_it_repeats_the_headline() {
        let mut core = test_core();
        let (_tx, rx) = std::sync::mpsc::channel();
        let progress = crate::scan::ScanProgress::new();
        core.arm_dir_scan(1, rx, progress.clone(), "Photos".into());

        progress.set_current("Photos".into());
        assert_eq!(core.scan_status().unwrap().current_dir, "");

        progress.set_current("Photos/2019".into());
        assert_eq!(core.scan_status().unwrap().current_dir, "Photos/2019");
    }

    /// The invariant phase 0 exists for, now end-to-end: an archive open supersedes an
    /// in-flight walk, so the walk's late batches can never reach the deck. In the shells
    /// this needed a hand-written `cancel_dir_scan()` at the right call site, and missing it
    /// was the "door card over a photo" corruption.
    #[test]
    fn an_archive_open_supersedes_an_in_flight_scan() {
        let (mut core, tx) = armed_scan_core();
        // The archive open claims the shared generation space.
        let (_open, superseded) = core
            .bg
            .begin(crate::background::OpKind::ArchiveOpen, core.now);
        assert!(
            matches!(superseded, Some((_, crate::background::OpKind::DirScan))),
            "the displaced scan must be handed back so its worker is stopped"
        );

        // A batch the walk already sent now arrives. It must be dropped, not applied.
        tx.send((1, crate::scan::ScanUpdate::Done)).unwrap();
        let poll = core.poll_dir_scan();
        assert_eq!(poll.dialog, crate::dir_scan::ScanDialogRequest::Close);
        assert!(core.dir_scan.is_none(), "the stale walk is dropped");
        assert!(!core.scanning);
    }

    /// Arming a second walk supersedes the first through the same one gate.
    #[test]
    fn a_second_scan_supersedes_the_first() {
        let (mut core, _tx) = armed_scan_core();
        let first = core.dir_scan.as_ref().map(|s| s.id).unwrap();
        let (_tx2, rx2) = std::sync::mpsc::channel();
        core.arm_dir_scan(2, rx2, crate::scan::ScanProgress::new(), "Other".into());
        assert!(
            !core.bg.is_current(first),
            "the first walk is stale at once"
        );
        assert!(core.bg.is_current(core.dir_scan.as_ref().unwrap().id));
    }

    /// Cancel clears the handle ITSELF - the macOS shape (task #126 section 11.2). The winit
    /// copy relied on every call site clearing afterwards, and its comment overstated that
    /// they all do.
    #[test]
    fn cancel_clears_the_handle_without_help_from_the_call_site() {
        let (mut core, _tx) = armed_scan_core();
        core.cancel_dir_scan();
        assert!(core.dir_scan.is_none(), "no call-site convention required");
        assert!(!core.scanning);
        assert_eq!(core.bg.active(), None);
        core.cancel_dir_scan(); // idempotent
        assert!(core.dir_scan.is_none());
    }

    /// A slow walk with nothing on screen asks for the dialog - but only after the delay, and
    /// only once. Deterministic: `now` is moved by hand, never slept on.
    #[test]
    fn a_slow_walk_asks_for_the_dialog_once_and_only_after_the_delay() {
        let (mut core, _tx) = armed_scan_core();
        let start = core.now;

        assert_eq!(
            core.poll_dir_scan().dialog,
            ScanDialogRequest::None,
            "too soon"
        );

        core.now = start + crate::dir_scan::SCAN_DIALOG_DELAY - Duration::from_millis(1);
        assert_eq!(
            core.poll_dir_scan().dialog,
            ScanDialogRequest::None,
            "still under"
        );

        core.now = start + crate::dir_scan::SCAN_DIALOG_DELAY;
        assert_eq!(
            core.poll_dir_scan().dialog,
            ScanDialogRequest::Reveal {
                name: "Photos".into(),
                progress: crate::scan::ScanProgress::new(),
            },
            "reveals at the deadline"
        );

        core.now = start + Duration::from_secs(30);
        assert_eq!(
            core.poll_dir_scan().dialog,
            ScanDialogRequest::None,
            "and never again - the latch is what stops a per-tick re-reveal"
        );
    }

    /// The dialog must never pop over a photo that is already up: once a batch has
    /// bootstrapped the view, the walk goes quiet however slow it is.
    #[test]
    fn the_dialog_never_pops_over_an_already_bootstrapped_photo() {
        let (mut core, _tx) = armed_scan_core();
        core.scan_bootstrapped = true;
        core.now += Duration::from_secs(30);
        assert_eq!(
            core.poll_dir_scan().dialog,
            ScanDialogRequest::None,
            "a photo is on screen; a progress dialog would be an interruption"
        );
    }

    /// A finished walk that found nothing hands the folder name back so the shell can toast
    /// it, and closes the dialog. A walk that DID find photos toasts nothing.
    #[test]
    fn an_empty_walk_reports_its_folder_name_and_a_productive_one_does_not() {
        let (mut core, tx) = armed_scan_core();
        tx.send((1, crate::scan::ScanUpdate::Done)).unwrap();
        let poll = core.poll_dir_scan();
        assert_eq!(poll.dialog, ScanDialogRequest::Close);
        assert_eq!(
            poll.found_no_photos.as_deref(),
            Some("Photos"),
            "an empty folder is reported by name"
        );
        assert!(core.dir_scan.is_none(), "a terminal path clears the walk");
        assert_eq!(core.bg.active(), None, "and retires the operation");

        let (mut core, tx) = armed_scan_core();
        core.scan_bootstrapped = true; // photos were found
        tx.send((1, crate::scan::ScanUpdate::Done)).unwrap();
        let poll = core.poll_dir_scan();
        assert_eq!(poll.found_no_photos, None, "nothing to apologise for");
    }

    /// A worker that dies without sending `Done` (panic, or a dropped sender) must not strand
    /// its dialog on screen forever.
    #[test]
    fn a_dead_worker_never_strands_its_dialog() {
        let (mut core, tx) = armed_scan_core();
        drop(tx); // the worker vanished
        let poll = core.poll_dir_scan();
        assert_eq!(poll.dialog, ScanDialogRequest::Close);
        assert!(core.dir_scan.is_none());
        assert!(!core.scanning);
        assert_eq!(core.bg.active(), None);
    }

    /// Polling with no walk in flight is a no-op, not a panic - the tick calls it every frame.
    #[test]
    fn polling_with_no_walk_is_inert() {
        let mut core = test_core();
        assert_eq!(core.poll_dir_scan(), crate::dir_scan::ScanPoll::idle());
    }

    /// A batch tagged with a stale WIRE generation is skipped even while the operation id is
    /// current (belt-and-braces: the channel is per-scan, so this is defensive).
    #[test]
    fn a_batch_from_a_stale_wire_generation_is_skipped() {
        let (mut core, tx) = armed_scan_core();
        tx.send((99, crate::scan::ScanUpdate::Done)).unwrap(); // wrong generation
        drop(tx);
        let poll = core.poll_dir_scan();
        // The stale Done was skipped; the loop then saw the disconnect.
        assert_eq!(
            poll.found_no_photos, None,
            "a stale Done must not report a result"
        );
        assert!(core.dir_scan.is_none());
    }

    /// A fresh scan is a fresh universe: stale delete tombstones from the previous deck must
    /// not survive into it.
    #[test]
    fn arming_a_scan_clears_stale_delete_tombstones() {
        let mut core = test_core();
        core.deleted.insert(std::path::PathBuf::from("gone.jpg"));
        let (_tx, rx) = std::sync::mpsc::channel();
        core.arm_dir_scan(1, rx, crate::scan::ScanProgress::new(), "Photos".into());
        assert!(core.deleted.is_empty(), "fresh scan, fresh universe");
    }

    /// #126 ledger item 3, and the bug it turned out to be hiding. A scan that ends NATURALLY
    /// with an empty deck restores the "Press O to open" hint (`finish_scan`), but no cancel
    /// path did — `show_open_hint` suppresses itself while `scanning` is true, and cancelling
    /// never called it again. A cold launch into a slow folder, cancelled before the first
    /// photo, left an empty canvas with the hint still suppressed.
    #[test]
    fn cancelling_a_scan_with_an_empty_deck_restores_the_welcome_hint() {
        let (mut core, _tx) = armed_scan_core();
        assert!(core.source.is_empty() && !core.scan_bootstrapped);
        core.effects.clear();

        assert!(core.cancel_scan_command(), "a scan was running");

        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "the welcome hint must be re-shown, or the user is left on a blank canvas"
        );
        assert!(core.dir_scan.is_none());
        assert!(!core.scanning);
    }

    /// ...but a cancel that leaves a photo up must NOT blank it with a welcome hint. Same gate
    /// `finish_scan` uses.
    #[test]
    fn cancelling_a_scan_that_found_photos_leaves_the_deck_alone() {
        let (mut core, _tx) = armed_scan_core();
        core.scan_bootstrapped = true; // photos streamed in
        core.effects.clear();

        assert!(core.cancel_scan_command());

        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "a partial deck stays on screen - never replaced by the open hint"
        );
    }

    /// The command is a no-op when nothing is running, so the menu item / key is safe to spam.
    #[test]
    fn cancelling_with_no_scan_running_is_inert() {
        let mut core = test_core();
        core.effects.clear();
        assert!(!core.cancel_scan_command(), "nothing to cancel");
        assert!(core.effects.is_empty());
    }

    // -- #124: smooth zoom binds the resident Original ----------------------------------

    /// A core parked on item 0 with BOTH representations resident and a fit box that really
    /// downscales, so `Fit` and `Original` are genuinely different pixels.
    fn zoom_test_core() -> AppCore {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg", "b.jpg"]);
        core.playlist = Playlist::new(2, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.target_item = Some(0);
        core.renderer = Some(Box::new(StashOk::default()));
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        let slot = core.ring.slot_for(0).expect("resident");
        core.present_item(0, slot);
        make_resident(&mut core, 0, pb_core::Representation::Original, &[0]);
        core
    }

    /// The core selector: only Fit mode + a real zoom + a resident, genuinely-larger
    /// Original picks `Original`. Every other combination stays on `Fit`.
    #[test]
    fn present_kind_picks_the_original_only_when_zoom_needs_it_and_it_is_resident() {
        let mut core = zoom_test_core();

        // Rule 2: at or below 1:1 the fit texture is exactly right.
        core.view.zoom = 1.0;
        assert_eq!(core.present_kind(0), pb_core::RepKind::Fit, "1.0 stays Fit");
        core.view.zoom = 0.5;
        assert_eq!(
            core.present_kind(0),
            pb_core::RepKind::Fit,
            "zoom out stays Fit"
        );

        // The win case.
        core.view.zoom = 3.0;
        assert_eq!(
            core.present_kind(0),
            pb_core::RepKind::Original,
            "past 1:1 with a resident Original, bind it"
        );

        // Rule 1: Fill/Original modes already display the Original; nothing to switch.
        core.view.mode = ScaleMode::Original;
        assert_eq!(
            core.present_kind(0),
            core.display_kind(),
            "mode wins in 1:1"
        );
        core.view.mode = ScaleMode::Fit;

        // Rule 3: nothing resident to switch to; graceful, today's behaviour.
        let mut bare = test_core();
        bare.source = photos_named(&["a.jpg"]);
        bare.playlist = Playlist::new(1, 0).with_cursor(0);
        bare.ring = ResidentRing::new(4);
        bare.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        bare.view.mode = ScaleMode::Fit;
        let fit_rep = bare.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut bare, 0, fit_rep, &[0]);
        bare.view.zoom = 3.0;
        assert_eq!(
            bare.present_kind(0),
            pb_core::RepKind::Fit,
            "no resident Original, stay on Fit rather than fail"
        );
    }

    /// The whole point: zooming past 1:1 rebinds to the Original **without disturbing the
    /// zoom**. `present_item` would have reset it to 1.0 via `view_for` - the trap this
    /// feature would have died on.
    #[test]
    fn zooming_past_one_to_one_rebinds_to_the_original_and_keeps_the_zoom() {
        let mut core = zoom_test_core();
        assert_eq!(core.presented_kind, Some(pb_core::RepKind::Fit));

        core.zoom_step(3.0);

        assert_eq!(
            core.presented_kind,
            Some(pb_core::RepKind::Original),
            "the zoom bound the full-res Original"
        );
        assert!(
            (core.view.zoom - 3.0).abs() < 1e-6,
            "the rebind must not reset the zoom (got {})",
            core.view.zoom
        );

        // And zooming back out returns to the cheaper fit texture.
        core.zoom_step(1.0 / 3.0);
        assert_eq!(core.presented_kind, Some(pb_core::RepKind::Fit));
        assert!((core.view.zoom - 1.0).abs() < 1e-6);
    }

    /// A hold-to-zoom ramp calls `reconcile_zoom_rep` every tick; it must rebind ONCE on the
    /// way past 1:1, not on every tick.
    #[test]
    fn a_zoom_ramp_rebinds_once_not_every_tick() {
        let mut core = zoom_test_core();
        let before = core.rebind_count;
        for _ in 0..20 {
            core.view.zoom *= 1.1; // the ramp's own mutation
            core.reconcile_zoom_rep();
        }
        assert_eq!(core.presented_kind, Some(pb_core::RepKind::Original));
        assert_eq!(
            core.rebind_count - before,
            1,
            "20 ramp ticks past the threshold must produce exactly one rebind"
        );
    }

    /// #124 clobber path 2, the worst one: a GPU derive landing while the user is zoomed
    /// must not run `present_item` (which resets zoom to 1.0 via `view_for`) nor bind its
    /// Fit over the zoom-selected Original. Asserts the guard's own predicate, which is what
    /// both derive sites branch on.
    #[test]
    fn a_gpu_derive_declines_to_bind_over_a_zoomed_original() {
        let mut core = zoom_test_core();
        core.zoom_step(3.0);
        let bound = core.ring.original_slot(0);

        let declines = core.presented_kind == Some(pb_core::RepKind::Original)
            && core.displayed_item == Some(0);

        assert!(declines, "the derive must decline to bind while zoomed");
        assert!((core.view.zoom - 3.0).abs() < 1e-6, "zoom survives");
        assert_eq!(core.ring.original_slot(0), bound, "still the Original");
    }

    /// #124 clobber paths 1 and 3: `try_gpu_sharpen` and the `drain_results` CPU sharpen
    /// landing both rebind the Fit slot for the displayed item. Neither may fire while a
    /// zoom has selected the Original - but both are wanted again once it zooms back out.
    #[test]
    fn a_landing_sharpen_declines_over_a_zoomed_original_but_returns_on_zoom_out() {
        let mut core = zoom_test_core();
        core.zoom_step(3.0);
        let calls = core.rebind_count;

        // Both sites share this guard shape.
        let would_bind = core.displayed_item == Some(0)
            && core.presented_kind != Some(pb_core::RepKind::Original);
        assert!(
            !would_bind,
            "a landed Fit must not bind over the zoomed Original"
        );

        core.zoom_step(1.0 / 3.0);
        assert_eq!(core.presented_kind, Some(pb_core::RepKind::Fit));
        assert!(core.rebind_count > calls, "zoom-out rebinds the banked Fit");
    }

    /// The decode path must not notice the zoom at all: `decode_fit` / `display_rep` /
    /// `display_kind` stay mode-derived, so the ring, the sharpen loop and the thumbnail
    /// strip keep decoding exactly what they did before.
    #[test]
    fn a_zoom_rebind_does_not_disturb_the_decode_targets() {
        let mut core = zoom_test_core();
        let (fit, rep, kind) = (core.decode_fit(), core.display_rep(), core.display_kind());

        core.zoom_step(8.0);

        assert_eq!(core.presented_kind, Some(pb_core::RepKind::Original));
        assert_eq!(core.decode_fit(), fit, "decode target changed");
        assert_eq!(core.display_rep(), rep, "display rep changed");
        assert_eq!(core.display_kind(), kind, "display kind changed");
    }

    /// A zoom must not rebind while a nav to another item is in flight - the target's own
    /// present picks the right representation when it lands.
    #[test]
    fn zoom_does_not_rebind_mid_nav() {
        let mut core = zoom_test_core();
        core.target_item = Some(1); // nav in flight
        core.view.zoom = 3.0;
        core.reconcile_zoom_rep();
        assert_eq!(
            core.presented_kind,
            Some(pb_core::RepKind::Fit),
            "mid-nav zoom must not fight the pending present"
        );
    }

    /// A geometry change rebuilds the ring, so the representation we were bound to may not
    /// survive it. A stale `presented_kind` would make the background-rebind guards lie.
    #[test]
    fn invalidating_geometry_clears_the_presented_kind() {
        let mut core = zoom_test_core();
        core.zoom_step(3.0);
        assert_eq!(core.presented_kind, Some(pb_core::RepKind::Original));
        core.invalidate_geometry();
        assert_eq!(
            core.presented_kind, None,
            "stale rep must not survive a rebuild"
        );
    }

    /// The owner's "I literally just had these pixels" pin: capture at A, move to B,
    /// return to A — the stash re-presents with ZERO decode, and the want stays
    /// suppressed while covered.
    #[test]
    fn toggle_back_re_presents_the_stashed_fit_with_zero_decode() {
        let a = FitBox {
            max_width: 100,
            max_height: 100,
        };
        let b = FitBox {
            max_width: 200,
            max_height: 150,
        };
        let mut core = stash_test_core(a);
        core.capture_fit_stash(b); // the resize hook, first event of the A→B burst
        assert!(
            core.fit_stash
                .iter()
                .flatten()
                .any(|s| s.item == 0 && s.fit == a),
            "the outgoing A-side texture is stashed with its exact identity"
        );

        core.fit = Some(b);
        core.invalidate_geometry(); // the settle at B (epoch bump, Fit slots dropped)
        assert!(
            !core.try_present_fit_stash(),
            "at B the A-stash does not match — no false hit"
        );

        core.fit = Some(a); // toggle back
        core.invalidate_geometry();
        assert!(
            core.try_present_fit_stash(),
            "back at A: the exact pixels re-present — a rebind, not a decode"
        );
        assert_eq!(core.displayed_item, Some(0));
        assert!(core.target_caught_up(), "resolved via the stash present");

        core.request_prefetch();
        assert!(
            !core.pool.enqueued().contains(&(
                0,
                crate::decode_pool::Purpose::Display,
                pb_core::RepKind::Fit
            )),
            "a covered photo emits no display-Fit want"
        );
    }

    /// Only definitive fulls stash (Codex Q3): a preview on screen records nothing.
    #[test]
    fn a_preview_is_never_stashed() {
        let a = FitBox {
            max_width: 100,
            max_height: 100,
        };
        let b = FitBox {
            max_width: 200,
            max_height: 150,
        };
        let mut core = stash_test_core(a);
        core.preview_resident.insert(0);
        core.capture_fit_stash(b);
        assert!(
            core.fit_stash.iter().all(Option::is_none),
            "a preview must not be re-presentable as a definitive Fit"
        );
    }

    /// #109.4 discipline: a mirror entry the renderer can't honour is dropped loudly and
    /// the settle falls through to the decode ladder.
    #[test]
    fn a_stash_mirror_without_a_texture_is_dropped() {
        let a = FitBox {
            max_width: 100,
            max_height: 100,
        };
        let mut core = stash_test_core(a);
        core.renderer = None; // headless: present_stash refuses via the default
        core.fit_stash[0] = Some(FitStash {
            item: 0,
            content_gen: core.content_gen,
            fit: a,
            top_inset: core.content_top_inset,
            quarter_turned: false,
            bytes: 64,
        });
        assert!(!core.try_present_fit_stash());
        assert!(
            core.fit_stash.iter().all(Option::is_none),
            "the orphan mirror entry is dropped, not retried forever"
        );
    }

    /// The stash is current-photo-scoped: a DIFFERENT photo successfully presented
    /// retires it; content changes kill it outright.
    #[test]
    fn nav_and_content_changes_retire_the_stash() {
        let a = FitBox {
            max_width: 100,
            max_height: 100,
        };
        let b = FitBox {
            max_width: 200,
            max_height: 150,
        };
        let mut core = stash_test_core(a);
        core.capture_fit_stash(b);
        assert!(core.fit_stash.iter().flatten().count() == 1);

        // Present a DIFFERENT photo (its own resident Fit) → the stash retires.
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 1, fit_rep, &[0, 1]);
        let slot1 = core.ring.slot_for(1).expect("resident");
        core.present_item(1, slot1);
        assert!(
            core.fit_stash.iter().all(Option::is_none),
            "landing on another photo retires the pair"
        );

        // And a content change clears whatever exists at the time.
        let mut core = stash_test_core(a);
        core.capture_fit_stash(b);
        core.invalidate_content();
        assert!(core.fit_stash.iter().all(Option::is_none));
    }

    /// #122 item 1: a TAP's advance GPU-sharpens with the key still down — the derive is
    /// rebind-class cost, so only the auto-repeat (blaze) phase defers it. Before this,
    /// the sharpen waited for key-up and every advance flashed the preview even with the
    /// Original resident.
    #[test]
    fn a_tap_advance_gpu_sharpens_with_the_key_still_down() {
        let mut core = stuck_preview_core(); // held key, initial_delay huge → NOT repeating
        core.renderer = Some(Box::new(DeriveOk));
        make_resident(&mut core, 0, pb_core::Representation::Original, &[0]);
        assert!(core.held_nav().is_some(), "the key is still down");
        assert_eq!(
            core.sharpen_now(),
            None,
            "the CPU sharpen still waits for key-up (unchanged)"
        );
        assert!(
            core.try_gpu_sharpen(),
            "the GPU sharpen fires during the tap window"
        );
        assert!(
            !core.preview_resident.contains(&0),
            "the displayed photo is sharp — no preview flash on a tap"
        );
    }

    /// #122 item 1's counterpart: once the hold is genuinely blazing (past the initial
    /// delay), the GPU sharpen defers — those frames are replaced too fast to derive.
    #[test]
    fn the_blaze_repeat_phase_defers_the_gpu_sharpen() {
        let mut core = stuck_preview_core();
        core.initial_delay = Duration::ZERO; // held AND past the delay = repeating
        core.renderer = Some(Box::new(DeriveOk));
        make_resident(&mut core, 0, pb_core::Representation::Original, &[0]);
        assert!(
            !core.try_gpu_sharpen(),
            "mid-blaze the derive defers to the preview ladder"
        );
        assert!(
            core.preview_resident.contains(&0),
            "the preview stays (the blaze wants throughput, not per-frame derives)"
        );
    }

    /// #109 item 4, the in-place upgrade flavor: a refused preview→full upgrade upload must
    /// leave the upgrade bookkeeping untouched — the renderer still shows the preview, so
    /// recording "sharp now" would freeze the photo blurry with no retry.
    #[test]
    fn a_refused_upgrade_upload_keeps_the_preview_sharpen_eligible() {
        let mut core = stuck_preview_core();
        core.held.clear(); // parked — the upgrade path, not the blaze gate, is under test
        core.renderer = Some(Box::new(RefusingUploads));
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(rgba_full(100, 67, 6000, 4000)),
        ));

        core.drain_results();

        assert!(
            core.ring.slot_for(0).is_some(),
            "the preview's own residency is real and stays"
        );
        assert!(
            core.preview_resident.contains(&0),
            "refused upgrade: the preview is still what's on screen — it must stay sharpen-eligible"
        );
    }

    #[test]
    fn scale_toggle_rebinds_a_held_original_without_a_re_decode() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit; // display rep = Fit (fit is Some)
                                         // The current photo is resident as BOTH its Fit (on screen) and its Original (the
                                         // parked tier pre-decoded it) — the state a Fit↔1:1 toggle must exploit.
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        make_resident(&mut core, 0, pb_core::Representation::Original, &[0]);
        core.target_item = Some(0);
        core.mark_resolved(0);
        assert!(core.target_caught_up());
        let e0 = core.epoch;

        core.set_scale_mode(ScaleMode::Original);

        assert_eq!(
            core.epoch, e0,
            "toggling to a HELD representation must NOT bump the geometry epoch (no re-decode)"
        );
        assert_eq!(core.display_kind(), pb_core::RepKind::Original);
        assert!(
            core.target_caught_up(),
            "the held Original was rebound — nothing is pending, no re-decode"
        );
        // The Fit is still resident, so toggling straight back is instant too.
        assert!(
            core.ring.slot_for(0).is_some(),
            "the Fit slot survived the toggle"
        );
    }

    #[test]
    fn resize_with_a_held_original_arms_the_quality_monotonic_hold() {
        // The fullscreen/resize analog of the Fit↔1:1 toggle (#106.7 §6): a TRUE window resize
        // rebinds the current photo's retained full-res Original — sharp immediately, no EXIF
        // preview flash — and arms `resize_hold` so the settle re-decode's preview can't downgrade
        // the on-screen frame. Navigating away ends the hold.
        let mut core = test_core();
        core.source = photos_named(&["a.jpg", "b.jpg"]);
        core.playlist = Playlist::new(2, 0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        core.displayed_item = Some(0);
        core.target_item = Some(0);
        // Parked: the Original is held while the Fit is on screen (the parked full-res tier).
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        make_resident(&mut core, 0, pb_core::Representation::Original, &[0]);
        core.mark_resolved(0);
        assert!(core.resize_hold.is_none(), "no hold before a resize");

        // A true resize (a new fit box, e.g. windowed → fullscreen) rebinds the Original + holds.
        core.resize(1920, 1080, 1.0);
        assert_eq!(
            core.resize_hold,
            Some(0),
            "a resize with a resident Original rebinds + holds it"
        );

        // Navigating to a different photo ends the hold (its preview guard no longer applies).
        core.present_item(1, 0);
        assert_eq!(
            core.resize_hold, None,
            "nav to another photo clears the hold"
        );
    }

    #[test]
    fn resize_without_a_held_original_does_not_arm_the_hold() {
        // No parked Original (radius 0 / just-blazed / an excluded item): the resize falls through
        // to the old upscale + async re-decode, and the quality-monotonic hold is never armed — so
        // preview-first still works normally for that photo.
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        core.displayed_item = Some(0);
        core.target_item = Some(0);
        // Only the Fit is resident — no Original to rebind.
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        core.mark_resolved(0);

        core.resize(1920, 1080, 1.0);
        assert_eq!(
            core.resize_hold, None,
            "no resident Original ⇒ no hold (old preview-first path unchanged)"
        );
    }

    #[test]
    fn scale_toggle_falls_back_to_re_decode_when_the_other_rep_is_not_held() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        // Only the Fit is resident — no parked Original for the toggle to rebind.
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        core.target_item = Some(0);
        core.mark_resolved(0);
        let e0 = core.epoch;

        core.set_scale_mode(ScaleMode::Original);

        assert_eq!(
            core.epoch,
            e0.wrapping_add(1),
            "no held Original → fall back to the async re-decode (epoch bumps)"
        );
        assert!(core.target_pending(), "the re-decode is pending");
    }

    #[test]
    fn a_preview_awaiting_its_full_still_reads_as_loading() {
        // #106.5 owner note: preview-first paints an instant blurry thumbnail, so the target
        // is "caught up" — but the loading pie must keep spinning until the sharp full lands,
        // or a slow open looks finished at the blurry stage. `sharpen_now()` is that signal.
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        // The photo is on screen as a PREVIEW (blurry embedded thumbnail); the full is pending.
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        core.preview_resident.insert(0);
        core.targets = vec![0];
        core.displayed_item = Some(0);
        core.target_item = Some(0);
        core.mark_resolved(0);

        assert!(
            core.target_caught_up(),
            "the preview is presented, so the target is caught up"
        );
        assert!(
            !core.target_pending(),
            "target_pending alone would hide the pie at the blurry stage"
        );
        assert_eq!(
            core.sharpen_now(),
            Some(0),
            "…but sharpen_now flags that a sharper full is still coming — keep the pie up"
        );

        // The full upgrades in: no longer a preview → nothing left to sharpen → pie can finish.
        core.preview_resident.remove(&0);
        core.upgrade_done.insert(0);
        assert_eq!(core.sharpen_now(), None, "sharp now — the pie stops");
    }

    /// Sets up a core parked on item 0 displayed as a resident PREVIEW, with a nav key stuck
    /// held (the lost-key-up race): `held` claims Space is down, but no release will ever come.
    /// `hold_start`/`initial_delay` are pinned so the tick's step-3 advance machinery stays out
    /// of the way — the subject under test is the 3b sharpen gate, not hold-to-blaze.
    fn stuck_preview_core() -> AppCore {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        core.preview_resident.insert(0);
        core.targets = vec![0];
        core.displayed_item = Some(0);
        core.target_item = Some(0);
        core.mark_resolved(0);
        core.held.insert(PbKey::Space, Action::Next);
        core.hold_start = Some(core.now);
        core.initial_delay = Duration::from_secs(3600);
        core
    }

    /// The ADR-024 watchdog (level-triggered safety net): a lost key-up leaves `held_nav` stuck
    /// `Some`, which suppresses the sharpen — the stuck-preview race. Once the displayed preview
    /// has lingered past `PREVIEW_WATCHDOG_AFTER`, the sharpen is forced regardless of
    /// `held_nav`, so the display converges to its full without waiting for a focus change.
    #[test]
    fn a_lingering_preview_sharpens_despite_a_stuck_held_nav() {
        let mut core = stuck_preview_core();
        assert!(core.held_nav().is_some(), "the stuck key reads as blazing");
        assert_eq!(
            core.sharpen_now(),
            None,
            "blazing suppresses the sharpen (the normal gate)"
        );

        let t0 = core.now;
        core.tick(); // arms the watchdog (stamps the lingering preview)
        assert_eq!(core.sharpen_now(), None, "not yet past the deadline");

        core.now = t0 + PREVIEW_WATCHDOG_AFTER + Duration::from_millis(100);
        core.tick(); // fires the watchdog
        assert_eq!(
            core.sharpen_now(),
            Some(0),
            "the lingering preview sharpens even though held_nav is stuck Some"
        );
        // The firing edge must also force the prefetch re-issue (the request path stamps
        // `full_requested_at`), because 3b's change-detection alone can't reopen the gate.
        assert!(
            core.full_requested_at.contains_key(&0),
            "the full was actually requested, not merely flagged wanted"
        );
    }

    /// A real blaze must never trip the watchdog: every advance re-arms the stamp for the new
    /// displayed item, so cumulative hold time is irrelevant — only *lingering on one photo*
    /// counts. The hot path stays preview-only.
    #[test]
    fn a_real_blaze_resets_the_watchdog_every_advance() {
        let mut core = stuck_preview_core();
        core.source = photos_named(&["a.jpg", "b.jpg"]);
        core.playlist = Playlist::new(2, 0).with_cursor(0);
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 1, fit_rep, &[0, 1]);
        core.preview_resident.insert(1);
        core.targets = vec![0, 1];

        let t0 = core.now;
        core.tick(); // arms for item 0

        // Most of a deadline later the blaze advances: a new photo is displayed (still a preview).
        core.now = t0 + PREVIEW_WATCHDOG_AFTER * 3 / 4;
        core.displayed_item = Some(1);
        core.target_item = Some(1);
        core.mark_resolved(1);
        core.tick(); // re-arms for item 1 — the clock restarts

        // Same again: total hold = 1.5× the deadline, but no single photo reached it.
        core.now = t0 + PREVIEW_WATCHDOG_AFTER * 3 / 2;
        core.tick();
        assert_eq!(
            core.sharpen_now(),
            None,
            "no single photo lingered past the deadline — a real blaze never trips the watchdog"
        );
    }

    /// Once the full lands (the preview upgrades), the watchdog disarms and the sharpen
    /// override ends — the level-trigger tracks the *current* display state, not history.
    #[test]
    fn the_watchdog_disarms_once_the_full_lands() {
        let mut core = stuck_preview_core();
        let t0 = core.now;
        core.tick();
        core.now = t0 + PREVIEW_WATCHDOG_AFTER + Duration::from_millis(100);
        core.tick();
        assert_eq!(core.sharpen_now(), Some(0), "fired");

        // The full lands: no longer a resident preview.
        core.preview_resident.remove(&0);
        core.tick();
        assert!(
            core.preview_watchdog.is_none(),
            "the watchdog clears the moment the display is no longer a resident preview"
        );
        assert_eq!(core.sharpen_now(), None, "nothing left to force");
    }

    /// The firing edge must force a prefetch re-issue even when the wanted-fulls set is
    /// byte-identical to `last_upgrade_set` — the change-detection can't see an *eligibility*
    /// change (blazing-suppressed → forced) that leaves the set equal. Without the
    /// `watchdog_fired_now` escape this scenario would flag the sharpen wanted but never
    /// actually request it.
    #[test]
    fn the_firing_edge_forces_a_reissue_even_with_an_unchanged_wanted_set() {
        let mut core = stuck_preview_core();
        // As if this exact set had already been issued before the key got stuck.
        core.last_upgrade_set = vec![0];

        let t0 = core.now;
        core.tick(); // arm
        assert!(
            !core.full_requested_at.contains_key(&0),
            "nothing requested while the gate is closed"
        );
        core.now = t0 + PREVIEW_WATCHDOG_AFTER;
        core.tick(); // fire — the set is unchanged, so only the firing edge can re-issue
        assert!(
            core.full_requested_at.contains_key(&0),
            "the firing edge forced request_prefetch despite an unchanged wanted-set"
        );
    }

    /// The override is a LEVEL, not a pulse: while the preview keeps lingering, later ticks
    /// keep `sharpen_now` forced (but the firing edge — the forced re-issue — is one-shot).
    #[test]
    fn the_fired_watchdog_holds_until_the_state_changes() {
        let mut core = stuck_preview_core();
        let t0 = core.now;
        core.tick();
        core.now = t0 + PREVIEW_WATCHDOG_AFTER;
        core.tick();
        assert_eq!(core.sharpen_now(), Some(0));

        core.now = t0 + PREVIEW_WATCHDOG_AFTER + Duration::from_secs(5);
        core.tick();
        assert_eq!(
            core.sharpen_now(),
            Some(0),
            "still lingering → still forced (level-triggered)"
        );
        assert!(
            core.preview_watchdog.is_some_and(|w| w.fired),
            "fired state persists while the preview lingers"
        );
    }

    /// A genuine fast blaze that has OUTRUN the ring never arms the watchdog: the old photo
    /// lingers only because the *next* target is still decoding (`target_caught_up` false), and
    /// forcing its full then would put a decode ahead of the previews the blaze is waiting on.
    #[test]
    fn an_outrun_blaze_waiting_on_its_target_never_arms_the_watchdog() {
        let mut core = stuck_preview_core();
        core.source = photos_named(&["a.jpg", "b.jpg"]);
        core.playlist = Playlist::new(2, 0).with_cursor(0);
        core.targets = vec![0, 1];
        core.target_item = Some(1); // the blaze wants item 1; it isn't decoded yet

        let t0 = core.now;
        core.tick();
        core.now = t0 + PREVIEW_WATCHDOG_AFTER + Duration::from_secs(1);
        core.tick();
        assert!(
            core.preview_watchdog.is_none(),
            "not caught up → the lingering old frame is the blaze's problem, not the watchdog's"
        );
        assert_eq!(core.sharpen_now(), None);
    }

    /// RAW is excluded from the watchdog: its forced "full" is a seconds-long uncancellable
    /// demosaic, and its embedded preview is near-full-res anyway. (When genuinely parked the
    /// normal `sharpen_now` path still upgrades RAW — only the held-nav override abstains.)
    #[test]
    fn raw_never_arms_the_watchdog() {
        let mut core = stuck_preview_core();
        core.source = photos_named(&["a.nef"]);

        let t0 = core.now;
        core.tick();
        core.now = t0 + PREVIEW_WATCHDOG_AFTER + Duration::from_secs(1);
        core.tick();
        assert!(core.preview_watchdog.is_none(), "RAW must never arm");
        assert_eq!(core.sharpen_now(), None);
    }

    /// Deck-index reassignment disarms the watchdog: `PreviewWatchdog.item` is deck-relative,
    /// so a fired entry for old-item-0 must not instantly force a sharpen on a NEW deck whose
    /// first photo also happens to sit at index 0 — the new photo gets a fresh arm.
    #[test]
    fn a_deck_rebuild_disarms_the_watchdog() {
        let mut core = stuck_preview_core();
        let t0 = core.now;
        core.tick();
        core.now = t0 + PREVIEW_WATCHDOG_AFTER;
        core.tick();
        assert!(core.preview_watchdog.is_some_and(|w| w.fired));

        let root = PathBuf::from("photos");
        let src: Arc<dyn ItemSource> =
            Arc::new(FsSource::new(vec![root.join("x.jpg"), root.join("y.jpg")]));
        core.rebuild_playlist(src, root, None, false, 0);
        assert!(
            core.preview_watchdog.is_none(),
            "indices were reassigned — the old fired state must not carry over"
        );
    }

    /// item-6 6a: a GEOMETRY change retains resident Originals — the whole point of
    /// retain-and-remap. The current photo's and an in-window neighbour's Originals must both
    /// survive `invalidate_geometry` (Fit slots drop; Originals compact into the new ring).
    #[test]
    fn a_geometry_change_retains_resident_originals() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg", "b.jpg"]);
        core.playlist = Playlist::new(2, 0).with_cursor(0);
        core.ring = ResidentRing::new(6);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0, 1];
        core.displayed_item = Some(0);
        core.target_item = Some(0);
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0, 1]);
        make_resident(&mut core, 0, pb_core::Representation::Original, &[0, 1]);
        make_resident(&mut core, 1, pb_core::Representation::Original, &[0, 1]);

        core.invalidate_geometry();

        assert!(
            core.ring.original_slot(0).is_some(),
            "the current photo's Original survives a geometry change"
        );
        assert!(
            core.ring.original_slot(1).is_some(),
            "an in-window neighbour's Original survives too (the advance-after-toggle fix)"
        );
        assert!(
            core.ring.slot_for_rep(0, pb_core::RepKind::Fit).is_none(),
            "Fit slots are geometry-stale and must drop"
        );
    }

    /// item-6 6a invariant (spec §4.1): a CONTENT change purges everything — a retained
    /// Original crossing a content change would show another deck's pixels at index N.
    #[test]
    fn a_content_change_purges_retained_originals() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.targets = vec![0];
        core.displayed_item = Some(0);
        make_resident(&mut core, 0, pb_core::Representation::Original, &[0]);

        core.invalidate_content();

        assert!(
            core.ring.original_slot(0).is_none(),
            "content changes must never retain — geometry-only retention"
        );
    }

    /// item-6 Part D: after the settle's geometry invalidation, a retained current Original is
    /// re-presented at the NEW epoch — `target_caught_up` holds with zero decodes landed, so
    /// there is no pie and no held-frame limbo. (Headless: `present_item` counts as presented.)
    #[test]
    fn the_settle_re_presents_a_retained_original() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.displayed_item = Some(0);
        core.target_item = Some(0);
        core.mark_resolved(0);
        make_resident(&mut core, 0, pb_core::Representation::Original, &[0]);

        // What the settle runs (tick 4d):
        core.invalidate_geometry();
        core.refresh_after_geometry_change();

        assert!(
            core.target_caught_up(),
            "the retained Original re-presented at the new epoch — no decode needed"
        );
        assert_eq!(core.presented_epoch, Some(core.epoch));
        assert!(
            core.ring.original_slot(0).is_some(),
            "and it is still resident in the compacted ring"
        );
    }

    /// #110 110b: when the renderer can't derive (headless here; no held Original / clamped /
    /// mode-1 in production), the reserved destination Fit slot must be RELEASED — a stale
    /// `Pending` would block the CPU fallback's own reservation of that (item, Fit) key and
    /// strand the photo soft forever.
    #[test]
    fn a_failed_derive_releases_its_reservation() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.displayed_item = Some(0);
        core.target_item = Some(0);

        assert!(
            !core.try_gpu_derive_fit(),
            "a headless renderer has nothing to derive from"
        );
        assert!(
            !core.ring.is_tracked_rep(0, pb_core::RepKind::Fit),
            "the reservation must be rolled back so the CPU Fit can take the slot"
        );
    }

    /// #110 110b dispatch gates: never while blazing (a derive competes with the shared GPU
    /// queue, §4), and only for a Fit display (Original/Fill decode at native res). Neither
    /// refusal may leave ring state behind.
    #[test]
    fn the_derive_is_parked_only_and_fit_only() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.displayed_item = Some(0);
        core.target_item = Some(0);

        core.held.insert(PbKey::Space, Action::Next);
        assert!(!core.try_gpu_derive_fit(), "blazing → no derive");
        core.held.clear();

        core.view.mode = ScaleMode::Original; // decode_fit() = None
        assert!(!core.try_gpu_derive_fit(), "native-res display → no derive");
        assert!(
            !core.ring.is_tracked_rep(0, pb_core::RepKind::Fit),
            "refused dispatches must not touch the ring"
        );
    }

    /// item-6 6b: on a NAV-shaped dispatch (target ≠ displayed) the held fallback frame is the
    /// PREVIOUS photo's texture and must never be used as a derive source — deriving item 1's
    /// "Fit" from item 0's pixels would present the wrong photo. Without a retained ring
    /// Original for the target, the derive refuses before touching the ring.
    #[test]
    fn a_nav_derive_never_sources_the_previous_photos_held_frame() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg", "b.jpg"]);
        core.playlist = Playlist::new(2, 0).with_cursor(1);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![1, 0];
        core.displayed_item = Some(0); // still showing the old photo
        core.target_item = Some(1); // navigating to its neighbour

        assert!(
            !core.try_gpu_derive_fit(),
            "no ring Original for the target and held is the WRONG photo → refuse"
        );
        assert!(
            !core.ring.is_tracked_rep(1, pb_core::RepKind::Fit),
            "the refusal happens before any reservation"
        );

        // With the target's own Original retained, the source resolves to Ring — headless the
        // derive still fails, but now via the reserve→release path (nothing left tracked).
        make_resident(&mut core, 1, pb_core::Representation::Original, &[1, 0]);
        assert!(!core.try_gpu_derive_fit(), "headless renderer can't derive");
        assert!(
            !core.ring.is_tracked_rep(1, pb_core::RepKind::Fit),
            "the failed Ring-sourced derive rolled its reservation back"
        );
        assert!(
            core.ring.original_slot(1).is_some(),
            "the source Original is untouched"
        );
    }

    /// The 2026-07-19 owner-hit stuck-blurry repro: during a blaze the pool untracks a finished
    /// preview job before its outcome is drained, so a re-issued preview decodes TWICE — and the
    /// second preview outcome, landing after the first made the item resident, was misread as
    /// "the full came back a preview" → `upgrade_done` → the sharpen loop AND the watchdog both
    /// permanently gated off. A duplicate preview outcome must be dropped without a verdict.
    #[test]
    fn a_duplicate_preview_outcome_never_poisons_upgrade_done() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 1440,
            max_height: 960,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.displayed_item = Some(0);
        core.target_item = Some(0);
        core.mark_resolved(0);
        // The FIRST preview outcome made the photo resident-as-preview.
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        core.preview_resident.insert(0);

        // The SECOND (duplicate) preview outcome arrives from a preview-request job.
        let dup = pb_decode::DecodedImage {
            width: 256,
            height: 171,
            orig_width: 6000,
            orig_height: 4000,
            codec: "JPEG",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 256 * 171 * 4],
            is_preview: true,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        };
        core.pending_uploads.push(
            Outcome::synthetic(
                0,
                core.epoch,
                core.content_gen,
                pb_core::RepKind::Fit,
                Ok(dup),
            )
            .from_preview_request(),
        );
        core.drain_results();

        assert!(
            !core.upgrade_done.contains(&0),
            "a duplicate preview outcome must never be read as an upgrade verdict"
        );
        assert!(core.preview_resident.contains(&0), "still sharpen-eligible");
        assert_eq!(
            core.sharpen_now(),
            Some(0),
            "the real full still gets requested — no stuck-blurry"
        );
    }

    /// The legit case the poisoned branch existed for: a genuine FULL request (job preview =
    /// false) whose best result is still a preview (a RAW whose only embedded image IS its
    /// preview) must still end the sharpen loop — no infinite re-decode.
    #[test]
    fn a_full_request_that_returns_a_preview_still_ends_the_loop() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 1440,
            max_height: 960,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.displayed_item = Some(0);
        core.target_item = Some(0);
        core.mark_resolved(0);
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit_rep, &[0]);
        core.preview_resident.insert(0);

        let best_is_preview = pb_decode::DecodedImage {
            width: 256,
            height: 171,
            orig_width: 6000,
            orig_height: 4000,
            codec: "JPEG",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 256 * 171 * 4],
            is_preview: true,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        };
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(best_is_preview),
        ));
        core.drain_results();

        assert!(
            core.upgrade_done.contains(&0),
            "a full request that can only produce a preview ends the loop (RAW semantics)"
        );
        assert_eq!(core.sharpen_now(), None, "no infinite re-decode");
    }

    /// The watchdog is the second chance for a POISONED `upgrade_done` (a decode error, or any
    /// future mis-flagging): it arms despite the flag, and its fire clears it exactly once per
    /// arming cycle so the sharpen reopens — parked photos converge to sharp no matter how the
    /// bookkeeping got lied to (ADR-024 "converge or self-correct").
    #[test]
    fn the_watchdog_gives_a_poisoned_upgrade_done_a_second_chance() {
        let mut core = stuck_preview_core();
        core.held.clear(); // genuinely parked — the poison case needs no stuck key
        core.upgrade_done.insert(0); // poisoned: sharpen_now is gated off
        assert_eq!(
            core.sharpen_now(),
            None,
            "poisoned — the sharpen is blocked"
        );

        let t0 = core.now;
        core.tick(); // arms despite upgrade_done
        core.now = t0 + PREVIEW_WATCHDOG_AFTER;
        core.tick(); // fires → clears the poison → forces the re-issue
        assert!(
            !core.upgrade_done.contains(&0),
            "the fire edge clears the poisoned flag"
        );
        assert_eq!(
            core.sharpen_now(),
            Some(0),
            "the sharpen path reopened — the photo converges to sharp"
        );
        assert!(
            core.full_requested_at.contains_key(&0),
            "and the full was actually re-requested"
        );
    }

    /// Codex P1: parked on a poisoned photo NOTHING else keeps the loop ticking (the wanted-set
    /// is empty), so the watchdog must schedule its own deadline or a sleeping event loop never
    /// evaluates it. The tick's SetWake must carry (at latest) the watchdog deadline.
    #[test]
    fn an_armed_watchdog_schedules_its_own_wake() {
        let mut core = stuck_preview_core();
        core.held.clear(); // genuinely parked
        core.upgrade_done.insert(0); // poisoned: no sharpen work keeps the loop awake
        let t0 = core.now;
        core.effects.clear();
        core.tick(); // arms the watchdog
        let wake = core
            .effects
            .iter()
            .rev()
            .find_map(|e| match e {
                contract::CoreEffect::SetWake(w) => Some(*w),
                _ => None,
            })
            .expect("tick emits SetWake");
        let deadline = wake.expect("armed watchdog must not let the loop go idle");
        assert!(
            deadline <= t0 + PREVIEW_WATCHDOG_AFTER + core.frame_interval,
            "the wake must land at (or before) the watchdog deadline"
        );
    }

    /// Codex P1: the Err path must not recreate the poison from a PREVIEW-request job — a
    /// failed preview (duplicate or otherwise) proves nothing about the full.
    #[test]
    fn an_error_from_a_preview_request_never_poisons_upgrade_done() {
        let mut core = stuck_preview_core();
        core.held.clear();
        core.pending_uploads.push(
            Outcome::synthetic(
                0,
                core.epoch,
                core.content_gen,
                pb_core::RepKind::Fit,
                Err(pb_decode::DecodeError::Corrupt("smb hiccup".into())),
            )
            .from_preview_request(),
        );
        core.drain_results();
        assert!(
            !core.upgrade_done.contains(&0),
            "a preview-request error is not an upgrade verdict"
        );
        assert_eq!(core.sharpen_now(), Some(0), "the sharpen stays open");
    }

    /// Codex P1: `upgrade_done` is DISPLAY-tier bookkeeping — a duplicate parked-Original
    /// decode erroring (its first copy resident) must not gate the Fit preview's sharpen.
    #[test]
    fn an_original_error_never_poisons_the_fit_tier() {
        let mut core = stuck_preview_core();
        core.held.clear();
        make_resident(&mut core, 0, pb_core::Representation::Original, &[0]);
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Original,
            Err(pb_decode::DecodeError::Corrupt("dup original".into())),
        ));
        core.drain_results();
        assert!(
            !core.upgrade_done.contains(&0),
            "an Original-rep error must not poison the Fit sharpen"
        );
        assert_eq!(core.sharpen_now(), Some(0));
    }

    /// Codex P1: a full-decode ERROR after the fire latched used to kill the second chance
    /// (fire → re-issue dedupes into the in-flight job → that job errors → poisoned with no
    /// edges left). The error now re-arms the fired watchdog for another 2 s cycle, bounded by
    /// `MAX_WATCHDOG_RETRIES` — transient errors converge, permanent ones stop retrying.
    #[test]
    fn a_full_error_rearms_a_fired_watchdog_boundedly() {
        let mut core = stuck_preview_core();
        core.held.clear();
        core.targets = Vec::new(); // keep the real pool inert — errors are driven manually

        let mut t = core.now;
        for attempt in 1..=MAX_WATCHDOG_RETRIES {
            core.tick(); // arm (or re-armed by the previous error)
            t += PREVIEW_WATCHDOG_AFTER;
            core.now = t;
            core.tick(); // fire
            assert_eq!(
                core.sharpen_now(),
                Some(0),
                "attempt {attempt}: the fire reopened the sharpen"
            );
            // The re-issued full ERRORS.
            core.pending_uploads.push(Outcome::synthetic(
                0,
                core.epoch,
                core.content_gen,
                pb_core::RepKind::Fit,
                Err(pb_decode::DecodeError::Corrupt("persistent".into())),
            ));
            core.drain_results();
            assert!(core.upgrade_done.contains(&0), "the error re-poisons");
            let w = core.preview_watchdog.expect("still tracking the photo");
            if attempt < MAX_WATCHDOG_RETRIES {
                assert!(
                    !w.fired,
                    "attempt {attempt}: the error re-armed the watchdog"
                );
            } else {
                assert!(
                    w.fired,
                    "the retry budget is spent — no endless every-2s retry loop"
                );
            }
        }
        // Well past any deadline, nothing changes: the photo keeps its (blurry) preview
        // honestly rather than churning decodes forever.
        core.now = t + PREVIEW_WATCHDOG_AFTER * 4;
        core.tick();
        assert_eq!(core.sharpen_now(), None, "no fourth attempt");
    }

    /// "+1 never waits" (owner, 2026-07-19): after a FORWARD blaze the window order ranks the
    /// item just behind the cursor ~10th, so its full decoded seconds after parking — backing
    /// up one found a preview. Parked fulls must decode nearest-the-cursor-first (stable:
    /// ahead beats behind at equal distance), same membership.
    #[test]
    fn parked_fulls_decode_nearest_first_after_a_forward_blaze() {
        let mut core = test_core();
        core.source = photos_named(&[
            "a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg", "f.jpg", "g.jpg", "h.jpg", "i.jpg",
            "j.jpg", "k.jpg", "l.jpg",
        ]);
        core.playlist = Playlist::new(12, 0).with_cursor(5);
        core.ring = ResidentRing::new(8);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        core.displayed_item = Some(5);
        core.target_item = Some(5);
        // The post-forward-blaze window shape: ahead-biased, behind items last.
        core.targets = vec![5, 6, 7, 8, 9, 4, 3];
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        for &i in &[5usize, 6, 7, 8, 9, 4, 3] {
            make_resident(&mut core, i, fit_rep, &[5, 6, 7, 8, 9, 4, 3]);
            core.preview_resident.insert(i);
        }

        assert_eq!(
            core.prefetch_fulls(),
            vec![6, 4, 7, 3, 8, 9],
            "fulls decode nearest-first (current excluded — it is the top-priority sharpen)"
        );

        // While BLAZING the window order stands — mid-blaze the fulls should land AHEAD,
        // where the user is heading (and the current photo's full rides the ring since the
        // sharpen tier is empty while a key is held).
        core.held.insert(PbKey::Space, Action::Next);
        assert_eq!(
            core.prefetch_fulls(),
            vec![5, 6, 7, 8, 9, 4, 3],
            "blazing keeps the ahead-biased window order"
        );
        core.held.clear();
    }

    /// With wrap on (the default), the last item is one Backspace from item 0 — the parked
    /// nearest-first distance must wrap too, or backing up across the deck seam waits.
    #[test]
    fn parked_fulls_distance_wraps_at_the_deck_seam() {
        let mut core = test_core();
        core.source = photos_named(&[
            "a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg", "f.jpg", "g.jpg", "h.jpg", "i.jpg",
            "j.jpg", "k.jpg", "l.jpg",
        ]);
        core.playlist = Playlist::new(12, 0).with_cursor(0);
        core.ring = ResidentRing::new(8);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        core.displayed_item = Some(0);
        core.target_item = Some(0);
        core.targets = vec![0, 1, 2, 11];
        let fit_rep = core.rep_of(pb_core::RepKind::Fit);
        for &i in &[0usize, 1, 2, 11] {
            make_resident(&mut core, i, fit_rep, &[0, 1, 2, 11]);
            core.preview_resident.insert(i);
        }
        assert_eq!(
            core.prefetch_fulls(),
            vec![1, 11, 2],
            "item 11 is wrap-distance 1 from cursor 0 — it beats the distance-2 item"
        );
    }

    /// Sharpen-via-derive gating: a failed/ineligible GPU sharpen (headless here) must leave
    /// every piece of preview bookkeeping intact so the CPU sharpen path proceeds unharmed.
    #[test]
    fn a_gpu_sharpen_failure_leaves_the_preview_bookkeeping_intact() {
        let mut core = stuck_preview_core();
        core.held.clear(); // parked
        make_resident(&mut core, 0, pb_core::Representation::Original, &[0]);
        assert_eq!(core.sharpen_now(), Some(0), "eligible to sharpen");

        assert!(
            !core.try_gpu_sharpen(),
            "headless renderer can't derive — falls back"
        );
        assert!(
            core.preview_resident.contains(&0),
            "the preview tracking is untouched"
        );
        assert!(
            core.ring.original_slot(0).is_some(),
            "the source Original is untouched"
        );
        assert_eq!(
            core.sharpen_now(),
            Some(0),
            "the CPU sharpen path is still open"
        );
    }

    /// The deadline boundary: strictly-before stays armed-only; at the deadline it fires.
    #[test]
    fn the_watchdog_fires_at_the_deadline_not_before() {
        let mut core = stuck_preview_core();
        let t0 = core.now;
        core.tick(); // arm at t0

        core.now = t0 + PREVIEW_WATCHDOG_AFTER - Duration::from_millis(1);
        core.tick();
        assert_eq!(core.sharpen_now(), None, "1 ms early — not fired");

        core.now = t0 + PREVIEW_WATCHDOG_AFTER;
        core.tick();
        assert_eq!(core.sharpen_now(), Some(0), "at the deadline — fired");
    }

    #[test]
    fn rebuild_playlist_clears_metadata_and_marks_nothing_presented() {
        use crate::meta::PhotoMeta;
        let mut core = test_core();
        core.current = Some(PhotoMeta {
            rel: "old.jpg".into(),
            w: 100,
            h: 80,
            size: None,
            codec: "PNG",
            animated: None,
        });
        core.target_item = Some(0);
        core.mark_resolved(0);
        assert!(core.target_caught_up());

        let root = PathBuf::from("photos");
        let src: Arc<dyn ItemSource> =
            Arc::new(FsSource::new(vec![root.join("a.jpg"), root.join("b.jpg")]));
        core.rebuild_playlist(src, root, None, false, 0);

        assert!(core.current.is_none(), "a new deck drops the old metadata");
        // `displayed_item` names the logical current, but nothing is presented at this epoch
        // (presented_epoch = None), so it reads as pending — the held old frame holds (with
        // the pie) until the async decode lands. No synchronous decode ran on the loop.
        assert_eq!(core.displayed_item, Some(0));
        assert_eq!(core.presented_epoch, None);
        assert_eq!(core.target_item, Some(0));
        assert!(core.target_pending());
        assert!(!core.target_caught_up());
    }

    /// Diagnostic (initial-video-poster bug): a video as the initial item, whose
    /// The stuck-preview bug (#111): a fullscreen toggle's transient tiny viewport yields a ~256px
    /// "full" (`is_preview=false`) decoded at a fit far smaller than the current viewport. Treating
    /// it as the final sharp full strands the photo low-res forever, because the job loop then reads
    /// the resident-but-untracked slot as "already full" (`resident && !preview_resident`) and never
    /// re-decodes. It must stay sharpen-eligible so the real full re-decodes at the current fit.
    #[test]
    fn an_undersized_full_from_a_stale_fit_stays_sharpen_eligible() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 1440,
            max_height: 2036,
        }); // the CURRENT (large) viewport
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.displayed_item = Some(0);
        core.target_item = Some(0);

        // A "full" (is_preview=false) but decoded at a stale ~256px fit: 256x171 of a 6000x4000 source.
        let undersized = pb_decode::DecodedImage {
            width: 256,
            height: 171,
            orig_width: 6000,
            orig_height: 4000,
            codec: "JPEG",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 256 * 171 * 4],
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        };
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(undersized),
        ));
        core.drain_results();

        assert!(
            core.display_slot(0).is_some(),
            "the (undersized) frame became resident"
        );
        assert!(
            core.preview_resident.contains(&0),
            "an undersized full must stay sharpen-eligible, not be treated as the final sharp full"
        );
        assert_eq!(
            core.sharpen_now(),
            Some(0),
            "so the real full is re-requested at the current fit"
        );
    }

    /// Regression guard: a full that actually fills the current fit is FINAL — cleared from
    /// `preview_resident`, nothing left to sharpen (no re-decode loop).
    #[test]
    fn a_full_that_fills_the_current_fit_is_final() {
        let mut core = test_core();
        core.source = photos_named(&["a.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 1440,
            max_height: 2036,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.displayed_item = Some(0);
        core.target_item = Some(0);

        // A real full: fills the fit width (1440), a 6000x4000 source downscaled.
        let full = pb_decode::DecodedImage {
            width: 1440,
            height: 960,
            orig_width: 6000,
            orig_height: 4000,
            codec: "JPEG",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 1440 * 960 * 4],
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        };
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(full),
        ));
        core.drain_results();

        assert!(
            !core.preview_resident.contains(&0),
            "a full that fills the fit is final, not sharpen-eligible"
        );
        assert_eq!(core.sharpen_now(), None, "nothing left to sharpen");
    }

    /// Regression guard: a genuinely small photo (native size — source no larger than the output,
    /// which decode-to-fit never upscales) is FINAL, so it can't spin an endless re-decode loop.
    #[test]
    fn a_native_small_photo_is_final_not_a_sharpen_loop() {
        let mut core = test_core();
        core.source = photos_named(&["tiny.jpg"]);
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 1440,
            max_height: 2036,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0];
        core.displayed_item = Some(0);
        core.target_item = Some(0);

        // A genuinely 256px source (orig == decoded): decode-to-fit never upscales it.
        let native = pb_decode::DecodedImage {
            width: 256,
            height: 171,
            orig_width: 256,
            orig_height: 171,
            codec: "JPEG",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 256 * 171 * 4],
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        };
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(native),
        ));
        core.drain_results();

        assert!(
            !core.preview_resident.contains(&0),
            "a native-size photo is final (no infinite re-decode)"
        );
        assert_eq!(
            core.sharpen_now(),
            None,
            "a small source has no larger full to fetch"
        );
    }

    /// pool-decoded poster lands at the launch epoch, MUST become resident AND be
    /// presented — exactly as a photo does. Reproduces the launch state
    /// `rebuild_playlist` leaves (displayed==target, presented_epoch=None).
    #[test]
    fn initial_video_poster_presents_when_it_lands() {
        let mut core = test_core();
        let root = PathBuf::from("videos");
        core.source = Arc::new(FsSource::new(vec![root.join("clip.mkv")]));
        core.playlist = Playlist::new(1, 0);
        core.ring = ResidentRing::new(4);
        core.displayed_item = Some(0);
        core.target_item = Some(0);
        core.presented_epoch = None;
        core.targets = vec![0];
        assert!(core.item_is_video(0), "clip.mkv is a video item");
        let poster = pb_decode::DecodedImage {
            width: 64,
            height: 64,
            orig_width: 64,
            orig_height: 64,
            codec: "HEVC",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![9; 64 * 64 * 4],
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        };
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Original,
            Ok(poster),
        ));
        core.drain_results();
        assert!(core.display_slot(0).is_some(), "the poster became resident");
        assert_eq!(
            core.presented_epoch,
            Some(core.epoch),
            "the poster was PRESENTED at launch (not left resident-but-unpresented)"
        );
    }

    #[test]
    fn present_failed_is_terminal_at_the_current_epoch() {
        // A corrupt target counts as "resolved" so readiness doesn't leave the loading pie
        // spinning on it forever — even right after a geometry change bumped the epoch.
        let mut core = test_core();
        core.target_item = Some(0);
        core.invalidate_geometry();
        assert!(core.target_pending());
        core.present_failed(0);
        assert!(
            core.target_caught_up(),
            "a failed target is terminal, not perpetually pending"
        );
        assert!(core.current.is_none());
    }

    #[test]
    fn scale_mode_change_does_not_decode_on_the_event_loop() {
        // The synchronous decode is gone (finding #5): switching scale mode must not attempt
        // a decode on the loop. With a non-existent file a sync decode would fail and mark the
        // item failed; instead the async prefetch owns the (re)decode, and the epoch bump
        // leaves the current item pending a re-present.
        let mut core = test_core();
        let root = PathBuf::from("photos");
        core.source = Arc::new(FsSource::new(vec![root.join("a.jpg")]));
        core.playlist = Playlist::new(1, 0);
        core.target_item = Some(0);
        core.mark_resolved(0);
        assert!(core.target_caught_up());

        core.set_scale_mode(ScaleMode::Fill);

        assert!(
            !core.failed.contains(&0),
            "a scale-mode change must not synchronously decode (and fail) the item"
        );
        assert_eq!(core.view.mode, ScaleMode::Fill);
        assert!(
            core.target_pending(),
            "the item is pending an async re-present at the new fit"
        );
    }

    /// Owner-reported (79.10 smoke): toggling fullscreen while a video played went
    /// jerky — the resize-settle re-decode ran a synchronous poster decode over the
    /// live frame and refilled the whole ring (neighbor poster storms) mid-playback.
    /// A live video must defer the refresh; stopping the video re-issues it.
    #[test]
    fn geometry_change_during_video_defers_the_ring_refill() {
        use crate::video::{VideoProducerEvent, VideoSessionId};
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.playlist = Playlist::new(1, 0);
        core.displayed_item = Some(0);
        let (session, io) = VideoSession::new(VideoSessionId(1), 1024);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(1),
                duration: Some(Duration::from_secs(10)),
                width: 64,
                height: 64,
                has_audio: false,
                frame_bytes: 64 * 64 * 4,
            })
            .unwrap();
        core.poll_video();

        // The settled geometry change defers instead of re-decoding.
        core.refresh_after_geometry_change();
        assert!(core.video_geometry_stale, "refresh deferred while playing");

        // Stopping the video re-issues the prefetch (targets recomputed).
        core.targets.clear();
        core.stop_video();
        assert!(!core.video_geometry_stale, "flag consumed");
        assert!(
            !core.targets.is_empty(),
            "ring refill re-issued once playback ended"
        );
    }

    #[test]
    fn stream_installs_playback_on_header_plus_first_frame_and_starts_audio_once() {
        // A real still + companion .mov pair on disk, so `live_motion_path` resolves and
        // the audio effect fires (the pairing checks the .mov exists).
        let dir = std::env::temp_dir().join("pb_stream_install_test");
        let _ = std::fs::create_dir_all(&dir);
        let still = dir.join("IMG_0001.jpg");
        std::fs::write(&still, b"not a real jpeg").unwrap();
        std::fs::write(dir.join("IMG_0001.mov"), b"not a real mov").unwrap();

        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![still]));
        let (tx, _cancel) = inject_stream(&mut core, 0, AnimWant::Play);
        // Nothing installs before the first frame exists.
        tx.send(stream_header()).unwrap();
        core.poll_anim_stream();
        assert!(core.playback.is_none(), "header alone must not install");
        // Header + first frame → a playing, incomplete streaming Playback + audio from 0.0.
        tx.send(stream_frame()).unwrap();
        core.poll_anim_stream();
        let pb = core.playback.as_ref().expect("playback installed");
        assert!(pb.is_playing() && !pb.is_complete());
        assert_eq!(pb.frame_count(), 1);
        let audio_starts = core
            .effects
            .iter()
            .filter(|e| matches!(e, contract::CoreEffect::StartLiveAudio { .. }))
            .count();
        // Live Photo audio resolves via `live_motion_path`, which is wired on macOS and
        // Windows always, and on Linux only under `--features livephoto` (see its cfg). Where
        // it isn't wired the install still happens, just with no audio effect — assert the
        // count that matches this build so the test is correct on every platform/config.
        #[cfg(any(
            target_os = "macos",
            windows,
            all(unix, not(target_os = "macos"), feature = "livephoto")
        ))]
        let expected_audio_starts = 1;
        #[cfg(not(any(
            target_os = "macos",
            windows,
            all(unix, not(target_os = "macos"), feature = "livephoto")
        )))]
        let expected_audio_starts = 0;
        assert_eq!(
            audio_starts, expected_audio_starts,
            "audio starts exactly once at install where Live audio is wired"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stream_frames_extend_installed_playback_and_done_completes_it() {
        let mut core = test_core();
        let (tx, _cancel) = inject_stream(&mut core, 0, AnimWant::Play);
        tx.send(stream_header()).unwrap();
        tx.send(stream_frame()).unwrap();
        core.poll_anim_stream();
        assert!(core.playback.is_some());
        // Later frames append to the live playback without reinstalling.
        tx.send(stream_frame()).unwrap();
        tx.send(stream_frame()).unwrap();
        core.poll_anim_stream();
        assert_eq!(core.playback.as_ref().unwrap().frame_count(), 3);
        // `Done` finalizes the loop count and clears the stream.
        tx.send(StreamMsg::Done {
            loop_count: 1,
            truncated: false,
        })
        .unwrap();
        core.poll_anim_stream();
        assert!(core.anim_stream.is_none());
        assert!(core.playback.as_ref().unwrap().is_complete());
    }

    #[test]
    fn stream_disconnect_after_install_completes_playback() {
        // The worker vanishing without a terminal chunk (panic / producer bug) must not
        // leave an installed playback incomplete — it would park on the decoded frontier
        // forever while the audio played on.
        let mut core = test_core();
        let (tx, _cancel) = inject_stream(&mut core, 0, AnimWant::Play);
        tx.send(stream_header()).unwrap();
        tx.send(stream_frame()).unwrap();
        core.poll_anim_stream();
        assert!(core.playback.is_some());
        drop(tx); // worker vanished
        core.poll_anim_stream();
        assert!(core.anim_stream.is_none());
        assert!(
            core.playback.as_ref().unwrap().is_complete(),
            "disconnect must complete the installed playback"
        );
    }

    #[test]
    fn stream_disconnect_before_install_leaves_no_playback() {
        let mut core = test_core();
        let (tx, _cancel) = inject_stream(&mut core, 0, AnimWant::Play);
        tx.send(stream_header()).unwrap();
        drop(tx); // worker vanished before any frame
        core.poll_anim_stream();
        assert!(core.anim_stream.is_none());
        assert!(core.playback.is_none());
    }

    #[test]
    fn stale_stream_is_cancelled_and_dropped() {
        // Epoch bump (viewport/geometry change) — the stream is stale: cancel + drop.
        let mut core = test_core();
        let (_tx, cancel) = inject_stream(&mut core, 0, AnimWant::Play);
        core.epoch += 1;
        core.poll_anim_stream();
        assert!(core.anim_stream.is_none());
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "the worker must be told to stop"
        );

        // Navigating away (displayed_item changed) — same deal.
        let (_tx, cancel) = inject_stream(&mut core, 0, AnimWant::Play);
        core.displayed_item = Some(1);
        core.poll_anim_stream();
        assert!(core.anim_stream.is_none());
        assert!(cancel.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn eager_stream_stashes_prepared_on_done() {
        let mut core = test_core();
        let (tx, _cancel) = inject_stream(&mut core, 0, AnimWant::Eager);
        tx.send(stream_header()).unwrap();
        tx.send(stream_frame()).unwrap();
        tx.send(stream_frame()).unwrap();
        core.poll_anim_stream();
        // Eager accumulates silently — no playback while the user hasn't pressed P.
        assert!(core.playback.is_none());
        tx.send(StreamMsg::Done {
            loop_count: 1,
            truncated: false,
        })
        .unwrap();
        core.poll_anim_stream();
        assert!(core.anim_stream.is_none());
        let prepared = core.prepared.as_ref().expect("eager stash filled");
        assert_eq!(prepared.item, 0);
        assert_eq!(prepared.anim.frame_count(), 2);
    }

    // ---- Thumbnails strip (task #83) ----

    fn thumb_test_core() -> AppCore {
        let mut core = test_core();
        core.native_thumbs = true; // headless default is false (no presenter)
        core.source = five_photos();
        core.playlist = Playlist::new(5, 0);
        core.ring = ResidentRing::new(4);
        core
    }

    fn tiny_thumb(w: u32, h: u32) -> crate::thumbs::ThumbPixels {
        crate::thumbs::ThumbPixels {
            rgba: vec![200; (w * h * 4) as usize],
            orig_w: 4000,
            orig_h: 3000,
            codec: "JPEG",
        }
    }

    #[test]
    fn shift_t_and_shift_f_share_the_left_pane_with_tab_semantics() {
        let mut core = thumb_test_core();
        assert!(!core.thumbs_visible());
        // Shift+T opens the pane on Thumbnails and enables capture.
        core.toggle_thumbnails();
        assert!(core.thumbs_visible());
        assert!(core.folder_tree_open);
        assert!(core.thumbs.enabled);
        assert_eq!(core.left_tab, crate::overlay::LeftTab::Thumbnails);
        // Shift+F switches tabs — the pane stays open.
        core.toggle_folder_tree();
        assert!(!core.thumbs_visible());
        assert!(core.folder_tree_open);
        assert_eq!(core.left_tab, crate::overlay::LeftTab::Folders);
        // Shift+T switches back.
        core.toggle_thumbnails();
        assert!(core.thumbs_visible());
        // Shift+T on the showing tab closes the pane.
        core.toggle_thumbnails();
        assert!(!core.folder_tree_open);
        assert!(!core.thumbs_visible());
        // Capture stays enabled after close (accumulates for the reopen).
        assert!(core.thumbs.enabled);
    }

    #[test]
    fn opening_thumbnails_lands_a_follow_scroll_on_current() {
        let mut core = thumb_test_core();
        core.playlist.jump_to(3);
        core.toggle_thumbnails();
        let cmd = core.thumbs.pending_scroll.expect("open scrolls to current");
        assert_eq!(cmd.item, 3);
    }

    #[test]
    fn thumb_jump_presents_the_cached_thumb_instantly_as_a_preview() {
        let mut core = thumb_test_core();
        core.toggle_thumbnails();
        // A cached thumb for a cold (non-resident) item…
        let demand = core.thumbs.demand(0);
        core.thumbs.cache.insert(
            3,
            pb_core::ThumbTier::Full,
            12,
            8,
            12 * 8 * 4,
            tiny_thumb(12, 8),
            &demand,
        );
        core.thumb_jump(3);
        // …presents NOW as a resident preview (the synthetic-outcome path).
        assert_eq!(core.displayed_item, Some(3), "no wait, no black flash");
        assert!(
            core.preview_resident.contains(&3),
            "lands as a preview so the real decode upgrades in place"
        );
        // The info panel sees the TRUE source facts, not the thumb's size.
        assert_eq!(
            core.meta_cache.get(&3).map(|m| (m.w, m.h)),
            Some((4000, 3000))
        );
        // Follow re-engaged onto the click target.
        assert_eq!(core.thumbs.pending_scroll.map(|c| c.item), Some(3));
    }

    #[test]
    fn thumb_jump_without_a_cached_thumb_still_jumps() {
        let mut core = thumb_test_core();
        core.toggle_thumbnails();
        core.thumb_jump(2);
        assert_eq!(core.playlist.current(), Some(2));
        assert_eq!(core.target_item, Some(2));
        assert_eq!(core.displayed_item, None, "cold: waits for the decode");
    }

    #[test]
    fn display_capture_lands_in_the_thumb_cache_via_the_derive_thread() {
        let mut core = thumb_test_core();
        core.toggle_thumbnails();
        let img = pb_decode::DecodedImage {
            width: 128,
            height: 64,
            orig_width: 128,
            orig_height: 64,
            codec: "PNG",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![7; 128 * 64 * 4],
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        };
        core.thumbs_capture(Outcome::synthetic(
            2,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(img),
        ));
        assert!(
            core.thumbs.working(),
            "derive in flight keeps the pump awake"
        );
        for _ in 0..200 {
            if core.thumbs.poll(0) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let e = core.thumbs.cache.get(2).expect("captured");
        assert_eq!((e.w, e.h), (128, 64));
        assert_eq!(e.tier, pb_core::ThumbTier::Full);
    }

    /// A displayed image, for the capture hook.
    fn captured_img(w: u32, h: u32) -> pb_decode::DecodedImage {
        pb_decode::DecodedImage {
            width: w,
            height: h,
            orig_width: w,
            orig_height: h,
            codec: "H.264",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![7; (w * h * 4) as usize],
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
        }
    }

    /// A video's poster is retained even though the strip was NEVER opened.
    ///
    /// On macOS/Linux there is no #114 selection pipeline, so this hook is the only thing
    /// that can keep a poster tile at all. Before this, browsing a movie folder with the
    /// strip closed threw away every poster it walked for, and opening the strip re-walked
    /// all of them from scratch at the bottom of the priority list.
    #[test]
    fn a_videos_poster_is_captured_even_with_the_strip_never_opened() {
        let mut core = thumb_test_core();
        core.source = photos_named(&["a.jpg", "film.mkv", "c.jpg"]);
        core.playlist = Playlist::new(3, 0);
        assert!(!core.thumbs.enabled, "the strip was never opened");
        assert!(!core.thumbs.capture);

        core.thumbs_capture(Outcome::synthetic(
            1, // the .mkv
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(captured_img(128, 64)),
        ));

        assert!(
            core.thumbs.capture,
            "a poster walk we already paid for must turn retention on"
        );
        assert!(
            !core.thumbs.enabled,
            "but the strip's own scheduled work stays gated on an actual panel open"
        );
        for _ in 0..200 {
            if core.thumbs.poll(0) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            core.thumbs.cache.get(1).is_some(),
            "the poster tile is kept"
        );
    }

    /// …and the asymmetry holds: a PHOTO is not captured while the strip is closed.
    ///
    /// This is the guard that makes the video case affordable. A photo thumb is a cheap
    /// local re-decode, so capturing every displayed photo would put a derive on every
    /// frame of a blaze — which is precisely what `thumbs.enabled` exists to prevent.
    /// Deleting this test's guarantee is how a blaze regression gets in.
    #[test]
    fn a_photo_is_not_captured_while_the_strip_is_closed() {
        let mut core = thumb_test_core();
        core.source = photos_named(&["a.jpg", "film.mkv", "c.jpg"]);
        core.playlist = Playlist::new(3, 0);

        core.thumbs_capture(Outcome::synthetic(
            0, // a photo
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(captured_img(128, 64)),
        ));

        assert!(
            !core.thumbs.capture,
            "a displayed photo must not switch retention on — that is a blaze cost"
        );
        assert!(core.thumbs.cache.get(0).is_none());
    }

    #[test]
    fn deck_rebuild_clears_thumbs_and_bumps_generation() {
        let mut core = thumb_test_core();
        core.toggle_thumbnails();
        let demand = core.thumbs.demand(0);
        core.thumbs.cache.insert(
            1,
            pb_core::ThumbTier::Full,
            4,
            4,
            64,
            tiny_thumb(4, 4),
            &demand,
        );
        let g0 = core.thumbs.cache.deck_gen();
        core.rebuild_playlist(five_photos(), PathBuf::from("photos"), None, false, 0);
        assert_eq!(core.thumbs.cache.len(), 0, "index-keyed thumbs dropped");
        assert!(core.thumbs.cache.deck_gen() > g0);
    }

    /// Privacy (task #83 / ADR-018): the whole thumbnail machinery — capture,
    /// derive, T1 fill decodes (incl. the EXIF-IFD1 fast path), cache churn —
    /// is RAM-only. A thumbs-enabled session over a real sandbox must create
    /// or modify nothing on disk. (The winit no-trace tests cover the broader
    /// view session; this one exercises exactly the strip's new code paths.)
    #[test]
    fn thumbnail_session_writes_nothing_to_disk() {
        use std::fs;

        fn snapshot(dir: &std::path::Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
            let mut out = Vec::new();
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                for e in fs::read_dir(&d).expect("read_dir") {
                    let e = e.expect("entry");
                    let m = e.metadata().expect("meta");
                    if m.is_dir() {
                        stack.push(e.path());
                    } else {
                        out.push((e.path(), m.len(), m.modified().expect("mtime")));
                    }
                }
            }
            out.sort();
            out
        }

        let dir = std::env::temp_dir().join(format!("pb_thumb_notrace_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir sandbox");
        const IMG: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pb-app/icons/blazeviewer.png"
        ));
        let mut paths = Vec::new();
        for name in ["a.png", "b.png", "c.png"] {
            let p = dir.join(name);
            fs::write(&p, IMG).expect("seed image");
            paths.push(p);
        }
        let before = snapshot(&dir);

        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(paths));
        // T1 fill path — the Thumb-purpose decode entry (EXIF-IFD1 probe, then
        // the fitted decode of the read bytes).
        for i in 0..source.len() {
            let img = crate::engine::decode_item_for(
                source.as_ref(),
                i,
                Some(crate::thumbs::thumb_fit()),
                true,
                crate::decode_pool::Purpose::Thumb,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("thumb fill decode");
            // T0 capture + derive + cache insert.
            let mut core = None;
            let core = core.get_or_insert_with(|| {
                let mut c = test_core();
                c.native_thumbs = true;
                c.source = source.clone();
                c.playlist = Playlist::new(source.len(), 0);
                c
            });
            core.toggle_thumbnails();
            core.thumbs_capture(Outcome::synthetic(
                i,
                core.epoch,
                core.content_gen,
                pb_core::RepKind::Fit,
                Ok(img),
            ));
            for _ in 0..200 {
                if core.thumbs.poll(0) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(core.thumbs.cache.get(i).is_some(), "thumb {i} cached (RAM)");
        }

        let after = snapshot(&dir);
        assert_eq!(before, after, "a thumbnail session must touch no files");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shift_f_from_thumbnails_switches_tab_and_ffi_state() {
        let mut core = thumb_test_core();
        core.native_tree = true; // the mac shell's flags
        core.toggle_thumbnails();
        assert!(core.thumbs_visible());
        core.effects.clear();
        // ⇧F while Thumbnails shows: switch, don't close.
        core.dispatch_action(Action::FolderTree);
        assert_eq!(
            core.left_tab,
            crate::overlay::LeftTab::Folders,
            "tab switched"
        );
        assert!(core.folder_tree_open, "pane stays open");
        assert!(!core.thumbs_visible());
        assert!(core.tree_panel_visible(), "native tree now visible");
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "the shell is re-signalled so both tab bars re-pull"
        );
        // And ⇧T switches back without closing.
        core.effects.clear();
        core.dispatch_action(Action::Thumbnails);
        assert_eq!(core.left_tab, crate::overlay::LeftTab::Thumbnails);
        assert!(core.thumbs_visible());
        assert!(!core.tree_panel_visible());
    }

    // --- archive doors: entering (task #104) ------------------------------

    /// A disk deck of `photo.jpg` + `album.zip` in `dir`, cursor on the door.
    fn core_on_a_door(dir: &Path) -> AppCore {
        let mut core = test_core();
        let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![
            dir.join("photo.jpg"),
            dir.join("album.zip"),
        ]));
        core.rebuild_playlist(src, dir.to_path_buf(), Some(dir.to_path_buf()), false, 0);
        core.displayed_item = Some(1); // the door
        core.presented_epoch = Some(core.epoch); // its frame is on screen (see `door_presented`)
        core
    }

    /// Regression: the door card must NOT flash over a still-held previous frame during a deck
    /// rebuild. `rebuild_playlist` names the current item (a door) but leaves `presented_epoch`
    /// None — the renderer still holds the old photo — so `door_presented`/`door_card` must be
    /// false/None until the door's own (transparent) frame is actually presented. The
    /// owner-reported "card on top of a photo" (and the archive-open card-with-no-image).
    #[test]
    fn door_card_waits_until_the_door_frame_is_presented() {
        let dir = std::env::temp_dir().join("pb_door_card_wait");
        let mut core = test_core();
        let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![
            dir.join("photo.jpg"),
            dir.join("album.zip"),
        ]));
        // Rebuild with the cursor on the door (start index 1): it is the current item, but nothing
        // is presented yet (the old frame is still held).
        core.rebuild_playlist(src, dir.to_path_buf(), Some(dir.to_path_buf()), false, 1);
        assert_eq!(core.displayed_item, Some(1), "the door is the current item");
        assert!(
            core.presented_epoch.is_none(),
            "a rebuild presents nothing yet"
        );
        assert!(
            !core.door_presented(),
            "no card while the previous frame is still held"
        );
        assert!(core.door_card().is_none());
        // Once the door's own frame is presented (its epoch resolves), the card appears.
        core.presented_epoch = Some(core.epoch);
        assert!(core.door_presented());
        assert!(core.door_card().is_some());
    }

    /// The session password cache (session-archive-password-cache): harvest/promote via
    /// `remember_archive_password` is MRU-ordered, deduped, empty-ignoring, capped, and
    /// wiped by `clear_archive_passwords`.
    #[test]
    fn archive_password_cache_is_mru_deduped_capped_and_clearable() {
        use crate::SecretString;
        let mut core = test_core();
        assert!(core.archive_passwords_snapshot().is_empty());

        // Empty passwords are never remembered.
        core.remember_archive_password(&SecretString::new(""));
        assert!(core.archive_passwords_snapshot().is_empty());

        // Newest-first (MRU).
        core.remember_archive_password(&SecretString::new("a"));
        core.remember_archive_password(&SecretString::new("b"));
        let snap = core.archive_passwords_snapshot();
        assert_eq!(snap.first().map(|s| s.expose()), Some("b"));

        // Re-using an existing password moves it to the front (no duplicate).
        core.remember_archive_password(&SecretString::new("a"));
        let snap = core.archive_passwords_snapshot();
        assert_eq!(
            snap.iter()
                .map(|s| s.expose().to_owned())
                .collect::<Vec<_>>(),
            vec!["a".to_owned(), "b".to_owned()],
            "MRU promotion, no dupes"
        );

        // Capped at MAX_ARCHIVE_PASSWORDS — the oldest fall off.
        for i in 0..AppCore::MAX_ARCHIVE_PASSWORDS + 5 {
            core.remember_archive_password(&SecretString::new(format!("p{i}")));
        }
        assert_eq!(
            core.archive_passwords_snapshot().len(),
            AppCore::MAX_ARCHIVE_PASSWORDS
        );

        // Teardown wipes it.
        core.clear_archive_passwords();
        assert!(core.archive_passwords_snapshot().is_empty());
    }

    /// Redaction is end-to-end: a password riding the `#[derive(Debug)]` contract types never
    /// prints in the clear (session-archive-password-cache — closes the pre-existing leak).
    #[test]
    fn contract_debug_redacts_the_password() {
        use crate::SecretString;
        let submitted =
            contract::DialogResult::PasswordSubmitted(Some(SecretString::new("hunter2")));
        let begin = contract::CoreEffect::BeginArchiveOpen {
            path: std::path::PathBuf::from("/a.zip"),
            password: Some(SecretString::new("hunter2")),
        };
        assert!(!format!("{submitted:?}").contains("hunter2"));
        assert!(!format!("{begin:?}").contains("hunter2"));
    }

    /// The door's only read. `P` on an archive emits exactly one archive open, with
    /// no password on the first attempt (the shell's failure path prompts and
    /// re-opens with `Some`, which is why guessing here would be wrong).
    #[test]
    fn p_on_a_door_opens_the_archive_and_nothing_else() {
        let dir = std::env::temp_dir().join("pb_door_enter");
        let mut core = core_on_a_door(&dir);
        core.effects.clear();

        core.toggle_play_pause();

        let opens: Vec<_> = core
            .effects
            .iter()
            .filter_map(|e| match e {
                contract::CoreEffect::BeginArchiveOpen { path, password } => {
                    Some((path.clone(), password.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            opens,
            vec![(dir.join("album.zip"), None)],
            "exactly one open, un-passworded"
        );
        assert!(
            core.playback.is_none(),
            "a door never starts the animation machinery"
        );
    }

    /// Entering is an open like any other, so it ends an Open-Parent climb — else
    /// the next `Alt+Up` would resume from a stale rung and jump somewhere absurd.
    #[test]
    fn entering_a_door_ends_a_climb() {
        let dir = std::env::temp_dir().join("pb_door_climb");
        let mut core = core_on_a_door(&dir);
        core.climb_anchor = Some(dir.join("somewhere/else"));

        core.toggle_play_pause();

        assert_eq!(core.climb_anchor, None);
    }

    /// `P` keeps its existing meanings — the door arm must not shadow a photo.
    #[test]
    fn p_on_a_photo_is_unaffected_by_the_door_arm() {
        let dir = std::env::temp_dir().join("pb_door_photo");
        let mut core = core_on_a_door(&dir);
        core.displayed_item = Some(0); // the .jpg
        core.effects.clear();

        core.toggle_play_pause();

        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::BeginArchiveOpen { .. })),
            "a photo must never open an archive"
        );
    }

    /// Inside an open archive, an entry named `inner.zip` is **not** a door — so `P`
    /// cannot enter it. Nesting stays unrepresentable rather than merely refused.
    #[test]
    fn p_inside_an_archive_cannot_enter_a_nested_zip() {
        let mut core = archive_core(&["a.jpg", "inner.zip"]);
        core.displayed_item = Some(1);
        core.effects.clear();

        assert_eq!(core.item_archive_kind(1), None, "an entry is never a door");
        core.toggle_play_pause();
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::BeginArchiveOpen { .. })),
            "a nested .zip entry must not open"
        );
    }

    /// The card is a door's entire on-screen presence (its frame draws nothing), so it
    /// must carry the name, the format, and the **live** shortcut — and appear only for
    /// a door.
    #[test]
    fn door_card_describes_the_presented_door_only() {
        let dir = std::env::temp_dir().join("pb_door_card");
        let mut core = core_on_a_door(&dir);

        let card = core.door_card().expect("a door presents a card");
        assert_eq!(card.name, "album.zip", "the file name, not the full path");
        assert_eq!(card.format, "ZIP Archive", "Title Case, like every heading");
        assert!(
            !card.shortcut.is_empty(),
            "from the live keymap, not hard-coded"
        );

        // A photo has no card — otherwise it would float over the picture.
        core.displayed_item = Some(0);
        assert!(core.door_card().is_none());
    }

    /// Keyed off the item **on screen**, never the playlist cursor: naming an archive the
    /// viewer is not looking at yet would be worse than naming none.
    #[test]
    fn door_card_follows_the_presented_item_not_the_cursor() {
        let dir = std::env::temp_dir().join("pb_door_card_cursor");
        let mut core = core_on_a_door(&dir);
        core.target_item = Some(0); // the cursor moved to the photo…
        assert!(
            core.door_card().is_some(),
            "…but the door is still on screen, so its card stays"
        );
        core.displayed_item = None;
        assert!(core.door_card().is_none(), "nothing presented, no card");
    }

    /// The perf timers (PB_PERF, task perf) are wired to the right choke points — not just
    /// the pure `Perf` logic (covered in `perf.rs`), but that the *hooks fire from the real
    /// methods*: `mark_resolved` (via `present_item`) closes the open→first-photo and
    /// resize→on-screen episodes, `rebuild_playlist` sets the all-cached target, and
    /// `perf_note_full` reports it. The GUI can't run headless, so this drives the core
    /// directly and reads the episodes back out of the `--metrics` recorder.
    #[test]
    fn perf_hooks_fire_from_the_real_present_and_resize_paths() {
        let dir = std::env::temp_dir().join("pb_perf_wiring");
        let mut core = test_core();
        core.perf = crate::perf::Perf::new(true);
        core.metrics = crate::metrics::StageTimes::enabled();

        // An open starts the clock (open_plan does this), then the deck installs (rebuild
        // calls deck_ready). Order matters: open_begin resets the all-cached target, so it
        // must precede the rebuild — exactly the real order (open_plan → … → rebuild).
        core.perf.open_begin(core.now);
        let src: Arc<dyn ItemSource> =
            Arc::new(FsSource::new(vec![dir.join("a.jpg"), dir.join("b.jpg")]));
        core.rebuild_playlist(src, dir.clone(), Some(dir), false, 0);

        let has = |c: &AppCore, stage: &str| c.metrics.summary().iter().any(|r| r.0 == stage);

        // First present of the current photo closes metric 1. `present_item` calls
        // `mark_resolved` even with no renderer, so the hook runs headless.
        core.present_item(0, 0);
        assert!(
            has(&core, "open->first-photo"),
            "presenting the first photo must record open->first-photo"
        );

        // A scale-mode switch begins a resize episode (refresh_after_geometry_change), and
        // the next present closes it — the exact Fit↔1:1 path the owner asked to measure.
        core.set_scale_mode(ScaleMode::Original);
        core.present_item(0, 0);
        assert!(
            has(&core, "resize->on-screen"),
            "a scale-mode switch then present must record resize->on-screen"
        );

        // The all-cached target is the deck size (2); reporting it once the last full lands.
        core.perf_note_full(0);
        assert!(!has(&core, "open->all-cached"), "one of two isn't all");
        core.perf_note_full(1);
        assert!(
            has(&core, "open->all-cached"),
            "the last full landing must record open->all-cached"
        );
    }

    /// A door reports its **size** where a photo reports dimensions — its frame is a 1×1
    /// sentinel, so `1 × 1` would be the alternative. The size rides `PhotoMeta` from the
    /// scan worker precisely because this runs on the frame path.
    #[test]
    fn the_info_line_reports_a_size_for_a_door_and_dimensions_for_a_photo() {
        let core = test_core();
        let door = crate::meta::PhotoMeta {
            rel: "album.zip".to_string(),
            w: 1,
            h: 1,
            size: Some(271_000_000),
            codec: "ZIP",
            animated: None,
        };
        let parts = core.info_line_parts(&door);
        assert!(parts.contains(&"271 MB".to_string()), "{parts:?}");
        assert!(
            !parts.iter().any(|p| p.contains('×')),
            "never print 1 × 1: {parts:?}"
        );

        let photo = crate::meta::PhotoMeta {
            rel: "a.jpg".to_string(),
            w: 4032,
            h: 3024,
            size: None,
            codec: "JPEG",
            animated: None,
        };
        let parts = core.info_line_parts(&photo);
        assert!(parts.contains(&"4032×3024".to_string()), "{parts:?}");
    }

    /// A door gets **no** play pill: its affordance is the door card (task #105), which is
    /// the only thing on screen for it. Regression bar for the kind-3 borrow that the card
    /// replaced — re-adding it would put a zip button under the card's own button.
    #[test]
    fn a_door_has_no_play_pill() {
        let dir = std::env::temp_dir().join("pb_door_hint");
        let mut core = core_on_a_door(&dir);
        assert_eq!(
            core.play_hint_kind(),
            0,
            "the card is the affordance, not a pill"
        );

        // …and settling on one arms nothing.
        let before = core.play_hint_seq;
        core.maybe_show_anim_hint(false);
        assert_eq!(core.play_hint_seq, before);
    }

    /// **The loop the feature promises**: enter a door, then climb back out to the
    /// folder of doors so the next one is a keypress away. The climb half already
    /// worked (`open_parent_cmd` anchors on the source's container); this pins the
    /// two halves together, which is what a viewer actually does.
    #[test]
    fn enter_a_door_then_climb_back_out_to_the_folder_of_doors() {
        let dir = std::env::temp_dir().join("pb_door_loop");
        let mut core = core_on_a_door(&dir);

        // 1. P enters album.zip.
        core.effects.clear();
        core.toggle_play_pause();
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::BeginArchiveOpen { path, .. } if *path == dir.join("album.zip"))));

        // 2. The archive deck lands (what the shell feeds back as ArchiveResolved).
        let entries: Arc<dyn ItemSource> = Arc::new(FakeArchive {
            names: vec!["1.jpg".to_string(), "2.jpg".to_string()],
            container: dir.join("album.zip"),
        });
        core.apply_archive(crate::scan::Resolved {
            root: dir.join("album.zip"),
            scan_root: None,
            recursive: false,
            source: entries,
            start: 0,
        });
        assert_eq!(core.source.len(), 2, "viewing inside the archive");

        // 3. Alt+Up climbs out to the folder that holds the archive — the folder of
        //    doors, which is where the next door is.
        core.effects.clear();
        core.open_parent_cmd();
        let scanned = core.effects.iter().rev().find_map(|e| match e {
            contract::CoreEffect::BeginDirScan {
                source: pb_core::open::Source::Scan { roots, .. },
                ..
            } => roots.first().cloned(),
            _ => None,
        });
        assert_eq!(
            scanned,
            Some(dir),
            "climbing out of an archive lands on its folder"
        );
    }
}
