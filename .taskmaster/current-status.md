# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-12 (archive video playback; earlier same day: GPU decode + playback
controls + macOS merge). Supersedes everything prior._

## State: main, all gates green

`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
the featured clippy (`libheif,dav1d`), and `fmt --check` all pass.
**`feat/mac-video-playback` is fully merged into main** (verified `HEAD..origin/...` empty).

## What shipped since the last handoff

- **Videos inside ZIP/7z archives now list and play (Windows).** The reported bug: a
  video-only archive refused with "Empty". Root cause was by-design — the archive
  predicate was images-only ("video items are path-only"). Now:
  `scan::is_supported_archive_entry` (images ∪ video containers) feeds ZipSource /
  SevenZSource / the 7z preflight; `item_kind` classifies by entry **name** (was path);
  playback runs through the new `pb_decode::VideoInput { Path | Bytes }` seam — a
  zero-copy read-only `IStream` over `Arc<Vec<u8>>` (`mf_stream.rs`, windows-core
  `#[implement]`) wrapped by `MFCreateMFByteStreamOnStream` → poster/probe/software
  producer/NVDEC/seek-reopen are configuration-identical for path and bytes. Audio plays
  the SAME Arc via `CreateRandomAccessStreamOverStream` → `MediaSource::CreateFromStream`
  (`StartVideoAudio` now carries `VideoInput`; the shared-buffer handoff is
  `ActiveVideo::media`, an `Arc<OnceLock<VideoInput>>` the fetch thread fills before
  `Opened` can land). The bytes fetch happens on the producer thread — never the event
  loop. Guardrails: entries > `pb_source::MAX_ENTRY_BYTES` (1 GiB) are **skipped at
  index time** everywhere (zip open, 7z open drain, 7z projection — an oversized entry
  used to refuse the whole 7z); `PhotoSource::size_hint` feeds the panel's size row
  (probe skipped for archive videos — duration arrives via `Opened`). No-trace holds:
  the zip no-trace test now includes a video poster. **macOS**: archive videos list but
  toast "can't play yet" (AVPlayer is URL-based) — parity is **task #82** (bytes-backed
  AVAsset via a resource loader; FFI, player, poster, no-trace subtasks; #81 was taken
  concurrently by the mac agent's frame-capture task).
  Verified end to end: real-MF tests (producer streams+seeks from bytes, poster/probe
  from bytes, WinRT audio opens from bytes) + app smoke on a corpus video-only zip.

- **79.10 GPU decode (Windows) — implemented, in `review` pending owner smoke.**
  NVDEC via the DXGI device manager + NV12 output + `Lock2DSize` readback + YUV→RGB
  in-shader, gated by pixel rate (> 4K30, SDR transfers only; PQ/HLG stays software).
  Software RGB32 path unchanged as the fallback for any hw setup failure. Measured:
  4K60 HEVC ceiling 72→191 fps; P→first-frame p50 183→127 ms. Spec:
  `.taskmaster/plans/79.10-nvdec-hw-decode.md`; numbers:
  `.taskmaster/docs/79.10-gpu-decode-spike.md`. A/B levers: `PB_VIDEO_FORCE_HW=0|1`,
  `PB_VIDEO_CPU_CONVERT=1`.
- **Playback controls (Windows)**: interactive scrubber bar + knob + play/pause button in
  the info line; `,`/`.` frame-step videos (pause-first; back = paused one-frame seek);
  Shift-seek ±10 s (keymap migration heals saved keymaps that froze the Shift chords);
  hover the bottom quarter reveals the controls (core policy, `flash_video_controls`);
  the seek OSD flashes the info line instead of the old time toast.
- **Stability fixes from owner smoke**: geometry re-decode deferred while a video owns the
  display (the fullscreen-toggle poster-storm jerkiness); resize pauses A/V together and
  the settle resumes them (replaced a reverted clock-heuristic attempt — bf96321 raced
  and seek-churned; never resurrect that approach); drop-focus on Windows.
- **Overlay fades**: info line + folder tree + Inspector fade 100 ms in / 250 ms out via
  `PanelFade<T>` (edge-stamped in `update_overlay` — stamps must NEVER live in renders;
  that was the inconsistency). `sdf_rect`/`sdf_panel` multiply their colors by
  `ui.opacity()` — egui paint callbacks bypass `set_opacity`. Help panel still pops
  (deliberate; two-line change if wanted).
