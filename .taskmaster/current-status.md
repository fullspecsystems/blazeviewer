# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-06-27. On `main`._

A fast, chrome-less, keyboard-driven photo viewer. The prefetch engine ("hold a
key and fly") is done, plus broad multi-codec support, full-res RAW, the color
story (in-shader ICC → wide-gamut → HDR, task #11), and the rotation/zoom/pan/
scaling/EXIF/help UI (#1/#3/#4/#5/#7). Privacy no-trace (#2), Esc teardown (#6),
`enter` random nav (+ `Shift+Enter` prev-random), and the Windows-integration +
MSI track are all done.

## ⏭ ACTIVE NEXT WORK: make HEIC decode fly — see
[`docs/heic-decode-plan.md`](docs/heic-decode-plan.md)

**Recent sessions shipped HEIC preview-first + on-land sharpen** (instant 320×240
previews while scrolling; the on-screen photo re-decodes to full ~250 ms–1 s after
you land). It works, but the full decode isn't PhotoBlaze-fast because **WIC's HEVC
decoder serializes (~1.7× on 8 threads, measured)**. **Decision (owner): pivot
HEIC/AVIF to CPU `libheif`** (parallel — the pool's 8 workers already run concurrent,
just bottlenecked on WIC's single session → ~8× and prefetch fulls *ahead*). NVDEC
deferred (iPhone HEICs are 48-tile HEVC grids; libheif handles that free). The full
phased plan, the build-toolchain blocker, the "higher-quality preview" spike, and
the deferred code-review findings are all in **`docs/heic-decode-plan.md`** — read it
first.

**Green bar:** `cargo test --workspace` (**167 passing**, 0 ignored),
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo fmt --all -- --check` — all clean. Working tree clean; preview-first/sharpen
landed in commits up to `3c74cee`.

---

## ✅ DONE: color management + wide-gamut + HDR output

Three layers, all behind the established seams (`ImageDecoder`, `Renderer`):

### 1. In-shader ICC color management
`pb_decode::color::ColorTransform { matrix:[[f32;3];3], trc:[f32;7], enabled }` —
source-linear→BT.709 3×3 (via `moxcms` `transform_matrix`) + the source EOTF as
moxcms's unified 7-param curve. Carried on `DecodedImage::color` (default sRGB
passthrough). Per-backend extraction:
- **JPEG** APP2 (`zune` `icc_profile`); **PNG/TIFF/WebP** (`image`-crate concrete
  decoder `icc_profile` — `load_with_icc`); **JXL** `rendered_icc`.
- **HEIC/AVIF** (`wic.rs`): the MS HEIF decoder returns **0 WIC color contexts**
  (verified), so the ISOBMFF **`colr` box** is parsed from bytes — `prof`/`rICC`
  embedded ICC *and* `nclx` CICP. (WIC color-context query kept as a fallback.)
- sRGB / ~2.2-gamma-with-sRGB-primaries → `enabled=false` passthrough (bit-exact).

### 2. fp16 scRGB render path (`pb-render`)
Scene → `Rgba16Float` **scRGB-linear intermediate** (`SCENE_WGSL`: source→scene-linear,
mode 0 sRGB / 1 convert-no-clamp / 2 scene-linear-passthrough; per-image output
`scale`). Then a fullscreen **present** pass (`PRESENT_WGSL`) → the surface: SDR 8-bit
= extended-Reinhard tone-map (per-image `peak`) + sRGB-encode; HDR fp16 = copy through.
Overlay composites into the linear intermediate so one present pass serves both.

### 3. Wide-gamut + HDR output — **pure wgpu, no native D3D12 interop**
**Key fact:** a DXGI **fp16 (`Rgba16Float`) flip-model swapchain is always scRGB**
(linear, BT.709, extended range; 1.0 = 80 nits) — no `SetColorSpace1` needed, and
wgpu already offers `Rgba16Float`. So `pb_render::display::primary_hdr()` (DXGI
`GetDesc1`) detects an HDR desktop and configures an fp16 surface; else 8-bit
non-sRGB. HDR AVIF/HEIC decode to fp16 scene-linear via WIC `128bppRGBAFloat` (**WIC
does the PQ/HLG decode + gamut + linearization for us**; `PixelFormat::Rgba16F`,
`common::finalize_hdr_scrgb`). Brightness baked in the scene pass: SDR content ×
SDR-white-scale, HDR content × 1.0 (absolute scRGB → highlights blow past SDR white).

**Tests:** color unit tests (passthrough / P3 / AdobeRGB / CICP / LUT-sRGB / garbage);
`colr`-box byte fixtures (prof + nclx + HDR-transfer); `finalize_hdr_scrgb` fp16
tests; pb-render golden tests (SDR round-trip, enabled-curve). Verified live via the
`decode` example + the `offscreen_png` render; on-screen wide-gamut/HDR confirmed by
the owner (the fp16/HDR swapchain is uncapturable by GDI — see caveat).

### Open followups (color/HDR)
- Real **SDR-white level** via the DisplayConfig API (currently a 200-nit default in
  `display.rs`); revisit WIC's scRGB reference-white assumption if brightness drifts.
- **Per-output** HDR detection (currently the primary output only).
- **Radiance-HDR / OpenEXR** (image-crate, not WIC) still clamped to SDR; CMYK JPEG
  mis-colored; LUT/CLUT & gray ICC → sRGB passthrough (`lcms2`-behind-a-flag).
- **Committable color test fixtures**: tiny re-tagged P3/AdobeRGB swatches +
  integration test (`magick` can tag PNG/TIFF/WebP/JPEG; emit the ICC via
  `moxcms::encode()`). AVIF/JXL/HEIC need delegates we lack, but `colr` is unit-tested.
- macOS output = wgpu `Rgba16Float` surface + CAMetalLayer EDR (deferred; cheap port).

### ⚠ Capture caveat
On an **HDR desktop**, GDI `CopyFromScreen` *and* `PrintWindow` capture the
flip-model swapchain as **all-white** (a Windows limitation, not a render bug). Use
`cargo run -q --example offscreen_png -p pb-app -- <img> out.rgba` (then
`magick -size WxH -depth 8 rgba:out.rgba out.png`) to verify rendering off-screen.

### Spike / dev tools (kept)
- `crates/pb-render/examples/hdr_probe.rs` — DXGI display-capability probe (→ folds
  into a real `DisplayCaps` detector later).
- `crates/pb-app/examples/offscreen_png.rs` — render the real pipeline to a buffer
  (visual verification while on-screen capture is broken).

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

## Run it
```
cargo run -p pb-app --release -- "D:\Media\Pictures" -r     # fullscreen, recursive
cargo run -p pb-app --release -- "<leaf folder>" --windowed # dev window
cargo run -q --example decode -p pb-decode -- <files...>    # decode + color-transform report
cargo run -q --example hdr_probe -p pb-render               # display HDR/gamut/nits probe
```

## Architecture
```
crates/pb-core    pure nav/shuffle/prefetch/cache + ResidentRing + open (launch policy) — no I/O, no GPU
crates/pb-decode  ImageDecoder backends (zune/image/jxl/svg/raw/wic) + dispatch + decode-to-fit + EXIF + color (ICC→shader transform, fp16 HDR)
crates/pb-render  wgpu presenter (gpu.rs: scene→fp16 scRGB intermediate→present; WGSL); display (HDR detect); ViewTransform; UploadStrategy
crates/pb-app     winit loop, decode_pool (priority workers), hud.rs, main.rs (engine wiring)
```

## The prefetch engine (don't break it)
Decode/I-O are off the event loop on a priority worker pool; neighbors are
prefetched into a byte-budgeted (~1.5 GB) resident GPU texture ring; a keypress is a
**rebind, not a decode** (the color/scale uniforms are baked at upload; present_slot
only updates a 16-byte peak uniform). Advance is **gated on readiness**. The
gated-advance/failure paths in `main.rs` (`advance`/`about_to_wait`/`drain_results`/
`present_item`/`present_failed`) are subtle — re-read before changing them.

## Other backlog (tasks.json)
- #8 configurable keybindings (TOML), #9 recursive ordering, #10 feedback toast.
- **#2 privacy/no-trace — DONE** (static audit + `viewing_a_folder_writes_nothing_to_disk`
  no-trace test + CLAUDE.md "Privacy guarantee" section; opt-in-persistence subtask
  deferred — nothing on disk to gate yet). **#6 esc-teardown — DONE** (`begin_exit`:
  hide window first → `clear_session_state` (RAM-only) → exit; Drop frees VRAM/pool
  after).
- #12 Windows open (file-arg/drag-drop/picker) — **in progress** (subtask 1, the pure
  `pb-core::open` seam, done in the tree); #13 MSI/associations; #14 polish; #15 macOS.
- #1/#3/#4/#5/#7/#11 done.
- Native scaled-decode (JPEG DCT, WebP downscale-on-decode) still a TODO.
- **`enter` random nav — WIRED** (Enter/NumpadEnter → `Playlist::random_next`, hold-to-fly
  via the new `Nav` enum). The pinned cycle-boundary prefetch bug is **fixed**
  (`extend_random` now peeks `Playlist::next_shuffle()` across the reshuffle seam) and
  its test un-ignored. NOTE: the shuffle seed is fixed (0), so the random order repeats
  each launch — fine for now (deterministic/testable/privacy-safe); vary the seed later
  if per-launch variety is wanted. The DXGI photon-timing step is the only Phase-3 item
  still deferred.
- **random→sequential is no longer slow** (polish): the `Direction::Random` prefetch
  now also keeps the current photo's *sequential* neighbours (cur±1) warm at LOW
  priority (`prefetch.rs`, HEDGE=2), so the first space/backspace after an `enter`
  jump is an instant ring hit instead of a cold decode — without slowing random fly
  (the hedge loads only once the pool catches up at rest).
- **"Not-ready" loading pie** (polish, #2-style affordance): a translucent top-right
  pie (`hud::render_pie` → renderer `set_pie` → `App::tick_pie`) shown while the next
  photo is still decoding (a miss outlasting ~120 ms). No true decode progress exists,
  so it eases asymptotically toward — never reaching — full on a self-calibrating time
  constant (`decode_ewma`, a rolling mean of real miss durations), snaps to full +
  fades when the photo lands, and brightens on a keypress the engine can't yet service.
  Re-rasterized only on a visible change. **Interactive verification by owner pending**
  (hold space/enter on a cold folder to see it; GDI capture is broken on the HDR desktop).

## Environment / gotchas
- `cargo` at `~/.cargo/bin` (`$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`).
- MSRV **1.80**: no `Option::is_none_or` (1.82+) — use `match`; `is_some_and` is fine.
- GPU tests run on the RTX 5090. Don't launch the **fullscreen** app from automation —
  use a short `--windowed` `Start-Process` + kill; quote paths with spaces.
  Desktop is currently in **HDR mode** (so the app uses the fp16 scRGB surface, and GDI
  screen capture is broken — see the capture caveat).
- `D:\Media\Pictures` is the real corpus (use `-r`); `D:\Media\Pictures\test-images`
  has the per-format corpus **plus wide-gamut/HDR test images** (`WideGamut-*-DisplayP3*.jpg/.avif`,
  `*-HDR.avif`, and `-sRGB` twins for A/B).
