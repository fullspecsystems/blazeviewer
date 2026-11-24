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
    /// the Inspector — unless the panel has nothing for what's on screen
    /// ([`describe_is_stale`](Self::describe_is_stale)), when it describes instead; while
    /// `Tab`-hidden it reveals (never closes).
    pub fn describe_image(&mut self) {
        let was_showing = self.slot_content() == Some(SlotContent::Describe);
        if !(was_showing && self.describe_is_stale()) {
            self.panels.toggle_inspector(InspectorTab::Describe);
            if was_showing {
                self.refresh_slot(); // closed it
                return;
            }
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

    /// Whether the open Describe panel has nothing for what's on screen, so `D` should
    /// describe rather than close it: a description (or one in flight) of a **different
    /// frame** — a video or animation that played on since (owner report 2026-11-14, #81;
    /// the Mac Inspector has no other "describe again" control) — or the idle "Press D to
    /// describe this image." invitation. A cached error for this frame is not stale: `D`
    /// closes it as before, and reopening retries.
    fn describe_is_stale(&self) -> bool {
        let Some(item) = self.displayed_item else {
            return false;
        };
        let key = self.frame_key(item);
        if let Some(scan) = self.describe_scan.as_ref().filter(|s| s.item == item) {
            return scan.frame != key;
        }
        match self.descriptions.get(&item) {
            None => true, // the idle "Press D" invitation
            // Keyed like `ensure_describe_scan`'s cache check (absent = the still).
            Some(_) => {
                self.frame_capture
                    .described
                    .get(&item)
                    .copied()
                    .unwrap_or_default()
                    != key
            }
        }
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
    ///
    /// On a video or a playing animation it describes **the frame on screen** (task #81); a
    /// cached description is reused only for that same frame. (The automatic run — the
    /// opt-in auto-describe with the panel open — waits for playback to stop: the tick
    /// checks [`frame_is_moving`](Self::frame_is_moving) before calling this.)
    pub(super) fn ensure_describe_scan(&mut self, prompt_override: Option<String>) {
        let Some(item) = self.displayed_item else {
            return;
        };
        let key = self.frame_key(item);
        let cached_key = self
            .frame_capture
            .described
            .get(&item)
            .copied()
            .unwrap_or_default();
        if (self.descriptions.contains_key(&item) && cached_key == key)
            || self
                .describe_scan
                .as_ref()
                .is_some_and(|s| s.item == item && s.frame == key)
        {
            return;
        }
        // A description of a different frame is stale: drop it so the panel shows the
        // describing state, not the old frame's text.
        self.descriptions.remove(&item);
        let describer = match self.describer_from_settings() {
            Ok(describer) => describer,
            Err(why) => {
                self.descriptions.insert(item, Err(why));
                self.frame_capture.described.insert(item, key);
                return;
            }
        };
        let prompt = prompt_override.unwrap_or_else(|| self.default_describe_prompt(item));
        let frame = self.frame_target(item);
        let gen = self.describe_gen;
        let source = Arc::clone(&self.source);
        // Bake the in-RAM rotation override: the model should see the pixels upright.
        let rot = self.capture_rotation(item, &frame);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::describe::describe_job(
                source.as_ref(),
                item,
                &frame,
                rot,
                &prompt,
                describer.as_ref(),
            ));
        });
        self.describe_scan = Some(crate::describe::DescribeScan {
            gen,
            item,
            frame: key,
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
                Ok(result) => Some((s.gen, s.item, s.frame, s.copy_when_done, result)),
                Err(TryRecvError::Empty) => return, // still describing
                Err(TryRecvError::Disconnected) => None, // worker died
            }
        };
        self.describe_scan = None;
        let Some((gen, item, frame, copy, result)) = outcome else {
            return;
        };
        if gen != self.describe_gen {
            return; // deck rebuilt while describing — stale indices
        }
        // Store the description, or the backend error as a one-line user message.
        self.descriptions
            .insert(item, result.map_err(|e| e.user_message()));
        self.frame_capture.described.insert(item, frame);
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

    /// Build the describer from settings (task #44, the owner-accepted policy):
    ///
    /// - **Auto** → the host's on-device model ([`AppCore::host_describer`] — Apple's on
    ///   macOS 27+) when it's available, since it is private by construction and needs no
    ///   setup; else the configured endpoint.
    /// - **Apple on-device** → only the host's model; never falls through to a server.
    /// - **Local endpoint** → only the endpoint.
    ///
    /// `Err` is the one-line message the panel shows instead (no backend, or why Apple's
    /// model can't be used right now).
    fn describer_from_settings(&self) -> Result<Box<dyn crate::describe::Describer>, String> {
        use crate::describe::{Describer, LocalEndpoint, OnHost};
        use crate::settings::DescribeBackend;
        const NO_BACKEND: &str = "No description backend is set up (Settings ▸ AI Descriptions).";
        let on_host = |host: &Arc<dyn crate::describe::HostDescriber>| -> Box<dyn Describer> {
            Box::new(OnHost {
                host: Arc::clone(host),
                max_tokens: self.settings.describe_max_tokens,
            })
        };
        let endpoint = || -> Option<Box<dyn Describer>> {
            let url = self.settings.describe_endpoint.trim();
            (!url.is_empty()).then(|| -> Box<dyn Describer> {
                Box::new(LocalEndpoint::new(
                    url.to_string(),
                    self.settings.describe_model.clone(),
                    self.settings.describe_max_tokens,
                ))
            })
        };
        let host = self.host_describer.as_ref();
        match self.settings.describe_backend {
            DescribeBackend::AppleOnDevice => match host {
                Some(h) => h.availability().map(|()| on_host(h)),
                None => Err(
                    "Apple's on-device model needs a Mac with macOS 27 or later and \
                             Apple Intelligence."
                        .to_string(),
                ),
            },
            DescribeBackend::LocalEndpoint => endpoint().ok_or_else(|| NO_BACKEND.to_string()),
            DescribeBackend::Auto => {
                let host_status = host.map(|h| (h, h.availability()));
                if let Some((h, Ok(()))) = host_status {
                    return Ok(on_host(h));
                }
                endpoint().ok_or_else(|| match host_status {
                    // Say why the on-device model is out, rather than only "set something up".
                    Some((_, Err(why))) => {
                        format!("{why} Or set up a model server in Settings ▸ AI Descriptions.")
                    }
                    _ => NO_BACKEND.to_string(),
                })
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
            frame: Default::default(),
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

    /// A stand-in for Apple's on-device model: `availability` as given; `describe` answers
    /// "on-device" and records the length cap it was handed.
    struct FakeHost {
        availability: Result<(), String>,
        asked_max_tokens: std::sync::Mutex<Option<u32>>,
    }

    impl crate::describe::HostDescriber for FakeHost {
        fn availability(&self) -> Result<(), String> {
            self.availability.clone()
        }
        fn describe(
            &self,
            _image: &crate::describe::PreparedImage,
            _prompt: &str,
            max_tokens: u32,
        ) -> Result<String, crate::describe::DescribeError> {
            *self.asked_max_tokens.lock().unwrap() = Some(max_tokens);
            Ok("on-device".to_string())
        }
    }

    fn with_host(availability: Result<(), String>) -> (AppCore, Arc<FakeHost>) {
        let mut core = test_core();
        let host = Arc::new(FakeHost {
            availability,
            asked_max_tokens: Default::default(),
        });
        core.host_describer = Some(host.clone());
        // A configured endpoint nothing listens on: refused at once, so a test can tell
        // which backend the policy picked without a network round trip.
        core.settings.describe_endpoint = "http://127.0.0.1:1/v1".to_string();
        (core, host)
    }

    /// Run whichever describer the policy picked on an empty image.
    fn describe_with_policy(core: &AppCore) -> Result<Result<String, DescribeError>, String> {
        let image = crate::describe::PreparedImage { jpeg: Vec::new() };
        core.describer_from_settings()
            .map(|d| d.describe(&image, "Describe it."))
    }

    use crate::describe::DescribeError;
    use crate::settings::DescribeBackend;

    #[test]
    fn auto_prefers_the_on_device_model_when_it_is_available() {
        let (mut core, host) = with_host(Ok(()));
        core.settings.describe_max_tokens = 1024;
        assert_eq!(describe_with_policy(&core), Ok(Ok("on-device".to_string())));
        assert_eq!(
            *host.asked_max_tokens.lock().unwrap(),
            Some(1024),
            "the length preset reaches the on-device model"
        );
    }

    #[test]
    fn auto_falls_back_to_the_endpoint_when_the_on_device_model_is_out() {
        let (core, _) = with_host(Err("Apple Intelligence is turned off.".to_string()));
        assert_eq!(
            describe_with_policy(&core),
            Ok(Err(DescribeError::Unreachable)),
            "the endpoint was tried"
        );
        // …and with no endpoint either, the panel says why the on-device model is out.
        let (mut core, _) = with_host(Err("Apple Intelligence is turned off.".to_string()));
        core.settings.describe_endpoint = String::new();
        let why = describe_with_policy(&core).unwrap_err();
        assert!(
            why.starts_with("Apple Intelligence is turned off."),
            "{why}"
        );
        assert!(why.contains("model server"), "{why}");
    }

    #[test]
    fn the_apple_backend_never_falls_through_to_a_server() {
        let (mut core, _) = with_host(Err("The model is still downloading.".to_string()));
        core.settings.describe_backend = DescribeBackend::AppleOnDevice;
        assert_eq!(
            describe_with_policy(&core),
            Err("The model is still downloading.".to_string())
        );
        // No host at all (Windows, Linux, or a hand-edited settings file).
        let mut core = test_core();
        core.settings.describe_backend = DescribeBackend::AppleOnDevice;
        let why = describe_with_policy(&core).unwrap_err();
        assert!(why.contains("macOS 27"), "{why}");
        // Available: it's used.
        let (mut core, _) = with_host(Ok(()));
        core.settings.describe_backend = DescribeBackend::AppleOnDevice;
        assert_eq!(describe_with_policy(&core), Ok(Ok("on-device".to_string())));
    }

    #[test]
    fn the_endpoint_backend_ignores_the_on_device_model() {
        let (mut core, _) = with_host(Ok(()));
        core.settings.describe_backend = DescribeBackend::LocalEndpoint;
        assert_eq!(
            describe_with_policy(&core),
            Ok(Err(DescribeError::Unreachable))
        );
    }

    /// A core showing one video, paused at `at` on the AVPlayer-style (native) route, with
    /// no endpoint configured so a describe runs synchronously (it caches the setup hint).
    fn paused_video_core(at: std::time::Duration) -> AppCore {
        use crate::video::{VideoSessionId, VideoSessionState as S};
        use crate::video_native::{ActiveVideoBackend, NativeVideoProxy};
        let mut core = test_core();
        core.settings.describe_endpoint = String::new(); // keep it thread-free
        core.displayed_item = Some(0);
        let id = VideoSessionId(7);
        let mut p = NativeVideoProxy::new(0, id, true);
        p.on_state_changed(id, S::Paused);
        p.set_position(at);
        core.video = Some(ActiveVideoBackend::Native(p));
        core
    }

    fn seek_to(core: &mut AppCore, at: std::time::Duration) {
        match core.video.as_mut() {
            Some(crate::video_native::ActiveVideoBackend::Native(p)) => p.set_position(at),
            _ => unreachable!("paused_video_core installs a native player"),
        }
    }

    /// Owner report 2026-11-14 (#81 follow-up): describe a video frame, play on, and `D`
    /// must describe the frame now on screen — it used to close the panel, and the Mac
    /// Inspector has no other "describe again" control. `D` still closes the panel once it
    /// shows the frame on screen.
    #[test]
    fn d_on_an_open_panel_describes_a_new_frame_instead_of_closing() {
        use crate::frame_capture::FrameKey;
        use std::time::Duration;
        let mut core = paused_video_core(Duration::from_secs(1));
        core.dispatch_action(Action::DescribeImage);
        assert_eq!(core.panels.inspector, Some(InspectorTab::Describe));
        // The 1 s frame is described.
        core.descriptions.insert(0, Ok("A grey frame.".to_string()));
        core.frame_capture
            .described
            .insert(0, FrameKey::Video { millis: 1000 });

        // Play on to 5 s, pause, press D: the panel stays open and the 5 s frame is described.
        seek_to(&mut core, Duration::from_secs(5));
        core.dispatch_action(Action::DescribeImage);
        assert_eq!(
            core.panels.inspector,
            Some(InspectorTab::Describe),
            "D re-describes rather than closing over a stale description"
        );
        assert_eq!(
            core.frame_capture.described.get(&0),
            Some(&FrameKey::Video { millis: 5000 })
        );
        assert!(
            !matches!(core.descriptions.get(&0), Some(Ok(t)) if t == "A grey frame."),
            "the 1 s description isn't shown as the 5 s frame's"
        );

        // Same frame now: D closes as it always has.
        core.dispatch_action(Action::DescribeImage);
        assert_eq!(core.panels.inspector, None);
    }

    /// The open panel inviting "Press D to describe this image." (auto-describe off, a new
    /// item) means `D` describes — it doesn't close the panel that asked for it.
    #[test]
    fn d_on_an_open_idle_panel_describes() {
        let mut core = test_core();
        core.settings.describe_endpoint = String::new();
        core.displayed_item = Some(0);
        core.panels.open_inspector(InspectorTab::Describe);
        assert!(matches!(
            core.describe_panel().body,
            crate::panels::DescribeBody::Idle
        ));
        core.dispatch_action(Action::DescribeImage);
        assert_eq!(core.panels.inspector, Some(InspectorTab::Describe));
        assert!(core.descriptions.contains_key(&0), "the describe ran");
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
