# #141 — Windows FFmpeg 8.1.2 → 8.1.3 (security point release)

Owner request 2026-11-13, relayed by the Mac session: the Windows half of the macOS bump in
ec8d6f87. 8.1.2 and 8.1.3 are bounds/overflow/leak fixes backported from 9.0.x, several in code we
run on untrusted files. Windows' trimmed build includes the ogg demuxer, so the oggdec fixes
apply here too. This is not a release; shipping it is a later owner call.

## Route

Stay on 8.1.x and patch the pinned vcpkg ffmpeg port in place. Do **not** move `-VcpkgRef`.
Upstream vcpkg went from 8.1.2 straight to 9.0.x. Bumping the ref would be a soname major
(avcodec-62 → 63 …) and would also move libheif / libde265 / dav1d. 9.x is a separate
both-platform project.

- **The pin:** `scripts/ffmpeg-windows.psd1` (`Version`, `Sha512`). It is read by two scripts.
- **`setup-libheif.ps1` step 5a** rewrites `ports\ffmpeg\vcpkg.json` `version` and the portfile's
  `SHA512` (the port downloads `REF "n${VERSION}"`). It throws on a missing anchor, and on a pin
  in a different minor than the port (that would not be a point bump).
- **Step 5c** removes the installed ffmpeg whenever this run changed the port, *or* the
  installed `include\libavutil\ffversion.h` is not the pin. That is the classic-mode trap: an
  install over an installed ffmpeg is a no-op, so without it a box silently keeps 8.1.2. After
  the install it throws if `ffversion.h` still isn't the pin.
- **The trim guard now actually guards.** On the DLL triplets `avcodec.lib` is an import
  library (a few KB), so the old "over 40 MB" check could never fire. It now sizes
  `avcodec-*.dll` (6 MB limit), falling back to the lib on a static tree.
- **`release-windows.ps1`** refuses to package an installed FFmpeg that is not the pin. Each
  arch builds on its own box, and the pin arrives by `git pull` while the DLLs only change
  when setup re-runs.

## Handoff

**Verified (Windows x64, 2026-11-13) — source:**
- `git ls-remote`: `refs/tags/n8.1.3` is tag `23151b11` → commit `1041abdc` on **both**
  github.com/FFmpeg/FFmpeg and git.ffmpeg.org/ffmpeg.git.
- `ffmpeg-8.1.3.tar.xz` from ffmpeg.org: sha256 `7138d28c…78a3`, identical to the macOS pin.
  `gpg --verify` with an isolated keyring and `ffmpeg-devel.asc` gave "Good signature from
  FFmpeg release signing key", primary key `FCF9 86EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8`.
- GitHub's `archive/n8.1.3.tar.gz` (what vcpkg fetches; SHA512 `8c667090…aafe`) extracted and
  `diff -r` against the signed tarball: every common file identical. The archive adds only
  `.gitignore` / `.gitattributes`, and the tarball adds only `VERSION`.
