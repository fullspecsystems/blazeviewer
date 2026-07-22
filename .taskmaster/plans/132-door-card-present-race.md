# Task 132 — Door card lands over the wrong item (intermittent present/ring race)

**Status:** FILED (2026-07-21), not started. Owner-reported during #131 macOS smoke-testing.
**Not** caused by #131 (that task never touches archive open, doors, the ring, or presentation).
This is a pre-existing, intermittent race in the **present/ring path** — the family the code already
names the *"cross-deck open race"* (`crates/pb-app-core/src/app_core_impl.rs:962`, Codex-diagnosed
2026-07-17). Needs a **reliable repro** before any hot-path fix — do not ship blind.

## Symptoms (two reports, same area)

1. **Card stuck over a photo.** Open an archive **door** that has no viewable images, then advance to
   the next photo — the "ZIP Archive / Open P" **door card stays on screen over the photo**. The window
   title reads the *photo* (e.g. `image (1).png · 31 of 56`) while the door card is up. (Original
   owner screenshot: `~/Downloads`, `ID1564_WAV_Preliminary…zip`.)
2. **Card missing on backward nav.** Navigating **backwards** from a photo *onto* a door, the door card
   sometimes **doesn't show** at all (the inverse timing failure).

Both are door-frame **presentation-timing** failures: `displayed_item` ends up on the wrong item
relative to what the user navigated to.

## What is NOT the bug (verified this session)

- **The core `door_card`/`door_presented` logic is self-consistent.** `door_card()`
  (`app_core_impl/item_kind.rs:91`) returns `Some` **iff** `door_presented()`
  (`item_kind.rs:67`) is true, which requires `presented_epoch == Some(epoch)` **and**
  `displayed_item` is an archive. Given a correct `displayed_item`, the card shows/hides correctly.
  So the defect is **`displayed_item` landing on the wrong item**, not the card gate.
- **The empty-archive core path does not navigate.** `finish_archive_open` on an empty archive returns
  `ArchiveOutcome::Failed(Empty)` (`app_core_impl/archive_open.rs:189`) and touches neither
  `displayed_item`, `target_item`, `epoch`, nor `archive_scope`. The mac host's
  `apply_archive_outcome(Failed)` (`pb-mac-ffi/src/lib.rs:2741`) only closes dialogs + `report_error`.
  ZIP is **synchronous** (`ArchiveKind::background_open()` is false — `pb-source/src/kind.rs:49`), so a
  plain zip resolves *before* any advance; a non-zip (7z / tar family) or a cached-password auto-try
  opens **async** and resolves *after*.
- **#131 is unrelated.** Delete/scan-cancel/toggle inversions don't touch this path.

## Evidence (the one capture — contaminated, but the mechanism is clear)

From a `PB_DOOR_DIAG` run on a 3-item folder (`1-photo.png`, empty `2-archive.zip`, `3-photo.png`),
correlating the built-in `render` line (`present_idx` = ring slot, `img` = texture dims: **1×1 = a door
sentinel**, **240×160 = a photo**) with `displayed`:

```
render present_idx=2 img=1x1     -> displayed=Some(1) "2-archive.zip"  (door shown — correct)
render present_idx=3 img=240x160 -> displayed=Some(2) "3-photo.png"    (advanced — correct)
render present_idx=2 img=1x1     -> displayed=Some(1) "2-archive.zip"  (SNAP BACK to the door)
```

The snap-back is a **late present of the door's sentinel slot** (`present_item(item=1, slot=2)` →
`mark_resolved(1)` → `displayed = 1`), with `epoch` unchanged (no rebuild) and **`target` also = 1**.
⚠ **Caveat:** that session overlapped scripted keystrokes with manual owner clicks, so the `target→1`
move may have been a stray backward-nav / thumbnail-strip click rather than the bug itself. Clean
automated runs (nav→P→advance) did **not** reproduce it. Treat this capture as *illustrative of the
failure shape*, not a proven trigger.

