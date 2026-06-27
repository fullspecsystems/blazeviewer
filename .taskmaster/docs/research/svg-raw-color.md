# PhotoBlaze Research: SVG, Camera RAW, and Color Management

Research date: 2026-06-26. Sources are 2024–2026 unless noted. Target platform:
Windows 11 (primary), RTX 5090, 7680×3840 @120 Hz display; future macOS (Apple
M-series) port. PhotoBlaze renders to a wgpu device and is obsessed with speed.

---

## Overview

Three subsystems are covered here, with very different maturity and risk profiles:

- **SVG (first-class want):** Two real options exist today. `resvg` (CPU, via
  `tiny-skia`) is mature, complete, and trivially integrated. `vello`/`vello_svg`
  (GPU, on wgpu) is fast and shares our device but is alpha/beta and SVG-incomplete.
  **Recommendation: ship resvg now, rasterizing at display resolution; keep vello on
  the watchlist.**
- **Camera RAW (nice-to-have):** The cheap, high-value path is **extracting the
  embedded full-size JPEG preview** that nearly every RAW file already contains —
  microseconds-to-milliseconds and "good enough for browsing." Full demosaic is
  100–1000× more expensive and only needed on zoom/1:1 inspection. **Recommendation:
  ship embedded-preview extraction first (small effort); defer demosaic.**
- **Color management:** For the common case (matrix/TRC profiles like sRGB, Display
  P3, Adobe RGB), the fast path is **parse the ICC → extract a 3×3 matrix + transfer
  curves → apply in a WGSL shader on the GPU**. Use a CMS library only to *build* the
  transform (and to handle complex LUT/CMYK profiles on the CPU). **Recommendation:
  `moxcms` (pure Rust, clean license, exposes matrices) as primary, with `lcms2`
  as a fallback for exotic profiles.**

---

## 1. SVG Rendering

### The two contenders

