# Flicker Compare Mode — pin (⇧Y) + instant A/B toggle (Y)

_Owner-approved design 2026-07-03 (task #43, re-scoped). Supersedes the original
split-view/multi-window sketches — flicker comparison at full resolution beats
side-by-side for the culling use case (change detection at a fixed gaze point;
side-by-side halves both images and forces eye travel). The split view is demoted
to an optional phase 2 that reuses this pin state if it's ever wanted._

## The feature in one paragraph

`Y` pins the current photo ("Pinned for compare" toast, thumbtack icon); after
that, `Y` flips instantly between the pinned photo and wherever you are —
full-screen, full-resolution, zoom/pan carried across when dimensions match, so
a 100% crop of an eye lands under your gaze on both frames. `⇧Y` moves the pin
to the current photo (or unpins if it's already pinned). The pin is a bookmark,
not a mode: navigation always works normally everywhere.

**Prime-directive fit:** the toggle is the engine's founding principle applied
to a bookmark — *a keypress is a rebind, never a decode.* The pinned texture is
exempted from ring eviction (one slot), so Y is instant even 500 photos away.

## Semantics (uniform rule, no modes)

- **`CompareToggle` (Y):**
  - no pin → pin the current photo (toast "Pinned for compare").
  - current == pin → present `compare_return` (no-op if none/invalid).
  - else → `compare_return = current`, present the pin.
  - Consequence: the "return point" naturally updates to wherever you last were
    when you flipped — Lightroom's fixed-Select semantics with the vim
    alternate-buffer economy of one key. Navigating while viewing the pin is
    just navigation; the next Y re-captures your new position.
- **`ComparePin` (⇧Y):** pin = current (toast "Pinned for compare"); if the
  current photo IS the pin → unpin (toast "Unpinned"). The whole management
  surface.
- Both are `ActionKind::OneShot`, ids `compare_toggle` / `compare_pin`.
  Y and ⇧Y are free in the default keymap (verified); rebindable like
  everything else via the Shortcuts editor.

## State (AppCore — RAM-only, privacy ADR-018)

```
compare_pin:    Option<usize>   // playlist index of the pinned photo
compare_return: Option<usize>   // where Y returns to from the pin
compare_pin_name: Option<String> // source path/name snapshot, for rebuild re-resolution
```

- **Never persisted** — a pin is a viewing trace; cleared in
  `clear_session_state` (Esc teardown writes nothing). The no-trace test
  already covers the session shape.
- **Deletion:** deleting the pinned photo unpins (toast); indices above the
  removed item shift down by one — fix up `compare_pin`/`compare_return`
  alongside the existing `cursor_after_removal` logic.
- **Playlist rebuild** (recursive toggle, new open, archive): re-resolve the
  pin by `compare_pin_name` the same way rebuild keeps the current photo by
  path; clear both fields if it's gone.
- Archive entries pin fine (they're playlist indices like anything else).

## Residency (the one real engine change)

`pb-core/src/cache.rs::plan_residency(resident, targets, capacity)` gains pin
awareness: the pinned index is always in the plan (callers append it to
`targets`, deduped), so the eviction pass never drops it as the prefetch window
recenters far away. `request_prefetch` includes the pin so it re-decodes if it
was ever lost (fallback: normal preview-first decode — degraded, not broken).

**Property tests (pb-core, TDD-first):** pinned index always survives the plan;
capacity never exceeded; capacity-1 edge prefers the *current* photo over the
pin (the toggle then pays one decode — acceptable, and the ring is never that
small in practice). Measure, don't argue: if flipping between distant
neighborhoods thrashes the prefetch window in practice, that's a measurement
point for a follow-up (two-cursor windowing), not a v1 blocker.

## View carry (what makes it a sharpness tool)

On each toggle, if the two photos share pixel dimensions, copy the outgoing
photo's zoom/pan/scale-mode to the incoming one (both directions, every flip).
Different dimensions → each keeps its own view state. Per-image rotation is
NOT carried (each photo keeps its own).

## Surfaces

- **Toasts:** "Pinned for compare" + thumbtack, "Unpinned" (+ thumbtack-slash
  if it reads better). Vendor FA **solid** `thumbtack.svg` into
  `crates/pb-hud/icons/` + `icon::assets` per the codified icon workflow
  (FA library on this Mac: `/Users/jdlien/Documents/FontAwesome`, `svgs/solid/`).
  No toast on the flip itself in v1 — the title bar names the file; add a small
  filename chip later only if the owner finds flips ambiguous in fullscreen.
- **Menus:** Image menu on both platforms — "Pin for Compare" (`compare_pin`)
  and "Compare with Pinned" (`compare_toggle`, enabled iff a pin exists and a
  photo is shown). muda `menu.rs` (Windows) + `MenuBar.swift` (same Action ids;
  NSMenuItemBadge shows the live Y/⇧Y hints automatically via
  `keymap_slot_display`). Context menu gets both. `MenuState` (+`MenuStateFfi`
  transparent-struct fields, both sides of the bridge) gains the enable flag.
- **Shortcuts editor:** add both actions to `keymap::EDITOR_GROUPS` — both
  shells' editors render from it.
- **Help overlay:** one row ("Pin / flip compare — ⇧Y / Y") via `help_sections`.
- **No new FFI effects** — everything rides the existing keymap/menu/toast paths.

## Non-goals (v1) + future seams

- No split view. If flicker proves insufficient, phase 2 ("show pin + current
  side by side") reuses this exact pin state — presentation change only.
- Slideshow: no special interaction; nav/toggle behave as everywhere else.
- **Squoosh-style export preview (owner note 2026-07-03):** a future exporter
  could flicker original ↔ re-encoded. Keep the toggle's present step a plain
  "present item/texture" call (don't couple it to nav history) so an ephemeral
  in-memory variant can slot in later; build nothing for it now.

## Implementation order (each step green: tests, clippy -D warnings, fmt)

1. **Core semantics, tests first** — `Action::{CompareToggle, ComparePin}`,
   state fields, dispatch arms, delete-fixup + rebuild-resolve; unit tests via
   the headless `AppCore` (existing `handle()`/`dispatch_action` test pattern).
2. **Residency** — `plan_residency` pin exemption + prefetch inclusion,
   property tests (pb-core).
3. **View carry** on same-dimension toggles.
4. **Toasts + icon** (thumbtack vendored).
5. **Surfaces** — keymap defaults (Y/⇧Y) + EDITOR_GROUPS + help + menus
   (muda, MenuBar.swift, context) + MenuState/MenuStateFfi.
6. **CHANGELOG** (Unreleased ▸ Added) + owner smoke.

## Owner smoke checklist

Pin → fly 500+ photos away → Y: instant flip both directions (no decode
flash). Zoom to 100% on a same-dimension burst pair → Y: crop stays put.
Real cull of a burst series. Delete the pinned photo → unpinned toast, no
crash. Toggle recursive mid-pin → pin survives (same folder) / clears (gone).
Pin inside a .zip. Slideshow running + Y. Rebind Y in Settings → menu badge
updates.
