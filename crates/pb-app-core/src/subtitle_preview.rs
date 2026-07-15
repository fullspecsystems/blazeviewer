//! The **Settings subtitle preview** (task #90.4): a sample frame, drawn with the real
//! rasterizer and the real placement math.
//!
//! ## Why this exists at all
//!
//! The eight style axes are unjudgeable in the abstract. "Outline 0.3%" and "background
//! 60%" are not decisions anyone can make from a number — you have to *see* the text over
//! a picture, and you cannot see it while you are in a modal Settings window with the film
//! behind you. So Settings draws its own frame.
//!
//! ## Why it cannot lie
//!
//! It calls **the same [`SubtitleRasterizer`]** the overlay calls, through **the same
//! [`SubtitleStyle::to_params`]**, and positions with **the same [`place`]**. There is no
//! second implementation to drift — which is the whole reason the one-rasterizer decision
//! was made in the first place (see the #90 design doc). If the preview and the film ever
//! disagree, that is a bug in one shared function rather than a difference of opinion
//! between two.
//!
//! Using `place()` is what makes the headline setting legible: the preview has a **real
//! letterbox**, so dragging the vertical offset negative visibly walks the text down into
//! the black bar — the owner's ask, and the thing almost no player gets right. A preview
//! that just centred text on a swatch would show nothing of the feature it exists to
//! demonstrate.

use pb_hud::subtitle::SubtitleRasterizer;

use crate::subtitle::{place, Rect, SubtitleStyle};

/// The sample the preview shows.
///
/// Two lines, because one line cannot show `line_spacing` and cannot show that multi-line
/// cues centre (which was a real bug — cosmic-text defaults to left, and a short line over
/// a long one rendered ragged). Short enough to fit a settings-sized swatch at the largest
/// size the slider allows.
const SAMPLE: &[&str] = &["The quick brown fox", "jumps over the lazy dog"];

/// The picture's aspect inside the preview frame.
///
/// **2.39:1** — the widest common cinema ratio, chosen so there is always a generous
/// letterbox to demonstrate the negative vertical offset against. A 16:9 sample in a 16:9
/// swatch would have no bars at all, and the setting the preview exists to explain would
/// be invisible.
const PICTURE_ASPECT: f32 = 2.39;

/// Render one preview frame: `w * h` RGBA8, top-left origin.
///
/// `letterbox` is the user's *real* letterbox colour, so the bars match what they will
/// actually see behind a film rather than an invented black.
pub fn render_preview(
    raster: &mut SubtitleRasterizer,
    style: &SubtitleStyle,
    w: u32,
    h: u32,
    letterbox: [u8; 3],
) -> Vec<u8> {
    let (w, h) = (w.max(1), h.max(1));
    let viewport = (w as f32, h as f32);
    let video = picture_rect(viewport);
    let mut rgba = backdrop(w, h, video, letterbox);

    let lines: Vec<String> = SAMPLE.iter().map(|s| s.to_string()).collect();
    let params = style.to_params(viewport);
    let Some(bmp) = raster.render(&lines, &params) else {
        return rgba; // nothing to draw is a normal answer (an empty sample can't happen)
    };
    // The real placement math, so the vertical offset means here exactly what it means on
    // a film. `controls_h` is 0: Settings has no transport bar.
    let at = place(viewport, video, (bmp.w as f32, bmp.h as f32), style, 0.0);
    blend(&mut rgba, w, h, &bmp.rgba, bmp.w, bmp.h, at.x, at.y);
    rgba
}

/// Where the "film" sits in the preview frame: [`PICTURE_ASPECT`], centred, letterboxed.
fn picture_rect(viewport: (f32, f32)) -> Rect {
    let (vw, vh) = viewport;
    // Fit the widest box that still leaves bars; if the swatch is even wider than the
    // picture, pillarbox instead so the geometry stays honest rather than overflowing.
    let ph = (vw / PICTURE_ASPECT).min(vh);
    let pw = ph * PICTURE_ASPECT;
    Rect {
        x: (vw - pw) / 2.0,
        y: (vh - ph) / 2.0,
        w: pw,
        h: ph,
    }
}

/// The frame behind the text: letterbox bars + a stand-in picture.
///
/// The picture is a **horizontal light-to-dark gradient with a bright band**, not a flat
/// colour, and that is the entire point: white text on flat grey tells you nothing about
/// legibility. Text that crosses from dark to light is exactly where an outline earns its
/// keep or fails to, so the preview shows you the case you are actually tuning for.
fn backdrop(w: u32, h: u32, video: Rect, letterbox: [u8; 3]) -> Vec<u8> {
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    for y in 0..h {
        for x in 0..w {
            let i = ((y as usize) * (w as usize) + x as usize) * 4;
            let (fy, fx) = (y as f32 + 0.5, x as f32 + 0.5);
            let inside =
                fx >= video.x && fx < video.x + video.w && fy >= video.y && fy < video.y + video.h;
            let px = if inside {
                let u = ((fx - video.x) / video.w).clamp(0.0, 1.0);
                let v = ((fy - video.y) / video.h).clamp(0.0, 1.0);
                picture_px(u, v)
            } else {
                letterbox
            };
            rgba[i] = px[0];
            rgba[i + 1] = px[1];
            rgba[i + 2] = px[2];
            rgba[i + 3] = 255;
        }
    }
    rgba
}

