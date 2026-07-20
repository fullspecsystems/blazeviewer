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
