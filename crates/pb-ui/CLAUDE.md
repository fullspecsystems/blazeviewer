# pb-ui — design-system inventory & how to extend it (crate-local context)

Auto-loads when working in `crates/pb-ui/`. The root `CLAUDE.md` carries the
rules (use pb-ui components, never hand-roll; conventions; the resolved
brand-first accent). This file is the inventory + the extension workflows.
`lib.rs`/`icon.rs` are the source of truth over both.

## What's there (`src/lib.rs`)

- **Tokens:** `SPACE_1..6` (4px scale), `GAP` (**the** standard gap — between rows, between
  cards, and the dialog button gap/inset; one knob), `RADIUS_CONTROL`/`RADIUS_CARD`,
  `CONTROL_H` (32px — set once, kills "every control a different size"), `FIELD_MARGIN`,
  `CARD_WRAP_WIDTH`, `PAGE_MARGIN`, `BUTTON_W`, `TAB_GAP`/`TAB_INDICATOR_*`,
  `SLIDER_VALUE_W`, `SECTION_GAP`, and `Palette` (named color roles, light + dark).
- **Accent API:** `BRAND_ACCENT` (#FF4915), `set_accent`/`accent()` (process-wide,
  lock-free), `ensure_legible` (fallback-to-brand guard), `text_on_accent`.
- **Theme:** `install_fonts` (native Segoe UI) once per dialog ctx; `apply_style(ctx,
  dark)` each frame (cheap; survives egui's own theme bookkeeping); `apply_to_ui(ui,
  dark)` to scope one region (e.g. a gallery column, or a **combo popup** — egui draws
  popup *contents* with the global ctx style, so re-assert it inside `show_ui`).
- **Components:** `group_card` (the **grouped-settings** card: a semibold heading inside
  the card + `card_row`s that **auto-space by `GAP`** — no dividers; related settings share
  one card, so a page is a few cards not one-per-setting), `card` (single), `card_row`
  (responsive: control on the right when wide, stacked under the header below
  `CARD_WRAP_WIDTH`), `toggle` / `toggle_with_label`, `page_title` / `section_label` (type ramp:
  page title 30 / section 17 — both semibold via the bundled Segoe UI Semibold face / card
  title 14.5 / description 12.5), `primary_button` / `secondary_button` / `danger_button` /
  `placeholder_button`, `text_field`, `slider` / `slider_stepped` (stable-width value box +
  solid-accent fill — no jitter), `tab_bar` (the Settings tabs), `progress_bar` (the
  Loading/Scanning views), `icon_sized`.
- **Icons (`src/icon.rs`):** Font Awesome SVGs **vendored per family**
  (`icons/<family>/<name>.svg`), rasterized to a **white square sprite** and **tinted at
  draw time** — one texture serves every tone and theme (cached in the egui ctx). A
  semantic `Icon` enum (`Lock`, `Warning`, `Trash`, …) names *meaning* not glyph; `Tone`
  (`Neutral`/`Accent`/`Warning`/`Danger`/`Success`) resolves through the `Palette` so it's
  light/dark-correct. Placement helpers — `lead_row` (gutter icon centered on the first
  content line: the dialog body shape) and `inline` — bake the alignment, so there is
  **no per-call nudging or top-clipping** (the old pain). The square render is our own
  `fa-fw` — FA glyphs aren't all square (lock is 384×512), so we center every glyph in a
  square box. **Switch families** by flipping `icon::ACTIVE_FAMILY` (vendor that family
  first). The HUD toasts keep their own CPU-composite rasterizer (`pb-hud/src/icon.rs`);
  only the egui chrome uses `pb-ui::icon`.

## To add a component

Put it in `pb-ui` (drive it from tokens/`Palette`, take a `&mut egui::Ui`, return
the `Response`), add it to the gallery `catalog`, then use it. Don't add UI
primitives to `pb-app`.

## To add an icon

Copy the glyph for each vendored family from the FA library
(`D:\Media\fontawesome-pro-plus-7.3.0-web\svgs\<family>\`) into
`icons/<family>/`, add a variant to `icon::Icon` + a `glyph!` arm, show it in the
gallery's Icons row. (FA **Pro** licensing: see the root `CLAUDE.md` — private
repo only, never redistributable.)
