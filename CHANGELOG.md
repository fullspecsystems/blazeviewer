# Changelog

All notable changes to Blaze Viewer are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions match the Git
tags / GitHub releases; the installer version is the numeric core (e.g. `0.1.0`),
with any pre-release suffix carried only by the tag.

## [Unreleased]

### Added
- **Subtitles on videos.** Press `C` to turn captions on and off; your choice is remembered.
  Blaze Viewer reads both the subtitle tracks stored **inside** an MKV or MP4 and a subtitle file sitting **beside** the video (the usual `Movie.eng.srt` or
  `Movie.vtt`) — SubRip (`.srt`), WebVTT (`.vtt`), ASS/SSA, and MP4's own timed text, all
  including non-Latin scripts and right-to-left languages. Text is rendered sharply at your
  display's real resolution, with a black outline so it stays readable over bright scenes.
  `C` prefers a forced/signs track matching the audio, then the file's own default track,
  then whatever it can read. (Image-based subtitles — PGS, VobSub — are not text and are
  still not shown.)
- **Pick a subtitle track from a list** (macOS): a button on the playback bar, just right of
  the running time, opens every track the file carries — named by language and format
  ("English · SubRip · Forced"), with a tick on the one you're watching. The same list is in
  the new **Playback ▸ Subtitle Track** menu, and `Shift+C` steps through the very same
  tracks if you'd rather not reach for the mouse. Turning subtitles off and on again brings
  back the track *you* chose, rather than reverting to the app's guess.
- **Your subtitle language follows you to the next film.** Choose "Arabic (SDH)" on one
  episode and the next one starts on its Arabic track — even though the two files number
  their tracks differently, and even if one says `ara` where the other says `ar`. If a film
  hasn't got that language, it falls back to its usual choice instead of showing nothing.
- **Choose the audio track** (macOS): **Playback ▸ Audio** lists every track a film carries —
  the director's commentary, the second language, the stereo mix beside the 5.1 — with a tick
  on the one you're hearing, or press **`A`** to step through them (**`Shift+A`** goes back).
  It changes the sound without interrupting the picture. Blaze Viewer only ever says a track
  changed once it actually has, so the message can be trusted.
- **A new Playback menu** (macOS) gathers the things that only apply to something playing —
  Play/Pause, frame stepping, audio and subtitle tracks, and Live Photo audio — instead of
  leaving them scattered through View and Image.
- **Garbled subtitles repair themselves.** Subtitle text that arrives mangled — `â™ª`
  instead of `♪`, `Iâ€™m` instead of `I'm` — is a mis-encoding that has been quietly
  breaking subtitles everywhere for decades. Blaze Viewer now detects and undoes it, and
  only ever when it can prove the text really was mangled, so correctly-written text
  (`café`, `señor`) is never touched.
- **Subtitle appearance is yours to set** (macOS): a new **Settings ▸ Subtitles** tab with
  font, size, colour and opacity, outline, drop shadow, background, and vertical position —
  including **below the picture, down in the black letterbox bar**, which almost no player
  lets you do. Outline, shadow, and background scale with the text, so changing the size
  keeps everything in proportion. A live preview shows exactly what you'll get, drawn by the
  same renderer that draws the real thing, so it can't mislead you. Your choices are
  remembered.

### Fixed
- **The scrubber knob no longer drifts out from under your cursor** (macOS). Clicking and
  holding on the playback bar could make the knob slide away and the video seek on its own,
  because the elapsed-time label changed width (e.g. crossing the one-hour mark) and nudged
  the whole bar sideways under the held pointer. The time now keeps a fixed width, so the bar
  stays put.
- **Seeking no longer pauses the video** (macOS). Clicking or dragging the scrubber on an
  MKV/WebM would stop playback dead — a click fires two internal seeks in a millisecond, and
  the second one mistook the first's momentary clock-hold for "the user paused." Playback now
  keeps whatever play/pause state it had across a seek, and the sound comes back with it.
