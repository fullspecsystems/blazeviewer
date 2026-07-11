# Self-hosted CI runners + local releases

_Why (2026-07-03): the hosted-minute bill (~260 billable min per full CI run, 78% of it
the macOS 10× multiplier) exhausted the GitHub Actions credits during the NS0–NS2 push.
The `windows` and `mac-swift` CI lanes now run on the owner's own machines — free,
unmetered, and the actual target hardware. Releases (signing included) build locally:
`scripts/release-macos.sh` on the Mac, `scripts/release-windows.ps1` on Windows._

## Layout

| Lane | Runs on | Labels |
|---|---|---|
| `windows` | GREMLIN (the Windows dev box), runner in `C:\actions-runner` | `self-hosted, Windows, X64` |
| `mac-swift` | the M2 Max (setup below) | `self-hosted, macOS, ARM64` |
| `core-linux` | `ubuntu-latest` (hosted, ~0.5 min at 1×) | `continue-on-error` until the account's billing works again |

Runner state persists between runs, so the GitHub cache actions were removed from the
self-hosted lanes: the runner's `_work` tree keeps cargo's target warm, and libheif
lives on disk (`C:\vcpkg-pb`, seeded from `~\vcpkg`; a fresh machine builds it once via
the "Ensure libheif" step). `release.yml` still targets hosted runners — it's dormant
while releases are local, and works again whenever billing does.

## Windows runner (done 2026-07-03)

Registered as `GREMLIN-win` in `C:\actions-runner`. To re-register (new machine, or
converting to a service — a token is single-use and expires in an hour):

```powershell
$token = gh api -X POST repos/jdlien/photoblaze/actions/runners/registration-token --jq .token
cd C:\actions-runner
.\config.cmd remove --token $token   # only if already configured
.\config.cmd --url https://github.com/jdlien/photoblaze --token $token `
  --name "$env:COMPUTERNAME-win" --unattended
.\run.cmd    # foreground; or install the service (below)
```

**Run it as a Windows service** (survives logoff/reboot; needs an ELEVATED shell —
re-run config with `--runasservice`, using your own account so the runner sees your
rustup/vcpkg/SDK environment):

```powershell
# elevated PowerShell:
$token = gh api -X POST repos/jdlien/photoblaze/actions/runners/registration-token --jq .token
cd C:\actions-runner
.\config.cmd remove --token $token
.\config.cmd --url https://github.com/jdlien/photoblaze --token $token `
  --name "$env:COMPUTERNAME-win" --unattended `
  --runasservice --windowslogonaccount $env:USERNAME
# it prompts for the account password once; the service auto-starts at boot
```

Gotchas learned setting this up:
- **`shell: bash` resolves to the WSL shim** (`WindowsApps\bash.EXE`) on a stock
  Windows PATH and chokes on the runner's Windows-style script paths. ci.yml prepends
  `C:\Program Files\Git\bin` via `GITHUB_PATH` as its first step — keep that step.
- The job env is the runner process's env: rustup, VS Build Tools, git, and pwsh all
  come from the machine. `VCPKG_ROOT=C:\vcpkg-pb` is set by the workflow.
- Jobs run even while the account's hosted billing is broken — the billing lock only
  blocks GitHub-hosted runners.

## Windows ARM64 runner (task #75)

A Windows 11 ARM64 guest VM on the Mac (Fusion/UTM/Parallels all evaluated — Parallels
won on speed/graphics; the hypervisor doesn't matter to this section, it's all guest-OS
config), so the workspace tests run **natively on aarch64-pc-windows-msvc** (the x64
build already runs on Snapdragon machines via Prism emulation, but emulation taxes the
SIMD decode paths — the thing PhotoBlaze exists for; a native lane keeps an ARM64 build
honest before it ever ships). Distinct from GREMLIN: this lane only runs tests
(fmt/clippy are platform-identical and stay on the x64 lane); no libheif/vcpkg yet
(`arm64-windows-static-md` triplet is the known follow-up risk when release builds
become a goal).

**⚠ `ring` needs `clang` on this target, not just MSVC.** The `ring` crate (pulled in
transitively — rustls/reqwest etc.) hard-requires clang to compile its assembly on
`aarch64-pc-windows-msvc`; `cl.exe` alone fails deep in `cargo test` with "failed to
find tool clang". GREMLIN (x64) never hit this because ring doesn't need clang there.
The bootstrap script installs the VS "C++ Clang Compiler for Windows" component
(`Microsoft.VisualStudio.Component.VC.Llvm.Clang`) and adds its bin dir to the
machine-wide PATH automatically (ring doesn't auto-discover the VS-bundled copy — it
has to be resolvable on PATH). If a runner predates this fix, rerun just the VS Build
Tools winget line with that component added, `Get-ChildItem` for `clang.exe` under
`...\BuildTools\VC\Tools\Llvm`, add its folder to the machine PATH, and
`Restart-Service actions.runner.*`.

**⚠ Foreground runner, not a service — by design.** A personal Windows install today is
very likely signed into a Microsoft account (unlocked with a Hello PIN), and the
GREMLIN-style Windows-service registration needs the account's real *password* for
`--windowslogonpassword` — a PIN doesn't work there, and `--unattended` can't prompt for
one, so it fails *after* already registering the runner (hit this twice, on two separate
VM rebuilds). The bootstrap script's default now skips the service entirely: it
registers the runner, then you run `.\run.cmd` yourself in a normal window and leave it
open. `-AsService` is still there for a deliberately-local-account VM with a real
password.

