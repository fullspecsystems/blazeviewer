# #77 — the macOS About panel is missing its LGPL attribution

**Status:** ready to implement, macOS-only, small.
**Found:** 2026-07-19, while cutting 0.3.0.
**Owning task:** #77 (LGPL compliance), still `in-progress`.

## The finding

The macOS `.app` bundles FFmpeg dylibs (LGPL-2.1) in `Contents/Frameworks`, but the
native About panel never names FFmpeg, never mentions LGPL, and never points at the
license texts that ship inside the bundle. Of the four obligations #77 tracks:

| Obligation | macOS |
|---|---|
| 1. Linkage (LGPL-2.1 §6(b)) | ✅ FFmpeg dylibs, `@rpath`, `bundle-ffmpeg-macos.sh` |
| 2. License text (LGPL-2.1 §6) | ✅ `Contents/Resources/licenses`, asserted at build time |
| 3. **Attribution / prominent notice** | ❌ **this document** |
| 4. Corresponding source (written offer) | ✅ `THIRD-PARTY-NOTICES.md` |

**Not a regression.** Every macOS DMG shipped to date has this gap; 0.3.0 is no worse
than 0.2.1. Windows and Linux are unaffected — they show the attribution already.

## Why it was missed (read before "fixing" it the obvious way)

The attribution text **already exists**, in the wrong shell. `crates/pb-app/src/dialog.rs`
(~1276–1319) builds a `bundled_libs()` list and appends a per-platform pointer line,
including this macOS-specific string at **dialog.rs:1315**:

```
"Full license texts are inside the app bundle, in Contents/Resources/licenses."
```

That string is **dead code on macOS**. `pb-app` is the winit shell; the Mac ships the
SwiftUI host in `mac/` and never links `pb-app` (see CLAUDE.md, *Architecture*). There is
even a unit test enforcing the attribution — `the_card_names_libheif_and_its_license`,
**dialog.rs:~3250** — on a code path macOS never executes. A green test suite therefore
"proved" an obligation the Mac never satisfied. That is the trap: the test is
shell-local, the obligation is per-artifact.

## ⚠️ Do NOT copy the Windows library list

It would make false claims about what the Mac ships. macOS links exactly **one**
attributable native library:

- **FFmpeg** — LGPL-2.1 — bundled dylibs. **The only one.**
- **libheif / libde265** — NOT linked on macOS. HEIC decodes via Apple Image I/O.
  Anchor: `crates/pb-decode/build.rs:38-39` — *"macOS is deliberately excluded (it
  decodes HEIC via Image I/O; no libheif backend there)."*
- **dav1d** — NOT linked on macOS. Gated `target_os == "windows"`.
  Anchor: `crates/pb-decode/build.rs:50`.

Naming libheif or dav1d in the Mac About panel would be a *false* attribution, which is
its own defect. Verify against the built artifact before writing the list:

```sh
otool -L "target/swift-host/Blaze Viewer.app/Contents/MacOS/Blaze Viewer" | grep -i 'libav\|libsw\|heif\|dav1d'
ls "target/swift-host/Blaze Viewer.app/Contents/Frameworks/"
```

(Reminder from CLAUDE.md's *Homebrew trap*: a **dev** build linking
`/opt/homebrew/.../ffmpeg` GPL dylibs is expected and correct. Judge licensing only from
a `--bundle-ffmpeg` release build or a `dist/` DMG. Homebrew paths in a dev build are not
the bug this document is about.)

## The change

**File:** `mac/Sources/BlazeViewerMac/CoreModel.swift`
**Function:** `aboutPanelOptions()` — **~line 1257**
**Called from:** `case "about"` at **~line 1220**, via
`NSApp.orderFrontStandardAboutPanel(options:)` at **~line 1222**

Today `options[.credits]` is exactly two things: the tagline *"An ultra-fast image
viewer"* and a clickable `blazeviewer.app` link. Add, below those:

1. A line naming **FFmpeg** with its copyright and license — mirror the Windows wording
   for consistency: `FFmpeg © the FFmpeg developers (GNU LGPL v2.1)`.
2. A pointer to the license texts, wording already written at dialog.rs:1315:
   *"Full license texts are inside the app bundle, in Contents/Resources/licenses."*

Keep using the **standard** About panel. Do not build a custom About window — the
standard panel with `.credits` is the Mac-native answer, and `orderFrontStandardAboutPanel`
is already wired. Load the `/mac-arsed-mac-app` skill before touching user-facing text or
layout.

**Style:** match the existing block — `NSFont.smallSystemFontSize`, centered
`NSMutableParagraphStyle`, `NSColor.secondaryLabelColor` is appropriate for the
attribution so it reads as fine print rather than competing with the tagline. Keep it to
two short lines; the About panel is not a legal document, it is the pointer to one.

Per the house copy style: plain and simple, **no em-dashes** in user-facing strings.

## Guard against the next drift

The root cause is that the obligation was enforced by a test in a shell that macOS does
not build. Consider a Swift-side test (or at minimum a comment at both sites cross-
referencing each other) asserting the credits string names every LGPL library the bundle
actually contains. A test that reads `Contents/Frameworks` and asserts each bundled
`libav*`/`libsw*` family is named in the credits would have caught this and will catch the
next library added.

## Verification

1. `./scripts/build-swift-host.sh --bundle-ffmpeg` (release path, not `--ffvideo`).
2. Launch, `About Blaze Viewer` — confirm FFmpeg + LGPL-2.1 + the licenses pointer are
   visible without scrolling.
3. Confirm the path in the text is real:
   `ls "…/Blaze Viewer.app/Contents/Resources/licenses/"` → must contain
   `ffmpeg-COPYING.LGPLv2.1.txt`. Staged by `scripts/build-swift-host.sh:159-167`, which
   already **hard-fails** if that file is missing.

## Notes / out of scope

- `licenses/` ships all four COPYING files (dav1d, ffmpeg, libde265, libheif) into every
  platform's bundle, including macOS which links only FFmpeg. Harmless — extra license
  text is not a false claim, and trimming per-platform risks tripping the build-time
  assertion for no compliance gain. Leave it.
- The remaining #77 item is unchanged and separate: the `cargo license` / `cargo about`
  sweep of the Rust dependency tree.

## Release sequencing

**This fix is NOT part of 0.3.0.** Windows x64 0.3.0 is already built, signed, and
published from tag `v0.3.0` (`508ead51`). Re-tagging to include a code change would make
`v0.3.0` stop describing the artifact users already have, which is precisely what the tag
exists to prevent.

So: ship the macOS 0.3.0 DMG from `v0.3.0` as-is (the gap is pre-existing, not new), and
land this fix in **0.3.1**. No re-tag needed, and this document being pushed after the tag
is fine — it is not part of any build.
