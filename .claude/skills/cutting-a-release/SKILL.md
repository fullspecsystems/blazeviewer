---
name: cutting-a-release
description: Cut and publish a Blaze Viewer release — Windows (Velopack), macOS (DMG + Sparkle), Linux (AppImage) — build, sign, upload, verify, plus every known release trap (vpk re-runs, cumulative feed, dirty-tree gate, YubiKey ssh, EdDSA key). Use whenever cutting, publishing, retrying, or debugging a release or the update feeds.
---

# Cutting a Blaze Viewer release

All three platforms build + sign **locally** (hosted GitHub Actions is too
expensive; the `v*`-tag auto-build was removed from release.yml). Read the
platform section you're releasing, then the numbered procedure at the end.

## Windows — Velopack

**Windows** ships **Velopack** (per-user installer + auto-update), built + signed **locally**:
`pwsh scripts/release-windows.ps1 -Upload` builds with libheif, signs the exe + `Setup.exe` +
`Update.exe` via Azure Trusted Signing, `vpk pack`s a full release, and rsyncs the flat feed to
`downloads.blazeviewer.app/win`. The app reads that feed over HTTP (`update.rs` `FEED_URL`) and
self-updates — downloads in the background, installs on quit. Version comes from
`crates/pb-app/Cargo.toml`, so it always matches the app; there is **no tag / GitHub Release for
Windows**.

