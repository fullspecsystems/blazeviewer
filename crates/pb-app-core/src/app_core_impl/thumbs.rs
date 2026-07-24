//! **Thumbnail strip** — the `AppCore` half of [`crate::thumbs`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! `thumbs.rs` owns the cache and layout; this file holds the `impl AppCore` methods that
//! show/hide the strip, jump from a click, and capture a decoded frame into the cache.
//!
//! ⚠ `thumbs_capture` is `pub(super)` because `drain_results` calls it from the middle of
//! the residency engine, which stays in the parent. That coupling is real, not an artefact
//! of the split.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
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
                        recovered: None,
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
    pub(super) fn thumbs_capture(&mut self, o: crate::decode_pool::Outcome) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{
        five_photos, photos_named, poster_payload, test_core,
    };

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
            recovered: None,
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
            recovered: None,
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
}
