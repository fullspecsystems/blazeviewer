# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-16 (rev 7). Supersedes rev 6. **Subtitles now display and are
configurable on Windows** (#90.5 + #90.4), owner-confirmed on screen. Rev 6 listed the
macOS track picker and View flyout as "remaining" — both shipped; that section was stale._

---

# Subtitles (#90) — where it stands

## ✅ Done + owner-tested

| | |
|---|---|
| **#90.1** discovery | Sidecars (`Movie.eng.srt`) beside a file **and inside archives**. |
| **#90.2** cues | **Both tiers.** Sidecars in pure Rust; **embedded streams** (MKV subrip/ass/webvtt, MP4 mov_text) via `avcodec_decode_subtitle2`. Plus mojibake repair + ASS drawing-mode. |
| **#90.3** engine | Rasterizer + placement + the macOS presenter, clocked off `video_position()`. |
| **#90.4** settings | **Both shells now.** The macOS 5th tab (SubtitlesPane.swift) and the **egui tab** (`dialog.rs`), both over the live preview. |
| **#90.5** presenter | **Windows/Linux draw subtitles.** `Renderer::set_subtitle_overlay` + `App::present_subtitles`. Owner-confirmed on a real film 2026-07-16. |
| **#99** (mac) | Playback-bar picker popover, the Subtitles flyout (`menuNeedsUpdate`), `C` / `Shift+C`. |

**Owner-tuned defaults** (2026-07-15): size **4%**, outline **2.00 px**, vertical **7%**,
shadow **off** — and when switched on: blur 2 px, offset (0, 2), 80% black.

## 🔜 Remaining — and it is all #99, not #90

1. **The winit track-picker UI.** The core is ready and shell-agnostic
   (`subtitle_picker_rows` / `select_subtitle_row` / `subtitle_tracks_known`, including the
   tri-state). Neither home is a port, though:
   - The winit "playback bar" is a **hand-laid-out egui info pill** (`panels_ui.rs`), not a
     transport `HStack` like `VideoControls.swift` — a popover there means manual layout,
     hit-testing, and joining the `video_bar_interactive` pointer gate.
   - A **menu flyout needs machinery muda lacks.** macOS pulls the per-file tracks in
     `NSMenuDelegate.menuNeedsUpdate` as the menu opens; muda builds the tree **once** and
     only mirrors checkmarks onto it. Rebuilding a submenu needs dynamic ids + a new effect,
     or the track list plumbed through `MenuState` (which the mac doc argues against).
   - `Shift+C` **and the new View ▸ Next Subtitle Track** cover the function meanwhile.
2. **Windows audio-track switching — blocked on design, not effort.** Three things, and the
   first is the one nobody has answered:
   - **A currency mismatch.** The catalog hands out `TrackLocator::FfStream(i)` — an
     **FFmpeg** container stream index, since #100 made FFmpeg the demuxer — while MF's
     Source Reader enumerates its **own** indices. They are not interchangeable. This is
     exactly what the `TrackLocator` seam exists for; it needs a decision.
   - `WasapiAudio::open` takes **no track parameter at all**.
   - Windows audio **is the master clock**, so a mid-playback switch must tear the engine
     down and re-prime it at the playhead (`main.rs` already calls this the hard half).
   - And it cannot be verified without **listening**. Left alone deliberately.
3. **#90.3 remainder** — seek generations (no stale cue flash while scrubbing) and wiring
   `controls_h` (hardcoded `0.0` on **both** shells; `place()` supports the lift, nothing
   measures a bar yet).
4. **Archive'd videos have no embedded cues** — `stream_cues` would mean decompressing the
   whole archive to RAM. Sidecars in archives work.
5. **Linux is unverified.** Same wgpu path as Windows, so it should follow — nobody ran it.

## ⚠ One open owner call (unchanged)

- **`Automatic` falls back past the frozen forced-only rule.** Order is now
  forced+matching-audio → the container's default → anything renderable. Strict forced-only
  showed **nothing** for the commonest case (English film, full English track, no forced
  track) right after a toast saying "Subtitles on". Flagged in `SubtitleMode`'s docs —
  **confirm or overrule.**

---

# The load-bearing knowledge (don't re-derive these)

## The subtitle bitmap is PREMULTIPLIED, and the CPU overlays are not

`pb_hud::subtitle` emits **premultiplied** RGBA (its own tests enforce it). Every *other*
CPU overlay — toast, info line, pie, tree, chip — is authored **straight** and shares a
pipeline whose `ALPHA_BLENDING` multiplies by alpha. Send one through the other and it
multiplies **twice**: measured **0.108 vs 0.216** linear, which reads as every antialiased
glyph edge going muddy. So the subtitle layer has **its own premultiplied-blend pipeline**
(`Pipelines::subtitle`), identical to `overlay` in every other respect.

`the_subtitle_pipeline_blends_premultiplied_not_straight` (pb-render, GPU readback) fails on
exactly that mistake — confirmed by pointing it at the wrong pipeline. The value it asserts
is **also what the macOS `CGImage` `.premultipliedLast` path produces**, so the two shells
match by construction rather than by eye. Don't "simplify" the subtitle layer onto
`overlay_pipeline`.

## The presenter is shell-local, and must stay that way

`App::present_subtitles` (winit) and `subtitle_rgba`/`subtitle_rect` (pb-mac-ffi) are two
presenters over **one** rasterizer. Putting the wgpu call in the shared `tick_subtitles`
would draw every cue **twice** on macOS — once into a canvas the `AVPlayerLayer` covers,
once for real.

## Cues STREAM — the ratio, not an optimization

Reading an embedded track is a **full linear pass over the container**. **Measured 39 s** on
the corpus MKV (4.4 GB over SMB) — as a blocking wait, indistinguishable from broken. But
the reader walks in **presentation order** at ~113 MB/s while playback consumes ~1.6 MB/s —
**~70× faster than playback needs** — so it hands cues over as it finds them: **first batch
at 1.06 s**. Confirmed live on Windows 2026-07-16: 15 → 382 cues streaming in while playing.
Cancelling stops a read in ~1 s (`CueLoad::drop`).

*The optimization NOT to take:* the playback demuxer already reads these packets and
discards them — but that exists only on routes using **our** demuxer, and **a feature must
never be gated on a backend**. Layer it *under* this reader; never replace it.

## The rules that were bought with real time

- **Prove the pipe, then build through it.** ~1500 lines / ~75 tests of pure subtitle modules
  were once merged before a single caller existed; the thin slice that followed found five
  defects in an afternoon, every one in a *seam*.
- **Look at the output.** Both ASS defects were found by *printing real cues*; every unit
  test passed. It paid twice more on 2026-07-16 — the egui swatch asked for a **9888 px**
  texture (egui's first frame reports a default `screen_rect`, and egui *panics* past the GPU
  limit rather than clamping), and the tab shot showed the preview was the only thing that
  could not be reviewed from the code. Use `PB_PREVIEW_OUT=<dir> cargo test -p pb-app-core
  --lib -- --ignored dump_preview`, and `--settings-shot --tab=subtitles`.
- **Never gate a feature on a backend.** Use `AppCore::video_showing()` / `video_position()`.
- **The unit rule:** position and size are **viewport**-relative; decoration is **text**-relative.
  `REFERENCE_FONT_PX` is a **fixed anchor at 47.5** and must not track the default size.
- **`#[serde(default)]` on a by-value nested settings struct is load-bearing.** Without it one
  typo in `[subtitle_style]` makes the file unparseable, and `Settings::load` answers that by
  discarding **every other setting** in it.
- **A scaled egui control must write back only on `.changed()`.** Round-tripping a fraction
  through `×100` / `÷100` is not bit-exact (`0.04` → `0.040000001`) and the settings fold
  saves on any diff — so an unconditional write-back spews a config write **every frame the
  tab is open**.

## Verify with the corpus

- **`\\beenas\Media\Movies`** — 163 films (the owner pointed at this 2026-07-16; the rev-6
  `/Volumes/Media` paths are the Mac's view of it). `Ali.Wong.Baby.Cobra` (stream 2, `eng`
  subrip) is a clean, fast check. `Ad.Astra` has Chinese ASS (stream 5, drawing mode);
  `Avatar…3D` (stream 3) authors every subtitle twice; `Alita.Battle.Angel…DON` is
  **PGS-only** → "No subtitle tracks" is **correct**.
- `crates/pb-decode/tests/fixtures/video/multitrack.mkv` — 1 s, 3 subrip tracks + a PGS one,
  local and instant. Good for track *selection*; too short to watch.
- Diagnostics: **`PB_SUBTITLE_TRACE=1`** is the fastest way to answer "why is nothing on
  screen" — it prints the gate that stopped it, then `loading track N`, then `drew WxH at t`
  and `placed at Rect{…}`. `PB_VIDEO_DIAG=1` for playback.
- ⚠ **`--features ffprobe` needs a VS Developer shell** (`. .\scripts\vs-dev-env.ps1`) **and
  `VCPKG_ROOT` exported** — without it FFmpeg's bindgen dies on `pkg-config`. Embedded cues
  are ffprobe-gated, so a plain `cargo build` gets **sidecars only** and an embedded track
  looks broken.

## THE authoritative docs

- **Subtitles:** `.taskmaster/docs/90-presenter-and-style-contract.md` — the owner's spec, the
  frozen decisions, and **two post-mortems**.
- **Video:** `.taskmaster/docs/video-playback-overhaul.md` (root causes R1–R12).
- **Track catalog:** `.taskmaster/docs/98-phase0-spike-findings.md`.

---

# Other open work (not subtitles)

- **Phase 2 follow-ons** (deferred, no blockers): planar rotation in geometry/UV, planar-`Vec`
  pool + two-plane single-submit upload, MF P010 on Windows.
- **#94.1 — space-pauses-a-playing-video** — deferred UX; owner wants to workshop the
  contextual-key idea first.
- Older Windows/loose ends (#80 slideshow×video, #82 macOS archive natives, #75/#76 CI/mirror).
- **E-AC-3 audio fails to decode on Windows** (seen 2026-07-16 on a `DD+2.0` file:
  `WASAPI failed: audio format not decodable … 0xC00D36B4`). Pre-existing, unfiled, unrelated
  to subtitles — the picture plays fine, silently.

# Notes carried forward

- **Commit + push directly to main** (owner-authorized); fetch/merge origin/main first.
- ⚠ **The owner drives the app while you work.** A tool-launched instance and theirs will
  fight; say what you're launching. Never `git commit -am` / `add -A` — stage explicit paths.
- **Windows cross-check from the Mac:** `cargo check -p pb-app --target x86_64-pc-windows-msvc`
  after two temporary manifest edits — **blake3 `pure` must be a DIRECT dep** + `ureq`
  `default-features = false` in both crates. Restore them and `git checkout Cargo.lock` after.
- ⚠ **`cargo test --workspace` cannot build `pb-app` on macOS.** Use `--workspace --exclude pb-app`.
- ⚠ **Always build `--features ffvideo` when testing video code** on the Mac: `ActiveVideo` has a
  SECOND literal construction under `cfg(ffvideo/macos)`. Run clippy on **both** feature sets.
- Quit the app before rebuilding (a running exe silently breaks the link step).
- swift-bridge bridge module: `//` comments only; `Vec<String>` does **not** cross back to
  Swift — use indexed accessors.
- `settings.save()` is called **unguarded** by the older toggles, so dispatching them in a test
  writes the user's real `settings.toml`. `ToggleSubtitles` / `SubtitleCycle` gate on
  `persist_prefs` instead — copy that. ⚠ `apply_settings` does **not** check it either.
- ⚠ **The Bash tool eats one backslash** in a heredoc'd Python patch script, so a needle
  containing `\t` silently becomes a real tab and never matches. Use the Edit tool for Rust
  string literals.
