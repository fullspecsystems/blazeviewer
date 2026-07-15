# Task 79 — Video playback (tier 2): camera clips of any length

**Status:** planned — **rev2** (2026-07-11: Codex review incorporated after verification; the
four load-bearing claims were checked against the tree and all held — see Verified findings)
**Scope:** all three platforms behind the existing seams; **Windows-first milestone**, then
Linux/macOS parity as follow-on work (see Schedule).

## Goal

Open a real photo library (iPhone/camera dump) and view **everything** in it. Videos —
overwhelmingly 10 s – 15 min mp4/mov clips — become first-class items that behave exactly like
Live Photos / animated images today: instant **poster frame** + **play badge**, `P` plays.
The player is a forward-only stream with constant memory, so a 2-hour film *works* (flat RAM,
bounded A/V drift, fast start) and is merely a coarse movie player by design.

**Non-goals (permanent-ish):** subtitles, scrub-bar/timeline UI, audio-track/chapter selection,
remember-position, frame-drop engine (seam reserved), hardware decode / GPU zero-copy (gated
escalation), native HDR video output (see Color policy).

## Decisions locked

| Question | Decision |
|---|---|
| Tier | Middle tier directly — streaming player; never ship a RAM-bounded interim. |
| Poster | **First non-black frame** via a capped mean-luma walk (≤1 s / ~30 frames). Dark-scene/letterbox misclassification is a documented limitation: fallback = last sampled frame; night-clip fixtures in the corpus make it deliberate. |
| UX shape | Poster + play badge, `P` to play, park on last frame, `P` replays (from t=0, not the poster time). No autoplay, no revert-to-still. |
| Seeking | **Horizontal-pan actions become seek while a video plays** (±2 s; `Shift` = ±15 s; hold = scrub via latest-value coalescing). Pan wins when the view is zoomed with horizontal overflow. `Space`/`Backspace` keep navigating. Contextual routing, NOT new default arrow bindings (see Input). |
| Underrun policy (rev2) | **Rebuffer, don't drift**: queue empty before EOS → pause audio, freeze the clock, hold the last frame, refill to preroll, hard re-anchor, resume together. Frame dropping stays a future policy behind the seam. |
| Queue bound (rev2) | **Bytes, not frames**: 2-3 decoded frames of lookahead under an explicit byte budget, with a one-frame exception when a single fitted frame exceeds it. |
| Color (rev2) | **Tier-2 guarantee: correct, tested SDR** — right matrix/transfer/range, P3-SDR preserved where available, HDR tone-mapped (or OS-converted) deterministically, poster and playback bit-identical in color policy. `VideoFrame` carries `VideoColorInfo` + pixel format now so fp16/NV12 backends slot in later without rewriting the session. |
| Output size (rev2) | **Not the 1440 Live-Photo cap** (`engine.rs:105` was a preview policy): fit to viewport, never upscale past source, geometry fixed per session; GPU scales during window resize; new fit applies on next play. |
| Unsupported containers (**owner-confirmed 2026-07-11**) | Common video containers (incl. MKV) are *visible* items with a generic placeholder poster and a "codec not available" error on `P`. Rationale: better than showing nothing, and the error itself becomes telemetry-by-eyeball for which codecs are worth supporting. Consequence: the recognition list is **one cross-platform container list**; per-file capability is a *runtime* property (the poster attempt is the capability probe) — so codec coverage grows with the OS without PhotoBlaze shipping anything. On Windows the error should name the fix when one exists (the HEVC/AV1/Web-Media Store extensions — same pattern as HEIC stills). ⚠ Codec-pack caveat: K-Lite/LAV are **DirectShow** filters, invisible to Media Foundation — installing them will NOT light PhotoBlaze up; only MF-registered handlers (the Store extensions) do. Also: Windows 10+ ships a **native MKV byte-stream handler** (Movies & TV plays MKV), so MF may demux MKV out of the box — phase-0 spike verifies and the format matrix gets corrected from measurement, not assumption. macOS has no user-installable AVFoundation codec path (Perian-era components are dead): Apple-blessed formats per OS version are the ceiling unless we ever bundle an FFmpeg backend there (Linux-style; deliberate future decision, not free). |

## Verified review findings (rev2 — all checked against the tree)

