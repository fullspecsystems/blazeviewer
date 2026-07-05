# HUD panel layer — rich panels move off the CPU rasterizer

*Drafted 2026-07-04 from the owner discussion; hardened 2026-07-05 after code review;
revised 2026-07-05 after the owner design review — panel chrome/placement, the tabbed
Inspector, Tab visibility, Esc semantics, persistence, and the committed render seam.
Companion to [ADR-023](decisions.md). The markdown-in-descriptions question (task #44)
surfaced this; the real issue is that the HUD has outgrown its job.*

## The problem

`pb-hud` is a **CPU software compositor**: it rasterizes text + panels into one RGBA8
bitmap and the renderer draws it as a single alpha-blended quad over the photo. That is
exactly right for **ephemeral, non-interactive** overlays. But it has grown into a mini UI
toolkit for rich panels:

- the **EXIF** panel gets cut off because there is no real scroll;
- the **folder tree** reinvents scrolling and hit-testing (`render_tree` returns hit rects
  the shell manually intersects);
- the **help** overlay is a long list with no scroll;
- **recognized text** (`T`) and **AI descriptions** are text-heavy, user-copyable surfaces;
- **descriptions** may arrive as Markdown the HUD cannot render (`**bold**`, `- bullets`).

These are not separate bugs. They are the rasterizer telling us it is the wrong tool for
content the user may want to read, scroll, select, or copy.

## The split

| Job | Members | Needs | Verdict |
|---|---|---|---|
| **Ephemeral over-photo compositing** | toasts, basic info line, play/rotate hints, scan chip, tooltips, empty-state CTA | instant, non-interactive or tiny hit-test, no rich layout | Keep in the CPU quad |
| **Rich panels** | folder tree, EXIF/details, help, recognized text, AI description/answers | scroll, selection, copy, wrap, Markdown, hit-test, native accessibility where possible | Move to a real UI presenter |

The bottom row is **chrome floating over the photo**. It should be backed by a retained UI
layer, but the shared boundary must not be a widget toolkit. The shared boundary is
semantic data + actions.

## Decision

1. **Keep the CPU-quad HUD** for the ephemeral layer. It works, it is off/near the hot path,
   and it needs no rich interaction.
2. **Create shell-neutral rich-panel models in `pb-app-core` first.** This is the load-bearing
   step. The presenters render these models; they do not derive rows, copy payloads, folder
   targets, or help text themselves.
3. **Build an egui viewport overlay for Windows/winit.** Windows has no native toolkit in this
   app, and egui already powers the dialog chrome through `pb-ui`. This work is not throwaway.
4. **Prefer native SwiftUI/AppKit presenters on macOS.** Do **not** build the macOS egui
   `RawInput` bridge as a speculative hedge. Build it only if we need short-term Mac parity
   for several panels before native presenters exist. Otherwise, the Mac path consumes the
   same panel models through FFI and renders them natively.

This means dual presenters are acceptable: egui for Windows, SwiftUI/AppKit for macOS. The
anti-goal is a generic cross-toolkit widget schema that becomes a third UI framework.

## Shared panel contract

Add a narrow, typed contract in `pb-app-core`:

```rust
pub enum RichPanel {
    FolderTree(FolderTreePanel),
    Inspector(InspectorPanel), // tabbed: Details (EXIF) | Text (OCR) | Describe (AI)
    Help(HelpPanel),
}

pub enum InspectorTab { Details, Text, Describe }

pub enum PanelAction {
    OpenTreeTarget(TreeTarget),
    SelectInspectorTab(InspectorTab),
    ClosePanel(PanelId),
    CopyPanel(PanelId),
    CopyRow(PanelRowId),
    AskImage(String),
}
```

The exact structs can differ, but this is the boundary:

- `pb-app-core` owns row ordering, grouping, labels, values, copy payloads, Markdown/plain
  source text, loading/error/empty states, folder targets, and actions.
- Presenters own layout, scroll state, text selection, hover/focus, platform styling, and
  native accessibility.
- `pb-app-core` stays free of `egui`, `SwiftUI`, `winit`, and AppKit.
- Clipboard behavior is explicit: selection copy is allowed for snippets, but each panel also
  has a **copy full panel** action where useful. EXIF/details keeps copying the untruncated full
  data, not merely the viewport-visible rows.
- Chrome actions are core actions: close and tab-select dispatch as `PanelAction`s so panel
  open-state stays authoritative in `AppCore` and `MenuState` checkmarks stay in sync. A
  presenter never closes or switches itself locally.

Useful panel shapes (EXIF/details, Text, and Description are the Inspector's three tab
bodies — separate content models grouped by one panel):

- **Folder tree:** stable row ids, depth, display name, optional count badge, current/open/up
  flags, optional `TreeTarget`.
- **EXIF/details:** span rows and key/value rows, untruncated copy payloads, optional row-copy
  payload.
- **Help:** sections of command labels + shortcut strings from the live keymap/menu model.
- **Text/OCR:** QR payloads and grouped recognized paragraphs, plus busy/error/empty states.
- **Description/Ask:** Markdown source, plain-copy source, busy/error/not-configured states,
  and future Ask input as a panel action or native sheet.

## Panel chrome, placement, and visibility (owner decisions, 2026-07-05)

### The window model: three floating panels, real chrome

At most three rich panels exist, by design: the **Inspector** (tabbed per-image content),
the **folder tree** (navigator), and **Help** (reference). Each gets real chrome — a title
bar with the panel title and an in-band **✕ close** button, drag-to-move by the title bar,
and a header **copy-all** button where the content is copy-heavy (Details/Text/Describe).
Panels float over the photo, may overlap freely, and z-order is last-interacted-on-top.
**No resize, no snapping, no docking in v1** — all deferred polish (see Deferred). Default
homes: tree top-left (as today), Inspector bottom-right (where the info panel lives today),
Help centered. This fixes the actual discoverability pain — today a panel can only be
dismissed by knowing its hotkey or finding the menu item.

### The Inspector: one tabbed content panel

Details (full EXIF), Text (OCR/QR), and Describe (AI description/answers) become **tabs of
one panel**, not three independent panels. The `InfoMode` state machine is already a single
content slot where these replace each other — invisibly, which is the confusing part (press
`T` and the EXIF panel silently vanishes). Tabs keep those semantics but make them legible,
and they solve free-multi-panel's hard problems (overlap management, N positions to persist,
z-order fights) by construction. All three tabs are facets of one question — "tell me about
this image" — which is exactly what tabs express; the tree (navigation) and Help (reference)
answer different questions and stay separate panels. Both target design languages have the
idiom natively: macOS inspectors (Preview, Xcode) and Photoshop tabbed palette groups. If
tabs ever feel cramped, tear-off-into-floating is the established evolution and the contract
doesn't change (tab bodies are already separate content models).

