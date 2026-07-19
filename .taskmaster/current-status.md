# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-18 (rev 17). **This handoff is the RENDERING-QUALITY track** — fullscreen /
resize / scale-mode sharpness. The Windows **video/audio** track (#5/#4/#1), the **macOS #106** perf
track, the **door gating** track (#105.2/#107), and the **macOS #109 port** run in parallel and are
**preserved below** — don't lose them, they're just not this thread's job._

---

# ▶️ START HERE — building #110, under the ADR-024 north star, on a branch

**Active line: branch `feat/110-gpu-lanczos-from-original` (you should be on it).** It contains
everything — `main`'s history + this session's fixes + the #110 Phase-110a work. `main` itself holds only
the planning docs + the two `main`-committed fixes (#111, the instrument bundle); the preview-into-native
fix, **ADR-024**, the frame-counter removal, and all #110 code live **on the branch** and reach `main`
when #110 merges. **Build from the branch.**

## The north star: ADR-024 — two modes, one residency invariant
(`.taskmaster/docs/decisions.md`, ADR-024.) The whole "stuck on a blurry preview / scale-swap shows a
thumbnail / 1 s re-decode" bug class had **one root**: a single preview-first pipeline served two modes
with opposite priorities, so the speed shortcut (a ~256 px preview) leaked into the tier where quality is
the point. The fix is to split them:
- **Blazing** (`held_nav().is_some()`): decode-to-fit **previews**, throwaway ring, speed-only. A preview
  is *correct* here. Hot path untouched.
- **Interacting / parked** (`held_nav().is_none()`): the current image ± a small neighbour window holds a
  **mipmapped full-res `Original`**, and **every** display (Fit any size, Fill, 1:1, zoom) is a **pure GPU
  derivation from that pyramid** — never a preview.
- **Bounded + portable:** cap the pyramid L0 to **~display resolution** (not image res), so a 24 MP and a
  100 MP photo cost the same resident footprint for viewing and it self-scales to a 4 GB laptop (small
  screen = small pyramid). Budget = a fraction of detected RAM/VRAM; radius auto-drops. Gigapixel true-1:1
  region-prefetch is a **named deferral**, not v1.
- **#110 is the sampler** (pyramid → any-size-quality); **item-6** keeps the pyramid resident on nav.

## What's shipped vs. in-flight this session
- **FIXED + owner-verified (`main`): #111 stuck-preview** (`327c3d0d`) — `decode_is_definitive_full()`
  keeps an undersized "full" (a transient-tiny-viewport ~256 px decode) sharpen-eligible instead of
  freezing on it. The rev-15 "surface drops presents" theory was **disproven** by a frame-counter capture
  (zero surface errors; backend is **Vulkan + Fifo**, not DX12/Mailbox).
- **FIXED + owner-verified (branch): preview-into-native-tier** (`100b3d3c`) — the prefetch only requests
  a preview when decoding a Fit (`allow_preview = fit.is_some()`), so switching scale modes never lands a
  thumbnail in the 1:1/native tier. This is the first enforcement of ADR-024.
- **#110 Phase 110a — 2 of ~4 pieces done (branch):** scale-aware Lanczos coefficients (`resample.rs`,
  6 CPU tests, `90f8c4a5`) + RingSlot owned-texture retention (`create_image_texture` →
  `RingSlot.texture/was_clamped/mode`, `fa5b30ab`). Behaviour-neutral, 57 GPU tests green.
- **Removed the `PB_FRAME_COUNTER` overlay** (`4b6fe022`) — it did its job (disproving the surface
  theory); the dots were noise.

## ⚠ Known open problem (queued, task #3): the lingering-preview watchdog
A **rare, unreproducible** stress-test bug (outrun the ring → flip fullscreen → stuck on a preview until a
resize). Cause: a lost key-up leaves `held_nav` stuck `Some`; the tick's sharpen re-issue (`3b`,
`app_core_impl.rs:1608`) and `sharpen_now()` both gate on `held_nav().is_none()`, so the sharpen is
suppressed until a focus change (fullscreen flip) fires the release net. **Fix (agreed, not built):**
enforce ADR-024 with a **level-triggered safety net** — a displayed image that stays a resident preview
past ~0.5 s gets its full requested regardless of `held_nav`. A real blaze never lingers that long → hot
path untouched. Test-first (fake-clock). Because the race can't be reproduced, the safety net (self-
correcting state) is the honest fix, not race-hunting.

