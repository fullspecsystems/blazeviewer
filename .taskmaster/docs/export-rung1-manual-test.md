# Export, frame capture and HDR — manual test script (tasks #89, #81, #146)

_Current as of 2026-11-14 (code through commit `789a4947`). Work top to bottom; each step says what
to do and what you should see. Note anything that differs in the plan's `## Handoff`
(`.taskmaster/docs/export-plan.md`). Section and step numbers are stable — the plan and
tasks.json refer to them._

## What you're testing

**Export as PNG** — File ▸ Export as PNG… (`E`; `⇧⌘E`; or right-click). It exports **what's on
screen**: a photo, a photo inside an archive, a **video frame** (paused or playing — or the
poster, before you've played it), an **animation frame**, a **Live Photo's motion frame**. The
native save sheet opens, and a full-resolution PNG is written in the background:
- the RAM rotation is baked in;
- wide gamut is kept, with a sanitized profile; 16-bit stays 16-bit;
- HDR becomes Display P3 SDR (the confirmation says so);
- no metadata, and no OS recent-file entry.

**The save sheet starts beside the photo.** Export from the same folder again and it opens where
you put the last one; a different folder starts beside its own photo. Nothing about where you
export is kept after you quit.

**The confirmation is clickable (Mac):** "✓ Exported name.png 🔍" — click it to show the file in
Finder. File ▸ **Show Last Export in Finder** does the same any time.

**The frame on screen, everywhere:** Copy, Copy Text from Image, Describe and Ask also act on the
paused/playing video frame or animation frame now, not the poster.

**HDR PNGs** (PQ/HLG-tagged) now display in HDR.

**Not built yet** (don't test): choosing a format (JPEG etc.), resize, the Export panel (rung 2 —
its encoding engine exists but has no UI); HDR *output* files (rung 5); HDR JPEG XL display; a
clickable confirmation on Windows.

**Tip — key names only:** a few checks read app preferences. The commands print key *names*,
never folder paths.

## 0. Build and launch (Mac)

```sh
cd ~/code/blazeviewer
git pull
./scripts/build-swift-host.sh --run      # quits any running copy first, then launches
```

A prebuilt app is at `target/swift-host/release/Blaze Viewer.app` (About shows its build id).
Rebuild after pulling new code.

Have handy:
- a folder with an **iPhone HEIC** (Display P3), and an **sRGB AVIF or HEIC**
  (`~/Downloads/test-images` has both);
- an **HDR HEIC/AVIF**;
- a **PNG with transparency**, and a **16-bit PNG or TIFF** (or a 10-bit HEIC);
- a large **NEF/ARW**, an **SVG**, a **JPEG XL**;
- an **MP4/MOV** clip and an **MKV or WebM** — ideally one with on-screen text, plus an **HDR**
  clip (an iPhone HDR video or an HDR10 file);
- an **animated GIF or WebP**, and a **Live Photo** (HEIC + MOV pair);
- a **.zip** containing photos and a video;
- the HDR test cards in `crates/pb-decode/tests/fixtures/hdr/` (section 10).

## 1. Where it lives

| # | Do | Expect |
|---|---|---|
| 1.1 | Launch with nothing open; open the **File** menu | **Export as PNG…** is present, **disabled**, shows ⇧⌘E; **Show Last Export in Finder** is present and disabled |
| 1.2 | Open a folder of photos; File menu again | Export as PNG… is **enabled** |
| 1.3 | Navigate to a **video** | Export as PNG… **enabled**; `E` opens the sheet. Before you've played it, it exports the **poster**, named `<clip>.png` (playing/paused frames: section 11) |
| 1.4 | In a folder containing a `.zip`, land on the zip's door tile | Menu item **disabled**; `E` toasts "Open the archive to export its images" |
| 1.5 | Press `?` (help overlay) | A row "Export as PNG — E" |
| 1.6 | Settings ▸ Shortcuts | "Export as PNG" bound to `E`, rebindable; "Show last export in Finder" listed with no key |
| 1.7 | Right-click a photo | "**Export as PNG…**" after the AI items, in a group with Show in Finder and Quick Sort; choosing it opens the same sheet as `E` |
| 1.8 | Right-click a **video**, a **zip door**, and a photo **inside a zip** | Video: Export **is** there. Door: **no** Export item. Inside the zip: Export is there, Show in Finder and Quick Sort aren't (and that group still has its separator) |

## 2. Basic export

| # | Do | Expect |
|---|---|---|
| 2.1 | On a JPEG, press `E` | A **sheet** slides down from the viewer window: title "Export as PNG", button "Export", name `<photo name>.png`, PNG only. It starts in **the photo's own folder** (section 6) |
| 2.2 | Click Export | Toast "Exporting name.png…", then the **"Exported name.png" pill** (section 6b) within a second or two |
| 2.3 | Open the PNG in Preview; ⌘I (Inspector) | Same pixel dimensions as the original; looks identical to Blaze's view of the original |
| 2.4 | Press `E` again on the same photo, same folder | Suggested name is now `<name>-1.png` (the existing file isn't offered) |
| 2.5 | Press `E`, then **Cancel** (or `Esc` while the sheet is up) | Sheet closes; **no toast**, no file; the app does **not** quit (`Esc` belongs to the sheet) |
| 2.6 | Press ⇧⌘E | Same as `E` |

## 3. Colour fidelity

| # | Do | Expect |
|---|---|---|
| 3.1 | Export an **iPhone HEIC** | Preview Inspector ▸ ColorSync profile: **Display P3**. Side by side with the original in Preview, the colours are identical (no washed-out or oversaturated look) |
| 3.2 | Export an **sRGB AVIF/HEIC** | Profile **sRGB IEC61966-2.1** — *not* Display P3 |
| 3.3 | Export a **16-bit PNG/TIFF** or a 10-bit HEIC; then `sips -g bitsPerSample out.png` | `bitsPerSample: 16` |
| 3.4 | Export an **HDR HEIC/AVIF** | The pill reads "Exported name.png (HDR as SDR)". The PNG looks natural: bright highlights aren't blown out, the image isn't dim, colours aren't muted — and **bright reds stay red, oranges stay orange** (they used to drift to magenta / yellow) |
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

## 6. Where the save sheet starts

_From your feedback: opening on the last folder you were in felt wrong. Now: beside the photo; the
same folder again → where its last export went; nothing kept after quitting._

| # | Do | Expect |
|---|---|---|
| 6.1 | Open a folder, `E` on a photo | The sheet starts **in the photo's own folder** |
| 6.2 | Export it into a *different* folder (e.g. a new `~/Desktop/exports`), then `E` on **another photo in the same folder** | Starts in **`~/Desktop/exports`** — this folder's exports keep landing together |
| 6.3 | Open a **different folder**, `E` | Starts **beside that photo**, not in `~/Desktop/exports` |
| 6.4 | Quit, relaunch, `E` in the first folder again | Beside the photo — nothing was remembered across the quit |
| 6.5 | `grep -c last_export_folder ~/Library/Application\ Support/BlazeViewer/settings.toml` | `0` once settings have been saved since this build (an old key, if any, drops at the next save — change any setting to force it) |
| 6.6 | Settings ▸ "Open files in": **a pinned folder**. Open a folder you haven't exported from, `E` | Starts in the **pinned folder**; after exporting elsewhere, that folder's next export goes back there (6.2's rule wins) |
| 6.7 | Inside a **zip**: `E` | Beside the archive (its folder); a second export from the same zip goes where the first went |

## 6b. The "Exported" notice (Mac)

| # | Do | Expect |
|---|---|---|
| 6b.1 | Export a photo | A pill, bottom-centre: **✓ Exported name.png 🔍** |
| 6b.2 | Leave it alone | It stays **~5 s**, then fades |
| 6b.3 | Export again and **hover** over it | It stays as long as you hover (and glows); move off → ~5 s more |
| 6b.4 | **Click** it | Finder opens with the PNG selected; the pill goes away |
| 6b.5 | Export, then press Space (next photo) or any other key | The pill disappears at once |
| 6b.6 | File ▸ **Show Last Export in Finder** (before any export, then after) | Disabled until the first export; then reveals it in Finder, even after the pill is gone |
| 6b.7 | Export an **HDR** photo | The pill reads "Exported name.png (HDR as SDR)" |
| 6b.8 | VoiceOver on (⌘F5), move to the pill | Reads "Exported name.png. Show in Finder"; VO-Space reveals it |

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
| 8.1 | Export a big **NEF/ARW** (or a 40+ MP photo) and, as soon as you click Export, **hold Space** to blaze | Browsing stays smooth, and the export isn't cancelled by navigating. The pill won't linger while you browse (navigating dismisses it) — check the file arrived, or use File ▸ Show Last Export in Finder |
| 8.2 | Optional numbers: launch with `PB_PERF=1` from Terminal (`PB_PERF=1 "target/swift-host/release/Blaze Viewer.app/Contents/MacOS/Blaze Viewer"`) | A `[perf] export->file: queue … decode … encode … write …` line per export (durations only) |
| 8.3 | Start exporting a big file and immediately ⌘Q | Quits within ~4 s; afterwards the file is either complete (opens fine) or absent, and no `.tmp` is left |

## 9. Other sources

| # | Do | Expect |
|---|---|---|
| 9.1 | Export from **inside a .zip** (enter it with `P`) | The sheet starts beside the zip; the export works |
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
| 10.4 | `pq-card.png` and `hlg-card.png` (HDR PNGs) | Look **identical** to the AVIF/HEIC cards (they used to be flat and grey). Preview should agree |
| 10.5 | `pq-card.jxl` | **Known gap**: shows as SDR (JPEG XL has no HDR path yet) |
| 10.6 | Export `pq-card.heic` and `pq-card.png` as PNG (`E`) | Both pills read "(HDR as SDR)"; both PNGs step up evenly from black to white with no clipping, and match each other |

## 11. Video and animation frames (task #81)

Copy, Copy Text from Image, Describe/Ask and Export act on **the frame on screen**, not the video's
poster. Use a clip with recognizable scenes (and on-screen text for 11.5). Frame-step a paused
video or animation with `.` (forward) and `,` (back); the arrow keys seek.

| # | Do | Expect |
|---|---|---|
| 11.1 | Open an **MP4/MOV**, play it, pause on a distinctive frame, `⌘C`; paste into Preview (File ▸ New from Clipboard) | Toast "Copying frame…" then "Copied image". The paste is **that paused frame** at the video's full resolution (not the poster), upright for a portrait phone clip |
| 11.2 | Same with an **MKV or WebM** (the other playback route) | Same: the paused frame |
| 11.3 | Step one frame with `.` while paused (or seek with an arrow key), then `⌘C` again | The new frame, not the previous one |
| 11.4 | `⌘C` while the video is **playing** | A frame from that moment (within a frame or two); the app never stalls |
| 11.5 | Pause on a frame with **text** in it, then Edit ▸ **Copy Text from Image** (or the right-click menu — it has no default key) | Toast "Reading text…", then the text **of that frame** is copied. Pause on a different frame and do it again: the new frame's text, not the old one's |
| 11.6 | Open the **Text** tab (`T`) while a video plays | Nothing churns while it plays; pause → "Reading text…" → that frame's text |
| 11.7 | With a describe backend set up: pause on a frame, press `D` (or Ask with `⇧D`) | The description is of **the paused frame**. Pause elsewhere and Ask again: describes the new frame |
| 11.8 | Pause on a frame and press `E` | Sheet name `<clip>-0h01m23s.png` (the paused time); the PNG is that frame at full resolution |
| 11.9 | Export a paused frame of an **HDR** clip — ideally **the clip whose reds looked magenta** | The pill reads "(HDR as SDR)"; the PNG looks natural, not washed out or dim, and **its reds stay red** (side by side with Blaze's own view of that frame). `⌘C` of the same frame should match too |
| 11.10 | `E` on a video **before playing it** | Exports the **poster** (what's on screen), named `<clip>.png` |
| 11.11 | A video **inside a zip** (enter with `P`): pause and `⌘C`, then `E` | The real paused frame for both (Copy used to give a grey placeholder for zipped MP4/MOV) |
| 11.12 | Copy a video frame and paste into **Messages** or **Mail** | An **image** is pasted, not the video file |
| 11.13 | Play an **animated GIF/WebP** (`P`), pause it on a later frame (or step with `.`), `⌘C` and `E` | That frame; the export is named `<name>-frame12.png` (1-based) |
| 11.14 | A **Live Photo**: press `P` and `⌘C` while the motion is on screen | The motion frame on screen, at full motion resolution (not the still) — Live Photo capture isn't covered by automated tests, so this one matters |
| 11.15 | Copy a frame, then immediately navigate away | The copy still completes (that frame lands on the clipboard); nothing hangs |

---

## Windows (the other machine)

`pwsh scripts/build-windows.ps1 -Run`. **Everything here is behaviour-unverified** — it was
only compile-checked from the Mac.

| # | Do | Expect |
|---|---|---|
| W1 | File menu | "Export as PNG…   E" (disabled until a photo or video shows) and "Show Last Export in File Explorer" (disabled until the first export) |
| W2 | Press `E` on a photo | The Windows save dialog: title "Export as PNG", button "Export", PNG filter, starts in the photo's folder, name `<photo>.png` |
| W3 | Export | Toast "Exported name.png" (a plain toast on Windows — not clickable yet; W16); the PNG opens in Photos with correct colour |
| W4 | `Win+R` → `shell:recent` | **No** entry for the exported file (`FOS_DONTADDTORECENT`) |
| W5 | `regedit` → `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\ComDlg32\OpenSavePidlMRU\png` and `…\LastVisitedPidlMRU` | Report whether new entries appeared. If they did, that's an owner decision (plan § 3.6) |
| W6 | Overwrite prompt: export onto an existing PNG | Windows asks to replace; confirm → replaced |
| W7 | Browse while a big export runs | Smooth; the export thread runs at background priority |
| W8 | Quit during an export | Quits within ~4 s; no `.tmp` left |
| W9 | Pause a film mid-scene, Ctrl+C, paste into Paint | The **paused frame** (it used to be the poster). Toast "Copying frame…" then the copy icon |
| W10 | Right-click a photo, a video, a zip door | "Export as PNG…" on the photo and the video, not the door; it opens the save dialog |
| W11 | `cargo test -p pb-decode hdr_audit -- --ignored --nocapture` (#146 Windows half) | Every ratio ≈ 1.00x. If WIC reads ~2.5x off, that's the 80-vs-203-nit scRGB mismatch — record the table in the plan's Handoff |
| W12 | Pause a film, `E`, export | Name `<film>-0h12m34s.png`; the PNG is **the paused frame** — if it is consistently one frame off, note it (Media Foundation's timeline origin is assumed to be 0) |
| W13 | Same on an **HDR** film | Exports, but **SDR and capped at 4096 px** on Windows for now (MF's replay path); note how it looks |
| W14 | Pause, Copy Text from Image; then `D` | Both act on the paused frame |
| W15 | Play an animated GIF, pause on a later frame (or step with `.`), Ctrl+C | That frame |
| W16 | Export, then File ▸ **Show Last Export in File Explorer** | Explorer opens with the PNG selected |
| W17 | `E` twice from the same folder, exporting elsewhere the first time; then from another folder | Same folder → where the last export went; another folder → beside its photo |
