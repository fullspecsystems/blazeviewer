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
                        DetailRow::Span { text, .. } => text,
                        DetailRow::Pair { label, value } => format!("{label}: {value}"),
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
}
