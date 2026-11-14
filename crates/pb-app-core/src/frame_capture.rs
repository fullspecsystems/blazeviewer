//! **Frame capture** (task #81, export-plan.md rung 0a): *which* frame Copy, Copy Text,
//! Describe/Ask and Export act on, and the worker-side decode that fetches it at full
//! resolution.
//!
//! Before this, every one of them used the item's **still** — for a video, its poster —
//! even while a different frame was on screen. Now the command freezes a [`FrameSource`] at
//! the moment it runs (`AppCore::frame_target`, in `app_core_impl/frame_capture.rs`):
//!
//! - a **video** with a live player/session: its playback position. The Session route's
//!   position is the presented frame's own PTS; the macOS AVPlayer route's is the ~20 Hz
//!   mirror, exact whenever the video is paused (the shell publishes on pause, seek and
//!   step) and at most one report stale while it plays;
//! - a **Live Photo's motion** on screen: the motion movie at the shown frame's time;
//! - an **animation** frame on screen: its index;
//! - otherwise the **still** — which for a video showing its poster *is* the frame on screen.
//!
//! [`capture`] then runs on the consumer's worker thread (never the event loop) and decodes
//! that frame: a video through FFmpeg (macOS, Linux) or Media Foundation's replay (Windows);
//! an animation by re-decoding at full size only when the playing frames were fitted to the
//! screen. A result cache keyed by item records the [`FrameKey`] it was made from, so a
//! cached OCR or description of one frame is never served for another. On demand only —
//! nothing captures while you merely watch.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use pb_decode::{DecodeError, DecodedImage, PixelFormat, VideoInput};
use pb_source::ItemSource;

/// Which frame a result was made from — the cache key that keeps a cached OCR or
/// description of one frame from being served for another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum FrameKey {
    /// The item's still (a photo; a video's poster; a Live Photo's still).
    #[default]
    Still,
    /// A video or Live Photo motion frame, by playback time (ms).
    Video { millis: u64 },
    /// An animation frame, by index.
    Animation { index: usize },
}

/// The frame a command acts on, frozen when it runs — owned, so it travels to a worker.
#[derive(Debug, Clone)]
pub enum FrameSource {
    /// The item's still: a photo, a video's poster, a Live Photo's still.
    Still,
    /// A video at `at` on its playback timeline. `input` is the session's exact input when
    /// it has one (archive bytes already in RAM); otherwise it is resolved from the item.
    Video {
        input: Option<VideoInput>,
        at: Duration,
    },
    /// A Live Photo's motion movie at `at`; `shown` (the frame on screen, motion-sized) is
    /// the fallback if the movie can't be decoded at full size.
    LiveMotion {
        movie: PathBuf,
        at: Duration,
        shown: Box<DecodedImage>,
    },
    /// Frame `index` of an animation. `shown` is that frame as displayed; `native` says it
    /// is already full size, so nothing is re-decoded.
    Animation {
        index: usize,
        shown: Box<DecodedImage>,
        native: bool,
    },
}

impl FrameSource {
    /// The cache key for results made from this frame.
    pub fn key(&self) -> FrameKey {
        match self {
            FrameSource::Still => FrameKey::Still,
            FrameSource::Video { at, .. } | FrameSource::LiveMotion { at, .. } => FrameKey::Video {
                millis: at.as_millis() as u64,
            },
            FrameSource::Animation { index, .. } => FrameKey::Animation { index: *index },
        }
    }

    /// The still — no capture needed (the original decode/export paths apply).
    pub fn is_still(&self) -> bool {
        matches!(self, FrameSource::Still)
    }

    /// A suffix for an exported file's name: `-0h01m23s` for a video time, `-frame12`
    /// (1-based) for an animation frame, nothing for the still.
    pub fn name_suffix(&self) -> String {
        match self {
            FrameSource::Still => String::new(),
            FrameSource::Video { at, .. } | FrameSource::LiveMotion { at, .. } => {
                let s = at.as_secs();
                format!("-{}h{:02}m{:02}s", s / 3600, (s / 60) % 60, s % 60)
            }
            FrameSource::Animation { index, .. } => format!("-frame{}", index + 1),
        }
    }
}

/// A captured frame ready for the clipboard: rotated straight-alpha RGBA8, width, height.
pub type ClipboardFrame = (Vec<u8>, u32, u32);

