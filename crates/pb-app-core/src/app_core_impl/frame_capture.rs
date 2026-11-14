//! **Frame capture** — the `AppCore` half of [`crate::frame_capture`] (task #81; the
//! two-halves rule in `docs/where-code-goes.md`).
//!
//! Reads live playback state to decide which frame a command acts on
//! ([`frame_target`](AppCore::frame_target) / the cheap [`frame_key`](AppCore::frame_key)),
//! and runs Copy Image's capture off the event loop — a video or animation frame is a decode
//! (seconds, worst case), and Copy used to run even a video's poster walk synchronously.

use super::*;
use crate::frame_capture::{FrameKey, FrameSource};

/// Where the item is on screen right now — cheap (no pixel clones); the one decision both
/// [`AppCore::frame_key`] and [`AppCore::frame_target`] are built from, so they can't
/// disagree.
enum FramePos {
    Still,
    Video(Duration),
    Live(Duration),
    Animation(usize),
}

impl AppCore {
    fn frame_pos(&self, item: usize) -> FramePos {
        use crate::video::VideoSessionState as S;
        // A video with a live (or ended — its last frame stays parked) player/session. While
        // it is still opening, the poster is what's on screen: the still.
        if let Some(v) = self.video.as_ref().filter(|v| v.item() == item) {
            if !matches!(v.state(), S::Opening | S::Failed | S::Stopped) {
                let at = match v.as_session() {
                    // The presented frame's own PTS — exactly the pixels on screen.
                    Some(s) => s.session.current_pts,
                    // AVPlayer's mirrored playhead (exact once paused).
                    None => v.as_native().and_then(|p| p.position()),
                };
                if let Some(at) = at {
                    return FramePos::Video(at);
                }
            }
        }
        // `playback` belongs to the displayed item (`stop_playback` runs on every navigate).
        if let (Some(pb), Some(true)) = (
            self.playback.as_ref(),
            self.displayed_item.map(|d| d == item),
        ) {
            return if pb.kind() == pb_decode::AnimationKind::LivePhoto {
                FramePos::Live(pb.elapsed())
            } else {
                FramePos::Animation(pb.index())
            };
        }
        FramePos::Still
    }

    /// The cache key for the frame of `item` on screen now (see [`FrameKey`]).
    pub(crate) fn frame_key(&self, item: usize) -> FrameKey {
        match self.frame_pos(item) {
            FramePos::Still => FrameKey::Still,
            FramePos::Video(at) | FramePos::Live(at) => FrameKey::Video {
                millis: at.as_millis() as u64,
            },
            FramePos::Animation(index) => FrameKey::Animation { index },
        }
    }

    /// Whether `item`'s frame is changing on its own — a video or animation playing. The
    /// automatic OCR / describe runs wait for it to stop rather than chase every frame.
    pub(crate) fn frame_is_moving(&self, item: usize) -> bool {
        let video = self
            .video
            .as_ref()
            .is_some_and(|v| v.item() == item && v.is_playing());
        let anim = self.playback.as_ref().is_some_and(|pb| pb.is_playing())
            && self.displayed_item == Some(item);
        video || anim
    }

    /// The frame of `item` a command acts on, frozen now (task #81). Explicit commands only —
    /// it clones the on-screen frame for animations; per-tick callers use
    /// [`frame_key`](Self::frame_key).
    pub(crate) fn frame_target(&mut self, item: usize) -> FrameSource {
        match self.frame_pos(item) {
            FramePos::Still => FrameSource::Still,
            FramePos::Video(at) => FrameSource::Video {
                input: self
                    .video
                    .as_ref()
                    .and_then(|v| v.as_session())
                    .and_then(|s| s.media.get().cloned()),
                at,
            },
            FramePos::Live(at) => {
                let shown = Box::new(self.shown_animation_frame());
                match self.live_motion_path(item) {
                    Some(movie) => FrameSource::LiveMotion { movie, at, shown },
                    // No movie to re-decode: the frame on screen is the best there is.
                    None => FrameSource::Animation {
                        index: 0,
                        shown,
                        native: true,
                    },
                }
            }
            FramePos::Animation(index) => {
                let shown = self.shown_animation_frame();
                // The playing frames are fitted to the screen; fitting never upscales, so a
                // frame smaller than the fit box on both axes is already full size.
                let native = self
                    .decode_fit()
                    .is_none_or(|fit| shown.width < fit.max_width && shown.height < fit.max_height);
                FrameSource::Animation {
                    index,
                    shown: Box::new(shown),
                    native,
                }
            }
        }
    }

