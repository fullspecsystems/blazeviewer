//! The **HUD component gallery** — a one-shot `--hud-gallery <out.png>` dev command that
//! rasterizes every on-image overlay onto a single PNG sheet, so the HUD layer can be
//! previewed and dialed in *without launching the viewer*. It's the CPU-compositor analogue
//! of `cargo run -p pb-ui --example gallery` (which can only show the egui dialog chrome).
//!
//! Two regions: a **catalog** of captioned tiles (each component in isolation) over a warm→cool
//! gradient so translucency reads, and a **real-layout composite** — the overlays placed where
//! they actually appear on screen (info pill top-left, scan card + loading pie top-right, toast
//! bottom-center) over a photo-ish gradient, to judge how the layers coexist.
//!
//! Everything is laid out in *logical* px and scaled by [`SCALE`] (acting as a Retina
//! `scale_factor`), so the sheet's proportions match a real high-DPI display while staying
//! crisp. The PNG write is an explicit dev command, so the no-trace guarantee (no disk writes
//! on the *view* path) is untouched.

use crate::hud::{self, Hud, Row};
use crate::icon::assets;
use resvg::tiny_skia;
use std::path::Path;

/// Render scale — overlays rasterize at `base_px * SCALE`, mirroring how the live HUD scales
/// by the display factor. 2.0 ≈ a Retina render: crisp, and proportioned like a 2× monitor.
const SCALE: f32 = 2.0;

/// Logical → device px (layout coordinates and sizes).
fn s(logical: f32) -> i32 {
    (logical * SCALE).round() as i32
}
/// Logical → device px (a text-height `px` passed to the HUD).
fn sp(logical: f32) -> f32 {
    logical * SCALE
}

/// Render the gallery to `out` as a PNG. Errors (no system font, encode/write failure) come
/// back as a message for `main` to print.
pub fn write_sheet(out: &Path) -> Result<(), String> {
    let hud = Hud::load().ok_or("no system UI font is available to render the HUD")?;
    let sheet = build_sheet(&hud);
    let png = encode_png(&sheet)?;
    std::fs::write(out, png).map_err(|e| format!("writing {}: {e}", out.display()))?;
    Ok(())
}

/// One catalog entry: a caption above a component bitmap.
struct Tile {
    caption: Bitmap,
    body: Bitmap,
}
type Bitmap = (Vec<u8>, u32, u32);

impl Tile {
    fn width(&self) -> i32 {
        self.caption.1.max(self.body.1) as i32
    }
    fn height(&self, cap_gap: i32) -> i32 {
        self.caption.2 as i32 + cap_gap + self.body.2 as i32
    }
}

/// Compose the full sheet: title, the captioned catalog (flow-packed), then the composite.
fn build_sheet(hud: &Hud) -> Sheet {
    let margin = s(28.0);
    let content_w = s(700.0); // the composite's logical width; the catalog packs within it
    let cap_gap = s(6.0);
    let h_gap = s(28.0); // between tiles in a row
    let v_gap = s(26.0); // between tile rows
    let sec_gap = s(10.0); // below a section label
    let block_gap = s(26.0); // between major blocks

    let title = text(hud, "PhotoBlaze \u{2014} HUD overlay components", sp(24.0));
    let sec_catalog = text(hud, "Components", sp(17.0));
    let sec_composite = text(
        hud,
        "Real-layout composite (overlays over a photo)",
        sp(17.0),
    );
    let tiles = build_tiles(hud);
    let composite = build_composite(hud);

    // Measure: walk the same layout once to learn the total height before allocating.
    let mut y = margin;
    y += title.2 as i32 + block_gap;
    y += sec_catalog.2 as i32 + sec_gap;
    let (positions, tiles_bottom) = pack(&tiles, content_w, margin, y, h_gap, v_gap, cap_gap);
    y = tiles_bottom + block_gap;
    y += sec_composite.2 as i32 + sec_gap;
    let composite_y = y;
    y += composite.h + margin;

    let sheet_w = margin * 2 + content_w;
    let sheet_h = y;
    // A warm→cool diagonal gradient, so the translucent panels show it through (the handoff's
    // chosen backdrop for judging opacity).
    let mut sheet = Sheet::gradient(
        sheet_w,
        sheet_h,
        [0xd8, 0xb0, 0x70],
        [0x5a, 0x8f, 0xd0],
        true,
    );

    // Paint: title, section label, the packed tiles, then the composite block.
    let mut yy = margin;
    sheet.over(&title, margin, yy);
    yy += title.2 as i32 + block_gap;
    sheet.over(&sec_catalog, margin, yy);
    for (i, x, ty) in &positions {
        let t = &tiles[*i];
        sheet.over(&t.caption, *x, *ty);
        sheet.over(&t.body, *x, *ty + t.caption.2 as i32 + cap_gap);
    }
    let sec_composite_y = tiles_bottom + block_gap;
    sheet.over(&sec_composite, margin, sec_composite_y);
    sheet.copy_from(&composite, margin, composite_y);
    sheet
}

