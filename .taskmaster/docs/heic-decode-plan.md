# HEIC/HEVC decode — plan & handoff (the "make HEIC fly" track)

_Written 2026-06-27 at the end of a long session, low on context. This is the
authoritative plan for the next session on HEIC decode speed._

## TL;DR

Scrolling HEIC is already fast (instant blurry **previews** + on-land **sharpen**,
shipped & green). The remaining problem: the **full** HEIC decode (the sharpen)
takes ~250 ms–1 s per photo, which isn't PhotoBlaze-fast. Root cause is **not**
that HEVC is slow — it's that the **Windows WIC HEVC decoder serializes**
(measured **~1.7× across 8 threads** on a 32-core box; JPEG gets 4.3×). We have 31
idle cores and an idle 5090.

**Decision (owner, 2026-06-27): pivot HEIC/AVIF decode to CPU `libheif` behind the
`ImageDecoder`/`DecodeBackend` seam, A/B-able vs WIC. NVDEC is deferred** (the
48-tile grid below makes the GPU path the hard 80%; libheif does grids for free).

The win is nearly free architecturally: **the decode pool already runs 8 concurrent
workers** — they were all bottlenecked on WIC's single DXVA session. libheif
decodes are plain CPU (no shared GPU session), so the same 8 workers run *truly* in
parallel → ~8× throughput → enough to **prefetch full-res ahead of the user** (the
real goal), not just sharpen-on-land.

## What shipped this session (committed, green: 167 tests, clippy+fmt clean)

- **Preview-first for HEIC** (`pb-decode/src/wic.rs`): `allow_preview` →
  `GetThumbnail` (320×240, ~18 ms warm vs ~250 ms full). Reports the **primary
  frame's** resolution as `orig_*` (so the info panel is correct). WIC auto-orients
  the thumbnail (pass orientation 1).
- **Two-tier prefetch + on-land sharpen** (`pb-app/src/main.rs`): the window is
  fetched as previews; once **settled on a photo**, *only that one* is re-decoded
  at full res and the slot is upgraded in place (`present_slot` refresh keeps
  zoom/pan). Crucially we **only ever full-decode the on-screen photo, never
  neighbours you fly past** — at most one uninterruptible WIC decode is ever "in the
  way", the other 7 workers stay free for previews.
- Review fixes (Codex + the workflow review): RAW preview-only busy-loop (retain
  marks `upgrade_done`), Original-mode stale dims (re-`present_slot`), stale
  thumbnail metadata. Idle CPU measured 0% (no busy-loop).
- Pure-Rust additions: `pb-core::ring::set_slot_bytes` (in-place upgrade
  accounting), `decode_pool` per-job `preview` flag.

## Key findings (don't re-derive these)

1. **WIC HEVC serializes** (~1.7× / 8 threads; STA vs MTA made no difference — it's
   the decoder/DXVA session, not a COM-apartment bug). This is the wall.
2. **An iPhone HEIC is a 48-tile HEVC grid**, not one bitstream:
   - primary item = `grid` (id 49) → `iref dimg` → **48 `hvc1` tiles**, each
     **512×512** (`ispe`), assembled to **4032×3024** and cropped. All share one
     `hvcC` config.
   - **thumbnail = item 50** (320×240, `iref thmb`).
   - **item 51 = `auxl`/`auxC` aux, 2016×1512** — believed to be the **HDR gain
     map** (needs confirming; see preview spike).
   - plus `colr` (P3 ICC), `irot`.
   This is why NVDEC is hard (DIY grid demux + 48 decodes + stitch) and why libheif
   wins (handles all of it).
3. Warm full HEIC decode-to-fit ≈ 200–280 ms (12 MP); ~1 s suggests the owner's
   recents may be **48 MP** (14 Pro+). Confirm with a measurement.

## Next-session plan (libheif integration)

**Phase 0 — toolchain (OWNER ACTION; blocker).** No vcpkg/cmake present. The only
libheif DLLs found are app-private (Affinity). Owner installs:
`vcpkg` + `cmake`, then `vcpkg install libheif` (pulls libde265 for HEVC, dav1d for
AVIF), set `VCPKG_ROOT`. (Owner owns toolchain — decisions.md Q-4.) Decide static
vs dynamic link; if dynamic, the MSI must ship the DLLs (update `packaging.md`).

