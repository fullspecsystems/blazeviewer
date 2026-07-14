# Bootstrap a Windows ARM64 self-hosted CI runner (a Windows-on-ARM VM on the Mac —
# Parallels/UTM/Fusion, whichever won). One paste in an ELEVATED PowerShell:
#
#   1. On the Mac (token is single-use, expires in 1 h):
#        gh api -X POST repos/fullspecsystems/blazeviewer/actions/runners/registration-token --jq .token
#   2. In the VM (elevated):
#        Set-ExecutionPolicy -Scope Process Bypass -Force
#        .\setup-windows-arm64-runner.ps1 -Token <paste>
#
# Installs Git + VS Build Tools (ARM64 MSVC + Windows SDK) + rustup (aarch64 host),
# downloads the Actions runner, and registers it — as a foreground process by
# DEFAULT (no service). Reasoning: a personal Windows install today is very likely
# signed in with a Microsoft account (PIN, not password) — GREMLIN's service
# convention needs the real account PASSWORD for `--windowslogonpassword`, which an
# MS-account/Hello-PIN setup usually doesn't have to hand, and hit exactly this wall
# twice (once as an outright config.cmd failure, once flagged before it could bite
# again). If you deliberately run a LOCAL account with a real password and want
# service persistence across reboots, pass -AsService — same convention as GREMLIN.
#
# -AsService installs the service to log on as -ServiceAccount (default: whichever
# account is running THIS script — but that's often not what you want: a dedicated
# CI service account is best created once, then this script re-run FROM YOUR OWN
# elevated session with -ServiceAccount gh-runner. Installing the *service* this way
# needs only admin rights in the invoking session (standard Windows service model:
# an admin configures a service to run as a different, often lower-privilege,
# account) — but rustup is per-user, so it still only lives in the invoking
# session's profile. If -ServiceAccount differs from the current user, the script
# warns and gives the two ways to fix it (see the warning text at step 4).
#
# After it finishes: flip the repo variable that enables the CI lane (see
# .taskmaster/docs/self-hosted-runners.md → "Windows ARM64 runner").
#Requires -RunAsAdministrator
param(
    [Parameter(Mandatory)] [string]$Token,
    [string]$RunnerDir = "C:\actions-runner",
    [string]$Repo = "https://github.com/fullspecsystems/blazeviewer",
    [switch]$AsService,
    [string]$ServiceAccount = $env:USERNAME
)
$ErrorActionPreference = "Stop"

Write-Host "== [1/4] Prerequisites via winget (Git, VS Build Tools ARM64 + Clang)" -ForegroundColor Cyan
winget install --id Git.Git -e --accept-source-agreements --accept-package-agreements
# ARM64-native MSVC toolset + Windows 11 SDK — what rustc's aarch64-pc-windows-msvc
# host toolchain links with. VC.Llvm.Clang is NOT optional: the `ring` crate (pulled
# in transitively by rustls/reqwest et al.) hard-requires clang to build its assembly
# on aarch64-pc-windows-msvc — MSVC cl.exe alone fails with "failed to find tool
# clang" deep in a `cargo test`. (~15 min; --wait blocks until the VS installer
# finishes.)
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --accept-package-agreements --override `
    "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 --add Microsoft.VisualStudio.Component.VC.Llvm.Clang --add Microsoft.VisualStudio.Component.Windows11SDK.22621 --includeRecommended"

# ring's build script expects `clang` resolvable on PATH — it does not auto-discover
# the VS-bundled copy. Find it (path/arch subfolder varies by VS/host) and add it
# machine-wide so every account (including a service account) sees it.
$clangDir = (Get-ChildItem "C:\Program Files*\Microsoft Visual Studio\2022\BuildTools\VC\Tools\Llvm" `
    -Recurse -Filter clang.exe -ErrorAction SilentlyContinue | Select-Object -First 1).DirectoryName
