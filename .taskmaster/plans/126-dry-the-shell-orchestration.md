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

## Handoff

**The single place cross-machine state lives** (`CLAUDE.md` → *Working across two machines*).
Read this first; write it last. Everything below this section is durable design rationale or
history — none of it is live state.

**Last updated:** 2026-07-20, **Windows session**, after the encrypted-archive smoke test.

### Windows smoke test — DONE, with two defects found and fixed

The step-2 winit shell had never been executed. It has now been, by the owner, against
`D:\Media\2002-password-is-test.7z` and `D:\Media\Pictures.zip`.

| *Not verified* item | result |
|---|---|
| 1 · wrong-password inline error | ✅ shows correctly |
| 2 · layout must not shift | ✅ stable |
| 7 · focus returns to the field | ✅ |
| 8 · no spinner flash on a fast open | ⚠️ see *boundary flash* below |

**Defect 1 — the "Checking…" spinner rendered under the button bar. FIXED.**
`set_password_error` clears `checking`, but `set_checking(true)` does **not** clear
`password_error` — so the ordinary retry path (wrong password → error shown → retype →
submit) had *both* live at once. Since `428b160c` made the error row always laid out, that
became two stacked rows in a fixed 500x250 window with a bottom button bar, and the spinner
overflowed underneath it. Collapsed into **one reserved status row** (checking wins, else the
error, else a transparent placeholder), height pinned to one text line via `allocate_ui` so
the row cannot resize as it changes state. Owner-confirmed: *"a much nicer design."*

⚠ **This is NOT the macOS untraced bottom-left spinner.** That one appears during a *quick
open* with no password dialog involved; this one only while a password is being verified.
Same description, different trigger — **the macOS one is still open and still untraced.**

**Defect 2 — boundary flash. FIXED by raising the gate, not by a minimum display time.**
`LOADING_DIALOG_DELAY` 250 → **500 ms**. An open landing at ~252 ms cleared the gate, painted,
and closed ~2 ms later.

- ⚠️ **Do not "fix" the residual flash with a minimum display duration.** Considered and
  rejected: it trades a few ms of blip for a real, deliberate delay on every slow-ish open,
  which is the wrong direction for this codebase. The rationale is in the constant's doc
  comment — read it before changing the value.
