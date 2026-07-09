<#
.SYNOPSIS
  Build (and optionally run) PhotoBlaze on Windows for **manual testing** — the dev twin of
  scripts/release-windows.ps1, without the sign/pack/upload steps.

.DESCRIPTION
  Locates the libheif vcpkg tree pb-decode/build.rs links (so HEIC/HEIF decode matches a real
  release build), then runs the cargo command with `--features libheif`. Debug by default (fast
  compiles); pass -Release for an optimized build (decode is slow in debug — use -Release when
  testing scrubbing/decode throughput). Pass -Run to launch the freshly built exe.

  Skip libheif entirely with -NoHeif (a plain `cargo build -p pb-app`) when you don't need HEIC
  and don't have vcpkg set up.

.EXAMPLE
  pwsh scripts/build-windows.ps1              # debug build with libheif
  pwsh scripts/build-windows.ps1 -Run         # ...then launch it
  pwsh scripts/build-windows.ps1 -Release -Run # optimized build, then launch
  pwsh scripts/build-windows.ps1 -NoHeif -Run  # skip libheif (no vcpkg needed)
#>
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$Run,
    [switch]$NoHeif
)
$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

# ── libheif is the ship config — locate the vcpkg tree pb-decode/build.rs links (same detection
#    as release-windows.ps1). Skipped with -NoHeif.
$features = @()
if (-not $NoHeif) {
    if (-not $env:VCPKG_ROOT) {
        $env:VCPKG_ROOT = @("C:\vcpkg-pb", "$env:USERPROFILE\vcpkg") |
            Where-Object { Test-Path "$_\installed\x64-windows-static-md\lib\heif.lib" } |
            Select-Object -First 1
    }
    if (-not $env:VCPKG_ROOT -or -not (Test-Path "$env:VCPKG_ROOT\installed\x64-windows-static-md\lib\heif.lib")) {
        throw "libheif not found (checked VCPKG_ROOT, C:\vcpkg-pb, ~\vcpkg). Run scripts/setup-libheif.ps1 first, or pass -NoHeif to build without HEIC support."
    }
    Write-Host "==> libheif: $env:VCPKG_ROOT"
    $features = @("--features", "libheif")
}

$profileArgs = if ($Release) { @("--release") } else { @() }
$profileName = if ($Release) { "release" } else { "debug" }

$buildArgs = @("build", "-p", "pb-app") + $profileArgs + $features
Write-Host "==> cargo $($buildArgs -join ' ')" -ForegroundColor Cyan
cargo @buildArgs
if ($LASTEXITCODE -ne 0) { throw "build failed" }

$Exe = "target\$profileName\photoblaze.exe"
Write-Host "==> Built $Exe" -ForegroundColor Green

if ($Run) {
    Write-Host "==> Running $Exe" -ForegroundColor Cyan
    & $Exe
}
