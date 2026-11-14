# Image Export — plan (task #89)

_Planning, 2026-11-13. Captures the owner design conversation of that date. Nothing is built
yet; every "today" claim below comes from a code survey on that date — re-check before
relying on one._

## Goal

Export the current image — or the current video frame — to a file. The MVP is PNG with
correct colour. The ambition, inspired by squoosh.app, is: a choice of codecs and quality, a
way to *judge* a lossy result against the original on the real viewing surface, HDR- and
wide-gamut-preserving output, and batch.

Squoosh is the inspiration, not the template. Two things it cannot do are the reason to build
this in Blaze rather than point people at Squoosh: the comparison happens in the real viewer
(GPU zoom, 1:1, instant flick), and it can show true HDR.

## How the directives apply

- **Prime Directive.** Export never touches the blaze path: decode and encode run off the
  event loop and never compete with display wants. Inside Export mode the interactions that
  must feel instant are *change a setting → see the result* and *flick original ↔ encoded*
  (a rebind, never a decode — the compare-mode rule).
- **Decode-to-purpose.** Export is a new purpose. It decodes at full precision (16-bit, float)
  and keeps the original ICC bytes. The display decode is untouched, so blazing pays nothing.
- **Privacy.** An export is an explicit user edit — the allowed category in ADR-018, never a
  passive write. Encoded previews are RAM-only and die with the process. Metadata is stripped
  by default, and location never stays switched on (see *Metadata*).
- **Cross-platform first.** Encoders, export state, option rules and the metadata allowlist
  live in the core. The shells render the panel and run the native save dialog.
- **Licensing.** The "we only decode" rule extends to "encoders must be permissively licensed."
  libwebp, libavif/aom, libjxl, mozjpeg and libultrahdr are BSD/Apache-style. **x265 is GPL
  and never ships**, so HEIC export is macOS-only, through Image I/O.

## What exists today (survey, 2026-11-13)

- **Copy is the nearest analogue.** `copy_image` (`app_core_impl/clipboard.rs`) decodes the
  original at full resolution and bakes the RAM-only rotation. It runs synchronously on the
  event loop, which is fine for Copy and not for export. `engine::to_clipboard_rgba8` passes
  wide-gamut `Rgba8` through *untagged* (a documented clipboard limit, but a colour bug in a
  file) and tone-maps HDR to SDR.
- **Encoders:** `pb_decode::encode_jpeg_rgba8` (the `image` crate, used by AI Describe) and
  `png` 0.17 in pb-app-core (dev tooling only).
- **Colour tooling:** moxcms 0.8.1 has `ColorProfile::encode()` and ready-made Display P3,
  BT.2020 PQ/HLG, Adobe RGB and ProPhoto profiles. Tagging any colour space needs no new
  dependency.
- **Metadata:** `little_exif` 0.6 writes (Save Rotation); `kamadak-exif` reads.
- **Video frames — broken on every platform.** `CoreEffect::CaptureNativeVideoFrame` and
  `NativeCapturePurpose` are defined, but nothing emits the effect and no shell handles it.
  `decode_item` on a video calls `decode_video_poster`, so **Copy on a video copies the poster,
  on Windows too**. #81's description ("emitted but dropped by the FFI … matching Windows") is
  stale and has been rewritten.
- **Compare (Y / ⇧Y):** `compare_pin: Option<usize>` is a playlist index whose texture is kept
  from ring eviction (one slot). It cannot hold a synthetic image such as an encoded preview.
