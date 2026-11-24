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

    /// Whether `item` is a 3D model: an image item with a model extension.
    fn item_is_model(&self, item: usize) -> bool {
        matches!(
            crate::video::item_kind(self.source.as_ref(), item),
            crate::video::LibraryItemKind::Image
        ) && live_model::model_format(self.source.name(item)).is_some()
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
            self.live_model.spin = Some(Spin::start());
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
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use crate::app_core_impl::test_support::test_core;

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
