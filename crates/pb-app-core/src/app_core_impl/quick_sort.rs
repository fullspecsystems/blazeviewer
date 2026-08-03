//! **Quick Sort** — the `AppCore` half of [`crate::quick_sort`] (task #136; the two-halves
//! rule in `docs/where-code-goes.md`).
//!
//! `quick_sort.rs` owns the slot model, the naming rules, and the file I/O. This file holds the
//! `impl AppCore` methods that turn a keypress into a filed photo: validate the slot, guard the
//! things that cannot be sorted, retire the item from the deck, hand the job to the worker, and
//! apply the result when it comes back.
//!
//! ⚠ Sorting is an **explicit user edit** — like delete and save-rotation, one of the few
//! places the app writes to the user's disk at all, and allowed precisely because it is never a
//! byproduct of viewing. See the privacy guarantee in the root `CLAUDE.md`.
//!
//! **The shape to preserve:** the keypress does no I/O. It settles, guards, removes the item
//! and schedules the advance — then returns. Everything that can block (a rename onto an SMB
//! share, a cross-volume copy) happens on the worker and lands back through
//! [`poll_quick_sort`](AppCore::poll_quick_sort). This is the difference between a key you can
//! hammer and one that stutters, and it is why quick sort does *not* simply copy `do_delete`'s
//! synchronous shape.

use super::*;
use crate::quick_sort::{SortDone, SortJob, SortMode, SortQueue};

impl AppCore {
    /// **Quick Sort** (`1`–`7`, `Shift+1`–`Shift+7`): file the displayed photo into slot
    /// `index`'s folder (0-based — the user-facing number is one more).
    ///
    /// Refuses, with an honest toast, everything that has no file to move: an unconfigured
    /// slot, an archive entry, an empty deck. A [`SortMode::Move`] retires the item and
    /// advances; a [`SortMode::Copy`] leaves the deck alone, because the photo is still there.
    pub fn quick_sort_to_slot(&mut self, index: u8) {
        // Settle any still-pending delete/sort advance first, so a rapid mixed run of Del and
        // digit keys can't operate on an item the deck is already retiring.
        self.flush_pending_delete();

        let slot_number = index as usize + 1;
        let Some(slot) = self.settings.quick_sort.get(index as usize).cloned() else {
            // Only reachable from a hand-built action; `Settings::clamp` normalizes the list.
            self.show_toast(&format!("No folder set for Quick Sort {slot_number}"));
            return;
        };
        let Some(dest_dir) = slot.folder.clone() else {
            // The digit keys did nothing before this feature existed, so a press with no
            // destination has to say what it *would* have done. Naming the feature is also the
            // breadcrumb to the fix: "Quick Sort" is the Settings tab's name.
            self.show_toast(&format!("No folder set for Quick Sort {slot_number}"));
            return;
        };
        let Some(item) = self.displayed_item else {
            return; // empty deck — nothing to file
        };
        let Some(src) = self.source.path(item).map(Path::to_path_buf) else {
            self.show_toast("Can't sort this"); // archive entry — no file on disk
            return;
        };

        let label = slot.display_label();
        // Already there: a no-op that must not remove the item from the deck, and must not
        // read as a failure either.
        if src.parent() == Some(dest_dir.as_path()) {
            self.show_toast(&format!("Already in {label}"));
            return;
        }

        // A playing video's reader holds the file open (task #79 action matrix) — stop it
        // before the worker tries to rename underneath it.
        if self.video.as_ref().is_some_and(|v| v.item() == item) {
            self.stop_video();
        }

        let job = SortJob {
            index: item,
            src,
            dest_dir,
            mode: slot.mode,
            slot_label: label.clone(),
        };
        if !self.quick_sort_queue().submit(job) {
            self.show_toast("Sort failed");
            return;
        }

        // The folder-with-a-down-arrow glyph carries the "filed into" sense, so the text is
        // just the destination name — an arrow in both would be saying it twice.
        self.show_toast_icon(&label, ToastIcon::Sorted);
        if slot.mode == SortMode::Move {
            // The item is leaving. Freeze any animation on the doomed photo and defer the
            // advance a beat so the pill registers first — exactly `finish_delete`'s timing,
            // deliberately, so Del and a sort key feel like the same class of operation.
            self.stop_playback();
            self.pending_delete = Some((self.now + DELETE_ADVANCE_DELAY, item));
        }
    }

