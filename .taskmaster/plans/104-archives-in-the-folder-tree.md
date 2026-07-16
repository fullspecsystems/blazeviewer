# Task 104 — Archives as doors: visible in the deck, entered on purpose

**Status:** planned — rev4 (2026-07-16, owner-approved direction)
**Proposed task id:** 104 (add the task entry when this plan is approved)
**Depends on:** `pb_source::archive_kind` (`crates/pb-source/src/kind.rs:61`) — shipped on `main`
(#102 tar family, #103 RAR5, merged `631e970`).
**Scope:** `pb-app-core` + `pb-decode` typing/poster; one keymap arm. Shell work is minimal —
the door is a deck item, so it renders through the existing photo path. **But read *The audit*
first:** adding a third `LibraryItemKind` to a two-kind world is the real work here, and it is
not optional — two paths would otherwise read every archive with no keypress.

> **rev4 (owner, 2026-07-16).** rev1–3 all assumed archives had to be reached through the
> **folder tree**. The owner proposed the better shape: *"show the archive itself in the viewer
> … as an icon or a button that a user could click to open and enter."* That is the **video
> item pattern**, which this codebase already shipped and locked. rev4 rebuilds the plan around
> it. rev3's tree-row analysis survives in *Appendix: the tree surface* — it is now optional and
> deferred, not the spine.

## The shape, in one paragraph

An archive on disk is a **typed deck item** — `LibraryItemKind::Archive(ArchiveKind)` — that
decodes to a small placeholder tile ("a door"). It appears while you browse, costs a solid-color
tile, and **never reads the archive**. Pressing `P` on a door **enters** it: the existing
explicit-open path runs, with its RAM pre-flight, progress dialog and password prompt, and the
deck becomes the archive's contents. Nothing is ever decompressed without that keypress.

## Why this shape: it is already the house pattern

`video.rs:40` states the locked decision:

> *"the **one cross-platform recognition list** of video containers (locked decision:
> unsupported containers are **visible** items with a **placeholder poster** and a useful error
> on play — per-file capability is a runtime property, the poster attempt is the probe)"*

The machinery is not hypothetical — it is mechanical, and every piece an archive door needs is
built and load-bearing today:

| Piece | Exists | Archive door needs |
|---|---|---|
| `LibraryItemKind` (`video.rs:31`) | `Image` \| `Video(VideoContainer)` | **+ `Archive(ArchiveKind)`** |
| `item_kind` (`video.rs:145`) | types by extension | + one arm |
| **Typed dispatch before `bytes()`** (`engine.rs:393`) | video returns before the read at `:491` | + one arm above the read |
| `video_placeholder` (`engine.rs:507`) | 320×180 solid tile, `codec` names the container | **+ `archive_placeholder(kind)`** |
| `P` = play/pause (`action.rs`) | contextual on the item's container-ness | + "enter" on a door |
| `▶ P` HUD hint (`engine.rs:495`) | already tells you `P` does something here | reused |
| `CoreEffect::BeginArchiveOpen` | drives pre-flight + progress + password re-open | reused verbatim |

> Note (owner, 2026-07-16): the *unsupported-container* case is now rare — the OS decodes MKV
> fine. So the placeholder tile is a **fallback** in the video feature, not its everyday face.
> For archives it is the **primary** face. That is the one real difference, and it is why the
> door earns an icon where the video tile did not (see *The tile*).

## What this dissolves

rev3 spent its entire Phase 0 on a blocker: a folder of only archives produces **no scan items**,
so it never becomes the browse root (`app_core_impl.rs:1114` skips empty snapshots; `finish_scan`
`:1140` keeps the existing deck by owner rule ③), leaving the tree pointed elsewhere.

**Doors delete the blocker rather than fix it.** That folder now has forty items. The scan
produces a batch, the folder becomes the deck root, `current_folder_abs()` resolves,
`tree_is_fs()` goes true, and the tree follows on its own. The owner's original use case — *"a
folder of encrypted 7z archives"* — goes from unreachable to the easy case, and needs **no**
location surgery.

## Why doors are safe where blending was not

Blending archive *contents* into the deck stays rejected, and the door makes the reason exact:

