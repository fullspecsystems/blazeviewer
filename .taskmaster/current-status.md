# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-17 (rev 12). **This is the launchpad for the next session: EXECUTE the
performance plan (task #106).** The design/investigation is done, Codex-reviewed, and pushed;
what remains is implementation. Two other tracks are in flight in parallel (Windows door
gating / Copy, and Subtitles/Audio) — preserved below so they aren't lost, but **#106 is the
job for a fresh context.**_

---

# ▶️ START HERE — execute Task #106 (performance: instant zoom, fast opens, instant thumbs)

**Prime directive: the app must FEEL fast. We measure, not guess — every claim is a `PB_PERF`
number, before and after.**

> **📋 THE BLUEPRINT (read it first, it is authoritative):**
> **`.taskmaster/plans/106-performance-archive-zoom.md` (rev2, Codex gpt-5.6 xhigh reviewed).**
> It has the full design, the review findings folded in (typed representation, content-gen
> fencing, synchronous re-present, real eviction, scheduler isolation, gigapixel ceiling), the
> decided answers, per-item file:line anchors, and the acceptance-test list. This status page is
> orientation only.

### The opening move

Start with **#106.7 — hold full-res for the parked window** (the biggest felt win; it also
subsumes #106.6). Build it in the plan's order — the foundation first:

1. **Typed `Representation::Fit { geometry_epoch } | Original`** threaded through `Want` →
   pool dedup key (`decode_pool.rs:181` dedups by `(item, purpose)` today — add representation)
   → `Outcome` → `ResidentRing` slot metadata (`ring.rs` records only `item` today). *This is
   the load-bearing foundation — `preview_resident == false` does NOT mean full-res
   (decode-to-fit also sets it false, `common.rs:84`).*
2. **Split `content_gen` from the geometry `epoch`.** An `Original` may cross a geometry change
   (resize/scale) but must be **purged on any content change** (deck rebuild `:3068`,
   save-rotation `:627`, delete/undo, source replace). Bypassing the epoch blindly presents the
   old deck's item N as the new deck's — the sharpest bug in the review.
3. **Parked full-res tier** (only when `held_nav().is_none()`): for `current ± full_res_radius`
   (sequential; order **current → compare-pin → neighbours**), decode the Original **once** and
   derive the Lanczos Fit from that one buffer (no double-decode; no bilinear quality drop). It
   **replaces** the fit sharpen for those items, strictly below the on-screen sharpen, with an
   occupancy cap + debounce.
4. **Synchronous re-present** on the geometry change: if the current item has a valid resident
   Original, rebind + `mark_resolved` immediately (no decode) — *or the app hangs
   `target_pending` forever* (`refresh_after_geometry_change:5085` never calls
   `try_present_target`).
5. **Real eviction on upgrade** + corrected byte accounting (HDR 8 B/px; `held` outside budget;
   `set_slot_bytes` doesn't evict, `ring.rs:218`). One typed ring + an explicit renderer
   retain/remap API — **not** a parallel `HashMap` (`reserve_ring` destroys the vector today).
6. **Exclusions + the gigapixel ceiling** (§8, §9): skip video/door/SVG; a full-res byte/pixel
   ceiling so we never decode/retain a monster (stays fit-only). 24–100 MP is in scope.
7. **The setting:** `full_res_radius: u8` (default 1) on `pb_app_core::Settings`, surfaced in the
   egui + SwiftUI panes (mirror `scale_mode`).

**Then:** #106.5 (EXIF preview-first → kills the cold 7 s) → #106.3 (thumbnail warm-window) →
#106.1 (encoded-byte cache; cold/re-open only). Each behind its seam, each `PB_PERF`-measured.

### Acceptance & measurement

The plan's test list is the bar (typed-rep survives geometry / purges on content_gen;
same-index deck replacement; no-decode + `target_caught_up` on a retained toggle; exact-budget
eviction; compare-pin retention; rapid-nav no-pile-up; SVG/video/door exclusion; renderer
remap; a 30000×20000 image opens without OOM). Prove the win with:
```
PB_PERF=1 "target/swift-host/release/Blaze Viewer.app/Contents/MacOS/Blaze Viewer" <archive> 2>/tmp/pb-perf.log
```
Target: `resize→on-screen` for a retained Original drops from ~400 ms to **~0 ms** (rebind).

---

# The measured baseline (owner, `PB_PERF` on an SMB `album.zip`, 8 × ~36 MP JPEGs)

| metric | cold | warm |
|---|---|---|
| open → first photo | **7068 ms** | — |
| open → all 8 cached | 8302 ms | — |
| resize Fit↔1:1 | 711–993 ms | **390–474 ms** |
| per-item read / decode | — | **~37 ms** read (warm OS cache) / **~400 ms** decode |

**The three findings (don't re-derive):**
1. The **cold read is the 7 s**, one time; warm the entry serves in ~37 ms. So the byte cache
   (#106.1) is a *cold/re-open* win only.
2. Warm, the **~400 ms decode is the whole steady-state cost.**
3. **Every Fit↔1:1 toggle re-decodes the WHOLE ring** (log: items 0–7 re-decode around each
   resize) — a geometry-epoch bump rebuilds the ring empty. This is the central slowness #106.7
   kills. Root cause: decode-to-fit discards the full-res (`common.rs:84`), so 1:1 has nothing to
   rebind. `base_scale` is `1.0`-of-texture for Original (`view.rs:110`).

---

# ✅ SHIPPED (Windows audio + seek, this arc — merged to main 2026-07-17)

The `feat/audio-track-selection` branch landed: **audio track selection + FFmpeg-first film
audio on Windows** (MF can't decode AC-3/E-AC-3/DTS, so films were silent — the trimmed FFmpeg
already ships the decoders at zero bundle cost; owner-confirmed clean), the **Playback ▸ Audio
Track** flyout, `WAVEFORMATEXTENSIBLE` speaker-mask sinks, off-thread switches, and the
**short-forward-hop for Windows seeks** (a +2 s arrow tap over SMB: ~1 s → 139 ms, owner
prioritized). The FFmpeg→MF locator "bridge" was a mistake and is DELETED. `PB_AUDIO_TRACE=1` /
`PB_VIDEO_DIAG=1` are the diag levers. **`scripts/build-windows.ps1` now defaults to
`ffprobe`** (a plain `cargo run` omits FFmpeg → silent films — the trap that bit twice).
Remaining in tasks.json: **#1** (MF poster deep-walk — Windows posters measure pure black on
films) and **#4** (further seek wins: codec-aware in-place seek for the recreate cases, then the
run-up convert-skip — the elephant for arbitrary/backward seeks).

# ✅ SHIPPED (macOS, this arc — all on main)

| commit | what |
|---|---|
| `d91666a0` | **Archive door card on macOS** (#105 phase 3) — SwiftUI `DoorCardView` + FFI + overlay + thumb cells; polish: artwork crop/bigger folder, `Cmd/Alt+↓` = Open, panel shadow reduced. |
| `3d87006e` | Fixed the `pb-mac-ffi` password test (7z async) + #105 cleanup. |
| `fdcedd16` | **`pb_app_core::perf`** — the `PB_PERF` episodic timers (open→first / all-cached / resize). |
| `5d8eebe1` | **#106.2** — read-vs-decode split + archive-open timing under `PB_PERF`. |
| plan+task commits | #106 plan (rev2, Codex-reviewed) + the tasks.json subtasks. |

`PB_PERF=1` works on the macOS host (stderr); pure logic + wiring are unit-tested
(`perf.rs`, `perf_hooks_fire_from_the_real_present_and_resize_paths`) — **the GUI can't run in
the agent env** (screen capture blocked, `onAppear` never fires), so the owner runs the real
numbers on the SMB volume.

---

# Carried-forward norms (cross-cutting — keep)

- **Commit + push directly to main** (owner-authorized); **fetch/merge origin/main first** —
  parallel agents (macOS + Windows) push there and have re-used task IDs (Copy was renumbered
  #106→#107). Stage explicit paths; the owner also edits the repo concurrently.
- ⚠ **The owner drives the app while you work.** A mid-test anomaly is often them, not a bug —
  A/B before believing it. On Windows check `Get-Process blazeviewer` before killing; a separate
  `CARGO_TARGET_DIR` under the scratchpad avoids fighting their build lock.
- **`pb-app` (winit/egui) does NOT build on macOS** (target-os guard) → the egui half of shared
  changes is inspection-only on the Mac; the Windows agent verifies it. Cross-check from the
  Mac: `cargo check -p pb-app --target x86_64-pc-windows-msvc` after two temp manifest edits
  (blake3 `pure` DIRECT dep + `ureq` `default-features=false` in both crates); restore +
  `git checkout Cargo.lock` after. ⚠ A macOS-side `AppCore` field add **will** E0063 the Windows
  `main.rs` struct literal — the `perf` field already caught this (`66c7ca6`).
- ⚠ **`cargo test --workspace` can't build `pb-app` on macOS** → `--workspace --exclude pb-app`.
- **macOS build/run:** `./scripts/build-swift-host.sh` → `target/swift-host/release/Blaze
  Viewer.app`. `--pb-door-shot <dir>` shoots the door card offscreen; `PB_PERF`/`PB_TRACE` gate
  stderr diagnostics. **Look at the output** — every door defect was visible and passed its tests.
- ⚠ **The Bash tool mangles `git show rev:path`** and eats a backslash in heredoc'd Python. Use
  PowerShell for `git show`; the Edit tool for Rust string literals.

---

# ⏸ Parallel track A — Windows door gating + Copy (still open; NOT #106)

The door card **renders correctly on Windows** (verified via `PB_SHOT_DOOR`), and the macOS
merge's build break is fixed (`66c7ca6`). Still owed there (detail in `tasks.json` #105.2 / #107
and git history):
- **Command gating on a door (#105.2):** `copy_image` decodes the 1×1 sentinel → `Ctrl+C`
  copies a transparent pixel. Gate OCR/Text, Describe/Ask, Compare **off** on a door via the
  `MenuState._enabled` pattern; Open/nav/Copy-Path/Reveal/Details/Delete stay on. Copy itself is
  fixed by #107 (offer the file, skip the image), not disabled.
- **#107 — "Copy" copies what makes sense:** relabel "Copy Image"→"Copy" (Windows `menu.rs:1269`
  + macOS `CoreModel.swift:1111`); emit file-only on a door; ⚠ Linux (arboard) falls back to
  path-as-text.
- **Interactive smoke tests owed:** `P` opens / `Alt+↑` climbs out; archive thumbnails;
  window-centring with both panels open; a folder of only archives (the case that once froze).

**Load-bearing door knowledge (egui):** a new `LibraryItemKind` opts **out** of byte reads
(positive `Image` guards, exhaustive matches, per-platform worklists); never `load_texture`
inside `data_mut`; an auto-sized anchored egui Window places from the *previous* run's rect;
artwork is subject-centred cropped and keeps its alpha; the retained overlay needs both
`overlay_panel_visible()` (draw) and `overlay_dirty` (rebuild); the size readout goes through
the scan worker, never the per-frame `info_line_parts` path (SMB stat blocks).

---

# ⏸ Parallel track B — Subtitles / Audio (Windows, the other agent)

- **Shipped:** subtitle display + Settings tab + the Playback menu + Subtitle-Track flyout on
  Windows (`756cfff`).
- **NEXT:** the **Audio flyout**, blocked on an **FFmpeg→MF stream-order bridge** (MF enumerates
  audio in a different order than FFmpeg — a naive wire ticks the right track, plays the wrong
  one). Bridge design settled; `MfAudioDecoder::open_track` done + tested (`d364d2e`).
- **Docs:** `.taskmaster/docs/90-presenter-and-style-contract.md`, `98-phase0-spike-findings.md`
  (MF limits — read before track work), `video-playback-overhaul.md`.
- **Also unfiled:** ⚠ AC-3/E-AC-3 doesn't decode on Windows (`0xC00D36B4`) — many DD/DD+ films
  play silent; worth its own task.
