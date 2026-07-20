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
        // A decode FAILURE (task #127): the file couldn't be shown by any rung of the
        // ladder. `current` is None on a failed decode, so there are no dimensions/codec
        // to report — name the reason here so the details still tell the full story
        // (mirrors the placeholder shown on the canvas).
        let decode_error = self.current_decode_error();
        if !decode_error.is_empty() {
            rows.push(DetailRow::Pair {
                label: "Error".to_string(),
                value: format!("can't display this image ({decode_error})"),
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
            // Recovery notice (task #127): this file was malformed but the decode
            // ladder salvaged it. Surface it right under the codec so anyone digging
            // into the details learns the file is non-conforming — and why it may
            // look imperfect — rather than the damage being silent.
            if let Some(reason) = &meta.recovered {
                rows.push(DetailRow::Pair {
                    label: "Recovered".to_string(),
                    value: format!("damaged file, shown anyway ({reason})"),
                });
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{five_photos, seed_details, test_core, track};

    /// **Regression — the silent one.** A `TrackId`'s whole contract is that `local_id` means
    /// something only within the generation it carries. The probe was handed the **deck**
    /// generation, which every film in a folder shares, so an id minted on one film compared
    /// equal against the next film's catalog and matched whatever stream sat at that
    /// `local_id`: a subtitle track picked as Arabic came back as **Korean** on the next
    /// episode — ticked, no error, no way to tell. `resolve_track`'s stale guard could never
    /// fire, because the two generations were the same number.
    ///
    /// Each catalog must mint its own generation, and the deck's must stay a separate count.
    /// (Verified to fail without the fix: both sides read 0.)
    #[test]
    fn each_probe_mints_its_own_catalog_generation() {
        let mut core = test_core();
        core.source = five_photos();

        core.probe_details_blocking(0);
        let first = core.catalog_seq;
        core.probe_details_blocking(1);
        let second = core.catalog_seq;

        assert_ne!(
            first, second,
            "two files in one deck must not share a catalog generation — that is the bug"
        );
        assert!(second > first, "and it advances rather than cycling");
        // The deck did not change, so its own generation must not have moved: conflating
        // these two counts is exactly what caused the defect.
        assert_eq!(
            core.details_gen, 0,
            "the deck generation is a separate question"
        );
    }

    fn seeded_rows(core: &AppCore, item: usize) -> Vec<String> {
        let d = core.exif_cache.get(&item).expect("seeded");
        let mut rows = Vec::new();
        if let Some(cat) = &d.media {
            rows = crate::tracks::track_rows(cat, d.has_audio);
        }
        rows.iter()
            .map(|r| match r {
                DetailRow::Span { text, .. } => format!("[{text}]"),
                DetailRow::Pair { label, value } => format!("{label}: {value}"),
            })
            .collect()
    }

    /// A described catalog reaches the Details table as real per-track rows — the
    /// user-visible point of task #98 (this is what retires the `Audio: Yes` placeholder).
    #[test]
    fn a_described_catalog_becomes_per_track_details_rows() {
        let mut core = test_core();
        let cat = pb_decode::MediaTrackCatalog::new(
            1,
            pb_decode::MediaBackend::FFmpeg,
            pb_decode::TrackSet::complete(vec![track("AAC", "eng")]),
            pb_decode::TrackSet::complete(vec![]),
        );
        seed_details(&mut core, 0, Some(cat), Some(true));
        assert_eq!(
            seeded_rows(&core, 0),
            vec![
                "[Audio]",
                "Track 1: English · AAC stereo · 48 kHz",
                "Subtitles: No",
            ]
        );
    }

    /// The rule that matters most in the panel: a probe that could not enumerate a file
    /// which *does* have audio must never render as "No audio".
    #[test]
    fn an_unenumerable_catalog_never_renders_as_no_audio() {
        let mut core = test_core();
        let cat =
            pb_decode::MediaTrackCatalog::unavailable(1, pb_decode::MediaBackend::MediaFoundation);
        seed_details(&mut core, 0, Some(cat), Some(true));
        let rows = seeded_rows(&core, 0);
        assert_eq!(rows, vec!["Audio: Present — details unavailable"]);
        assert!(!rows.iter().any(|r| r == "Audio: No"));
    }

    /// A still (no catalog) adds no track rows at all.
    #[test]
    fn a_still_adds_no_track_rows() {
        let mut core = test_core();
        seed_details(&mut core, 0, None, None);
        assert!(seeded_rows(&core, 0).is_empty());
    }

    /// Land a probe result as the worker would, so the staleness rules can be driven
    /// without a real container.
    fn fake_probe(core: &mut AppCore, gen: u64, item: usize, identity: &str) {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(crate::app_core::ItemDetails {
            size: 99,
            fields: vec![("Video codec".into(), "HEVC".into())],
            media: None,
            has_audio: Some(true),
            probe_state: crate::media_details::ProbeState::Ready,
            dovi_incompatible: false,
        })
        .unwrap();
        core.exif_cache
            .insert(item, crate::app_core::ItemDetails::loading());
        core.details_probe = Some(crate::media_details::DetailsProbe {
            gen,
            item,
            identity: identity.to_string(),
            copy_when_done: false,
            rx,
        });
    }

    #[test]
    fn a_landed_probe_replaces_the_loading_entry_and_refreshes_the_panel() {
        let mut core = test_core();
        core.source = five_photos();
        core.details_gen = 3;
        let name = core.source.name(1).to_string();
        fake_probe(&mut core, 3, 1, &name);

        core.effects.clear();
        core.poll_details_probe();
        let d = core.exif_cache.get(&1).expect("cached");
        assert_eq!(d.probe_state, crate::media_details::ProbeState::Ready);
        assert_eq!(d.size, 99);
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)));
        assert!(core.details_probe.is_none());
    }

    /// The headline staleness rule: a probe that lands after a deck rebuild describes a
    /// *different file* at that index, so it must be dropped, not cached.
    #[test]
    fn a_probe_landing_after_a_deck_rebuild_is_rejected() {
        let mut core = test_core();
        core.source = five_photos();
        let name = core.source.name(1).to_string();
        fake_probe(&mut core, 3, 1, &name);
        core.details_gen = 4; // the deck was rebuilt while the worker ran

        core.poll_details_probe();
        assert_eq!(
            core.exif_cache.get(&1).map(|d| d.probe_state),
            Some(crate::media_details::ProbeState::Loading),
            "the stale result must not overwrite the entry"
        );
        assert!(core.details_probe.is_none());
    }

    /// The subtler one the generation alone can't catch: same deck generation, but index
    /// `item` now names a different file.
    #[test]
    fn a_probe_whose_item_now_names_a_different_file_is_rejected() {
        let mut core = test_core();
        core.source = five_photos();
        fake_probe(&mut core, 0, 1, "some-other-file.mp4");

        core.poll_details_probe();
        assert_eq!(
            core.exif_cache.get(&1).map(|d| d.probe_state),
            Some(crate::media_details::ProbeState::Loading),
            "identity mismatch must reject the result"
        );
    }

    /// A dead worker must not leave the entry stuck on "Reading…" forever — the
    /// placeholder is also the spawn guard, so a stuck `Loading` would never re-probe.
    #[test]
    fn a_dead_worker_marks_the_entry_failed_rather_than_hanging_on_loading() {
        let mut core = test_core();
        core.source = five_photos();
        let name = core.source.name(1).to_string();
        let (tx, rx) = std::sync::mpsc::channel::<crate::app_core::ItemDetails>();
        drop(tx); // the worker died without sending
        core.exif_cache
            .insert(1, crate::app_core::ItemDetails::loading());
        core.details_probe = Some(crate::media_details::DetailsProbe {
            gen: core.details_gen,
            item: 1,
            identity: name,
            copy_when_done: false,
            rx,
        });

        core.poll_details_probe();
        assert_eq!(
            core.exif_cache.get(&1).map(|d| d.probe_state),
            Some(crate::media_details::ProbeState::Failed)
        );
        assert!(core.details_probe.is_none());
    }

    /// The real thing, end to end: a real container, the real worker, the real poll.
    /// `ensure_exif_cached` must return **without** the catalog (it did not block), and
    /// the catalog must arrive on a later tick.
    #[cfg(any(windows, target_os = "macos", all(unix, feature = "ffvideo")))]
    #[test]
    fn a_real_video_probes_off_thread_and_lands_its_catalog() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video/multitrack.mp4");
        assert!(fixture.exists(), "fixture missing: {}", fixture.display());

        let mut core = test_core();
        core.source = Arc::new(FsSource::new(vec![fixture]));
        core.playlist = Playlist::new(1, 0);
        core.displayed_item = Some(0);

        core.ensure_exif_cached(0);
        // The event loop was not made to wait for the container open.
        assert_eq!(
            core.exif_cache.get(&0).map(|d| d.probe_state),
            Some(crate::media_details::ProbeState::Loading),
            "ensure_exif_cached must not block on the probe"
        );
        assert!(core.details_probe.is_some());
        assert!(core.work_pending(), "the probe must keep the loop ticking");

        // Spin the poll as `tick` would, with a generous bound so a slow machine can't
        // flake but a genuine hang still fails.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while core.details_probe.is_some() && std::time::Instant::now() < deadline {
            core.poll_details_probe();
            std::thread::sleep(Duration::from_millis(5));
        }
        let d = core.exif_cache.get(&0).expect("cached");
        assert_eq!(
            d.probe_state,
            crate::media_details::ProbeState::Ready,
            "probe never landed"
        );
        let cat = d.media.as_ref().expect("catalog landed");
        assert_eq!(cat.audio.tracks.len(), 2, "the fixture's two audio tracks");
        assert_eq!(d.has_audio, Some(true));
        assert!(d
            .fields
            .iter()
            .any(|(k, v)| k == "Video codec" && v == "H.264"));
        // ...and it renders as real rows.
        let rows = crate::tracks::track_rows(cat, d.has_audio);
        assert!(matches!(&rows[0], DetailRow::Span { text, bold: true } if text == "Audio"));
    }
}
