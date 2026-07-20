# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-19 (rev 21). This session ran on the **macOS** side: #109 item 1 and
the #113 on-device verify are DONE, and the **#121 poster-walk DRY refactor** (owner-asked)
is underway — phases 1 and 3a merged to `main`. The parallel Windows agent landed #109.4 in
the same window; the two were merged (see the ID-collision note below)._
---

# ▶️ START HERE — #114 is DONE and on main (`a987d0d6` tip)

**The poster pipeline** (task #114, plan: `.taskmaster/plans/114-poster-pipeline-one-run.md`
rev 4 + its review log — 3 design review rounds + 3 implementation review rounds, all folded):

1. **One judged walk per movie per session.** The `PosterSelect` pool work kind (geometry-
   neutral, content-gen-keyed, level-triggered, thumb-cap→display class promotion) + the
   `PosterSelector` ledger on AppCore. Every consumer (display Fit, strip tile, parked
   Original) fans out from ONE typed payload.
2. **Fixed-size judging** (`POSTER_JUDGE_WIDTH` 256): the detail gate is resolution-
   independent for the grain/white-title-card class (the Ali Wong fix). ⚠ HONEST LIMIT
   (probed on The Holdovers): real MF-scaled pixels can still flip borderline picks between
   different-fit walks (39.29 s vs 81.0 s) — the ARCHITECTURE (one walk + surviving choice +
   replay) is the consistency guarantee, not the judge.
3. **The choice is a replayable locator** (`PosterChoice`, absolute `origin + relative` —
   MPEG-TS-correct, applied to the walk's own deep seeks too). Replay = fresh reader +
   decode-forward with identity enforcement (miss ⇒ error ⇒ scored-walk fallback). Measured:
   **273 ms** at 4K DoVi over SMB vs a 1.2–2.6 s walk.
4. **Walk variants A/B'd on the corpus** (48 films, counterbalanced): fitted vs native a wash
   at 1080p, native 15–20% slower at 4K, 0 pick mismatches ⇒ **fitted is the default**
   (`PB_POSTER_WALK=native` = the lever). `NATIVE_WALK_CAP` = 2 admission permits for
   native-class jobs (native walks + all replays).
5. **Videos resize like photos**: the native winner installs as `RepKind::Original` (mode-0
   only; mode-1/P3 memoized in `original_blocked`, their fp16 bake rides 110d) — parked
   videos pre-install via replay in spare capacity, then fullscreen/1:1 GPU-derives.
6. **Nav never blocks on a poster**: instant tile (the strip's own cached tile, staged
   straight into the upload queue) or the flat placeholder, upgraded in place. The
   selection's ready-made tile also lands DIRECTLY in the thumb cache the same tick as the
   poster (`a987d0d6` — the derive queue drops under burst and used to throw it away).
7. **Bounded retry** (all item kinds): one demand-re-entry second chance per failed item per
   session (`retry.rs`); recovery clears BOTH domains' gates. The old "one SMB hiccup blanks
   a tile forever" behavior is gone.
8. **BDMV `.m2ts` posters**: `MF_E_INVALID_POSITION` typed at the COM layer; the deep walk
   returns its head best instead of discarding it.

**Owner verdict:** "a good improvement — not perfect, sometimes still slow to load, but
limited by SMB over network among other things." ACCEPTED residual: first-visit walks over
SMB are 1–2.5 s by nature (once per movie per session); a cross-session on-disk poster cache
would fix cold starts but is a PRIVACY-CHARTER amendment (viewing-derived persistence,
ADR-018) — owner's call, deliberately not implemented.

**Also merged this session (earlier):** the whole #110/item-6/watchdog rendering arc (rev-19
notes), the branch cleanup (four stale branches deleted), and **79.10 NVDEC** from the wt1
worktree (seek convert-skip + HDR P010; its status note rode its own commits). The 79.10
rebase integration exposed + fixed a broken `run_video_producer` test call site
(`6b08e44a`).

