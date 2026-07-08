# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-08 (second session, later). Supersedes the tree-bleed investigation handoff._

## NEWEST (uncommitted): Linux menu keyboard nav + WSLg Wayland crash fix

Two work items on top of the committed bleed fix (`751d7db`), both verified live under the
X11 harness, ready to commit after owner sign-off:

1. **Menu-bar keyboard navigation (GTK-style), Linux egui bar.** The bar/dropdown are now
   fully state-driven by a shell-owned `menu::MenuNav` (open menu + selected row + Alt hint):
   - `Alt+F/E/V/G/I/H` opens that menu (mnemonic = title's first letter; underlines shown
     while Alt is held), `F10` toggles the first menu.
   - Arrows navigate (`←`/`→` switch menus, `↑`/`↓` move the selection, skipping separators
     and disabled rows, wrapping), `Home`/`End`, `Enter`/`Space` activates, a letter jumps to
     matching items (unique match activates). **Esc closes the menu instead of quitting.**
   - An open dropdown grabs all key *presses* (native behavior); key *releases* still reach
     the core's held-key tracker (fly-strand safety). Clicking outside closes the menu and
     the click never leaks to the photo.
   - Logic is the pure `menu::menu_nav_key` (unit-tested: 4 new tests in `menu.rs`); rendering
     in `panels_ui.rs` (`menu_bar` + shared `menu_dropdown`); wiring in `main.rs` (`menu_key`,
     interception at the top of the key-Pressed arm, ModifiersChanged → alt_hint, click-outside
     swallow). Pointer hover only steals the selection while the mouse is actually moving.

2. **WSLg Wayland crasher dodged (`build_event_loop`, main.rs).** Owner hit
   `attempt to add with overflow` in winit 0.30.13 `wayland/seat/keyboard/mod.rs:135`
   (`key + 8`) pressing Alt+Right on the default Wayland backend — WSLg's RDP input bridge
   sends bogus (u32::MAX-ish) keycodes. **Unfixed upstream** (checked winit master + issues;
   worth filing). Mitigation: the app now prefers the **X11/XWayland backend when WSL is
   detected** (`WSL_DISTRO_NAME` / `/proc/sys/fs/binfmt_misc/WSLInterop`, with `DISPLAY` set);
   real Linux desktops keep winit's normal Wayland preference. `PB_BACKEND=x11|wayland`
   overrides. Bonus: X11 also dodges WSLg's Wayland display-loss segfaults and makes the app
   screenshotable/drivable. Verified: launch with both servers up auto-picks X11, no crash.