## Leading hypothesis

A present tied to the door lands (or `target`/`displayed` is moved onto the door) **after** the user has
navigated past it, dragging `displayed_item` back to the door. Candidate mechanisms to investigate:

- A **late-arriving decode/tile/derive** for the door (or a strip-thumbnail present for it) that reaches
  `present_item` without a strict `target_item == item && !stale` guard. Audit every `present_item` /
  `present_slot` caller that isn't the main nav gate: `app_core_impl.rs:1120`, `:1935`, `:2221`,
  `:2335`, `:4253`, `:4365` (most check `target_item`, but confirm each; the `#124` rule is *background
  work may change residency/quality, never the presented representation*).
- The **cross-deck race** proper (`apply_scan_batch`, `app_core_impl.rs:921`/`:962`) when a **large,
  still-streaming folder scan** is in flight as the archive interaction happens — the existing guard
  covers *archive-deck-installed then stale-scan-extends*; the owner's variants may be uncovered
  (empty-archive open, or backward-nav onto a door, while the folder is mid-scan).
- The mac **pump-gating** interaction: `work_pending()` (`app_core_impl.rs`, the `pool.has_work()` /
  `pending_uploads` / `archive_load` arms) does **not** include `target_pending()`. If it goes false
  while a door sentinel present is still owed, the mac `updatePacing()`
  (`mac/Sources/BlazeViewerMac/CoreModel.swift:1998`) pauses the CADisplayLink, potentially stranding
  `displayed` between the door and the photo until the next input. (Bug #2 — card missing on backward
  nav — fits this shape.)

## Reproduction recipe (for the next session)

Likely needs the folder **still scanning** when the door is reached (a big, slow folder — `~/Downloads`
etc.), possibly with the **thumbnail strip open**. Capture with the built-in diag:

```sh
PB_DOOR_DIAG=1 "…/Blaze Viewer.app/Contents/MacOS/Blaze Viewer" --pb-open <BIG_FOLDER> 2>/tmp/door.log
```

Keep the window **visible/frontmost** the whole time (an occluded window pauses the CADisplayLink and
stops `pump()`, so no diag is logged — this defeated automated repro this session). The diag emits
`scan_batch -> BOOTSTRAP|REJECT|EXTEND`, `render present_idx/img`, `draw … door_card=…`, and
`present_slot(…) missed`. The **smoking gun** is a `render idx=<door-slot> img=1x1` (or a `scan_batch ->
REJECT`) arriving *after* `displayed` has moved to a photo.

## Code map (anchors, verified 2026-07-21)

- `app_core_impl/item_kind.rs:67` `door_presented`, `:91` `door_card` — the (correct) gate.
- `app_core_impl.rs:3512` `present_item` (calls `mark_resolved` → sets `displayed_item`), `:3645`
  `mark_resolved`, `:3113` `present_slot_for`, `:3701` `try_present_target`, `:3927` `drain_results`.
- `app_core_impl.rs:921` `apply_scan_batch` + the `:962` cross-deck-race guard + comment.
- `pb-mac-ffi/src/lib.rs:1955` the Swift door reconcile (per-pump `core.door_card()` → `doorVisible`);
  `mac/.../CoreModel.swift:1817` `pump()`, `:1998` `updatePacing` (pump/idle gating).

## Fix direction (when repro[ducible])

Add a **target-identity guard on the present path** so a stale/late present can never move
`displayed_item`/`target_item` onto an item the user has already navigated past — plus a **deterministic
`pb-core`/`pb-app-core` unit test** reproducing the late-door-present (synthetic outcome landing for the
door after `target` has advanced to a photo; assert `door_card()` stays `None`). Only then touch the hot
path. Consider folding `target_pending()` into the mac `work_pending()` if bug #2 proves to be the
pump-pause-strands-the-door variant.
