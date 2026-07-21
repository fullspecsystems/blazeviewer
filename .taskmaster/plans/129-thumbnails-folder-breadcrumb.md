# #129 — Folder breadcrumb / path bar atop the Thumbnails panel (macOS)

_Started 2026-07-20 (macOS session). Owner idea: the left pane hosts Folders and Thumbnails
as two tabs of one panel, but only one shows at a time. When you're on the Thumbnails tab you
lose all sense of **where** the current image lives, and going up a level means switching to the
Folders tab, finding the parent, and clicking. Put a thin, interactive folder path bar at the top
of the Thumbnails tab — below the tab-bar header — that both shows the current folder and lets you
jump up the tree in one click._

## The idea, sharpened

A horizontal, Finder-path-bar-style strip that:

1. **Shows where you are.** Renders the ancestry of the *current image's* folder — and because it
   tracks the current photo, it **updates live as you blaze**, so it's real "where am I" context,
   not a static label.
2. **Lets you go up.** Clicking any ancestor crumb opens that folder as the new deck (the same
   thing clicking that row in the Folders tree does). Down is what the thumbnails/tree are for;
   the path bar's value-add is *up*.
3. **Fits a narrow pane.** Long paths truncate from the **beginning** — show as many trailing
   ancestors + the current folder as fit; collapse the rest into a leading overflow control
   (a `‹` chevron opening a menu of the remaining ancestors up to root). This is exactly the
   Finder path-bar behaviour and it handles arbitrarily deep paths in the 120pt-floor pane.

## Why it fits the directives (and where it's honest about cost)

- **Prime Directive.** From the Thumbnails tab, "go up a level" is a plausible next action, and
  today it's tab-switch → hunt → click. The path bar collapses it to one click without leaving
  the thumbnails — the next likely action gets closer to instant.
