# NS0 keystone — `AppCore` ownership-inversion execution brief

> **Purpose.** Prime a fresh session to do the *one-pass* ownership inversion that
> finishes NS0 (ADR-021). Written 2026-06-30 on branch `swiftui` with full context of
> the seams already landed. Read `macos-native-ui-plan.md` §NS0 first; this is the
> execution detail under it. **Nothing here changes behavior on purpose** — it's a
> pure refactor gated on a manual smoke (owner is smoke-testing in parallel).

## 0. TL;DR of the move

`pb-app/src/main.rs` (7,120 lines) is a winit god-object: one `struct App` (73 fields)
that owns orchestration state *and* threads `event_loop: &ActiveEventLoop` through nearly
every method so those methods can `exit()`, `set_control_flow()`, open windows, and
`request_redraw()` inline. The inversion:

1. **Replace the `event_loop: &ActiveEventLoop` parameter with an effect sink.** Every
   `event_loop.*` / `window.*` / native-menu / rfd / clipboard call becomes
   `effects.push(CoreEffect::…)`. Methods stop touching winit.
2. **Split `App` into `AppCore` (orchestration, in `pb-app-core`) + `WinitShell`
   (winit/egui/muda/rfd/renderer surface, in `pb-app`).** `AppCore::handle(CoreEvent)
   -> Vec<CoreEffect>`; the shell translates real winit events → `CoreEvent`, drains the
   returned effects → real winit/native calls.
