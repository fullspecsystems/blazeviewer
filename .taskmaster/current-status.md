# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-17 (rev 10). Supersedes rev 9. **Archives are doors** (#104) and
**the door card** (#105) ship on Windows, owner-confirmed on screen. What's left: command
gating on a door, the macOS card, and the blaze measurement gate. Rev 9's subtitle work
shipped; its still-live remainder (the FFmpeg→MF audio bridge) is carried forward below._

---

# Where we are

An archive in a folder is no longer skipped — it's a **door**: a deck item you can see and
step onto, showing a card that says *what it is* and *how to enter*. `P` (or the Open
button) enters it, and it behaves like a folder from there. Nothing about the archive is
read until you do that.

```
  ┌──────────────────────────┐
  │  ZIP Archive             │   ← header: Help's own title type + separator
  ├──────────────────────────┤
  │                          │
  │        [artwork]         │   ← the owner's folder art (yellow/Windows, blue/mac+Linux)
  │                          │
  │   wedding-photos.zip     │   ← middle-elided
  │                          │
  │        Open (P)          │
  └──────────────────────────┘
```

**The one idea that made it work:** a door is **UI, not image content**. Four separate
defects (a 12× glyph, a photo-sized ring slot, a grey box, 2.1× magnification) all had that
one root cause. The frame is now a **1×1 transparent sentinel**; the card is chrome the
shell draws. Doors also *dissolved* the original blocker — a folder of only archives used to
produce zero scan items, so there was nothing to navigate.

---

# 🔜 What's left

## 1. Command gating on a door — the live thread (owner discussion 2026-07-17)

**The real bug, confirmed:** `copy_image` (`app_core_impl.rs:5395`) guards only on
`displayed_item`, then calls `decode_item` — which on a door returns the 1×1 sentinel. So
**`Ctrl+C` on a door silently copies a 1×1 transparent pixel and toasts success.** Confident
and invisible; this is Codex's blocker 4 and it's real.

**Already correct:** `copy_path` (`:5424`) reads `source.path(item)`, which for a door *is*
the archive. Works today, no change. ⚠ Note there is **no "Copy Filename" command** in the
codebase — `ids::COPY_PATH` is labelled **"Copy File Path"** and copies the full path.

**The owner's model:** _"it can behave kinda like a scenario where nothing is open"_ — which
maps onto machinery that already exists. `MenuState` carries per-command `_enabled` flags
(`save_rotation_enabled`, `reveal_enabled`, `compare_pin_enabled`…) and `menu_state_from`
already takes `displayed_item`. Gating is a flag, not new architecture.

| | On a door |
|---|---|
| Open (`P`), navigation, Copy File Path, Reveal, Details, Delete | **work** — they're about the file, and a door *is* a file |
| **Copy Image** | **disable** — there's no image |
| Copy Text from Image (OCR), Describe / Ask | **disable** — they'd scan a transparent pixel |
| Compare (`compare_pin_cmd`, `:3280`) | **disable** — it'd pin a sentinel |
| Rotate / Save rotation | already toasts honestly |
| Zoom | harmless no-op on a 1×1; leave it |
| Copy Image Details | **works** — the Details panel already shows a door's size + format, so copying those facts is consistent (its own doc says the name means "the facts the panel shows", not "EXIF") |

## 2. "Copy" should be multi-representation — a SEPARATE task (owner's insight, 2026-07-17)

I argued Copy Image shouldn't double as copy-the-file because you couldn't tell which you
got. **The owner is right that this is wrong:** _"on the mac, don't you kind of get both?"_

**Both platforms' clipboards are multi-representation.** `NSPasteboard` is built for it, and
the Windows clipboard takes several formats on one copy — `CF_DIB` **and** `CF_HDROP`
together. The *target* picks: Photoshop takes the pixels, Explorer takes the file. So the
ambiguity I was worried about doesn't exist.

That reframes the label, too. If `Ctrl+C` means **"copy this thing, with every
representation that makes sense"**, it's honest for every item type with **no per-item
relabelling** — which matters, because muda has no `menuNeedsUpdate` and a per-door relabel
would mean a **menu rebuild per item on the blaze path** (the rebuild-on-signature pattern
we already use for the subtitle flyout). The item wants to be **"Copy"** (Finder's word),
not "Copy Image": a photo offers pixels + file, a door offers the file, a video offers the
file.

**Why it's separate from #105:** it touches clipboard code on both shells and improves
*photos* (copy the `.jpg` straight into Explorer or an email). It's a feature on its own
merits, not a door special case. **File it, don't smuggle it in.** Until it lands, gate per
the table above.

## 3. macOS: the same card in SwiftUI (105.3)

Blocked on a real Swift host build. Artwork crosses FFI via a **one-time
generation + dims + RGBA accessor cached in `CoreModel`** — never clone ~4 MiB per pump.
⚠ `cargo check -p pb-mac-ffi` validates only the Rust half; this needs a real-folder smoke
test on the Mac. Also still open from #104: the two `cfg(macos)` routing arms.

## 4. The blaze measurement gate (105.4) — the honest debt

The card is the **first persistent chrome on the blaze hot path**, and its filename changes
every frame while scrubbing consecutive doors. The plan's original "costs nil" claim was
**withdrawn as unsupported** — per the prime directive, we don't guess. Compare an
image-only deck vs. a consecutive-door deck at target res + 120 Hz; report **p50/p95/p99**
CPU frame time and keypress→photon. Never means.

## 5. Cleanup (105.5)

`play_hint_persistent`, the kind-3 arms, `never_upscale`, the dead
`pb_hud::icon::assets::FILE_ZIPPER` + its vendored SVG. **Drop `Icon::Archive`** (glyph
arms, gallery entry, both vendored family SVGs) — the card uses the artwork, so it's likely
unused now; check before deleting. `CHANGELOG.md` still describes an archive tile/icon.

## 6. Task #104 is `review`

Needs owner validation on a real folder. The card, the centring fix, and RAR4 viewing
(#103) are all owner-confirmed already; what's unvalidated is the whole-feature pass.

---

# The load-bearing knowledge from this work (don't re-derive)

## A new `LibraryItemKind` must opt OUT of byte reads, not into them

`c19cfd6`. The door's whole guarantee — **nothing is read, and no password is prompted,
until you press `P`** — rests on typed dispatch sitting **above** `source.bytes()`. Two
byte-read leaks (the thumbs strip, `Shift+I`) were found in Phase 0 because the guards were
written as *negative* (`if kind != Video`), so a third kind fell straight through into a
read. They're now **positive `Image` guards + exhaustive matches**, so the compiler names
every site the next kind must handle. ⚠ That worklist is **platform-specific** — a
`cfg(macos)` arm won't fail a Windows build.

## egui: never call `load_texture` inside `data_mut` — it deadlocks

This froze the app on a folder of archives (`9110327`). `ctx.data_mut` holds a write lock on
the **whole Context**; `load_texture` re-enters it. `pb_ui::icon::texture` already had the
correct pattern — **read → load → insert** — and I invented a worse one anyway. Mutation-
verified: hangs at 45 s vs. 0.02 s passing.

## egui: an auto-sized anchored Window places from the PREVIOUS run's rect

This is why a long filename left the *next* card off-centre too (`b2a7a28`). Fix: compute
the size yourself and anchor `LEFT_TOP`. Mutation shows 192 px off. ⚠ When testing this,
read `ctx.memory(|m| m.area_rect(id))` — the SDF shadow's expanded clip rect reports a
perfectly centred card as 62 px off.

## The size readout goes through the scan worker, never the frame path

`info_line_parts` runs per frame; an SMB `stat` there would block it. So `FsSource::new`
(which its doc notes runs **on the scan worker thread**) stats **archives only** and
`size_hint(i)` serves it. `human_bytes()` uses decimal units to match Explorer/Finder.

## The artwork is cropped subject-centred, not ink-centred

`crop_to_content` takes **two** bboxes — ink (`alpha>=1`) and subject (`alpha>=200`) — and
is symmetric about the *subject's* centre, so the drop shadow doesn't shove the folder
off-centre. Measured: 1024² → 912×878, 49 px margins all round. **Keep the alpha** — an
invented opaque matte is what produced the grey box against the `[10,10,12]` letterbox.
Letterbox is configurable (`set_letterbox`), *not* `Color::BLACK`; `pb-scene-pipeline` uses
`ALPHA_BLENDING`, so transparent frames blend over it (proven by
`transparent_image_blends_over_letterbox`, `gpu.rs:4596`).

## The retained overlay needs two seams, and neither is egui's

`overlay_panel_visible()` (`main.rs:1458`) gates whether it draws at all; `overlay_dirty`
(`:525`, set `:1534`) gates rebuilds. **Nothing honours egui's own repaint request** — a new
persistent element must join both or it silently never appears / never updates.

## `ScaleMode::Fit` genuinely upscales

`view::base_scale` is `(sw/rw).min(sh/rh)` with **no clamp**. `pb_render::fit_rect`'s "we do
not upscale" doc describes a **dead function** — don't trust it.

---

# Carried forward from rev 9 — the FFmpeg→MF audio bridge (still unbuilt)

The Playback menu + subtitle track flyout **shipped** (`756cfff`, owner-confirmed). The
**Audio flyout is deliberately absent on Windows** until this bridge exists — it would
select the wrong language while confidently ticking the right one.

**The measured blocker** — `crates/pb-decode/tests/fixtures/video/multitrack.mp4`, whose two
audio tracks are a **440 Hz `eng`** tone then a **220 Hz `fra`** tone:

| | ordinal 0 | ordinal 1 |
|---|---|---|
| FFmpeg / container | eng, 440 Hz | fra, 220 Hz |
| **Media Foundation** | **fra, 220 Hz** | **eng, 440 Hz** |

MF also reorders wholesale (`[audio, audio, video]` vs FFmpeg's `[video, audio, audio]`).
Confirmed twice, independently: `mf_tracks.rs`'s phase-0 spike from the *enumeration* side,
and rev 9's measurement from the *decode* side.

**Why it bites:** on Windows the runtime catalog is **FFmpeg's** — it supersedes MF's at
`media_details.rs:193-199`, because MF models **no subtitle tracks at all**. So picker rows
carry `TrackLocator::FfStream` while the audio engine needs an MF ordinal.
`TrackLocator::MfStream(u32)` **already exists** for exactly this; nothing bridges the two.

**The agreed design** (owner picked "bridge audio next"): in `media_details.rs` under
`cfg(all(windows, feature = "ffprobe"))` we hold **both** catalogs at that moment. Match each
FFmpeg audio track to an MF one on **(language, codec, channels)** → `catalog.set_locator(id,
MfStream(ordinal))` → picker row → `WasapiAudio::set_track(n)`. On **ambiguity** (two
identical tracks) fall back to order *within that group* and document the residual risk.

**The rest** (settled, unwritten): `MfAudioDecoder::open_track` is **done + tested**
(`d364d2e`) and `next_chunk` reads `self.stream`, not `FIRST_AUDIO_STREAM`. `WasapiAudio`
needs `Cmd::SetTrack(ordinal)` — `Cmd::Seek` (`wasapi_audio.rs:260`) already does the whole
dance (stop → `Reset()` → `reseek` → `fill()` preroll → restart); a switch is that **plus
swapping the decoder**. A **failed open must keep the old decoder playing** and report
`false`. Report back via `Shared`: `active_track: AtomicI64` + `switch_seq`/`switch_ok`, then
`core.set_active_audio_row(row)` / `core.audio_track_switched(row, ok)`
(`app_core_impl.rs:8199` / `:8266`). `CoreEffect::SelectAudioTrack { row }` is already
emitted and currently **ignored** at `main.rs:3165` — `A` / `Shift+A` do nothing on Windows
today.

⚠ **Never disable a flyout's holder** to mean "no video": macOS learned that a disabled
submenu can't be hovered, so its delegate never fires and nothing can re-enable it. State
lives in the submenu's **contents**.

## One open owner call, still unanswered

`Automatic` falls back past forced-only (forced+matching-audio → container default →
anything renderable). Owner picked _"Forced its own row; Automatic = full subs"_ via the
picker, then thought aloud about reversing (_"just select it"_). **The fact that decides it,
given and unanswered:** a track pick **does not survive to the next film** —
`SubtitleWant::Track(id)` is bound to one file's catalog by generation, so "just select it"
means re-selecting forced on every film. A `Forced Only` *row* is portable, which is what
someone who wants forced signs actually wants. **Don't build until confirmed.** If yes: one
`SubtitleChoice` variant, one `resolve` branch, one picker row, `automatic()` loses its
forced step. Separate commit.

---

# Other open work

- **⚠ AC-3 / E-AC-3 does not decode on Windows.** `SetCurrentMediaType` fails
  `0xC00D36B4`. Seen on the fixture mkv's AC-3 5.1 track and a real `DD+2.0` film
  (`Ali.Wong.Baby.Cobra`) that **plays silently**. Pre-existing, unfiled — a lot of films are
  DD/DD+, so probably its own task.
- **Linux is unverified** for subtitles. Same wgpu path as Windows, so it should follow.
- **#90.3 remainder** — seek generations (no stale cue flash while scrubbing) and wiring
  `controls_h` (hardcoded `0.0` at `app_core_impl.rs:8394` on **both** shells).
- **Archived videos have no embedded cues** — `stream_cues` would mean decompressing the
  whole archive to RAM. Sidecars in archives work.
- **A winit playback-bar picker** is a *separate* ask from the menu: the winit bar is a
  hand-laid-out egui info pill, not a transport `HStack`.
- **#94.1 space-pauses-a-playing-video** — deferred; owner wants to workshop contextual keys.
- Older loose ends: #80 slideshow×video, #82 macOS archive natives, #75/#76 CI/mirror.

---

# THE authoritative docs

- **Doors + the card:** `.taskmaster/plans/104-archives-in-the-folder-tree.md` (rev6),
  `.taskmaster/plans/105-the-door-card.md` (rev4, Codex-reviewed).
- **Subtitles:** `.taskmaster/docs/90-presenter-and-style-contract.md` — the owner's spec,
  the frozen decisions, two post-mortems.
- **Video:** `.taskmaster/docs/video-playback-overhaul.md` (root causes R1–R12).
- **Track catalog:** `.taskmaster/docs/98-phase0-spike-findings.md` — read before any track
  work; where MF's limits were first measured.

# The rules that were bought with real time

- **Look at the output.** Every door defect the owner reported was *visible*, and none of
  them failed a test. `--egui-shot` / `PB_SHOT_DOOR=1|long` exist for this.
- **Prove the pipe, then build through it.** ~1500 lines / ~75 tests of pure subtitle
  modules were once merged before a single caller existed; the thin slice that followed
  found five defects in an afternoon, every one in a *seam*.
- **Never gate a feature on a backend.** Use `AppCore::video_showing()` / `video_position()`.
- **When a plan and the code disagree, the code wins — fix the plan.** My own "nine guards"
  claim was wrong (3 were already exhaustive, 2 were correct to leave, 4 needed changing);
  following it mechanically would have made things worse.

# Notes carried forward

- **Commit + push directly to main** (owner-authorized); fetch/merge origin/main first.
- ⚠ **The owner drives the app while you work.** Their instance locks
  `target/debug/blazeviewer.exe` and blocks rebuilds ("Access is denied"). **Check
  `Get-Process blazeviewer` before killing anything** — it is usually theirs. A separate
  `CARGO_TARGET_DIR` under the scratchpad is the non-invasive way to build meanwhile.
  A mid-test anomaly is often **the owner**, not a bug — A/B before believing it.
- Never `git commit -am` / `add -A` — stage explicit paths.
- ⚠ **`--features ffprobe` needs a VS Developer shell** (`. .\scripts\vs-dev-env.ps1`) **and
  `VCPKG_ROOT` exported** — without it FFmpeg's bindgen dies. Embedded cues are
  ffprobe-gated, so a plain `cargo build` gets **sidecars only**.
- ⚠ **`cargo test --workspace` cannot build `pb-app` on macOS.** Use `--exclude pb-app`.
- ⚠ **Always build `--features ffvideo` when testing video code** on the Mac: `ActiveVideo`
  has a SECOND literal construction under `cfg(ffvideo/macos)`. Clippy on **both** sets.
- **Windows cross-check from the Mac:** `cargo check -p pb-app --target
  x86_64-pc-windows-msvc` after two temporary manifest edits — **blake3 `pure` must be a
  DIRECT dep** + `ureq` `default-features = false` in both crates. Restore + `git checkout
  Cargo.lock` after.
- swift-bridge module: `//` comments only; `Vec<String>` does **not** cross back to Swift —
  use indexed accessors.
- `settings.save()` is **unguarded** in the older toggles, so dispatching them in a test
  writes the owner's real `settings.toml`. Gate on `persist_prefs`. ⚠ `apply_settings`
  doesn't check it either.
- ⚠ **Scripted edits keep silently no-op'ing** because `cargo fmt` reflowed the target text
  (it left doors arming a pill, and left debug prints in). The Bash tool also eats one
  backslash in a heredoc'd Python needle, so `\t` becomes a real tab. **Use the Edit tool
  for Rust string literals.**
- **Verify with the corpus:** `\\beenas\Media\Movies` (163 films); the doors test archives
  under `D:\Media` (RARs with real photos in them).
