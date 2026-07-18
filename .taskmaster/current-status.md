# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-18 (rev 15). **This handoff is the RENDERING / SURFACE track** — the
archive-door, fullscreen-scaling, and GPU-surface-present work, all merged to `main`. The Windows
**video/audio** track (#5/#4/#1), the **macOS #106** perf track, the **door gating** track
(#105.2/#107), and the **macOS #109 port** run in parallel and are **preserved below** — don't lose
them, they're just not this thread's job._

---

# ▶️ START HERE — the GPU surface drops presents (the ONE root under several "stuck" bugs)

**Owner-confirmed 2026-07-18.** Three symptoms that look unrelated are the **same** bug — the
swapchain surface drops the present around a window/content **size change**, so the last frame that
actually reached the screen stays put:

1. **Photo loads on a blurry low-res preview and never sharpens** until you resize / toggle
   fullscreen / change scale (task **#111**).
2. **Archive door card frozen over a stale photo** — `space` advances the filename in the title bar
   but the picture never changes (the #109 "frozen view" family).
3. **"Titles advance but the view is frozen."**

In every case the core is doing the right thing; the *present* just doesn't land.

## Proof (don't re-derive — it's measured)
A `PB_SHARP_DIAG` + `PB_DOOR_DIAG` capture on a **plain folder** (so it's **source-independent** —
the owner ruled out archives by testing extracted photos on a Gen4 SSD) — saved earlier as
`sharp-diag.log`:
- **Sharpen logic WORKS:** `12/12` `[sharp-diag] full landed → UPGRADE (sharpen applied)`. The lone
  `NO sharpen` line was a correct `held_nav=true` (don't sharpen mid-blaze). So the full DOES decode
  and the core DOES apply it — the sharpen/decode path is **not** the bug.
- **The surface dropped 17 presents:** `render: surface lost/outdated — config=WxH
  present_mode=Mailbox — reconfigured, frame dropped` ×17, plus 40 frames drawn from `Held`.
- **The surface config size was FLUCTUATING:** `1117×882`, `1454×864`, `1454×884`. A size change →
  the surface goes `Outdated` → a present is dropped; when the dropped present is a **sharpen** (or a
  door) frame, that frame never reaches the screen → you're stuck on the blurry/stale frame until a
  resize/switch forces a fresh present.
- **My size-drift heal never fired** (`heal_surface_if_dropped`, `8b5dc30b`) — no `surface heal` /
  `size OK` lines. Transient drops recover on the per-tick retry once the size settles, so
  `redraw_pending` clears before the heal checks; a present dropped **mid-fluctuation** is stranded.

## Two concrete leads (in priority)
1. **Split `Lost` vs `Outdated` (one-line change) then RECREATE the surface** (Codex's DX12
   guidance). `render()` (`crates/pb-render/src/gpu.rs` ~2965) currently combines
   `Lost | Outdated` into one branch and `reconfigure_surface` (~2217) reconfigures at the **same**
   `config` size every failure — which is useless for a **same-size DX12 `Lost`** (the likely case
   on the owner's mixed-DPI/RDP RTX setup). Real fix: a typed render result → **recreate the
   `wgpu::Surface`/DXGI swapchain after N (2–3) consecutive losses**, via a **shell-supplied surface
   factory/recovery seam**. Do **NOT** put `Arc<Window>` in `WgpuRenderer` — it takes a generic
   `SurfaceTarget` and also serves the macOS CAMetalLayer path. **First step: split the log**, so the
   next capture says `Lost` vs `Outdated` vs `Timeout`.
2. **The 20px oscillation** `1454×884 ↔ 1454×864`. That's a suspicious ~20px client-area toggle —
   likely a **docked toolbar / menu bar / info line** appearing+disappearing and churning the
   surface avoidably. Cheap to check: is the client area oscillating on its own (an app bug) vs the
   owner resizing? Killing needless churn may remove most of the drops without any surface-recreation
   at all.

## Prime directive: MEASURE, don't guess.
This exact class of bug cost **many** wrong attempts (ring desync, cross-deck race, stale overlay,
sharpen-upgrade) before instrumenting the **render/present path** revealed the surface. The
cross-deck race (#109) and the sharpen path were **real but downstream**. Instrument the surface
first; the two diag levers below already exist.

## Diag levers (env-gated, already in the tree)
- `PB_DOOR_DIAG=1` — renderer per-frame draw source (`RingSlot`/`Held`/`Single`) + the
  **`surface lost/outdated … config=WxH present_mode=…`** line + the core's door/deck belief + deck
  transitions + `surface heal` lines.
- `PB_SHARP_DIAG=1` — the preview→full "sharpen" lifecycle (`preview shown` / `sharpen requested` /
  `NO sharpen … (why)` / `full landed → UPGRADE|DROPPED|upgrade_done|ERROR`).

Both need a **debug** build for a console (release has none). Owner reproduces over their real
desktop; corpus folder `D:\Media\Pictures\…\Gill & JD's Wedding`.

---

# ✅ SHIPPED this session (rendering / archive — all on `main`, tests green)

- **Archive door/deck cross-type open-race fix (#109, `8293a662`).** A stale folder-scan batch could
  `extend_playlist` over an archive deck (Codex-diagnosed). Core **extend-guard** in
  `apply_scan_batch` (shell-neutral → protects macOS) + winit shell **cross-cancels** the other
  worker on each open. Reverted my bad `present_item` invalidate-on-miss repair
  (`cff70ca0`/`c383107a`) that had regressed instant-fullscreen to a preview flash.
- **Instant SHARP fullscreen/resize (#106.7 §6, `bcd37ea6` + `792dfa9e`).** On resize, rebind the
  retained full-res **Original** (a pure rebind like the `0` toggle); a `resize_hold` field makes the
  settle re-decode quality-monotonic (no EXIF-preview flash). `792dfa9e` fixed a stuck loading pie
  (the skip branch now `mark_resolved`s at the new epoch).
- **Phase 1 mipmapped near-Lanczos GPU downscaling (`d82df25f`).** `upload_slot` gains a `mip` flag;
  only the full-res `Original` rep gets a mip chain (Fit/preview/animation/UI stay L0). Mip-gen is
  **linear-light + premultiplied-alpha + odd-dim-safe** (`MIPGEN_WGSL`, `build_mipgen`/
  `generate_mips`); source-ICC (mode 1) images stay L0. Deterministic GPU test proves the
  linear-light average (2×2 B/W → 1×1 ≈ 188, not 128). **Owner-tested: helps the instant frame, but
  the re-decode swap is STILL visible on high-freq content — box mips ≠ Lanczos.**
- **Diagnostics:** `PB_DOOR_DIAG`, `PB_SHARP_DIAG`, `surface_size()` + `heal_surface_if_dropped`.

# 🔜 Tasks on this track (tasks.json)

- **#111 — stuck-blurry preview.** ⇒ Now **folded into the surface bug above** (START HERE). The
  sharpen works; the surface drops the present. Fix via surface robustness, not the decode path.
- **#110 — GPU HQ-scaling follow-ons.** Phase D (retire the re-decode = truly toggle-smooth) is
  **A/B-gated + needs the GPU-derived-Lanczos escalation** (box mips aren't enough on grass/fabric).
  Phase 1b VRAM: the ring records L0 bytes only (mipped Originals under-counted ~4/3);
  allocation-aware + HDR-limited `full_res_eligible`; the pre-existing `make_room_for_upgrade`/
  eviction bug. Plus odd-dim polyphase, mode-1 TRC mips, `clamp_to_max` nearest-neighbor, the golden
  suite (`nv-flip` isn't a dep yet), a Metal fp16 smoke test. Plan:
  `.taskmaster/plans/gpu-mipmap-hq-scaling.md` (**§0 = the surface breakthrough + Phase 1 result**).
- **#109 — macOS shell port + deeper hardening.** See the **📌 macOS TODO** at the very bottom (a
  self-contained handoff). The core guard already protects macOS from the severe form.

# 📓 Load-bearing knowledge (don't re-derive)

- **The surface bug is source-independent** — a plain folder reproduces it identically to an archive;
  an archive only *widens* timing windows. Don't chase archive-specific theories for it.
- **Mixed-DPI / RDP env** (`windows-display-rdp-env` memory): physical **7680×2160 @150% (RTX 5090)**
  vs RDP **~1470×923 @100%**; the `config` size fluctuation in the log is almost certainly this
  environment (or a toggling inset). Pin the active display for any surface/DPI/render bug.
- **Debug builds have a console** for `eprintln`/`PB_*` diag; **release does not**.
- ⚠ **The owner drives the app while you work** — the running exe locks
  `target\debug\blazeviewer.exe` (rebuild → "Access is denied"); that's THEM, don't kill it, they
  relaunch. `Get-Process blazeviewer`. **A silently-failed rebuild = you're testing a stale binary**
  (bit us: "still seeing the F regression" was a build that never took). Confirm the About-dialog
  build id after a rebuild.
- **Single worktree** now (`~/code/blazeviewer`); `git push origin main` works directly (the old
  two-worktree "can't checkout main" note is gone). **Stage explicit paths, never `-a`/`-A`** — the
  owner edits concurrently. Commits are SSH-signed; **no `Co-Authored-By`/AI trailer**.
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
