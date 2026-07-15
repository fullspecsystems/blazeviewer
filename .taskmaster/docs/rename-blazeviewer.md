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

### Artifact + CLI naming (decided 2026-07-14)

**Spaces appear on macOS and nowhere else** — that isn't an inconsistency, it's each
platform's own convention:

| Surface | Name | Why |
|---|---|---|
| macOS bundle | **`Blaze Viewer.app`** | Mac convention has spaces (`Visual Studio Code.app`); Finder shows the exact brand |
| macOS `CFBundleExecutable` | **`Blaze Viewer`** | drives the Activity Monitor process name |
| Windows exe | **`blazeviewer.exe`** | Windows exes never have spaces (`Code.exe`, `chrome.exe`, `Photoshop.exe`) — a space means quoting every command line forever |
| Linux binary + `.desktop` `Exec` | **`blazeviewer`** | lowercase, no spaces |
| Linux AppImage | **`BlazeViewer-<v>-<arch>.AppImage`** | you `chmod +x` and type it |
| CLI (macOS/Linux symlink) | **`blaze`** | the only name a human types |
| Display name / Start-menu / `packTitle` | **`Blaze Viewer`** | what Spotlight + Windows Search actually match |

**The premise that got tested and failed:** a short `blaze.exe` was floated for
findability. It doesn't help — **Spotlight matches `CFBundleDisplayName`/`CFBundleName`,
and Windows Search matches the Start-menu shortcut (`--packTitle`)**. Neither indexes the
executable name. Typing "blaze" finds "Blaze Viewer" regardless. What the exe name *does*
touch — Task Manager, crash reports, Event Viewer, AV/EDR allowlists — all favour the
unambiguous long form. Short pays only where you type, which is the CLI.

Precedent: `CliTool.swift` already cites "the VS Code / iTerm pattern" — VS Code ships
`Visual Studio Code.app` with a `code` CLI. Long artifact, short command.

⚠ **There is no separate Windows CLI.** `pb-cli` is a *library* that `pb-app` links, so
`blazeviewer.exe` **is** the Windows CLI. Nothing on Windows puts the install dir on PATH,
so a `blaze.exe` shim would be new surface for no gain — not planned.

⚠ `blaze` on PATH can collide with Google's pre-Bazel `blaze`. Accepted: the symlink is
opt-in behind an explicit menu action, and `CliTool.swift` already detects a foreign
symlink and reports rather than clobbers it.

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

### "Hold to Fly" → "Hold to Blaze" ✅ DONE

Owner call 2026-07-14: the fast-flick feature is named **Blaze**; "Hold to Fly" becomes
**"Hold to Blaze"**, and the verb is "blaze", not "fly". Landed across the code comments,
the canonical docs (README / CLAUDE.md / AGENTS.md / roadmap), and the one `flying` local
+ one test name. It was comments-only — no identifiers of substance, **no user-visible UI
copy said "fly" at all**.

Two things the sweep turned up, both already leaning this way:
- `app_core_impl.rs` already commented `"Blaze mode" = actually flying`.
- The archived Settings plan already grouped these options under **BLAZE**.

**Rule applied:** historical records (`CHANGELOG.md`, `.taskmaster/archive.json`,
`.taskmaster/docs/*-plan.md`) keep the old term — they describe what shipped, under the
name it shipped with. Only current-system docs move.

> **Open, owner's call:** the Settings section holding the blaze-speed options is
> currently labelled **"Navigation"** (`SettingsView.swift:149`) — the original plan
> called it BLAZE. Renaming it would make the feature name user-visible for the first
> time. That's UI copy, so it's deliberately *not* done here.

## Vendor identity — settled 2026-07-14, don't re-litigate

**The naming rule:** the full legal name where a field makes a *legal assertion*; the brand
name where it merely *identifies* us.

| Context | Value |
|---|---|
| `©` notices — `LegalCopyright`, `NSHumanReadableCopyright`, About, `LICENSE.md` | **FullSpec Systems Inc.** |
| Authorship / brand — Velopack `--packAuthors`, marketing | **FullSpec Systems** |
| Windows `CompanyName` | **FullSpec Systems Inc.** — it names the *vendor*; it said "PhotoBlaze", which was never right |

Word order is **year-first** (`© 2026 FullSpec Systems Inc.`), matching `LICENSE.md`.

### Known-open, deliberately deferred (all the same trigger: **first revenue**)

- **The IP assignment hasn't been executed.** `CLA.md` records it: JD Lien owns the
  copyright personally, so the shipped `©` names an entity that doesn't own it *yet*.
  Owner's call to defer — the product is two weeks old with no users, and the CLA's own
  trigger is "before first revenue". Deferring follows the plan rather than departing
  from it. Free today, a CRA valuation argument once it earns.
