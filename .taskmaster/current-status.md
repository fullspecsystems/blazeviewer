# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-17 (rev 13). **This handoff is the WINDOWS VIDEO/AUDIO track**
(the `feat/audio-track-selection` arc, all merged to main). The macOS `#106` performance
track and the door/Copy track run in parallel and are preserved at the bottom — don't lose
them, but they are not this thread's job._

---

# ▶️ START HERE — the audio gap on **pause/play** (task #5, owner HIGH priority)

**Owner, 2026-07-17:** pressing `P` to pause then play leaves a **multi-second gap before
audio resumes** — video comes back, audio lags badly. This hits *every unpause*, so it's
worse than the seek gaps we already fixed. **Not yet investigated.**

**Prime directive: measure, don't guess.** We just proved our own estimates wrong twice
this session (the seek "1s→1.5s" and the "111ms" settle that was really 172ms). Add timing
first, get real SMB numbers, THEN fix.

### The flow to trace (P → sound)
- `poll_video` (`app_core_impl.rs` ~7950, the **session-state audio bridge** right after the
  1D seek coordinator) turns session `Playing`/`Paused` into `CoreEffect::PauseVideoAudio` /
  `ResumeVideoAudio`.
- `main.rs` effect drain → `WasapiAudio::pause()`/`resume()` → engine `Cmd::Pause`/`Resume`
  → `sink.client.Stop()`/`Start()` (`crates/pb-app/src/wasapi_audio.rs`).

### Suspects (in rough priority)
1. **Resume preroll waits on audio-ready.** `video_session.rs` `preroll_satisfied` =
   frames + `audio_ready_or_absent` (bounded by `AUDIO_READY_TIMEOUT`). If the audio player
   doesn't report `Playing` promptly after `Start()`, the session may sit in preroll for
   seconds. This is the top suspect — it's exactly the "video's back, audio isn't" shape.
2. **The WASAPI `Start()` / a needless `Reset` or refill on resume** in the engine.
3. **The 1D coordinator (`scrub_audio_paused`) interacting with a plain pause** — a stale
   pause flag could hold audio down until a settle that never comes for a non-seek pause.

### How to measure (add this first)
- Engine (`wasapi_audio.rs`): time `Cmd::Resume` wall under `PB_AUDIO_TRACE` (mirror the
  `reseek` line already there).
- Core: a `PB_AV_SYNC`-style line — `P`-pressed → `ResumeVideoAudio` emitted → the audio
  clock next reports `Playing`. The plumbing pattern is the one we just used for the seek
  gap (`dbg_seek_land_at` + the commit log in `poll_video`).
- Owner runs the diag build over SMB (`\\beenas\Media\Movies`) and pastes the numbers.

---

# ✅ SHIPPED this arc (Windows video/audio — all on main)

The `feat/audio-track-selection` branch, merged to `origin/main` (last: seek + audio work
through `2ae144c7`). In order:

- **FFmpeg-first film audio on Windows.** MF can't decode AC-3/E-AC-3/DTS (`0xC00D36B4`), so
  films were silent. The trimmed Windows FFmpeg already ships every audio decoder (kept for
  channel-layout naming) at **zero bundle cost**, so the WASAPI engine now decodes
  FFmpeg-first (`FfAudioDecoder`), MF fallback, plain no-`ffprobe` builds MF-only.
  Owner-confirmed clean.
- **Audio track selection** (`A`/`Shift+A` + the **Playback ▸ Audio Track** flyout) — both
  currencies (`ff`/`mf`) per row, tick pinned to what actually decodes.
- **`WAVEFORMATEXTENSIBLE` speaker-mask sinks** (bare `WAVEFORMATEX` garbled multichannel;
  `FfAudioDecoder::layout_mask` — native-order bits ARE WAVE bits).
- **Off-thread track switches** (the open blocks seconds on SMB; on the engine thread it
  drained the buffer + jumped the master clock → jerky).
- **⚠ The FFmpeg→MF locator "bridge" was a MISTAKE, now DELETED** (regression test
  `audio_rows_keep_their_ffmpeg_locators`). It overwrote each audio row's `FfStream` locator
  with its MF twin → switches fell to the MF decoder (crackle/DTS-refused) + dead tick.
  Do NOT reintroduce it.
