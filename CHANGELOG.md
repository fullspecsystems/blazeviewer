# Changelog

All notable changes to PhotoBlaze are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions match the Git
tags / GitHub releases; the MSI ProductVersion is the numeric core (e.g. `0.1.0`),
with any pre-release suffix carried only by the tag.

## [Unreleased]

### Added
- **AI image descriptions (D) + Ask about an image (⇧D)** — describe the current
  photo with a vision model, entirely on hardware you control. Press **D** for a
  description in an on-image panel (press **D** again to retry if it failed), or **⇧D**
  to type your own multi-line question — *"What products are visible?"*, *"What year
  does this look like?"* — and get an answer in the same panel. **Edit ▸ Copy AI
  Description** (and the right-click menu) puts the text on the clipboard. Configure it
  under **Settings ▸ AI**: point it at any OpenAI-compatible local model server (LM
  Studio, Ollama, llama.cpp — on this machine or your own network), pick a response
  length, and customize the prompt. **Test & list models** confirms the server is
  reachable and fills a **model picker** with what it's serving — vision-capable models
  listed first — so you choose from a list instead of typing an exact model name. Optional **auto-describe** describes each
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

[Unreleased]: https://github.com/jdlien/photoblaze/compare/v0.1.0-beta.4...HEAD
[0.1.0-beta.4]: https://github.com/jdlien/photoblaze/compare/v0.1.0-beta.3...v0.1.0-beta.4
[0.1.0-beta.3]: https://github.com/jdlien/photoblaze/compare/v0.1.0-beta.2...v0.1.0-beta.3
[0.1.0-beta.2]: https://github.com/jdlien/photoblaze/compare/v0.1.0-beta.1...v0.1.0-beta.2
[0.1.0-beta.1]: https://github.com/jdlien/photoblaze/releases/tag/v0.1.0-beta.1
