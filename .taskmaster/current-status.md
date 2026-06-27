# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-06-27._

A working photo viewer. Sequential navigation through real folders is solid; the
"hold a key and fly on full-res photos" engine (Phase 3) is the next milestone.

## Where things stand
- **Phase 0, 1, 2 are done** (foundations + spikes, wgpu window/render, sequential
  viewer), plus polish beyond plan (see below).
- **Repo:** https://github.com/jdlien/photoblaze (private, `main`). Latest commit
  `3bf2e6a`. Working tree is committed except this status doc + roadmap update.
- **54 tests pass; clippy clean** (`-D warnings`).

## What works today
Run it:
```
cargo run -p pb-app --release -- "D:\Pictures" -r          # recurse subfolders
cargo run -p pb-app --release -- "<a leaf folder>"          # non-recursive
cargo run -p pb-app --release -- "<folder>" -r --windowed   # dev window
```
Keys: `space`/`→` next · `⌫`/`←` prev · `0`/`o` fit↔original-1:1 · `i` info panel ·
`esc` quit. Hold to fly (tap = one photo; ~400 ms before auto-repeat; release stops
within one decode; **every photo shown, none skipped**).

Features: JPEG decode (`zune-jpeg`) with EXIF orientation; **decode-to-fit** Lanczos3
downscaling (fixes grain on large photos, shrinks textures); linear sampling;
chrome-less fullscreen (no startup flicker, HiDPI-correct); GPU-adaptive texture
limits + oversize clamp; **info overlay** (path · WxH · codec) via a from-scratch
text layer using the OS UI font.

## Architecture (see `docs/architecture.md`, `docs/decisions.md`)
```
crates/pb-core    pure nav/shuffle/prefetch/cache logic (no I/O, no GPU) — 29 tests
crates/pb-decode  ImageDecoder trait + zune-jpeg backend, orientation, decode-to-fit
crates/pb-render  wgpu (DX12) presenter: textured quad, ScaleMode, overlay, fit math
crates/pb-app     winit event loop, self-paced advance, hud.rs (text), folder scan
spikes/           decode + upload throughput spikes (drove the architecture)
.taskmaster/      docs (research/architecture/decisions/roadmap/review) + tasks.json
```
**Key decision (don't relitigate):** wgpu + CPU decode + staging-ring upload is the
v1 engine; native-D3D12 + nvImageCodec/zero-copy is a *gated* later escalation. This
was measured (decode 2.5×, upload 3.4× the 120 Hz budget) and codex-reviewed — full
record in `docs/decisions.md` (post-spike update) and `docs/review-brief.md`.

## Lessons learned (this session)
- Decode-to-fit is a **quality** fix, not just speed (GPU minification of full-res =
  grain/aliasing). Color: surface must be **non-sRGB**. Nav must be **self-paced**
  (ignore OS key-repeat). Startup needs **hidden-until-first-frame**. All applied.

## What's next — Phase 3: the prefetch engine (the headline)
Today decode is **synchronous on the main thread**, so big photos (e.g. 24–45 MP
wedding JPEGs) page at ~4–5 fps — decode-bound. Phase 3 fixes this:
- A **priority decode thread pool** (cancellation + on-screen preemption) — pull
  jobs from the prefetch scheduler in `pb-core` (`prefetch_targets` + `plan_residency`
  already exist and are tested).
- A **resident texture ring** reused across photos (not a new texture per nav as
  today) fed by the **staging-buffer upload** path (the upload spike proved
  `copy_buffer_to_texture` ≈ 48 GB/s; never `write_texture`).
- Self-paced advance (already in `pb-app`) then **shows photos streaming by** at
  refresh because the next one is already decoded — the per-photo wait disappears.
- Add **keypress→photon instrumentation** (DXGI `GetFrameStatistics`) + Tracy.

After Phase 3: instant previews (Phase 4), format breadth incl. HEIC/AVIF (Phase 5).

## Feature backlog (`tasks.json`) — partial progress
- #3 zoom, #4 scaling modes: **partly done** — `0`/`o` toggles fit↔original 1:1
  (no pan/zoom/fill yet; `9` fill and arbitrary zoom remain).
- #5 metadata panel: **basic done** (`i`); `Shift+I` full-EXIF "nerd mode" remains.
- #9 recursive: a `-r` flag exists; the `R`-key toggle + folder-grouped/natural sort
  remain (note: `R` collides with rotate-CCW — unresolved, see tasks.json).
- Untouched: #1 rotate, #2 privacy, #6 esc-cleanup, #7 help overlay, #8 configurable
  keybindings (TOML), #10 feedback toast. The `hud.rs` text layer is reusable for
  #7/#10.

## Environment / gotchas for the next session
- `cargo` is at `~/.cargo/bin` (not on PATH). Use `& "$env:USERPROFILE\.cargo\bin\cargo.exe"`.
- Build/verify: `cargo test --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`.
  GPU is an RTX 5090; golden-image render test runs on it.
- **turbojpeg** (faster JPEG + finer scaled-decode) is an A/B alternative but needs
  **NASM + cmake** to build (absent here) — that's why v1 uses pure-Rust `zune-jpeg`.
- The viewer is a **GUI subsystem** binary in release (no console); debug keeps the console.
- Don't launch the fullscreen app from automation (it traps the session) — use a
  short windowed `Start-Process` + kill, or rely on the headless render test.
- `D:\Pictures` (~17.7k JPEGs) is the owner's real corpus; photos live in **subfolders**
  so use `-r`. Display is **7680×2160 @ 120 Hz, 1.5× scale**.
- Line endings: git warns LF→CRLF (harmless); `Cargo.lock` is committed.

## To verify visually (not yet owner-confirmed this session)
The info panel (`i`) was built + smoke-tested but the owner hasn't eyeballed the
text rendering/placement. If the text is mis-aligned vertically, adjust the baseline
math in `pb-app/src/hud.rs` (the `y0 = baseline - ymin - height` line).
