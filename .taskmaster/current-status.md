# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-13 (task #84 cross-platform FFmpeg video, phases 1–6 done +
phase 7 foundation). Supersedes everything prior; the pre-#84 Windows-video loose ends
are carried at the bottom._

## State: main, all gates green, everything pushed (latest 238cd556)

Task **#84** (plan: `.taskmaster/plans/cross-platform-ffmpeg-video.md`, **rev4** — owner
rulings locked in the header) executed phases 0–6 + the phase-7 foundation.
macOS/Linux tests, clippy `-D warnings` (all feature flavors), fmt: green.
Owner-confirmed on hardware: **WebM and MKV play on macOS, including a 4K MKV film
streamed over the network, with audio.**

## Overnight additions (2026-07-13)

- **Phase 6 — hardware decode (VideoToolbox/VAAPI), on main (`2abf5d38`).**
  `crates/pb-decode/src/ffmpeg/hw.rs` attaches a VideoToolbox device (macOS, default-on)
  / VAAPI (Linux, opt-in `PB_ENABLE_VAAPI`), then `av_hwframe_transfer_data`s the GPU
  surface to a CPU NV12/P010 frame the existing `FrameConverter` takes unchanged — the
  same GPU-decode → CPU-readback → re-upload pattern the Windows MF path ships. Offloads
  the dominant CPU cost (VP9/AV1/HEVC decode). SW fallback is structural (libavcodec
  get_format re-query; VP8 carve-out; `PB_NO_HWACCEL`). **Validated engaging** on a real
  HEVC clip (`VIDEOTOOLBOX` surfaces → NV12 1920×1440). Deferred by design (measure-first):
  the NV12-in-shader fast path (skip CPU swscale) + true zero-copy IOSurface→wgpu.
  **Owner: launch a 4K VP9/AV1/HEVC clip and watch CPU/smoothness — that's the §10 gate.**
