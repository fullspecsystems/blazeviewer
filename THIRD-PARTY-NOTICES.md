# Third-party notices

Blaze Viewer bundles and **dynamically links** the third-party components below. This file is
the human-readable summary: what is used, how it is linked, and why that linkage complies. The
verbatim upstream license texts live in `licenses/` beside it — that is the legal text; this is
the map.

Both ship with every binary distribution, and each release script **hard-fails** if they are
missing rather than shipping a binary without them:

| Platform | This file + `licenses/` land in |
|---|---|
| Windows | next to the exe inside the Velopack package (`scripts/release-windows.ps1`) |
| macOS | `Blaze Viewer.app/Contents/Resources/licenses` (`scripts/build-swift-host.sh`) |
| Linux | `usr/share/licenses` in the AppImage (`scripts/release-linux.sh`) |

## Versions (pinned)

| Library | Version | Where the pin lives |
|---|---|---|
| libheif | 1.23.0 | vcpkg commit `a0400024` (`scripts/setup-libheif.ps1 -VcpkgRef`) |
| libde265 | 1.1.1 | same vcpkg commit |
| dav1d | 1.5.3 | same vcpkg commit |
| FFmpeg | **8.1.2** on Windows (vcpkg) · **8.1.1** on macOS (pinned tarball + sha256 in `scripts/build-ffmpeg-macos.sh`) | as noted |
| Sparkle (macOS only) | 2.9.4 | `mac/Package.resolved` (SwiftPM) |

Linux takes libheif/FFmpeg from the build distro and bundles the resulting shared objects, so
its versions track that distro rather than the vcpkg pin.

## How the LGPL obligations are met

Three of these libraries are LGPL (libheif + libde265 under LGPL-3.0, FFmpeg under LGPL-2.1).
LGPL attaches to **distribution and linkage** — not to how much of the library is called, which
matters because Windows uses FFmpeg only to read containers. Each obligation is met as follows.

**1. Relink — LGPL-3.0 §4(d)(1) / LGPL-2.1 §6(b).** Every LGPL library is linked **dynamically
against a shared library the user can replace**. Dropping in a modified, ABI-compatible build
relinks the application, which is exactly what the licenses require:

| Platform | Mechanism |
|---|---|
| Windows | loose DLLs beside the exe, from vcpkg's **DLL** triplets (`x64-windows` / `arm64-windows`) |
| macOS | shared dylibs in `Blaze Viewer.app/Contents/Frameworks`, loaded via `@rpath` |
| Linux | shared `.so` files in the AppImage's `usr/lib` |

> The Windows DLL set is computed per release by a `dumpbin` transitive-import walk, not by hand:
> `heif`, `libde265`, `dav1d`, `avcodec-62`, `avformat-62`, `avutil-60`, `swresample-6`.
> `swscale` is deliberately absent — Windows only reads containers, so the scaler is never
> imported. All of them are Authenticode-signed alongside the exe.
>
> ⚠ `pb-decode/build.rs` links these dynamically **by default**. `PB_VCPKG_STATIC=1` forces a
> static link and exists for A/B measurement only — it is the non-compliant configuration by
> definition and **must never ship**. The same applies to re-adding `features = ["static"]` to
> the `ffmpeg-next` dependency, which silently returns FFmpeg to a static link.

**2. License text — LGPL-3.0 §4(b) / LGPL-2.1 §6.** Both licenses require the text itself to
travel with the binary. `licenses/` carries the verbatim upstream `COPYING` files (libheif and
libde265 ship the LGPL-3.0 **and** the GPL-3.0 it builds on, as §4(b) demands), plus dav1d's
BSD-2 text. See `licenses/README.md`.

**3. Attribution — LGPL-3.0 §4(c).** Triggered because the About panel displays a copyright
line: it names each bundled library with its copyright and points at the licenses folder.