/// Build the captioned catalog tiles — one per HUD component / variant.
fn build_tiles(hud: &Hud) -> Vec<Tile> {
    let bg = hud::BG;
    let info_pad = s(7.0) as u32;
    let toast_pad = s(12.0) as u32;
    // Match the scan card's button text size (heading 15 × CARD_SUB), so swatches read true.
    let btn_px = sp(15.0 * hud::tokens::CARD_SUB);
    let mut tiles = Vec::new();

    // Scan status card (the reference component) + its standalone Cancel button.
    if let Some((r, w, h, _)) = hud.render_scan_card(
        "Scanning \u{201c}Vacation 2024\u{201d}",
        "\u{2026}/Photos/Vacation 2024/Day 3 \u{2014} Coast",
        "8,230 images found",
        "Cancel Scan",
        assets::STOP,
        sp(15.0),
        s(320.0) as u32,
        bg,
        false,
    ) {
        tiles.push(tile(hud, "Scan status card", (r, w, h)));
    }

    // Info / EXIF / help panels.
    if let Some(b) = hud.render_panel(
        "DSC_0421.jpg \u{00b7} 6000\u{00d7}4000 \u{00b7} JPEG",
        sp(15.0),
        info_pad,
        bg,
    ) {
        tiles.push(tile(hud, "Info pill (i)", b));
    }
    if let Some(b) = hud.render_table(&exif_rows(), sp(15.0), info_pad, bg) {
        tiles.push(tile(hud, "EXIF panel (Shift+I)", b));
    }
    if let Some(b) = hud.render_table(&help_rows(), sp(16.0), info_pad, hud::BG) {
        tiles.push(tile(hud, "Help overlay (?)", b));
    }

    // Toasts: icon + text, status text, and an icon-only square pill.
    if let Some(b) = hud.render_panel_icon(
        "Copied to clipboard",
        sp(20.0),
        toast_pad,
        Some(assets::CLIPBOARD),
        bg,
    ) {
        tiles.push(tile(hud, "Toast \u{2014} command + icon", b));
    }
    if let Some(b) = hud.render_panel("Recursive scan: on", sp(20.0), toast_pad, bg) {
        tiles.push(tile(hud, "Toast \u{2014} status text", b));
    }
    if let Some(b) = hud.render_panel_icon("", sp(20.0), toast_pad, Some(assets::ROTATE_RIGHT), bg)
    {
        tiles.push(tile(hud, "Toast \u{2014} icon only (rotate)", b));
    }

    // Buttons (the reusable primitive) across a few icon/label variants + a text-only one.
    for (label, icon, cap) in [
        (
            "Cancel Scan",
            Some(assets::STOP),
            "Button \u{2014} Cancel Scan",
        ),
        ("Copy", Some(assets::CLIPBOARD), "Button \u{2014} Copy"),
        ("Delete", Some(assets::TRASH), "Button \u{2014} Delete"),
        (
            "Save Rotation",
            Some(assets::FLOPPY),
            "Button \u{2014} Save",
        ),
        ("OK", None, "Button \u{2014} text only"),
    ] {
        if let Some(b) = hud.render_button(label, icon, btn_px, bg, false) {
            tiles.push(tile(hud, cap, b));
        }
    }

    // Hover state — the same button at rest and lit (fill + border lifted), side by side, to
    // tune the BUTTON_*_HOVER alphas against the resting look.
    if let Some(b) = hud.render_button("Cancel Scan", Some(assets::STOP), btn_px, bg, false) {
        tiles.push(tile(hud, "Button \u{2014} rest", b));
    }
    if let Some(b) = hud.render_button("Cancel Scan", Some(assets::STOP), btn_px, bg, true) {
        tiles.push(tile(hud, "Button \u{2014} hover (lit)", b));
    }

    // Loading pie at a few fills.
    for (progress, cap) in [
        (0.0_f32, "Loading pie \u{2014} 0%"),
        (0.42, "Loading pie \u{2014} ~40%"),
        (0.93, "Loading pie \u{2014} ~90% (cap)"),
    ] {
        let b = hud::render_pie(s(46.0) as u32, progress, 0.0);
        tiles.push(tile(hud, cap, b));
    }

    // Empty-state hint.
    if let Some(b) = hud.render_centered(
        &["Press O to open a file", "or Shift+O to open a folder"],
        sp(20.0),
        s(10.0) as u32,
        bg,
    ) {
        tiles.push(tile(hud, "Empty-state hint", b));
    }

    tiles
}

