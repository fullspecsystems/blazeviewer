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
| **Windows** | **no** | — | Media Foundation handles video; FFmpeg is never linked |

The compliance argument below is the same on macOS and Linux: **FFmpeg is a shared library
the user can replace.** Windows carries no FFmpeg, so its separate libheif/libde265 static-link
item (task #77 original) is unaffected by this.

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

- **Version:** FFmpeg **8.1.1** (soname majors libavcodec.62 / libavformat.62 / libavutil.60 /
  libswscale.9 / libswresample.6). This **must** match `ffmpeg-sys-next` 8.1's ABI — the Rust
  crate is bindgen'd against these headers. Bumping `ffmpeg-sys-next` means re-pinning here.
- **Source integrity:** the upstream `ffmpeg-8.1.1.tar.xz` sha256 is recorded in the script and
  verified on fetch (`b6863add…edf3`). Re-verify against FFmpeg's PGP release signature before a
  public ship.
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

Contrast: the Windows **static** link of libheif/libde265 (task #77 original) does **not** get
this for free — a static link needs object files or another relink path. **FFmpeg here avoids
that problem entirely by being dynamically linked.**

## Patent note (separate from copyright license)

FFmpeg's H.264 / HEVC / AAC decoders are LGPL by **software license**, but those codecs are
**patent-encumbered**. Patent licensing is orthogonal to the LGPL copyright question and is an
**owner decision** before public/commercial distribution — the same consideration that applies
to the OS-codec paths (Windows WIC extensions, macOS VideoToolbox). This manifest does not
resolve it; it flags it.

## Remaining owner tasks before a public ship

1. **Release integration** — wire `build-ffmpeg-macos.sh` + `bundle-ffmpeg-macos.sh` into
   `release-macos.sh`, and add `ffvideo` to the macOS ship feature set. **Deliberately not done
   yet:** ship-gating by discipline (owner) keeps `ffvideo` out of release scripts until phases
   5–6 are owner-validated on hardware.
2. **Inside-out signing** — the FFmpeg dylibs must be Developer-ID signed **before** the app
   binary (like Sparkle's helpers), then the app, then notarized. `bundle-ffmpeg-macos.sh`
   ad-hoc-signs for local runs; `release-macos.sh`'s signing block must gain the FFmpeg dylibs.
3. **Clean-machine validation** — `otool -L` closure (the bundle script's audit) + a launch on a
   Mac **without** Homebrew FFmpeg, confirming playback with zero external dependency.
4. **PGP-verify** the pinned tarball; decide **universal vs arm64-only**.
5. **Patent-licensing decision** (above) for public/commercial distribution.
6. **Linux audit** — the AppImage already bundles FFmpeg shared; add the clean-container `ldd` +
   isolated-launch per-codec check the plan calls for (dynamically-loaded codec libs may not be
   discovered from the main exe).
