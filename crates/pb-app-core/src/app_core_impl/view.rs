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

    /// Drive the eased scroll-zoom set up by `queue_zoom_ease`. Each tick closes an exponential
    /// fraction of the remaining gap between
    /// the live zoom and the target (`ZOOM_EASE_TAU`), applying the step **about the stored
    /// anchor** so the pixel under the cursor stays pinned across the whole glide — turning the
    /// Windows touchpad's coarse ±10% pinch notches into a smooth ramp. Snaps to the target and
    /// clears the ease once within `ZOOM_EASE_EPS`. Returns whether it advanced (so the tick
    /// keeps polling + redrawing, exactly like a held zoom ramp). A held zoom key wins — it owns
    /// `view.zoom`, so any in-flight ease is dropped rather than fighting it.
    pub fn apply_zoom_ease(&mut self, now: Instant) -> bool {
        let Some(mut ease) = self.zoom_ease else {
            return false;
        };
        if self.zoom_held().is_some() {
            self.zoom_ease = None;
            return false;
        }
        let Some((iw, ih, sw, sh)) = self.screen_and_image() else {
            self.zoom_ease = None;
            return false;
        };

        let last = ease.last.unwrap_or(now);
        let dt = (now - last).as_secs_f32().min(0.1);
        ease.last = Some(now);

        let ratio = ease.target / self.view.zoom;
        let alpha = if (ratio - 1.0).abs() <= ZOOM_EASE_EPS {
            // Close enough: snap exactly onto the target and finish (no asymptotic tail).
            1.0
        } else {
            // Otherwise close an exponential fraction of the gap; `dt == 0` (first tick) latches
            // the clock and moves nothing — the glide starts next tick.
            crate::engine::zoom_ease_alpha(dt)
        };
        // `ratio^alpha` is the fraction of the multiplicative gap to close this tick; `alpha == 1`
        // applies the whole remaining `ratio`, landing exactly on the target.
        let step = ratio.powf(alpha);
        // #124: reconcile AFTER `zoom_about` (it reads the bound texture dims to pin the anchor).
        self.view.zoom_about(step, ease.anchor, iw, ih, sw, sh);
        self.reconcile_zoom_rep();
        self.push_view();
        self.draw();
        // A pinch that crosses whether the image overflows flips the grab affordance.
        self.refresh_cursor();

        if alpha >= 1.0 {
            self.zoom_ease = None;
        } else {
            self.zoom_ease = Some(ease);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{make_resident, photos_named, test_core, StashOk};

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

    // ----- Eased scroll-zoom (`apply_zoom_ease` / `queue_zoom_ease`) -----------------------------

    /// The first tick only latches the clock (dt = 0) and must NOT snap to the target — the
    /// glide happens on the ticks that have real elapsed time.
    #[test]
    fn eased_zoom_first_tick_latches_without_snapping() {
        let mut core = zoom_test_core();
        let t0 = core.now;
        core.zoom_ease = Some(crate::ZoomEase {
            target: 2.0,
            anchor: [0.5, 0.5],
            last: None,
        });

        let advanced = core.apply_zoom_ease(t0);
        assert!(advanced, "an active ease keeps the loop ticking");
        assert!(
            (core.view.zoom - 1.0).abs() < 1e-6,
            "the first (dt=0) tick must not move the zoom (got {})",
            core.view.zoom
        );
        assert!(core.zoom_ease.is_some(), "the ease is still in flight");
    }

    /// A flurry of scroll notches compounds onto a single eased target (so fast pinching
    /// accumulates rather than resetting), clamped to the view's zoom range, with the anchor
    /// tracking the live cursor. (Convergence of the glide itself is proved hermetically in
    /// `engine::tests::zoom_ease_alpha_*`, since the headless mock reports a zero image size.)
    #[test]
    fn queued_notches_compound_onto_one_target() {
        let mut core = zoom_test_core();
        core.view.zoom = 1.0;
        core.last_cursor = Some([12.0, 34.0]);

        core.queue_zoom_ease(1.1);
        core.queue_zoom_ease(1.1);
        let ease = core.zoom_ease.expect("an ease is in flight");
        assert!(
            (ease.target - 1.1 * 1.1).abs() < 1e-5,
            "two notches compound multiplicatively (got {})",
            ease.target
        );
        assert_eq!(ease.anchor, [12.0, 34.0], "the anchor tracks the cursor");

        // Zoom-out notches below the floor clamp to MIN_ZOOM, never past it.
        core.zoom_ease = None;
        core.view.zoom = MIN_ZOOM;
        core.queue_zoom_ease(0.5);
        assert!(
            (core.zoom_ease.unwrap().target - MIN_ZOOM).abs() < 1e-6,
            "the target is clamped to the zoom floor"
        );
    }

    /// A held zoom key owns `view.zoom`; an in-flight ease must yield to it rather than fight,
    /// dropping itself on the next tick.
    #[test]
    fn eased_zoom_yields_to_a_held_zoom_key() {
        let mut core = zoom_test_core();
        core.zoom_ease = Some(crate::ZoomEase {
            target: 4.0,
            anchor: [0.5, 0.5],
            last: None,
        });
        // Simulate a held zoom-in key (the mechanism `zoom_held` reads).
        core.held.insert(PbKey::Equal, Action::ZoomIn);

        let advanced = core.apply_zoom_ease(core.now);
        assert!(!advanced, "the ease defers to the held key");
        assert!(core.zoom_ease.is_none(), "and drops itself");
    }
}
