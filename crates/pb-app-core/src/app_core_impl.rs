//! `impl AppCore` — the orchestration methods (NS0 5.5 / Phase B).
//!
//! The ~62 pure-core methods that used to live on the winit `impl App` (nav/prefetch/
//! residency, view zoom/pan/rotate/fit, HUD build, animation playback, undo/misc). They
//! reference only core-owned state (`self.*`, formerly `self.*`) + the relocated
//! `engine` helpers + the engine crates — never winit/muda/egui/rfd. Moved verbatim
//! (behavior-preserving); the shell now calls them as `self.<method>()`.

#![allow(clippy::too_many_arguments)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::collections::HashSet;

use pb_core::{full_ring, prefetch_targets, prefetch_targets_scanning, ResidentRing};
use pb_decode::{read_exif_fields, FitBox};
use pb_render::{test_pattern, Rotation, ScaleMode, ViewTransform, MAX_ZOOM, MIN_ZOOM};
use pb_source::PhotoSource;

use hud::Row;
use pb_hud::{hud, icon};

use crate::animation::{AnimDecode, AnimWant};
use crate::contract;
use crate::decode_pool::Outcome;
use crate::engine::*;
use crate::{slideshow, Action, AppCore, Nav, OpenButton, PlayHint, Toast};

impl AppCore {
    /// Apply a scaling mode (8 = fit, 9 = fill, 0 toggles original ↔ fill). Always
    /// resets zoom/pan back to the mode's natural framing — so tapping a mode key
    /// is also "reset my zoom." Only an actual mode *change* bumps the geometry
    /// epoch and re-buffers neighbors (the decode resolution can change); pressing
    /// the current mode's key just re-frames, no re-decode.
    pub fn set_scale_mode(&mut self, mode: ScaleMode) {
        let changed = self.view.mode != mode;
        self.view.mode = mode;
        self.view.zoom = 1.0;
        self.view.pan = [0.0, 0.0];
        self.push_view();
        if changed {
            self.invalidate_geometry();
            self.load_current_sync();
            self.target_item = self.playlist.current();
            self.request_prefetch();
        } else {
            self.draw();
        }
    }

    /// Grow the playlist in place as a streaming scan delivers more images: swap in the
    /// larger snapshot and extend the cursor's universe **without** resetting the displayed
    /// photo, the cursor, the resident ring, or any per-image cache. The contrast with
    /// [`rebuild_playlist`](App::rebuild_playlist) is the whole point — a fresh open nukes
    /// everything; a *grow* keeps it, because indices are append-only (index `i` is still
    /// the same photo). New neighbours become decodable, so we re-issue prefetch (still the
    /// scanning, anti-thrash variant — the scan isn't done yet), and the title's "X / N"
    /// total ticks up. A no-op if the snapshot isn't actually larger.
    pub fn extend_playlist(&mut self, source: Arc<dyn PhotoSource>) {
        let new_len = source.len();
        if new_len <= self.source.len() {
            return;
        }
        self.source = source;
        self.playlist.extend(new_len);
        self.request_prefetch();
        self.refresh_title();
    }

    /// Recompute the prefetch want-list and hand it to the decode pool. Two tiers:
    /// the whole window is fetched as fast **previews** (HEIC thumbnails etc.) so
    /// scrolling never outruns decode; then, once **settled**, a current-first ring
    /// of the resident window is re-fetched at full resolution and upgraded in place
    /// (see `upgrade_set`). While a nav key is held the upgrade set is empty, so fast
    /// scrolling stays entirely on the cheap preview tier — the parallel decoders
    /// aren't tied up on fulls you fly past. (Pre-libheif this was a single on-screen
    /// full because WIC's HEVC decoder serialized; libheif decodes in parallel, so we
    /// now fill a VRAM-bounded ring of fulls around the cursor.)
    pub fn request_prefetch(&mut self) {
        // While a folder scan is streaming in, the random deck regenerates on every batch,
        // so prefetching the random look-ahead would decode-then-evict photos the user never
        // sees (thrash). Use the sequential-only, no-wrap variant until the scan completes,
        // then normal prefetch (with its random hedges) resumes (`poll_dir_scan` Done arm).
        self.targets = if self.scanning {
            prefetch_targets_scanning(&self.playlist, self.ahead, self.behind)
        } else {
            prefetch_targets(&self.playlist, self.ahead, self.behind)
        };
        let fit = self.decode_fit();
        // Drop tier bookkeeping for items no longer resident (evicted).
        self
            .preview_resident
            .retain(|i| self.ring.slot_for(*i).is_some());
        self
            .upgrade_done
            .retain(|i| self.ring.slot_for(*i).is_some());
        self
            .full_requested_at
            .retain(|i, _| self.ring.slot_for(*i).is_some());
        // Items decoded but not yet uploaded must not be re-requested (the pool no
        // longer tracks them, so it would decode them again).
        let pending: HashSet<usize> = self
            .pending_uploads
            .iter()
            .map(|o| o.key.item)
            .collect();
        let sharpen = self.sharpen_now();
        let ring: HashSet<usize> = self.prefetch_fulls().into_iter().collect();
        // Stamp when each full was first requested, for the `sharpen` latency metric.
        if let Some(d) = sharpen {
            self
                .full_requested_at
                .entry(d)
                .or_insert_with(Instant::now);
        }
        for &t in &ring {
            self
                .full_requested_at
                .entry(t)
                .or_insert_with(Instant::now);
        }

        // Build the job list in three priority tiers (the pool decodes by position):
        //   1. `sharpen` — the on-screen photo's full, so what you're looking at goes
        //      sharp ASAP the moment you park.
        //   2. previews — the whole window, so flying / re-flying is always instant.
        //   3. `ring` fulls — the sharp ring prefetched around the cursor, queued
        //      behind every preview, so a fast fly stays smooth (these decode only in
        //      the pool's spare capacity) and the fulls land ahead of where you're
        //      heading — a stop finds the photo already sharp.
        type Job = (usize, Option<FitBox>, bool);
        let (mut head, mut previews, mut fulls): (Vec<Job>, Vec<Job>, Vec<Job>) =
            (Vec::new(), Vec::new(), Vec::new());
        for &t in &self.targets {
            if self.failed.contains(&t) || pending.contains(&t) {
                continue;
            }
            let resident = self.ring.slot_for(t).is_some();
            let is_prev = resident && self.preview_resident.contains(&t);
            if resident && !is_prev {
                continue; // already full
            }
            if !resident {
                previews.push((t, fit, true));
            } else if Some(t) == sharpen {
                head.push((t, fit, false));
            } else if ring.contains(&t) {
                fulls.push((t, fit, false));
            }
            // else: resident preview not in the ring → leave it as a preview
        }
        let mut jobs = head;
        jobs.append(&mut previews);
        jobs.append(&mut fulls);
        self
            .pool
            .set_targets(self.epoch, &self.source, &jobs);
    }

    /// The decode-to-fit target for the current mode: the display size in Fit mode
    /// (downscale large photos), or full resolution for Fill / Original (so Fill
    /// isn't upscale-blurry and Original is pixel-exact).
    pub fn decode_fit(&self) -> Option<FitBox> {
        match self.view.mode {
            ScaleMode::Fit => self.fit,
            ScaleMode::Fill | ScaleMode::Original => None,
        }
    }

    /// Estimated bytes for one resident ring slot at the current scale mode: the
    /// decode-target box for bounded modes (Fit, and Fill later), or the current
    /// photo's true full-res size for Original. Sizes the ring so VRAM stays in
    /// budget even though full-res textures are much larger than fit ones.
    pub fn slot_bytes_estimate(&self) -> u64 {
        let fit = self.fit.unwrap_or(FitBox {
            max_width: 1,
            max_height: 1,
        });
        let fit_bytes = fit.max_width as u64 * fit.max_height as u64 * 4;
        match self.decode_fit() {
            // Bounded decode (Fit/Fill): a slot is at most the target box.
            Some(b) => (b.max_width as u64 * b.max_height as u64 * 4).max(1),
            // Full-res (Original): estimate from the current photo's true size,
            // never below a fit slot (and clamp_to_max bounds the real extreme).
            None => self
                .current
                .as_ref()
                .map(|m| (m.w as u64 * m.h as u64 * 4).max(fit_bytes))
                .unwrap_or(fit_bytes),
        }
    }

