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
