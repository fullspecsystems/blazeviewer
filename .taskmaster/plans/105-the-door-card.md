# Task 105 — The door card: one coherent element, not three fighting mechanisms

**Status:** planned — **rev4** (2026-07-17). Codex-reviewed; all six blockers verified and incorporated.
**Task id:** 105 (`tasks.json`), depends on 104. Supersedes 104's *"design the door tile"*.
**Depends on:** task #104 — **implemented through Phase 3, status `review`**: owner
validation on a real folder and the two `cfg(macos)` routing arms are still outstanding.
Not "shipped" (rev1–3 said so; wrong). Either 104's validation lands first, or 105
absorbs it explicitly — see *Open questions*.
**Scope:** `pb-app-core`, `pb-app` (egui), **`pb-mac-ffi` + `mac/`** (SwiftUI),
`egui_shot`, `tasks.json`, `CHANGELOG.md`.

> **rev4.** Codex reviewed rev3: *"right architecture, not implementation-ready."* Every
> blocker was checked against the source before being written here; **all six were real**,
> and two were things this plan's own author had left behind. The direction is unchanged —
> typed deck item, transparent sentinel, affordance in chrome — but the plan now covers
> overlay lifecycle, thumbnails, command gating, DPI-safe layout, and a measured
> performance gate.

## The problem, and why it kept recurring

Four defects in one evening, all found by the owner on screen:

| Symptom | "Fix" at the time |
|---|---|
| An FA glyph drawn for 16 px, blown up ~12× to fill a 7680-wide display | Rasterize it decode-to-fit |
| Decode-to-fit cost a photo-sized ring slot for an *icon* | Give the affordance to the play pill; flatten the tile |
| The artwork sat in an invented grey box | Stop matting it; keep the alpha |
| The artwork magnified ~2.1× — "looks weird at giant sizes" | `ViewTransform::never_upscale` |
| The filename is invisible unless the info line is up | *(unsolved — the trigger for this plan)* |

**One root cause: a door is UI, and we have been rendering it through the photo
pipeline.** Every fix bent something built for photographs — decode-to-fit, alpha
blending, scale modes, the prefetch ring — into doing a widget's job.

The codebase already draws the line: **viewer hot path is wgpu; chrome is egui /
SwiftUI**. A door belongs on the chrome side. It is not a picture of anything.

## The design (owner, 2026-07-17)

> *"I wouldn't make it a black tile, but just the regular letterbox — empty. Transparent.
> Then we show a card rendered as an egui element in the middle. […] Like a giant, fancy
> version of the play button […] basically says 'we're looking at wedding-photos.zip' P to
> open."*

The item **draws nothing**; one centred card carries the artwork, the filename, and the
`Open · P` button. Ambient and non-modal, modelled on `ScanPill` (`panels_ui.rs:282`).

> 💡 **The layout already exists.** `thumb_cell` (`panels_ui.rs:3416`) draws *"the thumb
> fit-within a letterbox box, a type badge, and the **middle-truncated filename below**"*
> via `middle_truncate` (`panels_ui.rs:3590`). The card is that cell, larger, with a
> button. Reuse both rather than inventing a second truncation and a second layout — and
> note it is also the answer to §4.

## What it deletes

The argument for it — this **removes** machinery rather than adding a layer:

- `play_hint_kind == 3` + `play_hint_persistent` — the card *is* the button.
- The `Icon::Archive` / `doc.zipper` cases and the *Open*-vs-*Play* label split.
- The door tile's artwork decode + composite in `engine.rs`, and the whole *"how big
  should the tile be"* question — answered wrong three times (flat → decode-to-fit → flat
  → artwork).
- `ViewTransform::never_upscale` — **remove it**; §5 makes the right size do the job, and
  speculative state should not survive its only caller.
- 🧹 **`pb_hud::icon::assets::FILE_ZIPPER` is already dead** (`pb-hud/src/icon.rs:67`) —
  its only caller went away when the pill took over, and the const + vendored SVG were
  left behind. Codex caught it; delete with this work.

## Design details

### 1. The sentinel: 1×1 transparent, and dimensions are *omitted*