1. **Bare arrows are `PanLeft`/`PanRight`** (`keymap.rs:502-503`); `Prev` is Backspace. The
   CLAUDE.md "Minimal UI" keymap list is the stale v0 spec (fix in phase 7 docs). Consequence:
   seek contends only with *pan*, exactly as the owner framed it — and seek must be a
   **contextual rewrite of the pan actions** in `AppCore`, not new default bindings, so custom
   keymaps keep working. Held-seek uses the app's own timers (OS repeat stays ignored) and
   cancels on release/focus-loss/navigation/session-end.
2. **Dropping `IMFSourceReader` can block ~1 s** (existing comment, `mf_video.rs:186`) —
   teardown criteria must split "UI cancellation within a frame" from "resources eventually
   released, bounded retirement" (below).
3. **The extension predicate is shared with archive indexing** (`pb-source/src/lib.rs:277,494,
   950` — callers pass `pb_decode::is_supported_extension` in). Broadening it would index
   videos inside ZIPs, violating path-only. Predicates must split (phase 1).
4. **`ItemSource::bytes()` is unconditional and `decode_item` always calls it**
   (`engine.rs:223`) — "check the extension first" is a convention, not enforcement. Video
   needs a **typed item classification** dispatched before any bytes request.
5. Prior survey facts stand: PTS discarded (`mf_video.rs:206`), Windows negotiates full-size
   RGB32 then per-frame copy + Lanczos (`mf_video.rs:144,213`), Linux audio pre-decodes whole
   PCM, audio effects are one-way (no position/readiness events back into core).

## Architecture (rev2)

### VideoSession — a separate core, not an extension of `Playback`

Live Photos / GIF / avis keep their existing `Playback`/`MotionChunk` paths untouched.

```
state:   Opening → Buffering → Playing ⇄ Paused → Seeking → Buffering → …→ Ended
         (terminal: Failed, Stopped)
identity: item, session_id, seek_generation
timing:  session-relative PTS origin (normalize nonzero/negative starts; keep rational
         media time as long as possible — no accumulated float deltas), clock anchor,
         last AudioClockSample, desired seek target (latest-value)
queues:  bounded decoded-frame receiver (byte-budgeted), latest-value command channel
present: current PTS, EOS flag, reusable GPU slot index
```

- **Frame contract:** `VideoFrame { session_id, seek_generation, pts, pixels, dims,
  color: VideoColorInfo }`. Stale session/generation frames are discarded at the consumer even
  if they raced a flush.
- **Queue (byte-budgeted):** lookahead 2-3 frames, budget ≈ 2× one fitted 4K RGBA frame
  (~64 MiB; constant, documented), one-frame exception above budget; CPU pixels released
  immediately after GPU upload; the GPU texture ring is bounded separately. Invariant
  (unit-tested structurally): `cpu_queued_bytes ≤ max(budget, one_frame) ∧ frames ≤ 3`.
- **Backpressure that can't deafen the producer:** demand-driven — the consumer grants
  capacity credits; the producer decodes only when credit exists and *selects* over
  {credit, command} so `Stop` and the latest `SeekTo` always interrupt (no blocking `send()`
  into a full channel; a paused session naturally holds zero credits without wedging
  commands). Crossbeam `select!` or an equivalent two-channel poll.
- **Clock:** master = audio when present and playing — from `AudioClockSample` events
  (below), extrapolated between samples via monotonic time with small bounded corrections;
  hard re-anchor (never smooth) on pause/seek/rebuffer. No audio (or audio Failed → silent
  playback): monotonic clock. Linux v1: **shared monotonic clock** — piped-sample counts are
  underrun telemetry, not a device clock; a true PipeWire presentation clock is future work.
- **Underrun/rebuffer:** per the locked decision. Preroll = 2 frames + audio ready-or-absent
  before entering `Playing`.

### Audio-clock return path (new core⇄shell contract)

Today's audio effects are one-way. Add a shell-neutral event:

```
AudioClockSample { session_id, state: Opening|Playing|Paused|Buffering|Ended|Failed|Absent,
                   position, sampled_at }
```

~4 samples/s. `session_id` prevents a straggler from a replaced player re-anchoring a new
session. Windows: `MediaPlaybackSession` exposes position/state/buffering/seek-completion.
macOS: `AVAudioPlayer.currentTime` readable+settable; call `prepareToPlay()` off the UI path
before sync-start. Linux: state + written-sample counts only (honest telemetry).

