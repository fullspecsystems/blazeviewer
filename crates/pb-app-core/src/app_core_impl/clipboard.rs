//! **Clipboard and reveal** — the `AppCore` methods behind Copy Image, Copy Path, Copy
//! Details and Reveal in Finder/Explorer (task #125).
//!
//! A *topic*, not a subsystem: these have no owned state and no logic module of their own,
//! just a handful of `AppCore` methods that gather what the user asked for and hand it to a
//! shell effect. `docs/where-code-goes.md` allows exactly this — a topic gets an
//! `app_core_impl/` file without inventing a sibling module to pair with.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Copy the current photo to the OS clipboard (`Ctrl+C` / Edit ▸ Copy, task #27).
    ///
    /// Decodes the original at **full resolution** here — not the fit-downscaled ring
    /// texture — so a paste lands at native size. This is a synchronous decode on the
    /// event-loop thread, which is fine: Copy is an explicit, infrequent user command
    /// (like the modal file picker), not the nav hot path. Any in-RAM rotation
    /// override is baked into the copied pixels so the clipboard is WYSIWYG.
    pub fn copy_image(&mut self) {
        let Some(item) = self.displayed_item else {
            return; // empty state — nothing to copy
        };
        let img = match decode_item(self.source.as_ref(), item, None, false) {
            Ok(img) => img,
            Err(e) => {
                eprintln!("copy: decode failed: {}: {e}", self.source.name(item));
                self.show_toast("Copy failed");
                return;
            }
        };
        let rgba = to_clipboard_rgba8(&img);
        let rot = self.rotations.get(&item).copied().unwrap_or_default();
        let (rgba, w, h) = rotate_rgba8(&rgba, img.width, img.height, rot);
        // Offer the source file as CF_HDROP too when there is one; an archive entry
        // has no file on disk, so it gets an image-only copy (pixels still paste). The
        // pure decode + rotate prep stays here; the platform write is the shell's job.
        let file = self.source.path(item).map(|p| p.to_path_buf());
        self.effects.push(contract::CoreEffect::WriteClipboard(
            contract::ClipboardPayload::Image { rgba, w, h, file },
        ));
    }

    /// Copy the current photo's **file path** to the clipboard as text (Shift+Ctrl+C /
    /// Edit ▸ Copy File Path; ⇧⌘C on macOS). The full path for a filesystem source, or
    /// the entry name for an archive (which has no path on disk). An explicit user
    /// command — never the view path. Uses the cross-platform text clipboard (arboard),
    /// separate from the image clipboard (`clipboard.rs`).
    pub fn copy_path(&mut self) {
        let Some(item) = self.displayed_item else {
            return; // empty state — nothing to copy
        };
        let text = match self.source.path(item) {
            Some(p) => p.to_string_lossy().into_owned(),
            None => self.source.name(item).to_string(),
        };
        // A specific toast: it copies the **full path**, so "Copied file path" (not the bare
        // file name the shell's single-line fallback would show, which read as a name copy).
        self.effects.push(contract::CoreEffect::WriteClipboard(
            contract::ClipboardPayload::Text {
                text,
                toast: Some("Copied file path".to_string()),
            },
        ));
    }

    /// Whether **Show in Finder/Explorer** is available: the displayed photo is a real
    /// file on disk (not an archive entry, not the empty deck). Drives the File-menu
    /// item's enabled state (`menu_state_from` / `apply_menu_state`), mirroring
    /// [`can_save_rotation`](Self::can_save_rotation).
    pub fn can_reveal(&self) -> bool {
        self.displayed_item
            .is_some_and(|item| self.source.path(item).is_some())
    }

    /// **Show in Finder/Explorer** (File menu): reveal the displayed photo in the OS file
    /// manager — its containing folder open, the file selected. Only a real on-disk file can
    /// be revealed; an archive entry or the empty deck toasts instead. An explicit user
    /// command that only launches the file manager on a path already being viewed — no pixel
    /// read, no persistent trace (privacy #2, same category as Copy File Path). The platform
    /// launch is the shell's job (`CoreEffect::RevealPath`).
    pub fn reveal_in_file_manager(&mut self) {
        let path = self.displayed_item.and_then(|item| self.source.path(item));
        match path {
            Some(p) => self
                .effects
                .push(contract::CoreEffect::RevealPath(p.to_path_buf())),
            None => self.show_toast("Nothing to reveal"),
        }
    }

    /// **Copy EXIF data** (context menu): copy the displayed photo's metadata to the
    /// clipboard as text — the same facts the full-EXIF panel shows (filename, dimensions,
    /// codec, exact byte size, and every non-blob EXIF tag), read on-demand from RAM
    /// (privacy #2). Unlike the panel, this is *not* truncated to the screen — it copies the
    /// full set. The platform clipboard write is the shell's job (`WriteClipboard`).
    pub fn copy_image_details(&mut self) {
        let Some(item) = self.displayed_item else {
            self.show_toast("Nothing to copy");
            return;
        };
        self.ensure_exif_cached(item);
        // A video's container probe may be in flight (task 98.6). Copying now would hand
        // the user a table missing its Duration/codec/track rows, so mark the probe
        // copy-when-done and let `poll_details_probe` re-enter — the same contract
        // `copy_image_text` uses for a scan that hasn't finished.
        if self
            .exif_cache
            .get(&item)
            .is_some_and(|d| d.probe_state == crate::media_details::ProbeState::Loading)
        {
            if let Some(p) = self.details_probe.as_mut().filter(|p| p.item == item) {
                p.copy_when_done = true;
                self.show_toast("Reading video details…");
                return;
            }
        }
        let mut lines: Vec<String> = vec![file_name_of(self.source.name(item)).to_string()];
        if let Some(meta) = &self.current {
            lines.push(format!("Dimensions: {} × {}", meta.w, meta.h));
            lines.push(format!("Codec: {}", meta.codec.to_uppercase()));
        }
        if let Some(details) = self.exif_cache.get(&item) {
            lines.push(format!(
                "File Size: {} bytes",
                hud::format_thousands(details.size)
            ));
            // The Generation block (task #137), from the same builder the panel
            // uses — this is a separate copy path, so sharing the derivation is
            // what stops the two disagreeing about the same file, including the
            // order the rows come in.
            if let Some(gen) = &details.gen {
                for row in crate::genmeta::detail_rows(gen) {
                    lines.push(match row {
                        DetailRow::Span { text, .. } | DetailRow::Section { text, .. } => text,
                        DetailRow::Pair { label, value } => format!("{label}: {value}"),
                        DetailRow::Note { label, text } => format!("{label}: {text}"),
                        DetailRow::Body { text } => text,
                    });
                }
            }
            for (tag, val) in &details.fields {
                // Skip binary blobs (Apple MakerNote/Padding) that render as meaningless hex.
                if is_exif_blob(tag, val) {
                    continue;
                }
                lines.push(format!("{tag}: {val}"));
            }
            // The audio/subtitle tracks (task #98) — the same rows the panel shows, so
            // "Copy Image Details" and the panel can't disagree about the same file.
            if let Some(catalog) = &details.media {
                for row in crate::tracks::track_rows(catalog, details.has_audio) {
                    lines.push(match row {
                        DetailRow::Span { text, .. } | DetailRow::Section { text, .. } => text,
                        DetailRow::Pair { label, value } => format!("{label}: {value}"),
                        DetailRow::Note { label, text } => format!("{label}: {text}"),
                        // Track rows never produce these, but the match stays
                        // exhaustive so a new row kind is a compile error here
                        // rather than a silently dropped line.
                        DetailRow::Body { text } => text,
                    });
                }
            }
        }
        // Only the filename line means there was nothing worth copying.
        if lines.len() <= 1 {
            self.show_toast("No EXIF data");
            return;
        }
        self.effects.push(contract::CoreEffect::WriteClipboard(
            contract::ClipboardPayload::Text {
                text: lines.join("\n"),
                toast: Some("Copied details".to_string()),
            },
        ));
    }

    /// **Copy generation prompt** (task #137): the positive prompt of an AI-generated
    /// image, which is the one line people actually want to paste.
    ///
    /// Refuses *with the reason* when the workflow assembled the prompt instead of
    /// storing it — there is genuinely no text to give, and a silent "nothing
    /// copied" would read as a bug rather than as a fact about the file.
    pub fn copy_generation_prompt(&mut self) {
        let Some(item) = self.displayed_item else {
            self.show_toast("Nothing to copy");
            return;
        };
        self.ensure_exif_cached(item);
        let Some(gen) = self.exif_cache.get(&item).and_then(|d| d.gen.as_ref()) else {
            self.show_toast("No generation metadata");
            return;
        };
        match gen.positive.as_ref() {
            Some(p) => match (&p.text, p.unresolved_reason()) {
                (Some(text), _) => {
                    let text = text.clone();
                    self.effects.push(contract::CoreEffect::WriteClipboard(
                        contract::ClipboardPayload::Text {
                            text,
                            toast: Some("Copied prompt".to_string()),
                        },
                    ));
                }
                (None, Some(why)) => self.show_toast(&format!("No prompt: {why}")),
                (None, None) => self.show_toast("No prompt in this image"),
            },
            None => self.show_toast("No prompt in this image"),
        }
    }

    /// **Copy generation data** (task #137): the raw payload — a ComfyUI workflow
    /// graph or an Automatic1111 parameters block — **byte-for-byte**, so it pastes
    /// back into the tool that made it.
    ///
    /// Two deliberate choices:
    ///
    /// - **Verbatim, never re-serialized.** Pretty-printing would reorder keys and
    ///   change bytes; the point of this command is fidelity, and a round-trip that
    ///   is merely equivalent is not what someone reloading a workflow wants.
    /// - **Re-read from the file.** The parsed facts are cached, the payload is not:
    ///   holding 24 KB of graph per item would cost ~140 MB across a 5,000-image
    ///   deck for something almost never asked for. This is an explicit, infrequent
    ///   command, so paying one read here is the right side of that trade.
    ///
    /// Prefers the UI `workflow` graph (what ComfyUI's Load accepts) over the API
    /// `prompt` graph, then the A1111 `parameters` block.
    pub fn copy_generation_data(&mut self) {
        let Some(item) = self.displayed_item else {
            self.show_toast("Nothing to copy");
            return;
        };
        self.ensure_exif_cached(item);
        if self
            .exif_cache
            .get(&item)
            .and_then(|d| d.gen.as_ref())
            .is_none()
        {
            self.show_toast("No generation metadata");
            return;
        }
        let Ok(bytes) = self.source.bytes(item) else {
            self.show_toast("Can't read this file");
            return;
        };
        let chunks = pb_decode::read_png_text(&bytes);
        let pick = |key: &str| {
            chunks
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.to_string())
        };
        let payload = pick("workflow")
            .or_else(|| pick("prompt"))
            .or_else(|| pick("parameters"))
            .or_else(|| pb_decode::read_exif_user_comment(&bytes));
        match payload {
            Some(text) => self.effects.push(contract::CoreEffect::WriteClipboard(
                contract::ClipboardPayload::Text {
                    text,
                    toast: Some("Copied generation data".to_string()),
                },
            )),
            None => self.show_toast("No generation metadata"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{clipboard_text_effects, test_core};

    /// A PNG carrying one `tEXt` chunk, valid enough for the real extractor.
    fn png_with_text(keyword: &str, text: &str) -> Vec<u8> {
        fn crc32(kind: &[u8], data: &[u8]) -> u32 {
            let mut crc = 0xFFFF_FFFFu32;
            for &b in kind.iter().chain(data) {
                crc ^= b as u32;
                for _ in 0..8 {
                    let mask = (crc & 1).wrapping_neg();
                    crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
                }
            }
            !crc
        }
        fn push(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            out.extend_from_slice(&crc32(kind, data).to_be_bytes());
        }
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        push(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        let mut chunk = keyword.as_bytes().to_vec();
        chunk.push(0);
        chunk.extend_from_slice(text.as_bytes());
        push(&mut png, b"tEXt", &chunk);
        push(
            &mut png,
            b"IDAT",
            &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
        );
        push(&mut png, b"IEND", &[]);
        png
    }

    /// Seed a one-item deck backed by a real file on disk, so the copy commands
    /// exercise the actual read → extract → parse path.
    fn core_with_png(tag: &str, keyword: &str, text: &str) -> (AppCore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("pb-gencopy-{}-{tag}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("img.png");
        std::fs::write(&path, png_with_text(keyword, text)).unwrap();
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![path.clone()]));
        core.playlist = pb_core::Playlist::new(1, 0);
        core.displayed_item = Some(0);
        // The HUD toast is a rasterized pill, so its text is unreadable in a test.
        // The native path keeps the message as data — use it to assert on wording.
        core.native_toast = true;
        (core, path)
    }

    fn toast_text(core: &AppCore) -> Option<String> {
        core.toast_native.as_ref().map(|t| t.message.clone())
    }

    #[test]
    fn copy_generation_prompt_copies_only_the_positive_text() {
        let (mut core, path) = core_with_png(
            "prompt",
            "parameters",
            "a red bird on a wire\nNegative prompt: blurry\nSteps: 20, Seed: 7",
        );
        core.dispatch_action(Action::CopyGenerationPrompt);
        let got = clipboard_text_effects(&core);
        assert_eq!(got.len(), 1);
        // The positive prompt alone — not the negative, not the params. This is
        // the paste-into-a-generator payload.
        assert_eq!(got[0].0, "a red bird on a wire");
        assert_eq!(got[0].1.as_deref(), Some("Copied prompt"));
        let _ = std::fs::remove_file(&path);
    }

    /// An unresolvable prompt must refuse **with the reason**. A silent "nothing
    /// copied" reads as a broken command rather than as a fact about the file.
    #[test]
    fn copy_generation_prompt_refuses_an_unresolved_prompt_with_the_reason() {
        let graph = r#"{
          "1": {"class_type": "PromptCombinator", "inputs": {"input_list_1": "girl"}},
          "2": {"class_type": "CLIPTextEncode", "inputs": {"text": ["1", 0]}},
          "3": {"class_type": "EmptyLatentImage", "inputs": {"width": 64, "height": 64}},
          "4": {"class_type": "KSampler", "inputs": {"seed": 1, "steps": 10,
                "positive": ["2", 0], "negative": ["2", 0], "latent_image": ["3", 0]}},
          "5": {"class_type": "SaveImage", "inputs": {"images": ["4", 0]}}
        }"#;
        let (mut core, path) = core_with_png("unres", "prompt", graph);
        core.dispatch_action(Action::CopyGenerationPrompt);
        assert!(
            clipboard_text_effects(&core).is_empty(),
            "nothing may be copied when there is no prompt to copy"
        );
        let toast = toast_text(&core).expect("a toast explaining why");
        assert!(
            toast.contains("PromptCombinator"),
            "the refusal must name the cause: {toast}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The payload copies **byte-for-byte**. Re-serializing would reorder keys and
    /// change bytes, and the whole point of this command is that the result loads
    /// back into the tool that wrote it.
    #[test]
    fn copy_generation_data_is_verbatim() {
        let graph = "{\"1\":  {\"class_type\":\"SaveImage\",   \"inputs\": {} }}";
        let (mut core, path) = core_with_png("verbatim", "prompt", graph);
        core.dispatch_action(Action::CopyGenerationData);
        let got = clipboard_text_effects(&core);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, graph, "the payload must not be reformatted");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_generation_commands_say_so_on_an_ordinary_photo() {
        let (mut core, path) = core_with_png("plain", "Software", "some editor");
        for action in [Action::CopyGenerationPrompt, Action::CopyGenerationData] {
            core.toast_native = None;
            core.dispatch_action(action);
            assert!(clipboard_text_effects(&core).is_empty(), "{action:?}");
            assert_eq!(
                toast_text(&core).as_deref(),
                Some("No generation metadata"),
                "{action:?} must explain itself"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn copy_description_uses_the_cache_and_carries_a_toast() {
        let mut core = test_core();
        core.displayed_item = Some(0);
        core.descriptions.insert(0, Ok("A calico cat.".to_string()));
        core.dispatch_action(Action::CopyDescription);
        let got = clipboard_text_effects(&core);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "A calico cat.");
        assert_eq!(got[0].1.as_deref(), Some("Copied description"));
    }
}
