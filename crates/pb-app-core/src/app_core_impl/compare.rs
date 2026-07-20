//! **Compare (A/B pin)** — the `AppCore` methods behind pinning one item and toggling
//! between it and the current one (task #125).
//!
//! A *topic*, not a subsystem: the state is a couple of `AppCore` fields, not an owned
//! module. `docs/where-code-goes.md` allows a topic its own `app_core_impl/` file without
//! inventing a sibling to pair with.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// `⇧Y` / Image ▸ Pin for Compare — pin the current photo, or unpin when it's
    /// already the pin. The whole pin-management surface.
    pub fn compare_pin_cmd(&mut self) {
        let Some(item) = self.displayed_item else {
            return; // empty state — nothing to pin
        };
        if self.compare_pin == Some(item) {
            self.clear_compare_pin();
            self.show_toast_icon("Unpinned", ToastIcon::Unpin);
        } else {
            self.set_compare_pin(item);
        }
    }

    /// `Y` / Image ▸ Compare with Pinned — flip between the pinned photo and the
    /// current one. With nothing pinned yet, pins the current photo instead, so a
    /// single key drives the whole feature.
    pub fn compare_toggle_cmd(&mut self) {
        let Some(current) = self.displayed_item else {
            return;
        };
        let Some(pin) = self.compare_pin else {
            self.set_compare_pin(current);
            return;
        };
        if current == pin {
            // Viewing the pin: flip back to the remembered position. No return point
            // yet (pinned, never flipped) → nothing to do.
            if let Some(ret) = self.compare_return {
                if ret != pin && ret < self.source.len() {
                    self.compare_jump(ret);
                }
            }
        } else {
            self.compare_return = Some(current);
            self.compare_jump(pin);
        }
    }

    fn set_compare_pin(&mut self, item: usize) {
        self.compare_pin = Some(item);
        self.compare_pin_id = Some(self.compare_identity(item));
        self.compare_return = None;
        self.show_toast_icon("Pinned for compare", ToastIcon::Pin);
        // Re-issue the want-list so the pin's eviction exemption takes effect now.
        self.request_prefetch();
    }

    /// Drop the pin and its bookkeeping (deleting the pinned photo, a new deck).
    pub fn clear_compare_pin(&mut self) {
        self.compare_pin = None;
        self.compare_return = None;
        self.compare_pin_id = None;
    }

    /// The pinned item's rebuild-stable identity: the full path where one exists,
    /// else the archive-entry name.
    pub(super) fn compare_identity(&self, item: usize) -> String {
        match self.source.path(item) {
            Some(p) => p.to_string_lossy().into_owned(),
            None => self.source.name(item).to_string(),
        }
    }

    /// Jump the cursor to an absolute index and present it — `advance`'s gated engine
    /// path for the compare flip. The live zoom/pan carries across the flip when both
    /// photos share dimensions and rotation (the 100%-crop sharpness workflow: the
    /// same crop of the other frame lands under your gaze).
    fn compare_jump(&mut self, item: usize) {
        self.flush_pending_delete();
        // Never jump while the previous target is still pending (a miss in flight) —
        // mirroring `advance`, so a photo is never silently skipped.
        if self.displayed_item != self.target_item {
            return;
        }
        // Stage the carried zoom/pan for `view_for` to consume, so the flip's FIRST
        // presented frame already has the view — one set_view + one draw. (The first
        // cut presented at the reset view and re-imposed the carry afterwards: two
        // draws, and the incoming photo flashed centered for a frame.)
        self.compare_carry = self.compare_carry_view(item);
        self.stop_playback();
        self.playlist.jump_to(item);
        self.target_item = self.playlist.current();
        self.try_present_target();
        // Not consumed (a ring miss / failed target): drop it rather than let some
        // later unrelated present inherit a stale view.
        self.compare_carry = None;
        self.request_prefetch();
    }

    /// The live zoom/pan to carry across a flip to `to`, or `None` when the view is
    /// the default or the two photos don't share geometry (same pixel dimensions AND
    /// the same rotation override — otherwise the crop wouldn't map anyway).
    pub(super) fn compare_carry_view(&self, to: usize) -> Option<(f32, [f32; 2])> {
        if self.view.zoom == 1.0 && self.view.pan == [0.0, 0.0] {
            return None; // default view — nothing worth carrying
        }
        let from = self.displayed_item?;
        let a = self.meta_cache.get(&from)?;
        let b = self.meta_cache.get(&to)?;
        let rot_a = self.rotations.get(&from).copied().unwrap_or_default();
        let rot_b = self.rotations.get(&to).copied().unwrap_or_default();
        ((a.w, a.h) == (b.w, b.h) && rot_a == rot_b).then_some((self.view.zoom, self.view.pan))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::test_core;
    use crate::contract::CoreEvent;

    #[test]
    fn sibling_results_are_stale_guarded_and_only_matches_navigate() {
        let opened_dir = |core: &AppCore| {
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::BeginDirScan { .. }))
        };
        let feed = |core: &mut AppCore,
                    from_root: PathBuf,
                    target: Option<crate::folder_tree::DiskTarget>| {
            let (tx, rx) = std::sync::mpsc::channel();
            tx.send(crate::folder_tree::TreeIoResult::Sibling { from_root, target })
                .unwrap();
            core.tree_io = Some(crate::folder_tree::tree_io_for_tests(rx));
            core.effects.clear();
            let t = core.now;
            core.handle(CoreEvent::Tick(t));
        };

        let mut core = compare_core(2);
        // A result computed for a deck the user already left: dropped — it must
        // not yank navigation somewhere the user moved away from.
        feed(
            &mut core,
            PathBuf::from("/somewhere/else"),
            Some(crate::folder_tree::DiskTarget::Directory(PathBuf::from(
                "/somewhere/else/next",
            ))),
        );
        assert!(!opened_dir(&core), "stale sibling results are dropped");
        assert!(core.tree_io.is_none(), "the finished job is released");

        // Nothing with photos in that direction: no navigation (the host shows
        // the toast — HUD-gated, so asserted in the live smoke, not here).
        let root = core.root.clone();
        feed(&mut core, root, None);
        assert!(
            !opened_dir(&core),
            "an exhausted search must not open anything"
        );

        // A live match opens exactly like Open Folder — the shared plan.
        let root = core.root.clone();
        let target = root.join("next-door");
        feed(
            &mut core,
            root,
            Some(crate::folder_tree::DiskTarget::Directory(target)),
        );
        assert!(opened_dir(&core), "a found sibling opens as a dir scan");
    }

    /// A headless core over an n-item deck of (nonexistent) temp paths — decode
    /// failures are tolerated everywhere off the hot path, and the compare tests
    /// assert on cursor/target/pin state, not on presentation.
    fn compare_core(n: usize) -> AppCore {
        let mut core = test_core();
        let dir = std::env::temp_dir();
        let paths: Vec<PathBuf> = (0..n).map(|i| dir.join(format!("cmp_{i}.png"))).collect();
        let source: Arc<dyn ItemSource> = Arc::new(FsSource::new(paths));
        core.rebuild_playlist(source, dir.clone(), Some(dir), true, 0);
        core
    }

    /// Move the cursor to `i` and mark it settled (headless: no decode ever lands,
    /// so the `displayed == target` gate is satisfied by hand).
    fn settle_at(core: &mut AppCore, i: usize) {
        core.playlist.jump_to(i);
        core.target_item = Some(i);
        core.displayed_item = Some(i);
    }

    #[test]
    fn compare_toggle_pins_first_then_flips_and_returns() {
        let mut core = compare_core(5);
        // First Y with nothing pinned: pins the current photo, no navigation.
        core.dispatch_action(Action::CompareToggle);
        assert_eq!(core.compare_pin, Some(0));
        assert_eq!(core.target_item, Some(0), "pinning must not navigate");
        // Browse to 3, then Y: flips to the pin, remembering where we were.
        settle_at(&mut core, 3);
        core.dispatch_action(Action::CompareToggle);
        assert_eq!(core.target_item, Some(0), "Y flips to the pin");
        assert_eq!(core.compare_return, Some(3));
        // Y again from the pin: returns to the remembered position.
        core.displayed_item = core.target_item;
        core.dispatch_action(Action::CompareToggle);
        assert_eq!(core.target_item, Some(3), "Y on the pin returns");
        assert_eq!(core.compare_pin, Some(0), "the pin itself stays fixed");
    }

    #[test]
    fn compare_pin_moves_and_unpins() {
        let mut core = compare_core(4);
        core.dispatch_action(Action::ComparePin);
        assert_eq!(core.compare_pin, Some(0));
        // ⇧Y elsewhere moves the pin (and resets the return point).
        settle_at(&mut core, 2);
        core.compare_return = Some(1);
        core.dispatch_action(Action::ComparePin);
        assert_eq!(core.compare_pin, Some(2));
        assert_eq!(core.compare_return, None, "re-pin resets the return point");
        // ⇧Y on the pinned photo unpins.
        core.dispatch_action(Action::ComparePin);
        assert_eq!(core.compare_pin, None);
        assert_eq!(core.compare_pin_id, None);
    }

    #[test]
    fn compare_flip_never_interrupts_a_pending_target() {
        let mut core = compare_core(5);
        core.dispatch_action(Action::CompareToggle); // pin = 0
                                                     // The launch decode of (nonexistent) cmp_0 failed, and a failed target
                                                     // auto-settles via `present_failed` — clear it so the flip is a genuine
                                                     // ring MISS that stays pending, which is what this test is about.
        core.failed.clear();
        settle_at(&mut core, 3);
        core.dispatch_action(Action::CompareToggle); // flip to the pin...
        assert_eq!(core.target_item, Some(0));
        // ...but the present hasn't landed (displayed still 3). A second Y must not
        // clobber the in-flight target (mirrors `advance`'s never-skip gate).
        core.dispatch_action(Action::CompareToggle);
        assert_eq!(core.target_item, Some(0));
        assert_eq!(core.displayed_item, Some(3));
    }

    #[test]
    fn compare_pin_survives_a_same_deck_rebuild_and_clears_on_a_new_deck() {
        let dir = std::env::temp_dir();
        let mut core = compare_core(4);
        settle_at(&mut core, 2);
        core.dispatch_action(Action::ComparePin); // pin = cmp_2 at index 2
                                                  // The delete-advance shape: same paths minus cmp_1 → cmp_2 shifts to index 1.
        let remaining: Vec<PathBuf> = [0usize, 2, 3]
            .iter()
            .map(|i| dir.join(format!("cmp_{i}.png")))
            .collect();
        let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(remaining));
        core.rebuild_playlist(src, dir.clone(), Some(dir.clone()), true, 0);
        assert_eq!(
            core.compare_pin,
            Some(1),
            "the pin re-resolves by path across a same-deck rebuild"
        );
        assert_eq!(core.compare_return, None, "the return point never survives");
        // A genuinely new deck has no matching identity — the pin clears.
        let other: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("other_{i}.png"))).collect();
        let src: Arc<dyn ItemSource> = Arc::new(FsSource::new(other));
        core.rebuild_playlist(src, dir.clone(), Some(dir), true, 0);
        assert_eq!(core.compare_pin, None);
        assert_eq!(core.compare_pin_id, None);
    }

    #[test]
    fn compare_pin_rides_the_prefetch_want_list_at_top_two() {
        let mut core = compare_core(50);
        core.dispatch_action(Action::ComparePin); // pin = 0
        settle_at(&mut core, 40);
        core.request_prefetch();
        assert_eq!(
            core.targets.first(),
            Some(&40),
            "current target stays first"
        );
        assert_eq!(
            core.targets.get(1),
            Some(&0),
            "the pin rides at top-2 priority so eviction can never drop it"
        );
        assert_eq!(
            core.targets.iter().filter(|&&t| t == 0).count(),
            1,
            "the pin appears exactly once"
        );
    }

    #[test]
    fn deleting_down_to_the_empty_state_clears_the_pin() {
        let mut core = compare_core(1);
        core.dispatch_action(Action::ComparePin);
        assert!(core.compare_pin.is_some());
        core.enter_empty_state();
        assert_eq!(core.compare_pin, None);
        assert_eq!(core.compare_pin_id, None);
    }

    #[test]
    fn compare_carry_applies_only_to_matching_geometry() {
        use crate::meta::PhotoMeta;
        let meta = |w: u32, h: u32| PhotoMeta {
            rel: String::new(),
            w,
            h,
            size: None,
            codec: "PNG",
            animated: None,
            recovered: None,
        };
        let mut core = compare_core(3);
        core.meta_cache.insert(0, meta(100, 80));
        core.meta_cache.insert(2, meta(100, 80));
        settle_at(&mut core, 2);
        core.view.zoom = 3.0;
        core.view.pan = [10.0, -4.0];
        assert_eq!(
            core.compare_carry_view(0),
            Some((3.0, [10.0, -4.0])),
            "same dims + same rotation → the crop carries"
        );
        // A rotation override on one side breaks the mapping.
        core.rotations.insert(0, Rotation::default().cw());
        assert_eq!(core.compare_carry_view(0), None);
        core.rotations.clear();
        // Dimension mismatch → no carry.
        core.meta_cache.insert(0, meta(99, 80));
        assert_eq!(core.compare_carry_view(0), None);
        // Default view → nothing worth carrying.
        core.meta_cache.insert(0, meta(100, 80));
        core.view.zoom = 1.0;
        core.view.pan = [0.0, 0.0];
        assert_eq!(core.compare_carry_view(0), None);
    }

    #[test]
    fn compare_carry_is_staged_for_the_flips_first_frame_and_is_one_shot() {
        // The owner-reported flicker: presenting at the reset view and re-imposing the
        // carry afterwards flashed the incoming photo centered for one frame. The carry
        // is now staged for `view_for` to consume, so the FIRST present lands
        // positioned — and it must be one-shot, never leaking into a later present.
        use crate::meta::PhotoMeta;
        let meta = |w: u32, h: u32| PhotoMeta {
            rel: String::new(),
            w,
            h,
            size: None,
            codec: "PNG",
            animated: None,
            recovered: None,
        };
        let mut core = compare_core(3);
        core.meta_cache.insert(0, meta(100, 80));
        core.meta_cache.insert(2, meta(100, 80));
        core.dispatch_action(Action::CompareToggle); // pin = 0
        core.failed.clear(); // cmp_0's launch decode failed; make the flip a clean MISS
        settle_at(&mut core, 2);
        core.view.zoom = 2.0;
        core.view.pan = [5.0, 6.0];
        core.dispatch_action(Action::CompareToggle); // flip stages the carry...
                                                     // ...but headless the present missed (no ring): the stash must be dropped so a
                                                     // later unrelated present resets instead of inheriting a stale view.
        assert_eq!(core.compare_carry, None);
        let v = core.view_for(2);
        assert_eq!((v.zoom, v.pan), (1.0, [0.0, 0.0]));
        // A staged carry is consumed by exactly ONE view_for (the flip's present).
        core.compare_carry = Some((2.0, [5.0, 6.0]));
        let v = core.view_for(0);
        assert_eq!((v.zoom, v.pan), (2.0, [5.0, 6.0]), "first frame carries");
        let v = core.view_for(0);
        assert_eq!((v.zoom, v.pan), (1.0, [0.0, 0.0]), "the carry is one-shot");
    }
}
