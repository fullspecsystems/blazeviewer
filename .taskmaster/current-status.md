# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-11 (end of the marathon video session). Supersedes everything prior._

## State: main @ `b0a4b12`, pushed, all gates green

`cargo test --workspace` (≈700 tests), `cargo clippy --workspace --all-targets -- -D warnings`,
the featured clippy (`pb-decode/pb-app/pb-cli` with `libheif,dav1d`), and `fmt --check` all pass.

**Task #79 (video playback, tier 2): the entire Windows milestone — phases 0 through 7 —
shipped today** (contracts + spikes, typed items, posters, GPU present reuse, the
VideoSession, audio + clock bridge, seeking + contextual input, polish/privacy/perf/docs).
Videos are first-class items: posters, playback with sound, seeking, the playback row in
the info line, mute/slideshow/delete integration, no-trace verified over real playback.
CHANGELOG carries the full user-facing story under `[Unreleased]`.

## ⚡ NEXT: subtask 79.10 — GPU-accelerated decode (OWNER: REQUIRED, not optional)

**Owner verdict (end of session): software playback is only acceptable for the
lowest-resolution videos.** Full-screen 4K60 HEVC is not smooth even after every software
fix landed (credit sizing 387860b, audio double-decode removal + adaptive sampling
70ae3c5). The ADR-012 gate has tripped; GPU decode is now required for the feature.

Measured facts driving the design (all on the target 7680×2160 / RTX 5090 box):
- Software 4K60 HEVC decode ≈ 14.5 ms/frame sustained (~69 fps ceiling, single stream).
- BGRX→RGBA copy ≈ 12.6 ms/frame at 4K, **memory-bound** — a word-wise vectorized rewrite
  changed nothing; the cost includes MF's own `ConvertToContiguousBuffer` internal copy.