Keymap: `Shift+I` / `T` / `D` open the Inspector at that tab, switch to it if the Inspector
is open on another tab, and close the Inspector if already on it. `?` and `⇧F` keep toggling
Help and the tree.

The **basic info line** (`i` — filename · resolution · format) is explicitly *not* part of
this: it is a glanceable ephemeral readout, stays on the CPU HUD quad, and is untouched by
all panel work.

### Esc always quits (owner)

Insta-quit is a priority feature and stays unconditional — Esc never closes a panel. Panels
close via ✕, their hotkey, the menu, or Tab-hide. Corollary: **no inline text inputs in
panels** — Ask stays a dialog/sheet, where Esc already means dismiss (the winit `esc_guard`
precedent), so Esc never has to arbitrate between "cancel my typing" and "quit the app".
Text *selection* inside panels is fine.

### `Tab` = master panel visibility (Photoshop idiom)

`Tab` (verified free in the default keymap) toggles a `panels_hidden` flag. **Hidden ≠
closed:** the open set is preserved. Any panel hotkey or menu action while hidden **reveals
first** — a toggle while hidden only ever shows, never hides, otherwise the user presses a
panel key and sees nothing happen. `Tab` with no panels open is a no-op. The ephemeral HUD
(toasts, basic line, hints, scan chip) is unaffected. Menu: a checkable "Hide Panels" item
(`MenuState`). Caveat: when a panel owns keyboard focus, `Tab` belongs to the toolkit's
focus traversal — Tab-hide applies only when no panel owns focus.