3. **PageUp/PageDown = prev/next folder (keymap secondaries).** Owner found physical
   Alt+Left/Right dead under WSLg even on X11 while Alt+Up works: the **Windows host
   translates Alt+Left/Right into browser back/forward app-commands for remoted (RDP/RAIL)
   windows**, so the arrow never reaches Linux at all (the microsoft/wslg#188 pattern —
   a WSLg contributor confirmed via xev that such chords are "not delivered from Windows
   side"; the earlier Wayland garbage-keycode crash was the same swallow manifesting).
   XTEST-injected Alt+Right works fine — the app wiring was never broken; only host-delivered
   input dies. Fix: `PrevFolder`/`NextFolder` gained always-delivered secondary chords
   **PageUp/PageDown** (keymap defaults, all platforms; Alt+Left/Right stay the shown
   primaries; unit test `folder_nav_has_pageup_pagedown_fallbacks`). Verified live.
   Alt+Left/Right will work on a real Linux box — this is purely a WSLg host limitation.

No CHANGELOG entries (Linux is experimental — per the norm).

---

## TL;DR (earlier this session)

The **Linux tree/Inspector panel "bleed" is FIXED and verified** (uncommitted). Root cause was
neither of the prior session's hypotheses: egui's `ScrollArea` clips its content to the viewport
**expanded by `Visuals::clip_rect_margin` (3px default)** (egui 0.29 `scroll_area.rs:595-600`) —
meant to keep focus rings/shadows from being cut inside a *padded* container. Our panel bodies run
flush against the panel background's edge, so every scrolled-out row painted a 3px sliver *outside*
the panel: into the header above, over the photo below. The fix is one line in the shared
`scroll_body` helper (`panels_ui.rs`): `ui.visuals_mut().clip_rect_margin = 0.0;` — covers the
tree, Inspector, and Help in one place. **This was never Linux-specific** — the winit/Windows
shell shares this code; it was just first noticed on Linux. CHANGELOG line added under
`[Unreleased] ▸ Fixed`.

## How it was pinned (and where the prior analysis went wrong)

Added a debug dump of the egui offscreen texture (`EguiOverlay::target`) to PNG *before*
compositing, plus per-paint-job clip-vs-vertex-bounds probes, then drove the live app under the
X11 harness:

- **The bleed was present in the offscreen texture itself** → egui pass, not the composite.
- **The composite is mathematically exact** — probed pixel (150,600): predicted premultiplied-over
  value 76, live value 76. Hypothesis (B) (premultiplied-alpha composite mismatch) is dead. The
  "photo showing through the panel" is just the ~71% `info_opacity` setting.
- Scroll-content jobs carried `clip=(21,86)-(367,779)` against a panel background ending at
  y=776 / viewport starting at 89 — the ±3 is `clip_rect_margin`. The leaked sliver measured
  exactly rows y=776–778.
- Prior session's errors: (1) the "2 paint jobs with full-screen clips" were **not** "the photo +
  a HUD element" (the photo is never an egui job); (2) headless `--egui-shot` "clipping fine"
  proved nothing — its tree content fit the viewport, so there was nothing scrolled-out to leak;
  (3) "geometry is provably correct" was true but irrelevant — the layout was never the bug.

Verified post-fix under the X11 harness with the tree scrolled mid-list (the leaking state):
top edge clean under the "Folders" header, bottom row cut exactly at the rounded corner, and the
pixel probe rows below the panel edge show only the soft shadow. Inspector (Shift+I) clean too.

## State of the tree (uncommitted)

- **The fix:** `panels_ui.rs` `scroll_body` — `clip_rect_margin = 0.0` + explanatory comment.
- **CHANGELOG.md**: new `Fixed` entry under `[Unreleased]`.
- **All `PB_TREE_DEBUG` scaffolding stripped** per the previous handoff's plan: the probes in
  `panels_ui.rs` (`tree_panel`, `sdf_panel`), the `[jobs]` block in `egui_overlay.rs`, and the
  startup build banner in `main.rs`. The temporary texture-dump code was added and removed within
  this session (recover from this session's transcript if ever needed again).
- **`cargo fmt --all` applied** — also reformatted files the previous wip commit left unformatted
  (`menu.rs`, `update.rs`, `default_app.rs`, …), so the diff has some fmt-only churn.
- `cargo test -p pb-app -p pb-render`: 79 passed. Clippy: only pre-existing dead-code warnings
  (Linux-gated menu scaffolding).

## Next steps

1. Owner: try the menu keyboard nav + confirm Alt+Right no longer crashes on a normal launch
   (the app should print "WSL detected — using the X11 (XWayland) backend"). Then commit the
   menu + backend work (bleed fix already landed as `751d7db`).
2. Consider filing the winit issue upstream (WSLg Wayland `key + 8` overflow, keyboard/mod.rs:135,
   still present on master).
3. The macOS task #58 (auto-size Settings window) handoff from 2026-07-07 is still pending —
   recover its plan via `git log` on this file if picking that up.

## Environment quick-ref (unchanged)

- Repo now lives natively in WSL Ubuntu at `~/photoblaze` (git clone, no rsync/mtime dance).
- X11 harness: `export DISPLAY=:0; unset WAYLAND_DISPLAY`, run the app, `xdotool search --class
  photoblaze`, `xdotool key --clearmodifiers shift+f` (XTEST, never `--window`), scroll via
  `xdotool mousemove … click --repeat N 5`, screenshot with `import -window $WIN out.png`.
- HEIC needs `--features libheif`; see [[linux-port]] for the Linux menu-bar/portability context.
