# Cross-platform FFmpeg video — Linux video playback + macOS MKV/WebM codec coverage

**Status:** planned — awaiting owner review before execution (2026-07-12).
**Relates to:** task #79 (video playback, tier 2 — "Windows shipped; Linux/macOS = parity
work"), task #77 (LGPL static-link compliance — a **hard gate for public release**).
**Supersedes/absorbs:** the "MKV/WebM stay on the placeholder" limitation noted in
`macos-archive-video-posters.md` and the loose-file `NativeVideoPlayer` MKV/WebM gap.

---

## 1. Goal

Two features, **one shared engine**:

1. **Linux video playback** (task #79 parity) — Linux has no video playback at all today.
2. **macOS MKV/WebM (and AVI/WMV/…) codec coverage** — AVFoundation can't demux Matroska/
   WebM or decode VP8/VP9, so those containers currently degrade to the placeholder + a
   "can't play" toast on macOS.

Both are solved by the same missing component: a **cross-platform FFmpeg video producer**
that feeds the existing `VideoSession`. Building it for Linux gives macOS the codec coverage
as a near-free rider (and vice-versa).

## 2. The key architectural finding (why this is cheaper than it looks)

The video **display path is already cross-platform and live on every OS.** In
`pb-app-core/src/app_core_impl.rs`:
- `poll_video()` (≈:6618) and `present_video_frame()` (≈:6754) are **not** `#[cfg(windows)]`.
- `poll_video()` runs unconditionally in the tick (≈:1179).
- `present_video_frame()` uploads the decoded frame via `renderer.set_image(...)` — the same
  wgpu path a photo uses, which exists on macOS (Metal) and Linux (Vulkan/GL) already.

So a `Session`-backed video renders its frames through the **existing wgpu canvas** — no
AVPlayerLayer on macOS, no new present path on Linux. The `ActiveVideoBackend` facade already
splits `Session` (producer-fed) vs `Native` (macOS AVPlayer). **The only Windows-only piece
is the producer** (`run_video_producer` → `mf_video_producer`, `#[cfg(windows)]`). On macOS
the `Session` machinery is compiled and driven but never *constructed* (the macOS
`start_video_session` arm always builds `Native`); on Linux no video session is constructed
at all yet.

**Conclusion:** the reusable heart is a single platform-neutral `ff_video_producer`. Wiring it
in is small on each platform; the per-platform *tails* are audio backends and library
bundling.

## 3. Decision

Use **FFmpeg via the existing `ffmpeg-next` 8.1 dependency** (already in `pb-decode/Cargo.toml`
behind the `livephoto` feature; `ff_live.rs` proves the glue works). FFmpeg is the pragmatic
"one dep does everything" choice: it demuxes MKV/WebM/AVI/MP4/… and decodes VP8/VP9/AV1/H.264/
HEVC + all the audio codecs.

**Rejected alternatives:**
- *Pure-Rust Matroska demux + dav1d (AV1) + a VP9 decoder:* more work for less coverage —
  VP8/VP9 have no production-grade pure-Rust decoders, and hardware VP9 via VideoToolbox means
  hand-wiring `VTDecompressionSession`. dav1d (already vendored for Windows avis) covers only
  AV1, not the VP8/VP9 that dominate real WebM.
- *GStreamer:* heavy, painful to bundle, no upside over FFmpeg here.

**Non-goal:** we do **not** replace the macOS `Native`/AVPlayer path for the formats it
already handles well (MP4/MOV/H.264/HEVC + HDR + hardware decode + system audio). AVPlayer
stays the default on macOS; the FFmpeg `Session` backend handles **only** the containers
AVFoundation refuses. On Linux, *all* video goes through the `Session`/FFmpeg backend (no
AVPlayer exists there).

## 4. The producer ↔ session contract (what `ff_video_producer` must implement)

Match `mf_video_producer` exactly — the `VideoSession` is platform-neutral and already
unit-tested against this protocol (`pb-decode/src/video.rs`):

Signature (mirror `run_video_producer`):
```rust
pub fn run_ff_video_producer(
    input: &VideoInput,               // Path or in-RAM Bytes (archive entries)
    fit: Option<FitBox>,
    session_id: VideoSessionId,
    generation: SeekGeneration,
    events: Sender<VideoProducerEvent>,
    msgs: Receiver<VideoProducerMsg>,
)
```

- **`VideoProducerEvent`**: `Opened { duration, width, height, has_audio, frame_bytes }` first,
  then `Frame(VideoFrame)` per credit, `EndOfStream { seek_generation }`, `Failed { error }`.
  `frame_bytes` must reflect the **real negotiated output** (RGBA8 to start; NV12 later — see
  §8) so the session's byte-budget credit accounting is correct.
- **`VideoProducerMsg`** (single merged channel — the only blocking point is `recv()`, so
  `Stop` is never deafened by backpressure): `Credit` (decode + send exactly one frame),
  `SeekTo { target, generation }` (recreate/reposition the reader at `target`, discard frames
  before it, stamp everything after with `generation`; **zeroes the credit balance** —
  latest-value wins), `Stop` (teardown; channel disconnect means the same).
- **Timing:** `VideoFrame.pts` is session-relative (normalize a nonzero/negative container
  start), rational-derived (`Duration`, never an accumulated float), carrying `session_id` +
  `seek_generation` so a stale post-flush frame is dropped at the consumer.
- **Color:** carry `VideoColorInfo` (primaries + transfer as `ColorTransform`; the container's
  `colr`/CICP where present) so the existing in-shader CMS applies — parity with the MF path.
- **Output pixel format:** RGBA8 first (software `sws_scale` to RGBA), matching the tier-2
  shipped baseline. NV12 + in-shader YUV is the reserved perf escalation (§8).
- **Poster:** a one-shot decode of the first non-black frame (reuse the shared
  `poster_frame_bright_enough` mean-luma walk), so `decode_video_poster`/`_input` get an
  FFmpeg backend on Linux/macOS for these containers — same seam `engine.rs` already calls.

## 5. Audio (the per-platform tail; phaseable)

The `VideoSession` uses a **shell-side audio player as the master clock**, fed back via
`AudioClockSample`s (`VideoSession::on_audio_clock`, ~4 Hz), with the shell servicing these
`CoreEffect`s: `StartVideoAudio { path/bytes, at }`, `StopVideoAudio`, `PauseVideoAudio`,
`ResumeVideoAudio`, `SeekVideoAudio { position }`, `SetVideoAudioMuted(bool)`. Windows uses a
WinRT `MediaPlayer` (audio-only). There is **no** `Session`-video audio backend on macOS or
Linux yet.

- **Phase C** adds a cross-platform PCM audio path: FFmpeg decodes the audio stream → PCM;
  a platform sink plays it and reports position as the clock.
  - **macOS:** `AVAudioEngine`/`AVAudioPlayerNode` (or a simpler `AudioQueue`). Note this is
    the `Session` audio only — `Native`/AVPlayer videos keep their own system audio.
  - **Linux:** PipeWire (the Live-Photo path already shells out to `pw-cat`; evaluate a proper
    sink vs reusing that).
- **Phase A/B ship video-only (muted)** to validate the producer + display + seek before audio
  lands. Acceptable interim (the item still plays, silently — same graceful-degradation
  contract as "audio codec unsupported").

## 6. Scope: shared vs per-platform

| Piece | Shared (Rust) | Per-platform |
|---|---|---|
| `ff_video_producer` (demux/decode/seek/EOS/poster) | ✅ all of it | — |
| Backend selection in `start_video_session` | facade exists | macOS: route MKV/WebM/AVI/WMV/… → `Session`; keep MP4/MOV/HEVC → `Native`. Linux: **all** video → `Session`. |
| Frame display | ✅ `present_video_frame` → wgpu | none (already works) |
| Audio | session clock model exists | macOS sink + Linux sink (Phase C) |
| Library bundling | — | **Linux already bundles FFmpeg** (`release-linux.sh` ships `--features livephoto` → linuxdeploy bundles FFmpeg + codecs). **macOS newly bundles FFmpeg** (currently ImageIO/AVFoundation-only). |
| Licensing (#77) | — | LGPL/GPL review for the FFmpeg build config + codecs on each shipped platform. |

## 7. Phasing (execution order — "start on Mac, finalize on Linux")

- **Phase A — the FFmpeg video producer (cross-platform Rust).** New `ff_video_producer.rs`
  behind a feature (e.g. `ffvideo`, likely folded into/alongside `livephoto`). Implements the
  §4 contract: open (Path + Bytes), `Opened`, credit-driven decode → RGBA8 `Frame`, `SeekTo`
  (reader reposition), `EndOfStream`, `Failed`; plus the poster one-shot. **Unit + integration
  tested on macOS** against real MKV/WebM/VP9/AV1 fixtures (add small clips to
  `tests/fixtures/video`). *The bulk of the work — roughly the size of `mf_video_producer`.*
- **Phase B — macOS routing + display.** `start_video_session` (macOS): build
  `Session(ff producer)` for AVFoundation-unsupported containers, keep `Native` otherwise;
  same split for the poster path in `engine.rs`. Video-only (muted). **Deliverable: MKV/WebM
  play + poster on macOS** through the existing wgpu path. Owner-validated on Mac.
- **Phase C — audio.** Cross-platform PCM sink + the `Start/Stop/Pause/Resume/Seek/Mute
  VideoAudio` effects + `AudioClockSample` feedback. macOS sink first (fast iteration), then
  Linux.
- **Phase D — Linux bring-up + finalize.** `start_video_session` (Linux): route **all** video
  → `Session(ff producer)`; confirm FFmpeg is in the AppImage (extend the bundle if needed);
  PipeWire audio; **real-hardware validation + polish** (this is the "finalize on Linux" step
  and the actual task-#79 Linux parity deliverable).
- **Gate — task #77.** LGPL/GPL compliance for the FFmpeg build + codecs on macOS and Linux
  **before any public release** carrying this. Windows is unaffected (MF/OS codecs).

## 8. Performance / hardware decode (reserved escalation, not v1)

Ship **software decode → RGBA8** first (the tier-2 baseline). If 4K VP9/AV1 is too heavy on the
CPU pool: (a) **NV12 output + in-shader YUV** (the session already carries `VideoColorInfo` +
`frame_bytes` is format-aware for this), then (b) **hardware decode** — VideoToolbox (macOS:
VP9 on Apple Silicon, AV1 on M3+; via FFmpeg `hwaccel` or a native `VTDecompressionSession`),
VAAPI (Linux). This mirrors the Windows 79.10 hw-decode path and is gated on a measured need
(ADR-012 kill-criterion style), never done speculatively.

## 9. Container / codec matrix (target coverage)

- **New via FFmpeg:** MKV (Matroska), WebM, AVI, WMV/ASF, MPG/MPEG, MTS/M2TS, 3GP — and their
  codecs VP8/VP9/AV1/H.264/HEVC/MPEG-4/… + audio (Vorbis/Opus/AAC/AC3/…).
- **macOS keeps `Native`/AVPlayer** for MP4/MOV/M4V/QT (H.264/HEVC/ProRes) — better HDR,
  hardware decode, and system A/V sync than the software FFmpeg path.
- **Linux** runs everything through FFmpeg (no AVPlayer).
- **Out of scope / graceful floor:** DRM-protected streams; exotic/rare codecs FFmpeg's build
  omits → the existing placeholder + "can't play" toast.

## 9a. Format registration (extensions, pickers, associations)

**The target set is already fully registered** — MKV/WebM/AVI/WMV/ASF/MPG/MPEG/MTS/M2TS/3GP/
3G2 were added to every list when they became *recognized-but-unplayable*, so this plan only
adds *playback*, not selectability. Audited 2026-07-12, all present:

1. **Recognition** — `VideoContainer::from_extension` + `LibraryItemKind` (`pb-app-core/src/video.rs`)
2. **Archive byte-stream MIME** — `video_content_type` (`pb-decode/src/video.rs`) — needed so
   archive-entry playback resolves the container handler
3. **Windows Open picker** — `VIDEO_FILTER_EXTS` (`pb-app/src/main.rs`)
4. **macOS Open panel** — the ext list in `presentOpenPanel` (`mac/…/CoreModel.swift`)
5. **Windows default-app / file-association** — `VIDEO_EXTS` (`pb-app/src/default_app.rs`)
6. **Linux desktop MIME** — the `MimeType=` line in the generated `.desktop`
   (`scripts/release-linux.sh`)

**Checklist — if FFmpeg lets us opportunistically enable a container NOT in the target set
above** (e.g. FLV, OGV/OGG, TS, VOB, MXF, F4V), add its extension(s) to **all six** sites in
lockstep, or it will decode but be unselectable / not open-with-able. This is the easily-missed
step; treat it as part of "adding a format," not an afterthought. (A future refactor could
collapse 1+3+4+5 to one shared list to make drift impossible — noted, not required here.)

## 10. Risks / open questions

- **Scale.** The producer alone ≈ `mf_video_producer` (substantial: demand-driven, seek,
  byte/frame budget, EOS park). Plus audio + bundling. Multi-session feature; sequence the
  phases and keep each shippable.
- **`ffmpeg-next` API surface.** `ff_live.rs` uses `codec`/`format`/`software-scaling`. Seeking
  (`av_seek_frame`/`avformat_seek_file`) + reader reposition need care to match the "fresh
  reader positions in ~0 ms, warm reposition blocks" behavior the session expects. Validate the
  seek model early (Phase A) against the session's flush/regrant race contract.
- **Bytes input (archive entries).** FFmpeg from in-RAM bytes needs a custom `AVIOContext`
  (read/seek callbacks over the `Arc<Vec<u8>>`) — the FFmpeg analog of `mem_istream`. Needed so
  archive MKV/WebM work too; scope into Phase A (`VideoInput::Bytes`).
- **Audio clock accuracy.** The session is built around an audio-master clock; a new sink must
  report position precisely or A/V drifts. Validate against the existing session unit tests +
  real clips.
- **Licensing (#77).** Real gate. Decide LGPL-only build (dynamic link, no GPL-only codecs) vs
  the implications of the codecs we enable; document per shipped platform. **Blocks public
  release, not internal dev.**
- **macOS binary size.** Bundling FFmpeg + codecs grows the `.app` notably; measure and decide
  which codecs to include (drop ones AVPlayer already covers to trim, since `Native` handles
  those).
- **Build matrix.** A new feature flag interacts with the existing `livephoto`/`dav1d`/
  `libheif` gates and the CI lanes; keep non-video builds (benches, headless) clean.

## 11. Testing / validation

- **Unit (Rust, any platform):** the producer against the session protocol (fixtures: small
  VP9/AV1/H.264-in-MKV/WebM clips); seek generation + flush/regrant; poster mean-luma walk;
  bytes-input `AVIOContext`.
- **Golden/parity:** a Session-decoded frame vs the same clip's known pixels (perceptual diff),
  reusing the existing golden-image harness.
- **macOS (Phase B):** owner smoke — MKV/WebM play + poster + seek/scrub + zoom/pan/rotation
  (rides the existing wgpu/placement path) + no-trace for archive MKV/WebM.
- **Linux (Phase D):** real-hardware playback (4K included), audio A/V sync, AppImage bundling
  verified (the codecs resolve inside the bundle, like the libheif-plugin handling already in
  `release-linux.sh`), seek/scrub, the full container matrix.
- **Regression:** macOS MP4/MOV/HEVC still use `Native`/AVPlayer (unchanged); Windows untouched.

## 12. Acceptance criteria

- Linux: MP4/MOV/MKV/WebM/AVI play with A/V sync, seek, poster, and the full viewer feature
  set (zoom/pan/rotation, info line, prefetch) — task #79 Linux parity **done**.
- macOS: MKV/WebM/AVI/WMV play + poster (loose files **and** archive entries); MP4/MOV/HEVC
  unchanged on `Native`.
- No regression on Windows or on the macOS `Native` path.
- #77 resolved (or explicitly deferred with a documented pre-public-release checklist) before
  shipping.
