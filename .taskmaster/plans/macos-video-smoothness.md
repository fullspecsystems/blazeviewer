# macOS video smoothness — route MKV to the Session (wgpu) path, retire the sample-buffer route

> Status: **PLAN — diagnosis COMPLETE + owner-confirmed; execution NOT started** · Owner: JD
> Scope: **macOS video playback smoothness.** The winit (Windows/Linux) shell already uses the
> Session route and is unaffected. This is a routing + parity change, not a rendering rewrite.
>
> **One-line summary:** `AVSampleBufferDisplayLayer` (the macOS "sample-buffer" route we use for
> MKV/WebM) drops ~3 frames/sec on steady-state playback that every other route plays flawlessly.
> We already have a smooth, mpv-class renderer — the **Session route** (FFmpeg → wgpu → Metal).
> Make it the default for MKV/WebM; keep the sample-buffer route parked for a possible future
> Dolby-Vision path we've decided **not** to build.

---

## "You're probably wondering how I got into this situation"

The owner reported: *"Playback isn't very smooth… it stutters a little here and there,"* on a 1080p
BluRay **H.264 MKV** (`Ad.Astra.2019.…x264-Geek.mkv`, 23.976 fps) on a **120 Hz** display, on an
M2 Max — hardware that should laugh at 1080p H.264. We chased it with the same measure-first
discipline as the seek work, and it took **eight** ruled-out hypotheses to corner it. Recorded
here in full so no one re-runs them.

**Instrumentation built (all behind `PB_TRACE`, in `DemuxReader`/`SampleBufferPresenter`/`FramePump`):**
- `sb-read diag` — per-2s `demux_read_packet` latency window (feed rate, avg/max, slow-read counts).
- `sb-play diag` — VideoToolbox's own metrics via `displayLayer.sampleBufferRenderer.videoPerformanceMetrics`
  (macOS 14.4+): `numberOfDroppedFrames`, `totalNumberOfFrames`,
  `numberOfFramesDisplayedUsingOptimizedCompositing` (the hardware-overlay-plane count),
  `totalAccumulatedFrameDelay`. **This is the objective metric that drove the whole diagnosis.**
- `pump diag` — main-thread `pump()` tick rate + busy %.
- reveal log — content fps vs display Hz.

**Ruled out, in order, each by an A/B or a measurement (do NOT re-litigate these):**

