# PhotoBlaze → Blaze Viewer: the rename plan

**Decided 2026-07-14 (owner).** The product is **Blaze Viewer**, slug `blazeviewer`,
domain **blazeviewer.app**, repo **fullspecsystems/blazeviewer**. The app is in stealth
with exactly one user (the owner), so **orphaned installs and dead links are acceptable
collateral** — no back-compat shims, no migration code unless it's nearly free.

Owner decisions captured:

| Question | Decision |
|---|---|
| Bundle id / ProgIDs / packId | **Clean break** → `ca.fullspec.BlazeViewer` (reverse-DNS of `fullspec.ca`; Apple convention capitalizes the app segment) |
| Config dir | **Rename, no migration** — keymap gets re-created by hand |
| Update feeds | **Move, reinstall by hand** |
| About link | **blazeviewer.app** (repo is private) |
| `pb-*` crate prefixes | **Keep forever** — internal, brand-neutral, renaming is pure churn |

---

## The one rule

> **Do not mass-`sed` "photoblaze".** 381 occurrences of `PhotoBlaze` + 74 of
> `photoblaze` are *not* interchangeable. Roughly a third are **artifact names**
> (`PhotoBlaze.app`, `photoblaze.exe`) that documentation correctly describes *today*.
> Renaming the prose before the artifact makes the docs wrong; renaming the artifact
> without the scripts breaks the build. They move together, per phase, or not at all.

---

## Phase A — URL corrections ✅ DONE (`8da7ad31`)

Strings that were already **factually wrong**, independent of the rename:

- `github.com/jdlien/photoblaze` → `github.com/fullspecsystems/blazeviewer` (CHANGELOG
  compare links, `setup-windows-arm64-runner.ps1`, `self-hosted-runners.md`)
- About box (both shells) → `blazeviewer.app`
- egui screenshot harness sample QR → `blazeviewer.app`
- Documented that the runner's Windows **service name is fixed at registration time**:
  a pre-rename runner is still `actions.runner.jdlien-photoblaze.GREMLIN-win` on GREMLIN
  until re-registered.

---

## Phase B — Internal + cosmetic (no user-visible identity, no reinstall)

Safe to land incrementally, in any order. **None of this changes what the OS sees.**

### B1. Introduce a single product-name constant *first*

There is **no central product name today** — `pb-app-core` has `TAGLINE` but no `APP_NAME`.
The name is hardcoded in ~381 places, which is *why* this rename is expensive. Fix the
cause before the symptom:

```rust
// crates/pb-app-core/src/lib.rs
pub const APP_NAME: &str = "Blaze Viewer";   // display
pub const APP_SLUG: &str = "blazeviewer";    // paths, exe, feed
pub const APP_ID:   &str = "ca.fullspec.BlazeViewer";
```

Then have `config.rs`, `single_instance.rs`, `default_app.rs`, `update.rs`, and the
dialogs consume it. This makes Phase C a handful of edits instead of a sweep, and it
means the *next* rename is one line. **Do this before C.**

### B2. Mechanical renames

- **Docs prose** — README, CLAUDE.md, AGENTS.md, CONTRIBUTING, THIRD-PARTY-NOTICES,
  `.taskmaster/docs/*` (31 files). ⚠ Leave `PhotoBlaze.app` / `photoblaze.exe` mentions
  **alone** until C renames the artifact — otherwise the docs lie.
- **LICENSE** — header says `PhotoBlaze — End-User License Agreement`, © **JD Lien**;
  the About box says © **FullSpec Systems Inc.** These already disagree. Pick one owner
  while renaming. (Flagged, not decided.)
- **Swift target** `PhotoBlazeMac` → `BlazeViewerMac`: `mac/Package.swift`,
  `mac/Sources/PhotoBlazeMac/` → `BlazeViewerMac/`, `PhotoBlazeMacApp.swift`. Touches
  `build-swift-host.sh`. Product-internal; the *bundle* name is C.
- **Icon assets** — `icons/photoblaze-icon-*.svg`, `crates/pb-app/icons/photoblaze.{ico,png}`,
  `photoblaze-splash.jpg`. Referenced by `release-windows.ps1`, `build-macos-icons.sh`,
  `build.rs`. Rename file + reference together.
- **Single-instance mutex** `Local\PhotoBlaze.SingleInstance` + window class. Windows-only.
  Worst case: an old and new build running simultaneously don't see each other. Harmless.

---

## Phase C — The identity cutover (breaks installs — do it ALL in ONE release)

Every item here forces a reinstall. Doing them in separate releases forces *several*
reinstalls. **Land them together, cut one release, reinstall once per box.**

