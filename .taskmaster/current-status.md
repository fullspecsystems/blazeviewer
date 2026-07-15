# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-15 (rev 5). Supersedes prior status. **0.2.1 shipped.** Task #91
(video-playback overhaul, Phases 0–3) and **#98** (media-track catalog) are COMPLETE +
owner-validated. **Task #90.2 (subtitle cues) is now COMPLETE — embedded streams included.**_

## ✅ NEW THIS SESSION (2026-07-15): embedded subtitle streams, streaming cues, mojibake repair

All on `main` (`38462733`). **Not yet owner-tested — see "Test in the morning" below.**

Before this, only *sidecars* rendered: the owner's MKV showed its `.eng.srt` and not the
English track muxed inside it. The engine did its own sidecar discovery and never consulted
the track catalog, so `resolve_track` was a finished bridge with no traffic on it.

- **Embedded streams** (`pb_decode::ffmpeg::cues`) — demux + `avcodec_decode_subtitle2` for
  MKV `subrip`/`ass`/`webvtt` and MP4 `mov_text`. Proven on a real MKV: **850 cues**.
- **The engine reads the catalog** — `ensure_loaded` takes the Details probe's
  `MediaTrackCatalog` (which already merges embedded streams *and* sidecars into one id
  namespace) and asks `resolve_track`. `tick_subtitles` drives `ensure_exif_cached` itself
  rather than waiting for someone to open the Inspector.
- **Cues stream** — see the perf note below; this is the design decision worth knowing.
- **Mojibake repair** (`pb_app_core::mojibake`) — `â™ª` → `♪`, provable-only.
- **`Shift+C` cycles subtitle tracks** with a toast (#99's first slice).
- **Real-ASS handling**, both found by *printing real cues* rather than by any assertion:
  ASS **drawing mode** (`{\p1}`…`{\p0}` = vector geometry for logos/signs) was rendering as
  screenfuls of coordinates on a real Chinese track; and **Half-SBS 3D rips author every
  subtitle twice** (one per eye, identical but for the margins we ignore), which drew as
  literal double vision. Both fixed + tested.

Verified: 18/18 suites green (both feature sets), clippy clean, macOS host builds, Windows
cross-check passes.

### The perf decision worth knowing: cues STREAM

Reading an embedded track is a **full linear pass over the container** — subtitle blocks are
scattered through every cluster, so finding them means reading the film. **Measured: 39 s**
on the corpus MKV (4.4 GB over SMB). As a blocking wait that is indistinguishable from broken.

The way out is a ratio, not an optimization: the reader walks in **presentation order** at
~113 MB/s while playback consumes at ~1.6 MB/s — **~70× faster than playback needs**. So it
hands cues over as it finds them. **Measured: first batch at 1.06 s**, 177 batches; cancelling
stops a read in ~1 s instead of 40. `CueLoad::drop` cancels, so a nav can't leave a worker
chewing through 20 GB.

*Known hole:* seek past the read frontier in the first ~40 s and those cues aren't there yet.
They arrive as the reader reaches them; once the pass completes it's moot forever.

*Future optimization (do NOT make it the only path):* the playback demuxer already reads these
packets and discards them — forwarding them would make cues cost zero extra I/O. But that only
exists on routes using *our* demuxer, and **the hard-won rule of this task is that a feature
must never be gated on a backend**. Layer it under this reader, never replace it.

### ⚠ Three things the owner should know before judging what they see

1. **The Grey's Anatomy MKV's embedded track is genuinely mojibake'd — in the file.**
   Verified with `ffprobe -show_data`: the raw packet payload is `c3a2 e284 a2c2 aa` where
   `e299 aa` (`♪`) belongs. The `.eng.srt` beside it is clean. **Our reader is faithful; the
   muxer was wrong.** The new repair fixes it on the fly, so it should now look correct — but
   that's us un-breaking a broken file, not us decoding properly in the first place.
2. **`Automatic` now falls back past the frozen forced-only rule.** Flagged in
   `SubtitleMode`'s docs. Strict forced-only would show **nothing** for the common case (an
   English film, a full English track, no forced track) right after a toast saying "Subtitles
   on" — and it would have silently regressed what was validated on 2026-07-14. New order:
   forced+matching-audio → the container's default → anything renderable. **Owner call:
   confirm or overrule.**
