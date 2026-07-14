# Video Playback Overhaul — Execution Plan

> Status: **review-hardened, ready for implementation spikes** · Owner: JD
> Started 2026-07-13 from 0.2.0 beta feedback · Revised 2026-07-13 after live-code review
> Revised again 2026-07-13: code audit confirmed R1–R9 with exact constants, added R10–R12 +
> secondary fragilities, recorded the 0A corpus characterization (DoVi 8.1, four audio tracks,
> 15 s GOP), and resolved the first §14 open decision.
> Scope: smooth, seekable, glitch-free playback for native containers and FFmpeg-backed
> MKV/WebM/other fallback media, with correct SDR/HDR behavior and bounded resource use.

The immediate corpus is a 4K Dolby Vision/HDR HEVC MKV with TrueHD/Atmos audio. The plan must
also leave a durable fallback for codecs Apple cannot decode and must not regress Windows or Linux.

## 0. Status (2026-07-13)

- **DONE:** Phase 0 0A/0C (corpus characterized; remux controls at `~/Downloads/pb-remux-control-*.mp4`);
  **Phase 1 1A–1E** on `main` (see §7 table for commits) — deadlock (R1) + post-seek audio glitches
  (R2/R4) fixed, owner-confirmed smoother; plus the seek perf work + `frames_to_units` untangle.
  **1E** (owned off-main audio decoder, R5 + R10 + R12) — owner-confirmed smooth local playback +
  seeking, no glitches. **Plus a big win (owner-reported network bug, fixed 2026-07-13):** the audio
  decoder was seeking by the AUDIO stream index, which MKV Cues don't cover → a byte-position linear
  scan measured **~73 s over SMB** on a 16 GB 4K clip, which blew the watchdog and wedged audio
  ("audio never recovers after a network seek"). Seeking the DEFAULT stream (video Cues) instead =
  **~20-40 ms**, lands on target; audio stays in sync after a jump.
- **NEXT (Opus session):** **1G** — lifecycle/failure containment (session-identity on the audio
  effects; the pause-forever fallback timeout; the two audit fragilities tagged 1G). Then Phase 2/3.
- **BLOCKED:** **1F** network read-ahead needs the **0D SMB spike** first (owner's NAS
  `/Volumes/{JD,Media,appdata}`). **Phase 2** (GPU P010 — the "proper HW HDR", retires the R8 stopgap)
  and **Phase 3** (Apple `AVSampleBuffer` — the DoVi end-state) are the big post-Phase-1 wins.
- Session findings + gotchas: memory `video-playback-overhaul.md`; `PB_VIDEO_DIAG=1` for backend/seek timing.

## 1. Outcome and non-goals

### Required outcome

- Playback does not deadlock at 4K HDR in Fit, Fill, or Original.
- Once preroll completes, a transient video delay does **not** create an audible pause/resume.
- Audio remains the master clock when present; video repeats or drops frames to recover.
- Held-key and scrubber seeks are latest-value, generation-safe, and do not repeatedly tear down
  audio for superseded targets.
- Hardware-decodable media stays hardware decoded. PQ/HLG color work does not run as a large
  per-pixel CPU loop in steady state.
- All queues and pools remain byte-bounded, including 8K/oversized inputs and archive bytes.
- Path-backed playback over a mounted network share uses bounded compressed read-ahead and does not
  turn ordinary network jitter into repeated audio glitches.
- Stop/navigation/quit/seek can retire work promptly; no stale frame or callback can cross a
  session or seek generation.
- Viewing remains read-only and RAM-only under ADR-018.

### Explicit non-goals for this overhaul

- This does **not** promise Atmos object rendering or TrueHD bitstream passthrough. V1 must play
  such files smoothly and honestly report the output actually produced. Channel-preserving PCM is
  desirable where the device accepts it; a documented downmix is an acceptable fallback.
- Subtitles, playback-speed controls, and general-purpose editing/color grading are separate work.
- Linux-specific HDR output is still out of scope. Linux should inherit the planar decode/present
  improvements without holding the macOS release bar.
- Zero-copy is not a requirement if the measured P010/NV12 readback-and-upload path clears the
  target with margin.
- No player can provide uninterrupted playback when sustained source throughput is below the
  stream's long-run bitrate. In that case the contract is a clean, bounded rebuffer and recovery,
  not concealed data starvation. HTTP streaming and streaming from solid 7z entries are not added
  by this overhaul; the network target is a seekable path on a mounted share (initially SMB).

## 2. Current architecture and verified evidence

PhotoBlaze has two product-level video backends:

| Backend | Current use | Media authority | Current status |
|---|---|---|---|
| `Native` | macOS AVFoundation-native MP4/MOV/3GP/some AVI | `AVPlayer` owns decode, HDR/color, audio, timing, buffering, and seek | solid |
| `Session` | Windows Media Foundation; Linux FFmpeg; macOS FFmpeg fallback for MKV/WebM/etc. | `VideoSession` + platform producer + separate audio sink | fragile under demanding MKV/HDR |

Do not call the Windows route “the FFmpeg route”: Windows currently uses the Media Foundation
producer. `VideoSession` is shared; acquisition, audio, and presentation are platform-specific.

### Evidence from the corpus and current code

- **Corpus characterization (0A, recorded 2026-07-13; facts only, no path persisted):** HEVC
  Main 10 (L153), 3840×2076 yuv420p10le @ 24000/1001 fps, bt2020nc/PQ (smpte2084), limited range.
  **Dolby Vision profile 8.1** (`dv_profile=8`, `bl_signal_compatibility_id=1`): single-layer
  BL+RPU, **no enhancement layer, HDR10-compatible base layer** — the exact profile Apple's
  sample-buffer/AVPlayer stack supports as `dvhe.08`. HDR10 static metadata present (mastering
  0.0001–1000 nits, MaxCLL 1000 / MaxFALL 400). Container ~39.9 Mbps overall, ~83 min.
  **Four audio tracks:** TrueHD+Atmos 7.1 48 kHz, FLAC stereo, AC-3 5.1 (640 kbps), AC-3 stereo
  (192 kbps) — plus 11 subtitle tracks. **Long GOP, B-heavy:** sampled head = 9.2 s average /
  15.0 s max keyframe spacing, 84% B-frames — which is why deep seek run-ups decoded 50–124
  frames, and why compressed-packet shedding must discard whole dependency runs.
- VideoToolbox **is** engaged on the MKV route (`hwaccel=VideoToolbox`). The residual path is
  VideoToolbox surface → CPU P010/NV12 transfer → swscale RGBA64 → CPU PQ/HLG-to-scRGB pack →
  RGBA16F upload.
