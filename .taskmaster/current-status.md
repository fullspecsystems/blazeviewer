# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-13. Supersedes prior status. **0.2.0 shipped** (private beta);
the active work is the **video playback overhaul** from beta feedback._

## State: `main`, everything pushed

The macOS/FFmpeg (`Session`) video backend is being turned into a stable, robust, efficient
foundation for demanding media (4K, 10-bit, HDR10/HLG/**Dolby Vision**, heavy audio like
TrueHD/Atmos), plus an eventual Apple-stack path for Apple-decodable MKV. Corpus: a 4K DoVi/HDR
HEVC MKV with TrueHD/Atmos 7.1 (`~/Downloads/Ghost*.mkv`).

## THE authoritative doc — read this first

**`.taskmaster/docs/video-playback-overhaul.md`** governs: root-cause inventory (R1–R12),
locked architectural rules, target architecture (routing/capability split), and Phase 0–3 with
per-item specs. Its top `## 0. Status` block is the live progress index. Tracked by **task #91**
(overhaul) and **#92** (poster feature-film selection on the Windows MF + macOS AVFoundation
backends — the FFmpeg backend already shipped). Session findings/gotchas: auto-memory
`video-playback-overhaul.md`. Diagnostics: `PB_VIDEO_DIAG=1` (backend/decode-res + per-seek timing).

## Done (this beta-feedback cycle)

- Beta items shipped: info-line default → center; **poster** feature-film frame selection (FFmpeg
  backend); build-script `--ffvideo` default. Subtitles → task #90 (deferred).
- **Seek perf:** convert-skip on discarded run-up frames (`d1760d0c`) + short-forward-decode +
  parallel-HDR-convert stopgap (R8) + `PB_VIDEO_DIAG` (`4244a69f`) — ~25→~3 ms/frame, 7–10× faster.
- **Phase 1 1A–1D** (see the doc's §7 table for commits): feasible preroll (R1 deadlock dead),
  audio-continuous starvation (R2), drop-late (R3), one-audio-commit-per-seek coordinator (R4).
  Owner-confirmed playback + seeking feel much smoother. `frames_to_units` untangle (`f6fafc3a`, 1E prep).
- **Phase 1 1E — owned off-main audio decoder** (R5 + R10 + R12): **owner-confirmed** smooth local
  playback + seeking, no glitches. The FFmpeg audio decoder moved off `@MainActor` behind a
  **usize-pointer FFI** (`open_stashed_session_audio` + `session_audio_*` free fns over a thread-safe
  global stash), driven on a Swift serial feeder queue (`OwnedAudioDecoder`, frees once in `deinit`);
  `SessionAudioPlayer` rewritten async (Opening clock during the open gap, generation-gated reads).
  R10 = disposition-aware track selection; R12 = Failed distinct from EOF; stash survives a failed open.
- **Network-seek audio fix** (owner-reported, same cycle): seeking a large movie on an SMB share used
  to permanently kill audio. Root cause: the audio decoder sought by the AUDIO stream index → MKV Cues
  (video-only) → byte-position linear scan **~73 s over SMB** (16 GB 4K corpus) → blew the watchdog,
  wedged the demuxer, R12-latched. Fix: seek the **default stream** (video Cues) = **~20-40 ms**, lands
  on target. Reproduced + measured via the `net_seek_read_timing` harness (`PB_NET_TEST_MKV`). Residual
  post-seek network *stutter* (both demuxers re-read) is inherent until 1F. See the doc's §7/1E.

## Done (this cycle, cont.)

- **Task #94.2 — session-only video resume** (both backends): returning to a video resumes near
  where you left off; RAM-only, dropped on quit; watched-to-end restarts. Native (AVPlayer) path via
  a `native_video_progress` FFI + `start_secs` on `PlayVideo`; Session path via `note_video_position`.
- **1G lifecycle — the high-value slices**: terminal-drain (tick-loop spin), bounded-retry recovery
  from mid-playback network stalls (`AudioError` transient/fatal + strikes + Rebuffering clock),
  device-change/sleep-wake audio reprime. The remaining 1G items (session-tag effects, pause-forever,
  replay-after-EOS) were assessed low-value/already-covered by the architecture — see the doc's §7/1G.
- **1F cancel-flag slice + 0D margin trace**: the FFmpeg producer's interrupt flag is armed (stuck
  network read retires the thread promptly). **0D verdict: the bottleneck is video decode+convert
  (~1.19× real-time), NOT network (~15× headroom), audio ~29.5×** — measured with the
  `net_decode_throughput`/`net_audio_throughput` harnesses. **Full 1F deshelved** (no gain above
  ~44 Mbps; 1G bounded-retry covers below). The real margin win is Phase 2 (GPU convert).

- **Phase 2 — GPU P010/NV12 + PQ/HLG shader — LANDED + measured** (plan
  `.taskmaster/plans/91-phase2-gpu-planar-color.md`, Codex-reviewed). The FFmpeg producer negotiates
  the output format on the first frame and emits planar **NV12 (SDR) / P010 (10-bit + HDR)** for
  eligible clips; the GPU does YUV + range + PQ/HLG transfer + BT.2020→709 primaries in
  `fs_scene_planar` (goldens vs an independent from-spec reference). The per-frame CPU convert (R6)
  and its R8 thread fan-out are off the fast path; the parallel RGBA/fp16 path stays as the fallback
  for rotated/anamorphic/4:4:4/no-16bit-norm clips. HDR peak is metadata-driven (R11 running-max
  retired). **Measured 0D A/B (Dune 4K DoVi/HDR10+): 1.48× → 8.45× real-time (35.5 → 203 fps),
  5.72× faster.** `PB_VIDEO_NO_PLANAR=1` = escape hatch. ⚠ **Owner still to verify** live playback
  timing / audio sync (first-frame negotiation) + the HDR look on a real EDR panel.

## Next (in order)

1. **Phase 3 — Apple `AVSampleBuffer` presenter**: FFmpeg-demux → system decode/HDR/**correct
   DoVi**/one clock; also delivers #5 zoom/pan via layer transform. The macOS end-state.
2. **Phase 2 follow-ons** (deferred, plan §Non-goals): planar **rotation in geometry/UV** (portrait
   phone video currently takes the parallel RGBA fallback), a bounded planar-`Vec` pool + two-plane
   single-submit upload (true zero-alloc steady state), and MF P010 on Windows.
3. **1F full packet-source rework — DESHELVED** (0D: decode-bound, not network-bound). Only revisit if
   a genuinely constrained network (<~44 Mbps) proves the 1G bounded-retry degrades badly. The
   optional throttled stress trace to check that needs `dnctl`/`pfctl` (sudo).
   (Acute network pain already fixed by the seek work + 1G bounded-retry; 1F is graceful degradation.)
- Deferred UX call: **#94.1 space-pauses-a-playing-video** (owner wants to workshop the contextual-key idea).

## Build / test the macOS app

- `scripts/build-swift-host.sh` (now **defaults `--ffvideo` ON**; `--no-ffvideo` to opt out) →
  `target/swift-host/release/PhotoBlaze.app`. `--bundle-ffmpeg` = self-contained (release-style).
- Run with diagnostics: `PB_VIDEO_DIAG=1 .../PhotoBlaze.app/Contents/MacOS/PhotoBlaze` from a terminal.
- ⚠ **Always build `--features ffvideo` when testing video code:** `ActiveVideo` has a SECOND
  literal construction under `cfg(ffvideo/macos)` in `app_core_impl` (~6819) that
  `cargo test -p pb-app-core` alone misses.
- Quit the app before rebuilding (`open` won't relaunch a live app — stale-build trap).

## Notes carried forward

- swift-bridge bridge module: `//` comments only (`///` panics codegen); non-FFI-able payloads use
  the stash-pull pattern (the audio `VideoInput` stash is the 1E case).
- CLAUDE.md states platform-specific behavior as if global (e.g. "HDR stays on software" is the
  **Windows** path only) — verify perf/behavior claims against the cfg-gated source, not the doc.
- Older Windows/loose-end items (#80 slideshow×video, #82 macOS archive natives, #75/#76 CI/mirror)
  remain in tasks.json; not part of the overhaul.
