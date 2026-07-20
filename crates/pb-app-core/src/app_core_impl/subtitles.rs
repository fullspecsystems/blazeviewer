//! **Subtitles** — the `AppCore` half of [`crate::subtitle`], [`crate::subtitle_engine`]
//! and [`crate::cues`] (task #125).
//!
//! Track selection, the picker rows, and the per-tick cue update.
//!
//! ⚠ `cues.rs::active_at` is the single source of truth for subtitle timing, and `pb-hud` is
//! a rasterizer only. Neither invariant may be worked around from here — the layering was
//! audited clean and is called out as such in the technical-debt audit.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Rebuild the subtitle overlay against the playhead (task #90) — the one call that
    /// joins discovery, the cue track, placement, and the rasterizer to the screen. Runs
    /// every tick; both shells then read [`AppCore::subtitles`] and composite.
    ///
    /// The clock is [`video_position`](Self::video_position) — the playhead of whichever
    /// backend is live, so a subtitle can't drift from the picture and can't be silently
    /// switched off by a routing change. On `Session` that's the session's own
    /// `desired_position`; on `Native` (macOS AVPlayer *and* the sample-buffer route) it's
    /// the shell's ~20 Hz report, which is ~50 ms granular against cues that last seconds.
    /// `Shift+C` — the next subtitle *track* (#99).
    ///
    /// Tracks only: `Off` belongs to `C` now, and `Automatic` is not a step (it resolves to
    /// one of these tracks anyway, so it would show the same subtitles twice under two
    /// names). It also switches subtitles **on** — asking for the next track when they are
    /// off can only mean you want to see one.
    ///
    /// Toasts **optimistically**, which is safe here and would not be for audio: swapping a
    /// subtitle track only re-aims a cue reader, so there is no clock to re-prime and
    /// nothing to fail silently. (Task #99's rule that audio must toast only on a
    /// *confirmed* switch stands — it is a different risk, not the same one.)
    pub fn cycle_subtitle_track(&mut self) {
        let Some(item) = self.displayed_item.filter(|_| self.video_showing()) else {
            return; // not a video: `Shift+C` says nothing rather than lying
        };
        self.ensure_exif_cached(item);
        let Some(catalog) = self.exif_cache.get(&item).and_then(|d| d.media.as_ref()) else {
            self.show_toast_icon("Reading tracks…", ToastIcon::Captions);
            return;
        };
        // What is *currently showing* — so the cycle advances from the track on screen
        // rather than jumping back to it (which is what Automatic would otherwise do).
        let showing = self
            .subtitles
            .selection
            .resolve(catalog, crate::subtitle::audio_language_of(catalog))
            .map(|t| t.id);
        let Some(next) = crate::subtitle::next_track(catalog, showing) else {
            // Nothing to step through. Say so rather than no-op'ing: a key that does
            // nothing is indistinguishable from a key that is broken.
            self.show_toast_icon("No subtitle tracks", ToastIcon::CaptionsOff);
            return;
        };
        self.apply_subtitle_choice(crate::subtitle::SubtitleChoice::Track(next));
    }

    /// Commit a picker choice: the state, the persisted on/off preference, and the toast.
    ///
    /// Shared by `Shift+C` and the picker (#99) so a choice made either way behaves
    /// identically — the alternative is two paths that agree until one of them is edited.
    ///
    /// The picker toasts too, even though its tick already moved: a track can be silent for
    /// half a minute, so without it "did that work?" has no answer until a cue happens to be
    /// due.
    fn apply_subtitle_choice(&mut self, choice: crate::subtitle::SubtitleChoice) {
        use crate::subtitle::SubtitleChoice;

        let Some(catalog) = self
            .displayed_item
            .and_then(|item| self.exif_cache.get(&item))
            .and_then(|d| d.media.as_ref())
        else {
            return; // nothing to choose from, and no catalog to read a preference out of
        };
        // The label through the same `track_summary` the Details panel uses (#98) — two
        // formatters would drift. Built before the mutable borrows below.
        let label = match choice {
            SubtitleChoice::Off => "Subtitles off".to_string(),
            SubtitleChoice::Automatic => "Subtitles automatic".to_string(),
            SubtitleChoice::Track(id) => catalog
                .subtitles
                .tracks
                .iter()
                .find(|t| t.id == id)
                .map(crate::tracks::track_summary)
                .unwrap_or_else(|| "Subtitles on".into()),
        };
        let catalog = catalog.clone(); // ends the borrow of `exif_cache`
        self.subtitles.selection.apply(choice, &catalog);

        let on = self.subtitles.selection.enabled;
        // `settings.subtitles` is the on/off preference `C` persists; keep it honest when a
        // picker row turns them off, or `C` would come back on to a state the file no
        // longer describes.
        self.settings.subtitles = on;
        if self.persist_prefs {
            self.settings.save();
        }
        self.show_toast_icon(
            &label,
            if on {
                ToastIcon::Captions
            } else {
                ToastIcon::CaptionsOff
            },
        );
    }

    /// The subtitle picker's rows for the video on screen (#99) — the playback bar's
    /// popover and the Playback ▸ Subtitles flyout read this.
    ///
    /// Empty means "offer nothing": no video, or the track probe hasn't landed yet. The
    /// shells distinguish those with [`subtitle_tracks_known`](Self::subtitle_tracks_known)
    /// rather than reading an empty list as "this file has none".
    pub fn subtitle_picker_rows(&mut self) -> Vec<crate::tracks::PickerRow> {
        let Some(item) = self.displayed_item.filter(|_| self.video_showing()) else {
            return Vec::new();
        };
        self.ensure_exif_cached(item);
        let Some(catalog) = self.exif_cache.get(&item).and_then(|d| d.media.as_ref()) else {
            return Vec::new();
        };
        crate::tracks::subtitle_picker_rows(
            catalog,
            &self.subtitles.selection,
            crate::subtitle::audio_language_of(catalog),
        )
    }

    /// Are subtitles switched on — the `C` state?
    ///
    /// The playback bar's picker button fills its icon on this, so the control reports its
    /// own state the way play/pause does. Not the same question as "is a cue on screen":
    /// subtitles are on through every silent gap between cues.
    pub fn subtitles_on(&self) -> bool {
        self.subtitles.selection.enabled
    }

    /// Has the track probe landed for the video on screen? `false` = "still reading", which
    /// is not the same answer as "no tracks" and must not be drawn as one.
    pub fn subtitle_tracks_known(&self) -> bool {
        self.displayed_item
            .filter(|_| self.video_showing())
            .and_then(|item| self.exif_cache.get(&item))
            .is_some_and(|d| d.media.is_some())
    }

    /// Apply picker row `row` — an index into the very list
    /// [`subtitle_picker_rows`](Self::subtitle_picker_rows) returned.
    ///
    /// Out-of-range is ignored rather than clamped: the only way to get one is a list that
    /// changed under the user (a nav mid-popover), and silently selecting *a different
    /// track than the one clicked* is the worst available answer.
    pub fn select_subtitle_row(&mut self, row: usize) {
        let Some(item) = self.displayed_item.filter(|_| self.video_showing()) else {
            return;
        };
        let Some(catalog) = self.exif_cache.get(&item).and_then(|d| d.media.as_ref()) else {
            return;
        };
        let Some(&next) = crate::subtitle::picker_choices(catalog).get(row) else {
            return;
        };
        self.apply_subtitle_choice(next);
    }

    pub fn tick_subtitles(&mut self) {
        self.subtitles.poll(self.details_gen);

        // Every "nothing should be on screen" case leaves through HERE, and this exit
        // always clears. Off used to get its own early `return` — which skipped the clear,
        // so pressing `C` left the last cue frozen on screen forever. One exit, one rule:
        // an overlay can never outlive the state that produced it.
        let (displayed, active) = (self.displayed_item, self.video_showing());
        let on = self.subtitles.selection.enabled;
        // `|| always_forced` (task #99): with dialogue subtitles off we must still get far
        // enough to look for a *forced* track, because forced signs are part of the film —
        // `resolve_display` is what decides, and it can't decide from behind this gate.
        let forced = self.subtitles.selection.always_forced;
        let Some(item) = displayed.filter(|_| active && (on || forced)) else {
            self.subtitles.trace(|| {
                format!(
                    "idle: displayed_item={displayed:?} session_active={active} on={on} \
                     always_forced={forced}"
                )
            });
            self.subtitles.clear_item();
            return;
        };
        // Cost of `always_forced` being on by default, stated honestly: a video now pays the
        // ~20 ms header probe below and the one-per-session 261 ms rasterizer build even with
        // subtitles off. Both are off-thread and behind `video_showing()`, so **the photo path
        // is untouched** — which is the property that actually matters here. Turning the
        // setting off restores "off costs nothing" exactly.
        //
        // The track catalog is the Details probe's, and it is what holds the container's
        // own subtitle streams *and* the sidecars beside it in one id namespace — so
        // subtitles cannot select anything until it lands. Drive the probe rather than
        // waiting for someone to open the Inspector: idempotent, guarded by its own
        // `Loading` placeholder, ~20 ms of container header, and only ever reached with
        // subtitles on and a video playing.
        self.ensure_exif_cached(item);
        let source = Arc::clone(&self.source);
        let deck_gen = self.details_gen;
        // Split the borrow: `ensure_loaded` needs `&mut subtitles` while it reads the
        // catalog out of `exif_cache`, and they are disjoint fields.
        let Self {
            subtitles,
            exif_cache,
            ..
        } = self;
        let catalog = exif_cache.get(&item).and_then(|d| d.media.as_ref());
        let audio_lang = catalog.and_then(crate::subtitle::audio_language_of);
        subtitles.ensure_loaded(&source, item, deck_gen, catalog, audio_lang);

        let Some((x, y, w, h, _rot)) = self.video_placement() else {
            self.subtitles
                .trace(|| "no video_placement — the still geometry isn't up yet".into());
            // Hide, don't just leave: with no geometry there is nowhere correct to draw,
            // and a kept overlay would hang at its last position — the same defect the
            // exit above had.
            self.subtitles.hide();
            return;
        };
        let Some(t) = self.video_position() else {
            self.subtitles
                .trace(|| "no playhead yet — the shell hasn't reported one".into());
            self.subtitles.hide();
            return;
        };
        let vp = (self.viewport.width as f32, self.viewport.height as f32);
        let video = crate::subtitle::Rect { x, y, w, h };
        // `controls_h` is 0 until a shell reports its transport bar's height — the lift
        // exists in `place()`, nothing measures it yet.
        //
        // No redraw is requested on a change: a playing video already draws every frame,
        // which is the only state this runs in.
        self.subtitles
            .update(t, vp, video, 0.0, self.viewport.scale_factor);
    }
}