*Architecture:* the script defaults to the host arch and takes `-Arch x64|arm64`. **x64** ships as the
historical `win` Velopack channel; **ARM64** as `win-arm64` — both land in the same flat feed dir, and
an install only ever auto-updates within its own channel (Velopack tracks the channel the app was
installed from, so `update.rs` needs no arch logic and the two never cross). Each arch is built on its
own **native** box (no cross toolchain wired up), after building that arch's native decode libs
(libheif, dav1d, **and FFmpeg** — tasks #76 / #100) once with `scripts/setup-libheif.ps1 -Triplet
<arch>-windows` — the **DLL** triplet, *not* `-static-md` (task #77: LGPL relink; a static build
cannot ship, and `release-windows.ps1` throws if it can't find `installed\<triplet>\bin\heif.dll`).
The script pins the vcpkg tree to a recorded commit (`-VcpkgRef`) and installs all three ports;
`pb-decode/build.rs` picks the vcpkg triplet from the target arch. The ship
feature set is `--features libheif,dav1d,ffprobe`. ARM64 uses the `vcredist143-arm64` redist framework.

> **`ffprobe` needs a VS Developer shell — it's the first feature that does.** FFmpeg's
> `bindgen` runs its own clang, which reads `INCLUDE` to find `stdint.h`; a plain `cargo build`
> never needed that, because rustc finds the MSVC linker itself. `scripts/vs-dev-env.ps1` handles
> it (release script + both CI lanes call it; it no-ops if you're already in a dev shell), and VS
> already ships the required libclang at `VC\Tools\Llvm\{x64,ARM64}\bin` — nothing extra to install.
> It also needs `VCPKG_ROOT` **exported**: the `vcpkg` crate `ffmpeg-sys-next` uses has no `~/vcpkg`
> fallback, unlike our own build.rs. FFmpeg here is **demux/metadata only** (MF still decodes
> everything) — the setup script patches the port to a trimmed build, which is the difference
> between **+3.06 MB** and +16.42 MB on the exe.

## macOS — DMG + Sparkle

**macOS** is **built locally on the owner's Mac** via `scripts/release-macos.sh` (Developer ID +
notarization), then published to `downloads.blazeviewer.app/mac` with
`scripts/release-mac-upload.sh` — which scp's the DMG + appcast **straight from the Mac** to
jdlien.com and repoints the `BlazeViewer-latest.dmg` symlink (the remote `mac/` dir is
jdlien-owned, no sudo). No Windows detour: `scripts/release-mac-upload.ps1` is the equivalent for
running the upload from the Windows box, but the whole Mac release now stays on the Mac. Hosted
GitHub Actions is too expensive to use, so `.github/workflows/release.yml` — which builds the DMG
on a hosted `macos-15` runner — is **`workflow_dispatch`-only (dormant)**; a `v*` tag no longer
auto-triggers it. A GitHub Release for the DMG, if wanted, is created manually. Signing setup is
in `.taskmaster/docs/release-signing.md`.

macOS **auto-updates via Sparkle** (task #65) — the in-app equivalent of Windows' Velopack. The
`.app` embeds `Sparkle.framework` (assembled by `build-swift-host.sh`, since a SwiftPM executable
has no Xcode "Embed Frameworks" phase) and reads an EdDSA-signed `appcast.xml` next to the DMG
(`SUFeedURL` in `Info-swift-host.plist`). `release-macos.sh` re-signs Sparkle's nested helpers with
the Developer ID (inside-out, before the app) and, after notarizing, EdDSA-signs the DMG and writes
`dist/appcast.xml` (`scripts/generate-mac-appcast.sh`); `release-mac-upload.ps1` publishes that
appcast alongside the DMG. The **private EdDSA signing key lives only in the release Mac's login
keychain** (generated once via Sparkle's `generate_keys`; the public `SUPublicEDKey` is committed in
the plist) — **back it up** (`generate_keys -x`); losing it means no future build can be signed for
auto-update without shipping a new public key via a stopgap manual update.

## Linux — AppImage

**Linux** ships a self-contained **AppImage** — one executable the user downloads, `chmod +x`es, and
runs; **no `apt install`, no dependency hunt.** Built locally with `scripts/release-linux.sh` (→
`dist/BlazeViewer-<version>-<arch>.AppImage`). It builds the full-feature release binary
(`--features livephoto,pb-decode/libheif`) and uses **linuxdeploy** (fetched to `dist/appimage-tools`)
to bundle the *specialized* decode libraries — libheif, FFmpeg, and the AV1/HEVC codecs — while
leaving the ~universal system stack (glibc, GTK, Mesa/GL, X11, Wayland) to the host, per the AppImage
excludelist. Two things linuxdeploy/`ldd` can't see are handled by the script: **libheif's dlopen'd
plugins** (`libheif-libde265.so` etc.) are copied into `usr/lib/libheif/plugins` with their own deps
(libde265/libaom/…), and a **custom `AppRun`** exports `LIBHEIF_PLUGIN_PATH` + `LD_LIBRARY_PATH` so
they resolve inside the bundle. Live Photo *audio* still needs `pw-cat` (PipeWire) on the user's PATH
— present on any modern desktop, degrades to silent motion if absent, so it's intentionally **not**
bundled. **Unsigned** (no Developer-ID/GPG equivalent yet), but it **does self-update** (below).
`dist/` is git-ignored, so the artifacts never get committed.

`release-linux.sh` builds for the **host arch**, so from a Mac/Windows box (no Linux VM needed) use
**`scripts/release-linux-docker.sh [amd64|arm64|both] [--upload]`** — it builds an **Ubuntu 26.04**
container (`scripts/appimage.Dockerfile`, matching the FFmpeg 8 / libheif 1.21 the code targets) and
runs `release-linux.sh` inside it. `both` builds x86_64 then aarch64; `--upload` publishes the
result afterwards (see below). On **Apple Silicon + OrbStack** `linux/amd64` runs under **Rosetta**,
so the **x86_64** artifact (what most Linux users need) builds at near-native speed; `arm64` is
native. It uses a container-only `CARGO_TARGET_DIR` (a cached volume) so it never clashes with the
host's macOS `target/`, and `APPIMAGE_EXTRACT_AND_RUN=1` so no FUSE/`--privileged` is required. The
build distro sets the glibc floor (2.43 here → recent-distro runtime); dropping it means building
FFmpeg/libheif from source on an older base. **AppImages can only be built on Linux** (the container
*is* that Linux) — there's no native macOS/Windows AppImage build. ⚠ The container build image
pre-installs the Rust toolchain via `rustup-init`, whose `--component` takes a **comma-separated**
list (`rustfmt,clippy,…`) — a space-separated list makes it reject the second component.

**Publishing + auto-update (Linux) — the JSON-feed self-replace model** (the Velopack/Sparkle analog
for AppImages). `scripts/release-linux-upload.sh` (or `release-linux-docker.sh … --upload`) scp's the
versioned AppImage(s) + a `.sha256` sidecar each to `downloads.blazeviewer.app/linux`, writes a
shared `latest.json` manifest (version + per-arch url/sha256/size), and repoints the
`BlazeViewer-latest-<arch>.AppImage` symlinks; Caddy redirects `/latest/linux` (x86_64) and
`/latest/linux-arm64` (aarch64) at them. The app's `update.rs` `linux` module reads
`latest.json` in a background thread, and if it advertises a newer build for this arch it downloads
the AppImage, **verifies the sha256**, and swaps `$APPIMAGE` in place on quit (atomic rename — the
next launch is the new version). Self-gates when `$APPIMAGE` is unset (a `cargo run` / extracted
binary) or the AppImage's directory isn't writable (installed read-only) — then it just stays put.
`PB_UPDATE_FEED` overrides the feed base URL for offline testing.

## The clean-tree gate (enforced, not remembered)

> `crates/pb-app/build.rs` stamps the build id `-dirty` on **any** `git status --porcelain`
> output — **untracked files included** — and that ships in the About dialog. Every release
> script refuses to run from a dirty tree; `scripts/release-preflight.sh` is the shared bash
> gate (release-windows.ps1 mirrors it inline — PowerShell can't source bash).
>
> ⚠️ **Why the gate is two-sided — a pre-flight `git status` is NOT enough.** Bumping
> `crates/pb-app/Cargo.toml` changes `pb-app`'s entry in `Cargo.lock`, but *nothing rewrites
> the lockfile until a cargo command runs* — which is the release build itself. So a tree that
> is genuinely clean when checked goes dirty **mid-build**, and the DMG ships
> `0.2.1 (abc1234-dirty)` having been verified clean minutes earlier (hit on 0.2.1,
> 2026-07-14). Hence:
> 1. **`release_preflight`** runs `cargo metadata` *first* to settle the lockfile, *then*
>    checks — turning that mid-build rewrite into an up-front, actionable failure.
> 2. **`assert_build_id_clean` / `assert_tree_clean_after_build`** run *after* the build and
>    assert what was actually stamped, so causes we haven't thought of still get caught.
>    Placed **before** codesign/notarize, so a doomed build never costs an Apple round-trip.
>
> Both honour an escape hatch — `--allow-dirty` (mac), `PB_ALLOW_DIRTY=1` (linux),
> `-AllowDirty` (windows) — for a deliberate throwaway build. **Never for a real release:** it
> only downgrades the abort to a warning; the artifact is still stamped `-dirty`.

> **Never let a tool auto-invoke a paid CI run.** Hosted runners cost real money (a macOS run is
> billed at 10×), so releases are scripted and run locally — the `v*`-tag trigger was removed from
> release.yml precisely so a tag push (or `gh release create`) can't quietly start a hosted build.
> Don't re-add an automatic hosted trigger; script the build instead.

## The procedure

1. **Roll the `CHANGELOG.md`.** Move `## [Unreleased]` into `## [<version>] - <YYYY-MM-DD>`,
   leave a fresh empty `[Unreleased]`, and update the compare links at the bottom. The crate
   version (`crates/pb-app/Cargo.toml`) must match the tag's numeric core (a `-beta.N` suffix
   lives only on the tag). **Write a `### Highlights` block** (a ~7-line, plain-English "what's
   new" — regular users, not contributors) as the first subsection of the version, above
   `### Added` — the macOS **Sparkle** update dialog shows *only* that block
   (`generate-mac-appcast.sh` extracts it; it falls back to the whole section if absent), while the
   full `Added/Changed/Fixed` detail stays in the file for the curious and the GitHub release body.
2. **Windows:** `pwsh scripts/release-windows.ps1 -Upload` from this machine (with `.env.release`
   signing creds). **Run it from native PowerShell, not the Bash tool / Git Bash** — the ssh
   config's YubiKey `Match exec` hook has a Windows path that Git Bash mangles, so the upload fails
   `Permission denied (publickey)`. The build + sign + pack still succeed there; only the `-Upload`
   scp/rsync needs native PowerShell.
   > ⚠️ **A re-run is NOT upload-only.** `-Upload` is the last step of the *whole* pipeline, and
   > `vpk pack` **hard-fails** on a second run — *"There is a release in channel win which is equal
   > or greater to the current version"* — because the version it just packed is sitting in
   > `dist\feed`. So if the pack succeeded and only the upload failed, do **not** re-run the script:
   > `scp` the already-signed feed yourself (`cd dist\feed; scp * jdlien.com:/var/www/downloads.blazeviewer.app/win/`).
   > Re-running means clearing `dist\feed` first, which re-signs everything for no gain (hit on 0.2.1).

   > ⚠️ **`dist\feed` is a *cumulative* feed, and `vpk` merges whatever it finds there — including a
   > different product.** On 0.2.1 the dir still held the PhotoBlaze packages, so `releases.win.json`
   > advertised PhotoBlaze 0.1.0/0.1.1/0.2.0 *beside* BlazeViewer 0.2.1 and vpk built a delta **across
   > the packId rename** (PhotoBlaze 0.2.0 → BlazeViewer 0.2.1). Upload sends the whole directory, so
   > that would have published the old product to the new feed. `vpk` keys deltas on channel+version,
   > **not packId**. Check `dist\feed` holds only this product before packing.

   Prune superseded packages on the server periodically.
3. **macOS (all on your Mac):** `./scripts/release-macos.sh --release` builds the signed +
   notarized DMG **and** EdDSA-signs it into `dist/appcast.xml` (Sparkle auto-update, task #65),
   then `./scripts/release-mac-upload.sh` scp's the DMG **and the appcast** to jdlien.com and
   repoints `BlazeViewer-latest.dmg` — no Windows box needed. (Optionally verify the seed's updater
   first with `./scripts/test-sparkle-update.sh dist/BlazeViewer-<version>.dmg`.) A GitHub Release is
   **optional and manual** — nothing auto-builds from a tag:
   `gh release create v<version> dist/BlazeViewer-<version>.dmg* --notes-file <(bash
   scripts/changelog-section.sh <version>)`. Write **real, curated, user-facing** CHANGELOG notes
   before tagging so `changelog-section.sh` has a body. **Never** enable `generate_release_notes`.
4. **Linux (from your Mac via OrbStack):** `./scripts/release-linux-docker.sh both --upload` builds
   both AppImages and publishes them + `latest.json` to the feed (repointing the `latest-<arch>`
   symlinks). Needs your ssh keys for the scp step (it runs host-side, after the container work). A
   launched older AppImage then self-updates on next quit.
5. **Tag for posterity** (optional): `git tag -a v<version> -m "…" && git push origin v<version>`.
   Windows never needs it (Velopack reads the version from `Cargo.toml`); it's a record + the anchor
   for a manual macOS GitHub Release. Safe to push now that release.yml doesn't auto-build on tags.
6. **Verify:** the Windows feed serves the new `releases.win.json` + `.nupkg` (and a launched build
   self-updates); the macOS DMG is genuinely notarized — `xcrun stapler validate <dmg>` and
   `spctl -a -t open --context context:primary-signature -vv <dmg>` → `source=Notarized Developer
   ID`; the macOS feed serves the new `appcast.xml` (curl it) and a launched older build detects →
   downloads → installs-on-quit the update; the Linux feed serves the new `latest.json` +
   `latest-<arch>` symlinks (curl `…/latest/linux`). A `-` in a tag marks a pre-release; a
   clean `vX.Y.Z` is a full release.