/// The stand-in picture at normalized `(u, v)`.
///
/// A cool dark left, a warm bright band right-of-centre, and a vignette toward the bottom
/// where subtitles actually land — so the sample text crosses a hard case rather than
/// sitting on a colour chosen to flatter it.
fn picture_px(u: f32, v: f32) -> [u8; 3] {
    // Base: dark slate → light warm grey, left to right.
    let t = u * u * (3.0 - 2.0 * u); // smoothstep: a gentler ramp than linear
    let base = [
        lerp(28.0, 205.0, t),
        lerp(34.0, 198.0, t),
        lerp(46.0, 178.0, t),
    ];
    // A bright band, so there is a genuinely hostile patch for white text.
    let band = (1.0 - ((u - 0.72) / 0.16).abs()).clamp(0.0, 1.0);
    let lit = [
        base[0] + band * 46.0,
        base[1] + band * 44.0,
        base[2] + band * 30.0,
    ];
    // Darken toward the very bottom — the picture edge the text is anchored to.
    let vign = 1.0 - 0.28 * ((v - 0.72) / 0.28).clamp(0.0, 1.0);
    [
        (lit[0] * vign).clamp(0.0, 255.0) as u8,
        (lit[1] * vign).clamp(0.0, 255.0) as u8,
        (lit[2] * vign).clamp(0.0, 255.0) as u8,
    ]
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Source-over composite of a premultiplied-by-alpha-at-blend RGBA sprite onto the frame.
///
/// Clipped rather than clamped: a block that does not fit (an absurd size against a small
/// swatch) shows the part that fits instead of panicking or wrapping.
#[allow(clippy::too_many_arguments)]
fn blend(dst: &mut [u8], dw: u32, dh: u32, src: &[u8], sw: u32, sh: u32, at_x: f32, at_y: f32) {
    let (ox, oy) = (at_x.round() as i64, at_y.round() as i64);
    for sy in 0..sh as i64 {
        let dy = oy + sy;
        if dy < 0 || dy >= dh as i64 {
            continue;
        }
        for sx in 0..sw as i64 {
            let dx = ox + sx;
            if dx < 0 || dx >= dw as i64 {
                continue;
            }
            let si = ((sy as usize) * (sw as usize) + sx as usize) * 4;
            let a = src[si + 3] as u32;
            if a == 0 {
                continue;
            }
            let di = ((dy as usize) * (dw as usize) + dx as usize) * 4;
            for c in 0..3 {
                let s = src[si + c] as u32;
                let d = dst[di + c] as u32;
                // Straight (non-premultiplied) source-over, rounded.
                dst[di + c] = ((s * a + d * (255 - a) + 127) / 255) as u8;
            }
            dst[di + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(rgba: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y as usize) * (w as usize) + x as usize) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    #[test]
    fn the_frame_is_the_requested_size_and_fully_opaque() {
        let mut r = SubtitleRasterizer::new();
        let rgba = render_preview(&mut r, &SubtitleStyle::default(), 400, 160, [10, 10, 10]);
        assert_eq!(rgba.len(), 400 * 160 * 4);
        assert!(rgba.chunks_exact(4).all(|p| p[3] == 255), "no holes");
    }

    /// The letterbox is real, in the user's own colour — which is what makes the negative
    /// vertical offset judgeable at all.
    #[test]
    fn the_bars_are_the_users_letterbox_colour() {
        let mut r = SubtitleRasterizer::new();
        let (w, h) = (400u32, 300u32);
        let rgba = render_preview(&mut r, &SubtitleStyle::default(), w, h, [7, 9, 11]);
        // 400/2.39 ≈ 167 tall, so the top ~66 rows are bar.
        assert_eq!(px(&rgba, w, 200, 2), [7, 9, 11, 255], "top bar");
        assert_eq!(px(&rgba, w, 200, h - 3), [7, 9, 11, 255], "bottom bar");
        // ...and the middle is picture, not bar.
        assert_ne!(px(&rgba, w, 200, h / 2), [7, 9, 11, 255], "picture");
    }

    /// The picture is a gradient, not a flat fill — the reason being that white text on
    /// flat grey teaches you nothing about whether your outline is enough.
    #[test]
    fn the_picture_is_a_gradient() {
        let mut r = SubtitleRasterizer::new();
        let (w, h) = (400u32, 300u32);
        let rgba = render_preview(&mut r, &SubtitleStyle::default(), w, h, [0, 0, 0]);
        let left = px(&rgba, w, 20, h / 2);
        let right = px(&rgba, w, w - 20, h / 2);
        assert!(
            (right[0] as i32 - left[0] as i32) > 60,
            "left {left:?} must be much darker than right {right:?}"
        );
    }

    /// The preview actually draws the text — the whole point. Compared against the same
    /// frame with a fully transparent colour, which must differ.
    #[test]
    fn the_sample_text_is_drawn() {
        let mut r = SubtitleRasterizer::new();
        let (w, h) = (500u32, 220u32);
        let shown = render_preview(&mut r, &SubtitleStyle::default(), w, h, [0, 0, 0]);
        let invisible = SubtitleStyle {
            color: [255, 255, 255, 0],
            outline_pct: 0.0,
            ..Default::default()
        };
        let blank = render_preview(&mut r, &invisible, w, h, [0, 0, 0]);
        assert_ne!(shown, blank, "the sample must actually render");
    }

    /// THE headline setting, pinned: a negative offset walks the text down into the
    /// letterbox. If this stops being true, the preview stops demonstrating the one
    /// feature the owner singled out.
    #[test]
    fn a_negative_offset_moves_the_text_into_the_letterbox() {
        let mut r = SubtitleRasterizer::new();
        let (w, h) = (500u32, 380u32);
        let video = picture_rect((w as f32, h as f32));

        let rows_lit = |style: &SubtitleStyle, r: &mut SubtitleRasterizer| -> usize {
            let rgba = render_preview(r, style, w, h, [0, 0, 0]);
            // Count bar rows that are no longer pure bar → the text is in the bar.
            (video.bottom().ceil() as u32..h)
                .filter(|&y| (0..w).any(|x| px(&rgba, w, x, y) != [0, 0, 0, 255]))
                .count()
        };

        let inside = SubtitleStyle {
            vertical_offset_pct: 0.05, // up into the picture
            ..Default::default()
        };
        assert_eq!(rows_lit(&inside, &mut r), 0, "must not touch the bar");

        let below = SubtitleStyle {
            vertical_offset_pct: -0.2, // down into the bar
            ..Default::default()
        };
        assert!(
            rows_lit(&below, &mut r) > 0,
            "a negative offset must reach the letterbox"
        );
    }

    /// An absurd size against a small swatch clips instead of panicking. The style is
    /// clamped in practice, but the preview must survive a draft mid-drag regardless.
    #[test]
    fn an_oversized_block_clips_rather_than_panicking() {
        let mut r = SubtitleRasterizer::new();
        let huge = SubtitleStyle {
            size_pct: 0.25, // the clamp ceiling
            ..Default::default()
        };
        let rgba = render_preview(&mut r, &huge, 80, 40, [0, 0, 0]);
        assert_eq!(rgba.len(), 80 * 40 * 4);
    }

    #[test]
    fn a_degenerate_size_does_not_panic() {
        let mut r = SubtitleRasterizer::new();
        assert_eq!(
            render_preview(&mut r, &SubtitleStyle::default(), 0, 0, [0, 0, 0]).len(),
            4,
            "clamped up to 1x1"
        );
    }

    /// Dump preview frames to look at. Not an assertion — the two ASS defects this
    /// project just shipped were both found by eye, not by a test.
    /// `PB_PREVIEW_OUT=/tmp/x cargo test -p pb-app-core --lib -- --ignored --nocapture dump_preview`
    #[test]
    #[ignore = "diagnostic: set PB_PREVIEW_OUT to a directory"]
    fn dump_preview() {
        let Ok(dir) = std::env::var("PB_PREVIEW_OUT") else {
            return;
        };
        let mut r = SubtitleRasterizer::new();
        // 16:9 — a display. A 2.39:1 picture inside it letterboxes, which is the case
        // the preview exists to show. (A swatch WIDER than 2.39 pillarboxes instead and
        // has no bars at all — which is exactly what a first dump at 760x300 showed.)
        let (w, h) = (640u32, 360u32);

        let mut shots: Vec<(&str, SubtitleStyle)> = Vec::new();
        shots.push(("default", SubtitleStyle::default()));

        shots.push((
            "in-the-letterbox",
            SubtitleStyle {
                vertical_offset_pct: -0.16,
                ..Default::default()
            },
        ));

        shots.push((
            "background",
            SubtitleStyle {
                background: [0, 0, 0, 153],
                outline_pct: 0.0,
                ..Default::default()
            },
        ));

        shots.push((
            "shadow",
            SubtitleStyle {
                shadow: Some(crate::subtitle::Shadow::default()),
                ..Default::default()
            },
        ));

        shots.push((
            "big-yellow",
            SubtitleStyle {
                size_pct: 0.10,
                color: [255, 235, 59, 255],
                ..Default::default()
            },
        ));

        for (name, style) in shots {
            let rgba = render_preview(&mut r, &style, w, h, [12, 12, 14]);
            let path = format!("{dir}/preview-{name}.png");
            let f = std::fs::File::create(&path).unwrap();
            let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&rgba).unwrap();
            eprintln!("wrote {path}");
        }
    }
}
