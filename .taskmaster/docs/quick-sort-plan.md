# Quick Sort — folder slots bound to `1`–`7` (task #136)

> Status: **plan**, branch `feat/quick-sort-folders`, opened 2026-08-02.
> Owner use case: binning hundreds of face crops into identity folders for ML
> training. "Bang out a long, tedious sort operation one key at a time."

## The feature in one paragraph

Seven **slots**, each holding a destination folder, a label, and a Move/Copy mode.
Each slot is bound to a digit key (`1`–`7` by default, remappable like every other
action). Pressing the key sorts the displayed photo into that slot's folder
*immediately* — no prompt, no dialog — flashes the slot name, and advances to the
next photo. `Ctrl+Z` walks it back, one photo at a time, off the existing undo stack.

## Precedent

| App | Model |
|---|---|
| **FastStone Image Viewer** | Configurable "Move To Folder" slots bound to keys — the exact feature |
| **IrfanView / XnView MP** | Move-to-folder with a configured destination list |
| **Photo Mechanic** | Destination hotkeys (the pro culling tool) |
| **Lightroom** | *Deliberately not this* — flags/labels then bulk-act on a filter |

Lightroom's is the **mark → commit** model, which this repo has already written down
for touch (`mobile-vision.md` § "blaze → mark → commit"). We are building move-now
because for a labelling pass the destination *is* the label — there is nothing to
review later. The two compose: same slots, different commit timing. Building this
does not foreclose adding marks on top.

## The Prime-Directive read

**A quick-sort key is a nav key in disguise.** Press `3` → the photo leaves the deck
→ the next photo must already be on screen, inside the one-refresh budget. The deck
half of that is solved: `pending_delete` + `DELETE_ADVANCE_DELAY` + rebuild-and-advance
(`app_core_impl/delete.rs`).

**What is *not* solved: the I/O.** `do_delete` calls `crate::delete::recycle(path)`
**synchronously on the event loop**. Fine for an occasional `Del`; fatal when the key is
being hammered — an `fs::rename` to an SMB share is tens of ms, and a cross-volume copy
of a 40 MB RAW is hundreds. That violates "never block the event loop."

So the split is:

1. **On the keypress (event loop, microseconds):** validate the slot, retire the item
   from the deck, flash the pill, schedule the advance. Nothing touches the filesystem.
2. **Off-thread (the decode pool):** the actual move/copy. CLAUDE.md already blesses
   non-image work kinds riding that pool under the same priority rules.
3. **On completion (effect → core):** push the undo entry. On *failure*, toast and
   `reinsert_after_restore(index, path)` — machinery undo-delete already implements.

**Rebuild coalescing.** Each removal today rebuilds the `FsSource` from all remaining
paths (O(n) clone). Delete's own comment calls that "fine for an explicit, infrequent
command" — quick sort makes it frequent. Coalesce removals into **one rebuild per
`DELETE_ADVANCE_DELAY` beat** rather than one per press. At hundreds of photos this is
invisible either way; at a 50k recursive deck and 10 presses/sec it is not.

## Slots and keys

**Sixteen slots; fourteen bound by default.** The count and the binding are deliberately
different numbers — how many destinations a sort needs is a property of the corpus, how many
chords are free is a property of the keymap. Tying them together would cap the feature at
whatever the keyboard could spare.

| Slots | Default chord | Why |
|---|---|---|
| 1–7 | `1`–`7` (+ `Numpad1`–`7` secondary) | Verified free — `8`/`9`/`0` are Fit / Fill / Toggle-1:1 (`keymap.rs:586-589`) |
| 8–14 | `Shift+1`–`Shift+7` | Shifted digits are entirely unbound |
| 15–16 | *none* | Configured the same way; the user binds a chord in Settings ▸ Shortcuts |

Chords match the **physical** key plus modifier flags (`KeyChord`), so `Shift+1` is a real
chord on every layout — it never has to become `!`.
`defaults_have_no_duplicate_keys` (`keymap.rs:866`) polices collisions for free.

## Unassigned slot → an honest error

The digit keys were inert before this feature, so pressing one with nothing configured must
say what happened *and* leave a breadcrumb. House toast style is short and sentence-case with
no trailing period ("Can't delete this", "Nothing to undo", "Couldn't restore"):

> **`No folder set for Quick Sort 3`**

It names the feature — which is also the Settings tab's name, so it doubles as the
where-to-fix-it hint — without spending a second sentence on it.

## Owner decisions (2026-08-02)

| Question | Decision |
|---|---|
| Sidecars | **Move known sidecars with the image.** Orphaning a `.txt` label file would silently corrupt a training set. |
| Copy mode | **Per-slot Move/Copy toggle.** ⚠ A copy leaves the item in the deck → **no advance**. |
| Missing destination | **Create it, toast on failure.** The folder was deliberately configured. |

### Sidecar rules

Follows the house pattern already set by `sidecar.rs`: **the rules operate on a list of
sibling names, never on the filesystem**, so they are pure and unit-testable with no temp
dirs. Two naming conventions are both real and both must match:

- **Stem match** — `IMG_1234.xmp` beside `IMG_1234.cr2` (Adobe's RAW convention, YOLO's
  `IMG_1234.txt`)
