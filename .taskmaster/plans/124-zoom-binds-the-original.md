# Task 124 — Smooth zoom binds the resident Original

**Status:** plan, rev 2 — Codex round 1 folded (1 new P0 found: the clobber paths, §3.6).
**Owner call needed on:** §7 (the on-demand want) and §9.

## The one-sentence version

The Fit↔1:1 toggle is already an instant rebind onto the resident full-res `Original`
(#106.7). **Smooth zoom is not** — it magnifies the fit-sized texture. Make smooth zoom
reuse the machinery the toggle already uses.

---

## 1. What is actually broken

Zoom is a pure vertex transform. All three mutators —

| entry | site |
|---|---|
| `+` / `-` keys, menu | `zoom_step` (`pb-app-core/src/app_core_impl.rs:2180`) |
| pinch / Ctrl+scroll | `zoom_about_cursor` (`app_core_impl.rs:3825`) |
| hold-to-zoom ramp | `apply_view_holds` (`app_core_impl.rs:11395`) |

— mutate `self.view.zoom`, then `push_view()` (`:8630`) → `Renderer::set_view`
(`pb-render/src/gpu.rs:3444`), which **only rewrites the 4-vertex quad**. UVs stay
`0.0..1.0` (`view.rs:48`). Nothing re-selects the texture.

The texture is selected by `display_slot` → `display_kind` → `display_rep` → `decode_fit`
(`app_core_impl.rs:6700 / 6677 / 6666 / 6628`), and `decode_fit` reads **`view.mode` alone**:

```rust
match self.view.mode { Fit => self.fit, Fill | Original => None }
```

`view.zoom` is never consulted, so in Fit mode zoom can never reach the ring's `Original`
slot — *even when #106.7 has already made it resident*.

**Impact.** 24 MP photo, 3840×2160 viewport, Fit, zoom 300%: ~1280×720 texels of the
~3840×2160 fit texture, magnified by a plain bilinear `mag_filter` (`gpu.rs:3133`; no
anisotropy, no `lod_clamp`) — roughly 1/9 the available detail. `MAX_ZOOM` is 32
(`view.rs:60`).

### 1a. Scope, corrected by the owner (2026-07-19)

The comment at `:6386-6392` ("a Fit↔1:1 toggle / zoom … is an instant rebind") was first
misread as an unimplemented claim. The owner, who wrote it, confirms **"zoom" there means
the Fit↔1:1 mode toggle**. The comment is accurate; nothing to fix there. This is an
unimplemented capability, not a regression. The corrected matrix:

| interaction | today | after |
|---|---|---|
| Fit↔1:1 toggle (`0`) | ✅ instant rebind onto `Original` | unchanged |
| **smooth zoom in Fit** | ❌ magnifies the fit texture | ✅ rebinds onto `Original` |
| smooth zoom in 1:1/Fill | ✅ `Original` already bound | unchanged |

Zoom past 100% in 1:1 is *correctly* showing all the data that exists — not a bug.

---

## 2. Why this is small: the expensive half already shipped

#106.7 keeps the *other* representation resident for the parked window
(`app_core_impl.rs:6386-6413`), precisely so a mode toggle is a rebind. `set_scale_mode`
(`:5476`) consumes it in five lines: flip `view.mode` → `display_slot(item)` now resolves to
the `Original` → `present_item` → return, **no epoch bump, no re-decode**.

Smooth zoom needs the same rebind under a different trigger. The pixels are already paid for.

---

## 3. Design

### 3.1 Do NOT make `display_kind()` zoom-aware

`display_kind` / `display_slot` / `decode_fit` have ~30 call sites, and most are on the
**decode/prefetch** path (`request_prefetch`, thumbs, the sharpen loop, `slot_bytes_estimate`,
`decode_is_definitive_full`). Making them zoom-aware would change decode targets for the whole
ring and the thumbnail strip — a shotgun change for a contained problem.

**Instead:** add a narrow *present-time* selector used only for the currently displayed item.
Decode targets stay mode-derived and unchanged.

```rust
/// Which representation the CURRENT item should be presented from, accounting for zoom.
/// Decode targets are unaffected — this is a display-time choice over what is already resident.
fn present_kind(&self, item: usize) -> pb_core::RepKind
```

Rules, in order:

1. Not `ScaleMode::Fit` → `display_kind()` (today's answer; Fill/Original already bind Original).
2. `view.zoom <= 1.0 + ZOOM_REP_EPS` → `RepKind::Fit`.
3. `Original` not resident for `item` → `RepKind::Fit` (graceful; §7 covers wanting it).
4. The resident `Original` is not genuinely larger than the resident `Fit` → `RepKind::Fit`
   (a small photo that never got downscaled: `downscale_to_fit` caps its scale at 1.0,
   `pb-decode/src/common.rs:319`, cap at `:327`, so both reps are the same pixels — switching buys nothing).
5. Otherwise → `RepKind::Original`.

### 3.2 The no-jump invariant (the load-bearing claim — now proved)

`base_scale` (`pb-render/src/view.rs:106`) is computed from the **bound texture's** dims:

- Fit rep: dims `(ow·k, oh·k)` where `k = min(sw/ow, sh/oh).min(1.0)` is the decode-to-fit
  scale. `base_scale_fit = min(sw/(ow·k), sh/(oh·k)) = (1/k)·k = 1.0`.
  `displayed = ow·k·zoom`.
- Original rep: dims `(ow, oh)`. `base_scale_orig = min(sw/ow, sh/oh) = k`.
  `displayed = ow·k·zoom`.

**Identical.** The swap is geometrically invisible at any fixed zoom. Two riders:

- **Rounding.** The decoder rounds to integer dims, so the *non-constraining* axis can differ
  by sub-pixel amounts (the constraining axis is exact: both reduce to `sw·zoom`). Bounded by
  ≤1 px. `PAN_DEADZONE_PX` is already 1.0 (`view.rs:131`), so it cannot flip a pan affordance.
- **`k == 1.0`** (source ≤ viewport): the two reps are identical, which rule 4 already excludes.

Pan is stored in **screen pixels** (`view.rs:69`), and `max_pan` derives from the invariant
`displayed_size` — so pan survives the swap untouched. This is why no geometry stash (#123) is
needed here.

### 3.3 ⚠ The trap: `present_item` resets the zoom

`present_item` (`:7593`) opens with `let view = self.view_for(item);`, and `view_for` (`:7008`)
sets `view.zoom = 1.0; view.pan = [0.0, 0.0]` unless a `compare_carry` is staged. **Calling
`present_item` mid-zoom would snap the user back to 100%** — the single most likely way to get
this wrong.

That reset is correct for its callers (landing on a new photo; `set_scale_mode` deliberately
zeroes zoom first). So add a sibling rather than weakening it:

```rust
/// Rebind the SAME item to a different resident representation, preserving zoom/pan/rotation.
/// Not `present_item`: that resets the view for a fresh landing (`view_for`), which would
/// cancel the very zoom that asked for this rebind.
fn rebind_same_item(&mut self, item: usize, slot: usize)
```

**There is already an in-tree precedent for exactly this**, and the new function should mirror
it rather than invent a shape. `try_gpu_sharpen` (`:5937`) carries the comment:

> Re-bind so the renderer picks up the sharp texture's dims — via `present_slot` directly,
> exactly like the CPU upgrade path: `present_item` would run `view_for` and RESET the user's
> zoom/pan (**and re-stamp `last_present`, skewing slideshow dwell**) — an in-place quality
> upgrade of the already-presented photo must not touch the view (Codex).

So the contract, corrected by Codex round 1:

**Must NOT** — call `view_for`; reset zoom/pan; **re-stamp `last_present`** (it drives slideshow
dwell — a rebind mid-slideshow would restart the timer); re-emit `SetTitle`; re-arm
`anim_hint_shown_for`; touch `resize_hold`; record the `present` metric (this is not an advance);
touch `displayed_item` (same item).

**Must** — apply the *current* view; `present_slot(slot)`; **check `present_slot`'s return value
before committing any state** (it returns `false` on a core↔renderer ring desync — `:7610`);
`ring.set_displayed(slot)` so the actually-displayed slot is eviction-pinned; update
`presented_kind`; issue exactly one `draw()`.

⚠ Codex notes `try_gpu_sharpen` itself omits the `ring.set_displayed` update and ignores
`present_slot`'s result. Mirror the *view* discipline, not those two omissions.

Note `ring.set_displayed` clears the #122 refusal latch on a *change*; here that is benign and
mildly helpful (a new residency decision is being made).

### 3.4 Rebind on decision *change*, not per frame

The hold-to-zoom ramp mutates `zoom` every tick. Track the presented kind and act only on a
flip; otherwise a zoom ramp would rebind at frame rate.

- New field `presented_kind: Option<pb_core::RepKind>`, set by `rebind_same_item`,
  `present_item`, and cleared wherever `displayed_item` changes.
- `ZOOM_REP_EPS` (≈ `1e-3`) gives a deadband at exactly 1.0, so a zoom that lands on 1.000001
  and back doesn't flap.

### 3.5 Where to call it

One shared reconcile, called **after** each zoom mutator has finished its own math:

```rust
/// Re-select the presented representation after a zoom change. Must run AFTER the zoom/pan
/// math: `zoom_about` reads the CURRENT texture dims via `placement()` to keep the anchor
/// pinned, so rebinding first would do that math against the wrong dims.
fn reconcile_zoom_rep(&mut self)
```

Call sites: `zoom_step` (`:2180`), `zoom_about_cursor` (`:3825`), and the zoom branch of
`apply_view_holds` (`:11395`) — each immediately before its existing `push_view()`/`draw()`.

Guards inside `reconcile_zoom_rep`:

- `displayed_item` is `Some` and equals `target_item` (don't fight an in-flight nav).
- `playback.is_none()` — a live video draws via `set_image`, not the ring (`:11726`).
  Zoom during playback must not rebind.
- Nothing held (`held_nav().is_none()`) is **not** required: zoom while blazing is not a
  thing, but the residency check in rule 3 already makes it a no-op.

### 3.6 ⚠ P0 (Codex round 1): three background paths clobber the choice

This is the finding that would have shipped a broken feature. Timer/tick-driven work rebinds the
**Fit** slot for the displayed item with no awareness that a zoom-driven choice exists:

| # | site | what it does | user sees while zoomed |
|---|---|---|---|
| 1 | `try_gpu_sharpen` (`:5942`) | `present_slot(dst)` where `dst` is the Fit slot | framing holds, **detail silently reverts to soft** |
| 2 | `try_gpu_derive_fit` (`:5834`) | `present_item(item, res.slot)` | **zoom snaps back to 100%**, recentres, Fit bound |
| 3 | `drain_results` CPU sharpen landing (`:8271`) | `present_slot(slot)` on the landed Fit | framing holds, **detail reverts to soft** |

Path 2 is the worst — `present_item` → `view_for` resets zoom/pan mid-gesture. Its comment at
`:8269` ("`present_slot` keeps the current view, so any zoom/pan is preserved") is true about the
*view* and silent about the *representation*, which is exactly the gap.

All three fire while **parked**, which is precisely when the user is zooming. `sharpen_candidate`
(`:6744`) requires the Fit rep to be a resident *preview* with an Original also resident — a
common state right after landing on a photo, and the same state that makes rung 0 work.

**The fix (Codex option ii, agreed): derive, but do not rebind while `Original` is the presented
kind.** Not option (i) "skip the derive" — the derived Fit is still wanted for when the user zooms
back out, and throwing it away wastes work already done. Not option (iii) "rebind then re-apply" —
pointless churn and a visible intermediate frame.

The guard must test the **authoritative `presented_kind`**, never `display_kind()` (which still
says `Fit` in Fit mode — that is the whole premise of §3.1) and never bare `zoom > 1.0` (which is
wrong when no Original is resident, and would break under any future hysteresis).

**The general rule to write into the code, because it generalises past this task:**

> Background/timer work may change **residency** or **quality**. It must never choose the
> **presented representation**. That choice belongs to user state alone.

Concretely, funnel same-item rebinds through one resolver that consults `present_kind`, instead of
bolting an `if` onto each of the three sites — otherwise the fourth site added next year
reintroduces this.

**Also flagged (P2), not in scope:** `restore_still` (`:11733`, Live Photo revert) picks its slot
via `display_slot` + `present_item`. It runs when a Live Photo finishes, so it is only reachable on
a motion item and cannot collide with a zoomed still today. Note it, do not fix it here.

---

## 4. Also fixed for free: zoom-out aliasing

Fit reps are uploaded `mip_level_count: 1`; only `Original` reps get a mip chain
(`gpu.rs:1898` `do_mip` / `:1913` `mip_level_count`). So minifying in Fit mode today is unfiltered. Binding the Original
past zoom 1.0 doesn't address zoom < 1.0, and **this plan deliberately does not** — see §9.

⚠ Verify before relying on it: source-ICC (mode 1) images are documented as *not* mipped
(`gpu.rs:1897-1898`: `do_mip = mip.is_some() && mode != 1.0`). If an Original can be mip-less, §3.1 rule 5 is still correct (more
texels is still better at magnification), but don't advertise a filtering win for those.

---

## 5. What this does NOT do

- **No new decode.** Rung 0 is strictly "bind what is already resident."
- **No ROI/crop decode.** That is the separate rung 2 (task #124 subtask 5). Decode-to-purpose
  today means `FitBox` — a *scale*, not a *crop*.
- **No change to blazing.** The parked tier is empty while a nav key is held (`:6402`).
- **RAW, SVG, video/doors, >gigapixel get nothing**, because `full_res_eligible` (`:6969`)
  excludes them from the parked tier. This is pre-existing and consistent: their 1:1 toggle
  isn't instant today either. RAW is the painful one (uncancellable demosaic) and is exactly
  where zoom detail matters — call it out in the task, don't silently ship a partial win.

---

## 6. Files touched

| file | change |
|---|---|
| `pb-app-core/src/app_core_impl.rs` | `present_kind`, `rebind_same_item`, `reconcile_zoom_rep`, `presented_kind` field, 3 zoom call sites, **+ the §3.6 guard at 3 clobber sites** |
| `pb-app-core/src/app_core.rs` | the `presented_kind` field decl |
| `pb-render/src/view.rs` | tests only (§8 invariant) — no production change |

No `pb-render` production change, no shader change, no new GPU state.

---

## 7. Optional, owner call: an on-demand Original want

When zoom > 1 and no Original is resident (radius 0, or a photo the tier hasn't reached),
today's answer is "stay soft forever." A bounded escalation: treat *zoom itself* as an
explicit want for **this one item**, so the parked tier requests its Original even at radius 0.

Arguments for: the user just made an unambiguous "I want detail here" gesture; it is one job,
refuse-before-reserve already applies, and it is cancellable like any other.
Arguments against: radius 0 is a deliberate VRAM opt-out, and overriding a user setting from an
unrelated gesture is presumptuous.

**Recommendation: ship §3 first without this, measure how often rule 3 actually misses, then
decide.** Do not bundle it — it is the only part of this plan that changes *decode* behavior,
and bundling it would put the one risky bit inside the safe change.

---

## 8. Tests (written first, per the house rule)

Pure, no GPU:

1. `present_kind` truth table — (mode, zoom, residency, relative rep sizes) → expected kind.
   Covers all five rules, including rule 4's equal-size case.
2. **The no-jump invariant, in `pb-render/src/view.rs`**: for a spread of source dims, viewport
   dims and zooms, `displayed_size(fit_dims…)` == `displayed_size(orig_dims…)` within 1 px.
   This is §3.2 as an executable claim and is the test that must exist before any code.
3. **The zoom-preservation regression**: rebinding must not reset zoom/pan (the §3.3 trap).
   Assert `view.zoom` is unchanged across a `reconcile_zoom_rep` that flips the kind.
4. Rebind-on-change-only: N successive zoom steps past the threshold produce exactly **one**
   rebind (guards the §3.4 per-frame churn).
5. Decode targets unchanged: `decode_fit()` / `display_rep()` / the `request_prefetch` job list
   are byte-identical before and after a zoom that flips `present_kind` — the explicit pin on
   §3.1's "don't disturb the decode path."
6. Guards: no rebind while a video plays; no rebind when `target_item != displayed_item`.
7. **§3.6 clobber regressions — one test per path, and these are the tests that matter most:**
   zoomed onto the Original, then (a) `try_gpu_sharpen` fires, (b) `try_gpu_derive_fit` fires,
   (c) a CPU full lands in `drain_results`. In all three: `view.zoom` unchanged **and** the
   presented slot is still the Original. Each must fail against a build with §3.6 omitted.
8. `last_present` is NOT re-stamped by a rebind (the slideshow-dwell rider in §3.3).
9. `present_slot` returning `false` leaves `presented_kind` uncommitted (no state on a desync).

Property test: `present_kind` is monotonic in zoom (once Original is chosen, more zoom never
reverts to Fit at the same residency) and never returns a kind that is not resident.

Golden-image (pb-render, headless + nv-flip): a known photo at 300% in Fit mode must match a
reference rendered from full-res within tolerance. **Today's build must fail this** — that is
how we know it tests the bug.

Measurement (`PB_PERF`): add a zoom episode — `zoom input → back on screen` (must stay within
one refresh; it is a rebind) and `zoom-stop → max quality` (currently unbounded in Fit mode).

---

## 9. Deliberately out of scope

- Rung 1, a better magnification kernel (Catmull-Rom/Lanczos over bilinear). Independent of
  this change, wants its own `ab_report` A/B/X round, and would muddy this one's measurement.
- Rung 2, ROI decode on zoom-stop.
- Mips for Fit reps (would fix zoom-out aliasing in Fit mode generally).
- **macOS is unverified.** The Renderer trait seam (`pb-render/src/lib.rs:229`) suggests the
  Swift host shares `set_view`, but the mac zoom path was not audited. Since all logic here
  lands in `pb-app-core`, the Mac *should* inherit it — confirm, don't assume, and file a gap
  if its zoom is implemented host-side.

---

## 10. Risk register

| risk | severity | mitigation |
|---|---|---|
| `present_item`'s `view_for` resets zoom (§3.3) | **breaks the feature outright** | dedicated `rebind_same_item`; test 3 |
| Rebind storm during a hold-to-zoom ramp | perf | change-detect via `presented_kind`; test 4 |
| Decode-path disturbance from a display-path change | wide regression | present-time selector only; test 5 |
| Sub-pixel jump on the non-constraining axis | cosmetic | proved ≤1 px; below `PAN_DEADZONE_PX`; test 2 |
| Rebinding a slot the renderer hasn't uploaded | dropped frame | `present_slot` returns false and holds the previous frame (`:7610`); residency is checked first |
| Zoom during video playback | wrong texture | explicit `playback.is_none()` guard; test 6 |
| **Background sharpen/derive/drain rebinding Fit over the zoomed Original (§3.6)** | **P0 — ships a feature that silently undoes itself** | one resolver consulting `presented_kind`; tests 7a/b/c |
| Rebind re-stamping `last_present` | slideshow dwell resets | explicitly omitted; test 8 |
| `present_slot` desync leaves `presented_kind` lying | wrong texture believed bound | commit state only on `true`; test 9 |
