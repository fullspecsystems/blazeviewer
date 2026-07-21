# Task 131 — Finish the NS0 inversion: de-duplicate the last shell orchestration (audit #1b / #2)

**Status:** planned — 2026-07-21 (rev 2, Codex-reviewed — round 1 folded in; see §7a). Remediation for
**technical-debt audit finding #1(b)** ("finish the NS0 inversion so `AppCore` owns orchestration and
the mirror flags have a single owner") and **finding #2** ("the two parallel platform shells"). This is
the deliberately-separated *(b)* half of finding #1 — *(a)*, the `app_core_impl.rs` file-split, shipped
as #125/#128.

## 0. ⚠ Read this first — the scope is far smaller than the audit/status imply

The audit (and `current-status.md` before rev 28) size this as **"16 orchestration functions
duplicated across two shells + `struct DirScan`/`struct ArchiveLoad` in both."** **That is stale.**
Re-verified against the *current* code on 2026-07-21 (two independent agent sweeps of `pb-app` and
`pb-mac-ffi`, cited throughout §1):

- **`struct DirScan` and `struct ArchiveLoad` are already gone from both shells.** Only #126 NOTE
  comments remain (`pb-app/src/main.rs:4673`/`:4676`, `pb-mac-ffi/src/lib.rs:39`/`:44`). The state
  lives on `AppCore` (`dir_scan: Option<DirScanState>`, `archive_load: Option<ArchiveOpenState>`,
  `bg: BackgroundOps` — `app_core.rs:496-503`).
- **8 of the "16" are already thin core-delegating adapters** (`begin/poll/cancel_dir_scan`,
  `cancel_scan_command`, `scan_pill_visible`, `begin/poll/cancel_archive_load`). #126 moved the whole
  worker lifecycle (spawn / generation / supersession / cancel-timer / stale-result policy) into
  `pb-app-core/src/app_core_impl/{dir_scan,archive_open}.rs`; the shells just pump it.
- **2 are legitimately per-shell** (`apply_menu_state`, `prompt_archive_password`) — their reusable
  logic is *already* in the core (`AppCore::menu_state_from`, `ArchiveOutcome::NeedPassword{wrong}`);
  what remains is genuine platform chrome (muda/egui vs AppKit sheet).

So #126 did the heavy lifting. **What genuinely remains is small and nameable**, and the codebase
already names it: the `ShellFlowAction` catch-all effect is documented as temporary — *"until 5.6
inverts them into specific effects/events"* (`app_core_impl.rs:474`; `contract.rs:602`). Mute,
About/Settings, save-rotation, fullscreen, and `filter_deleted` have **already** been inverted off it
(the `NS0 5.6, inverted` comments at `app_core_impl.rs:530/621/631/642/869`). **This task inverts the
four *cross-shell* flow arms and collapses one stale mirror flag.** (`Quit` and the winit-only
`ToggleToolbar` stay on the seam — §1 — so this does not fully empty it.)

## 1. The exact residue (per-function, both shells, cited)

Classification from the two agent sweeps. **THIN = already a core-delegating adapter (leave).
PLATFORM = genuine per-shell chrome, core logic already extracted (leave). MOVE = duplicated
orchestration logic to invert into the core.**

| Function | winit (`pb-app/src/main.rs`) | macOS (`pb-mac-ffi/src/lib.rs`) | Class | Note |
|---|---|---|---|---|
| `begin/poll/cancel_dir_scan` | 1179 / 1189 / 1198 | 2748 / 2757 / 2770 | THIN | delegate to `core.*` |
| `cancel_scan_command` | 1206 | 2713 | **MOVE (tail)** | core has the policy (`dir_scan.rs:143`); shells add toast + close scanning dialog |
| `scan_pill_visible` | 3020 | 1514 (bridge decl at 4804) | THIN | `core.scan_status()` read |
| `begin/poll/cancel_archive_load` | 1098 / 1112 / 1165 | 2784 / 2798 / 2843 | THIN | delegate to `core.*` |
| `prompt_archive_password` | 1225 | 2227 | PLATFORM | retry driven by `ArchiveOutcome::NeedPassword{wrong}`; only the sheet/window is per-shell |
| `is_archive` (free fn) | 4625 | 3249 | *(opportunistic)* | 1-line `pb_source::archive_kind(p).is_some()`, dup'd; optional shared helper |
| `apply_menu_state` | 2511 | 2358 | PLATFORM | wraps `AppCore::menu_state_from`; cfg-split muda/egui/AppKit + `SetMenuState` emit |
| **`toggle_recursive`** | **1286** (27 lines) | **2649** (~24) | **MOVE** | pure: flip flag → preserve-cursor → `begin_dir_scan` → toast. No shell types. |
| **`toggle_show_archives`** | **1345** (10) | **2678** (~31) | **MOVE** | pure: `settings.show_archives` flip + `save()` + rescan + toast. |
| **`confirm_delete_permanent`** | **1053** (13) | **2200** (~21) | **MOVE (split)** | guard/arm is pure; only the confirm-dialog render is per-shell. |
| `rescan_current_folder` | 1320 *(not in the "16")* | inlined into the toggles | **MOVE (helper)** | shared by both toggles; the cursor-preservation snippet is duplicated verbatim. |