`archive_placeholder` returns a **1×1 fully transparent** RGBA frame — the cheapest
possible ring slot. The scene pass clears to the **configured letterbox colour**
(`gpu.rs`: `letterbox_linear(self.letterbox)` at `:2996` / `:3029`, default
`LETTERBOX = [10,10,12,255]` at `:16`, settable via `set_letterbox`) and draws the photo
quad with `BlendState::ALPHA_BLENDING`, so `a = 0` leaves it untouched.

> ✅ Already proven: `pb-render`'s `transparent_image_blends_over_letterbox`
> (`gpu.rs:4596`) covers exactly this.
>
> ⚠ rev1–3 said "the black clear". **Wrong** — the letterbox is configurable and defaults
> to `[10,10,12]`. It is also why the grey box was so visible: the matted backdrop was
> `[38,38,42]` against a `[10,10,12]` letterbox.

1×1 is only safe because **dimensions are omitted for a door everywhere** (§6). Never
render `1 × 1`.

The **no-read guarantee is untouched**: the typed dispatch still returns above
`source.bytes()`.

### 2. The card is a `PanelFrame` member

Mirror `ScanPill`: pure semantic data, **never a cloned RGBA buffer**.

```rust
pub struct DoorCard {
    /// The archive's file name, e.g. `wedding-photos.zip` (full; the shell elides).
    pub name: String,
    /// Secondary line, from `ArchiveKind::name()` — e.g. `ZIP archive`.
    pub format: String,
    /// The Open shortcut (`P`), from the live keymap — never hard-coded.
    pub shortcut: String,
}
```

`AppCore::door_card() -> Option<DoorCard>`, keyed off the **presented** item (§3).

### 3. The overlay lifecycle — the card will *not* appear on its own

**Blocker (verified).** The winit overlay is retained, removed when
`overlay_panel_visible()` (`main.rs:1458`) is false, and rebuilt only when
`overlay_dirty` (`main.rs:525`, set at `:1524`) or egui asks for a repaint. Adding a
`PanelFrame` field does none of: keep the texture alive, dirty it when the presented item
changes, clear it moving door → photo, or update the name while blazing consecutive
archives.

Each shell needs an explicit **door-card visibility + signature seam**:

- `overlay_panel_visible()` must be true while a door is presented — it is content, not a
  panel (§8).
- A signature (presented item + name) marks `overlay_dirty` on change — the same
  rebuild-on-signature shape the subtitle-track menu already uses.
- It must track the **actually presented item** (`displayed_item`, set in
  `mark_resolved`), **not** the playlist cursor, or the card will name an archive the
  screen is not showing yet.

### 4. Thumbnails must not go blank

**Blocker (verified).** Archive artwork currently reaches the thumb strip *through the
decoded placeholder*. With a transparent sentinel, `thumb_cell` (`panels_ui.rs:3416`)
gets a blank image, and its badge logic knows only video / Live Photo / animation. Same
on macOS (`pb-mac-ffi`, `ThumbnailsPanel.swift`).

**Draw the same cached artwork directly in the thumbnail cell**, optionally with a compact
archive badge. **Do not** put the large artwork back into the resident ring — that is the
mistake this plan exists to undo.

### 5. Sizing — the fixed 512 pt rule was wrong

**Blocker (verified).** rev2's "512 pt is 1:1 on retina" holds only to 200%; Windows goes
past it. And 512 pt of art **cannot fit** the macOS minimum window —
`.frame(minWidth: 520, minHeight: 360)` (`BlazeViewerMacApp.swift:204`) — once the name
and button are included.

```text
art_points = min(
    512,                                    // the design cap
    asset_pixels / display_scale,           // never magnify: 1024 / scale
    available_width  - horizontal_padding,
    available_height - text_and_button_height
)
```

- On a compact window, **shrink or omit the artwork first**; keep the name and the Open
  control at accessible token sizes. Do **not** scale "art and all" uniformly — rev2 said
  that, and it would produce an unreadable filename in a small window.
- The name is **one-line middle ellipsis preserving the extension** (`middle_truncate`);
  the full name goes to hover / accessibility.
- Test 1×, 1.5×, 2×, >2×, plus 520×360 and a pathological filename.