| What | From | To |
|---|---|---|
| Bundle id | `com.jdlien.PhotoBlaze` | `ca.fullspec.BlazeViewer` |
| macOS app | `PhotoBlaze.app` | `BlazeViewer.app` |
| Windows exe | `photoblaze.exe` | `blazeviewer.exe` |
| Velopack | `--packId PhotoBlaze` | `--packId BlazeViewer` |
| ProgIDs | `PhotoBlaze.Image/.Archive/.Video` | `BlazeViewer.*` |
| Registry | `SOFTWARE\PhotoBlaze\Capabilities` | `SOFTWARE\BlazeViewer\Capabilities` |
| Config (Win) | `%APPDATA%\PhotoBlaze` | `%APPDATA%\BlazeViewer` |
| Config (mac) | `~/Library/Application Support/PhotoBlaze` | `…/BlazeViewer` |
| Config (Linux) | `~/.config/photoblaze` | `~/.config/blazeviewer` |
| Feed (Win) | `…/photoblaze/win` | `…/blazeviewer/win` |
| Feed (mac) | `…/photoblaze/mac/appcast.xml` | `…/blazeviewer/mac/appcast.xml` |
| Feed (Linux) | `…/photoblaze/linux` | `…/blazeviewer/linux` |

### ⚠ Uninstall the OLD app BEFORE installing the new one

This is the one operational detail that's easy to get wrong and **permanent** if you do.

The old build's HKCU registry — ProgIDs, `RegisteredApplications`, Capabilities — is
cleaned by **its own Velopack uninstall hook**. A new `packId` is a *different app* to
Velopack, so installing Blaze Viewer does **not** trigger PhotoBlaze's uninstaller. If
you delete the old install folder by hand, those ProgIDs are orphaned in your registry
**forever**, and stale entries keep showing up in Windows' "Open with" list.

**Correct order, per Windows box:**
1. Uninstall PhotoBlaze via Settings ▸ Apps (runs `--veloapp-uninstall` → unregisters ProgIDs)
2. Verify: `Get-ChildItem HKCU:\SOFTWARE\Classes | ? Name -match 'PhotoBlaze'` → empty
3. Install BlazeViewer

macOS is easier — drag `PhotoBlaze.app` to the Trash, then `lsregister -kill -r -domain local -domain user`
to flush stale LaunchServices associations. Linux: delete the old AppImage + `~/.config/photoblaze`.

### Sparkle note

The **EdDSA key is not tied to the app name** — it stays valid across the rename. Do **not**
regenerate it. (It lives only in the release Mac's login keychain; if it isn't backed up
via `generate_keys -x`, do that *before* touching any of this.)

The bundle-id change means the renamed build is a **new app** to Sparkle: the installed
PhotoBlaze will never auto-update into BlazeViewer. That's expected and accepted.

---

## Phase D — Distribution hosting (open question)

**Owner's question:** keep paying DigitalOcean egress for `downloads.fullspec.ca`, or host
the bundles on GitHub for free bandwidth?

### The constraint nobody mentions until it hurts

**The feed URL is burned into every installed binary.** `FEED_URL`, `FEED_BASE`, and
`SUFeedURL` are compiled in. Bytes can live anywhere and move freely; **the feed URL can
never move** without orphaning every install. So:

> **Own the URL. Rent the bytes.**

### Recommended shape

- **Feed URL** → `downloads.blazeviewer.app`, a Cloudflare Worker on the domain you own.
  Serves the tiny metadata (`appcast.xml`, `latest.json`, `releases.win.json`) directly,
  and **302-redirects** asset requests to GitHub. Free tier covers this many times over.
- **Bytes** → a **public** `fullspecsystems/blazeviewer-releases` repo. Code stays private;
  only artifacts are public. GitHub Release bandwidth is free and CDN-backed (2 GB/asset
  limit — our DMG/AppImage are far under).

Why the redirect rather than pointing feeds straight at GitHub:
- Velopack/Sparkle/our Linux updater all see a **normal flat feed on your own domain**
- Cloudflare serves only a ~200-byte redirect; GitHub's CDN serves the megabytes
- If you ever leave GitHub, you change the Worker — **installed apps never notice**
- `…/releases/latest/download/<name>` gives a stable "latest" URL (the symlink equivalent)

### ⚠ Must verify before committing to this

I'm confident about the shape, not these specifics — **check them, don't trust me**:
1. **Velopack + 302 on `.nupkg`.** Velopack's flat feed may resolve package paths
   *relative to the feed base*. If it won't follow a cross-origin redirect, use Velopack's
   native **`GithubSource`** for Windows instead (first-class, `vpk upload github`) and
   accept that Windows' feed lives on GitHub.
2. **Private-repo assets need auth** — this is why the releases repo must be *public*.
   Confirm that's acceptable before creating it (it makes release artifacts + version
   history public while the source stays closed).
3. Sparkle following a 302 to a GitHub asset — standard and widely used, but smoke-test
   with `scripts/test-sparkle-update.sh` before relying on it.

**Sequencing:** D is independent of B and C. But if you're doing the one-time manual
reinstall in C anyway, **do D at the same time** — that reinstall is the only free moment
to change the feed URL without stranding anything.

---

## Suggested order

1. **B1** (the `APP_NAME` constant) — makes everything after it cheap
2. **B2** (docs, Swift target, icons, mutex) — incremental, low-risk
3. **D decision** — verify the Velopack redirect question
4. **C + D together** — one release, one reinstall per box, done

## Not changing

- **`pb-*` crate names** — internal, brand-neutral, already correct.
- **`PB_*` env vars** (`PB_LIVE_TEST_MOV`, `PB_UPDATE_FEED`) — same reasoning.
- The `pb-app` / `pb-app-core` split, `PbKey`, etc.
