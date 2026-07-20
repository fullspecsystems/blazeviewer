# Technical-Debt Audit — Blaze Viewer

**Date:** 2026-07-18
**Method:** Repo-wide metrics (LOC, git churn, test density, cfg/unwrap/unsafe counts,
function length) cross-checked with three focused code reads of the god-object, the
core↔renderer ring, and platform/duplication sprawl. Structural claims were re-verified
against the live tree at commit `8b5dc30b`; the ones below carry `file:line` anchors.
**Scope:** A standing reference for *maintainability* debt — the structures likely to
keep generating bugs and slowing changes. Not a bug list and not a performance review
(the hot path is deliberately out of scope; per the prime directive it is measured, not
guessed at).

> **Freshness caveat (important).** This repo moves ~1000 commits/6 months and the
> working tree is edited concurrently, so **the exact numbers below are a snapshot and
> drift within days** (the two headline file sizes changed mid-audit). Trust the
> *structural conclusions and rankings* — those are durable — and **regenerate the
> metrics** (see appendix) before quoting a figure. One time-sensitive item: **finding
> #3's status is tied to task #109 and to the "archive-card-over-photo-bug" memory**;
> confirm both before acting on it. During the audit, one sub-agent also described a
> *reverted* repair (`c383107a`) as if it were live — a reminder to re-anchor any
> ring/deck claim against the current `present_item`/`apply_scan_batch` source.

---

## The through-line (read this first)

Blaze Viewer is **well-engineered with debt concentrated in a few structural places**,
not pervasive rot. The pure crates are genuinely clean: `pb-core` is 100% cfg-free and
property-tested, logic lives in ~43 well-factored sibling modules, and the logic crates
carry 12–30 tests/kLOC. Most of what follows is a *single recurring pattern*:

> **Two sources of truth, kept in sync by hand-written discipline, with no reconciliation
> and no compiler enforcement.**

It appears three times, and each instance is a top finding:
1. the shell↔core **mirror flags** (`AppCore` ⇄ `App`),
2. the two **platform shells** (`main.rs` ⇄ `pb-mac-ffi/lib.rs`),
3. the core↔renderer **ring / deck** (identity-blind renderer ring).

The reason the pattern is dangerous is proven by the "door card over a photo" bug
(root-caused 2026-07-17): a folder-scan batch swapped `self.source` under the rings
without an epoch bump, and nothing could detect that slot N now named different pixels —
so `present_slot` returned *true with the wrong occupant*. Fixed acutely in `8293a662`;
the *structural* fix (identity-stamped slots, one open generation) is deferred to task
**#109**. The whole audit is, in effect, a map of where that pattern still lives.

The debt is also **spatially concentrated**: the three largest files are the three
most-churned files. Big + hot = where the pain is.

---

## Baseline metrics

Snapshot at `8b5dc30b` (2026-07-18); `app_core_impl.rs` and `main.rs` already grew
~250 / ~105 lines during the audit — treat as approximate.

| File | LOC | Changes (12 mo) |
|---|---:|---:|
| `pb-app-core/src/app_core_impl.rs` | 16,013 | 205 |
| `pb-app/src/main.rs` | 6,382 | 270 |
| `pb-mac-ffi/src/lib.rs` | 6,274 | 105 |
| `pb-render/src/gpu.rs` | 4,722 | 40 |
| `pb-app/src/panels_ui.rs` | 4,187 | 37 |
| `pb-app/src/dialog.rs` | 3,260 | 61 |

**Test density (tests/kLOC):** pb-core 30.6 · pb-app-core 17.1 · pb-decode 13.9 ·
pb-source 12.0 · pb-hud 11.3 · pb-render 7.3 · pb-mac-ffi 6.0 · pb-ui 4.7 · **pb-app 3.9**.
The two big *shells* are the least-tested large code and among the most-churned.

**Longest functions:** `AppCore::tick` 487 · `drain_results` 225 · `poll_video` 212 ·
`dispatch_action` 199 (app_core_impl.rs); `window_event` 366 · `App::new` 341 ·
`about_to_wait` 253 · `drain_effects` 232 · `resumed` 224 (main.rs); `gpu::render` 309.

**Other:** 792 `unwrap()` total (122 in the hostile-byte archive parsers, `pb-source`);
293 `unsafe` (pb-decode, expected FFI); 255 `target_os="macos"` + 155 `cfg(windows)`
across 40 files; `AppCore` ≈165 `pub` fields; `App` 66 fields; contract = 46 `CoreEffect`
+ 19 `CoreEvent` variants.

---

## Ranked findings

### 1. The god-objects + the half-finished NS0 inversion (the hub)

