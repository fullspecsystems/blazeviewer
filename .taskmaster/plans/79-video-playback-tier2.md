# Task 79 — Video playback (tier 2): camera clips of any length

**Status:** planned (2026-07-11; owner decisions incorporated — see Decisions)
**Scope:** all three platforms behind the existing seams; implemented Windows-first (primary),
Linux close behind (FFmpeg mirrors), macOS via the same pb-app-core contract.

## Goal

Open a real photo library (iPhone/camera dump) and view **everything** in it. Videos —
overwhelmingly 10 s – 15 min mp4/mov clips — become first-class items that behave exactly like
Live Photos / animated images do today: the item shows instantly as a **poster frame** with the
**play badge**, and `P` plays it. Because the player is a forward-only stream with constant
memory, a 2-hour feature film *works* (flat RAM, bounded A/V drift, fast start) and is merely a
coarse movie player by design.

**Non-goals (permanent-ish):** subtitles, scrub-bar/timeline UI, audio-track/chapter selection,
remember-position, hardware decode / GPU zero-copy (stays the gated escalation), MKV demuxing on
Windows/macOS (no OS demuxer — graceful "can't play"; self-selects toward camera content).

## Decisions locked (owner, 2026-07-11)

| Question | Decision |
|---|---|
| Tier | Build the **middle tier directly** — streaming discard-ring player; never ship the RAM-bounded interim. |
| Poster | **First non-black frame**, always: cheap mean-luma walk, not semantic frame picking. |
| UX shape | Identical to Live Photos/animated images: poster + play badge, `P` to play. |
| Seeking | **`←`/`→` = ±2 s while a video is playing; holding scrubs** (auto-repeat of coalesced seeks). Overrides pan/nav *only during video playback*, and pan wins when the view is zoomed with horizontal overflow (rare for video). `Space`/`Backspace` keep navigating; everything else unchanged. |
| Long content | 2 s is tuned for short clips; `Shift+←/→` = **15 s** for longer files (owner-revised 2026-07-11 from 30 s — 30 s jumps only pay off on genuinely long files, which are edge content here; constant, revisit only if real use demands). |
| Loop semantics | `AnimationKind::Video`: play once, park on the last frame; `P` replays; **no** Live-Photo revert-to-still. |

## Verified current state (2026-07-11 survey; key anchors)

- The per-platform motion decoders are **codec-general and stream from disk by path**:
  `mf_video.rs:136` (`MFCreateSourceReaderFromURL`, first video stream, RGB32 out),
  `livephoto.rs:341` (AVAssetReader), `ff_live.rs:134` (FFmpeg). Nothing in the open path is
  Live-Photo-specific; the wrapping is (`codec: "Live Photo"` at `mf_video.rs:231`,
  `loop_count: 1`, the 450 ms revert).
- **`MotionChunk` streaming exists end to end** (`animation.rs:113-152` contract,
  `start_live_stream` → `poll_anim_stream` → `install_stream_playback` → `present_anim_frame`
  in `app_core_impl.rs`), but the consumer **appends every frame to a Vec forever**
  (`Playback` walks a growing array; no eviction) — that's the single line tier 2 crosses.
- **PTS is discarded** by the producers (`mf_video.rs:206`); pacing is last-present + nominal
  delay. Fine at 3 s, wrong at 15 min.
- **Audio is fire-and-forget** per platform (WinRT `MediaPlayer` by URI / `AVAudioPlayer` /
  FFmpeg-PCM → `pw-cat` pipe, `live_audio.rs`), started with the frame pump and never
  consulted again. ⚠ The Linux path **pre-decodes the entire PCM** — 2 h stereo f32 ≈ 2.7 GB.
- **Videos are invisible today**: `is_supported_extension` (`lib.rs:478`) rejects mp4/mov/webm/
  avi (a test asserts it); a `.mov` exists only as a Live-Photo companion
  (`engine.rs:536 companion_motion`, same-stem match).
- Frame caps `MAX_MOTION_FRAMES=600` / `MAX_DECODED_BYTES=1.5 GiB` truncate; with the discard
  ring those caps simply **stop applying to Video** (they remain for the batch kinds).
