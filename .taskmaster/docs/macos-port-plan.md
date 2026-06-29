# macOS port — plan & handoff (the "PhotoBlaze on the Mac" track)

_Written 2026-06-29. Authoritative plan for the macOS (Apple Silicon) port.
Grounded in a read of the actual tree on this date — cfg-guard counts, the
`Renderer`/`DecodeBackend` seams, and `pb-render/src/display.rs` — not a guess.
Sequenced **SDR-first**: a signed, notarized Mac build that runs is the goal of
M0–M3; HDR/EDR is an isolated follow-up (M4), not a blocker._

> Anchoring decisions: **ADR-002** (wgpu is the v1 renderer — DX12 on Windows,
> Metal on macOS, behind a `Renderer` trait) and **ADR-002a** (macOS is a cheap
> wgpu/Metal port, not a rewrite). This plan executes ADR-002a. **ADR-015** (the
> `libheif` cargo feature is OFF by default; pure-Rust core builds with no C
> toolchain) shapes the M0/M1 split. HDR-on-macOS is the deferred slice of the
> HDR work, not the gated zero-copy escalation (ADR-012) — that stays v2 on both
> platforms.

---

## TL;DR — the shape of the work

Windows took ~2.5 days because it was building *everything from zero* (logic,
the HEIC saga, the `pb-ui` design system, packaging). The Mac port **reuses all
of that** — it's translation, not invention, and the seams are already cut. The
honest estimate is **~1 focused day to a signed, notarized SDR beta**, +~½ day
for EDR/HDR later.

**What ports untouched (verified — 0 platform code):** `pb-core`, `pb-source`,
`pb-ui` (0 cfg-guards, 0 win32-touching files each). Scanning, navigation,
archive/7z reading, the entire egui design system: free.

**This is one monorepo, not a fork.** The Mac port is `#[cfg(target_os = "macos")]`
siblings filling in beside the existing `#[cfg(windows)]` arms in the *same* files,
plus a per-OS CI matrix for packaging. `cargo build` already selects the right
backends per-OS (wgpu→Metal, winit, muda→NSMenu, `windows` crate gated out under
`[target.'cfg(windows)'.dependencies]`). Windows keeps building the whole time —
the very first fix below (`Backends::PRIMARY`) proves it: one edit, both platforms.
Do the work on `main`/short-lived branches, never a long-lived divergent fork.

**Where the actual work is (re-verified 2026-06-29, second pass):**
| Crate | cfg(windows) guards | win32/d3d12/wic files | Mac work |
|---|---|---|---|
| `pb-core` | 0 | 0 | none |
| `pb-source` | 0 | 0 | none |
| `pb-ui` | 0 | 0 | fonts + theme-source (small) |
| `pb-render` | 2 | 1 | **backend mask fixed ✅** (M0); EDR detect + Metal surface poke (M4, deferrable) |
| `pb-decode` | 5 | 5 | libheif build branch *or* Image I/O backend (HEIC/AVIF) |
| `pb-app` | 19 | 4 | **the bulk** — menu *wiring* (muda, cross-platform), file-assoc, window, icon |

> Counts corrected from the first-pass estimate (decode 8→5, app 23→19) by a direct
> `grep` of `crates/*/src`. The over-count was harmless; the shape is identical.

---

## Step zero — Apple Developer account
**DONE / not a blocker.** JD has shipped notarized macOS apps before; the
Developer ID identity, notarization workflow, and tooling are already in hand.
Noted here only so a fresh session doesn't re-derive it as a risk.

---

## M0 — Compile green on Apple Silicon (arm64-osx)  — ⚙️ MOSTLY DONE
Get the workspace *building and running* on macOS before touching any feature.

**Status (verified 2026-06-29 on an M2 Max, Metal 4):**
- ✅ `cargo check --workspace` (no `libheif`) is **green** — 0 errors, only
  dead-code warnings. Every Windows API already has a `cfg(not(windows))` stub
  (e.g. `archive::available_physical_ram`), so the tree compiles as-is on Mac.
