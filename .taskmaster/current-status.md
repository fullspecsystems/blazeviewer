# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-20 (rev 27). Windows session; a macOS session worked in parallel through
the day. Everything below is on `main`, pushed (`git rev-list HEAD...origin/main` = 0/0)._

---

# ▶️ START HERE

The god-object refactor arc is **DONE**: #124, #125, #126, #128 all complete. `app_core_impl.rs`
went **22,105 → 8,286 lines** and its `mod tests` is now charter-only. See *Completed arc* below.

**The next task is #130 — de-duplicate the media stacks (audit #5).** The plan is written,
Codex-reviewed, and folded (`.taskmaster/plans/130-media-stack-dedup.md`). It is **ready to
implement**; the owner was reviewing before giving the final go. **A fresh session should read
that plan in full, then start Part A step 1.** Everything you need to begin is in §*#130* below.

**Tomorrow, probably: the NS0 shell de-dup** (audit #1b/#2) — the cross-machine one. **It has no
plan yet**; it must be written first. Pointers in §*NS0* below.

New this arc, read before writing any code: **`docs/where-code-goes.md`** — an ordered decision
procedure for where a function belongs. "Put it on `AppCore`" is the *last* answer. Linked from
`CLAUDE.md` → *Working norms*.

---

# 🎯 #130 — media-stack de-dup (audit #5) — THE LIVE TASK, plan ready

**Plan: `.taskmaster/plans/130-media-stack-dedup.md` (rev 2, Codex-reviewed). Read it in full
before starting — this summary is the map, not the territory.**

### What it is, honestly scoped (from a full read of the code, not the audit)

The audit lumps two things under finding #5; they are **very** different in size:

- **Part A — a `VideoProducerBackend` trait (the real prize).** The two ~1,300-line producers
  (`crates/pb-decode/src/mf_video_producer.rs` = Media Foundation; `crates/pb-decode/src/ffmpeg/
  video_producer.rs` = FFmpeg) each carry a ~180-line credit/seek loop that is **near-verbatim
  duplicated**. An agent mapped it precisely (in the plan §4): the select (S1) and *all* the
  credit / generation / seek-epoch machinery are character-for-character identical; the only
  divergence is a **~9-operation reader seam** that becomes the trait. **FFmpeg's existing
  `Reader` struct is already ~90% of that trait.** Win: ~350 duplicated lines collapse into one
  `fn run<B: VideoProducerBackend>`.
- **Part B — a shared YUV color primitive (small, ~30 lines).** The audit calls YUV
  "triplicated, correctness-critical" — but `pb-decode/src/yuv.rs` and `pb-render/src/yuv.rs` are
  **different converters** (AVIF-decode→RGBA8 vs video-render NV12/P010 with HDR) that overlap
  **only** in ~6 luma constants (`Bt601 (0.299,0.114)`, `Bt709 (0.2126,0.0722)`, `Bt2020
  (0.2627,0.0593)`) + the `coeffs()` derivation. And pb-render *already* guards drift with an
  independent-from-spec golden test. So Part B is a cheap constants-consolidation, **not** a
  correctness rescue. One open design call: where the shared constants live — a new `pb-color`
  micro-crate (the plan's lean) vs a `color` module in pure-but-nav-themed `pb-core`. Decide
  before writing code.
- **Part C — posters (3 extractors) + audio decoders (2) — DEFERRED.** Noted in the plan §7,
  not committed; needs its own mapping after A+B land.

### ⚠ The two things that make this different from #125/#128

1. **This is NOT a pure move — no byte-hash safety.** It's a behavioural refactor (extract a
   trait, fold two loops into one). `verify-pure-move.py` does **not** apply. The **primary
   safety net is a NEW mock-backend unit test** the trait makes possible: the loop becomes
   drivable by a fake backend with no MF/FFmpeg and no video file, so the seek/credit/generation/
   park logic is tested deterministically and cross-platform. Plan §3 — write it **before**
   porting the second backend, assert backend **call-order** (not just events), use timing hooks
   for the generation race, and script reordered/keyframe timestamps (Codex's #1 hole is a
   one-frame-wrong seek landing that a clean CFR mock misses).
2. Secondary net: the **~10 existing MF integration tests** (`mf_video_producer.rs`, they
   `spawn()` the real producer against a fixture video) must pass **unchanged**. Tertiary:
   manual play/seek on `\\beenas\Media\Movies`. ⚠ The real-video probe tests are **known-flaky**
   (see *Known-red* below) — never call the refactor "verified" off a green `cargo test` alone;
   layer 1 (the mock test) is what makes a green run mean something.

### Sequencing (Part A — plan §4a)

1. Lift the FFmpeg loop **verbatim** into `fn run<B>` (its `reader.*` calls become `backend.*`
   trait calls); FFmpeg's `Reader` gets `impl VideoProducerBackend` (near-trivial bodies);
   `run_ff_video_producer` becomes a 3-line wrapper. Verify: FFmpeg path + app unchanged.
2. Write the mock-backend loop test against the now-existing trait (pins the contract).
3. Port MF to `impl VideoProducerBackend` — its free fns + loop-locals become a backend struct;
   absorb the `Gap` retry and `&mut kind` stride threading *inside* it. Verify: the ~10 MF spawn
   tests pass unchanged + seek-heavy app run.
4. Delete the two dead loop copies.

One backend per commit. This whole task is **Windows-verifiable** (both backends compile here) —
no cross-machine blind spot, unlike NS0.

### Codex-review design decisions already folded in (plan §7a)

- Teardown → `impl Drop`, **not** `close(self)` (MF's mandatory off-thread retire can't be skippable).
- `open()` runs **on the producer thread** (`!Send` backends never cross `thread::spawn`).
- The FFmpeg planar-prime (must-decode-first-frame-to-pick-format) stays **inside the backend** as
  an `InitialState { NeedRead, Ready{ts,pixels}, Eos }` enum, **not** exposed from `open()` — this
  keeps the shared loop unaware of pixel-format probing (the biggest design win of the review).
- Trait shape, the exact differing-op table, and the ranked "biggest holes" are all in the plan.

---

# 🔜 NS0 shell de-dup (audit #1b/#2) — TOMORROW, needs a plan first

**There is NO plan file yet — one must be written before implementing.** The refactor is
described in two places (read both):

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