- **Inspector:** the right-hand rich-panel slot, `InspectorTab { Details, Text, Describe }`.
- **Batch plumbing:** Quick Sort (#136) has `unique_name_in` (collision suffixes) and an
  off-thread file-operation worker. There is no multi-select model.
- **Layout:** the side panels are floating cards over the photo, and `pb_render::fit_rect` has
  no insets. That's fine — the owner's call is that zoom and pan handle the area under the panel.

## Colour and HDR fidelity

An export can only keep what the decoder kept.

| Source | What survives decoding today | Faithful export? |
|---|---|---|
| Wide-gamut SDR (P3 HEIC, Adobe RGB JPEG, ProPhoto TIFF) | Source-encoded 8-bit pixels plus a matrix + TRC reduction of the profile; the ICC bytes are read, then discarded | **Yes, cheaply.** Keep the bytes; write the pixels unchanged with the original profile. Bit-exact, and works even for LUT profiles our renderer can't show |
| HDR HEIC/AVIF (PQ/HLG, via WIC / Image I/O) | fp16 linear scRGB, BT.709 primaries extended range, **1.0 = 203 nits** (BT.2408) | **Yes.** Converting back to PQ/BT.2020 is exact at fp16 precision |
| HDR video frames (P010) | 10-bit PQ/HLG BT.2020 YUV | **Yes, the cleanest case.** YUV→RGB, keep the PQ code values |
| 16-bit PNG/TIFF | Narrowed to 8-bit (`image_backend.rs`, `to_rgba8()`) | Only via the export decode |
| HDR JPEG XL | Clamped to 8-bit (`jxl.rs` `to_rgba8`) | Only via the export decode (jxl-oxide already renders f32) |
| Gain-map HDR (iPhone HEIC, Ultra HDR JPEG) | No gain-map handling exists → the SDR base | **No** — needs gain-map decode first (task #145) |
| EXR / Radiance | Clamped to SDR (documented v1 limit) | No |

Linux has no HDR still decode, so Linux exports are SDR — consistent with Linux being
deliberately cheap.

### Output formats

| Format | Wide gamut | HDR | Notes |
|---|---|---|---|
| PNG | 8/16-bit + `iCCP` | 16-bit + `cICP` (PQ/HLG) + `cLLI` | HDR PNG reader support is still uneven |
| JPEG | 8-bit + ICC | Gain map (ISO 21496-1 / Ultra HDR) | The most compatible HDR: SDR everywhere, HDR where understood |
| WebP | ICC | — | HDR sources are tone-mapped, with a visible warning |
| AVIF | ICC or nclx, 10/12-bit | PQ/HLG nclx + `clli`; gain maps via libavif | Best all-rounder |
| JPEG XL | Yes | Native, including float | Best on paper; weakest reader support |
| HEIC | Yes | PQ or gain map | **macOS only** (Image I/O) |
| TIFF / EXR | 16-bit / float | Linear float | For interchange into editors |

Rules:

- **Brightness metadata is measured, not guessed.** MaxCLL/MaxFALL for a still are computed
  exactly from its pixels.
- **Never write an HDR format we can't read back.** The flick compare and the round-trip tests
  decode our own output, so each HDR output ships with its matching decode (`cICP` PNG, HDR
  JXL, gain maps).

## Metadata

Stripped by default. Three things are never user choices:

- **Colour profile** — always written. It defines what the pixels mean; it is not a privacy
  leak.
- **Orientation** — always reset to 1, because rotation is baked into the pixels. Copying the
  source tag would rotate the image twice.
- **Embedded thumbnails/previews** — always dropped. They go stale after a resize, and once
  crop export exists they can leak the uncropped original.

User choices, as a preset menu (None / Camera info / All / Custom…) with per-category
checkboxes under a disclosure:

| Category | Default | Notes |
|---|---|---|
| Camera & date | off | Capture date, camera, lens, exposure — keeps photo libraries sorting |
| Copyright & credit | off | IPTC/XMP creator and copyright |
| Location | off, **resets on every Export-mode entry** | Never sticky: a one-off "keep location" must not leak into later exports |
| Generation data | off | The AI prompt/workflow data #137 reads; ComfyUI workflows can carry local model/file names |
| Everything else | only in "All" | Manufacturer notes, serial numbers, software |

- **Per-photo warning:** when this photo has GPS and the chosen setting keeps it, the panel
  says so. Categories this photo doesn't contain are greyed out.
- **Allowlist, never denylist.** Build a fresh metadata block from the chosen fields. Never copy
  everything and delete GPS: location also lives in XMP, IPTC city fields, manufacturer notes
  and PNG text chunks.
- **Where the setting lives:** in the Export panel, remembered (Preview/Photos convention). A
  Settings entry is added only when a dialog-less path exists (quick export, drag-out, batch
  presets), and it shows the **same** stored value — one setting, two views.

## UI — Export mode

The owner's decision: **Export mode replaces the right-hand Inspector** (Details / Text /
Describe) with an *Export controls + details* panel. There is no modal except the final save
dialog. The image stays fully interactive; zoom and pan handle whatever the panel covers.

```
┌──────────────────────────────────────────────┬──────────────┐
│                                               │ Export       │
│                                               │ Format [AVIF]│
│        image — zoom / pan / Y flicks          │ Quality ──●─ │
│        original ↔ encoded                     │ Resize  [—]  │
│                                               │ Colour [Keep]│
│                                               │ HDR   [Keep] │
│   ┌───────────────────────────┐               │ Metadata None│
│   │ AVIF · q72 · 412 KB  (Y)  │               │ ⚠ has GPS    │
│   └───────────────────────────┘               │ 6000×4000 ·  │
│                                               │ 2.1 MB → −81%│
│                                               │[Cancel][Export…]
└──────────────────────────────────────────────┴──────────────┘
```

- **Entry:** `Action::Export`, File ▸ Export…, and a key (bare `E` is the tentative pick —
  #89's notes). On macOS the mode works like Photos' Edit mode: the image stays central, the
  controls take the right column, and you finish with Export… or Cancel.
- **The panel** shows format; quality or effort; resize; colour (Keep original / sRGB for web
  / Display P3); HDR (Keep / SDR fallback / Tone-map); metadata; and *details*: source
  dimensions, codec, size, colour space, HDR, GPS present. The live readout (rung 3) adds
  output size, % of original and encode time. Options a format can't do are greyed.
- **Keyboard:** `Esc` leaves Export mode (it does not quit). `Return` / `⌘S` runs Export….
  Space/Backspace move to the next/previous photo **keeping the settings**; encoding waits
  until you park, so blazing stays full speed. That makes "export these few with the same
  settings" work before batch exists.
- **Comparison — flick first, wipe later (owner call).** `Y` in Export mode flips between the
  original and the encoded result. Flick has no spatial cue, so a chip always says which one
  is showing ("Original" vs "AVIF · q72 · 412 KB"). Press-and-hold to see the original
  (Photos' convention) is worth considering. A wipe divider is a later nice-to-have.
- **The final step** is the native save dialog — an `NSSavePanel` sheet on macOS, `rfd` on
  Windows — for filename and folder only. It never defaults to overwriting the source.
- **Windows** builds the same panel as an egui panel from pb-ui components (the house rule).
- **Mac review:** run `/mac-arsed-mac-app` before building the macOS panel.

## The ladder

Each rung lands core-first with tests, then shell by shell (see *Handoff*).

### Rung 0 — groundwork (no new UI)

- **0a. Full-resolution video frame grab** → task #81 (rewritten). The frame at the current
  time at the original size, not the display-scaled image. macOS AVPlayer route: likely
  `AVAssetImageGenerator` at the current time with zero tolerance. For HDR, reuse whatever
  #144 lands — it was in progress in a parallel session on 2026-11-13, adding HDR posters for
  this route — rather than building a second HDR frame path. Session route (FFmpeg/MF): keep
  the last presented frame in RAM, or re-decode one frame at its PTS. Animations and Live
  Photos use the current frame. Ship it alone: it also fixes Copy, OCR, Describe and Compare
  on video.
- **0b. Export decode + pixel preparation.** A new decode purpose that keeps 16-bit and float
  and carries the original ICC bytes (the decoders already read them: `zune`
  `decode_rgba_with_icc`, `image_backend` `load_with_icc`, `jxl` `rendered_icc`). Then one
  preparation function: bake the rotation, keep alpha, apply the colour policy, tone-map HDR
  when the target is SDR. Move Copy onto it too, so Copy gains correct colour — the macOS
  pasteboard's TIFF can carry a profile.
- **0c. The `pb-encode` crate.** An encoder seam mirroring the decode seam, roughly
  `encode(&ExportImage, &EncodeOptions, &cancel) -> Result<Vec<u8>>`; pure, test-first, with
  round-trip tests through `pb-decode`. `encode_jpeg_rgba8` moves in as the first backend.
  Orchestration goes in a new `export.rs` subsystem with its `app_core_impl/export.rs` concern
  file in the same commit (the two-halves rule, `docs/where-code-goes.md`).

### Rung 1 — MVP: Export as PNG

- `Action::Export` → native save dialog → decode + encode on a worker that **navigation does
  not cancel** (the user asked for it) and that never competes with display wants. A toast
  confirms, with Reveal.
- Wide gamut kept by default (source pixels + source profile). HDR sources: a tone-mapped SDR
  PNG by default, with a 16-bit PQ PNG (`cICP` + `cLLI`) as the option — it's just maths, and
  the `png` crate writes any chunk.
- Metadata: none, apart from the colour tags.
- "Well-optimized": `png` with adaptive filtering, then an optional `oxipng` pass at a moderate
  preset (its top presets can take minutes). Pick the default from corpus numbers: encode-time
  p50/p95 and bytes per megapixel.
- Default filename `<stem>.png`; video frames `<stem>-0h12m34s.png`.
- A `CHANGELOG.md` entry.

### Rung 2 — the Export mode panel

- The panel and keyboard model above, replacing the Inspector while active. It shows the
  original only — no live encode yet.
- Formats: PNG and JPEG (pure Rust, already here) first.
- Quality, resize (longest edge / %), colour and HDR menus, greyed per format.
- The metadata allowlist writer and the preset/category UI.
- Settings remembered (format, quality, metadata preset — not location).

### Rung 3 — live preview + flick

- An encode worker: latest-wins; a new value cancels the in-flight encode; slider debounced.
- **The preview slot:** one extra resident texture in pb-render, kept from eviction like the
  compare pin. `Y` flips original ↔ preview as a rebind. A dimension-changing resize is shown
  at the original's display size so the flick lines up.
- Readouts: output size, % of original, encode time; optionally a perceptual score
  (`ssimulacra2`).
- **Never repeat likely work:** encodes cached in RAM keyed by (item, settings), within a
  budget, so dragging back to an earlier value is instant. When idle, pre-encode the
  neighbouring quality steps.
- **Only if measured necessary:** encode the visible region first (aligned to the codec's
  block size), then the whole image — if full-image encode p95 is too slow for a responsive
  slider on the corpus.
- `PB_PERF` episodes: *setting change → preview on screen* and *Y → photon*.

### Rung 4 — codec breadth

Each codec is independent and can land alone behind the seam: mozjpeg (noticeably smaller than
the `image` crate's JPEG), libwebp (the `image` crate's WebP is lossless-only), libavif + aom,
libjxl, and Image I/O HEIC on macOS. The risk is C build dependencies on three platforms
(vcpkg on Windows), not licensing. Platform encoders (Image I/O, WIC) sit behind the same seam
for A/B.

### Rung 5 — HDR export

- PQ AVIF (10/12-bit) and JPEG XL for archiving; gain-map JPEG/AVIF as the "share HDR"
  option; HEIC on macOS.
- Each output lands with its matching decode (the read-back rule).
- **Depends on task #145 (gain-map decode)** for the most common HDR photos. #145 is a viewer
  fix in its own right and should come earlier.
- In Export mode, add a "what SDR viewers will see" view for gain-map outputs.

### Rung 6 — batch

- A scope without multi-select first: "all items in this folder". A real multi-select model
  (thumbnail strip, keyboard marking — `mobile-vision.md`) is a later, separate design.
- Quick Sort's `unique_name_in` and worker; the existing determinate progress + Cancel views;
  a naming template (`{stem}`, `{n}`); a destination; an overwrite policy; "apply these
  settings to N items" from Export mode.
- Encoder threads capped so a batch never degrades browsing. Exporting archive entries comes
  for free.

### Later

The wipe divider; saved presets; a Settings → Export tab (once dialog-less paths exist);
dragging out of the window (`NSFilePromiseProvider`); "export what I see" (crop/zoom); fit
into the uncovered area (`fit_rect` insets); lossless JPEG → JPEG XL recompression (smaller,
and exactly reversible).

## Decisions

**Made (owner, 2026-11-13)**

- The feature is in. This resolves #89's original product-fit question.
- Metadata is stripped by default, with per-category choices in the Export panel. A Settings
  default comes later, once dialog-less export paths exist.
- Export mode replaces the right-hand Inspector with export controls + details. There is no
  modal except the final save dialog.
- The area under the panel is handled by zoom/pan; no fit-inset work is needed now.
- Flick (`Y`) first; wipe is a later nice-to-have.

**Recommended, not yet confirmed**

- Export the original pixels with rotation baked, not the on-screen crop/zoom.
- Wide gamut: keep the source pixels and profile by default.
- HDR default: "HDR with SDR fallback" (gain map) where the format supports it, PQ otherwise.
- The location toggle resets to off on every Export-mode entry.
- Bare `E` for Export.

**Open**

- OS save-dialog memory: Windows adds saved files to Recent Items unless
  `FOS_DONTADDTORECENT` is set (does `rfd` expose it?), and the macOS panel remembers its last
  folder. Acceptable for an explicit save? The Open dialog already falls in this category.
- Remember the last export folder ourselves? It's a path trace — an ADR-022-style owner call.

## Testing and measurement

- **Round trips:** lossless output decodes pixel-identical; a P3 export decodes to the same
  colour transform; a PQ export decodes within one code value at 12-bit; a 16-bit source
  round-trips at 16-bit.
- **Correctness:** rotation baked; orientation tag = 1; the metadata allowlist emits only the
  chosen fields (fixtures with GPS in EXIF, XMP and IPTC).
- **Privacy:** `viewing_a_folder_writes_nothing_to_disk` still passes; one export writes
  exactly one file at the chosen path; encoded previews never touch disk.
- **Perf:** corpus p50/p95/p99 for encode time per format and preset, bytes per megapixel,
  and the rung 3 `PB_PERF` episodes. Never means.

## Handoff

**Verified**

- Nothing built. The survey above was done by reading code on macOS, 2026-11-13.

**Not verified**

- "Copy on a video copies the poster on Windows" is inferred from the code
  (`decode_item` → `decode_video_poster`, no shell special-case) — confirm on Windows by
  pausing a film mid-scene, pressing Ctrl+C and pasting.

**Decisions / corrections**

- #81's description was stale (the capture effect is not emitted anywhere); rewritten as a
  cross-platform task.

**Cross-platform debt**

- None yet.

**Claimed**

- None.
