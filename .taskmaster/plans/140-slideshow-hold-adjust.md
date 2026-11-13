# #140 — Slideshow interval: 0.1s taps + hold-to-accelerate for `[` / `]`

Owner request 2026-11-12: finer slideshow timing (0.1s), and holding `[` / `]` should
accelerate through coarser steps instead of just repeating 0.1s quickly.

## Design

- **Taps scale with the interval** via `slideshow::ZONES` (rows of `from`, `tap`,
  `max`): 0.1s under 10s, 0.5s from 10s to 30s, 1s from 30s (was 0.5s at/above 0.5s,
  0.1s below). Zones are
  directional (going down, a boundary belongs to the zone below), and a step stops on
  a boundary rather than crossing it, so a tap up and a tap down always cancel out
  (9.9 → 10.0 → 10.5 → 10.0 → 9.9; pinned by `taps_walk_the_same_ladder_both_ways`).
- **Holding** steps on `slideshow::HOLD_CURVE`, one data table of rungs
  (`after` = how long held, `step`, `every` = repeat cadence). Starting values:
  0.1s every 80 ms → 0.5s every 150 ms after 1 s → 1s every 150 ms after 2.5 s →
  5s every 200 ms after 5 s. The first repeat also waits out the shared hold delay
  (`initial_delay`, the nav tap-vs-hold setting), so a tap is always exactly one step.
  A hold starts on the first rung at least as coarse as the zone's tap
  (`hold_stage`): at 12s it starts on the 0.5s rung *and its cadence*, rather than
  forcing 0.5s steps onto the 80 ms rung, which would run fast then slow down at 1 s.
- **`Zone::max`** caps a held step by magnitude: 0.1s below 1s, 1s below 10s, 5s
  above. Keeps the low end fine and stops a coarse rung leaping the floor.
