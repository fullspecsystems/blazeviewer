# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-06. **The macOS rich-panel migration (task #54 / ADR-023) is COMPLETE and
owner-approved.** **The Windows / egui HUD rework (Phase 4) is now DONE** — every interactive
on-photo overlay is egui (tree, inspector, help, info line, scan pill, welcome buttons, play hint),
the dead CPU-HUD code is removed, and ⌘+arrow folder nav is wired in the winit macOS menu. What's
left is optional/non-HUD (see below)._

Latest on `main`: `a3a6bfb` (⌘+arrow folder nav). Prior this session: `7d29a2b` (dead-CPU-HUD
cleanup), `9a7a48d` (play hint), `e0d8541` (welcome screen), `6fee0b1` (scan pill + settings polish).
All pushed.

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

- **Scan pill (task #2)** — the folder-scan progress is now the top-center egui pill (SwiftUI
  `ScanPillView` parity), replacing the CPU-HUD scan card: a hand-drawn arc spinner (throttled
  ~30 fps via `request_repaint_after`, *not* `egui::Spinner` — avoids per-frame overlay churn for
  a whole scan), `Scanning <Name>` + a live `N found` in a fixed 300px column, the current
  sub-folder (truncated), a hairline divider, and an accent **Cancel** button
  (`PanelAction::CancelScan` → `cancel_scan_command`). Lives in `panels_ui::scan_pill` +
  `PanelFrame.scan` (shell-owned: `App::scan_pill_frame` fills it in `render_overlay_frame`, since
  scan state is in `App::dir_scan`, not the core). `tick_chip` now marks `overlay_dirty` on the
  scan signature instead of `push_chip`/`clear_chip`; `scan_pill_visible()` joins
  `overlay_panel_visible()` (pointer routing → Cancel gets clicks). Verified: `--egui-shot` (both
  themes) + a live 100k-file slow scan (pill SHOW/HIDE fires, no panics). **Gated the SAME as the
  old card** (`scan_bootstrapped && past-`SCAN_DIALOG_DELAY``), so the rare **pre-bootstrap
  `DialogKind::Scanning` window is UNCHANGED** — the pill is the post-bootstrap surface only.
  - **Follow-up (not done):** *full parity* = drop the bootstrap gate + suppress the pre-bootstrap
    Scanning dialog so the pill covers the whole scan like macOS (needs pre-photo overlay rendering
    confirmed live — couldn't verify headlessly, screen capture blocked). (The CPU-chip removal
    follow-up noted here earlier is DONE — see `7d29a2b`.)
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

## The egui HUD rework is DONE

Every **interactive** on-photo overlay is now egui. Landed this session (all on `main`, pushed):
**Scan pill (#2)**, **welcome / empty-state Open buttons (#5, `native_open`)**, **play hint
(#4, `native_play`)** — all reuse the shared `open_button` design (`draw_open_button` +
`open_button_width` + `OPEN_*` in `panels_ui.rs`; the play hint is the same button, translucent,
bottom-center `EDGE` above the info line). Then the **dead-CPU-HUD cleanup** (574 lines: the open-
panel/play-hint/scan-chip rasterizers + hit-tests across pb-app-core/pb-render/pb-app/pb-mac-ffi;
kept the pb-hud rasterizers — `hud_gallery` still uses them — and `chip_sig`/`chip_built`). And
**⌘+arrow folder nav** in the winit macOS menu (a "Go" submenu; the actions were already core-handled).

## What's LEFT (all optional / non-HUD)

The remaining CPU-HUD bits are **non-interactive**, so there's no hit-testing to remove — pure
cosmetic consistency, low value:
1. **Toasts (`native_toast` false)** — work well; port only for pixel-consistency.
2. **The loading pie** (decode-wait spinner) — CPU-HUD, non-interactive, no flag; same story.

Non-HUD parity items still open:
3. **Drag-to-move / drag-to-resize panels** — SwiftUI has `ResizeHandle`; egui panels are
   fixed-position/width today.
4. **Folder count precompute** — tree counts only appear after opening a folder (core
   `folder_counts` only covers the recursive deck; siblings are blank until visited).
5. **Windows folder-nav modifier** — winit macOS now uses ⌘+arrow (Go menu); Windows keeps
   `Alt+arrow` (keymap, Explorer idiom). Switch to `Ctrl+arrow` only if the owner asks.

## Working notes for a new session

- **Run/iterate on macOS:** `cargo run -p pb-app` (winit+wgpu window; `⇧F` tree, `⇧I`/`T`/`D`
  inspector, `?` help, `i` info line, `⌘,` Settings). Headless previews (screen capture is blocked
  here): `--egui-shot [out.png] [--light] [--tab=details|text|describe]` for the on-photo panels
  (Help / Inspector / tree / info line / **scan pill**), and `--settings-shot [out.png] [--light]
  [--tab=general|appearance|shortcuts]` for the **Settings** (tab strip + the chosen tab's content;
  `dialog::settings_shot_body`).
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
