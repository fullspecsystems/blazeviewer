//! **Audio track selection** — the `AppCore` half of [`crate::tracks`] (task #125).
//!
//! The Playback menu's audio-track picker, and the row/locator plumbing that maps a chosen
//! row back onto a concrete stream — which is genuinely fiddly, because a track can be named
//! by an FFmpeg stream index, a Media Foundation stream index, or an AVFoundation property
//! list, depending on the route that opened the file.
//!
//! ⚠ Audio decodes **FFmpeg-first**: Media Foundation cannot decode AC-3, E-AC-3 or DTS
//! (`0xC00D36B4`), which is the #1 "no sound" trap on Windows. See the root `CLAUDE.md` on
//! building with the `ffprobe` feature — a bare `cargo run` omits it and every film with
//! those codecs plays silent.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Has the track probe landed for the displayed video? The audio twin of
    /// [`subtitle_tracks_known`](Self::subtitle_tracks_known), minus its session gate.
    pub fn audio_tracks_known(&self) -> bool {
        self.displayed_video_item()
            .and_then(|item| self.exif_cache.get(&item))
            .is_some_and(|d| d.media.is_some())
    }

    /// The catalog for the displayed video, if its probe has landed.
    fn showing_catalog(&self) -> Option<&pb_decode::MediaTrackCatalog> {
        self.displayed_video_item()
            .and_then(|item| self.exif_cache.get(&item))
            .and_then(|d| d.media.as_ref())
    }

    /// The Playback ▸ Audio flyout's rows. Empty = offer nothing (no video, or the probe
    /// hasn't landed — [`audio_tracks_known`](Self::audio_tracks_known) tells those
    /// apart).
    pub fn audio_picker_rows(&mut self) -> Vec<crate::tracks::PickerRow> {
        let Some(item) = self.displayed_video_item() else {
            return Vec::new();
        };
        self.ensure_exif_cached(item);
        let active = self.audio_active;
        self.showing_catalog()
            .map(|c| crate::tracks::audio_picker_rows(c, active))
            .unwrap_or_default()
    }

    /// Row `row`'s track id, if it exists.
    fn audio_row_id(&self, row: usize) -> Option<pb_decode::TrackId> {
        self.showing_catalog()
            .and_then(|c| c.audio.tracks.get(row))
            .map(|t| t.id)
    }

    /// Row `row`'s locator — **how the shell reaches this track on its route**.
    ///
    /// The two backends speak different currencies and this is the seam that hides it: the
    /// FFmpeg catalog locates a track by container stream index, AVFoundation by a
    /// serialized `AVMediaSelectionOption`. Even `local_id` means different things (a real
    /// stream index vs. a running counter), which is exactly why nothing outside may treat
    /// an id as a stream number.
    fn audio_row_locator(&self, row: usize) -> Option<&pb_decode::tracks::TrackLocator> {
        let id = self.audio_row_id(row)?;
        self.showing_catalog()?.locator(id)
    }

    /// Row `row` as an FFmpeg stream index (`-1` = this row isn't FFmpeg-located) — the
    /// sample-buffer route's currency, fed straight to `session_audio_set_track`.
    pub fn audio_row_ff_stream(&self, row: usize) -> i64 {
        match self.audio_row_locator(row) {
            Some(pb_decode::tracks::TrackLocator::FfStream(i)) => *i as i64,
            _ => -1,
        }
    }

    /// Row `row`'s serialized `AVMediaSelectionOption` (empty = this row isn't
    /// AVFoundation-located) — the AVPlayer route's currency. The spike proved this
    /// round-trips through `mediaSelectionOptionWithPropertyList:` to an option that
    /// `isEqual:` the original, which is what lets the shell re-find this exact option
    /// without trusting an ordinal.
    pub fn audio_row_av_plist(&self, row: usize) -> Vec<u8> {
        match self.audio_row_locator(row) {
            Some(pb_decode::tracks::TrackLocator::AvOption { property_list, .. }) => {
                property_list.clone()
            }
            _ => Vec::new(),
        }
    }

    /// Row `row` as a **Media Foundation reader stream index** (`-1` = this row isn't
    /// MF-located) — one of the two currencies the Windows WASAPI engine accepts.
    /// Only MF's *own* catalog mints these, which on Windows means a no-`ffprobe`
    /// build: the usual `ffprobe` build runs FFmpeg's catalog with `FfStream`
    /// locators and the engine decodes those directly (FFmpeg-first). MF's stream
    /// order differs from the container's, which is why an MF index can never be a
    /// row or an FFmpeg index in disguise.
    pub fn audio_row_mf_stream(&self, row: usize) -> i64 {
        match self.audio_row_locator(row) {
            Some(pb_decode::tracks::TrackLocator::MfStream(s)) => *s as i64,
            _ => -1,
        }
    }

    /// The picker row whose locator names MF reader stream `stream` (`-1` = none) —
    /// how the Windows shell translates the engine's "this is what is actually
    /// decoding" report into a row for [`set_active_audio_row`](Self::set_active_audio_row).
    /// The winit twin of the macOS host's `reportActiveAudioStream`.
    pub fn audio_row_for_mf_stream(&self, stream: i64) -> i64 {
        self.audio_row_for(|loc| {
            matches!(loc, pb_decode::tracks::TrackLocator::MfStream(s) if *s as i64 == stream)
        })
    }

    /// The picker row whose locator names FFmpeg stream `stream` (`-1` = none) — the
    /// same translation for the Linux engine, whose currency is container stream
    /// indices end-to-end.
    pub fn audio_row_for_ff_stream(&self, stream: i64) -> i64 {
        self.audio_row_for(|loc| {
            matches!(loc, pb_decode::tracks::TrackLocator::FfStream(i) if *i as i64 == stream)
        })
    }

    fn audio_row_for(&self, mut hit: impl FnMut(&pb_decode::tracks::TrackLocator) -> bool) -> i64 {
        let Some(catalog) = self.showing_catalog() else {
            return -1;
        };
        catalog
            .audio
            .tracks
            .iter()
            .position(|t| catalog.locator(t.id).is_some_and(&mut hit))
            .map_or(-1, |r| r as i64)
    }

    /// The shell reports which row it is **actually playing** (`-1` = unknown/none).
    ///
    /// Called on open and after every switch — including a *refused* one, where it re-states
    /// the unchanged track. That is what keeps the tick honest rather than optimistic.
    pub fn set_active_audio_row(&mut self, row: i64) {
        self.audio_active = usize::try_from(row).ok().and_then(|r| self.audio_row_id(r));
    }

    /// `A` / `Shift+A` — step to the next/previous audio track (task #99).
    ///
    /// **Emits an effect rather than switching**, because the core cannot switch audio: the
    /// player lives in the shell, the two routes reach a track by different locators, and
    /// the switch can fail. So this only decides *which row*, and the shell does it and
    /// reports back through the same `audio_track_switched` path the menu uses — one route
    /// in, one route out, so a key and a menu click cannot behave differently.
    ///
    /// Steps from **what is playing**, not from a remembered request: `audio_active` is the
    /// shell's report, so a stale pick that quietly fell back to the policy still advances
    /// from the track you can actually hear.
    pub fn cycle_audio_track(&mut self, forward: bool) {
        let Some(item) = self.displayed_video_item() else {
            return; // not a video: the key says nothing rather than lying
        };
        self.ensure_exif_cached(item);
        let Some(catalog) = self.showing_catalog() else {
            self.show_toast_icon("Reading tracks…", ToastIcon::AudioTrack);
            return;
        };
        // Only tracks we can actually play are steps — one we'd refuse would be a dead stop
        // in the rotation.
        let rows: Vec<usize> = catalog
            .audio
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.capability != pb_decode::TrackCapability::Unsupported)
            .map(|(i, _)| i)
            .collect();
        if rows.len() < 2 {
            // Nothing to step to. Say so rather than no-op: a key that does nothing is
            // indistinguishable from a key that is broken.
            let msg = if rows.is_empty() {
                "No audio tracks"
            } else {
                "Only one audio track"
            };
            self.show_toast_icon(msg, ToastIcon::AudioTrackFailed);
            return;
        }
        let here = self
            .audio_active
            .and_then(|id| catalog.audio.tracks.iter().position(|t| t.id == id))
            .and_then(|i| rows.iter().position(|r| *r == i));
        let next = match here {
            Some(i) if forward => rows[(i + 1) % rows.len()],
            Some(i) => rows[(i + rows.len() - 1) % rows.len()],
            // Not told what is playing yet (the probe or the open is still landing) — start
            // at the first rather than guessing at the policy's pick.
            None => rows[0],
        };
        self.effects
            .push(crate::contract::CoreEffect::SelectAudioTrack { row: next });
    }

    /// The shell reports the outcome of a switch it attempted: toast, and re-state the tick.
    ///
    /// **Only on a confirmed switch** (#99's rule, and the asymmetry with subtitles is
    /// deliberate). Swapping a subtitle track only re-aims a cue reader, so it can toast
    /// optimistically; audio re-opens a decoder and rebuilds a format, and either can fail
    /// while the previous track plays on. A toast naming a track over unchanged audio would
    /// teach the user to distrust every other toast in the app.
    pub fn audio_track_switched(&mut self, row: usize, ok: bool) {
        if !ok {
            self.show_toast_icon("Couldn't switch audio track", ToastIcon::AudioTrackFailed);
            return;
        }
        // The row the shell actually landed on is re-reported separately by
        // `set_active_audio_row`, so read the label back from the catalog rather than
        // trusting the request: a stale pick falls back to the policy inside the decoder,
        // and the toast must name what is *playing*.
        let label = self
            .audio_active
            .and_then(|id| {
                self.showing_catalog()?
                    .audio
                    .tracks
                    .iter()
                    .find(|t| t.id == id)
            })
            .map(crate::tracks::track_summary)
            .or_else(|| {
                self.showing_catalog()?
                    .audio
                    .tracks
                    .get(row)
                    .map(crate::tracks::track_summary)
            })
            .unwrap_or_else(|| "Audio track changed".into());
        self.show_toast_icon(&label, ToastIcon::AudioTrack);
    }
}