### 6. What a door reports: size, not dimensions

A door has no pixels to describe. **Never probe** — not the entry count, not whether it is
encrypted. Owner: *"Too expensive for blazing."* All we know is its size and its
extension, and that is all we say.

- **Details panel (`Shift+I`) — ✅ shipped** (`cf8ae8a`): size via `fs::metadata` (or
  `size_hint` for an entry) + a `Format` row, no EXIF, no probe, precise bytes, cached
  behind `exif_cache` so the stat runs once per item.
- **Info line (`i`) — to do**: **omit dimensions**; show a human-readable size instead.
- **Copy Details** — omit dimensions there too.

> 🪤 **Nothing may `stat` on the frame path *or* on a blaze step.** `info_line_parts`
> (`app_core_impl.rs:4746`) is reached from `info_line_snapshot()` while the shell builds
> its `PanelFrame` — a `fs::metadata` there is disk I/O on the event loop **every frame**.
> Codex extends this correctly: **a stat over SMB can block**, so it must not run per
> *item* on the event loop either.
>
> Sources, best first: (a) teach the **scan** to carry sizes — `read_dir` yields metadata
> essentially free on Windows, and `FsSource` does not implement `size_hint` today (only
> the archive sources do); (b) fill asynchronously and show the size when it lands.
> **Not** a stat at the `meta_cache` insert, which is on the event loop.
>
> `archive::human_gb` is **GB-only** (it formats the RAM budget) — a door needs KB/MB/GB.

### 7. Command matrix — photo-only commands must be gated

**Blocker (verified).** With a transparent sentinel, Copy Image copies a transparent
pixel; Compare (`compare_pin_cmd`, `app_core_impl.rs:3280`) pins a UI sentinel; OCR /
Describe / Ask run on nothing; Zoom implies the door is content.

| Command | On a door |
|---|---|
| Open (`P`), navigation, Copy Path, Reveal, Details, Delete the archive file | **Valid** |
| Rotate / Save rotation | Disabled — already toasts honestly (`app_core_impl.rs:598`) |
| Copy Image, OCR/Text, Describe/Ask, Compare | **Disabled** or an explanatory no-op |
| Zoom | Decide (open question 2) — harmless on a 1×1, but the *card* does not zoom |

Audit menu state, context menus, shortcuts, and dispatch — not just the keymap.

### 8. The card's place in the overlay stack

**Blocker.** A centred card can collide with the tree, inspector, info line, help, toolbar
inset, toast, or scan pill. Define it as **content chrome**:

- Above the photo canvas; **below** help, dialogs, progress, and toasts.
- Centred in the **unobstructed content area** (minus open side panels), not blindly in
  the window.
- **Persistent** when panels are hidden with `Tab`, in fullscreen, and while blazing (§10).

### 9. Artwork: one asset, one texture per shell

Asset stays in `pb-app-core/assets/` (`cfg(windows)` = manila / else blue). Both decode,
have alpha, are 1024×1024 sRGB WebP.

- Decode **off the event loop**, or initialise before the first door is presented.
- Cache **one egui texture**; `pb_ui::icon` already caches rasters in the egui ctx.
- macOS: a **one-time generation + width/height + RGBA** bridge accessor cached in
  `CoreModel`. **Do not** clone ~4 MiB through FFI per pump. *(This is why `pb-mac-ffi` is
  in scope — rev3 omitted it.)*
- **Artwork failure degrades to a text-and-button card**, never a hidden door.
- Consider **lossless** WebP/PNG for the assets: they are lossy 4:2:0 today, a poor fit for
  hard edges and fine linework, and the size cost is irrelevant for two one-time assets.

### 10. Blazing — the card shows (owner, 2026-07-17)

