# Blaze Viewer — Current Status (session handoff)

_Last updated: 2026-07-20 (rev 24). Session ran on **Windows**, with a macOS session working
the same tasks in parallel through the day. Everything below is on `main`, pushed._

---

# ▶️ START HERE

**#124 (zoom) and #126 (DRY the shells) are both COMPLETE.** **#125 (split
`app_core_impl.rs`) is IN PROGRESS — step 1 landed.** That is the live work.

New this session and worth reading before writing any code:
**`docs/where-code-goes.md`** — an ordered decision procedure for where a new function
belongs. **"Put it on `AppCore`" is the last answer, not the first.** Linked from
`CLAUDE.md` → *Working norms*.

---

# 🧱 #125 — split `app_core_impl.rs` (LIVE)

Plan: `.taskmaster/plans/125-split-app-core-impl.md` (rev 3, Codex round 1 folded).

### The shape of it, in five lines

- `app_core_impl.rs` is **22,105 lines** (12,504 production + 9,602 tests, **363 methods**),
  ~3.1× the next-largest file in the repo.
- Split it into concern-scoped **`impl AppCore` blocks** in `app_core_impl/<concern>.rs`.
  Rust lets an inherent impl span modules in one crate, so this is a **pure move**: no call
  site, type, signature or visibility changes.
- Verified by `scripts/verify-pure-move.py` — snapshot, move, check.
- **Leaf-first.** The residency/present engine and `tick`/`dispatch_action` move **last or
  not at all**.
- Scope gate: this is audit finding #1 remediation **(a) only**. It splits the *file*, not
  the god object. Do not drift into (b)/(c).

### How to work it

```sh
python scripts/verify-pure-move.py selftest                       # the tool's own tests
python scripts/verify-pure-move.py snapshot crates/pb-app-core/src > /tmp/before.json
# ...move one cluster...
python scripts/verify-pure-move.py check crates/pb-app-core/src /tmp/before.json
cargo fmt --all && cargo clippy --all-targets && cargo test --workspace
```

One cluster per commit. Land it the same day it is written (merge-conflict discipline, §8).
Stage explicit paths — **never `git add -A`**; the owner edits concurrently.

### ⚠ Four traps, all learned the hard way

1. **Private methods break when they move** (§3c). A private `fn` moved into
   `app_core_impl/<x>.rs` becomes private *to that child*, and the parent can no longer call
   it. `handle` calls private `apply_scan_batch`/`apply_archive`; `tick` calls private
   `drive_fs_tree`. Fix: leave cross-concern entry points in the parent, **or** make the moved
   method `pub(super)` (same effective visibility region). **`pub(super)` IS an edit** — give
   it its own labelled commit so a real change cannot hide inside a move. The `prefs`
   rehearsal dodged this only because all four of its methods were `pub`.
2. **The cluster table (§4) is a starting point, not an assignment.** `rebind_same_item` was
   auto-clustered into *prefs* because its name contains `bind` — it is residency code from
   #124. Re-read every auto-assignment; the LOC figures are approximate.
3. **The verifier proves textual conservation, NOT behavioural equivalence.** It cannot see
   scope/import resolution (`app_core_impl.rs` has a glob `use crate::engine::*`), module-
   sensitive macros (`file!`/`line!`/`module_path!`), same-name swaps, or non-function items.
   Each moved module uses `use super::*;` so it inherits the parent's scope verbatim — any
   *narrowing* of imports is a separate reviewed step. Never call a passing run "proven
   correct"; say "nothing was dropped, invented or edited".
4. **Anchors churn.** Every `app_core_impl.rs:NNNN` reference in plans/memory goes stale.
   Accepted and unavoidable.

### Done so far

- **`scripts/verify-pure-move.py`** — hashes **attributes + signature + body** per function,
  compares the multiset across the crate. Has a `selftest` (array returns, bodyless trait
  decls, braces in strings/raw strings, `fn ` in comments, lifetimes, nested fns, attribute
  and visibility sensitivity). ⚠ Its first version had a **false negative found by Codex, not
  by me**: any `;` before `{` read as a bodyless declaration, so `fn f() -> [u8; 3]` was
  untracked — 5 functions invisible in `app_core_impl.rs` alone. Fixed and pinned.