- **Menu clicks jump between round values** (#140.7, owner 2026-11-13): View ▸
  Slideshow Faster / Slower steps through `slideshow::MENU_STOPS` (0.1, 0.2, 0.5, 1,
  2, 3, 4, 5, 7, 10, 15, 20, 30, 45, 60s), so 4s → 30s is six clicks, not 26. A value
  the keys left between stops snaps to the nearest stop in the click's direction; at
  either end a click stays put. Same route on both shells: `CoreEvent::MenuAction` →
  `dispatch_action` → `AppCore::slideshow_menu_step`. Keys never take this path (they
  resolve to `RepeatStart`).
- **Grid snapping:** each step snaps to its own size in the direction of travel
  (5.1 → 5.5 → 6.0). Math in whole deciseconds, so no float drift.
- **Pause while held:** the slideshow does not advance while `[` or `]` is down. Any
  interval change (tap, hold, menu) sets `Slideshow::adjusted_at`; the dwell counts
  from `dwell_anchor` = later of `last_present` and `adjusted_at`, so the current
  slide gets a full interval at the new speed. `last_present` itself is untouched (it
  also paces hold-to-blaze and the eager animation prep).
- **Input:** new `ActionKind::Repeat` → `KeyResolution::RepeatStart`; press steps once
  and inserts into `held`; `tick_slideshow_adjust` drives repeats and keeps the loop
  polling while held. Focus loss clears `held`, which ends the hold on the next tick.
  Shells needed no input changes: neither registers a menu accelerator for these
  actions, and both forward clean KeyDown/KeyUp with OS repeat flagged.
- **Constants, not settings** (owner call 2026-11-12): tune once, users don't need it.

Tuning: edit `HOLD_CURVE` / `ZONES` / `MENU_STOPS`, then
`cargo test -p pb-app-core curve_report -- --ignored --nocapture` prints the
value-over-time trajectory. The pinned tests check properties (tap = one step,
monotonic, half-second grid on coarse steps, reaches both limits, fine run into
the floor), not exact sequences, so retuning shouldn't need test edits unless a
property changes on purpose.

## Handoff

**Verified (Windows, 2026-11-12):**
- `cargo test -p pb-app-core` green, including the 22 pure `slideshow::tests` and 6
  `app_core_impl::slideshow::tests` driving real KeyDown/KeyUp/FocusLost/MenuAction
  events through `AppCore::handle` + `tick` with a fake clock.
- `cargo clippy -p pb-app-core -p pb-app --all-targets -- -D warnings` clean.
- Ship-feature build of the winit shell compiles, and the owner held and tapped
  `[` / `]` in it (two rounds, signed off).

**Verified (macOS, Buzz, 2026-11-12):**
- `cargo clippy --all-targets -p pb-mac-ffi -p pb-app-core -p pb-core -p pb-decode
  -p pb-source -p pb-render -p pb-hud -- -D warnings` clean (the CI mac-swift set).
- `cargo test -p pb-app-core -p pb-mac-ffi`: 1018 + 39 passed (4 ignored, the same
  1022 as Windows), including all 28 slideshow tests.
- The Swift host builds against this change (`build-swift-host.sh --release
  --bundle-ffmpeg`) with the keyUp `syncToolbar()` fix below.
- Menu double-fire ruled out by reading the code: `item("slideshow_faster"/"_slower")` has no
  key equivalent, and the key monitor swallows a plain `[` / `]` before AppKit's menu
  could see it.
- **Owner feel test on the Mac, 2026-11-13:** held and tapped `[` / `]` in the rebuilt app
  and signed off ("working well"). 140.6 and #140 closed.

**Verified (Windows, 2026-11-13, #140.7 menu stops):**
- `cargo test -p pb-app-core`: 1027 passed, 4 ignored. That includes 4 new pure
  `MENU_STOPS` tests and 2 core tests: a `MenuAction` click jumps to the next stop
  and restarts the slide, and a key tap after a click still moves a tenth.
- `cargo clippy -p pb-app-core -p pb-app --all-targets -- -D warnings` and
  `cargo fmt --all -- --check` clean.

**Verified (macOS, Buzz, 2026-11-13, #140.7 menu stops, at 19cff206, clean tree):**
- `cargo clippy --all-targets` on the mac-swift set (`pb-mac-ffi pb-app-core pb-core
  pb-decode pb-source pb-render pb-hud`) with `-D warnings`: clean.
- `cargo test -p pb-app-core -p pb-mac-ffi`: 1023 + 39 passed, 4 ignored. That is +5 on
  the 1018 at 0d3db2f4: the 6 new tests, less the one renamed. The slideshow filter is 33
  passed, 1 ignored, and all 6 new menu tests are among them.
- `build-swift-host.sh --release --bundle-ffmpeg` builds, and the FFmpeg audit is OK. No
  Swift change was needed. The menu reaches the core through `MenuBar.swift` →
  `CoreModel.menuAction` → `pb-mac-ffi menu_action` (lib.rs:939) →
  `CoreEvent::MenuAction`, and the Mac toolbar has no faster/slower buttons.

**Not verified:**
- #140.7 by hand on either platform: clicking the menu items in a running app. Close
  140.7 and #140 once the owner has clicked through them.

**Cross-platform debt:**
- ~~**Mac toolbar interval label goes stale during/after a hold.**
  `mac/Sources/BlazeViewerMac/CoreModel.swift` (~line 385) re-syncs the toolbar only on
  a non-repeat keyDown, but held steps now land on `tick`. Fix: also `syncToolbar()` on
  keyUp (or on any `[`/`]` keyUp). The Windows toolbar reads `slideshow.interval` live
  and needs nothing. Owed by: Mac. Subtask 6.~~ **Fixed on the Mac 2026-11-12:**
  `CoreModel.swift` now re-syncs on every keyUp as well as a non-repeat keyDown. The toast
  shows the live value during a hold, and the label catches up on release. Build and tests
  pass; owner signed off on the Mac 2026-11-13.

**Decisions / corrections:**
- 2026-11-12, owner feel test of the first build: the hold is "pretty good", but a
  flat 0.1s tap is too slow at large intervals. Taps now scale with the interval
  (`ZONES`: 0.1s / 0.5s from 10s / 1s from 30s). Round 2, same day: owner approved
  ("looking good"). Feel on Windows is signed off; tune the tables only if it starts
  to grate.
- 2026-11-13, owner: stepping by tenths from the menu "would take forever", so menu
  clicks now jump between round values (`MENU_STOPS`, #140.7). The alternative was a
  Slideshow ▸ Speed submenu listing the values with a checkmark. It was deferred:
  it needs menu work in both shells and an action that carries a value, and the
  step-through gets most of the benefit with a core-only change.
- 2026-11-13: the winit menu keeps its `[` / `]` hints ("Slideshow Faster\t[",
  pb-app/src/menu.rs) even though a click now jumps further than a key tap. The key
  goes the same direction, and the hint is how a menu user learns the faster, finer
  path. The Mac menu shows no key hint (unchanged). The Mac session flagged this.

**Claimed:** nothing. The Mac verification of #140.7 is recorded above. Subtask 6
landed in 0d3db2f4.
