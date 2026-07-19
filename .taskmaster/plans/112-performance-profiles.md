# 112 — Performance profiles: hardware-sized budgets (Safe / Normal / High)

_Status: **DESIGN DRAFT rev 4** (2026-07-19) — Codex rounds 1–3 folded (review log at the
bottom); owner sign-off pending (a round 4 is the owner's call — the loop is converging and the
remaining items are implementation contracts the phase tests verify). **Do not implement** until
the owner green-lights the design._

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
| VRAM ring (`RING_BUDGET_BYTES`, engine.rs:38) | 1.5 GB const | min(15% V, the 1.5 GB const) | min(25% V, 12 GiB) | min(45% V, 20 GiB) |
| Ring slot cap (`ring_capacity` clamp, engine.rs:324) | 64 | 64 | 128 ⚠gated | 256 ⚠gated |
| Decode pool RAM (`POOL_BUDGET_BYTES`, engine.rs:41) | 512 MiB const | 512 MiB | clamp(R/32, 512 MiB, 2 GiB) | clamp(R/16, 1 GiB, 8 GiB) |
| Parked full-res radius clamp (`full_res_radius`, today ≤3) | 3 | 3 | 4 | 8 |

V = detected dedicated VRAM (further bounded by the WDDM headroom model below — the fraction is
the *request*, the headroom model is the *enforcement*). R = physical RAM, and the pool formula
is additionally bounded by **live memory pressure at sizing time** (round-1 finding 9 / round-2
finding 6): `pool = min(formula, ram_available / 2, commit_available / 2)`, where both fields are
explicit in `HwInfo` (on Windows both come from `GlobalMemoryStatusEx` — `ullAvailPhys` and
`ullAvailPageFile`). A RAM-pressured machine gets a smaller pool than its total suggests, and the
archive pre-flight (below) uses the same fields. Worked examples (ring):

| Machine | Safe | Normal | High |
|---|---|---|---|
| 6 GB laptop dGPU | 0.9 GiB | 1.5 GiB (= today) | 2.7 GiB |
| 12 GB midrange | 1.5 GiB | 3 GiB | 5.4 GiB |
| RTX 5090 (32 GB) | 1.5 GiB | 8 GiB | 14.4 GiB |

On the owner's display a 7680×2160 RGBA8 Fit slot is 66 355 200 bytes. Normal's 8 GiB holds 129
such slots — one MORE than its 128 slot cap, so Normal is **slot-cap-bound** there (round 3;
~128 resident photos is the intended outcome, stated honestly). High's 14.4 GiB ≈ 233 slots is
**byte-budget-bound** (233 < 256). Unit honesty: the shipping constant is 1 500 000 000 B,
~7.4% under a true 1.5 GiB — Safe's cap is **the constant itself**, so Safe ≡ today exactly.

- **`full_res_radius`**: the profile moves the *clamp*, not the user's chosen value. Whether High
  should also raise the *default* radius (1 → 2) is an open question for the owner. ⚠ The clamp
  lives in **two** places: the settings-side clamp and the hard `min(.., 3)` in
  `app_core_impl.rs:6173` — both must route through the limits or High silently stays at 3
  (round-2 finding 3).
- **`MAX_FULL_RING` (engine.rs:48, today 24)** caps the parked-fulls tier independently of the
  ring budget (`prefetch_fulls` re-derives from `RING_BUDGET_BYTES` at app_core_impl.rs:6111 —
  a second hardcoded consumer the plumbing must catch). Open design point: does 24 stay an
  independent Original-tier quota or join the limits struct? (Recommend: join, scaled mildly —
  it exists to stop Originals starving Fit decodes, which is a *ratio* concern, not absolute.)
- **Integrated / UMA GPUs**: classify by wgpu `DeviceType`, **not** by a VRAM threshold
  (round-1 finding 5): a real 1 GB discrete card uses its real (tiny) VRAM — substituting `R/4`
  there would authorize gigabytes of demoted residency. Only a genuine `IntegratedGpu` uses the
  shared arm (V_eff = R/4, halved caps), and its ring bytes then count against *system* RAM
  headroom (they are the same physical resource — see the archive section).
