# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-05 (late). **Task #54 (ADR-023 rich-panel migration, mac-first) +
the Finder folder-browser redesign.** All green: **555 tests**, clippy clean, Swift host
builds. Owner is smoking live and iterating._

## Where we are in the big picture (task #54 phases)

The macOS-first rich-panel migration (`hud-panels-plan.md`):
- **Phase 0 — panel models:** DONE (`panels.rs`, `overlay.rs`).
- **Phase 1 — mac presenter seam + Help pilot:** DONE. _(Replay harness still deferred.)_
- **Phase 2 — mac folder tree:** DONE, then **redesigned** into the Finder browser (below).
- **Phase 3 — mac Inspector tabs:** DONE (native tabbed Inspector, redesigned).
- **Phase 4 — Windows egui presenter track:** NOT STARTED — re-do Help + tree + Inspector on
  winit via egui, reusing the shell-neutral models (`FsTree`, `panels.rs`).

## Landed this session (all on `main`, pushed)

- **Welcome surface** (`2585b33`) + polish → stacked Open pills (`59f14b8`).
- **AI-settings auto-list** (`abd9528`) — owner's parallel work.
- **Native Inspector** (`53a553b`) → redesign (`f1cbac5`): top-right, **segmented tab bar**
  (blue-fill selection + white text, `40af0af`), **Markdown** descriptions, full EXIF (scrolls,
  no "…" cap), selectable values, ✕.
- **Native folder tree**: v1 (`8b6d5bc`) → **Finder browser** ① model `fs_tree.rs` (`7acc91c`)
  + ② wiring (`b599b0a`).
- **Panel polish** (`f3eba3c`, `40af0af`): up-row indent (outdented parent), Help topmost
  z-order, arrow-cursor-on-hover stopgap, solid `panelSecondary` glyphs, constant-weight tabs.
- **Resizable panels** (`002402f`): drag the tree's right / Inspector's left edge.
- **⌘←/⌘→ folder nav fix** (`3f9dff1`): anchor on the current folder, in-deck jump.
- **Folder-nav polish batch** (`2532121`): **files-before-folders** sort (a folder's photos
  precede its subfolders' — deck reads like the tree); **⌘↑ Open Parent** anchors on the
  current photo's folder (not the deck root — no more climbing toward `/`); tree
  **auto-collapses** branches you've scrolled past (chevron-pinned ones stay, via
  `Node.user_expanded`); tree **scrolls the current folder into view** (`tree_current_path`
  FFI + `ScrollViewReader`).

## Finder folder browser — increment status

**Design (owner, 2026-07-05):** navigate ≠ load. Chevron = expand/collapse (off-thread
`read_dir`, no scan); folder-name click = open (load photos). Persistent, incremental,
RAM-only `FsTree`. Up-affordance row (outdented parent). Siblings at every level.

- **① Resident model — DONE** (`fs_tree.rs`, 6 tests): nodes (expanded / lazy children /
  count), `rows()`, `set_current` reveal+mark, `extend_root_up`. Shell-neutral.
- **② Wire behind native path — DONE**: `AppCore::fs_tree` + `fs_tree_io`; tick kicks
  off-thread reads + installs; FFI snapshot (`tree_uses_fs`, `tree_toggle`, chevron rows);
  SwiftUI outline. Disk decks only; archive/empty keep the v1 flat scoped list.
- **③ Keep-deck-until-photos — TODO**: opening a folder still tears the current deck down
  immediately (brief empty gap during its scan). Keep the current deck alive until the new
  folder yields its first frame; empty → "No photos in *Foo*" toast, deck intact. This is the
  anti-"stuck" safety.
- **④ Ambient cancellable scan pill — TODO**: the folder/archive open scan becomes a
  non-blocking **top-center** SwiftUI element with a **Cancel** (owner: not a blocking modal —
  browse the streamed-in photos while it scans). Confirmed scope = the folder/archive open
  scan (not the Inspector's OCR/describe states). Pairs with ③.

## ⌘←/⌘→ folder nav — REDESIGNED (owner, 2026-07-05)

Two root causes found + fixed: (1) it anchored on the deck **root**, not the current folder
(`3f9dff1`); (2) the scan sorted **case-sensitively** (raw bytes), so the deck order desynced
from the tree (`2a495a3` — now case-insensitive `ci_path_cmp`, matching the tree/Finder).
Then the **model** was reworked to the owner's mental model (`78764d5`): **"next photo, but by
folder"** — step to the next/previous folder **boundary in the deck's tree-ordered sequence**
(enter subfolders, walk siblings, climb up), landing on that folder's first photo. In-deck,
instant, can't dead-end. `adjacent_folder_item` in `app_core_impl.rs`; single-folder decks
fall back to the disk sibling search. **Still open (minor, owner to eyeball):** the ⌘←
"start of previous run" convention (vs. "rewind to current folder's start first"); and
**natural/numeric sort** (img2 before img10) is a separate deferred enhancement.

## Known follow-ups (deferred, not lost)

- **Pointer-gating** (proper): the canvas suppresses pointer handling inside a presenter's
  reported frame. Current `arrowCursorOnHover` is a stopgap; clicks on panels still reach the
  canvas conceptually — verify a tree/tab click doesn't also pan a zoomed photo.
- **Panel drag-to-move + disk persistence**: positions (owner: "persist positions only") and
  the new resizable **widths** (session-only today) both want disk persistence — one slice via
  `settings.panel_pos_*` + FFI.
- **Current-folder icon**: `folder.fill` (SF has no open-folder glyph) — vendor FA
  `folder-open` if the literal look is wanted.
- **Inspector Markdown**: inline only (bold/italic/links + paragraph breaks); block lists /
  headings not styled yet.
- **Replay harness** (Phase 1 tail): headless `CoreEvent` replay → keypress→photon p50/p95/p99.

## Next (suggested order)

1. **③ Keep-deck-until-photos** + **④ ambient scan pill** (finish the Finder browser; they pair).
2. **⌘←/→ redesign** (with the owner — it's a UX decision, not a patch).
3. **Pointer-gating** (proper) + panel drag/persistence.
4. **Phase 4 — Windows egui** track.

Plans: `.taskmaster/docs/hud-panels-plan.md`, `folder-tree-plan.md`. Seams: `native_*` flags
in `app_core.rs`; FFI in `pb-mac-ffi/src/lib.rs`; models in `fs_tree.rs` / `panels.rs`.