- **Full-name match** — `IMG_1234.jpg.xmp` (what several other tools write)

Extension set: `xmp`, `txt`, `json`, `yaml`, `yml`, `aae`, `thm`, `pp3`, `dop`, `on1`, `arp`.
Case-insensitive by rule (not by host filesystem), same reasoning as `sidecar.rs`.

A sidecar move is **best-effort and never blocks the image**: the image is the operation;
a sidecar that fails to move is reported but does not fail the sort. Undo restores every
file that actually moved.

## Where the code goes

Two halves (`docs/where-code-goes.md`):

- **`crates/pb-app-core/src/quick_sort.rs`** — the `QuickSortSlot` model; the pure logic
  (`unique_name_in`, `sidecars_for`, destination resolution, no-op detection); the move I/O
  (`fs::rename` with a cross-volume copy → verify → unlink fallback for `EXDEV`).
- **`crates/pb-app-core/src/app_core_impl/quick_sort.rs`** — `impl AppCore`: validate the
  slot, stop a playing video, retire + advance, dispatch the job, apply the result.

Touched:

| File | Change |
|---|---|
| `action.rs` | `Action::QuickSort(u8)` — stays `Copy + Hash + Eq`; const id/label tables keep `ALL` `&'static` |
| `keymap.rs` | defaults `1`–`7` + `Numpad1`–`7` |
| `settings.rs` | `quick_sort: Vec<QuickSortSlot>` |
| `undo.rs` | `UndoAction::Sorted { from, to, moved_sidecars, index, name, slot_label }` — path-keyed like every other variant |
| `contract.rs` | the dispatch + completion effects |
| `mac/…/SettingsView.swift` + `pb-mac-ffi` | the Quick Sort tab, per-slot FFI |
| `pb-app/src/dialog.rs` | `SettingsTab::QuickSort` built from `pb-ui` components |

## Details that bite

- **Collisions.** Destination already holds `IMG_1234.jpg` → auto-suffix `IMG_1234-1.jpg`.
  Refusing stalls the flow, which defeats the feature. Must handle double extensions
  (`.tar.gz`), no-extension names, and non-ASCII stems.
- **Archive entries / doors.** `source.path(item)` is `None` → toast "Can't sort this",
  the same guard `request_delete_confirm` uses.
- **Videos.** `stop_video()` first — the reader holds the file open. Copy delete's
  retry-while-the-reader-retires loop.
- **Undo timing.** Push the entry when the move *completes*, mirroring `finish_delete`.
  An instant `Ctrl+Z` mid-flight says "Nothing to undo" — honest, and avoids racing a rename.
- **Feedback.** Delete flashes an icon-only pill; quick sort flashes the **slot name**
  ("→ Portraits"). With seven slots the user must see which one they hit.
- **Destination inside a recursive deck.** The item is removed from the deck regardless —
  it has been sorted, you are done with it. A rescan brings it back at its new path.
- **Same-folder no-op.** Slot points at the file's own folder → toast "Already in Portraits",
  no move, no advance.

## Privacy

Both halves are in-bounds and this should be stated in the module doc so a future audit
does not re-litigate it:

- **Slot paths are user-chosen preferences**, set deliberately in Settings — the same
  category as `picker_dir` (explicitly in-bounds, ADR-018). Not a viewing trace.
- **The moves are explicit user edits** — the same allowed category as delete and
  save-rotation. Never a byproduct of viewing.
- **Hard no:** no MRU of destinations, no log of what was sorted where, no count
  persisted per slot.

The no-trace test (`pb-app/src/main.rs:5208`) exercises only *viewing*, so it still holds —
a scan/decode never reaches a quick-sort command.

## Tests (TDD — write first)

Pure, in `quick_sort.rs`:

- `unique_name_in` — collision suffixing; `.tar.gz`; no extension; non-ASCII stems
- `sidecars_for` — stem match, full-name match, case-insensitivity, near-miss rejection
  (`IMG_12345.txt` is not a sidecar of `IMG_1234.jpg`)
- slot resolution — unconfigured → `None`; own folder → `NoOp`

I/O, temp dirs:

- move round-trip; the cross-volume copy fallback behind a seam so it is forceable
- undo restores image + sidecars

`AppCore`:

- an archive entry toasts and does nothing
- the deck advances and the item leaves (Move); the deck does **not** advance (Copy)
- a failed move reinserts the item at its old index
- `Action::QuickSort` id/label round-trip + `ALL` uniqueness (existing tests cover it once
  the variants are added)

## Handoff

**Verified** — (nothing yet; plan only)

**Not verified** — everything.

**Decisions / corrections** — the three owner decisions above (2026-08-02).

**Cross-platform debt**
- Adding `AppCore` state breaks `pb-app`'s **struct literal** and not the Mac's
  `new_host` — the Windows cross-check is mandatory before pushing (see
  `windows-cross-check-from-macos` memory for the exact temporary Cargo edits).
- The no-trace test lives in `pb-app/src/main.rs`, which does not build on macOS. A Mac
  session **cannot** run it.
- The winit/egui `SettingsTab::QuickSort` is authored blind from the Mac —
  behaviour-unverified until a Windows session launches it.

**Claimed** — Mac session, 2026-08-02: `pb-app-core` core + macOS Settings tab.