**The prefetch ring is the whole argument.** A blended deck makes archive entries ordinary items,
so the direction-biased window decodes ahead into them — decompressing archives the user never
chose, as a side effect of scrolling nearby. A **door** has no such property, because *the door's
decode is drawing a tile, not reading the archive* (`engine.rs:393` returns before `:491`'s
`source.bytes()`). Prefetch can hold a hundred doors for the price of a hundred solid tiles —
which `video_placeholder`'s own doc already promises: *"prefetch can hold many without denting
the ring budget."*

That is the line the owner drew — *"not automatic where you suddenly OOM the user"* — enforced by
where the `return` sits, not by a rule.

The other standing objections against blending:

1. **Format-dependent cost is an unexplainable user model.** *"Why did it descend into
   `vacation.zip` but not `vacation.7z`?"* — and with RAR, per-*file*, since solidness is a
   property of the archive, not the suffix.
2. **No file manager blends.** Explorer makes a `.zip` navigable; it never merges its contents
   into the parent listing.
3. **A folder of `.cbz` files is a shelf of books.** Blending fuses forty comics into one
   4,000-page deck with no chapter boundaries.
4. **It is one-way.** If you wanted them separate, you cannot un-blend.
5. **It needs an architecture change.** `AppCore.source` is one `Arc<dyn ItemSource>`
   (`app_core.rs:361`) — one deck, one source.

