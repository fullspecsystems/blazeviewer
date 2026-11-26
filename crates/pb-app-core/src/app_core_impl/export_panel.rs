//! **Export mode** — the `AppCore` half (task #89, rung 2). The subsystem is
//! [`crate::export_panel`] (settings, prefs, the panel model, the key layer); the write path
//! is [`crate::export`] + `app_core_impl/export.rs`. Paired per the two-halves rule.
//!
//! `E` toggles the mode; File ▸ Export… (`⇧⌘E`) and `⌘↩` / `Ctrl+Enter` enter it, or — in
//! it — export with the panel's settings. The panel's controls call the `export_set_*`
//! methods; the shells re-pull [`AppCore::export_panel`] on `PanelsChanged`.

use super::*;
use crate::export_panel::{
    clock_label, export_key_override, CustomSize, ExportKey, ExportPanel, ExportSettings,
    FormatChoice, PanelKey, PanelSource, PngCompression, SizeBy, SizeChoice, SizeDialog,
};
use crate::frame_capture::FrameKey;
use crate::overlay::PanelContent;

impl AppCore {
    /// Whether Export mode is what the right-hand slot shows — its panel is up and its
    /// contextual keys are live. Off while `Tab`-hidden (the mode is *suspended*) and while
    /// Help overlays it.
    pub fn export_mode_active(&self) -> bool {
        self.panels.content() == Some(PanelContent::Export)
    }

    /// File ▸ Export… — enabled whenever there's an item to enter the mode on (a door says
    /// why it can't export, in the panel); in the mode, only when this item can export.
    pub fn export_menu_enabled(&self) -> bool {
        self.displayed_item.is_some() && (!self.export_mode_active() || self.can_export())
    }

    /// Whether the shell should draw the Export panel (both shells draw it natively).
    pub fn export_panel_visible(&self) -> bool {
        self.export_mode_active()
    }

    /// `E` (`Action::ExportMode`): enter or leave Export mode. A fresh entry starts from the
    /// saved preferences with Strip metadata back on, and stops a running slideshow (its
    /// `[` / `]` become the quality keys).
    pub fn toggle_export_mode(&mut self) {
        if !self.panels.export {
            self.export.draft = ExportSettings::from_prefs(&self.settings.export);
            if self.slideshow.on {
                self.toggle_slideshow();
            }
        }
        self.panels.toggle_export();
        self.refresh_slot();
        self.emit_panels_changed();
    }

    /// File ▸ Export… / `⌘↩` (`Action::Export`): enter the mode, or — in it — export.
    pub fn export_enter_or_commit(&mut self) {
        if self.export_mode_active() {
            self.export_commit();
        } else {
            if self.panels.help && self.panels.export && !self.panels.hidden {
                // The mode is on under Help: close Help and show it.
                self.panels.help = false;
                self.refresh_slot();
                self.emit_panels_changed();
                return;
            }
            self.toggle_export_mode();
        }
    }

    /// The panel's ✕.
    pub fn export_close(&mut self) {
        if self.panels.export {
            self.panels.export = false;
            self.refresh_slot();
            self.emit_panels_changed();
        }
    }

    /// Export… with the panel's settings: freeze the request, then the save dialog.
    pub fn export_commit(&mut self) {
        if let Some(why) = self.displayed_item.and_then(|i| self.export_blocked(i)) {
            self.show_toast(&why);
            return;
        }
        // The same limit the panel shows (and disables Export… for): ⌘↩ mustn't get past it.
        let dims = self.displayed_item.and_then(|i| self.export_source_dims(i));
        if let Some(why) = dims.and_then(|d| self.export.draft.size_limit(d)) {
            self.show_toast(&why);
            return;
        }
        self.save_export_prefs();
        let settings = self.export.draft;
        // The preview's file for exactly these settings, when it's made: written as is.
        let encoded = self.export_preview_file();
        self.export_cmd_with(settings, encoded);
    }

    /// Why `item` can't be exported, if it can't — the panel says it, Export… refuses.
    pub(crate) fn export_blocked(&self, item: usize) -> Option<String> {
        match crate::video::item_kind(self.source.as_ref(), item) {
            crate::video::LibraryItemKind::Image => None,
            crate::video::LibraryItemKind::Video(_) => (!crate::frame_capture::CAN_CAPTURE_VIDEO)
                .then(|| "Exporting video frames isn't supported in this build".to_string()),
            crate::video::LibraryItemKind::Archive(_) => {
                Some("Open the archive to export its images".to_string())
            }
        }
    }

