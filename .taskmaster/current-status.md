# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-20 (rev 26). Session ran on **Windows**, with a macOS session working
the same tasks in parallel through the day. Everything below is on `main`, pushed._

---

# ▶️ START HERE

**#124 (zoom) and #126 (DRY the shells) are both COMPLETE. #125 (split
`app_core_impl.rs`) has reached its STOP POINT — every leaf is split, 22,105 → 14,218 lines,
26 concern files.** What remains in the parent is the charter (lifecycle, dispatch, residency &
present engine); splitting the engine is a deliberate STOP (§7 step 5), a separate task if ever.
**There is no live #125 work to continue** — the next thing a fresh session picks is from the
backlog below, or the one Mac-only verification item in the plan's `## Handoff`.

New this session and worth reading before writing any code:
**`docs/where-code-goes.md`** — an ordered decision procedure for where a new function
belongs. **"Put it on `AppCore`" is the last answer, not the first.** Linked from
`CLAUDE.md` → *Working norms*.

---

# ✅ #125 — split `app_core_impl.rs` (LEAVES DONE, at the STOP point)

Plan: `.taskmaster/plans/125-split-app-core-impl.md` (rev 5 — read *Progress* → "Step 3" and
`## Handoff`).

**Result: 22,105 → 14,218 lines, 26 concern files under `app_core_impl/`** (production
12,504 → 4,730, 80 methods). Every leaf concern is now its own `app_core_impl/<name>.rs` block:
`animation archive_open audio_tracks background clipboard compare delete describe dir_scan
image_text item_kind menu meta nav open panels prefs save_rotation secret slideshow subtitles
thumbs toast tree undo video view`. 9-of-10 early ones and most later ones pair with an
existing logic module (the two-halves rule).

**How it was verified.** Alternating commits: pure moves (each `verify-pure-move` **2051 →
2051 byte-identical**, re-checked after `cargo fmt`) and clearly-labelled `pub(super)`
visibility edits (each flagging *only* its expected names, hand-diffed to keyword-only).
`clippy --workspace --all-targets -D warnings` clean, `cargo test --workspace` green, and the
**ship build** `pwsh scripts/build-windows.ps1` (`libheif,dav1d,ffprobe`) succeeds.
**Codex-reviewed clean** at the stop point on visibility / scope-resolution / cfg (plan → *Codex
review*).

**The STOP (§7 step 5).** What's left in the parent is the charter and nothing else: lifecycle,
dispatch, deck ingestion, and the residency & present engine (`tick`, `dispatch_action`,
`request_prefetch`, `drain_results`, `present_item`, the fit-stash/GPU-derive block, …).
Splitting the engine is genuinely coupled hot-path work and is a **separate task with its own
plan if ever** — do not continue on momentum. The charter is now a test, not a slogan: the
`effective_*` accessors and menu projection were moved out precisely because they contradicted
it.

**One Mac-only verification owed** (plan `## Handoff`): `video.rs`/`animation.rs` hold
macOS-gated methods no Windows build type-checks. They moved byte-identically and were audited
fully `crate::`-qualified, so scope can't rebind them — but a Mac should build `pb-mac-ffi` once
to confirm "compiles on macOS". Verification, not owed work. **The parent is released** — no
longer claimed; anyone may edit it.

### Reusable machinery left behind (in `scratchpad/`, not committed — the verifier is the keeper)

`extract.py` (cut methods with docs+attrs, bounded to the production impl, bracket-depth
attribute walk), `privcheck.py` (which privates need `pub(super)` — scans parent + `mod tests`
+ all siblings), `namecheck.py` (would `mod X;` collide with an import), `wiremods.py` (rewrite
the sorted `mod` block). If the engine split is ever taken up, these are the starting point.

### ⚠ Ten traps now, all learned the hard way (full text in plan §3c + "Traps 7–10")

