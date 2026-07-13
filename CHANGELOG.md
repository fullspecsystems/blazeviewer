# Changelog

All notable changes to PhotoBlaze are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions match the Git
tags / GitHub releases; the installer version is the numeric core (e.g. `0.1.0`),
with any pre-release suffix carried only by the tag.

## [Unreleased]

## [0.2.0] - 2026-07-13

### Added
- **Undo a delete with `Ctrl+Z`.** Sent a photo to the Recycle Bin by mistake? Press `Ctrl+Z`
  (or Edit ▸ Undo Delete) to restore it — the photo comes back in place and PhotoBlaze jumps to
  it with a "Restored <name>" message. Works even if it was the last photo in the folder.
  (Windows and Linux; a permanent `Shift+Del` can't be undone.)
- **A toolbar for mouse control (Windows/Linux).** A row of buttons now sits under the menu
  in windowed mode: previous/next and random (hold to fly, just like the keys), previous/next
  folder, play/pause, slideshow, rotate left/right, delete, and toggles for the info line,
  all-info panel, and folder tree — with the photo counter and a fullscreen button on the
  right. Buttons light up on hover, and a toggle (or the play button while something is
  playing) fills with your accent color. It is on by default; turn it off from **View ▸ Show
  Toolbar** or under Settings ▸ Appearance ▸ **Show toolbar** to give the photo the whole
  window. The keyboard still does everything without it.
- **PhotoBlaze now matches your Windows accent color.** Buttons, tabs, selection, and other
  highlights follow your system accent (the color you pick in Windows Settings) out of the box,
  so the app looks at home on your desktop, and update the moment you change your Windows accent
  (no restart needed). You can change this in Settings under Appearance ▸
  Accent color: **System** (follow Windows), **Custom** (pick any color), or **Blaze Orange**
  (the PhotoBlaze color). Any color you pick is honored — if it would be too faint to see it is
  nudged just enough to stay visible against the panels, and button text automatically switches
  between light and dark to stay readable. (Linux and macOS keep Blaze Orange for now.)
- **A Thumbnails panel: press `Shift+T` for a scrollable strip of your photos.**
  The left pane now has two tabs — Folders and Thumbnails — sharing one panel (`Shift+F`
  and `Shift+T` switch between them; clicking the tabs works too). The strip shows small
  previews of the photos around where you are, with the current one highlighted, each
  cell labeled with its filename, and badges for videos, Live Photos, and animations.
  **Click any thumbnail to jump straight to it** — if you jump somewhere far away, the
  thumbnail itself appears instantly while the full image loads, so there is never a
  black flash or a wait. The strip follows along as you navigate (scrolling it by hand
  pauses the following; your next keypress or click brings it back), rotations you make
  show in the strip, and the panel is resizable — thumbnails stay sharp when you widen
  it. Thumbnails are generated from decode work the viewer is already doing, so browsing
  speed is unaffected, and everything stays in memory only: no thumbnail files or
  databases are ever written to disk. Available on all platforms now (Windows, Linux, and
  macOS); on Windows and Linux the left pane and the info panel resize by dragging their
  edge, and the tabs shrink to fit as you narrow them.
- **Videos now appear in your library, with real poster frames.** Camera clips (MP4, MOV,
  MKV, WebM, AVI, and other common containers) are listed alongside photos when you browse
  a folder. Each clip shows its first non-black frame as a poster (with the correct
  rotation and colors), a play badge, and its details in the info panel (duration, codec,
  frame rate, audio). **Press `P` to play, with sound** — pause/resume on `P`, replay
  after the clip ends, and the existing mute toggle applies to video too (muting keeps
  perfect sync; unmuting picks up mid-clip). **Seek with the arrow keys while a video
  plays**: `←`/`→` jump 2 seconds, `Shift+←`/`→` jump 10, and holding scrubs (a keymap
  saved by an earlier version is healed so the Shift chords work there too). While a
  video plays, the info line (`i`) grows a playback row — elapsed time, a progress bar,
  and the total — so position is always visible when the line is on; when it is off,
  seeking flashes the line briefly as the position readout. When you are zoomed in far
  enough that the picture pans horizontally, the arrows keep panning — seeking steps
  aside. A seek while paused updates the frame and stays paused. Playback streams the
  file with constant memory, so a clip of any length plays without loading it into RAM.
  A clip whose audio cannot start still plays, silently. A clip
  Windows has no codec for shows a plain dark tile instead of an error. Live Photo motion
  files are not listed twice: an `IMG_1234.MOV` next to `IMG_1234.HEIC` stays hidden and
  keeps playing through its photo with `P`. Viewing videos leaves no trace on disk, same
  as photos.
- **Videos inside ZIP and 7z archives play too.** An archive's videos are listed alongside
  its photos — an archive of nothing but videos now opens instead of being refused — with
  the same posters, playback, sound, and seeking as loose files. Everything stays in
  memory: the clip is never extracted to disk, so viewing an archive still leaves no trace.
  Entries over 1 GiB are skipped (extract those to play them). Works on Windows, macOS, and
  Linux.
- **The video playback bar is interactive.** The progress bar in the info line has a round
  position knob: click anywhere on the bar to jump there, or drag the knob to scrub. A
  play/pause button sits at the left of the row (pause while playing, play while paused)
  and works like the `P` key, and the knob now glides smoothly during playback. Moving the
  mouse over the bottom of the window while a video is active reveals the controls even
  when the info line is off, like other video players; they fade away on their own.
- **Frame stepping works on videos.** `,` and `.` step one frame back/forward (hold to
  scrub), pausing playback first — the same behavior animations already had. Stepping
  forward is instant; stepping backward re-decodes from the nearest keyframe, so it can
  take a moment on long-GOP clips.
- **Video playback comes to macOS, with hardware decoding.** Camera clips play through the
  system's native player (AVPlayer / VideoToolbox), so virtually every codec plays with
  hardware acceleration — poster frames, `P` to play with sound, arrow-key seek, frame-step,
  mute, zoom / pan / rotation, the interactive playback bar, and archive (ZIP/7z) videos all
  match the rest of the app. Anything the system player can't open falls back to a bundled
  FFmpeg decoder, so the codec coverage is the same everywhere.
- **Video playback now works on Linux (beta).** Camera clips play with sound, posters, and
  seeking through the system FFmpeg libraries, the same as on Windows and macOS. It is newer
  and less tested than the other platforms, so a few clips may not play yet — please report
  any that don't.
- **PhotoBlaze now takes command-line options.** Run `photoblaze --help` to see them all. You can
  point it at files, a folder, or a `.zip` / `.7z`, and shape a single launch without changing your
  saved settings: `--slideshow[=SECS]` (optionally with a per-slide time — `5`, `3s`, or
  `0.5m`), `--shuffle` and
  `--reverse` (together they play a reverse shuffle), `--scale fit|fill|original`,
  `--theme light|dark|system`, `--windowed` / `--fullscreen`, `--recursive` / `--no-recursive`,
  `--info` / `--no-info`, `--details` and `--folders` (open those panels), `--mute`, and
  `--start-at N|NAME` to begin at a given photo. `--help` and `--version` also work. Every option
  applies only to that launch and never writes to your settings. On Windows the help and version
  text now print into the terminal you launched from.
- **The command line works on macOS too.** Install the `photoblaze` command once from
  **PhotoBlaze ▸ Install Command-Line Tool…**, then every option above works from the Terminal —
  including bare paths (`photoblaze ~/Photos`), colored `--help` on a TTY (plain when piped), and
  `--version` printing the same version the About panel shows. Launching from Finder or the Dock is
  unchanged; a bad option on a GUI launch shows a dialog instead of failing silently.
- **The About dialog now shows the build's CPU architecture** next to the build id (e.g.
  `Build 1ad1043 · ARM64`), so you can confirm at a glance whether you're running the native
  Windows ARM64 build or the x64 one.
- **Linux now updates itself.** The AppImage checks for a new version in the background and, when
  one is available, downloads and verifies it, then swaps itself in the next time you quit — the
  same hands-off updating Windows and macOS already have, so you no longer re-download the AppImage
  by hand. Update checks send only your version and CPU architecture, never anything about the
  photos you view. (If the AppImage is installed somewhere you can't write to, it's left untouched.)
