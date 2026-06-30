# Handoff: a HUD component library + demo (the on-image overlays)

_Written 2026-06-30. A task brief for the next agent. The owner wants to standardize the
**on-image HUD overlays** (info cards, toasts, the scan status card, buttons, the loading
pie) into a small reusable component library with consistent tokens (sizing, padding, radii,
colors), and a **demo/gallery** to preview them — the equivalent of the egui `pb-ui` gallery,
but for the HUD layer._

---

## The one critical thing to understand first

PhotoBlaze has **two completely separate UI systems**. Do not conflate them:

1. **egui (`crates/pb-ui`)** — the **dialog chrome** (Settings, About, Confirm / Message /
   Password / Loading / Scanning dialogs) in a *second winit window* (`pb-app/src/dialog.rs`).
   It already has a component library + a Storybook-style gallery:
   `cargo run -p pb-ui --example gallery`. This is the **precedent** for what the owner wants —
   tokens + components + a gallery — **but it is egui**, and it cannot render the HUD overlays.

2. **HUD (`crates/pb-app/src/hud.rs`)** — the **on-image overlays**: drawn directly on the
   photo frame, **not egui**. They are **CPU-composited straight-alpha RGBA8 bitmaps**, handed
   to the renderer and drawn as a **single alpha-blended wgpu quad each** (one quad per layer:
   toast, info panel, pie, scan card, empty-state hint). Rebuilt only when their *content*
   changes — never per frame (Prime Directive: off the photo hot path).

**This task is about #2.** There is no component library or gallery for the HUD layer yet.
The owner's words: _"a little component library for our info cards/toasts/hud etc. so that we
can standardize the appearance and dimensions and sizing… worked into our little demo app."_
And: _"I'll need a couple of these sorts of buttons."_

---

## What exists in the HUD layer today (`crates/pb-app/src/hud.rs`)

The compositor is `Hud` (loads the OS UI font — Segoe UI / SF Pro / DejaVu — at runtime).
Each component is a method returning an RGBA8 bitmap `(Vec<u8>, w, h)` (or `+ a sub-rect`):

| Component | Method | Used for |
|---|---|---|
| One-line pill | `render_panel` / `render_panel_icon(text, px, pad, icon, bg)` | basic `i` overlay; command toasts (optional leading icon) |
| Two-column table | `render_table(rows: &[Row], px, pad, bg)` (`Row::Span`/`Pair`) | full-EXIF panel; help overlay |
| Centered lines | `render_centered(lines, px, pad, bg)` | empty-state "Press O to open…" hint |
| **Status card** | `render_scan_card(heading, path_line, count_line, button_label, button_icon, px, width, bg) -> (rgba,w,h,button_rect)` | the scan status card — the newest, most-designed component |
| Loading pie | `render_pie(diameter, progress, glow)` (no font) | the "not-ready" spinner |

**Primitives** (private, on `Canvas` — a software straight-alpha RGBA8 buffer):
- `Canvas::new(w,h,bg,radius)` — rounded-rect background, AA corners via `corner_coverage`.
- `draw_line` (outlined text), `draw_icon` (outlined icon — black halo for legibility over
  photos, then the white glyph/icon).
- `fill_round_rect` — filled rounded rect (the button's faint fill).
- `stroke_round_rect` — rounded-rect **outline**; **SETS** the ring to a translucent
  `(rgb, alpha)` via `set_toward` (not `over`) — see the gotcha below.
- `over` (composite over), `blit_rgba` / `blit_silhouette` (blit an icon), `set_toward`
  (premultiplied lerp toward a target that can *reduce* alpha — for the translucent border).

**Tokens / helpers (currently scattered as magic numbers — the thing to centralize):**
- Colors: `BG = (0,0,0,153)`, `TEXT = white`, `TEXT_DIM = (188,191,198)` (card secondary
  line), `SHADOW`/`SHADOW_ALPHA` (the legibility halo). `bg_for_opacity(pct)` for the panel.
- `Weight { Regular, Semibold, Bold }` (real faces + faux-bold smear on macOS), `Keep {
  Start, End }` (which end to keep when eliding), `fit_text(text, px, weight, max_w, keep)`
  (width-aware ellipsis), `format_thousands(n)`.
- Per-component sizes are ad-hoc: e.g. the scan card uses `pad = px*0.85`, `card_r = px*0.6`,
  `px_sub = px*0.82`, `gap_lines = px*0.14`, button radius `px*0.42`, border `px*0.08`, etc.
  **These should become named tokens** (a `pb-ui`-style scale: spacing, radii, the text ramp,
  gaps), so cards/toasts/panels share one source of truth and stop drifting.

**The scan card's current design (keep it / make it the reference):** fixed width (320 logical
px, clamped to the window), **center-aligned**; a **semibold heading** `Scanning "Folder"`, a
**dim** current-folder line (left-elided `…/leaf` when long), a **dim** count line, and a
**centered button** (stop icon + label) with a faint fill and a **50%-translucent rounded
border** (radius a touch tighter than the card). Equal inset from the window's top + right
edges. The app rebuilds it throttled to ~120 ms (the live path changes per directory). See
`push_chip`/`tick_chip` in `main.rs` and `streaming-playlist-runbook.md` §4.

