# PhotoBlaze — Project Guide

PhotoBlaze is a photo viewer with exactly one obsession: **how fast you can flick
through thousands of images.** No chrome, fit-to-screen, keyboard-driven, with
photos decoded ahead of time and held resident in GPU memory so the next frame is
already there when you press a key.

## Prime directive

> **Every architectural decision is answered by one question: "Will this make it
> faster, or have basically zero performance impact?"** If it's neither, it
> doesn't ship. Speed is the feature.

Corollary: **we do not guess about speed — we measure it.** Performance claims
require numbers from the benchmark corpus. Architecture choices that affect the
hot path are built behind swappable seams and A/B tested (see *Instrumentation*).

## The performance model (read this before optimizing anything)

The naive intuition is "tune the GPU rendering." That's wrong. Drawing one
textured quad is microseconds; the GPU is never the bottleneck for display. The
real wall is **decode throughput**, and the architecture exists to hide it:

1. **Decode-to-fit.** Never decode more pixels than the display shows. On the
   7680×3840 target, a 24 MP JPEG is decoded at a reduced scale (libjpeg-turbo
   DCT scaling 1/2, 1/4, 1/8), often cutting decode several-fold. A major lever
   in general — but **measured inert on this 7680-wide display for ≤24 MP photos**
   (decode-spike: 0/200 triggered scaling), where full-decode throughput dominates
   instead. Encoded in `pb-decode::FitBox`.
2. **Preview-first, then refine.** Show the embedded thumbnail/preview (EXIF,
   HEIC, RAW) instantly, swap in the scaled full decode when ready. Makes fast
   scrubbing feel instant even when full decode lags.
3. **Prefetch ring → resident VRAM.** A direction-biased window of neighbors is
   decoded and uploaded *ahead* of the user into a ring of resident GPU
   textures. **A keypress is a rebind, never a decode or an upload.**
4. **Self-paced advance.** Holding a key advances to the newest *ready* frame
   each vsync — so you fly when decode keeps up and degrade gracefully (to
   previews) when it can't. Capped at the monitor refresh rate.

The metric that matters is **keypress → photon** (input to the pixel actually
scanning out), target ≤ one refresh interval (~8.3 ms @ 120 Hz). Throughput is
capped by refresh: the job is "one fresh frame per vsync, with an instant preview
fallback," not "infinite fps."

## Architecture

```
crates/
  pb-core    pure nav / precomputed-random / prefetch / cache-residency
             — no I/O, no GPU, deterministic, 100% unit-testable
  pb-decode  decode abstraction (decode-to-fit + preview-first) + swappable backends
  pb-source  PhotoSource seam: "encoded bytes + name for item i" over a filesystem
             listing (FsSource), a ZIP (ZipSource), or a 7z (SevenZSource) — RAM-only,
             read-only on the view path; archive viewing lives here
  pb-render  fit-to-screen geometry now; wgpu presenter (swapchain, ring, draw) later
  pb-ui      the chrome design system: egui tokens + components (cards, toggle,
             buttons, text fields) + a Windows-tracking light/dark theme. egui-only,
             no app deps; powers the dialogs and the standalone component gallery
  pb-app-core platform-neutral orchestration model (NS0/ADR-021): the action vocabulary,
             the PbKey key model + keymap, slideshow + hold-to-fly timing, the shared
             config dir, and the CoreEvent/CoreEffect/MenuState/Modifiers/KeyResolution
             contract. toml-only — no winit/egui/GPU — so the macOS SwiftUI shell and the
             winit shell can drive the same core. The winit App re-exports its modules.
  pb-app     the winit shell binary: event loop, decode thread pool, egui dialogs, wiring
             over pb-app-core (still holds most orchestration until the NS0 AppCore-struct
             inversion; the shell-neutral seams already live in pb-app-core)
```

The crate boundaries *are* the A/B seams. Anything whose "is this faster?" answer
is non-obvious goes behind a trait so alternatives can be benchmarked: decode
backend, cache/eviction policy, present mode, upload strategy.

### Threading
- **Event-loop thread (winit):** input, swapchain, draw. Never blocks on I/O or
  decode. On keypress: advance index → rebind resident texture → present.
