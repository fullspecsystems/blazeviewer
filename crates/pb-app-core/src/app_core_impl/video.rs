//! **The video session** — the `AppCore` half of [`crate::video`],
//! [`crate::video_session`] and [`crate::video_native`] (task #125).
//!
//! Opening a session, the platform route (Media Foundation / FFmpeg / AVPlayer / sample
//! buffer), seeking, frame stepping, the progress row, posters, and the per-tick poll.
//!
//! ⚠ Two things here are easy to get wrong and expensive to rediscover:
//!
//! - **The audio clock is the master.** Pacing decisions belong to `VideoSession`, which is
//!   backend-blind and unit-tested; nothing here should second-guess it.
//! - **The reader thread is demand-driven and never blocks the event loop.** Everything
//!   reports back as effects or messages. If a change here needs to wait for the decoder,
//!   it is in the wrong place.
//!
//! A video's poster frame is its blaze-mode face, which is why poster selection rides the
//! same decode pool under the same priority rules as any other frame.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// The current item's on-screen placement for the **macOS native video layer**
    /// (task 79.9 phase 3): the same geometry the wgpu still renderer computes in
    /// `quad_vertices` — `ViewTransform::placement` against the content region below the
    /// top-bar inset, then slid down by the inset — so the `AVPlayerLayer` tracks Fit/Fill/
    /// Original, zoom, pan, and rotation identically to a photo. Returns
    /// `(x, y, w, h, rotation)` in **physical px, top-left origin**; `w`/`h` are the
    /// *rotated* footprint and `rotation` is the CW quadrant (0/1/2/3 = 0/90/180/270) the
    /// shell rotates the layer by about its center. `None` before the renderer/fit exist.
    /// Take the in-RAM container bytes stashed for a `PlayVideoBytes` (macOS archive
    /// video) — the shell pulls them once and serves them to `AVPlayer` via a resource
    /// loader. Empty if none pending. Consumes the stash.
    pub fn take_pending_video_bytes(&mut self) -> Vec<u8> {
        self.pending_video_bytes.take().unwrap_or_default()
    }

    /// Take the archive-video **poster** bytes stashed for `request_id` (macOS). The shell
    /// pulls them, generates a poster frame, and returns it via `video_poster_ready`. Consumes.
    pub fn take_pending_poster_bytes(&mut self, request_id: u64) -> Vec<u8> {
        self.pending_poster_bytes
            .remove(&request_id)
            .unwrap_or_default()
    }

    /// macOS: keep Swift-generated posters flowing for archive videos — the displayed one
    /// **and the prefetch window ahead**, so advancing to the next clip shows its poster
    /// with no placeholder gap. Two steps each tick:
    ///
    /// 1. Drain finished off-thread byte reads → stash + emit `RequestVideoPoster` (or clear
    ///    the in-flight guard if the read failed).
    /// 2. For archive-video placeholders (resident *previews*) not already in flight — the
    ///    displayed item first, then the direction-biased prefetch targets — spawn an
    ///    off-thread byte read, up to a small concurrency cap (bounds transient RAM: each
    ///    read holds one container copy until Swift consumes it).
    ///
    /// A poster upgrades its placeholder in place and the item leaves `preview_resident`, so
    /// it stops being a candidate; a ring eviction re-placeholders it and it re-qualifies.
    /// macOS: land a finished off-thread archive-video **playback** read — if the session is
    /// still current, stash the bytes and emit `PlayVideoBytes`; a stale read (the user
    /// navigated away) is dropped, an empty one (read error) surfaces a toast.
    #[cfg(target_os = "macos")]
    pub fn drain_archive_video_read(&mut self) {
        while let Ok((id, name, muted, bytes)) = self.video_read_rx.try_recv() {
            let current = self
                .video
                .as_ref()
                .and_then(ActiveVideoBackend::as_native)
                .map(|p| p.session_id)
                == Some(id);
            if !current {
                continue; // a newer session (or none) — the read is stale
            }
            if bytes.is_empty() {
                self.show_toast("couldn't read the video from the archive");
                self.video = None;
                self.update_video_progress();
                continue;
            }
            // A remembered position for this archive item (task #94.2).
            let item = self
                .video
                .as_ref()
                .and_then(ActiveVideoBackend::as_native)
                .map(|p| p.item);
            let start_secs = item
                .and_then(|i| self.video_resume.get(&i))
                .map_or(0.0, |d| d.as_secs_f64());
            self.pending_video_bytes = Some(bytes);
            self.effects.push(contract::CoreEffect::PlayVideoBytes {
                name,
                session_id: id,
                muted,
                start_secs,
            });
        }
    }

    #[cfg(target_os = "macos")]
    pub fn request_archive_posters(&mut self) {
        // Cap concurrent reads/generations: the displayed clip + a couple ahead covers the
        // advance gap without holding many full containers in RAM at once.
        const MAX_INFLIGHT: usize = 3;

        // 1. Finished reads: stash the bytes + ask the shell to generate the poster.
        while let Ok((request_id, item, bytes)) = self.poster_read_rx.try_recv() {
            // A read whose request is no longer the tracked one (the deck changed and a
            // new-deck request re-used the index) is a straggler — drop it whole rather
            // than stash bytes / clear a marker that now belongs to the replacement.
            if self.poster_inflight.get(&item) != Some(&request_id) {
                continue;
            }
            if bytes.is_empty() {
                self.poster_inflight.remove(&item); // read failed — allow a later retry
                continue;
            }
            let name = self.source.name(item).to_string();
            let max_edge = self
                .decode_fit()
                .map(|f| f.max_width.max(f.max_height))
                .unwrap_or(2048)
                .max(1);
            self.pending_poster_bytes.insert(request_id, bytes);
            self.effects.push(contract::CoreEffect::RequestVideoPoster {
                request_id,
                item,
                name,
                max_edge,
            });
        }

        // 2. Spawn reads for the next placeholders, in priority order, up to the cap.
        if self.poster_inflight.len() >= MAX_INFLIGHT {
            return;
        }
        let mut candidates: Vec<usize> = Vec::new();
        if let Some(d) = self.displayed_item {
            candidates.push(d);
        }
        candidates.extend(self.targets.iter().copied());
        for item in candidates {
            if self.poster_inflight.len() >= MAX_INFLIGHT {
                break;
            }
            if self.poster_inflight.contains_key(&item) // already in flight (also dedups this list)
                || self.source.path(item).is_some() // loose file — the pool posters those
                || !self.preview_resident.contains(&item) // placeholder not resident yet
                || !self.item_is_video(item)
            {
                continue;
            }
            self.poster_req_seq += 1;
            let request_id = self.poster_req_seq;
            self.poster_inflight.insert(item, request_id);
            let source = self.source.clone();
            let tx = self.poster_read_tx.clone();
            // Off the event loop: a ZIP entry inflates here (7z copies from resident RAM).
            std::thread::spawn(move || {
                let bytes = source.bytes(item).unwrap_or_default();
                let _ = tx.send((request_id, item, bytes));
            });
        }
    }

    /// A macOS archive-video poster the shell generated (via `AVAssetImageGenerator`) — feed
    /// it into the resident ring as a synthetic full-decode [`Outcome`](crate::decode_pool::Outcome),
    /// upgrading the preview placeholder in place through the normal `drain_results` path
    /// (so retention + prefetch come for free). Dropped if the pixel count is wrong.
    pub fn video_poster_ready(
        &mut self,
        request_id: u64,
        item: usize,
        w: u32,
        h: u32,
        rgba: Vec<u8>,
    ) {
        // Drop a straggler whose request we no longer expect — item indices are
        // deck-relative, so it must not upgrade a same-index item in a new deck. The id
        // check (not just item membership, #119 diff review) is what makes this hold when
        // the NEW deck has already re-requested the same index: the straggler's stale id
        // no longer matches the marker's owner, so the replacement's marker survives.
        if self.poster_inflight.get(&item) != Some(&request_id) {
            return;
        }
        self.poster_inflight.remove(&item);
        if w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
            return;
        }
        let img = pb_decode::DecodedImage {
            width: w,
            height: h,
            orig_width: w,
            orig_height: h,
            codec: "Video",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: rgba,
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
            recovered: None,
        };
        let rep_kind = self.display_kind();
        self.pending_uploads
            .push(crate::decode_pool::Outcome::synthetic(
                item,
                self.epoch,
                self.content_gen,
                rep_kind,
                Ok(img),
            ));
    }

    /// Cache the inspector's video-fact rows for a macOS archive video, probed by the shell
    /// via AVFoundation (Rust can't build an `AVAsset` from bytes). Mirrors the row set the
    /// Windows/loose-file probe produces: Duration / Video codec / Frame rate / Audio.
    /// Re-signals the panel so an already-open inspector refreshes. `fps_milli` = fps×1000;
    /// `duration_ms` < 0 = unknown.
    ///
    /// **This is the thin path.** It carries no track catalog, so on a build where the
    /// FFmpeg backend can read the entry's bytes itself (`media_details::probe_job`, task
    /// 98.7) that probe produces strictly more — and this must not overwrite it. Hence the
    /// richer-wins guard below rather than a blind `insert`: the two race by construction
    /// (a detached Swift `Task` vs. a Rust worker) and whichever lands second would
    /// otherwise win on timing alone.
    pub fn archive_video_meta_ready(
        &mut self,
        item: usize,
        codec: String,
        fps_milli: u32,
        duration_ms: i64,
        has_audio: bool,
    ) {
        // A catalog-bearing entry is strictly richer than anything this path can build.
        if self
            .exif_cache
            .get(&item)
            .is_some_and(|d| d.media.is_some())
        {
            return;
        }
        let mut rows: Vec<(String, String)> = Vec::new();
        if duration_ms > 0 {
            let d = std::time::Duration::from_millis(duration_ms as u64);
            rows.push(("Duration".into(), crate::video::format_video_duration(d)));
        }
        if !codec.is_empty() {
            rows.push(("Video codec".into(), codec));
        }
        if fps_milli > 0 {
            rows.push((
                "Frame rate".into(),
                format!("{:.2} fps", f64::from(fps_milli) / 1000.0),
            ));
        }
        rows.push(("Audio".into(), if has_audio { "Yes" } else { "No" }.into()));
        let size = self.source.size_hint(item).unwrap_or(0);
        self.exif_cache.insert(
            item,
            crate::app_core::ItemDetails {
                size,
                fields: rows,
                // No catalog on this path. That is **not** a gap in practice: every macOS
                // build that can play an archived video also links FFmpeg — the shipped DMG
                // (`release-macos.sh` → `--bundle-ffmpeg`, which implies `--ffvideo`) and
                // dev builds (`build-swift-host.sh`, `--ffvideo` by default) — so
                // `media_details::probe_job` reads the entry's bytes and produces the real
                // catalog, and the guard above keeps it. This path is the fallback for the
                // one build without FFmpeg (`release-macos.sh --no-video`), where MKV/WebM
                // don't play at all. Not worth an FFI to carry a catalog across.
                media: None,
                has_audio: Some(has_audio),
                // The shell already probed this one; there is no worker to wait on.
                probe_state: crate::media_details::ProbeState::Ready,
                // AVFoundation probed it — that path doesn't parse the DoVi record.
                dovi_incompatible: false,
            },
        );
        self.emit_panels_changed();
    }

    pub fn video_placement(&self) -> Option<(f32, f32, f32, f32, u8)> {
        let (iw, ih, sw, sh) = self.screen_and_image()?;
        let content_h = sh.saturating_sub(self.content_top_inset).max(1);
        let mut p = self.view.placement(iw, ih, sw, content_h);
        p.y += self.content_top_inset as f32;
        let rotation = match self.view.rotation {
            Rotation::R0 => 0,
            Rotation::R90 => 1,
            Rotation::R180 => 2,
            Rotation::R270 => 3,
        };
        Some((p.x, p.y, p.w, p.h, rotation))
    }

    /// The active video's Windows/Linux [`VideoSession`] bundle, if the backend is
    /// `Session` (`None` on macOS, where playback is the shell's native `AVPlayer`
    /// and there is no session to drive). The producer-driving methods below funnel
    /// through these so they naturally no-op on the `Native` backend.
    fn session_ref(&self) -> Option<&crate::video_session::ActiveVideo> {
        self.video.as_ref().and_then(ActiveVideoBackend::as_session)
    }
    fn session_mut(&mut self) -> Option<&mut crate::video_session::ActiveVideo> {
        self.video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
    }

    /// `P` on a video item (task #79 phase 4): toggle the streaming session —
    /// pause/resume while it runs, start (or restart after end/failure) otherwise.
    /// On macOS (the `Native` backend) the session-op arms are inert — pause/
    /// resume/replay of a live native player are wired with input parity (79.9
    /// phase 3) via the `PauseVideo`/`ResumeVideo`/`SeekVideoFraction` commands;
    /// the "start fresh" default still opens playback through `start_video_session`.
    pub fn video_play_pause(&mut self, item: usize) {
        use crate::video::VideoSessionState::*;
        let existing = self.video.as_ref().map(|v| (v.item(), v.state()));
        match existing {
            Some((playing_item, state)) if playing_item == item => match state {
                Playing => self.video_pause_current(),
                Paused => self.video_resume_current(),
                Ended => self.video_replay_current(),
                Failed | Stopped => self.start_video_session(item),
                // Starting up (or mid-seek later): let it be.
                Opening | Buffering | Seeking => {}
            },
            // A different item's session (stale) or none: start fresh.
            _ => self.start_video_session(item),
        }
    }

    /// Pause the current video — the Session path pauses the `VideoSession` (+ its
    /// audio player); the Native (macOS) path commands the shell's `AVPlayer`. Each
    /// arm returns the effect to emit so the `self.video` borrow is released before
    /// `self.effects`/`self.draw` (disjoint-field borrows aside, this stays clean).
    fn video_pause_current(&mut self) {
        let now = self.now;
        let cmd = match self.video.as_mut() {
            Some(ActiveVideoBackend::Session(v)) => {
                v.session.pause(now);
                Some(contract::CoreEffect::PauseVideoAudio)
            }
            Some(ActiveVideoBackend::Native(p)) => Some(contract::CoreEffect::PauseVideo {
                session_id: p.session_id,
            }),
            None => None,
        };
        if let Some(cmd) = cmd {
            self.effects.push(cmd);
            self.draw();
        }
    }

    /// Resume the current video (from `Paused`).
    fn video_resume_current(&mut self) {
        let now = self.now;
        let mut flush: Option<Duration> = None;
        let cmd = match self.video.as_mut() {
            Some(ActiveVideoBackend::Session(v)) => {
                let was_paused = v.session.state() == crate::video::VideoSessionState::Paused;
                v.session.resume(now);
                if was_paused {
                    // Flush a landed-but-uncommitted seek BEFORE the resume, so
                    // audio rejoins at the seeked position, never the stale one.
                    flush = v.pending_audio_commit.take();
                    v.scrub_audio_paused = false;
                    v.last_seek_intent = None;
                }
                Some(contract::CoreEffect::ResumeVideoAudio)
            }
            Some(ActiveVideoBackend::Native(p)) => Some(contract::CoreEffect::ResumeVideo {
                session_id: p.session_id,
            }),
            None => None,
        };
        if let Some(position) = flush {
            self.effects
                .push(contract::CoreEffect::SeekVideoAudio { position });
        }
        if let Some(cmd) = cmd {
            self.effects.push(cmd);
            self.draw();
        }
    }

    /// Replay from the top (`P` at `Ended`). Session: a seek to 0 on the SAME session
    /// (the producer parks after EOS for this). Native: `ResumeVideo`, which the shell
    /// resolves as seek-to-0-then-play when the player is parked at the end.
    fn video_replay_current(&mut self) {
        let now = self.now;
        enum Replay {
            Session,
            Native(contract::CoreEffect),
        }
        let action = match self.video.as_mut() {
            Some(ActiveVideoBackend::Session(v)) => v.session.replay(now).map(|_| Replay::Session),
            Some(ActiveVideoBackend::Native(p)) => {
                Some(Replay::Native(contract::CoreEffect::ResumeVideo {
                    session_id: p.session_id,
                }))
            }
            None => None,
        };
        match action {
            Some(Replay::Session) => {
                // 1D: audio pauses now; the landing at 0 commits the audio seek
                // and the resume follows it (in order), via the coordinator.
                self.note_video_seek_intent();
                self.draw();
            }
            Some(Replay::Native(cmd)) => {
                self.effects.push(cmd);
                self.draw();
            }
            None => {}
        }
    }

    /// The session id of the active macOS native video (`0` = none). The shell reconciles
    /// its `AVPlayer` against this each pump: if it holds a player the core no longer has
    /// (a torn-down/replaced session), it tears that player down — a belt-and-suspenders
    /// against a missed `StopVideo` leaving a second video playing.
    pub fn native_video_session_id(&self) -> u64 {
        self.video
            .as_ref()
            .and_then(ActiveVideoBackend::as_native)
            .map(|p| p.session_id.0)
            .unwrap_or(0)
    }

    /// The player finished opening: record duration + audio presence.
    pub fn native_video_opened(&mut self, session_id: u64, duration_ms: i64, has_audio: bool) {
        let sid = crate::video::VideoSessionId(session_id);
        let duration = (duration_ms >= 0).then(|| Duration::from_millis(duration_ms as u64));
        if let Some(p) = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
        {
            p.on_opened(sid, duration, has_audio);
        }
        self.update_video_progress();
    }

    /// The player's playback state changed (`state`: 0 Opening / 1 Buffering / 2 Playing
    /// / 3 Paused — `Ended`/`Failed` have their own callbacks).
    pub fn native_video_state_changed(&mut self, session_id: u64, state: u8) {
        let sid = crate::video::VideoSessionId(session_id);
        let st = match state {
            1 => crate::video::VideoSessionState::Buffering,
            2 => crate::video::VideoSessionState::Playing,
            3 => crate::video::VideoSessionState::Paused,
            _ => crate::video::VideoSessionState::Opening,
        };
        let changed = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
            .is_some_and(|p| p.on_state_changed(sid, st));
        if changed {
            self.update_video_progress();
            self.draw();
        }
    }

    /// The player reached end-of-stream (parks the last frame; `P` replays).
    pub fn native_video_ended(&mut self, session_id: u64) {
        let sid = crate::video::VideoSessionId(session_id);
        let applied = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
            .is_some_and(|p| p.on_ended(sid));
        if applied {
            self.update_video_progress();
            self.draw();
        }
    }

    /// A seek acknowledged (`finished` = landed cleanly; `false` = superseded by a
    /// newer seek). Clears the proxy's in-flight flag for the current generation.
    pub fn native_video_seek_completed(
        &mut self,
        session_id: u64,
        generation: u64,
        finished: bool,
    ) {
        let sid = crate::video::VideoSessionId(session_id);
        let gen = crate::video::SeekGeneration(generation);
        if let Some(p) = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
        {
            p.on_seek_completed(sid, gen, finished);
        }
    }

    /// The player failed. `recoverable` is the shell's error classification
    /// (task #84 §8a level 2): `true` for demux/codec-shaped failures where the
    /// FFmpeg fallback is worth attempting; `false` for missing-file /
    /// permission / DRM / network errors, which no other backend can fix.
    ///
    /// With `ffvideo` built in, a recoverable failure on the displayed item
    /// **retries through the FFmpeg session before any error surfaces** — the
    /// user sees exactly one final error only if both backends fail (the
    /// session's own failure path owns that toast). Otherwise: surface the
    /// error and return to the poster, mirroring the Session `poll_video`
    /// failure path.
    pub fn native_video_failed(&mut self, session_id: u64, error: String, recoverable: bool) {
        let sid = crate::video::VideoSessionId(session_id);
        let failed_item = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
            .and_then(|p| p.on_failed(sid, error.clone()).then_some(p.item));
        let Some(item) = failed_item else { return };
        // Stop + detach the failed AVPlayer either way.
        self.effects
            .push(contract::CoreEffect::StopVideo { session_id: sid });
        self.video = None;
        self.update_video_progress();
        #[cfg(all(target_os = "macos", feature = "ffvideo"))]
        if recoverable && Some(item) == self.displayed_item {
            // §8a fallback: no toast before the FFmpeg attempt; the flag makes
            // the very next start route to the session, consumed there.
            self.video_ffmpeg_fallback = Some(item);
            self.start_video_session(item);
            return;
        }
        #[cfg(not(all(target_os = "macos", feature = "ffvideo")))]
        let _ = (recoverable, item);
        let msg = if error.is_empty() {
            "Video playback failed".to_string()
        } else {
            error
        };
        self.show_toast(&msg);
    }

    /// Start video playback of `item` (task #79 phase 4 / task #84 §8a): a fresh
    /// `VideoSession` fed by a dedicated reader thread — Media Foundation on
    /// Windows, the FFmpeg producer on Linux and on macOS for everything
    /// `AVPlayer` doesn't handle (MKV/WebM included; smoothness plan) — or, on
    /// macOS for nominally-native containers, the shell's `AVPlayer`. The
    /// producer thread is never joined — teardown is a Stop message / channel
    /// disconnect.
    // The early return is load-bearing when the session block below compiles in
    // (macOS + ffvideo); without the feature it's the trailing statement and
    // clippy calls it needless — allow rather than fork the body per cfg.
    #[allow(clippy::needless_return)]
    pub fn start_video_session(&mut self, item: usize) {
        self.stop_video();
        // macOS routing (§8a; smoothness plan): AVPlayer for what it handles well
        // (MP4/MOV); the Session route (FFmpeg → wgpu → Metal) for everything else,
        // including MKV/WebM — it presents smoothly where the sample-buffer
        // presenter drops frames. The presenter is parked opt-in
        // (`sample_buffer_opt_in`, the DoVi reference renderer) and still level-2
        // falls back to Session on a classified failure.
        #[cfg(target_os = "macos")]
        {
            // A classified native/sample-buffer failure forces the Session route
            // exactly once (level 2): consume the flag here so neither Apple route
            // is retried for this same item.
            #[cfg(feature = "ffvideo")]
            let forced_session = self.video_ffmpeg_fallback.take() == Some(item);
            #[cfg(not(feature = "ffvideo"))]
            let forced_session = false;
            if !forced_session {
                if self.macos_native_route(item) {
                    self.start_native_video(item);
                    return;
                }
                #[cfg(feature = "ffvideo")]
                if self.macos_sample_buffer_route(item) {
                    self.start_sample_buffer_video(item);
                    return;
                }
            }
        }
        // Session platforms: Windows (MF), Linux (FFmpeg), and the macOS FFmpeg
        // route above falling through (task #84 §8a).
        #[cfg(any(windows, all(unix, feature = "ffvideo")))]
        {
            let fit = self.decode_fit();
            // Credit-granting estimate of one fitted RGBA8 frame. The fit box is a
            // conservative bound (aspect makes real frames smaller); no fit
            // (Fill/Original modes) assumes 4K.
            let frame_bytes = fit
                .map(|f| f.max_width as u64 * f.max_height as u64 * 4)
                .unwrap_or(3840 * 2160 * 4);
            self.video_seq += 1;
            let id = crate::video::VideoSessionId(self.video_seq);
            let (session, io) = crate::video_session::VideoSession::new(id, frame_bytes);
            let generation = crate::video::SeekGeneration::FIRST;
            // Planar GPU color path (task #91 Phase 2): the producer emits NV12/P010
            // for eligible clips when the renderer supports it and the escape hatch
            // isn't set. Captured here (off the producer thread) from the renderer's
            // real device capability.
            let planar_opts = self.planar_video_options();
            // The media slot `poll_video`'s audio start reads; the producer thread
            // shares the same Arc, so both pipelines read ONE copy of the container.
            let media: std::sync::Arc<std::sync::OnceLock<crate::video::VideoInput>> =
                std::sync::Arc::new(std::sync::OnceLock::new());
            if let Some(path) = self.source.path(item).map(Path::to_path_buf) {
                let input = crate::video::VideoInput::Path(path);
                let _ = media.set(input.clone());
                std::thread::spawn(move || {
                    run_platform_video_producer(
                        &input,
                        fit,
                        id,
                        generation,
                        io.events,
                        io.msgs,
                        io.cancel,
                        planar_opts,
                    );
                });
            } else {
                // Archive entry: fetch the container bytes OFF the event loop (a
                // ZIP entry inflates; a 7z copies from its resident RAM), publish
                // them through the media slot — before the producer can report
                // `Opened`, so the audio start always finds them — then run the
                // same producer through the bytes seam. RAM-only end to end:
                // nothing is ever extracted to disk (privacy #2).
                let source = self.source.clone();
                let name = self.source.name(item).to_string();
                let media_slot = media.clone();
                std::thread::spawn(move || {
                    let data = match source.bytes(item) {
                        Ok(data) => std::sync::Arc::new(data),
                        Err(e) => {
                            let _ = io.events.send(crate::video::VideoProducerEvent::Failed {
                                session_id: id,
                                error: format!("couldn't read the video from the archive: {e}"),
                            });
                            return;
                        }
                    };
                    let input = crate::video::VideoInput::Bytes { data, name };
                    let _ = media_slot.set(input.clone());
                    run_platform_video_producer(
                        &input,
                        fit,
                        id,
                        generation,
                        io.events,
                        io.msgs,
                        io.cancel,
                        planar_opts,
                    );
                });
            }
            // A remembered position for this item (task #94.2) → resume there once
            // the session can seek (`poll_video`), held from a stale re-scan clear.
            let resume_to = self.video_resume.get(&item).copied();
            self.video = Some(ActiveVideoBackend::Session(
                crate::video_session::ActiveVideo {
                    session,
                    item,
                    audio_started: false,
                    media,
                    scrub_audio_paused: false,
                    pending_audio_commit: None,
                    last_seek_intent: None,
                    dbg_seek_land_at: None,
                    resume_to,
                },
            ));
            self.anim_hint_shown_for = Some(item); // engaged — retire the hint
                                                   // Honest DoVi UX (macos-video-smoothness §2): warm the container probe
                                                   // (async, never blocks) and warn now if it already landed; otherwise
                                                   // `poll_details_probe` warns when it does. Also warms the track
                                                   // pickers, which read the same catalog.
            self.ensure_exif_cached(item);
            self.maybe_warn_dovi(item);
            self.draw();
        }
        #[cfg(not(any(windows, target_os = "macos", all(unix, feature = "ffvideo"))))]
        {
            let _ = item;
            self.show_toast("Video playback is not available yet on this platform");
        }
    }

    /// One-time honest-UX warning (macos-video-smoothness §2): the item playing on
    /// the **Session route** carries a Dolby Vision stream whose base layer cannot
    /// show correct color without RPU reshaping (Profile 5 / compat-id 0 — the
    /// green/purple tint). AVPlayer and the opted-in sample-buffer presenter decode
    /// DoVi natively, so only a Session backend warns. Called from
    /// `start_video_session` (probe already cached) and `poll_details_probe` (probe
    /// landing mid-playback); `dovi_warned` makes it once per item.
    pub(super) fn maybe_warn_dovi(&mut self, item: usize) {
        let session_here = self
            .video
            .as_ref()
            .and_then(|v| v.as_session())
            .is_some_and(|s| s.item == item);
        let incompatible = self
            .exif_cache
            .get(&item)
            .is_some_and(|d| d.dovi_incompatible);
        if session_here && incompatible && self.dovi_warned.insert(item) {
            self.show_toast("Dolby Vision (Profile 5) — colors can't be shown correctly");
        }
    }

    /// macOS §8a routing: `true` = try the shell's `AVPlayer`. Known-unsupported
    /// containers (MKV/WebM/WMV/MPEG-PS/AVCHD) route to the FFmpeg session
    /// (level 1), and a just-failed classified native attempt forces the
    /// session exactly once (level 2 — the flag is consumed here, so a later
    /// fresh open retries native first). Without `ffvideo` there is no FFmpeg
    /// backend and everything stays native (the failure toast is the outcome).
    #[cfg(target_os = "macos")]
    fn macos_native_route(&mut self, item: usize) -> bool {
        // The level-2 fallback flag is consumed by the caller (start_video_session)
        // so it can skip both Apple routes at once; this is now a pure container test.
        #[cfg(not(feature = "ffvideo"))]
        {
            let _ = item;
            true
        }
        #[cfg(feature = "ffvideo")]
        {
            match crate::video::item_kind(self.source.as_ref(), item) {
                crate::video::LibraryItemKind::Video(c) => c.macos_native(),
                // Not a video (unreachable from the play paths) — native no-op.
                // A door reaches `P` but enters an archive rather than playing,
                // so it never gets here either.
                crate::video::LibraryItemKind::Image
                | crate::video::LibraryItemKind::Archive(_) => true,
            }
        }
    }

    /// `true` = use the macOS **sample-buffer presenter** (FFmpeg demux →
    /// `AVSampleBufferDisplayLayer`) for this item. **Default OFF** — the presenter
    /// drops ~3 frames/sec on steady-state playback that both `AVPlayer` and the
    /// Session route play flawlessly (measured; see
    /// `.taskmaster/plans/macos-video-smoothness.md`), so MKV/WebM route to the
    /// Session route (FFmpeg → wgpu → Metal). The presenter is **parked, not
    /// deleted**: it is the on-device Dolby-Vision reference renderer, opt-in via
    /// `PB_SAMPLE_BUFFER=1` ([`AppCore::sample_buffer_opt_in`], read once at host
    /// construction — tests set the field directly). When opted in it keeps the old
    /// restrictions: loose-file **MKV/WebM** only, self-probing the codec and
    /// falling back to Session (level 2) for anything it can't sample-decode.
    /// Reached only for non-native containers (`macos_native_route` runs first).
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    fn macos_sample_buffer_route(&self, item: usize) -> bool {
        if !self.sample_buffer_opt_in || self.source.path(item).is_none() {
            return false;
        }
        match crate::video::item_kind(self.source.as_ref(), item) {
            crate::video::LibraryItemKind::Video(c) => matches!(
                c,
                crate::video::VideoContainer::Mkv | crate::video::VideoContainer::Webm
            ),
            // Neither is a video, so neither uses the presenter.
            crate::video::LibraryItemKind::Image | crate::video::LibraryItemKind::Archive(_) => {
                false
            }
        }
    }

    /// Start the macOS sample-buffer presenter for `item` (Phase 3). Mirrors
    /// [`Self::start_native_video`]: the core keeps only a passive `Native` proxy
    /// (the presenter fires the same `native_video_*` callbacks), and the demux
    /// container input is carried on the effect for the host to stash + open off
    /// the main actor. Loose-file only for now; an archive item (no file URL)
    /// falls back to the Session route.
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    fn start_sample_buffer_video(&mut self, item: usize) {
        let Some(path) = self.source.path(item).map(Path::to_path_buf) else {
            // No file URL (archive) — not yet supported on this route. Force the
            // Session route (the flag is consumed there, so no re-entry here).
            self.video_ffmpeg_fallback = Some(item);
            self.start_video_session(item);
            return;
        };
        self.video_seq += 1;
        let id = crate::video::VideoSessionId(self.video_seq);
        let muted = self.effective_mute();
        let start_secs = self
            .video_resume
            .get(&item)
            .map_or(0.0, |d| d.as_secs_f64());
        self.video = Some(ActiveVideoBackend::Native(
            crate::video_native::NativeVideoProxy::new(item, id, muted),
        ));
        self.effects.push(contract::CoreEffect::PlaySampleBuffer {
            input: crate::video::VideoInput::Path(path),
            session_id: id,
            muted,
            start_secs,
        });
        self.anim_hint_shown_for = Some(item); // engaged — retire the hint
        self.draw();
    }

    /// macOS native playback (task 79.9): the shell's `AVPlayer` owns the whole
    /// pipeline. The core keeps only a passive `Native` proxy and commands the
    /// player via `PlayVideo`; the shell reveals the layer on the first frame and
    /// reports state back through the `native_video_*` callbacks (79.9 phase 2).
    #[cfg(target_os = "macos")]
    fn start_native_video(&mut self, item: usize) {
        self.video_seq += 1;
        let id = crate::video::VideoSessionId(self.video_seq);
        let muted = self.effective_mute();
        // A remembered position for this item (task #94.2) → the shell seeks the
        // player here before revealing/playing. `0.0` = from the start.
        let start_secs = self
            .video_resume
            .get(&item)
            .map_or(0.0, |d| d.as_secs_f64());
        if let Some(path) = self.source.path(item).map(Path::to_path_buf) {
            self.video = Some(ActiveVideoBackend::Native(
                crate::video_native::NativeVideoProxy::new(item, id, muted),
            ));
            self.effects.push(contract::CoreEffect::PlayVideo {
                path,
                session_id: id,
                muted,
                start_secs,
            });
        } else {
            // Archive entry: no file URL. Read the container bytes OFF the event loop (a
            // large ZIP inflates; 7z copies from resident RAM), then — once they arrive
            // (drained in the tick) — hand them to the shell, which serves them to
            // `AVPlayer` through a resource loader. RAM-only, never to disk (privacy #2).
            // The proxy is live now so the session is gated; the poster holds until play.
            self.video = Some(ActiveVideoBackend::Native(
                crate::video_native::NativeVideoProxy::new(item, id, muted),
            ));
            let name = self.source.name(item).to_string();
            let source = self.source.clone();
            let tx = self.video_read_tx.clone();
            std::thread::spawn(move || {
                let bytes = source.bytes(item).unwrap_or_default();
                let _ = tx.send((id, name, muted, bytes));
            });
        }
        self.anim_hint_shown_for = Some(item); // engaged — retire the hint
        self.draw();
    }

    /// One seek step on the active video (task #79 phase 6): ±2 s, Shift ±10 s,
    /// relative to the **desired** target so a held key scrubs the intent. Seeks
    /// audio alongside and surfaces the position feedback.
    pub fn video_seek(&mut self, back: bool) {
        let step = if self.mods.shift {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(2)
        };
        // Native backend (macOS): AVPlayer owns the clock, so the core issues only a
        // relative, generation-gated seek *intent*; the shell resolves it against the
        // player and clamps to the seekable range (the proxy holds no position). The
        // live position comes back through the periodic progress observer.
        if let Some(p) = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_native_mut)
        {
            let session_id = p.session_id;
            let generation = p.begin_seek();
            let delta = i64::try_from(step.as_millis()).unwrap_or(i64::MAX);
            self.effects.push(contract::CoreEffect::SeekVideoBy {
                session_id,
                generation,
                delta_ms: if back { -delta } else { delta },
            });
            self.arm_video_line_flash(); // reveal the controls during a keyboard seek
            return;
        }
        let now = self.now;
        let Some(v) = self.session_mut() else {
            return; // no active session backend
        };
        let Some(target) = v.session.seek_by(back, step, now) else {
            return;
        };
        // 1D: audio pauses once per seek run; the ONE audio seek (+ resume)
        // commits in `poll_video` after the run settles — never per step.
        self.note_video_seek_intent();
        self.video_position_feedback(target);
    }

    /// Register a Session-backend seek intent with the 1D audio coordinator:
    /// pause the shell audio player once per run, supersede any landed-but-
    /// uncommitted position, and restart the settle window. `poll_video` emits
    /// the single `SeekVideoAudio` (+ resume if the clip plays on) once no new
    /// intent has arrived for [`VIDEO_SEEK_AUDIO_SETTLE`] — so a held key or a
    /// scrubber drag never stops/seeks/refills the audio decoder per target (R4).
    fn note_video_seek_intent(&mut self) {
        let now = self.now;
        let Some(v) = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
        else {
            return;
        };
        v.pending_audio_commit = None;
        v.last_seek_intent = Some(now);
        let first_of_run = !v.scrub_audio_paused;
        v.scrub_audio_paused = true;
        if first_of_run {
            self.effects.push(contract::CoreEffect::PauseVideoAudio);
        }
    }

    /// Absolute seek from the playback bar (a click/drag on the info line's bar,
    /// task #79 follow-up): `frac` of the clip's duration. Same seek semantics as
    /// the keyboard — playing resumes at the target, paused shows it and stays.
    pub fn video_seek_fraction(&mut self, frac: f32) {
        let now = self.now;
        let Some(v) = self.session_mut() else {
            return; // macOS drives seeks through the native player (79.9 phase 3)
        };
        let Some(d) = v.session.duration else {
            return; // no duration → no bar to click
        };
        let target = Duration::from_secs_f64(d.as_secs_f64() * f64::from(frac.clamp(0.0, 1.0)));
        if v.session.seek_to(target, now, None).is_none() {
            return;
        }
        // 1D: drag spam coalesces exactly like held keys — commit on settle.
        self.note_video_seek_intent();
        self.update_video_progress();
    }

    /// `,`/`.` on a video item (task #79 follow-up): step one frame, pausing
    /// playback first (the same contract as animation frame-step). Forward serves
    /// the next decoded frame straight from the session queue — instant; backward
    /// is a paused one-frame seek (the reader re-runs the GOP, a normal seek
    /// landing). Returns whether an active video consumed the step.
    pub fn video_frame_step(&mut self, delta: i32) -> bool {
        use crate::video::VideoSessionState::*;
        // Native backend (macOS): the shell drives AVPlayerItem.step(byCount:) — it pauses
        // first and no-ops when the item can't step that direction. The proxy's paused state
        // syncs back via the state_changed callback.
        if let Some(p) = self.video.as_ref().and_then(ActiveVideoBackend::as_native) {
            if Some(p.item) != self.displayed_item || matches!(p.state(), Failed | Stopped) {
                return false;
            }
            let session_id = p.session_id;
            self.effects.push(contract::CoreEffect::StepVideo {
                session_id,
                forward: delta > 0,
            });
            self.arm_video_line_flash();
            return true;
        }
        let now = self.now;
        let displayed = self.displayed_item;
        let outcome = {
            // Inline the field borrow (not the `session_mut` helper): `v` must borrow
            // only `self.video` so `self.effects` stays usable alongside it below.
            let Some(v) = self
                .video
                .as_mut()
                .and_then(ActiveVideoBackend::as_session_mut)
            else {
                return false; // macOS: native player frame-step (79.9 phase 3)
            };
            if Some(v.item) != displayed || matches!(v.session.state(), Failed | Stopped) {
                return false;
            }
            // Stepping is scrubbing, not playback: pause first (like animations).
            if v.session.state() == Playing {
                v.session.pause(now);
                self.effects.push(contract::CoreEffect::PauseVideoAudio);
            }
            v.session.step_frame(delta > 0, now)
        };
        match outcome {
            crate::video_session::FrameStep::Present(frame) => {
                let pts = frame.pts;
                self.present_video_frame(&frame);
                // A settled instant step: sync the paused audio player directly
                // (so a later resume doesn't yank the clock back) and supersede
                // any landed-but-uncommitted seek — this step is newer intent.
                if let Some(v) = self
                    .video
                    .as_mut()
                    .and_then(ActiveVideoBackend::as_session_mut)
                {
                    v.pending_audio_commit = None;
                    v.last_seek_intent = None;
                }
                self.effects
                    .push(contract::CoreEffect::SeekVideoAudio { position: pts });
                self.draw();
                self.video_position_feedback(pts);
            }
            crate::video_session::FrameStep::Seeking(target) => {
                // 1D: the landing commits the audio seek at the landed PTS.
                self.note_video_seek_intent();
                self.video_position_feedback(target);
            }
            crate::video_session::FrameStep::None => {}
        }
        true
    }

    /// Surface a video seek/step position to the user. The info line's playback
    /// row is the readout (owner call 2026-07-11): if the line is on it already
    /// tracks the target; if it's off, **flash the line** for a beat instead of
    /// the old `m:ss / m:ss` toast — the line looks better and does more. The
    /// toast survives only where the line can't flash (HUD shells, Tab-hidden).
    fn video_position_feedback(&mut self, target: Duration) {
        if self.info_line && self.info_line_visible() {
            return; // the persistent line's row tracks the seek already
        }
        if self.arm_video_line_flash() {
            return;
        }
        let osd = match self.video.as_ref().and_then(|v| v.duration()) {
            Some(d) => format!(
                "{} / {}",
                crate::video::format_video_duration(target),
                crate::video::format_video_duration(d)
            ),
            None => crate::video::format_video_duration(target),
        };
        self.show_toast(&osd);
    }

    /// Stop and drop any active video session (navigation, delete, teardown). The
    /// currently displayed frame stays on screen; the caller decides what replaces it.
    /// Record — or forget — a video's session-only resume position (task #94.2),
    /// applying the [`video_resume_target`] policy: a spot meaningfully into a
    /// long-enough clip is remembered (rewound a touch); a near-start / near-end /
    /// watched-to-the-end position FORGETS any prior entry so returning restarts.
    /// Keyed by item index; RAM-only. Both backends funnel their position here.
    fn note_video_position(&mut self, item: usize, pos: Duration, dur: Duration) {
        match video_resume_target(pos, dur) {
            Some(target) => {
                self.video_resume.insert(item, target);
            }
            None => {
                self.video_resume.remove(&item);
            }
        }
    }

    /// Shell → core: the macOS native player's current position (task #94.2). The
    /// core holds no native clock, so the shell reports it each pump; this folds it
    /// into the resume map so returning to the item resumes where it left off. Only
    /// the live session's reports count (session-gated).
    pub fn native_video_progress(
        &mut self,
        session_id: u64,
        position_secs: f64,
        duration_secs: f64,
    ) {
        let sid = crate::video::VideoSessionId(session_id);
        // Mirror the playhead onto the proxy (task #90): on this backend the shell owns the
        // clock, so this ~20 Hz report is the core's only view of where the picture is —
        // and subtitle cues need it. Stored on every report, independent of the resume
        // bookkeeping below, which deliberately ignores a near-start/end position.
        if position_secs >= 0.0 {
            if let Some(p) = self
                .video
                .as_mut()
                .and_then(ActiveVideoBackend::as_native_mut)
                .filter(|p| p.session_id == sid)
            {
                p.set_position(Duration::from_secs_f64(position_secs));
            }
        }
        let item = self
            .video
            .as_ref()
            .and_then(ActiveVideoBackend::as_native)
            .filter(|p| p.session_id == sid)
            .map(|p| p.item);
        if let Some(item) = item {
            if duration_secs > 0.0 && position_secs >= 0.0 {
                self.note_video_position(
                    item,
                    Duration::from_secs_f64(position_secs),
                    Duration::from_secs_f64(duration_secs),
                );
            }
        }
    }

    pub fn stop_video(&mut self) {
        let now = self.now;
        if let Some(v) = self.video.take() {
            match v {
                ActiveVideoBackend::Session(mut s) => {
                    // Remember where we're leaving off (task #94.2) before teardown,
                    // so returning to this item resumes near here (or forgets a
                    // watched-to-the-end clip so it restarts). RAM-only.
                    if let Some(dur) = s.session.duration {
                        let pos = s.session.desired_position(now);
                        self.note_video_position(s.item, pos, dur);
                    }
                    s.session.stop();
                    self.effects.push(contract::CoreEffect::StopVideoAudio);
                }
                // macOS: tear down the native player (which owns its own audio);
                // stale callbacks are rejected by session id.
                ActiveVideoBackend::Native(p) => {
                    self.effects.push(contract::CoreEffect::StopVideo {
                        session_id: p.session_id,
                    });
                }
            }
            self.update_video_progress(); // drops the playback row promptly
                                          // A flashed seek OSD dies with its session (don't linger a bare line).
            if self.video_osd_until.take().is_some() {
                self.emit_panels_changed();
            }
            // A geometry change deferred while this video played: refill the ring
            // now that the decode pool can't jerk the playback (the displayed
            // frame stays; navigation's own load handles the current item).
            if std::mem::take(&mut self.video_geometry_stale) {
                self.target_item = self.playlist.current();
                self.request_prefetch();
            }
            // A resize-pause dies with its session (never resume a later one).
            self.video_paused_by_resize = false;
        }
    }

    /// Per-tick video drive (task #79 phases 4+5): poll the session, present the
    /// due frame through the reusable present path, keep the shell audio player in
    /// lockstep with the session state, surface failures.
    pub fn poll_video(&mut self) {
        // Session backends only (Windows/Linux, and macOS for the containers
        // AVPlayer doesn't handle — MKV/WebM since the smoothness plan). The
        // macOS `AVPlayer` route has no session to pump — it runs itself and
        // reports back via callbacks.
        // Inline the field borrow (not the helper) so `v` borrows only `self.video`,
        // leaving `self.now`/`self.effects`/`self.source` usable below.
        let now = self.now;
        // Is the keyboard seek key released? `video_seek_last` is set on every held
        // repeat and cleared the tick a horizontal-seek key lifts (`apply_view_holds`,
        // which runs AFTER this in the tick — so this reads last tick's state, ~8 ms
        // stale, which is fine). Drives the adaptive audio-commit below. Captured here
        // because `v` borrows `self.video` exclusively.
        let seek_key_released = self.video_seek_last.is_none();
        let Some(v) = self
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
        else {
            return;
        };
        let mut update = v.session.poll(now);
        let state = v.session.state();
        let started = v.session.has_started();
        let session_id = v.session.id;
        // PB_TRACE: the Session route's objective smoothness numbers
        // (macos-video-smoothness §4) — the analog of the sample-buffer route's
        // `sb-play diag`. `dropped` counts late frames the catch-up drain
        // discarded (plan 1C/0B); `rebuf` counts mid-play starvation freezes
        // (the other stutter flavor — a network read spike that empties the
        // queue freezes rather than drops). A healthy clip holds both at 0.
        // ~Every 2 s while playing, with the dropped delta since the last line.
        if pb_trace() && state == crate::video::VideoSessionState::Playing {
            let stale = self.video_diag_last.is_none_or(|(id, t, _)| {
                id != session_id || now.saturating_duration_since(t) >= Duration::from_secs(2)
            });
            if stale {
                let dropped = v.session.dropped_frames();
                let prev = self
                    .video_diag_last
                    .filter(|(id, _, _)| *id == session_id)
                    .map_or(0, |(_, _, n)| n);
                eprintln!(
                    "[pb-video] session diag: dropped={dropped} (+{}) rebuf={} pos={:.1}s",
                    dropped.saturating_sub(prev),
                    v.session.rebuffers(),
                    v.session.position(now).as_secs_f64()
                );
                self.video_diag_last = Some((session_id, now, dropped));
            }
        }
        // One-shot resume seek (task #94.2): once the fresh session can accept a
        // seek, jump to the remembered position. The poster is held (present
        // suppressed) until then, and the seek flushes the pre-resume frames by
        // generation, so returning to a video lands where you left off with no
        // start-flash. Wired into the 1D audio coordinator below (same fields a
        // user seek sets), so audio lands at the resume point too.
        let mut resume_pause_audio = false;
        if v.resume_to.is_some() {
            if state != crate::video::VideoSessionState::Opening {
                if let Some(target) = v.resume_to.take() {
                    if v.session.seek_to(target, now, None).is_some() {
                        v.pending_audio_commit = None;
                        resume_pause_audio = !v.scrub_audio_paused;
                        v.scrub_audio_paused = true;
                        v.last_seek_intent = Some(now);
                    }
                }
            }
            update.present = None; // hold the poster until the resume frame lands
        }
        // 1D audio-seek coordinator: a landing stores the commit position (only
        // the latest generation lands, so this is inherently supersede-safe);
        // the commit fires once the run settles — no new seek intent for
        // VIDEO_SEEK_AUDIO_SETTLE — producing exactly ONE audio seek per run.
        if let Some(pos) = update.seek_landed {
            v.pending_audio_commit = Some(pos);
            // The picture is now at the target; stamp it so the audio commit below
            // can report how long after that it fired (the A/V-gap settle residual).
            v.dbg_seek_land_at = Some(now);
        }
        // Adaptive settle (task #4 follow-up): a discrete tap commits its audio seek
        // fast — once the seek key is released and a brief quiet has passed — instead
        // of always waiting out the full window. Held scrubbing keeps the key down, so
        // it only ever clears the full-window fallback (which its 200 ms repeats keep
        // resetting until release), never the fast path. The full window also covers
        // any input that doesn't drive `video_seek_last` (a scrubber drag), where
        // `seek_key_released` reads true — but there the frequent drag intents keep
        // resetting even the short quiet, so it still coalesces.
        let elapsed = |win: Duration| {
            v.last_seek_intent
                .is_none_or(|t| now.saturating_duration_since(t) >= win)
        };
        let settled = elapsed(VIDEO_SEEK_AUDIO_SETTLE)
            || (seek_key_released && elapsed(VIDEO_SEEK_AUDIO_QUIET));
        let commit = if settled {
            v.pending_audio_commit.take()
        } else {
            None
        };
        // A/V-gap measurement (PB_AV_SYNC): capture the delays now, while `v` is
        // borrowed and BEFORE `resume_with_commit` below clears `last_seek_intent`.
        // The land delay is the settle residual the user perceives as "audio catching
        // up"; the WASAPI reseek (PB_AUDIO_TRACE) stacks on top.
        let dbg_av_delays = commit.map(|_| {
            (
                v.dbg_seek_land_at
                    .take()
                    .map(|l| now.saturating_duration_since(l)),
                v.last_seek_intent.map(|t| now.saturating_duration_since(t)),
            )
        });
        let resume_with_commit = if commit.is_some() {
            let was_scrub_paused = v.scrub_audio_paused;
            v.scrub_audio_paused = false;
            v.last_seek_intent = None;
            was_scrub_paused && state == crate::video::VideoSessionState::Playing
        } else {
            false
        };
        let scrub_paused = v.scrub_audio_paused;
        // Start the shell audio player the moment the producer reports a track
        // (Opened), paused — it opens in parallel with the video preroll and the
        // two resume together. Silent clips never create a player. The player
        // opens the SAME container the producer reads (the media slot): a path,
        // or an archive entry's shared in-RAM bytes. The slot is always set by
        // the time `Opened` lands (the producer thread fills it first).
        let start_audio = !v.audio_started && v.session.has_audio() == Some(true);
        if start_audio {
            v.audio_started = true;
        }
        let audio_input = if start_audio {
            v.media.get().cloned()
        } else {
            None
        };
        if let Some(input) = audio_input {
            let muted = self.effective_mute();
            self.effects.push(contract::CoreEffect::StartVideoAudio {
                input,
                session_id,
                muted,
            });
        }
        // The resume seek pauses audio once for its run (after StartVideoAudio so
        // the player exists; it opens paused anyway). Its landing commits the ONE
        // SeekVideoAudio (+ resume) below via the same 1D path as a user seek.
        if resume_pause_audio {
            self.effects.push(contract::CoreEffect::PauseVideoAudio);
        }
        // The settled seek run's ONE audio commit: seek, then resume (in that
        // order) if the clip plays on. Emitted before the state bridge below so
        // audio can never resume at a pre-seek position.
        if let Some(pos) = commit {
            if let (Some((land_delay, intent_delay)), true) =
                (dbg_av_delays, std::env::var_os("PB_AV_SYNC").is_some())
            {
                eprintln!(
                    "[pb-avsync] audio seek committed to {:.2}s — {:?} after the picture landed, {:?} after the last seek input",
                    pos.as_secs_f64(),
                    land_delay,
                    intent_delay,
                );
            }
            self.effects
                .push(contract::CoreEffect::SeekVideoAudio { position: pos });
            if resume_with_commit {
                self.effects.push(contract::CoreEffect::ResumeVideoAudio);
            }
        }
        // Session state drives the audio player (freeze together, resume
        // together): Playing = resume; a mid-play rebuffer or the end = pause.
        // While a seek run holds audio paused (`scrub_paused`), Playing
        // promotions do NOT resume — the commit above owns the resume, so
        // intermediate landings of a held/scrubbed run stay silent (1D).
        if update.state_changed {
            use crate::video::VideoSessionState::*;
            match state {
                Playing if !scrub_paused && commit.is_none() => {
                    self.effects.push(contract::CoreEffect::ResumeVideoAudio);
                }
                // A landing's Seeking→Buffering hop while the run already holds
                // audio paused would just spam redundant pauses — skip those.
                Buffering if started && !scrub_paused => {
                    self.effects.push(contract::CoreEffect::PauseVideoAudio);
                }
                Ended => self.effects.push(contract::CoreEffect::PauseVideoAudio),
                _ => {}
            }
        }
        if let Some(frame) = update.present {
            self.present_video_frame(&frame);
            // The frame (and its CPU pixels) drops here — released after upload.
            self.draw();
            return;
        }
        if update.state_changed {
            match state {
                crate::video::VideoSessionState::Failed => {
                    let msg = self
                        .session_ref()
                        .and_then(|v| v.session.error.clone())
                        .unwrap_or_else(|| "Video playback failed".into());
                    self.video = None;
                    self.effects.push(contract::CoreEffect::StopVideoAudio);
                    self.show_toast(&msg);
                }
                // Ended parks on the last presented frame; P replays.
                _ => self.draw(),
            }
        }
    }

    /// Arm (or refresh) the transient info-line reveal while a video is active —
    /// shared by the seek/step OSD and the pointer hover reveal. `true` when the
    /// flash path applies (native-line shells, chrome not Tab-hidden); the tick
    /// arm drops the line at the deadline.
    fn arm_video_line_flash(&mut self) -> bool {
        if !self.native_info || self.panels.hidden || self.current.is_none() {
            return false;
        }
        let fresh = self.video_osd_until.is_none();
        self.video_osd_until = Some(self.now + VIDEO_OSD_HOLD);
        if fresh {
            self.show_info_line();
            self.emit_panels_changed();
        }
        true
    }

    /// Pointer hover over the **controls zone** — the bottom quarter of the
    /// window, where the info line lives — reveals the playback controls while a
    /// video is active, like every video player (owner request). It's the same
    /// transient reveal the seek OSD uses: refreshed on every pointer move inside
    /// the zone, decaying via the tick arm once the pointer leaves. Shell-neutral
    /// policy: pointer moves arrive as `CoreEvent::PointerMoved` from every shell
    /// (the macOS SwiftUI shell shares this the moment it forwards its hovers).
    pub fn video_hover_reveal(&mut self, y: f32) {
        use crate::video::VideoSessionState::*;
        if self.info_line {
            return; // the persistent line already shows the controls
        }
        if y < self.viewport.height as f32 * (1.0 - VIDEO_HOVER_ZONE) {
            return;
        }
        let active = self.video.as_ref().is_some_and(|v| {
            Some(v.item()) == self.displayed_item && !matches!(v.state(), Failed | Stopped)
        });
        if active {
            self.arm_video_line_flash();
        }
    }

    /// Re-arm the transient controls reveal directly (no hover geometry). The macOS shell
    /// calls this when the user releases the info-line scrubber: a SwiftUI drag captures the
    /// pointer, so canvas pointer-moves — and thus [`video_hover_reveal`] — stop firing, and
    /// the flash would snap away the instant the drag ends. This lets it fade out gracefully
    /// instead. Same active guard as the hover path so it can't flash the line for a still.
    pub fn flash_video_controls(&mut self) {
        use crate::video::VideoSessionState::*;
        if self.info_line {
            return; // the persistent line is already up
        }
        let active = self.video.as_ref().is_some_and(|v| {
            Some(v.item()) == self.displayed_item && !matches!(v.state(), Failed | Stopped)
        });
        if active {
            self.arm_video_line_flash();
        }
    }

    /// The planar-video producer options for a new session (task #91 Phase 2):
    /// attempt the planar GPU color path unless `PB_VIDEO_NO_PLANAR` disables it
    /// (the A/B lever / safety hatch), reporting the renderer's real P010
    /// capability so 10-bit sources fall back to RGBA/fp16 on adapters without
    /// `TEXTURE_FORMAT_16BIT_NORM`. No renderer (headless) → no planar path.
    ///
    /// Gated to match its only call site (the session-platform block in
    /// `start_video_playback`): without `ffvideo` on macOS, video is the native
    /// AVFoundation player, so there is no producer to hand options to and this is dead
    /// code — which `cargo clippy --all-targets -- -D warnings`, the documented lint
    /// command (it passes no features), rejects.
    #[cfg(any(windows, all(unix, feature = "ffvideo")))]
    fn planar_video_options(&self) -> pb_decode::VideoProducerOptions {
        let planar = std::env::var_os("PB_VIDEO_NO_PLANAR").is_none() && self.renderer.is_some();
        let supports_p010 = self.renderer.as_ref().is_some_and(|r| r.supports_p010());
        pb_decode::VideoProducerOptions {
            planar,
            supports_p010,
        }
    }

    /// Upload one decoded video frame through the reusable present path,
    /// dispatching on its pixel format (task 79.10): RGBA8 rides `set_image`
    /// exactly as before; NV12 splits its planes and goes through the renderer's
    /// `set_video_nv12` (in-shader YUV on wgpu; a CPU convert on fallback shells);
    /// fp16 HDR frames (task #84 plan §9) ride `set_image`'s HDR arm with the
    /// frame's scene-linear `peak` — the same fp16 scRGB present path as HDR
    /// stills, so PQ/HLG video gets real headroom on an EDR/HDR surface and a
    /// correct tone-map on SDR, never an RGBA8 clip.
    pub(super) fn present_video_frame(&mut self, frame: &pb_decode::VideoFrame) {
        let item = self.video.as_ref().map(|v| v.item());
        // The metadata half of a present (owner report 2026-07-16): video frames
        // stream around `present_item`, and the first frame's `mark_resolved`
        // below makes a still-decoding poster skip its own present when it lands
        // — so a video started before the poster (P beats a slow SMB poster
        // decode every time) left `current` unset for the whole session, and
        // with it the info line, the `i` toggle, the hover reveal, and the
        // playback controls (`arm_video_line_flash` requires metadata). The
        // poster's meta still reaches `meta_cache` when its decode completes
        // (`drain_results` caches it unconditionally) — adopt it the moment it
        // exists.
        if let Some(item) = item {
            if self.current.is_none() && self.displayed_item == Some(item) {
                self.current = self.meta_cache.get(&item).cloned();
            }
        }
        {
            let Some(a) = self.renderer.as_mut() else {
                return;
            };
            if frame.format.is_planar_video() {
                // NV12 / P010 (task 79.10 / #91 Phase 2): split at the checked Y-plane
                // span (never a raw `split_at`, which would panic on a short buffer —
                // though `VideoSession` already rejects malformed frames) and hand the
                // two planes to the in-shader planar path.
                if let Some((y_len, _uv_off, _uv_len)) =
                    frame.format.planar_plane_spans(frame.width, frame.height)
                {
                    let (y, uv) = frame.pixels.split_at(y_len.min(frame.pixels.len()));
                    a.set_video_planar(
                        y,
                        uv,
                        frame.width,
                        frame.height,
                        crate::engine::render_planar_present(frame.format, &frame.color),
                    );
                }
            } else {
                a.set_image(
                    &frame.pixels,
                    frame.width,
                    frame.height,
                    render_color(&frame.color.transform),
                    frame.format == pb_decode::PixelFormat::Rgba16F,
                    frame.color.peak,
                );
            }
        }
        // Each presented frame re-resolves the video item at the current epoch, so a
        // resize during playback keeps `target_caught_up` true (no loading pie over live
        // video) — `present_video_frame` streams frames without going through
        // `present_item` (task #18 finding #5).
        if let Some(item) = item {
            self.mark_resolved(item);
        }
    }

    /// Shell → core: the platform video-audio player's latest clock sample
    /// (task #79 phase 5). Routed to the active session, which uses it as the
    /// master clock while both sides play.
    pub fn video_audio_clock(&mut self, sample: crate::video::AudioClockSample) {
        // Session backends only — on macOS the native `AVPlayer` is its own clock.
        let now = self.now;
        if let Some(v) = self.session_mut() {
            v.session.on_audio_clock(sample, now);
        }
    }

    /// The info line's playback row for the displayed item's live video session
    /// (`None` on stills / dead sessions — the line renders single-row as always).
    /// Public: the winit shell's egui info line (and later the macOS SwiftUI one)
    /// reads it to draw the `elapsed ▰▰▰▱▱ total` row natively.
    pub fn video_progress_row(&self) -> Option<hud::ProgressRow> {
        // Session backends only: the row is computed from the session's clock. On
        // macOS the SwiftUI info row reads the native `AVPlayer` directly (79.9
        // phase 5), so the core provides no progress there.
        let v = self.session_ref()?;
        if Some(v.item) != self.displayed_item {
            return None;
        }
        use crate::video::VideoSessionState::*;
        if matches!(v.session.state(), Failed | Stopped) {
            return None;
        }
        let pos = v.session.desired_position(self.now);
        let (total, fraction) = match v.session.duration {
            Some(d) if !d.is_zero() => (
                Some(crate::video::format_video_duration(d)),
                (pos.as_secs_f32() / d.as_secs_f32()).clamp(0.0, 1.0),
            ),
            _ => (None, 0.0),
        };
        Some(hud::ProgressRow {
            elapsed: crate::video::format_video_duration(pos),
            total,
            fraction,
        })
    }

    /// Whether the DISPLAYED item plays through the cross-platform
    /// `VideoSession` backend — on macOS that's the FFmpeg route (task #84 §8),
    /// and the SwiftUI shell keys its controls visibility + scrubber routing on
    /// this (its `nativeVideo` checks cover only the `Native` backend).
    pub fn video_session_active(&self) -> bool {
        use crate::video::VideoSessionState::*;
        self.session_ref().is_some_and(|v| {
            Some(v.item) == self.displayed_item && !matches!(v.session.state(), Failed | Stopped)
        })
    }

    /// The active session's playhead in seconds — raw numbers for the SwiftUI
    /// scrubber (the winit shell reads the formatted [`Self::video_progress_row`]
    /// instead). `0.0` when no session is active.
    pub fn video_session_elapsed_secs(&self) -> f64 {
        match self.session_ref() {
            Some(v) if self.video_session_active() => {
                v.session.desired_position(self.now).as_secs_f64()
            }
            _ => 0.0,
        }
    }

    /// Is a video actually on screen right now — **on either backend**?
    ///
    /// [`video_session_active`](Self::video_session_active) answers only for the
    /// `VideoSession` route. On macOS the sample-buffer presenter (the default for
    /// MKV/WebM since Phase 3F) and AVPlayer are both `Native` backends, so a
    /// session-only check reads false while a video plays — which silently disabled
    /// subtitles the moment that route became the default. Anything asking "is the user
    /// watching a video" wants this, not that.
    pub fn video_showing(&self) -> bool {
        use crate::video::VideoSessionState::*;
        self.video.as_ref().is_some_and(|b| {
            Some(b.item()) == self.displayed_item && !matches!(b.state(), Failed | Stopped)
        })
    }

    /// The playhead of whatever is playing, on either backend. `None` when nothing is, or
    /// before the shell's first position report on the `Native` route.
    pub fn video_position(&self) -> Option<Duration> {
        self.video
            .as_ref()
            .filter(|_| self.video_showing())
            .and_then(|b| b.position(self.now))
    }

    /// The displayed item when it is a **video** — by its own kind, or because a video
    /// session is showing for it. The audio picker's gate (owner, 2026-07-17): the track
    /// catalog belongs to the *item* (the details probe), so it must not require a
    /// running session — gating on `video_showing()` made the flyout claim "No Video"
    /// over a film sitting at its poster.
    pub(super) fn displayed_video_item(&self) -> Option<usize> {
        self.displayed_item
            .filter(|&i| self.item_is_video(i) || self.video_showing())
    }

    /// Is the displayed item a video (playing or not)? The shell's flyout gate.
    pub fn displayed_is_video(&self) -> bool {
        self.displayed_video_item().is_some()
    }

    /// The active session's duration in seconds; `0.0` when unknown/none (the
    /// scrubber renders duration-less streams without a bar, like the native path).
    pub fn video_session_duration_secs(&self) -> f64 {
        match self.session_ref() {
            Some(v) if self.video_session_active() => {
                v.session.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0)
            }
            _ => 0.0,
        }
    }

    /// Whether the displayed item's video is playing right now (session
    /// `Playing`). The winit shell's playback row uses it two ways: the
    /// play/pause button's glyph, and a timed egui repaint so the knob glides
    /// between the once-a-second text refreshes; anything paused/parked keeps
    /// the overlay fully retained.
    pub fn video_playing(&self) -> bool {
        self.video
            .as_ref()
            .is_some_and(|v| Some(v.item()) == self.displayed_item && v.is_playing())
    }

    /// Keep the info line's playback row in step with the session (task #79):
    /// refresh the line only when the displayed second (or the row's presence)
    /// changes — once per second while playing, never per frame. A natively-drawn
    /// line (the winit egui overlay, macOS) gets a panels-changed marker so the
    /// shell re-pulls; the HUD path re-rasterizes directly.
    pub fn update_video_progress(&mut self) {
        let desired = self
            .video_progress_row()
            .map(|r| (r.elapsed, r.total.unwrap_or_default()))
            .map(|(a, b)| format!("{a}/{b}"));
        if desired == self.video_pill_text {
            return;
        }
        self.video_pill_text = desired;
        if !self.info_line_visible() {
            return;
        }
        if self.native_info {
            self.emit_panels_changed(); // the shell re-renders its info line
        } else {
            self.show_info_line(); // re-raster with (or without) the playback row
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{
        core_with_a_native_video, seed_details, test_core, track,
    };

    /// **Regression.** Subtitles gated on `video_session_active()`, which is false for the
    /// Native backend. When the macOS sample-buffer route became the default for MKV/WebM,
    /// that silently turned subtitles off for exactly the files they were built for — no
    /// error, no failing test, just nothing on screen.
    ///
    /// "Is a video on screen" must not depend on which backend is drawing it.
    #[test]
    fn a_native_backed_video_counts_as_showing() {
        let core = core_with_a_native_video();
        assert!(
            core.video_showing(),
            "the sample-buffer / AVPlayer route is still a video playing"
        );
        assert!(
            !core.video_session_active(),
            "and it is NOT session-backed — which is exactly why the old check failed"
        );
    }

    /// The playhead has to come from whichever backend is live. On Native the shell owns
    /// the clock and reports it ~20 Hz; before the first report there is simply no answer,
    /// and inventing one would put cues out of step with the picture.
    #[test]
    fn the_native_playhead_comes_from_the_shells_reports() {
        let mut core = core_with_a_native_video();
        assert_eq!(core.video_position(), None, "no report yet — no answer");

        core.native_video_progress(7, 12.5, 100.0);
        assert_eq!(core.video_position(), Some(Duration::from_secs_f64(12.5)));

        core.native_video_progress(7, 13.0, 100.0);
        assert_eq!(core.video_position(), Some(Duration::from_secs_f64(13.0)));
    }

    /// A report from a torn-down player must never move the live one's clock — the same
    /// session-identity rule every other native callback follows.
    #[test]
    fn a_stale_sessions_progress_does_not_move_the_playhead() {
        let mut core = core_with_a_native_video();
        core.native_video_progress(7, 12.5, 100.0);
        core.native_video_progress(999, 88.0, 100.0); // a straggler from a dead session
        assert_eq!(
            core.video_position(),
            Some(Duration::from_secs_f64(12.5)),
            "a straggler must not be believed"
        );
    }

    /// A core with a live `VideoSession` on item 0 — the state `tick_subtitles` only
    /// does real work in, and the state the switched-off bug needed to appear.
    fn core_with_a_playing_video() -> AppCore {
        let mut core = test_core();
        let (session, _io) =
            crate::video_session::VideoSession::new(pb_decode::VideoSessionId(1), 1 << 20);
        core.video = Some(crate::video_native::ActiveVideoBackend::Session(
            crate::video_session::ActiveVideo::new(session, 0),
        ));
        core.displayed_item = Some(0);
        // Leak the producer end: dropping it would fail the session, and this core never
        // decodes anything — it exists to make `video_session_active()` true.
        std::mem::forget(_io);
        assert!(
            core.video_session_active(),
            "the fixture must actually be active"
        );
        core
    }

    /// **Regression.** Pressing `C` with a cue on screen left it frozen there forever.
    ///
    /// `update()` hides correctly when the mode is Off — and a unit test proved it. But
    /// the tick had its own `if Off { return }` fast path that never called `update()`, so
    /// the bitmap and its generation just sat there and the shell kept drawing the last
    /// cue. The test passed; the feature was broken. This one drives `tick_subtitles`,
    /// which is where the bug actually lived.
    #[test]
    fn switching_subtitles_off_clears_a_cue_that_is_on_screen() {
        use crate::subtitle::SubtitleSelection;
        let mut core = core_with_a_playing_video();
        core.subtitles.selection = SubtitleSelection::automatic();
        core.subtitles.force_showing_for_test();
        let before = core.subtitles.gen();

        core.subtitles.selection = SubtitleSelection::off();
        core.tick_subtitles();

        assert!(
            core.subtitles.bitmap().is_none(),
            "the last cue must not survive being switched off"
        );
        assert!(
            core.subtitles.gen() > before,
            "the shell only stops drawing when the generation moves"
        );
    }

    /// The Windows (WASAPI) currency accessors (task #99): each row resolves in
    /// whichever currency its locator carries, a stream resolves back to its row, and a
    /// row without a locator in the asked currency answers `-1` — the shell's cue to try
    /// the other currency or refuse. Both currencies coexist because the engine takes
    /// either (FFmpeg's own catalog carries `FfStream`; MF's fallback catalog `MfStream`).
    #[test]
    fn audio_stream_accessors_round_trip_in_both_currencies() {
        let mut core = core_with_a_native_video();
        let mut a0 = track("AAC", "eng");
        a0.id.local_id = 1;
        let mut a1 = track("AC-3", "fra");
        a1.id.local_id = 2;
        let mut catalog = pb_decode::MediaTrackCatalog::new(
            1,
            pb_decode::MediaBackend::FFmpeg,
            pb_decode::TrackSet::complete(vec![a0, a1]),
            pb_decode::TrackSet::complete(vec![]),
        );
        // Row 0 is MF-located (the fallback-catalog shape), row 1 FFmpeg-located.
        catalog.set_locator(1, pb_decode::tracks::TrackLocator::MfStream(3));
        catalog.set_locator(2, pb_decode::tracks::TrackLocator::FfStream(2));
        seed_details(&mut core, 0, Some(catalog), Some(true));

        assert_eq!(core.audio_row_mf_stream(0), 3);
        assert_eq!(core.audio_row_mf_stream(1), -1, "no MF twin → refuse");
        assert_eq!(core.audio_row_mf_stream(9), -1, "out of range → refuse");

        assert_eq!(core.audio_row_for_mf_stream(3), 0);
        assert_eq!(core.audio_row_for_mf_stream(9), -1);
        assert_eq!(core.audio_row_for_ff_stream(2), 1);
        assert_eq!(core.audio_row_for_ff_stream(0), -1);
    }

    /// R4 (overhaul plan 1D): a held-seek run pauses audio ONCE and commits ONE
    /// audio seek + resume (in that order) at the settled final landing — never
    /// stopping/seeking/refilling the audio decoder per intermediate target.
    #[test]
    fn held_seek_run_coalesces_to_one_audio_commit() {
        use crate::video::{
            SeekGeneration, VideoProducerEvent, VideoProducerMsg, VideoSessionId, VideoSessionState,
        };
        use crate::video_session::{ActiveVideo, VideoSession, VideoSessionIo};
        use pb_decode::video::VideoColorInfo;

        let mut core = test_core();
        let sid = VideoSessionId(9);
        let (session, io) = VideoSession::new(sid, 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        let frame = |pts_ms: u64, generation: SeekGeneration| pb_decode::VideoFrame {
            session_id: sid,
            seek_generation: generation,
            pts: Duration::from_millis(pts_ms),
            width: 1,
            height: 1,
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 4],
            color: VideoColorInfo::srgb(),
        };
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(60)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(0, SeekGeneration::FIRST)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33, SeekGeneration::FIRST)))
            .unwrap();
        core.poll_video();
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Playing
        );

        // The generation of the last SeekTo the producer saw (drains the inbox).
        let last_seek_gen = |io: &VideoSessionIo| {
            let mut generation = None;
            while let Ok(msg) = io.msgs.try_recv() {
                if let VideoProducerMsg::SeekTo { generation: g, .. } = msg {
                    generation = Some(g);
                }
            }
            generation.expect("a SeekTo reached the producer")
        };

        core.effects.clear();
        // Model the seek key being HELD for the whole run — the real held-key path
        // sets `video_seek_last` each repeat (`apply_view_holds`), and the adaptive
        // audio commit keys off it: while the key is down, only the full settle window
        // applies, so intermediate targets never commit (below). A bare `video_seek`
        // wouldn't set it, so set it explicitly to model the hold.
        core.video_seek_last = Some(core.now);
        // Held repeat: two forward seeks 200 ms apart, each landing quickly.
        core.video_seek(false);
        let gen1 = last_seek_gen(&io);
        io.events
            .send(VideoProducerEvent::Frame(frame(2000, gen1)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(2033, gen1)))
            .unwrap();
        core.now += Duration::from_millis(200);
        core.poll_video(); // gen1 lands mid-run
        core.video_seek(false);
        let gen2 = last_seek_gen(&io);
        io.events
            .send(VideoProducerEvent::Frame(frame(4000, gen2)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(4033, gen2)))
            .unwrap();
        core.now += Duration::from_millis(100);
        core.poll_video(); // gen2 lands; run not settled yet (100 < 250 ms)

        let pauses = |core: &AppCore| {
            core.effects
                .iter()
                .filter(|e| matches!(e, contract::CoreEffect::PauseVideoAudio))
                .count()
        };
        let seeks = |core: &AppCore| {
            core.effects
                .iter()
                .filter(|e| matches!(e, contract::CoreEffect::SeekVideoAudio { .. }))
                .count()
        };
        let resumes = |core: &AppCore| {
            core.effects
                .iter()
                .filter(|e| matches!(e, contract::CoreEffect::ResumeVideoAudio))
                .count()
        };
        assert_eq!(pauses(&core), 1, "audio pauses once at run begin");
        assert_eq!(seeks(&core), 0, "no audio seek for intermediate targets");
        assert_eq!(resumes(&core), 0, "audio stays paused mid-run");

        // The run settles → exactly one commit: seek to the LANDED position,
        // then resume, in that order.
        core.now += VIDEO_SEEK_AUDIO_SETTLE;
        core.poll_video();
        assert_eq!(pauses(&core), 1);
        assert_eq!(seeks(&core), 1, "one audio seek per run");
        assert_eq!(resumes(&core), 1, "one resume per run");
        let seek_at = core.effects.iter().position(
            |e| matches!(e, contract::CoreEffect::SeekVideoAudio { position } if *position == Duration::from_millis(4000)),
        );
        let resume_at = core
            .effects
            .iter()
            .position(|e| matches!(e, contract::CoreEffect::ResumeVideoAudio));
        assert!(
            seek_at.expect("seek to the landed pts") < resume_at.expect("resume"),
            "audio seeks before it resumes"
        );

        // A later poll adds nothing — the commit is one-shot.
        core.now += Duration::from_millis(100);
        core.poll_video();
        assert_eq!((seeks(&core), resumes(&core)), (1, 1));
    }

    /// Task #4 follow-up: a DISCRETE tap — the seek key already released — commits its
    /// audio seek after the short [`VIDEO_SEEK_AUDIO_QUIET`], NOT the full settle
    /// window, so audio lands with the picture instead of ~172 ms behind it (measured).
    /// The held run above proves the slow path still coalesces; this proves a tap is
    /// fast, and the two differ only by whether the key is down.
    #[test]
    fn a_released_tap_commits_audio_after_the_short_quiet() {
        use crate::video::{
            SeekGeneration, VideoProducerEvent, VideoProducerMsg, VideoSessionId, VideoSessionState,
        };
        use crate::video_session::{ActiveVideo, VideoSession};
        use pb_decode::video::VideoColorInfo;

        let mut core = test_core();
        let sid = VideoSessionId(9);
        let (session, io) = VideoSession::new(sid, 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        let frame = |pts_ms: u64, generation: SeekGeneration| pb_decode::VideoFrame {
            session_id: sid,
            seek_generation: generation,
            pts: Duration::from_millis(pts_ms),
            width: 1,
            height: 1,
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 4],
            color: VideoColorInfo::srgb(),
        };
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(60)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(0, SeekGeneration::FIRST)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33, SeekGeneration::FIRST)))
            .unwrap();
        core.poll_video();
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Playing
        );

        core.effects.clear();
        // The key is UP (a single tap already released) — the release signal.
        core.video_seek_last = None;
        core.video_seek(false);
        let generation = {
            let mut g = None;
            while let Ok(msg) = io.msgs.try_recv() {
                if let VideoProducerMsg::SeekTo { generation, .. } = msg {
                    g = Some(generation);
                }
            }
            g.expect("a SeekTo reached the producer")
        };
        // Two frames satisfy preroll (PREROLL_FRAMES) so the seek lands, as the held
        // test does; the landing anchors at the first frame's pts (2000).
        io.events
            .send(VideoProducerEvent::Frame(frame(2000, generation)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(2033, generation)))
            .unwrap();

        // Just past the short quiet — and well below the full settle window.
        assert!(VIDEO_SEEK_AUDIO_QUIET < VIDEO_SEEK_AUDIO_SETTLE);
        core.now += VIDEO_SEEK_AUDIO_QUIET + Duration::from_millis(1);
        core.poll_video();
        let seeks = core
            .effects
            .iter()
            .filter(|e| matches!(e, contract::CoreEffect::SeekVideoAudio { position } if *position == Duration::from_millis(2000)))
            .count();
        assert_eq!(
            seeks, 1,
            "a released tap commits after the short quiet, not the full window"
        );
    }

    /// Task #94.2: leaving a video far enough into a long-enough clip remembers a
    /// (rewound) resume position keyed by item; a near-start leave remembers nothing.
    #[test]
    fn stop_video_remembers_a_mid_clip_position() {
        use crate::video::{VideoProducerEvent, VideoSessionId};
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        let sid = VideoSessionId(20);
        let (session, io) = VideoSession::new(sid, 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 3)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(60)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        core.poll_video();
        // Playhead at ~10 s (a seek sets desired_position without needing frames).
        if let Some(v) = core
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
        {
            v.session.seek_to(Duration::from_secs(10), core.now, None);
        }
        core.stop_video();
        assert_eq!(
            core.video_resume.get(&3).copied(),
            Some(Duration::from_secs(8)), // 10 s − RESUME_REWIND
            "leaving mid-clip remembers the rewound position"
        );

        // A near-start leave remembers nothing (item 3's entry stays as-is; a new
        // item 4 left at 2 s is not recorded).
        let (session2, io2) = VideoSession::new(VideoSessionId(21), 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session2, 4)));
        io2.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(21),
                duration: Some(Duration::from_secs(60)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        core.poll_video();
        if let Some(v) = core
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
        {
            v.session.seek_to(Duration::from_secs(2), core.now, None);
        }
        core.stop_video();
        assert_eq!(
            core.video_resume.get(&4),
            None,
            "near-start is not remembered"
        );
    }

    /// Task #94.2: a session started for an item with a remembered position seeks
    /// there once it can (leaving Opening), holds the poster until then, and pauses
    /// audio for the resume run.
    #[test]
    fn returning_to_a_video_resumes_at_the_remembered_position() {
        use crate::video::{VideoProducerEvent, VideoProducerMsg, VideoSessionId};
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        let sid = VideoSessionId(22);
        let (session, io) = VideoSession::new(sid, 4);
        let mut av = ActiveVideo::new(session, 5);
        av.resume_to = Some(Duration::from_secs(30)); // as start_video_session would set it
        core.video = Some(ActiveVideoBackend::Session(av));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(120)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        core.poll_video();

        // The resume seek fired to 30 s, and the one-shot target was consumed.
        let v = core
            .video
            .as_ref()
            .and_then(ActiveVideoBackend::as_session)
            .expect("session");
        assert_eq!(
            v.resume_to, None,
            "the resume target is consumed once applied"
        );
        assert_eq!(
            v.session.desired_position(core.now),
            Duration::from_secs(30),
            "the session sought to the remembered position"
        );
        // The producer was told to seek to ~30 s (video Cues path).
        let sought = std::iter::from_fn(|| io.msgs.try_recv().ok())
            .any(|m| matches!(m, VideoProducerMsg::SeekTo { target, .. } if target == Duration::from_secs(30)));
        assert!(sought, "a SeekTo(30s) reached the producer");
    }

    /// Task #94.2 (native path): the shell's periodic position report folds into
    /// the resume map — mid-clip remembered (rewound), watched-to-end forgotten,
    /// a stale session ignored.
    #[test]
    fn native_video_progress_records_and_forgets_resume() {
        use crate::video::VideoSessionId;
        use crate::video_native::NativeVideoProxy;

        let mut core = test_core();
        core.video = Some(ActiveVideoBackend::Native(NativeVideoProxy::new(
            7,
            VideoSessionId(30),
            false,
        )));
        // Mid-clip → remembered, rewound by RESUME_REWIND.
        core.native_video_progress(30, 40.0, 100.0);
        assert_eq!(
            core.video_resume.get(&7).copied(),
            Some(Duration::from_secs(38))
        );
        // Near the end → forgotten, so returning restarts.
        core.native_video_progress(30, 99.0, 100.0);
        assert_eq!(core.video_resume.get(&7), None);
        // Re-record mid-clip, then a wrong-session report must NOT touch it.
        core.native_video_progress(30, 50.0, 100.0);
        core.native_video_progress(999, 10.0, 100.0);
        assert_eq!(
            core.video_resume.get(&7).copied(),
            Some(Duration::from_secs(48))
        );
    }

    /// A paused seek commits the audio position on settle but never resumes —
    /// paused stays paused (plan 1D).
    #[test]
    fn paused_seek_commits_audio_position_without_resume() {
        use crate::video::{
            SeekGeneration, VideoProducerEvent, VideoProducerMsg, VideoSessionId, VideoSessionState,
        };
        use crate::video_session::{ActiveVideo, VideoSession};
        use pb_decode::video::VideoColorInfo;

        let mut core = test_core();
        let sid = VideoSessionId(10);
        let (session, io) = VideoSession::new(sid, 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        let frame = |pts_ms: u64, generation: SeekGeneration| pb_decode::VideoFrame {
            session_id: sid,
            seek_generation: generation,
            pts: Duration::from_millis(pts_ms),
            width: 1,
            height: 1,
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 4],
            color: VideoColorInfo::srgb(),
        };
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(60)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(0, SeekGeneration::FIRST)))
            .unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33, SeekGeneration::FIRST)))
            .unwrap();
        core.poll_video();

        // Pause, then seek: the landing presents once and stays paused.
        if let Some(v) = core
            .video
            .as_mut()
            .and_then(ActiveVideoBackend::as_session_mut)
        {
            v.session.pause(core.now);
        }
        core.effects.clear();
        core.video_seek(false);
        let generation = {
            let mut generation = None;
            while let Ok(msg) = io.msgs.try_recv() {
                if let VideoProducerMsg::SeekTo { generation: g, .. } = msg {
                    generation = Some(g);
                }
            }
            generation.expect("a SeekTo reached the producer")
        };
        io.events
            .send(VideoProducerEvent::Frame(frame(2000, generation)))
            .unwrap();
        core.now += VIDEO_SEEK_AUDIO_SETTLE;
        core.poll_video();
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Paused
        );
        assert!(
            core.effects.iter().any(|e| matches!(
                e,
                contract::CoreEffect::SeekVideoAudio { position } if *position == Duration::from_millis(2000)
            )),
            "the paused audio player follows the landed position"
        );
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ResumeVideoAudio)),
            "paused stays paused"
        );
    }

    /// Archive video playback: when the producer reports an audio track, the
    /// `StartVideoAudio` effect carries the SAME `Arc`-shared in-RAM container the
    /// producer reads (the `ActiveVideo::media` slot) — an archive entry has no
    /// path, and the one-copy contract is the point of the slot.
    #[test]
    fn archive_video_audio_starts_from_the_shared_bytes() {
        use crate::video::{VideoInput, VideoProducerEvent, VideoSessionId};
        use crate::video_session::{ActiveVideo, VideoSession};

        struct FakeArchive;
        impl pb_source::ItemSource for FakeArchive {
            fn len(&self) -> usize {
                1
            }
            fn name(&self, _i: usize) -> &str {
                "folder/clip.mp4"
            }
            fn bytes(&self, _i: usize) -> std::io::Result<Vec<u8>> {
                Ok(b"fake".to_vec())
            }
        }

        let mut core = test_core();
        core.source = Arc::new(FakeArchive);
        core.displayed_item = Some(0);

        let sid = VideoSessionId(1);
        let (session, io) = VideoSession::new(sid, 4);
        let av = ActiveVideo::new(session, 0);
        let data = std::sync::Arc::new(b"fake mp4 container".to_vec());
        av.media
            .set(VideoInput::Bytes {
                data: data.clone(),
                name: "folder/clip.mp4".into(),
            })
            .expect("fresh slot");
        core.video = Some(ActiveVideoBackend::Session(av));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(2)),
                width: 1,
                height: 1,
                has_audio: true,
                frame_bytes: 4,
            })
            .unwrap();
        core.effects.clear();
        core.poll_video();

        let started = core.effects.iter().find_map(|e| match e {
            contract::CoreEffect::StartVideoAudio {
                input, session_id, ..
            } => Some((input.clone(), *session_id)),
            _ => None,
        });
        let (input, got_sid) =
            started.expect("Opened(has_audio) must start the shell audio player");
        assert_eq!(got_sid, sid);
        match input {
            VideoInput::Bytes { data: d, name } => {
                assert!(
                    std::sync::Arc::ptr_eq(&d, &data),
                    "the audio player must read the SAME buffer (one resident copy)"
                );
                assert_eq!(name, "folder/clip.mp4");
            }
            VideoInput::Path(_) => {
                panic!("an archive entry must start audio from bytes, not a path")
            }
        }
    }

    /// Arrow-seek on the macOS **native** backend emits a relative, generation-bumped
    /// `SeekVideoBy` intent (±2 s, Shift ±10 s; the shell resolves it against AVPlayer).
    #[test]
    fn native_arrow_seek_emits_relative_seek_intent() {
        use crate::video::VideoSessionId;
        use crate::video_native::{ActiveVideoBackend, NativeVideoProxy};

        fn seek_of(core: &AppCore) -> Option<(u64, u64, i64)> {
            core.effects.iter().find_map(|e| match e {
                contract::CoreEffect::SeekVideoBy {
                    session_id,
                    generation,
                    delta_ms,
                } => Some((session_id.0, generation.0, *delta_ms)),
                _ => None,
            })
        }

        let mut core = test_core();
        core.displayed_item = Some(0);
        core.video = Some(ActiveVideoBackend::Native(NativeVideoProxy::new(
            0,
            VideoSessionId(7),
            false,
        )));

        // Forward ±2 s; generation bumps off FIRST(0) → 1.
        core.effects.clear();
        core.video_seek(false);
        assert_eq!(seek_of(&core), Some((7, 1, 2000)));

        // Backward is a negative delta; generation keeps climbing.
        core.effects.clear();
        core.video_seek(true);
        assert_eq!(seek_of(&core), Some((7, 2, -2000)));

        // Shift widens the step to ±10 s.
        core.mods.shift = true;
        core.effects.clear();
        core.video_seek(false);
        assert_eq!(seek_of(&core), Some((7, 3, 10_000)));
    }

    /// The macOS archive-video byte stash is pulled exactly once — a second pull (a stale
    /// or superseded session) gets nothing, never another session's container.
    #[test]
    fn pending_video_bytes_is_taken_once() {
        let mut core = test_core();
        assert!(
            core.take_pending_video_bytes().is_empty(),
            "none by default"
        );
        core.pending_video_bytes = Some(vec![1, 2, 3, 4]);
        assert_eq!(core.take_pending_video_bytes(), vec![1, 2, 3, 4]);
        assert!(core.take_pending_video_bytes().is_empty(), "consumed once");
    }

    /// A shell-generated archive-video poster becomes a synthetic full-decode `Outcome`
    /// queued for the ring; a wrong-sized frame is dropped, but the in-flight guard always
    /// clears so a later revisit can re-request.
    #[test]
    fn video_poster_ready_queues_a_synthetic_outcome() {
        let mut core = test_core();

        // Wrong pixel count (claims 4x4 but sends 10 bytes) → dropped, guard still cleared.
        core.poster_inflight.insert(3, 1);
        core.video_poster_ready(1, 3, 4, 4, vec![0u8; 10]);
        assert!(
            !core.poster_inflight.contains_key(&3),
            "in-flight cleared even on a bad frame"
        );
        assert!(
            core.pending_uploads.is_empty(),
            "bad pixel count is dropped"
        );

        // A STALE request id (the marker now belongs to a newer request, #119 diff
        // review): dropped whole — the replacement's marker survives.
        core.poster_inflight.insert(4, 9);
        core.video_poster_ready(2, 4, 2, 2, vec![255u8; 16]);
        assert!(
            core.poster_inflight.contains_key(&4),
            "a straggler with a stale id must not consume the replacement's marker"
        );
        assert!(core.pending_uploads.is_empty(), "and installs nothing");
        core.poster_inflight.remove(&4);

        // Correct 2x2 RGBA8 (16 bytes) → queued as a full (non-preview) outcome for item 5.
        core.poster_inflight.insert(5, 2);
        core.video_poster_ready(2, 5, 2, 2, vec![255u8; 16]);
        assert!(!core.poster_inflight.contains_key(&5));
        assert_eq!(core.pending_uploads.len(), 1);
        let o = &core.pending_uploads[0];
        assert_eq!(o.key.item, 5);
        assert_eq!(o.key.epoch, core.epoch);
        assert!(o
            .result
            .as_ref()
            .is_ok_and(|img| img.width == 2 && img.height == 2 && !img.is_preview));
    }

    /// The thin Swift round-trip races the Rust worker by construction, so it must never
    /// overwrite a richer catalog-bearing entry just by landing second.
    #[test]
    fn the_shell_archive_round_trip_never_clobbers_a_richer_catalog() {
        let mut core = test_core();
        let cat = pb_decode::MediaTrackCatalog::new(
            1,
            pb_decode::MediaBackend::FFmpeg,
            pb_decode::TrackSet::complete(vec![track("AAC", "eng")]),
            pb_decode::TrackSet::complete(vec![]),
        );
        seed_details(&mut core, 2, Some(cat), Some(true));

        core.archive_video_meta_ready(2, "HEVC".to_string(), 30_000, 5_000, true);

        let d = core.exif_cache.get(&2).expect("still cached");
        assert!(d.media.is_some(), "the catalog must survive");
        assert!(
            !d.fields.iter().any(|(k, _)| k == "Audio"),
            "the placeholder Audio row must not come back"
        );
        // ...but it still populates an entry that has no catalog.
        core.exif_cache.remove(&2);
        core.archive_video_meta_ready(2, "HEVC".to_string(), 30_000, 5_000, true);
        assert!(core
            .exif_cache
            .get(&2)
            .expect("cached")
            .fields
            .iter()
            .any(|(k, v)| k == "Video codec" && v == "HEVC"));
    }

    /// A shell-probed archive-video's facts become the inspector's rows (codec/fps/duration/
    /// audio) and re-signal the panel; unknown duration is omitted.
    #[test]
    fn archive_video_meta_ready_builds_inspector_rows() {
        let mut core = test_core();
        core.archive_video_meta_ready(2, "HEVC".to_string(), 30_000, 5_000, true);
        let rows = &core
            .exif_cache
            .get(&2)
            .expect("rows cached for item 2")
            .fields;
        assert!(rows.iter().any(|(k, v)| k == "Video codec" && v == "HEVC"));
        assert!(rows
            .iter()
            .any(|(k, v)| k == "Frame rate" && v == "30.00 fps"));
        assert!(rows.iter().any(|(k, _)| k == "Duration"));
        assert!(rows.iter().any(|(k, v)| k == "Audio" && v == "Yes"));
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)));

        // Unknown duration (-1) is omitted; no audio reads "No".
        core.archive_video_meta_ready(3, "H.264".to_string(), 0, -1, false);
        let rows = &core.exif_cache.get(&3).unwrap().fields;
        assert!(
            !rows.iter().any(|(k, _)| k == "Duration"),
            "unknown duration omitted"
        );
        assert!(
            !rows.iter().any(|(k, _)| k == "Frame rate"),
            "unknown fps omitted"
        );
        assert!(rows.iter().any(|(k, v)| k == "Audio" && v == "No"));
    }

    /// Poster byte stashes are keyed by request id and consumed once.
    #[test]
    fn pending_poster_bytes_keyed_and_taken_once() {
        let mut core = test_core();
        core.pending_poster_bytes.insert(7, vec![9, 8, 7]);
        assert!(
            core.take_pending_poster_bytes(99).is_empty(),
            "wrong id → nothing"
        );
        assert_eq!(core.take_pending_poster_bytes(7), vec![9, 8, 7]);
        assert!(
            core.take_pending_poster_bytes(7).is_empty(),
            "consumed once"
        );
    }

    /// Frame-step on the native backend emits a `StepVideo` intent for the displayed item,
    /// and no-ops for a stale/mismatched item.
    #[test]
    fn native_frame_step_emits_step_intent() {
        use crate::video::VideoSessionId;
        use crate::video_native::{ActiveVideoBackend, NativeVideoProxy};

        let mut core = test_core();
        core.displayed_item = Some(0);
        core.video = Some(ActiveVideoBackend::Native(NativeVideoProxy::new(
            0,
            VideoSessionId(9),
            false,
        )));

        core.effects.clear();
        assert!(core.video_frame_step(1));
        assert!(core.effects.iter().any(|e| matches!(e,
            contract::CoreEffect::StepVideo { session_id, forward: true } if session_id.0 == 9)));

        // Displayed item moved on: the step is dropped (a stale key press).
        core.displayed_item = Some(5);
        core.effects.clear();
        assert!(!core.video_frame_step(-1));
        assert!(!core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::StepVideo { .. })));
    }

    /// Owner-reported (79.10 smoke): a resize drag stalls the presenter (the OS
    /// modal loop) while audio plays on — playback must freeze *together* and
    /// resume together at settle, exactly where it froze. (The clock-catch-up
    /// alternative raced or seek-churned — tried, regressed, reverted.)
    #[test]
    fn resize_pauses_playback_and_settle_resumes_it() {
        use crate::video::{VideoProducerEvent, VideoSessionId, VideoSessionState};
        use crate::video_native::ActiveVideoBackend;
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.playlist = Playlist::new(1, 0);
        core.displayed_item = Some(0);
        let (session, io) = VideoSession::new(VideoSessionId(1), 16);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(1),
                duration: Some(Duration::from_secs(10)),
                width: 2,
                height: 2,
                has_audio: false,
                frame_bytes: 16,
            })
            .unwrap();
        let frame = |pts_ms: u64| pb_decode::VideoFrame {
            session_id: VideoSessionId(1),
            seek_generation: crate::video::SeekGeneration::FIRST,
            pts: Duration::from_millis(pts_ms),
            width: 2,
            height: 2,
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 16],
            color: pb_decode::video::VideoColorInfo::srgb(),
        };
        io.events.send(VideoProducerEvent::Frame(frame(0))).unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33)))
            .unwrap();
        core.poll_video();
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Playing
        );

        // A resize lands mid-playback: freeze together.
        core.effects.clear();
        core.resize(320, 200, 1.0);
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Paused,
            "resize pauses the session"
        );
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PauseVideoAudio)),
            "…and the audio with it"
        );
        assert!(core.video_paused_by_resize);

        // The settle deadline passes: resume together, exactly where frozen.
        core.effects.clear();
        core.resize_settle_at = Some(core.now - Duration::from_millis(1));
        core.handle(contract::CoreEvent::Tick(Instant::now()));
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Playing,
            "settle resumes the session"
        );
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ResumeVideoAudio)),
            "…and the audio with it"
        );
        assert!(!core.video_paused_by_resize, "one-shot");
        drop(io);
    }

    /// Owner-reported (79.10 smoke): toggling fullscreen while a video played went
    /// jerky — the resize-settle re-decode ran a synchronous poster decode over the
    /// live frame and refilled the whole ring (neighbor poster storms) mid-playback.
    /// A live video must defer the refresh; stopping the video re-issues it.
    #[test]
    fn geometry_change_during_video_defers_the_ring_refill() {
        use crate::video::{VideoProducerEvent, VideoSessionId};
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.playlist = Playlist::new(1, 0);
        core.displayed_item = Some(0);
        let (session, io) = VideoSession::new(VideoSessionId(1), 1024);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(1),
                duration: Some(Duration::from_secs(10)),
                width: 64,
                height: 64,
                has_audio: false,
                frame_bytes: 64 * 64 * 4,
            })
            .unwrap();
        core.poll_video();

        // The settled geometry change defers instead of re-decoding.
        core.refresh_after_geometry_change();
        assert!(core.video_geometry_stale, "refresh deferred while playing");

        // Stopping the video re-issues the prefetch (targets recomputed).
        core.targets.clear();
        core.stop_video();
        assert!(!core.video_geometry_stale, "flag consumed");
        assert!(
            !core.targets.is_empty(),
            "ring refill re-issued once playback ended"
        );
    }
    /// The owner's missing-chrome report (2026-07-16): pressing `P` before the
    /// poster decode lands (an SMB movie's poster takes seconds; `P` always wins)
    /// left `current` unset for the WHOLE playback — the Session route streams
    /// frames around `present_item`, and the first frame's `mark_resolved` makes
    /// the late poster skip its present. With no `current` there is no info line,
    /// no `i`, no hover reveal, no playback controls. The fix: a presented video
    /// frame adopts the poster's metadata from `meta_cache` the moment it lands.
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn a_video_frame_adopts_late_poster_meta_so_the_controls_can_show() {
        let mut core = test_core();
        core.native_info = true;
        core.info_line = false;
        core.viewport.width = 800;
        core.viewport.height = 1000;
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            "/nope/movie.mkv",
        )]));
        core.displayed_item = Some(0);
        core.toggle_play_pause(); // P wins the race: no poster has presented yet
        assert!(core
            .video
            .as_ref()
            .is_some_and(|v| v.as_session().is_some()));
        assert!(core.current.is_none(), "the poster hasn't landed");

        let frame = crate::video::VideoFrame {
            session_id: crate::video::VideoSessionId(core.video_seq),
            seek_generation: crate::video::SeekGeneration::FIRST,
            pts: Duration::ZERO,
            width: 2,
            height: 2,
            pixels: vec![0; 16],
            format: pb_decode::PixelFormat::Rgba8,
            color: crate::video::VideoColorInfo::srgb(),
        };
        // Frames present before the poster lands: nothing to adopt, no controls yet.
        core.present_video_frame(&frame);
        assert!(core.current.is_none());
        core.video_hover_reveal(900.0);
        assert!(
            !core.info_line_visible(),
            "no metadata yet — nothing to show"
        );

        // The poster decode completes off-thread; drain_results caches its meta.
        core.meta_cache.insert(
            0,
            crate::meta::PhotoMeta {
                rel: "movie.mkv".into(),
                w: 1920,
                h: 1080,
                size: None,
                codec: "MKV",
                animated: None,
                recovered: None,
            },
        );
        // The very next presented frame adopts it — chrome comes alive mid-play.
        core.present_video_frame(&frame);
        assert!(core.current.is_some(), "the late poster's meta is adopted");
        core.video_hover_reveal(900.0);
        assert!(
            core.info_line_visible(),
            "hover now reveals the playback controls"
        );
    }

    /// Chrome parity on the new default route (owner report 2026-07-16): an MKV
    /// playing on the Session route must still (a) report `video_session_active`
    /// — the SwiftUI shell's gate for the playback row/scrubber — and (b) reveal
    /// the controls line on a bottom-zone hover, exactly like the old
    /// sample-buffer route did.
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn session_mkv_reports_active_and_reveals_the_controls() {
        let mut core = test_core();
        core.native_info = true;
        core.info_line = false;
        core.viewport.width = 800;
        core.viewport.height = 1000;
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            "/nope/clip.mkv",
        )]));
        core.displayed_item = Some(0);
        core.current = Some(crate::meta::PhotoMeta {
            rel: "clip.mkv".into(),
            w: 64,
            h: 64,
            size: None,
            codec: "MKV",
            animated: None,
            recovered: None,
        });
        core.toggle_play_pause(); // the real routing → Session backend
        assert!(
            core.video
                .as_ref()
                .is_some_and(|v| v.as_session().is_some()),
            "MKV plays on the Session route"
        );
        assert!(
            core.video_session_active(),
            "the SwiftUI chrome gate must see the session"
        );
        core.video_hover_reveal(900.0);
        assert!(
            core.info_line_visible(),
            "bottom-zone hover reveals the controls line"
        );
    }

    /// macOS §8a level-2 fallback (task #84): a *recoverable* native failure on
    /// a nominally-native container retries through the FFmpeg session with no
    /// toast before the attempt; an unrecoverable one surfaces immediately.
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn recoverable_native_failure_falls_back_to_the_ffmpeg_session() {
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            "/nope/clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.native_toast = true;
        core.toggle_play_pause();
        assert!(
            core.video.as_ref().unwrap().as_native().is_some(),
            "MP4 tries AVPlayer first"
        );
        let sid = core.native_video_session_id();
        assert!(sid > 0);
        // The shell classifies a demux/codec failure as recoverable.
        core.native_video_failed(sid, "no codec for this video".into(), true);
        assert!(
            core.video
                .as_ref()
                .is_some_and(|v| v.as_session().is_some()),
            "fallback started the FFmpeg session"
        );
        assert!(
            core.toast_native.is_none(),
            "no error surfaces before the fallback attempt"
        );
        // The flag was consumed — it never loops.
        assert_eq!(core.video_ffmpeg_fallback, None);
    }

    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    #[test]
    fn unrecoverable_native_failure_surfaces_without_fallback() {
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            "/nope/clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.native_toast = true;
        core.toggle_play_pause();
        let sid = core.native_video_session_id();
        core.native_video_failed(sid, "The file couldn't be opened".into(), false);
        assert!(core.video.is_none(), "no fallback for missing-file/DRM");
        assert!(core.toast_native.is_some(), "the error surfaces at once");
    }

    /// Owner-reported: the info line showed no playback row during video playback.
    /// This drives the real chain — session → update_video_progress →
    /// show_info_line — and asserts each link.
    #[test]
    fn video_playback_grows_the_info_line_row() {
        use crate::video::{VideoProducerEvent, VideoSessionId};
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        core.hud = pb_hud::hud::Hud::load();
        if core.hud.is_none() {
            eprintln!("no system UI font — skipping");
            return;
        }
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.info_line = true;
        core.current = Some(crate::meta::PhotoMeta {
            rel: "clip.mp4".into(),
            w: 64,
            h: 64,
            size: None,
            codec: "MP4",
            animated: None,
            recovered: None,
        });
        assert!(core.info_line_visible(), "precondition: the line is on");

        let (session, io) = VideoSession::new(VideoSessionId(1), 1024);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(1),
                duration: Some(Duration::from_secs(10)),
                width: 64,
                height: 64,
                has_audio: false,
                frame_bytes: 64 * 64 * 4,
            })
            .unwrap();
        core.poll_video();
        assert!(
            core.video_progress_row().is_some(),
            "a live session on the displayed item must yield a progress row"
        );
        core.update_video_progress();
        assert!(
            core.video_pill_text.is_some(),
            "the row text must be computed"
        );
        assert!(
            core.info_line_shown,
            "update_video_progress must re-raster the info line"
        );
    }

    /// `,`/`.` on a playing video (task #79 follow-up): stepping pauses the
    /// session, serves the next queued frame, keeps the paused audio player in
    /// step, and — with the `i` toggle off on a native-info shell — flashes the
    /// info line as the position OSD instead of toasting. A backward step then
    /// launches a paused seek.
    #[test]
    fn frame_step_on_video_pauses_steps_and_flashes_the_info_line() {
        use crate::video::{VideoProducerEvent, VideoSessionId, VideoSessionState};
        use crate::video_session::{ActiveVideo, VideoSession};
        use pb_decode::video::VideoColorInfo;

        let mut core = test_core();
        core.native_info = true;
        core.info_line = false; // the toggle is OFF — feedback must flash the line
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.current = Some(crate::meta::PhotoMeta {
            rel: "clip.mp4".into(),
            w: 64,
            h: 64,
            size: None,
            codec: "MP4",
            animated: None,
            recovered: None,
        });

        let sid = VideoSessionId(1);
        let (session, io) = VideoSession::new(sid, 4);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: sid,
                duration: Some(Duration::from_secs(10)),
                width: 1,
                height: 1,
                has_audio: false,
                frame_bytes: 4,
            })
            .unwrap();
        let frame = |pts_ms: u64| pb_decode::VideoFrame {
            session_id: sid,
            seek_generation: crate::video::SeekGeneration::FIRST,
            pts: Duration::from_millis(pts_ms),
            width: 1,
            height: 1,
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![0; 4],
            color: VideoColorInfo::srgb(),
        };
        io.events.send(VideoProducerEvent::Frame(frame(0))).unwrap();
        io.events
            .send(VideoProducerEvent::Frame(frame(33)))
            .unwrap();
        core.poll_video(); // → Playing, presents pts 0
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Playing
        );

        core.effects.clear();
        core.frame_step(1);
        let v = core.video.as_ref().unwrap().as_session().unwrap();
        assert_eq!(
            v.session.state(),
            VideoSessionState::Paused,
            "stepping pauses playback, like animations"
        );
        assert_eq!(v.session.current_pts, Some(Duration::from_millis(33)));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PauseVideoAudio)),
            "the shell audio player pauses with the session"
        );
        assert!(
            core.effects.iter().any(|e| matches!(
                e,
                contract::CoreEffect::SeekVideoAudio { position } if *position == Duration::from_millis(33)
            )),
            "the paused audio player follows the stepped position"
        );
        assert!(
            core.video_osd_until.is_some() && core.info_line_visible(),
            "with `i` off, the position feedback flashes the info line"
        );
        assert!(
            core.toast_native.is_none() && core.toast.is_none(),
            "no `m:ss / m:ss` toast when the line is the readout"
        );

        // A backward step launches a paused one-frame seek.
        core.frame_step(-1);
        assert_eq!(
            core.video.as_ref().unwrap().state(),
            VideoSessionState::Seeking
        );

        // The flash lapses at its deadline (tick clears it + notifies the shell).
        core.video_osd_until = Some(core.now - Duration::from_millis(1));
        core.handle(contract::CoreEvent::Tick(Instant::now()));
        assert!(core.video_osd_until.is_none(), "the OSD flash expires");
        assert!(!core.info_line_visible(), "the flashed line drops");
    }

    /// Hovering the bottom controls zone reveals the playback controls while a
    /// video is active (owner request — the video-player convention); the top of
    /// the window doesn't, and the persistent `i` line needs no flash.
    #[test]
    fn hovering_the_controls_zone_reveals_the_playback_line() {
        use crate::video::{VideoProducerEvent, VideoSessionId};
        use crate::video_native::ActiveVideoBackend;
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        core.native_info = true;
        core.info_line = false;
        core.viewport.width = 800;
        core.viewport.height = 1000;
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.current = Some(crate::meta::PhotoMeta {
            rel: "clip.mp4".into(),
            w: 64,
            h: 64,
            size: None,
            codec: "MP4",
            animated: None,
            recovered: None,
        });
        let (session, io) = VideoSession::new(VideoSessionId(1), 16);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(1),
                duration: Some(Duration::from_secs(10)),
                width: 2,
                height: 2,
                has_audio: false,
                frame_bytes: 16,
            })
            .unwrap();
        core.poll_video();

        // Above the zone: nothing.
        core.video_hover_reveal(100.0);
        assert!(core.video_osd_until.is_none(), "top hover reveals nothing");
        // Inside the bottom quarter: the line flashes on.
        core.video_hover_reveal(900.0);
        assert!(core.video_osd_until.is_some() && core.info_line_visible());

        // With the persistent line on, hover never arms the flash.
        core.video_osd_until = None;
        core.info_line = true;
        core.video_hover_reveal(900.0);
        assert!(
            core.video_osd_until.is_none(),
            "persistent line needs no flash"
        );
        drop(io);
    }

    /// `flash_video_controls` (the scrubber-release re-arm) reveals the line for an active
    /// video regardless of pointer position, but never for a still or when `i` is already on.
    #[test]
    fn flash_video_controls_re_arms_the_reveal() {
        use crate::video::{VideoProducerEvent, VideoSessionId};
        use crate::video_native::ActiveVideoBackend;
        use crate::video_session::{ActiveVideo, VideoSession};

        let mut core = test_core();
        core.native_info = true;
        core.info_line = false;
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.current = Some(crate::meta::PhotoMeta {
            rel: "clip.mp4".into(),
            w: 64,
            h: 64,
            size: None,
            codec: "MP4",
            animated: None,
            recovered: None,
        });
        let (session, io) = VideoSession::new(VideoSessionId(1), 16);
        core.video = Some(ActiveVideoBackend::Session(ActiveVideo::new(session, 0)));
        io.events
            .send(VideoProducerEvent::Opened {
                session_id: VideoSessionId(1),
                duration: Some(Duration::from_secs(10)),
                width: 2,
                height: 2,
                has_audio: false,
                frame_bytes: 16,
            })
            .unwrap();
        core.poll_video();

        // Active video: the flash arms with no geometry (a mid-drag re-arm).
        core.flash_video_controls();
        assert!(core.video_osd_until.is_some() && core.info_line_visible());

        // Persistent `i` line up: nothing to re-arm.
        core.video_osd_until = None;
        core.info_line = true;
        core.flash_video_controls();
        assert!(
            core.video_osd_until.is_none(),
            "persistent line needs no flash"
        );

        // No active video: never arms (can't flash a still's line).
        core.info_line = false;
        core.video = None;
        core.flash_video_controls();
        assert!(core.video_osd_until.is_none(), "no video → no flash");
        drop(io);
    }
}
