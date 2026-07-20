//! **Metadata panels** — the `AppCore` half of [`crate::meta`] and [`crate::media_details`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! EXIF rows, the cached per-item panel data, the off-thread media-details probe, and the
//! animation detail rows.
//!
//! ⚠ Everything cached here is RAM-only and dies with the process — `meta_cache` is named in
//! the privacy guarantee's inventory. EXIF is read on demand from the file; nothing is
//! written back except through the explicit save-rotation command.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// The full-EXIF "nerd" panel rows for the displayed photo: a filename/path
    /// header (spanning both columns), then a two-column table of dimensions,
    /// codec, exact byte size, and every EXIF tag. Read on-demand from RAM
    /// (privacy task #2: nothing cached to disk). Capped to fit the screen height.
    pub fn exif_rows(&self) -> Vec<DetailRow> {
        let Some(item) = self.displayed_item else {
            return Vec::new();
        };
        let name = self.source.name(item);
        let mut rows = Vec::new();
        // Identity header: filename (bold) over its folder (the filename is already
        // shown above, so the path row is the parent directory only).
        rows.push(DetailRow::Span {
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
            rows.push(DetailRow::Span {
                text: location,
                bold: false,
            });
        }
        if let Some(meta) = &self.current {
            rows.push(DetailRow::Pair {
                label: "Dimensions".to_string(),
                value: format!("{} × {}", meta.w, meta.h),
            });
            rows.push(DetailRow::Pair {
                label: "Codec".to_string(),
                value: meta.codec.to_uppercase(),
            });
        }
        // Animation facts (frame count, live current frame, rate, loop) — right under
        // the codec so an animated file reads as one block.
        rows.extend(self.animation_rows(item));
        // File size + EXIF from the memoized read (populated by `ensure_exif_cached`
        // before this is called; a cold miss simply omits them until the next rebuild).
        if let Some(details) = self.exif_cache.get(&item) {
            // A video's container probe runs on a worker (task #98). Until it lands the
            // panel says so, rather than showing a table that looks complete but isn't;
            // `poll_details_probe` re-signals the Inspector when the result arrives.
            if details.probe_state == crate::media_details::ProbeState::Loading {
                rows.push(DetailRow::Span {
                    text: "Reading video details…".to_string(),
                    bold: false,
                });
                return rows;
            }
            rows.push(DetailRow::Pair {
                label: "File Size".to_string(),
                value: format!("{} bytes", hud::format_thousands(details.size)),
            });
            for (tag, val) in &details.fields {
                // Skip binary blobs that render as meaningless hex (Apple
                // MakerNote/Padding are kilobytes long); truncate anything else
                // that's overlong so one field can't blow out the panel width.
                if is_exif_blob(tag, val) {
                    continue;
                }
                rows.push(DetailRow::Pair {
                    label: tag.clone(),
                    value: truncate_exif_value(val),
                });
            }
            // The video's audio + subtitle tracks (task #98), under the basic facts.
            // Completeness-driven, so a probe that failed reads as "details
            // unavailable", never as "No audio".
            if let Some(catalog) = &details.media {
                rows.extend(crate::tracks::track_rows(catalog, details.has_audio));
            }
        }
        // Cap to what fits the screen height (~1.5x the font size per line) — for the
        // fixed-height HUD table only. The native Inspector scrolls, so it shows every row.
        if !self.native_inspector {
            if let Some(fit) = self.fit {
                let line_h = ((15.0 * self.viewport.scale_factor).max(8.0) * 1.5).max(1.0);
                let max_rows = (((fit.max_height as f32) - 40.0) / line_h).max(1.0) as usize;
                if rows.len() > max_rows {
                    rows.truncate(max_rows.saturating_sub(1));
                    rows.push(DetailRow::Span {
                        text: "…".to_string(),
                        bold: false,
                    });
                }
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
        // A video's encoded bytes never enter RAM here (only playback fetches them):
        // the panel's file size comes from a stat (or the archive directory's size
        // hint for an entry), and its facts (duration/codec/fps/audio) from a
        // reader-metadata probe — container headers only, ~15-25 ms once, cached for
        // the item's lifetime (comparable to the sync fs::read the image path below
        // already does here). An archive entry skips the probe: it would inflate the
        // whole entry on the event loop; playback's `Opened` carries duration anyway.
        // Exhaustive on purpose: the `Image` arm reads the item's **entire** encoded
        // bytes synchronously, on the event loop. That is only a bounded cost for a
        // photo, so every kind must state its own answer here rather than inherit the
        // read by falling past a video-shaped `if let`.
        match crate::video::item_kind(self.source.as_ref(), item) {
            crate::video::LibraryItemKind::Video(_) => {
                // Off to a worker: opening a container is an unbounded wait (damaged file,
                // network share, a codec the OS reader labours over), and the event loop must
                // never take it. Record `Loading` — which the panel shows honestly, and which
                // also stops a second worker being spawned for this item — and let `tick`
                // pick the result up.
                self.exif_cache
                    .insert(item, crate::app_core::ItemDetails::loading());
                // Two generations, deliberately: the deck's (has index `item` been reassigned
                // under us?) and a fresh one for this catalog alone (which file's tracks are
                // these?). See `AppCore::catalog_seq` — handing the deck's to both is what let a
                // picked track resolve against the next film's catalog.
                self.catalog_seq += 1;
                self.details_probe = Some(crate::media_details::spawn(
                    &self.source,
                    item,
                    self.details_gen,
                    self.catalog_seq,
                    self.source.name(item).to_string(),
                ));
            }
            // A door's facts are its size and its format — both free. The size comes
            // from a **stat**, never a read (`media_details::probe_job` uses the same
            // rule); reading a 2 GB archive here, on the event loop, just to fill a
            // panel is exactly what the door exists to avoid. No EXIF, no probe: what
            // is inside is unknown until the viewer enters it, and saying so honestly
            // beats opening it to find out.
            crate::video::LibraryItemKind::Archive(kind) => {
                let size = self
                    .source
                    .path(item)
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map(|m| m.len())
                    .or_else(|| self.source.size_hint(item))
                    .unwrap_or(0);
                self.exif_cache.insert(
                    item,
                    crate::app_core::ItemDetails::ready(
                        size,
                        vec![("Format".to_string(), format!("{} archive", kind.name()))],
                    ),
                );
            }
            crate::video::LibraryItemKind::Image => {
                if let Ok(bytes) = self.source.bytes(item) {
                    let fields = read_exif_fields(&bytes);
                    self.exif_cache.insert(
                        item,
                        crate::app_core::ItemDetails::ready(bytes.len() as u64, fields),
                    );
                }
            }
        }
    }

    /// Pick up a finished Details probe (called each tick).
    ///
    /// Accepts the result only if the deck generation **and** the item's identity still
    /// match what was requested: a rebuild reassigns indices, so an older result names a
    /// different file and is dropped rather than cached against the wrong photo. A dead
    /// worker marks the entry `Failed` — otherwise its `Loading` placeholder would sit on
    /// "Reading…" forever, and never re-probe (the placeholder is also the spawn guard).
    pub fn poll_details_probe(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let outcome = {
            let Some(p) = self.details_probe.as_ref() else {
                return;
            };
            match p.rx.try_recv() {
                Ok(details) => Some((
                    p.gen,
                    p.item,
                    p.identity.clone(),
                    p.copy_when_done,
                    Some(details),
                )),
                Err(TryRecvError::Empty) => return, // still probing
                Err(TryRecvError::Disconnected) => {
                    Some((p.gen, p.item, p.identity.clone(), p.copy_when_done, None))
                }
            }
        };
        self.details_probe = None;
        let Some((gen, item, identity, copy, details)) = outcome else {
            return;
        };
        if gen != self.details_gen {
            return; // deck rebuilt while probing — the indices were reassigned
        }
        if self.source.name(item) != identity {
            return; // same index, different file — not our result
        }
        match details {
            Some(d) => {
                self.exif_cache.insert(item, d);
            }
            None => {
                // The worker died. Keep the entry (so we don't respawn in a loop) but say so.
                if let Some(e) = self.exif_cache.get_mut(&item) {
                    e.probe_state = crate::media_details::ProbeState::Failed;
                }
            }
        }
        // The open Inspector may be sitting on this item's "Reading…" row.
        self.emit_panels_changed();
        // The probe may have landed mid-playback for the very video it describes —
        // the DoVi warning's second chance (the first is at session start).
        self.maybe_warn_dovi(item);
        if self.slot_content() == Some(SlotContent::Details) && self.displayed_item == Some(item) {
            self.show_overlay();
        }
        // The cache is warm now, so this re-entry takes the normal path (it cannot loop).
        if copy && self.displayed_item == Some(item) {
            self.copy_image_details();
        }
    }

    /// The old synchronous body, kept only for tests that need a probed entry without a
    /// tick loop. Never call this from the event loop — that is what
    /// [`Self::poll_details_probe`] exists to prevent.
    #[cfg(test)]
    pub fn probe_details_blocking(&mut self, item: usize) {
        self.catalog_seq += 1;
        let d = crate::media_details::probe_job(self.source.as_ref(), item, self.catalog_seq);
        self.exif_cache.insert(item, d);
    }

    /// Animation facts for the detailed panel: empty for a still. Once the sequence is
    /// decoded (playing, or eagerly prepped) it reports the live current frame, the
    /// average frame rate, the duration, and the loop count; before that, just a hint
    /// that `P` will play it. The codec/format is already shown by the Codec row above.
    pub fn animation_rows(&self, item: usize) -> Vec<DetailRow> {
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
            rows.push(DetailRow::Pair {
                label: "Live Photo".to_string(),
                value: detail.map_or(PENDING.to_string(), |(_, count, _, _)| {
                    format!("{count} frames")
                }),
            });
        }
        rows.push(DetailRow::Pair {
            label: "Frame".to_string(),
            value: detail.map_or(PENDING.to_string(), |(idx, count, _, _)| {
                format!("{} / {}", idx + 1, count)
            }),
        });
        rows.push(DetailRow::Pair {
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
        rows.push(DetailRow::Pair {
            label: "Duration".to_string(),
            value: detail.map_or(PENDING.to_string(), |(_, _, total, _)| {
                format!("{:.2} s", total.as_secs_f64())
            }),
        });
        // A Live Photo always plays once; the loop count is only meaningful for a
        // GIF/APNG/WebP loop.
        if !is_live {
            rows.push(DetailRow::Pair {
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
}
