# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-11 (late). Supersedes the morning handoff (#76 shipped / #79 planned)._

## State: main, #79 phases 0-7 done (Windows scope), all gates green

**Phase 7 (Windows scope) landed — subtask 79.8 `review`; Linux/macOS parity split to
NEW subtask 79.9 (pending; the session core + contracts are platform-neutral).**
- Slideshow suspends while a video plays, resumes at Ended (action matrix).
- Delete-while-playing: handles released FIRST, then a bounded off-loop retry
  (300 ms × 6 via `poll_delete_retry`) outlasts the ~1 s HEVC reader retirement.
- Persistent position pill: new `video_pill` renderer layer (bottom-center, above the
  toast strip), re-rasterized once per second, cleared with the session.
- Privacy: `playing_a_video_writes_nothing_to_disk` — poster + probe + playback + seek
  over a sandboxed fixture, tree byte-identical.
- Perf (opt-in test, audio-failed harness = pure video path): **P→first-frame 4K60 HEVC
  p50 183 ms / p95 320 ms; H.264 p50 53 ms**.
- **Owner-reported 4K60 stutter at full screen, fixed two ways:** (1) the audio
  `MediaPlayer` was double-decoding the picture — it now gets a `MediaPlaybackItem`
  with video tracks **deselected** (audio-only decode); (2) audio sampling is adaptive
  (30 ms while opening, 250 ms after) so the 4 Hz grid no longer quantizes preroll.
