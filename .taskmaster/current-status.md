# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-17 (rev 10). Supersedes rev 9 (subtitle/audio, Windows — that work
is a **separate in-flight track**, summarized at the bottom; its detail lives in git history
+ `.taskmaster/docs/90-*` / `98-*`). **This session is macOS: the door card shipped, then a
performance investigation.** Active work: **task #106 (performance)**, starting **#106.2**._

---

# ✅ SHIPPED this session (all pushed to main)

| commit | what |
|---|---|
| `d91666a0` | **The archive door card, on macOS** (task #105 phase 3) — SwiftUI `DoorCardView` + FFI (`DoorCardFfi`, `door_art_*` free fns, `thumb_archive`, `key_is_bound`), overlay slot, archive thumb cells. Plus polish: artwork crop fix + bigger folder, `Cmd+↓`/`Alt+↓` = Open (Finder), panel shadow dialed down. |
| `3d87006e` | Fixed the `pb-mac-ffi` password-recheck test (7z went async in #102; the test now pumps the worker) + #105 Phase-5 cleanup (most deletes already landed earlier). |
| `fdcedd16` | **`pb_app_core::perf`** — episodic latency timers (open→first-photo, open→all-cached, resize→on-screen), `PB_PERF` env, folded into `--metrics`. |
| `dc/51d5…` | Task #106 tracking + refinements. |

**Door card #105 status:** subtasks 1,2,5 done; **3 (macOS) is `review`** — owner tested live
and it "looks great"; the only unseen bit is window-centering with both side panels open, and
the **egui/Windows half** (window-centring + door card) is inspection-only here (`pb-app`
won't build on macOS — Windows agent / cross-check should confirm). Subtask 4 (blaze perf
gate) still pending. **HARD CONSTRAINT: the macOS GUI does not come up in this agent env** —
screen capture returns the wallpaper, `onAppear` never fires, a launched app produces no
output. Verify SwiftUI via the offscreen `--pb-door-shot <dir>` `ImageRenderer` harness
(runs in `App.init`, before any scene); real-window smoke tests are the owner's.

---

# 🎯 ACTIVE — Task #106: fast archive opens, instant resize, instant thumbnails

**Prime directive: the app must FEEL fast. We measure, not guess.**

> **📋 THE EXECUTION BLUEPRINT: `.taskmaster/plans/106-performance-archive-zoom.md` (rev2,
> Codex-reviewed).** Read it first — it has the full design, the Codex (gpt-5.6 xhigh) findings
> folded in (typed representation, content-gen fencing, synchronous re-present, real eviction,
> scheduler isolation), the decided answers, and the acceptance-test list. The summary below is
> the orientation; the plan is authoritative. **Owner: start a fresh context to execute.**

## The measured baseline (owner ran `PB_PERF=1` on an SMB `album.zip`, 8 × ~36 MP JPEGs)

| metric | time |
|---|---|
| **open → first photo** | **7068 ms** |
| open → all 8 cached | 8302 ms |
| resize Fit↔1:1 | 711–993 ms |

**The shape is the finding:** the first photo is ~85 % of the whole open — the other 7 cache
in **+1.2 s**. So metric 1 is a **large serial cost paid once** (archive-open + the first
39 MB read over SMB), *not* per-photo throughput. Resize ≈ the SMB re-read + re-decode.

## The plan (5 subtasks in `tasks.json` #106)

1. **Encoded-byte cache** in front of `source.bytes()` — kills the re-read on resize /
   preview→full / revisit / OCR / Describe. Bounded RAM-only LRU, archive-cheap, RAM-dropped
   on nav/Esc (privacy #2). Fixes resize (~0.8 s → decode-only).
2. **⏳ NEXT — break down the 7 s open→first-photo.** Sub-instrument metric 1 into phases to
   find where the 6 s goes. (Details below.)
3. **Thumbnails: keep a tiny window warm when parked.** Owner call: DON'T always-capture
   everything (thumbs off by default — bad trade for never-users). When *parked*
   (`held_nav().is_none()`), derive thumbs for **current + next 2–3** already-resident items
   off-thread — pure downsample, zero source reads — so opening the panel shows neighbors
   instantly instead of a placeholder wall. Blaze pays nothing; RAM capped.
4. **First-image bandwidth throttle** — only if #2 shows read contention (vs a fixed cost).
5. **Preview-first for big JPEGs via the embedded EXIF thumbnail** — the real metric-1 win.
   (Details below.)

## Load-bearing perf findings (verified this session — don't re-derive)

- **Every image decode re-reads the source.** `decode_item_cancellable` (`engine.rs:512`)
  calls `source.bytes(item)` fresh every time — no byte cache in the pool or anywhere. So a
  resize (epoch bump → re-decode) re-reads the whole 39 MB entry.
- **ZIP is LAZY; 7z is EAGER (owner had these flipped).** `ZipSource::open` reads only the
  central directory + a local header per entry; `bytes(i)` inflates **one entry on demand**.
  It's **7z** that decompresses the *whole* archive to RAM on open (solid). So the ZIP's 7 s
  is **not** a full-unzip — it's ~all the single 39 MB entry read over SMB + ~0.3 s decode.
  ⚠ A ZIP is **not** `background_open` (`kind.rs:50`), so `scan::open_archive` runs **sync on
  the event loop** — #106.2 must check that isn't itself a stall.
- **The EXIF-thumbnail preview-first opportunity is real.** Big JPEGs embed a small thumb
  near the file start (owner's file: `JPEGInterchangeFormat 240, len 22467` = 22 KB). We have
  `pb_decode::exif_thumbnail` but use it **only for the thumbs strip** — the main viewer
  decodes the full JPEG and shows nothing until done. Read ~128 KB → show the thumb in
  ~100 ms → full lands + sharpens. Needs partial entry reads + a JPEG preview-first path.
- **Thumbnails re-read because capture is gated.** `thumbs_capture` (`app_core_impl.rs:2160`)
  already downsamples every decoded full — but `if !self.thumbs.enabled` (`thumbs.enabled`
  only flips true on the **first panel open**, `thumbs.rs:107`). So browse-then-open throws
  the thumbs away and re-decodes all N from source via `Job::thumb`.
- **The prefetch already prioritizes the on-screen photo** (`request_prefetch` sharpen/head
  tier). `mark_resolved` (`app_core_impl.rs:5988`) is THE present choke point (all present
  paths funnel through it) — that's where the perf `presented()` hook lives.

## The PB_PERF harness (how to measure)

`PB_PERF=1` → live `[perf] …` lines to stderr; works on the macOS host (unlike winit-only
`--metrics`). Run the executable directly so stderr is captured:
```
PB_PERF=1 "target/swift-host/release/Blaze Viewer.app/Contents/MacOS/Blaze Viewer" \
  <path-or-archive> 2>/tmp/pb-perf.log
```
Pure logic tested in `perf.rs`; wiring tested headless in
`perf_hooks_fire_from_the_real_present_and_resize_paths` (reads episodes back out of the
`--metrics` recorder, since the GUI can't run here).

## #106.2 — what "break down the 7 s" means (the next concrete step)

Split metric 1 into phases: **open-cmd → archive-open done (central-dir read) → first
`bytes()` read → first decode → present.** Two high-value cuts:
1. In `decode_item_cancellable` (`engine.rs`), **time `source.bytes()` vs `decode_named_bytes`
   separately** and log under `PB_PERF` (with item + KB) — proves the 39 MB SMB read is the
   cost, not the decode.
2. Time `scan::open_archive` (the central-dir read) — proves the sync open isn't a stall.
   (`perf_on()` / the env check lives in `app_core_impl` / `perf.rs`; `engine.rs` is in the
   same crate so it can gate on the same env.)

---

# Carried-forward notes (cross-cutting — keep)

- **Commit + push directly to main** (owner-authorized); **fetch/merge origin/main first** — a
  parallel **Windows agent** also pushes there. Stage explicit paths (avoid `add -A` when
  unrelated files are dirty; the perf/door work staged cleanly).
- ⚠ **The owner drives the app while you work** (their instance may lock the build). On
  Windows check `Get-Process blazeviewer` before killing; a separate `CARGO_TARGET_DIR` under
  the scratchpad builds without fighting them.
- **`pb-app` (winit/egui shell) does NOT build on macOS** by design — target-os guard. So the
  egui half of shared changes is inspection-only here. Windows cross-check from the Mac:
  `cargo check -p pb-app --target x86_64-pc-windows-msvc` after two temp manifest edits (blake3
  `pure` as a DIRECT dep + `ureq` `default-features=false` in both crates); restore + `git
  checkout Cargo.lock` after.
- ⚠ **`cargo test --workspace` can't build `pb-app` on macOS** → `--workspace --exclude pb-app`.
- **macOS build/run:** `./scripts/build-swift-host.sh` → `target/swift-host/release/Blaze
  Viewer.app`. `--pb-door-shot <dir>` shoots the door card offscreen. `PB_PERF` / `PB_TRACE`
  gate stderr diagnostics.
- **The pre-existing `pb-mac-ffi` failure is GONE** (fixed in `3d87006e`); the suite is green.

---

# Separate in-flight track — Subtitles / Audio (Windows, the other agent)

Not this session. Summary so the knowledge isn't lost:
- **Shipped:** subtitle display + Settings tab + the **Playback menu + Subtitle-Track flyout**
  on Windows (`756cfff`).
- **NEXT there:** the **Audio flyout**, blocked on an **FFmpeg→MF stream-order bridge** — MF
  enumerates audio streams in a *different order* than FFmpeg/the container, so a naive wire
  would tick the right track and play the wrong one. Bridge design is settled (match on
  lang/codec/channels, set `TrackLocator::MfStream(ordinal)`); `MfAudioDecoder::open_track` is
  done + tested (`d364d2e`).
- **Authoritative docs:** `.taskmaster/docs/90-presenter-and-style-contract.md` (subtitles),
  `98-phase0-spike-findings.md` (MF track-catalog limits — read before any track work),
  `video-playback-overhaul.md` (R1–R12).
- **Also unfiled:** ⚠ **AC-3/E-AC-3 doesn't decode on Windows** (`0xC00D36B4`) — many DD/DD+
  films play silent; worth its own task.
