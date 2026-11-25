//! **The live 3D model view — AppCore's half** (task #153). State and the worker job live in
//! [`crate::live_model`]; here is when to prepare a model, how input turns its camera, and how
//! the frame reaches the renderer.
//!
//! The routing rule (owner decision D1): once the model on screen is ready, the input that would
//! pan or zoom a photo turns or zooms the model instead. That covers drag, the two-finger swipe,
//! pinch, the scroll-wheel zoom, the held `=`/`-` keys and the arrows. The first such input
//! swaps the still for the live view. Fit (`8`), 100% (`0`) and the smart-magnify double-tap
//! put the still back. Navigation leaves, always: presenting any photo stops the renderer
//! drawing the mesh, and the next tick frees it.
//!
//! `P` is the turntable: the model spins on its own until `P` again (it coasts to a stop) or a
//! drag takes it back in hand. Zooming leaves it spinning. The play-hint pill advertises it once
//! the model is ready, which is also the cue that a model can be turned at all.

use std::sync::atomic::AtomicBool;

use super::*;
use crate::live_model::{self, Loading, Ready, Spin};

/// How long a turned view must hold still before it counts as settled (see
/// [`AppCore::live_view_moving`]).
const VIEW_SETTLE: Duration = Duration::from_millis(250);

impl AppCore {
    /// Per-tick upkeep: drop whatever belongs to an item we've left, land a finished preparation
    /// on the GPU, and start preparing the model the user has settled on (anticipation: the first
    /// drag should never wait for a parse).
    pub fn tick_live_model(&mut self, blazing: bool) {
        let (shown, gen) = (self.displayed_item, self.content_gen);
        if self.live_model.holds_other_than(shown, gen) {
            self.leave_live_model();
        }
        if self
            .live_model
            .spin_wanted
            .is_some_and(|(i, g)| Some(i) != shown || g != gen)
        {
            self.live_model.spin_wanted = None; // P was for a model we've left
        }
        self.poll_live_model();

        let Some(item) = shown else {
            return;
        };
        let settled = !blazing && self.presented_epoch == Some(self.epoch);
        let lm = &self.live_model;
        if !settled || lm.loading.is_some() || lm.ready.is_some() || lm.failed == Some((item, gen))
        {
            return;
        }
        if self.item_is_model(item) {
            self.start_live_model(item);
        }
    }

    /// Settings ▸ Appearance changed the model look; `old` is what it replaced. A spinning
    /// model eases to the new speed. A new colour reaches the live view on its next draw (a
    /// uniform), and the stills baked in the old one are re-rendered.
    pub(crate) fn apply_model_look(&mut self, old: &crate::settings::Settings) {
        pb_decode::set_model_color(self.settings.model_color);
        if old.model_spin_rpm != self.settings.model_spin_rpm {
            let cruise = live_model::spin_speed(self.settings.model_spin_rpm);
            if let Some(s) = self.live_model.spin.as_mut() {
                s.set_cruise(cruise);
            }
        }
        if old.model_color != self.settings.model_color {
            self.recolor_models();
        }
    }

    /// The model colour changed (Settings ▸ Appearance). The live view picks it up on its next
    /// draw (a uniform), but every model still already decoded was baked in the old colour.
    /// Those, and only those, are drawn again: photos, video posters, full-size originals and
    /// their strip tiles stay put, so a new colour costs the rest of the folder nothing. (It
    /// used to be a deck-wide content change, which re-decoded everything and re-ran every
    /// poster search, and the Settings window's auto-apply repeated it while the colour was
    /// being picked.) A deck with no model in it pays nothing.
    ///
    /// - The model on screen keeps showing until its redraw replaces it in place, the way a
    ///   preview sharpens (`drain_results`), so it never blanks. The other models' stills are
    ///   dropped, and the prefetch draws them again in its usual order.
    /// - Strip tiles stay on show until they are derived again from the new pixels.
    /// - Model decodes already under way in the old colour are stopped; any that were already
    ///   on their way are dropped when they arrive ([`Self::model_still_outdated`]).
    pub(crate) fn recolor_models(&mut self) {
        let source = Arc::clone(&self.source);
        let is_model = |item: usize| is_model_item(source.as_ref(), item);
        if !(0..source.len()).any(is_model) {
            return;
        }
        self.live_model.stills_look = pb_decode::look_generation();
        self.pool.cancel_where(is_model);
        self.pending_uploads.retain(|o| !is_model(o.key.item));
        let on_screen = self
            .displayed_item
            .filter(|&d| is_model(d))
            .and_then(|d| Some((d, self.display_slot(d)?)));
        self.ring
            .evict_where(is_model, on_screen.map(|(_, slot)| slot));
        if let Some((item, _)) = on_screen {
            self.preview_resident.insert(item);
            self.upgrade_done.remove(&item);
        }
        if self.fit_stash.iter().flatten().any(|s| is_model(s.item)) {
            self.clear_fit_stash();
        }
        self.thumbs.refresh_where(is_model);
        self.request_prefetch();
    }

    /// A model still decoded before the last colour change: dropped on arrival, because it may
    /// show the old colour (see [`Self::recolor_models`]).
    pub(super) fn model_still_outdated(&self, o: &crate::decode_pool::Outcome) -> bool {
        o.look_gen < self.live_model.stills_look && self.item_is_model(o.key.item)
    }

    /// The turned model on screen, as Export, Copy and Describe capture it (task #153.6): its
    /// camera, and the viewport size in pixels the capture renders at (owner decision: the
    /// window as shown, at its real pixels). `None` unless the live view is up on `item`.
    pub(crate) fn live_view_capture(&self, item: usize) -> Option<(pb_mesh::LiveFrame, [u32; 2])> {
        if !self.live_model.active || self.displayed_item != Some(item) {
            return None;
        }
        let ready = self.live_model.ready_for(item, self.content_gen)?;
        let vp = self.live_viewport()?;
        Some((ready.view.frame(vp), [vp[0] as u32, vp[1] as u32]))
    }

