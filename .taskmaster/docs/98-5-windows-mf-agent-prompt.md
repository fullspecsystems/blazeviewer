# Prompt: finish Media Foundation track enumeration (PhotoBlaze task 98.5)

> Hand this whole file to a Claude Code agent on the Windows box. It is written to be
> self-contained.

---

Implement **Media Foundation audio/subtitle track enumeration** for PhotoBlaze — subtask
**98.5** of task **#98** (video Details: real audio + subtitle track listings).

## Get the branch

```powershell
git fetch origin
git checkout feat/media-track-catalog   # 5 commits ahead of main; already merged with main
cargo test -p pb-decode -p pb-app-core  # baseline: should be green before you touch anything
```

## What already exists (do not rebuild any of it)

Task #98 built a **platform-neutral media-track catalog**. Two of three backends are done
and shipping; yours is the last one.

- **`crates/pb-decode/src/tracks.rs`** — the model + **pure maps**. Read this first; it is
  the contract you fill in. `MediaTrackCatalog { generation, backend, audio: TrackSet,
  subtitles: TrackSet }`, `TrackSet { completeness, total, tracks }`,
  `TrackCompleteness { Complete, CountOnly, Partial, Unavailable }`, `MediaTrack`,
  `TrackId`, `TrackLocator` (already has an `MfStream(u32)` variant for you).
- **`crates/pb-decode/src/ffmpeg/tracks.rs`** — the **reference backend**. Read it: it is
  the shape yours should take.
- **`crates/pb-decode/src/livephoto.rs`** (`catalog_from_asset`) — the AVFoundation backend.
- **`crates/pb-app-core/src/tracks.rs`** — `track_summary` / `track_rows`. **Already done —
  do not touch.** Produce a correct catalog and the Details rows render themselves.

**Reuse the pure maps — do not write your own codec/language logic:**

```rust
pb_decode::tracks::audio_codec_display(codec_raw: &str, profile: Option<i32>) -> String
pb_decode::tracks::subtitle_codec_display(codec_raw: &str) -> String
pb_decode::tracks::subtitle_capability(codec_raw: &str) -> TrackCapability
pb_decode::tracks::normalize_lang(tag: &str) -> Option<String>   // "und"/"mis"/"zxx" -> None
```

They are keyed on **FFmpeg's `avcodec_get_name` vocabulary** (`"aac"`, `"ac3"`, `"eac3"`,
`"dts"`, `"truehd"`, `"pcm_s16le"`, `"subrip"`, `"hdmv_pgs_subtitle"`, …). Your job is to map
an **MF subtype GUID → that vocabulary**, exactly as `livephoto::fourcc_to_codec_raw` maps a
CoreMedia FourCC into it. One codec map serves every backend; that is the point.

Pass `None` for `profile` unless you are certain the codec is DTS — the constants collide
across codecs (`AV_PROFILE_DTS_ES == AV_PROFILE_TRUEHD_ATMOS == AV_PROFILE_EAC3_DDP_ATMOS ==
30`), and the map is keyed per-codec for that reason.

## Your target

**`crates/pb-decode/src/mf_poster.rs`** → `probe_video_details_input(input, generation)`.

It currently returns a real `VideoStreamInfo` plus
`MediaTrackCatalog::unavailable(generation, MediaBackend::MediaFoundation)`. That is
deliberate and **honest, not a stub-that-lies**: the Details rows render `Unavailable` +
`has_audio: true` as *"Audio: Present — details unavailable"* — never as "No audio". Read
its doc comment; it carries the finishing spec.

Replace the `unavailable(...)` with a real enumeration.

### Useful things already in the file / crate

- `open_video_reader(input) -> IMFSourceReader` — opens a path **or in-RAM archive bytes**
  (`VideoInput::Bytes`, via `mf_stream::mem_istream`). Works for both; archive videos route
  here too (task 98.7).
- `retire_reader(reader)` — the required teardown (MFT shutdown off-thread). Use it.
- `stream_info(&reader)` — the existing video-facts read; `probe_video_details_input`
  already calls it via `probe_video_input`.
- `codec_name(sub: &GUID) -> &'static str` — the **video** subtype map. Model your audio /
  subtitle GUID map on it.
- **`crates/pb-decode/src/mf_audio.rs` already reads `MF_MT_AUDIO_NUM_CHANNELS` and
  `MF_MT_AUDIO_SAMPLES_PER_SECOND` via `GetUINT32`** — copy that pattern rather than
  rediscovering it.

## The open questions (this is why the task is Windows-only)

Phase 0 spiked FFmpeg and AVFoundation on a Mac and **corrected the design four times**
(see `.taskmaster/docs/98-phase0-spike-findings.md` — read it, the lessons transfer). The MF
questions could not be answered from macOS, and the code there cannot even be compile-run
against a real reader. **Spike these before writing the backend:**

