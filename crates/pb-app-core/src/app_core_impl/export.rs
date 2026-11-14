//! **Image export** — the `AppCore` half (task #89, rung 1; plan of record:
//! `.taskmaster/docs/export-plan.md`). The subsystem is [`crate::export`]; the worker
//! [`crate::export_worker`]. Paired in one commit per the two-halves rule.
//!
//! Flow: `Action::Export` → freeze an [`ExportRequest`] → suggest a name off the event loop
//! → `CoreEffect::ShowSavePanel` → the shell's native dialog → [`AppCore::save_panel_resolved`]
//! → the write worker → [`AppCore::poll_export`] lands the result (toast, remembered
//! folder). The loop polls export work at a low rate ([`EXPORT_POLL`]) — never via
//! `work_pending`, which would hold the frame pump hot for seconds (review F8).

use std::sync::mpsc::TryRecvError;

use super::*;
use crate::export::{self, ExportRequest, InFlight, Suggesting};
use crate::export_worker::{ExportDone, ExportJob, ExportWorker};

/// How often the loop wakes while export work is outstanding.
pub(crate) const EXPORT_POLL: Duration = Duration::from_millis(50);

impl AppCore {
    /// Whether File ▸ Export… is available: a still image is on screen — not a video (until
    /// task #81's frame capture), not an archive door, not the empty deck.
    pub fn can_export(&self) -> bool {
        self.displayed_item.is_some_and(|item| {
            match crate::video::item_kind(self.source.as_ref(), item) {
                crate::video::LibraryItemKind::Image => true,
                // The frame on screen (task #81) — when this build can decode one.
                crate::video::LibraryItemKind::Video(_) => crate::frame_capture::CAN_CAPTURE_VIDEO,
                crate::video::LibraryItemKind::Archive(_) => false,
            }
        })
    }

    /// `Action::Export` (File ▸ Export…, `E`): freeze the request and ask the shell for a
    /// destination once a name suggestion is ready.
    pub fn export_cmd(&mut self) {
        let Some(item) = self.displayed_item else {
            return; // empty deck
        };
        let is_video = match crate::video::item_kind(self.source.as_ref(), item) {
            crate::video::LibraryItemKind::Image => false,
            crate::video::LibraryItemKind::Video(_) => {
                if !crate::frame_capture::CAN_CAPTURE_VIDEO {
                    self.show_toast("Exporting video frames isn't supported in this build");
                    return;
                }
                true
            }
            crate::video::LibraryItemKind::Archive(_) => {
                self.show_toast("Open the archive to export its images");
                return;
            }
        };
        // The frame on screen, frozen now (task #81): a video's playback frame (or its
        // poster), an animation's current frame. A photo's still re-decodes the file.
        let frame = self.frame_target(item);
        let frame = (is_video || !frame.is_still()).then_some(frame);
        let suffix = frame.as_ref().map(|f| f.name_suffix()).unwrap_or_default();
        let id = self.export.next_id();
        let source_path = self.source.path(item).map(Path::to_path_buf);
        let container = self.source.container().map(Path::to_path_buf);
        let rotation = self.rotations.get(&item).copied().unwrap_or_default();
        let request = ExportRequest {
            id,
            source: Arc::clone(&self.source),
            item,
            name: self.source.name(item).to_string(),
            source_path: source_path.clone(),
            rotation_cw: export::quarter_turns(rotation),
            frame,
        };
        let remembered = export::source_folder(source_path.as_deref(), container.as_deref())
            .and_then(|key| self.export.folder_memory.get(&key).cloned());
        let start_dir = export::start_dir(
            remembered.as_deref(),
            self.settings.picker_dir.as_deref(),
            source_path.as_deref(),
            container.as_deref(),
            &crate::engine::default_picker_dir(),
        );
        // The existence check runs off the event loop: a sleeping network share can block a
        // `stat` for seconds (review F27). Pending destinations are avoided too.
        let (tx, rx) = std::sync::mpsc::channel();
        let pending = self.export.pending_destinations();
        let dir = start_dir.clone();
        // A frame's name carries its time or index: `clip-0h01m23s.png`, `anim-frame12.png`.
        let name = if suffix.is_empty() {
            request.name.clone()
        } else {
            let file = crate::engine::file_name_of(&request.name);
            let stem = Path::new(&file)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or(file);
            format!("{stem}{suffix}.png")
        };
        let _ = std::thread::Builder::new()
            .name("pb-export-name".into())
            .spawn(move || {
                let suggestion = export::suggested_name(&name, |candidate| {
                    let p = dir.join(candidate);
                    pending.contains(&p) || p.exists()
                });
                let _ = tx.send(suggestion);
            });
        // A failed spawn drops `tx`; `poll_export` then falls back to the plain stem.
        self.export.suggesting.push(Suggesting {
            request,
            start_dir,
            rx,
        });
    }