    /// Whether the turned view is still moving: spinning, or turned, zoomed or panned within
    /// the last [`VIEW_SETTLE`]. A drag changes the view every frame, and the automatic text
    /// scan and describe must not render the whole window for each one.
    pub(crate) fn live_view_moving(&self) -> bool {
        self.live_model.active && (self.live_model.spin.is_some() || self.live_view_settling())
    }

    /// The view moved less than [`VIEW_SETTLE`] ago: the pump keeps ticking until it settles,
    /// so the automatic runs start then rather than at the next input.
    pub(crate) fn live_view_settling(&self) -> bool {
        self.live_model.active
            && self
                .live_model
                .moved_at
                .is_some_and(|t| self.now.saturating_duration_since(t) < VIEW_SETTLE)
    }

    /// Whether `item` is a 3D model: an image item with a model extension.
    fn item_is_model(&self, item: usize) -> bool {
        is_model_item(self.source.as_ref(), item)
    }

    fn start_live_model(&mut self, item: usize) {
        let gen = self.content_gen;
        let source = Arc::clone(&self.source);
        let cancel = Arc::new(AtomicBool::new(false));
        let job_cancel = Arc::clone(&cancel);
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("pb-live-model".into())
            .spawn(move || {
                let _ = tx.send(live_model::prepare(source.as_ref(), item, &job_cancel));
            });
        if spawned.is_err() {
            self.live_model.failed = Some((item, gen));
            return;
        }
        self.live_model.loading = Some(Loading {
            item,
            content_gen: gen,
            rx,
            cancel,
        });
    }

