# Folder Hierarchy Navigation — HUD tree overlay (⇧F)

_Draft plan 2026-07-03, from discussion. Scope is split into an explicit
phase 1 (render-only proof) and phase 2 (interactive navigation), per the
owner's ask to see it rendering before building selection._

> **Status (2026-07-03): phase 1 implemented** (tasks.json #47, in review —
> owner smoke pending). Deltas from the plan below, reflecting the post-NS0
> architecture and owner feedback on the first draft: the rasterizer is
> `pb-hud::Hud::render_tree` (not the old shell-side hud.rs) with FA solid
> folder/folder-open glyphs per the owner's ask; the panel is a **new
> top-left renderer layer** (`Renderer::set_tree` — the info panel's
> bottom-right `overlay_margin` inset, mirrored, per the owner: concentric,
> pinned to the corner); derivation lives in `pb-app-core/src/folder_tree.rs`
> and is **anchored at the root PhotoBlaze opened** (owner: never walk above
> it): root heading → the ancestor chain down to the current folder ("you are
> here"; chains deeper than 4 collapse their middle into a dim "…" marker
> row) → the current folder's siblings (current highlighted) → its children
> one level deeper. Disk decks still cost exactly two read-only `read_dir`s
> (the chain comes from the path itself); archives group their in-RAM entry
> names. The action is `Action::FolderTree` (`folder_tree`, default ⇧F — Esc
> stays Quit, so ⇧F/deck-empty are the close paths); rebuilds are gated by a
> `root|folder` signature per tick, skipped while flying. Overflow windows
> around the current row with "… n more" markers. Preview: `--hud-gallery`.

## The feature in one paragraph

Today the playlist is a single flat, alphabetically-sorted index —
`pb-core::Playlist` has no concept of "which folder is photo *i* in," and a
recursive scan (`Action::Recursive`) flattens every subfolder into one big
sorted list at scan time. This feature adds a toggleable HUD overlay (⇧F) that
shows the folder containing the currently-viewed photo, plus its sibling
folders, so the user can browse and jump to a different folder without
leaving the viewer or dropping to the OS file picker. Phase 1 only *shows*
this (current folder + siblings, non-interactive, closes on ⇧F/Esc). Phase 2
adds keyboard selection and lets Enter/→ actually re-open into the chosen
folder.

**Prime-directive fit:** this never touches the render hot path or `pb-core`'s
tested nav math. It's built entirely from the existing HUD compositor
(`pb-hud`) and the existing folder-open plumbing (`open::plan`, `scan.rs`) —
a keypress that opens the overlay costs one `read_dir` at most, and normal
photo nav/prefetch is completely unaffected whether the overlay is open or
closed.

## Why the HUD, not egui

