# CPU Image-Decode Library Landscape in Rust (2025–2026)

Research for **PhotoBlaze**, a from-scratch Rust photo viewer optimized for render/display
speed. Images are decoded ahead-of-time by a multithreaded pool and kept resident for instant
flicking. **Windows 11 is the primary target; an Apple-Silicon (macOS) port is desired later.**

> Scope: per-format CPU decode crates, ranked for speed, with attention to SIMD, multithreaded
> decode, **scaled / downscale-on-decode** (decode only enough pixels to fit a ~7680×3840
> display), and **Windows build friction** (C toolchain / CMake / NASM / meson / vcpkg /
> pkg-config vs pure-Rust / prebuilt). "Works today" is distinguished from "experimental".

---

## 1. Overview

Three findings shape the whole design:

1. **Scaled-decode is a JPEG/JXL/WebP privilege, not a universal feature.** Only DCT-based
   (JPEG) and some container codecs expose native reduced-resolution decode. **libjpeg-turbo
   (via `turbojpeg`/`mozjpeg`) decodes directly at 1/2, 1/4, 1/8** — the single biggest win for
   a viewer pushing large JPEGs to a fixed-size screen. **WebP (libwebp)** has an internal
   rescaler (`use_scaling`). **JPEG XL** is natively progressive with a 1/8 DC pass (but the
   pure-Rust decoder does not yet expose it). **PNG, AVIF, HEIC, TIFF, QOI, BMP have NO native
   scaled decode** — you decode full then downsample (use SIMD `fast_image_resize`), or fall
   back to an **embedded thumbnail/preview** carried in the container.

2. **Pure-Rust has caught or beaten C for the inflate/PNG/JPEG core**, removing build friction
   for the most common formats. The image-rs `png` crate now *outperforms* libpng and spng
   (June 2026 benchmark) and is Chromium's default PNG decoder since M139. `zune-jpeg` is within
   ~10 ms of libjpeg-turbo and is image-rs's new default JPEG backend. The C dependencies that
   remain genuinely necessary are the **modern video-codec formats**: AVIF (dav1d) and HEIC
   (libheif/libde265), plus optionally `turbojpeg` (only if you want its scaled decode).

3. **Single-image decode is essentially single-threaded for every still format.** Inflate, PNG
   defiltering, JPEG entropy decode, and single-tile AVIF/HEIC are serial. Threading pays off
   **across images**, not within one — so PhotoBlaze's prefetch pool, not per-decode
   parallelism, is where concurrency lives.

---

## 2. Per-format summary table

| Format | Best crate (speed) | Pure-Rust? | SIMD | Scaled decode? | Threads (1 image) | Windows build |
|---|---|---|---|---|---|---|
| **JPEG** | `turbojpeg` (libjpeg-turbo) | No (C) | SSE2/AVX2/NEON | **YES — DCT 1/2,1/4,1/8** | No | CMake + C + **NASM** (or vcpkg/pkg-config prebuilt) |
| JPEG (pure-Rust) | `zune-jpeg` | Yes | AVX2/SSE + NEON | No (resize after) | No | None |
| **PNG / APNG** | `png` (image-rs) | Yes | partial `std::simd`* | No | No | None (`zlib-rs` opt = unsafe-Rust, still no C) |
| PNG (alt) | `zune-png` | Yes | x86 + portable-simd | No | No | None |
| **WebP** | `libwebp-sys` | No (C) | SSE/AVX/NEON | **YES — `use_scaling`** | No | `cc` (C compiler, **no CMake**) |
| WebP (pure-Rust) | `zenwebp` / `image-webp` | Yes | zenwebp: yes / image-webp: no | No | No | None (zenwebp = AGPL!) |
| **AVIF** | `dav1d` crate + `avif-parse` | No (C) | AVX2/SSSE3/NEON asm | No (use thumbnail) | tile-only (rare) | meson+ninja+**nasm**; vcpkg+pkg-config |
| AVIF (batteries) | `libavif-sys` | No (C) | via dav1d | No | tile-only | CMake + bundled dav1d (meson/nasm) |
| **HEIF/HEIC** | `libheif-rs` (libde265) | No (C) | libde265 SSE | No (use `thmb` item) | tile-only | **HIGH — vcpkg (de265/aom/x265)** |
| **JPEG XL** | `jxl-oxide` | Yes | `jxl_simd` | No native (progressive/DC only) | rayon (default) | None |
| JXL (native 1/8 DC) | `jpegxl-rs` (libjxl) | No (C++) | highway | **YES — ratios 1,2,4,8** | yes | CMake + C++ (or vcpkg / `DEP_JXL_LIB`) |
| **TIFF** | `tiff` (image-rs) | Yes | minimal | No (no pyramid API) | No | None |
| **BMP** | `image` crate | Yes | n/a | No | No | None |
| **QOI** | `qoi` (aldanor) | Yes | n/a (serial) | No (by design) | No | None |