- The presenter allocates per frame (`present_anim_frame` → `set_image`; `StagingUpload`
  allocs a staging buffer per upload) — tolerable for an 8 s GIF, real churn over 27,000
  frames; the reuse ring is in scope here (also closes the #76 perf-gate follow-up).

## The three guardrails (what makes "any length works" true)

1. **Path-only video items.** A video's bytes must never travel the RAM `PhotoSource` path — a
   2 h 4K movie is 10-50 GB. Extension-routed detection (no content sniff), poster + playback +
   metadata via the path-based readers, and an **audit of every `source.bytes()` / `fs::read`
   touchpoint** (EXIF panel, copy-image, hashing, describe) gated for video items. Archive
   entries have no path ⇒ archive-embedded videos are excluded (as Live Photos already are).
2. **Duration-independent state everywhere, including audio.** No "collect then finish" step
   anywhere in the video path; the Linux audio path becomes incremental decode-and-pipe.
3. **No timeline assumptions.** No whole-clip index; backward motion is a seek, not an array
   walk. (Frame-step `.` works forward while paused — pull one frame; `,` is unsupported for
   Video in v1.)

## Architecture

```
item (path-only) ──► poster decode (prefetch pool, like any slow still)
                       per-platform first-frame + non-black luma walk (≤1s cap)
                       → DecodedImage { animated: Some(Video) } → play badge
                       cached in resident ring / meta_cache (RAM-only)

P ──► VideoSession
        producer thread (per-platform reader, opened by path)
          │  emits (AnimFrame, pts) — PTS from the stream, no longer discarded
          ▼
        bounded ring channel (≈8-16 frames)   ← backpressure = pacing for free:
          │  producer blocks when full; decode never runs ahead unboundedly
          ▼
        consumer (event-loop tick): present frame when master_clock ≥ pts
          master clock = audio position (polled ~4×/s, smoothed) when audio
          is playing, else monotonic clock; pause freezes it; seek re-anchors
        frames DROPPED after present — constant memory at any duration

commands (consumer → producer): SeekTo(t) [coalesced], Stop
seek = reopen/reposition at keyframe: MF SetCurrentPosition / FFmpeg
av_seek_frame / AVAssetReader recreate with timeRange; flush ring; re-anchor
clock; reposition audio (Position / currentTime / restart pw-cat at offset)
```

- **Decode slower than real time** (v1 policy): playback slows — the clock effectively waits
  for frames; no frame-drop engine. Acceptable degradation for CPU decode of large content.
- **Seek coalescing:** while a seek is in flight, further `←`/`→` presses adjust the *target*
  rather than queueing — holding the key becomes scrubbing at roughly one landed seek per
  ~100-150 ms (10-20 s of content per second of holding at the 2 s step).
- **Key resolution** (pb-app-core `KeyResolution`): `SeekForward`/`SeekBackward` bind to
  `→`/`←` and are consumed **only when** a Video playback is active **and** the view has no
  horizontal pan overflow; otherwise the keys fall through to today's behavior. This keeps the
  photo keymap contract untouched.
- **HUD:** during seek (and briefly on play/pause) show a position pill — `m:ss / m:ss` — via
  the existing toast/HUD compositor; the info line gains duration + codec for video items.
- **Existing paths untouched:** Live Photos, GIF/APNG/WebP, avis keep their current batch /
  append-streaming models and caps. `AnimationKind::Video` is a new arm, not a rewrite.

### Poster: first non-black frame

Decode frame 0 → mean luma over a downsampled grid; if below threshold (≈ 16/255), continue
decoding forward up to **1 s or ~30 frames** (whichever first), take the first frame above
threshold; if all dark, keep the *last* sampled frame (better than frame 0 for fade-ins).
Bounded, cheap (only paid by fade-in/screen-recording content), pure-function testable.
**Windows Shell thumbnails ruled out** for generation: requesting one populates
`thumbcache.db` with content pixels for viewed files — an ADR-018 viewing trace. The only
acceptable variant is an opportunistic `SIIGBF_INCACHEONLY` read (never generates); decoding
ourselves is the baseline and the tested path.

### Extension allowlist (per-platform, graceful-absence like HEVC/AV1 today)

| Container | Windows (MF) | macOS (AVF) | Linux (FFmpeg) |
|---|---|---|---|
| mp4 / m4v / mov | ✔ (H.264 in-box; HEVC/AV1 via Store ext) | ✔ | ✔ |
| webm | Only with "Web Media Extensions" | ✘ | ✔ |
| avi | partial (old codecs) | ✘ | ✔ |
| mkv | ✘ (graceful error) | ✘ | ✔ |

**Companion dedup rule:** a `.mov` that pairs with a same-stem still (`companion_motion`)
must NOT appear as its own item — otherwise every Live Photo lists twice.

## Phases (mirror tasks.json #79 subtasks)

1. **Item plumbing.** Allowlist + per-platform gating; path-only rule enforced at the type
   level where possible (video items never construct a bytes request); companion dedup;
   archive exclusion; RAM-path audit. *Accept:* mixed folder lists videos once; EXIF panel /
   copy / describe on a video item never reads the file into RAM (test with a sparse
   multi-GB dummy file — instant, no allocation spike).
2. **Posters.** Per-platform first-frame + the luma walk; prefetch/ring integration; badge via
   `animated: Some(Video)`. *Accept:* blaze through a video-heavy folder — posters appear via
   prefetch, zero black posters on fade-in fixtures, hold-to-fly degrades gracefully (no
   stalls, no blanks).
3. **Streaming Playback core.** `VideoSession`, bounded ring, discard-after-present,
   play-once-park semantics. *Accept:* a 10-min clip plays end to end with flat memory
   (RSS delta < 100 MB over the run); navigate-away tears down within ~100 ms.
4. **PTS pacing.** Producers emit stream PTS (Video path only; Live-Photo path untouched);
   presentation anchored to PTS; VFR fixture plays at correct wall duration. *Accept:* a
   60 s fixture completes in 60 s ± 0.5 s; VFR fixture's uneven cadence is reproduced.
5. **A/V sync.** Audio-position master clock + smoothing; pause/resume/mute/seek correctness;
   Linux incremental audio. *Accept:* sync probe (tone-blip + flash-frame fixture) stays
   within ~50 ms over 5 min incl. two pause/resume cycles.
6. **Seeking.** Reopen-at-keyframe per backend; ±2 s / Shift 15 s; coalescing; HUD position
   pill; audio repositioning; key-resolution gating vs pan/nav. *Accept:* hold-→ scrubs
   smoothly through a 4-min clip; seek lands within one keyframe interval; photos' arrow
   behavior byte-identical when no video is playing.
7. **GPU upload ring.** Texture/bind-group reuse for the animation present path (all kinds
   benefit; closes the #76 follow-up). *Accept:* `present_anim_frame` p95 drops measurably
   (record numbers); zero per-frame texture creation during steady playback.
8. **Tests, audit, perf, docs.** See matrix. CLAUDE.md (keymap, wired-notes, format matrix),
   CHANGELOG ("Videos in your library now play"), no-trace test still green.

## Test matrix

- **Pure units:** luma-walk (dark/bright/fade fixtures as raw buffers); ring/backpressure
  state machine (producer-blocks, discard, teardown); seek coalescing; PTS pacing math;
  clock re-anchoring across pause/seek. All platform-free logic → pb-core-style tests.
- **Fixtures** (ffmpeg-generated, committed tiny like the avis set): 3 s solid-color mp4
  (poster + basic play), fade-from-black mp4 (non-black poster), VFR mp4, tone-blip+flash
  sync fixture, 60 s pacing fixture; one long low-res synthetic (generated at test time or
  in the corpus, not committed) for memory flatness.
- **Guardrail tests:** sparse-file no-RAM-read; duration-independence (memory flat over the
  long synthetic); no-trace integration test extended to include viewing a video folder.
- **Corpus verification (manual smoke):** real iPhone clips incl. Live Photos coexisting with
  standalone videos (dedup!), a feature-length mp4 (plays, coarse seeks, flat memory), an
  MKV (graceful error), webm with/without the Store extension.

## Risks / open items

- **AVAssetReader has no cheap in-place seek** (recreate with `timeRange`) — scrub cost on
  macOS is a reader rebuild per landed seek (~100-300 ms); acceptable, but measure.
- **MF `SetCurrentPosition` + `ENABLE_ADVANCED_VIDEO_PROCESSING`** interaction should be
  spiked early (phase 3) — if repositioning is flaky, fall back to reader rebuild like macOS.
- Audio-position APIs report with device-latency offsets; the smoothing constant needs one
  tuning pass on real hardware (phase 5 measurement).
- `pw-cat` restart-at-offset for Linux seek is crude (gap during restart) — fine for v1.
- Estimate: **2-3 weeks**, Windows-first; phases 1-2 are independently shippable (posters +
  badges with playback still tier-1-less would already improve the library experience, though
  we won't ship that intermediate state unless the schedule demands it).
