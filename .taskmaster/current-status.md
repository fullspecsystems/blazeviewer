# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-21 (rev 32). **#131 DONE both platforms** (macOS `ShowDeleteConfirm` wired,
lever removed). **#109 ring-bridge close-out IMPLEMENTED** (B + A, Codex-reviewed plan) — the durable
fail-at-divergence fix for the door-card race (#132). Everything on `main`, pushed
(`git rev-list HEAD...origin/main` = 0/0)._

---

# ▶️ #109 — ring-bridge close-out: IMPLEMENTED (status `review`, 2026-07-21)

Finished the audit's finding #3 (core↔renderer ring fragility) + the durable fix for **#132** (the
door-card wrong-occupant race). Plan + full ledger: `.taskmaster/plans/109-ring-bridge-closeout.md`
→ `## Handoff`. Three pieces:

- **C — verified already closed** (no code): #126's shared `BackgroundOps` generation + `poll_dir_scan`'s
  `is_current` gate already drop any stale/cross-type scan batch before `apply_scan_batch`; the audit's
  "mode B" hole is stale.
- **B (`3bc3d027`)** — `present_item` **atomic** (commits state only on a verified bind), returns `bool`,
  and **recovers** on a refusal (`ResidentRing::evict_slot` + `request_prefetch`, no epoch/gen bump).
  This fixes a **live** bug: `present_item` used to advance the title / `displayed_item` / `mark_resolved`
  even when the renderer refused — the "title advances but the view is frozen" corruption.