## Next actions
1. **#121 phase 2 — the MF port — is OWNER-OWNED, on Windows.** It is the one phase this Mac
   cannot compile or run (`pb-app`/MF are Windows-only). The plan spells the contract:
   keep `scan`'s per-sample deadline check, map BOTH invalid-position sites, and make reader
   retirement `Drop`-based. Everything it needs already exists on `main`.
2. **#121 phases 3b / 4a / 4b** (FFmpeg deadline+interrupt classification; AVFoundation PTS
   plumbing; `av_poster` onto the driver = **#92.2**) — all doable on the Mac.
3. **Owner smoke of the FFmpeg poster path** (macOS MKV/WebM) after 3a — it is meant to be
   behavior-neutral; the one accepted delta is the driver's between-burst deadline check.
4. **#112 performance profiles** — design rev 4 IMPLEMENTATION-READY, paused at owner
   sign-off; do not implement without the go.
5. **#119 / #120** (new, from the Windows side): the toggle-storm root cause and the
   diagnostics panel. #119 is adjacent to the #113 derive work verified this session.
6. **#102 fuzz/bench subtask, door gating #105.2/#107** — the smaller owed items.

## ⚠️ Task-ID collision (2026-07-19) — read before filing a task
Both sides filed a **#115** concurrently: the Windows agent's "compcol RAR dependency" (which
kept the number) and this session's poster-walk refactor (**renumbered #121**). The phase-1
commit `7089c548` still says "#115 phase 1" — same work. **Re-fetch before picking an id.**

# ▶️ THIS SESSION (macOS) — what landed

- **#109 item 1 (`5089f36a`)** — the macOS shell now cross-cancels its two deck-installing
  workers (archive open ⇄ folder scan), closing mode B. Also fixed a stale `pb-mac-ffi`
  fixture that had been RED on `main` since `cd07a388` (`handle_on_a_door` never stamped
  `presented_epoch`, so the `door_presented()` gate hid the card).