- Before `d1760d0c`, discarded keyframe-to-target seek frames paid the full conversion cost:
  roughly 25 ms/frame; 56 frames took 1405 ms and 124 took 3108 ms. The committed split now
  converts only the landing frame and reduced measured run-up to roughly 3 ms/frame.
- Commit `4244a69f` contains short-forward decode seeks and the parallel CPU HDR conversion
  stopgap. It is no longer “in the working tree.” The per-frame thread fan-out is temporary and
  must be removed when Phase 2 lands.
- HDR conversion cost scales with output area: approximately 2036×1100 was smooth while
  approximately 2940×1588 stuttered in the observed run.
- The existing tests pass but do not cover a full-size 4K RGBA16F preroll feasibility failure.
  Verified constants: `VIDEO_QUEUE_BYTE_BUDGET` = 94.92 MiB (`video.rs:351`), `PREROLL_FRAMES` = 2
  (`video_session.rs:33`), a 4K fp16 frame = 63.28 MiB — the second frame is always rejected
  (63.3 + 63.3 > 94.9) and `preroll_satisfied` has no capacity clamp, so any frame size in
  (47.5 MiB, 94.9 MiB] deadlocks preroll. The one-frame exception in `admits` fires only when a
  single frame exceeds the whole budget, which 63.3 MiB does not.
- The FFmpeg video producer and audio decoder each own a separate `FfInput`/demuxer over the same
  path. On a network share this can read/demux the interleaved container twice; local page caching
  can hide the cost, so duplicated source bytes must be measured rather than assumed harmless.
  In fact playback start can run **up to four** independent `avformat_open_input` +
  `find_stream_info` passes over the same clip: video producer, audio decoder, poster, and probe.
- The FFmpeg interrupt-callback cancellation seam exists (`io.rs`), but the borrowed cancel flag
  is **null on both hot paths** — `Reader::open` and `FfAudioDecoder::open` pass `None`; only the
  poster path arms a real flag. A blocked producer/audio read today can be aborted only by the
  per-op watchdog (10 s read/seek, 20 s open). Harmless on local files; unacceptable over SMB.
- `PB_VIDEO_DIAG` today prints only an open banner and a per-seek timing line in
  `video_producer.rs`. `pb-app-core` (VideoSession poll/starvation/rebuffer/seek/audio) is
  entirely uninstrumented — Phase 0B is greenfield there, not an extension.
- pb-render's reusable NV12 texture slot + YUV shader exist (`gpu.rs`, `fs_scene_nv12`) but are
  fed **only by the Windows MF path**. The FFmpeg route CPU-converts to RGBA and uploads through
  the plain image slot, so Phase 2 is "make the FFmpeg producer ship planar frames end-to-end,"
  not merely "widen the texture slot to P010."

## 3. Root-cause inventory

| ID | Defect | User-visible effect | Primary seam |
|---|---|---|---|
| R1 | Two 4K RGBA16F frames (2 × 63.3 MiB) do not fit the fixed 94.9 MiB queue budget | Some HDR geometry modes can remain in `Buffering` forever | `video.rs`, `video_session.rs` |
| R2 | The first empty-queue poll enters `Buffering`; core immediately pauses audio | A decode spike becomes an audible stop/start | `VideoSession::poll`, `poll_video` |
| R3 | Playback presents at most one old due frame and never discards late frames | It cannot catch up cleanly once behind | `VideoSession::poll` |
| R4 | Each repeated seek immediately stops/seeks/refills the independent audio decoder | Crackle and long recovery during/after held seeks | `video_seek*`, `SeekVideoAudio` |
| R5 | Audio open/read/seek/deinterleave/refill is performed through a shared core handle on `@MainActor` | Heavy codecs and seeks contend with the UI/pump | `SessionAudioPlayer.swift`, `pb-mac-ffi` |
| R6 | HDR steady state creates CPU P010/NV12, RGBA64, and RGBA16F traffic, then uploads 8 B/px | CPU-bound large-window playback | `ffmpeg/hw.rs`, `ffmpeg/convert.rs` |
| R7 | macOS route selection is container-extension based; MKV always takes the full custom session even when its codec is Apple-decodable | Native decode/presentation capabilities are left unused | `VideoContainer::macos_native`, `start_video_session` |
| R8 | The stopgap spawns fresh scoped OS threads (`available_parallelism()−2`, clamped to `[1, rows]`) per converted frame | Burst scheduling jitter and unnecessary thread churn | `pack_scrgb_f16` |
| R9 | Video and audio independently open/demux the same path and have no coordinated compressed-packet read-ahead | SMB jitter or duplicate reads can drain the streams at different times; seeks multiply remote I/O | `ffmpeg/io.rs`, `video_producer.rs`, `audio_decoder.rs` |
| R10 | Audio track selection is a blind `best(Audio)` pick; no policy chooses among multiple tracks (the corpus carries TrueHD+Atmos 7.1 **and** FLAC stereo **and** AC-3 5.1/stereo) | The most expensive, least platform-friendly codec is decoded when cheaper Apple-decodable tracks sit unused | `audio_decoder.rs`, route probing |
| R11 | The HDR tone-map `peak` is a monotonic running max that never decays | One bright frame permanently raises the SDR white point for the rest of the session, dimming everything after it | `convert.rs` |
| R12 | A mid-stream audio decode/seek error silently drops the decoder (`session_audio = None`) and reads back as EOF | A corrupt audio tail is indistinguishable from a clean end of stream; playback "ends" audio without any error | `pb-mac-ffi/lib.rs`, `SessionAudioPlayer.swift` |

R6 is real, but it is not sufficient to explain R1–R5 or R9. GPU color conversion alone will not
make audio reliable, and decoded-frame buffering is the wrong way to hide network jitter.

Secondary fragilities confirmed by audit (fix inside the owning phase; each needs a test):

- **Upward frame-size renegotiation can overshoot the byte budget.** `Opened` overwrites
  `frame_bytes` after credits were granted at the old estimate and credits are never revoked, so
  `queued + credited` can transiently exceed `max_bytes`. Tests only cover downward correction.
  (Phase 1A.)
- **Credit-reset-on-seek is an unchecked cross-component invariant** — `seek_to` zeroes
  `credits_out` and *assumes* the producer zeroes its balance on `SeekTo`. (Phase 1A/1D tests.)
