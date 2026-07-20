# Task 126 — DRY the two shells: move dir-scan + archive-open orchestration into the core

**Status:** plan, rev 1 (pre-Codex).
**Relationship to #125:** do **this first**. See §1.

## The one-sentence version

Two shells hand-maintain identical copies of the dir-scan and archive-open worker
lifecycles. The core already owns the logic, already spawns threads, and already has the
contract vocabulary — so move the lifecycle in and let both shells become event pumps.

---

## 1. Why this outranks #125 (the file split)

#125 splits `app_core_impl.rs`. This task de-duplicates `main.rs` ⇄ `pb-mac-ffi/lib.rs`.
**They are different files and the work is orthogonal** — #125 buys nothing toward
cross-platform maintainability, which is the actual goal. Ordering:

| | task | buys | on the cross-platform path? |
|---|---|---|---|
| **1st** | **#126 (this)** | every future cross-platform feature costs 1× instead of 2× | ✅ yes |
| 2nd | #125 | navigability, review size, merge-conflict surface | ❌ no — comfort, not capability |

Both get done (owner, 2026-07-19); this is the order.

## 2. Where it slots into the audit

Audit **finding #2** (*"the two parallel platform shells"*, called *"the single worst
cross-platform liability"*) and **finding #1 remediation (b)**.

⚠ **Both are mis-scoped in the audit** — see `.taskmaster/plans/125-split-app-core-impl.md`
§2a. The audit calls this *"the half-finished NS0 inversion"* and sizes it at ~2,870 lines.
NS0 shipped (`pb-app-core` is 50,337 LOC; the real FFI surface starts at line 4,390 of
6,393; 102 `CoreEffect` variants; macOS ships a native SwiftUI host that could not exist
otherwise). The ~2,870 figure measured all of `impl AppCoreHandle`, most of which is
legitimate FFI shimming. **The real duplication is ~390 lines per shell.**

## 3. The evidence that this belongs in the core

Four findings, all re-measured at `HEAD` (`348da435`):

1. **The duplicated structs have zero platform-specific fields.** `DirScan` is
   `{generation, rx, progress, name, started}` in *both* shells — field for field.
   `ArchiveLoad` is `{generation, rx, path, attempted_password, progress}` in both. Every
   type involved (`Receiver`, `ScanProgress`, `Instant`, `PathBuf`, `SecretString`,
   `OpenProgress`) is platform-neutral and already visible to `pb-app-core`. There is
   nothing shell-shaped about this state.
2. **The core already spawns threads** — 7 `thread::spawn` sites in `app_core_impl.rs`
   (decode pool, archive opens, container probes, RAW demosaic). "The core can't own a
   worker" is not a real objection; it already does, repeatedly.
3. **The core already owns the payload logic** — `scan.rs` (1,675 lines) exposes
   `stream_scan`, `resolve_scan`, `ScanProgress`, `ScanUpdate`, `Resolved`;
   `archive.rs` (385) owns the open + `ArchiveOpenError`. The shells spawn a thread
   *only to call these*, then hand-manage the receiver.
4. **The contract already has the whole vocabulary** — `BeginDirScan`, `BeginArchiveOpen`,
   `ScanBatch`, `ScanDone`, `ArchiveResolved`, `CancelScan`, `CancelArchiveLoad`,
   `ShowDialog`, `CloseDialog`, `Password`, `Loading`, `Scanning`, `PasswordSubmitted`,
   `PasswordCancelled`, `DialogResolved`, `SetDialogChecking`. Nothing needs inventing.

**Nothing blocks this. It was simply not finished.**

## 4. The duplication, measured

16 functions, ~390 lines per shell:

| function | winit LOC | mac LOC |
|---|---:|---:|
| `begin_dir_scan` | 61 | 61 |
| `poll_dir_scan` | 78 | 61 |
| `cancel_dir_scan` | 8 | 7 |
| `begin_archive_open` | 94 | 78 |
| `poll_archive_load` | 30 | 27 |
| `prompt_archive_password` | 24 | 18 |
| `apply_menu_state` | 33 | 37 |
| `confirm_delete_permanent` | 14 | 22 |
| `toggle_recursive` | 28 | 25 |
| `toggle_show_archives` | 11 | 32 |
| `finish_archive_open` / `fail_archive_open` / `cancel_archive_load` | 11 | 15 |

Plus `struct DirScan` + `struct ArchiveLoad` in both, and `SCAN_DIALOG_DELAY = 250ms`
declared independently in each (`main.rs`, `pb-mac-ffi/lib.rs:44`).

**Drift status — stated honestly.** Normalized body comparison: `begin_dir_scan` 38
identical / 10 winit-only / 13 mac-only; `poll_dir_scan` 38 / 21 / 9; `begin_archive_open`
53 / 13 / 5. **Much of that is mechanical**, not semantic (`self.core.` vs `handle`,
`close_scanning_dialog()` vs `close_dialog_kinds(&[Scanning])`). **No live behavioural
divergence has been demonstrated**, and one suspected case (the scan-dialog delay) was
checked and found identical on both. So the case for this task is *a proven drift hazard
plus a 2× cost on every future feature* — **not** "there are bugs in here right now."
Do not oversell it; if a real divergence turns up during the move, record it as a finding.

