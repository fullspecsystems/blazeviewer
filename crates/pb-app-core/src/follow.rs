//! Auto-follow state for the Thumbnails strip (task #83, plan §9): "keep the
//! current photo centered during nav; a manual scroll disengages; the next nav
//! keypress (or a strip click) re-engages." Pure and shell-neutral — both the
//! SwiftUI and egui strips drive the same machine, and the **generation token**
//! is what lets a shell distinguish its own programmatic scroll animation from
//! the user grabbing the list (SwiftUI has no native way to tell them apart).
//!
//! Protocol: when an event returns a [`ScrollTo`], the shell starts a scroll to
//! that item (smooth for short moves, snap for long ones — the shell knows its
//! rows-per-viewport; wraps and long jumps snap) and calls
//! [`FollowState::programmatic_done`] with the token when the animation lands.
//! Scroll movement the shell observes **while its own animation is live** is
//! not a user scroll; anything else is, and detaches.

/// A scroll command for the shell: center `item`, then report `gen` back via
/// [`FollowState::programmatic_done`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollTo {
    pub item: usize,
    pub gen: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Tracking the current photo; nav emits scroll commands.
    Following,
    /// The user scrolled away; the highlight moves but the list stays put
    /// until the next nav / jump / reopen re-engages.
    Detached,
    /// A follow scroll is animating; scroll noise it generates is not a user
    /// scroll. Lands back in `Following` when the shell reports `gen` done.
    Programmatic { gen: u64 },
}

/// See module docs. One per strip; lives in `AppCore`, driven over FFI/egui.
#[derive(Clone, Copy, Debug)]
pub struct FollowState {
    mode: Mode,
    next_gen: u64,
}

impl Default for FollowState {
    fn default() -> Self {
        FollowState {
            mode: Mode::Following,
            next_gen: 0,
        }
    }
}

impl FollowState {
    /// Whether the strip is (or is animating back to) following the current item.
    pub fn following(&self) -> bool {
        !matches!(self.mode, Mode::Detached)
    }

    fn issue(&mut self, item: usize) -> Option<ScrollTo> {
        self.next_gen += 1;
        self.mode = Mode::Programmatic { gen: self.next_gen };
        Some(ScrollTo {
            item,
            gen: self.next_gen,
        })
    }

    /// The panel just opened (or the deck was rebuilt under it): land centered
    /// on `current` regardless of prior detachment — an open is a fresh look.
    pub fn panel_opened(&mut self, current: usize) -> Option<ScrollTo> {
        self.issue(current)
    }

    /// A navigation advance (key nav, hold-to-fly, random, slideshow). Always
    /// follows — the owner rule is "the next nav re-engages", so this issues
    /// even from `Detached`.
    pub fn navigation(&mut self, current: usize) -> Option<ScrollTo> {
        self.issue(current)
    }

    /// An absolute jump the user asked for (a strip click, compare flip): always
    /// re-engages — the user pointed at where they want to be.
    pub fn jump(&mut self, current: usize) -> Option<ScrollTo> {
        self.issue(current)
    }

    /// Playlist indices changed under the strip (delete / rebuild): if following,
    /// re-center on the new current. While detached, stay put — a background
    /// delete must not yank the list out of the user's hands.
    pub fn playlist_mutated(&mut self, current: usize) -> Option<ScrollTo> {
        match self.mode {
            Mode::Detached => None,
            _ => self.issue(current),
        }
    }

    /// The shell observed scroll movement it did not itself animate. During a
    /// live programmatic scroll this is animation noise and is ignored; any
    /// other time it is the user grabbing the list → detach.
    pub fn user_scrolled(&mut self) {
        if !matches!(self.mode, Mode::Programmatic { .. }) {
            self.mode = Mode::Detached;
        }
    }

    /// The shell's programmatic scroll for `gen` finished. A stale generation
    /// (superseded by a newer command, or reported after a detach) is ignored.
    pub fn programmatic_done(&mut self, gen: u64) {
        if self.mode == (Mode::Programmatic { gen }) {
            self.mode = Mode::Following;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_follows_and_lands() {
        let mut f = FollowState::default();
        let cmd = f.navigation(5).expect("following → scroll");
        assert_eq!(cmd.item, 5);
        f.programmatic_done(cmd.gen);
        assert!(f.following());
        assert!(f.navigation(6).is_some(), "keeps following after landing");
    }

    #[test]
    fn user_scroll_detaches_and_next_nav_reengages() {
        let mut f = FollowState::default();
        let cmd = f.navigation(5).unwrap();
        f.programmatic_done(cmd.gen);
        f.user_scrolled();
        assert!(!f.following());
        // The owner rule: the very next nav keypress re-engages.
        let cmd = f.navigation(6).expect("nav re-engages from detached");
        assert_eq!(cmd.item, 6);
        f.programmatic_done(cmd.gen);
        assert!(f.following());
    }

    #[test]
    fn scroll_noise_during_animation_is_not_a_user_scroll() {
        let mut f = FollowState::default();
        let cmd = f.navigation(5).unwrap();
        f.user_scrolled(); // the animation moving the list
        assert!(f.following(), "still programmatic — not detached");
        f.programmatic_done(cmd.gen);
        assert!(f.following());
        f.user_scrolled(); // now it IS the user
        assert!(!f.following());
    }

    #[test]
    fn stale_generation_cannot_resurrect_following() {
        let mut f = FollowState::default();
        let old = f.navigation(5).unwrap();
        let new = f.navigation(6).unwrap(); // supersedes; old.gen is stale
        assert_ne!(old.gen, new.gen);
        // The superseded animation lands late — ignored.
        f.programmatic_done(old.gen);
        f.user_scrolled();
        assert!(
            f.following(),
            "new gen still live: scroll noise, not a user scroll"
        );
        f.programmatic_done(new.gen);
        assert!(f.following());
        // A landing reported after a real detach must not re-attach.
        let cmd = f.navigation(7).unwrap();
        f.programmatic_done(cmd.gen);
        f.user_scrolled();
        f.programmatic_done(cmd.gen); // duplicate/late report
        assert!(!f.following(), "stale landing can't undo the user's detach");
    }

    #[test]
    fn jump_reengages_from_detached() {
        let mut f = FollowState::default();
        f.user_scrolled();
        assert!(!f.following());
        let cmd = f.jump(42).expect("a click re-engages");
        assert_eq!(cmd.item, 42);
        f.programmatic_done(cmd.gen);
        assert!(f.following());
    }

    #[test]
    fn panel_open_and_playlist_mutation_recenter() {
        let mut f = FollowState::default();
        assert_eq!(f.panel_opened(9).unwrap().item, 9);
        // Mutation while following: recenter.
        let cmd = f.playlist_mutated(3).unwrap();
        f.programmatic_done(cmd.gen);
        // Mutation while detached: stay put — don't yank the user's scroll.
        f.user_scrolled();
        assert!(f.playlist_mutated(4).is_none());
        // But a reopen always recenters.
        assert_eq!(f.panel_opened(4).unwrap().item, 4);
    }
}
