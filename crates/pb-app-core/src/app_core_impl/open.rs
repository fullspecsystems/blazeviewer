//! **Opening things** — the `AppCore` methods behind Open, the folder/archive climb, and
//! deck rescoping (task #125).
//!
//! `open_plan` is the one routing point: a `Source` becomes a directory scan
//! ([`super::dir_scan`]) or an archive open ([`super::archive_open`]). Both of those moved
//! out first, so the routing follows them rather than sitting in the parent alone.
//!
//! `Alt+Up` climbs out of an archive via `open_parent_cmd`, which anchors on
//! `source.container()` — that is how you reach the next door in a folder.
//!
//! Every method moved byte-identically; see `scripts/verify-pure-move.py`.

use super::*;

impl AppCore {
    /// Route an opened source (NS0 5.6 Step 3c) — the entry point the picker / drag-drop / a
    /// deferred launch funnel through. An **archive** or a **folder scan** starts its off-thread
    /// worker via an effect (`BeginArchiveOpen` / `BeginDirScan`; the host owns the thread +
    /// progress dialog + generation, and feeds results back as `ArchiveResolved` / `ScanBatch`).
    /// A finite **explicit list** has no directory walk, so it resolves inline and installs now.
    pub fn open_plan(&mut self, source: pb_core::open::Source, cursor: pb_core::open::Cursor) {
        use pb_core::open::Source;
        // Perf (PB_PERF): start the open→first-photo clock now, *before* the archive/scan
        // worker — that wait (central-directory read, a networked ZIP, a big scan) is part of
        // what the user feels, so metric 1 has to include it.
        self.perf.open_begin(self.now);
        // Any explicit open breaks an Open-Parent climb — the next ⌘↑ restarts from the
        // current folder. `open_parent_cmd` re-sets the anchor *after* it calls through here.
        self.climb_anchor = None;
        match source {
            Source::Archive(path) => {
                self.effects.push(contract::CoreEffect::BeginArchiveOpen {
                    path,
                    password: None,
                });
            }
            src @ Source::Scan { .. } => {
                self.effects.push(contract::CoreEffect::BeginDirScan {
                    source: src,
                    cursor,
                });
            }
            src @ Source::Explicit(_) => {
                let r = crate::scan::resolve_playlist(&src, &cursor, self.settings.show_archives);
                if r.source.is_empty() {
                    eprintln!("{}: no supported images in that selection", crate::APP_NAME);
                    return;
                }
                self.rebuild_playlist(r.source, r.root, r.scan_root, r.recursive, r.start);
            }
        }
    }

    /// Open `dir` exactly like choosing it in the Open Folder picker or dropping it
    /// on the window — the shared plan (recursive per the launch policy), so tree
    /// clicks and the Go commands can't drift from the canonical open path.
    pub fn open_dir(&mut self, dir: PathBuf) {
        self.open_dir_at(dir, None);
    }

    /// Like [`open_dir`](Self::open_dir), but land the deck cursor on `at` (a file in the
    /// folder) instead of the first item — Open Parent uses it to land on the archive **door**
    /// you climbed out of (task #108). `at` must be a file the scan will actually surface (an
    /// archive is only surfaced when `show_archives` is on), or the streaming scanner would gate
    /// its first batch on an unreachable target and show nothing — the caller gates that.
    pub fn open_dir_at(&mut self, dir: PathBuf, at: Option<PathBuf>) {
        let plan = pb_core::open::plan(pb_core::open::LaunchInput::Directory(dir));
        let cursor = match at {
            Some(p) => pb_core::open::Cursor::At(p),
            None => plan.cursor,
        };
        self.open_plan(plan.source, cursor);
    }

    /// Open a typed [`DiskTarget`](crate::folder_tree::DiskTarget) (task #108): a folder as a
    /// folder deck, or an archive as its own deck (the full door / File-open path — password
    /// prompt, RAM pre-flight, progress). The shared activation for the Go-sibling walk and the
    /// tree rows, so neither can drift on how an archive opens.
    pub fn open_disk_target(&mut self, target: crate::folder_tree::DiskTarget) {
        match target {
            crate::folder_tree::DiskTarget::Directory(p) => self.open_dir(p),
            crate::folder_tree::DiskTarget::Archive(p) => self.open_plan(
                pb_core::open::Source::Archive(p),
                pb_core::open::Cursor::First,
            ),
        }
    }

