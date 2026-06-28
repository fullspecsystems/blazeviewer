# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-06-28. On `main`._

A fast, chrome-less, keyboard-driven photo viewer. The prefetch engine ("hold a
key and fly") is done, plus broad multi-codec support, full-res RAW, the color
story (in-shader ICC → wide-gamut → HDR, task #11), and the rotation/zoom/pan/
scaling/EXIF/help UI (#1/#3/#4/#5/#7). Privacy no-trace (#2), Esc teardown (#6),
`enter` random nav (+ `Shift+Enter` prev-random), and the Windows-integration +
MSI track are all done. **Archive viewing (ZIP + 7z) shipped 2026-06-28**, now
including **in-app password entry + launch-path async open** — see the next section
(only RAM-budget tuning + small polish remain). **Configurable keybindings (#8) and
the fly-speed cap (#20) are also done, and the typed Settings model + live backend
are in (#22)** — only the Settings *dialog form* remains; see "Settings + configurable
keymap stream" below.

## Archive viewing — ZIP + 7z (2026-06-28) — DONE incl. password entry; budget-tune + polish remain

Open a `.zip`/`.7z` and browse the images inside like a folder (CLI arg, double-click
association, drag-drop, or the Open dialog's "Images & archives" filter). Same fast
prefetch/nav as loose files; in-archive browsing is recursive/flattened (sorted by entry
name). All on `origin/main`, gated (clippy `-D warnings` + fmt + tests). **Task 30** in
`tasks.json`.

**How it works:**
- **`pb-source` crate** — a `PhotoSource` seam (bytes + name + container for item `i`):
  `FsSource`, `ZipSource` (lazy per-entry, handle-pool for parallel reads),
  `SevenZSource` (eager — 7z is usually *solid* = no cheap random access, so the whole
  archive is decompressed into RAM on open). `pb-core` nav is unchanged (index-only), so
  the prefetch ring / decode pool didn't change.
- `pb_decode::decode_named_bytes` — decode in-memory bytes with an extension hint (so
  RAW/SVG/TGA route without a file path). Shift+I panel shows the archive path + in-zip folder.
- **7z memory safety** (`pb-app/src/archive.rs`): a real OOM *aborts* (uncatchable) in
  Rust → **predict-and-refuse** rather than try/catch. Sum the 7z header's uncompressed
  image sizes vs a RAM budget (fraction of `GlobalMemoryStatusEx` available − app
  reservations − transient margin; `PB_ARCHIVE_RAM_BUDGET` env override). Over budget →
  instant refusal, no load. `Vec::try_reserve` backstops the buffers.
- **Async open:** a `.7z` eager-decompresses on a background thread
  (`begin_archive_open` → per-tick `poll_archive_load`, generation-guarded so a newer
  open supersedes the first); the event loop stays live + the current photo stays
  visible; "Loading archive…" toast. `.zip` open is instant (synchronous).
- **Launch-path async (2026-06-28):** an archive on the CLI / double-click is **deferred
  into `resumed()`** (`pending_launch` + `queue_launch`, fired once after the window +
  engine exist) — the window appears immediately and a big `.7z` loads behind the spinner,
  and a launched encrypted/failed archive can use the egui dialogs instead of only logging.
  Folders / file lists still resolve synchronously in `main()`.
- **Password entry (2026-06-28):** `DialogKind::Password` — a dark-aware egui dialog with a
  blue lock icon, masked auto-focused field, Unlock/Cancel, **Enter submits / Esc cancels**;
  a **wrong password re-prompts in place** with an inline "Incorrect password" error, and a
  **"Checking…"** state covers the async 7z re-open. `ZipSource::password_ok()` validates a
  supplied zip password (a zip `open` succeeds even when *wrong* — an entry decrypt is the
  real check); `seven_z_projected_bytes` threads the password so a header-encrypted 7z
  pre-flights. `Option<password>` runs through `begin_archive_open`/`open_archive`/
  `seven_z_preflight`/`load_seven_z`; `finish_archive_open` routes PasswordRequired→prompt,
  success→close+rebuild, other→error dialog (`password_archive` holds the pending path).
  Password is RAM-only + scrubbed on dialog drop. Verified end-to-end on encrypted `.zip`
  **and** `.7z` (wrong→error, correct→opens); plain archives unaffected.
- **Structured errors → egui dialog:** `ArchiveOpenError`
  (too-large / corrupt / OOM / empty) → `DialogKind::Message` (dark-aware, via `open_message`).
  PasswordRequired no longer hits this — it opens the password prompt instead.
- **Privacy:** RAM-only — `viewing_a_zip_writes_nothing_to_disk` +
  `viewing_a_7z_writes_nothing_to_disk` prove no extraction to a temp dir.
- Crates: `zip` (deflate + aes-crypto) + `sevenz-rust2` 0.21 (incl. LZMA2/bzip2/ppmd) —
  both **pure Rust, no C build risk**.

**Remaining (handoff — budget-tune + small polish):**
- **Tune the RAM budget** — the `0.6` fraction + margin in `archive.rs` are guesses;
  **measure** open time + peak working set on a real solid-LZMA2 photo `.7z` and set them
  from data (per the prime directive). The 96 GB dev box never refuses naturally → drive
  the refusal path with `PB_ARCHIVE_RAM_BUDGET`.
- **Deterministic over-budget refusal test** — the no-trace tests exist; the refusal test
  needs a budget-injection seam (env vars race across parallel tests).
- **Huge-archive escalations** (behind the same seam, pick later): in-RAM per-block LRU
  (bounds RAM, keeps no-trace) or opt-in extract-and-delete (disk → opt-in + disclose +
  clear-on-close + leftover-sweep-on-startup). v1 just refuses + lets the user extract.
- **WiX:** register `.7z`/`.zip` as "Open with" candidates (NOT the default handler).
- Exotic 7z codecs (zstd/brotli/lz4 features off) and header-encrypted 7z error gracefully.

**Key files:** `crates/pb-source/src/lib.rs` (incl. `ZipSource::password_ok`,
password-threaded `seven_z_projected_bytes`), `crates/pb-app/src/archive.rs`,
`crates/pb-app/src/dialog.rs` (`DialogKind::Password` + `password_dialog`), and the
`main.rs` open path (`open_input` / `begin_archive_open` / `poll_archive_load` /
`finish_archive_open` / `prompt_archive_password` / `open_archive` / `seven_z_preflight` /
`load_seven_z` / `resolve_playlist`; launch defer = `queue_launch` + `resumed`). Tests:
`pb-source` (14, incl. encrypted-7z round-trip + zip `password_ok`) + `pb-app`
archive-budget + `viewing_a_{zip,7z}_writes_nothing_to_disk`. Password flow verified
interactively on encrypted `.zip`/`.7z` (GDI-capturable egui dialog).

## Settings + configurable keymap stream (2026-06-28) — keymap (#8) + fly-cap (#20) DONE; Settings dialog (#22) backend in, form is next

The keyboard is now fully configurable and the typed settings model is in; the one
remaining piece is the Settings **dialog form** (controls + the keybinding editor).
All committed on `main`, gated (clippy `-D warnings` + fmt + `cargo test -p pb-app`, 81 green).

**Shipped (committed `499e3d6`, `c5e5bf5`):**
- **#8 configurable keybindings — DONE.** New `action.rs` (the central `Action` enum:
  one-shot / nav / held `kind` + stable snake_case `id`; pure + unit-tested) and
  `keymap.rs` (`KeyChord` parse/Display like `"Ctrl+S"`, a default binding table = today's
  keys, optional `keymap.toml` → load / merge-over-defaults / validate with unknown-action,
  bad-key, and duplicate-key warnings). Every keypress now resolves through the keymap and
  routes by kind to **one `App::dispatch_action`**; the native menu maps
  `MenuAction::to_action` into the *same* dispatcher; the help overlay's key labels are
  generated from the live keymap (single source of truth). `held` is now
  `HashMap<KeyCode, Action>` (action captured at press) so nav/pan/zoom are remappable too.
  ~16 action/keymap unit tests.
- **#20 max photos/sec cap — DONE.** `advance_interval` gained a `max_rate` ceiling (the
  cap clamped to the display refresh; `0` or `>= refresh` = uncapped), read **live** from
  `Settings.max_advance_rate`. New `advance_interval_caps_at_max_rate` test.
- **#22 Settings — typed model + live backend (subtask 2 DONE; 3/5 in progress).**
  `settings.rs` is now a typed **serde + toml** `Settings { fullscreen, recursive,
  start_speed, ramp_secs, max_advance_rate, hold_delay_ms, scale_mode, letterbox,
  info_opacity }` with `#[serde(default)]`, clamped `load`, atomic `save`, + 7 tests;
  defaults mirror today's constants and an old `key=value` `fullscreen` file still loads.
  `App` holds it; the nav-feel curve + `initial_delay` + the #20 cap read it live (a
  `settings.toml` edit applies on next launch; mutating `App.settings` will apply live).
  **File ▸ Settings…** menu item added + `Ctrl+,` (both open the dialog).

**Remaining for #22 (the dialog form):**
- Wire the egui `settings_ui` controls to live `App.settings` — Save / Cancel / Esc +
  live-apply (mirror `take_confirm_result`: the dialog returns the edited `Settings`,
  `App` applies it live and snapshots on open for Cancel-revert).
- Two backend setters still to land so the dialog's color/opacity controls do something:
  **letterbox color** (`WgpuRenderer::set_letterbox`, currently the `pb_render::LETTERBOX`
  const) and **info-panel opacity** (thread an alpha into `hud`'s info panel, currently
  the `hud::BG` const); plus applying default scale/recursive at startup.
- The **keybinding editor** (subtask 4): key-capture → assign via the keymap, conflict
  display (reuse the keymap's duplicate-key check), reset-to-default, persist `keymap.toml`.
- **Coordination:** the form work is in `dialog.rs`, which the archive session has been
  co-editing (`button_bar`, password dialog) — do it once that settles to avoid the churn.

**Key files:** `crates/pb-app/src/` `action.rs`, `keymap.rs`, `settings.rs`, `menu.rs`
(`to_action` + the Settings item), `main.rs` (`dispatch_action`, `advance_interval` cap,
`held` map, `App.settings`). Config lives at `%APPDATA%\PhotoBlaze\{settings.toml,
keymap.toml}` — read-only on the view path (privacy #2; writes only on Save / fullscreen toggle).

## UI / file-commands stream (2026-06-28) — what just shipped + what's next

Separate from the HEIC/decode stream below. All on `origin/main`, each gated
(`clippy --all-targets -D warnings` + `fmt` + `cargo test -p pb-app`, 51 tests green).

**Shipped this session:**
- **Native menu bar** (`menu.rs`, muda; dark-aware via `darkmode.rs`) —
  File/Edit/View/Image/Help, windowed-only. Pure `action_for` (tested) →
  `App::dispatch_menu`. Dynamic enable-state for File ▸ Save Rotation.
- **egui dialog infra** (`dialog.rs`: 2nd winit window + egui-wgpu, OS dark/light):
  **About** (done), **Settings** (skeleton form), and a themed **Confirm** dialog.
- **#27 Copy** (`Ctrl+C` / Edit ▸ Copy, `clipboard.rs`): full-res decode → clipboard
  in BOTH **CF_DIBV5** (pixels) + **CF_HDROP** (file ref) via Win32 (dropped arboard).
  Pure transforms (fp16→sRGB8, rotate-bake) unit-tested.
- **#29 Save Rotation** (`Ctrl+S` / File, `save_rotation.rs`): **lossless** EXIF
  Orientation write via `little_exif` (JPEG only; atomic temp+rename; verified scan
  byte-identical + ICC preserved). Pure orientation-compose tested; drop RAM override
  + refresh-from-disk after save.
- **#28 Delete** (`delete.rs`): `Del` → Recycle Bin (`trash` crate), `Shift+Del` →
  **themed egui Confirm** (Directory Opus-style: file-✗ icon, ⚠ line, red Delete) →
  permanent. Pure cursor-after-removal tested; rebuilds source minus the path,
  advances (prev if last; empty state if none). Icon-only toasts; 160 ms
  deferred-advance so the icon shows before advancing.
- **FA icon system** (`icon.rs`): vendored **solid** FA SVGs (`icons/*.svg`) →
  rasterized (resvg) into HUD/toast pills + dialog chrome. (Tried duotone, switched
  to solid.) "To add an icon" workflow codified in CLAUDE.md.
- **#19 hold-to-fly accel ramp** (done).

**Next (UI), recommended order:**
- **#8 keybindings + #20 fly-cap — DONE**, and **#22 Settings** has its typed model +
  live backend in (see the "Settings + configurable keymap stream" section above). The
  remaining #22 work is the **dialog form** (wire controls to live `App.settings` with
  Save/Cancel, + the keybinding editor) plus two small backend setters (letterbox color,
  info-panel opacity). Do the form once the archive session's `dialog.rs` refactor settles.
- Then: **#9** recursive ordering, **#10** richer per-action toast strings (now easy —
  route through `Action`), **#23** slideshow.
- **Decided/deferred:** file-open picker stays **native `rfd`** (auto-dark on macOS;
  the light Windows dialog is an accepted gap — theming the shell dialog isn't worth
  it). The egui Confirm is the portable keeper (no `NSAlert` needed for the Mac port).

**Key files** (`crates/pb-app/src/`): `main.rs` (App + winit loop + dispatch + delete/
save/copy wiring + `dialog_event`), `menu.rs`, `dialog.rs`, `clipboard.rs`,
`save_rotation.rs`, `delete.rs`, `icon.rs`, `hud.rs`, `settings.rs`, `darkmode.rs`.
**GOTCHA:** the photo window is an uncapturable flip-swapchain (HDR) — verify on-photo
visuals with the owner; the **egui dialogs + menu DO GDI-capture** (screenshot them).
The release exe is **GUI-subsystem (no stderr)** — debug via a temp log file, not eprintln.

## ⏭ ACTIVE NEXT WORK: HEIC decode — Phases 0–3 DONE; only follow-ups remain — see
[`docs/heic-decode-plan.md`](docs/heic-decode-plan.md) (read the SESSION UPDATE at top)

**The libheif pivot landed end-to-end (Phases 0–3 done, 2026-06-28).** WIC's HEVC
decoder serializes (1.57×/8 threads, measured); the new **CPU `libheif` backend** is
parallel (~5×/8 threads) → **~45 full HEIC/s vs WIC's 9.4 (≈4.8×)**, lower single-image
latency too (115 ms vs 167). Behind the **`libheif` cargo feature** (OFF by default —
pure-Rust core stays toolchain-free, ADR-015); routed for **full SDR HEIC only**
(previews/AVIF/HDR stay on WIC); A/B via `PB_HEIC_BACKEND=wic`. iPhone output is
**pixel-identical to WIC**; orientation perfect. Set up: **`scripts/setup-libheif.ps1`**
(vcpkg + decode-only static libheif, `-DENABLE_PLUGIN_LOADING=OFF`).

- **Build/run with it:** `cargo run -p pb-app --release --features libheif -- "<folder>" -r`
  (needs `VCPKG_ROOT` or vcpkg at `~/vcpkg`; run the setup script once first).
## 🔬 2026-06-28 (late): the "1 s after flying" hunt — root cause was RAW, not HEIC

Owner reported full-quality still lagging ~1 s after flying + stopping in
`D:\Media\Pictures\2021` (905 iPhone HEIC + 285 Sony `.arw` + jpg/png). Stopped
guessing and **instrumented the real pipeline** (`--metrics` now also prints a
`sharpen` stage = full-requested→on-screen, and `pool decode (under load)`
percentiles + the slowest files). Findings, all measured:
- **The villain was RAW, not HEIC.** Pool decode p95 was **1388 ms**; the slowest were
  all `prev DSC*.ARW` at **~1.4 s each** — the RAW **preview** path was **demosaicing**
  (`DSC` sorts before `IMG`, so the ARWs sat in the startup window jamming all 8
  workers; any HEIC you stopped near paid the contention).
- iPhone HEIC sharpen itself is ~120 ms isolated but stretches under 8-way load
  (decodes balloon several-fold). No re-decode churn (decode count normal).

**Three fixes landed (all green):**
1. **Fix C — RAW preview never demosaics** (`pb-decode/raw.rs`): a preview request
   uses the embedded JPEG thumbnail (fast, ~tens of ms); the 100×+ demosaic is now
   **full-decode-only**. *This is the actual ~1 s fix.* Result on the 2021 folder:
   pool decode **p99 1467→259 ms, CPU 58→13 s**, the 1.4 s tail gone.
2. **Fix A — no-thumbnail HEICs route to libheif** (`route_full_heic` + `has_thumbnail_ref`):
   WIC fakes a thumbnail by full-decoding the grid (slow) for HEICs lacking a real
   `thmb` item (macOS-encoded Sony HEICs); those previews now go to libheif (one
   parallel decode, no WIC double-decode).
3. **Fix B — prefetch fulls *ahead*** (`pb-app` `sharpen_now`/`prefetch_fulls`/tiered
   `request_prefetch`): the full-res ring is now requested **even while flying**, but
   at LOW priority (queued behind every preview), so it fills the cores' spare
   capacity and the photo you stop on is often already sharp. RAW is **excluded** from
   the speculative ahead-ring (demosaic is too expensive to do for neighbours).
   Converges to idle (no churn). **Fly-then-stop feel needs owner verification** (can't
   inject keypresses).

**Still open (smaller):** iPhone HEIC *thumbnails* (WIC `GetThumbnail`) serialize under
load (~240 ms each when 8 run) — flying through dense HEIC could still be preview-bound.
Plus the earlier follow-ups: Sony HEIC color (**tasks.json #24**), Fill-mode decode-to-fit,
sync load paths bypass preview-first, AVIF on libheif.

**Privacy cleanup (flagged 2026-06-28, NOT yet applied):** the `--metrics` `pool decode`
diagnostic logs viewed photo **filenames** (`main.rs` ~L491, committed in `f346506`) to
stdout. Low practical exposure — opt-in flag, and release is a GUI subsystem with no console
— but the strict no-trace guarantee says *"no log of viewed paths,"* and `--metrics` is meant
to run in **release** (benchmarking), so the code ships. One-line fix: log the **extension
only** (`prev .arw`, not `prev DSC02715.ARW`) — keeps the format-level diagnostic, drops photo
identity. Held off because `main.rs` is the parallel session's active file.

### 🧪 2026-06-28 — parallel thumbnail extraction: TRIED, REVERTED (negative result — don't blind-retry)
Implemented libheif thumbnail extraction to replace WIC `GetThumbnail` (the ~240 ms
serializer above). **The capability works**: `heif_image_handle_get_thumbnail` + decode
gives the embedded thumbnail in **~3 ms (vs WIC ~20 ms isolated), correctly oriented**
(240×320 on a portrait file, matches WIC), fully parallel. **But it made things worse
overall and I reverted it**, because:
- Routing previews onto libheif made the *concurrent full* decodes **~4× slower** —
  windowed bench p95 **235 → 900 ms on the same files**. Mechanism: fast previews freed
  the workers to run *more* full grid decodes at once, and many concurrent libheif
  decodes slow each other down badly.
- **Two obvious fixes did NOT help** (both measured): capping libheif's per-context tile
  threads (`heif_context_set_max_decoding_threads` 1/2/4), and capping *concurrent full
  decodes* in the pool (a non-preview semaphore, 2/3/4). p95 stayed ~900 ms either way.
- So it's **not** thread oversubscription and **not** simple concurrent-full count.
  Leading hypotheses: (a) a **libheif/libde265 global lock** taken during decode (more
  concurrent calls → more contention), or (b) a **windowed-only artifact** — the bench
  uses a 64-photo window + Lanczos downscale-to-fit; the owner runs **fullscreen** with a
  ~12-photo window and *no* downscale for ≤12 MP, so it may not reproduce there at all.

**Headline learning:** HEIC **full** decodes balloon several-fold under 8-way concurrent
load (~138 ms isolated → ~900 ms windowed). That under-load latency — not the per-decode
speed — is the real ceiling on stop-to-sharp; Fix B (prefetch-ahead) dodges it by
pre-decoding, but understanding/limiting it is the next real lever.

**Kept (committed in `f346506`):** the `--metrics` instrumentation that found all of this
— `sharpen` (full-requested→on-screen) and `pool decode (under load)` percentiles + the
slowest-files list. Re-run with `--metrics` to investigate further.

**To resume:** re-apply the thumbnail extraction (it's straightforward — handle→
`get_number_of_thumbnails`/`get_list_of_thumbnail_IDs`/`get_thumbnail`→decode, orientation
1) **behind a default-off flag** so it can be A/B'd in fullscreen; OR first test the
global-lock hypothesis (time decode vs. time-holding-a-lock). Don't re-land it on by
default without a fullscreen win.

**Phase 3 evolution:** `upgrade_item`→`sharpen_now` (displayed, tier 1) +
`prefetch_fulls` (ahead-ring, tier 3, ungated); pure `pb_core::full_ring` bounds the
ring (budget + `MAX_FULL_RING=24`). The held-nav gate is gone (replaced by the priority
tiers). Decode **cancellation works** (queued-you-flew-past skipped; in-flight finishes
but result discarded; no mid-decode abort).

**Green bar:** `cargo test --workspace` (**175**) + `-p pb-decode --features libheif`
(**58**, +3 routing/thumb tests); `cargo clippy --workspace --all-targets` and
`--features libheif` (clean); `cargo fmt --all --check`. Converges to idle on every
folder tested; no stderr spam. Diagnostics (`sharpen`, `pool decode`) are `--metrics`-gated.
Throwaway A/B tools: `heic_bench`, `heic_compare` in `pb-decode`.

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
Ctrl+S           save rotation to file (lossless EXIF; JPEG only)
Ctrl+C           copy full-res image to clipboard (pixels + file ref)
Del / Shift+Del  delete → Recycle Bin / permanent (themed confirm)
i / Shift+I      info panel / full-EXIF "nerd" panel
/ or ?           keybindings help overlay
Ctrl+,           settings (egui dialog — model + backend wired; form WIP)
esc              quit
(windowed mode also has a native menu bar: File/Edit/View/Image/Help, incl. File ▸ Settings…)
```
**These defaults are now the built-in keymap (task #8 done).** All keys resolve through
`keymap.rs` and are remappable via an optional `%APPDATA%\PhotoBlaze\keymap.toml`
(`[keys]` table, action-id → chord string/array, e.g. `rotate_cw = "R"`); the in-app
keybinding editor is the remaining #22 piece.

## Run it
```
cargo run -p pb-app --release -- "D:\Media\Pictures" -r     # fullscreen, recursive
cargo run -p pb-app --release -- "<leaf folder>" --windowed # dev window
cargo run -p pb-app --release -- "album.7z" --windowed      # open a .zip / .7z archive
cargo run -q --example decode -p pb-decode -- <files...>    # decode + color-transform report
cargo run -q --example hdr_probe -p pb-render               # display HDR/gamut/nits probe
```

## Architecture
```
crates/pb-core    pure nav/shuffle/prefetch/cache + ResidentRing + open (launch policy) — no I/O, no GPU
crates/pb-decode  ImageDecoder backends (zune/image/jxl/svg/raw/wic) + dispatch + decode-to-fit + EXIF + color (ICC→shader transform, fp16 HDR) + decode_named_bytes
crates/pb-source  PhotoSource seam: FsSource / ZipSource / SevenZSource (bytes+name+container for item i; RAM-only, read-only) — zip + 7z archive viewing
crates/pb-render  wgpu presenter (gpu.rs: scene→fp16 scRGB intermediate→present; WGSL); display (HDR detect); ViewTransform; UploadStrategy
crates/pb-app     winit loop, decode_pool (priority workers), hud.rs, archive.rs (RAM budget + errors), action.rs + keymap.rs (central Action + configurable keymap, #8), settings.rs (typed serde+toml prefs), menu.rs/dialog.rs, main.rs (engine wiring + dispatch_action)
```

## The prefetch engine (don't break it)
Decode/I-O are off the event loop on a priority worker pool; neighbors are
prefetched into a byte-budgeted (~1.5 GB) resident GPU texture ring; a keypress is a
**rebind, not a decode** (the color/scale uniforms are baked at upload; present_slot
only updates a 16-byte peak uniform). Advance is **gated on readiness**. The
gated-advance/failure paths in `main.rs` (`advance`/`about_to_wait`/`drain_results`/
`present_item`/`present_failed`) are subtle — re-read before changing them.

## Other backlog (tasks.json)
- **#8 configurable keybindings (TOML) — DONE**; **#20 fly-speed cap — DONE**; **#22
  Settings UI — in progress** (model + backend done; dialog form + keybinding editor left).
- #9 recursive ordering, #10 feedback toast (now routable via `Action`), #23 slideshow.
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