- **Signing identities don't match the vendor.** Azure Trusted Signing is FullSpec
  Systems (Windows); Apple notarization uses JD Lien's **personal** Developer ID (macOS).
  Cosmetic and normal mid-incorporation.

### The Apple question — answered, no action needed

**Changing the Apple Team ID later will NOT break Sparkle auto-update.** Verified against
the Sparkle 2.9.4 source (`Sparkle/SUUpdateValidator.m`), not docs:
- `validateDownloadPathWithFallbackOnCodeSigning:` returns `YES` on **EdDSA alone**; the
  Team-ID check below it is commented *"As fallback for key rotation"* and only runs when
  EdDSA has **failed**.
- The post-extraction check rejects only when EdDSA passed **and** the signature doesn't
  match the old one **and** the new bundle isn't validly signed *in its own right*. A
  changed Team ID (`!passedCodeSigning`) is tolerated so long as the new build is properly
  signed.

⇒ So long as the **EdDSA key is preserved** (never regenerate it) and the new build is
signed + notarized, the Team ID can move whenever. This makes it **unlike** the bundle id
and feed URL — those were "free now, expensive once you have users"; this one costs the
same whenever, so there is **no deadline to beat**.

Other Apple facts (knowledge cutoff Jan 2026 — confirm against Apple's docs before acting):
- Individual and Organization memberships are **both $99/yr** (~$129 CAD). No premium for
  the corp.
- **No self-serve conversion** Individual → Organization: enroll fresh, with a **D-U-N-S
  number** for FullSpec Systems Inc.
- An Apple ID holds only **one** membership as Account Holder → the Org needs a **new
  Apple ID** on the company domain (a role address like `developer@fullspec.ca` beats
  `jd@`; either beats a personal Apple ID). Then invite the personal Apple ID in as Admin.
- An Individual membership is bound to your **legal name** — Apple will not display
  "FullSpec Systems Inc." on it. The Org account is the only real path.

**Do the free part now, if anything — and it may matter more than expected.** The only real
lead time is the **D-U-N-S number**; it's free and needs no Apple enrolment or payment.

⚠ **There is an existing D-U-N-S `245285353`, but it is probably stale.** It was registered
to the *original* FullSpec Systems, and the owner has **re-incorporated** since. Apple
matches the D-U-N-S record on the **legal entity name**, not the address — so "it has all
my info" is not the test. If the re-incorporation created a new legal entity (dissolve +
form), the D&B record may describe an entity that no longer exists, and Apple rejects on
the name mismatch.