- **Animated AVIF now plays on Windows.** Files with an AVIF image sequence (the `avis` format)
  show the play hint and loop like a GIF, with correct frame timing and wide-gamut color.
  HDR animated AVIFs deliberately stay on the still path, where they render in full HDR
  quality rather than a washed-out clamped animation. Animated HEIC stays first-frame-static
  on Windows (it uses HEVC, which this decoder does not handle).
- **Animated AVIF now plays on Linux.** Multi-frame `.avif` files (and HEIC image sequences) show
  the first frame as usual and play the full animation on `P`, looping like a GIF — previously they
  were stuck on the first frame. (Wide-gamut/HDR sequences play but their colors read a touch
  oversaturated for now; full per-sequence color management is a follow-up.)
- **Photoshop files without a flattened preview now show something instead of nothing.** A `.psd`
  saved *without* "Maximize Compatibility" has no merged image inside — only the layer stack — so
  it used to open to a blank error. PhotoBlaze now falls back to the small preview thumbnail
  Photoshop embeds in every file (the image Finder shows): low resolution, but you can see the
  photo. On macOS, less-common PSDs (CMYK and others, when a flattened image *is* present) render
  at full resolution via the system decoder.

### Changed
- **`Del` no longer silently loses a photo on drives that skip the Recycle Bin.** If a drive is
  set to permanently delete instead of recycling (Recycle Bin Properties ▸ "Don't move files to
  the Recycle Bin"), pressing `Del` now asks you to confirm first — the same prompt as
  `Shift+Del` — instead of quietly deleting with no warning and no way to undo. When a file
  really is gone for good, the on-screen icon now shows a permanent delete rather than a
  recycle. (Windows.)
- **Settings now save as you change them.** The Settings dialog on Windows and Linux
  applies each change live and keeps it, the same way it already worked on macOS. There is
  no more Save or Cancel button. Adjust a slider, toggle, or color and you see it take
  effect on your photos right away, then click Done to close. "Reset settings" still puts
  everything back to defaults.
- **Renamed "Show All EXIF Info" to "Show Detailed Info"** in the View menu, matching the
  Details panel it opens (and consistent across Windows, Linux, and macOS).
- **`F` is now shown everywhere as the full screen shortcut.** The View menu (and the toolbar's
  exit hint) advertise `F` — the memorable, cross-platform key — instead of `F11`, which is still
  bound as a secondary. If an older keymap had dropped `F`, it's restored automatically on
  launch (F11 and Alt+Enter still work).
- **4K video now plays using your graphics card.** High-resolution clips (above 4K30 —
  where software decoding could not keep up full screen) decode on the GPU and convert
  color on the GPU, roughly tripling the playback headroom on 4K60 HEVC footage and
  starting playback faster. Lighter clips keep the proven software path, which also
  remains the automatic fallback on machines without a capable GPU. HDR (HLG/Dolby
  Vision) clips stay on the software path for now so their brightness stays correct.
- **Smoother animation and Live Photo playback.** Showing each frame no longer creates
  fresh GPU resources: frames now reuse one resident texture and upload buffer, which
  removes the worst-case per-frame hitches (95th-percentile frame-present overhead dropped
  about 3x at 1080p) and stops playback from continuously allocating memory. This also
  lays the presentation groundwork for video playback.
- **Live Photos start playing almost immediately on every platform.** Pressing `P` now begins
  playback as soon as the first frames are ready and keeps extending the motion while the rest of
  the clip decodes in the background, instead of waiting for the whole clip first. On Windows this
  cuts the wait from about one to two seconds on a typical Live Photo down to a fraction of that,
  and wide-gamut (P3) clips keep their correct color. On a slow machine playback may briefly pause
  if decoding can't keep up, then resume, which is a fair trade for a near-instant start.

### Fixed
- **Icons are crisp at 125%/150% display scaling.** Toolbar and dialog icons were slightly
  blurred and looked flattened at the bottom on fractional display scales (and worse over Remote
  Desktop); they now render pixel-aligned and sharp.
- **Older camera clips now play with sound on Windows.** Some legacy clips — notably Motion
  JPEG AVIs from 2000s-era point-and-shoots (like `.AVI` files from Canon and Fujifilm
  cameras) — played their video but stayed silent, because the previous audio player refused
  to open those files. Audio now goes through the same media layer that already decodes the
  picture, so anything Windows can play the video of, it can now play the sound of too. Modern
  MP4/MOV clips are unaffected and stay in sync.
- **Dropping files onto the window now focuses PhotoBlaze.** Previously the drag's source
  (usually Explorer) kept keyboard focus, so the freshly opened photos ignored the arrow
  keys and Space until you clicked the window.
- **The folder tree (Shift+F) now shows for ZIP and 7z archives.** Opening an archive that has
  internal folders showed an empty "Folders" panel. The panel was reading only the on-disk folder
  browser, which does not apply to an archive, so the archive's folder list never appeared. It now
  falls back to the archive's own folder tree (the same way it already worked on macOS), and
  clicking a folder re-scopes the view to it.
- **ZIP/7z archives now open on machines with less RAM.** The memory check that guards against
  opening an archive too large to fit in RAM was too conservative and could refuse *every* archive
  — even a tiny one — on a machine with around 8 GB of RAM. It now reserves memory before applying
  its safety margin, so normal archives open on smaller machines while genuinely oversized ones are
  still refused.
- **The About and Settings dialogs now open on virtual machines / GPUs without a low-power
  Direct3D adapter.** On some setups (notably a Parallels VM on Apple Silicon) opening a dialog
  picked an OpenGL compatibility adapter that couldn't be initialized, so the dialog never
  appeared and the photo behind it was left stretched. Dialogs now use the same Direct3D backend
  as the viewer, so they open reliably and the main view is undisturbed.
- **The Keyboard Shortcuts panel (`/` or `?`) no longer clips its last rows** on shorter windows.
  Its rows and spacing were tightened and it now uses more of the available window height, so all
  the shortcuts stay visible instead of the bottom of the list being cut off.
- **The play hint (the `▶ Play` / Live Photo pill) now fades out cleanly.** Its background, border,
  and shortcut keycap fade together with the label, instead of the label fading while the pill's
  shell lingered on screen — and on Linux it no longer stays stuck until you move the mouse.

## [0.1.1] - 2026-07-08

### Added
- **macOS now updates itself.** PhotoBlaze checks for new versions in the background and, when
  one is available, downloads it and installs it the next time you quit — the same hands-off
  updating the Windows version already has, so you no longer need to re-download the DMG by
  hand. There's a **PhotoBlaze ▸ Check for Updates…** menu item to check on demand, and two
  switches in **Settings ▸ General ▸ Startup** — "Automatically check for updates" and
  "Download and install updates automatically" — if you'd rather manage it yourself. Update
  checks send only your version and macOS version, never anything about the photos you view.
- **Photoshop `.psd` files now open.** PhotoBlaze shows the flattened composite Photoshop
  saves inside the file (the default "Maximize Compatibility" merged image), so a `.psd` in a
  folder displays and flicks past just like any other photo — no Photoshop, no layer
  flattening, no slowdown.
- **macOS: transparent toolbar** (Settings ▸ Appearance ▸ "Transparent toolbar", on by
  default) — a zoomed or filled photo now extends up under the translucent glass toolbar
  instead of being cut off by an opaque bar, so you see more of the image. A soft gradient
  keeps the title readable over a bright photo. Fit-to-window is unchanged, and you can turn
  it off.
- **macOS: hold the toolbar nav / random buttons to "blaze"** — press and hold ‹ ›, the
  shuffle pair (or click-and-hold) and the toolbar flies through photos exactly like holding
  the arrow keys or Space: self-paced to how fast frames decode, so it never skips or stalls.
  A quick click is still a single step. Mouse users get the same fast flick-through the
  keyboard has always had.
- **macOS: "Set as Default" photo viewer** (Settings ▸ General ▸ File Associations). One click
  makes PhotoBlaze the default for every image type it supports — JPEG, PNG, GIF, TIFF, BMP,
  ICO, WebP, HEIC/HEIF, AVIF, JPEG XL, SVG, TGA, and **each camera's RAW format**
  (Sony/Nikon/Canon/Adobe DNG/Fuji/Panasonic/Olympus/Samsung/Pentax) — instead of hunting
  through Finder's Get Info ▸ "Change All…" one type at a time. It also updates the Quick Look
  "Open with" button (same setting). The row shows whether PhotoBlaze is already your default,
  and prompts you to move the app to Applications first if it's running from a quarantined copy.

### Changed
- **"Quick Full Screen" is now named as such**, everywhere it appears (View menu, toolbar,
  keyboard help, the exit hint, and the right-click menu) — so our instant borderless mode
  reads clearly apart from macOS's native full screen sitting right below it. Its shortcut is
  **F** (with **⌥⏎** alongside). On macOS, **F11** is no longer surfaced as the secondary
  shortcut in Settings ▸ Shortcuts — it needs the **fn** key and clashes with Mission Control,
  so it's now a hidden alternate that still works. F11 stays the visible secondary on Windows.

### Fixed
- **Panels no longer stretch while you resize the window.** As you dragged the window edge, the
  Folders tree, the Inspector, and the info line were stretched to the new window size (their
  text and rows squished or elongated) until you let go. They now redraw at the new size every
  step of the drag, so they keep their shape and reflow smoothly.
- **Panels no longer leak scrolled-off rows past their edges.** When the Folders tree or
  the Inspector held more rows than fit, a thin slice of the next scrolled-out row was
  painted outside the panel — into the header above and over the photo below. Rows now
  clip exactly at the panel's edge.
- **macOS: the Folders and Details panels are easier to resize.** Hovering the draggable inner
  edge of either panel now shows the ↔ resize cursor (the photo canvas was reasserting the plain
  arrow over the whole window and hiding it), the grab zone is wider and centered on the border
  so it's no longer a near-invisible one-pixel target, and dragging tracks the pointer smoothly
  instead of flickering between two widths.
- **macOS: the panels now fade in and out smoothly and consistently.** The Folders and Details
  panels used to pop instantly on close and, when hidden together with Tab, briefly collapse
  their contents before disappearing — a visible height jump. Now the Folders panel, the
  Details panel, the info line, and Help all share one quick, gentle fade whether you toggle
  them individually or hide everything at once, holding their full size the whole way out.

## [0.1.0] - 2026-07-07

### Added
- **macOS: a window toolbar** (customizable, hideable) for mouse-driven control — a
  discoverability layer over the keyboard-first core. The default set centers a **Previous /
  Next** pair, a **Random / Previous-Random** pair, a **Previous / Next Folder** pair (double
  chevrons — jump to a sibling folder), a **Slideshow** control that shows the current interval
  (e.g. "4s"), and a **Play Animation** button that dims on stills and lights
  up while a Live Photo / animation plays; trailing are a **Rotate ⟲ / ⟳** pair, toggles for the
  **Folders**, **Image Info**, and **Details** panels (which light up with a tinted background
  while open), and a **Fullscreen** button (kept until last as the window narrows, so it stays
  reachable). Every control just fires the command its keyboard shortcut and menu item already
  do. It merges into the title bar (which now shows the filename
  with the **folder name and "N of M" counter** as a subtitle — e.g. "Vacation · 2 of 147"),
  rendered in the system's Liquid Glass, and
  because it lives up there it doesn't eat into the image. Entering fullscreen **by the
  toolbar/menu** briefly shows how to get back out ("Press F to exit fullscreen") — skipped when
  you used the key yourself. **View ▸ Hide Toolbar** (⌥⌘T) turns it off, **View ▸ Customize
  Toolbar…** lets you drag your own set in or out (Scale, Zoom, Save Rotation, Pin, Compare,
  Copy, Copy Path, Show in Finder, Delete, Describe, Text, Open, Settings, Enclosing Folder),
  and it auto-hides in full-screen — so power users can make the window
  completely chrome-less again.
- **Windows: the rich panels are now a real UI (egui).** On the Windows/winit build, the
  **Help** (`?`), **Inspector** (Details / Text / Describe — `⇧I`, `T`, `D`), and **folder
  tree** (`⇧F`) panels are drawn with a proper retained-mode UI over the photo instead of the
  CPU-rasterized overlay — so they **scroll**, you can **select and copy** text, the folder
  tree has **disclosure chevrons** (browse without loading) and count badges, the Inspector
  has a **tabbed** header, and long metadata / descriptions no longer get cut off. They track
  the OS light/dark theme and composite correctly on SDR and HDR displays, with no cost to
  the flick-through hot path (the panel is only redrawn when it changes). This brings the
  Windows panels toward parity with the native macOS panels; drag-to-move and resize are still
  to come, and the toasts stay on the fast CPU layer.
- **Windows: the image info line (`i`) is now a real UI too**, matching the panels: it shows
  `folder/name · W×H`, a **Live-Photo** or **animation** mark, and a **codec badge**, and it
  **auto-ducks** — the folder tree / inspector shrink so they never cover it. Its fields
  (folder / filename / resolution / codec), position, and *show-by-default* are all in Settings
  ▸ Appearance, and the *Info panel opacity* slider now drives the whole chrome.
- **Windows: the folder-scan progress is now an ambient pill**, matching the panels and the
  macOS scan pill. When you open a large or deeply-nested folder, a top-center pill shows a
  spinner, the folder name and a live **N found** count, the sub-folder currently being
  scanned, and a **Cancel** button — so a slow scan is clearly in progress and stoppable
  without ever blocking your browsing (whatever's already found stays put when you cancel).
- **The image info line is now configurable** (Settings ▸ Appearance ▸ *Image Info*): a
  **"Show image info by default"** toggle sets whether the `i` readout starts shown on launch
  (the `i` key still toggles it live), a **Position** picker places it, and per-field
  checkboxes for **Folder**, **Filename**, **Resolution**, and **Codec** let you dial in
  exactly what it lists (Folder is prepended to the filename with a `/`). Field changes apply
  live, and at least one field always stays on.
- **A "Panel opacity" slider** (Settings ▸ Appearance) sets how much of the photo shows
  through the native panels — the folder tree, inspector, and scan pill. It defaults high to
  keep text crisp, and you can dial it down to ~50% to see more of the image behind the
  chrome. The panels now share one consistent backdrop (material, border, shadow), and the
  scan pill's heavier blur is dialed back to match the rest.
- **Finder-style folder browser on macOS** — the **⇧F** folder tree is now a real
  filesystem browser: **disclosure chevrons** expand/collapse folders (pure browsing — no
  scan), so you can walk through photo-less folders to *find* the one you want, then click a
  folder's **name** to open it. Siblings show at every level, the current folder is
  highlighted with a photo-count badge, an "up to *parent*" row climbs a level, and it
  scrolls. **Drag its right edge to widen it.** Reads happen off-thread, so a slow network
  share never freezes a click. (Archives keep the simple scoped list.) As you navigate, the
  tree **scrolls the current folder into view** and **auto-collapses branches you've moved
  past** — while leaving any folder you expanded yourself open — so it stays tidy on a tall
  tree or a short window.
- **Native Inspector panel on macOS** — Details, Text, and Describe are now one tabbed
  macOS panel (top-right, parallel to the folder tree) instead of three separate on-image
  overlays: crisp native text, an **icon + label** tab bar for Details / Text / Describe,
  selectable values you can copy, and a **✕** to close. **Drag its left edge to widen it.**
  It stays live — switching photos, or waiting on OCR / an AI description, updates in place.
  A **Copy button** (⧉ in the header) copies the whole active tab — details, recognized
  text, or the AI description — in one click, and you can now **drag-select across the whole
  readout** instead of one value at a time. **AI descriptions render full Markdown** —
  headings and bullet/numbered lists, not just inline bold/italic. The Describe tab has an
  **Ask** button that opens the follow-up-question dialog right there (no ⇧D needed).
- **Redesigned welcome screen on macOS** — with no photos open, the window shows a clean
  **Open File** / **Open Folder** pair (each with its shortcut key) and nothing else.
  Dragging images onto the window still works.
- **Native keyboard-shortcuts panel on macOS** — the **?** help panel is now a real
  macOS panel: crisp native text, keys shown as little keycaps (with plain separators
  so a `/` key never reads as "or"), two columns per section for a compact scan, and a
  **✕ close button** so you can dismiss it without hunting for the key. (First of the
  on-image panels to go native on Mac; the rest follow.)
- **File info position (Left / Center / Right)** — choose where the one-line info
  readout (**I**) sits along the bottom edge, under **Settings ▸ Appearance**.
  Whatever it shares a corner with gets out of the way: the detailed panel lifts
  above it, the folder tree shortens, and even a long filename spanning the width on
  a narrow window pushes both aside — so the info never overlaps another panel.
  Defaults to the right (unchanged).
- **Hide Panels (Tab)** — press **Tab** to hide every on-image panel (the detailed
  info / text / description panel, keyboard help, and the folder tree) without
  closing them, and Tab again to bring them all back — the Photoshop-style
  "unclutter my view" toggle. Any panel shortcut pressed while hidden reveals the
  panels instead of appearing to do nothing. Also in **View ▸ Hide Panels**. Toasts
  and the one-line info readout are unaffected.
- **AI image descriptions (D) + Ask about an image (⇧D)** — describe the current
  photo with a vision model, entirely on hardware you control. Press **D** for a
  description in an on-image panel (press **D** again to retry if it failed), or **⇧D**
  to type your own multi-line question — *"What products are visible?"*, *"What year
  does this look like?"* — and get an answer in the same panel. **Edit ▸ Copy AI
  Description** (and the right-click menu) puts the text on the clipboard. Configure it
  under **Settings ▸ AI**: point it at any OpenAI-compatible local model server (LM
  Studio, Ollama, llama.cpp — on this machine or your own network), pick a response
  length, and customize the prompt. Opening the tab fills a **model picker** with what
  the server is already serving — vision-capable models listed first — so you choose
  from a list instead of typing an exact model name; **Test & list models** re-checks
  it after starting a different model. Optional **auto-describe** describes each
  photo as you move to it while the panel is open. The prompt includes salient photo
  metadata (time, camera, location) framed as *unverified* so the model trusts what it
  sees. Nothing is uploaded except to the endpoint you configure; results are kept in
  RAM only. Available on Windows and Mac.
- **Copy Text from Image + text panel (T)** — read and copy the text visible in a
  photo, entirely on your machine. Press **T** to see the recognized text (and any
  QR-code contents, listed on top) in an on-image panel, or use **Edit ▸ Copy Text
  from Image** / the right-click menu to put it straight on the clipboard — the toast
  confirms what you got ("Copied 214 characters", "Copied text + 1 QR code"). Works
  on rotated photos and inside archives; results are kept in RAM only and nothing is
  ever uploaded. Recognized text is grouped back into **paragraphs**, so copied text
  reads as flowing blocks instead of hard-broken lines. Text recognition uses your
  operating system's built-in on-device OCR — Windows OCR on Windows, Apple Vision on
  the Mac — and QR codes are read on both.
- **Light mode** — a new **Theme** setting under Settings ▸ Appearance: **System**
  (default) follows your OS light/dark theme live, or pin **Light** / **Dark**. In light
  mode the on-image overlays (info panels, toasts, help, folder tree, scan card) switch
  to dark text on a translucent white panel, and the light and dark themes each get
  their own background color around the photo (both pickable in Settings). Available on
  Windows and Mac; on the Mac a pinned theme also restyles the menus and windows.
- **Folder tree overlay (⇧F)** — press Shift+F for a "you are here" view in the top-left
  corner: the opened folder shown **among its neighbours** under its parent, then **every
  folder at every level** down the path to the current photo's folder — the path expands
  in place, tree-style, so even when opening a big library lands you deep in its first
  folder, all the top-level folders stay one click away, and after hopping into a folder
  its adjacent folders are still right there (very deep paths collapse in the middle).
  The current folder is highlighted, its subfolders nest below it, and rows carry a
  photo-count badge where the count is already known. **Click a folder to open it**
  (exactly like Open Folder), including
  the topmost row — the parent of what you opened — to go up; rows light up on hover.
  Long lists show "… n more" rows — click them to page. The tree keeps tracking the
  current folder even while you hold a key and fly, works over an *empty* folder (so a
  photo-less folder is a navigation point, not a dead end), and shows an archive's
  internal folders for a `.zip`/`.7z`. Press ⇧F again to dismiss.
- **Folder navigation inside archives** — the ⇧F tree's rows are clickable for
  `.zip`/`.7z` decks too: click an internal folder to view just that folder, and click
  the archive's own row to bring the whole archive back. Switching folders is instant
  even for a huge solid `.7z` — the archive is never re-read or re-decompressed, and an
  unlocked encrypted archive never re-asks for its password. Nothing is ever extracted
  to disk.
- **Go commands** — walk the folder hierarchy without the picker: **Parent Folder**
  (⌘↑ on Mac, Finder's chord; Alt+↑ on Windows, Explorer's) plus **Previous / Next
  Folder** (⌘←/⌘→ / Alt+←/→) to step between sibling folders — the fast way to review
  a folder-per-day library. In the new Go menu, rebindable in Settings ▸ Shortcuts.
  They work inside archives too: Parent Folder steps back out one folder level at a
  time (then to the folder on disk containing the archive), and Previous/Next Folder
  step between the archive's sibling folders.

### Changed
- **Windows: a one-click installer with automatic updates.** PhotoBlaze now installs on Windows
  in a few seconds with no admin prompt (a per-user install), and keeps itself current — new
  versions download in the background and install when you quit, so you never fetch an installer
  again. This replaces the old Windows Installer (`.msi`) package.
- **Windows: crisper panel corners.** The egui chrome (info pill and codec badge, the Help /
  Inspector / folder-tree panels, the scan pill, keycaps, tab selector, and the Open buttons)
  now draws its rounded rectangles with an analytic signed-distance-field shader instead of
  egui's tessellated corners, so they're clean and resolution-independent on HiDPI instead of
  reading soft or jagged — closer to the native macOS panels. The codec badge's `JPEG` / `BMP`
  label is also vertically centered on its ink now (it used to ride high). No cost to the
  flick-through hot path (the chrome is only redrawn when it changes).
- **Fullscreen's primary shortcut is now `F`** (was `F11`) — the Mac-idiomatic single key,
  shown on the View-menu badge, the keyboard-help panel, and the toolbar's exit hint. `F11`
  and `Alt`/`Option`+`Enter` still work for Windows muscle memory.
- **Windows: the Live-Photo / animation play hint is now a real UI (egui).** When you settle on a
  Live Photo or animated image, a `Play` button (with the keyboard shortcut) flashes bottom-center
  above the info line — matching the welcome buttons — and clicking it plays; it holds while you
  hover and fades on its own otherwise. Replaces the CPU-rasterized hint.
- **Windows: the empty-state welcome screen is now a real UI (egui).** When no photos are open,
  the centered **Open File** / **Open Folder** buttons (each showing its keyboard shortcut) are
  drawn with proper buttons over "or drag and drop here", matching the macOS welcome screen —
  instead of the CPU-rasterized hint.
- **Settings polish.** The Settings tabs now have icons (General / Appearance / AI / Shortcuts),
  each section's heading sits **above** its card (like macOS System Settings) with a clearer gap
  grouping the heading to the settings it labels, and empty shortcut slots show a dimmed
  **Set / Add** placeholder so it's obvious which keys aren't bound yet. Secondary text in the
  panels (counts, sub-labels) is also a shade lighter in dark mode for better contrast over a
  bright photo.
- **All transient notifications are now native on macOS.** Every toast — Copied, rotate,
  Saved rotation, Pinned/Unpinned, muted/unmuted, deleted, "Scan stopped", "No photos in
  *Foo*", "Recursive folders: on", … — is one consistent **bottom-center SwiftUI pill**
  (matching the panel chrome) instead of the old CPU-rasterized HUD overlay, with a fitting
  icon (or icon-only for rotate/mute/save). One notification surface for the whole app.
- **Scanning a large folder no longer blocks you with a modal dialog.** On macOS a slow
  folder walk used to pop a modal "Scanning…" dialog. Now a compact, non-blocking pill sits
  at the top-center — "Scanning *Folder* · N found" plus the sub-folder being walked, with a
  **Cancel** — so you keep browsing the photos already streaming in while the rest scans.
- **A folder's own photos now come before its subfolders' photos** (files-before-folders),
  instead of interleaving them by name. So browsing — and previous/next folder (⌘←/→) —
  finishes a folder's photos and *then* descends into its subfolders, reading the way the
  folder tree looks top-to-bottom, rather than bouncing in and out of subfolders mid-folder.
- The one-line info readout (**I**) is now fully independent of the bigger panels:
  it has its own spot in the bottom-right corner and can be shown **at the same time**
  as the detailed info, text, or description panel (which now sits just above it)
  rather than replacing it. Its View-menu checkmark tracks independently of **Show
  All EXIF Info** — both can be checked at once.
- **The welcome screen's Open File / Open Folder buttons now share the play hint's on-image
  hover cue** (a subtle glow + 1% grow) — one consistent "this floats on your photo and
  responds to you" language for on-canvas controls, distinct from ordinary panel/dialog
  buttons.

### Fixed
- **Windows: photos are no longer clipped at the bottom edge in fullscreen.** The borderless
  fullscreen window was being sized to the monitor while the menu bar was still attached, so it
  ended up one menu-bar taller than the screen and its bottom hung off the display — cropping the
  bottom of any photo that filled the height. The menu is now hidden before the window is sized, so
  fullscreen matches the monitor exactly.
- **Windows: the image info line no longer jumps around between photos** when it's centered or
  right-aligned. It was positioned from the *previous* photo's width, so every photo landed it in a
  slightly different spot; it's now pinned to the correct spot from each photo's own width.
- **Random photo ([enter]) is actually random now.** The shuffle order was seeded with a fixed
  constant, so opening the same folder always produced the exact same "random" sequence — every
  launch, every day. Each open now draws a fresh seed, so the order varies from one open to the
  next as intended.
- **Flying through photos stays smooth with the Inspector's Text or Describe tab open.** OCR
  and AI-description scans were being kicked on *every* photo you flew past — an OCR thread or a
  describe network round-trip per frame — competing with decode and stuttering the flight. They
  now wait until you settle, so the current photo is scanned the moment you stop, and nothing
  expensive runs during a held fast scrub.
- **Tab (Hide Panels) now also hides the basic info line, not just the folder tree and
  Inspector/Help.** It stayed on screen after Tab, leaving one piece of chrome behind when
  everything else was supposed to declutter. It comes back — along with anything else Tab had
  hidden — the moment you press Tab again or **i**.
- **The welcome screen's Open File / Open Folder buttons no longer stack up multiple file
  pickers.** Clicking either button again (or clicking the other one) while an Open panel was
  already up spawned a second one on top instead of doing nothing — both buttons now disable
  while a panel is open.
- **Opening a folder with no photos no longer interrupts you with a modal alert.** On macOS,
  opening an empty folder popped a blocking "No supported images" dialog you had to dismiss.
  Now it keeps the photos you were already viewing and shows a brief "No photos in *Foo*"
  toast instead — a mis-click into an empty or deep folder never interrupts browsing.
- **Open Parent (⌘↑, or `P`) now climbs one level at a time from the folder you're viewing.**
  It went up from the folder you originally *opened*, so after a recursive open it jumped
  toward `/Users` or `/`. It also used to **get stuck**: when a parent had no photos of its
  own, opening it re-landed you in the same deep subfolder, so the next ⌘↑ went nowhere. Now
  it steps up from the current photo's folder and each further press continues up from the
  last, so repeated presses walk cleanly up the tree instead of oscillating.
- **Folders now sort case-insensitively (like Finder), not by raw byte order.** A recursive
  open landed on whatever subfolder sorted first by ASCII — where *every* uppercase letter
  beats *every* lowercase one, so `Screenshots` came before `onlinethumbnailcache` and the
  first photo showed up somewhere unexpected. Now folders and files order the way you'd
  expect (and the way the folder tree already showed them), so the first photo is in the
  first folder alphabetically — and ⌘←/→ lines up with the tree.
- **Previous/next folder (⌘← / ⌘→, Alt+← / →) is now "next photo, but by folder."** It used
  to jump to a sibling of the *opened* folder — after a recursive open, a seemingly random
  far-away subtree. Now it steps to the next/previous **folder boundary in the deck's
  sequence**: it enters subfolders, walks siblings, and climbs back up exactly as you'd
  traverse the tree — landing on that folder's first photo. Instant, no re-scan, and it can't
  dead-end (every jump lands on a real photo). (A single-folder deck still opens the next
  folder on disk.)
- Pressing **I**, then **⇧I**, then **I** again now does what you'd expect (turns the
  one-line readout off) instead of appearing to do nothing — the line and the detailed
  panel are no longer entangled.
- **Encrypted 7z archives with many files open dramatically faster.** The AES key was
  being re-derived from the password for every file in the archive (tens of
  milliseconds each, slower still with a long password), which dominated the whole
  open on stored, per-file-encrypted archives — the kind 7-Zip makes with "Store"
  compression plus a password. The key is now derived once and reused, the same way
  7-Zip does it: a 3,000-file encrypted archive drops from about 4 s to under 0.1 s,
  and a many-thousand-photo archive that took ~24 s now opens in the time it takes to
  read it from disk.

- **Solid 7z archives open about 2.5–3× faster.** A solid archive packs everything into
  one compressed block, which used to decode on a single core. When enough RAM is free,
  PhotoBlaze now decodes the block's independent chunks across many cores (a 10 GB
  archive drops from roughly 30 s to about 10 s). Non-solid archives already decoded in
  parallel and are unchanged, and on machines without the spare RAM the open simply
  works as before.

