//! **Compare (A/B pin)** — the `AppCore` methods behind pinning one item and toggling
//! between it and the current one (task #125).
//!
//! A *topic*, not a subsystem: the state is a couple of `AppCore` fields, not an owned
//! module. `docs/where-code-goes.md` allows a topic its own `app_core_impl/` file without
//! inventing a sibling to pair with.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
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
    pub(super) fn compare_identity(&self, item: usize) -> String {
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
    pub(super) fn compare_carry_view(&self, to: usize) -> Option<(f32, [f32; 2])> {
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
}