    /// Land a finished preparation: upload the soup and keep the camera, if the user is still
    /// on that model. A refused or failed model keeps its still, quietly (the still already
    /// shows the model; this only loses the ability to turn it).
    fn poll_live_model(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let result = {
            let Some(l) = self.live_model.loading.as_ref() else {
                return;
            };
            match l.rx.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => Err("worker died".into()),
            }
        };
        let Some(loading) = self.live_model.loading.take() else {
            return;
        };
        let (item, gen) = (loading.item, loading.content_gen);
        if self.displayed_item != Some(item) || self.content_gen != gen {
            return; // stale: the user moved on while it parsed
        }
        // PB_TRACE: the live view's lifecycle, by item index only (never a name or path).
        let trace = pb_trace();
        let uploaded = match result {
            Ok(prepared) => {
                let corners = prepared.vertices.len();
                let ok = self
                    .renderer
                    .as_mut()
                    .is_some_and(|r| r.set_mesh(Some(&prepared.vertices)));
                if trace {
                    eprintln!("PB live-model item={item} corners={corners} uploaded={ok}");
                }
                ok.then_some(prepared.view)
            }
            Err(why) => {
                if trace {
                    eprintln!("PB live-model item={item} not prepared: {why}");
                }
                None
            }
        };
        let wanted_spin = self.live_model.spin_wanted.take() == Some((item, gen));
        match uploaded {
            Some(view) => {
                self.live_model.ready = Some(Ready {
                    item,
                    content_gen: gen,
                    view,
                });
                self.refresh_cursor(); // the open hand: this can be grabbed now
                if wanted_spin {
                    self.start_spin();
                }
            }
            None => self.live_model.failed = Some((item, gen)),
        }
    }

    /// Drop the live view entirely: stop any preparation, stop drawing the mesh, free it.
    pub fn leave_live_model(&mut self) {
        let had_gpu = self.live_model.ready.take().is_some();
        self.live_model.loading = None; // dropping a `Loading` cancels its worker
        self.live_model.active = false;
        self.live_model.spin = None;
        self.live_model.spin_wanted = None;
        if had_gpu {
            if let Some(r) = self.renderer.as_mut() {
                r.show_mesh(None);
                r.set_mesh(None);
            }
        }
    }

    /// The content area (below the top inset) in surface pixels: the live view's viewport, the
    /// same box the still is fitted into.
    fn live_viewport(&self) -> Option<[f32; 2]> {
        let (w, h) = self.renderer.as_ref()?.surface_size();
        let h = h.saturating_sub(self.content_top_inset);
        (w > 0 && h > 0).then_some([w as f32, h as f32])
    }

    /// Whether the model on screen is ready to turn (drives the open-hand cursor).
    pub fn live_model_ready(&self) -> bool {
        self.displayed_item
            .is_some_and(|i| self.live_model.ready_for(i, self.content_gen).is_some())
    }

    /// Apply `f` to the on-screen model's camera and bring the live view up. No draw; returns
    /// whether the live view took the input (false = not a ready model, handle it as a photo).
    fn live_model_apply(&mut self, f: impl FnOnce(&mut pb_mesh::LiveView, [f32; 2])) -> bool {
        let Some(vp) = self.live_viewport() else {
            return false;
        };
        let Some(item) = self.displayed_item else {
            return false;
        };
        let gen = self.content_gen;
        let Some(ready) = self.live_model.ready_for_mut(item, gen) else {
            return false;
        };
        f(&mut ready.view, vp);
        if !self.live_model.active && pb_trace() {
            eprintln!(
                "PB live-model item={item} active viewport={}x{}",
                vp[0], vp[1]
            );
        }
        self.live_model.active = true;
        self.live_model.moved_at = Some(self.now);
        self.anim_hint_shown_for = Some(item); // engaged: the hint has done its job
        true
    }

    /// Turn the model by a drag or swipe of `(dx, dy)` pixels, without drawing. Taking the model
    /// in hand stops the turntable.
    pub fn live_model_orbit_quiet(&mut self, dx: f32, dy: f32) -> bool {
        let took = self.live_model_apply(|v, vp| v.orbit_by(dx, dy, vp));
        if took {
            self.live_model.spin = None;
        }
        took
    }

    /// Turn the model by a drag or swipe of `(dx, dy)` pixels, and draw.
    pub fn live_model_orbit(&mut self, dx: f32, dy: f32) -> bool {
        let took = self.live_model_orbit_quiet(dx, dy);
        if took {
            self.draw();
        }
        took
    }

    /// Zoom the model by `factor`, about the centre, without drawing.
    pub fn live_model_zoom_quiet(&mut self, factor: f32) -> bool {
        self.live_model_apply(|v, _| v.zoom_by(factor))
    }

    /// Zoom the model by `factor` about the pointer (pinch, scroll wheel, menu), and draw.
    pub fn live_model_zoom(&mut self, factor: f32) -> bool {
        let inset = self.content_top_inset as f32;
        let cursor = self.last_cursor.map(|[x, y]| [x, y - inset]);
        let took = self.live_model_apply(|v, vp| match cursor {
            Some(c) => v.zoom_about(factor, c, vp),
            None => v.zoom_by(factor),
        });
        if took {
            self.draw();
        }
        took
    }

    /// Put the still back (Fit / 100% / double-tap while the live view is up): the camera goes
    /// home, which is exactly the still's view, and the supersampled still takes over again.
    /// Returns whether there was a live view to reset.
    pub fn live_model_reset(&mut self) -> bool {
        if !self.live_model.active {
            return false;
        }
        let Some(item) = self.displayed_item else {
            return false;
        };
        let gen = self.content_gen;
        let Some(ready) = self.live_model.ready_for_mut(item, gen) else {
            return false;
        };
        ready.view.reset();
        self.live_model.active = false;
        self.live_model.spin = None;
        if let Some(r) = self.renderer.as_mut() {
            r.show_mesh(None);
        }
        self.draw();
        true
    }

    /// `P` on a 3D model: start the turntable, or let it coast to a stop. Returns whether the item
    /// on screen is a model `P` acts on; `false` hands `P` to its other meanings.
    ///
    /// A model that isn't on the GPU yet (`P` straight after landing) spins the moment it lands.
    /// One that can't be turned at all (too big, or broken) isn't claimed.
    pub fn live_model_toggle_spin(&mut self) -> bool {
        let Some(item) = self.displayed_item else {
            return false;
        };
        let gen = self.content_gen;
        if self.live_model.ready_for(item, gen).is_none() {
            if !self.item_is_model(item) || self.live_model.failed == Some((item, gen)) {
                return false;
            }
            let lm = &mut self.live_model;
            lm.spin_wanted = (lm.spin_wanted != Some((item, gen))).then_some((item, gen));
            self.anim_hint_shown_for = Some(item);
            return true;
        }
        match self.live_model.spin.as_mut() {
            Some(s) if s.running() => s.stop(),
            Some(s) => s.resume(),
            None => self.start_spin(),
        }
        true
    }

    /// Bring the live view up where the camera is and set the turntable going. The ticks turn it.
    fn start_spin(&mut self) {
        if self.live_model_apply(|_, _| {}) {
            let cruise = live_model::spin_speed(self.settings.model_spin_rpm);
            self.live_model.spin = Some(Spin::start(cruise));
            self.draw();
        }
    }

    /// Whether the model on screen is spinning (or spinning up): the toolbar's Play button lights.
    pub fn live_model_spinning(&self) -> bool {
        self.live_model.spin.is_some_and(|s| s.running())
    }

    /// The turntable's tick: turn the model by however far it spun since the last one. Doesn't
    /// draw (the tick draws once for this and any held key); returns whether it moved, which also
    /// keeps the loop at frame rate.
    pub fn spin_live_model(&mut self, now: Instant) -> bool {
        let Some(spin) = self.live_model.spin.as_mut() else {
            return false;
        };
        let angle = spin.advance(now);
        if spin.stopped() {
            self.live_model.spin = None;
        }
        if !self.live_model_apply(|v, _| v.spin_by(angle)) {
            self.live_model.spin = None; // nothing on screen to spin
            return false;
        }
        true
    }

    /// A drag moved the model `(dx, dy)` px at `at`: remember it for the throw on release.
    pub fn live_model_drag_moved(&mut self, at: Instant, dx: f32, dy: f32) {
        if self.live_model.active && self.live_model_ready() {
            self.live_model.drag.push(at, dx, dy);
        }
    }

    /// The velocity to throw the model with, for a drag released at `now`, if it was a flick
    /// (a model that was turning, and a pointer still moving when it let go). Consumes the
    /// drag's samples either way.
    pub(crate) fn live_model_throw(&mut self, now: Instant) -> Option<[f32; 2]> {
        let v = self.live_model.drag.throw(now);
        self.live_model.drag.clear();
        (self.live_model.active && self.live_model_ready())
            .then_some(v)
            .flatten()
    }

    /// The on-screen model's camera height, while one is ready: how a glide spots a pole.
    pub(crate) fn live_model_elevation(&self) -> Option<f32> {
        self.live_model
            .ready_for(self.displayed_item?, self.content_gen)
            .map(|r| r.view.elevation())
    }

    /// The frame to draw this time, while the live view is up for the item on screen.
    /// Recomputed each draw, so a resize or a new top inset re-frames it for free.
    pub(crate) fn live_model_frame(&self) -> Option<pb_render::MeshFrame> {
        if !self.live_model.active {
            return None;
        }
        let ready = self
            .live_model
            .ready_for(self.displayed_item?, self.content_gen)?;
        let f = ready.view.frame(self.live_viewport()?);
        Some(pb_render::MeshFrame {
            view_proj: f.view_proj,
            eye: f.eye,
            key_dir: f.key_dir,
            fill_dir: f.fill_dir,
            lighting: f.lighting,
            base_color: self.settings.model_color,
        })
    }
}

