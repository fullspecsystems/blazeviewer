# Prior Art: Fast Photo Viewers — Techniques to Steal for PhotoBlaze

Research date: 2026-06-26. Scope: how the fastest existing image viewers/tools achieve speed,
and which techniques PhotoBlaze (Rust, Windows-primary, GPU-resident, decode-ahead) should adopt.
Citations are inline. Where a viewer is open source, claims marked **(code-verified)** come from
reading the actual source.

---

## Overview

Every fast viewer converges on the same core idea, stated four different ways:

> **Never decode more pixels than the current view needs, and never decode them on the thread
> that has to stay responsive. Show *some* correct pixels instantly, then refine.**

The concrete mechanisms cluster into six families:

1. **Embedded-preview-first decode** — display the camera/EXIF JPEG (or a pyramid level) that
   already exists in the file, instead of demosaicing/decoding the full frame. This is the single
   biggest "feels instant" trick, used by Photo Mechanic, Lightroom, macOS ImageIO, XnView,
   IrfanView, FastStone, nomacs.
2. **Asymmetric browse-vs-view pipelines** — a cheap browse path (thumbnails + embedded previews +
   metadata without full decode) separate from an expensive full-decode view path triggered only on
   explicit open/zoom.
3. **Off-thread decode + off-thread scale** — the UI thread only ever blits a small, already-sized
   bitmap/texture; decode and high-quality resample happen on worker threads/pools.
4. **Bounded read-ahead / prefetch** — speculatively load neighbors in the navigation direction
   (usually ±1, occasionally a deeper forward-biased window), discard speculation on direction change.
5. **Two-tier caching** — a tiny hot RAM cache of decoded frames (often just current±1) plus a
   persistent on-disk thumbnail/preview database (SQLite or freedesktop PNGs) so re-browsing is instant.
6. **GPU-resident rendering (the frontier)** — decode once, upload once to a GPU texture, do all
   scale/zoom/pan/color as shader passes. Almost no general-purpose still viewer does this today
   (mpv/libplacebo and imv are the exceptions). **This is PhotoBlaze's main opportunity to beat
   everything else.**

A striking finding across the survey: **the famously "fast" Windows viewers (JPEGView, IrfanView,
FastStone, XnView) are all CPU/GDI with no GPU at all**, and most are single-threaded for
single-image decode. Their speed is low overhead + libjpeg-turbo + embedded previews + good caching.
A GPU-resident, multi-threaded Rust viewer has wide-open headroom.

---

## Per-viewer findings

### Photo Mechanic (Camera Bits) — the pro-culling speed king

