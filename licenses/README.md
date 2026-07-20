# Bundled library licenses

These are the **upstream license texts, verbatim**, for the native libraries the app links.
They ship with every binary distribution (Windows, macOS, Linux) — see the release scripts.

This is not courtesy attribution. Both licenses require the text itself to travel with the
binary, in words that leave no room:

- **LGPL-2.1 §6** (FFmpeg): *"You must supply a copy of this License."*
- **LGPL-3.0 §4(b)** (libheif, libde265): *"Accompany the Combined Work with a copy of the GNU
  GPL and this License."* — both documents, which is why those two files are large: upstream's
  `COPYING` concatenates the LGPL-3.0 **and** the GPL-3.0 it builds on.
- **BSD-2-Clause** (dav1d): binary redistribution must reproduce the copyright notice and
  disclaimer.

`THIRD-PARTY-NOTICES.md` at the repo root is the human-readable summary (what is used, how it
is linked, and why that linkage complies). It points here; this is the legal text.

| File | Library | License |
|---|---|---|
| `libheif-COPYING.txt` | libheif 1.23.0 | LGPL-3.0-or-later (+ GPL-3.0, + MIT for samples) |
| `libde265-COPYING.txt` | libde265 1.1.1 | LGPL-3.0-or-later (+ GPL-3.0, + MIT for samples) |
| `ffmpeg-COPYING.LGPLv2.1.txt` | FFmpeg 8.1.2 (Windows) · 8.1.1 (macOS) | LGPL-2.1-or-later |
| `dav1d-COPYING.txt` | dav1d 1.5.3 | BSD-2-Clause |

## Provenance

Copied verbatim from the pinned upstream sources — the same trees the shipped binaries are
built from (the vcpkg commit recorded in `scripts/setup-libheif.ps1 -VcpkgRef`, and the pinned
FFmpeg tarball in `scripts/build-ffmpeg-macos.sh`). Not fetched from the web, not retyped.

**When a pinned version moves, re-copy these.** A license text is only correct for the version
it shipped with, and an upstream project can relicense between releases. Versions above must
match `THIRD-PARTY-NOTICES.md`.

## Not covered here

- **Sparkle** (macOS auto-update, MIT) — deliberately not in this folder, because everything
  here is copied to Windows and Linux too and Sparkle is macOS-only. Its `LICENSE` is staged
  into the app bundle as `sparkle-LICENSE.txt` at build time, copied from the resolved SwiftPM
  artifact so it tracks `mac/Package.resolved` automatically (`scripts/build-swift-host.sh`,
  which hard-fails if it is missing). See `THIRD-PARTY-NOTICES.md`.
- **Rust crates** — predominantly MIT/Apache-2.0. A generated inventory (`cargo about`) is still
  an open item; see `THIRD-PARTY-NOTICES.md`.
- **OS codecs** (Windows WIC / Media Foundation, macOS Image I/O + VideoToolbox) — components of
  the user's operating system. Nothing is redistributed, so nothing is licensed here.
- **Patents.** Orthogonal to these copyright licenses and not addressed by them.
