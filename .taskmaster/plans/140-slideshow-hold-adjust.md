# #140 — Slideshow interval: 0.1s taps + hold-to-accelerate for `[` / `]`

Owner request 2026-11-12: finer slideshow timing (0.1s), and holding `[` / `]` should
accelerate through coarser steps instead of just repeating 0.1s quickly.

## Design

- **Taps scale with the interval** via `slideshow::ZONES` (rows of `from`, `tap`,
  `max`): 0.1s under 10s, 0.5s from 10s to 30s, 1s from 30s (was 0.5s at/above 0.5s,
  0.1s below). Menu clicks (View ▸ Slideshow Faster / Slower) are taps too. Zones are
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

Tuning: edit `HOLD_CURVE` / `ZONES`, then
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

**Not verified:**
- **macOS** at all: core-only change, but the Mac shell has not been built against it.
  Expected to work unchanged for input (the Mac forwards `isARepeat` and has no key
  equivalent on these menu items).

**Cross-platform debt:**
- **Mac toolbar interval label goes stale during/after a hold.**
  `mac/Sources/BlazeViewerMac/CoreModel.swift` (~line 385) re-syncs the toolbar only on
  a non-repeat keyDown, but held steps now land on `tick`. Fix: also `syncToolbar()` on
  keyUp (or on any `[`/`]` keyUp). The Windows toolbar reads `slideshow.interval` live
  and needs nothing. Owed by: Mac. Subtask 6.

**Decisions / corrections:**
- 2026-11-12, owner feel test of the first build: the hold is "pretty good", but a
  flat 0.1s tap is too slow at large intervals. Taps now scale with the interval
  (`ZONES`: 0.1s / 0.5s from 10s / 1s from 30s). Round 2, same day: owner approved
  ("looking good"). Feel on Windows is signed off; tune the tables only if it starts
  to grate.

**Claimed:** the Mac session (Buzz) took subtask 6, the `CoreModel.swift` keyUp
`syncToolbar()`, plus building pb-mac-ffi and the Swift host against this change,
once it is pushed.