- **Seeking forward in an MKV now actually goes forward.** Pressing `→` jumped the film
  *back* to the nearest 5-second mark instead of advancing 2 seconds; seeking backward was
  unaffected, which is why it looked so strange.
- **Resumed MKV/WebM videos keep their sound in step** (macOS). Reopening a video where you
  left off moved the picture to your spot but left the audio playing from the start, so it
  was silent or lagging until the sound caught up. Both now resume together.
- **The scrubber no longer flashes back after a click-seek** (macOS). Clicking ahead on the
  playback bar jumped to the spot, snapped back to where you were, then landed on the spot
  again; the knob now holds the position you asked for until the video actually gets there.
- **Rapid scrubbing lands where you let go** (macOS). Dragging quickly could let an earlier
  position win over your final one; only the last seek now takes effect.

## [0.2.1] - 2026-07-14

### Highlights

**PhotoBlaze is now Blaze Viewer** — new name, new home at blazeviewer.app. The feature
this app exists for, holding a key to race through your images, is now called **Blaze**.

⚠️ **Install this one, then remove the old PhotoBlaze.** Because the name changed, your
computer sees Blaze Viewer as a different program: it installs *alongside* PhotoBlaze
rather than replacing it, and PhotoBlaze will never update itself into it. Your settings
and shortcuts don't carry over, so any custom keys need setting once more.

Also in this release: **audio and subtitle tracks listed properly** in the Details panel
instead of a bare "Audio: Yes", **MKV and WebM playing through the system decoder on macOS**
with correct Dolby Vision, and **video posters appearing 2–4× faster**.

### Changed
- **PhotoBlaze is now Blaze Viewer.** The app, its website (blazeviewer.app), and everything
  in it have been renamed. The fast-flick feature this app exists for is now called
  **Blaze** — holding a key to race through photos is "Hold to Blaze".
  > ⚠️ **This version installs alongside the old one rather than replacing it.** The rename
  > changes the app's identity, so your computer treats Blaze Viewer as a brand-new
  > program: PhotoBlaze will not auto-update into it. **Uninstall PhotoBlaze first**
  > (Windows: Settings ▸ Apps — this also cleans up its "Open with" entries, which is why
  > deleting the folder by hand isn't enough; macOS: drag it to the Trash; Linux: delete
  > the old AppImage), *then* install Blaze Viewer. Your settings and keyboard shortcuts
  > are not carried over — they live in a new folder — so you'll need to redo any
  > customised shortcuts once.
- **About now links to blazeviewer.app** instead of the source repository, which is private.

### Added
- **Videos remember where you left off — for the current session.** Switch away from a video and
  come back (for example, press Space expecting pause — which actually advances to the next item —
  then Backspace) and playback resumes near where you were instead of restarting. Positions are kept
  only in memory and never written to disk; they're forgotten when you quit. A glance at the first or
  last few seconds isn't remembered (it just starts over), and a video you watched to the end restarts
  from the top. Works for every video and format on all platforms.
- **Opening photos from Explorer now reuses the running window (Windows).** Double-clicking a
  photo, or selecting several and pressing Open, hands them to the Blaze Viewer you already have open
  instead of launching a separate copy for each file. A multi-selection opens as one playlist, and
  the existing window comes to the front.
- **About now credits the open-source libraries Blaze Viewer builds on** — libheif, libde265, dav1d
  and FFmpeg — with each one's copyright and license. Their full license texts now ship alongside
  the app (in the `licenses` folder next to it on Windows and Linux, or inside the app bundle on
  macOS).

### Changed
- **MKV/WebM videos play through the system decoder on macOS, with correct Dolby Vision.** Videos in
  containers macOS can't open on its own now hand the compressed video to the system (VideoToolbox)
  decoder — the same engine the built-in player uses — so Dolby Vision and HDR render the way the film
  was mastered, at lower CPU cost, instead of the app converting color itself. Audio and seeking run
  on one shared clock for tight A/V sync. Anything the system can't decode from that path falls back
  automatically to the previous player; set `PB_NO_SAMPLE_BUFFER=1` to force the old path.
