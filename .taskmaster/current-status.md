# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-11 (late). Supersedes the morning handoff (#76 shipped / #79 planned)._

## State: main, #79 phases 0 AND 1 done (Windows scope), all gates green

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

**Next: phase 2 (posters + metadata, subtask 79.3)** — cancellable low-priority poster
probing via MF (the spike's `video_probe` open+first-frame path is the blueprint:
open 4-20 ms, first frame 30-100 ms), first-non-black luma walk, placeholder/error
posters for unsupported containers, `VideoMetadata` from the reader (never RAM reads),
rotation+color identical to the future playback path. Read the spike results doc first.

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
