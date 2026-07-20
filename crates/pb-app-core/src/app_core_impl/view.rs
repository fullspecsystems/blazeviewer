//! **View transform** — fit/fill/1:1, zoom, pan, rotate (task #125).
//!
//! The *parked* half of the performance model: once the user settles on an item, every one of
//! these has the same budget as a blaze frame — interaction → photon within one refresh — and
//! quality is maximum. That is why they are cheap state changes here and why the pixels they
//! need are pre-arranged by the residency engine in the parent, not fetched on demand.
//!
//! ⚠ House rule from #124, worth re-reading before touching any of this: background work may
//! change residency or quality, **never the presented representation**. `rotate` also
//! invalidates the item's cached OCR text and AI description, because the rotated pixels are
//! a different image.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
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

    /// Push the current view transform to the renderer (re-places the quad).
    pub fn push_view(&mut self) {
        let view = self.view;
        if let Some(a) = self.renderer.as_mut() {
            a.set_view(view);
        }
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
}