3. **Subtitles now cost a Details probe.** With subtitles on, `tick_subtitles` triggers the
   ~20 ms container probe itself. That's new I/O on the video path — only ever with subtitles
   on and a video playing, but it's a real change.

## Test in the morning (macOS, `scripts/build-swift-host.sh`)

Corpus: `/Volumes/Media/TV Shows/Grey's.Anatomy.S01…/` has both an embedded `eng subrip`
(stream 2) **and** an `.eng.srt` of the same content — the ideal A/B. `/Volumes/Media/Movies`
has 163 more (several with forced tracks + ASS worth trying).

1. **The headline:** open an MKV with an embedded track and **no** sidecar, press `C`.
   Subtitles should appear within ~1–2 s. This never worked before.
2. **`Shift+C`** cycles: Off → track → track → Off, toasting each ("English · SubRip").
   On a Grey's episode you should be able to switch between the embedded track and the
   sidecar and see they're the same content.
3. **Mojibake:** on Grey's, the `♪` should now render as a music note, not `â™ª`.
4. **ASS tracks** (`/Volumes/Media/Movies`): `Ad.Astra…mkv` has Chinese ASS (stream 5) —
   should show Chinese text, **not** a wall of `m 211 -8 b 217 -6…` coordinates.
   `Avatar.The.Way.of.Water…3D…mkv` (stream 3) is a 3D rip whose subs are authored twice —
   should show each line **once**, not doubled.
5. **Nothing regressed:** a plain `.srt` beside an MP4 still works via `C`.
6. **Nav mid-load:** press `C` on a big MKV, then immediately arrow to the next photo.
   Should be instant — no hitch (the read cancels).
7. **`PB_SUBTITLE_TRACE=1`** prints why nothing is on screen if any of the above is blank.

**PGS-only files show nothing, correctly** — `Alita.Battle.Angel…mkv` is `hdmv_pgs_subtitle`
only, which is bitmap, an explicit #90 non-goal. `C` says "No subtitle tracks". Not a bug.

Small reusable ASS test files (subtitle-stream-only remuxes, ~300 KB, read in 8 ms instead
of 39 s) can be rebuilt with:
`ffmpeg -i <film>.mkv -map 0:<sub-stream> -c copy /tmp/ass-test.mkv`, then
`PB_TEST_SUB_MKV=/tmp/ass-test.mkv PB_TEST_SUB_STREAM=0 cargo test -p pb-decode --features ffvideo -- --ignored --nocapture embedded`

## Next (in priority order)

1. **#90.4 — the subtitle Settings UI.** Still the biggest usability gap: all eight style
   axes are implemented, clamped, and tested but reachable only from code, and the owner's
   read on the defaults is *"slightly large"*. Build with `pb-ui` components.
2. **#90.3 remainder** — seek generations (no stale cue flash while scrubbing) and wiring
   `controls_h` so cues lift above the transport bar (`place()` supports the lift; nothing
   measures the bar).
3. **#99 remainder** — `A`/`Shift+A` audio cycling, the CC button, the popover. Note the
   task's rule: **audio must toast only on a *confirmed* switch** (subtitles may toast
   optimistically — that asymmetry is deliberate).
4. **#90.5 — the winit presenter** (`Renderer::set_subtitle_overlay`). Windows/Linux still
   show nothing. Worth deferring until the style defaults settle.
5. **Archive'd videos have no embedded cues** — `stream_cues` needs a real path; an archive
   would mean decompressing the whole entry to RAM. Sidecars in archives still work.
6. **Phase 2 follow-ons** (deferred, no blockers): planar rotation in geometry/UV, planar-`Vec`
   pool + two-plane single-submit upload, MF P010 on Windows.
