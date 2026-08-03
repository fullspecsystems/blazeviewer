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

## Codex review triage (2026-08-03)

A `codex exec` review of `main...HEAD`. Its verdict was "not safe to merge yet" and on the
first finding it was right — that one was reproduced as real data loss before fixing.

**Fixed (commit `68641649`):**

| # | Finding | Verdict |
|---|---|---|
| 1 | An incoming sidecar overwrote one already at the destination when the *image* name was free but the *sidecar* name was taken | **Real, critical, reproduced.** Fixed by `unique_name_for_group` |
| 2 | Undo moved sidecars back unconditionally, clobbering one that reappeared | **Real.** Sidecars now get the image's no-clobber rule |

**Also fixed (commit `5169ae1c`)** — findings #6 and #10, which shared one root cause: companions
were *generated* as a fixed candidate set and probed, so they could not be matched
case-insensitively, could not include a Live Photo's `.MOV`, and could not express a qualified
`movie.en.forced.srt`. They are now **discovered** from the directory listing
(`companions_in`), with subtitles delegated to `sidecar::parse_sidecar`.

**Accepted, not yet fixed** — ranked by how likely they are to bite this corpus:

1. ~~**Live Photo + subtitle companions are dropped.**~~ **FIXED** — `IMG_1234.HEIC` + `IMG_1234.MOV` is a
   Live Photo; sorting the still orphans the motion and breaks it. `movie.mkv` +
   `movie.en.srt` likewise. The app *already* knows the first pairing —
   `engine::companion_motion`. The fixed-candidate model also can't express a qualified name
   like `movie.en.srt`. **The most likely of these to hurt a real library.**
2. ~~**Sidecar matching is not case-insensitive**~~ **FIXED** — matching is now case-insensitive
   *by rule* (not by host filesystem), so a corpus behaves the same on APFS and on a
   case-sensitive volume.
3. **In-flight sorts aren't scoped to their deck.** `SortJob` carries no deck generation, so a
   slow sort that lands after the user opens a different folder can push an undo entry — or
   reinsert an old path — into the *new* deck.
4. **Rapid presses can sort a photo that was never shown.** `flush_pending_delete` advances
   `displayed_item` logically while the previous frame is still on screen, so a second press
   inside the 160 ms advance window files the *next* photo. Sharpest finding in the review;
   it strikes exactly the hammering case the feature is for.
5. **The worker doesn't keep the event loop awake and isn't drained at shutdown.**
   `work_pending()` doesn't include quick sort, so a slow SMB result can sit in the channel
   until an unrelated event wakes the app; teardown neither joins nor cancels, so a queued
   sort can silently never happen.
6. **A copy that succeeded but whose source removal failed is reported as total failure** —
   leaving an untracked duplicate at the destination with no undo entry. Retrying then makes
   `a-1.jpg`, `a-2.jpg`. Also: the copy fallback runs after *every* rename error, not just
   `EXDEV`, and the plan's "copy → verify → unlink" never got its verify.
7. Undo runs its I/O **synchronously on the event loop** — undoing a cross-volume move can
   freeze for the length of the copy. The forward path was made async and the reverse wasn't.
8. Same-folder detection is **lexical**, so a symlinked destination isn't recognized as the
   photo's own folder and it self-renames instead of no-opping.
9. Destination-inside-source can be re-discovered by a *streaming* recursive scan.
10. RAW+JPEG sharing one `.xmp`: whichever is sorted first claims it.

**Judged not worth acting on:** symlink/xattr/timestamp preservation through the copy fallback
(real but platform-dependent and untested against these volumes); the first press spawning a
thread (~µs); the O(N) playlist rebuild per press (already known — see the Prime-Directive
section above). The review also flagged that failure paths `eprintln!` full file paths; that is
**pre-existing house practice** (`delete.rs` does the same) rather than something Quick Sort
introduced, but it is worth a project-wide decision.

## Handoff

**Verified** (Mac session, 2026-08-02)

- `pb-app-core`: 941 tests pass (38 new). The 2 remaining failures
  (`a_real_video_probes_off_thread_and_lands_its_catalog`,
  `copy_details_mid_probe_defers_and_copies_the_complete_set`) **reproduce on clean `main`**
  — pre-existing, task #134. Clippy clean under `-D warnings` for `pb-app-core` + `pb-mac-ffi`.