- **Decode pool:** a dedicated worker pool with **priorities + cancellation**
  (not bare rayon — work-stealing reorders prefetch and there are no priorities).
  Pulls jobs from the prefetch scheduler, decodes-to-fit, hands off for upload.
- **Upload (`UploadStrategy` seam):** v1 uses a **persistent staging-buffer ring**
  (`copy_buffer_to_texture`) — measured ~48 GB/s, 3.4× the 120 Hz budget. Never
  `write_texture` (the trap: 60–75 fps on large frames). Uploads land in the
  resident ring during prefetch — **never on the keypress frame**. A zero-copy CUDA
  alias is the gated escalation behind the same seam.

## Test-Driven Development (required)

- **Write the test first.** Especially for `pb-core` logic (nav, random walk,
  prefetch window, eviction) — it's pure, so there is no excuse.
- **Coverage target: >80%**, measured with **`cargo-llvm-cov`** (Windows-native;
  tarpaulin is Linux-only). The hard-to-test GPU/present shell is marked
  `#[coverage(off)]` so the number stays honest rather than gamed.
- **Property tests** (`proptest`) for nav/cache invariants (e.g. a random cycle
  visits each item exactly once; a residency plan never exceeds capacity).
- **Golden-image tests** for rendering: a headless **wgpu** render reads back to
  a CPU buffer, compared to reference PNGs with a perceptual diff (**nv-flip**) and
  a tolerance. Run in CI on a software adapter (**WARP**/lavapipe, no GPU required).
- **Fuzz** the decoders (`cargo-fuzz`) — they parse hostile bytes.
- Keep logic testable by isolating it from I/O and GPU (the whole point of
  `pb-core`). If something is hard to test, that's usually a design smell.

## Instrumentation & A/B methodology (first-class, not an afterthought)

- **Live profiling:** `tracing` + `tracing-tracy` (Tracy) with **wgpu timestamp
  queries** (+ `wgpu-profiler`) for unified CPU+GPU zones and frame markers. Behind a
  feature; compiles out of release.
- **keypress → photon:** timestamp input via QPC, read true scanout time from
  DXGI `GetFrameStatistics().SyncQPCTime` (flip-model swapchain + waitable object
  + max-frame-latency = 1). Validate against Intel **PresentMon** as an oracle.
  GPU pass split (upload vs draw) via wgpu timestamp queries. macOS later uses
  the same seam with `CAMetalDrawable.presentedTime`.
- **Microbenchmarks:** **Criterion** locally over the pinned corpus (decode from
  memory). **CI regression gate** runs deterministic instruction-count benches
  (**CodSpeed** / iai-callgrind) on *platform-independent* code on Linux — never
  gate on Windows wall-clock noise.
- **A/B pattern:** `Box<dyn Trait>` at the swap seam (decode backend, cache
  policy, present mode), generics in the hot inner loop, cargo features only for
  whole-program/profiler toggles. One runner replays a scripted keypress workload
  per variant and logs **per-frame NDJSON**; report **p50/p95/p99**, never means.

## Minimal UI (the entire user-facing surface)

- Borderless, chrome-less window; borderless-fullscreen at native res (enables
  DXGI Independent Flip for lowest latency).
- Image fit to screen, centered, **never cropped** (`pb-render::fit_rect`).
- Keymap:
  - `space` / `→` — next photo
  - `backspace` / `←` — previous photo
  - `enter` — random photo (precomputed shuffle order; reversible)
  - `esc` — quit
- Hold any nav key to iterate as fast as frames become ready.
- Key handling tracks **held physical keys** (`Pressed`/`Released`), ignoring OS
  key-repeat events, plus a focus-loss release net (avoids known winit repeat/lost
  key-up bugs).

### HUD / toast icons — Font Awesome solid (codified workflow)

Overlay panels (`hud.rs`) composite white outlined text **and optional icons** into
one software RGBA8 pill, drawn as a single alpha-blended quad — rebuilt only on
change, never per frame (off the photo hot path). Icons are **Font Awesome `solid`**
SVGs (the house style): a single `currentColor` path tinted white, with the text's
black-outline pass for legibility. They're vendored into the repo and rasterized via
the same `resvg`/`usvg`/`tiny-skia` stack `pb-decode` uses (`icon::rasterize`).