> ⚠ rev2 argued this from a **false premise** (*"listing three 2 GB `.7z` files would decompress
> 6 GB to draw a listing"*). `ArchiveKind::eager()` (`kind.rs:38`) documents what our current
> **open** does; it is not evidence that enumeration must retain contents. Deleted — the
> prefetch argument is the true one and does not need it.

## Design

### 1. Typing — path-only, no new predicate

```rust
pub enum LibraryItemKind {
    Image,
    Video(VideoContainer),
    /// A `.zip`/`.7z`/`.cbz`/… **on disk**. Displays as a door; `P` enters it. Never
    /// read by the decode path — entering is a separate, explicit action.
    Archive(ArchiveKind),
}
```

In `item_kind` (`video.rs:145`), the arm is:

```rust
source.path(item).and_then(pb_source::archive_kind).map(LibraryItemKind::Archive)
```

Two properties fall out **for free**, which is why it is written this way:

- **Path-only.** `source.path(item)` is `None` for an archive entry, so a `.zip` inside a `.zip`
  is never a door — nested archives are excluded by construction, not by a rule. This mirrors the
  video precedent exactly (*"path-only, never indexed inside archives"*).
- **Double extensions work.** `archive_kind` takes a `&Path` and already handles `.tar.gz` /
  `.cbr` / `.cbz`. Name-based typing (`rsplit_once('.')` → `"gz"`) would get them wrong. Since
  doors are path-only we *have* the path, so we use the one shipped classifier and add no fourth
  `is_archive`.

### 2. The tile

`archive_placeholder(kind)` mirrors `video_placeholder` (`engine.rs:507`): tiny, `codec` naming
the format, so the GPU upscale is invisible and prefetch stays cheap.

**Recommendation: give the door an icon, unlike the video tile.** For video the tile was a
phase-1 stand-in that real posters replaced; for an archive the tile *is* the affordance — a
featureless dark rectangle is a poor door, and the owner's framing was explicitly *"an icon or a
button."* Rasterize the Font Awesome glyph via the `pb-decode` resvg stack and **cache one raster
per `ArchiveKind`** (there are nine) — never per item, never per frame.

If that proves fiddly, ship the solid tile first (exact video parity) and add the icon second;
the mechanism does not depend on it.

### 3. `P` enters (owner-approved)

`Enter` is random-photo and `O` is the Open dialog — both taken. `P` is already **contextual on
the item's container-ness** (play a video, play an animation, play a Live Photo), so "act on this
container" extends cleanly to "enter this archive." Owner (2026-07-16): *"P is… weird but kind of
fits… play this archive. I'm totally cool to use that."* The `▶ P` HUD hint (`engine.rs:495`)
already exists and carries it.

Entering pushes the existing effect:

```rust
CoreEffect::BeginArchiveOpen { path, password: None }
```

`begin_archive_open` is **shell-side** (`main.rs:1038`, `mac-ffi:2730`), so core pushes the
effect and never calls it. Both shells already handle it.

### 4. Passwords: already work — but ZIP/7z only

`OpenError::PasswordRequired` (`pb-source/src/lib.rs:262`) → prompt → re-open with `Some(pw)` is
real on both shells (`app_core_impl.rs:1024`, `main.rs:1148`, `mac-ffi:2819`). `password: None`
on the first attempt is correct, not a shortcut.

⚠ An encrypted **RAR** returns `OpenError::Unsupported`, not `PasswordRequired`
(`pb-source/src/lib.rs:283` — *"a format tier we don't decode … encrypted RAR"*). So an encrypted
`.cbr` door offers itself and then refuses with an honest "unsupported". That is today's Open-File
behaviour, not a regression — but the plan says so rather than implying every locked archive
prompts.

### 5. Cut: no resident archive cache (owner, 2026-07-16)

The owner's first sketch included *"if we've opened it before, we've got it in memory and treat it
like a folder"*. **Cut** — owner agreed: *"that probably could be a nasty trap, depending on what
someone has in their archives."* Keeping opened archives resident means holding decompressed
7z/tar.gz contents in RAM indefinitely, competing with the texture ring for the budget the whole
architecture is built around, and buying a latency cliff nobody can predict (same door, sometimes
instant, sometimes a progress dialog, depending on invisible session history). Entering is
entering; the progress dialog handles the slow case honestly.

A lazy-ZIP handle-pool cache is cheap and could return later **on measurement** — it is not what
makes this design good.

## ⚠ The audit that makes the promise true (owner question, 2026-07-16)

> *"We only try to read (and prompt for a password) when a user clicks the 'oPen' button or hits
> P, right?"*

**That is the design. It is not what the code would do if we only added the `decode_item`
arm.** Verified 2026-07-16 — there are at least two paths that read the whole archive with **no
keypress**:

| Site | What happens to a door | Cost |
|---|---|---|
| **Thumbs strip** — `decode_item_for` (`engine.rs:356`) | the guard is **negative** (`!matches!(kind, Video(_))`), so a door falls *into* the branch and calls `source.bytes(item)` (`:360`) | a full `fs::read` of **every archive in the folder**; `native_thumbs: true` by default (`main.rs:838`) |
| **`Shift+I` panel** (`app_core_impl.rs:6544`) | the guard is **positive-for-video**, so a door falls past the probe into the image path's sync `fs::read` (`:6568`) | the whole archive, **on the event loop** |

Still to audit (same pattern, unverified): `app_core_impl.rs:4046`, `:7114`, `:7315`.

### Root cause — a two-kind world

The tree encodes **video vs "everything else, therefore an image, therefore safe to read
bytes."** Nine sites match on `LibraryItemKind` (`app_core_impl.rs` ×7, `engine.rs` ×2,
`main.rs` ×1) and every one of them is an `if let` or a `!matches!`. A third variant silently
lands in the *image* bucket at all of them — so **reading is the default**, which is backwards
for a door.

### The fix — make the compiler find them, don't grep

`if let Video(_) = …` and `!matches!(…, Video(_))` **do not error when a variant is added.** So:

1. **Invert the guards to positive** — `Image` reads bytes; everything else routes to the typed
   dispatch. Then a future `LibraryItemKind` defaults to *not* reading.
2. **Convert the kind guards to exhaustive `match`** before adding the variant. Adding
   `Archive(_)` then produces a **compile error at every site that must decide**, which turns
   "audit nine places and hope" into a list the compiler hands you.

Do (2) **first**, as its own commit with no behaviour change. It is the difference between a
promise and a hope.

### 🪤 The no-trace test cannot catch this

`viewing_a_folder_writes_nothing_to_disk` (`main.rs:5120`) asserts nothing is **written**.
Reading a 2 GB archive writes nothing — **it passes while the app reads the entire corpus.** The
guarantee here is about *reads*, which that test was never built to see.