/// Whether `item` of `source` is a 3D model: an image item with a model extension.
fn is_model_item(source: &dyn ItemSource, item: usize) -> bool {
    matches!(
        crate::video::item_kind(source, item),
        crate::video::LibraryItemKind::Image
    ) && live_model::model_format(source.name(item)).is_some()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use crate::app_core_impl::test_support::{make_resident, photos_named, rgba_full, test_core};
    use crate::decode_pool::Outcome;

    use super::*;

    /// What the renderer double saw.
    #[derive(Default)]
    struct Seen {
        /// `Some(n)` = `n` vertices resident; `None` = freed / never uploaded.
        resident: Option<usize>,
        /// The last `show_mesh` argument was `Some`.
        showing: bool,
        draws: usize,
    }

    /// A renderer double that records the live-view calls; an 800×600 surface.
    struct MeshRecorder(Arc<Mutex<Seen>>);

    impl pb_render::Renderer for MeshRecorder {
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
            self.0.lock().unwrap().showing = false; // like the real renderer
            true
        }
        fn surface_size(&self) -> (u32, u32) {
            (800, 600)
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
            self.0.lock().unwrap().draws += 1;
            Ok(true)
        }
        fn set_mesh(&mut self, vertices: Option<&[pb_render::MeshVertex]>) -> bool {
            let mut s = self.0.lock().unwrap();
            s.resident = vertices.map(<[_]>::len);
            if vertices.is_none() {
                s.showing = false;
            }
            true
        }
        fn show_mesh(&mut self, frame: Option<pb_render::MeshFrame>) {
            self.0.lock().unwrap().showing = frame.is_some();
        }
    }

    /// An in-memory source: a one-triangle STL, a photo, and a second STL.
    struct Mem;

    const TRIANGLE: &[u8] = b"solid t\nfacet normal 0 0 1\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 1\nendloop\nendfacet\nendsolid t\n";
    const NAMES: [&str; 3] = ["part.stl", "photo.jpg", "other.stl"];

    impl ItemSource for Mem {
        fn len(&self) -> usize {
            NAMES.len()
        }
        fn name(&self, i: usize) -> &str {
            NAMES.get(i).copied().unwrap_or("")
        }
        fn bytes(&self, i: usize) -> std::io::Result<Vec<u8>> {
            match i {
                0 | 2 => Ok(TRIANGLE.to_vec()),
                1 => Ok(vec![0xFF, 0xD8]),
                _ => Err(std::io::Error::new(std::io::ErrorKind::NotFound, "oob")),
            }
        }
    }

    fn core_on(item: usize) -> (AppCore, Arc<Mutex<Seen>>) {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let mut core = test_core();
        core.source = Arc::new(Mem);
        core.renderer = Some(Box::new(MeshRecorder(Arc::clone(&seen))));
        core.displayed_item = Some(item);
        core.presented_epoch = Some(core.epoch);
        (core, seen)
    }

    /// Tick until the preparation lands (it runs on a real thread).
    fn settle(core: &mut AppCore) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            core.tick_live_model(false);
            if core.live_model.loading.is_none() {
                return;
            }
            assert!(Instant::now() < deadline, "preparation never landed");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn settling_on_a_model_prepares_and_uploads_it_before_any_input() {
        let (mut core, seen) = core_on(0);
        settle(&mut core);
        assert!(core.live_model_ready());
        assert_eq!(
            seen.lock().unwrap().resident,
            Some(3),
            "one triangle = 3 corners"
        );
        assert!(
            !core.live_model.active,
            "the still keeps showing until the user turns it"
        );
        assert!(!seen.lock().unwrap().showing);
    }

    #[test]
    fn blazing_past_a_model_does_not_prepare_it() {
        let (mut core, _) = core_on(0);
        core.tick_live_model(true);
        assert!(core.live_model.loading.is_none() && core.live_model.ready.is_none());
    }

    #[test]
    fn photos_are_never_prepared() {
        let (mut core, _) = core_on(1);
        core.tick_live_model(false);
        assert!(core.live_model.loading.is_none());
    }

    #[test]
    fn a_drag_turns_the_model_and_shows_the_live_view() {
        let (mut core, seen) = core_on(0);
        settle(&mut core);
        core.pan_by_pixels(40.0, 10.0);
        assert!(core.live_model.active);
        assert!(seen.lock().unwrap().showing, "the draw pushed a frame");
        let ready = core.live_model.ready.as_ref().unwrap();
        assert!(!ready.view.is_home());
        assert_eq!(core.view.pan, [0.0, 0.0], "the photo view never panned");
    }

    /// #153.6, owner decisions: Export / Copy / Describe of a turned model capture the window
    /// as shown (its shape, the user's turn, zoom and pan), at its real pixels, unrotated by
    /// `R` (which turns the still only). Before the first turn and after Fit, the still.
    #[test]
    fn a_turned_model_is_captured_as_the_view_on_screen() {
        use crate::frame_capture::{FrameKey, FrameSource};
        let (mut core, _) = core_on(0);
        settle(&mut core);
        assert!(core.frame_target(0).is_still(), "not turned yet: the still");
        core.rotations.insert(0, pb_render::Rotation::R90);
        core.pan_by_pixels(40.0, 10.0);

        let frame = core.frame_target(0);
        let FrameSource::ModelView { width, height, .. } = frame else {
            panic!("the turned view, got {frame:?}");
        };
        assert_eq!(
            (width, height),
            (800, 600),
            "the viewport at its real pixels"
        );
        assert_eq!(frame.name_suffix(), "-view");
        assert_eq!(
            core.capture_rotation(0, &frame),
            pb_render::Rotation::default()
        );
        let key = core.frame_key(0);
        assert!(matches!(key, FrameKey::View { .. }));
        core.pan_by_pixels(5.0, 0.0);
        assert_ne!(core.frame_key(0), key, "another turn is another view");

        let cancel = std::sync::atomic::AtomicBool::new(false);
        let img = crate::frame_capture::capture(core.source.as_ref(), 0, &frame, &cancel).unwrap();
        assert_eq!((img.width, img.height), (800, 600));
        assert!(
            img.pixels.chunks_exact(4).any(|p| p[3] == 255),
            "the model is in it"
        );
        assert_eq!(img.pixels[3], 0, "on a transparent background");

        core.live_model_reset(); // Fit: back to the still
        assert!(core.frame_target(0).is_still());
    }

    #[test]
    fn the_export_panel_offers_the_view_at_the_windows_size() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        core.pan_by_pixels(40.0, 10.0);
        core.now += Duration::from_secs(1); // held still
        let panel = core.export_panel();
        let source = panel.source.as_ref().expect("a source");
        assert_eq!(source.dims, Some((800, 600)));
        assert_eq!(source.frame.as_deref(), Some("The view on screen"));
        // The same words while it moves (a drag, or the turntable on P): a longer line there
        // made the panel jump each time the model started or stopped turning.
        core.toggle_play_pause();
        assert!(core.frame_is_moving(0), "the turntable is turning it");
        let panel = core.export_panel();
        assert_eq!(
            panel.source.as_ref().and_then(|s| s.frame.as_deref()),
            Some("The view on screen")
        );
    }

    /// A drag changes the view every frame; the automatic text scan and describe wait until
    /// it holds still, and the pump stays awake until then so they start without new input.
    #[test]
    fn the_turned_view_counts_as_moving_until_it_settles() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        core.pan_by_pixels(40.0, 10.0);
        assert!(core.frame_is_moving(0) && core.live_view_settling());
        assert!(core.work_pending(), "keeps ticking until it settles");
        core.now += Duration::from_millis(300);
        assert!(!core.frame_is_moving(0) && !core.live_view_settling());
        core.toggle_play_pause(); // the turntable
        assert!(core.frame_is_moving(0), "a spinning model is moving");
        assert!(!core.frame_is_moving(1), "only the item on screen");
    }

    #[test]
    fn zoom_keys_and_menu_zoom_the_model_not_the_photo() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        core.zoom_step(1.25);
        assert!(core.live_model.active);
        assert_eq!(core.live_model.ready.as_ref().unwrap().view.zoom(), 1.25);
        assert_eq!(core.view.zoom, 1.0);
    }

    #[test]
    fn fit_puts_the_still_back() {
        let (mut core, seen) = core_on(0);
        settle(&mut core);
        core.pan_by_pixels(40.0, 0.0);
        core.set_scale_mode(pb_render::ScaleMode::Fit);
        assert!(!core.live_model.active);
        assert!(!seen.lock().unwrap().showing);
        assert!(core.live_model.ready.as_ref().unwrap().view.is_home());
        assert!(core.live_model_ready(), "still ready to turn again");
    }

    #[test]
    fn navigating_away_stops_drawing_and_frees_the_mesh() {
        let (mut core, seen) = core_on(0);
        settle(&mut core);
        core.pan_by_pixels(40.0, 0.0);
        // The next photo is presented: the renderer stops drawing the mesh, and the core's
        // draw must not put it back for an item that is no longer on screen.
        core.displayed_item = Some(1);
        seen.lock().unwrap().showing = false;
        core.draw();
        assert!(!seen.lock().unwrap().showing);
        core.tick_live_model(false);
        assert_eq!(seen.lock().unwrap().resident, None, "VRAM freed");
        assert!(core.live_model.ready.is_none() && !core.live_model.active);
    }

    #[test]
    fn moving_to_another_model_prepares_that_one() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        core.displayed_item = Some(2);
        settle(&mut core);
        assert_eq!(core.live_model.ready.as_ref().map(|r| r.item), Some(2));
    }

    #[test]
    fn a_new_deck_is_a_different_item_even_at_the_same_index() {
        let (mut core, seen) = core_on(0);
        settle(&mut core);
        core.content_gen += 1;
        core.tick_live_model(true); // blazing: tears down, doesn't re-prepare
        assert!(core.live_model.ready.is_none());
        assert_eq!(seen.lock().unwrap().resident, None);
    }

    /// Run the turntable's ticks for `secs` at 120 Hz from `t`.
    fn spin_for(core: &mut AppCore, t: &mut Instant, secs: f32) {
        let end = *t + Duration::from_secs_f32(secs);
        while *t < end {
            *t += Duration::from_millis(8);
            core.spin_live_model(*t);
        }
    }

    fn azimuth_turned(core: &AppCore) -> bool {
        !core.live_model.ready.as_ref().unwrap().view.is_home()
    }

    #[test]
    fn p_spins_a_ready_model_and_p_again_coasts_it_to_a_stop() {
        let (mut core, seen) = core_on(0);
        settle(&mut core);
        core.toggle_play_pause();
        assert!(
            core.live_model.active,
            "the live view is up from the keypress"
        );
        assert!(seen.lock().unwrap().showing);
        assert!(core.live_model_spinning() && core.motion_playing());
        let mut t = Instant::now();
        spin_for(&mut core, &mut t, 1.0);
        assert!(azimuth_turned(&core));
        core.toggle_play_pause();
        assert!(!core.live_model_spinning(), "coasting");
        spin_for(&mut core, &mut t, 1.0);
        assert!(core.live_model.spin.is_none(), "came to rest");
        assert!(core.live_model.active, "and stays where it stopped, live");
    }

    #[test]
    fn p_before_the_model_is_ready_spins_it_as_it_lands() {
        let (mut core, _) = core_on(0);
        core.toggle_play_pause(); // nothing prepared yet: not even started
        assert_eq!(core.live_model.spin_wanted, Some((0, core.content_gen)));
        settle(&mut core);
        assert!(core.live_model.active && core.live_model_spinning());
    }

    #[test]
    fn a_wanted_spin_is_forgotten_when_the_user_moves_on() {
        let (mut core, _) = core_on(0);
        core.toggle_play_pause();
        core.displayed_item = Some(2);
        settle(&mut core);
        assert!(core.live_model.spin_wanted.is_none());
        assert!(core.live_model.spin.is_none() && !core.live_model.active);
    }

    #[test]
    fn a_drag_takes_the_model_off_the_turntable_but_zoom_does_not() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        core.toggle_play_pause();
        core.zoom_step(1.25);
        assert!(core.live_model_spinning(), "zooming in on a spinning model");
        core.pan_by_pixels(30.0, 0.0);
        assert!(core.live_model.spin.is_none());
        assert!(core.live_model.active);
    }

    #[test]
    fn fit_stops_the_spin_and_puts_the_still_back() {
        let (mut core, seen) = core_on(0);
        settle(&mut core);
        core.toggle_play_pause();
        let mut t = Instant::now();
        spin_for(&mut core, &mut t, 0.5);
        core.set_scale_mode(pb_render::ScaleMode::Fit);
        assert!(core.live_model.spin.is_none() && !core.live_model.active);
        assert!(!seen.lock().unwrap().showing);
        assert!(!core.spin_live_model(t), "no more ticks to drive");
    }

    #[test]
    fn the_spin_hint_flashes_once_the_model_is_ready_and_retires_when_used() {
        let (mut core, _) = core_on(0);
        let seq = core.play_hint_seq;
        core.maybe_show_anim_hint(false);
        assert_eq!(core.play_hint_seq, seq, "not before it can be turned");
        assert_eq!(core.play_hint_kind(), 0);
        settle(&mut core);
        assert!(
            core.current_has_motion(),
            "the toolbar's Play button lights up"
        );
        core.maybe_show_anim_hint(false);
        assert_eq!(core.play_hint_seq, seq + 1);
        assert_eq!(core.play_hint_kind(), 4);
        core.maybe_show_anim_hint(false);
        assert_eq!(core.play_hint_seq, seq + 1, "once per item");
        core.pan_by_pixels(10.0, 0.0);
        assert_eq!(core.play_hint_kind(), 0, "turned by hand: the pill goes");
    }

    #[test]
    fn a_hand_turned_model_never_flashes_the_hint_later() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        core.pan_by_pixels(10.0, 0.0); // engaged before the tick got to the hint
        let seq = core.play_hint_seq;
        core.set_scale_mode(pb_render::ScaleMode::Fit);
        core.maybe_show_anim_hint(false);
        assert_eq!(core.play_hint_seq, seq);
    }

    /// Press, drag at `px` per 8 ms frame for `frames` frames, and release `rest` after the last
    /// move. Returns the release time.
    fn flick(core: &mut AppCore, px: f32, frames: u32, rest: Duration) -> Instant {
        let mut t = Instant::now();
        core.set_dragging_at(true, t);
        for _ in 0..frames {
            t += Duration::from_millis(8);
            core.pan_by_pixels(px, 0.0);
            core.live_model_drag_moved(t, px, 0.0);
        }
        let up = t + rest;
        core.set_dragging_at(false, up);
        up
    }

    /// Glide ticks every 8 ms from `t` for `secs`; returns how many ticks were still gliding.
    fn glide_for(core: &mut AppCore, t: &mut Instant, secs: f32) -> usize {
        let end = *t + Duration::from_secs_f32(secs);
        let mut live = 0;
        while *t < end {
            *t += Duration::from_millis(8);
            live += usize::from(core.apply_pan_inertia(*t));
        }
        live
    }

    #[test]
    fn a_flicked_model_keeps_turning_and_slows_to_a_stop() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        let mut t = flick(&mut core, 16.0, 20, Duration::from_millis(4));
        let az = |c: &AppCore| c.live_model.ready.as_ref().unwrap().view;
        let at_release = az(&core);
        assert!(core.pan_inertia.is_some(), "the flick threw it");
        glide_for(&mut core, &mut t, 0.1);
        assert_ne!(
            az(&core),
            at_release,
            "still turning after the button came up"
        );
        glide_for(&mut core, &mut t, 4.0);
        assert!(core.pan_inertia.is_none(), "and it came to rest");
    }

    #[test]
    fn a_drag_that_rests_before_letting_go_stays_put() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        flick(&mut core, 16.0, 20, Duration::from_millis(200));
        assert!(core.pan_inertia.is_none());
    }

    #[test]
    fn a_photo_is_never_thrown_by_the_mouse() {
        let (mut core, _) = core_on(1);
        flick(&mut core, 16.0, 20, Duration::from_millis(4));
        assert!(
            core.pan_inertia.is_none(),
            "a mouse drag on a photo stops dead"
        );
    }

    #[test]
    fn a_touch_fling_turns_a_model_instead_of_dying_on_the_first_tick() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        core.pan_by_pixels(1.0, 0.0); // the live view is up
        core.fling_pan(2000.0, 0.0);
        let mut t = Instant::now();
        assert!(glide_for(&mut core, &mut t, 0.3) > 20);
        assert!(core.pan_inertia.is_some());
    }

    #[test]
    fn a_vertical_glide_stops_at_the_pole() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        core.pan_by_pixels(0.0, 1.0);
        core.fling_pan(0.0, 5000.0);
        let mut t = Instant::now();
        // Without the pole test it would grind on invisibly for about two seconds.
        assert!(glide_for(&mut core, &mut t, 2.0) < 30);
        assert!(core.pan_inertia.is_none());
    }

    /// Swap in new settings and apply the model look, as `apply_settings` does (without its
    /// write to the real config file).
    fn change_settings(core: &mut AppCore, f: impl FnOnce(&mut crate::settings::Settings)) {
        let mut new = core.settings.clone();
        f(&mut new);
        let old = std::mem::replace(&mut core.settings, new);
        core.apply_model_look(&old);
    }

    #[test]
    fn a_new_model_colour_keeps_the_live_view_and_re_renders_the_stills() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        core.toggle_play_pause();
        let gen = core.content_gen;
        change_settings(&mut core, |s| s.model_color = [200, 40, 40]);
        assert_eq!(core.content_gen, gen, "a colour is not a new deck");
        assert!(
            core.live_model.stills_look > 0,
            "the baked stills are stale"
        );
        assert!(core.live_model_ready(), "the mesh is untouched");
        assert!(core.live_model.active && core.live_model_spinning());
        let frame = core.live_model_frame().expect("still drawing");
        assert_eq!(
            frame.base_color,
            [200, 40, 40],
            "the GPU draws the new colour"
        );
        // That new stills use it too isn't asserted here: the colour pb-decode bakes with is
        // process-wide, and every parallel test that runs `refresh_theme` resets it to the
        // default, so reading it back raced (it failed under full-suite load, 2026-11-25).
        // pb-decode's `the_model_colour_setting_reaches_the_still` covers that half.
        core.tick_live_model(false);
        assert!(
            core.live_model_ready(),
            "and the next tick doesn't tear it down"
        );
        pb_decode::set_model_color(pb_mesh::DEFAULT_COLOR); // process-wide: put it back
    }

    #[test]
    fn a_deck_of_photos_pays_nothing_for_a_new_model_colour() {
        let mut core = test_core();
        core.source = Arc::new(Photos);
        core.recolor_models();
        assert_eq!(core.live_model.stills_look, 0, "nothing to redraw");
        assert!(core.pool.enqueued().is_empty());
    }

    /// A deck of models, a photo and a video, parked on the first model (`a.stl`) in Fit mode
    /// with everything resident: the photo in both representations.
    fn mixed_deck() -> AppCore {
        let mut core = test_core();
        core.source = photos_named(&["a.stl", "b.jpg", "c.stl", "d.mp4", "e.stl", "f.jpg"]);
        core.playlist = Playlist::new(6, 0).with_cursor(0);
        core.ring = ResidentRing::new(8);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        core.view.mode = ScaleMode::Fit;
        core.targets = vec![0, 1, 2, 3, 4, 5];
        let fit = core.rep_of(pb_core::RepKind::Fit);
        let keep = core.targets.clone();
        for item in 0..4 {
            make_resident(&mut core, item, fit, &keep);
        }
        make_resident(&mut core, 1, pb_core::Representation::Original, &keep);
        core.target_item = Some(0);
        core.mark_resolved(0);
        core.ring.set_displayed(core.ring.slot_for(0).unwrap());
        core
    }

    fn landed(core: &AppCore, item: usize, rk: pb_core::RepKind) -> Outcome {
        let img = rgba_full(100, 100, 100, 100);
        Outcome::synthetic(item, core.epoch, core.content_gen, rk, Ok(img))
    }

    /// The fix for "a new colour re-decodes every cached picture in the folder": only the models'
    /// stills are drawn again. The photo keeps both its textures, the video keeps its poster and
    /// the deck keeps its generation (so poster choices and the strip survive).
    #[test]
    fn a_new_model_colour_redraws_only_the_models() {
        let mut core = mixed_deck();
        let (epoch, gen) = (core.epoch, core.content_gen);
        let on_screen = core.ring.slot_for(0).unwrap();
        core.pending_uploads
            .push(landed(&core, 4, pb_core::RepKind::Fit)); // a model, decoded in the old colour
        core.pending_uploads
            .push(landed(&core, 5, pb_core::RepKind::Fit)); // a photo
        let tile = |rgba| crate::thumbs::ThumbPixels {
            rgba,
            orig_w: 1,
            orig_h: 1,
            codec: "t",
        };
        let demand = pb_core::ThumbDemand::centered(0, 8);
        for item in [0, 1] {
            let px = tile(vec![0; 4]);
            core.thumbs
                .cache
                .insert(item, pb_core::ThumbTier::Full, 1, 1, 4, px, &demand);
        }

        change_settings(&mut core, |s| s.model_color = [11, 22, 33]);

        assert_eq!(
            (core.epoch, core.content_gen),
            (epoch, gen),
            "no reset of any kind"
        );
        assert_eq!(
            core.ring.slot_for(0),
            Some(on_screen),
            "the model on screen stays up"
        );
        assert!(
            core.preview_resident.contains(&0),
            "until its redraw replaces it in place"
        );
        assert!(
            core.ring.slot_for(2).is_none(),
            "the model off screen is dropped"
        );
        assert!(
            core.ring.slot_for(1).is_some(),
            "the photo keeps its picture"
        );
        assert!(
            core.ring.original_slot(1).is_some(),
            "and its full-size original"
        );
        assert!(
            core.ring.slot_for(3).is_some(),
            "the video keeps its poster"
        );
        let staged: Vec<usize> = core.pending_uploads.iter().map(|o| o.key.item).collect();
        assert_eq!(
            staged,
            vec![5],
            "the old-colour model still waiting to upload is gone"
        );
        let mut asked: Vec<usize> = core
            .pool
            .enqueued()
            .into_iter()
            .filter(|&(_, p, rk)| {
                p == crate::decode_pool::Purpose::Display && rk == pb_core::RepKind::Fit
            })
            .map(|(item, _, _)| item)
            .collect();
        asked.sort();
        assert_eq!(asked, vec![0, 2, 4], "exactly the models are drawn again");
        assert!(core.thumbs.cache.is_stale(0) && !core.thumbs.cache.is_stale(1));
        pb_decode::set_model_color(pb_mesh::DEFAULT_COLOR); // process-wide: put it back
    }

    /// A model decode under way when the colour changed lands afterwards in the old colour: it
    /// is dropped, and the photo that arrives with it is not.
    #[test]
    fn a_model_still_drawn_in_the_old_colour_is_dropped_on_arrival() {
        let mut core = mixed_deck();
        core.ring = ResidentRing::new(8);
        core.preview_resident.clear();
        let (old_model, photo) = (
            landed(&core, 2, pb_core::RepKind::Fit),
            landed(&core, 1, pb_core::RepKind::Fit),
        );
        change_settings(&mut core, |s| s.model_color = [12, 34, 56]);
        let tx = core.pool.test_sender();
        tx.send(old_model).unwrap();
        tx.send(photo).unwrap();

        core.drain_results();

        assert!(core.ring.slot_for(1).is_some(), "the photo lands");
        assert!(
            core.ring.slot_for(2).is_none(),
            "the old-colour model does not"
        );
        tx.send(landed(&core, 2, pb_core::RepKind::Fit)).unwrap();
        core.drain_results();
        assert!(
            core.ring.slot_for(2).is_some(),
            "its redraw in the new colour does"
        );
        pb_decode::set_model_color(pb_mesh::DEFAULT_COLOR);
    }

    /// While the live view is up the new colour is already on screen, so the still redrawing
    /// behind it is nothing to wait for: no loading pie.
    #[test]
    fn the_live_view_shows_no_pie_for_the_still_behind_it() {
        let (mut core, _) = core_on(0);
        core.ring = ResidentRing::new(4);
        core.fit = Some(FitBox {
            max_width: 100,
            max_height: 100,
        });
        let fit = core.rep_of(pb_core::RepKind::Fit);
        make_resident(&mut core, 0, fit, &[0]);
        settle(&mut core);
        assert!(
            core.live_model_orbit(5.0, 0.0),
            "turned: the live view is up"
        );
        change_settings(&mut core, |s| s.model_color = [14, 15, 16]);
        assert_eq!(
            core.sharpen_now(),
            Some(0),
            "the still behind it is being redrawn"
        );
        let now = Instant::now() + Duration::from_secs(5);
        assert!(!core.tick_pie(now), "but nothing is shown waiting on it");
        core.live_model_reset(); // Fit: back to the still
        assert!(
            core.tick_pie(now),
            "the still itself does wait for its redraw"
        );
        pb_decode::set_model_color(pb_mesh::DEFAULT_COLOR);
    }

    /// The redraw of the model on screen replaces it in the same slot, in Fit mode and at 100%
    /// (where a model still is an Original, which otherwise never replaces itself).
    #[test]
    fn the_model_on_screen_is_redrawn_in_place() {
        for mode in [ScaleMode::Fit, ScaleMode::Original] {
            let mut core = mixed_deck();
            let rk = if mode == ScaleMode::Fit {
                pb_core::RepKind::Fit
            } else {
                core.view.mode = mode;
                core.ring = ResidentRing::new(8);
                make_resident(&mut core, 0, pb_core::Representation::Original, &[0]);
                core.ring.set_displayed(core.ring.original_slot(0).unwrap());
                pb_core::RepKind::Original
            };
            let slot = core.ring.slot_for_rep(0, rk).unwrap();
            change_settings(&mut core, |s| s.model_color = [13, 57, 91]);
            core.pool.test_sender().send(landed(&core, 0, rk)).unwrap();

            core.drain_results();

            assert_eq!(
                core.ring.slot_for_rep(0, rk),
                Some(slot),
                "{mode:?}: same slot"
            );
            assert!(
                !core.preview_resident.contains(&0),
                "{mode:?}: the redraw landed"
            );
            pb_decode::set_model_color(pb_mesh::DEFAULT_COLOR);
        }
    }

    #[test]
    fn a_new_spin_speed_reaches_a_spinning_model() {
        let (mut core, _) = core_on(0);
        settle(&mut core);
        core.toggle_play_pause();
        let mut t = Instant::now();
        spin_for(&mut core, &mut t, 1.0);
        change_settings(&mut core, |s| s.model_spin_rpm = 10.0);
        spin_for(&mut core, &mut t, 2.0); // eases up to the new speed
                                          // Cruising at 10 rpm, a second turns 60°.
        let mut probe = core.live_model.spin.unwrap();
        let turned: f32 = (1..=125)
            .map(|k| probe.advance(t + Duration::from_millis(8 * k)))
            .sum();
        assert!(
            (turned - live_model::spin_speed(10.0)).abs() < 0.02,
            "{turned}"
        );
    }

    /// Only photos.
    struct Photos;

    impl ItemSource for Photos {
        fn len(&self) -> usize {
            3
        }
        fn name(&self, _: usize) -> &str {
            "photo.jpg"
        }
        fn bytes(&self, _: usize) -> std::io::Result<Vec<u8>> {
            Ok(vec![0xFF, 0xD8])
        }
    }

    #[test]
    fn p_on_a_photo_is_not_the_turntable() {
        let (mut core, _) = core_on(1);
        assert!(!core.live_model_toggle_spin());
        assert!(core.live_model.spin_wanted.is_none());
    }

    #[test]
    fn input_before_the_model_is_ready_behaves_like_a_photo() {
        let (mut core, _) = core_on(0);
        core.zoom_step(1.25);
        assert_eq!(core.view.zoom, 1.25);
        assert!(!core.live_model.active);
    }
}
