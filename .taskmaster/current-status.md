# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-06. The macOS rich-panel migration (task #54 / ADR-023) is COMPLETE and
owner-approved. The **Windows / egui HUD rework (Phase 4) is DONE** — every interactive on-photo
overlay is egui, the dead CPU-HUD code is gone, ⌘+arrow folder nav is wired, and the info-line
alignment bug is fixed. What's left is optional / non-HUD (see below)._

Latest on `main`: `4d69ee5` (info-line right-align pin + regression test). Earlier this session:
`a3a6bfb` (⌘+arrow folder nav), `7d29a2b` (dead-CPU-HUD cleanup), `9a7a48d` (play hint),
`e0d8541` (welcome screen), `6fee0b1` (scan pill + settings polish). All pushed.

## Big picture

Two shells drive one shared, platform-neutral core:
- **`mac/` SwiftUI host** (the shipping macOS app) — every on-image overlay is native SwiftUI.
- **`pb-app` winit shell** (the shipping Windows app; also runs on macOS via wgpu-Metal) — the main
  window is **custom wgpu** for the photo, **plus an egui overlay** for every rich/interactive panel
  (tree / inspector / help / info line / scan pill / welcome / play hint), **plus egui in a second
  window** (`dialog.rs`) for Settings / About / dialogs.

The core (`pb-app-core`, `pb-core`, `fs_tree.rs`, `panels.rs`, settings) is 100% shared. The shell
seam is the `native_*` flags in `app_core.rs`: when set, the core suppresses HUD rasterization and
exposes the data via `&self` accessors + a `PanelsChanged` effect. **On winit these are ALL true
now** — `native_help`, `native_inspector`, `native_tree`, `native_info`, `native_open`,
`native_play`. The **only** one still false (still CPU-HUD) is `native_toast`.

## The egui-over-wgpu seam (the hard part — DONE)

- `pb-app/src/egui_overlay.rs` — an `EguiOverlay` (egui ctx + egui-winit state + egui-wgpu
  renderer) sharing the **main renderer's** wgpu device/queue, rendering the panels into an
  offscreen `Rgba8UnormSrgb` texture. **Retained:** re-renders only on `overlay_dirty` /
  `PanelsChanged` / an egui animation frame — **never per nav frame** (except the info line, which
  changes per photo; see the positioning note below).
- `pb-render` composites that texture into the fp16 scRGB intermediate **before** tone-map
  (premultiplied blend; `Renderer::set_egui_overlay`). Color-correct on SDR + HDR.
- `pb-app/src/panels_ui.rs` — all the panels, consuming the shell-neutral `pb_app_core::panels` /
  `fs_tree` models. `PanelFrame::snapshot(core)` copies the data out (pure), `build()` lays it out
  and returns `PanelAction`s the shell applies.
- **Verify headlessly:** `./target/debug/photoblaze --egui-shot [out.png] [--light]
  [--tab=details|text|describe] [--welcome]` renders the panels to a PNG (screen capture is blocked
  on this Mac — TCC + borderless surface). `sample_frame` in `egui_shot.rs` holds the content.

## Durable implementation notes (reuse these — they bit us)

- **The vertical-centering system** — egui won't center text next to icons, so panels hand-place
  text via `paint_vtext` (a `TEXT_LIFT` knob calibrated to sub-point) + geometric-drawn icons.
  This is the backbone of every panel's layout; reuse it.
- **The shared open-button design** — `draw_open_button` + `open_button_width` + the `OPEN_*`
  constants in `panels_ui.rs`. The welcome buttons AND the play hint use it (the hint is the same
  button, translucent, bottom-center one `EDGE` above the info line). Tweak once, both stay in sync.