/// A Copy Image capture in flight: the captured frame arrives on `rx`.
pub struct CopyCapture {
    pub item: usize,
    pub rx: std::sync::mpsc::Receiver<Result<ClipboardFrame, String>>,
}

/// `AppCore::frame_capture` — the one field this subsystem adds.
#[derive(Default)]
pub struct FrameCaptureState {
    /// Copy Image of a video/animation frame, capturing off the event loop.
    pub copy: Option<CopyCapture>,
    /// Which frame each cached description (`AppCore::descriptions`) was made from. A
    /// description cached without an entry here is the still's.
    pub described: std::collections::HashMap<usize, FrameKey>,
}

/// Whether this build can decode a video frame at a time (FFmpeg on macOS/Linux, Media
/// Foundation on Windows). Without it, video frames can't be captured and the commands say
/// so rather than silently using the poster.
pub const CAN_CAPTURE_VIDEO: bool = cfg!(any(windows, feature = "ffvideo"));

/// Decode `frame` of `item` at full resolution. Runs on a worker thread; honours `cancel`.
pub fn capture(
    source: &dyn ItemSource,
    item: usize,
    frame: &FrameSource,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    match frame {
        FrameSource::Still => still(source, item, cancel),
        FrameSource::Video { input, at } => {
            let input = match input {
                Some(i) => i.clone(),
                None => video_input(source, item)?,
            };
            video_frame_at(&input, *at, cancel)
        }
        FrameSource::LiveMotion { movie, at, shown } => {
            let movie = VideoInput::Path(movie.clone());
            video_frame_at(&movie, *at, cancel).or_else(|e| {
                eprintln!(
                    "frame capture: full-size motion frame failed ({e}); using the shown one"
                );
                Ok((**shown).clone())
            })
        }
        FrameSource::Animation {
            index,
            shown,
            native,
        } => {
            if *native {
                return Ok((**shown).clone());
            }
            animation_frame(source, item, *index, cancel).or_else(|e| {
                eprintln!(
                    "frame capture: full-size animation frame failed ({e}); using the shown frame"
                );
                Ok((**shown).clone())
            })
        }
    }
}

/// The still — the display decode at full size. A macOS archive MP4/MOV's display poster
/// is drawn by the shell (AVFoundation can't read archive bytes here), so the decode
/// returns a placeholder tile; for a consumer, take the FFmpeg poster of the bytes instead.
fn still(
    source: &dyn ItemSource,
    item: usize,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    #[cfg(all(target_os = "macos", feature = "ffvideo"))]
    if source.path(item).is_none()
        && matches!(
            crate::video::item_kind(source, item),
            crate::video::LibraryItemKind::Video(_)
        )
    {
        return pb_decode::ff_decode_video_poster(&video_input(source, item)?, None, cancel);
    }
    crate::engine::decode_item_cancellable(source, item, None, false, cancel)
}

/// The item as a video input: its path, or an archive entry's bytes.
fn video_input(source: &dyn ItemSource, item: usize) -> Result<VideoInput, DecodeError> {
    match source.path(item) {
        Some(p) => Ok(VideoInput::Path(p.to_path_buf())),
        None => {
            let data = source
                .bytes(item)
                .map_err(|e| DecodeError::Corrupt(format!("read: {e}")))?;
            Ok(VideoInput::Bytes {
                data: std::sync::Arc::new(data),
                name: source.name(item).to_string(),
            })
        }
    }
}

/// The frame shown at `at` on `input`'s playback timeline, at full resolution.
#[allow(unused_variables)] // a build with neither backend uses none of them
fn video_frame_at(
    input: &VideoInput,
    at: Duration,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    // Windows: Media Foundation, the decoder playback uses. The replay seeks to the
    // preceding keyframe and decodes forward to the first frame at/after the target —
    // the presented frame's own PTS on the Session route (origin = the stream start).
    #[cfg(windows)]
    {
        let thumb = pb_decode::FitBox {
            max_width: 64,
            max_height: 64,
        };
        let at_hns = (at.as_nanos() / 100) as i64;
        return pb_decode::decode_video_poster_replay(input, 0, at_hns, None, thumb, true, cancel)?
            .native
            .ok_or_else(|| DecodeError::Corrupt("no frame at that time".into()));
    }
    #[cfg(all(not(windows), feature = "ffvideo"))]
    {
        return pb_decode::ff_decode_frame_at(input, at, cancel);
    }
    #[allow(unreachable_code)]
    Err(DecodeError::Unsupported)
}