- **79.9 macOS merged**: native AVPlayer behind the `ActiveVideoBackend` facade
  (`pb-app-core/src/video_native.rs`), posters (`av_poster.rs`), scale modes, info-line
  controls + scrubber, toolbar play sync, hover reveal — plus #78 macOS CLI parity.
  Plan: `.taskmaster/plans/79.9-*.md` (rev4).
- **The torture corpus**: `D:\Media\test-videos` — 28 files (H.264/HEVC 8+10-bit,
  HLG/PQ with real `colr` boxes, AV1, VP8/9, MPEG-2/4, MJPEG, WMV, ProRes, 3GP; MKV/TS;
  rot90; VFR/120fps; no-audio/PCM/FLAC; truncated + garbled hostiles). Regen:
  `make-corpus.ps1` (⚠ PowerShell: never name a param `$args`). MF sweep: 26/28 open;
  ProRes = the one codec gap (graceful error); truncated fails cleanly.
  ⚠ MF reads HDR colorimetry from the mp4 `colr` box, NOT the bitstream VUI — synthetic
  HDR clips need `-movflags +write_colr` or the transfer gate can't see them.

## What's left for video v1 (the short list)

1. **Owner smoke matrix** (79.10 `review` → done). Early pass: corpus "works about as
   expected; some glitches on exotic entries presumed encoding artifacts — normal video
   fine." Still needs deliberate eyes on: 4K60 full screen (`h264_4k60_aac.mp4`,
   `hevc_4k60_source.mov` — THE hw-path acceptance), `h264_720p_rot90.mp4`
   (poster ≡ playback orientation), the HLG gate (`hevc_hlg10_4k60.mp4` jerky BY DESIGN
   — it's software), hostile pair fails politely.
2. **The HDR decision (plan 79.10 open question 8).** Modern iPhone 4K60 HDR (HLG/DV)
   takes the software path → not smooth. Ship as documented v1 limitation, or pull
   P010 + PQ/HLG-in-shader into scope (phase-B-sized; the NV12 two-plane path is the
   rails). Blocked on: a real iPhone HDR clip for the corpus (still missing).
3. **#80 slideshow × video policy** (owner one-liner + small tested core change).
4. **macOS remainder** (79.9 `in-progress`, other agent): cursor auto-hide (#68 folded
   in), owner smoke on the physical display, flip to done. They should pull main.
5. Owner is stewing on **Space = pause-while-playing vs next** (contextual, like the
   arrow-key seek). No action until called.

**Explicitly post-v1:** Linux video (whole port is experimental), P010/HDR (unless #2
flips), zero-copy interop, frame-drop engine, backward-frame cache for VFR stepping,
the `f`-toggle audio blip (owner: tolerable), ProRes.

## Other loose ends carried forward

- **#76 ARM64 mirror** (setup-libheif on the ARM64 box); **#77 LGPL note** (confirm the
  patent paragraph); **#75** ARM64 CI lane.
- CHANGELOG `[Unreleased]` has duplicate `Added`/`Changed`/`Fixed` heading groups (merge
  accretion) — consolidate before the next release roll.

## Environment / conventions quick-ref

- Stress clips: 4K60 HEVC `D:\Media\Pictures\2019\2019-08-01 - Morinville\IMG_0060.MOV`,
  the 5.9 GB `...\2019-12-27 - Nanaimo\IMG_1281.MOV`, 22 s `IMG_1283.MOV` (corpus source).
  Torture corpus: `D:\Media\test-videos` (README maps file → what it exercises).
- Harnesses: `video_probe -- spike|sweep|copybench` (copybench = the 79.10 rung
  measurements); opt-in `PB_VIDEO_PERF_CLIP` (P→first-frame), `present_path_churn`.
- Plan docs: `79-video-playback-tier2.md` (tier-2 spec), `79.10-nvdec-hw-decode.md`,
  `79.9-*.md` (macOS); results: `79.10-gpu-decode-spike.md`.
- tasks.json edits: PowerShell ConvertFrom/To-Json round-trip; IDs stay numeric.
- Commits: no AI-attribution trailers. Perf verdicts: always **release** builds.
- ⚠ Quit the app before rebuilding (`os error 5` + stale-relaunch trap).