> **Style decision (2026-06-28, owner):** we tried `duotone` first but switched to
> **solid** — "boring but reliable and effective." Duotone's 40%-opacity secondary
> layer muddied at toast size. Use **solid** for any new icon; don't reintroduce
> duotone without a reason.

**To add an icon:**
1. Find it in the local FA library:
   `D:\Media\fontawesome-pro-plus-7.3.0-web\svgs\solid\<name>.svg`
   (`ls svgs/solid | grep <kw>` to search; always the `solid` weight).
2. Copy it **verbatim** into `crates/pb-app/icons/<name>.svg`.
3. Add `pub const <NAME>: &str = include_str!("../icons/<name>.svg");` to
   `icon::assets` (`crates/pb-app/src/icon.rs`).
4. Show it: `show_toast_icon(msg, Some(icon::assets::<NAME>), ..)` for icon+text, or
   `show_toast_icon("", Some(..), ..)` for an icon-only square pill (e.g. rotate).

**Licensing:** FA **Pro** assets are licensed to the owner but **not redistributable**.
The repo is **private**, so vendoring the SVGs is in-bounds. If it ever goes public:
git-ignore `icons/` and load from the local FA path at build, or swap to the free-tier
solid set (most of these icons, including the ones used here, are in FA Free).
(Privacy task #2 is unaffected — the SVGs are compile-time assets, not a viewing trace.)

## Chrome design system — the `pb-ui` crate (don't reinvent components)

The viewer hot path is custom wgpu (above). The **chrome** — Settings, About, and
the Confirm / Message / Password dialogs — is **egui**, in a second winit window
(`pb-app/src/dialog.rs`), and it is **component-based on `crates/pb-ui`**. The rule:
**when you need a button / field / toggle / card / section, reach for the `pb-ui`
component — never hand-roll one in a dialog.** That's the whole point of the crate; a
one-off `egui::Button` in `dialog.rs` is a bug (it will drift in size/color/radius).

`pb-ui` is egui-only (no app deps), so it stays reusable and testable, and it powers
the **gallery** — the egui equivalent of a Storybook page:

```sh
cargo run -p pb-ui --example gallery   # every token + component, Light/Dark/Both
```

The gallery is dev-only (eframe dev-dependency; never shipped) and is the place to
**preview and A/B the system** — including light vs dark side by side, which a real
single-theme dialog can't show.

**What's there (`pb-ui/src/lib.rs`):**
- **Tokens:** `SPACE_1..6` (4px scale), `GAP` (**the** standard gap — between rows, between
  cards, and the dialog button gap/inset; one knob), `RADIUS_CONTROL`/`RADIUS_CARD`,
  `CONTROL_H` (32px — set once, kills "every control a different size"), `FIELD_MARGIN`,
  `CARD_WRAP_WIDTH`, and `Palette` (named color roles, light + dark).
- **Theme:** `install_fonts` (native Segoe UI) once per dialog ctx; `apply_style(ctx,
  dark)` each frame (cheap; survives egui's own theme bookkeeping); `apply_to_ui(ui,
  dark)` to scope one region (e.g. a gallery column, or a **combo popup** — egui draws
  popup *contents* with the global ctx style, so re-assert it inside `show_ui`).
- **Components:** `group_card` (the **grouped-settings** card: a semibold heading inside
  the card + `card_row`s that **auto-space by `GAP`** — no dividers; related settings share
  one card, so a page is a few cards not one-per-setting), `card` (single), `card_row`
  (responsive: control on the right when wide, stacked under the header below
  `CARD_WRAP_WIDTH`), `toggle` / `toggle_with_label`, `page_title` / `section_label` (type ramp: page title
  30 / section 17 — both semibold via the bundled Segoe UI Semibold face / card title 14.5
  / description 12.5), `primary_button` / `secondary_button` / `danger_button`,
  `text_field`, `slider` (stable-width value box + solid-accent fill — no jitter),
  `icon_sized`. Section headings are Title Case; setting labels stay sentence case.
- **Icons (`pb-ui/src/icon.rs`):** Font Awesome SVGs **vendored per family**
  (`icons/<family>/<name>.svg`), rasterized to a **white square sprite** and **tinted at
  draw time** — one texture serves every tone and theme (cached in the egui ctx). A
  semantic `Icon` enum (`Lock`, `Warning`, `Trash`, …) names *meaning* not glyph; `Tone`
  (`Neutral`/`Accent`/`Warning`/`Danger`/`Success`) resolves through the `Palette` so it's
  light/dark-correct. Placement helpers — `lead_row` (gutter icon centered on the first
  content line: the dialog body shape) and `inline` — bake the alignment, so there is
  **no per-call nudging or top-clipping** (the old pain). The square render is our own
  `fa-fw` — FA glyphs aren't all square (lock is 384×512), so we center every glyph in a
  square box. **Switch families** by flipping `icon::ACTIVE_FAMILY` (vendor that family
  first). The HUD toasts keep their own CPU-composite rasterizer (`pb-app/src/icon.rs`);
  only the egui chrome uses `pb-ui::icon`.

**Conventions:** primary = accent default action (Save/OK/Unlock); secondary = neutral
(Cancel); danger = red (Delete). Dialog status icons: Password = `Lock`/`Neutral`,
Message = `Warning`/`Warning`, Confirm-delete = `Trash`/`Danger`. `dialog.rs` keeps only
the *scaffold* (the second window + `button_bar` / `dialog_frame` panels) and **composes
pb-ui atoms** inside it. Theme is locked to the OS-resolved light/dark at open (explicit
`ThemePreference`, not `System`, so `apply_style` isn't re-clobbered).

**To add a component:** put it in `pb-ui` (drive it from tokens/`Palette`, take a
`&mut egui::Ui`, return the `Response`), add it to the gallery `catalog`, then use it.
Don't add UI primitives to `pb-app`. **To add an icon:** copy the glyph for each vendored
family from the FA library (`D:\Media\fontawesome-pro-plus-7.3.0-web\svgs\<family>\`) into
`pb-ui/icons/<family>/`, add a variant to `icon::Icon` + a `glyph!` arm, show it in the
gallery's Icons row.

> **Provisional (2026-06-28):** the theme currently leans *native-Windows*
> (Segoe UI + an OS-ish accent). An *own-brand* identity (a tuned palette + Inter,
> Rerun-`re_ui` style) is on the table; it's a one-file token+font swap in `pb-ui`.
> Accent colors are explicitly flagged for a later revisit.

## Privacy guarantee (no record of viewed photos) — tasks.json #2

**The line is content, not footprint** (owner, ADR-018): PhotoBlaze keeps **no
persistent record of which photos were viewed, or any pixel/metadata derived from
them.** The app's own *existence* on disk is fine — the installer's registry writes
(the per-user ProgID, file associations, folder verb the Velopack install hook writes),
a Start-menu shortcut, and read-only config (the future task #8) are all explicitly in-bounds. What's forbidden is a
trace of the *viewing*: no thumbnail DB or pixel cache, no recent-files/MRU of
photo paths, no decoded-pixel temp files, no log of viewed paths. **One deliberate
exception (ADR-022, owner 2026-07-02):** `settings::last_folder` remembers the single
most-recent *folder* (never file names) as the Open dialog's default start on a fresh
launch — it never auto-opens anything (owner call 2026-07-03 reversed the brief
reopen-on-launch behavior); it's written on the explicit open action, and unit tests
can't write it (`AppCore::persist_prefs`).

**Explicit user edits are a separate, allowed category** — not an exception to be
minimized. Deleting a photo, saving a rotation (an EXIF Orientation write back to
the file or a sidecar), and similar metadata updates *do* touch disk, but only ever
on a **user-initiated command** — never as a passive byproduct of viewing. The line
is *involuntary traces of viewing*, not *the user editing their own files*; nothing
is persisted unless the user deliberately invokes the action.

- **Every runtime cache is RAM-only and dropped on exit.** Inventory: the resident
  GPU texture ring + its `pb-core::ResidentRing` mirror, the decode pool's in-flight
  buffers + `pending_uploads`, `meta_cache` (per-photo panel data), per-image
  `rotations`, the `failed` set, the transient `toast`, and on-demand EXIF reads
  (`Shift+I`) — all in memory, never serialized. `pb-core` is pure (no `std::fs`).
- **On-disk I/O is read-only on every view/cache hot path.** The only files
  PhotoBlaze opens *while viewing* are the photos themselves, and only to *read*:
  directory scan (`read_dir`), decode (`fs::read`), and the panel's
  `fs::read`/`fs::metadata`.
- **Writes happen only on an explicit user command, never on the view path.** The
  Edit-menu file operations — delete a photo, save a rotation (write the EXIF
  Orientation), future metadata edits — modify disk, but they are deliberate,
  user-triggered actions on the user's own files: gated behind a command (with
  confirmation for destructive ones), never automatic, never reached by scrolling or
  decoding. Per-image `rotations` stay RAM-only until the user chooses *Save*.
- **Esc teardown writes nothing** (task #6): hide the window, drop the RAM caches
  (`clear_session_state`), exit — no flush-to-disk step exists.
- **Enforced two ways:** a no-trace integration test
  (`viewing_a_folder_writes_nothing_to_disk`) diffs a sandbox before/after a real
  scan+decode+EXIF session and asserts zero files created or modified — it exercises
  only *viewing*, so it still holds once the Edit commands land (a scan/decode never
  triggers them); and a static audit that no `fs::write`/`File::create` sits on the
  *passive view/decode path* (today the only ones in the tree are `#[cfg(test)]` code
  and the `offscreen_png` example). The Edit-menu commands (delete, save-rotation) are
  the one place app code writes — reachable only from an explicit user action. Re-run
  the audit before adding any *passive* disk write; any on-disk scratch must be opt-in
  + cleared.

