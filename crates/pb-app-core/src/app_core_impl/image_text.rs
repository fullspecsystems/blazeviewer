//! **Text in image (OCR)** — the `AppCore` half of [`crate::image_text`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! `image_text.rs` holds the recognition backends; this file holds the `impl AppCore`
//! methods that arm a scan for the displayed item, poll it, and copy what it found.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// **Copy Text from Image** (Edit / context menu, task #45): put the text
    /// recognized *in* the displayed photo — on-device OCR lines plus QR-code
    /// payloads — on the clipboard. Uses the cached scan when present; otherwise
    /// kicks the off-thread scan and copies when it lands (`copy_when_done`). An
    /// explicit user command; the scan never leaves the machine and the result is
    /// RAM-only (privacy #2).
    pub fn copy_image_text(&mut self) {
        let Some(item) = self.displayed_item else {
            self.show_toast("Nothing to copy");
            return;
        };
        if self.recognized_text.contains_key(&item) {
            self.copy_recognized(item);
            return;
        }
        self.ensure_text_scan();
        if let Some(scan) = self.text_scan.as_mut() {
            if scan.item == item {
                scan.copy_when_done = true;
                // Feedback that a scan is running; the result toast replaces it.
                self.show_toast("Reading text…");
            }
        }
    }

    /// **Show text in image** (`T`, task #45): toggle the Inspector's Text tab.
    /// Opens the Inspector on Text, switches to Text if it's open elsewhere, closes
    /// it if Text is already showing; while `Tab`-hidden it reveals (never closes).
    pub fn toggle_image_text(&mut self) {
        self.panels.toggle_inspector(InspectorTab::Text);
        self.refresh_slot();
    }

    /// Kick the off-thread text scan for the displayed photo unless its result is
    /// already cached or that same scan is already in flight. Replacing a stale
    /// in-flight scan (another item's) drops its receiver — the worker's send fails
    /// and its thread exits quietly. Decode + OCR + QR all run on the worker; the
    /// event loop never blocks.
    pub(super) fn ensure_text_scan(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.recognized_text.contains_key(&item)
            || self.text_scan.as_ref().is_some_and(|s| s.item == item)
        {
            return;
        }
        let gen = self.text_gen;
        let source = Arc::clone(&self.source);
        // Bake the in-RAM rotation override: OCR wants the pixels upright as shown.
        let rot = self.rotations.get(&item).copied().unwrap_or_default();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::image_text::scan_job(source.as_ref(), item, rot));
        });
        self.text_scan = Some(crate::image_text::TextScan {
            gen,
            item,
            copy_when_done: false,
            rx,
        });
    }

    /// Pick up a finished text scan (called each tick). A result from before a
    /// playlist rebuild is dropped — the indices were reassigned — but a result for
    /// an item the user merely navigated away from still caches (it's item-keyed, so
    /// the revisit is instant).
    pub fn poll_text_scan(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let outcome = {
            let Some(s) = self.text_scan.as_ref() else {
                return;
            };
            match s.rx.try_recv() {
                Ok(result) => Some((s.gen, s.item, s.copy_when_done, result)),
                Err(TryRecvError::Empty) => return, // still scanning
                Err(TryRecvError::Disconnected) => None, // worker died
            }
        };
        self.text_scan = None;
        let Some((gen, item, copy, result)) = outcome else {
            return;
        };
        if gen != self.text_gen {
            return; // deck rebuilt while scanning — stale indices
        }
        self.recognized_text.insert(item, result);
        // The `T` panel may be sitting on its "Reading text…" state for this item.
        if self.slot_content() == Some(SlotContent::Text) && self.displayed_item == Some(item) {
            self.show_overlay();
        }
        if copy {
            self.copy_recognized(item);
        }
    }

    /// Push a cached scan result to the clipboard seam with its specific toast
    /// ("Copied 214 characters" / "Copied text + 1 QR code"), or toast why there is
    /// nothing to copy.
    fn copy_recognized(&mut self, item: usize) {
        let Some(r) = self.recognized_text.get(&item) else {
            return;
        };
        if r.is_empty() {
            let msg = r
                .ocr_error
                .clone()
                .unwrap_or_else(|| "No text found".to_string());
            self.show_toast(&msg);
            return;
        }
        self.effects.push(contract::CoreEffect::WriteClipboard(
            contract::ClipboardPayload::Text {
                text: r.clipboard_text(),
                toast: Some(r.copy_toast()),
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{clipboard_text_effects, test_core, text_result};

    /// Install an in-flight scan whose result is already sitting in the channel.
    fn feed_scan(core: &mut AppCore, item: usize, copy: bool, r: crate::image_text::ImageText) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(r).unwrap();
        core.text_scan = Some(crate::image_text::TextScan {
            gen: core.text_gen,
            item,
            copy_when_done: copy,
            rx,
        });
    }

    #[test]
    fn text_scan_result_caches_by_item() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_scan(&mut core, 0, false, text_result(&[], &["Hello"]));
        core.poll_text_scan();
        assert!(core.text_scan.is_none(), "job consumed");
        assert_eq!(core.recognized_text[&0].lines, vec!["Hello"]);
        assert!(
            clipboard_text_effects(&core).is_empty(),
            "no copy was requested"
        );
    }

    #[test]
    fn a_result_from_before_a_rebuild_is_dropped() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_scan(&mut core, 0, false, text_result(&[], &["stale"]));
        core.text_gen += 1; // the deck was rebuilt while the scan ran
        core.poll_text_scan();
        assert!(
            core.recognized_text.is_empty(),
            "stale-generation result must not cache under a recycled index"
        );
    }

    #[test]
    fn a_result_for_a_left_item_still_caches_for_the_revisit() {
        let mut core = test_core();
        core.displayed_item = Some(3); // user moved on mid-scan
        feed_scan(&mut core, 0, false, text_result(&[], &["kept"]));
        core.poll_text_scan();
        assert_eq!(
            core.recognized_text[&0].lines,
            vec!["kept"],
            "item-keyed result is still valid — revisits are instant"
        );
    }

    #[test]
    fn copy_requested_mid_scan_copies_when_the_result_lands() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_scan(&mut core, 0, true, text_result(&[], &["late"]));
        core.poll_text_scan();
        let got = clipboard_text_effects(&core);
        assert_eq!(got.len(), 1, "deferred copy fired on landing");
        assert_eq!(got[0].0, "late");
    }

    #[test]
    fn an_empty_scan_result_never_writes_the_clipboard() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_scan(&mut core, 0, true, text_result(&[], &[]));
        core.poll_text_scan();
        assert!(
            clipboard_text_effects(&core).is_empty(),
            "nothing found → toast only, no clipboard write"
        );
    }

    #[test]
    fn rebuild_playlist_drops_text_results_and_bumps_the_generation() {
        let mut core = test_core();
        core.recognized_text.insert(0, text_result(&[], &["old"]));
        let gen = core.text_gen;
        let dir = std::env::temp_dir();
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![dir.join("a.png")]));
        core.rebuild_playlist(source, dir.clone(), Some(dir), true, 0);
        assert!(core.recognized_text.is_empty());
        assert!(core.text_gen > gen);
    }
}
