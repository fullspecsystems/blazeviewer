# Task 106 — Make Blaze feel fast: archive opens, resize, zoom, thumbnails

**Status:** planned — **rev2** (2026-07-17). Measured baseline in hand (`PB_PERF`); the
instrumentation (#106.2) is shipped. **Codex (gpt-5.6, xhigh) reviewed rev1 — its findings are
incorporated below (see "Codex review — incorporated" and the reworked #106.7).**
**Task id:** 106 (`tasks.json`). Depends on nothing; #106.2 done.
**Scope:** `pb-app-core` (residency, prefetch, decode, settings), `pb-render` (full-res
retention across invalidation), `pb-source` (partial reads for #106.5/#106.1), the egui +
SwiftUI settings panes. Prime directive: **the app must FEEL fast.** Every claim here is a
`PB_PERF` number, before and after.

---

## The measured baseline (owner ran `PB_PERF=1` on an SMB `album.zip`, 8 × ~36 MP JPEGs)

| metric | cold (first run) | warm (subsequent) |
|---|---|---|
| open → first photo | **7068 ms** | — |
| open → all 8 cached | 8302 ms | — |
| resize Fit↔1:1 | 711–993 ms | **390–474 ms** |
| per-item read | — | **~37 ms / 33–38 MB** (≈900 MB/s) |
| per-item decode | — | **~300–500 ms** (36 MP JPEG) |

**Three findings, load-bearing — do not re-derive:**

1. **The cold read is the 7 s first-photo, one time.** Warm, the OS SMB cache serves the
   entry in ~37 ms. So the encoded-byte cache (#106.1) is a *cold / re-open* win; it does
   **nothing** for a warm resize.
2. **Warm, the DECODE (~400 ms/photo) is the whole steady-state cost.**
3. **Every Fit↔1:1 toggle re-decodes the WHOLE resident ring** (the log shows items 0–7
   re-decoding around each `resize→on-screen`), because a scale change bumps the geometry
   epoch and `invalidate_geometry` rebuilds the ring empty. Toggle back and forth → re-decode
   every time. This is the biggest felt-slowness and the owner's central complaint.

## The mental model (why it's built this way, and where it's wrong)

**Decode-to-fit is correct for blazing** (never decode more than the screen shows — the prime
directive) and it's why the ring stays small and fast. `finalize` (`pb-decode/common.rs:84`)
downscales the decode to the fit box in Fit mode and **discards the full-resolution pixels**;
the true resolution survives only as `orig_width/orig_height`. `ViewTransform::base_scale`
(`pb-render/view.rs:110`) then draws the bound texture at `min(fit)` for Fit, `1.0` for
Original — *of the texture's own pixel dimensions*.

That model breaks the moment you **stop and zoom**: there is no full-res to rebind, so 1:1
re-decodes from scratch, and toggling back and forth re-decodes each way.

---

## The optimizations, prioritized by felt impact

| # | optimization | wins | cost |
|---|---|---|---|
| **106.7** | **Hold full-res for the PARKED window (current ± N)** | **the zoom/toggle win — warm & cold** | VRAM (bounded, configurable) |
| 106.5 | Preview-first for big JPEGs (embedded EXIF thumbnail) | the **cold** 7 s first-photo → ~instant blurry → sharp | partial read + a JPEG preview path |
| 106.6 | Resize jumps to true-1:1 *size* instantly, then sharpens | the two-stage "grow → jump" | mostly **subsumed by 106.7** |
| 106.3 | Thumbnails: keep a tiny warm window when parked | panel-open shows neighbors instantly | a few off-thread downsamples |
| 106.1 | Encoded-byte cache in front of `source.bytes()` | **cold** re-read / re-open the same archive | bounded RAM |
| 106.2 | ✅ DONE — `PB_PERF` instrumentation | measurement | — |

**Recommended build order:** 106.7 first (biggest felt win, and it subsumes 106.6), then
106.5 (kills the cold 7 s), then 106.3 (thumbnails), then 106.1 (cold re-read). Each behind a
seam, each measured with the same `PB_PERF` lines.

---

## 106.7 — Full-res retention for the parked window (the centrepiece)

**Goal:** while blazing, keep decode-to-fit (unchanged). When **parked**
(`held_nav().is_none()`), decode the current photo **and its sequential neighbours** at full
resolution and **hold them**, so Fit↔1:1↔Fit are instant *rebinds* — the ~400 ms decode is
paid **once, in the background**, never on a toggle, and never again until you navigate away.

> **Codex rev2:** rev1's "carry full-res slots forward, present unchanged, byte-budget is the
> backstop" was too loose. Retention is sound and correctly prioritized, but it needs (1)
> **typed representation identity**, (2) a **content generation separate from the geometry
> epoch**, (3) an **explicit synchronous re-present** on the geometry change, (4) **real
> eviction** on upgrade, and (5) **scheduler isolation** so it doesn't double-decode or starve
> the screen. All five are now in the design.

### Design decisions (owner, 2026-07-17)

- **Window = sequential current ± N**, default **N = 1** (previous + current + next). *Not*
  the random-deck neighbours (deferred). **Budget order: current first, then the compare pin
  (§ pin), then the sequential neighbours** — the pin is a promised instant rebind and must not
  be evicted by the window (`app_core_impl.rs:5152`).
- **Configurable, bounded (not "OOM-safe").** A Settings knob (`full_res_radius`): `0` =
  current only, `1` = ±1 (default), up to a small cap. It caps how many originals the parked
  tier *requests*; the ring byte budget is the hard admission gate. **"OOM-safe" is too strong**
  — the budget is a fixed `RING_BUDGET_BYTES` (~1.5 GB), not tied to adapter memory, and the
  renderer's `held` texture + staging buffers sit *outside* it. Call it "bounded," size the
  default conservatively, and let a slow box dial to 0.

### (1) Full-res is a **typed representation**, not `preview_resident == false`

**Codex P0.** `preview_resident == false` does **not** mean full-res: decode-to-fit also sets
`is_preview = false` after downscaling (`common.rs:84`), and `request_prefetch` then treats it
as finished (`app_core_impl.rs:5200`). The ring records only `item` — no representation
(`ring.rs:16`).

Add a typed `Representation` carried through **`Want` → `DecodeKey`/pool dedup → `Outcome` →
`ResidentRing` slot metadata**:

```rust
enum Representation { Fit { geometry_epoch: u64 }, Original }
```

`Fit` is geometry-epoch-gated exactly as today; `Original` is geometry-*independent*.

### (2) `Original` bypasses the **geometry** epoch — but is fenced by a **content generation**

**Codex P0 (the sharp one).** The same `epoch` is bumped for *both* geometry changes
(resize/scale — safe to keep an Original across) **and content changes**: deck rebuild
(`app_core_impl.rs:3068`), saved EXIF rotation (`:627`), delete/undo, source replacement.
Bypassing the epoch indiscriminately would present the **old deck's item N as the new deck's
item N** — a real correctness bug.

Fix: split the two. Keep a **`content_gen`** (bumped on deck rebuild, source replace,
save-rotation, delete/undo, teardown) distinct from the geometry `epoch`. An `Original` slot is
valid iff its `content_gen` still matches; it may cross a **geometry** epoch but is **purged on
any content_gen bump**. Fit decodes, display previews, SVG fit rasters, and video posters stay
fully geometry-gated. Unsaved (session) rotation is shader-only → safe to keep the Original;
**saved** EXIF rotation is a content edit → purge.

### (3) The retained hit must be **explicitly re-presented** (or the app hangs)

**Codex P0.** After `invalidate_geometry`, `presented_epoch` is stale and
`refresh_after_geometry_change` (`:5085`) sets the view + queues work but **never calls
`try_present_target`**. If the retained Original makes `request_prefetch` skip the decode, *no
outcome ever arrives* to call `present_item` → the app is stuck `target_pending` forever
(spinner that never resolves).

Fix: on the geometry change, if the current item has a valid resident `Original`,
**synchronously rebind it and call `mark_resolved`** (no decode, no upload) — a real present at
the new epoch. Only fall through to the async re-decode when there's no retained hit.

### (4) Decode the Original **once**, derive the Fit from it — no double-decode, no quality loss

**Codex P0 + P1.8.** Appending an `fit=None` tier *below* the existing sharpen/`prefetch_fulls`
would decode the **same source twice** — and for JPEG the fit path *already* does a complete
full-resolution decode before discarding the big buffer (`zune.rs:23`, `common.rs:84`). Also,
**GPU-downscaling an Original for Fit uses bilinear (`gpu.rs:1285`) while decode-to-fit uses
Lanczos** — serving Fit from the Original is a visible quality drop.

Fix: for a parked item selected for full-res, the Original request **replaces** the fit sharpen
(not follows it). Decode full-res **once**; from that one buffer produce **both** representations
— upload the `Original` texture *and* a Lanczos-downscaled `Fit` texture (reuse
`downscale_to_fit`). Hold both typed slots. Then:
- **Fit mode** rebinds the `Fit` slot (Lanczos, unchanged quality).
- **Original mode** rebinds the `Original` slot (true 1:1).
- Both toggles are instant; neither re-decodes; Fit keeps its Lanczos sharpness.

(If holding two textures per parked item is too much VRAM at the chosen radius, the fallback is
to serve Fit from the Original by GPU-downscale — but that must be **golden-image/perceptually
tested** against the Lanczos path first, not assumed equivalent.)

### (5) Scheduler isolation — no starving the on-screen photo, no obsolete-original pile-up

**Codex P0.** The pool dedups only by `(item, purpose)`, ignoring representation
(`decode_pool.rs:181`) — an already-running *fit* job can't silently become an *original* job;
the typed `Representation` must be part of the dedup key. And cancellation only discards the
*result* (`engine.rs:382`); it doesn't stop an in-flight 36 MP decode — so rapid taps can leave
every worker finishing obsolete originals.

Fix: (a) a **full-res occupancy cap** / a small number of reserved display workers so originals
never consume the whole pool; (b) **debounce** neighbour-original requests (fire after a short
park dwell, not on the first settled frame); (c) representation in the dedup key; (d) originals
strictly below the on-screen sharpen + previews in priority.

### (6) One typed residency manager + an explicit renderer retain/remap API

**Codex P1.6.** Carry Originals through the **unified ring** with slot metadata
`{ item, content_gen, representation, bytes }` — **not** a separate presenter
`HashMap<item, Texture>` (that would be a second eviction policy + duplicate identity + split
budget). The renderer owns the textures, so "core re-hands slots" is *not* possible today:
`reserve_ring` (`gpu.rs:2870`) destroys the vector after extracting only the displayed slot, and
**capacity changes between Fit and Original**. Define an explicit **retain + old-slot→new-slot
remap** contract on the `Renderer` so the surviving Original textures move into the rebuilt ring
instead of being dropped.

### (7) Real eviction on upgrade (the byte budget is not currently a hard backstop)

**Codex P0.4.** `set_slot_bytes` (`ring.rs:218`) performs **no eviction** — it permits an
over-budget ring until some later reservation, so three parked upgrades can leave a persistent
overage. Also the accounting is off: `slot_bytes_estimate` still estimates *fit*-sized RGBA8 in
Fit mode so capacity doesn't shrink for a mixed ring (`app_core_impl.rs:5252`); **HDR is 8
bytes/px, not 4**; one item larger than the whole budget is deliberately admitted (`ring.rs:150`);
the `held` texture + staging sit outside the budget (`gpu.rs:1864`).

Fix: replace the in-place `set_slot_bytes` upgrade with an **atomic upgrade admission** that
evicts lower-priority slots *before* the upload, using the outcome's **actual** byte length and
correct bytes-per-pixel. Fix `slot_bytes_estimate` for the mixed (fit + original) ring and HDR.

### (8) Exclusions — not every item has a geometry-independent "full resolution"

**Codex P1.8.** The parked-original tier must **exclude**:
- **Videos and archive doors** — no meaningful still full-res (doors are a 1×1 sentinel; videos
  are streamed).
- **SVG** — `fit=None` rasterizes at *natural* size, while Fit deliberately rasterizes for the
  viewport (`svg.rs:42`); an SVG "original" does **not** satisfy arbitrary Fit geometry. Keep SVG
  fully geometry-gated (re-rasterize per fit).
- **HDR** — 8-byte accounting + a max-texture-dimension policy (a huge original can exceed the
  GPU's max texture size).

### The compare pin

The pin rides every want-list at top-2 and is promised an **instant rebind** (`:5152`). It may
be a *distant* item, outside `current ± N`. So the full-res budget order is **current → pin →
sequential neighbours**, and window-eviction must never drop the pinned Original.

### Config + Settings

- New field on `pb_app_core::Settings` (`settings.rs`, `#[serde(default)]`): `full_res_radius:
  u8` (default 1), clamped to `[0, CAP]`.
- Surfaced in **both** panes (egui `settings_ui` + SwiftUI `SettingsView` / `SettingsFormFfi`),
  mirroring `scale_mode`. Label ~ "Keep full resolution for nearby photos (Off / Current /
  Nearby)".

### Tests (Codex's acceptance list, adopted)

Pure `pb-core` + `pb-app-core`, no GPU where avoidable:
- A **geometry** epoch bump keeps a valid `Original`; a **content_gen** bump (deck rebuild,
  save-rotation, delete/undo, source replace) **purges** it.
- **Same-index deck replacement** never presents the old item's Original as the new item.
- A geometry toggle on a retained Original issues **no decode** (assert the pool gets no
  job) **and** the retained hit becomes `target_caught_up` (§3 — proves no hang).
- **Exact-budget in-place upgrade** evicts before upload; the ring is never left over-budget;
  HDR uses 8 B/px.
- **Compare-pin retention** across the window; a distant pin's Original survives.
- **Rapid navigation** with obsolete originals in flight — occupancy cap holds, workers aren't
  all stuck on 36 MP decodes.
- **Heterogeneous / HDR sizes**; **SVG / video / door exclusion**; **renderer slot remap** across
  a Fit↔Original capacity change.
- `PB_PERF`: resize→on-screen for a retained Original is **~0 ms** (rebind) vs ~400 ms; a
  first-park→full-res-ready line is emitted.

### (9) Gigapixel / large-image safety ceiling (owner question, 2026-07-17)

**What's already safe (VRAM/GPU):** `upload_image`'s `clamp_to_max` (`gpu.rs`) downscales any
RGBA8 image beyond `max_texture_dimension_2d` (8192–16384) *before* creating the texture, so a
40000-px panorama uploads as a ≤16384² texture instead of failing device validation — the
resident texture is bounded (~1 GB worst case at 16384²). The ring's `RING_BUDGET_BYTES` = 1.5 GB
plus the "a single over-budget image is admitted **only when nothing else is resident**" rule
(`ring.rs:150`) means the ring never holds *many* huge images — one huge current photo evicts the
rest, then shows alone. **This part is fine and #106.7 inherits it.**

**What is NOT safe today (decode-side RAM — a pre-existing gap, not created by #106.7):**
decode-to-fit's native scaled decode (JPEG DCT, WebP) is still a **TODO** — every format
currently decodes to a **full-resolution RGBA8 buffer in RAM** and *then* `finalize`
Lanczos-downscales (`common.rs:84`). There is **no `image::Limits` / max-pixel guard** on the
decode path (only the macOS ImageIO backend has a `MAX_DIM = 100_000` sanity bound). So a true
gigapixel image — e.g. 40000×25000 = 1 Gpx × 4 B = **4 GB** — allocates that buffer during
decode, and an OOM there is an **uncatchable abort** (`catch_panics` can't catch an allocation
failure). This is a latent crash risk **for any mode**, today.

**#106.7's obligation — do not make it worse, and add the ceiling that should exist anyway:**
- **A full-res retention ceiling.** Never *request or retain* an Original whose decoded size
  exceeds a byte/pixel ceiling (e.g. cap on `orig_width·orig_height·bpp`, sized against
  `RING_BUDGET_BYTES` and `max_texture_dimension_2d`). Above the ceiling the item stays
  **fit-only** — shown downscaled, which is **all a screen can display anyway** (1:1 of a
  gigapixel is a meaningless ~8 MP crop, and the on-screen texture is already `clamp_to_max`'d).
  So "Original mode" on a gigapixel legitimately means "the clamped, screen-sized view," not a
  4 GB buffer. The zoom-instant win simply doesn't apply to images too big to zoom into
  meaningfully — and we don't pretend it does.
- **A decode-side ceiling (pre-existing fix, worth doing here).** Add an `image::Limits`
  memory bound / a max-pixel pre-check so a gigapixel decode is **refused or scaled** rather than
  OOMing — for *all* modes, not just the parked tier. This closes the latent abort. (Longer term,
  wiring true scaled-decode — JPEG DCT, WebP `use_scaling` — decodes small directly and removes
  the transient buffer entirely; out of scope here, but the ceiling is the safety net until then.)
- **Acceptance test:** a synthetic very-large image (e.g. 30000×20000) opens without OOM, shows
  downscaled, and is **not** retained as an Original (stays fit-only); `full_res_radius` never
  admits it into the parked window.

---

## 106.5 — Preview-first for big JPEGs via the embedded EXIF thumbnail

**Goal:** the cold 7 s first-photo becomes ~instant. Big JPEGs embed a small thumbnail near
the file start (owner's file: `JPEGInterchangeFormat 240, len 22467` = 22 KB). We already have
`pb_decode::exif_thumbnail` (`thumb.rs`) but use it **only** for the thumbs strip; the viewer
decodes the full JPEG and shows nothing until done.

**Mechanism:**
1. **Partial entry read.** `source.bytes()` reads the whole entry today. Add a
   `bytes_prefix(item, max)` (or a range read) to `ItemSource` so the JPEG's EXIF/thumbnail
   (first ~128 KB) can be read without the full 39 MB. ZIP can read a bounded prefix of an
   entry; the plain-tar/eager sources already have the bytes.
2. **JPEG preview-first path.** In `decode_named_bytes` / the JPEG backend, when
   `allow_preview` and an EXIF thumbnail is present, return it as a `is_preview = true` image.
   The existing preview→full upgrade (`drain_results`, `preview_resident`) then swaps in the
   full decode when it lands — the same model RAW/HEIC already use.
3. Show the thumb fit-to-screen (blurry) in ~100 ms; sharpen at the full decode.

**Codex P1.9 — the preview must carry the FULL image's dimensions, not the thumbnail's.**
`exif_thumbnail` reports the *thumbnail's* ~160 px as `orig_width/orig_height` (`thumb.rs:189`).
If used verbatim as the viewer preview, the metadata panel and Original-mode placement snap to
160 px, and `meta_cache` is **not refreshed when the full upgrade lands** (`app_core_impl.rs:6180`
uploads the full but never re-inserts meta). Fix: parse the **main JPEG SOF dimensions from the
prefix** and stamp them as `orig_width/orig_height` on the preview, **or** overwrite `meta_cache`
(and the placement) when the full decode upgrades in. Without this the preview is placed wrong
and the panel lies until the next event.

**Scope note:** niche but exactly where it matters (giant Lightroom/camera exports over SMB);
small JPEGs are already fast. Applies to filesystem *and* archived JPEGs.

**Risks:** partial reads must not break the no-trace guarantee or the panic-safety wrapper;
the EXIF thumbnail may be absent/oriented differently — degrade to the current full-decode.

---

## 106.6 — Resize jumps to true 1:1 size instantly, then sharpens

Largely **subsumed by 106.7**: once the parked photo is held at full-res, a Fit↔1:1 toggle
rebinds it at true dims — no intermediate low-res size, no re-decode. **If 106.7 ships first,
106.6 is only needed as a fallback** for the first toggle before the full-res is ready (or when
`full_res_radius = 0`): place the quad at the true size (`orig_width/orig_height`) and let the
GPU upscale the fit texture (soft), then sharpen. Keep as a small follow-on; don't build twice.

---

## 106.3 — Thumbnails: keep a tiny warm window when parked

**Goal:** opening the Thumbnails panel shows the neighbours instantly instead of a placeholder
wall. `thumbs_capture` (`app_core_impl.rs:2160`) already downsamples every decoded full into
the thumb store — but it's gated on `thumbs.enabled`, which only flips true on the **first
panel open** (`thumbs.rs:107`). So browse-then-open re-reads all N from SMB via `Job::thumb`.

**Mechanism (owner call — bounded, off by default cost):** when **parked**
(`held_nav().is_none()`), derive thumbs for **current + next 2–3** already-resident items
off-thread on the existing derive thread — pure downsample, **zero source reads**. Blaze pays
nothing (parked-gate); RAM capped (a handful). Spawn the derive thread eagerly (or lazily on
first parked decode). The ring-reuse (GPU-downsample a resident VRAM texture) stays a
**fallback** for evicted items.

**Interaction with 106.7:** the parked full-res tier and the parked thumb tier both fire on
park — order the thumb derive **after** the full-res decode so zoom-readiness wins.

---

## 106.1 — Encoded-byte cache in front of `source.bytes()`

**Goal:** stop re-reading the same entry from disk/network on re-decode. Bounded RAM-only LRU
of encoded (inflated) bytes, consulted before `source.bytes()` (`engine.rs:512`). **Reframed by
the measurements:** this is a **cold / re-open** win (the warm read is already 37 ms), so it's
**lower priority** than 106.7/106.5. Still worth it: re-opening the same archive, the second
zoom before the OS cache is warm, and slow/uncached shares.

**Mechanism:** an `Arc<[u8]>` LRU, bounded by a byte budget, dropped on nav-away / Esc
(privacy #2 — same category as `pending_uploads`). Decide archive-only vs all-sources (archives
have expensive `bytes()`; `FsSource` leans on the OS cache). Behind a seam; A/B via `PB_PERF`.

**Codex — the "re-open" claim needs stable content identity.** A cache keyed by
*source-generation* + dropped on nav/source-away **cannot survive re-opening the same source**
(a new open = a new generation, and the entries were already dropped). A true same-session
re-open cache needs a **stable content identity** — container path + version/mtime + entry
identity — that survives deck replacement while staying RAM-only. Either build that identity, or
**drop the "re-open win" from the pitch** and scope this to same-deck re-reads only (which the
warm OS cache already largely covers — reinforcing that this is the lowest-priority item).

---

## Cross-cutting: measurement discipline

- Every optimization lands **behind its seam/setting** and is validated with the same
  `PB_PERF` lines (`open→first-photo`, `open→all-cached`, `resize→on-screen`, plus the
  `item N: read/decode` split). Report before/after; never a claim without a number.
- The macOS GUI can't run in the agent env (screen capture blocked, `onAppear` never fires) —
  the **owner runs `PB_PERF` on the real SMB volume** for the real numbers. Pure logic and the
  wiring are unit-tested headless (`perf.rs`, `perf_hooks_fire_from_the_real_present_and_resize_paths`).

## Phases (execution order for a fresh context)

1. **106.7** — full-res parked retention + the setting. The centrepiece; subsumes 106.6.
2. **106.5** — EXIF preview-first (partial read + JPEG preview path).
3. **106.3** — thumbnail warm-window when parked.
4. **106.1** — encoded-byte cache (cold/re-open).
5. **106.6** — only if the first-toggle-before-full-res-ready gap needs the upscale fallback.

## Codex review (gpt-5.6, xhigh) — incorporated in rev2

Verdict: *"Do not execute #106.7 exactly as [rev1] wrote it. Retaining full-resolution textures
is sound and correctly prioritized, but the design lacked representation identity,
content-generation fencing, hard upgrade budgeting, and scheduler isolation."* All findings are
folded into §§1–8 of #106.7 and the #106.5/#106.1 notes above. Map, for traceability:

| Codex | severity | where it landed |
|---|---|---|
| Full-res must be a typed representation (not `preview_resident==false`) | P0 | #106.7 §1 |
| Only `Original` bypasses the *geometry* epoch, and needs a *content* generation | P0 | #106.7 §2 |
| Retained hit must be explicitly re-presented or the app hangs `target_pending` | P0 | #106.7 §3 |
| Byte budget isn't a hard backstop; fix eviction + HDR/held accounting | P0 | #106.7 §7 |
| Parked tier double-decodes + can starve the screen; dedup ignores representation | P0 | #106.7 §4, §5 |
| One typed residency manager + a real renderer remap API (not a `HashMap`) | P1 | #106.7 §6 |
| Preserve the compare-pin instant-rebind invariant | P1 | #106.7 § pin |
| Exclude video/door/SVG/HDR; Lanczos vs bilinear is a quality change | P1 | #106.7 §4, §8 |
| EXIF preview must carry the *full* SOF dimensions, refresh meta on upgrade | P1 | #106.5 |
| #106.1 "re-open" needs stable content identity, not source-generation | — | #106.1 |
| Priority reprioritization (full-res/preview before byte-cache) is **correct** | ✓ | confirmed |

**The decided answers** to rev1's open questions (were: separate map vs ring; priority;
config surface; partial reads):
1. **Unified ring with typed slot metadata** `{ item, content_gen, representation, bytes }` +
   an explicit renderer **retain/remap** API — *not* a parallel `HashMap` (Codex P1.6).
2. **Original bypasses geometry, not content** — split `content_gen` from the geometry `epoch`
   (Codex P0.2).
3. **Originals replace the fit sharpen** for selected parked items (decode once, derive both
   representations), strictly below the on-screen sharpen, with an occupancy cap + debounce
   (Codex P0.4/P0.5).
4. **Config:** `full_res_radius: u8` (0 = current / 1 = ±1 default), rendered as an Off /
   Current / Nearby control.

**Still open for execution (small):**
- 106.5 partial reads: `bytes_prefix(item, max)` / range read on `ItemSource`, and how the
  eager 7z/tar sources (bytes already in RAM) participate — likely a no-op prefix for them.
- The exact VRAM default for `full_res_radius = 1` given the corrected accounting (§7) — pick a
  conservative `RING_BUDGET_BYTES` interaction and measure.
