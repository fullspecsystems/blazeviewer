# NS0 Step 5 — Behavior Inversion (`AppCore::handle` + thin shell) — Execution Plan

**Status:** proposed, for review. **Branch:** `swiftui` (worktree `photoblaze-wt1`).
**Prereq done:** 5.1–5.4 (the bulk of the *state* inversion). **This plan covers 5.5 + 5.6.**

Read the companion `ns0-appcore-inversion-brief.md` first for the effect-seam (4a–4e) and
the field-group map. This document is the concrete, sequenced plan to finish NS0.

---

## 0. How to use this

Execute top-to-bottom. **Every numbered step ends green** (`cargo test --workspace`,
`cargo clippy --all-targets -- -D warnings`, `cargo fmt --all`) and is its own commit.
Smoke the hot path (zoom/pan/rotate/hold-to-fly/nav/info-panel/`--metrics`) at the ★ marks.
The recipe for every "move X into AppCore" is the proven one from 5.3: **migrate the
pb-app-local *types* first, then relocate fields/methods, then redirect refs** (watch the
three recurring hazards: prefix over-matches → placeholder swap; multiline `self\n.field` →
compiler catches; non-`self` refs like `app.field` / `module::fn`).

---

## 1. Corrected status — the state inversion is ~80%, not 100%

5.1–5.4 moved the *big* groups into `AppCore` (timing, view/geometry, caches, decode/
prefetch/residency, metrics, nav/playlist, HUD state + compositor, renderer). But the
method-graph analysis surfaced **state groups still on `App`** that must move before the
methods can:

| still-on-App state | fields | type migration needed |
|---|---|---|
| **Display** | `scale_factor` | none (f32) |
| **Prefetch-upgrade** | `upgrade_done`, `last_upgrade_set`, `full_requested_at` | none (std) |
| **Live Photo cache** | `live_motion_cache` | none (std) |
| **Drops** | `pending_drops` | none (std) |
| **Undo** | `undo_stack`, `undo_item` | `UndoAction` (main.rs:272 enum) → pb-app-core |
| **Animation** | `playback`, `anim_*`, `prepared`, `framestep_*`, `live_revert_at`, `live_audio` | `Playback` (animation.rs), `Prepared` (main.rs:704), `animation.rs` module |
| **Config** | `keymap`, `settings` | `Keymap` already in core; **`Settings`** (settings.rs, 26 uses) → pb-app-core |

Genuinely **shell-owned** (stay on `App`): `window`, `windowed`, `dialog`, `effects`,
`pending_dialog`, the native menu handles (`menu`, `window_menu`, `native_fullscreen_item`,
`proxy_icon_path`, `save_rotation_item`, `cancel_scan_item`, `menu_attached`), `menu_state`,
`last_edr_headroom`. Plus the **scan/archive/launch** group (`dir_scan`, `scan_gen`,
`archive_load`, `archive_gen`, `pending_launch`, `password_archive`, `pending_delete`,
`pending_confirm_delete`) — deferred to 5.6 (coupled to the dialog flow).

---

## 2. Target end-state architecture

```
winit shell (pb-app)                         AppCore (pb-app-core)
────────────────────                         ─────────────────────
ApplicationHandler::window_event  ──translate──▶  handle(CoreEvent) ──▶ dispatch to
ApplicationHandler::about_to_wait ──translate──▶     orchestration methods (impl AppCore)
ApplicationHandler::resumed       ── Started ──▶     which mutate self.* and push
                                                     self.effects: Vec<CoreEffect>
drain_effects(event_loop) ◀── reads self.core.effects ── (window/menu/dialog/clipboard ops)
```

- **The shell shrinks to:** create the window (`resumed`), translate every winit event into
  a `CoreEvent`, call `self.core.handle(ev)`, then `self.drain_effects(event_loop)`. Plus the
  effect *executors* (native window/menu/dialog/clipboard ops) — the only place winit/muda/
  rfd/objc2 is touched. The egui `dialog` window + `dialog_event` stay shell (its own window).
- **`AppCore::handle(CoreEvent)`** is the single entry point. It owns the key→action
  resolution and calls `dispatch_action` (already the central `match action` at main.rs:2741).

### Method census (impl App, 156 methods)
- **88 pure-core** — touch only `self.core.*` + `self.effects`; move to `impl AppCore` once
  their *types* are in pb-app-core.
- **~18 window/scale_factor touchers** — most are HUD-build methods needing only
  `scale_factor` (→ moves to core, Phase A) or window *size* (→ read `self.core.fit`, which
  already holds surface w/h, Phase B). ~6 are genuine native-window ops that **stay shell**
  (native fullscreen, EDR reconfigure, geometry capture, proxy icon, `begin_exit`,
  `drain_effects`, `apply_window_mode`).
- **37 dialog/menu/event_loop-coupled** — split between "push an effect and become core" and
  "stay shell" (the scan/archive/dialog flow = 5.6).

---

## 3. Design decisions (please confirm during review)

