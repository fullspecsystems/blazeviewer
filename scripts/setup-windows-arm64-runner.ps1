# Bootstrap a Windows ARM64 self-hosted CI runner (the Fusion VM on the Mac).
# One paste in an ELEVATED PowerShell inside the fresh VM:
#
#   1. On the Mac (token is single-use, expires in 1 h):
#        gh api -X POST repos/jdlien/photoblaze/actions/runners/registration-token --jq .token
#   2. In the VM (elevated):
#        Set-ExecutionPolicy -Scope Process Bypass -Force
#        .\setup-windows-arm64-runner.ps1 -Token <paste>
#
# Installs Git + VS Build Tools (ARM64 MSVC + Windows SDK) + rustup (aarch64 host),
# then registers the Actions runner as a Windows service under this account (so jobs
# see this user's rustup/MSVC environment — same convention as GREMLIN).
# After it finishes: flip the repo variable that enables the CI lane (see
# .taskmaster/docs/self-hosted-runners.md → "Windows ARM64 runner").
#Requires -RunAsAdministrator
param(
    [Parameter(Mandatory)] [string]$Token,
    [string]$RunnerDir = "C:\actions-runner",
    [string]$Repo = "https://github.com/jdlien/photoblaze"
)
$ErrorActionPreference = "Stop"

Write-Host "== [1/4] Prerequisites via winget (Git, VS Build Tools ARM64)" -ForegroundColor Cyan
winget install --id Git.Git -e --accept-source-agreements --accept-package-agreements
# ARM64-native MSVC toolset + Windows 11 SDK — what rustc's aarch64-pc-windows-msvc
# host toolchain links with. (~15 min; --wait blocks until the VS installer finishes.)
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --accept-package-agreements --override `
    "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 --add Microsoft.VisualStudio.Component.Windows11SDK.22621 --includeRecommended"

Write-Host "== [2/4] rustup (aarch64-pc-windows-msvc host)" -ForegroundColor Cyan
$rustupInit = "$env:TEMP\rustup-init.exe"
Invoke-WebRequest -Uri "https://win.rustup.rs/aarch64" -OutFile $rustupInit
# Stable default; the repo's rust-toolchain.toml pin overrides per-invocation anyway.
& $rustupInit -y --default-host aarch64-pc-windows-msvc
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
rustc --version

Write-Host "== [3/4] Actions runner (win-arm64) -> $RunnerDir" -ForegroundColor Cyan
$ver = (Invoke-RestMethod "https://api.github.com/repos/actions/runner/releases/latest").tag_name.TrimStart("v")
New-Item -ItemType Directory -Force -Path $RunnerDir | Out-Null
$zip = "$env:TEMP\actions-runner.zip"
Invoke-WebRequest -Uri "https://github.com/actions/runner/releases/download/v$ver/actions-runner-win-arm64-$ver.zip" -OutFile $zip
Expand-Archive -Path $zip -DestinationPath $RunnerDir -Force
Remove-Item $zip

Write-Host "== [4/4] Register as a service (prompts once for this account's password)" -ForegroundColor Cyan
Set-Location $RunnerDir
# Default labels come out as: self-hosted, Windows, ARM64 — exactly what ci.yml targets.
.\config.cmd --url $Repo --token $Token --name "$env:COMPUTERNAME-arm64" --unattended `
    --runasservice --windowslogonaccount $env:USERNAME

Write-Host ""
Write-Host "Done. Next (from the Mac): enable the CI lane —" -ForegroundColor Green
Write-Host "  gh variable set WIN_ARM64_RUNNER --body 1 --repo jdlien/photoblaze"
Write-Host "Optional (only for Live Photo corpus tests): install 'HEVC Video Extensions'"
Write-Host "from the Microsoft Store and copy the test clips over."