    /// The worker, spawned on first use so a session that never sorts (and every headless
    /// test that only exercises the guards) starts no thread.
    fn quick_sort_queue(&mut self) -> &SortQueue {
        self.quick_sort_queue.get_or_insert_with(SortQueue::new)
    }

    /// Apply every sort that finished since the last tick. Called from `tick`; a no-op when the
    /// worker was never started or nothing has completed.
    pub fn poll_quick_sort(&mut self) {
        let Some(queue) = self.quick_sort_queue.as_ref() else {
            return;
        };
        for done in queue.drain() {
            self.finish_quick_sort(done);
        }
    }

    /// One completed sort: record the undo entry, or put the item back and say what went wrong.
    fn finish_quick_sort(&mut self, done: SortDone) {
        let SortDone { job, result } = done;
        let name = crate::engine::file_name_of(&job.src.to_string_lossy());
        match result {
            Ok(outcome) => {
                if outcome.sidecar_failures > 0 {
                    // The image is filed — that is the operation — but a label file left
                    // behind is exactly the silent corruption this feature must not cause,
                    // so it is said out loud rather than buried in stderr.
                    self.show_toast(&format!(
                        "Sorted, but {} sidecar file(s) stayed behind",
                        outcome.sidecar_failures
                    ));
                }
                self.undo_stack.push(UndoAction::Sorted {
                    index: job.index,
                    from: job.src,
                    to: outcome.dest,
                    sidecars: outcome.sidecars,
                    name,
                    mode: job.mode,
                });
            }
            Err(e) => {
                eprintln!("quick sort failed: {}: {e}", job.src.display());
                self.show_toast(&format!("Couldn't sort into {}", job.slot_label));
                // A Move already retired the item from the deck on the keypress. The file is
                // still on disk, so put it back where it was rather than leaving the user with
                // a photo that silently vanished from the deck but not from the folder.
                if job.mode == SortMode::Move {
                    self.flush_pending_delete();
                    self.reinsert_after_restore(job.index, &job.src);
                }
            }
        }
    }

