# PhotoBlaze — Current Status (session handoff)

_Last updated: 2026-06-27 (overnight autonomous session)._

Phase 3 — **the prefetch engine ("hold a key and fly")** — is implemented. Decode
is off the event loop, neighbors are prefetched into a resident GPU texture ring,
and a keypress is a **rebind, not a decode**. Builds green, **84 tests pass**,
clippy (incl. `incompatible_msrv`) + fmt clean. **Not yet owner-verified for fly
behavior** (see below).

**Codex review: converged clean.** Ran `codex exec review --base main` iteratively;
it found **15 issues across 6 rounds** (all P1/P2 — nav edge cases, failure/Original
stalls, a visible-skip in the gated loop, drain ordering, staging buffer limits,
the 1.80 MSRV) — **all fixed and re-verified**, and round 7 reported "no blocking
correctness issues." Each fix is its own commit (`git log`). Worth knowing: several
bugs were in the gated-advance/failure interactions, which are subtle — re-read
`advance`/`about_to_wait`/`drain_results` in `main.rs` if you change them.

## Branch / how to review
- Work is on branch **`feature/phase3-prefetch-engine`** (NOT pushed; `main`
  untouched). ~13 commits this session, one per step / per review round — read in
  order: `3.0` harden · `3.1` staging upload · `3.3(core)` ResidentRing ·
  `3.2/3.3(render)` renderer ring · `3.2/3.3/3.4` engine wiring · `#4` fit geom ·
  then 6 codex-review fix commits.
- Full plan + decisions: **`.taskmaster/docs/phase3-plan.md`** (revised twice after
  two reviews; §5 has the per-step status, §6 the resolved decisions).

## What got built (Phase 3)
- **`pb-app::decode_pool`** — priority worker pool (capped 2–8), cancellation,
  dedup, byte-budget backpressure; injected decode fn (6 concurrency tests).
- **`pb-core::ring::ResidentRing`** — pure item↔slot bookkeeping: `Empty/Pending/
  Resident` states, epoch-validated `reserve`/`mark_resident`, displayed-slot pin
  (10 tests incl. a 20k randomized invariant stress).
- **`pb-render::upload`** — `UploadStrategy` seam; `StagingUpload` replaces
  `write_texture` with `copy_buffer_to_texture` (round-trip test on an unaligned
  width covers the 256-byte row-padding path).
- **`pb-render` ring** — `reserve_ring`/`upload_slot`/`present_slot` on the
  `Renderer` trait; v1 slots are **image-sized** (full UVs, no bleed); fixed-size +
  sub-rect-UV (zero prefetch-alloc) is the documented next optimization.
- **`pb-app` wiring** — gated-advance state machine: **every photo shown in order;
  a miss holds the previous frame until its decode lands** (fly = min(refresh,
  decode)). Epoch bumps on resize/fit-toggle discard stale-geometry decodes.
  Original (1:1) mode stays synchronous, outside the ring.
- **`pb-app::metrics`** — opt-in (`--metrics`) per-stage timing (decode/upload/
  render) + tested `percentiles`; nothing written to disk (privacy task #2).
- **Hardening** — focus-loss held-key clear; first-frame re-decode at true size;
  `cargo fmt` drift cleared (fmt now part of the green bar).
- **tasks.json #4** (subtask 4.2): pure `cover_rect` (fill) + `original_rect`.

## Run it
```
cargo run -p pb-app --release -- "D:\Pictures" -r              # fullscreen
cargo run -p pb-app --release -- "<leaf folder>" --windowed    # dev window
cargo run -p pb-app --release -- "<folder>" --metrics          # print stage timings on exit
```
Keys unchanged: `space`/`→` next · `⌫`/`←` prev · `0`/`o` fit↔1:1 · `i` info · `esc`.

## ⚠ Needs owner verification (couldn't be done headless)
1. **The actual fly experience.** A windowed smoke run started cleanly on 11 real
   JPEGs for 6 s with no panics/decode errors, but **keypress navigation / hold-to-
   fly was not driven**. Please hold `→` through `D:\Pictures -r` and confirm: it
   flies, every photo shows, a miss holds (never a blank/garbage frame), reverse is
   cheap. Compare felt speed vs `main` on the 24–45 MP wedding folders.
2. **VRAM**: ring budget is ~1.5 GB (`RING_BUDGET_BYTES` in `main.rs`) → ~16–32
   fit-slots on the 7680-wide display. Watch VRAM on the big corpus.
3. Run with `--metrics`, hold a key, `esc`, and eyeball the decode/upload/render
   p50/p95/p99 printout (debug build has a console).

## What's next
- **3.5b instrumentation (deferred):** photon-accurate keypress→photon via DXGI
  `GetFrameStatistics` (unsafe wgpu-hal downcast, behind a `metrics` feature) +
  PresentMon validation — the only Phase 3 step not done.
- **Optimization (measure first):** recycled staging-buffer ring (zero per-upload
  alloc); fixed-size ring slots + sub-rect UVs (zero prefetch-alloc).
- **Known latent bug (pinned, ignored test):** random-prefetch cycle boundary in
  `pb-core::prefetch` — fix when random/`enter` nav is wired (`phase3-plan.md` §7).
- **Deferred (owner-confirmed):** incremental folder scan, after the engine.
- **Feature backlog (`tasks.json`):** #4 finish (wire 8/9 keys + Fill decode-res),
  #8 keymap, #1 rotate, #3 zoom, etc.

## Environment / gotchas
- `cargo` is at `~/.cargo/bin` (PATH-prepend: `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`).
- Gate: `cargo test --workspace`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --all -- --check`.
- GPU render/round-trip tests run on the RTX 5090. Don't launch the **fullscreen**
  app from automation (use a short `--windowed` `Start-Process` + kill; quote paths
  with spaces). Display: 7680×2160 @ 120 Hz (smoke box reported 60 Hz windowed).
- `D:\Pictures` is the real corpus; photos in subfolders → use `-r`.