- The old comment argued this constant must stay equal to `SCAN_DIALOG_DELAY` (*"the same
  judgement about the same human"*). That is answered, not ignored: the pill is **ambient**,
  this is a **modal window**, and a higher bar for interrupting than for informing is
  principled. Both numbers now carry the reasoning.
- `archive_status_tracks_the_open` was advancing by `SCAN_DIALOG_DELAY` in an *archive* test
  and only passed because the constants were equal. Now pins its own delay, both sides of it.

**Worth knowing:** a plain `.zip` normally opens on the *synchronous* fast path where no
dialog is possible at all. It only goes async when `will_autotry` is true — i.e. **the session
password cache is non-empty**. So the flash was partly an artifact of testing an encrypted
archive first; on a fresh launch the same 8.5 MB zip shows nothing.

**Items 3 and 4 — owner-run on Windows 2026-07-20, both pass.** Session MRU auto-try works on
a second archive. Cancel mid-load on a large `.7z` closes quietly and keeps what was on screen.

**Decision recorded while checking 4:** a cancelled open does **not** promote its password to
the session MRU. The owner reasoned that backing out is not a request to remember, and that is
now the decision — but note it was *incidental* until today: `finish_archive_open` is the only
promotion site and a cancel never reaches it. Worth flagging because the code's own stated rule
(*"an archive that opened at all proves its password"*) argues the other way — 7z verifies
before bulk extraction, so a visible progress bar means the password was already accepted.
Pinned by `a_cancelled_open_never_remembers_its_password`, with
`a_completed_open_does_remember_its_password` as its other half, so a future "unify the terminal
paths" refactor cannot reverse it silently.

**Still not run on Windows:** *Not verified* items 5–6 (session MRU auto-try on a second
archive, Cancel mid-load on a large `.7z`, plain-`.zip` fast path from a *fresh* session, and
scan-over-archive supersession).

### Verified — what was actually run, and where

| what | platform | evidence |
|---|---|---|
| Step 1 (dir-scan) in the core | both | 867 tests in `pb-app-core` |
| Step 1 — macOS shell rewired, **−202/+82** | macOS | builds, runs, 36 `pb-mac-ffi` tests; owner smoke: unchanged, which **is** the pass condition for a pure-DRY move |
| Step 1 — winit shell rewired, **−270/+102** | Windows | native `clippy -D warnings` + tests on the self-hosted runner; owner smoke: builds, pill correct |
| Step 1 — winit pill covers the pre-bootstrap phase (UX change) | Windows | owner-confirmed |
| Linux gate (`clippy -D warnings` on `pb-app` + tests + release build) | Linux/docker | PASS, run locally from the Mac |
| Step 2 (archive-open) in the core | macOS | 9 tests; **not wired to either shell yet** |
| Empty-deck cancel restores the welcome hint | both | core test + the macOS follow-up in `8f69b709` |
| Step 2 — **macOS shell rewired, −216/+72** | macOS | 38 `pb-mac-ffi` tests; plain `.zip` (sync fast path) and encrypted `.7z` both open; owner entered the correct password and got in |
| Step 2 — **winit shell rewired** | Windows target only | `clippy --all-targets -D warnings` for `x86_64-pc-windows-msvc`; **never executed** |
| Wrong-password inline error (bug fix) | macOS | owner-confirmed; regression test fails with the fix reverted |
| Password error row no longer shifts the layout | macOS | owner-reported, fixed, host rebuilt |
| No spinner flash on a fast archive open | macOS | owner-reported, fixed (`LOADING_DIALOG_DELAY`), owner-confirmed "much better" |
| Focus returns to the password field after a rejection | macOS | owner-reported, fixed, owner-confirmed — retry without clicking |
| Wrong → wrong → correct password, end to end | macOS | owner-driven: retried until correct, archive opened |
| Privacy gates after the archive move | macOS | redaction tests, `no_outcome_variant_can_leak_a_password`, `settings.rs` has **zero** `archive_passwords` references, no-trace test green |

### Not verified — what the next machine must check

**All of this is Windows-side, and none of it is checkable from a Mac.** `pb-app` cannot even
be built here; everything below was type-checked via the cross-target compile only.

1. **Open an encrypted `.7z` and enter the WRONG password.** It must show *"Incorrect password.
   Please try again."* inline. Until 2026-07-20 it showed **nothing** — the field just cleared
   — for every archive that routes through the Loading dialog. Windows had this bug identically;
   only the synchronous-`.zip` path ever displayed the message.
2. **Same dialog: the layout must not shift** when that error appears. The row is now always
   laid out and only its colour changes (transparent when absent). Confirm nothing jumps and
   there is no odd gap when there is no error.
3. **Enter the correct password** — it should open, and a second encrypted archive with the
   *same* password should not ask again (the session MRU auto-try).
4. **Open a large `.7z` and hit Cancel** mid-load. It should close quietly, keep whatever was on
   screen, and show no error dialog.
5. **Open a plain `.zip`** — the synchronous fast path, which must open with no Loading dialog
   flash at all.
6. **Start a folder scan over an in-flight archive open, and vice versa.** The displaced worker
   is now cancelled by the *core*, not the shell; a stale result must never rebuild the deck.

7. **After a wrong password, focus should stay in the field** so you can retype immediately.
   winit already does this (`set_password_error` sets `focus_password`, `dialog.rs:914`) and was
   never broken — macOS was, and is fixed. Worth one confirming look, no code expected.
8. **A fast archive open must not flash the "Opening…" dialog at all.** It is now revealed
   only after `LOADING_DIALOG_DELAY` (250 ms), like the scan pill. Confirm a plain `.zip` shows
   no dialog, and that a slow `.7z` still gets one with a working Cancel.

If any of 1–2 or 7–8 look wrong, they are chrome-only and confined to `dialog.rs` /
`prompt_archive_password` / `reveal_slow_archive_open`.

**One loose end, macOS-side and cosmetic:** with the sheet suppressed, a small spinner now
appears at the bottom left during a quick open. The owner likes it, but nobody has traced what
draws it — `BlazeViewerMacApp.swift`'s overlay stack has no bottom-leading spinner, and the only
bottom-left-capable element is the info line (`infoLineAlignment` = 0), a metadata pill. Benign,
but it should be identified rather than relied on.

### Cross-platform debt — landed for one shell, still owed on the other

> ⚠ **The most dangerous category, because it is green.** A change can land, pass CI, and be
> *wrong on the platform that could not compile it*. `808c99f9` fixed the empty-deck hint "in
> the core so both shells get it" and pointed winit at it — but macOS kept its own copy and kept
> the bug, and nothing failed. **Any commit that touches shared code it cannot fully verify must
> add a line here.**

| owed | to | raised by | status |
|---|---|---|---|
| `cancel_scan_command` → the core's version | macOS | `808c99f9` (Windows) | ✅ paid by `8f69b709` |
| Wrong-password error + reserved error row — **egui half never run** | Windows | `e57fa033`, `428b160c` (Mac) | ⏳ **open** — see *Not verified* 1–2 |
| Delay-gated Loading dialog — **egui half never run** | Windows | `7bb9c4d3` (Mac) | ⏳ **open** — see *Not verified* 7 |

### Claimed — who is holding what right now

> Prevents the other failure mode: duplicate work. Both machines fixed the `AppCore` struct
> literal independently today, and the Mac session nearly rewrote `background.rs` from scratch
> as `ops.rs` because it started from a stale HEAD. **Fetch, read this table, then claim.**

| area | machine | since |
|---|---|---|
| *(released — step 2 shell migration landed)* | | |

The Mac session's claim is **released** as of `428b160c`. Both shells are migrated.

**Windows is now needed for a run only** — the full list is in *Not verified* above.

### Next

1. **Windows: run the checks in *Not verified*.** That is the only thing standing between step 2
   and done. No code is expected to change unless something looks wrong.
2. **Delete winit's dead Scanning dialog** — `DialogRequest::Scanning`, `DialogOutcome::
   ScanningCancelled`, and `dialog.rs`'s whole `Scanning` view. Nothing constructs it since
   `f3ca4795`; it is `#[allow(dead_code)]`-marked and the owner has confirmed the pill on
   Windows, so its revert-safety reason has expired. ⚠ One snag: `contract::DialogKind::Scanning`
   must **stay** (macOS uses it via `close_dialog_kinds`), so `shell_dialog_kind` /
   `contract_dialog_kind` in `main.rs` need a story once winit's own variant goes — they are
   currently total matches between the two enums. **Best done on Windows**, since it is
   `pb-app`-only and wants a run afterwards.
3. **Then task #126 is done.** The "strays" (`apply_menu_state`, `confirm_delete_permanent`, the
   two toggles) were deliberately scoped out in §5a and want their own task; `#125` (splitting
   `app_core_impl.rs`) is the separate follow-on, and is now *more* attractive because this task
   added ~400 lines to the core that #125 will sort into the right cluster once.