- **Replay after producer exit is fragile:** replay sends `SeekTo` assuming the producer parked
  after EOS; if the thread exited, the send is silently dropped and the disconnect path turns
  replay into a spurious failure toast. (Phase 1G.)
- **`audio_seek_ack` uses a hardcoded 1 s proximity window**, so a seek near the stale pre-seek
  position can ack against a not-yet-updated audio sample. (Phase 1D.)
- **Back-step before any presented frame uses a 30 fps default interval** — interval learning
  only updates from consecutive presented PTS, never from paused-seek landings. (Phase 1C.)
- **Terminal sessions keep `is_active` true** — `Ended`/`Failed` never drain `queued_bytes`, so
  the tick loop spins until the backend drops. (Phase 1G.)
- **`video_audio_open` consumes the stashed input on failure** — no retry without the shell
  re-issuing `StartVideoAudio`. (Phase 1E.)
- **`frames_to_units` mutates the clock origin as a side effect** of a unit-conversion helper —
  refactor hazard in the audio epoch logic. (Phase 1E.)
- **HDR/poster downscale is bilinear** (`ScaleFlags::BILINEAR`), below the Lanczos bar used
  elsewhere; revisit when the planar GPU path makes swscale quality moot. (Phase 2.)

## 4. Locked architectural rules

1. **Audio continuity wins over displaying every video frame.** When audio exists, it is the
   master clock. Repeat the last video frame during a short miss and drop obsolete video frames
   to recover. Pause audio only for an actual prolonged rebuffer or an explicit user action.
2. **Bound memory before decoding.** Queue admission must never require unbounded growth just to
   satisfy a fixed frame-count preroll.
3. **Route video and audio capabilities independently.** An MKV may contain Apple-decodable HEVC
   video and non-Apple-decodable TrueHD audio. That is a mixed route, not an all-or-nothing choice.
4. **One clock per active playback path.** The macOS sample-buffer route must put video and audio
   under one `AVSampleBufferRenderSynchronizer`. The Session route keeps one audio-master clock
   with delivery-delay-aware samples.
5. **No blocking media work on the main actor/event loop.** Demux, decode, seek run-up, conversion,
   PCM preparation, and queue refill run on owned workers/serial queues.
6. **All async work is identity-gated.** Commands and callbacks carry `session_id`; seek-related
   commands and landings carry `seek_generation`. A stale result is dropped without side effects.
7. **The fallback remains first-class.** Apple sample-buffer playback is an optimization and
   correctness win for supported codecs, not permission to leave VP8/exotic/software paths brittle.
8. **Do not claim Dolby Vision correctness until it is proven.** A static PQ shader can render an
   HDR10-compatible base layer but does not implement Dolby Vision dynamic metadata.
9. **Buffer compressed media, not decoded frames, for I/O jitter.** Network read-ahead is bounded
   jointly by bytes and media-time span and remains RAM-only. Decoded queues stay sized for
   scheduling/presentation, not seconds of network insurance.
10. **One demux seek per user intent.** Video and audio must not independently hammer a mounted
    share for every intermediate seek. A final generation resets both streams atomically.

## 5. Target architecture

### 5.1 Shared product model

Keep `ActiveVideoBackend` at two core-visible variants:

```text
Native  — macOS shell owns the media mechanics; core holds a passive proxy
Session — shared VideoSession owns policy; platform producer/sink supply media
```

Do **not** add a third core state machine. In the macOS shell, introduce a common native-presenter
facade implemented by:

```text
NativeVideoPresenter
  ├─ AVPlayerPresenter        (today's NativeVideoPlayer)
  └─ SampleBufferPresenter    (FFmpeg-demuxed, Apple-rendered media)
```

Both expose the same play/pause/seek/step/mute/transform/capture/stop lifecycle and report through
the existing session/generation-gated native callbacks. This confines platform divergence to the
shell and avoids duplicating core menu/slideshow/delete/progress behavior.

### 5.2 macOS capability route

Probe off-main, then select by actual stream capabilities—not just extension:

```text
1. AVFoundation-native container
   -> AVPlayerPresenter

2. Non-native container (MKV/WebM/...)
   + compressed video accepted by Apple sample-buffer renderer
   -> FFmpeg demux -> compressed video CMSampleBuffers -> SampleBufferPresenter

   Audio sub-route, independently:
   0. select the audio *track* first (see track-selection policy below)
   a. compressed audio renderer accepts the codec -> enqueue compressed samples
   b. otherwise FFmpeg decodes -> timestamped LPCM CMSampleBuffers

3. Sample-buffer setup/decode fails, or video codec unsupported by Apple
   -> hardened Session backend (FFmpeg decode + planar GPU present + platform audio sink)
```

Each attempt may occur at most once per session. The fallback graph records attempted backends so
an error cannot loop Native → SampleBuffer → Session → Native.

**Audio track-selection policy (all backends, R10).** Real remux containers carry several audio
tracks (the corpus has four). Selection must be an explicit, tested policy, not FFmpeg's
`best(Audio)`:

1. Honor the container's default/forced disposition and language where present.
2. Among otherwise-equal candidates, prefer a track the active audio route handles cheaply and
   natively (e.g. AC-3/AAC/FLAC over TrueHD for the FFmpeg-decode or compressed-enqueue routes)
   **only when** measurement shows the premium track is a reliability or cost problem on that
   route; never silently downgrade when the preferred track plays fine.
3. The chosen track and the reason are diagnostics-visible; switching tracks in-session is a
   non-goal for this overhaul, but the seam (one selected stream index, chosen at open) must not
   preclude it.

### 5.3 Windows and Linux

- **Windows:** retain Media Foundation acquisition and the existing Windows audio sink. Reuse the
  planar `NV12`/future `P010` renderer contract where MF can expose it.
- **Linux:** retain FFmpeg acquisition and pw-cat/direct PipeWire policy. Reuse planar GPU
  conversion where the backend provides NV12/P010; software formats keep the compatibility path.
- Platform acquisition may differ, but `VideoFrame` timing/color/plane contracts and
  `VideoSession` scheduling policy must remain shared and testable.

### 5.4 Shared FFmpeg input and compressed-packet plane

For FFmpeg-backed path inputs, introduce a `FfPacketSource`/demux coordinator seam that can feed
both the hardened Session decoders and the macOS sample-buffer presenter:

```text
seekable path / archive bytes
          │
    one FFmpeg demuxer
          │
   bounded packet fan-out
      ├─ video packets -> software/VideoToolbox decoder OR sample-buffer renderer
      └─ audio packets -> FFmpeg audio decoder OR compressed sample-buffer renderer
```