- **Short-forward-hop for seeks** (`mf_video_producer.rs`): a small forward seek decodes
  forward from the live reader instead of recreating it — a `+2 s` tap over SMB **~1 s →
  139 ms** (measured, 7.5×). `should_hop()` pure + unit-tested; 5 s cap; `reader_pos`
  tracked across seek/sequential paths.
- **Adaptive audio-seek settle** (`app_core_impl.rs` + `engine.rs`): a discrete tap (seek
  key released) commits its audio seek after a short 60 ms `VIDEO_SEEK_AUDIO_QUIET` instead
  of the full 250 ms `VIDEO_SEEK_AUDIO_SETTLE`, so a tap's audio lands **with** the picture
  (measured **172 ms → ~10 ms**). Held scrubbing keeps the key down → unchanged.
  Owner-confirmed good on 2 s taps.

### Diag levers (all env-gated, keep them)
- `PB_AUDIO_TRACE=1` — engine: decoder choice, sink mode, underruns, switches, reseek wall.
- `PB_VIDEO_DIAG=1` — MF seek: HOP vs recreate, run-up frames, wall.
- `PB_AV_SYNC=1` — core: the audio-seek settle residual (land → commit).

### Build (⚠ the trap that bit twice)
`pwsh scripts/build-windows.ps1 -Run` — defaults to `--features libheif,dav1d,ffprobe` and
enters the VS Dev shell FFmpeg's bindgen needs. **A plain `cargo run` omits `ffprobe` → no
FFmpeg linked → every AC-3/E-AC-3/DTS film plays SILENT.** `-NoFfmpeg` for a quick build.

# 🔜 Remaining on this track (tasks.json)
- **#5 — the pause/play audio gap** (above; owner HIGH priority, START HERE).
- **#4 — the 10 s Shift-seek gap** (~1.2–1.6 s, owner-confirmed still bad). MEASURED to be
  entirely the **video recreate+run-up** (settle residual is 0 — audio is blameless). Fix
  tiers logged in the task: cheap (raise hop cap + codec-aware in-place seek) = modest
  ~0.8–1 s; the real fix (NV12 software decode + in-shader YUV so run-up skips MF's RGB
  conversion) = ~400–600 ms but it's the deferred **task 79.10 planar path**, multi-day,
  **video-color regression risk**. RECOMMENDATION: **defer** — a 10 s Shift-tap is a coarse
  deliberate jump that tolerates ~1 s; scope the good fix WITH 79.10.