- **Phase 7 — distribution foundation, on main (`238cd556`).** A pinned, LGPL, decode-only,
  VideoToolbox, `@rpath` FFmpeg (`scripts/build-ffmpeg-macos.sh`, ~14 MB) + bundling +
  closure audit (`scripts/bundle-ffmpeg-macos.sh`) + `build-swift-host.sh --bundle-ffmpeg`.
  Proven end-to-end: dyld loads all 5 FFmpeg dylibs from the app's own Frameworks, **zero
  Homebrew**. `THIRD-PARTY-NOTICES.md` FFmpeg section + `.taskmaster/docs/ffmpeg-compliance.md`
  (#77 manifest). **Owner-gated remainder** (creds/HW): release-macos.sh integration +
  Developer-ID inside-out signing + notarization + add ffvideo to ship features + CHANGELOG.

## What shipped today (all on main)

- **`pb-decode/src/ffmpeg/`** behind `ffmpeg`/`ffvideo` features (`livephoto` re-based on
  `ffmpeg`): `io.rs` custom AVIO over `Arc` bytes + interrupt-callback cancellation with
  per-op watchdogs (plan §6 proof gate, in production code); `probe.rs` non-attached-pic
  stream selection, rotation, SAR, start-time; `color.rs` CICP resolution + SDR display
  convention + **explicit swscale coefficients** (default silently assumes BT.601);
  `convert.rs` decode-to-fit-in-swscale + rotation + frame-over-decoder color precedence;
  `pcm.rs` hand interleaving (swresample's FFmpeg-8 layout churn stays sidestepped).
- **`run_ff_video_producer`** — exact `VideoProducerEvent/Msg` protocol parity with the MF
  producer (same `VideoSession`, protocol tests ported + green): in-place
  `avformat_seek_file` seek, EOS park + replay, latest-seek-wins, corrupt-input budgets.
  **Poster + probe** with the shared mean-luma walk. Fixtures: VP8/VP9 WebM, H.264 MKV,
  rotated90, HDR PQ, 5.1 AAC, VP9+Opus (all lavfi, `tests/fixtures/video/README.md`).
- **fp16 HDR path (§9, owner decision #1)**: PQ/HLG → LUT-driven scene-linear scRGB f16
  (1.0 = 203 nits, mirrors `finalize_hdr_scrgb`), BT.2020→709, running peak;
  `present_video_frame` routes `Rgba16F` through the stills HDR arm (was SDR-mishandled);
  `VideoColorInfo` gained `peak` (MF sites stamp 1.0).
- **Linux integration (phase 3)**: all Linux video → Session via the FFmpeg producer;
  posters/probe wired; validated in the appimage container (arm64 OrbStack image;
  `libswresample-dev` added to `appimage.Dockerfile` — apt-install it ad hoc until the
  image is rebuilt). Also fixed **pre-existing winit-shell breakage**: 9 AppCore fields
  missing from main.rs's constructor (pb-app hadn't compiled on Windows/Linux since the
  macOS archive-video work) — Windows agent should pull.
- **macOS dual-backend (phase 4, §8)**: `VideoContainer::macos_native` level-1 routing
  (MKV/WebM/WMV/MPEG-PS/AVCHD → FFmpeg session; MP4/MOV/3GP/AVI → AVPlayer); level-2
  fallback on *classified* recoverable native failures (Swift maps NSError domains; DRM/
  missing-file/permission/network never fall back; flag consumed, no loops, one final
  error max); FFI `video_session_active/elapsed/duration/playing` + `video_seek_fraction`;
  SwiftUI controls/scrubber backend-blind; posters/probe keep-both (AVFoundation primary,
  FFmpeg on refusal — incl. archive entries with non-native containers).
- **Streaming audio (phase 5, §7)**: `FfAudioDecoder` (pull-based, constant-memory,
  in-place seek, `open_capped(input, 2)` folds 5.1/7.1 → stereo); macOS
  `SessionAudioPlayer.swift` (AVAudioEngine/AVAudioPlayerNode over `video_audio_*` FFI,
  ~250 ms buffers ×3, clock = rendered sampleTime − presentationLatency, ~4 Hz to
  `video_audio_clock`); Linux pw-cat **streaming** sink in `pb-app/src/video_audio.rs`
  (SIGSTOP pause, seek = respawn, clock = frames-written − 150 ms characterized estimate,
  §7 option b); producer `has_audio` now honest.
- **Crash fix (owner report, SIGABRT on an MKV)**: AVAudioEngine throws **ObjC
  exceptions Swift can't catch** — `mac/Sources/PBCatch` shim wraps every engine call
  (degrade to silent, never abort); engine graph uses STANDARD (deinterleaved) formats
  only; stereo cap in Rust. Bonus bug found: ffmpeg-next's `frame.data(i)` sizes audio
  planes from `linesize[i]` (only [0] is filled) → **all channels but the first were
  silent** (stereo played left-only). Fixed in `pcm.rs` via `extended_data`. Verified by
  replaying the exact crash scenario in-app (5.1 MKV + P: plays, alive).

## Build / test the macOS app

- `scripts/build-swift-host.sh --ffvideo` → `target/swift-host/release/PhotoBlaze.app`
  (DEV: links Homebrew FFmpeg, `brew install ffmpeg` required).
- `scripts/build-swift-host.sh --bundle-ffmpeg` → the same but **self-contained** (bundles
  the pinned LGPL FFmpeg into `Contents/Frameworks`, no Homebrew needed to run; ad-hoc
  signed). Builds FFmpeg on first use (~10-20 min), then instant.
- **Neither is in any release script** — ship-gating is by discipline (owner ruling) until
  phases 5–6 are hardware-validated and the Developer-ID signing integration lands.

## Next (in order) — mostly owner-in-the-loop now

1. **Owner morning validation** (the point of the overnight work):
   - **Phase 6 §10 gate**: launch a 4K VP9/AV1/HEVC clip, watch CPU + smoothness with HW
     decode on (default) vs `PB_NO_HWACCEL=1`. That decides whether the deferred
     NV12-in-shader fast path (skip CPU swscale) is even needed.
   - Confirm phases 5–6 (audio, dual-backend) hold on real content; re-check the possible
     4K-over-network audio glitches locally (screen sharing is the prime suspect).
   - Try `--bundle-ffmpeg` and confirm the self-contained .app plays video.
2. **Phase 7 ship integration (owner-gated, needs creds/HW)**: wire `build-ffmpeg-macos.sh`
   + `bundle-ffmpeg-macos.sh` into `release-macos.sh`; Developer-ID **inside-out** signing
   (FFmpeg dylibs before the app) + notarization + a clean-machine launch; add `ffvideo` to
   the macOS ship feature set; the Linux clean-container AppImage audit; universal-vs-arm64
   decision; PGP-verify the pinned tarball. Then the **CHANGELOG** entry. See
   `.taskmaster/docs/ffmpeg-compliance.md` for the full remaining-tasks list.
3. **Deferred phase-6 optimizations** (measure-first): NV12-in-shader fast path (emit
   `PixelFormat::Nv12`, reuse Windows `set_video_nv12`/`upload_nv12_reusable`; gate on
   SAR=1 + no-rotation) and true zero-copy IOSurface→wgpu (no external-texture ingest
   exists in the renderer). Only if the owner's perf test shows swscale is the bottleneck.
4. **Cleanup**: A/V drift measurement vs the ≤50 ms target; §14 corpus expansion (VFR,
   B-frame-heavy, nonzero start-PTS); §11 displayed-frame Copy/OCR for session videos
   (overlaps #81); re-run the Linux container validation of the pcm.rs fix (OrbStack was
   down); rebuild the appimage builder image (picks up libswresample-dev).

## Session gotchas (also in auto-memory `ffvideo-progress.md`)

- ffmpeg-next: swresample feature = `software-resampling` (`resampling` = dead
  libavresample). `frame.data(i)` is broken for audio planes ≥1 — use `extended_data`.
- `AVIOInterruptCB` fires only inside *blocking* libav work; fast paths need explicit
  cancel checks (the poster walk has them).
- AVAudioEngine: NSExceptions (uncatchable in Swift) from connect/play/scheduleBuffer —
  always via `PBCatch`; standard deinterleaved mono/stereo formats only.
- Linux validation: `docker run … photoblaze-appimage-builder:arm64` with the
  `photoblaze-target-arm64` + `photoblaze-cargo-registry` volumes (see release-linux-
  docker.sh for the incantation); arm64 is native-speed on this Mac.
- swift-bridge bridge module: `//` comments only (`///` panics codegen); stash-pull for
  non-FFI-able payloads (`pending_audio_input` is the newest example).

## Pre-#84 loose ends (carried, mostly Windows-agent territory)

- **79.10 owner smoke matrix** (NVDEC review → done): 4K60 fullscreen acceptance,
  rot90 poster≡playback, HLG-jerky-by-design, hostile pair.
- **HDR decision for Windows** (79.10 open question 8): P010 + PQ/HLG in-shader vs
  documented v1 limitation — note task #84's fp16 machinery (scene-linear LUT convert,
  `VideoColorInfo::peak`, present-path HDR arm) now exists and is reusable rails.
- **#80** slideshow × video policy (owner one-liner pending). **#82** macOS archive
  natives via resource loader — note MKV/WebM archive entries already play via the
  FFmpeg bytes path; #82 is now only about *native-format* entries.
- #76 ARM64 vcpkg mirror, #77 patent-paragraph confirm (now folded into phase 7), #75
  ARM64 CI lane. CHANGELOG `[Unreleased]` heading dedup before the next release roll.
- Owner still stewing on Space = pause-vs-next while a video plays.

## Conventions quick-ref

- Commits: no AI-attribution trailers; perf verdicts from **release** builds only.
- tasks.json: numeric IDs (task #84 subtasks 1–5 done/review, 6=review, 7–8 pending —
  renumbering note: subtask 6 = audio, 7 = HW decode, 8 = distribution).
- Quit the app before rebuilding (`open` won't relaunch; stale-build trap).
- Fixture regen commands live in `crates/pb-decode/tests/fixtures/video/README.md`.