- ✅ **Decode works** on Mac (`pb-decode --example decode` on real JPEGs: OK).
- ✅ **Metal render works** — after the one real M0 bug below. `offscreen_png`
  renders a real photo (fit-to-screen, letterboxed) and the full `pb-render`
  suite passes **26/26 on Metal**, golden images included.
- 🐛 **Fixed:** `pb-render` created its wgpu instance with
  `Backends::DX12 | Backends::VULKAN` (in *two* places — `gpu.rs` and
  `upload.rs`), excluding Metal → "no GPU adapter" on Mac. Changed to
  `Backends::PRIMARY` (DX12 on Windows / Metal on macOS / Vulkan on Linux). This
  is the monorepo principle in miniature: DX12 stays primary on Windows, so the
  fix is platform-neutral.

**Remaining for M0:**
- The dead-code warnings *are the feature gaps*. The loud one: the entire `muda`
  menu (`menu::build_menu`, `ensure_menu`, the `menu`/`menu_attached` fields) is
  **dead on Mac** because `apply_menu_for_mode` has a real `#[cfg(windows)]` body
  and an **empty `#[cfg(not(windows))]` stub** (`main.rs:1990`). That's M2 work,
  not M0 — but it's why a Mac window currently has no menu bar.
- Confirm the **actual window** opens and the **prefetch/held-key fly** path runs
  (the offscreen example proves decode+render, not the winit swapchain loop).
- **Exit criteria:** `cargo run -p pb-app --release -- <folder of JPEGs>` opens a
  window on macOS and flies through JPEG/PNG with the prefetch engine working.

## M1 — HEIC decode on macOS (two options; pick or do both behind the seam)
The `DecodeBackend` seam + `route_full_heic` + `PB_HEIC_BACKEND` env switch
already exist (one binary, A/B-able). Mac slots in behind them.

- **Option A — port libheif (mirrors Windows).** `libheif.rs` is a hand-rolled
  `extern "C"` surface with no `libheif-sys` version coupling, so the *Rust* side
  is already portable. Work is in **`build.rs`** (add a macOS branch emitting link
  dirs) + a Mac equivalent of `scripts/setup-libheif.ps1` (Homebrew `libheif`, or
  vcpkg `arm64-osx`). **⚠️ Carry `-DENABLE_PLUGIN_LOADING=OFF` to the Mac build
  too** — and note the header's "plugins only supported on Unix" means the dynamic
  plugin scanner is *more* active on macOS than Windows, so this flag matters more
  here, not less. Static-link libde265 as on Windows → no dylibs to ship.
- **Option B — Apple Image I/O backend (the Mac-native win).** A new
  `DecodeBackend` using `CGImageSource` (Image I/O). **Two advantages over
  libheif on Mac:** (1) it's the OS decoder Apple already ships, fast and
  hardware-assisted; (2) **it's patent-clean** — Apple holds the HEVC license, so
  the Mac path sidesteps the HEVC-patent exposure that bundled libheif carries on
  Windows (see the patent note in `decisions.md` / packaging). This is the
  "OS-decode is the license-clean default" strategy realized on Mac for free.
- **Recommendation:** ship **Option B as the Mac default** (fast + patent-clean),
  keep Option A buildable behind the feature flag as a cross-check / fallback. The
  `colr`/brand parse already factored out of `wic.rs` is reusable for color parity.
- **Exit criteria:** an iPhone HEIC folder opens on Mac with preview-first scroll
  and sharpen-on-land; color matches the Windows render (reuse `heic_compare`).