### Known-red, and NOT ours — do not chase

- `pb-decode`'s `plain_fixtures_have_no_dovi_summary` fails on both Windows runners with
  `FFmpeg decoder: Decoder not found`. Pre-existing: identical failure in run `29716604750`,
  hours before this task's first commit. It keeps the ffprobe lane red regardless of what lands.
- The `linux-gate` runner failed on a Docker **keychain** error, not on code — fixed 2026-07-20
  by dropping `credsStore` from `~/.docker/config.json` (the runner has no interactive session
  to unlock the keychain, and the gate only pulls public images).
- Two `pb-app-core` video-probe tests (`a_real_video_probes_off_thread…`,
  `copy_details_mid_probe…`) are **flaky**, not failing — timing-dependent off-thread probes
  that pass on an idle box.

---

## 0. HARD CONSTRAINT: neither machine can compile the other's shell

> ⚠ **Corrected 2026-07-20 — this section was written on the Windows box and read as a global
> truth when it is one-directional.** The asymmetry is real but it points *both* ways, and on a
> Mac it points the other way: `pb-mac-ffi` compiles, tests **and runs**, while `pb-app`'s
> `build.rs` hard-errors on `target_os = "macos"`. A Mac can therefore do *both* halves — the
> mac one natively, the winit one via the `x86_64-pc-windows-msvc` cross-check — which is why
> step 1 finished in a single session rather than waiting for a second machine. See §12.1.

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

## Appendix — session log (history, not live state)

Chronological. Kept for the *why* behind decisions; **status lives in `## Handoff` above**, and
these sections are deliberately not updated as work proceeds.

### Session 1 — 2026-07-19, Windows, unattended

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

### 11.6 Exact starting point for the macOS session ⛔ SUPERSEDED

> Historical. This was the Windows session's handoff; it has been executed and its premises
> partly corrected (§12.1–12.4). **Live next-steps are in `## Handoff` → Next.**

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

### Session 2 — 2026-07-20, macOS

Picks up from §11.6. Three of its five items are done; two of its premises were wrong and are
corrected below.

### 12.1 §0 is inverted on the Mac (and the position is better than §0 assumed)

§0 is a *Windows-box* statement, not a global one. On the Mac the asymmetry runs the other way:

| crate | on the Windows box | on the Mac |
|---|---|---|
| `pb-mac-ffi` | ⛔ compiled out (`#![cfg(target_os = "macos")]`) | ✅ compiles, tests, **and runs** |
| `pb-app` | ✅ compiles, tests, lints | ⛔ `build.rs` hard-errors on `target_os = "macos"` |

