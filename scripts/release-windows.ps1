<#
.SYNOPSIS
  Build, sign, and package the PhotoBlaze Windows MSI locally — the Windows twin of
  scripts/release-macos.sh, for cutting releases without CI (Actions credits are finite).

.DESCRIPTION
  Pipeline (mirrors release.yml's windows-msi job): cargo build --release --features
  libheif → sign photoblaze.exe (Azure Trusted Signing) → cargo wix MSI (wraps the
  signed exe, no rebuild) → sign the MSI → SHA256 sidecar → dist\.

  Gated like the macOS script: every stage that lacks its inputs skips cleanly, so a
  machine without signing credentials still produces an (UNSIGNED) MSI.

  Signing = Azure Trusted Signing via signtool /dlib (the same account/profile
  release.yml uses). Credentials, either way works:
    a) Service-principal env vars (what CI uses; DefaultAzureCredential reads them):
         AZURE_TENANT_ID / AZURE_CLIENT_ID / AZURE_CLIENT_SECRET
       — set them in .env.release (gitignored; sourced here like the mac script does).
    b) Azure CLI: `az login` with an account holding the "Trusted Signing Certificate
       Profile Signer" role on the signing account (no secret on disk at all).
  The identity needs that role either way.

  Tools it fetches on first use (cached in %LOCALAPPDATA%\PhotoBlaze\build-tools):
  WiX v3.14 binaries and the Microsoft.Trusted.Signing.Client dlib. Needs a Windows
  SDK signtool (10.0.22621+) and the .NET 8 runtime — both standard on a dev box.

.EXAMPLE
  pwsh scripts/release-windows.ps1
  # → dist\PhotoBlaze-<version>-x86_64.msi (+ .sha256)
#>
[CmdletBinding()]
param(
    # Endpoint/account/profile default to the owner's public-trust setup (same values
    # as release.yml); override for a different signing account.
    [string]$Endpoint = "https://wus.codesigning.azure.net/",
    [string]$AccountName = "jdlien-signing",
    [string]$ProfileName = "jdlien-public-trust"
)
$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot
$ToolCache = Join-Path $env:LOCALAPPDATA "PhotoBlaze\build-tools"
New-Item -ItemType Directory -Force $ToolCache | Out-Null

function Invoke-Checked {
    param([string]$Label, [scriptblock]$Block)
    & $Block
    if ($LASTEXITCODE -ne 0) { throw "$Label failed (exit $LASTEXITCODE)" }
}

# ── 0. Credentials: .env.release (gitignored) is sourced like the mac script does.
if (Test-Path .env.release) {
    foreach ($line in Get-Content .env.release) {
        if ($line -match '^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$') {
            Set-Item -Path "env:$($Matches[1])" -Value $Matches[2].Trim().Trim('"')
        }
    }
}

$Version = ((cargo metadata --no-deps --format-version 1 | ConvertFrom-Json).packages |
    Where-Object { $_.name -eq 'pb-app' }).version
Write-Host "==> PhotoBlaze $Version (Windows MSI)" -ForegroundColor Cyan

# ── 1. libheif is the ship config — locate the vcpkg tree build.rs will link.
if (-not $env:VCPKG_ROOT) {
    $env:VCPKG_ROOT = @("C:\vcpkg-pb", "$env:USERPROFILE\vcpkg") |
        Where-Object { Test-Path "$_\installed\x64-windows-static-md\lib\heif.lib" } |
        Select-Object -First 1
}
if (-not $env:VCPKG_ROOT -or -not (Test-Path "$env:VCPKG_ROOT\installed\x64-windows-static-md\lib\heif.lib")) {
    throw "libheif not found (checked VCPKG_ROOT, C:\vcpkg-pb, ~\vcpkg). Run scripts/setup-libheif.ps1 first — the release MSI ships --features libheif."
}
Write-Host "==> libheif: $env:VCPKG_ROOT"

# ── 2. Always build fresh — a stale exe must never be silently re-signed/re-packaged.
Write-Host "==> cargo build --release -p pb-app --features libheif" -ForegroundColor Cyan
Invoke-Checked "build" { cargo build --release -p pb-app --features libheif }
$Exe = "target\release\photoblaze.exe"
if (-not (Test-Path $Exe)) { throw "$Exe not found after build" }

# ── 3. Signing setup (skips cleanly when neither credential source is available).
$HaveEnvCreds = -not [string]::IsNullOrEmpty($env:AZURE_CLIENT_SECRET)
$HaveAzLogin = $false
if (-not $HaveEnvCreds -and (Get-Command az -ErrorAction SilentlyContinue)) {
    az account show *> $null
    $HaveAzLogin = ($LASTEXITCODE -eq 0)
}
$Signing = $HaveEnvCreds -or $HaveAzLogin

