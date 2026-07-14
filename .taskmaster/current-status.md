# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-14 (rev 2). Supersedes prior status. **0.2.0 shipped** (private beta)._

## ⏳ ACTIVE: Phase 3 — macOS sample-buffer presenter (in progress)

The DoVi/HDR end-state for containers `AVPlayer` can't demux (MKV): FFmpeg (Rust) demuxes →
Swift wraps compressed packets into `CMSampleBuffer`s → `AVSampleBufferDisplayLayer` (system
decode + correct Dolby Vision). Built in test-first slices, all on `main`:

- **Slice A — Rust demux-only packet source** (`68efdb8f`) ✅ `VideoDemuxer` in `pb-decode`:
  extradata (hvcC/avcC) + NAL length + **DoVi config** + compressed packets, no decoder. Unit-tested;
  **verified on the real DoVi corpus** (profile 8.1, `dvvC`, length-prefixed nal_len=4).
- **Slice B — FFI bridge + routing** (`4799353c`) ✅ `CoreEffect::PlaySampleBuffer` + env-gated
  (`PB_SAMPLE_BUFFER`) routing; `DemuxHandle` / `open_stashed_demux` / `demux_*` FFI (mirrors the
  session-audio seam). Reuses the `Native` proxy + `native_video_*` callbacks wholesale.
- **Slice C — Swift `SampleBufferPresenter` (the 0C spike)** (`d818388a`) ✅ compiles + builds a
  `PhotoBlaze.app`. `AVSampleBufferDisplayLayer` under an `AVSampleBufferRenderSynchronizer`,
  reveal-on-first-frame, park-at-EOS, reused AVPlayer transform math; `DemuxReader` builds the
  `CMVideoFormatDescription` (+ `dvvC` box) and feeds sample buffers on renderer backpressure.
  **Video-only** so far.

**🚦 GATE (owner, on-device): does DoVi/HDR render correctly?** Run the dev app, set
`PB_SAMPLE_BUFFER=1`, open an MKV (the Dune corpus), and confirm correct Dolby Vision/HDR on the
physical display — "it decoded" ≠ "DoVi is right." Env-gated OFF by default, so nothing changes for
normal use until this passes. If HDR looks washed-out/wrong, the fix is explicit CICP color
attachments on the format description (cheap follow-up); if it's right, proceed to D/E.

**Remaining Phase 3 slices (after the gate):** D — **audio** under the synchronizer (AC-3 compressed
enqueue probe, else FFmpeg LPCM); E — **real seek** (flush + re-enqueue + re-anchor, generation-safe),
frame-step, archive-bytes input, and replacing the extension-only route with a probed capability +
one-shot Session fallback.

_The rest of task #91 (Phases 0–2) is complete + owner-validated (below)._

## State: `main`, everything pushed (latest `ed6a2aed`)

