<#
.SYNOPSIS
  Publish the macOS PhotoBlaze DMG to the downloads.fullspec.ca feed and repoint the "latest"
  symlink — the mac half of release-windows.ps1's upload step. (Mac still ships a plain versioned
  DMG, not Velopack, so unlike Windows' constant-named Setup.exe the "latest" link needs a symlink.)

.DESCRIPTION
  Finds PhotoBlaze-<version>.dmg (+ its .sha256 sidecar) in -Source, scp's both to
  /var/www/downloads.fullspec.ca/photoblaze/mac/, then repoints PhotoBlaze-latest.dmg -> the
  uploaded DMG so https://downloads.fullspec.ca/photoblaze/latest/mac always serves the newest.

  The DMG is built on the Mac (scripts/release-macos.sh) and lands in -Source on this box; this
  script publishes it. SSH host jdlien.com, same access release-windows.ps1 uses. Idempotent —
  re-running with the same DMG just re-uploads and re-points to the same target.

.EXAMPLE
  pwsh scripts/release-mac-upload.ps1                                 # newest DMG in D:\Media\photoblaze
  pwsh scripts/release-mac-upload.ps1 -Source D:\builds              # from another folder
  pwsh scripts/release-mac-upload.ps1 -Dmg D:\x\PhotoBlaze-0.1.0.dmg # a specific DMG
#>
[CmdletBinding()]
param(
    # Folder to search for the newest PhotoBlaze-*.dmg (where release-macos.sh's output lands).
    [string]$Source = "D:\Media\photoblaze",
    # An explicit DMG path; overrides -Source discovery.
    [string]$Dmg,
    [string]$UploadHost = "jdlien.com",
    [string]$RemoteDir = "/var/www/downloads.fullspec.ca/photoblaze/mac"
)
$ErrorActionPreference = "Stop"

# ── 1. Locate the DMG: explicit -Dmg, else the newest PhotoBlaze-*.dmg in -Source.
if ($Dmg) {
    $dmgFile = Get-Item -LiteralPath $Dmg
} else {
    $dmgFile = Get-ChildItem -Path $Source -Filter "PhotoBlaze-*.dmg" -File |
        Where-Object { $_.Name -ne "PhotoBlaze-latest.dmg" } |   # never publish the alias as a version
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $dmgFile) { throw "No PhotoBlaze-*.dmg in $Source (pass -Dmg <path> or -Source <dir>)." }
}
if ($dmgFile.Name -eq "PhotoBlaze-latest.dmg") { throw "Refusing to publish the 'latest' alias; need a versioned DMG." }
$name = $dmgFile.Name
Write-Host "==> DMG: $($dmgFile.FullName)" -ForegroundColor Cyan

# ── 2. Include the .sha256 sidecar if it sits next to the DMG.
$files = @($name)
$sha = "$name.sha256"
if (Test-Path (Join-Path $dmgFile.DirectoryName $sha)) { $files += $sha }
else { Write-Host "    (no $sha sidecar; uploading the DMG only)" -ForegroundColor Yellow }

# ── 3. scp the file(s). cd into the source first so scp uses bare names — a Windows absolute
#      path (C:\...) would have its drive-letter colon misread as an scp remote-host separator.
$target = "${UploadHost}:${RemoteDir}/"
Push-Location $dmgFile.DirectoryName
try {
    Write-Host "==> scp $($files -join ', ') -> $target" -ForegroundColor Cyan
    scp @files $target
    if ($LASTEXITCODE -ne 0) { throw "scp upload failed" }
} finally { Pop-Location }

# ── 4. Repoint the permanent "latest" symlink at the DMG we just uploaded (relative target so it
#      stays valid within the dir). This is what makes /photoblaze/latest/mac serve the newest.
Write-Host "==> repoint PhotoBlaze-latest.dmg -> $name" -ForegroundColor Cyan
ssh -o BatchMode=yes $UploadHost "cd '$RemoteDir' && ln -sfn '$name' PhotoBlaze-latest.dmg && ls -l PhotoBlaze-latest.dmg"
if ($LASTEXITCODE -ne 0) { throw "symlink repoint failed" }

# ── 5. Verify the permanent URL now serves this build.
$r = Invoke-WebRequest "https://downloads.fullspec.ca/photoblaze/latest/mac" -Method Head -SkipHttpErrorCheck
if ($r.StatusCode -ne 200) { throw "latest/mac returned HTTP $($r.StatusCode)" }
Write-Host "==> Live:" -ForegroundColor Green
Write-Host "    https://downloads.fullspec.ca/photoblaze/latest/mac  (HTTP 200)"
Write-Host "    https://downloads.fullspec.ca/photoblaze/mac/$name"
