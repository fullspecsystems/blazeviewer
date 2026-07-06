# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-06. **The macOS rich-panel migration (task #54 / ADR-023) is COMPLETE and
owner-approved.** **Windows / egui parity (Phase 4) is now LARGELY DONE too** — the egui-over-wgpu
main-window seam is built and the tree, inspector, help panel, and info line are all ported. This
doc is the handoff for the remaining parity items (next up: the scan UI)._

Latest on `main`: `1794d25` (egui info readout + ducking). Prior Phase-4 commits: `1e90eb1`
(the panels), `53807a7` (inspector + help polish, markdown, opacity, shortcuts), `159f764`
(slideshow-key revert). All pushed.

## Big picture

Two shells drive one shared, platform-neutral core:
- **`mac/` SwiftUI host** (the shipping macOS app) — every on-image overlay is native SwiftUI.
- **`pb-app` winit shell** (the shipping Windows app; also runs on macOS via wgpu-Metal) — the main
  window is **custom wgpu** for the photo, **plus an egui overlay** for the rich panels (tree /
  inspector / help / info line), **plus egui in a second window** (`dialog.rs`) for Settings /
  About / dialogs. Only the **ephemeral toasts + play hint** are still CPU-HUD on winit.

The core (`pb-app-core`, `pb-core`, `fs_tree.rs`, `panels.rs`, settings) is 100% shared. The shell
seam is the `native_*` flags in `app_core.rs`: when set, the core suppresses HUD rasterization and
exposes the data via `&self` accessors. **On winit these are now true:** `native_help`,
`native_inspector`, `native_tree`, `native_info`. Still false (CPU HUD): `native_toast`,
`native_play`, `native_open`.

## The egui-over-wgpu seam (the hard part — DONE)

- `pb-app/src/egui_overlay.rs` — an `EguiOverlay` (egui ctx + egui-winit state + egui-wgpu
  renderer) sharing the **main renderer's** wgpu device/queue, rendering the panels into an
  offscreen `Rgba8UnormSrgb` texture. **Retained:** re-renders only on `overlay_dirty` /
  `PanelsChanged` / an egui animation frame — **never per nav frame**.
- `pb-render` composites that texture into the fp16 scRGB intermediate **before** tone-map
  (premultiplied blend; `Renderer::set_egui_overlay`). Color-correct on SDR + HDR.
- `pb-app/src/panels_ui.rs` — all the panels, consuming the shell-neutral `pb_app_core::panels` /
  `fs_tree` models. `PanelFrame::snapshot(core)` copies the data out (pure), `build()` lays it out
  and returns `PanelAction`s the shell applies.
- **Verify headlessly:** `./target/debug/photoblaze --egui-shot [out.png] [--light]
  [--tab=details|text|describe]` renders the panels to a PNG (screen capture is blocked on this Mac
  — TCC + borderless surface). `sample_frame` in `egui_shot.rs` holds representative content.

## What's DONE this session (winit egui)

- **Folder tree** — Finder-style, disclosure chevrons (browse ≠ open), dark count pills, outdented
  parent row, truncation, current-folder = accent open-folder icon + bold name (no band).
- **Inspector** — tabbed (Details / Text / Describe) with FA tab icons; Details wraps + 13px +
  wide label column; **Markdown** in Describe (`md.rs`: headings/lists/bold/code/links; italic →
  upright); copy-all wired; **Ask** button opens the real `AskImage` dialog.
- **Help panel** — keyboard shortcuts, keycaps (chords grouped: `⇧R` / `Shift+R` one cap), dark
  keycap badges, full-width section bars aligned to the keys, correct shortcut order.
- **Info readout (`i`)** — egui pill, `folder/name · W×H`, Live-Photo / animation (FA `film`)
  marks, codec badge, **auto-ducks** the tree/inspector, shows independently of the panels.
- **Settings parity** — the Image Info field toggles (show-by-default + folder/filename/resolution/
  codec) added to the winit dialog; opacity reads `info_opacity`.
- **The vertical-centering system** — egui won't center text next to icons, so panels hand-place
  text via `paint_vtext` (a `TEXT_LIFT` knob calibrated to sub-point) + geometric-drawn icons.
  This is the backbone of every panel's layout; reuse it.
- **Core fixes (shared w/ mac):** Details EXIF now warmed on the native path (was only after a
  Describe round-trip); `fresh_shuffle_seed`; `last_info_snap` + a `native_info` tick block.

## What's LEFT for winit⇄mac parity (priority order)

Owner priority (2026-07-06): **#3 info-line ducking = DONE**. Remaining:

1. **Scan UI + Cancel (#2)** — NEXT. A CPU-HUD scan chip already exists (`tick_chip` in `main.rs`:
   "Scanning *Folder* · N found" + Cancel). The likely work is moving it into the egui overlay for
   consistency and confirming Cancel is wired — probably not a from-scratch build. Check that first.
2. **Welcome-screen buttons (#5)** — first-run empty state. `open_panel_visible` exists in the
   core; needs the egui surface + Open File / Open Folder buttons. Flip `native_open`.
3. **Drag-to-move / drag-to-resize panels** — SwiftUI has `ResizeHandle`; egui panels are
   fixed-position/width today.
4. **Folder count precompute** — tree counts only appear after opening a folder (core
   `folder_counts` only covers the recursive deck; siblings are blank until visited).
5. **Play-hint fade (#4)** — Live-Photo/animation only; still CPU HUD (`native_play` false).
6. **Toasts (#1)** — lowest; non-interactive, CPU-HUD toasts already work. Consistency only.
7. **Cmd/Ctrl+arrow folder nav** — `Action::PrevFolder`/`NextFolder` exist but aren't bound/reaching
   the winit shell.

## Working notes for a new session

- **Run/iterate on macOS:** `cargo run -p pb-app` (winit+wgpu window; `⇧F` tree, `⇧I`/`T`/`D`
  inspector, `?` help, `i` info line, `⌘,` Settings). Headless panel preview: `--egui-shot`.
  Components: `cargo run -p pb-ui --example gallery`.
- **Gate:** `cargo fmt --all && cargo clippy -p pb-ui -p pb-app -p pb-app-core --all-targets -- -D
  warnings && cargo test -p pb-ui -p pb-app -p pb-app-core`. ⚠ Never run `apply_settings` /
  `apply_keymap` end-to-end in a test — they write the **real** `settings.toml`.
- **Where things live:** egui panels = `crates/pb-app/src/panels_ui.rs` (+ `egui_overlay.rs`,
  `md.rs`, `egui_shot.rs`); render seam = `crates/pb-render/src/gpu.rs`; egui Settings/dialogs =
  `dialog.rs`; winit shell wiring = `main.rs`; egui components/icons = `crates/pb-ui/`; shared
  models = `panels.rs` / `fs_tree.rs`; `native_*` flags + accessors = `app_core.rs` /
  `app_core_impl.rs`. Detailed gotchas: the auto-memory `egui-panels-winit-phase4.md`.
- **Perf note:** the info line changes per photo, so the retained overlay re-renders per nav (per
  frame during hold-to-fly). Fine so far; throttle if fly-through stutters.
- **CHANGELOG:** user-facing lines under `[Unreleased]`.
- **Windows validation still owed:** WIC codecs, DXGI HDR, MSI, signing — can't be tested from the
  Mac. All egui work above is verified on macOS via `cargo run` + `--egui-shot`.