    /// The on-screen photo to sharpen FIRST (top decode priority): the displayed one,
    /// but only when parked (no nav key held) and currently a resident preview with a
    /// better decode to pull. `None` while flying (sharpening a frame that's about to
    /// change is pointless) and `None` once it's already full.
    pub fn sharpen_now(&self) -> Option<usize> {
        if self.held_nav().is_some() {
            return None;
        }
        let d = self.displayed_item?;
        (self.ring.slot_for(d).is_some()
            && self.preview_resident.contains(&d)
            && !self.upgrade_done.contains(&d))
        .then_some(d)
    }

    /// The full-res "sharp ring" to prefetch around the cursor at LOW priority (below
    /// every preview) — a VRAM-bounded, current-first prefix of the window, filtered
    /// to resident previews, minus `sharpen_now` (requested at high priority instead).
    ///
    /// Unlike `sharpen_now`, this runs EVEN WHILE FLYING: the fulls are queued behind
    /// all previews (see `request_prefetch`), so a fast fly stays preview-smooth — the
    /// pool decodes them only in spare capacity. But as you slow down or browse, the
    /// fulls for where you're heading land *ahead* of you, so a stop finds the photo
    /// already sharp instead of paying a cold ~115 ms–1 s decode after the fact. The
    /// workers that decode them would otherwise be idle, so it's near-free.
    pub fn prefetch_fulls(&self) -> Vec<usize> {
        let full_bytes = self.slot_bytes_estimate();
        let sharpen = self.sharpen_now();
        full_ring(
            &self.targets,
            full_bytes,
            RING_BUDGET_BYTES,
            self.ring.capacity().min(MAX_FULL_RING),
        )
        .into_iter()
        .filter(|&i| {
            Some(i) != sharpen
                && self.ring.slot_for(i).is_some()
                && self.preview_resident.contains(&i)
                && !self.upgrade_done.contains(&i)
                && !self.is_raw_item(i)
        })
        .collect()
    }

