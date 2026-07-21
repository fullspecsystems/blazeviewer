//! **Folder tree** — the `AppCore` half of [`crate::folder_tree`] and [`crate::fs_tree`]
//! (task #125; the two-halves rule in `docs/where-code-goes.md`).
//!
//! Two trees behind one panel: the *deck* tree (folders present in the open playlist) and
//! the *filesystem* tree (browse anywhere), switched by mode. This file holds the `AppCore`
//! methods that show/hide the panel, build and push its rows, drive the async fs walk, and
//! handle hit-testing, hover and clicks.
//!
//! `drive_fs_tree`, `push_folder_tree`, `show_folder_tree_mode` and `folder_sig` are
//! `pub(super)` because `tick` drives them every frame and stays in the parent.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Apply the tree's visibility after a hide/reveal: clear the bitmap when hidden,
    /// force a rebuild against fresh state when revealed (the signature gate re-runs
    /// the derivation next tick).
    pub(super) fn refresh_tree_visibility(&mut self) {
        if self.panels.tree_visible(self.folder_tree_open) {
            self.folder_tree_sig = None; // rebuild + re-upload next tick
        } else {
            self.hide_folder_tree();
        }
    }

    /// Toggle the folder-tree overlay (`Shift+F`): the current photo's folder in its
    /// hierarchy — up affordance, root, ancestor chain, siblings, children — in the
    /// top-left corner. Rows are clickable (full Open Folder semantics) and the
    /// "… n more" windowing markers page the list. See
    /// `.taskmaster/docs/folder-tree-plan.md`.
    pub fn toggle_folder_tree(&mut self) {
        if self.panels.reveal() {
            // ⇧F while Tab-hidden reveals first and only ever *shows* (the reveal
            // rule): the tree opens/re-draws and any hidden Inspector/Help panel
            // comes back with it — `hidden` is one master flag, the Photoshop idiom.
            self.folder_tree_open = true;
            self.left_tab = crate::overlay::LeftTab::Folders;
            self.show_folder_tree();
            self.refresh_slot();
            return;
        }
        if self.folder_tree_open && self.left_tab == crate::overlay::LeftTab::Thumbnails {
            // ⇧F while the Thumbnails tab is showing: switch tabs, don't close —
            // the Inspector's per-tab semantics for the left pane (task #83).
            self.left_tab = crate::overlay::LeftTab::Folders;
            self.show_folder_tree();
            self.emit_panels_changed();
            return;
        }
        self.folder_tree_open = !self.folder_tree_open;
        self.left_tab = crate::overlay::LeftTab::Folders;
        if self.folder_tree_open {
            self.show_folder_tree();
        } else {
            self.hide_folder_tree();
        }
        self.emit_panels_changed();
    }

    /// The displayed photo's containing folder as a forward-slashed path — the
    /// tree's cheap identity, no I/O. Root-relative (`""` = the root level) for
    /// photos under the root; the **absolute** parent for out-of-root photos
    /// (explicit multi-folder decks), so two different folders never collapse to
    /// the same rebuild signature.
    fn current_folder_rel(&self, item: usize) -> String {
        match self.source.path(item) {
            Some(p) => crate::folder_tree::folder_identity(p, &self.root),
            None => crate::folder_tree::folder_of(self.source.name(item)).to_string(),
        }
    }

    /// The drawn tree's rebuild signature: deck root + current folder (`@root` for
    /// an empty deck, which browses from the root itself). Compared per tick while
    /// the overlay is open (string ops only — the `read_dir`s in
    /// [`show_folder_tree`](Self::show_folder_tree) run only when this changes).
    pub(super) fn folder_sig(&self) -> String {
        match self.displayed_item {
            Some(item) => format!("{}|{}", self.root.display(), self.current_folder_rel(item)),
            None => format!("{}|@root", self.root.display()),
        }
    }

    /// The per-deck folder-counts map (`disk_counts` over the playlist), cached by
    /// (root, deck length) so the badges and the flight fast path never re-walk an
    /// unchanged deck. One O(n) pass when the deck (or a streaming batch) changes.
    fn folder_counts(&mut self) -> Arc<std::collections::HashMap<PathBuf, u64>> {
        if let Some((r, n, map)) = &self.folder_tree_counts {
            if *r == self.root && *n == self.source.len() {
                return map.clone();
            }
        }
        let map = Arc::new(crate::folder_tree::disk_counts(
            (0..self.source.len()).filter_map(|i| self.source.path(i)),
            &self.root,
        ));
        self.folder_tree_counts = Some((self.root.clone(), self.source.len(), map.clone()));
        map
    }

    /// Derive + rasterize + draw the folder tree for the current deck state, and
    /// stamp [`folder_tree_sig`](crate::AppCore::folder_tree_sig). Hover and page
    /// state reset — this is the fresh-content path; transitions re-render through
    /// [`push_folder_tree`](Self::push_folder_tree) from the cached rows instead.
    pub fn show_folder_tree(&mut self) {
        self.show_folder_tree_mode(false);
    }

    /// [`show_folder_tree`](Self::show_folder_tree) with the derivation choice:
    /// `lite` = the no-I/O flight variant (its signature is stamped `|lite`, so
    /// settling upgrades to the full `read_dir` view).
    ///
    /// Archive decks group their in-RAM entry names — no I/O, drawn right here.
    /// Disk decks (and the empty deck, which browses from the root so a photo-less
    /// folder never strands you) paint the **lite** view immediately — sibling and
    /// child folders from the cached counts map, pure in-RAM — and, for the full
    /// view, hand the `read_dir` derivation to an off-thread worker that `tick`
    /// installs when it lands. The disk I/O never runs on this thread: a
    /// spun-down drive or a dead network share must not stall the event loop.
    pub(super) fn show_folder_tree_mode(&mut self, lite: bool) {
        // Check the cheap gates before deriving rows, so a font-less host doesn't
        // pay the derivation on every retry tick. A Tab-hidden tree derives nothing
        // either — reveal forces the rebuild via the cleared signature. A disk deck on
        // the native host uses the resident Finder tree (`drive_fs_tree`), not this.
        if self.hud.is_none()
            || !self.panels.tree_visible(self.folder_tree_open)
            || self.tree_is_fs()
        {
            return;
        }
        let sig = self.folder_sig();
        let lite_stamp = format!("{sig}|lite");

        // An archive deck: entry names carry the internal folder paths; the
        // archive file labels the root, and the up row opens the folder on disk
        // containing it. The derivation reads the **full** (unscoped) source, so
        // a deck scoped to one internal folder still shows the whole archive
        // around it — the archive analog of the disk tree anchoring above the
        // opened root; clicking a row re-scopes to its prefix (the root row =
        // back to everything).
        if let Some(item) = self.displayed_item {
            if self.source.path(item).is_none() {
                let full = self
                    .archive_scope
                    .as_ref()
                    .map(|s| Arc::clone(&s.full))
                    .unwrap_or_else(|| Arc::clone(&self.source));
                let container = self.source.container().unwrap_or(&self.root);
                let label = crate::folder_tree::name_of(container);
                let current = self.current_folder_rel(item);
                let names = (0..full.len()).map(|i| full.name(i));
                let mut m = crate::folder_tree::rows_from_names(names, &current, &label);
                if let Some(par) = container.parent().filter(|p| !p.as_os_str().is_empty()) {
                    let par = par.to_path_buf();
                    m.push_up(&crate::folder_tree::name_of(&par), par.clone());
                }
                self.push_folder_tree(m.rows, m.targets, 0, None);
                self.folder_tree_sig = Some(sig);
                return;
            }
        }

        // A disk deck, anchored at the opened root (never above it — the up row is
        // the one deliberate exit); an empty deck browses from the root itself.
        let disk_dir: Option<PathBuf> = match self.displayed_item {
            Some(item) => self
                .source
                .path(item)
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            None => {
                (!self.root.as_os_str().is_empty() && self.root.is_dir()).then(|| self.root.clone())
            }
        };
        let Some(dir) = disk_dir else {
            // Nothing to show (bare launch): drop any stale quad, remember why.
            if self.folder_tree_panel.is_some() {
                self.hide_folder_tree();
            }
            self.folder_tree_sig = Some(if lite { lite_stamp } else { sig });
            return;
        };
        // Paint the lite view now — unless this folder's lite view is already up
        // (the settle-upgrade path), so upgrading doesn't reset hover/page.
        if self.folder_tree_sig.as_deref() != Some(lite_stamp.as_str()) {
            let counts = self.folder_counts();
            let model = crate::folder_tree::rows_from_paths(&self.root, &dir, &counts);
            self.push_folder_tree(model.rows, model.targets, 0, None);
        }
        self.folder_tree_sig = Some(lite_stamp);
        if !lite {
            // The settled read_dir view (it adds photo-less folders) derives
            // off-thread; the `|lite` stamp doubles as its "pending" marker.
            let counts = self.recursive.then(|| self.folder_counts());
            self.tree_io = Some(crate::folder_tree::spawn_full_tree(
                self.root.clone(),
                dir,
                counts,
                sig,
            ));
        }
    }

    /// Rasterize + upload the tree from prepared rows — the shared path for fresh
    /// derivations, hover transitions, and paging (the latter two reuse the cached
    /// rows, so they never re-derive and never touch the disk).
    pub(super) fn push_folder_tree(
        &mut self,
        rows: Vec<hud::TreeRow>,
        targets: Vec<Option<crate::folder_tree::TreeTarget>>,
        page: i32,
        hovered: Option<hud::TreeHit>,
    ) {
        // Native host (task #54): don't rasterize — store the full rows/targets for the
        // SwiftUI list (which scrolls, so no HUD windowing / paging markers or hit rects)
        // and signal the shell to re-pull. Reached only on a real derivation change
        // (hover/paging are HUD-only and never fire on this path), so we always emit.
        if self.native_tree {
            self.folder_tree_panel = Some(crate::overlay::TreePanel {
                w: 0,
                h: 0,
                margin: 0,
                hits: Vec::new(),
                targets,
                rows,
                hovered: None,
                page: 0,
                built: self.now,
            });
            self.emit_panels_changed();
            return;
        }
        let px = (15.0 * self.viewport.scale_factor).max(8.0);
        let pad = (7.0 * self.viewport.scale_factor).round().max(2.0) as u32;
        let margin = self.overlay_margin();
        let full_max_h = (self.viewport.height as i32 - 2 * margin as i32).max(1);
        let render = |hud: &pb_hud::hud::Hud, max_h: i32| {
            hud.render_tree(
                &rows,
                px,
                pad,
                hud.theme().bg,
                max_h,
                page,
                hovered,
                hud::TreeCounts::Capsule,
            )
        };
        let Some(hud) = self.hud.as_ref() else {
            return;
        };
        let Some((mut bitmap, mut w, mut h, mut hits)) = render(hud, full_max_h) else {
            return;
        };
        // The tree is top-left-anchored and a full-height one reaches the bottom strip;
        // if the info line overlaps the tree's column `[margin, margin + w]`, cap the
        // height by the line strip and re-render so a tall tree pages one row shorter
        // and leaves the line room (task #54). Only left/center/wide lines trigger this
        // — the default right line clears a normal tree column, so no re-render.
        let reserve = self.info_line_reserve_for(margin as f32, margin as f32 + w as f32);
        if reserve > 0 {
            let capped = (full_max_h - reserve as i32).max(1);
            if let Some(hud) = self.hud.as_ref() {
                if let Some(re) = render(hud, capped) {
                    (bitmap, w, h, hits) = re;
                }
            }
        }
        if let Some(a) = self.renderer.as_mut() {
            a.set_tree(Some((&bitmap, w, h)), margin);
        }
        self.folder_tree_panel = Some(crate::overlay::TreePanel {
            w,
            h,
            margin,
            hits,
            targets,
            rows,
            hovered,
            page,
            built: self.now,
        });
        self.draw();
    }

    /// Whether the **native** folder tree should be visible — the signal the mac host
    /// reads to show/hide its SwiftUI list: the tree is open, not `Tab`-hidden, and the
    /// host presents it natively.
    pub fn tree_panel_visible(&self) -> bool {
        self.native_tree
            && self.panels.tree_visible(self.folder_tree_open)
            && self.left_tab == crate::overlay::LeftTab::Folders
    }

    /// Activate a native tree row by index (a SwiftUI list click): navigate its target —
    /// open the folder, or re-scope the archive — exactly like the HUD tree's row click.
    /// Rows without a target (the current folder, a bare label) are inert.
    pub fn tree_activate(&mut self, index: usize) {
        let target = self
            .folder_tree_panel
            .as_ref()
            .and_then(|p| p.targets.get(index).cloned().flatten());
        match target {
            Some(crate::folder_tree::TreeTarget::Dir(dir)) => self.open_dir(dir),
            Some(crate::folder_tree::TreeTarget::Scope(prefix)) => self.rescope_archive(prefix),
            None => {}
        }
    }

    /// The current photo's containing folder (absolute), or `None` on an archive/empty
    /// deck (`source.path` is `None` for an archive entry). Gates the Finder tree.
    pub fn current_folder_abs(&self) -> Option<PathBuf> {
        let item = self.displayed_item?;
        self.source.path(item)?.parent().map(Path::to_path_buf)
    }

    /// Whether the native **Finder** tree (the resident [`FsTree`]) applies right now:
    /// the host presents the tree natively and the deck is a disk deck with a current
    /// folder. Archive/empty decks fall back to the v1 `folder_tree_panel`.
    pub fn tree_is_fs(&self) -> bool {
        self.native_tree && self.current_folder_abs().is_some()
    }

    /// The Finder tree's visible rows (empty when not built).
    pub fn fs_tree_rows(&self) -> Vec<crate::fs_tree::Row> {
        self.fs_tree.as_ref().map(|t| t.rows()).unwrap_or_default()
    }

    /// The name of the Finder tree root's parent — the label for the "up to parent" row
    /// (clicking it climbs a level). `None` at the filesystem root or when not built.
    pub fn fs_tree_parent_name(&self) -> Option<String> {
        self.fs_tree.as_ref().and_then(|t| t.parent_name())
    }

    /// Build (or re-root) the resident tree for the current disk deck and mark the current
    /// folder. Kept persistent while the current folder stays under the tree root (so
    /// browsing/expansion survives photo navigation); re-rooted only when the deck opens
    /// somewhere outside it. Fresh trees root one level above the deck root (so the deck
    /// root shows among its siblings).
    fn ensure_fs_tree(&mut self) {
        let Some(current) = self.current_folder_abs() else {
            self.fs_tree = None;
            self.fs_tree_io = None;
            return;
        };
        // Rebuild when there's no tree, the current folder left its root, OR the Show Archives
        // setting no longer matches what the tree was read with (task #108) — a live toggle then
        // refreshes the rows (its already-loaded children were read under the old setting).
        let show_archives = self.settings.show_archives;
        let rebuild = self.fs_tree.as_ref().is_none_or(|t| {
            // Rebuild when the current image folder left the tree root, when the Show Archives
            // setting no longer matches (task #108), OR when the *deck root* itself moved outside
            // the tree root (task #129). That last case is subtle: the breadcrumb can open an
            // ANCESTOR of the current deck (e.g. deck `/A/B/C`, tree rooted `/A/B`, open `/A`)
            // while the recursive scan's first/current image still sits under the old root — so
            // the current-folder check alone stays satisfied and Folders would keep a tree that
            // can't browse the newly-opened deck's siblings. `FsTree::set_current` refuses to
            // reveal an out-of-root folder, so the invariant "deck root ⊆ tree root" must hold.
            current.strip_prefix(t.root()).is_err()
                || self.root.strip_prefix(t.root()).is_err()
                || t.show_archives() != show_archives
        });
        if rebuild {
            let root = self
                .root
                .parent()
                .filter(|p| !p.as_os_str().is_empty() && current.starts_with(p))
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.root.clone());
            let (tx, rx) = std::sync::mpsc::channel();
            let mut tree = crate::fs_tree::FsTree::new(root);
            tree.set_show_archives(show_archives);
            self.fs_tree = Some(tree);
            self.fs_tree_io = Some(crate::app_core::FsTreeIo {
                tx,
                rx,
                pending: std::collections::HashSet::new(),
            });
        }
    }

    /// Drive the resident tree each tick while it's shown: install finished off-thread
    /// `read_dir` results, reveal + mark the current folder, kick reads for any expanded-
    /// but-unread folder, and refresh count badges — signalling the host on change.
    pub(super) fn drive_fs_tree(&mut self) {
        self.ensure_fs_tree();
        if self.fs_tree.is_none() {
            return;
        }
        let mut changed = false;
        // 1. Install finished reads.
        let done: Vec<(PathBuf, Vec<crate::folder_tree::DiskTarget>)> = self
            .fs_tree_io
            .as_ref()
            .map(|io| io.rx.try_iter().collect())
            .unwrap_or_default();
        for (path, subdirs) in done {
            if let Some(io) = self.fs_tree_io.as_mut() {
                io.pending.remove(&path);
            }
            if let Some(t) = self.fs_tree.as_mut() {
                t.set_children(&path, subdirs);
            }
            changed = true;
        }
        // 2. Reveal + mark the current folder (only when it moved).
        if let Some(folder) = self.current_folder_abs() {
            let moved = self
                .fs_tree
                .as_ref()
                .and_then(|t| t.current().map(Path::to_path_buf))
                != Some(folder.clone());
            if moved {
                if let Some(t) = self.fs_tree.as_mut() {
                    t.set_current(folder);
                }
                changed = true;
            }
        }
        // 3. Kick an off-thread read for each expanded-but-unread visible folder.
        let to_read: Vec<PathBuf> = self
            .fs_tree
            .as_ref()
            .map(|t| {
                t.rows()
                    .into_iter()
                    .filter(|r| r.loading)
                    .map(|r| r.path)
                    .collect()
            })
            .unwrap_or_default();
        for path in to_read {
            let in_flight = self
                .fs_tree_io
                .as_ref()
                .is_some_and(|io| io.pending.contains(&path));
            if in_flight {
                continue;
            }
            if let Some(io) = self.fs_tree_io.as_mut() {
                io.pending.insert(path.clone());
                let tx = io.tx.clone();
                // Read subfolders always, and archive files too when Show Archives is on
                // (task #108) — an archive shows as a leaf zipper row inside the folder.
                let show_archives = self.settings.show_archives;
                std::thread::spawn(move || {
                    let children = crate::folder_tree::dir_children(&path, show_archives);
                    let _ = tx.send((path, children));
                });
            }
        }
        // 4. Refresh count badges from the deck's folder-counts (cheap when cached).
        if changed {
            let counts = self.folder_counts();
            if let Some(t) = self.fs_tree.as_mut() {
                for (p, c) in counts.iter() {
                    t.set_count(p, Some(*c));
                }
            }
            self.emit_panels_changed();
        }
    }

    /// Toggle a folder's expansion (the chevron) — browsing only, never loads photos.
    pub fn fs_tree_toggle(&mut self, path: &Path) {
        if let Some(t) = self.fs_tree.as_mut() {
            t.toggle(path);
            self.emit_panels_changed();
        }
    }

    /// Open a row from the tree (a name click): a folder re-roots the deck; an **archive** row
    /// (task #108) opens the archive as its own deck (the door / File-open path). The row's
    /// kind is taken from the resident tree — never re-classified from the extension, so a real
    /// folder that happens to be named `foo.zip` still opens as a folder.
    pub fn fs_tree_open(&mut self, path: PathBuf) {
        let is_archive = self
            .fs_tree
            .as_ref()
            .is_some_and(|t| t.is_archive_row(&path));
        if is_archive {
            self.open_disk_target(crate::folder_tree::DiskTarget::Archive(path));
        } else {
            self.open_dir(path);
        }
    }

    /// The up-affordance: re-root the tree one level higher (kicks its read next tick).
    pub fn fs_tree_extend_up(&mut self) {
        if self.fs_tree.as_mut().is_some_and(|t| t.extend_root_up()) {
            self.emit_panels_changed();
        }
    }

    /// Hide the folder tree (clears its quad + interactive state). The open/closed
    /// *state* stays with the caller — [`toggle_folder_tree`](Self::toggle_folder_tree)
    /// flips it.
    pub fn hide_folder_tree(&mut self) {
        if let Some(a) = self.renderer.as_mut() {
            a.set_tree(None, 0);
        }
        self.folder_tree_sig = None;
        self.folder_tree_panel = None;
        self.draw();
    }

    /// The interactive tree hit under a physical-px cursor point: a clickable folder
    /// row (one with a target) or a paging marker. The panel sits `margin` px in
    /// from the top-left, so screen rects derive from the live geometry — resize-
    /// and DPI-proof, like the other interactive overlays.
    pub fn folder_tree_hit(&self, x: f32, y: f32) -> Option<hud::TreeHit> {
        if !self.folder_tree_open {
            return None;
        }
        let p = self.folder_tree_panel.as_ref()?;
        let (x0, y0) = (p.margin as f32, p.margin as f32);
        for (hit, r) in &p.hits {
            let rect = [
                x0 + r[0] as f32,
                y0 + r[1] as f32,
                x0 + (r[0] + r[2]) as f32,
                y0 + (r[1] + r[3]) as f32,
            ];
            if point_in_rect(rect, x, y) {
                return match hit {
                    hud::TreeHit::Row(i) => p.targets.get(*i)?.is_some().then_some(*hit),
                    _ => Some(*hit),
                };
            }
        }
        None
    }

    /// Track the pointer over the tree: on a hover **transition** (enter/leave/move
    /// between hits), re-render the panel from its cached rows so the hovered row's
    /// band lights up — the chip-hover pattern; nothing runs per-move or per-frame.
    pub fn update_tree_hover(&mut self) {
        let hovered = self
            .last_cursor
            .and_then(|[x, y]| self.folder_tree_hit(x, y));
        let Some(panel) = self.folder_tree_panel.as_ref() else {
            return;
        };
        if panel.hovered == hovered {
            return;
        }
        let Some(panel) = self.folder_tree_panel.take() else {
            return;
        };
        self.push_folder_tree(panel.rows, panel.targets, panel.page, hovered);
    }

    /// A left-press over the folder tree: a "… n more" marker pages the window; a
    /// folder row opens that folder — full Open Folder semantics, the same plan the
    /// picker/drop path builds. Returns whether the press was consumed (the shells'
    /// click ladders fall through to drag-to-pan otherwise).
    pub fn folder_tree_click(&mut self) -> bool {
        let Some(hit) = self
            .last_cursor
            .and_then(|[x, y]| self.folder_tree_hit(x, y))
        else {
            return false;
        };
        match hit {
            hud::TreeHit::PageUp | hud::TreeHit::PageDown => {
                let delta = if hit == hud::TreeHit::PageUp { -1 } else { 1 };
                let Some(panel) = self.folder_tree_panel.take() else {
                    return true;
                };
                // Render the new page without hover; the immediate re-check lights
                // whatever now sits under the still cursor.
                self.push_folder_tree(panel.rows, panel.targets, panel.page + delta, None);
                self.update_tree_hover();
                true
            }
            hud::TreeHit::Row(i) => {
                let target = self
                    .folder_tree_panel
                    .as_ref()
                    .and_then(|p| p.targets.get(i).cloned().flatten());
                match target {
                    Some(crate::folder_tree::TreeTarget::Dir(dir)) => self.open_dir(dir),
                    Some(crate::folder_tree::TreeTarget::Scope(prefix)) => {
                        self.rescope_archive(prefix)
                    }
                    None => {}
                }
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_core_impl::test_support::{test_core};
    use crate::contract::{CoreEvent};

    #[test]
    fn native_tree_visibility_and_safe_activate() {
        let mut core = test_core();
        assert!(!core.tree_panel_visible(), "off by default");
        core.native_tree = true;
        assert!(!core.tree_panel_visible(), "closed → not visible");
        core.folder_tree_open = true;
        assert!(core.tree_panel_visible(), "open + native → visible");
        core.panels.hidden = true;
        assert!(!core.tree_panel_visible(), "Tab-hidden → not visible");
        core.panels.hidden = false;
        // A tick signals the host on the visibility transition (no hud needed for the diff).
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "the tree's visibility change signals the host"
        );
        // Activate with nothing derived is a safe no-op (no target).
        core.tree_activate(0);
        // Winit (native_tree off) is never native-visible.
        core.native_tree = false;
        assert!(!core.tree_panel_visible());
    }

    /// Regression (task #129): opening an ancestor ABOVE the resident tree root must re-root the
    /// tree even when the current image still sits under the old root — otherwise Folders shows a
    /// tree that can't browse the newly-opened deck. The breadcrumb path bar makes this reachable.
    #[test]
    fn opening_an_ancestor_above_the_tree_root_rebuilds_the_tree() {
        let mut core = test_core();
        core.native_tree = true;
        // Deck at `/A/B/C`, current image `/A/B/C/one.jpg`.
        core.source = Arc::new(FsSource::new(vec![PathBuf::from("/A/B/C/one.jpg")]));
        core.playlist = Playlist::new(1, 0).with_cursor(0);
        core.displayed_item = Some(0);
        core.root = PathBuf::from("/A/B/C");

        core.ensure_fs_tree();
        let tree_root = core.fs_tree.as_ref().unwrap().root().to_path_buf();
        // A fresh tree roots one level above the deck root so the deck shows among its siblings.
        assert_eq!(tree_root, PathBuf::from("/A/B"));
        assert!(core.root.starts_with(&tree_root), "deck root under tree root");

        // The breadcrumb opens ancestor `/A`: the recursive scan makes `/A` the deck root, but
        // the first/current image is still under the old tree root `/A/B`. The current-folder-only
        // check would keep the stale tree; the deck-root check forces a rebuild.
        core.root = PathBuf::from("/A");
        core.ensure_fs_tree();
        let tree_root = core.fs_tree.as_ref().unwrap().root().to_path_buf();
        assert!(
            core.root.starts_with(&tree_root),
            "tree re-rooted so the new deck root {:?} is browsable under {:?}",
            core.root,
            tree_root
        );
    }

    /// Regression (task #129): the breadcrumb path bar must track the displayed photo's folder
    /// even with only the Thumbnails tab open (Folders hidden) — including the async cache-miss
    /// path where `displayed_item` moves with no `advance`/`drive_fs_tree` marker. The per-tick
    /// snapshot diff re-signals the host on a folder change.
    #[test]
    fn the_breadcrumb_re_signals_on_a_folder_change_with_only_thumbnails_open() {
        let mut core = test_core();
        core.native_tree = true;
        // Two photos in DIFFERENT folders.
        core.source = Arc::new(FsSource::new(vec![
            PathBuf::from("/A/one.jpg"),
            PathBuf::from("/B/two.jpg"),
        ]));
        core.playlist = Playlist::new(2, 0).with_cursor(0);
        core.displayed_item = Some(0);
        core.root = PathBuf::from("/A");
        // The Thumbnails tab is open; Folders is NOT the visible tab, so `drive_fs_tree` is idle.
        core.folder_tree_open = true;
        core.left_tab = crate::overlay::LeftTab::Thumbnails;
        assert!(core.thumbs_visible());
        assert!(!core.tree_panel_visible(), "Folders is not the visible tab");

        // First tick establishes the snapshot at folder `/A`.
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert_eq!(core.last_breadcrumb_snap, Some(PathBuf::from("/A")));

        // Move the displayed item to a photo in `/B` WITHOUT any nav marker — the async
        // present path. The tick's snapshot diff must catch the folder cross and re-signal.
        core.displayed_item = Some(1);
        core.effects.clear();
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert_eq!(core.last_breadcrumb_snap, Some(PathBuf::from("/B")));
        assert!(
            core.effects
                .iter()
                .any(|e| matches!(e, contract::CoreEffect::PanelsChanged)),
            "a folder change re-signals the host even with only Thumbnails open"
        );

        // Closing the strip forgets the snapshot so re-showing re-signals a fresh pull.
        core.folder_tree_open = false;
        core.handle(CoreEvent::Tick(std::time::Instant::now()));
        assert_eq!(core.last_breadcrumb_snap, None);
    }
}
