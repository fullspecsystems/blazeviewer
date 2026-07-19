# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-19 (rev 20). Two major arcs MERGED to `main` + pushed this session:
the **#114 one-run poster pipeline** (this thread) and **79.10 NVDEC hardware decode** (the
parallel wt1 agent). The macOS ports (#113, #109) and the #112 profiles design are the open
threads._

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
1. **Owner continues smoking main** (rebuild!). Known-accepted: SMB-bound first walks.
2. **#112 performance profiles** — design rev 4 IMPLEMENTATION-READY per its review log, but
   paused at owner sign-off (3 Codex rounds folded); do not implement without the go.
3. **#113 macOS on-device verify** (+ #109 item-1 parity edits) — the rendering arc rides the
   shared `WgpuRenderer`, unverified on Metal. #114 phase 5 (optional: `DecodeError::Cancelled`
   variant, mac poster parity via the shared walk constants) belongs to this trip.
4. **#102 fuzz/bench subtask, #92 macOS poster half, door gating #105.2/#107** — the smaller
   owed items (rev-19 audit stands).

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