## Cross-platform discipline (Windows now, Apple Silicon later)

Windows 11 is the target, but the spikes + codex review (see `decisions.md`,
post-spike update) put us on the **portable path**: **wgpu is the v1 renderer**
(DX12 backend on Windows, Metal on macOS), because CPU decode (2.5×) and the
staging-ring upload (3.4×) already clear 120 Hz — wgpu's portability costs nothing
measurable here.
- **`winit` owns windowing + input**; the wgpu surface is created on its window
  handle. Rendering/upload/decode sit behind the `Renderer`, `DecodeBackend`, and
  `UploadStrategy` traits.
- **macOS is a cheap port** (wgpu Metal backend + a hardware-decode/upload backend
  swap), not a rewrite — deferred to v2.
- **GPU decode + zero-copy is a gated acceleration backend** (native D3D12 +
  nvImageCodec), pursued only if the high-MP stress test proves a need (ADR-012
  kill criterion). The CPU pool is the permanent baseline and handles formats GPU
  decode can't.
- Do color management **in-shader** (matrix + TRC) so it ports unchanged.
- Isolate every platform-specific call (refresh-rate query, swapchain latency
  hook, photon timestamp) behind a single helper so the eventual port is a small
  surface.

## Current library picks

These are the starting points from the research in `.taskmaster/docs/`. Each is
**provisional and benchmark-justified** — the A/B seams exist precisely so we can
replace any of them with data.

