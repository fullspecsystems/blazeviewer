//! The info-panel text layer.
//!
//! Rasterizes a one-line info string into a translucent-black panel with white
//! text, using the OS UI font (Segoe UI on Windows, loaded at runtime — not
//! bundled). The result is an RGBA8 bitmap the renderer draws as a single
//! alpha-blended quad, so it's rebuilt only when the info changes, never per
//! frame. Reusable later for the help overlay and command-feedback toast.

use std::path::PathBuf;

pub struct Hud {
    font: fontdue::Font,
}

impl Hud {
    /// Load the system UI font. Returns `None` if none is found, in which case
    /// the overlay is simply disabled (no crash).
    pub fn load() -> Option<Hud> {
        let bytes = system_font()?;
        let font = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()?;
        Some(Hud { font })
    }

    /// Rasterize `text` into a 40%-black panel with white text, at font size `px`
    /// and `pad` px of padding around the text. Returns `(rgba, width, height)`.
    pub fn render_panel(&self, text: &str, px: f32, pad: u32) -> Option<(Vec<u8>, u32, u32)> {
        let lm = self.font.horizontal_line_metrics(px)?;
        let ascent = lm.ascent;
        let line_h = (lm.ascent - lm.descent).ceil().max(1.0) as u32;

        // Rasterize each glyph and measure the line width.
        let mut glyphs = Vec::new();
        let mut pen = 0.0f32;
        for ch in text.chars() {
            let (m, bitmap) = self.font.rasterize(ch, px);
            glyphs.push((m, bitmap, pen));
            pen += m.advance_width;
        }
        let text_w = pen.ceil().max(1.0) as u32;

        let pw = text_w + 2 * pad;
        let ph = line_h + 2 * pad;
        let mut panel = vec![0u8; (pw as usize) * (ph as usize) * 4];
        for px4 in panel.chunks_exact_mut(4) {
            px4.copy_from_slice(&[0, 0, 0, 102]); // 40% black background
        }

        let baseline = pad as f32 + ascent;
        for (m, bitmap, gx) in &glyphs {
            if m.width == 0 || m.height == 0 {
                continue; // e.g. a space — advance only, no pixels
            }
            let x0 = (pad as f32 + gx + m.xmin as f32).round() as i32;
            let y0 = (baseline - m.ymin as f32 - m.height as f32).round() as i32;
            for row in 0..m.height {
                for col in 0..m.width {
                    let cov = bitmap[row * m.width + col];
                    if cov == 0 {
                        continue;
                    }
                    let x = x0 + col as i32;
                    let y = y0 + row as i32;
                    if x < 0 || y < 0 || x >= pw as i32 || y >= ph as i32 {
                        continue;
                    }
                    let idx = ((y as u32 * pw + x as u32) * 4) as usize;
                    composite_white_over(&mut panel[idx..idx + 4], cov);
                }
            }
        }
        Some((panel, pw, ph))
    }
}

/// Composite white text (coverage `cov`, 0..=255) over the existing
/// straight-alpha pixel.
fn composite_white_over(dst: &mut [u8], cov: u8) {
    let a_src = cov as f32 / 255.0;
    let rgb_d = dst[0] as f32;
    let a_d = dst[3] as f32 / 255.0;
    let a_out = a_src + a_d * (1.0 - a_src);
    if a_out <= 0.0 {
        return;
    }
    let rgb_out = (255.0 * a_src + rgb_d * a_d * (1.0 - a_src)) / a_out;
    let v = rgb_out.round().clamp(0.0, 255.0) as u8;
    dst[0] = v;
    dst[1] = v;
    dst[2] = v;
    dst[3] = (a_out * 255.0).round().clamp(0.0, 255.0) as u8;
}

fn system_font() -> Option<Vec<u8>> {
    candidate_font_paths()
        .into_iter()
        .find_map(|p| std::fs::read(p).ok())
}

fn candidate_font_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(windows)]
    {
        let windir = std::env::var("WINDIR")
            .or_else(|_| std::env::var("SystemRoot"))
            .unwrap_or_else(|_| "C:\\Windows".to_string());
        let fonts = PathBuf::from(windir).join("Fonts");
        v.push(fonts.join("segoeui.ttf"));
        v.push(fonts.join("arial.ttf"));
    }
    #[cfg(target_os = "macos")]
    {
        v.push(PathBuf::from("/Library/Fonts/Arial.ttf"));
        v.push(PathBuf::from("/System/Library/Fonts/Supplemental/Arial.ttf"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        v.push(PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"));
        v.push(PathBuf::from("/usr/share/fonts/TTF/DejaVuSans.ttf"));
    }
    v
}