- **The Mac app is now a native SwiftUI/AppKit application** — the same fast Rust engine, with
  the chrome rebuilt Mac-native: real menus with shortcut hints on every command, native dialogs
  (Finder-style delete confirmation, password prompts for encrypted archives, progress for big
  archives and folder scans), a native Settings window (⌘,) with a **keyboard-shortcut editor**
  (every command rebindable, two slots each), a true borderless fullscreen that uses every pixel,
  window position/size remembered across launches, and the title-bar proxy icon. It replaces the
  previous Mac build under the same app identity, so file associations and permissions carry over.

- **The Open dialog now starts in your last-used folder.** On a fresh launch with nothing
  open yet, pressing O opens the picker in the folder you last viewed, so you're back in
  your library in one keystroke — the bare launch itself stays on the empty open screen and
  never auto-opens anything. Pinning a folder in Settings makes the picker always start
  there instead; once photos are open, the picker follows the current photo's folder.
- **Holding a key waits a touch longer (250 ms) before it starts flying**, so a slow tap on
  next/previous no longer skips ahead by accident (new-install default; an existing hold-delay
  setting is unchanged).
- **macOS: the About panel now shows the full app details** — the build stamp (git commit)
  alongside the version, the tagline, a clickable link to the GitHub page, and the copyright.