7. **#94.1 — space-pauses-a-playing-video** — deferred UX; owner wants to workshop the
   contextual-key idea first.

## THE authoritative docs — read these first

- **Video:** `.taskmaster/docs/video-playback-overhaul.md` (root causes R1–R12, locked rules,
  Phases 0–3). Phase 2 plan: `.taskmaster/plans/91-phase2-gpu-planar-color.md`.
- **Subtitles:** `.taskmaster/docs/90-presenter-and-style-contract.md` — the owner's spec, the
  frozen design decisions, **and the § *As built* post-mortem** (the sequencing rule + the four
  seam bugs). `.taskmaster/docs/90-p08-text-shaping-spike.md` is the shaping gate.
- **Track catalog:** `.taskmaster/docs/98-phase0-spike-findings.md`.
- Diagnostics: `PB_VIDEO_DIAG=1`, `PB_SUBTITLE_TRACE=1`.

## The lesson that keeps paying (it cost a day once)

**Prove the pipe, then build through it.** ~1500 lines / ~75 tests of pure subtitle modules
were once merged before a single caller existed; the thin slice that followed found five
defects in an afternoon that the 75 tests never could, because every one was in a *seam*.

It paid again this session: the embedded reader was run against a real MKV **before** anything
was built on it, which is the only reason the 39 s cost and the in-file mojibake were found at
design time rather than by the owner. Two of this session's best tests (the WHATWG passthrough
bytes; the streaming assertion) exist because a test failed for a *real* reason, not a typo.

**Never gate a feature on a backend.** Subtitles once silently switched off for exactly the
files they were built for when a routing change made MKV a `Native` backend. Use
`AppCore::video_showing()` / `video_position()`. The routing will change again.

## Notes carried forward

- **Commit + push directly to main** (owner-authorized); fetch/merge origin/main first — a
  parallel Windows agent also pushes there.
- **Windows cross-check from the Mac:** `cargo check -p pb-app --target x86_64-pc-windows-msvc`
  after two temporary manifest edits — **blake3 `pure` must be a DIRECT dep under
  `[dependencies]`** (putting it in the Linux-only target section does nothing, and blake3's
  build script then fails compiling C for the host) + `ureq` `default-features = false` in both
  crates. Restore them and `git checkout Cargo.lock` after. It catches every `AppCore`
  struct-literal break; it has fired every time.
- ⚠ **`cargo test --workspace` cannot build `pb-app` on macOS** (by design — it's the winit
  shell). Use `--workspace --exclude pb-app`. Also: an `examples/` file in `pb-decode` that
  uses a feature-gated symbol breaks the *default* build — feature-gate or don't add one.
- ⚠ **Always build `--features ffvideo` when testing video code:** `ActiveVideo` has a SECOND
  literal construction under `cfg(ffvideo/macos)` that `cargo test -p pb-app-core` alone misses.
  Run clippy on **both** feature sets — dead code slips through otherwise.
- ⚠ **Don't drive the app from a tool session while the owner is testing.** `pkill` kills
  *their* window, and a tool-launched bare binary comes up **windowless** (the
  `NSTreatUnknownArgumentsAsOpen` hazard) → no pump, no tick, no trace.
- Quit the app before rebuilding (`open` won't relaunch a live app — stale-build trap).
- swift-bridge bridge module: `//` comments only (`///` panics codegen); non-FFI-able payloads
  use the stash-pull pattern.
- CLAUDE.md states platform-specific behavior as if global — verify perf/behavior against the
  cfg-gated source, not the doc.
- `settings.save()` is called **unguarded** by the older toggles (e.g. `MuteLiveAudio`), so
  dispatching them in a test writes the user's real `settings.toml`. `ToggleSubtitles` /
  `SubtitleCycle` gate on `persist_prefs` instead — copy that, and consider a cleanup pass.
- Older Windows/loose-end items (#80 slideshow×video, #82 macOS archive natives, #75/#76
  CI/mirror) remain in tasks.json.