- **A (`d51bb3b5`)** — `pb_core::SlotIdentity` stamps every renderer `RingSlot` (from the decode
  outcome's key, never a live `content_gen`); `present_slot(slot, expected)` **refuses a wrong occupant**
  → fail-at-divergence instead of silent stale pixels.

**Verified (macOS):** pb-core + pb-app-core suites green (bar the 2 documented-flaky video-probe timing
tests — confirmed flaky-not-regression); clippy + fmt clean; `pb-mac-ffi` builds clean. **The ONE open
gate:** the winit `pb-app` build/run on **Windows** — safe by construction (pb-app has **zero**
references to the changed `Renderer` methods; no `AppCore` field added), but a real build is the gate
(the Mac can't compile `pb-app`). See the plan `## Handoff`.

**#132** is now `review` — A+B are its durable fix; re-check against a big-still-scanning-folder repro
if one can be captured (the invariant/recovery tests are the proof either way).

---

---

# ▶️ START HERE

**#131 — the NS0 shell de-dup (audit #1b/#2) — DONE on BOTH platforms and pushed.** Windows landed
the four `ShellFlowAction` inversions (five commits); the macOS session (this one) then wired the new
`ShowDeleteConfirm` effect into the native shell, removed the `cfg(macos)` lever, and deleted every
now-dead arm/body. Plan + full ledger: `.taskmaster/plans/131-ns0-shell-dedup.md` → **`## Handoff`**.

**Only two things remain, both owner GUI smoke-runs (behaviour gates, not code):**
- **Windows (winit):** `pwsh scripts/build-windows.ps1 -Run` → Ctrl+R recursive on/off, View ▸ Show
  Archives on/off, File ▸ Stop Scanning (menu + scan-pill Cancel → "Scan stopped" toast), Shift+Del
  (confirm names the file).
- **macOS (native):** launch `Blaze Viewer.app` → the same four (recursive toggle, Show Archives
  toggle, Stop Scanning → pill Cancel toasts, Shift+Del → native confirm sheet names the file + Yes deletes).

**macOS close-out commit (this session):** the render arm in `next_effect`, the lever removal in
`delete.rs`, −119 lines of dead shell bodies, `scan_pill_cancel` re-routed through the core, two stale
comments fixed + the delete test strengthened (Codex round-2). All `pb-mac-ffi` (39) + core delete
tests pass; Swift host builds/links clean; clippy + fmt clean on new code.

**Filed during smoke-testing → #132 (pending):** an **intermittent door-card present/ring race** —
opening a no-images archive door then advancing leaves the door card stuck over the next photo; and
backward-nav onto a door sometimes fails to show the card. Diagnosed as a pre-existing present-timing
bug (the "cross-deck open race" family, `app_core_impl.rs:962`), **not** from #131. Could not get a
reliable repro this session (timing-dependent; likely needs a big, still-scanning folder). Full
diagnosis + `PB_DOOR_DIAG` capture recipe: `.taskmaster/plans/132-door-card-present-race.md`.

---

## 📌 Historical: the Windows-side ledger (kept for reference)

**#131 Windows-side DONE and pushed** (five commits). All four threads inverted the last cross-shell
`ShellFlowAction` orchestration into the core.

- **`41c37afe` Thread B** — `archive_loading` mirror flag → `AppCore::archive_loading()` getter;
  `scanning` doc fixed; `launching`/`dialog_open`/`redraw_pending` documented as intentional signals.
- **`a5476e28` A.1** — `Recursive`/`ShowArchives` → `AppCore::toggle_recursive`/`toggle_show_archives`
  (+ shared `current_photo_cursor`/`emit_rescan`/`rescan_current_folder`; emit `BeginDirScan` effect,
  `persist_prefs`-gated save). winit bodies + flow arms deleted.
- **`30fd74ea` A.2** — `CancelScan` → core dispatch (cancel + toast, **no dialog effect** — the
  unscoped-`CloseDialog` was a bug). scan-pill Cancel routed through `dispatch_action`; winit wrapper deleted.
- **`a4a06fc8` A.3** — `DeletePermanent` → `AppCore::request_delete_confirm` + new
  `CoreEffect::ShowDeleteConfirm { name }` (name snapshotted, avoids the struct-literal trap); **all three**
  emission sites converted; **macOS lever live** (`cfg(macos)` keeps the legacy path). winit renders the
  new effect; `confirm_delete_permanent` deleted.
- **`74f0fa03` docs** — `ShellFlowAction` seam now carries only `Quit` + `ToggleToolbar`.

**Verified (Windows):** `cargo test -p pb-app-core --lib` = **894 pass** (11 new tests, incl. the
CancelScan unscoped-close guard + the A.3 `do_delete` fallback); `cargo test -p pb-app` = **80 pass**;
clippy + fmt clean on new code. **Owner still owes a live winit RUN** of the four flows (Handoff item 1).

## ✅ macOS close-out — DONE (this session, 2026-07-21)

The A.3 render arm + lever removal + dead-arm cleanup all landed. Details in the #131 plan `## Handoff`
(items 2-5 struck) + §7b (Codex round-2). Summary:

- **A.3 rendered:** `C::ShowDeleteConfirm { name }` arm in `next_effect` composes Finder's wording, sets
  `shown_dialog`/`dialog_open`, returns `ShowDialog(Confirm)`. Lever removed from `delete.rs`
  (`request_delete_confirm` is now single-path on every platform); core delete tests un-gated + green on
  macOS; the FFI delete test strengthened + green.
- **−119 lines dead code:** the macOS `confirm_delete_permanent` / `toggle_recursive` /
  `toggle_show_archives` / `cancel_scan_command` bodies and the four dead `ShellFlowAction` arms.
  `scan_pill_cancel` (the one live caller) re-routed through `core.dispatch_action(Action::CancelScan)`,
  matching winit.
- **Verified:** A.2 Scanning-Cancel still uses `DialogResult::ScanningCancelled`; `archive_loading` getter
  reports true. Swift host builds/links clean; 39 FFI tests + core delete tests pass; clippy + fmt clean.

**Remaining = the two owner GUI smoke-runs listed in START HERE** (winit on Windows, native on macOS) —
behaviour gates, not code.

**#130 — media-stack de-dup (audit #5) — DONE + pushed** (`c6a5d0e8` + `daac240d`), below. ⚠ Its one
open cross-machine gap still stands: the **FFmpeg backend runtime is unverified** (Windows has no FFmpeg
video decoders) — a macOS/Linux session must run the FFmpeg producer tests + real play/seek + pb-render's
golden-image tests (byte-identical by construction, but not a run). See the #130 plan `## Handoff`.

Read before writing any code: **`docs/where-code-goes.md`** — an ordered decision procedure for
where a function belongs. "Put it on `AppCore`" is the *last* answer. This is the doc NS0 leaned on.

---

# ✅ #130 — media-stack de-dup (audit #5) — DONE, pushed (`c6a5d0e8` + `daac240d`)

Plan `.taskmaster/plans/130-media-stack-dedup.md` (has the full design + Codex round-1 fold-ins).
Both parts landed this session, verified on Windows (clippy + fmt clean, consumers compile):

- **Part A** (`c6a5d0e8`) — `crates/pb-decode/src/video_producer_loop.rs`: the `VideoProducerBackend`
  trait + shared `run<B>` credit/seek loop. FFmpeg `Reader` and a new `MfBackend` both `impl` it;
  each `run_*` is a thin open-then-delegate wrapper. **10 deterministic mock-backend tests**
  (call-order + the ranked seek/supersede/park/prime holes) + the **10 real-MF integration tests**
  pass. Two deliberate, plan-sanctioned nuances: FFmpeg zero-frame-planar EOS defers to the first
  credit (`InitialState`, §5.1); MF `Gap` retried inside the backend (§5.2). **Codex round-2
  reviewed → no defects** (confirmed behaviour-equivalence, `invalidate_primed`/`is_parked`
  correctness, MF no-double-retire/leak).
- **Part B** (`daac240d`) — new **`crates/pb-color`** micro-crate (owner-chosen home): `YuvMatrix` +
  `kr_kb()` + `coeffs()`. pb-render re-exports it (coeffs moved verbatim); pb-decode's `Matrix::kr_kb`
  delegates. **Byte-identical** — pb-decode's 12 AVIF YUV tests + pb-render's coeffs round-trip pass.
  WGSL shader keeps its own copy, still golden-test-guarded (no codegen, per plan).
- **Part C** (posters ×3 + audio decoders ×2) — still **DEFERRED** (plan §7); its own future task.

⚠ **`## Handoff` (the one cross-machine debt):** the **FFmpeg backend runtime is unverified** —
Windows has no FFmpeg video decoders (the pre-existing "Decoder not found"), so
`ffmpeg::video_producer::tests` can't run here (they compile clean). **A macOS/Linux session must**:
(1) run those FFmpeg producer integration tests + a real play/seek/EOS/replay pass on a corpus clip;
(2) run **pb-render's golden-image tests** (the YUV change is byte-identical by construction, but
that's an argument, not a run). Both expected green.

---

# ✅ #131 — NS0 shell de-dup (audit #1b/#2) — Windows-side DONE + pushed (Mac owns A.3 close-out)

_Landed this session (see START HERE + the plan `## Handoff`). The design write-up below is kept for
reference — it is the territory the four inversions followed._

---

# 📐 #131 design reference (as-planned)

**Plan: `.taskmaster/plans/131-ns0-shell-dedup.md` (rev 2, Codex round-1 folded in). Read it in full —
it is the territory; this is the map.** Investigation-grounded (two shell-mapping agent sweeps
2026-07-21, cited in the plan).

⚠ **The audit's "16 duplicated functions + `struct DirScan`/`ArchiveLoad` in both shells" framing is
STALE — do not plan against it.** Re-verified against current code: **`DirScan`/`ArchiveLoad` are already
gone from both shells** (#126); **8 of the 16 are already thin core-delegating adapters**; 2 are
legitimately per-shell. **The real, small residue** (plan §1):

- **Thread A — invert the four *cross-shell* `ShellFlowAction` flow arms** into the core:
  - **A.1 `Recursive` / `ShowArchives`** → `AppCore::toggle_recursive` / `toggle_show_archives` (pure;
    the biggest win). Core runs them directly + stops emitting the effect; mac arms go dead (harmless).
  - **A.2 `CancelScan`** → core runs `cancel_scan_command` + toast, **no dialog effect** (unconditional
    `CloseDialog` was a bug — winit has no scanning dialog now, the pill replaced it).
  - **A.3 `DeletePermanent`** → `AppCore::request_delete_confirm` + a new `CoreEffect::ShowDeleteConfirm
    { name }`; convert **all three** emission sites (dispatch + `delete.rs:35`/`:90`).
- **Thread B — collapse the redundant `archive_loading` mirror flag** to a core getter (`archive_load.
  is_some()`); fix the stale `scanning` doc (leave it a field). (`launching`/`dialog_open`/`redraw_pending`
  stay — legitimate one-way shell signals. `Quit` + winit-only `ToggleToolbar` stay on the seam.)

**Sequencing (plan §5): Thread-B → A.1 → A.2 → A.3.** One commit per arm.

⚠ **Cross-machine — but Windows can do ALL FOUR (plan §6a).** `pb-mac-ffi` is an empty staticlib on
Windows. Key finding: macOS's `next_effect` has a catch-all (`other => map_effect(other)`), so adding
`ShowDeleteConfirm` **compiles on macOS but silently no-ops** until the Swift side renders it. The **plan
of record: Windows does all four threads and never edits `pb-mac-ffi`**, using a **target-scoped lever**
for A.3 (emit legacy `ShellFlowAction(DeletePermanent)` on `cfg(target_os="macos")`, the new effect
elsewhere) so macOS stays green *and functional* at every push. **The Mac's entire job is one additive
Handoff item:** wire `ShowDeleteConfirm` into `map_effect`/Swift, flip the `cfg(macos)` lever onto it,
run the delete flow + the macOS delete test. (Dead-arm cleanup is non-blocking.)

---

# ✅ Completed arc (all DONE, pushed — compressed to pointers)

- **#128 — migrate `app_core_impl` tests to their concern modules** (plan
  `128-migrate-app-core-tests.md`, `## Outcome`). Finished #125's §4/§6 intent: #125 moved the
  *methods*, #128 moved each concern's *tests* beside them, and a cleanup pass then moved 17
  second-pass concern-belongers so the parent `mod tests` is **charter-only** (115 tests). Shared
  fixtures + shared non-fn stubs (`FakeArchive`/`DeriveOk`/`StashOk` `ItemSource`/`Renderer`s,
  `ARCHIVE` const) live in `app_core_impl/test_support.rs` as `pub(super)`. Safety was the
  test-name **multiset** (not set) from `cargo test -- --list` + `ignored` count + byte-hash +
  suite. Lessons (all in the plan): verify fixture tiers by call site not name; non-fn items are
  invisible to both the fn-mover and the byte-hash (manual moves, carry `#[derive]`); use `cargo
  fix --tests` for import cleanup (compiles first, unlike regex); read clippy's warning LOCATION
  (parent vs concern) before deleting an import.
- **#125 — split `app_core_impl.rs`** (plan `125-split-app-core-impl.md`). 26 concern-scoped
  `impl AppCore` files; the residency/present engine is a deliberate STOP (§7 step 5), a separate
  task if ever. Verified both platforms (Mac gated methods compile/link/launch). Ten hard-won
  traps are in the plan §3c + "Traps 7-10"; the `verify-pure-move.py` script + scratchpad helpers
  are the keepers if the engine split is ever taken up.
- **#126 — DRY the two shells' dir-scan + archive-open** (plan `126-...`). One tested core impl
  (`background.rs` + `dir_scan.rs` + `archive_open.rs`), ~−800 lines. ⚠ Do **not** "fix" the
  residual Opening-dialog boundary flash with a minimum display duration — rejected; rationale in
  `LOADING_DIALOG_DELAY`'s doc comment.
- **#124 — zoom binds the resident Original** (plan `124-...`, owner-verified). House rule that
  came out of it and still governs the residency engine: **background work may change residency
  or quality, never the presented representation.**
- **#127 (other machine) — lenient decode / recovery ladder for malformed images** — landed
  during this arc; the "burning-Polaroid" decode-error placeholder + a "Recovered" details row.
- **#129 (other machine) — macOS Thumbnails folder breadcrumb** — landed during this arc.

---

# 📓 Load-bearing knowledge (don't re-derive)

- **Cross-machine:** `CLAUDE.md` → *Working across two machines*. One `## Handoff` section per
  plan is the only place live cross-machine state lives. Never mark verified what you could not
  run. ⚠ **`pb-app` builds `AppCore` as a struct literal; `pb-mac-ffi` uses `AppCore::new_host`**
  — adding an `AppCore` field breaks winit and *not* the Mac, with no warning on the Mac. It has
  broken `main` once.
- **`pb-mac-ffi` is `#![cfg(target_os = "macos")]`** — on Windows it compiles to an empty
  staticlib, so a syntax error in it produces **zero** errors. Mac-shell edits are unverifiable
  from Windows (this is why NS0 above is the risky one). The reverse holds too: `pb-app`'s
  `build.rs` hard-errors on macOS; the Mac cross-checks winit via `x86_64-pc-windows-msvc` (needs
  `ureq` at `default-features = false` **plus `features = ["json"]`**).
- **Windows build:** `pwsh scripts/build-windows.ps1 -Run` (ship features `libheif,dav1d,ffprobe`).
  A bare `cargo run` omits `ffprobe` → silent AC-3/DTS. ⚠ A **running exe blocks the linker**
  (`Access is denied`, os error 5) — close the app before rebuilding.
- **Codex:** unreliable on this repo's big files (2 of 4 runs produced *nothing*, exhausting
  budget just reading). Works when you **inline the relevant code into the prompt** and ask ≤3
  focused questions. When it works it earns its keep — it found the #125 verifier's false
  negative, #126's dialog-identity P0, and the #130 `InitialState`/`Drop` design fixes. `codex
  exec --sandbox read-only "<prompt>"` from Git Bash.
- **Git:** everything on `main`, pushed. Stage explicit paths, never `-a`. SSH-signed, no AI
  attribution trailers. The owner drives the app while you work (so `git add -A` can capture their
  edits — always stage explicit paths).
- **`python -c "..."` one-liners are unreliable in this Bash tool** (a shell wrapper injects
  `|| goto :error`, breaking indented Python). Use a `python - <<'EOF'` heredoc instead.

## Diag levers (debug console only)

`PB_SHARP_DIAG`, `PB_DOOR_DIAG`, `PB_PERF`, `PB_THUMB_DIAG`, `PB_SCALE_POLICY=cpu`,
`PB_DERIVE_KERNEL`, `PB_DERIVE_MIP_BIAS`, `PB_POSTER_WALK=native|fitted`, `PB_AUDIO_TRACE`,
`PB_VIDEO_DIAG` (relevant to #130 — the producer seek/credit diag), `PB_AV_SYNC`. Probes:
`probe_one_file` / `ab_poster_walk` (ignored tests in pb-decode; `PB_PROBE_FILE` /
`PB_POSTER_AB_DIR`).

Corpus: `\\beenas\Media\Movies` (video — the #130 seek/play corpus),
`D:\Media\Pictures\…\Wedding`, `D:\Media\2002-password-is-test.7z` (encrypted, password `test`),
`D:\Media\test-archives\`, `D:\Media\Pictures.zip`.

## Known-red / flaky, NOT ours (matters for #130)

- **Two `pb-app-core` video-probe tests are flaky**, not failing (timing-dependent off-thread
  probes; pass on an idle box). **Directly relevant to #130** — the producer integration tests
  are the same kind of timing-dependent real-video tests, which is exactly why the plan's
  primary net is the deterministic mock-backend loop test, not these.
- `pb-decode`'s `plain_fixtures_have_no_dovi_summary` — `FFmpeg decoder: Decoder not found`,
  pre-existing on both Windows runners.
- **Read step results, not job conclusions**, on this repo's CI.

## ⚠️ Task-ID collisions — re-fetch before filing

Happened once already (#115 filed twice; the poster refactor became #121). Highest plan id in use
is now **#131** (NS0 = `131-ns0-shell-dedup.md`); highest in `tasks.json` is #127 — plans run ahead
of task entries, so check `ls .taskmaster/plans/` too before filing the next one.
