//! Video playback session contracts (task #79, phase 0) — the shell-neutral model the
//! `VideoSession` (phase 4) is built on: typed library items, the session state machine's
//! states + legal transitions, the audio-clock return path, and the byte-budgeted queue
//! admission invariant.
//!
//! This is a *contract module*: pure data + pure rules, unit-tested, no I/O, no codecs.
//! The plan (`.taskmaster/plans/79-video-playback-tier2.md`, rev2) is the spec; the
//! decisions encoded here are locked there (rebuffer-don't-drift, byte-budgeted 2-3-frame
//! queue, MKV-visible-with-error → one cross-platform container list, path-only video
//! items typed *before* any `source.bytes()`).

use std::path::Path;
use std::time::Duration;

pub use pb_decode::video::{
    SeekGeneration, VideoColorInfo, VideoFrame, VideoProducerEvent, VideoProducerMsg,
    VideoSessionId,
};
use pb_source::PhotoSource;

// ---------------------------------------------------------------------------
// Item model: typed classification, dispatched before any bytes() request.
// ---------------------------------------------------------------------------

/// What a scanned library entry *is* — decided at scan time from the path, carried on
/// the item, and dispatched on **before** any `PhotoSource::bytes()` call. Today the
/// engine reads bytes unconditionally (`engine.rs::decode_item`); video items are
/// path-only (the platform readers stream from disk), so the type — not a convention —
/// keeps whole-file RAM reads off the video path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryItemKind {
    /// A still (or animated) image: the existing decode path, bytes-in-RAM.
    Image,
    /// A filesystem video, identified by container. Always path-backed — archive
    /// entries are never classified as video (the archive predicate excludes them).
    Video(VideoContainer),
}

/// The **one cross-platform recognition list** of video containers (locked decision:
/// unsupported containers are *visible* items with a placeholder poster and a useful
/// error on play — per-file capability is a runtime property, the poster attempt is
/// the probe). Recognition ≠ playability: MKV is recognized everywhere and may only
/// play where the OS has a handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoContainer {
    /// .mp4 / .m4v
    Mp4,
    /// .mov (QuickTime) — also the Live Photo companion container; companion dedup
    /// (same-stem rule) happens at scan before classification matters.
    Mov,
    /// .mkv — Windows 10+ ships a native MF byte-stream handler (phase-0 spike
    /// verifies real coverage); macOS has none.
    Mkv,
    /// .webm — plays on Windows only with the Web Media Extensions.
    Webm,
    /// .avi
    Avi,
    /// .wmv / .asf
    Wmv,
    /// .mpg / .mpeg (MPEG-1/2 program streams)
    Mpeg,
    /// .mts / .m2ts (AVCHD camcorders)
    Avchd,
    /// .3gp / .3g2 (old phones)
    ThreeGp,
}

impl VideoContainer {
    /// Classify a file extension (no dot, any case) as a recognized video container.
    /// Deliberately separate from `pb_decode::is_supported_extension` (images): the
    /// archive scanners keep receiving the images-only predicate, so videos inside
    /// ZIP/7z are never indexed (path-only invariant).
    pub fn from_extension(ext: &str) -> Option<VideoContainer> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "mp4" | "m4v" => VideoContainer::Mp4,
            "mov" | "qt" => VideoContainer::Mov,
            "mkv" => VideoContainer::Mkv,
            "webm" => VideoContainer::Webm,
            "avi" => VideoContainer::Avi,
            "wmv" | "asf" => VideoContainer::Wmv,
            "mpg" | "mpeg" => VideoContainer::Mpeg,
            "mts" | "m2ts" => VideoContainer::Avchd,
            "3gp" | "3g2" => VideoContainer::ThreeGp,
            _ => return None,
        })
    }

    /// Display name for HUD/panel/error copy.
    pub fn name(self) -> &'static str {
        match self {
            VideoContainer::Mp4 => "MP4",
            VideoContainer::Mov => "QuickTime",
            VideoContainer::Mkv => "MKV",
            VideoContainer::Webm => "WebM",
            VideoContainer::Avi => "AVI",
            VideoContainer::Wmv => "WMV",
            VideoContainer::Mpeg => "MPEG",
            VideoContainer::Avchd => "AVCHD",
            VideoContainer::ThreeGp => "3GP",
        }
    }
}

