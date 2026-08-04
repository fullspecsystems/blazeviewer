//! **AI describe / ask** — the `AppCore` half of [`crate::describe`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! `describe.rs` holds the endpoint client and the downscale/re-encode step; this file
//! holds the `impl AppCore` methods that arm a description for the displayed item, poll it,
//! and copy the result.
//!
//! ⚠ This is the one network-touching feature in the app. Re-read the Second Directive in
//! the root `CLAUDE.md` before changing what is sent or when: it goes only to the
//! user-configured endpoint, only on an explicit command or the opt-in auto-describe
//! toggle, downscaled and JPEG-re-encoded first, and the result is RAM-only.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// **Describe image** (`D`, task #44): toggle the Inspector's Describe tab.
    /// Showing it kicks the vision-model describe for the displayed photo (no-op
    /// when cached / already running); `D` on an already-showing Describe tab closes
    /// the Inspector; while `Tab`-hidden it reveals (never closes).
    pub fn describe_image(&mut self) {
        let was_showing = self.slot_content() == Some(SlotContent::Describe);
        self.panels.toggle_inspector(InspectorTab::Describe);
        if was_showing {
            self.refresh_slot(); // closed it
            return;
        }
        // An explicit `D` retries a previously-failed describe (the endpoint may have come up,
        // or Local Network permission was just granted) — a cached *error* is cleared so the
        // scan re-runs; a cached success stays put (revisits are instant). This is why a
        // failure is never a dead end: press D again.
        if let Some(item) = self.displayed_item {
            if matches!(self.descriptions.get(&item), Some(Err(_))) {
                self.descriptions.remove(&item);
            }
        }
        self.ensure_describe_scan(None); // default (accessibility) prompt
        self.show_overlay();
        self.refresh_info_line_visibility(); // Tab-hidden reveals with the panel
    }

    /// **Ask about image** (`Shift+D`, task #44 subtask 9): open the text-input dialog for a
    /// question about the current photo. The shell collects the (multi-line) text and returns
    /// it as [`contract::DialogResult::AskSubmitted`], which drives [`Self::ask_describe`].
    /// Nothing to ask about on the empty deck.
    pub fn ask_image(&mut self) {
        if self.displayed_item.is_none() {
            self.show_toast("Nothing to ask about");
            return;
        }
        self.effects.push(contract::CoreEffect::ShowDialog(
            contract::DialogKind::AskImage,
        ));
    }

    /// Run a describe for the displayed photo with a caller-supplied question (from the
    /// Ask dialog). Bypasses the general-description cache so each question re-runs, and
    /// shows the answer in the same panel.
    pub fn ask_describe(&mut self, question: String) {
        let q = question.trim().to_string();
        if q.is_empty() {
            return;
        }
        if let Some(item) = self.displayed_item {
            // Force a fresh run for the question, replacing any cached general description.
            self.descriptions.remove(&item);
            if self.describe_scan.as_ref().is_some_and(|s| s.item == item) {
                self.describe_scan = None;
            }
        }
        // Showing an answer never *closes* the panel — open, not toggle.
        self.panels.open_inspector(InspectorTab::Describe);
        self.ensure_describe_scan(Some(q));
        self.show_overlay();
        self.refresh_info_line_visibility(); // Tab-hidden reveals with the panel
    }

    /// Kick the off-thread describe for the displayed photo unless its result is cached or
    /// that same describe is already running. `prompt_override` is the Ask question; `None`
    /// builds the default accessibility prompt from salient EXIF (`prompt::build_prompt`).
    /// A misconfigured backend caches a one-line error rather than a description.
    pub(super) fn ensure_describe_scan(&mut self, prompt_override: Option<String>) {
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.descriptions.contains_key(&item)
            || self.describe_scan.as_ref().is_some_and(|s| s.item == item)
        {
            return;
        }
        let Some(describer) = self.describer_from_settings() else {
            self.descriptions.insert(
                item,
                Err("No description backend is set up (Settings ▸ AI Descriptions).".to_string()),
            );
            return;
        };
        let prompt = prompt_override.unwrap_or_else(|| self.default_describe_prompt(item));
        let gen = self.describe_gen;
        let source = Arc::clone(&self.source);
        // Bake the in-RAM rotation override: the model should see the pixels upright.
        let rot = self.rotations.get(&item).copied().unwrap_or_default();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::describe::describe_job(
                source.as_ref(),
                item,
                rot,
                &prompt,
                describer.as_ref(),
            ));
        });
        self.describe_scan = Some(crate::describe::DescribeScan {
            gen,
            item,
            copy_when_done: false,
            rx,
        });
    }

    /// Pick up a finished describe (called each tick). A result from before a playlist
    /// rebuild is dropped (indices reassigned); a result for an item merely navigated away
    /// from still caches (item-keyed, so a revisit is instant).
    pub fn poll_describe_scan(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let outcome = {
            let Some(s) = self.describe_scan.as_ref() else {
                return;
            };
            match s.rx.try_recv() {
                Ok(result) => Some((s.gen, s.item, s.copy_when_done, result)),
                Err(TryRecvError::Empty) => return, // still describing
                Err(TryRecvError::Disconnected) => None, // worker died
            }
        };
        self.describe_scan = None;
        let Some((gen, item, copy, result)) = outcome else {
            return;
        };
        if gen != self.describe_gen {
            return; // deck rebuilt while describing — stale indices
        }
        // Store the description, or the backend error as a one-line user message.
        self.descriptions
            .insert(item, result.map_err(|e| e.user_message()));
        if self.slot_content() == Some(SlotContent::Describe) && self.displayed_item == Some(item) {
            self.show_overlay();
        }
        // A deferred Copy AI description: copy the text now, or toast the error.
        if copy {
            match self.descriptions.get(&item) {
                Some(Ok(text)) => {
                    self.effects.push(contract::CoreEffect::WriteClipboard(
                        contract::ClipboardPayload::Text {
                            text: text.clone(),
                            toast: Some("Copied description".to_string()),
                        },
                    ));
                }
                Some(Err(msg)) => {
                    let msg = msg.clone();
                    self.show_toast(&msg);
                }
                None => {}
            }
        }
    }

    /// Build the describer from settings. Apple Foundation Models (Auto-when-available /
    /// AppleOnDevice) is delegated to the Swift host via a `CoreEffect` (subtask 5), so on
    /// this build the local endpoint is the only in-core backend and `Auto` resolves to it.
    /// `None` when no endpoint is configured — the caller surfaces the "set up a backend"
    /// message.
    fn describer_from_settings(&self) -> Option<Box<dyn crate::describe::Describer>> {
        use crate::settings::DescribeBackend;
        match self.settings.describe_backend {
            // Apple-only with no FM host wired yet → nothing in-core can serve it.
            DescribeBackend::AppleOnDevice => None,
            DescribeBackend::Auto | DescribeBackend::LocalEndpoint => {
                let url = self.settings.describe_endpoint.trim();
                if url.is_empty() {
                    return None;
                }
                Some(Box::new(crate::describe::LocalEndpoint::new(
                    url.to_string(),
                    self.settings.describe_model.clone(),
                    self.settings.describe_max_tokens,
                )))
            }
        }
    }

    /// The default describe prompt for `item`: salient EXIF + filename/folder framed as
    /// unverified (`prompt::build_prompt`), honoring a `describe_prompt` custom template.
    fn default_describe_prompt(&mut self, item: usize) -> String {
        self.ensure_exif_cached(item);
        let name = self.source.name(item).to_string();
        let exif: &[(String, String)] = self
            .exif_cache
            .get(&item)
            .map(|d| d.fields.as_slice())
            .unwrap_or(&[]);
        // No calendar clock in the pure core → skip future-date filtering (the epoch-default
        // junk filter still applies); a stray future date is harmless (metadata is unverified).
        let ctx = crate::prompt::build_context(&name, exif, None);
        crate::prompt::build_prompt(&ctx, self.settings.describe_prompt.as_deref())
    }

    /// **Copy AI description** (Edit / context menu, task #44): put the current photo's
    /// description on the clipboard. Uses the cached description when present; otherwise
    /// kicks the describe off-thread and copies when it lands (`copy_when_done`, the
    /// Copy-Text-from-Image shape). A cached *error* is cleared and retried (conditions may
    /// have changed). An explicit user command; the result is RAM-only (privacy #2).
    pub fn copy_description(&mut self) {
        let Some(item) = self.displayed_item else {
            self.show_toast("Nothing to copy");
            return;
        };
        if let Some(Ok(text)) = self.descriptions.get(&item) {
            self.effects.push(contract::CoreEffect::WriteClipboard(
                contract::ClipboardPayload::Text {
                    text: text.clone(),
                    toast: Some("Copied description".to_string()),
                },
            ));
            return;
        }
        // No usable description yet (never generated, or a stale error) — generate one and
        // copy when it lands.
        self.descriptions.remove(&item);
        self.ensure_describe_scan(None);
        match self.describe_scan.as_mut() {
            Some(scan) if scan.item == item => {
                scan.copy_when_done = true;
                self.show_toast("Describing…");
            }
            // No backend → `ensure_describe_scan` cached the setup-hint error instead of
            // spawning; surface it rather than leaving the user without feedback.
            _ => {
                if let Some(Err(msg)) = self.descriptions.get(&item) {
                    let msg = msg.clone();
                    self.show_toast(&msg);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{clipboard_text_effects, test_core};

    fn feed_describe(
        core: &mut AppCore,
        item: usize,
        r: Result<String, crate::describe::DescribeError>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(r).unwrap();
        core.describe_scan = Some(crate::describe::DescribeScan {
            gen: core.describe_gen,
            item,
            copy_when_done: false,
            rx,
        });
    }

    /// ⛔ **Privacy (Second Directive), task #137.** A generation prompt must never
    /// reach the describe context. Describe can post the image and its context to a
    /// **user-configured endpoint** — so leaking the prompt here would ship the
    /// user's own (often personal) prompt text to a server as a silent byproduct of
    /// pressing `D`, with no opt-in and no warning.
    ///
    /// `build_context` happens to read only `ItemDetails::fields` today, so this
    /// passes by construction — which is exactly why it needs a test. That is an
    /// accident of the current code, not a guarantee, and the obvious "improvement"
    /// of feeding richer metadata to the describer would silently break it.
    #[test]
    fn a_generation_prompt_never_reaches_the_describe_prompt() {
        use crate::genmeta::{GenTool, GenerationMeta};
        let mut core = test_core();
        core.displayed_item = Some(0);
        // Seed the cache directly: `ensure_exif_cached` returns early when the
        // entry exists, so this is the item's metadata as far as describe is
        // concerned — no filesystem needed.
        let mut gen = GenerationMeta {
            tool: GenTool::ComfyUI,
            positive: None,
            negative: None,
            model: Some("SECRETMODEL".into()),
            loras: Vec::new(),
            params: vec![("Seed".into(), "SECRETSEED".into())],
            passes: Vec::new(),
            has_payload: true,
        };
        gen.positive = Some(crate::genmeta::PromptText {
            text: Some("SECRETPROMPT a very private thing".into()),
            source: crate::genmeta::PromptSource::Literal,
        });
        core.exif_cache.insert(
            0,
            crate::app_core::ItemDetails::ready(10, vec![("Make".into(), "Canon".into())])
                .with_gen(Some(gen)),
        );

        let prompt = core.default_describe_prompt(0);
        for secret in ["SECRETPROMPT", "SECRETMODEL", "SECRETSEED"] {
            assert!(
                !prompt.contains(secret),
                "generation metadata leaked into the describe prompt ({secret}): {prompt}"
            );
        }
        // The ordinary EXIF path still works, so this is a real boundary rather
        // than an empty context that would pass vacuously.
        assert!(
            prompt.contains("Canon"),
            "the EXIF camera fact should still reach the describer: {prompt}"
        );
    }

    #[test]
    fn describe_result_caches_by_item() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_describe(&mut core, 0, Ok("A red bicycle.".to_string()));
        core.poll_describe_scan();
        assert!(core.describe_scan.is_none(), "job consumed");
        assert_eq!(core.descriptions[&0].as_deref(), Ok("A red bicycle."));
    }

    #[test]
    fn describe_result_from_before_a_rebuild_is_dropped() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_describe(&mut core, 0, Ok("stale".to_string()));
        core.describe_gen += 1; // deck rebuilt while describing
        core.poll_describe_scan();
        assert!(
            core.descriptions.is_empty(),
            "stale-generation result must not cache under a recycled index"
        );
    }

    #[test]
    fn describe_backend_error_caches_a_one_line_user_message() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        feed_describe(
            &mut core,
            0,
            Err(crate::describe::DescribeError::Unreachable),
        );
        core.poll_describe_scan();
        let msg = core.descriptions[&0].as_ref().unwrap_err();
        assert!(msg.contains("model server"), "actionable message: {msg}");
    }

    #[test]
    fn copy_description_defers_the_copy_until_the_describe_lands() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        core.settings.describe_endpoint = "http://localhost:1234/v1".to_string();
        // Nothing cached → the copy arms `copy_when_done` on the in-flight scan.
        core.dispatch_action(Action::CopyDescription);
        assert!(
            core.describe_scan
                .as_ref()
                .is_some_and(|s| s.copy_when_done),
            "copy is deferred to the scan result"
        );
        // Simulate the result landing.
        feed_describe(&mut core, 0, Ok("A late description.".to_string()));
        core.describe_scan.as_mut().unwrap().copy_when_done = true;
        core.poll_describe_scan();
        let got = clipboard_text_effects(&core);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "A late description.");
        assert_eq!(got[0].1.as_deref(), Some("Copied description"));
    }

    #[test]
    fn ask_describe_bypasses_the_general_description_cache() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        core.settings.describe_endpoint = String::new(); // keep it thread-free
        core.descriptions
            .insert(0, Ok("old general description".to_string()));
        core.ask_describe("What year is this?".to_string());
        // The cached general description was dropped and a fresh run attempted (which,
        // with no endpoint, resolves to the setup hint rather than the stale text).
        assert!(
            core.descriptions[&0].is_err(),
            "the question re-ran instead of returning the cached description"
        );
        assert_eq!(core.panels.inspector, Some(InspectorTab::Describe));
    }
}
