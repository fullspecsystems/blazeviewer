# Streaming playlist — browse before the scan finishes (plan & handoff)

_Written 2026-06-30. Authoritative plan for the "start browsing at the earliest
possible moment" track. Reviewed/agreed in discussion with the owner; **pending
final sign-off before execution.** Grounded in a full read of the open/nav/source
machinery and the overlay/input rendering layer (file:line refs throughout)._

_**Updated 2026-06-30 after a Codex review pass.** Now specifies: all entry points
(startup launch + `Ctrl+R`, not just runtime `open_input`); first-batch **bootstrap
vs. extend**; **delete-during-scan** semantics; **off-thread** snapshot construction
(replacing the unmeasured "negligible" claim); **`sort_by_file_name`** == today's order
(empirically verified — no custom walker, no behavior change); **random state on extend**;
a real **`CancelScan` action** (Esc is `Quit` today); and **found-vs-published** in the
count chip. See the "Review round 1/2" deltas marked **[R1]**/**[R2]** throughout._

---

## The thesis

Today PhotoBlaze is **"resolve everything, then browse."** Opening a folder runs a
full off-thread walk **+ a global sort**, and only when that completes does the
playlist exist and the first photo appear (`begin_dir_scan` → `poll_dir_scan` →
`rebuild_playlist`, `main.rs:1874-1939`). Opening a *single file* is no better: a
lone file is planned as a **flat scan of its parent** with the cursor pointing at
the file (`open.rs:94-111`), and the file's index is resolved by `position()` in
the **sorted** list (`open.rs:128-133`) — so even one photo waits for its whole
folder to be read and sorted.

We want **"browse immediately, resolve in the background."** The user-pointed-at
image (an exact file, or "first of folder") should display as soon as its pixels
exist; the rest of the playlist fills in around it while the user is already
flicking. This is a pure perceived-latency win — squarely on the Prime Directive.

**Good news from the investigation:** the lower layers are already streaming-shaped.
The scan runs off-thread with a progress handle (`ScanProgress`); the decode pool
carries the `source` `Arc` **per job** and never reads `len()` (`decode_pool.rs:71-84`;
its test source returns `len() == usize::MAX`); and the resident ring + residency
planner index **by item, not modulo N** (`ring.rs` `by_item: HashMap<usize,usize>`,
`cache.rs:24-52`). So "what to decode and show" is already count-agnostic. The
resistance is concentrated in three spots, all about *the playlist being a fixed,
finished, sorted array*.

---

## Decisions locked in discussion (2026-06-30)

1. **Append-only order, matching today exactly via `sort_by_file_name`. [R2]** The
   playlist only ever *grows by appending* — existing indices never move under the user.
   **[R2] Empirically verified** (`PathBuf::cmp` test, see Problem 1): today's
   `paths.sort()` is **component-wise** (`Path`/`OsStr` `Ord`), **not** byte-string — and
   component-wise order is **identical** to `walkdir`'s `.sort_by_file_name()` in every
   case, including the `a.jpg` vs `a/b.jpg` boundary (both put `a/b.jpg` first). So we use
   plain **`walkdir.sort_by_file_name()`** — no custom comparator, no behavior change,
   flat or recursive. (R1's "custom `name + '/'` comparator" was solving for a byte-string
   baseline that isn't what the app uses — dropped. Natural sort `img2 < img10` and
   files-first remain separate deferred opt-ins.)
2. **Random (Enter) is decode-on-demand during a scan, not prefetched-ahead** (owner
   preferred deck-update over a "random unavailable" toast; refined to avoid prefetch
   thrash). The deck regenerates for the grown universe (lazily, on actual random use —
   the regen is a sub-ms integer shuffle, no I/O), but **the random-ahead prefetch window
   is suppressed while a scan is in flight** so the ring doesn't decode-then-evict a deck
   that changes every batch. A random keypress mid-scan decodes that one target on demand
   (preview-first softens it). At completion the final deck is built once and normal
   random-ahead prefetch resumes (Enter is a rebind again). The "visit each once per cycle,
   reversible" guarantee is relaxed only during the live scan. _Rationale + the deferred
   "extendable deck" alternative are in the Problem 3 / Random sections below._
3. **Ambient progress, not a modal.** A count indicator in the **top-right corner**,
   next to the existing loading pie. The growing count *is* the scan progress.
4. **The "Scanning Folder" dialog becomes first-image-gated.** It only appears if the
   **first image** still isn't on screen after the delay; the instant the first image
   displays, the dialog closes and the ambient chip takes over. With streaming this is
   rare. **Archives keep the dialog unchanged** — a solid `.7z` genuinely can't stream
   (you must decompress the whole thing before entry _i_ exists), so blocking + dialog
   stays correct there.
5. **Cancel lives on the image** as a clickable control (FA stop icon + label). This
   requires **net-new overlay hit-testing**, which we'll build as a reusable precedent —
   the same mechanism the upcoming **EXIF "copy" buttons** will use.
6. **Scope: the full thing**, sequenced in three risk-ordered phases (below).

---

## The three structural problems & how we solve them

### Problem 1 — Order stability (the sort)

Today: one global `paths.sort()` after the whole walk (`resolve_source`, `main.rs:4769`;
`collect_images` deliberately appends unsorted, `main.rs:4534`). Showing images before
that sort would shift indices/neighbors as more arrive.

**Solution: never reorder, only append — via `walkdir.sort_by_file_name()`, which
reproduces today's order exactly. [R2]** The first draft assumed `paths.sort()` was a
byte-string sort and feared `sort_by_file_name` would diverge. **That was wrong, and it's
now empirically settled:** `Vec<PathBuf>::sort()` uses `Path`'s **component-wise** `Ord`
(each path component compared as an `OsStr`), and a quick `PathBuf::cmp` experiment shows
it is **identical to `sort_by_file_name`** in every case — `a.jpg` vs `a/b.jpg` →
**both** put `a/b.jpg` first (component-wise: first component `"a"` < `"a.jpg"`); the
byte-string order (which would put `a.jpg` first because `.` < `/`) is the one that
differs, and the app never used it. So the streaming worker just iterates
`WalkDir::new(root).sort_by_file_name()` (depth-first, entries sorted per directory),
emitting image matches in batches as it goes. Append-only and streamable, and it keeps
walkdir's symlink/error safety — no hand-rolled walker, no comparator.

- Flat (no subdirs): trivially identical to today.
- Recursive: identical to today (`sort_by_file_name` == `paths.sort()`) — **no behavior
  change**. (Multi-root drag-drop streams per-root concatenated rather than globally
  interleaved — a minor, arguably-nicer divergence only for that edge case.)
- The guarantee test: build a tree with the edge cases (`a_subdir/x.jpg` vs `z.jpg`; the
  `a.jpg` vs `a/b.jpg` prefix edge; `img2` vs `img10`) and assert the streamed/walked
  order **equals the same paths run through `paths.sort()`** — directly pinning "no
  behavior change" rather than reasoning about it. (Natural sort is a separate, deferred
  opt-in.)

### Problem 2 — The growable playlist

`FsSource` is immutable: two owned `Vec`s built once (`pb-source/src/lib.rs:96-128`),
shared as `Arc<dyn ItemSource>` across decode threads as `&self`, returning **borrowed**
`&str`/`&Path` (`trait`, `lib.rs:52-94`). True interior mutability is awkward (can't
borrow through a lock), so:

**Solution — snapshot-swap, built off-thread, time-bounded batching. [R1]** The scan
worker streams matches and **constructs each `FsSource` snapshot itself, off the event
loop**, sending a **ready-to-use `Arc<dyn ItemSource>`** over the channel; the event loop
then swaps `self.source` in **O(1)** (just an `Arc` store). This sidesteps the unmeasured
"negligible" claim entirely — the per-batch O(N) work (cloning the cumulative `Vec` +
rebuilding names in `FsSource::new`, `pb-source/src/lib.rs:105`) never touches the hot
thread. Batches are **time-bounded (~150 ms)**, not per-count (per-count would risk O(N²)).
No `ItemSource` trait change.

- **[R1] Measure, don't assert:** add a Criterion microbench for snapshot construction at
  **10k / 100k / 1M** paths, and an instrumented per-scan log of snapshot build time. If a
  1M-path folder shows the O(N × batches) worker CPU mattering, escalate to a **segmented
  append-only buffer** (chunked `Vec`, never reallocated, readers index stable chunks → O(1)
  grow, no re-clone) behind the same swap seam. Also microbench `prefetch_targets`, which
  allocates `vec![false; len]` every call (`prefetch.rs:16`) — reuse a scratch buffer if it
  shows up.
- Because indices are **append-only stable**, the index-keyed caches (`meta_cache`,
  `rotations`, `failed`, `preview_resident`, `upgrade_done`) stay valid across a grow —
  `extend_playlist` must **not** clear them (unlike `rebuild_playlist`). Index _i_ always
  means the same photo. (Exceptions that *do* shift indices: the single-file provisional
  handoff — Phase 1 — and a user **delete** mid-scan — see Edge cases.)
- The decode pool needs nothing: `set_targets` already takes the current `&self.source`
  each call (`main.rs:945`), and jobs carry their own source `Arc`.

**[R1] First non-empty snapshot bootstraps; later snapshots extend.** The piece that
actually *displays* the first image — sets `target_item`/`displayed_item` and starts the
first decode — is `rebuild_playlist` (`main.rs:2654`), via `load_current_sync`. A bare
`extend_playlist` only grows `len` + re-prefetches and would show nothing. So:
- **First non-empty batch → bootstrap** (rebuild-playlist semantics: build the playlist,
  set the cursor — `0`/first for `Cursor::First`, the target's index for `Cursor::At` — and
  display + decode the first image).
- **Subsequent batches → `extend_playlist(new_source, new_len)`:** swap `self.source`;
  `self.playlist.extend(new_len)` (new pb-core method — grow `len`, **preserve cursor**);
  reseat the shuffle deck (see Problem 3 / random state); `request_prefetch()` (new
  neighbors now decodable); update the count chip + title. It does **not** reset position,
  caches, ring, or undo.

### Problem 3 — Fixed-N features

The fixed-N wall is exactly three places (everything below them already copes):

- **`Playlist::len` + wrap/clamp math** (`playlist.rs`: `step` `pos % len` ~210-223,
  `next/prev`, `with_cursor` clamp ~60). → Add `Playlist::extend(new_len)`; wrap/clamp read
  the live `len`. Mechanical.
- **`prefetch_targets` `vec![false; len]`** (`prefetch.rs:17-23`). → Auto-sizes to the new
  `pl.len()` each call. Free once `Playlist::extend` lands.
- **`ShuffleOrder`** — the genuinely hard one. It materializes a full Fisher–Yates
  permutation of `0..N` up front (`shuffle.rs:23-31`), built inside `Playlist::new`
  (`playlist.rs:43`). → **Regenerate for the grown `len`, lazily on actual random use**
  (decision #2). The regen is cheap (sub-ms integer shuffle); the cost we must avoid is
  *decoding* an unstable random-ahead window — see **Random & prefetch thrash** below.
- **[R1] Random *state* on extend, not just the deck.** `Playlist` carries `random_started`,
  `shuffle_pos`, and the live `cursor` across random nav (`playlist.rs:170-206`).
  `Playlist::extend` must define their handling: **if random hasn't started**, just
  regenerate the deck (`shuffle_pos` stays 0); **if it has**, regenerate the deck *then reseat
  `shuffle_pos` to the current item's index in the new permutation*, so the next `random_next`
  advances to a fresh item and the **displayed photo never jumps on a grow**. Already-visited
  history (for `random_prev` retrace) is **not** preserved across a grow — the accepted
  decision-#2 relaxation during a live scan. Tests: extend **before vs. after** random has
  started; `random_next`/`random_prev` post-extend; `peek_random_*` bounds.

### Random & prefetch thrash (why we suppress random-ahead during a scan)

The prefetch window peeks *ahead in the shuffle deck* to pre-decode upcoming random photos
(`extend_random`, `prefetch.rs:135-150`) — that's what makes Enter a rebind, not a decode.
If we regenerate the deck every batch, that random-ahead set changes every ~150 ms, so the
resident ring would **decode a set, evict it unseen, decode a different set, repeat** — a lot
of decode load for photos the user never sees. (Sequential prefetch does *not* have this
problem: appending doesn't move your neighbors, so the sequential window is stable under
growth.)

**Fix (decision #2):** while a scan is in flight, **suppress the random-ahead prefetch
window** (a `deck_unstable` / `scanning` flag read by `prefetch_targets`); keep prefetching
sequential neighbors + current. A random keypress mid-scan **decodes that one target on
demand** (preview-first softens the latency — uncommon action, short scan window). At scan
completion, build the final deck once and resume random-ahead prefetch. The expensive lever
is the *fetch*, not the deck regen, so this kills the churn while keeping random usable.

**Also suppress sequential wrap during a scan:** if the user reaches the *last loaded* item,
`ahead` would wrap to index 0 and prefetch early items that stop being targets as the tail
grows (minor churn, and it'd skip the un-loaded tail). Clamp at the last loaded item while
scanning ("more coming"); enable wrap only at completion.

**Deferred elegant alternative:** an *extendable* shuffle (insert new indices at random
positions in the un-consumed tail) keeps the already-drawn + near-future prefix stable, which
would make random-ahead prefetch safe even mid-scan (no suppression). That's the delicate
deck surgery we deferred; only worth it if scans were long (they're not). Revisit if measured.

`ResidentRing` (`ring.rs`), `plan_residency` (`cache.rs:24-52`), and the decode pool
(`decode_pool.rs`) are already N-agnostic — **no changes**.

---

## Instant first image (the highest-value piece)

Two sub-cases, both flowing from `open_input` → `open::plan`:

- **Single file — `Cursor::At(target)` (we know the exact path).** Decode and show the
  target's pixels **immediately**, before the scan produces anything (the parent readdir
  can be slow for a 100k-file folder; the user shouldn't wait on it to see the photo they
  clicked). When the streamed playlist first lands, set the cursor to the target's real
  index. **Recommended v1:** show a provisional 1-item view (`rebuild_playlist` with
  `[target]`), and on handoff to the full list accept **one cheap re-decode** of the target
  (bytes are warm in the OS cache; it's a single decode while the user is already looking at
  the provisional). The no-re-decode rebind (match displayed pixels by path, keep them
  resident across the swap) is a later optimization if measured to matter. _This handoff is
  the most delicate detail in Phase 1; the provisional view is the only place index-keyed
  caches get reset (the target moves from index 0 → index K)._
- **Folder — `Cursor::First` (path unknown until the walk).** No provisional possible (we
  don't know the first path), but the **first batch lands fast** (root directory read), so
  the first image is near-instant anyway.

---

## Entry points — every way a playlist is (re)built [R1]

The first draft only covered runtime `open_input`. There are **four** entry points; all
must use the streaming path or they keep the old blocking scan:

1. **Runtime open** (`open_input`, `main.rs:1667`) — picker / drag-drop. Covered above.
2. **Startup launch** (cold double-click / file association / CLI) — **today resolves
   non-archive folders/files *synchronously before the window exists*** (`main.rs:5163-5168`:
   archives are deferred via `Resolved::empty()`, but `Source::Scan`/`Explicit` call
   `resolve_playlist` right then). So a double-clicked folder still blocks on the full
   scan+sort before the viewer even appears. **Fix:** defer `Source::Scan` exactly like
   `Source::Archive` — construct `App` with an empty source + a pending launch, and kick off
   the streaming scan (with provisional single-file for `Cursor::At`) from `resumed()` once
   the window exists. The window shows immediately; the first image streams in.
3. **`Ctrl+R` recursive toggle** (`toggle_recursive`, `main.rs:2077`) — **today a blocking
   `resolve_playlist` → `rebuild_playlist`** that re-finds the current photo (`keep`) in the
   rebuilt list. **Fix:** route through the streaming scan, preserving the current photo via
   `Cursor::At(current_path)` (the existing `keep` logic). **And the escape-hatch (owner
   insight):**
   - **Recursive → OFF *during a scan* = instant cancel + "just this folder."** Because the
     walk is depth-first, the **root directory's images are already the first streamed
     batch**, so turning recursive off can **cancel the in-flight recursive walk and drop to
     the flat root listing with no new wait** (reuse the already-streamed depth-0 entries, or
     a single fast readdir). This makes `Ctrl+R`-off a natural "stop, I only wanted the root"
     gesture. Preserve the current photo if it's in the root; else fall back to index 0 (the
     existing `keep`/fallback). _Real UX win — call it out in the changelog._
   - **Flat → ON** starts a recursive stream, preserving the current photo.
4. **Delete** (`flush_pending_delete`, `main.rs:1280`) — see Edge cases (it shifts indices and
   interacts with in-flight batches).

---

## Reconciling the progress dialog

The centered dialog is correct UI for a **blocking** operation. Streaming makes the scan a
**background** task, so the dialog's role shrinks to "we're still finding your *first*
photo":

- **Reveal condition changes** from "scan running > 250 ms" to "**first image of this open
  not yet displayed** AND > delay." Track an `awaiting_first_image` flag set when a streaming
  open begins and cleared when the first image displays (provisional decode, or first batch).
- When the first image displays → close the dialog (if open) → the **ambient count chip**
  takes over.
- **Archives unchanged** (`DialogKind::Loading` stays blocking). Most of the `ScanProgress`
  / deferred-reveal machinery we built last session transfers directly.

---

## The ambient count chip + the clickable-overlay precedent

### Where it renders (low-friction — clone the pie's pattern)

The top-right loading pie is a CPU-rasterized bitmap drawn as one quad: `hud::render_pie`
(`hud.rs:549`) → `App::push_pie` (`main.rs:3462/3477`) → `renderer.set_pie` (`gpu.rs:1336`)
→ `top_right_quad_vertices` (`gpu.rs:672`, anchored `margin` in from top+right). Overlay
membership is "which `Option<OverlayDraw>` fields are `Some`," each its own render pass
(`render`, `gpu.rs:1665`; fields at `gpu.rs:1018-1028`).

Add a sibling layer:
- Composite the chip with `Hud::render_panel_icon` (`hud.rs:153`) — count text, optional
  leading icon, rounded pill (the toast/icon compositor; white fill + black outline already).
- New `WgpuRenderer::set_chip` setter + `Option<OverlayDraw>` field + a top-right quad-vertex
  variant offset **below the pie** (a `y` shift on the `gpu.rs:672` math) + a `render()` pass
  + a `resize()` re-place branch (`gpu.rs:1422-1484`).
- **Count source — distinguish *browsable* from *found*. [R1]** The position denominator must
  be the **published/browsable** count, `self.source.len()` (the latest swapped snapshot;
  today only in `title_for`, `main.rs:4505`) — **not** `ScanProgress::found()`, which runs
  *ahead* of the browsable snapshot (the worker has matched more than it's published) and
  isn't navigable yet. (`ScanProgress::current()` is the folder *string*, not a count.) So:
  `"{idx+1} / {source.len()}"` for position, with an **optional separate scanning marker**
  (`found()` and/or a spinner glyph: "scanning… N found") while a scan is live. The
  borderless-fullscreen mode hides the title bar, which is *why* the on-image chip is needed.
- **Content / visibility (review detail):** _Open question: show the chip only during a scan,
  or keep a persistent position counter once an image is loaded, or make it a setting (the
  viewer is deliberately chrome-less)._

### The Cancel control + hit-testing (net-new, reusable)

**There is no click-hit-testing on the viewer today.** `MouseInput{Left}` only toggles
drag-to-pan (`main.rs:4174`); `CursorMoved` stores `last_cursor` and pans if dragging
(`main.rs:4162`); cursor state is `last_cursor: Option<[f32;2]>` (`main.rs:492`) +
`dragging` (`main.rs:495`). The macOS proxy-icon is AppKit-native, no app hit-testing.

Build a small, reusable **overlay hit-region registry** — this is the precedent the owner
asked for:
- A list on `App` of `OverlayHit { rect_px, action }`, (re)populated whenever a clickable
  overlay is pushed/resized (the quad-vertex functions already compute the pixel rect
  `x0,y0,panel_w,panel_h` — mirror it app-side).
- In `MouseInput{Left, Pressed}`, **before** setting `dragging`: test `last_cursor` against
  the registry; on hit, dispatch the action and **swallow** the event (don't start a pan).
- Optional polish: hover state in `CursorMoved` → pointer cursor + highlight (extends
  `refresh_cursor`, `main.rs:3674`).
- **Cancel action already exists:** `App::cancel_dir_scan` (`main.rs:2004`) →
  `ScanProgress::request_cancel` (`main.rs:5045`).

**Icon:** vendor an FA **solid** `stop` (or `xmark`) glyph — none is vendored yet (existing
assets: `CLIPBOARD`, `TRASH`, `ROTATE_*`, `FLOPPY`, `RECYCLE`, `UNDO`, `icon.rs:17-34`).
Copy the SVG from the Mac FA library (`/Users/jdlien/Documents/FontAwesome`, family `solid`)
into `crates/pb-app/icons/stop.svg`, add `pub const STOP` to `icon::assets`. Render via the
existing `icon::rasterize(svg, height, rgb)` (`icon.rs:39`).

**Sets the precedent for EXIF copy buttons:** the same `OverlayHit` registry serves the info
panel. `CLIPBOARD` (copy icon) is **already vendored**. The one extra piece copy buttons need:
`Hud::render_table` (`hud.rs:202`) currently keeps its per-row layout metrics (`row_top =
pad + i*line_h`, `value_x`) internal — it must **also return per-row rects** so each row's
copy hit-target is registerable. (Out of scope for this plan; noted so the precedent is built
to accommodate it — multi-target overlays return sub-element rects.)

---

## Cancellation semantics (changed, for the better)

With streaming, **cancelling a folder scan keeps whatever streamed so far** (it's already a
valid, browsable partial playlist) — rather than discarding to the prior view. More useful,
and it falls out naturally. (Distinct from archive cancel, which discards.) _Owner to confirm
at review._

**[R1] There must be a real cancel route once the dialog is gone.** Today folder cancel only
exists inside the scanning-dialog button → `cancel_dir_scan`; **Esc is `Action::Quit`**
(`keymap.rs:371` → exit in `main.rs:2594`), so once the dialog auto-closes after the first
image, Esc would *quit the app*, not cancel the scan. Routes to provide:
1. **A new `Action::CancelScan` + a menu item** ("Stop Scanning", enabled only while a scan is
   live) — the always-available route, needs no hit-testing. **Add in Phase 2.**
2. **`Ctrl+R`-off escape hatch** (Entry points #3) — cancels the recursive walk and drops to
   the flat root. Free with the toggle.
3. **The on-image chip Cancel button** — Phase 3 (needs the hit-test registry).
4. The **scanning dialog's** Cancel — still there for the (now rare) pre-first-image window.

_Do **not** repurpose Esc-while-scanning to cancel — Esc=Quit is muscle memory and a silent
override would surprise. Use the explicit `CancelScan` action/menu instead._

---

## Edge cases & risks

- **Empty / "no supported images"** is only known at scan completion (Done, zero total) —
  show that message then, not early. A provisional single-file view stands until then.
- **Supersession** (open B while A streams): bump `scan_gen`, cancel A's walk, re-point the
  chip/dialog to B (mirrors today's generation logic).
- **`Ctrl+R` rescan** re-streams; preserve the current photo by identity if still present.
  Recursive-**off** mid-scan is the instant escape hatch (Entry points #3).
- **Delete during scan — NOT safe as a no-op. [R1]** `flush_pending_delete` (`main.rs:1280`)
  rebuilds the source from the *current* snapshot's remaining paths and **shifts indices**; a
  later batch built from the worker's *cumulative* list would **reintroduce the deleted path**
  (the worker never saw the delete) and desync index-keyed state. Pick a strategy:
  - **(Recommended) Path tombstones.** App keeps `deleted: HashSet<PathBuf>`; the browsable
    list is always `cumulative_paths − deleted`, and **every** snapshot the worker publishes is
    filtered through it before the swap. A delete adds a tombstone + does the normal
    rebuild-preserving-position. Scan keeps streaming; deletes stick. Cost: a per-snapshot
    filter + a full (not append-only) rebuild at the delete (rare; fine). This unifies
    extend/bootstrap/delete as "rebuild the filtered snapshot, preserve current."
  - **(Simple fallback) Delete cancels the scan** — finalize the playlist at the current
    snapshot minus the deleted item. Dead simple/safe, but a single delete stops discovery of
    a big tree (annoying for curation). Acceptable for v1 if tombstones add too much risk.
  - **(Rejected) Restart the scan** — wasteful, re-shows the scanning state.
  _Owner decision; I lean tombstones._
- **Provisional handoff** is the one place index-keyed caches reset (target index 0 → K).
  Accept it (one image).
- **O(N²) trap**: must time-bound batching (~150 ms), not per-count. Called out in Problem 2.
- **No-trace privacy holds**: streaming is still read-only; the `viewing_a_folder_writes_
  nothing_to_disk` test must stay green (the chip/count are RAM-only).

---

## Test plan (TDD; pb-core is pure, no excuse)

- `Playlist::extend(new_len)`: cursor preserved, `len` grows, `next/prev/step` cover the new
  range; property — extend then traverse visits the new items, wrap uses the new `len`.
- **[R1] Random state on extend:** extend **before** random starts (deck regenerated, pos 0)
  vs. **after** (pos reseated to current, displayed photo doesn't move); `random_next`/
  `random_prev`/`peek_random_*` correct + in-bounds post-extend. `ShuffleOrder` regenerate is
  a valid permutation of `0..new_len`.
- **[R2] Streamed walk order = `paths.sort()` exactly:** build a tree with the edge cases
  (`a_subdir/x.jpg` vs `z.jpg`; `a.jpg` vs `a/b.jpg` prefix edge; `img2` vs `img10`) and
  assert `sort_by_file_name` walk order == the same paths `paths.sort()`-ed. Append-only:
  no already-emitted index is ever reordered.
- **[R1] Snapshot construction microbench** (Criterion, off-thread builder): 10k / 100k / 1M
  paths; plus `prefetch_targets`' `vec![false; len]` per-call alloc.
- `prefetch_targets` with a grown `len`: window clamps, no OOB, `seen` bitmap sized to new len.
- **Anti-thrash invariant (the random concern, made measurable):**
  - pb-core unit test: with the `scanning`/`deck_unstable` flag set, `prefetch_targets` emits
    **only sequential targets — no random-ahead entries** and **no wrap** past the last loaded.
  - Decode-count bound: scripted run (open big folder; press Enter at t≈200 ms, t≈400 ms)
    asserts total decodes ≈ `sequential window + on-demand presses`, **not**
    `deck-size × num-batches` — using the pool's existing `POOL_DECODE_MS` instrumentation.
  - Manual: per-frame NDJSON / Tracy decode log over that workload stays flat (no churn spike).
- Hit-test: point-in-rect dispatch unit test (the `OverlayHit` registry).
- Integration: `viewing_a_folder_writes_nothing_to_disk` still passes (streaming + chip).

---

## Phasing (risk-ordered, each independently shippable)

- **Phase 1 — Instant first image + dialog gating + entry points. [R1]** Provisional decode of
  `Cursor::At` targets; show immediately; hand off to the playlist when the scan lands (accept
  one re-decode; first batch = **bootstrap**, not extend). Make the "Scanning Folder" dialog
  first-image-gated. **Cover all entry points** — defer the startup launch like archives, and
  route `Ctrl+R` through the async path. _Biggest perceived win, smallest blast radius; needs
  no growable source yet (the scan can still complete fully before the rest is navigable —
  fine for flat/single-file, the Phase-1 target)._
- **Phase 2 — Streaming growable playlist. [R1]** Custom-walker per-dir batched emit
  (off-thread snapshot build); `extend_playlist`; `Playlist::extend` + shuffle reseat; ambient
  **count chip** (browsable count); **`Action::CancelScan` menu item** + the `Ctrl+R`-off
  escape hatch (Esc stays `Quit`); cancel-keeps-partial; **delete tombstones**. This makes
  `→ → →` work mid-scan.
- **Phase 3 — Clickable overlay infra + on-image Cancel button.** The `OverlayHit` registry +
  click routing + the FA stop chip-button, establishing the precedent the EXIF copy buttons
  will reuse.

---

## File-by-file change map (for execution)

| Area | File | Change |
|---|---|---|
| Plan/cursor | `crates/pb-core/src/open.rs` | (likely none) cursor stays `At/First`; reindex happens app-side on handoff |
| Playlist | `crates/pb-core/src/playlist.rs` | **add `extend(new_len)`** (grow len, preserve `pos`) |
| Shuffle | `crates/pb-core/src/shuffle.rs` | regenerate permutation on grow (called from `Playlist::extend`) |
| Prefetch | `crates/pb-core/src/prefetch.rs` | auto-sizes to new `len`; **add a `scanning`/`deck_unstable` flag that suppresses the random-ahead window + sequential wrap while a scan is live** |
| Streamed walk order | `crates/pb-app/src/main.rs` (`collect_images` 4534, `resolve_source` 4769) | **[R2]** `WalkDir::sort_by_file_name()` (== `paths.sort()`, verified); iterate incrementally for batches; drop the external `paths.sort()` for single-root |
| Scan worker | `crates/pb-app/src/main.rs` (`begin_dir_scan`/`poll_dir_scan` 1874-1939) | stream batches; **build each `FsSource` snapshot off-thread** and send a ready `Arc`; channel carries `Batch(Arc)`/`Done`, not one `Resolved` |
| Playlist swap | `crates/pb-app/src/main.rs` (`rebuild_playlist` 2654) | **add `extend_playlist`** (swap source, `playlist.extend`, no cache/ring reset); **[R1]** first non-empty batch uses bootstrap (rebuild) semantics |
| Instant first | `crates/pb-app/src/main.rs` (`open_input` 1667) | provisional decode for `Cursor::At`; handoff/reindex |
| **[R1] Startup launch** | `crates/pb-app/src/main.rs` (5163-5168, `resumed`/`pending_launch`) | defer `Source::Scan` like `Source::Archive` — empty source at boot, stream from `resumed()` |
| **[R1] Ctrl+R** | `crates/pb-app/src/main.rs` (`toggle_recursive` 2077) | route through streaming (`Cursor::At(current)`); **recursive-off mid-scan = cancel + flat root** (reuse depth-0 batch) |
| **[R1] Delete tombstones** | `crates/pb-app/src/main.rs` (`flush_pending_delete` 1280) | `deleted: HashSet<PathBuf>`; filter every published snapshot; rebuild-preserving-position |
| Dialog gating | `crates/pb-app/src/main.rs` + `dialog.rs` | reveal on `awaiting_first_image`, not raw elapsed |
| Count chip | `hud.rs` (`render_panel_icon` 153), `gpu.rs` (new `set_chip`+field+quad+pass+resize), `main.rs` (`push`/state) | new top-right chip layer below the pie; **[R1]** denominator = `source.len()` (browsable), `found()` only as a scanning marker |
| **[R1] Cancel action** | `crates/pb-app/src/action.rs`, `keymap.rs`, `menu.rs` | add `Action::CancelScan` + a "Stop Scanning" menu item (Esc stays `Quit`) |
| Hit-testing | `crates/pb-app/src/main.rs` (`MouseInput` 4174, `CursorMoved` 4162) | `OverlayHit { rect_px, action }` registry; test-before-pan; swallow |
| Cancel icon | `crates/pb-app/icons/stop.svg` (new), `icon.rs` (`assets::STOP`) | vendor FA solid stop |
| Random | `crates/pb-core/src/playlist.rs`, `shuffle.rs` | **[R1]** `Playlist::extend` regenerates deck + reseats `shuffle_pos` to current; suppress random-ahead prefetch during scan; no toast |

---

## Open questions for owner review

1. ~~Recursive order~~ **[R2] RESOLVED** — `walkdir.sort_by_file_name()` == today's
   `paths.sort()` (component-wise; empirically verified), so there's **no behavior change**
   and no custom comparator. (Natural sort / files-first remain separate deferred opt-ins —
   want either as a follow-up?)
2. **Delete during scan [R1]**: tombstones (recommended — scan keeps streaming, deletes stick)
   vs. delete-cancels-scan (simple, but stops discovery on one delete)?
3. **Count chip visibility**: scan-only, or a persistent position counter once an image is
   loaded, or a setting? (Chrome-less aesthetic vs. always-visible position.)
4. **Cancel-keeps-partial**: confirm cancelling a folder scan keeps what streamed so far (vs.
   discarding to the prior view).
5. **Cancel button form**: icon-only stop, or stop + "Cancel" label? Placement: below the pie,
   or beside the count?
6. **Provisional handoff**: accept one cheap re-decode of the target at handoff (simple), or
   build the no-re-decode rebind now (complex)?
7. **Phasing**: do all three phases in the order above? Any to defer?