    /// Whether `item`'s full decode is a slow RAW demosaic (seconds, and once started
    /// it can't be cancelled). Excluded from the speculative ahead-ring so a few RAWs
    /// in the window can't tie up the decode workers — starving the previews a fly
    /// needs — for neighbours you may never visit. The displayed RAW still sharpens
    /// via `sharpen_now`, and a RAW's embedded preview is often near-full-res anyway.
    pub fn is_raw_item(&self, item: usize) -> bool {
        Path::new(self.source.name(item))
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| pb_decode::is_raw_extension(&e.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    /// All items we currently want decoded to full (sharpen + ahead-ring), for the
    /// idle pump's change-detection and "keep ticking while sharpening" gate.
    pub fn fulls_wanted(&self) -> Vec<usize> {
        let mut v = Vec::new();
        if let Some(d) = self.sharpen_now() {
            v.push(d);
        }
        v.extend(self.prefetch_fulls());
        v
    }

    /// Load the per-photo view state for `item`: rotation from the RAM override
    /// map (upright if absent), zoom/pan reset to a fresh framing. Returns the
    /// view to push to the renderer. (Scaling mode is global and left unchanged.)
    pub fn view_for(&mut self, item: usize) -> ViewTransform {
        self.view.rotation = self.rotations.get(&item).copied().unwrap_or_default();
        self.view.zoom = 1.0;
        self.view.pan = [0.0, 0.0];
        self.view
    }

    /// Rotate the on-screen photo 90° clockwise (counter-clockwise on `Shift+R`).
    /// Per-image and RAM-only; returning to upright drops the override entry.
    pub fn rotate(&mut self, ccw: bool) {
        let Some(item) = self.displayed_item else {
            return;
        };
        let cur = self.rotations.get(&item).copied().unwrap_or_default();
        let new = if ccw { cur.ccw() } else { cur.cw() };
        if new == Rotation::default() {
            self.rotations.remove(&item);
        } else {
            self.rotations.insert(item, new);
        }
        self.view.rotation = new;
        self.push_view();
        // Flash a directional rotate icon (icon-only pill) as feedback.
        let ico = if ccw {
            icon::assets::ROTATE_LEFT
        } else {
            icon::assets::ROTATE_RIGHT
        };
        self.show_toast_icon("", Some(ico));
    }

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
        // The shell writes the text and reports the success/failure toast (it can recover
        // the file name from `text` for the "Copied …" message).
        self.effects.push(contract::CoreEffect::WriteClipboard(
            contract::ClipboardPayload::Text(text),
        ));
    }

    /// Show ring `slot` (holding `item`): the keypress fast path — a rebind, no
    /// decode or upload. Updates the pin, title, and info panel.
    pub fn present_item(&mut self, item: usize, slot: usize) {
        // `present` = the whole event-loop-thread cost of one advance (rebind + title +
        // GPU-submit), the keypress fast path. It's the metric to watch for a hold-to-fly
        // regression: the NS0 inversion (renderer behind `Box<dyn Renderer>`, window ops as
        // effects) must leave this flat. `--metrics` only; a no-op branch otherwise.
        let t0 = Instant::now();
        let view = self.view_for(item);
        let title = title_for(self.source.name(item), item, self.source.len());
        if let Some(r) = self.renderer.as_mut() {
            r.set_view(view);
            r.present_slot(slot);
        }
        self.effects.push(contract::CoreEffect::SetTitle(title));
        self.ring.set_displayed(slot);
        // A fresh landing on a *different* photo re-arms the play hint. `anim_hint_shown_for`
        // is keyed to the item and only updated when landing on an animated one — so without
        // this, visiting a non-animated photo in between would leave it latched, and returning
        // to an animated photo (or arriving from a non-animated one) wouldn't re-show the hint.
        // Guarded on the item actually changing, so a re-present of the same photo (e.g. a play
        // reverting to its still) doesn't re-arm it.
        if self.displayed_item != Some(item) {
            self.anim_hint_shown_for = None;
        }
        self.displayed_item = Some(item);
        self.current = self.meta_cache.get(&item).cloned();
        // The panel (if shown) is now stale for the old photo; `about_to_wait`
        // rebuilds it for `item` next tick (or hides it while flying), so it
        // tracks the photo with no blank flash. The bitmap stays up meanwhile.
        self.last_present = Some(Instant::now());
        self.draw();
        self.metrics.record("present", t0.elapsed());
    }

    /// A target that failed to decode (corrupt/unreadable): count it as "shown"
    /// so the gated advance isn't stuck on it, but clear the previous frame's
    /// stale metadata — set a decode-error window title and drop the info panel so
    /// neither misreports the held-over pixels as the failed photo. The previous
    /// frame stays up rather than flashing black.
    pub fn present_failed(&mut self, item: usize) {
        self.displayed_item = Some(item);
        self.current = None;
        let name = file_name_of(self.source.name(item));
        let total = self.source.len();
        self.effects.push(contract::CoreEffect::SetTitle(format!(
            "{name} ({}/{total}) - decode error",
            item + 1
        )));
        // The info panel belonged to the previous photo — drop it (and redraw to
        // remove it). Only touch the renderer if a panel was actually showing.
        if self.overlay_shown {
            if let Some(r) = self.renderer.as_mut() {
                r.set_overlay(None, 0);
            }
            self.overlay_shown = false;
            self.overlay_item = None;
            self.draw();
        }
    }

    /// Try to show `target_item`: present it on a ring hit, otherwise keep the
    /// previous frame (a miss is a hold, never a skip). Returns whether shown.
    pub fn try_present_target(&mut self) -> bool {
        let Some(item) = self.target_item else {
            return false;
        };
        if self.displayed_item == Some(item) {
            return true;
        }
        if self.failed.contains(&item) {
            // Known-bad file: count it as shown (the previous frame stays up) so
            // navigation never stalls on a corrupt prefetched JPEG.
            self.present_failed(item);
            return true;
        }
        if let Some(slot) = self.ring.slot_for(item) {
            self.present_item(item, slot);
            true
        } else {
            false
        }
    }

    /// Drain finished decodes: discard stale/duplicate results, handle decode
    /// errors, then upload the highest-priority ready images (**current target
    /// first**) into ring slots — at most `UPLOADS_PER_TICK` per tick so a burst
    /// can't blow the frame budget. Lower-priority leftovers are stashed for the
    /// next tick (so the target never waits behind neighbors), keeping their pool
    /// byte-budget reservation as backpressure.
    pub fn drain_results(&mut self) {
        // Gather everything ready plus last tick's leftovers, dropping stale /
        // duplicate / errored results so only live decoded images remain.
        let mut ready: Vec<Outcome> = std::mem::take(&mut self.pending_uploads);
        while let Ok(o) = self.results.try_recv() {
            ready.push(o);
        }
        let mut target_failed: Option<usize> = None;
        ready.retain(|o| {
            if o.key.epoch != self.epoch {
                return false; // stale geometry
            }
            let item = o.key.item;
            let resident = self.ring.slot_for(item).is_some();
            if let Err(ref e) = o.result {
                if resident {
                    // A full-upgrade decode failed, but the resident preview is fine
                    // — keep it and stop retrying the upgrade.
                    self.upgrade_done.insert(item);
                    return false;
                }
                eprintln!("decode failed for item {item}: {e}");
                self.failed.insert(item);
                // Unstick the gated loop: a corrupt target counts as "shown".
                // (Deferred out of the closure — `present_failed` needs &mut self.)
                if self.target_item == Some(item) {
                    target_failed = Some(item);
                }
                return false;
            }
            if resident {
                // Already resident. The only outcome we still want is a *full*
                // decode upgrading a resident preview (uploaded in place below). A
                // preview-only upgrade result (e.g. RAW whose only image is its
                // preview) is marked done here so the idle pass stops retrying —
                // otherwise the upgrade loops forever, re-decoding every tick. Any
                // other already-resident duplicate is dropped.
                let is_prev = self.preview_resident.contains(&item);
                let img = o.result.as_ref().expect("Err handled above");
                if is_prev && img.is_preview {
                    self.upgrade_done.insert(item);
                }
                return is_prev && !img.is_preview;
            }
            true
        });
        if let Some(item) = target_failed {
            self.present_failed(item);
        }

        // Current target first, then by prefetch priority, unknowns last.
        let target = self.target_item;
        ready.sort_by_key(|o| {
            let item = o.key.item;
            if target == Some(item) {
                0usize
            } else {
                self.targets
                    .iter()
                    .position(|&t| t == item)
                    .map(|p| p + 1)
                    .unwrap_or(usize::MAX)
            }
        });

        let mut uploads = 0;
        let mut leftover = Vec::new();
        for outcome in ready {
            let item = outcome.key.item;
            let Ok(ref img) = outcome.result else {
                continue; // errors were already filtered out above
            };
            // A full decode for an item already resident as a preview is its
            // in-place upgrade (the retain above kept only real fulls; preview-only
            // upgrade results were already marked `upgrade_done` and dropped).
            let upgrade =
                self.preview_resident.contains(&item) && self.ring.slot_for(item).is_some();
            if uploads >= UPLOADS_PER_TICK {
                // Carry still-wanted leftovers to the next tick (in priority order);
                // drop now-obsolete ones so they don't pin pool byte-budget while
                // the loop idles (work_pending wouldn't keep polling for them).
                if self.targets.contains(&item) && (upgrade || self.ring.slot_for(item).is_none()) {
                    leftover.push(outcome);
                }
                continue;
            }
            if !self.meta_cache.contains_key(&item) {
                let m = meta_for(self.source.as_ref(), item, &self.root, img);
                self.meta_cache.insert(item, m);
            }
            let item_bytes = img.pixels.len() as u64;
            if upgrade {
                let slot = self.ring.slot_for(item).expect("resident as preview");
                if let Some(a) = self.renderer.as_mut() {
                    let t0 = Instant::now();
                    a.upload_slot(
                        slot,
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(img),
                        img.peak,
                    );
                    self.metrics.record("upload", t0.elapsed());
                }
                self.ring.set_slot_bytes(item, item_bytes);
                self.preview_resident.remove(&item);
                // Real end-to-end sharpen latency for the ON-SCREEN photo (what the
                // user actually waits on): full requested → full on screen. Ahead-ring
                // fulls land late by design (low priority), so they'd skew this — only
                // record the displayed one.
                let t0 = self.full_requested_at.remove(&item);
                if self.displayed_item == Some(item) {
                    if let Some(t0) = t0 {
                        self.metrics.record("sharpen", t0.elapsed());
                    }
                }
                uploads += 1;
                // If it's the photo on screen, re-present the slot so the renderer
                // picks up the full texture's dimensions/peak and re-places the quad
                // (it kept the preview's dims otherwise — visible in Original mode),
                // then redraw it now-sharp. `present_slot` keeps the current view, so
                // any zoom/pan is preserved.
                if self.displayed_item == Some(item) {
                    if let Some(a) = self.renderer.as_mut() {
                        a.present_slot(slot);
                    }
                    self.draw();
                }
                continue;
            }
            if let Some(res) = self
                .ring
                .reserve_bytes(item, self.epoch, item_bytes, &self.targets)
            {
                if let Some(a) = self.renderer.as_mut() {
                    let t0 = Instant::now();
                    a.upload_slot(
                        res.slot,
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(img),
                        img.peak,
                    );
                    self.metrics.record("upload", t0.elapsed());
                }
                self.ring.mark_resident(item, res.slot, self.epoch);
                if img.is_preview {
                    self.preview_resident.insert(item);
                } else {
                    self.preview_resident.remove(&item);
                }
                uploads += 1;
                if self.target_item == Some(item) && self.displayed_item != Some(item) {
                    self.present_item(item, res.slot);
                }
            }
            // reserve == None (no longer wanted): drop the outcome, freeing budget.
        }
        self.pending_uploads = leftover;
    }

    /// Synchronous decode + display of the current item — an instant frame on the
    /// first paint and on geometry changes (resize / scale-mode toggle), before the
    /// async ring re-fills neighbors at the new resolution.
    pub fn load_current_sync(&mut self) {
        let Some(idx) = self.playlist.current() else {
            return;
        };
        let t0 = Instant::now();
        // Preview-first (`allow_preview = true`): this decode runs **synchronously on
        // the event-loop thread**, so it must be fast. For RAW/HEIC that means the
        // embedded preview (tens of ms) instead of a full sensor demosaic — which on a
        // 40 MB NEF is ~20 s and froze the loop into a beachball on a Finder open. The
        // full-resolution decode lands off-thread: `request_prefetch` (called by every
        // caller right after this) re-decodes this item into the ring and `sharpen_now`
        // upgrades the on-screen preview to full in place (`drain_results`). This is the
        // documented "preview-first, then refine" model, now applied to the first frame
        // too. (JPEG/PNG/etc. have no cheaper preview, so this is a full decode anyway —
        // fast enough not to beachball, and faster still once dev builds optimize the
        // decoders; see the `[profile.dev]` note in the workspace Cargo.toml.)
        let decoded = decode_item(self.source.as_ref(), idx, self.decode_fit(), true);
        self.metrics.record("decode", t0.elapsed());
        match decoded {
            Ok(img) => {
                let meta = meta_for(self.source.as_ref(), idx, &self.root, &img);
                self.current = Some(meta.clone());
                self.meta_cache.insert(idx, meta);
                let view = self.view_for(idx);
                let title = title_for(self.source.name(idx), idx, self.source.len());
                if let Some(r) = self.renderer.as_mut() {
                    r.set_view(view);
                    r.set_image(
                        &img.pixels,
                        img.width,
                        img.height,
                        render_color(&img.color),
                        is_hdr(&img),
                        img.peak,
                    );
                    r.set_overlay(None, 0);
                }
                self.effects.push(contract::CoreEffect::SetTitle(title));
                self.overlay_shown = false;
                self.overlay_item = None;
                self.displayed_item = Some(idx);
            }
            Err(e) => {
                eprintln!("decode failed: {}: {e}", self.source.name(idx));
                self.failed.insert(idx);
                // Keep the gate unstuck (count the bad file as "shown") and clear
                // the stale frame's title/panel so they don't misreport it.
                self.present_failed(idx);
            }
        }
        self.last_present = Some(Instant::now());
        self.draw();
    }

    /// Bump the geometry epoch and rebuild the (now-invalid) ring. Called on resize
    /// and fit/original toggle so in-flight decodes for the old size are discarded.
    pub fn invalidate_geometry(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        let fit = self.fit.unwrap_or(FitBox {
            max_width: 1,
            max_height: 1,
        });
        let cap = ring_capacity(self.slot_bytes_estimate());
        self.ring = ResidentRing::new_with_budget(cap, RING_BUDGET_BYTES);
        if let Some(a) = self.renderer.as_mut() {
            a.reserve_ring(cap, fit.max_width, fit.max_height);
        }
        let (ahead, behind) = window_for_capacity(cap);
        self.ahead = ahead;
        self.behind = behind;
        // Drop decodes staged for the old geometry; they free their pool budget.
        self.pending_uploads.clear();
    }

    /// Start / stop the slideshow (task #23, the `S` key + View ▸ Slideshow). Starting
    /// resets the timer (`last_present = now`) so the first slide shows for a full
    /// interval before advancing; `about_to_wait` drives the auto-advance from there.
    pub fn toggle_slideshow(&mut self) {
        let on = self.slideshow.toggle();
        if on {
            self.last_present = Some(Instant::now());
        }
        self.show_toast(if on { "Slideshow" } else { "Slideshow Stopped" });
    }

    /// Change the slideshow interval by `steps` × 0.5s (the `[` / `]` keys: `-1`
    /// shortens, `+1` lengthens), clamped, and flash the new value (e.g. `2.0s`). The
    /// change applies live: the deadline is `last_present + interval`, so a running
    /// slideshow's current slide gets more / less remaining time immediately.
    pub fn adjust_slideshow(&mut self, steps: i32) {
        let interval = self.slideshow.adjust(steps);
        self.show_toast(&slideshow::format_interval(interval));
    }

    /// Request the native picker (`O` = file(s), `Shift+O` = folder). Computes the start
    /// directory from live state (core), then emits an [`CoreEffect::OpenFilePanel`] /
    /// [`OpenFolderPanel`](CoreEffect::OpenFolderPanel); the shell runs the modal panel in
    /// the drain and re-enters via [`App::finish_picker`]. Modal — it blocks the event loop
    /// while open, which is fine: the app isn't flying through photos with a dialog up.
    pub fn open_picker(&mut self, folder: bool) {
        let fallback = default_picker_dir();
        let mut start_dir = picker_start_dir(
            self.settings.picker_dir.as_deref(),
            self.source.container(),
            self.scan_root.as_deref(),
            &self.root,
            &fallback,
        );
        // If the chosen folder no longer exists (e.g. a pinned folder was deleted or
        // unmounted), use the safe default rather than letting the OS dialog surface its
        // own remembered last folder.
        if !start_dir.is_dir() {
            start_dir = fallback;
        }
        self.effects.push(if folder {
            contract::CoreEffect::OpenFolderPanel { start_dir }
        } else {
            contract::CoreEffect::OpenFilePanel { start_dir }
        });
    }

    /// Re-set the window title for the currently displayed photo (e.g. after a streaming
    /// grow bumps the "X / N" total). No-op if nothing is displayed.
    pub fn refresh_title(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        if item >= self.source.len() {
            return;
        }
        let title = title_for(self.source.name(item), item, self.source.len());
        self.effects.push(contract::CoreEffect::SetTitle(title));
    }

    /// Push the current view transform to the renderer (re-places the quad).
    pub fn push_view(&mut self) {
        let view = self.view;
        if let Some(a) = self.renderer.as_mut() {
            a.set_view(view);
        }
    }

    /// Decode the first image at the display size for an instant first frame.
    /// Returns `(pixels, w, h, color, hdr, peak, title)`.
    pub fn initial_image(
        &mut self,
    ) -> (
        Vec<u8>,
        u32,
        u32,
        pb_render::ColorTransform,
        bool,
        f32,
        String,
    ) {
        let srgb = pb_render::ColorTransform::srgb();
        match self.playlist.current() {
            // Preview-first (see `load_current_sync`): this runs synchronously while the
            // window is hidden during setup, so it grabs the fast embedded preview for
            // RAW/HEIC; the pool upgrades it to full once `resumed` kicks off prefetch.
            Some(idx) => match decode_item(self.source.as_ref(), idx, self.decode_fit(), true) {
                Ok(img) => {
                    let meta = meta_for(self.source.as_ref(), idx, &self.root, &img);
                    self.current = Some(meta.clone());
                    self.meta_cache.insert(idx, meta);
                    let title = title_for(self.source.name(idx), idx, self.source.len());
                    let (w, h, hdr, peak) = (img.width, img.height, is_hdr(&img), img.peak);
                    let color = render_color(&img.color);
                    (img.pixels, w, h, color, hdr, peak, title)
                }
                Err(e) => {
                    eprintln!("decode failed: {}: {e}", self.source.name(idx));
                    self.current = None;
                    let p = test_pattern(1600, 1000);
                    (
                        p,
                        1600,
                        1000,
                        srgb,
                        false,
                        1.0,
                        "PhotoBlaze (decode error)".to_string(),
                    )
                }
            },
            None => {
                // No images: hand the renderer a 1×1 dummy just to construct, then the
                // caller blanks it (`clear_image`) and shows the "Press O to open" hint.
                self.current = None;
                (
                    vec![0, 0, 0, 255],
                    1,
                    1,
                    srgb,
                    false,
                    1.0,
                    "PhotoBlaze".to_string(),
                )
            }
        }
    }

    /// The full-EXIF "nerd" panel rows for the displayed photo: a filename/path
    /// header (spanning both columns), then a two-column table of dimensions,
    /// codec, exact byte size, and every EXIF tag. Read on-demand from RAM
    /// (privacy task #2: nothing cached to disk). Capped to fit the screen height.
    pub fn exif_rows(&self) -> Vec<Row> {
        let Some(item) = self.displayed_item else {
            return Vec::new();
        };
        let name = self.source.name(item);
        let mut rows = Vec::new();
        // Identity header: filename (bold) over its folder (the filename is already
        // shown above, so the path row is the parent directory only).
        rows.push(Row::Span {
            text: file_name_of(name),
            bold: true,
        });
        // Location row. A real file shows its on-disk folder. An archive entry
        // shows the archive's path, with the in-archive folder appended (after a
        // `›`) when the entry lives in a subfolder — so a zip's photos report
        // *where the zip is* plus *where inside it they are*.
        let location = match (self.source.path(item), self.source.container()) {
            (Some(p), _) => p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.display().to_string()),
            (None, Some(zip)) => {
                let inner = Path::new(name)
                    .parent()
                    .map(|d| d.to_string_lossy().replace('\\', "/"))
                    .filter(|s| !s.is_empty());
                Some(match inner {
                    Some(dir) => format!("{} › {}", zip.display(), dir),
                    None => zip.display().to_string(),
                })
            }
            (None, None) => Path::new(name)
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.to_string_lossy().replace('\\', "/")),
        };
        if let Some(location) = location {
            rows.push(Row::Span {
                text: location,
                bold: false,
            });
        }
        if let Some(meta) = &self.current {
            rows.push(Row::Pair {
                label: "Dimensions".to_string(),
                value: format!("{} × {}", meta.w, meta.h),
            });
            rows.push(Row::Pair {
                label: "Codec".to_string(),
                value: meta.codec.to_uppercase(),
            });
        }
        // Animation facts (frame count, live current frame, rate, loop) — right under
        // the codec so an animated file reads as one block.
        rows.extend(self.animation_rows(item));
        // File size + EXIF from the memoized read (populated by `ensure_exif_cached`
        // before this is called; a cold miss simply omits them until the next rebuild).
        if let Some((size, fields)) = self.exif_cache.get(&item) {
            rows.push(Row::Pair {
                label: "File Size".to_string(),
                value: format!("{} bytes", hud::format_thousands(*size)),
            });
            for (tag, val) in fields {
                // Skip binary blobs that render as meaningless hex (Apple
                // MakerNote/Padding are kilobytes long); truncate anything else
                // that's overlong so one field can't blow out the panel width.
                if is_exif_blob(tag, val) {
                    continue;
                }
                rows.push(Row::Pair {
                    label: tag.clone(),
                    value: truncate_exif_value(val),
                });
            }
        }
        // Cap to what fits the screen height (~1.5x the font size per line).
        if let Some(fit) = self.fit {
            let line_h = ((15.0 * self.viewport.scale_factor).max(8.0) * 1.5).max(1.0);
            let max_rows = (((fit.max_height as f32) - 40.0) / line_h).max(1.0) as usize;
            if rows.len() > max_rows {
                rows.truncate(max_rows.saturating_sub(1));
                rows.push(Row::Span {
                    text: "…".to_string(),
                    bold: false,
                });
            }
        }
        rows
    }

    /// Populate the per-item EXIF cache (file size + raw tag/value pairs) if absent, so
    /// the detailed panel — and its per-frame rebuilds while an animation plays — read
    /// the encoded bytes at most once. RAM-only, read-only (privacy #2).
    pub fn ensure_exif_cached(&mut self, item: usize) {
        if self.exif_cache.contains_key(&item) {
            return;
        }
        if let Ok(bytes) = self.source.bytes(item) {
            let fields = read_exif_fields(&bytes);
            self.exif_cache.insert(item, (bytes.len() as u64, fields));
        }
    }

    /// Animation facts for the detailed panel: empty for a still. Once the sequence is
    /// decoded (playing, or eagerly prepped) it reports the live current frame, the
    /// average frame rate, the duration, and the loop count; before that, just a hint
    /// that `P` will play it. The codec/format is already shown by the Codec row above.
    pub fn animation_rows(&self, item: usize) -> Vec<Row> {
        // A Live Photo (its pairing is resolved into `live_motion_cache` when the panel
        // opens) or an animated container. Neither → nothing to add.
        let is_live = self.is_live_photo(item);
        let is_animated = self.current.as_ref().and_then(|m| m.animated).is_some();
        // Frame/timing detail needs a decoded sequence — the live playback, or the one
        // eagerly prepped for this item.
        let detail: Option<(usize, usize, Duration, u32)> = if let Some(pb) = &self.playback {
            Some((
                pb.index(),
                pb.frame_count(),
                pb.total_duration(),
                pb.loop_count(),
            ))
        } else if let Some(p) = self.prepared.as_ref().filter(|p| p.item == item) {
            let count = p.anim.frames.len();
            let total: Duration = p.anim.frames.iter().map(|f| f.delay).sum();
            Some((0, count, total, p.anim.loop_count))
        } else {
            None
        };
        if !is_live && !is_animated && detail.is_none() {
            return Vec::new();
        }
        // Reserve every row up front — the labels are known from the header sniff /
        // pairing, so a Live Photo or animation always shows the same rows. Values are a
        // pending placeholder until the sequence is decoded (eager prep on dwell), then
        // fill in **in place**, so the panel never reflows when the numbers land a beat
        // later. Playback then updates the live "Frame X / N" value with no row churn.
        const PENDING: &str = "…";
        let mut rows = Vec::new();
        // A Live Photo names itself + its frame count (the Codec row shows the still's
        // format); an animation's count lives in the Frame row below.
        if is_live {
            rows.push(Row::Pair {
                label: "Live Photo".to_string(),
                value: detail.map_or(PENDING.to_string(), |(_, count, _, _)| {
                    format!("{count} frames")
                }),
            });
        }
        rows.push(Row::Pair {
            label: "Frame".to_string(),
            value: detail.map_or(PENDING.to_string(), |(idx, count, _, _)| {
                format!("{} / {}", idx + 1, count)
            }),
        });
        rows.push(Row::Pair {
            label: "Frame Rate".to_string(),
            value: detail.map_or(PENDING.to_string(), |(_, count, total, _)| {
                let secs = total.as_secs_f64();
                if secs > 0.0 {
                    format!("{:.1} fps", count as f64 / secs)
                } else {
                    PENDING.to_string()
                }
            }),
        });
        rows.push(Row::Pair {
            label: "Duration".to_string(),
            value: detail.map_or(PENDING.to_string(), |(_, _, total, _)| {
                format!("{:.2} s", total.as_secs_f64())
            }),
        });
        // A Live Photo always plays once; the loop count is only meaningful for a
        // GIF/APNG/WebP loop.
        if !is_live {
            rows.push(Row::Pair {
                label: "Loop".to_string(),
                value: detail.map_or(PENDING.to_string(), |(_, _, _, loops)| {
                    if loops == 0 {
                        "Forever".to_string()
                    } else {
                        format!("{loops}×")
                    }
                }),
            });
        }
        rows
    }

    /// Whether item `item` is a Live Photo, from the pairing cache (populated when the
    /// info panel opens / on dwell). A `&self` read — never triggers a stat — so it's
    /// safe from the render/rows path; the `&mut` [`live_motion_path`](App::live_motion_path)
    /// is what fills the cache.
    pub fn is_live_photo(&self, item: usize) -> bool {
        self.live_motion_cache
            .get(&item)
            .is_some_and(|paired| paired.is_some())
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

    /// Hide the info panel (clears the overlay quad).
    pub fn hide_overlay(&mut self) {
        if let Some(a) = self.renderer.as_mut() {
            a.set_overlay(None, 0);
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

    /// Build the empty-state **open panel** — two centered, interactive buttons ("Open File" and
    /// "Open Folder", each with its shortcut dimmed and right-aligned menu-style), lit per the
    /// current [`open_hover`]. `None` if no system font loaded. Returns the owned bitmap plus
    /// each button's bitmap-relative rect, so callers can apply it to a renderer they still own
    /// (e.g. mid-setup) and record the click targets.
    ///
    /// [`open_hover`]: App::open_hover
    pub fn open_panel_bitmap(&self) -> Option<hud::OpenPanelBitmap> {
        let hud = self.hud.as_ref()?;
        // A normal button size (like the scan card's Cancel button) — the call to action doesn't
        // need to shout; it's white text on an empty gray screen.
        let px = (16.0 * self.viewport.scale_factor).max(11.0);
        let file_key = self.shortcut_for(Action::OpenFile);
        let folder_key = self.shortcut_for(Action::OpenFolder);
        hud.render_open_panel(
            "Open File",
            &file_key,
            "Open Folder",
            &folder_key,
            px,
            hud::BG,
            true, // shortcut hint a notch heavier (Semibold) so the dimmed text keeps presence
            self.open_hover == Some(OpenButton::File),
            self.open_hover == Some(OpenButton::Folder),
        )
    }

    /// Flash a transient status message at the bottom-center (tasks.json #10) — for
    /// commands that otherwise give no visual feedback, e.g. the recursion toggle.
    /// A new toast replaces any current one.
    pub fn show_toast(&mut self, msg: &str) {
        self.show_toast_icon(msg, None);
    }

    /// Like [`show_toast`] but with an optional leading duotone icon (an SVG source
    /// from [`icon::assets`]) — e.g. the clipboard glyph on the Copy toast, or an
    /// icon-only pill (empty `msg`) for the rotate toasts. Always redraws, so a
    /// caller that also changed the view (e.g. `rotate`) renders even when there's
    /// no system font to build a toast from.
    pub fn show_toast_icon(&mut self, msg: &str, icon: Option<&str>) {
        let px = (26.0 * self.viewport.scale_factor).max(16.0);
        let pad = (12.0 * self.viewport.scale_factor).round().max(4.0) as u32;
        // A passive toast is not the interactive play hint — drop any play-hint state so it
        // doesn't respond to hover/click while a Copy/Save/… toast is up.
        self.play_hint = None;
        if let Some(hud) = self.hud.as_ref() {
            if let Some((rgba, w, h)) = hud.render_panel_icon(msg, px, pad, icon, hud::BG) {
                self.toast = Some(Toast {
                    rgba,
                    w,
                    h,
                    started: Instant::now(),
                    uploaded_alpha: -1.0,
                });
                self.push_toast(1.0);
            }
        }
        self.draw();
    }

    /// Build the **play hint** toast — the `▶ Play  P` button (leading icon, label, dimmed
    /// shortcut), at `hovered` fill/border — and push it. Returns the bitmap `(w, h)` so the
    /// caller can record the hit rect. Shared by the first flash and the hover re-render.
    pub fn build_play_hint(&mut self, icon: &str, hovered: bool) -> Option<(u32, u32)> {
        let px = (20.0 * self.viewport.scale_factor).max(13.0);
        let shortcut = self.shortcut_for(Action::PlayPause);
        let built = self.hud.as_ref().and_then(|hud| {
            let spec = hud::ButtonSpec {
                label: "Play",
                icon: Some(icon),
                shortcut: (!shortcut.is_empty()).then_some(shortcut.as_str()),
                shortcut_semibold: true,
                min_w: 0,
            };
            hud.render_button(&spec, px, hud::BG, hovered)
        });
        let (rgba, w, h) = built?;
        self.toast = Some(Toast {
            rgba,
            w,
            h,
            started: Instant::now(),
            uploaded_alpha: -1.0,
        });
        self.push_toast(1.0);
        Some((w, h))
    }

    /// Upload the current toast bitmap to the renderer at `alpha` (its alpha
    /// channel scaled), centered near the bottom.
    pub fn push_toast(&mut self, alpha: f32) {
        let (faded, w, h) = {
            let Some(t) = self.toast.as_mut() else {
                return;
            };
            t.uploaded_alpha = alpha;
            (scale_alpha(&t.rgba, alpha), t.w, t.h)
        };
        let margin = (64.0 * self.viewport.scale_factor).round().max(8.0) as u32;
        if let Some(a) = self.renderer.as_mut() {
            a.set_toast(Some((&faded, w, h)), margin);
        }
    }

    /// Advance the toast's hold/fade and return whether one is still active (so the
    /// event loop keeps ticking). Re-uploads only on a meaningful alpha change;
    /// clears the layer once expired.
    pub fn tick_toast(&mut self, now: Instant) -> bool {
        // A hovered play hint pauses the fade: keep its toast pinned in the full-opacity hold
        // window, so the button never vanishes out from under the pointer.
        if self.play_hint.is_some_and(|ph| ph.hovered) {
            if let Some(t) = self.toast.as_mut() {
                t.started = now;
            }
        }
        let Some(alpha) = self.toast.as_ref().and_then(|t| t.alpha(now)) else {
            self.play_hint = None;
            if self.toast.take().is_some() {
                if let Some(a) = self.renderer.as_mut() {
                    a.set_toast(None, 0);
                }
                self.draw();
            }
            return false;
        };
        let changed = self
            .toast
            .as_ref()
            .is_some_and(|t| (alpha - t.uploaded_alpha).abs() > 0.02);
        if changed {
            self.push_toast(alpha);
            self.draw();
        }
        true
    }

    /// The current keypress brighten-pulse intensity (0..=1), decaying to 0 over
    /// `PIE_GLOW_DUR` after the last dropped nav press.
    pub fn pie_glow(&self, now: Instant) -> f32 {
        match self.pie_glow_started {
            Some(t) => (1.0 - (now - t).as_secs_f32() / PIE_GLOW_DUR).clamp(0.0, 1.0),
            None => 0.0,
        }
    }

    /// Drive the top-right "not-ready" loading pie (#2). While the next photo is
    /// still decoding (a miss outlasting `PIE_SHOW_DELAY`), show a pie that eases
    /// asymptotically toward — but never reaches — full, on a time constant
    /// self-calibrated to how long misses usually take (`decode_ewma`). Once the
    /// photo lands, learn from the wait, then snap to full and fade. Returns
    /// whether the pie still needs the loop to keep ticking.
    pub fn tick_pie(&mut self, now: Instant) -> bool {
        let not_ready = self.target_item.is_some() && self.displayed_item != self.target_item;
        if not_ready {
            self.pie_finish = None;
            let start = *self.wait_started.get_or_insert(now);
            let elapsed = (now - start).as_secs_f32();
            if elapsed >= PIE_SHOW_DELAY {
                let tau = self.decode_ewma.max(PIE_TAU_MIN);
                // Asymptotic ease: ~half-full at one tau, approaching the cap but
                // never quite arriving (the deliberate, honest-ish "lie").
                let progress = (1.0 - 2f32.powf(-elapsed / tau)).min(PIE_FILL_CAP);
                let glow = self.pie_glow(now);
                self.push_pie(progress, glow, 1.0);
            } else {
                self.clear_pie();
            }
            return true; // keep ticking while we wait
        }
        // Caught up. If we were mid-wait, learn how long it took (so the estimate
        // tracks this machine + folder), and if the pie was up, play the finish.
        if let Some(start) = self.wait_started.take() {
            let waited = (now - start).as_secs_f32();
            self.decode_ewma = (self.decode_ewma * (1.0 - PIE_EWMA_ALPHA)
                + waited * PIE_EWMA_ALPHA)
                .clamp(PIE_TAU_MIN, 2.0);
            if self.pie_drawn {
                self.pie_finish = Some(now);
            }
        }
        if let Some(fstart) = self.pie_finish {
            let t = (now - fstart).as_secs_f32();
            if t < PIE_FINISH_FADE {
                let glow = self.pie_glow(now);
                self.push_pie(1.0, glow, 1.0 - t / PIE_FINISH_FADE);
                return true;
            }
            self.pie_finish = None;
        }
        self.clear_pie();
        false
    }

    /// Rasterize + upload the pie at `progress`/`glow`, scaled by a global `alpha`
    /// (the finish fade). Re-uploads + redraws only when the visible result
    /// changes (quantized), so the slow tail of the asymptote doesn't churn.
    pub fn push_pie(&mut self, progress: f32, glow: f32, alpha: f32) {
        let want = (progress, glow, alpha);
        let unchanged = self.pie_pushed.is_some_and(|(p, g, a)| {
            (p - progress).abs() < 0.01 && (g - glow).abs() < 0.04 && (a - alpha).abs() < 0.02
        });
        if unchanged && self.pie_drawn {
            return;
        }
        let diameter = (PIE_DIAMETER * self.viewport.scale_factor)
            .round()
            .max(12.0) as u32;
        let (mut rgba, w, h) = hud::render_pie(diameter, progress, glow);
        if alpha < 1.0 {
            rgba = scale_alpha(&rgba, alpha);
        }
        let margin = (PIE_MARGIN * self.viewport.scale_factor).round().max(4.0) as u32;
        if let Some(a) = self.renderer.as_mut() {
            a.set_pie(Some((&rgba, w, h)), margin);
        }
        self.pie_drawn = true;
        self.pie_pushed = Some(want);
        self.draw();
    }

    /// Clear the pie layer if it's up (and redraw to remove it).
    pub fn clear_pie(&mut self) {
        if self.pie_drawn {
            if let Some(a) = self.renderer.as_mut() {
                a.set_pie(None, 0);
            }
            self.pie_drawn = false;
            self.pie_pushed = None;
            self.draw();
        }
    }

    /// Rasterize the scan status card and place it at the top-right with equal top/right insets.
    /// Records the centered **Cancel Scan button's** physical-px rect (the only click target).
    pub fn push_chip(&mut self, name: &str, path: &str, count: usize) {
        let heading = format!("Scanning \u{201c}{name}\u{201d}");
        let noun = if count == 1 { "image" } else { "images" };
        let count_line = format!("{} {noun} found", hud::format_thousands(count as u64));
        // Equal inset from the top and right edges; fixed card width, clamped to the window.
        let margin = (PIE_MARGIN * self.viewport.scale_factor).round().max(4.0) as u32;
        let win_w = self.viewport.width;
        let width = ((SCAN_CARD_WIDTH * self.viewport.scale_factor).round())
            .min((win_w as f32 - 2.0 * margin as f32).max(1.0))
            .max(1.0) as u32;
        let card = self.hud.as_ref().and_then(|hud| {
            let px = (15.0 * self.viewport.scale_factor).max(10.0);
            hud.render_scan_card(
                &heading,
                path,
                &count_line,
                "Cancel Scan",
                icon::assets::STOP,
                px,
                width,
                hud::BG,
                self.chip_hovered,
            )
        });
        let Some((rgba, w, h, btn)) = card else {
            self.chip_rect = None;
            return;
        };
        // Card top-left in physical px (right edge inset by `margin`, top inset by `margin`),
        // then the button rect offset within it → the click hit-target.
        let card_x0 = self.viewport.width as f32 - margin as f32 - w as f32;
        let card_y0 = margin as f32;
        let [bx, by, bw, bh] = btn.map(|v| v as f32);
        self.chip_rect = Some([
            card_x0 + bx,
            card_y0 + by,
            card_x0 + bx + bw,
            card_y0 + by + bh,
        ]);
        if let Some(a) = self.renderer.as_mut() {
            a.set_chip(Some((&rgba, w, h)), margin, margin);
        }
        self.draw();
    }

    /// Clear the scan card layer if it's up (and redraw to remove it).
    pub fn clear_chip(&mut self) {
        if self.chip_sig.is_some() {
            if let Some(a) = self.renderer.as_mut() {
                a.set_chip(None, 0, 0);
            }
            self.chip_rect = None;
            self.chip_hovered = false;
            self.draw();
        }
    }

    /// Hit-test a physical-px cursor position against the scan card's **Cancel Scan button**
    /// rect. The reusable overlay-click primitive: store a rect when you draw an interactive
    /// overlay, test it here before the click falls through to drag-to-pan. (Future EXIF copy
    /// buttons will register their own rects the same way.)
    pub fn chip_hit(&self, x: f32, y: f32) -> bool {
        self.chip_rect.is_some_and(|rect| point_in_rect(rect, x, y))
    }

    /// Update the Cancel Scan button's hover "lit" state from the latest cursor position, and —
    /// only when hover **changes** — re-rasterize the card so the button lights up / dims. This
    /// runs on every cursor-move, but the rebuild fires just on the enter/leave transition (one
    /// ~320px CPU composite), never per move or per frame, so it stays off the photo hot path.
    pub fn update_chip_hover(&mut self) {
        let hovered = self.last_cursor.is_some_and(|[x, y]| self.chip_hit(x, y));
        if hovered == self.chip_hovered {
            return;
        }
        self.chip_hovered = hovered;
        // Re-render the card in the new hover state; its content (name/path/count) is unchanged,
        // so this bypasses the content throttle and feels instant.
        if let Some((name, path, count)) = self.chip_sig.clone() {
            self.push_chip(&name, &path, count);
        }
    }

    /// The interactive play hint's on-screen `[x0, y0, x1, y1]` rect (physical px), derived from
    /// the live window size — the toast is bottom-center with a fixed margin, so this matches
    /// the renderer's placement and survives resizes. `None` unless the play hint is the current
    /// toast and still on screen.
    pub fn play_hint_rect(&self) -> Option<[f32; 4]> {
        let ph = self.play_hint?;
        self.toast.as_ref()?; // only while its toast is actually up
        let sz = self.viewport;
        let margin = (64.0 * self.viewport.scale_factor).round().max(8.0);
        let x0 = ((sz.width as f32 - ph.w as f32) * 0.5).max(0.0);
        let y1 = sz.height as f32 - margin;
        let y0 = y1 - ph.h as f32;
        Some([x0, y0, x0 + ph.w as f32, y1])
    }

    /// Whether the pointer is over the interactive play hint.
    pub fn play_hint_hit(&self) -> bool {
        match (self.last_cursor, self.play_hint_rect()) {
            (Some([x, y]), Some(rect)) => point_in_rect(rect, x, y),
            _ => false,
        }
    }

    /// Update the play hint's hover state from the cursor. On an enter/leave transition it
    /// re-renders the button lit/unlit (the fade itself is paused while hovered — see
    /// [`tick_toast`]). Cheap: one CPU composite per transition, never per move.
    ///
    /// [`tick_toast`]: App::tick_toast
    pub fn update_play_hint_hover(&mut self) {
        let hovered = self.play_hint_hit();
        let Some(ph) = self.play_hint else {
            return;
        };
        if ph.hovered == hovered {
            return;
        }
        // Re-render at the new state (size is unchanged; keep the recorded w/h). Rebuilding
        // resets the fade clock, which is what we want: hovering holds it at full, and leaving
        // gives it a fresh hold before it fades.
        self.build_play_hint(ph.icon, hovered);
        self.play_hint = Some(PlayHint { hovered, ..ph });
        self.draw();
    }

    /// Render one frame.
    pub fn draw(&mut self) {
        let t0 = Instant::now();
        let mut fatal = false;
        let drew = if let Some(a) = self.renderer.as_mut() {
            if let Err(e) = a.render() {
                eprintln!("fatal render error: {e:?}");
                fatal = true;
            }
            a.poll();
            true
        } else {
            false
        };
        // Push after the `self.renderer` borrow ends (can't touch `self.effects` inside it).
        if fatal {
            self.effects.push(contract::CoreEffect::Quit);
        }
        if drew {
            self.metrics.record("render", t0.elapsed());
        }
    }

    /// Which way we're currently paging, from the held nav actions (ambiguous/none =
    /// idle). Next (forward), Prev (backward), and Random / RandomPrev advance; two
    /// keys bound to the *same* direction (e.g. Enter + NumpadEnter) still count as
    /// one, but two *different* nav directions held at once is treated as idle.
    pub fn held_nav(&self) -> Option<Nav> {
        let mut dir: Option<Nav> = None;
        for &action in self.held.values() {
            if let Some(n) = nav_of(action) {
                match dir {
                    None => dir = Some(n),
                    Some(d) if d == n => {}
                    Some(_) => return None, // two different directions → idle
                }
            }
        }
        dir
    }

    /// Zoom direction from the held actions: `+1` in ([`Action::ZoomIn`]), `-1` out
    /// ([`Action::ZoomOut`]), `None` if neither or both.
    pub fn zoom_held(&self) -> Option<f32> {
        let mut zin = false;
        let mut zout = false;
        for &action in self.held.values() {
            match action {
                Action::ZoomIn => zin = true,
                Action::ZoomOut => zout = true,
                _ => {}
            }
        }
        match (zin, zout) {
            (true, false) => Some(1.0),
            (false, true) => Some(-1.0),
            _ => None,
        }
    }

    /// Pan velocity direction from the held pan actions (image-space; positive pan
    /// reveals the right/bottom). Diagonals combine. `(0, 0)` if none held.
    pub fn pan_held(&self) -> (f32, f32) {
        let mut x = 0.0;
        let mut y = 0.0;
        for &action in self.held.values() {
            match action {
                Action::PanLeft => x += 1.0,
                Action::PanRight => x -= 1.0,
                Action::PanUp => y += 1.0,
                Action::PanDown => y -= 1.0,
                _ => {}
            }
        }
        (x, y)
    }

    /// The current image texture + screen dimensions for pan-clamp math.
    pub fn screen_and_image(&self) -> Option<(u32, u32, u32, u32)> {
        let fit = self.fit?;
        let (iw, ih) = self.renderer.as_ref()?.image_size();
        Some((iw, ih, fit.max_width, fit.max_height))
    }

    /// Whether the image currently overflows the viewport (so panning does
    /// something). Drives the grab-hand cursor affordance.
    pub fn pannable(&self) -> bool {
        self.screen_and_image()
            .map(|(iw, ih, sw, sh)| {
                let mp = self.view.max_pan(iw, ih, sw, sh);
                mp[0] > 0.0 || mp[1] > 0.0
            })
            .unwrap_or(false)
    }

    /// Pan by a raw pixel delta (trackpad two-finger swipe), clamped to the image
    /// bounds. No effect when the image fits within the screen (nothing to pan).
    pub fn pan_by_pixels(&mut self, dx: f32, dy: f32) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        self.view.pan[0] += dx;
        self.view.pan[1] += dy;
        if let Some((iw, ih, sw, sh)) = self.screen_and_image() {
            let mp = self.view.max_pan(iw, ih, sw, sh);
            self.view.pan[0] = self.view.pan[0].clamp(-mp[0], mp[0]);
            self.view.pan[1] = self.view.pan[1].clamp(-mp[1], mp[1]);
        }
        self.push_view();
        self.draw();
    }

    /// Apply continuous zoom/pan while their keys are held, with a time-based
    /// acceleration ramp (gentle start for fine tuning, faster the longer held).
    /// Returns whether anything changed (so the loop keeps polling + redrawing).
    pub fn apply_view_holds(&mut self, now: Instant) -> bool {
        let mut changed = false;

        match self.zoom_held() {
            Some(dir) => {
                let start = *self.zoom_started.get_or_insert(now);
                let last = self.zoom_last.replace(now).unwrap_or(start);
                let dt = (now - last).as_secs_f32().min(0.1);
                let t = (now - start).as_secs_f32();
                let rate =
                    ZOOM_MIN_RATE + (ZOOM_MAX_RATE - ZOOM_MIN_RATE) * (t / ZOOM_RAMP_SECS).min(1.0);
                // Exponential (multiplicative) zoom about the screen center.
                self.view.zoom =
                    (self.view.zoom * (rate * dir * dt).exp()).clamp(MIN_ZOOM, MAX_ZOOM);
                changed = true;
            }
            None => {
                self.zoom_started = None;
                self.zoom_last = None;
            }
        }

        let (px, py) = self.pan_held();
        if px != 0.0 || py != 0.0 {
            let start = *self.pan_started.get_or_insert(now);
            let last = self.pan_last.replace(now).unwrap_or(start);
            let dt = (now - last).as_secs_f32().min(0.1);
            let t = (now - start).as_secs_f32();
            let speed =
                PAN_MIN_SPEED + (PAN_MAX_SPEED - PAN_MIN_SPEED) * (t / PAN_RAMP_SECS).min(1.0);
            self.view.pan[0] += px * speed * dt;
            self.view.pan[1] += py * speed * dt;
            if let Some((iw, ih, sw, sh)) = self.screen_and_image() {
                let mp = self.view.max_pan(iw, ih, sw, sh);
                self.view.pan[0] = self.view.pan[0].clamp(-mp[0], mp[0]);
                self.view.pan[1] = self.view.pan[1].clamp(-mp[1], mp[1]);
            }
            changed = true;
        } else {
            self.pan_started = None;
            self.pan_last = None;
        }

        if changed {
            self.push_view();
            self.draw();
        }
        changed
    }

    /// Whether item `item` is an animated container (from the cached header sniff).
    pub fn current_is_animated(&self, item: usize) -> bool {
        self.meta_cache
            .get(&item)
            .and_then(|m| m.animated)
            .is_some()
    }

    /// The companion motion `.mov` for item `item` if it's a Live Photo, else `None`
    /// (task #38). Filesystem pairing, memoized per item and computed lazily — only ever
    /// reached when settled on a photo, never on the fly-through path. Always `None` off
    /// macOS (Windows Live Photos are task #39, since the decoder is macOS-only).
    pub fn live_motion_path(&mut self, item: usize) -> Option<PathBuf> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = item;
            return None;
        }
        #[cfg(target_os = "macos")]
        if let Some(cached) = self.live_motion_cache.get(&item) {
            return cached.clone();
        }
        #[cfg(target_os = "macos")]
        {
            let paired = self.source.path(item).and_then(companion_motion);
            self.live_motion_cache.insert(item, paired.clone());
            paired
        }
    }

    /// Whether item `item` has an on-demand motion component to play on `P` — either an
    /// animated container (GIF/APNG/WebP/HEIF sequence) or a Live Photo's `.mov`.
    pub fn has_motion(&mut self, item: usize) -> bool {
        self.current_is_animated(item) || self.live_motion_path(item).is_some()
    }

    /// Kick the whole-sequence decode for `item` on a worker thread so a big GIF/WebP (or
    /// a Live Photo `.mov`) never stalls the event loop; the still first frame stays on
    /// screen until it lands (picked up by `poll_anim_decode`). `want` decides what
    /// happens on arrival — eager prep (stash ready), play (`P`), or step (frame-step).
    pub fn start_animation_decode(&mut self, item: usize, want: AnimWant) {
        self.anim_gen += 1;
        let gen = self.anim_gen;
        let epoch = self.epoch;
        let source = Arc::clone(&self.source);
        let fit = self.decode_fit();
        // A Live Photo decodes its companion `.mov` via AVFoundation; everything else
        // decodes the still's own bytes as a multi-frame animation.
        let live = self.live_motion_path(item);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = decode_motion_job(live, &source, item, fit);
            let _ = tx.send(result);
        });
        self.anim_decode = Some(AnimDecode {
            gen,
            item,
            epoch,
            want,
            rx,
        });
        // A user-initiated decode (P / step) means they've engaged — suppress the "▶ P"
        // hint. An eager prep is invisible background work, so leave the hint alone.
        if want != AnimWant::Eager {
            self.anim_hint_shown_for = self.displayed_item;
        }
    }

    /// When the user has rested on an animated still, eagerly decode the whole sequence
    /// in the background so pressing `P` is instant (fixes the slow first-play on WebP /
    /// AVIF, ~0.6–2s to decode). Returns the wake deadline while the dwell elapses (so
    /// the idle loop wakes to kick it), else `None`. Strictly off the hot path — only
    /// when settled (never while flying), exactly when the prefetch pool is idle.
    pub fn maybe_prepare_animation(&mut self, now: Instant) -> Option<Instant> {
        if self.playback.is_some() || self.anim_decode.is_some() {
            return None; // already playing, or a decode is already in flight
        }
        let item = self.displayed_item?;
        if self.displayed_item != self.target_item {
            return None; // still catching up to the target — not settled yet
        }
        if self.prepared.as_ref().is_some_and(|p| p.item == item) {
            return None; // already prepped and ready
        }
        if !self.has_motion(item) {
            return None;
        }
        match self.last_present.map(|t| t + EAGER_PREP_DELAY) {
            // Still within the dwell window — wake at the deadline to kick it then.
            Some(due) if now < due => Some(due),
            _ => {
                self.start_animation_decode(item, AnimWant::Eager);
                None
            }
        }
    }

    /// Revert a finished Live Photo to its crisp full-res still: rebind the resident
    /// still texture (it was never evicted — playback draws via `set_image`, not the
    /// ring), re-decoding only in the rare case it's no longer resident.
    pub fn restore_still(&mut self) {
        let Some(item) = self.displayed_item else {
            return;
        };
        if let Some(slot) = self.ring.slot_for(item) {
            self.present_item(item, slot);
        } else {
            self.load_current_sync();
        }
    }

    /// The held frame-step direction: `+1` ([`Action::FrameNext`]) / `-1`
    /// ([`Action::FramePrev`]) / `0` if neither or both.
    pub fn held_frame_step(&self) -> i32 {
        let mut dir = 0i32;
        for &action in self.held.values() {
            match action {
                Action::FrameNext => dir += 1,
                Action::FramePrev => dir -= 1,
                _ => {}
            }
        }
        dir.signum()
    }
}
