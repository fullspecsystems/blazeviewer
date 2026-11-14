# Export as PNG (task #89, rungs 0b / 0c / 1) — manual test script

_Written 2026-11-13 with the implementation; session 2 (2026-11-14) added 1.7–1.8, 6.8–6.9 and
section 10 (HDR, including HDR PNGs). Work top to bottom; each step says what to do and what you should see. Note anything
that differs in the plan's `## Handoff` (`.taskmaster/docs/export-plan.md`)._

**What shipped:** File ▸ **Export as PNG…** (`E`; `⇧⌘E` on the Mac menu; right-click the photo).
It opens the native
save dialog, then writes a full-resolution PNG off the event loop:
- the RAM rotation is baked in;
- wide gamut is kept, with a sanitized profile;
- 16-bit stays 16-bit;
- HDR becomes P3 SDR;
- no metadata;
- no OS recent-file entry.

There's no Export panel yet (rung 2). Video frames aren't exportable yet (#81).

**Tip — key names only:** a few checks read app preferences. The commands print key *names*,
never folder paths.

## 0. Build and launch (Mac)

```sh
cd ~/code/blazeviewer
git pull
./scripts/build-swift-host.sh --run      # quits any running copy first, then launches
```

A prebuilt app is at `target/swift-host/release/Blaze Viewer.app` (session 2, built from the
commit that added this line — About shows its build id). Rebuild if you've pulled since.

**What changed in session 3** (2026-11-14): **video and animation frames** — Copy, Copy Text,
Describe/Ask and Export act on the frame on screen (section 11; W9, W12–W15). Rows 1.3, 1.8 and 9.6
changed with it.

**What changed in session 2** (2026-11-14), so you know where to look:
- the right-click **Export as PNG…** item (1.7–1.8);
- pinning "Open files in" now forgets a remembered export folder (6.8–6.9);
- **HDR PNGs** display in HDR instead of flat grey (10.4, 10.6);
- rung 2's encoding core (JPEG, resize) landed with **no UI** — nothing to click yet.

Have handy:
- a folder with an **iPhone HEIC** (Display P3);
- an **sRGB AVIF or HEIC** (`~/Downloads/test-images` has both);
- an **HDR HEIC/AVIF**;
- a **PNG with transparency**;
- a **16-bit PNG or TIFF**, or a 10-bit HEIC;
- a large **NEF/ARW**;
- an **SVG**;
- a **video**;
- a **.zip of photos**;
- the HDR test cards in `crates/pb-decode/tests/fixtures/hdr/` (section 10).

## 1. Where it lives

| # | Do | Expect |
|---|---|---|
| 1.1 | Launch with nothing open; open the **File** menu | **Export as PNG…** is present, **disabled**, shows ⇧⌘E |
| 1.2 | Open a folder of photos; File menu again | Export as PNG… is **enabled** |
| 1.3 | Navigate to a **video** (session 3: video frames export now) | Menu item **enabled**; `E` opens the sheet named `<clip>.png` (the poster, before playing) — section 11 covers playing/paused frames |
| 1.4 | In a folder containing a `.zip`, land on the zip's door tile | Menu item **disabled**; `E` toasts "Open the archive to export its images" |
| 1.5 | Press `?` (help overlay) | A row "Export as PNG — E" |
| 1.6 | Settings ▸ Shortcuts | "Export as PNG" listed, bound to `E`, rebindable |
| 1.7 | Right-click a photo | "**Export as PNG…**" after the AI items, in a group with Show in Finder and Quick Sort; choosing it opens the same sheet as `E` |
| 1.8 | Right-click a **video**, a **zip door**, and a photo **inside a zip** | Video: Export **is** there (session 3). Door: **no** Export item. Inside the zip: Export is there, Show in Finder and Quick Sort aren't (and that group still has its separator) |

## 2. Basic export

| # | Do | Expect |
|---|---|---|
| 2.1 | On a JPEG, press `E` | A **sheet** slides down from the viewer window: title "Export as PNG", button "Export", name `<photo name>.png`, PNG only. First time: it starts in **the photo's own folder** |
| 2.2 | Click Export | Toast "Exporting name.png…", then "Exported name.png" within a second or two |
| 2.3 | Open the PNG in Preview; ⌘I (Inspector) | Same pixel dimensions as the original; looks identical to Blaze's view of the original |
| 2.4 | Press `E` again on the same photo, same folder | Suggested name is now `<name>-1.png` (the existing file isn't offered) |
| 2.5 | Press `E`, then **Cancel** (or `Esc` while the sheet is up) | Sheet closes; **no toast**, no file; the app does **not** quit (`Esc` belongs to the sheet) |
| 2.6 | Press ⇧⌘E | Same as `E` |

## 3. Colour fidelity

| # | Do | Expect |
|---|---|---|
| 3.1 | Export an **iPhone HEIC** | Preview Inspector ▸ ColorSync profile: **Display P3**. Side by side with the original in Preview, the colours are identical (no washed-out or oversaturated look) |
| 3.2 | Export an **sRGB AVIF/HEIC** | Profile **sRGB IEC61966-2.1** — *not* Display P3 (this was review F5's bug) |
| 3.3 | Export a **16-bit PNG/TIFF** or a 10-bit HEIC; then `sips -g bitsPerSample out.png` | `bitsPerSample: 16` |
| 3.4 | Export an **HDR HEIC/AVIF** | Toast "Exported name.png (HDR as SDR)". The PNG looks natural: bright highlights aren't blown out, the image isn't dim, colours aren't muted |
| 3.5 | Export a **PNG with transparency** | Transparency preserved (Preview shows the checkerboard) |
| 3.6 | Export an **opaque** photo; `sips -g hasAlpha out.png` | `hasAlpha: no` (no wasted alpha channel) |

## 4. Rotation and the original

| # | Do | Expect |
|---|---|---|
| 4.1 | Press `R` (rotate) on a photo, then `E`, Export | The PNG is rotated as shown on screen |
| 4.2 | Check the original in Finder | **Unchanged**: same modified date, still unrotated (export never touches it) |

## 5. Metadata and privacy

| # | Do | Expect |
|---|---|---|
| 5.1 | Export a photo **with GPS** (most iPhone photos). Check with `exiftool out.png` (or `mdls out.png \| grep -i -E 'gps\|latitude\|model'`) | **No** GPS/location, no camera make/model, no dates. Only the colour profile |
| 5.2 | `exiftool out.png \| grep -i 'profile description'` | A standard name ("Display P3" / "sRGB IEC61966-2.1") or "RGB profile" — never a device or person name |
| 5.3 | After any export: `defaults read ca.fullspec.BlazeViewer 2>/dev/null \| grep -o -E 'NSOSPLastRootDirectory\|NSNavLastRootDirectory\|NSNavRecentPlaces'` | **No output** (the save panel's folder memory was removed). Note: the **Open** panel still writes it until task #147 — so test right after an export |
| 5.4 | Check **Recent Items** (Apple menu ▸ Recent Items, and Finder ▸ Go ▸ Recent Folders) | The exported file does **not** appear (NSSavePanel shouldn't add it — confirm) |

## 6. Remembered folder ("Open files in")

| # | Do | Expect |
|---|---|---|
| 6.1 | Settings ▸ General ▸ "Open files in": **the current image's folder** (default). Export into a *different* folder (e.g. a new `~/Desktop/exports`) | Works |
| 6.2 | Export another photo | The sheet **starts in `~/Desktop/exports`** |
| 6.3 | Quit (⌘Q), relaunch, open photos, export | Still starts in `~/Desktop/exports` (remembered across launches) |
| 6.4 | `grep -c last_export_folder ~/Library/Application\ Support/BlazeViewer/settings.toml` | `1` |
| 6.5 | Settings ▸ "Open files in": **a pinned folder** → choose one. Export | The sheet starts in the **pinned folder** |
| 6.6 | Export to a third folder, then export again | Starts in that third folder (remembered **for this session only**) |
| 6.7 | Quit, relaunch, export | Starts in the **pinned folder** again — nothing was remembered |
| 6.8 | Re-run the `grep -c` from 6.4 | `0` — **pinning in 6.5 forgot the remembered folder** (plan Q6, implemented in session 2 on your "pinned means don't remember" reasoning). Tell me if you'd rather it kept it |
| 6.9 | Settings ▸ "Open files in": back to **the current image's folder**. Export | Starts in the **photo's own folder** (nothing remembered); after this export, `grep -c` shows `1` again |

## 7. Safety

| # | Do | Expect |
|---|---|---|
| 7.1 | Export a **PNG photo** into its own folder **with its own name**; confirm "Replace" | Toast "Export failed — that's the original file — choose a different name". The original is intact |
| 7.2 | Export over a *different* existing PNG; confirm Replace | It's replaced |
| 7.3 | Look in the export folder after several exports | No `*.blaze-export-*.tmp` files |
| 7.4 | Export to a folder on an **exFAT USB stick** or an **SMB share** | Works (the no-hard-link fallback). Note the timing if slow |
| 7.5 | Export to **Desktop**/**Documents** after `tccutil reset SystemPolicyDesktopFolder ca.fullspec.BlazeViewer` | Works, or macOS asks permission once. Report any failure — the direct-write fallback is untested on a real TCC refusal |

## 8. Never slows browsing

| # | Do | Expect |
|---|---|---|
| 8.1 | Export a big **NEF/ARW** (or a 40+ MP photo) and, as soon as you click Export, **hold Space** to blaze | Browsing stays smooth. The "Exported…" toast arrives a second or two later. The export isn't cancelled by navigating |
| 8.2 | Optional numbers: launch with `PB_PERF=1` from Terminal (`PB_PERF=1 "target/swift-host/release/Blaze Viewer.app/Contents/MacOS/Blaze Viewer"`) | A `[perf] export->file: queue … decode … encode … write …` line per export (durations only) |
| 8.3 | Start exporting a big file and immediately ⌘Q | Quits within ~4 s; afterwards the file is either complete (opens fine) or absent, and no `.tmp` is left |

## 9. Other sources

| # | Do | Expect |
|---|---|---|
| 9.1 | Export from **inside a .zip** (enter it with `P`) | The sheet starts in the **zip's folder**; the export works |
| 9.2 | Export an **SVG** | PNG at the SVG's own size |
| 9.3 | Export a **RAW** with a full-size preview (most Nikon/Sony) | Full-size PNG of the camera's rendering |
| 9.4 | Export a **JPEG XL** | Works; colours match |
| 9.5 | Export a **CMYK JPEG** (if you have one) | Toast "Export failed — CMYK JPEGs can't be exported yet" |
| 9.6 | Export an **animated GIF/WebP** without playing it | The first frame, named `<name>.png` (playing/paused frames: section 11) |

## 10. HDR brightness (task #146 — the visual half)

The numbers are already measured: on the Mac every HDR decoder agrees that 203 nits = SDR white
(within 3 %). This is the eyeball check. The cards live in
`crates/pb-decode/tests/fixtures/hdr/`: six flat grey patches, left to right **0 · 100 · 203 · 400 ·
1000 · 4000 nits** (PQ) or **0 · 50 · 100 · 203 · 400 · 1000** (HLG).

| # | Do | Expect |
|---|---|---|
| 10.1 | Open the `hdr` fixture folder in Blaze; step through `pq-card.avif`, `pq-card.heic`, `pq-card.mp4` | All three look **identical** — same brightness for each patch. On an HDR (XDR) screen the right-hand patches glow brighter than white UI; on an SDR screen they compress toward white but still step up |
| 10.2 | Same for `hlg-card.avif`, `.heic`, `.mp4` | Identical to each other |
| 10.3 | Compare the **203-nit** patch (3rd PQ, 4th HLG) with a white Finder window side by side | Roughly the same white — that patch *is* SDR reference white |
| 10.4 | `pq-card.png` and `hlg-card.png` (**new in session 2** — HDR PNGs) | Look **identical** to the AVIF/HEIC cards (they used to be flat and grey). Preview should agree |
| 10.5 | `pq-card.jxl` | **Known gap**: shows as SDR (JPEG XL has no HDR path yet) |
| 10.6 | Export `pq-card.heic` and `pq-card.png` as PNG (`E`) | Both toast "(HDR as SDR)"; both PNGs step up evenly from black to white with no clipping, and match each other |

## 11. Video and animation frames (task #81 — session 3)

Copy, Copy Text from Image, Describe/Ask and Export now act on **the frame on screen**, not the
video's poster. Use a film or phone clip with recognizable scenes (and on-screen text for 11.5).

| # | Do | Expect |
|---|---|---|
| 11.1 | Open an **MP4/MOV**, play it, pause on a distinctive frame, `⌘C`; paste into Preview (File ▸ New from Clipboard) | Toast "Copying frame…" then "Copied image". The paste is **that paused frame** at the video's full resolution (not the poster), upright for a portrait phone clip |
| 11.2 | Same with an **MKV or WebM** (the other playback route) | Same: the paused frame |
| 11.3 | Step a frame with the arrow key while paused (or scrub), then `⌘C` again | The new frame, not the previous one |
| 11.4 | `⌘C` while the video is **playing** | A frame from that moment (within a frame or two); the app never stalls |
| 11.5 | Pause on a frame with **text** in it, then Edit ▸ **Copy Text from Image** (or the right-click menu — it has no default key) | Toast "Reading text…", then the text **of that frame** is copied. Pause on a different frame and do it again: the new frame's text, not the old one's |
| 11.6 | Open the **Text** tab (`T`) while a video plays | Nothing churns while it plays; pause → "Reading text…" → that frame's text |
| 11.7 | With a describe backend set up: pause on a frame, press `D` (or Ask with `⇧D`) | The description is of **the paused frame**. Pause elsewhere and Ask again: describes the new frame |
| 11.8 | Pause on a frame and press `E` | Sheet name `<clip>-0h01m23s.png` (the paused time); the PNG is that frame at full resolution |
| 11.9 | Export a paused frame of an **HDR** clip (iPhone HDR / HLG or an HDR10 file) | Toast "(HDR as SDR)"; the PNG looks natural, not washed out or dim |
| 11.10 | `E` on a video **before playing it** | Exports the **poster** (what's on screen), named `<clip>.png` |
| 11.11 | A video **inside a zip** (enter with `P`): pause and `⌘C`, then `E` | The real paused frame for both (Copy used to give a grey placeholder for zipped MP4/MOV) |
| 11.12 | Copy a video and paste into **Messages** or **Mail** | An **image** is pasted, not the video file |
| 11.13 | Play an **animated GIF/WebP** (`P`), pause it on a later frame (or step with the arrows), `⌘C` and `E` | That frame; the export is named `<name>-frame12.png` (1-based) |
| 11.14 | A **Live Photo**: press `P` and `⌘C` while the motion is on screen | The motion frame on screen, at full motion resolution (not the still) — Live Photo capture isn't covered by automated tests, so this one matters |
| 11.15 | Copy a frame, then immediately navigate away | The copy still completes (that frame lands on the clipboard); nothing hangs |

---

## Windows (the other machine)

`pwsh scripts/build-windows.ps1 -Run`. **Everything here is behaviour-unverified** — it was
only compile-checked from the Mac.

| # | Do | Expect |
|---|---|---|
| W1 | File menu | "Export as PNG…   E", disabled until a photo shows |
| W2 | Press `E` on a photo | The Windows save dialog: title "Export as PNG", button "Export", PNG filter, starts in the photo's folder, name `<photo>.png` |
| W3 | Export | Toasts as on Mac; the PNG opens in Photos with correct colour |
| W4 | `Win+R` → `shell:recent` | **No** entry for the exported file (`FOS_DONTADDTORECENT`) |
| W5 | `regedit` → `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\ComDlg32\OpenSavePidlMRU\png` and `…\LastVisitedPidlMRU` | Report whether new entries appeared. If they did, that's an owner decision (plan § 3.6) |
| W6 | Overwrite prompt: export onto an existing PNG | Windows asks to replace; confirm → replaced |
| W7 | Browse while a big export runs | Smooth; the export thread runs at background priority |
| W8 | Quit during an export | Quits within ~4 s; no `.tmp` left |
| W9 | Pause a film mid-scene, Ctrl+C, paste into Paint | The **paused frame** (session 3 — it used to be the poster). Toast "Copying frame…" then the copy icon |
| W10 | Right-click a photo, a video, a zip door | "Export as PNG…" on the photo **and the video** (session 3), not the door; it opens the save dialog |
| W11 | `cargo test -p pb-decode hdr_audit -- --ignored --nocapture` (#146 Windows half) | Every ratio ≈ 1.00x. If WIC reads ~2.5x off, that's the 80-vs-203-nit scRGB mismatch — record the table in the plan's Handoff |
| W12 | Pause a film, `E`, export | Name `<film>-0h12m34s.png`; the PNG is **the paused frame** — if it is consistently one frame off, note it (Media Foundation's timeline origin is assumed to be 0) |
| W13 | Same on an **HDR** film | Exports, but **SDR and capped at 4096 px** on Windows for now (MF's replay path); note how it looks |
| W14 | Pause, Copy Text from Image; then `D` | Both act on the paused frame |
| W15 | Play an animated GIF, pause on a later frame, Ctrl+C | That frame |
