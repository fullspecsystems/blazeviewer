# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-08 (second session). Supersedes the tree-bleed investigation handoff._

## TL;DR

The **Linux tree/Inspector panel "bleed" is FIXED and verified** (uncommitted). Root cause was
neither of the prior session's hypotheses: egui's `ScrollArea` clips its content to the viewport
**expanded by `Visuals::clip_rect_margin` (3px default)** (egui 0.29 `scroll_area.rs:595-600`) —
meant to keep focus rings/shadows from being cut inside a *padded* container. Our panel bodies run
flush against the panel background's edge, so every scrolled-out row painted a 3px sliver *outside*
the panel: into the header above, over the photo below. The fix is one line in the shared
`scroll_body` helper (`panels_ui.rs`): `ui.visuals_mut().clip_rect_margin = 0.0;` — covers the
tree, Inspector, and Help in one place. **This was never Linux-specific** — the winit/Windows
shell shares this code; it was just first noticed on Linux. CHANGELOG line added under
`[Unreleased] ▸ Fixed`.

## How it was pinned (and where the prior analysis went wrong)

Added a debug dump of the egui offscreen texture (`EguiOverlay::target`) to PNG *before*
compositing, plus per-paint-job clip-vs-vertex-bounds probes, then drove the live app under the
X11 harness:

- **The bleed was present in the offscreen texture itself** → egui pass, not the composite.
- **The composite is mathematically exact** — probed pixel (150,600): predicted premultiplied-over
  value 76, live value 76. Hypothesis (B) (premultiplied-alpha composite mismatch) is dead. The
  "photo showing through the panel" is just the ~71% `info_opacity` setting.
- Scroll-content jobs carried `clip=(21,86)-(367,779)` against a panel background ending at
  y=776 / viewport starting at 89 — the ±3 is `clip_rect_margin`. The leaked sliver measured
  exactly rows y=776–778.
- Prior session's errors: (1) the "2 paint jobs with full-screen clips" were **not** "the photo +
  a HUD element" (the photo is never an egui job); (2) headless `--egui-shot` "clipping fine"
  proved nothing — its tree content fit the viewport, so there was nothing scrolled-out to leak;
  (3) "geometry is provably correct" was true but irrelevant — the layout was never the bug.

Verified post-fix under the X11 harness with the tree scrolled mid-list (the leaking state):
top edge clean under the "Folders" header, bottom row cut exactly at the rounded corner, and the
pixel probe rows below the panel edge show only the soft shadow. Inspector (Shift+I) clean too.

## State of the tree (uncommitted)

- **The fix:** `panels_ui.rs` `scroll_body` — `clip_rect_margin = 0.0` + explanatory comment.
- **CHANGELOG.md**: new `Fixed` entry under `[Unreleased]`.
- **All `PB_TREE_DEBUG` scaffolding stripped** per the previous handoff's plan: the probes in
  `panels_ui.rs` (`tree_panel`, `sdf_panel`), the `[jobs]` block in `egui_overlay.rs`, and the
  startup build banner in `main.rs`. The temporary texture-dump code was added and removed within
  this session (recover from this session's transcript if ever needed again).
- **`cargo fmt --all` applied** — also reformatted files the previous wip commit left unformatted
  (`menu.rs`, `update.rs`, `default_app.rs`, …), so the diff has some fmt-only churn.
- `cargo test -p pb-app -p pb-render`: 79 passed. Clippy: only pre-existing dead-code warnings
  (Linux-gated menu scaffolding).

## Next steps

1. Owner: confirm the fix on the real desktop (Wayland run, real scale factor) — expected to hold
   at any ppp since `clip_rect_margin` is logical-unit.
2. Commit the fix + CHANGELOG + fmt churn (suggest: `fix(panels): clip scrolled rows exactly at
   the panel edge (egui clip_rect_margin bleed)`).
3. The macOS task #58 (auto-size Settings window) handoff from 2026-07-07 is still pending —
   recover its plan via `git log` on this file if picking that up.

## Environment quick-ref (unchanged)

- Repo now lives natively in WSL Ubuntu at `~/photoblaze` (git clone, no rsync/mtime dance).
- X11 harness: `export DISPLAY=:0; unset WAYLAND_DISPLAY`, run the app, `xdotool search --class
  photoblaze`, `xdotool key --clearmodifiers shift+f` (XTEST, never `--window`), scroll via
  `xdotool mousemove … click --repeat N 5`, screenshot with `import -window $WIN out.png`.
- HEIC needs `--features libheif`; see [[linux-port]] for the Linux menu-bar/portability context.
