# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-16 (rev 9). Supersedes rev 8. **Subtitles display + configure on
Windows** (owner-confirmed on screen), and **the Playback menu + subtitle track flyout now
ship on Windows** (`756cfff`). Next: the Audio flyout, after an FFmpeg→MF bridge._

---

# ✅ DONE — the Playback menu (#99, Windows) — `756cfff`

**Owner ask (2026-07-16):** "menu parity with macOS for the 'Playback' menu for selecting
subtitles and audio tracks."

**Owner decision when I hit the audio blocker (see below): _"Ship Subtitles flyout now,
bridge audio next."_** So:

## The shipped shape (matches macOS `MenuBar.swift`)

Transport moved out of Image; the subtitle items moved out of View.

```
Playback
  Play/Pause            P
  Next Frame            .
  Previous Frame        ,
  ──────────────────────
  Subtitles             C   ← on/off toggle
  Subtitle Track      ▸     ← Automatic / ──── / English · SubRip / …   WORKS
  ──────────────────────
  Mute Live Photo Audio M
```

The **Audio flyout is deliberately absent on Windows** until the bridge below exists.

**Two items, not one** (owner, 2026-07-15, reaffirmed 2026-07-16): "Subtitles" is on/off,
"Subtitle Track" is *which*. Fusing them was the original defect — turning subtitles off had
to forget the track you picked, so picking Chinese and toggling twice gave you English.

**Verified live** on a 26-track film over SMB: `Automatic` at row 1, a separator, then rows
2..26 with canonical indices and the tick on the resolved track. `PB_SUBTITLE_TRACE=1` now
prints what the flyout would show (`menu: subtitle track flyout = […]`) — a native menu's
contents are invisible to everything but a human with a mouse, which makes "is the list
right?" the one question about this feature no test can answer.

### How it works — don't redesign this

macOS pulls its rows in `NSMenuDelegate.menuNeedsUpdate` (fires as the menu opens → never
stale). muda has **no such hook**: `build_menu` builds the tree **once** and
`apply_menu_to_native` only mirrors checkmarks onto it. So rows are **rebuilt on change**,
guarded by `SubtitleMenuSig` (`main.rs`) — a `Copy`, allocation-free tuple of (item /
showing / probed / active / on). Calling `subtitle_picker_rows()` every tick would format a
`Vec<String>` of track labels behind a playing video, which is exactly the hot path. The
rules live in the pure `menu::subtitle_track_rows` (testable with no menu/window/OS); the
muda builder just walks the result.

⚠ **Never disable the flyout's holder** to mean "no video". macOS learned this the hard way:
a disabled submenu item can't be hovered, so its delegate never fires again and nothing can
re-enable it — greying it out once greyed it out *permanently*. State lives in the submenu's
**contents**, which are always re-derivable.

**The core API it stands on** — shell-agnostic, do not rebuild any of it:
- `AppCore::subtitle_picker_rows() -> Vec<PickerRow>` (`app_core_impl.rs:8109`) — row 0 is
  **Off**, a real choice. Labels come from `tracks::track_summary` — **do not format
  tracks twice**.
- `AppCore::select_subtitle_row(row)` (`:8320`) — out-of-range is *ignored, not clamped*
  (a list that changed under the user must not silently select a different track).
- `AppCore::subtitle_tracks_known()` (`:8307`) / `subtitles_on()` (`:8301`).
- **The tri-state must not collapse:** no video → "No Video"; video whose probe hasn't
  landed → "Reading Tracks…"; probed and genuinely none → "No Subtitle Tracks". Saying
  "No subtitles" over an unread file is a confident lie. All three look empty — only
  `subtitle_tracks_known()` tells them apart.
- **Row 0 (Off) is dropped from the flyout; every surviving row keeps its index.** The
  toggle owns off/on. Hiding a row is fine; **renumbering silently selects a different track
  than the one the user read** — `the_flyout_drops_the_off_row_without_renumbering_the_rest`
  was verified to fail on exactly that mutation (every row shifted by one).
- **Linux keeps `Next Subtitle Track`**: its egui bar is hand-rolled with no submenus, so
  `Shift+C` stays the route there and that item is what advertises it. That's why
  `MenuAction::SubtitleCycle` still exists after the native bars dropped it.

