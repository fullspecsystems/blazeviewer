# pb-decode — formats, backends, color, video decode (crate-local context)

Auto-loads when working in `crates/pb-decode/`. The root `CLAUDE.md` carries the
summary; this file is the detail. Sections below were maintained in the root doc
until 2026-07-19 — keep them current here.

## The provisional library-picks table (historical reference)

The original research starting points (`.taskmaster/docs/`), each provisional and
benchmark-justified — the A/B seams exist so any can be replaced with data. The
"actually wired" section below records where reality diverged.

| Concern | Primary | A/B alternative / notes |
|---|---|---|
| JPEG | `turbojpeg` (libjpeg-turbo, **native scaled decode**) | `zune-jpeg` (pure Rust, SIMD; pair with `fast_image_resize`) |
| PNG/APNG | `png` (image-rs) + `zlib-rs` backend (pure Rust, fastest now) | — (no scaled decode exists for PNG) |
| WebP | `libwebp-sys` (`use_scaling` = true downscale-on-decode) | `image-webp` (pure Rust, no SIMD/scaling) |
| AVIF | stills: Windows WIC / macOS ImageIO; **animated (`avis`): vcpkg dav1d + own demuxer** (task #76) | Linux: FFmpeg (`livephoto`) for animated |
| HEIC | `libheif-rs` | ⚠ **highest** Windows build risk — pin vcpkg ports or ship DLLs |
| JXL | `jxl-oxide` (pure Rust) | `jpegxl-rs` only if native DC downscale needed |
| TIFF / BMP / QOI | `tiff` / `image` / `qoi` (all pure Rust) | — |
| SVG | `resvg`/`usvg` → `tiny-skia` pixmap → texture | rasterize at on-screen res; watch `vello_hybrid` for live-zoom |
| RAW | `kamadak-exif` → extract embedded JPEG preview → JPEG path | full demosaic deferred (100×+ cost); `rawler` optional (LGPL) |
| Color | `moxcms` (pure Rust; 3×3 matrix + TRC in-shader) | `lcms2` behind a flag for exotic CLUT/CMYK profiles |
| GPU decode | **CPU decode pool** — measured 2.5× @ 120 Hz | nvImageCodec/CUDA zero-copy is a gated escalation (ADR-012 kill criterion); benchmark 5090 HW-JPEG first |

### Decode-to-fit value ranking
JPEG ≫ WebP > JXL(C) ≫ everything else. Prioritize the scaled-decode path where
it pays.

## What's actually wired — deviations from the provisional table

Multi-codec dispatch is implemented behind the `ImageDecoder` seam
(`decode_bytes` sniff-registry + extension routing for the ambiguous ones). The
pragmatic crate choices differ from the table above and are the current baseline:

- **`image` crate** (one dep, `default-features` curated) covers PNG/GIF/BMP/TIFF/
  **WebP**/TGA/QOI/ICO/PNM/HDR/EXR — chosen over the per-format crates (libwebp-sys,
  png+zlib-rs, …) for zero build risk; swap individual formats back behind the seam if
  benchmarks justify it.
- **JXL** `jxl-oxide`; **SVG** `resvg/usvg/tiny-skia` (rasterized at display res).
- **RAW** (ARW/NEF/CR2/DNG/…, `raw.rs`): hybrid. If the embedded preview is full-size
  (long edge ≥ `PREVIEW_FULL_MIN`=4000, e.g. Nikon's ≈7360) use it (fast); else **demosaic
  to true sensor res** via `rawloader` + `imagepipe` (e.g. Sony's 1616 thumbnail → 6048).
  Demosaic runs on a **256 MB-stack thread** — some RAW decoders recurse deep enough to
  overflow the default stack (a Nikon D800 NEF does; it's why the preview path exists for
  full-preview cameras). Slower than preview-only; "preview-first then refine" is the
  future optimization. JPEG-segment-parse finds embedded previews (`jpeg_spans`).
- **AVIF + HEIC stills**: **Windows WIC** (`wic.rs`, `cfg(windows)`, `windows` crate) using the
  OS codec extensions — first platform-specific decode backend (macOS mirrors with ImageIO).
  Needs the AV1/HEVC/HEIF Store extensions; absent → graceful decode error. **JPEG-2000 not
  added** (no pure-Rust decoder; OpenJPEG/C, rare).
- **Animated AVIF (`avis`) on Windows** (task #76): the vcpkg static **dav1d** behind
  `--features dav1d` (ship config, like libheif; same pinned tree via `setup-libheif.ps1`).
  WIC exposes only frame 0 and MF can't demux `.avif`, so `avis.rs` demuxes the
  sample tables itself (pure Rust, fuzzed) and feeds dav1d through a **C accessor shim**
  (`csrc/dav1d_shim.c`, compiled by build.rs against the pinned headers — dav1d structs never
  cross the FFI by hand). `probe_avis` is the shared detect/decode decision: avis-only (msf1 =
  HEVC stays still), HDR → the fp16 WIC still path, fragmented/encrypted/stz2 → still; no dead
  play hints. YUV→RGB in `yuv.rs` (identity/601/709/2020, limited+full, 8/10/12-bit); the trak
  `colr` rides as the display `ColorTransform`. Cancellable via `decode_animation_cancellable`
  (the whole animation path now checks the flag). Loops like a GIF; corpus 26-frame 1280×531 ≈
  139 ms release decode.
- **Orientation** (subtle, was buggy): `common::read_orientation` scans **all** EXIF IFDs
  (HEIC/RAW put Orientation outside the primary IFD; a PRIMARY-only lookup wrongly read 1).
  WIC **already applies** the container rotation, so the WIC path passes orientation=1 (re-
  applying double-rotated). imagepipe also self-orients (demosaic path → 1). The RAW
  *preview* path applies the container's orientation to the sensor-order preview.
- Every backend returns full-res RGBA8 → shared `common::finalize`/`finalize_oriented`
  (orientation + Lanczos decode-to-fit). Native scaled-decode (JPEG DCT, WebP) still a TODO.
- **Panic-safety**: `decode_bytes`/`decode_image_file` wrap the decoders in `catch_panics`
  (`catch_unwind`) — a third-party decoder panicking on a hostile file becomes a
  `DecodeError`, not an app crash. **Release profile is `panic = "unwind"`** (changed from
  abort) so this works; the GPU present hot path never panics, so unwind tables don't cost
  it. A hard *stack overflow* (some NEFs) is still uncatchable — mitigated by the demosaic
  big-stack thread, not `catch_unwind`.
- **TGA is routed by extension** (`.tga`), not content — Targa has no magic number, so
  `image::guess_format` can't sniff it; `decode_image_file` hands it an explicit format hint.

## Color management + wide-gamut + HDR (tasks.json #11) — three layers

1. **In-shader ICC CMS.** `pb_decode::color` parses the source profile (`moxcms`) to a
   3×3 (source primaries → BT.709) + a 7-param TRC, carried on `DecodedImage::color`.
   Read per backend: JPEG APP2 (zune `icc_profile`), PNG/TIFF/WebP (`image` `icc_profile`),
   JXL `rendered_icc`, and — because the Windows HEIF decoder returns **no** WIC color
   contexts — the ISOBMFF `colr` box parsed directly (`prof`/`rICC` ICC or `nclx` CICP) in
   `wic.rs`. sRGB / ~2.2-gamma-sRGB-primaries → passthrough. Fixes oversaturated P3 HEICs.
2. **fp16 scRGB render path.** `pb-render` renders to an `Rgba16Float` scRGB-linear
   intermediate (no gamut clamp), then a present pass → the surface: SDR 8-bit gets an
   extended-Reinhard tone-map (per-image `peak`) + sRGB-encode; HDR fp16 copies straight.
3. **Wide-gamut + HDR output (pure wgpu, no native D3D12).** A DXGI **fp16 flip swapchain
   is always scRGB**, so `pb_render::display::primary_hdr()` (DXGI `GetDesc1`) detects an
   HDR desktop and configures an `Rgba16Float` surface. HDR AVIF/HEIC (PQ/HLG) decode to
   fp16 scene-linear via WIC `128bppRGBAFloat` (`PixelFormat::Rgba16F`); brightness is baked
   in the scene pass (SDR×SDR-white-scale, HDR×1.0 absolute). P3 shows wider and HDR gets
   real headroom on a capable panel.

## Video playback (tier 2, task #79 — Windows shipped; Linux/macOS = parity work)

Filesystem videos (one cross-platform container recognition list; per-file playability is
a runtime property of the OS codecs) are **typed items** — `LibraryItemKind`, dispatched
in `decode_item` *before* any `bytes()` read; path-only, never indexed inside archives;
Live-Photo companion `.mov`s are hidden by same-stem-per-directory dedup at scan.
Poster = the clip's first non-black frame via an MF reader configured identically to
playback (rotation/color parity by construction); panel facts come from a ~20 ms
header-only probe. Playback = a forward-only `VideoSession` (pb-app-core, injected-time,
fully unit-tested on fake producers) fed by a demand-driven MF reader thread (pb-decode):
one credit = one frame over a **single merged command channel** (Stop/SeekTo can never be
deafened by backpressure), byte/frame-budgeted queue (constant memory at any clip
length), rebuffer-don't-drift, audio = a shell WinRT `MediaPlayer` (video tracks
deselected — audio only) whose position is the master clock via ~4 Hz
`AudioClockSample`s. Seek recreates the reader positioned at the target (repositioning a
warm HEVC reader blocks ~1 s — spike-measured); the producer parks after EOS so `P`
replays via seek-to-0. 4K30 software decode is comfortable; 4K60 is borderline — NV12 +
in-shader YUV or hardware decode is the reserved escalation (ADR-012).

**macOS routing (2026-07-16, macos-video-smoothness plan):** MP4/MOV → `AVPlayer`;
**everything else (MKV/WebM included) → the Session route** (FFmpeg → wgpu → Metal). The
`AVSampleBufferDisplayLayer` "sample-buffer" presenter is **parked, opt-in only**
(`PB_SAMPLE_BUFFER=1`) — it dropped ~3 frames/sec that both other routes play flawlessly;
it is kept as the on-device Dolby-Vision **reference renderer** (DoVi itself is deferred;
detection ships — a DoVi Profile-5 file on the Session route toasts an honest
colors-can't-be-shown warning and the Details panel names every DoVi profile).
`PB_TRACE=1` prints a per-2s Session dropped-frames diag (the `sb-play diag` analog).

## Known v1 limitations (deliberate)

Radiance-HDR / OpenEXR (image-crate, not WIC) still clamped to SDR; CMYK JPEG
mis-colored; first frame only (GIF/animated-WebP/Live-Photo/multipage-TIFF).
LUT/CLUT & gray/CMYK ICC profiles → sRGB passthrough (the `lcms2`-behind-a-flag
escalation). SDR-white level is a 200-nit default (real value via DisplayConfig =
TODO). ⚠ On an **HDR desktop**, GDI screen capture of the flip swapchain returns
all-white — a Windows limit, not a render bug (use the `offscreen_png` example to
verify rendering).
