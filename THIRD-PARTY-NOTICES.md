# Third-party notices

PhotoBlaze bundles or statically links the third-party components below. This file ships
with the binary distributions (the Windows Velopack package copies it next to the exe;
see `scripts/release-windows.ps1`). Licenses that *require* their text to accompany binary
redistribution are reproduced in full; the rest are summarized with pointers.

Component versions are pinned via the vcpkg commit recorded in
`scripts/setup-libheif.ps1` (`-VcpkgRef`, currently `a0400024` → libheif 1.23.0,
libde265 1.1.1, dav1d 1.5.3, FFmpeg 8.1.2).

## dav1d (AV1 decoder) — BSD-2-Clause

Statically linked on Windows (`--features dav1d`, task #76) for animated AVIF playback.
Homepage: https://code.videolan.org/videolan/dav1d

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

## libheif (HEIF/HEIC container) — LGPL-3.0-or-later

Statically linked on Windows (`--features libheif`); bundled as a shared library in the
Linux AppImage; not used on macOS (Image I/O). Homepage: https://github.com/strukturag/libheif

> ⚠ **Open compliance item (2026-07-11, from the task #76 license audit; task #77):** LGPL-3.0
> (§4 / GPLv3 §6 via LGPL) attaches conditions to distributing a *statically* linked
> LGPL library in a proprietary binary — conventionally satisfied by shipping object
> files or another relink mechanism, or by dynamic linking (the Linux AppImage's shared
> `.so` satisfies this already; the Windows static link does not by itself). This
> predates task #76 and is unchanged by it. Owner to decide before a public/commercial
> release: relink materials, a DLL build, or counsel review. Same applies to libde265 —
> **and, since task #100 (2026-07-14), to FFmpeg on Windows too** (LGPL-2.1 §6, see below).
> The remedy should be chosen once and applied to all three; they are the same problem.

## libde265 (HEVC decoder, libheif dependency) — LGPL-3.0-or-later

Statically linked on Windows alongside libheif; bundled in the Linux AppImage.
Homepage: https://github.com/strukturag/libde265 — see the compliance note above.

## FFmpeg (video/audio decode; container reading on Windows) — LGPL-2.1-or-later

Provides cross-platform video playback and the macOS codec fallback (MKV/WebM/VP8/VP9/AV1
and their audio; task #84). On **Windows** it reads container metadata *only* — the audio and
subtitle track table Media Foundation cannot enumerate (task #100) — while Media Foundation
still performs every decode there.

| Platform | FFmpeg | Linkage | LGPL §6 relink condition |
|---|---|---|---|
| macOS | yes | **shared dylibs** in `PhotoBlaze.app/Contents/Frameworks` | satisfied by design |
| Linux | yes | **shared `.so`** in the AppImage's `usr/lib` | satisfied by design |
| Windows | yes (demux/metadata only, since task #100) | ⚠ **static** (vcpkg `*-windows-static-md`) | ⚠ **OPEN — see below** |

Built from a **pinned source with a decode-only, LGPL configuration** — no `--enable-gpl`,
no `--enable-nonfree` — by `scripts/build-ffmpeg-macos.sh` (macOS) and the AppImage build
(Linux). The Windows build comes from the pinned vcpkg tree, patched to a demux/metadata-only
configuration by `scripts/setup-libheif.ps1`; vcpkg's `gpl` and `nonfree` features are opt-in
and stay off, and FFmpeg's own configure reports `License: LGPL version 2.1 or later` for it.
Homepage: https://ffmpeg.org

**macOS + Linux — relink condition satisfied by design.** FFmpeg is linked **dynamically
against shared libraries the user can replace**: dropping a modified, ABI-compatible
`libav*.dylib` into `Contents/Frameworks` (macOS) or the AppImage's `usr/lib` (Linux) relinks
the application, which is exactly what LGPL-2.1 §6(b) requires. The **corresponding source** is
the pinned upstream tarball (recorded with its sha256 in `scripts/build-ffmpeg-macos.sh`) plus
that build script as the reproducible configuration. FFmpeg's own license texts
(`COPYING.LGPLv2.1`, etc.) accompany its source.

> ⚠ **Windows — open compliance item (2026-07-14, tasks #100 / #77).** Windows links FFmpeg
> **statically**, so LGPL-2.1 §6's relink condition is **not** satisfied by attribution alone —
> the same unresolved item as libheif/libde265 above, arrived at by the same convenience call
> (no DLLs to ship). It must be resolved the same way and at the same time; DLL linkage (§6(b))
> is the conventional fix. **A public or commercial Windows build must not ship until #77
> closes.** Nothing changes for macOS or Linux, which were already compliant by construction.

> **Patent note (orthogonal to the copyright license):** FFmpeg's H.264/HEVC/etc. *decoders*
> are LGPL-licensed, but those codecs are patent-encumbered. Patent licensing is a separate
> question from the software license and is an owner consideration before any public or
> commercial distribution — the same posture as the OS-codec paths on Windows/macOS.

A full per-component compliance manifest (configure flags, enabled decoders/demuxers,
license map) lives in `.taskmaster/docs/ffmpeg-compliance.md`.

## Linux AppImage bundled libraries

`scripts/release-linux.sh` bundles the specialized decode stack — FFmpeg (LGPL-2.1+
core), libheif + its dlopen'd codec plugins (libde265, libaom (BSD-2-Clause), …) — as
*shared* libraries inside the AppImage, per the AppImage excludelist model. Their license
texts are available from the bundled `.so` files' upstream projects; a generated
per-release manifest is a future improvement.

## Operating-system codecs (not bundled)

Windows AVIF/HEIC still decode uses the user-installed Microsoft Store AV1/HEVC/HEIF
extensions via WIC; macOS uses Image I/O. These are OS components — nothing is
redistributed by PhotoBlaze.

## Font Awesome Pro icons

UI icons are rasterized at build time from Font Awesome Pro SVGs under the owner's FA Pro
license (see CLAUDE.md). The SVG sources are not redistributed; rendered icons in the
shipped binary are permitted app usage under that license.

## Rust crate dependencies

The Rust dependency graph is predominantly MIT OR Apache-2.0 (permissive; no text-in-binary
requirement beyond attribution norms). A complete generated inventory (`cargo about` /
`cargo license`) is a future improvement if a public release warrants it.
