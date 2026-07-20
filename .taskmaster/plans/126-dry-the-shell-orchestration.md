# Task 126 — DRY the two shells: move dir-scan + archive-open orchestration into the core

**Status:** plan, **rev 2 - Codex round 1 folded**. Codex verdict: *"approve the direction,
but revise the plan before implementation. Add a phase zero."* Its architectural requirements
are now section 5a; a hard tooling constraint found while starting work is section 0.
**Relationship to #125:** do **this first**. See §1.

## The one-sentence version

Two shells hand-maintain identical copies of the dir-scan and archive-open worker
lifecycles. The core already owns the logic, already spawns threads, and already has the
contract vocabulary — so move the lifecycle in and let both shells become event pumps.

---

## 0. HARD CONSTRAINT: the macOS half cannot be written from the Windows box

`crates/pb-mac-ffi/src/lib.rs:22` is `#![cfg(target_os = "macos")]` - **the entire crate is
compiled out on every non-macOS target.** Verified empirically, not assumed: injecting a
deliberate syntax error into `struct DirScan` and running `cargo check -p pb-mac-ffi` on
Windows produces **zero errors**. A `--workspace` build makes it an empty staticlib.

Consequences, severe for this task specifically:

- Changes to the mac shell cannot be compiled, type-checked, linted or tested from the
  Windows dev box. Not "hard to verify" - **not verifiable at all**.
- The half living in `pb-mac-ffi` is exactly the half Codex identifies as carrying the real
  platform-specific risk (dialog realization, main-actor drain ordering, the Swift password
  boundary).

**So the work splits into two independently landable halves:**

| half | where | verifiable on Windows? |
|---|---|---|
| **A** - core machinery + the winit shell rewired | `pb-app-core`, `pb-app` | yes: compiles, unit tests, clippy |
| **B** - the mac shell rewired onto the same core | `pb-mac-ffi` + `mac/` | **no - requires a macOS machine** |

Half A is a strict improvement alone: the canonical lifecycle lands in the core with tests
and one of the two copies retires. The mac shell keeps its copy until B, so the count goes
2 -> 2 but one becomes authoritative and tested. **The DRY win is not realised until B
lands**, and B must be done on a Mac by someone who can run it.

**Rule for half A: additive-only changes to `pb-app-core`'s public surface.** The mac shell
compiles against it and cannot be checked here, so nothing it uses may change signature or
disappear. Breaking changes are deferred to B.

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

## 5a. Phase zero - Codex's architectural requirements (fold before implementing)

Codex round 1 approved the direction but ruled the plan under-specified. Four requirements
must land *before* either flow moves, or they get retrofitted painfully:

1. **An operation-identity / cancellation coordinator, first.** Codex on the ordering in
   section 7: *"The stated order is incomplete. Establish the shared cross-operation
   cancellation/generation coordinator first. Without that preparation, moving only
   directory scanning creates split ownership of a single invariant."* Dir-scan and
   archive-open supersede **each other**, so their generation/cancel policy is one
   invariant; moving one flow without the shared coordinator splits its ownership across
   crates - strictly worse than today.
2. **Identity-stamped dialogs (the P0).** Today's `ShowDialog`/`CloseDialog` are
   *kind-only*, so a late close from generation N can dismiss the dialog belonging to
   generation N+1. Effects must carry an operation id, and shells should **reconcile toward
   a desired dialog state** rather than execute unqualified imperative commands; stale
   show/update/close/`DialogResolved` are ignored by identity. Codex: *"the main
   platform-specific resistance to unification is not the worker, but realization and
   acknowledgement of its UI state."*
3. **Injectable runtime + clock.** Core-owned threads do **not** break headless tests
   (`renderer = None` is orthogonal) - but uninjectable `thread::spawn`, real filesystem
   work and `Instant::now()` make the state machine's tests slow and nondeterministic.
   Production uses `std::thread::spawn`; tests drive workers manually with a fake clock and
   explicit completion points. Keep it a *private core-runtime* abstraction, not a broad
   public trait. Store a **deadline supplied by the injected clock**, not a bare `Instant`,
   so the reveal-delay test is not a real-time test.
