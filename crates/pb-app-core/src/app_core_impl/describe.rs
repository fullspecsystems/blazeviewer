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