**The `ShellFlowAction` flow arms still live** (`app_core_impl.rs` dispatch → shell handler at
`main.rs:3414` / `lib.rs:2540-2542`). This task inverts the **four cross-shell** ones: `Recursive`,
`ShowArchives`, `CancelScan`, `DeletePermanent`. Two others **stay** and this task does not claim to
empty the seam: `Quit` (genuine window teardown, `app_core_impl.rs:5344`) and **`ToggleToolbar`**
(`app_core_impl.rs:664`, deliberately **winit-only** — no macOS counterpart, so it isn't cross-shell
duplication; inverting it is a separate winit-only cleanup, out of scope here). So "finish 5.6" here
means *the duplicated cross-shell arms are inverted*, **not** "only `Quit` is left on the seam."
⚠ When slimming the shells' flow-action handler (winit `perform_flow_action`), **preserve the `Quit`
and `ToggleToolbar` arms** — do not delete the handler wholesale.

### 1b. The mirror flags (the "single owner" half of finding #1b)

The audit named five hand-synced mirror flags (`app_core.rs:546-568`). Re-checked against #126:

- **`archive_loading` — a redundant, drifting mirror (not a live bug, but the right thing to delete).**
  Written in **exactly one place**: `pb-app/src/main.rs:4326`
  (`self.core.archive_loading = self.core.archive_load.is_some()`). The core never sets it, and
  **macOS never sets it either** (`mac_refs = 0`), so it is stale on macOS by construction — the "mirror
  flag desync across shells" *shape* finding #1b describes. ⚠ **Honest correction (Codex): this is not a
  currently-observable macOS malfunction** — `work_pending()` already checks `self.archive_load.is_some()`
  **independently** (`app_core_impl.rs:396`), so nothing actually depends on the stale flag. The flag is
  simply **redundant** now that #126 made `archive_load` a core field. Deleting it (→ a getter) removes a
  desync-shaped footgun and one hand-sync site; it does not *fix* a live bug. Don't oversell it.
- **`scanning` — already single-owned; only the doc is stale.** Every production write is in the core
  (`dir_scan.rs:119/166/189/248`, `background.rs:24`, `app_core_impl.rs:958`); the shells only *read*
  it. The lone shell write (`lib.rs:5020`) is inside a `#[test]`. The doc comment still claims *"the
  shell keeps it in sync"* (`app_core.rs:546`) — **false since #126**; fix the doc.
- **`launching`, `dialog_open`, `redraw_pending` — legitimate one-way shell→core signals, NOT the
  desync class.** They mirror genuinely shell-owned state: `launching` ← winit's `pending_launch`
  (`main.rs:421`, winit-only), `dialog_open` ← the shell's dialog *window*, `redraw_pending` ← a
  dropped surface present. Each has a single writer per shell and cannot be derived from core state.
  **Leave them, and document them as intentional** so a future session doesn't "fix" them into a
  regression.

## 2. Scope gate

- **In:**
  - **Thread A — invert the four flow arms** off `ShellFlowAction` into core-run logic / specific
    effects: `Recursive`, `ShowArchives`, `CancelScan`, `DeletePermanent`. Factor the shared
    cursor-preservation helper. Delete both shells' duplicated bodies.
  - **Thread B — collapse `archive_loading`** to a core getter (`self.archive_load.is_some()`), delete
    the winit sync site, fix the `scanning` doc, decide `scanning` getter-vs-field, and document the
    three legitimate mirror flags.
