# Task 103 — RAR5 archive viewing (`RarSource`)

**Status:** implemented — rev2 (2026-07-16), after #102 as ordered. `pb-source/src/rar.rs`
(own container parser + `ItemSource`), wired into both shells through the #102
`load_archive` dispatch. **Corpus differential: 18 archives, 60 entries byte-identical to
`unrar`, zero mismatches** (`rar_corpus_matches_unrar`, `--ignored`, PB_RAR_CORPUS); the
refusals are all honest per-entry Unsupported (Delta-filtered bmp/wav content + solid-group
degradation from the Delta member on), exactly the designed scope. 12 unit tests over
committed WinRAR 7.23 fixtures (`tests/fixtures/rar/`, generation commands in the test
module) + a `rar_open` cargo-fuzz target.

**Implementation decisions that refine rev1** (recorded, not silently changed):
1. **Dependency pin:** an exact **rev** (`5c47b0e`, the pushed fork branch tip carrying the
   x86 fix), stricter than the planned branch reference — a branch dep is mutable, and the
   plan's own rule is exact-pin. Upstream PR #121 is still open; swap to `compcol = "=x.y.z"`
   when it merges + releases.
2. **Encrypted RAR → `OpenError::Unsupported` (honest message), NOT `PasswordRequired`** —
   deviation from rev1 §2, deliberate: `PasswordRequired` routes to the shells' password
   prompt, and no typed password can help (we detect RAR encryption; we do not decrypt), so
   prompting would loop forever. Same honest-refusal channel serves RAR4 and multi-volume.
   An archive whose *every* item is unavailable for one shared reason refuses at open with
   that reason instead of showing a deck of N failing items.
3. **Delta is currently the whole solid-group tail** (as scoped): a refused member marks
   itself + everything after it in its group unavailable; earlier members and other groups
   serve. **Upgrade path already sighted:** the local (unpushed) compcol branch
   `rar3-standard-filters` (da7f779) decodes Delta and adds `Decoder::add_file_boundary`
   (correct per-file x86 semantics in solid groups). When it lands upstream, bump the pin
   and register member boundaries in `decode_solid_group` — the Delta refusals then simply
   stop happening. Until then, an x86-filtered member *after the first* in a solid group
   decodes with wrong call targets; our CRC catches it (per-entry "damaged") and x86 filters
   never apply to photos.
4. **Fixture subtlety worth remembering:** WinRAR sorts solid input by extension, and its
   content analysis Delta-filters *any* fixed-stride data — the delta fixture pins order
   with `-ds`, and the text fixtures are deliberately aperiodic.

## Goal

View images (and play videos) inside **RAR5** archives, through the existing
`pb_source::ItemSource` seam, exactly like ZIP/7z do today — RAM-only, read-only, no-trace.
`.rar` (and `.cbr`, comic-book RAR = RAR renamed) open as playlists.

**Scope is RAR5, honestly bounded** (from the spike, all verified against real WinRAR 7.23):
- **In:** stored + LZ/Huffman entries, x86-filtered entries, all dictionary sizes, non-solid
  (lazy per-entry) and solid (eager sequential). This is byte-perfect vs `unrar` **after our
  x86 fix** (§1).