The play pill is suppressed while a nav key is held (`maybe_show_anim_hint`'s nag rule).
The card must **not** inherit that: the sentinel draws nothing, so blazing through a folder
of archives would show an **entirely blank screen**. Owner: *"part of the rationale for not
showing things is to keep it fast, but without showing something it'll just look broken."*
It is the item's content, not a nag about it — the first overlay that survives blazing,
deliberately. See the gate below for what that costs.

## Performance — the claim, corrected, and the gate

rev2 claimed the card "costs nil". **Unsupported, and withdrawn** — CLAUDE.md: *performance
claims require numbers from the benchmark corpus.* The honest statement:

> No archive payload read, no image decode, and no artwork upload after init — but **one UI
> update per newly presented archive**. While blazing consecutive doors the filename changes
> each frame, which on Windows means text layout, allocation, a full egui offscreen render,
> and composition of the full-window overlay. **This is the first persistent chrome proposed
> for the blaze hot path.**

**Gate before closing 105:** measure an image-only deck against a consecutive-door deck at
the target resolution and 120 Hz; report **p50/p95/p99** CPU frame time and keypress→photon
(never means). If full-window egui repaint is material, retain the card in a smaller overlay
texture/quad.

## Phases

**Phase 0 — tracking.** Numeric task 105 + subtasks in `tasks.json`, depending on 104.
Resolve 104's outstanding validation (open question 3).

**Phase 1 — core.** 1×1 transparent sentinel; `door_card()` keyed off the presented item;
omit dimensions for doors (info line + Details + Copy Details); a size source that never
stats on the event loop (§6); expose the artwork once. Tests: sentinel is transparent and
still costs no read; `door_card` is `Some` only on a door and carries the live shortcut; the
info line omits dimensions and reports a size; **no `fs::metadata` on the frame path**.

**Phase 2 — winit/egui.** `PanelFrame.door`; the visibility + signature seam (§3); the card
from `pb-ui` atoms + `middle_truncate`, sized per §5; archive thumbnails (§4); command
gating (§7); stack placement (§8); shows while blazing (§10). Remove the pill's kind-3 path.

**Phase 3 — macOS.** The same card in SwiftUI; the artwork bridge (§9); thumbnails; command
gating. ⚠ **Requires a real Swift host build + a real-folder smoke test** —
`cargo check -p pb-mac-ffi` validates only the Rust half.

**Phase 4 — measurement.** The blaze gate above. Do not close 105 on an unmeasured claim.

**Phase 5 — cleanup.** Delete `play_hint_persistent`, the kind-3 arms, `never_upscale`,
`pb_hud::icon::assets::FILE_ZIPPER` + its vendored SVG; and if `Icon::Archive` ends up
unused, its glyph arms, gallery entry, and **both** vendored family SVGs. Update
`CHANGELOG.md` — its current text describes an archive tile/icon.

**Screenshots:** extend **`--egui-shot`** (`egui_shot.rs` — the viewer-overlay harness),
**not** `--settings-shot` (the Settings equivalent), with door-card modes: long names,
compact windows, several scale factors, light/dark.

## Open questions

1. **Does the card also show the size?** The info line carries it (§6) and the card names
   the file. Lean: name + format on the card, size on the line — the line is where facts
   live. (Codex's suggestion to use the otherwise-unused `DoorCard.format` as the card's
   secondary line is adopted in §2.)
2. **Zoom on a door** (§7) — no-op, or leave it harmless?
3. **104's remaining validation** — finish first, or absorb into 105? 104 is `review`, not
   done: it still wants a real-folder pass and a Mac build for the `cfg(macos)` arms.

## Risks

1. **The card is built twice**, egui + SwiftUI. Inherent to native chrome; the deal
   `dialog.rs` and the SwiftUI panes already accept. Owner: *"And then do it again in
   SwiftUI :P"*.
2. **First persistent chrome in the blaze path** — hence the gate.
3. **No golden-image coverage**: shell-rendered chrome can't ride the headless wgpu render
   tests; `--egui-shot` is the nearest equivalent.
4. **macOS unverifiable from the Windows box** — the standing blind spot (the `cfg(macos)`
   arms, the blue asset).
5. **It rewrites two-day-old code.** Owner: *"that's just iteration. Write code delete
   repeat."* The deleted version is what made the design legible.

## Non-goals

- Per-format artwork; the card names the format in text.
- Any interactivity beyond Open — no context menu, no content preview.
- Touching the pill for videos/animations; only the door's kind-3 borrow goes away.
- Probing archives for entry counts, encryption, or anything needing them opened.
