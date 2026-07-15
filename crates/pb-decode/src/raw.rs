//! RAW backend: extract the largest embedded JPEG preview and decode it.
//!
//! A full RAW demosaic is 100×+ the cost of the embedded preview and rarely worth
//! it for a fast viewer (CLAUDE.md), so the MVP shows the camera-baked preview.
//! RAW containers (ARW/NEF/CR2/DNG/… — mostly TIFF, CR3 is ISOBMFF) embed one or
//! more JPEGs; we find every top-level JPEG by properly parsing segment lengths
//! (so a thumbnail nested in another preview's EXIF is skipped, not mistaken for
//! the end), then decode the largest via the tuned zune backend.
//!
//! Routed by file extension, not magic — a RAW's TIFF header is indistinguishable
//! from a plain TIFF's, so [`ImageDecoder::can_decode`] returns false and
//! `decode_image_file` dispatches here by extension.

use crate::{common, DecodeError, DecodeRequest, DecodedImage, ImageDecoder};

/// Extensions handled by the RAW path.
pub fn is_raw_extension(ext: &str) -> bool {
    matches!(
        ext,
        "arw" | "nef" | "cr2" | "cr3" | "dng" | "raf" | "rw2" | "orf" | "srw" | "pef" | "raw"
    )
}

/// An embedded preview at/above this long edge counts as "full size" — use it
/// directly (fast, robust, and avoids demosaicing huge files some pure-Rust RAW
/// decoders choke on). Nikon embeds a near-full-res preview (≈7360) and takes this
/// path; Sony embeds a 1616 thumbnail or a reduced ~3968 preview, both of which
/// fall below and trigger a full demosaic so you can zoom to true sensor
/// resolution (matching the out-of-camera JPEG).
const PREVIEW_FULL_MIN: u32 = 4000;

#[derive(Debug, Clone, Copy, Default)]
pub struct RawPreviewDecoder;

impl ImageDecoder for RawPreviewDecoder {
    fn can_decode(&self, _bytes: &[u8]) -> bool {
        false // routed by extension; a RAW looks like TIFF to a magic sniff
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedImage, DecodeError> {
        // The largest embedded preview, decoded to sensor-orientation pixels.
        let preview = largest_preview(req.bytes);
        let preview_long = preview.as_ref().map_or(0, |(_, w, h)| (*w).max(*h));
        let has_full_preview = preview_long >= PREVIEW_FULL_MIN;

        // Use the camera-baked embedded JPEG directly (fast, ~tens of ms) when EITHER:
        //  - this is a **preview** request (the blaze-by tier): a small thumbnail is
        //    exactly the "instant blurry, sharpen on land" placeholder — demosaicing
        //    here is what jammed scrolling through Sony RAW (~1.4 s each, all workers
        //    stuck); or
        //  - the embedded preview is already full-size (Nikon ≈7360) — demosaic adds
        //    nothing.
        // The 100×+ demosaic is reserved for the **full** decode of a RAW whose only
        // embedded image is a small thumbnail (Sony 1616) — see below.
        if req.allow_preview || has_full_preview {
            if let Some((rgba, w, h)) = preview {
                let orient = common::read_orientation(req.bytes);
                // Refinable (the app will pull the full demosaic on land) only when
                // it's a small thumbnail shown for a preview request.
                let is_preview = req.allow_preview && !has_full_preview;
                return common::finalize_oriented(rgba, w, h, orient, "RAW", req.fit, is_preview);
            }
            // No embedded JPEG at all → fall through to demosaic (no faster option).
        }

        // Full decode, only a small thumbnail embedded (e.g. Sony) → demosaic for true
        // sensor resolution. imagepipe applies orientation itself, so pass upright (1).
        // Keep it only if it actually beats the preview (else the demosaic added
        // nothing).
        if let Some((rgba, w, h)) = demosaic_full(req.bytes) {
            if w.max(h) >= preview_long {
                return common::finalize_oriented(rgba, w, h, 1, "RAW", req.fit, false);
            }
        }

        // Demosaic unavailable/failed/smaller — fall back to the preview.
        if let Some((rgba, w, h)) = preview {
            let orient = common::read_orientation(req.bytes);
            return common::finalize_oriented(rgba, w, h, orient, "RAW", req.fit, true);
        }
        Err(DecodeError::Corrupt(
            "no decodable embedded preview and demosaic failed".into(),
        ))
    }

    fn name(&self) -> &'static str {
        "raw-preview"
    }
}

/// Decode the largest embedded JPEG preview to sensor-orientation RGBA8 (trying
/// largest-first). `None` if there's no decodable embedded JPEG.
fn largest_preview(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut spans = jpeg_spans(bytes);
    spans.sort_by_key(|&(start, end)| std::cmp::Reverse(end - start));
    for (start, end) in spans {
        if let Ok(rgba) = crate::zune::decode_rgba(&bytes[start..end]) {
            return Some(rgba);
        }
    }
    None
}