- **Layout decided:** *not* a `core/` subdirectory (`dir_scan.rs`, `archive_open.rs`,
  `background.rs` are orchestration too and already sit top-level). Rust's `foo.rs` + `foo/`
  pairing instead — `app_core_impl.rs` keeps its name and gains `app_core_impl/prefs.rs`.
- **Step 1 rehearsal:** `apply_keymap`, `refresh_theme`, `apply_settings`, `keymap_shortcut`
  → `app_core_impl/prefs.rs` (161 lines). Verified 2051 → 2051 items byte-identical, re-checked
  *after* `cargo fmt`.

### Next on #125

1. **Subtask 2 — the triage.** Read the ~60 unassigned methods AND re-check the auto-assigned
   ones. Per cluster ask two questions: **(a) subsystem or topic?** — a subsystem owns state
   and gets its own module; a topic is just related `AppCore` methods and gets an
   `app_core_impl/` file. **(b) which methods are private, and do they stay in the parent or
   become `pub(super)`?** Output is a written assignment list, not code.
2. Then the small leaves (`archive`, `scan`, `view`), the mid ones (`files`, `tree`,
   `panels`), then `video` (96 fns) last among the leaves.
3. **Stop and reassess** before residency. Do not pre-commit to moving it.

### The point of the whole thing (do not oversell it)

It buys navigability, review size and merge-conflict surface. It does **not** reduce coupling —
`AppCore` keeps all ~165 `pub` fields and every method's privilege to touch them. Codex said
this independently and it is recorded in §11. The *charter* is what makes it stick:

> **`app_core_impl.rs` holds the `AppCore` lifecycle, dispatch, and the residency & present
> engine. Nothing else.**

Anti-regrowth is **structural, not a lint** (owner call): the split must leave no "misc"
bucket, and `docs/where-code-goes.md`'s **two-halves rule** (a new subsystem module gets its
`app_core_impl/<name>.rs` in the same commit) is the mechanism.

---

# ✅ #126 — DRY the two shells (COMPLETE 2026-07-20)

Plan: `.taskmaster/plans/126-dry-the-shell-orchestration.md` (see its `## Outcome`).

Both shells' dir-scan and archive-open copies are gone (**~−800 lines across the two**),
replaced by one tested core implementation: `background.rs` (one generation space across both
operation kinds), `dir_scan.rs`, `archive_open.rs`. All six Windows verification items and the
macOS run passed. winit's dead Scanning dialog deleted (−208/+23).

**Five defects found along the way** (none were the point of the task): the empty-deck welcome
hint (both shells), the "Checking…" spinner rendering under the button bar on password retry,
the Opening-dialog boundary flash (gate 250 → **500 ms**), the wrong-password message that
never showed for anything routing through the Loading dialog, and cancel-with-a-proven-password
now promoting it to the session MRU (gated on `progress.done() > 0`).

⚠ **Do not "fix" the residual boundary flash with a minimum display duration** — considered and
rejected; the rationale is in `LOADING_DIALOG_DELAY`'s doc comment. Read it before changing
the value.

**Open, not part of the task:** the macOS untraced bottom-left spinner during a quick open
(needs a Mac); two cosmetic one-frame gaps (door card over a photo on archive entry — core
proven innocent over 923 frames; `archive_scope` lagging the deck by one frame); and the
**"strays"** (`apply_menu_state`, `confirm_delete_permanent`, `toggle_recursive`,
`toggle_show_archives`) — deliberately deferred out, subtask 3 is *cancelled* not skipped, and
they want their own task.

---

# ✅ #124 — zoom binds the resident Original (COMPLETE, owner-verified)

Smooth zoom (`=`/`-`, pinch, Ctrl+scroll, hold-to-zoom) in Fit mode magnified the fit-sized
texture. `display_kind()` picked the rep from `view.mode` alone, so zoom could never reach the
ring's `Original` — even when #106.7 had it resident. Now a present-time selector
(`present_kind`) binds it; decode targets stay mode-derived (pinned by a test).

⚠ **The trap:** `present_item` resets zoom/pan via `view_for`, so the rebind needs its own
view-preserving path (`rebind_same_item`) which must **not** re-stamp `last_present` (slideshow
dwell). ⚠ **The Codex P0:** three background paths rebind the Fit slot for the displayed item —
`try_gpu_sharpen`, `try_gpu_derive_fit`, and the `drain_results` sharpen landing. All now
decline while `presented_kind == Original`. **House rule: background work may change residency
or quality, never the presented representation.**

