<#
.SYNOPSIS
  Phase 0 for the libheif HEIC decode backend (see .taskmaster/docs/heic-decode-plan.md).
  Bootstraps vcpkg (if needed) and builds a decode-only, plugin-loader-free static
  libheif that `pb-decode/build.rs` links when built with `--features libheif`.

.DESCRIPTION
  Idempotent — safe to re-run (e.g. after a `git pull` in the vcpkg tree, which
  reverts the portfile patch). It:
    1. Clones vcpkg at $VCPKG_ROOT (default: ~/vcpkg) if absent, pins the tree to the
       recorded known-good commit (-VcpkgRef; carries libheif 1.23.0, libde265 1.1.1,
       dav1d 1.5.3), and bootstraps the matching vcpkg.exe. The pin makes port versions
       — and the C ABI our FFI builds against — reproducible across boxes (task #76).
       FFmpeg is the one exception: step 5 patches its port in place to the version pinned
       in ffmpeg-windows.psd1 (task #141) and rebuilds it whenever the installed copy differs.
    2. Patches the libheif portfile to add -DENABLE_PLUGIN_LOADING=OFF. This kills
       the dynamic-plugin scanner (which on Windows spams ~one LoadLibraryA per HEVC
       grid tile, ~98 per iPhone image, all failing) at the source. The statically
       linked libde265 is unaffected — it registers via the static path, not the
       scanner. Matches the "single self-contained binary, no loose DLLs" stance.
    3. Installs libheif[core]:x64-windows-static-md — `core` drops the x265 *encoder*
       default; libde265 (HEVC *decode*) is a hard dep so it's always present.
       static-md = static libs + dynamic CRT, matching Rust MSVC's default CRT.

  Prerequisite: MSVC C++ build tools (VS 2022 / Build Tools with the C++ workload).

  Architecture: -Triplet selects the vcpkg triplet, which must match the build target
  (pb-decode/build.rs derives it from CARGO_CFG_TARGET_ARCH). Build each arch you ship on
  its own native box: x64-windows-static-md (default) on x64, arm64-windows-static-md on ARM64.

.EXAMPLE
  pwsh scripts/setup-libheif.ps1                                # x64 (default)
  pwsh scripts/setup-libheif.ps1 -Triplet arm64-windows-static-md   # native ARM64
  # then:  cargo run -p pb-app --release --features libheif -- "<folder>" -r
#>
[CmdletBinding()]
param(
    # Same preference order as build-windows.ps1 / release-windows.ps1 (and CI's VCPKG_ROOT):
    # VCPKG_ROOT, then C:\vcpkg-pb when it exists, then ~/vcpkg. Defaulting straight to ~/vcpkg
    # updated a tree the build never read on a box that has both (hit by task #141).
    [string]$VcpkgRoot = $(if ($env:VCPKG_ROOT) { $env:VCPKG_ROOT } elseif (Test-Path "C:\vcpkg-pb\.git") { "C:\vcpkg-pb" } else { "$env:USERPROFILE\vcpkg" }),
    # DLLs, not static libs — an LGPL requirement, not a preference (task #77): libheif +
    # libde265 (LGPL-3.0 §4) and FFmpeg (LGPL-2.1 §6) oblige us to let a user relink the app
    # against a modified library, which a static link into a proprietary binary does not allow.
    # vcpkg's plain `x64-windows` / `arm64-windows` triplets are the DLL ones; the historical
    # `-static-md` default is the non-compliant configuration and must not ship.
    [string]$Triplet = "x64-windows",
    # The recorded known-good vcpkg commit (2026-06-26 tip: libheif 1.23.0, libde265 1.1.1,
    # dav1d 1.5.3). Override to move the pin deliberately — then update this default so every
    # box (x64, ARM64, CI) builds the same port versions.
    [string]$VcpkgRef = "a0400024711b283056538ac19ced80b91a83c24c"
)
$ErrorActionPreference = "Stop"
Write-Host "vcpkg tree: $VcpkgRoot ($Triplet)" -ForegroundColor Cyan

# 1. vcpkg present?
if (-not (Test-Path "$VcpkgRoot\.git")) {
    Write-Host "Cloning vcpkg into $VcpkgRoot ..." -ForegroundColor Cyan
    git clone --depth 1 https://github.com/microsoft/vcpkg.git $VcpkgRoot
}

# 1b. Pin the tree to the recorded known-good commit — a bare clone tracks the moving tip, so
# without this no ABI/version assumption is reproducible across boxes. `checkout -f` discards
# the portfile patch; step 2 re-applies it. Drop vcpkg.exe on a ref change so the bootstrap
# below rebuilds the tool version the tree expects.
$head = git -C $VcpkgRoot rev-parse HEAD
if ($head -ne $VcpkgRef) {
    Write-Host "Pinning vcpkg to $VcpkgRef (was $head) ..." -ForegroundColor Cyan
    git -C $VcpkgRoot fetch --depth 1 origin $VcpkgRef
    git -C $VcpkgRoot checkout -f $VcpkgRef
    if (Test-Path "$VcpkgRoot\vcpkg.exe") { Remove-Item "$VcpkgRoot\vcpkg.exe" -Force }
}

if (-not (Test-Path "$VcpkgRoot\vcpkg.exe")) {
    Write-Host "Bootstrapping vcpkg ..." -ForegroundColor Cyan
    & "$VcpkgRoot\bootstrap-vcpkg.bat" -disableMetrics
}

# 2. Patch the portfile (idempotent).
$portfile = "$VcpkgRoot\ports\libheif\portfile.cmake"
if (-not (Test-Path $portfile)) { throw "libheif portfile not found at $portfile" }
$content = Get-Content $portfile -Raw
if ($content -match 'ENABLE_PLUGIN_LOADING') {
    Write-Host "Portfile already patched (ENABLE_PLUGIN_LOADING present)." -ForegroundColor DarkGray
} else {
    $anchor = '-DPLUGIN_DIRECTORY=  # empty'
    if ($content -notmatch [regex]::Escape($anchor)) {
        throw "Could not find the '-DPLUGIN_DIRECTORY=' anchor in $portfile; upstream layout changed. Add '-DENABLE_PLUGIN_LOADING=OFF' to the vcpkg_cmake_configure OPTIONS manually."
    }
    $patched = $content -replace [regex]::Escape($anchor), "$anchor`r`n        -DENABLE_PLUGIN_LOADING=OFF  # PhotoBlaze: static libde265 only; kills the per-tile LoadLibraryA scan"
    Set-Content -Path $portfile -Value $patched -NoNewline
    Write-Host "Patched portfile: added -DENABLE_PLUGIN_LOADING=OFF" -ForegroundColor Green
    # Force a rebuild even if a binary cache hit exists for the old port hash.
    & "$VcpkgRoot\vcpkg.exe" remove "libheif:$Triplet" --disable-metrics 2>$null
}

# 3. Install (decode-only).
Write-Host "Installing libheif[core]:$Triplet ..." -ForegroundColor Cyan
& "$VcpkgRoot\vcpkg.exe" install "libheif[core]:$Triplet" --disable-metrics

# 4. dav1d — the AV1 decoder for animated AVIF playback (task #76; pb-decode's
# `dav1d` feature links it and compiles the C shim against this tree's headers).
Write-Host "Installing dav1d:$Triplet ..." -ForegroundColor Cyan
& "$VcpkgRoot\vcpkg.exe" install "dav1d:$Triplet" --disable-metrics

# 5. FFmpeg — container reading ONLY (task #100). Media Foundation keeps every decode;
# FFmpeg is here because MF's media sources cannot enumerate subtitle tracks at all (a
# source's presentation descriptor reports 3 streams for a file holding 7). The port is
# patched to a demux/metadata build: no video decoders, no encoders, no network. That is
# the difference between +3.06 MB and +16.42 MB on the shipped exe (measured).
$ffPortfile = "$VcpkgRoot\ports\ffmpeg\portfile.cmake"
$ffManifest = "$VcpkgRoot\ports\ffmpeg\vcpkg.json"
foreach ($f in $ffPortfile, $ffManifest) { if (-not (Test-Path $f)) { throw "ffmpeg port file not found at $f" } }
$ffRebuild = $false

# 5a. Pin the FFmpeg version (task #141). The port at -VcpkgRef is 8.1.2, and upstream vcpkg
# jumped from there to 9.0.x (a soname major), so a security point release is taken by
# patching this port in place rather than by moving the ref. The pin and how it was vetted
# live in ffmpeg-windows.psd1. The port downloads `REF "n${VERSION}"`, so the version in
# vcpkg.json picks the tag and SHA512 vouches for the archive.
$ffPin = Import-PowerShellDataFile "$PSScriptRoot\ffmpeg-windows.psd1"
$ffJson = Get-Content $ffManifest -Raw
$ffVer = [regex]::Match($ffJson, '(?m)^  "version": "([^"]+)",')
if (-not $ffVer.Success) { throw "Could not find the top-level `"version`" in $ffManifest; upstream layout changed. Re-derive the pin from task #141." }
if ($ffVer.Groups[1].Value -ne $ffPin.Version) {
    # Only a same-minor point bump is safe here: the port's patches and our trim were written
    # for 8.1. A ref that moved the port to another minor needs task #141 redone, not this.
    $minor = { param($v) ($v -split '\.')[0..1] -join '.' }
    if ((& $minor $ffVer.Groups[1].Value) -ne (& $minor $ffPin.Version)) {
        throw "The vcpkg ffmpeg port is $($ffVer.Groups[1].Value) but ffmpeg-windows.psd1 pins $($ffPin.Version). Pinning across a minor is not a point bump; revisit task #141."
    }
    $ffJson = $ffJson.Remove($ffVer.Groups[1].Index, $ffVer.Groups[1].Length).Insert($ffVer.Groups[1].Index, $ffPin.Version)
    Set-Content -Path $ffManifest -Value $ffJson -NoNewline
    Write-Host "Pinned ffmpeg port version: $($ffVer.Groups[1].Value) -> $($ffPin.Version)" -ForegroundColor Green
    $ffRebuild = $true
}
$ffContent = Get-Content $ffPortfile -Raw
$ffSha = [regex]::Match($ffContent, 'REF "n\$\{VERSION\}"\s+SHA512 ([0-9a-f]{128})')
if (-not $ffSha.Success) { throw "Could not find the 'REF `"n`${VERSION}`" / SHA512' anchor in $ffPortfile; upstream layout changed. Re-derive the pin from task #141." }
if ($ffSha.Groups[1].Value -ne $ffPin.Sha512) {
    $ffContent = $ffContent.Remove($ffSha.Groups[1].Index, 128).Insert($ffSha.Groups[1].Index, $ffPin.Sha512)
    Set-Content -Path $ffPortfile -Value $ffContent -NoNewline
    Write-Host "Pinned ffmpeg source SHA512 for n$($ffPin.Version)" -ForegroundColor Green
    $ffRebuild = $true
}

# 5b. Trim the build to demux/metadata.
if ($ffContent -match 'PhotoBlaze task #100') {
    Write-Host "ffmpeg portfile already patched (demux-only)." -ForegroundColor DarkGray
} else {
    $ffAnchor = 'set(OPTIONS "--enable-pic --disable-doc --enable-runtime-cpudetect --disable-autodetect")'
    if ($ffContent -notmatch [regex]::Escape($ffAnchor)) {
        throw "Could not find the base OPTIONS anchor in $ffPortfile; upstream layout changed. Re-derive the trim from task #100.5 and apply it to the configure OPTIONS manually."
    }
    # The audio decoders are load-bearing twice. Playback decodes audio FFmpeg-first (the
    # WASAPI engine, pb_decode::ffmpeg::audio_decoder), because MF cannot decode AC-3 /
    # E-AC-3 / DTS at all: drop them and most films play silent. And avformat_find_stream_info
    # opens each audio codec to read its config, which is the only source of a NAMED channel
    # layout; without them a 5.1 track reads "6 channels". They are keyed to the codecs
    # pb_decode::tracks::audio_codec_display already knows how to name.
    #
    # SUBTITLES ARE ASYMMETRIC ON PURPOSE (task #100.8): the subtitle DECODERS are enabled but
    # the subtitle DEMUXERS (srt/webvtt/ass) are not. Cues inside a container therefore work
    # (mov+movtext, matroska+subrip/ass/webvtt), while a STANDALONE .srt/.vtt/.ass sidecar
    # cannot be opened at all -- a decoder turns packets into cues, it cannot open a file.
    # That is a real constraint on #90's sidecar tier, and Windows-only (Linux/macOS link a
    # full FFmpeg). If #90 chooses to open sidecars via FFmpeg rather than parse them in Rust,
    # add `--enable-demuxer=srt,webvtt,ass` here and re-measure. See #100.8.
    $ffTrim = @'
# --- PhotoBlaze task #100: demux/metadata-only FFmpeg -----------------------
string(APPEND OPTIONS " --disable-everything --disable-network --disable-encoders"
       " --disable-muxers --disable-devices --disable-filters --disable-programs"
       " --disable-mediafoundation --disable-d3d11va --disable-d3d12va --disable-dxva2"
       " --enable-protocol=file"
       " --enable-demuxer=matroska,mov,avi,asf,mpegts,mp3,flac,ogg,wav,aac,ac3"
       " --enable-decoder=subrip,ass,ssa,movtext,webvtt,text"
       " --enable-decoder=aac,aac_latm,ac3,eac3,dca,truehd,mlp,flac,alac,opus,vorbis"
       " --enable-decoder=mp1,mp1float,mp2,mp2float,mp3,mp3float"
       " --enable-decoder=wmav1,wmav2,wmapro,wmalossless,amrnb,amrwb"
       " --enable-decoder=pcm_s16le,pcm_s16be,pcm_s24le,pcm_s24be,pcm_s32le,pcm_f32le"
       " --enable-decoder=pcm_f64le,pcm_u8,pcm_alaw,pcm_mulaw,pcm_bluray,pcm_dvd"
       " --enable-decoder=adpcm_ima_qt,adpcm_ms"
       " --enable-parser=h264,hevc,aac,aac_latm,ac3,dca,flac,opus,vorbis,mpegaudio"
       " --enable-parser=vp8,vp9,av1,mpeg4video,mpegvideo,vc1,webp,png,mjpeg"
       " --enable-bsf=extract_extradata,aac_adtstoasc,h264_mp4toannexb,hevc_mp4toannexb"
       " --enable-bsf=vp9_superframe,av1_frame_split,mpeg4_unpack_bframes,vp9_superframe_split")
# ---------------------------------------------------------------------------
'@
    $ffPatched = $ffContent -replace [regex]::Escape($ffAnchor), "$ffAnchor`r`n$ffTrim"
    Set-Content -Path $ffPortfile -Value $ffPatched -NoNewline
    Write-Host "Patched ffmpeg portfile: demux/metadata-only build" -ForegroundColor Green
    $ffRebuild = $true
}

# 5c. vcpkg classic mode only asks "is it installed?" -- it never re-evaluates a changed port,
# so an install over an existing ffmpeg is a no-op that silently keeps the OLD build: the
# untrimmed one after the first trim, or the previous version after a pin bump. Remove it
# whenever this run changed the port, and also whenever what is installed is not the pin
# (a box whose port was patched by an earlier run that never got as far as the install).
$ffVersionH = "$VcpkgRoot\installed\$Triplet\include\libavutil\ffversion.h"
function Get-InstalledFfmpegVersion {
    if (-not (Test-Path $ffVersionH)) { return $null }
    if ((Get-Content $ffVersionH -Raw) -match 'FFMPEG_VERSION "([^"]+)"') { return $Matches[1] }
    return "unknown"
}
$ffInstalled = Get-InstalledFfmpegVersion
if ($ffInstalled -and ($ffRebuild -or $ffInstalled -ne $ffPin.Version)) {
    Write-Host "Removing installed ffmpeg:$Triplet ($ffInstalled) so the pinned $($ffPin.Version) build replaces it ..." -ForegroundColor Cyan
    & "$VcpkgRoot\vcpkg.exe" remove "ffmpeg:$Triplet" --disable-metrics 2>$null
}
Write-Host "Installing ffmpeg[core,avcodec,avformat,swresample,swscale]:$Triplet ..." -ForegroundColor Cyan
& "$VcpkgRoot\vcpkg.exe" install "ffmpeg[core,avcodec,avformat,swresample,swscale]:$Triplet" --disable-metrics

$libdir = "$VcpkgRoot\installed\$Triplet\lib"
foreach ($lib in "heif.lib", "dav1d.lib", "avcodec.lib", "avformat.lib") {
    if (-not (Test-Path "$libdir\$lib")) { throw "Install reported success but $libdir\$lib is missing." }
}
# The version check is fatal: it is the bump trap above, and a release must not ship it.
$ffInstalled = Get-InstalledFfmpegVersion
if ($ffInstalled -ne $ffPin.Version) {
    throw "ffmpeg:$Triplet installed as '$ffInstalled' but ffmpeg-windows.psd1 pins $($ffPin.Version). Run: $VcpkgRoot\vcpkg.exe remove ffmpeg:$Triplet, then re-run this script."
}
Write-Host "FFmpeg $ffInstalled installed for $Triplet." -ForegroundColor Green
# A guard against the trim trap above. Size the DLL when there is one: on the DLL triplets we
# ship, avcodec.lib is only an import library, so its size says nothing about the trim.
# Measured trimmed sizes: avcodec-62.dll ~2.0 MB (2026-11-13); static avcodec.lib ~23 MB vs
# ~131 MB untrimmed (2026-07-14). The untrimmed DLL was not measured; 6 MB is 3x the trimmed one.
$avcodecDll = Get-ChildItem "$VcpkgRoot\installed\$Triplet\bin" -Filter "avcodec-*.dll" -ErrorAction SilentlyContinue | Select-Object -First 1
$avcodecFile, $limitMB, $expect = if ($avcodecDll) { $avcodecDll, 6, "~2 MB" } else { (Get-Item "$libdir\avcodec.lib"), 40, "~23 MB" }
$avcodecMB = [math]::Round($avcodecFile.Length / 1MB, 1)
if ($avcodecMB -gt $limitMB) {
    Write-Host "WARNING: $($avcodecFile.Name) is $avcodecMB MB - expected $expect for the demux-only build." -ForegroundColor Yellow
    Write-Host "         The portfile patch likely did not take effect. Run:" -ForegroundColor Yellow
    Write-Host "         $VcpkgRoot\vcpkg.exe remove ffmpeg:$Triplet   # then re-run this script" -ForegroundColor Yellow
} else {
    Write-Host "Trim check: $($avcodecFile.Name) is $avcodecMB MB (demux-only)." -ForegroundColor DarkGray
}
Write-Host "`nNative decode libs ready: $libdir\{heif,dav1d,avcodec,avformat}.lib" -ForegroundColor Green
Write-Host "Set VCPKG_ROOT=$VcpkgRoot (or keep it at ~/vcpkg) and build with --features libheif,dav1d,ffprobe." -ForegroundColor Green
Write-Host "ffprobe (FFmpeg) additionally needs, at BUILD time only:" -ForegroundColor Green
Write-Host "  * LIBCLANG_PATH -> a libclang for bindgen. Visual Studio already ships one:" -ForegroundColor Green
Write-Host "      <VS>\VC\Tools\Llvm\{x64,ARM64}\bin   (no separate LLVM install needed)" -ForegroundColor Green
Write-Host "  * a Developer shell (Enter-VsDevShell / vcvars): bindgen's clang reads INCLUDE" -ForegroundColor Green
Write-Host "    to find stdint.h. A plain cargo build does not need this; bindgen does." -ForegroundColor Green
