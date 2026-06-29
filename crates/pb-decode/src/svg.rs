//! SVG backend: `resvg` / `usvg` / `tiny-skia` (pure Rust).
//!
//! Vector, so we rasterize straight to the on-screen size (crisp at any zoom,
//! unlike a raster downscale): scale the SVG to fit the display box, render, then
//! hand straight RGBA8 to the renderer — no separate decode-to-fit pass.
//! Routed by file extension (SVG is XML text, not magic-sniffable), so
//! [`ImageDecoder::can_decode`] returns false. Built with resvg's default
//! features off, so SVG `<text>` without an embedded font may not render — paths
//! and shapes do.

use std::borrow::Cow;
use std::io::Read;

use flate2::read::GzDecoder;
use resvg::{tiny_skia, usvg};

use crate::{common, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

/// Upper bound on the rasterization scale, so a tiny SVG asked to fill a huge
/// display can't allocate an absurd pixmap.
const MAX_SCALE: f32 = 64.0;

#[derive(Debug, Clone, Copy, Default)]
pub struct SvgDecoder;

impl ImageDecoder for SvgDecoder {
    fn can_decode(&self, _bytes: &[u8]) -> bool {
        false // routed by extension in `decode_image_file`, not by content sniff
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        let opt = usvg::Options::default();
        let bytes = svg_bytes(req.bytes)?;
        let tree = usvg::Tree::from_data(bytes.as_ref(), &opt)
            .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
        let size = tree.size();
        let (sw, sh) = (size.width(), size.height());
        if sw <= 0.0 || sh <= 0.0 {
            return Err(DecodeError::Corrupt("svg has zero size".into()));
        }

        // Rasterize at the display size (vector upscales crisply); natural size
        // when no fit box is given.
        let scale = match req.fit {
            Some(f) => (f.max_width as f32 / sw).min(f.max_height as f32 / sh),
            None => 1.0,
        }
        .clamp(0.01, MAX_SCALE);
        let tw = (sw * scale).round().max(1.0) as u32;
        let th = (sh * scale).round().max(1.0) as u32;

        let mut pixmap = tiny_skia::Pixmap::new(tw, th)
            .ok_or_else(|| DecodeError::Corrupt("svg pixmap alloc failed".into()))?;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );

        // tiny-skia stores premultiplied RGBA; the renderer's alpha blend expects
        // straight RGBA, so demultiply on handoff instead of baking in black.
        let rgba = pixmap.take_demultiplied();
        common::finalize(rgba, tw, th, None, "SVG", None, false)
    }

    fn name(&self) -> &'static str {
        "resvg"
    }
}

/// Cap on an SVGZ's inflated size. SVGZ is gzip, so a tiny file can claim to expand to
/// gigabytes (a decompression bomb); a real SVG is vector text, far smaller than this.
const MAX_SVG_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB

fn svg_bytes(bytes: &[u8]) -> Result<Cow<'_, [u8]>, DecodeError> {
    if bytes.starts_with(&[0x1F, 0x8B]) {
        let mut out = Vec::new();
        // Bound the inflate: read at most one byte past the cap, then reject if we hit it,
        // so a bomb can't exhaust memory before resvg ever parses it.
        GzDecoder::new(bytes)
            .take(MAX_SVG_BYTES + 1)
            .read_to_end(&mut out)
            .map_err(|e| DecodeError::Corrupt(format!("svgz inflate failed: {e}")))?;
        if out.len() as u64 > MAX_SVG_BYTES {
            return Err(DecodeError::Corrupt(
                "svgz inflates beyond the size limit".into(),
            ));
        }
        Ok(Cow::Owned(out))
    } else {
        Ok(Cow::Borrowed(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(bytes: &[u8]) -> DecodeRequest<'_> {
        DecodeRequest {
            bytes,
            fit: None,
            allow_preview: false,
        }
    }

    #[test]
    fn svgz_bytes_are_inflated_before_parse() {
        use std::io::Write;

        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="9" height="7"><rect width="9" height="7" fill="red"/></svg>"#;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(svg).unwrap();
        let bytes = gz.finish().unwrap();

        let img = SvgDecoder.decode(&req(&bytes)).expect("svgz decode");
        assert_eq!((img.orig_width, img.orig_height), (9, 7));
        assert!(img.is_well_formed());
    }

    #[test]
    fn svgz_inflation_is_bounded_against_a_bomb() {
        use std::io::Write;

        // ~70 MiB of zeros compresses to a few KB but exceeds the 64 MiB inflate cap.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..70 {
            gz.write_all(&chunk).unwrap();
        }
        let bomb = gz.finish().unwrap();
        assert!(bomb.len() < 1 << 20, "the bomb is tiny compressed");
        match svg_bytes(&bomb) {
            Err(DecodeError::Corrupt(_)) => {}
            other => panic!("expected a Corrupt rejection, got {other:?}"),
        }

        // A normal small SVGZ still inflates fine (regression guard).
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"<svg/>").unwrap();
        let ok = gz.finish().unwrap();
        assert!(matches!(svg_bytes(&ok), Ok(Cow::Owned(_))));
    }

    #[test]
    fn svg_alpha_is_returned_straight_not_premultiplied() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><rect width="1" height="1" fill="red" fill-opacity="0.5"/></svg>"#;
        let img = SvgDecoder.decode(&req(svg)).expect("svg decode");
        let px = &img.pixels[0..4];
        assert!(px[3] > 0 && px[3] < 255, "alpha = {}", px[3]);
        assert!(px[0] > 240, "red should be straight, got {:?}", px);
    }
}
