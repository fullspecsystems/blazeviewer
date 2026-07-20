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