/// Classify a filesystem path as a library item — the **scanner's** predicate (phase 1
/// predicate split): images + loose videos. `None` = not a library file (skip it).
/// Archive scanners never use this; they keep the images-only
/// `pb_decode::is_supported_extension`, so a video inside a ZIP/7z is never indexed
/// (video items are path-only by construction).
pub fn classify_library_file(path: &Path) -> Option<LibraryItemKind> {
    let ext = path.extension()?.to_str()?;
    if pb_decode::is_supported_extension(ext) {
        return Some(LibraryItemKind::Image);
    }
    VideoContainer::from_extension(ext).map(LibraryItemKind::Video)
}

/// What item `item` of `source` *is* — the dispatch the decode scheduler runs **before**
/// any `source.bytes()` request. O(1) and pure: video is decided by the path's extension
/// (the same rule the scanner admitted it under). Archive entries have no filesystem
/// path and the archive predicate is images-only, so they are always `Image`.
pub fn item_kind(source: &dyn PhotoSource, item: usize) -> LibraryItemKind {
    source
        .path(item)
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .and_then(VideoContainer::from_extension)
        .map(LibraryItemKind::Video)
        .unwrap_or(LibraryItemKind::Image)
}

/// Whether `path` is a Live-Photo *companion candidate* — a `.mov`/`.qt` video, the only
/// container Apple pairs with a still (`IMG_1234.HEIC` + `IMG_1234.MOV`). The scan-side
/// dedup hides exactly these when a same-stem image sits in the same directory; an
/// `IMG_1234.mp4` next to `IMG_1234.jpg` is *not* a companion and stays visible.
pub fn is_companion_candidate(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mov") || e.eq_ignore_ascii_case("qt"))
}

/// The pairing key for companion dedup: (parent directory, lowercased stem). Same-stem
/// matching is per-directory — recursive scans never pair across folders.
pub fn companion_key(path: &Path) -> Option<(std::path::PathBuf, String)> {
    let parent = path.parent()?.to_path_buf();
    let stem = path.file_stem()?.to_str()?.to_lowercase();
    Some((parent, stem))
}

/// Everything the UI knows about a video before (and without) playing it. Flows from
/// the platform reader at poster time — **never** from a whole-file RAM read; every
/// field the reader can't cheaply report stays `None`/default. Supplements
/// `PhotoMeta.animated` rather than forcing video through `Option<AnimationKind>`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoMetadata {
    /// Media duration, when the container reports one (VFR/streams may not).
    pub duration: Option<Duration>,
    pub container: Option<VideoContainer>,
    /// Human codec name as the reader reports it (e.g. "H.264", "HEVC").
    pub codec: Option<String>,
    /// Source pixel dimensions (pre-rotation), when known.
    pub dims: Option<(u32, u32)>,
    /// Display rotation in degrees CW the reader says the container requests
    /// (0/90/180/270). Platform paths that pre-rotate (MF advanced processing)
    /// report 0 here — the field describes what the *consumer* must still apply.
    pub rotation: u16,
    /// Whether the container has an audio track. `None` = not probed yet.
    pub has_audio: Option<bool>,
}

/// A media duration for the panel/HUD: `m:ss` under an hour, `h:mm:ss` above
/// (`0:07`, `3:05`, `1:02:07`). Sub-second clips round to `0:01` so a real clip
/// never reads as empty.
pub fn format_video_duration(d: Duration) -> String {
    let total = d
        .as_secs_f64()
        .round()
        .max(if d.is_zero() { 0.0 } else { 1.0 }) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

// ---------------------------------------------------------------------------
// Session state machine: states + legal transitions (pure rules, tested).
// ---------------------------------------------------------------------------

/// The `VideoSession` lifecycle (plan rev2):
/// `Opening → Buffering → Playing ⇄ Paused → Seeking → Buffering → … → Ended`,
/// with `Failed` and `Stopped` terminal. A separate machine from `Playback`
/// (Live Photos / GIF / avis stay untouched).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoSessionState {
    /// Reader/producer being created; nothing decoded yet.
    Opening,
    /// Filling the preroll (2 frames + audio ready-or-absent) — initial fill,
    /// post-seek refill, and mid-play rebuffer all land here.
    Buffering,
    Playing,
    Paused,
    /// A seek target is being landed (flush → reposition → decode-forward).
    Seeking,
    /// EOS reached: the true last frame stays parked; replay starts a fresh landing
    /// at t=0.
    Ended,
    /// Terminal: producer/audio failure surfaced to the user (no audio ≠ Failed —
    /// audio-absent playback is silent, not dead).
    Failed,
    /// Terminal: torn down (navigation, explicit stop). Resources retire in the
    /// background; a new play is a new session with a fresh id.
    Stopped,
}

