# pb-hud — overlay compositing & the toast-icon workflow (crate-local context)

Auto-loads when working in `crates/pb-hud/`. Root `CLAUDE.md` carries the style
rule (Font Awesome **solid**, house style) and the FA Pro licensing constraint.

## How the compositor works

Overlay panels (`hud.rs`) composite white outlined text **and optional icons**
into one software RGBA8 pill, drawn as a single alpha-blended quad — rebuilt only
on change, never per frame (off the photo hot path). Icons are FA `solid` SVGs: a
single `currentColor` path tinted white, with the text's black-outline pass for
legibility. They're vendored into `icons/` and rasterized via the same
`resvg`/`usvg`/`tiny-skia` stack `pb-decode` uses (`icon::rasterize`). Text is
`fontdue`; subtitles have their own module (`subtitle.rs`).

> **Style decision (2026-06-28, owner):** we tried `duotone` first but switched to
> **solid** — "boring but reliable and effective." Duotone's 40%-opacity secondary
> layer muddied at toast size. Use **solid** for any new icon; don't reintroduce
> duotone without a reason.

## To add a toast icon

1. Find it in the local FA library:
   `D:\Media\fontawesome-pro-plus-7.3.0-web\svgs\solid\<name>.svg`
   (`ls svgs/solid | grep <kw>` to search; always the `solid` weight).
2. Copy it **verbatim** into `crates/pb-hud/icons/<name>.svg`.
3. Add `pub const <NAME>: &str = include_str!("../icons/<name>.svg");` to
   `icon::assets` (`src/icon.rs`).
4. Show it: `show_toast_icon(msg, Some(icon::assets::<NAME>), ..)` for icon+text, or
   `show_toast_icon("", Some(..), ..)` for an icon-only square pill (e.g. rotate).

**Licensing:** FA **Pro** assets are licensed to the owner but **not
redistributable**. The repo is **private**, so vendoring the SVGs is in-bounds. If
it ever goes public: git-ignore `icons/` and load from the local FA path at build,
or swap to the free-tier solid set (most of these icons are in FA Free). (Privacy
task #2 is unaffected — the SVGs are compile-time assets, not a viewing trace.)