- **Keyboard zoom and pan ease in more gently.** Holding the zoom/pan keys now starts slower and
  builds up along a smooth curve instead of ramping linearly, so small taps make fine adjustments
  (handy for nudging a letterboxed film to fill the screen) while a longer hold still moves quickly.
  The top speed is also a touch lower.
- **Video posters appear much faster (macOS/Linux).** The still frame shown for a video (before
  you press play) is generated ~2–4× quicker: it now uses hardware decode and finds the first
  non-black frame at a small scale, converting only the chosen frame at full resolution instead of
  every candidate at 4K. On a 4K HDR film that's ~1.5s → ~0.6s of decode work — so a video opened as
  the first item stops sitting blank for several seconds.
- **HDR and 10-bit video now decode with more headroom (macOS/Linux).** The per-frame color
  conversion for 4K HDR (Dolby Vision / HDR10 / HLG) and 10-bit video moved onto the GPU, so the
  CPU does far less work per frame — smoother playback with margin to spare, especially on demanding
  files. Colors are unchanged (verified against a from-spec reference); rotated and unusual-format
  clips keep the previous path automatically. (Set `PB_VIDEO_NO_PLANAR=1` to opt out.)
- **Video details now list every audio and subtitle track, instead of just "Audio: Yes".** The
  Inspector's Details tab (`Shift+I`) shows each track on its own line — language, codec, channel
  layout, sample rate, and whether it's the default, forced, a commentary, or SDH — so a film with a
  director's commentary and three subtitle tracks reads as exactly that. Image-based subtitle formats
  (like PGS or VobSub) are still listed, marked "Unsupported". Details
  that genuinely can't be read say so, rather than claiming a file has no audio. Copy all details
  (`Shift+I` → copy) includes the tracks; if you copy while a video is still being read, the copy
  waits and gives you the complete set rather than a half-filled one. Reading a video's details never
  stalls the viewer — it happens in the background, so a damaged file or one on a slow network share
  can't freeze the window; the panel says "Reading video details…" until it's ready. A video inside a
  ZIP or 7z now reports exactly what the same file reports loose, and Windows, macOS and Linux all
  read the same file the same way.
- The info line (`i`) now defaults to the **bottom-center** of the screen instead of the
  bottom-right. (Existing users keep whatever position they've already set; this only affects
  fresh installs.)
- **Videos with several audio tracks pick a track deliberately.** A file that carries more
  than one audio track (say Dolby TrueHD plus a stereo mix) now honors the one the container
  marks as default or forced, instead of a blind guess.

### Fixed
- **A video's left/right arrows now seek reliably instead of occasionally doing nothing.** The
  arrows seek a playing video only when it isn't zoomed in enough to pan sideways. A rounding-level
  overflow — a video that fills the width, or an imperceptible zoom you didn't notice — could leave
  a sub-pixel sliver that flipped the arrows into an invisible pan, so they appeared dead even in
  Fit mode. Overflow smaller than a pixel is now treated as "fits," so the arrows seek.
- **Resizing the window, switching scale mode, or opening a folder no longer freezes on HEIC
  libraries.** These used to re-decode the current photo on the spot, stalling the app for a
  moment (about a quarter second on a 12-megapixel HEIC, longer on RAW). The current frame now
  stays on screen and rescales instantly while the sharper version is prepared in the
  background, so the app never hangs.
- **Seeking a video on a network share no longer kills the audio.** Jumping into a large
  movie stored on a NAS/SMB share used to make the audio cut out for the rest of playback
  (and stutter for a while). Two causes: the audio track was sought the slow way — a byte-by-byte
  scan through the file because the seek index only covers the video track — which took ~73
  seconds over the network on a 16 GB 4K film and left the audio decoder wedged; and on films
  with widely-spaced keyframes a large jump (especially backward) made the decoder briefly read
  more than expected while realigning, which was mistaken for a corrupt stream and cut the audio.
  Audio now seeks through the video index like the picture does (tens of milliseconds), realigns
  without giving up, and lands right on target, so sound stays in sync after a jump either way.