impl VideoSessionState {
    /// No further transitions leave these states — a new session id is the only exit.
    pub fn is_terminal(self) -> bool {
        matches!(self, VideoSessionState::Failed | VideoSessionState::Stopped)
    }

    /// The legal transition relation. Everything not listed is a contract violation
    /// (phase 4's session asserts on it in debug).
    pub fn can_transition_to(self, to: VideoSessionState) -> bool {
        use VideoSessionState::*;
        // Failure and teardown can interrupt any non-terminal state.
        if self.is_terminal() {
            return false;
        }
        if matches!(to, Failed | Stopped) {
            return true;
        }
        matches!(
            (self, to),
            // Preroll after open; an empty/instant EOS stream can end from Buffering.
            (Opening, Buffering)
                | (Buffering, Playing)
                | (Buffering, Paused)   // seek-while-paused refills, then stays paused
                | (Buffering, Ended)
                | (Buffering, Seeking)  // a held scrub supersedes mid-fill (latest-value)
                | (Playing, Paused)
                | (Playing, Seeking)
                | (Playing, Buffering)  // rebuffer-don't-drift underrun
                | (Playing, Ended)
                | (Paused, Playing)
                | (Paused, Seeking)
                | (Seeking, Buffering)
                | (Ended, Seeking) // replay = land t=0 through the seek path
        )
    }
}

// ---------------------------------------------------------------------------
// Audio clock return path (new core⇄shell contract; wiring lands in phase 5).
// ---------------------------------------------------------------------------

/// What the platform audio player is doing, from the session's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioClockState {
    Opening,
    Playing,
    Paused,
    Buffering,
    Ended,
    /// Audio is broken (missing codec, device loss). Playback continues silently on
    /// the monotonic clock.
    Failed,
    /// The item has no audio track — expected, not an error.
    Absent,
}

/// One sample of the audio clock, flowing shell → core ~4×/s. Master clock when
/// audio is present and playing; between samples the session extrapolates via
/// monotonic time with small bounded corrections, and **hard re-anchors** (never
/// smooths) on pause/seek/rebuffer.
///
/// `sampled_at_monotonic` is the shell's monotonic timestamp for when `position` was
/// read, as a duration from an epoch both sides share (the session's start instant) —
/// a plain `Duration` keeps the contract `Instant`-free and testable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioClockSample {
    /// Guards against a straggler sample from a replaced player re-anchoring a new
    /// session.
    pub session_id: VideoSessionId,
    pub state: AudioClockState,
    /// Media position of the audio track at the sample instant.
    pub position: Duration,
    /// Monotonic time the position was read, relative to the session clock epoch.
    pub sampled_at_monotonic: Duration,
}

// ---------------------------------------------------------------------------
// Byte-budgeted queue admission (locked decision: bytes, not frames).
// ---------------------------------------------------------------------------

/// Hard frame-count ceiling on decoded frames **queued plus in flight**: the
/// session's accounting charges granted credits against this cap too (stricter
/// than the plan's queue-only invariant), so 4 = the plan's 2-3 queued frames of
/// lookahead + the one the producer is decoding against a credit.
pub const VIDEO_QUEUE_MAX_FRAMES: usize = 4;

/// The byte budget for queued + in-flight decoded frames: 3× one fitted 4K RGBA8
/// frame (~95 MiB, constant). Sized so a 4K60 stream keeps 2-3 frames of real
/// lookahead (~50 ms — enough to ride the measured 30-100 ms GOP-boundary decode
/// spikes) *after* charging the in-flight credit; anything smaller serializes the
/// credit round-trip and playback runs below real time (the owner-reported 2/3×
/// on a 4K60 HEVC clip). Admission still enforces the one-frame exception when a
/// single fitted frame exceeds the whole budget.
pub const VIDEO_QUEUE_BYTE_BUDGET: u64 = 3 * 3840 * 2160 * 4;