- All 14 patches in `ports\ffmpeg\` apply to the 8.1.3 tree in portfile order, with vcpkg's
  `git apply --ignore-whitespace` flags. That includes `0048-backport-23039`, so 8.1.3 does
  not already contain it.
- `licenses/ffmpeg-COPYING.LGPLv2.1.txt` is identical to 8.1.3's `COPYING.LGPLv2.1` apart from
  CRLF.

**Verified (Windows x64, 2026-11-13) — setup script:**
- First run on `~\vcpkg`: pinned 8.1.2 → 8.1.3, removed the installed 8.1.2, and built 8.1.3
  from the GitHub archive (all patches applied; the same trimmed configure line) in 3.6 min.
- Second run: a no-op (nothing patched, removed or rebuilt).
- Stale install under an already-patched port: faked by setting the installed `ffversion.h` to
  8.1.2. Setup removed it and restored 8.1.3 from the binary cache, and the version guard passed.
- **This box has two vcpkg trees, and the one that matters is `C:\vcpkg-pb`.** CI
  (`VCPKG_ROOT=C:\vcpkg-pb`), `build-windows.ps1` and `release-windows.ps1` all prefer it, while
  setup defaulted to `~\vcpkg`. So the first app build still linked 8.1.2 from `C:\vcpkg-pb`.
  Setup's default now uses the same order (VCPKG_ROOT → `C:\vcpkg-pb` → `~\vcpkg`) and prints
  the tree it is using. A bare re-run updated `C:\vcpkg-pb` to 8.1.3: it restored the package
  just built (same ABI hash), and its trim check passed. Both trees are now 8.1.3.
- Release guard logic (the same lines run standalone): passes `C:\vcpkg-pb` x64-windows (8.1.3),
  refuses a stale tree (8.1.2) and a missing one ("unknown").

**Verified (Windows x64, 2026-11-13) — app build against `C:\vcpkg-pb`:**
- `build-windows.ps1` (libheif,dav1d,ffprobe) builds. The DLLs it copies beside the exe were
  loaded and asked at run time: `av_version_info` **8.1.3**, avutil 60.26.103 / avcodec
  62.28.103 / avformat 62.12.103 (the same as the Mac build), `avcodec_license` "LGPL version
  2.1 or later". The same holds for the copies in `target\debug\deps` that the tests load.
- Trim intact: decoders ac3, eac3, dca, truehd, aac, opus, vorbis, flac, subrip, ass present;
  h264 / hevc / av1 absent. Demuxers matroska, mov, ogg, avi, mpegts present; srt absent.
  `avcodec-62.dll` 1.94 MB (was 2.0 MB at 8.1.2).
- dumpbin import closure of `blazeviewer.exe`: heif, libde265, dav1d, avcodec-62, avformat-62,
  avutil-60, swresample-6 (via avcodec). The same 7 DLLs, no swscale.
- `cargo test -p pb-decode --features ffprobe`: 366 passed, 10 ignored. That includes 8
  `ffmpeg::tracks`, 22 `ffmpeg::audio_decoder` (two decode the AC-3 5.1 track of
  `multitrack.mkv`), and 6 `ffmpeg::cues`. The `ffmpeg::details` tests compile only under
  `ffvideo`, so they are Mac/Linux-only.
- `cargo test -p pb-app-core --features ffprobe`: 1030 passed, 5 ignored, including the 5
  `media_details` tests that build the Details pane data for `multitrack.mkv`.

**Not verified:**
- **By hand on x64:** a real film with AC-3 or DTS audio still has sound, and the Details pane
  on `multitrack.mkv` looks the same. AC-3 decode is covered by tests above. DTS has no fixture;
  only "the dca decoder is in the DLL" was checked. Owner check with `pwsh
  scripts/build-windows.ps1 -Run`.
- **ARM64.** The VM must `git pull` and re-run `pwsh scripts/setup-libheif.ps1 -Triplet
  arm64-windows` before its next release. `release-windows.ps1 -Arch arm64` refuses to package
  until it does. Subtask 141.4.
- `x64-windows-static-md` still holds ffmpeg 8.1.2 in both trees. Nothing links it:
  `ffmpeg-sys-next` always takes the DLL triplet, even under `PB_VCPKG_STATIC=1`. It is left
  alone.
- A bare `cargo build` with `VCPKG_ROOT` unset falls back to `~\vcpkg` (`build.rs`,
  `vs-dev-env.ps1`), not `C:\vcpkg-pb`. Every script and CI sets `VCPKG_ROOT`, and both trees
  here are 8.1.3, so this was left as is.

**Cross-platform debt:** none. macOS has been on 8.1.3 since ec8d6f87.

**Decisions / corrections:**
- 2026-11-13: route chosen by the Mac session's recommendation (owner did not pick one): patch
  the port in place, keep `-VcpkgRef`.
- 2026-11-13: `.taskmaster/docs/ffmpeg-compliance.md` still described Windows FFmpeg as a
  **static** link with an unmet §6 obligation. That has been false since task #77 (DLLs,
  validated in 0.2.1). It is corrected in the same change.
- 2026-11-13: the old "avcodec.lib over 40 MB" trim guard could never fire on the DLL triplet,
  where avcodec.lib is a few-KB import library. It now sizes `avcodec-*.dll`.
- 2026-11-13: corrected a stale comment in the trim block. It said nothing decodes audio with
  FFmpeg, but playback decodes AC-3 / E-AC-3 / DTS FFmpeg-first.

**Claimed:** nothing. The Windows session finished 141.1–141.3 and 141.5. 141.4 (ARM64) is open
for whichever session next has the ARM64 VM.