    /// The shell's save dialog closed. `path` is `None` (or empty) when cancelled. Stale or
    /// unknown request ids are ignored.
    pub fn save_panel_resolved(&mut self, request_id: u64, path: Option<PathBuf>) {
        let Some(request) = self.export.awaiting_dialog.remove(&request_id) else {
            return;
        };
        let Some(path) = path.filter(|p| !p.as_os_str().is_empty()) else {
            return; // cancelled
        };
        let dest = export::ensure_png_extension(path);
        if self.export.pending_destinations().contains(&dest) {
            self.show_toast("Another export is already writing that file");
            return;
        }
        let file_name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let source_folder =
            export::source_folder(request.source_path.as_deref(), request.source.container());
        let job = ExportJob {
            id: request.id,
            source: request.source,
            item: request.item,
            name: request.name,
            source_path: request.source_path,
            rotation_cw: request.rotation_cw,
            frame: request.frame,
            dest: dest.clone(),
            queued_at: std::time::Instant::now(),
        };
        let worker = self.export.worker.get_or_insert_with(ExportWorker::new);
        if !worker.submit(job) {
            self.show_toast("Export failed");
            return;
        }
        self.export.in_flight.insert(
            request_id,
            InFlight {
                dest,
                file_name: file_name.clone(),
                source_folder,
            },
        );
        self.show_toast(&format!("Exporting {file_name}…"));
    }

    /// Land ready name suggestions (→ the save dialog) and finished writes (→ a toast and
    /// the remembered folder). Called every tick; cheap when idle.
    pub fn poll_export(&mut self) {
        let mut i = 0;
        while i < self.export.suggesting.len() {
            let ready = match self.export.suggesting[i].rx.try_recv() {
                Ok(name) => Some(name),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(export::suggested_name(
                    &self.export.suggesting[i].request.name,
                    |_| false,
                )),
            };
            match ready {
                Some(suggested_name) => {
                    let s = self.export.suggesting.remove(i);
                    let request_id = s.request.id;
                    self.export.awaiting_dialog.insert(request_id, s.request);
                    self.effects.push(contract::CoreEffect::ShowSavePanel {
                        request_id,
                        start_dir: s.start_dir,
                        suggested_name,
                    });
                }
                None => i += 1,
            }
        }
        let done: Vec<ExportDone> = self
            .export
            .worker
            .as_ref()
            .map(ExportWorker::drain)
            .unwrap_or_default();
        for d in done {
            self.finish_export(d);
        }
    }

    fn finish_export(&mut self, done: ExportDone) {
        let Some(flight) = self.export.in_flight.remove(&done.id) else {
            return;
        };
        if self.perf.enabled() {
            // Durations only — never a path (privacy #2).
            let t = done.timings;
            eprintln!(
                "[perf] export->file: queue {:.1}ms read {:.1}ms decode {:.1}ms prepare {:.1}ms encode {:.1}ms write {:.1}ms",
                t.queue.as_secs_f64() * 1e3,
                t.read.as_secs_f64() * 1e3,
                t.decode.as_secs_f64() * 1e3,
                t.prepare.as_secs_f64() * 1e3,
                t.encode.as_secs_f64() * 1e3,
                t.write.as_secs_f64() * 1e3,
            );
        }
        match done.result {
            Ok(outcome) => {
                let msg = if outcome
                    .notes
                    .contains(&pb_encode::ExportNote::HdrToneMapped)
                {
                    format!("Exported {} (HDR as SDR)", flight.file_name)
                } else {
                    format!("Exported {}", flight.file_name)
                };
                // The native shell draws a clickable notice (Show in Finder); the HUD shows
                // the same words as a toast — File ▸ Show Last Export reveals it there.
                if self.native_toast {
                    let now = self.now;
                    let item = self.displayed_item;
                    self.export.show_notice(msg, done.dest.clone(), item, now);
                } else {
                    self.show_toast(&msg);
                }
                self.export.last_export = Some(done.dest.clone());
                self.remember_export_folder(flight.source_folder, done.dest.parent());
            }
            Err(why) => {
                eprintln!("export failed: {why}");
                self.show_toast(&format!("Export failed — {why}"));
            }
        }
    }

