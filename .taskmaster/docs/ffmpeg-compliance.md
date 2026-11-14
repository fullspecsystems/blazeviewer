# FFmpeg compliance manifest (task #77 / #84 phase 7)

The concrete LGPL-compliance deliverable for bundling FFmpeg in PhotoBlaze, per the
`cross-platform-ffmpeg-video.md` plan §9-dist. Covers **what** is bundled, **how** it is
licensed and built, and **why** the distribution is compliant — plus the owner tasks that
remain before a public/commercial ship.

## Scope — where FFmpeg ships

| Platform | FFmpeg? | Linkage | Source of the libs |
|---|---|---|---|
| **macOS** | yes (task #84) | **shared dylibs** in `PhotoBlaze.app/Contents/Frameworks` | `scripts/build-ffmpeg-macos.sh` (pinned source, LGPL config) |
| **Linux** | yes | **shared `.so`** in the AppImage's `usr/lib` | `scripts/release-linux.sh` (system/distro FFmpeg, bundled by linuxdeploy) |
| **Windows** | **yes** — demux/metadata only (task #100, 2026-07-14) | **shared DLLs** beside the exe (`x64-windows` / `arm64-windows` vcpkg triplets, task #77) | pinned vcpkg tree; the ffmpeg port is moved to the pin in `scripts/ffmpeg-windows.psd1` (task #141) and trimmed to demux-only by `scripts/setup-libheif.ps1` |

The compliance argument below holds on all three platforms: **FFmpeg is a shared library the
user can replace.**

**Windows joined this manifest with task #100** (2026-07-14). Before that the table read
"Windows: no — FFmpeg is never linked". Windows links FFmpeg because Media Foundation cannot
enumerate subtitle tracks at all. MF still does every *video* decode, so FFmpeg's role there is
narrow, but LGPL attaches to **distribution and linkage**, not to how much of the library gets
called. FFmpeg was first linked statically there, which left an unmet §6 relink obligation;
task #77 moved it (with libheif/libde265) to DLLs, validated in the shipped 0.2.1 packages.

## License mode — LGPL-2.1-or-later, decode-only

- Built with **neither `--enable-gpl` nor `--enable-nonfree`** → FFmpeg's default **LGPL
  v2.1+**. The build script asserts this after `configure` (aborts if `CONFIG_GPL=yes` or
  `CONFIG_NONFREE=yes` appears in `config.mak`), so a GPL/nonfree build can never ship
  silently.
- **Encoders and muxers are disabled** — PhotoBlaze only ever *decodes*. This also removes the
  components most likely to be GPL (x264/x265 wrappers) and narrows the license surface.
- All decoders/demuxers/parsers stay enabled (see *Format coverage* below), so codec support
  matches the Homebrew build the Rust side is validated against — no format silently dropped.

## Build configuration (macOS)

Pinned in `scripts/build-ffmpeg-macos.sh`:

- **Version:** FFmpeg **8.1.3** (soname majors libavcodec.62 / libavformat.62 / libavutil.60 /
  libswscale.9 / libswresample.6). `ffmpeg-sys-next` 9.1 binds to whatever FFmpeg pkg-config
  finds, and `build-swift-host.sh --bundle-ffmpeg` points pkg-config at this pin, so the Rust
  side is always bindgen'd against the headers of the dylibs it bundles. The install is stamped
  (`.ffmpeg-version`); a stamp that differs from the pin forces a clean rebuild.
- **Source integrity:** the upstream `ffmpeg-8.1.3.tar.xz` sha256 is recorded in the script and
  verified on fetch (`7138d28c…78a3`), and the tarball was checked against FFmpeg's detached PGP
  signature (good signature, release key `FCF9 86EA … D676 58D8`) when pinned (2026-11-12).
- **Configure flags** (rationale):
  - `--enable-shared --disable-static` — shared libs are the LGPL relink mechanism (below).
  - `--disable-programs --disable-doc` — no `ffmpeg`/`ffplay`/`ffprobe` binaries.
  - `--disable-encoders --disable-muxers` — decode-only.
  - `--disable-avdevice --disable-avfilter` — unused by the crate (codec/format/swscale/
    swresample only); also smaller.
  - `--disable-network` — input is file / in-RAM archive bytes via a custom AVIO; no protocols.
  - `--disable-xlib --disable-libxcb --disable-sdl2` — **stop configure from auto-linking
    Homebrew's X11/XCB/SDL2** into every dylib (a real bug the first build hit; would break
    self-containment). zlib/bzlib/iconv stay — they are macOS **system** libs (`/usr/lib`).
  - `--enable-videotoolbox` — the phase-6 hardware decode path (`ffmpeg/hw.rs`).
  - `--install-name-dir=@rpath` — each dylib's id and inter-lib deps are `@rpath/lib….dylib`,
    so bundling only has to point the app binary at `Contents/Frameworks`.
  - `--extra-cflags/ldflags=-mmacosx-version-min=14.0` — matches the app's deployment floor.
- **Self-verification:** after install the script asserts every dylib's dependencies are either
  a system lib (`/usr/lib`, `/System/Library`) or an `@rpath` sibling — no stray external.

Output: `third_party/ffmpeg/arm64/{lib,include}` (git-ignored). arm64-only today (the app's
single slice, ADR-021); a universal build is a future `--arch x86_64` + `lipo` step.

## Build configuration (Windows)

Built by vcpkg from the tree pinned in `scripts/setup-libheif.ps1 -VcpkgRef`, with two local
changes the script applies to that tree's `ports\ffmpeg` on every run:

- **Version (task #141):** FFmpeg **8.1.3**, the same as macOS. The port at the pinned commit is
  8.1.2, and upstream vcpkg went from there straight to 9.0.x, a soname major. So the script
  moves the port's `version` and source `SHA512` to the values in `scripts/ffmpeg-windows.psd1`
  rather than moving the ref. Same sonames (avcodec-62 / avformat-62 / avutil-60 /
  swresample-6) and API, so it is a pure patch bump. The script rebuilds FFmpeg whenever the
  installed `ffversion.h` differs from the pin, and `release-windows.ps1` refuses to package any
  other version.
- **Source integrity (2026-11-13):** the `n8.1.3` tag is the same object (`23151b11`, commit
  `1041abdc`) on GitHub and `git.ffmpeg.org`. The release tarball has a good PGP signature from
  the release key `FCF9 86EA … D676 58D8`, and GitHub's tag archive (what vcpkg downloads, pinned
  by SHA512) matches it file for file. The archive adds only `.gitignore` / `.gitattributes`, and
  the tarball adds only `VERSION`.
- **Demux/metadata only (task #100):** `--disable-everything` plus the file protocol, the
  container demuxers, the subtitle decoders, and the audio decoders (audio playback and named
  channel layouts both need them). No video decoders, encoders, muxers, filters, network or
  programs. The exact list is the `PhotoBlaze task #100` block in `setup-libheif.ps1`.

Shipped beside the exe: the import closure of `blazeviewer.exe`, which `release-windows.ps1`
computes with dumpbin rather than a fixed list. Measured on x64 at 8.1.3 (2026-11-13):

| DLL | size | pulled in by |
|---|---|---|
| `avcodec-62.dll` | 1.94 MB | exe |
| `avformat-62.dll` | 0.54 MB | exe |
| `avutil-60.dll` | 0.84 MB | exe |
| `swresample-6.dll` | 0.12 MB | avcodec |

vcpkg also builds `swscale-9.dll`, but nothing imports it, so it is not shipped. At run time the
DLLs report `av_version_info` 8.1.3 and `avcodec_license` "LGPL version 2.1 or later".

## Bundled components (macOS)

Five shared dylibs, ~14 MB total (real files; the `.NN.dylib` / bare-name symlinks are not
copied — `bundle-ffmpeg-macos.sh` copies each under its soname):

| dylib | soname | approx size | role |
|---|---|---|---|
| libavcodec | `libavcodec.62.dylib` | ~11 MB | decoders + parsers (the bulk) |
| libavformat | `libavformat.62.dylib` | ~1.4 MB | demuxers |
| libswscale | `libswscale.9.dylib` | ~1.2 MB | YUV→RGB + scale |
| libavutil | `libavutil.60.dylib` | ~0.7 MB | shared utilities |
| libswresample | `libswresample.6.dylib` | ~0.1 MB | audio resample/mix |

## Format coverage

Enabled decoders/demuxers/parsers as configured (captured from `config.mak` at build time —
see the generated list appended by the build if run with the coverage dump). The target set for
task #84 is covered: **H.264, HEVC, VP8, VP9, AV1** video; **AAC, Opus, Vorbis, MP3, AC-3,
FLAC, PCM** audio; **Matroska/WebM, MOV/MP4, AVI, ASF/WMV, MPEG-PS/TS, 3GP** containers. (VP8
decodes in software only — no silicon hardware-decodes it, §10 carve-out.)

## Why this is LGPL-compliant

LGPL-2.1 §6 lets you distribute a work that *uses* the library in a proprietary application
**provided the user can relink against a modified library.** §6(b) explicitly accepts **"a
suitable shared library mechanism for linking with the Library."**

- **macOS:** FFmpeg is bundled as **shared dylibs** in `Contents/Frameworks`, loaded via
  `@rpath`. A user can build a modified, ABI-compatible `libavcodec.62.dylib` (from the same
  pinned source + `build-ffmpeg-macos.sh`, or their own) and **drop it into
  `Contents/Frameworks`** — the app loads it. That is the §6(b) relink mechanism. *(After such a
  swap the app's code signature over that dylib is invalidated; a user doing this locally can
  re-sign ad-hoc or run with Gatekeeper relaxed — the relink *capability* is what LGPL requires,
  and it is present.)*
- **Linux:** the AppImage bundles FFmpeg as shared `.so`; the same replace-in-`usr/lib` story.
- **Corresponding source:** the pinned upstream tarball (recorded sha256) + `build-ffmpeg-
  macos.sh` as the exact reproducible configuration. FFmpeg's own license texts
  (`COPYING.LGPLv2.1`, `LICENSE.md`) live in that source tree.

- **Windows:** FFmpeg ships as DLLs next to `blazeviewer.exe` (`avcodec-62.dll`,
  `avformat-62.dll`, `avutil-60.dll`, `swresample-6.dll`). Windows loads a DLL from the exe's own
  directory first, so replacing one with a modified, ABI-compatible build is the same §6(b)
  mechanism. Corresponding source: the upstream `n<version>` tag pinned in
  `scripts/ffmpeg-windows.psd1`, the port patches in the pinned vcpkg tree, and
  `setup-libheif.ps1` as the exact configuration.

History: Windows first linked FFmpeg (task #100) and libheif/libde265 **statically**
(`*-windows-static-md`), which does not get this for free; a static link needs object files or
another relink path. Task #77 chose DLL linkage for all three. `PB_VCPKG_STATIC=1` still forces
libheif/dav1d back to static for A/B measurement only (such a build must never ship). FFmpeg
stays a DLL even then: `ffmpeg-sys-next` picks its own triplet, and pb-decode deliberately does
not enable its `static` feature (see `crates/pb-decode/build.rs`).

## Patent note (separate from copyright license)

FFmpeg's H.264 / HEVC / AAC decoders are LGPL by **software license**, but those codecs are
**patent-encumbered**. Patent licensing is orthogonal to the LGPL copyright question and is an
**owner decision** before public/commercial distribution — the same consideration that applies
to the OS-codec paths (Windows WIC extensions, macOS VideoToolbox). This manifest does not
resolve it; it flags it.

## Remaining owner tasks before a public ship

1. ~~**Release integration**~~ — **DONE (2026-07-13).** `release-macos.sh` now builds the
   shipping app via `build-swift-host.sh --bundle-ffmpeg` by default (which pulls the `ffvideo`
   feature and bundles the pinned LGPL FFmpeg through `bundle-ffmpeg-macos.sh`). `--no-video`
   opts out; the script hard-fails if `nasm` (the FFmpeg build's one non-Xcode prereq) is absent
   rather than silently shipping a video-less DMG.
2. ~~**Inside-out signing**~~ — **DONE (2026-07-13).** `release-macos.sh`'s signing block re-signs
   every bundled `libav*/libsw*` dylib with the Developer ID + `--options runtime --timestamp`
   **before** the app binary (mirroring the Sparkle-helper step), so hardened-runtime library
   validation is satisfied by our own Team ID and notarization gets a secure timestamp. The
   existing `codesign --verify --deep --strict` seals them.
3. **Clean-machine validation** — `otool -L` closure (the bundle script's audit, run automatically
   during the build) + a launch on a Mac **without** Homebrew FFmpeg, confirming playback with
   zero external dependency. **Owner-run** (needs a second/clean machine).
4. **PGP-verify** the pinned tarball (the sha256 is verified on fetch; a PGP check against
   FFmpeg's release signature is the extra belt); decide **universal vs arm64-only** —
   arm64-only matches the app's single slice (ADR-021), so this defaults settled unless an
   x86_64 build is ever wanted.
5. **Patent-licensing decision** (above) for public/commercial distribution. **Owner decision.**
6. **Linux audit** — the AppImage already bundles FFmpeg shared; add the clean-container `ldd` +
   isolated-launch per-codec check the plan calls for (dynamically-loaded codec libs may not be
   discovered from the main exe).