- **Mac-assed.** A path bar is a native macOS idiom (Finder's bottom path bar / `NSPathControl`),
  not a web pattern. See *Open decision 1* for the native-control-vs-custom tension.
- **Shared Action vocabulary (CLAUDE.md).** The click routes through the **same core entry point**
  the Folders tree uses (`fs_tree_open` → `open_dir`), so the keymap/menu/tree/breadcrumb can't
  drift into three different "open folder" behaviours.
- **Off the hot path / mostly RAM-only.** The bar itself is passive chrome rebuilt only when the
  current folder changes and reflects `current_folder_abs()` (already in RAM). ⚠️ **The bar
  persists nothing, but *clicking* it invokes an explicit folder open, and a completed
  folder-backed rebuild updates `settings.last_folder` and may save `settings.toml`
  (`app_core_impl/nav.rs:114`).** That is the existing owner-approved explicit-action exception
  (ADR-022) — not a new leak — but state it accurately (codex #7). FFI tests must keep preference
  persistence disabled, as the existing helper does (`lib.rs:4931`).

## What the code already gives us (grounding — verified by reading the tree)

- **Data source exists, but is gated on the Folders tab (must fix — codex #1).**
  `AppCore::current_folder_abs()` (`crates/pb-app-core/src/app_core_impl/tree.rs:313`) → the
  current photo's containing folder, `None` on an archive/empty deck. Surfaced to Swift as
  `tree_current_path()` (`crates/pb-mac-ffi/src/lib.rs:823`) and observed as
  `CoreModel.currentTreePath` — **but `refreshTree()` early-returns when the Folders tab is hidden
  (`CoreModel.swift:898`), *before* it reads `treeUsesFs`/`currentTreePath`.** `tree_visible()` is
  specifically the *Folders* tab (`tree.rs:287`); Thumbnails is a separate tab (`thumbs.rs:16`). So
  on the Thumbnails tab these values are stale — the breadcrumb would never appear (or would show a
  stale disk path after switching to an archive). **The plan's original "no new data FFI, updates
  for free" claim was wrong.** Fix: a breadcrumb-context refresh that pulls `tree_uses_fs()` +
  `tree_current_path()` **independent of Folders visibility**, clearing both on archive/empty
  decks. See Step 1.
  - **Second propagation gap (codex #1b):** `advance()` emits `PanelsChanged` *before* an async
    cache miss updates `displayed_item` in `mark_resolved()` (`app_core_impl.rs:3594`), which does
    **not** re-emit a panel change. Thumbnail derivation may incidentally signal, but it's not
    guaranteed. Emit an explicit current-folder/breadcrumb signal when `displayed_item` actually
    changes (gated on Thumbnails visibility), or keep a cheap current-folder snapshot in the core.
    Test the async-miss case, not just resident nav.
- **The click target — correct *action*, but the above-root re-root invariant is broken
  (must fix — codex #2).** `AppCore::fs_tree_open(path)` (`tree.rs:467`) re-roots the deck to a
  folder via `open_dir` (archive rows become doors; a plain ancestor always takes `open_dir`).
  **But `ensure_fs_tree` rebuilds the resident tree only when the current *image folder* leaves the
  old tree root (`tree.rs:347`: `current.strip_prefix(t.root()).is_err()`) — it never checks
  whether the newly opened *deck root* moved above the tree root.** Failure case: deck `/A/B/C`,
  tree root `/A/B`; breadcrumb opens `/A`; the recursive scan's first image is still under
  `/A/B/C`, so `current` stays inside `/A/B` and the tree is *not* rebuilt — switching back to
  Folders then shows a tree that can't browse the new deck's `/A/X`. `FsTree::set_current` refuses
  to reveal an out-of-root folder (`fs_tree.rs:196`), so this matters. **Fix in the core:** extend
  the rebuild condition to also fire when the deck root moves outside the resident tree root
  (check both `current` and `self.root` against `t.root()`), with a regression test. Until then,
  delegating the FFI straight to `fs_tree_open` is *not* sufficient. (`fs_tree_extend_up` is a
  different affordance — widens the browsable tree without changing the deck — not the fix here.)
- **The gate:** `tree_is_fs()` (`tree.rs:321`) is true only for a disk deck with a current folder.
  When false, `tree_current_path()` is empty → the breadcrumb is naturally absent for archive/empty
  decks. v1 shows the bar **only** when `treeUsesFs` (`CoreModel.treeUsesFs`, `:181`) is true.
- **The one gap:** `fs_tree_open` is not directly callable over FFI — today it's only reached via
  `tree_activate(i)` on a row-index snapshot (`lib.rs:832`). We add one thin FFI method that opens
  a folder by **path** (below).

## Open decisions (flag for review)

1. **RESOLVED (codex #9) — custom SwiftUI breadcrumb, not `NSPathControl`.** For this translucent,
   tokenized, custom-hover panel a SwiftUI control is the better fit: it avoids fighting
   `NSPathControl`'s opaque background / focus ring / drag behavior / AppKit bridging, and — a point
   the grounding surfaced — sidesteps possible **filesystem metadata/icon lookups on network paths**
   the native control does. A restyled `NSPathControl` subclass isn't worth the maintenance cost.
   Build it as a custom SwiftUI `Layout` (or a measured right-to-left row) that reserves the current
   crumb + the overflow control's width *before* deciding which ancestors fit, uses a `Menu` for the
   omitted ancestors, sets explicit accessibility labels + button traits, and uses **text-only
   component names** (no `NSWorkspace`/display-name/icon lookups on the main actor).

2. **Also show it on the Folders tab?** No, for v1 — the Folders tree already shows ancestry via
   indentation and has an explicit "up to parent" row (`fs_tree_extend_up`). The path bar is
   redundant there. Thumbnails-only keeps scope tight and the tab distinction crisp.

3. **The *current* folder crumb (corrected — codex #4).** `tree_current_path()` is the current
   *image's folder*, **not the deck root** — the core does not expose the deck root. So clicking the
   trailing crumb is **not a no-op**: in a recursive deck rooted at `/Photos` while viewing
   `/Photos/2026/Trip/a.jpg`, opening the `Trip` crumb would *narrow and rebuild* the deck at `Trip`,
   and re-opening `/Photos` rescans it. Decision: render the trailing (current-folder) crumb
   **disabled/non-interactive**, and describe that as *deliberately preventing an accidental
   narrow/reopen*, not as "a no-op." If the UI ever needs the true deck root, expose it separately
   over FFI (out of scope v1).

4. **Very short paths.** When the current folder has little ancestry (a top-level volume, or one
   crumb), still render the single current crumb — context is useful even with nowhere to go up to.
   Hide the bar only when `treeUsesFs` is false. (Open: whether a lone single crumb earns its
   vertical space, or the bar should appear only with ≥2 levels — pick during owner-smoke.)

## Design

**Layout** (Thumbnails tab, top-down): tab-bar header → `PanelDivider` → **breadcrumb strip** →
`PanelDivider` → thumbnails `ScrollView`. The strip is one control-height row (~`PanelMetrics`
header-ish, tighter), horizontally scroll-free (it truncates, never scrolls), with the same
side inset as the strip (`stripPad`).

**Crumb model (derived in Swift, no new data FFI):** from `model.currentTreePath` (an absolute
path string), build `[(name, absPath)]` via `URL(fileURLWithPath:).pathComponents` (or split on
`/`), mapping each prefix to its abs path and each component to a display name. Recompute only when
`currentTreePath` changes (it already fires exactly on folder change).

**Fit / overflow:** measure available width; lay crumbs right-to-left (current folder pinned
visible). Ancestors that don't fit collapse into a single leading `‹` control whose `Menu` lists
the dropped ancestors (nearest-first, up to root). Each visible ancestor is a `Button` →
`model.openFolderPath(absPath)`. Separators are `chevron.compact.right` in `panelSecondary`.

**Interaction routing (shared, no drift):** one new FFI + one `CoreModel` method. The `&str` shape
is a routine supported bridge shape (no `Vec<enum>` / Swift-keyword friction; `path` is a fine
identifier — codex #5). It must be added in **two** places: the `impl` **and** the `extern "Rust"`
bridge declaration beside the existing tree methods (`lib.rs:4702`), and the bridge-module comment
must use `//`, not `///` (`crates/pb-mac-ffi/README.md:53`). Keep the `tree_is_fs()` guard — it
defensively rejects stale actions on archive/empty decks and matches the bar's visibility contract
(it checks native-tree mode + a disk-backed current folder; it does *not* require a built `FsTree`).

```rust
// crates/pb-mac-ffi/src/lib.rs — impl beside tree_activate
/// Open a folder by absolute path (the breadcrumb path bar, task #129): the SAME
/// folder-open the Folders tree performs, so behaviour can't drift. Ignored on a
/// non-fs deck (archive/empty) — the bar isn't shown there anyway. ASYNC: this only
/// QUEUES a recursive dir scan; the deck/root do not change until scan batches return.
fn open_tree_folder(&mut self, path: &str) {
    self.core.now = Instant::now();
    if self.core.tree_is_fs() {
        self.core.fs_tree_open(std::path::PathBuf::from(path));
    }
}
```

```swift
// mac/Sources/BlazeViewerMac/CoreModel.swift — beside activateTreeRow
func openFolderPath(_ path: String) { core.open_tree_folder(path); pump() }
```

No change to `AppCore` state, no new struct fields → **no struct-literal cross-platform trap.**

**⚠️ Broad-ancestor cost (codex #3).** `fs_tree_open` → `open_dir` → `LaunchInput::Directory`, whose
policy hard-codes `recursive: true` (`pb-core/open.rs:72`); the streaming worker then walks every
descendant (`scan.rs:576`). It's off-thread and cancellable, but clicking `/`, `/Users`, `/Volumes`,
a share root, or a large SMB ancestor can spawn a very long scan, reset the deck, and contend with
decode I/O — materially more than "one-click go up" implies. v1 must **not** make broad system
ancestors ordinary one-click targets: either disable crumbs at/above a volume/share boundary (e.g.
stop the interactive ancestry at the deck's containing volume), or require confirmation for them.
Measure local-deep-tree and SMB behaviour before calling this a speed win.

**Path fidelity (codex #8).** Build crumb prefixes with `URL`/path-component APIs but do **not**
lowercase, canonicalize, `resolveSymlinksInPath`, or renormalize Unicode — the core's tree
containment is lexical `strip_prefix`/`starts_with` and case-insensitive volumes don't make those
comparisons case-insensitive, so a "cleaned" path can disagree with the resident tree. Note the
limitation that `tree_current_path()` uses `to_string_lossy()` (`lib.rs:823`): a filename with
invalid UTF-8 can't round-trip through this string action (rare on macOS, but real).

## Steps (two-halves discipline — `docs/where-code-goes.md`)

1. **Core fix — above-root rebuild invariant (subtask 1, codex #2).** In `ensure_fs_tree`
   (`app_core_impl/tree.rs:341`) extend the rebuild condition so the resident tree also rebuilds
   when the **deck root** moves outside the tree root (not only the current image folder). Pure-ish
   core logic → the concern already lives in `tree.rs`/`fs_tree.rs`. **Regression test (this Mac):**
   deck root moves upward while the first/current image stays inside the old tree root; assert the
   tree is re-rooted and can browse the new deck's siblings. **Fully testable + verifiable here.**
2. **Core signal — breadcrumb context independent of Folders visibility (subtask 2, codex #1/#1b).**
   Ensure the current-folder value the breadcrumb reads is available and fresh on the Thumbnails
   tab: either emit a panel/breadcrumb signal when `displayed_item` changes (incl. the async
   `mark_resolved` path, `app_core_impl.rs:3594`) gated on Thumbnails visibility, or keep a cheap
   current-folder snapshot the shell can pull unconditionally. Tests: (a) folder path refreshes
   while Thumbnails is visible and Folders hidden; (b) the async cache-miss case updates it;
   (c) disk→archive transition clears it.
3. **FFI seam (subtask 3).** Add `open_tree_folder(&str)` in **both** the `impl` and the
   `extern "Rust"` bridge block (`lib.rs:4702`; `//` not `///`), delegating to `fs_tree_open`.
   **Rust test (async-aware — codex #6):** assert the call *queues* a recursive dir scan of the
   ancestor (`Source::Scan { roots:[ancestor], recursive:true }`) rather than asserting a
   synchronous root change; a no-op on an archive deck; and (for an end-to-end root assertion) use a
   temp hierarchy and pump the real scan to completion. Keep preference persistence disabled per the
   existing helper (`lib.rs:4931`). **Verification requires the generated bridge + `swift build`**,
   not just `cargo test` (`README.md:60`).
4. **Swift breadcrumb view (subtask 4).** New `FolderBreadcrumb.swift` (its own file — the panel is
   already large): a custom `Layout`/measured right-to-left row (current + overflow width reserved
   first), the overflow `Menu`, panel-token styling, accessibility labels/traits, text-only crumb
   names. `CoreModel.openFolderPath`. Extract the crumb-model derivation (path → crumbs; `/`, volume
   roots, trailing slash, empty→none; **lexical, no canonicalize/lowercase/symlink-resolve**) as a
   pure helper. ⚠️ **`mac/Package.swift` declares no test target (`:28`)** — add a
   `BlazeViewerMacTests` target (or place the pure helper in an already-testable module) so the
   derivation is actually runnable, else the "unit-testable" claim is empty.
5. **Mount in the Thumbnails panel (subtask 5).** In `ThumbnailsPanelView`
   (`ThumbnailsPanel.swift:125`), insert the strip + a `PanelDivider` between the
   `LeftPaneTabBar`/divider and the `ScrollViewReader`, shown only when `model.treeUsesFs`.
   Recompute `visibleRows`/height math for the strip's added chrome height. Apply the broad-ancestor
   boundary from *Design* (don't offer `/`, `/Volumes` as one-click targets). Owner-smoke per the
   matrix below.
6. **Docs + changelog (subtask 6).** `CHANGELOG.md` under `Added` (user-facing). Update task #83
   notes / this plan's Handoff.

## Non-goals / cross-platform

- **winit/egui parity is out of scope for v1** and is **cross-platform debt**, not a regression:
  the macOS thumbnails panel is native SwiftUI; the winit Thumbnails strip is a separate egui
  surface (task #83, egui parity still pending). The *behaviour* is already shareable — the data
  (`current_folder_abs` / `tree_current_path`) and the action (`fs_tree_open`) live in the core —
  so a future winit breadcrumb reuses the same seams; only the presentation is owed. Note it in the
  Handoff so the Windows session can pick it up.
- No drag-and-drop, no editable path, no "reveal in Finder" from the bar in v1 (all things a real
  `NSPathControl` could add later).

## Tests

- **Rust (this Mac):** (a) `ensure_fs_tree` above-root rebuild regression (Step 1); (b)
  breadcrumb-context refresh — Thumbnails-visible/Folders-hidden, async cache-miss, disk→archive
  clear (Step 2); (c) `open_tree_folder` *queues* a recursive scan of the ancestor and no-ops on an
  archive deck (async-aware, not a synchronous root assertion — Step 3). Full `pb-mac-ffi` +
  `pb-app-core` suites green; clippy clean incl. `pb-mac-ffi`. **Also run `swift build`** — the
  generated bridge can fail where `cargo test` can't see it.
- **Swift:** pure crumb-derivation helper unit test (`/`, nested, volume root, trailing slash,
  empty → no crumbs; lexical fidelity) — **requires the new `BlazeViewerMacTests` target**.
- **Owner-smoke (behaviour-unverified until run) — matrix:** live-tracking while blazing across
  subfolders; ancestor click re-roots (and the tree is browsable after, per Step 1); **120pt** width
  overflow menu; **`/`**, **`/Volumes/<share>`**, a **deep path**, an **archive transition**, a
  **slow SMB ancestor**; light **and** dark (panel shadow/tint — `[[mac-panel-shadow-tokens]]`).

## Handoff

**Implemented end-to-end on macOS (2026-07-20 session), all six steps:**
- **Step 1 — above-root rebuild fix** (`ensure_fs_tree`, `app_core_impl/tree.rs`): rebuild also
  fires when the *deck root* leaves the tree root. Regression test
  `opening_an_ancestor_above_the_tree_root_rebuilds_the_tree`. ✅
- **Step 2 — breadcrumb-context signal** (`app_core.rs` field `last_breadcrumb_snap` + a per-tick
  snapshot block in `app_core_impl.rs`): the current folder re-signals `PanelsChanged` on a folder
  change even with only Thumbnails open, incl. the async `mark_resolved` path. Test
  `the_breadcrumb_re_signals_on_a_folder_change_with_only_thumbnails_open`. ✅
- **Step 3 — FFI** `open_tree_folder(&str)` (impl + `extern "Rust"` decl, `//` comments): delegates
  to `fs_tree_open`, guarded on `tree_is_fs()`. Async-aware test
  `open_tree_folder_queues_a_recursive_scan_and_no_ops_off_fs`. ✅
- **Step 4 — Swift**: `FolderBreadcrumbView` (`mac/…/FolderBreadcrumb.swift`) — custom path bar,
  AppKit text-measured greedy right-to-left fit, overflow `Menu`, disabled current crumb, boundary
  guard. The **pure model + boundary rule live in a new `mac/PbBreadcrumb` package** (the PbSeek
  pattern) with 6 passing unit tests (`swift test`, zero native deps). `CoreModel.openFolderPath`
  + `breadcrumbPath` pulled in `refreshThumbs`. ✅
- **Step 5 — mount**: shown in `ThumbnailsPanelView` only on an fs deck (`!breadcrumbPath.isEmpty`),
  `chromeHeight` folds in the strip so the grid math stays exact. ✅
- **Step 6 — changelog** (`### Added`) + this Handoff. ✅

**Verified (this Mac):**
- `pb-app-core` + `pb-mac-ffi` Rust tests green (the three new tests above; full suites TBD in the
  commit run). `PbBreadcrumb` `swift test` = 6/6.
- Full `swift build` (debug, `--no-ffvideo`): _pending at write time — the generated bridge must
  accept `open_tree_folder` and all Swift must compile._

**Not verified — needs owner-smoke (behaviour-unverified):** the matrix in *Tests* — live tracking
while blazing across subfolders; ancestor click re-roots + tree browsable after (Step 1); 120pt
overflow menu; `/`, `/Volumes/<share>`, deep path, archive transition, slow SMB ancestor; light +
dark. Tune the boundary rule (`FolderBreadcrumbModel.isInteractive`) and the single-crumb question
(Open decision 4) on smoke.

**Cross-platform debt:** winit/egui Thumbnails breadcrumb parity is owed (seams shared —
`current_folder_abs`/`fs_tree_open` are in the core; the per-tick signal is gated on `native_tree`,
off for winit — presentation per-shell). The `last_breadcrumb_snap` `AppCore` field was added to all
three literals incl. the winit `pb-app/src/main.rs` (blind from the Mac — the struct-literal trap);
a Windows `cargo clippy -p pb-app` should confirm it compiles (low risk, mechanical `field: None`).

**Claimed:** macOS session holds #129 (implemented 2026-07-20). Coexists with in-flight #127
(burning-Polaroid) WIP in the same tree — #129 committed as isolated hunks, #127 left untouched.

---

## Codex review (2026-07-20, codex-cli 0.144.6, read-only)

Ran a read-only codex review against the real code; findings verified and **folded into the body
above**. Two Criticals were spot-checked by hand and confirmed. Summary of record:

**Critical**
1. **`currentTreePath` doesn't update on the Thumbnails tab** — `refreshTree()` early-returns when
   Folders is hidden (`CoreModel.swift:898`) before reading `treeUsesFs`/`tree_current_path()`.
   *Verified.* → grounding bullet #1 + Step 2. Includes the async-miss propagation gap (#1b).
2. **`fs_tree_open` does not re-root above the tree root when the deck root moves but the current
   image stays under the old root** — `ensure_fs_tree` checks only the current folder
   (`tree.rs:347`). *Verified.* → grounding bullet #2 + Step 1 (core fix + regression test).

**Should-fix**
3. Every crumb click starts an **always-recursive** scan (`pb-core/open.rs:72`) — guard broad
   ancestors (`/`, `/Volumes`, SMB). *Verified.* → *Design* broad-ancestor note + Step 5.
4. Plan conflated current-folder / deck-root / tree-root; the trailing crumb is **not** a no-op
   (`tree_current_path` = image folder, not deck root). → Open decision 3 rewritten.
5. FFI shape is safe, but: add to **both** `impl` + `extern "Rust"` block, `//` not `///`, and
   verification needs `swift build`, not just `cargo test`. Keep the `tree_is_fs()` guard. → Design
   routing + Step 3.
6. Opening is **async** (queues `BeginDirScan`) — the proposed synchronous test assertion is wrong;
   assert the queued scan or pump to completion. `mac/Package.swift` has **no test target** — the
   Swift "unit-testable" claim needs one added. → Steps 3–4.
7. "Persists nothing" inaccurate — a click's completed open updates `settings.last_folder`
   (ADR-022 explicit-action exception); keep persistence off in FFI tests. → *Why it fits* bullet.

**Nice-to-have**
8. Preserve **lexical** paths (no canonicalize/lowercase/symlink-resolve); note `to_string_lossy`
   UTF-8 caveat. → Design *Path fidelity*.
9. **Use a custom SwiftUI breadcrumb, not `NSPathControl`** (avoids AppKit restyling fights +
   network-path icon/metadata work). → Open decision 1 RESOLVED, with the `Layout` spec.
