# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-04 late. **Task #54 (HUD panel layer, ADR-023): Phase 0 + info-line
decouple + alignment (54.1) AND the Phase-1 macOS presenter seam + native Help pilot
(54.2) IMPLEMENTED — both in review, owner smoke pending.** Remaining Phase-1 item: the
headless replay harness._

## Where we are

The rich-panel migration is mac-first. Phase 0 extracted the semantic panel models and
the Inspector/`Tab` state machine (HUD still renders them). Then the basic `i` info line
was fully decoupled onto its own permanent layer with a geometry-aware reserve, plus a
Left/Center/Right alignment preference. Now Phase 1 has landed the **suppress-HUD-per-shell
seam** and the **native SwiftUI Help panel** — the first on-image panel to render natively
on macOS.

## What landed this session

- **Phase 1 seam:** `AppCore::native_help` (mac host sets it at construction) → `show_overlay`
  suppresses Help's HUD rasterization (clearing any leftover panel) → the tick emits
  `CoreEffect::PanelsChanged` only on a real Help show/hide (`apply_keymap` re-emits on
  content change). FFI: the `PanelsChanged` marker + `help_refresh`/`help_visible` + indexed
  `help_row_*` accessors (keymap-editor pull pattern; no `Vec<struct>` FFI return).
- **Native Help (Swift):** `CoreModel.helpVisible`/`helpRows` refresh on the marker; a
  `HelpPanelView` card (title bar + ✕ close + scroll, `.regularMaterial`) layered
  `.overlay(.center)` over `MetalCanvas`. It hit-tests above the canvas (its scroll/click are
  its own; the rest falls through to nav). Winit untouched (`native_help == false`, no-op
  drain arm).

## Verification

545 workspace tests (incl. `native_help_suppresses_the_hud_and_signals_visibility`,
`winit_keeps_help_on_the_hud_no_native_signal`, `info_line_reserve_follows_the_horizontal_overlap`),
`clippy -D warnings` clean, `cargo fmt`, `./scripts/build-swift-host.sh` builds. **Not yet
owner-smoked live** (the Swift Help view especially — I can't run the macOS GUI).

## Deferred by design (not gaps)

Input gating (Help is read-only → no first-responder gate needed; that + ⌘C-beats-Copy-Image
come with the Inspector). Drag-to-move (Help centers; comes with the shared panel chrome). The
headless replay harness (its own slice — needs the corpus + a real renderer to produce
meaningful keypress→photon percentiles).

## Help panel — owner-smoked ✓ + design polish landed (2026-07-05)

The native Help panel is confirmed working on macOS. Owner design feedback applied
(HelpPanel.swift): keycap pills per key with plain dim `/` separators; two column-major
columns per section; tinted grouped section headers; faint groove vs. hard divider;
adaptive height (fits content up to the window height, scrolls when short). Swift host
builds. **Known glitch → drives the next slice:** the mouse cursor still changes over the
empty-state Open/Open-Folder buttons *through* an open Help panel — a SwiftUI overlay
blocks clicks but not the canvas's `mouseMoved` tracking, so the HUD open-buttons under it
still drive the cursor. Fixed by construction when those buttons go native (below).

## Native empty-state Open panel — IMPLEMENTED (subtask 54.6, owner smoke pending)

The empty-state CTA is now a native SwiftUI **welcome surface** (`EmptyStateView`): app
icon + name, Open File (prominent) / Open Folder (bordered) buttons, essential-keys tips
(Next/Previous/Random) reusing the Help keycaps, and a "See all shortcuts" link that opens
Help. Same `native_open` suppress seam + `PanelsChanged` signal; adds the click-dispatch
path (`menu_action` → `Action::OpenFile`/`OpenFolder`). **The cursor-through-panel glitch
is fixed by construction** — `show_open_hint` no longer rasterizes a HUD panel on mac, so
there's no open-button hit-rect under a native panel. New generic `action_shortcut(id)` FFI
primitive so future welcome tips need no bespoke accessor. 546 tests, clippy, fmt, builds.

## Next (priority order)

1. **Replay harness** (finishes Phase 1): a headless `CoreEvent` replay over the corpus that
   dumps `StageTimes` p50/p95/p99 for hidden / panel-open+idle / panel-open+fly.
2. **Phase 2:** the macOS folder tree as a flat SwiftUI `List` over `FolderTreePanel`, reusing
   the suppress-HUD seam + `PanelsChanged` (add `native_tree`) — and the general pointer-gating
   (presenter reports its frame; canvas suppresses `pointerMoved` inside it, so hovering a panel
   over a zoomed photo doesn't show the grab cursor). Then Phase 3 Inspector tabs (input gating +
   drag chrome). Plan: `.taskmaster/docs/hud-panels-plan.md`.

## Welcome-surface growth ideas (owner-aligned, deferred)

`EmptyStateView` is architected to grow (owner steer): a **Reopen [last folder]** quick action
(via `settings.last_folder`, ADR-022 — a folder name is in-bounds), more/rotating tips, recent
items. The `action_shortcut(id)` FFI already makes adding shortcut tips trivial.
