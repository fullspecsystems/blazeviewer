# Task 83 — Thumbnails panel (left-pane tab)

**Rev 2, 2026-07-12.** Rev 1 + the Codex review incorporated. All four P0 findings were
verified against the code and are real; the phase order below is rebuilt around them.
Key corrections vs Rev 1: the decode pool needs a multi-consumer request identity before
any thumbnail work can share it; T0 derivation moves to a **post-upload buffer handoff**
(a third option — neither event-loop resize nor pre-Outcome worker resize); T1 is
format-gated (zune JPEG ignores `allow_preview` and always full-decodes); absolute jump
already exists (`Playlist::jump_to`, the #43 compare flip) and just needs exposing;
"OOM impossible" is restated as budget-honesty. SwiftUI (macOS) ships first;
winit/egui parity follows. All caches RAM-only per ADR-018.

## Goal

A scrollable vertical strip of neighbor thumbnails in the **left pane**, as a second tab
beside the existing folder tree (**Folders | Thumbnails**), so the user can see what's
ahead/behind and click to jump — without stepping one photo at a time. Steady-state
browsing must cost **zero additional decodes**: thumbnails are a byproduct of decode work
the viewer already does, and thumbnail work must be provably unable to displace display
work.

## Settled decisions (owner, 2026-07-12 — unchanged from Rev 1)

- **Name:** *Thumbnails* (Preview.app precedent; "Filmstrip" connotes Lightroom's bottom
  bar). Tab pair **Folders | Thumbnails** in the left pane, replicating the Inspector's
  icon+label pill tab design. Icon: FA solid `images`, vendored per family.
- **Hotkey:** **`Shift+T`** → new `Action::Thumbnails`, pairing with `Shift+F`
  (FolderTree). Inspector-style per-tab semantics (open on tab / switch / close if
  showing). `Tab` (TogglePanels) hides it like any panel. `Cmd+F` reserved for find.
- **Order:** always **playlist/file order**, including random mode (highlight jumps; the
  strip shows in-order neighbors of wherever you landed).
- **No keyboard focus.** The strip is a mirror + mouse surface.
- **Fixed uniform cells:** image fit-within a fixed ~3:2 box at panel width,
  centered/letterboxed, middle-truncated filename below + hover tooltip; hover +
  current-item highlights. (Rotation must not reflow; O(1) virtualization; panoramas.)
- **Thumb size:** long edge ≤ `THUMB_EDGE = 512` for *generated* thumbs (the resizable
  panel stretch justifies 512 over 384); embedded previews stored at native size +
  preview tier — sizes need not match.
- **Wraparound:** nav wraps but the strip stays **linear** — no ghost cells. Scroll rule:
  smooth ≤ ~2 viewports, hard snap beyond (jumps, wraps).
- **Auto-follow** with manual-scroll disengage + next-nav re-engage (see FollowState).
- **Badges:** item-type only (Live Photo / video / animated); video thumbs = the #79
  poster path. Session rotation applied at draw time. Failed items → broken-image glyph.
- **Click-to-jump presents the thumb instantly** (see the present contract below).
- **The strip is "a view over an item list"** — a seam a future find can filter.

## Engineering contracts (Rev 2 — from the Codex review)

### 1. Decode-pool multi-consumer contract (P0 — prerequisite)

Today `DecodePool::set_targets` replaces the entire want-set, cancels anything absent,
and dedups by **bare item index** (`decode_pool.rs::set_targets`). A second consumer
calling it would cancel viewer prefetch; a viewer request would suppress a
differently-sized thumb request for the same item. Before any thumbnail scheduling:

- **Request identity** becomes `{item, epoch, purpose, fit-class, tier}` (purpose:
  Display | Poster | Thumb…). `Outcome` carries it back.
- **One merged scheduler**, priority order:
  `DisplayCurrent > DisplayPreview > DisplayFull > Poster > ThumbVisible > ThumbWarm >
  ThumbRefine`. AppCore already composes the prioritized list (`request_prefetch`);
  it now appends purpose-tagged thumb wants below all display/poster wants — no second
  pool, no per-consumer `set_targets` stomping.
- **Occupancy guard** (priority alone doesn't prevent inversion — workers busy on slow
  thumb decodes would make a fresh display job wait): thumb-purpose jobs are capped at
  **max(1, workers−2)** concurrent, are cancellable mid-decode, and expensive fill
  decodes (see T1 matrix) run **only while parked** (no held nav — the `sharpen_now`
  precedent).
- **Tests:** thumbnail demand can never cancel/dedup/delay display work; with a
  saturated queue no thumb job starts before any queued display job; per-purpose
  cancellation leaves the other consumer's jobs untouched.

### 2. T0 derive contract (P0)

`drain_results` + ring upload run on the event-loop side; resizing there is forbidden,
and resizing on the worker before it sends its `Outcome` would delay display readiness.
Neither. Instead:

- **Post-upload handoff:** after `drain_results` uploads an image's pixels into the ring,
  the CPU buffer (which would otherwise drop) is **moved** over a bounded channel to a
  dedicated **thumb-derive thread**. Zero clone, zero added display latency; the
  event-loop cost is one channel send. If any path later retains CPU pixels, an `Arc`
  share replaces the move — never a full-frame clone.
- **Bounded in-flight:** channel capacity 2–3 buffers = the in-flight byte cap.
  Channel full ⇒ skip the handoff (thumbs are best-effort; the warm-window fill
  regenerates misses later).
- **`derive_thumbnail(&DecodedImage) -> Result<ThumbSrgb8>`** must handle every pixel
  format the viewer produces:
  - RGBA8 + `ColorTransform`: downscale to ≤512 first (fast_image_resize), then apply
    the 3×3+TRC on the ~0.2 MP result (cheap; the shader normally does this — thumbs
    need it baked since SwiftUI/egui show plain sRGB).
  - `Rgba16F` (HDR): downscale in fp16, then tone-map (the extended-Reinhard the present
    pass uses, per-image `peak`) + sRGB-encode.
  - Alpha: keep straight alpha; shells composite over the panel background.
  - `Nv12` / video frames: never derived — video thumbs come from the poster path.
- **Perf gate (acceptance, not vibes):** corpus A/B with capture disabled/enabled —
  decode-to-upload and keypress→photon p50/p95/p99 must be unchanged within noise.
  "Single-digit ms on the derive thread" is the design intent, not the criterion.

### 3. T1 cold-fill format matrix (P0 — replaces Rev 1's blanket claim)

`allow_preview` is **not** a cheap-preview guarantee: `ZuneJpegDecoder` ignores it and
always full-decodes (`zune.rs`). Preview extraction is format-specific:

| Class | Formats | Cold-cell behavior |
|---|---|---|
| Embedded-preview capable | RAW (largest preview), HEIC (`thmb`), PSD, WIC thumbnail (Windows) | T1 fill: cheap, native-size, preview tier |
| Embedded-preview capable with small new work | JPEG via **kamadak-exif IFD1 thumbnail** (~160px, header-parse only — new but small; kamadak-exif already a dep) | T1 fill once wired; until then falls to next row |
| Full-decode-and-downscale fallback | JPEG (today), PNG, WebP, JXL, TIFF, … | **Expensive**: scheduled only while parked, capped concurrency, cancellable; cells stay placeholders while flying or while the queue is busy |
| Native scaled-decode capable (future T2) | JPEG DCT (the long-standing TODO), WebP `use_scaling` | Upgrades the fallback row when wired |
| Video | any | poster path (#79), thumb-sized |
| Unsupported/failed | — | placeholder / broken-image glyph, never retried in a loop |

Cold cells showing placeholders under load is **correct behavior**, not a bug.

### 4. Click-jump present contract (P0)

`Playlist::jump_to` **already exists** (the #43 flicker-compare flip; several call
sites). The work is: expose it as a public jump command (action + FFI) extracted from
the compare-jump flow — not new navigation machinery.

Presenting a far-away thumb instantly needs a renderer contract (the renderer presents
resident ring slots; a distant item isn't resident):

- If the item **is** ring-resident (near jump): present the resident slot — done.
- Else upload the cached thumb into a **dedicated transient preview slot** — not a ring
  slot, never evicts the ring. A ≤1 MiB upload through the existing staging path is
  ~20 µs at the measured 48 GB/s; allowed on the click pump (a discrete pointer action,
  not the keypress hot path), measured by the same harness as the perf gate.
- The full decode then replaces it through the normal preview→full swap; the transient
  slot clears on present. No thumb cached ⇒ placeholder background + normal decode flow
  (no flash of stale content).

### 5. Memory budgets (P1 — honest accounting)

Worst case is square: 512×512×4 = 1 MiB ⇒ the 256 MB canonical budget holds **~256
worst-case entries** (~365 at typical 3:2). Budgets are **exact allocated bytes**, and
each owner is bounded separately:

| Pool | Bound |
|---|---|
| Canonical Rust cache (CPU RGBA8) | 256 MB (config constant, A/B-able) |
| In-flight derive buffers | channel capacity (2–3 full-res buffers) |
| FFI transfer | one entry per request, copied once on demand — never a bulk clone |
| SwiftUI `CGImage` cache | visible + overscan cells only; evicted when cells leave demand |
| egui CPU-side + GPU textures | same visible+overscan bound; textures freed on leave |

Language fix: not "OOM impossible" but — *cache residency stays within the configured
budget; allocation failure degrades to a placeholder without aborting* (fallible
allocation on the derive/fill paths).

### 6. Generations & playlist mutation (P1)

`rebuild_playlist` reassigns indices and already drops every index-keyed cache — thumbs
follow the same convention:

- **Append-only `extend_playlist`** (streaming scan): preserve the cache (indices stable).
- **Delete / arbitrary rebuild:** **clear** the thumb cache (v1 simplification, owner
  accepted; safer than clever remapping).
- Every request/result carries **deck generation + request generation**; late results
  from a prior deck or demand window are discarded. Test: an in-flight thumb from the
  old deck can never install pixels under a new index.

### 7. Eviction classes (P1 — protect what's on screen)

Distance-from-current alone would evict what a manually-scrolled user is looking at.
Eviction order (last first):

1. **Pinned:** visible cells (never evicted while visible).
2. Overscan cells.
3. Current warm window (±64).
4. Everything else — farthest/oldest first.

If visible+overscan alone exceeds the budget (absurd panel on a tiny budget), keep the
nearest-visible subset deterministically, placeholders elsewhere. Invariants (proptest):
budget never exceeded counting in-flight reservations; a pinned entry is never evicted;
tier upgrades monotonic.

### 8. Shell bridge protocol (P1 — batching)

One naive "thumbs changed" per thumbnail would mean one wake + full model refresh each —
expensive during prefetch. Instead:

- A **monotonic dirty counter** on the cache; notifications **coalesced to ≤1 per
  pump/frame** (ride the existing `emit_panels_changed` cadence).
- A **lightweight item snapshot** (name, badges, state, rotation, current index) for
  list rendering — no pixel data.
- **Per-entry byte accessor**: the shell pulls exactly the entries whose cells are
  materializing, once each; entries record the generation the shell last pulled so
  unchanged cells transfer nothing.
- Shells **evict their `CGImage`/texture** when a cell leaves visible+overscan.

### 9. FollowState (P1 — shared, pure, tested)

"Manual scroll disengages; next nav re-engages" becomes a pure state machine in
pb-app-core (both shells drive it; SwiftUI cannot natively distinguish its own
programmatic scroll from the user's — the generation token does):

- States: `Following` | `Detached{anchor}` | `ProgrammaticScroll{target, generation}`.
- Events: `PanelOpened`, `UserScrolled`, `Navigation`, `Jump`, `PlaylistMutated`,
  `ProgrammaticScrollCompleted(generation)`.
- A scroll event during `ProgrammaticScroll` with a live generation is **not** a user
  scroll; a stale generation is. Tests: wrap, random jump, deletion, streamed append,
  scroll-during-animation.

## Phases (test-first; rebuilt order — pool + T0 correctness before any UI)

0. **Pure policy:** pb-core `ThumbCache` (tiers, exact-byte accounting, eviction
   classes, fill-plan ordering, generation discard) + `FollowState` — bytes stored
   outside the pure policy; proptest invariants above.
1. **Decode-pool multi-consumer:** request identity, merged priority order, occupancy
   guard, per-purpose cancellation; the displacement tests are the deliverable.
2. **T0 derive:** post-upload handoff + `derive_thumbnail` (all pixel formats) + the
   corpus perf gate. **Do not proceed if display latency regresses.**
3. **Core model + jump:** left-pane tab state, `Action::Thumbnails` (+keymap/help/menus),
   public absolute-jump command extracted from the compare flow, batched snapshot/FFI
   protocol (§8).
4. **macOS SwiftUI panel:** tabs, virtualized uniform rows, placeholders, bounded image
   cache, T0 thumbnails, FollowState wiring, click-jump for **ring-resident** targets.
5. **Transient preview slot:** the cold-jump instant present (§4).
6. **T1 cold fills**, format-gated per the matrix (incl. the kamadak-exif JPEG IFD1
   path).
7. **winit/egui parity** (show_rows, texture registry, resize handle).
8. **T2 refine** — only after native scaled-decode exists and benchmarks justify it.
   Privacy + docs ride the last shipped phase (no-trace over fs **and ZIP and 7z**
   thumbnails-open sessions; CHANGELOG; CLAUDE.md keymap note).

## Acceptance gates

- Hidden-panel baseline unchanged (capture gated on first open; before that: zero RAM,
  zero pool time, zero wakes).
- Thumbnail work never displaces display work (the §1 tests + the §2 perf gate).
- Cache accounting covers **all** owned buffers (§5 table), not just the canonical cache.
- Stale results cannot cross a deck generation (§6).
- Privacy: no-trace tests over filesystem, ZIP, and 7z sessions with the panel open.

## Open questions

- Final budget constant (256 MB default; config-constant, A/B-able).
- Wire the kamadak-exif JPEG IFD1 path in phase 6 or defer (placeholders may be
  acceptable for v1 cold cells given T0 covers everything visited).
- Whether the "↩ current" re-engage pill is needed or next-keypress re-engage suffices.
