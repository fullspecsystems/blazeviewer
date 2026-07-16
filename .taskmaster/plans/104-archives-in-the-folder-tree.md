# Task 104 — Archives in the folder tree: visible, navigable, never auto-entered

**Status:** planned — rev3 (2026-07-16, rewritten after a Codex review of rev2)
**Proposed task id:** 104 (add the task entry when this plan is approved)
**Depends on:** `pb_source::archive_kind` (`crates/pb-source/src/kind.rs:61`) — **shipped and on
`main`** (#102 tar family, #103 RAR5, merged `631e970`). Base this work on `main`.
**Scope:** core + both shells. Not small — see *Why rev2 was wrong*.

> **rev3 (2026-07-16).** rev2 was reviewed by Codex and **blocked**: it planned against the
> wrong tree. Its anchors were mostly real lines, but several meant something other than what
> rev2 said they meant — which is worse than a dangling anchor, because it survives a grep.
> Every finding below was re-verified against the source by hand before being written down.
> **rev2's phase plan is void; do not work from it.**

## Why rev2 was wrong (read this first)

**There are two folder-tree implementations, and rev2 targeted the dead one.**

| | Legacy | **Active** |
|---|---|---|
| Model | `folder_tree.rs` / `FolderTreeModel` / `TreeTarget` | **`fs_tree.rs` / `FsTree`** |
| Render | `pb-hud` raster path (`hud.rs:256` `TreeRow`) | **`panels_ui.rs:2924` `tree_row`** + `pb_ui::Icon` |
| Live? | fallback only | **yes** — `native_tree: true` (`main.rs:839`), `panels_ui.rs:342` branches on `core.tree_is_fs()` (`app_core_impl.rs:2411`) and reads `fs_tree_rows()` (`:2416`) |

So rev2's entire Phase 1–2 (`TreeTarget::Archive`, `rows_from_paths`, a `pb-hud` glyph) would
have produced **archive rows that never appear on either shell.** The legacy path still serves
the archive/empty-deck deck, so it is not deletable — but it is not where this feature lives.

Two more rev2 claims are simply false:

- **"`rows_from_paths` is the disk row builder"** — its own doc says the opposite: *"Build the
  tree **without touching the disk** … instead of `read_dir`. The hold-to-blaze fast path."*
  It is the **lite** builder. The disk one is `rows_from_disk` (`folder_tree.rs:443`).
- **"Archive rows cannot expand by construction"** — the reason this was wrong is the
  interesting one, so it gets its own section (*The typing problem*).

**Lesson for the anchor discipline:** rev2's anchors were checked for *existence*, which they
had. Check what the line **means**, not that a symbol sits on it.

## Problem

Archives are a **mode you have to invoke**. You reach one by explicitly choosing it in Open
File; while browsing a folder, the `.zip` / `.7z` / `.cbz` files sitting in it are invisible.
Owner (2026-07-16): *"using archives means specifically selecting them, but they don't just
load up, like, say, a folder would when recursing."*

A folder of archives is a dead end: you know they're there, and the app acts as if they aren't.

## The design (owner, 2026-07-16)

> *"Show them as folders in the folder view, but don't automatically navigate into them as if
> they are folders… So they become navigable from the folder view, but not automatic where you
> suddenly OOM the user or confuse the experience."*

Archives appear as **leaf rows in the folder tree** (`⇧F`) with a distinct glyph; clicking one
opens it through the **existing** explicit-open path. Never expanded, never descended into by
a scan, never opened without a click.

The cost model falls out of that: listing a folder of archives costs the `read_dir` it already
costs. Decompression, the RAM pre-flight and the password prompt happen only when a human
picks one.

## The blocking design gap: what folder is the tree even showing?

**This is the finding that decides whether the feature works at all**, and rev2 never
addressed it. The literal use case — *"a folder of encrypted 7z archives"* — is **currently
unreachable**:

- A scan of a photo-less folder produces **no batch**: empty snapshots are skipped
  (`app_core_impl.rs:1114`), and `finish_scan` (`:1140`) leaves the existing source and root
  **untouched** — deliberately (③ keep-deck-until-photos, owner 2026-07-05: a mis-click must
  never blank a deck). `scan_found_no_photos` (`:1154`) just toasts.
- So opening a folder that contains **only** archives leaves the tree pointed at wherever you
  already were. The archive rows would render — in the wrong folder.
- And once an archive **is** open, the tree comes from `rows_from_names` (`:2241`) over the
  resident entry names, so sibling archives on disk are not present at all: you cannot get from
  one archive to the next without going back through Open File. Which is the actual complaint.

**Therefore rev3's central requirement: the tree needs a browsing location independent of the
photo deck.** Today `FsTree`'s location is a shadow of the deck's root. It must become
navigable on its own, so that "browse a folder of archives, click one, then click the next"
works. This is the real work of task 104; the glyph is the easy part.

**Decision needed from the owner (recommendation: A).**

- **A — the tree gets its own `current` location** *(recommended)*. `fs_tree_open` on a folder
  moves the tree there and kicks a scan; if the scan finds no photos, the deck stays as-is (rule
  ③ preserved) **but the tree stays where you navigated**. Smallest change that makes the use
  case work, and it matches every file manager: the folder pane doesn't jump back because a
  folder had nothing to preview.
- **B — a photo-less folder becomes the root anyway.** Rejected: directly contradicts owner rule
  ③ and would let a mis-click blank a live deck.
- **C — archive rows only in folders that already have photos.** Rejected: the owner's own
  example (a folder of *only* `.7z`s) is exactly the case it fails.

Everything below assumes **A**.

## The typing problem (why "cannot expand by construction" was false)

`FsTree`'s children are **untyped**: `children: Option<Vec<PathBuf>>` (`fs_tree.rs:36`).
`push_rows` (`:219`) treats every child as a node, and an unread child **optimistically gets a
chevron** (`:230` — `has_children: children.is_none_or(|c| !c.is_empty())`). Drop a `.cbz` path
into that list as-is and you get an expandable archive row that kicks a `read_dir` on a file.

So the "by construction" property rev2 *asserted* has to be **built**:

```rust
enum Child {
    Dir(PathBuf),
    /// Kind is stored at enumeration time, never recomputed per render.
    Archive { path: PathBuf, kind: ArchiveKind },
}
```

with `push_rows` matching on it: `Dir` keeps today's behaviour; `Archive` emits
`has_children: false`, `loading: false`, `count: None`, and is never handed to the read worker.
*Then* it is unrepresentable rather than a rule. Legacy `folder_tree.rs`'s `Role::At` → always
`TreeTarget::Dir` (`:517`) is untouched — the legacy tree gets no archive rows.

> **Prime directive:** `archive_kind` allocates a lowercased extension `String`
> (`kind.rs:61`). It runs **once per entry at enumeration**, never in `tree_row`. No per-frame
> heap on the render path.

## Enumeration: a new typed reader, not a retyped `subdirs`

The tree worker reads via `folder_tree::subdirs` (`app_core_impl.rs:2520`), which hard-filters
to directories (`folder_tree.rs:326`, `if !e.file_type().ok()?.is_dir()`).

**Do not retype `subdirs` in place.** It has a second consumer: `sibling_with_photos`
(`folder_tree.rs:664`) — the `Go ▸ Next/Prev Folder` commands, whose doc says they *"step
through the same listing the tree shows."* Retyping it would leak archives into Go-sibling
navigation, which must stay directory-only (an archive is not a sibling folder to step onto).

So: **add `folder_tree::dir_entries(dir) -> Vec<Child>`** (dirs + archives, one `read_dir`,
same hidden-file rules), used only by the `FsTree` worker. `subdirs` keeps its signature, its
tests, and its callers. Go-sibling stays directory-only **by construction** rather than by a
filter someone must remember. This is pure and read-only — `pb-app-core` is I/O-free, so
`dir_entries` lives beside `subdirs` and takes the same `&Path`.

## Activation: three dispatches, one core helper

Rev2 planned one click arm. There are **three** paths, and none is `folder_tree_click`:

| Shell | Path |
|---|---|
| winit | `panels_ui.rs:3000` `TreeOpen(path)` → `main.rs:2100` → `AppCore::fs_tree_open` (`app_core_impl.rs:2550`) |
| macOS | `pb-mac-ffi/src/lib.rs:834` branches on `tree_is_fs()` |
| legacy fallback | `AppCore::tree_activate` (`app_core_impl.rs:2387`) — `Dir｜Scope` only |

`fs_tree_open(path: PathBuf)` takes a bare path with no kind, so **all three would need the
same "is this an archive?" test** — the exact duplication `archive_kind` exists to end.

**Instead: one core helper, `AppCore::fs_tree_activate(row_index)`** (or `(path, kind)`), which
owns the branch:

```rust
match kind {
    None => /* today's fs_tree_open behaviour */,
    Some(_) => self.effects.push(CoreEffect::BeginArchiveOpen { path, password: None }),
}
```

Both shells call it; the kind comes from the row (already typed), so no shell re-classifies and
no fourth predicate appears. It must also share `open_plan`'s `climb_anchor = None` reset
(`app_core_impl.rs:1200`) — opening an archive ends a climb like any other navigation.

`BeginArchiveOpen` is **shell-side** (`main.rs:1038`, `mac-ffi:2730`), so core must **push the
effect**, never call it. Both shells already handle it: RAM pre-flight, progress dialog,
cancel, and the password re-open all come free.

### Passwords: verified working, but ZIP/7z only

The `PasswordRequired` (`pb-source/src/lib.rs:262`) → prompt → re-open-with-`Some(pw)` loop is
real on both shells (`app_core_impl.rs:1024`, `main.rs:1148`, `mac-ffi:2819`). `password: None`
on the first attempt is correct, not a shortcut.

⚠ **But the owner's "folder of encrypted archives" case is ZIP and 7z only.** An encrypted
**RAR** returns `OpenError::Unsupported`, not `PasswordRequired` (`pb-source/src/lib.rs:283` —
"a format tier we don't decode … encrypted RAR"). So an encrypted `.cbr` row offers itself and
then refuses with "unsupported". Acceptable for v1 (it is today's behaviour via Open File, not
a regression) but **say so** rather than implying every locked archive prompts.

## Icons

One archive glyph for v1 — the format is already legible in the row (it's the extension). Kind
is `Copy` and carried on the row, so per-format glyphs are a later `match`.

- **winit:** a `pb_ui::Icon` variant (`pb-ui/src/icon.rs:49`), vendored per `CLAUDE.md`'s icon
  workflow, drawn by `panels_ui.rs:2924` `tree_row`. **Not** a `pb-hud` glyph (rev2's Phase 2).
- **macOS:** `FolderTreePanel.swift:13` `FolderTreeRow` needs a kind field; `icon(row)` gains a
  case (`doc.zipper` / `archivebox`).

## FFI (rev2 said "whatever FFI carries the row kind" — here it is)

1. `TreeRowFfi` (`pb-mac-ffi/src/lib.rs:151`) gains a kind field (a `u8`/enum — the bridge is
   not generic over Rust enums).
2. The bridge accessor that builds the rows for Swift (`lib.rs:568`, `for r in self.core.fs_tree_rows()`) fills the new field.
3. Swift `FolderTreeRow` (`FolderTreePanel.swift:13`) mirrors it.
4. `CoreModel.swift:877`'s `FolderTreeRow(...)` construction carries it through.

⚠ Cannot be compiled from the Windows box. `cargo check -p pb-mac-ffi` **does** run there and
validates the Rust half + swift-bridge codegen; the Swift half needs a Mac.

## The rejected alternative — blending archive contents into a folder scan

Rejected 2026-07-16 (owner: *"just listing archives as part of normal browsing is fraught with
problems we shouldn't have to deal with"*). Recorded because it is the obvious idea.

> **rev3 correction.** rev2 argued this from a **false premise**: *"listing a folder holding
> three 2 GB `.7z` files would mean decompressing 6 GB into RAM to draw a directory listing."*
> `ArchiveKind::eager()` (`kind.rs:38`) documents what our current **open** does — it is not
> evidence that *enumeration* requires retaining decompressed contents. A listing-only pass
> could stream and discard. Deleted; the honest arguments stand on their own.

1. **The prefetch ring would cross the archive boundary.** This is the real one. A blended deck
   makes archive entries ordinary items, so the direction-biased prefetch window decodes ahead
   into them — **decompressing archives the user never clicked**, as a side effect of scrolling
   near them. That is the OOM the owner named, and it arrives without any click.
2. **Format-dependent cost is an unexplainable user model.** *"Why did it descend into
   `vacation.zip` but not `vacation.7z`?"* — and with RAR, per-*file*, since solidness is a
   property of the archive, not the suffix: two `.cbr`s side by side, one cheap, one not, with
   nothing on screen to tell them apart.
3. **No file manager blends.** Explorer makes a `.zip` navigable; it never merges its contents
   into the parent listing. Navigation is the idiom.
4. **A folder of `.cbz` files is a shelf of books.** Blending fuses forty comics into one
   4,000-page deck with no chapter boundaries.
5. **It is one-way.** If you wanted them separate, you cannot un-blend.
6. **It needs an architecture change.** `AppCore.source` is a single `Arc<dyn ItemSource>`
   (`app_core.rs:361`) — one deck, one source. Blending needs a composite source or per-item
   routing.

## Phases

**Phase 0 — owner decision on the browsing-location gap** (A/B/C above). Blocks everything;
without it the feature does not reach its use case.

**Phase 1 — core, TDD, pure.** `Child` typing in `fs_tree.rs`; `folder_tree::dir_entries`;
`push_rows` archive arm; `fs_tree_activate`; the tree's own `current` (per Phase 0).

> Testability: `dir_entries` touches disk, so tests feed `Child` lists directly into `FsTree`
> (`set_children` already takes a `Vec`) — the typing, row shape, sort, and activation are all
> pure. `dir_entries` itself gets a tempdir test beside `subdirs`' existing one
> (`folder_tree.rs:1052`).

Tests: mixed dirs/archives/images → expected rows (folders first, archives after, images
absent); an archive row has `has_children: false`, `loading: false`, `count: None`; it is never
queued for a child read; activation on an archive row emits **exactly one**
`BeginArchiveOpen { password: None }` and clears `climb_anchor`; activation on a dir keeps
today's behaviour; `subdirs` and Go-sibling are unchanged (regression).

**Phase 2 — winit.** `pb_ui::Icon` variant + `tree_row` case; gallery entry.

**Phase 3 — macOS.** FFI kind (4 steps above) + the Swift icon case. Needs a Mac.

**Phase 4 — integration.** Click a zip row → deck; a 7z row → progress dialog → deck; an
encrypted zip/7z → prompt → unlock → deck; an encrypted `.cbr` → the honest "unsupported"
error; browse a folder of only archives (the Phase 0 case) and open two in a row.

**Docs:** `CLAUDE.md` archive bullet, CHANGELOG `Added`, tasks.json entry.

## Details rev2 missed

- **`FsTree` caches children permanently** (`fs_tree.rs:114` `needs_children` — read once, never
  invalidated). An archive added to a folder while the panel is open never appears. Today that
  is a folder-only staleness nobody notices; archives make it more visible. **Decide:**
  invalidate on panel reopen (cheap, recommended) or accept.
- **`set_children` sorts one undifferentiated list** (`fs_tree.rs:122`, case-insensitive by
  display name). "Folders first, then archives" needs a **typed comparator** — sort by
  `(is_archive, name)`. It does not fall out of the existing sort.
- **Paging: not on this path.** Codex flagged the `TreeHit::PageUp/PageDown` "… n more" markers
  as a gap, citing `app_core.rs:508` — **that line does not exist, and neither does the type
  there.** `TreeHit` is a `pb-hud` type (`pb-hud/src/hud.rs:270`), consumed only by the legacy
  path (`app_core_impl.rs:2631`); the marker machinery is `hud.rs:1080`. The native tree scrolls
  instead. So paging needs no change and no new test here. Recorded because "add a paging
  regression test" would otherwise look like diligence rather than the wasted work it is.
- **No-trace.** Listing is `read_dir` only, so `viewing_a_folder_writes_nothing_to_disk`
  (`main.rs:5120`) should pass unchanged — **extend it over the new listing path** rather than
  assuming.
- **Keyboard nav.** There is no core tree-navigation model — `Action::FolderTree`
  (`action.rs:102`) only toggles the panel. Archive rows are mouse-activated like every other
  row. **Declared non-goal**, noted so it isn't discovered mid-implementation.

## Corrections to rev2's coordination section

- ~~"Base this on `feat/enhanced-archives`"~~ — **merged to `main`** (`631e970`). Base on `main`.
- ~~"Do not add a fourth `is_archive` … the three copies only agree by luck"~~ — **historical.**
  #102/#103 already fixed that: both shells' `is_archive` (`main.rs:4388`, `mac-ffi:3188`) are
  now one-line wrappers over `pb_source::archive_kind`. The rule survives in a better form:
  **the row carries the kind from enumeration; nothing re-classifies.**

## Risks / open questions

1. **Noise.** A folder of `.zip` backups shows rows nobody wants. No setting in v1; revisit if
   it grates.
2. **The lock badge costs what this design saves.** `ZipSource::needs_password`
   (`pb-source/src/lib.rs:484`) can answer it — a zip's central directory is readable without
   the password — but it costs an archive open per row, and an SMB round trip each. 7z is worse:
   its header can itself be encrypted. **Not in v1**; if it lands, opportunistic only — filled
   lazily off-thread for rows already on screen, blank otherwise, never blocking the tree.
3. **Does the tree stop being "the folder tree"?** It becomes a places tree. Fine: it is our
   only navigation surface.
4. **Root/up rows.** `push_up` (`folder_tree.rs:70`) and `Role::Root`/`Role::At` (`:517`) assume
   directories. Once archives are **typed leaves** they can never be ancestors, so those paths
   stay directory-only by construction. Codex confirmed this is sound — verify, don't assume.

## Explicit non-goals

- **Blending archive contents into a folder deck.**
- **Auto-entering an archive** from a scan, `Go ▸ Next Folder`, or recursion. A click, always.
- **Nested archives** (an archive inside an archive) — the inner bytes are in RAM, not on disk.
- **Expanding an archive in place** — the eager-decompression trap wearing a smaller hat.
- **A lock badge in v1** (risk #2).
- **Per-format icons in v1.**
- **Tree keyboard navigation** (no core model exists).
