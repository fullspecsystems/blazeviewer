# Task 128 — Migrate the `app_core_impl` tests into their concern modules

**Status:** **COMPLETE** — 2026-07-20. All 21 leaf concerns now hold their own tests;
`app_core_impl.rs` is **14,231 → 9,049** lines (its `mod tests` **9,613 → 4,370**, the 132 charter
tests that belong there). Full workspace clippy `-D warnings` clean, `cargo test --workspace` green,
ship build passes. Direct completion of **#125's original intent** (§4/§6): #125 moved the *methods* and
left every test behind; this moved them home.

## 1. What #125 actually left

#125 took `app_core_impl.rs` from 22,105 → 14,231 lines, but that headline hides the split:

| | before #125 | after #125 (now) |
|---|---:|---:|
| production (the `impl AppCore` methods) | 12,504 | **4,618** |
| `mod tests` (one flat module) | 9,602 | **9,613** — untouched |
| total | 22,105 | 14,231 |

The production code — what you navigate, review, and merge — dropped **63%**, to the charter floor
(lifecycle, dispatch, the residency & present engine). But **all 296 tests / 9,613 lines still sit in
one `mod tests` at the bottom of the parent**, and every one of the 26 concern files has **zero** tests.
So `video.rs` holds the video methods and none of the video tests; the tests are 9k lines away in a
different file. That is the same navigability problem the split was meant to cure, still live for tests.

