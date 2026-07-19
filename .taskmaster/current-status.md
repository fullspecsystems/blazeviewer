# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-19 (rev 18). **This handoff is the RENDERING-QUALITY track** — fullscreen /
resize / scale-mode sharpness. The Windows **video/audio** track (#5/#4/#1), the **macOS #106** perf
track, the **door gating** track (#105.2/#107), and the **macOS #109 port** run in parallel and are
**preserved below** — don't lose them, they're just not this thread's job._

---

# ▶️ START HERE — #110 + item-6 + the watchdog SHIPPED; owner manual verification is the next step

**Branch `feat/110-gpu-lanczos-from-original`** now carries the whole ADR-024 arc, built in one
autonomous overnight session (2026-07-18→19), every phase Codex-reviewed with findings folded in:

1. **The ADR-024 watchdog** (`21c9df9a` + review fixes): a displayed photo lingering as a resident
   preview past 2 s gets its full force-requested regardless of a stuck `held_nav` — level-triggered,
   armed only when caught-up + image + non-RAW, disarmed on deck rebuild. 9 fake-clock tests.
2. **#110 110a** (`c06e3d51`, `e6b49a94`): the odd-dim MIPGEN regression (pins that the box chain
   DROPS the trailing odd row/col — the old comment lied), and the two-pass scale-aware Lanczos
   derive (`DERIVE_WGSL`) with the §3b colour chain. Codex caught two real shader defects, both
   fixed + regression-tested: un-premultiply must divide by the UNCLAMPED filtered alpha, and fp16
   sources need Inf/NaN containment. 9 GPU derive tests incl. a CPU oracle.
3. **#110 110b** (`c4013e6d`, `38126061`): on a settled resize/toggle the core GPU-derives the
   current photo's exact Fit from its retained Original — **the ~1 s CPU re-decode is gone** when a
   mipped Original is resident. Reserve-then-derive with `release_pending` rollback; rotation/inset-
   aware target box; 256 MB scratch cap; `FULLSCREEN_SETTLE` 50 ms (a toggle is one event, not a
   drag stream). Levers: `PB_SCALE_POLICY=cpu`, `PB_DERIVE_KERNEL`, `PB_DERIVE_MIP_BIAS`.
4. **item-6 6a+6b** (`2a4b163b`, `9004ca4d`): `invalidate_geometry` **retains + remaps** resident
   Originals (`drop_fit_slots`+`compact_to`+new `Renderer::remap_ring`); content changes still purge
   (spec §4.1 invariant); the settle re-presents the retained Original; and `try_present_target`
   **derives a missing Fit from a retained neighbour Original on nav** — advance-after-toggle lands
   sharp, never the ~256 px preview flash. The spec's SatisfiedBy machinery proved unnecessary
   because #110 landed first. The undo-rotation-while-navigated-away content hole is closed.
5. **#110 110c** (`a9f1f11b`): the A/B/X harness (`cargo test -p pb-render --release -- --ignored
   ab_report --nocapture`): FLIP (nv-flip) + linear RMSE + detail ratio over 4 patterns × 8 ratios,
   vs a linear-light Lanczos reference. **Data picked the defaults: Lanczos-3 + mip_bias −1**
   (FLIP ≤ 0.012 everywhere; bias 0 goes soft above 2× and collapses at exactly 2×; L2 aliases 1-px
   diagonals). Two always-run regressions pin the derive-beats-trilinear and derive≡Lanczos-at-L0
   facts. Plus the mip plan §4d fixes: mip-inclusive slot bytes + `make_room_for_upgrade`.

**State:** all suites green (pb-app-core 747, pb-render 69+2 ignored, pb-core 98, workspace clean),
clippy `-D warnings` clean, ship-feature build (`build-windows.ps1`) green, CHANGELOG updated.

## Next actions
1. **Owner manual verification** — the full script is `.taskmaster/docs/110-manual-test-script.md`
   (fullscreen-toggle crispness, advance-after-toggle, rotation/HDR/ICC edges, watchdog stress,
   `PB_SCALE_POLICY=cpu` as the feel A/B). Run on the physical display, fresh build id.
2. **Phase 1b (display-capped pyramid budget)** — a reviewed DESIGN DRAFT at
   `.taskmaster/plans/110c-phase1b-display-capped-pyramid.md` (Codex review may still be landing;
   check the plan for its verdict note). Deliberately NOT implemented: it changes residency
   semantics (`Pyramid` vs `Original`), has zero effect on the 7680 display, and high value only
   for the small-machine story. Implement after owner sign-off on the design.
3. **110d (deferred, own plan):** mode-1 → fp16-pyramid conversion so ICC photos join the derive.
4. After the merge to `main`: mirror `remap_ring` on the macOS shell (it currently uses the
   drop-all trait default = pre-item-6 behaviour, no break).

## Prime directive held: MEASURE, don't guess
Every quality claim above has a number (the ab_report matrix) or a regression test. The kernel/bias
defaults were picked from measured FLIP data, not assertion. Codex reviewed every phase (watchdog
×2, 110a, 110b, the Phase-1b design); all P0/P1 findings are folded in and cited in commit messages.

## Diag levers (env-gated, debug build only — release has no console)
- `PB_SHARP_DIAG=1` — sharpen lifecycle + `GPU-derived Fit item=…` + `preview watchdog FIRED`.
- `PB_DOOR_DIAG=1` — draw source / backend / present diags. `PB_PRESENT_FIFO=1` — force Fifo.
- `PB_SCALE_POLICY=cpu` · `PB_DERIVE_KERNEL=2|3` · `PB_DERIVE_MIP_BIAS=0|-1` — the #110 A/B levers.
- `PB_PERF=1` — open→first-photo / resize→on-screen ms. Corpus: `D:\Media\Pictures\…\Wedding`.

# 📓 Load-bearing knowledge (don't re-derive)

- **ADR-024 is the organizing principle** — previews are blazing-only; the interaction display is a
  pure function of a resident Original pyramid. #110 (sampler) + item-6 (residency) + the watchdog
  (enforcement) now ALL exist. The Phase-1b cap is the last residency piece (design drafted).
- **The derive's colour chain is load-bearing** (gpu.rs `DERIVE_WGSL` doc): mips are straight-alpha,
  mode-0 sRGB-encoded; premultiply after EOTF; fp16 premult-linear intermediate; un-premultiply
  ONCE by the UNCLAMPED filtered alpha; straight-alpha finals. Don't "simplify" any of it.
- **The Held derive source is only valid for the displayed photo** — on nav it is the previous
  photo's texture (pinned by `a_nav_derive_never_sources_the_previous_photos_held_frame`).
- **Retain iff `content_gen` unchanged** — geometry retains Originals, content purges everything
  (`rebuild_ring(retain)`); `invalidate_content` must NEVER call the retaining path.
- **The rev-15 "surface drops presents" theory stays DISPROVEN**; backend is **Vulkan + Fifo** on
  the owner's RTX 5090. Mixed-DPI/RDP memory applies to any fullscreen/DPI bug.
- **Git topology:** everything above is branch-only until #110 merges to `main`. **Stage explicit
  paths, never `-a`/`-A`** (owner edits concurrently). SSH-signed commits, no AI trailers.
- **Codex CLI reviews are part of the loop now** (owner instruction 2026-07-18): `codex exec
  --sandbox read-only "<scoped prompt>"` at each section boundary + for complex plans; keep prompts
  scoped; long reviews run in background (10 min+ is normal).
- ⚠ **The owner drives the app while you work** — a running exe locks `target\debug\blazeviewer.exe`;
  confirm the About build id after any rebuild. Debug builds have a console; release does not.
- **Repo:** `github.com/fullspecsystems/blazeviewer`; product name **Blaze Viewer**.

---

# ⏸ PARALLEL TRACK — Windows video/audio (#5 / #4 / #1) — NOT this thread

_The `feat/audio-track-selection` arc, all merged to main. Still real open work._

**Shipped:** FFmpeg-first film audio on Windows (MF can't decode AC-3/E-AC-3/DTS `0xC00D36B4`);
audio-track selection (`A`/`Shift+A` + Playback ▸ Audio Track); `WAVEFORMATEXTENSIBLE` speaker-mask
sinks; off-thread track switches; short-forward-hop for seeks (+2 s tap over SMB ~1 s → **139 ms**);
adaptive audio-seek settle (172 ms → ~10 ms). **⚠ The FFmpeg→MF locator "bridge" was a MISTAKE,
DELETED** (regression test `audio_rows_keep_their_ffmpeg_locators`) — do NOT reintroduce.

**Open:**
- **#5 — pause/play audio gap (owner HIGH).** `P` pause→play leaves a multi-second gap before audio
  resumes. **Not yet investigated.** Trace: `poll_video` → `CoreEffect::ResumeVideoAudio` →
  `WasapiAudio::resume()` → engine `Cmd::Resume` → `sink.client.Start()`. Top suspect: resume
  preroll waits on audio-ready (`video_session.rs` `preroll_satisfied`). Measure first
  (`PB_AUDIO_TRACE`). Owner runs over SMB (`\\beenas\Media\Movies`).
- **#4 — the 10 s Shift-seek gap** (~1.2–1.6 s): MEASURED as video recreate+run-up; real fix is the
  79.10 NV12 path. RECOMMENDATION: defer; scope with 79.10.
- **#1 — MF poster deep-walk** (Windows video posters pure black, luma 0.000). Owner-approved fix
  scoped in tasks.json #1: port `mf_poster.rs` to the `ffmpeg/poster.rs` reference (scored
  best-so-far walk + deep seek 8/20/45/90 s + reader-recreate per offset + 15 s deadline).

**Video/audio load-bearing:** WASAPI reseek ~10 ms; MF enumerates audio streams in a different
order than FFmpeg (the `ff`/`mf` two-currency locators); Windows audio is the master clock while
playing; a `Failed` clock is terminal. **Build:** `pwsh scripts/build-windows.ps1 -Run` — a plain
`cargo run` omits `ffprobe` → AC-3/E-AC-3/DTS films play SILENT. Diag: `PB_AUDIO_TRACE`,
`PB_VIDEO_DIAG`, `PB_AV_SYNC`.

# ⏸ PARALLEL TRACK — macOS #106 performance (NOT this thread)

Blueprint: `.taskmaster/plans/106-performance-archive-zoom.md` (rev2, Codex-reviewed). The
Windows/rendering side of #106.7 is now far ahead (this session: item-6 retain/remap + the #110
derive). The mac host still owns its `PB_PERF` baselines. Shipped historically: door card
(`d91666a0`), perf timers (`fdcedd16`), #106.2 read/decode split (`5d8eebe1`).

# ⏸ PARALLEL TRACK — Windows door gating + Copy (#105.2 / #107)

Door renders correctly. Owed: gate OCR/Describe/Compare **off** on a door (`MenuState._enabled`);
#107 relabel "Copy Image"→"Copy" + emit file-only on a door; interactive smoke tests.

---

# 📌 macOS TODO — port the cross-type open-race fix (task #109)

**For a macOS agent.** The archive/folder open-race was root-caused + fixed on Windows
(`8293a662`). The **core** half (`apply_scan_batch` extend-guard) is shell-neutral; the **shell**
half was only done in the winit shell. Mirror the two winit edits into
`crates/pb-mac-ffi/src/lib.rs` (inspection-only — can't build `pb-mac-ffi` on Windows):

1. **`begin_archive_open`** (~2877): after it cancels a prior archive (~2891), also
   `self.cancel_dir_scan();` (the mac `cancel_dir_scan` ~2864 already nulls `dir_scan`).
2. **`begin_dir_scan`** (~2744): after the `Source::Scan` match,
   `if let Some(prev) = self.archive_load.take() { prev.progress.request_cancel(); }`.

Authoritative reference: `git show 8293a662 -- crates/pb-app/src/main.rs`. **Also new since
item-6:** mirror `Renderer::remap_ring` on the mac shell when porting (it currently inherits the
drop-all trait default — correct but pre-item-6 behaviour).

**Deeper #109 hardening (deferred, both shells, Codex-recommended):** one shared open generation;
`content_gen`/`deck_gen` in `DecodeKey`; `upload_slot`/`mark_resident` return-checked;
`present_item` returning success + abort-drain-then-resync-once.