### Seek (exact landing spec)

1. Clamp target to the seekable range; 2. bump `seek_generation`, discard queued frames;
3. reposition/recreate the reader (MF `SetCurrentPosition`; FFmpeg `av_seek_frame` **+ decoder
flush**; macOS: recreate `AVAssetReader` with a new `timeRange` — its range is immutable once
reading starts); 4. decode forward from the keyframe, discarding frames before the target;
5. publish the first frame ≥ target (documented tolerance: one frame interval); 6. seek audio
and await its acknowledgement; 7. hard re-anchor; resume only if playing before the seek.
A seek while paused stays paused but updates the displayed frame. A newer target supersedes
every stage of an older seek (latest-value, relative to the previous *desired* target so held
keys scrub the intent, not whatever last landed). Superseded seeks must never flash frames.

### Windows frame path (spike before the session is built)

The current path (full-res RGB32 → copy → CPU Lanczos per frame) will dominate 4K playback.
**Phase-0 spike:** request fitted output geometry in the negotiated media type so the MF video
processor scales; fallbacks behind the backend seam: fast SIMD scale (video moves; Lanczos is
still-photo polish), NV12 upload + shader convert, or the current path as correctness
baseline. Acceptance: steady 4K30 decode-to-fit on the target machine with no per-frame
Lanczos in steady state. **Also: deselect all streams, then select only the video stream** —
MF documents that selected-but-unread streams queue samples indefinitely (the audio stream is
played by the separate audio player, not read from this reader).

### Teardown (split criteria)

UI/session cancellation observed within one frame; no stale-session frame can present; audio
stopped promptly; decoder resources released *eventually* via session generations + a bounded
retirement pool (never join a decoder thread from the event loop; `IMFSourceReader` drop can
block ~1 s, `mf_video.rs:186`). Rapid enter/leave must not grow retired workers unboundedly.

## Item model (rev2 — typed, not convention)

- `LibraryItemKind::Image | Video(VideoContainer)` classified at scan; the decode scheduler
  dispatches on it **before** any `source.bytes()` request. Video metadata
  (duration?/container/codec?/audio-presence/dims/rotation/color) flows from the reader —
  never from RAM reads; unknowns allowed. Supplements `PhotoMeta.animated` rather than forcing
  video through `Option<AnimationKind>`.
- **Predicate split:** `classify_library_file(path)` (images + filesystem videos) for the
  scanner vs `is_supported_archive_image_extension` (images only) for pb-source callers.
  Tests: loose mp4/mov list as videos; identical names inside ZIP/7z are excluded; no archive
  `bytes()` is ever attempted for a video.
- **Live-Photo companion dedup:** same-stem rule kept for v1; false-positive risk (an
  unrelated `IMG_1234.mov` beside `IMG_1234.jpg` gets hidden) documented + tested across scan
  batches and recursive folders; a companion is never briefly published then removed.
  Content-identifier validation is the future refinement.

## Action matrix (video items)

| Action | Behavior |
|---|---|
| Navigate / delete / trash / reveal / copy path | Supported. Navigation or delete during playback stops the session and **releases media handles first** (Windows file locks), then acts. |
| Copy image | Copies the currently displayed poster/frame — never encoded video bytes. |
| OCR / AI describe | Operate on the displayed frame (or explicitly disabled — decide in phase 1; never a RAM read of the file). |
| Save rotation | Disabled for video. In-memory display rotation permitted **and stays live during playback** (owner 2026-07-11: real footage starts portrait and rotates to landscape mid-clip — manual rotate while playing is the fix), never persisted. Rotation is a view transform on the presented quad, so it costs nothing on the frame path. |
| Compare pin | Compares the parked poster/current frame; no hidden live playback. |
| Slideshow | Landing shows the poster; explicit play suspends slideshow advance until playback ends/stops. |
| Focus loss | Clears held-seek state like any held key; playback continues (audio owns the clock). |
| System sleep/resume | Re-anchor from audio state; never advance the clock by the sleep interval. |

## Phases (rev2 order — contracts and spikes first, GPU reuse before long playback)

