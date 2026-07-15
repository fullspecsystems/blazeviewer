# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-15 (rev 6). Supersedes prior status. **0.2.1 shipped.** Tasks #91
(video-playback overhaul, Phases 0–3) and #98 (media-track catalog) are COMPLETE +
owner-validated. **Subtitles (#90) now render, are selectable, and are configurable** —
see below for the map._

---

# Subtitles (#90) — where it stands

## ✅ Done + owner-tested (all on `main`, 2026-07-15)

| | |
|---|---|
| **#90.1** discovery | Sidecars (`Movie.eng.srt`) beside a file **and inside archives** (`ItemSource::sibling_*`). |
| **#90.2** cues | **Both tiers.** Sidecars parsed in pure Rust; **embedded streams** (MKV subrip/ass/webvtt, MP4 mov_text) demuxed via `avcodec_decode_subtitle2`. Plus **mojibake repair** and **ASS drawing-mode** handling. |
| **#90.3** engine | Rasterizer + placement + the macOS presenter, clocked off `video_position()`. Selection goes through the **catalog** + `resolve_track`. |
| **#90.4** settings | A **5th Settings tab** (macOS) over a **live preview**, all eight axes, owner-tuned defaults. |
| **#99** (part) | `C` toggles, **`Shift+C` cycles tracks** with a toast. |

**Owner-tuned defaults** (settled 2026-07-15): size **4%**, outline **2.00 px**, vertical
**7%** (clear of the info line + scrubber), shadow **off** — and when switched on: blur 2 px,
offset (0, 2), 80% black.

## 🔜 Remaining, in priority order

1. **The track picker — the owner's #1 ask.** A control **to the right of the total runtime**
   on the playback bar (complementing the play icon) opening a subtitle + audio track list.
   See *The three new asks* below. This is #99's popover with the placement now specified.
2. **View ▸ Subtitles flyout** (owner's #2) — direct track selection from the menu bar.
3. **`A` / `Shift+A` audio cycling** (#99). ⚠ **Audio may only toast on a *confirmed*
   switch** — subtitles may toast optimistically; the asymmetry is deliberate and documented
   in #99. A toast naming a track over dead audio trains distrust of every other toast.
4. **#90.3 remainder** — seek generations (no stale cue flash while scrubbing) and wiring
   `controls_h` so cues lift above the transport bar (`place()` supports the lift; nothing
   measures the bar yet).
5. **#90.5 — the winit presenter** (`Renderer::set_subtitle_overlay`). **Windows/Linux show
   no subtitles at all.** This is why the egui Settings tab was deferred: it would configure
   an invisible feature. `SettingsDraft::to_settings` preserves `subtitle_style` untouched
   meanwhile, so nothing rots (Windows cross-check clean).
6. **Archive'd videos have no embedded cues** — `stream_cues` needs a real path; an archive
   entry would mean decompressing the whole thing to RAM. Sidecars in archives still work.

## ⚠ Two open owner calls

- **`Automatic` falls back past the frozen forced-only rule.** Order is now
  forced+matching-audio → the container's default → anything renderable. Strict forced-only
  showed **nothing** for the commonest case (English film, full English track, no forced
  track) right after a toast saying "Subtitles on". Flagged in `SubtitleMode`'s docs —
  **confirm or overrule.**
- **Forward seek fix is UNVERIFIED end-to-end** (I can't drive the app while you test in it).
  If a forward seek shows a brief smear before settling, the pre-roll's reference frames are
  being dropped and the clock needs holding differently.

---

# The three new asks (owner, 2026-07-15) — with feasibility

## 1. A track-picker button right of the total runtime — **straightforward**

`VideoControls.swift` is a plain SwiftUI `HStack`: play · elapsed · scrubber · `videoTotal`.
A button after `videoTotal` + a SwiftUI `.popover` is the natural fit. `Icon::Sliders` already
exists (#99 suggests reusing it over vendoring a gear).

What it needs:
- **Track lists across the FFI** — indexed accessors (`subtitle_track_count()` /
  `subtitle_track_label(i)` …), *not* a `Vec<String>`, which does not cross back to Swift.
  Same pattern as the Shortcuts editor.
- **Rows reuse `pb_app_core::tracks::track_summary`** (#98). Do NOT format tracks twice.
- ⚠ **Interacting must re-arm the controls reveal**, or the bar fades out from under an open
  popover (`AppCore::flash_video_controls`).
- Off is a **real first row** for subtitles; audio has no Off. `cycle_choices` already models
  exactly this — reuse it rather than rebuilding the list.

## 2. View ▸ Subtitles flyout — **doable, and the mechanism already exists**

`MenuBar.swift` builds a native `NSMenu` and **already has a `submenu(title, children)`
helper**. The one wrinkle is that the track list is per-file while `MenuState` is a fixed
struct pushed via `SetMenuState`.

**Don't plumb a generation into MenuState.** Use **`NSMenuDelegate.menuNeedsUpdate(_:)`** —
it fires just before the menu opens, so the flyout can pull the current tracks *then*. No
push, no sync, never stale. That is the idiomatic AppKit answer and it sidesteps the whole
problem.

- Items carry the track's `local_id` in `representedObject` (the existing `fire(_:)` pattern
  uses a String id; a dedicated select-track FFI call is cleaner than string-encoding an id).
- **The submenu replaces the checkmark-toggle**, so its first row must be **Off** (radio
  semantics, checkmark on the active row). `C` keeps toggling regardless.

## 3. Audio flyout under View — **works, but the owner is right that it's a stretch**

Same machinery as #2, so it is nearly free once #2 exists. But "View" is the wrong home:
every player that does this well puts them side by side — IINA and VLC both have top-level
**Audio** and **Subtitle** menus. If #3 happens, the likely end state is a **Playback menu**
holding both, and Subtitles moves out of View with it. Worth deciding before building #2's
home, not after.

---

# The load-bearing knowledge (don't re-derive these)

## Cues STREAM — the ratio, not an optimization

Reading an embedded track is a **full linear pass over the container** (subtitle blocks are
scattered through every cluster). **Measured: 39 s** on the corpus MKV (4.4 GB over SMB) —
as a blocking wait, indistinguishable from broken. But the reader walks in **presentation
order** at ~113 MB/s while playback consumes ~1.6 MB/s — **~70× faster than playback needs**
— so it hands cues over as it finds them: **first batch at 1.06 s**, 177 batches. Cancelling
stops a read in ~1 s instead of 40 (`CueLoad::drop`), so a nav can't leave a worker chewing
20 GB.

*Known hole:* seek past the read frontier in the first ~40 s and those cues aren't there yet.
Moot once the pass completes.

*The optimization NOT to take:* the playback demuxer already reads these packets and discards
them — forwarding them would make cues free. But that exists only on routes using **our**
demuxer, and **a feature must never be gated on a backend** (see below). Layer it *under* this
reader; never replace it.

## The rules that were bought with real time

- **Prove the pipe, then build through it.** ~1500 lines / ~75 tests of pure subtitle modules
  were once merged before a single caller existed; the thin slice that followed found five
  defects in an afternoon, every one in a *seam*. It paid again: the embedded reader was run
  against a real MKV **before** anything was built on it, which is the only reason the 39 s
  cost and the in-file mojibake were design-time facts rather than owner bug reports.
- **Look at the output.** Both ASS defects (drawing-mode coordinates; 3D rips authoring every
  subtitle twice) were found by *printing real cues*. Every unit test passed. Use
  `PB_PREVIEW_OUT=<dir> cargo test -p pb-app-core --lib -- --ignored dump_preview` and the
  corpus-gated `embedded_cues_read_from_a_real_container`.
- **Never gate a feature on a backend.** Subtitles once switched off for exactly the files
  they were built for when a routing change made MKV a `Native` backend. Use
  `AppCore::video_showing()` / `video_position()`. The routing will change again.
- **The unit rule:** position and size are **viewport**-relative; decoration is **text**-relative
  (outline/shadow/radius/padding are fractions of the font size). `REFERENCE_FONT_PX` is a
  **fixed anchor at 47.5** and must not track the default size, or changing that default
  silently re-labels every user's saved settings.
- **`#[serde(default)]` on a by-value nested settings struct is load-bearing.** Without it one
  typo in `[subtitle_style]` makes the file unparseable, and `Settings::load` answers that by
  discarding **every other setting** in it.

## Verify with the corpus

- `/Volumes/Media/TV Shows/Grey's.Anatomy.S01…/` — embedded `eng subrip` (stream 2) **and** an
  `.eng.srt` of the same content: the ideal A/B. ⚠ **Its embedded track is genuinely
  mojibake'd in the file** (`ffprobe -show_data` proves it); the repair un-breaks it on the fly.
- `/Volumes/Media/Movies` — 163 more. `Ad.Astra` has Chinese ASS (stream 5, drawing mode);
  `Avatar…3D` (stream 3) is a Half-SBS rip with every subtitle authored twice;
  `Alita.Battle.Angel` is **PGS-only** → "No subtitle tracks" is **correct**, bitmap is an
  explicit non-goal.
- Small reusable ASS test files (subtitle-stream-only remuxes, ~300 KB, 8 ms instead of 39 s):
  `ffmpeg -i <film>.mkv -map 0:<sub-stream> -c copy /tmp/ass-test.mkv`, then
  `PB_TEST_SUB_MKV=/tmp/ass-test.mkv PB_TEST_SUB_STREAM=0 cargo test -p pb-decode --features ffvideo -- --ignored --nocapture embedded`
- Diagnostics: `PB_SUBTITLE_TRACE=1` (six gates: clock, placement, sidecar, cues, font system,
  bitmap), `PB_VIDEO_DIAG=1`.

## THE authoritative docs

- **Subtitles:** `.taskmaster/docs/90-presenter-and-style-contract.md` — the owner's spec, the
  frozen decisions, and **two post-mortems** (§ *As built*, § *As built, part 2*).
- **Video:** `.taskmaster/docs/video-playback-overhaul.md` (root causes R1–R12, Phases 0–3).
- **Track catalog:** `.taskmaster/docs/98-phase0-spike-findings.md`.

---

# Other open work (not subtitles)

- **Phase 2 follow-ons** (deferred, no blockers): planar rotation in geometry/UV, planar-`Vec`
  pool + two-plane single-submit upload, MF P010 on Windows.
- **#94.1 — space-pauses-a-playing-video** — deferred UX; owner wants to workshop the
  contextual-key idea first.
- Older Windows/loose ends (#80 slideshow×video, #82 macOS archive natives, #75/#76 CI/mirror).

# Notes carried forward

- **Commit + push directly to main** (owner-authorized); fetch/merge origin/main first — a
  parallel Windows agent also pushes there.
- **Windows cross-check from the Mac:** `cargo check -p pb-app --target x86_64-pc-windows-msvc`
  after two temporary manifest edits — **blake3 `pure` must be a DIRECT dep under
  `[dependencies]`** (in the Linux-only target section it does nothing, and blake3's build
  script then fails compiling C for the host) + `ureq` `default-features = false` in both
  crates. Restore them and `git checkout Cargo.lock` after. It catches every `AppCore`
  struct-literal break; it has fired every time.
- ⚠ **`cargo test --workspace` cannot build `pb-app` on macOS** (by design — it's the winit
  shell). Use `--workspace --exclude pb-app`. An `examples/` file in `pb-decode` using a
  feature-gated symbol breaks the *default* build — feature-gate it or don't add one.
- ⚠ **Always build `--features ffvideo` when testing video code:** `ActiveVideo` has a SECOND
  literal construction under `cfg(ffvideo/macos)` that `cargo test -p pb-app-core` alone
  misses. Run clippy on **both** feature sets.
- ⚠ **Don't drive the app from a tool session while the owner is testing.** `pkill` kills
  *their* window, and a tool-launched bare binary comes up **windowless** (the
  `NSTreatUnknownArgumentsAsOpen` hazard) → no pump, no tick, no trace.
- Quit the app before rebuilding (`open` won't relaunch a live app — stale-build trap).
- swift-bridge bridge module: `//` comments only (`///` panics codegen); non-FFI-able payloads
  use the stash-pull pattern. `Vec<String>` does **not** cross back to Swift — use indexed
  accessors.
- The build is quiet now: the Homebrew-FFmpeg `ld:` warnings are filtered by a pattern scoped
  to `/opt/homebrew` (a real linker warning still comes through; a broken build still exits 1
  — verified). `block v0.1.6`'s future-incompat notice is transitive through wgpu 22's Metal
  backend and is left alone.
- CLAUDE.md states platform-specific behavior as if global — verify perf/behavior against the
  cfg-gated source, not the doc.
- `settings.save()` is called **unguarded** by the older toggles (e.g. `MuteLiveAudio`), so
  dispatching them in a test writes the user's real `settings.toml`. `ToggleSubtitles` /
  `SubtitleCycle` gate on `persist_prefs` instead — copy that, and consider a cleanup pass.
- ⚠ `apply_settings` does **not** check `persist_prefs`, so a test that drives it writes the
  real config. Never test settings-apply end to end.
