# macOS native UI — plan (the "Mac-assed Mac app" track)

_Written 2026-06-30. The plan for replacing the egui chrome on the **macOS
target** with a native AppKit/SwiftUI shell, so PhotoBlaze on the Mac feels —
per John Siracusa — like a properly **Mac-assed Mac app**: native window, native
controls, native preferences, idiomatic everything. Grounded in a direct read of
the tree on this date (`pb-render/src/gpu.rs`, `pb-app/src/dialog.rs`,
`pb-render/src/lib.rs`, the workspace manifests), not a guess._

> **Relationship to the other plans.** This is the **successor to
> [`macos-port-plan.md`](macos-port-plan.md)**, not a replacement. That plan
> (M0–M4) gets a signed, notarized **egui-on-Mac** beta running — native menu
> (muda, done), Finder integration, dark/light, packaging, EDR. **Ship that
> first.** This plan is the post-beta quality track that swaps egui chrome for
> SwiftUI. Milestones here are **NS0–NS3** ("Native Shell") to avoid collision
> with that plan's M-series.
>
> Anchoring decisions: **ADR-002/002a** (wgpu behind a `Renderer` trait; macOS is
> a cheap Metal port), **ADR-007** (workspace + trait A/B seams), **ADR-019**
> (`pb-core::open` already normalizes every entry point into a `LaunchInput` — the
> file-open path is *already* delivery-layer-only). The decision is recorded as
> **ADR-021** (native shell on the Mac target via an AppKit-owned host + an extracted
> platform-neutral `AppCore`) — **ratified in `decisions.md`, 2026-06-30**, with the
> arm64-only / macOS-14 / native-About parameters resolved (see the bottom of this doc).

---

## TL;DR — the shape of the work

The egui-on-Mac port is *translation* (cheap; `macos-port-plan`). This is
*inversion* (real, but bounded). Today **winit owns** `main()`, the window, the
event loop, and the frame pump; egui dialogs are secondary winit windows. A
Mac-assed app needs **AppKit/SwiftUI to own** the window + run loop, with the Rust
engine as a linked library it drives. That's two foundational changes; everything
else is reuse.

**This is the larger, later track — weeks, not the ~1 day the egui port took.**
The risk is open-ended yak-shave on a working app (the documented "build-not-ship"
failure mode). So the entire plan is sequenced **strangler-fig: a shippable build
exists at every step**, the SwiftUI shell lives behind a build flag, and the
egui-on-Mac beta stays the default Mac artifact until NS3 cuts over.

**Risk posture after Codex review (2026-06-30):** this is a good, feasible plan
with a meaningful native-Mac payoff, but it is **not** "low-risk UI polish." The
SwiftUI views are the easy part. The hard parts are (1) extracting a tested
`AppCore` without changing behavior, and (2) proving the AppKit-owned
`CAMetalLayer`/wgpu/frame-pump bridge. Treat NS0 and NS1 as proof gates: if either
one gets messy, stop there and keep the egui-on-Mac beta shipping. Do not cut over
until both gates are boring and measured. **Update (2026-06-30):** NS0's low-risk
half is landed — the shell-neutral model + tested seams are extracted into
`pb-app-core` (see NS0 below). The *remaining* NS0 gate is the `AppCore`-struct
ownership inversion, still un-started and still the behavior-sensitive part.

