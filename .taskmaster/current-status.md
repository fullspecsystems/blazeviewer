# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-18 (rev 16). **This handoff is the RENDERING-QUALITY track** — fullscreen /
resize sharpness. The Windows **video/audio** track (#5/#4/#1), the **macOS #106** perf track, the
**door gating** track (#105.2/#107), and the **macOS #109 port** run in parallel and are **preserved
below** — don't lose them, they're just not this thread's job._

---

# ▶️ START HERE — the #110 → item-6 rendering-quality roadmap (plans committed, on a branch)

**The "stuck on a blurry preview" bug (#111) is FIXED and owner-verified. The rev-15 "surface drops
presents" theory was WRONG — disproven by a live capture.** The current work is a two-plan
rendering-quality roadmap; both plans are committed to `main` and Codex-reviewed, and implementation is
staged on the branch **`feat/110-gpu-lanczos-from-original`** (checked out; `main` holds all planning).

## What was actually wrong (don't re-derive — it's measured)
A `PB_FRAME_COUNTER` + `PB_SHARP_DIAG` capture (`counter.log`) on the wedding folder showed **zero**
surface `Lost/Outdated/Timeout` errors and presents landing (the on-screen frame counter advanced on
mouse-move) — so the surface never dropped presents. Backend is **Vulkan + Fifo** on the owner's
physical RTX 5090 desktop (not DX12/Mailbox as rev 15 assumed). **Real root:** a fullscreen toggle's
transient tiny viewport made `decode_fit()` ~256px, so a "full" (`is_preview=false`) decoded at that
stale fit landed ~256px, was treated as the definitive full, cleared `preview_resident`, and the job
loop then read the resident-but-untracked Fit slot as "already full" (`resident && !is_prev → continue`)
and never re-decoded → stuck low-res until a resize/switch nudged it.

## The two remaining residuals (owner priority: #110 FIRST, then item-6)
1. **Current photo on toggle: box-mip-instant → ~1s CPU-Lanczos re-decode** ("slightly fuzzy, then
   sharpens after ~1s"). → **task #110**: GPU-derive an exact-size Lanczos Fit from the retained
   **mipped Original** — no CPU decode, no SMB re-read. **Independent of item-6** (the current photo
   always has a resident Original) and the **higher-value** change. Plan (v2, Codex-reviewed):
   `.taskmaster/plans/110-gpu-lanczos-from-original.md` + `110-gpu-lanczos-SPEC-codex-review.md`.
2. **Advance-after-toggle: a NEIGHBOUR shows its 256px preview for ~2s.** → **item-6 (retain/remap)**.
   KEY correction: `invalidate_geometry` news up an empty ring → nukes the WHOLE ring **incl. Originals**;
   only the CURRENT photo survives (via `resize()`-presents-Original-then-renderer-`held`), so neighbours
   have nothing high-res. Fix = retain Originals across the toggle + a "Fit display may be satisfied by a
   resident Original" rule on **NAV**. Hardened spec: `.taskmaster/plans/106.7-item6-retain-remap-SPEC.md`
   (+ `-DRAFT.md` = Rachel's original, `-SPEC-codex-review.md` = Codex's review).

## Next action: implement #110 Phase 110a
On `feat/110-gpu-lanczos-from-original`. Per plan v2 §9: the `RingSlot`/`held` owned-texture bundle
(uploaded dims, mip count, mode, hdr/scene-scale, `was_clamped`); the two scale-aware Lanczos pipelines
(fp16 intermediate; RGBA8-sRGB + fp16 finals); coefficient precompute; the pure-CPU coefficient test +
odd-dim MIPGEN regression. No behaviour change in 110a. **Read plan v2 §2 first — four P0 blockers Codex
found** (mips are straight-alpha + mode-0 sRGB-encoded, NOT premult-linear; tap support = `a·max(s,1)`;
the v1 source is `renderer.held` not a ring slot; a fp16 mode-0 Fit needs mode-2 sampling).

## Prime directive held: MEASURE, don't guess.
The surface theory (rev 15) was a wrong guess that the frame-counter instrument disproved in one capture.
The stuck-preview fix was root-caused from the `counter.log` evidence and owner-verified before claiming
victory. #110/item-6 both carry explicit "measure, don't assert" gates (the A/B harness, the P0 list).

## Diag levers (env-gated, in the tree)
- `PB_FRAME_COUNTER=1` — a 24-bit binary present-liveness counter painted in the tonemap pass (proves
  whether presents reach the screen). `PB_PRESENT_FIFO=1` — force Fifo (A/B vs Mailbox).
- `PB_SHARP_DIAG=1` — the preview→full "sharpen" lifecycle + `resize to SMALL viewport` +
  `reserve upload …` + `upgrade got UNDERSIZED full … kept sharpen-eligible` (the #111 fix firing).
- `PB_DOOR_DIAG=1` — renderer draw source (`RingSlot`/`Held`/`Single`) + backend/present_mode/img dims +
  the split `render: surface {Lost|Outdated|Timeout} …` line + core door/deck belief.

Both need a **debug** build for a console (release has none). Corpus folder
`D:\Media\Pictures\…\Gill & JD's Wedding`. The `counter.log` / `sharp-diag.log` captures are untracked in
the tree.

---

# ✅ SHIPPED this session (rendering — all on `main`, tests green)

- **Stuck-preview fix (#111, `327c3d0d`, owner-verified).** `decode_is_definitive_full()` gates both the
  reserve + upgrade paths in `drain_results`: an **undersized full** (downscaled below the current fit
  while `orig_width/height` shows more source) stays **sharpen-eligible** so the real full re-decodes at
  the current fit. Native-size photos + placeholders unaffected (no re-decode loop). 3 end-to-end
  `drain_results` tests. Owner: "much better — no full reloads, no getting stuck on low-res."
- **Instrument bundle (`3a0822ac`).** Env-gated dev tooling that ruled OUT the surface theory:
  `PB_FRAME_COUNTER` binary counter in the tonemap pass; `render()` splits `Lost/Outdated/Timeout` with
  a truthful reconfigure result + backend; `PB_PRESENT_FIFO` A/B toggle; backend/present_mode/img in the
  door-diag render line. Zero cost when off.
- **Plans committed to `main`:** the hardened item-6 spec (`80fd5ca6`), the #110 v2 plan (`1f37ddfe`),
  and both Codex-review docs (`9f81b02a`, in `1f37ddfe`).
- **Prior (still current):** #106.7 §6 instant SHARP resize/rebind (`bcd37ea6`/`792dfa9e`); Phase-1
  mipmapped GPU downscaling (`d82df25f` — the Original rep gets a linear-light/premult mip chain; the
  instant frame is sharp-ish but box-mips ≠ Lanczos, which is what #110 addresses).

# 🔜 Tasks on this track (tasks.json)

- **#110 — GPU-Lanczos-from-Original (NEXT).** Plan v2 is ready + Codex-reviewed. Phasing: 110a (plumbing
  + tests, no behaviour change) → 110b (the derive + `ScalePolicy` seam, current photo only — *the phase
  the owner feels*) → 110c (A/B harness + nv-flip + golden) → 110d defer (mode-1 fp16 pyramid; compose
  with item-6). Also folds in the deferred Phase-1b VRAM accounting (mipped-Original ~4/3 undercount;
  allocation-aware `full_res_eligible`; `make_room_for_upgrade`/eviction bug) and `clamp_to_max`
  eligibility. `nv-flip` isn't a `pb-render` dep yet.
- **item-6 — retain/remap the ring across a geometry change (AFTER #110).** Fixes the advance-after-toggle
  neighbour preview. Hardened spec ready (Codex's 3 P0s baked in). Once #110 lands, its derive covers
  neighbours automatically via `derive_fit(Ring(slot), …)`.
- **#109 — macOS shell port + deeper hardening.** See the **📌 macOS TODO** at the very bottom. The core
  guard already protects macOS from the severe form.

# 📓 Load-bearing knowledge (don't re-derive)

- **The rev-15 "surface drops presents" theory is DISPROVEN** — the frame-counter capture showed zero
  surface errors and presents landing. The stuck bugs were a preview-tracking / undersized-full issue,
  now fixed. Don't re-open the surface-recovery-seam theory for this. (Codex also debunked the DX12
  "same-size configure is a no-op" premise: wgpu-hal DX12 `configure` always ResizeBuffers.)
- **Backend is Vulkan + Fifo** on the owner's physical RTX 5090 desktop (not DX12/Mailbox). Relevant for
  any present/queue reasoning.
- **Mixed-DPI / RDP env** (`windows-display-rdp-env` memory): physical **7680×2160 @150% (RTX 5090)** vs
  RDP **~1470×923 @100%**. A transient small viewport during a fullscreen transition is what fed the #111
  bug (`resize to SMALL viewport` diag). Pin the active display for any DPI/render bug.
- **Debug builds have a console** for `eprintln`/`PB_*` diag; **release does not**.
- ⚠ **The owner drives the app while you work** — the running exe locks `target\debug\blazeviewer.exe`
  (rebuild → "Access is denied"); that's THEM, don't kill it, they relaunch. `Get-Process blazeviewer`.
  **A silently-failed rebuild = you're testing a stale binary.** Confirm the About-dialog build id.
- **Single worktree** (`~/code/blazeviewer`); `git push origin main` works directly. **Stage explicit
  paths, never `-a`/`-A`** — the owner edits concurrently. Commits are SSH-signed; **no
  `Co-Authored-By`/AI trailer**. **Planning on `main`; implementation on a branch** (owner's rule).
- **Repo:** `github.com/fullspecsystems/blazeviewer` (HTTPS). Product is **Blaze Viewer**
  (blazeviewer.app); don't propagate the old "PhotoBlaze" name.

---

# ⏸ PARALLEL TRACK — Windows video/audio (#5 / #4 / #1) — NOT this thread

_The `feat/audio-track-selection` arc, all merged to main. Was the previous session's START HERE;
still real open work, just not the active thread._

**Shipped:** FFmpeg-first film audio on Windows (MF can't decode AC-3/E-AC-3/DTS `0xC00D36B4`);
audio-track selection (`A`/`Shift+A` + Playback ▸ Audio Track); `WAVEFORMATEXTENSIBLE` speaker-mask
sinks; off-thread track switches; short-forward-hop for seeks (+2 s tap over SMB ~1 s → **139 ms**);
adaptive audio-seek settle (172 ms → ~10 ms). **⚠ The FFmpeg→MF locator "bridge" was a MISTAKE,
DELETED** (regression test `audio_rows_keep_their_ffmpeg_locators`) — do NOT reintroduce.

**Open:**
- **#5 — pause/play audio gap (owner HIGH).** Pressing `P` to pause then play leaves a multi-second
  gap before audio resumes (video's back, audio lags). **Not yet investigated.** Trace: `poll_video`
  (`app_core_impl.rs` ~7950) → `CoreEffect::ResumeVideoAudio` → `main.rs` drain →
  `WasapiAudio::resume()` → engine `Cmd::Resume` → `sink.client.Start()` (`wasapi_audio.rs`). Top
  suspect: **resume preroll waits on audio-ready** (`video_session.rs` `preroll_satisfied` =
  frames + `audio_ready_or_absent`, bounded by `AUDIO_READY_TIMEOUT`). Measure first: time
  `Cmd::Resume` under `PB_AUDIO_TRACE`; a `PB_AV_SYNC`-style `P → ResumeVideoAudio → clock Playing`
  line. Owner runs over SMB (`\\beenas\Media\Movies`).
- **#4 — the 10 s Shift-seek gap** (~1.2–1.6 s). MEASURED entirely the **video recreate+run-up**
  (audio blameless). Real fix = NV12 software decode + in-shader YUV (the deferred **79.10 planar**
  path, multi-day, color-regression risk). RECOMMENDATION: **defer** — a coarse 10 s jump tolerates
  ~1 s; scope with 79.10.
- **#1 — MF poster deep-walk** (Windows video posters are **pure black**, MEASURED luma 0.000).
  Owner-approved fix, full scope in `tasks.json` #1: port `crates/pb-decode/src/mf_poster.rs` to the
  `ffmpeg/poster.rs` reference — **scored best-so-far walk** + **deep seek past the intro**
  (`POSTER_SEEK_OFFSETS` 8/20/45/90 s, lifted into `video.rs`) + **MF seek = RECREATE the reader per
  offset** (warm HEVC reposition blocks ~1 s; a fresh open ~86 ms) + 15 s deadline. Home videos open
  on content → settle in the head walk, zero added cost. Verify with `PB_VIDEO_POSTER_CLIP`.

**Video/audio load-bearing:** WASAPI reseek ~10 ms (so eager audio-commit is safe, and pause/play is
a *different* path than seek). MF enumerates audio streams in a **different order** than FFmpeg
(hence the `ff`/`mf` two-currency locators, why the bridge was wrong). Windows audio is the **master
clock** while playing; a `Failed` clock is **terminal**. **Build:** `pwsh scripts/build-windows.ps1
-Run` (ship features `libheif,dav1d,ffprobe` + the VS Dev shell FFmpeg's bindgen needs) — a plain
`cargo run` omits `ffprobe` → **AC-3/E-AC-3/DTS films play SILENT**. Diag: `PB_AUDIO_TRACE`,
`PB_VIDEO_DIAG`, `PB_AV_SYNC`.

# ⏸ PARALLEL TRACK — macOS #106 performance (NOT this thread)

Blueprint: `.taskmaster/plans/106-performance-archive-zoom.md` (rev2, Codex-reviewed). #106.7 (typed
`Representation::Fit{epoch}|Original` + parked full-res tier) is now **substantially shipped and
extended on the Windows/rendering side this session** (see SHIPPED above — the resize rebind, the
mip work, the resize-hold). The mac host still owns its `PB_PERF` baselines. Shipped historically:
door card (`d91666a0`), perf timers (`fdcedd16`), #106.2 read/decode split (`5d8eebe1`).

# ⏸ PARALLEL TRACK — Windows door gating + Copy (#105.2 / #107)

Door renders correctly. Owed: gate OCR/Describe/Compare **off** on a door (`MenuState._enabled`);
#107 relabel "Copy Image"→"Copy" + emit file-only on a door; interactive smoke tests.

---

# 📌 macOS TODO — port the cross-type open-race fix (task #109)

**For a macOS agent.** The archive/folder open-race was root-caused + fixed on Windows
(`8293a662`). The **core** half (`apply_scan_batch` extend-guard) is shell-neutral, so macOS is
already protected against the *severe* form; the **shell** half was only done in the winit shell.
Mirror the two winit edits into `crates/pb-mac-ffi/src/lib.rs` (inspection-only — I can't build
`pb-mac-ffi` on Windows):

1. **`begin_archive_open`** (~2877): right after it cancels a prior archive (`self.archive_load.take()
   … request_cancel()`, ~2891), also `self.cancel_dir_scan();` (the mac `cancel_dir_scan` ~2864
   already nulls `dir_scan`, so a bare call suffices — do NOT also write `self.dir_scan = None;`).
2. **`begin_dir_scan`** (~2744): after it confirms it's really starting a scan (mirror the winit
   placement — after the `Source::Scan` match), `if let Some(prev) = self.archive_load.take() {
   prev.progress.request_cancel(); }`.

Authoritative reference: `git show 8293a662 -- crates/pb-app/src/main.rs`. Verify: opening an archive
over a still-scanning folder, or clicking tree folders/archives rapidly, always lands on the *right*
deck; `PB_DOOR_DIAG=1` if anything slips.

**Deeper #109 hardening (deferred, both shells, Codex-recommended):** one **shared open generation**
replacing `archive_gen`/`scan_gen` + the global `scan_bootstrapped` boolean; `content_gen`/`deck_gen`
in `DecodeKey`; `upload_slot`/`mark_resident` return-checked; `present_item` returning success +
abort-drain-then-resync-once. Full Codex analysis captured in the 2026-07-18 session.