- **Demanding video audio no longer competes with the interface (macOS).** The audio for
  session-backed videos (MKV/WebM and other FFmpeg-decoded formats) is now decoded on its own
  background thread instead of the main UI thread, so heavy tracks (Dolby TrueHD/Atmos, large
  surround mixes) and rapid seeks stay smooth and don't hitch the window.
- **A corrupted audio tail is no longer mistaken for the clean end of a video.** A decode error
  partway through the audio now surfaces as an error rather than silently "ending" playback as if
  the stream had finished.
- **Holding a seek key no longer crackles or restarts the audio for every hop.** While
  you hold an arrow (or drag the playback bar), audio now pauses once, the video scrubs
  freely, and when you let go the audio rejoins in sync at the final position with a
  single clean resume — instead of tearing down and refilling the audio stream for every
  intermediate ±2 s step.
- **Video recovers instantly after falling behind.** When decoding briefly couldn't keep
  up, playback used to show every late frame one screen-refresh apart — a slow-motion
  crawl until it caught up. It now skips straight to the newest frame that should be on
  screen and stays in step with the audio.
- **A brief video hiccup no longer stutters the audio.** A momentary decode spike (a
  heavy scene, a GOP boundary) used to pause the audio the instant the next frame was
  late and resume it a beat later — an audible stop/start. Playback now rides out short
  video stalls (up to ~300 ms) by holding the current frame while audio continues
  uninterrupted; only a genuine sustained stall pauses for a real rebuffer.
- **4K HDR video can no longer get stuck "buffering" forever.** The decoded-frame
  memory budget could never hold the two HDR frames playback insisted on before starting
  (each 4K HDR frame is ~63 MiB against a ~95 MiB budget), so some window/scale modes
  never left the buffering state. The budget is now sized for HDR frames, and playback
  starts from however many frames actually fit rather than waiting for an impossible fill.
- **Short forward jumps in video are near-instant.** An arrow-key ±2 s skip forward now decodes
  ahead from where you are instead of seeking back to the previous keyframe and grinding the whole
  GOP. (Backward and long jumps still seek to a keyframe.)
- **HDR video plays more smoothly in large windows.** The per-frame HDR color conversion now runs
  across CPU cores (leaving headroom for audio), so it keeps up at higher resolutions instead of
  dropping frames. (A GPU-based conversion is the planned long-term replacement.)
- **Seeking in high-bitrate 4K / HDR videos is dramatically faster.** Jumping around a long-GOP
  clip used to stall for 1–3 seconds while it needlessly color-converted (and HDR-tone-mapped)
  every frame between the keyframe and your target — frames it only threw away. It now decodes
  those frames but converts only the one it lands on, cutting the wait roughly 7–10× (e.g. a 3.1 s
  seek down to ~0.5 s). macOS MKV/WebM and all Linux video.
- **Video poster frames for feature films.** Long films that open on a studio logo over black
  or a slow fade-in used to get an all-black thumbnail. The poster picker now seeks past the
  intro and chooses the most visually detailed frame it finds, so you get a real picture.
  (Mac MKV/WebM and all Linux video for now; MP4/MOV on Mac and Windows to follow.)

## [0.2.0] - 2026-07-13

### Highlights
- **Video playback** — clips (and videos inside ZIP/7z archives) with sound, seeking, and frame-stepping, on every platform.
- **Thumbnails panel** (`Shift+T`) — a scrollable strip to glance through and jump around your photos.
- **Undo a delete** (`Ctrl+Z` / `⌘Z`) — get a photo back if you trash it by mistake.
- **A mouse toolbar** (Windows/Linux) and **live Windows accent-color** matching.
- **Command-line launching** — open a folder, start a slideshow, shuffle, and more.
- **Faster, smoother** Live Photos, animation, and 4K video (now GPU-accelerated).
- Plus animated AVIF playback, Linux self-updates, and a batch of fixes.

