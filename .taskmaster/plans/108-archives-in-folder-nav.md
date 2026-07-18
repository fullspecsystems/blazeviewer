# #108 — Archives in the folder tree + Go navigation (zipper icon)

**Status:** planned (revised after Codex review) · **Owner-decided** 2026-07-17 · gated on
`settings.show_archives` · builds on #104 (doors) / #105 (door card).

## Goal

When **Show Archives** is on, archives behave like folders while browsing:

1. **Go ▸ Next/Prev Folder (`Alt+←/→`) steps across sibling archives.** Inside an archive
   scoped to an internal folder, `Alt+←/→` steps the archive's internal sibling folders
   (unchanged). At the **root** of an archive, `Alt+←/→` opens the adjacent **archive on disk**
   in the containing folder.
2. **Open Parent (`Alt+↑`) preserves position** — climbing out of an archive lands the deck
   cursor on **that archive's door**, so `space` continues to the next item.
3. **The `⇧F` folder tree lists archives** with a **zipper icon**; clicking one **opens** the
   archive (`open_plan(Source::Archive)`), not a re-root.

All gated on `settings.show_archives`.

## Owner decisions (2026-07-17)

- In-archive `Alt+←/→` = internal sibling folders as today; **from the archive root** it steps
  to adjacent archives on disk.
- Open Parent **preserves position** (land on the archive's door). The consistent design.
- **Do both surfaces**, get it all done.

## Codex review — confirmed invariants (design rests on these)

- **`self.root` is the archive file path** for an archive deck (`scan.rs:84` `archive_resolved`
  sets `root: path.to_path_buf()`; `apply_archive` installs it unchanged; re-scope preserves it).
- **`ItemSource::container()` returns the opened archive file** (ZIP/7z/TAR/RAR return their
  file path; `ScopedSource` delegates; `FsSource` = `None`). So Open-Parent's existing
  `container().parent()` (`app_core_impl.rs:2805`) is already the archive's containing folder —
  the door-preserve just needs the right **cursor**.

## Codex review — corrections folded in

1. **Open-Parent cursor must be gated on `show_archives`.** `open_dir` uses `Cursor::First`
   (`app_core_impl.rs:2748`). A door-preserve passes `Cursor::At(archive_path)`. **But with
   `show_archives == false` the archive is filtered out of the scan, and the streaming scanner
   gates its interval batches on finding the target (`scan.rs:559`) — so `Cursor::At` on a
   hidden archive stalls the first-photo display.** Use `Cursor::At(archive_path)` **only when
   `show_archives` is on**; else keep `Cursor::First`. (You can still open an archive via File▸
   Open with archives hidden, so this path is reachable.)
2. **Archives ARE deck items** (correcting the earlier plan). With Show Archives on, archive
   files are admitted to the `FsSource` playlist as door items (`scan.rs:138`
   `admits_library_file`), so `adjacent_folder_item` (`app_core_impl.rs:2888`) **already** groups
   them and folder-deck `Alt+→` **already** lands on an archive door in the *current deck*. The
   disk sibling walk (below) is only needed for an archive **beside the deck root** (not itself
   in the deck) — a single-folder deck's disk sibling.
3. **The sibling result is opened as a directory at `app_core_impl.rs:1702`** —
   `TreeIoResult::Sibling.target: Option<PathBuf>` (`folder_tree.rs:549`, from
   `sibling_with_photos` `:654`) is fed unconditionally to `self.open_dir(d)`. **That is the
   exact re-root-wrongly site** an archive target must not hit. The worker also **rejects an
   archive root** (`if !root.is_dir()`, `folder_tree.rs:662`) and enumerates `subdirs(parent)`
   only — so an archive-root sibling mode / typed walker is required.
4. **Two tree models — the shipping one is the native `FsTree`, not the HUD.** `TreeTarget::Open`
   only helps the legacy HUD tree (`tree_activate` `:2473` / `folder_tree_click` `:2709`). The
   **shipping** `⇧F` tree flows: winit row-click (`panels_ui.rs:3152`) → `PanelAction::TreeOpen(PathBuf)`
   (`main.rs:2273`) → **`fs_tree_open` which always calls `open_dir`** (`app_core_impl.rs:2634`);
   macOS = indexed activation (`mac-ffi:849`). Phase 2 targets `FsTree`.
5. **Live-setting invalidation.** `FsTree` caches loaded children permanently; `ensure_fs_tree`
   rebuilds only when the current folder leaves its root (`app_core_impl.rs:2523`). Toggling
   Show Archives must be folded into the tree's identity (that rebuild condition **and** the
   legacy full-tree signature `:2252`), pending old-mode reads discarded — else archives stay
   stale in an already-loaded tree.
