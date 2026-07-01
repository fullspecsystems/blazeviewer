# Changelog

All notable changes to PhotoBlaze are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions match the Git
tags / GitHub releases; the MSI ProductVersion is the numeric core (e.g. `0.1.0`),
with any pre-release suffix carried only by the tag.

## [Unreleased]

### Added
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

### Changed
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