`crates/pb-app-core/src/app_core_impl.rs` — one flat `impl AppCore` (lines 76–10009) with
**323 production methods and zero internal module structure** (the only `mod` is
`mod tests` at 10011). `AppCore` (`app_core.rs:193-792`) has **~165 `pub` fields**, every
one public by admission ("only because the shells build it as a struct literal",
`app_core.rs:629`). The shell's `App` (`main.rs:338`) is a **second god-struct**: 66
fields, ~114 methods. `tick()` is **487 lines** calling ~28 subsystem methods across every
concern; `dispatch_action()` is a 199-line ~60-arm match.

The sharpest hazard is the **hand-synced mirror flags** — `scanning`, `launching`,
`dialog_open`, `archive_loading`, `redraw_pending` (`app_core.rs:453-475`), each documented
as "the core-owned mirror of the shell's `X.is_some()`, kept in sync at every mutation."
The same fact lives in two structs across two crates, correct only while every mutation
site remembers to re-sync. **This is the desync bug class, structurally.**

- **Why it hurts:** no compiler-enforced boundaries — any of 323 methods can touch any of
  164 fields, so an invariant change has a 10k-line blast radius; `tick`'s cross-cutting
  body makes per-frame ordering a global concern; it is a merge-conflict magnet for
  concurrent feature work (video / archives / panels all funnel through the same impl).
- **Mitigating truth:** the *logic* is delegated to ~43 well-factored sibling modules
  (`video.rs`, `folder_tree.rs`, `thumbs.rs`, …); `app_core_impl.rs` is mostly the
  orchestration/glue layer. The debt is that the glue was bulk-moved onto one struct and
  never re-partitioned.