- **#1 — MF poster deep-walk** (Windows video posters are **pure black** — owner-reported,
  then MEASURED luma **0.000** on Arrival / Apollo 13 / A Christmas Story via
  `PB_VIDEO_POSTER_CLIP`). Owner-approved fix; full scope here so it survives a clear:

  **Root cause.** `crates/pb-decode/src/mf_poster.rs` still runs the *original* walk:
  sample ≤30 frames / ≤1 s of media, accept the first frame with mean luma >10%, else
  fall back to the **last sampled** frame. A feature film is black/studio-logo/fade for its
  first 30–90 s, so the 1 s budget exhausts inside the black lead-in and the "fallback" is
  another black frame. Meanwhile the **macOS/FFmpeg** path (`crates/pb-decode/src/ffmpeg/poster.rs`)
  evolved past this and is the reference to port.

  **The port** (mirror `ffmpeg/poster.rs`, MF-specific where noted):
  1. **Scored walk, best-so-far.** Score each candidate with `poster_frame_score`
     (contrast std-dev + brightness — already platform-neutral in
     `crates/pb-decode/src/video.rs`); keep the best frame, not the last. Stop early on a
     frame ≥ `POSTER_GOOD_SCORE`.
  2. **Deep seek past the intro.** If the head walk (≤30 frames from the start) finds
     nothing good, seek to `POSTER_SEEK_OFFSETS` (8 s → 20 s → 45 s → 90 s), 12-frame
     bursts, capped at `poster_deep_cap` = min(half the clip, 5 min); stop at the first
     good frame. Lift `POSTER_SEEK_OFFSETS` / `POSTER_DEEP_MIN` / `poster_deep_cap` /
     `POSTER_BURST_FRAMES` from `ffmpeg/poster.rs` **into `video.rs`** so both backends read
     one policy (they'll drift otherwise).
  3. **MF seek = RECREATE the reader per offset**, NOT `SetCurrentPosition` on the warm one
     (warm HEVC reposition blocks ~1 s — spike; a fresh open is ~86 ms even over SMB;
     `mf_poster::reopen_at` + `retire_reader` already tear old ones down off-thread). Score
     at the already-fitted decode size (MF's processor emits fitted frames anyway).
  4. **15 s overall deadline** + best-so-far fallback (never the last frame).

  **Why the album use case is still served:** a home video/personal clip opens on content,
  so it settles in the head walk with **no seeking** (zero added cost) — only a
  dark/logo/fade opening pays the deep seek. That's the whole point: albums stay instant,
  films stop being black.

  **Verify:** `PB_VIDEO_POSTER_CLIP=<file>` prints codec/luma/time (already in `mf_poster.rs`
  tests, `#[ignore]`); land a committed fixture with a >1 s black lead + a bright scene at
  the first seek offset to lock the deep-seek in CI. Before/after luma over the corpus.

# 📓 Load-bearing knowledge (don't re-derive)
- The WASAPI reseek is **~10 ms** (FFmpeg audio seek 30–100 µs) — cheap. That's why eager
  audio-commit is safe, and it means the pause/play gap is a **different** path, not the seek.
- MF enumerates audio streams in a **different order** than the container/FFmpeg — hence the
  two-currency (`ff`/`mf`) locators and why the "bridge" was wrong.
- Windows audio is the **master clock** while playing; a mid-play re-open/re-prime is the
  delicate half. The session treats a `Failed` clock as **terminal** ("never resurrected") —
  relevant if pause/play ever needs a late-audio handback.
- Corpus: `\\beenas\Media\Movies` (mostly **x264/H.264**; Aladdin = DTS main + AC-3
  commentaries, Ad.Astra = DDP 7.1, both good test films). Fixture: `multitrack.mp4`
  (2 AAC tracks, tone-labelled).
- ⚠ **The owner drives the app while you work** — the running exe locks
  `target\debug\blazeviewer.exe` (rebuild fails "Access is denied"); that's THEM, don't kill
  it, they'll relaunch. `Get-Process blazeviewer` to check.
- Merge to main: **parallel tracks push fast** — `git fetch origin main && git merge` right
  before pushing; expect to re-merge once or twice. Push with `git push origin HEAD:main`
  (main is checked out in the owner's other worktree, can't `git checkout main` here).

---

# ⏸ Parallel track — macOS `#106` performance (NOT this thread)

The authoritative blueprint is **`.taskmaster/plans/106-performance-archive-zoom.md`**
(rev2, Codex-reviewed) — full design, file:line anchors, acceptance tests. Headline: **every
Fit↔1:1 toggle re-decodes the whole ring** because decode-to-fit discards the full-res
(`common.rs:84`); #106.7 (typed `Representation::Fit{epoch}|Original`, parked full-res tier,
synchronous re-present, real eviction) fixes it. Baseline (SMB `album.zip`, 8×36 MP JPEG):
cold open→first **7068 ms** (one-time cold read), warm resize Fit↔1:1 **390–474 ms**, warm
decode **~400 ms**/item. `PB_PERF=1` on the mac host. Shipped so far: door card (`d91666a0`),
`pb_app_core::perf` timers (`fdcedd16`), #106.2 read/decode split (`5d8eebe1`).

# ⏸ Parallel track — Windows door gating + Copy (#105.2 / #107)

Door renders correctly on Windows. Owed: gate OCR/Describe/Compare **off** on a door
(`MenuState._enabled`); #107 relabel "Copy Image"→"Copy" + emit file-only on a door; interactive
smoke tests (`P` opens / `Alt+↑` climbs out, archive thumbnails, all-archives folder).
