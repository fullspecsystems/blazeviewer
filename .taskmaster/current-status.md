# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-06-27. On `main`, **pushed to origin** (HEAD `16aaf77`)._

A fast, chrome-less, keyboard-driven photo viewer. The prefetch engine ("hold a
key and fly") is done. **This session added broad multi-codec support, full-res +
upright RAW, panic-safety, transparent-image rendering, decode-failure handling,
and a HUD/EXIF overhaul** — all committed and pushed.

**➡ NEXT TASK is ICC color management (see the top section).** It's handed to a
fresh agent with full context here.

**Green bar:** `cargo test --workspace` (**~117 passing**, +1 ignored),
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`
— all clean. Release builds.

---

## ⏭ NEXT TASK: ICC color management (in-shader)

**The gap:** every decoder hands back RGBA8 that the renderer treats as **sRGB**,
with no profile handling. Wide-gamut sources are therefore wrong:
- **Display-P3 iPhone HEICs render oversaturated** — the big one; the library is
  full of them. (This is also *proof* the WIC HEIC path returns native P3 pixels,
  un-converted — see below.)
- AdobeRGB / ProPhoto JPEG/TIFF exports: wrong colors.
- HDR/EXR clamped to SDR (no tone-map) — related but a separate sub-task.

**The plan (per `CLAUDE.md` → "Color"):** do it **in-shader** — parse the source
ICC profile to a 3×3 matrix (source primaries → display) + transfer curve (TRC),
apply matrix+TRC in the fragment shader. GPU-cheap and ports to macOS unchanged.
**`moxcms`** is the picked crate (pure Rust; **already in the tree** as a transitive
dep of the `image` crate). `lcms2` behind a cargo feature only if exotic CLUT/CMYK
profiles turn up.

**Where to wire it:**
1. **Extract the profile on decode.** Add a color-space/ICC field to
   `pb_decode::DecodedImage` (default = sRGB). Per backend:
   - `image` crate (PNG/JPEG/TIFF/WebP): `DynamicImage`/decoders expose
     `icc_profile()`.
   - JPEG (zune): read the ICC from APP2 chunks (or zune's info).
   - **WIC (HEIC/AVIF):** WIC's format converter does **not** color-manage — it
     returns the codec's native pixels (Display-P3 for iPhone) with the profile in
     the HEIF container. Extract it (or detect P3) in `wic.rs`.
   - RAW: imagepipe outputs sRGB already; the embedded JPEG preview carries the
     camera profile (usually sRGB/AdobeRGB).
   - SVG/QOI/BMP/farbfeld/etc.: assume sRGB.
2. **Pass it to `pb-render`** and apply matrix+TRC in `gpu.rs`'s WGSL fragment
   shader (the draw path). `moxcms` parses the ICC and builds the matrix/TRC.
3. **Test:** open a Display-P3 iPhone HEIC (any in `D:\Media\Pictures`, e.g.
   `2021\2021-01-02 - Home\IMG_0357.HEIC`) and confirm it's correctly saturated, not
   neon. Use Windows Photos (color-managed) as the oracle.

**Watch out:** the photo pipeline now uses `ALPHA_BLENDING` (transparent images
composite over the letterbox) — keep color-conversion consistent with that. Do not
disturb the gated-advance engine (`main.rs`) or the decode-to-fit path.

---

## Keymap (current)
```
space            next photo            ⌫              previous photo
← ↑ ↓ →          pan (hold; accelerates)
= / -            zoom in/out (hold; accelerates; numpad +/- too)
8 / 9            scaling mode: fit / fill        0   toggle original 1:1 ↔ fit
                 (any of 8/9/0 also resets zoom/pan to that mode's framing)
r / Shift+R      rotate 90° cw / ccw (per-image, RAM-only)
i / Shift+I      info panel / full-EXIF "nerd" panel
/ or ?           keybindings help overlay
esc              quit
```
(`enter` random nav still unwired.)

## Run it
```
cargo run -p pb-app --release -- "D:\Media\Pictures" -r     # fullscreen, recursive
cargo run -p pb-app --release -- "<leaf folder>" --windowed # dev window
cargo run -p pb-app --release -- "<folder>" --metrics       # stage timings on exit
cargo run -q --example decode -p pb-decode -- <files...>    # decode-only CLI (codec smoke test)
```

## Image format support (this session's big add)
Multi-codec dispatch behind the `ImageDecoder` seam (`pb_decode::decode_bytes`
sniff-registry + extension routing for RAW/SVG/TGA):
- **JPEG** zune-jpeg; **PNG/GIF/BMP/TIFF/WebP/TGA/QOI/ICO/PNM/HDR/EXR** the
  pure-Rust `image` crate.
- **JXL** jxl-oxide; **SVG/SVGZ** resvg/usvg/tiny-skia (rasterized at display res,
  straight alpha; usvg inflates svgz itself).
- **RAW** (ARW/NEF/CR2/DNG/…): full-size embedded preview when present (Nikon
  ≈7360), else **demosaic** via rawloader+imagepipe to true sensor res (Sony
  1616/3968 → 6048) on a **256 MB-stack thread** (some NEFs overflow the default
  stack; full-preview cameras skip the demosaic).
- **AVIF + HEIC/HEIF**: **Windows WIC** (`wic.rs`, `cfg(windows)`, the `windows`
  crate — no vcpkg/dav1d/libheif), via OS codec extensions (AV1/HEVC/HEIF). COM
  init per-call so it works on the decode-pool worker threads.
- **Orientation:** `common::read_orientation` scans **all** EXIF IFDs (HEIC/RAW
  store it outside the primary IFD). WIC + imagepipe self-orient, so those paths
  pass orientation=1; the RAW preview path applies the container orientation.
  Verified: portrait HEIC→3024×4032, portrait NEF→4924×7374, portrait Sony
  demosaic→4024×6048.
- **Panic-safe:** decoders wrapped in `catch_panics`; **release profile is now
  `panic = "unwind"`** (was abort) so a panicking decoder skips the file, not the
  app. Stack overflows are still uncatchable → the big-stack demosaic thread.
- **Decode failure:** `App::present_failed` sets a "decode error" title + clears
  the stale panel/metadata; the previous frame is held (no black flash). Verified
  against corrupt JPEG/PNG files.
- **Transparent rendering:** photo pipeline switched to `ALPHA_BLENDING` so
  transparent PNG/WebP/SVG/icons composite over the letterbox (was REPLACE).
- `is_supported_extension` is the single source of truth the scanner filters on.

## Architecture
```
crates/pb-core    pure nav/shuffle/prefetch/cache + ResidentRing (no I/O, no GPU)
crates/pb-decode  ImageDecoder backends (zune/image/jxl/svg/raw/wic) + dispatch + decode-to-fit + EXIF + orientation
crates/pb-render  wgpu presenter (gpu.rs + WGSL shader); ViewTransform (view.rs); UploadStrategy (upload.rs)
crates/pb-app     winit loop, decode_pool (priority workers), hud.rs (overlay/table), main.rs (engine wiring)
```

## The prefetch engine (don't break it)
Decode/I-O are off the event loop on a priority worker pool; neighbors are
prefetched into a byte-budgeted (~1.5 GB) resident GPU texture ring; a keypress is
a **rebind, not a decode**. Advance is **gated on readiness** — every photo shown
in order; a miss holds the previous frame until its decode lands. The
gated-advance/failure paths in `main.rs` (`advance` / `about_to_wait` /
`drain_results` / `present_item` / `present_failed`) are subtle — re-read before
changing them. The DXGI photon-timing step is the only Phase-3 item still deferred.

## Known gaps / deferred (besides color management)
- HDR/EXR clamped to SDR (no tone-map); CMYK JPEG mis-colored; **first-frame-only**
  for GIF/animated-WebP/Live-Photo/multipage-TIFF.
- RAW demosaic is slow (~1 s/file); "preview-first then refine" is the future fix.
- AVIF/HEIC are Windows-only (WIC); macOS would mirror with ImageIO.
- Native scaled-decode (JPEG DCT 1/2·1/4·1/8, WebP downscale-on-decode) not done —
  currently full-decode + Lanczos.
- `enter` random nav unwired; a pinned `#[ignore]`d test for the random-prefetch
  cycle boundary (`pb-core::prefetch`) — fix when random nav is wired.
- `tasks.json` backlog: #2 privacy/no-trace, #6 esc-teardown, #8 configurable
  keybindings (TOML), #9 recursive ordering, #10 feedback toast, #11 color
  management (new). #1/#3/#4/#5/#7 done.

## Review tooling (codex)
Codex reviewed this session's codec/render work — the alpha-blend, SVG
straight-alpha, and decode-failure items came from that pass. To re-review vs the
pushed baseline:
```
& "C:\Users\jdlien\.codex\packages\standalone\releases\0.142.3-x86_64-pc-windows-msvc\bin\codex.exe" exec review --base origin/main
```
**Concurrency note:** a Codex agent has edited this tree in parallel this session —
re-check `git status` before large edits to avoid collisions.

## Environment / gotchas
- `cargo` at `~/.cargo/bin` (`$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`).
- MSRV **1.80** (`rust-version` in Cargo.toml): no `Option::is_none_or` (1.82+) —
  use a plain `match`; `is_some_and` (1.70) is fine. (Owner is fine bumping if a
  feature needs it.)
- GPU tests run on the RTX 5090. Don't launch the **fullscreen** app from
  automation — use a short `--windowed` `Start-Process` + kill; quote paths with
  spaces. Display 7680×2160 @ 120 Hz.
- `D:\Media\Pictures` is the real corpus (subfolders → use `-r`); it has many
  Display-P3 iPhone HEICs for color-mgmt testing. `D:\Media\Pictures\test-images`
  is the one-per-format codec corpus (jpg/png/qoi/webp×2/jxl/avif/heic/svg/arw/nef).
- Line endings: git warns LF→CRLF (harmless); `Cargo.lock` is committed.
