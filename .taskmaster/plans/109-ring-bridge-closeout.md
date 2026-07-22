# Task 109 — Ring-bridge close-out: fail at the divergence, not a tick later

**Status:** DRAFT rev 2 (2026-07-21) — **Codex round-1 folded in** (see §9). Not started. Remediation
for **technical-debt audit finding #3**
(core↔renderer ring/deck fragility) — the *durable* close-out of the bug class whose acute symptom is
the "door card frozen over a photo / title advances but the view is stale" corruption (filed as **#132**).

**What already landed (do not redo):**
- **#109.3** (deck-identity generation) — via **#119**: the core `ResidentRing` (`pb-core/src/ring.rs`)
  is identity-aware — a slot carries `{ item, content_gen, representation }` and a completion is
  rejected unless `(item, content_gen, rep)` still matches the reservation. `DecodeKey`/`Validity`
  domains enforce staleness at pool-cancel, ingestion, ring-rebuild, and drain.
- **#109.4** (fail-loud fill) — `upload_slot` returns `bool` (`pb-render/src/gpu.rs:3688`) with a
  loud refusal + reservation rollback; `mark_resident` runs only after a successful upload.

**What remains (this task):** the renderer ring is still **identity-blind**, and `present_item` does
**not** propagate its miss — so a core↔renderer drift is invisible at the bind and only self-heals a
tick later (or not at all — #132). Three pieces below.

## 0. The one-paragraph diagnosis

The **core** mirror (`ResidentRing`) knows which `(item, content_gen, rep)` sits in each slot. The
**renderer** ring does not: `RingSlot` is `{ bind_group, w, h, peak, texture, was_clamped, mode,
content_hdr }` (`pb-render/src/gpu.rs:2472`) with **no item/gen**, and `present_slot(slot)`
(`gpu.rs`, the `fn present_slot` above `upload_slot`) binds **whatever texture is in `ring[slot]`**,
trusting the caller's index. When a deck mutation moves what index N *names* without the renderer ring
following (the cross-deck/stale-batch race — `apply_scan_batch`, `app_core_impl.rs:961-973`),
`present_slot` returns **`true` with the wrong occupant**: the title/`displayed_item` say photo, the
bound texture is the door (or vice-versa). That is #132. The fix is to **stamp the renderer slot with
the identity the core ring validated it against and verify at the bind** (fail at the divergence), and
to **propagate a refused present** so the drain repairs instead of trusting a lie.

⚠ **The subtle part (Codex r1, §9.1):** the identity to stamp is **not** "read `self.content_gen` at
upload time." The race that produces #132 — a stale folder-scan batch calling `extend_playlist`, which
swaps `self.source` **without bumping `content_gen`** — would upload pixels whose `content_gen` *equals*
`self.content_gen`, so a naive stamp would **match and mislabel wrong pixels as fresh** (a false
negative). The stamp must come from the **accepted outcome's reservation identity** — the exact
`(item, content_gen, rep)` the core `ResidentRing` already validates a completion against
(`pb-core/ring.rs`) — carried on the decode outcome, not re-derived from live core state at bind time.
Piece C then has to guarantee no *legitimate* stamp-consistent-but-wrong-deck swap slips through.

⚠ **Do NOT reintroduce the invalidate-on-miss self-heal** (`cff70ca0` / `c383107a`): it bumped the
epoch *mid-`drain_results`* and purged the retained full-res tier, regressing instant fullscreen to a
preview flash. It was deliberately reverted; `present_slot`'s `false` is a diagnostic today, never a
control-flow branch. The resync in Piece B runs **once after the loop**, never mid-loop.

## 1. Current-state anchors (verified 2026-07-21 — re-grep before editing)

- `pb-render/src/gpu.rs:2472` `struct RingSlot` — identity-blind (no `item`/`content_gen`).
- `pb-render/src/gpu.rs` `fn present_slot(&mut self, slot) -> bool` — index-only bind; returns `false`
  only when the slot is empty/not-yet-uploaded, never on a wrong-occupant.
- `pb-render/src/gpu.rs:3688` `fn upload_slot(...) -> bool` — the write path to stamp at (109.4).
- `pb-app-core/src/app_core_impl.rs:3512` `present_item(&mut self, item, slot)` — returns `()`; a
  local `presented` bool feeds a **diagnostic-only** `eprintln` (`:3537`), no propagation. **#109.5.**
- `pb-app-core/src/app_core_impl.rs:3113` `present_slot_for(item)`, `:3701` `try_present_target`,
  `:3927` `drain_results` — the callers that know the intended `item` and must thread it through.
- `pb-app-core/src/app_core_impl.rs:3134` `rebind_same_item` — the **legitimate** in-place rep swap
  (#124); must stay green (same `item`, same `content_gen`, different `rep`) — the stamp check must
  not refuse it.
- `pb-app-core/src/app_core.rs:219`/`:538` `content_gen`; `app_core_impl.rs:4484`
  `self.content_gen = self.content_gen.wrapping_add(1)` (in `invalidate_content`).
- `pb-app-core/src/app_core_impl.rs:921` `apply_scan_batch`, `:947` the `BOOTSTRAP` branch (the
  "mode B" hole), `:961-973` the existing cross-deck extend-guard + rationale.
- `pb-app-core/src/background.rs:64` `BackgroundOps` — the **already-shared generation space** across
  every `OpKind` (`begin`/`active`/`is_current`); `poll_archive_load`/`poll_dir_scan` already gate on
  `bg.is_current(id)`. This is the substrate for #109.2, not a from-scratch build.
- `pb-core/src/ring.rs:72-95` the identity-aware core slot + reservation (what to mirror).

## 2. The three pieces

### A. RingSlot identity stamp + verified present (the load-bearing fix)

- **Stamp the full core identity `(item, content_gen, rep: Representation)`** on the renderer
  `RingSlot` — not just `(item, content_gen)`. Including `rep`/geometry catches a stale-size texture,
  and a legitimate Fit↔Original swap still passes because the destination slot carries the *expected*
  representation (`rebind_same_item` requests the slot for the rep it wants).
- **Stamp from the reservation identity, carried on the outcome — never re-read `self.content_gen`
  at upload time** (§0 / §9.1). The value written is the `(item, content_gen, rep)` the core
  `ResidentRing` validated the completion against; a swap-without-bump therefore cannot mint a
  matching stamp for wrong pixels.
- **Stamp EVERY renderer writer, and preserve stamps on every remap** (Codex r1, §9.1). Audit all
  constructors of `RingSlot` and all ring mutations, not just `upload_slot`: e.g. the GPU
  `derive_fit` path builds a `RingSlot` directly, and `invalidate_geometry`'s retain-and-remap must
  carry each slot's stamp across the rebuild. A writer that forgets to stamp defeats the whole check.
- Change `present_slot(slot)` → `present_slot(slot, expected: SlotIdentity) -> bool`: after fetching
  the slot, compare its stamp. On **mismatch**, return **`false`** (refuse the wrong-occupant bind;
  the renderer keeps the held frame — a stale-but-correct photo beats a wrong one) **and** emit the
  loud stderr diagnostic. A `debug_assert!` is **fine only where a mismatch is a genuine invariant
  violation** — but see §4: the refuse-path test must run in a build where the assert does **not**
  fire, so gate the assert (`#[cfg(not(test))]` or a `strict_ring` cfg) or assert a recorded diagnostic
  counter instead of panicking.
- Thread `expected: SlotIdentity` from every core bind site (`present_item`, `rebind_same_item`, the
  `try_gpu_*` upgrade paths). Each already knows its `item`; the `content_gen`/`rep` come from the
  **residency record** for that slot, not from live `self.content_gen`.
- **Headless (`renderer = None`, unit tests)** keeps counting as bound (matches today), so pure-core
  assertions hold.

**Tests:** a `pb-render` headless test that uploads identity X to a slot then calls
`present_slot(slot, Y)` and asserts it **refuses** (returns false, keeps the prior frame) rather than
binding X as Y — in a build where the assert is disabled (§4). A `pb-app-core` test that a deck swap
under the ring makes the next present refuse. **Plus (Codex r1):** a `derive_fit`-stamps-correctly test
and an `invalidate_geometry` **remap-preserves-stamps** test (the two writers most likely to drift).

### B. #109.5 — present-result propagation + repair (must land WITH A — §3)

⚠ **Codex r1 (§9.2) found a liveness hole that makes B mandatory-with-A:** A refusal is only safe if it
**commits no state** and **guarantees eventual recovery**. Today `present_item` calls `set_view`,
`ring.set_displayed`, `mark_resolved` (→ `displayed_item`/`presented_epoch`/title/kind) *before/around*
the bind — so a refused present that still ran those would strand the viewer on a wrong `displayed_item`
with the pump asleep. **A without B converts an intermittent glitch into a permanent hold.**

- **Atomic present — commit nothing until the bind succeeds.** Reorder `present_item` so the
  `present_slot` verification runs **first**; only on success does it `set_view` /
  `ring.set_displayed` / `mark_resolved` / emit the title / record `presented_kind`. On refusal it
  mutates **no** core-visible state (in particular `set_view` must not have already reframed the held
  prior frame — make view application part of the success path).
- **`present_item` returns `bool`.** Update the callers (`try_present_target:3701`; the `drain_results`
  present arms `:3907`/`:4040`/`:4354`; the derive/sharpen re-present paths) to observe it.
- **On a refusal, guarantee recovery (the minimal liveness contract):**
  - **invalidate the exact residency record** for that `(item, slot)` in the core mirror (a
    bridge-only reset — **no** `content_gen`/`epoch` bump), so it is no longer considered resident;
  - keep the **target unresolved** (`target_caught_up()` stays false) and the **pump awake** — fold
    `target_pending()` into `work_pending()` if a bare refusal would otherwise let the pump sleep;
  - **re-`request_prefetch`** the target so a fresh, correctly-stamped decode is scheduled;
  - retry on subsequent ticks **until a verified bind or a terminal decode failure** (`failed` set /
    `present_failed`) — the loop cannot spin forever because each pass either binds, re-decodes, or
    terminates.
- **"Once after the loop" = one *coalesced repair*, not one final bind attempt** (Codex r1): during a
  multi-item drain, collect the refusal(s) and run the repair **once** after the loop; recovery then
  proceeds over the following ticks under the liveness contract above. Never invalidate mid-loop (the
  reverted `cff70ca0`/`c383107a` repair).

**Tests:** (1) a refused present commits **no** state — `displayed_item`/`presented_epoch`/title/`view`
are unchanged after a refusal (the non-mutation guarantee). (2) A refused present during a multi-item
drain triggers **exactly one** coalesced repair, no `content_gen`/`epoch` bump. (3) **Eventual
recovery** — after a refusal + a fresh correctly-stamped decode lands, the correct item presents and
`target_caught_up()` becomes true (proves no permanent hold).

### C. Mode-B hole — provenance-gate the scan batch (correctness-critical, NOT belt-and-suspenders)

⚠ **Codex r1 (§9.3) corrected my "A makes this moot" claim — it does not.** A refuses a *stamp
mismatch*, but a stale `BOOTSTRAP` **rebuilds both rings, bumps `content_gen`, and uploads the folder
deck** — so every stamp is **internally consistent** while the archive deck was wrongly superseded. The
stamp check sees nothing wrong; the user is silently thrown out of the archive. So C is a real
correctness fix that A does not cover.

- **The naive `archive_scope.is_some()` guard is UNSAFE** (Codex r1): a *legitimate* user-initiated
  folder scan started from within an archive still has `archive_scope` set until its own first batch
  lands — the guard would reject the very scan the user asked for. Do not gate on `archive_scope`.
- **Gate by operation provenance instead.** Carry the originating `BackgroundOps` `OpId` on the scan
  result and apply a batch only when `bg.is_current(op_id)` — i.e. it belongs to the scan the core
  still considers live. A superseded/cross-type scan's batch is dropped; a legitimate archive→folder
  scan (which *is* the current op) applies. This is the audit's **#109.2**, done right.
- ⚠ **Not as small as rev-1 claimed** (Codex r1): `crate::scan::Resolved` currently carries **no
  generation**, so this means threading the `OpId` from `arm_dir_scan` → the worker → `Resolved` →
  `apply_scan_batch`. Alternatively, make batches applicable **only** through the already-validated
  `poll_dir_scan` path (which already checks `bg.is_current`) so `apply_scan_batch` can never be
  reached with a stale batch — assess which is the smaller, safer change during implementation.

**Tests:** a stale (superseded) scan batch is **dropped** by `apply_scan_batch`; a legitimate
**archive→folder** open (scan is the current op, `archive_scope` still set) is **applied**, not
rejected — the exact case the naive guard would have broken.

## 3. Sequencing

⚠ **A and B ship together** (Codex r1, §9.2): **A alone converts corruption into a permanent hold** —
it refuses the wrong-occupant bind but, without B's recovery contract, leaves the viewer stranded on a
held frame. Land them as one reviewed unit (a two-commit sequence is fine, but **do not push A without
B**; if split, keep A behind the recovery path so no intermediate state is shippable).

1. **A + B together** — the identity stamp + verified `present_slot` **and** the atomic-present +
   refusal-recovery contract. Test-first: the refuse test *and* the eventual-recovery test are the
   gate.
2. **C** (provenance-gate the scan batch) — closes the mode-B / cross-deck hole A cannot see. Can land
   as its own commit after A+B, but it is **not** optional — it is a distinct correctness gap.

One commit per landing unit so a bisect stays clean; A+B is one unit.

## 4. Test-first + hot-path discipline (this is the 120 Hz path)

- **`present_slot` gains a stamp compare** — two integer compares, no alloc, no branch on the happy
  path (match → proceed). Still: **measure** `present` (the keypress fast-path metric, `--metrics`)
  p50/p95/p99 **before/after** on the corpus; the acceptance bar is *flat within noise*. If a compare
  ever shows up, it is a data-layout problem, not a reason to skip the check.
- **No per-frame heap allocation** on the bind path (house rule). The stamp is `Copy` scalars.
- **The legit rep-swap must stay green:** `rebind_same_item` (#124) swaps `rep` at the **same**
  `(item, content_gen)` — the stamp check must pass it. Add/keep a test.
- **Golden-image** (headless WARP, no GPU) for the renderer refuse path; **pure `pb-core`/
  `pb-app-core`** unit tests for the drain resync (deterministic, no GPU).
- ⚠ **The refuse test cannot also trip a `debug_assert!`** (Codex r1): a mismatch that `debug_assert!`s
  would panic the very test that wants to observe the `false` return. Gate the assert (a `strict_ring`
  cfg or `#[cfg(not(test))]`) so tests exercise the release **refuse-and-recover** behaviour, and cover
  the assert-fires path separately (e.g. `#[should_panic]` under the strict cfg) if kept at all.
- **The reverted repair stays reverted** — assert no `content_gen`/`epoch` bump happens inside
  `drain_results`.

## 5. Cross-machine (both shells present through this path)

`pb-render` + `pb-core` + `pb-app-core` are shared; the winit shell (Windows/Linux) and the macOS
native host both present through the same `present_slot`. So the change is **shared-code** and
half-verified by construction on whichever machine lands it:

- The macOS host renders the still via the same `Renderer`/ring, so the stamp check runs on Metal too.
- Land + fully verify on one shell; the other runs the cross-check (`cargo clippy -p pb-app
  --target x86_64-pc-windows-msvc` from the Mac, or the mac build from Windows-can't) and a real run.
- Leave a `## Handoff` with **verified/not-verified/claimed**, per `CLAUDE.md`. The renderer change is
  the risky half — a golden-image test that runs in CI on WARP/lavapipe is the portable safety net.

## 6. What this retires

- **Finding #3** (ring/deck fragility) — converted from "silently drift, self-heal a tick later" to
  "fail at the divergence." The last structural item of the audit's high-ROI sequence (item #2).
- **#132** (door-card wrong-occupant race) — the durable fix: **A+B** kill the wrong-occupant *bind*
  (and recover), **C** closes the stale-batch *silent-eviction* variant A can't see. **Verified by the
  invariant/recovery tests**, not the flaky manual repro. Once landed, re-check #132 against a
  big-still-scanning-folder repro if one can be captured; either way the tests are the proof.

## 7. Explicit non-goals

- No epoch/`content_gen` bump inside `drain_results` (the reverted repair).
- No change to the retained full-res `Original` tier / instant-fullscreen behaviour (#106.7 / #124).
- Not the `RingSlot` → typed-representation redesign; this is an additive stamp + a verified bind, not
  a ring rewrite. `ResidentRing` (the core structure) is explicitly on the audit's "leave alone" list.
- Not #1c (accessor discipline) or #4 (routing seam) — separate findings.

## 8. Risks

| risk | severity | mitigation |
|---|---|---|
| Stamp check refuses a **legitimate** rep swap (`rebind_same_item`) | high — regresses zoom/1:1 | §4: compare on `(item, content_gen)` only, not `rep`; keep the #124 rebind test green |
| Hot-path cost on the keypress present | medium | §4: 2 scalar compares, measured `present` p50/p95 must stay flat |
| Resync-once re-introduces the mid-loop epoch bump | high — the reverted regression | §2B: resync strictly **after** the loop; a test asserts no `content_gen`/`epoch` change in-drain |
| Over-scoping into a ring rewrite | medium | §7 non-goals: additive stamp only; `ResidentRing` untouched |
| Shared-code change unverified on the other shell | medium | §5 Handoff + the CI golden-image test as the portable net |
| **A shipped without B → permanent hold** (Codex r1) | **high** | §3: A+B are one landing unit; refusal-recovery contract (§2B) proven by the eventual-recovery test |
| **Stamp minted from live `content_gen`** matches wrong pixels (swap-without-bump) | high | §2A/§0: stamp from the outcome's reservation identity, never `self.content_gen` at bind time |
| **Mode-B silent archive-eviction** (stamps stay consistent) | high | §2C: provenance-gate by `OpId`, not the unsafe `archive_scope` guard |
| A renderer writer forgets to stamp (`derive_fit`, remap) | medium | §2A: audit every `RingSlot` constructor + remap; dedicated tests |

## 9. Codex review (2026-07-21, round 1 — folded in)

Reviewed the rev-1 plan with the plan + ground-truth code inlined (`RingSlot`, `present_slot`,
`present_item`, the core `ResidentRing` identity model, `apply_scan_batch`). Three focused questions;
all three surfaced real holes, now folded above:

1. **Piece A was incomplete.** `(item, content_gen)` catches drift *only if every change to what index
   N means bumps `content_gen`* — but the actual #132 race (`extend_playlist` swapping `self.source`
   **without** a bump) mints a *matching* stamp on wrong pixels (false negative). Fix: stamp from the
   accepted **outcome/reservation identity**, not live `self.content_gen`; mirror the **full
   `(item, content_gen, Representation)`**; and **stamp every renderer writer** (`derive_fit`, remaps),
   not just `upload_slot`. (§0, §2A)
2. **Piece B had a liveness hole — and makes B mandatory-with-A.** A refusal that still commits
   `set_view`/`set_displayed`/`mark_resolved`/title, or that leaves the core thinking the target is
   resident, strands the viewer forever with the pump asleep — **A alone turns corruption into a
   permanent hold.** Fix: atomic present (commit only on a verified bind), invalidate the exact
   residency record on refusal (no epoch/gen bump), keep the target unresolved + pump awake
   (fold `target_pending()` into `work_pending()`), re-request, and retry until a verified bind or
   terminal failure; the post-loop resync is **one coalesced repair**, not one final attempt. (§2B, §3)
3. **Piece C is correctness-critical, not belt-and-suspenders.** A stale `BOOTSTRAP` rebuilds both
   rings consistently (all stamps match) while wrongly evicting the archive deck — invisible to A. And
   the naive `archive_scope` guard would reject a legitimate archive→folder scan. Fix: **provenance-gate
   by `OpId`** (`bg.is_current`), threading it onto `Resolved` (which carries no generation today — so
   not the trivial patch rev 1 implied) or routing all batches through the validated `poll_dir_scan`.
   (§2C)

Also folded: the `debug_assert!`-vs-`assert-false-return` test contradiction — the refuse test runs
with the assert gated off (§2A/§4); and the extra tests Codex named (`derive_fit` stamp, remap
preservation, refusal non-mutation, async eventual recovery, legit archive→folder open).