### Added
- **Video playback.** Camera clips (MP4, MOV, MKV, WebM, AVI, and more) appear alongside your
  photos with poster frames and details (duration, codec, frame rate, audio). `P` plays with
  sound; arrow keys seek (`Shift` for ±10s, hold to scrub); `,` / `.` step frames. The info line
  grows an interactive scrub bar with a play/pause button. Plays any length at constant memory and
  leaves no trace on disk. Videos inside ZIP/7z archives play too (entries over 1 GiB are skipped).
  Windows and Linux (Linux is beta); a codec Windows lacks shows a dark tile, not an error.
- **Video playback on macOS, hardware-accelerated.** Clips play through the system player
  (AVPlayer/VideoToolbox) with a bundled FFmpeg fallback, so nearly every codec plays — with all
  the same controls, posters, and archive support.
- **Undo a delete — `Ctrl+Z` (`⌘Z` on macOS).** Restore a photo you sent to the Trash / Recycle Bin
  (also Edit ▸ Undo Delete); it comes back in place and PhotoBlaze jumps to it, even if it was the
  last photo in the folder. All platforms. A permanent delete (`Shift+Del`, or `⌥⌘⌫`) can't be undone.
- **Thumbnails panel — `Shift+T`.** A scrollable strip in the left pane (Folders / Thumbnails tabs)
  shows previews around your position with badges for videos, Live Photos, and animations. Click to
  jump — a far jump shows the thumbnail instantly, so there's no black flash. It follows as you
  navigate, reflects your rotations, and is resizable. Memory-only — no thumbnail files written. All
  platforms.
- **A toolbar for mouse control (Windows/Linux).** A button row under the menu in windowed mode:
  previous/next and random (hold to fly), folders, play/pause, slideshow, rotate, delete, panel
  toggles, a photo counter, and fullscreen. Toggles fill with your accent color. On by default —
  hide it from **View ▸ Show Toolbar**. (macOS has its own native toolbar.)
- **PhotoBlaze follows your Windows accent color.** Buttons, tabs, and selection match the accent
  you pick in Windows Settings and update live (Settings ▸ Appearance ▸ Accent: **System**,
  **Custom**, or **Blaze Orange**). A too-faint custom color is nudged to stay visible and button
  text stays readable. (Linux and macOS keep Blaze Orange for now.)
- **Command-line options.** `photoblaze --help` lists them all. Open files, a folder, or a
  `.zip` / `.7z`, and shape a single launch without touching your saved settings:
  `--slideshow[=SECS]`, `--shuffle` / `--reverse`, `--scale`, `--theme`, `--windowed` /
  `--fullscreen`, `--recursive`, `--start-at N|NAME`, `--mute`, and more. On macOS, install the
  `photoblaze` command once from **PhotoBlaze ▸ Install Command-Line Tool…**.
- **Animated AVIF plays (Windows and Linux).** `avis` image sequences show the play hint and loop
  like a GIF with correct timing and wide-gamut color. (HDR animated AVIF stays on the full-HDR
  still path; animated HEIC stays first-frame.)
- **Linux updates itself.** The AppImage checks in the background, downloads and verifies a new
  version, and swaps itself in on quit — the same hands-off updating Windows and macOS have. Update
  checks send only your version and CPU architecture, never anything about the photos you view.
- **Photoshop files without a flattened preview show something instead of nothing.** A `.psd` saved
  without "Maximize Compatibility" used to open blank; it now falls back to the embedded preview
  thumbnail. (macOS renders CMYK and other PSDs at full resolution via the system decoder.)
- **The About dialog shows the build's CPU architecture** (e.g. `Build 1ad1043 · ARM64`), so you
  can tell the native Windows ARM64 build from the x64 one at a glance.

### Changed
- **`Del` / `⌘⌫` no longer silently loses a photo when the Recycle Bin / Trash is unavailable.** On
  a Windows drive set to skip the Recycle Bin, or a macOS volume with no Trash (read-only or
  network), it now asks you to confirm a permanent delete — the same prompt as `Shift+Del` / `⌥⌘⌫` —
  instead of quietly deleting (Windows) or just failing (macOS).
