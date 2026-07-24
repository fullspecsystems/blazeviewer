//! **Panels, overlay and the info line** — the `AppCore` half of [`crate::panels`] and
//! [`crate::overlay`] (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! Everything the viewer shows *about* the current item rather than the item itself: the
//! help overlay, the inspector tabs (info / EXIF / details / text / describe), and the info
//! line along the bottom edge.
//!
//! The UI philosophy these implement is in the root `CLAUDE.md`: every one of these is a
//! keypress away and every one is dismissible. Nothing here may put itself back on screen.
//!
//! ⚠ `emit_panels_changed` is `pub(super)` and has ~23 callers across the whole crate — it
//! is the "something a panel displays just changed" notifier, not a panels-local helper.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Toggle the keybindings help panel (`/` or `?`). Shares the single HUD overlay
    /// slot with the Inspector (interim), so opening it replaces whichever tab was
    /// showing; while `Tab`-hidden it reveals instead of closing (the reveal rule).
    pub fn toggle_help(&mut self) {
        self.panels.toggle_help();
        self.refresh_slot();
    }

    /// Toggle rich-panel visibility (`Tab`, task #54): hide the Inspector/Help/tree
    /// **and** the basic `i` info line without closing/un-toggling any of them, or
    /// reveal them all. No-op when nothing is open (including the line). Toasts and
    /// hints stay their own ephemeral layer, untouched by `Tab`.
    pub fn toggle_panels(&mut self) {
        if !self
            .panels
            .toggle_hidden(self.folder_tree_open, self.info_line)
        {
            return;
        }
        self.refresh_tree_visibility();
        self.refresh_slot();
    }

    /// Re-render or clear the shared overlay slot after panel state changed, and sync
    /// the info line's drawn state alongside it — both can flip on any action that
    /// touches `panels.hidden`, and this is the one choke point nearly all of them
    /// already call.
    pub(super) fn refresh_slot(&mut self) {
        if self.slot_content().is_some() {
            self.show_overlay();
        } else {
            self.hide_overlay();
        }
        self.refresh_info_line_visibility();
    }

    /// Apply the info line's visibility after a hide/reveal: hide it while
    /// `Tab`-hidden (state stays "on" for when panels reveal), draw it when revealed
    /// — mirrors `refresh_tree_visibility`. Applied eagerly rather than left to the
    /// next tick, since the app sleeps when idle and a tick may not run again soon.
    pub(super) fn refresh_info_line_visibility(&mut self) {
        if self.info_line && !self.panels.hidden {
            if !self.info_line_shown || self.info_line_item != self.displayed_item {
                self.show_info_line();
            }
        } else if self.info_line_shown {
            self.hide_info_line();
        }
    }

    /// `i` (the basic one-line info readout) or `Shift+I` (the Inspector's Details
    /// tab). Independent (task #54): opening/closing one never touches the other —
    /// the two can be on at once, the line sitting below the panel. When shown the
    /// line appears immediately (idle); after navigation it reappears once you stop
    /// (see the tick). `Tab`-hidden is the one thing they share (it's a single master
    /// switch): `i` while hidden follows the same reveal rule as `Shift+I`/Help/tree —
    /// reveal everything first, and only ever end up *shown*, never toggled off with
    /// nothing visibly changing.
    pub fn toggle_info(&mut self, full: bool) {
        if full {
            self.panels.toggle_inspector(InspectorTab::Details);
            self.refresh_slot();
        } else if self.panels.reveal() {
            self.info_line = true;
            self.refresh_tree_visibility();
            self.refresh_slot();
        } else {
            self.info_line = !self.info_line;
            if self.info_line {
                self.show_info_line();
            } else {
                self.hide_info_line();
            }
        }
    }

    /// The keybindings help table: a title row, then every hotkey → action as a
    /// shaded-key / description pair. The key labels are read from the live keymap
    /// (task #8 — single source of truth), so rebinding a key updates the help. A
    /// few rows stay curated: pan (shown as arrow glyphs), help (`/ or ?`), and the
    /// "hold to blaze" hint (no single binding).
    /// The user-facing shortcut hint for an action, formatted for this platform: on macOS the
    /// menu's ⌘-accelerator ([`menu::macos_menu_chord`]) where one exists — so Copy shows ⌘C and
    /// Move to Trash shows ⌘⌫, matching the menu bar rather than the keymap's legacy binding —
    /// else the primary keymap binding as Mac symbols; on Windows/Linux the spelled-out primary
    /// binding. Empty when unbound.
    pub fn help_shortcut(&self, action: Action) -> String {
        #[cfg(target_os = "macos")]
        if let Some(chord) = crate::keymap::macos_menu_chord(action) {
            return chord.mac_symbol();
        }
        self.keymap_shortcut(action)
    }

    /// The Help panel model (task #54): grouped sections (description + shortcut),
    /// sourced from the live keymap / menu so customized bindings and platform
    /// symbols stay correct. The HUD projects it via `render_shortcuts`; presenters
    /// consume it directly.
    pub fn help_panel(&self) -> HelpPanel {
        let sc = |a: Action| self.help_shortcut(a);
        let two =
            |a: Action, b: Action| format!("{} / {}", self.help_shortcut(a), self.help_shortcut(b));
        let row = |desc: &str, shortcut: String| (desc.to_string(), shortcut);
        // Platform wording for the trash action (the shortcut itself comes from `help_shortcut`).
        #[cfg(target_os = "macos")]
        let trash = "Move to Trash";
        #[cfg(not(target_os = "macos"))]
        let trash = "Delete to Recycle Bin";

        let section = |title: &str, rows: Vec<(String, String)>| HelpSection {
            title: title.to_string(),
            rows,
        };
        let sections = vec![
            section(
                "Browse",
                vec![
                    row("Next image", sc(Action::Next)),
                    row("Next even while playing", sc(Action::SkipNext)),
                    row("Previous image", sc(Action::Prev)),
                    row("Random image", sc(Action::Random)),
                    row("Previous random", sc(Action::RandomPrev)),
                    row("Slideshow", sc(Action::SlideshowToggle)),
                    row(
                        "Slideshow faster / slower",
                        two(Action::SlideshowFaster, Action::SlideshowSlower),
                    ),
                ],
            ),
            section(
                "View & Zoom",
                vec![
                    row("Fit to screen", sc(Action::ScaleFit)),
                    row("Crop to fill", sc(Action::ScaleFill)),
                    row("Toggle 1:1 and fit", sc(Action::ToggleOriginal)),
                    row("Zoom out / in", two(Action::ZoomOut, Action::ZoomIn)),
                    row("Pan", "\u{2190} \u{2191} \u{2193} \u{2192}".to_string()),
                    row(
                        "Rotate right / left",
                        two(Action::RotateCw, Action::RotateCcw),
                    ),
                    row(
                        "Flip / pin compare",
                        two(Action::CompareToggle, Action::ComparePin),
                    ),
                    row("Quick Full Screen", sc(Action::Fullscreen)),
                ],
            ),
            section(
                "Animation",
                vec![
                    row("Play / pause", sc(Action::PlayPause)),
                    row(
                        "Previous / next frame",
                        two(Action::FramePrev, Action::FrameNext),
                    ),
                    row("Mute Live Photo audio", sc(Action::MuteLiveAudio)),
                    row("Subtitles on/off", sc(Action::ToggleSubtitles)),
                    row("Next subtitle track", sc(Action::SubtitleCycle)),
                ],
            ),
            section(
                "Files & App",
                vec![
                    // Open and Quit show the *keymap* keys (O / ⇧O / Esc), not the menu's
                    // ⌘-chords `sc` would prefer — the bare keys are the ones this help
                    // exists to teach; every Mac user already knows ⌘O and ⌘Q.
                    row("Open file", self.keymap_shortcut(Action::OpenFile)),
                    row("Open folder", self.keymap_shortcut(Action::OpenFolder)),
                    row("Copy image", sc(Action::Copy)),
                    row("Copy file path", sc(Action::CopyPath)),
                    row("Save rotation", sc(Action::SaveRotation)),
                    row("Undo", sc(Action::Undo)),
                    row(trash, sc(Action::Delete)),
                    row("Delete permanently", sc(Action::DeletePermanent)),
                    row("Recursive (this folder)", sc(Action::Recursive)),
                    row("Info panel", sc(Action::Info)),
                    row("Detailed info panel", sc(Action::FullExif)),
                    row("Text in image", sc(Action::ShowImageText)),
                    row("Folder tree", sc(Action::FolderTree)),
                    row("Thumbnails", sc(Action::Thumbnails)),
                    row("Hide/show panels", sc(Action::TogglePanels)),
                    row("Parent folder", sc(Action::OpenParent)),
                    row(
                        "Previous / next folder",
                        two(Action::PrevFolder, Action::NextFolder),
                    ),
                    row("Settings", sc(Action::Settings)),
                    row("Quit", self.keymap_shortcut(Action::Quit)),
                    // Curated: the two real keys are "/" and "?" — `two()` would render
                    // the ⇧/ chord, and the keymap's names can't say "?" (the renderer
                    // dims only the spaced " / " separator, so the "/" key stays bright).
                    row("Help", "/ / ?".to_string()),
                ],
            ),
        ];
        HelpPanel { sections }
    }

    /// What the single **rich-panel** overlay slot shows right now, priority-resolved:
    /// Help > the Inspector's active tab. `None` = no rich panel (everything closed or
    /// `Tab`-hidden). The basic `i` line is a separate layer — see `info_line`.
    pub fn slot_content(&self) -> Option<SlotContent> {
        use crate::overlay::PanelContent;
        match self.panels.content() {
            Some(PanelContent::Help) => Some(SlotContent::Help),
            Some(PanelContent::Tab(InspectorTab::Details)) => Some(SlotContent::Details),
            Some(PanelContent::Tab(InspectorTab::Text)) => Some(SlotContent::Text),
            Some(PanelContent::Tab(InspectorTab::Describe)) => Some(SlotContent::Describe),
            None => None,
        }
    }

    /// Whether the current overlay-slot content is presented **natively** by the host
    /// (so the core suppresses its HUD rasterization): Help when `native_help`, and any
    /// Inspector tab when `native_inspector`. The tree joins as it goes native.
    fn slot_is_native(&self) -> bool {
        match self.slot_content() {
            Some(SlotContent::Help) => self.native_help,
            Some(SlotContent::Details | SlotContent::Text | SlotContent::Describe) => {
                self.native_inspector
            }
            None => false,
        }
    }

    /// Whether the **native** Inspector panel should be visible right now — the signal the
    /// mac host reads (via FFI) to show/hide its SwiftUI Inspector: the Inspector is open
    /// on some tab, not `Tab`-hidden, and the host presents it natively.
    pub fn inspector_panel_visible(&self) -> bool {
        self.native_inspector && self.panels.inspector.is_some() && !self.panels.hidden
    }

    /// A content snapshot of the Inspector's active tab (task #54) — for the tick's
    /// change diff and (indirectly) the host's re-pull. Details when the Inspector is
    /// closed (only read while visible, i.e. on some tab).
    pub fn inspector_snapshot(&self) -> crate::panels::InspectorSnapshot {
        use crate::panels::InspectorSnapshot;
        match self.panels.inspector {
            Some(InspectorTab::Text) => InspectorSnapshot::Text(self.text_panel()),
            Some(InspectorTab::Describe) => InspectorSnapshot::Describe(self.describe_panel()),
            _ => InspectorSnapshot::Details(self.details_panel()),
        }
    }

    /// Whether the **native** Help panel should be visible right now — the signal the
    /// mac host reads (via FFI) to show/hide its SwiftUI Help view. Help open, not
    /// `Tab`-hidden, and the host presents it natively.
    pub fn help_panel_visible(&self) -> bool {
        self.native_help && self.panels.help && !self.panels.hidden
    }

    /// Whether the **native** empty-state Open panel should be visible — the welcome
    /// surface shown when no photos are loaded (and no scan is bootstrapping). The host
    /// reads this to show/hide its native view, gated on the native flag.
    pub fn open_panel_visible(&self) -> bool {
        self.native_open && self.source.is_empty() && !self.scanning && !self.launching
    }

    /// Push the [`CoreEffect::PanelsChanged`] marker so the host re-pulls the native
    /// panel model — deduped (the drain can pull once for several mutations in a tick).
    pub(super) fn emit_panels_changed(&mut self) {
        if !self
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::PanelsChanged))
        {
            self.effects.push(contract::CoreEffect::PanelsChanged);
        }
    }

    /// The Inspector ▸ Text tab model (task #54): the semantic scan state for the
    /// displayed photo, read from the RAM-only caches. Pure projection — building
    /// it kicks nothing off (the show path calls `ensure_text_scan` separately).
    pub fn text_panel(&self) -> TextPanel {
        let body = match self.displayed_item {
            None => TextBody::NoPhoto,
            Some(item) => match self.recognized_text.get(&item) {
                Some(r) => TextBody::Ready {
                    qr: r.qr.clone(),
                    paragraphs: r.lines.clone(),
                    ocr_error: r.ocr_error.clone(),
                },
                None => TextBody::Scanning,
            },
        };
        TextPanel { body }
    }

    /// The Inspector ▸ Describe tab model (task #54): the semantic describe state
    /// for the displayed photo. Pure projection of the RAM-only caches.
    pub fn describe_panel(&self) -> DescribePanel {
        let body = match self.displayed_item {
            None => DescribeBody::NoPhoto,
            Some(item) => match self.descriptions.get(&item) {
                Some(Ok(text)) => DescribeBody::Ready(text.clone()),
                Some(Err(msg)) => DescribeBody::Error(msg.clone()),
                None if self.describe_scan.as_ref().is_some_and(|s| s.item == item) => {
                    DescribeBody::Busy
                }
                None => DescribeBody::Idle,
            },
        };
        DescribePanel { body }
    }

    /// The Inspector ▸ Details tab model (task #54): the full metadata table.
    pub fn details_panel(&self) -> DetailsPanel {
        DetailsPanel {
            rows: self.exif_rows(),
        }
    }

    /// Rasterize the active **rich panel** (Inspector tab or Help) and draw it,
    /// lifted above the info-line strip if that line shares the corner. The help
    /// overlay uses a larger font than the info panels. The basic `i` line is drawn
    /// separately by [`show_info_line`](Self::show_info_line).
    pub fn show_overlay(&mut self) {
        // A natively-presented panel (Help on the mac host) is drawn by the shell, not
        // the HUD — suppress the CPU rasterization entirely. Clear any HUD panel left
        // from a previous slot (e.g. switching Details → Help) so it doesn't linger
        // under the native view; the tick's visibility diff emits the marker.
        if self.slot_is_native() {
            if self.overlay_shown {
                self.hide_overlay();
            }
            return;
        }
        let px = (15.0 * self.viewport.scale_factor).max(8.0);
        let pad = (7.0 * self.viewport.scale_factor).round().max(2.0) as u32;
        // The info / EXIF panels honor the user's opacity setting; the help overlay
        // keeps the standard translucency. Both take the active theme's panel color.
        let theme = self.hud.as_ref().map_or(hud::Theme::DARK, |h| h.theme());
        let info_bg = theme.bg_for_opacity(self.settings.info_opacity);
        // Resolve the Live Photo pairing (cached; one stat) so the detailed table can
        // label it.
        if let Some(item) = self.displayed_item {
            self.live_motion_path(item);
        }
        // Cap the paragraph panels to a readable column, never wider than the window
        // allows at this margin.
        let margin = self.overlay_margin();
        let para_max_w = ((self.viewport.width as i32 - 2 * margin as i32).max(1) as u32)
            .min((440.0 * self.viewport.scale_factor) as u32);
        let max_h = (self.viewport.height as i32 - 2 * margin as i32).max(1);
        let panel = match self.slot_content() {
            None => return,
            Some(SlotContent::Details) => {
                // Warm the EXIF read once so the table build (and its per-frame rebuilds
                // during playback) never re-read the file.
                if let Some(item) = self.displayed_item {
                    self.ensure_exif_cached(item);
                }
                let model = self.details_panel();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                if model.rows.is_empty() {
                    return;
                }
                // Interim HUD projection: core rows → the rasterizer's table rows.
                let rows: Vec<Row> = model.rows.into_iter().map(hud_row).collect();
                hud.render_table(&rows, px, pad, info_bg)
            }
            Some(SlotContent::Help) => {
                let help_px = (15.0 * self.viewport.scale_factor).max(10.0);
                let model = self.help_panel();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                let sections: Vec<hud::ShortcutSection> = model
                    .sections
                    .into_iter()
                    .map(|s| hud::ShortcutSection {
                        title: s.title,
                        rows: s.rows,
                    })
                    .collect();
                hud.render_shortcuts(&sections, help_px, theme.bg, max_h)
            }
            Some(SlotContent::Text) => {
                if self.current.is_none() {
                    return;
                }
                // The panel tracks the displayed photo while open, so settling on a
                // new item kicks its scan here (no-op when cached / already running).
                self.ensure_text_scan();
                let lines = self.text_panel().lines();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                hud.render_paragraph(&lines, px, pad, info_bg, para_max_w, max_h)
            }
            Some(SlotContent::Describe) => {
                if self.current.is_none() {
                    return;
                }
                // Auto-describe (opt-in, `describe_auto`): only while the panel is already
                // open (this arm), so it's never a passive background send — the user chose
                // to be looking at descriptions. Off by default for privacy + token cost; on,
                // settling on a new photo describes it without another `D`. (This is a settle
                // path, not a per-frame one, so hold-to-blaze doesn't machine-gun the backend.)
                if self.settings.describe_auto {
                    self.ensure_describe_scan(None);
                }
                let lines = self.describe_panel().lines();
                let Some(hud) = self.hud.as_ref() else {
                    return;
                };
                hud.render_paragraph(&lines, px, pad, info_bg, para_max_w, max_h)
            }
        };
        let Some((bitmap, w, h)) = panel else {
            return;
        };
        // Lift the panel above the info line only when the line actually overlaps this
        // panel's horizontal span (task #54). The rich panel is bottom-right-anchored:
        // its span is `[sw - margin - w, sw - margin]`. Right inset stays `margin`.
        let sw = self.viewport.width as f32;
        let px1 = sw - margin as f32;
        let bottom = margin + self.info_line_reserve_for(px1 - w as f32, px1);
        if let Some(a) = self.renderer.as_mut() {
            a.set_overlay(Some((&bitmap, w, h)), margin, bottom);
        }
        self.overlay_shown = true;
        self.overlay_item = self.displayed_item;
        self.draw();
    }

    /// The info line's horizontal span `[x0, x1]` in physical px when it's drawn,
    /// from its alignment + rasterized width + the corner margin. `None` when the
    /// line isn't shown. The core-owned footprint every colliding layer reserves
    /// against (and, later, what the native presenters inset their layout by).
    fn info_line_span(&self) -> Option<(f32, f32)> {
        if !self.info_line_shown || self.info_line_w == 0 {
            return None;
        }
        let sw = self.viewport.width as f32;
        let w = self.info_line_w as f32;
        let m = self.overlay_margin() as f32;
        let x0 = match self.settings.info_line_align {
            settings::InfoLineAlign::Left => m,
            settings::InfoLineAlign::Center => ((sw - w) * 0.5).max(0.0),
            settings::InfoLineAlign::Right => (sw - m - w).max(0.0),
        };
        Some((x0, x0 + w))
    }

    /// The vertical strip (line height + gap) a layer must yield to clear the info
    /// line **iff** its horizontal `[px0, px1]` span overlaps the line's — so a panel
    /// on the opposite side reserves nothing, but a wide centered line that spans the
    /// whole width pushes both corner panels *and* the toast. `0` when there's no
    /// overlap or the line is hidden.
    pub(super) fn info_line_reserve_for(&self, px0: f32, px1: f32) -> u32 {
        let Some((lx0, lx1)) = self.info_line_span() else {
            return 0;
        };
        // A small gap so touching (not overlapping) edges don't trigger a reserve.
        let gap = 6.0 * self.viewport.scale_factor;
        if px0 < lx1 + gap && lx0 < px1 + gap {
            self.info_line_h + gap.round() as u32
        } else {
            0
        }
    }

    /// The info line's alignment as the renderer's [`pb_render::HAlign`].
    fn info_line_halign(&self) -> pb_render::HAlign {
        match self.settings.info_line_align {
            settings::InfoLineAlign::Left => pb_render::HAlign::Left,
            settings::InfoLineAlign::Center => pb_render::HAlign::Center,
            settings::InfoLineAlign::Right => pb_render::HAlign::Right,
        }
    }

    /// Rasterize + upload the basic info line (`i`) into its own bottom-anchored layer
    /// at the configured alignment, then re-place any colliding panel/tree/toast above
    /// it. A no-op without a font/photo (the tick retries on settle). Mirrors
    /// [`show_overlay`](Self::show_overlay) but for the ephemeral line.
    /// The full one-line readout — `rel · W×H · CODEC[· Live]`, each field gated by its
    /// Settings toggle — or `None` with no photo. The HUD (winit) rasterizes this whole string;
    /// the native shell instead reads `info_line_main` + `info_line_codec` (codec as a pill).
    pub fn info_line_content(&self) -> Option<String> {
        let meta = self.current.as_ref()?;
        let mut parts = self.info_line_parts(meta);
        if self.settings.info_show_codec {
            parts.push(meta.codec.to_string());
        }
        if self.displayed_item.is_some_and(|i| self.is_live_photo(i)) {
            parts.push("Live".to_string()); // a Live Photo's motion is playable (P)
        }
        Some(parts.join(" · "))
    }

    /// The name (folder / filename) and resolution fields, each gated by its Settings toggle
    /// (shared by the full HUD string and the native main text). Folder is prepended to the
    /// filename with a `/` — the relative dir when the scan is recursive, else the containing
    /// folder's name.
    pub(super) fn info_line_parts(&self, meta: &crate::meta::PhotoMeta) -> Vec<String> {
        let mut parts = Vec::new();
        // `rel` is the path relative to the scan root, so split its directory (nested scans)
        // from the file name.
        let (dir, file) = match meta.rel.rsplit_once('/') {
            Some((d, f)) => (Some(d.to_string()), f),
            None => (None, meta.rel.as_str()),
        };
        let name = match (
            self.settings.info_show_folder,
            self.settings.info_show_filename,
        ) {
            (true, true) => match dir.or_else(|| self.info_folder_name()) {
                Some(f) if !f.is_empty() => format!("{f}/{file}"),
                _ => file.to_string(),
            },
            (false, true) => file.to_string(),
            (true, false) => dir.or_else(|| self.info_folder_name()).unwrap_or_default(),
            (false, false) => String::new(),
        };
        if !name.is_empty() {
            parts.push(name);
        }
        if self.settings.info_show_resolution {
            // An archive door has no pixels to report — its frame is a 1×1 transparent
            // sentinel (task #105), so `w`/`h` would read `1 × 1`. Its size is the fact
            // that matters, and it rides `PhotoMeta` from `ItemSource::size_hint`
            // (resolved on the scan worker) precisely because this runs on the frame
            // path and may never touch the disk.
            match meta.size {
                Some(bytes) => parts.push(crate::meta::human_bytes(bytes)),
                None => parts.push(format!("{}×{}", meta.w, meta.h)),
            }
        }
        parts
    }

    /// The immediate containing folder's name — used for the Folder field when `rel` has no
    /// directory of its own (a flat, non-recursive scan). `None` for a source without a real
    /// path on disk (an archive entry).
    fn info_folder_name(&self) -> Option<String> {
        let item = self.displayed_item?;
        let path = self.source.path(item)?;
        path.parent()?
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }

    /// Whether the info readout should show: toggled on (`i`), not suppressed by
    /// `Tab`, and the enabled fields produce some text — an empty pill (all fields
    /// off, or a folder-only field that can't resolve) reads as a bug, so it hides
    /// instead. **The native macOS shell polls this directly** (`CoreModel.swift`)
    /// as its actual show/hide gate — it does not consult `info_line_shown` (that's
    /// the winit HUD rasterizer's own drawn-state bookkeeping) — so `panels.hidden`
    /// belongs here, not just in the HUD-side `refresh_info_line_visibility`.
    pub fn info_line_visible(&self) -> bool {
        // `video_osd_until` flashes the line as the video seek/step position OSD
        // even when the user's `i` toggle is off (it replaces the position toast).
        (self.info_line || self.video_osd_until.is_some())
            && !self.panels.hidden
            && self.info_line_content().is_some_and(|s| !s.is_empty())
    }

    /// The info readout's main text (native shell) — `rel · W×H[· Live]`, codec split out so the
    /// shell can pill it separately (like the folder-tree count badges). Each field is gated.
    pub fn info_line_main(&self) -> Option<String> {
        let meta = self.current.as_ref()?;
        // Note: no "Live" here — the native shell shows the livephoto *symbol* by the codec
        // (`info_line_is_live`) instead of the word. The HUD string (`info_line_content`) keeps
        // the text, since it can't draw a symbol.
        Some(self.info_line_parts(meta).join(" · "))
    }

    /// Whether the current photo is a Live Photo — the native shell renders the livephoto mark
    /// beside the codec in place of the "Live" text.
    pub fn info_line_is_live(&self) -> bool {
        self.current.is_some() && self.displayed_item.is_some_and(|i| self.is_live_photo(i))
    }

    /// Whether the current photo is an animated image (GIF / APNG / animated WebP / AVIF / …) —
    /// the native shell shows a motion mark by the codec. Distinct from a Live Photo, which has
    /// its own mark (`info_line_is_live`).
    pub fn info_line_is_animated(&self) -> bool {
        !self.info_line_is_live() && self.current.as_ref().is_some_and(|m| m.animated.is_some())
    }

    /// Whether the displayed item is a video (task 79.9): the info line shows a film
    /// mark by the codec, the way a Live Photo shows the livephoto glyph. Distinct
    /// from `info_line_is_animated` (GIF/APNG) — a video is a `LibraryItemKind::Video`,
    /// not a `PhotoMeta.animated`.
    pub fn info_line_is_video(&self) -> bool {
        self.current.is_some() && self.displayed_item.is_some_and(|i| self.item_is_video(i))
    }

    /// The current photo's codec label (e.g. `JPEG`) for the info readout's pill — empty when
    /// the codec field is toggled off (so the shell omits the badge).
    pub fn info_line_codec(&self) -> String {
        if !self.settings.info_show_codec {
            return String::new();
        }
        self.current
            .as_ref()
            .map(|m| m.codec.to_string())
            .unwrap_or_default()
    }

    /// The current photo's recovery notice (task #127): a human-readable reason when
    /// the file was **malformed but salvaged** by the decode ladder (e.g. `"Extra
    /// bytes between headers"` for a JPEG a strict decoder rejects). Non-empty means
    /// the details panel should show a "recovered from a damaged file" notice so a
    /// user digging into the file learns the full story; empty means a clean decode
    /// (the shell shows nothing). Reads the displayed photo's cached metadata.
    pub fn recovered_notice(&self) -> String {
        self.current
            .as_ref()
            .and_then(|m| m.recovered.clone())
            .unwrap_or_default()
    }

    /// The displayed item's decode-failure reason (task #127): non-empty when the
    /// current photo could not be decoded by ANY rung of the recovery ladder — a
    /// genuinely corrupt or unsupported file. The shell shows a "can't display this
    /// image" placeholder (with this as the subtitle) instead of leaving a black canvas
    /// and a spinning pie. Empty when the current item decoded (even if only recovered),
    /// or when there is no current item.
    pub fn current_decode_error(&self) -> String {
        self.displayed_item
            .filter(|i| self.failed.contains(i))
            .and_then(|i| self.failed_reason.get(&i).cloned())
            .unwrap_or_default()
    }

    /// The displayed item's bare file name (no path), or empty when nothing is shown.
    /// Used by the "can't display this image" placeholder to name the file (task #127) —
    /// where `current` is `None`, so the placeholder can't read the name from there.
    pub fn current_file_name(&self) -> String {
        self.displayed_item
            .map(|i| crate::engine::file_name_of(self.source.name(i)))
            .unwrap_or_default()
    }

    /// A change-detection snapshot of the natively-drawn info readout — `(main, codec, live,
    /// animated)` when visible, `None` when hidden. The tick diffs it so a native info line
    /// re-pulls on a real content change (a photo swap), never per tick. Alignment/opacity
    /// changes come through `apply_settings` → `emit_panels_changed`.
    pub fn info_line_snapshot(&self) -> Option<(String, String, bool, bool)> {
        self.info_line_visible().then(|| {
            (
                self.info_line_main().unwrap_or_default(),
                self.info_line_codec(),
                self.info_line_is_live(),
                self.info_line_is_animated(),
            )
        })
    }

    pub fn show_info_line(&mut self) {
        // Native shell draws the line — just track the toggle state; no HUD raster / colliders.
        if self.native_info {
            self.info_line_shown = self.current.is_some();
            self.info_line_item = self.displayed_item;
            return;
        }
        let Some(hud) = self.hud.as_ref() else {
            return;
        };
        let Some(text) = self.info_line_content() else {
            return;
        };
        let px = (15.0 * self.viewport.scale_factor).max(8.0);
        let pad = (7.0 * self.viewport.scale_factor).round().max(2.0) as u32;
        let theme = hud.theme();
        let info_bg = theme.bg_for_opacity(self.settings.info_opacity);
        // While a video session is live on the displayed item, the line gains its
        // playback row (`0:42 ▰▰▰▱▱ 9:01`, task #79 — owner design): one block,
        // one `i` toggle, the bar filling whatever width the summary establishes.
        let Some((bitmap, w, h)) = (match self.video_progress_row() {
            Some(row) => hud.render_panel_progress(&text, &row, px, pad, info_bg),
            None => hud.render_panel(&text, px, pad, info_bg),
        }) else {
            return;
        };
        let margin = self.overlay_margin();
        let align = self.info_line_halign();
        if let Some(a) = self.renderer.as_mut() {
            a.set_info_line(Some((&bitmap, w, h)), margin, align);
        }
        self.info_line_shown = true;
        self.info_line_item = self.displayed_item;
        self.info_line_w = w;
        self.info_line_h = h;
        self.replace_colliders(); // re-lift the panel / re-cap the tree if they overlap
    }

    /// Clear the info-line layer and drop any reservation it was causing.
    pub fn hide_info_line(&mut self) {
        // Native shell: nothing to tear down — just clear the tracking state.
        if self.native_info {
            self.info_line_shown = false;
            self.info_line_item = None;
            return;
        }
        if let Some(a) = self.renderer.as_mut() {
            a.set_info_line(None, 0, pb_render::HAlign::Right);
        }
        self.info_line_shown = false;
        self.info_line_item = None;
        self.info_line_w = 0;
        self.info_line_h = 0;
        self.replace_colliders();
    }

    /// Re-place the layers that reserve space against the info line — the rich panel
    /// (lifts) and the folder tree (caps its height) — after the line's presence,
    /// size, or alignment changed. The toast re-reads the reserve on its next build
    /// (it's transient). A redraw covers the case where nothing needed re-placing.
    fn replace_colliders(&mut self) {
        if self.overlay_shown {
            self.show_overlay();
        }
        if self.folder_tree_panel.is_some() {
            self.folder_tree_sig = None; // force a rebuild at the new tree height budget
        }
        if !self.overlay_shown {
            self.draw();
        }
    }

    /// Corner inset (physical px) for the info/EXIF/help panel. Scales with the
    /// surface's short edge so a fixed gap doesn't look jammed against the corner on a
    /// huge fullscreen display (#3), with a DPI-scaled floor for small windows. Read
    /// fresh on every (re)show, so toggling between window sizes always re-spaces it.
    pub fn overlay_margin(&self) -> u32 {
        let short_edge = self
            .fit
            .map(|f| f.max_width.min(f.max_height))
            .unwrap_or(800) as f32;
        let floor = 10.0 * self.viewport.scale_factor;
        (short_edge * 0.015).max(floor).round().max(1.0) as u32
    }

    /// Hide the rich panel (clears the overlay quad). The info line, a separate
    /// layer, is untouched.
    pub fn hide_overlay(&mut self) {
        if let Some(a) = self.renderer.as_mut() {
            a.set_overlay(None, 0, 0);
        }
        self.overlay_shown = false;
        self.overlay_item = None;
        self.draw();
    }

    /// The pre-formatted shortcut hint for an action's primary binding (empty if unbound) — the
    /// macOS symbol form (`⇧ O`) on macOS, the spelled-out form (`Shift+O`) elsewhere. Drives the
    /// open-screen buttons' shortcut hints, so they reflect any shortcut the user remapped in
    /// Settings.
    pub fn shortcut_for(&self, action: Action) -> String {
        self.keymap
            .bindings_for(action)
            .first()
            .map(|c| c.shortcut_label())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{photos_named, test_core};
    use crate::contract::CoreEvent;
    use crate::Viewport;

    #[test]
    fn info_line_fields_respect_the_settings_toggles() {
        use crate::meta::PhotoMeta;
        let mut core = test_core();
        core.current = Some(PhotoMeta {
            rel: "folder/photo.jpg".to_string(),
            w: 4032,
            h: 3024,
            size: None,
            codec: "JPEG",
            animated: None,
            recovered: None,
        });
        core.info_line = true;

        // Default fields (folder off, filename/resolution/codec on): the file NAME only, not
        // the relative dir.
        assert_eq!(
            core.info_line_content().as_deref(),
            Some("photo.jpg · 4032×3024 · JPEG")
        );
        assert_eq!(
            core.info_line_main().as_deref(),
            Some("photo.jpg · 4032×3024")
        );
        assert_eq!(core.info_line_codec(), "JPEG");
        assert!(core.info_line_visible());

        // Folder on → the relative dir is prepended to the file name with a `/`.
        core.settings.info_show_folder = true;
        assert_eq!(
            core.info_line_content().as_deref(),
            Some("folder/photo.jpg · 4032×3024 · JPEG")
        );
        // Folder on, filename off → just the folder.
        core.settings.info_show_filename = false;
        assert_eq!(
            core.info_line_content().as_deref(),
            Some("folder · 4032×3024 · JPEG")
        );
        core.settings.info_show_filename = true;
        core.settings.info_show_folder = false;

        // Codec off → dropped from the string, and the pill accessor goes empty (folder is
        // back off, so it's the file name alone again).
        core.settings.info_show_codec = false;
        assert_eq!(
            core.info_line_content().as_deref(),
            Some("photo.jpg · 4032×3024")
        );
        assert_eq!(core.info_line_codec(), "");

        // Filename off too → only the resolution remains.
        core.settings.info_show_filename = false;
        assert_eq!(core.info_line_content().as_deref(), Some("4032×3024"));
        assert_eq!(core.info_line_main().as_deref(), Some("4032×3024"));

        // All fields off → the line hides (empty-pill guard) even though `i` is on.
        core.settings.info_show_resolution = false;
        assert!(!core.info_line_visible());
    }

    #[test]
    fn recovered_notice_surfaces_a_malformed_files_reason() {
        use crate::meta::PhotoMeta;
        let mut core = test_core();
        // A clean decode carries no reason → the details panel shows no notice.
        core.current = Some(PhotoMeta {
            rel: "trip/clean.jpg".to_string(),
            w: 4032,
            h: 3024,
            size: None,
            codec: "JPEG",
            animated: None,
            recovered: None,
        });
        assert_eq!(core.recovered_notice(), "");

        // A malformed-but-recovered decode carries the reason → the notice shows it.
        core.current = Some(PhotoMeta {
            rel: "trip/ticket.jpg".to_string(),
            w: 4864,
            h: 3616,
            size: None,
            codec: "JPEG",
            animated: None,
            recovered: Some("Extra bytes between headers".to_string()),
        });
        assert_eq!(core.recovered_notice(), "Extra bytes between headers");

        // No current photo → empty (no panic).
        core.current = None;
        assert_eq!(core.recovered_notice(), "");
    }

    #[test]
    fn the_details_panel_shows_a_recovery_notice_for_a_malformed_file() {
        use crate::meta::PhotoMeta;
        let mut core = test_core();
        core.source = Arc::new(FsSource::new(vec![PathBuf::from("trip/ticket.jpg")]));
        core.displayed_item = Some(0);
        core.current = Some(PhotoMeta {
            rel: "trip/ticket.jpg".to_string(),
            w: 4864,
            h: 3616,
            size: None,
            codec: "JPEG",
            animated: None,
            recovered: Some("Extra bytes between headers".to_string()),
        });

        // A recovered file gets a "Recovered" row naming the reason, right in Details.
        let rows = core.details_panel().rows;
        assert!(
            rows.iter().any(|r| matches!(r,
                DetailRow::Pair { label, value }
                    if label == "Recovered" && value.contains("Extra bytes between headers"))),
            "a recovered file must show the notice row; got {rows:?}"
        );

        // A clean decode (no reason) shows no such row.
        core.current.as_mut().unwrap().recovered = None;
        let clean = core.details_panel().rows;
        assert!(
            !clean
                .iter()
                .any(|r| matches!(r, DetailRow::Pair { label, .. } if label == "Recovered")),
            "a clean file must not show a Recovered row; got {clean:?}"
        );
    }

    /// `info_line_visible()` is what the **native macOS shell** actually polls
    /// (`CoreModel.swift`) to show/hide its SwiftUI info-line view — unlike the
    /// winit HUD path, it never looks at `info_line_shown`. So `Tab` must suppress
    /// it here directly, or the native line ignores Tab-hide entirely.
    #[test]
    fn info_line_visible_respects_tab_hidden() {
        use crate::meta::PhotoMeta;
        let mut core = test_core();
        core.current = Some(PhotoMeta {
            rel: "a.jpg".to_string(),
            w: 100,
            h: 100,
            size: None,
            codec: "JPEG",
            animated: None,
            recovered: None,
        });
        core.info_line = true;
        assert!(core.info_line_visible());

        core.dispatch_action(Action::TogglePanels); // Tab: the line alone counts as open
        assert!(core.panels.hidden);
        assert!(
            !core.info_line_visible(),
            "the native shell's actual gate must hide too"
        );
        assert!(core.info_line, "…without turning the toggle off");

        core.dispatch_action(Action::TogglePanels); // Tab again reveals
        assert!(!core.panels.hidden);
        assert!(core.info_line_visible());
    }

    #[test]
    fn native_help_suppresses_the_hud_and_signals_visibility() {
        let mut core = test_core();
        core.native_help = true;
        // Opening Help does not rasterize a HUD overlay (the shell draws it).
        core.dispatch_action(Action::Help);
        assert!(core.panels.help, "Help is open in the model");
        assert!(!core.overlay_shown, "…but nothing is rasterized to the HUD");
        assert!(
            core.help_panel_visible(),
            "the native Help view should show"
        );
        // A tick emits the PanelsChanged marker on the show transition.
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "the tick signals the host to re-pull the panel model"
        );
        // Tab-hide hides the native view without closing it; a tick re-signals.
        core.dispatch_action(Action::TogglePanels);
        assert!(core.panels.help && core.panels.hidden);
        assert!(!core.help_panel_visible(), "hidden → the native view hides");
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)));
    }

    #[test]
    fn native_inspector_suppresses_the_hud_and_signals_on_tab_and_content() {
        use crate::overlay::InspectorTab;
        use crate::panels::InspectorSnapshot;
        let mut core = test_core();
        core.native_inspector = true;
        // Closed → not visible, no snapshot.
        assert!(!core.inspector_panel_visible());
        // Open the Details tab: visible, and the tick signals the host.
        core.panels.open_inspector(InspectorTab::Details);
        assert!(core.inspector_panel_visible());
        assert!(matches!(
            core.inspector_snapshot(),
            InspectorSnapshot::Details(_)
        ));
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "opening the Inspector signals the host"
        );
        // Switching tabs changes the snapshot → re-signals.
        core.panels.open_inspector(InspectorTab::Text);
        assert!(matches!(
            core.inspector_snapshot(),
            InspectorSnapshot::Text(_)
        ));
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "a tab switch re-signals"
        );
        // Tab-hidden → not visible (the master switch wins).
        core.panels.hidden = true;
        assert!(!core.inspector_panel_visible());
        // Winit (native_inspector off) never treats it as native-visible.
        core.panels.hidden = false;
        core.native_inspector = false;
        assert!(!core.inspector_panel_visible());
    }

    #[test]
    fn winit_keeps_help_on_the_hud_no_native_signal() {
        // With native_help off (the winit shell), Help is a HUD panel and never a
        // native-visible one, and the marker never fires.
        let mut core = test_core();
        core.dispatch_action(Action::Help);
        assert!(core.panels.help);
        assert!(
            !core.help_panel_visible(),
            "no native presentation on winit"
        );
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(!core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)));
    }

    #[test]
    fn info_line_reserve_follows_the_horizontal_overlap() {
        let mut core = test_core();
        core.viewport = Viewport {
            width: 1000,
            height: 800,
            scale_factor: 1.0,
        };
        core.info_line_shown = true;
        core.info_line_w = 300;
        core.info_line_h = 30;
        let m = core.overlay_margin() as f32;
        // Panel spans for a right-anchored Inspector and a left-anchored tree — narrow
        // columns near the edges, so a short centered line clears both.
        let right_panel = (1000.0 - m - 200.0, 1000.0 - m); // bottom-right, 200px wide
        let left_tree = (m, m + 200.0); // top-left, 200px column

        // Right-aligned line: overlaps the right panel, clears the left tree.
        core.settings.info_line_align = settings::InfoLineAlign::Right;
        assert!(core.info_line_reserve_for(right_panel.0, right_panel.1) > 0);
        assert_eq!(core.info_line_reserve_for(left_tree.0, left_tree.1), 0);

        // Left-aligned line: overlaps the tree, clears the right panel.
        core.settings.info_line_align = settings::InfoLineAlign::Left;
        assert!(core.info_line_reserve_for(left_tree.0, left_tree.1) > 0);
        assert_eq!(core.info_line_reserve_for(right_panel.0, right_panel.1), 0);

        // Narrow, short centered line: reaches neither corner.
        core.settings.info_line_align = settings::InfoLineAlign::Center;
        core.info_line_w = 200;
        assert_eq!(core.info_line_reserve_for(left_tree.0, left_tree.1), 0);
        assert_eq!(core.info_line_reserve_for(right_panel.0, right_panel.1), 0);

        // The narrow-window case the owner flagged: a wide centered line (a long
        // filename spanning most of the width) overlaps BOTH corner panels.
        core.info_line_w = 900;
        assert!(
            core.info_line_reserve_for(left_tree.0, left_tree.1) > 0,
            "a wide centered line reaches the left tree"
        );
        assert!(
            core.info_line_reserve_for(right_panel.0, right_panel.1) > 0,
            "…and the right panel too"
        );

        // Hidden line reserves nothing regardless of alignment.
        core.info_line_shown = false;
        assert_eq!(core.info_line_reserve_for(left_tree.0, left_tree.1), 0);
        assert_eq!(core.info_line_reserve_for(right_panel.0, right_panel.1), 0);
    }

    #[test]
    fn show_image_text_toggles_the_panel_mode() {
        let mut core = test_core();
        core.dispatch_action(Action::ShowImageText);
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Text),
            "T opens the Inspector on Text"
        );
        core.dispatch_action(Action::ShowImageText);
        assert_eq!(core.panels.inspector, None, "T again closes it");
        // The basic `i` line is now fully independent (task #54 decouple): pressing
        // `i` while the Text panel is open turns the line on WITHOUT closing the
        // panel — they coexist, the line sitting below the panel.
        core.dispatch_action(Action::ShowImageText);
        core.dispatch_action(Action::Info);
        assert!(core.info_line, "i turns the line on");
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Text),
            "…and the Text panel stays open — no longer mutually exclusive"
        );
        assert_eq!(core.slot_content(), Some(SlotContent::Text));
    }

    #[test]
    fn tab_hides_and_panel_toggles_reveal() {
        let mut core = test_core();
        core.dispatch_action(Action::TogglePanels);
        assert!(!core.panels.hidden, "Tab with nothing open is a no-op");
        core.dispatch_action(Action::ShowImageText);
        core.dispatch_action(Action::TogglePanels);
        assert!(core.panels.hidden, "Tab hides…");
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Text),
            "…without closing"
        );
        assert_eq!(core.slot_content(), None, "hidden panels draw nothing");
        // T while hidden reveals and keeps the panel open — never closes.
        core.dispatch_action(Action::ShowImageText);
        assert!(!core.panels.hidden);
        assert_eq!(core.panels.inspector, Some(InspectorTab::Text));
        // `hidden` is one master flag shared with the basic line (task #54 follow-up):
        // `i` while Tab-hidden follows the same reveal rule as `T`/Help/tree — it
        // reveals everything (not just the line) and only ever ends up shown.
        core.dispatch_action(Action::TogglePanels);
        assert!(core.panels.hidden);
        core.dispatch_action(Action::Info);
        assert!(core.info_line, "i turns the line on…");
        assert!(
            !core.panels.hidden,
            "…and reveals the rest too — same shared flag as Tab"
        );
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Text),
            "the Text panel comes back with it"
        );
    }

    #[test]
    fn describe_image_toggles_the_panel_mode() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        // Pre-cache so `D` is a pure toggle (no worker/network kicked).
        core.descriptions
            .insert(0, Ok("A cat on a sofa.".to_string()));
        core.dispatch_action(Action::DescribeImage);
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Describe),
            "D opens the Inspector on Describe"
        );
        core.dispatch_action(Action::DescribeImage);
        assert_eq!(core.panels.inspector, None, "D again closes it");
        // The basic line is independent — `i` while Describe is open turns the line
        // on and the panel stays (task #54 decouple).
        core.dispatch_action(Action::DescribeImage);
        core.dispatch_action(Action::Info);
        assert!(core.info_line);
        assert_eq!(core.panels.inspector, Some(InspectorTab::Describe));
        assert_eq!(core.slot_content(), Some(SlotContent::Describe));
    }

    /// A door reports its **size** where a photo reports dimensions — its frame is a 1×1
    /// sentinel, so `1 × 1` would be the alternative. The size rides `PhotoMeta` from the
    /// scan worker precisely because this runs on the frame path.
    #[test]
    fn the_info_line_reports_a_size_for_a_door_and_dimensions_for_a_photo() {
        let core = test_core();
        let door = crate::meta::PhotoMeta {
            rel: "album.zip".to_string(),
            w: 1,
            h: 1,
            size: Some(271_000_000),
            codec: "ZIP",
            animated: None,
            recovered: None,
        };
        let parts = core.info_line_parts(&door);
        assert!(parts.contains(&"271 MB".to_string()), "{parts:?}");
        assert!(
            !parts.iter().any(|p| p.contains('×')),
            "never print 1 × 1: {parts:?}"
        );

        let photo = crate::meta::PhotoMeta {
            rel: "a.jpg".to_string(),
            w: 4032,
            h: 3024,
            size: None,
            codec: "JPEG",
            animated: None,
            recovered: None,
        };
        let parts = core.info_line_parts(&photo);
        assert!(parts.contains(&"4032×3024".to_string()), "{parts:?}");
    }
    #[test]
    fn a_failed_decode_surfaces_a_cant_display_error_in_details() {
        let mut core = test_core();
        core.source = photos_named(&["dead.jpg"]);
        core.playlist = Playlist::new(1, 0);
        core.displayed_item = Some(0);
        // The state after every rung of the ladder failed: item in `failed`, its reason
        // recorded, and `current` cleared by `present_failed`.
        core.failed.insert(0);
        core.failed_reason.insert(0, "No more bytes".into());
        core.current = None;

        assert_eq!(core.current_decode_error(), "No more bytes");
        let rows = core.details_panel().rows;
        assert!(
            rows.iter().any(|r| matches!(r,
                DetailRow::Pair { label, value }
                    if label == "Error" && value.contains("No more bytes"))),
            "a failed file must show a can't-display Error row; got {rows:?}"
        );

        // A healthy displayed item reports no error (the placeholder stays hidden).
        core.failed.remove(&0);
        core.failed_reason.remove(&0);
        assert_eq!(core.current_decode_error(), "");
    }

    #[test]
    fn native_open_suppresses_the_hud_and_signals() {
        let mut core = test_core(); // headless → empty source
        core.native_open = true;
        assert!(
            core.open_panel_visible(),
            "an empty deck shows the native welcome surface"
        );
        // show_open_hint must not rasterize a HUD panel (so its buttons are never
        // hit-tested beneath a native panel — the cursor fix).
        core.show_open_hint();
        // A tick signals the host on the visibility transition.
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "the empty-state visibility change signals the host"
        );
        // With native_open off (winit), the same deck is not a native-visible panel.
        core.native_open = false;
        assert!(!core.open_panel_visible());
    }
}