- **macOS: Settings now save automatically** — every change (including shortcut edits) applies
  and persists the moment you make it, the Mac way; the Save and Cancel buttons are gone. The
  window is sized to fit its content (long lists like Shortcuts scroll).

### Fixed
- **Toggling fullscreen (F) on the Mac no longer stretches what's on screen or leaves
  it the wrong size.** The transition used to briefly balloon/squeeze the current
  photo — or the "Open File" / "Open Folder" buttons on the empty start screen — for a
  frame, and returning to a window could leave the buttons stuck mis-sized until you
  nudged the window or moved the mouse. Content now holds its natural size through the
  transition and settles to the new window within a frame; if the app ever misses a
  size change (e.g. moving between displays with different scales), it now self-corrects
  immediately instead of staying stuck.
- **Previous/Next Folder (⌘←/⌘→, Alt on Windows) now skip folders with no photos.**
  Stepping into an empty sibling folder used to stop everything with a "No supported
  images in that selection." dialog — and since the deck never moved, every re-press
  hit the same folder again. The commands now hop straight to the nearest sibling
  folder that actually contains photos (each candidate is checked only until its first
  image, in the background, bounded so one enormous photo-less tree can't stall a
  keypress); if nothing in that direction has photos, a quiet "No more folders with
  photos" toast says so. Explicitly opening an empty folder (the picker, a tree click,
  drag-drop) still reports it — clicks never get redirected.
