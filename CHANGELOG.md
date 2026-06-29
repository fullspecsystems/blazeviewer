# Changelog

All notable changes to PhotoBlaze are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions match the Git
tags / GitHub releases; the MSI ProductVersion is the numeric core (e.g. `0.1.0`),
with any pre-release suffix carried only by the tag.

## [Unreleased]

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

[Unreleased]: https://github.com/jdlien/photoblaze/compare/v0.1.0-beta.3...HEAD
[0.1.0-beta.3]: https://github.com/jdlien/photoblaze/compare/v0.1.0-beta.2...v0.1.0-beta.3
[0.1.0-beta.2]: https://github.com/jdlien/photoblaze/compare/v0.1.0-beta.1...v0.1.0-beta.2
[0.1.0-beta.1]: https://github.com/jdlien/photoblaze/releases/tag/v0.1.0-beta.1