| Concern | Primary | A/B alternative / notes |
|---|---|---|
| JPEG | `turbojpeg` (libjpeg-turbo, **native scaled decode**) | `zune-jpeg` (pure Rust, SIMD; pair with `fast_image_resize`) |
| PNG/APNG | `png` (image-rs) + `zlib-rs` backend (pure Rust, fastest now) | — (no scaled decode exists for PNG) |
| WebP | `libwebp-sys` (`use_scaling` = true downscale-on-decode) | `image-webp` (pure Rust, no SIMD/scaling) |
| AVIF | `dav1d` crate + `avif-parse` | parallelize across images (single-tile ≈ no thread gain) |
| HEIC | `libheif-rs` | ⚠ **highest** Windows build risk — pin vcpkg ports or ship DLLs |
| JXL | `jxl-oxide` (pure Rust) | `jpegxl-rs` only if native DC downscale needed |
| TIFF / BMP / QOI | `tiff` / `image` / `qoi` (all pure Rust) | — |
| SVG | `resvg`/`usvg` → `tiny-skia` pixmap → texture | rasterize at on-screen res; watch `vello_hybrid` for live-zoom |
| RAW | `kamadak-exif` → extract embedded JPEG preview → JPEG path | full demosaic deferred (100×+ cost); `rawler` optional (LGPL) |
| Color | `moxcms` (pure Rust; 3×3 matrix + TRC in-shader) | `lcms2` behind a flag for exotic CLUT/CMYK profiles |
| Windowing | `winit` (window + input; `refresh_rate_millihertz` for the advance cap) | portable; the wgpu surface is created on its window handle |
| GPU API | **wgpu** (DX12 backend on Windows, Metal on macOS), present **Mailbox** | native D3D12 retained as a gated acceleration backend behind `Renderer` |
| GPU decode | **CPU decode pool** (`zune-jpeg`; `turbojpeg` as A/B) — measured 2.5× @ 120 Hz | nvImageCodec/CUDA zero-copy is a gated escalation (ADR-012 kill criterion); benchmark 5090 HW-JPEG first |

