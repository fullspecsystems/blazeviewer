# Task 105 — The door card: one coherent element, not three fighting mechanisms

**Status:** planned — rev2 (2026-07-17, owner-designed + sized; not yet Codex-reviewed)
**Proposed task id:** 105 (supersedes task 104's *"design the door tile"* follow-up)
**Depends on:** task #104 (archives as doors) — shipped through Phase 3 on `main`.
**Scope:** `pb-app-core` (one accessor, a trivial tile) + `pb-app` (an egui card) +
`mac/` (the same card in SwiftUI). Net **deletes** more than it adds.

## The problem, and why it kept recurring

Four separate defects in one evening, all found by the owner on screen:

| Symptom | "Fix" at the time |
|---|---|
| An FA glyph drawn for 16 px, blown up ~12× to fill a 7680-wide display | Rasterize it decode-to-fit |
| Decode-to-fit cost a photo-sized ring slot for an *icon* | Give the affordance to the play-hint pill instead; flatten the tile |
| The owner's artwork sat in an invented grey box | Stop matting it; keep the alpha (the scene pipeline already alpha-blends) |
| The artwork magnified ~2.1× — "looks weird at giant sizes" | `ViewTransform::never_upscale`, a per-item scale cap |
| The filename is invisible unless the info line is up | *(unsolved — the trigger for this plan)* |

**One root cause: a door is UI, and we have been rendering it through the photo
pipeline.** Every fix bent something built for photographs — decode-to-fit, alpha
blending, scale modes, the prefetch ring — into doing a widget's job. The filename was
simply the first symptom with no available bend.

The codebase already draws the line in the right place: **the viewer hot path is wgpu;
chrome is egui / SwiftUI** (Settings, About, the dialogs, the info line, the play pill,
the scan pill). A door belongs on the chrome side. It is not a picture of anything.

## The design (owner, 2026-07-17)

> *"I wouldn't make it a black tile, but just the regular letterbox — empty.
> Transparent. Then we show a card rendered as an egui element in the middle. […] Like
> a giant, fancy version of the play button, basically. A little card, maybe it's a
> little bit like the scanning status pill […] basically says 'we're looking at
> wedding-photos.zip' P to open."*

**The item draws nothing.** Its decoded frame is fully transparent, so the scene pass
blends it to nothing and the normal letterbox shows through — no black tile, no special
case in the ring, present, or nav paths.

**One centred card carries everything**: the folder artwork, the filename at a readable
size, and the `Open · P` button. Ambient and non-modal, modelled on `ScanPill`
(`panels_ui.rs:282`) — a pure data snapshot out of the core, drawn by the shell.

```
        ┌─────────────────────┐
        │      [artwork]      │
        │  wedding-photos.zip │
        │    [ 🗀 Open  P ]   │
        └─────────────────────┘
```

## What it deletes

This is the argument for it. The card **removes** machinery rather than adding a layer:

- `play_hint_kind == 3` and `play_hint_persistent` (`app_core_impl.rs`) — the card *is*
  the button; the pill goes back to meaning "this plays".
- The `Icon::Archive` / `doc.zipper` cases in both pills, and the *Open* vs *Play* label
  split.
- `ViewTransform::never_upscale`'s **only caller** — the card draws its art at a fixed
  512 pt, so there is no scale to cap (see §5: the right *size* replaces the clamp). ⚠
  See *Open questions*: the field then has no user, and either the owner's "shrink to
  fit" mode adopts it or it should be reverted.
- The door tile's artwork decode + composite in `engine.rs`, and with it the entire
  *"how big should the tile be"* question — which has now been answered wrong three
  times (flat → decode-to-fit → flat → artwork).

## Design details

### 1. The transparent tile

`archive_placeholder` returns a fully transparent frame. `BlendState::ALPHA_BLENDING`
over the `Color::BLACK` clear (`pb_render::gpu`) means `a = 0` leaves the letterbox
untouched — invisible, with no change to the ring/present/nav paths and no new "an item
with no texture" case to plumb.

The **no-read guarantee is untouched**: the typed dispatch still returns above
`source.bytes()`, which is what makes a door safe to prefetch past. It gets cheaper —
the ring slot is now trivial.

⚠ **Size it deliberately, not at 1×1.** `DecodedImage.orig_width/height` feed the info
line's dimensions readout, so a 1×1 tile would have the info line announce `1×1` for
every archive. See *Open questions*.

### 2. The card is a `PanelFrame` member

Mirror `ScanPill` exactly:

```rust
pub struct DoorCard {
    /// The archive's file name, e.g. `wedding-photos.zip`.
    pub name: String,
    /// The format, from `ArchiveKind::name()` — e.g. `ZIP`, `7z`, `TAR.GZ`.
    pub format: String,
    /// The Open shortcut (`P`), from the live keymap — never hard-coded.
    pub shortcut: String,
}
```

Core exposes one accessor (`AppCore::door_card() -> Option<DoorCard>`); the shell
snapshots it into `PanelFrame.door`, and `door_card()` in `panels_ui.rs` draws it from
`pb-ui` atoms + the existing `draw_open_button`. No core state, no new effects.

### 3. The artwork reaches both shells from core

The asset stays in `pb-app-core/assets/` (one copy, `cfg(windows)` = manila / else
blue). Core exposes the **decoded pixels**; each shell uploads a texture **once** and
caches it — `pb_ui::icon` already caches rasters in the egui ctx, so the pattern exists.
macOS gets the pixels over the bridge and builds an `Image`.

Rejected: bundling the art separately in the `.app`. Two copies of an asset that must
not drift, for no gain.

### 4. Click and key stay as they are

The card's button dispatches `PanelAction::PlayPause`, exactly as the pill does today —
which already enters a door. `P` is unchanged. No new action, no new keymap entry.

## Phases

**Phase 1 — core.** Transparent tile; `door_card()`; expose the art pixels. Tests: the
tile is transparent and still costs no read; `door_card` is `Some` only on a door and
carries the live shortcut.

**Phase 2 — winit/egui.** `PanelFrame.door` + `door_card()` in `panels_ui.rs`; the art
texture uploaded once into the ctx, drawn at **512 pt, shrinking to fit** (§5). Shows
while blazing — do *not* gate it on the pill's nag rule. Remove the pill's kind-3 path.

**Phase 3 — macOS.** The same card in SwiftUI; art over the bridge. ⚠ Cannot be compiled
here — `cargo check -p pb-mac-ffi` covers only the Rust half.

**Phase 4 — cleanup.** Delete `play_hint_persistent`, the kind-3 arms, `Icon::Archive`
if now unused, and resolve `never_upscale` (adopt or revert).

### 5. Sizing: 512 pt of artwork, shrinking to fit (owner, 2026-07-17)

The card's artwork draws at **512 pt** — not the art's full 1024, and never the
viewport. That number is not arbitrary, and it is the whole reason this design fixes
the crispness problem for free:

| Display | 512 pt in physical px | vs the 1024 px asset |
|---|---|---|
| Retina / 2× | **1024** | **exactly 1:1** |
| Windows @ 150% | 768 | a downscale — crisp |
| 1× | 512 | a downscale — crisp |

egui and SwiftUI both lay out in points, so the art lands at or below its native
resolution on **every** display. It is never magnified — which is precisely what
`never_upscale` was invented to prevent, achieved here by picking the right size
instead of clamping a scale.

**It must still shrink.** 512 pt is a *maximum*: on a small window the card scales down
so it always fits, art and all. Nothing about a door should ever be clipped or push
past the viewport.

## Open questions — decide before building

1. ~~**Does the card show while blazing?**~~ **Resolved (owner, 2026-07-17): yes.** The
   play pill is suppressed while a nav key is held (`maybe_show_anim_hint`'s nag rule),
   and inheriting that would be a real bug now — the tile draws nothing, so blazing
   through a folder of archives would show an **entirely blank screen**. Owner: *"part
   of the rationale for not showing things is to keep it fast, but without showing
   something it'll just look broken."* This makes the card the **first** overlay that
   survives blazing; that is deliberate, because it is the item's content, not a nag
   about it. (Cost is nil: the card is static per item — no per-frame decode, no raster,
   just a cached texture and two labels.)
2. **What should the info line say for a door?** It reads dimensions off the decoded
   frame, which is now a lie whatever size we pick. Options: special-case doors to show
   file size + format (the panel already does this — `app_core_impl.rs`'s `Archive` arm
   reads a stat), or suppress the dimensions row. Related: the card and the info line
   would both name the file — is that redundant, or is the info line's the one to drop?
3. **`never_upscale`'s fate.** Its only caller disappears here. The owner asked whether
   "shrink to fit" should be a real user-facing mode; if yes, this field is its engine
   and stays. If not, revert it with this change rather than leaving a tested,
   documented, unused feature.
4. **Does the card need the archive's file size?** "wedding-photos.zip · 271 MB" sets an
   expectation before a 7z spends ten seconds decompressing. Cheap (a stat, already done
   for the panel) — but it is another row.

## Risks

1. **The card is built twice**, egui and SwiftUI. Inherent to native chrome and already
   the deal `dialog.rs` + the SwiftUI panes accept; `pb-ui` makes the egui half cheap.
   Owner: *"And then do it again in SwiftUI :P"* — accepted going in.
2. **No golden-image coverage.** A shell-rendered card can't appear in the headless wgpu
   render tests the way a decoded tile can. `--settings-shot`-style capture is the
   nearest equivalent.
3. **macOS is unverifiable from the Windows box** — same standing blind spot as the
   `cfg(macos)` routing arms and the blue asset.
4. **It is a rewrite of two-days-old code.** `play_hint` kind 3, the persistent flag and
   `never_upscale` all shipped hours before this plan. They were not wasted — they are
   what made the design problem legible — but the diff will read as churn without this
   document.

## Non-goals

- **Per-format artwork.** One picture per platform; the card names the format in text.
- **Making the card interactive beyond Open.** No context menu, no rename, no preview of
  contents — entering the archive is the feature.
- **Touching the pill for videos/animations.** It keeps its flash-and-fade behaviour;
  only the door's kind-3 borrow goes away.