- Open and demux a path once per active attempt. Do not add independent read-ahead threads around
  the existing video and audio demuxers and thereby double remote I/O.
- Bound the combined packet plane by both bytes and PTS span. Derive a target from measured source
  bitrate and observed read throughput, clamp it to explicit minimum/maximum caps, and reserve
  enough audio horizon to avoid crackle during a short video slowdown.
- Use high/low watermarks. A full queue stops read-ahead; it does not grow. If video falls behind
  while audio must continue, shed video only at a safe decode boundary (for example, discard a
  complete dependency run and resume from the next keyframe), never by dropping arbitrary
  compressed packets.
- A seek bumps generation, interrupts the current read, clears both packet queues and decoder
  delay, performs one demux seek, then prerolls both streams from the new dependency point.
- Reuse probe results and cancel obsolete poster/probe reads when playback starts so startup does
  not create several competing passes over the network file.
- `VideoInput::Bytes` remains RAM-resident and seekable. Network-hosted ZIP/7z acquisition retains
  its existing eager-entry semantics; do not claim packet read-ahead makes a solid archive stream.
- System-owned AVPlayer/Media Foundation routes may retain their own I/O/buffering. They must still
  expose buffering/recovery state through the common lifecycle rather than adding a core clock.

## 6. Phase 0 — Baseline and blocking spikes

No production architecture is selected from intuition alone.

### 0A. Reproducible corpus characterization

> Stream/HDR/GOP/audio-track characterization recorded 2026-07-13 — see §2 evidence. Still open:
> the remux controls (below), display/EDR facts per test display, and rolling-window peak bitrate
> (fold into 0D).

Record, without persisting viewed paths:

- container; video codec/profile/level; bit depth; pixel format; transfer/primaries/matrix/range;
  HDR10 mastering/MaxCLL metadata; Dolby Vision profile/config/RPU presence; dimensions/fps/SAR;
  keyframe spacing and B-frame use;
- audio codec/profile/channel layout/rate/start offset; whether FFmpeg currently downmixes;
- display refresh/EDR headroom and Fit/Fill/Original output dimensions.

Run separate controls:

1. Original MKV through the Session route.
2. **Video-only** stream-copy remux through AVPlayer. Muxing gotcha (verified 2026-07-13):
   the target must be **MP4, not MOV** — FFmpeg's movenc writes the DoVi `dvvC` box only in
   MP4 mode, and it needs `-strict unofficial`; the working control is
   `ffmpeg -i in.mkv -map 0:v:0 -an -c copy -tag:v hvc1 -strict unofficial control.mp4`
   (verify with a binary grep for `dvvC`). A `.mov` output silently degrades the control to
   plain HDR10.
3. A second control with the corpus **AC-3 5.1 track copied alongside** (`-map 0:a:2 -c copy`)
   — AC-3 in MP4 is AVFoundation-supported, so this also previews the R10 track-selection win.
   Both controls were generated 2026-07-13 as `~/Downloads/pb-remux-control-{video-only,video-ac3}.mp4`.

The remux isolates whether Apple can play the encoded video stream smoothly. It does **not** prove
that PhotoBlaze can construct correct `CMSampleBuffer`s or preserve Dolby Vision metadata; that is
the next spike.

### 0B. Instrumentation that does not perturb the hot path

Extend `PB_VIDEO_DIAG` with batched counters/timers rather than per-frame `eprintln!`:

- demux/read, raw decode, hardware transfer, swscale, HDR pack, upload, draw/present;
- queue frames/bytes/PTS span, credits outstanding, late frames dropped, repeated frames,
  starvation duration, rebuffer count/duration;
- audio scheduled duration, completion-to-refill latency, underrun/device-reset count,
  pause/resume count and reason;
- seek request/generation/landing/audio-resume latency and superseded work;
- source bytes read, duplicate bytes/passes, blocked-read duration, rolling input throughput,
  packet-queue bytes/PTS span, low-water events, rebuffer cause, and reconnect attempts;
- p50/p95/p99 plus worst case, never only a mean.

Diagnostics are opt-in, path-redacted, and written only to stderr/tracing for the run. They must not
create a persistent viewing log.

### 0C. Apple sample-buffer proof gate

Build an isolated on-device spike before integrating a new presenter. It must prove:

- FFmpeg packet/extradata → valid `CMVideoFormatDescription` and compressed
  `CMSampleBuffer` for the corpus HEVC stream;
- correct PTS, DTS, duration, sync/dependency attachments, B-frame ordering, and EOS drain;
- the exact packet representation the renderer requires (length-prefixed versus Annex B), with
  explicit bitstream filtering where necessary;
- VPS/SPS/PPS and `hvcC`; Dolby Vision `dvcC`/`dvvC` plus RPU NAL preservation where applicable;
  HDR mastering/content-light metadata propagation;
- `AVSampleBufferVideoRenderer`/display-layer backpressure from a background serial queue;
- first-frame reveal, pause/resume, flush, seek-to-keyframe plus decode-forward, replay, and parked
  last frame;
- HDR/EDR and Dolby Vision visual correctness on the physical M2 display, not just “it decoded”;
- displayed-frame capture for Copy/OCR/Describe/Compare;
- OS-version API choice for the app's deployment target (renderer receiver APIs versus supported
  legacy enqueue APIs).

For audio, test the corpus TrueHD stream separately. If compressed enqueue is unsupported, prove
FFmpeg-decoded, timestamped LPCM under the same synchronizer. Record whether channels are preserved
or downmixed; do not represent PCM output as Atmos. Also test the corpus **AC-3 5.1 track** as the
compressed-enqueue candidate (AC-3 is a system-decodable codec, unlike TrueHD) — if it enqueues
cleanly, the R10 track-selection policy may make the whole TrueHD problem optional on this route.

**Gate:** Phase 3 proceeds only if the spike is at least as smooth as the AVPlayer remux control,
has one stable timebase, preserves required HDR metadata, and survives repeated flush/seek/stop.

### 0D. Network I/O characterization and packet-source spike

The original network symptom must be reproduced separately from CPU/HDR load. Test the same file
locally and over a mounted SMB share while controlling available throughput, latency, jitter, and
short outages. Record the source's average and rolling-window peak bitrate; “gigabit SMB” alone is
not a useful workload description.

Required tiers include:

- ample capacity (at least 2× the measured rolling demand), then progressively constrained
  capacity around 1.5×, 1.0×, and below the long-run media bitrate;