- **#113 VERIFIED on Metal** — the #110/ADR-024 arc rides free on macOS, no porting:
  `[sharp-diag] GPU-derived Fit` fires on every fullscreen toggle, and `PB_SCALE_POLICY=cpu`
  is a clean A/B (zero derive lines, CPU re-decode at each new fit dim). All 69 `pb-render`
  tests pass headless on Metal, including the real GPU derive suite (they `.expect("no GPU
  adapter")` and diff GPU vs a CPU reference — so WGSL→MSL is genuinely correct, not merely
  compiling). ⚠ **Two traps for whoever re-runs this:** test #110 on a **JPEG** — a RAW is
  excluded from the parked tier, so a folder whose first item is a `.NEF` shows zero derive
  lines and looks broken; and `110-manual-test-script.md:74` was **stale** (it claimed macOS
  uses the `remap_ring` trait default — it does not: shared `rebuild_ring`
  (`app_core_impl.rs:8046`) calls `WgpuRenderer`'s override at `gpu.rs:3709`).
  NOT exercised: the watchdog (needs real hold-to-blaze pressure) and any crispness/feel
  judgment — those need the owner's eyes.
- **#121 phases 1 + 3a** — see below.

# 🧱 #121 — one poster walk (the DRY refactor the owner asked for)

Plan: `.taskmaster/plans/121-one-poster-walk-driver.md` (rev 2, Codex round 1 folded:
no P0, 5×P1 + 3×P2). **Why:** `deep_scan` and `Best` existed near-verbatim TWICE
(`mf_poster.rs:695`/`477` and `ffmpeg/poster.rs:352`/`122` — same comments word-for-word) and
not at all in `av_poster.rs`, and they drifted: two-scale scoring landed in FFmpeg only
(`cfd55ffe`), origin capture + judge-size scoring + the BDMV degrade in MF only (#114).
Addresses tech-debt-audit finding **#5**.

- **Phase 1 (`7089c548`)** — `pb-decode/src/poster_walk.rs`: the policy once, behind
  `PosterBackend` (duration/cancelled/seek/scan) + `Best<F>` + `SeekError` + `ScanOutcome`.
  24 tests where the policy had none, driven by a `FakeBackend` with no video in it.
- **Phase 3a** — the FFmpeg backend ported onto the driver; the scRGB judge hoisted into
  `video.rs`, so **every** poster threshold now lives there.
- **LOAD-BEARING:** `ScanOutcome{Good,Exhausted,Stop}` is not a bool because MF's typed
  `MF_E_INVALID_POSITION` degrade fires from **two** sites — the `SetCurrentPosition` seek
  (`mf_poster.rs:763`) *and* the post-seek `ReadSample` inside the burst (`:658`, caught at
  `deep_scan:752`). Collapsing the second into "found nothing" silently turns "stop, keep the
  head best" into "try the next offset". Codex caught this; the rev-1 design had it wrong.
- `video::rational_to_hns` is the one locator conversion (signed i128, saturating, `None` on a
  bad denominator — **never** a silent 0, and never `pts_to_duration`, which clamps and
  round-trips through f64).

## Diag levers (debug console only)
`PB_SHARP_DIAG`, `PB_DOOR_DIAG`, `PB_PERF`, `PB_SCALE_POLICY=cpu`, `PB_DERIVE_KERNEL`,
`PB_DERIVE_MIP_BIAS`, `PB_POSTER_WALK=native|fitted`, `PB_AUDIO_TRACE`, `PB_VIDEO_DIAG`,
`PB_AV_SYNC`. Probes: `probe_one_file` / `ab_poster_walk` (ignored tests, pb-decode;
`PB_PROBE_FILE` / `PB_POSTER_AB_DIR`). Corpus: `\\beenas\Media\Movies`,
`D:\Media\Pictures\…\Wedding`.

# 📓 Load-bearing knowledge (don't re-derive)

- **ADR-024 + #114 together**: previews/placeholders are transient; the display converges to
  a resident-Original derivation (photos AND parked videos now). The selection ledger is
  content-gen-fenced; its Fit artifact is `(epoch, FitBox)`-tagged; budget guards are CARVED
  per artifact (`synthetic_carved`).
- **The pool's selection contracts** (decode_pool.rs): epoch-exempt, gen-replaced,
  level-triggered (absorb_results at the top of every emission pass closes the
  sent-but-undrained double-walk window), promotion mutates queued class + notifies,
  `took_thumb_slot`/native permits are class-at-admission.
- **Replay identity is strict** (tolerance 0.5 s): a miss errors into a scored-walk fallback
  — never silently different pixels under the same locator.
- **`Outcome.preview` vs `img.is_preview`**, **present_slot-not-present_item for in-place
  upgrades**, **`prefetch_fulls()` order IS the decode priority** — all rev-19 rules stand.
- **Mode-1 (enabled-transform 8-bit) textures are unmipped and underive-able** by renderer
  design — anything wanting to derive must install mode-0 or fp16 mode-2 (110d).
- **Git:** everything is on `main`, pushed. Stage explicit paths, never `-a`. SSH-signed, no
  AI trailers. Codex review loop at section boundaries (owner expects verified anchors,
  multiple rounds). The owner drives the app while you work — confirm the About build id.

# ⏸ PARALLEL TRACK — Windows video/audio

79.10 NVDEC **shipped** (this session, wt1). Older notes stand: poster deep-walk done +
good-enough (#92.1; macOS half open); the pause/play audio gap was Bluetooth (NOT the app);
the FFmpeg→MF locator bridge stays dead. Diag + build rules unchanged (`build-windows.ps1
-Run`; a bare `cargo run` ships silent AC-3/DTS).

# 📌 macOS TODO — #113 (verify the rendering + poster arcs) + #109 item 1

Full details in tasks.json #113 + rev-19's section (in git history). Everything since (the
#114 arc) is ALSO shared-code: pb-app-core orchestration + `WgpuRenderer` + the shared walk
policy constants — EXCEPT `engine::poster_select_supported()` gates selection to Windows, so
macOS keeps legacy poster behavior until the parity pass (#114 phase 5) flips it after
on-device verification.