    /// Re-scope the archive deck to the entries under the internal folder
    /// `prefix` (`""` = back to the whole archive) — the archive analog of
    /// [`open_dir`](Self::open_dir), sharing its rebuild semantics (cursor to
    /// the first item, caches dropped). Pure in-RAM: the full source is wrapped
    /// in a `ScopedSource` (one pass over the resident name list), never
    /// re-opened — a solid 7z's eager decode is paid once, and an unlocked
    /// encrypted archive stays unlocked. Silent no-op on a disk deck.
    pub fn rescope_archive(&mut self, prefix: String) {
        let Some(scope) = self.archive_scope.clone() else {
            return;
        };
        let source: Arc<dyn ItemSource> = if prefix.is_empty() {
            Arc::clone(&scope.full)
        } else {
            Arc::new(pb_source::ScopedSource::new(
                Arc::clone(&scope.full),
                &prefix,
            ))
        };
        // A scope only ever comes from a tree row / sibling step, which derive
        // from the entry names — so it can't be empty; the guard is belt and
        // braces (rebuild_playlist refuses an empty deck anyway, un-stamped).
        if source.is_empty() {
            return;
        }
        self.rebuild_playlist(source, self.root.clone(), None, false, 0);
        self.archive_scope = Some(crate::ArchiveScope {
            full: scope.full,
            prefix,
        });
    }

    /// Go ▸ parent folder (⌘↑ / Alt+↑ — Finder's Enclosing Folder idiom): open the
    /// deck anchor's parent. A disk deck's anchor is the opened root. An archive
    /// deck scoped to an internal folder steps the scope up one level first
    /// (`a/b` → `a` → the whole archive); from the archive root, "up" opens the
    /// folder on disk containing the archive file. Silent no-op with nothing
    /// open or at a filesystem root.
    pub fn open_parent_cmd(&mut self) {
        if let Some(scope) = &self.archive_scope {
            if !scope.prefix.is_empty() {
                let parent = crate::folder_tree::folder_of(&scope.prefix).to_string();
                self.rescope_archive(parent);
                return;
            }
        }
        // An archive deck at its root goes up to the disk folder *containing* the archive.
        // A normal disk deck **climbs one level per press**: the first ⌘↑ goes up from the
        // current photo's folder; each subsequent ⌘↑ continues up from the folder the last
        // one opened (`climb_anchor`) — not the current photo's folder, which stays at the
        // deepest level (a parent with no direct photos re-lands it there), so anchoring on
        // it would get stuck oscillating. The climb resets the moment any other open happens.
        let anchor = self
            .source
            .container()
            .map(Path::to_path_buf)
            .or_else(|| self.climb_anchor.clone())
            .or_else(|| self.current_folder_abs())
            .unwrap_or_else(|| self.root.clone());
        if anchor.as_os_str().is_empty() {
            return;
        }
        if let Some(par) = anchor.parent().filter(|p| !p.as_os_str().is_empty()) {
            let par = par.to_path_buf();
            // Climbing out of an archive **root**, land on that archive's door (task #108), so
            // `space` continues past it instead of restarting at the folder's first item — the
            // owner's "more consistent" fix. `self.root` is the archive file for an archive deck.
            // Gate on `show_archives`: with archives hidden the door isn't in the scan, and the
            // streaming scanner would wait for that unreachable target before showing anything
            // (Codex review) — so fall back to the first item there.
            let at = (self.archive_scope.is_some() && self.settings.show_archives)
                .then(|| self.root.clone());
            self.open_dir_at(par.clone(), at); // clears climb_anchor (via open_plan)…
            self.climb_anchor = Some(par); // …then remembers this rung for the next ⌘↑.
        }
    }