Confirmed (this session's investigation): `pb-hud`'s rasterizer already has
generic multi-line layout (`render_table` for row lists, `render_centered` for
line lists) — panel size is derived from `rows.len()`/`lines.len()`, no
hardcoded single-line limit. egui exists in this codebase **only** in the
second dialog window (Settings/About/Confirm) — the main viewer window's wgpu
pass (`pb-render/src/gpu.rs`) has zero egui integration, and mixing native/egui
widgets into the main window would be a much bigger lift than reusing the
overlay system that already draws toasts/EXIF panel/help. Cost: the HUD is a
pure CPU rasterizer with **no hit-testing** — it's a bitmap. Phase 2 selection
must therefore be keyboard-driven (↑/↓ to move a highlight, Enter to commit),
not mouse-clickable — which fits the keyboard-first philosophy anyway.

## Source-agnostic by construction (works inside .zip / .7z too)

`ZipSource`/`SevenZSource` (`pb-source/src/lib.rs`) store each entry's **full
internal path** in `name(i)` (e.g. `"vacation/day1/img.jpg"`, slash-delimited,
directory entries themselves dropped) — `path(i)` returns `None` for archive
entries (no disk path). So the tree can't be built generically off `path(i)`.
Instead, build it off a **relative-path-segments** helper that works
uniformly:
- `FsSource`: disk path relative to the scan root, split on `/`.
- `ZipSource` / `SevenZSource`: `name(i)` directly, already relative and
  slash-delimited.

This means one small function derives "children of this folder" for any
`PhotoSource`, and the exact same overlay renders whether you opened a folder
or a `.zip`/`.7z`. No archive-specific UI branch.

## Phase 1 — render only (this pass)

**Goal:** prove the overlay renders correctly; no selection, no navigation.

1. **Data:** given the current photo's index, derive its containing folder
   (relative path minus the last segment) and list sibling folders at that
   level — for `FsSource`, one `read_dir` on the parent (filtered to
   directories); for archive sources, group the existing flat entry list by
   first differing path segment (no extra I/O — the paths are already
   in-memory). Cache the list; rebuild only when the current folder changes
   (not per-frame, not per-photo-within-the-same-folder).
2. **Action:** `Action::ToggleFolderTree` (`ActionKind::OneShot`), default
   binding Shift+F (verify free in the default keymap), added to
   `pb-app-core/src/action.rs` + `keymap::EDITOR_GROUPS` like any other
   action.
3. **State (AppCore, RAM-only):** `folder_tree_open: bool` +
   `folder_tree_cache: Option<FolderTreeSnapshot>` (current folder name,
   sibling names, index of current within siblings). Never persisted — this
   is viewing-trace-adjacent the same way the EXIF panel / help overlay are,
   and both already live in RAM only. Cleared on Esc teardown like other
   session state.
4. **Render:** one `pb-hud` panel via `render_table`/`render_centered`,
   current folder marked distinctly (bold, marker glyph, or bracket — reuse
   whatever visual convention the EXIF panel uses for "active" rows if one
   exists). Toggled by the same action that opened it, or Esc.
5. **No interaction yet:** arrow keys, click, etc. all pass through to normal
   photo nav while the panel is open. Closing doesn't change the played
   folder.

**Done when:** ⇧F shows a list of sibling folders with the current one
visually distinct, for both a real directory and a photo opened from inside a
`.zip`, and ⇧F/Esc closes it. No regressions to nav/prefetch (existing
property tests unaffected — nothing in `pb-core` changes in this phase).

> **Status (2026-07-03, later): phase 2 IMPLEMENTED for disk decks** (tasks.json
> #47.6, in review). Everything below landed as designed, plus two owner calls
> made during the build: **the tree tracks the current folder during
> hold-to-fly** (a no-I/O `rows_from_paths` derivation over a per-deck cached
> `disk_counts` map, throttled to one rebuild per 100 ms, upgraded to the full
> `read_dir` view on settle — the `|lite` signature stamp) and **both count
> looks shipped for the A/B** (capsule is live in the app; trailing renders in
> `--hud-gallery`; flipping is the one `TreeCounts` argument in
> `push_folder_tree`). Archive prefix re-scoping is phase 3 (#47.7).
>
> **Same-day rework from the first smoke:** the display is now a true
> **level tree** — every folder at every level along the path shows, with the
> on-path folder expanding in place (`folder_tree::assemble` over per-level
> lists + `emit`). The trigger was ~/Pictures: a recursive open lands on a
> deep first photo, and with only the current level listed, the root's other
> folders were unreachable by click. Changing click semantics was rejected
> (parallel behaviors); the display generalization keeps one semantic and
> costs one `read_dir` per visible level (≤ MAX_ANCESTORS + 2), still only on
> folder change — the fly path derives level lists from the counts map, still
> zero I/O. Also from that smoke: indent hairlines removed (intersected the
> badges), and the capsule badges went contrasty (theme *shadow* fill at 0.85
> — near-black pill on dark, white on light — with full-strength digits).

## Phase 2 — clickable navigation (owner-approved design, 2026-07-03)

Phase 1's "no mouse in the overlay" assumption is obsolete: the scan chip's
Cancel button and the open-screen buttons established the full interactive-
overlay pattern (bitmap-internal rects from the rasterizer, on-screen rects
derived from the live window at hit-test time, hover re-rasterizing only on
enter/leave, `PointerMoved` + the shells' mouse-left ladders, pointer cursor
via `refresh_cursor`). Phase 2 rides that pattern; keyboard *selection*
(↑/↓/Enter) is deferred — arrows pan, and re-purposing them needs a focus-mode
design that ⌘-chords make unnecessary for now.

**Decisions (owner, 2026-07-03):**
- **Click = full Open Folder** (new root, new scan, recursive per the current
  setting) — not a position-move within the existing deck. Single click, no
  selection state. The tree stays open across the open (the signature-gated
  rebuild re-roots it), so it behaves like a navigator.
- **Hover highlight** on clickable rows: a lighter sibling of the current-row
  band (theme-relative — lands on top of #46's `Theme` roles), plus the
  pointer cursor via the existing `refresh_cursor` over-button set.
- **"Up" row above the root** — the parent folder's *real name* (not `..`)
  with the FA `folder-arrow-up` glyph (vendored), only when the root has a
  parent. Keeps the invariant "every row is a folder; clicking opens it."
- **"… n more" windowing markers are clickable — they page the window** (an
  offset in the panel state, re-rendered from cached rows, no re-derivation).
  Without this, rows hidden by the height budget are unreachable by mouse, so
  the overlay silently fails on large folders. Wheel-scrolling the panel is
  explicitly rejected (fights the pan/zoom wheel routing).
- **Go commands, first-class actions** (work with the tree open or closed):
  *Open parent folder* (macOS ⌘↑ via a menu key-equivalent — Finder's
  Enclosing Folder chord; Windows Alt+↑ via the keymap) and *previous / next
  sibling folder* (⌘← / ⌘→; Windows Alt+← / Alt+→ — PhotoBlaze has no
  back/forward history, so the Explorer chords are free). Stop at the ends.
  Menu placement: a small "Go" group.
- **Archives navigate too — no dimmed second-class rows** (owner: make it
  *work* rather than styling the gap). Click targets are an enum:
  `Dir(PathBuf)` | `ArchiveScope(String prefix)`. Opening a scope wraps the
  **original** archive source in a `ScopedSource` (a PhotoSource delegating
  through an index map of entries under the prefix — pure RAM, zip + 7z
  identical) and rebuilds via the existing `rebuild_playlist`. The tree keeps
  deriving from the *unscoped* source's names, so out-of-scope siblings stay
  visible/clickable; "up" from the archive root opens the folder on disk
  containing the archive (`container()`).
- **Empty decks still show the tree** (owner catch: a photo-less root
  stranded you). With no `displayed_item` but a root present, derive from the
  root itself (`rows_from_disk(root, root)`) — the tree becomes a folder
  browser over the empty state, so you can drill into or out of an image-less
  folder entirely from the overlay.
- **Per-folder photo counts, only where free** (archive/recursive decks —
  derived from the in-RAM entry list, zero I/O; plain-disk siblings omit
  them). Two candidate looks to be A/B'd in `--hud-gallery` before wiring:
  a small capsule badge left of the folder glyph (owner suggestion) vs
  trailing right-aligned dim numbers (the Mail/Finder idiom). Owner picks by
  eye; drop the feature if both read as clutter.
- ~~Hairline indent guides~~ — tried, then removed at owner smoke: they
  intersected the count badges and added noise without earning it.

## Non-goals (unchanged)

- No persisted tree/expand state on disk (privacy guarantee — RAM-only,
  cleared on Esc teardown like everything else in the inventory).
- No keyboard row-selection (↑/↓/Enter) yet — see above; ⌘↑/⌘←/⌘→ cover the
  keyboard story without a focus mode.
- No wheel/trackpad scrolling of the panel (the paging markers cover it).
- No OS-native tree control (`NSOutlineView`/Win32 `SysTreeView32`) — the main
  window has no native-control layer to host one in; the HUD/egui-in-a-
  second-window split is the established pattern and this stays consistent
  with it.

**Sequencing note:** phase 2 builds on task #46's HUD `Theme` (in flight in a
parallel session as of 2026-07-03) — the hover band and badges take theme
roles, and `render_tree`'s signature changes land after that session's
token→theme sweep to avoid a two-writer collision in `hud.rs` /
`app_core_impl.rs`.

## Implementation order (each step green: tests, clippy -D warnings, fmt)

1. **Phase 1 data helper** — relative-path-segments function over
   `PhotoSource`, unit-tested against `FsSource` and both archive sources
   (including the archive-has-no-disk-path case).
2. **Action + keymap** — `Action::ToggleFolderTree`, default Shift+F, added to
   `EDITOR_GROUPS`, verified free.
3. **State** — `folder_tree_open` / `folder_tree_cache` in `AppCore`,
   rebuild-on-folder-change only, cleared in `clear_session_state`.
4. **Render** — HUD panel via existing multi-line layout, current-folder
   marker.
5. **CHANGELOG** (Unreleased ▸ Added) + owner smoke.
6. Stop. Get owner feedback on phase 1 before starting phase 2 selection
   logic — the open questions above (scoped-rebuild vs. position-move,
   single- vs multi-level) are real design forks, not implementation details.

## Owner smoke checklist (phase 1)

⇧F over a photo in a plain folder: siblings list appears, current folder
marked, Esc/⇧F closes cleanly. Same inside a `.zip` and a `.7z`. Toggle while
a slideshow is running. Toggle immediately after opening (cache builds
correctly on first show, not stale from a previous session). Recursive scan
toggle (Ctrl+R) while the overlay is closed, then open it — reflects the
*current* photo's folder, not the scan root. No visible cost to normal
keypress→photon nav timing with the overlay closed (should be zero — nothing
new runs unless ⇧F is pressed).