    fn shown_animation_frame(&self) -> pb_decode::DecodedImage {
        let pb = self.playback.as_ref().expect("an animation is playing");
        let f = pb.current_frame();
        crate::frame_capture::rgba8_image(f.rgba.clone(), f.width, f.height, pb.codec(), pb.color())
    }

    /// Copy Image for a video or animation frame (task #81): capture on a worker, then
    /// write the clipboard when it lands ([`poll_frame_copy`](Self::poll_frame_copy)). The
    /// in-RAM rotation is baked in. Image-only — offering the video file too would make
    /// file-preferring apps (Messages, Mail, Slack) paste the whole movie.
    pub(super) fn copy_frame(&mut self, item: usize, frame: FrameSource) {
        if matches!(frame, FrameSource::Video { .. }) && !crate::frame_capture::CAN_CAPTURE_VIDEO {
            self.show_toast("Copying video frames isn't supported in this build");
            return;
        }
        let source = Arc::clone(&self.source);
        let rot = self.rotations.get(&item).copied().unwrap_or_default();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("pb-frame-copy".into())
            .spawn(move || {
                crate::export_worker::lower_thread_priority();
                let cancel = std::sync::atomic::AtomicBool::new(false);
                let result = crate::frame_capture::capture(source.as_ref(), item, &frame, &cancel)
                    .map(|img| {
                        let rgba = to_clipboard_rgba8(&img);
                        rotate_rgba8(&rgba, img.width, img.height, rot)
                    })
                    .map_err(|e| e.to_string());
                let _ = tx.send(result);
            });
        if spawned.is_err() {
            self.show_toast("Copy failed");
            return;
        }
        // A second Copy replaces the first (its receiver drops; that worker's send fails).
        self.frame_capture.copy = Some(crate::frame_capture::CopyCapture { item, rx });
        self.show_toast("Copying frame…");
    }

    /// Land a finished frame copy (called each tick).
    pub fn poll_frame_copy(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let result = match self.frame_capture.copy.as_ref().map(|c| c.rx.try_recv()) {
            None | Some(Err(TryRecvError::Empty)) => return,
            Some(Ok(r)) => r,
            Some(Err(TryRecvError::Disconnected)) => Err("the capture worker stopped".into()),
        };
        let item = self.frame_capture.copy.take().map(|c| c.item);
        match result {
            Ok((rgba, w, h)) => self.effects.push(contract::CoreEffect::WriteClipboard(
                contract::ClipboardPayload::Image {
                    rgba,
                    w,
                    h,
                    file: None,
                },
            )),
            Err(why) => {
                if let Some(item) = item {
                    eprintln!(
                        "copy: frame capture failed: {}: {why}",
                        self.source.name(item)
                    );
                }
                self.show_toast("Copy failed — couldn't read that frame");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::test_core;
    use crate::video::{VideoSessionId, VideoSessionState as S};
    use crate::video_native::{ActiveVideoBackend, NativeVideoProxy};

    /// 30 frames at 30 fps; frame k is a flat grey of RGB 16 + 8k (pb-decode's fixture).
    fn frame_index_clip() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video/frame_index.mp4")
    }

    /// A core showing the frame-index clip on the AVPlayer-style (native) route, in `state`
    /// at `at`.
    fn video_core(state: S, at: Duration) -> AppCore {
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![frame_index_clip()]));
        core.displayed_item = Some(0);
        let id = VideoSessionId(7);
        let mut p = NativeVideoProxy::new(0, id, true);
        p.on_state_changed(id, state);
        p.set_position(at);
        core.video = Some(ActiveVideoBackend::Native(p));
        core
    }

