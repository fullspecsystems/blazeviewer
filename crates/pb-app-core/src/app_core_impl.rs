//! `impl AppCore` — the orchestration methods (NS0 5.5 / Phase B).
//!
//! The ~62 pure-core methods that used to live on the winit `impl App` (nav/prefetch/
//! residency, view zoom/pan/rotate/fit, HUD build, animation playback, undo/misc). They
//! reference only core-owned state (`self.*`, formerly `self.*`) + the relocated
//! `engine` helpers + the engine crates — never winit/muda/egui/rfd. Moved verbatim
//! (behavior-preserving); the shell now calls them as `self.<method>()`.

#![allow(clippy::too_many_arguments)]

/// Concern-scoped `impl AppCore` blocks, split out of this file (task #125). Rust allows an
/// inherent impl to span multiple modules in one crate, so these are pure relocations: no
/// call site, type, signature or visibility changes. `app_core_impl.rs` keeps lifecycle,
/// dispatch and the residency engine.
mod animation;
mod archive_open;
mod audio_tracks;
mod background;
mod clipboard;
mod compare;
mod delete;
mod describe;
mod dir_scan;
mod image_text;
mod item_kind;
mod menu;
mod meta;
mod nav;
mod open;
mod panels;
mod prefs;
mod quick_sort;
mod save_rotation;
mod secret;
mod slideshow;
mod subtitles;
mod thumbs;
mod toast;
mod tree;
mod undo;
mod video;
mod view;

#[cfg(test)]
mod test_support;

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
    settings, timing, Action, AppCore, FitStash, InspectorTab, NativeToast, Nav, Panels,
    SlotContent, Toast, ToastIcon, UndoAction,
};

/// Is the eased scroll-zoom on? Default on; `PB_EASE_ZOOM=0` restores the instant
/// per-notch behavior (the revert lever + an A/B feel comparison). Read once and cached.
fn ease_scroll_zoom() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("PB_EASE_ZOOM").map_or(true, |v| v != "0"))
}