**Phase 1 — `LibHeifDecoder` backend.** Add `libheif-rs` (or `libheif-sys`).
Implement an `ImageDecoder` for HEIC+AVIF behind the existing seam. Reuse
`common::finalize_oriented` (libheif can hand back RGB(A) + apply orientation; keep
our `colr`/CICP color path, or use libheif's). Gate behind a cargo feature so a
broken C build never blocks the core (ADR-015 pattern), A/B vs `WicDecoder`.

**Phase 2 — measure (the validation).** Bench **concurrent** decode throughput,
libheif vs WIC, over the real corpus (12 & 48 MP). Target ≥ ~50/s so we can keep
the full-res window prefetched. (Use the throwaway-bench pattern from this session;
`--metrics` also reports decode p50/p95/p99.)

**Phase 3 — prefetch fulls ahead (the payoff).** If throughput clears the bar,
change `request_prefetch` so the **full** decodes fill the window ahead (not just
the displayed-only sharpen) — sharp images at scroll speed. The two-tier machinery
and `set_slot_bytes` upgrade path already exist; just widen `upgrade_item` from "the
displayed photo" to "the window, current-first", now that decode is fast + parallel.
Watch VRAM (the byte budget) and per-frame churn.

**Phase 4 (later/gated) — NVDEC.** Only if libheif still can't keep up at 48 MP.
The 5090 has multiple NVDEC engines (~20–40 ms/image once built). Cost: Rust HEIF
grid parser + cuvid FFI + tile stitch + NV12→RGB; copy-back first, CUDA↔D3D12
zero-copy interop (native-D3D12 backend) as v2. This is ADR-012's gated escalation.

## Preview-quality spike (owner flagged — "probably an easy lever")

Verify whether a **higher-res embedded preview** exists that beats the 320×240
thumbnail:
- Decode **item 51 (2016×1512 aux)** — is it the HDR gain map (expected) or a
  usable preview? (Needs libheif or a HEIF item extractor.)
- Check **non-iPhone HEICs** (the Sony `DSC*.heic` in `D:\Media\Pictures\test-images`
  and `…\iCloud Photos`) — cameras may embed a true mid-res `hvc1` preview.
- If any usable mid-res single-`hvc1` image exists, decode **it** for the preview: a
  ~6× sharper placeholder, one fast decode (no 48-tile grid). Big, cheap quality win.

## Deferred review findings (from Codex + the workflow review)

- **HDR HEICs bypass preview-first** (the HDR float branch returns before
  `allow_preview`). Real gap; needs SDR-thumb-preview → HDR-full upgrade (slot
  format change). Relates to the gain-map aux above.
- **Fill mode decodes full-res** (`decode_fit` returns `None` for Fill+Original);
  give Fill a cover-to-screen target.
- **Decode-pool byte budget is a soft cap** (workers check-then-add); reserve before
  decode. Largely moot now (displayed-only ⇒ ≤1 full in flight).
- **`DecodeKind::{Preview,Full}` in the pool key** — defensive; no current bug.
- **Sync load paths bypass preview-first** (startup/resize/mode → ~250 ms+ HEIC
  freeze on the event loop). Separate from scrolling.
- **No unit/property tests for the tier state machine** (it lives in the pb-app
  shell). Extract pure bits where possible (TDD).

## Useful commands / context

- Real HEIC corpus: `C:\Users\jdlien\Pictures\iCloud Photos\Photos` (24,475 imgs;
  quote the path — spaces split args), and `D:\Media\Pictures\test-images`
  (`IMG_0394.HEIC` = 12 MP landscape; Sony `DSC*.heic`).
- `cargo run -q --example decode -p pb-decode -- <file>` (decode report).
- GUI runtime checks need the owner (can't inject keypresses from automation; GDI
  capture is broken on the HDR desktop). Boot smoke: short `--windowed`
  `Start-Process` + kill; idle-CPU sample catches busy-loops.