    /// Go ▸ previous / next folder (`dir` = ∓1; ⌘←/⌘→ / Alt+←/→): open the nearest
    /// sibling directory **with photos** in that direction — photo-less siblings
    /// are skipped (#49: name-adjacency dead-ended behind a "No supported images"
    /// modal, and since a failed open never moves the root, re-pressing retried
    /// the same empty folder forever). The search runs on the tree-io worker
    /// (per-candidate probes can walk entire subtrees, and even the `is_dir`
    /// stat can stall on a dead share); `tick` opens the target when it lands,
    /// or toasts when nothing that way has photos. A rapid re-press supersedes
    /// AND cancels the in-flight search. On an archive deck scoped to an
    /// internal folder, step to the adjacent sibling *prefix* in the same
    /// sorted row the tree shows — pure in-RAM, no worker, and never photo-less
    /// by construction (archive folders derive from image entry names); the
    /// row's ends toast the same way. Silent no-op with nothing open.
    pub fn open_sibling_cmd(&mut self, dir: i32) {
        // Stepping to a sibling folder ends an Open-Parent (⌘↑) climb — the next ⌘↑ restarts
        // from the folder you land on, not the stale climb rung.
        self.climb_anchor = None;
        if let Some(scope) = &self.archive_scope {
            // Scoped into an internal folder: step the archive's internal sibling folders
            // (in-RAM over the entry names), exactly as before.
            if !scope.prefix.is_empty() {
                let full = Arc::clone(&scope.full);
                let names = (0..full.len()).map(|i| full.name(i));
                match crate::folder_tree::sibling_scope(names, &scope.prefix, dir) {
                    Some(sib) => self.rescope_archive(sib),
                    None => self.show_toast("No more folders with images"),
                }
                return;
            }
            // At the archive **root** (the whole-archive deck): step through the archive's own
            // internal folders first — jump the cursor to the next internal-folder boundary, like
            // Go Next Folder on a disk deck (task #108). Only when there is **no more internal
            // folder** that way do we step to the adjacent archive on disk.
            if let Some(idx) = self.archive_adjacent_folder_item(dir) {
                self.stop_playback();
                self.playlist.jump_to(idx);
                self.target_item = self.playlist.current();
                self.try_present_target();
                self.request_prefetch();
                return;
            }
            // No more internal folders in that direction → the adjacent archive on disk (anchor on
            // the archive file, archives-only, off-thread — the containing folder's `read_dir` can
            // stall on a share). Only when Show Archives is on; otherwise nothing to step to.
            if self.settings.show_archives {
                self.tree_io = Some(crate::folder_tree::spawn_sibling(
                    self.root.clone(),
                    dir,
                    true, // show_archives
                    true, // archives_only — flick archive → archive
                ));
            } else {
                self.show_toast("No more folders with images");
            }
            return;
        }
        if self.source.container().is_some() {
            return;
        }
        // "Next/previous photo, but by folder": jump within the deck to the next/previous
        // folder *boundary* in the deck's (tree-order) sequence — entering subfolders,
        // stepping siblings, or climbing back up, exactly as the traversal runs. Instant,
        // and it can never hit "No photos" (every jump lands on a real deck item).
        if let Some(idx) = self.adjacent_folder_item(dir) {
            self.stop_playback();
            self.playlist.jump_to(idx);
            self.target_item = self.playlist.current();
            self.try_present_target();
            self.request_prefetch();
            return;
        }
        // No adjacent folder in the deck. A multi-folder deck means you're at its first /
        // last folder — toast (HUD-gated, so the host shows it). A single-folder deck opens
        // the next folder *on disk* (the disk sibling of the current folder, skipping
        // photo-less ones), so ⌘←/→ still browses when the deck is just one folder.
        if self.deck_spans_multiple_folders() {
            self.show_toast("No more folders");
            return;
        }
        let anchor = self
            .current_folder_abs()
            .unwrap_or_else(|| self.root.clone());
        if anchor.as_os_str().is_empty() {
            return;
        }
        self.tree_io = Some(crate::folder_tree::spawn_sibling(
            anchor,
            dir,
            self.settings.show_archives,
            false, // archives_only=false — a folder deck steps to folder-or-archive siblings
        ));
    }

