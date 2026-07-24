//! **Navigation and the playlist** — the `AppCore` methods that move the cursor through the
//! deck and rebuild it (task #125).
//!
//! `nav_press`/`advance`/`held_nav` are the canonical spine — space/backspace/enter, and
//! hold-any-nav-key to blaze. The blaze budget lives in the root `CLAUDE.md`: keypress →
//! photon within one refresh interval, which holds only because a keypress is a REBIND, never
//! a decode and never an upload. Nothing in this file may reach for a decode.
//!
//! The prefetch ring that makes that true stays in the parent, with the rest of the
//! residency engine.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
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
        self.failed_reason.clear();
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
        self.failed_reason.clear();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{
        five_photos, make_resident, photos_named, stuck_preview_core, test_core, DeriveOk,
    };
    use crate::contract::CoreEvent;
    use crate::PbKey;

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
            recovered: None,
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
}