1. **Does the `GetNativeMediaType(i, 0)` loop actually terminate on
   `MF_E_INVALIDSTREAMNUMBER`?** Do not assume. Prove the exact `HRESULT` and stop condition.
2. **Where do language and title actually live?** ⚠ **Do not assume `MF_SD_LANGUAGE` is on
   `IMFMediaType`.** It is a *stream descriptor* attribute — you may need the presentation
   descriptor (`IMFMediaSource::CreatePresentationDescriptor` →
   `GetStreamDescriptorByIndex` → `IMFStreamDescriptor` attributes), which means opening the
   source rather than only the reader. Find out what is really there, and **report what you
   find** rather than shipping a guess. Note `MF_SOURCE_READER_MEDIASOURCE` is already
   imported in this file — the reader can hand you the underlying source.
3. **Do subtitle streams appear at all?** MF's subtitle exposure is limited. If MKV SubRip
   tracks simply do not enumerate, that is a **real finding**: report `CountOnly` or
   `Unavailable` for subtitles honestly. Do **not** invent them, and do **not** report
   `Complete` for a set you could not really enumerate.

## The contract you must not break

- **An empty vector cannot mean "none".** Only `TrackCompleteness::Complete` with
  `total == Some(0)` may render as "No". If you enumerate audio but not subtitles, that is
  `audio: Complete` + `subtitles: Unavailable` — **not** `subtitles: complete(vec![])`.
  Turning an enumeration limit into "No subtitles" is a confident lie about the user's file.
- **Backends are allowed to disagree.** The catalog names its `MediaBackend` precisely
  because AVFoundation synthesizes options FFmpeg doesn't. If MF sees the file differently,
  say so via completeness — don't force it to match.
- **Identity**: `local_id` should be the real MF stream ordinal, and register
  `TrackLocator::MfStream(ordinal)` via `catalog.set_locator(local_id, ..)`, mirroring
  `ffmpeg/tracks.rs`. Every `TrackId` carries `generation`.
- **Never on the poster path.** `probe_video_stream` / `probe_video_input` feed poster
  decoding, which runs for **every prefetched video** whether the Inspector opens or not.
  Keep your enumeration in `probe_video_details_input` only. (It's already off the event
  loop — `pb_app_core::media_details` runs it on a worker.)
- **Read-only, RAM-only** (privacy #2): no temp files, no extraction, ever.

## Tests

- Fixtures exist: **`crates/pb-decode/tests/fixtures/video/multitrack.mkv`** (2 audio: AAC
  stereo `eng` *default* + AC-3 5.1 `fra` "Director's Commentary" *comment*; 4 subtitles:
  SubRip `eng` *default*, SubRip `eng` *forced+SDH*, a **bitmap PGS** `spa`, SubRip `jpn`
  "日本語字幕") and **`multitrack.mp4`**. Both are tiny and committed. Regen block is in
  the fixtures README.
  ⚠ MKV/HEVC may need Windows codec extensions — if MF can't open the MKV, use
  `multitrack.mp4` / `color_with_tone.mp4` / `tone_51.mp4` (H.264+AAC, in-box) and record
  the MKV limitation.
- Follow `ffmpeg/tracks.rs`'s test style. Assert what MF **actually** reports, not what you
  wish it did — those tests are the record of real backend behavior.
- `black_then_color.mp4` is silent: it must come back `Complete` + `total: Some(0)` →
  "Audio: No".

## Verify

```powershell
cargo test -p pb-decode -p pb-app-core
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

Then **smoke it in the real app** (this is the point of doing it on Windows): open a
multi-track video, press `Shift+I`, confirm the Details tab lists real tracks, and confirm
"Copy Image Details" includes them. Also open a video **inside a ZIP** — archive videos
route through the same `probe_video_details_input`.

## When done

- Update `.taskmaster/tasks/tasks.json`: task #98 subtask 5 → `done` (and #98 itself → `done`
  if 5 was the last one — check the others first).
- `CHANGELOG.md` `## [Unreleased] → Changed`: the video-Details entry currently ends with
  *"Windows currently reports 'Present — details unavailable' for audio, pending its own
  track enumeration."* — **remove that caveat** if you close it, or amend it to what's
  actually true.
- Record what you learned about questions 1–3 in
  `.taskmaster/docs/98-phase0-spike-findings.md` (it has a "Media Foundation — NOT RUN"
  section waiting for exactly this).
- Commit to `feat/media-track-catalog` and push.

## Cross-checking macOS from Windows

You can't build the macOS shell, but `pb-decode`/`pb-app-core` are shared. Nothing here
should touch non-Windows code — if you find yourself editing `livephoto.rs`, `tracks.rs`, or
`pb-app-core/src/tracks.rs`, stop and reconsider: those are done and shared.

## Honesty bar

If a question can't be answered — e.g. MF genuinely won't surface subtitle languages —
**say so and encode it in the completeness**, rather than making the panel confident. The
whole design exists so the app can admit what it doesn't know. A truthful
`CountOnly` beats a fabricated `Complete`.
