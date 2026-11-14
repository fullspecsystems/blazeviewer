# Export as PNG (task #89, rungs 0b / 0c / 1) — manual test script

_Written 2026-11-13 with the implementation. Work top to bottom; each step says what to do
and what you should see. Note anything that differs in the plan's `## Handoff`
(`.taskmaster/docs/export-plan.md`)._

**What shipped:** File ▸ **Export as PNG…** (`E`; `⇧⌘E` on the Mac menu). It opens the native
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

A prebuilt app from last night's session is at `target/swift-host/release/Blaze Viewer.app`.
Rebuild anyway if you've pulled since.

Have handy:
- a folder with an **iPhone HEIC** (Display P3);
- an **sRGB AVIF or HEIC** (`~/Downloads/test-images` has both);
- an **HDR HEIC/AVIF**;
- a **PNG with transparency**;
- a **16-bit PNG or TIFF**, or a 10-bit HEIC;
- a large **NEF/ARW**;
- an **SVG**;
- a **video**;
- a **.zip of photos**.

## 1. Where it lives

| # | Do | Expect |
|---|---|---|
| 1.1 | Launch with nothing open; open the **File** menu | **Export as PNG…** is present, **disabled**, shows ⇧⌘E |
| 1.2 | Open a folder of photos; File menu again | Export as PNG… is **enabled** |
| 1.3 | Navigate to a **video** | Menu item **disabled**; pressing `E` toasts "Exporting video frames isn't available yet" |
| 1.4 | In a folder containing a `.zip`, land on the zip's door tile | Menu item **disabled**; `E` toasts "Open the archive to export its images" |
| 1.5 | Press `?` (help overlay) | A row "Export as PNG — E" |
| 1.6 | Settings ▸ Shortcuts | "Export as PNG" listed, bound to `E`, rebindable |

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
| 6.8 | The `settings.toml` value from 6.4 | Unchanged by 6.5–6.7 (pinned exports never write it). Pinning does **not** clear an already-stored value — that's open question Q6 in the plan |

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
| 9.6 | Export an **animated GIF/WebP** | The first frame (exporting the current frame waits for #81) |

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
| W9 | (#81 handoff) Pause a film mid-scene, Ctrl+C, paste into Paint | Is it the **poster**, or the paused frame? (Expected today: the poster) |