- **The folder tree can no longer freeze the app on a slow drive.** Opening the tree
  (⇧F), crossing into a new folder with it open, and the Previous/Next Folder commands
  used to read the disk on the spot — on a network share or a spun-down external drive
  that could hang the whole app for seconds. The tree now appears instantly from what's
  already known and fills in folder listings in the background.
- **Windows: hidden system folders no longer show in the folder tree.** `$RECYCLE.BIN`,
  `System Volume Information`, and other attribute-hidden folders Explorer hides are now
  filtered out.
- **The folder tree tracks correctly when photos from more than one folder are open**
  (e.g. two folders dropped together) — it used to stop updating as you moved between
  the extra folders.
- **Old zips with Windows-style folder names now show their folder structure.** Archives
  written with backslash entry paths (some legacy Windows archivers) appeared flat, with
  the folder tree unable to group or track them.
- **Windows: a pinned Light or Dark theme now themes the whole window.** The title bar,
  menu bar, and menu dropdowns follow the Theme setting instead of staying on the OS
  scheme, so dialogs no longer render a light body under a dark title bar (or vice versa).
- **Light mode consistency:** the loading pie now renders in the light scheme instead of
  staying a dark disc; the "Press P to play" button re-themes even while hovered; and
  launching on a light desktop no longer flashes the open screen in dark colors for the
  first frame.
