# Release signing & publishing (Windows MSI + macOS DMG)

How the signed PhotoBlaze installers get built, and the one-time setup to turn on
signing on each platform — **Azure Trusted Signing** (Windows) and **Apple
Developer ID + notarization** (macOS). The pipeline lives in
`.github/workflows/release.yml`; the macOS codesign/DMG/notarize steps are in
`scripts/release-macos.sh` (so they run identically on a dev machine).

## Pipeline (already wired)

On a `v*` tag push (or a manual **Run workflow** / `workflow_dispatch`):

1. **test** job — `cargo fmt --check`, `clippy -D warnings`, `cargo test` (the
   release can't ship code that fails CI).
2. **windows-msi** job — installs WiX v3.14 (pinned, not relying on the runner),
   checks the tag matches the crate version, builds the release `.exe`,
   **signs the `.exe`**, packages the MSI with cargo-wix, **signs the MSI**,
   writes a `.sha256` sidecar, and attaches the MSI + checksum to the GitHub
   Release.

Signing **auto-skips** when the secrets below are absent — you still get an
(unsigned) MSI artifact, with a warning. So you can dry-run the whole build path
before any Azure setup.

## Fast path: reuse the existing setup (already done)

The owner already runs Azure Trusted Signing for the **secrt** project, and a
code-signing certificate profile is **not app-specific** — one profile signs
every app. PhotoBlaze's workflow is already pointed at that setup:

- endpoint `https://wus.codesigning.azure.net/` · account `jdlien-signing` ·
  profile `jdlien-public-trust` (hardcoded in `release.yml` — not secret).
- The Trusted Signing account, the public-trust profile, the identity validation
  (the slow part), and a working signer service principal **all already exist**.

So enabling signing for PhotoBlaze is just: **add the same three secrets this
repo's `secrt` sibling uses** — `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`,
`AZURE_CLIENT_SECRET` — under Settings → Secrets and variables → Actions. (If the
existing client secret has expired, mint a new one on the same app registration;
see step 2 below.) Then jump to **Verify**.

The full setup below is kept for reference / reproducing from scratch.

## One-time setup: enable Azure Trusted Signing (reference)

> Trusted Signing (formerly Azure Code Signing) signs via Microsoft-managed
> certificates — there are no cert files to store. You authenticate from CI with
> a service principal.

### 1. Trusted Signing account + certificate profile (Azure Portal)

- Create a **Trusted Signing account**. Pick a region and note it — the region
  maps to the signing endpoint URL, e.g. East US → `https://eus.codesigning.azure.net/`.
- Complete an **Identity Validation** (Public Trust) on the account. **This is the
  long pole** — approval can take days and has eligibility rules (individual vs.
  organization identity history). Signing will not work until it shows
  **Completed**.
- Create a **Certificate Profile** (type: *Public Trust*) under the validated
  identity. Note its **name**.

### 2. Service principal (Microsoft Entra app registration)

- Entra ID → **App registrations** → **New registration** (e.g. `photoblaze-signing`).
- Note the **Application (client) ID** and **Directory (tenant) ID**.
- **Certificates & secrets** → **New client secret** → copy the **secret value**
  (shown only once).

### 3. Grant the signer role

- On the Trusted Signing account (or its resource group) → **Access control (IAM)**
  → **Add role assignment** → role **"Trusted Signing Certificate Profile Signer"**
  → assign to the `photoblaze-signing` service principal.

### 4. GitHub repo secrets

Repo → **Settings → Secrets and variables → Actions → Secrets**:

| Name                  | Value                              |
| --------------------- | ---------------------------------- |
| `AZURE_TENANT_ID`     | Directory (tenant) ID              |
| `AZURE_CLIENT_ID`     | Application (client) ID            |
| `AZURE_CLIENT_SECRET` | the client secret **value**        |

(No variables needed — the endpoint / account / profile are hardcoded in
`release.yml`, since they aren't secret.)

### 5. Verify

1. **Unsigned dry run** — Actions → **release** → **Run workflow**
   (`workflow_dispatch`). Confirms the build + WiX + MSI path end to end; download
   the `photoblaze-msi` artifact (it logs an "UNSIGNED" warning).
2. **Signed tagged release** — make sure `crates/pb-app/Cargo.toml` `version`
   equals the tag, then:
   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```
3. Download the MSI and verify the signature (Windows SDK `signtool`):
   ```sh
   signtool verify /pa /v PhotoBlaze-0.1.0-x86_64.msi
   ```
   Expect a valid chain and an RFC3161 timestamp.
4. Install on a clean Windows 11 VM and check: "Open with" shows PhotoBlaze for a
   `.jpg`; the folder right-click "Open with PhotoBlaze" verb; the Start-menu
   shortcut + Add/Remove Programs icon; clean uninstall; and an upgrade
   (install `0.1.0`, then `0.1.1` over it).

## macOS DMG (Apple Developer ID + notarization)

The `macos-dmg` job builds `PhotoBlaze.app` (`scripts/bundle-macos.sh` — Liquid Glass
icon via `actool` + the flat `.icns`), then `scripts/release-macos.sh` **Developer ID
codesigns** it under the hardened runtime, packages a **DMG**, **notarizes** it
(`notarytool`), and **staples** the ticket. Like Windows, it **auto-skips** signing +
notarization when the secrets are absent (you still get an unsigned DMG, with a warning).

### Fast path: reuse your existing Apple setup

You already notarize `unifi-protect-viewer`, so the slow parts — the **Developer ID
Application** certificate, the Apple Developer membership, the app-specific password — all
exist. GitHub secrets are **per-repo**, so just add the **same values** to this repo. The
helper sets all five securely (reads the `.p12` + app-specific passwords with `read -rs` —
never echoed, never in argv/history):

```sh
./scripts/setup-signing-secrets.sh ~/Downloads/certs/<developer-id-application>.p12
```

It verifies the `.p12` is a *Developer ID Application* cert (not Installer / Apple
Distribution), checks it isn't expired, auto-detects the **Team ID** from the cert, and
sets the secrets via `gh`.

### GitHub repo secrets (macOS)

| Name                          | Value                                                    |
| ----------------------------- | -------------------------------------------------------- |
| `CSC_LINK`                    | base64 of the **Developer ID Application** `.p12`        |
| `CSC_KEY_PASSWORD`            | that `.p12`'s export password                            |
| `APPLE_ID`                    | Apple ID e-mail (notarization)                           |
| `APPLE_APP_SPECIFIC_PASSWORD` | app-specific password (appleid.apple.com → Sign-In)      |
| `APPLE_TEAM_ID`               | Developer Team ID (the `OU` in the cert)                 |

> Use a **fresh** app-specific password and don't commit it anywhere (an exposed one
> should be revoked + reissued). `notarytool` reads it from the env at run time only.

### Runner / toolchain

`runs-on: macos-15` (arm64). The Liquid Glass icon needs **Xcode 26+**'s `actool`; the job
selects the newest Xcode on the runner. If 26 isn't present yet, `bundle-macos.sh` falls
back to the flat `.icns` (graceful) — **cut the release locally** (your machine has Xcode 26
+ the cert) for a guaranteed-glass DMG: `./scripts/release-macos.sh`.

### Verify (macOS)

1. **Unsigned dry run** locally: `./scripts/release-macos.sh` → `dist/PhotoBlaze-<v>.dmg`
   (logs "UNSIGNED"). Confirms the bundle + DMG path.
2. After setting the secrets, a `v*` tag (or **Run workflow**) produces a signed, notarized
   DMG. Verify on a **clean** Mac:
   ```sh
   spctl -a -vvv -t open --context context:primary-signature PhotoBlaze-0.1.0.dmg  # → accepted, source=Notarized Developer ID
   xcrun stapler validate PhotoBlaze-0.1.0.dmg                                       # → The validate action worked!
   codesign --verify --deep --strict --verbose=2 /Applications/PhotoBlaze.app
   ```
   Mount, drag to Applications, launch — **zero Gatekeeper warnings**, no right-click-Open needed.

## Notes

- **SmartScreen.** A valid signature builds publisher reputation. Brand-new
  publisher identities may still see a first-download warning until reputation
  accrues; Trusted Signing uses Microsoft's PKI, which generally clears quickly.
- **WiX.** The workflow installs WiX v3.14 binaries itself, so it doesn't depend
  on whatever toolset the GitHub runner image happens to ship.
- **Version source of truth.** cargo-wix stamps the MSI version from Cargo
  metadata, not the tag; the workflow fails fast if the tag and crate version
  disagree.
- **Default file handler.** Windows forbids silently seizing a default handler
  (the SID-salted UserChoice hash). The MSI only registers PhotoBlaze as an
  "Open with" *candidate*; making it the default is a user action (an in-app
  "Set as default" deep-link to `ms-settings:defaultapps` is task #14).