- **Remediation:** (a) split the one impl into concern-scoped `impl AppCore` blocks across
  files (video / tree / overlay / prefetch) — Rust allows this for free; mechanical,
  low-risk, immediately shrinks the blast radius and conflict surface. (b) Finish the NS0
  inversion so `AppCore` owns orchestration and the mirror flags have a single owner — this
  is what actually dissolves the bug class (and finding #2). (c) Consider a `#[non_exhaustive]`
  / accessor discipline to stop the all-`pub`-fields sprawl once the shells stop building it
  as a literal.

### 2. The two parallel platform shells — `main.rs` ⇄ `pb-mac-ffi/lib.rs`

`pb-mac-ffi/lib.rs` (6,274 lines) is **not primarily an FFI boundary**. ~2,870 lines
(`impl AppCoreHandle`, 217 methods, lines 175–3042) are a hand-maintained mirror of the
winit shell: **37 "mirror" comments** and **16+ byte-identical orchestration function
names** in both files (verified: `begin_archive_open`, `begin_dir_scan`, `poll_dir_scan`,
`apply_menu_state`, `confirm_delete_permanent`, `prompt_archive_password`,
`toggle_recursive`, `toggle_show_archives`, `cancel_dir_scan`, `finish_archive_open`,
`poll_archive_load`, `fail_archive_open`, …). Both independently define
`struct DirScan` and `struct ArchiveLoad`. The genuine FFI surface (`#[swift_bridge::bridge]
mod ffi`) starts only at line 4376.

- **Why it hurts:** the **single worst cross-platform liability** — every orchestration
  feature is written twice and drifts silently. This is what will make the macOS port a
  grind and what leaves macOS exposed to race variants Windows already closed (see #3, task
  #109 item 1: the mac shell still lacks the cross-cancel that Windows got in `8293a662`).
- **Remediation:** this is the payoff of finishing #1. A shared `AppCore` that owns
  orchestration collapses both shells to thin, platform-specific I/O adapters. Until then,
  every change to a mirrored function must be applied to both files by hand — treat that as a
  known tax and grep the mirror-name list before landing shell orchestration changes.

### 3. The core↔renderer ring / deck fragility (acute bug fixed; structure still soft)

**Status correction:** the "door card frozen over a photo / titles advance but view frozen"
bug is **root-caused and fixed** (`8293a662`, Codex review 2026-07-17). It was *not* a
capacity desync — it was a **cross-type open race**: a still-alive folder-scan worker's late
batch called `extend_playlist`, which swaps `self.source` **without touching the ring, epoch,
or `content_gen`**, so index N named a folder item while both rings still held the *archive*
texture for N. `present_slot` then returned **true with the wrong occupant**
(`app_core_impl.rs:1181-1196` for the fix; `6381-6403` for the diagnostic). The fix is a
core extend-guard (reject a batch when `archive_scope.is_some()` or `scan_root` mismatches)
plus a winit shell cross-cancel of the two worker types.

**Do NOT reintroduce** the invalidate-on-miss self-heal (`cff70ca0` / `c383107a`): it bumped
the epoch mid-`drain_results` and purged the retained full-res tier, regressing instant
fullscreen to a preview flash. It was **deliberately reverted**; `present_slot`'s `false`
return is now a loud diagnostic only, never a control-flow branch (`app_core_impl.rs:6381-6390`).

**But the structural debt the bug exposed is real and only partly addressed:**
- The **renderer ring is identity-blind** — `RingSlot` holds only `{bind_group, w, h, peak}`
  (`gpu.rs:1754`), no item id / `content_gen`, so it *cannot* verify slot N holds the item the
  core believes. There is no reconciliation; drift surfaces only downstream.
- ✅ **The fill is transactional now (#109.4, landed 2026-07-19):** `upload_slot` returns
  `bool` (a loud stderr refusal on out-of-bounds instead of a silent no-op), `mark_resident`
  runs only after a successful upload and its return is checked; a refused upload rolls its
  reservation back (reserve path) or leaves the preview bookkeeping untouched (upgrade path).
  Both drain paths are pinned by regression tests (`a_refused_upload_*`).
- ✅ **Epoch carries geometry AND deck identity now (#109.3 via task #119, landed
  2026-07-19):** `DecodeKey` carries a real `content_gen`; staleness is a declared
  per-work-kind `Validity` domain (`decode_pool::validity`, exhaustive match) enforced at the
  pool cancel arms, ingestion, ring-rebuild retention, and the drain gate. Cross-deck decodes
  can no longer dedup as current, and viewport-independent work (Originals/thumbs/poster
  walks) survives geometry changes — the #119 fullscreen-toggle blur storm. See
  `.taskmaster/plans/119-decode-validity-domains.md`.
- A **known-open hole** remains: `apply_scan_batch`'s `BOOTSTRAP` branch while
  `archive_scope=true` ("mode B", `app_core_impl.rs:1146-1148`) — a stale scan's *first* batch
  can still bootstrap over an archive deck (low severity: a clean rebuild onto a valid-but-wrong
  folder deck, not the frozen-view corruption). The most recent commit (`8b5dc30b`) is another
  heal-style patch (frozen display on swapchain drift), so this area is still active.
- **Remediation — already scoped as task #109** (medium priority), and it matches this audit's
  recommendation to *fail loud instead of self-heal*:
  - (#109.2) one **monotonic open generation** shared by both worker types, threaded through the
    contract, so any result whose generation ≠ latest is dropped in the core — the general,
    shell-neutral, race-proof fix that also closes mode B.
  - ✅ (#109.3) **landed 2026-07-19 via #119** — see the epoch/deck-identity bullet above.
  - ✅ (#109.4) **landed 2026-07-19** — see the fill bullet above.
  - (#109.5) `present_item` returns success; `try_present_target`/`drain_results` propagate it; on a
    genuine miss, abort the drain and resync **once after the loop** (never mid-loop — that was the
    reverted repair).
  - Add to #109 if pursued: an **identity stamp on `RingSlot`** (`item`/`content_gen`) so
    `present_slot`/`upload_slot` can `debug_assert` rather than trust the index, plus a
    `debug_assert_eq!` on the two ring capacities after `invalidate_geometry`. These convert
    "silently drift, self-heal a tick later" into "fail at the divergence," which is the only way
    a future cross-type-race variant gets caught early.

### 4. Platform *routing* is scattered inline (the seams are only half-true)

CLAUDE.md claims platform code is "quarantined behind a single helper seam." **Half right.**
Low-level *calls* are genuinely isolated — `primary_hdr()` (`pb-render/src/display.rs:55/92/150`),
the `mf_*` cluster, the `pb-mac-ffi` crate. But platform *routing* is woven through the
orchestration layer as 3–4-way compound cfg predicates: `app_core_impl.rs` alone carries **41
`target_os="macos"` blocks**, and `engine.rs:458-555` forks the video-poster path **4+ ways
inline in one function** (`#[cfg(all(target_os="macos", feature="ffvideo"))]`,
`#[cfg(not(any(windows, target_os="macos", …)))]`). Heaviest concentration: `app_core_impl.rs`,
`main.rs` (35 macos + 16 windows), `menu.rs`, `engine.rs`, `pb-decode/lib.rs`.

- **Why it hurts:** adding a platform or a codec route means editing compound predicates across
  ~40 files with no single dispatch point; easy to miss a fork.
- **Remediation:** lift routing to a runtime backend-selection seam (a `VideoRoute` /
  `PosterSource` enum resolved once at startup) instead of cfg-forking each call site. Keep the
  low-level call seams as-is — they are the part that works.

### 5. Parallel media stacks with no unifying trait

Two video producers — `run_video_producer` (MF, `mf_video_producer.rs:70`) and
`run_ff_video_producer` (FFmpeg, `ffmpeg/video_producer.rs:76`, which literally comments itself
"the FFmpeg mirror of `run_video_producer`") — unified only by a shared wire protocol
(`pb-decode/src/video.rs`) and a cfg dispatcher. The **~180-line credit/seek loop is duplicated
near-verbatim** (`mf_video_producer.rs:188-397` vs `ffmpeg/video_producer.rs:166-344`); only the
~5 reader calls inside differ. Plus **3 poster extractors** (`av_poster`, `mf_poster.rs:244`,
`ffmpeg/poster.rs:122`) and 2 audio decoders (`MfAudioDecoder`, `FfAudioDecoder`), none behind a
`VideoProducer` / `Poster` / `AudioDecoder` trait. Separately, **YUV→RGB is triplicated** —
`pb-decode/src/yuv.rs`, `pb-render/src/yuv.rs`, and the WGSL `fs_scene_planar` in `gpu.rs` each
hand-write the BT.601/709/2020 Kr/Kb tables; the module doc at `pb-render/src/yuv.rs:5` openly
admits it.

- **Why it hurts:** the seek loop and the YUV constants are correctness-critical copies that must
  agree; there is no compiler link between them.
- **Remediation:** a `VideoProducer` trait over the existing protocol collapses the producer
  duplication; a tiny shared color-primitive (constants + a matrix builder) de-triplicates YUV.

### 6. The shell is big, hot, and thinly tested; parsers lean on `unwrap`

`pb-app` is 22.9k LOC at **~3.9 tests/kLOC** while containing the #1 churn file — the
least-tested large code is also the most-changed. And **122 production `unwrap()`s sit in the
hostile-byte archive parsers** (`pb-source`); the `catch_unwind` net downgrades them to
`DecodeError`, but a stack overflow is not catchable and unwrap-on-attacker-input is brittle
fuzzing surface.

- **Mitigating truth:** some shell untestability is inherent (GPU/present is deliberately
  `#[coverage(off)]`). But the *orchestration* flows still living in the shell (see #1/#2)
  become testable the moment they move into `AppCore`.
- **Remediation:** convert parser `unwrap`s to `?`/`OpenError` incrementally as the `fuzz/`
  targets flag them; gain shell test coverage as a side effect of the NS0 inversion.

---

## Leave these alone (checked — well-factored, not debt)

Do **not** "refactor" these; they were verified clean:
- **RAR5/RAR4** share one `ItemSource`/`RarSource` (`rar4.rs` imports `rar.rs`'s types; single
  `impl ItemSource`); the "completely different containers" framing oversells the split.
- **Subtitles** are textbook layering with one timing source of truth (`cues.rs::active_at`);
  `pb-hud` is a rasterizer only.
- **`VideoSession`** (pacing state machine) is a single backend-blind copy both producers feed.
- **The two `tracks.rs`/`video.rs`** are clean re-export layering, not redefined structs.
- **`ResidentRing`** the data structure is excellent (typed reps, staleness rejection, two
  20k-iteration property tests). The ring *bridge* is the debt, not the ring.
- **`pb-core`** purity (0 cfg, 0 unsafe) is intact — keep it that way.

Also noted: one **stale doc** — `CLAUDE.md` still describes video audio as "a shell WinRT
MediaPlayer"; it has been WASAPI since `ae6f412`. Worth a one-line correction.

---

## Recommended sequencing (highest ROI first)

1. **Split `app_core_impl.rs` into concern-scoped `impl AppCore` blocks.** Mechanical, low-risk,
   immediate relief to blast radius and merge conflicts. No behavior change.
2. **Land task #109 item 5** (`present_item` result propagation with a once-after-loop resync;
   item 4 — the fail-loud `upload_slot` bridge — landed 2026-07-19). Small, and completes the
   ring bridge's conversion from silent-drift to loud-failure — the durable close-out of the
   #3 bug class.
3. **Then the structural work:** finish the NS0 inversion so `AppCore` owns orchestration. This
   is the move that dissolves #1, #2, and the mirror-flag bug class together, and it unblocks
   collapsing the two shells (#2) and testing the shell (#6).
4. **Opportunistic:** `VideoProducer` trait + shared YUV primitive (#5); runtime routing seam
   (#4); parser `unwrap`→`?` as fuzz flags them (#6).

Items 1–2 are safe to do now and independently. Item 3 is the large, high-value program the
other findings mostly reduce to.

---

## Appendix — how to regenerate the metrics

```sh
# LOC per crate / largest files
find crates -name '*.rs' -not -path '*/target/*' | xargs wc -l | sort -rn | head -35
# Churn (bug-proneness proxy)
git log --since="12 months ago" --name-only --pretty=format: -- 'crates/**/*.rs' \
  | grep '\.rs$' | sort | uniq -c | sort -rn | head -25
# Test density: #[test]/#[tokio::test] per crate vs LOC
# unwrap / unsafe / cfg counts: grep -rn over crates --include=*.rs
```