### Persistence: positions only (owner)

Persist each panel's dragged position (logical points, per panel) via the `persist_prefs`
pattern, written on drag-end and clamped to the visible frame on restore (multi-monitor /
backing-scale safety). Open/closed state is session-only — a fresh launch starts clean.
ADR-018: layout is app footprint, not a viewing trace — in-bounds.

### One design system, two skins

Shared across platforms: the semantic panel models, the action vocabulary, the chrome
affordances (title / ✕ / copy / drag), the visibility system, and the behavioral rules
above. Per-platform: the concrete materials, typography, and icons. **Glass is a chrome
material, not a content backdrop** — panel bodies stay ~90% opaque for legibility (the
owner's call: seeing content *through* a panel is not the goal; moving or dismissing it is);
translucency/blur lives in the frame. macOS uses native materials (`NSVisualEffectView` /
`.regularMaterial`, the newer glass effects where the OS floor allows) — backdrop sampling
composites correctly over the CAMetalLayer, so real blur-over-photo is free — and must
respect Reduce Transparency; icons lean SF Symbols through the semantic `Icon` mapping.
Windows/egui uses flat translucency from `pb-ui` `Palette` tokens (real blur under panels
is a possible later pass sampling the intermediate; not v1). Do not chase pixel parity —
the tokens are semantic (`panel-surface`, `panel-chrome`, elevation); the skins differ.

## Presenter strategy

### Windows / winit: egui overlay

Build an in-window egui overlay for the main viewport:

- `egui_winit` handles input for the main window.
- `egui_wgpu` draws after the photo has rendered and before the surface is presented.
- Reuse `pb-ui` tokens/components where they fit; add panel-specific components in `pb-ui`
  only when they are reusable.

**Committed seam (code review, 2026-07-05): egui renders to an offscreen texture composited
as an overlay layer.** `WgpuRenderer::render()` composites every HUD overlay into the fp16
scRGB **intermediate**, *before* the tone-map/present pass, with `srgb_to_linear` in the
overlay shader (`gpu.rs` passes 2–2d). The egui presenter takes exactly the same path:

- the shell owns an `egui_wgpu::Renderer` targeting an offscreen `Rgba8Unorm` texture,
  **sharing the main renderer's device/queue** (`pb-render` exposes clones);
- `pb-render` gains one texture-backed overlay slot (like `set_tree`, but accepting a wgpu
  texture — no CPU round-trip), drawn by the existing `overlay_pipeline` into the
  intermediate;
- the panel texture is **retained**: egui re-renders only when it requests a repaint
  (interaction, animation, content-generation bump). During hold-to-fly with a panel open,
  photo frames rebind under an unchanged texture — per-nav-frame egui cost is zero;
- egui's requested-repaint deadline feeds the existing `SetWake`/`work_pending` scheduling,
  exactly as the dialog-repaint deadline already does in the winit shell.

Why not the alternatives: drawing egui onto the surface *after* tone-map renders wrong on
HDR desktops (the surface is linear scRGB with SDR-white scaling baked in the scene pass —
panels come out dim/washed); drawing egui directly into the fp16 intermediate hits
egui-wgpu 0.29 float-target gamma subtleties and loses the retained-texture win. **Do not
crib from `dialog.rs`** — the dialog window deliberately creates its own wgpu
instance/device/queue; the overlay must share the main renderer's. Verify egui's
premultiplied-alpha output against the overlay blend state. A window-sized panel texture is
~113 MB at 7680×3840 — acceptable v1; sizing it to the union of panel rects (with pointer
coordinate translation) is the noted follow-up. Version pin: the egui stack is locked at
0.29 ↔ wgpu 22 ↔ winit 0.30 — any new egui-adjacent dep must match. Do not bolt egui on by
racing a second surface/window over the viewport.

### macOS: native presenters first

The long-term Mac direction is SwiftUI/AppKit for chrome. Rich panels should follow that
direction unless a concrete short-term need says otherwise.

For most panels, SwiftUI is enough:

- Help = `ScrollView` / `List` over sections.
- EXIF/details = `Table`, `Grid`, or `List` with context menus / copy commands.
- Text/Description = selectable `Text` / `TextEditor`-style read-only views, native copy,
  VoiceOver, and Speak integration.

For the folder tree, the native options are (refined 2026-07-05 — see Phase 3):

- **Flat SwiftUI `List` over the flat row model (first choice):** our tree is a *navigator*,
  not a Finder outline — click re-roots the deck, and "expansion" is implied by the current
  path, already encoded by the core derivation as flat rows with depth. Render those rows
  directly: `depth` → leading padding, `.badge(count)` for counts, `.onHover` for the hover
  band, native scroll/selection/VoiceOver free. Needs `.scrollContentBackground(.hidden)` /
  `.listStyle(.plain)` to sit inside the panel chrome material.
- **SwiftUI `List(..., children:)` / `OutlineGroup`:** real user-managed disclosure
  chevrons — but that means converting flat rows back into a recursive structure and
  fighting the control's expansion state to mirror "expansion follows the path, clicks
  re-root." Only right if user-managed expand/collapse independent of navigation ever
  becomes a feature.
- **SwiftUI `Table` with `DisclosureTableRow`:** better when columns matter (name + count),
  but heavier than we likely need for a floating navigator.
- **AppKit `NSOutlineView` via `NSViewRepresentable`:** the most Finder-like and capable
  option — type-select, arrow-key navigation, disclosure animation. The escalation case has
  shrunk: keyboard row-selection is deferred (⌘↑/⌘←/⌘→ cover it) and SwiftUI's
  `.typeSelectEquivalent` covers type-to-select on list rows. Use only if the flat SwiftUI
  version fails the owner smoke on feel.

Counts are not disqualifying. They can be trailing row content / `.badge` in SwiftUI or a
custom cell in AppKit. What no native control provides — and none needs to — is the
semantic layer (ancestor chain, siblings, counts, up-row, archive scoping): that stays in
`pb-app-core::folder_tree`, written once, crossing FFI as flat rows.

The macOS egui `RawInput` bridge remains a fallback, not a planned default.

Integration notes (NS1/NS2 scar tissue):

- Panels are SwiftUI views in a ZStack **over `MetalCanvas` in the same window** — never
  child windows/NSPanels (the NS2 `object_setClass` KVO segfault stands as the warning).
  AppKit hit-testing then routes pointer/scroll to panels naturally; the canvas only
  receives what falls through — pointer routing is *easier* than on Windows.
- The NSEvent keyboard monitor intercepts **before** the responder chain: extend the
  existing `panelOpen || alertUp || activeSheet` gate with "a rich panel owns first
  responder", and make ⌘C first-responder-aware so selected panel text beats Copy Image.
- Panel models cross FFI **flattened** (the swift-bridge Vec-of-enum landmine): flat row
  structs / parallel arrays plus a generation counter so SwiftUI diffs cheaply, pulled via
  the established marker-effect + accessor pattern (the dialog stash-pull).
- Smoke fullscreen/resize transitions with a panel open against the fresh compositing fixes
  (commit 0effc66), and test 1×↔2× backing-scale drags — the owner's known multi-monitor
  bug source. Persisted positions are logical points, clamped on restore.

## Input routing

When a rich panel is open, input must be routed by ownership:

- pointer/drag/selection inside the panel goes to the presenter;
- wheel/trackpad scroll inside the panel scrolls the panel, not the photo — this
  deliberately **reverses the folder-tree plan's "no wheel scroll" decision**: that
  rejection assumed a hit-rect bitmap with no positional routing; first-refusal routing
  dissolves the rationale, and the "… n more" paging markers retire with it;
- `Ctrl+C` / `Cmd+C` copies selected panel text when the panel owns focus/selection
  (macOS: first-responder-aware, so selection beats the Copy Image menu command);
- panel-specific shortcuts such as Copy Panel can dispatch `PanelAction::CopyPanel`;
- pointer/keys outside the panel fall through to `AppCore` for pan/zoom/nav — nav keys are
  never captured by a panel unless a widget actually owns keyboard focus;
- **key releases always reach the core held-key tracker, even when the panel layer consumed
  the key-down** — a swallowed `KeyUp` on a held nav key is a stuck fly, the worst bug this
  work can create;
- a panel taking keyboard focus fires the same clear as `FocusLost` (the existing release
  net); focus loss still clears held navigation keys.

This is not optional. Without a first-refusal input gate, selectable text will fight
drag-to-pan and copy-image shortcuts.

## Hot-path safety

The performance contract is concrete:

- with no rich panel visible, egui/SwiftUI panel rendering is not invoked;
- no hidden-panel repaint wake keeps the frame pump alive;
- no per-frame allocations or layout happen on the keypress-to-photon path;
- the photo render pass and resident-ring present path remain untouched;
- CPU HUD toasts/basic info still rebuild only on change;
- with a panel **visible**, hold-to-fly still costs no per-nav-frame presenter work: the
  panel texture is retained and re-renders only on the presenter's own repaint requests —
  never per photo frame;
- the presenter's requested-repaint deadline feeds the existing `SetWake`/`work_pending`
  scheduling (the dialog-repaint deadline already does exactly this).

Correction (code review, 2026-07-05): the scripted-workload NDJSON runner in the
instrumentation methodology **does not exist yet** — today only `--metrics` (`StageTimes`,
RAM-only p50/p95/p99) is real. Phase 1 therefore builds a minimal headless replay: pump
synthetic `CoreEvent::KeyDown`/`Tick` over the pinned corpus (`AppCore` is already
headless-drivable — the Swift host proves it) and dump `StageTimes` percentiles. Compare
before/after in three states: panels hidden, panel open + idle, panel open + hold-to-fly.
"It should be zero cost" is not evidence.

## Sequencing

### Phase 0 — extract panel models

Before any presenter work:

1. Add typed rich-panel model/accessor methods in `pb-app-core`.
2. Move EXIF/details, help, text, and description panel display data behind these methods.
3. Decouple the basic `i` info line from the rich-panel state — it stays on the ephemeral
   HUD, untouched by everything below.
4. Replace the `InfoMode` content slot with the Inspector model (open/closed + active tab),
   add `panels_hidden` and per-panel placement state (positions persisted via the
   `persist_prefs` pattern; open state RAM-only), and add the `ClosePanel`/tab-select
   actions, the `Tab` visibility action, and the menu items + `MenuState` wiring.
5. Keep `pb-hud` rendering as the first consumer so behavior stays unchanged while the seam
   lands (the HUD keeps rendering whichever Inspector tab is active, exactly as `InfoMode`
   does today).
6. Add pure tests for model shape, copy payloads, visibility semantics (reveal-on-toggle
   while hidden; `Tab` no-op with nothing open), and placement clamping.

Exit criteria: current HUD panels still render, but the renderer consumes a semantic model
rather than ad hoc display rows wherever practical.

### Phase 1 — Windows egui overlay seam

Stand up the winit egui viewport overlay with **Help as the pilot panel** — read-only, so
it exercises scroll, chrome (title / ✕ / drag), Tab-hide, and input gating with zero
actions before the tree adds them:

- hidden overlay costs zero in the headless replay harness (built in this phase);
- panel input has first refusal; key releases still reach the held tracker;
- photo interactions fall through outside the panel;
- resize/DPI/theme changes remain correct (test 1×↔2× scale transitions);
- the egui pass is part of the same surface/present path, not a second window;
- the committed offscreen-texture seam is color-correct on both SDR and HDR desktops.

### Phase 2 — folder tree pilot on Windows

Use the folder tree because it exercises interaction:

- render `FolderTreePanel` in a scrollable egui panel with the standard panel chrome
  (title bar, ✕, drag-to-move);
- row click dispatches `PanelAction::OpenTreeTarget` and calls the same core open/rescope path
  the HUD tree uses today;
- keep collapsed-ancestor markers where they represent real model state;
- drop the old "… n more" paging markers unless row count proves virtualization is needed;
- counts render as badges or trailing text.

Exit criteria: Windows tree is done, current HUD `render_tree` is retired for winit, and no new
navigation logic exists in the presenter.

### Phase 3 — native macOS folder presenter

Build the Mac folder presenter over the same `FolderTreePanel` model, starting with a
**flat SwiftUI `List` rendering the model's rows directly** (depth → indentation,
`.badge` counts, native scroll/hover/VoiceOver) — the model is already flat-with-depth and
click means re-root, not expand, so a disclosure tree (`List(..., children:)` /
`OutlineGroup`) would fight the control's expansion state for no gain. Escalate to AppKit
`NSOutlineView` only if the SwiftUI version fails the owner smoke on feel, selection,
keyboard behavior, or count presentation.