- **Virtual / CPU / ambiguous adapters** (RDP paravirtual GPUs, WARP): constants fallback,
  cap 64. Never the RAM arm.

### The window split rides along (global, not per-profile)

`window_for_capacity` (engine.rs:331) splits the ring 4/5 ahead, 1/5 behind. The "blazed past it
and it's gone" repro motivates rebalancing to **2/3 ahead, 1/3 behind** — on today's 22-slot ring
that's 14 ahead / 7 behind instead of 16 / 5. This is a UX-shape question, not a resource
question, so it is one global change, not a per-profile knob — and it must be **A/B'd separately
from the profiles** (round-1 finding 11): keypress→photon p50/p95/p99, preview-vs-sharp landing
rate on both forward blaze and reverse-after-blaze, upload backlog depth, and WDDM usage — not
aggregate throughput. It trades forward readiness for reverse readiness; the numbers decide if
the trade is free.

## What NEVER scales (the safety budgets)

Explicitly out of scope, regardless of profile — these are sized against **malicious input**, not
hardware envy:

- Archive eager-decode RAM budgets and pre-flights (7z, compressed-tar mid-stream `TooLarge`) —
  but see below: the pre-flight's *reservation constant* must become budget-aware.
- Every hostile-input cap in `pb-source` (PAX/GNU metadata quotas, entry/name-table caps,
  expanded-work caps, zstd/xz pre-checks).
- Decode worker count (CPU-bound; a separate concern with its own tradeoffs).
- Video queue byte/frame budgets (rebuffer semantics, deliberately constant-memory).

### ⚠ The archive pre-flight reservation must become dynamic — via a snapshot (finding r2-5)

`archive.rs:32` hardcodes `APP_RESERVATIONS = 1_500_000_000 + 512 MiB` — literally the two old
constants — and subtracts that (plus a transient margin) from *currently available* RAM to size
an archive open. Under High that under-reserves by up to ~7.5 GiB of future pool commitment (and
on UMA, the ring too). But naively subtracting a *dGPU* ring budget would over-reserve — dedicated
VRAM isn't system RAM.

The corrected shape (round 2): the pre-flight must subtract **remaining pool headroom**, and on
**UMA only** remaining shared-ring headroom — never already-committed memory, never dedicated
VRAM. And because today's plumbing is a context-free global `ram_budget()` (archive.rs:128) that
7z even samples **twice** per open (scan.rs:762), while the pool's held-bytes counter excludes
currently *executing* decodes (workers gate before decoding, charge on completion —
decode_pool.rs:381), the design is: compute **one immutable `ReservationSnapshot`** at the open
decision — `{future_pool_claim, uma_ring_headroom, ram_available, commit_available}` — and
thread it through `load_archive` and **every** pre-flight of that open, so all checks see the
same numbers (created **above** `load_archive_with_cache` and reused across password attempts;
7z samples the global twice per open today, scan.rs:762/:828). ⚠ Round 3 corrected the
arithmetic: the term is the app's FUTURE claim, so active decodes **add** to it —
`future_pool_claim = saturating_sub(pool_budget, held) + active_reserved` — and no sound
per-decode bound exists today to derive `active_reserved` from (the 200 MP ceiling applies only
when metadata is already cached — app_core_impl.rs:6207; HDR decodes are 8 B/px; `clamp_to_max`
runs *after* the CPU decode; encoded-input clones and scratch sit outside the byte counter).
So the pool gains **enforced per-active-job reservations**: a worker reserves a hard per-job
ceiling (from the fit box / probed dims / a format cap) before decoding, and the snapshot reads
the reserved sum — checked/saturating arithmetic throughout. The safety fraction and transient
margin stay.
(Quiescing the pool before an open was considered and rejected: an archive open during a blaze
must not stall the blaze.)

## Detection

One platform-quarantined helper (per the cross-platform discipline) producing a
`HwInfo { vram_dedicated, vram_os_budget, vram_current_usage, ram_total, ram_available,
commit_available, device_type }`:

- **Windows**: the adapter identity must be **exact** — wgpu selects the adapter against the
  surface during renderer construction (gpu.rs:2785), and locked wgpu 22's `AdapterInfo` carries
  vendor/device ids but **no LUID**, which is ambiguous with two identical GPUs. Get the LUID from
  the selected DX12 device (`ID3D12Device::GetAdapterLuid` via wgpu-hal's dx12 escape hatch; on
  the Vulkan backend, `VK_KHR_get_physical_device_properties2` exposes the same LUID) and query
  that exact `IDXGIAdapter3`: `DXGI_ADAPTER_DESC` for capacity, `QueryVideoMemoryInfo` for
  `Budget` **and `CurrentUsage`**. (The existing `display.rs:53` helper takes adapter 0 /
  output 0 — the 110c plan already flags that as unsafe; don't copy it.) System RAM via
  `GlobalMemoryStatusEx` (total, available, **commit available**).
- **Ordering** (validated by round 2 against both shells): limits are computed **after** renderer
  construction — winit has a clean window after the renderer is boxed and before the initial
  prefetch (main.rs:4008 → 4039), and the mac FFI construction has the equivalent
  (pb-mac-ffi lib.rs:2463 → 2471). The ring is *constructed* at fallback size and immediately
  reconfigured through the same live-apply path a profile change uses — one code path, exercised
  at every startup. On winit, sample `CurrentUsage` **after** the egui overlay exists so its
  textures count as non-ring usage.
- **macOS (later)**: `MTLDevice.recommendedMaxWorkingSetSize` + `sysctl hw.memsize`. Until wired,
  detection returns `None` → constants fallback (today's behavior, no break).
- **Linux (later)**: no portable VRAM query worth trusting; constants fallback initially.
- **Re-detection**: on device recreation, and on the DXGI **budget-change notification**
  (`RegisterVideoMemoryBudgetChangeNotificationEvent`) — WDDM budgets move when other apps start;
  reacting means we shrink before the OS starts demoting us. Not on the hot path, ever.

## The VRAM ceiling model (this is the enforcement, the fractions are just requests)

`Budget` covers **all** process GPU usage — surface, fp16 scene target, held texture, staging
ring, derive scratch, egui overlay, thumb textures — not just the photo ring. So the enforced
bound is:

```
ring_bytes ≤ safety × Budget − non_ring_usage − transient_headroom
```

where `non_ring_usage = CurrentUsage − tracked ring residency` (measured at sizing time),
`transient_headroom` covers the derive scratch cap (256 MB) + staging + one worst-case slot, and
`safety ≈ 0.8`. The profile fraction (15/25/45% of dedicated VRAM) applies first; this model then
clamps it. On a budget-change notification the ring re-clamps through the reconfigure path. The
45% High fraction is a starting heuristic to be A/B'd, **not** an assumption that demotion can't
happen — the A/B matrix includes a VRAM-usage-vs-Budget trace over a long blaze (below).

## GPU allocation failure (rev 3 — reshaped twice by review; read the history here)

**What's actually true** (round 1): `reserve_ring` allocates no textures — it resizes a host-side
`Vec<Option<_>>` (gpu.rs:3643); slot textures are created lazily in `upload_slot`
(`device.create_texture`, gpu.rs:1909/3664), and with no error scope installed an allocation
failure hits wgpu's uncaptured handler and **panics**. There is nothing at reserve time to halve.

**And the naive error-scope fix is backend-dependent** (round 2): on **Vulkan**,
`VK_ERROR_OUT_OF_*_MEMORY` surfaces as `DeviceError::OutOfMemory` and an OOM scope catches it.
On **DX12** — a first-class Windows backend — wgpu-hal's suballocator null-checks the resource
*before* processing the HRESULT (wgpu-hal dx12/suballocation.rs:179) and reports
`ResourceCreationFailed`, which wgpu-core classifies as a **Validation** error
(wgpu-22.1.0 wgpu_core.rs:270). An OOM-only scope misses it; the panic remains.

The design, three parts:

1. **Proactive sizing so the driver is never asked for more than measured headroom allows** — the
   ceiling model above. This is the primary defense and the normal operating mode; it's also the
   philosophy the ring already embodies (logical byte budget enforced before upload).
2. **Typed, fallible slot allocation — the wgpu-hal HRESULT patch is MANDATORY** (round 3):
   tight scopes alone cannot attribute a DX12 failure, because wgpu-hal null-checks the resource
   *before* processing the HRESULT (dx12/suballocation.rs:179 → `ResourceCreationFailed` →
   classified Validation) — so the vendor patch processes the HRESULT first and surfaces
   E_OUTOFMEMORY as a true `OutOfMemory` error. (Task #53 is the *planned* precedent for
   carrying a wgpu-hal patch — it is pending, not applied; round 3 corrected rev-3's wording.)
   `upload_slot` becomes fallible and **retries only a positively classified OOM**; a Validation
   capture is an invariant bug — logged loudly, representation marked failed, NO retry (a retry
   could mask a real error). Fallibility covers the **whole upload transaction**, not just
   `create_texture`: the staging pool can `create_buffer` on a miss (upload.rs:156), and mip
   generation, views, the uniform buffer, sampler, and bind group all allocate after the texture
   (gpu.rs:1919) — each under the scopes, or preallocated. Claim narrowed accordingly: "no panic
   on allocation failure in the ring upload path", not "never a crash".
3. **Rollback that matches each path, and retirement that actually frees VRAM** (rounds 2+3):
   the *fresh-admission* path rolls back with `release_pending` (that IS its contract — it
   refuses residents, ring.rs:561, which is correct there). The *in-place upgrade* path
   (app_core_impl.rs:7212, after `make_room_for_upgrade`) must instead **preserve the old
   resident texture and its byte accounting on failure** — the photo keeps displaying the
   preview it had. Eviction reporting becomes **complete**: `reserve_bytes` and
   `make_room_for_upgrade` can each evict SEVERAL residents silently today (ring.rs:354, :461 —
   both return only the immediate answer), so both APIs change to return **every** eviction
   `(slot, item, RepKind)`, and the caller retires the corresponding GPU textures via a new
   per-slot `Renderer` **retire** method on every ordinary admission/upgrade eviction — before
   the next allocation attempt, not only after an OOM (CPU-side eviction alone leaves the
   `RingSlot` texture alive; dropping it is what returns the memory). Repeated
   upgrade-allocation failure is recorded **per representation**, never in the item-wide
   `failed` set (which would also suppress the preserved preview's own re-decode if it were
   later evicted — app_core_impl.rs:5791).

**Honesty note on device loss** (round-2 correction): rev 2 claimed device loss "has an existing
surface-recreation path" — wrong. Today's code reconfigures the surface only for
`SurfaceError::Lost/Outdated`; a true device loss is **not** currently recovered anywhere. That
is a pre-existing gap, out of scope for #112 (this design reduces the chance of triggering it;
it does not add device recreation).

**Test requirement**: a **real allocation-failure test** on DX12 and Vulkan (ignored/dev-gated).
⚠ Not an absurd-size descriptor — that fails *validation*, not allocation (round 3). Drive a
**valid** descriptor to genuine exhaustion (repeated large-but-valid allocations under budget
instrumentation) or use HAL fault injection, and assert typed-capture-not-panic. The
fake-renderer OOM-injection test remains for the rollback logic, but it cannot prove the capture
property.

## Plumbing: one `ResidencyLimits` seam, shared with 110c

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
plumbing must convert (verified across both review rounds): `ring_capacity`/`rebuild_ring`
(engine.rs:324, app_core_impl.rs:7440), `prefetch_fulls`'s second `RING_BUDGET_BYTES` read
(app_core_impl.rs:6111), **the hard radius `min(.., 3)` at app_core_impl.rs:6173**,
`MAX_FULL_RING` (engine.rs:48), pool construction (app_core_impl.rs:294, pb-app/main.rs:749),
the archive `APP_RESERVATIONS` (archive.rs:32, via the snapshot), and **both shell ring
constructors** (pb-app/main.rs:3993, pb-mac-ffi/lib.rs:2452). 110c's display-capped pyramid then
sizes *per-item* cost inside the same limits — one detection, two consumers, no second source of
truth.

- The **decode pool budget is immutable today** (decode_pool.rs:230) and its worker gate checks
  *before* decode, so in-flight jobs legitimately overshoot on completion (decode_pool.rs:381/422).
  The live setter is therefore **prospective**: shrinking stops new job starts until held bytes
  fall under the new limit; it never revokes live guards. The overshoot bound
  (≈ workers × largest decode) is not merely documented — it is a term in the archive
  `ReservationSnapshot` above.

### Live apply — `reconfigure_residency`, not `rebuild_ring(true)`

`rebuild_ring(true)` is a **geometry invalidation** — it bumps the epoch, `drop_fit_slots()`
deliberately purges every Fit, `compact_to` hardcodes survivor remaps as `RepKind::Original`, and
`pending_uploads` is cleared. Correct for a resize; wrong for "same geometry, new budget"
(neighbour Fits would go cold for no reason). A profile change gets its own transition:

- **`ResidentRing::reconfigure(capacity, byte_budget, priorities)`** — transactional. Priorities
  are keyed **`(item, RepKind)`** (one item can own both a Fit and an Original slot — round-2
  finding 3). Contract: growth evicts nothing; shrink evicts lowest-priority residents until
  **both** limits hold; the displayed item may remain as the sole oversized exception; pending
  reservations are kept-within-limits or explicitly cancelled; it returns survivors + evictions +
  remaps (both representations — no Fit purge).
- **Two-phase CPU/GPU commit** (rounds 2+3 — "atomic" spelled honestly): phase 1 computes the
  plan (survivors/evictions/remaps) without mutating; phase 2 applies it to the `ResidentRing`
  and calls `Renderer::remap_ring`, whose return becomes a **structured outcome**
  `{ moved, held_presented: bool }` — a bare moved-list cannot say whether an unmoved
  *displayed* texture survived, because that happens only when it sat at `present_idx` and was
  stashed into `held` (gpu.rs:3725). Reconcile: unmoved slots demote to evicted in the CPU
  mirror; the displayed slot may demote **only when `held_presented`** (the screen keeps showing
  the held frame) — otherwise clear the hold and immediately request/present the normal
  fallback. Then **re-present the displayed survivor**: `remap_ring` unconditionally clears
  `present_idx` (gpu.rs:3709); the geometry path already shows the rebind shape
  (app_core_impl.rs:5459).
- **`AppCore::reconfigure_residency(limits)`** — no epoch bump. Steps, in order: ring
  `reconfigure` two-phase as above; **recompute `ahead`/`behind` from the new capacity** (they are
  stored fields `request_prefetch` consumes — app_core_impl.rs:5708); pool budget setter;
  bookkeeping reconciliation (below); one `request_prefetch()`.
- **Bookkeeping reconciliation** (round-2 finding 4 corrected the rev-2 rule): `pending_uploads`
  are kept or dropped by the new **wanted `(item, rep_kind, epoch)`** — not by "was the item
  evicted" (residency and wanted-work are different questions); **thumb outcomes are
  ring-independent and always kept** (app_core_impl.rs:7033), as is the thumb cache;
  `last_upgrade_set` is **recomputed** on the next tick, not pruned (it records the last *issued
  request set*, not resident state — app_core.rs:400); `preview_resident`/`upgrade_done`/
  `full_requested_at` prune to surviving residents; `compare_pin` is preserved (it re-enters
  targets and re-decodes if evicted — that is its normal contract); `resize_hold` is preserved while the
  displayed survivor remains — or, if it was demoted, only when the remap outcome reports
  `held_presented`; **`Perf::full_seen` is intersected with the surviving full-resident set,
  preserving the already-fired state** (a blanket reset would leave survivors never re-emitting
  `full_resident`, wedging the all-cached metric incomplete forever — perf.rs:64/:136);
  active video state is untouched (outside the ring); poster outcomes are ordinary
  representation-aware pending work.
- Used by: profile change, the post-renderer startup sizing (above), and budget-change
  notifications. Three callers, one transition, fully unit-testable with the fake renderer.

## UI (owner call: a slider on the General page)

- `pb-ui` `slider` component, **3 detents** (Safe / Normal / High), on the General settings page
  in a `group_card` — no new UI primitives needed.
- Under it, a computed line in plain language (UI copy style: simple, no em-dashes), e.g. on the
  owner's box at Normal: "Ring: 8 GiB of 32 GiB graphics memory. Decode pool: 2 GiB." — honest
  numbers rendered from the actual `ResidencyLimits`, never hand-waved.
- The High description carries the caveat: "Uses most of your graphics memory. Best when Blaze
  Viewer is the main app running."
- Settings field: `performance_profile = "safe" | "normal" | "high"` (default `normal`). A
  preference, not a viewing trace — ADR-018 clean. (Codex concurred; the one privacy edge is the
  A/B telemetry: NDJSON stays numeric-only, opt-in, never paths/names/metadata.)
- **A/B lever before UI exists**: `PB_PERF_PROFILE=safe|normal|high` env override (the
  `PB_SCALE_POLICY` pattern), so the profiles can be measured in phase 1 with no chrome work.

## Interaction with Phase-1b (110c display-capped pyramid)

Complementary, not competing: 110c already calls for one overall GPU residency budget, a
parked-source quota, and reserved derive-scratch headroom — exactly the fields `ResidencyLimits`
carries. 110c shrinks **per-item** cost on small machines; profiles raise the **total** on big
ones. One detection, two consumers; neither blocks the other.

## Risks (ranked, rev 3)

1. **WDDM demotion is silent** — the ceiling model + budget-change notifications + the measured
   VRAM trace; the fraction alone is NOT the defense.
2. **Allocation failure panics today, and the capture is backend-dependent** — the three-part
   design above; the typed-OOM design (mandatory wgpu-hal patch) and its real capture test must
   land **before** High ships.
3. **O(cap) and O(cap²) event-loop work at 128/256 slots** — beyond the targets/keep-list scans:
   outcome sorting does `targets.position()` per pending result (app_core_impl.rs:7154), leftover
   pending results re-scan `targets.contains()` every tick (app_core_impl.rs:7186), and ring
   admission under a shrink/upgrade burst is O(cap²) (ring.rs:320, 453). The slot-cap raise is
   **gated** on two benches: a worst-case completion-burst tick and a full-ring shrink, at cap 256.
4. **Pool soft-cap overshoot scales** — prospective setter; the bound is a term in the archive
   snapshot (above).
5. **Downgrade compatibility**: an older build ignores the new settings key. Fine by construction.

## Test plan

- **Pure unit + property tests** (the bulk): `(HwInfo, profile) → ResidencyLimits` — caps
  respected; monotonic in profile per knob; `IntegratedGpu` arm bounded by its RAM fraction;
  small-discrete keeps real VRAM; virtual/CPU/None → exactly `fallback()`; pool bounded by
  available-RAM **and commit** at sizing time.
- **Ring `reconfigure` contract tests**: grow evicts nothing; shrink respects `(item, RepKind)`
  priority order and the displayed-item exception; returned plan consistent with survivors; both
  representations survive (regression: a profile change must NOT behave like `drop_fit_slots`);
  partial-remap reconciliation demotes CPU-side too.
- **`reconfigure_residency` integration** (fake renderer): the finding-4 bookkeeping matrix
  (thumbs kept, pin kept, `last_upgrade_set` recomputed, perf reset, displayed survivor
  re-presented after remap); epoch unchanged; ahead/behind recomputed; pending uploads for
  surviving wanted work still land.
- **Allocation-failure tests**: the **real DX12 + Vulkan** capture test (ignored/dev-gated,
  absurd-size texture under nested scopes → captured, not panicked) plus the fake-renderer
  rollback tests (fresh-admission vs in-place-upgrade paths, retire-then-retry, old resident
  preserved on upgrade failure).
- **Measured, not asserted** (prime directive): (a) tick p99 at cap 256 vs 64 — completion-burst
  + shrink benches; (b) Safe vs High on the 5090: reverse-after-blaze landing quality and
  keypress→photon unchanged; (c) VRAM `CurrentUsage` vs `Budget` trace over a long blaze at High
  — the demotion check; (d) the window-split A/B, separately, with the finding-11 metric set.
- **Manual**: the blaze → flip → back-up repro at each profile, RDP and physical.

## Phases (after owner sign-off)

1. **Detection + `ResidencyLimits` + formulas + `PB_PERF_PROFILE`** — pure logic, fully tested,
   dark (fallback stays the default until phase 4 flips it). Includes the LUID adapter plumbing.
2. **`ResidentRing::reconfigure` + the pool setter + `AppCore::reconfigure_residency`** + the
   startup-sizing call; the archive `ReservationSnapshot` + per-active-job pool reservations;
   the fallible upload transaction (mandatory wgpu-hal HRESULT patch) + the retire /
   complete-eviction-reporting plumbing + the capture test.
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

- **Round 1 (2026-07-19, rev 1): NOT sign-off-ready.** 1×P0, 8×P1, 2×P2. Headline: rev-1's
  "halve-and-retry `reserve_ring`" was based on a false model (textures allocate lazily; OOM
  panics). Also: live-apply could not reuse `rebuild_ring(true)`; the WDDM min() ignored
  `CurrentUsage`; adapter matching needed LUID; UMA misclassified by VRAM threshold; archive
  `APP_RESERVATIONS` hardcode; missed hardcoded consumers; slot-cap superlinear paths; pool
  sizing ignored live pressure; arithmetic. All folded into rev 2. Confirmed: constants anchors,
  ring examples, 110c complementarity, privacy cleanliness.
- **Round 2 (2026-07-19, rev 2): NOT sign-off-ready.** 1×P0, 5×P1, 1×P2 — all folded into rev 3:
  the P0 (an OOM-only error scope misses DX12 allocation failure — wgpu-hal reports
  `ResourceCreationFailed`, classified Validation → still panics) became the nested-scope +
  vendor-patch-escalation design with a required real DX12 test; `release_pending` scoped to
  fresh-admission only, in-place upgrades preserve the old resident, `Renderer` gains per-slot
  retire (eviction must free actual VRAM); the device-loss "existing recovery path" claim
  retracted (none exists — pre-existing gap, out of scope); `reconfigure_residency` gained
  ahead/behind recompute, `(item, RepKind)` priorities, displayed-survivor re-present
  (remap clears `present_idx`), two-phase commit, and the corrected bookkeeping matrix (thumbs
  ring-independent, `last_upgrade_set` recomputed not pruned, perf reset, pin/resize_hold/video
  rules); the archive fix became an immutable `ReservationSnapshot` threaded through every
  pre-flight (7z samples the global twice today) including the pool's active-decode overshoot;
  `commit_available` became a real `HwInfo` field used by pool + archive; the radius hard-clamp
  at app_core_impl.rs:6173 joined the consumer list; slot arithmetic corrected to true GiB
  (129/233). Round 2 confirmed: the startup-ordering windows in both shells, the CurrentUsage
  ceiling, DeviceType classification, the slot-cap benchmark gate, and the separately-measured
  window split.
- **Round 3 (2026-07-19, rev 3): NOT sign-off-ready.** 2×P0, 3×P1, 3×P2 — all folded into rev 4:
  the nested-scope idea alone cannot attribute DX12 failures (null-check precedes the HRESULT) →
  the wgpu-hal patch is now MANDATORY, retry only typed OOM, Validation = fatal, fallibility
  widened to the whole upload transaction (staging/mips/views/bind groups), the capture test
  re-specified (valid descriptor to real exhaustion / HAL fault injection — an absurd-size
  descriptor only tests validation); the snapshot arithmetic had the active-decode term
  backwards and no sound per-decode bound exists → enforced per-active-job pool reservations;
  ring eviction APIs return every eviction and GPU textures retire on every ordinary eviction;
  upgrade failures recorded per representation; the remap return became
  `{moved, held_presented}` with the resize_hold rule tied to it; `Perf::full_seen`
  intersect-not-reset; Normal is slot-cap-bound (129 > 128) on the owner's display; Safe's cap
  is the shipping constant (1.5 GiB ≠ 1 500 000 000 B); task #53 is a *planned* wgpu-hal-patch
  precedent, not an applied one. Round 3 confirmed closed: the device-loss retraction, the
  startup windows, `commit_available`, the radius hard-clamp consumer, the bookkeeping matrix
  (minus perf), and that one snapshot closes the 7z double-sample TOCTOU.
- **Round 4: the owner's call.** The loop is converging (each round now finds implementation
  contracts, not architecture); the phase tests are where the remaining risk lives. Sign-off can
  proceed on rev 4, with or without a fourth pass.
