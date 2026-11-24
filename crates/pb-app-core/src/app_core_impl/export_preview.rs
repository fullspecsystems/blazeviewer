//! **Export mode's live preview** — the `AppCore` half (task #89, rung 3). The subsystem is
//! [`crate::export_preview`] (identity, the spring, the worker). Paired per the two-halves
//! rule.
//!
//! Each tick works out what the screen should preview ([`TickKey`], cheap, no allocation);
//! once that has held still for [`PREVIEW_DEBOUNCE`] it asks the worker, latest-wins. A
//! finished preview lands in the renderer's two aux textures (0 = the original, 1 = the
//! export), and [`AppCore::draw`] presents the side the spring shows **over** whatever the
//! viewer presented — so every one of the viewer's own re-present paths (a sharpen landing, a
//! resize, a rebind) can carry on unchanged, and the next draw simply covers it again (the
//! present gate, review F3, as one check at the single render point). Navigating, playing,
//! turning a model or leaving the mode uncovers the photo.

use super::*;
use crate::export_preview::{
    size_label, ExportShown, OriginalShown, PreviewKey, PreviewRequest, PreviewResult,
    PreviewState, PreviewStatus, PreviewWorker, Side, SourceKey, SpringOwner, TickKey,
    AUX_EXPORT, AUX_ORIGINAL, PREVIEW_DEBOUNCE, PREVIEW_MAX_PIXELS, PREVIEW_POLL,
};
use crate::frame_capture::FrameKey;

/// [`PreviewStatus`] borrowed — so the per-tick change key never allocates.
enum StatusRef<'a> {
    Off,
    Working,
    Ready(u64),
    Failed(&'a str),
    TooLarge,
}

impl AppCore {
    /// The screen's preview key now, or `None` when nothing should be previewed: the mode
    /// isn't showing, the frame is moving (playing, a model being turned), a door, or a
    /// source too big to preview. Cheap and allocation-free — it runs every tick and draw.
    pub(crate) fn preview_tick_key(&self) -> Option<TickKey> {
        if !self.export_mode_active() {
            return None;
        }
        let item = self.displayed_item?;
        if self.target_item.is_some_and(|t| t != item) {
            return None; // navigating: the screen is about to change
        }
        match crate::video::item_kind(self.source.as_ref(), item) {
            crate::video::LibraryItemKind::Image => {}
            crate::video::LibraryItemKind::Video(_) if crate::frame_capture::CAN_CAPTURE_VIDEO => {}
            _ => return None, // a door, or a video this build can't capture
        }
        if self.frame_is_moving(item) {
            return None;
        }
        let (w, h) = self.export_source_dims(item)?;
        if w as u64 * h as u64 > PREVIEW_MAX_PIXELS {
            return None;
        }
        let frame = self.frame_key(item);
        let view_cw = self
            .rotations
            .get(&item)
            .copied()
            .map(crate::export::quarter_turns)
            .unwrap_or(0);
        // A photo's file carries the rotation; a turned model's view ignores R (its capture
        // is the window as shown) while the view transform still applies it.
        let (rotation_cw, display_turns) = match frame {
            FrameKey::View { .. } => (0, view_cw),
            _ => (view_cw, 0),
        };
        Some(TickKey {
            content_gen: self.content_gen,
            item,
            frame,
            rotation_cw,
            display_turns,
            settings: self.export.draft,
        })
    }

    /// The aux slot [`draw`](Self::draw) should present, laid out at the source's size —
    /// `None` shows the viewer's own presentation.
    pub(crate) fn export_preview_target(&self) -> Option<(usize, u32, u32)> {
        let cur = self.preview_tick_key()?;
        let st = &self.export.preview;
        match st.spring.shown() {
            Side::Export => st
                .export
                .as_ref()
                .filter(|e| e.tick.same_source(&cur))
                .map(|e| (AUX_EXPORT, e.source_dims.0, e.source_dims.1)),
            Side::Original => st
                .original
                .as_ref()
                .filter(|o| o.tick.same_source(&cur))
                .map(|o| (AUX_ORIGINAL, o.dims.0, o.dims.1)),
        }
    }

