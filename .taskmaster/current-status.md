# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-05 (night). **The macOS rich-panel migration (task #54 / ADR-023) is
COMPLETE and owner-approved for 1.0.** All green: **563 tests**, clippy clean (workspace +
aarch64), Swift host builds. The remaining track is **Windows / egui parity (Phase 4)** — this
doc is the handoff for it._

## Big picture

Two shells drive one shared, platform-neutral core:
- **`mac/` SwiftUI host** (the shipping macOS app) — drives the Rust engine over swift-bridge.
  **Every on-image overlay is now native SwiftUI**; nothing is left on the CPU HUD rasterizer.
- **`pb-app` winit shell** (the shipping Windows app; also runs on macOS via wgpu-Metal) — the
  main window is **custom wgpu + the CPU-rasterized HUD** for on-image overlays, plus **egui in a
  second window** (`dialog.rs`) for Settings / About / Confirm / Message / Password.

The core (`pb-app-core`, `pb-core`, `fs_tree.rs`, `panels.rs`, settings) is **100% shared and
shell-neutral**. The shell seam is the `native_*` flags in `app_core.rs`: when a flag is set the
core suppresses HUD rasterization and exposes the data via `&self` FFI accessors instead; the
SwiftUI host reads them. The winit shell leaves the flags false and keeps the HUD.

## What's fully DONE (macOS, on `main`)

Native SwiftUI, matching/exceeding the old HUD: **welcome screen, Finder-style folder tree**
(chevron browse ≠ open, `fs_tree.rs`), **tabbed Inspector** (Details/Text/Describe, Markdown,
copy-all + Ask button), **scan pill** (ambient, cancellable), **unified toasts**, **one-line
info readout**, and the **play hint** (▶/livephoto pill, fades, hover-to-hold, click-plays).
Plus this session's polish: shared 24px margin system, width-aware panel↔info-line collision,
concentric codec badge, **configurable info line** (Settings ▸ Appearance ▸ Image Info: show-by-
default + Folder/Filename/Resolution/Codec toggles + position), Live-Photo & animated-image
marks by the codec, "photo"→"image" wording pass, Ask-dialog polish.

## The drift to close (mac ⇄ winit)

Everything below is **already in the core**; only the **winit/egui view + Settings UI** lag.

**A. Settings UI drift (small — the real 1.0 requirement).** `mac/…/SettingsView.swift` has the
**Image Info** section; the egui Settings (`dialog.rs`) does **not**. Missing egui rows:
`show_image_info` + `info_show_folder` / `_filename` / `_resolution` / `_codec` (the
`info_line_align` position picker is already there). Also: the winit shell (`main.rs`) hardcodes
`info_line: false` — wire it to `settings.show_image_info` like `new_host` does. The winit HUD
info line **already honors** the field toggles (`info_line_content` gates each field); it just
lacks the Settings UI to set them, and shows no live/animated symbol (text-only — could append
"Live"/"GIF" if wanted).

**B. On-image panel richness (large — optional, post-1.0 candidate).** The winit shell still
renders the **original HUD** tree/inspector/help/etc. — functional, but the older, pre-redesign,
less-refined design (no Finder browser, no tabbed inspector, no material look). "Phase 4" =
bringing these up to the mac's polish **in egui**.

## Answering the owner's Phase-4 questions

- **Start over?** No. The core, models, settings, and `fs_tree`/`panels` are shared and done —
  only the egui **view layer** is new. `pb-ui` (egui component system: cards, toggles, buttons,
  slider, icons + a **gallery** example) already exists and styles the dialogs.
- **Can egui ape the SwiftUI look?** The broad strokes, yes (that's what `pb-ui` + the dialogs
  already do). It will be **less refined**: egui has **no native material/blur** (use a
  translucent solid fill), plainer fonts, and less-smooth animation/transitions than SwiftUI.
  Clean and functional, not glassy.
- **The architectural crux:** the winit **main window has no egui today** — on-image panels are
  HUD, egui lives only in the dialog window. Real Phase 4 (B) needs an **egui-over-wgpu pass in
  the main window** (egui context + input routing + a render pass after the photo). That seam is
  the hard part; once it exists, each panel is a moderate port of a shell-neutral model.
- **One big-bang automated session?** **Settings parity (A): yes, easily** — pure Rust/egui,
  testable on macOS via `cargo run -p pb-app`. **Full panel upgrade (B): draftable, not "done."**
  A workflow can stand up the egui-in-main-window seam + port every panel in one big session, but
  expect polish rounds and a **Windows-only validation pass** (WIC codecs, DXGI HDR, MSI, signing
  can't be tested from the Mac).

## Recommendation

Ship **Windows 1.0 on the existing HUD panels + egui Settings parity (A)** — small, low-risk,
all doable on macOS. Treat the **egui panel upgrade (B)** as a fast-follow, where the "big-bang
draft then polish" approach fits and a Windows box is on hand.

## Working notes for a new session

- **Run/iterate the egui shell on macOS:** `cargo run -p pb-app` (opens the winit+wgpu window;
  ⌘, / menu opens the egui Settings). `cargo run -p pb-ui --example gallery` previews components.
- **Gate:** `cargo fmt --all && cargo test --workspace && cargo clippy --all-targets -- -D
  warnings`. ⚠ Never run `apply_settings`/`apply_keymap` end-to-end in a test — they write the
  **real** `settings.toml` (unit-test the pure content/visibility fns instead, e.g.
  `info_line_fields_respect_the_settings_toggles`).
- **Where things live:** egui Settings + dialogs = `crates/pb-app/src/dialog.rs`; winit shell +
  HUD wiring = `crates/pb-app/src/main.rs`; egui components = `crates/pb-ui/`; shared settings =
  `crates/pb-app-core/src/settings.rs`; info-line/play-hint accessors + `native_*` flags =
  `app_core.rs` / `app_core_impl.rs`. Plans: `.taskmaster/docs/hud-panels-plan.md`,
  `folder-tree-plan.md`.
- **CHANGELOG:** add user-facing lines under `[Unreleased]`.

## Suggested order

1. **Settings parity (A)** — add the Image Info egui rows to `dialog.rs`; wire `main.rs`
   launch default. Verify on macOS with `cargo run -p pb-app`. (Ships Windows 1.0.)
2. **Sweep `dialog.rs` for any other mac⇄egui Settings drift** while in there.
3. **(Post-1.0) Panel upgrade (B)** — egui-over-wgpu main-window seam, then port tree /
   inspector / help / info line / toasts / play hint; Windows validation pass.