**resvg + usvg + tiny-skia (CPU)** — the Linebender-maintained stack.
- Architecture splits cleanly: `usvg` parses/normalizes SVG into a simplified tree;
  `resvg` rasterizes that tree with `tiny-skia` (a Rust port of a Skia subset). You
  can use `usvg` alone and render the tree with your own backend.
  ([resvg GitHub](https://github.com/linebender/resvg),
  [tiny-skia](https://github.com/linebender/tiny-skia))
- Maturity: **production-grade.** Latest release **v0.47.0 (2026-02-10)**, ~1,186
  commits, active SVG 2 work, dual **Apache-2.0/MIT** (clean for a proprietary
  viewer). It is the reference for SVG correctness — even the Vello team points users
  who need a "correct SVG renderer" back to resvg.
  ([docs.rs/resvg](https://docs.rs/resvg/latest/resvg/),
  [lib.rs/resvg](https://lib.rs/crates/resvg))
- Completeness: handles the static SVG feature set comprehensively — paths, fills,
  strokes, gradients, patterns, clip paths, masks, text/fonts, and filter effects.
  It is the most complete SVG renderer in the Rust ecosystem.
- 100% Rust, no C deps, ~3 MB CLI — **zero Windows/macOS build friction.**
- Output: renders into a `tiny_skia::Pixmap`, which is just premultiplied RGBA8 in a
  contiguous buffer. That maps **directly** to a `wgpu::Texture` upload
  (`queue.write_texture`) — so "CPU renderer" does not mean "CPU display path." We
  rasterize once at the needed pixel size and the GPU composites/zooms it.

**vello + vello_svg (GPU compute on wgpu)** — Linebender's next-gen renderer.
- A GPU-compute-centric 2D renderer using wgpu; same PostScript/SVG-style imaging
  model. `vello_svg` bridges `usvg` output into Vello scenes.
  ([vello GitHub](https://github.com/linebender/vello),
  [vello_svg](https://github.com/linebender/vello_svg)) Apache-2.0/MIT.
- **Device sharing: yes** — Vello is constructed against an existing
  `wgpu::Device`/`Queue`, so it can share PhotoBlaze's wgpu device rather than
  spinning up its own. This is the strongest argument in its favor for us.
- Maturity (verified, this is the key caveat): the main `vello` crate is **explicitly
  "alpha"**; the README still lists blur/filter effects, conflation artifacts, GPU
  memory allocation strategy, and glyph caching as in-progress. MSRV 1.88.
  ([vello README](https://github.com/linebender/vello))
- As of **2026 Q1**, Linebender ships *three* implementations: `vello` (classic GPU
  compute, alpha), `vello_cpu` (SIMD/multithreaded CPU), and `vello_hybrid`
  (GPU/CPU "sparse strips"). `vello_hybrid` is described as **"roughly beta
  quality… should be usable"** with rough edges and perf work remaining. Releases:
  sparse-strips 0.0.7, vello 0.8.0.
  ([Linebender Dec 2025](https://linebender.org/blog/tmil-24/),
  [Linebender 2026 Q1](https://linebender.org/blog/tmil-25/))
- **SVG support is incomplete by design.** `vello_svg`'s own docs say it provides
  "decent coverage… up to what vello will support" and recommend resvg "if you are
  looking for a correct SVG renderer." It lacks a number of SVG features (notably the
  filter/blur effects still missing in Vello itself).
  ([vello_svg lib.rs](https://lib.rs/crates/vello_svg))
- Ecosystem-usage analysis (Jan 2025) found most Vello adopters only use basic
  `fill`/`stroke`; advanced paths (`draw_image`, `push_layer`) — exactly what
  arbitrary SVG needs — see thin adoption and are less battle-tested.
  ([PoignardAzur analysis](https://poignardazur.github.io/2025/01/18/vello-analysis/))

### Performance at high resolution (7680-wide)

- The viewer wants SVGs crisp at display resolution, so we rasterize at the actual
  on-screen pixel size (e.g. up to ~7680 px wide), **not** at nominal SVG size.
- resvg/tiny-skia is CPU SIMD rasterization. For a static viewer this is a **one-time
  cost per zoom level**, amortized: rasterize once, cache the Pixmap as a GPU texture,
  then pan/zoom is pure GPU compositing at 120 Hz. A full-screen complex SVG rasterize
  is on the order of single-digit-to-tens of milliseconds — perfectly fine because it
  is not per-frame. Re-rasterize only when the zoom factor changes enough to matter
  (and that can be done off the UI thread / debounced).
- Vello's advantage is *re-rasterizing every frame is cheap* (animation, continuous
  zoom). For a photo viewer showing mostly static vector art, that advantage is
  largely moot — we don't redraw the SVG each frame. tiny-skia lacks specific
  published 4K/8K benchmarks, but the architectural point stands: our SVG workload is
  cache-friendly. ([tiny-skia lib.rs](https://lib.rs/crates/tiny-skia))

### SVG recommendation

**Use `resvg`/`usvg` now.** It is complete, correct, Apache/MIT, zero-build-friction
on Windows and macOS, and integrates with our GPU path by uploading its Pixmap to a
wgpu texture. Rasterize at the target on-screen resolution and cache per zoom level.

Keep an eye on **`vello_hybrid`** (not classic `vello`) as the future GPU path: it can
share our wgpu device and would let us re-rasterize SVG live during continuous zoom
without a CPU round-trip. Revisit when (a) it reaches stable/non-beta and (b)
`vello_svg` closes its SVG feature gaps (filters/masks). Today it is too immature to be
the *only* SVG renderer for a viewer that markets crispness and correctness. A clean
abstraction (render-SVG-to-texture trait) lets us swap backends later with low cost.

---

## 2. Camera RAW

### Key insight (verified): embedded previews are the fast path

Nearly every RAW file embeds one or more **JPEG previews**, typically including a
**full-resolution** preview the camera generated for its own LCD/review. Extracting
that JPEG is essentially a file-structure seek + copy of already-compressed bytes — no
demosaic, no color pipeline. This is exactly how fast browsers/cullers (FastRawViewer,
Lightroom's initial "embedded preview" mode) feel instant.
([FastRawViewer RawPreviewExtractor](https://www.fastrawviewer.com/RawPreviewExtractor),
[have-camera-will-travel: RAW already contains a JPEG](https://havecamerawilltravel.com/extract-jpg-raw-2/))

Speed evidence: `rawtojpg`/`jpgfromraw` extracts embedded JPEGs at **~386 MiB/s vs
exiftool's ~24 MiB/s (~15× faster)** across a 4000-file directory, "as-is, same image
size, no recompression." Extraction does not touch sensor data.
([rawtojpg / jpgfromraw](https://github.com/cdown/rawtojpg),
[lib.rs/rawtojpg](https://lib.rs/crates/rawtojpg))

Caveat: a few sources lack a *full-size* embedded preview (Cinema-DNG, some phones /
action cams) — they may carry only a small thumbnail or none. Fallback there is
demosaic (or a "no high-res preview available" state).

### How to extract embedded previews in Rust

The embedded preview lives in EXIF/MakerNotes IFDs (e.g. `PreviewImage`,
`JpgFromRaw`, or a large `ThumbnailImage`), referenced by offset+length. Options:

- **`rawler`** (pure Rust; the de-facto Rust RAW crate) parses RAW file structures
  and metadata across 300+ cameras incl. Bayer + X-Trans + CR3. It exposes file
  structure/metadata and is used as the RAW foundation by real Rust apps. For formats
  it doesn't fully cover, downstream apps fall back to exiftool to pull the embedded
  preview. License: **LGPL-2.1** (copyleft — note for a proprietary static-linked
  binary; dynamic-link/relink terms apply).
  ([rawler docs.rs](https://docs.rs/rawler/latest/rawler/),
  [rawler lib.rs](https://lib.rs/crates/rawler))
- **`kamadak-exif` (`exif-rs`)** — pure-Rust EXIF/TIFF-IFD parser (MIT/Apache). For
  many RAW formats (which are TIFF-based: CR2, NEF, ARW, DNG, etc.) you can walk the
  IFDs yourself, find the largest `JPEGInterchangeFormat`/preview tag, and slice those
  bytes out. This is the **lightest, cleanest-license** way to get the embedded JPEG
  without pulling in a full RAW decoder, and avoids LGPL.
  ([exif-rs](https://github.com/kamadak/exif-rs))
- **`raw_preview_rs`** — convenience crate that returns preview JPEG bytes for 27+
  formats, but it wraps **LibRaw + libjpeg-turbo + stb_image** and is **GPL-3.0** —
  build friction (C deps) *and* a license that's a non-starter for proprietary code.
  Avoid. ([raw_preview_rs lib.rs](https://lib.rs/crates/raw_preview_rs))
- **`zenraw`** (imazen) — newer safe-Rust decoder, scene-referred linear f32 output,
  swappable backends (rawloader/rawler/darktable). But **AGPL-3.0 / commercial** —
  AGPL is disqualifying for a proprietary viewer unless a commercial license is bought.
  ([zenraw](https://github.com/imazen/zenraw))

**Recommended preview approach:** start with `kamadak-exif` (or a tiny custom IFD
walker) to pull the largest embedded JPEG and decode it with our normal JPEG path
(`zune-jpeg`/`jpeg-decoder`/`image`). Add `rawler` only if/when we need broader format
coverage or its richer metadata — and weigh its LGPL terms then.

### Cost of full demosaic (only when the user zooms to 1:1)

Full RAW rendering = decode/decompress sensor data → black/white-level → white
balance → **demosaic** → color matrix → output transform. Demosaic dominates and
scales with resolution and algorithm quality (bilinear is fast and ugly;
AHD/Menon/AMaZE are slow and good). On CPU this is typically **hundreds of ms to a
couple of seconds** for a 24–60 MP file — orders of magnitude slower than preview
extraction, and clearly *not* something to do on every thumbnail.
([LibRaw processing model](https://www.libraw.org/docs),
[LibRaw](https://github.com/LibRaw/LibRaw))

The fast modern answer is **GPU demosaic**. Real-world Rust proof point: **RapidRAW**
(Rust + Tauri) decodes with **`rawler`** then runs the *entire* pipeline incl. a
**Menon demosaic in WGSL on wgpu**, hitting **120 fps** on large files. This is
directly portable to PhotoBlaze's wgpu device.
([RapidRAW](https://github.com/CyberTimon/RapidRAW),
[RapidRAW wgpu renderer blog](https://www.getrapidraw.com/blog/wgpu-renderer))

### RAW effort estimate & verdict

- **Embedded-preview browsing: SMALL effort (days).** IFD walk + JPEG decode +
  existing texture upload. ~90% of the "I want to see my RAWs" value. Ship this.
- **Full demosaic on zoom: MEDIUM–LARGE effort (weeks).** Either (a) CPU demosaic via
  `rawler` (simple, slow, blocks a worker thread) as a stopgap, or (b) a wgpu/WGSL
  demosaic pipeline (fast, the right long-term answer, more work; RapidRAW is a strong
  template). Defer until preview browsing ships and there's demand.

---

## 3. Color Management

### The model

Decoded pixels live in some source color space (sRGB, Display P3, Adobe RGB, ProPhoto,
or a camera/scanner profile carried as an embedded **ICC profile**). They must be
transformed to the **display** color space so colors are correct on a wide-gamut
panel. Two profile classes:

1. **Matrix/TRC profiles** (the overwhelmingly common case for photos: sRGB, Display
   P3, Adobe RGB, Rec.2020, most camera output): characterized by per-channel
   **transfer curves (TRC/gamma)** + a **3×3 matrix** (RGB→XYZ). The full transform is
   `gamma-decode → 3×3 source-RGB→XYZ → 3×3 XYZ→dest-RGB → gamma-encode`, collapsible
   to **two small LUTs + one 3×3 matrix.**
2. **LUT-based / CMYK / N-channel profiles** (scanners, printers, some camera DNG
   profiles): need multidimensional CLUTs and a real CMS engine.

### The fast path: 3×3 matrix in a shader (GPU-side)

For matrix/TRC profiles, the whole transform is a per-pixel **gamma LUT lookup (or
analytic gamma) + a single mat3 multiply + re-encode** — trivially a WGSL fragment
shader, effectively free at our fill rates. This is the textbook "fast viewer"
approach and it is fully GPU-side: zero per-image CPU cost beyond parsing the ICC once
to obtain the matrix and curves. Windows itself exposes a GPU display transform
(linear color matrix + 1D LUT) for exactly this reason.
([MS Learn: advanced-color ICC profiles](https://learn.microsoft.com/en-us/windows/win32/wcs/advanced-color-icc-profiles),
[MS Learn: display calibration MHC](https://learn.microsoft.com/en-us/windows/win32/wcs/display-calibration-mhc))

So: **per-image ICC transform is NOT a perf concern at our speeds** — *provided* we use
the matrix-in-shader path for matrix profiles. The only real CPU work is parsing the
profile (microseconds) and, for the rare complex profile, building/applying a CLUT.

### Crate choice

- **`moxcms`** (awxkee) — **pure Rust**, "fast and safe," **BSD-3-Clause/Apache-2.0**
  (clean license), actively developed (v0.8.1, 2026-03-07). Crucially for us, its API
  exposes the building blocks for the GPU path: `Matrix3f`/`Matrix3d`, `Vector3`,
  built-in `SRGB_MATRIX`/`DISPLAY_P3_MATRIX`/`BT2020_MATRIX` constants, a
  `ColorProfile` ICC representation, `RenderingIntent`, `Layout`, and a `Stage`/LUT
  architecture. We can parse an embedded ICC into a `ColorProfile`, pull the primaries
  matrix + TRC for matrix profiles, and feed those straight into a WGSL uniform; fall
  back to its CPU transform executors for LUT/CMYK profiles.
  ([moxcms GitHub](https://github.com/awxkee/moxcms),
  [moxcms docs.rs](https://docs.rs/moxcms/latest/moxcms/))
- **`qcms`** (Firefox) — pure-ish Rust, security-hardened ICC parser, "one of the
  fastest CMS around," **MPL-2.0**. Battle-tested in Firefox for exactly this
  (transforming web image data between ICC profiles). Solid, but more
  transform-engine-oriented and less rich on exposing raw matrices than moxcms; ICC v4
  support historically weaker. ([qcms GitHub](https://github.com/FirefoxGraphics/qcms),
  [qcms docs.rs](https://docs.rs/qcms/latest/qcms/))
- **`lcms2`** (kornelski bindings to Little CMS) — the **most complete/correct** CMS,
  the industry reference, handles every exotic profile. **C dependency**, but
  `lcms2-sys` **bundles LCMS 2.15 and builds from source** (`static` feature) so
  **Windows builds need no external library/vcpkg** — friction is low in practice.
  License MIT. Used by the real Rust viewer **avis-imgv** (which ships sRGB/Adobe
  RGB/Display P3 ICCs and picks via profile description).
  ([rust-lcms2](https://github.com/kornelski/rust-lcms2),
  [lcms2-sys](https://github.com/kornelski/rust-lcms2-sys),
  [avis-imgv](https://github.com/hats-np/avis-imgv))
- `kolor` — small gamut/color-space math crate, good for hardcoded space conversions,
  but not a general ICC engine; not sufficient alone for arbitrary embedded profiles.

### Color recommendation

**Primary: `moxcms`.** Pure Rust, clean license, no build friction on Windows or
macOS, and it exposes matrices/curves so we can run the common matrix/TRC transform as
a **mat3 + LUT in WGSL** — the fast, GPU-side path. Use `moxcms` to *build* transforms
on the CPU; execute matrix profiles on the GPU, and only fall back to a CPU CLUT pass
for the rare LUT/CMYK/N-channel profile. Keep **`lcms2`** available behind a feature
flag as the correctness fallback for pathological profiles (its bundled-source build
means Windows is painless).

Pipeline: decode → detect embedded ICC (or assume sRGB if untagged) → if matrix/TRC,
extract matrix+TRC and pass to shader uniforms; if complex, CPU-transform once into a
working space → upload texture → GPU does source→display transform during composite.
Cache the per-profile matrix/LUT (images sharing a profile reuse it).

### HDR / wide-gamut output (the display side)

The transform's *destination* is the display space, which ties into wgpu surface
format. Reality check for 2024–2026:

- wgpu still lacks well-defined cross-platform **HDR surface** support; tracking issue
  **#2920 is open**. The common pattern is render HDR internally (`Rgba16Float`) then
  tonemap to an 8-bit `Bgra8UnormSrgb` surface.
  ([wgpu HDR tutorial](https://sotrh.github.io/learn-wgpu/intermediate/tutorial13-hdr/),
  [wgpu issue #2920](https://github.com/gfx-rs/wgpu/issues/2920))
- **Windows/DXGI** HDR path: `R16G16B16A16_FLOAT` back buffer with
  `RGB_FULL_G10_NONE_P709` colorspace — usable but wgpu's surface plumbing for it is
  limited today; may need backend-specific handling for true HDR on the RTX 5090.
- **macOS/Metal**: an `Rgba16Float` surface engages EDR/wide-gamut on Apple displays
  with values >1.0 — the cleaner HDR story.

For v1, a **wide-gamut SDR** target (transform source → Display P3 / sRGB, output 8-bit
or 10-bit) is the pragmatic, correct-looking choice; treat true HDR display output as a
later milestone gated on wgpu surface maturity.

---

## 4. macOS Portability Notes (future M-series port)

- **SVG (resvg/usvg/tiny-skia):** 100% Rust, no C deps — compiles cleanly on Apple
  Silicon, no extra work. Vello/vello_hybrid run on wgpu→Metal; same alpha/beta
  caveats apply on both platforms.
- **RAW:** `rawler`/`kamadak-exif` are pure Rust — clean on macOS. Embedded-JPEG path
  is fully portable. A wgpu/WGSL demosaic (RapidRAW-style) runs on Metal unchanged.
  Avoid LibRaw-based crates (`raw_preview_rs`, `rsraw`) partly to dodge C-build setup
  on macOS and partly for licensing.
- **Color:** `moxcms`/`qcms` are pure Rust — clean. `lcms2-sys` builds from bundled
  source on macOS too (just needs a C compiler from Xcode CLT). macOS has strong
  system color management (**ColorSync**), and Metal surfaces get wide-gamut/EDR more
  readily than Windows. Note the OpenGL/Metal "opt-in color space" gotcha — set the
  layer/surface color space explicitly so we control the transform rather than letting
  the compositor double-correct.
  ([Apple color management across frameworks](https://juniperphoton.substack.com/p/color-management-across-apple-frameworks),
  [Apple TN2313](https://developer.apple.com/library/archive/technotes/tn2313/_index.html))
- General: keeping all three subsystems pure-Rust (resvg, rawler/kamadak-exif, moxcms)
  makes the macOS port essentially a recompile for these areas — the strongest reason
  to prefer the pure-Rust crates over the C-backed ones.

---

## 5. Recommendations (summary)

| Area | Recommendation | Effort | License | Risk |
|------|----------------|--------|---------|------|
| **SVG** | `resvg`/`usvg`, rasterize at on-screen res → upload Pixmap to wgpu texture, cache per zoom | Small | Apache/MIT | Low |
| SVG (future) | Watch `vello_hybrid` (shares wgpu device) for live-zoom GPU SVG | — | Apache/MIT | Alpha/Beta |
| **RAW preview** | `kamadak-exif` IFD walk → slice embedded full-size JPEG → normal JPEG decode | Small | MIT/Apache | Low |
| RAW (broader) | `rawler` for more formats/metadata if needed | Small–Med | LGPL-2.1 | Copyleft note |
| RAW (zoom 1:1) | Defer; later add wgpu/WGSL demosaic (RapidRAW template) | Med–Large | — | Deferred |
| **Color** | `moxcms`: parse ICC, matrix/TRC → mat3+LUT in WGSL (GPU); CPU fallback for CLUT/CMYK | Small–Med | BSD/Apache | Low |
| Color fallback | `lcms2` (bundled build, no vcpkg) behind feature flag for exotic profiles | Small | MIT | Low |
| HDR output | Wide-gamut SDR (P3/sRGB) v1; true HDR later, gated on wgpu surface support | — | — | wgpu gap |

**Net:** All three subsystems can be built primarily on **pure-Rust, permissively
licensed, zero-build-friction crates** (resvg, kamadak-exif/rawler, moxcms) that share
PhotoBlaze's wgpu device for the display/transform step. SVG is shippable and complete
today; RAW browsing is a small, high-value addition; color management's fast path is a
cheap GPU shader and not a bottleneck at our speeds.

---

## 6. Open Questions

1. **rawler LGPL-2.1**: acceptable for PhotoBlaze's distribution/licensing model? If
   not, how far does a custom `kamadak-exif` IFD walker get us on format coverage
   before we need rawler/LibRaw? (Decide license policy early.)
2. **Embedded preview coverage**: which target cameras lack a *full-size* embedded
   preview (vs only a ~1–2 MP thumbnail)? For those, is on-zoom demosaic acceptable, or
   do we upscale the thumbnail as a stopgap?
3. **vello_hybrid timeline**: when does it reach stable + does `vello_svg` close its
   filter/mask gaps? That determines whether/when we add a GPU SVG backend.
4. **SVG re-rasterization policy**: at what zoom-delta do we re-rasterize resvg vs GPU-
   upscale the cached Pixmap? Need a quality/perf threshold (esp. at 7680 px wide).
5. **Display profile acquisition on Windows**: read the system/monitor ICC via Windows
   Color System APIs, or assume sRGB/P3? Affects correctness on the user's wide-gamut
   panel.
6. **True HDR on RTX 5090**: is backend-specific DXGI HDR surface code worth it for v1,
   or wait for wgpu issue #2920 to land well-defined HDR surfaces?
7. **moxcms maturity at scale**: is moxcms's ICC v4 / edge-case coverage sufficient, or
   do enough real-world profiles force the lcms2 fallback that we should lead with
   lcms2? (Benchmark against a corpus of real embedded profiles.)
8. **Demosaic quality vs speed**: which algorithm for the eventual GPU path (Menon, as
   RapidRAW uses, vs AMaZE/bilinear) balances 120 Hz interactivity with quality at 1:1?

---

## 7. Sources

SVG:
- resvg — https://github.com/linebender/resvg
- resvg docs — https://docs.rs/resvg/latest/resvg/ , https://lib.rs/crates/resvg
- tiny-skia — https://github.com/linebender/tiny-skia , https://lib.rs/crates/tiny-skia
- vello — https://github.com/linebender/vello , https://docs.rs/vello
- vello_svg — https://github.com/linebender/vello_svg , https://lib.rs/crates/vello_svg
- vello_cpu — https://crates.io/crates/vello_cpu
- Linebender Dec 2025 — https://linebender.org/blog/tmil-24/
- Linebender 2026 Q1 — https://linebender.org/blog/tmil-25/
- Vello usage analysis — https://poignardazur.github.io/2025/01/18/vello-analysis/

RAW:
- rawler — https://docs.rs/rawler/latest/rawler/ , https://lib.rs/crates/rawler
- rawloader — https://github.com/pedrocr/rawloader
- kamadak exif-rs — https://github.com/kamadak/exif-rs
- rawtojpg/jpgfromraw — https://github.com/cdown/rawtojpg , https://lib.rs/crates/rawtojpg
- raw_preview_rs — https://lib.rs/crates/raw_preview_rs
- zenraw — https://github.com/imazen/zenraw
- rsraw — https://github.com/hexilee/rsraw
- LibRaw — https://www.libraw.org/docs , https://github.com/LibRaw/LibRaw
- FastRawViewer preview extractor — https://www.fastrawviewer.com/RawPreviewExtractor
- "RAW already contains a JPEG" — https://havecamerawilltravel.com/extract-jpg-raw-2/
- avis-imgv (Rust viewer) — https://github.com/hats-np/avis-imgv
- RapidRAW (Rust+wgpu RAW editor) — https://github.com/CyberTimon/RapidRAW , https://www.getrapidraw.com/blog/wgpu-renderer

Color:
- moxcms — https://github.com/awxkee/moxcms , https://docs.rs/moxcms/latest/moxcms/
- rust-lcms2 — https://github.com/kornelski/rust-lcms2 , https://github.com/kornelski/rust-lcms2-sys , https://lib.rs/crates/lcms2
- qcms — https://github.com/FirefoxGraphics/qcms , https://docs.rs/qcms/latest/qcms/
- MS Learn advanced-color ICC — https://learn.microsoft.com/en-us/windows/win32/wcs/advanced-color-icc-profiles
- MS Learn display calibration (MHC) — https://learn.microsoft.com/en-us/windows/win32/wcs/display-calibration-mhc

HDR / wgpu / macOS:
- wgpu HDR tutorial — https://sotrh.github.io/learn-wgpu/intermediate/tutorial13-hdr/
- wgpu HDR surface issue #2920 — https://github.com/gfx-rs/wgpu/issues/2920
- Apple color management across frameworks — https://juniperphoton.substack.com/p/color-management-across-apple-frameworks
- Apple TN2313 color management — https://developer.apple.com/library/archive/technotes/tn2313/_index.html