    /// The Export panel's view model for the item on screen.
    pub fn export_panel(&self) -> ExportPanel {
        let source = self
            .displayed_item
            .map(|item| self.export_panel_source(item));
        let preview = crate::export_panel::PreviewReadout {
            status: self.export_preview_status(),
            side: self.export_preview_side(),
            resized: self.export_preview_resized(),
            own_size: self.export.preview.own_size,
            available: self.export_source_metadata(),
            source_bytes: self.export_source_bytes(),
        };
        ExportPanel::build(self.export.draft, source)
            .with_preview(preview, self.export_preview_notes())
    }

    fn export_panel_source(&self, item: usize) -> PanelSource {
        let kind = crate::video::item_kind(self.source.as_ref(), item);
        let moving = self.frame_is_moving(item);
        let frame = match (kind, self.frame_key(item)) {
            // Turning a model doesn't change the wording: "The view on screen" already means
            // the view when you export, and a longer line while it moves made the panel jump.
            (_, FrameKey::View { .. }) => Some("The view on screen".to_string()),
            (_, _) if moving => Some("The frame on screen when you export".to_string()),
            (crate::video::LibraryItemKind::Video(_), FrameKey::Still) => {
                Some("Poster frame".to_string())
            }
            (crate::video::LibraryItemKind::Video(_), FrameKey::Video { millis }) => {
                Some(format!("Frame at {}", clock_label(millis / 1000)))
            }
            // A Live Photo's motion, paused.
            (_, FrameKey::Video { millis }) => Some(format!(
                "Live Photo frame at {}",
                clock_label(millis / 1000)
            )),
            (_, FrameKey::Animation { index }) => Some(format!("Frame {}", index + 1)),
            _ => None,
        };
        PanelSource {
            name: crate::engine::file_name_of(self.source.name(item)),
            frame,
            dims: self.export_source_dims(item),
            blocked: self.export_blocked(item),
        }
    }

    /// `item`'s pixel size **as displayed** (the RAM rotation applied), when known.
    pub(super) fn export_source_dims(&self, item: usize) -> Option<(u32, u32)> {
        // A turned model exports the window as shown, at its real pixels.
        if let Some((_, [w, h])) = self.live_view_capture(item) {
            return Some((w, h));
        }
        if matches!(
            crate::video::item_kind(self.source.as_ref(), item),
            crate::video::LibraryItemKind::Archive(_)
        ) {
            return None; // a door's frame is a sentinel, not a size
        }
        let meta = self.meta_cache.get(&item)?;
        let (w, h) = (meta.w, meta.h);
        if w == 0 || h == 0 {
            return None;
        }
        let turns = self.rotations.get(&item).copied().unwrap_or_default();
        Some(if crate::export::quarter_turns(turns) % 2 == 1 {
            (h, w)
        } else {
            (w, h)
        })
    }

    /// The cheap key the tick compares to decide whether the panel changed.
    pub(super) fn export_panel_key(&self) -> Option<PanelKey> {
        if !self.export_panel_visible() {
            return None;
        }
        let item = self.displayed_item;
        let moving = item.is_some_and(|i| self.frame_is_moving(i));
        Some(PanelKey {
            item,
            settings: self.export.draft,
            frame: item.filter(|_| !moving).map(|i| self.frame_key(i)),
            moving,
            rotation: item
                .and_then(|i| self.rotations.get(&i).copied())
                .map(crate::export::quarter_turns)
                .unwrap_or(0),
            dims: item.and_then(|i| self.export_source_dims(i)),
            preview: {
                let (state, bytes) = self.export_preview_status_key();
                (state, bytes, self.export_preview_side())
            },
        })
    }

    // ---- The panel's controls -------------------------------------------------------------

    pub fn export_set_format(&mut self, format: FormatChoice) {
        let mut s = self.export.draft;
        s.format = format;
        self.export_edited(s);
    }

    /// The quality slider: the current format's quality (each lossy format keeps its own).
    pub fn export_set_quality(&mut self, quality: u8) {
        let mut s = self.export.draft;
        s.set_quality(quality);
        self.export_edited(s);
    }

