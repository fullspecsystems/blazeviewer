# PhotoBlaze — Current Status (session handoff)

_**CUTOVER OFFICIAL 2026-07-02 night: the SwiftUI host IS PhotoBlaze on macOS** — bundle id
flipped to com.jdlien.PhotoBlaze (associations/TCC carry over from the egui beta, which is now
retired as the Mac artifact; release.yml's macos-dmg lane still builds the egui bundle — flip it
to `release-macos.sh --swift-host` when CI credits return, untestable until then). HDR verified
by the owner after the potential-vs-current headroom fix; menu-state mirroring restored (Save
Rotation et al). Earlier the same night: the
app is named PhotoBlaze everywhere users see it (bundle id still com.jdlien.PhotoBlazeMac
until the id flip), local sign+notarize works (dist/PhotoBlaze-<v>-swiftui.dmg), and the
`swiftui` branch == main. The photoblaze-wt1 WORKTREE IS RETIRING — work on main in
~/code/photoblaze from now on (remove with `git worktree remove ../photoblaze-wt1` once no
session is using it). CI didn't gate this merge (Actions credits exhausted) — local gates
ran instead (fmt, clippy -D warnings, full workspace tests); re-green via workflow_dispatch
when credits reset._

_Last updated: 2026-07-02 late (**NS2 FEATURE-COMPLETE incl. the Shortcuts editor, plus a
post-NS2 polish evening** — superellipse HUD corners, true borderless F mode, menu badges,
Finder-parity delete, DPI-race fix; most of it owner-smoked live. **NEXT: the three ▶ FINALIZE
items** in the Resume section — EDR-per-window + XDR check, startup/geometry settings,
local packaging. CI Actions credits are exhausted — build releases locally.)_

## 🪟 Windows session 2026-07-01→02 — CI green again + Live Photos + HEIC ships parallel

**CI fix:** the Windows job had been red since 7/1 — two clippy `-D warnings` errors in
`pb-app-core` (an unused `keymap` import; a needless return in a `cfg(not(macos))` arm),
both invisible from macOS. Fixed + a 4-agent review of the whole NS0 refactor landed real
fixes: **reveal-in-Explorer quoting** (paths with spaces opened Documents — `raw_arg` +
path-only quotes), a `pending_dialog` overwrite race, dir-scans now keep the loop polling
(`work_pending`), `resumed` drains launch effects, `tick()` flushes `pending_delete` (NS1
contract), **nine `Instant::now()` calls in the core re-stamped from the injected
`self.now`** (the NS0 0.3 determinism contract), archive-open supersession (`take()` the
old load) + the stranded Loading dialog on worker death. Preexisting scan-vs-archive
playlist clobber documented but NOT fixed (needs a design pass: `open_plan` should cancel
cross-type in-flight opens).

