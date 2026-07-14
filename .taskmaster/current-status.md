# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-07-14 (rev 4). Supersedes prior status. **0.2.0 shipped** (private beta).
**Task #91 (video-playback overhaul, Phases 0–3) is COMPLETE + owner-validated.**
**Task #98 (media-track catalog) is COMPLETE.** **Task #90 (subtitles) has a working end-to-end
slice on macOS** — see below._

## ✅ NEW THIS SESSION: subtitles render, end to end (task #90)

**The milestone: the pipe is proven.** A real `.srt` beside a real MKV goes discovery → parse →
shape → rasterize → screen, clocked off the playhead, toggled with `C`, and the choice persists.
Owner-verified on the physical machine. Everything remaining is filling in behind a pipe that is
known to work.

Branch `feat/subtitle-display` (merged to `main` this session). Owner's read on the look:
*"Subtitles look good, slightly large, but this may be a fine default."*

**What shipped:**
- **Sidecar discovery** (#90.1) — `Movie.eng.srt` / `.vtt` beside `Movie.mkv`, pure matching over a
  sibling list. `PhotoSource` gained `sibling_names`/`sibling_bytes` so **archives work too** (a
  `.srt` in a ZIP was previously unreachable — `bytes(i)` is index-only and sidecars aren't indexed).
- **Cues** (#90.2, *sidecars only*) — SubRip + WebVTT → timed plain text, overlaps kept.
- **Style / placement / rasterizer** (#90.3/.4 core) — the owner's eight axes, one cosmic-text
  rasterizer feeding both shells (`pb-hud::subtitle`), physical-px discipline throughout.
- **The engine + macOS presenter** — `pb-app-core::subtitle_engine` joins the parts; the bitmap
  crosses the FFI on a generation (the `thumb_rgba` contract) into a SwiftUI overlay on the canvas.
- **The switch** — `Action::ToggleSubtitles`, bare `C`, View ▸ Subtitles in both shells, a toast,
  and a persisted `settings.subtitles`.

**Diagnostics:** `PB_SUBTITLE_TRACE=1` prints *why* nothing is on screen (six gates: clock,
placement, sidecar, cues, font system, bitmap — a silent failure at any one looks identical from
outside). It is a diagnostic only; it never turns subtitles on.

### ⚠ The lesson worth carrying (it cost a day)

Four pure modules (~1500 lines, ~75 passing tests) were built and merged **before a single caller
existed** — the owner twice tried to test playback that did not exist. The thin slice that followed
(one real cue on screen) found **five defects in an afternoon that the 75 tests never could**,
because every one was in a *seam*: a tick that skipped the hide, a preference not applied at launch,
a 52 pt SwiftUI safe-area mismatch, ragged-left multi-line cues, and a feature gated on a *backend*
that a routing change flipped. **Prove the pipe, then build through it.** Full write-up (with the
rules each bug produced) in `.taskmaster/docs/90-presenter-and-style-contract.md` § *As built*.

**The one that will bite again:** subtitles gated on `video_session_active()`, so when Phase 3F made
the sample-buffer presenter the default MKV route (a `Native` backend), subtitles silently switched
off for exactly the files they were built for. **Never gate a feature on a backend** — use
`AppCore::video_showing()` / `video_position()`, which answer for whichever route is live. The
routing will change again.

## ✅ DONE: Phase 3 — macOS sample-buffer presenter (task #91 complete)

The DoVi/HDR end-state for containers `AVPlayer` can't demux (MKV/WebM): FFmpeg (Rust) demuxes →
Swift wraps compressed packets into `CMSampleBuffer`s → `AVSampleBufferDisplayLayer` (system decode
+ **correct Dolby Vision**), audio + video on one `AVSampleBufferRenderSynchronizer`. **Default route
for loose-file MKV/WebM on macOS** (`68b7fd0f`); `PB_NO_SAMPLE_BUFFER=1` forces the old Session route,
and the presenter self-probes the codec + falls back to Session for anything it can't sample-decode.
Built test-first, owner-validated on the physical Neo G9/GN95C:

- **A — Rust demux-only packet source** (`68efdb8f`) `VideoDemuxer` in `pb-decode`: hvcC/avcC extradata
  + NAL length + **DoVi config (dvvC box)** + compressed packets, no decoder.
- **B — FFI bridge + routing** (`4799353c`) `PlaySampleBuffer` effect; reuses the `Native` proxy +
  `native_video_*` callbacks.
- **C — Swift `SampleBufferPresenter` + `DemuxReader`** (`d818388a`) **0C gate PASSED** — owner
  confirmed DoVi/HDR renders correctly.
- **D — synchronized audio** (`9de1bb3d`) `AudioSampleFeeder` → `AVSampleBufferAudioRenderer` on the
  SAME synchronizer (one clock). Stereo downmix, honest.
- **E — generation-safe seek + frame-step + default routing** (`9de1bb3d`, `68b7fd0f`).

**Remaining (deferred, not blockers):** archive-bytes (ZIP/7z) videos fall back to Session; WMV/MPEG/
AVCHD stay on Session; audio capped at stereo. Phase 2 follow-ons still deferred (planar rotation-in-
geometry, planar-Vec pool + two-plane single-submit upload, MF P010 on Windows).

## ✅ DONE: task #98 — media-track catalog + Details listings

All four backends (FFmpeg reference, AVFoundation, **Media Foundation** — finished by the Windows
agent) + the off-thread generation-safe probe + archive parity + the shared formatter. Inspector
Details (`Shift+I`) lists every audio/subtitle track (language, codec, channels, default/forced/
commentary/SDH). `Audio: Yes` is retired. **An empty vector cannot represent degradation** — only
`Complete + total Some(0)` may render "No"; anything else says so honestly.

## THE authoritative docs — read these first

- **Video:** `.taskmaster/docs/video-playback-overhaul.md` (root causes R1–R12, locked rules,
  Phases 0–3). Phase 2 plan: `.taskmaster/plans/91-phase2-gpu-planar-color.md`.
- **Subtitles:** `.taskmaster/docs/90-presenter-and-style-contract.md` — the owner's spec, the
  frozen design decisions, **and the § *As built* post-mortem** (the sequencing rule + the four
  seam bugs). `.taskmaster/docs/90-p08-text-shaping-spike.md` is the shaping gate.
- **Track catalog:** `.taskmaster/docs/98-phase0-spike-findings.md` — the four design corrections
  the spikes produced. Read before #90/#99 work.
- Diagnostics: `PB_VIDEO_DIAG=1`, `PB_SUBTITLE_TRACE=1`.

## Next (in priority order)

1. **#90.4 — the subtitle Settings UI.** The natural next slice: all eight axes are implemented,
   clamped, and tested but reachable only from code, and the owner already has a read on the
   defaults (*"slightly large"*). This is what turns the proof of concept into something usable
   daily. Build it with the `pb-ui` components (never hand-roll a control in a dialog).
2. **#90.2 — embedded subtitle streams.** The biggest *functional* gap: in-container SubRip /
   mov_text needs a demuxer read. Only sidecars render today, so the owner's MKV shows its `.eng.srt`
   but not its embedded English track. Chunkier than the Settings UI.
3. **#90.3 remainder** — seek generations (no stale cue flash while scrubbing) and wiring
   `controls_h` so cues lift above the transport bar (`place()` already supports the lift; nothing
   measures the bar).
4. **#99 — the track picker** (`A` cycling + popover). Also what makes `Automatic` mean the
   specified forced-only + matching-audio-language rule; today it shows the first renderable
   sidecar. `resolve_track` is already written against the catalog and unit-tested — it just has no
   catalog to select from yet.
5. **#90.5 — the winit presenter** (`Renderer::set_subtitle_overlay`). Windows/Linux show nothing.
   Worth deferring until the style defaults settle — it's a second implementation of decisions still
   in flux.
6. **Phase 2 follow-ons** (deferred, none blockers): planar rotation in geometry/UV, planar-`Vec`
   pool + two-plane single-submit upload, MF P010 on Windows.
7. **#94.1 — space-pauses-a-playing-video** — deferred UX; owner wants to workshop the
   contextual-key idea first.

## Owner verification still worth a look

- **Subtitle defaults** — size/outline/offset are the author's guess. Owner: *"slightly large… I'll
  scrutinize it more as I spend more time with it."* Worth settling before #90.4's UI is built
  around them.
- **HDR look** on the real 32:9 EDR panel (Phase 2) — verified against a from-spec golden, but a
  physical-panel gut-check on P010/PQ tone-mapping is the last unautomatable check.
  `PB_VIDEO_NO_PLANAR=1` reverts instantly.

## Build / test the macOS app

- `scripts/build-swift-host.sh` (defaults `--ffvideo` **ON**; `--no-ffvideo` to opt out) →
  `target/swift-host/release/PhotoBlaze.app`. `--bundle-ffmpeg` = self-contained (release-style).
- ⚠ **Always build `--features ffvideo` when testing video code:** `ActiveVideo` has a SECOND
  literal construction under `cfg(ffvideo/macos)` that `cargo test -p pb-app-core` alone misses.
  Run `cargo clippy -D warnings` on **both** feature sets — dead code slips through otherwise.
- ⚠ **Don't drive the app from a tool session while the owner is testing.** `pkill PhotoBlaze`
  kills *their* window, and a bare-path launch can hand the file to their instance. A tool-launched
  bare binary also comes up **windowless** (the `NSTreatUnknownArgumentsAsOpen` hazard) → no pump,
  no tick, no trace. Ask the owner to run it instead; that is genuinely faster.
- Quit the app before rebuilding (`open` won't relaunch a live app — stale-build trap).
- Perf corpus on the SMB share `/Volumes/Media/`. Real subtitle corpus:
  `/Volumes/Media/TV Shows/Grey's.Anatomy.S01…/` (embedded eng SubRip **and** an `.eng.srt` of the
  same content — which is how the "two identical rows" defect was caught; sidecar tracks are marked
  `external: true`).

## Notes carried forward

- **Commit + push directly to main** (owner-authorized); fetch/merge origin/main first — a parallel
  Windows agent also pushes there (`feat/media-track-catalog` is their branch; re-merge if it advances).
- **Windows cross-check from the Mac:** `cargo check -p pb-app --target x86_64-pc-windows-msvc` after
  two temporary manifest edits (blake3 `pure` as a **direct** pb-app dep + `ureq` `default-features =
  false` in both crates); restore them and `git checkout Cargo.lock` after. It catches every
  `AppCore` struct-literal break in `crates/pb-app/src/main.rs` — it has fired every time.
- swift-bridge bridge module: `//` comments only (`///` panics codegen); non-FFI-able payloads use
  the stash-pull pattern.
- CLAUDE.md states platform-specific behavior as if global — verify perf/behavior against the
  cfg-gated source, not the doc.
- Planar path gotcha: `use ff::format::Pixel::*` brings `Pixel::None` into scope — qualify `Option::None`.
- `settings.save()` is called **unguarded** by the older toggles (e.g. `MuteLiveAudio`), so
  dispatching them in a test writes the user's real `settings.toml`. `ToggleSubtitles` gates on
  `persist_prefs` instead — copy that, and consider a cleanup pass on the older ones.
- Older Windows/loose-end items (#80 slideshow×video, #82 macOS archive natives, #75/#76 CI/mirror)
  remain in tasks.json.