One divergence the audit does claim (#3 / task #109 item 1, **unverified here**): the mac
shell lacks the cross-cancel between the two worker types that Windows got in `8293a662`.
If true, this task fixes it for free. **Verify before claiming it as a benefit.**

## 5. Target design

`AppCore` owns both lifecycles; the shells keep only what is genuinely platform-bound.

**Moves into the core:** `DirScan` + `ArchiveLoad` (as private core state), the thread
spawn, the `try_recv` pump, generation/supersede logic, `SCAN_DIALOG_DELAY` and the
reveal-after-delay decision, cancel semantics, the password re-prompt/retry state machine,
and the MRU promotion of a winning session password.

**Stays in the shell:** creating/destroying the actual dialog *window* (an egui second
window on winit, a SwiftUI sheet on macOS) — already effect-routed via
`ShowDialog`/`CloseDialog` — and the platform's own event/timer pump that calls `tick`.

**The shells reduce to:** forward user intent as `CoreEvent`, call `tick`, drain
`CoreEffect`. The expected diffstat is roughly **−390 lines in each shell**, +~400 in the
core. A step that does not *delete* meaningfully from both shells has not achieved anything.

## 6. ⚠ Privacy constraint (non-negotiable)

The archive-password path carries `SecretString`s under the Second Directive: zeroized on
drop, redacted `Debug`, never `Display`ed or serialized, never a `Settings` field, wiped at
teardown (`clear_session_state` / the macOS quit intercept) explicitly so it holds even if
the process `exit()`s without unwinding.

Moving `attempted_password` and the auto-try winner across the crate boundary is exactly the
kind of change that quietly regresses this. **Requirements:** no new `Debug`/log site touches
a password; the zeroizing teardown still runs on both platforms; `settings.save()` still
cannot reach it. Add an explicit test that a moved password is redacted in `{:?}` output,
and re-run the no-trace integration test before landing.

## 7. Sequencing

1. **`dir_scan` first** — 5 functions, one dialog kind, no password path, no retry. It is
   the whole pattern in miniature: spawn, pump, supersede, cancel, reveal-after-delay.
   Prove the shape here.
2. **`archive_open`** — the hard one: password prompt, wrong-password re-prompt, RAM
   pre-flight, progress, the `SecretString` path (§6), and the Loading→Password dialog
   transition. Do not start until step 1 has landed on both platforms.
3. **The strays** — `apply_menu_state`, `confirm_delete_permanent`, `toggle_recursive`,
   `toggle_show_archives`. Small, independent, and a good place to stop if fatigue sets in.
4. **Then #125** (the file split), with these ~800 lines already gone from the shells.

## 8. Verification (this is NOT #125's body-diff — the structure genuinely changes)

#125 is a pure move and can be proven by hashing bodies. **This one cannot.** It needs
behavioural proof:

- **The payoff is testability.** These flows are currently in `pb-app` (**3.9 tests/kLOC**,
  the least-tested large code in the repo, per audit #6) and in `pb-mac-ffi` (6.0). In the
  core they become unit-testable for the first time. Write the state-machine tests
  *first*: supersede-by-generation, cancel mid-walk, reveal-after-delay, wrong-password
  re-prompt, teardown-while-in-flight.
- **One test, both platforms.** A core-level test asserting the emitted `CoreEffect`
  sequence for a scan/open is the structural replacement for "keep two copies in sync by
  hand" — that is the actual deliverable, more than the line count.
- **macOS must be exercised, not assumed.** The mac host cannot be tested from the Windows
  dev box. Every step needs a real macOS run before the next begins: open a folder, open a
  password-protected archive, cancel a slow scan, quit mid-scan.
- Re-run the no-trace integration test (`viewing_a_folder_writes_nothing_to_disk`) and the
  static write-path audit after step 2.

## 9. Risks

| risk | severity | mitigation |
|---|---|---|
| macOS regression invisible from the Windows dev box | **highest** | §8: a real macOS run gates every step; dir-scan first so the first mac test is the simple one |
| `SecretString` privacy regression in the move | **high** — Second Directive | §6: explicit redaction test, no-trace test re-run, zeroizing teardown verified on both |
| Scope creep into the `AppCore` field sprawl / #125 | medium | those are (c) and #125; note and defer |
| Teardown/cancel semantics differ subtly per platform and get flattened | medium | write the cancel/teardown tests before moving; treat any discovered difference as a finding to decide deliberately, not to silently unify |
| "Nothing to delete" — code moves but shells keep wrappers | medium | §5: a step that doesn't delete ~equally from both shells failed |
| Merge conflicts (concurrent owner edits, #1 churn file) | medium | one function group per commit, explicit paths, land same-day |

## 10. What this does NOT fix

- `AppCore`'s ~165 `pub` fields (that is remediation (c), and it needs this first).
- `app_core_impl.rs` being 20,932 lines — that is #125, and this task *adds* ~400 lines to
  the core before #125 removes them. Expect the file to get slightly worse before it gets
  better; that is the correct order anyway, because moving these flows in first means #125
  sorts them into the right cluster once instead of twice.
