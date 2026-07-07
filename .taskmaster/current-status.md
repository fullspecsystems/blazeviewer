# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-07._

## What we worked on (this session)
- **macOS toolbar (task #55):** hold-to-fly on the nav/random buttons — **committed + pushed** (`a79674a4`).
- **Transparent Toolbar (task #59) — NEW, UNCOMMITTED:** windowed mode extends the canvas under a
  translucent glass toolbar so a zoomed/filled photo shows *under* the bar (fit mode unchanged),
  with a top **legibility scrim** and a **Settings ▸ Appearance ▸ "Transparent toolbar"** toggle
  (default on). Debug env flag removed — the setting drives it. Owner likes it; needs a final
  on-device look at the scrim + toggle before commit.
- **Started task #58:** auto-size the Settings window (in progress — nothing written yet).

## Relevant files (uncommitted #59 work)
- `crates/pb-render/{gpu.rs,lib.rs}` — `Renderer::set_content_top_inset(px)`; `quad_vertices` fits
  against `surface_h − inset` and offsets the quad down by the inset.
- `crates/pb-app-core/src/settings.rs` — `glass_toolbar: bool` (default `true`).
- `crates/pb-mac-ffi/src/lib.rs` — FFI: `set_content_top_inset`, `glass_toolbar` form field.
- `mac/.../CoreModel.swift` — `glassToolbar` (reads setting via `refreshGlassToolbar`),
  `updateContentTopInset` / `glassTopInsetPoints` / `glassScrimVisible`, glass branch in
  `assertWindowChrome`.
- `mac/.../GlassTopScrim.swift` (new) — gradient scrim: ease-out stops (`0.58→…→0`), darker top,
  height ≈ `inset*1.08`, dark-in-dark / light-in-light.
- `mac/.../PhotoBlazeMacApp.swift` — scrim overlay (lowest); `refreshGlassToolbar()` on launch.
- `mac/.../SettingsView.swift` — the toggle + per-tab frame heights.

## Current problem — task #58 (auto-size Settings window)
Each tab hard-codes a frame height (General 615 / Appearance 615 / AI 645 / Shortcuts 640). Owner
is tired of hand-tuning; **Appearance 615 is a hair too tall** (bottom padding > sides). Root cause
of the 5 past failures: **SwiftUI `Form` doesn't self-size vertically** (it fills). Plan: an
`AutoSizingPane` wrapper that measures a hidden `.fixedSize(vertical:)` copy → frames the visible
copy to `min(natural, screenCap)`, with a per-tab **fallback** so it never breaks. Clamp to the
screen; taller-than-screen content scrolls.
⚠ **Double-render is only safe for General + Appearance.** **AI** pane has
`.onAppear { autoListModelsIfNeeded() }`; **ShortcutsPane** installs an `NSEvent` key monitor in
`onAppear` — rendering those twice = double network calls / double key capture. Keep AI + Shortcuts
at a **capped fixed height**. Can't verify SwiftUI sizing headlessly (screen capture blocked) —
needs owner at the console.

## Next tasks (priority order)
1. Build the auto-sizer: General/Appearance measured; AI/Shortcuts capped fixed; clamp to screen.
2. Owner: confirm scrim look + the toggle flips glass on/off cleanly, on device.
3. Commit the task #59 set + a `CHANGELOG.md` entry ("Transparent toolbar").