- **Windows: installing a build over an existing install now actually replaces it.**
  Same-version installs (all betas carry the numeric version 0.1.0) used to pile up as
  separate entries in "Installed apps" while leaving the previously installed files in
  place — you had to uninstall by hand to really update. The installer now treats an
  equal-version install as a full upgrade: old entries are removed (including any
  duplicates already accumulated) and the new files land fresh.
- **macOS: toggling fullscreen (F) no longer wipes the menu bar.** Entering or leaving the
  borderless fullscreen mode could reset the menus to a bare default set (just PhotoBlaze /
  View / Window / Help) until relaunch; the full File / Edit / Image / View menus now
  survive the toggle — and anything else that tries to replace them.
- **macOS: the Settings tab icons no longer shift upward on the first tab change.** The
  toolbar icons (General / Appearance / Shortcuts) used to render a touch too small and low
  when the window opened, then visibly snap into place on the first tab switch; they now
  render in their final position from the start.
- **macOS: ⌘W closes the frontmost window again** (Settings, About, or the viewer) — a
  standard Close Window item is back in the File menu.
- **macOS: closing the last window now quits the app.** ⌘W on the viewer used to leave
  PhotoBlaze running with no window and no way to get one back — File ▸ Open and
  Open Folder silently did nothing. PhotoBlaze is a single-window app, so closing that
  window now exits cleanly (the same no-trace teardown as Esc); ⌘W on Settings or About
  still just closes that window.
- **macOS: quitting in borderless fullscreen no longer restarts the app on the wrong
  monitor.** Launching back into fullscreen now lands on the display you were using, exiting
  fullscreen restores your last windowed spot (not a default frame), and the fullscreen state
  itself is now reliably remembered across launches. Restoring a window saved on a display
  with a different scale factor (e.g. a 1x ultrawide vs. a Retina display) also lands
  correctly now instead of falling back to the default screen.
- **macOS: opening a huge folder tree no longer blocks browsing behind the Scanning
  dialog.** The progress sheet now disappears the moment the first photo is found (you can
  start flicking immediately), and the corner **scan chip** — `Scanning "Folder" — N images
  found`, with a Cancel button — now shows on macOS while the rest of the tree streams in,
  matching Windows.
- **macOS: a keypress landing right after a dialog, menu, or panel closed could be silently
  ignored** (most visibly as "Esc needs pressing twice to quit") — keys aimed at the viewer
  window now register even before macOS finishes handing keyboard focus back to it.
- **macOS: HDR highlights now render at the panel's full brightness** (the previous build could
  crush them toward SDR on launch), and the display's HDR headroom follows the window across
  monitors — including HDR being toggled while the app is running.
- **macOS: sharp/blurry rendering after launch or moving between displays with different scale
  factors** (e.g. a 1x ultrawide and a Retina display) is fixed — the canvas now tracks backing-
  scale changes immediately, so on-screen buttons stay crisp and clickable.

### Added
- **Flicker compare mode — pin a photo, then flip.** Press **Y** to pin the photo you're on
  ("Pinned for compare"), browse anywhere, then press **Y** to flip between the pinned photo
  and where you are — full screen, at full resolution, instantly (the pinned photo stays
  ready on the GPU no matter how far away you fly). It's the blink-comparator way to pick
  the best of a burst: your eye catches what *changes*, which side-by-side can't do. Zoom
  into a detail first and the same crop carries across the flip (for same-size photos, like
  a burst pair). **Shift+Y** moves the pin to the current photo — or unpins, if it's already
  the pin. Both commands live in the Image menu and the right-click menu, both keys are
  rebindable in Settings ▸ Shortcuts, and the pin is forgotten when the app closes (nothing
  is ever written to disk).
- **Show the current photo in Finder / File Explorer.** A new **File ▸ Show in Finder** (macOS)
  / **Show in File Explorer** (Windows) command opens the photo's containing folder with the file
  selected, so you can jump from the viewer straight to the file on disk. It's greyed out for
  photos inside an archive (there's no file to reveal) and on the empty open screen.
- **Right-click a photo for a context menu.** A secondary-click over the image brings up the
  common per-photo commands — Next / Previous / Random / Previous Random, Rotate Left / Right,
  Play/Pause (for animated photos and Live Photos), Start/Stop Slideshow, Copy Image, Copy File
  Path, Copy Image Details, Show in Finder / File Explorer, and Enter/Exit Fullscreen. It works
  in fullscreen too, where the menu bar is hidden — so it's the easy way back out.
- **Copy a photo's details** to the clipboard as text (from the right-click menu) — the same
  facts the info panel shows (dimensions, format, size, and every EXIF tag).
- The **About** dialog now shows a **build identifier** (the git commit the build came from,
  with a `-dirty` marker for a build with local changes) under the version — so a specific
  build can be traced back to the exact code it was built from.
- **Play animated images on demand.** Animated **GIF, APNG, and animated WebP** (plus
  animated AVIF/HEIC on macOS) can now be played back — press **P** to play/pause. Landing on
  an animated photo flashes a brief **▶ Press P to play** hint (never while you're flicking
  through a folder). Step through frames one at a time with **.** (next) and **,** (previous) —
  stepping pauses playback, and holding either key scrubs. Browsing stays exactly as fast as
  before: flicking through a folder still only decodes each photo's first frame, and the whole
  animation is decoded only when you actually ask to play it. Once you pause on an animated
  photo it's quietly prepared in the background, so pressing **P** plays **instantly** even for
  slower formats (a large WebP or AVIF used to take a second or two to start). All three
  shortcuts are remappable (Settings ▸ Shortcuts) and available under the **Image** menu.
- The detailed info panel (**Shift+I**) now shows animation properties for an animated photo —
  the current frame / total frames (live while it plays or steps), the frame rate, the
  duration, and the loop count.
- **Live Photos play** (macOS): landing on an Apple Live Photo (a still with a companion
  `.mov` — both the older JPEG and newer HEIC variants) shows a **Live Photo ◉ Press P to play**
  hint; pressing **P** plays the motion using the same controls as animations (play/pause,
  frame-step with **.** / **,**). The motion is prepared in the background while you look at the
  still, so playback starts promptly, and it snaps back to the crisp full-res photo a beat after
  it finishes. The Live Photo's **audio** plays with the motion; mute it with **M** (or Image ▸
  Mute Live Photo Audio — the choice is remembered). The detailed info panel shows it's a Live
  Photo (with frame count / rate / duration), and the basic info line marks it **· Live**.
- **Live Photos play on Windows too.** The same Live Photo experience that landed on macOS
  now works on Windows: landing on an Apple Live Photo (a still with a companion `.mov`)
  shows the **Live Photo ◉ Press P to play** hint, **P** plays the motion with sound (mute
  with **M**), and frame-step / info-panel details all work identically. The motion decodes
  through the Windows OS decoder, so H.264 clips (iPhone 6s) work out of the box and HEVC
  clips (iPhone 7 and later) use the same **HEVC Video Extensions** the viewer already
  relies on for HEIC stills — plus the frames now follow the clip's true per-frame timing.
