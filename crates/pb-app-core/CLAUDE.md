# pb-app-core — item kinds & doors (crate-local context)

Auto-loads when working in `crates/pb-app-core/`. Root `CLAUDE.md` has the
summary; archive *internals* live in `crates/pb-source/CLAUDE.md`. Maintained
here since 2026-07-19.

## Archives are "doors" in the deck (tasks.json #104, 2026-07-16)

An archive **on disk** is a typed item — `LibraryItemKind::Archive(ArchiveKind)`,
the third arm beside `Image`/`Video` — so a folder's `.zip`/`.7z`/`.cbz`/… are
*visible while browsing* instead of reachable only via Open File. It decodes to
the owner's **folder artwork** (`pb-app-core/assets/folder-zip-*.webp`,
`cfg(windows)` = manila / else blue, matching each OS's own folder colour;
decoded + composited onto an opaque backdrop **once** per process), and **`P`
enters it** (routed through `open_plan(Source::Archive)`, so it is the same
operation as the picker's open — password prompt, RAM pre-flight and progress
dialog all included). `Alt+Up` climbs back out to the folder (`open_parent_cmd`
anchors on `source.container()`), which is how you reach the next archive. Doors
also make a folder of *only* archives openable: it now yields scan items, where
before it hit the keep-deck rule and reported no images.

- **The affordance is the play-hint pill, not the tile** — `play_hint_kind` = `3` +
  `play_hint_persistent` (the pill reads *Open*, and unlike an animation's flash the shells
  hold it open, because a door's picture alone never says "press P"). The tile briefly carried
  a Font Awesome glyph instead: an icon drawn for 16 px stretched to the height of a 7680-wide
  display. Don't go back.
- **The guarantee:** `decode_item_cancellable` returns the tile **above** the `source.bytes()`
  request, so browsing past a door *never* decompresses. That — **not** the texture size — is
  why doors are safe where blending archive contents into the deck was not (the prefetch ring
  would have decompressed archives nobody clicked). Pinned by a panicking-`bytes()` source
  driven through **every** decode entry point. ⚠️ The tile's *size* has been argued from three
  times and been wrong three times; it only has to clear a comfort bar
  (`a_full_ring_of_doors_fits_the_byte_budget`).
- ⚠️ **A new `LibraryItemKind` must opt *out* of byte reads, not into them.** The tree encodes a
  two-kind world (video vs "everything else, therefore an image, therefore safe to read"). Guards
  written `!matches!(…, Video(_))` or `if let Video(_)` silently drop a new kind in the *image*
  bucket — which is how the thumbs strip and the `Shift+I` panel would each have `fs::read` every
  archive in a folder. Read guards are **positive** (`Image` reads bytes) and kind matches are
  **exhaustive** so the compiler lists the sites; note it only lists them *per platform* —
  `macos_native_route`/`macos_sample_buffer_route` are `cfg(macos)` and invisible on Windows.
- Doors are typed off the item's **path**, not its name (unlike video, deliberately): an archive
  entry has no path, so a `.zip` inside a `.zip` is unrepresentable rather than merely refused,
  and `archive_kind` gets `.tar.gz` right where a name-based split sees `gz`.
