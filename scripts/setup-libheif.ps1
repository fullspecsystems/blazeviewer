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
    [string]$VcpkgRoot = $(if ($env:VCPKG_ROOT) { $env:VCPKG_ROOT } else { "$env:USERPROFILE\vcpkg" }),
    [string]$Triplet = "x64-windows-static-md",
    # The recorded known-good vcpkg commit (2026-06-26 tip: libheif 1.23.0, libde265 1.1.1,
    # dav1d 1.5.3). Override to move the pin deliberately — then update this default so every
    # box (x64, ARM64, CI) builds the same port versions.
    [string]$VcpkgRef = "a0400024711b283056538ac19ced80b91a83c24c"
)
$ErrorActionPreference = "Stop"

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
if (-not (Test-Path $ffPortfile)) { throw "ffmpeg portfile not found at $ffPortfile" }
$ffContent = Get-Content $ffPortfile -Raw
if ($ffContent -match 'PhotoBlaze task #100') {
    Write-Host "ffmpeg portfile already patched (demux-only)." -ForegroundColor DarkGray
} else {
    $ffAnchor = 'set(OPTIONS "--enable-pic --disable-doc --enable-runtime-cpudetect --disable-autodetect")'
    if ($ffContent -notmatch [regex]::Escape($ffAnchor)) {
        throw "Could not find the base OPTIONS anchor in $ffPortfile; upstream layout changed. Re-derive the trim from task #100.5 and apply it to the configure OPTIONS manually."
    }
    # The audio decoders look droppable and are NOT: nothing decodes audio with them
    # (MF does), but avformat_find_stream_info opens each audio codec to read its config,
    # which is the only source of a NAMED channel layout. Without them a 5.1 track reads
    # "6 channels" -- i.e. straight back to MF's limitation. They are keyed to the codecs
    # pb_decode::tracks::audio_codec_display already knows how to name.
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
    # vcpkg classic mode only asks "is it installed?" -- it never re-evaluates a changed
    # portfile, so without this an install over an existing ffmpeg is a no-op that
    # silently keeps the UNTRIMMED build. Same reason libheif is removed above.
    & "$VcpkgRoot\vcpkg.exe" remove "ffmpeg:$Triplet" --disable-metrics 2>$null
}
Write-Host "Installing ffmpeg[core,avcodec,avformat,swresample,swscale]:$Triplet ..." -ForegroundColor Cyan
& "$VcpkgRoot\vcpkg.exe" install "ffmpeg[core,avcodec,avformat,swresample,swscale]:$Triplet" --disable-metrics

$libdir = "$VcpkgRoot\installed\$Triplet\lib"
foreach ($lib in "heif.lib", "dav1d.lib", "avcodec.lib", "avformat.lib") {
    if (-not (Test-Path "$libdir\$lib")) { throw "Install reported success but $libdir\$lib is missing." }
}
# A guard against the trap above: the untrimmed avcodec.lib is ~131 MB, the trimmed one
# ~23 MB (measured 2026-07-14 from this exact recipe). If a stale full build survived,
# say so rather than silently ship it.
$avcodecMB = [math]::Round((Get-Item "$libdir\avcodec.lib").Length / 1MB, 1)
if ($avcodecMB -gt 40) {
    Write-Host "WARNING: avcodec.lib is $avcodecMB MB - expected ~23 MB for the demux-only build." -ForegroundColor Yellow
    Write-Host "         The portfile patch likely did not take effect. Run:" -ForegroundColor Yellow
    Write-Host "         $VcpkgRoot\vcpkg.exe remove ffmpeg:$Triplet   # then re-run this script" -ForegroundColor Yellow
}
Write-Host "`nNative decode libs ready: $libdir\{heif,dav1d,avcodec,avformat}.lib" -ForegroundColor Green
Write-Host "Set VCPKG_ROOT=$VcpkgRoot (or keep it at ~/vcpkg) and build with --features libheif,dav1d,ffprobe." -ForegroundColor Green
Write-Host "ffprobe (FFmpeg) additionally needs, at BUILD time only:" -ForegroundColor Green
Write-Host "  * LIBCLANG_PATH -> a libclang for bindgen. Visual Studio already ships one:" -ForegroundColor Green
Write-Host "      <VS>\VC\Tools\Llvm\{x64,ARM64}\bin   (no separate LLVM install needed)" -ForegroundColor Green
Write-Host "  * a Developer shell (Enter-VsDevShell / vcvars): bindgen's clang reads INCLUDE" -ForegroundColor Green
Write-Host "    to find stdint.h. A plain cargo build does not need this; bindgen does." -ForegroundColor Green