    /// The source index at the next (`dir > 0`) / previous folder **boundary** in the deck
    /// sequence: forward → the first item after the current whose folder differs (the start
    /// of the next folder-run); backward → the start of the previous folder-run. `None` at
    /// the deck's last / first folder. Pure, RAM-only — the deck is already tree-ordered.
    fn adjacent_folder_item(&self, dir: i32) -> Option<usize> {
        let n = self.source.len();
        let c = self.displayed_item.filter(|&c| c < n)?;
        let folder = |i: usize| self.source.path(i).and_then(Path::parent);
        let cur = folder(c)?.to_path_buf();
        if dir > 0 {
            (c + 1..n).find(|&i| folder(i) != Some(cur.as_path()))
        } else {
            // Walk back to the start of the current run, then to the start of the one before.
            let mut s = c;
            while s > 0 && folder(s - 1) == Some(cur.as_path()) {
                s -= 1;
            }
            if s == 0 {
                return None; // already in the deck's first folder-run
            }
            let prev = folder(s - 1)?.to_path_buf();
            let mut p = s - 1;
            while p > 0 && folder(p - 1) == Some(prev.as_path()) {
                p -= 1;
            }
            Some(p)
        }
    }

    /// The deck index at the next (`dir > 0`) / previous internal-folder boundary of an
    /// **archive** deck — the archive analog of [`adjacent_folder_item`](Self::adjacent_folder_item),
    /// grouping by the entry name's internal folder ([`folder_of`](crate::folder_tree::folder_of))
    /// since archive entries have no filesystem path (task #108). `None` at the deck's first /
    /// last internal folder — the caller then steps to an adjacent archive on disk.
    fn archive_adjacent_folder_item(&self, dir: i32) -> Option<usize> {
        let n = self.source.len();
        let c = self.displayed_item.filter(|&c| c < n)?;
        let folder = |i: usize| crate::folder_tree::folder_of(self.source.name(i)).to_string();
        let cur = folder(c);
        if dir > 0 {
            (c + 1..n).find(|&i| folder(i) != cur)
        } else {
            let mut s = c;
            while s > 0 && folder(s - 1) == cur {
                s -= 1;
            }
            if s == 0 {
                return None; // already in the archive's first internal folder
            }
            let prev = folder(s - 1);
            let mut p = s - 1;
            while p > 0 && folder(p - 1) == prev {
                p -= 1;
            }
            Some(p)
        }
    }

    /// Whether the deck's photos span more than one folder (early-exits on the second).
    fn deck_spans_multiple_folders(&self) -> bool {
        let mut first: Option<&Path> = None;
        for i in 0..self.source.len() {
            if let Some(f) = self.source.path(i).and_then(Path::parent) {
                match first {
                    None => first = Some(f),
                    Some(f0) if f0 != f => return true,
                    _ => {}
                }
            }
        }
        false
    }

    /// Request the native picker (`O` = file(s), `Shift+O` = folder). Computes the start
    /// directory from live state (core), then emits an [`CoreEffect::OpenFilePanel`] /
    /// [`OpenFolderPanel`](CoreEffect::OpenFolderPanel); the shell runs the modal panel in
    /// the drain and re-enters via [`App::finish_picker`]. Modal — it blocks the event loop
    /// while open, which is fine: the app isn't blazing through photos with a dialog up.
    pub fn open_picker(&mut self, folder: bool) {
        let fallback = default_picker_dir();
        let mut start_dir = picker_start_dir(
            self.settings.picker_dir.as_deref(),
            self.source.container(),
            self.scan_root.as_deref(),
            &self.root,
            self.settings.last_folder.as_deref(),
            &fallback,
        );
        // If the chosen folder no longer exists (e.g. a pinned folder was deleted or
        // unmounted), use the safe default rather than letting the OS dialog surface its
        // own remembered last folder.
        if !start_dir.is_dir() {
            start_dir = fallback;
        }
        self.effects.push(if folder {
            contract::CoreEffect::OpenFolderPanel { start_dir }
        } else {
            contract::CoreEffect::OpenFilePanel { start_dir }
        });
    }
}