**Live Photos on Windows (task #39, DONE, owner-verified):** `pb-decode/src/mf_video.rs`
(Media Foundation IMFSourceReader → RGB32, auto-rotation, true per-sample timing) +
`live_audio.rs` Windows impl (WinRT MediaPlayer). Gates widened to `any(macos, windows)`.

**HEIC parallel decode ships (task #16 was stale — phases were done 6/28):** re-verified
post-NS0 (heic_bench **45.4/s, 4.88×** vs WIC's 9.4/s), `autocorrect_broken_input` set for
the Sony color quirk (#24 → review; no Sony sample on disk to confirm), and **release.yml
now builds the MSI with `--features libheif`** (+ ci.yml gates the feature) via
`setup-libheif.ps1` with the vcpkg `installed` tree cached (`VCPKG_ROOT=C:\vcpkg-pb` on
runners — first run pays ~15 min, then cached). **"Set default…" (task #14.1)** now
deep-links PhotoBlaze's own Default-apps page (HKCU self-registration fallback +
MSI `AppRegistration` component).

---

_Below: the NS1 handoff (macOS), unchanged._

_**NS1 (the macOS SwiftUI/AppKit host) has BEGUN — its FFI
foundation is laid and on `origin/main` (`59fdd77`).** The FFI boundary is
**`swift-bridge`** (owner-confirmed, the plan's pick); a new macOS-only staticlib crate
**`crates/pb-mac-ffi`** bridges the platform-neutral `AppCore` to a Swift host — opaque
`AppCoreHandle`, events in (`key_down`/`key_up`/`focus_lost`/`tick`), effects out
(`drain_effects → Vec<CoreEffectFfi>`). The **KeyDown→effect round-trip is proven** by a Rust
test, and `AppCore::headless(Viewport)` is the public construction seam it builds on (the
`handle` tests reuse it). swift-bridge deps + the `ffi` module are **macOS-target-gated**, so
Windows/Linux `--workspace` CI builds an empty staticlib (winit `pb-app` untouched). The rest of
NS1 is scoped as a 10-item list — see the **▶ Resume** section. Also shipped this session (both on
`main`): **Show in Finder / File Explorer** (task #40) and the **right-click photo context menu**
(task #41). Green throughout: workspace tests, clippy `-D warnings`, fmt._

_**NS0 — the AppCore ownership inversion (ADR-021) is COMPLETE** and on `main`. The winit `App`
god-object is fully inverted into a platform-neutral **`AppCore`** (crate `pb-app-core`) + a thin
winit shell: every input event (keyboard/focus/pointer/pinch/double-tap/**resize**/**scroll**), the
tick loop, all menu + dialog actions, the dialog outcomes, and the whole open/scan/archive flow run
through **`AppCore::handle(CoreEvent)`** + a **`CoreEffect`** drain. The shell is a translator
(`WindowEvent`→`CoreEvent`) + effect-executor; what stays shell is genuine platform machinery — the
winit window, the GPU surface, `thread::spawn`+`mpsc` workers, and the egui dialogs. The NS0 section
below is the detailed step-by-step record; everything under it is previously-shipped work (macOS
port, archive, settings, HEIC, color), unchanged._

_The macOS (Apple Silicon) port is complete and SHIPPED in `v0.1.0-beta.4` (2026-06-30) — the
first signed + notarized macOS DMG alongside the signed Windows MSI. Verified notarized +
stapled + Gatekeeper-accepted (`source=Notarized Developer ID`)._

---

## 🧭 NS0 — AppCore ownership inversion (SwiftUI groundwork, ADR-021) — ✅ COMPLETE (2026-07-01)

**Branch `swiftui`, fast-forwarded + pushed to `origin/main` (`4089157`). Green throughout: 393
workspace tests, clippy `-D warnings`, fmt; owner-smoke-verified across the inversion. `main.rs` is
down from 7,618 → ~4,300 lines; `impl AppCore` carries the full engine + `handle`/`tick`/
`dispatch_action` + every command arm, and the resolve/scan compute (`scan`), EXIF-write
(`save_rotation`), trash (`delete`), and archive-RAM (`archive`) modules all moved into
`pb-app-core`. Rollback (if ever needed): `git reset --hard 44d9464` (pre-Phase-B). The step-by-step
detail below is a historical record; the actionable next step is the **▶ Resume** section (NS1).**

**Goal:** invert the winit `App` god-object into a platform-neutral **`AppCore`** (in
`pb-app-core`) + a thin shell, so a macOS SwiftUI/AppKit host can drive the *same* core via the
`CoreEvent`/`CoreEffect` contract. This is the prerequisite for **NS1** (the Swift bridge).
Two parts: **STATE** inversion (fields → AppCore) then **BEHAVIOR** inversion (methods →
`impl AppCore` + a `handle(CoreEvent)` entry point).

### ✅ DONE — the full STATE inversion
- **5.1–5.4** — held-key/input/timing, view/geometry, metadata caches, the decode/prefetch/
  residency engine, metrics, nav/playlist, HUD state + compositor, and the renderer — **all in
  `AppCore`**. Construction switched from a growing `new(…)` to a named struct literal.
- **New `pb-hud` crate** — the CPU HUD compositor (`hud.rs` + `icon.rs`, `fontdue`+`resvg`)
  extracted from the winit shell so `pb-app-core` can own the render path without dragging a
  UI-toolkit dep in. The shell re-exports it (`pub use pb_hud::{hud, icon}`) → zero call-site
  churn. Pure-Rust → iOS-safe; the seam for a future native-overlay backend.
- **5.5 Phase 0 (contract cleanup, codex-reviewed r1):** `effects: Vec<CoreEffect>` → AppCore
  (before any method move); **`now: Instant` injected clock** (shell stamps it at each event
  entry; the core never calls `Instant::now()` → `handle` is deterministically testable);
  **`Viewport{width,height,scale_factor}`** (killed the scattered `window.inner_size()` /
  `scale_factor` reads in core methods); **`Settings` migrated** into pb-app-core (added the
  `serde` dep). `SetWake(Option<Instant>)` + live-audio effects intentionally deferred to the
  phases that first construct them (dead-code rule).
- **5.5 Phase A (remaining state):** prefetch-upgrade trio, `live_motion_cache`, undo (migrated
  `UndoAction`), animation (git-mv `animation.rs` + relocated `AnimWant`/`AnimDecode`/`Prepared`).

**Net:** `AppCore` owns **all** orchestration state, and now (Phase B) also the deferred-delete
state (`pending_delete`/`pending_confirm_delete`) + the `scanning`/`launching` flow mirrors. Still
shell-owned = only platform handles + live async flow: `window`, `windowed`, `dialog`
(+`pending_dialog`), the muda native-menu handles (`menu`/`window_menu`/`native_fullscreen_item`/
`proxy_icon_path`/`save_rotation_item`/`cancel_scan_item`/`undo_item`/`menu_attached`),
`menu_state`, `last_edr_headroom`, the `live_audio` ObjC `AVAudioPlayer` handle (driven via
effects now), plus the **scan/archive/launch/drop FLOW** handles (`dir_scan`/`scan_gen`/
`archive_load`/`archive_gen`/`pending_launch`/`password_archive`/`pending_drops`) → inverted in 5.6.

### ✅ DONE — Phase B (the pure-core BEHAVIOR inversion), 2026-07-01
The ~99 pure-core orchestration methods now live on **`impl AppCore`** (new
`crates/pb-app-core/src/app_core_impl.rs`) instead of `impl App`. Done as **8 green commits**,
each `cargo test --workspace` + `clippy -D warnings` + `fmt` clean, behavior-preserving
(`self.core.X` → `self.X`; the shell calls them as `self.core.method()`):
- **Engine helpers/consts → `pb_app_core::engine`** (new module): the decode entry points
  (`decode_item`/`decode_motion_job`), `meta_for`/`rel_to_root`, `render_color`, `is_hdr`,
  `nav_of`, `ring_capacity`/`window_for_capacity`, the EXIF/`scale_alpha`/`point_in_rect`/
  `title_for`/`file_name_of` helpers, the picker-dir helpers, the **clipboard pixel transforms**
  (`to_clipboard_rgba8`/`rotate_rgba8`/`srgb_oetf`/`reinhard`, `half` dep added), `companion_motion`,
  `cursor_after_removal`, `scale_mode_of`, `frame_step_dir`, and all the tuning consts (ring/zoom/
  pan/pie/frame-step). **`macos_menu_chord` → `pb_app_core::keymap`** (pure Action→KeyChord table).
- **Methods moved (by cluster):** nav/prefetch/residency (`present_item`, `drain_results`,
  `try_present_target`, `sharpen_now`, `prefetch_fulls`, `load_current_sync`, **`request_prefetch`**,
  **`advance`**, **`nav_press`**, …), view zoom/pan/rotate/fit (`view_for`, `zoom_held`, `pan_held`,
  `pan_by_pixels`, `apply_view_holds`, `zoom_step`, `zoom_about_cursor`, `set_scale_mode`, …), HUD
  build + overlay/help (`show_overlay`, `help_sections`, `help_shortcut`, `toggle_info`,
  `toggle_help`, push/tick toast/pie/chip, play-hint, `exif_rows`, `open_panel_bitmap`, the
  open-hint chain), the **animation-playback cluster** (`toggle_play_pause`, `frame_step`,
  `frame_step_press`, `poll_anim_decode`, `start_live_audio`, `stop_playback`,
  `present_anim_frame`, `install_animation`, `tick_playback`, `tick_frame_step`,
  `maybe_show_anim_hint`), the delete-advance pure cluster (`flush_pending_delete`,
  `enter_empty_state`, `rebuild_playlist`), `refresh_cursor`, `copy_image`/`copy_path`,
  `menu_state_from`, `apply_settings`, `apply_keymap`, `refresh_hz`, `windowed_restore`, …
- **Couplings resolved to enable the moves:**
  - **`scanning` + `launching` bools** on `AppCore` — core-owned mirrors of the shell's
    `dir_scan.is_some()` / `pending_launch.is_some()`, kept in sync at every mutation. Let
    `request_prefetch` + the open-hint chain move without the shell flow fields.
  - **Live-audio effects (B4):** `CoreEffect::{StartLiveAudio{path,at_secs}, StopLiveAudio,
    PauseLiveAudio, ResumeLiveAudio}`. The shell owns the ObjC `AVAudioPlayer` and executes them
    in `drain_effects` (no-op on non-macOS via the stub); the core pushes them. Same-turn drain =
    behavior-preserving.
  - **Delete state → core:** `pending_delete` + `pending_confirm_delete` moved onto `AppCore`.
    `do_delete`'s trash I/O + `delete_current`'s confirm dialog stay shell (→ 5.6).

### ✅ DONE — `dispatch_action` → core + `handle(CoreEvent)` (Phase C1), 2026-07-01
- **`dispatch_action` moved onto `impl AppCore`** via a `CoreEffect::ShellFlowAction(Action)`
  seam. Its 22 pure arms (nav/zoom/scale/rotate/copy/info/slideshow/play/frame) run in the core;
  its 11 flow arms (save-rotation, delete×2, undo, fullscreen, recursive, cancel-scan, mute,
  settings, about, quit) push `ShellFlowAction(action)`, which the shell's `perform_flow_action`
  runs from `drain_effects`. So the **whole** action vocabulary dispatches through one core entry
  point. `drain_effects` now **loops until quiescent** (bounded) so a flow action that enqueues a
  follow-up effect lands the same event turn — `Fullscreen`→`SetWindowMode`, `Quit`→`Quit` need
  this; dialog opens already ran at drain's end. `ShellFlowAction` is **provisional** — 5.6 gives
  each flow a specific effect/`CoreEvent` + native macOS handling.
- **`AppCore::handle(CoreEvent)` exists + is unit-tested (5 tests).** C1 covers the input + menu
  events: KeyDown (→ `resolve_key_down` → dispatch/nav_press/held/frame-step), KeyUp + FocusLost
  (the held-key release net), MenuAction (→ `dispatch_action`), KeymapSubmitted. A minimal test
  `AppCore` (empty source, dummy pool, no window) drives it. Escape isn't special-cased here (it
  resolves through the keymap to `Quit` → `ShellFlowAction`; the host owns dialog-dismiss-vs-quit).

**✅ NS1 (the Swift bridge) is UNBLOCKED** — and, after C2 below, the winit shell drives the *entire*
engine through `handle()` (not just keyboard/menu), so the Swift host has a fully worked reference.

### ✅ DONE — Phase C2 (winit shell → translator). **The whole input surface + tick loop route through `handle()`.**
The winit shell now translates its **input events + the whole tick loop** to `CoreEvent`s and calls
`self.core.handle(ev)` — the SAME entry the Swift host uses — instead of calling core methods inline:
- **✅ Keyboard + focus** (`0b080ce`): `KeyboardInput` press/release → `KeyDown`/`KeyUp`;
  `Focused(false)` → `FocusLost`. Escape keeps its shell pre-filter (dialog-dismiss / esc-guard /
  begin_exit) ahead of `handle`.
- **✅ Pointer gestures** (`345e830`): `CursorMoved` → `PointerMoved`, `PinchGesture` → `Pinch`,
  `DoubleTapGesture` → `DoubleTap`. `CursorLeft`/`MouseInput`/`MouseWheel` stay shell (no clean
  `CoreEvent` fit; already call core methods directly).
- **✅ The Tick loop** (`47b9ffa` prep + `2b98d5b`): `about_to_wait`'s per-tick core loop — held
  zoom/pan, hold-to-fly advance, slideshow, sharpen/prefetch, info-panel/toast/pie, deferred resize
  decode + geometry save, animation playback + Live Photo revert + eager-prep, and the next-wake
  computation — now lives in **`AppCore::tick()`**, reached via `handle(CoreEvent::Tick(now))`. New
  seams: `CoreEffect::SetWake(Option<Instant>)` (stored in `App.requested_wake`, min'd with the
  shell's dialog-repaint deadline for `set_control_flow` — the old 5-way min split core/shell),
  `dialog_open`/`archive_loading` core mirrors, `work_pending`→core, and the revert's
  `live_audio`→`StopLiveAudio` effect. `about_to_wait` is now a translator: shell flow-polling
  (menu/scan/archive/drops) + mirror sync + the two host-side overlays (scan chip, dialog egui
  clock) + `handle(Tick)` + drain + control-flow. **✅ Owner-smoke-verified (2026-07-01)** —
  hold-to-fly / slideshow / animation / resize / dialog responsiveness all felt right.

**✅ C2 COMPLETE — the two former loose ends are wired (⚠ unsmoked):**
- **`Resized`/`ScaleFactorChanged`** (`6ae58da`) → `handle(CoreEvent::Resized{width,height,scale})`
  → `AppCore::resize` (viewport, DPI-change overlay rescale, fit, swapchain reconfigure via the
  core-owned `renderer.resize`, debounced crisp re-decode). The shell computes the fit-change before
  calling `handle` and does its GPU/window bits *around* it — the macOS `hdr_surface` EDR re-assert
  (after the reconfigure, before draw) + the `draw` + `track_windowed_geometry` — all gated on that
  fit-change. `CoreEvent::Resized` dropped its `edr_headroom` field (the host owns EDR).
- **`MouseWheel`/scroll** (`45af83b`) → a new `contract::ScrollDelta{Lines|Pixels}` on
  `CoreEvent::Scroll` → `AppCore::scroll` (pan/zoom per the setting, Ctrl flips; the four tuning
  consts moved to `engine`). The shell just maps `MouseScrollDelta` → `ScrollDelta`.
- Still shell (no clean seam / genuinely platform): `MouseInput`/`CursorLeft` (call core methods
  directly), `DroppedPaths` (the shell classifies inputs → `open_input` → the core `open_plan`).
  **The whole input surface + engine now runs through `handle()`.**
### ✅ DONE — Phase 5.6 (the FLOW inversion). **7 of 11 flow actions inverted; the rest are legitimately host-side.**
The `ShellFlowAction(Action)` catch-all is being decomposed: each flow action that has genuinely-
core logic moves into its own `dispatch_action` arm / specific effect; only true platform ops stay.
- **✅ Inverted this session (2026-07-01, owner-smoke-verified "never seen a regression"):**
  - **MuteLiveAudio** (`bbfc003`) → core arm — the mute *state* + toast are core; only the ObjC
    player is shell (`Stop/StartLiveAudio` effects); the native menu re-asserts via per-tick
    `apply_menu_state`.
  - **About / Settings** (`b96aaed`) → the existing `CoreEffect::ShowDialog(DialogKind)` + a shell
    `shell_dialog_kind` map (drops `open_about`/`open_settings`).
  - **SaveRotation / Undo** (`d54e444`) → **pure core arms**: `git mv save_rotation.rs → pb-app-core`
    (little_exif + the `orient6.jpg` fixture move with it), since they were ~95% core-state around a
    thin platform-neutral EXIF write.
  - **Delete-to-trash** (`9eb299a`) → core arm + `do_delete` → core: `git mv delete.rs → pb-app-core`
    (trash dep + `DELETE_ADVANCE_DELAY`→engine); `Del` is now pure core, `do_delete` uses `self.now`.
  - **Fullscreen** (`0655292`, ✅ smoke-verified) → core arm + **`windowed: bool` migrated App → AppCore**
    (16 refs): the arm flips `windowed`/`settings.fullscreen` + pushes `SetWindowMode`; the live-
    window geometry snapshot + `settings.save()` moved into the `SetWindowMode` handler
    (`apply_window_mode`), which runs before the window ops so capture-before-resize still holds.
- **Legitimately host-side — stay behind `ShellFlowAction` (reframed, `ab7a9e2`)**, since execution
  *is* a platform op: **DeletePermanent** (opens the confirm dialog; its Yes calls the core
  `do_delete`), **Recursive** / **CancelScan** (spawn / cancel the off-thread scan + its dialog),
  **Quit** (hide-window teardown, also reached from window-close / Esc).
### ✅ DONE — Archive/scan flow inversion (2026-07-01). The last coupled piece — now fully core-driven.
Mapped end-to-end (Explore agent) and inverted in the recommended low-risk order:
- **✅ Step 1 — dialog-outcome reactions → core** (`3006765`, ✅ owner-smoke-verified): `handle_dialog_outcome`'s
  reactions (apply_settings/keymap, `do_delete`, esc-guard, clear pending confirm/password) moved
  into **`AppCore::handle_dialog_resolved`**, reached via a new **`CoreEvent::DialogResolved(DialogResult)`**.
  The housekeeping is now effects: **`CloseDialog`** (wired its previously-dead drain arm) + new
  **`CancelScan`** / **`CancelArchiveLoad`**. `password_archive` (pure `PathBuf`) migrated App →
  AppCore. Shell `route_dialog_outcome` maps `DialogOutcome`→`DialogResult`; only **PasswordSubmitted**
  (spawns the archive worker + pokes the live dialog) stays shell. 4 new core unit tests. Behavior-
  preserving (effects drain right after `dialog_event`, same event turn).
- **✅ Step 2 — the entire resolve/scan COMPUTE relocated to `pb_app_core::scan`** (✅ owner-smoke-verified — large archive + dialogs + progress bar unchanged):
  - **2a** (`3cdb016`): the `Resolved` currency (fields `pub`) + pure builders + `ScanUpdate`.
  - **2b** (`b21f59a`): the walkdir dir-scan resolvers (`is_supported_image`/`rel_display`/
    `collect_images`/`image_walker`/`resolve_source`/`resolve_scan`/`stream_scan`) + `ScanProgress`
    (externally-called methods → `pub`) + a `walkdir` dep. main.rs re-exports `ScanProgress` so
    dialog.rs is untouched; the scan/order tests pass unchanged via the `scan::` prefix.
  - **2c** (`ef40042`): the archive resolvers (`open_archive`/`seven_z_preflight[_within]`/
    `load_seven_z`/`resolve_playlist`) + `git mv archive.rs → pb-app-core` (with its tests + cfg-gated
    `windows`/`libc` for the RAM pre-flight). main.rs re-exports the `archive` module.
  - Net: the whole compute (folder walk, archive open, RAM pre-flight, cursor resolution) is now
    core + reusable by the Swift host; **only the `thread::spawn`+`mpsc` worker plumbing + the egui
    progress dialogs stay shell** (`begin_*`/`poll_*` spawn `scan::stream_scan` / call the resolvers).
- **✅ Step 3 — the worker flow inverted (✅ owner-smoke-verified "all clear")**, in three sub-commits:
  - **3a — dir-scan apply** (`dc0982d`): `poll_dir_scan` → `CoreEvent::ScanBatch(Resolved)` →
    `AppCore::apply_scan_batch` (`filter_deleted`→bootstrap-first/extend-rest) + `ScanDone` →
    `finish_scan` (scanning=false / request_prefetch / restore open-hint on an empty deck).
    `filter_deleted` moved to core; `DirScan::bootstrapped` → a core `scan_bootstrapped` mirror;
    `Resolved` gained `Clone`+manual `Debug` to ride on the event.
  - **3b — archive-open apply** (`e9c58f8`): `finish_archive_open`'s success → `ArchiveResolved(Resolved)`
    → `apply_archive` (rebuild + forget password). The failure cases (empty / password re-prompt /
    cancelled / error) stay host-side (native dialogs + `was_password_attempt`).
  - **3c — open-routing + begin-worker trigger** (`0b8f023`): `open_plan` moved onto `AppCore` — it
    pushes `CoreEffect::BeginArchiveOpen`/`BeginDirScan` (the shell's `begin_*` are now those drain
    handlers; they own the `thread::spawn`+`mpsc`+generation+progress-dialog), and resolves a finite
    explicit list inline. `open_input` + the deferred launch call `self.core.open_plan`.
  - Net: **the whole archive/scan flow is core-driven** — core routes the open → `Begin*` effect →
    host worker → `ScanBatch`/`ScanDone`/`ArchiveResolved` events → core applies. Only the
    thread/mpsc/egui-dialog plumbing stays shell. Behavior-preserving (the `Begin*` runs at drain,
    same event turn as the old inline call). **NS0 5.6 is COMPLETE** — the last coupled
    flow is inverted; even PasswordSubmitted (`8dfc507`, the one dialog outcome left shell in Step 1)
    is now core via `BeginArchiveOpen` + a `SetDialogChecking` effect (⚠ that last commit unsmoked).
- The other `ShellFlowAction` arms (DeletePermanent confirm, Recursive/CancelScan scan-thread spawn,
  Quit teardown) remain genuine platform ops. None of this blocks NS1.

### ▶ Resume (NS1 ✅ complete + owner-smoked; NS2 dialogs code-complete, smoke pending)

**ITEMS 1–3 of 10 DONE (2026-07-02 session, commits `4048c69`/`6e3744f`/`9485884`, pushed to
`origin/swiftui`; origin/main merged in — incl. its Windows-CI clippy fix).** The Swift host is
real: a **SwiftPM executable** (`mac/`, macOS 14+ arm64 — chosen over an .xcodeproj;
`open mac/Package.swift` still gives the full Xcode IDE) built by **`scripts/build-swift-host.sh`**
(cargo staticlib → the `create-package` bin wraps it + the generated glue into the
xcframework-backed local package `crates/pb-mac-ffi/PbMacFfi/` → swift build →
`target/swift-host/<profile>/PhotoBlazeMac.app`, bundle id `com.jdlien.PhotoBlazeMac` — coexists
with the egui beta).

- **Item 1 — host + build wiring ✅ (proven live):** Esc → `key_down("Escape")` → keymap →
  `ShellFlowAction("quit")` → `NSApp.terminate` — the app quit itself through the Rust core.
- **Item 2 — CAMetalLayer wgpu surface ✅ (proven live):** `WgpuRenderer::new_from_ca_layer`
  (macOS-only, `SurfaceTargetUnsafe::CoreAnimationLayer`; the safety contract is on the doc
  comment) over a **plain layer-hosting NSView** (`MetalCanvasNSView` — deliberately NOT MTKView;
  wgpu owns the drawable loop). Create/draw/resize/teardown verified on-screen: the Rust engine
  drew the letterbox + "Open File / Open Folder" hint inside the SwiftUI window. The host owns the
  EDR poke (Swift tags the layer extended-linear-sRGB + `wantsEDR` + headroom after attach/resize —
  `wants_edr()` / `set_edr_headroom()` over FFI).
- **Item 3 — real construction + open flow ✅ (VERIFIED LIVE 2026-07-02 + e2e-tested):**
  `AppCore::new_host(viewport)` (real decode pool + loaded Settings/Keymap + Hud;
  `POOL_BUDGET_BYTES` moved to `engine`); FFI `open_path` classifies dir/archive/file →
  `open_plan`; the **`Begin*`/`Cancel*` effects are intercepted inside pb-mac-ffi's drain** and run
  on Rust worker threads (the winit `begin_*`/`poll_*` mirrored; `tick()` polls results into
  `ScanBatch`/`ScanDone`/`ArchiveResolved`) — Swift never sees them. Failures → the new
  `CoreEffectFfi::ReportError` (incl. PasswordRequired until the NS2 dialogs). `attach_layer` does
  the full `resumed()` dance (ring sizing/reserve, prefetch kickoff, empty-deck hint). An e2e Rust
  test drives open_path → scan worker → playlist bootstrap with no Swift/GPU. **Seen on-screen
  (M2 Max):** the corpus scans, photos (HEIC/JPG/PNG) render in the AppKit-owned CAMetalLayer
  through the real pool + resident ring, Space advances 1/15→4/15 by ring rebind with `SetTitle`
  effects, Esc quits via `ShellFlowAction("quit")`.

**swift-bridge gotchas (now 4, all in `crates/pb-mac-ffi/README.md`):** (1) no `///` in the bridge
module; (2) crate-wide `allow(unnecessary_cast)`; (3) **a `Vec<transparent enum>` return generates
Rust that compiles but Swift that doesn't** (no `Vectorizable` conformance — only `swift build`
catches it) → the drain is pull-style `next_effect() -> Option<CoreEffectFfi>`; (4) **never name a
bridge param a Swift keyword** (`repeat` → `is_repeat`) — the generated call site doesn't escape
it. Also fixed: keys cross the FFI as `PbKey::from_name` spellings (`"Right"`/`"C"`), NOT winit's
(`"ArrowRight"`/`"KeyC"`).

**The windowless-app saga, RESOLVED (2026-07-02, commit `29a1a48`) — two stacked problems:**
1. **AppKit treats a bare path in `argv[1]` as a document-open launch and suppresses the initial
   `WindowGroup` window entirely** (windowless app with a live menu bar; `NSApp.windows` empty;
   File ▸ New Window still works). This is why every `--args <path>` launch failed while no-arg
   launches worked. **Dev launches now use a flag AppKit ignores:**
   `open target/swift-host/debug/PhotoBlazeMac.app --args --pb-open <folder>`.
   (The real Finder-open path — `application:openURLs:` — is NS1 item 4.)
2. The disk genuinely hit ~500 MB free and macOS temporarily stopped creating windows for ANY
   newly launched app (Calculator/TextEdit too) — owner freed ~200 GB and it cleared without a
   reboot. That transient made (1) look environmental for a whole evening.
   TCC note: the app's Downloads access is approved; a non-TCC corpus lives at
   `/private/tmp/pb-corpus`.

**Item 4 — NSEvent input adapter ✅ (2026-07-02, `44cb76f`; R-rotate + I-info-panel verified
live in the canvas — HUD overlays render; pinch/scroll/drag/drop need an owner trackpad
smoke):** `KeyMap.swift` (full Carbon-keyCode→PbKey-name table; numpad digits unmapped —
`from_name` has no spelling), ⌘-chords forward to the core AND pass to AppKit (menu shortcuts
live), focus net incl. `didResignKey`, canvas responder overrides (pointer/mouse/scroll/pinch/
smart-magnify, winit conventions), `.fileURL` drop + `application:open:` (pre-handler URL
buffer) → `open_paths(Vec<String>)` (classify_inputs mirrored). New FFI: pointer_moved /
mouse_left (open-panel buttons + play-hint + chip-cancel + drag-pan) / scroll_lines /
scroll_pixels / pinch / double_tap / open_paths.

**Items 5–9 ✅ (2026-07-02 session, commits `50b961d`…`2a7df31`):**
- **Item 5 (event surface):** everything except `DialogResolved` (NS2's half) — MenuAction via
  `menu_action(id)`, the rest landed with item 4.
- **Item 6 (effect handlers) — the drain is feature-complete except NS2 dialogs.** Panels →
  **NSOpenPanel** (images+archives filter; key monitor passes through while a panel is up);
  SetCursor → NSCursor + **canvas cursor ownership** (`.cursorUpdate` re-assert — fixed the
  owner-reported resize-cursor leak); WriteClipboard → **marker + pull accessors** (a `Vec<u8>`
  in a transparent-enum payload is gotcha-#3 territory) → one pasteboard item with TIFF + fileURL,
  verified live incl. ⇧⌃C path copy; RevealPath → NSWorkspace; live-audio → a Swift
  AVAudioPlayer; SetWindowMode → the chromeless-but-key speed mode (keep `.titled` — a borderless
  NSWindow can't become key — hide titlebar/lights, auto-hide menu bar+Dock), verified live F in/
  out; HideWindow → orderOut; SetTitle → the real window title; ShellFlowAction
  Recursive/CancelScan intercepted Rust-side (toggle_recursive/cancel_scan_command mirrored).
- **Item 7 (frame pump) — the 30 Hz stand-in is gone.** `FramePump` = `NSView.displayLink`
  (macOS 14+) driving tick+drain per refresh while active; `SetWake(f64 secs)` (converted at
  drain time) schedules precise off-link Timers; `work_pending()` over FFI decides
  continuous-vs-idle; every input `kick()`s. **Verified live: slideshow advances on pure SetWake
  timers; idle CPU 0.0%.**
- **Item 8 (menu):** `MenuBar.swift` mirrors the muda macOS build_menu (same Action ids, real ⌘
  key-equivalents, Finder delete idioms ⌘⌫/⌥⌘⌫, native Hide/Window selectors, windowsMenu+
  helpMenu); SetMenuState → `MenuStateChanged` marker + `menu_state() -> MenuStateFfi`
  (transparent struct works); About → the **standard NSApplication panel** (the ADR-021 choice);
  the right-click context menu pops the curated task-#41 set through the same ids. Verified live:
  menu-Rotate transformed the canvas, Fit ✓, Stop Scanning disabled, About opens.
- **Item 9 (CI):** the `mac-swift` lane (macos-15 arm64) runs the pb-mac-ffi + pb-app-core tests
  and the full `build-swift-host.sh --debug` chain — also the only CI compile of the
  macOS-gated Rust. (No separate egui-Mac lane was ever needed; release.yml builds the DMG.)

**Item 10 — NS1 exit criteria ✅ OWNER-SMOKED (2026-07-02):** the owner's release-build pass on
the real machine: *"I can scan through photos very fast, I can drag and drop, everything seems to
work as before"* — fly-at-refresh, drag-drop, and the input surface all confirmed by hand, on top
of the machine-verified set (photos/nav/rotate/info/slideshow/clipboard/panels/menu/About/
fullscreen/idle-pause). Windows untouched (all Mac code target-gated; CI's windows lane green).
**NS1 IS FUNCTIONALLY COMPLETE** — the "extra chrome + debug section" the owner noted is the dev
host's effect log, which retires as the NS2 dialogs land. Optional leftover check: HDR/P3 through
the AppKit-owned layer on the XDR (`~/Downloads/test-images/WideGamut-*-HDR.avif` — trust the
panel, not a screenshot).

**▶ NS2 — the native dialogs: CODE-COMPLETE except the shortcut editor (2026-07-02 session,
⚠ awaiting the owner smoke).** Everything except NS2.6 landed in one pass, green throughout
(workspace tests incl. 4 new pb-mac-ffi dialog-flow tests, clippy `-D warnings`, fmt,
`build-swift-host.sh --debug`, and a live launch+quit smoke on `/private/tmp/pb-corpus`):
- **The effect bridge:** `DialogResolved` crosses as per-gesture FFI entry points
  (`dialog_dismissed/closed/confirm_answered`, `password_submitted/cancelled`,
  `loading/scanning/settings_cancelled`, `submit_settings`) — NOT an enum payload (gotcha #3).
  `CloseDialog` + `SetDialogChecking` cross out; dialog text rides the clipboard-style
  **stash + pull** (`dialog_message()`, `dialog_password_error()`). pb-mac-ffi keeps a
  `shown_dialog` mirror (the winit `dialog.kind()` twin) for kind-guarded closes, and maintains
  `core.dialog_open` (slideshow pauses while a dialog is up).
- **Confirm/Message → NSAlert sheets** (`presentConfirmAlert`/`presentMessageAlert` in
  CoreModel): `ShellFlowAction(DeletePermanent)` is now intercepted Rust-side
  (`confirm_delete_permanent` mirrors winit — flush pending delete, archive-entry toast, arm
  `pending_confirm_delete`, "Permanently delete 'name'?"); `ReportError` presents natively.
- **Loading/Scanning → SwiftUI sheets** (`DialogViews.swift`) over a `dialog_progress()`
  snapshot FFI (the handles already lived in pb-mac-ffi): Loading = determinate bar (spinner
  until the 7z header publishes a total) + Cancel; Scanning = the winit-parity **deferred
  reveal** (250 ms `SCAN_DIALOG_DELAY`, never over a shown photo/another dialog) with live
  count + current folder + Cancel; a superseding open re-points the sheet in place.
- **Password → SwiftUI sheet with SecureField** (native secure entry): lock icon + two-line
  prompt, Enter submits / Esc cancels, wrong password **re-prompts in place** with the inline
  error + "Checking…" state (`SetDialogChecking`) — full winit parity incl. the
  password→loading promote (state-driven sheets make `become_loading` implicit). **Encrypted
  archives now work on the Swift host** (the NS1 `ReportError` stopgap is gone).
- **Settings groundwork (item 5):** `SettingsFormFfi` transparent-struct mirror +
  `settings_form()/submit_settings()` (the fold is a **pure fn** `fold_settings_form`,
  unit-tested WITHOUT touching the real settings.toml — `apply_settings` persists!), and a
  SwiftUI `Settings` scene (`SettingsView.swift`) with the full egui form surface (speed/ramp/
  max-fps-with-refresh-ceiling/hold-delay sliders, scroll/scale/startup pickers, letterbox
  ColorPicker, opacity, slideshow interval, recursive, pinned picker folder + Choose…).
  ⌘, / App menu ▸ Settings route through `ShowDialog("settings")` → the `openSettings`
  environment action. Save = fold+clamp Rust-side → `SettingsSaved` applies + persists;
  Cancel/window-✕ discards (and clears `dialog_open` so the slideshow resumes).
- **The NS1 debug chrome is retired:** `EffectLogView` is gone — the canvas fills the window;
  the FFI trace still prints on a terminal launch (DEBUG builds).
- **Key-monitor gating:** while a panel/alert/sheet is up, the local monitor passes events
  through untouched (typing in the password field must not drive the viewer).

**Owner smoke checklist (all at once):** (1) ⇧Del on a photo → native Delete/Cancel sheet, both
answers; on an archive entry → "Can't delete this" toast. (2) A corrupt/refused archive →
native error alert. (3) An encrypted .zip AND .7z → password sheet; wrong entry → inline-error
re-prompt; right entry → opens (a 7z shows Checking… → Opening…). (4) A big .7z → determinate
progress + Cancel. (5) A huge folder tree (~/Library-scale) → Scanning sheet after ~250 ms with
live count/folder; its Cancel keeps the prior view, while menu ▸ Stop Scanning mid-stream keeps
the partial deck. (6) ⌘, → Settings: edit + Save → behavior changes + persists across relaunch;
Cancel/✕ discards; the slideshow pauses while any dialog is up.

**✅ NS2.6 — the Shortcuts editor SHIPPED (2026-07-02, same session): NS2 IS FEATURE-COMPLETE.**
Built per `ns2-shortcut-capture-notes.md`: `EDITOR_GROUPS` hoisted to `pb_app_core::keymap`
(egui's KB_GROUPS re-points — the two shells render the same editor), an FFI **draft keymap**
(begin/capture/clear/reset/steal-note/dirty; chords display via `KeyChord::mac_symbol` glyphs),
and a Settings **General/Shortcuts TabView** with a global Save/Cancel (commits both drafts —
`submit_settings` folds `SettingsSaved{keymap}` only when dirty). Capture = an app-local
NSEvent monitor that swallows the chord (⌘Q can't quit mid-capture; **bare keys capture** —
the KeyboardShortcuts-package disqualifier), maps through the same Carbon→PbKey table as the
viewer, Esc cancels (the recorded punt). **Latent input bug fixed en route:** the key monitor
now forwards only while the *viewer* window is key — typing Space in Settings used to advance
the photo behind it (also a plausible double-Esc mechanism; its debug tracing remains in).

**Also this session (owner-verified live):** title-bar proxy icon (SetTitle-cadence
`representedURL`), **true borderless F mode** (square corners/every pixel — dynamic
`canBecomeKeyWindow` via `class_replaceMethod` on the window CLASS; the `object_setClass`
isa-swap version segfaulted in SwiftUI's KVO teardown, a recorded gotcha), titlebar
clobber-proofing (`assertWindowChrome` per drain + `.toolbarBackground(.hidden)`),
clamp-oversized-window-on-monitor-change (shrink-only, drag-settle-aware; the OS's Tahoe
window management resizes windows cross-display on its own — resize forensics logging is in
the debug build), and CI cost policy (main+PR+dispatch only — a full run bills ~260 min).

**The post-NS2 polish evening (2026-07-02, all committed to `swiftui`, owner-in-the-loop):**
- **Settings window resizable** (`windowResizability(.contentMinSize)`, min 520×480).
- **Finder-parity delete confirm**: NSAlert `.critical` (caution triangle + app-icon badge) +
  Finder's exact wording, composed Rust-side as `headline\ninformative` (the host splits).
- **Launch race fixed — blurry/unclickable Open buttons**: the canvas attached with a stale
  backing scale (the owner runs THREE DPI contexts: 1x ultrawide main, 2x Studio, 2x Screen
  Sharing) and a bare scale flip never re-runs `layout()` → new
  `viewDidChangeBackingProperties` override routes it through the resize path. On any
  blurry/missed-click report, check the `PB:` trace's "canvas attached (W×H @Sx)" line first.
- **HUD corners are superellipses on macOS** (`pb-hud/hud.rs`): `corner_distance` = n-norm
  gauge, **n = 2.2** (the first pick n=5 was measurably wrong — an n=5 corner silhouettes
  like a much smaller circle: "still too sharp"), and buttons derive radius from HEIGHT
  (`BUTTON_RADIUS_OF_HEIGHT = 0.33`, owner-tuned; `BUTTON_RADIUS` is the non-mac token —
  the file docs now say which is which after the owner edited the wrong one).
- **True borderless F mode**: `class_replaceMethod` makes the window's CLASS answer
  `canBecomeKeyWindow` (⚠ NEVER `object_setClass` a SwiftUI window — KVO isa corruption →
  segfault in the next setStyleMask, crash-report-verified), then drop `.titled` → square
  corners, every pixel; `assertWindowChrome()` re-asserts per drain (SwiftUI clobbers
  chrome) + `.toolbarBackground(.hidden)` + `.ignoresSafeArea()`; shadow off in F.
- **Cross-monitor window clamp** (shrink-only, drag-settle-aware via pressedMouseButtons
  polling — window-drag events never reach the app). Note: **Tahoe itself resizes windows
  across displays** (community-confirmed); DEBUG builds log every programmatic resize
  (`inLiveResize == false` filter) to tell our clamp from the OS.
- **Menu parity**: `NSMenuItemBadge` shortcut hints on every bare-key item, sourced from the
  LIVE keymap (`keymap_slot_display`), refreshed after Settings save — the pill look is the
  badge control's own style (owner: "better than the alternatives we've tried"); native
  Enter Full Screen (⌃⌘F, self-titling); `allowsAutomaticWindowTabbing = false` (kills Show
  Tab Bar — which accidentally sparked a compare-photos idea → **task #43**); **Copy Image
  Details now in Edit** (was context-menu-only; an FFI test proves the full chain).
- **The stale-pair trap** (bit twice tonight): `build-swift-host.sh` defaults to RELEASE;
  `--debug` builds debug only — `open` the profile you built. When handing the owner a
  change, build BOTH.
- **CI cost policy**: ci.yml now main+PR+workflow_dispatch only, paths-ignore on docs,
  cancel-in-progress, job timeouts (a full run bills ~260 min; 13 branch pushes burned ~2k
  minutes in a day). **The owner's Actions credits are EXHAUSTED → releases must build
  LOCALLY for now** (finalize item 3).
- **Double-Esc**: never root-caused; Esc gate tracing lives in DEBUG builds; quiet since the
  key-monitor isKeyWindow gate landed.

**Owner-smoked so far:** photos/nav/fly/drag-drop (NS1 item 10), F mode incl. borderless,
proxy icon, Settings incl. the Shortcuts editor + resizable window, delete confirm,
menus/badges, **password sheet + loading bar ("look okay"; the blue `lock.fill` SF Symbol
reads slightly un-native — polish candidate, no decision yet)**. Not yet smoked: the Scanning
sheet on a huge tree, error alerts, rebind-updates-menu-badges.

**▶ FINALIZE — ALL THREE LANDED (2026-07-02 late-late; owner halves remain):**
1. ✅ EDR per-window + live re-poke (didChangeScreen + didChangeScreenParameters). OWNER:
   the XDR visual check with WideGamut-*-HDR.avif.
2. ✅ Startup mode + geometry honored/persisted (winit-convention coords; re-asserts once
   after SwiftUI's launch layout — smoke-verified restoring 900×482 over SwiftUI's 514).
3. ✅ Packaging: icon (Assets.car+icns) + doc types in the bundle;
   `release-macos.sh --swift-host` = the local codesign→DMG→notarize→staple path. OWNER:
   run it at the keyboard (keychain prompt + APPLE_* env), then check "Open With" +
   Dock icon on a fresh copy.

Original item detail (for reference):
1. **EDR/HDR per-window correctness + XDR validation.** `configureEDR` reads
   `NSScreen.main` (TODO comment in CoreModel.swift) — the exact multi-display bug the winit
   port hit ("HDR looked totally broken"); must read the WINDOW's screen, re-poke on
   `didChangeScreen` (an observer already exists for the clamp) +
   `NSApplication.didChangeScreenParametersNotification`. Then the deferred owner check:
   `~/Downloads/test-images/WideGamut-*-HDR.avif` on the XDR (trust the panel, not
   screenshots — they clip EDR).
2. **Startup mode + window geometry are DEAD SETTINGS on the Swift host** (grep-verified: no
   `start_fullscreen`/`settings.window` use in pb-mac-ffi or mac/). Honor
   `start_fullscreen()` at launch, restore `settings.window` (via `geometry_on_screen`
   against live monitor rects), and write the frame back (winit's `track_windowed_geometry`
   equivalent). SwiftUI's own frame restoration will fight — ours must win.
3. **Local redistributable packaging** (CI credits exhausted → local, not a CI lane): the
   bundle has NO icon and NO `CFBundleDocumentTypes` (empty Resources/, grep-verified).
   Reuse the egui pipeline's assets/muscle: `packaging/macos/` icns + Assets.car +
   Info.plist doc-types, `scripts/release-macos.sh` (codesign → DMG → notarytool → stapler)
   adapted to the Swift host, run locally. Bundle id decision pending: keep
   `com.jdlien.PhotoBlazeMac` (coexists with the egui beta) until the NS3 cutover renames to
   the real identity.

After those: Settings polish (owner-deferred to last), the Shortcuts editor's missing actions
(Slideshow/CopyPath/CopyImageDetails/Reveal — same gap as egui; its "every action" comment is
stale), VoiceOver on the canvas overlays, then the cutover itself (default Mac target, retire
egui-on-Mac).

**Host-launch note:** the `AppDelegate` sets the activation policy in
`applicationWillFinishLaunching` so bare-binary launches (`swift run` / the executable straight
out of the .app) front + focus properly; it's also the future `application:openURLs:` hook. (The
windowless-window mystery itself was the `argv[1]` document-open suppression above, not
activation.)

**Pre-existing (optional) smoke — the three green tail commits of NS0** await a final owner pass:
scroll (`45af83b`), resize (`6ae58da`), PasswordSubmitted (`8dfc507`). Not blocking NS1.

**Move-recipe hazards (for any further Rust-side moves):** multiline `self\n.core\n.field` chains
(regex-collapse, not a literal replace); a method calling a shell *module* path or an associated
`Self::`/`App::` fn — the token classifier misses both, grep the body first; test-only imports go in
the `#[cfg(test)] mod tests` block; effects that enqueue follow-ups rely on the drain's bounded
loop-until-quiescent (already in place). Scratchpad has `move_methods.py` / `reclass3.py`.

**Optional Rust-side tidy (not blocking NS1):** the residual `ShellFlowAction` arms
(DeletePermanent-confirm / Recursive / CancelScan / Quit) are legitimately host-side commands and can
stay; the two egui-dialog *payload* seams (progress handles, error text) are the last things still
crossing shell↔core as concrete types.

---

A fast, chrome-less, keyboard-driven photo viewer. The prefetch engine ("hold a
key and fly") is done, plus broad multi-codec support, full-res RAW, the color
story (in-shader ICC → wide-gamut → HDR, task #11), and the rotation/zoom/pan/
scaling/EXIF/help UI (#1/#3/#4/#5/#7). Privacy no-trace (#2), Esc teardown (#6),
`enter` random nav (+ `Shift+Enter` prev-random), and the Windows-integration +
MSI track are all done. **Archive viewing (ZIP + 7z) shipped 2026-06-28**, now
including **in-app password entry + launch-path async open** — see the next section
(only RAM-budget tuning + small polish remain). **Configurable keybindings (#8) and
the fly-speed cap (#20) are also done, and the typed Settings model + live backend
are in (#22)** — only the Settings *dialog form* remains; see "Settings + configurable
keymap stream" below.

## 🍎 macOS (Apple Silicon) port — ✅ SHIPPED (v0.1.0-beta.4, 2026-06-30) — see `.taskmaster/docs/macos-port-plan.md`

A **cfg-gated monorepo** port (NOT a fork): one codebase, `#[cfg(target_os="macos")]`
siblings beside the `#[cfg(windows)]` arms; `cargo build` picks Metal / NSMenu / Image I/O
per-OS (the `windows` crate is target-gated so it never compiles on Mac). Validated
end-to-end on an **M2 Max** — HEIC/AVIF decode, P3 wide-gamut, and HDR all **confirmed
working on the built-in XDR**. Full plan + milestone/task table: `.taskmaster/docs/macos-port-plan.md`.
The session task list (`#1–#11`) maps to that plan.

**DONE this session (all on `main`; gated: 293 workspace tests, clippy `-D warnings`, fmt):**
- **M0 — compiles + runs on Metal.** The one real fix: the wgpu instance excluded Metal
  (`Backends::DX12 | VULKAN` → `Backends::PRIMARY`, in `gpu.rs` + `upload.rs`). Everything
  else (winit, egui, muda, rfd, trash) was already cross-platform.
- **HEIC/AVIF via Apple Image I/O** (`pb-decode/src/imageio.rs`, hand-rolled CGImageSource
  FFI, no new deps) — Mac mirror of `wic.rs`, same dispatch seam. SDR → Display-P3 RGBA8 +
  fixed P3→709 transform; HDR (PQ/HLG) → extended-linear-sRGB fp16. Shared ISOBMFF/`colr`
  parsers extracted to **`pb-decode/src/isobmff.rs`** (tests now run cross-platform).
  **Orientation gotchas (was the #1 bug report), both fixed:** (a) **NO CTM flip** —
  `CGContextDrawImage` lands the buffer top-down already; a flip vertically mirrors
  everything (was masked on symmetric photos). (b) Read **`kCGImagePropertyOrientation`**
  (combines `irot`+EXIF), NOT kamadak-EXIF — HEIC/AVIF often store rotation in the ISOBMFF
  `irot` transform, so kamadak returns 1 and the photo shows sideways. `apply_orientation`
  generalized to a byte-stride variant (`orientation.rs`) so the HDR fp16 path orients too.
- **EDR/P3 detection + wide-gamut/HDR Metal surface (the big one, fully working).**
  `display.rs` got a macOS `primary_hdr()` (raw `objc2` NSScreen) + `wide_gamut` +
  `edr_headroom` fields on `DisplayHdr`. Gate is now `want_fp16 = (hdr_on || wide_gamut)`
  → fp16 scRGB surface on **any P3 panel** (P3 lights up even on SDR panels). **wgpu sets
  the format but not the layer colorspace/EDR**, so `pb-app/src/hdr_surface.rs` pokes the
  `CAMetalLayer`: `colorspace = extendedLinearSRGB` + `wantsExtendedDynamicRangeContent`.
  **Critical platform difference: macOS EDR HARD-CLIPS above the panel headroom** (Windows
  DWM tone-maps for you) → the present pass got a **highlight roll-off** (`gpu.rs`
  PRESENT_WGSL `rolloff()`: ≤1.0 identity, `[1,peak]` asymptotes to the headroom). **Detect
  EDR from the WINDOW's screen, not `mainScreen`** — this was the bug that made HDR look
  totally broken on a multi-display setup (mainScreen was an SDR-mode monitor). And it
  **adapts on window move** (`WindowEvent::Moved` → re-query `[NSView window].screen`,
  re-poke only when the headroom changed). ⚠️ **objc2 gotcha:** `-[CAMetalLayer
  setColorspace:]` needs a *typed* `*const CGColorSpace` (`^{CGColorSpace=}`); a bare
  `*const c_void` panics objc2's debug msg_send verification → **abort at launch** (the
  panic can't unwind through AppKit's `app_did_finish_launching`).
- **`.app` bundle** — `scripts/bundle-macos.sh` + `packaging/macos/Info.plist` →
  `target/<profile>/bundle/PhotoBlaze.app`. Launches frontmost via LaunchServices; menu +
  ⌘-shortcuts work. Placeholder `.icns` from the 1024 master (real squircle = #7).
- **Menu wired on macOS** — `menu.rs` has a `#[cfg(target_os="macos")] build_menu`: proper
  App menu (About / Settings ⌘, / Quit ⌘Q), **real ⌘ accelerators** (Copy ⌘C …, via
  `Modifiers::SUPER`), `init_for_nsapp()` attach. `KeyChord` gained a **`logo` (⌘)** field
  so ⌘-chords don't fall through to bare-key actions (⌘S→Slideshow etc.). **Native
  fullscreen** (⌃⌘F / Globe+F) via an "Enter Full Screen" item → winit native fullscreen
  (muda's predefined Fullscreen item is buggy — maps `META` not `SUPER`, so it'd be ⌃F);
  borderless speed-mode stays F / ⌥⏎ / F11 (bare **`F`** added on *both* platforms).
- **Copy File Path** (Shift+Ctrl+C / ⇧⌘C, Edit menu) — cross-platform, via `arboard`.

**DONE 2026-06-29 (second pass — the "native-feel bundle," gated: 298 workspace tests,
clippy `-D warnings`, fmt; smoke-launched windowed + gallery, no panic):**
- **#9 macOS Window menu — DONE.** `menu.rs` macOS `build_menu` gained a standard
  **Window** submenu (Minimize ⌘M / Zoom / Bring All to Front, via muda
  `PredefinedMenuItem`s — native labels + selectors + ⌘M for free), placed App·File·Edit·
  View·Image·**Window**·Help. `BuiltMenu` returns the submenu (macOS-gated field) so
  `apply_menu_for_mode` can call `set_as_windows_menu_for_nsapp()` **after**
  `init_for_nsapp` (muda's required order) → macOS appends the live window list.
- **#5 SF Pro fonts — DONE (HUD + dialogs).** The theme *source* was already cross-platform
  (dialogs read winit `window.theme()`), so #5 was only the font swap. **HUD** (`hud.rs`):
  Arial → **SF Pro** (`/System/Library/Fonts/SFNS.ttf`). Tahoe ships SF Pro only as a
  *variable* font with no static semibold/bold, and **fontdue 0.9 can't instance the weight
  axis** (verified empirically) — so semibold/bold are **faux-bolded** by horizontal coverage
  dilation (`embolden_glyph`, baked at layout time before the outline pass; gated to only
  fire when no real heavier face exists, so Windows/Segoe is untouched). 4 new unit tests.
  **Dialogs** (`pb-ui::install_fonts`): Segoe → SF Pro too (egui/ab_glyph parses SFNS.ttf
  fine — gallery launches clean); kept pb-ui `cfg`-free by listing both platforms' paths.
- **#10 real `available_physical_ram()` — DONE.** macOS impl via Mach
  `host_statistics64(HOST_VM_INFO64)` (free+inactive+speculative × page size — the analog of
  Windows' `ullAvailPhys`), through **`libc`** (already in the lock graph; added as a macOS
  target dep — no new build cost). `#[allow(deprecated)]`-scoped on `mach_host_self` (libc
  moved mach to the `mach2` crate; not worth a dep for one call). Sanity test runs on the Mac.
  The stub is now `cfg(not(any(windows, macos)))`; the 7z budget is exact on Mac.
- **#12 title-bar proxy icon — DONE.** New `proxy_icon.rs` (mirrors `hdr_surface.rs`'s
  objc2 reach: raw-window-handle → `[NSView window]` → `setRepresentedURL:` an `NSURL`/nil).
  Cached `App::refresh_proxy_icon` runs per-tick in `about_to_wait`, gated on windowed-mode +
  `displayed_item` change (off the fly hot path); clears for archive entries / empty state /
  fullscreen (`source.path` is `None`). Gives the draggable doc-proxy + ⌘-click folder popup.
  RAM-only, never calls `noteNewRecentDocumentURL:` (no Recents → privacy #2 holds). The
  `stringWithUTF8String:` objc2 debug arg-encoding gotcha was tested clear in a windowed launch.
  **Visual confirm pending (owner):** hover the title bar in windowed mode to reveal/drag it
  (hover-to-reveal is standard since macOS 11).

**#7 app icon — DONE (2026-06-29).** JD authored the Liquid Glass icon in Icon Composer
(`icons/AppIcon.icon` — 3 SVG layers: flame/border/mountains-sun) + a flat legacy PNG.
`bundle-macos.sh` now: **modern** = `actool` compiles the `.icon` → `Assets.car`
(`CFBundleIconName=AppIcon`, the no-Xcode path; needs Xcode 26+, skips gracefully); **legacy**
= the flat PNG → `PhotoBlaze.icns` (`CFBundleIconFile=PhotoBlaze`) with its transparent photo
interior **flood-filled white** (ImageMagick) so it reads on a dark *and* light Dock. Covers
Tahoe 26 + Golden Gate 27 (same pipeline). Verified: bundle assembles both, `assetutil` shows
the AppIcon appearances, the rendered `.icns` is correct on dark/light.

**#11 release pipeline — WIRED (2026-06-29), pending JD's secrets + a first tagged release.**
`scripts/release-macos.sh` (codesign hardened-runtime → DMG → `notarytool` → `stapler`,
gated on secrets like the Windows job), a `macos-dmg` job in `release.yml` (`macos-15` arm64,
selects newest Xcode, `brew install imagemagick`), `packaging/macos/entitlements.plist`
(empty — a Rust app needs no hardened-runtime exceptions), and `scripts/setup-signing-secrets.sh`
(securely sets the 5 repo secrets from a `.p12`). Doc: `.taskmaster/docs/release-signing.md`
(macОС section). **TODO (JD):** run `setup-signing-secrets.sh` with the Developer ID `.p12`,
then tag a release. CI gets the glass icon only on a runner with Xcode 26 (else flat fallback)
— cut locally for guaranteed glass.

**#6 file associations — DONE (2026-06-30).** Info.plist `CFBundleDocumentTypes` for images
+ archives (role Viewer / rank Alternate). winit drops `application:openURLs:`, so
`macos_open.rs` grafts that method onto winit's delegate via `class_addMethod` (installed in
`main()` before `run_app` so even a cold double-click is caught) → queue → `open_input`.
Verified cold + running via `open -a`.
**#8 chromeless borderless fullscreen — DONE (2026-06-30).** `macos_chrome::set_chromeless`
sets `NSApplicationPresentationOptions` to auto-hide menu bar + Dock in the borderless mode
(reclaims the strip + un-clips the photo top, stays in the current Space; hover reveals).
Smoke-tested (launch/quit clean, no Dock wedge).

**LEFT (small follow-ups only):**
**Follow-up:** toggling a display's HDR *while the window sits on it* (no move) isn't caught
live — needs an `NSApplicationDidChangeScreenParametersNotification` observer (adapts on next
move/navigate now). Optional: proxy-icon thumbnail (Option 3) if a recognizable mini-photo is
wanted in the title bar. CI ships the glass icon from prebuilt `packaging/macos/Assets.car`
(no Xcode 26 on the runner); regenerate via `scripts/build-macos-icons.sh` when the icon changes.

**Build/run/test on Mac:**
- `./scripts/bundle-macos.sh` → `open target/release/bundle/PhotoBlaze.app --args <folder>`
- `cargo run -p pb-render --example hdr_probe` — per-display EDR/P3 + what `primary_hdr` picks.
- `cargo run -p pb-decode --example decode -- <heic/avif>` — decode + color report.
- **HDR check:** window on the built-in XDR (or any HDR-*enabled* panel) → flick
  `~/Downloads/test-images/WideGamut-*-HDR.avif`. **Screenshots clip EDR to SDR** (look
  blown even when the panel is correct) — trust the panel, not a grab. **Quit cleanly via
  ⌘Q / the menu** — force-killing the frontmost app can wedge the Dock's auto-hide.

**Key macОС files:** `crates/pb-decode/src/{imageio.rs,isobmff.rs,orientation.rs}`,
`crates/pb-render/src/{display.rs,gpu.rs}`, `crates/pb-app/src/{hdr_surface.rs,menu.rs,
keymap.rs,main.rs,action.rs}`, `scripts/bundle-macos.sh`, `packaging/macos/Info.plist`.

---

## Archive viewing — ZIP + 7z (2026-06-28) — DONE (Task 30 complete)

Open a `.zip`/`.7z` and browse the images inside like a folder (CLI arg, double-click
association, drag-drop, or the Open dialog's "Images & archives" filter). Same fast
prefetch/nav as loose files; in-archive browsing is recursive/flattened (sorted by entry
name). All on `origin/main`, gated (clippy `-D warnings` + fmt + tests). **Task 30** in
`tasks.json`.

**How it works:**
- **`pb-source` crate** — a `PhotoSource` seam (bytes + name + container for item `i`):
  `FsSource`, `ZipSource` (lazy per-entry, handle-pool for parallel reads),
  `SevenZSource` (eager — 7z is usually *solid* = no cheap random access, so the whole
  archive is decompressed into RAM on open). `pb-core` nav is unchanged (index-only), so
  the prefetch ring / decode pool didn't change.
- `pb_decode::decode_named_bytes` — decode in-memory bytes with an extension hint (so
  RAW/SVG/TGA route without a file path). Shift+I panel shows the archive path + in-zip folder.
- **7z memory safety** (`pb-app/src/archive.rs`): a real OOM *aborts* (uncatchable) in
  Rust → **predict-and-refuse** rather than try/catch. Sum the 7z header's uncompressed
  image sizes vs a RAM budget (fraction of `GlobalMemoryStatusEx` available − app
  reservations − transient margin; `PB_ARCHIVE_RAM_BUDGET` env override). Over budget →
  instant refusal, no load. `Vec::try_reserve` backstops the buffers.
- **Async open:** a `.7z` eager-decompresses on a background thread
  (`begin_archive_open` → per-tick `poll_archive_load`, generation-guarded so a newer
  open supersedes the first); the event loop stays live + the current photo stays
  visible; "Loading archive…" toast. `.zip` open is instant (synchronous).
- **Launch-path async (2026-06-28):** an archive on the CLI / double-click is **deferred
  into `resumed()`** (`pending_launch` + `queue_launch`, fired once after the window +
  engine exist) — the window appears immediately and a big `.7z` loads behind the spinner,
  and a launched encrypted/failed archive can use the egui dialogs instead of only logging.
  Folders / file lists still resolve synchronously in `main()`.
- **Password entry (2026-06-28):** `DialogKind::Password` — a dark-aware egui dialog with a
  blue lock icon, masked auto-focused field, Unlock/Cancel, **Enter submits / Esc cancels**;
  a **wrong password re-prompts in place** with an inline "Incorrect password" error, and a
  **"Checking…"** state covers the async 7z re-open. `ZipSource::password_ok()` validates a
  supplied zip password (a zip `open` succeeds even when *wrong* — an entry decrypt is the
  real check); `seven_z_projected_bytes` threads the password so a header-encrypted 7z
  pre-flights. `Option<password>` runs through `begin_archive_open`/`open_archive`/
  `seven_z_preflight`/`load_seven_z`; `finish_archive_open` routes PasswordRequired→prompt,
  success→close+rebuild, other→error dialog (`password_archive` holds the pending path).
  Password is RAM-only + scrubbed on dialog drop. Verified end-to-end on encrypted `.zip`
  **and** `.7z` (wrong→error, correct→opens); plain archives unaffected.
- **Structured errors → egui dialog:** `ArchiveOpenError`
  (too-large / corrupt / OOM / empty) → `DialogKind::Message` (dark-aware, via `open_message`).
  PasswordRequired no longer hits this — it opens the password prompt instead.
- **Privacy:** RAM-only — `viewing_a_zip_writes_nothing_to_disk` +
  `viewing_a_7z_writes_nothing_to_disk` prove no extraction to a temp dir.
- Crates: `zip` (deflate + aes-crypto) + `sevenz-rust2` 0.21 (incl. LZMA2/bzip2/ppmd) —
  both **pure Rust, no C build risk**.

**Task 30 finished (2026-06-28) — the last three subtasks landed:**
- **RAM budget measured + validated (#5).** New `crates/pb-source/examples/archive_probe.rs`
  (projected resident bytes + eager-open time/throughput + process peak working set via a
  tiny `K32GetProcessMemoryInfo` FFI). Measured solid-LZMA2 JPEG archives from the real
  corpus: 3.3 GB → open 3.3 s (~1 GB/s), peak/resident **1.02×**, transient **+60 MB**;
  0.7 GB → open 0.9 s, peak/resident **1.26×**, transient **+201 MB**. So the projection
  closely predicts real RAM (gate is trustworthy), the eager-open transient is a flat
  ~60–200 MB (fixed decoder cost, not proportional → a flat `TRANSIENT_MARGIN` is correct,
  512 MB covers it), and ~1 GB/s open confirms async is needed. The `archive.rs` constants
  (0.6 / reservations / 512 MB) are validated; their comments rewritten from guesses to data.
- **Deterministic over-budget refusal test (#6).** `seven_z_preflight` now delegates to
  `seven_z_preflight_within(path, password, budget)` — a budget-injection seam (no
  `PB_ARCHIVE_RAM_BUDGET` env-var race). `over_budget_7z_is_refused_with_structured_error`
  pre-flights a real 7z against a 1-byte budget → `ArchiveOpenError::TooLarge` (never an
  abort), then passes under `u64::MAX`. All 10 pb-app archive tests green.
- **WiX archive registration (#7).** `wix/main.wxs` adds a `PhotoBlaze.Archive` ProgId +
  `.zip`/`.7z` `OpenWithProgids` (candidate-only, never the default handler). The Open
  dialog already filtered `.zip`/`.7z`. XML validated (well-formed, all refs resolve).

**Owner/CI-side manual checks (can't automate here):** install the MSI → `.zip`/`.7z` show
PhotoBlaze in the "Open with" menu; on a real low-RAM machine the over-budget *dialog*
appears (the 96 GB dev box never refuses naturally — drive it with `PB_ARCHIVE_RAM_BUDGET`).

**Deferred (behind the same seam, not needed for v1):**
- **Huge-archive escalations:** in-RAM per-block LRU (bounds RAM, keeps no-trace) or opt-in
  extract-and-delete (disk → opt-in + disclose + clear-on-close + leftover-sweep-on-startup).
  v1 just refuses + lets the user extract.
- Exotic 7z codecs (zstd/brotli/lz4 features off) and header-encrypted 7z error gracefully.

**Key files:** `crates/pb-source/src/lib.rs` (incl. `ZipSource::password_ok`,
password-threaded `seven_z_projected_bytes`), `crates/pb-app/src/archive.rs`,
`crates/pb-app/src/dialog.rs` (`DialogKind::Password` + `password_dialog`), and the
`main.rs` open path (`open_input` / `begin_archive_open` / `poll_archive_load` /
`finish_archive_open` / `prompt_archive_password` / `open_archive` / `seven_z_preflight` /
`load_seven_z` / `seven_z_preflight` → `seven_z_preflight_within` / `resolve_playlist`;
launch defer = `queue_launch` + `resumed`), `crates/pb-app/wix/main.wxs` (archive ProgId),
`crates/pb-source/examples/archive_probe.rs` (the #5 RAM-measurement tool). Tests:
`pb-source` (14, incl. encrypted-7z round-trip + zip `password_ok`) + `pb-app`
archive-budget + `viewing_a_{zip,7z}_writes_nothing_to_disk` + `over_budget_7z_is_refused_with_structured_error`.
Password flow verified interactively on encrypted `.zip`/`.7z` (GDI-capturable egui dialog).

## Settings + configurable keymap stream (2026-06-28) — keymap (#8) + fly-cap (#20) DONE; Settings dialog (#22) backend in, form is next

The keyboard is now fully configurable and the typed settings model is in; the one
remaining piece is the Settings **dialog form** (controls + the keybinding editor).
All committed on `main`, gated (clippy `-D warnings` + fmt + `cargo test -p pb-app`, 81 green).

**Shipped (committed `499e3d6`, `c5e5bf5`):**
- **#8 configurable keybindings — DONE.** New `action.rs` (the central `Action` enum:
  one-shot / nav / held `kind` + stable snake_case `id`; pure + unit-tested) and
  `keymap.rs` (`KeyChord` parse/Display like `"Ctrl+S"`, a default binding table = today's
  keys, optional `keymap.toml` → load / merge-over-defaults / validate with unknown-action,
  bad-key, and duplicate-key warnings). Every keypress now resolves through the keymap and
  routes by kind to **one `App::dispatch_action`**; the native menu maps
  `MenuAction::to_action` into the *same* dispatcher; the help overlay's key labels are
  generated from the live keymap (single source of truth). `held` is now
  `HashMap<KeyCode, Action>` (action captured at press) so nav/pan/zoom are remappable too.
  ~16 action/keymap unit tests.
- **#20 max photos/sec cap — DONE.** `advance_interval` gained a `max_rate` ceiling (the
  cap clamped to the display refresh; `0` or `>= refresh` = uncapped), read **live** from
  `Settings.max_advance_rate`. New `advance_interval_caps_at_max_rate` test.
- **#22 Settings — typed model + live backend (subtask 2 DONE; 3/5 in progress).**
  `settings.rs` is now a typed **serde + toml** `Settings { fullscreen, recursive,
  start_speed, ramp_secs, max_advance_rate, hold_delay_ms, scale_mode, letterbox,
  info_opacity }` with `#[serde(default)]`, clamped `load`, atomic `save`, + 7 tests;
  defaults mirror today's constants and an old `key=value` `fullscreen` file still loads.
  `App` holds it; the nav-feel curve + `initial_delay` + the #20 cap read it live (a
  `settings.toml` edit applies on next launch; mutating `App.settings` will apply live).
  **File ▸ Settings…** menu item added + `Ctrl+,` (both open the dialog).

**Remaining for #22 (the dialog form):**
- Wire the egui `settings_ui` controls to live `App.settings` — Save / Cancel / Esc +
  live-apply (mirror `take_confirm_result`: the dialog returns the edited `Settings`,
  `App` applies it live and snapshots on open for Cancel-revert).
- Two backend setters still to land so the dialog's color/opacity controls do something:
  **letterbox color** (`WgpuRenderer::set_letterbox`, currently the `pb_render::LETTERBOX`
  const) and **info-panel opacity** (thread an alpha into `hud`'s info panel, currently
  the `hud::BG` const); plus applying default scale/recursive at startup.
- The **keybinding editor** (subtask 4): key-capture → assign via the keymap, conflict
  display (reuse the keymap's duplicate-key check), reset-to-default, persist `keymap.toml`.
- **Coordination:** the form work is in `dialog.rs`, which the archive session has been
  co-editing (`button_bar`, password dialog) — do it once that settles to avoid the churn.

**Key files:** `crates/pb-app/src/` `action.rs`, `keymap.rs`, `settings.rs`, `menu.rs`
(`to_action` + the Settings item), `main.rs` (`dispatch_action`, `advance_interval` cap,
`held` map, `App.settings`). Config lives at `%APPDATA%\PhotoBlaze\{settings.toml,
keymap.toml}` — read-only on the view path (privacy #2; writes only on Save / fullscreen toggle).

## UI / file-commands stream (2026-06-28) — what just shipped + what's next

Separate from the HEIC/decode stream below. All on `origin/main`, each gated
(`clippy --all-targets -D warnings` + `fmt` + `cargo test -p pb-app`, 51 tests green).

**Shipped this session:**
- **Native menu bar** (`menu.rs`, muda; dark-aware via `darkmode.rs`) —
  File/Edit/View/Image/Help, windowed-only. Pure `action_for` (tested) →
  `App::dispatch_menu`. Dynamic enable-state for File ▸ Save Rotation.
- **egui dialog infra** (`dialog.rs`: 2nd winit window + egui-wgpu, OS dark/light):
  **About** (done), **Settings** (skeleton form), and a themed **Confirm** dialog.
- **#27 Copy** (`Ctrl+C` / Edit ▸ Copy, `clipboard.rs`): full-res decode → clipboard
  in BOTH **CF_DIBV5** (pixels) + **CF_HDROP** (file ref) via Win32 (dropped arboard).
  Pure transforms (fp16→sRGB8, rotate-bake) unit-tested.
- **#29 Save Rotation** (`Ctrl+S` / File, `save_rotation.rs`): **lossless** EXIF
  Orientation write via `little_exif` (JPEG only; atomic temp+rename; verified scan
  byte-identical + ICC preserved). Pure orientation-compose tested; drop RAM override
  + refresh-from-disk after save.
- **#28 Delete** (`delete.rs`): `Del` → Recycle Bin (`trash` crate), `Shift+Del` →
  **themed egui Confirm** (Directory Opus-style: file-✗ icon, ⚠ line, red Delete) →
  permanent. Pure cursor-after-removal tested; rebuilds source minus the path,
  advances (prev if last; empty state if none). Icon-only toasts; 160 ms
  deferred-advance so the icon shows before advancing.
- **FA icon system** (`icon.rs`): vendored **solid** FA SVGs (`icons/*.svg`) →
  rasterized (resvg) into HUD/toast pills + dialog chrome. (Tried duotone, switched
  to solid.) "To add an icon" workflow codified in CLAUDE.md.
- **#19 hold-to-fly accel ramp** (done).

**Next (UI), recommended order:**
- **#8 keybindings + #20 fly-cap — DONE**, and **#22 Settings** has its typed model +
  live backend in (see the "Settings + configurable keymap stream" section above). The
  remaining #22 work is the **dialog form** (wire controls to live `App.settings` with
  Save/Cancel, + the keybinding editor) plus two small backend setters (letterbox color,
  info-panel opacity). Do the form once the archive session's `dialog.rs` refactor settles.
- Then: **#9** recursive ordering, **#10** richer per-action toast strings (now easy —
  route through `Action`), **#23** slideshow.
- **Decided/deferred:** file-open picker stays **native `rfd`** (auto-dark on macOS;
  the light Windows dialog is an accepted gap — theming the shell dialog isn't worth
  it). The egui Confirm is the portable keeper (no `NSAlert` needed for the Mac port).

**Key files** (`crates/pb-app/src/`): `main.rs` (App + winit loop + dispatch + delete/
save/copy wiring + `dialog_event`), `menu.rs`, `dialog.rs`, `clipboard.rs`,
`save_rotation.rs`, `delete.rs`, `icon.rs`, `hud.rs`, `settings.rs`, `darkmode.rs`.
**GOTCHA:** the photo window is an uncapturable flip-swapchain (HDR) — verify on-photo
visuals with the owner; the **egui dialogs + menu DO GDI-capture** (screenshot them).
The release exe is **GUI-subsystem (no stderr)** — debug via a temp log file, not eprintln.

## ⏭ ACTIVE NEXT WORK: HEIC decode — Phases 0–3 DONE; only follow-ups remain — see
[`docs/heic-decode-plan.md`](docs/heic-decode-plan.md) (read the SESSION UPDATE at top)

**The libheif pivot landed end-to-end (Phases 0–3 done, 2026-06-28).** WIC's HEVC
decoder serializes (1.57×/8 threads, measured); the new **CPU `libheif` backend** is
parallel (~5×/8 threads) → **~45 full HEIC/s vs WIC's 9.4 (≈4.8×)**, lower single-image
latency too (115 ms vs 167). Behind the **`libheif` cargo feature** (OFF by default —
pure-Rust core stays toolchain-free, ADR-015); routed for **full SDR HEIC only**
(previews/AVIF/HDR stay on WIC); A/B via `PB_HEIC_BACKEND=wic`. iPhone output is
**pixel-identical to WIC**; orientation perfect. Set up: **`scripts/setup-libheif.ps1`**
(vcpkg + decode-only static libheif, `-DENABLE_PLUGIN_LOADING=OFF`).

- **Build/run with it:** `cargo run -p pb-app --release --features libheif -- "<folder>" -r`
  (needs `VCPKG_ROOT` or vcpkg at `~/vcpkg`; run the setup script once first).
## 🔬 2026-06-28 (late): the "1 s after flying" hunt — root cause was RAW, not HEIC

Owner reported full-quality still lagging ~1 s after flying + stopping in
`D:\Media\Pictures\2021` (905 iPhone HEIC + 285 Sony `.arw` + jpg/png). Stopped
guessing and **instrumented the real pipeline** (`--metrics` now also prints a
`sharpen` stage = full-requested→on-screen, and `pool decode (under load)`
percentiles + the slowest files). Findings, all measured:
- **The villain was RAW, not HEIC.** Pool decode p95 was **1388 ms**; the slowest were
  all `prev DSC*.ARW` at **~1.4 s each** — the RAW **preview** path was **demosaicing**
  (`DSC` sorts before `IMG`, so the ARWs sat in the startup window jamming all 8
  workers; any HEIC you stopped near paid the contention).
- iPhone HEIC sharpen itself is ~120 ms isolated but stretches under 8-way load
  (decodes balloon several-fold). No re-decode churn (decode count normal).

**Three fixes landed (all green):**
1. **Fix C — RAW preview never demosaics** (`pb-decode/raw.rs`): a preview request
   uses the embedded JPEG thumbnail (fast, ~tens of ms); the 100×+ demosaic is now
   **full-decode-only**. *This is the actual ~1 s fix.* Result on the 2021 folder:
   pool decode **p99 1467→259 ms, CPU 58→13 s**, the 1.4 s tail gone.
2. **Fix A — no-thumbnail HEICs route to libheif** (`route_full_heic` + `has_thumbnail_ref`):
   WIC fakes a thumbnail by full-decoding the grid (slow) for HEICs lacking a real
   `thmb` item (macOS-encoded Sony HEICs); those previews now go to libheif (one
   parallel decode, no WIC double-decode).
3. **Fix B — prefetch fulls *ahead*** (`pb-app` `sharpen_now`/`prefetch_fulls`/tiered
   `request_prefetch`): the full-res ring is now requested **even while flying**, but
   at LOW priority (queued behind every preview), so it fills the cores' spare
   capacity and the photo you stop on is often already sharp. RAW is **excluded** from
   the speculative ahead-ring (demosaic is too expensive to do for neighbours).
   Converges to idle (no churn). **Fly-then-stop feel needs owner verification** (can't
   inject keypresses).

**Still open (smaller):** iPhone HEIC *thumbnails* (WIC `GetThumbnail`) serialize under
load (~240 ms each when 8 run) — flying through dense HEIC could still be preview-bound.
Plus the earlier follow-ups: Sony HEIC color (**tasks.json #24**), Fill-mode decode-to-fit,
sync load paths bypass preview-first, AVIF on libheif.

**Privacy cleanup (flagged 2026-06-28, NOT yet applied):** the `--metrics` `pool decode`
diagnostic logs viewed photo **filenames** (`main.rs` ~L491, committed in `f346506`) to
stdout. Low practical exposure — opt-in flag, and release is a GUI subsystem with no console
— but the strict no-trace guarantee says *"no log of viewed paths,"* and `--metrics` is meant
to run in **release** (benchmarking), so the code ships. One-line fix: log the **extension
only** (`prev .arw`, not `prev DSC02715.ARW`) — keeps the format-level diagnostic, drops photo
identity. Held off because `main.rs` is the parallel session's active file.

### 🧪 2026-06-28 — parallel thumbnail extraction: TRIED, REVERTED (negative result — don't blind-retry)
Implemented libheif thumbnail extraction to replace WIC `GetThumbnail` (the ~240 ms
serializer above). **The capability works**: `heif_image_handle_get_thumbnail` + decode
gives the embedded thumbnail in **~3 ms (vs WIC ~20 ms isolated), correctly oriented**
(240×320 on a portrait file, matches WIC), fully parallel. **But it made things worse
overall and I reverted it**, because:
- Routing previews onto libheif made the *concurrent full* decodes **~4× slower** —
  windowed bench p95 **235 → 900 ms on the same files**. Mechanism: fast previews freed
  the workers to run *more* full grid decodes at once, and many concurrent libheif
  decodes slow each other down badly.
- **Two obvious fixes did NOT help** (both measured): capping libheif's per-context tile
  threads (`heif_context_set_max_decoding_threads` 1/2/4), and capping *concurrent full
  decodes* in the pool (a non-preview semaphore, 2/3/4). p95 stayed ~900 ms either way.
- So it's **not** thread oversubscription and **not** simple concurrent-full count.
  Leading hypotheses: (a) a **libheif/libde265 global lock** taken during decode (more
  concurrent calls → more contention), or (b) a **windowed-only artifact** — the bench
  uses a 64-photo window + Lanczos downscale-to-fit; the owner runs **fullscreen** with a
  ~12-photo window and *no* downscale for ≤12 MP, so it may not reproduce there at all.

**Headline learning:** HEIC **full** decodes balloon several-fold under 8-way concurrent
load (~138 ms isolated → ~900 ms windowed). That under-load latency — not the per-decode
speed — is the real ceiling on stop-to-sharp; Fix B (prefetch-ahead) dodges it by
pre-decoding, but understanding/limiting it is the next real lever.

**Kept (committed in `f346506`):** the `--metrics` instrumentation that found all of this
— `sharpen` (full-requested→on-screen) and `pool decode (under load)` percentiles + the
slowest-files list. Re-run with `--metrics` to investigate further.

**To resume:** re-apply the thumbnail extraction (it's straightforward — handle→
`get_number_of_thumbnails`/`get_list_of_thumbnail_IDs`/`get_thumbnail`→decode, orientation
1) **behind a default-off flag** so it can be A/B'd in fullscreen; OR first test the
global-lock hypothesis (time decode vs. time-holding-a-lock). Don't re-land it on by
default without a fullscreen win.

**Phase 3 evolution:** `upgrade_item`→`sharpen_now` (displayed, tier 1) +
`prefetch_fulls` (ahead-ring, tier 3, ungated); pure `pb_core::full_ring` bounds the
ring (budget + `MAX_FULL_RING=24`). The held-nav gate is gone (replaced by the priority
tiers). Decode **cancellation works** (queued-you-flew-past skipped; in-flight finishes
but result discarded; no mid-decode abort).

**Green bar:** `cargo test --workspace` (**175**) + `-p pb-decode --features libheif`
(**58**, +3 routing/thumb tests); `cargo clippy --workspace --all-targets` and
`--features libheif` (clean); `cargo fmt --all --check`. Converges to idle on every
folder tested; no stderr spam. Diagnostics (`sharpen`, `pool decode`) are `--metrics`-gated.
Throwaway A/B tools: `heic_bench`, `heic_compare` in `pb-decode`.

---

## ✅ DONE: color management + wide-gamut + HDR output

Three layers, all behind the established seams (`ImageDecoder`, `Renderer`):

### 1. In-shader ICC color management
`pb_decode::color::ColorTransform { matrix:[[f32;3];3], trc:[f32;7], enabled }` —
source-linear→BT.709 3×3 (via `moxcms` `transform_matrix`) + the source EOTF as
moxcms's unified 7-param curve. Carried on `DecodedImage::color` (default sRGB
passthrough). Per-backend extraction:
- **JPEG** APP2 (`zune` `icc_profile`); **PNG/TIFF/WebP** (`image`-crate concrete
  decoder `icc_profile` — `load_with_icc`); **JXL** `rendered_icc`.
- **HEIC/AVIF** (`wic.rs`): the MS HEIF decoder returns **0 WIC color contexts**
  (verified), so the ISOBMFF **`colr` box** is parsed from bytes — `prof`/`rICC`
  embedded ICC *and* `nclx` CICP. (WIC color-context query kept as a fallback.)
- sRGB / ~2.2-gamma-with-sRGB-primaries → `enabled=false` passthrough (bit-exact).

### 2. fp16 scRGB render path (`pb-render`)
Scene → `Rgba16Float` **scRGB-linear intermediate** (`SCENE_WGSL`: source→scene-linear,
mode 0 sRGB / 1 convert-no-clamp / 2 scene-linear-passthrough; per-image output
`scale`). Then a fullscreen **present** pass (`PRESENT_WGSL`) → the surface: SDR 8-bit
= extended-Reinhard tone-map (per-image `peak`) + sRGB-encode; HDR fp16 = copy through.
Overlay composites into the linear intermediate so one present pass serves both.

### 3. Wide-gamut + HDR output — **pure wgpu, no native D3D12 interop**
**Key fact:** a DXGI **fp16 (`Rgba16Float`) flip-model swapchain is always scRGB**
(linear, BT.709, extended range; 1.0 = 80 nits) — no `SetColorSpace1` needed, and
wgpu already offers `Rgba16Float`. So `pb_render::display::primary_hdr()` (DXGI
`GetDesc1`) detects an HDR desktop and configures an fp16 surface; else 8-bit
non-sRGB. HDR AVIF/HEIC decode to fp16 scene-linear via WIC `128bppRGBAFloat` (**WIC
does the PQ/HLG decode + gamut + linearization for us**; `PixelFormat::Rgba16F`,
`common::finalize_hdr_scrgb`). Brightness baked in the scene pass: SDR content ×
SDR-white-scale, HDR content × 1.0 (absolute scRGB → highlights blow past SDR white).

**Tests:** color unit tests (passthrough / P3 / AdobeRGB / CICP / LUT-sRGB / garbage);
`colr`-box byte fixtures (prof + nclx + HDR-transfer); `finalize_hdr_scrgb` fp16
tests; pb-render golden tests (SDR round-trip, enabled-curve). Verified live via the
`decode` example + the `offscreen_png` render; on-screen wide-gamut/HDR confirmed by
the owner (the fp16/HDR swapchain is uncapturable by GDI — see caveat).

### Open followups (color/HDR)
- Real **SDR-white level** via the DisplayConfig API (currently a 200-nit default in
  `display.rs`); revisit WIC's scRGB reference-white assumption if brightness drifts.
- **Per-output** HDR detection (currently the primary output only).
- **Radiance-HDR / OpenEXR** (image-crate, not WIC) still clamped to SDR; CMYK JPEG
  mis-colored; LUT/CLUT & gray ICC → sRGB passthrough (`lcms2`-behind-a-flag).
- **Committable color test fixtures**: tiny re-tagged P3/AdobeRGB swatches +
  integration test (`magick` can tag PNG/TIFF/WebP/JPEG; emit the ICC via
  `moxcms::encode()`). AVIF/JXL/HEIC need delegates we lack, but `colr` is unit-tested.
- macOS output = wgpu `Rgba16Float` surface + CAMetalLayer EDR (deferred; cheap port).

### ⚠ Capture caveat
On an **HDR desktop**, GDI `CopyFromScreen` *and* `PrintWindow` capture the
flip-model swapchain as **all-white** (a Windows limitation, not a render bug). Use
`cargo run -q --example offscreen_png -p pb-app -- <img> out.rgba` (then
`magick -size WxH -depth 8 rgba:out.rgba out.png`) to verify rendering off-screen.

### Spike / dev tools (kept)
- `crates/pb-render/examples/hdr_probe.rs` — DXGI display-capability probe (→ folds
  into a real `DisplayCaps` detector later).
- `crates/pb-app/examples/offscreen_png.rs` — render the real pipeline to a buffer
  (visual verification while on-screen capture is broken).

---

## Keymap (current)
```
space            next photo            ⌫              previous photo
← ↑ ↓ →          pan (hold; accelerates)
= / -            zoom in/out (hold; accelerates; numpad +/- too)
8 / 9            scaling mode: fit / fill        0   toggle original 1:1 ↔ fit
                 (any of 8/9/0 also resets zoom/pan to that mode's framing)
r / Shift+R      rotate 90° cw / ccw (per-image, RAM-only)
Ctrl+S           save rotation to file (lossless EXIF; JPEG only)
Ctrl+C           copy full-res image to clipboard (pixels + file ref)
Del / Shift+Del  delete → Recycle Bin / permanent (themed confirm)
i / Shift+I      info panel / full-EXIF "nerd" panel
/ or ?           keybindings help overlay
Ctrl+,           settings (egui dialog — model + backend wired; form WIP)
esc              quit
(windowed mode also has a native menu bar: File/Edit/View/Image/Help, incl. File ▸ Settings…)
```
**These defaults are now the built-in keymap (task #8 done).** All keys resolve through
`keymap.rs` and are remappable via an optional `%APPDATA%\PhotoBlaze\keymap.toml`
(`[keys]` table, action-id → chord string/array, e.g. `rotate_cw = "R"`); the in-app
keybinding editor is the remaining #22 piece.

## Run it
```
cargo run -p pb-app --release -- "D:\Media\Pictures" -r     # fullscreen, recursive
cargo run -p pb-app --release -- "<leaf folder>" --windowed # dev window
cargo run -p pb-app --release -- "album.7z" --windowed      # open a .zip / .7z archive
cargo run -q --example decode -p pb-decode -- <files...>    # decode + color-transform report
cargo run -q --example hdr_probe -p pb-render               # display HDR/gamut/nits probe
```

## Architecture
```
crates/pb-core    pure nav/shuffle/prefetch/cache + ResidentRing + open (launch policy) — no I/O, no GPU
crates/pb-decode  ImageDecoder backends (zune/image/jxl/svg/raw/wic) + dispatch + decode-to-fit + EXIF + color (ICC→shader transform, fp16 HDR) + decode_named_bytes
crates/pb-source  PhotoSource seam: FsSource / ZipSource / SevenZSource (bytes+name+container for item i; RAM-only, read-only) — zip + 7z archive viewing
crates/pb-render  wgpu presenter (gpu.rs: scene→fp16 scRGB intermediate→present; WGSL); display (HDR detect); ViewTransform; UploadStrategy
crates/pb-app     winit loop, decode_pool (priority workers), hud.rs, archive.rs (RAM budget + errors), action.rs + keymap.rs (central Action + configurable keymap, #8), settings.rs (typed serde+toml prefs), menu.rs/dialog.rs, main.rs (engine wiring + dispatch_action)
```

## The prefetch engine (don't break it)
Decode/I-O are off the event loop on a priority worker pool; neighbors are
prefetched into a byte-budgeted (~1.5 GB) resident GPU texture ring; a keypress is a
**rebind, not a decode** (the color/scale uniforms are baked at upload; present_slot
only updates a 16-byte peak uniform). Advance is **gated on readiness**. The
gated-advance/failure paths in `main.rs` (`advance`/`about_to_wait`/`drain_results`/
`present_item`/`present_failed`) are subtle — re-read before changing them.

## Other backlog (tasks.json)
- **#8 configurable keybindings (TOML) — DONE**; **#20 fly-speed cap — DONE**; **#22
  Settings UI — in progress** (model + backend done; dialog form + keybinding editor left).
- #9 recursive ordering, #10 feedback toast (now routable via `Action`), #23 slideshow.
- **#2 privacy/no-trace — DONE** (static audit + `viewing_a_folder_writes_nothing_to_disk`
  no-trace test + CLAUDE.md "Privacy guarantee" section; opt-in-persistence subtask
  deferred — nothing on disk to gate yet). **#6 esc-teardown — DONE** (`begin_exit`:
  hide window first → `clear_session_state` (RAM-only) → exit; Drop frees VRAM/pool
  after).
- #12 Windows open (file-arg/drag-drop/picker) — **in progress** (subtask 1, the pure
  `pb-core::open` seam, done in the tree); #13 MSI/associations; #14 polish; #15 macOS.
- #1/#3/#4/#5/#7/#11 done.
- Native scaled-decode (JPEG DCT, WebP downscale-on-decode) still a TODO.
- **`enter` random nav — WIRED** (Enter/NumpadEnter → `Playlist::random_next`, hold-to-fly
  via the new `Nav` enum). The pinned cycle-boundary prefetch bug is **fixed**
  (`extend_random` now peeks `Playlist::next_shuffle()` across the reshuffle seam) and
  its test un-ignored. NOTE: the shuffle seed is fixed (0), so the random order repeats
  each launch — fine for now (deterministic/testable/privacy-safe); vary the seed later
  if per-launch variety is wanted. The DXGI photon-timing step is the only Phase-3 item
  still deferred.
- **random→sequential is no longer slow** (polish): the `Direction::Random` prefetch
  now also keeps the current photo's *sequential* neighbours (cur±1) warm at LOW
  priority (`prefetch.rs`, HEDGE=2), so the first space/backspace after an `enter`
  jump is an instant ring hit instead of a cold decode — without slowing random fly
  (the hedge loads only once the pool catches up at rest).
- **"Not-ready" loading pie** (polish, #2-style affordance): a translucent top-right
  pie (`hud::render_pie` → renderer `set_pie` → `App::tick_pie`) shown while the next
  photo is still decoding (a miss outlasting ~120 ms). No true decode progress exists,
  so it eases asymptotically toward — never reaching — full on a self-calibrating time
  constant (`decode_ewma`, a rolling mean of real miss durations), snaps to full +
  fades when the photo lands, and brightens on a keypress the engine can't yet service.
  Re-rasterized only on a visible change. **Interactive verification by owner pending**
  (hold space/enter on a cold folder to see it; GDI capture is broken on the HDR desktop).

## Environment / gotchas
- `cargo` at `~/.cargo/bin` (`$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`).
- MSRV **1.80**: no `Option::is_none_or` (1.82+) — use `match`; `is_some_and` is fine.
- GPU tests run on the RTX 5090. Don't launch the **fullscreen** app from automation —
  use a short `--windowed` `Start-Process` + kill; quote paths with spaces.
  Desktop is currently in **HDR mode** (so the app uses the fp16 scRGB surface, and GDI
  screen capture is broken — see the capture caveat).
- `D:\Media\Pictures` is the real corpus (use `-r`); `D:\Media\Pictures\test-images`
  has the per-format corpus **plus wide-gamut/HDR test images** (`WideGamut-*-DisplayP3*.jpg/.avif`,
  `*-HDR.avif`, and `-sRGB` twins for A/B).
