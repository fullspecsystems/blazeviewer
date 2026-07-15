# Task 61 — Windows toolbar (parity with macOS #55, Windows conventions)

**Rev 2, 2026-07-13.** Rev 1 (drafted as "86") + a Codex review, whose findings were each
verified against the code and are real. Key corrections vs Rev 1: this is **task 61** (the
existing Windows-toolbar task), whose old "hidden by default / no reserved strip" premise is
**superseded** by the owner's settled persistent/default-on decision; state moves to a
**separate `ToolbarState`** (not `MenuState`, which is `Copy + Eq` and re-syncs the native
menu); the counter derives from **`displayed_item`** (present-truth), not the nav target; the
top strip **reuses the existing `set_content_top_inset` seam** (not a `fit_rect` change); the
"never occludes" guarantee is **Fit-only** (Fill/zoom/pan ride under, per the #59 spike); nav
buttons need the **pointer hold-to-blaze lifecycle**, not one-shot `MenuAction`; icons must
vendor **both FA families** (the `glyph!` macro requires it); and the plan is reordered
**test-first**. No unverified "zero cost" claim — a windowed-mode A/B benchmark gates it.

## Task-tracker reconciliation (do this first)

- Task **55** (macOS `NSToolbar`) shipped 2026-07-06. Task **54** built the egui presenter
  seam. Task **59** was the top-inset (content-under-bar) spike. Task **61** is *this* — the
  winit/egui sibling of #55.
- **Task 61 in `tasks.json` (line 268) must be rewritten.** Its current description says
  "hidden by default… never persistent chrome that eats the fit-to-screen area… must fully
  hide (no reserved strip)." The owner settled the *opposite* on 2026-07-13: **docked,
  persistent, default-on, reserves a strip.** Rewrite 61's description/details/testStrategy to
  match this plan before implementation, and add subtasks per the phases below. **Canonical ID
  stays 61** — do not open a duplicate 86.

## Goal

A **docked, always-visible toolbar strip** directly under the native menu bar in windowed
mode, giving mouse users one-click access to high-frequency actions (nav, random, folder-jump,
play/pause, slideshow, rotate, delete, panels, fullscreen) plus a photo counter. It reuses the
shared `Action` vocabulary, the existing egui overlay, and the existing top-inset seam — no new
command path, no second window. It is hidden in fullscreen speed mode (which already drops all
chrome) and can be turned off entirely via a Settings toggle. It is discoverability scaffolding
for mouse users; the keyboard already does everything faster.

## Settled decisions (owner, 2026-07-13; Codex-informed)

- **Placement: docked top strip** under the native menu bar (IrfanView/XnView/ACDSee idiom),
  **not** floating. Reserves its height via the renderer inset; fullscreen speed mode reserves
  nothing.
- **Visibility: persistent** in windowed mode. No auto-hide/idle/fade.
- **Alignment:** left-aligned groups, right-aligned counter + fullscreen (Windows idiom).
- **Scope: quick-access subset**, not an everything-drawer. The native menu bar and right-click
  context menu carry the full vocabulary. Power features stay in menus.
- **We ship Delete** (`trash-can`); macOS keeps it palette-only.
- **Default on**, one toggle to disable (`show_toolbar`, default `true`).
- **State seam: a separate `ToolbarState`**, diffed against the last snapshot, dirtying egui
  only on change — **not** `MenuState` (see Rationale below).
- **Styling: `pb-ui` tokens + `Palette` + OS accent.** Flat icon-only buttons; hover fill;
  pressed sunken; active toggle = accent fill; play/pause = accent while actually playing.
- **v1 button set is frozen** until the perf + narrow-width measurements land. No Open File, no
  Save Rotation on the default strip.
- **Group collapse order under width pressure:** folder-nav first, then rotate, then random.
  Nav / play / slideshow / panels / counter / fullscreen never collapse.
- **Separators:** whitespace between groups (dialog style) + one subtle bottom edge dividing
  toolbar from content.
- **Counter is bare** (`idx / count`), no folder prefix.

## Why the plumbing is mostly already there

1. **egui already overlays the main window.** `crates/pb-app/src/egui_overlay.rs` renders
   retained-mode egui (Help, Inspector, tree) into an offscreen `Rgba8UnormSrgb` texture that
   `pb-render` composites into the fp16 scRGB intermediate, sharing the main device
   (`egui_overlay.rs:12-16`). The toolbar is one more panel there — no second window (that's
   `dialog.rs`).
2. **The action vocabulary is shared.** Every button dispatches the same `Action`
   (`crates/pb-app-core/src/action.rs:28`) the keymap and menu use, via
   `CoreEvent::MenuAction(Action)` (`contract.rs:307`) — **except** hold-to-blaze nav, which uses
   the pointer path (below). No parallel command path.
3. **The top-inset seam exists.** `Renderer::set_content_top_inset(px)`
   (`crates/pb-render/src/gpu.rs:1976`) already reserves a top band; the **Linux shell already
   drives it** for its egui menu bar (`main.rs:1910`, `menu_inset_px()`; `panels_ui::MENU_BAR_H`).
   We reuse it — no `fit_rect` change.
4. **OS accent already flows** to `pb_ui::set_accent` via `crates/pb-app/src/accent.rs` (WinRT
   `UISettings`, live-tracked). Accent theming + live OS-accent updates are free.

## Layout

Left-aligned groups (whitespace-separated), counter + fullscreen pinned right. Icon-only,
~`CONTROL_H` (32px) square buttons.

```
┌────────────────────────────────────────────────────────────────────────────────┐
│ ‹ ›   ⇄ shuffle   « »    ▶/⏸  ⧉images 4s    ↺ ↻  🗑          ⓘ  ▤  🗁tree      2/120  ⤢ │
└────────────────────────────────────────────────────────────────────────────────┘
  nav     random    folder    playback           image ops         panels        count full
```

| Group | Button | Action (`action.rs`) | FA glyph | Behavior |
|---|---|---|---|---|
| Nav | Prev / Next | `Prev` / `Next` | `chevron-left` / `chevron-right` | **pointer hold-to-blaze** |
| Random | Rand prev / Random | `RandomPrev` / `Random` | `shuffle` (mirrored) / `shuffle` | **pointer hold-to-blaze** |
| Folder | Prev / Next folder | `PrevFolder` / `NextFolder` | `angles-left` / `angles-right` | one-shot `MenuAction` |
| Playback | Play / Pause | `PlayPause` | `play` / `pause` | **accent while `motion_playing()`**; disabled when `!current_has_motion()` |
| Playback | Slideshow | `SlideshowToggle` | `images` + interval text | **accent while running** |
| Image | Rotate CCW / CW | `RotateCcw` / `RotateCw` | `rotate-left` / `rotate-right` | one-shot |
| Image | **Delete** | `Delete` | `trash-can` | gated by `reveal_enabled` (needs on-disk file) |
| Panels | Info line | `Info` | `circle-info` | toggle → accent when on |
| Panels | Details / EXIF | `FullExif` | `table-list` | toggle → accent when on |
| Panels | Folder tree | `FolderTree` | `folder-tree` | toggle → accent when on |
| Right | Counter | — | — | `displayed_idx+1 / count`, from `ToolbarState`; hidden until first present |
| Right | Fullscreen | `Fullscreen` | `up-right-and-down-left-from-center` | one-shot |

**Deliberately omitted from v1** (in menus/context menu): Save Rotation, Compare/Pin, Copy,
Copy Path, Reveal, Describe, Show Text, Zoom, Scale modes, Open File/Folder, Settings, Open
Parent.

**Counter is a deliberate duplication.** The native Windows title bar already carries
`name (idx+1/n)` (`engine.rs:571` `title_for`), so the counter duplicates it — kept because the
title text is small and easy to miss, and the strip has the room. Bare, no folder prefix.

## Rationale: `ToolbarState`, not `MenuState`

`MenuState` (`contract.rs:132`) is `#[derive(Clone, Copy, PartialEq, Eq, …)]` — a semantic
snapshot the shell diffs to decide whether to **re-push the native muda menu** (`main.rs:2075`).
Its fields change rarely (a scale mode, a panel toggle). Putting the **current index, count,
`motion_playing`, and slideshow interval** on it would make it unequal on nearly every photo
transition, triggering a native-menu re-sync each nav — waste on the browse path.

Instead: a compact **`ToolbarState`** (a winit-shell snapshot, or a small `pb-app-core` struct)
carrying `displayed_index`, `count`, `motion_playing`, `has_motion`, `slideshow_interval`
(store a **`Duration`/deciseconds, not a formatted `String`**), `folder_tree_visible`, plus the
`MenuState` flags the toolbar reads (`reveal_enabled`, `info_basic`, `info_full`, `fullscreen`,
`slideshow`, `scale`). The shell compares it to the previous snapshot and **dirties egui only on
change**. Keep `MenuState` for the menu; derive both from the pure core state in one pass.

State sources (all in `crates/pb-app-core/src/app_core_impl.rs` unless noted):

- **Counter** — `displayed_item` (present-truth; changes only after successful presentation),
  **not** `playlist.current()` (which advances before the resident slot is ready, so the counter
  would lie during a ring miss). `count = playlist.len()` (`pb-core/playlist.rs`). Cold start:
  counter hidden until the first image is presented.
- **`motion_playing()`** (`:7753`) — covers animation, Live Photo, **and video**. Play accent.
- **`current_has_motion()`** (`:7739`) — video-aware; enables/disables the play button.
- **`folder_tree_visible`** — the "currently visible after `Tab` (TogglePanels) suppression"
  semantic (not merely "selected/open"). Pick this meaning explicitly and mirror the Inspector's
  own convention.
- **`slideshow_interval`** — `Duration`, formatted at draw time.

## Input lifecycle (nav hold-to-blaze)

Nav/random buttons are **not** one-shot `MenuAction`s. They use the pointer hold path
(`begin_pointer_nav(action)` / `end_pointer_nav()`) — the same machinery held-Space uses,
matching the macOS `HoldSegmentedControl`. The panel must handle, explicitly:

- **press** → `begin_pointer_nav`; **release** → `end_pointer_nav`.
- **drag-off** the button while held → `end_pointer_nav` (treat as release).
- **pointer-cancel / window focus loss** → `end_pointer_nav` (a lost pointer-up must not leave
  nav stuck flying — mirror the keyboard focus-loss release net).
- **quick click** (press+release, no hold) → exactly **one** advance. Do **not** also fire an
  egui `clicked()` `MenuAction` — that double-advances. Drive the whole nav interaction from the
  press/release states, not `clicked()`.

The toolbar must also be added to **`overlay_panel_visible()`** (`main.rs:1388`) and the
pointer-routing gate (`main.rs:1790`, `pointer_over_panel && overlay_panel_visible()`), or egui
will draw it but not reliably receive pointer events. Non-nav buttons can use egui `clicked()` →
`MenuAction`.

## Layout reservation (reuse `set_content_top_inset`)

- When `show_toolbar` is on **and** windowed, reserve `TOOLBAR_H` (logical px, ~36–40) at the
  top by calling `renderer.set_content_top_inset(physical_px)`. One logical `TOOLBAR_H` →
  physical via the current scale factor. Keep the egui panel's exact drawn height **==** the
  renderer inset, or the photo and the bar disagree by a hairline.
- **Verify the setter refreshes geometry.** `set_content_top_inset` (`gpu.rs:1976-1977`) stores
  the field; whether it rebuilds the quad immediately or only on the next `set_view` push must be
  checked — the Linux menu-bar path exercises it, so either it works or there's a latent
  one-frame lag. If it doesn't rebuild, fix the setter to refresh geometry, or push the current
  view right after changing the inset.
- **Occlusion is Fit-only.** The comment at `gpu.rs:385` documents (task #59 spike) that a
  zoomed/cropped photo's overflow **rides up under the bar** — so Fill, Original, zoomed, and
  panned images *do* extend under the strip. v1 requirement is therefore **"Fit-mode content
  never overlaps the toolbar,"** matching the existing menu-inset behavior. A strict
  non-occlusion guarantee for all modes would need a content-viewport scissor in the scene pass —
  **out of scope for v1**; note it as a possible follow-up.

## Fullscreen + live-toggle atomic sync

Chrome must never flash a stale frame. **Before the first fullscreen present:** (1) stop
building the toolbar panel, (2) `set_content_top_inset(0)`, (3) invalidate/clear the retained
toolbar texture — in that order. **Before the first windowed present:** reverse it. The same
atomic sequence applies to toggling `show_toolbar` at runtime (both the overlay build and the
photo geometry must change on the same frame). This mirrors how the shell already asserts chrome
on the fullscreen round-trip.

## Settings

- `show_toolbar: bool`, **default `true`** (`pb_app_core::settings`). Needs
  **`#[serde(default = …)]`** so old settings files (without the key) load as enabled.
- Full **draft → load → save round-trip** wiring in the Settings dialog (the #85 live-autosave
  path), using `pb_ui::toggle` in a View/Appearance group.
- **Immediate apply** (no restart): flips the overlay build + the renderer inset on the next
  frame via the atomic sequence above.
- **Platform scope:** stored in shared settings but **honored only by the Windows (winit)
  shell** — macOS uses its native Hide-Toolbar; Linux has its own menu-bar inset. Document this
  so the key isn't assumed cross-platform.

## Icons (subtask) — vendor BOTH families

The `glyph!` macro (`icon.rs:118-124`) emits `include_str!` for **both** `icons/solid/<name>.svg`
**and** `icons/regular/<name>.svg` for every `Icon` variant (even though `ACTIVE_FAMILY = Solid`).
**Solid-only vendoring will not compile.** For each new glyph, vendor **both** the solid and the
regular SVG (from `D:\Media\fontawesome-pro-plus-7.3.0-web\svgs\{solid,regular}\`), add an `Icon`
variant + `glyph!` arm, and show it in the gallery Icons row.

New glyphs needed: `chevron-left`, `chevron-right`, `angles-left`, `angles-right`, `shuffle`,
`rotate-left`, `rotate-right`, `table-list` (Details — distinct from `Icon::Info`, which is
already `circle-info` and serves the info-line toggle), `folder-tree`,
`up-right-and-down-left-from-center`. Reuse existing: `Trash` (`trash-can`), `Play`, `Pause`,
`Images`.

**Mirrored shuffle** (for `RandomPrev`) needs an **explicit flipped-UV paint API** added to
`pb_ui::icon` (a `paint_tinted_flipped` / `Mirror` option) — a "render mirrored" comment is not a
mechanism. Matches the macOS `shuffleImage(flipped:)` trick.

## Accessibility

Every icon-only control needs a **tooltip + accessible label**, derived from the existing
`Action::label()` plus the bound shortcut from the keymap (so "Next (Space)" etc.), keeping the
toolbar legible and screen-reader-navigable without hand-maintained strings.

## Responsive layout

A **pure width-selection function**: given available width and the ordered group priorities,
returns which groups are shown. Every group has a priority; there is a **final narrow-window
fallback** (nav + counter + fullscreen minimum) since there is no enforced minimum window width.
Collapse order: folder-nav → rotate → random. **Assert left and right clusters never overlap at
any supported DPI** (100/125/150/200%). A true `»` overflow pull-down is **later work**; v1 uses
deterministic priority hiding — but controls must never clip or overlap.

## Performance — measure, don't assert

Do **not** claim zero nav cost. Retaining the egui texture avoids re-tessellation when static,
but the present pass still composites the full-window egui texture every frame
(`pb-render/src/gpu.rs:2828`), and the toolbar goes **dirty on every nav** (counter, play/active
states change), forcing a re-tessellate + re-upload of the strip region.

**Bounding context:** the toolbar is **windowed-only**; the keypress→photon target is measured
in the *fullscreen* borderless Independent-Flip path, which has **no toolbar** — so the hot-path
budget is structurally untouched. The cost to characterize is **windowed-browsing** composite +
toolbar-dirty-on-nav.

**A/B gate:** a scripted keypress workload at the 8K target / 120 Hz, **toolbar-on vs
toolbar-off** (windowed), reporting **CPU/GPU p50/p95/p99**, **missed refreshes**, and
**keypress→photon** (per the project's NDJSON A/B pattern). If the full-window composite is
measurable, the escalation is a **strip-sized retained target** (or another bounded composite) —
composite only the dirty strip rows, not the whole window. Keep the action set frozen until this
and the narrow-width measurements are in.

## Implementation phases (test-first)

Per the TDD norm, each phase writes its tests before the implementation.

1. **`ToolbarState` + derivation.** Tests: counter stays on `displayed_item` during a
   resident-ring miss (nav target advanced, photo not yet presented); `motion_playing` /
   `current_has_motion` are video-aware; snapshot equality dirties only on real change; cold
   start hides the counter. Then implement the struct + the pure derivation and the shell diff.
2. **Icons.** Vendor both families for each new glyph, add `Icon` variants + `glyph!` arms, add
   the flipped-UV paint API; show all in the gallery. (Compile gate is the test.)
3. **`pb-ui` `toolbar_button` atom** (idle/hover/pressed/active-accent/disabled) driven by
   tokens + `Palette`, returning the `Response`; tooltip/label plumbing. Gallery entry with
   headless **light/dark snapshots** for active, disabled, and narrow states.
4. **Responsive width-selection function** (pure). Tests: correct group set at each breakpoint;
   narrow fallback; left/right never overlap at 100/125/150/200% DPI. Then wire it.
5. **`toolbar.rs` panel** in `EguiOverlay::run`: lays out groups from the atom + width function;
   non-nav buttons → `MenuAction`; nav/random → pointer hold-to-blaze. Tests: button→`Action`
   mapping table (mirrors `menu.rs` `action_for`); pointer quick-click fires exactly one advance
   (no double), hold/release/drag-off/focus-loss lifecycle. Add the toolbar to
   `overlay_panel_visible()` + the pointer gate.
6. **Layout reservation** via `set_content_top_inset`; verify/fix the setter refresh; panel
   height == inset. Tests: Fit content never overlaps the bar; Fill/Original/zoom-anchor/pan
   behave (ride-under accepted); DPI conversion correct.
7. **Fullscreen + live-toggle atomic sync.** Tests: fullscreen round-trip has no stale-toolbar
   frame and no stale photo-geometry frame; runtime `show_toolbar` toggle changes overlay **and**
   geometry on the same frame; toolbar-off preserves the exact current no-overlay path.
8. **Settings.** `show_toolbar` with `#[serde(default)]`; draft/load/save round-trip; immediate
   apply. Tests: old settings (missing key) default to enabled; toggle round-trips.
9. **Performance A/B gate** (the 8K/120 windowed toolbar-on/off benchmark above). Only after this
   is the "no measurable nav cost" claim allowed — with numbers.

## Testing (consolidated)

- Counter remains on the displayed item during a resident-ring miss; hidden at cold start.
- Video-aware motion + play state (`motion_playing` / `current_has_motion`).
- Pointer quick-click (one advance), hold, release, drag-off, focus loss.
- Layout at narrow widths and 100/125/150/200% DPI; left/right never overlap.
- Old settings (no `show_toolbar`) default to enabled; toggle round-trips.
- Runtime toggle changes overlay **and** photo geometry atomically.
- Fullscreen round-trip: no stale-toolbar / stale-geometry frame.
- Toolbar-off preserves the current no-overlay code path.
- Headless light/dark snapshots: active, disabled, narrow layouts.
- The 8K/120 windowed A/B perf gate (CPU/GPU p50/p95/p99, missed refreshes, keypress→photon).
- Privacy: the existing no-trace test is unaffected (`show_toolbar` is app config; the toolbar
  reads only in-RAM state, writes nothing on the view path).

## Non-goals (v1)

- Auto-hide / fade / floating placement (rejected — docked + persistent).
- User customization / drag-to-rearrange (the macOS palette). Fixed set for v1.
- A true `»` overflow pull-down (deterministic priority hiding for v1).
- Strict non-occlusion for Fill/zoom/pan (content-viewport scissor) — Fit-only for v1.
- Showing the filename/title in the bar.

## Open questions (owner)

1. **Details/EXIF glyph** — `table-list` proposed to distinguish from the info-line's
   `circle-info`. OK, or prefer a different pair?
2. **`ToolbarState` home** — a `pb-app-core` struct (shareable, testable in core) vs. a
   winit-shell-only snapshot. Leaning core struct for the pure derivation test.
3. **Bottom edge** — one subtle 1px divider under the strip (settled) — confirm tone (a faint
   `Palette` border vs. a hairline shadow).