- **The open screen is now interactive.** With no photo loaded, PhotoBlaze shows two clickable
  buttons — **Open File** and **Open Folder** — each with its keyboard shortcut shown dimmed and
  right-aligned, menu style (`O` and, on macOS, `⇧ O`), reflecting anything you've remapped in
  Settings ▸ Shortcuts. They light up on hover and open the picker on click; the keyboard
  shortcuts still work exactly as before. (Replaces the old "Press O to open…" text hint.)

### Changed
- **HEIC is much faster on Windows.** Full-resolution HEIC decoding now uses a parallel
  CPU decoder (libheif) instead of the Windows built-in codec, which can only decode one
  photo at a time no matter how many cores the machine has. Measured on the reference
  corpus: ~4.9× the throughput (45 vs 9 full decodes per second on a 12 MP iPhone HEIC),
  with lower per-photo latency too. Combined with the existing look-ahead, sharp
  full-resolution HEICs are now pre-decoded around the photo you're on as you browse,
  instead of each one sharpening only after you land on it. (Ships in the installer
  build; H.264/HEVC hardware requirements are unchanged.) It also **renders some HEICs
  more faithfully**: full-range HEICs (e.g. photos received through Apple shared albums)
  were decoded by the Windows codec as TV-range — washed out, with the deepest shadows
  and brightest highlights clipped. The new decoder honors the file's tags, so those
  photos get their real contrast back.
- **"Set default…" now opens PhotoBlaze's own page** in Windows Settings ▸ Default apps —
  with every image type PhotoBlaze handles listed for one-click switching — instead of
  dumping you at the top of the generic Default apps list. Works on existing installs
  (the button registers the app for your user account if the installer hasn't), and new
  installs register machine-wide.
- **The keyboard-help overlay (`?`) has been redesigned.** It's now grouped into sections
  (Browse, View & Zoom, Animation, Files & App) with each command's shortcut shown dimmed and
  right-aligned, menu style, at a more compact size that fits on screen. On macOS the shortcuts
  use the real Mac symbols (⌘, ⇧, ⌥, ⌫) matching the menu bar — so **Move to Trash** shows ⌘⌫
  and **Copy** shows ⌘C, not the legacy keys. It also now lists shortcuts that were missing:
  Play/pause and frame-step, Mute Live Photo audio, Copy, Copy path, Save rotation, Undo, Move
  to Trash, and Delete permanently. All reflect any bindings you've remapped in Settings.
- The animation / Live Photo **"Play" hint** now matches the open-screen buttons — a play icon,
  "Play", and the dimmed **P** shortcut — instead of the old "Press P to play" text, and it's a
  **real button**: hovering it holds it open (pausing the fade) and lights it up, and clicking it
  plays, just like pressing **P**.
- The **Image** menu now shows each command's keyboard shortcut — Next, Previous, Random,
  Rotate, Play/Pause, and frame-step — reflecting any shortcut you've remapped in
  Settings ▸ Shortcuts. (On macOS these are single, unmodified keys like Space or R that a
  native menu can't display as a shortcut without hijacking the key, so those items stay
  label-only there; the ⌘-based commands such as Copy and Settings show their shortcuts as
  usual.)
- **Open a folder and start browsing immediately — even huge, deeply nested ones.** Folders
  now **stream in**: the first photo appears almost at once and the rest of the library loads
  in the background while you flick through what's already there, instead of waiting for the
  whole tree to be scanned. Applies everywhere a folder opens — the picker, drag-and-drop,
  double-click / file association at launch (the window shows right away now), and the
  recursive toggle (**Ctrl+R**). A small status card in the top-right names the folder and
  shows the image count climbing as it loads, with a **Cancel Scan** button (or
  **File ▸ Stop Scanning**) to stop early and keep whatever has loaded so far. Toggling
  recursion **off** mid-scan instantly drops to just the current folder. Quick folders still
  open instantly with no chrome; a "Scanning Folder" dialog only appears in the rare case the
  very first photo is slow to find.
- The on-image overlays (the info pill, toasts, the EXIF/help panel, and the "Press O to
  open" hint) now have wider, balanced side margins, so their text no longer sits tight
  against the left and right edges.

### Fixed
- **Show in File Explorer now actually shows the photo** (Windows). For almost any real
  path — anything with a space in it, like `My Photos`, or a comma — it opened a Documents
  window with nothing selected instead of the photo's folder, because of how Explorer's
  `/select` argument was quoted. Paths with spaces, commas, and non-ASCII characters all
  select correctly now.
- **A slow folder scan no longer waits for you to move the mouse.** On a slow scan of a
  quiet app, the first photo (and the scanning progress card) could stall until the next
  input event woke the app; the app now keeps checking on its own while a scan streams in.
- **Opening another archive while a large 7z is still loading is now reliable.** The
  superseded load could finish late and replace the archive you'd just opened; and if an
  archive open died mid-load, its "Opening…" spinner could stay up forever.
- **The "Scroll wheel" setting (pan vs zoom) now applies to a macOS trackpad**, not just a
  mouse wheel. A two-finger swipe was hard-wired to pan and ignored the setting; now Pan stays
  the default but choosing **Zoom** makes a swipe zoom, and **Ctrl+swipe** always does the other
  action (so zoom is reachable even in Pan mode). Pinch-to-zoom is unchanged.
- **Copy Image (Ctrl/⌘C) now works on macOS** — it previously showed "Copy failed" because the
  image clipboard was implemented only on Windows. Like on Windows, it copies **both** the
  picture and a reference to the file, so a paste does the right thing wherever it lands: paste
  into an image editor or document to get the **pixels** (with any unsaved rotation baked in),
  into **Finder** to copy the **file**, or into a **terminal** to paste its **path**. (An image
  from inside an archive has no file, so it copies just the pixels.) Linux copies the pixels via
  arboard. Copying the *file path* as text (⇧⌘C) already worked everywhere.
- **Opening a photo is instant again — no more multi-second freeze on RAW or large images.**
  The first frame is now decoded **preview-first**: for RAW and HEIC the embedded preview shows
  immediately (and refines to full resolution in the background a moment later), instead of
  running a full sensor demosaic on the UI thread — which could beachball the app for many
  seconds when opening a large RAW from Finder. Plain photos (JPEG/PNG/…) open the same as
  before, just without ever blocking the window.
- Overlay text (the scan card, info/EXIF panel, loading spinner, "Press O to open" hint) now
  stays crisp when you drag the window between monitors of different pixel density (e.g. a
  regular display and a Retina one) — previously it was baked at the starting monitor's DPI
  and looked soft or wrong-sized on the other. The viewer photo itself was already correct.
- macOS app icon: the Clear/Tinted (monochrome) appearance now reads high-contrast
  instead of washed-out, using dedicated white artwork shown only in that mode.

## [0.1.0-beta.4] - 2026-06-30

### Added
- **Mouse & trackpad zoom & pan** (Windows + macOS): plain scroll pans the image — a
  mouse wheel or a precision-trackpad two-finger swipe, both axes — and **Ctrl+scroll**
  zooms toward the cursor. Prefer the reverse? Settings ▸ General ▸ Navigation Feel ▸
  **Scroll wheel** switches the default to zoom (then Ctrl+scroll pans). Click-and-drag
  also pans when zoomed in (the pointer shows an open hand when panning is available, a
  closed hand while dragging).
- **Trackpad gestures** (macOS): native pinch to zoom and two-finger swipe to pan, both
  centered on the pointer — a pinch zooms toward wherever the cursor is. A two-finger
  double-tap toggles 100% (the same as pressing `0`). On Windows these arrive as scroll
  events, so two-finger swipe pans and zoom is Ctrl+scroll (native pinch isn't available
  there).
- All of the above drive the same zoom/pan as the keyboard, so they share its limits and
  reset framing on each new photo.
- **Undo** (Ctrl+Z, ⌘Z on macOS, or Edit ▸ Undo) reverses a saved rotation, restoring
  the file's previous EXIF orientation. The Edit menu names what will be undone (e.g.
  "Undo Save Rotation") and is greyed out when there's nothing to undo. Multiple saves
  can be undone in turn.
- Fullscreen now also toggles with the **F** key, alongside F11 and Alt+Enter — a
  memorable, discoverable shortcut for the app's most-used view toggle.
- **Copy File Path** (Shift+Ctrl+C, or Edit ▸ Copy File Path) copies the current
  photo's full path to the clipboard as text — handy for pasting a filename into a
  message or terminal. (Archive entries copy their entry name.)
- macOS: a native **Window menu** — Minimize (⌘M), Zoom, and Bring All to Front,
  with the standard live window list — so the menu bar feels like a real Mac app.
- macOS: the title bar now shows the current photo's **proxy icon** (in windowed
  mode) — ⌘-click it for the enclosing-folder path, or drag it into Finder, Mail, or
  Messages to move or attach the file, just like a native document window.