The test that catches it is a source whose **`bytes()` panics** (or counts calls), exercised
across **every** entry point — `decode_item`, `decode_item_for` with `Purpose::Thumb` *and*
`Purpose::Display`, and the panel path. rev4's Phase 1 originally specified this for
`decode_item` only; that would have shipped both leaks above with a green suite.

## Open question — verify before building: can you get back out?

**A door you cannot return from is a trap.** After entering, the deck is the archive's contents;
getting back to the folder-of-doors must work, or the feature is worse than nothing.

`climb_anchor` (`app_core_impl.rs:1200`) implies a climb/up mechanism exists. **Verify first**
that the existing up/climb command returns from an archive deck to its parent folder deck. If it
does, this is free and the "open the next archive" flow works: enter, browse, climb out, `P` the
next door. If it does not, **building it is part of this task** — not a follow-up.

This is the one thing that could make rev4 bigger than it looks. Check it before estimating.

## Phases

**Phase 0 — exhaustive guards, no behaviour change.** Convert the nine `LibraryItemKind` guards
from `if let` / `!matches!` to exhaustive `match`, and invert the read guards to positive
(`Image` reads bytes). Its own commit, green before and after. This is what makes Phase 1's
variant addition produce a compiler-generated to-do list instead of a grep. See *The audit*.

**Phase 1 — core typing + door, TDD, pure.** `LibraryItemKind::Archive`; the `item_kind` arm; the
`decode_item_cancellable` arm **above** the `bytes()` read (`engine.rs:491`);
`archive_placeholder`. Scan includes archives as items. Every site Phase 0's compiler errors
surfaced gets an explicit decision.

Tests, mirroring the ones video already has:
- an archive path types as `Archive(kind)`; `.tar.gz` types as `TarGz` (the double-extension case
  a name-based classifier gets wrong);
- an archive **entry** inside an archive types as `Image`/not-a-door (path-only, by construction);
- **a door never reads the archive — across every entry point.** A source whose `bytes()`
  **panics**, driven through `decode_item`, `decode_item_for` with `Purpose::Thumb` *and*
  `Purpose::Display`, and the `Shift+I` panel path. This is the feature's central promise, and
  both confirmed leaks live in entry points a `decode_item`-only test would miss. Mutation
  check: it must fail against today's negative guard (`engine.rs:356`).
- a door's tile is tiny (ring-budget property).

**Phase 2 — enter.** `P` on a door → exactly one `BeginArchiveOpen { password: None }`, and it
clears `climb_anchor` like any other navigation. Verify the climb-out (above).

**Phase 3 — the icon.** Per-`ArchiveKind` cached raster in the tile.

**Phase 4 — integration.** A folder of only archives shows doors and becomes the deck root (the
blocker that rev3 could not solve); `P` a zip → deck; `P` a 7z → progress → deck; `P` an encrypted
zip/7z → prompt → unlock → deck; `P` an encrypted `.cbr` → honest "unsupported"; climb out and
enter the next door.

**No-trace (do not skip).** `viewing_a_folder_writes_nothing_to_disk` (`main.rs:5120`) already
seeds `clip.mp4` with garbage bytes because *"the placeholder path must not even read them"*
(`main.rs:5130`). **Add a garbage `.zip` to that fixture and assert the same.** The test's shape
is already correct; this is a two-line extension and it pins the door's central promise.

**Docs:** `CLAUDE.md` archive bullet, CHANGELOG `Added`, tasks.json entry.

## Risks / open questions

1. **Climb-out** (above). The one thing that could change the size of this.
2. **Noise.** A `backup.zip` in a vacation folder becomes a door in the deck. This is exactly what
   a stray `.mp4` does today, so it is consistent rather than new — accept for v1, setting later
   if it grates.
3. **The random cycle can land on a door.** `Enter` = random photo; doors are items, so the
   pb-core invariant ("a random cycle visits each item exactly once") includes them. Same as
   landing on a video today. Accept and note — excluding them would break the invariant, which is
   a worse trade than an occasional door.
4. **Holding space flashes past doors.** Also already true of video posters. The tile is a solid
   color, so it costs a rebind like any other frame.
