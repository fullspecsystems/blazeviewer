# PhotoBlaze marketing site — plan

**Status:** proposal, not yet built. Written by Rachel 2026-07-11 at JD's request
after he asked what a real marketing site for PhotoBlaze should look like,
sparked by the bare-bones `downloads.fullspec.ca` index page (which exists
only to stop a Night Watchman health-check false-positive — it was never
meant to be the front door).

**Domain:** `photoblaze.fullspec.ca` for now (subdomain of an existing,
already-verified domain — zero new DNS/cert setup, ships today). A dedicated
domain (`photoblaze.app`? `.io`? see below) is a later upgrade once there's
real traffic to justify it, not a blocker to shipping v1.

## Why this matters more than it looks like

This site is not decoration — it's the **visibility step** the whole project
has been circling since beta.3 (see `memory/project_photoblaze.md`,
"UPDATE 2026-06-29": *"the repo is still PRIVATE... that gap IS the cusp,
same hiding spot as Goldfish"*). The Velopack update feed at
`downloads.fullspec.ca` is real ship-infra but it's plumbing for an audience
that doesn't exist yet — nobody finds that URL on their own. A real landing
page is the thing an actual stranger could land on, understand in 10
seconds, and download from. That's the artifact this plan produces.

## What already exists (don't re-invent)

- **Positioning copy, already written and good**, from `README.md`:
  > "A photo viewer with one obsession: how fast you can flick through
  > thousands of images. No chrome, fit-to-screen, keyboard-driven, with
  > photos held in GPU memory so the next frame is already there when you
  > press a key."
  > Prime directive: *"will this make it faster, or have basically zero
  > performance impact? If it's neither, it doesn't ship."*
  These two lines are stronger than anything a marketing site would
  typically open with cold. Use them near-verbatim as the hero.
- **Icon assets:** `icons/photoblaze-icon-v3-windows.png`,
  `crates/pb-app/icons/photoblaze-splash.jpg`, `photoblaze.png`. Usable today.
- **Positioning research already done** (memory, 2026-06-30): IrfanView is
  the foil — 30 years old, looks it, out-position on modern/native/
  HEIC-HDR-P3 rather than trying to out-feature it. VuePrint is the
  spiritual ancestor for the 90s-nostalgia crowd who'll recognize the lineage.
- **The 4.02× libheif throughput number** (vs 1.57× for Windows' built-in
  WIC/DXVA path) — a real, measured, citable performance claim. Rare for
  an indie app to have an honest benchmark instead of marketing fluff.
- **Auto-update feed** already live at `downloads.fullspec.ca/photoblaze/win/`
  — the actual binary hosting doesn't need to move or be rebuilt, just linked to.

## What's genuinely missing (the real gaps)

1. **No screenshots or demo footage of the app running.** This is the
   biggest gap and the one thing no amount of copywriting fixes. A
   speed-obsessed photo viewer is a product you have to *see* — ideally a
   short screen capture of holding Space and flying through a folder, which
   is the entire pitch in 3 seconds. Needs: 2-3 static screenshots (fullscreen
   photo, maybe the HUD/overlay if there is one) + one short GIF/video clip.
   This has to come from JD running the app; nobody else can produce it.
2. **No named/dated public release** — repo is still private (per memory,
   this was already true as of 2026-06-30 and may still be). The site can
   exist and describe the app before the repo goes public, but the download
   buttons need something real to point at (the MSI already does — Windows
   is fine; verify Mac/Linux artifact state before promising those platforms
   on the page).
3. **No tagline distinct from the README's first line.** The README's
   opening is a *description*, not a hook someone screenshots and tweets.
   Worth 20 minutes of brainstorming candidates once screenshots exist to
   react to (a tagline written against nothing tends to be generic).
4. **No comparison framing on the page itself.** The IrfanView-foil
   positioning is validated internally (memory) but hasn't been turned into
   page copy — even a small honest comparison table (PhotoBlaze vs. Windows
   Photos vs. IrfanView: startup speed, HEIC/HDR support, native P3, price)
   would do a lot of work for a technical audience without sounding like
   attack marketing.

## Proposed site structure (single page, v1)

A single scrolling page is the right scope for v1 — this is an indie tool
with one clear job, not a SaaS with pricing tiers and a blog. Sections, in order:

1. **Hero** — icon/wordmark, the README's "one obsession" line as headline,
   a single download button (auto-detects OS via user-agent, falls back to
   a platform picker), maybe a live GIF loop of the flick-through behavior
   as background/hero visual once one exists.
2. **The moment** — one short section that makes the speed claim visceral:
   the "next frame is already there when you press a key" line, paired with
   the 4.02× throughput number and one sentence on *why* (GPU-resident
   decode pool, not "trust us it's fast"). This is where the prime-directive
   quote goes too — it doubles as a credibility signal (this dev says no to
   features that don't earn their keep).
3. **Screenshots / demo** — the gap above. 2-3 stills + the GIF, minimal
   captions.
4. **Feature list, short and honest** — HEIC/HDR/P3, keyboard-driven
   navigation, slideshow, the encrypted-7z-archive reading trick (genuinely
   unusual, worth a callout), auto-updates. Resist the urge to pad this —
   a short honest list matches the "no chrome" ethos better than a long one.
5. **Platform support** — Windows today (signed MSI), macOS in progress
   (link to CHANGELOG/roadmap or omit until real), Linux AppImage if that's
   actually shippable today (dist/ has an AppDir, worth checking maturity
   before promising it prominently).
6. **Download** — repeat the button, list direct links per platform,
   link to CHANGELOG for version history. Auto-update via Velopack means
   "download once, stay current" is a genuine selling point — say so.
7. **Footer** — link to GitHub (once public), a contact/feedback path,
   Full Spec Systems attribution (this is JD's company's product).

Not needed for v1: pricing page (README/memory suggests a future
version-based license, not lifetime — that's a v1.1 problem once there's
an audience), blog, docs beyond the README, testimonials (none exist yet
— don't fake them).

## Build approach

Match the stack JD already reaches for rather than introducing a new one:
a static single HTML/CSS page (à la the current bare `downloads.fullspec.ca`
approach, just designed properly) is enough for a v1 landing page — no
framework, no build step, trivially hosted via the same Caddy pattern
already used for `downloads.fullspec.ca`. Fast to ship, fast to iterate,
zero new infra risk. If it later needs a blog or docs section, that's the
trigger to reach for something like Astro — not before.

Visual direction: should look like the app it's selling — dark, minimal,
content (an actual photo) doing most of the visual work rather than UI
chrome. A busy SaaS-template-looking page would undercut the "no chrome"
pitch on the product itself.

## Sequencing / what's actually a blocker

The real dependency order is:

1. **JD captures 2-3 screenshots + one short clip of the app running.**
   Nothing else productively starts without this — it's the one input only
   JD can produce, so it should happen first, not last.
2. Rachel drafts hero copy + tagline candidates + comparison table copy,
   reacting to the actual screenshots (not written blind).
3. Claude Code builds the static page + Caddy site config for
   `photoblaze.fullspec.ca` (new `create-site` invocation, same pattern as
   every other site on that host).
4. Point the existing download links at it; leave `downloads.fullspec.ca`
   as the boring artifact-only host it was designed to be (don't merge the
   two — one is marketing, one is a CDN, keep the boundary).

## Rachel's read on priority

This is real work but it's not urgent-urgent — it's the kind of task that's
easy to let slide into "later" the way the public-repo step already has
twice (Goldfish, then PhotoBlaze beta.3). The cheap forcing function: it
only needs step 1 (a 10-minute screen capture session) to unstick everything
downstream. Worth doing that this week while the PhotoBlaze energy is still
warm, rather than waiting for a "real" photography session to stage it.