- **Graceful `Unsupported` per-entry** (archive still opens; the entry shows an honest error):
  Delta/ARM-filtered entries (until compcol grows Delta — the corpus's `gradient.bmp`/RAW case),
  encrypted entries.
- **Out:** RAR4 (detect signature → "RAR4 not supported yet"; the decoder work is a separate
  *upstream* compcol effort — see `~/code/compcol-rar-corpus/RAR4-IMPLEMENTATION-PLAN.md`, **not**
  this task); encryption *decrypt*; recovery records; multi-volume (v1).

## 1. The compcol dependency — the one real gate

RAR5 decode comes from **`compcol` (`features=["rar5"]`)**, a pure-Rust MIT crate we've audited
and are contributing to. **It must carry our x86-filter fix (upstream PR #121)** — the crates.io
release still has the bug that corrupts x86-filtered executables (48 wrong bytes on a real
notepad.exe slice; the whole reason we found it via differential testing).

**Until #121 is merged + released**, depend on our fork branch:
```toml
compcol = { git = "https://github.com/jdlien/compcol", branch = "rar5-x86-filter-fix", features = ["rar5"] }
```
**Once released**, switch to an exact crates.io pin (`compcol = "=x.y.z"`) at/after the version
containing #121. Exact-pin regardless (single-vendor, 6-week-old crate; review the diff on every
bump — zero-dep + forbid-unsafe makes that tractable). Add a `THIRD-PARTY-NOTICES.md` entry (MIT,
clean-room). This is the *only* external blocker; nothing else here waits on anyone.

## 2. Design — `RarSource` in `pb-source`

New module `crates/pb-source/src/rar.rs`, re-exported from the crate root, implementing
`ItemSource` like `ZipSource`/`SevenZSource`. **Write our own ~350-line container parser on
`compcol::rar5`; do NOT depend on `fstool`.** fstool proved the shape works but is a monolithic
crate (all ~15 filesystem backends + a clap/serde/uuid dep floor compile in), skips CRC, has no
RAR fuzzing, and its `&mut dyn BlockDevice` threading forbids concurrent readers — wrong for our
decode pool. **Use fstool as a *reference implementation*** (`~/code/fstool/src/fs/archive/rar.rs`):
its RAR5 block-chain scan (~165 lines) and its solid-group decode-once cursor (`LiveSolid`,
`out_pos`/`starts[]`, stored-member-in-solid rejection, ~150 lines) are the crib sheet.

Two access models, both already precedented in `pb-source`:

- **Non-solid → lazy per-entry** (ZIP's model). Scan the block chain once for the file table
  (names + `unpack_size` + per-entry compressed location), no decode. `bytes(i)` seeks to the
  entry and decodes it independently through `compcol::rar5::Decoder`. `random_access() = true`.
- **Solid → eager, or a forward cursor** (7z's model). A solid group is one continuous LZ stream
  over a shared window; members can't be decoded independently. Follow fstool's `LiveSolid`: one
  persistent decoder driven forward; reuse the cursor when reads advance, rebuild-from-group-start
  on a backward read. Simplest correct v1: **eager-decode the whole solid group to RAM on open**
  (like `SevenZSource`), reusing the existing `OpenProgress` cancel/RAM-budget plumbing and the
  `MAX_ENTRY_BYTES` guard. Optimize to the live cursor only if a real archive shows the eager cost
  hurts.

**Things we add that neither compcol nor fstool do:**
- **CRC32 verification** of decoded entries against the RAR5 header CRC (both upstreams skip it →
  silent garbage on corruption). `crc32fast` or the in-tree impl.
- **Window-size cap before constructing the decoder** — a hostile RAR5 header can demand a 1 GiB
  window; cap it (fstool uses 64 MiB) so a malicious header can't force a huge allocation.
- **Encryption detection at the container layer** → `OpenError::PasswordRequired` (reuse the
  existing password-prompt flow), never feed ciphertext to the decoder (fstool's bug: it surfaces
  a generic "corrupt" instead).
- **Solid-group degradation:** an unsupported (Delta) member inside a solid group must mark *that
  group's later members* unavailable, not error the whole archive (fstool poisons the stream here).

## 3. App wiring (reuses #102's classifier)

- `ArchiveKind::Rar` (and `.cbr`/`.cbz` alias handling) in the pb-source classifier from #102 —
  `.cbz` is just ZIP, add it too. Kills the last of the copy-pasted `zip`/`7z` predicates
  (`pb-app/src/main.rs:4284`, `pb-mac-ffi/src/lib.rs:3165`, `pb-app-core/src/scan.rs`).
- `scan.rs::open_archive` dispatches `Rar` → `RarSource` (eager solid uses the async
  `begin_archive_open` + progress dialog path, like 7z; non-solid can open sync like ZIP).
- Picker filter: add `rar`, `cbr`, `cbz`. **No OS file association** for `.rar` (don't steal it
  from the system, same rule as ZIP).
- `is_supported_archive_entry` reuses as-is (archived videos inside a RAR play from RAM via
  `VideoInput::Bytes`, like ZIP/7z).

## 4. Phases

0. **Unblock:** wire the fork git-dep; confirm `compcol::rar5` decodes `~/code/compcol-rar-corpus`'s
   RAR5 archives from a throwaway harness (the differential harness there already does this).
1. **`RarSource` non-solid** (lazy): scan + file table + per-entry decode + CRC + window cap +
   encryption detection. TDD against the corpus's `rar5_m*_nonsolid` archives.
2. **`RarSource` solid** (eager first): whole-group decode-to-RAM + progress/cancel/budget; solid
   corpus archives byte-identical to unrar; Delta-member-in-solid degrades gracefully.
3. **App wiring:** classifier `Rar`/`.cbr`/`.cbz`, dispatch, picker, no-trace test
   (`viewing_a_rar_writes_nothing_to_disk`), archived-video-in-rar smoke.
4. **Harden:** graceful Unsupported (Delta/ARM/encrypted) surfaces as an honest per-entry message;
   `.cbr` end-to-end; docs (CLAUDE.md archive section, CHANGELOG, tasks.json entry).

## 5. Tests

- **Differential is the bar:** decoded entries **byte-identical to `unrar`** (the corpus + harness
  in `~/code/compcol-rar-corpus` are the ground truth; the RAR5 corpus already exists there).
- **No-trace:** `viewing_a_rar_writes_nothing_to_disk` (mirror the ZIP/7z tests) — RAM-only holds.
- **Bomb/hostile:** oversized declared sizes refused (`MAX_ENTRY_BYTES`, `try_reserve`, window cap),
  corrupt CRC caught, no panics (`compcol` decode is wrapped by our `catch_panics` on the app side).

## Cross-links
- `102-tar-family-archives.md` — the tar work + the full compcol spike findings this builds on.
- `~/code/compcol-rar-corpus/` — RAR5 (+ RAR4) corpus, differential harness, `RAR4-CORPUS.md`.
- `~/code/compcol-rar-corpus/RAR4-IMPLEMENTATION-PLAN.md` — the **separate, upstream** RAR4 decoder
  effort. Not part of this viewer task; RAR4 stays "detect + honest message" here until compcol grows it.
- Memory: `compcol-rar-evaluation` — audit verdicts, fork, PR state.

## Non-goals (v1)
RAR4 decode, encryption *decrypt*, recovery records, multi-volume, RAR3/2/1, extracting to disk
(RAM-only, same no-trace guarantee as ZIP/7z).