### Decode-to-fit value ranking
JPEG ≫ WebP > JXL(C) ≫ everything else. Prioritize the scaled-decode path where
it pays.

### What's actually wired (2026-06-27) — deviations from the provisional table
Multi-codec dispatch is implemented in `pb-decode` behind the `ImageDecoder` seam
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
- **AVIF + HEIC**: **Windows WIC** (`wic.rs`, `cfg(windows)`, `windows` crate) using the
  OS codec extensions — NOT dav1d/libheif/vcpkg. First platform-specific decode backend
  (macOS would mirror with ImageIO). Needs the AV1/HEVC/HEIF Store extensions; absent →
  graceful decode error. **JPEG-2000 not added** (no pure-Rust decoder; OpenJPEG/C, rare).
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
- **Color management + wide-gamut + HDR output are wired** (tasks.json #11). Three layers:
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
- **Archive viewing (ZIP + 7z)** (tasks.json #30) is wired in the new `pb-source` crate
  behind the `PhotoSource` seam, decoded via `pb_decode::decode_named_bytes` (bytes +
  extension hint). ZIP = lazy per-entry (handle pool for parallel reads); 7z = **eager
  decode-to-RAM** (solid archives have no cheap random access), opened **off-thread** with
  a RAM **pre-flight that predict-and-refuses** archives that won't fit (a real OOM aborts
  uncatchably in Rust). RAM-only — never extracted to disk, so the no-trace guarantee holds
  (`viewing_a_{zip,7z}_writes_nothing_to_disk`). Errors surface in the egui `Message` dialog.
  Password-protected archives are *detected* (`OpenError::PasswordRequired` /
  `ZipSource::needs_password`) but in-app password **entry is a TODO**. Crates: `zip` +
  `sevenz-rust2` (both pure Rust, no C build risk).
- **Known v1 limitations** (deliberate): Radiance-HDR / OpenEXR (image-crate, not WIC) still
  clamped to SDR; CMYK JPEG mis-colored; first frame only (GIF/animated-WebP/Live-Photo/
  multipage-TIFF). LUT/CLUT & gray/CMYK ICC profiles → sRGB passthrough (the `lcms2`-behind-a-
  flag escalation). SDR-white level is a 200-nit default (real value via DisplayConfig = TODO).
  ⚠ On an **HDR desktop**, GDI screen capture of the flip swapchain returns all-white — a
  Windows limit, not a render bug (use the `offscreen_png` example to verify rendering).

## Build, test, bench

> Rust toolchain required (`rustup`); `rust-toolchain.toml` pins stable + the
> components for coverage.

```sh
cargo test                 # unit + property + golden tests
cargo llvm-cov --workspace # coverage (target >80%)
cargo clippy --all-targets -- -D warnings
cargo fmt --all
cargo run -p pb-app        # the viewer (scaffold today)
cargo bench                # criterion microbenchmarks over the corpus
```

## Working norms

- **Test first.** New `pb-core` logic without a failing-then-passing test is not
  done.
- **No per-frame heap allocations on the hot path.** Pre-allocate pools; reuse.
- **Never block the event loop.** Decode and I/O are always off-thread.
- **Never decode or upload on the keypress frame.** If you're tempted to, the
  prefetch window is wrong — fix that instead.
- **Quarantine platform-specific code** behind the established helper seams.
- When a decision touches the hot path and the answer isn't obvious, **put it
  behind a seam and benchmark both** rather than arguing.
- **Update `CHANGELOG.md`.** When you land a user-facing bug fix or feature, add a
  line under `## [Unreleased]` (Keep a Changelog format — group under `Added` /
  `Changed` / `Fixed` / `Removed`; write for users, not commits). Skip purely
  internal churn (refactors, test-only changes, CI/workflow tweaks, formatting).
  On release, the `[Unreleased]` block moves under the new version's heading + date
  and a fresh empty `[Unreleased]` is left at the top.

## Cutting a release

**Windows** ships **Velopack** (per-user installer + auto-update), built + signed **locally**
(GitHub Actions credits are finite): `pwsh scripts/release-windows.ps1 -Upload` builds with
libheif, signs the exe + `Setup.exe` + `Update.exe` via Azure Trusted Signing, `vpk pack`s a full
release, and rsyncs the flat feed to `downloads.fullspec.ca/photoblaze/win`. The app reads that
feed over HTTP (`update.rs` `FEED_URL`) and self-updates — downloads in the background, installs on
quit. Version comes from `crates/pb-app/Cargo.toml`, so it always matches the app; there is **no
tag / GitHub Release for Windows**.

**macOS** is **built locally on the owner's Mac** via `scripts/release-macos.sh` (Developer ID +
notarization), then published to `downloads.fullspec.ca/photoblaze/mac` with
`scripts/release-mac-upload.sh` — which scp's the DMG + appcast **straight from the Mac** to
jdlien.com and repoints the `PhotoBlaze-latest.dmg` symlink (the remote `mac/` dir is
jdlien-owned, no sudo). No Windows detour: `scripts/release-mac-upload.ps1` is the equivalent for
running the upload from the Windows box, but the whole Mac release now stays on the Mac. Hosted
GitHub Actions is too expensive to use, so `.github/workflows/release.yml` — which builds the DMG
on a hosted `macos-15` runner — is **`workflow_dispatch`-only (dormant)**; a `v*` tag no longer
auto-triggers it. A GitHub Release for the DMG, if wanted, is created manually. Signing setup is
in `.taskmaster/docs/release-signing.md`.

