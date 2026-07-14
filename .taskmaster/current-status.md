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

## Next (in order)

1. **1E — owned off-main audio decoder** (the immediate task; the last audio-glitch leg, R5).
   Extract the decoder out of the shared `&mut` handle behind a **usize-pointer FFI** (swift-bridge
   0.1.59 can't return owned opaque), open/read/seek/free on a Swift feeder queue, fold in **R10**
   (track selection) + **R12** (error state ≠ EOF). **The concrete file:line seam is in the doc's
   §7/1E.** Untestable-here for audio → build → owner-listen → iterate.
2. **1G — lifecycle/failure containment** (session replace/nav/quit cancel + reject stale by
   `session_id`; sleep/wake/device-change reprime; one-error-not-toast-loops; the two audit
   fragilities tagged 1G).
3. **1F — network read-ahead** (original beta item #1, R9): **BLOCKED on the 0D SMB
   characterization spike** — needs the owner's NAS (`/Volumes/{JD,Media,appdata}`). Do 0D first.
4. **Phase 2 — GPU P010/NV12 shader** (the "proper HW HDR"): removes the CPU color convert +
   retires the R8 parallel stopgap; cross-platform + macOS Apple-can't-decode fallback.
5. **Phase 3 — Apple `AVSampleBuffer` presenter**: FFmpeg-demux → system decode/HDR/**correct
   DoVi**/one clock; also delivers #5 zoom/pan via layer transform. The macOS end-state.

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
