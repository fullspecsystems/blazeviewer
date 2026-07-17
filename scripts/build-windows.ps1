<#
.SYNOPSIS
  Build (and optionally run) Blaze Viewer on Windows for **manual testing** — the dev twin of
  scripts/release-windows.ps1, without the sign/pack/upload steps.

.DESCRIPTION
  Locates the vcpkg tree pb-decode/build.rs links (so HEIC/HEIF decode and animated AVIF match a
  real release build), then runs the cargo command with the **ship feature set**,
  `--features libheif,dav1d,ffprobe`. `ffprobe` is what links FFmpeg, which is what decodes the
  audio codecs Media Foundation refuses (AC-3 / E-AC-3 / DTS — most films). WITHOUT it a build is
  MF-only and every film plays silent (0xC00D36B4), which is the #1 "why is there no sound" trap —
  so it is on by default here, exactly as the release ships it. Debug by default (fast compiles);
  pass -Release for an optimized build (decode is slow in debug — use -Release when testing
  scrubbing/decode throughput). Pass -Run to launch the freshly built exe.

  Because `ffprobe` links FFmpeg, its bindgen needs a VS Developer shell (for the MSVC/SDK include
  paths + libclang) and `VCPKG_ROOT` exported — this script enters one for you when you aren't
  already inside it, so a plain pwsh prompt works. Pass -NoFfmpeg to drop back to
  `libheif,dav1d` (no FFmpeg, no Developer shell needed — films will be silent). Skip the native
  decode libs entirely with -NoNative (a plain `cargo build -p pb-app`) when you don't need HEIC /
  animated AVIF / film audio and don't have vcpkg set up. (-NoHeif is the old alias for -NoNative.)

.EXAMPLE
  pwsh scripts/build-windows.ps1               # debug build, libheif + dav1d + ffprobe (film audio)
  pwsh scripts/build-windows.ps1 -Run          # ...then launch it
  pwsh scripts/build-windows.ps1 -Release -Run # optimized build, then launch
  pwsh scripts/build-windows.ps1 -NoFfmpeg -Run # no FFmpeg (films silent), no Developer shell
  pwsh scripts/build-windows.ps1 -NoNative -Run # skip every native lib (no vcpkg needed)
#>
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Run,
    [switch]$NoFfmpeg,
    [switch]$NoNative,
    [switch]$NoHeif # legacy alias for -NoNative
)
$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

# ── libheif + dav1d + FFmpeg are the ship config — locate the vcpkg tree pb-decode/build.rs links
#    (same detection as release-windows.ps1). Skipped entirely with -NoNative (or legacy -NoHeif).
$features = @()
$withFfmpeg = $false
if (-not ($NoNative -or $NoHeif)) {
    # heif/dav1d link the static-md triplet in a dev build (release ships the DLL triplet for the
    # LGPL relink); this is the tree that carries them, and the anchor VCPKG_ROOT detection uses.
    if (-not $env:VCPKG_ROOT) {
        $env:VCPKG_ROOT = @("C:\vcpkg-pb", "$env:USERPROFILE\vcpkg") |
            Where-Object { (Test-Path "$_\installed\x64-windows-static-md\lib\heif.lib") -and (Test-Path "$_\installed\x64-windows-static-md\lib\dav1d.lib") } |
            Select-Object -First 1
    }
    foreach ($lib in "heif.lib", "dav1d.lib") {
        if (-not $env:VCPKG_ROOT -or -not (Test-Path "$env:VCPKG_ROOT\installed\x64-windows-static-md\lib\$lib")) {
            throw "$lib not found (checked VCPKG_ROOT, C:\vcpkg-pb, ~\vcpkg). Run scripts/setup-libheif.ps1 first, or pass -NoNative to build without HEIC/animated-AVIF support."
        }
    }
    Write-Host "==> native decode libs: $env:VCPKG_ROOT"
    $features = @("libheif", "dav1d")

    # FFmpeg (the `ffprobe` feature) — the film-audio decoder. Unlike heif/dav1d, ffmpeg-sys-next
    # always links the **DLL** triplet (it sets VCPKGRS_DYNAMIC=1), so its DLLs live under
    # installed\x64-windows\bin regardless of the static-md heif/dav1d above. Their soname is in
    # the filename (avcodec-62.dll), so match by prefix, not a version that rots at the next bump.
    if (-not $NoFfmpeg) {
        $ffBin = "$env:VCPKG_ROOT\installed\x64-windows\bin"
        $haveFfmpeg = ("avcodec", "avformat" | ForEach-Object {
                [bool](Get-ChildItem "$ffBin\$_-*.dll" -ErrorAction SilentlyContinue)
            }) -notcontains $false
        if (-not $haveFfmpeg) {
            throw "FFmpeg DLLs (avcodec/avformat, x64-windows) not found under $ffBin. Run ``scripts/setup-libheif.ps1 -Triplet x64-windows`` to install the FFmpeg port, or pass -NoFfmpeg to build without it (films will play silent — MF can't decode AC-3/E-AC-3/DTS)."
        }
        $features += "ffprobe"
        $withFfmpeg = $true
    }
}

# FFmpeg's bindgen needs the MSVC/SDK include paths + libclang (task #100) — a plain cargo build
# never did, so enter a VS Developer shell when the caller isn't already in one. No-op inside a VS
# prompt (INCLUDE already set), and this also exports VCPKG_ROOT, which ffmpeg-sys-next's vcpkg
# lookup has no ~/vcpkg fallback for. Only needed when actually building FFmpeg.
if ($withFfmpeg -and (-not $env:INCLUDE -or -not $env:LIBCLANG_PATH)) {
    Write-Host "==> entering VS Developer shell (FFmpeg bindgen needs it)" -ForegroundColor Cyan
    & "$PSScriptRoot\vs-dev-env.ps1"
}

$profileArgs = if ($Release) { @("--release") } else { @() }
$profileName = if ($Release) { "release" } else { "debug" }
$featureArgs = if ($features) { @("--features", ($features -join ",")) } else { @() }

$buildArgs = @("build", "-p", "pb-app") + $profileArgs + $featureArgs
Write-Host "==> cargo $($buildArgs -join ' ')" -ForegroundColor Cyan
cargo @buildArgs
if ($LASTEXITCODE -ne 0) { throw "build failed" }

$Exe = "target\$profileName\blazeviewer.exe"
Write-Host "==> Built $Exe" -ForegroundColor Green
if (-not $withFfmpeg -and -not ($NoNative -or $NoHeif)) {
    Write-Host "    (no FFmpeg — films with AC-3/E-AC-3/DTS audio will play SILENT; drop -NoFfmpeg for sound)" -ForegroundColor Yellow
}

if ($Run) {
    Write-Host "==> Running $Exe" -ForegroundColor Cyan
    & $Exe
}