/// Build the real-layout composite: a faux 700×360 (logical) "screen" with the overlays where
/// they live on screen — info pill top-left, loading pie top-center, scan card top-right, a
/// command toast bottom-center.
fn build_composite(hud: &Hud) -> Sheet {
    let w = s(700.0);
    let h = s(360.0);
    let inset = s(18.0);
    // A vertical sky→ground gradient stands in for a photo.
    let mut c = Sheet::gradient(w, h, [0x86, 0xa6, 0xcf], [0x24, 0x2b, 0x20], false);
    let bg = hud::BG;

    if let Some((r, pw, ph)) = hud.render_panel(
        "DSC_0421.jpg \u{00b7} 6000\u{00d7}4000 \u{00b7} JPEG",
        sp(15.0),
        s(7.0) as u32,
        bg,
    ) {
        c.over(&(r, pw, ph), inset, inset);
    }
    if let Some((r, cw, ch, _)) = hud.render_scan_card(
        "Scanning \u{201c}Vacation 2024\u{201d}",
        "\u{2026}/Day 3 \u{2014} Coast",
        "8,230 images found",
        "Cancel Scan",
        assets::STOP,
        sp(15.0),
        s(320.0) as u32,
        bg,
        false,
    ) {
        c.over(&(r, cw, ch), w - inset - cw as i32, inset);
    }
    let pie = hud::render_pie(s(46.0) as u32, 0.62, 0.0);
    c.over(&pie, (w - pie.1 as i32) / 2, inset);
    if let Some((r, tw, th)) = hud.render_panel_icon(
        "Copied to clipboard",
        sp(20.0),
        s(12.0) as u32,
        Some(assets::CLIPBOARD),
        bg,
    ) {
        c.over(&(r, tw, th), (w - tw as i32) / 2, h - inset - th as i32);
    }
    c
}

/// Flow-pack tiles left-to-right within `content_w`, wrapping rows. Returns each tile's
/// `(index, x, y)` plus the bottom y of the last row — pure arithmetic, so the same layout
/// can be measured before the sheet is allocated and then replayed to paint.
fn pack(
    tiles: &[Tile],
    content_w: i32,
    x0: i32,
    y0: i32,
    h_gap: i32,
    v_gap: i32,
    cap_gap: i32,
) -> (Vec<(usize, i32, i32)>, i32) {
    let mut out = Vec::with_capacity(tiles.len());
    let mut x = x0;
    let mut y = y0;
    let mut row_h = 0;
    for (i, t) in tiles.iter().enumerate() {
        let tw = t.width();
        if x > x0 && x + tw > x0 + content_w {
            // wrap
            x = x0;
            y += row_h + v_gap;
            row_h = 0;
        }
        out.push((i, x, y));
        x += tw + h_gap;
        row_h = row_h.max(t.height(cap_gap));
    }
    (out, y + row_h)
}

/// A tile = a small caption (white outlined text on transparent) above the component bitmap.
fn tile(hud: &Hud, caption: &str, body: Bitmap) -> Tile {
    Tile {
        caption: text(hud, caption, sp(12.0)),
        body,
    }
}

/// White outlined text on a transparent background — the HUD's own text rasterizer with no
/// pill behind it (used for captions and section labels).
fn text(hud: &Hud, s: &str, px: f32) -> Bitmap {
    hud.render_panel(s, px, (2.0 * SCALE).round() as u32, [0, 0, 0, 0])
        .unwrap_or((vec![0; 4], 1, 1))
}

fn exif_rows() -> Vec<Row> {
    vec![
        Row::Span {
            text: "DSC_0421.jpg".into(),
            bold: true,
        },
        Row::Span {
            text: "/Photos/Vacation 2024/Day 3".into(),
            bold: false,
        },
        pair("Camera", "SONY ILCE-7M4"),
        pair("Lens", "FE 24-70mm F2.8 GM II"),
        pair("Exposure", "1/500s \u{00b7} f/4.0 \u{00b7} ISO 200"),
        pair("Focal length", "53 mm"),
        pair("Dimensions", "6000 \u{00d7} 4000 \u{00b7} 14.2 MB"),
    ]
}