3. **`AppCore` owns `Box<dyn Renderer>`** (the plan's preference) once the shell has
   created the surface; the shell keeps the `Arc<Window>` and the wgpu surface lifecycle.

The `CoreEvent` / `CoreEffect` / `MenuState` / `Modifiers` / `KeyResolution` vocabulary is
**already defined** in `crates/pb-app-core/src/contract.rs` — this step wires it to the
live loop and fills in the deferred payloads.

## 1. What's already landed (don't re-do)

- `pb-app-core` crate (`toml`-only; **no winit/egui/wgpu/muda/rfd/objc**). Modules:
  `action`, `pb_key`, `keymap`, `slideshow`, `config`, `contract`, `timing`. The winit
  shell re-exports them: `use pb_app_core::{action, contract, keymap, pb_key, slideshow,
  timing};` so `crate::action::*` etc. still resolve in shell submodules.
- `PbKey` + `pb_app::pb_key_winit::from_winit` adapter; `KeyChord` is winit-free.
- **`resolve_key_down`** (contract.rs) already routes KeyDown by `ActionKind` in the live
  `WindowEvent::KeyboardInput` arm; the 4 modifier bools are unified into
  `contract::Modifiers` (field `App.mods`).
- **`MenuState`** already derived by a pure `menu_state_from(...)` and applied by a diffed
  `apply_menu_state()` (replaced the five `refresh_*` methods + caches). Field:
  `App.menu_state: Option<contract::MenuState>`.
- **Timing** in core: `slideshow` (dwell), `timing::advance_interval`,
  `timing::elapsed_since` (the shared tap-delay/repeat gate; used by both nav hold-to-fly
  and frame-step scrubbing).
- All behavior-preserving; full suite green (374), clippy/fmt clean; animated-image
  support from `main` merged and flowing through the seams.

## 2. The god-object inventory (categorize before moving)

`struct App` at `main.rs:469`; `struct Active { window: Arc<Window>, renderer:
WgpuRenderer }` at `main.rs:399`.

### Shell-owned → stay in `WinitShell` (must NOT enter `pb-app-core`)
- **Window + surface:** `Active.window: Arc<Window>`, the wgpu surface lifecycle,
  `scale_factor` (517), `last_edr_headroom` (604), `windowed` (470). Shell supplies these
  to core via `Started{…}` / `Resized{…}` events; core decides mode, shell applies via
  `SetWindowMode`.
- **Native menu (muda):** `menu` (658), `window_menu` (664), `native_fullscreen_item`
  (670), `save_rotation_item` (681), `cancel_scan_item` (683), `undo_item` (689),
  `view_checks` (692), `proxy_icon_path` (677), `menu_attached` (708). Core emits
  `SetMenuState(MenuState)`; the shell owns these handles and applies the diff (move the
  *apply* half of `apply_menu_state` shell-side; the pure `menu_state_from` stays in core).
- **Dialog window:** `dialog: Option<dialog::DialogWindow>` (711) — a *second winit window
  + egui*. Shell-owned. Core emits `ShowDialog/UpdateDialog/CloseDialog`; shell feeds
  outcomes back as `CoreEvent::{DialogResult, PasswordSubmitted, SettingsSubmitted,
  KeymapSubmitted, CancelDialog}`.
- **Renderer (the split):** `Active.renderer: WgpuRenderer` → move into `AppCore` as
  `Box<dyn pb_render::Renderer>` (WgpuRenderer already impls the trait). Only surface
  create/resize/EDR stays shell-side.

### `AppCore`-owned → move to `pb-app-core` (orchestration/state)
- **Nav/playlist:** `source` (473), `playlist` (474), `targets` (543), `last_nav` (541),
  `displayed_item` (532), `target_item` (534), `epoch` (530), `root` (503),
  `scan_root` (581), `recursive` (577).
- **Prefetch/decode/residency:** `pool` (523), `results` (525), `ring` (527),
  `ahead`/`behind` (551/552), `pending_uploads` (565), `preview_resident` (644),
  `upgrade_done` (648), `last_upgrade_set` (651), `full_requested_at` (655),
  `failed` (555), `deleted` (561).
- **Held-key/timing:** `held` (480), `last_present` (484), `frame_interval` (486),
  `hold_start` (488), `initial_delay` (490), `slideshow` (537).
- **View/geometry:** `fit` (492), `view` (494), `last_cursor` (498), `dragging` (501),
  `zoom_started`/`zoom_last` (607/608), `pan_started`/`pan_last` (609/610),
  `resize_settle_at` (595), `geometry_save_at` (599).
- **Overlays/HUD/feedback:** `hud` (505), `info` (507), `overlay_shown` (509),
  `overlay_item` (513), `current` (515), `toast` (586), `chip_*` (628–640),
  `pie_*`/`wait_started`/`decode_ewma`/`pie_drawn`/`pie_pushed` (618–623), `metrics` (519).
- **Metadata caches:** `meta_cache` (545), `exif_cache` (549), `rotations` (568).
- **Input state:** `mods` (575), `esc_guard_until` (590).
- **Config:** `keymap` (715), `settings` (719).
- **Scan/archive/launch:** `dir_scan` (729), `scan_gen` (732), `archive_load` (722),
  `archive_gen` (725), `pending_launch` (736), `password_archive` (739),
  `pending_drops` (584).
- **Menu-derived / undo / delete:** `menu_state` (698), `undo_stack` (686),
  `pending_delete` (702), `pending_confirm_delete` (705).
- **Animation:** `playback` (745), `anim_frame_shown_at` (748), `anim_decode` (752),
  `prepared` (756), `anim_gen` (759), `anim_hint_shown_for` (762),
  `framestep_started`/`framestep_last` (765/766).

## 3. The effect seam (the mechanical heart)

Every orchestration method currently ends in `…, event_loop: &ActiveEventLoop)`. Change
the signature to take an effect sink (`effects: &mut Vec<CoreEffect>`, or a small
`EffectSink` newtype with `push`/helpers) and convert each winit touch:

| Current inline call | Becomes |
|---|---|
| `event_loop.exit()` (4085, 4110) | `CoreEffect::Quit` |
| `event_loop.set_control_flow(WaitUntil(at))` (5477) | `CoreEffect::WakeAt(Some(at))` |
| `event_loop.set_control_flow(Wait)` (5478, 6333) | `CoreEffect::WakeAt(None)` |
| `window.request_redraw()` (×many) | `CoreEffect::RequestRender` |
| `window.set_title(…)` | `CoreEffect::SetTitle(String)` |
| `window.set_cursor(…)` | `CoreEffect::SetCursor(CursorKind)` |
| `window.set_fullscreen(…)` / `windowed` flips | `CoreEffect::SetWindowMode(WindowMode)` |
| muda `set_checked/enabled/text` | folded into `CoreEffect::SetMenuState(MenuState)` |
| `rfd::FileDialog…pick_*()` (2900–2921) | `CoreEffect::OpenFilePanel/OpenFolderPanel` |
| `clipboard::set_image*` (1174–1187) | `CoreEffect::WriteClipboard(ClipboardPayload)` |
| show/update/close `DialogWindow` | `CoreEffect::ShowDialog/UpdateDialog/CloseDialog` |
| `begin_exit` → `clear_session_state` + exit (4105) | core drops RAM caches, emits `HideWindow`+`Quit` |

The shell's loop becomes:
```
fn window_event(ev) {
    let core_event = translate(ev);           // §4
    let effects = self.core.handle(core_event);
    for e in effects { self.apply(e); }        // §5 — the ONLY winit/native calls
}
```
`draw()` stays a shell entry that calls `core.render(&mut *renderer)` (or core owns the
renderer and the shell just calls `core.draw()` after `pre_present_notify`).

## 4. Event mapping — winit → `CoreEvent`

The winit surface is `impl ApplicationHandler for App` (4717): `resumed` (4718),
`window_event` (4935), `about_to_wait` (5199). Map:

- `resumed` → build surface, `CoreEvent::Started{surface_size, scale, refresh_hz, edr}`.
- `WindowEvent::CloseRequested` (4942) → `Quit` request (core runs teardown).
- `Resized` (4944) / `ScaleFactorChanged` (4982) → `Resized{w,h,scale,edr}`.
- `Moved` (4993) → geometry-save debounce (a `CoreEvent::Moved` or fold into `Tick`).
- `RedrawRequested` (5000) → `Redraw`.
- `DroppedFile` (5005) → accumulate → `DroppedPaths(Vec<PathBuf>)`.
- `KeyboardInput` (5012) → `KeyDown{key: PbKey, mods, repeat}` / `KeyUp{key}` (already
  half-done via `resolve_key_down` + `pb_key_winit`). **Keep Escape's pre-seam special
  case** (it's handled before `resolve_key_down` today — preserve exactly).
- `ThemeChanged` (5081) → `ThemeChanged` (dialog re-theme only).
- `ModifiersChanged` (5087) → update `Modifiers` (core state).
- `Focused(false)` (5101) → `FocusLost` (the held-key release net — **must** clear `held`).
- `CursorMoved` (5116)/`CursorLeft` (5129)/`MouseInput` (5135) → pointer events.
- `PinchGesture` (5157)/`DoubleTapGesture` (5164)/`MouseWheel` (5174) → `Pinch/DoubleTap/Scroll`.
- `about_to_wait` (5199) → `Tick(now)` (drives the hold-loop, prefetch drain, wake calc).

## 5. Effect execution — `CoreEffect` → winit/native

Shell-side `apply(effect)` is the *only* place winit/muda/rfd/egui are touched. Notables:
- `SetMenuState(m)` → the diff-apply currently in `apply_menu_state` (move it here; keep
  the cached/no-op comparison against the last applied `MenuState`).
- `OpenFilePanel/OpenFolderPanel` → run the (blocking) rfd panel, then re-enter core with
  `CoreEvent::Open(LaunchInput)` — see trap in §6.
- `ShowDialog/UpdateDialog/CloseDialog` → own the `DialogWindow`; poll it each frame; on
  submit/cancel push the matching `CoreEvent` back into core next tick.
- `RequestRender` → `window.request_redraw()`; `WakeAt(Some/None)` →
  `set_control_flow(WaitUntil/Wait)`.

## 6. Behavior-preservation traps (each has bitten before or will)

1. **rfd panels are modal/blocking today.** `open_picker` (2885) runs `pick_files()`
   synchronously, *then* does `self.held.clear()` + `esc_guard_until = now+300ms`
   (2926–2927) to swallow the panel's stray Esc/Enter. In the effect model, that
   clear+guard must still happen around the panel — do it in the shell when it runs the
   panel effect, or have core set them as part of emitting the panel effect and again on
   the returned `Open`. **Getting this wrong = cancelling the picker quits the app.**
2. **⌘ / logo no-fall-through.** A Cmd-chord must not fall through to the bare-key action.
   `resolve_key_down` already encodes it via `Modifiers` — keep the modifier plumbed on
   every `KeyDown`, and keep Escape special-cased *before* the seam.
3. **Held-key semantics.** Ignore OS key-repeat (`repeat` flag), track *physical* keys in
   `held: HashMap<PbKey, Action>`, and the **focus-loss release net** (`Focused(false)`
   clears `held`). All three must survive the move — they are the hold-to-fly feature.
4. **Never decode/upload on the keypress frame.** The inversion must not move any decode
   or `copy_buffer_to_texture` onto the `KeyDown`/`Redraw` path — keep uploads in the
   prefetch drain on `Tick`. A keypress stays a rebind.
5. **The self-paced advance control flow** (`about_to_wait`, ~5268–5360) is the headline
   feel. Keep the exact order: initial-delay gate, `caught_up` present-vs-advance branch,
   `advance_interval` ramp, `elapsed_since` due-check. This is why the phase is smoke-gated.
6. **Two-window egui dialog.** The dialog is a real second winit window in the same loop —
   it consumes its own `WindowId`. The shell must route events by `WindowId` (already does
   at 4935 via `id`), so core never sees dialog-window events except as digested results.
7. **`epoch` / `*_gen` counters** (geometry epoch, `scan_gen`, `archive_gen`, `anim_gen`)
   gate stale async results. They must move *with* the state they guard, atomically.
8. **`clear_session_state`** (4118) is the no-trace teardown — keep it RAM-only, no
   flush-to-disk (privacy task #2). `begin_exit` (4105) = teardown then exit.

## 7. Recommended sequencing (stay compilable throughout)

Do it as one PR but in this internal order so each step builds+tests green:

1. **Introduce the effect sink type + drain loop** without moving the struct: change
   `dispatch_action` and the "leaf" effect methods to push `CoreEffect` into a
   `self.effects: Vec<CoreEffect>` and drain it at the end of `window_event`/`about_to_wait`.
   Start with the cheap leaves: `Quit`, `WakeAt`, `RequestRender`, `SetTitle`, `SetCursor`,
   `SetWindowMode`. Suite must stay green after each batch.
2. **Route `SetMenuState`** through the sink (the apply half already exists).
3. **Async-ify the dialogs and rfd panels** into effects + return-events (trap §6.1). This
   is the behavior-sensitive step — smoke here.
4. **Split the struct:** move the AppCore-owned fields (§2) into `pb_app_core::AppCore`,
   leave shell-owned in `WinitShell`. Move the renderer into `AppCore` as `Box<dyn
   Renderer>`. Add `AppCore::handle(CoreEvent) -> Vec<CoreEffect>` and reduce `App`/
   `WinitShell` to translate+apply.
5. **Add parity tests** in `pb-app-core` (§8), then run the manual smoke.

## 8. Tests to add before the Swift target

Pure `pb-app-core` unit tests (no winit): action dispatch → effect sequences; held-key
state machine (press/release/repeat-ignore/focus-loss-clear); wake scheduling
(`WakeAt` values across hold ramp); dialog/effect sequencing (open→result→close);
scan/archive cancellation via the `*_gen` counters; and `menu_state_from` parity across
representative states (already partly covered).

## 9. Validation gate (manual — owner runs)

Build the egui-Mac (and ideally Windows) shell and smoke:
- **hold-to-fly** on a big folder (space/→ held): ramps slow→fast, every photo shown, no
  stutter, degrades to preview under decode pressure.
- **frame-step scrub** `,`/`.` on an animated image; `P` play/pause; the play-hint flash.
- **dialogs:** Settings save, About, Confirm-delete, Message, Password (archive), the
  Scanning/Loading progress cards + Cancel.
- **file panels:** Open file / Open folder; cancel the panel → app stays up (trap §6.1).
- **menu:** checkmarks (scale/info/recursive/fullscreen/slideshow), Save Rotation / Stop
  Scanning / Undo enable+label, the shortcut editor round-trip.
- **teardown:** Esc quits with no disk writes (privacy no-trace test still green).

## 10. Rollback

Each sequencing step (§7) is its own commit and independently green — if the struct split
(step 4) gets messy, the effect-sink steps (1–3) are still a net win and shippable, and the
egui-Mac beta keeps working regardless. Stop at the last green step; don't force the whole
inversion if the smoke reveals a feel regression.

## 11. Key anchors (main.rs, 7,120 lines)

- `struct Active` 399 · `struct App` 469 · `impl ApplicationHandler` 4717
  (`resumed` 4718, `window_event` 4935, `about_to_wait` 5199)
- `dispatch_menu` 2823 · `dispatch_action` 2831 · `open_picker` 2885 · `rebuild_playlist` 2938
- `begin_exit` 4105 · `clear_session_state` 4118
- self-paced advance block ~5268–5360 · frame-step tick ~4655–4665
- control-flow: `exit` 4085/4110 · `set_control_flow` 5477/5478/6333
- contract vocabulary: `crates/pb-app-core/src/contract.rs`
- landed seams: `pb-app-core/src/{pb_key,keymap,slideshow,timing,contract}.rs`,
  `pb-app/src/pb_key_winit.rs`
