//! Rasterize the vendored Font Awesome **solid** SVG icons into straight-alpha
//! RGBA8 bitmaps for the HUD/toast overlays, reusing the workspace's
//! `resvg`/`usvg`/`tiny_skia` stack (the same one `pb-decode`'s SVG backend uses).
//!
//! A solid icon is a single `<path fill="currentColor">`. We tint `currentColor` to
//! one color (white, to match the overlay text); the outline pass in `hud.rs` gives
//! legibility over photos. (We started with duotone but switched to solid — boring
//! but reliable + crisp at toast size; see CLAUDE.md.) Rasterization happens only
//! when an overlay bitmap is rebuilt (on a command, then cached/faded), never per
//! frame — off the photo hot path.

use resvg::{tiny_skia, usvg};

/// The vendored icon sources (`include_str!` so they ship in the binary — no runtime
/// file dependency, no privacy trace). Font Awesome Pro is licensed to the owner;
/// the repo is private (see the discussion in CLAUDE.md / session notes).
pub mod assets {
    /// Clipboard — the Copy (`Ctrl+C`) toast.
    pub const CLIPBOARD: &str = include_str!("../icons/clipboard.svg");
    /// Clockwise rotate — the Rotate Right (`r`) toast.
    pub const ROTATE_RIGHT: &str = include_str!("../icons/rotate-right.svg");
    /// Counter-clockwise rotate — the Rotate Left (`Shift+R`) toast.
    pub const ROTATE_LEFT: &str = include_str!("../icons/rotate-left.svg");
    /// Floppy disk — the Save Rotation (`Ctrl+S`) success toast.
    pub const FLOPPY: &str = include_str!("../icons/floppy-disk.svg");
    /// Recycle bin — the Delete-to-Recycle-Bin (`Del`, recoverable) toast.
    pub const RECYCLE: &str = include_str!("../icons/bin-recycle.svg");
    /// Trash — the Delete-Permanently (`Shift+Del`) toast.
    pub const TRASH: &str = include_str!("../icons/trash.svg");
}

/// Rasterize `svg` at pixel `height` (width follows the icon's aspect ratio),
/// tinting every `currentColor` to `rgb`. Returns straight-alpha RGBA8
/// `(pixels, w, h)`, or `None` if the SVG can't be parsed / sized.
pub fn rasterize(svg: &str, height: u32, rgb: [u8; 3]) -> Option<(Vec<u8>, u32, u32)> {
    let height = height.max(1);
    // Resolve `currentColor` ourselves (usvg has no document color context here).
    let hex = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
    let src = svg.replace("currentColor", &hex);

    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(src.as_bytes(), &opt).ok()?;
    let size = tree.size();
    let (sw, sh) = (size.width(), size.height());
    if sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    let scale = height as f32 / sh;
    let tw = (sw * scale).round().max(1.0) as u32;

    let mut pixmap = tiny_skia::Pixmap::new(tw, height)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia stores premultiplied RGBA; the overlay canvas blends straight alpha.
    Some((pixmap.take_demultiplied(), tw, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_rasterizes_to_white() {
        let (rgba, w, h) = rasterize(assets::CLIPBOARD, 32, [255, 255, 255])
            .expect("clipboard icon should rasterize");
        assert_eq!(h, 32);
        // viewBox is 384×512 → width is narrower than the height.
        assert!(w > 0 && w < 32, "aspect-correct width, got {w}");
        assert_eq!(rgba.len(), (w as usize) * 32 * 4);

        // The solid fill produces fully-opaque pixels, all tinted white.
        let opaque = rgba.chunks_exact(4).filter(|p| p[3] == 255).count();
        assert!(opaque > 0, "the solid fill should produce opaque pixels");
        for p in rgba.chunks_exact(4).filter(|p| p[3] > 0) {
            assert_eq!([p[0], p[1], p[2]], [255, 255, 255], "currentColor → white");
        }
        // Anti-aliased edges leave some partially-transparent pixels (not a hard mask).
        let partial = rgba
            .chunks_exact(4)
            .filter(|p| p[3] > 0 && p[3] < 255)
            .count();
        assert!(partial > 0, "anti-aliased edges should be semi-transparent");
    }

    #[test]
    fn garbage_svg_returns_none() {
        assert!(rasterize("not an svg", 16, [255, 255, 255]).is_none());
    }
}
