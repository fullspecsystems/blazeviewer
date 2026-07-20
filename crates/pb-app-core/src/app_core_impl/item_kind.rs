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