- macOS: **file associations** — PhotoBlaze now registers for images (JPEG, PNG, HEIC,
  AVIF, GIF, TIFF, BMP, WebP, JXL, SVG, ICO, JP2, TGA, camera RAW) and image archives
  (ZIP, 7z). Double-click one in Finder, drag it onto the Dock icon, or use "Open With ▸
  PhotoBlaze" and it opens — whether or not the app is already running. It registers as a
  candidate handler (never silently seizes a file type's default).

### Changed
- New **brand accent color**: the chrome's accent is now PhotoBlaze orange (`#FF4915`,
  matching the logo) instead of the old Windows blue — it colors primary buttons, the
  active settings tab underline, selection, slider and toggle fills, and accent icons.
  Destructive actions (the Delete button, danger icons) use a darker red sibling so they
  stay clearly distinct from the primary action.
- The **Settings** dialog's section tabs (General / Display / Shortcuts) are now a clean
  underlined pivot — the active section is marked by a slim accent underline instead of a
  filled blue button, so it no longer competes with the Save button, and the labels have
  room to breathe.
- The View menu's info items are now clearer: **Info Panel** → **Show Image Info**
  and **Full EXIF** → **Show All EXIF Info**.
- macOS: the **Enter Full Screen** menu item (⌃⌘F) now flips to **Exit Full Screen**
  while in full screen, matching standard Mac apps — and stays correct whether you
  toggle from the menu, ⌃⌘F, the green window button, or a Mission Control gesture.
- macOS: the borderless fullscreen (F / ⌥⏎) is now truly **chromeless** — it auto-hides
  the menu bar and Dock, reclaiming that screen space for the photo (and no longer
  clipping the photo's top edge behind the menu bar), while staying in the current Space
  (no full-screen animation). The menu bar and Dock still slide down/up on hover.
- macOS: the File menu's delete items now use Finder's idioms — **Move to Trash (⌘⌫)**
  and **Delete Immediately… (⌥⌘⌫)** — with the shortcuts shown in the menu. (The Del /
  Shift+Del keys still work too.) ⌥⌘⌫ is used rather than ⇧⌘⌫, which Finder reserves for
  Empty Trash.
- macOS: on-screen text overlays (the toasts and the info / EXIF panels) now render
  in **SF Pro**, the macOS system font, instead of Arial — a cleaner, native look.
- macOS: the 7z-archive memory check now reads the machine's actual available RAM
  instead of assuming a fixed 8 GB, so the "archive too large to open safely" guard
  is accurate on every Mac.

### Fixed
- The View menu's **Show Image Info** and **Show All EXIF Info** items now show a
  checkmark when their panel is on, so the menu reflects what's actually displayed.
- Opening a **large or deeply nested folder** (e.g. your whole Pictures library, or
  macOS's `~/Library`) no longer freezes the window — or crashes — while it's scanned.
  The folder is now walked off the main thread (the current photo stays interactive,
  and a slow scan shows a brief "Scanning folder…" note), and the scan can no longer
  loop forever on a folder symlink/alias that points back at itself.

## [0.1.0-beta.3] - 2026-06-29

### Added
- Settings now lets you choose where the **Open dialog starts**: the current photo's
  folder (default) or a specific folder you pin. Pinning a folder also stops the dialog
  from quietly remembering — and revealing — where you last browsed, on the next launch.
- Opening a 7z archive now shows a **progress bar with a Cancel button** instead of
  a spinner that could appear to hang forever — with the percentage and bytes
  decoded — so a large archive shows real progress and can be stopped at any time.

### Changed
- Opening a second archive while one is still loading now cancels the first instead
  of decompressing both at once (which could exhaust memory).

### Fixed
- Pressing Open (the file picker) while viewing photos inside an archive now opens in
  the folder that contains the archive, instead of trying to browse inside it — which
  failed on an encrypted archive with "Windows cannot open the folder."
- Password-protected 7z archives now open dramatically faster — often 30–40×. Two
  separate problems were fixed: the decryptor read the compressed stream in 512-byte
  chunks straight off disk (millions of tiny reads on a large archive — slow when
  cached, near-hung when not), now solved by buffering; and an archive of many small
  images (one encrypted block per image) derived each image's decryption key one after
  another. Those independent blocks now decode in parallel across all CPU cores — the
  way 7-Zip does it — turning a ~3-minute open of a 3,000-image encrypted archive into
  a few seconds, with memory still bounded.
- Cancelling a large archive open now stops promptly — even partway through a single
  big block — instead of only at the next block boundary.
- An archive mixing photos with large non-image files (videos, documents) opens
  faster: blocks containing no images are skipped instead of being decompressed.
- Hardened against malformed/hostile archives: an oversized or decompression-bomb
  entry (including a compressed SVG) is refused with bounded reads, so it can't
  exhaust memory before the decoder rejects it.
- Settings dialog now animates without needing the mouse to move: combo dropdowns
  open on click, the "Checking…" spinner advances, and the text cursor blinks —
  by honoring egui's requested repaint timing (immediate re-arm for zero-delay
  requests, a scheduled wake for timed ones) in the dialog loop.

## [0.1.0-beta.2] - 2026-06-28

### Added
- Empty-state screen: a centered "Press O to open…" panel over the letterbox
  background when no image is loaded (bare launch, or after the last photo is
  deleted), cleared the moment a photo is shown.
- View menu: **Slideshow Faster** (`[`) and **Slideshow Slower** (`]`) items.

### Fixed
- Dialog windows are clamped to the monitor, so a saved or newly-opened dialog
  can't land off-screen after a monitor/resolution change (multi-monitor aware).
- A toast that ended up jammed in the corner after a fullscreen toggle is now
  re-shown in the correct position.
- The slideshow interval is clamped to its `[min, max]` bounds, so the `[` / `]`
  keys can't drive it to zero or negative.

## [0.1.0-beta.1] - 2026-06-28

Initial beta — a fast, keyboard-driven Windows photo viewer.

### Added
- Fit-to-screen viewing, keyboard navigation, and hold-to-fly fast scrubbing.
- Scaling modes (fit / crop-to-fill / original 1:1), zoom, and rotation with a
  lossless EXIF-orientation save.
- Slideshow mode with an adjustable, persisted interval.
- Multi-codec decode (JPEG, PNG, GIF, BMP, TIFF, WebP, JXL, SVG, RAW; AVIF/HEIC
  via Windows codecs) with in-shader ICC color management plus wide-gamut and
  HDR output.
- EXIF info panel, copy-image-to-clipboard, and delete to Recycle Bin or
  permanent delete.
- ZIP and 7z archive viewing (RAM-only — never extracted to disk).
- Privacy guarantee: no persistent record of which photos were viewed.
- Signed WiX/MSI installer with file associations and an "Open with PhotoBlaze"
  folder verb.

[Unreleased]: https://github.com/jdlien/photoblaze/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jdlien/photoblaze/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/jdlien/photoblaze/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/jdlien/photoblaze/compare/v0.1.0-beta.4...v0.1.0
[0.1.0-beta.4]: https://github.com/jdlien/photoblaze/compare/v0.1.0-beta.3...v0.1.0-beta.4
[0.1.0-beta.3]: https://github.com/jdlien/photoblaze/compare/v0.1.0-beta.2...v0.1.0-beta.3
[0.1.0-beta.2]: https://github.com/jdlien/photoblaze/compare/v0.1.0-beta.1...v0.1.0-beta.2
[0.1.0-beta.1]: https://github.com/jdlien/photoblaze/releases/tag/v0.1.0-beta.1
