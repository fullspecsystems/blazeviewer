<#
.SYNOPSIS
  Build, sign, and package the PhotoBlaze Windows release with **Velopack** (per-user installer +
  built-in auto-update), locally. Replaces the retired WiX/MSI flow. Twin of scripts/release-macos.sh.

.DESCRIPTION
  Pipeline: cargo build --release --features libheif → `vpk pack` (Azure Trusted Signing signs the
  app exe + Setup.exe + Update.exe; bundles the icon, install splash, and the VC++ redist check) →
  the flat feed lands in dist\feed (releases.win.json + .nupkg packages + PhotoBlaze-win-Setup.exe).
  Pass -Upload to rsync that feed to the downloads.fullspec.ca web root, which the app reads over
  HTTP for auto-update (see crates/pb-app/src/update.rs FEED_URL).

  Version comes from Cargo.toml (pb-app), so the package always matches the app. Velopack packs a
  full release each time and auto-generates deltas; the flat feed serves whichever is smaller.

  Signing = Azure Trusted Signing (the same account/profile the old MSI used). vpk bundles a
  compatible signtool + dlib, so we only supply the metadata.json + credentials:
    a) service-principal env vars (AZURE_TENANT_ID / AZURE_CLIENT_ID / AZURE_CLIENT_SECRET) in
       .env.release (gitignored; sourced here), or
    b) `az login` with the "Trusted Signing Certificate Profile Signer" role.
  Needs the .NET 8+ runtime (vpk's ATS signing) — present with the .NET SDK. vpk itself is installed
  on first use as a dotnet global tool. Signing skips cleanly (unsigned) when no credentials exist.

.EXAMPLE
  pwsh scripts/release-windows.ps1            # build + sign + pack → dist\feed
  pwsh scripts/release-windows.ps1 -Upload    # ...then rsync the feed to downloads.fullspec.ca
#>
[CmdletBinding()]
param(
    # Signing account defaults to the owner's public-trust setup; override for a different account.
    [string]$Endpoint = "https://wus.codesigning.azure.net/",
    [string]$AccountName = "jdlien-signing",
    [string]$ProfileName = "jdlien-public-trust",
    # Push the packed feed to the droplet's downloads.fullspec.ca web root over SSH. Off by default
    # so a plain run just produces the feed locally. SSH host is jdlien.com (same droplet); the path
    # is the downloads.fullspec.ca site root. Pass a full `[user@]host:/path` to override.
    [switch]$Upload,
    [string]$UploadTarget = "jdlien.com:/var/www/downloads.fullspec.ca/photoblaze/win/"
)
$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

# ── 0. Credentials: .env.release (gitignored) is sourced like the mac script does.
if (Test-Path .env.release) {
    foreach ($line in Get-Content .env.release) {
        if ($line -match '^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$') {
            Set-Item -Path "env:$($Matches[1])" -Value $Matches[2].Trim().Trim('"')
        }
    }
}

# ── 1. Version from Cargo.toml (pb-app) — the package and the app always agree.
$Version = ((cargo metadata --no-deps --format-version 1 | ConvertFrom-Json).packages |
    Where-Object { $_.name -eq 'pb-app' }).version
Write-Host "==> PhotoBlaze $Version (Windows / Velopack)" -ForegroundColor Cyan

# ── 2. libheif is the ship config — locate the vcpkg tree pb-decode/build.rs links.
if (-not $env:VCPKG_ROOT) {
    $env:VCPKG_ROOT = @("C:\vcpkg-pb", "$env:USERPROFILE\vcpkg") |
        Where-Object { Test-Path "$_\installed\x64-windows-static-md\lib\heif.lib" } |
        Select-Object -First 1
}
if (-not $env:VCPKG_ROOT -or -not (Test-Path "$env:VCPKG_ROOT\installed\x64-windows-static-md\lib\heif.lib")) {
    throw "libheif not found (checked VCPKG_ROOT, C:\vcpkg-pb, ~\vcpkg). Run scripts/setup-libheif.ps1 first — the release ships --features libheif."
}
Write-Host "==> libheif: $env:VCPKG_ROOT"

# ── 3. Always build fresh — a stale exe must never be silently signed/packaged.
Write-Host "==> cargo build --release -p pb-app --features libheif" -ForegroundColor Cyan
cargo build --release -p pb-app --features libheif
if ($LASTEXITCODE -ne 0) { throw "build failed" }
$Exe = "target\release\photoblaze.exe"
if (-not (Test-Path $Exe)) { throw "$Exe not found after build" }

# ── 4. vpk (Velopack CLI) — a dotnet global tool; install on first use, cache after.
if (-not (Get-Command vpk -ErrorAction SilentlyContinue)) {
    $toolPath = Join-Path $env:USERPROFILE ".dotnet\tools"
    if (-not (Test-Path (Join-Path $toolPath "vpk.exe"))) {
        Write-Host "==> Installing vpk (Velopack CLI)" -ForegroundColor Cyan
        dotnet tool install -g vpk
        if ($LASTEXITCODE -ne 0) { throw "dotnet tool install -g vpk failed" }
    }
    $env:PATH = "$toolPath;$env:PATH"   # ~\.dotnet\tools may not be on PATH this session
}

# ── 5. Signing (Azure Trusted Signing). vpk bundles a compatible signtool + dlib, so we only hand
#      it the metadata.json; credentials come from the env vars (DefaultAzureCredential) or az login.
#      Skips cleanly (unsigned) when neither is available.
$HaveEnvCreds = -not [string]::IsNullOrEmpty($env:AZURE_CLIENT_SECRET)
$HaveAzLogin = $false
if (-not $HaveEnvCreds -and (Get-Command az -ErrorAction SilentlyContinue)) {
    az account show *> $null
    $HaveAzLogin = ($LASTEXITCODE -eq 0)
}
$SignArgs = @()
if ($HaveEnvCreds -or $HaveAzLogin) {
    $meta = Join-Path $env:TEMP "pb-trusted-signing.json"
    @{ Endpoint = $Endpoint; CodeSigningAccountName = $AccountName; CertificateProfileName = $ProfileName } |
        ConvertTo-Json | Set-Content $meta -Encoding ascii
    $SignArgs = @("--azureTrustedSignFile", $meta)
    $how = if ($HaveEnvCreds) { "service-principal env vars" } else { "az login" }
    Write-Host "==> Signing enabled ($how; $AccountName/$ProfileName)" -ForegroundColor Green
} else {
    Write-Host "==> Signing SKIPPED — no AZURE_CLIENT_SECRET (.env.release) and no az login" -ForegroundColor Yellow
}

# ── 6. Stage the exe and pack. --framework installs the VC++ runtime during setup if it's missing
#      (the Rust CRT is dynamically linked); the flat feed keeps auto-generated deltas usable.
$Stage = Join-Path $RepoRoot "dist\stage"
$Feed = Join-Path $RepoRoot "dist\feed"
if (Test-Path $Stage) { Remove-Item -Recurse -Force $Stage }
New-Item -ItemType Directory -Force $Stage, $Feed | Out-Null
Copy-Item $Exe (Join-Path $Stage "photoblaze.exe") -Force

$packArgs = @(
    "pack",
    "--packId", "PhotoBlaze",
    "--packVersion", $Version,
    "--packDir", $Stage,
    "--mainExe", "photoblaze.exe",
    "--packTitle", "PhotoBlaze",
    "--packAuthors", "FullSpec Systems",
    "--icon", "crates\pb-app\icons\photoblaze.ico",
    "--splashImage", "crates\pb-app\icons\photoblaze-splash.jpg",
    "--splashProgressColor", "#FF4915",
    "--framework", "vcredist143-x64",
    "--channel", "win",
    "--outputDir", $Feed
) + $SignArgs
Write-Host "==> vpk pack $Version" -ForegroundColor Cyan
vpk @packArgs
if ($LASTEXITCODE -ne 0) { throw "vpk pack failed" }

$Setup = Get-ChildItem (Join-Path $Feed "*Setup.exe") | Select-Object -First 1
Write-Host "==> Packed → $Feed" -ForegroundColor Green
Write-Host "    Installer: $($Setup.Name)"
if ($SignArgs.Count -eq 0) { Write-Host "    (UNSIGNED — set AZURE_* in .env.release or az login)" -ForegroundColor Yellow }

# ── 7. Upload the feed to downloads.fullspec.ca (opt-in). Prefers rsync (incremental); falls back
#      to scp — stock Windows has no rsync, only OpenSSH. Both add/overwrite files and leave the
#      rest in place, so the delta chain + older versions stay reachable. Prune superseded packages
#      on the server later. $UploadTarget may carry a `user@` prefix, or resolve it via ~/.ssh/config.
if ($Upload) {
    $uploaded = $false
    # cd into the flat feed so a relative source (./ or bare names) is used — the Windows
    # drive-letter colon (C:\...) would otherwise be misread as an rsync/scp remote-host separator.
    Push-Location $Feed
    try {
        # rsync is incremental but flaky on Windows (cwRsync invokes its own bundled ssh that can't
        # see the OpenSSH key → protocol error 12), so try it, then fall back to scp — Windows-native
        # OpenSSH, reliable, just re-sends the whole (small) flat feed.
        if (Get-Command rsync -ErrorAction SilentlyContinue) {
            Write-Host "==> rsync → $UploadTarget" -ForegroundColor Cyan
            rsync -avz ./ $UploadTarget
            if ($LASTEXITCODE -eq 0) { $uploaded = $true }
            else { Write-Host "    rsync failed (exit $LASTEXITCODE); falling back to scp" -ForegroundColor Yellow }
        }
        if (-not $uploaded -and (Get-Command scp -ErrorAction SilentlyContinue)) {
            $names = (Get-ChildItem -File).Name
            Write-Host "==> scp $($names.Count) file(s) → $UploadTarget" -ForegroundColor Cyan
            scp @names $UploadTarget
            if ($LASTEXITCODE -eq 0) { $uploaded = $true }
        }
    } finally { Pop-Location }
    if (-not $uploaded) { throw "upload failed — no working rsync/scp (scp ships with Windows OpenSSH)." }
    Write-Host "==> Live at https://downloads.fullspec.ca/photoblaze/win/" -ForegroundColor Green
} else {
    Write-Host "==> Not uploaded (pass -Upload). Feed: $Feed" -ForegroundColor Yellow
}
