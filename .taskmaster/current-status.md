# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-06-27._

A fast, chrome-less photo viewer. **Phase 3 (the "hold a key and fly" prefetch
engine) is done and codex-reviewed-clean. The first batch of the tasks.json
feature backlog (5 interactive viewer features: #1/#3/#4/#5) is implemented and
green but NOT yet owner-verified or codex-reviewed.**

(Note on naming: the roadmap's numbered phases differ — roadmap Phase 4 is
"Instant previews," not yet started. These viewer features are the post-engine
*feature backlog* the roadmap defers until the engine is stable.)

All tests green: **~89 passing (+1 ignored)**, `clippy --all-targets -D warnings`
(incl. `incompatible_msrv`) clean, `fmt` clean — on every commit.

## Branch / how to review
- Everything is on branch **`feature/phase3-prefetch-engine`** — **NOT pushed**,
  `main` untouched. **22 commits**, one per step / per review-fix. Read in order;
  commit messages explain each.
- Plan/decisions for the engine: **`.taskmaster/docs/phase3-plan.md`**.
- Feature status + deviations: **`.taskmaster/tasks/tasks.json`** (#1, #3, #4, #5
  are `done` with detailed completion notes; #2, #6, #7, #8, #9, #10 pending).

## Keymap (current)
```
space            next photo            ⌫              previous photo
← ↑ ↓ →          pan (hold; accelerates)
= / -            zoom in/out (hold; accelerates; numpad +/- too)
0 / 8 / 9        scaling mode: original 1:1 / fit / fill
r / Shift+R      rotate 90° cw / ccw (per-image, RAM-only)
i / Shift+I      info panel / full-EXIF "nerd" panel
esc              quit
```
(Recursive `R` intentionally dropped — recursion comes from how the app is
invoked, e.g. a future Explorer "open folder" entry. `enter` random nav is still
unwired.)

## Run it
```
cargo run -p pb-app --release -- "D:\Pictures" -r           # fullscreen, recursive
cargo run -p pb-app --release -- "<leaf folder>" --windowed # dev window
cargo run -p pb-app --release -- "<folder>" --metrics       # stage timings on exit
```

## Architecture
```
crates/pb-core    pure nav/shuffle/prefetch/cache + ResidentRing (no I/O, no GPU)
crates/pb-decode  ImageDecoder (zune-jpeg) + decode-to-fit + EXIF (orientation, metadata)
crates/pb-render  wgpu presenter; ViewTransform (view.rs); UploadStrategy (upload.rs)
crates/pb-app     winit loop, decode_pool (priority workers), hud.rs, metrics.rs
```

## Phase 3 — the prefetch engine (DONE, codex-reviewed clean)
Decode + I/O are off the event loop on a priority worker pool; neighbors are
prefetched into a **byte-budgeted (~1.5 GB) resident GPU texture ring**; a keypress
is a **rebind, not a decode**. Advance is **gated on readiness** — every photo is
shown in order; a miss holds the previous frame until its decode lands. Both Fit
and Original/Fill modes use this async engine (Original was a sync cliff, now fixed).
- Key files: `pb-app::decode_pool`, `pb-core::ring::ResidentRing` (epoch-validated
  reserve/mark_resident + displayed-slot pin + byte budget), `pb-render::upload`
  (StagingUpload = `copy_buffer_to_texture`, not `write_texture`; row-banded).
- **Codex converged clean** over 7 rounds (15 P1/P2 fixed: nav edge cases,
  failure/Original stalls, a visible-skip, drain ordering, staging limits, the 1.80
  MSRV, mixed-folder VRAM). The gated-advance/failure paths are subtle —
  re-read `advance`/`about_to_wait`/`drain_results` in `main.rs` before changing them.
- **Deferred:** photon-accurate keypress→photon (DXGI `GetFrameStatistics`, behind
  a `metrics` feature) — the only Phase-3 step not done.

## Viewer features — tasks.json #1/#3/#4/#5 (DONE, NOT verified/reviewed)
A pure, tested **`ViewTransform`** (`pb-render::view`) composes scaling mode +
rotation + zoom + pan → placement + UVs; the renderer draws from it, so
rotation/zoom/pan are perf-neutral GPU transforms (no re-decode).
- **Scaling modes** `0/8/9` original/fit/**fill** (#4). Global/sticky; Fill &
  Original decode full-res (byte budget bounds VRAM); a mode switch bumps the
  geometry epoch and re-buffers neighbors (one mode's worth resident at a time).
- **Rotation** `r`/`Shift+R` (#1). Per-image RAM map; identity drops the entry.
- **Zoom** `=`/`-` + **pan** arrows (#3). Hold-to-act with a **time-based
  exponential acceleration ramp**; pan clamped to image bounds. Tunable constants
  at the top of `main.rs` (`ZOOM_MIN/MAX_RATE`, `PAN_MIN/MAX_SPEED`, `*_RAMP_SECS`).
- **Full-EXIF "nerd" panel** `Shift+I` (#5). Mutually-exclusive `InfoMode`; every
  EXIF tag via `pb_decode::read_exif_fields`, read on-demand from RAM (privacy).

## ⚠ Needs owner verification (couldn't be driven headless)
Builds + unit tests + a windowed startup smoke (clean, no panics) only — **no
keypress was ever driven**. Please run it and confirm:
1. **Hold-to-fly** (`space`): flies on the corpus, every photo shows, a miss holds
   (no blank/garbage), reverse is cheap. Compare felt speed vs `main` on 24–45 MP.
2. **Interactions:** pan (arrows) + zoom (`=`/`-`) feel/acceleration; `9` fill crop;
   `0` 1:1 pan; `r`/`Shift+R` rotation (incl. portrait aspect); `Shift+I` EXIF panel
   content + placement (it's **bottom-right/tall**, not top-right — easy to move).
3. **VRAM** on the big corpus (`RING_BUDGET_BYTES` in `main.rs`); `--metrics` p50/p95/p99.

## ⚠ Codex review still pending on Phase 4
Phase 3 is reviewed; the Phase-4 feature commits are **not** (hit the OpenAI usage
limit). When it resets, run:
```
& "C:\Users\jdlien\.codex\packages\standalone\releases\0.142.3-x86_64-pc-windows-msvc\bin\codex.exe" exec review --base main
```
(Reviews the whole branch vs `main`. The earlier-session rounds are in the scratchpad.)

## What's next / deferred
- **Run codex on Phase 4**, fix findings, then push the branch / open a PR.
- **Deferred refinements** (in tasks.json): Tier-2 re-decode on deep zoom (crispness),
  zoom-about-pan-focus (vs center), carry-view-position-to-next-photo, EXIF scroll +
  top-right anchor, mtime line.
- **Pinned latent bug** (`#[ignore]`d test): random-prefetch cycle boundary in
  `pb-core::prefetch` — fix when `enter`/random nav is wired (`phase3-plan.md` §7).
- **Engine perf (measure first):** recycled staging-buffer ring; fixed-size ring
  slots + sub-rect UVs; the DXGI photon timing (Phase 3.5b).
- **Feature backlog (`tasks.json`):** #2 privacy/no-trace, #6 esc-teardown, #7 help
  overlay, #8 configurable keybindings (TOML — would unify the keymap), #9 recursive
  ordering, #10 feedback toast. Incremental folder scan (owner-deferred, post-engine).

## Environment / gotchas
- `cargo` is at `~/.cargo/bin` (PATH-prepend: `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`).
- Green bar: `cargo test --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo fmt --all -- --check`. MSRV is **1.80** (`rust-version` in Cargo.toml): no
  `Option::is_none_or` (1.82+) — use a plain `match`; `is_some_and` (1.70) is fine.
- GPU render/round-trip tests run on the RTX 5090. Don't launch the **fullscreen**
  app from automation (use a short `--windowed` `Start-Process` + kill; quote paths
  with spaces). Display: 7680×2160 @ 120 Hz (the smoke box reported 60 Hz windowed).
- `D:\Pictures` is the real corpus; photos live in subfolders → use `-r`. A small
  test folder used for smoke runs: `D:\Pictures\1990s\1990-12-24 - Christmas1990` (11).
- Line endings: git warns LF→CRLF (harmless); `Cargo.lock` is committed.
</content>