    /// WebP's and JPEG XL's Lossless checkbox.
    pub fn export_set_lossless(&mut self, on: bool) {
        let mut s = self.export.draft;
        s.set_lossless(on);
        self.export_edited(s);
    }

    pub fn export_set_png_compression(&mut self, c: PngCompression) {
        let mut s = self.export.draft;
        s.png = c;
        self.export_edited(s);
    }

    /// A ready-made size from the Size menu (it leaves Custom).
    pub fn export_set_size(&mut self, size: SizeChoice) {
        let mut s = self.export.draft;
        s.size = size;
        s.custom = None;
        self.export_edited(s);
    }

    /// The Size menu's **Custom** (macOS): show the Custom row, giving the current size the
    /// way it's already given (a preset or the full size: by long edge). The export doesn't
    /// change until a number is entered.
    pub fn export_size_custom(&mut self) {
        let mut s = self.export.draft;
        if s.custom.is_none() {
            s.custom = Some(s.size.by_value().map_or(SizeBy::LongEdge, |(by, _)| by));
            self.export_edited(s);
        }
    }

    /// The Custom row's "Resize by": give the size this way, keeping the export the size it
    /// was ([`CustomSize::for_size`]). An export at full size stays at full size.
    pub fn export_set_size_by(&mut self, by: SizeBy) {
        let mut s = self.export.draft;
        s.custom = Some(by);
        let dims = self.displayed_item.and_then(|i| self.export_source_dims(i));
        let resized = dims.is_some_and(|d| s.size.target(d).is_some());
        if resized {
            if let Some(size) = CustomSize::for_size(by, s.size, dims)
                .value
                .and_then(|v| SizeChoice::custom(by, v))
            {
                s.size = size;
            }
        }
        self.export_edited(s);
    }

    /// The Custom row's number, entered. Returns `false` (nothing changes) when it's out of
    /// range for `by`.
    pub fn export_set_custom_size(&mut self, by: SizeBy, value: u32) -> bool {
        let Some(size) = SizeChoice::custom(by, value) else {
            return false;
        };
        let mut s = self.export.draft;
        s.size = size;
        s.custom = Some(by);
        self.export_edited(s);
        true
    }

    /// The Size menu's **Custom…**: ask for a size (a sheet on macOS, the dialog window on
    /// winit). The answer comes back as `DialogResult::ExportSizeChosen`.
    pub fn export_custom_size(&mut self) {
        if !self.export_mode_active() {
            return;
        }
        self.effects.push(contract::CoreEffect::ShowDialog(
            contract::DialogKind::ExportSize,
        ));
    }

    /// The Custom… dialog's starting values: the current size (from the full size, the
    /// image's own long edge) and the image's size, for the live result line
    /// ([`SizeDialog::readout`]).
    pub fn export_size_dialog(&self) -> SizeDialog {
        SizeDialog::for_choice(
            self.export.draft.size,
            self.displayed_item.and_then(|i| self.export_source_dims(i)),
        )
    }

    /// Whether the photo on screen has anything for `kind`: the panel greys a category it
    /// lacks. Known once the preview has read the source; until then all are offered.
    pub fn export_keep_available(&self, kind: crate::export_panel::MetadataKind) -> bool {
        self.export_source_metadata().is_none_or(|a| a.get(kind))
    }

    /// One of the categories unchecking Strip metadata reveals (plan § 3.7).
    pub fn export_set_keep(&mut self, kind: crate::export_panel::MetadataKind, on: bool) {
        let mut s = self.export.draft;
        s.keep.set(kind, on);
        self.export_edited(s);
    }

    pub fn export_set_strip_metadata(&mut self, strip: bool) {
        let mut s = self.export.draft;
        s.strip_metadata = strip;
        self.export_edited(s);
    }

    /// `[` / `]` in the mode: quality down / up.
    pub fn export_step_quality(&mut self, dir: i32) {
        let mut s = self.export.draft;
        if s.step_quality(dir) {
            self.export_edited(s);
        }
    }

    fn export_edited(&mut self, settings: ExportSettings) {
        if settings == self.export.draft {
            return;
        }
        self.export.draft = settings;
        self.export.prefs_dirty = true;
        self.emit_panels_changed();
    }