- added latency/jitter and deterministic 100 ms, 500 ms, and 2 s read stalls;
- startup, steady playback, held seeks, a far seek, pause/resume, stop during a blocked read, and
  reconnect after the share becomes available again;
- current independent video/audio demuxers versus a one-demux bounded packet-source prototype.

The prototype must demonstrate that one demuxer can feed video and audio without starvation,
unbounded packets, or arbitrary compressed-packet loss. Verify cancellation inside FFmpeg I/O on
the mounted filesystem. If the OS can hold a file read past the interrupt deadline, isolate it from
the UI and session lifecycle, cap abandoned/stuck workers, and prove repeated stop/open cannot leak
threads indefinitely.

**Gate:** choose the packet-buffer byte/time caps and default preroll from traces. The shared
packet-source design must reduce remote bytes or starvation versus the two-demux baseline without
regressing local-file startup/seek latency by more than 5%.

## 7. Phase 1 — Make the Session backend reliable

This phase is unconditional because the fallback never disappears.

### 1A–1D — DONE (landed on `main` 2026-07-13, test-first)

| Item | What landed | Commit |
|---|---|---|
| **1A** feasible preroll (R1) | byte budget → `3 × 4K-fp16` (~190 MiB) + `effective_preroll` clamp so one oversized frame starts instead of deadlocking; 4K-fp16 preroll test | `a2cc6aa8` |
| **1B** audio-continuous starvation (R2) | 300 ms window: a transient empty queue holds the displayed frame and keeps audio running; rebuffer + pause audio only past the threshold | `d81a6adc` |
| **1C** drop-late (R3) | present only the newest due frame per tick, drop older; per-frame interval learning + `dropped_frames` counter | `29c669ed` |
| **1D** seek coordinator (R4) | core-side over the existing 3 effects: pause once per run; landing stores `pending_audio_commit` (generation-safe via `SessionUpdate::seek_landed`); ONE `SeekVideoAudio` after `VIDEO_SEEK_AUDIO_SETTLE`=250 ms; resume flushes pending | `9fbee85c`, `0a40a8b4` |

Also landed this session (pre-1E foundation): seek run-up convert-skip `d1760d0c`; short-forward-decode + parallel-HDR-convert stopgap (R8) + `PB_VIDEO_DIAG` `4244a69f`; `frames_to_units` origin untangle `f6fafc3a`. Owner confirmed playback + seeking feel much smoother.

⚠ **Build gotcha:** `ActiveVideo` has a SECOND literal construction under `cfg(ffvideo/macos)` in `app_core_impl` (~6819) — `cargo test -p pb-app-core` alone misses it; always also build `--features ffvideo`.