if ($clangDir) {
    $machinePath = [Environment]::GetEnvironmentVariable("Path", "Machine")
    if ($machinePath -notlike "*$clangDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$machinePath;$clangDir", "Machine")
    }
    $env:Path = "$clangDir;$env:Path"
    Write-Host "  clang found at $clangDir (added to machine PATH)" -ForegroundColor DarkGray
} else {
    Write-Host "⚠ clang.exe not found under VS Build Tools — the ring crate will fail to build." -ForegroundColor Yellow
}

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

Set-Location $RunnerDir
# Default labels come out as: self-hosted, Windows, ARM64 — exactly what ci.yml targets.
if ($AsService) {
    Write-Host "== [4/4] Register as a service (logon account: $ServiceAccount)" -ForegroundColor Cyan
    if ($ServiceAccount -ne $env:USERNAME) {
        # Installing the SERVICE only needs admin rights in THIS session (standard
        # Windows model — an admin configures a service to run as a different
        # account). rustup does NOT follow that: it just installed into
        # $env:USERPROFILE\.cargo for $env:USERNAME, and the service will run jobs
        # as $ServiceAccount, which won't see it there — `cargo`/`rustc` will be
        # missing when the first job runs. Fix with ONE of:
        Write-Host "⚠ Registering to run as '$ServiceAccount', but rustup was just installed for" -ForegroundColor Yellow
        Write-Host "  '$env:USERNAME' (this session) — the service won't see cargo/rustc there." -ForegroundColor Yellow
        Write-Host "  Before enabling the CI lane, do ONE of:" -ForegroundColor Yellow
        Write-Host "   (a) log into $ServiceAccount once and run: iwr https://win.rustup.rs/aarch64 -OutFile `$env:TEMP\r.exe; & `$env:TEMP\r.exe -y --default-host aarch64-pc-windows-msvc" -ForegroundColor Yellow
        Write-Host "   (b) set CARGO_HOME/RUSTUP_HOME as machine-wide (System) env vars pointing at a" -ForegroundColor Yellow
        Write-Host "       shared folder (e.g. C:\rustup, C:\cargo), reinstall rustup once — every" -ForegroundColor Yellow
        Write-Host "       account then shares the same toolchain (the more robust fix long-term)." -ForegroundColor Yellow
    }
    # --unattended CANNOT prompt: the password must be passed explicitly or the service
    # install dies with "Invalid configuration provided for windowslogonpassword" AFTER
    # the runner has already registered. Only works with a real local-account password
    # (an MS-account PIN won't do) — Read-Host keeps it out of shell history. `.\` scopes
    # it to a local account, per GitHub's documented --windowslogonaccount format.
    $pw = Read-Host "Password for $ServiceAccount (runner service logon)"
    .\config.cmd --url $Repo --token $Token --name "$env:COMPUTERNAME-arm64" --unattended `
        --runasservice --windowslogonaccount ".\$ServiceAccount" --windowslogonpassword $pw
    Write-Host ""
    Write-Host "Done (installed as a service). Next (from the Mac): enable the CI lane —" -ForegroundColor Green
} else {
    Write-Host "== [4/4] Register (foreground — no service, no password needed)" -ForegroundColor Cyan
    .\config.cmd --url $Repo --token $Token --name "$env:COMPUTERNAME-arm64" --unattended
    Write-Host ""
    Write-Host "Registered. Now open a NEW, NON-elevated PowerShell (matches the PATH a" -ForegroundColor Green
    Write-Host "real build would see), cd $RunnerDir, and run: .\run.cmd" -ForegroundColor Green
    Write-Host "Leave that window open — it prints 'Listening for Jobs' when ready." -ForegroundColor Green
    Write-Host "(Persistence across reboots without a service: a Task Scheduler 'At log on'" -ForegroundColor Green
    Write-Host " trigger running run.cmd needs no stored password — ask if you want that set up.)" -ForegroundColor Green
    Write-Host ""
    Write-Host "Then (from the Mac): enable the CI lane —" -ForegroundColor Green
}
Write-Host "  gh variable set WIN_ARM64_RUNNER --body 1 --repo fullspecsystems/blazeviewer"
Write-Host "Optional (only for Live Photo corpus tests): install 'HEVC Video Extensions'"
Write-Host "from the Microsoft Store and copy the test clips over."