    /// Remember where an export from `source` (its folder key) went, for this session —
    /// RAM only, so nothing about where you export outlives the process (§ 3.6, 2026-11-14).
    fn remember_export_folder(&mut self, source: Option<PathBuf>, dir: Option<&Path>) {
        let (Some(source), Some(dir)) = (source, dir.filter(|d| !d.as_os_str().is_empty())) else {
            return;
        };
        self.export.folder_memory.insert(source, dir.to_path_buf());
    }

    /// File ▸ Show Last Export in Finder/Explorer — and the export notice's click: reveal
    /// this session's most recent export in the file manager, and dismiss the notice.
    pub fn reveal_last_export(&mut self) {
        self.export.notice = None;
        match self.export.last_export.clone() {
            Some(path) => self.effects.push(contract::CoreEffect::RevealPath(path)),
            None => self.show_toast("Nothing exported yet"),
        }
    }

    /// The pointer entered/left the export notice (macOS): hovering holds it open.
    pub fn export_notice_hover(&mut self, hovering: bool) {
        let now = self.now;
        self.export.hover_notice(hovering, now);
    }

    /// Expire the export notice (called each tick).
    pub fn tick_export_notice(&mut self) {
        let (now, displayed) = (self.now, self.displayed_item);
        self.export.tick_notice(now, displayed);
    }

