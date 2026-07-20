# #127 — Lenient decode + recovery ladder + graceful failure for malformed images

_Started 2026-07-20 (macOS session). Triggered by a real file: a JPEG downloaded from a
government agency (`IMG_1340.JPG`, a speeding-ticket photo) that sat under the loading pie
forever on macOS and never displayed._

## The bug, diagnosed

The file is a 17.6 MP baseline JPEG whose tail is malformed (no `ffd9` EOI; ~2 MB for a Q90
17.6 MP image; stray bytes between segments). **Preview.app / `sips` / ImageIO decode it fine.**
Our JPEG backend, `zune-jpeg`, runs in **strict mode by default** and rejects it in ~100 µs with
`[strict-mode]: Extra bytes between headers` — a spec-pedantry complaint; the pixels are all
there. A viewer should be a *renderer*, not a *validator*.

Two failures stack:

1. **No fallback.** `decode_bytes` picks the *first* backend whose `can_decode()` matches (zune,
   on the JPEG magic) and returns its error. ImageIO (macOS) / WIC (Windows) sit **after** zune
   in the list and are never tried — and their `can_decode` is ISOBMFF-only anyway, so they'd
   never volunteer for a `.jpg`.
2. **Stuck pie.** The core has `present_failed` (resolves the pie, sets a "decode error" title),
   but the macOS shell doesn't react to the failed/resolved state, so the spinner never stops.

### Evidence (measured 2026-07-20, isolated harness)

| Input | zune strict | zune lenient | ImageIO (`sips`) |
|---|---|---|---|
| `IMG_1340.JPG` (real) | ERR "Extra bytes between headers" | **OK** full 52 MB | OK |
| crafted stray-bytes-after-SOI | ERR "Extra bytes between headers" | **OK** | OK |
| truncated tail (−20 %) | ERR "Exhausted data" | ERR "Exhausted data" | **OK** |

→ The ladder **strict zune → lenient zune → OS codec** is exactly right: each rung recovers what
the previous can't. Reaching a fallback rung *is* the signal that the file was malformed.

## The design — one coherent thing, not four patches

**The recovery ladder.** Try zune **strict** first (a pass = provably clean, ~free for good
files). On error, walk: **lenient zune → OS codec (ImageIO/WIC) → image crate.** First success
returns the image flagged `DecodedImage.recovered = Some(reason)`, where `reason` is the original
strict error. Only if *every* rung fails do we return `DecodeError` → the graceful "can't
display" placeholder. That one flag drives the details-panel notice; the placeholder is the floor
underneath.

This unifies the four asks: #1 (lenient) + #2 (fallback) + the details-panel notice are the
ladder; #3 (kill the pie / placeholder) is the failure floor.

## Steps

1. **pb-decode — the ladder + the flag** (subtask 1). Add `recovered: Option<String>` to
   `DecodedImage` (43 construction sites — mechanical, compiler-listed). zune.rs: strict-first
   then lenient, capturing the strict error as the reason. Dispatch (`decode_bytes_inner`): on
   the chosen backend's error, try the OS codec's `decode` directly (bypassing its narrow
   `can_decode`) then the image crate; stamp `recovered`. Tests: crafted stray-bytes JPEG
   recovers via lenient with the flag set; a fabricated truncation recovers via the OS-codec rung
   (macOS/Windows only). **Fully testable + verifiable on this Mac.**
2. **pb-app-core — carry it + notice + reliable failure** (subtask 2). A RAM-only
   `recovered: HashMap<usize, String>` on `AppCore`, populated in `drain_results` from the
   `Outcome`'s image; cleared with `failed`/on nav reset. Details/info panel renders a notice
   line. Confirm a truly-failed decode reaches `present_failed`. Build-verifiable here; panel
   wording per-shell.
3. **macOS shell** (subtask 3). React to failed/resolved: stop the pie, show a "can't display
   this image" placeholder; render the recovery notice in the inspector. Build + owner-smoke here.
4. **winit shell** (subtask 4). Placeholder + notice parity — **cross-platform debt**, cannot be
   verified from the Mac; leave to the Windows session.

## Handoff

**Verified (macOS, this session):**
- **Step 1 (ladder):** committed `819fe56d`. Full pb-decode suite green (232), clippy clean.
  Proven end-to-end on the **real** `IMG_1340.JPG`: `decode_bytes` → `codec=JPEG 4864x3616
  recovered=Some("Extra bytes between headers")`. Crafted-fixture tests lock strict-clean vs
  lenient-recover through the whole dispatch.
- **Step 2 (notice + merge):** committed `fff18dc0`. The details "Recovered" row shows in the
  mac inspector (built once in `exif_rows`, so winit's HUD table gets it too). Fixed the
  preview-then-full ordering that hid the notice (the clean embedded thumbnail seeded
  `meta_cache` first; `drain_results` now merges the later full decode's flag). Owner confirmed
  the image displays; details notice verified live (the Original decode arrives recovered=Some
  and merges). Tests: accessor, details row, merge regression. Clippy clean incl. `pb-mac-ffi`.
- **Step 3 (graceful failure):** committed `9bdd3556` (then merged with #128). The stuck-pie
  root cause found + fixed — a PARKED failed target wasn't re-resolved after a geometry-epoch
  bump (`try_present_target` runs only under a held nav key), so `presented_epoch` stayed stale
  and `tick_pie` spun forever. `resolve_parked_failure()` re-stamps it each tick (regression
  test). Added a `failed_reason` map, `current_decode_error()` + `current_file_name()` accessors,
  a Details "Error" row, canvas-blanking in `present_failed` (`clear_image`), and a native
  `DecodeErrorView` placeholder in the **door-card panel language** (header "Error Displaying
  Image" + divider + broken-image glyph + filename over reason). **Owner-confirmed the look
  ("right vibe").** Two owner iterations landed: (a) restyle from a bare glyph to the door-card
  card; (b) the `held_nav`-gated clear left the previous photo up when navigating *to* a corrupt
  file — removed the gate so `present_failed` always blanks; added the filename; renamed the
  header. 876 pb-app-core tests green, clippy clean on the post-merge tree.

**Not verified — Windows must check (cross-platform debt):**
1. **Two `AppCore`/`DecodedImage` struct-literal fields added blind from the Mac — the
   documented trap that already broke `main` once:** `DecodedImage.recovered`
   (`pb-app/src/clipboard.rs` test helper, in `819fe56d`) and `AppCore.failed_reason`
   (`pb-app/src/main.rs:819` struct literal). A Windows `cargo clippy -p pb-app --all-targets`
   (or the macOS→win cross-check) must confirm both compile. Both are mechanical `field: init,`
   additions mirroring the struct defs; low risk, but unverifiable from the Mac.
2. **Step 4 (winit parity):** the details "Recovered"/"Error" rows are shared (`exif_rows`) so
   winit's HUD table already gets them — but the winit shell has **no equivalent of the
   `DecodeErrorView` placeholder** (a genuinely-dead file on winit still shows black + a
   "decode error" title). The pie fix IS shared (core `tick`), so the stuck pie is gone on winit
   too. Only the centered placeholder graphic is owed on winit.

**Claimed:** macOS session holds steps 1–3 (done, pending owner smoke). Windows owns the pb-app
cross-check + step 4 (winit placeholder).