    /// Per tick: debounce, ask the worker, land results, and redraw when what should be on
    /// screen changed. Tears everything down once the mode closes.
    pub(super) fn tick_export_preview(&mut self) {
        let now = self.now;
        if !self.panels.export {
            if !self.export.preview.is_idle() {
                self.end_export_preview();
            }
            return;
        }
        let tk = self.preview_tick_key();
        if tk != self.export.preview.seen {
            // Navigation, a rotation, a settings change, playing: show the export again.
            self.export.preview.spring.reset();
            self.export.preview.seen = tk;
            self.export.preview.seen_since = Some(now);
        }
        let settled = self
            .export
            .preview
            .seen_since
            .is_some_and(|t| now.saturating_duration_since(t) >= PREVIEW_DEBOUNCE);
        if let Some(tk) = tk.filter(|_| settled) {
            if self.export.preview.asked != Some(tk) {
                self.export.preview.asked = Some(tk);
                self.request_export_preview(tk);
            }
        }
        self.poll_export_preview(tk);

        let target = self.export_preview_target();
        if target != self.export.preview.last_target {
            self.draw(); // records `last_target`
        }
    }

    /// The loop's next wake for the preview: the debounce's end, or a short poll while one
    /// is being made.
    pub(super) fn export_preview_wake(&self, now: Instant) -> Option<Instant> {
        let st = &self.export.preview;
        if st.in_flight.is_some() {
            return Some(now + PREVIEW_POLL);
        }
        match (st.seen, st.seen_since) {
            (Some(tk), Some(since)) if st.asked != Some(tk) => {
                Some((since + PREVIEW_DEBOUNCE).max(now + Duration::from_millis(1)))
            }
            _ => None,
        }
    }

    /// Build the full key for the settled `tk` and ask the worker — unless the screen
    /// already has exactly that preview (or it's being made).
    fn request_export_preview(&mut self, tk: TickKey) {
        let item = tk.item;
        let (frame, _rotation) = self.export_frame(item);
        let key = PreviewKey {
            source: SourceKey {
                path: self.source.path(item).map(Path::to_path_buf),
                container: self.source.container().map(Path::to_path_buf),
                name: self.source.name(item).to_string(),
                frame: tk.frame,
                content_gen: tk.content_gen,
            },
            rotation_cw: tk.rotation_cw,
            settings: tk.settings,
        };
        let st = &mut self.export.preview;
        if st
            .export
            .as_ref()
            .is_some_and(|e| e.key == key && e.tick.same_source(&tk))
        {
            // Back to the settings already on screen (`[` then `]` within an encode): drop
            // whatever is being made for the ones just left, or it would land over this.
            if st.in_flight.take().is_some() {
                st.gen += 1;
                if let Some(w) = st.worker.as_ref() {
                    w.cancel(st.gen);
                }
            }
            self.emit_panels_changed();
            return;
        }
        let need_original = st
            .original
            .as_ref()
            .is_none_or(|o| o.key != key.source || !o.tick.same_source(&tk));
        st.gen += 1;
        let gen = st.gen;
        let worker = st.worker.get_or_insert_with(PreviewWorker::new);
        let submitted = worker.submit(PreviewRequest {
            gen,
            key,
            source: Arc::clone(&self.source),
            item,
            frame,
            need_original,
            display_turns: tk.display_turns,
            queued_at: Instant::now(),
        });
        st.in_flight = submitted.then_some((gen, tk));
        st.failed = None;
        self.emit_panels_changed();
    }

    /// Land finished previews: upload the sides that still match the screen.
    fn poll_export_preview(&mut self, current: Option<TickKey>) {
        let Some(worker) = self.export.preview.worker.as_ref() else {
            return;
        };
        let Some(done) = worker.drain() else {
            // The thread is gone (it shouldn't be — every stage is panic-wrapped): start
            // over on the next settle rather than waiting forever.
            let st = &mut self.export.preview;
            st.worker = None;
            if let Some((_, tk)) = st.in_flight.take() {
                st.failed = Some((tk, "the preview stopped unexpectedly".into()));
            }
            st.asked = None;
            self.emit_panels_changed();
            return;
        };
        for r in done {
            let Some((gen, tk)) = self.export.preview.in_flight else {
                continue;
            };
            if r.gen != gen {
                continue; // superseded
            }
            self.export.preview.in_flight = None;
            // The screen moved on while it was being made (the worker's cache keeps it):
            // forget it was asked for, so coming back asks again.
            if !current.is_some_and(|c| c.same_source(&tk)) {
                if self.export.preview.asked == Some(tk) {
                    self.export.preview.asked = None;
                }
                continue;
            }
            self.land_export_preview(r, tk);
        }
    }