**The check is free and instant:** Apple's D-U-N-S lookup (`developer.apple.com/enroll/
duns-lookup/`) with the *current* legal name + address shows exactly what Apple will see.
- Returns `245285353` under the current entity → set, no lead time.
- Returns nothing / the old entity → D&B must update the record or issue a new one. **This
  is where weeks go** — which is precisely why it's worth discovering now rather than the
  week an iOS build needs to go into review.

**What's actually on the Individual account** (checked with the owner 2026-07-14):
- **Two sticker packs.** These are why "just let the Individual lapse" is wrong: **a lapsed
  membership removes your apps from sale.** The eventual move is *transfer, then lapse* —
  App Store Connect supports app transfers, and sticker packs use none of the things that
  block one (iCloud, Apple Pay), but confirm eligibility against Apple's "Transfer an app"
  doc before relying on it. One of the two is being removed by Apple anyway → not a
  transfer candidate.
- **Client iPad work — irrelevant to this decision.** Being a *member* of someone else's
  team is **free and needs no membership of your own**; the client just invites an Apple ID.
  Never a reason to keep the Individual account.

⇒ Long run this is still **one** membership, not two: transfer the keeper, let the
Individual lapse, done. The double-pay is only the overlap window, and only when *we*
choose to start it. Nothing here has a clock on it.

**TCC caveat:** changing the Team ID resets macOS TCC grants (the Local Network permission
for AI descriptions), because TCC keys on the signature's designated requirement. Phase C's
bundle-id change already resets TCC once, so deferring means taking that hit a second time
— one re-prompt, for one user. Not worth $129.

**Where it actually bites: iOS, not macOS.** The App Store shows the **seller name
publicly** on the product page; Developer ID on macOS surfaces the signer almost nowhere
(`spctl -a -vv`, Get Info — there is no SmartScreen-style "Verified publisher" banner).

> **The App Store/LGPL blocker is probably macOS-shaped, not App-Store-shaped.** CLAUDE.md
> says an App Store channel requires re-opening the LGPL question first — but that stems
> from **macOS bundling FFmpeg** (LGPL) for MKV/WebM/VP9. The mobile vision targets iOS
> **ImageIO / VideoToolbox / PhotoKit**, and macOS already proves the pattern (Apple Image
> I/O, *"no libheif exposure at all"*). An iOS build over PhotoKit assets
> (HEIC/JPEG/MOV/MP4) plausibly links **zero LGPL libraries**, so the conflict never
> arises. Confirm when iOS is actually scoped — but don't assume the macOS blocker
> transfers.

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
| macOS app | `PhotoBlaze.app` | `Blaze Viewer.app` (+ `CFBundleExecutable` `Blaze Viewer`) |
| macOS CLI symlink | `/usr/local/bin/photoblaze` | `/usr/local/bin/blaze` |
| Windows exe | `photoblaze.exe` | `blazeviewer.exe` |
| Linux binary + `.desktop` Exec | `photoblaze` | `blazeviewer` |
| Velopack | `--packId PhotoBlaze` | `--packId BlazeViewer` |
| ProgIDs | `PhotoBlaze.Image/.Archive/.Video` | `BlazeViewer.*` |
| Registry | `SOFTWARE\PhotoBlaze\Capabilities` | `SOFTWARE\BlazeViewer\Capabilities` |
| Config (Win) | `%APPDATA%\PhotoBlaze` | `%APPDATA%\BlazeViewer` |
| Config (mac) | `~/Library/Application Support/PhotoBlaze` | `…/BlazeViewer` |
| Config (Linux) | `~/.config/photoblaze` | `~/.config/blazeviewer` |
| Dispatch queue labels (4, Swift) | `ca.fullspec.photoblaze.*` | `ca.fullspec.blazeviewer.*` |
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

### ✅ VERIFIED 2026-07-14 (against the actual velopack 0.0.70 source, not docs)

An earlier draft of this plan said "if Velopack won't follow a redirect, fall back to its
native `GithubSource`." **That fallback does not exist** — `update.rs` already documented
it and the source confirms:

1. **The Rust binding has no `GithubSource`** — only `HttpSource` / `FileSource`
   (`velopack-0.0.70/src/sources.rs`). GitHub's per-release asset layout is something
   `HttpSource` cannot walk, and a flat dir is also what keeps **delta updates** working.
2. **`download_release_entry` does `url.join(&asset.FileName)`** — the `.nupkg` URL is
   *strictly* `FEED_URL + FileName`. `releases.win.json` carries a **filename, not a URL**,
   so Windows can never be pointed straight at GitHub. **The flat dir is mandatory** and
   the 302 is the only route to off-host bytes.
3. **The 302 works.** velopack builds its agent as
   `ureq::AgentBuilder::new().tls_connector(..).build()` and **never** calls `.redirects(0)`,
   so ureq's default `redirects: 5` applies (`ureq-2.12.1/src/agent.rs:262`).

**macOS and Linux don't need the redirect at all** — both carry absolute URLs:
Sparkle's `<enclosure url=…>` (`generate-mac-appcast.sh`, base already overridable via
`PB_APPCAST_BASE_URL`) and the Linux manifest's `Asset.url` (whose sibling `file` field is
literally commented *"informational; the URL is what we fetch"*).

| Platform | Feed metadata | Bytes | Redirect? |
|---|---|---|---|
| Windows | `releases.win.json` on the Worker | `.nupkg` → **302** → GitHub | **required** |
| macOS | `appcast.xml` on the Worker | enclosure → GitHub direct | no |
| Linux | `latest.json` on the Worker | `Asset.url` → GitHub direct | no |

⚠ **Worker gotcha:** velopack appends `?localVersion=<v>&id=<id>` to the
`releases.win.json` request. The Worker must ignore that query rather than 404 on it.

⚠ **The releases repo must be PUBLIC** — a private repo's release assets require auth.
That exposes artifacts + version history while the source stays closed. Confirm before
creating it.

### The sequencing that actually matters

**Decouple the URL from the bytes — they have completely different deadlines.**

- **Moving the feed URL is only free during a forced reinstall.** It's compiled into every
  binary, so the Phase C cutover is the *one* moment it can change without stranding an
  install. Miss it and you need *another* forced reinstall later.
- **Moving the bytes is free forever, afterwards.** Once the URL is
  `downloads.blazeviewer.app`, switching from DigitalOcean to GitHub (or anywhere) is a
  Worker config change that no installed app ever notices.

**Therefore:** point the feed at our own domain **in Phase C**, and treat GitHub hosting as
a *later, independent* decision. Right now the product is stealth with one user, so the
bandwidth bill is ~zero and GitHub buys nothing yet — but if the URL isn't moved during C,
the option costs a reinstall forever after. Do the cheap irreversible thing now; defer the
reversible one.

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
