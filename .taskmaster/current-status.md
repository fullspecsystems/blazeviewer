# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-05 (overnight). **Task #54 (ADR-023 rich-panel migration,
mac-first): three native panels landed this session — welcome / Inspector / folder tree.**
All build + 548 tests + clippy clean + Swift host builds. **NOT yet owner-smoked** (I can't
run the macOS GUI) — please smoke, then refine._

## What landed this session (all on `main`, pushed)

Every panel uses the same seam as the Help pilot: a `native_*` flag → the core suppresses
that panel's HUD rasterization → emits `CoreEffect::PanelsChanged` on change → the SwiftUI
host re-pulls via indexed FFI accessors (no `Vec<struct>` returns).

1. **Welcome surface** (`EmptyStateView`, commit `2585b33`) — equal-width Open File / Open
   Folder buttons (right-aligned keycaps), drag-and-drop hint, Next/Prev/Random reference
   keys, Show Shortcuts link. Fixed the cursor-through-panel glitch by construction.
2. **Inspector** (`InspectorPanelView`, commit `53a553b`) — Details / Text / Describe as one
   tabbed panel on the **trailing** edge. Segmented tab bar, selectable values, ✕ close.
   `native_inspector` + a per-tick `InspectorSnapshot` diff re-signals on **async** OCR /
   describe results, so the open panel updates in place. Tab clicks = `open_inspector`
   (never toggle-closed); the tick kicks the active tab's scan (OCR for Text; auto-describe
   only when the setting is on).
3. **Folder tree** (`FolderTreePanelView`, commit `8b6d5bc`) — ⇧F tree as a native list on
   the **leading** edge: depth-indented rows, folder icons, count badges, current folder
   highlighted, **scrolls** (no HUD "… n more" paging). `native_tree` makes
   `push_folder_tree` store rows/targets + emit instead of rasterizing — reusing the
   existing derivation + navigation untouched. `tree_activate(i)` drives a row click.

## Please smoke (in priority order)

- **Inspector:** ⇧I (Details table), **T** (Text → OCR should populate in place when the
  scan finishes), **D** (Describe). Tab-bar clicks switch facets; ✕ closes; values are
  selectable (⌘C copies the selection). Switch photos with it open — it should track.
- **Folder tree:** ⇧F. Click folders to navigate; current folder highlighted + badged;
  long folders scroll. Try an archive (re-scope) if handy.
- **Esc still quits** from all of these (panels never trap it); **Tab** hides/shows panels.

## Known limitations / things to refine (honest list)

- **Pointer-gating not done** (same class as the old Help cursor bug): the panels hit-test
  **clicks** above the canvas, but `mouseMoved` may still reach the canvas underneath, so
  hovering a panel over a zoomed photo could show the grab cursor / drive hover. Clicks on
  panel buttons *should* be consumed by SwiftUI, but **verify a tree/inspector click doesn't
  also pan the photo** — if it does, that's the gating work (presenter reports its frame;
  canvas suppresses pointer inside it). This is the top follow-up.
- **Placement is provisional** — Inspector trailing, tree leading, both floating cards.
  Drag-to-move + position persistence is a later slice (owner decision: persist positions).
- **Inspector on an empty deck**: opening it with no photo shows "Nothing to show" and can
  overlap the welcome surface — unlikely, unhandled; easy to gate on `current` if it bugs.
- **No ⌘C-copy-all** in the Inspector yet (values are individually selectable). The
  `DetailsPanel::copy_text` / `DescribePanel::copy_text` payloads exist for a copy button.
- **Details rebuilds `exif_rows()` each tick** while open (for the change diff) — cache-only,
  bounded, but could be signature-gated if it shows on a profile.

## Next (priority order)

1. **Pointer-gating** for the native panels (canvas suppresses pointer inside a presenter's
   reported frame) — makes hovering/clicking panels not leak to the photo.
2. **Drag-to-move + position persistence** (shared panel chrome; `settings.panel_pos_*`
   already exist).
3. **Windows egui parity** for the three panels (the winit shell still uses the HUD).
4. **Replay harness** (finishes Phase 1 perf validation).

Plan: `.taskmaster/docs/hud-panels-plan.md`. Seam reference: `native_help`/`native_open`/
`native_inspector`/`native_tree` in `app_core.rs`; FFI accessors in `pb-mac-ffi/src/lib.rs`.