    fn land_export_preview(&mut self, r: PreviewResult, tk: TickKey) {
        if self.perf.enabled() {
            // Durations only — never a path (privacy #2).
            let t = r.timings;
            let ms = |d: Duration| d.as_secs_f64() * 1e3;
            eprintln!(
                "[perf] export preview: queue {:.1}ms decode {:.1}ms original {:.1}ms prepare {:.1}ms encode {:.1}ms read-back {:.1}ms total {:.1}ms",
                ms(t.queue),
                ms(t.decode),
                ms(t.original),
                ms(t.prepare),
                ms(t.encode),
                ms(t.read_back),
                ms(t.total),
            );
        }
        if let Some(img) = r.original {
            let ok = self.renderer.as_mut().is_some_and(|rd| {
                rd.upload_aux(
                    AUX_ORIGINAL,
                    &img.pixels,
                    img.width,
                    img.height,
                    crate::engine::render_color(&img.color),
                )
            });
            self.export.preview.original = ok.then(|| OriginalShown {
                key: r.key.source.clone(),
                tick: tk,
                dims: (img.width, img.height),
            });
        }
        match r.export {
            Ok(e) => {
                let img = &e.image;
                let ok = self.renderer.as_mut().is_some_and(|rd| {
                    rd.upload_aux(
                        AUX_EXPORT,
                        &img.pixels,
                        img.width,
                        img.height,
                        crate::engine::render_color(&img.color),
                    )
                });
                self.export.preview.export = ok.then_some(ExportShown {
                    key: r.key,
                    tick: tk,
                    texture_dims: (img.width, img.height),
                    bytes: e.bytes,
                    notes: e.notes,
                    dims: e.dims,
                    source_dims: e.source_dims,
                });
                if !ok {
                    self.export.preview.failed =
                        Some((tk, "the preview couldn't be shown".into()));
                }
            }
            Err(why) => {
                eprintln!("export preview failed: {why}");
                self.export.preview.failed = Some((tk, why));
            }
        }
        self.sync_stash_external();
        self.emit_panels_changed();
        // A new texture behind the slot already on screen changes no target: draw it now.
        self.draw();
    }

    /// Leave the preview behind: uncover the photo, free both textures and the worker (its
    /// caches die with it), forget everything.
    fn end_export_preview(&mut self) {
        if let Some(r) = self.renderer.as_mut() {
            r.clear_aux(AUX_EXPORT);
            r.clear_aux(AUX_ORIGINAL);
        }
        self.export.preview = PreviewState::default();
        self.sync_stash_external();
        self.draw();
    }

    /// What the panel says about the preview of the item on screen.
    pub fn export_preview_status(&self) -> PreviewStatus {
        match self.preview_status_ref() {
            StatusRef::Off => PreviewStatus::Off,
            StatusRef::Working => PreviewStatus::Working,
            StatusRef::Ready(bytes) => PreviewStatus::Ready { bytes },
            StatusRef::Failed(why) => PreviewStatus::Failed(why.to_string()),
            StatusRef::TooLarge => PreviewStatus::TooLarge,
        }
    }

    /// [`export_preview_status`](Self::export_preview_status) as the panel's per-tick change
    /// key — allocation-free.
    pub(super) fn export_preview_status_key(&self) -> (u8, u64) {
        match self.preview_status_ref() {
            StatusRef::Off => (0, 0),
            StatusRef::Working => (1, 0),
            StatusRef::Ready(bytes) => (2, bytes),
            StatusRef::Failed(_) => (3, 0),
            StatusRef::TooLarge => (4, 0),
        }
    }

