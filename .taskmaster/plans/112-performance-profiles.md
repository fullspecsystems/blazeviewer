# 112 — Performance profiles: hardware-sized budgets (Safe / Normal / High)

_Status: **DESIGN DRAFT rev 2** (2026-07-19) — Codex round-1 findings folded in (see the review
log at the bottom); round 2 + owner sign-off pending. **Do not implement** until the owner
green-lights the design._

## The ask (owner, 2026-07-19)

The comfort budgets are hardcoded for a modest machine ("don't hurt a 6 GB laptop"), but the
owner's box has an RTX 5090 (32 GB VRAM) and "insane gobs" of system RAM sitting idle. The
observable symptom that started this: blaze forward, flip direction, and the just-passed photos
are already evicted — the 1.5 GB ring only keeps ~5 photos behind you on the 7680×2160 display.
Owner wants **a few performance profiles (safe / normal / high or similar)**, floated
auto-detection, and chose the UI: **a slider on the General settings page**.

## Design principle: detection is the foundation, profiles are the fraction

"Auto" answers *what the machine can give*; a profile answers *how much of it the user hands
over*. Those are different questions, so there is **no separate "Auto" entry** competing with the
tiers. Instead, hardware detection runs under every profile, and each profile is a **fraction of
detected resources** with caps:

- **Safe** — the budgets Blaze Viewer has always shipped, or less on very small GPUs. For "I run
  this beside Lightroom and forty Chrome tabs."
- **Normal (default)** — today's *fraction* generalized. On a 6 GB laptop it computes to today's
  numbers; on a 32 GB card it grows to match. Most users never open the setting and still get the
  right answer for their machine.
- **High** — the beast tier: most of the GPU, deliberately. The settings description carries the
  honest caveat.

Detection failure (or a platform without a detector yet) falls back to **exactly today's
constants** — the fallback IS the current shipping behavior, so nothing can regress by accident.

## What scales (the comfort budgets)

Units: **GiB throughout** (the shipped constants are decimal-ish — 1 500 000 000 B ring,
512 MiB pool — the formulas below supersede them; the fallback keeps the constants verbatim).

| Knob | Today | Safe | Normal | High |
|---|---|---|---|---|
| VRAM ring (`RING_BUDGET_BYTES`, engine.rs:38) | 1.5 GB const | min(15% V, 1.5 GiB) | min(25% V, 12 GiB) | min(45% V, 20 GiB) |
| Ring slot cap (`ring_capacity` clamp, engine.rs:324) | 64 | 64 | 128 ⚠gated | 256 ⚠gated |
| Decode pool RAM (`POOL_BUDGET_BYTES`, engine.rs:41) | 512 MiB const | 512 MiB | clamp(R/32, 512 MiB, 2 GiB) | clamp(R/16, 1 GiB, 8 GiB) |
| Parked full-res radius clamp (`full_res_radius`, today ≤3) | 3 | 3 | 4 | 8 |

V = detected dedicated VRAM (further bounded by the WDDM headroom model below — the fraction is
the *request*, the headroom model is the *enforcement*). R = physical RAM, **additionally bounded
by available/commit headroom at sizing time** (finding 9): the pool formula takes
`min(formula, available_ram / 2)` so a RAM-pressured machine gets a smaller pool than its total
suggests. Worked examples (ring):

| Machine | Safe | Normal | High |
|---|---|---|---|
| 6 GB laptop dGPU | 0.9 GiB | 1.5 GiB (= today) | 2.7 GiB |
| 12 GB midrange | 1.5 GiB | 3 GiB | 5.4 GiB |
| RTX 5090 (32 GB) | 1.5 GiB | 8 GiB | 14.4 GiB |

On the owner's display (66 MB/slot fit textures) Normal takes the ring from 22 resident photos to
~121, High to ~218 — both **byte-budget-bound** (218 < the 256 slot cap; the slot cap binds only
when slots are small, e.g. over RDP, which is exactly why it scales with the profile).

- **`full_res_radius`**: the profile moves the *clamp*, not the user's chosen value. Whether High
  should also raise the *default* radius (1 → 2) is an open question for the owner.