    /// A key press in Export mode that means something different there (the contextual
    /// layer, [`export_key_override`]). Returns whether it was consumed.
    pub(super) fn export_key(
        &mut self,
        key: crate::pb_key::PbKey,
        res: contract::KeyResolution,
    ) -> bool {
        if !self.export_mode_active() {
            return false;
        }
        let Some(k) = export_key_override(res) else {
            return false;
        };
        match k {
            ExportKey::Quality(dir) => self.export_step_quality(dir),
            ExportKey::Flick => self.export_flick_down(key),
            ExportKey::Commit => self.export_commit(),
            ExportKey::Leave => self.export_close(),
        }
        true
    }

    /// Persist the panel's preferences once they've settled: when the mode closes (by any
    /// route), at Export…, and at quit — not on every slider tick.
    pub(super) fn tick_export_prefs(&mut self) {
        if self.export.prefs_dirty && !self.panels.export {
            self.save_export_prefs();
        }
    }

    pub(super) fn save_export_prefs(&mut self) {
        if !std::mem::take(&mut self.export.prefs_dirty) {
            return;
        }
        let prefs = self.export.draft.to_prefs();
        if prefs != self.settings.export {
            self.settings.export = prefs;
            // A preference (privacy #2: never what was exported or where). Tests never write.
            if self.persist_prefs {
                self.settings.save();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{photos_named, test_core};
    use crate::contract::{CoreEvent, KeyResolution, Modifiers};

    fn core_on(names: &[&str]) -> AppCore {
        let mut core = test_core();
        core.persist_prefs = false;
        core.source = photos_named(names);
        core.displayed_item = Some(0);
        core
    }

    fn key(core: &mut AppCore, chord: &str) {
        let c = crate::keymap::KeyChord::parse(chord).unwrap();
        core.handle(CoreEvent::KeyDown {
            key: c.code,
            mods: Modifiers {
                ctrl: c.ctrl,
                shift: c.shift,
                alt: c.alt,
                logo: c.logo,
            },
            repeat: false,
        });
        core.handle(CoreEvent::KeyUp { key: c.code });
    }

    fn with_dims(core: &mut AppCore, item: usize, w: u32, h: u32) {
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

    /// View ▸ Export Mode is checked in the mode, and the `[` / `]` menu items (Slideshow
    /// Faster / Slower) say and do what the keys do there: the quality, PNG's compression, or
    /// nothing. Outside the mode they're the slideshow's again.
    #[test]
    fn the_step_menu_items_follow_export_mode() {
        use crate::contract::StepKeys;
        let mut core = core_on(&["a.jpg"]);
        assert_eq!(core.export_menu(), (false, StepKeys::Slideshow));
        key(&mut core, "E");
        core.export_set_format(FormatChoice::Jpeg);
        assert_eq!(core.export_menu(), (true, StepKeys::Quality));
        let q = core.export.draft.quality().unwrap();
        core.handle(CoreEvent::MenuAction(Action::SlideshowFaster));
        assert!(
            core.export.draft.quality().unwrap() < q,
            "the [ item lowers the quality"
        );
        core.handle(CoreEvent::MenuAction(Action::SlideshowSlower));
        assert_eq!(core.export.draft.quality(), Some(q));
        core.export_set_format(FormatChoice::Png);
        assert_eq!(core.export_menu(), (true, StepKeys::Compression));
        core.export_set_format(FormatChoice::Tiff);
        assert_eq!(core.export_menu(), (true, StepKeys::Inert));
        key(&mut core, "E");
        assert_eq!(core.export_menu(), (false, StepKeys::Slideshow));
    }

    /// Review finding: ⌘↩ / Ctrl+Enter mustn't get past the limit the panel disables Export…
    /// for (WebP stops at 16383 px). A smaller size gets through.
    #[test]
    fn the_commit_keys_respect_the_formats_size_limit() {
        let mut core = core_on(&["pano.jpg"]);
        core.native_toast = true; // read the toast's text
        with_dims(&mut core, 0, 20_000, 1_000);
        key(&mut core, "E");
        core.export_set_format(FormatChoice::Webp);
        core.effects.clear();
        core.export_commit();
        assert!(!core.export.busy(), "no request, no save dialog");
        assert!(
            core.toast_native
                .as_ref()
                .is_some_and(|t| t.message.contains("16383 pixels")),
            "the panel's reason, as a toast"
        );
        core.export_set_size(SizeChoice::LongEdge(3840));
        core.export_commit();
        assert!(core.export.busy(), "a size that fits goes ahead");
    }

    #[test]
    fn e_toggles_the_mode_and_the_slot_shows_its_panel() {
        let mut core = core_on(&["a.jpg", "b.jpg"]);
        key(&mut core, "E");
        assert!(core.export_mode_active());
        assert_eq!(core.slot_content(), Some(SlotContent::Export));
        key(&mut core, "E");
        assert!(!core.export_mode_active());
        assert_eq!(core.slot_content(), None);
    }

    #[test]
    fn the_first_export_press_enters_the_mode_and_the_second_exports() {
        let mut core = core_on(&["a.jpg"]);
        core.handle(CoreEvent::MenuAction(Action::Export));
        assert!(core.export_mode_active(), "File ▸ Export… enters the mode");
        assert!(!core.export.busy(), "and doesn't export yet");
        core.handle(CoreEvent::MenuAction(Action::Export));
        assert!(core.export.busy(), "in the mode it exports");
    }

    #[test]
    fn custom_size_asks_with_the_current_size_and_applies_the_answer() {
        let mut core = core_on(&["a.jpg"]);
        with_dims(&mut core, 0, 3000, 2000);
        core.export_custom_size();
        assert!(
            core.effects.is_empty(),
            "outside the mode there's no Size menu"
        );
        key(&mut core, "E");
        core.effects.clear();
        core.export_custom_size();
        assert!(core.effects.iter().any(|e| matches!(
            e,
            contract::CoreEffect::ShowDialog(contract::DialogKind::ExportSize)
        )));
        let d = core.export_size_dialog();
        assert_eq!(
            (d.by, d.value, d.dims),
            (
                crate::export_panel::SizeBy::LongEdge,
                3000,
                Some((3000, 2000))
            ),
            "from full size, the dialog offers the image's own long edge"
        );
        core.effects.clear();
        core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::ExportSizeChosen(SizeChoice::Width(1200)),
        ));
        assert_eq!(core.export.draft.size, SizeChoice::Width(1200));
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::CloseDialog)));
        assert_eq!(core.export_panel().output_dims, Some((1200, 800)));
        let d = core.export_size_dialog();
        assert_eq!((d.by, d.value), (crate::export_panel::SizeBy::Width, 1200));
        // Esc on the dialog changes nothing.
        core.handle(CoreEvent::DialogResolved(
            contract::DialogResult::Dismissed(Some(contract::DialogKind::ExportSize)),
        ));
        assert_eq!(core.export.draft.size, SizeChoice::Width(1200));
    }

    /// The macOS Custom row (owner, 2026-11-26): choosing Custom shows it without changing the
    /// export, "Resize by" keeps the export's size, a number applies, and a preset leaves it.
    #[test]
    fn the_custom_row_shows_converts_and_applies() {
        use crate::export_panel::{CustomSize, SizeBy};
        let mut core = core_on(&["a.jpg"]);
        with_dims(&mut core, 0, 3000, 2000);
        key(&mut core, "E");
        assert_eq!(core.export_panel().custom, None, "a preset: no Custom row");

        // From full size: by long edge, the image's own, and still the full size.
        core.export_size_custom();
        let p = core.export_panel();
        assert_eq!(
            p.custom,
            Some(CustomSize {
                by: SizeBy::LongEdge,
                value: Some(3000)
            })
        );
        assert_eq!(core.export.draft.size, SizeChoice::Full);
        assert_eq!(p.output_dims, Some((3000, 2000)));
        // Changing "Resize by" at full size stays at full size: no "already … px" note.
        core.export_set_size_by(SizeBy::Width);
        assert_eq!(core.export.draft.size, SizeChoice::Full);
        assert_eq!(
            core.export_panel().custom.map(|c| c.value),
            Some(Some(3000))
        );
        core.export_set_size_by(SizeBy::Percent);
        assert_eq!(core.export_panel().custom.map(|c| c.value), Some(Some(100)));

        // A number applies; out of range changes nothing.
        assert!(!core.export_set_custom_size(SizeBy::Percent, 0));
        assert!(!core.export_set_custom_size(SizeBy::Percent, 101));
        assert!(core.export_set_custom_size(SizeBy::Percent, 50));
        assert_eq!(core.export.draft.size, SizeChoice::Percent(50));
        assert_eq!(core.export_panel().output_dims, Some((1500, 1000)));
        // Resize by keeps the output: 50% of 3000 × 2000 is 1500 wide, 1000 high.
        core.export_set_size_by(SizeBy::Width);
        assert_eq!(core.export.draft.size, SizeChoice::Width(1500));
        core.export_set_size_by(SizeBy::Height);
        assert_eq!(core.export.draft.size, SizeChoice::Height(1000));
        core.export_set_size_by(SizeBy::LongEdge);
        assert_eq!(core.export.draft.size, SizeChoice::LongEdge(1500));
        assert_eq!(core.export_panel().output_dims, Some((1500, 1000)));

        // A custom number equal to a preset stays on Custom.
        assert!(core.export_set_custom_size(SizeBy::LongEdge, 2048));
        let p = core.export_panel();
        assert_eq!(p.settings.size, SizeChoice::LongEdge(2048));
        assert_eq!(p.custom.map(|c| c.by), Some(SizeBy::LongEdge));

        // A preset from the menu leaves Custom, even the same size.
        core.export_set_size(SizeChoice::LongEdge(2048));
        assert_eq!(core.export_panel().custom, None);
        // Custom from a preset starts from it.
        core.export_size_custom();
        assert_eq!(
            core.export_panel().custom,
            Some(CustomSize {
                by: SizeBy::LongEdge,
                value: Some(2048)
            })
        );
        assert_eq!(core.export.draft.size, SizeChoice::LongEdge(2048));
        // The row shapes the panel, never the preview's file.
        assert_eq!(core.export.draft.normalized().custom, None);
    }

    // ---- The mode × action table (export-plan.md § 3.11) — one test per row --------------

    #[test]
    fn row_entering_stops_a_slideshow_and_brackets_become_quality() {
        let mut core = core_on(&["a.jpg", "b.jpg"]);
        core.toggle_slideshow();
        assert!(core.slideshow.on);
        key(&mut core, "E");
        assert!(!core.slideshow.on, "entering the mode stops it");
        core.export_set_format(FormatChoice::Jpeg);
        let q = core.export.draft.jpeg_quality;
        let interval = core.slideshow.interval;
        key(&mut core, "]");
        assert_eq!(core.export.draft.jpeg_quality, q + 5, "] is quality up");
        key(&mut core, "[");
        key(&mut core, "[");
        assert_eq!(core.export.draft.jpeg_quality, q - 5, "[ is quality down");
        assert_eq!(
            core.slideshow.interval, interval,
            "the slideshow speed is untouched"
        );
    }

    #[test]
    fn row_navigating_keeps_the_mode_and_its_settings() {
        let mut core = core_on(&["a.jpg", "b.jpg", "c.jpg"]);
        key(&mut core, "E");
        core.export_set_format(FormatChoice::Jpeg);
        core.export_set_size(SizeChoice::LongEdge(2048));
        core.handle(CoreEvent::MenuAction(Action::Next));
        assert!(core.export_mode_active());
        assert_eq!(core.export.draft.format, FormatChoice::Jpeg);
        assert_eq!(core.export.draft.size, SizeChoice::LongEdge(2048));
    }

    #[test]
    fn row_tab_suspends_the_mode_and_its_keys() {
        let mut core = core_on(&["a.jpg"]);
        key(&mut core, "E");
        key(&mut core, "Tab");
        assert!(!core.export_mode_active(), "suspended");
        assert!(core.panels.export, "not left");
        assert!(!core.export_panel_visible());
        let q = core.export.draft.png;
        key(&mut core, "]");
        assert_eq!(core.export.draft.png, q, "] is the slideshow key again");
        key(&mut core, "Tab");
        assert!(core.export_mode_active(), "Tab brings it back");
    }

    #[test]
    fn row_a_door_says_why_and_export_refuses() {
        let mut core = core_on(&["set.zip", "a.jpg"]);
        key(&mut core, "E");
        let panel = core.export_panel();
        assert!(!panel.can_export);
        assert!(panel.source.as_ref().unwrap().blocked.is_some());
        core.export_commit();
        assert!(!core.export.busy(), "nothing exported from a door");
        // The next photo exports.
        core.displayed_item = Some(1);
        assert!(core.export_panel().can_export);
    }

    #[test]
    fn row_help_overlays_the_mode_and_its_keys_rest() {
        let mut core = core_on(&["a.jpg"]);
        key(&mut core, "E");
        core.toggle_help();
        assert_eq!(core.slot_content(), Some(SlotContent::Help));
        assert!(!core.export_mode_active());
        core.toggle_help();
        assert!(
            core.export_mode_active(),
            "closing Help returns to the mode"
        );
    }

    #[test]
    fn row_an_inspector_tab_leaves_the_mode_and_leaving_restores_the_tab() {
        let mut core = core_on(&["a.jpg"]);
        core.panels.open_inspector(InspectorTab::Text);
        key(&mut core, "E");
        assert!(
            !core.inspector_panel_visible(),
            "the mode covers the Inspector"
        );
        key(&mut core, "E");
        assert!(core.inspector_panel_visible() || !core.native_inspector);
        assert_eq!(core.slot_content(), Some(SlotContent::Text));
    }

    #[test]
    fn row_esc_and_enter_keep_their_global_meanings() {
        // Owner decision 11 (pinned for revisit): the key layer leaves them alone.
        let mut core = core_on(&["a.jpg", "b.jpg", "c.jpg"]);
        key(&mut core, "E");
        let res = contract::resolve_key_down(
            &core.keymap,
            crate::keymap::KeyChord::parse("Enter").unwrap().code,
            Modifiers::NONE,
            false,
        );
        assert!(crate::export_panel::export_key_override(res).is_none());
        assert!(matches!(res, KeyResolution::NavStart(Action::Random)));
    }

    #[test]
    fn the_panel_describes_the_item_as_displayed() {
        let mut core = core_on(&["a.jpg"]);
        with_dims(&mut core, 0, 4000, 3000);
        key(&mut core, "E");
        core.export_set_size(SizeChoice::LongEdge(2048));
        let p = core.export_panel();
        assert_eq!(p.source.as_ref().unwrap().name, "a.jpg");
        assert_eq!(p.output_dims, Some((2048, 1536)));
        // A quarter turn swaps the box.
        core.rotations.insert(0, pb_render::Rotation::R90);
        let p = core.export_panel();
        assert_eq!(p.source.as_ref().unwrap().dims, Some((3000, 4000)));
        assert_eq!(p.output_dims, Some((1536, 2048)));
    }

    #[test]
    fn a_fresh_entry_starts_from_the_prefs_and_strip_is_always_back_on() {
        let mut core = core_on(&["a.jpg"]);
        key(&mut core, "E");
        core.export_set_format(FormatChoice::Jpeg);
        core.export_set_strip_metadata(false);
        key(&mut core, "E"); // leave
        core.tick_export_prefs();
        assert_eq!(core.settings.export.format, FormatChoice::Jpeg, "persisted");
        key(&mut core, "E"); // fresh entry
        assert_eq!(core.export.draft.format, FormatChoice::Jpeg);
        assert!(
            core.export.draft.strip_metadata,
            "Strip starts checked (Q7)"
        );
    }

    #[test]
    fn prefs_save_when_the_mode_closes_not_on_every_edit() {
        let mut core = core_on(&["a.jpg"]);
        key(&mut core, "E");
        core.export_set_format(FormatChoice::Jpeg);
        core.export_set_quality(60);
        core.tick_export_prefs();
        assert_ne!(core.settings.export.jpeg_quality, 60, "still in the mode");
        core.export_close();
        core.tick_export_prefs();
        assert_eq!(core.settings.export.jpeg_quality, 60);
        assert!(!core.persist_prefs, "and a test never writes settings.toml");
    }

    #[test]
    fn the_panel_key_changes_only_with_what_the_panel_shows() {
        let mut core = core_on(&["a.jpg", "b.jpg"]);
        assert_eq!(core.export_panel_key(), None, "not visible");
        key(&mut core, "E");
        let k = core.export_panel_key();
        assert!(k.is_some());
        assert_eq!(core.export_panel_key(), k, "stable while nothing changes");
        core.export_set_format(FormatChoice::Jpeg);
        assert_ne!(core.export_panel_key(), k, "a setting");
        let k = core.export_panel_key();
        core.displayed_item = Some(1);
        assert_ne!(core.export_panel_key(), k, "navigation");
    }
}