**Windows invariant (the one rule):** every change here is either platform-neutral
refactor (NS0) or `#[cfg(target_os = "macos")]` / a Mac-only target (NS1–NS3).
Windows builds the egui shell, untouched, the whole way through. If a change to
land "native Mac UI" requires touching the Windows code path, it's wrong — back it
out and find the cfg seam. (Same discipline as `macos-port-plan`'s "one
monorepo, not a fork.")

---

## North star — what "Mac-assed" means here (the NS3 bar)

Not "looks like macOS." *Is* macOS, idiomatically:

- Real `NSWindow` chrome, real traffic lights, native full-screen behavior, window
  state restoration across launches.
- A **Settings window that looks like System Settings** (SwiftUI `Settings` scene),
  not a custom panel — `⌘,` opens it, it's a real preferences window.
- Native About (the standard `NSApplication` about panel, or a tasteful SwiftUI
  one), native alerts (`NSAlert` / SwiftUI `.alert`) for delete-confirm and errors.
- SF Pro everywhere, `NSColor.controlAccentColor`, vibrancy/materials where they
  belong, dark/light tracking `effectiveAppearance` instantly.
- Native text editing (the password field gets the emoji picker, spelling, the
  Services menu — for free, because it's a real `NSTextField`).
- VoiceOver/accessibility on the chrome (egui has none).
- Trackpad gestures (pinch-zoom, two-finger pan) as first-class `NSGestureRecognizer`s.

The image canvas stays a wgpu/Metal surface — that's correct and Apple would agree;
SwiftUI's job is the *chrome*, never the hot path.

---

## Already reusable (verified — do not rebuild)

The seams JD already cut mean the engine ports untouched:

- **Renderer is mostly host-agnostic, but the Mac surface constructor needs one
  explicit adapter.** `WgpuRenderer::new` currently takes
  `target: impl Into<wgpu::SurfaceTarget<'static>>` (`pb-render/src/gpu.rs`) and
  works for a winit `Window`/raw-window-handle. A live `CAMetalLayer` is **not**
  the same safe `SurfaceTarget` path in wgpu 22; it must go through
  `Instance::create_surface_unsafe(SurfaceTargetUnsafe::CoreAnimationLayer(ptr))`.
  Add a macOS-only constructor such as `unsafe WgpuRenderer::new_from_ca_layer(...)`
  or a small `SurfaceHost` adapter in NS1, with the layer lifetime/main-thread
  rules documented and tested. Metal is still the backend (`Backends::PRIMARY`);
  the EDR layer configuration code already exists and should be reused/adapted.
- **Shaders are WGSL, compiled at runtime by naga** → no `.metal` build step in the
  Xcode target; the shaders ship inside the Rust lib.
- **Progress is already UI-agnostic.** `OpenProgress` / `ScanProgress` are `Arc`
  handles the worker threads bump and the dialog merely *reads*
  (`dialog.rs`: `progress.fraction()/done()/total()/found()/current()/request_cancel()`).
  NS0 should move/export `ScanProgress` with `AppCore` and expose FFI-safe snapshot
  methods for both handles. A SwiftUI progress view then reads the same handles over
  FFI. The hard part (threading) is done.
- **Settings is a clean serde model** (`settings::Settings`). `SettingsDraft` in
  `dialog.rs` is private and egui-shaped; SwiftUI should bind a public FFI-safe
  `SettingsForm`/`SettingsPatch` mirror and hand back a validated `Settings`.
- **One dialog mechanism, not seven.** All kinds (About, Settings, Confirm,
  Message, Password, Loading, Scanning) route through `DialogWindow`. We replace
  one thing.
- **Launch/open is already a delivery-layer shim** (ADR-019: `pb-core::open` →
  `LaunchInput`). The Mac `openFiles` Apple Event and the native file panel just
  build a `LaunchInput`. Nothing to re-architect there.
- **Menu model + file-open policy are already reusable, but the shell pump must be
  rewired.** The macOS `muda` menu already follows Mac conventions and dispatches
  by stable ids; `pb-core::open` already owns launch policy. Under an AppKit-owned
  `NSApp`, NS1 must explicitly decide whether to keep `muda` and drain its
  `MenuEvent` receiver from the Swift shell, or rebuild the same menu in Swift/AppKit
  from the pure action model. The existing winit `about_to_wait` pump disappears.
  File panels can stay `rfd` only if they compose cleanly under AppKit; otherwise
  use native Swift `NSOpenPanel` and return the same `LaunchInput`.

Decoders, renderer, progress, settings model, launch path, menu model, and file
open policy are reused. Net-new risk surface is focused: the host window, the
`CAMetalLayer` surface adapter, the FFI bridge, input mapping, menu/event pump,
and the seven dialog *views*.

---

## The two foundational pieces

**1. Ownership inversion.** AppKit/SwiftUI owns the `NSWindow` and the run loop;
the canvas is an `MTKView` whose `CAMetalLayer` is handed to Rust through the
macOS-only surface adapter; the **frame pump inverts** — instead of winit waking
`about_to_wait` and calling `render()`, the AppKit shell calls into Rust on
`MTKViewDelegate.draw(in:)` / scheduled ticks. Input inverts too: `NSEvent` /
gesture recognizers replace winit `WindowEvent`s. The Swift shell supplies
surface size, display/EDR headroom, focus, input, open/drop/menu events, and wake
deadlines; Rust keeps the decode/prefetch/render orchestration.

**2. Extract `AppCore` from the winit god-object.** `pb-app/src/main.rs` is **256 KB**
(over the read limit) — that size *is* the diagnosis: input→action dispatch, nav
state, slideshow timing, held-key pacing, and dialog choreography live tangled
inside a winit `ApplicationHandler`. For anything but winit to drive the app, lift
a platform-neutral `AppCore` out of it: intent-level commands in, state/effects
out, *driven by* the shell instead of *being* it. `pb-core` is already pure; this
is about the orchestration layer above it.

The dialogs are downstream of both — until there's an AppKit host with an `MTKView`
and an `AppCore` to call, a SwiftUI Settings view has nowhere to live and nothing
to talk to.

---

## NS0 — Extract `AppCore` (pure Rust, no Swift, winit still drives) — the keystone

Refactor only. No Swift, no behavior change. The winit shell becomes a thin adapter
over a platform-neutral controller; everything keeps working on Windows **and** the
egui-on-Mac beta.

### Landed (2026-06-30, on branch `swiftui`) — the shell-neutral seams

The strangler-fig groundwork is in and green (full workspace suite, clippy, fmt), all
behavior-preserving with Windows/winit + egui untouched. Main's animated-image support
(GIF/APNG/WebP + macOS Image I/O; `P` play/pause, `,`/`.` frame-step) was merged in and
flows through the new seams unchanged.

- **`pb-app-core` crate exists** — `toml`-only, **no winit/egui/wgpu/muda/rfd/objc**.
  Modules: `action`, `pb_key`, `keymap`, `slideshow`, `config` (the shared config dir),
  `contract`, `timing`. The winit shell re-exports them at its crate root
  (`use pb_app_core::{action, contract, keymap, pb_key, slideshow, timing}`), so the
  existing `crate::…` paths in shell modules resolve unchanged (the crate move stayed
  invisible to the rest of `pb-app`, incl. the feature branch it was merged with).
- **App-owned physical-key model** — `PbKey` + the `pb_key_winit` adapter
  (`winit::KeyCode → PbKey`); `KeyChord` stores `PbKey`, not winit `KeyCode`. The keymap
  is now winit-free. (The `NSEvent → PbKey` adapter is NS1.)
- **Contract vocabulary sketched** (`contract.rs`) — `CoreEvent` / `CoreEffect` /
  `Modifiers` / `MenuState` / `KeyResolution` + value enums (`DialogKind`, `ScaleMode`,
  `InfoOverlay`, `WindowMode`, `CursorKind`). This is the *vocabulary*, not yet the
  driving loop; payloads that need types still in the shell or other crates (a `Settings`
  form, `LaunchInput`, dialog request/result, the GPU surface handle) are marked
  `NS-later`.
- **Two contract types already proven against the live winit shell:**
  - **`MenuState`** — one pure `menu_state_from` derive + one diffed `apply_menu_state`
    replaced the five `refresh_*` methods and their five caches (checkmarks, Save Rotation
    / Stop Scanning / Undo enabled+label, macOS native-fullscreen label), preserving the
    per-item cached/no-op behavior.
  - **`KeyResolution`** — `resolve_key_down` routes a KeyDown by `ActionKind`
    (incl. the merged `FrameStep`), folding in the repeat gate and the ⌘-no-fall-through
    rule; the four modifier bools were unified into `contract::Modifiers`.
- **Timing moved into core** — `slideshow` (dwell), `timing::advance_interval` (the
  accelerating hold-to-fly gap), and `timing::elapsed_since` (the shared tap-delay /
  repeat gate now used by *both* the nav hold-to-fly and the frame-step scrubbing paths).
  Pacing *math* is unit-tested in a pure crate; the shell keeps the control flow.

### Still to do — the keystone (proof-gate; needs a critical call + manual validation)

The remaining NS0 work is the actual **`AppCore` struct** and the ownership inversion —
the behavior-sensitive part the plan gates on:

- **Lift the orchestration state** out of the winit `App` god-object (`main.rs`) into an
  `AppCore` that owns nav / prefetch / cache-residency / dialog / menu state and processes
  `CoreEvent` → `CoreEffect`, reducing the winit `App` to a thin
  `WinitShell: ApplicationHandler` that translates events→commands and effects→winit/render
  calls. Separate (a) orchestration/state → `AppCore`, (b) shell-owned objects (`Window`,
  `ActiveEventLoop`, dialog windows, native menu handles, file panels, clipboard), and
  (c) renderer/surface ownership (prefer `AppCore` owning a `Box<dyn Renderer>` once the
  shell creates the surface, so render is unit-testable with a fake renderer).
- **Firm up the deferred contract payloads** (`Settings` form, `Open(LaunchInput)`,
  `PasswordSubmitted`, dialog request/result, `Started{surface,…}`) as each is wired.
- **Coverage before the Swift target:** focused parity tests for action dispatch, held-key
  state, wake scheduling, dialog/effect sequencing, and scan/archive cancellation.
- **Why gated:** this touches the self-paced-advance control flow, dialog choreography, and
  the whole event loop — so it's the proof gate. Run a manual **hold-to-fly + frame-step +
  dialog + shortcut-editor** smoke on the egui-Mac (and ideally Windows) build before/at
  this step, and stop if it gets messy (the egui-Mac beta stays shippable regardless).

- **Exit criteria:** identical behavior on Windows and egui-on-Mac; `AppCore` is
  unit-testable with no winit/egui dependency; `cargo run -p pb-app` still works; full
  suite green on both platforms; zero user-visible change. *This phase is valuable even if
  SwiftUI never happens* (it de-tangles the Windows egui side too).

**Status (2026-06-30, branch `swiftui`) — the ownership inversion is underway and green.**
The full execution log + measurement methodology + the resume point live in
**`ns0-appcore-inversion-brief.md`** (read it before continuing). In short, all committed +
green + owner-smoke-verified:
- `event_loop` **fully de-threaded** (68 → 5 params); `CoreEffect` queue + `drain_effects`
  seam; **dialog opens deferred** through the shell (ckpts 1–3).
- Keystone **step 1**: `Active` split into `window` + `renderer` fields (perf-neutral,
  *measured flat*). **Step 2**: window ops (`set_title`/`request_redraw`/cursor) → effects
  (*measured flat*; work relocated to the drain). **Step 3**: `renderer` → `Box<dyn
  Renderer>` — the `Renderer` trait extended with the 9 previously-inherent methods; field is
  now `Option<Box<dyn Renderer>>` (*measured flat*: `present` p50 0.177 ms / `drain` p95
  0.135 ms, vs step-2's ~0.16 / ~0.13; the vtable dispatch is ~1 ns).
- **Instrumentation added** (`present` + `drain` `--metrics` stages) so every step is
  before/after measured against a pinned corpus — the prime directive, applied to the refactor.
- **Effect-seam (step 4a–4e) complete:** menu → `SetMenuState`, clipboard → `WriteClipboard`,
  rfd panels → `OpenFilePanel/OpenFolderPanel`, dialog results → `DialogOutcome` (partial;
  clean seam folds into step 5), fullscreen → `SetWindowMode`. Orchestration no longer calls
  muda/rfd/clipboard/window-mode directly. All green + committed, **owner-smoke pending**.
- **Remaining:** step 5 — the physical move of orchestration state + methods into `pb-app-core`
  as `AppCore` + a thin `WinitShell`. The single `AppCore` object is the remaining gate; do it
  on a smoke-verified base, incrementally (see the brief's step-5 increment order).

## NS1 — Minimal SwiftUI/AppKit host: canvas only (proves the inversion)

Stand up the Mac app target. **Behind a build flag / separate target** so the
shippable egui-on-Mac beta is never broken. No dialogs yet.

> **✅ ITEMS 1–3 of 10 DONE (2026-07-02; foundation was `59fdd77`).** FFI = **`swift-bridge`**
> (owner-confirmed). The host is a **SwiftPM executable** (`mac/`, macOS 14+ arm64 — not an
> .xcodeproj; `open mac/Package.swift` gives the full Xcode IDE), built by
> **`scripts/build-swift-host.sh`**: cargo staticlib → the `create-package` bin (swift-bridge's
> `create_package`) wraps it + the generated glue into the xcframework-backed local package
> `crates/pb-mac-ffi/PbMacFfi/` → swift build → `PhotoBlazeMac.app`. **Proven live:** the
> Esc→`ShellFlowAction("quit")`→terminate round trip (item 1); the AppKit-owned
> `CAMetalLayer` canvas — `WgpuRenderer::new_from_ca_layer` over a plain layer-hosting NSView
> (never MTKView), create/draw/resize/EDR/teardown on-screen (item 2); and the real engine over
> FFI — `AppCore::new_host`, `open_path`, the `Begin*` scan/archive workers running **inside
> pb-mac-ffi** on Rust threads, e2e-tested (item 3). Effects drain **pull-style**
> (`next_effect() -> Option<CoreEffectFfi>` — swift-bridge can't emit compilable Swift for
> `Vec<transparent enum>`). All four swift-bridge gotchas: `crates/pb-mac-ffi/README.md`.
> **The live NS1 task list (items 4–10 remaining) is in `.taskmaster/current-status.md`
> (▶ Resume).** The remaining bullets below are the design reference for those tasks.

- **FFI boundary — `swift-bridge` ✅ (decided + wired).** (Recommended over UniFFI: it handles
  methods and simple enums/opaque handles cleanly; UniFFI is awkward for the live surface + tight
  loop. Also chosen over a hand-rolled C-ABI for the ergonomic marshaling of the enum-heavy effect
  drain — swift-bridge 0.1.59 handles the enum-with-data / `&str` / `Vec<enum>` / opaque-handle
  design as-is.) Rust compiles through the dedicated `pb-mac-ffi` `staticlib`; it exposes the
  opaque `AppCore` handle, FFI-safe command/event calls, and the effect-drain API.
- **Main-thread rule:** Swift/AppKit/wgpu surface calls run on the main thread. Rust
  worker threads may enqueue work internally, but effects are **drained by Swift on
  the main actor** (`drain_effects()`), not delivered through arbitrary callbacks.
  A callback may only wake `DispatchQueue.main`; it must not mutate SwiftUI/AppKit
  state or call renderer APIs directly.
- **Host + surface:** an AppKit/SwiftUI app owns `NSWindow` + `MTKView`; pass the
  retained `CAMetalLayer` pointer (`*mut c_void`) to the macOS-only renderer surface
  constructor. Document the safety contract: the layer outlives the Rust surface,
  creation/configuration/render happen on the main thread, size changes are reported
  before drawing, and Swift releases/destroys Rust before the view/layer dies.
- **Frame pump:** drive `CoreEvent::Tick`/`CoreEvent::Redraw` from
  `MTKViewDelegate.draw(in:)` plus scheduled wake deadlines returned by `WakeAt`.
  Use `preferredFramesPerSecond` at the display refresh; use on-demand drawing when
  idle and continuous drawing only while nav/animation/decode work is outstanding.
  Photon-timestamp seam = `CAMetalDrawable.presentedTime` (per architecture §10).
- **Input mapping:** `NSEvent` keyDown/keyUp → `PbKey` + modifiers; `AppCore` resolves
  the loaded `Keymap` to `Action` exactly like winit. Ignore OS key repeat for held
  actions, clear held state on focus loss, and preserve the existing press/release
  semantics. Magnify gesture → zoom; scroll → pan/zoom per settings; file-drop +
  `application:openURLs:` → `LaunchInput` (ADR-019).
- **Menu/file panels:** either keep `muda` and drain `MenuEvent` from Swift each tick,
  or build a thin Swift/AppKit menu from the same `Action` ids. In both cases,
  `MenuState` effects must keep Save Rotation, Undo, Stop Scanning, scale mode,
  recursive, fullscreen, slideshow, and info check/enabled states synchronized.
- **Exit criteria:** the SwiftUI-shelled Mac build shows photos and flies through a
  folder at refresh rate with keyboard + trackpad, native menu, native open — and
  **no egui in the canvas path**. Windows untouched. (Settings/About unavailable on
  this build — expected; the egui beta remains the shippable Mac artifact.)

## NS2 — Port the dialogs to SwiftUI (easiest first)

First sub-step is the **effect bridge**: turn `AppCore`'s `ShowDialog`/`Progress`/
`Error` effects into a SwiftUI-observable model (`@Observable`), so a dialog is just
a view bound to state + a command on dismiss. Then port in this order (each
independently shippable):

1. **About** → standard `NSApplication` about panel (`orderFrontStandardAboutPanel`)
   fed a **`credits` attributed string** (tagline + GitHub link); icon/name/version/
   copyright come from `Info.plist`. Maps the current egui About 1:1, native, ~no
   custom UI (ADR-021). A bespoke SwiftUI About is a deferred NS3 nicety.
2. **Confirm (delete) + Message (error)** → `NSAlert` / SwiftUI `.alert`. Native
   delete-confirm is a visceral Mac-assed win.
3. **Loading + Scanning** → SwiftUI progress views reading the existing
   `OpenProgress`/`ScanProgress` handles over FFI (determinate bar / indeterminate
   spinner + count). Minimal — the data source already exists.
4. **Settings** → a SwiftUI `Settings` scene bound to a public `SettingsForm` mirror
   of `settings::Settings` (not egui's private `SettingsDraft`); `⌘,` opens it; Save
   validates/clamps and hands a `Settings` back through `SettingsSubmitted`.
5. **Password** → a real `NSSecureTextField` (gets native secure entry for free);
   submit → `PasswordSubmitted`.
6. **Settings → Shortcuts capture — LAST, the long pole.** The keybinding editor
   reads raw winit `KeyCode` (`dialog.rs::handle_capture_event`); it needs a genuine
   AppKit key-event-capture rewrite (local event monitor → `PbKey`/`KeyChord`), not
   a form. After NS0, the `Keymap`/`Action`/`KeyChord` *model* is portable; only the
   capture mechanism is shell-specific.
- **Exit criteria:** every dialog is native on the SwiftUI build; the shortcut
  editor captures chords via AppKit; the Settings round-trip (open → edit → Save →
  apply → persist) matches the egui build's behavior.

## NS3 — "Mac-assed" polish + cutover

Lift to the north-star bar, then flip the default.

- Window state restoration; standard **Window** menu (Minimize `⌘M`, Zoom), Help
  menu, Services; SF Pro + accent + vibrancy passes; instant `effectiveAppearance`
  tracking; accessibility on the chrome; trackpad gestures as `NSGestureRecognizer`s;
  EDR/P3 validated through the SwiftUI host (it inherits `macos-port-plan` M4's
  `CAMetalLayer` work — confirm the layer poke still lands when AppKit owns the
  view).
- **Cutover:** make the SwiftUI shell the **default Mac target**; retire the egui
  `DialogWindow` *on macOS only* (`#[cfg(not(target_os = "macos"))]` keeps it
  compiling on Windows/Linux). egui lives on, themed, as the Windows chrome forever.
- **Exit criteria:** a fresh-Mac user can't tell it isn't a hand-written Cocoa app;
  notarized DMG of the SwiftUI build passes Gatekeeper clean; Windows CI still green
  and visually unchanged.

---

## What NOT to do

**Don't keep winit alive underneath AppKit** (hosting AppKit views inside a winit
`NSView`). The coupling is too deep — `ActiveEventLoop` in `DialogWindow::open`, raw
`KeyCode` in capture — so the half-alive hybrid is two run loops knife-fighting over
the main thread. Clean inversion: on the Mac target, winit is simply **absent**.

**Don't gate the egui-on-Mac beta on any of this.** NS0 ships invisibly; NS1–NS2
live behind a flag; the egui beta is the Mac artifact until NS3. There is a
shippable, sellable build at every commit.

---

## FFI & build system

- **Bridge:** `swift-bridge` (Rust `staticlib` ⇄ generated Swift glue). Effects are a
  Rust-owned queue drained by Swift on the main actor; worker-thread callbacks, if
  used, only schedule a main-thread drain.
- **Build:** **Xcode owns the Mac target** — a build phase runs `cargo build
  -p pb-mac-ffi --target aarch64-apple-darwin --release`, links the produced
  static library, embeds `Info.plist` + `.icns` (the squircle variant from
  `macos-port-plan` M2/M3), then codesign → notarize → staple → DMG (JD has this
  muscle; `macos-port-plan` step zero). WGSL-at-runtime ⇒ no Metal compile step.
  Windows/Linux keep `cargo build -p pb-app` producing the winit binary unchanged.
- **Target (ADR-021):** **arm64-only**, **min macOS 14 (Sonoma)**. No universal2 —
  Intel is sunsetting and can't be validated without Intel hardware; the macOS-14
  floor is gated by `@Observable` and excludes no Apple Silicon hardware. `x86_64` is
  a same-afternoon add if a real Intel user ever appears.
- **CI:** add a Mac-SwiftUI lane to the matrix *without* removing the egui-Mac lane
  until NS3 cutover.

## Risk register

| Risk | Mitigation |
|---|---|
| `AppCore` extraction is open-heart surgery on a 256 KB file | NS0 is pure refactor with a green-suite gate, parity tests, and **zero behavior change**; land alone before any Swift. |
| Two-run-loop contention | Clean inversion (winit absent on Mac); never the winit-under-AppKit hack. |
| `CAMetalLayer` handle + wgpu lifetime rules | NS1 creates a macOS-only unsafe CoreAnimationLayer constructor and proves creation, resize, EDR reconfigure, draw, and teardown on a canvas-only build before any dialogs depend on it. |
| FFI effects delivered on the wrong thread | Use a drained main-thread queue; callbacks may only wake the main actor, never mutate AppKit/SwiftUI or render directly. |
| Shortcut-capture rewrite (winit `KeyCode` → AppKit) | NS0 first replaces winit `KeyCode` in `KeyChord` with app-owned `PbKey`; AppKit capture is sequenced **last** (NS2.6), isolated to shell input. |
| Yak-shave eats the ship | Strangler-fig + build flag + egui beta stays default until NS3; stop after any phase with a working app. |
| muda menu under AppKit-owned `NSApp` | Verify `init_for_nsapp` and `MenuEvent` draining compose in NS1; fallback is a thin Swift/AppKit menu generated from the same `Action` ids and `MenuState`. |
| Mixed Rust+Swift notarization | Standard for stapled `.app` bundles with embedded static libs; no dylibs to sign (static link, as on Windows). |

## ADR-021 — ratified (2026-06-30)

**Accepted** and recorded in [`decisions.md`](decisions.md) (ADR-021): a native
AppKit/SwiftUI shell on the macOS target over an extracted platform-neutral
`AppCore`; egui retained on Windows. Resolved parameters: **arm64-only**, **min
macOS 14 (Sonoma)**, **standard About panel + credits string**. `decisions.md` is
the canonical record of the rationale; this plan is the execution detail.

## Owner decisions (resolved 2026-06-30)

1. **Min macOS floor → macOS 14 (Sonoma).** Gated by `@Observable` (Observation
   framework) for the Rust↔SwiftUI state bridge; excludes no Apple Silicon hardware.
   The egui-on-Mac beta keeps its 11.0 floor; the floor rises to 14 at the NS3 cutover.
2. **About → standard `NSApplication` panel + `credits` attributed string** (tagline
   + GitHub link). Maps the current egui About 1:1. Bespoke SwiftUI About deferred to NS3.
3. **arm64-only** (no universal2). Intel is sunsetting and unvalidatable without Intel
   hardware; `x86_64` is an on-demand add if a real Intel user ever asks.
4. **tasks.json — deferred until after the Codex review of this plan.** Generating
   task entries now would immediately go stale against Codex's edits; generate NS0–NS3
   tasks once the plan settles.

## Verified-on-2026-06-30 grounding (so a fresh session trusts this)

- `pb-render/src/gpu.rs`: `WgpuRenderer::new(target: impl Into<wgpu::SurfaceTarget<'static>>)`
  — host-agnostic for safe window handles; `Backends::PRIMARY` (Metal on macOS);
  WGSL shaders compiled at runtime; `hdr_surface_wants_edr()` + the current
  winit-window `CAMetalLayer` EDR path present. A native `MTKView.layer` needs a
  macOS-only unsafe `SurfaceTargetUnsafe::CoreAnimationLayer` constructor in NS1.
- `pb-render/src/lib.rs`: `Renderer` trait (`set_image`/`present_slot`/`render`/…) —
  the swappable render seam (ADR-002/007).
- `pb-app/src/dialog.rs`: one `DialogWindow` over a second winit window + egui;
  `DialogKind` = {About, Settings, Confirm, Message, Password, Loading, Scanning};
  `OpenProgress`/`ScanProgress` `Arc` handles already decouple worker→UI progress;
  shortcut capture now maps the winit `KeyCode` through `pb_key_winit::from_winit`
  into a `PbKey`/`KeyChord` (winit-free model; NS0), so only the *capture mechanism*
  is shell-specific for NS2.6.
- `pb-app/src/main.rs`: still the large winit `ApplicationHandler` god-object and the
  remaining `AppCore`-struct extraction target — but the shell-neutral **model** and
  the tested seams (`PbKey`, keymap, `MenuState`/`apply_menu_state`,
  `resolve_key_down`/`Modifiers`, `timing`) are already lifted out into `pb-app-core`
  (NS0 landed; see above).
- Manifests: the **crate split is done** — `pb-app-core` (lib, `toml`-only) holds the
  shell-neutral model; `pb-app` (bin) is the winit shell over it. NS1 still adds a
  macOS FFI/`staticlib` crate that links `pb-app-core` for the Swift bridge (don't make
  Xcode depend on the `pb-app` bin). Existing ingredients: `winit 0.30`, `wgpu 22`,
  `egui 0.29`, `muda 0.19` (NSMenu), `rfd 0.14` (native panels), `objc2 0.6` (in tree).
- ADR-019 (`pb-core::open` → `LaunchInput`): launch/open is already delivery-layer
  only; the Mac shell's open path is a shim, not new architecture.