**4. Corresponding source — LGPL-2.1 §4 / LGPL-3.0 §4 (via GPL-3.0 §6).** Shipping the DLLs
means distributing the libraries themselves in object form, which obliges us to make their
source available. **The libraries are unmodified upstream releases** at the versions pinned
above — only build *configuration* differs (decode-only flags; no source patches beyond the
upstream-packaged vcpkg portfile adjustments, which are themselves published in the pinned
vcpkg tree). The exact configuration is reproducible from `scripts/setup-libheif.ps1` (Windows)
and `scripts/build-ffmpeg-macos.sh` (macOS).

> **Written offer.** For three years from the date you received this binary, we will provide,
> on request, the complete corresponding machine-readable source code for the LGPL libraries
> distributed with it (libheif, libde265, FFmpeg), for no more than our cost of physically
> performing the distribution. Contact **jd@jdlien.com**. The same source is available directly
> from each project's upstream at the pinned versions listed above.

## dav1d (AV1 decoder) — BSD-2-Clause

Shipped as `dav1d.dll` on Windows (`--features dav1d`, task #76) for animated AVIF playback.
Windows-only in effect: macOS uses Image I/O and Linux uses FFmpeg for that path. BSD-2-Clause
is attribution-only — linkage is legally irrelevant here, and the DLL is simply a consequence of
the shared vcpkg tree. Homepage: https://code.videolan.org/videolan/dav1d

License text (reproduced as the license requires for binary distribution):

```
Copyright © 2018-2025, VideoLAN and dav1d authors
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this
   list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND
ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## Sparkle (macOS auto-update) — MIT

Embedded as `Sparkle.framework` in `Blaze Viewer.app/Contents/Frameworks` (task #65). **macOS
only** — Windows updates via Velopack and Linux via the self-updating AppImage, so no other
platform carries it. Homepage: https://sparkle-project.org

MIT asks only that the copyright notice and permission notice travel with the binary; there is
no relink or source obligation, and no on-screen notice requirement (unlike the LGPL libraries
above, which is why the About panel names FFmpeg but not Sparkle). The version is pinned by
`mac/Package.resolved` rather than a build script, so it moves when that file does.

Its `LICENSE` also carries the notices for the code Sparkle vendors — **bsdiff 4.3**
(BSD-2-Clause, © 2003-2005 Colin Percival) and **sais-lite** (MIT) — so shipping that one file
verbatim discharges all three at once. It is copied into `Contents/Resources/licenses/
sparkle-LICENSE.txt` straight from the resolved SwiftPM artifact that gets embedded, so it
cannot drift from the version actually shipped; `scripts/build-swift-host.sh` hard-fails if it
is missing.

> Historical note: Sparkle was bundled from task #65 but appeared in neither this file nor the
> app bundle's `licenses/` folder until task #77 — the same per-artifact gap as the macOS
> attribution, found the same way. The bundle-time guard in `scripts/build-swift-host.sh` now
> walks `Contents/Frameworks` and fails on any binary whose license text is not staged, which
> covers frameworks and dylibs alike.

## libheif (HEIF/HEIC container) — LGPL-3.0-or-later

Shipped as `heif.dll` on Windows (`--features libheif`); bundled as a shared library in the
Linux AppImage; **not used on macOS**, which decodes HEIC via Apple Image I/O — so macOS carries
no libheif exposure at all. Homepage: https://github.com/strukturag/libheif

Its plugin loader is compiled **off** (`-DENABLE_PLUGIN_LOADING=OFF` via the pinned portfile),
so nothing is `dlopen`'d outside the shipped DLL set on Windows. The Linux AppImage is the
exception and does use the dlopen'd plugins — see the AppImage section below.

Relink, license text, attribution and source: see *How the LGPL obligations are met* above.

## libde265 (HEVC decoder, libheif dependency) — LGPL-3.0-or-later

Shipped as `libde265.dll` on Windows beside libheif (it is `heif.dll`'s own dependency rather
than one the app imports directly); bundled in the Linux AppImage.
Homepage: https://github.com/strukturag/libde265

> Build trap, recorded because it cost real time: libde265's **import** library is `de265.lib`
> while its **static** library is `libde265.lib` — the vcpkg triplets disagree on that one name
> (`heif.lib` and `dav1d.lib` are stable across both, and the DLL is `libde265.dll` either way).
> The link name must track the triplet, not just the link kind.

## FFmpeg (video/audio decode; container reading on Windows) — LGPL-2.1-or-later

Provides cross-platform video playback and the macOS codec fallback (MKV/WebM/VP8/VP9/AV1 and
their audio; task #84). On **Windows** it reads container metadata *only* — the audio and
subtitle track table Media Foundation cannot enumerate (task #100) — while Media Foundation
still performs every decode there. That narrow role does **not** narrow the obligation: LGPL
attaches to linkage and distribution, not to call volume.

| Platform | FFmpeg | Linkage | LGPL §6 relink condition |
|---|---|---|---|
| macOS | yes | **shared dylibs** in `Blaze Viewer.app/Contents/Frameworks` | satisfied |
| Linux | yes | **shared `.so`** in the AppImage's `usr/lib` | satisfied |
| Windows | yes (demux/metadata only, since task #100) | **shared DLLs** (vcpkg `x64-windows` / `arm64-windows`) | satisfied |

Built from **pinned sources with a decode-only, LGPL configuration** — no `--enable-gpl`, no
`--enable-nonfree` — by `scripts/build-ffmpeg-macos.sh` (macOS, which asserts LGPL at the end of
the build) and the AppImage build (Linux). The Windows build comes from the pinned vcpkg tree,
patched to a demux/metadata-only configuration by `scripts/setup-libheif.ps1`; vcpkg's `gpl` and
`nonfree` features are opt-in and stay off, and FFmpeg's own configure reports
`License: LGPL version 2.1 or later`. We only ever **decode** — the GPL-only encoders and
filters (x264, x265, GPL avfilter) are irrelevant to a viewer and are excluded, so nothing is
given up by staying LGPL. Homepage: https://ffmpeg.org

> **Patent note (orthogonal to the copyright license):** FFmpeg's H.264/HEVC/etc. *decoders* are
> LGPL-licensed, but those codecs are patent-encumbered. Patent licensing is a separate question
> from the software license and is an owner consideration before any public or commercial
> distribution — the same posture as the OS-codec paths on Windows/macOS.

A full per-component compliance manifest (configure flags, enabled decoders/demuxers, license
map) lives in `.taskmaster/docs/ffmpeg-compliance.md`.

## Linux AppImage bundled libraries

`scripts/release-linux.sh` bundles the specialized decode stack — FFmpeg (LGPL-2.1+ core),
libheif + its dlopen'd codec plugins (libde265, libaom (BSD-2-Clause), …) — as *shared*
libraries inside the AppImage, per the AppImage excludelist model. Because they are shared
objects the user can replace, the LGPL relink condition is satisfied by construction. Their
license texts ship in `usr/share/licenses`; a generated per-release manifest covering the
plugins' full transitive set is a future improvement.

## Operating-system codecs (not bundled)

Windows AVIF/HEIC still decode uses the user-installed Microsoft Store AV1/HEVC/HEIF extensions
via WIC; macOS uses Image I/O and VideoToolbox. These are components of the user's operating
system — nothing is redistributed by Blaze Viewer, so nothing is licensed here.

## Font Awesome Pro icons

UI icons are rasterized at build time from Font Awesome Pro SVGs under the owner's FA Pro
license (see CLAUDE.md). The SVG sources are not redistributed; rendered icons in the shipped
binary are permitted app usage under that license.

## Rust crate dependencies

The Rust dependency graph is predominantly MIT OR Apache-2.0 (permissive; no text-in-binary
requirement beyond attribution norms). A complete generated inventory (`cargo about` /
`cargo license`) is still an open item — tracked under task #77.
