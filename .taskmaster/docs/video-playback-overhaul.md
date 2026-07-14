# Video Playback Overhaul — Plan

> Status: **draft / in progress** · Owner: JD · Started 2026-07-13 (0.2.0 beta feedback)
>
> Goal: a **stable, robust, efficient** foundation for playing back video — especially
> the codecs/containers AVPlayer can't open (MKV/WebM/…) and demanding content
> (4K, 10-bit, HDR10/HLG/**Dolby Vision**, high-bitrate lossless audio like TrueHD/Atmos) —
> as well as the system handles the easy cases.

---

## 1. Context

PhotoBlaze has **two** video routes on macOS:

| Route | Used for | Decode | HDR/DoVi | Clock | State |
|---|---|---|---|---|---|
| **AVPlayer** (`video_native.rs`, `NativeVideoPlayer.swift`) | MP4/MOV that AVFoundation can demux | system (VideoToolbox) | system, correct | system | solid |
| **FFmpeg session** (`pb-decode/ffmpeg/*`, `video_session.rs`, `SessionAudioPlayer.swift`) | MKV/WebM/VP8/VP9/AV1 + level-2 fallback | FFmpeg (+ VideoToolbox hwaccel readback) | **CPU** PQ→scRGB | **our** manual A/V bridge | fragile |

The FFmpeg session route is also **the only** video route on Windows (MF) and Linux
(the whole path is shared; only the sink/present differs). So its correctness matters
cross-platform, not just for macOS MKV.

The beta problems are all on the **FFmpeg session route** with a 4K DoVi HDR MKV
(`Ghost.in.the.Shell…2160p…DoVi.HDR.x265…TrueHD.Atmos.7.1.mkv`).

### What we measured this session (facts, not guesses)

- **Hardware decode *is* engaging** on the MKV path: `hwaccel=VideoToolbox`, output `Rgba16F`
  (diag added behind `PB_VIDEO_DIAG=1`). The earlier "HDR forces software" belief was the
  **Windows** path leaking into a global claim in `CLAUDE.md`; there is **no** macOS HDR→software gate.
- **Seeking was decode-conversion-bound**, not I/O: `demux ≈ 0–3 ms`, but the keyframe→target
  run-up ran the full readback + downscale + PQ→scRGB tone-map on every discarded frame
  (~25 ms/frame). A 56-frame seek = 1405 ms; 124 frames = 3108 ms.
- **HDR color conversion is CPU-bound and scales with output area** (`convert.rs::pack_scrgb_f16`,
  a per-pixel LUT + 3×3 matrix + f16 pack). Smooth at ~2036×1100; stutters at ~2940×1588.
- **Post-seek audio glitches** on TrueHD/Atmos (a heavy, lossless codec decoded on `@MainActor`).

### Already landed / in flight (this session)

- **Committed:** poster frame selection (seek-deep, shallow-first) · info-line default center ·
  build-script `--ffvideo` default · **seek run-up: skip convert on discarded frames**
  (`d1760d0c`, ~25 → ~3 ms/frame, 7–10× faster seeks).
- **In the working tree, not yet committed:** seek **forward-decode** for short forward hops
  (arrow ±2 s decode forward instead of re-seeking to a keyframe) · **parallelized `pack_scrgb_f16`**
  across cores (a *stopgap* — see Phase 2) · `PB_VIDEO_DIAG` diagnostics.
- **Reverted:** the `#5` aspect-toggle "restart at native fit" experiment (froze on 4K; see
  Phase 3 — the Apple layer gives scale/zoom as a free transform instead).

---

## 2. Root-cause inventory (verified against the code)

| # | Defect | Symptom it causes | Location | swift-bridge blocked? |
|---|---|---|---|---|
| R1 | 2-frame **preroll** can't fit two 4K **RGBA16F** frames (63.3 MiB each) in the 94.9 MiB budget (`3 × 3840×2160×**4**`, RGBA8-sized) | Full-native (Original/Fill) HDR can stay in **Buffering forever** | `video.rs:342,351`; `video_session.rs` preroll | no |
| R2 | Session enters **Buffering** on the *first* empty-queue poll and **pauses audio**; resumes on 2 frames | Every decode/convert spike → **audible pause/resume** (your post-seek crackle) | `video_session.rs` poll; `app_core_impl.rs:7108` | no |
| R3 | **Audio seek is not coalesced**: each held/repeated ±2 s seek stops the node, seeks the 2nd FFmpeg decoder, refills ~750 ms PCM, restarts — **on `@MainActor`**, per intermediate target | **Persistent** post-seek audio glitch | `app_core_impl.rs:6905`; `SessionAudioPlayer.swift:104,174` | no (coalescing); partial (off-main) |
| R4 | **Never drops late frames** ("rebuffer-don't-drift") — can't catch up once behind | Slow recovery after any stall | `video_session.rs` queue policy | no |
| R5 | HDR path = VideoToolbox → **CPU readback → swscale RGBA64 → per-pixel scRGB → 63 MiB upload** | Steady-state CPU cost; large-window stutter | `hw.rs:191`; `convert.rs:150,228` | no (new P010 shader) |
| R6 | FFmpeg **audio decode/deinterleave/refill on `@MainActor`** | Network pinwheel; heavy-codec crackle; blocks the pump | `SessionAudioPlayer.swift`; `pb-mac-ffi` `video_audio_*` | **yes** — needs owned-decoder FFI |

Note R6's blocker: the audio decoder lives inside the shared `&mut self` FFI handle, so running
it off-main needs it **extracted into an owned object** the Swift feeder owns exclusively.
swift-bridge 0.1.59 rejected the clean owned-opaque return; the viable shape is a **raw-pointer
(usize) handle** (the same trick `attach_layer(layer_ptr: usize)` already uses).

---

## 3. Target architecture

**Principle: lean on the platform's media stack wherever it can decode the content; keep a
hardened FFmpeg+wgpu path for everything it can't.**

### macOS routing (end state)

```
still / Live Photo / animation ─────────────────────────► wgpu (unchanged)

video:
  ├─ AVFoundation can demux (MP4/MOV) ──────────────────► AVPlayer            (today)
  ├─ Apple can DECODE but not demux (HEVC/H.264/DoVi/… in MKV/WebM)
  │      FFmpeg DEMUX → CMSampleBuffers → AVSampleBuffer* stack  ◄── Phase 3 (new)
  │        AVSampleBufferDisplayLayer + AVSampleBufferAudioRenderer
  │        + AVSampleBufferRenderSynchronizer (one timebase)
  └─ Apple can't decode (VP8, exotic) ──────────────────► FFmpeg + wgpu       (hardened: Phase 1[+2])
```

### Windows / Linux

```
all video ──► FFmpeg + wgpu (present via MF / pw-cat)  (hardened: Phase 1 + Phase 2 GPU shader)
```

### Why the Apple `AVSampleBuffer` stack for the MKV-HEVC/DoVi case

- System **hardware decode**; **correct Dolby Vision** *dynamic* metadata (a static PQ→scRGB
  shader can't do DoVi properly — Apple explicitly recommends its pipeline for DoVi).
- **One authoritative A/V clock** (the synchronizer) — deletes our manual clock bridge (R2/R3/R6
  all partly evaporate).
- Native frame **scheduling + dropping** and **bounded backpressure** — deletes our credit loop,
  preroll budget, and rebuffer policy (R1/R4).
- Layer **transform** gives fit/fill/zoom/pan for free (the thing `#5` tried to hand-roll).

### Drawbacks (why it's an addition, not a takeover)

1. **macOS-only.** Windows/Linux keep the FFmpeg+wgpu path — so Phase 2 (GPU shader) is still needed.
2. **Apple-decodable codecs only.** VP8/exotic still fall to FFmpeg+wgpu → that path can't be retired.
3. **Presentation is a CALayer**, composited by the window server, not drawn in the wgpu surface.
   Chrome (info line/scrub/HUD) overlays as a layer (as the AVPlayer path already does). Any effect
   that needs the *decoded video pixels inside the wgpu pipeline* (unified photo/video color grading,
   say) wouldn't apply to these clips. Acceptable for a viewer.
4. **CMSampleBuffer construction** from FFmpeg packets (parameter sets, `CMVideoFormatDescription`,
   DoVi config record, timing) is fiddly, and **seek = flush + re-feed** from the demux point.
5. A **third** macOS video path to maintain (AVPlayer, AVSampleBuffer, FFmpeg+wgpu-fallback). The
   routing table above keeps it comprehensible; a spike must prove the maintenance cost is worth it.

---

## 4. Phased plan

### Phase 0 — Isolate + instrument (cheap, first)

- **A/B remux test:** `ffmpeg -i film.mkv -c copy film.mov` → open the `.mov` (AVPlayer route).
  If it's flawless, it **proves the streams + the M2 decoder are fine** and the entire problem is
  our FFmpeg-fallback orchestration — and that the Phase 3 Apple-stack path will work (same system
  pipeline, fed differently). If it *isn't* smooth, re-scope.
- **Instrumentation** (extend `PB_VIDEO_DIAG`): per-stage timing (raw decode / hw transfer / swscale
  / scRGB pack / upload), queue depth, video underruns, **audio pause/resume count**, audio scheduled
  duration, seek generations. Numbers drive every later decision.

### Phase 1 — Reliability (hardens the FFmpeg+wgpu path; fixes the audible symptoms *now*)

All in `pb-app-core` / Swift; **none blocked** by swift-bridge; each unit-testable.

- **R1 — preroll budget.** Guarantee the queue admits at least `PREROLL_FRAMES × frame_bytes`
  (+1 in-flight), using the **format-aware** `frame_bytes` (fp16 = 8 B/px). Add a **4K RGBA16F
  regression test** (the case no current test exercises).
- **R2 — audio-continuous through short starvation.** Don't pause audio on the first empty poll:
  **hold/repeat the last video frame**, and only rebuffer (and pause audio) after **sustained**
  starvation (~250–500 ms). Audio is the master clock; keep it running.
- **R4 — drop-late-to-recover.** When several frames are already due, present only the **newest due**
  frame and drop the rest, so playback catches up to audio instead of drifting.
- **R3 — coalesce audio seeks.** During held-key/scrubber seeking, **pause audio once**, coalesce all
  intermediate intents, and seek+refill audio **only after the final current-generation video
  landing** (mirror the video producer's supersede logic). Removes the per-target main-actor churn.

Exit criteria: your MKV plays with clean audio through seeks, and full-native HDR no longer deadlocks.

### Phase 2 — GPU planar (P010/NV12) shader path (cross-platform perf; retires the CPU convert + the stopgap)

Behind the existing `PixelFormat` seam (`convert.rs` / `pb-render/gpu.rs`):

- Keep the transferred VideoToolbox frame as **P010** (10-bit HDR) / **NV12** (SDR); stop producing
  RGBA64 + RGBA16F on the CPU.
- Upload **P010 → `R16Unorm` luma + `Rg16Unorm` chroma** (NV12 → `R8`/`Rg8`). (`gpu.rs` already has an
  8-bit NV12 planar path to extend.)
- In the fragment shader: limited/full-range expand → YUV matrix → **PQ/HLG EOTF** →
  **BT.2020→scRGB** primaries → scale. (New shader work — the current HDR shader consumes *already*
  scene-linear fp16.)
- Peak for the SDR tone-map from **mastering/MaxCLL metadata** (or a stable default), **not** a
  per-frame CPU scan.
- Keep the RGBA converter as the **compatibility fallback** only.

Payoff: queued 4K frame **63.3 → 23.7 MiB** (≈4 fit the budget), **no per-frame CPU color loop**,
and the per-frame-thread-spawn parallelization stopgap can be **removed**. Needed for Windows/Linux
and the macOS Apple-can't-decode fallback.

### Phase 3 — macOS Apple-stack path for Apple-decodable MKV (the end state)

- FFmpeg **demux only** (no decode) → build `CMSampleBuffer`s (with `CMVideoFormatDescription`,
  incl. HDR/DoVi config) → enqueue into `AVSampleBufferDisplayLayer` +
  `AVSampleBufferAudioRenderer`, coordinated by one `AVSampleBufferRenderSynchronizer`.
- Route selection: Apple-decodable video codec in a non-AVFoundation container → this path; else the
  Phase-1/2 FFmpeg+wgpu fallback.
- Seek = flush both renderers + re-feed from the demux seek point against the synchronizer timeline.
- Scale/zoom/pan via the layer transform (retire the `#5` approach for these clips).

### Phase 4 — optional escalations (measure first)

- **Zero-copy** on Apple: `CVPixelBuffer → CVMetalTextureCache` (IOSurface-backed) to drop the
  readback for the Apple-can't-demux-but-can-decode-and-we-still-use-wgpu case — only if Phase 2's
  readback proves to be the remaining bottleneck. (wgpu can't import an external Metal texture, so
  this likely wants a small native Metal presenter — overlaps with Phase 3's machinery.)

---

## 5. Sequencing & dependencies

1. **Phase 0** (an afternoon): the remux test + instrumentation. Decides confidence in Phase 3.
2. **Phase 1** next, unconditionally — it fixes the shipping symptom and is needed under *every*
   strategy (it hardens the fallback that never goes away).
3. Then **Phase 3** for the macOS common case (HEVC/DoVi MKV) — the strategic win — **and/or**
   **Phase 2** for Windows/Linux + the macOS fallback. These are parallelizable; Phase 2 is not a
   prerequisite for Phase 3.
4. **R6 (audio off `@MainActor`)** — the raw-pointer-FFI feeder — is **subsumed by Phase 3** on the
   Apple path (the synchronizer owns audio). Still do it for the **FFmpeg+wgpu fallback** (Win/Linux
   + Apple-can't-decode), where our `SessionAudioPlayer` remains.

---

## 6. Testing strategy

- **pb-core / pb-app-core** (pure): preroll-budget property test incl. 4K RGBA16F; underrun→hold vs
  rebuffer thresholds; drop-late selection; seek-coalescing generation ordering. `cargo test`.
- **pb-decode**: producer seek landing (exists); planar-shader golden pixels vs the CPU converter
  for a P010 sample (Phase 2).
- **On-device (owner loop)**: the corpus clip + the remux A/B; `PB_VIDEO_DIAG` numbers before/after
  each phase (audio pause/resume count is the key regression metric for R2/R3).
- **No-trace**: unchanged — viewing still writes nothing; demux/decemux are read-only.

---

## 7. Open questions / risks

- **DoVi profiles**: confirm which profile the corpus clip is (8.1 vs 5 vs 7) — affects whether the
  Phase-2 static-PQ approximation is even acceptable as a fallback, and how the CMFormatDescription
  must be built for Phase 3.
- **CMSampleBuffer from FFmpeg packets**: HEVC parameter-set extraction + DoVi config record is the
  fiddliest part of Phase 3; spike it early.
- **Chrome-over-layer** compositing for the AVSampleBuffer path (info line, scrub bar): reuse the
  AVPlayer path's layer-ordering approach.
- **Three macOS video paths**: keep the routing table (§3) authoritative and documented so the
  maintenance surface stays legible.
```
