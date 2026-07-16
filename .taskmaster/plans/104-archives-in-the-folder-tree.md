# Task 104 — Archives in the folder tree: visible, navigable, never auto-entered

**Status:** planned — rev1 (2026-07-16, not yet Codex-reviewed)
**Proposed task id:** 104 (next free — `tasks.json` tops out at **101**; plans 102/103 exist
without task entries yet; add the task entry when this plan is approved)
**Depends on:** **#102's `archive_kind` classifier** — hard dependency, see *Coordination*.
**Scope:** all three platforms. Core + `pb-hud` are shared; each shell adds one glyph case.

## Problem

Archives are a **mode you have to invoke**. You reach one by explicitly choosing it in Open
File; while browsing a folder, the `.zip` / `.7z` / `.cbz` files sitting in it are invisible
to the app. Owner (2026-07-16): *"using archives means specifically selecting them, but they
don't just load up, like, say, a folder would when recursing."*

So a folder of archives is a dead end: you know they're there, and the app acts as if they
aren't.

## The rejected alternative — and why it stays rejected

**Blending archive contents into a recursive folder scan** (i.e. recursing *into* archives so
their images join the deck). Discussed and rejected 2026-07-16 (owner: *"just listing archives
as part of normal browsing is fraught with problems we shouldn't have to deal with"*). Recorded
here because it is the obvious idea and will be re-proposed otherwise:

1. **Half the formats cannot be enumerated without decompressing them.** `pb-source` splits
   into *lazy* random-access (zip, cbz, tar, cbt — a cheap index pass) and *eager* (7z,
   tar.gz, tar.bz2, tar.zst — "pays the whole decompression once on open",
   `crates/pb-source/src/lib.rs:17-32`). Listing a folder holding three 2 GB `.7z` files
   would mean decompressing 6 GB into RAM **to draw a directory listing**, and
   `seven_z_projected_bytes`' pre-flight would refuse them mid-scan during an operation the
   user never asked for. Scan cost becomes proportional to archive *contents* rather than
   directory size — on the SMB corpus, brutal. That is a straight prime-directive violation.
2. **It would therefore be format-dependent**, which is an unexplainable user model: *"why
   did it descend into `vacation.zip` but not `vacation.7z`?"*
3. **No file manager blends.** Explorer makes a `.zip` *navigable* — you double-click into
   it — but never merges its contents into the parent listing. Navigation is the idiom;
   merging is not.
4. **A folder of `.cbz` files is a shelf of books.** Blending fuses forty comics into one
   4,000-page deck with no chapter boundaries. And "zip of photos" vs "zip that is a book"
   is not reliably distinguishable.
5. **It is one-way.** If you wanted them separate, you cannot un-blend.
6. **It needs an architecture change.** `AppCore.source` is a single `Arc<dyn ItemSource>`
   (`crates/pb-app-core/src/app_core.rs:361`) — one deck, one source. Blending needs a
   composite source or per-item routing.

## The design (owner, 2026-07-16)

> *"Show them as folders in the folder view, but don't automatically navigate into them as if
> they are folders… So they become navigable from the folder view, but not automatic where you
> suddenly OOM the user or confuse the experience."*

Archives appear as **rows in the folder tree** (`⇧F`), with a glyph distinct from a folder,
and clicking one opens it through the **existing** explicit-open path. They are never
expanded, never descended into by a scan, never opened without a click.

The whole cost model falls out of that: listing a folder of archives costs the `read_dir` it
already costs. The decompression, the RAM pre-flight, and the password prompt all happen only
when a human picks one — which is exactly what happens today, just reached from a better place.

### Why this is small: every seam already exists

The tree was already built with more than one kind of target.

| Piece | Today | Change |
|---|---|---|
| `TreeTarget` (`folder_tree.rs:52`) | `Dir(PathBuf)` \| `Scope(String)` | **+ `Archive(PathBuf)`** |
| Click dispatch (`app_core_impl.rs:2642-2653`) | matches those two arms | **+ one arm** |
| `CoreEffect::BeginArchiveOpen { path, password }` (`contract.rs:377`) | **already exists**, already drives the RAM pre-flight, the progress dialog, and the password re-open | reused verbatim |
| Disk row builder `rows_from_paths` (`folder_tree.rs:420`) | folders only | emits archive rows |
| `hud::TreeRow` (`crates/pb-hud/src/hud.rs:256`) | `{depth,name,open,current,marker,up,count}` | **+ a kind** |
| winit glyph (`pb-hud`, FA folder glyphs, icon cell already width-aligned per depth) | folder / folder-open / folder-up | + archive |
| macOS glyph (`FolderTreePanel.swift:109`, `Image(systemName: icon(row))`) | per-row `icon(row)` already | + a case |

`TreeTarget::Dir` → `open_dir` works because `open_dir` is core-side; `begin_archive_open` is
**shell-side** (`crates/pb-app/src/main.rs:1035`, `crates/pb-mac-ffi/src/lib.rs:2728`), so the
Archive arm must **push `CoreEffect::BeginArchiveOpen`** rather than call it. That effect
already exists and both shells already handle it — the password dialog, the 7z pre-flight and
the progress/cancel plumbing come along free.

### 1. `TreeTarget::Archive(PathBuf)`

```rust
pub enum TreeTarget {
    Dir(PathBuf),
    Scope(String),
    /// A `.zip`/`.7z`/`.cbz`/… sitting in the current folder. Clicking opens it as its own
    /// deck — it is NEVER expanded in place and NEVER entered by a scan.
    Archive(PathBuf),
}
```

Click dispatch gains:

```rust
Some(TreeTarget::Archive(path)) => self
    .effects
    .push(CoreEffect::BeginArchiveOpen { path, password: None }),
```

`password: None` is correct and not a shortcut: the shells' existing failure path turns
`OpenError::PasswordRequired` (`pb-source/src/lib.rs:243`) into the prompt and re-opens via the
same effect with `Some(pw)`. Nothing new is needed for encrypted archives.

### 2. Archive rows never expand — by construction, not by rule

An archive row carries **no children and no chevron**. Model it so that is unrepresentable
rather than a rule someone can forget: the row kind has no child list, so the expand/collapse
path (`fs_tree.rs:114` `needs_children` / `:122` `set_children` / `:140` `expand`) can't reach
it. This is the *"don't automatically navigate into them"* requirement, encoded.

### 3. The count badge stays `None`

`TreeRow.count` is already `Option<u64>`, so "we don't know" is already representable —
**use it.** We cannot know an archive's photo count without opening it, which is the entire
cost this design avoids. A `0` would be a lie and a real count would defeat the point.

### 4. Sorting: folders first, then archives, each alphabetical

The Explorer idiom, and it keeps the folder tree scannable while making the archive group
obvious at a glance.

### 5. Icons: one archive glyph now

The owner floated per-format glyphs ("maybe we even can have separate icons for each format,
but the key thing is making them distinct from folders"). **Recommendation: one archive glyph
for v1.** The format is already legible in the row — it is the file extension, right there in
the name. Per-format glyphs are cheap to add later behind #102's `ArchiveKind` if wanted.

The distinction that is genuinely *invisible* is **which archives are locked** — which is the
owner's own example ("a folder of encrypted 7z archives"). But see *Risks* #2: it costs an
open per row, which is precisely the cost this design removes. **Not in v1.**

## Coordination — ⚠ read before starting

A **parallel agent is working in `../blazeviewer-wt1`** on #102/#103 (tar family, RAR5), in
these exact files.

- **Use #102's `pb_source::archive_kind`. Do not add a fourth `is_archive`.** There are
  already three copies (`crates/pb-app/src/main.rs:4392`, `crates/pb-mac-ffi/src/lib.rs:3197`,
  plus `scan.rs`'s ad-hoc `is_7z`) which #102 notes "only agree by luck". A fourth that drifts
  would show `.cbz` as an archive row in the tree and then refuse to open it — the worst
  possible failure, because it is *offered* and then declined.
- Therefore **land this after #102's phase 1** (the classifier), or coordinate the classifier
  landing first. Everything else here is independent of which formats exist.
- 🪤 **#102's plan cites stale line anchors** (as of 2026-07-16 it says `is_archive` at
  main.rs:4284 → really **4392**; `begin_archive_open` at 1008 → really **1035**; mac-ffi
  3165 → **3197**, 2696 → **2728**). The anchors in *this* plan were verified at rev1 and will
  drift the same way — re-grep, don't trust either document's line numbers.

## Phases

**Phase 1 — core, TDD.** `TreeTarget::Archive`; `rows_from_paths` emits archive rows via
`archive_kind`; the click arm pushes `BeginArchiveOpen`. Tests: a folder of mixed
folders/archives/images produces the expected rows+targets (folders first, then archives,
images absent); an archive row's target is `Archive(path)` with the right path; an archive row
has no children and cannot expand; `count` is `None`; a non-archive file produces no row; the
click arm emits exactly one `BeginArchiveOpen` with `password: None`.

**Phase 2 — `pb-hud` + the winit renderer.** `TreeRow` kind; the archive glyph (Font Awesome
`file-zipper` or `box-archive`, `solid` — vendor per `CLAUDE.md`'s icon workflow); the icon
cell already aligns names per depth, so confirm the new glyph doesn't widen it. Gallery/golden
coverage as the tree has today.

**Phase 3 — macOS.** One case in `FolderTreePanel.swift`'s `icon(row)` (SF Symbols —
`doc.zipper` / `archivebox`), plus whatever FFI carries the row kind across. ⚠ Cannot be
compiled from the Windows box; needs a Mac build.

**Phase 4 — integration.** Click an archive row → deck opens (zip, and a 7z with its progress
dialog); an **encrypted** archive row → prompt → unlock → deck, through the untouched existing
path; the no-trace guarantee still holds (listing archives is `read_dir` only, so
`viewing_a_folder_writes_nothing_to_disk` should need no change — confirm).

**Docs:** `CLAUDE.md` archive bullet, CHANGELOG `Added`, tasks.json entry.

## Risks / open questions

1. **Noise.** A folder of `.zip` backups now shows rows nobody wants. No setting in v1
   (defaults on); revisit if it grates. A setting is trivial to add later, but shipping one
   nobody asked for is worse.
2. **The lock badge costs what this design saves.** `ZipSource::needs_password`
   (`pb-source/src/lib.rs:386`) can answer it — a zip's central directory is readable without
   the password — but it costs an archive open per row, and on SMB a round trip each. 7z is
   worse: its header can itself be encrypted, so sometimes you cannot know without trying.
   **If it lands later, it must be opportunistic**: filled in lazily off-thread for rows
   already on screen, blank otherwise, never blocking the tree.
3. **Does the tree stop being "the folder tree"?** It becomes a places tree. Judged fine: it
   is our only navigation surface (we have no separate file pane), so this is where an archive
   has to appear at all.
4. **Root-level and up-row behaviour.** `push_up` (`folder_tree.rs:70`) and the `Role::Root` /
   `Role::At` targets (`folder_tree.rs:515-517`) assume directories. An archive is never an
   ancestor, so it should never reach those paths — verify rather than assume.
5. **Interaction with an already-open archive deck.** The tree in an archive deck is built by
   `rows_from_names` (`folder_tree.rs:224`) over the resident name list and targets
   `Scope`. Nested archives (a `.zip` inside a `.zip`) are **out of scope** — see non-goals.

## Explicit non-goals

- **Blending archive contents into a folder deck** (see above — the whole point).
- **Auto-entering an archive** from a scan, `Go ▸ Next Folder`, or recursion. A click, always.
- **Nested archives** (an archive inside an archive). The inner one's bytes are in RAM, not on
  disk, and `TreeTarget::Archive` carries a `PathBuf` — a different feature, not a stretch of
  this one.
- **Expanding an archive in place** (showing its internal folders as tree children before it is
  opened). That is the eager-decompression trap wearing a smaller hat.
- **A lock badge in v1** (risk #2).
- **Per-format icons in v1** (design §5).