**Preferred for a "set up properly" VM: a dedicated local service account** (e.g.
`gh-runner`, local admin, real password — created once regardless of whether your own
login is MS-account-tied). Run the script from **your own** elevated session (repo
already cloned there, no need to switch users) with
`-AsService -ServiceAccount gh-runner` — installing a *service* only needs admin rights
in the invoking session (standard Windows model: an admin configures a service to log on
as a different account), so this works fine. The one thing that does NOT carry over:
rustup is per-user, so it's installed for whoever ran the script, not for
`-ServiceAccount` — if they differ, the script prints exactly what to do (log into the
service account once and rerun the two-line rustup install, or point
`CARGO_HOME`/`RUSTUP_HOME` at a shared machine-wide folder as System env vars — the more
robust fix if more accounts join later).

Setup, in order:

1. **VM**: Windows 11 ARM64 ISO (not the default x64 one), UEFI Secure Boot ON, the
   vTPM's partial VM encryption, local account via `start ms-cxh:localonly` at OOBE
   (Shift+F10) *if you want a local account* — otherwise a Microsoft-account sign-in is
   completely fine for this runner (see above; it just means the foreground path, not
   the service). License recycled from the Microsoft account via Settings → Activation →
   Troubleshoot → "I changed hardware on this device recently".
   **Rebuilds: skip the whole OOBE gauntlet** (MS-account push, privacy-toggle parade,
   OEM/upsell screens) with `scripts/windows-arm64-autounattend.xml` — instructions in
   the file's header comment (attach as a tiny second CD image; creates the local
   admin directly, declines every "express setting"). Only relevant if you're deliberately
   going local-account; skip it entirely for an MS-account install.
2. **Bootstrap** (installs Git + VS Build Tools ARM64 + rustup + downloads/registers the
   runner): generate a token on the Mac —
   `gh api -X POST repos/jdlien/photoblaze/actions/runners/registration-token --jq .token`
   — then in an **elevated** PowerShell in the VM:
   `.\scripts\setup-windows-arm64-runner.ps1 -Token <paste>` (grab the script via a
   shared folder or `Invoke-WebRequest` from the repo). When it finishes it prints the
   next step: open a **new, non-elevated** PowerShell (so the runner sees the same PATH
   a real build would), `cd C:\actions-runner`, run `.\run.cmd`, and leave that window
   open — it prints "Listening for Jobs" when ready. If a prior attempt left a
   half-registered/offline runner behind, `.\config.cmd remove --token <fresh token>`
   before retrying (or remove it from the Mac: `gh api -X DELETE
   repos/jdlien/photoblaze/actions/runners/<id>`).
3. **Enable the lane**: `gh variable set WIN_ARM64_RUNNER --body 1 --repo jdlien/photoblaze`.
   The `windows-arm64` job is `if`-gated on that variable so a paused/unregistered VM
   never leaves CI runs queued open — **set it back to `0` whenever the VM will be off
   for a while**, and jobs skip cleanly instead of hanging.
4. Optional, only for the Live Photo corpus tests (they self-skip otherwise): install
   **HEVC Video Extensions** from the Store and copy `test-images/live/` over.

The default runner labels (`self-hosted, Windows, ARM64`) match the lane. Fusion note:
the VM must be *running* for jobs to pick up — suspend counts as offline (which is
exactly what the toggle variable is for).

## macOS runner (to do on the Mac)

```bash
mkdir -p ~/actions-runner && cd ~/actions-runner
ver=$(gh api repos/actions/runner/releases/latest --jq '.tag_name' | tr -d v)
curl -o runner.tar.gz -L "https://github.com/actions/runner/releases/download/v${ver}/actions-runner-osx-arm64-${ver}.tar.gz"
tar xzf runner.tar.gz && rm runner.tar.gz
token=$(gh api -X POST repos/jdlien/photoblaze/actions/runners/registration-token --jq .token)
./config.sh --url https://github.com/jdlien/photoblaze --token "$token" \
  --name "$(hostname -s)-mac" --unattended
./svc.sh install && ./svc.sh start   # launchd service, runs at login
```

The default labels (`self-hosted, macOS, ARM64`) match the `mac-swift` lane already.
The lane needs Xcode CLTs + rustup, which the machine has. Until this runner is
registered, `mac-swift` jobs queue for up to 24 h and then fail — expected, not a code
failure.

## Cutting a release locally (both installers)

1. Roll `CHANGELOG.md`, bump `crates/pb-app/Cargo.toml`, tag + push as usual
   (CLAUDE.md → "Cutting a release"). The tag still fires release.yml, which fails
   while hosted billing is down — ignore it, or delete the workflow runs.
2. **Windows box:** `pwsh scripts/release-windows.ps1` → `dist\PhotoBlaze-<v>-x86_64.msi`
   (+ `.sha256`). Signing needs either `AZURE_*` service-principal vars in
   `.env.release` or an `az login` session (role: Trusted Signing Certificate Profile
   Signer). Without either it produces an unsigned MSI and says so.
3. **Mac:** `./scripts/release-macos.sh --release` → `dist/PhotoBlaze-<v>.dmg`
   (+ `.sha256`), signed + notarized + stapled (keychain identity + notary profile).
4. Publish from either machine with both artifacts in `dist/`:

```bash
bash scripts/changelog-section.sh <version> > /tmp/notes.md
gh release create v<version> dist/PhotoBlaze-*.msi* dist/PhotoBlaze-*.dmg* \
  --title v<version> --notes-file /tmp/notes.md   # add --prerelease for -beta tags
```

5. Verify like CLAUDE.md says: both installers + `.sha256` attached, DMG stapled
   (`xcrun stapler validate`), MSI signature (`signtool verify /pa`).