## NEXT — the Audio flyout, AFTER the FFmpeg→MF bridge

**Do not wire an Audio flyout until the bridge exists.** It would select the wrong
language while confidently ticking the right one — and #99's rule is that audio may only
toast on a **confirmed** switch, precisely because a toast naming a track over unchanged
audio teaches the user to distrust every other toast.

### The blocker, measured this session (this is the load-bearing finding)

**MF enumerates audio streams in a different order than the container / FFmpeg.**
Measured on `crates/pb-decode/tests/fixtures/video/multitrack.mp4`, whose two audio tracks
are a **440 Hz `eng`** tone then a **220 Hz `fra`** tone (fixtures README):

| | ordinal 0 | ordinal 1 |
|---|---|---|
| FFmpeg / container | eng, 440 Hz | fra, 220 Hz |
| **Media Foundation** | **fra, 220 Hz** | **eng, 440 Hz** |

MF also reorders wholesale: it enumerates `[audio, audio, video]` where FFmpeg reads
`[video, audio, audio]`. `mf_tracks.rs`'s phase-0 spike found the same thing from the
*enumeration* side ("MF marks the **French Director's Commentary** `selected=true` … it
simply takes the first stream of the `MF_SD_MUTUALLY_EXCLUSIVE` group"); this session
confirmed it independently from the *decode* side. Two measurements, one conclusion.

**Why it bites:** on Windows the runtime catalog is **FFmpeg's** — it *supersedes* MF's in
`media_details.rs:193-199`, because MF models **no subtitle tracks at all**. So picker rows
are in FFmpeg's order with `TrackLocator::FfStream` locators, while the audio engine needs
MF's ordinal. `TrackLocator::MfStream(u32)` **already exists** for exactly this ("an FFmpeg
stream index, an MF stream ordinal … are different namespaces" — `tracks.rs:24`), but
**nothing bridges the two**.

### The agreed bridge design (owner picked "bridge audio next")

In `media_details.rs`, on `#[cfg(all(windows, feature = "ffprobe"))]`, we hold **both**
catalogs at that moment (MF's was built first, then FFmpeg's supersedes). So:

1. Match each FFmpeg audio track to an MF audio track on **(language, codec, channels)**.
2. `catalog.set_locator(id, TrackLocator::MfStream(ordinal))` on the FFmpeg catalog's
   audio tracks.
3. Picker row → `MfStream(n)` → `WasapiAudio::set_track(n)`.
4. **Ambiguity** (two identical tracks — same lang/codec/channels): fall back to order
   *within that group*. Document the residual risk; don't pretend it's exact.

### The rest of the audio switch (design is settled, code not written)

- **`MfAudioDecoder::open_track(input, rate, Some(ordinal))` is DONE + tested** (committed
  this session, `d364d2e`). `next_chunk` reads `self.stream`, not `FIRST_AUDIO_STREAM` —
  that mistake would play the old track while ticking the new one.
- **`WasapiAudio` needs `Cmd::SetTrack(ordinal)`.** `Cmd::Seek` (`wasapi_audio.rs:260`)
  already does the whole dance — stop client → `Reset()` → `engine.reseek(pos)` →
  `engine.fill()` preroll → restart if it was playing. A track switch is **that, plus
  swapping the decoder**: open a new `MfAudioDecoder` on the ordinal, seek it to the
  playhead, replace, preroll, restart.
  - A **failed open must keep the old decoder playing** (#99: a failed switch costs you the
    choice, not the sound) and report `false`.
  - Channel-count changes are already handled — the WASAPI client's format is the
    *device's*, and `write_frame`/`map_sample` map source→device channels.
  - Windows audio **is the master clock**, so the re-prime is the delicate half.
- **Reporting back:** the engine runs on its own thread. Add to `Shared`: `active_track:
  AtomicI64` (-1 unknown), plus a `switch_seq: AtomicU64` + `switch_ok: AtomicBool` so the
  shell can pick up the outcome once. Shell then calls `core.set_active_audio_row(row)` +
  `core.audio_track_switched(row, ok)` (`app_core_impl.rs:8199` / `:8266`).
- **The effect is already plumbed**: `CoreEffect::SelectAudioTrack { row }` is emitted by
  `cycle_audio_track` and currently **ignored** at `main.rs:3165`. `A` / `Shift+A` are
  bound and dispatched — they just do nothing on Windows today.

---

# ✅ Done this session (5 commits, pushed to main)

| commit | what |
|---|---|
| `e9e2047` | **The winit presenter** — `Renderer::set_subtitle_overlay` + `App::present_subtitles`. Windows/Linux draw subtitles at last. Also fixed the launch-preference bug. |
| `d984f3a` | **Settings ▸ Subtitles tab** (egui) over the live preview — all eight axes. |
| `d34c7f2` | **The Windows menu had NO subtitle entry at all** — added `Subtitles` + `Next Subtitle Track` to the native View menu. |
| `a40f610` | Section spacing on the Subtitles tab (`SECTION_GAP` between cards). |
| `d364d2e` | `MfAudioDecoder::open_track` + the MF-order measurement above. |
| `756cfff` | **The Playback menu + the subtitle track flyout** (above). The View items from `d34c7f2` moved into it. |

## A false alarm worth not re-running (2026-07-16) — CLOSED

Mid-verification the trace showed `Automatic` resolving to **Greek** on an English film with
English audio — it loaded `eng`, then switched to `gre`. It looked like a real bug. It was
**the owner selecting the Greek track as a test** while I was measuring (owner-confirmed).

Kept here because the ruling-out is reusable, and because the diagnosis nearly went the
wrong way:

- `ffprobe` says **no** subtitle track in that file has `default` or `forced` set, and a
  probe of `automatic()` printed `pref=None … -> default/first Some("eng")`. The resolve was
  correct all along.
- Greek can only come from `want == Track(gre_id)`, which `resolve()` returns **before**
  calling `automatic()` — i.e. only a deliberate pick puts it there.
- **A/B settled it:** HEAD (no menu changes) vs. my branch, 45 s of playback each on the same
  film → `loading track 2 (eng, subrip)` and nothing else, on both.

**Two lessons:** ⚠ the owner drives the app while you test (check `Get-Process blazeviewer`
before killing anything, and A/B before believing a mid-test anomaly) — and this pick, if it
came from the new flyout, is the **only end-to-end confirmation of the click path** that
exists: row 6 in the menu → `subtitle_track:6` → `select_subtitle_row(6)` → `FfStream(6)` →
Greek played. That chain is exactly what no unit test can reach.

**Owner-confirmed on screen 2026-07-16:** _"Subtitles do display!"_ — verified against a
real film over SMB, and by trace (`loading track 2 (eng, subrip) — FfStream(2)` → cues
streaming 15→382 → `drew 532x93` → `placed at Rect { x: 469.0, y: 718.89, … }`, x centred
exactly).

# 🔜 Remaining after the Playback menu

- **Linux is unverified.** Same wgpu path as Windows, so it should follow — nobody ran it.
- **#90.3 remainder** — seek generations (no stale cue flash while scrubbing) and wiring
  `controls_h` (hardcoded `0.0` at `app_core_impl.rs:8394` on **both** shells; `place()`
  supports the lift, nothing measures a bar yet).
- **Archive'd videos have no embedded cues** — `stream_cues` would mean decompressing the
  whole archive to RAM. Sidecars in archives work.
- **A winit playback-bar picker** (the Mac's popover, right of the runtime) is a *separate*
  ask from the menu: the winit "playback bar" is a hand-laid-out egui info pill
  (`panels_ui.rs`), not a transport `HStack`, so it means manual layout + hit-testing +
  joining the `video_bar_interactive` pointer gate.

## ⚠ One open owner call — now half-answered, awaiting a final yes/no

**The question was:** `Automatic` falls back past the frozen forced-only rule (order today:
forced+matching-audio → container default → anything renderable). Strict forced-only showed
**nothing** for the commonest case — English film, full English track, no forced track —
right after a toast saying "Subtitles on".

**Owner picked (2026-07-16, via the picker):** _"Forced its own row; Automatic = full subs"_
— add `SubtitleChoice::Forced` as a portable, persisted choice; `Automatic` then drops its
forced step and means "container default → anything renderable". That retires the question
above, and `automatic()` gets **simpler**, not bigger.

**Then the owner thought aloud and may be reversing:** _"Forced subtitles are always their
own track, right? So if that's what you want, just select it. … Handling those properly is
probably a bit of extra scope."_

**The fact that decides it, which I gave them and they haven't answered yet:** a track pick
**does not survive to the next film**. `SubtitleWant::Track(id)` is bound to one file's
catalog by generation, so "just select it" means re-selecting forced on every single film. A
`Forced Only` *row* is a portable preference — which is what someone who wants forced signs
actually wants: always, on every film. And it's the only route left for them, because "Off
means off, no forced exception" is frozen.

**Do not build this until the owner confirms.** If they do: one `SubtitleChoice` variant,
one `resolve` branch, one picker row, `automatic()` loses its forced step. **Separate commit
from the menu**, so it can revert alone.

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
match by construction rather than by eye. Don't "simplify" it onto `overlay_pipeline`.

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
at 1.06 s**. Confirmed live on Windows 2026-07-16 (15 → 382 cues while playing). Cancelling
stops a read in ~1 s (`CueLoad::drop`).

*The optimization NOT to take:* the playback demuxer already reads these packets and
discards them — but that exists only on routes using **our** demuxer, and **a feature must
never be gated on a backend**. Layer it *under* this reader; never replace it.

## The rules that were bought with real time

- **Prove the pipe, then build through it.** ~1500 lines / ~75 tests of pure subtitle modules
  were once merged before a single caller existed; the thin slice that followed found five
  defects in an afternoon, every one in a *seam*.
- **Look at the output.** Both ASS defects were found by *printing real cues*; every unit
  test passed. It paid three more times on 2026-07-16 — the egui swatch asked for a
  **9888 px** texture (egui's first frame reports a default `screen_rect`, and egui *panics*
  past the GPU limit rather than clamping); the tab shot showed the preview was the only
  thing that couldn't be reviewed from the code; and **the MF track-order finding above
  killed a fix that would otherwise have shipped playing the wrong language**. Use
  `PB_PREVIEW_OUT=<dir> cargo test -p pb-app-core --lib -- --ignored dump_preview`, and
  `--settings-shot --tab=subtitles`.
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
- **Settings pages set `item_spacing.y = 0`** (`settings_ui`): every gap is explicit. Cards
  need `ui.add_space(pbui::SECTION_GAP)` **between** them or the headings sit flush.

## Verify with the corpus

- **`\\beenas\Media\Movies`** — 163 films (the owner's pointer; the old `/Volumes/Media`
  paths are the Mac's view of the same share). `Ali.Wong.Baby.Cobra` (stream 2, `eng`
  subrip) is a clean fast check. `Ad.Astra` has Chinese ASS (stream 5, drawing mode);
  `Avatar…3D` (stream 3) authors every subtitle twice; `Alita.Battle.Angel…DON` is
  **PGS-only** → "No subtitle tracks" is **correct**.
- `crates/pb-decode/tests/fixtures/video/multitrack.{mkv,mp4}` — 1 s, local, instant. The
  **mp4** is the one for audio work: 2 AAC tracks, 440 Hz `eng` / 220 Hz `fra`, so the
  decoded *tone* proves which stream you got. The **mkv**'s 2nd audio track is **AC-3 and MF
  refuses to decode it** (see below) — don't reach for it.
- Diagnostics: **`PB_SUBTITLE_TRACE=1`** answers "why is nothing on screen" in one line —
  it prints the gate that stopped it, then `loading track N`, `drew WxH at t`, `placed at
  Rect{…}`. `PB_VIDEO_DIAG=1` for playback.
- ⚠ **`--features ffprobe` needs a VS Developer shell** (`. .\scripts\vs-dev-env.ps1`) **and
  `VCPKG_ROOT` exported** — without it FFmpeg's bindgen dies on `pkg-config`. Embedded cues
  are ffprobe-gated, so a plain `cargo build` gets **sidecars only** and an embedded track
  looks broken.

## THE authoritative docs

- **Subtitles:** `.taskmaster/docs/90-presenter-and-style-contract.md` — the owner's spec, the
  frozen decisions, and **two post-mortems**.
- **Video:** `.taskmaster/docs/video-playback-overhaul.md` (root causes R1–R12).
- **Track catalog:** `.taskmaster/docs/98-phase0-spike-findings.md` — read this before any
  track work; it is where MF's limits were first measured.

---

# Comic archives — CBZ / CBT / CBR (owner raised 2026-07-16)

Rides on the owner's `102-tar-family-archives.md` plan (untracked, theirs). My read:

- **CBZ = ZIP, CBT = TAR — same bytes, different extension.** Two more arms in that plan's
  `archive_kind` classifier (`cbz → Zip`, `cbt → Tar`) and they work: `ScopedSource`, name
  sorting (which *is* page order for comics), and the no-trace guarantee all carry over.
  Nearly free; should ride along with #102 rather than be its own task.
- **CBR = RAR is the one that costs**, and #102 already calls RAR a non-goal. UnRAR's licence
  permits decompression — which is all we'd ever do ("we only ever DECODE" covers it) — but
  it is **not OSI-approved, not Apache-compatible** for the FSL→Apache-2.0 conversion in two
  years, and it's a **C dep on three platforms**, exactly the build risk
  `pb-source/Cargo.toml` says the crate exists to avoid. No usable pure-Rust RAR5 decoder.
- **Cheap partial win regardless:** many `.cbr` files in the wild are **actually ZIPs** that
  were misnamed. Sniffing magic bytes (`Rar!\x1a\x07` vs `PK\x03\x04`) instead of trusting
  the extension opens those for free, and lets a genuine RAR say so instead of failing
  obscurely. Worth doing even if UnRAR is never taken.

# Other open work (not subtitles)

- **⚠ AC-3 / E-AC-3 audio does not decode on Windows.** `SetCurrentMediaType` fails with
  `0xC00D36B4` ("audio format not decodable"). Seen twice on 2026-07-16: the fixture mkv's
  AC-3 5.1 track, and a real `DD+2.0` film (`Ali.Wong.Baby.Cobra`) that **plays silently**.
  Pre-existing, unfiled, unrelated to subtitles — but a lot of films are DD/DD+, so this is
  probably worth a task of its own.
- **Phase 2 follow-ons** (deferred, no blockers): planar rotation in geometry/UV, planar-`Vec`
  pool + two-plane single-submit upload, MF P010 on Windows.
- **#94.1 — space-pauses-a-playing-video** — deferred UX; owner wants to workshop the
  contextual-key idea first.
- Older Windows/loose ends (#80 slideshow×video, #82 macOS archive natives, #75/#76 CI/mirror).

# Notes carried forward

- **Commit + push directly to main** (owner-authorized); fetch/merge origin/main first.
- ⚠ **The owner drives the app while you work.** Their instance locks
  `target/debug/blazeviewer.exe` and blocks rebuilds ("Access is denied"). **Check
  `Get-Process blazeviewer` before killing anything** — it is usually theirs. A separate
  `CARGO_TARGET_DIR` under the scratchpad is the non-invasive way to build meanwhile.
- Never `git commit -am` / `add -A` — stage explicit paths.
- **Windows cross-check from the Mac:** `cargo check -p pb-app --target x86_64-pc-windows-msvc`
  after two temporary manifest edits — **blake3 `pure` must be a DIRECT dep** + `ureq`
  `default-features = false` in both crates. Restore them and `git checkout Cargo.lock` after.
- ⚠ **`cargo test --workspace` cannot build `pb-app` on macOS.** Use `--workspace --exclude pb-app`.
- ⚠ **Always build `--features ffvideo` when testing video code** on the Mac: `ActiveVideo` has a
  SECOND literal construction under `cfg(ffvideo/macos)`. Run clippy on **both** feature sets.
- swift-bridge bridge module: `//` comments only; `Vec<String>` does **not** cross back to
  Swift — use indexed accessors.
- `settings.save()` is called **unguarded** by the older toggles, so dispatching them in a test
  writes the user's real `settings.toml`. `ToggleSubtitles` / `SubtitleCycle` gate on
  `persist_prefs` instead. ⚠ `apply_settings` does **not** check it either.
- ⚠ **The Bash tool eats one backslash** in a heredoc'd Python patch script, so a needle
  containing `\t` silently becomes a real tab and never matches. Use the Edit tool for Rust
  string literals.