**Goal:** each concern file gains its own `#[cfg(test)] mod tests` holding the tests for its methods.
`app_core_impl.rs` drops to roughly **~8k** (its ~4.6k charter production + only the charter's own tests),
and every concern file becomes genuinely self-contained.

**Non-goal / honest framing:** this does not delete a single line — tests *relocate*. Total repo LOC is
unchanged. The win is per-file: the parent shrinks ~14.2k → ~8k and each concern gets its tests beside its
code. Do not sell it as a line-count reduction.

## 2. Scope gate (same discipline as #125)

- **No test logic changes.** No added, removed, renamed, `#[ignore]`d, or edited tests. A test moves
  byte-identically or it does not move.
- **No production changes** except `pub(super)` visibility widenings that a *moved test* forces — each its
  own labelled commit, exactly as in #125 §3c.
- **The charter's own tests stay put.** Tests of the residency/present engine, `tick`, `dispatch_action`,
  deck ingestion, and the contract remain in the parent's `mod tests`. This task moves only the tests of
  already-moved concerns.
- **Not the engine split.** #125 §7 step 5 still holds; nothing here touches the residency engine or argues
  to split it.

## 3. Why this is NOT "#125 part 2" — three ways tests are harder

The production moves were near-mechanical because the file was *already ordered* by concern. Tests are not,
and two other properties make them more entangled. The plan exists to handle exactly these.

### 3a. Tests do not cluster by concern

Sampled in file order, consecutive tests jump across subtitle → tracks → launch → info-line → describe →
open → dialog → video → archive → residency → scan → password → stash → nav → watchdog → stream → contract.
There *is* some locality (the video tests do group ~L7500–8900), but there is no clean contiguous span per
concern the way there was for methods. **Consequence: assigning the 296 tests to concerns is real
classification work, done by reading each test, not by cutting a range.** `scratchpad/extract.py` cannot be
pointed at a line span; it must be given an explicit list of test-fn names per concern.

### 3b. Shared fixtures can't move to one concern

38 non-`#[test]` helper fns live in `mod tests`. Usage is wildly uneven:

| tier | fixtures | rule |
|---|---|---|
| **cross-concern** (must be shared) | `test_core` (204 uses), `photos_named` (59), `make_resident` (33), `five_photos` (19), `rgba_full` (15), `track` (27), `seed_details` (10), `meta_dims` (4), `captured_img` (3) | → a shared `test_support` module (§4) |
| **concern-local** (move with their concern) | `zoom_test_core`, `thumb_test_core`/`tiny_thumb`, `archive_core`/`armed_archive_core`, `armed_scan_core`, `compare_core`, `stash_test_core`, `stuck_preview_core`, `core_with_a_native_video`/`core_with_a_playing_video`, `core_with_audio_tracks`/`selected_audio_rows`/`labels`, `core_with_subtitle_tracks`/`sub`, `text_result`/`clipboard_text_effects`, `feed_scan`/`feed_describe`, `seed_details`/`seeded_rows`, `poster_payload`, `settle_at`, `inject_stream`/`stream_header`/`stream_frame`, `fake_probe`, `deck_names`, `core_on_a_door` | → move into the owning concern's `mod tests` |

The tier boundary is "used by more than one concern's tests", and it needs verifying per fixture during
triage — a fixture that *looks* concern-local but is called from two concerns is promoted to `test_support`.

### 3c. The conservation check is WEAKER for tests

`verify-pure-move.py` keys on the *unqualified* fn name. Test modules already contain same-name items —
`stream_frame` is both a production method and a test helper (#125 trap 10) — and helper names like `sub`,
`track`, `labels` are generic enough to recur. Two same-named fns already read as a multiset of 2; moving one
does not change the multiset, so the byte-hash **cannot prove a test was not swapped or dropped** the way it
could for uniquely-named production methods.

**So this task adds a second, stronger anchor that the byte-hash lacks: the test-name set.**

## 4. The safety model — test-name-set conservation is the primary check

`cargo test -p pb-app-core --lib -- --list` emits every test's fully-qualified path. The **module path
changes** on a move (`app_core_impl::tests::foo` → `app_core_impl::video::tests::foo`) — that is the point —
so the invariant is over the **leaf name**, and Codex was right that it must be a **multiset, after asserting
uniqueness**, not a set (a set silently absorbs a dropped duplicate):

```sh
snap() { cargo test -p pb-app-core --lib -- --list 2>/dev/null | grep ': test$' \
         | sed 's|.*::||' | sort; }
snap > /tmp/tests_before.txt          # before the move
# assert leaf names are unique to begin with, else the multiset argument is unsound:
test "$(sort -u /tmp/tests_before.txt | wc -l)" = "$(wc -l < /tmp/tests_before.txt)" \
  || echo "⚠ DUPLICATE leaf names exist — switch to full-path diff for the dupes"
# ...move one concern's tests...
snap > /tmp/tests_after.txt
diff /tmp/tests_before.txt /tmp/tests_after.txt      # sorted multiset, must be empty
```

A clean multiset diff (with uniqueness asserted) proves nothing was **dropped, renamed, or duplicated**.
But `--list` does **not** distinguish an ignored test — Codex corrected a wrong claim in an earlier draft:
`#[ignore]` tests still appear in `--list`. So `#[ignore]` gets its own anchor:

```sh
cargo test -p pb-app-core --lib -- --ignored --list 2>/dev/null | grep ': test$' | wc -l  # must not change
```

Layered on top:

1. **`verify-pure-move.py`** still runs. It hashes **attributes + signature + body**, so it *does* catch a
   changed `#[should_panic]` / `#[should_panic(expected=…)]` / `#[ignore]` on a moved test — the one thing
   the name-set is blind to (Codex's point 2.4). It cannot catch a same-name *swap* (its weakness here), which
   is why the name-set is primary and this is the belt.
2. **The suite passes with the same passed AND ignored counts** — `cargo test -p pb-app-core` before/after
   must report identical `N passed; M ignored`. The pair pins what either number alone would miss.
3. **Run the feature matrix, not just the default build** (Codex's highest-ranked risk, 2.1). A test gated on
   a `cfg`/feature/target absent from the default `--list` is invisible to every check above. So capture the
   snapshots under the **ship features too**, and for the video/animation concerns specifically:
   ```sh
   cargo test -p pb-app-core --lib --features libheif,dav1d,ffprobe -- --list   # and the macos-gated set is a Mac check (see Handoff)
   ```
   The macOS-only test paths (`cfg(target_os="macos")`) cannot be listed from Windows at all — that is a Mac
   verification item, exactly as #125's macOS-gated *methods* were.
4. **`cargo test`, not `cargo check`/`clippy`, is mandatory.** `mod tests` is `#[cfg(test)]`, so a broken test
   module produces **zero** errors under `cargo check`/`clippy`/`cargo build`. This is #125 trap 5 in a
   sharper form: here the *entire moved unit* is cfg(test), not just a stray caller. Every step ends with a
   real `cargo test -p pb-app-core`.

**Corollary:** a non-empty multiset diff (or a changed passed/ignored count) is a bug, not a judgement call.

## 5. The shared test-support module

Create `crates/pb-app-core/src/app_core_impl/test_support.rs`, declared in the parent as an **outer** attribute
(Codex 3 — not an inner `#![cfg(test)]` inside the file):

```rust
// in app_core_impl.rs:
#[cfg(test)]
mod test_support;

// in app_core_impl/test_support.rs:
//! Shared test fixtures for the app_core_impl concern test modules (task #128).
use super::*;

pub(super) fn test_core() -> AppCore { ... }       // pub(super) == pub(in app_core_impl),
pub(super) fn photos_named(...) -> ... { ... }      // clearer and less path-dependent
// ...the §3b tier-1 fixtures, moved verbatim
```

`pub(super)` from inside `test_support` *is* `pub(in crate::app_core_impl)` — same region, clearer spelling
(Codex 3). It keeps the fixtures exactly as reachable as they are today (private-to-`mod tests` → shared
across the `app_core_impl` subtree) without going crate-wide.

⚠ **Import the fixtures EXPLICITLY and absolutely, never by glob** (Codex 3 + 4 — the single most important
change from the first draft). Each concern's `mod tests` does:

```rust
#[cfg(test)]
mod tests {
    use super::*;                                                  // the concern's own scope
    use crate::app_core_impl::test_support::{test_core, photos_named /* only what's used */};
    // ...tests...
}
```

**not** `use super::super::test_support::*`. Two reasons: the glob is nesting-sensitive (breaks if the module
depth ever changes), and — the real hazard — a glob can silently rebind a generic helper name (`track`, `sub`,
`stream_frame`) to a different item than the test meant, compiling green while testing the wrong thing. An
explicit absolute import cannot be shadowed and names exactly one item. This is §4's byte-hash blind spot
closed by construction rather than by review.

⚠ **The fixtures move too, so they are verified moves like any other.** They are non-`#[test]` fns, so they
ride `verify-pure-move`'s byte-hash but NOT the test-name-set check — read their diffs by hand, as with the
#125 `pub(super)` commits.

⚠ **Fixture → concern coupling.** A fixture may itself call a `pub(super)` concern method (e.g. `make_resident`
touching residency helpers, `armed_scan_core` calling `arm_dir_scan`). Since `test_support` is a child of
`app_core_impl`, it sees every `pub(super)` method already — no new widening for the fixtures themselves.
Verify this holds during step 0 rather than assuming it.

### 5a. The per-test audit that closes the biggest hole (Codex 4)

The one silent failure neither the name-set diff, the byte-hash, nor a green suite catches: a **moved test
body whose text is unchanged but now resolves a bare name to a different item** — `super::track(...)` meaning
`app_core_impl::track` before and `app_core_impl::video::track` after, or an unqualified helper/const/trait
method rebinding. So every moved test is grepped, before the move, for the tokens that make relocation
scope-sensitive:

```sh
# over the block of tests being moved:
grep -nE '(^|[^:_a-zA-Z])(super|self)::|::\*;' <block>
```

Any `super::` / `self::` / glob in a moving test is qualified to an absolute `crate::…` path **as its own
reviewed edit** (a real change, its own commit — never bundled into the move), or the test stays put. In
practice most tests call `core.method()` and `test_core()`; the qualified-path fixups should be few, but they
are the difference between "relocated" and "still testing the same thing."

## 6. The pub(super) surface for moved tests

A test that moves into `concern::tests` and calls a **private** method of a *different* concern, or a
**parent-private** (charter) method, loses access — same failure as #125 §3c, now with the *test* as the
mover. `scratchpad/privcheck.py` already scans `mod tests` and all siblings, so it applies directly: run it
per concern over the *methods the moving tests call*, land any widenings as their own labelled commits FIRST
(#125 trap 6), then move.

Expectation: **small.** Most tests drive the public `handle`/`tick`/`dispatch_action` surface and assert on
public state; the private calls are usually to the concern's *own* methods (which move with them) or to
already-`pub(super)` methods. But charter privates (`apply_scan_batch`, `capture_fit_stash`, the watchdog
helpers) are called by tests that will stay in the parent anyway — confirm, don't assume.

## 7. Classification method (the real work of step 1)

For each of the 296 tests, assign a home by this order (first match wins):

1. **The concern-local fixture it uses.** A test calling `zoom_test_core` is a view test; `archive_core` →
   archive; `core_with_a_native_video` → video; `compare_core` → compare. This resolves the majority cleanly.
2. **The method under test**, read from the test name and its assertions — `open_parent_climbs_one_level` →
   open; `info_line_and_inspector_are_independent` → panels; `scan_status_is_none_once_the_walk_ends` → scan
   (dir_scan). NOT "which methods it calls" — every test calls `tick`/`handle`; that is noise.
3. **Charter, stays in the parent**, if the subject is the residency/present engine, `tick` ordering, deck
   ingestion, or the contract — e.g. `an_inadmissible_original_want_is_never_emitted`,
   `parked_fulls_decode_nearest_first_after_a_forward_blaze`, `contract_debug_redacts_the_password`.

⚠ **Do not split a test across concerns.** A test that genuinely exercises two concerns goes to its *primary*
subject; note the secondary in the commit if it matters. Splitting or duplicating a test is a logic change
and out of scope.

The deliverable of step 1 is a written **test → concern** assignment list (like #125 subtask 2's), reviewed
before any move.

## 8. Sequencing

0. **`test_support.rs` first** — move the tier-1 shared fixtures (§5). Verify: byte-hash on the fixtures,
   and the suite still green (nothing calls them yet from a new place, so this is a pure relocation of the
   fixtures + updating the parent `mod tests` to `use` them from `test_support`). This is the riskiest single
   step because everything depends on it; do it alone.
1. **Rehearsal on a small, clean concern** — `slideshow` or `compare` (few tests, a concern-local fixture,
   little charter entanglement). Prove the whole loop end to end: name-set diff, byte-hash, suite green.
2. **The rest, one concern per commit**, pairing with the production file already there. Roughly smallest
   first; `video`/`panels`/`tree` (the big test populations) last.
3. **The parent's `mod tests` is what remains** — the charter tests. Confirm it is coherent as "tests of
   lifecycle, dispatch, and the residency engine", the test-side mirror of the production charter.

One concern per commit; land same day (merge-conflict discipline — the parent `mod tests` is huge and
churny). Stage explicit paths; never `git add -A`.

## 9. Risks

| risk | severity | mitigation |
|---|---|---|
| A test silently dropped / renamed / duplicated in a move | **high** | §4 leaf-name **multiset** diff (uniqueness asserted first) — primary check |
| A test silently `#[ignore]`d | high | §4 — `--list` does NOT flag ignored (Codex); separate `--ignored --list` count + the suite's `M ignored` count |
| `#[should_panic]` / `expected=` changed on a moved test | medium | §4 — `verify-pure-move` hashes attributes, so it catches this |
| A test gated by an inactive cfg/feature/target is invisible to every check | **high** (Codex's #1) | §4 — snapshot under the ship features; macOS-gated tests are a Mac item |
| Bare `super::`/`self::`/glob in a moved test rebinds silently (green but wrong) | **high** (Codex's #4) | §5a per-test audit + §5 explicit absolute fixture imports |
| Same-name test helper swapped invisibly (`stream_frame`, `sub`, `track`) | high | §4 multiset + read fixture diffs by hand; extractor bounded to the production impl (#125 trap 10) |
| `cargo check`/`clippy` green while the test build is broken | high | §4: every step ends with `cargo test -p pb-app-core`, never just check |
| A "shared" fixture actually only used by one concern (or vice-versa) | medium | §3b tier boundary is verified per fixture in triage, not guessed |
| Misclassifying a charter test into a concern (or vice-versa) | medium | §7 order: fixture → subject → charter; a wrong call is a cheap re-move, not a correctness bug |
| Merge conflict with concurrent edits to the giant `mod tests` | **the top risk** | §8 one concern/commit, same-day; the other agent (#127, error-handling) may touch decode tests — coordinate before moving `image_text`/decode-adjacent tests |
| pub(super) churn on charter privates | low | §6: expected small; privcheck.py already accounts for test callers |
| Fixture visibility too wide | low | `pub(in crate::app_core_impl)`, not `pub(crate)` — exactly today's reachable region |

## 10. What this does NOT do

- It does not reduce total repo LOC — tests relocate, not vanish.
- It does not touch the residency/present engine or reopen the #125 §7 stop.
- It does not change coupling — `AppCore` still has ~165 `pub` fields (that is #125's (b)/(c), still separate).
- It does not alter, add, or remove any test — a passing 296-before must be a passing 296-after with identical
  leaf names.

## 10a. Codex review (2026-07-20, folded into this plan)

Reviewed with the mechanism inlined (the only way Codex works on this repo). It confirmed the visibility model
(a sibling's private is NOT reachable — only `pub(super)` is; the plan already assumed this) and produced four
corrections, all folded in above:

1. **Name-set must be a multiset, uniqueness asserted first** — a set diff absorbs a dropped duplicate (§4).
2. **`--list` does not flag `#[ignore]`** — an earlier draft wrongly claimed an ignored test drops from the
   list. It does not; `#[ignore]` needs its own count anchor, and the pass/ignore count pair pins the rest (§4).
3. **Fixtures: outer `#[cfg(test)] mod test_support;`, `pub(super)` fns, and EXPLICIT absolute imports** — not
   an inner `#![cfg(test)]` and not `use …::*`. The explicit import is what makes the biggest hole unreachable
   rather than merely audited (§5).
4. **The biggest hole is silent lexical rebinding** of a byte-identical test body (`super::track` resolving into
   the new module). Closed two ways: explicit absolute fixture imports (§5) and a per-test grep for
   `super::`/`self::`/glob before each move (§5a).

The through-line of all four: for tests, **the byte-hash is necessary but not sufficient**, because test bodies
lean on generically-named, relocation-sensitive helpers. The name-set multiset + the config matrix + the per-test
scope audit are what make a move trustworthy.

## Progress

### Step 0 — DONE 2026-07-20: `test_support` module + baseline

Two commits (the #125 widen-then-move pattern applied to fixtures):
- `26f39b55` — the five cross-concern fixtures widened to `pub(super)` in place (keyword-only diff).
- `53d9cdb0` — moved to `app_core_impl/test_support.rs`. **Test-name multiset identical, 881 → 881**
  (the primary check, first real exercise); 878 passed + 3 ignored unchanged; clippy clean. The 5
  byte-hash flags are the unavoidable de-indent (nested module → module scope), hand-diffed identical.

**Baseline captured:** `scratchpad/tests_before.txt` — 881 unique leaf names (uniqueness holds, so the
multiset argument is sound). #127 landed first (recovery ladder for malformed images), adding 3 tests
(296 → 299 in `mod tests`) and correctly putting its new methods in `meta.rs`/`panels.rs`.

**Two wiring facts for the next migrator:**
- The parent `mod` block is not alphabetical past `…undo, video, view` — match real neighbours.
- `test_support` needed `use crate::Viewport` — `use super::*` gives production scope only, not the
  test-only prelude (`use crate::{PbKey, Viewport}`, contract/animation/dir_scan/archive_open imports)
  that lived inside `mod tests`. Each concern's `mod tests` will need its slice of that old prelude.

### ⚠ Finding: the fixture tiers need per-fixture verification (as §3b warned)

`settle_at` was tentatively mapped to *video* in the first classification pass. It is not — it is a
**general navigation helper** (`jump_to` + set `target_item`/`displayed_item`), used by the *compare*
tests too. So it is **cross-concern → `test_support`**, not video-local. Confirmed by reading its body,
not its name. The lesson the plan already stated, now with a concrete instance: **verify each "local"
fixture against its actual callers before moving it with one concern** — a mis-tier strands another
concern's tests.

Known-shared so far (→ `test_support`): the 5 already moved, **plus `settle_at`** (confirmed), plus the
heavy hitters still to promote as their first consumer migrates: `photos_named`✓, `track` (27 uses,
tier unverified), `make_resident`✓. Concern-local confirmed: `compare_core` (compare), `zoom_test_core`
(view), `thumb_test_core`/`tiny_thumb` (thumbs), `archive_core`/`armed_archive_core` (archive),
`core_with_a_native_video`/`core_with_a_playing_video` (video).

### Classification status (step 1 deliverable, in progress)

The fixture pass (`scratchpad/fixture_spread.txt`) resolves **155 of 299** tests to a concern by the
concern-local fixture they use. **144 remain** — they use only shared fixtures or none, so they need
name/subject reading (§7 rule 2). That 144-test read is the next work unit and the real bulk of the
task; it was deliberately not rushed at the end of a long session.

**The `compare` concern is fully scoped and ready to be the step-1 rehearsal:** 9 tests
(`sibling_results_are_stale_guarded…`, `compare_toggle_pins_first_then_flips…`, `compare_pin_moves_and_unpins`,
`compare_flip_never_interrupts…`, `compare_pin_survives_a_same_deck_rebuild…`,
`compare_pin_rides_the_prefetch_want_list…`, `deleting_down_to_the_empty_state_clears_the_pin`,
`compare_carry_applies_only_to_matching_geometry`, `compare_carry_is_staged_for_the_flips_first_frame…`),
the `compare_core` fixture (concern-local, moves with them), and a prerequisite: **promote `settle_at`
to `test_support` first**. `compare_identity`/`compare_carry_view` are already `pub(super)` from #125.

### Next

1. Promote `settle_at` (and re-verify `track`'s tier) into `test_support`.
2. Rehearse the full concern-test loop on `compare` — this is what exercises the name-set *path change*
   and the §5a per-test scope audit, which step 0 (fixtures only) did not.
3. Do the 144-test name/subject classification → the written assignment list.
4. Then concern-by-concern, smallest first, `video`/`panels`/`tree` last.

## Handoff

**Verified (Windows):** step 0's two commits — name-set 881 → 881 identical, suite 878+3 unchanged,
clippy clean, byte-hash hand-diffed. Everything pushed to `main`.

**Not verified / owed:** the macOS-gated *test* paths (`cfg(target_os="macos")`) can't be `--list`ed from
Windows, same as #125's macOS methods — a Mac must confirm the video/animation test moves when they happen.
None have moved yet, so nothing is owed there today.

**Claimed:** `app_core_impl.rs`'s `mod tests` is the migration target on **Windows**. It is huge and churny;
coordinate before editing it concurrently. #127 (error-handling) is the one other active toucher — it adds
tests near the decode/`present_failed` path; fold, don't race.

## Outcome (2026-07-20)

**Done: 21 concerns, ~167 tests relocated, `app_core_impl.rs` 14,231 → 9,049.** Each concern file
(`video.rs`, `panels.rs`, `tree.rs`, …) now ends with a `#[cfg(test)] mod tests` holding its tests;
the parent keeps the 132 charter tests (residency/present engine, dispatch, contract, deck ingestion) —
the test-side mirror of its production charter, exactly as intended.

**`test_support.rs`** holds the shared infrastructure: the cross-concern fixtures (`test_core`,
`photos_named`, `make_resident`, `five_photos`, `rgba_full`, `track`, `settle_at`→compare-local in the
end, and six more found by the tier audit), plus the shared **non-fn** test items §3a warned about —
the `FakeArchive`/`DeriveOk`/`StashOk` stub `ItemSource`/`Renderer`s and the `ARCHIVE` const, moved by
hand as `pub(super)`.

**The safety model held.** Every move: test-name **multiset identical (881→883** after #127 added two),
`ignored` count steady at 3, `verify-pure-move` byte-identical, suite green, clippy `-D warnings` clean.
Not one test was dropped, renamed, or `#[ignore]`d.

**What actually cost time — the honest tail** (all recorded so a future migration skips them):
- **Fixture tiers must be verified by call sites, not names.** Seven fixtures I guessed local were
  shared; `clipboard_text_effects` tripped it first, after which one wholesale caller-cross-reference
  pass settled every tier. `settle_at` went the other way — guessed shared (video), proved compare-local.
- **Non-fn items (structs, consts) are invisible to the fn machinery** and to `verify-pure-move`. Four
  surfaced (`FakeArchive`, `DeriveOk`, `StashOk`, `ARCHIVE`); each is a manual `pub(super)` move, and
  `StashOk` also needed its `#[derive(Default)]` carried along (the multi-attribute-capture rule again).
- **Import bookkeeping is the fiddly part, in both directions.** A moved concern can strand a now-unused
  import in the **parent** (`mpsc`, `Viewport`, `ScanDialogRequest`, `AnimStream/StreamMsg`) — production
  often uses these fully-qualified, so the bare `use` was test-only. And clippy's "unused" location
  (parent vs concern) must be read before deleting: twice I removed the wrong one and broke compilation.
  **Lesson: use `cargo fix --tests` for import cleanup, not regex pruning** — it compiles first, so it
  never removes a still-needed import. The last two concerns (archive_open, video) used it and were clean
  first try.
- A driver bug (`open(cf,"w").write(open(cf).read()…)` truncates before reading) destroyed a file's impl
  once — caught immediately by the compile, restored from git. Read-into-a-var-first.

**Cleanup pass — DONE 2026-07-20:** the ~132 remainder was re-classified and the 17 that still had a
concern signal (they drive via `handle`/`tick`, so they missed the fixture/method-call first pass) were
moved: 8 more video, 3 open, 2 panels, 2 item_kind, 1 audio_tracks, 1 nav. A fresh classification then
confirms **no strong concern-belongers remain** — the **115** parent tests left are all genuine charter
(residency/present engine, watchdog, selection, dispatch, contract, lifecycle), the test-side mirror of
the production charter. `app_core_impl.rs` is now **8,286 lines** (from 22,105).

The cleanup used `insert_tests.py` (into each concern's *existing* `mod tests`, since #128 already created
them) + `cargo fix --tests` for import pruning; the only manual step was adding the shared-fixture/symbol
imports the new tests needed (cargo fix removes, never adds). One reclassification: `frame_step_on_video`
went to video, not the animation `frame_step` it happens to call.

**Machinery** (scratchpad, not committed): `migrate_concern.py` (per-concern mover with the verified
tier maps + a §3b own-fixture guard), `movetests.py`, the classifier (`assign.json`), and the mvc
verify-loop. The keeper is still `scripts/verify-pure-move.py` + the test-name multiset check.

## 11. Interaction with #127 (error-handling, in flight on another machine)

#127 ("Lenient decode + recovery ladder for malformed images") is landing on `main` concurrently. It touches
`pb-decode` primarily, but may add tests near the decode/`load_current_sync`/`present_failed` path. **Fetch
before starting and again before each push** (#125's rule). If #127 has added decode-failure tests to
`app_core_impl`'s `mod tests`, fold them into the classification rather than racing them — and skip moving any
concern #127 is actively editing until it lands.
