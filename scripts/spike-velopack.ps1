<#
.SYNOPSIS
  Blaze Viewer local update-loop tester — build + pack a throwaway release into a local feed you
  can install and self-update entirely offline. This is NOT the production pipeline (that's
  scripts/release-windows.ps1: libheif + Azure Trusted Signing + upload to downloads.blazeviewer.app);
  it's a fast, unsigned local harness for exercising the per-user install + file-association +
  auto-update loop without a network or the vcpkg/libheif build.

.DESCRIPTION
  Pipeline: ensure `vpk` (the Velopack CLI) is installed -> cargo build --release -> stage the exe
  -> `vpk pack` into a local feed folder that doubles as the update source (a Velopack FileSource).

  Run once with -Version 0.0.1 and install the produced Setup.exe. Then run again with
  -Version 0.0.2 to publish an "update" into the same feed, and launch the installed app
  with PB_UPDATE_FEED set to watch it self-update. See the printed NEXT STEPS.

  Local-harness shortcuts (deliberately not how we ship):
    * Builds WITHOUT --features libheif, so no vcpkg is needed (HEIC just won't decode here;
      every other format, incl. the .jpg used to test associations, works).
    * Packs UNSIGNED and skips --framework (the VC++ redist check — a dev box already has the
      runtime). Unsigned Setup.exe trips SmartScreen — click "More info" -> "Run anyway". The
      production script signs via Azure Trusted Signing and adds the redist bootstrapper.

.EXAMPLE
  pwsh scripts/spike-velopack.ps1 -Version 0.0.1   # then install the Setup.exe
  pwsh scripts/spike-velopack.ps1 -Version 0.0.2   # then watch the installed app self-update
#>
[CmdletBinding()]
param(
    [string]$Version = "0.0.1",
    # Local feed folder — also the update source the installed app reads. Under dist\ so
    # it's out of the way; delete dist\spike to start clean.
    [string]$FeedDir = "dist\spike\feed"
)
$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

$StageDir = Join-Path $RepoRoot "dist\spike\stage"
$FeedDir = Join-Path $RepoRoot $FeedDir

# ── 1. Ensure vpk (Velopack CLI, a dotnet global tool). Needs the .NET SDK (present:
#      the ATS signing path already relies on the .NET 8 runtime).
if (-not (Get-Command vpk -ErrorAction SilentlyContinue)) {
    $toolPath = Join-Path $env:USERPROFILE ".dotnet\tools"
    if (-not (Test-Path (Join-Path $toolPath "vpk.exe"))) {
        Write-Host "==> Installing vpk (Velopack CLI) as a dotnet global tool" -ForegroundColor Cyan
        dotnet tool install -g vpk
        if ($LASTEXITCODE -ne 0) { throw "dotnet tool install -g vpk failed" }
    }
    $env:PATH = "$toolPath;$env:PATH"   # ~\.dotnet\tools may not be on PATH this session
}
if (-not (Get-Command vpk -ErrorAction SilentlyContinue)) { throw "vpk not found after install." }
Write-Host "==> vpk: $((Get-Command vpk).Source)"

# ── 2. Build (unsigned local harness; no libheif so no vcpkg is needed). velopack is a normal
#      always-on dep now, so there's no feature flag — the lifecycle hooks are always compiled in.
Write-Host "==> cargo build --release -p pb-app (feed v$Version)" -ForegroundColor Cyan
cargo build --release -p pb-app
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
$Exe = "target\release\blazeviewer.exe"
if (-not (Test-Path $Exe)) { throw "$Exe not found after build" }

# ── 3. Stage a clean pack dir with just the exe. A release Rust build needs no sidecar
#      files besides the VC++ runtime, which a dev box already has (a shipping build would
#      add `--framework vcredist143-x64` so Setup installs it when missing).
if (Test-Path $StageDir) { Remove-Item -Recurse -Force $StageDir }
New-Item -ItemType Directory -Force $StageDir | Out-Null
Copy-Item $Exe (Join-Path $StageDir "blazeviewer.exe") -Force

# ── 4. Pack into the local feed (also the FileSource the installed app reads for updates).
#      Default channel is "win" -> writes releases.win.json, which FileSource expects.
New-Item -ItemType Directory -Force $FeedDir | Out-Null
Write-Host "==> vpk pack $Version -> $FeedDir" -ForegroundColor Cyan
vpk pack `
    --packId BlazeViewer `
    --packVersion $Version `
    --packDir $StageDir `
    --mainExe blazeviewer.exe `
    --packTitle "Blaze Viewer" `
    --packAuthors "FullSpec Systems" `
    --icon "crates\pb-app\icons\blazeviewer.ico" `
    --splashImage "crates\pb-app\icons\blazeviewer-splash.jpg" `
    --splashProgressColor "#FF4915" `
    --outputDir $FeedDir
if ($LASTEXITCODE -ne 0) { throw "vpk pack failed" }

$Setup = Get-ChildItem (Join-Path $FeedDir "*Setup.exe") -ErrorAction SilentlyContinue | Select-Object -First 1
$InstalledExe = Join-Path $env:LOCALAPPDATA "BlazeViewer\current\blazeviewer.exe"

Write-Host ""
Write-Host "==> Done. Feed: $FeedDir" -ForegroundColor Green
Write-Host ""
Write-Host "NEXT STEPS" -ForegroundColor Cyan
if ($Version -eq "0.0.1") {
    Write-Host "  1. Install (per-user, no UAC, a few seconds):"
    Write-Host "       $($Setup.FullName)"
    Write-Host "     Velopack runs 'blazeviewer.exe --veloapp-install', which registers the HKCU"
    Write-Host "     file associations, then auto-launches the app."
    Write-Host "  2. Test associations: double-click a .jpg -> 'Open with' -> Blaze Viewer"
    Write-Host "     (or Settings > Apps > Default apps). Confirm the command targets ...\current\:"
    Write-Host "       Get-ItemProperty 'HKCU:\Software\Classes\BlazeViewer.Image\shell\open\command'"
    Write-Host "  3. Publish an update:  pwsh scripts/spike-velopack.ps1 -Version 0.0.2"
}
else {
    Write-Host "  Watch the installed app self-update from this feed:"
    Write-Host "    `$env:PB_UPDATE_FEED = '$FeedDir'"
    Write-Host "    & '$InstalledExe'"
    Write-Host "  -> a background check downloads v$Version, then a toast reads 'Update ready. It"
    Write-Host "     installs when you quit.' Quit the app to apply it; relaunch to confirm v$Version."
    Write-Host ""
    Write-Host "  Uninstall (verify HKCU cleanup via --veloapp-uninstall): Settings > Apps, or"
    Write-Host "    & '$InstalledExe' --veloapp-uninstall   # then re-check the registry key above is gone"
}
