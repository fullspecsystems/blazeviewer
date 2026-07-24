//! **Item kind and its affordances** — is this a photo, a video, a live photo, or a door?
//! (task #125.)
//!
//! A *topic*, and a load-bearing one: these are the `AppCore` methods that classify a
//! `LibraryItemKind` and decide what the viewer offers for it — the play-hint pill, the door
//! card, the archive kind.
//!
//! ⚠ **A new `LibraryItemKind` must opt OUT of byte reads, not into them.** Guards written
//! `!matches!(.., Video(_))` silently drop a new kind into the *image* bucket, which is how
//! the thumb strip and the info panel would each `fs::read` every archive in a folder. Read
//! guards are positive and kind matches are exhaustive so the compiler lists the sites — but
//! only *per platform*, so a `cfg(macos)` route stays invisible from Windows. The full rule
//! is in `crates/pb-app-core/CLAUDE.md`.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Whether item `item` is a Live Photo, from the pairing cache (populated when the
    /// info panel opens / on dwell). A `&self` read — never triggers a stat — so it's
    /// safe from the render/rows path; the `&mut` [`live_motion_path`](App::live_motion_path)
    /// is what fills the cache.
    pub fn is_live_photo(&self, item: usize) -> bool {
        self.live_motion_cache
            .get(&item)
            .is_some_and(|paired| paired.is_some())
    }

    /// The native play-hint kind for the current item: `0` = none (a still, or already
    /// playing — the hint's job is done), `1` = Live Photo (the livephoto mark), `2` = another
    /// animation (play ▶). An archive door has **no pill** — its affordance is the door card
    /// (task #105), which is the only thing on screen for it. Stays consistent with
    /// `has_motion` (which bumps `play_hint_seq`): a fresh motion item is a Live Photo (→1) or
    /// has an `animated` container (→2).
    pub fn play_hint_kind(&self) -> u8 {
        if self.playback.is_some() {
            return 0; // engaged — no hint while it plays/pauses
        }
        let Some(item) = self.displayed_item else {
            return 0;
        };
        if self.is_live_photo(item) {
            1
        } else if self.current.as_ref().is_some_and(|m| m.animated.is_some())
            || self.item_is_video(item)
        {
            2
        } else {
            0
        }
    }

    /// Whether item `item` is a video (task #79) — typed off the path, no I/O.
    pub fn item_is_video(&self, item: usize) -> bool {
        matches!(
            crate::video::item_kind(self.source.as_ref(), item),
            crate::video::LibraryItemKind::Video(_)
        )
    }

    /// Whether an archive **door** is on screen right now — the cheap predicate the
    /// shells poll each frame to gate their overlay and spot a change.
    ///
    /// Allocation-free, unlike [`door_card`](Self::door_card), which builds Strings: a
    /// per-frame visibility gate must not allocate.
    pub fn door_presented(&self) -> bool {
        // Gate on the frame being **actually on screen** at the current epoch, not merely named:
        // `rebuild_playlist` sets `displayed_item` to the new current index with
        // `presented_epoch = None` (nothing presented yet — the renderer still holds the old
        // frame). Without this check the door card would flash over that held photo the instant a
        // door becomes the current item, before its own (transparent) frame is presented — the
        // owner-reported "card on top of a photo" (and the archive-open card-with-no-image).
        self.presented_epoch == Some(self.epoch)
            && self
                .displayed_item
                .is_some_and(|i| self.item_archive_kind(i).is_some())
    }

    /// The **door card** to draw over the letterbox, or `None` when the presented item
    /// isn't a door (task #105).
    ///
    /// A door's frame is a 1×1 transparent sentinel — it draws nothing — so this card is
    /// the entire on-screen presence of an archive: its artwork, its name, and the key
    /// that opens it. The shells snapshot it into their panel frame and render it as
    /// chrome, which is what a door is.
    ///
    /// Keyed off `displayed_item` — the item **actually on screen** — never the playlist
    /// cursor, or the card would name an archive the viewer isn't looking at yet. Pure:
    /// no I/O, safe on the frame path.
    pub fn door_card(&self) -> Option<crate::app_core::DoorCard> {
        // Only once the door's own frame is actually presented (see `door_presented`) — never over
        // a still-held previous photo during a deck rebuild.
        if !self.door_presented() {
            return None;
        }
        let item = self.displayed_item?;
        let kind = self.item_archive_kind(item)?;
        Some(crate::app_core::DoorCard {
            name: self
                .source
                .path(item)
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.source.name(item).to_string()),
            format: format!("{} Archive", kind.name()),
            shortcut: self.shortcut_for(Action::PlayPause),
        })
    }

    /// The format of item `item` if it is an archive **door** (task #104), else
    /// `None` — typed off the path, no I/O. A door is an archive sitting on disk
    /// that the viewer can enter with `P`; an archive *entry* is never one, so
    /// this answers `None` inside an open archive.
    pub fn item_archive_kind(&self, item: usize) -> Option<pb_source::ArchiveKind> {
        match crate::video::item_kind(self.source.as_ref(), item) {
            crate::video::LibraryItemKind::Archive(kind) => Some(kind),
            crate::video::LibraryItemKind::Image | crate::video::LibraryItemKind::Video(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{test_core, FakeArchive};

    /// pool-decoded poster lands at the launch epoch, MUST become resident AND be
    /// presented — exactly as a photo does. Reproduces the launch state
    /// `rebuild_playlist` leaves (displayed==target, presented_epoch=None).
    #[test]
    fn initial_video_poster_presents_when_it_lands() {
        let mut core = test_core();
        let root = PathBuf::from("videos");
        core.source = Arc::new(FsSource::new(vec![root.join("clip.mkv")]));
        core.playlist = Playlist::new(1, 0);
        core.ring = ResidentRing::new(4);
        core.displayed_item = Some(0);
        core.target_item = Some(0);
        core.presented_epoch = None;
        core.targets = vec![0];
        assert!(core.item_is_video(0), "clip.mkv is a video item");
        let poster = pb_decode::DecodedImage {
            width: 64,
            height: 64,
            orig_width: 64,
            orig_height: 64,
            codec: "HEVC",
            format: pb_decode::PixelFormat::Rgba8,
            pixels: vec![9; 64 * 64 * 4],
            is_preview: false,
            color: pb_decode::ColorTransform::srgb(),
            peak: 1.0,
            animated: None,
            recovered: None,
        };
        core.pending_uploads.push(Outcome::synthetic(
            0,
            core.epoch,
            core.content_gen,
            pb_core::RepKind::Original,
            Ok(poster),
        ));
        core.drain_results();
        assert!(core.display_slot(0).is_some(), "the poster became resident");
        assert_eq!(
            core.presented_epoch,
            Some(core.epoch),
            "the poster was PRESENTED at launch (not left resident-but-unpresented)"
        );
    }

    /// A disk deck of `photo.jpg` + `album.zip` in `dir`, cursor on the door.
    fn core_on_a_door(dir: &Path) -> AppCore {
        let mut core = test_core();
        let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![
            dir.join("photo.jpg"),
            dir.join("album.zip"),
        ]));
        core.rebuild_playlist(src, dir.to_path_buf(), Some(dir.to_path_buf()), false, 0);
        core.displayed_item = Some(1); // the door
        core.presented_epoch = Some(core.epoch); // its frame is on screen (see `door_presented`)
        core
    }

    /// The door's only read. `P` on an archive emits exactly one archive open, with
    /// no password on the first attempt (the shell's failure path prompts and
    /// re-opens with `Some`, which is why guessing here would be wrong).
    #[test]
    fn p_on_a_door_opens_the_archive_and_nothing_else() {
        let dir = std::env::temp_dir().join("pb_door_enter");
        let mut core = core_on_a_door(&dir);
        core.effects.clear();

        core.toggle_play_pause();

        let opens: Vec<_> = core
            .effects
            .iter()
            .filter_map(|e| match e {
                contract::CoreEffect::BeginArchiveOpen { path, password } => {
                    Some((path.clone(), password.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            opens,
            vec![(dir.join("album.zip"), None)],
            "exactly one open, un-passworded"
        );
        assert!(
            core.playback.is_none(),
            "a door never starts the animation machinery"
        );
    }

    /// Entering is an open like any other, so it ends an Open-Parent climb — else
    /// the next `Alt+Up` would resume from a stale rung and jump somewhere absurd.
    #[test]
    fn entering_a_door_ends_a_climb() {
        let dir = std::env::temp_dir().join("pb_door_climb");
        let mut core = core_on_a_door(&dir);
        core.climb_anchor = Some(dir.join("somewhere/else"));

        core.toggle_play_pause();

        assert_eq!(core.climb_anchor, None);
    }

    /// `P` keeps its existing meanings — the door arm must not shadow a photo.
    #[test]
    fn p_on_a_photo_is_unaffected_by_the_door_arm() {
        let dir = std::env::temp_dir().join("pb_door_photo");
        let mut core = core_on_a_door(&dir);
        core.displayed_item = Some(0); // the .jpg
        core.effects.clear();

        core.toggle_play_pause();

        assert!(
            !core
                .effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::BeginArchiveOpen { .. })),
            "a photo must never open an archive"
        );
    }

    /// The card is a door's entire on-screen presence (its frame draws nothing), so it
    /// must carry the name, the format, and the **live** shortcut — and appear only for
    /// a door.
    #[test]
    fn door_card_describes_the_presented_door_only() {
        let dir = std::env::temp_dir().join("pb_door_card");
        let mut core = core_on_a_door(&dir);

        let card = core.door_card().expect("a door presents a card");
        assert_eq!(card.name, "album.zip", "the file name, not the full path");
        assert_eq!(card.format, "ZIP Archive", "Title Case, like every heading");
        assert!(
            !card.shortcut.is_empty(),
            "from the live keymap, not hard-coded"
        );

        // A photo has no card — otherwise it would float over the picture.
        core.displayed_item = Some(0);
        assert!(core.door_card().is_none());
    }

    /// Keyed off the item **on screen**, never the playlist cursor: naming an archive the
    /// viewer is not looking at yet would be worse than naming none.
    #[test]
    fn door_card_follows_the_presented_item_not_the_cursor() {
        let dir = std::env::temp_dir().join("pb_door_card_cursor");
        let mut core = core_on_a_door(&dir);
        core.target_item = Some(0); // the cursor moved to the photo…
        assert!(
            core.door_card().is_some(),
            "…but the door is still on screen, so its card stays"
        );
        core.displayed_item = None;
        assert!(core.door_card().is_none(), "nothing presented, no card");
    }

    /// A door gets **no** play pill: its affordance is the door card (task #105), which is
    /// the only thing on screen for it. Regression bar for the kind-3 borrow that the card
    /// replaced — re-adding it would put a zip button under the card's own button.
    #[test]
    fn a_door_has_no_play_pill() {
        let dir = std::env::temp_dir().join("pb_door_hint");
        let mut core = core_on_a_door(&dir);
        assert_eq!(
            core.play_hint_kind(),
            0,
            "the card is the affordance, not a pill"
        );

        // …and settling on one arms nothing.
        let before = core.play_hint_seq;
        core.maybe_show_anim_hint(false);
        assert_eq!(core.play_hint_seq, before);
    }

    /// **The loop the feature promises**: enter a door, then climb back out to the
    /// folder of doors so the next one is a keypress away. The climb half already
    /// worked (`open_parent_cmd` anchors on the source's container); this pins the
    /// two halves together, which is what a viewer actually does.
    #[test]
    fn enter_a_door_then_climb_back_out_to_the_folder_of_doors() {
        let dir = std::env::temp_dir().join("pb_door_loop");
        let mut core = core_on_a_door(&dir);

        // 1. P enters album.zip.
        core.effects.clear();
        core.toggle_play_pause();
        assert!(core
            .effects
            .iter()
            .any(|e| matches!(e, contract::CoreEffect::BeginArchiveOpen { path, .. } if *path == dir.join("album.zip"))));

        // 2. The archive deck lands (what the shell feeds back as ArchiveResolved).
        let entries: Arc<dyn ItemSource> = Arc::new(FakeArchive {
            names: vec!["1.jpg".to_string(), "2.jpg".to_string()],
            container: dir.join("album.zip"),
        });
        core.apply_archive(crate::scan::Resolved {
            root: dir.join("album.zip"),
            scan_root: None,
            recursive: false,
            source: entries,
            start: 0,
        });
        assert_eq!(core.source.len(), 2, "viewing inside the archive");

        // 3. Alt+Up climbs out to the folder that holds the archive — the folder of
        //    doors, which is where the next door is.
        core.effects.clear();
        core.open_parent_cmd();
        let scanned = core.effects.iter().rev().find_map(|e| match e {
            contract::CoreEffect::BeginDirScan {
                source: pb_core::open::Source::Scan { roots, .. },
                ..
            } => roots.first().cloned(),
            _ => None,
        });
        assert_eq!(
            scanned,
            Some(dir),
            "climbing out of an archive lands on its folder"
        );
    }
    /// Task #79 phase 4: `P` on a video item starts a `VideoSession` — never the
    /// animation decode machinery (which would read the file into RAM). A producer
    /// that can't open the file fails the session cleanly through `poll_video`,
    /// which surfaces a toast and clears the session.
    #[test]
    fn p_on_a_video_item_starts_a_session_never_the_animation_machinery() {
        let mut core = test_core();
        core.source = Arc::new(pb_source::FsSource::new(vec![std::path::PathBuf::from(
            r"C:\nope\clip.mp4",
        )]));
        core.displayed_item = Some(0);
        core.native_toast = true; // headless has no HUD raster; the native path retains text
        assert!(core.item_is_video(0));
        assert_eq!(core.play_hint_kind(), 2, "video badge is the play glyph");

        core.toggle_play_pause();
        assert!(
            core.playback.is_none(),
            "video never uses the animation playback"
        );
        assert!(core.anim_decode.is_none(), "no batch decode kicked");
        assert!(core.anim_stream.is_none(), "no stream kicked");
        // Session platforms: Windows (MF) and Linux with the FFmpeg producer
        // (task #84) — same protocol, same failure contract.
        #[cfg(any(windows, all(unix, not(target_os = "macos"), feature = "ffvideo")))]
        {
            assert!(core.video.is_some(), "P starts the video session");
            // The missing file fails the producer; the session surfaces it via
            // poll (bounded wait — the producer thread races this assert).
            let deadline = Instant::now() + Duration::from_secs(10);
            while core.video.is_some() && Instant::now() < deadline {
                core.now = Instant::now();
                core.poll_video();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(core.video.is_none(), "failure clears the session");
            assert!(
                core.toast_native.is_some(),
                "the failure surfaces to the user"
            );
        }
        // macOS (task 79.9): `P` starts a `Native` backend and commands the shell's
        // AVPlayer via `PlayVideo` — no Rust producer/session.
        #[cfg(target_os = "macos")]
        {
            assert!(core.video.is_some(), "P starts a native video session");
            assert!(
                core.video.as_ref().unwrap().as_native().is_some(),
                "macOS uses the Native backend, not a VideoSession"
            );
            assert!(
                core.effects
                    .iter()
                    .any(|e| matches!(e, contract::CoreEffect::PlayVideo { .. })),
                "the native player is commanded to open the clip"
            );
        }
        #[cfg(not(any(windows, target_os = "macos", all(unix, feature = "ffvideo"))))]
        assert!(core.video.is_none(), "no producer on this platform yet");
    }

    /// Regression: the door card must NOT flash over a still-held previous frame during a deck
    /// rebuild. `rebuild_playlist` names the current item (a door) but leaves `presented_epoch`
    /// None — the renderer still holds the old photo — so `door_presented`/`door_card` must be
    /// false/None until the door's own (transparent) frame is actually presented. The
    /// owner-reported "card on top of a photo" (and the archive-open card-with-no-image).
    #[test]
    fn door_card_waits_until_the_door_frame_is_presented() {
        let dir = std::env::temp_dir().join("pb_door_card_wait");
        let mut core = test_core();
        let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(vec![
            dir.join("photo.jpg"),
            dir.join("album.zip"),
        ]));
        // Rebuild with the cursor on the door (start index 1): it is the current item, but nothing
        // is presented yet (the old frame is still held).
        core.rebuild_playlist(src, dir.to_path_buf(), Some(dir.to_path_buf()), false, 1);
        assert_eq!(core.displayed_item, Some(1), "the door is the current item");
        assert!(
            core.presented_epoch.is_none(),
            "a rebuild presents nothing yet"
        );
        assert!(
            !core.door_presented(),
            "no card while the previous frame is still held"
        );
        assert!(core.door_card().is_none());
        // Once the door's own frame is presented (its epoch resolves), the card appears.
        core.presented_epoch = Some(core.epoch);
        assert!(core.door_presented());
        assert!(core.door_card().is_some());
    }
}