Video playback on the macOS/FFmpeg (`Session`) backend is now a stable, fast foundation for demanding
media (4K, 10-bit, HDR10/HLG/**Dolby Vision**, TrueHD/Atmos): smooth playback, near-instant seeking,
GPU HDR color, fast posters. Owner confirmed this cycle: *"Playback seems really smooth and fluid."*

## THE authoritative doc — read this first

**`.taskmaster/docs/video-playback-overhaul.md`** governs: root causes (R1–R12), locked rules,
target architecture, Phases 0–3. Phase 2 plan: **`.taskmaster/plans/91-phase2-gpu-planar-color.md`**
(Codex-reviewed twice). Session findings/gotchas: auto-memory `video-playback-overhaul.md`.
Diagnostics: `PB_VIDEO_DIAG=1`. Tracked by **task #91** (overhaul), **#92** (poster frame selection
on MF/AVFoundation), **#98** (Details track listings — merged this cycle), **#99** (playback-bar
track cycling — new).

## Done — the overhaul (task #91)

- **Phase 1 (reliability)** — feasible preroll (R1), audio-continuous starvation (R2), drop-late
  (R3), one-audio-commit-per-seek (R4), owned off-main audio decoder (R5/R10/R12), network-seek
  audio fix (seek the video Cues, not the audio index), 1G bounded-retry / device-wake reprime.
  Owner-confirmed smooth. Details in the prior status / the doc's §7.
- **Phase 2 (GPU planar color) — LANDED + measured.** The FFmpeg producer negotiates output format
  on the first frame and emits planar **NV12 (SDR) / P010 (10-bit + HDR)**; the GPU does YUV + range
  + PQ/HLG + BT.2020→709 in `fs_scene_planar` (goldens vs an independent from-spec reference). CPU
  convert (R6) + R8 threads off the fast path; parallel RGBA/fp16 fallback kept for
  rotated/anamorphic/4:4:4/no-16bit-norm. HDR peak metadata-driven (R11 retired). **Measured 0D A/B
  (Dune 4K DoVi/HDR10+): 1.48× → 8.45× real-time (35.5 → 203 fps), 5.72× faster.**
  `PB_VIDEO_NO_PLANAR=1` = escape hatch. Slices: contract `3a81b95d`, render `35348f03`, peak
  `e349608e`, producer `62c28aac`, measure `dff1355b`.
- **0D verdict:** decode+convert-bound, NOT network (~15× headroom) → **full 1F deshelved**; Phase 2
  was the real win.

## Done — this session (2026-07-14, all on `main`)

- **Video posters ~2.4× faster** (`cfd55ffe`). Was ~1.5s on a 4K HDR film (software decode +
  converting every candidate frame at 4K); now `0.63s`: **hardware decode** in the poster walk
  (`decoder_for` attaches VideoToolbox/VAAPI like playback) + a **two-scale walk** (score each
  candidate at ≤480px — brightness is scale-invariant, same frame wins — then convert only the
  winner at full fit). Golden tests unchanged. This is what made an initial video item stop sitting
  blank for seconds. Gated bench: `poster_generation_time` (`PB_NET_TEST_MKV`).
- **Zoom/pan smoothness (#67)** (`338abda8` + `4ebf1233`). New `hold_ramp` = quadratic ease-in so a
  tap barely moves (fine control) and speed builds on hold; zoom & pan share the curve. Lower floors
  (ZOOM_MIN 0.5→0.35→**0.18**, PAN_MIN 450→340→**170** — the tap speed, halved per owner for ~1–3 px
  taps on the 240 Hz display), slightly lower ceilings (ZOOM_MAX 2.5→2.2, PAN_MAX 3200→2700), longer
  ramp (0.7→0.9s). All one-line tunables in `engine.rs`.
- **Media-track catalog (#98)** — merged (`c56113a8`). Inspector Details (`Shift+I`) lists every
  audio + subtitle track (language, codec, channels, default/forced/commentary/SDH); off-event-loop
  probe; archive parity. The merge also brought `cacf2a67`, which **fixes a real dead-code clippy
  warning in my Phase-2 `planar_video_options`** (only referenced from a cfg-gated block → failed
  `clippy -D warnings` on a macOS **non-ffvideo** build, i.e. what `release-macos.sh` ships).
  Verified clippy-clean + tests green on **both** feature sets.
- **Initial-video-poster bug** — root cause was the slow poster decode (fixed above) plus a
  launch geometry-epoch race; the core present path is provably correct (regression test
  `initial_video_poster_presents_when_it_lands`). Owner reports it's better now.
- **Tree-scan poster latency — investigated, measured, likely resolved** (`ed6a2aed`). Owner: a
  tree-structured NAS folder seemed to wait for the whole scan before the first poster; flat folders
  posted instantly. Measured on `/Volumes/Media/TV Shows` (881 files/58 dirs over SMB): first file
  **169ms**, total scan 4.26s; poster decode **343ms alone vs 128ms during a concurrent walk** (the
  walk warms the SMB cache — so **no contention**, and **no logic gate** on `scanning`). So in
  isolation the poster should present <1s. Owner now says *"things are better"* (the poster
  speedup). **If it recurs:** the delay is in the live pipeline (pool scheduling on an all-video
  folder / launch epoch bump / macOS shell render), NOT the scan — add a `PB_VIDEO_DIAG` timestamp
  (poster requested→completed→presented) to pin the stage. Gated benches:
  `scan_first_file_vs_total`, `scan_contends_with_the_poster` (`PB_SCAN_DIR`).

## Next (in priority order)

1. **Phase 3 — Apple `AVSampleBuffer` presenter** (the macOS end-state, NOT yet started).
   FFmpeg-demux → system decode → `AVSampleBufferDisplayLayer`, for: **correct Dolby Vision** (today
   we render the HDR10 base layer only — honest but not full DoVi), lower CPU via system decode, one
   presentation clock, and #5 zoom/pan via layer transform. This is the biggest remaining item and
   is what "fully done" would mean for the macOS video story. Design it against the doc's Phase 3 spec.
2. **Phase 2 follow-ons** (deferred, plan §Non-goals; none are blockers): planar **rotation in
   geometry/UV** (portrait phone video currently takes the parallel RGBA fallback), a bounded
   planar-`Vec` pool + **two-plane single-submit upload** (true zero-alloc steady state), and **MF
   P010 on Windows**.
3. **#99 — playback-bar subtitle/audio track cycling** (natural follow-on to the #98 catalog:
   `C` subtitle toggle, `A` audio/subtitle cycling, track picker popover).
4. **#90 — text subtitles** (SRT/mov_text/WebVTT + legibility). Deferred from the beta cycle.
5. **#94.1 — space-pauses-a-playing-video** — deferred UX; owner wants to workshop the
   contextual-key idea before committing to changing a key's meaning by context.

## Owner verification still worth a look (Phase 2)

The HDR **look** on the real 32:9 EDR panel (7680×2160 Odyssey Neo G9) — colors are verified against
a from-spec golden, but a physical-panel gut-check on P010/PQ tone-mapping + highlight headroom is
the last unautomatable check. `PB_VIDEO_NO_PLANAR=1` reverts instantly if anything looks off.

## Build / test the macOS app

- `scripts/build-swift-host.sh` (defaults `--ffvideo` ON; `--no-ffvideo` to opt out) →
  `target/swift-host/release/PhotoBlaze.app`. `--bundle-ffmpeg` = self-contained (release-style).
- ⚠ **Always build `--features ffvideo` when testing video code:** `ActiveVideo` has a SECOND
  literal construction under `cfg(ffvideo/macos)` in `app_core_impl` (~6819) that
  `cargo test -p pb-app-core` alone misses. Also run `cargo clippy -D warnings` on **both** the
  default (no-ffvideo, what macOS ships) and `--features ffvideo` sets — dead-code slips through
  otherwise (the `planar_video_options` case).
- Quit the app before rebuilding (`open` won't relaunch a live app — stale-build trap).
- Perf corpus is on the SMB share `/Volumes/Media/Movies/` (Dune 4K DoVi/HDR10+, Tron). Gated
  `#[ignore]` benches take `PB_NET_TEST_MKV` / `PB_SCAN_DIR`; run release for real numbers.

## Notes carried forward

- **Commit + push directly to main** (owner-authorized); fetch/merge origin/main first — a parallel
  Windows agent also pushes there (`feat/media-track-catalog` is their branch; re-merge if it advances).
- swift-bridge bridge module: `//` comments only (`///` panics codegen); non-FFI-able payloads use
  the stash-pull pattern.
- CLAUDE.md states platform-specific behavior as if global — verify perf/behavior against the
  cfg-gated source, not the doc.
- Planar path gotcha: `use ff::format::Pixel::*` brings `Pixel::None` into scope — qualify `Option::None`.
- Older Windows/loose-end items (#80 slideshow×video, #82 macOS archive natives, #75/#76 CI/mirror)
  remain in tasks.json; not part of the overhaul.