- **Windows/Linux winit shell type-checks**: `cargo clippy -p pb-app --all-targets --target
  x86_64-pc-windows-msvc -- -D warnings` — clean. So the new `AppCore.quick_sort_queue` field
  is *not* struct-literal debt; it is verified. (⚠ This contradicts the 2026-07-20 note that
  the Mac cross-check was blocked by blake3's C build — retested, it completes. Memory
  updated.)
- **Live on macOS, end to end**, against a real folder: chose a destination through the
  Settings pane (confirmed it wrote `settings.toml`), pressed `1` on `photo1.jpg` → the file
  **and its `photo1.txt` YOLO label** landed in the destination, the deck advanced and rebuilt
  (`photo2.jpg · 1 of 2`), `⌘Z` returned both files with content intact and the deck went back
  to `1 of 3`. Pressing an unconfigured slot toasted **"No folder set for Quick Sort 3"**.
- The Quick Sort Settings pane renders correctly: 16 rows, chord column showing `1`–`7` then
  `⇧1`–`⇧7`, Move/Copy picker, Choose…, and Clear All.

> ## ⛔ The egui Settings tab is ON HOLD — do not build it (owner, 2026-08-02)
>
> "I'll keep polishing the mac side — wait until I'm really happy with this one and I'll do
> the egui side on windows later."
>
> The macOS pane is still moving (two owner-driven revisions on day one: the row layout and
> the toast icon). Porting a design that is still changing means porting it twice and then
> reconciling two drifted versions — the exact failure the cross-machine section of the root
> `CLAUDE.md` warns about. **Wait for the owner to call the Mac pane settled.** Windows/Linux
> keyboard behaviour already works; only slot *configuration* needs `settings.toml` by hand
> in the meantime.

**Not verified**

- **The winit/egui Settings tab does not exist yet** — on Windows/Linux the feature works from
  the keyboard, but slots can only be configured by hand-editing `settings.toml`.
  **Deliberately deferred — see the hold notice above.**
- Menu entries (an Image ▸ Quick Sort submenu listing configured slots) are not wired on
  either shell.
- Nothing was tested on a **network share or across volumes**, so the `EXDEV` copy fallback
  has only its forced unit test behind it, not a real cross-volume move.
- Hammering the key at blaze rates was not measured; the coalesce-removals-per-beat
  optimization in the plan above is **not implemented** (one rebuild per press today).
- The no-trace test lives in `pb-app/src/main.rs`, which does not build on macOS — a Mac
  session cannot run it. It should still hold (quick sort is only reachable from a keypress),
  but a Windows session should confirm.

**Decisions / corrections**

- The three owner decisions above (2026-08-02): sidecars travel, per-slot Move/Copy, create a
  missing destination.
- Owner, mid-session: 16 slots (not 7), `Shift+1`–`Shift+7` for the second bank, and a
  Clear All for privacy.
- **Numpad secondaries were designed in and then dropped.** `PbKey`'s numpad *digits* are
  deliberately display-only (no `from_name` arm) **and** `mac/…/KeyMap.swift` does not map them,
  so the binding would have been dead on macOS. Wiring both is its own change.
- ⚠ **Automation note for whoever tests this next:** `osascript ... keystroke "1"` does **not**
  deliver digits to this app — `key code 18` does. Two apparent "the feature doesn't fire"
  failures were entirely this. A pre-existing binding (`9` = Fill) reproduced the same
  non-response, which is what proved it was the harness and not the feature.

**Cross-platform debt** — none outstanding from this commit (the winit cross-check passed).
The missing egui Settings tab is *unbuilt work*, listed under **Not verified** above, not debt
from a blind edit.

**Claimed** — Mac session, 2026-08-02: `pb-app-core` core + macOS Settings tab. **Released.**

The egui Settings tab is **not** available to claim — see the hold notice above. The owner is
polishing the macOS pane first and will do the egui side on Windows themselves afterwards. A
Windows session arriving here should pick up something else.
