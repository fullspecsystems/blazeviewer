//! **Animation and Live Photos** — the `AppCore` half of [`crate::animation`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! The still-image motion path: animated GIF/WebP/AVIF playback, and the Live Photo video
//! that rides alongside a HEIC. Distinct from [`super::video`], which is the real video
//! session with its own reader thread and audio clock — these frames arrive through the
//! decode pool like any other still.
//!
//! `toggle_play_pause` lives here because animation is what it checks first; it falls
//! through to the video session when there is no `playback`. It is the shared `P` handler,
//! not an animation-only one.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Arm the play hint when settling on an item `P` acts on — suppressed while blazing
    /// (the nag the owner flagged) and once the user has engaged (P / step, tracked via
    /// `anim_hint_shown_for`). An eager prep decoding in the background does *not* suppress
    /// it — that's invisible work, and the hint is what invites the user to press P in the
    /// first place.
    ///
    pub fn maybe_show_anim_hint(&mut self, blazing: bool) {
        if blazing || self.playback.is_some() {
            return;
        }
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.anim_hint_shown_for == Some(item) {
            return;
        }
        // Videos show the hint too (task #79: poster + play badge is the UX shape) —
        // deliberately NOT via has_motion, which still gates the animation decode
        // machinery videos must never enter (their bytes never enter RAM). An archive
        // door gets no pill: its affordance is the door card (task #105).
        if self.has_motion(item) || self.item_is_video(item) {
            self.anim_hint_shown_for = Some(item);
            // Both shells present the play hint natively (the winit egui overlay / the macOS
            // SwiftUI pill): flash-signal it (bump the seq); the shell renders + fades the pill
            // and reads `play_hint_kind` for the icon. No HUD raster / colliders / cursor.
            self.play_hint_seq = self.play_hint_seq.wrapping_add(1);
            self.draw(); // wake the shell so it reads the new seq
        }
    }

    /// `P`: play/pause the current animation. Uses the eagerly-prepped sequence for
    /// instant playback when it's ready; otherwise (upgrading an in-flight eager prep,
    /// or kicking a fresh decode) it starts playing the moment frames land. On a still,
    /// `P` does nothing.
    pub fn toggle_play_pause(&mut self) {
        if self.playback.is_some() {
            // Was it parked at the end of a finite loop? Then toggling *restarts* from
            // frame 0 (so the audio must restart too, not resume mid-track).
            let was_finished = self.playback.as_ref().unwrap().is_finished();
            let playing = self.playback.as_mut().unwrap().toggle_play();
            if playing {
                // (Re)started — present the current frame (frame 0 when replaying a
                // finished loop, so the stale last frame doesn't linger) + anchor timing.
                self.present_anim_frame();
                if was_finished {
                    if let Some(item) = self.displayed_item {
                        self.start_live_audio(item); // replay from the top
                    }
                } else {
                    self.effects.push(contract::CoreEffect::ResumeLiveAudio);
                }
            } else {
                self.draw(); // paused — just redraw the held frame
                self.effects.push(contract::CoreEffect::PauseLiveAudio);
            }
            return;
        }
        let Some(item) = self.displayed_item else {
            return;
        };
        // A video item (task #79 phase 4): its own streaming session — never the
        // animation decode machinery (that would read the file into RAM).
        if self.item_is_video(item) {
            self.video_play_pause(item);
            return;
        }
        // An archive door (task #104): `P` **enters** it. Consistent rather than
        // cute — `P` already acts on whatever the current item contains (play a
        // clip, play an animation, play a Live Photo), and an archive's contents
        // are simply reached by going in. This is the *only* path that ever reads
        // an archive: browsing past a door costs a tile (see `engine`'s dispatch).
        //
        // Routed through `open_plan`, not a hand-pushed effect, so entering is the
        // same operation as opening the archive from the picker: it ends an
        // Open-Parent climb, and the RAM pre-flight, progress dialog and password
        // prompt all come along unchanged. `Alt+Up` climbs back out to the folder
        // of doors (`open_parent_cmd` anchors on the source's container).
        if self.item_archive_kind(item).is_some() {
            if let Some(path) = self.source.path(item).map(Path::to_path_buf) {
                self.open_plan(
                    pb_core::open::Source::Archive(path),
                    pb_core::open::Cursor::First,
                );
            }
            return;
        }
        // Eagerly prepared on dwell → play instantly (no decode wait).
        if self.prepared.as_ref().is_some_and(|p| p.item == item) {
            let anim = self.prepared.take().unwrap().anim;
            self.anim_hint_shown_for = Some(item); // engaged
            self.install_animation(anim, true, 0);
            self.start_live_audio(item);
            return;
        }
        // An eager stream (task #69) is decoding → upgrade it to play and start playing
        // whatever's decoded so far (the rest keeps streaming in).
        if self.anim_stream.is_some() {
            if let Some(s) = self.anim_stream.as_mut() {
                s.want = AnimWant::Play;
            }
            self.anim_hint_shown_for = Some(item);
            self.install_stream_playback(); // no-op until the first frame lands, then installs
            return;
        }
        // An eager prep is already decoding → upgrade it to play on arrival.
        if let Some(d) = self.anim_decode.as_mut() {
            d.want = AnimWant::Play;
            self.anim_hint_shown_for = Some(item);
            return;
        }
        if self.has_motion(item) {
            self.start_animation_decode(item, AnimWant::Play);
        }
    }

    /// Step the current animation one frame (`delta`: `+1` next, `-1` previous),
    /// pausing playback. Uses the eager prep when ready; otherwise upgrades an in-flight
    /// prep (or kicks one) so the held-key scrub steps once frames land. No-op on a still.
    pub fn frame_step(&mut self, delta: i32) {
        // A live video session steps through the session, not `playback` — and a
        // video can't have Live Photo audio, so the silencing below stays animation's.
        if self.video_frame_step(delta) {
            return;
        }
        // Scrubbing is not continuous playback — silence any Live Photo audio.
        self.effects.push(contract::CoreEffect::StopLiveAudio);
        if self.playback.is_some() {
            self.playback.as_mut().unwrap().step(delta);
            self.present_anim_frame();
            return;
        }
        let Some(item) = self.displayed_item else {
            return;
        };
        if self.prepared.as_ref().is_some_and(|p| p.item == item) {
            let anim = self.prepared.take().unwrap().anim;
            self.anim_hint_shown_for = Some(item);
            self.install_animation(anim, false, delta); // paused, stepped
            return;
        }
        if let Some(s) = self.anim_stream.as_mut() {
            s.want = AnimWant::Step(delta);
            self.anim_hint_shown_for = Some(item);
            return;
        }
        if let Some(d) = self.anim_decode.as_mut() {
            d.want = AnimWant::Step(delta);
            self.anim_hint_shown_for = Some(item);
            return;
        }
        if self.has_motion(item) {
            self.start_animation_decode(item, AnimWant::Step(delta));
        }
    }

    /// Keyboard frame-step press: track the key for hold-to-scrub, then step once now.
    pub fn frame_step_press(&mut self, key: PbKey, action: Action) {
        self.held.insert(key, action);
        let now = self.now;
        self.framestep_started = Some(now);
        self.framestep_last = Some(now);
        self.frame_step(frame_step_dir(action));
    }

    /// Pick up a finished off-thread animation decode (called each `about_to_wait`).
    /// Discards a stale result (superseded request, geometry change, or the user
    /// navigated away) and otherwise installs the [`Playback`] and shows frame 0.
    pub fn poll_anim_decode(&mut self) {
        use std::sync::mpsc::TryRecvError;
        // Receive (and copy out what we need) in a scope so the `anim_decode` borrow
        // ends before we mutate it / install the playback.
        let outcome = {
            let Some(d) = self.anim_decode.as_ref() else {
                return;
            };
            match d.rx.try_recv() {
                Ok(result) => Some((d.gen, d.epoch, d.item, d.want, result)),
                Err(TryRecvError::Empty) => return, // still decoding
                Err(TryRecvError::Disconnected) => None, // worker died
            }
        };
        self.anim_decode = None;
        let Some((gen, epoch, item, want, result)) = outcome else {
            return;
        };
        // Stale: a newer request superseded it, the fit changed, or we moved on.
        if gen != self.anim_gen || epoch != self.epoch || self.displayed_item != Some(item) {
            return;
        }
        match result {
            Ok(anim) => match want {
                // Eager prep: hold it ready; the still keeps showing (frame 0 == still),
                // so there's no visible change — `P` will play it instantly. If the
                // detailed panel is open, refresh it so the frame count/rate/loop appear.
                AnimWant::Eager => {
                    self.prepared = Some(Prepared { item, anim });
                    if self.overlay_shown && self.slot_content() == Some(SlotContent::Details) {
                        self.show_overlay();
                    }
                }
                AnimWant::Play => {
                    self.install_animation(anim, true, 0);
                    self.start_live_audio(item); // in sync with the first frame
                }
                AnimWant::Step(delta) => self.install_animation(anim, false, delta),
            },
            Err(e) => {
                // An eager prep that fails stays silent (the user never asked); only a
                // user-initiated P/step surfaces the error.
                eprintln!("animation decode failed for item {item}: {e}");
                if want != AnimWant::Eager {
                    self.show_toast("Can't play this animation");
                }
            }
        }
    }

    /// Drain a streaming Live Photo motion decode (task #69): install a playing Playback on
    /// the first frame, extend it as frames arrive, and finalize on `Done` — so the clip
    /// starts within a frame or two instead of after the whole `.mov` decodes. Called each
    /// tick alongside [`poll_anim_decode`](Self::poll_anim_decode). A no-op where no
    /// streaming producer exists (`anim_stream` is only set on the Linux FFmpeg and macOS
    /// AVAssetReader paths).
    pub fn poll_anim_stream(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some((gen, epoch, item)) = self.anim_stream.as_ref().map(|s| (s.gen, s.epoch, s.item))
        else {
            return;
        };
        // Stale (superseded / geometry changed / navigated away): cancel + drop.
        if gen != self.anim_gen || epoch != self.epoch || self.displayed_item != Some(item) {
            self.cancel_anim_stream();
            return;
        }
        // Drain everything available now without holding the receiver borrow.
        let mut msgs = Vec::new();
        let mut disconnected = false;
        {
            let s = self.anim_stream.as_ref().unwrap();
            loop {
                match s.rx.try_recv() {
                    Ok(m) => msgs.push(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for msg in msgs {
            self.apply_stream_msg(msg);
            if self.anim_stream.is_none() {
                return; // a terminal Done/Failed cleared it
            }
        }
        if disconnected {
            // The worker vanished without a terminal message (a panic, or a producer bug).
            // Treat it as a stream failure rather than silently dropping: if a Playback is
            // already installed and incomplete, `stream_failed` marks it complete — else it
            // would park on the decoded frontier forever while the audio played on.
            self.stream_failed("Live Photo stream worker vanished".into());
        }
    }

    fn apply_stream_msg(&mut self, msg: StreamMsg) {
        match msg {
            StreamMsg::Header {
                width,
                height,
                color,
                codec,
            } => {
                if let Some(s) = self.anim_stream.as_mut() {
                    s.header = Some(StreamHeader {
                        width,
                        height,
                        color,
                        codec,
                    });
                }
            }
            StreamMsg::Frame(frame) => self.stream_frame(frame),
            StreamMsg::Done {
                loop_count,
                truncated,
            } => self.stream_done(loop_count, truncated),
            StreamMsg::Failed(e) => self.stream_failed(e),
        }
    }

    /// A streaming frame arrived: extend the live Playback if one's installed, else buffer it
    /// (and, in `Play` mode, install a playing streaming Playback as soon as we can).
    fn stream_frame(&mut self, frame: pb_decode::AnimFrame) {
        let Some((installed, want)) = self.anim_stream.as_ref().map(|s| (s.installed, s.want))
        else {
            return;
        };
        if installed {
            if let Some(pb) = self.playback.as_mut() {
                pb.push_frame(frame);
            }
            return;
        }
        if let Some(s) = self.anim_stream.as_mut() {
            s.pending.push(frame);
        }
        // Eager/Step accumulate until `Done`; Play starts the moment a frame + header exist.
        if matches!(want, AnimWant::Play) {
            self.install_stream_playback();
        }
    }

    /// Install a **playing** streaming [`Playback`] from the stream's header + all frames
    /// buffered so far, and start its audio. Returns whether it installed (needs the header
    /// and at least one buffered frame). Used both for the first `Play` frame and the
    /// eager→`Play` upgrade (play whatever's decoded so far, then keep extending it).
    fn install_stream_playback(&mut self) -> bool {
        let Some(s) = self.anim_stream.as_mut() else {
            return false;
        };
        if s.installed {
            return true;
        }
        if s.header.is_none() || s.pending.is_empty() {
            return false; // header not here yet, or nothing decoded — wait for the first frame
        }
        let header = s.header.as_ref().unwrap();
        let (width, height, codec, color) =
            (header.width, header.height, header.codec, header.color);
        let item = s.item;
        let frames = std::mem::take(&mut s.pending);
        s.installed = true;
        let anim = pb_decode::Animation {
            kind: pb_decode::AnimationKind::LivePhoto,
            width,
            height,
            frames,
            loop_count: 0, // provisional; the real count lands with `Done` → `mark_complete`
            codec,
            color,
            truncated: false,
        };
        self.playback = Some(Playback::new_streaming(anim, true));
        self.present_anim_frame();
        self.start_live_audio(item);
        true
    }

    /// A streaming decode finished. If it's already playing, finalize the live Playback's loop
    /// count (so a finite Live Photo ends instead of looping); otherwise build the accumulated
    /// frames into a complete [`Animation`] and route it by `want` (eager → stash, step → step).
    fn stream_done(&mut self, loop_count: u32, truncated: bool) {
        let Some(installed) = self.anim_stream.as_ref().map(|s| s.installed) else {
            return;
        };
        if installed {
            if let Some(pb) = self.playback.as_mut() {
                pb.mark_complete(loop_count);
            }
            self.anim_stream = None;
            if truncated {
                self.show_toast("Animation truncated");
            }
            return;
        }
        let Some(stream) = self.anim_stream.take() else {
            return;
        };
        let (item, want) = (stream.item, stream.want);
        let Some(anim) = stream.into_animation(loop_count, truncated) else {
            return; // no header/frames — nothing to show
        };
        match want {
            AnimWant::Eager => {
                self.prepared = Some(Prepared { item, anim });
                if self.overlay_shown && self.slot_content() == Some(SlotContent::Details) {
                    self.show_overlay();
                }
            }
            // A Play stream installs on its first frame, so reaching here means it completed
            // before any frame was consumed as "installed" — play the whole thing now.
            AnimWant::Play => {
                self.install_animation(anim, true, 0);
                self.start_live_audio(item);
            }
            AnimWant::Step(delta) => self.install_animation(anim, false, delta),
        }
    }

    /// A streaming decode failed. Mid-playback, treat it as a truncated finish (don't yank the
    /// video away); before any frame, surface it like a batch decode failure (silent for an
    /// eager prep the user never asked for).
    fn stream_failed(&mut self, err: String) {
        let Some((installed, want)) = self.anim_stream.as_ref().map(|s| (s.installed, s.want))
        else {
            return;
        };
        if installed {
            if let Some(pb) = self.playback.as_mut() {
                pb.mark_complete(1);
            }
        } else {
            eprintln!("live photo stream failed: {err}");
            if want != AnimWant::Eager {
                self.show_toast("Can't play this animation");
            }
        }
        self.anim_stream = None;
    }

    /// Stop and drop any playback / in-flight decode / eager prep, reverting to the
    /// still. Called when navigating away or changing source (the frames are RAM-only —
    /// privacy #2).
    pub fn stop_playback(&mut self) {
        self.playback = None;
        self.anim_frame_shown_at = None;
        self.cancel_anim_decode(); // stop an in-flight decode, don't just orphan it
                                   // Video rides the same teardown points (navigate / delete / new source):
                                   // stop the session; the producer exits on the Stop/disconnect and its
                                   // reader retires on a detached thread (never joined here).
        self.stop_video();
        self.prepared = None;
        self.framestep_started = None;
        self.framestep_last = None;
        self.live_revert_at = None;
        self.effects.push(contract::CoreEffect::StopLiveAudio); // dropping the player stops it
    }

    /// Start the Live Photo's audio from the top (its `.mov` track), if `item` is a Live
    /// Photo with audio and audio isn't muted — the "cheap path" (task #38). A no-op for
    /// an animation (no audio track), a silent clip, or when muted. Called when the motion
    /// starts playing from frame 0.
    pub fn start_live_audio(&mut self, item: usize) {
        if self.effective_mute() {
            self.effects.push(contract::CoreEffect::StopLiveAudio);
            return;
        }
        // The core decides the motion path; the shell owns the ObjC player (drained effect).
        // No companion motion → clear any existing audio (mirrors the old `and_then` → None).
        match self.live_motion_path(item) {
            Some(path) => self
                .effects
                .push(contract::CoreEffect::StartLiveAudio { path, at_secs: 0.0 }),
            None => self.effects.push(contract::CoreEffect::StopLiveAudio),
        }
    }

    /// Install a decoded animation as active playback and show its first (or stepped)
    /// frame. `play` starts continuous playback; a non-zero `step` lands paused on that
    /// frame (the frame-step path). Surfaces the truncation toast.
    pub fn install_animation(&mut self, anim: pb_decode::Animation, play: bool, step: i32) {
        let truncated = anim.truncated;
        let mut pb = Playback::new(anim, play);
        if step != 0 {
            pb.step(step);
        }
        self.playback = Some(pb);
        self.present_anim_frame();
        if truncated {
            self.show_toast("Animation truncated");
        }
    }

    /// Upload the current animation frame and redraw (the playback present path —
    /// `set_image`, never the prefetch ring). Resets the per-frame deadline anchor.
    pub fn present_anim_frame(&mut self) {
        {
            let Some(pb) = self.playback.as_ref() else {
                return;
            };
            let color = render_color(&pb.color());
            let frame = pb.current_frame();
            if let Some(a) = self.renderer.as_mut() {
                a.set_image(&frame.rgba, frame.width, frame.height, color, false, 1.0);
            }
        }
        self.anim_frame_shown_at = Some(self.now);
        // Keep a shown detailed-EXIF panel's live "Frame X / N" in sync as the frame
        // changes. Off the hot path (only during user-engaged playback/stepping), and
        // the EXIF read is memoized so this never re-reads the file per frame.
        if self.overlay_shown && self.slot_content() == Some(SlotContent::Details) {
            self.show_overlay(); // rebuilds the table + draws
        } else {
            self.draw();
        }
    }

    /// Advance playback to the due frame and return the next frame's wake deadline
    /// (None when not actively playing), so the loop sleeps exactly until then.
    pub fn tick_playback(&mut self, now: Instant) -> Option<Instant> {
        let shown = self.anim_frame_shown_at;
        let due = self.playback.as_ref().is_some_and(|pb| {
            let since = shown
                .map(|t| now.saturating_duration_since(t))
                .unwrap_or(Duration::ZERO);
            pb.is_due(since)
        });
        if due {
            self.playback.as_mut().unwrap().advance();
            self.present_anim_frame(); // updates anim_frame_shown_at + draws
        }
        // A finished Live Photo reverts to the crisp still after a beat (rather than
        // parking on the low-res last motion frame). Arm the timer once, on the finish.
        let live_finished = self
            .playback
            .as_ref()
            .is_some_and(|pb| pb.is_finished() && pb.kind() == pb_decode::AnimationKind::LivePhoto);
        if live_finished && self.live_revert_at.is_none() {
            self.live_revert_at = Some(now + LIVE_REVERT_DELAY);
        }
        let shown = self.anim_frame_shown_at;
        self.playback
            .as_ref()
            .filter(|pb| pb.is_playing())
            .map(|pb| shown.unwrap_or(now) + pb.current_delay())
    }

    /// Drive the held-key frame-step scrub (`,`/`.`). Returns whether a frame-step key
    /// is held (so the loop keeps polling). One step on press, then repeats at
    /// [`FRAME_STEP_REPEAT`] after the initial tap delay.
    pub fn tick_frame_step(&mut self, now: Instant) -> bool {
        let dir = self.held_frame_step();
        if dir == 0 {
            self.framestep_started = None;
            self.framestep_last = None;
            return false;
        }
        // A live video session scrubs through the session, not `playback` (task
        // #79 follow-up). Forward repeats serve queued frames; backward repeats
        // chain paused seeks (latest-value coalescing absorbs any landing lag).
        if self
            .video
            .as_ref()
            .is_some_and(|v| Some(v.item()) == self.displayed_item)
        {
            let past_delay = timing::elapsed_since(self.framestep_started, now, self.initial_delay);
            let due = timing::elapsed_since(self.framestep_last, now, FRAME_STEP_REPEAT);
            if past_delay && due {
                self.video_frame_step(dir);
                self.framestep_last = Some(now);
            }
            return true;
        }
        // Need a decoded sequence to scrub; while it's still decoding, keep ticking.
        if self.playback.is_none() {
            return true;
        }
        let past_delay = timing::elapsed_since(self.framestep_started, now, self.initial_delay);
        let due = timing::elapsed_since(self.framestep_last, now, FRAME_STEP_REPEAT);
        if past_delay && due {
            self.playback.as_mut().unwrap().step(dir);
            self.present_anim_frame();
            self.framestep_last = Some(now);
        }
        true
    }

    /// Whether item `item` is an animated container (from the cached header sniff).
    pub fn current_is_animated(&self, item: usize) -> bool {
        self.meta_cache
            .get(&item)
            .and_then(|m| m.animated)
            .is_some()
    }

    /// The companion motion `.mov` for item `item` if it's a Live Photo, else `None`
    /// (tasks #38 / #39). Filesystem pairing, memoized per item and computed lazily —
    /// only ever reached when settled on a photo, never on the blaze-through path. Always
    /// `None` on platforms without a motion decoder (macOS + Windows have one).
    pub fn live_motion_path(&mut self, item: usize) -> Option<PathBuf> {
        #[cfg(not(any(
            target_os = "macos",
            windows,
            all(unix, not(target_os = "macos"), feature = "livephoto")
        )))]
        {
            let _ = item;
            None
        }
        #[cfg(any(
            target_os = "macos",
            windows,
            all(unix, not(target_os = "macos"), feature = "livephoto")
        ))]
        if let Some(cached) = self.live_motion_cache.get(&item) {
            return cached.clone();
        }
        #[cfg(any(
            target_os = "macos",
            windows,
            all(unix, not(target_os = "macos"), feature = "livephoto")
        ))]
        {
            // Only image items Live-pair (task #79): a *video* item with a same-stem
            // .mov sibling (IMG_1.MP4 + IMG_1.MOV) is two videos, not a Live Photo.
            let paired = match crate::video::item_kind(self.source.as_ref(), item) {
                crate::video::LibraryItemKind::Image => {
                    self.source.path(item).and_then(companion_motion)
                }
                // Neither a video nor a door Live-pairs: a door is not half of
                // anything, and a same-stem .mov beside `holiday.zip` is unrelated.
                crate::video::LibraryItemKind::Video(_)
                | crate::video::LibraryItemKind::Archive(_) => None,
            };
            self.live_motion_cache.insert(item, paired.clone());
            paired
        }
    }

    /// Whether item `item` has an on-demand motion component to play on `P` — either an
    /// animated container (GIF/APNG/WebP/HEIF sequence) or a Live Photo's `.mov`.
    pub fn has_motion(&mut self, item: usize) -> bool {
        self.current_is_animated(item) || self.live_motion_path(item).is_some()
    }

    /// Whether the currently displayed item has a playable motion component — the macOS
    /// toolbar dims its Play button on stills (task #55). Includes **video** (task 79.9):
    /// a video is playable motion too, so the toolbar Play button enables on it. `&mut`
    /// because Live-Photo pairing is resolved + cached on first check (cheap cache hit
    /// after; the display path has usually primed it already).
    pub fn current_has_motion(&mut self) -> bool {
        self.displayed_item
            .is_some_and(|i| self.has_motion(i) || self.item_is_video(i))
    }

    /// Whether an animation / Live Photo is actively playing — the toolbar lights its
    /// Play-Animation button while it runs.
    pub fn animation_playing(&self) -> bool {
        self.playback.as_ref().is_some_and(|pb| pb.is_playing())
    }

    /// Whether *any* motion is playing — an animation/Live Photo **or** a video (task
    /// 79.9). The toolbar's Play/Pause glyph reads this so it reflects a playing video,
    /// not just an animation (`animation_playing` is video-blind by design).
    pub fn motion_playing(&self) -> bool {
        self.animation_playing() || self.video_playing()
    }

    /// The **displayed** photo's 1-based position and total count, for the toolbar counter
    /// (task #61) — mirrors the window title's `(idx+1/n)` (`title_for`). Derived from
    /// [`displayed_item`](Self::displayed_item), the *present-truth* index, **not** the nav
    /// target: during a resident-ring miss the target advances while the old photo is still
    /// on screen, so a target-based counter would lie. `None` until the first image is
    /// presented (the counter hides on a cold start / empty deck).
    pub fn display_counter(&self) -> Option<(usize, usize)> {
        self.displayed_item.map(|i| (i + 1, self.source.len()))
    }

    /// Kick the whole-sequence decode for `item` on a worker thread so a big GIF/WebP (or
    /// a Live Photo `.mov`) never stalls the event loop; the still first frame stays on
    /// screen until it lands (picked up by `poll_anim_decode`). `want` decides what
    /// happens on arrival — eager prep (stash ready), play (`P`), or step (frame-step).
    /// Signal any in-flight animation decode to stop and drop it. The worker checks the flag and
    /// bails early rather than decoding the whole clip onto a now-dropped channel — so navigating
    /// through Live Photos doesn't pile up orphaned decodes (wasted CPU + transient RAM).
    pub fn cancel_anim_decode(&mut self) {
        if let Some(d) = &self.anim_decode {
            d.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.anim_decode = None;
        self.cancel_anim_stream();
    }

    /// Signal any in-flight streaming Live Photo decode (task #69) to stop and drop it — the
    /// worker checks the flag per packet and bails. Called on navigate/supersede (via
    /// [`cancel_anim_decode`](Self::cancel_anim_decode)) so streams don't pile up.
    pub fn cancel_anim_stream(&mut self) {
        if let Some(s) = &self.anim_stream {
            s.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.anim_stream = None;
    }

    pub fn start_animation_decode(&mut self, item: usize, want: AnimWant) {
        // A Live Photo streams its motion (task #69) — play the `.mov` while it's still
        // decoding rather than waiting for the whole clip. Wired on every platform with a
        // motion decoder: the Linux FFmpeg path, the macOS AVAssetReader path, and the
        // Windows Media Foundation path. (GIF/APNG/WebP stay on the batch path below
        // everywhere — decoded from the still bytes.)
        #[cfg(any(
            target_os = "macos",
            windows,
            all(unix, not(target_os = "macos"), feature = "livephoto")
        ))]
        if self.live_motion_path(item).is_some() {
            self.start_live_stream(item, want);
            return;
        }
        // Supersede any in-flight decode so its orphaned worker stops promptly (see `cancel`).
        self.cancel_anim_decode();
        self.anim_gen += 1;
        let gen = self.anim_gen;
        let epoch = self.epoch;
        let source = Arc::clone(&self.source);
        let fit = self.decode_fit();
        // A Live Photo decodes its companion `.mov` via AVFoundation; everything else
        // decodes the still's own bytes as a multi-frame animation.
        let live = self.live_motion_path(item);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_job = std::sync::Arc::clone(&cancel);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = decode_motion_job(live, &source, item, fit, &cancel_job);
            let _ = tx.send(result);
        });
        self.anim_decode = Some(AnimDecode {
            gen,
            item,
            epoch,
            want,
            rx,
            cancel,
        });
        // A user-initiated decode (P / step) means they've engaged — suppress the "▶ P"
        // hint. An eager prep is invisible background work, so leave the hint alone.
        if want != AnimWant::Eager {
            self.anim_hint_shown_for = self.displayed_item;
        }
    }

    /// Kick a **streaming** Live Photo motion decode (task #69) — FFmpeg on Linux,
    /// AVAssetReader on macOS, Media Foundation on Windows: the worker emits each frame as
    /// it's decoded (mapped onto the platform-neutral [`StreamMsg`]), and
    /// [`poll_anim_stream`](Self::poll_anim_stream) installs/extends the playing sequence so
    /// the clip starts within a frame or two instead of after the whole `.mov`. Same cancel /
    /// generation / epoch discipline as [`start_animation_decode`](Self::start_animation_decode).
    #[cfg(any(
        target_os = "macos",
        windows,
        all(unix, not(target_os = "macos"), feature = "livephoto")
    ))]
    pub fn start_live_stream(&mut self, item: usize, want: AnimWant) {
        // Supersede any in-flight decode/stream so its orphaned worker stops promptly.
        self.cancel_anim_decode();
        self.anim_gen += 1;
        let gen = self.anim_gen;
        let epoch = self.epoch;
        let Some(path) = self.live_motion_path(item) else {
            return;
        };
        // Cap the motion's long edge to the display fit (decode-to-fit), never above the RAM
        // ceiling — the same bound the batch `decode_motion_job` uses.
        let edge = self
            .decode_fit()
            .map(|f| f.max_width.max(f.max_height))
            .unwrap_or(crate::engine::MOTION_MAX_LONG_EDGE)
            .min(crate::engine::MOTION_MAX_LONG_EDGE);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_job = std::sync::Arc::clone(&cancel);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Map the decoder's chunks onto the neutral `StreamMsg` the core wiring consumes.
            let mut emit = |chunk: pb_decode::MotionChunk| {
                let msg = match chunk {
                    pb_decode::MotionChunk::Header(h) => StreamMsg::Header {
                        width: h.width,
                        height: h.height,
                        color: h.color,
                        codec: h.codec,
                    },
                    pb_decode::MotionChunk::Frame(f) => StreamMsg::Frame(f),
                    pb_decode::MotionChunk::Done {
                        loop_count,
                        truncated,
                    } => StreamMsg::Done {
                        loop_count,
                        truncated,
                    },
                    pb_decode::MotionChunk::Failed(e) => StreamMsg::Failed(e.to_string()),
                };
                let _ = tx.send(msg);
            };
            pb_decode::decode_live_motion_streaming(&path, edge, &cancel_job, &mut emit);
        });
        self.anim_stream = Some(crate::animation::AnimStream {
            gen,
            item,
            epoch,
            want,
            rx,
            cancel,
            header: None,
            pending: Vec::new(),
            installed: false,
        });
        if want != AnimWant::Eager {
            self.anim_hint_shown_for = self.displayed_item;
        }
    }

    /// When the user has rested on an animated still, eagerly decode the whole sequence
    /// in the background so pressing `P` is instant (fixes the slow first-play on WebP /
    /// AVIF, ~0.6–2s to decode). Returns the wake deadline while the dwell elapses (so
    /// the idle loop wakes to kick it), else `None`. Strictly off the hot path — only
    /// when settled (never while blazing), exactly when the prefetch pool is idle.
    pub fn maybe_prepare_animation(&mut self, now: Instant) -> Option<Instant> {
        if self.playback.is_some() || self.anim_decode.is_some() || self.anim_stream.is_some() {
            return None; // already playing, or a decode/stream is already in flight
        }
        let item = self.displayed_item?;
        if !self.target_caught_up() {
            return None; // still catching up to the target (incl. a geometry re-present) — not settled
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
        if let Some(slot) = self.display_slot(item) {
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
