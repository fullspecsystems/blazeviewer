# Task 61 — Windows toolbar (parity with macOS #55, Windows conventions)

**Status (2026-07-19, owner):** shipped and working well visually — the full rev2 spec
(state model, icons, pointer hold-to-blaze nav, layout reservation, fullscreen/live-toggle
sync, settings toggle) is implemented. What's left is small.

## Remaining

1. **Narrow-width overlap: cut the counter.** The right-aligned photo counter (`idx / count`)
   is redundant with the native title bar, which already shows `name (idx+1/n)`, and it
   overlaps other groups weirdly at narrow window widths. Likely fix is simply to drop the
   counter from the toolbar — no real design needed here.
2. **Performance A/B gate** — the planned 8K/120 windowed toolbar-on vs toolbar-off benchmark
   (CPU/GPU p50/p95/p99, missed refreshes, keypress→photon) was never run. Worth doing
   eventually to put a number on it, but owner doesn't believe it's a meaningful perf hit in
   practice — **low priority, not a blocker.**
