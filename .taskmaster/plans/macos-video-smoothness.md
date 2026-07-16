# macOS video smoothness — route MKV to the Session (wgpu) path, retire the sample-buffer route

> Status: **IMPLEMENTED 2026-07-16 — all automatable work done + tested; awaiting the §4
> owner-verified pass** (smoothness by eye + `PB_TRACE` session diag, audio-switch/subtitle/
> seek/resume/scale/EOS/mute parity, HDR10 on the XDR, MP4-regression). Diagnosis was
> COMPLETE + owner-confirmed 2026-07-15; Codex-reviewed 2026-07-15; second review 2026-07-15
> (Claude — audio-switch transaction shape DECIDED, DoVi door-open design added).
> · Owner: JD
>
> **Execution notes (2026-07-16):** everything in §§1–5 that an agent can do is on main —
> routing flip + inverted/opt-in tests, A/B-seam cleanup, the switch-as-rebuffer transaction
> (`SessionAudioPlayer` + the pure `AudioSwitchPolicy` in PbSeek, 11 new policy tests),
> CoreModel session branches, DoVi detect + Details row + Profile-5 toast (+ tests), the
> `PB_TRACE` session dropped-frames diag, the `hdr_pq.mkv` fixture + container-parity test,
> CHANGELOG + CLAUDE.md. **Bonus fix found by the new decoder tests:** a pre-read seek
> (resume / post-switch) mis-anchored `FfAudioDecoder`'s media-zero `origin`, offsetting
> every LATER seek by the resume position (resume at 20:00 then scrub to 5:00 aimed the
> audio discard at 25:00 — possibly the residual "SMB cold-seek freeze"). One-line fix in
> `audio_decoder.rs::seek` + regression test `a_pre_read_seek_does_not_offset_later_seeks`.
> Windows cross-check (`cargo check -p pb-app --target x86_64-pc-windows-msvc --tests`) run
> and green.
> Scope: **macOS video playback smoothness.** The winit (Windows/Linux) shell already uses the
> Session route and is unaffected. This is a routing + parity change, not a rendering rewrite.
>
> **One-line summary:** `AVSampleBufferDisplayLayer` (the macOS "sample-buffer" route we use for
> MKV/WebM) drops ~3 frames/sec on steady-state playback that every other route plays flawlessly.
> We already have a smooth, mpv-class renderer — the **Session route** (FFmpeg → wgpu → Metal).
> Make it the default for MKV/WebM; keep the sample-buffer route parked — not as the future
> Dolby-Vision implementation, but as its **reference renderer**. DoVi itself is deferred with the
> door deliberately kept open (detection ships now; see *Keeping the DoVi door open*).

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

**But we decided NOT to build it now, for concrete reasons** (the door stays open — next section):
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

**Decision: retire the sample-buffer route as the default; ship the Session route; defer DoVi.**
Keep the sample-buffer code **parked** (not deleted) behind the opt-in flag (§1). If HDR dynamic metadata
ever matters beyond DoVi, HDR10+ (the Samsung format) would be the one to consider — equally niche,
equally unverifiable-by-agent, same conclusion.

### Keeping the DoVi door open (added 2026-07-15 — deliberate, and cheap now)

Deferring DoVi must not mean architecting it out. Four commitments keep a later DoVi feature a
*feature*, not a rework:

1. **The revival vehicle is WGSL-in-Session, not the parked route.** If DoVi ever ships, it is the
   RPU-reshaping pass in our existing fp16 scRGB shader (FFmpeg already exposes
   `AV_FRAME_DATA_DOVI_METADATA` per frame; the math is unit-testable against libplacebo/libdovi
   reference vectors — see the analysis above). Shipping DoVi by un-parking
   `AVSampleBufferDisplayLayer` would reintroduce the exact ~3 drops/sec this plan exists to fix —
   the parked route is never the end-state.