- **Out (this task):**
  - `Quit` (stays a `ShellFlowAction`). `apply_menu_state`, `prompt_archive_password` (core logic
    already extracted; the remainder is genuine chrome). The 8 thin scan/archive adapters (already
    done by #126). `launching`/`dialog_open`/`redraw_pending` (legitimate signals).
  - The all-`pub`-fields / `#[non_exhaustive]` discipline (finding #1c). The inline platform *routing*
    consolidation (finding #4). The god-object *file*-split (finding #1a — shipped as #125).
  - `is_archive` free-fn dedup is **opportunistic-only** — a 1-line helper move, take it or leave it;
    not load-bearing.
  - **No behaviour change.** Every toggle/delete/cancel must do exactly what it does today; this is a
    de-dup, not a redesign. If the two shells have drifted, preserve each shell's current observable
    behaviour and file any real difference separately.

## 3. The unifying design — invert, don't just relocate

The lever is the existing `ShellFlowAction` seam and the 5.6 inversion pattern already used five
times. For each flow arm, the core dispatch (`app_core_impl.rs` ~5337-5434) currently does
`self.effects.push(ShellFlowAction(action))`; a shell handler then runs the logic. Inversion means
the **core runs the logic itself** and either needs no shell round-trip (pure arms) or emits a
**specific** effect the shells already render (dialog arms).

### Thread A.1 — `Recursive` / `ShowArchives` (pure — Windows-safe, unilateral)

Move the bodies to `AppCore::toggle_recursive()` / `AppCore::toggle_show_archives()` and have the core
dispatch **call them directly instead of emitting `ShellFlowAction`**. Both are pure core-state logic
(verified identical across shells — `lib.rs:2649-2673` verbatim matches `main.rs:1286`):

```
toggle_recursive: root = scan_root? ; recursive = !recursive ;
                  cursor = current_photo_cursor() ; EMIT BeginDirScan(Scan{roots:[root],recursive}, cursor) ;
                  show_toast("Recursive folders: on|off")
toggle_show_archives: settings.show_archives ^= 1 ; if persist_prefs { settings.save() } ;
                      rescan_current_folder() /* re-arms via EMIT BeginDirScan */ ;
                      show_toast("Show archives: on|off")
```

- Factor `current_photo_cursor()` (`displayed_item → source.path → Cursor::At | Cursor::First`) and a
  core `rescan_current_folder()` **once** — the snippet is duplicated verbatim across `toggle_recursive`,
  `toggle_show_archives`, and the existing winit `rescan_current_folder` (`main.rs:1320`). ⚠ **Move
  *every* rescan caller** onto the core helper, not just the two action toggles — Codex flags winit's
  archive opt-out and the live Settings-edit rescan paths as callers too. ⚠ **Do not implement it via
  `open_plan()`** — that also resets the open-performance timing and `climb_anchor`, which today's
  rescan does not; keep it a plain re-arm of the current scan root.
- ⚠ **`begin_dir_scan` is a real thread spawn.** The toggles must **enqueue the existing `BeginDirScan`
  effect** (which the shells already drain in the same event turn — `main.rs:3202` / `lib.rs:2527`),
  **not** call `AppCore::begin_dir_scan()` synchronously (Codex). Calling it directly would spawn the
  walk *ahead of* previously-queued effects (a subtle supersession/ordering change) and would spin a
  real worker thread inside the toggle **unit tests**. Emitting the effect preserves today's
  worker-start boundary and keeps the tests thread-free (they assert the effect was pushed).
- ⚠ **`persist_prefs` is a boolean *gate*, not a method** (Codex — there is no `AppCore::persist_prefs()`).
  `toggle_show_archives` in the core must write `if self.persist_prefs { self.settings.save(); }`, so a
  unit test (which leaves the gate false) never writes a settings file. Assert exactly that in a test.
- **Why this is Windows-safe / unilateral:** once the core stops emitting `ShellFlowAction(Recursive|
  ShowArchives)`, the macOS handler arms (`lib.rs:2540-2541`) simply **never fire** — they become dead
  code (removed by the Mac session), with **no double-run and no regression** (the core already did the
  work). The winit session can land and verify this alone.

### Thread A.2 — `CancelScan` (core-run cancel + toast — **NO unconditional `CloseDialog`**)

The core already owns the policy: `cancel_scan_command()` returns `bool` (`dir_scan.rs:143`). Invert the
`Action::CancelScan` arm to **just run `cancel_scan_command()` + the toast** in the core — and stop there.

⚠ **Do NOT emit `CloseDialog` here (Codex — this was a bug in rev 1).** `CloseDialog` (`contract.rs:397`)
is **unscoped**: winit closes *whatever* dialog is open, macOS clears *whatever* `shown_dialog` it
tracks. Two facts make an unconditional close wrong:
- **winit no longer presents a Scanning dialog at all** — the ambient scan pill replaced it
  (`main.rs:1183`). So there is nothing to close on winit, and a `CloseDialog` would close an *unrelated*
  dialog if one happened to be up.
- macOS is only safe today because it closes **kind-scoped** (`close_dialog_kinds([Scanning])`,
  `lib.rs:2252`), and the *actual* Scanning-dialog Cancel button already routes through
  `DialogResult::ScanningCancelled`, which emits the correct close on its own.

So the `Action::CancelScan` inversion carries **no dialog effect**. If a legacy scanning-sheet cleanup
must remain on the action path, add a **kind-scoped `CloseDialogKind(Scanning)`** effect — never the
unconditional `CloseDialog`. (Prefer to leave it to the existing `ScanningCancelled` route and add
nothing.) A unit test must prove `CancelScan` does **not** close an unrelated open dialog.

### Thread A.3 — `DeletePermanent` (split — needs a name payload; the coordinated part)

The shared logic is `flush_pending_delete()` → refuse an archive entry (`source.path(item).is_none()`
→ toast) → arm `pending_confirm_delete = Some(item)`; only *rendering the confirm dialog* is per-shell.
Invert to `AppCore::request_delete_confirm()`: do the guard + arm, then either `show_toast("Can't
delete this")` (undeletable) or **emit a confirm request that carries the file name**. Answering is
unchanged — both shells already send `ConfirmAnswered(bool)` (`contract.rs:278`), handled by the existing
`pending_confirm_delete.take()` core arm (`app_core_impl.rs:853`).

**The design decision — a dedicated `ShowDeleteConfirm { name }` effect (Codex-preferred over a getter).**
The confirm dialog shows the file name, which today each shell composes itself (winit
`open_confirm_delete(&name)`, `main.rs:2839`; macOS stashes its own message before emitting
`ShowDialog(Confirm)`, `lib.rs:2200`). The generic `ShowDialog(DialogKind)` (`contract.rs:395`) carries
**no payload and each shell's generic handler only forwards the *kind*** — it does not derive a name.
So two options:

- **(i) A core getter** `confirm_delete_name()` the shell reads when rendering `ShowDialog(Confirm)`.
  Smallest contract change, **but** it is an order-sensitive side channel every shell must remember to
  read, and it does *not* fix the "generic `ShowDialog` handler forwards only the kind" gap.
- **(ii) A dedicated `CoreEffect::ShowDeleteConfirm { name }` variant — RECOMMENDED (Codex).** The
  contract stays explicit, the name is **snapshotted** into the effect (no stale-index race), and each
  shell keeps its own wording. Costs one variant + a match arm in each shell's effect handler.

Go with **(ii)**. This **changes what the shells must handle** (they stop handling `ShellFlowAction(
DeletePermanent)` and start rendering the confirm from `ShowDeleteConfirm`) → **it is the
cross-machine-coordinated arm** (§6).

⚠ **Convert ALL THREE emission sites, not just the dispatch (Codex).** `ShellFlowAction(DeletePermanent)`
is emitted in three places: the action dispatch **plus** two trash-fallback paths — the delete-preflight
fallback (`app_core_impl/delete.rs:35`) and the failed-trash fallback (`delete.rs:90`). Removing the shell
handlers while leaving either fallback on the old effect **breaks those delete flows**. All three must
move to `request_delete_confirm()` / `ShowDeleteConfirm`.

Because A.3 alone touches the macOS effect handler, keep #131 low-risk by landing **A.1 + A.2 + Thread B
first** (fully Windows-verifiable), then A.3 as the coordinated arm the Mac co-lands (§6).

### Thread B — `archive_loading` → getter; `scanning` doc/getter

- Replace `pub archive_loading: bool` with `fn archive_loading(&self) -> bool { self.archive_load.is_some() }`
  (or keep the field but compute it at read). Delete the winit sync site (`main.rs:4326`). Update the
  ~2 winit read sites. macOS now reports the true value for free — but per §1b this is a redundancy
  cleanup, **not** a live-bug fix (`work_pending` already reads `archive_load` directly). Confirm nothing
  in either shell relied on the old always-`false` macOS behaviour (it shouldn't).
- `scanning`: it is already core-owned (every production write is in the core; the shells only read).
  **Fix the stale doc** (`app_core.rs:546`, which still claims the shell syncs it). Then decide: convert
  to a getter `self.dir_scan.is_some()` **only if** provably equal at every transition. ⚠ It is **not**
  trivially equal (Codex): the public `CoreEvent::ScanDone` → `finish_scan()` (`app_core_impl.rs:957`)
  clears **`scanning` but not `dir_scan`**, so equivalence is conventional, not type-enforced. Unless
  this task also internalizes/removes that stale `ScanDone` path, **leave `scanning` a field** (it is
  already single-owned — the shell sync the audit feared was removed by #126) and just correct the doc.
- Add a one-line doc to `launching`/`dialog_open`/`redraw_pending` noting they are intentional
  shell→core signals with a single writer, not mirror-desync debt.

## 4. Safety model (what stands in for #130's mock test)

Layered, strongest first:

1. **New core unit tests — the primary net, and a first-class deliverable.** Every moved method becomes
   a pure `AppCore` method testable with no shell: build an `AppCore` (the `dir_scan.rs`/`archive_open.rs`
   test modules already show the fixtures — `armed_scan_core`, `archive_core`), call
   `toggle_recursive()` / `toggle_show_archives()` / `request_delete_confirm()`, and assert on **(a)**
   the resulting state (`recursive`, `settings.show_archives`, `pending_confirm_delete`, the armed
   `dir_scan`), **(b)** the **emitted `CoreEffect`s** (a `BeginDirScan`/`ShowDialog`/`CloseDialog` was
   pushed — the same `effects`-vector assertions the existing tests use, e.g.
   `app_core_impl.rs:5439`), and **(c)** the toast text. Include the edge cases the shells guard:
   `toggle_recursive` with `scan_root == None` (no-op), `request_delete_confirm` on an archive entry
   (toast, no arm), `toggle_show_archives` persistence via `persist_prefs` (a unit test must not write
   a file). `archive_loading` gets a test that it tracks `archive_load` with no shell involvement.
   **Required tests Codex called out explicitly:**
   - **Dispatch-level negative tests:** each converted action (`Recursive`/`ShowArchives`/`CancelScan`/
     `DeletePermanent`) emits **no** `ShellFlowAction` (the mirror of the existing
     `app_core_impl.rs:5439` assertion — proves the inversion actually happened).
   - **The two delete-fallback sites** (`delete.rs:35`/`:90`) now go through `request_delete_confirm` /
     `ShowDeleteConfirm`, not `ShellFlowAction(DeletePermanent)`.
   - **`CancelScan` does not close an unrelated open dialog** (the A.2 unscoped-`CloseDialog` guard).
   - **`begin_dir_scan` is not called synchronously** by the toggles — assert the `BeginDirScan`
     **effect** is emitted (so the tests stay thread-free, matching the direct-vs-effect design choice).
2. **The existing core suites pass unchanged** — `dir_scan.rs`/`archive_open.rs` carry ~30 tests
   (supersession, cancel, reveal-delay, empty-deck welcome-hint); a moved toggle must not disturb them.
   ⚠ **Update + run the existing macOS delete test** (`delete_permanent_confirms_then_deletes` — Codex)
   against the new `ShowDeleteConfirm` path; do not rely on a manual smoke alone.
3. **Winit build + owner run on Windows** — real toggle-recursive / show-archives / delete / cancel on a
   corpus folder. Behaviour-unverified until a human runs it.
4. **macOS build + run** — the cross-machine gate (§6). **Only the Mac can compile `pb-mac-ffi`.**

**Also update the stale prose (Codex):** contract/comment text that still says "the host owns the
scan/archive worker lifecycle" is wrong post-#126 — fix it where the inversion touches it.

## 5. Sequencing

1. **Thread B first (smallest, Windows-safe, de-risks the reads):** `archive_loading` getter + delete
   the winit sync + `scanning` doc fix. Verify: core tests + winit build + the existing suites.
2. **Thread A.1 (`Recursive`/`ShowArchives`):** add the core methods + `current_photo_cursor()` /
   `rescan_current_folder()` helpers + tests; switch the core dispatch to run them directly and stop
   emitting `ShellFlowAction(Recursive|ShowArchives)`; delete the winit bodies + handler arms. Verify:
   core tests + winit run. macOS dead-handler removal is deferred to the Mac (harmless until then).
3. **Thread A.2 (`CancelScan`):** core runs `cancel_scan_command` + toast + `CloseDialog`; slim the
   winit copy. Verify.
4. **Thread A.3 (`DeletePermanent`):** the coordinated arm — implement `request_delete_confirm()` + the
   name-getter (option i), switch the core to emit `ShowDialog(Confirm)`, slim the winit copy, verify
   the winit confirm still shows the file name. **Leave the macOS side to the Handoff.**
5. **`is_archive`** (optional): if trivial, hoist to a shared `pb_source`/core helper; else skip.

One commit per thread/arm, so the cross-machine bisect stays clean and the Mac can cherry-pick the
verification per arm.

## 6. Cross-machine handoff (the real risk — this is NOT #130)

**#130 was Windows-verifiable end to end. This is not.** `pb-mac-ffi` is `#![cfg(target_os = "macos")]`
— on Windows it compiles to an **empty staticlib, so a syntax error in it produces zero errors**
(`CLAUDE.md` → *Working across two machines*). The Windows session does the core + winit; the Mac must
finish and verify its shell. Leave a live `## Handoff` in this plan with:

- **Verified (Windows):** core unit tests, winit build + owner run of each flow (list which).
- **NOT verified — the Mac must do:**
  1. **Delete the dead macOS arms** for `Recursive`/`ShowArchives` (`lib.rs:2540-2541`) and the
     `toggle_recursive`/`toggle_show_archives` bodies (`lib.rs:2649`/`2678`). *(Dead, not broken — the
     core runs the logic; removal is cleanup.)*
  2. **⚠ The load-bearing check — `DeletePermanent` (A.3):** add the macOS effect-handler arm for the new
     `CoreEffect::ShowDeleteConfirm { name }` and confirm it renders the confirm **sheet with that name**.
     The macOS `ShowDialog(Confirm)` handler forwards only the kind and would not carry a name, so **if
     the `ShowDeleteConfirm` arm is missing, macOS delete-confirm silently breaks** (unhandled effect /
     nameless sheet) — invisible to the Windows session. This is the one arm that can break macOS
     silently. Also update + run the macOS `delete_permanent_confirms_then_deletes` test.
  3. **`CancelScan` (A.2):** confirm the macOS scanning-sheet Cancel still closes correctly via its own
     `DialogResult::ScanningCancelled` route (the action arm now emits **no** dialog effect). If A.2 added
     a kind-scoped `CloseDialogKind(Scanning)`, handle it on macOS.
  4. **`archive_loading`:** macOS now reports the true value. Per §1b this is redundancy cleanup, not a
     live-bug fix (`work_pending` already reads `archive_load`); confirm nothing relied on the old value.
- **Cross-platform debt line** (the dangerous green category): any commit touching shared dispatch that
  the Windows session couldn't compile-verify for macOS gets a line here; only the Mac strikes it.
- **Revert lever (make it exact, not "a one-line gate" — Codex):** if A.3 must land before the Mac
  co-lands, use a **target-scoped emission gate** — emit the legacy `ShellFlowAction(DeletePermanent)`
  on `cfg(target_os = "macos")` and the new `ShowDeleteConfirm` elsewhere, **never both**. State here the
  exact removal condition (delete the macOS `cfg` arm once the Mac has the `ShowDeleteConfirm` handler +
  a green delete test). A.1/A.2/Thread B need no lever — their old arms are inert dead code, not a fork.
- **⚠ The struct-literal trap — and why `ShowDeleteConfirm` avoids it:** `pb-app` builds `AppCore` as a
  **struct literal** while `pb-mac-ffi` uses `AppCore::new_host`, so a new `AppCore` field breaks the
  **winit** build (caught on Windows) and **not** macOS. The recommended `ShowDeleteConfirm { name }`
  **snapshots the name into the effect**, so no new `AppCore` field is needed — prefer it over a cached
  field or an order-sensitive getter for exactly this reason.
- **Fetch before start and before push** (both machines commit to `main`).

## 7. Risks

| risk | severity | mitigation |
|---|---|---|
| **macOS delete-confirm silently breaks** (A.3 `ShowDeleteConfirm` arm missing on macOS → unhandled effect / nameless sheet) | **high — silent macOS regression** | §6 item 2 is the explicit Mac gate; `ShowDeleteConfirm { name }` snapshots the name (no getter side channel); target-scoped revert lever holds the legacy path on macOS until it lands |
| **`CancelScan` closes an unrelated dialog** (unconditional `CloseDialog` — the rev-1 bug) | high | §3 A.2: the action arm emits **no** dialog effect; rely on the existing `ScanningCancelled` route; a test proves no unrelated dialog closes |
| **A `DeletePermanent` emission site is missed** (the two `delete.rs` trash-fallbacks) → that delete flow breaks | high | §3 A.3: convert **all three** sites; a test covers each fallback |
| A moved toggle changes observed behaviour on one shell (drift between the two copies) | high | §2 preserve-behaviour gate; the core unit tests pin state+effects+toast; port winit first against the tested core, Mac verifies parity |
| Toggle calls `begin_dir_scan` synchronously → spins a real thread in unit tests / reorders supersession | medium | §3 A.1: emit the `BeginDirScan` **effect**; tests assert the effect, no thread |
| `persist_prefs` misuse — a unit test writes a settings file (privacy/test hygiene) | medium | §3 A.1: `if self.persist_prefs { settings.save() }`; assert no file written |
| `scanning` getter conversion is not exactly `dir_scan.is_some()` (the `ScanDone`→`finish_scan` edge) | medium | §3 B: only convert if provably equal; otherwise fix the doc and leave the field — already single-owned |
| Scope creep into the other findings (routing #4, pub-fields #1c, `apply_menu_state`, `ToggleToolbar`) | medium | §2/§1 out-list is explicit; `ToggleToolbar`/`Quit` stay on the seam, `apply_menu_state`/password chrome stay per-shell |
| The Windows session marks A.3 "done" off a green winit run | high | §6: A.3 is **not done** until the Mac has the `ShowDeleteConfirm` handler + a green delete test |

## 7a. Codex review (2026-07-21, round 1 — folded in)

Reviewed with the plan + ground-truth inlined; Codex read the cited sources. Confirmed the inversion is
appropriately scoped and low-regression **with corrections**, all folded above:

1. **`CancelScan` unconditional `CloseDialog` was a real bug** — the effect is unscoped and winit no
   longer even has a Scanning dialog (the pill replaced it). Now emits no dialog effect (§3 A.2).
2. **`DeletePermanent` → a dedicated `ShowDeleteConfirm { name }` effect**, not `ShowDialog(Confirm)` +
   getter (explicit contract, snapshots the name, avoids the struct-literal field trap) (§3 A.3, §6).
3. **All three `DeletePermanent` emission sites** convert, incl. the two `delete.rs` trash-fallbacks (§3 A.3).
4. **Toggles emit `BeginDirScan`, not call `begin_dir_scan()`** (ordering + thread-free tests); **`persist_prefs`
   is a boolean gate**, not a method; **route every rescan caller** through the core helper; **don't build
   rescan on `open_plan()`** (§3 A.1).
5. **`ToggleToolbar` also still on `ShellFlowAction`** (winit-only) — corrected the "only `Quit` remains"
   overclaim; both stay on the seam (§1).
6. **`scanning` equivalence is conventional** (`ScanDone`→`finish_scan` clears `scanning` not `dir_scan`) —
   leave it a field, fix the doc (§3 B). **`archive_loading` is redundant, not a live bug** — `work_pending`
   already reads `archive_load` (§1b).
7. **Extra tests required** — dispatch-level "no `ShellFlowAction` emitted", the two fallback sites, the
   CancelScan-doesn't-close-unrelated test, update+run the macOS delete test, fix stale worker-lifecycle
   prose, and preserve `ToggleToolbar`/`Quit` when slimming the flow handler (§4).

## 8. What this does NOT do

- It does not touch the wire/contract semantics beyond inverting four `ShellFlowAction` arms into
  core execution + (for delete) one specific effect/getter. `Quit` stays on the seam.
- It does not change what the user sees: a correct execution is behaviour-identical (same toggles, same
  toasts, same confirm dialog, same cancel), with the logic living once instead of twice.
- It does not address the god-object *field* discipline (#1c), the inline platform *routing* (#4), or
  the `apply_menu_state`/`prompt_archive_password` chrome — each is its own finding.
- It does not, by itself, let the shells stop building `AppCore` as a struct literal — that is the #1c
  accessor-discipline task, gated on this landing.