    /// Undo a completed Quick Sort: return the file (and its sidecars) and put it back in the
    /// deck. The `Copy` half deletes the copy instead — see [`UndoAction::Sorted`].
    pub(super) fn undo_quick_sort(
        &mut self,
        index: usize,
        from: PathBuf,
        to: PathBuf,
        sidecars: Vec<(PathBuf, PathBuf)>,
        name: String,
        mode: SortMode,
    ) {
        match crate::quick_sort::undo_sort(&from, &to, &sidecars, mode) {
            Ok(()) => {
                if mode == SortMode::Move {
                    self.reinsert_after_restore(index, &from);
                }
                self.show_toast_icon(&format!("Returned {name}"), ToastIcon::Undo);
            }
            Err(e) => {
                eprintln!("undo sort failed: {}: {e}", to.display());
                // The file is still safely at its sorted location either way; the entry is
                // spent, matching how a failed undelete reports and moves on.
                self.show_toast("Couldn't undo the sort");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{test_core, FakeArchive};
    use crate::quick_sort::QuickSortSlot;

    /// A throwaway directory tree, removed on drop.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "pb_qs_core_{tag}_{}_{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("mkdir");
            TempTree(dir)
        }
        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("mkdir parent");
            }
            std::fs::write(&p, body).expect("write");
            p
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn toast_of(core: &AppCore) -> Option<String> {
        core.toast_native.as_ref().map(|t| t.message.clone())
    }

    /// Point slot `index` at `dir` and make the core use it.
    fn configure(core: &mut AppCore, index: usize, dir: &Path, mode: SortMode) {
        core.native_toast = true;
        core.settings.quick_sort[index] = QuickSortSlot {
            folder: Some(dir.to_path_buf()),
            label: String::new(),
            mode,
        };
    }

    /// Wait for the worker to report, then apply it. Bounded so a hang fails the test rather
    /// than wedging the suite.
    fn settle(core: &mut AppCore) {
        for _ in 0..200 {
            core.poll_quick_sort();
            if core
                .undo_stack
                .iter()
                .any(|a| matches!(a, UndoAction::Sorted { .. }))
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the quick-sort worker never reported");
    }

    /// The **menu route** (task #136): File ▸ Quick Sort and the photo context menu carry the
    /// stable Action id (`quick_sort_6`) on each row, which the host hands straight to
    /// `Action::from_id` → `CoreEvent::MenuAction`. This pins that a menu-invoked sort is the
    /// *same operation* as the keypress, right down to the slot it picks — an off-by-one
    /// between the row's 1-based name and the action's 0-based payload would silently file
    /// photos into the neighbouring folder, which is the worst possible failure here.
    #[test]
    fn a_menu_row_files_into_the_slot_its_id_names() {
        let t = TempTree::new("menu");
        let src = t.write("src/a.jpg", "pixels");
        let dest = t.path("out");
        let mut core = test_core();
        // Slot 6 for the user, index 5 for the code.
        configure(&mut core, 5, &dest, SortMode::Move);
        core.source = Arc::new(FsSource::new(vec![src.clone()]));
        core.displayed_item = Some(0);

        let action = crate::action::Action::from_id("quick_sort_6").expect("the menu row's id");
        assert_eq!(
            action,
            crate::action::Action::QuickSort(5),
            "the 1-based row name maps to the 0-based slot index"
        );
        core.handle(contract::CoreEvent::MenuAction(action));

        settle(&mut core);
        assert!(
            dest.join("a.jpg").exists(),
            "the menu route files into slot 6's folder, not a neighbour's"
        );
    }

    #[test]
    fn an_unconfigured_slot_says_so_and_does_nothing() {
        let mut core = test_core();
        core.native_toast = true;
        core.source = Arc::new(FsSource::new(vec![PathBuf::from("/p/a.jpg")]));
        core.displayed_item = Some(0);

        core.quick_sort_to_slot(2); // 0-based → the user's "Quick Sort 3"

        assert_eq!(
            toast_of(&core).as_deref(),
            Some("No folder set for Quick Sort 3"),
            "names the slot the way the user counts it"
        );
        assert!(core.pending_delete.is_none(), "nothing was retired");
        assert!(
            core.quick_sort_queue.is_none(),
            "and no worker thread was spawned"
        );
    }

    #[test]
    fn an_archive_entry_cannot_be_sorted() {
        let t = TempTree::new("archive");
        let mut core = test_core();
        configure(&mut core, 0, &t.path("out"), SortMode::Move);
        core.source = Arc::new(FakeArchive {
            names: vec!["a/photo.jpg".to_string()],
            container: std::env::temp_dir().join("deck.zip"),
        });
        core.displayed_item = Some(0);

        core.quick_sort_to_slot(0);

        assert_eq!(toast_of(&core).as_deref(), Some("Can't sort this"));
        assert!(core.pending_delete.is_none(), "nothing was retired");
    }

    #[test]
    fn sorting_into_the_photos_own_folder_is_a_no_op() {
        let t = TempTree::new("noop");
        let src = t.write("src/a.jpg", "pixels");
        let mut core = test_core();
        configure(&mut core, 0, &t.path("src"), SortMode::Move);
        core.source = Arc::new(FsSource::new(vec![src.clone()]));
        core.displayed_item = Some(0);

        core.quick_sort_to_slot(0);

        assert_eq!(toast_of(&core).as_deref(), Some("Already in src"));
        assert!(
            core.pending_delete.is_none(),
            "an already-there photo must not leave the deck"
        );
        assert!(src.exists());
    }

    #[test]
    fn a_move_retires_the_item_immediately_and_files_it_off_thread() {
        let t = TempTree::new("move");
        let src = t.write("src/a.jpg", "pixels");
        let dest = t.path("out");
        let mut core = test_core();
        configure(&mut core, 0, &dest, SortMode::Move);
        core.source = Arc::new(FsSource::new(vec![src.clone(), t.path("src/b.jpg")]));
        core.displayed_item = Some(0);

        core.quick_sort_to_slot(0);

        // The keypress half is synchronous and does no I/O: the item is already retiring.
        assert_eq!(
            core.pending_delete.map(|(_, item)| item),
            Some(0),
            "the advance is scheduled on the keypress, not on the I/O"
        );
        assert_eq!(
            toast_of(&core).as_deref(),
            Some("out"),
            "the pill names the destination"
        );

        settle(&mut core);
        assert!(!src.exists(), "the file left the source folder");
        assert_eq!(
            std::fs::read_to_string(dest.join("a.jpg")).expect("read"),
            "pixels"
        );
    }

    #[test]
    fn a_copy_leaves_the_deck_alone() {
        let t = TempTree::new("copy");
        let src = t.write("src/a.jpg", "pixels");
        let dest = t.path("out");
        let mut core = test_core();
        configure(&mut core, 0, &dest, SortMode::Copy);
        core.source = Arc::new(FsSource::new(vec![src.clone()]));
        core.displayed_item = Some(0);

        core.quick_sort_to_slot(0);

        assert!(
            core.pending_delete.is_none(),
            "a copy does not retire the item — the photo is still on screen"
        );
        settle(&mut core);
        assert!(src.exists(), "the original stayed put");
        assert!(dest.join("a.jpg").exists());
    }

    #[test]
    fn a_completed_move_can_be_undone_file_and_deck() {
        let t = TempTree::new("undo");
        let src = t.write("src/a.jpg", "pixels");
        t.write("src/a.txt", "label"); // a sidecar must come back too
        let dest = t.path("out");
        let mut core = test_core();
        configure(&mut core, 0, &dest, SortMode::Move);
        core.source = Arc::new(FsSource::new(vec![src.clone()]));
        core.displayed_item = Some(0);

        core.quick_sort_to_slot(0);
        settle(&mut core);
        assert!(!src.exists());

        core.undo();

        assert_eq!(
            std::fs::read_to_string(&src).expect("restored"),
            "pixels",
            "the image came home"
        );
        assert_eq!(
            std::fs::read_to_string(t.path("src/a.txt")).expect("restored"),
            "label",
            "and so did its label file"
        );
        assert!(
            !dest.join("a.jpg").exists(),
            "nothing left at the destination"
        );
    }

    #[test]
    fn undoing_a_copy_removes_the_copy_rather_than_moving_it_back() {
        let t = TempTree::new("undocopy");
        let src = t.write("src/a.jpg", "pixels");
        let dest = t.path("out");
        let mut core = test_core();
        configure(&mut core, 0, &dest, SortMode::Copy);
        core.source = Arc::new(FsSource::new(vec![src.clone()]));
        core.displayed_item = Some(0);

        core.quick_sort_to_slot(0);
        settle(&mut core);

        core.undo();

        assert!(src.exists(), "the original was never ours to move");
        assert!(!dest.join("a.jpg").exists(), "the copy we made is gone");
    }

    #[test]
    fn a_failed_move_puts_the_item_back_in_the_deck() {
        let t = TempTree::new("fail");
        // A *file* stands where the destination folder would go, so `create_dir_all` fails.
        let blocker = t.write("blocked", "not a folder");
        let src = t.write("src/a.jpg", "pixels");
        let mut core = test_core();
        configure(&mut core, 0, &blocker, SortMode::Move);
        core.source = Arc::new(FsSource::new(vec![src.clone()]));
        core.displayed_item = Some(0);

        core.quick_sort_to_slot(0);
        assert!(core.pending_delete.is_some(), "optimistically retired");

        // Drain until the failure lands (no undo entry is ever pushed for a failure).
        for _ in 0..200 {
            core.poll_quick_sort();
            if core.pending_delete.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(src.exists(), "the user's file never moved");
        assert!(
            core.undo_stack.is_empty(),
            "a failure records nothing to undo"
        );
        assert!(
            (0..core.source.len()).any(|i| core.source.path(i) == Some(src.as_path())),
            "and the photo is back in the deck"
        );
    }
}