2. **The parked route's real future role: reference renderer + Profile-5 escape hatch.** The
   verification wall (perceptual color judgment, no agent-readable metric) shrinks a lot with an
   on-device oracle: the same clip through Apple's DoVi pipeline (`PB_SAMPLE_BUFFER=1`) on the XDR
   beside our shader output, plus mpv. That is why the route stays parked-and-tested rather than
   deleted (§4's opt-in routing test is what keeps it from rotting), and why its
   `DoviConfig` → `dvcC`/`dvvC` plumbing stays intact.
3. **Detection lands NOW (§2, small).** `read_dovi`/`DoviConfig` — profile, level,
   `bl_signal_compatibility_id`, the packed 24-byte config box — **already exists and is
   unit-tested** in `pb-decode/src/ffmpeg/demux.rs` (built for the sample-buffer demux). The
   Session producer reads the same stream side data at open and carries a slim summary on
   `Opened`, giving us: an honest UX for non-backward-compatible DoVi (§2), a content inventory
   (Details panel), and the input any future DoVi routing or shader work needs.
4. **A documented-not-built policy option: auto-route compat-0 DoVi to the parked route.**
   `bl_signal_compatibility_id == 0` (Profile 5, IPTPQc2) is the only DoVi flavor whose base layer
   is *visibly wrong* without the RPU; Apple's decoder renders it correctly, slightly stuttery —
   for that flavor alone, correct-but-stuttery beats wrong-color. If P5 content ever matters, a
   producer-side classified fallback (Session → sample-buffer, the mirror of today's level-2) can
   route just that flavor; the routing predicate itself must **never** synchronously probe a
   container on the keypress path (never block the event loop), which is why this is a
   producer-report design, not a `macos_sample_buffer_route` change. NOT built now — the owner's
   corpus has zero DoVi-P5 — recorded so the option isn't re-derived from scratch.

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
  - **One knob (added):** `PB_SAMPLE_BUFFER=1` is the *only* flag after the flip — **delete the
    `PB_NO_SAMPLE_BUFFER` / `PB_SAMPLE_BUFFER=0` handling** (the default *is* no-sample-buffer now; a
    legacy opt-out of an opt-in is noise). Read the env **once** into a field/constructor parameter
    rather than per-call — that is also the §4 test seam (tests set the field; no process-global
    `set_var`).
  - **Fix the stale doc comments while there (added):** the routing narrative at `start_video_session`
    (~7014), the `macos_sample_buffer_route` doc (~7163 — "Default on … the DoVi/HDR end-state"), and
    the routing test's doc (~11572) all state the old rationale as current. This repo has a record of
    stale comments misleading later agents; rewrite them to: Session is the smooth default,
    sample-buffer is the parked DoVi reference (opt-in), and link this plan.
  - `macos_native_route` (AVPlayer for MP4/MOV) is checked **first** and is unaffected — MP4/MOV keep
    using AVPlayer (smooth).
- **Not affected (Codex correction):** archive **MKVs** already fall through to Session (no file path);
  archive **MP4/MOV** use `PlayVideoBytes` (AVPlayer via a resource loader) and are untouched by this flip;
  **Live-Photo companions** use the animation/Live-Photo pipeline, not `start_video_session`, so they are
  unrelated. No extra verification needed for these beyond a smoke test.
- **Bonus (verified, added):** the flip also removes a wasted hop for non-H.264/HEVC **WebM** — the
  sample-buffer demux sample-decodes only H.264 + HEVC (`DemuxReader.buildFormatDescription` rejects
  everything else), so a VP8/VP9/AV1 WebM today burns an open + classified failure + level-2 fallback
  before landing on Session anyway. After the flip it goes there directly.

### 2. Close the parity gaps on the macOS Session route (the real work)

The Session route is smooth but was a *fallback* — confirm it has feature parity with what the
sample-buffer route grew. Known gap + things to verify:

- **⚠ Audio-track switching (#99) — the real work, and NOT a verbatim copy (Codex P1).**
  `CoreModel.selectAudioTrack` (~3068) routes to `sampleBufferVideo`/`nativeVideo` only; the Session route's
  audio (`SessionAudioPlayer.swift`) has no `switchTrack`. The FFI `session_audio_set_track` (`pb-mac-ffi`
  ~3438/3450) exists, **but it synchronously opens a NEW FFmpeg decoder on the same serial queue that
  services refills** — on SMB/slow storage the ~750 ms audio lookahead can drain while that queue is blocked
  (a *new* stutter), and its `true` return confirms **only decoder replacement**, not AVAudioEngine reconfig,
  seek, buffer scheduling, or resumed playback. **Do NOT copy `AudioSampleFeeder.switchTrack`
  (`AudioSampleFeeder.swift` ~196) verbatim — after its decoder replacement, a format-build failure has no
  rollback.** Design a proper **transaction**:
  - **DECIDED (2026-07-15): switch-as-rebuffer, on the existing feeder queue — no new FFI, no pause
    choreography.** The core already makes this safe: `on_audio_clock` applies clock corrections **only
    when the sample state is `Playing`** (`video_session.rs` ~703–746) — a `Buffering` sample is recorded
    but never yanks or freezes the video, which keeps running on its monotonic clock and re-syncs by
    bounded corrections once audio returns near the playhead. So a track switch is modeled as the
    transient stall the machinery was built for (plan 1G), and the slow-storage "queue blocked while the
    lookahead drains" hazard becomes a designed-for `Buffering` interlude inside an explicitly
    user-initiated action — not a mystery stutter. The transaction:
    1. *Main actor:* capture the authoritative playhead + target row/ff-stream; set `switching` (while
       set, `sample()` reports **Buffering at the frozen position**); bump `seekGen` (drops every
       in-flight read/seek completion); `node.stop()` (flush the old track's ~750 ms lookahead — the old
       language must not keep playing over the switch); zero `inFlight`/`reading`, clear
       `sourceDrained`/`rebuffering`.
    2. *Feeder queue* (serialized behind any in-flight op; generation captured):
       `session_audio_set_track(ptr, ffStream)`. **Refused (`false`)** → the old decoder is untouched,
       but its engine lookahead was already flushed — so **re-prime the OLD track at the playhead** (the
       `applySeek` dance) and report `ok = false`. **Succeeded** → read the new rate/channels +
       `session_audio_stream_index` on the queue.
    3. *Main-actor commit:* if rate/channels changed (`multitrack.mkv`'s 44.1 kHz-stereo-AAC →
       48 kHz-6ch-AC-3 is the normal case, not an edge), rebuild `format` and disconnect/re-connect the
       node with the new format — `PBCatchException`-shielded like every other graph call. A failure here
       **rolls back**: feeder-queue `session_audio_set_track(oldStream)` + re-prime; if even the rollback
       fails, latch `failed` — the core's existing degrade path (permanent silent fallback on the
       monotonic clock; playback never dies with the audio).
    4. *Feeder queue → main:* `session_audio_seek(ptr, playhead)` — the fresh decoder starts at zero, so
       the seek is **mandatory** (same as `AudioSampleFeeder.switchTrack`'s "resume where the picture
       is"); re-anchor `epochSecs`, clear `switching`, `topUp()`, `startNode()` unless paused.
    5. Report the **actual outcome**: `audio_track_switched(row, ok)` + update the cached
       `activeAudioStream` from `session_audio_stream_index` — what is *playing*, never what was asked
       (`open_track`'s stale-track policy fallback makes those legitimately differ).
    Concretely: add `OwnedAudioDecoder.setTrack(_:then:)` beside `read`/`seek`, and extend the `open`
    callback to also return the initial stream index (it seeds `activeAudioStream`, next bullet).
  - *Two-phase prepare/commit was considered and REJECTED:* it needs new FFI + a second queue + care with
    raw-pointer aliasing on the shared `SessionAudioDecoder` (every feeder-queue call takes `&mut` to the
    whole struct; a prep-queue read of `d.input` would alias it — UB territory), all to shave a stall the
    rebuffer path already bounds and reports honestly. Revisit only if measured switch latency on SMB is
    actually obnoxious.
  - The transaction is **generation-gated** end-to-end and targets the playhead **at commit time**, per
    the Codex requirements above — with one precision (added): supersession is only a plain *drop*
    **before** decoder replacement (step 2). Once the decoder has been replaced, the format
    rebuild/reporting **must still run** (the engine graph must match the live decoder — a dropped
    half-switch is exactly the mis-strided-garble hazard); what a superseding seek wins is the *position*
    (step 4 re-resolves to the latest authoritative playhead), and a superseding *switch* simply queues
    behind on the serial feeder queue.
  - **Interaction with preroll (verified, added):** `audio_ready_or_absent` (`video_session.rs` ~681)
    counts only `Paused|Playing|Ended` as ready — a seek landing mid-switch sees `Buffering` audio and
    waits, **bounded by `AUDIO_READY_TIMEOUT`**, then degrades to silent-until-the-clock-returns and
    re-syncs through the clock bridge (the code's own design for a late-joining player). No deadlock is
    possible; worst case is a brief silent preroll during a pathological seek-during-switch. Acceptable —
    note it in the Swift-side transaction tests (§4).
  - Wire `CoreModel.selectAudioTrack` to it (a `sessionAudio` branch beside `sbv`/`nv`), resolving the row
    through `audioRowFfStream` — the FFmpeg-stream locator end-to-end. `cycle_audio_track` (`A`/`Shift+A`,
    `app_core_impl.rs` ~8126) drives it via the same `SelectAudioTrack` effect and needs nothing new.
- **⚠ Active-track reporting is a REQUIRED change, not "confirm" (Codex P1).** `resolveActiveAudioRow()`
  (`CoreModel.swift` ~3010) handles sample-buffer + AVPlayer and then **explicitly clears the tick for every
  Session video** — so even a *successful* switch loses its checkmark the next time the picker opens. Add a
  cached `SessionAudioPlayer.activeAudioStream`: **populate it from the initial open result**, update it from
  the completed switch transaction, and add **`sessionAudio` branches to both `resolveActiveAudioRow()` and
  `selectAudioTrack()`**. The two routes reach a track by *different locators* — the picker exposes
  `audio_track_ff_stream` (FFmpeg stream index, ~3021) and `audio_track_av_plist` (~3025); the Session route
  must use the **FFmpeg-stream** currency end-to-end.
- **DoVi on the Session route — detect + warn (added, small).** Reuse `read_dovi`
  (`pb-decode/src/ffmpeg/demux.rs` — already parsed + unit-tested for the sample-buffer demux) in the
  Session producer's open path; carry a slim `(profile, bl_signal_compatibility_id)` summary on `Opened`.
  When compat-id = 0 (Profile 5): a one-time toast — *"Dolby Vision (Profile 5): colors can't be shown
  correctly"* — and a Details-panel fact. Compat-id 1/2/4 (P7/P8.x HDR10-/SDR-/HLG-compatible base
  layers) play their base layer silently — that *is* correct degradation. This turns §4's "explicitly
  unsupported / known-bad" from a release-note footnote into something the user is told at play time, and
  it is the foundation piece of *Keeping the DoVi door open* above.
- **Subtitles (#90)** — the core renders them as an overlay clocked off `video_position()`, route-agnostic
  in principle. **Verify** they render + are selectable on the Session route (they should be, but confirm
  `C`/`Shift+C`, the picker, the settings preview).
- **Verify the rest**: seek (`video_session::seek_to/seek_by`), resume (`video_resume`, `startSecs`),
  poster/first-frame reveal, scale/Fit/Fill/Original + zoom/pan/rotation placement, play/pause, EOS/replay,
  scrubber progress + pinned-target (the seek-robustness §H3 pin is in `CoreModel`, likely route-agnostic —
  confirm), mute, HDR10 output on the XDR.
- **⚠ Session-route audio health (Codex P2 — corrected).** R2/R4/R5 (post-seek audio) are **already closed
  + owner-confirmed** (`.taskmaster/docs/video-playback-overhaul.md` ~434) — **test them for regression, do
  NOT reopen their scope.** The remaining measured network item is **R9: duplicate demuxers + shared packet
  read-ahead** — keep it a **follow-up** unless this repro (making MKV the daily driver over SMB) actually
  demonstrates it. Track-switch correctness (above) is the in-scope audio risk, not the demuxer redesign.

### 3. Clean up the diagnostic scaffolding

- **KEEP:** the `work_pending`/`needs_frame_pacing` pump fix (`216c591d`) — real efficiency win, ships.
- **KEEP (optional, cheap, behind `PB_TRACE`):** `sb-read`/`sb-play`/`pump diag` + the reveal Hz log. Useful
  if the sample-buffer route revives. Owner's call — could trim to reduce surface.
- **REVERT / REMOVE (throwaway A/B seams):** `PB_NO_SB_AUDIO`, `PB_NO_PROGRESS`, `PB_NO_MASK` (and revert
  `MetalCanvas.swift` `container.masksToBounds` to unconditional `true`), `PB_RETIME` (and remove
  `DemuxReader.presentationTime`/`frameRate`). None helped; they're cruft.
- Delete the test artifact `~/Downloads/adastra_avplayer_test.mp4` (a remux, not committed).

### 4. Testing — automatable core + owner-verified perceptual (Codex P2: TDD is required)

**Automatable (write these — the routing, locator mapping, rollback, reporting, and switching are pure
logic, only the *smoothness* is perceptual):**
- **Invert the existing macOS routing regression test** (`app_core_impl.rs` ~11572) — it currently asserts
  MKV → sample-buffer; it must now assert MKV/WebM → Session, MP4/MOV → AVPlayer.
- **Opt-in route test without mutating process-global env** (don't `std::env::set_var` in a test — the
  env-read-once field from §1 *is* the seam; tests set the field) so the parked `PB_SAMPLE_BUFFER=1` path
  stays covered — this is also what keeps the DoVi reference renderer from rotting.
- **`session_audio_set_track` / `FfAudioDecoder::open_track`** against the committed `multitrack.mkv`
  fixture, in `pb-decode/src/ffmpeg/audio_decoder.rs`'s existing test module (the fixture and `open_track`
  both live there). The fixture conveniently switches **44.1 kHz stereo AAC → 48 kHz 6-ch AC-3**,
  exercising the format rebuild: assert **success**, **refusal** (old stream still decoding, format
  intact), **actual-stream reporting** (`stream_index()` reflects what's playing, not what was requested),
  and **post-switch seek + read** yields frames (the transaction's step 4 is mandatory — a fresh decoder
  starts at zero).
- **The Swift-side transaction ordering** (generation gating, refused-switch re-prime, rollback,
  stale-generation completions dropped before touching the graph): factor the switch state machine into a
  plain-Swift component tested in the **PbSeek pattern** (`mac/PbSeek/Tests` is the precedent from the
  seek-robustness arc) where practical; whatever genuinely can't be factored off `AVAudioEngine` goes on
  the owner-verified list below.
- **DoVi detection (§2):** unit-test the compat-id-0 warning trigger on a synthetic `DoviConfig`
  (`build_dovi` is already unit-testable without a clip); extend the `PB_DOVI_TEST`-gated ignored test to
  assert the Session producer surfaces the summary on a real DoVi clip.
- **Add a small HDR10 MKV fixture** (remux/generate) so HDR metadata carriage is verified for the **MKV**
  container, not only the current MP4 fixture — Codex confirms the FFmpeg Session path already carries
  per-frame color/transfer into P010/fp16, but there's no MKV regression fixture proving it.

**Owner-verified (perceptual / A/V — no metric an agent can read):**
- **Smoothness:** `Ad.Astra.…mkv` (local + SMB) plays smooth. The Session route has no `sb-play diag`,
  but the objective number is cheaper than assumed: **`VideoSession::dropped_frames()` already exists and
  is unit-tested** (`video_session.rs` ~763, the plan-1C/0B late-frame counter) — it's just read by
  nothing on the app path. **In scope (cheap):** surface it as a periodic `PB_TRACE` diag line (the
  Session analog of `sb-play diag`). The saga's own lesson says don't accept a perceptual pass without a
  number; this also gives future smoothness regressions a metric on day one. Then verify by eye against
  the AVPlayer MP4 (known-smooth reference) and mpv.
- **Parity:** audio-track switch (`A`/menu/picker) with a confirmed toast **and a surviving tick on re-open**;
  subtitles (`C`/`Shift+C`/picker/settings); seek (arrow + scrubber — the seek-robustness fixes must still
  hold, no pause/jump); resume at nonzero position with audio; scale + zoom/pan/rotation; EOS/replay; mute.
- **HDR (Codex P1 — corrected acceptance):** an HDR10 MKV renders correctly on the XDR (fp16). **DoVi
  acceptance is profile-specific, NOT a blanket "clean HDR10 base layer":** Profiles **7 and 8.1** have
  HDR10-compatible base layers and degrade cleanly; **Profile 5 is NOT HDR10/SDR-compatible** — ignoring its
  RPU produces *visibly wrong* color (the green/purple tint), so it is **explicitly unsupported / known-bad**,
  not "clean." State this in the release notes; don't claim all DoVi degrades gracefully. The §2 play-time
  Profile-5 warning is what makes "known-bad" honest — if a P5 sample is available, verify the toast fires
  and that compat-1/2/4 content plays its base layer silently (no toast).
- **Regression:** MP4/MOV still route to AVPlayer and still resume/seek (the seek-robustness resume-audio +
  scrubber fixes were on both routes — don't regress).

### 5. CHANGELOG + docs

- User-facing `Fixed`: *"Video playback is smooth again (macOS) — MKV/WebM now use the same renderer as
  everything else instead of a path that dropped frames."*
- User-facing `Added` (if the §2 DoVi warning ships): *"Dolby Vision Profile 5 videos now explain why
  their colors look wrong instead of failing silently (macOS)."* (Wording to taste.)
- Update `CLAUDE.md`'s video section: the macOS default for MKV/WebM is now the Session route; the
  sample-buffer route is parked (opt-in `PB_SAMPLE_BUFFER=1`, kept as the DoVi reference renderer; DoVi
  itself deferred with detection shipped). Update the memory note `video-playback-overhaul` / the
  `macos-video-smoothness-arc` note for this.

---

## Open questions — RESOLVED by Codex review (2026-07-15)

1. **Route switch shape:** *Keep* `macos_sample_buffer_route` and the selection branch (it's called only
   from `start_video_session` — smallest reversible seam); make it **exact opt-in via `PB_SAMPLE_BUFFER=1`**.
2. **Audio parity scope:** `switchTrack` is **NOT the only gap** — also required: active-stream
   caching/reporting, serialized decoder operations, graph rollback, clock coordination, and completion
   generation. The Session route uses the **FFmpeg-stream** locator (`audio_track_ff_stream`) end-to-end.
   (Folded into §2 above.) **Transaction shape decided 2026-07-15: switch-as-rebuffer** — justified by the
   verified core behavior that non-`Playing` clock samples never correct the video clock (§2).
3. **Audio robustness:** R2/R4/R5 are **already closed** — test for regression, don't reopen. **R9**
   (duplicate demuxers + shared read-ahead) is the remaining architecture item; **follow-up**, not this task.
4. **HDR10/MKV:** the FFmpeg Session path **already extracts stream HDR metadata + carries per-frame
   color/transfer into P010/fp16**. Requirement: add a real **MKV** HDR10 regression fixture before declaring
   container parity (the current fixture is MP4 only). (Folded into §4 above.)
5. **Anything else lost:** No second codec advantage — the parked sample-buffer route decodes **only H.264 +
   HEVC** (`DemuxReader.swift` ~203); Session/FFmpeg is broader. **Dolby Vision is its only meaningful
   distinction**, and that's deferred — see *Keeping the DoVi door open* (added 2026-07-15): detection
   ships now, the parked route is the future reference renderer, WGSL-in-Session is the revival vehicle.

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
  path instead of perfecting `AVSampleBufferDisplayLayer`, and why DoVi is deferred until there's a
  verification story (the parked route as an on-device reference oracle is that story's first half — see
  *Keeping the DoVi door open*).
