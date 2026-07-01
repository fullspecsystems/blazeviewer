# NS0 Step 5 — Behavior Inversion (`AppCore::handle` + thin shell) — Execution Plan

**Status:** revised after codex review (r1). **Branch:** `swiftui`. **Prereq done:** 5.1–5.4
(state) + Phase 0 (contract cleanup) + Phase A (remaining state moves) + **Phase B (the ~99
pure-core method moves onto `impl AppCore`) DONE (2026-07-01)** — all green (383 tests, clippy
`-D warnings`, fmt), 8 commits, not yet fast-forwarded onto `main`. **RESUME by effect-ifying
`dispatch_action`'s 10 remaining shell arms** (see `../current-status.md` §NS0 ▶ Resume for the
exact list + order), then Phase C (`handle`), then Phase E (=5.6, the dialog/scan/archive flow).
The scanning/launching flags, live-audio effects, and delete-state moves that Phase B needed are
all in place. See `../current-status.md` §NS0 for the full done-vs-remaining summary.

Read `ns0-appcore-inversion-brief.md` first for the effect-seam (4a–4e) and the field-group
map. **r1 changes (from codex review):** a new **Phase 0 contract-cleanup** runs first —
`effects`→core, a `now` clock field, a `Viewport` struct, `SetWake(Option<Instant>)`,
live-audio effects, and the `Settings` migration with its real deps — because several method
moves fail to compile or leak the boundary if attempted before their inputs are core-owned
and clock-injected. The scan/archive/dialog/delete/**drop** flow stays **explicitly
shell-side until Phase E**.

---

## 0. How to use this

Execute top-to-bottom. **Every numbered step ends green** (`cargo test --workspace`,
`cargo clippy --all-targets -- -D warnings`, `cargo fmt --all`) and is its own commit. Smoke
the hot path (zoom/pan/rotate/hold-to-fly/nav/info-panel/`--metrics`) at the ★ marks. The
recipe for every "move X into AppCore" is the proven one from 5.3: **migrate the pb-app-local
*types* first, then relocate fields/methods, then redirect refs** (hazards: prefix
over-matches → placeholder swap; multiline `self\n.field` → compiler catches; non-`self` refs
like `app.field` / `module::fn`).

---

## 1. Corrected status — the state inversion is ~80%, not 100%

5.1–5.4 moved the *big* groups into `AppCore`. State groups **still on `App`** that must move
before their methods can:

| still-on-App state | fields | type migration |
|---|---|---|
| **Viewport** | `scale_factor` (+ the window-size reads scattered in methods) | none — new `Viewport` struct (§3.4) |
| **Prefetch-upgrade** | `upgrade_done`, `last_upgrade_set`, `full_requested_at` | none |
| **Live Photo cache** | `live_motion_cache` | none |
| **Drops** | `pending_drops` | none (but stays shell-*routed* until E — see §3.6) |
| **Undo** | `undo_stack`, `undo_item` | `UndoAction` (main.rs:272) → core |
| **Animation** | `playback`, `anim_*`, `prepared`, `framestep_*`, `live_revert_at` | `Playback` (animation.rs), `Prepared` (main.rs:704) → core |
| **Live audio** | *(handle stays shell)* — see §3.5 | represent as effects, not a moved field |
| **Config** | `keymap`, `settings` | `Keymap` already in core; **`Settings`** → core (needs `serde` dep, §3.7) |
| **Effects** | `effects: Vec<CoreEffect>` | none — **moves in Phase 0.1, before any method move** |

Genuinely **shell-owned** (stay on `App`): `window`, `windowed`, `dialog`, `pending_dialog`,
the native menu handles (`menu`, `window_menu`, `native_fullscreen_item`, `proxy_icon_path`,
`save_rotation_item`, `cancel_scan_item`, `menu_attached`), `menu_state`, `last_edr_headroom`,
the `live_audio` handle (§3.5). Plus the **scan/archive/launch/delete/drop** flow (`dir_scan`,
`scan_gen`, `archive_load`, `archive_gen`, `pending_launch`, `password_archive`,
`pending_delete`, `pending_confirm_delete`, `pending_drops`-routing) — deferred to **5.6**.

---

## 2. Target end-state architecture

```
winit shell (pb-app)                         AppCore (pb-app-core)
────────────────────                         ─────────────────────
window_event  ──stamp now, translate──▶  handle(CoreEvent) ──▶ dispatch to
about_to_wait ──stamp now, translate──▶      orchestration methods (impl AppCore)
resumed       ──── Started/Resized ───▶      mutate self.*, read self.now/self.viewport,
                                             push self.effects: Vec<CoreEffect>
drain_effects(event_loop) ◀── reads self.core.effects ── (window/menu/dialog/clipboard/
                                                          live-audio/wake ops)
```

- **The shell shrinks to:** create window (`resumed`); at each event-loop entry stamp
  `self.core.now`; translate the winit event into a `CoreEvent`; call `self.core.handle(ev)`;
  then `self.drain_effects(event_loop)`. Plus the effect *executors* (native window/menu/
  dialog/clipboard/live-audio/wake) — the only place winit/muda/rfd/objc2 is touched. The egui
  `dialog` window + `dialog_event` stay shell.
- **`AppCore::handle(CoreEvent)`** owns key→action resolution and calls `dispatch_action`
  (central `match action` at main.rs:2741). In **5.5 it handles the non-flow events only**;
  flow events stay shell-routed until 5.6 (§3.6).

### Method census (impl App, 156 methods)
88 pure-core · ~18 window/`scale_factor` touchers (→ `Viewport`, §3.4) · 37 dialog/menu/
flow-coupled (→ mostly 5.6).

---

## 3. Design decisions (locked by review)

1. **`effects` moves into `AppCore` in Phase 0.1 — *before* any method move.** Methods on
   `impl AppCore` push `self.effects`; the shell drains `self.core.effects`. (Fixes the draft's
   self-contradiction: do **not** defer this field to the end.)
2. **`handle` signature:** `pub fn handle(&mut self, ev: CoreEvent)`; effects accumulate on
   `self.effects`; the shell always follows with `drain_effects`.
3. **Clock injection via a `now` field (codex #1).** Add `AppCore::now: Instant`. The shell
   stamps `self.core.now` at each event-loop entry (and `Tick` carries the same instant).
   **Every `Instant::now()` inside a core-bound method becomes `self.now`** — the flagged sites
   (`nav_press` 4455, toast/play-hint 3806/3830, `frame_step_press` 4797, animation present,
   chip timing, …) are converted in Phase 0.3 for still-on-App methods and stay converted
   through the move. Core never calls `Instant::now()` → deterministic unit tests set `now`.
4. **`Viewport { width: u32, height: u32, scale_factor: f32 }` on `AppCore` (codex #4).**
   Replaces the loose `scale_factor` field and **all** `window.inner_size()` / `scale_factor`
   reads in core-bound methods (audit catches `show_overlay`'s help-height read at 3696, not
   just the 3 hit-rect methods). The shell updates it on `Resized` / `ScaleFactorChanged` /
   `resumed`. Always-present (not `Option`), independent of the decode-fit `FitBox`.
5. **Live audio stays a shell handle; audio is effects (codex #6).** `live_audio` is an ObjC
   `AVAudioPlayer` wrapper (live_audio.rs:34). Move only the *playback/pairing state* into core;
   add `CoreEffect::{StartLiveAudio(PathBuf), PauseLiveAudio, ResumeLiveAudio, StopLiveAudio}`;
   the shell owns the `LiveAudio` object and executes those in `drain_effects`.
6. **Wake semantics: `CoreEffect::SetWake(Option<Instant>)` (codex #7).** `Some(at)` →
   `ControlFlow::WaitUntil(at)`, `None` → `Wait`. Replaces the un-expressible `WakeAt(Instant)`
   (couldn't say "go idle"). `handle`/`about_to_wait` emit it.
7. **`Settings` migration is concrete (codex #5).** `settings.rs` uses `serde` derives +
   `crate::slideshow` + `pb_app_core::config_dir()`. Add
   `serde = { version = "1", features = ["derive"] }` to pb-app-core; move the module in;
   `crate::slideshow` resolves unchanged (slideshow is already in core); `pb_app_core::
   config_dir()` → `crate::config_dir()`; re-export (`pub use settings::Settings` + a crate-root
   `pub use pb_app_core::settings` in the shell) so `dialog.rs` / `main.rs` don't churn.
8. **Flow stays shell-side until Phase E (codex #3).** In 5.5, `handle` covers input/timing/
   view/nav + non-flow menu actions. `DroppedPaths`, dialog/picker completion (`finish_picker`
   at 4336), archive open (`begin_archive_open`→`DialogWindow` at 1848), scan/delete results
   remain shell-routed. We do **not** claim NS1 can drive all behavior through `handle` until E
   makes those flow events real.

---

## 4. Phased execution plan

### Phase 0 — Contract cleanup (do first; unblocks everything)
- **0.1 `effects` → AppCore.** Field move; `drain_effects` reads `self.core.effects`. *(no
  behavior change; must precede Phase C.)*
- **0.2 Wake + live-audio effects.** Add `CoreEffect::SetWake(Option<Instant>)` and
  `Start/Pause/Resume/StopLiveAudio`; route `about_to_wait`'s Wait/WaitUntil through `SetWake`.
- **0.3 Clock field.** Add `AppCore::now: Instant`; shell stamps it at each event entry;
  convert `Instant::now()` in core-bound methods → `self.core.now` (still on App) / `self.now`
  (after move). ★ smoke (timing-sensitive).
- **0.4 `Viewport` struct.** Introduce on `AppCore`; shell updates on resize/scale-change;
  replace every `window.inner_size()` + `scale_factor` read in core-bound methods. Removes the
  loose `scale_factor` field. ★ smoke (overlay/HUD sizing).
- **0.5 Config migration.** Add `serde` to pb-app-core; move `settings.rs` in (§3.7); move
  `keymap` + `settings` fields to core. ★ smoke (settings + keymap).

### Phase A — remaining pure-core STATE moves (mechanical, proven recipe)
- **A1** Prefetch-upgrade trio (`upgrade_done`, `last_upgrade_set`, `full_requested_at`).
- **A2** `live_motion_cache` (the pairing cache; `pending_drops` stays shell-routed per §3.6 —
  move the field only if its readers are all core; otherwise leave until E).
- **A3** Undo — migrate `UndoAction`, then `undo_stack` + `undo_item`.
- **A4** Animation — migrate `animation.rs` (`Playback`) + `Prepared`; move `playback`,
  `anim_*`, `prepared`, `framestep_*`, `live_revert_at`. Audio via effects (§3.5). ★ smoke.

**Exit 0+A:** every non-flow state field + input (clock, viewport) is core-owned.

### Phase B — move the pure-core methods to `impl AppCore` (batches, green each)
~110 pure-core methods, moved by concern. Each references only core-available types (Phase 0/A
migrations + pb-core/pb-decode/pb-render/pb-source/pb-hud):
- **B1 Nav/prefetch/residency** — `advance`, `nav_press`, `request_prefetch`, `drain_results`,
  eviction/targets helpers (pb-core `open::{LaunchInput,Source,Cursor}` already in core).
- **B2 View — zoom/pan/rotate/fit** — `zoom_held`, gestures, `set_scale_mode`, `view_for`. ★ smoke.
- **B3 HUD build** — `show_overlay`, `push_toast/pie/chip`, `build_play_hint`, `exif_rows`,
  `open_panel_bitmap`, `show_toast_icon` (pb-hud types; viewport in core).
- **B4 Animation playback** — toggle/frame-step/tick methods (audio via effects).
- **B5 Undo + small helpers.** `dispatch_action` moves in whichever batch leaves it pure; its
  *flow* arms push effects / defer to E. ★ smoke after B.

### Phase C — `AppCore::handle(CoreEvent)` for non-flow events + thin the shell (the keystone)
- **C1** Add `pub fn handle(&mut self, ev: CoreEvent)`: `KeyDown`→resolve→`dispatch_action` +
  held-key update; `KeyUp`/`FocusLost`→release; `Tick`→advance/slideshow (+ `SetWake`);
  `Resized`/`ScaleFactorChanged`→viewport + renderer-resize effect; `PointerMoved`/`Scroll`/
  `Pinch`/`DoubleTap`→view; `Redraw`→frame decision; `MenuAction(a)`→`dispatch_action` for
  **non-flow** actions. **Add core unit tests** (inject `now`; assert emitted effects) — the
  first real `handle` tests.
- **C2** Rewrite the shell handlers as translators: main-window `window_event` + `about_to_wait`
  stamp `now`, build a `CoreEvent`, call `handle`, then `drain_effects`. `resumed` stays. The
  egui `dialog` arm + all flow events stay shell-routed (§3.6). ★ **smoke thoroughly.**

**Exit C = 5.5 done:** the shell is a translator + effect executor for all non-flow behavior.

### Phase E — 5.6: invert the scan/archive/dialog/launch/delete/drop flow + native fullscreen
Migrate `Resolved` (main.rs:6332) into core. Untangle the entangled paths codex flagged:
`drain_effects` running pickers + immediate `finish_picker` (4336) → `OpenFilePanel`/
`OpenFolderPanel` effect returns a `CoreEvent::Open`/`Picked`; `begin_archive_open` mutating
`DialogWindow` (1848) → `ShowDialog`/`Progress` effects + `CoreEvent::PasswordSubmitted`/
`Open`. Wire the NS-later events (`Open(LaunchInput)`, `SettingsSubmitted(Settings)`,
`PasswordSubmitted(String)`, `DialogResult(..)`) and `DroppedPaths`→core. Native fullscreen
(`toggle_native_fullscreen`) stays a shell executor driven by `SetWindowMode`. ★ smoke.

**Exit E = NS0 complete.** Only then does NS1's Swift bridge drive *all* behavior via `handle`.

---

## 5. Risks & mitigations
- **Compile-order (was a real draft bug):** `effects` (0.1), clock (0.3), viewport (0.4),
  and type migrations must land *before* the methods that use them. Phase 0/A enforce this.
- **Cross-crate method move exposes hidden pb-app type deps.** Per-batch recipe migrates types
  first; if a method drags a shell type, split it (thin shell wrapper pushes an effect, move
  the pure part). Compiler enumerates gaps.
- **Flow entanglement (codex #3):** `finish_picker`/`begin_archive_open`/`DialogWindow` are
  intertwined; keep the whole scan/archive/dialog/delete/drop flow shell-routed until E rather
  than half-invert it in 5.5.
- **Borrow conflicts** — low risk; proven across 5.1–5.4 (disjoint `self.core.*` field borrows).
- **`Settings` surface (26 uses)** — config I/O only; verify no winit/egui and that load/save
  still resolve via `config_dir`.
- **`live_audio`** — never move the ObjC handle; only state + effects (codex #6).

## 6. Effort shape
Phase 0: ~5 commits · Phase A: ~4 · Phase B: ~5 · Phase C: ~2 (keystone) · Phase E (5.6):
~4–6. Total ~20–22 green commits; smoke at 0.3/0.4/0.5/A4/B2/B(end)/C2/E. **Recommend
0→A→B→C in one focused session (= 5.5 complete), E (5.6) in a second.**

## 7. Decisions locked by review (were open questions)
1. `effects` **and** `Settings` move into pb-app-core — **yes** (0.1, 0.5; `serde` added).
2. Remaining state groups fold in as Phase 0/A prep moves — **yes**.
3. 5.5 (0–C) and 5.6 (E) split across two sessions — **yes** (flow stays shell until E).
4. `live_audio` ObjC handle stays shell-side — **yes** (state moves, audio is effects).