    fn preview_status_ref(&self) -> StatusRef<'_> {
        let Some(item) = self.displayed_item.filter(|_| self.export_mode_active()) else {
            return StatusRef::Off;
        };
        let Some(cur) = self.preview_tick_key() else {
            let too_large = self
                .export_source_dims(item)
                .is_some_and(|(w, h)| w as u64 * h as u64 > PREVIEW_MAX_PIXELS);
            return if too_large && !self.frame_is_moving(item) {
                StatusRef::TooLarge
            } else {
                StatusRef::Off
            };
        };
        let st = &self.export.preview;
        if let Some((tk, why)) = &st.failed {
            if *tk == cur {
                return StatusRef::Failed(why);
            }
        }
        match &st.export {
            Some(e)
                if e.tick.same_source(&cur)
                    && e.key.settings == cur.settings
                    && e.key.rotation_cw == cur.rotation_cw =>
            {
                StatusRef::Ready(e.bytes.len() as u64)
            }
            _ => StatusRef::Working,
        }
    }

    /// The side of the flick on screen.
    pub fn export_preview_side(&self) -> Side {
        self.export.preview.spring.shown()
    }

    /// The panel's Export / Original control (and the menu's Compare with Pinned in the mode).
    pub fn export_set_preview_side(&mut self, side: Side) {
        if self.export.preview.spring.set(side) {
            self.flicked();
        }
    }

    /// The menu's toggle in the mode (no hold to peek).
    pub(crate) fn export_flick_toggle(&mut self) {
        let side = self.export.preview.spring.shown().other();
        self.export_set_preview_side(side);
    }

    /// `Y` down in the mode: flick (a hold peeks).
    pub(super) fn export_flick_down(&mut self, key: crate::pb_key::PbKey) {
        if self
            .export
            .preview
            .spring
            .down(SpringOwner::Key(key), Instant::now())
        {
            self.flicked();
        }
    }

    /// A key-up: if it's the one holding the flick, a long hold flicks back.
    pub(super) fn export_flick_up(&mut self, key: crate::pb_key::PbKey) {
        if self
            .export
            .preview
            .spring
            .up(SpringOwner::Key(key), Instant::now())
        {
            self.flicked();
        }
    }

    /// Focus loss: a held flick never sticks.
    pub(super) fn export_flick_release_all(&mut self) {
        if self.export.preview.spring.reset() {
            self.flicked();
        }
    }

    /// The side changed: say which (the chip, F-design § Rung 3) and show it now — both
    /// sides are on the GPU, so this is a rebind.
    fn flicked(&mut self) {
        let msg = match self.export.preview.spring.shown() {
            Side::Original => "Original".to_string(),
            Side::Export => match self.export_preview_status() {
                PreviewStatus::Ready { bytes } => format!("Export ({})", size_label(bytes)),
                _ => "Export".to_string(),
            },
        };
        if self.export_preview_target().is_some() || self.export.preview.export.is_some() {
            self.show_toast(&msg);
        }
        self.emit_panels_changed();
        self.draw();
    }

    /// The encoded file on screen, when it is exactly what Export… with the current
    /// settings would write — so Export… writes it as is (F10).
    pub(super) fn export_preview_file(&self) -> Option<crate::export::PreEncoded> {
        let cur = self.preview_tick_key()?;
        let e = self.export.preview.export.as_ref()?;
        (e.tick.same_source(&cur)
            && e.key.settings == cur.settings
            && e.key.rotation_cw == cur.rotation_cw)
            .then(|| crate::export::PreEncoded {
                bytes: Arc::clone(&e.bytes),
                notes: e.notes.clone(),
            })
    }

    /// Notes the preview knows that the settings alone can't: HDR exported as SDR, and a
    /// resized export shown at the original's size.
    pub(super) fn export_preview_notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        let Some(cur) = self.preview_tick_key() else {
            return notes;
        };
        let Some(e) = self
            .export
            .preview
            .export
            .as_ref()
            .filter(|e| e.tick.same_source(&cur))
        else {
            return notes;
        };
        if e.notes.contains(&pb_encode::ExportNote::HdrToneMapped) {
            notes.push("This HDR image is exported as SDR (Display P3).".into());
        }
        let turned = e.key.rotation_cw % 2 == 1;
        let (sw, sh) = if turned {
            (e.source_dims.1, e.source_dims.0)
        } else {
            e.source_dims
        };
        if e.dims != (sw, sh) {
            notes.push(
                "The preview shows the export enlarged to the original's size, to compare \
                 them."
                    .into(),
            );
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{test_core, AuxLog, AuxOk};
    use crate::export_panel::FormatChoice;
    use crate::export_preview::Side;
    use std::sync::Mutex;

    fn sandbox(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pb-preview-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A small JPEG with some detail, so quality changes the file.
    fn write_jpeg(path: &Path, w: u32, h: u32) {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 7) as u8, (y * 5) as u8, ((x ^ y) * 3) as u8, 255]);
            }
        }
        std::fs::write(path, pb_encode::encode_jpeg_rgba8(&rgba, w, h, 95).unwrap()).unwrap();
    }

    fn dims(core: &mut AppCore, item: usize, w: u32, h: u32) {
        core.meta_cache.insert(
            item,
            crate::meta::PhotoMeta {
                rel: core.source.name(item).to_string(),
                w,
                h,
                size: None,
                codec: "JPEG",
                animated: None,
                recovered: None,
            },
        );
    }

    /// A core on two real JPEGs, in Export mode, with a renderer that records the aux slots.
    fn preview_core(tag: &str) -> (AppCore, Arc<Mutex<AuxLog>>, PathBuf) {
        let lib = sandbox(tag);
        write_jpeg(&lib.join("a.jpg"), 64, 48);
        write_jpeg(&lib.join("b.jpg"), 40, 30);
        let mut core = test_core();
        core.persist_prefs = false;
        core.source = Arc::new(pb_source::FsSource::new(vec![
            lib.join("a.jpg"),
            lib.join("b.jpg"),
        ]));
        core.displayed_item = Some(0);
        dims(&mut core, 0, 64, 48);
        dims(&mut core, 1, 40, 30);
        let log = Arc::new(Mutex::new(AuxLog::default()));
        core.renderer = Some(Box::new(AuxOk(Arc::clone(&log))));
        core.dispatch_action(Action::ExportMode);
        (core, log, lib)
    }

    /// Tick in real time until `done` (the worker is a real thread).
    fn tick_until(core: &mut AppCore, mut done: impl FnMut(&AppCore) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(core) {
            assert!(Instant::now() < deadline, "the preview never settled");
            core.now = Instant::now();
            core.tick();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn ready(core: &AppCore) -> bool {
        matches!(core.export_preview_status(), PreviewStatus::Ready { .. })
    }

    fn shown(log: &Arc<Mutex<AuxLog>>) -> Option<(usize, u32, u32)> {
        log.lock().unwrap().shown
    }

    #[test]
    fn the_mode_previews_the_export_once_it_settles() {
        let (mut core, log, lib) = preview_core("settle");
        assert_eq!(core.export_preview_status(), PreviewStatus::Working);
        assert!(shown(&log).is_none(), "nothing covers the photo yet");
        tick_until(&mut core, ready);
        let l = log.lock().unwrap();
        assert_eq!(l.uploaded, [Some((64, 48)), Some((64, 48))], "both sides, full size");
        assert_eq!(l.shown, Some((AUX_EXPORT, 64, 48)), "the export covers the photo");
        drop(l);
        let PreviewStatus::Ready { bytes } = core.export_preview_status() else {
            unreachable!()
        };
        let file = core.export.preview.export.as_ref().unwrap();
        assert_eq!(bytes, file.bytes.len() as u64, "the size is the file's, exactly");
        assert_eq!(&file.bytes[..8], b"\x89PNG\r\n\x1a\n", "PNG by default");
        // Only parked work: a viewer re-present is simply covered again on the next draw.
        core.renderer.as_mut().unwrap().present_slot(
            0,
            pb_core::SlotIdentity {
                item: 0,
                content_gen: 0,
                rep: pb_core::RepKind::Fit,
            },
        );
        assert!(shown(&log).is_none());
        core.draw();
        assert_eq!(shown(&log), Some((AUX_EXPORT, 64, 48)), "the present gate");
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn y_flicks_between_the_export_and_the_original() {
        let (mut core, log, lib) = preview_core("flick");
        tick_until(&mut core, ready);
        let y = crate::pb_key::PbKey::KeyY;
        core.handle(contract::CoreEvent::KeyDown {
            key: y,
            mods: contract::Modifiers::NONE,
            repeat: false,
        });
        assert_eq!(core.export_preview_side(), Side::Original);
        assert_eq!(shown(&log).map(|s| s.0), Some(AUX_ORIGINAL), "at once — a rebind");
        core.handle(contract::CoreEvent::KeyUp { key: y });
        assert_eq!(core.export_preview_side(), Side::Original, "a tap stays");
        assert!(core.compare_pin.is_none(), "Y doesn't pin in the mode");
        // The panel's control (and the menu) set it too.
        core.export_set_preview_side(Side::Export);
        assert_eq!(shown(&log).map(|s| s.0), Some(AUX_EXPORT));
        core.handle(contract::CoreEvent::MenuAction(Action::CompareToggle));
        assert_eq!(core.export_preview_side(), Side::Original, "the menu toggles");
        // A settings change shows the export again.
        core.export_set_format(FormatChoice::Jpeg);
        core.now = Instant::now();
        core.tick();
        assert_eq!(core.export_preview_side(), Side::Export);
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn a_settings_change_keeps_the_old_preview_until_the_new_one_lands() {
        let (mut core, log, lib) = preview_core("stale");
        tick_until(&mut core, ready);
        core.export_set_format(FormatChoice::Jpeg);
        core.now = Instant::now();
        core.tick();
        assert_eq!(core.export_preview_status(), PreviewStatus::Working);
        assert_eq!(
            shown(&log).map(|s| s.0),
            Some(AUX_EXPORT),
            "the last preview stays up meanwhile"
        );
        tick_until(&mut core, ready);
        let file = core.export.preview.export.as_ref().unwrap();
        assert_eq!(&file.bytes[..2], &[0xFF, 0xD8], "now a JPEG");
        let jpeg85 = file.bytes.len();
        core.export_set_jpeg_quality(30);
        tick_until(&mut core, |c| {
            matches!(c.export_preview_status(), PreviewStatus::Ready { bytes } if bytes as usize != jpeg85)
        });
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn navigating_uncovers_the_photo_and_previews_the_next() {
        let (mut core, log, lib) = preview_core("nav");
        tick_until(&mut core, ready);
        core.displayed_item = Some(1);
        core.now = Instant::now();
        core.tick();
        assert!(shown(&log).is_none(), "another photo's preview never shows");
        assert_eq!(core.export_preview_status(), PreviewStatus::Working);
        tick_until(&mut core, ready);
        assert_eq!(shown(&log), Some((AUX_EXPORT, 40, 30)));
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn tab_suspends_and_leaving_the_mode_frees_everything() {
        let (mut core, log, lib) = preview_core("leave");
        tick_until(&mut core, ready);
        core.dispatch_action(Action::TogglePanels);
        core.now = Instant::now();
        core.tick();
        assert!(shown(&log).is_none(), "Tab hides the preview with the panel");
        assert!(log.lock().unwrap().uploaded[AUX_EXPORT].is_some(), "but keeps it");
        core.dispatch_action(Action::TogglePanels);
        core.now = Instant::now();
        core.tick();
        assert_eq!(shown(&log).map(|s| s.0), Some(AUX_EXPORT), "back, no re-encode");
        core.dispatch_action(Action::ExportMode);
        core.now = Instant::now();
        core.tick();
        assert!(shown(&log).is_none());
        assert_eq!(log.lock().unwrap().uploaded, [None, None], "textures freed");
        assert!(core.export.preview.is_idle(), "worker and caches gone");
        assert_eq!(core.export.preview.vram_bytes(), 0);
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn export_writes_the_previewed_file_byte_for_byte() {
        let (mut core, _log, lib) = preview_core("commit");
        let out = sandbox("commit-out");
        core.export_set_format(FormatChoice::Jpeg);
        core.export_set_jpeg_quality(60);
        tick_until(&mut core, ready);
        let previewed = core.export.preview.export.as_ref().unwrap().bytes.clone();
        core.dispatch_action(Action::Export); // in the mode: export
        tick_until(&mut core, |c| {
            c.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ShowSavePanel { .. }))
        });
        let id = core
            .effects
            .iter()
            .find_map(|e| match e {
                contract::CoreEffect::ShowSavePanel { request_id, .. } => Some(*request_id),
                _ => None,
            })
            .unwrap();
        core.save_panel_resolved(id, Some(out.join("out.jpg")));
        tick_until(&mut core, |c| !c.export.busy());
        let written = std::fs::read(out.join("out.jpg")).unwrap();
        assert_eq!(written, *previewed, "the preview's bytes are the file's");
        let _ = std::fs::remove_dir_all(&lib);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Review finding 1: a recursive scan can hold two files of the same name. Each gets its
    /// own preview, and Export… never writes the other's bytes.
    #[test]
    fn two_files_with_one_name_never_share_a_preview() {
        let lib = sandbox("samename");
        std::fs::create_dir_all(lib.join("100")).unwrap();
        std::fs::create_dir_all(lib.join("101")).unwrap();
        write_jpeg(&lib.join("100/IMG_0001.JPG"), 64, 48);
        write_jpeg(&lib.join("101/IMG_0001.JPG"), 40, 30);
        let mut core = test_core();
        core.persist_prefs = false;
        core.source = Arc::new(pb_source::FsSource::new(vec![
            lib.join("100/IMG_0001.JPG"),
            lib.join("101/IMG_0001.JPG"),
        ]));
        assert_eq!(core.source.name(0), core.source.name(1), "the premise");
        core.displayed_item = Some(0);
        dims(&mut core, 0, 64, 48);
        dims(&mut core, 1, 40, 30);
        let log = Arc::new(Mutex::new(AuxLog::default()));
        core.renderer = Some(Box::new(AuxOk(Arc::clone(&log))));
        core.dispatch_action(Action::ExportMode);
        tick_until(&mut core, ready);
        let first = core.export.preview.export.as_ref().unwrap().bytes.clone();
        core.displayed_item = Some(1);
        tick_until(&mut core, ready);
        assert_eq!(shown(&log), Some((AUX_EXPORT, 40, 30)), "the second file's own preview");
        let second = core.export_preview_file().unwrap().bytes;
        assert_ne!(*second, *first, "and its own bytes for Export…");
        let _ = std::fs::remove_dir_all(&lib);
    }

    /// Review finding 2: `[` then `]` while the first encode runs — the preview for the
    /// quality just left must never land over the one on screen.
    #[test]
    fn going_back_to_the_settings_on_screen_cancels_the_encode_in_flight() {
        let (mut core, _log, lib) = preview_core("backforth");
        core.export_set_format(FormatChoice::Jpeg);
        tick_until(&mut core, ready);
        let q85 = core.export.preview.export.as_ref().unwrap().key.clone();
        let t = Instant::now();
        core.export_set_jpeg_quality(80);
        core.now = t;
        core.tick();
        core.now = t + PREVIEW_DEBOUNCE * 2;
        core.tick(); // settled: the 80 request goes out
        assert!(core.export.preview.in_flight.is_some());
        core.export_set_jpeg_quality(85);
        core.now = t + PREVIEW_DEBOUNCE * 3;
        core.tick();
        core.now = t + PREVIEW_DEBOUNCE * 5;
        core.tick(); // settled back on 85: already on screen
        assert!(core.export.preview.in_flight.is_none(), "the 80 encode is dropped");
        std::thread::sleep(Duration::from_millis(200)); // let any 80 result arrive
        core.now = t + PREVIEW_DEBOUNCE * 6;
        core.tick();
        assert_eq!(core.export.preview.export.as_ref().unwrap().key, q85);
        assert!(ready(&core), "85 is on screen and ready, not stuck calculating");
        let _ = std::fs::remove_dir_all(&lib);
    }

    /// Review finding 3: a preview that lands while you're on another photo is dropped — and
    /// asked for again when you come back.
    #[test]
    fn coming_back_asks_again_for_a_preview_dropped_while_away() {
        let (mut core, log, lib) = preview_core("away");
        let t = Instant::now();
        core.now = t;
        core.tick();
        core.now = t + PREVIEW_DEBOUNCE * 2;
        core.tick(); // item 0's request goes out
        assert!(core.export.preview.in_flight.is_some());
        core.displayed_item = Some(1);
        let away = t + PREVIEW_DEBOUNCE * 3;
        core.now = away;
        core.tick();
        // Item 1 never settles (the clock stands still) while item 0's result lands and is
        // dropped.
        let deadline = Instant::now() + Duration::from_secs(20);
        while core.export.preview.in_flight.is_some() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
            core.tick();
        }
        assert!(shown(&log).is_none());
        core.displayed_item = Some(0);
        tick_until(&mut core, ready);
        assert_eq!(shown(&log), Some((AUX_EXPORT, 64, 48)));
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn a_door_or_the_empty_deck_has_no_preview() {
        let mut core = test_core();
        core.source = crate::app_core_impl::test_support::photos_named(&["set.zip"]);
        core.displayed_item = Some(0);
        core.dispatch_action(Action::ExportMode);
        assert_eq!(core.export_preview_status(), PreviewStatus::Off);
        assert!(core.preview_tick_key().is_none());
        core.displayed_item = None;
        assert_eq!(core.export_preview_status(), PreviewStatus::Off);
    }
}
