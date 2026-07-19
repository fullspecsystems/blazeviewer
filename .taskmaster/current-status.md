# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-19 (rev 19). **This handoff is the RENDERING-QUALITY track** — fullscreen /
resize / scale-mode sharpness. The Windows **video/audio** track, the **macOS #106** perf track,
the **door gating** track (#105.2/#107), and the **macOS ports** (#113, #109) run in parallel and
are **preserved below** — don't lose them, they're just not this thread's job._

---

# ▶️ START HERE — the whole rendering arc is MERGED TO MAIN (2026-07-19)

**`feat/110-gpu-lanczos-from-original` fast-forwarded into `main` (22 commits) and pushed on
2026-07-19.** The branch ref is kept for a few days as a rollback handle (it is not >2 days old;
the owner's cleanup rule). Merged-and-stale branches `feat/rar4-viewing`, `feat/enhanced-archives`,
`feat/media-track-catalog`, `feat/subtitle-display` were deleted local+remote;
`feat/audio-track-selection` (fully merged, ~1.7 days old) survives one more sweep.

The arc, every phase Codex-reviewed with findings folded in:

1. **The ADR-024 watchdog** (`21c9df9a` + review fixes): a displayed photo lingering as a resident
   preview past 2 s gets its full force-requested regardless of a stuck `held_nav` — level-
   triggered, armed only when caught-up + image + non-RAW, disarmed on deck rebuild.
2. **#110 110a** (`c06e3d51`, `e6b49a94`): the odd-dim MIPGEN regression + the two-pass
   scale-aware Lanczos derive (`DERIVE_WGSL`) with the §3b colour chain (unclamped-alpha
   un-premultiply, fp16 Inf/NaN containment — both Codex catches, regression-tested).
3. **#110 110b** (`c4013e6d`, `38126061`): settled resize/toggle GPU-derives the exact Fit from
   the retained Original — **the ~1 s CPU re-decode is gone**. Reserve-then-derive with rollback;
   rotation/inset-aware; `FULLSCREEN_SETTLE` 50 ms. Levers: `PB_SCALE_POLICY=cpu`,
   `PB_DERIVE_KERNEL`, `PB_DERIVE_MIP_BIAS`.
4. **item-6 6a+6b** (`2a4b163b`, `9004ca4d`): `invalidate_geometry` retains + remaps resident
   Originals; content changes still purge (§4.1 invariant); nav derives a missing Fit from a
   retained neighbour Original — advance-after-toggle lands sharp.
5. **#110 110c** (`a9f1f11b`): the A/B/X harness (nv-flip FLIP + linear RMSE + detail ratio).
   **Data picked the defaults: Lanczos-3 + mip_bias −1.** Two always-run regressions pin
   derive-beats-trilinear and derive≡Lanczos-at-L0.
6. **The owner's stuck-blurry repro root-caused + fixed** (`66e5f7c3`): the pool untracks finished
   jobs before their outcomes drain, so a blaze re-issued duplicate previews and the *second*
   preview outcome was misread as "the full came back as a preview" — poisoning `upgrade_done`,
   gating off both the sharpen and the watchdog. Fix: `Outcome.preview` carries the JOB's
   allow_preview flag; a duplicate (`is_prev && img.is_preview && o.preview`) is dropped with no
   verdict.
7. **Watchdog second-chance hardening** (`89d457cb`): arms despite `upgrade_done` (fire-edge
   clears it), schedules its own wake (`watchdog_wake` in the SetWake min-list — fake-clock tests
   masked its absence once), bounded re-arm on post-fire errors (`MAX_WATCHDOG_RETRIES` = 3).
8. **"+1 never waits"** (`d443751f` + `dac0a9d1` review fixes): parked GPU **sharpen-via-derive**
   replaces the CPU re-decode when a mipped Original is resident (re-binds via `present_slot` +
   `draw`, never `present_item` — zoom/pan + slideshow timing preserved); parked fulls decode
   **nearest-first** (wrap-aware at the deck seam, parked-only) so back-up-one is second in line.
   Codex caught the sort being collected into a set and never reaching the decoder — the fulls
   tier is now built from `prefetch_fulls()`'s returned order, and the test pins the order the
   pool actually receives.

**Owner verification so far (RDP):** fullscreen toggle instant; can't outrun the ring during a
forward blaze; the direction-flip eviction observation became the #112 design (below). Still owed:
the physical-display pass + rotation/HDR/ICC edges from
`.taskmaster/docs/110-manual-test-script.md`, and a deliberate re-test of the old blaze → stop →
back-one repro.

**State:** pb-app-core 757 tests, workspace green, clippy `-D warnings` clean, ship-feature build
green, CHANGELOG updated.

## Next actions
1. **Finish owner manual verification** on the physical display (script above).
2. **#112 performance profiles (Safe / Normal / High slider)** — design draft **rev 2** at
   `.taskmaster/plans/112-performance-profiles.md`. Codex round 1: NOT sign-off-ready (1×P0 —
   the reserve_ring recovery idea was based on a false model; textures allocate lazily in
   `upload_slot` and an OOM panics via wgpu's uncaptured handler — plus 8×P1); **all findings
   folded into rev 2** (two-layer allocation design, `reconfigure_residency` + transactional ring
   `reconfigure`, CurrentUsage-aware WDDM ceiling, LUID-exact adapter match, dynamic archive
   pre-flight, `ResidencyLimits` seam shared with Phase-1b). Round-2 review + owner sign-off on
   the open questions pending. **Do not implement before sign-off.** Motivating repro: blaze →
   flip direction → just-passed photos already evicted (the 1.5 GB ring keeps ~5 behind on the
   7680 display). The window-split rebalance (4/5→2/3 ahead) rides along as its own phase.
3. **Phase 1b (110c display-capped pyramid)** — reviewed design draft at
   `.taskmaster/plans/110c-phase1b-display-capped-pyramid.md`, owner sign-off pending; its
   per-machine budget hook now lands in #112's shared `ResidencyLimits` seam.
4. **110d (deferred, own plan):** mode-1 ICC → fp16-pyramid conversion so ICC photos join the
   derive.
5. **#113 macOS on-device verify (NEW task).** Rev-18's "the mac shell uses the drop-all trait
   default for `remap_ring`" claim was **stale**: the mac shell constructs the shared
   `WgpuRenderer` directly (`pb-mac-ffi/src/lib.rs` ~2437 `new_from_ca_layer`), so `remap_ring`,
   `derive_fit`, and the whole arc ride free at the next mac build via wgpu's Metal backend —
   **unverified until run on-device**. #113 also carries the #109 item-1 shell parity edits.
6. **Blank thumbnails: downgraded from active-bug to hardening.** The owner can no longer easily
   reproduce it; most plausible reason is the cross-deck open-race fix (`8293a662`) plus this
   branch's drain hardening closing the practical window. The **structural hole remains**:
   `DecodeKey` = item/epoch/purpose/rep_kind with **no deck identity** (`decode_pool.rs:76`), so a
   stale in-flight decode can still dedup a fresh same-index want across a deck swap. #109 item
   (3) (content_gen/deck_gen in the key) is the definitive close. Failing-to-repro is not proof.

## Prime directive held: MEASURE, don't guess
Every quality claim above has a number (the ab_report matrix) or a regression test. Kernel/bias
defaults were picked from measured FLIP data. Codex reviewed every phase and both #112 design
revisions; all P0/P1 findings are folded in and cited in commit messages.

## Diag levers (env-gated, debug build only — release has no console)
- `PB_SHARP_DIAG=1` — sharpen lifecycle + `GPU-derived Fit item=…` + `GPU-sharpened item=…` +
  `preview watchdog FIRED` + `parked on item=… as a resident PREVIEW: …` (names the gate if a
  photo ever parks blurry).
- `PB_DOOR_DIAG=1` — draw source / backend / present diags. `PB_PRESENT_FIFO=1` — force Fifo.
- `PB_SCALE_POLICY=cpu` · `PB_DERIVE_KERNEL=2|3` · `PB_DERIVE_MIP_BIAS=0|-1` — the #110 A/B levers.
- `PB_PERF=1` — open→first-photo / resize→on-screen ms. Corpus: `D:\Media\Pictures\…\Wedding`.

# 📓 Load-bearing knowledge (don't re-derive)

- **ADR-024 is the organizing principle** — previews are blazing-only; the interaction display is a
  pure function of a resident Original pyramid. Sampler (#110) + residency (item-6) + enforcement
  (watchdog + sharpen-via-derive) all exist and are on main.
- **The derive's colour chain is load-bearing** (gpu.rs `DERIVE_WGSL` doc): mips are straight-alpha,
  mode-0 sRGB-encoded; premultiply after EOTF; fp16 premult-linear intermediate; un-premultiply
  ONCE by the UNCLAMPED filtered alpha; straight-alpha finals. Don't "simplify" any of it.
- **The Held derive source is only valid for the displayed photo** (pinned by
  `a_nav_derive_never_sources_the_previous_photos_held_frame`).
- **Retain iff `content_gen` unchanged** — geometry retains Originals, content purges everything;
  `invalidate_content` must NEVER call the retaining path.
- **`Outcome.preview` is the job's flag, `img.is_preview` the image's** — the duplicate-preview
  verdict needs both; don't collapse them.
- **The GPU sharpen re-binds via `present_slot` + `draw`, never `present_item`** (zoom/pan +
  `last_present`).
- **`prefetch_fulls()`'s returned order IS the fulls decode priority** — `request_prefetch` must
  build the fulls tier from it (Codex caught it collected into a HashSet and discarded, once).
- **The rev-15 "surface drops presents" theory stays DISPROVEN**; backend is **Vulkan + Fifo** on
  the owner's RTX 5090. Mixed-DPI/RDP memory applies to any fullscreen/DPI bug.
- **Git topology:** the arc is on `main` and pushed. **Stage explicit paths, never `-a`/`-A`**
  (owner edits concurrently). SSH-signed commits, no AI trailers.
- **Codex CLI reviews are part of the loop** (owner instruction 2026-07-18): `codex exec
  --sandbox read-only "<scoped prompt>"` at each section boundary + for complex plans; long
  reviews run in background (10 min+ is normal). The owner expects verified anchors and multiple
  rounds on complex plans.
- ⚠ **The owner drives the app while you work** — a running exe locks `target\debug\blazeviewer.exe`;
  confirm the About build id after any rebuild. Debug builds have a console; release does not.
- **Repo:** `github.com/fullspecsystems/blazeviewer`; product name **Blaze Viewer**.

---

# ⏸ PARALLEL TRACK — Windows video/audio — NOT this thread

_The `feat/audio-track-selection` arc, all merged to main._

**Shipped:** FFmpeg-first film audio on Windows (MF can't decode AC-3/E-AC-3/DTS `0xC00D36B4`);
audio-track selection (`A`/`Shift+A` + Playback ▸ Audio Track); `WAVEFORMATEXTENSIBLE` speaker-mask
sinks; off-thread track switches; short-forward-hop seeks; adaptive audio-seek settle. **⚠ The
FFmpeg→MF locator "bridge" was a MISTAKE, DELETED** (regression test
`audio_rows_keep_their_ffmpeg_locators`) — do NOT reintroduce.

**Track status (corrected rev 19):**
- **Pause/play audio gap: RESOLVED — NOT the app.** Root cause was the owner's AirPods Max
  Bluetooth A2DP link idle-sleeping (~1 s off-head); reproduced wired = gone. Trap comment at
  `wasapi_audio.rs` pause/resume (`6b4e958f`). Rev-18 listed this open/HIGH — struck.
- **Poster deep-walk: DONE + owner-flagged GOOD ENOUGH FOR NOW** (`267b1e9b`, on main): pure-black
  posters resolved. Accepted quirk: some Netflix specials (e.g. Ali Wong) that previously looked
  good now pick white-ish frames — likely the blank-title-card skip trading black leads for white
  cards. tasks.json **#92.1 done**; the macOS AVFoundation half (#92.2) remains open.
- **10 s Shift-seek gap** (~1.2–1.6 s, measured as video recreate+run-up): defer — scope with
  **79.10 NVDEC hardware decode**, which is the owner-required next video work (4K60 is borderline
  on software decode; plan at `.taskmaster/plans/79.10-nvdec-hw-decode.md`).

**Load-bearing:** WASAPI reseek ~10 ms; MF and FFmpeg enumerate audio streams in different orders
(the `ff`/`mf` two-currency locators); Windows audio is the master clock while playing; a `Failed`
clock is terminal. **Build:** `pwsh scripts/build-windows.ps1 -Run` — a plain `cargo run` omits
`ffprobe` → AC-3/E-AC-3/DTS films play SILENT. Diag: `PB_AUDIO_TRACE`, `PB_VIDEO_DIAG`,
`PB_AV_SYNC`.

# ⏸ PARALLEL TRACK — macOS #106 performance (NOT this thread)

Blueprint: `.taskmaster/plans/106-performance-archive-zoom.md` (rev2, Codex-reviewed). The
Windows/rendering side of #106.7 is far ahead (item-6 retain/remap + the #110 derive, now on
main). The mac host still owns its `PB_PERF` baselines. Shipped historically: door card
(`d91666a0`), perf timers (`fdcedd16`), #106.2 read/decode split (`5d8eebe1`).

# ⏸ PARALLEL TRACK — Windows door gating + Copy (#105.2 / #107)

Door renders correctly. Owed: gate OCR/Describe/Compare **off** on a door (`MenuState._enabled`);
#107 relabel "Copy Image"→"Copy" + emit file-only on a door; interactive smoke tests.

---

# 📌 macOS TODO — #113 (verify the rendering arc) + #109 item 1 (open-race parity)

**For a macOS agent, after pulling main.** Full task description in tasks.json **#113**. Summary:
the arc should ride free via the shared `WgpuRenderer`; verify on-device (`PB_SHARP_DIAG`
`GPU-derived`/`GPU-sharpened` lines, the 110 manual script, the DERIVE shader on Metal), and while
in there do the **#109 item-1** shell edits in `crates/pb-mac-ffi/src/lib.rs` (inspection-anchored
in task #109; authoritative reference `git show 8293a662 -- crates/pb-app/src/main.rs`):

1. **`begin_archive_open`** (~2877): after it cancels a prior archive (~2891), also
   `self.cancel_dir_scan();`.
2. **`begin_dir_scan`** (~2744): after the `Source::Scan` match,
   `if let Some(prev) = self.archive_load.take() { prev.progress.request_cancel(); }`.

**Deeper #109 hardening (deferred, both shells, Codex-recommended):** one shared open generation;
`content_gen`/`deck_gen` in `DecodeKey` (**also the definitive blank-thumbnails close — see Next
actions #6**); `upload_slot`/`mark_resident` return-checked; `present_item` returning success +
abort-drain-then-resync-once.