- **Overlay positioning gotcha (fixed for the info line; LATENT elsewhere)** — egui's `Area::anchor`
  *and* its default `constrain: true` position/clamp an area using the **previous** frame's stored
  size. For a retained overlay that re-renders once per photo, any **width-varying, edge-anchored**
  content bounces between photos (the info-line bug — center was fine because it's never near an
  edge; right/left hit the screen-edge clamp). Fix pattern: compute the top-left from the **known
  current width** + `.fixed_pos()` + `.constrain(false)` (see `info_line` + its regression test
  `right_aligned_info_line_pins_to_edge_regardless_of_previous_width`). ⚠ The scan pill / play hint /
  welcome still use `.anchor()` — safe **only** because their widths are fixed (or they re-render
  every frame); give any of them variable-width content and apply the same pattern.

## What's LEFT (all optional / non-HUD)

Remaining CPU-HUD bits are **non-interactive** (no hit-testing to remove) — pure cosmetic
consistency, low value:
1. **Toasts (`native_toast` false)** — work well; port only for pixel-consistency.
2. **The loading pie** (decode-wait spinner) — CPU-HUD, non-interactive, no flag; same story.

Non-HUD parity / polish items still open:
3. **Scan pill full parity** — drop the bootstrap gate + suppress the pre-bootstrap
   `DialogKind::Scanning` window so the pill covers the whole scan like macOS (needs pre-photo
   overlay rendering confirmed live — couldn't verify headlessly, screen capture blocked).
4. **Drag-to-move / drag-to-resize panels** — SwiftUI has `ResizeHandle`; egui panels are
   fixed-position/width today.
5. **Folder count precompute** — tree counts only appear after opening a folder (core
   `folder_counts` only covers the recursive deck; siblings are blank until visited).
6. **Windows folder-nav modifier** — winit macOS now uses ⌘+arrow (Go menu); Windows keeps
   `Alt+arrow` (keymap, Explorer idiom). Switch to `Ctrl+arrow` only if the owner asks.

## Working notes for a new session

- **Run/iterate on macOS:** `cargo run -p pb-app` (winit+wgpu window; `⇧F` tree, `⇧I`/`T`/`D`
  inspector, `?` help, `i` info line, `⌘,` Settings, `⌘↑`/`⌘←`/`⌘→` folder nav). Headless previews
  (screen capture blocked here): `--egui-shot [out.png] [--light] [--tab=details|text|describe]
  [--welcome]` for the on-photo panels, and `--settings-shot [out.png] [--light]
  [--tab=general|appearance|shortcuts]` for **Settings** (`dialog::settings_shot_body`).
  Components: `cargo run -p pb-ui --example gallery`.
- **Gate:** `cargo fmt --all && cargo clippy -p pb-ui -p pb-app -p pb-app-core --all-targets -- -D
  warnings && cargo test -p pb-ui -p pb-app -p pb-app-core`. ⚠ Never run `apply_settings` /
  `apply_keymap` end-to-end in a test — they write the **real** `settings.toml`.
- **Where things live:** egui panels = `crates/pb-app/src/panels_ui.rs` (+ `egui_overlay.rs`,
  `md.rs`, `egui_shot.rs`); render seam = `crates/pb-render/src/gpu.rs`; egui Settings/dialogs =
  `dialog.rs`; winit shell wiring = `main.rs`; the macOS menu (⌘ accelerators / Go submenu) =
  `menu.rs`; egui components/icons = `crates/pb-ui/`; shared models = `panels.rs` / `fs_tree.rs`;
  `native_*` flags + accessors = `app_core.rs` / `app_core_impl.rs`. Detailed gotchas: the
  auto-memory `egui-panels-winit-phase4.md`.
- **Perf note:** the info line changes per photo, so the retained overlay re-renders per nav (per
  frame during hold-to-fly). Fine so far; throttle if fly-through stutters.
- **CHANGELOG:** user-facing lines under `[Unreleased]`.
- **Windows validation still owed:** WIC codecs, DXGI HDR, MSI, signing — can't be tested from the
  Mac. All egui work above is verified on macOS via `cargo run` + `--egui-shot`.