- **Edit ▸ Undo names what it will undo** — "Undo Delete a.jpg" / "Undo Rotate a.jpg" (long names
  shortened) instead of a bare "Undo". All platforms.
- **Settings save as you change them (Windows/Linux).** Each change applies live and sticks — no
  Save/Cancel, just Done to close (as macOS already worked). "Reset settings" still restores defaults.
- **4K video plays on your graphics card.** Clips above 4K30 decode and color-convert on the GPU
  (~3× the 4K60 HEVC headroom, faster start); lighter clips keep the software path (also the
  fallback without a capable GPU). HDR (HLG/Dolby Vision) stays on software so brightness is correct.
- **Live Photos start almost instantly, every platform.** `P` begins as soon as the first frames
  are ready and extends while the rest decodes — a ~1–2s wait on Windows drops to a fraction, and P3
  color stays correct. On a slow machine it may briefly pause, then resume.
- **Smoother animation and Live Photo playback** — frames reuse one resident texture and upload
  buffer instead of allocating per frame (95th-percentile present overhead down ~3× at 1080p).
- **`F` is the full screen shortcut everywhere** (View menu and exit hint) instead of `F11`, which
  stays bound as a secondary. A keymap that had dropped `F` is healed on launch.
- **Renamed "Show All EXIF Info" to "Show Detailed Info"** (View menu), matching the panel it opens.

### Fixed
- **Undo no longer forgets an earlier edit after you delete a photo.** Saving a rotation and then
  deleting a photo used to silently drop the rotation from undo history. Undo now keeps every action
  — rotations and deletes, in any mix — until you open a different folder. All platforms.
- **Icons are crisp at 125%/150% display scaling** (and over Remote Desktop) — no longer blurred or
  flattened at the bottom.
- **Older camera clips play with sound on Windows.** Motion JPEG AVIs from 2000s point-and-shoots
  (Canon, Fujifilm `.AVI`) played silently; audio now goes through the same media layer as the
  picture. Modern MP4/MOV are unaffected.
- **Dropping files onto the window focuses PhotoBlaze** — the arrows and Space work immediately,
  without a click.
- **The folder tree (`Shift+F`) shows for ZIP and 7z archives** — it now falls back to the archive's
  own folder tree, and clicking a folder re-scopes the view.
- **ZIP/7z archives open on machines with less RAM** — the over-conservative memory guard no longer
  refuses every archive (even a tiny one) on an ~8 GB machine.
- **The About and Settings dialogs open on VMs / GPUs without a low-power Direct3D adapter**
  (e.g. Parallels on Apple Silicon) — they now use the viewer's Direct3D backend instead of failing
  to appear.
- **The Keyboard Shortcuts panel (`/` or `?`) no longer clips its last rows** on short windows.
- **The play hint (`▶ Play` / Live Photo pill) fades out cleanly** — background, border, and keycap
  fade with the label — and on Linux it no longer stays stuck until you move the mouse.

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

[Unreleased]: https://github.com/fullspecsystems/blazeviewer/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/fullspecsystems/blazeviewer/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/fullspecsystems/blazeviewer/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/fullspecsystems/blazeviewer/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/fullspecsystems/blazeviewer/compare/v0.1.0-beta.4...v0.1.0
[0.1.0-beta.4]: https://github.com/fullspecsystems/blazeviewer/compare/v0.1.0-beta.3...v0.1.0-beta.4
[0.1.0-beta.3]: https://github.com/fullspecsystems/blazeviewer/compare/v0.1.0-beta.2...v0.1.0-beta.3
[0.1.0-beta.2]: https://github.com/fullspecsystems/blazeviewer/compare/v0.1.0-beta.1...v0.1.0-beta.2
[0.1.0-beta.1]: https://github.com/fullspecsystems/blazeviewer/releases/tag/v0.1.0-beta.1