- Known ceiling (documented in CLAUDE.md): 4K60 software HEVC at full-size output is
  borderline — decode ~14.5 ms + BGRX→RGBA ~12.6 ms/frame (memory-bound, incl. MF's own
  `ConvertToContiguousBuffer` copy). Reserved escalations: `IMF2DBuffer::Lock2D` (skip
  MF's copy), NV12 + in-shader YUV (2.7× less data), hardware decode (ADR-012 gate).
- CLAUDE.md: stale v0 keymap list fixed; video architecture documented.

**Phase 6 (seeking + contextual input) landed — subtask 79.7 `review` (owner smoke:
arrows seek ±2 s / Shift ±15 s while a video plays, hold to scrub, OSD shows
`m:ss / m:ss`; zoomed-with-overflow keeps panning; paused seek updates the frame and
stays paused; P after the end replays from 0 on the same session).** Also mid-session:
the owner-reported 4K-at-⅔-speed bug was fixed (credit starvation — frame_bytes now
corrected from `Opened`'s negotiated size; budget 3×4K + frame cap 4 to cover the
in-flight credit accounting; commit 387860b).
Phase-6 mechanics: producer `SeekTo` zeroes its credit balance (flush race-free by
channel order), retires + recreates the reader positioned before first read
(spike-locked), decodes forward to ≥ target, supersedes at every stage
(never flashes), and **parks after EOS** so replay seeks work; session clamps inside
duration (MF errors past EOS), bumps generation + flushes, freezes the clock at the
target, lands via Seeking→Buffering (Buffering→Seeking now legal for held scrubs),
resumes only if it was playing, gates audio corrections until the post-seek ack sample;
`seek_by` scrubs the DESIRED target. Input: horizontal pan actions contextually seek
(no new bindings; default keymap adds Shift+Left/Right → the pan actions), self-timed
200 ms repeat, `pannable_horizontally()` keeps pan-wins-when-zoomed. HUD persistent
position pill deferred to phase-7 polish (the seek OSD toast covers feedback).

**Phase 5 (audio + clock bridge) landed — subtask 79.6 `review` (owner smoke: play a clip
WITH sound; check lip-sync by eye; mute/unmute mid-play; pause/resume keeps sync).**
- Session: `Opened` carries `has_audio`; preroll = 2 frames + audio **ready-or-absent**
  (1 s timeout degrades to silent — never a hang); `on_audio_clock` makes audio the
  master clock while both play (bounded ±50 ms corrections per ~4 Hz sample, hard
  re-anchor past 500 ms); audio `Failed` → permanent silent fallback. 14 session tests.
- Shell: `pb-app/src/video_audio.rs` — WinRT `MediaPlayer` over the video file, opened
  **paused** (loads in parallel with the frame preroll; core resumes both together via
  `ResumeVideoAudio` on the Buffering→Playing transition); ~4 Hz sampling into
  `AppCore::video_audio_clock`; mute-in-place keeps the clock running (sync is
  mute-independent); creation failure reports one `Failed` sample.
- Effects: `Start/Stop/Pause/ResumeVideoAudio` + `SetVideoAudioMuted`; the session's
  state changes drive them (freeze together on rebuffer, resume together on Playing);
  the existing Mute toggle now also mutes/unmutes video audio in place.
- New committed fixture `color_with_tone.mp4` (H.264 + 440 Hz AAC) asserts `has_audio`
  detection. Linux PCM / macOS audio player = phase 7 parity.

**Phase 4 (silent bounded playback) landed — subtask 79.5 `review` (owner smoke: open a
video, press P; pause/resume on P; replay after end; navigate away mid-play).**
- `pb-app-core/src/video_session.rs`: the `VideoSession` — injected-time state machine
  (preroll 2, rebuffer-don't-drift with a frozen clock + hard re-anchor, EOS parks the
  last frame, VFR, stale-generation discard), demand-driven credit queue enforcing the
  `VideoQueueBudget` invariant (credits count as in-flight bytes). 11 fake-producer unit
  tests + a REAL end-to-end (MF producer over the committed fixture plays to `Ended` at
  wall duration with the invariant asserted every poll).
- Protocol (pb-decode::video): `VideoProducerEvent` {Opened, Frame, EndOfStream, Failed} +
  **one merged msg channel** (`Credit`|`Stop`) — the producer's only blocking point is
  `recv()`, so Stop can never be deafened by backpressure (no crossbeam needed).
- `pb-decode/src/mf_video_producer.rs`: the Windows producer thread — mf_poster's reader
  config (fitted RGB32, advanced processing, streams deselected), first-PTS
  normalization, mid-stream stride re-query, off-thread reader retirement. 3 integration
  tests incl. stop-while-credit-starved.
- AppCore: P → `video_play_pause` (start/pause/resume/replay), `poll_video` in `tick`
  presents through the phase-3 reusable `set_image` path, `stop_video` rides
  `stop_playback` (navigation/delete/new source), `work_pending` keeps the loop ticking.
- ⚠ Known limitation (phase-7 polish): deleting the *currently playing* video can fail
  while its reader retires (~1 s HEVC) — needs retry-after-retirement.

**Phase 3 (reusable GPU presentation path) landed — subtask 79.4 done (closes the #76 perf
follow-up).** `set_image` (the per-frame animation/video present path) now runs through a
`ReuseSlot` in pb-render: same-geometry frames upload into the existing texture and rewrite
the color uniform in place (`ReuseOutcome::Reused` keeps the renderer's bind group — wgpu 22
resources aren't Clone); rebuild only on item/resize/HDR change. `StagingUpload` recycles
its staging buffers via background `map_async` re-map (bounded pool of 3, opportunistic,
never waits on the GPU — a miss allocates fresh). Measured (`present_path_churn` --ignored
test): 1080p p95 1.35→0.42 ms (−69 %); steady-state per-frame creations 5 wgpu resources +
one multi-MB staging alloc → 0. Reuse-identity + staging-recycle round-trip tests added.

**Phase 2 (posters + metadata) landed — subtask 79.3 `review`.** Videos now show their
first non-black frame as a poster (MF reader, playback-identical rotation/color config),
a play badge (`play_hint_kind` 2; P shows an honest "not available yet" toast and never
enters the animation machinery), and panel rows (Duration / Video codec / Frame rate /
Audio) from a ~22 ms reader probe cached per item. Key pieces:
`pb-decode/src/mf_poster.rs` (probe_video_stream + decode_video_poster, mean-luma walk
≤1 s/30 frames with last-frame fallback, bounded off-thread reader retirement — HEVC drop
~1 s), pure luma helpers + committed 2.4 KB H.264 black-lead-in fixture, the pool's
per-job cancel now threaded into `DecodeFn`/`decode_item_cancellable` (mid-walk cancel),
graceful placeholder fallback when MF can't open/decode. Real-clip verify: 4K HEVC poster
~210 ms, probe ~22 ms (incl. the 5.9 GB clip); opt-in corpus test `PB_VIDEO_POSTER_CLIP`.
macOS/Linux poster backends = phase 7 parity (placeholder tile there meanwhile).

**Phase 1 (typed items + predicate split + action gating) landed after phase 0 — subtask
79.2 is `review` (awaiting owner smoke).** What it does, end to end: folders now list
videos (MP4/MOV/MKV/WebM/AVI/WMV/MPEG/AVCHD/3GP) as items showing a 320×180 dark
placeholder tile; Live-Photo companion `.mov`s are hidden by same-stem-per-directory
dedup (streaming-safe: batches append-only, a companion is never published; opening the
companion itself via Cursor::At keeps it visible); `decode_item` dispatches
`LibraryItemKind` **before** any `bytes()` (a video's encoded bytes never enter RAM —
no-trace test now covers a video folder); save-rotation shows a video toast, the EXIF
panel stat-only for videos, video items never Live-pair; picker filter, Windows
`PhotoBlaze.Video` Open-With candidacy (never default), Linux MimeType, macOS
CFBundleDocumentTypes all updated; CHANGELOG has the user-facing entry. Playback-dependent
matrix rows (delete-releases-handles-first, slideshow suspend) land with phases 4-6.
Key files: `pb-app-core/src/video.rs` (classify/item_kind/companion helpers),
`scan.rs` (`dedup_companions` + `CompanionFilter` + wiring), `engine.rs`
(`video_placeholder` + dispatch), `default_app.rs`, `main.rs`, release-linux.sh,
Info-swift-host.plist.

**Owner verdict on 4K60 full-screen: still not smooth after the double-decode fix —
the ADR-012 gate has tripped. NEW subtask 79.10: GPU-accelerated decode** (spike ladder
in the subtask: NV12 + in-shader YUV → MF hardware decode via D3D manager → full
zero-copy; measure each rung; also try `IMF2DBuffer::Lock2D` on the current path).

**Position pill replaced per owner design (2026-07-11):** the info line (`i`) now grows
a **playback row** while a video plays —
`filename · W×H · codec` / `0:42 ▰▰▰▱▱▱ 9:01` — via `hud::render_panel_progress`.
Width = max(summary row, bar minimum) so it's constant for a clip (no 1 Hz jitter);
re-rastered only when the displayed second changes (`update_video_progress`); one block,
one `i` toggle = the show/hide. The `video_pill` renderer layer was removed.

**Next:** 79.10 (GPU decode — the performance unblock) then 79.9 (Linux/macOS parity).

`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, the featured
clippy (`libheif,dav1d`), and `fmt --check` all pass.

## What landed this session (#79 video playback, phase 0)

- **Contracts (TDD, 23 unit tests):**
  - `crates/pb-decode/src/video.rs` — `VideoFrame` (session_id + seek_generation + pts +
    PixelFormat + `VideoColorInfo`), `VideoSessionId`, `SeekGeneration`.
  - `crates/pb-app-core/src/video.rs` — `LibraryItemKind`/`VideoContainer` (the one
    cross-platform recognition list; provably disjoint from the image predicate),
    `VideoMetadata`, `VideoSessionState` + full legal-transition relation,
    `AudioClockState`/`AudioClockSample`, `VideoQueueBudget` (byte-budget admission with the
    one-frame exception; invariant property-tested).
- **All Windows MF spikes measured** via the new dev harness
  `crates/pb-decode/examples/video_probe.rs` (`spike` + `sweep` subcommands).
  **Results doc (read before phase 1): `.taskmaster/docs/79-phase0-spike-results.md`.**
  Headlines:
  - No blockers; **Windows milestone estimate stands (2–3 wk)**.
  - **Seek = recreate the reader** (open 4–20 ms, position-before-first-read 0.2 ms, 4K HEVC
    landing ~350 ms, H.264 ~55 ms). `SetCurrentPosition` on a *warm* reader blocks ~1 s on
    HEVC (Store MFT) — codec-specific, as is the ~1 s reader-drop (H.264: 43 ms) →
    retirement pool confirmed.
  - Fitted-output scaling (`MF_MT_FRAME_SIZE` on the RGB32 out type) **is honored**; cost ≈
    neutral vs native+copy but kills the per-frame Lanczos and shrinks queue bytes.
  - 4K60 HEVC ≈ 14.5 ms/frame sustained (4K30 comfortable); the BGRX→RGBA copy (13 ms @4K)
    is the hot consumer-side cost — keep it on the producer thread.
  - Container sweep on this (fully codec-extension-equipped) box: **everything** decodes —
    mkv H.264+HEVC (native MKV handler is real), webm VP8/VP9, avi, wmv, mpg (MPEG-2), mts,
    3gp, AV1. Open-vs-codec failures are distinguishable hresults → precise error UI.
  - HLG 10-bit BT.2020 negotiates RGB32 → plausible SDR (eyeballed vs SDR reference).
    Seek-past-EOS *errors* (0xC00D36E5), doesn't clamp → session must clamp first.
    Nonzero start PTS is real (MPEG-TS: 766.67 ms) → PTS normalization required.
- pb-decode windows dep grew `Win32_System_Com_StructuredStorage` + `Win32_System_Variant`
  (PROPVARIANT for duration/seek).
- Plan doc updated: in-memory rotation **stays live during video playback** (owner: real
  clips rotate portrait→landscape mid-recording), never persisted.
- ffmpeg-generated container/HLG/rotation fixtures live in the session scratchpad only
  (2 s testsrc2 clips; regen commands in the spike doc) — nothing committed to the corpus.

## Next: #79 phase 1 (typed items, predicate split, action gating) — subtask 79.2

Per the plan (`.taskmaster/plans/79-video-playback-tier2.md`, the spec): split
`classify_library_file` (scanner: images + filesystem videos) from the images-only predicate
pb-source callers get; dispatch `LibraryItemKind` **before** any `source.bytes()`
(`engine.rs:229` reads unconditionally today); Live-Photo companion dedup (same-stem, tested
across scan batches); update every open surface; implement/gate the action matrix. The
contracts it needs are already in `pb-app-core::video`.

## Phase 0 remainder (deferred, not Windows-blocking)

- macOS: AVAssetReader-recreate + `prepareToPlay()` timing spikes (needs the Mac).
- Linux: incremental-PCM audio prototype.
- Clean-VM (no Store extensions) sweep for the out-of-box format-matrix row.
- Real HLG/Dolby-Vision phone clip for the corpus (library's 2015–2021 iPhone footage is
  all SDR BT.709).

## Loose ends carried forward

1. **#78 CLI owner smoke** → flip to done (+ CHANGELOG entry if missing).
2. **#76 ARM64 mirror** (setup-libheif on the ARM64 box; check dav1d ARM asm in vcpkg log).
3. **#77** LGPL note: patent-exposure paragraph appended? (was pending this morning).
4. CLAUDE.md stale v0 keymap list — fix in #79 phase 7 docs.

## Environment / conventions quick-ref

- Spike harness: `cargo run --release -p pb-decode --example video_probe -- spike <file>
  [--fit W H] [--frames N] [--dump <dir>]` / `-- sweep <files...>`.
- Real 4K HEVC test clips: `D:\Media\Pictures\2019\2019-08-01 - Morinville\IMG_0060.MOV`
  (93 s), `...\2019-12-27 - Nanaimo\IMG_1281.MOV` (5.9 GB). VFR:
  `...\2019-07-16\RPReplay_Final1563339583.mp4`. Real MKV/MPG under `D:\Media\Music Videos`.
- tasks.json edits: PowerShell ConvertFrom/To-Json round-trip; IDs stay numeric.
- Commits: no AI-attribution trailers.