Also fixed this session: a **dialog-window `Resized` was being applied to the main window**
(negative-only id filter in `window_event`), which stretched the toolbar ~10×.

---

# ⏭️ Backlog beyond #125 (carried forward, unchanged)

1. **#121 subtask 4 (FFmpeg 3b)** — interrupt classification (cancel vs deadline vs real
   decode error, `ffmpeg/poster.rs:224`). Small, Mac-doable, closes a known gap.
2. **#109 items 2/3/5** — shared open generation, decode identity, `present_item` propagation.
   Items 1 and 4 are done. ⚠ Its **item 1 is stale**: the audit's claim that macOS lacks the
   cross-cancel is **false** (corrected in `technical-debt-audit.md` 2026-07-20).
3. **#112 profiles** — design rev 4 implementation-ready, paused on owner sign-off.
4. **#106.1 byte cache / #106.3 read throttling** — the COLD-read side; only pays cold.
5. **Re-measure the thumb strip** (`PB_THUMB_DIAG=1`) on the Videos share — two fixes landed
   after the 199 ms reading. The number decides whether #114 selection parity is ever worth it.
6. **Low value:** #121 subtasks 5/6, #92.2 (AVFoundation).

---

# 📓 Load-bearing knowledge (don't re-derive)

- **Cross-machine:** `CLAUDE.md` → *Working across two machines*. One `## Handoff` section per
  plan is the only place live cross-machine state lives. Never mark verified what you could
  not run. ⚠ **`pb-app` builds `AppCore` as a struct literal; `pb-mac-ffi` uses
  `AppCore::new_host`** — so adding an `AppCore` field breaks winit and *not* the Mac, with no
  warning on the Mac. It has already broken `main` once.
- **`pb-mac-ffi` is `#![cfg(target_os = "macos")]`** — on Windows it compiles to an empty
  staticlib, so a syntax error in it produces **zero** errors. Mac-shell edits are unverifiable
  from Windows. The reverse also holds: `pb-app`'s `build.rs` hard-errors on macOS, so the Mac
  reaches it only via the `x86_64-pc-windows-msvc` cross-check (which needs `ureq` at
  `default-features = false` **plus `features = ["json"]`**).
- **Windows build:** `pwsh scripts/build-windows.ps1 -Run`. A bare `cargo run` omits `ffprobe`
  → silent AC-3/DTS. ⚠ A **running exe blocks the linker** (`Access is denied`, os error 5) —
  close the app before rebuilding.
- **Codex:** unreliable on this repo's big files — 2 of 4 runs produced *nothing*, exhausting
  their budget just reading. It works when you **inline the relevant code into the prompt** and
  ask ≤3 focused questions. When it does work it is worth it: it found the verifier's false
  negative and #126's dialog-identity P0.
- **Git:** everything on `main`, pushed. Stage explicit paths, never `-a`. SSH-signed, no AI
  attribution trailers. The owner drives the app while you work.

## Diag levers (debug console only)

`PB_SHARP_DIAG`, `PB_DOOR_DIAG`, `PB_PERF`, `PB_THUMB_DIAG`, `PB_SCALE_POLICY=cpu`,
`PB_DERIVE_KERNEL`, `PB_DERIVE_MIP_BIAS`, `PB_POSTER_WALK=native|fitted`, `PB_AUDIO_TRACE`,
`PB_VIDEO_DIAG`, `PB_AV_SYNC`. Probes: `probe_one_file` / `ab_poster_walk` (ignored tests in
pb-decode; `PB_PROBE_FILE` / `PB_POSTER_AB_DIR`).

Corpus: `\\beenas\Media\Movies`, `D:\Media\Pictures\…\Wedding`,
`D:\Media\2002-password-is-test.7z` (encrypted, password `test`),
`D:\Media\test-archives\`, `D:\Media\Pictures.zip`.

## Known-red, NOT ours

- `pb-decode`'s `plain_fixtures_have_no_dovi_summary` — `FFmpeg decoder: Decoder not found`,
  pre-existing on both Windows runners.
- Two `pb-app-core` video-probe tests are **flaky**, not failing (timing-dependent off-thread
  probes; pass on an idle box).
- **Read step results, not job conclusions**, on this repo's CI right now.

## ⚠️ Task-ID collisions — re-fetch before filing

Happened once already (#115 filed twice; the poster refactor became #121). Highest id in use
is **#126**.