| # | Hypothesis | How it died |
|---|---|---|
| 1 | **SMB / network I/O** | Copied the file to local SSD. Reads dropped from ~0.5ms to **0.0ms** — and it dropped frames **identically**. (SMB read *spikes* to 273ms *do* cause extra bursts, but they're not the steady-state cause.) |
| 2 | **Decode** | 1080p H.264 hardware decode is trivial; `delay 0.0ms` throughout — frames aren't arriving late. |
| 3 | **Main-thread pump at 120 Hz** | `work_pending()` kept the `CADisplayLink` pump spinning `pump()` at refresh rate during OS-presented playback. **Fixed** it (`needs_frame_pacing()`, commit `216c591d`) — pump idled to ~4/s — **drops unchanged.** (Fix kept: it's a real efficiency win.) |
| 4 | **SwiftUI compositing churn** | `PB_NO_PROGRESS` (stop the ~20 Hz scrubber update) → **no change.** |
| 5 | **Overlay-plane blocked by `masksToBounds`** | `PB_NO_MASK` (drop the clipping ancestor) → optimized-composite ratio **didn't move**, drops unchanged. |
| 6 | **Audio clock beating vsync** | `PB_NO_SB_AUDIO` (detach the audio renderer from the synchronizer) → **no change** (local + audio-off was, if anything, *worse*). |
| 7 | **Millisecond-jitter PTS** (MKV `1/1000` timebase → 41/42ms deltas) | `PB_RETIME` (snap PTS/DTS to the exact frame-rate grid at timescale 240000) → **zero change.** Drops are *completely independent of the timestamps we send.* (A subagent's math had predicted this: ±0.06 vsync jitter is ~16× too small to cause a 12% drop rate.) |
| 8 | **Layer embedding / `CAMetalLayer` nesting / EDR window** | See the decisive test below — killed by AVPlayer being smooth in the *same* embedding. |

**The two decisive tests (these are the whole ballgame):**

1. **Remuxed the MKV → video-only MP4** (`ffmpeg -map 0:v -c:v copy`, at `~/Downloads/adastra_avplayer_test.mp4`)
   so it plays through the **AVPlayer route** (`AVPlayerLayer`). **Flawless.** Same H.264 stream, same
   window, **same layer embedding** (the AVPlayer layer uses the identical container with `masksToBounds`
   nested under the `CAMetalLayer`). → the bug is **NOT** the embedding/window/EDR/display/content. It is
   **specific to the sample-buffer route.**
2. **`PB_NO_SAMPLE_BUFFER=1`** forces the MKV to the **Session route** (FFmpeg → wgpu → Metal, vsync-locked
   present — *mpv's architecture*). **Perfectly smooth, no visible drops.**

**Verdict.** Three routes, one culprit:

| Route | Same H.264 MKV | Result |
|---|---|---|
| AVPlayer (`AVPlayerLayer`, MP4/MOV) | ✅ | smooth |
| **Session (FFmpeg → wgpu → Metal)** | ✅ | **smooth** |
| Sample-buffer (`AVSampleBufferDisplayLayer` + `AVSampleBufferRenderSynchronizer`) | ✅ | **~3 drops/sec** |

The exact defect inside `AVSampleBufferDisplayLayer`'s manual-feed presentation is **unknown and we
deliberately did not chase it further** — AVPlayer's and our own wgpu route both present the same frames
smoothly, so the fix is to **route around it**, not to keep reverse-engineering Apple's black box. (For
the record, `numberOfFramesDisplayedUsingOptimizedCompositing` sat at ~20/24 and oscillated — the layer
bounces between the hardware overlay plane and GPU compositing, and the dropped frames come from that
churn. AVPlayer keeps its layer promoted; we couldn't make ours stay. That's the closest thing to a
mechanism we have, and it doesn't change the plan.)

---

## Why the sample-buffer route existed, and why retiring it is fine

It was chosen (task #91 Phase 3) for **Dolby Vision** — `AVSampleBufferDisplayLayer` + Apple's system
decoder gets DoVi "for free," which the wgpu shader can't. That is the route's **only** real advantage;
the Session route already renders **SDR and HDR10** correctly (the fp16 scRGB path, `pb-render`).

We investigated whether we could keep DoVi *and* smoothness. **mpv does DoVi entirely in software+shader**
(confirmed in `~/code/mpv`: FFmpeg extracts `AV_FRAME_DATA_DOVI_METADATA` → **libplacebo**
`pl_map_avdovi_metadata` / `pl_hdr_metadata_from_dovi_rpu` applies the RPU reshaping in the GPU shader;
dual-layer Profile 7 via `filters/f_enhancement_pair.c`). So DoVi does **not** require Apple's decoder.

**But we decided NOT to build it, for concrete reasons:**
- **libplacebo doesn't fit wgpu.** Its GPU backends are Vulkan/OpenGL/D3D11 — no Metal, no wgpu. On macOS
  it runs over **MoltenVK**, i.e. a *parallel GPU stack* beside our wgpu renderer, **+~8–12 MB** download
  on macOS (libplacebo ~1–3 MB + MoltenVK + loader). Licensing is fine (LGPL-2.1+, like FFmpeg/libheif),
  but the size + a second GPU stack are not.
- **Rolling our own DoVi-in-WGSL** (FFmpeg gives us the parsed `AVDOVIMetadata`; apply the reshaping
  polynomials/MMR + dynamic trims in our existing HDR shader) is ~0 MB and architecturally clean — but the
  **verification is the wall.** DoVi correctness is a *perceptual judgment on a calibrated HDR display*
  with no objective metric an agent can read; it needs many rounds of eyes-on-glass against a reference
  (mpv / a DoVi TV). The math (Phase 1–2) is unit-testable against libplacebo/libdovi reference vectors;
  the end-to-end color correctness (Phase 3) is not automatable. Weeks of human-in-the-loop work per
  profile (8.1 tractable, 5 harder, 7 a major feature).
- **The owner's hardware settles it.** The **Samsung QN90B TV does not do Dolby Vision at all** (Samsung
  backs HDR10+); it already shows DoVi content as its **HDR10 base layer** — *exactly what the Session
  route outputs*, so **zero loss on the TV.** The **M2 Max MBP XDR** is the only DoVi-capable display, a
  laptop, where DoVi is currently *stuttery* anyway. So: can't verify it, main display can't show it, and
  where it could, smooth-HDR10 beats stuttery-DoVi.

**Decision: retire the sample-buffer route as the default; ship the Session route; do not build DoVi.**
Keep the sample-buffer code **parked** (not deleted) behind `PB_NO_SAMPLE_BUFFER`'s inverse, in case DoVi
ever revives with a WGSL implementation. If HDR dynamic metadata ever matters, HDR10+ (the Samsung format)
would be the one to consider — equally niche, equally unverifiable-by-agent, same conclusion.

---

## Execution plan

### 1. Flip the default route: MKV/WebM → Session

- **`crates/pb-app-core/src/app_core_impl.rs`**
  - `macos_sample_buffer_route(item)` (~7174): today returns `true` for `Mkv | Webm` (unless
    `PB_NO_SAMPLE_BUFFER`/`PB_SAMPLE_BUFFER=0`). **Change it to return `false` by default** so MKV/WebM fall
    through to the Session route in `start_video_session` (~7027–7037: native → sample-buffer → Session).
    Invert the env flag: make the sample-buffer route **opt-IN** (e.g. `PB_SAMPLE_BUFFER=1`) so it stays
    reachable/parked for DoVi experiments and A/B. Keep `start_sample_buffer_video` and the whole
    presenter intact — just not selected by default.
  - `macos_native_route` (AVPlayer for MP4/MOV) is checked **first** and is unaffected — MP4/MOV keep
    using AVPlayer (smooth).
- **Verify** the archive-video and Live-Photo-companion paths (which forced the Session route already via
  `video_ffmpeg_fallback`) still behave.

### 2. Close the parity gaps on the macOS Session route (the real work)

The Session route is smooth but was a *fallback* — confirm it has feature parity with what the
sample-buffer route grew. Known gap + things to verify:

- **⚠ Audio-track switching (#99) — the one confirmed gap.** `CoreModel.selectAudioTrack` (~3068) routes
  to `sampleBufferVideo` and `nativeVideo` only; the macOS Session route's audio player
  (`SessionAudioPlayer.swift`) has **no `switchTrack`**. The FFI exists: `session_audio_set_track`
  (`pb-mac-ffi` ~3450). **Add** a `SessionAudioPlayer.switchTrack(stream:at:)` that calls it, rebuilds the
  format (rate/channels can differ per track), re-primes, and reports the confirmed switch back through the
  same `audio_track_switched(row:ok:)` path (#99's confirmed-switch rule). Wire `selectAudioTrack` to it.
  Also confirm the **active-track report** (`reportActiveAudioStream` / the picker tick) works for the
  Session route, and that `cycle_audio_track` (`A`/`Shift+A`, `app_core_impl.rs` ~8126) drives it.
- **Subtitles (#90)** — the core renders them as an overlay clocked off `video_position()`, route-agnostic
  in principle. **Verify** they render + are selectable on the Session route (they should be, but confirm
  `C`/`Shift+C`, the picker, the settings preview).
- **Verify the rest**: seek (`video_session::seek_to/seek_by`), resume (`video_resume`, `startSecs`),
  poster/first-frame reveal, scale/Fit/Fill/Original + zoom/pan/rotation placement, play/pause, EOS/replay,
  scrubber progress + pinned-target (the seek-robustness §H3 pin is in `CoreModel`, likely route-agnostic —
  confirm), mute, HDR10 output on the XDR.
- **⚠ Session-route audio health.** The video-playback-overhaul flagged post-seek audio issues (R2/R4/R5)
  on the Session route (see `.taskmaster/docs/video-playback-overhaul.md` + memory `video-playback-overhaul`).
  Make MKV playback the daily driver → these become front-and-center. Budget for A/V sync + post-seek audio
  polish on the Session route; this is the biggest execution risk.

### 3. Clean up the diagnostic scaffolding

- **KEEP:** the `work_pending`/`needs_frame_pacing` pump fix (`216c591d`) — real efficiency win, ships.
- **KEEP (optional, cheap, behind `PB_TRACE`):** `sb-read`/`sb-play`/`pump diag` + the reveal Hz log. Useful
  if the sample-buffer route revives. Owner's call — could trim to reduce surface.
- **REVERT / REMOVE (throwaway A/B seams):** `PB_NO_SB_AUDIO`, `PB_NO_PROGRESS`, `PB_NO_MASK` (and revert
  `MetalCanvas.swift` `container.masksToBounds` to unconditional `true`), `PB_RETIME` (and remove
  `DemuxReader.presentationTime`/`frameRate`). None helped; they're cruft.
- Delete the test artifact `~/Downloads/adastra_avplayer_test.mp4` (a remux, not committed).

### 4. Testing (owner-verified — the smoothness + A/V bits can't be unit-tested)

- **Smoothness:** `Ad.Astra.…mkv` (local + SMB) plays smooth, `PB_TRACE` `sb-play`… — wait, the Session
  route has no `sb-play diag`; verify by eye + (if wanted) add a minimal drop counter to the Session
  present path. Compare against the AVPlayer MP4 (known-smooth reference) and mpv.
- **Parity checklist (owner):** audio-track switch (`A`/menu/picker) with a confirmed toast; subtitles
  (`C`/`Shift+C`/picker/settings); seek (arrow + scrubber, no pause/jump — the seek-robustness fixes must
  still hold); resume at nonzero position with audio; scale modes + zoom/pan/rotation; EOS + replay; mute.
- **HDR:** an HDR10 MKV renders correctly on the XDR (fp16 path). A DoVi MKV renders as a **clean HDR10 base
  layer** (acceptable, documented).
- **Regression:** MP4/MOV still route to AVPlayer and still resume/seek correctly (the seek-robustness
  resume-audio + scrubber fixes were on both routes — don't regress).

### 5. CHANGELOG + docs

- User-facing `Fixed`: *"Video playback is smooth again (macOS) — MKV/WebM now use the same renderer as
  everything else instead of a path that dropped frames."*
- Update `CLAUDE.md`'s video section: the macOS default for MKV/WebM is now the Session route; the
  sample-buffer route is parked (DoVi-only, opt-in, not built). Update the memory note
  `video-playback-overhaul` / add one for this.

---

## Open questions for Codex validation

1. **Is `macos_sample_buffer_route → false` the cleanest switch**, or should the sample-buffer selection be
   removed from `start_video_session` entirely (with the env as the only way back)? Any caller besides
   `start_video_session` that assumes MKV → sample-buffer?
2. **Session-route audio parity scope** — is `SessionAudioPlayer.switchTrack` the *only* gap, or are there
   core-side assumptions (`audio_active` reporting, the catalog's `av_plist` currency vs the session's
   FFmpeg-stream currency) that differ between routes? `audio_track_ff_stream` vs `audio_track_av_plist`
   (`CoreModel` ~3021/3025) — the two routes reach a track by different locators; confirm the Session route
   uses the FFmpeg-stream one end-to-end.
3. **Session-route audio robustness** — will making MKV the daily driver surface the overhaul's R2/R4/R5
   post-seek/network-seek audio issues? Should those be in-scope here or a follow-up?
4. **HDR10 correctness** on the Session route for MKV specifically (the sample-buffer route parsed the
   ISOBMFF `colr` box directly for HEIF; does the Session/FFmpeg path carry HDR10 metadata correctly for
   MKV?).
5. **Anything the sample-buffer route did that's genuinely lost** and matters — DoVi is decided; is there a
   second thing (e.g. a codec the Session route's FFmpeg build can't decode but VideoToolbox could)?

---

## Notes carried in

- **Repro corpus:** `~/Downloads/Ad.Astra.2019.1080p.BluRay.DDP.7.1.x264-Geek.mkv` (local) and
  `/Volumes/Media/Movies/…` (SMB); the 184 MKVs there are 1080p BluRay H.264 ~10.4s GOP. The remuxed
  `~/Downloads/adastra_avplayer_test.mp4` is the AVPlayer-smooth reference (delete after).
- **The env switch already exists:** `PB_NO_SAMPLE_BUFFER=1` forces the Session route on the *current*
  build — that's how the decisive test was run. Anyone can reproduce "Session route is smooth" immediately.
- **Don't drive the app from a tool session while the owner is testing** (`pkill` kills their window; a
  tool-launched bare binary is windowless — the trace runs are owner-run). Build with
  `./scripts/build-swift-host.sh --release` → `target/swift-host/release/Blaze Viewer.app`; `PB_TRACE=1` on
  the binary, `2> log`, gives the diagnostics.
- **The seek-robustness work** (`.taskmaster/plans/seek-robustness.md`) landed on both routes; its
  resume-audio, scrubber-pin, epoch-gating, and rate-capture fixes must not regress when MKV moves to the
  Session route — several were in `CoreModel`/route-agnostic, but the sample-buffer-specific ones
  (`SampleBufferPresenter`) simply stop being exercised.
- **Lesson from the saga:** the objective metric (`sb-play diag` dropped-frame count) is what let us kill
  seven wrong hypotheses cheaply and trust the two decisive route tests. When a fix is perceptual (DoVi) or
  when there's *no* metric, autonomous work stalls — which is exactly why we're routing to a known-smooth
  path instead of perfecting `AVSampleBufferDisplayLayer`, and why we're **not** building DoVi.