1. **`effects` moves into `AppCore`.** Methods on `impl AppCore` push to `self.effects`;
   the shell drains `self.core.effects` in `drain_effects`. (Alternative — `handle(&mut self,
   ev, sink: &mut Vec<CoreEffect>)` — threads a param through every method; rejected as
   noisier. Moving the field is consistent with everything else.)
2. **`handle` signature:** `pub fn handle(&mut self, ev: CoreEvent)` on `impl AppCore`,
   returning nothing (effects accumulate on `self.effects`). The shell always follows a
   `handle` call with `drain_effects`.
3. **`Tick` carries `Instant`** (already in the variant) — `AppCore` stays clock-injectable
   (no `Instant::now()` inside core; the shell stamps it). Keeps core deterministically
   testable — a *big* payoff: `handle`-driven nav/timing become unit-testable without winit.
4. **Window-size decoupling:** the 3 hit-rect methods (`open_button_rect`, `play_hint_rect`,
   `push_chip`) read `self.window.inner_size()` today; switch them to `self.core.fit`
   (`FitBox { max_width, max_height }` is the surface size). Removes their last window dep.
5. **`Settings` migration:** move `settings.rs` into pb-app-core (config I/O already lives
   there — `config_dir`, keymap load/save; the brief lists Config as AppCore-owned). Verify
   it pulls no winit/egui (expected: it's `toml` + `serde`).
6. **What does NOT move in 5.5:** the scan/archive/launch/delete flow and its dialog
   choreography (that's 5.6). 5.5's `handle` will, for those actions, push the *existing*
   effects / emit the NS-later CoreEvents (`Open`, `SettingsSubmitted`, `PasswordSubmitted`,
   `DialogResult`) as stubs the shell still services until 5.6 inverts them.

---

## 4. Phased execution plan

### Phase A — finish the pure-core STATE moves (mechanical, proven recipe)
Each is a `git`-committed increment; no behavior change. Order chosen so later method moves
find their fields already in `core`.

- **A1. `scale_factor` → AppCore.** Field move; redirect ~19 refs; the 2 shell set-sites
  (`resumed` L5281, resize L5451) become `self.core.scale_factor = …`. Unblocks ~10 HUD-build
  methods. *(no type migration)*
- **A2. Prefetch-upgrade trio → AppCore** (`upgrade_done`, `last_upgrade_set`,
  `full_requested_at`). *(no type migration)*
- **A3. `live_motion_cache` + `pending_drops` → AppCore.** *(no type migration)*
- **A4. Undo → AppCore.** Migrate `UndoAction` (main.rs:272) → `pb_app_core` first, then
  move `undo_stack`, `undo_item`.
- **A5. Animation → AppCore.** Migrate `animation.rs` (Playback + friends) + `Prepared`
  (main.rs:704) into pb-app-core (verify winit/egui-free), then move `playback`, `anim_*`,
  `prepared`, `framestep_*`, `live_revert_at`, `live_audio`. *(largest of Phase A; `live_audio`
  may be platform — if it wraps a macOS/OS audio handle, keep the handle shell-side behind an
  effect and move only the pairing state.)*
- **A6. Config → AppCore.** Migrate `settings.rs` (`Settings`) into pb-app-core, then move
  `keymap` + `settings`. ★ **smoke** (settings + keymap touch a lot).

**Exit A:** every non-flow state field is in `AppCore`; `App` holds only window/dialog/menu/
effects/flow state. ~6 commits.

### Phase B — decouple the window-size hit-rect methods
- **B1.** Rewrite `open_button_rect`, `play_hint_rect`, `push_chip` to use `self.core.fit`
  instead of `self.window.inner_size()`. Small; makes them pure-core. 1 commit.

### Phase C — move the pure-core methods to `impl AppCore`
Now ~110 methods are pure-core. Move them in **concern batches**, green per batch. The move
is physical (main.rs `impl App` → app_core.rs `impl AppCore`); each method must reference only
types available in pb-app-core (Phase A migrations + pb-core/pb-decode/pb-render/pb-source/
pb-hud). Suggested batches (each ~10–25 methods):

- **C1. Nav/prefetch/residency** — `advance`, `nav_press`, `request_prefetch`, `drain_results`,
  eviction/targets helpers. (Uses pb-core `open::{LaunchInput,Source,Cursor}` — already in core.)
- **C2. View — zoom/pan/rotate/fit** — `zoom_held`, gesture handlers, `set_scale_mode`,
  `view_for`. ★ smoke.
- **C3. HUD build** — `show_overlay`, `push_toast`/`push_pie`/`push_chip`, `build_play_hint`,
  `exif_rows`, `open_panel_bitmap`, `show_toast_icon` (pb-hud types; `scale_factor` now in core).
- **C4. Animation playback** — `toggle_playback`/frame-step/tick methods.
- **C5. Undo + misc** — `undo`, `push_undo`, small helpers.

**Note:** `dispatch_action` (2741) moves in whichever batch leaves it pure; its arms that do
*flow* things (open dialog, scan, delete) push effects / emit CoreEvents and are finalized in
5.6. ★ smoke after C. ~5 commits.

### Phase D — `AppCore::handle(CoreEvent)` + thin the shell
- **D1. Move `effects` into AppCore** (field move; `drain_effects` reads `self.core.effects`).
- **D2. Add `pub fn handle(&mut self, ev: CoreEvent)`** on `impl AppCore`, matching each
  variant to the moved methods: `KeyDown`→resolve via keymap→`dispatch_action` (+ held-key set
  update); `KeyUp`/`FocusLost`→held-key release; `Tick`→advance/slideshow eval; `Resized`→fit/
  renderer-resize effect; `PointerMoved`/`Scroll`/`Pinch`/`DoubleTap`→view methods; `Redraw`→
  frame decision; `MenuAction(a)`→`dispatch_action(a)`; `DroppedPaths`→core; `KeymapSubmitted`/
  `CancelDialog`→core. Unit-test `handle` directly (clock injected) — first real core tests.
- **D3. Rewrite the shell event handlers as translators.** `window_event` (main window arm)
  and `about_to_wait` build the `CoreEvent`, call `self.core.handle(ev)`, then
  `self.drain_effects(event_loop)`. `resumed` stays (creates window + renderer, then
  `handle(Started/Resized)`). The egui `dialog` window arm stays shell. ★ **smoke thoroughly.**
  ~3 commits.

**Exit D = 5.5 done:** the winit shell is a translator + effect executor; `AppCore` owns
behavior. NS1's Swift bridge can now call `handle` with `NSEvent`-derived `CoreEvent`s.

### Phase E — 5.6: invert the scan/archive/dialog/launch flow + native fullscreen
The remaining 37 coupled methods + the 4d blocker (`begin_archive_open` reaches into
`DialogWindow::become_loading`). Migrate `Resolved` (main.rs:6332) into core;
wire the NS-later CoreEvents/Effects: `Open(LaunchInput)`, `SettingsSubmitted(Settings)`,
`PasswordSubmitted(String)`, `DialogResult(..)`, and `ShowDialog`/`Progress`/`Error` effects.
Dialog *results* become `CoreEvent`s; dialog *opens* become `CoreEffect`s (the shell owns the
egui `DialogWindow`). Native fullscreen (`toggle_native_fullscreen`) stays a shell effect
executor driven by `SetWindowMode`. ★ smoke. ~4–6 commits.

**Exit E = NS0 complete.**

---

## 5. Risks & mitigations

- **Cross-crate method move exposes hidden pb-app type deps.** Mitigation: the per-batch
  recipe migrates types first; if a method drags an unexpected shell type, split it (keep a
  thin shell wrapper that pushes an effect, move the pure part). The compiler enumerates the
  gaps immediately.
- **Borrow conflicts when a moved method calls another + holds a field borrow.** Low risk —
  proven across 5.1–5.4 (disjoint `self.core.*` field borrows; methods never held a borrow
  across an `&mut self` call). Same-crate `impl AppCore` methods behave identically.
- **`dispatch_action` arms that do shell work.** They already push effects post-4a–4e; any
  residual direct shell call becomes an effect or a deferred-to-5.6 `CoreEvent` stub. Audit
  the `match action` at 2741 before C.
- **`live_audio` / platform handles in the animation group.** If it owns an OS audio object,
  keep the object shell-side behind a `PlayAudio`/`StopAudio` effect; move only pairing state.
- **`Settings` migration surface (26 uses).** It's config I/O (toml/serde) — should be clean,
  but verify no winit/egui and that `Settings::load/save` paths still resolve via `config_dir`.
- **Two `KeyboardInput` sites** (dialog window vs main). Only the main-window arm becomes a
  `KeyDown` translation; the dialog arm stays egui.

---

## 6. Effort shape (for scheduling one session)
- Phase A: ~6 small commits (mechanical field/type moves).
- Phase B: 1 small commit.
- Phase C: ~5 batch commits (the physical method relocation — the bulk).
- Phase D: ~3 commits (the keystone: `handle` + translators).
- Phase E (5.6): ~4–6 commits (flow inversion + native fullscreen).

Total ~19–21 green commits. A/B/D/E each have a smoke checkpoint. **Recommend executing
A→B→C→D in one focused session (that completes 5.5), then E (5.6) in a second**, but the
plan supports one continuous run.

## 7. Open questions for review
1. OK to **move `effects` and `Settings` into pb-app-core** (§3.1, §3.5)?
2. OK to fold the remaining **state groups (Phase A)** in as "5.4c-style" moves before the
   method inversion, rather than calling them separate roadmap items?
3. Split **5.5 (A–D) and 5.6 (E) across two sessions**, or one continuous run?
4. Any method you want to keep shell-side regardless (e.g. if you're already eyeing a native
   SwiftUI reimplementation for it in NS2/NS3)?
