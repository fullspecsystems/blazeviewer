# Release checklist (cross-platform)

Go top-to-bottom. A full release ships **5 artifacts from 3 physical machines**, each
to its own **independent** feed under `downloads.fullspec.ca/photoblaze/` — there is no
single atomic "publish". Each platform self-updates only within its own channel.

| Seat | Produces | Feed dir | Auto-update |
|---|---|---|---|
| **This Mac** | macOS DMG + **both** Linux AppImages | `/mac`, `/linux` | Sparkle / JSON-feed |
| **GREMLIN** (Windows x64) | `win` Velopack channel | `/win` | Velopack |
| **Parallels VM** (Windows ARM64) | `win-arm64` Velopack channel | `/win` | Velopack |

> Linux x86_64 + aarch64 are **one command from the Mac** (OrbStack) — not two seats.
> x86_64 builds under Rosetta, aarch64 native.

Companion docs: signing setup → `release-signing.md`; runner/box provisioning →
`self-hosted-runners.md`; the narrative version → `CLAUDE.md` § *Cutting a release*.

---

## 0. Pre-flight — once, on the Mac (the hub)

- [ ] **Decide the version.** Current `crates/pb-app/Cargo.toml` = `0.1.1`. Bump the crate
      version if this ships a new number. The crate version is the numeric core; a
      `-beta.N` suffix lives **only on the git tag**, never in `Cargo.toml`.
- [ ] **Roll `CHANGELOG.md`.** Move `## [Unreleased]` → `## [<version>] - YYYY-MM-DD`,
      leave a fresh empty `[Unreleased]`, update the compare links at the bottom. Write
      **real, curated, user-facing** notes — `changelog-section.sh` reads this for release
      notes. (Never `generate_release_notes`.)
- [ ] **Write the `### Highlights` block** (first subsection of the version, above `### Added`):
      a ~7-line plain-English "what's new" for regular users. **macOS Sparkle shows *only* this**
      in the update dialog (`generate-mac-appcast.sh` extracts it; falls back to the full section
      if absent). Keep the full `Added/Changed/Fixed` detail for the curious + the GitHub release.
- [ ] **Commit + push to `main`.** (Fetch/merge `origin/main` first — a parallel Windows
      agent may push there too.)
- [ ] `git status` is **clean** — `build.rs` stamps `-dirty` on *any* porcelain output,
      **untracked files included**, and it shows in the About dialog.
- [ ] Sanity gate: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test`
      (self-hosted CI covers Win x64 + macOS + Linux; run locally if CI is paused).

> ⚠ **Every build box must `git fetch && git checkout <same commit>` before building** —
> version *and* clean-tree must match across all 5 artifacts. Do the pre-flight commit
> first, then pull that exact commit on GREMLIN and the ARM VM.

---

## 1. macOS — on this Mac

Needs: Developer ID cert + notarization creds + the **EdDSA signing key in the login
keychain** (Sparkle; back it up — losing it breaks auto-update).

- [ ] `./scripts/release-macos.sh --release`
      — builds → signs (re-signs Sparkle's nested helpers inside-out) → notarizes →
      EdDSA-signs the DMG → writes `dist/appcast.xml`.
- [ ] *(optional)* Verify the seed updates itself: `./scripts/test-sparkle-update.sh dist/PhotoBlaze-<version>.dmg`
- [ ] `./scripts/release-mac-upload.sh`
      — scp's DMG **+ appcast** to jdlien.com, repoints `PhotoBlaze-latest.dmg`.
- [ ] **Verify:**
  - [ ] `xcrun stapler validate dist/PhotoBlaze-<version>.dmg`
  - [ ] `spctl -a -t open --context context:primary-signature -vv dist/PhotoBlaze-<version>.dmg` → `source=Notarized Developer ID`
  - [ ] `curl` the feed's `appcast.xml` shows the new version; a launched older build detects → downloads → installs-on-quit.

---

## 2. Linux (x86_64 **and** aarch64) — on this Mac via OrbStack

Needs: OrbStack running; your ssh keys (the scp step runs host-side). Unsigned, but
self-updates via `latest.json`.

- [ ] `./scripts/release-linux-docker.sh both --upload`
      — builds both AppImages in an Ubuntu 26.04 container (`--features livephoto,pb-decode/libheif`),
      then publishes both + `.sha256` sidecars + shared `latest.json`, repoints
      `PhotoBlaze-latest-<arch>.AppImage` symlinks.
- [ ] **Verify:** `curl -sSL https://downloads.fullspec.ca/photoblaze/latest/linux` (x86_64)
      and `…/latest/linux-arm64` resolve to the new build; `latest.json` lists both arches.