/// Full demosaic via rawloader + imagepipe → upright RGBA8 (imagepipe applies
/// orientation). Runs on a large-stack thread: some RAW decoders recurse deeply
/// enough to overflow the default 8 MB stack on certain files (a Nikon D800 NEF
/// does). `None` on any decode error (or, in dev builds, a caught panic).
fn demosaic_full(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let owned = bytes.to_vec();
    std::thread::Builder::new()
        .name("pb-raw-demosaic".into())
        .stack_size(256 * 1024 * 1024)
        .spawn(move || -> Option<(Vec<u8>, u32, u32)> {
            let raw = rawloader::decode(&mut std::io::Cursor::new(&owned)).ok()?;
            let mut pipe =
                imagepipe::Pipeline::new_from_source(imagepipe::ImageSource::Raw(raw)).ok()?;
            let img = pipe.output_8bit(None).ok()?;
            let (w, h) = (img.width as u32, img.height as u32);
            Some((common::rgb_to_rgba(&img.data), w, h))
        })
        .ok()?
        .join()
        .ok()
        .flatten()
}

/// Byte ranges `[start, end)` of every top-level embedded JPEG. Nested JPEGs
/// (e.g. a thumbnail inside a preview's APP1/EXIF) are skipped because we advance
/// past each JPEG's true end.
fn jpeg_spans(b: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i + 3 < b.len() {
        if b[i] == 0xFF && b[i + 1] == 0xD8 && b[i + 2] == 0xFF {
            if let Some(end) = jpeg_end(b, i) {
                spans.push((i, end));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    spans
}

/// Exclusive end index of the JPEG that starts at `start` (an `FFD8` SOI), found
/// by walking markers and honoring segment lengths. `None` if it runs off the end
/// without a clean EOI.
fn jpeg_end(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 2; // past SOI
    loop {
        // Seek the next marker; markers may be preceded by fill `FF` bytes.
        while i < b.len() && b[i] != 0xFF {
            i += 1;
        }
        while i < b.len() && b[i] == 0xFF {
            i += 1; // skip the FF(s); i now points at the marker code
        }
        let marker = *b.get(i)?;
        i += 1;
        match marker {
            0xD9 => return Some(i),  // EOI
            0x01 | 0xD0..=0xD7 => {} // standalone markers: no length
            0xDA => {
                // SOS: 2-byte header length, then entropy data until a real marker.
                let len = u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as usize;
                i = i.checked_add(len)?;
                while i + 1 < b.len() {
                    if b[i] == 0xFF {
                        let n = b[i + 1];
                        // Stuffed FF00 and restart markers stay inside the scan.
                        if n == 0x00 || (0xD0..=0xD7).contains(&n) {
                            i += 2;
                            continue;
                        }
                        break; // real marker — resume the outer loop
                    }
                    i += 1;
                }
            }
            _ => {
                // Segment with a 2-byte big-endian length (includes the 2 bytes).
                let len = u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as usize;
                if len < 2 {
                    return None;
                }
                i = i.checked_add(len)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_extensions_recognized() {
        assert!(is_raw_extension("arw"));
        assert!(is_raw_extension("nef"));
        assert!(is_raw_extension("dng"));
        assert!(!is_raw_extension("jpg"));
        assert!(!is_raw_extension("tiff"));
    }

    #[test]
    fn finds_minimal_jpeg_span() {
        // SOI + APP0(len=2, empty) + SOS(len=2) + entropy 'AB' + EOI, wrapped in noise.
        let jpeg = [
            0xFF, 0xD8, // SOI
            0xFF, 0xE0, 0x00, 0x02, // APP0, length 2 (no payload)
            0xFF, 0xDA, 0x00, 0x02, // SOS, header length 2
            0x41, 0x42, // entropy data
            0xFF, 0xD9, // EOI
        ];
        let mut buf = vec![0x00, 0x11, 0x22];
        buf.extend_from_slice(&jpeg);
        buf.extend_from_slice(&[0x33, 0x44]);
        let spans = jpeg_spans(&buf);
        assert_eq!(spans, vec![(3, 3 + jpeg.len())]);
    }

    #[test]
    fn skips_nested_thumbnail_via_segment_length() {
        // An APP1 segment whose payload *contains* a full FFD8…FFD9 thumbnail must
        // be stepped over by its length, not mistaken for the outer JPEG's end.
        let thumb = [0xFF, 0xD8, 0xFF, 0xD9]; // 4-byte "JPEG" inside the payload
        let app1_len = 2 + thumb.len(); // length field counts itself + payload
        let mut jpeg = vec![0xFF, 0xD8]; // SOI
        jpeg.extend_from_slice(&[0xFF, 0xE1]); // APP1 marker
        jpeg.extend_from_slice(&(app1_len as u16).to_be_bytes());
        jpeg.extend_from_slice(&thumb); // nested thumbnail in the payload
        jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]); // SOS
        jpeg.extend_from_slice(&[0x10, 0x20]); // entropy
        jpeg.extend_from_slice(&[0xFF, 0xD9]); // outer EOI
        let spans = jpeg_spans(&jpeg);
        assert_eq!(
            spans,
            vec![(0, jpeg.len())],
            "outer JPEG spans the whole buffer"
        );
    }
}