**Why it's fast:** It does *not* process RAW like a RAW editor. It reads the **JPEG the camera
already embedded inside the RAW** and displays that for both thumbnails and large previews.
Extracting a pre-made 1–2 MB JPEG is near-instant vs. demosaicing a 45–100 MP RAW, which is why you
can hold the arrow key and flick through huge files with zero lag. RAW pixels are only rendered on
explicit demand. (https://docs.camerabits.com/support/solutions/articles/48001146200-caching-preferences,
https://imagen-ai.com/valuable-tips/photo-mechanic-vs-lightroom-culling/)

- **Two-tier cache, treated as disposable:** disk cache (user-located, kept off Spotlight-indexed
  paths; usefulness plateaus "at a few thousand MB") + memory cache (~10% of RAM recommended) + a
  sort cache. Crucially: *"Photo Mechanic generates thumbnails and previews quickly so it is not
  necessary to keep the cache between sessions"* — it offers Empty-on-Quit and age-out specifically
  because a large persistent cache *slows launch*. Because regeneration is cheap (embedded JPEG), the
  cache is deliberately lean.
  (https://docs.camerabits.com/support/solutions/articles/48001146200-caching-preferences)
- **Preview-then-refine:** a separate Render Cache + "Enable RAW Rendering" (built-in or via Adobe
  DNG Converter) produces true RAW previews on demand and caches them, for cases where embedded
  quality isn't enough (e.g. HIF).
  (https://docs.camerabits.com/support/solutions/articles/48001252598-render-cache-preferences)
- **Overlap I/O with interaction:** ingest is non-blocking — you cull a contact sheet as soon as the
  first images land while the rest copy in the background (and it copies to two destinations at once).
  (https://docs.camerabits.com/support/solutions/articles/48000207409-ingesting-photos-with-photo-mechanic)
- **Trade-off to design around:** the embedded JPEG is the camera's processed render, so deep zoom
  can look soft and won't match the eventual RAW develop.
  (https://havecamerawilltravel.com/workflow/extract-jpg-raw-2/)

**Most valuable lesson:** *Treat the embedded camera JPEG as the primary display image, and treat the
cache as disposable.* Showing a pre-made small JPEG (RAW render only on demand) is what makes culling
feel instant.

### Adobe Lightroom Classic — preview pyramids and the "loading…" tax

**Why it's fast (when it is):** each image has a **pyramid of JPEGs** from thumbnail up to loupe size;
Lightroom serves the smallest level that satisfies the current view. The visible "loading…" stall is
exactly the cost of an on-the-fly render when no appropriately-sized level is cached.
(https://www.lightroomqueen.com/community/threads/lightroom-previews-smart-previews.25096/)

- **Preview tiers:** Minimal (tiny embedded thumbnail) → Standard (browse-sized) → 1:1 (full-res, for
  pixel-peeping) → Embedded & Sidecar (camera JPEG directly) → Smart Preview.
  (https://www.lightroomqueen.com/what-is-the-difference-between-minimal-previews-standard-previews-and-11-previews/)
- **On-disk structure (`Previews.lrdata`, reverse-engineered):** `previews.db` (11 tables) +
  `root-pixels.db` index small thumbs; each `.lrprev` is a container of blocks prefixed `AgHg`, with
  up to **7 pyramid levels (1:16…1:1), and every level's block is a fully valid JPEG**. Files keyed
  `{uuid}-{digest}.lrprev`. (https://www.seachess.net/notes/dive-into-lightroom-catalogues/)
- **Camera Raw cache** (separate from previews): holds partially-processed "fast-load" data for recent
  RAWs so Develop skips early decode stages; default 5 GB, big wins at 20 GB+, recommended on NVMe.
  (https://helpx.adobe.com/lightroom-classic/kb/optimize-performance-lightroom.html)
- **Smart Preview:** a compressed lossy DNG resized to ~2560 px long edge, fully editable in real time
  with the original offline. (https://helpx.adobe.com/lightroom-classic/help/lightroom-smart-previews.html)
- **Embedded fast path on import** (LrC 7.0, 2017): "Embedded & Sidecar" displays the camera JPEG —
  reportedly ~90% faster import (1000 RAWs in ~3 min vs ~30). Caveat: DSLRs embed full-size JPEGs,
  many mirrorless bodies embed small previews, so loupe quality varies.
  (https://darkroomphotos.com/lightroom-classic-new-embedded-previews/)
- **Anti-pattern it exhibits:** the Develop module *always* bypasses cached previews and re-renders
  from the original RAW — which is why Develop lags even when Library is instant.

**Most valuable lesson:** *Build a persisted multi-resolution preview pyramid up front and always serve
the smallest level that satisfies the view; a "loading…" stall is an on-demand render you failed to
pre-cache.*

### Apple Preview / macOS Quick Look / ImageIO — the embedded-thumbnail primitive

**Why it's fast:** the actual primitive is **ImageIO `CGImageSourceCreateThumbnailAtIndex`**, which
**returns the file's embedded thumbnail/preview when present** and only synthesizes from the full image
when told to. Benchmark: thumbnailing a 12 MP JPEG ≈ **26 ms, ~30× faster** than naive
full-decode-then-resize.
(https://mjtsai.com/blog/2026/04/19/fast-thumbnails-with-cgimagesource/,
https://developer.apple.com/documentation/imageio/cgimagesourcecreatethumbnailatindex(_:_:_:))

- Key flags to replicate: `…ThumbnailFromImageIfAbsent` (make one only if no embedded preview),
  `…MaxPixelSize` (cap long edge), `…ThumbnailWithTransform` (apply EXIF orientation),
  `…FromImageAlways` (force full decode — the slow path to avoid).
  (https://developer.apple.com/documentation/imageio/kcgimagesourcecreatethumbnailfromimageifabsent)
- **Quick Look daemon** layers a multi-GB in-memory cache (retained ~2 days, purged under pressure) →
  on-disk cache → fresh generation in sandboxed per-type helpers, with **request coalescing/dedup** so
  concurrent browse requests collapse.
  (https://eclecticlight.co/2026/05/16/explainer-quicklook/)
- RAW/CR3 embed previews too (CR3 HDR-PQ embeds HEVC previews); failures happen when the embedded
  format is unexpected — a reminder to have a robust fallback decode path.

**Most valuable lesson:** *Make embedded-thumbnail extraction the default fast path
(pull embedded preview → cap with max-pixel-size → apply EXIF orientation → full decode only if
absent), then layer memory→disk caching with request coalescing on top.*

### JPEGView (Windows, open source, `sylikc/jpegview`) — the architecture to copy

The most important open-source reference. Speed comes from a tight, well-separated pipeline:
**one background decode thread (libjpeg-turbo) + a tiny 2-slot direction-aware read-ahead/LRU + a
hand-written SIMD resampler on a strip-parallel thread pool + a cached display-DIB so repaints are
free, blitted via plain GDI. No GPU.** **(code-verified)**

- **Decode:** JPEG via statically-linked **libjpeg-turbo** (`TurboJpeg::ReadImage()`), falling back
  to GDI+ then WIC; other formats via bundled libpng-apng/libwebp/libjxl/libheif/libavif/libraw/qoi.
  (https://github.com/sylikc/jpegview/blob/master/src/JPEGView/ImageLoadThread.cpp)
- **Threading:** decode on **exactly ONE** background worker
  (`new CJPEGProvider(m_hWnd, NUM_THREADS=1, READ_AHEAD_BUFFERS=2)`); completion posts
  `WM_IMAGE_LOAD_COMPLETED` to the UI.
  (https://github.com/sylikc/jpegview/blob/master/src/JPEGView/MainDlg.cpp)
- **Read-ahead:** `CJPEGProvider` auto-issues a prefetch for the next image in the current direction
  (`enum EReadAheadDirection { NONE, FORWARD, BACKWARD, TOGGLE }`); **on direction reversal the
  speculative decode is discarded** ("the strategy was wrong"). Window = 1 (current + one neighbor).
  (https://github.com/sylikc/jpegview/blob/master/src/JPEGView/JPEGProvider.cpp)
- **Cache:** `std::list<CImageRequest*>` bounded to ~2, **LRU by `AccessTimeStamp`**, evicting the
  oldest ready-but-unused image; reserves one free buffer; `FreeAllPossibleMemory()` under pressure.
  Deliberately tiny decoded-image cache.
- **Second-level cache (key trick):** each image keeps original pixels **and** a processed
  display-resolution DIB (`m_pDIBPixels`); `GetDIB()` with the same params returns the cached result
  with no reprocessing — so **pan/repaint at fixed zoom never re-resamples**.
  (https://github.com/sylikc/jpegview/blob/master/src/JPEGView/JPEGImage.h)
- **Resampler:** separable two-pass HQ downscale (Lanczos option, sharpen 0.3) + bicubic upscale,
  with three hand-written SIMD paths (MMX/SSE/AVX2), **parallelized across up to 4 cores** by
  `CProcessingThreadPool` (each thread does a horizontal strip).
  (https://github.com/sylikc/jpegview/blob/master/src/JPEGView/ProcessingThreadPool.cpp)
- **Notable non-optimizations:** decodes JPEG to **full resolution then downsamples** (no DCT-scaled
  decode-to-fit); does **not** use the embedded EXIF thumbnail for first paint; **GDI CPU blit, RAM
  only, no VRAM**. (The `QuickView` fork rewrote it in Direct2D specifically to add GPU —
  https://github.com/justnullname/QuickView.)

**Most valuable lesson:** *Separate the pipeline into independent stages and cache at every boundary*
(decode thread → bounded prefetch LRU → SIMD strip-parallel resampler → cached display surface).
Copy this shape in Rust and add the GPU blit it lacks.

### IrfanView (Windows, closed source) — "fast" = minimal overhead

Profile: **single-threaded, CPU/GDI, libjpeg-turbo, plugin-DLL decoders, ~8 MB binary, on-demand
decode with no view prefetch.** Its speed is "small and does almost nothing extra."

- libjpeg-turbo for JPEG; built-in common formats; exotic formats are bitness-locked plugin DLLs
  (WebP/HEIF/AVIF/RAW). (https://www.irfanview.com/faq.htm, https://www.irfanview.com/plugins.htm)
- **Viewer is single-threaded**; multithreading exists only for *batch conversion* (added v4.72, 2025)
  — single-image viewing stayed single-threaded for ~29 years, i.e. by design.
  (https://www.irfanview.com/history_old.htm)
- **No read-ahead / no decoded-frame cache for viewing** — "preload next image" is a perennial
  unfulfilled feature request; next/prev feels fast only because decode is fast.
  (https://irfanview-forum.de/forum/program/feature-requests/998-)
- Embedded fast paths: "Try to load EXIF-Thumbnail for JPG files" for the thumbnail browser; RAW
  prefers embedded preview (full RAW load = "very slow"). **No persistent thumbnail database.**
- **CPU/GDI only**; developer says GPU "would require completely rewriting the internals."
  (https://irfanview-forum.de/forum/program/feature-requests/91941-gpu-acceleration)

**Most valuable lesson:** *Most of "fast" is just low overhead + a fast SIMD decoder, so the bar is
low and explicit.* Keep the minimalism; add the things its design never could — read-ahead with a
decoded-frame cache, a persistent thumbnail cache, and GPU display/zoom.

### FastStone Image Viewer (Windows, closed source, Delphi) — pre-warmable thumbnail DB

The one big concrete speed feature is a **persistent, pre-scannable on-disk thumbnail database**.

- `FSViewer.db`/`FSSettings.db` under `%AppData%\FastStone\FSIV\`; revisiting a folder is instant,
  first visit builds it. **v7.6 added a new DB engine + a "Pre-scan folders into thumbnail database"
  tool** to warm the cache before the user opens a folder.
  (https://www.faststone.org/FSViewerDetail.htm)
- RAW uses the **embedded JPEG preview** for fast display ("especially noticeable for large images").
- Lanczos resampling with a user-exposed quality knob (Lanczos / Lanczos_Softer / Bilinear).
- Read-ahead/prefetch of adjacent images is **not documented** (treat as your own design choice);
  almost certainly CPU resample + GDI blit, no GPU.

**Most valuable lesson:** *Win perceived speed with a pre-warmable persistent thumbnail/preview
database, and always paint the cheapest correct pixels first* (embedded previews before full decode).

### XnView / XnViewMP (Windows, closed source) — embedded-preview browse + SQLite thumbs

- **In-house GFL decode engine** (~500 read formats; sold separately as the GFL SDK); can read
  EXIF/IPTC and **embedded thumbnails without decoding the full image** — the API enabler of fast
  browse. Video=FFmpeg, PDF=Ghostscript, exotic codecs as copy-in plugins.
  (https://en.wikipedia.org/wiki/XnView, https://www.xnview.com/wiki/index.php?title=GFL_SDK)
- **Thumbnail DB = SQLite** (`Thumb.db` for thumbs, can hit multiple GB; `XnView.db` for
  categories/ratings), with **configurable lossy JPEG or WebP compression** to pack more thumbs per
  byte; thumbnails generated lazily following the scroll slider.
  (https://newsgroup.xnview.com/viewtopic.php?t=42338, https://newsgroup.xnview.com/viewtopic.php?t=21149)
- **Single-threaded decode/thumbnailing** (dev-confirmed) — its exploitable weakness. User benchmark:
  2000 JPEG thumbnails ≈ 12 s (multithreaded nomacs) vs **35–55 s** (XnView).
  (https://newsgroup.xnview.com/viewtopic.php?t=43291)
- **Read-ahead = 1 image; no GPU; CPU Lanczos; everything in RAM** (dev-confirmed "No" to GPU).
  Notable limitation: cannot use embedded preview for browse *and* full-res for view simultaneously —
  exactly the asymmetric pipeline to get right.
  (https://newsgroup.xnview.com/viewtopic.php?f=82&t=45024)

**Most valuable lesson:** *Build two asymmetric pipelines* — a browse path that almost never full-
decodes (SQLite-cached thumbs fed by embedded previews + metadata-without-decode) and a separate
full-decode view path — then **parallelize** thumbnailing/decode across all cores (the thing XnView
never did; the 12 s vs 55 s gap is entirely threading).

### nomacs (Qt, open source, `nomacs/nomacs`) — decouple I/O from decode

A clean layered pipeline: `DkImageLoader` (nav/cache) → `DkImageContainerT` (threaded per-image) →
`DkBasicLoader` (decode) → `DkImageStorage` (full + async-scaled display copy) → `DkViewPort`
(QPainter render). **(code-verified)** (https://deepwiki.com/nomacs/nomacs/2.2-image-loading-system)

- **Two-stage off-GUI loading** via `QtConcurrent::run` + `QFutureWatcher`, with **separate watchers
  for file-read vs decode** — so it can prefetch *bytes* far ahead while only *decoding* the immediate
  neighbor. (https://github.com/nomacs/nomacs/blob/master/ImageLounge/src/DkCore/DkImageContainer.cpp)
- **Forward-biased two-tier read-ahead:** fully decode `cIdx+1` if within budget; merely buffer bytes
  (`fetchFile()`, no decode) further ahead up to the window; previous image retained but not
  pre-decoded. (https://github.com/nomacs/nomacs/blob/master/ImageLounge/src/DkCore/DkImageLoader.cpp)
- **Cache = positional sliding window + MB budget** (not classic LRU): decoded pixels retained only
  for current±1 (`if(abs(cIdx-idx)>1) cImg->clear();`). Defaults: `cacheMemory=256 MB`,
  `maxImagesCached=5`, `maxThumbSize=256`, `loadRawThumb=raw_thumb_always`.
  (https://github.com/nomacs/nomacs/blob/master/ImageLounge/src/DkCore/DkSettings.cpp)
- **Embedded EXIF thumbnail for instant preview** via Exiv2 `ExifThumb` before any full decode; RAW
  prefers embedded JPEG.
- **CPU/QPainter render** (no OpenGL). The smoothness trick is `DkImageStorage`: holds full-res
  `mOriginal` + an async-computed screen-res `mScaled` (multi-step halving then OpenCV `INTER_AREA`),
  so the viewport always blits a small pre-scaled image and never blocks on resample.
  (https://github.com/nomacs/nomacs/blob/master/ImageLounge/src/DkCore/DkImageStorage.cpp)
- **Persistent thumbnail cache** (freedesktop spec, MD5-of-URI keyed, atomic writes, written back only
  if generation took ≥10 ms) on a **dedicated under-subscribed pool (`idealThreadCount-2`)** so bulk
  thumbnailing never starves the UI. (Does *not* decode-to-fit on read — issue #1269 tracks that.)

**Most valuable lesson:** *Split the pipeline into cheap (I/O) and expensive (decode/scale) stages and
pay the expensive stage only for the image about to show* — embedded-preview-first, two-tier prefetch
(buffer bytes ahead vs decode one ahead) under a RAM budget with a tight current±1 retention window,
plus a separate async screen-sized "display mip" the render loop always blits.

### qimgv (Qt + OpenGL, open source, `easymodo/qimgv`) — scheduling beats GPU

Critical finding: the still-image fast path is **NOT GPU-rendered** (OpenGL is video-only via libmpv).
Perceived speed comes from **threaded decode/scale + a tiny pinned cache + queue-jumping
prioritization.** **(code-verified)**

- **Stills render via `QGraphicsView` (CPU raster)** with two pixmap items; OpenGL (`QOpenGLWidget`)
  is used only by the libmpv video plugin.
  (https://raw.githubusercontent.com/easymodo/qimgv/master/qimgv/gui/viewers/imageviewerv2.cpp)
- **Three independent thread pools:** Loader (decode) capped at **2**, Scaler at **1**, Thumbnailer at
  **4** (capped to hw). Decoded RAM ≈ 3 images; no global MB budget.
- **The real smoothness lever — queue-jumping/preemption:** `loadAsyncPriority()` calls `clearPool()`
  which `QThreadPool::tryTake`s stale not-yet-started preloads and runs the wanted image at priority 1,
  so fast scrubbing never stalls behind old work.
  (https://raw.githubusercontent.com/easymodo/qimgv/master/qimgv/components/loader/loader.cpp)
- **Tiny pinned cache:** `QMap` of `shared_ptr<Image>` guarded by semaphores; eviction is external —
  keep-list = {current, prev, next}, `trimTo(...)` → **≤3 decoded images regardless of folder size**.
  Prefetch = ±1 both directions, hard-coded.
- **Instant-then-sharp swap:** the viewer instantly shows a cheap view-transformed pixmap, then swaps
  in an OpenCV-sharpened (`INTER_AREA`/`CUBIC` + unsharp) rescaled pixmap once motion settles / scale
  crosses a threshold; a single-thread Scaler uses "latest wins" to discard intermediate zoom states.
  (https://raw.githubusercontent.com/easymodo/qimgv/master/qimgv/components/scaler/scaler.cpp)

**Most valuable lesson:** *Perceived browsing speed comes from scheduling, not GPU.* Decode and scale
on separate pools; keep a deliberately tiny pinned cache (current±1); **let the currently-viewed image
jump the queue (preempt/drop stale preloads)**; show a fast placeholder instantly and swap in the HQ
version once motion settles.

### mpv as an image viewer + libplacebo — the GPU model to emulate

mpv's default renderer is fully GPU-based; people repurpose it ("mvi") for its scaling/color pipeline.
**Decode once → upload once to a GPU texture → every redraw (pan/zoom/vsync) samples the resident
texture.** (https://github.com/occivink/mpv-image-viewer)

- **Image = a one-frame video** held open via `--image-display-duration=inf` ("the file is kept open
  forever … should not use any resources during playback") so the decoded frame stays resident, never
  re-decoded. (https://mpv.io/manual/master/)
- **GPU backends:** default VO `gpu-next` (built on **libplacebo**), selectable `--gpu-api` =
  opengl / **vulkan** / **d3d11**. Async uploads via `--opengl-pbo` (helps for 4K+). HW decode can
  decode straight into GPU memory.
- **State-of-the-art GPU scaling** as shader passes, separated into `--scale`/`--dscale`/`--cscale`:
  `bilinear` (fast/low quality), `lanczos` (separable, good, default), `ewa_lanczos` /
  `ewa_lanczossharp` (polar "Jinc"/EWA — 2-D radial kernel, very high quality, the `high-quality`
  default), `ewa_lanczos4sharpest` (with built-in anti-ringing on gpu-next). Plus
  `--correct-downscaling`, `--linear-downscaling`, `--sigmoid-upscaling` (avoid ringing),
  `--scaler-resizes-only` (bilinear passthrough at 1:1). (https://mpv.io/manual/master/)
- **Prefetch:** `--prefetch-playlist=yes` opens/reads the *next file* in a background thread (demuxer
  layer only — it does **not** decode ahead). For a local image, "one current frame resident on GPU,
  next file pre-opened on disk." There is no LRU of many decoded frames.
- **libplacebo** (https://github.com/haasn/libplacebo, https://libplacebo.org/): "the core rendering
  algorithms of mpv rewritten as an independent library," now also used by VLC and FFmpeg.
  Multi-backend (**Vulkan ≥1.2 / OpenGL / D3D11 / MoltenVK**); tiers from filter kernels →
  `pl_gpu` (thread-safe, async, refcounted) → GLSL shader generation → dispatch (with **shader
  compilation caching**) → high-level `pl_renderer` (presets: fast / default / high_quality). A
  `pl_queue` abstraction handles decode-ahead (decoder pushes frames, render loop pulls per vsync).
  Shader/3DLUT cache can be persisted to disk so recompiles amortize across runs.

**Most valuable lesson:** *Decode once → upload once to a GPU texture → do all scale/zoom/pan/color as
shader passes, and persist the compiled-shader cache.* Strongly consider **binding libplacebo** rather
than writing the scaler/tone-map/dither stack yourself — it's multi-backend (Vulkan/GL/D3D11), exactly
PhotoBlaze's Windows-now/macOS-later target.

### imv (Wayland/X11, open source, `eXeC64/imv` mirror) — minimal GPU viewer

- Pluggable build-time **backends** (FreeImage / libtiff / libpng / libjpeg-turbo / librsvg / libnsgif
  / libheif); loading on **detached pthreads**, one worker per source at a time.
  (https://github.com/eXeC64/imv/blob/master/src/source.c)
- **Legacy fixed-function OpenGL** (`GL_TEXTURE_RECTANGLE`, immediate mode): uploads the bitmap with
  `glTexImage2D` **only when the bitmap changes**; pan/zoom/rotate are pure GPU transforms over the
  cached texture — no re-upload, no re-decode.
  (https://github.com/eXeC64/imv/blob/master/src/canvas.c)
- Filtering is just GL `GL_LINEAR`/`GL_NEAREST` (no Lanczos/EWA — hardware bilinear ceiling). Decode is
  full-res; fit/zoom is a GPU transform. **Holds only the current image** (no neighbor prefetch).

**Most valuable lesson:** *Background-thread decode + one cached GPU texture + fit/zoom as cheap GPU
transforms already feels instant for a single image.* The obvious upgrade: add neighbor prefetch (imv
has none) and replace GL bilinear with proper resampling.

### feh (X11, open source, `derf/feh`) — bounded decoded-image LRU

- Built on **Imlib2**, CPU render to X11 drawables (no GPU). Speed = tiny startup/deps.
- **`--cache-size <MiB>`** (default 4, up to 2048): an Imlib2 in-memory **LRU of decoded images** so
  slideshow loops / back-forward never re-decode — "A higher cache size can significantly improve
  performance." (https://www.mankier.com/1/feh)
- `--cache-thumbnails` writes freedesktop `$XDG_CACHE_HOME/thumbnails`. `--preload` is a *validation/
  metadata* pass, not decode-ahead.

**Most valuable lesson:** *A bounded decoded-image LRU is exactly the cache layer mpv/imv lack* — worth
adding on top of a GPU pipeline so revisits/loops are free.

### vimiv-qt (Qt, open source, `karlch/vimiv-qt`) — worker-pool thumbnails + XDG cache

- Images via `QImageReader` (CPU/Qt raster, EXIF auto-transform). **Thumbnails on a `QThreadPool`**
  (one `QRunnable` per path), async `created(index, QIcon)` updates.
- Persists to the **freedesktop XDG thumbnail cache** (`thumbnails/{large,normal}`, hash-keyed by
  URI+mtime, with a `fail/` dir) so grids survive restarts and are shared across compliant apps.

**Most valuable lesson:** *Push thumbnail decode onto a worker pool and persist to a hash-keyed
(URI+mtime) on-disk thumbnail cache* — but its CPU/Qt-raster path is the ceiling a GPU-resident Rust
viewer should beat.

---

## Cross-cutting techniques table

| Technique | Who uses it | Why it helps | Applicability to PhotoBlaze |
|---|---|---|---|
| **Embedded/EXIF/RAW preview as first paint** | Photo Mechanic, Lightroom, macOS ImageIO, XnView, IrfanView, FastStone, nomacs | ~26 ms vs ~800 ms full decode (~30×); makes RAW culling instant | **Adopt as the default fast path.** Parse EXIF APP1 thumb, RAW embedded JPEG, CR3 HEVC, HEIF; decode the small image first, refine later |
| **Asymmetric browse-vs-view pipelines** | XnView, Photo Mechanic, Lightroom, nomacs | Browse never pays full-decode cost; view path only on explicit open/zoom | **Adopt.** Two code paths sharing a cache; fix XnView's limitation (use embedded for browse *and* full-res for view) |
| **Off-thread decode** | JPEGView (1 thread), nomacs (QtConcurrent), qimgv (pool=2), imv (pthreads) | UI thread never blocks on decode; navigation stays at frame rate | **Adopt.** Rust: dedicated decode worker(s) + channel back to UI |
| **Off-thread, screen-sized resample (display mip)** | JPEGView (`m_pDIBPixels`), nomacs (`DkImageStorage`), qimgv (`Scaler`) | UI only ever blits a small pre-sized bitmap; repaint/pan never re-resamples | **Adopt** (or do it on GPU). Cache the display-resolution surface; invalidate on zoom change |
| **Direction-aware read-ahead, discard on reverse** | JPEGView (FORWARD/BACKWARD/TOGGLE), nomacs (forward-biased), XnView (+1) | Next image ready before user arrives; no wasted work after a U-turn | **Adopt + extend.** Predict direction from recent nav; deeper forward window than ±1 since GPU decode is cheap |
| **Two-tier prefetch (buffer bytes ahead vs decode one ahead)** | nomacs | Cheap I/O can run far ahead; expensive decode stays near the cursor | **Adopt.** Especially valuable for slow disks / network drives |
| **Tiny pinned decoded cache (current±1)** | qimgv (≤3), nomacs (current±1, 256 MB), JPEGView (~2) | RAM stays trivial regardless of folder size | **Adopt for full-res frames**; pair with a larger thumbnail cache |
| **Bounded decoded-image LRU** | feh (`--cache-size`), JPEGView (timestamp LRU) | Back/forward and slideshow loops never re-decode | **Adopt**, sized to a RAM budget (e.g. a few hundred MB or N frames) |
| **Persistent on-disk thumbnail/preview DB** | FastStone (`FSViewer.db` + pre-scan), XnView (SQLite + WebP), nomacs/vimiv/feh (freedesktop PNG) | Re-browsing any folder is instant; survives restarts | **Adopt.** SQLite/redb, MD5(path+mtime+size) key, lossy WebP/JPEG payload; background pre-scan likely-next folders |
| **Request coalescing / dedup** | macOS Quick Look, qimgv (`QHash` in-flight), nomacs | Concurrent requests for the same image collapse to one decode | **Adopt** in the loader (in-flight map keyed by path+target size) |
| **Queue preemption (viewed image jumps the line)** | qimgv (`tryTake` stale preloads, priority 1) | Fast scrubbing never stalls behind stale speculative work | **Adopt** — highest felt-speed-per-line mechanism found |
| **Instant placeholder → sharpen when settled** | qimgv (transform→OpenCV swap), Lightroom (minimal→standard→1:1), Photo Mechanic (embedded→RAW render) | Eye sees *something* immediately; HQ cost paid only when motion stops | **Adopt.** Show embedded preview/low mip instantly; swap HQ render when nav settles or user zooms |
| **GPU-resident texture; scale/zoom/pan as shaders** | mpv/libplacebo, imv | Decode/upload once; redraws are ~free; zoom/pan are GPU transforms | **Core architecture for PhotoBlaze (wgpu).** Nobody in the Windows-viewer space does this |
| **High-quality GPU scalers (Lanczos / EWA-Jinc), linear-light, anti-ring** | mpv/libplacebo | Downscale/upscale quality matches CPU Lanczos, at GPU speed | **Adopt** — port or bind libplacebo's scaler set; do HQ downscale in a compute/fragment pass |
| **Persisted compiled-shader / pipeline cache** | mpv/libplacebo (`cache.h`) | Avoids shader recompile stalls on subsequent runs | **Adopt** — cache wgpu pipelines / SPIR-V to disk |
| **Decode-to-fit (DCT-scaled / half-size RAW)** | XnView (RAW half-size), (Lightroom pyramid levels) | Decode only the resolution the view needs | **Adopt for non-embedded path.** Use libjpeg-turbo `scale_num/scale_denom`; note JPEGView/IrfanView/FastStone do *not* do this for JPEG — an easy win |
| **Resample parallelized across cores (strip-parallel SIMD)** | JPEGView (`CProcessingThreadPool`, up to 4 cores, AVX2) | CPU resample scales with cores | **Adopt for CPU fallback** (rayon + SIMD); primary path is GPU |
| **Under-subscribed thumbnail pool** | nomacs (`idealThreadCount-2`), qimgv (separate 4-thread pool) | Bulk thumbnailing never starves foreground decode/UI | **Adopt.** Separate, lower-priority pool for grid thumbnails |
| **Disposable / lean cache to protect startup** | Photo Mechanic (empty-on-quit, age-out) | A giant persistent cache slows launch | **Heed.** Keep persistent cache bounded + aged; lazy-open the DB |
| **Overlap copy/ingest with browsing** | Photo Mechanic (cull while copying) | Pro workflow: start culling before import finishes | **Adopt** if PhotoBlaze does ingest |

---

## Open-source code worth reading

| Repo | What to study | Single most valuable takeaway |
|---|---|---|
| **JPEGView** — https://github.com/sylikc/jpegview | `JPEGProvider`, `ImageLoadThread`, `BasicProcessing`, `ProcessingThreadPool`, `JPEGImage` | The whole pipeline shape: decode thread → bounded direction-aware prefetch LRU → strip-parallel SIMD resampler → cached display-DIB. Copy this, add GPU. |
| **libplacebo** — https://github.com/haasn/libplacebo · https://libplacebo.org/ | `pl_renderer`, `pl_gpu`, `pl_queue`, filter kernels, `cache.h` | A drop-in, multi-backend (Vulkan/GL/D3D11) GPU render/scale/tone-map/dither library with EWA scalers and a frame queue. Consider binding it instead of writing shaders. |
| **mpv** — https://mpv.io/manual/master/ · https://github.com/occivink/mpv-image-viewer | `--scale`/`--dscale`/`--cscale`, `gpu-next`, `--prefetch-playlist`, `--image-display-duration` | "Decode once, keep frame resident on GPU, pre-open next file on disk" is the whole image-viewer model. |
| **nomacs** — https://github.com/nomacs/nomacs (`ImageLounge/src/DkCore`) | `DkImageLoader`, `DkImageContainer`, `DkImageStorage`, `DkSettings`, `DkCachedThumb` | Split I/O from decode; two-tier prefetch under a RAM budget; async screen-sized display mip; under-subscribed thumbnail pool. |
| **qimgv** — https://github.com/easymodo/qimgv (`qimgv/components/{loader,cache,scaler}`) | `Loader::loadAsyncPriority`/`clearPool`, `Cache::trimTo`, `Scaler` "latest wins" | Queue preemption (viewed image jumps the line, drop stale preloads) + instant-placeholder-then-sharp swap. |
| **imv** — https://github.com/eXeC64/imv (`src/canvas.c`, `src/source.c`) | `draw_bitmap` uploading texture only on change; pthread per-source decode | Minimal GPU viewer: upload once, transform on GPU; add the neighbor prefetch it lacks. |
| **feh** — https://github.com/derf/feh | `--cache-size` Imlib2 LRU | A bounded decoded-image LRU is the cheap win mpv/imv omit. |
| **QuickView** — https://github.com/justnullname/QuickView | Direct2D rewrite of the JPEGView concept | Reference for the GPU/Direct2D path the Windows viewers omit. |
| **libjpeg-turbo** — https://libjpeg-turbo.org/ | `scale_num/scale_denom` DCT-scaled decode | The SIMD JPEG engine behind JPEGView/IrfanView. In Rust, bind via `turbojpeg`/`mozjpeg-sys`, or use native `zune-jpeg`; use DCT-scaled decode-to-fit. |

---

## Key lessons / anti-patterns

**What makes browsing feel INSTANT to pros (do these):**

1. **Show the embedded preview first.** RAW culling at "hold-the-arrow-key" speed is entirely about
   displaying the camera's embedded JPEG, not the RAW. (Photo Mechanic's whole reputation.)
2. **Never block the UI thread.** Decode and HQ-resample off-thread; the UI only blits a ready surface.
3. **Have the next image ready before the user asks.** Direction-aware prefetch makes flicking feel
   like the images are already there.
4. **Let the current image preempt everything.** When the user scrubs fast, drop stale speculative
   work and prioritize what's on screen (qimgv).
5. **Refine after motion settles.** Instant low-res/embedded → swap to sharp HQ when the user pauses or
   zooms. The eye never waits on a blank screen.
6. **Persist thumbnails to disk and pre-warm them.** Revisiting a folder must be instant
   (FastStone pre-scan, XnView SQLite).
7. **Cache the display-resolution surface.** Pan and repeated repaint at a fixed zoom must cost zero
   resampling (JPEGView `m_pDIBPixels`).

**Anti-patterns that make viewers feel slow (avoid these):**

1. **Full-decoding RAW (or forcing `…FromImageAlways`) when an embedded preview exists** — the #1
   cause of culling lag. (Lightroom "Minimal"/Develop re-render; macOS `…FromImageAlways`.)
2. **No read-ahead — decode the next image only when the user navigates to it** (IrfanView). Feels
   "fast" only because decode is fast; a hitch appears on big files.
3. **Single-threaded decode/thumbnailing** (XnView, IrfanView) — leaves 7/8 of a modern CPU idle;
   12 s vs 55 s for 2000 thumbnails is purely threading.
4. **No appropriately-sized cached preview → on-the-fly render** = Lightroom's "loading…" stall.
   The fix is a persisted resolution pyramid.
5. **Re-resampling on every repaint/pan** instead of caching the display surface.
6. **Decoding to full resolution then downscaling on CPU** when DCT-scaled / half-size decode would do
   (JPEGView/IrfanView/FastStone all leave this on the table for JPEG).
7. **A giant persistent cache that slows startup** (Photo Mechanic warns about exactly this) — keep it
   bounded, aged, and lazily opened, off OS-indexed paths.
8. **CPU/GDI blit with no GPU** — universal among the Windows speed kings; the ceiling PhotoBlaze
   should blow past.

---

## Recommendations for PhotoBlaze

Concrete architecture, synthesizing the above into a Rust/Windows-first/GPU-resident design:

1. **Embedded-preview-first decode is non-negotiable.** Build a metadata/preview extractor (EXIF APP1
   thumb, RAW embedded JPEG, CR3 HEVC, HEIF/AVIF previews) that returns a small image in tens of ms.
   This is the default browse and first-paint path. Full decode is a deferred upgrade. (Proven by
   Photo Mechanic, Lightroom, macOS ImageIO, XnView, FastStone, IrfanView, nomacs.)

2. **GPU-resident rendering via wgpu (Vulkan/D3D12 on Windows, Metal on macOS later).** Decode once on
   a worker → upload once to a GPU texture → zoom/pan/fit are GPU transforms; repaint is ~free. Keep N
   neighbor textures resident in VRAM (bounded by a VRAM budget). **Seriously evaluate binding
   libplacebo** for the scaler/tone-map/dither stack — it's multi-backend and already battle-tested in
   mpv/VLC/FFmpeg; otherwise port its EWA-Lanczos / Lanczos / sigmoid-upscale / anti-ring scalers as
   wgpu shaders. Persist the compiled pipeline cache to disk.

3. **Three-tier cache:**
   - VRAM: current ± a small window of full-res textures (qimgv/imv model, but deeper).
   - RAM: bounded LRU of decoded frames + extracted previews (feh `--cache-size` + JPEGView LRU).
   - Disk: persistent thumbnail/preview DB (SQLite or redb), key = hash(path+mtime+size), payload =
     lossy WebP; background pre-scan of likely-next folders (FastStone pre-scan, XnView SQLite).
   Keep all of it bounded + aged so startup stays fast (Photo Mechanic warning).

4. **Worker model:** a small decode pool (start 2–4) + a separate, lower-priority thumbnail pool
   (`cores-2`) so grid work never starves foreground decode (nomacs). CPU resample fallback is
   rayon + SIMD, strip-parallel (JPEGView). Dedup in-flight requests (qimgv/Quick Look).

5. **Prefetch + scheduling = where "instant" actually lives:**
   - Direction-aware read-ahead; predict from recent nav; deeper forward window than ±1 (cheap on GPU).
   - Two-tier prefetch: read *bytes* far ahead, *decode* only near the cursor (nomacs) — matters on
     network/SD-card drives.
   - **Queue preemption:** the on-screen image always jumps the line and cancels stale speculative
     decodes (qimgv `tryTake`). This is the highest-leverage felt-speed mechanism in the survey.

6. **Preview-then-refine everywhere:** paint the embedded preview / low GPU mip instantly; swap in the
   full HQ render when navigation settles or the user zooms. Never show a blank/loading frame.
   (qimgv, Lightroom, Photo Mechanic.)

7. **Decode-to-fit for the non-embedded path:** libjpeg-turbo `scale_num/scale_denom` (or `zune-jpeg`)
   and half-size RAW decode so you never decode more pixels than the viewport needs — a win the famous
   Windows viewers leave on the table for JPEG.

8. **Don't re-resample on repaint:** cache the display-resolution surface (GPU texture/mip); invalidate
   only on zoom change (JPEGView `m_pDIBPixels`).

**Where PhotoBlaze can credibly be the fastest:** GPU-resident textures + HQ GPU scaling (nobody in the
Windows viewer space does this), full multi-core parallel + DCT-scaled decode (beats XnView/IrfanView's
single thread several-fold), and qimgv-style queue preemption layered on a deeper, direction-predicted
prefetch window. The "boring" wins (embedded-preview-first, persistent thumbnail DB, display-surface
cache, never-block-the-UI) are table stakes that every fast viewer already proves work.

---

## Sources

Primary sources are cited inline above. Load-bearing references, grouped:

**Photo Mechanic / Lightroom / Apple:**
- https://docs.camerabits.com/support/solutions/articles/48001146200-caching-preferences
- https://docs.camerabits.com/support/solutions/articles/48001252598-render-cache-preferences
- https://docs.camerabits.com/support/solutions/articles/48000207409-ingesting-photos-with-photo-mechanic
- https://www.lightroomqueen.com/community/threads/lightroom-previews-smart-previews.25096/
- https://www.seachess.net/notes/dive-into-lightroom-catalogues/
- https://helpx.adobe.com/lightroom-classic/help/lightroom-smart-previews.html
- https://helpx.adobe.com/lightroom-classic/kb/optimize-performance-lightroom.html
- https://darkroomphotos.com/lightroom-classic-new-embedded-previews/
- https://mjtsai.com/blog/2026/04/19/fast-thumbnails-with-cgimagesource/
- https://developer.apple.com/documentation/imageio/cgimagesourcecreatethumbnailatindex(_:_:_:)
- https://developer.apple.com/documentation/imageio/kcgimagesourcecreatethumbnailfromimageifabsent
- https://eclecticlight.co/2026/05/16/explainer-quicklook/

**JPEGView / IrfanView / FastStone:**
- https://github.com/sylikc/jpegview (ImageLoadThread.cpp, MainDlg.cpp, JPEGProvider.cpp, JPEGImage.h, BasicProcessing.cpp, ProcessingThreadPool.cpp, CHANGELOG.txt, Config/JPEGView.ini)
- https://github.com/justnullname/QuickView
- https://www.irfanview.com/faq.htm · https://www.irfanview.com/history_old.htm · https://www.irfanview.com/plugins.htm
- https://irfanview-forum.de/forum/program/feature-requests/91941-gpu-acceleration
- https://www.faststone.org/FSViewerDetail.htm · https://www.softerviews.org/FastStoneViewer.html
- https://libjpeg-turbo.org/

**XnView / nomacs / qimgv:**
- https://en.wikipedia.org/wiki/XnView · https://www.xnview.com/wiki/index.php?title=GFL_SDK
- https://newsgroup.xnview.com/viewtopic.php?t=43291 · https://newsgroup.xnview.com/viewtopic.php?t=42338 · https://newsgroup.xnview.com/viewtopic.php?f=82&t=45024
- https://github.com/nomacs/nomacs (DkImageLoader.cpp, DkImageContainer.cpp, DkImageStorage.cpp, DkSettings.cpp, DkCachedThumb.cpp) · https://deepwiki.com/nomacs/nomacs/2.2-image-loading-system
- https://github.com/easymodo/qimgv (loader.cpp, cache.cpp, scaler.cpp, imageviewerv2.cpp)

**mpv / libplacebo / imv / feh / vimiv:**
- https://mpv.io/manual/master/ · https://github.com/occivink/mpv-image-viewer · https://github.com/mpv-player/mpv/wiki/GPU-Next-vs-GPU
- https://github.com/haasn/libplacebo · https://libplacebo.org/ (renderer/, basic-rendering/) · https://www.phoronix.com/news/Libplacebo-MPV-Rendering-Lib
- https://github.com/eXeC64/imv (src/canvas.c, src/source.c, src/backend.h) · https://man.archlinux.org/man/imv.1.en
- https://github.com/derf/feh · https://www.mankier.com/1/feh
- https://github.com/karlch/vimiv-qt