1. A moved private breaks its parent caller → `pub(super)`, as its **own labelled commit**.
2. §4's name-clustered table is superseded — the file is **already ordered**; read it in order.
3. The verifier proves **textual conservation, not behaviour** — can't see scope/imports,
   module-macros, same-name swaps, non-function items.
4. `app_core_impl.rs:NNNN` anchors churn — accepted.
5. `cargo check` misses it — a moved private can break **only the test build**. Use
   `clippy --all-targets`.
6. The visibility edit goes **FIRST, in place** — move-then-widen has no compiling intermediate.
7. **Bare crate-module imports collide with a paired `mod`** (`slideshow`, `hud`→`toast`) —
   §3a firing for real, compiler-only. `namecheck.py` pre-checks.
8. **`privcheck` must scan already-split siblings**, not just the parent (`refresh_info_line_visibility`).
9. **The extractor severed multi-line `#[cfg(any(…))]`** — the one bug that could have silently
   dropped a cfg gate; caught by both the compiler and the hash. Fixed to rewind by bracket depth.
10. **Same-name items in `mod tests`** (`stream_frame`) — extractor now bounds to the production impl.

---

# ✅ #126 — DRY the two shells (COMPLETE 2026-07-20)

Plan: `.taskmaster/plans/126-dry-the-shell-orchestration.md` (see its `## Outcome`).

Both shells' dir-scan and archive-open copies are gone (**~−800 lines across the two**),
replaced by one tested core implementation: `background.rs` (one generation space across both
operation kinds), `dir_scan.rs`, `archive_open.rs`. All six Windows verification items and the
macOS run passed. winit's dead Scanning dialog deleted (−208/+23).

**Five defects found along the way** (none were the point of the task): the empty-deck welcome
hint (both shells), the "Checking…" spinner rendering under the button bar on password retry,
the Opening-dialog boundary flash (gate 250 → **500 ms**), the wrong-password message that
never showed for anything routing through the Loading dialog, and cancel-with-a-proven-password
now promoting it to the session MRU (gated on `progress.done() > 0`).

⚠ **Do not "fix" the residual boundary flash with a minimum display duration** — considered and
rejected; the rationale is in `LOADING_DIALOG_DELAY`'s doc comment. Read it before changing
the value.

**Open, not part of the task:** the macOS untraced bottom-left spinner during a quick open
(needs a Mac); two cosmetic one-frame gaps (door card over a photo on archive entry — core
proven innocent over 923 frames; `archive_scope` lagging the deck by one frame); and the
**"strays"** (`apply_menu_state`, `confirm_delete_permanent`, `toggle_recursive`,
`toggle_show_archives`) — deliberately deferred out, subtask 3 is *cancelled* not skipped, and
they want their own task.

---

# ✅ #124 — zoom binds the resident Original (COMPLETE, owner-verified)

Smooth zoom (`=`/`-`, pinch, Ctrl+scroll, hold-to-zoom) in Fit mode magnified the fit-sized
texture. `display_kind()` picked the rep from `view.mode` alone, so zoom could never reach the
ring's `Original` — even when #106.7 had it resident. Now a present-time selector
(`present_kind`) binds it; decode targets stay mode-derived (pinned by a test).

⚠ **The trap:** `present_item` resets zoom/pan via `view_for`, so the rebind needs its own
view-preserving path (`rebind_same_item`) which must **not** re-stamp `last_present` (slideshow
dwell). ⚠ **The Codex P0:** three background paths rebind the Fit slot for the displayed item —
`try_gpu_sharpen`, `try_gpu_derive_fit`, and the `drain_results` sharpen landing. All now
decline while `presented_kind == Original`. **House rule: background work may change residency
or quality, never the presented representation.**

Also fixed this session: a **dialog-window `Resized` was being applied to the main window**
(negative-only id filter in `window_event`), which stretched the toolbar ~10×.

---

# ⏭️ Backlog beyond #125 (carried forward, unchanged)

1. **#121 subtask 4 (FFmpeg 3b)** — interrupt classification (cancel vs deadline vs real
   decode error, `ffmpeg/poster.rs:224`). Small, Mac-doable, closes a known gap.