/// Pure admission rule for the producer→session frame queue. The session owns one;
/// the producer receives *credits* only when `admits` says the next frame fits, so
/// backpressure is demand-driven and a full queue can never deafen the command
/// channel.
///
/// Invariant (unit-tested here, structurally re-asserted by the phase-4 queue):
/// `queued_bytes ≤ max(budget, largest_single_frame) ∧ queued_frames ≤ 3`.
#[derive(Debug, Clone, Copy)]
pub struct VideoQueueBudget {
    pub max_bytes: u64,
    pub max_frames: usize,
}

impl Default for VideoQueueBudget {
    fn default() -> Self {
        VideoQueueBudget {
            max_bytes: VIDEO_QUEUE_BYTE_BUDGET,
            max_frames: VIDEO_QUEUE_MAX_FRAMES,
        }
    }
}

impl VideoQueueBudget {
    /// May a frame of `frame_bytes` join a queue currently holding `queued_frames`
    /// frames totalling `queued_bytes`?
    ///
    /// - The frame cap always binds.
    /// - Within the byte budget: admit while `queued_bytes + frame_bytes ≤ max_bytes`.
    /// - **One-frame exception:** a single frame larger than the whole budget is
    ///   admitted only into an *empty* queue (playback must not wedge on 8K video),
    ///   and nothing more is admitted behind it until it drains.
    pub fn admits(&self, queued_bytes: u64, queued_frames: usize, frame_bytes: u64) -> bool {
        if queued_frames >= self.max_frames {
            return false;
        }
        if frame_bytes > self.max_bytes {
            return queued_frames == 0;
        }
        queued_bytes.saturating_add(frame_bytes) <= self.max_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use VideoSessionState::*;

    // --- item model -------------------------------------------------------

    #[test]
    fn container_classification_covers_the_locked_list_and_nothing_else() {
        assert_eq!(
            VideoContainer::from_extension("MP4"),
            Some(VideoContainer::Mp4)
        );
        assert_eq!(
            VideoContainer::from_extension("m4v"),
            Some(VideoContainer::Mp4)
        );
        assert_eq!(
            VideoContainer::from_extension("MoV"),
            Some(VideoContainer::Mov)
        );
        assert_eq!(
            VideoContainer::from_extension("mkv"),
            Some(VideoContainer::Mkv)
        );
        assert_eq!(
            VideoContainer::from_extension("webm"),
            Some(VideoContainer::Webm)
        );
        assert_eq!(
            VideoContainer::from_extension("m2ts"),
            Some(VideoContainer::Avchd)
        );
        assert_eq!(
            VideoContainer::from_extension("3gp"),
            Some(VideoContainer::ThreeGp)
        );
        // Images, junk, and empty never classify as video.
        for not_video in ["jpg", "heic", "avif", "gif", "txt", "", "mp3", "wav"] {
            assert_eq!(
                VideoContainer::from_extension(not_video),
                None,
                "{not_video}"
            );
        }
    }

    #[test]
    fn video_and_image_extension_sets_are_disjoint() {
        // The predicate split's core guarantee: nothing is both an image (archive-
        // indexable, bytes-decoded) and a video (path-only).
        for ext in [
            "mp4", "m4v", "mov", "qt", "mkv", "webm", "avi", "wmv", "asf", "mpg", "mpeg", "mts",
            "m2ts", "3gp", "3g2",
        ] {
            assert!(
                VideoContainer::from_extension(ext).is_some(),
                "{ext} should classify"
            );
            assert!(
                !pb_decode::is_supported_extension(ext),
                "{ext} must not be an image extension"
            );
        }
    }

    #[test]
    fn classify_library_file_splits_images_videos_and_junk() {
        use std::path::PathBuf;
        assert_eq!(
            classify_library_file(&PathBuf::from(r"C:\pics\a.JPG")),
            Some(LibraryItemKind::Image)
        );
        assert_eq!(
            classify_library_file(&PathBuf::from(r"C:\pics\a.heic")),
            Some(LibraryItemKind::Image)
        );
        assert_eq!(
            classify_library_file(&PathBuf::from(r"C:\pics\clip.MP4")),
            Some(LibraryItemKind::Video(VideoContainer::Mp4))
        );
        assert_eq!(
            classify_library_file(&PathBuf::from(r"C:\pics\clip.mkv")),
            Some(LibraryItemKind::Video(VideoContainer::Mkv))
        );
        assert_eq!(
            classify_library_file(&PathBuf::from(r"C:\pics\note.txt")),
            None
        );
        assert_eq!(
            classify_library_file(&PathBuf::from(r"C:\pics\noext")),
            None
        );
    }

    #[test]
    fn item_kind_types_fs_videos_and_never_archive_entries() {
        use pb_source::FsSource;
        use std::path::PathBuf;
        // Paths need not exist — classification is pure (no I/O before dispatch).
        let src = FsSource::new(vec![
            PathBuf::from(r"C:\nope\a.jpg"),
            PathBuf::from(r"C:\nope\b.mov"),
            PathBuf::from(r"C:\nope\c.webm"),
        ]);
        assert_eq!(item_kind(&src, 0), LibraryItemKind::Image);
        assert_eq!(
            item_kind(&src, 1),
            LibraryItemKind::Video(VideoContainer::Mov)
        );
        assert_eq!(
            item_kind(&src, 2),
            LibraryItemKind::Video(VideoContainer::Webm)
        );
        // Out of range degrades to Image (no path), matching the bytes-decode fallback.
        assert_eq!(item_kind(&src, 99), LibraryItemKind::Image);
    }

    #[test]
    fn companion_candidates_are_mov_qt_only_and_keys_are_per_directory() {
        use std::path::PathBuf;
        assert!(is_companion_candidate(&PathBuf::from(r"D:\p\IMG_1.MOV")));
        assert!(is_companion_candidate(&PathBuf::from(r"D:\p\IMG_1.qt")));
        assert!(!is_companion_candidate(&PathBuf::from(r"D:\p\IMG_1.mp4")));
        assert!(!is_companion_candidate(&PathBuf::from(r"D:\p\IMG_1.jpg")));
        let a = companion_key(&PathBuf::from(r"D:\p\IMG_1.MOV")).unwrap();
        let b = companion_key(&PathBuf::from(r"D:\p\img_1.HEIC")).unwrap();
        let c = companion_key(&PathBuf::from(r"D:\q\IMG_1.HEIC")).unwrap();
        assert_eq!(a, b, "case-insensitive stem, same dir");
        assert_ne!(a, c, "different directory never pairs");
    }

    #[test]
    fn durations_format_like_a_player() {
        use std::time::Duration as D;
        assert_eq!(format_video_duration(D::from_secs(0)), "0:00");
        assert_eq!(format_video_duration(D::from_millis(400)), "0:01");
        assert_eq!(format_video_duration(D::from_secs(7)), "0:07");
        assert_eq!(format_video_duration(D::from_secs(185)), "3:05");
        assert_eq!(format_video_duration(D::from_secs(3727)), "1:02:07");
    }

    #[test]
    fn metadata_defaults_leave_everything_unknown() {
        let m = VideoMetadata::default();
        assert_eq!(m.duration, None);
        assert_eq!(m.codec, None);
        assert_eq!(m.dims, None);
        assert_eq!(m.has_audio, None);
        assert_eq!(m.rotation, 0);
    }

    // --- state machine ----------------------------------------------------

    const ALL: [VideoSessionState; 8] = [
        Opening, Buffering, Playing, Paused, Seeking, Ended, Failed, Stopped,
    ];

    #[test]
    fn terminals_absorb() {
        for from in [Failed, Stopped] {
            for to in ALL {
                assert!(!from.can_transition_to(to), "{from:?} → {to:?}");
            }
        }
    }

    #[test]
    fn every_live_state_can_fail_or_stop() {
        for from in ALL.iter().filter(|s| !s.is_terminal()) {
            assert!(from.can_transition_to(Failed), "{from:?} → Failed");
            assert!(from.can_transition_to(Stopped), "{from:?} → Stopped");
        }
    }

    #[test]
    fn the_happy_path_is_legal() {
        // Open → preroll → play → pause → resume → seek → refill → play → end → replay.
        let path = [
            Opening, Buffering, Playing, Paused, Playing, Seeking, Buffering, Playing, Ended,
            Seeking, Buffering, Playing,
        ];
        for w in path.windows(2) {
            assert!(w[0].can_transition_to(w[1]), "{:?} → {:?}", w[0], w[1]);
        }
    }

    #[test]
    fn rebuffer_and_paused_seek_are_legal() {
        // Underrun: Playing → Buffering (freeze) → Playing (re-anchored resume).
        assert!(Playing.can_transition_to(Buffering));
        assert!(Buffering.can_transition_to(Playing));
        // Seek while paused: Paused → Seeking → Buffering → Paused (stays paused).
        assert!(Paused.can_transition_to(Seeking));
        assert!(Seeking.can_transition_to(Buffering));
        assert!(Buffering.can_transition_to(Paused));
    }

    #[test]
    fn nonsense_transitions_are_rejected() {
        assert!(
            !Opening.can_transition_to(Playing),
            "no play without preroll"
        );
        assert!(!Opening.can_transition_to(Seeking));
        assert!(!Playing.can_transition_to(Opening));
        assert!(
            !Ended.can_transition_to(Playing),
            "replay lands via Seeking"
        );
        assert!(!Ended.can_transition_to(Buffering));
        assert!(
            !Paused.can_transition_to(Buffering),
            "paused holds its frame"
        );
        assert!(!Seeking.can_transition_to(Playing), "a seek refills first");
        assert!(
            Buffering.can_transition_to(Seeking),
            "a held scrub supersedes mid-fill (latest-value)"
        );
        for s in ALL {
            assert!(!s.can_transition_to(s), "no self-loops: {s:?}");
        }
    }

    // --- byte budget --------------------------------------------------------

    /// One fitted 4K RGBA8 frame.
    const FRAME_4K: u64 = 3840 * 2160 * 4;

    #[test]
    fn budget_is_three_fitted_4k_frames() {
        assert_eq!(VIDEO_QUEUE_BYTE_BUDGET, 3 * FRAME_4K);
        // ~95 MiB — constant and documented (queue + in-flight, see the consts).
        let b = VideoQueueBudget::default();
        assert!(b.max_bytes < 100 * 1024 * 1024);
        assert_eq!(b.max_frames, VIDEO_QUEUE_MAX_FRAMES);
    }

    #[test]
    fn admits_within_budget_up_to_the_frame_cap() {
        let b = VideoQueueBudget::default();
        // A small (1080p) frame: byte budget allows many, the frame cap binds at 4.
        let f = 1920 * 1080 * 4u64;
        assert!(b.admits(0, 0, f));
        assert!(b.admits(f, 1, f));
        assert!(b.admits(2 * f, 2, f));
        assert!(b.admits(3 * f, 3, f));
        assert!(!b.admits(4 * f, 4, f), "frame cap");
    }

    #[test]
    fn byte_budget_binds_for_4k() {
        let b = VideoQueueBudget::default();
        assert!(b.admits(0, 0, FRAME_4K));
        assert!(b.admits(FRAME_4K, 1, FRAME_4K));
        assert!(b.admits(2 * FRAME_4K, 2, FRAME_4K), "exactly at budget");
        assert!(
            !b.admits(3 * FRAME_4K, 3, FRAME_4K),
            "fourth 4K frame exceeds bytes before the frame cap"
        );
    }

    #[test]
    fn oversized_single_frame_only_enters_an_empty_queue() {
        let b = VideoQueueBudget::default();
        let f8k = 7680 * 4320 * 4u64; // one 8K frame > the whole budget
        assert!(f8k > b.max_bytes);
        assert!(b.admits(0, 0, f8k), "one-frame exception");
        assert!(!b.admits(f8k, 1, f8k), "never two oversized frames");
        assert!(
            !b.admits(f8k, 1, 1024),
            "nothing rides behind an oversized frame"
        );
    }

    #[test]
    fn admission_preserves_the_invariant_for_any_admitted_sequence() {
        // Structural property check: replay random-ish admit/pop traffic and assert
        // the invariant after every admitted frame. Deterministic pattern (no RNG on
        // purpose — reproducible), mixing tiny, 4K, and oversized frames.
        let b = VideoQueueBudget::default();
        let sizes = [
            1024u64,
            FRAME_4K,
            7680 * 4320 * 4,
            FRAME_4K / 2,
            333,
            FRAME_4K,
        ];
        let mut queue: Vec<u64> = Vec::new();
        for step in 0..1000usize {
            let f = sizes[step % sizes.len()];
            let bytes: u64 = queue.iter().sum();
            if b.admits(bytes, queue.len(), f) {
                queue.push(f);
                let now: u64 = queue.iter().sum();
                let largest = *queue.iter().max().unwrap();
                assert!(
                    now <= b.max_bytes.max(largest),
                    "byte invariant broke at step {step}: {now}"
                );
                assert!(queue.len() <= b.max_frames, "frame invariant at {step}");
            }
            // Drain one frame every other step (consumer pace).
            if step % 2 == 1 && !queue.is_empty() {
                queue.remove(0);
            }
        }
    }
}