## M2 — Platform glue in `pb-app` (the bulk of the work)
- ✅ **Menu bar — DONE (2026-06-29).** It was `muda`, so this was wiring, not
  invention. Landed: a `#[cfg(target_os="macos")]` `menu::build_menu` with the
  standard **App menu** (About / Settings ⌘, / Hide / Quit ⌘Q — the first submenu
  becomes the bold app menu under `init_for_nsapp`) and **real ⌘ accelerators**
  (`Modifiers::SUPER`) for Copy/Save/Open/Settings/Quit; clean labels (no Windows
  `\t` hint text). Attach via `App::apply_menu_for_mode` → `Menu::init_for_nsapp()`
  once at window creation (the bar auto-hides in fullscreen but its ⌘-equivalents
  stay live). Clicks/⌘-keys already dispatch through the cross-platform
  `muda::MenuEvent` path — unchanged. **Keymap made Cmd-aware:** `KeyChord` gained a
  `logo` (⌘/Win) field so OS-standard ⌘-chords are distinct from the bare keys
  (else ⌘S→Slideshow, ⌘R→Rotate would misfire); the winit keymap stays the owner of
  bare-key fast-nav, NSMenu owns the ⌘-commands — no double-fire. Tests: 20 keymap +
  2 menu + 119 pb-app bins green on macOS.
  - **Still TODO (menu polish):** a `Window` menu (Minimize ⌘M); running as a real
    `.app` bundle so the app activates frontmost (a bare-binary launch attaches the
    menu correctly but may not become the active app, so the bar can show behind the
    launching terminal — cosmetic, fixed by M3's bundle).
- **File associations / "Open with":** Windows registry + WiX → macOS
  **`Info.plist`** `CFBundleDocumentTypes` + `UTImportedTypeDeclarations` /
  `LSItemContentTypes` for the image UTIs. Drives double-click + drag-onto-Dock.
- **Fullscreen — decided (2026-06-29): support BOTH.** The everyday mode is the
  **borderless windowed-fullscreen** (`toggle_fullscreen` already does it via
  portable winit calls — `set_fullscreen(None)` + decorations off + size-to-monitor
  — staying in the current Space, no swoosh, lowest latency, ⌘-equivalents live).
  Bound to **F** (new, discoverable), **Option+Enter** (`Alt+Enter`), and **F11**;
  the bare `F` binding was added on *both* platforms. The **native Spaces
  fullscreen** (`toggleFullScreen:`, the green-button / ⌃⌘F / Globe+F behavior) is
  left available for whoever wants it — it doesn't conflict, since it's on different
  shortcuts. ✅ Bare-`F` + Cmd-aware keymap landed. **TODO:** keep our `windowed`
  state in sync if the user triggers *native* fullscreen externally (green button /
  Globe+F) — winit emits a resize but our flag/checkmark can desync; minor polish.
  Globe+F intentionally drives *native* fullscreen (it's a system key winit can't
  redirect to the borderless path).
- **DPI:** **a gift** — the Per-Monitor-V2 manifest pain is gone; macOS backing
  scale is automatic. Mostly deletion.
- **Icon:** `.ico` → **`.icns`**. ⚠️ Tahoe wants the icon *inside* the rounded-rect
  squircle — the flame breaking the bounding box that's correct on Windows is
  **not** allowed on Mac. Needs a macOS icon variant (flame tucked in / recomposed).
- **Theme + fonts (`pb-ui`):** the crate's "Windows-tracking light/dark theme"
  needs a Mac source — `NSApp.effectiveAppearance` (or the `dark-light` crate);
  and the Segoe UI references → **SF Pro / system font**. Small but visible.
- **Exit criteria:** app feels native — system menu bar, ⌘-shortcuts, double-click
  a HEIC in Finder opens PhotoBlaze, dark/light tracks System Settings.

## M3 — Packaging: `.app` → codesign → notarize → staple → DMG
- ✅ **`.app` assembly — DONE (2026-06-29, task #1).** Hand-rolled (tool-agnostic, so
  any notarization workflow consumes it): `packaging/macos/Info.plist` (id
  `com.jdlien.PhotoBlaze`, min OS 11.0, version from Cargo) + `scripts/bundle-macos.sh`
  → `target/<profile>/bundle/PhotoBlaze.app`. **Verified via Accessibility:** launches
  **frontmost** through LaunchServices (fixes the bare-binary activation cosmetic); the
  menu bar reads *PhotoBlaze · File · Edit · View · Image · Help* with the app menu
  holding About / Settings / Quit and ⌘-accelerators rendering (Copy = ⌘C); clicking
  *Quit PhotoBlaze* dispatched through our `Action::Quit`. Placeholder `.icns` from the
  1024 master (real squircle = task #7).
- **Remaining:** **Developer ID** codesign, submit for **notarization**, **staple** the
  ticket, wrap in a **DMG** (task #11). JD has this muscle memory already (step zero).
- Decide the universal-vs-arm64 question (arm64-only is fine for a v1 beta; add
  x86_64 / universal2 only if a beta tester needs Intel).
- **Exit criteria:** a notarized DMG runs on a *clean* Mac (no dev tools, fresh
  user) with **zero Gatekeeper warnings**, and the release workflow builds it.

## M4 — Wide-gamut (P3) + HDR/EDR on macOS  (tasks #3, #4)
**Premise correction (verified 2026-06-29 in `pb-render/src/gpu.rs`):** there is
**no d3d12 render path** — the renderer is 100% wgpu (that's why it already drew on
Metal). Windows wide-gamut/HDR rides on **one implicit DXGI behavior**: *"a float
flip-model swapchain is always scRGB"* (`gpu.rs:1065`), so configuring an
`Rgba16Float` wgpu surface yields extended-range scRGB output **for free** — there
is **no** explicit color-space poke (the earlier "lone d3d12 poke" note was wrong;
the only Windows code is `display.rs::primary_hdr()`, pure **detection**). The
in-shader CMS (source-primaries → BT.709 + TRC) is already portable.

macOS has **no** "fp16 = scRGB free" behavior, so the shim is two seams:
- ✅ **(#3) Detection — DONE (2026-06-29).** `display.rs` gained a
  `#[cfg(target_os="macos")]` `primary_hdr()` via `NSScreen` (raw `objc2` `msg_send!`,
  AppKit force-linked): **potential** EDR headroom
  (`maximumPotentialExtendedDynamicRangeColorComponentValue`) + `canRepresentDisplayGamut(P3)`.
  Added a `wide_gamut` field to `DisplayHdr` so an SDR-P3 panel still lights up the
  wide path. Verified on hardware via `cargo run -p pb-render --example hdr_probe`
  (Studio Display → P3, EDR ×2.0; Odyssey G95NC → P3, SDR). **Follow-up:** queries
  `mainScreen`, not the window's screen — on a multi-display setup the EDR value can
  be for the wrong panel (P3/wide_gamut is right for all-P3 setups regardless).
- ✅ **(#4) Surface poke — DONE (2026-06-29, needs on-device visual validation).**
  `gpu.rs` gate is now `want_fp16 = (hdr_on || wide_gamut) && Rgba16Float` (lights
  up the fp16 scRGB surface on any P3+ panel). `pb-app/src/hdr_surface.rs` reaches
  the `CAMetalLayer` (raw-window-handle → `[NSView layer]`) and sets `colorspace =
  extendedLinearSRGB` + `wantsExtendedDynamicRangeContent`, re-asserted on resize.
  ⚠️ Gotcha fixed: objc2 verifies `msg_send!` arg encodings in debug builds, so
  `setColorspace:` needs a typed `*const CGColorSpace` (`^{CGColorSpace=}`), not a
  bare `*const c_void` — passing the latter aborts at launch (panic can't unwind
  through the AppKit `app_did_finish_launching` callback). Runs clean on the fp16
  surface; **visual P3/HDR confirmation + brightness calibration are the open
  on-device step.** Original (superseded) spec follows:

- **(#4) Surface poke — (original spec).** After
  `surface.configure` with `Rgba16Float`, reach the `CAMetalLayer` (`raw-window-handle`
  → `NSView.layer`, or the wgpu Metal HAL — `raw-window-handle`/`objc2-quartz-core`/
  `objc2-metal` are all already in the tree) and set `colorspace = extendedLinearSRGB`
  (= scRGB, matching the shader output) + `wantsExtendedDynamicRangeContent = true`
  (+ `CAEDRMetadata` for PQ/HLG). **Plus** flip the `gpu.rs` gate from
  `want_fp16 = disp.hdr_on && …` to `(disp.hdr_on || disp.wide_gamut) && …`, and
  **re-assert the layer props after every reconfigure (resize)**. This is the one
  piece that can't be unit-tested (it bypasses `offscreen_png`) and must not ship
  the gate-flip without the poke (an fp16 layer left in the display's sRGB space
  would clip/misrender) — so it's done as a focused step with on-device visual
  validation. Plumbing: `RenderState::new`/`WgpuRenderer::new` need the window's raw
  handle alongside the surface target (`pb-app/src/main.rs:3316`).
  **Re-assert after every reconfigure (resize).** No shader changes.

**Decision (owner, 2026-06-29): "best mode the display can handle."** Unlike
Windows (wide-gamut gated behind HDR-desktop-mode), macOS enables the wide-gamut
fp16 surface **whenever the panel is P3+ — SDR included** (every modern Mac), and
layers EDR headroom when `maxEDR > 1.0`. Cost ≈ zero (present is one textured quad;
fp16 bandwidth at display res is trivial), and it lights up correct-saturation P3
photos universally — a bigger everyday win than HDR. P3 and EDR are separable:
the `colorspace` poke alone gives P3 on SDR; `wantsExtendedDynamicRangeContent`
adds the HDR headroom on XDR/mini-LED.

- **Testing wrinkle:** `offscreen_png` validates the color *math* but **bypasses the
  surface**, and EDR content resists ordinary screen capture (cf. the Windows
  all-white-grab) — so the wide-gamut/EDR surface itself needs on-device visual checks.
- **Exit criteria:** a P3 photo shows full saturation on any P3 Mac (SDR); an HDR
  (PQ/HLG) HEIC shows real extended-range highlights on an XDR/Pro Display; SDR-sRGB
  panels unchanged; no regression on Windows.

---

## Sequencing & the one rule
**M0 → M1 → M2 → M3 ships the SDR beta. M4 is a clean follow-up.** Do **not** let
EDR gate the first Mac build — the renderer already degrades to SDR and compiles,
so a runnable Mac PhotoBlaze is much closer than the HDR work suggests. Ship SDR,
get it in front of a Mac user (this is also the *visibility* step), add EDR second.

## Open questions for the owner
1. ~~**Menu:** hand-rolled Win32 or a crate?~~ **ANSWERED: it's `muda` 0.19**
   (cross-platform), and the menu *model* (`build_menu`, `action_for`,
   `dispatch_menu`) is already platform-neutral. M2's biggest item **collapses to
   wiring**: implement the `#[cfg(not(windows))]` arm of `apply_menu_for_mode`
   with muda's `Menu::init_for_nsapp()` (the global NSMenu — no per-window hwnd),
   and route `muda::MenuEvent` the same way the Windows path does. The Win-only
   bits to leave gated: `darkmode.rs` (uxtheme) and the `init_for_hwnd`/per-window
   show/hide dance (macOS has one app-global bar, attached once).
2. **Decode default on Mac:** Image I/O (Option B, recommended — fast + patent-clean)
   vs libheif-everywhere for cross-platform parity? (This plan assumes B as default.)
3. **arm64-only vs universal2** for the v1 beta? (arm64-only assumed.)
4. **Min macOS version** — EDR APIs are old, but pick a floor for `Info.plist`.

## Verified-on-2026-06-29 grounding (so a fresh session trusts this)
- `pb-core`/`pb-source`/`pb-ui`: 0 cfg-guards, 0 win32 files.
- `pb-render`: wgpu-based (`lib.rs`/`upload.rs`/`gpu.rs`); HDR = fp16 scRGB surface;
  only `display.rs` (DXGI detection) + one swapchain poke are Windows-locked.
- `pb-decode`: 8 cfg-guards; WIC backend is Windows-only (dead weight on Mac);
  `libheif.rs` Rust surface is portable; `route_full_heic` + `PB_HEIC_BACKEND`
  give the A/B seam.
- `pb-app`: 23 cfg-guards across 4 win32-touching files — the real glue budget.