5. **`item_kind` is per decode job, not per frame** (`engine.rs:393`, `:357`) — `archive_kind`'s
   lowercase-extension `String` never lands on the hot path. **Verify no caller pulls `item_kind`
   into a per-frame path** before shipping.

## Explicit non-goals

- **Blending archive contents into a folder deck.**
- **Auto-entering an archive** from a scan, `Go ▸ Next Folder`, or recursion. A keypress, always.
- **Nested archives** — excluded by construction (doors are path-only).
- **A resident archive cache** (design §5, owner-cut).
- **Mouse-click-to-enter.** The owner's sketch allowed "a button… click to open"; the deck is
  keyboard-first and chrome-less, and `P` is approved. A click target in the wgpu deck can come
  later; it does not gate this.
- **A lock badge** (see the appendix).
- **Per-format icons** — one glyph per kind is already what §2 caches; distinct art per format is
  a later `match`.

---

# Appendix: the tree surface (rev3's plan — now optional, deferred)

rev1–3 put archive rows in the folder tree (`⇧F`). **Doors make that redundant for the core use
case**: navigating the tree to a folder already scans it, and the deck then fills with doors — so
you reach the archive without an archive row ever existing. **Recommendation: defer.** Keep the
tree a *folder* tree.

Retained because the analysis was expensive and is correct if the rows are ever wanted:

- **There are two tree implementations, and rev2 targeted the dead one.** Live: `fs_tree.rs` +
  `panels_ui.rs:2924` `tree_row` + `pb_ui::Icon`, selected by `native_tree: true` (`main.rs:839`)
  and `panels_ui.rs:342`'s `tree_is_fs()` branch (`app_core_impl.rs:2411`). Legacy (fallback
  only): `folder_tree.rs` / `TreeTarget` / `pb-hud` (`hud.rs:256`).
- **"Archive rows cannot expand by construction" was false.** `FsTree` children are untyped
  (`fs_tree.rs:36`), `push_rows` treats every child as a node (`:219`), and an unread child
  optimistically gets a chevron (`:230`). It would need a typed `Dir` vs `Archive{path,kind}`
  child — the property must be *built*, not asserted.
- **Do not retype `subdirs`.** It is shared with `sibling_with_photos` (`folder_tree.rs:664`) —
  the `Go ▸ Next/Prev Folder` commands — so retyping it in place would leak archives into folder
  navigation. Add a separate typed reader for the `FsTree` worker (`app_core_impl.rs:2520`).
- **Three activation dispatches**, none of them `folder_tree_click`: winit (`panels_ui.rs:3000` →
  `main.rs:2100` → `fs_tree_open`, `app_core_impl.rs:2550`), macOS (`mac-ffi:834`), legacy
  (`tree_activate`, `app_core_impl.rs:2387`). They would route through one core helper so no
  shell re-classifies.
- **FFI:** `TreeRowFfi` (`mac-ffi:151`) kind field; the accessor at `mac-ffi:568`; Swift
  `FolderTreeRow` (`FolderTreePanel.swift:13`); `icon(row)` (`:159`); `CoreModel.swift:877`.
- **`FsTree` caches children permanently** (`fs_tree.rs:114`) — a new archive would not appear
  until the panel reopens.
- **`set_children` sorts one undifferentiated list** (`fs_tree.rs:122`) — "folders first" needs a
  typed comparator.
- **A lock badge** would cost an archive open per row (`ZipSource::needs_password`,
  `pb-source/src/lib.rs:484`), and 7z headers can themselves be encrypted. Opportunistic only, if
  ever.
- **Paging is not on this path.** Codex flagged `TreeHit::PageUp/PageDown` citing
  `app_core.rs:508` — that line does not exist and the type lives in `pb-hud`
  (`pb-hud/src/hud.rs:270`, consumed at `app_core_impl.rs:2631`). Recorded so "add a paging
  regression test" is not mistaken for diligence.
- **The anchor lesson:** rev2's anchors all *existed* — several just meant something other than
  what rev2 claimed (it called `rows_from_paths` "the disk row builder"; its doc says *"build the
  tree **without touching the disk** … the hold-to-blaze fast path"*; the disk one is
  `rows_from_disk`, `folder_tree.rs:443`). **Check what the line means, not that a symbol sits on
  it.**