    fn pump(core: &mut AppCore, mut done: impl FnMut(&AppCore) -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !done(core) {
            assert!(std::time::Instant::now() < deadline, "never settled");
            core.poll_frame_copy();
            core.poll_export();
            core.poll_text_scan();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn mean_red(rgba: &[u8]) -> f32 {
        let px = rgba.chunks_exact(4);
        let n = px.len() as f32;
        px.map(|p| p[0] as f32).sum::<f32>() / n
    }

    #[test]
    fn the_frozen_frame_follows_the_player() {
        let at = Duration::from_millis(1234);
        let mut core = video_core(S::Paused, at);
        assert_eq!(core.frame_key(0), FrameKey::Video { millis: 1234 });
        assert!(!core.frame_is_moving(0));
        assert!(matches!(core.frame_target(0), FrameSource::Video { at: a, .. } if a == at));
        // Playing: still a frame (the one at the keypress), but a moving one.
        let core = video_core(S::Playing, at);
        assert!(core.frame_is_moving(0));
        // Still opening: the poster is on screen — the still.
        let core = video_core(S::Opening, at);
        assert_eq!(core.frame_key(0), FrameKey::Still);
        // Another item's player doesn't count.
        let mut core = video_core(S::Paused, at);
        core.source = Arc::new(pb_source::FsSource::new(vec![
            frame_index_clip(),
            frame_index_clip(),
        ]));
        core.displayed_item = Some(1);
        assert_eq!(core.frame_key(1), FrameKey::Still);
    }

    /// Copy on a video never blocks the event loop — even the poster is captured on a
    /// worker (the poster walk used to run synchronously, for up to 15 s).
    #[test]
    fn copy_on_a_video_captures_off_the_event_loop() {
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![frame_index_clip()]));
        core.displayed_item = Some(0);
        core.copy_image();
        assert!(core.frame_capture.copy.is_some(), "a capture is in flight");
        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::WriteClipboard(_))),
            "nothing written synchronously"
        );
    }

    /// The paused frame is what lands on the clipboard — image only (no video file).
    #[cfg(feature = "ffvideo")]
    #[test]
    fn copy_on_a_paused_video_copies_the_frame_on_screen() {
        let mut core = video_core(S::Paused, Duration::from_secs_f64(20.0 / 30.0));
        core.copy_image();
        pump(&mut core, |c| c.frame_capture.copy.is_none());
        let copied = core.effects.iter().find_map(|e| match e {
            contract::CoreEffect::WriteClipboard(contract::ClipboardPayload::Image {
                rgba,
                w,
                h,
                file,
            }) => Some((rgba.clone(), *w, *h, file.clone())),
            _ => None,
        });
        let (rgba, w, h, file) = copied.expect("a clipboard write");
        assert_eq!((w, h), (64, 48), "full resolution");
        assert!(file.is_none(), "image only — not the movie file");
        let red = mean_red(&rgba);
        assert!((red - 176.0).abs() < 4.0, "frame 20 (grey 176), got {red}");
    }

    /// A reading cached from the still (or another frame) is never served for the paused
    /// frame: Copy Text starts a fresh read of the frame on screen.
    #[test]
    fn copy_text_reads_the_frame_on_screen_not_another_frames_cache() {
        let mut core = video_core(S::Paused, Duration::from_secs(1));
        core.recognized_text.insert(
            0,
            crate::app_core_impl::test_support::text_result(&[], &["poster text"]),
        );
        core.copy_image_text();
        let scan = core.text_scan.as_ref().expect("a fresh read");
        assert_eq!(scan.frame, FrameKey::Video { millis: 1000 });
        assert!(scan.copy_when_done);
        assert!(
            !core.recognized_text.contains_key(&0),
            "the other frame's text is dropped, not shown as this frame's"
        );
        assert!(
            crate::app_core_impl::test_support::clipboard_text_effects(&core).is_empty(),
            "the poster's text was not copied"
        );
    }

    /// The automatic read (Text tab open) waits for playback to stop, then reads the
    /// paused frame once.
    #[test]
    fn the_automatic_text_read_waits_for_playback_to_stop() {
        let mut core = video_core(S::Playing, Duration::from_secs(1));
        core.ensure_text_scan();
        assert!(core.text_scan.is_none(), "not while playing");
        let mut core = video_core(S::Paused, Duration::from_secs(1));
        core.ensure_text_scan();
        assert_eq!(
            core.text_scan.as_ref().map(|s| s.frame),
            Some(FrameKey::Video { millis: 1000 })
        );
    }

    /// An animation's current frame: keyed by index, used as shown when it's full size.
    #[test]
    fn an_animation_frame_is_the_one_on_screen() {
        let frame = |v: u8| pb_decode::AnimFrame {
            rgba: [v, v, v, 255].repeat(4),
            width: 2,
            height: 2,
            delay: Duration::from_millis(100),
        };
        let anim = pb_decode::Animation {
            kind: pb_decode::AnimationKind::Gif,
            width: 2,
            height: 2,
            frames: vec![frame(10), frame(20), frame(30)],
            loop_count: 0,
            codec: "GIF",
            color: pb_decode::ColorTransform::srgb(),
            truncated: false,
        };
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![PathBuf::from("/x/a.gif")]));
        core.displayed_item = Some(0);
        let mut pb = crate::animation::Playback::new(anim, false);
        pb.step(1);
        core.playback = Some(pb);
        assert_eq!(core.frame_key(0), FrameKey::Animation { index: 1 });
        let FrameSource::Animation {
            index,
            shown,
            native,
        } = core.frame_target(0)
        else {
            panic!("an animation frame")
        };
        assert_eq!((index, native), (1, true), "2×2 is under any fit box");
        assert_eq!(shown.pixels[0], 20);
        // Copy lands that frame (no file read needed — it is native size).
        core.copy_image();
        pump(&mut core, |c| c.frame_capture.copy.is_none());
        let red = core.effects.iter().find_map(|e| match e {
            contract::CoreEffect::WriteClipboard(contract::ClipboardPayload::Image {
                rgba,
                ..
            }) => Some(mean_red(rgba)),
            _ => None,
        });
        assert_eq!(red, Some(20.0));
    }

    /// Export a paused video frame end to end: the suggested name carries the time, and the
    /// PNG holds exactly that frame at full size.
    #[cfg(feature = "ffvideo")]
    #[test]
    fn exporting_a_paused_video_frame_writes_that_frame() {
        let out = std::env::temp_dir().join(format!("pb-frame-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(&out).unwrap();
        let mut core = video_core(S::Paused, Duration::from_secs_f64(12.0 / 30.0));
        core.persist_prefs = false;
        assert!(core.can_export());
        core.export_cmd();
        pump(&mut core, |c| {
            c.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::ShowSavePanel { .. }))
        });
        let (id, name) = core
            .effects
            .iter()
            .find_map(|e| match e {
                contract::CoreEffect::ShowSavePanel {
                    request_id,
                    suggested_name,
                    ..
                } => Some((*request_id, suggested_name.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(name, "frame_index-0h00m00s.png");
        let dest = out.join("f.png");
        core.save_panel_resolved(id, Some(dest.clone()));
        pump(&mut core, |c| !c.export.busy());
        let png = std::fs::read(&dest).expect("written");
        let img = pb_decode::decode_named_bytes("f.png", &png, None, false).unwrap();
        assert_eq!((img.width, img.height), (64, 48));
        let red = mean_red(&img.pixels);
        assert!(
            (red - (16.0 + 96.0)).abs() < 4.0,
            "frame 12 (grey 112), got {red}"
        );
        let _ = std::fs::remove_dir_all(&out);
    }
}
