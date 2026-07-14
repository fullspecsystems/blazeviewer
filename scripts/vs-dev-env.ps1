<#
.SYNOPSIS
    Put the current process into a Visual Studio Developer environment for FFmpeg's bindgen.

.DESCRIPTION
    Building with `--features ffprobe` (task #100) is the first thing in this repo that needs a
    *Developer shell* rather than just a linker: ffmpeg-sys-next runs bindgen, bindgen runs its
    own clang, and that clang reads INCLUDE to find stdint.h. A plain `cargo build` never needed
    this, because rustc locates the MSVC linker itself.

    Sets, in the current process (env vars are process-wide, so `& scripts/vs-dev-env.ps1` is
    enough — no dot-sourcing needed):
      * INCLUDE / LIB / PATH  — via Enter-VsDevShell
      * LIBCLANG_PATH         — Visual Studio already ships a libclang; no separate LLVM install.
    Under GitHub Actions ($env:GITHUB_ENV set) it also exports them to later steps.

    ⚠ Why this does not just call `vswhere -latest -products *`: that is the pattern everyone
    copies, and on a box with SQL Server Management Studio installed it returns *SSMS* — SSMS is
    built on the VS shell, so it matches `-products *`, and it has no C++ tools. The failure is
    a confusing "module not found" for DevShell.dll. So: require the VC tools component, and
    then verify the install really has what we need before trusting it.

.PARAMETER Arch
    x64 or arm64. Builds are native (no cross toolchain), so this is both target and host.
#>
[CmdletBinding()]
param(
    [ValidateSet('x64', 'arm64')]
    [string]$Arch = $(if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' })
)

$ErrorActionPreference = 'Stop'

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) {
    throw "vswhere not found at $vswhere — is Visual Studio installed? FFmpeg's bindgen needs its headers + libclang."
}

# Ask for installs that actually carry C++ tools. Either component is acceptable: an ARM64 VS
# may ship only the ARM64 tools, an x64 VS only the x86/x64 ones, and plenty of boxes have both.
$vcComponents = @(
    'Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
    'Microsoft.VisualStudio.Component.VC.Tools.ARM64'
)
$candidates = & $vswhere -latest -products * -requires $vcComponents -requiresAny -property installationPath

# Trust but verify: the install must have the DevShell module and a libclang for this arch.
$llvmDir = if ($Arch -eq 'arm64') { 'ARM64' } else { 'x64' }
$vsPath = $candidates | Where-Object {
    (Test-Path "$_\Common7\Tools\Microsoft.VisualStudio.DevShell.dll") -and
    (Test-Path "$_\VC\Tools\Llvm\$llvmDir\bin\libclang.dll")
} | Select-Object -First 1

if (-not $vsPath) {
    $found = if ($candidates) { ($candidates -join '; ') } else { '(none)' }
    throw @"
No usable Visual Studio C++ install found for $Arch.
Checked: $found
Each needs BOTH:
  * Common7\Tools\Microsoft.VisualStudio.DevShell.dll   (Developer shell)
  * VC\Tools\Llvm\$llvmDir\bin\libclang.dll               (VS component: 'C++ Clang tools for Windows')
Install the missing component via the Visual Studio Installer, then re-run.
"@
}

# ⚠ Enter-VsDevShell OVERWRITES VCPKG_ROOT with the vcpkg bundled inside Visual Studio — a tree
# that has none of our pinned ports. That silently redirects both our own build.rs and
# ffmpeg-sys-next's `vcpkg` crate lookup at the wrong tree, so capture the intended root first
# and put it back afterwards. (Setting VCPKG_ROOT before calling this script does NOT survive.)
# When it's unset, mirror build.rs's own `~/vcpkg` fallback rather than inheriting VS's value:
# ffmpeg-sys-next has no such fallback, so leaving VS's root in place turns a clear "set
# VCPKG_ROOT" error into a confusing "FFmpeg not found in a perfectly real vcpkg tree".
$vcpkgRoot = $env:VCPKG_ROOT
if (-not $vcpkgRoot) {
    $default = Join-Path $HOME 'vcpkg'
    if (Test-Path (Join-Path $default 'installed')) { $vcpkgRoot = $default }
}

# Cosmetic: VsDevCmd.bat shells out to a bare `vswhere.exe`, so without the Installer dir on PATH
# it prints "'vswhere.exe' is not recognized" into every build log. Harmless — nothing we need
# comes from that call — but it reads like a failure, so keep it out of release/CI output.
$installerDir = Split-Path $vswhere -Parent
if ($env:PATH -notlike "*$installerDir*") { $env:PATH = "$installerDir;$env:PATH" }

Import-Module "$vsPath\Common7\Tools\Microsoft.VisualStudio.DevShell.dll"
# -SkipAutomaticLocation: Enter-VsDevShell otherwise cd's to the VS "source path", which would
# yank the caller out of the repo mid-build.
Enter-VsDevShell -VsInstallPath $vsPath -DevCmdArguments "-arch=$Arch -host_arch=$Arch" -SkipAutomaticLocation | Out-Null
$env:LIBCLANG_PATH = "$vsPath\VC\Tools\Llvm\$llvmDir\bin"

if ($vcpkgRoot) { $env:VCPKG_ROOT = $vcpkgRoot }
elseif (Test-Path Env:\VCPKG_ROOT) { Remove-Item Env:\VCPKG_ROOT }

Write-Host "==> VS Developer environment ($Arch): $vsPath"
Write-Host "==> VCPKG_ROOT: $(if ($env:VCPKG_ROOT) { $env:VCPKG_ROOT } else { '(unset — FFmpeg will not be found)' })"

# GitHub Actions runs each step in a fresh shell, so hand the env to the later steps.
if ($env:GITHUB_ENV) {
    "INCLUDE=$env:INCLUDE"             | Add-Content $env:GITHUB_ENV
    "LIB=$env:LIB"                     | Add-Content $env:GITHUB_ENV
    "LIBCLANG_PATH=$env:LIBCLANG_PATH" | Add-Content $env:GITHUB_ENV
    if ($env:VCPKG_ROOT) { "VCPKG_ROOT=$env:VCPKG_ROOT" | Add-Content $env:GITHUB_ENV }
    Write-Host "==> exported INCLUDE / LIB / LIBCLANG_PATH / VCPKG_ROOT to GITHUB_ENV"
}