macOS **auto-updates via Sparkle** (task #65) — the in-app equivalent of Windows' Velopack. The
`.app` embeds `Sparkle.framework` (assembled by `build-swift-host.sh`, since a SwiftPM executable
has no Xcode "Embed Frameworks" phase) and reads an EdDSA-signed `appcast.xml` next to the DMG
(`SUFeedURL` in `Info-swift-host.plist`). `release-macos.sh` re-signs Sparkle's nested helpers with
the Developer ID (inside-out, before the app) and, after notarizing, EdDSA-signs the DMG and writes
`dist/appcast.xml` (`scripts/generate-mac-appcast.sh`); `release-mac-upload.ps1` publishes that
appcast alongside the DMG. The **private EdDSA signing key lives only in the release Mac's login
keychain** (generated once via Sparkle's `generate_keys`; the public `SUPublicEDKey` is committed in
the plist) — **back it up** (`generate_keys -x`); losing it means no future build can be signed for
auto-update without shipping a new public key via a stopgap manual update.

> **Release only from a clean, committed workspace.** `crates/pb-app/build.rs` stamps the build
> id `-dirty` on **any** `git status --porcelain` output — **untracked files included** — and that
> shows in the About dialog. Commit (or `.gitignore`) everything first: both release scripts build
> fresh from the working tree, so a stray file (a local `vendor/` dir, a scratch note) silently
> ships a `-dirty` build. Check `git status` is clean before running a release script.

> **Never let a tool auto-invoke a paid CI run.** Hosted runners cost real money (a macOS run is
> billed at 10×), so releases are scripted and run locally — the `v*`-tag trigger was removed from
> release.yml precisely so a tag push (or `gh release create`) can't quietly start a hosted build.
> Don't re-add an automatic hosted trigger; script the build instead.

To cut a release:

1. **Roll the `CHANGELOG.md`.** Move `## [Unreleased]` into `## [<version>] - <YYYY-MM-DD>`,
   leave a fresh empty `[Unreleased]`, and update the compare links at the bottom. The crate
   version (`crates/pb-app/Cargo.toml`) must match the tag's numeric core (a `-beta.N` suffix
   lives only on the tag).