    /// At quit: give a confirmed export up to `timeout` to land rather than silently dropping
    /// it (review F11). The window is already on its way down; nothing new is queued. Pending
    /// dialogs and suggestions are simply abandoned.
    pub fn finish_exports_for_exit(&mut self, timeout: Duration) {
        self.export.suggesting.clear();
        self.export.awaiting_dialog.clear();
        let deadline = std::time::Instant::now() + timeout;
        while !self.export.in_flight.is_empty() && std::time::Instant::now() < deadline {
            let done: Vec<ExportDone> = self
                .export
                .worker
                .as_ref()
                .map(ExportWorker::drain)
                .unwrap_or_default();
            for d in done {
                // Quitting: nothing to remember (the folder memory dies with the process).
                self.export.in_flight.remove(&d.id);
            }
            if !self.export.in_flight.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::test_core;

    /// Drive the core until the export settles (suggestion → dialog effect, or a finished
    /// write), with a real-time budget — the worker is a real thread.
    fn pump_until(core: &mut AppCore, mut done: impl FnMut(&AppCore) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !done(core) {
            assert!(std::time::Instant::now() < deadline, "export never settled");
            core.poll_export();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn take_save_panel(core: &mut AppCore) -> Option<(u64, PathBuf, String)> {
        let i = core
            .effects
            .iter()
            .position(|e| matches!(e, contract::CoreEffect::ShowSavePanel { .. }))?;
        match core.effects.remove(i) {
            contract::CoreEffect::ShowSavePanel {
                request_id,
                start_dir,
                suggested_name,
            } => Some((request_id, start_dir, suggested_name)),
            _ => unreachable!(),
        }
    }

    fn sandbox(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pb-export-core-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_jpeg(path: &Path, w: u32, h: u32) {
        let rgba = vec![180u8; (w * h * 4) as usize];
        std::fs::write(path, pb_encode::encode_jpeg_rgba8(&rgba, w, h, 90).unwrap()).unwrap();
    }

    fn snapshot(dir: &Path) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().unwrap().len(),
                )
            })
            .collect();
        v.sort();
        v
    }

    /// The right-click menu offers "Export as PNG…" exactly when the File menu would: a
    /// still, a video when this build can capture its frame (#81), never an archive door.
    #[test]
    fn the_context_menu_offers_export_only_for_exportable_items() {
        let lib = sandbox("ctx");
        write_jpeg(&lib.join("a.jpg"), 4, 4);
        std::fs::write(lib.join("b.mp4"), b"").unwrap();
        std::fs::write(lib.join("c.zip"), b"").unwrap();
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![
            lib.join("a.jpg"),
            lib.join("b.mp4"),
            lib.join("c.zip"),
        ]));
        let offered = |core: &mut AppCore, item: usize| {
            core.displayed_item = Some(item);
            core.effects.clear();
            core.show_context_menu();
            match core.effects.pop() {
                Some(contract::CoreEffect::ShowContextMenu(s)) => {
                    assert_eq!(s.can_export, core.can_export(), "menu bar parity");
                    s.can_export
                }
                _ => panic!("no context menu"),
            }
        };
        assert!(offered(&mut core, 0), "a still");
        assert_eq!(
            offered(&mut core, 1),
            crate::frame_capture::CAN_CAPTURE_VIDEO,
            "a video"
        );
        assert!(!offered(&mut core, 2), "an archive door");
        let _ = std::fs::remove_dir_all(&lib);
    }

    #[test]
    fn nothing_to_export_on_an_empty_deck() {
        let mut core = test_core();
        assert!(!core.can_export());
        core.export_cmd();
        assert!(!core.export.busy());
        assert!(take_save_panel(&mut core).is_none());
    }

    /// The core-level no-trace + export test that runs on every platform (export-plan.md
    /// rung 0c; the pb-app no-trace test is Windows/Linux-only): viewing writes nothing,
    /// one export writes exactly one file at the chosen path, no temp is left, and the
    /// rotation is baked in.
    #[test]
    fn viewing_writes_nothing_and_one_export_writes_exactly_one_file() {
        let lib = sandbox("lib");
        let out = sandbox("out");
        write_jpeg(&lib.join("a.jpg"), 8, 4);
        write_jpeg(&lib.join("b.jpg"), 8, 4);
        let mut core = test_core();
        core.persist_prefs = false;
        core.source = Arc::new(pb_source::FsSource::new(vec![
            lib.join("a.jpg"),
            lib.join("b.jpg"),
        ]));
        core.displayed_item = Some(0);
        let lib_before = snapshot(&lib);

        // Rotate 90° in RAM, then export.
        core.rotations.insert(0, pb_render::Rotation::R90);
        assert!(core.can_export());
        core.export_cmd();
        // Navigating / rotating after the command must not change the frozen request.
        core.rotations.insert(0, pb_render::Rotation::R0);
        core.displayed_item = Some(1);
        pump_until(&mut core, |c| {
            c.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ShowSavePanel { .. }))
        });
        let (id, start_dir, name) = take_save_panel(&mut core).unwrap();
        assert_eq!(start_dir, lib, "nothing remembered → beside the photo");
        assert_eq!(name, "a.png");
        assert_eq!(
            snapshot(&lib),
            lib_before,
            "the command itself wrote nothing"
        );

        core.save_panel_resolved(id, Some(out.join("a.png")));
        assert!(core.export.busy());
        pump_until(&mut core, |c| !c.export.busy());

        assert_eq!(snapshot(&lib), lib_before, "the library is untouched");
        let files = snapshot(&out);
        assert_eq!(files.len(), 1, "exactly one file, no temp left: {files:?}");
        assert_eq!(files[0].0, "a.png");
        let png = pb_decode::decode_named_bytes(
            "a.png",
            &std::fs::read(out.join("a.png")).unwrap(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            (png.width, png.height),
            (4, 8),
            "the frozen 90° rotation is baked in"
        );
        // Where it went is remembered for this folder, in RAM only, and it is the last export.
        assert_eq!(core.export.folder_memory.get(&lib), Some(&out));
        assert_eq!(core.export.last_export, Some(out.join("a.png")));
        let _ = std::fs::remove_dir_all(&lib);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// The dialog starts beside the photo; a second export from the **same** folder goes
    /// back where the first went; another folder starts beside its own photo (or the pin).
    #[test]
    fn the_start_folder_is_remembered_per_source_folder_for_the_session() {
        let lib1 = sandbox("mem-lib1");
        let lib2 = sandbox("mem-lib2");
        let out = sandbox("mem-out");
        write_jpeg(&lib1.join("a.jpg"), 4, 4);
        write_jpeg(&lib1.join("b.jpg"), 4, 4);
        write_jpeg(&lib2.join("c.jpg"), 4, 4);
        let mut core = test_core();
        core.persist_prefs = false;
        core.source = Arc::new(pb_source::FsSource::new(vec![
            lib1.join("a.jpg"),
            lib1.join("b.jpg"),
            lib2.join("c.jpg"),
        ]));
        let dialog_for = |core: &mut AppCore, item: usize| {
            core.displayed_item = Some(item);
            core.export_cmd();
            pump_until(core, |c| {
                c.effects
                    .iter()
                    .any(|e| matches!(e, contract::CoreEffect::ShowSavePanel { .. }))
            });
            take_save_panel(core).unwrap()
        };
        let (id, start, _) = dialog_for(&mut core, 0);
        assert_eq!(start, lib1, "beside the photo");
        core.save_panel_resolved(id, Some(out.join("a.png")));
        pump_until(&mut core, |c| !c.export.busy());
        let (id, start, _) = dialog_for(&mut core, 1);
        assert_eq!(start, out, "same folder → where its last export went");
        core.save_panel_resolved(id, None); // cancel
        let (id, start, _) = dialog_for(&mut core, 2);
        assert_eq!(start, lib2, "another folder starts beside its own photo");
        core.save_panel_resolved(id, None);
        // A pinned "Open files in" folder beats beside-the-photo, not this folder's memory.
        core.settings.picker_dir = Some(out.join("pin"));
        let (id, start, _) = dialog_for(&mut core, 2);
        assert_eq!(start, out.join("pin"));
        core.save_panel_resolved(id, None);
        let (_, start, _) = dialog_for(&mut core, 0);
        assert_eq!(start, out);
        for d in [&lib1, &lib2, &out] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// File ▸ Show Last Export reveals this session's last export (and clears the notice).
    #[test]
    fn show_last_export_reveals_it() {
        let mut core = test_core();
        core.reveal_last_export();
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::RevealPath(_))),
            "nothing exported yet"
        );
        let p = PathBuf::from("/out/a.png");
        core.export.last_export = Some(p.clone());
        let now = core.now;
        core.export
            .show_notice("Exported a.png".into(), p.clone(), None, now);
        core.reveal_last_export();
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::RevealPath(x) if *x == p)));
        assert!(core.export.notice.is_none());
        // Any other command dismisses a notice; Show Last Export itself is the exception.
        core.export
            .show_notice("Exported a.png".into(), p, None, now);
        core.dispatch_action(crate::action::Action::ScaleFit);
        assert!(core.export.notice.is_none());
    }

    #[test]
    fn cancel_and_stale_ids_do_nothing_and_existing_names_are_avoided() {
        let lib = sandbox("cancel");
        write_jpeg(&lib.join("c.jpg"), 4, 4);
        std::fs::write(lib.join("c.png"), b"already here").unwrap();
        let mut core = test_core();
        core.persist_prefs = false;
        core.source = Arc::new(pb_source::FsSource::new(vec![lib.join("c.jpg")]));
        core.displayed_item = Some(0);
        core.export_cmd();
        pump_until(&mut core, |c| {
            c.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ShowSavePanel { .. }))
        });
        let (id, _, name) = take_save_panel(&mut core).unwrap();
        assert_eq!(name, "c-1.png", "an existing c.png is not offered");
        core.save_panel_resolved(id + 99, Some(lib.join("x.png"))); // stale id
        core.save_panel_resolved(id, None); // cancelled
        assert!(!core.export.busy());
        assert!(!lib.join("x.png").exists());
        let _ = std::fs::remove_dir_all(&lib);
    }
}