2. **#109 items 2/3/5** — shared open generation, decode identity, `present_item` propagation.
   Items 1 and 4 are done. ⚠ Its **item 1 is stale**: the audit's claim that macOS lacks the
   cross-cancel is **false** (corrected in `technical-debt-audit.md` 2026-07-20).
3. **#112 profiles** — design rev 4 implementation-ready, paused on owner sign-off.
4. **#106.1 byte cache / #106.3 read throttling** — the COLD-read side; only pays cold.
5. **Re-measure the thumb strip** (`PB_THUMB_DIAG=1`) on the Videos share — two fixes landed
   after the 199 ms reading. The number decides whether #114 selection parity is ever worth it.
6. **Low value:** #121 subtasks 5/6, #92.2 (AVFoundation).

---

# 📓 Load-bearing knowledge (don't re-derive)

- **Cross-machine:** `CLAUDE.md` → *Working across two machines*. One `## Handoff` section per
  plan is the only place live cross-machine state lives. Never mark verified what you could
  not run. ⚠ **`pb-app` builds `AppCore` as a struct literal; `pb-mac-ffi` uses
  `AppCore::new_host`** — so adding an `AppCore` field breaks winit and *not* the Mac, with no
  warning on the Mac. It has already broken `main` once.
- **`pb-mac-ffi` is `#![cfg(target_os = "macos")]`** — on Windows it compiles to an empty
  staticlib, so a syntax error in it produces **zero** errors. Mac-shell edits are unverifiable
  from Windows. The reverse also holds: `pb-app`'s `build.rs` hard-errors on macOS, so the Mac
  reaches it only via the `x86_64-pc-windows-msvc` cross-check (which needs `ureq` at
  `default-features = false` **plus `features = ["json"]`**).
- **Windows build:** `pwsh scripts/build-windows.ps1 -Run`. A bare `cargo run` omits `ffprobe`
  → silent AC-3/DTS. ⚠ A **running exe blocks the linker** (`Access is denied`, os error 5) —
  close the app before rebuilding.
- **Codex:** unreliable on this repo's big files — 2 of 4 runs produced *nothing*, exhausting
  their budget just reading. It works when you **inline the relevant code into the prompt** and
  ask ≤3 focused questions. When it does work it is worth it: it found the verifier's false
  negative and #126's dialog-identity P0.
- **Git:** everything on `main`, pushed. Stage explicit paths, never `-a`. SSH-signed, no AI
  attribution trailers. The owner drives the app while you work.

## Diag levers (debug console only)

`PB_SHARP_DIAG`, `PB_DOOR_DIAG`, `PB_PERF`, `PB_THUMB_DIAG`, `PB_SCALE_POLICY=cpu`,
`PB_DERIVE_KERNEL`, `PB_DERIVE_MIP_BIAS`, `PB_POSTER_WALK=native|fitted`, `PB_AUDIO_TRACE`,
`PB_VIDEO_DIAG`, `PB_AV_SYNC`. Probes: `probe_one_file` / `ab_poster_walk` (ignored tests in
pb-decode; `PB_PROBE_FILE` / `PB_POSTER_AB_DIR`).

Corpus: `\\beenas\Media\Movies`, `D:\Media\Pictures\…\Wedding`,
`D:\Media\2002-password-is-test.7z` (encrypted, password `test`),
`D:\Media\test-archives\`, `D:\Media\Pictures.zip`.

## Known-red, NOT ours

- `pb-decode`'s `plain_fixtures_have_no_dovi_summary` — `FFmpeg decoder: Decoder not found`,
  pre-existing on both Windows runners.
- Two `pb-app-core` video-probe tests are **flaky**, not failing (timing-dependent off-thread
  probes; pass on an idle box).
- **Read step results, not job conclusions**, on this repo's CI right now.

## ⚠️ Task-ID collisions — re-fetch before filing

Happened once already (#115 filed twice; the poster refactor became #121). Highest id in use
is **#126**.