- Cost scales with fitted OUTPUT size (a small window is smooth; full screen isn't).
- P→first-frame today: 4K60 HEVC p50 183 ms / p95 320 ms; H.264 p50 53 ms (opt-in test).

The spike ladder (details in tasks.json 79.10; measure each rung before climbing):
1. **`IMF2DBuffer::Lock2D`** on the current path — skip MF's internal copy. Cheapest test.
2. **NV12 output + YUV→RGB in-shader** — 12 bpp vs 32 through every CPU stage (2.7× less);
   pure wgpu/portable; renderer needs a two-plane texture + convert path (the phase-3
   `ReuseSlot` extends naturally); the color contract already rides `VideoColorInfo`.
3. **MF hardware decode**: `MF_SOURCE_READER_D3D_MANAGER` (IMFDXGIDeviceManager) +
   `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS` → NVDEC; decode CPU → ~0. With NV12 CPU
   readback + (2), likely comfortable 4K60. All behind the existing producer seam.
4. **Full zero-copy** (D3D11 shared handle → wgpu DX12 interop, YUV in-shader): the
   endgame, most plumbing. Only if (2)+(3) still fall short.

After 79.10: **79.9 Linux/macOS parity** (producers per platform behind the unchanged
`VideoProducerEvent/Msg` protocol; the session core + all its tests are platform-neutral).

## Video architecture map (what the next session builds on)

- **Contracts** `pb-decode/src/video.rs` + `pb-app-core/src/video.rs`: `VideoFrame`
  (session_id + seek_generation + pts + `PixelFormat` + `VideoColorInfo`), producer
  protocol (`Opened{duration,dims,has_audio}` / `Frame` / `EndOfStream` / `Failed` in;
  **one merged channel** `Credit|SeekTo|Stop` out — backpressure can never deafen
  control; a `SeekTo` zeroes the producer's credit balance so flush is race-free by
  channel order), `LibraryItemKind`/`VideoContainer`, `VideoSessionState` matrix,
  `AudioClockSample`, `VideoQueueBudget` (3× fitted-4K bytes, frame cap 4 = queue +
  in-flight credits).
- **Session** `pb-app-core/src/video_session.rs`: injected-time state machine — preroll
  (2 frames + audio ready-or-absent w/ 1 s degrade-to-silent), rebuffer-don't-drift,
  audio master clock (±50 ms bounded corrections per sample, hard re-anchor > 500 ms,
  post-seek ack gate), 8-step seek (clamp FIRST — MF errors past EOS), paused-seek
  presents once + stays paused, replay = seek-to-0 (the producer PARKS after EOS for
  this). ~20 unit tests on fake producers + a real-producer E2E over the committed
  fixture; opt-in perf test `PB_VIDEO_PERF_CLIP`.
- **Windows producer** `pb-decode/src/mf_video_producer.rs`: demand-driven MF reader
  thread; poster-identical config (`mf_poster.rs`: advanced processing, streams
  deselected, native color, fitted RGB32); seek = retire + recreate the reader
  positioned before first read (warm HEVC reposition blocks ~1 s — spike-locked);
  off-thread bounded reader retirement.
- **Posters/metadata** `mf_poster.rs`: first-non-black mean-luma walk; ~22 ms header
  probe feeds the panel rows; placeholder tile on any failure (diagnostic hresults
  distinguish no-container vs no-codec).
- **Audio** `pb-app/src/video_audio.rs`: WinRT `MediaPlayer` over the file with **video
  tracks deselected** (audio-only decode), opened paused, resumed with the session;
  sampled 30 ms while opening / 250 ms after into `AppCore::video_audio_clock`.
- **AppCore glue** (`app_core_impl.rs`): `video_play_pause` (P), `poll_video` +
  `update_video_progress` in `tick`, `stop_video` rides `stop_playback`,
  `video_seek`/hold-repeat inside `apply_view_holds` (horizontal pan actions seek when
  no horizontal overflow; ±2 s / Shift ±15 s / 200 ms self-timed repeat; OSD toast),
  slideshow suspends while playing, delete releases handles first + bounded retry
  (`poll_delete_retry`).
- **Playback row (owner design)**: the info line (`i`) grows
  `filename · W×H · codec` / `0:42 ▰▰▰▱▱ 9:01`. ⚠ The winit shell renders it in the
  **egui overlay** (`panels_ui.rs::info_line`, `InfoLine.progress`), fed by the now-pub
  `AppCore::video_progress_row()` + `emit_panels_changed` once per displayed second —
  `native_info = true`, so the HUD `render_panel_progress` path is tests/HUD-mode only
  (first attempt landed only there and was invisible; fixed in `b0a4b12`). Width =
  max(summary row, `INFO_BAR_MIN`) — constant per clip, no jitter. `--egui-shot`
  previews it.
- **GPU present path** (phase 3, benefits everything): `set_image` reuses one texture +
  uniform via `ReuseSlot` (1080p p95 1.35→0.42 ms); `StagingUpload` recycles mapped
  staging buffers (bounded pool, never waits).

## Owner smoke status

Confirmed good: placeholder tiles → posters (real footage), silent + audio playback,
the ⅔-speed fix. **Just fixed, needs a re-look:** the playback row (`b0a4b12` — it was
invisible before because of the egui-vs-HUD path). Seeking (phase 6) hasn't had a
dedicated smoke pass. ⚠ Reminder that bit us: **quit the app before rebuilding** — a
running exe makes `cargo build` fail (`os error 5`) and it's easy to relaunch stale.

## Loose ends carried forward

1. **#78 CLI owner smoke** → flip to done (+ CHANGELOG entry if missing).
2. **#76 ARM64 mirror** (setup-libheif on the ARM64 box; check dav1d ARM asm in vcpkg log).
3. **#77 LGPL note**: patent-exposure paragraph — confirm it was appended.
4. Subtasks 79.5–79.8 sit in `review` pending owner smoke sign-off (playback, audio,
   seeking, phase-7 polish).
5. Phase-0 leftovers (not blocking): clean-VM codec sweep for the out-of-box format
   matrix; a real HLG/Dolby-Vision phone clip for the corpus.
6. Deferred perf idea if 79.10's ladder needs company: `Lock2D` measurement doubles as
   rung 1.

## Environment / conventions quick-ref

- Test clips: 4K60 HEVC `D:\Media\Pictures\2019\2019-08-01 - Morinville\IMG_0060.MOV`
  (93 s) and the 5.9 GB `...\2019-12-27 - Nanaimo\IMG_1281.MOV` (the owner's stress
  clip); H.264 VFR `...\2019-07-16\RPReplay_Final1563339583.mp4`; real MKV/MPG under
  `D:\Media\Music Videos`. Committed fixtures: `crates/pb-decode/tests/fixtures/video/`
  (black-lead-in H.264 + AAC-tone; regen commands in its README).
- Spike harness: `cargo run --release -p pb-decode --example video_probe -- spike|sweep …`.
- Opt-in measurements: `PB_VIDEO_PERF_CLIP` (P→first-frame), `PB_VIDEO_POSTER_CLIP`,
  `present_path_churn -- --ignored` (GPU present).
- Design preview: `cargo run --release -p pb-app -- --egui-shot out.png` (includes the
  playback row).
- Plan doc (the spec): `.taskmaster/plans/79-video-playback-tier2.md`; spike results:
  `.taskmaster/docs/79-phase0-spike-results.md`.
- tasks.json edits: PowerShell ConvertFrom/To-Json round-trip; IDs stay numeric.
- Commits: no AI-attribution trailers. Perf verdicts: always **release** builds.