\* image-rs `png` ships `std::simd` defilter paths (Chromium-contributed) but they are **not
runtime-enabled by default yet** — more headroom remains.

---

## 3. Per-format detail

### 3.1 JPEG — the format where scaled-decode matters most

JPEG is the highest-volume format and the one with the richest scaled-decode story, so it
deserves the most care. Candidates:

**`turbojpeg` (libjpeg-turbo bindings) — fastest *with* scaled decode. Recommended for the JPEG
hot path.**
- Latest `turbojpeg` 1.4.0 (Jan 2026); `turbojpeg-sys` bundles **libjpeg-turbo 3.1.0**. Active
  (honzasp/rust-turbojpeg).
- The speed reference everyone benchmarks against: hand-written SSE2/AVX2 (x86-64) and NEON
  (ARM) SIMD.
- **Native DCT scaled decode (the key capability).** `Decompressor` carries a
  `scaling_factor: ScalingFactor` with `set_scaling_factor()` / `scaling_factor()` and
  `supported_scaling_factors() -> Vec<ScalingFactor>` (the 16 libjpeg ratios 2/1 … 1/8).
  Constants verified in source: `ScalingFactor::{ONE, ONE_HALF, ONE_QUARTER, ONE_EIGHTH, TWO}`
  (`ONE_HALF = {num:1, denom:2}`, etc.). Doc: *"scaling is implemented in the DCT algorithm, so
  scaling factors are limited to multiples of 1/8."* Workflow: `read_header()` →
  `DecompressHeader`; `header.scaled(factor)` returns a header with the **downscaled
  (width,height)**; allocate an `Image<&mut [u8]>` at that size; `decompress()`. This decodes
  *only enough pixels*, saving both time and memory — exactly PhotoBlaze's strategy. Lossy JPEG
  only.
- No single-image multithreading.
- **Windows friction: HIGH if building from source** (`TURBOJPEG_SOURCE=vendor` needs CMake +
  C compiler + **NASM** for SIMD; with default `require-simd` it *errors* rather than silently
  going scalar if NASM is absent). **Prebuilt path exists**: the `pkg-config` feature or
  `TURBOJPEG_SOURCE=explicit` links a system/vcpkg libjpeg-turbo (vcpkg ships it on Windows).

**`mozjpeg` (kornelski) — also has scaled decode; choose only if you also encode.**
- `mozjpeg` 0.10.13 over `mozjpeg-sys` 2.2.x (statically builds the mozjpeg fork of
  libjpeg-turbo). Decode speed ≈ libjpeg-turbo (same SIMD core); its real differentiator is
  encode quality.
- Scaled decode: `Decompress::scale(numerator: u8)` rescales by `numerator/8` (1–16; 8 =
  unscaled) → libjpeg `scale_num/scale_denom`.
- **Windows friction: HIGH** — needs **NASM + C compiler + CMake**; no clean prebuilt
  convenience comparable to turbojpeg's pkg-config story.

**`zune-jpeg` — fastest *pure-Rust*, zero build friction, NO scaled decode.**
- 0.5.x (2026), very active (Caleb Etemesi); **image-rs migrated its JPEG path to it** from
  jpeg-decoder. Self-described goal: "as fast as libjpeg-turbo … ±10 ms"; roughly ~2× faster
  than jpeg-decoder.
- **SIMD by default includes both `x86` (AVX2/SSE) AND `neon` (ARM)** — the NEON default matters
  for the Apple-Silicon port. Optional `portable_simd` (Rust ≥ 1.87).
- Single-threaded; **no DCT downscale** (decode full, then `fast_image_resize`).
- **Pure Rust** (only dep `zune-core`): no cc/cmake/bindgen/NASM. Zero Windows friction.

**`jpeg-decoder` — pure-Rust, *has* scaled decode, but slow and being retired.**
- 0.3.2, explicitly in maintenance mode (image-rs is moving to zune-jpeg). Slowest of the four;
  no meaningful hand-tuned SIMD.
- Pure-Rust scaled decode: `Decoder::scale(req_w, req_h) -> (u16,u16)` picks the smallest
  supported factor ≥ requested (factors 1/8, 1/4, 1/2, 1). Optional `rayon` parallelizes
  post-processing only. Still reachable transitively (the `tiff` crate uses it for JPEG-in-TIFF).

> **JPEG verdict:** Use **`turbojpeg`** for the production JPEG path — top SIMD speed *plus*
> native 1/2·1/4·1/8 decode straight to display size. Keep **`zune-jpeg`** as the pure-Rust,
> zero-friction A/B alternative (pair with `fast_image_resize` for downscale). Also worth
> watching: `developer0hye/libjpeg-turbo-rs`, a full Rust port hitting 0.77–1.23× of C on
> AVX2 and ~1.0× on M1 NEON — promising but newer/less battle-tested.

### 3.2 PNG / APNG — inflate-bound, and pure-Rust now wins

