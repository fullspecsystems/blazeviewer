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
    /// Closed captioning — the Subtitles on (`C`) toast.
    pub const CAPTIONS: &str = include_str!("../icons/closed-captioning.svg");
    /// Closed captioning, slashed — the Subtitles off (`C`) toast.
    pub const CAPTIONS_SLASH: &str = include_str!("../icons/closed-captioning-slash.svg");
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
    /// U-turn arrow — the Undo (`Ctrl+Z` / Edit ▸ Undo) toast.
    pub const UNDO: &str = include_str!("../icons/arrow-u-turn-up-left.svg");
    /// Stop square — the "Cancel Scan" button on the scan status card.
    pub const STOP: &str = include_str!("../icons/stop.svg");
    /// Play triangle — the "Press P to play" hint shown on an animated still (#37).
    pub const PLAY: &str = include_str!("../icons/play.svg");
    /// Live Photo mark (inner circle ringed by dots) — the play hint on a Live Photo
    /// still (#38). Derived from the SF Symbols `livephoto` glyph as a placeholder; to be
    /// replaced with an own-brand variant (owner).
    pub const LIVE_PHOTO: &str = include_str!("../icons/livephoto.svg");
    /// Speaker with sound waves — the "Live Photo audio on" toast (unmute). Mirrors the Mac
    /// keyboard's volume key (#38).
    pub const VOLUME: &str = include_str!("../icons/volume.svg");
    /// Speaker with a slash — the "Live Photo audio muted" toast (mute). Mirrors the Mac
    /// keyboard's mute key (#38).
    pub const VOLUME_SLASH: &str = include_str!("../icons/volume-slash.svg");
    /// Thumbtack — the "Pinned for compare" toast (task #43's flicker compare).
    pub const THUMBTACK: &str = include_str!("../icons/thumbtack.svg");
    /// Thumbtack with a slash — the "Unpinned" toast (compare pin cleared).
    pub const THUMBTACK_SLASH: &str = include_str!("../icons/thumbtack-slash.svg");
    /// Closed folder — a row of the folder-tree overlay (`Shift+F`).
    pub const FOLDER: &str = include_str!("../icons/folder.svg");
    /// Open folder — the folder-tree overlay's current/ancestor rows.
    pub const FOLDER_OPEN: &str = include_str!("../icons/folder-open.svg");
    /// Folder with an up arrow — the folder-tree overlay's "up to the parent" row.
    pub const FOLDER_UP: &str = include_str!("../icons/folder-arrow-up.svg");
    /// Up caret — the folder tree's clickable "n more above" paging marker.
    pub const CARET_UP: &str = include_str!("../icons/caret-up.svg");
    /// Down caret — the folder tree's clickable "n more below" paging marker.
    pub const CARET_DOWN: &str = include_str!("../icons/caret-down.svg");
    // Status icons for the dialogs (lock/warning/trash) now live in the `pb-ui` icon
    // system (white-rasterized + tinted), not here — this set is only the HUD toasts.
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

    /// The vendored SF-derived Live Photo glyph (a many-subpath fill) parses and
    /// rasterizes to a white, near-square mark — guards against a malformed extraction.
    #[test]
    fn live_photo_rasterizes_to_white() {
        let (rgba, w, h) = rasterize(assets::LIVE_PHOTO, 40, [255, 255, 255])
            .expect("livephoto icon should rasterize");
        assert_eq!(h, 40);
        // viewBox is ~1750×1748 → essentially square.
        assert!((w as i32 - 40).abs() <= 2, "near-square width, got {w}");
        let opaque = rgba.chunks_exact(4).filter(|p| p[3] == 255).count();
        assert!(opaque > 0, "the ring + dots should produce opaque pixels");
        for p in rgba.chunks_exact(4).filter(|p| p[3] > 0) {
            assert_eq!([p[0], p[1], p[2]], [255, 255, 255], "currentColor → white");
        }
    }

    #[test]
    fn garbage_svg_returns_none() {
        assert!(rasterize("not an svg", 16, [255, 255, 255]).is_none());
    }
}
