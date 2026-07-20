# Task 125 — Split `app_core_impl.rs` into concern-scoped `impl AppCore` blocks

**Status:** **in progress** — rev 3 (Codex round 1 folded), re-measured at `facd7e5c` (2026-07-20) after #126. Step 1
(the `prefs` rehearsal + the verifier) has landed; see *Progress* below. **Scope gate:** this is audit finding #1 remediation
**(a) only** — see §2, which is the most important section in this document.

## Where this slots into the technical-debt audit

It is not a new idea; it is the audit's **top-ranked finding and its #1 recommended
action**, never filed as a task:

- Finding #1, *"the god-objects + the half-finished NS0 inversion (the hub)"* — **note the
  audit's label is wrong, see §2a** — remediation (a): *"split the one impl into concern-scoped `impl AppCore` blocks across files
  (video / tree / overlay / prefetch) — Rust allows this for free; mechanical, low-risk,
  immediately shrinks the blast radius and conflict surface."*
- Recommended sequencing #1: *"Mechanical, low-risk, immediate relief to blast radius and
  merge conflicts. No behavior change."* Marked safe to do now, independent of everything
  else.

The audit references exactly one task (finding #5 → #121). Finding #1's remediation (a) was
never given an id, which is the likeliest reason it is the one recommendation that didn't
happen while #109/#119/#121 all did.

## 1. Why now: it is compounding faster than anything else in the repo

| commit | date | `app_core_impl.rs` |
|---|---:|---:|
| `8b5dc30b` (the audit's own baseline) | 2026-07-18 | 15,898 |
| `5f3b4429` (when this plan was filed) | 2026-07-19 | 20,932 |
| `facd7e5c` (re-measured, after #126) | 2026-07-20 | **22,105** |

**+5,034 lines (+32%) in about a day.** The audit flagged this file as its #1 debt and
warned its numbers would drift; they drifted by a third within 24 hours. Its stated figures
(16,013 LOC, 323 methods, `mod tests` at 10011) are all now wrong in the same direction.

Current, re-measured at `HEAD`:

- **22,105 lines** — 12,504 production, 9,602 tests
- **363 production methods**, essentially one flat `impl AppCore`
- next-largest file in the repo is `gpu.rs` at 7,127, so this is **~3.1× the runner-up**

#126 added ~1,170 lines to it, exactly as §11 predicted — which is the argument for doing
#126 first (its new lifecycle code gets sorted into the right cluster once, not twice), and
also why the figures below are re-measured rather than quoted from the filing.

Three concrete costs paid during task #124 alone, all attributable to file size:

1. Two full Codex review runs produced **nothing** — both exhausted their budget exploring
   the file. A review was only obtained by manually extracting excerpts into the prompt.
2. A patch landed in the **wrong function** first (`rebuild_playlist` instead of
   `rebuild_ring`) — plausible-looking neighbours in a 349-entry method list.
3. "What else touches this?" is not answerable by reading a file, so every change needs a
   repo-wide grep to be safe.

## 2a. ⚠ Correction to the audit: NS0 is **not** half-finished (owner, 2026-07-19)

The audit calls finding #1 *"the half-finished NS0 inversion."* That label is wrong and
should be fixed at the source. Re-measured at `HEAD`:

- `pb-app-core` is **50,337 LOC** — the core owns the overwhelming bulk of behavior.
- In `pb-mac-ffi/src/lib.rs` (6,393 lines) the genuine FFI surface starts at **line 4,390**.
- The contract carries **102 `CoreEffect` variants**.
- macOS ships a native SwiftUI/AppKit host over that core, which is the inversion's whole
  point and could not exist if it hadn't happened.

**NS0 shipped.** What remains is a single bounded residue: **16 orchestration functions
duplicated across the two shells**, plus `struct DirScan` and `struct ArchiveLoad` defined
in both —

```
begin_dir_scan  poll_dir_scan  cancel_dir_scan  cancel_scan_command  scan_pill_visible
begin_archive_open  poll_archive_load  finish_archive_open  fail_archive_open
cancel_archive_load  prompt_archive_password  is_archive
apply_menu_state  confirm_delete_permanent  toggle_recursive  toggle_show_archives
```

Every one is **async worker orchestration for directory scan and archive open** — the two
flows that straddle the boundary because they own *shell-side* resources (worker threads,
channels, modal dialogs) while their state belongs to the core. That is a reason they were
left, not an oversight.

This matters beyond pedantry: "half-finished inversion" reads as *the big refactor was
abandoned midway* and makes (b) sound like a large program, when it is a nameable de-dup of
~16 functions and 2 structs. Audit findings #1 (title) and #2 (which calls this *"the single
worst cross-platform liability"* and sizes it at ~2,870 lines) should be re-scoped
accordingly — the 41 "mirror" comments in `pb-mac-ffi` remain accurate, but they cluster on
these two flows rather than spreading across the shell.

---

## 2. ⚠ Scope gate — (a) not (b)

The audit deliberately splits finding #1 into two remediations. **They must stay separate
tasks**, and this plan is strictly the first:

| | what | risk | this task? |
|---|---|---|---|
| **(a)** | move methods into concern-scoped `impl AppCore` blocks in new files | **none if done as a pure move** (§3) | ✅ **yes** |
| (b) | de-duplicate the remaining shell-worker orchestration (scan + archive open); give the mirror flags one owner | design change, cross-crate, dissolves findings #1/#2/#6 | ❌ separate task |
| (c) | end the ~165-`pub`-field sprawl via accessors / `#[non_exhaustive]` | needs (b) first (shells build it as a literal) | ❌ later |

Bundling (a) with (b) is the main way this goes wrong: it converts a provably-safe file move
into a behavioral refactor, and the safety property in §3 evaporates. **No field moves, no
signature changes, no visibility changes, no `AppCore` struct changes, no call-site edits.**
If a method looks wrong while moving it, note it and move it unchanged.

## 3. The conservation check (what makes this reviewable)

> **Renamed from "the safety property" after Codex round 1 (2026-07-20), which was right that
> the old wording overclaimed.** The check verifies *textual conservation of function items*,
> **not behavioural equivalence**. Identical text can behave differently in a new module. What
> it cannot see is enumerated in `scripts/verify-pure-move.py`'s docstring and summarised in
> §3a. Do not describe a passing run as "proven correct" — describe it as "nothing was dropped,
> invented or edited."


Rust permits multiple `impl AppCore` blocks across files in one crate. So every method can
move with **zero** changes to call sites, types, or visibility.

That means this refactor can be **provably** behavior-preserving, not merely
tested-and-hopefully-fine. Verify mechanically:

`scripts/verify-pure-move.py` compares the multiset of `(fn name, item hash)` across every
`.rs` in the crate, where the hash covers **attributes + signature + body**. So visibility,
generics, `async`/`unsafe`/`const`, parameter and return types, and `#[cfg]`/`#[inline]`/
`#[track_caller]` changes all fail the check — not just body edits.

`cargo test` is the backstop, not the check: a passing suite would not notice a silently
dropped method that nothing covers, and this would.

**Corollary: a non-empty diff is a bug, not a judgement call.**

### 3a. What the check does NOT cover (Codex round 1)

Accepted in full. These need review by other means:

- **Imports and scope.** An unchanged `foo()` or `.method()` can resolve to a *different*
  function, trait method, const or macro in the destination module. `app_core_impl.rs` carries
  a glob `use crate::engine::*`, so this is live, not theoretical. **Mitigation:** each moved
  module uses `use super::*;` so it inherits the parent's scope verbatim; any *narrowing* of
  imports is a separate, reviewed step — never bundled into a move.
- **Module-sensitive macros** — `file!()`, `line!()`, `column!()`, `module_path!()`, relative
  `include_str!`/`include_bytes!`. Grep each cluster for these before moving it.
- **Same-name swaps.** Records are keyed by unqualified name, so two same-named functions in
  different modules could in principle exchange bodies invisibly. The report prints the
  per-name file map to make relocations human-visible; a stricter fix is the manifest in §3b.
- **Non-function items** — structs, consts, statics, type aliases, `mod` declarations, macros,
  trait/impl headers. Untracked.
- **Impl target.** It cannot tell a method moved between impls for *different types*. #125
  moves only within `impl AppCore`.
- **Config and codegen** — nothing about per-platform `#[cfg]` resolution, inlining or
  performance.

### 3b. The verifier is itself tested

The whole argument rests on the tool, so `verify-pure-move.py selftest` covers the cases that
have actually bitten: array return types, bodyless trait declarations, braces in strings and
raw strings, `fn ` inside comments, lifetimes vs char literals, nested fns, and sensitivity to
an added attribute or a visibility change.

⚠ **The first version had a real false negative, found by Codex, not by me.** It treated any
`;` before the opening `{` as a bodyless declaration — so `fn effective_letterbox() -> [u8; 3]`
was **untracked and could have been dropped or edited while the tool reported success.** Five
functions in `app_core_impl.rs` alone were invisible (772 tracked, 777 actual). Fixed by
matching the terminator at bracket depth 0, and pinned by a self-test.

**Deferred, not rejected** (Codex's stronger gate): parse with `syn` rather than by hand, and
keep an explicit source→destination manifest so same-name swaps are impossible. Worth doing if
a cluster move ever looks ambiguous; not worth it up front for a check that is one input among
`cargo test`, clippy and a run.

## 3c. ⚠ Private methods break when they move (Codex round 1) — the next step hits this

A private `fn` moved from `app_core_impl.rs` into `app_core_impl/<cluster>.rs` becomes private
**to that child module**, and the parent can no longer call it. This did not bite the `prefs`
rehearsal only because all four of its methods were `pub`. It will bite the very next cluster:

- `handle` calls private `apply_scan_batch` and `apply_archive`.
- `tick` calls private tree helpers such as `drive_fs_tree`.

Two clean options, per cluster:

1. **Leave cross-concern entry points in the parent.** Right when the method is genuinely
   dispatch glue rather than part of the concern.
2. **Change the moved method to `pub(super)`.** The spelling changes but the effective
   visibility region does not: private-in-parent and `pub(super)`-in-child both mean "visible
   in the parent module and its descendants".

⚠ Option 2 **is** an edit, so the conservation check will flag it — correctly. Do not suppress
it: make the visibility change its own clearly-labelled commit, separate from the move, so each
commit is either a pure move (check passes) or a reviewed edit (check is expected to fail).
Mixing them is how a real change hides inside a move.

## 4. Cluster inventory (measured at `HEAD`, not guessed)

349 production methods, auto-clustered by name, with the LOC each would take along:

| cluster | fns | ~LOC | note |
|---|---:|---:|---|
| video / audio / playback / poster / subtitle | 96 | 2,882 | biggest; already has siblings (`video_session.rs`, `video_native.rs`, `subtitle.rs`) |
| **residency / decode / present / ring** | 44 | 2,388 | **the hot engine — genuinely coupled, moves LAST or not at all** |
| *(unassigned — needs manual triage)* | 59 | 1,878 | §5 |
| lifecycle / dispatch (`tick`, `dispatch_action`, `handle`) | 10 | 1,101 | `tick` is 487 lines by itself |
| panels / overlay / hud / toast | 40 | 891 | |
| tree / fs / nav / playlist | 38 | 856 | |
| file ops / undo / delete / copy | 15 | 581 | |
| view / geometry / zoom / compare | 22 | 568 | touched by #124 |
| scan / describe / text | 13 | 395 | |
| settings / keymap | 5 | 185 | |
| archive / door | 7 | 129 | |

The eight leaf clusters (everything except residency, dispatch and unassigned) are
**~6,487 LOC ≈ 54% of production** — and their tests travel with them, which is where the
bulk of the 8,970 test lines goes. Extracting only the leaves takes the file from 20,932 to
roughly 6–8k without touching the hot path once.

## 5. The unassigned 59

Auto-clustering leaves 59 methods (1,878 LOC) unmatched — e.g. `open_plan`, `advance`,
`trim_caches`, `work_pending`, `enter_empty_state`, `refresh_after_geometry_change`,
`apply_stream_msg`, `menu_state_from`, `windowed_restore`, `index_of_path`. These need a
human read, and they are the interesting ones: a method that resists naming into a concern
is either genuinely cross-cutting (keep central) or badly named (note it, move it unchanged,
file a rename separately). **Do not rename anything during the move** — a rename breaks the
§3 body-diff proof.

## 6. Proposed file map

New files in `crates/pb-app-core/src/core/`, each a single `impl AppCore` block plus its own
`mod tests`:

```
core/video.rs      core/panels.rs     core/tree.rs      core/view.rs
core/files.rs      core/scan.rs       core/archive.rs   core/prefs.rs
```

`app_core_impl.rs` keeps lifecycle/dispatch, the residency engine, and the triaged
cross-cutting remainder. Naming note: `pb-app-core` already has top-level `scan.rs`,
`video.rs`, `subtitle.rs` etc. as *logic* modules — hence the `core/` subdirectory for the
*orchestration* halves, so `core/video.rs` (AppCore's video methods) is visibly distinct from
`video.rs` (the video logic). Confirm that reads right before committing to it.

## 7. Sequencing

Leaf-first, smallest-blast-radius-first, one cluster per commit:

1. `prefs` (5 fns / 185 LOC) — the smallest possible end-to-end rehearsal of the whole
   process, including the §3 verifier. If anything about the mechanism is wrong, learn it here.
2. `archive` (7 / 129), `scan` (13 / 395), `view` (22 / 568) — still small, still leafy.
3. `files` (15 / 581), `tree` (38 / 856), `panels` (40 / 891).
4. `video` (96 / 2,882) — the big win, deliberately after the process is proven six times.
5. **Stop and reassess.** At this point the file is ~6–8k and the remaining content is
   dispatch + residency + the triaged remainder. Splitting the residency engine is a
   different (and more debatable) question; it is genuinely coupled and it is the hot path.
   Do not pre-commit to it here.

`tick` (487 lines) and `dispatch_action` are **not** decomposed by this task. Cutting them up
is a behavior-risk change to per-frame ordering; file it separately if wanted.

## 8. Merge-conflict strategy (this is the real risk, not correctness)

The owner edits this tree concurrently, and this file is the repo's #1 churn file. A
large move will conflict with anything in flight.

- **Land each step in its own commit, same day it is written.** Do not batch.
- **Check with the owner that nothing is mid-flight in a cluster before moving it.**
- **Never `git add -A`** (standing rule) — stage the explicit paths.
- If a conflict does occur, resolving it is unusually safe here *because* of §3: the body
  diff tells you mechanically whether the resolution preserved every method.

## 9. Anti-regrowth: structural, NOT a line-count guard (owner call, 2026-07-20)

An earlier draft proposed a test failing if any file in `pb-app-core/src` exceeded ~1.3x the
largest post-split file. **Rejected by the owner, and rightly.** A threshold lint measures the
symptom: the number is arbitrary, it fires on a legitimately large cluster as readily as on
sprawl, and the cheapest response when it fires is to raise the number — which trains everyone
to route around it.

**The mechanism is the structure itself: give every likely place for growth somewhere proper
to live, and it will not pile up in one file.**

The diagnosis supports this. `app_core_impl.rs` grew ~1,170 lines during #126 *even though*
that task created three new modules (`background.rs`, `dir_scan.rs`, `archive_open.rs`). The
**logic** had a home; the `impl AppCore` **methods** did not, so `arm_dir_scan`, `poll_dir_scan`
and `cancel_dir_scan` landed in the big file right next to their own module. Growth was not
carelessness — "where does an `AppCore` method go?" had exactly one answer. Once
`app_core_impl/dir_scan.rs` exists beside `dir_scan.rs`, it has a better one.

### The condition this depends on — do not skip it

**The split must leave no "misc" bucket.** If `app_core_impl.rs` ends at ~6-8k as "lifecycle +
dispatch + residency + the ~60 I never sorted", it is still the default destination, just with
a smaller line count. The attractor survives the refactor.

So the deliverable is not "the moves are done" but: **the remainder has a stated charter** —
one sentence naming what belongs in it — so a method that does not fit that sentence visibly
needs a home somewhere else. That is what converts "where does this go?" from a default into
an answerable question.

This raises the stakes on the §5 triage: the unassigned methods are the difference between a
charter and a remainder, not bookkeeping.

### The lightweight complement (not a test)

When the split settles, record the convention where the crate's guidance already lives
(`crates/pb-app-core/CLAUDE.md`): which concern owns which file, and that a new `AppCore`
method goes beside its logic module rather than into `app_core_impl.rs`. Documentation of a
structure that exists, not a nag about one that does not.

## 10. Risks

| risk | severity | mitigation |
|---|---|---|
| Merge conflicts with concurrent owner edits | **the top risk** | §8: one cluster per commit, check before moving, land same-day |
| Scope creep into NS0 (b) or renames | high — destroys the §3 proof | §2 scope gate; move unchanged, file follow-ups separately |
| A method silently dropped in a move | high | §3 normalized body diff; `cargo test` as backstop |
| Splitting the residency engine on momentum | high — hot path | §7 step 5 is an explicit stop-and-reassess, not a plan to continue |
| Anchor churn in plans/memory (`app_core_impl.rs:NNNN` everywhere) | medium, unavoidable | accept; the audit's own anchors are already stale, and line anchors were never stable in a file growing 5k/day |
| `core/` naming collides confusingly with existing logic modules | low | §6; confirm before step 1 |

## 11. What this does NOT fix

Worth stating plainly so the task isn't oversold. After a perfect execution:

- `AppCore` still has ~165 `pub` fields, and any method in any new file can still touch any
  of them. **The blast radius is unchanged in principle** — only navigability, review size,
  and merge-conflict surface improve.
- The mirror-flag desync bug class (audit #1's actual hazard) is **untouched**; that is (b).
- The two-shell duplication (#2) is untouched; that is also (b) — and per §2a that is ~16
  functions across the scan and archive-open flows, not a rewrite.

**Codex round 1 put it plainly, and it belongs here:** *"this splits the source file, not the
god object. `AppCore` retains the same state, coupling, and privilege to touch every field. It
will materially improve navigation, reviewability, and merge conflicts, which is worthwhile.
The later work — private fields, narrower component APIs, and extracting owned stateful
subsystems — is what will actually dismantle the god class."* That is exactly the scope gate in
§2, from an independent reading.

This task buys tractability, and it makes (b) approachable. It is not itself the cure the
audit is pointing at. The audit calls its item 3 *"the large, high-value program the other
findings mostly reduce to"* — but per §2a that framing over-sizes what is actually left.

---

## Progress

### Step 1 — DONE 2026-07-20: the verifier, and the `prefs` rehearsal

**`scripts/verify-pure-move.py`** implements §3's safety property. Snapshot the crate, move
code, check: it compares the multiset of `(fn name, body hash)` across every `.rs` in the
crate, so a dropped, invented or edited body fails loudly regardless of which file it lives in.

Byte-level rather than AST — no `syn` dependency, and stricter. It brace-matches while
skipping string/char/raw-string literals and comments, because `println!("{}")` alone derails
a naive matcher.

**The verifier is itself tested**, since the whole safety argument rests on it:

| self-check | result |
|---|---|
| unchanged tree | ✅ verifies |
| one function deleted | ✅ `MISSING/CHANGED request_cancel` |
| one body edited (a comment added) | ✅ same name, two different hashes |

Its limits are documented in the script and are real: it compares **functions only**, so
`use` lines, structs, constants and attributes still need review; and it cannot detect a
method moved between impl blocks for *different types* (out of scope here — #125 moves only
within `impl AppCore`).

**Layout decision (§6 was left open).** Not the proposed `core/` subdirectory: `dir_scan.rs`,
`archive_open.rs` and `background.rs` are orchestration too and already sit top-level, so that
distinction was dead on arrival. Instead Rust's `foo.rs` + `foo/` pairing —
`app_core_impl.rs` keeps its name and gains `app_core_impl/prefs.rs` beside it. Zero renames,
no new concept, and the ownership is obvious from the path.

**The rehearsal:** `apply_keymap`, `refresh_theme`, `apply_settings`, `keymap_shortcut` →
`app_core_impl/prefs.rs` (161 lines). Compiled first try; verifier reports **2039 → 2039
functions, all byte-identical**; re-verified *after* `cargo fmt` to prove formatting did not
reflow anything; clippy clean; workspace green.

### Finding: the cluster inventory has false positives

`rebind_same_item` was auto-clustered into *prefs* because its name contains `bind`. It is
residency code from #124. So the §4 table is a **starting point, not an assignment** — its
per-cluster LOC figures are approximate and each cluster needs the human read that subtask 2
already calls for. The prefs cluster was 5 by the script and 4 in reality.

### Next

1. **Subtask 2 — triage the ~60 unassigned methods**, and re-check the auto-assignments for
   more false positives of the `rebind_same_item` kind. Output is a written assignment list.
2. Then the small leaves (`archive`, `scan`, `view`), then the mid ones, then `video`.
3. §9 is settled: anti-regrowth is structural, not a guard test (owner, 2026-07-20). The
   thing that makes it work is that the remainder gets a CHARTER, not leftovers — see §9.
