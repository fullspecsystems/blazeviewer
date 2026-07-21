# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-20 (rev 28). Windows session; a macOS session worked in parallel through
the day. Everything below is on `main`, pushed (`git rev-list HEAD...origin/main` = 0/0)._

---

# ▶️ START HERE

**#130 — de-duplicate the media stacks (audit #5) — is DONE and pushed** (both parts):

- **`c6a5d0e8` Part A** — one `VideoProducerBackend` trait + shared `video_producer_loop::run<B>`;
  the MF and FFmpeg producers' ~180-line duplicated credit/seek loops collapse into one wrapper
  each. New deterministic **mock-backend loop test** (10 tests, the primary net) + the ~10 existing
  MF integration tests pass. Codex-reviewed, no defects.
- **`daac240d` Part B** — new **`pb-color`** micro-crate owns the YUV `(Kr,Kb)` table + `coeffs()`;
  pb-render re-exports it, pb-decode delegates. Byte-identical (all YUV tests pass).
- ⚠ **One cross-machine gap (see `## Handoff` in the #130 plan):** the FFmpeg backend's *runtime*
  behaviour is **unverified** — Windows has no FFmpeg video decoders, so its integration tests
  can't run here. **A macOS/Linux session must run the FFmpeg producer tests + real play/seek, and
  pb-render's golden-image tests** (both expected green; the changes are byte-identical by
  construction, but that is not a run).

**The next task is the NS0 shell de-dup (audit #1b/#2) — THE LIVE TASK.** Its plan is being written
**this session** at `.taskmaster/plans/131-ns0-shell-dedup.md` (Codex review pending). It is the
**cross-machine** refactor (touches `pb-mac-ffi`, an empty staticlib on Windows), so it is genuinely
riskier than #130 and needs the Mac available to close the loop. Read the plan in full, then start.
Pointers in §*NS0* below.

Read before writing any code: **`docs/where-code-goes.md`** — an ordered decision procedure for
where a function belongs. "Put it on `AppCore`" is the *last* answer. This is the doc NS0 leans on.

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

# 🔜 NS0 shell de-dup (audit #1b/#2) — THE LIVE TASK; plan `#131` being written this session

**Plan: `.taskmaster/plans/131-ns0-shell-dedup.md` — authored this session (Codex review in
progress). Read it in full before implementing.** Grounding sources (read both):

- **`technical-debt-audit.md`** — finding **#1(b)** ("finish the NS0 inversion so `AppCore` owns
  orchestration and the mirror flags have a single owner") and finding **#2** ("the two parallel
  platform shells").
- **`125-split-app-core-impl.md` §2 and §2a** — the scope-gate table (this is remediation *(b)*,
  deliberately separated from the *(a)* file-split we just finished) and the **exact residue**:
  **16 orchestration functions duplicated across the two shells** + `struct DirScan`/`struct
  ArchiveLoad` defined in both. §2a lists them verbatim (begin_dir_scan, poll_dir_scan,
  cancel_dir_scan, cancel_scan_command, scan_pill_visible, begin_archive_open, poll_archive_load,
  finish_archive_open, fail_archive_open, cancel_archive_load, prompt_archive_password,
  is_archive, apply_menu_state, confirm_delete_permanent, toggle_recursive, toggle_show_archives).
  The last four are the "strays" #126 deliberately deferred.

⚠ **This is the CROSS-MACHINE one.** It touches `pb-mac-ffi`, which compiles to an **empty
staticlib on Windows** — a Windows session cannot verify the Mac shell (see *Load-bearing
knowledge*). Plan for: do the core+winit side on Windows, leave a `## Handoff` for the Mac to
compile/verify. This is genuinely riskier than #130 and needs the Mac available to close the loop.
Confirmed still duplicated as of 2026-07-20: the 16 mirror fns exist in both `crates/pb-app/src/`
and `crates/pb-mac-ffi/src/` (the four strays live in both shells and *not* the core).

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
is **#130** (`.taskmaster/plans/`); highest in `tasks.json` is #127 — plans run ahead of task
entries, so check `ls .taskmaster/plans/` too before filing. NS0 will need the next free id.