6. **macOS Swift is in scope for Phase 2** (the plan had omitted it): the archive-row kind must
   propagate `fs_tree::Row` → winit `tree_row()` (`Icon::Archive`, already
   `pb_ui::Icon::Archive → file-zipper` at `pb-ui/src/icon.rs:71`) → `TreeRowFfi` (needs a kind
   field, `mac-ffi:157`) → Swift `FolderTreeRow` + icon (`FolderTreePanel.swift:159`, uses
   `doc.zipper`).
7. **Icon locations:** `pb-app/src/icon.rs` does **not** exist. The *shipping* trees use the
   native icons above (winit `pb_ui::Icon::Archive`; macOS `doc.zipper`). The legacy HUD tree, if
   still updated, lives in `pb-hud/src/icon.rs:17` + `render_tree` (`hud.rs:998/1096/1181`).
8. **`rows_from_disk` is invasive** (legacy HUD only): its shared assembler takes
   `Fn(&Path)->Vec<String>` and maps every row to `TreeTarget::Dir` (`folder_tree.rs:454`);
   interleaving archive leaves means typing the per-level lists/roles while keeping
   `rows_from_paths` folder-only. Deprioritised — the native `FsTree` is what ships.

## Design (revised — Codex's "simpler shape")

### The backbone: one typed disk target + one router

```rust
// pb-app-core (folder_tree.rs or a small nav module)
pub enum DiskTarget {
    Directory(PathBuf),      // open as a folder deck (open_dir)
    Archive(PathBuf),        // open the archive (open_plan(Source::Archive))
}
```
Used for **directory children, sibling-worker results, and `FsTree` rows**. One activation
router enforces the gate and picks the open:
```rust
impl AppCore {
    fn open_disk_target(&mut self, t: DiskTarget) {
        match t {
            DiskTarget::Directory(p) => self.open_dir(p),
            // Archives only ever *become* a target when show_archives is on (readers gate
            // them), so this is safe; the door open is the full path (password + RAM pre-flight).
            DiskTarget::Archive(p) => self.open_plan(Source::Archive(p), Cursor::First),
        }
    }
}
```
`ArchiveKind` need not ride the target (the classifier resolves it at open); carry it only if a
row needs to display it.

### A. Typed directory reader (opt-in; #104 trap intact)

`subdirs(dir) -> Vec<String>` stays **folder-only** (shared with the folder-only sibling walk).
Add `dir_children(dir, show_archives) -> Vec<DiskTarget>` (folders always; archive files too when
`show_archives`, classified via `pb_source::archive_kind`, sorted/interleaved by name like
`subdirs`, hidden filtered).

### B. Open Parent preserves the door (gated)

`open_parent_cmd`: the scoped-archive branch (`prefix` non-empty) re-scopes up, unchanged. When
climbing out of an archive **root**, capture the archive path (`self.root`) and open the
containing folder with `Cursor::At(archive_path)` **iff `show_archives`** (else `Cursor::First`,
per correction 1). Add `open_dir_at(dir, cursor)`; `open_dir` delegates with `First`.
`climb_anchor` unchanged. Scope: archive door only (a folder has no single door item).

### C. Archive-root sibling stepping + folder-deck disk siblings — one typed walker

Replace the sibling worker's bare `PathBuf` with `Option<DiskTarget>` end-to-end:
- `sibling_with_photos`/`spawn_sibling` → `TreeIoResult::Sibling.target: Option<DiskTarget>`;
  the landing site (`app_core_impl.rs:1702`) calls `self.open_disk_target(t)` instead of
  `open_dir(d)`.
- The worker gains a **typed walk** over the parent via `dir_children(parent, show_archives)`:
  a sibling may be a photo-folder (existing `dir_has_image` gate) **or** an archive
  (`DiskTarget::Archive`). This handles both (a) a folder-deck's disk sibling being an archive,
  and (b) an **archive root**: anchor on `self.root` (the archive file), walk its parent's
  `dir_children` for the adjacent **archive**, open it.
- Route the archive-root case in `open_sibling_cmd`: `archive_scope` present **and
  `prefix.is_empty()`** → spawn the typed sibling walk anchored on `self.root` (instead of the
  in-RAM `sibling_scope`, which correctly only handles a scoped internal row). The
  `!root.is_dir()` guard must allow an archive-file anchor.