So **half B is the verifiable half here, and it is the only half that can actually be
executed.** Half A remains reachable via the `x86_64-pc-windows-msvc` cross-check (the
temporary blake3 `pure` + `ureq` `default-features = false` Cargo edits), which type-checks
but cannot run. Net: a Mac can do *both* halves, which is why step 1 completed in one session.

### 12.2 Two gaps in the handoff, found on arrival

- **`AppCore::begin_dir_scan` did not exist.** `dir_scan.rs`'s module docs describe it as "the
  thin production wrapper that makes the channel and spawns the walk" and `arm_dir_scan`'s
  doc-comment links to it, but only `arm_dir_scan` was written. The core owned the pump and
  none of the spawn, so **neither shell could have migrated**. Written in `a78bfddd`.
- **The audit's cross-cancel claim (subtask 4) is false at `HEAD`,** independently confirming
  `6bc8f7db`. macOS has cross-cancel in *both* directions with explicit `#109 item 1, winit
  parity — 8293a662` comments. Already closed; the audit is stale.

### 12.3 Correction: winit is NOT dialog-only. Both shells already have the same pill.

An earlier reading in this session claimed winit shows a modal Scanning dialog where macOS
shows an ambient pill, and called that a deliberate Mac-assed divergence. **That was wrong,
and the owner caught it.**

winit already has the same top-center pill, with a working hit-tested Cancel button
(`pb-app/src/panels_ui.rs:1287`), documented as a deliberate parity port of the macOS SwiftUI
`ScanPillView`. The `pb-hud` scan chip is dead code — reachable only from the dev gallery
(`hud_gallery.rs:144`, `:367`).

The **actual** divergence is much narrower, and is a gate, not a design:

> winit's pill is gated **post-bootstrap** (`main.rs:3314`:
> `displayed_item.is_some() && scan_bootstrapped && …`), so the modal `DialogKind::Scanning`
> window covers only the *pre-bootstrap* phase. macOS's pill covers both phases and its shell
> never reveals the sheet. The comment at `main.rs:3313` says so outright.

Owner decision (2026-07-20): **unify on the ambient pill.** That is mostly *deleting* winit's
special case — relax the gate, stop calling `open_scanning_dialog`, and port the dialog's
`Searching…` zero-state string so the pill does not read "0 found" for the whole pre-bootstrap
phase. That string is the one genuine UX regression risk in the change.

### 12.4 The P0 was scoped down, deliberately (owner call)

§11.6 step 2 called for stamping an `OpId` onto every dialog effect (~50 `pb-mac-ffi` sites
plus the Swift host) *before* moving dir-scan. Owner chose the narrow form: **reconcile the
two worker flows only.**

The reasoning: `AppCore::scan_status()` is a *described state*, not an imperative command, so
it removes the stale-close hazard **by construction** for this flow — there is no late `Close`
that can arrive after a newer walk started, because the core never issues one; it only ever
describes whatever operation is current now. The remaining dialogs (Settings, About, Confirm)
are synchronous and never had the hazard. Revisit the wider stamp after step 2, with evidence
from having moved both flows.

### 12.5 `slow` and `bootstrapped` must stay separate facts

The one non-obvious core design point. `BackgroundOps::should_reveal` **latches** (a modal
dialog is an *event*: open one, once). An ambient pill is a *state* and must be answerable
every frame — driven by the latch it would appear for exactly one frame and vanish. Hence
`BackgroundOps::is_slow` alongside it, and hence `ScanStatus` reporting `slow` and
`bootstrapped` independently rather than one "should I show something" boolean. That is
precisely what lets one core serve blocking chrome (hides once a photo is up) and ambient
chrome (stays for the whole walk).

### 12.6 The trap waiting in step 2

Wiring the mac shell's archive-open cancel to `begin_dir_scan`'s `superseded` return
**compiles, reads correctly, and silently breaks cross-type supersession** — because the
archive open is not registered in the core's generation space until step 2, so the core always
returns `None`. Caught immediately by
`beginning_a_folder_scan_supersedes_an_in_flight_archive_open`, the test that exists because
missing this cancel *is* the "door card over a photo" corruption.

The cancel is unconditional again, commented. **Step 2 must flip it to the `superseded`
return in the same commit that registers the archive open** — doing either half alone is a
silent regression in one direction or a double-cancel in the other.

### 12.9 Cross-check memo correction

`crates/pb-app/Cargo.toml` + `pb-app-core/Cargo.toml` need `ureq` at
`default-features = false` **plus `features = ["json"]`** — the recorded recipe omits the
second half, and without it `describe.rs:224` loses `send_json` and `pb-app-core` fails to
build for the Windows target before `pb-app` is even reached.