This deliberately replaces the earlier "build a macOS egui RawInput bridge" pilot. The bridge
is deferred until there is a concrete need.

### Phase 4 — migrate the Inspector, one tab at a time

Help already landed as the Phase 1 pilot (sections sourced from the live keymap/menu
model). The Inspector migrates tab by tab:

1. **Details (EXIF)** — scroll, row copy, copy full details, no truncation in copy payload.
2. **Text/OCR** — selectable recognized text + QR payloads, copy full text.
3. **Describe/Ask** — Markdown display where useful (see the Markdown rule below),
   selectable text, copy full answer, Speak/VoiceOver-friendly on macOS. Ask input stays a
   dialog/sheet (the Esc rule), reachable from the panel via `PanelAction::AskImage`.

Retire the matching `pb-hud render_*` method as each panel/tab lands (`render_shortcuts`
with Phase 1, `render_table` and `render_paragraph`'s rich uses here). Keep `render_panel`,
toasts, play/rotate hints, scan chip, tooltips, and empty-state CTA in `pb-hud`.

## Interim stopgap (task #44)

Until Description moves to a rich presenter, keep the disposable pure `markdown_to_plain` pass
on the model output so current HUD descriptions read cleanly (strip `**`/`*`/`` ` ``/`#`,
`- ` -> bullet, `[t](u)` -> `t`) plus the prompt nudge toward plain prose. Delete it when the
Description panel migrates.

## Markdown, privacy, accessibility

- **Never fetch remote content from a panel.** Model output can contain `![](https://…)`;
  a markdown widget with image loaders installed fires a network request on render — a
  passive send from the view path, violating ADR-018 by construction. Windows: prefer a
  small in-house Markdown→`LayoutJob` subset (bold/italic/code/bullets/headings) over
  `egui_commonmark`; if `egui_commonmark` is used anyway, install no image loaders and
  render links as plain, non-clickable text. macOS: `AttributedString(markdown:)` with
  remote images stripped. The in-house subset also dodges the version treadmill (the egui
  stack pin above).
- **Accessibility, corrected:** egui 0.29 bundles AccessKit (already transitive in the
  tree via `egui-winit`) — enable it so the Windows overlay speaks Narrator/NVDA. The
  "egui has no VoiceOver story" concern is macOS-only, where the native-presenter decision
  already answers it.

## Open decisions

- **Mac folder presenter:** flat SwiftUI `List` over the flat row model first (see Phase 3);
  `NSOutlineView` remains the escalation if the owner smoke says it doesn't feel native
  enough. Decided by eye at Phase 3, not up front.

## Deferred (deliberately, not forgotten)

- **Docked/sidebar placement mode** — a later additive placement state on the same panel
  model. It needs the "photo draws into a sub-rect of the window" concept, which task #43's
  demoted split view also needs — build that once, for both. Dock/undock must GPU-rescale
  the already-resident textures into the reduced photo rect (refine the parked image
  lazily, preview-first style) — **never** re-fit/re-decode the prefetch ring on a layout
  change. Overlay↔docked switching then falls out, since placement is presenter-side state.
- **Panel snapping, resize, tear-off Inspector tabs** — Photoshop-style polish; the model
  contract already supports all three.
- **Windows blur-under-panel** — real backdrop blur means sampling the intermediate under
  panel rects; panel-visible-only cost, but new render work. macOS gets real material blur
  natively for free.

## What this is NOT

- Not a rewrite of the viewport.
- Not "egui everywhere"; Windows needs egui, macOS should stay native unless proven otherwise.
- Not a generic cross-platform UI schema.
- Not removing the HUD; the ephemeral layer remains exactly what a CPU quad is good at.
- Not a docking/snapping window manager — v1 panel chrome is a title bar, ✕, and drag;
  docked mode is a deferred placement state, not a v1 goal.