**`png` (image-rs) — fastest PNG, recommended.** Pure Rust, v0.18.x. In the **June 2026
image-rs benchmark** it leads everything (geomean MP/s): **image-png 339.9 vs zune-png 313.2 vs
spng 233.1 vs libpng 180.4 on Ryzen 9 7950X**; **310.5 vs 253.7 vs 159.1 vs 177.6 on Apple M4**.
Memory-safe Rust now decisively beats the C PNG libraries. It is **Chromium's default PNG
decoder since M139 (Aug 2025)** and underlies **GNOME 49** (via glycin/gdk-pixbuf). Inflate
backends: **miniz_oxide** (default, safe), **zlib-rs** (optional, faster, some unsafe —
"outstanding performance"), **fdeflate** (image-rs's PNG-tuned deflate, strong at low
compression). **APNG supported since 2020.** SIMD defilter paths exist (`std::simd`) but aren't
runtime-enabled by default yet → future headroom. Zero Windows friction.

**`zune-png` — close second.** v0.5.x; a hair behind image-png on x86, wider gap on M4. Own
`zune-inflate`; x86 intrinsics + `portable-simd` feature; **decodes APNG**. Pure Rust.

**`spng` (libspng bindings) — not worth it now.** Old marketing claimed 3–5× over the `png`
crate, but that predates image-png's rewrite; today it's *slower*. Needs a C toolchain; the
`zlib-ng` feature additionally needs **CMake**. Skip.

**`fpng`/`fpnge` — disqualified for a general viewer.** fpng's fast decoder **only decodes PNGs
fpng itself encoded** (looks for a private `fdEC` chunk, else `FPNG_DECODE_NOT_FPNG`). Useless
for arbitrary user files. C++/SSE4.1 build.

**Scaled decode: none** — PNG has no reduced-resolution decode in any crate; decode full then
downsample.

> **PNG verdict:** **`png` (image-rs) with the `zlib-rs` backend** — fastest, pure-ish Rust,
> APNG built-in, zero Windows friction. `zune-png` is a fine pure-Rust A/B alternative.

### 3.3 WebP — the other format with real scaled-decode

**`libwebp-sys` (valpackett) — fastest WebP and the only crate exposing scaled decode.** v0.14.x
(2026), active. Builds bundled libwebp with the **`cc` crate — no CMake**, just a C compiler.
Inherits libwebp's full **SSE/AVX/NEON** SIMD. Crucially it exposes the raw `WebPDecoderConfig`
with **`options.use_scaling`, `scaled_width`, `scaled_height`** → true **downscale-on-decode**
via libwebp's internal rescaler. This is the WebP equivalent of JPEG DCT scaling for a
speed-obsessed viewer.

**`webp` (jaredforth) — convenience wrapper, limited.** v0.3.x wraps an older libwebp-sys but
only the default config — **no scaling exposed**, and the main `Decoder` doesn't do animation
(separate `AnimDecoder`). Prefer `libwebp-sys` directly.

**`image-webp` (image-rs) — pure Rust, no SIMD, no scaling.** v0.2.x, `#![forbid(unsafe_code)]`,
supports lossy/lossless/alpha/animation; decode speed ~70–100% of libwebp but **no baseline
SIMD** and **no scaled decode**.

**`zenwebp` (imazen) — fastest pure-Rust WebP, but licensing landmine.** v0.4.x; fork of
image-webp adding **safe SIMD** (SSE2/SSE4.1/AVX2, NEON, WASM) bit-exact with libwebp;
**1.06–1.14× faster than libwebp (lossy), up to ~1.3× (lossless)**. But **no scale/crop on
decode**, and it is **AGPL-3.0-or-commercial** — a product blocker unless you buy the commercial
license.

> **WebP verdict:** **`libwebp-sys`** for the production path (`use_scaling` downscale + SIMD,
> easy `cc` build). For a pure-Rust/no-C build use **`image-webp`** (slower, no scaling); use
> `zenwebp` only if AGPL is acceptable.

### 3.4 AVIF — fast, but expensive, and no scaled decode

AVIF decode = AV1 **intra** decode, **far** more CPU-costly than JPEG (one cited measurement:
~5–6× the decode time of JPEG). The fast path is **dav1d** (the SIMD AV1 decoder used by Chrome,
Firefox, VLC): **1.5–2.5× faster single-threaded** and **2.3–4.5× faster multithreaded** than
libaom, with hand-written **AVX2/SSSE3/SSE/NEON** assembly.

- **`dav1d` crate (rust-av/dav1d-rs)** — v0.11.x (Nov 2025); thin safe FFI over **libdav1d
  ≥ 1.3.0**. It does **not** vendor/build dav1d — it locates a prebuilt lib via
  `system-deps`/pkg-config. Pair with **`avif-parse`** (pure-Rust ISOBMFF/MIAF parser that
  extracts the AV1 OBU + alpha) for a minimal, fast decode pipeline.
- **`libavif-sys` / `libavif` (njaard)** — v0.17.x; higher-level, wraps libavif's full container
  orchestration, **builds via CMake** (default codecs dav1d + rav1e). Batteries-included but
  more build moving parts.