**Still open from the 1D contract** (fold into 1E/1G): session-identity fields on the audio effects (ride 1E's owned-handle rework); the pause-forever fallback timeout if a final landing never arrives (1G watchdog).

### 1E. Remove audio decode/refill from `@MainActor` — CODE-COMPLETE (2026-07-13; audio pending owner-listen)

The session-video audio decoder no longer lives in an `Option<FfAudioDecoder>` on the
`@MainActor`-bound `AppCoreHandle` (where every open/read/seek/refill contended with the UI +
pump, R5). It is now an exclusively-owned decoder behind a raw `usize` pointer, opened and driven
on a dedicated serial feeder `DispatchQueue` — **off the main actor** — with only the AVAudioEngine
control + played-position clock left on the main actor.

What landed (test-first where testable; audio is untestable in a headless agent → **owner-listen**):

| Area | What | Where |
|---|---|---|
| **R10** track selection | `choose_audio` (pure, unit-tested) + `select_audio_stream` glue: forced > default > FFmpeg `best` > first; `PB_VIDEO_DIAG` prints the chosen stream + reason. Cost-based downgrade deferred (route-level, measurement-gated). | `pb-decode/src/ffmpeg/audio_decoder.rs` |
| **usize-pointer FFI** | `SessionAudioDecoder { inner, failed }` boxed → `Box::into_raw`; free fns `open_stashed_session_audio(session_id)->usize` + `session_audio_{rate,channels,read,state,seek,free}(ptr,…)`; a thread-safe global `AUDIO_STASH` replaces the `pending_audio_input`/`session_audio` handle fields. `StartVideoAudio` stashes into the global. | `pb-mac-ffi/src/lib.rs` |
| **R12** error≠EOF | `session_audio_state(ptr)` → 0 Ok / 1 Eof / **2 Failed**; a read/seek error latches `failed`, a null ptr reads Failed (never a clean EOF). | `pb-mac-ffi/src/lib.rs` |
| stash survives failed open | open **consumes on success, keeps on failure** (clones out of the stash) → the host can retry without the core re-issuing the effect (fixes the old consume-on-failure bug). | `pb-mac-ffi/src/lib.rs` |
| **Swift off-main** | `OwnedAudioDecoder` (`@unchecked Sendable`): serial feeder queue owns the ptr, frees exactly once in `deinit`; async `open`/`read`/`seek` hop results back to the main actor FIFO. `SessionAudioPlayer` rewritten: **async non-failable init** (clock reports `Opening` during the gap; failure → `Failed`), generation-gated reads across seeks, deferred resume/seek until open lands. `CoreModel.StartVideoAudio` drops the `== nil` check + `core:` arg. | `mac/.../SessionAudioPlayer.swift`, `CoreModel.swift` |

`frames_to_units` origin untangle was already done (`f6fafc3a`, pre-1E).

Tests: `choose_audio_honors_disposition_intent`, `select_audio_stream_picks_the_lone_track`
(pb-decode); `open_failure_keeps_the_stash_for_retry`, `open_success_consumes_stash_and_streams_to_eof`,
`state_maps_failure_apart_from_eof` (R12/null), `owned_decoder_seeks_and_continues` (pb-mac-ffi,
`--features ffvideo`). All green; `build-swift-host.sh` links the full app.

**Network seek fix (2026-07-13, owner-reported, in the 1E follow-up commit):** the audio decoder
sought by the AUDIO stream index (`avformat_seek_file(ctx, self.index, …)`). MKV Cues index the
VIDEO track, so that found no index entries and fell back to a byte-position linear scan —
**measured ~73 s over SMB** on the 16 GB 4K corpus (`net_seek_read_timing` harness, gated by
`PB_NET_TEST_MKV`) — which blew the 10 s op-deadline watchdog *mid-seek*, returned `Ok` with a
corrupted demuxer, and then failed every read → (with the fresh R12 latch) permanent audio death.
Fix: seek the **default stream** (`-1`, AV_TIME_BASE units) so the video Cues are used (**~20-40 ms**,
lands within 0.1 s of target); the forward-discard still lands the audio precisely. Video was
unaffected because its producer already seeks the video track. This is a seek-*strategy* fix,
orthogonal to R9/1F (the double-demuxer); 1F still wins by making the seek happen once.

**Still open (fold into 1G):** session-identity on the audio effects (the owned handle is
generation-gated Swift-side, but the effects themselves aren't session-tagged); the pause-forever
fallback timeout if a final seek landing never arrives; a *bounded-retry recovery* so a genuinely
slow seek/read (some other container/route) rebuffers instead of latching R12 permanently — no
longer urgent now the linear scan is gone, but the right 1G robustness. Chunk lookahead still
3×250 ms. **Owner status:** local playback + seeking confirmed smooth; network seek confirmed fixed
(no more permanent audio cut-out); residual post-seek network *stutter* while both demuxers re-read
is inherent until 1F. Possible A/V-sync improvement post-seek (owner to confirm — the precise
landing should help).

### 1F. Bounded compressed read-ahead and network recovery

Land the Phase 0D winner behind the packet-source seam; do not solve SMB playback by inflating the
decoded `VideoFrame` queue.

- One demux worker performs sequential read-ahead and feeds bounded audio/video packet queues.
  Consumers decode independently, but all packets carry `session_id` and `seek_generation`.
- Arm the real cancel flag on every `FfInput` this worker owns. Today the producer and audio
  paths pass a null cancel pointer and rely on the 10–20 s watchdog — that must not survive the
  packet-source migration.
- Size the target horizon from bitrate and measured throughput, then clamp it by a hard combined
  byte cap and a maximum PTS span. Keep explicit audio/video reservations and report both horizons.
- Start playback only when the required audio horizon and minimum video dependency/preroll are
  present, or when EOF proves the clip is shorter. Do not wait for a nominal target the file cannot
  satisfy.
- Distinguish decode starvation, packet starvation, and source-disconnected states. A video-only
  miss follows 1B and leaves audio running. If the audio packet/PCM horizon also drains, pause once,
  refill both streams, re-anchor once, and resume without a crackle or clock jump.
- Intermediate seeks update intent but do not touch network I/O. The final seek flushes packet and
  decoder state once; any in-flight old-generation read or packet is ignored.
- On a transient read failure, attempt a small bounded reconnect/backoff sequence. Reopen and seek
  from a safe dependency point near the last committed media time, validate that the path still
  identifies the same file (stable facts such as size plus container/stream signature), and never
  splice bytes from a replaced file.
- Exhausted retry produces one error. Stop/navigation cancels retry immediately, never joins a
  blocked network worker on the main actor, and never starts a retry for a stale session.
- All buffering is RAM-only. No disk cache, partial-download file, MRU, or path-bearing telemetry.

Tests cover queue high/low watermarks, audio reservation, VBR bursts, insufficient-throughput
rebuffer, clean resume, safe video dependency shedding, final-seek invalidation, reconnect success,
file replacement, retry exhaustion, stop during blocked read, and global memory invariants.

### 1G. Lifecycle and failure containment

- Session replacement, navigation, delete, window close, and quit must cancel video + audio work
  and reject stale callbacks by `session_id`.
- Sleep/wake, display switch, audio-device change, resize/fullscreen, and app activation must have
  explicit resume/reprime behavior.
- Corrupt/truncated input, decoder disconnect, audio-device loss, and sample-renderer failure
  produce one user-facing error after the fallback graph is exhausted—never repeated toasts or an
  unbounded retry loop.

**Phase 1 exit:** the corpus plays through ordinary decoder spikes and qualifying SMB jitter
without an audio interruption; full-size HDR never deadlocks; seek spam results in one final audio
commit and one remote seek; insufficient throughput produces a clean bounded rebuffer; all new
pure-state tests pass.

## 8. Phase 2 — Planar GPU color and scale path

This is the durable Session presentation path for Windows/Linux and the macOS fallback.

### 2A. Extend the frame contract

Add explicit planar formats rather than pretending every frame is one tight RGBA buffer:

- `NV12` (8-bit 4:2:0) and `P010` (10 valid high bits in 16-bit words);
- per-plane offset/length/row stride, coded size, visible crop, chroma subsampling/siting;
- display size/SAR and rotation metadata kept separate from pixel storage;
- CICP primaries/transfer/matrix/range plus mastering display and content-light metadata;
- checked byte calculations and even-dimension/crop validation.

Avoid CPU plane rotation and full-frame repacking. Apply rotation/SAR/crop in geometry/UV mapping.
If the uploader cannot consume source row stride, copy into reusable tight staging bands—not a new
full RGBA allocation.

### 2B. Acquire planar frames by platform

- macOS FFmpeg hardware path: transfer VideoToolbox surfaces as P010/NV12 for this rung.
- Windows MF path: request/retain NV12/P010 where the reader and HDR policy permit it.
- Linux: accept software/VAAPI-derived NV12/P010 when available; retain the software compatibility
  converter for unsupported formats.
- If a decoder outputs a different 10-bit layout, convert once to the planar contract or take the
  compatibility path; never reinterpret bytes silently.

### 2C. Reusable GPU resources and shader

- Extend the existing NV12 reusable slot to P010: `R16Unorm` Y + `Rg16Unorm` UV after proving
  filterability/support on wgpu 22 for Metal, DX12, and the supported Linux adapters. Note the
  NV12 slot is currently fed only by the Windows MF path — wiring the FFmpeg producer to emit
  planar frames (bypassing swscale/`pack_scrgb_f16` entirely) is the bulk of this phase, not the
  texture-slot widening.
- Normalize P010's high-aligned 10-bit samples correctly; use 10-bit limited/full-range constants,
  not the NV12 8-bit constants.
- Shader order: range expansion → YUV matrix → PQ/HLG EOTF (or SDR transfer) → source-primary to
  scRGB matrix → output scale. Apply each transform exactly once.
- Preserve the existing fp16 scene intermediate and SDR/HDR present pass.
- Reuse textures, bind groups, uniforms, and staging buffers across frames. No per-frame texture,
  thread, or full-size RGBA allocation.
- Remove the per-frame scoped-thread `pack_scrgb_f16` stopgap once the planar path passes gates.

### 2D. HDR policy

- HDR10/HLG: use stream/frame metadata; tone-map SDR output from mastering/MaxCLL when trustworthy,
  otherwise a stable documented default. Do not CPU-scan every frame for peak. This also retires
  R11 (the current running-max peak that never decays); if any adaptive peak survives, it must
  decay or reset on seek, and the corpus provides MaxCLL 1000 so metadata should win here.
- Dolby Vision: the planar fallback may render only a verified HDR10-compatible base layer unless
  dynamic-metadata processing is explicitly implemented. Surface this internally as a capability,
  not as “Dolby Vision correct.”
- HDR10+/other unsupported dynamic metadata follows the same honest base-layer policy.

### 2E. Correctness and performance gates

- CPU reference versus GPU output golden tests for NV12 and P010 across BT.601/709/2020,
  limited/full range, SDR/PQ/HLG, black/white/chroma ramps, odd visible crops, and rotation/SAR.
- Use a high-quality independent reference (FFmpeg zscale/libplacebo or captured AVFoundation
  output), not only the old CPU implementation, to avoid preserving an existing bug.
- Physical-display EDR validation for highlight scale and SDR-white behavior.
- Steady-state conversion/upload p99 must clear the content frame interval with measured margin;
  queue starvation and audio pauses after preroll must be zero on the hardware-supported corpus.

## 9. Phase 3 — macOS sample-buffer presenter

Proceed only after Phase 0C passes.

### 3A. Demux and sample construction

- Consume the shared packet-source seam from 5.4/1F over `VideoInput::Path` or shared archive bytes;
  do not open a second demuxer for fallback audio.
- Convert packet time base to `CMTime` without float round-trips. Carry PTS, DTS, duration, sync and
  dependency flags; preserve B-frame decode/presentation order.
- Build and retain format descriptions from codec private data. Apply the exact required bitstream
  normalization and reject unsupported mid-stream description changes cleanly.
- Bound compressed queues by bytes and timestamp span. Drive enqueue from renderer readiness; never
  preload the clip or busy-poll `isReadyForMoreMediaData`.

### 3B. One synchronized media timeline

- Attach video and audio renderers to one `AVSampleBufferRenderSynchronizer` before enqueue.
- Normalize different stream start offsets to one session timeline.
- Preroll both streams, reveal on the first displayable frame, and start the synchronizer once.
- Position/progress comes from this timebase. The Rust core receives passive native-state updates;
  it does not run a competing media clock.
- Define sustained-buffering, end-of-audio-versus-video, and silent/no-audio behavior explicitly.

### 3C. Audio capability split

- Probe compressed audio acceptance separately from video.
- If accepted, enqueue compressed audio with correct format description and timing.
- Otherwise decode with FFmpeg off-main and enqueue timestamped LPCM. Preserve channel layout where
  supported; apply a documented downmix otherwise.
- TrueHD/Atmos playback success means clean synchronized audio. Atmos-object preservation requires
  separate proof and must not be inferred from a filename or source codec.

### 3D. Seek, backpressure, and cancellation

- Pause the synchronizer, bump generation, stop requests, flush both renderers, seek FFmpeg to a
  suitable keyframe, re-feed decode dependencies, preroll current-generation audio/video, then
  re-anchor and resume according to pre-seek state.
- Superseding seeks cancel/replace the pending feed. Only the final generation may reveal/resume.
- Frame-step and paused seek update exactly one displayed frame without starting audio.
- Stop/navigation tears down request callbacks before releasing demuxer, renderers, or layer.

### 3E. Shell integration without a third product state machine

- `SampleBufferPresenter` conforms to the same native-presenter facade as `AVPlayerPresenter`.
- Reuse the proven video-layer host and preserve `MetalCanvasNSView` ownership of pointer, scroll,
  pinch, context menu, drag/drop, resize, and detach.
- Reuse transform placement for Fit/Fill/Original/zoom/pan/rotation and letterbox behavior.
- Keep the poster until `isReadyForDisplay`; park the true last frame at EOS until stop/navigation.
- Expose `displayedPixelBuffer()` for Copy/OCR/Describe/Compare with explicit HDR/P3-to-consumer
  conversion policy.
- Keep native playback controls, hover hit testing, cursor ownership, menu/toolbar state,
  slideshow suppression, delete, and teardown behavior backend-neutral.

### 3F. Routing and fallback

- Replace the current extension-only `macos_native_route` with the probed capability result.
- Cache only non-sensitive codec/container capability facts for the active session; do not persist
  viewed paths or media-derived data.
- Runtime sample-renderer failure may fall back once to Session, carrying the desired position when
  safe. Never show an error before the final allowed backend fails.

**Phase 3 exit:** supported HEVC/H.264 MKV matches the AVPlayer remux control for smoothness,
audio continuity, seek behavior, HDR/EDR output, last-frame parking, capture, and teardown.

## 10. Phase 4 — Optional zero-copy/native Metal escalation

Only pursue this if Phase 2 metrics show hardware-frame transfer/upload is still the limiting stage
for a codec the sample-buffer presenter cannot cover.

- Map IOSurface-backed `CVPixelBuffer` planes through `CVMetalTextureCache` and keep the pixel
  buffer/texture alive until GPU completion.
- Because current wgpu has no external-Metal-texture ingest, prefer a small isolated native Metal
  presenter over a broad renderer fork.
- It must conform to the same native-presenter facade and synchronized audio contract; zero-copy is
  not permission to reintroduce a second clock.

## 11. Sequencing

1. Phase 0A/0B: characterize the exact corpus and capture an honest baseline.
2. Phase 0C/0D: prove or reject the macOS sample-buffer architecture and shared packet-source
   architecture in isolated spikes.
3. Phase 1: land Session reliability and bounded network buffering unconditionally, in test-first
   slices.
4. If Phase 0C passes, integrate Phase 3 for Apple-decodable MKV while keeping Session fallback.
5. Land Phase 2 for Windows/Linux and unsupported macOS codecs; it may proceed alongside Phase 3
   only when file ownership does not overlap.
6. Remove the parallel CPU conversion stopgap after Phase 2 is proven.
7. Consider Phase 4 only from residual measured bottlenecks.

Recommended Phase 1 commit slices:

1. Preroll-capacity test + bounded fix.
2. Starvation timer/state + audio-continuity tests.
3. Latest-due frame selection/drop accounting.
4. Generation-aware seek coordinator and final-on-release contracts.
5. Owned off-main audio decoder/feeder.
6. Shared packet source + bounded read-ahead, with the old two-demux path retained only for A/B.
7. Network recovery, lifecycle/device/sleep stress fixes, and diagnostics.

## 12. Acceptance gates

Use the AVPlayer remux control as the system baseline where applicable. Phase 3 should be within
20% of its startup/seek latency and should not use materially more CPU during steady playback.

### Steady playback

- Ten-minute corpus run after startup: **zero video-induced audio pauses, underruns, or rebuffers**.
- No persistent A/V drift. Target p95 absolute A/V error <= 40 ms and worst case <= 80 ms after
  startup/seek settling, unless the AVPlayer control itself exceeds that on the same device.
- No deadlock in Fit/Fill/Original, HDR/SDR display mode, or muted playback.
- Dropped/repeated video frames are measured. For hardware-supported 4K30, target <0.5% steady-state
  drops; 4K60 is compared against AVPlayer on the same display/refresh.
- No per-frame OS-thread creation or full RGBA allocation on the planar/native hot paths.

### Seeking and controls

- Record p50/p95/p99 request-to-displayed-frame and final-intent-to-clean-audio-resume.
- Held seek and scrub spam produce no stale flash, no obsolete audio restart, and exactly one final
  audio commit.
- Paused seek stays paused; frame-step presents one frame; replay starts from zero; EOS parks the
  final frame.

### Mounted-network playback

- On the Phase 0D qualifying profile—transport capacity at least 1.5× the measured rolling source
  demand with deterministic stalls up to the proven packet horizon—there are zero post-preroll
  audio underruns/rebuffers and no stale video flash. Final thresholds are recorded with the corpus
  rather than reduced to the label “gigabit.”
- At or below the long-run source bitrate, behavior degrades honestly: one clean pause when the
  audio horizon empties, then remain buffering until enough recovery margin exists (or fail after
  the bounded retry policy). Re-anchor once on a sustainable resume. There is no crackle,
  premature pause/resume loop, unbounded memory growth, or silent A/V drift.
- A final seek performs one remote demux seek. Seek spam, navigation, and stop do not produce
  duplicate old-generation network work.
- Local-file startup and seek p95 regress by no more than 5% against the pre-packet-source baseline.

### Robustness

- Navigate/stop/quit during open, preroll, decode, audio refill, and seek.
- Corrupt/truncated files; missing timestamps; VFR; B-frames; long GOP; no audio; audio-only error;
  stream start offsets; odd crop/SAR/rotation; 8K oversized input.
- Resize/fullscreen/scale changes, display switch, sleep/wake, audio-device change, mute, and archive
  bytes playback.
- SMB jitter/stalls, share disconnect/reconnect, file replacement during reconnect, insufficient
  sustained throughput, and stop/navigation while a read is blocked.
- Every failure is bounded, cancellable, session-gated, and produces at most one final user error.

### Privacy and distribution

- Existing viewing-no-write tests remain green for filesystem, ZIP, and 7z video sessions.
- Diagnostics remain opt-in and path-redacted.
- macOS bundled FFmpeg closure, signing, notarization, and clean-machine launch remain green.

## 13. Test matrix and file map

### Automated tests

- `pb-app-core`: property/state tests for queue capacity, starvation threshold, latest-due
  selection, pause/rebuffer reasons, seek generation/coalescing, packet/source states, stale
  callback rejection, EOS.
- `pb-decode`: PTS/DTS conversion, packet dependency flags, bitstream normalization, P010/NV12
  plane/stride/crop validation, packet queue byte/time bounds, stream fairness, safe dependency
  shedding, one-demux seek, reconnect identity, seek supersession, corrupt input/cancellation.
- `pb-render`: independent-reference golden tests for NV12/P010 color and geometry; reusable resource
  tests; staging stride/alignment tests.
- `pb-mac-ffi`: owned audio-handle lifecycle, double-drop/use-after-stop prevention, callback
  identity, capability result round trips.
- Swift tests: presenter facade parity, sample-buffer backpressure, fallback graph, seek/stop races,
  audio capability split, last-frame/capture lifecycle.

### Expected implementation seams

- `crates/pb-app-core/src/video.rs`
- `crates/pb-app-core/src/video_session.rs`
- `crates/pb-app-core/src/app_core_impl.rs`
- `crates/pb-app-core/src/contract.rs`
- `crates/pb-decode/src/ffmpeg/{io,probe,packet_source,video_producer,audio_decoder,convert,hw}.rs`
- `crates/pb-render/src/{gpu,upload,yuv}.rs`
- `crates/pb-mac-ffi/src/lib.rs`
- `mac/Sources/PhotoBlazeMac/{CoreModel,NativeVideoPlayer,SessionAudioPlayer,MetalCanvas}.swift`
- New macOS files should isolate the presenter facade, sample-buffer presenter, and owned audio
  decoder wrapper rather than growing `CoreModel.swift` further.

## 14. Open decisions that must be resolved by evidence

- ~~Exact Dolby Vision profile/config and whether the corpus contains a usable HDR10 base layer.~~
  **Resolved 2026-07-13 (0A):** DoVi profile 8.1, BL+RPU, no EL, HDR10-compatible base layer with
  full HDR10 static metadata — the Apple-supported `dvhe.08` profile.
- Whether the supported macOS versions' sample-buffer renderer preserves and presents that Dolby
  Vision profile correctly from reconstructed format descriptions and packets.
- Whether compressed TrueHD is accepted (do not expect it); required LPCM channel layout/downmix.
- Whether compressed AC-3 enqueue works and whether the R10 track-selection policy should prefer
  the AC-3 5.1 track over TrueHD on the sample-buffer and Session routes (measure TrueHD decode
  cost off-main first; do not downgrade quality without evidence of a problem).
- The Session short-starvation threshold and audio chunk/lookahead sizes.
- The shared packet-source byte/time caps, reconnect count/backoff, and qualifying SMB profile from
  Phase 0D; do not hard-code these from a single “gigabit” run.
- The bounded 4K/8K queue byte caps after P010 measurements.
- Whether P010 `R16Unorm`/`Rg16Unorm` sampling meets all supported wgpu backend requirements.
- Whether Phase 2 readback/upload already clears the target; if yes, do not build Phase 4.

## 15. Definition of done

The overhaul is complete when the corpus and representative SDR/HDR/VFR/no-audio/unsupported-codec
files meet the acceptance gates; the fallback remains bounded and cancellable; qualifying mounted-
network playback is smooth and insufficient bandwidth recovers cleanly; macOS routing is capability-
based without retry loops; native and Session presenters preserve all existing controls and
lifecycle behavior; automated tests cover the previously missing 4K HDR/preroll/audio-seek/network
cases; the changelog documents user-visible playback improvements; and the task/architecture docs
describe the shipped routes rather than interim experiments.