$SignTool = $null; $Dlib = $null; $Dmdf = $null
if ($Signing) {
    # Newest Windows SDK signtool (the old ClickOnce one on PATH has no /dlib).
    $SignTool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\10.*\x64\signtool.exe" -ErrorAction SilentlyContinue |
        Sort-Object { [version]($_.Directory.Parent.Name) } | Select-Object -Last 1 -ExpandProperty FullName
    if (-not $SignTool) { throw "No Windows SDK signtool found (need 10.0.22621+ for Trusted Signing). Install a Windows 11 SDK." }

    # Trusted Signing client dlib (NuGet package, cached after the first download).
    $Dlib = Join-Path $ToolCache "Microsoft.Trusted.Signing.Client\bin\x64\Azure.CodeSigning.Dlib.dll"
    if (-not (Test-Path $Dlib)) {
        Write-Host "==> Fetching Microsoft.Trusted.Signing.Client (one-time)" -ForegroundColor Cyan
        $nupkg = Join-Path $env:TEMP "trusted-signing-client.zip"
        Invoke-WebRequest "https://www.nuget.org/api/v2/package/Microsoft.Trusted.Signing.Client" -OutFile $nupkg
        Expand-Archive $nupkg (Join-Path $ToolCache "Microsoft.Trusted.Signing.Client") -Force
        Remove-Item $nupkg
        if (-not (Test-Path $Dlib)) { throw "Azure.CodeSigning.Dlib.dll missing after extract — NuGet package layout changed?" }
    }

    $Dmdf = Join-Path $env:TEMP "pb-trusted-signing.json"
    @{ Endpoint = $Endpoint; CodeSigningAccountName = $AccountName; CertificateProfileName = $ProfileName } |
        ConvertTo-Json | Set-Content $Dmdf -Encoding ascii
    $how = if ($HaveEnvCreds) { "service-principal env vars" } else { "Azure CLI login" }
    Write-Host "==> Signing enabled ($how; $AccountName/$ProfileName)" -ForegroundColor Green
} else {
    Write-Host "==> Signing SKIPPED — no AZURE_CLIENT_SECRET (env or .env.release) and no az login" -ForegroundColor Yellow
}

function Sign-File([string]$Path) {
    Invoke-Checked "signtool sign $Path" {
        & $SignTool sign /v /fd SHA256 /tr "http://timestamp.acs.microsoft.com" /td SHA256 /dlib $Dlib /dmdf $Dmdf $Path
    }
    Invoke-Checked "signtool verify $Path" { & $SignTool verify /pa $Path }
}

if ($Signing) { Write-Host "==> Signing the exe" -ForegroundColor Cyan; Sign-File $Exe }

# ── 4. Package the MSI around the (signed) exe. cargo-wix needs WiX v3's candle/light.
if (-not $env:WIX -or -not (Test-Path "$env:WIX\bin\candle.exe")) {
    $wixDir = Join-Path $ToolCache "wix3"
    if (-not (Test-Path "$wixDir\bin\candle.exe")) {
        Write-Host "==> Fetching WiX v3.14 binaries (one-time)" -ForegroundColor Cyan
        $zip = Join-Path $env:TEMP "wix3.zip"
        Invoke-WebRequest "https://github.com/wixtoolset/wix3/releases/download/wix3141rtm/wix314-binaries.zip" -OutFile $zip
        Expand-Archive $zip "$wixDir\bin" -Force
        Remove-Item $zip
    }
    $env:WIX = "$wixDir\"   # cargo-wix reads $WIX and appends \bin
}
if (-not (Get-Command cargo-wix -ErrorAction SilentlyContinue)) {
    Write-Host "==> Installing cargo-wix (one-time)" -ForegroundColor Cyan
    Invoke-Checked "cargo install cargo-wix" { cargo install cargo-wix --locked }
}
Write-Host "==> cargo wix (no rebuild — wraps the built exe)" -ForegroundColor Cyan
Invoke-Checked "cargo wix" { cargo wix --package pb-app --no-build --nocapture }

$Msi = Get-ChildItem target\wix\*.msi | Sort-Object LastWriteTime | Select-Object -Last 1
if ($Signing) { Write-Host "==> Signing the MSI" -ForegroundColor Cyan; Sign-File $Msi.FullName }

# ── 5. Checksum sidecar + dist\ (same "hash  name" format as the CI job / mac script).
New-Item -ItemType Directory -Force dist | Out-Null
$DistMsi = Join-Path dist $Msi.Name
Copy-Item $Msi.FullName $DistMsi -Force
$hash = (Get-FileHash $DistMsi -Algorithm SHA256).Hash.ToLower()
"$hash  $($Msi.Name)" | Out-File "$DistMsi.sha256" -Encoding ascii
Write-Host "$hash  $($Msi.Name)"

Write-Host "==> Done: $DistMsi" -ForegroundColor Green
if (-not $Signing) { Write-Host "    (UNSIGNED — set AZURE_* in .env.release or az login; see the script header)" -ForegroundColor Yellow }