---

## 3. Windows x64 — on GREMLIN (`Gremlin.local`)

Needs: `.env.release` (Azure Trusted Signing creds); vcpkg/libheif tree already warm at
`C:\vcpkg-pb`.

- [ ] `git fetch && git checkout <release commit>`; `git status` clean.
- [ ] **From native PowerShell** (NOT the Bash tool / Git Bash — the YubiKey `Match exec`
      ssh hook mangles paths there and the upload fails `Permission denied (publickey)`):
      `pwsh scripts/release-windows.ps1 -Upload`
      *(defaults to host arch = x64; `-Arch x64` to be explicit)*.
- [ ] If the build/sign/pack succeeded but only the **upload** failed: the feed is already
      in `dist\feed`, so re-run — it's upload-only from there.
- [ ] **Verify:** feed serves the new `releases.win.json` + `.nupkg`; a launched build self-updates.
- [ ] *(housekeeping)* Prune superseded `.nupkg`s on the server periodically.

---

## 4. Windows ARM64 — on the Parallels VM

Needs: the VM **running** (suspend = offline). Same `.env.release` signing creds.

- [ ] ⚠ **One-time prerequisite (do this before the first ARM64 release):** the ARM lane is
      **CI/test-only today and has no libheif/vcpkg**. A shipping build needs
      `--features libheif,dav1d`. Provision it once:
      `scripts/setup-libheif.ps1 -Triplet arm64-windows-static-md`
      (pins the vcpkg tree, installs libheif **+ dav1d**). Uses the `vcredist143-arm64` redist.
- [ ] `git fetch && git checkout <release commit>`; `git status` clean.
- [ ] **From native PowerShell:** `pwsh scripts/release-windows.ps1 -Upload -Arch arm64`
      — lands in the **same** `/win` feed dir as x64, but as the `win-arm64` channel.
      Velopack tracks the install channel, so the two never cross.
- [ ] **Verify:** feed serves the new `releases.win-arm64.json` + arm64 `.nupkg`.

> The `WIN_ARM64_RUNNER` gh variable only gates the **CI** lane, not the release script —
> but the VM must be powered on either way.

---

## 5. Wrap-up — back on the Mac

- [ ] **Tag** (optional but recommended as the record + the anchor for a manual macOS
      GitHub Release): `git tag -a v<version> -m "…" && git push origin v<version>`.
      A `-` in the tag (`v0.1.2-beta.1`) marks a pre-release; clean `vX.Y.Z` = full release.
      Safe now — `release.yml` no longer auto-builds on tags.
- [ ] **Never** let a tag/`gh release create` trigger a paid hosted CI run — `release.yml`
      is `workflow_dispatch`-only and stays that way.
- [ ] *(optional)* Manual macOS GitHub Release:
      `gh release create v<version> dist/PhotoBlaze-<version>.dmg* --notes-file <(bash scripts/changelog-section.sh <version>)`

---

## At-a-glance command sequence

```sh
# ── Mac (hub) ──────────────────────────────────────────────
#   pre-flight: roll CHANGELOG, bump Cargo.toml, commit+push clean
./scripts/release-macos.sh --release && ./scripts/release-mac-upload.sh
./scripts/release-linux-docker.sh both --upload

# ── GREMLIN (Windows x64), native PowerShell ───────────────
git fetch; git checkout <commit>
pwsh scripts/release-windows.ps1 -Upload

# ── Parallels VM (Windows ARM64), native PowerShell ────────
git fetch; git checkout <commit>
#   first time only: scripts/setup-libheif.ps1 -Triplet arm64-windows-static-md
pwsh scripts/release-windows.ps1 -Upload -Arch arm64

# ── Mac: tag ───────────────────────────────────────────────
git tag -a v<version> -m "release" && git push origin v<version>
```
