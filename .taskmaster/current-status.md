# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-05. **Task #54 (ADR-023 rich-panel migration, mac-first).** Five
native pieces landed + a folder-tree redesign started. All green: 554 tests, clippy clean,
Swift host builds. Owner smoked the panels live and is iterating on the tree._

## Landed this session (all on `main`, pushed)

1. **Welcome surface** (`2585b33`) — native empty-state (equal-width Open buttons w/ keycaps,
   drag hint, reference keys, Show Shortcuts). Fixed the cursor-through-panel glitch.
2. **AI-settings auto-list** (`abd9528`) — owner's parallel work, committed separately.
3. **Native Inspector** (`53a553b`, redesigned in `f1cbac5`) — Details/Text/Describe as one
   panel, **top-right**; **icon+label tab bar** (translucent accent tint, no solid segmented
   track), inline **✕**, selectable values, **Markdown** for Describe. Live-updates on async
   OCR/describe via a per-tick `InspectorSnapshot` diff.
4. **Native folder tree v1** (`8b6d5bc`) — ⇧F tree as a native scrolling list + **✕** close.
   (Being replaced by the Finder redesign below.)

## In progress: Finder-style folder browser (owner design 2026-07-05)

**The problem with v1:** it's an auto-derived "where am I" path — folds deep ancestors into a
dead "…" row, re-derives per photo, and couples browsing to loading (a click re-roots the
deck). Owner wants a real **Finder tree**: expand/collapse chevrons, siblings at every level,
browsing decoupled from loading.

**Aligned design:**
- **Navigate ≠ load.** Chevron = expand/collapse (read_dir, no scan). Folder-name click =
  open (load photos). So you can browse photo-less folders to find one.
- **Persistent, incremental, RAM-only tree** — kept resident, updated as you navigate (no
  per-photo re-walk). Privacy-clean (paths + counts in memory like `meta_cache`, never written).
- **No jail:** up-affordance (real parent, `arrow.up.folder`, not "…") + expand anywhere.
- **Stuck-proofing:** ① keep the current deck alive until the newly-opened folder yields its
  first photo (mis-click into an empty/deep folder never strands you); ② off-thread read_dir
  (a slow share never freezes a chevron); ③ the ambient cancellable scan pill.

**Increments** (① done):
- **① Resident model — DONE** (`7acc91c`): `pb-app-core/src/fs_tree.rs` — pure `FsTree`
  (nodes: expanded / lazy children / count; `rows()` flatten; `set_current` reveal+mark;
  `extend_root_up`). Shell does read_dir off-thread → `set_children`. 6 tests. Shell-neutral
  (Windows egui reuses it).
- **② Wire behind the native path** — core owns an `FsTree`; tick kicks off-thread read_dir
  for `needs_children` folders + installs results; `set_current` on folder change; FFI
  `tree_toggle`/`tree_open`/`tree_extend_up` + row accessors incl. `has_children`/`expanded`/
  `depth`; SwiftUI outline (chevrons, indent, current highlight). Replaces the v1 native tree
  (keep winit HUD on the old `folder_tree.rs` derivation until the egui track).
- **③ Keep-deck-until-photos** — open a folder without tearing down the current deck until
  the new scan's first frame; empty → "No photos in *Foo*" toast, deck intact.
- **④ Ambient scan pill** — the folder/archive open scan becomes a non-blocking top-center
  SwiftUI element with a Cancel (owner: not a blocking modal — browse while it scans).

## Known follow-ups / smoke notes

- **Pointer-gating** still not done (panels hit-test clicks but `mouseMoved` may reach the
  canvas underneath) — verify a tree/tab click doesn't also pan the photo. Top non-tree task.
- Current-folder icon is `folder.fill` (SF has no open-folder glyph) — vendor FA `folder-open`
  if the literal look is wanted.
- Inspector Markdown uses inline-only syntax (bold/italic/links + paragraph breaks); block
  lists/headings aren't styled yet.

Plan: `.taskmaster/docs/hud-panels-plan.md` + `folder-tree-plan.md`. Seam refs:
`native_*` flags in `app_core.rs`; FFI in `pb-mac-ffi/src/lib.rs`; new model in `fs_tree.rs`.