## Next action
Either **task #3** (the watchdog — small, bulletproofs a known bug) or **task #4** (#110 110a WGSL
pipelines — the derive shader). For #4, **read the #110 plan §3b first** (Codex's corrected colour chain:
mips are stored straight-alpha + mode-0 sRGB-encoded, NOT premult-linear) and §2 (the four P0s). The
coefficients are proven, so the shader is "apply known-good weights," not "invent the kernel."

## Prime directive held: MEASURE, don't guess.
The surface theory was a wrong guess an instrument disproved in one capture. Every fix this session was
root-caused from evidence and owner-verified. #110 carries explicit "measure, don't assert" gates (the A/B
harness; the derive is a box+Lanczos composite, not a proven upgrade until measured).

## Diag levers (env-gated, in the tree; debug build only — release has no console)
- `PB_SHARP_DIAG=1` — the preview→full "sharpen" lifecycle + `resize to SMALL viewport` + `reserve upload`
  + `upgrade got UNDERSIZED full … kept sharpen-eligible` (the #111 fix firing).
- `PB_DOOR_DIAG=1` — renderer draw source (`RingSlot`/`Held`/`Single`) + backend/present_mode/img dims +
  the split `render: surface {Lost|Outdated|Timeout}` line + core door/deck belief.
- `PB_PRESENT_FIFO=1` — force Fifo (A/B vs Mailbox). (`PB_FRAME_COUNTER` was **removed** this session.)
- Corpus: `D:\Media\Pictures\…\Gill & JD's Wedding`.

## In-session task queue (Claude Code tasks)
- **#3** — lingering-preview watchdog (above).
- **#4** — #110 110a: WGSL two-pass Lanczos derive pipelines + colour chain.
- **#5** — #110 110a: odd-dim MIPGEN regression test (mipgen drops the trailing odd row/col).
- **#6** — #110 110b: wire `derive_fit` into the core (current photo) — *the phase the owner feels*.
- **#7** — #110 110c: downscale-quality A/B/X harness + the ADR-024 display-capped-pyramid budget (Phase 1b VRAM).
- **#8** — item-6: retain/remap the ring across a geometry change (after #110).

# 📓 Load-bearing knowledge (don't re-derive)

- **ADR-024 is the organizing principle** — previews are blazing-only; the interaction display is a pure
  function of a resident, display-capped mipmapped Original. #110 + item-6 implement it.
- **The rev-15 "surface drops presents" theory is DISPROVEN** (frame-counter capture: zero surface errors,
  presents land). The stuck bugs were preview-tracking / undersized-full / preview-leak issues, now fixed.
  Don't re-open the surface-recovery-seam theory. (Codex debunked the DX12 "same-size configure is a no-op"
  premise too.)
- **Backend is Vulkan + Fifo** on the owner's physical RTX 5090 desktop (not DX12/Mailbox).
- **Mixed-DPI / RDP env** (`windows-display-rdp-env` memory): physical **7680×2160 @150% (RTX 5090)** vs
  RDP **~1470×923 @100%**. A transient small viewport during a fullscreen transition fed the #111 bug.
- **Git topology:** `main` = planning + #111 + instrument bundle (`3cf8790e`); **branch
  `feat/110-gpu-lanczos-from-original`** = all of that + the preview fix + ADR-024 + frame-counter removal
  + #110 110a. Some fixes + ADR-024 are **branch-only** until #110 merges. **Stage explicit paths, never
  `-a`/`-A`** — the owner edits concurrently. Commits SSH-signed; **no `Co-Authored-By`/AI trailer**.
  Planning on `main`; implementation on the branch (owner's rule).
- **Debug builds have a console** for `PB_*` diag; release does not. ⚠ **The owner drives the app while you
  work** — the running exe locks `target\debug\blazeviewer.exe`; a silently-failed rebuild = a stale
  binary. Confirm the About-dialog build id.
- **Repo:** `github.com/fullspecsystems/blazeviewer`. Product is **Blaze Viewer** (blazeviewer.app); don't
  propagate the old "PhotoBlaze" name.

---

# ⏸ PARALLEL TRACK — Windows video/audio (#5 / #4 / #1) — NOT this thread

_The `feat/audio-track-selection` arc, all merged to main. Was a previous session's START HERE;
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
extended on the Windows/rendering side this session** (the resize rebind, the mip work, the resize-hold,
and now #110's texture retention). The mac host still owns its `PB_PERF` baselines. Shipped historically:
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