4. **A defined wake contract** - how a worker's completion wakes the core on each platform
   (winit's event-loop proxy vs the macOS main-actor drain).

Also folded, same round:

- **Section 8's effect-sequence assertion is too brittle** and is replaced - see the
  rewritten section 8.
- **`ScanProgress` (a shared atomic) is not an FFI-safe progress model.** SwiftUI should not
  observe a Rust synchronisation object. Expose immutable snapshots, or a query keyed by
  operation id. Half B's problem, but design for it in A.
- **Error/disconnect semantics are unspecified** and must be defined: worker panic, sender
  dropped with no terminal update, receiver disconnected after cancel, progress arriving
  after a terminal message, duplicate terminals, send-failure because the operation was
  superseded, an invalid `BeginDirScan` payload.
- **`begin_dir_scan` mutates state before validating its input** (it flips cancellation,
  clears tombstones and bumps the generation before confirming the `Source` is
  `Source::Scan`). Harmless in a shell method with one caller; a latent bug once it is a
  generally callable core transition. Validate first, or take a scan-specific input type.
- **The "strays" (`apply_menu_state`, `confirm_delete_permanent`, the toggles) become their
  own task.** They are not worker-lifecycle code, and bundling them blurs the acceptance
  boundary. Section 7 step 3 is deferred out of #126.
- **Line deletion is evidence, not the acceptance contract.** The real criterion: *neither
  shell owns operation generation, cancellation, retry, timeout or stale-result policy.*

**Rejected, with reasons.** Codex was asked whether a `ShellOrchestration` trait or one
generic worker-lifecycle abstraction would be better. Both were rejected and I agree: a
trait *"risks preserving shell-owned policy behind two implementations"* - DRY-looking while
keeping two policies - and a single generic lifecycle is premature because scans are
**streaming** while archive opens are **one-shot, retrying and secret-bearing**. The shape
adopted instead is a core-owned coordinator plus **two bespoke state machines sharing small
private primitives** (identity, cancellation, wake, deadline, terminal cleanup).

## 6. Privacy constraint (non-negotiable)

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

0. **The coordinator + runtime/clock + dialog identity** (section 5a), *before* either flow.
1. **`dir_scan`** - 5 functions, one dialog kind, no password path, no retry. It is
   the whole pattern in miniature: spawn, pump, supersede, cancel, reveal-after-delay.
   Prove the shape here.
2. **`archive_open`** — the hard one: password prompt, wrong-password re-prompt, RAM
   pre-flight, progress, the `SecretString` path (§6), and the Loading→Password dialog
   transition. Do not start until step 1 has landed on both platforms.
3. ~~The strays~~ **deferred out of this task** per section 5a (not worker-lifecycle code).
4. **Then #125** (the file split), with these lines already gone from the shells.

Each of 0/1/2 splits into half A (core + winit, verifiable here) and half B (mac, needs a
Mac) per section 0.

## 8. Verification (this is NOT #125's body-diff — the structure genuinely changes)

#125 is a pure move and can be proven by hashing bodies. **This one cannot.** It needs
behavioural proof:

- **The payoff is testability.** These flows are currently in `pb-app` (**3.9 tests/kLOC**,
  the least-tested large code in the repo, per audit #6) and in `pb-mac-ffi` (6.0). In the
  core they become unit-testable for the first time. Write the state-machine tests
  *first*: supersede-by-generation, cancel mid-walk, reveal-after-delay, wrong-password
  re-prompt, teardown-while-in-flight.
- **Assert semantic invariants, NOT an effect transcript** (Codex P1: a full ordered
  sequence pins incidental details, like whether a progress update drains before a dialog
  reveal). Pin instead: only the latest operation may modify the deck; starting either
  operation cancels and invalidates the other; no Scanning dialog before the delay, and at
  most once for a still-current slow scan; a cancelled generation emits no successful
  terminal application; a wrong password re-prompts for the *same* operation; a successful
  password is promoted once and never emitted back to the UI; teardown permits no later deck
  or dialog mutation; every terminal path clears the active operation. Use **partial-order**
  assertions where ordering is genuinely semantic (`Begin` < reveal; `PasswordSubmitted` <
  retry/success; `Cancel` < ignored completion).
- **macOS must be exercised, not assumed, and per section 0 it cannot even be COMPILED
  from the Windows box.** Half B needs a real macOS machine: build, then open a folder, open
  a password-protected archive, cancel a slow scan, and quit mid-scan.
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

---

## 11. Findings from the first working session (2026-07-19, unattended)

Recorded as they were verified, so the next session does not re-derive them.

### 11.1 The audit's cross-cancel claim is FALSE — correct it at the source

Audit finding #3 / task #109 item 1 claims *"the mac shell still lacks the cross-cancel that
Windows got in `8293a662`"*, and §4 of this plan carried it forward as unverified. **It is
wrong.** `pb-mac-ffi`'s `begin_archive_open` contains the cross-cancel with an explicit
citation:

> Cross-type supersession (#109 item 1, winit parity — `8293a662`) … supersedes the scan too.

So macOS is at parity here. This removes the one concrete benefit §4 hoped to claim for
#126, and the task must not be justified on it. **Fix `technical-debt-audit.md` finding #3
and task #109 item 1.**

### 11.2 A real (harmless today) asymmetry in `cancel_dir_scan`

- **macOS** clears the handle *inside* `cancel_dir_scan` (`self.dir_scan = None;`).
- **winit** does not; its comment claims *"Every cancel path clears `dir_scan` immediately
  after"* — and **two of its five call sites do not** (`begin_dir_scan`, which replaces the
  handle anyway, and `clear_session_state`, which is teardown).

So the behaviour is equivalent today and there is **no live bug** — but winit's version is
correct only by a call-site convention its own comment overstates, while macOS's is correct
by construction. **When unifying, adopt the macOS shape** (clear inside the function). This
is a good example of the drift hazard being real without a bug being present.

### 11.3 Codex's "injectable clock" requirement was already half-met

`AppCore::now` already exists and is stamped by the shells once per event (NS0 5.5), and 18
existing test sites already drive it. So no `Clock` trait is needed — the state machines just
have to take `now` as a parameter instead of calling `Instant::now()`. `BackgroundOps` does
this. What is *not* yet solved is the injectable **worker runtime** (deterministic completion
points for "cancel mid-walk" / "teardown in flight"), which remains open.

### 11.4 Status at end of session

| item | state |
|---|---|
| Plan rev 2 (Codex round 1 folded) | ✅ on `main` |
| Phase 0 — `BackgroundOps` coordinator + 7 tests | ✅ on `feat/126-dry-shell-orchestration` |
| Phase 0 — identity-stamped dialogs (Codex P0) | ❌ not started |
| Phase 0 — injectable worker runtime | ❌ not started |
| Phase 0 — wake contract | ❌ not started |
| Step 1 — dir-scan state machine in the core + 11 tests | ✅ on the branch |
| Step 1 — winit rewired onto it | ⏸ **deliberately deferred, see 11.5** |
| Step 1 — mac rewired | ⛔ **impossible from this machine** (§0) |

`BackgroundOps` is additive and wired to nothing, so the branch is safe to leave or discard.
The next session should do identity-stamped dialogs (the P0) before moving dir-scan, since
retrofitting dialog identity after the flow moves is the painful order.

### 11.5 Why the winit shell was deliberately left unwired

The core now owns the dir-scan lifecycle and the shells' copies are redundant — but neither
shell has been migrated yet, on purpose.

The interim API is a **return value** (`ScanPoll` / `ScanDialogRequest`) rather than a
`CoreEffect`, because §0 forbids touching the shared contract from this machine. The very
next piece of work — the phase-0 P0, on a Mac — replaces that with **identity-stamped dialog
effects** and migrates both shells onto them together.

So rewiring winit now would migrate it **twice**: once onto `ScanPoll`, then again onto the
final effects a day later, with a throwaway shell diff in the repo's #1 churn file in
between. Leaving it lets the Mac session move both shells once, onto the shape that lasts.

**Consequence to be honest about:** no shell code has been deleted yet, so *the DRY win is
not yet realised*. What exists is the tested, canonical implementation both shells will
adopt. If the Mac session never happens, this commit is net-neutral-plus-tests, not a win.

### 11.6 Exact starting point for the macOS session

1. Read §0, §5a, and §11 here. Nothing needs re-deriving.
2. Do the **P0 first**: give `CoreEffect::ShowDialog`/`CloseDialog`/`SetDialogChecking` an
   operation id, and have shells reconcile toward a desired dialog state rather than execute
   imperative commands. `BackgroundOps::OpId` is the identity to stamp. ~50 sites in
   `pb-mac-ffi`, 3 of them constructing `ShowDialog`.
3. Then migrate **both** shells' dir-scan onto `AppCore::{arm,poll,cancel}_dir_scan`,
   deleting `struct DirScan`, `SCAN_DIALOG_DELAY` and the ~150 duplicated lines from each.
   Keep the shell-side gate on *whether it may* reveal (it must not steal a Settings window
   or overwrite a queued Password request) — that is genuine shell knowledge.
4. Verify on macOS: open a folder, open a password-protected archive, cancel a slow scan,
   quit mid-scan.
5. Only then start step 2 (archive-open), which carries the `SecretString` path in §6.
