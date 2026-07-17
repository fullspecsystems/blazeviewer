# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-17 (rev 11). Merges the macOS agent's rev 10 (below) with the
Windows-side door items it flagged for this agent. **The macOS door card shipped and looks
great**; the **egui/winit half is now being verified on Windows** (this session), and the
door command-gating + Copy work is still open here. The subtitle/audio track is separate,
summarized at the bottom (detail in git history + `.taskmaster/docs/90-*` / `98-*`)._

---

# ✅ SHIPPED on macOS this session (all pushed to main)

| commit | what |
|---|---|
| `d91666a0` | **The archive door card, on macOS** (task #105 phase 3) — SwiftUI `DoorCardView` + FFI (`DoorCardFfi`, `door_art_*` free fns, `thumb_archive`, `key_is_bound`), overlay slot, archive thumb cells. Plus polish: artwork crop fix + bigger folder, `Cmd+↓`/`Alt+↓` = Open (Finder), panel shadow dialed down. |
| `3d87006e` | Fixed the `pb-mac-ffi` password-recheck test (7z went async in #102; the test now pumps the worker) + #105 Phase-5 cleanup (most deletes already landed earlier). |
| `fdcedd16` | **`pb_app_core::perf`** — episodic latency timers (open→first-photo, open→all-cached, resize→on-screen), `PB_PERF` env, folded into `--metrics`. |
| `dc/51d5…` | Task #106 tracking + refinements. |

**Door card #105 status:** subtasks 1,2,5 done; **3 (macOS) is `review`** — owner tested live
and it "looks great"; the only unseen bit is window-centering with both side panels open, and
the **egui/Windows half** (window-centring + door card) is inspection-only on the Mac
(`pb-app` won't build on macOS). **← that is this Windows session's job (below).** Subtask 4
(blaze perf gate) still pending.

---

# 🪟 THIS SESSION (Windows) — door card verified on Windows; a build break fixed

**✅ The door card renders correctly on Windows** (`PB_SHOT_DOOR=1|long` → PNG): centred in
the window, adaptive width for long names with middle-elision, 162 pt artwork well-proportioned,
"ZIP Archive" header + separator + "Open (P)". The macOS centring + bigger-folder changes
landed right in egui.

**⚠ But the macOS merge broke the Windows build — fixed in `66c7ca6`** (the exact hazard this
section warned about: `pb-app` doesn't compile on macOS, so the Mac agent couldn't catch it):
- `AppCore` grew a `perf` field; `main.rs`'s struct literal didn't set it → **E0063**. Wired it
  like `AppCore::headless` (gated on `perf::env_enabled()`).
- the new whole-window centring left `PanelFrame.left_pane` with no reader → **dead-code
  warning** = a `clippy -D warnings` CI failure. Removed the field + its 5 setters.
- ⚠ **A `| tail` on the build masked cargo's non-zero exit** (pipe reports tail's 0), so the
  first "build" looked green and ran a **stale** binary. Capture the real exit code, or don't
  pipe the build.

**Still owed a real interactive smoke test** (the shot harness can't do these): `P` opens a
door and `Alt+↑` climbs out; archive **thumbnails**; **window-centring with both side panels
open** (the one bit the Mac owner couldn't see); a **folder of only archives** (the case that
used to freeze — `9110327`). Corpus: the doors test archives under `D:\Media` (RARs with real
photos). `Cmd+↓` never arrives on Windows (Win+↓ minimizes) — `Alt+↓` carries the Open alias.

## Door command gating (task 105.2 — still open on Windows)

`copy_image` (`app_core_impl.rs`) guards only on `displayed_item`, then decodes — on a door
that's the 1×1 sentinel, so **Ctrl+C silently copies a transparent pixel**. The *file* half
already works (`source.path(item)` is real → CF_HDROP is offered), so this is fixed by #107,
not disabled. Gate the rest via the existing `MenuState._enabled` pattern (`menu_state_from`
already takes `displayed_item`):

| | On a door |
|---|---|
| Open (`P`), navigation, Copy File Path, Reveal, Details, Delete | **work** — a door is a file |
| **Copy** | **works once #107 lands** — offer the file, skip the image; don't disable it |
| OCR / Copy Text, Describe / Ask, Compare (`compare_pin_cmd`) | **disable** — they'd scan/pin the sentinel |
| Rotate / Save rotation | already toasts honestly |
| Copy Image Details | **works** — the panel already shows a door's size + format |

## #107 — "Copy" copies whatever makes sense (renumbered from #106 to clear the collision)

Most of it already ships: the two-format clipboard (Windows CF_DIBV5 + CF_HDROP; macOS
`.tiff` + `.fileURL`), and macOS's menu bar already says "Copy". Real remaining work: relabel
"Copy Image" → **"Copy"** on Windows (`menu.rs:1269`) + the macOS context menu
(`CoreModel.swift:1111`); emit **file-only** on a door (`ClipboardPayload::File`); ⚠ **Linux**
(arboard, no file-list support) must fall back to **path-as-text** or it copies nothing. Full
detail in `tasks.json` **#107**. Non-goal (owner): a Copy Filename command — Copy File Path is
the useful one.

## Load-bearing door knowledge (egui — don't re-derive)

- **A new `LibraryItemKind` opts OUT of byte reads, not in** (`c19cfd6`). The no-read
  guarantee rests on typed dispatch **above** `source.bytes()`; guards are **positive `Image`
  + exhaustive matches** so the compiler names every site. ⚠ The worklist is per-platform (a
  `cfg(macos)` arm won't fail a Windows build) — so a leak can hide on the side you didn't run.
- **egui: never `load_texture` inside `data_mut`** (`9110327`, froze on a folder of archives).
  `data_mut` write-locks the whole Context; `load_texture` re-enters it. Pattern = read → load
  → insert (`pb_ui::icon::texture` has it right).
- **egui: an auto-sized anchored Window places from the PREVIOUS run's rect** (`b2a7a28`, the
  off-centre long-filename card). Compute the size yourself + anchor `LEFT_TOP`. ⚠ Test via
  `ctx.memory(|m| m.area_rect(id))` — the SDF shadow's expanded clip rect reads a centred card
  as 62 px off.
- **Artwork is cropped subject-centred** (two bboxes: ink `alpha>=1`, subject `alpha>=200`),
  and **keeps its alpha** — an opaque matte is what made the grey box against the `[10,10,12]`
  letterbox. `pb-scene-pipeline` is `ALPHA_BLENDING`, so transparent frames blend over it.
- **The retained overlay needs two seams:** `overlay_panel_visible()` (`main.rs:1458`) gates
  drawing, `overlay_dirty` (`:525`, set `:1534`) gates rebuilds — nothing honours egui's own
  repaint request, so a new persistent element must join both.
- **The size readout goes through the scan worker** (`FsSource::new` stats archives only),
  never the per-frame `info_line_parts` path (an SMB stat there blocks).

---

# 🎯 ACTIVE (macOS agent) — Task #106: fast archive opens, instant resize, instant thumbnails

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

# The rules that were bought with real time

- **Look at the output.** Every door defect the owner reported was *visible*, and none of
  them failed a test. `--egui-shot` / `PB_SHOT_DOOR=1|long` (winit) and `--pb-door-shot <dir>`
  (macOS `ImageRenderer`) exist for exactly this.
- **When a plan and the code disagree, the code wins — fix the plan.** My own "nine guards"
  claim was wrong (3 were already exhaustive, 2 correct to leave, 4 needed changing);
  following it mechanically would have made things worse.
- **Never gate a feature on a backend.** Use `AppCore::video_showing()` / `video_position()`.

---

# Carried-forward notes (cross-cutting — keep)

- **Commit + push directly to main** (owner-authorized); **fetch/merge origin/main first** — a
  parallel **macOS agent** also pushes there (and re-uses task IDs, so watch for collisions —
  Copy was renumbered #106→#107 this merge). Stage explicit paths (avoid `add -A`; the owner
  edits the repo concurrently).
- ⚠ **The owner drives the app while you work** (their instance may lock the build). On
  Windows check `Get-Process blazeviewer` before killing; a separate `CARGO_TARGET_DIR` under
  the scratchpad builds without fighting them. A mid-test anomaly is often the owner, not a
  bug — A/B before believing it.
- **`pb-app` (winit/egui shell) does NOT build on macOS** by design — target-os guard, so the
  egui half of shared changes is inspection-only there (hence this session). Windows
  cross-check from the Mac: `cargo check -p pb-app --target x86_64-pc-windows-msvc` after two
  temp manifest edits (blake3 `pure` as a DIRECT dep + `ureq` `default-features=false` in both
  crates); restore + `git checkout Cargo.lock` after.
- ⚠ **`cargo test --workspace` can't build `pb-app` on macOS** → `--workspace --exclude pb-app`.
- **macOS build/run:** `./scripts/build-swift-host.sh` → `target/swift-host/release/Blaze
  Viewer.app`. `--pb-door-shot <dir>` shoots the door card offscreen. `PB_PERF` / `PB_TRACE`
  gate stderr diagnostics.
- ⚠ **The Bash tool mangles `git show rev:path`** (the colon → `;`, slashes → `\`) and eats a
  backslash in heredoc'd Python needles. Use **PowerShell** for `git show`, and the Edit tool
  for Rust string literals.

---

# Separate in-flight track — Subtitles / Audio (Windows, the other agent)

Not this session's focus. Summary so the knowledge isn't lost:
- **Shipped:** subtitle display + Settings tab + the **Playback menu + Subtitle-Track flyout**
  on Windows (`756cfff`).
- **NEXT there:** the **Audio flyout**, blocked on an **FFmpeg→MF stream-order bridge** — MF
  enumerates audio streams in a *different order* than FFmpeg/the container, so a naive wire
  would tick the right track and play the wrong one. Bridge design is settled (match on
  lang/codec/channels, set `TrackLocator::MfStream(ordinal)`); `MfAudioDecoder::open_track` is
  done + tested (`d364d2e`). Wiring detail (the `Cmd::SetTrack` dance, the `Shared` atomics,
  the ignored `SelectAudioTrack` effect at `main.rs:3165`) is in git history at rev 9.
- **Authoritative docs:** `.taskmaster/docs/90-presenter-and-style-contract.md` (subtitles),
  `98-phase0-spike-findings.md` (MF track-catalog limits — read before any track work),
  `video-playback-overhaul.md` (R1–R12).
- **Also unfiled:** ⚠ **AC-3/E-AC-3 doesn't decode on Windows** (`0xC00D36B4`) — many DD/DD+
  films play silent; worth its own task.