- **Nav decision (asymmetry, pinned with tests):** from an **archive root** step to the
  adjacent *archive* (owner's words: "switch to adjacent archives"); from a **folder** the
  in-deck `adjacent_folder_item` already crosses folder→archive doors, and the disk walk (only
  for a single-folder deck) accepts folder-or-archive siblings. If the owner later wants fully
  symmetric mixed stepping from an archive root, it's a one-line filter relaxation.

### D. The tree (Phase 2) — native `FsTree` first

- **`fs_tree::Node.children`** becomes typed (`Directory`/`Archive`) so an archive child is a
  **leaf** (`loading=false`, `has_children=false`, no expansion) — `drive_fs_tree` must not
  schedule `subdirs()` for it (`app_core_impl.rs:2583`), and `push_rows`'s optimistic chevron
  (`fs_tree.rs:219`) must not apply.
- Kind propagates to **`fs_tree::Row`** → winit **`tree_row()`** (draws `pb_ui::Icon::Archive`,
  emits a `TreeOpen`-archive activation) and **`TreeRowFfi`** (+kind) → Swift **`FolderTreeRow`**
  (`doc.zipper`).
- **Activation:** `fs_tree_open` (`app_core_impl.rs:2634`) and the macOS indexed activation
  (`mac-ffi:849`) route an archive row through `open_disk_target(Archive)` instead of `open_dir`.
  Since `PanelAction::TreeOpen(PathBuf)` carries only a path, either add the kind to the panel
  action or have `fs_tree_open` re-classify the path (it already knows the row) — prefer the
  typed row so no re-`stat`.
- **Live invalidation (correction 5):** include `show_archives` in `ensure_fs_tree`'s rebuild
  identity (`:2523`) and the legacy full-tree signature (`:2252`); on a toggle, rebuild
  `fs_tree`/`fs_tree_io` and discard pending old-mode reads.
- **Legacy HUD tree** (`rows_from_disk`/`TreeRow`/`render_tree`): update only if it's still a
  live surface — deprioritised since the native trees ship. If updated: `TreeRow` gains an
  `archive` marker + a `pb-hud/src/icon.rs` zipper raster (`hud.rs:998/1096/1181`), and
  `rows_from_disk`'s assembler is typed per correction 8.

## Files (indicative)

- `crates/pb-app-core/src/folder_tree.rs` — `DiskTarget`, `dir_children`, typed
  `sibling_with_photos`/`spawn_sibling` result, `sibling_scope` unchanged.
- `crates/pb-app-core/src/app_core_impl.rs` — `open_disk_target`, `open_dir_at`,
  `open_parent_cmd` (gated door), `open_sibling_cmd` (archive-root walk), the sibling landing
  (`:1702`), `fs_tree_open`, `ensure_fs_tree`/full-tree signature (invalidation),
  `drive_fs_tree` (archive leaves).
- `crates/pb-app-core/src/fs_tree.rs` — typed children/rows; archive leaf.
- `crates/pb-app/src/{panels_ui.rs,main.rs}` — winit `tree_row` zipper (`pb_ui::Icon::Archive`)
  + archive activation through `PanelAction`.
- `crates/pb-mac-ffi/src/lib.rs` — `TreeRowFfi` kind; indexed activation → archive open.
- `mac/Sources/BlazeViewerMac/FolderTreePanel.swift` — `doc.zipper` for archive rows.
- (legacy, optional) `crates/pb-hud/src/{hud.rs,icon.rs}`, `folder_tree.rs::rows_from_disk`.
- `CHANGELOG.md`.

## Tests (pb-app-core, pure)

- `dir_children`: folders always; archives only when `show_archives`; interleaved by name;
  hidden filtered; `subdirs` still folder-only.
- **Open-Parent door:** archive root + `show_archives` → the resulting plan/landing uses
  `Cursor::At(archive_path)`; with `show_archives` **off** → `Cursor::First` (no stall).
- **Archive-root sibling:** fixture folder with two archives → `open_sibling_cmd(±1)` at an
  archive root opens the adjacent archive; a **scoped** archive still steps internal folders;
  both gated off when `show_archives` is false.
- **Folder-deck sibling** to an archive (single-folder-deck disk walk).
- Sibling landing routes `DiskTarget::Archive` through `open_disk_target` (emits
  `BeginArchiveOpen`, not a folder scan).
- (Phase 2) `FsTree` emits a leaf archive row (no chevron, no `subdirs` schedule) only when
  `show_archives`; toggling the setting rebuilds the tree.

## Phasing

- **Phase 1 (keyboard, unit-testable in `pb-app-core`):** `DiskTarget` + `open_disk_target`;
  `dir_children`; Open-Parent gated door; typed sibling walker (archive-root + folder-deck);
  the `:1702` landing route. Ship + test before the tree.
- **Phase 2 (native tree, both shells + Swift):** typed `FsTree` rows/leaves + activation +
  the zipper icon (winit `pb_ui::Icon::Archive`, macOS `doc.zipper`) + live-setting
  invalidation. Legacy HUD tree only if still live.

## Pinned decisions (were Codex open questions)

- Archive-root stepping is **archives-only** (owner's phrasing); folder stepping crosses to
  archive doors via the existing in-deck path. Asymmetry is deliberate — pinned by the tests
  above; a mixed walker is a one-line relaxation if wanted.
- Multi-folder decks still toast at `:2863` before the disk walker (existing behaviour, kept).
- Open-Parent door-preserve is **archive-only** (folders have no single door item).