- **`MAX_FULL_RING` (engine.rs:48, today 24)** caps the parked-fulls tier independently of the
  ring budget (`prefetch_fulls` re-derives from `RING_BUDGET_BYTES` at app_core_impl.rs:6111 —
  a second hardcoded consumer the plumbing must catch). Open design point: does 24 stay an
  independent Original-tier quota or join the limits struct? (Recommend: join, scaled mildly —
  it exists to stop Originals starving Fit decodes, which is a *ratio* concern, not absolute.)
- **Integrated / UMA GPUs**: classify by wgpu `DeviceType`, **not** by a VRAM threshold
  (finding 5): a real 1 GB discrete card uses its real (tiny) VRAM — substituting `R/4` there
  would authorize gigabytes of demoted residency. Only a genuine `IntegratedGpu` uses the shared
  arm (V_eff = R/4, halved caps), and its ring bytes then count against *system* RAM headroom
  (they are the same physical resource — see the archive section).
- **Virtual / CPU / ambiguous adapters** (RDP paravirtual GPUs, WARP): constants fallback,
  cap 64. Never the RAM arm.

### The window split rides along (global, not per-profile)

`window_for_capacity` (engine.rs:331) splits the ring 4/5 ahead, 1/5 behind. The "blazed past it
and it's gone" repro motivates rebalancing to **2/3 ahead, 1/3 behind** — on today's 22-slot ring
that's 14 ahead / 7 behind instead of 16 / 5. This is a UX-shape question, not a resource
question, so it is one global change, not a per-profile knob — and it must be **A/B'd separately
from the profiles** (finding 11): keypress→photon p50/p95/p99, preview-vs-sharp landing rate on
both forward blaze and reverse-after-blaze, upload backlog depth, and WDDM usage — not aggregate
throughput. It trades forward readiness for reverse readiness; the numbers decide if the trade is
free.

## What NEVER scales (the safety budgets)

Explicitly out of scope, regardless of profile — these are sized against **malicious input**, not
hardware envy:

- Archive eager-decode RAM budgets and pre-flights (7z, compressed-tar mid-stream `TooLarge`) —
  but see below: the pre-flight's *reservation constant* must become budget-aware.
- Every hostile-input cap in `pb-source` (PAX/GNU metadata quotas, entry/name-table caps,
  expanded-work caps, zstd/xz pre-checks).
- Decode worker count (CPU-bound; a separate concern with its own tradeoffs).
- Video queue byte/frame budgets (rebuffer semantics, deliberately constant-memory).

### ⚠ The archive pre-flight reservation must become dynamic (finding 6)

`archive.rs:32` hardcodes `APP_RESERVATIONS = 1_500_000_000 + 512 MiB` — literally the two old
constants — and subtracts that (plus a transient margin) from *currently available* RAM to size
an archive open. Under High that under-reserves by up to ~7.5 GiB of future pool commitment (and
on UMA, the ring too). But naively subtracting a *dGPU* ring budget would over-reserve — dedicated
VRAM isn't system RAM. The fix: the pre-flight subtracts **remaining pool headroom**
(pool budget − currently held outcome bytes) plus, **on UMA only**, remaining shared-ring
headroom — never already-committed memory, and never dedicated VRAM. The safety fraction and
transient margin stay as they are.

## Detection

One platform-quarantined helper (per the cross-platform discipline) producing a
`HwInfo { vram_dedicated, vram_os_budget, ram_total, ram_available, device_type }`:

- **Windows**: the adapter identity must be **exact** — wgpu selects the adapter against the
  surface during renderer construction (gpu.rs:2785), and locked wgpu 22's `AdapterInfo` carries
  vendor/device ids but **no LUID**, which is ambiguous with two identical GPUs. Get the LUID from
  the selected DX12 device (`ID3D12Device::GetAdapterLuid` via wgpu-hal's dx12 escape hatch) and
  query that exact `IDXGIAdapter3`: `DXGI_ADAPTER_DESC` for capacity, `QueryVideoMemoryInfo` for
  `Budget` **and `CurrentUsage`**. (The existing `display.rs:53` helper takes adapter 0 / output 0
  — the 110c plan already flags that as unsafe; don't copy it.) System RAM via
  `GlobalMemoryStatusEx` (total, available, commit headroom).
- **Consequence for ordering**: limits are computed **after** renderer construction (the adapter
  is only known then), so the ring/pool are *constructed* at fallback size and immediately
  reconfigured through the same live-apply path a profile change uses. One code path, exercised at
  every startup — not a separate boot special case.
- **macOS (later)**: `MTLDevice.recommendedMaxWorkingSetSize` + `sysctl hw.memsize`. Until wired,
  detection returns `None` → constants fallback (today's behavior, no break).
- **Linux (later)**: no portable VRAM query worth trusting; constants fallback initially.
- **Re-detection**: on device recreation, and on the DXGI **budget-change notification**
  (`RegisterVideoMemoryBudgetChangeNotificationEvent`) — WDDM budgets move when other apps start;
  reacting means we shrink before the OS starts demoting us. Not on the hot path, ever.

## The VRAM ceiling model (finding 4 — this is the enforcement, the fractions are just requests)

`Budget` covers **all** process GPU usage — surface, fp16 scene target, held texture, staging
ring, derive scratch, thumb textures — not just the photo ring. So the enforced bound is:

```
ring_bytes ≤ safety × Budget − non_ring_usage − transient_headroom
```

where `non_ring_usage = CurrentUsage − tracked ring residency` (measured at sizing time),
`transient_headroom` covers the derive scratch cap (256 MB) + staging + one worst-case slot, and
`safety ≈ 0.8`. The profile fraction (15/25/45% of dedicated VRAM) applies first; this model then
clamps it. On a budget-change notification the ring re-clamps through the reconfigure path. The
45% High fraction is a starting heuristic to be A/B'd, **not** an assumption that demotion can't
happen — the A/B matrix includes a VRAM-usage-vs-Budget trace over a long blaze (below).

## GPU allocation failure: what's actually true, and the recovery design (finding 1 — the rev-1 P0)

Rev 1 proposed "halve-and-retry `reserve_ring`". **That was wrong about the code**: `reserve_ring`
allocates no textures — it resizes a host-side `Vec<Option<_>>` (gpu.rs:3643) and returns `()`;
slot textures are created lazily in `upload_slot` (`device.create_texture`, gpu.rs:1909/3664).
And an allocation failure today doesn't "fail the reserve" — with locked wgpu 22 and no error
scope installed, an uncaptured OOM hits wgpu's fatal default handler and **panics**. There is
nothing at reserve time to halve. The replacement design, two layers:

1. **Proactive sizing so the driver is never asked for more than measured headroom allows** — the
   ceiling model above. This is the primary defense and the normal operating mode; it's also the
   philosophy the ring already embodies (logical byte budget enforced before upload).
2. **An error scope around ring-slot texture creation in `upload_slot`** (`push_error_scope
   (OutOfMemory)` / async pop), making slot allocation *genuinely fallible*: on OOM the upload
   transaction rolls back (`release_pending` already exists for exactly this shape), the ring
   evicts the lowest-priority resident, and the upload retries once smaller-footprint; repeated
   failure marks the item failed rather than crashing. This is the backstop for the day the
   headroom model is wrong (driver bugs, budget races). True *device loss* stays a separate
   class: the existing surface-recreation path, not this mechanism.

## Plumbing: one `ResidencyLimits` seam, shared with 110c (finding 7)

Not the rev-1 four-field struct — a richer limits object both plans consume:

```
ResidencyLimits {
  ring_bytes, ring_slot_cap,
  parked_original_quota,   // the MAX_FULL_RING successor (or its keeper)
  derive_scratch_headroom, // reserved out of the VRAM ceiling (110c wants this too)
  pool_bytes,
  radius_clamp,
}
```

with `ResidencyLimits::fallback()` = today's constants verbatim. Known hardcoded consumers the
plumbing must convert (verified by Codex round 1): `ring_capacity`/`rebuild_ring`
(engine.rs:324, app_core_impl.rs:7440), `prefetch_fulls`'s second `RING_BUDGET_BYTES` read
(app_core_impl.rs:6111), `MAX_FULL_RING` (engine.rs:48), pool construction
(app_core_impl.rs:294, pb-app/main.rs:749), and **both shell ring constructors**
(pb-app/main.rs:3993, pb-mac-ffi/lib.rs:2452). 110c's display-capped pyramid then sizes
*per-item* cost inside the same limits — one detection, two consumers, no second source of truth.

- The **decode pool budget is immutable today** (decode_pool.rs:230) and its worker gate checks
  *before* decode, so in-flight jobs legitimately overshoot on completion (decode_pool.rs:381/422).
  The live setter is therefore **prospective**: shrinking stops new job starts until held bytes
  fall under the new limit; it never revokes live guards. Document the overshoot bound
  (≈ workers × largest decode) — it exists today and merely scales.

### Live apply — `reconfigure_residency`, not `rebuild_ring(true)` (findings 2 + 3)

Rev 1 wanted to reuse the item-6 retain path. Codex is right that it can't: `rebuild_ring(true)`
is a **geometry invalidation** — it bumps the epoch, `drop_fit_slots()` deliberately purges every
Fit, `compact_to` hardcodes survivor remaps as `RepKind::Original`, and `pending_uploads` is
cleared. Correct for a resize; wrong for "same geometry, new budget" (neighbour Fits would go
cold for no reason). A profile change gets its own transition:

- **`ResidentRing::reconfigure(capacity, byte_budget, priorities)`** — transactional, replacing
  rev-1's `set_budget` + `compact_to` idea. Contract: growth evicts nothing; shrink evicts
  lowest-priority residents until **both** limits hold; the displayed item may remain as the sole
  oversized exception; pending reservations are kept-within-limits or explicitly cancelled; it
  returns survivors + evictions + remaps (both representations — no Fit purge) so the CPU and GPU
  mirrors update atomically.
- **`AppCore::reconfigure_residency(limits)`** — no epoch bump; applies the ring reconfigure,
  feeds the returned remaps to `Renderer::remap_ring` **and consumes its returned
  actually-moved list** (the existing caller ignores it — fix that here); reconciles the
  bookkeeping keyed to residency: `pending_uploads` (keep — epoch unchanged — unless their item
  was evicted), pool budget setter, `full_requested_at` / `last_upgrade_set` /
  `preview_resident` / `upgrade_done` pruned to survivors, watchdog re-armed if its item was
  evicted, then one `request_prefetch()`.
- Used by: profile change, the post-renderer startup sizing (above), and budget-change
  notifications. Three callers, one transition, fully unit-testable with the fake renderer.

## UI (owner call: a slider on the General page)

- `pb-ui` `slider` component, **3 detents** (Safe / Normal / High), on the General settings page
  in a `group_card` — no new UI primitives needed.
- Under it, a computed line in plain language (UI copy style: simple, no em-dashes), e.g. on the
  owner's box at Normal: "Ring: 8 GiB of 32 GiB graphics memory. Decode pool: 2 GiB." — honest
  numbers from the actual `ResidencyLimits` (rev 1's example overstated the Normal pool; the line
  renders whatever the formulas produced, it never hand-waves).
- The High description carries the caveat: "Uses most of your graphics memory. Best when Blaze
  Viewer is the main app running."
- Settings field: `performance_profile = "safe" | "normal" | "high"` (default `normal`). A
  preference, not a viewing trace — ADR-018 clean. (Codex concurred; the one privacy edge is the
  A/B telemetry: NDJSON stays numeric-only, opt-in, never paths/names/metadata.)
- **A/B lever before UI exists**: `PB_PERF_PROFILE=safe|normal|high` env override (the
  `PB_SCALE_POLICY` pattern), so the profiles can be measured in phase 1 with no chrome work.

## Risks (ranked, rev 2)

1. **WDDM demotion is silent** — addressed by the ceiling model + budget-change notifications +
   the measured VRAM trace; the fraction alone is NOT the defense.
2. **Allocation OOM panics today** — addressed by the two-layer design above; the error-scope
   backstop must land **before** High ships.
3. **O(cap) and O(cap²) event-loop work at 128/256 slots** — beyond the targets/keep-list scans,
   Codex found: outcome sorting does `targets.position()` per pending result
   (app_core_impl.rs:7154), leftover pending results re-scan `targets.contains()` every tick
   (app_core_impl.rs:7186), and ring admission under a shrink/upgrade burst is O(cap²)
   (ring.rs:320, 453). The slot-cap raise is **gated** on two new benches: a worst-case
   completion-burst tick and a full-ring shrink, both at cap 256.
4. **Pool soft-cap overshoot scales** — documented bound, prospective setter (above).
5. **Downgrade compatibility**: an older build ignores the new settings key. Fine by construction.

## Test plan

- **Pure unit + property tests** (the bulk): `(HwInfo, profile) → ResidencyLimits` — caps
  respected; monotonic in profile per knob; `IntegratedGpu` arm bounded by its RAM fraction;
  small-discrete keeps real VRAM; virtual/CPU/None → exactly `fallback()`; pool bounded by
  available-RAM at sizing time.
- **Ring `reconfigure` contract tests**: grow evicts nothing; shrink respects priority order and
  the displayed-item exception; returned remaps consistent with survivors; both representations
  survive (regression: a profile change must NOT behave like `drop_fit_slots`).
- **`reconfigure_residency` integration** (fake renderer): bookkeeping sets pruned to survivors;
  epoch unchanged; pending uploads for surviving items still land; watchdog sanity.
- **Error-scope backstop test**: fake renderer injecting OOM on slot create → rollback, evict,
  retry, no panic, item eventually failed-not-crashed.
- **Measured, not asserted** (prime directive): (a) tick p99 at cap 256 vs 64 — completion-burst
  + shrink benches; (b) Safe vs High on the 5090: reverse-after-blaze landing quality and
  keypress→photon unchanged; (c) VRAM `CurrentUsage` vs `Budget` trace over a long blaze at High
  — the demotion check; (d) the window-split A/B, separately, with the finding-11 metric set.
- **Manual**: the blaze → flip → back-up repro at each profile, RDP and physical.

## Phases (after owner sign-off)

1. **Detection + `ResidencyLimits` + formulas + `PB_PERF_PROFILE`** — pure logic, fully tested,
   dark (fallback stays the default until phase 4 flips it). Includes the LUID adapter plumbing.
2. **`ResidentRing::reconfigure` + the pool setter + `AppCore::reconfigure_residency`** + the
   startup-sizing call; archive pre-flight goes dynamic. The error-scope backstop in upload_slot.
3. **Window split rebalance** — independent, small, own A/B.
4. **Slider UI** on General + settings field + computed description line; default → Normal.
5. **Measure + ship**: the bench/A-B matrix above, CHANGELOG, manual-test-script addendum;
   slot-cap raise lands here only if the benches clear.

## Open questions (owner)

1. Naming: Safe / Normal / High? (Alternatives: Balanced / Maximum. UI copy style says plain.)
2. Show the computed absolute numbers in the settings description, or keep it qualitative?
3. Should High also raise the *default* `full_res_radius` (1 → 2), or only the clamp?
4. Default = Normal for existing installs on update (recommended — it computes to today's numbers
   on modest hardware), or keep existing users on Safe until they opt in?
5. Does `MAX_FULL_RING` join `ResidencyLimits` (recommended) or stay an independent constant?

## Codex review log

- **Round 1 (2026-07-19, rev 1): NOT sign-off-ready.** 1×P0, 8×P1, 2×P2 — all folded into rev 2:
  the P0 (halve-and-retry `reserve_ring` was based on a false model; textures allocate lazily and
  OOM panics) became the two-layer allocation design; `rebuild_ring(true)` live-apply became
  `reconfigure_residency` + a transactional ring `reconfigure`; the WDDM min() became the
  CurrentUsage-aware ceiling model + budget-change notifications; LUID-exact adapter matching and
  DeviceType-based (not VRAM-threshold) UMA classification; the archive `APP_RESERVATIONS`
  hardcode goes dynamic; the seam grew into `ResidencyLimits` with the missed consumers
  enumerated; the slot-cap raise gated on completion-burst + shrink benches; pool sizing bounded
  by available RAM with a prospective setter; arithmetic fixes (byte-budget-bound at High, GiB
  units, honest UI example). Round 1 confirmed: the existing-constant anchors, the worked ring
  examples, the 110c complementarity, and privacy-charter cleanliness.
- **Round 2: pending** — re-review of rev 2 before owner sign-off.