- **`image` crate AVIF** (`avif-native` feature) = pure-Rust `mp4parse` container +  **C dav1d**
  codec. So even image-rs's fast AVIF still depends on libdav1d. (`ravif` is encode-only.)
- **Pure-Rust AV1?** `rav1d` (memorysafety/Prossimo) is a Rust port of dav1d (v1.1.0, May 2025),
  but it still uses the **original hand-written assembly**, exposes a **C API only** (no native
  Rust API), and is ~5–6% slower than dav1d — proven (runs in Chromium) but awkward to consume
  from Rust. **`rav1d-safe` (imazen)** replaces the asm with **safe Rust SIMD**, has a real Rust
  API (`Decoder::decode`), passes 784/803 conformance vectors, but is **~1.25–1.56× slower** and
  **experimental**. Track it as the future dependency-light path.
- **Scaled decode: none** — no JPEG-style reduced-res AV1 decode. Mitigation: AVIF/HEIF
  containers *can* carry a smaller thumbnail item (rarer for AVIF than HEIC).
- **Threading on a single still:** a still is one frame, so frame-threading gives nothing; only
  **tile threading** helps, and only if the encoder produced multiple tiles. Typical single-tile
  camera AVIFs see near-zero (sometimes negative, dav1d #398) speedup — **parallelize across
  images.**
- **Windows build: MODERATE.** The `dav1d` crate needs a prebuilt libdav1d; building dav1d needs
  **meson + ninja + nasm**, so on Windows install `dav1d:x64-windows` via **vcpkg** + pkg-config
  and set `PKG_CONFIG_PATH`/`SYSTEM_DEPS_DAV1D_*`. `libavif-sys` is **MODERATE–HIGH** (CMake plus
  bundled dav1d's meson/nasm).

> **AVIF verdict:** **`dav1d` crate + `avif-parse`** for the fast, lean path (or `libavif-sys`
> if you want container batteries). Expect AVIF ≫ JPEG cost; decode in the prefetch pool, prefer
> a container thumbnail for a downscaled preview, and watch `rav1d-safe`.

### 3.5 HEIF/HEIC — the Windows build pain point

- **`libheif-rs` (Cykooz) + `libheif-sys`** — v2.7.x (Feb 2026); safe wrapper over C libheif
  (min 1.17, feature flags `v1_17`…`v1_21`). Decode backend is **libde265** (HEVC intra; x265 is
  encode-only). libde265 has SSE optimizations (weaker/less complete than dav1d's). **There is
  no credible pure-Rust HEIC decoder.** It exposes embedded **thumbnail items**
  (`number_of_thumbnails()` / `get_thumbnail()` → separate `thmb` handle) — the cheap fast-preview
  path — plus arbitrary post-decode scaling.
- **Scaled decode: none native** — use the embedded `thmb` item when present; HEIF grid/tile
  images also allow region decode.
- **Windows build: HIGH — this is the known pain point.** On Windows `libheif-sys` uses the
  **vcpkg** crate to locate libheif, and the vcpkg build pulls **libde265, libaom, x265,
  libogg**; there are many 2024–2025 vcpkg `BUILD_FAILED` reports (x265 #43416, aom/de265 build-
  order issues, "VS C compiler is broken" #37112). Budget real integration effort; pin the
  vcpkg port versions. The `embedded-libheif` feature compiles libheif from vendored sources but
  **still needs system de265/aom**.

> **HEIC verdict:** **`libheif-rs`** is the only real option — accept the Windows vcpkg cost,
> pin versions, and prefer the embedded `thmb` thumbnail for instant preview.

### 3.6 JPEG XL — pure-Rust today, native DC-scaling only via C

- **`jxl-oxide` (pure Rust) — ship this.** v0.12.x (May 2026), ~100k downloads/mo, active;
  multithreaded via **rayon** (default), uses an internal `jxl_simd` crate. No C deps in the
  default build (optional `lcms2` = C, or pure-Rust `moxcms` for color management).
  - **Lossless JPEG transcode round-trip: supported** — `jpeg_reconstruction_status()` and
    `reconstruct_jpeg(writer)` rebuild the original JPEG bitstream from a JXL that losslessly
    recompressed a JPEG.
  - **Progressive: yes, but no native downscaled buffer.** `render_loading_frame()` /
    `render_loading_frame_cropped()` render the partial frame as bytes stream in, and
    `set_image_region()` / `render_frame_cropped()` give ROI crops. But the **DC-only / 1/8 /
    1/4 downscaled decode is NOT exposed** — issue #78 ("Downsampled rendering") has been open
    since Sept 2023. For JXL you full-decode then resize, *or* use the streaming partial render
    as a coarse preview.
- **`jpegxl-rs` (libjxl C++ bindings) — fallback for native DC scaling.** v0.13.x+libjxl-0.11.2
  (Feb 2026). The **libjxl decoder natively supports downsampled decode at ratios 1, 2, 4, 8** —
  the true "decode the 1/8 DC pass" capability jxl-oxide lacks. Also supports JPEG
  reconstruction. **Windows build: needs CMake + C++ toolchain** (`vendored` feature builds
  libjxl; or point at a prebuilt via `DEP_JXL_LIB` / vcpkg).
- **Speed:** no clean public jxl-oxide-vs-libjxl number found. The strongest datapoint is the
  *official* pure-Rust rewrite **`jxl-rs`** (Chrome 145 behind a flag; Firefox Nightly): "within
  15–25% of the C++ reference" after heavy 2025 optimization — but it's **decoder-only,
  experimental, not on stable crates.io**, and progressive is still unfinished. Not a
  works-today option yet; watch it.

> **JXL verdict:** **`jxl-oxide` now** (pure Rust, zero Windows friction, JPEG-transcode
> round-trip, streaming progressive). Keep **`jpegxl-rs` (vendored)** as a fallback *only if*
> native 1/8 DC decode becomes performance-critical. Watch `jxl-rs`.

### 3.7 TIFF, BMP, QOI

**TIFF — `tiff` (image-rs), pure Rust.** Decode compressions: **None, LZW, Deflate (ZIP),
PackBits, Fax4 (CCITT G4), JPEG-in-TIFF, ZSTD** (encode is narrower). **BigTIFF, multipage, and
incremental decoding supported.** Bit depths 8/16/32/64 incl. **IEEE float 32/64**; photometric
WhiteIsZero/BlackIsZero/RGB(A)/CMYK/palette. **Key limit:** all samples must share one bit depth
(no mixed-depth channels), and there is **no pyramid/overview or tile-level random-access API**
— no scaled/partial decode; you read a full page. Fine for typical files, not a tiled-pyramid
engine.

**BMP — the `image` crate.** Pure Rust, trivial, fast, no scaled decode needed. Nothing to
evaluate.

**QOI — `qoi` (aldanor) vs `rapid-qoi`.** Both pure Rust, `no_std`, no unsafe. QOI is
**inherently serial** (each pixel depends on prior decode state) so SIMD gives little; both are
very fast (~330–427 MP/s; well above the C reference). **Maintenance is the tiebreaker:**
`rapid-qoi` is stuck at v0.6.1 (Feb 2022, unmaintained); **`qoi` is current (v0.4.x, 2025) and
ecosystem-aligned** → use `qoi`. No scaled decode exists for QOI by design.

---

## 4. Embedded thumbnail / preview extraction (the "instant preview" fast path)

Strategy: synchronously extract+decode a small embedded image (sub-millisecond), display it
immediately, then swap in the full decode from the pool. **Formats that reliably carry a usable
embedded preview today: JPEG (EXIF/IFD1 + MPF), HEIC (`thmb`).** JXL is experimental.

- **JPEG EXIF/IFD1 thumbnail — `kamadak-exif` (crate `exif`). Best, simplest path.** Parses EXIF
  (incl. IFD1, the thumbnail IFD). Read `Tag::JPEGInterchangeFormat` (offset) and
  `Tag::JPEGInterchangeFormatLength` (length) from `In::THUMBNAIL`. **Caveat:** the offset is
  relative to the TIFF header *inside* the EXIF blob, and the crate doesn't hand you the bytes —
  keep the raw EXIF buffer and slice `buf[offset..offset+len]`, yielding a standalone baseline
  JPEG (~160 px) you decode normally. Reliable on essentially all camera/phone JPEGs.
- **JPEG MPF (Multi-Picture Format, APP2) — larger preview, near-full-res when present.** No
  mature dedicated crate. Pragmatic option today: **`gainforge`** (awxkee) exposes
  `MpfInfo::from_bytes` / `MpfEntry` / `MpfTag` that parse the APP2 MPF index and return offsets
  to the embedded images (the "Large Thumbnail"/preview, then MPImage#N). New/niche (built for
  Ultra-HDR gain maps) but real Rust MPF parsing. Alternative: pull the APP2 "MPF" segment bytes
  with **`img-parts`** and parse the TIFF-structured index by hand (ExifTool MPF tag table
  documents the layout).
- **`img-parts` — container surgery, no decode.** Read/write JPEG/PNG/RIFF-WebP segments + high-
  level `exif()` / `icc_profile()`. Use it to extract the APP1 EXIF blob (feed to kamadak-exif),
  APP2 MPF, or APP2 ICC. Returns raw bytes only; doesn't decode or locate thumbnails itself.
- **`little_exif` — not for this.** Pure-Rust EXIF read/write but limited tag set, no
  thumbnail-extraction focus. Skip for previews.
- **HEIC/AVIF `thmb` item — `libheif-rs`, works today.** `ImageHandle::number_of_thumbnails()` +
  `get_thumbnail()` returns the separate `thmb` handle you decode independently of the primary
  image — exactly the fast path. (AVIF thumbnail items exist but are rarer.)
- **JPEG XL preview — experimental.** JXL can flag a dedicated preview frame, and every JXL has
  an inherent 1/8 DC pass; `jxl-oxide` can render an early/partial frame, but there's no clean
  one-call "give me the preview" API and embedded preview frames are rare in practice. Treat as
  experimental.

---

## 5. Decode abstraction & architecture

### 5.1 The `image` umbrella vs hand-picked per-format crates

The old "`image` uses the slow `jpeg-decoder`" criticism is **outdated**: `image` 0.25.x decodes
JPEG via **`zune-jpeg`** (within ~10 ms of libjpeg-turbo). `jpeg-decoder` is now only pulled
transitively (e.g. by `tiff` for JPEG-in-TIFF). Real costs of the umbrella that matter to
PhotoBlaze:

1. **No scaled-decode fast path.** `image` decodes to full resolution then you `resize()`. It
   exposes **no DCT 1/2·1/4·1/8 decode** and no WebP `use_scaling` — the single biggest win for
   thumbnailing large JPEG/WebP is only reachable by calling `turbojpeg` / `libwebp-sys`
   directly.
2. **Hidden backend knobs.** `DynamicImage`/auto-detection hides backend-specific controls and
   blocks embedded-thumbnail/MPF access.
3. **Marginal extra effort to go direct.** Per-format crates (zune-jpeg, jxl-oxide, libheif-rs)
   are not much harder to call directly and give full control.

**Recommendation:** keep `image` for breadth/correctness and rare formats (BMP, TIFF, ICO, GIF),
but for the **hot formats (JPEG, WebP, HEIC, AVIF)** call the backends directly and pair with
SIMD `fast_image_resize` for any non-native downscale.

### 5.2 Decode trait — make backends A/B-benchmarkable

Define a small trait so every backend returns a uniform decoded buffer + metadata:

```rust
pub struct Decoded {
    pub pixels: Vec<u8>,       // RGBA8 (or a Cow / Arc<[u8]>)
    pub width: u32,
    pub height: u32,
    pub orientation: Orientation,   // EXIF
    pub icc: Option<Vec<u8>>,
}

pub struct DecodeRequest {
    pub target: Option<(u32, u32)>, // desired max dims → pick DCT/scaling factor
}

pub trait Decoder {
    fn decode(&self, bytes: &[u8], req: &DecodeRequest) -> Result<Decoded>;
}
```

- Use **enum dispatch** for the closed backend set on the hot path
  (`match Backend { ZuneJpeg, TurboJpeg, Dav1dAvif, Libheif, JxlOxide, Png, … }`, optionally via
  the `enum_dispatch` crate). It inlines, avoids the vtable + heap of `Box<dyn>`, and lets you
  enumerate variants — perfect for swapping `Backend::ZuneJpeg` vs `Backend::TurboJpeg` under
  `criterion`. (In absolute terms a vtable call is negligible against multi-ms decode; the real
  reason to prefer enums here is ergonomics + the ability to enumerate.)
- Reserve `dyn Decoder` only if you later want open/plugin extensibility.
- Thread `DecodeRequest.target` into each backend so it can choose `turbojpeg` `ScalingFactor`,
  `libwebp` `use_scaling`, `jpeg-decoder.scale()`, or "decode-then-fast_image_resize".

### 5.3 Threading model — dedicated prioritized pool, not bare rayon

Rayon is data-parallel work-stealing on a **single global pool with no priorities** (spawns are
only relative-FIFO; priorities require separate pools), and work-stealing reorders execution —
both poor fits for "decode current image first, then neighbors". For PhotoBlaze prefer a
**dedicated bounded worker pool fed by a priority queue** (`BinaryHeap` behind `Mutex`+`Condvar`,
or crossbeam channels), with:

- **Priority** = current image ≫ adjacent neighbors ≫ further-out prefetch.
- **Cancellation tokens** so images scrolled past are dropped and resident memory stays bounded.
- N ≈ physical cores worker threads; **one decode per thread** (since single-image decode is
  serial for every format). You can still let an *individual* decode use rayon internally
  (jxl-oxide, jpeg-decoder post-processing) — but keep the *scheduler* on your own prioritized
  pool so background prefetch never starves the interactive current-image decode.

---

## 6. macOS (Apple-Silicon) portability notes

- **Pure-Rust crates port trivially** — no build changes: `png`, `zune-png`, `zune-jpeg`
  (NEON-by-default), `image-webp`, `jxl-oxide`, `tiff`, `qoi`, `kamadak-exif`, `img-parts`,
  `fast_image_resize`. These are the easy 80%.
- **SIMD carries over:** zune-jpeg ships NEON by default; libjpeg-turbo, libwebp, dav1d, and
  libde265 all have NEON paths, so the C libraries are *fast* on M-series — the issue is only
  *building* them.
- **C-library build differs by platform, not in Rust glue:**
  - `turbojpeg` / `mozjpeg`: NASM is x86-only; on ARM64 they use the GNU assembler (`gas`) path
    — generally smoother to build on macOS than the Windows NASM dance.
  - `dav1d` crate: needs libdav1d via pkg-config; on macOS install via **Homebrew** (`brew
    install dav1d`) — much smoother than vcpkg on Windows.
  - `libheif-rs`: **Homebrew `libheif`** is a single clean formula — dramatically easier than
    the Windows vcpkg chain. This is the format where macOS is *much* less painful.
  - `jpegxl-rs`: Homebrew `jpeg-xl`, or the vendored CMake build.
- **Net:** Windows is the hard platform for the C-dependent formats (AVIF/HEIC/optional
  turbojpeg/jpegxl). The macOS port's friction is *lower*, so design the build around the
  Windows constraints and macOS follows.

---

## 7. Recommendations (ranked)

**Primary stack (works today):**

1. **JPEG → `turbojpeg`** (DCT scaled decode 1/2·1/4·1/8 + SIMD). Pure-Rust A/B alternative:
   **`zune-jpeg`** + `fast_image_resize`.
2. **PNG/APNG → `png` (image-rs)** with the `zlib-rs` backend. Pure Rust, fastest, zero
   friction.
3. **WebP → `libwebp-sys`** (`use_scaling` downscale-on-decode + SIMD, `cc` build no CMake).
   Pure-Rust fallback: `image-webp`.
4. **JPEG XL → `jxl-oxide`** (pure Rust, JPEG-transcode round-trip). Fallback for native 1/8 DC:
   `jpegxl-rs`.
5. **TIFF → `tiff`**, **BMP → `image`**, **QOI → `qoi`** — all pure Rust, zero friction.
6. **AVIF → `dav1d` crate + `avif-parse`** (or `libavif-sys` for batteries). Expensive decode;
   parallelize across images; use container thumbnail for preview.
7. **HEIC → `libheif-rs`** (only real option; budget the Windows vcpkg cost; use `thmb`
   thumbnail).

**Embedded preview fast path:** `kamadak-exif` (JPEG IFD1) + `gainforge`/`img-parts` (JPEG MPF) +
`libheif-rs` (HEIC `thmb`).

**Architecture:** thin `Decoder` trait + **enum dispatch**; a **dedicated priority-queue worker
pool** with cancellation (not bare rayon); use `image` only for the long-tail formats.

**Scaled-decode priority order** (where it helps most): **JPEG (huge) > WebP (large) > JXL
(if jpegxl-rs) >> everything else (resize-after or thumbnail).**

**Build-friction tiers:** Tier 0 (no C): png, zune-jpeg, zune-png, image-webp, jxl-oxide, tiff,
qoi, kamadak-exif. Tier 1 (C compiler, easy): libwebp-sys (`cc`), turbojpeg via prebuilt. Tier 2
(CMake/NASM/meson): turbojpeg-from-source, mozjpeg, dav1d, libavif-sys, jpegxl-vendored. **Tier 3
(painful on Windows): libheif-rs (vcpkg chain).**

---

## 8. Open questions

1. **No clean public `jxl-oxide` vs `libjxl` decode benchmark** was found — measure on
   representative JXL files before deciding whether the `jpegxl-rs` native-DC path is worth its
   build cost.
2. **Does `turbojpeg`'s pkg-config/explicit prebuilt path on Windows reliably avoid NASM** for a
   CI/distribution build, or does shipping a prebuilt libjpeg-turbo binary become the cleaner
   route? Needs a Windows build trial.
3. **AVIF tile-threading payoff for real-world stills** — most single-tile camera AVIFs see ~0
   speedup; confirm on a sample corpus whether any speedup justifies enabling dav1d threads
   per-image vs reserving all cores for cross-image parallelism.
4. **`rav1d-safe` trajectory** — at ~1.25–1.56× slower it's not ready, but a pure-Rust AVIF path
   would eliminate the worst remaining Windows build dep; re-evaluate periodically.
5. **MPF parsing maturity** — `gainforge`'s MPF API is new/niche; validate it (or a hand-rolled
   APP2 parser) against a corpus of phone/camera JPEGs that carry full-res MPF previews.
6. **HEIC on Windows** — pin a known-good vcpkg port set (libheif + libde265 + libaom + x265 +
   libogg); decide between vcpkg vs shipping prebuilt libheif DLLs.
7. **image-png SIMD defilter** — the `std::simd` paths aren't runtime-enabled by default yet;
   track when they land for a free PNG speedup.

---

## 9. Sources

**JPEG**
- https://docs.rs/turbojpeg/latest/turbojpeg/struct.Decompressor.html
- https://raw.githubusercontent.com/honzasp/rust-turbojpeg/master/src/decompress.rs (ScalingFactor constants, `set_scaling_factor`, `DecompressHeader::scaled`)
- https://raw.githubusercontent.com/honzasp/rust-turbojpeg/master/turbojpeg-sys/README.md (NASM/CMake/pkg-config build)
- https://crates.io/crates/turbojpeg
- https://github.com/libjpeg-turbo/libjpeg-turbo/blob/main/BUILDING.md
- https://lib.rs/crates/zune-jpeg , https://github.com/etemesi254/zune-image , https://github.com/Shnatsel/zune-jpeg
- https://docs.rs/jpeg-decoder/latest/jpeg_decoder/struct.Decoder.html , https://github.com/image-rs/jpeg-decoder
- https://raw.githubusercontent.com/kornelski/mozjpeg-rust/main/src/decompress.rs , https://github.com/kornelski/mozjpeg-sys
- https://github.com/developer0hye/libjpeg-turbo-rs
- https://github.com/image-rs/image/issues/1845

**PNG / WebP**
- https://blog.image-rs.org/2026/06/18/png-adoption.html (benchmark geomeans; Chromium M139; GNOME 49; miniz_oxide/zlib-rs/fdeflate)
- https://www.phoronix.com/news/Rust-PNG-Outperforms-C-PNG
- https://docs.rs/zune-png , https://github.com/image-rs/fdeflate
- https://github.com/aloucks/spng-rs , https://libspng.org/docs/build/
- https://github.com/richgel999/fpng , https://crates.io/crates/fpng-rs
- https://lib.rs/crates/libwebp-sys , https://github.com/qnighy/libwebp-sys2-rs , https://docs.rs/webp
- https://lib.rs/crates/image-webp , https://github.com/image-rs/image-webp , https://github.com/imazen/zenwebp
- https://developers.google.com/speed/webp/docs/api , https://chromium.googlesource.com/webm/libwebp/+/HEAD/doc/api.md

**AVIF / HEIC**
- https://crates.io/crates/dav1d , https://github.com/rust-av/dav1d-rs , https://lib.rs/crates/dav1d
- https://crates.io/crates/avif-parse , https://crates.io/crates/avif-decode , https://crates.io/crates/aom-decode
- https://github.com/njaard/libavif-rs , https://docs.rs/crate/libavif-sys/latest
- https://github.com/memorysafety/rav1d , https://www.memorysafety.org/blog/rav1d-performance-optimization/ , https://ohadravid.github.io/posts/2025-05-rav1d-faster/
- https://github.com/imazen/rav1d-safe , https://crates.io/crates/rav1d-safe
- https://docs.rs/crate/libheif-rs/latest , https://github.com/Cykooz/libheif-rs , https://github.com/strukturag/libheif/blob/master/examples/heif_thumbnailer.cc
- vcpkg Windows build issues: https://github.com/microsoft/vcpkg/issues/43079 , /42499 , /32216 , /43416 , /37112 ; https://github.com/strukturag/libheif/issues/1162
- https://code.videolan.org/videolan/dav1d/-/issues/398 (single-tile threading)

**JPEG XL / TIFF / QOI**
- https://github.com/tirr-c/jxl-oxide , https://docs.rs/jxl-oxide/latest/jxl_oxide/struct.JxlImage.html , https://lib.rs/crates/jxl-oxide , https://github.com/tirr-c/jxl-oxide/issues (issue #78 downsampled rendering)
- https://docs.rs/jpegxl-rs , https://github.com/inflation/jpegxl-rs , https://libjxl.readthedocs.io/en/latest/api_decoder.html (downsampling ratios 1/2/4/8)
- https://github.com/libjxl/jxl-rs , https://www.corewebvitals.io/pagespeed/jpeg-xl-core-web-vitals-support
- https://github.com/image-rs/image-tiff , https://blog.image-rs.org/2021/02/10/hindsight-on-2020.html
- https://github.com/aldanor/qoi-rust , https://github.com/zakarumych/rapid-qoi , https://deepwiki.com/wx257osn2/qoi-benchmark/3-qoi-implementations

**Embedded previews / architecture**
- https://docs.rs/kamadak-exif , https://github.com/kamadak/exif-rs
- https://docs.rs/img-parts/latest/img_parts/ , https://github.com/paolobarbolini/img-parts
- https://docs.rs/little_exif/latest/little_exif/
- https://crates.io/crates/gainforge , https://docs.rs/gainforge/latest/gainforge/ , https://exiftool.org/TagNames/MPF.html
- https://docs.rs/libheif-sys/latest/libheif_sys/fn.heif_image_handle_get_thumbnail.html
- https://github.com/image-rs/image/issues/2289 (image → zune-jpeg; transitive jpeg-decoder via tiff)
- https://docs.rs/enum_dispatch , https://www.somethingsblog.com/2025/04/20/rust-dispatch-explained-when-enums-beat-dyn-trait/
- https://pkolaczk.github.io/multiple-threadpools-rust/ , https://users.rust-lang.org/t/dealing-with-work-priority-and-rayon/30954 , https://github.com/rayon-rs/rayon/blob/main/FAQ.md , https://gendignoux.com/blog/2024/11/18/rust-rayon-optimized.html
- https://crates.io/crates/fast_image_resize

---

*Compiled June 2026. "Works today" reflects crate versions current as of Q1–Q2 2026; the
fastest-moving items (jxl-rs, rav1d-safe, image-png SIMD defilter) are flagged experimental and
should be re-checked.*