/// Frame `index` of the item's animation at full size: the same decoder playback uses, so
/// the indices agree, without the fit (bounded by its own frame and byte caps).
fn animation_frame(
    source: &dyn ItemSource,
    item: usize,
    index: usize,
    cancel: &AtomicBool,
) -> Result<DecodedImage, DecodeError> {
    let bytes = source
        .bytes(item)
        .map_err(|e| DecodeError::Corrupt(format!("read: {e}")))?;
    let anim = pb_decode::decode_animation_cancellable(&bytes, None, cancel)?;
    let codec = anim.codec;
    let color = anim.color;
    let frame = anim
        .frames
        .into_iter()
        .nth(index)
        .ok_or_else(|| DecodeError::Corrupt("frame past the decoded sequence".into()))?;
    Ok(rgba8_image(
        frame.rgba,
        frame.width,
        frame.height,
        codec,
        color,
    ))
}

/// Wrap straight-alpha RGBA8 pixels (an animation or motion frame) as a [`DecodedImage`].
pub fn rgba8_image(
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    codec: &'static str,
    color: pb_decode::ColorTransform,
) -> DecodedImage {
    DecodedImage {
        width,
        height,
        orig_width: width,
        orig_height: height,
        codec,
        format: PixelFormat::Rgba8,
        pixels: rgba,
        is_preview: false,
        color,
        peak: 1.0,
        animated: None,
        recovered: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_name_suffixes() {
        let v = FrameSource::Video {
            input: None,
            at: Duration::from_millis(83_250),
        };
        assert_eq!(v.key(), FrameKey::Video { millis: 83_250 });
        assert_eq!(v.name_suffix(), "-0h01m23s");
        let long = FrameSource::Video {
            input: None,
            at: Duration::from_secs(2 * 3600 + 5 * 60 + 9),
        };
        assert_eq!(long.name_suffix(), "-2h05m09s");
        let shown = Box::new(rgba8_image(
            vec![0; 4],
            1,
            1,
            "GIF",
            pb_decode::ColorTransform::srgb(),
        ));
        let a = FrameSource::Animation {
            index: 11,
            shown,
            native: true,
        };
        assert_eq!(a.key(), FrameKey::Animation { index: 11 });
        assert_eq!(a.name_suffix(), "-frame12");
        assert_eq!(FrameSource::Still.key(), FrameKey::Still);
        assert_eq!(FrameSource::Still.name_suffix(), "");
        assert!(FrameSource::Still.is_still() && !a.is_still());
    }

    /// A native-size animation frame is used as shown — no decode, not even a byte read.
    #[test]
    fn a_native_animation_frame_is_not_re_decoded() {
        struct NoBytes;
        impl ItemSource for NoBytes {
            fn len(&self) -> usize {
                1
            }
            fn name(&self, _: usize) -> &str {
                "a.gif"
            }
            fn bytes(&self, _: usize) -> std::io::Result<Vec<u8>> {
                panic!("a native frame must not be re-read")
            }
        }
        let shown = rgba8_image(
            vec![1, 2, 3, 255],
            1,
            1,
            "GIF",
            pb_decode::ColorTransform::srgb(),
        );
        let frame = FrameSource::Animation {
            index: 3,
            shown: Box::new(shown),
            native: true,
        };
        let img = capture(&NoBytes, 0, &frame, &AtomicBool::new(false)).unwrap();
        assert_eq!(img.pixels, vec![1, 2, 3, 255]);
    }

    /// A video frame from the frame-index fixture (frame k = grey 16 + 8k): the capture
    /// returns the frame at the frozen time, not the poster.
    #[cfg(feature = "ffvideo")]
    #[test]
    fn a_video_capture_returns_the_frame_at_its_time() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../pb-decode/tests/fixtures/video/frame_index.mp4");
        let source = pb_source::FsSource::new(vec![path]);
        let frame = FrameSource::Video {
            input: None,
            at: Duration::from_secs_f64(20.0 / 30.0),
        };
        let img = capture(&source, 0, &frame, &AtomicBool::new(false)).unwrap();
        let mean = img.pixels.chunks_exact(4).map(|p| p[0] as f32).sum::<f32>()
            / (img.pixels.len() / 4) as f32;
        assert!((mean - (16.0 + 160.0)).abs() < 4.0, "frame 20, got {mean}");
    }
}