2. **Windows:** `pwsh scripts/release-windows.ps1 -Upload` from this machine (with `.env.release`
   signing creds). **Run it from native PowerShell, not the Bash tool / Git Bash** — the ssh
   config's YubiKey `Match exec` hook has a Windows path that Git Bash mangles, so the upload fails
   `Permission denied (publickey)`. The build + sign + pack still succeed there; only the `-Upload`
   scp/rsync needs native PowerShell (the feed is already in `dist\feed`, so a retry is upload-only).
   Prune superseded packages on the server periodically.
3. **macOS (all on your Mac):** `./scripts/release-macos.sh --release` builds the signed +
   notarized DMG **and** EdDSA-signs it into `dist/appcast.xml` (Sparkle auto-update, task #65),
   then `./scripts/release-mac-upload.sh` scp's the DMG **and the appcast** to jdlien.com and
   repoints `PhotoBlaze-latest.dmg` — no Windows box needed. (Optionally verify the seed's updater
   first with `./scripts/test-sparkle-update.sh dist/PhotoBlaze-<version>.dmg`.) A GitHub Release is
   **optional and manual** — nothing auto-builds from a tag:
   `gh release create v<version> dist/PhotoBlaze-<version>.dmg* --notes-file <(bash
   scripts/changelog-section.sh <version>)`. Write **real, curated, user-facing** CHANGELOG notes
   before tagging so `changelog-section.sh` has a body. **Never** enable `generate_release_notes`.
4. **Tag for posterity** (optional): `git tag -a v<version> -m "…" && git push origin v<version>`.
   Windows never needs it (Velopack reads the version from `Cargo.toml`); it's a record + the anchor
   for a manual macOS GitHub Release. Safe to push now that release.yml doesn't auto-build on tags.
5. **Verify:** the Windows feed serves the new `releases.win.json` + `.nupkg` (and a launched build
   self-updates); the macOS DMG is genuinely notarized — `xcrun stapler validate <dmg>` and
   `spctl -a -t open --context context:primary-signature -vv <dmg>` → `source=Notarized Developer
   ID`; the macOS feed serves the new `appcast.xml` (curl it) and a launched older build detects →
   downloads → installs-on-quit the update. A `-` in a tag marks a pre-release; a clean `vX.Y.Z`
   is a full release.


## Project Task Tracking

This project uses [taskmaster](https://github.com/eyaltoledano/claude-task-master) conventions for tracking tasks, with a `.taskmaster/tasks/tasks.json` using the following structure:

### Directory Structure

```
.taskmaster/
├── tasks/
│   └── tasks.json    # Active tasks
├── docs/             # Documentation or notes
└── archive.json      # Completed tasks (optional)
```

### Schema

**Important:** Task and subtask IDs must be numbers, not strings. Task Studio cannot look up individual tasks if IDs are quoted strings like `"25"` instead of `25`.

```json
{
  "master": {
    "tasks": [
      {
        "id": 1,
        "title": "Brief task title",
        "description": "What needs to be done",
        "status": "pending|in-progress|done|review|deferred|cancelled",
        "priority": "high|medium|low",
        "dependencies": [],
        "subtasks": [
          {
            "id": 1,
            "title": "Subtask title",
            "description": "Subtask details",
            "status": "pending"
          }
        ]
      }
    ]
  }
}
```

---