0. **Contracts + platform spikes.** Define `LibraryItemKind`, `VideoMetadata`, `VideoFrame` /
   `VideoColorInfo`, session states, `AudioClockSample`, byte-budget invariants. Spike: MF
   fitted-output scaling + stream deselection + PTS + seek-forward + cancellation cost **+
   container probe sweep (does Win10/11's native MKV handler open our test MKVs? webm with/
   without Web Media Extensions?)** — the format matrix ships from these measurements; macOS
   reader-recreate cost + `prepareToPlay` timing; Linux incremental audio prototype. Lock the
   SDR guarantee against real phone footage (HLG iPhone clip). **Spike results replace the
   schedule estimate.**
1. **Item plumbing + action gating.** Predicate split; typed items; companion dedup; every
   open surface updated (scanner, file lists, drag/drop, picker filters, Windows associations,
   macOS content types, Linux MIME); the action matrix implemented/gated.
2. **Posters + metadata.** Cancellable, **low-priority** poster probing (below current still +
   immediate neighbors; capped concurrent decoder inits), non-black walk, placeholder/error
   posters for unsupported containers, metadata without whole-file reads, rotation + color
   identical to the playback path.
3. **Reusable GPU presentation path.** Texture/bind-group + staging reuse for the animation
   present path (all kinds benefit; closes the #76 follow-up). Never wait on GPU completion on
   the event loop. Measure alloc count + upload p95 before/after.
4. **Silent bounded playback.** Pure `VideoSession` state machine + PTS scheduling + preroll/
   rebuffer, on fake producers first (unit tests) then the real Windows producer.
   Duration-independent memory proven (plateau slope, not a fixed delta); VFR + EOS before
   audio exists.
5. **Audio + clock bridge.** Bidirectional events; readiness/pause/resume/mute/failure/
   interruption/no-audio; Linux incremental PCM; drift measured (software-clock relation —
   physical ≤50 ms lip-sync is a separate manual/hardware check).
6. **Seeking + contextual input.** Generation-safe latest-value seeks; decode-forward landing;
   audio ack; pan-action rewrite + self-timed hold; HUD position pill (`m:ss / m:ss`);
   paused/buffering/EOS/superseded cases; custom-keymap tests.
7. **Parity, failures, packaging, docs.** Linux then macOS matrices; codec-missing/corrupt UI;
   privacy audit (no-trace test extended over a video folder); perf corpus; CLAUDE.md (incl.
   fixing the stale keymap list) + CHANGELOG.

## Acceptance criteria (rev2 highlights)

- **Memory:** queue byte-invariant unit-tested; CPU queued bytes plateau after warm-up; no
  duration-correlated slope over 10-min (and an opt-in 2-h) run; pause accumulates nothing;
  MF unread streams deselected.
- **Responsiveness:** nothing blocking on the event loop (open/create/seek/join/GPU waits);
  navigate-away swaps the visible item within one frame; cancelled/superseded frames never
  present; bounded retired-worker count under rapid navigation; record `P → first moving
  frame` p50/p95 for H.264 + HEVC.
- **Correctness:** CFR + VFR complete at wall duration; nonzero/negative start PTS
  normalized; pause/resume/rebuffer accumulate no drift; audio failure → silent playback;
  EOS parks the true last frame; replay starts at zero.
- **Seeking:** clamped targets; keyframe + decode-forward landing within tolerance; paused
  seek stays paused; superseded seeks flash nothing; both sides re-anchored before resume.
- **Visual:** portrait/mirrored transforms; poster ≡ playback in geometry + color; BT.709
  limited, P3-SDR, HDR→SDR fixtures; steady-state 4K has no CPU Lanczos pass; window resize
  never restarts playback.
- **Library:** mixed folders list once; companions hidden; archive videos excluded;
  unsupported videos = visible placeholder + useful error (per the owner-confirmed decision);
  all open surfaces agree on extensions.

## Test practicality

Unit tests use **fake producers** (clock, queue, coalescing, state machine — no codecs, no
ffmpeg binary). A few tiny committed fixtures for integration; long/corpus/codec-extension
tests opt-in where platform codecs may be absent. The tone-blip/flash fixture validates the
*internal* audio-position↔frame-PTS relation only; physical sound-vs-photon ≤50 ms is a manual
measurement.

## Schedule (rev2)

**2-3 weeks for the Windows-first SDR milestone** (contingent on the phase-0 MF scaling and
audio-clock spikes landing quickly), with Linux/macOS parity + validation as additional,
separately-estimated work after the spikes convert guesses into measurements. The rev1
"2-3 weeks for everything" was optimistic.