---

## What to build

### 1. Centralize tokens
Define a `tokens` section in `hud.rs` (or a small `hud/tokens.rs`): a spacing scale, corner
radii (card vs button), the text-size ramp (heading / body / secondary / caption) as
multiples of a base `px`, gaps, border thickness/alpha, and the color roles (`TEXT`,
`TEXT_DIM`, `BG`, shadow). Re-express the existing components in terms of these so the scan
card, toasts, and panels are visually consistent. Mirror how `pb-ui/src/lib.rs` does it
(`SPACE_*`, `RADIUS_*`, `CONTROL_H`, `Palette`) — but for the HUD's CPU compositor.

### 2. Extract a reusable HUD **button** (the owner needs "a couple of these")
The scan card currently draws its button **inline**. Extract it:
- `button_size(label, icon, px) -> (w, h)` (measure, for layout) and
  `draw_button(&mut Canvas, x, y, label, icon, px) -> [u32;4]` (draw into a target canvas,
  return the rect). Refactor `render_scan_card` to use them.
- **GOTCHA — read this before you "render a button to a bitmap and blit it":** the button's
  border is drawn with `stroke_round_rect` → `set_toward`, which **SETS** the ring's alpha to
  0.5 so the photo shows through (a true 50%-opacity outline). This only reads as 50% when
  drawn **directly onto the card canvas** (it overrides the card bg's alpha there). If you
  render the button as a *separate* straight-alpha bitmap and `blit_rgba` it over the card,
  the 50% border re-composites **over** the card and reads ~80% again. So the reusable button
  must be a **`draw_button(&mut Canvas, …)`** that draws into the destination — *not* a
  standalone bitmap. (For a standalone swatch in the gallery, draw it onto a swatch `Canvas`
  with the same `bg`.) A `render_button(label, icon, px, bg) -> (rgba,w,h)` swatch wrapper for
  the gallery is fine because the swatch *is* its own canvas.

### 3. The demo / gallery
A way to render the HUD components to a **viewable PNG sheet** so they can be previewed and
iterated **without launching the full app** (the HUD-layer equivalent of the egui gallery).

- **Constraint:** `hud` is a **private module of the `pb-app` binary crate** (no `lib.rs`), so
  an `examples/` file *cannot* `use` it. Two options:
  - **(Recommended, smaller)** A hidden CLI flag in `main.rs`, e.g.
    `--hud-gallery <out.png>`: it has direct access to `hud`/`icon`; render the components onto
    a sheet, encode PNG, write, exit before the event loop. Run:
    `cargo run -p pb-app -- --hud-gallery /tmp/hud.png`.
  - **(Bigger)** Give `pb-app` a `lib.rs` exposing `pub mod hud; pub mod icon;` and add a real
    `examples/hud_gallery.rs` (the bin then becomes lib+bin; more churn, but matches the egui
    gallery's `example` shape).
- **PNG encoding, no new dep:** `resvg` is a direct dep and re-exports `tiny_skia`
  (`crates/pb-app/src/icon.rs` already uses `resvg::tiny_skia`). `tiny_skia::Pixmap` stores
  **premultiplied** RGBA — so composite the components onto an **opaque** sheet (then
  premult == straight) and call `pixmap.encode_png()`. (`Canvas` is private; do the final
  composite of the straight-alpha tiles over the opaque sheet with a small blend loop in the
  gallery code, or expose a tiny helper.)
- **Contents:** show the scan card, the info/EXIF table (`render_table`), a toast
  (`render_panel_icon`), a few **button variants** (Cancel Scan/`STOP`, Copy/`CLIPBOARD`,
  Delete/`TRASH`), and the pie — each captioned, over a **mid-gray or gradient** background so
  translucency is visible.

---

## How to iterate visually (this is the key workflow)

You can **see** what you render, headlessly:
1. A temporary `#[test] #[ignore]` in `hud.rs` that renders a component and writes its raw
   RGBA to the scratchpad, printing `w h`.
2. Convert with ImageMagick (installed: `magick`): over a flat bg
   `magick -size WxH -depth 8 rgba:in.rgba -background '#333' -flatten out.png`, or over a
   gradient to judge translucency
   `magick -size WxH gradient:'#d8b070-#5a8fd0' bg.png && magick bg.png \( -size WxH -depth 8
   rgba:in.rgba \) -compose over -composite out.png`.
3. **Read the PNG** (the Read tool renders images) to inspect it; iterate; remove the temp
   test before committing.

Fonts load on this Mac (SF Pro at `/System/Library/Fonts/SFNS.ttf`), so `Hud::load()` works in
tests/tools here. (CI is headless/Linux — keep font-dependent rendering out of the committed
test suite; the existing hud tests only cover pure functions: `embolden_*`,
`format_thousands`, `corner_coverage`, `pie_*`.)

---

## Constraints & norms (don't break these)

- **Hot path:** overlays are rebuilt only on content change, never per frame. Any new
  component/cache must follow that (cache by content signature, like `chip_sig`/`pie_pushed`).
- **DPI:** overlays rasterize at `px * scale_factor`; `scale_factor` updates on
  `ScaleFactorChanged` and `rescale_overlays()` (in `main.rs`) invalidates the per-tick caches
  so they re-rasterize crisply on a monitor move. **Any new overlay cache must be invalidated
  in `rescale_overlays`** or it'll go stale across DPI changes.
- **Privacy:** rendering is RAM-only; no disk writes on the view path (the gallery flag writing
  a PNG is an explicit dev command, fine).
- **Icons:** FA **solid** glyphs, rasterized white + tinted (`icon::rasterize`); vendored in
  `crates/pb-app/icons/` and declared in `icon::assets` (`STOP`, `CLIPBOARD`, `TRASH`,
  `FLOPPY`, `RECYCLE`, `UNDO`, `ROTATE_*`). FA Pro library on this Mac:
  `/Users/jdlien/Documents/FontAwesome/.../svgs/solid/`. Copy a glyph verbatim, add an
  `assets` const.
- **Commit + push per unit of work** (owner's standing instruction). Run
  `cargo clippy -p pb-app --all-targets` + `cargo fmt -p pb-app` before each commit.

## Pointers
- `crates/pb-app/src/hud.rs` — the HUD compositor (everything above).
- `crates/pb-app/src/icon.rs` — icon rasterizer + `assets`.
- `crates/pb-app/src/main.rs` — `push_chip`/`tick_chip`, `push_toast`, `push_pie`,
  `show_overlay`, `show_open_hint`, `rescale_overlays`; the `--hud-gallery` flag would live in
  `main()` near arg parsing.
- `crates/pb-render/src/gpu.rs` — the `set_chip`/`set_toast`/`set_pie`/`set_overlay`/
  `set_message` overlay-layer setters (how a bitmap becomes a positioned quad).
- `crates/pb-ui/src/lib.rs` + `crates/pb-ui/examples/gallery.rs` — the **egui** component
  library + gallery (the parallel precedent — read it for the *shape* of tokens/components/
  gallery, then build the HUD analogue; do **not** try to share code, different rendering).
- `.taskmaster/docs/streaming-playlist-runbook.md` §4 — the scan card's intended behavior.

## Recently landed (context)
The scan status card was just built and refined (commits up to `6fb071c`): fixed-width,
centered, current-path line with elision, centered button, and a genuinely 50%-translucent
border (the `set_toward` insight). Overlay text is now DPI-correct across mixed-density
monitors (`ScaleFactorChanged` handler). That card is the reference quality bar for the
component library.