/// Interim adapter (task #54 Phase 0): a core [`DetailRow`] to the HUD table row it
/// projects onto. Retires with the HUD's Details tab.
fn hud_row(r: DetailRow) -> Row {
    match r {
        DetailRow::Span { text, bold } => Row::Span { text, bold },
        DetailRow::Pair { label, value } => Row::Pair { label, value },
        // The HUD is a static raster with no hit-testing, so a Section's buttons
        // cannot exist here — it degrades to the bold heading it also is. The
        // commands remain on the Edit menu for this path.
        DetailRow::Section { text, .. } => Row::Span { text, bold: true },
        // The HUD table has no paragraph row — it lays every row out on one
        // line, sized to the widest column. A prompt therefore projects onto a
        // plain full-width span and will be clipped rather than wrapped. That is
        // acceptable only because this adapter is the interim path that retires
        // with the HUD's Details tab; the egui and native presenters both wrap.
        DetailRow::Body { text } => Row::Span { text, bold: false },
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
            slideshow: crate::slideshow::Slideshow::default(),
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
            zoom_ease: None,
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
            failed_reason: std::collections::HashMap::new(),
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
            last_breadcrumb_snap: None,
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
            quick_sort_queue: None,
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

    /// Whether a background archive open is still decompressing. Derived directly from
    /// `archive_load` (#131 B) — this replaced a hand-synced `archive_loading: bool` mirror
    /// field that the winit shell wrote each tick and macOS never wrote (so it was stale on
    /// macOS by construction). It is a pure redundancy cleanup: `work_pending` already reads
    /// `archive_load.is_some()` independently, so nothing ever depended on the old flag.
    pub fn archive_loading(&self) -> bool {
        self.archive_load.is_some()
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
    /// (one-shot keys, via the keymap) and the menu (`MenuAction::to_action`). Nearly every arm
    /// now runs here in the core (view/nav/HUD/animation, plus the scan toggles, cancel, and the
    /// delete-confirm request — #131); the only arms still routed to the shell/host via
    /// [`CoreEffect::ShellFlowAction`] are **Quit** (window teardown) and **ToggleToolbar** (a
    /// winit-shell-only concept). Navigation here is a single step (what the menu wants); the
    /// keyboard's held-to-blaze nav and continuous pan/zoom are driven by the hold loop, not this
    /// path.
    pub fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::Next | Action::SkipNext => self.advance(Nav::Forward),
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
            Action::CopyGenerationPrompt => self.copy_generation_prompt(),
            Action::CopyGenerationData => self.copy_generation_data(),
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
            // is inverted too (#131 A.3): `request_delete_confirm` arms the pending item and
            // emits `ShowDeleteConfirm { name }`; the shell only renders the confirm, whose Yes
            // routes back through `ConfirmAnswered(true)` → the core `do_delete(.., true)`.
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
            // Recursive-scan + Show-Archives toggles (NS0 5.6 / #131 A.1, inverted): both are pure
            // core-state logic (re-arm the walk via a `BeginDirScan` effect, flip the pref, toast),
            // identical across the two shells, so the core runs them directly instead of routing a
            // `ShellFlowAction` the shells each re-implemented. The macOS handler arms are now dead
            // code (removed by the Mac session) — no double-run, since the core already did the work.
            Action::Recursive => self.toggle_recursive(),
            Action::ShowArchives => self.toggle_show_archives(),
            // Stop Scanning (NS0 5.6 / #131 A.2, inverted): the core owns the whole policy —
            // cancel-keeps-partial, resume normal prefetch, restore the welcome hint on an empty
            // deck (`cancel_scan_command`) — plus the confirmation toast (its "tail", which each
            // shell used to add). It emits **no dialog effect**: winit no longer presents a
            // Scanning dialog (the ambient pill replaced it), and the macOS Scanning-sheet Cancel
            // closes itself via `DialogResult::ScanningCancelled` — an unconditional `CloseDialog`
            // here would close whatever *unrelated* dialog happened to be up (the rev-1 bug).
            Action::CancelScan => {
                if self.cancel_scan_command() {
                    self.show_toast("Scan stopped");
                }
            }
            // Permanent-delete confirm (NS0 5.6 / #131 A.3, inverted): the core settles + guards +
            // arms `pending_confirm_delete`, then emits `ShowDeleteConfirm { name }` (non-macOS) or
            // the legacy `ShellFlowAction(DeletePermanent)` (macOS lever) — see
            // `request_delete_confirm`. The shell only renders the confirm; Yes routes back through
            // `ConfirmAnswered(true)`.
            Action::DeletePermanent => self.request_delete_confirm(),
            // Quick Sort (task #136): file the photo into slot `i`'s folder. The press itself
            // does no I/O — see `quick_sort_to_slot`.
            Action::QuickSort(i) => self.quick_sort_to_slot(i),
            // Host-side commands — the residue whose execution *is* a platform operation: Quit's
            // window teardown. Routed through the one `ShellFlowAction` seam so the whole action
            // vocabulary still dispatches here; the host runs the native op (see the effect's doc).
            // `ToggleToolbar` (#61) also routes here: the docked toolbar is a Windows/Linux-shell
            // concept (macOS has its native toolbar), so the shell owns flipping `show_toolbar`,
            // persisting it, and re-reserving the photo's top inset — the core stays agnostic.
            Action::Quit | Action::ToggleToolbar => self
                .effects
                .push(contract::CoreEffect::ShellFlowAction(action)),
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
                    // A line-precise zoom notch is coarse — on Windows a touchpad *pinch* is
                    // delivered as ±10% Ctrl+wheel steps, so applying each instantly stairsteps.
                    // Fold the notch into an eased target that the tick glides toward instead.
                    // `PB_EASE_ZOOM=0` restores the instant behavior.
                    if ease_scroll_zoom() {
                        self.queue_zoom_ease((1.0 + y * WHEEL_ZOOM_STEP).max(0.05));
                    } else {
                        self.zoom_about_cursor((1.0 + y * WHEEL_ZOOM_STEP).max(0.05));
                    }
                } else {
                    self.pan_by_pixels(
                        x * WHEEL_PAN_STEP * GESTURE_PAN_DIR,
                        y * WHEEL_PAN_STEP * GESTURE_PAN_DIR,
                    );
                }
            }
        }
    }

    /// Fold one coarse scroll-zoom notch into the eased target (see [`ZoomEase`]).
    /// Compounds onto the in-flight target if a glide is already
    /// running (so a fast flurry of notches accumulates), else onto the live zoom.
    /// The anchor tracks the current cursor so the ease zooms toward wherever the
    /// pointer is. The tick's `apply_zoom_ease` does the actual per-frame motion.
    fn queue_zoom_ease(&mut self, factor: f32) {
        let base = self.zoom_ease.map_or(self.view.zoom, |z| z.target);
        let target = (base * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let anchor = self.last_cursor.unwrap_or_else(|| {
            self.screen_and_image()
                .map_or([0.0, 0.0], |(_, _, sw, sh)| {
                    [sw as f32 / 2.0, sh as f32 / 2.0]
                })
        });
        match self.zoom_ease.as_mut() {
            Some(z) => {
                z.target = target;
                z.anchor = anchor;
            }
            None => {
                self.zoom_ease = Some(crate::ZoomEase {
                    target,
                    anchor,
                    last: None,
                });
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
        // 0''. Quick Sort (task #136): land any move/copy the worker finished — the undo entry
        // is recorded here, and a failure puts the item back in the deck. Cheap no-op until
        // the user's first sort spawns the worker.
        self.poll_quick_sort();
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
        // Also glide any eased scroll-zoom toward its target — OR'd into `transforming` so it
        // keeps the loop ticking + suppresses the info panel like a held ramp.
        let transforming = self.apply_view_holds(now) | self.apply_zoom_ease(now);

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

        self.resolve_parked_failure();

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
        // The Thumbnails breadcrumb (task #129) tracks the *displayed* photo's folder even when
        // the Folders tab is hidden, so it can't ride `drive_fs_tree` (Folders-only). Snapshot-
        // diff the current folder — like the info line below — so a folder change re-signals the
        // host to re-pull, which crucially catches the async cache-miss path: `mark_resolved`
        // updates `displayed_item` with no marker of its own, and `advance` only signalled the
        // *target*. Gated on the strip being visible so a parked, panel-closed session pays
        // nothing; cleared when hidden/non-fs so re-showing re-signals a fresh pull.
        if self.native_tree && self.thumbs_visible() {
            let snap = self
                .tree_is_fs()
                .then(|| self.current_folder_abs())
                .flatten();
            if snap != self.last_breadcrumb_snap {
                self.last_breadcrumb_snap = snap;
                self.emit_panels_changed();
            }
        } else if self.last_breadcrumb_snap.is_some() {
            self.last_breadcrumb_snap = None;
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

    /// Minimum interval between mid-flight folder-tree rebuilds: crossing a folder
    /// boundary every frame at full blaze speed re-rasterizes at most ~10×/s (a ~1 ms
    /// CPU composite each), so the tree tracks live without denting the one-frame-
    /// per-vsync advance budget.
    pub const TREE_FLY_REBUILD: Duration = Duration::from_millis(100);

    // ── Finder-style resident folder browser (task #54, native disk decks) ──────────

    /// Refresh rate in Hz (rounded, ≥1) — caps the Settings blaze-speed slider and is
    /// passed to every dialog window.
    pub fn refresh_hz(&self) -> u32 {
        (1.0 / self.frame_interval.as_secs_f32()).round().max(1.0) as u32
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
                self.slideshow.interval =
                    crate::slideshow::clamp_interval(Duration::from_secs_f64(secs));
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

    // --- Flicker compare (task #43): pin one photo, `Y` flips between it and the
    // current one at full resolution — change detection at a fixed gaze point, the
    // culling tool. The pin rides the prefetch want-list at top-2 priority
    // (`request_prefetch`), so both directions of the flip are ring rebinds.

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
        let identity = pb_core::SlotIdentity {
            item,
            content_gen: cg,
            rep: pb_core::RepKind::Fit,
        };
        let derived = self.renderer.as_mut().and_then(|r| {
            r.derive_fit(
                source,
                res.slot,
                fw,
                fh,
                derive_kernel(),
                derive_mip_bias(),
                identity,
            )
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
        let identity = self.slot_identity(item, pb_core::RepKind::Fit);
        let derived = self.renderer.as_mut().and_then(|r| {
            r.derive_fit(
                pb_render::DeriveSource::Ring(src),
                dst,
                fw,
                fh,
                derive_kernel(),
                derive_mip_bias(),
                identity,
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
            let expected = self.slot_identity(item, pb_core::RepKind::Fit);
            if let Some(a) = self.renderer.as_mut() {
                a.present_slot(dst, expected);
            }
            self.presented_kind = Some(pb_core::RepKind::Fit);
            self.draw();
        }
        true
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
        self.failed_reason.remove(&item);
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
                self.failed_reason.remove(&it);
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
                                recovered: None,
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
        let expected = self.slot_identity(item, kind);
        let bound = match self.renderer.as_mut() {
            Some(r) => {
                r.set_view(view);
                r.present_slot(slot, expected)
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

    // (The `T` panel's display lines moved to `panels::TextPanel::lines` — the
    // shell-neutral model owns the projection; see `Self::text_panel`.)

    // --- AI image description (task #44) ------------------------------------------

    // (The Describe panel's display lines moved to `panels::DescribePanel::lines` —
    // the shell-neutral model owns the projection; see `Self::describe_panel`.)

    /// Show ring `slot` (holding `item`): the keypress fast path — a rebind, no
    /// decode or upload. Updates the pin, title, and info panel.
    /// #109 A — the identity to STAMP a slot with (at upload/derive) or VERIFY a bind against
    /// (at present): the current deck generation plus `item` and its representation. The renderer
    /// refuses a `present_slot` whose slot doesn't carry this, so a core↔renderer divergence fails
    /// loud. A resident slot's `content_gen` always equals `self.content_gen` (stale ones are
    /// dropped), so reading it live here matches the stamp written from the outcome's key.
    fn slot_identity(&self, item: usize, rep: pb_core::RepKind) -> pb_core::SlotIdentity {
        pb_core::SlotIdentity {
            item,
            content_gen: self.content_gen,
            rep,
        }
    }

    pub fn present_item(&mut self, item: usize, slot: usize) -> bool {
        // `present` = the whole event-loop-thread cost of one advance (rebind + title +
        // GPU-submit), the keypress fast path. It's the metric to watch for a hold-to-blaze
        // regression: the NS0 inversion (renderer behind `Box<dyn Renderer>`, window ops as
        // effects) must leave this flat. `--metrics` only; a no-op branch otherwise.
        let t0 = Instant::now();
        let view = self.view_for(item);
        // Ask the renderer to rebind the slot. `present_slot` returns `false` when the slot
        // doesn't hold what the core believes — either it isn't uploaded yet (a core↔renderer
        // ring desync), or (once #109 A lands) its identity stamp mismatches (the wrong
        // occupant, the "archive card over a photo" corruption). **#109 B — atomic present:**
        // a refused bind commits **NO** core-visible state below (no title, no `displayed_item`,
        // no `mark_resolved`) — the renderer keeps the correct held frame — so the title can
        // never "advance over a frozen/wrong view", which is what this method used to do
        // unconditionally. On a refusal we RECOVER instead of self-healing mid-loop (the reverted
        // cff70ca0/c383107a epoch-bump repair): drop the diverged slot's residency belief so
        // `request_prefetch` re-decodes a correctly-stamped texture, keep the target unresolved
        // (so `target_pending` holds the pump awake), and retry on a later tick until a verified
        // bind or a terminal decode failure. Headless (`renderer = None`, unit tests) counts as
        // presented so the pure-core assertions hold.
        let expected = self.slot_identity(item, self.present_kind(item));
        let presented = match self.renderer.as_mut() {
            Some(r) => {
                r.set_view(view);
                r.present_slot(slot, expected)
            }
            None => true,
        };
        if !presented {
            if door_diag() {
                eprintln!(
                    "[door-diag] present_slot({slot}) REFUSED for item {item} (archive_kind={:?}) — recovering",
                    self.item_archive_kind(item),
                );
            }
            // Recovery (no epoch/content_gen bump): evict the diverged slot so its stale
            // residency stops satisfying the prefetch planner, then re-request. The target is
            // deliberately left unresolved.
            self.ring.evict_slot(slot);
            self.request_prefetch();
            return false;
        }
        let title = title_for(self.source.name(item), item, self.source.len());
        // #123 fix 2: the stash is current-photo-scoped — a DIFFERENT photo successfully
        // on screen retires it. Present-success, not mere target churn (a failed present
        // must not orphan pixels we may still return to).
        if self.fit_stash.iter().flatten().any(|s| s.item != item) {
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
        true
    }

    /// A target that failed to decode (corrupt/unreadable): count it as "shown"
    /// so the gated advance isn't stuck on it, but clear the previous frame's
    /// stale metadata — set a decode-error window title and drop the info panel so
    /// neither misreports the held-over pixels as the failed photo.
    ///
    /// **The canvas is BLANKED (task #127):** the previous photo's pixels are cleared so
    /// the "can't display this image" placeholder stands over an empty surface, not the
    /// last good image (owner request 2026-07-20 — a held-over photo reads as though *it*
    /// is the broken one). This runs even mid-nav: navigating *to* a corrupt file holds
    /// the nav key while `present_failed` fires, so a `held_nav`-gated clear left the old
    /// image up — the exact bug the owner hit. A brief black frame while blazing *past* a
    /// corrupt file is the acceptable cost; nav never stalls (the item is marked resolved).
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
        if let Some(r) = self.renderer.as_mut() {
            r.clear_image();
        }
        self.draw();
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
    /// Re-resolve a terminally-failed **parked** target after a geometry-epoch bump.
    /// `try_present_target` (which re-stamps a failed item via `present_failed`) only runs
    /// under a held nav key — so an epoch bump *after* the item first failed (the window
    /// settling on open, a resize) leaves `presented_epoch` stale, keeping
    /// `target_caught_up` false and the loading pie spinning forever on a file that can
    /// never decode (task #127). Re-stamp it here every tick; idempotent once caught up.
    fn resolve_parked_failure(&mut self) {
        if !self.target_caught_up() {
            if let Some(item) = self.target_item {
                if self.failed.contains(&item) {
                    self.present_failed(item);
                }
            }
        }
    }

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
            // #109 B: propagate the present result — a refused bind (wrong occupant / not
            // uploaded) means the target is NOT on screen, so readiness stays pending and the
            // pump retries after the recovery re-decodes. Never report a refusal as "shown".
            self.present_item(item, slot)
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
                    self.failed_reason
                        .insert(item, crate::engine::clean_decode_reason(&e));
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
                self.failed_reason
                    .insert(item, crate::engine::clean_decode_reason(e));
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
            } else if img.recovered.is_some() {
                // The cache was seeded by whichever decode landed FIRST — for the parked
                // target that's usually the preview (often a clean embedded thumbnail),
                // which carries no recovery flag. When the malformed FULL decode arrives
                // and salvages it (task #127), merge the flag into the cached meta and the
                // live `current` mirror, so the details notice appears without a
                // re-navigation. Only ever sets it, never clears — a clean full can't
                // un-recover an item.
                if let Some(m) = self.meta_cache.get_mut(&item) {
                    if m.recovered.is_none() {
                        m.recovered = img.recovered.clone();
                    }
                }
                if self.displayed_item == Some(item) {
                    if let Some(cur) = self.current.as_mut() {
                        if cur.recovered.is_none() {
                            cur.recovered = img.recovered.clone();
                        }
                    }
                }
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
                        pb_core::SlotIdentity {
                            item,
                            content_gen: cg,
                            rep: rk,
                        },
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
                    let expected = pb_core::SlotIdentity {
                        item,
                        content_gen: cg,
                        rep: rk,
                    };
                    if let Some(a) = self.renderer.as_mut() {
                        a.present_slot(slot, expected);
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
                        pb_core::SlotIdentity {
                            item,
                            content_gen: cg,
                            rep: rk,
                        },
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
                self.failed_reason
                    .insert(idx, crate::engine::clean_decode_reason(&e));
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

    // Shell → core callbacks for the macOS native player (task 79.9 phase 2). The
    // shell's `AVPlayer` is the timing/lifecycle authority; these advance the passive
    // `NativeVideoProxy` so the core's play/pause/replay dispatch + policy see real
    // state. Each is session-gated inside the proxy (a stale player is ignored).

    // ── The audio track picker (task #99) ────────────────────────────────
    //
    // Audio differs from subtitles in a way that shapes all of this: the core does not own
    // the choice. The decoder picks a track at open, the shell owns the player, and only
    // the shell can say what is coming out of the speakers. So the core formats the rows and
    // hands out locators; the shell acts and reports back.

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
    use crate::PbKey;
    use test_support::{
        clipboard_text_effects, five_photos, make_resident, photos_named, poster_payload,
        rgba_full, seed_details, stuck_preview_core, test_core, text_result, track, DeriveOk,
        FakeArchive,
    };

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

    // -- A / Shift+A audio cycling (#99) ------------------------------------

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
            recovered: None,
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

    /// **Regression (task #127).** A truly-undecodable file, PARKED, left the loading
    /// pie spinning forever: `present_failed` resolves it at the current epoch, but the
    /// window settling on open bumps the geometry epoch, and nothing re-stamps a failed
    /// parked target (`try_present_target` runs only under a held nav key), so
    /// `target_caught_up` stayed false and `tick_pie` kept the pie up. This is the exact
    /// state the owner saw on a corrupt `dead.jpg`.
    #[test]
    fn a_parked_failed_target_reresolves_after_a_geometry_epoch_bump() {
        let mut core = test_core();
        core.source = photos_named(&["dead.jpg"]);
        core.playlist = Playlist::new(1, 0);
        core.target_item = Some(0);
        core.failed.insert(0);
        core.present_failed(0);
        assert!(
            core.target_caught_up(),
            "present_failed resolves it at the current epoch"
        );

        // A geometry change (the window settling on open, or a resize) bumps the epoch,
        // staling the stamp — exactly what happened between epoch 2 and 3 on the real file.
        core.epoch += 1;
        assert!(!core.target_caught_up(), "the epoch bump un-resolves it");

        // Parked (no nav key held): the tick's resolve must re-stamp it.
        core.resolve_parked_failure();
        assert!(
            core.target_caught_up(),
            "a parked failed target must re-resolve after the bump, or the pie spins forever"
        );
    }

    /// **Regression (task #127).** The parked target shows its embedded preview
    /// (often a clean thumbnail) FIRST — that seeds `meta_cache`/`current` with no
    /// recovery flag. When the malformed FULL decode lands later, salvaged by the
    /// ladder, its flag must merge into the already-seeded cache and the live mirror,
    /// or the details notice never appears (the exact symptom the owner hit on
    /// IMG_1340: the image displayed, but Details showed nothing).
    #[test]
    fn a_later_full_decode_merges_the_recovery_flag_the_clean_preview_lacked() {
        use crate::meta::PhotoMeta;
        let mut core = test_core();
        core.source = photos_named(&["ticket.jpg"]);
        core.playlist = Playlist::new(1, 0);
        core.fit = Some(FitBox {
            max_width: 800,
            max_height: 600,
        });
        core.targets = vec![0];
        core.ring = ResidentRing::new(4);
        core.displayed_item = Some(0);

        // The clean preview already landed → cache + current carry NO flag.
        let preview = PhotoMeta {
            rel: "ticket.jpg".into(),
            w: 4864,
            h: 3616,
            size: None,
            codec: "JPEG",
            animated: None,
            recovered: None,
        };
        core.meta_cache.insert(0, preview.clone());
        core.current = Some(preview);
        core.preview_resident.insert(0);

        // The malformed FULL Original decode lands, salvaged by the recovery ladder.
        let mut img = rgba_full(64, 48, 4864, 3616);
        img.recovered = Some("Extra bytes between headers".into());
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Original,
            Ok(img),
        ));
        core.drain_results();

        assert_eq!(
            core.meta_cache.get(&0).and_then(|m| m.recovered.as_deref()),
            Some("Extra bytes between headers"),
            "the full decode's recovery flag must merge into the preview-seeded cache"
        );
        assert_eq!(
            core.current.as_ref().and_then(|m| m.recovered.as_deref()),
            Some("Extra bytes between headers"),
            "and into the live `current` mirror so the details notice appears"
        );
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

    // --- "Text in image" state machine (task #45): drive `poll_text_scan` with a
    // hand-fed channel — the worker/OCR backend stays out of these tests entirely.

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
            recovered: None,
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

    // --- AI describe state machine (task #44): drive `poll_describe_scan` with a
    // hand-fed channel; the worker/HTTP backend stays out of these tests entirely. The
    // endpoint is blanked so nothing can spawn a real network thread.

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
    fn recursive_and_show_archives_dispatch_in_core_without_a_flow_effect() {
        // #131 A.1: both toggles are inverted off `ShellFlowAction` — the core runs them during
        // dispatch and re-arms the walk via a `BeginDirScan` effect. The mirror of the existing
        // "no ShellFlowAction" assertion, proving the inversion actually happened. (A scan root is
        // set so `Recursive` isn't a no-op.)
        for action in [Action::Recursive, Action::ShowArchives] {
            let mut core = test_core();
            core.scan_root = Some(std::path::PathBuf::from("/photos"));
            core.handle(CoreEvent::MenuAction(action));
            assert!(
                !core
                    .effects
                    .iter()
                    .any(|e| matches!(e, contract::CoreEffect::ShellFlowAction(_))),
                "{action:?} must not route a ShellFlowAction"
            );
            assert!(
                core.effects
                    .iter()
                    .any(|e| matches!(e, contract::CoreEffect::BeginDirScan { .. })),
                "{action:?} re-arms the walk directly"
            );
        }
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

    // -- media-track Details rows (task #98) --------------------------------

    // -- the off-thread Details probe (task 98.6) ---------------------------

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
            recovered: None,
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

    // ── #106.7: parked full-res tier + instant Fit↔1:1 rebind ──

    fn meta_dims(rel: &str, w: u32, h: u32) -> crate::meta::PhotoMeta {
        crate::meta::PhotoMeta {
            rel: rel.into(),
            w,
            h,
            size: None,
            codec: "JPEG",
            animated: None,
            recovered: None,
        }
    }

    // --- Poster selection (task #114, phase 1) ------------------------------

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
                recovered: None,
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
            _: pb_core::SlotIdentity,
        ) -> bool {
            false
        }
        fn present_slot(&mut self, _: usize, _: pb_core::SlotIdentity) -> bool {
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

    // ── #109 B: atomic present — a refused bind commits no state, and recovers ──

    /// A `Renderer` double whose `present_slot` REFUSES every bind — the answer a real renderer
    /// gives once #109 A's identity stamp catches a wrong occupant (and today, a not-yet-uploaded
    /// slot). `upload_slot` succeeds so a slot can be marked resident before the refusal.
    struct RefusingPresent;

    impl pb_render::Renderer for RefusingPresent {
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
            _: pb_core::SlotIdentity,
        ) -> bool {
            true
        }
        fn present_slot(&mut self, _: usize, _: pb_core::SlotIdentity) -> bool {
            false
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

    /// #109 B — a refused present commits **no** core-visible state: no title, no
    /// `displayed_item`, no `mark_resolved` (`presented_epoch`). Before this, `present_item`
    /// advanced all three unconditionally — the "title advances but the view is frozen"
    /// corruption. It must also report not-shown so readiness stays pending and the pump retries
    /// after the recovery re-decodes.
    #[test]
    fn a_refused_present_commits_no_state_and_reports_not_shown() {
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
        core.renderer = Some(Box::new(RefusingPresent));
        core.displayed_item = None;
        core.presented_epoch = None;
        core.effects.clear();

        let shown = core.present_item(0, 0);

        assert!(!shown, "a refused present reports not-shown");
        assert_eq!(
            core.displayed_item, None,
            "no displayed_item commit on a refusal"
        );
        assert_eq!(
            core.presented_epoch, None,
            "no mark_resolved (presented_epoch) on a refusal"
        );
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::SetTitle(_))),
            "no title advance over a frozen view (#109 B atomic present)"
        );
    }

    // ── #109 A: identity stamp — verify the bind, refuse a wrong occupant ──

    /// A `Renderer` double that faithfully mirrors the real `WgpuRenderer`'s #109 A contract:
    /// `upload_slot` STAMPS the slot with its identity; `present_slot` REFUSES a bind whose
    /// `expected` doesn't match the stamp (keeps the held frame). Proves the core↔renderer
    /// identity handshake + the refusal recovery end-to-end, with no GPU.
    struct StampingRenderer {
        stamps: Vec<Option<pb_core::SlotIdentity>>,
    }
    impl StampingRenderer {
        fn new(cap: usize) -> Self {
            Self {
                stamps: vec![None; cap],
            }
        }
    }
    impl pb_render::Renderer for StampingRenderer {
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
            slot: usize,
            _: &[u8],
            _: u32,
            _: u32,
            _: pb_render::ColorTransform,
            _: bool,
            _: f32,
            _: bool,
            identity: pb_core::SlotIdentity,
        ) -> bool {
            if slot >= self.stamps.len() {
                return false;
            }
            self.stamps[slot] = Some(identity);
            true
        }
        fn present_slot(&mut self, slot: usize, expected: pb_core::SlotIdentity) -> bool {
            self.stamps.get(slot).copied().flatten() == Some(expected)
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

    /// #109 A — the wrong-occupant guard, end-to-end: a slot stamped for one identity refuses a
    /// present that expects another, and the core recovers (evicts the diverged slot so it
    /// re-decodes) instead of showing stale pixels — the "archive card over a photo" corruption.
    /// A matching identity still binds (the happy path is preserved).
    #[test]
    fn present_refuses_a_diverged_slot_stamp_and_recovers() {
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
        core.renderer = Some(Box::new(StampingRenderer::new(4)));

        // The drain uploads item 0's Fit AND stamps its slot with (0, content_gen, Fit).
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Fit,
            Ok(rgba_full(100, 67, 6000, 4000)),
        ));
        core.drain_results();
        let slot = core
            .ring
            .slot_for_rep(0, pb_core::RepKind::Fit)
            .expect("item 0 Fit is resident + stamped");

        // Happy path: a correctly-stamped slot binds.
        assert!(core.present_item(0, slot), "a matching identity binds");

        // Divergence: the deck's content generation advances but the ring wasn't cleared, so the
        // slot's stamp is now stale — the shape of a core↔renderer wrong-occupant desync.
        core.content_gen = core.content_gen.wrapping_add(1);
        core.effects.clear();
        let shown = core.present_item(0, slot);

        assert!(
            !shown,
            "a stale-generation stamp is REFUSED (the wrong-occupant guard)"
        );
        assert!(
            core.ring.slot_for_rep(0, pb_core::RepKind::Fit).is_none(),
            "the diverged slot is evicted so a fresh, correctly-stamped decode is requested"
        );
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::SetTitle(_))),
            "no state committed on the refusal"
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

    // ── #122: parked-tier livelock guard + derive-before-preview on nav ──

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

    // -- #126 step 2: the archive-open lifecycle, now core-owned -----------------------------

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

    // -- #126 step 1: the directory-scan lifecycle, now core-owned --------------------------

    // -- #124: smooth zoom binds the resident Original ----------------------------------

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
            recovered: None,
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
            recovered: None,
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
            recovered: None,
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
            recovered: None,
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
            recovered: None,
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

    // ---- Thumbnails strip (task #83) ----

    // --- archive doors: entering (task #104) ------------------------------

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
}