fn help_rows() -> Vec<Row> {
    vec![
        Row::Span {
            text: "Keyboard".into(),
            bold: true,
        },
        pair("Space / \u{2192}", "Next photo"),
        pair("Backspace / \u{2190}", "Previous photo"),
        pair("Enter", "Random photo"),
        pair("i", "Info overlay"),
        pair("Esc", "Quit"),
    ]
}

fn pair(label: &str, value: &str) -> Row {
    Row::Pair {
        label: label.into(),
        value: value.into(),
    }
}

/// An opaque RGBA8 sheet the gallery composites onto (straight alpha; alpha stays 255 since the
/// background is opaque, so it's directly valid as the premultiplied buffer `tiny_skia` wants).
struct Sheet {
    px: Vec<u8>,
    w: i32,
    h: i32,
}

impl Sheet {
    /// A `w×h` opaque sheet filled with a two-color gradient (`diagonal`, else top→bottom).
    fn gradient(w: i32, h: i32, c0: [u8; 3], c1: [u8; 3], diagonal: bool) -> Sheet {
        let mut px = vec![255u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let t = if diagonal {
                    (x as f32 / w as f32 + y as f32 / h as f32) * 0.5
                } else {
                    y as f32 / h as f32
                };
                let i = ((y * w + x) * 4) as usize;
                px[i] = lerp(c0[0], c1[0], t);
                px[i + 1] = lerp(c0[1], c1[1], t);
                px[i + 2] = lerp(c0[2], c1[2], t);
            }
        }
        Sheet { px, w, h }
    }

    /// Composite a straight-alpha tile over the sheet at `(x, y)`. The sheet is opaque, so the
    /// result stays opaque (`over` with `dst_a = 1`).
    fn over(&mut self, tile: &Bitmap, x: i32, y: i32) {
        let (rgba, tw, th) = tile;
        let (tw, th) = (*tw as i32, *th as i32);
        for ry in 0..th {
            let dy = y + ry;
            if dy < 0 || dy >= self.h {
                continue;
            }
            for rx in 0..tw {
                let dx = x + rx;
                if dx < 0 || dx >= self.w {
                    continue;
                }
                let si = ((ry * tw + rx) * 4) as usize;
                let a = rgba[si + 3] as f32 / 255.0;
                if a <= 0.0 {
                    continue;
                }
                let di = ((dy * self.w + dx) * 4) as usize;
                self.px[di] = blend(self.px[di], rgba[si], a);
                self.px[di + 1] = blend(self.px[di + 1], rgba[si + 1], a);
                self.px[di + 2] = blend(self.px[di + 2], rgba[si + 2], a);
            }
        }
    }

    /// Blit another opaque sheet's pixels in (a straight copy — the composite region).
    fn copy_from(&mut self, other: &Sheet, x: i32, y: i32) {
        for ry in 0..other.h {
            let dy = y + ry;
            if dy < 0 || dy >= self.h {
                continue;
            }
            for rx in 0..other.w {
                let dx = x + rx;
                if dx < 0 || dx >= self.w {
                    continue;
                }
                let si = ((ry * other.w + rx) * 4) as usize;
                let di = ((dy * self.w + dx) * 4) as usize;
                self.px[di..di + 4].copy_from_slice(&other.px[si..si + 4]);
            }
        }
    }
}

/// `dst*(1-a) + src*a`, rounded — the over-blend for one opaque channel.
fn blend(dst: u8, src: u8, a: f32) -> u8 {
    (src as f32 * a + dst as f32 * (1.0 - a))
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Linear interpolate two `u8` channel values by `t ∈ [0,1]`.
fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 * (1.0 - t) + b as f32 * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Encode the opaque sheet to PNG bytes via `tiny_skia` (a direct dep through `resvg`).
fn encode_png(sheet: &Sheet) -> Result<Vec<u8>, String> {
    let size =
        tiny_skia::IntSize::from_wh(sheet.w as u32, sheet.h as u32).ok_or("invalid sheet size")?;
    let pixmap =
        tiny_skia::Pixmap::from_vec(sheet.px.clone(), size).ok_or("could not build pixmap")?;
    pixmap.encode_png().map_err(|e| format!("PNG encode: {e}"))
}
