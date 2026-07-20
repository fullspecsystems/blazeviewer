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

**Verified (macOS, this session):** _pending — filled as steps land._

**Not verified — Windows must check:** step 4 (winit placeholder + notice) and that the
`DecodedImage.recovered` field addition compiles in `pb-app` (struct literal in
`pb-app/src/clipboard.rs` test helper — Windows-only). Mac cannot build `pb-app`; a Windows
`cargo clippy -p pb-app --all-targets` is required after step 1.

**Cross-platform debt:** the `recovered` field touches `pb-app/src/clipboard.rs` (Windows shell
test helper) — added blind from the Mac, must compile-check on Windows. Step 4 is owed on winit.

**Claimed:** macOS session holds steps 1–3. Windows owns step 4 + the pb-app cross-check.
